//! Reconcile 協調器：`sessions/ReconcileCoordinator.ts` 逐函式移植（§4.2，
//! S3.5 part B）。
//!
//! 語意重點（照舊版程式碼）：
//! - [`DefaultReconcileCoordinator::request`]（同步、void）：把 desired state
//!   替換到單一 pending 位置並記 `reconcile_requested`（已有一筆要被換掉時加記
//!   `reconcile_coalesced`）；只在沒有 worker 在跑時啟動一個 worker flight。
//! - worker（[`drain_task`]）：while accepting && pending 逐筆處理；每筆記
//!   `reconcile_started`（`queuedMs`）與 `reconcile_completed`（`completed` 以
//!   debug、`failed` 以 error，附 `durationMs`）；任務結束的 finally 段落（清
//!   slot、必要時重啟 worker）完成後才發 flight 完成信號，對應舊版 promise
//!   在 `.finally()` 回呼之後才 resolve 的順序。
//! - [`DefaultReconcileCoordinator::stop`]：`accepting = false`、丟棄 pending、
//!   呼叫 `cancelPendingStarts`（舊版 `?.()`：workerFlight 缺席時也會呼叫），
//!   並以 `Promise.race([worker + cancellation, 逾時計時器])` 等待——逾時**不
//!   視為錯誤**且**不清 worker flight**，對應 Rust 版
//!   [`tokio::time::timeout`]（逾時把「等待的未來」drop 掉；worker 自身的
//!   spawn 任務不受影響，照舊繼續收尾；與 browser manager 對逾時 drop 的註記
//!   同款取捨）。5 秒逾時以 tokio paused time 可測（舊測試的 fake timers），
//!   本方法內沒有實際睡眠以外的等待。
//! - `waitForIdle`：loop 等 worker flight 直到 slot 清空。
//!
//! 依賴以窄介面注入（§4.2、DEC-020 同類）：
//! - [`ReconcileSessionManager`] 取代 `Pick<SessionManager, 'reconcile'> &
//!   Partial<Pick<…, 'cancelPendingStarts'>>`；`cancel_pending_starts` 具預設
//!   no-op 本體，對應舊版的 `?.()` 選用性（正式 SessionManager 兩者都實作）。
//! - [`ReconcileLogger`] 取代 `Pick<Logger, 'debug' | 'error'>`（照
//!   `StallWatchdogLogger` 前例）。
//!
//! 補充：[`ReconcileError`] 以 `message` 承載舊版 promise rejection 的
//! `Error.message`；記點時再經 [`redact_sensitive_string`]
//! （`safeErrorMessage` 的移植；照 `browser/safe_logging` 前例）。
#![allow(dead_code)]

use std::collections::HashSet;
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::sync::watch;

use crate::config::thresholds::MAX_SAFE_INTEGER;
use crate::logging::logger::JsonLineLogger;
use crate::logging::redact::{SanitizedValue, redact_sensitive_string};

/// 舊版 `DEFAULT_SHUTDOWN_TIMEOUT_MS`。
const DEFAULT_SHUTDOWN_TIMEOUT_MS: u64 = 5_000;

/// 對應舊版選項的 `now: () => number`：回傳 epoch 毫秒（預設 [`default_now_ms`]；
/// 同 `sessions` 的 `RecoveryClock` 與 browser 層的 `ManagerClock` 前例）。
pub(crate) type ReconcileClock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// 舊版 promise rejection 的移植形：協調器只取訊息記 log（`safeErrorMessage`），
/// 故以 `message` 欄位呈現（照 DEC-019 `BotCommandError { message }` 前例；
/// scheduler 的注入接縫亦重用本型別）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReconcileError {
    pub(crate) message: String,
}

