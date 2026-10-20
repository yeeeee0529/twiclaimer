//! click／hover 的指標操作機制（§7.4；照 Playwright 1.62.1 `dom.ts` 的
//! `_retryPointerAction`／`_performPointerAction` 與 `frames.ts` 的
//! `_retryWithProgressIfNotConnected`／`waitForSignalsCreatedBy`）。
//!
//! 單一重試迴圈（`_retryAction` 的 waitTime 語意）：重試第 n 次前等
//! `[0, 20, 100, 100, 500]` 的第 n 項，之後固定 500 ms；整個操作受 timeout
//! 限制，逾時回 `TimeoutError`。每次重試都重新解析元素（元素途中被移除 →
//! 下次重試重取）。不做 `performActionPreChecks`／`_markAsTargetElement`／
//! iframe 的 frame→frame hit target 檢查（本專案只用主框架）。
//!
//! 單次操作的步驟（`_performPointerAction`）：
//! 1. `injected.checkElementStates`（click：visible+enabled+stable、hover：
//!    visible+stable；stable 在頁內以連續兩次 rAF 判定）。
//! 2. 捲動：第 0 次 `DOM.scrollIntoViewIfNeeded`，之後頁內 `scrollIntoView` 的
//!    end／center／start 輪替。
//! 3. `_clickablePoint`：`DOM.getContentQuads`＋`Page.getLayoutMetrics`，取與
//!    視窗相交面積 > 0.99 的第一個 quad 中點（捨入到小數 2 位）。
//! 4. `injected.setupHitTargetInterceptor`（初步 hit target 檢查與事件攔截）；
//!    回傳描述字串時重試。
//! 5. click 送 `mouseMoved`→`mousePressed`→`mouseReleased`（hover 只
//!    `mouseMoved`）→ 攔截器 `stop()`（回描述字串時重試；導覽已被請求但未
//!    收尾時照 `raceAgainstEvaluationStallingEvents` 不送出）→ click 再送
//!    `Page.enable`（inputActionEpilogue）並等動作觸發的導覽收尾；hover 兩者皆無。
//!
//! 照 Playwright，兩個世界裡的行為等價：injected script 在主框架 main world
//! 建立（`isUtilityWorld: false`，S1.2 錄製值），頁內沒有干擾的腳本時結果相同。
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::context::DriverError;
use super::injected::{self, ElementStateCheck, HitTargetSetup, HitTargetStop, release_object};
use super::locator::Locator;
use super::page::PageShared;

/// 重試的等待序列（`dom.ts` `_retryAction` 的 waitTime；重試第 n 次前等第 n 項）。
const RETRY_WAIT_MS: [u64; 5] = [0, 20, 100, 100, 500];

/// 捲動對齊的輪替（`_retryPointerAction` 的 scrollOptions；第 0 項是 protocol 捲動）。
const SCROLL_ALIGNMENTS: [Option<(&'static str, &'static str)>; 4] = [
    None,
    Some(("end", "end")),
    Some(("center", "center")),
    Some(("start", "start")),
];

/// 動作點的最小面積（照 `_clickablePoint`：允許 1×1 元素，0.99 吸收捨入誤差）。
const MIN_QUAD_AREA: f64 = 0.99;

/// 指標操作名（`_retryPointerAction` 的 actionName）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PointerActionName {
    Click,
    Hover,
}

impl PointerActionName {
    /// X7 的 API 名前綴。
    fn api_name(self) -> &'static str {
        match self {
            PointerActionName::Click => "locator.click",
            PointerActionName::Hover => "locator.hover",
        }
    }

    /// 等待的元素狀態（照 `_performPointerAction`：click 多一個 enabled）。
    fn element_states(self) -> &'static [&'static str] {
        match self {
            PointerActionName::Click => &["visible", "enabled", "stable"],
            PointerActionName::Hover => &["visible", "stable"],
        }
    }

    /// hit target 攔截器的 action type（照 `_performPointerAction`：
    /// click／dblclick 是 mouse、hover 是 hover）。
    fn interceptor_action(self) -> &'static str {
        match self {
            PointerActionName::Click => "mouse",
            PointerActionName::Hover => "hover",
        }
    }

    /// click 等動作觸發的導覽（`waitForSignalsCreatedBy`）；hover 不等。
    fn waits_for_signals(self) -> bool {
        matches!(self, PointerActionName::Click)
    }
}

