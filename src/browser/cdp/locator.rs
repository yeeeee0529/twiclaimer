//! Locator 核心 API（§7.4）：selector 組合、輪詢等待與查詢。
//!
//! 照 Playwright 分工：client 端 `locator.ts` 只做 selector 字串組合
//! （`locator(a).locator(b)` → `a >> b`、`first()` → `a >> nth=0`、
//! `nth(i)` → `a >> nth=<i>`），頁面內的解析與查詢用 injected script
//! （[`super::injected`]），輪詢／逾時照 `frames.ts` 的
//! `retryWithProgressAndBackoff`（`backoffScale = [20, 50, 100, 100, 500]`、
//! 超過 `timeout / 5` 的間隔修剪掉，前面補 0，之後固定最後一項）。
//!
//! strict 一律開（照 client 的 `{strict: true, ...options}`）；`count()` 與
//! `evaluate_all()` 照原始碼不開 strict。沒有元素的 `isVisible` 回 false、
//! `count` 回 0（frames.ts 的語意）；頁面已關閉時照錯誤上拋。
//!
//! click／hover 的重試機制照 `_retryPointerAction`（[`super::pointer`]）。
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::context::DriverError;
use super::injected;
use super::injected::{call_function_on, remote_is_undefined, remote_value};
use super::page::PageShared;

/// 一般操作的預設逾時（§7.6：30 000 ms；`{timeout}` 覆寫，0 = 無逾時）。
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// 輪詢的原始間隔（照 frames.ts 的 `backoffScale`；實際序列前面再補 0）。
const BACKOFF_SCALE_MS: [u64; 5] = [20, 50, 100, 100, 500];

/// `wait_for` 支援的狀態（舊版只用這兩種；照 `waitForSelector` 的 `state`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitForState {
    Visible,
    Hidden,
}

/// 等到有元素並回傳第一個元素的處理結果（`_waitForFunctionOnSelector` 的
/// 解析＋查詢＋回呼合一；沒有元素時整體回 undefined → 繼續輪詢）。
macro_rules! await_first_element_source {
    ($body:expr) => {
        concat!(
            r#"(injected, data) => {
  const parsed = injected.parseSelector(data.selector);
  const elements = injected.querySelectorAll(parsed, injected.document);
  if (data.strict && elements.length > 1)
    throw injected.strictModeViolationError(parsed, elements);
  const element = elements[0];
  if (!element) return undefined;
  return "#,
            $body,
            r#";
}"#
        )
    };
}

/// `textContent`：`{ result: element.textContent }`（frames.ts 的
/// `(injected, element) => ({ result: element.textContent })`）。
const AWAIT_TEXT_CONTENT_SOURCE: &str =
    await_first_element_source!("{ result: element.textContent }");

/// `getAttribute(name)`：`{ result: element.getAttribute(name) }`。
const AWAIT_ATTRIBUTE_SOURCE: &str =
    await_first_element_source!("{ result: element.getAttribute(data.name) }");

/// `dispatchEvent(type, eventInit)`：在元素上派送事件（injected 的
/// `dispatchEvent`），回 `{ result: undefined }` 表示完成。
const DISPATCH_EVENT_SOURCE: &str = await_first_element_source!(
    "injected.dispatchEvent(element, data.type, data.eventInit), { result: undefined }"
);

/// `waitForSelector` 的狀態觀察（frames.ts：`element ? isElementVisible : false`；
/// 元素存在才會走到回呼，這裡直接用 `utils.isElementVisible`）。
const WAIT_STATE_SOURCE: &str = r#"(injected, data) => {
  const parsed = injected.parseSelector(data.selector);
  const elements = injected.querySelectorAll(parsed, injected.document);
  if (data.strict && elements.length > 1)
    throw injected.strictModeViolationError(parsed, elements);
  const element = elements[0];
  return {
    count: elements.length,
    visible: element ? injected.utils.isElementVisible(element) : false,
  };
}"#;