impl ReconcileError {
    /// 組一筆對應舊版 `Error(message)` 的值。
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl core::fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ReconcileError {}

/// 舊版 `Pick<SessionManager, 'reconcile'> & Partial<Pick<…,
/// 'cancelPendingStarts'>>` 的窄介面。實作端（S3.6 的正式 SessionManager）
/// 兩者都提供；`cancel_pending_starts` 的預設本體承載舊版 `?.()` 的選用性
/// （缺席即 `Promise.resolve()` 的 no-op）。
#[async_trait]
pub(crate) trait ReconcileSessionManager: Send + Sync {
    /// 舊版 `reconcile(activeChannels)`：可能 reject 的 promise 以 [`Result`]
    /// 表達（照 DEC-020 在窄介面上對「可拋錯」的前例），reject 由協調器記
    /// `reconcile_completed`（`outcome: 'failed'`）。
    async fn reconcile(&self, desired_channels: &[String]) -> Result<(), ReconcileError>;

    /// 舊版 `cancelPendingStarts?(reason)`；選用性由預設本體承載。
    async fn cancel_pending_starts(&self, reason: &str) {
        let _ = reason;
    }
}

/// 舊版 `Pick<Logger, 'debug' | 'error'>` 的窄介面。欄位值為消毒前的
/// [`SanitizedValue`]（正式 logger 內部再消毒）。
pub(crate) trait ReconcileLogger: Send + Sync {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>);
    fn error(&self, event: &str, fields: Option<&SanitizedValue>);
}

/// 正式 logger 的轉接（browser 層 `BrowserManagerLogger` 的同型前例）。
impl ReconcileLogger for JsonLineLogger {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        JsonLineLogger::debug(self, event, fields);
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        JsonLineLogger::error(self, event, fields);
    }
}

/// 對應舊版 `ReconcileCoordinator` 介面（scheduler 等呼叫端的型別依據）。
#[async_trait]
pub(crate) trait ReconcileCoordinator: Send + Sync {
    /// 同步、void；把 desired state 併入 pending（`request` 的 fire-and-forget）。
    fn request(&self, desired_channels: &[String]);

    /// 舊版 `stop(reason)`：有限時間等待 worker 與取消完成，逾時不回報錯誤。
    async fn stop(&self, reason: &str);

    /// 舊版 `waitForIdle()`：等 worker flight 清空。
    async fn wait_for_idle(&self);
}

/// 對應舊版 `ReconcileCoordinatorOptions`。
pub(crate) struct ReconcileCoordinatorOptions {
    pub(crate) session_manager: Arc<dyn ReconcileSessionManager>,
    pub(crate) logger: Arc<dyn ReconcileLogger>,
    /// 舊版 `now?: () => number`；`None` 用系統時鐘。
    pub(crate) now: Option<ReconcileClock>,
    /// 舊版 `shutdownTimeoutMs?: number`。舊版以 `Math.max(1, Math.trunc(…))`
    /// 夾住；trunc 的「向下取整」由 `u64`（整數）型別承接，`Math.max(1, …)`
    /// 照述。舊測試以 `vi.useFakeTimers` ＋ `shutdownTimeoutMs: 500` 驗證；
    /// Rust 測試以 tokio paused time 對應。
    pub(crate) shutdown_timeout_ms: Option<u64>,
}

/// 舊版 `ReconcileRequest`。
#[derive(Clone)]
struct ReconcileRequest {
    id: u64,
    desired_channels: Vec<String>,
    requested_at_ms: u64,
}

/// 共用狀態（std 鎖，鎖內無 await；單一協調器可有多呼叫端）。
struct CoordinatorState {
    accepting: bool,
    pending: Option<ReconcileRequest>,
    sequence: u64,
}

/// 舊版 `workerFlight` promise 的完成信號：多個等待者（stop／waitForIdle）
/// 各自訂閱（watch channel；`JoinHandle` 只能等一次，不能多端共用）。
struct FlightSignal {
    done: watch::Sender<Option<()>>,
}

impl FlightSignal {
    fn new() -> Self {
        Self {
            done: watch::channel(None).0,
        }
    }