impl Locator {
    /// `locator.click({timeout})`：等到元素 visible+enabled+stable、捲動、算出
    /// 動作點、hit target 檢查後送 mousePressed／mouseReleased。
    pub(crate) async fn click(&self, timeout: Option<u64>) -> Result<(), DriverError> {
        retry_pointer_action(
            &self.page,
            &self.selector,
            PointerActionName::Click,
            timeout,
        )
        .await
    }

    /// `locator.hover({timeout})`：等到元素 visible+stable、捲動、算出動作點、
    /// hit target 檢查後送 mouseMoved。
    pub(crate) async fn hover(&self, timeout: Option<u64>) -> Result<(), DriverError> {
        retry_pointer_action(
            &self.page,
            &self.selector,
            PointerActionName::Hover,
            timeout,
        )
        .await
    }
}

/// 單次操作的結果（照 dom.ts 的 PerformActionResult 語意；除 Done 外都可重試）。
enum PointerOutcome {
    /// 完成。
    Done,
    /// 元素途中脫離 DOM（下次重試重取元素）。
    NotConnected,
    /// 沒有 layout（`display:none` 等）或 quads 取不到。
    NotVisible,
    /// 動作點不在視窗內。
    NotInViewport,
    /// 缺少元素狀態（`visible`／`enabled`／`stable` 之一）。
    MissingState(String),
    /// 被描述的元素蓋住（hit target 檢查失敗）。
    HitTarget(String),
    /// 其他可重試的錯誤（照 frames.ts 的「retry upon all other errors」）。
    RetryOther,
}

/// 單一步驟的結果：帶值繼續下一步，或以可重試的結果結束本輪。
enum StepFlow<T> {
    Proceed(T),
    Retry(PointerOutcome),
}

/// 錯誤是否不可重試（照 frames.ts 的 `isNonRetriableError`：已關閉、crash、
/// JS 例外（含 strict 違規）不可重試；context 被導覽摧毀可重試）。
fn is_fatal_error(error: &DriverError) -> bool {
    *error == DriverError::target_closed() || error.is_crashed() || !error.is_context_destroyed()
}

/// click／hover 的重試迴圈（照 `_retryWithProgressIfNotConnected`＋
/// `_retryAction` 的合流；每次重試都重新解析元素）。
async fn retry_pointer_action(
    page: &Arc<PageShared>,
    selector: &str,
    name: PointerActionName,
    timeout: Option<u64>,
) -> Result<(), DriverError> {
    let api = name.api_name();
    let run = async {
        let mut attempts = 0usize;
        loop {
            // 重試第 n 次前等 waitTime 第 n 項（`_retryAction`）。
            if attempts > 0 {
                let wait = RETRY_WAIT_MS[(attempts - 1).min(RETRY_WAIT_MS.len() - 1)];
                if wait > 0 {
                    tokio::time::sleep(Duration::from_millis(wait)).await;
                }
            }
            // 重新解析元素（strict；缺失時照 `_retryWithProgressIfNotConnected` 續等）。
            let element = match resolve_element(page, selector).await {
                Ok(Some(element)) => element,
                Ok(None) => {
                    attempts += 1;
                    continue;
                }
                Err(error) => {
                    if is_fatal_error(&error) {
                        return Err(error.with_api(api));
                    }
                    // context 被導覽摧毀：重試（下一輪重新解析）。
                    attempts += 1;
                    continue;
                }
            };
            let element_id = injected::remote_object_id(&element)
                .unwrap_or_default()
                .to_string();
            let outcome = perform_pointer_action(page, &element_id, name, attempts).await;
            // 元素把手的釋放（照 `_retryWithProgressIfNotConnected` 的 finally dispose）。
            release_object(page, &element_id);
            match outcome {
                Ok(PointerOutcome::Done) => return Ok(()),
                Ok(_) => attempts += 1,
                Err(error) => {
                    if is_fatal_error(&error) {
                        return Err(error.with_api(api));
                    }
                    // context 被導覽摧毀：重試。
                    attempts += 1;
                    continue;
                }
            }
        }
    };
    // 整個操作受 timeout 限制（照 progress 的 abort；0 = 無逾時）。
    match Locator::deadline_of(timeout) {
        Some(deadline) => match tokio::time::timeout_at(deadline, run).await {
            Ok(result) => result,
            Err(_) => Err(Locator::timeout_error(timeout).with_api(api)),
        },
        None => run.await,
    }
}