/// `evaluateAll`：以所有符合元素的陣列呼叫函式（frames.ts 的
/// `_evalOnSelectorAll`；不等待、不開 strict、fn 收 `(elements, arg)`）。
const EVALUATE_ALL_SOURCE: &str = r#"(injected, data) => {
  const parsed = injected.parseSelector(data.selector);
  const elements = injected.querySelectorAll(parsed, injected.document);
  const fn = injected.eval("(" + data.functionText + ")");
  return fn(elements, data.arg);
}"#;

/// Locator：頁面 + 組合後的 selector 字串（照 client `locator.ts`）。
pub(crate) struct Locator {
    pub(crate) page: Arc<PageShared>,
    pub(crate) selector: String,
}

impl PageShared {
    /// `page.locator(selector)`。
    pub(crate) fn locator(self: &Arc<Self>, selector: impl Into<String>) -> Locator {
        Locator {
            page: Arc::clone(self),
            selector: selector.into(),
        }
    }
}

impl Locator {
    /// `locator(selector)`（子 locator；client 的 `this._selector + " >> " + selector`）。
    pub(crate) fn locator(&self, selector: impl Into<String>) -> Locator {
        self.composed(format!("{} >> {}", self.selector, selector.into()))
    }

    /// `first()`（client 的 `this._selector + " >> nth=0"`）。
    pub(crate) fn first(&self) -> Locator {
        self.nth(0)
    }

    /// `nth(index)`（client 的 ``this._selector + ` >> nth=${index}` ``；
    /// 負數照 client 原樣組出，由 injected 的 nth 引擎解讀）。
    pub(crate) fn nth(&self, index: i64) -> Locator {
        self.composed(format!("{} >> nth={}", self.selector, index))
    }

    fn composed(&self, selector: String) -> Locator {
        Locator {
            page: Arc::clone(&self.page),
            selector,
        }
    }

    /// 組合後的 selector（測試與偵錯用）。
    pub(crate) fn selector(&self) -> &str {
        &self.selector
    }

    // ===== 查詢（不等待）=====

    /// `locator.count()`：不等待，回傳符合的數量（frames.ts 的 `queryCount`：
    /// 非已關閉的錯誤一律回 0）。
    pub(crate) async fn count(&self) -> Result<usize, DriverError> {
        let result = self.resolve_count(false).await.map(|count| count as usize);
        match result {
            Err(error) if error == DriverError::target_closed() => {
                Err(api_error("locator.count", error))
            }
            // 照 frames.ts：context 摧毀等可重試情況回 0。
            Err(error) if error.is_context_destroyed() => Ok(0),
            result => result.map_err(|error| api_error("locator.count", error)),
        }
    }

    /// `locator.isVisible()`：不等待、strict；沒有元素回 false（frames.ts 的
    /// `isVisibleInternal`：可重試的錯誤也回 false，已關閉照錯誤上拋）。
    pub(crate) async fn is_visible(&self) -> Result<bool, DriverError> {
        let result = self.observe_state().await;
        match result {
            Ok((count, visible)) => Ok(count > 0 && visible),
            Err(error) if error == DriverError::target_closed() => {
                Err(api_error("locator.isVisible", error))
            }
            // 照 frames.ts：可重試錯誤（context 摧毀）回 false。
            Err(error) if error.is_context_destroyed() => Ok(false),
            Err(error) => Err(api_error("locator.isVisible", error)),
        }
    }

    // ===== 等待（輪詢）=====

    /// `locator.waitFor({state, timeout})`：輪詢到狀態成立（frames.ts 的
    /// `waitForSelector`；strict）。visible = 有元素且可見；hidden = 沒有元素
    /// 或第一個元素不可見。
    pub(crate) async fn wait_for(
        &self,
        state: WaitForState,
        timeout: Option<u64>,
    ) -> Result<(), DriverError> {
        Self::poll_with_backoff(timeout, {
            let page = Arc::clone(&self.page);
            let selector = self.selector.clone();
            move || {
                let page = Arc::clone(&page);
                let selector = selector.clone();
                async move {
                    let (count, visible) = observe_state_on(&page, &selector).await?;
                    let success = match state {
                        WaitForState::Visible => count > 0 && visible,
                        WaitForState::Hidden => count == 0 || !visible,
                    };
                    Ok(if success {
                        PollOutcome::Done(())
                    } else {
                        PollOutcome::Continue
                    })
                }
            }
        })
        .await
        .map_err(|error| api_error("locator.waitFor", error))
    }

