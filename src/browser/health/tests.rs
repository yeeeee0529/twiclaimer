//! `ChannelHealthChecker` 的單元測試：移植舊版
//! `test/unit/channel-health-checker.test.ts` 的 5 個案例；fake page／locator
//! 照舊測試的 `createPage()` 逐一對應（斷言包含 evaluate／locator 的呼叫次數）。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{
    ChannelHealthReason, ChannelHealthResult, HealthLocator, HealthPage, evaluate_channel_health,
};
use crate::browser::cdp::context::DriverError;
use crate::browser::selectors::CHANNEL_HEALTH_SELECTORS;

/// 舊測試的 `TARGET_URL`。
const TARGET_URL: &str = "https://www.twitch.tv/streamer_one";

/// 舊測試 fake 的共用狀態（`warningClick` 會翻轉 `liveVisible`）。
#[derive(Debug, Default)]
struct FakePageInner {
    /// 舊測試的 `evaluate` 回傳值照 `createPage` 的 snapshot 原樣（含可缺省的 loggedOut）。
    snapshot: Value,
    /// 舊測試的 `liveVisible`（初始為 `snapshot.liveContent`）。
    live_visible: bool,
    /// Old creator options：contentWarning probe 的回值（undefined＝false）。
    content_warning_option: Option<bool>,
    /// 舊測試的 `contentWarningClickError`。
    content_warning_click_error: Option<DriverError>,
    /// 舊測試 `mockPage.evaluate` 的呼叫次數。
    evaluate_calls: usize,
    /// 舊測試 `mockPage.locator` 回呼收到的 selector，依序記錄。
    locator_calls: Vec<String>,
    /// 舊測試 `warningClick` 的呼叫次數。
    warning_click_calls: usize,
}

#[async_trait]
impl HealthPage for FakeSharedHealthPage {
    fn is_closed(&self) -> bool {
        // 舊測試 fake：`isClosed: () => false`。
        false
    }

    fn url(&self) -> String {
        // 舊測試 fake：`url: () => TARGET_URL`。
        TARGET_URL.to_string()
    }

    async fn evaluate(
        &self,
        _fn_declaration: &str,
        _arg: Option<Value>,
    ) -> Result<Value, DriverError> {
        self.inner.lock().unwrap().evaluate_calls += 1;
        Ok(self.inner.lock().unwrap().snapshot.clone())
    }

    fn locator(&self, selector: &str) -> Arc<dyn HealthLocator> {
        self.inner
            .lock()
            .unwrap()
            .locator_calls
            .push(selector.to_string());
        Arc::new(FakeHealthLocator {
            selector: selector.to_string(),
            shared: Arc::clone(&self.inner),
        })
    }
}

/// 舊測試 fake page：共用狀態在 Arc 內（click 翻轉 live content 可見性）。
struct FakeSharedHealthPage {
    inner: Arc<Mutex<FakePageInner>>,
}

#[async_trait]
impl HealthLocator for FakeHealthLocator {
    fn first(&self) -> Arc<dyn HealthLocator> {
        // 舊測試 fake：`first(): Locator` 回同一個 mockLocator。
        Arc::new(FakeHealthLocator {
            selector: self.selector.clone(),
            shared: Arc::clone(&self.shared),
        })
    }

    async fn is_visible(&self) -> Result<bool, DriverError> {
        let inner = self.shared.lock().unwrap();
        if self.selector == CHANNEL_HEALTH_SELECTORS.content_warning {
            return Ok(inner.content_warning_option.unwrap_or(false));
        }
        if self.selector == CHANNEL_HEALTH_SELECTORS.live_content {
            return Ok(inner.live_visible);
        }
        Err(DriverError::new(
            "Error",
            format!("Unexpected locator probe: {}", self.selector),
        ))
    }

    async fn click(&self, _timeout: Option<u64>) -> Result<(), DriverError> {
        let mut inner = self.shared.lock().unwrap();
        if let Some(error) = inner.content_warning_click_error.clone() {
            return Err(error);
        }
        inner.live_visible = true;
        inner.warning_click_calls += 1;
        Ok(())
    }

    async fn wait_visible(&self, _timeout: Option<u64>) -> Result<(), DriverError> {
        let inner = self.shared.lock().unwrap();
        if !inner.live_visible {
            return Err(DriverError::new("Error", "Live content is not visible"));
        }
        Ok(())
    }
}

struct FakeHealthLocator {
    selector: String,
    shared: Arc<Mutex<FakePageInner>>,
}

/// 舊測試 `createPage(snapshot, options)`（鍵名照舊測試，`Value` 原樣回給 evaluate）。
fn create(
    snapshot: Value,
    content_warning: Option<bool>,
    content_warning_click_error: Option<DriverError>,
) -> Arc<FakeSharedHealthPage> {
    let live_content = snapshot
        .get("liveContent")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Arc::new(FakeSharedHealthPage {
        inner: Arc::new(Mutex::new(FakePageInner {
            snapshot,
            live_visible: live_content,
            content_warning_option: content_warning,
            content_warning_click_error,
            evaluate_calls: 0,
            locator_calls: Vec::new(),
            warning_click_calls: 0,
        })),
    })
}

fn shared(page: &FakeSharedHealthPage) -> Arc<Mutex<FakePageInner>> {
    Arc::clone(&page.inner)
}