/// 解析元素（strict）。
async fn resolve_element(
    page: &Arc<PageShared>,
    selector: &str,
) -> Result<Option<Value>, DriverError> {
    injected::resolve_element_object(page, page.injected(), selector, true).await
}

/// 單次指標操作（照 `_performPointerAction`；不含重試與元素解析）。
/// `Ok(PointerOutcome::Done)` 表示完成；其餘 outcome 由呼叫端重試。
async fn perform_pointer_action(
    page: &Arc<PageShared>,
    element_id: &str,
    name: PointerActionName,
    retry: usize,
) -> Result<PointerOutcome, DriverError> {
    // 1. 元素狀態（照 !force 分支；stable 檢查在頁內以 rAF 完成）。
    match check_states(page, element_id, name).await? {
        StepFlow::Proceed(()) => {}
        StepFlow::Retry(outcome) => return Ok(outcome),
    }
    // 2. 捲動（照 doScrollIntoView：第 0 次 protocol、之後對齊輪替）。
    match scroll_into_view(page, element_id, retry).await? {
        StepFlow::Proceed(()) => {}
        StepFlow::Retry(outcome) => return Ok(outcome),
    }
    // 3. 動作點（照 _clickablePoint）。
    let (point_x, point_y) = match clickable_point(page, element_id).await? {
        StepFlow::Proceed(point) => point,
        StepFlow::Retry(outcome) => return Ok(outcome),
    };
    // 4. hit target 攔截器（初步檢查＋事件攔截）。
    let interceptor = match setup_interceptor(page, element_id, name, point_x, point_y).await? {
        StepFlow::Proceed(handle) => handle,
        StepFlow::Retry(outcome) => return Ok(outcome),
    };
    // 5. 動作與後續等待（照 waitForSignalsCreatedBy；hover 不等訊號）。
    //    run_action 內含 stop 與攔截器把手的釋放；動作錯誤時再補一次
    //    fire-and-forget 的 stop（照 waitForSignalsCreatedBy 的 .finally）。
    let action = run_action(page, name, point_x, point_y, &interceptor);
    let result = if name.waits_for_signals() {
        wait_for_signals_created_by(page, action).await
    } else {
        match action.await {
            Ok(stop) => Ok(stop),
            Err(error) => {
                stop_may_fail(page, &interceptor);
                release_object(page, &interceptor);
                Err(error)
            }
        }
    };
    result.map(|stop| match stop {
        HitTargetStop::Done => PointerOutcome::Done,
        HitTargetStop::HitTargetDescription(description) => PointerOutcome::HitTarget(description),
    })
}

/// 步驟 1：`injected.checkElementStates`。
async fn check_states(
    page: &Arc<PageShared>,
    element_id: &str,
    name: PointerActionName,
) -> Result<StepFlow<()>, DriverError> {
    let states = name.element_states();
    match injected::check_element_states(page, page.injected(), element_id, states).await {
        Ok(ElementStateCheck::Ready) => Ok(StepFlow::Proceed(())),
        Ok(ElementStateCheck::NotConnected) => Ok(StepFlow::Retry(PointerOutcome::NotConnected)),
        Ok(ElementStateCheck::Missing(state)) => {
            Ok(StepFlow::Retry(PointerOutcome::MissingState(state)))
        }
        // 照 evaluateInUtility：非不可重試的錯誤視同 notconnected。
        Err(error) if error.is_context_destroyed() => {
            Ok(StepFlow::Retry(PointerOutcome::NotConnected))
        }
        Err(error) => Err(error),
    }
}