    // ===== 取值（等到元素存在）=====

    /// `locator.getAttribute(name)`：等到元素存在、strict；沒有該屬性回 None。
    pub(crate) async fn get_attribute(
        &self,
        name: &str,
        timeout: Option<u64>,
    ) -> Result<Option<String>, DriverError> {
        let reply = self
            .await_first(AWAIT_ATTRIBUTE_SOURCE, json!({"name": name}), timeout)
            .await
            .map_err(|error| api_error("locator.getAttribute", error))?;
        let value = remote_value(&reply)
            .get("result")
            .cloned()
            .unwrap_or(Value::Null);
        Ok(remote_string_option(&value))
    }

    /// `locator.textContent()`：等到元素存在、strict；元素內文（可為 null）。
    pub(crate) async fn text_content(
        &self,
        timeout: Option<u64>,
    ) -> Result<Option<String>, DriverError> {
        let reply = self
            .await_first(AWAIT_TEXT_CONTENT_SOURCE, json!({}), timeout)
            .await
            .map_err(|error| api_error("locator.textContent", error))?;
        let value = remote_value(&reply)
            .get("result")
            .cloned()
            .unwrap_or(Value::Null);
        Ok(remote_string_option(&value))
    }

    /// `locator.evaluate(fn, arg)`：照 client `locator.ts` 的 `_withElement` —
    /// 先 `waitForSelector(attached, strict)` 輪詢到元素存在，之後 evaluate 的
    /// 回值（含 undefined）直接回傳、不再輪詢（frames.ts 的 `_evalOnSelector`：
    /// 只有解析不到元素才拋錯；函式回 undefined 不是「沒有元素」）。
    pub(crate) async fn evaluate(
        &self,
        fn_declaration: &str,
        arg: Option<Value>,
        timeout: Option<u64>,
    ) -> Result<Value, DriverError> {
        let value = Self::poll_with_backoff(timeout, {
            let page = Arc::clone(&self.page);
            let selector = self.selector.clone();
            let declaration = fn_declaration.to_string();
            move || {
                let page = Arc::clone(&page);
                let selector = selector.clone();
                let declaration = declaration.clone();
                let arg = arg.clone();
                async move {
                    let injected = page.injected();
                    let reply = call_function_on(
                        &page,
                        injected,
                        EVALUATE_ELEMENT_SOURCE,
                        json!([{"value": {
                            "selector": selector,
                            "strict": true,
                            "functionText": declaration,
                            "arg": arg,
                        }}]),
                        true,
                    )
                    .await?;
                    // 元素存在 → 函式已執行，回值直接收（undefined → Null）；
                    // 沒有元素 → 照 frames.ts 繼續輪詢。
                    let answer = remote_value(&reply);
                    Ok(
                        if answer.get("elementFound").and_then(Value::as_bool) == Some(true) {
                            PollOutcome::Done(answer.get("result").cloned().unwrap_or(Value::Null))
                        } else {
                            PollOutcome::Continue
                        },
                    )
                }
            }
        })
        .await
        .map_err(|error| api_error("locator.evaluate", error))?;
        Ok(value)
    }