    /// 等待完成（對應 `await flightPromise`；多次等待／多等待者皆可）。
    async fn wait(&self) {
        let mut receiver = self.done.subscribe();
        while receiver.borrow_and_update().is_none() {
            // `Err` 表示 sender 全部消失；等待端自持 Arc，sender 至少活到
            // 等待結束（同 `browser/session` 的 FlightCell 註記）。
            if receiver.changed().await.is_err() {
                continue;
            }
        }
    }

    /// 發出完成信號（單一 drain 任務是唯一的 setter；對應 promise resolve）。
    fn complete(&self) {
        let _ = self.done.send(Some(()));
    }
}

/// 對應舊版 `DefaultReconcileCoordinator`。`Clone` 供測試 spawn（共用同一份內部
/// 狀態，正式接線多持 `Arc<dyn ReconcileCoordinator>`）。
#[derive(Clone)]
pub(crate) struct DefaultReconcileCoordinator(Arc<Inner>);

struct Inner {
    session_manager: Arc<dyn ReconcileSessionManager>,
    logger: Arc<dyn ReconcileLogger>,
    now: ReconcileClock,
    shutdown_timeout_ms: u64,
    state: StdMutex<CoordinatorState>,
    /// 舊版 `workerFlight`：`Some(flight)` 表示有 worker；`None` 為 idle。
    /// 與 `state` 兩把 std 鎖**不巢狀**（取得其中一把前先放掉另一把）。
    worker_flight: StdMutex<Option<Arc<FlightSignal>>>,
}

impl DefaultReconcileCoordinator {
    pub(crate) fn new(mut options: ReconcileCoordinatorOptions) -> Self {
        let now = options
            .now
            .take()
            .unwrap_or_else(|| Arc::new(default_now_ms));
        // 舊版 `Math.max(1, Math.trunc(…))`：trunc 由整數型別承接，max(1) 照述。
        let shutdown_timeout_ms = options
            .shutdown_timeout_ms
            .unwrap_or(DEFAULT_SHUTDOWN_TIMEOUT_MS)
            .max(1);
        Self(Arc::new(Inner {
            session_manager: options.session_manager,
            logger: options.logger,
            now,
            shutdown_timeout_ms,
            state: StdMutex::new(CoordinatorState {
                accepting: true,
                pending: None,
                sequence: 0,
            }),
            worker_flight: StdMutex::new(None),
        }))
    }

    /// 對應舊版 `request` 的同步本體：鎖內接受／記初始態（無 await），出鎖後
    /// 記事件、嘗試啟動 worker。
    fn request(&self, desired_channels: &[String]) {
        let (request, replaced) = {
            let mut state = lock(&self.0.state);
            if !state.accepting {
                return;
            }
            let request = ReconcileRequest {
                id: next_sequence(&mut state.sequence),
                desired_channels: unique_in_order(desired_channels),
                requested_at_ms: (self.0.now)(),
            };
            // 舊版：const replaced = this.pending; this.pending = request;
            let replaced = state.pending.replace(request.clone());
            (request, replaced)
            // 鎖在 scope 結束放掉；此方法為同步段落，不含 await。
        };

        self.0.logger.debug(
            "reconcile_requested",
            Some(&sv_object(vec![
                ("reconcileId", SanitizedValue::Number(request.id as f64)),
                (
                    "desiredChannels",
                    sv_string_array(&request.desired_channels),
                ),
            ])),
        );
        if let Some(replaced) = replaced {
            self.0.logger.debug(
                "reconcile_coalesced",
                Some(&sv_object(vec![
                    ("reconcileId", SanitizedValue::Number(request.id as f64)),
                    (
                        "replacedReconcileId",
                        SanitizedValue::Number(replaced.id as f64),
                    ),
                    (
                        "desiredChannels",
                        sv_string_array(&request.desired_channels),
                    ),
                ])),
            );
        }
        // 舊版：this.startWorker()。
        start_worker(&self.0);
    }

    /// 對應舊版 `waitForIdle`。
    async fn wait_for_idle(&self) {
        while let Some(flight) = flight_snapshot(&self.0.worker_flight) {
            flight.wait().await;
        }
    }