/// 步驟 2：捲動（照 doScrollIntoView）。
async fn scroll_into_view(
    page: &Arc<PageShared>,
    element_id: &str,
    retry: usize,
) -> Result<StepFlow<()>, DriverError> {
    match SCROLL_ALIGNMENTS[retry % SCROLL_ALIGNMENTS.len()] {
        // 第 0 次：`DOM.scrollIntoViewIfNeeded`（照 _scrollRectIntoViewIfNeeded；
        // 本專案沒有 position 選項，不帶 rect 欄）。
        None => {
            let reply = page
                .send(
                    "DOM.scrollIntoViewIfNeeded",
                    json!({"objectId": element_id}),
                )
                .await;
            match reply {
                Ok(_) => Ok(StepFlow::Proceed(())),
                Err(error) if error == DriverError::target_closed() => Err(error),
                Err(error) if error.is_crashed() => Err(error),
                Err(error) => {
                    let message = error.message;
                    if message.contains("Node does not have a layout object") {
                        Ok(StepFlow::Retry(PointerOutcome::NotVisible))
                    } else if message.contains("Node is detached from document") {
                        Ok(StepFlow::Retry(PointerOutcome::NotConnected))
                    } else {
                        // 未知錯誤照 frames.ts 重試（retry upon all other errors）。
                        Ok(StepFlow::Retry(PointerOutcome::RetryOther))
                    }
                }
            }
        }
        // 之後：頁內 scrollIntoView 的對齊輪替（照 evaluateInUtility：失敗視同
        // notconnected）。
        Some((block, inline)) => {
            let reply = injected::call_on_object(
                page,
                injected::SCROLL_INTO_VIEW_SOURCE,
                element_id,
                json!([{"value": {"block": block, "inline": inline}}]),
                true,
            )
            .await;
            match reply {
                Ok(_) => Ok(StepFlow::Proceed(())),
                Err(error) if error == DriverError::target_closed() => Err(error),
                Err(error) if error.is_crashed() => Err(error),
                Err(_) => Ok(StepFlow::Retry(PointerOutcome::NotConnected)),
            }
        }
    }
}

/// 步驟 3：動作點（照 `_clickablePoint`）。
async fn clickable_point(
    page: &Arc<PageShared>,
    element_id: &str,
) -> Result<StepFlow<(f64, f64)>, DriverError> {
    // quads 與視窗大小並行取得（照 Promise.all）。
    let (quads_reply, metrics_reply) = tokio::join!(
        page.send("DOM.getContentQuads", json!({"objectId": element_id})),
        page.send("Page.getLayoutMetrics", json!({})),
    );
    // 照 `_sendMayFail`：quads 失敗視同 null → error:notvisible。
    let quads = match quads_reply {
        Ok(reply) => reply
            .get("quads")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        Err(_) => return Ok(StepFlow::Retry(PointerOutcome::NotVisible)),
    };
    // metrics 失敗照 `_clickablePoint` 直接上拋（closed → 終止；其餘可重試）。
    let metrics = metrics_reply?;
    if quads.is_empty() {
        return Ok(StepFlow::Retry(PointerOutcome::NotVisible));
    }
    let (width, height) = viewport_size(&metrics);
    // 與視窗相交後面積 > 0.99 的第一個 quad（照 _clickablePoint 的 filtering；
    // 主框架的 framePosition 為 (0, 0)，quad 不位移）。
    for quad in &quads {
        let Some(points) = quad_points(quad) else {
            continue;
        };
        let clipped = [
            clamp_to_viewport(points[0], width, height),
            clamp_to_viewport(points[1], width, height),
            clamp_to_viewport(points[2], width, height),
            clamp_to_viewport(points[3], width, height),
        ];
        if quad_area(&clipped) > MIN_QUAD_AREA {
            let middle = quad_middle(&clipped);
            let (x, y) = round_point(middle);
            return Ok(StepFlow::Proceed((x, y)));
        }
    }
    Ok(StepFlow::Retry(PointerOutcome::NotInViewport))
}