    /// `locator.evaluateAll(fn)`：不等待；以所有元素的陣列呼叫（frames.ts 的
    /// `evalOnSelectorAll`；不開 strict，沒有元素時為空陣列）。
    pub(crate) async fn evaluate_all(&self, fn_declaration: &str) -> Result<Value, DriverError> {
        let page = Arc::clone(&self.page);
        let injected = page.injected();
        let reply = call_function_on(
            &page,
            injected,
            EVALUATE_ALL_SOURCE,
            json!([{"value": {
                "selector": self.selector,
                "functionText": fn_declaration,
                "arg": Value::Null,
            }}]),
            true,
        )
        .await
        .map_err(|error| api_error("locator.evaluateAll", error))?;
        // call_function_on 已解開 RemoteObject 並翻譯例外；這裡取其 value。
        Ok(remote_value(&reply))
    }

    /// `locator.dispatchEvent('click', eventInit)`：等到元素存在、strict，
    /// 在元素上派送事件（frames.ts 的 `dispatchEvent`；main world）。
    pub(crate) async fn dispatch_event(
        &self,
        event_type: &str,
        event_init: Value,
        timeout: Option<u64>,
    ) -> Result<(), DriverError> {
        self.await_first(
            DISPATCH_EVENT_SOURCE,
            json!({"type": event_type, "eventInit": event_init}),
            timeout,
        )
        .await
        .map(|_| ())
        .map_err(|error| api_error("locator.dispatchEvent", error))
    }

    // ===== 內部共用 =====

    /// 等到元素存在並以宣告處理（`_waitForFunctionOnSelector`：每次輪詢一次
    /// injected 呼叫，回 undefined → 繼續；回值 → 成功）。
    async fn await_first(
        &self,
        declaration: &str,
        extra: Value,
        timeout: Option<u64>,
    ) -> Result<Value, DriverError> {
        Self::poll_with_backoff(timeout, {
            let page = Arc::clone(&self.page);
            let selector = self.selector.clone();
            let declaration = declaration.to_string();
            move || {
                let page = Arc::clone(&page);
                let selector = selector.clone();
                let declaration = declaration.clone();
                let extra = extra.clone();
                async move {
                    let injected = page.injected();
                    let mut data = serde_json::Map::new();
                    data.insert("selector".to_string(), json!(selector));
                    data.insert("strict".to_string(), json!(true));
                    if let Value::Object(fields) = extra {
                        for (key, value) in fields {
                            data.insert(key, value);
                        }
                    }
                    let reply = call_function_on(
                        &page,
                        injected,
                        &declaration,
                        json!([{"value": data}]),
                        true,
                    )
                    .await?;
                    // 回 undefined（沒有元素）：繼續輪詢；其餘為 { result: … }。
                    Ok(if remote_is_undefined(&reply) {
                        PollOutcome::Continue
                    } else {
                        PollOutcome::Done(reply)
                    })
                }
            }
        })
        .await
    }

    /// 解析選擇器的數量（strict 由呼叫端決定）。
    async fn resolve_count(&self, strict: bool) -> Result<i64, DriverError> {
        let page = Arc::clone(&self.page);
        let injected = page.injected();
        injected::resolve_count(&page, injected, &self.selector, strict).await
    }

    /// 觀察第一個元素的（數量, 可見）狀態（單次、strict）。
    async fn observe_state(&self) -> Result<(i64, bool), DriverError> {
        let page = Arc::clone(&self.page);
        observe_state_on(&page, &self.selector).await
    }

    /// 操作逾時：`None` → 預設 30 000 ms；`Some(0)` → 無逾時（照 Playwright 的
    /// `timeout: 0` 關閉逾時）。
    pub(crate) fn deadline_of(timeout: Option<u64>) -> Option<tokio::time::Instant> {
        let ms = timeout.unwrap_or(DEFAULT_TIMEOUT_MS);
        if ms == 0 {
            return None;
        }
        Some(tokio::time::Instant::now() + Duration::from_millis(ms))
    }

    /// 逾時錯誤（X7：`Timeout <ms>ms exceeded.`；0 不會走到這裡）。
    pub(crate) fn timeout_error(timeout: Option<u64>) -> DriverError {
        DriverError::timeout(timeout.unwrap_or(DEFAULT_TIMEOUT_MS) as u128)
    }