/// 舊測試的 snapshot 物件（缺 `loggedOut` 時照 createPage 原樣省略）。
fn snapshot(
    login_required: bool,
    logged_out: Option<bool>,
    error: bool,
    offline: bool,
    live_content: bool,
    unsupported_player_error: bool,
) -> Value {
    let mut value = json!({
        "loginRequired": login_required,
        "error": error,
        "offline": offline,
        "liveContent": live_content,
        "unsupportedPlayerError": unsupported_player_error,
    });
    if let Some(logged_out) = logged_out {
        value["loggedOut"] = json!(logged_out);
    }
    value
}

// 原測試：以單一 DOM snapshot 探測一般健康狀態
#[tokio::test]
async fn probes_general_health_with_a_single_dom_snapshot() {
    let page = create(snapshot(false, None, false, false, true, false), None, None);

    let result = evaluate_channel_health(&*page, TARGET_URL).await;
    assert_eq!(
        result,
        ChannelHealthResult::healthy(),
        "healthy: true, reason: 'live'"
    );

    let inner = shared(&page);
    let inner = inner.lock().unwrap();
    assert_eq!(inner.evaluate_calls, 1);
    assert_eq!(inner.locator_calls.len(), 1);
    assert_eq!(
        inner.locator_calls[0],
        CHANNEL_HEALTH_SELECTORS.content_warning
    );
}

// 原測試：保留 login、error、offline、content warning、live 的判斷優先序
#[tokio::test]
async fn keeps_login_error_offline_and_live_decision_order() {
    // loginRequired 最優先；其他指標同時為真也不影響。
    let login_page = create(snapshot(true, None, true, true, true, true), None, None);
    assert_eq!(
        evaluate_channel_health(&*login_page, TARGET_URL).await,
        ChannelHealthResult::failed(ChannelHealthReason::LoginRequired)
    );
    assert!(shared(&login_page).lock().unwrap().locator_calls.is_empty());

    // error 次之。
    let error_page = create(snapshot(false, None, false, true, true, true), None, None);
    assert_eq!(
        evaluate_channel_health(&*error_page, TARGET_URL).await,
        ChannelHealthResult::failed(ChannelHealthReason::ErrorPage)
    );
    assert!(shared(&error_page).lock().unwrap().locator_calls.is_empty());

    // offline 再次之。
    let offline_page = create(snapshot(false, None, false, true, true, false), None, None);
    assert_eq!(
        evaluate_channel_health(&*offline_page, TARGET_URL).await,
        ChannelHealthResult::failed(ChannelHealthReason::Offline)
    );
    assert!(
        shared(&offline_page)
            .lock()
            .unwrap()
            .locator_calls
            .is_empty()
    );
}

// 原測試：一般未登入回報 login_missing，明確登入阻擋仍優先
#[tokio::test]
async fn reports_login_missing_for_generic_logged_out_but_login_required_still_wins() {
    let logged_out = snapshot(false, Some(true), false, false, true, false);
    assert_eq!(
        evaluate_channel_health(&*create(logged_out.clone(), None, None), TARGET_URL).await,
        ChannelHealthResult::failed(ChannelHealthReason::LoginMissing)
    );
    assert_eq!(
        evaluate_channel_health(
            &*create(
                merge(&logged_out, json!({"loginRequired": true})),
                None,
                None,
            ),
            TARGET_URL,
        )
        .await,
        ChannelHealthResult::failed(ChannelHealthReason::LoginRequired)
    );
    assert_eq!(
        evaluate_channel_health(
            &*create(merge(&logged_out, json!({"error": true})), None, None),
            TARGET_URL,
        )
        .await,
        ChannelHealthResult::failed(ChannelHealthReason::ErrorPage)
    );
    // liveContent 不在且 content warning 存在（點完後 live content 仍未出現）
    // 時，一般的未登入狀態仍回報 login_missing。
    assert_eq!(
        evaluate_channel_health(
            &*create(
                merge(&logged_out, json!({"liveContent": false})),
                Some(true),
                None,
            ),
            TARGET_URL,
        )
        .await,
        ChannelHealthResult::failed(ChannelHealthReason::LoginMissing)
    );
}

// 原測試：content warning 仍用 Locator 點擊並等待 live content
#[tokio::test]
async fn content_warning_still_uses_locator_click_and_waits_for_live_content() {
    let page = create(
        snapshot(false, None, false, false, false, false),
        Some(true),
        None,
    );

    assert_eq!(
        evaluate_channel_health(&*page, TARGET_URL).await,
        ChannelHealthResult::healthy()
    );

    let inner = shared(&page);
    let inner = inner.lock().unwrap();
    assert_eq!(inner.evaluate_calls, 1);
    assert_eq!(inner.warning_click_calls, 1);
    assert!(
        inner
            .locator_calls
            .iter()
            .any(|selector| selector == CHANNEL_HEALTH_SELECTORS.live_content)
    );
}

// 原測試：content warning 無法點擊時回報 content_warning
#[tokio::test]
async fn reports_content_warning_when_click_is_blocked() {
    let page = create(
        snapshot(false, None, false, false, false, false),
        Some(true),
        Some(DriverError::new("Error", "button blocked")),
    );

    assert_eq!(
        evaluate_channel_health(&*page, TARGET_URL).await,
        ChannelHealthResult::failed(ChannelHealthReason::ContentWarning)
    );
}

/// 舊測試 `{ ...snapshot, key: value }` 的展開複製。
fn merge(base: &Value, extra: Value) -> Value {
    let mut merged = base.clone();
    let Value::Object(extra_object) = extra else {
        return merged;
    };
    if !merged.is_object() {
        return merged;
    }
    for (key, value) in extra_object {
        merged[key] = value;
    }
    merged
}