/// 步驟 4：安裝 hit target 攔截器（照 `_performPointerAction` 的
/// `_evaluateHandleInUtility` 呼叫）。
async fn setup_interceptor(
    page: &Arc<PageShared>,
    element_id: &str,
    name: PointerActionName,
    point_x: f64,
    point_y: f64,
) -> Result<StepFlow<String>, DriverError> {
    match injected::setup_hit_target_interceptor(
        page,
        page.injected(),
        element_id,
        name.interceptor_action(),
        Some((point_x, point_y)),
        false, // trial：本專案不用 trial run
    )
    .await
    {
        Ok(HitTargetSetup::Installed(handle)) => Ok(StepFlow::Proceed(handle)),
        Ok(HitTargetSetup::NotConnected) => Ok(StepFlow::Retry(PointerOutcome::NotConnected)),
        Ok(HitTargetSetup::HitTargetDescription(description)) => {
            Ok(StepFlow::Retry(PointerOutcome::HitTarget(description)))
        }
        // 照 evaluateInUtility：非不可重試的錯誤視同 notconnected。
        Err(error) if error.is_context_destroyed() => {
            Ok(StepFlow::Retry(PointerOutcome::NotConnected))
        }
        Err(error) => Err(error),
    }
}

/// 步驟 5 的動作本體：送出輸入事件 → `interceptor.stop()` → 釋放攔截器把手
/// （順序照錄製結果：stop（id38）→ releaseObject 攔截器（id39））。
///
/// click 照 `input.ts` Mouse.click 的無 delay 分支（move → down → up 的線上
/// 順序）；hover 只送 mouseMoved。
async fn run_action(
    page: &Arc<PageShared>,
    name: PointerActionName,
    point_x: f64,
    point_y: f64,
    interceptor: &str,
) -> Result<HitTargetStop, DriverError> {
    match name {
        PointerActionName::Click => {
            page.mouse_move(point_x, point_y).await?;
            page.mouse_press(point_x, point_y).await?;
            page.mouse_release(point_x, point_y).await?;
        }
        PointerActionName::Hover => {
            page.mouse_move(point_x, point_y).await?;
        }
    }
    // 動作後停止攔截器（照 raceAgainstEvaluationStallingEvents 的
    // 「Frame is currently attempting a navigation」防線：導覽已被請求但尚未
    // 收尾時不送出 evaluate）；錯誤一律視同 done（照 .catch(e => 'done')）。
    let stop = stop_result(page, interceptor).await;
    // stop 的 .finally dispose。
    release_object(page, interceptor);
    Ok(stop)
}

/// `interceptor.stop()`（或導覽 pending 時跳過）的結果；錯誤吞掉。
async fn stop_result(page: &Arc<PageShared>, interceptor: &str) -> HitTargetStop {
    if page.has_pending_navigation() {
        return HitTargetStop::Done;
    }
    match injected::stop_hit_target_interceptor(page, interceptor).await {
        Ok(stop) => stop,
        Err(_) => HitTargetStop::Done,
    }
}

/// 攔截器的第二次 stop（fire-and-forget；照 waitForSignalsCreatedBy 的
/// `.finally` 清理，錯誤吞掉）。
fn stop_may_fail(page: &PageShared, interceptor: &str) {
    page.send_may_fail(
        "Runtime.callFunctionOn",
        json!({
            "objectId": interceptor,
            "functionDeclaration": injected::STOP_HIT_TARGET_INTERCEPTOR_SOURCE,
            "arguments": [{"objectId": interceptor}],
            "returnByValue": true,
            "awaitPromise": true,
            "userGesture": true,
        }),
    );
}