    /// 輪詢迴圈（照 `retryWithProgressAndTimeouts`）：每次先等第 n 個間隔
    /// （第一個是 0），之後固定最後一項；整個迴圈受 `timeout_at` 限制，
    /// deadline 到了回 `TimeoutError`（照 progress 的 abort）。
    async fn poll_with_backoff<T, F, Fut>(
        timeout: Option<u64>,
        mut action: F,
    ) -> Result<T, DriverError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<PollOutcome<T>, DriverError>>,
    {
        // 照 frames.ts：`backoffScale` 補上 0；超過 `timeout / 5` 的尾端間隔修剪
        // （timeout 300ms → [0, 20, 50]，之後固定 50ms）。
        let timeout_ms = timeout.unwrap_or(DEFAULT_TIMEOUT_MS);
        let mut intervals = vec![0u64];
        for interval in BACKOFF_SCALE_MS {
            if timeout_ms != 0 && interval > timeout_ms / 5 {
                break;
            }
            intervals.push(interval);
        }

        let run = async {
            let mut index = 0usize;
            loop {
                let interval = intervals[index.min(intervals.len() - 1)];
                index += 1;
                if interval > 0 {
                    tokio::time::sleep(Duration::from_millis(interval)).await;
                }
                match action().await? {
                    PollOutcome::Done(value) => return Ok(value),
                    PollOutcome::Continue => {}
                }
            }
        };
        match Self::deadline_of(timeout) {
            Some(deadline) => match tokio::time::timeout_at(deadline, run).await {
                Ok(result) => result,
                Err(_) => Err(Self::timeout_error(timeout)),
            },
            None => run.await,
        }
    }
}

/// 輪詢動作的結果（照 frames.ts 的 `continuePolling`）。
enum PollOutcome<T> {
    /// 本輪沒有結果：照原始碼的間隔繼續。
    Continue,
    /// 輪詢完成。
    Done(T),
}

/// api 名稱前綴（X7：`message` 為 `<api>: <內容>`）。
fn api_error(api: &str, error: DriverError) -> DriverError {
    error.with_api(api)
}

/// 遠端物件的字串值（`null` → None；其他型別原樣）。
fn remote_string_option(value: &Value) -> Option<String> {
    if value.is_null() {
        return None;
    }
    value.as_str().map(str::to_string)
}

/// 觀察第一個元素的（數量, 可見）狀態（單次、strict）。
async fn observe_state_on(
    page: &Arc<PageShared>,
    selector: &str,
) -> Result<(i64, bool), DriverError> {
    let injected = page.injected();
    let reply = call_function_on(
        page,
        injected,
        WAIT_STATE_SOURCE,
        json!([{"value": {"selector": selector, "strict": true}}]),
        true,
    )
    .await?;
    let value = remote_value(&reply);
    let count = value.get("count").and_then(Value::as_i64).unwrap_or(0);
    let visible = value
        .get("visible")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok((count, visible))
}

/// locator.evaluate 的元素級呼叫：照 client 的 `_withElement`，等到元素存在
/// 後以 `(element, arg)` 呼叫使用者函式（injected 在 main world 已持有元素，
/// 直接以元素呼叫）。回物件的 `elementFound` 表示元素是否存在：沒有元素時由
/// Rust 端輪詢重試；元素存在後函式回 undefined 不視為「沒有元素」
/// （frames.ts 的 `_evalOnSelector`：只有解析不到元素才拋錯）。
const EVALUATE_ELEMENT_SOURCE: &str = r#"(injected, data) => {
  const parsed = injected.parseSelector(data.selector);
  const elements = injected.querySelectorAll(parsed, injected.document);
  if (data.strict && elements.length > 1)
    throw injected.strictModeViolationError(parsed, elements);
  const element = elements[0];
  if (!element) return { elementFound: false };
  const fn = injected.eval("(" + data.functionText + ")");
  return { elementFound: true, result: fn(element, data.arg) };
}"#;

#[cfg(test)]
mod tests;