    /// 對應舊版 `stop`。
    async fn stop(&self, reason: &str) {
        {
            let mut state = lock(&self.0.state);
            state.accepting = false;
            // 舊版：this.pending = undefined（直接丟棄，不 coalesce）。
            state.pending = None;
        }
        // 同步取下 flight（Arc 複製），鎖不跨 await。
        let flight = flight_snapshot(&self.0.worker_flight);

        // 舊版：Promise.all([worker ?? Promise.resolve(), cancellation])——
        // 兩翼並行；以 join! 呈現（與 Promise.all 同為「全部完成才 resolve」）。
        let flight_wait = async {
            if let Some(flight) = flight.as_ref() {
                flight.wait().await;
            }
        };
        let cancellation = self.0.session_manager.cancel_pending_starts(reason);
        // 舊版 Promise.race([Promise.all([...]), timer])：逾時**不視為錯誤**，
        // 也不清 worker flight。逾時把等待的未來 drop 掉——worker 的 spawn 任務
        // 與 `cancel_pending_starts` 未完成的持續工作留在原任務，照舊收尾；
        // `clearTimeout` 的到點即回語意由 timeout 的「逾時立刻 resolve」重現。
        let _ = tokio::time::timeout(
            Duration::from_millis(self.0.shutdown_timeout_ms),
            async move {
                let ((), ()) = tokio::join!(flight_wait, cancellation);
            },
        )
        .await;
    }
}

#[async_trait]
impl ReconcileCoordinator for DefaultReconcileCoordinator {
    fn request(&self, desired_channels: &[String]) {
        DefaultReconcileCoordinator::request(self, desired_channels);
    }

    async fn stop(&self, reason: &str) {
        DefaultReconcileCoordinator::stop(self, reason).await;
    }

    async fn wait_for_idle(&self) {
        DefaultReconcileCoordinator::wait_for_idle(self).await;
    }
}

/// 對應舊版 `startWorker`：僅在無 worker 時登記 flight 並 spawn drain 任務。
/// （`self`/`inner` 以 `Arc<Inner>` 注入供 finally 段重用。）
fn start_worker(inner: &Arc<Inner>) {
    let cell = {
        let mut worker = lock(&inner.worker_flight);
        if worker.is_some() {
            return;
        }
        let cell = Arc::new(FlightSignal::new());
        *worker = Some(Arc::clone(&cell));
        cell
    };
    let base = Arc::clone(inner);
    tokio::spawn(async move {
        drain_task(base, cell).await;
    });
}

/// 舊版 `drain` + `.finally()`：逐筆清 pending；結束後清 slot、必要時重啟
/// worker，最後發完成信號。
async fn drain_task(inner: Arc<Inner>, cell: Arc<FlightSignal>) {
    drain(inner.as_ref()).await;
    finish_drain(&inner, &cell);
}

/// 對應舊版 `drain`：while accepting && pending；pending 取出（鎖內同步）後
/// 才 await `reconcile`（進行中的 coalesce 流與舊版相同）。
async fn drain(inner: &Inner) {
    while let Some(request) = take_pending(inner) {
        let started_at_ms = (inner.now)();
        inner.logger.debug(
            "reconcile_started",
            Some(&sv_object(vec![
                ("reconcileId", SanitizedValue::Number(request.id as f64)),
                (
                    "desiredChannels",
                    sv_string_array(&request.desired_channels),
                ),
                (
                    "queuedMs",
                    SanitizedValue::Number(max_zero_ms(started_at_ms, request.requested_at_ms)),
                ),
            ])),
        );
        match inner
            .session_manager
            .reconcile(&request.desired_channels)
            .await
        {
            Ok(()) => inner.logger.debug(
                "reconcile_completed",
                Some(&sv_object(vec![
                    ("reconcileId", SanitizedValue::Number(request.id as f64)),
                    (
                        "desiredChannels",
                        sv_string_array(&request.desired_channels),
                    ),
                    ("outcome", SanitizedValue::String("completed".to_string())),
                    (
                        "durationMs",
                        SanitizedValue::Number(max_zero_ms((inner.now)(), started_at_ms)),
                    ),
                ])),
            ),
            Err(error) => inner.logger.error(
                "reconcile_completed",
                Some(&sv_object(vec![
                    ("reconcileId", SanitizedValue::Number(request.id as f64)),
                    (
                        "desiredChannels",
                        sv_string_array(&request.desired_channels),
                    ),
                    ("outcome", SanitizedValue::String("failed".to_string())),
                    (
                        "durationMs",
                        SanitizedValue::Number(max_zero_ms((inner.now)(), started_at_ms)),
                    ),
                    (
                        "error",
                        SanitizedValue::String(redact_sensitive_string(&error.message)),
                    ),
                ])),
            ),
        }
    }
}