/// click 的訊號等待（照 `frames.ts` 的 `waitForSignalsCreatedBy`＋
/// `SignalBarrier`）：動作完成後送 `inputActionEpilogue`（`Page.enable`），
/// 再等動作期間被請求的導覽收尾（commit 或被取代）。
async fn wait_for_signals_created_by(
    page: &Arc<PageShared>,
    action: impl std::future::Future<Output = Result<HitTargetStop, DriverError>>,
) -> Result<HitTargetStop, DriverError> {
    // 先訂閱 watch 並記 baseline（避免錯過動作期間的更新）。
    let mut snapshot_rx = page.subscribe_snapshot();
    let baseline = page.navigation_counters();
    let stop = action.await?;
    // inputActionEpilogue（照 crPage：Page.enable；錯誤一律吞掉）。
    let _ = page.send("Page.enable", json!({})).await;
    // barrier.waitFor：釋放初始 retain 後，等「請求了但未收尾」的導覽歸零。
    loop {
        let snapshot = snapshot_rx.borrow().clone();
        if snapshot.closed || snapshot.crashed {
            return Err(page.lifecycle_stop_error());
        }
        let (requests, commits, same_document) = (
            snapshot.navigation_requests - baseline.0,
            snapshot.commit_count - baseline.1,
            snapshot.same_document_commits - baseline.2,
        );
        // 初始 retain 已在 waitFor 釋放：剩下取決於未收尾的請求。
        if requests <= commits + same_document {
            return Ok(stop);
        }
        if snapshot_rx.changed().await.is_err() {
            return Err(DriverError::target_closed());
        }
    }
}

/// CDP quad（8 個數字）→ 4 個頂點（照 `_clickablePoint` 的 quad 展開）。
fn quad_points(quad: &Value) -> Option<[(f64, f64); 4]> {
    let values = quad.as_array()?;
    if values.len() != 8 {
        return None;
    }
    let mut numbers = [0.0f64; 8];
    for (index, value) in values.iter().enumerate() {
        numbers[index] = value.as_f64()?;
    }
    Some([
        (numbers[0], numbers[1]),
        (numbers[2], numbers[3]),
        (numbers[4], numbers[5]),
        (numbers[6], numbers[7]),
    ])
}

/// 視窗大小（照 `_clickablePoint` 的 innerWidth／innerHeight；取
/// `cssLayoutViewport`，退回 `layoutViewport`）。
fn viewport_size(metrics: &Value) -> (f64, f64) {
    let viewport = metrics
        .get("cssLayoutViewport")
        .or_else(|| metrics.get("layoutViewport"));
    let width = viewport
        .and_then(|view| view.get("clientWidth"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let height = viewport
        .and_then(|view| view.get("clientHeight"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    (width, height)
}

/// 把點夾進視窗（照 intersectQuadWithViewport）。
fn clamp_to_viewport(point: (f64, f64), width: f64, height: f64) -> (f64, f64) {
    (point.0.clamp(0.0, width), point.1.clamp(0.0, height))
}

/// quad 的有向面積總和（照 computeQuadArea）。
fn quad_area(points: &[(f64, f64); 4]) -> f64 {
    let mut area = 0.0;
    for index in 0..points.len() {
        let p1 = points[index];
        let p2 = points[(index + 1) % points.len()];
        area += (p1.0 * p2.1 - p2.0 * p1.1) / 2.0;
    }
    area.abs()
}

/// quad 四個頂點的中點（照 quadMiddlePoint：逐一加 1/4，順序相同）。
fn quad_middle(points: &[(f64, f64); 4]) -> (f64, f64) {
    let mut result = (0.0, 0.0);
    for point in points {
        result.0 += point.0 / 4.0;
        result.1 += point.1 / 4.0;
    }
    result
}

/// 點捨入到小數 2 位（照 roundPoint 的 `(x * 100 | 0) / 100`：向零截斷）。
fn round_point(point: (f64, f64)) -> (f64, f64) {
    (
        (point.0 * 100.0).trunc() / 100.0,
        (point.1 * 100.0).trunc() / 100.0,
    )
}