/// 舊版 `.finally()`：清 slot（僅當 slot 仍是同一 flight）、accepting 且
/// pending 未清空時重啟 worker（重啟檢查在放鎖後進行）；完成信號最後發，
/// 對應 promise 在 finally 回呼結束後才 resolve。
fn finish_drain(inner: &Arc<Inner>, cell: &Arc<FlightSignal>) {
    {
        let mut worker = lock(&inner.worker_flight);
        let same = worker
            .as_ref()
            .map(|current| Arc::ptr_eq(current, cell))
            .unwrap_or(false);
        if same {
            *worker = None;
        }
    }
    // 重啟檢查在還開第一把鎖後進行（兩把鎖不巢狀）。
    let restart = {
        let state = lock(&inner.state);
        state.accepting && state.pending.is_some()
    };
    if restart {
        start_worker(inner);
    }
    cell.complete();
}

/// 鎖內同步取得 pending（accepting 且有值時取走）。
fn take_pending(inner: &Inner) -> Option<ReconcileRequest> {
    let mut state = lock(&inner.state);
    if !(state.accepting && state.pending.is_some()) {
        return None;
    }
    state.pending.take()
}

/// 鎖內取下 flight 的 Arc 複製（同步段落；鎖不跨 await）。
fn flight_snapshot(slot: &StdMutex<Option<Arc<FlightSignal>>>) -> Option<Arc<FlightSignal>> {
    lock(slot).clone()
}

/// 舊版 `nextId`：`(sequence % Number.MAX_SAFE_INTEGER) + 1`（循環編號）。
fn next_sequence(sequence: &mut u64) -> u64 {
    *sequence = (*sequence % (MAX_SAFE_INTEGER as u64)) + 1;
    *sequence
}

/// 舊版 `[...new Set(list)]`：保持首見順序的去重（不做大小寫正規化）。
fn unique_in_order(values: &[String]) -> Vec<String> {
    let mut seen = HashSet::with_capacity(values.len());
    let mut unique = Vec::with_capacity(values.len());
    for value in values {
        if seen.insert(value.as_str()) {
            unique.push(value.clone());
        }
    }
    unique
}

/// 舊版 `Math.max(0, now - base)`：時鐘回撥時夾為 0。
fn max_zero_ms(now_ms: u64, base_ms: u64) -> f64 {
    (now_ms as i64 - base_ms as i64).max(0) as f64
}

/// std 鎖的固定還原慣例（同 `browser/session` 前例； toxicity 鎖照樣取值）。
fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 對應舊版 `Date.now()` 的預設值：UNIX epoch 起的毫秒（1970 年前取 0）。
fn default_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

/// 欄位物件（照插入順序；對應舊版逐字面的欄位序）。
fn sv_object(entries: Vec<(&'static str, SanitizedValue)>) -> SanitizedValue {
    SanitizedValue::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
}

/// 欄位值為字串陣列（對應舊版 `desiredChannels: readonly string[]`）。
fn sv_string_array(values: &[String]) -> SanitizedValue {
    SanitizedValue::Array(
        values
            .iter()
            .map(|value| SanitizedValue::String(value.clone()))
            .collect(),
    )
}

#[cfg(test)]
mod tests;
