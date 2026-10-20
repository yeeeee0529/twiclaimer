//! 主框架 main world 的 injected script（X8、§7.4）。
//!
//! 照 Playwright `dom.ts` 的 `injectedScript()`：第一次需要時以 `Runtime.evaluate`
//! 在主框架 main world 建立 `InjectedScript` 實例（`isUtilityWorld: false`、
//! `customEngines: []`，其餘選項填 1.62.1 Chromium 的值），`returnByValue: false`
//! 保留回傳的 objectId，不設全域變數。收到 execution context 摧毀事件或偵測到
//! context 換新時丟棄，下次使用重建（事件泵在 [`PageShared`] 上清空）。
//!
//! 後續的操作照 `frameSelectors.ts`：先 `injected.parseSelector(selector)`，
//! 再 `injected.querySelectorAll(parsed, document)`（以 `Runtime.callFunctionOn`
//! 在 injected object 上呼叫）。本檔只放呼叫的骨架與回傳值解析；各 API 的
//! 重試／逾時流程在 [`super::locator`]。
//!
//! 以下 API 尚無正式呼叫端，等對應步驟接上後移除 allow：
//! - `InjectedScript::invalidate`（事件泵呼叫）。
//! - `check_element_states`／`setup_hit_target_interceptor`（click／hover 用）。
//! - `release_object`（淘汰重複建立時）。
#![allow(dead_code)]

use std::sync::Mutex;

use serde_json::{Value, json};

use super::context::DriverError;
use super::page::PageShared;

/// injected script 建立時的選項（照 1.62.1 `dom.ts` 的 `injectedScript()`；
/// Chromium 的 `rafCountForStablePosition()` 為 1、`shouldPrependErrorPrefix()`
/// 為 false；`isUnderTest()` 為 false；`frameSeq` 對主框架固定 1）。
const INJECTED_SCRIPT_OPTIONS: &str = r#"{"isUnderTest":false,"sdkLanguage":"javascript","frameSeq":1,"testIdAttributeName":"data-testid","stableRafCount":1,"browserName":"chromium","shouldPrependErrorPrefix":false,"isUtilityWorld":false,"customEngines":[]}"#;

/// injected script 的原始碼（S1.2 逐字 dump，不得修改）。
const INJECTED_SCRIPT_SOURCE: &str = include_str!("js/injected-script.js");

/// `Runtime.callFunctionOn` 在 injected object 上呼叫用的固定宣告：解析選擇器
/// （先 `parseSelector`）、在主文件上查詢、依需要拋出 strict 違規，回傳元素
/// （缺失時 undefined）與數量。
const RESOLVE_ELEMENTS_SOURCE: &str = r#"(injected, { selector, strict }) => {
  const parsed = injected.parseSelector(selector);
  const elements = injected.querySelectorAll(parsed, injected.document);
  if (strict && elements.length > 1)
    throw injected.strictModeViolationError(parsed, elements);
  return { count: elements.length, element: elements[0] ?? undefined };
}"#;

/// 解析選擇器並回傳**所有**元素（嚴格檢查；`evaluateAll` 用）。
const RESOLVE_ALL_SOURCE: &str = r#"(injected, { selector, strict }) => {
  const parsed = injected.parseSelector(selector);
  const elements = injected.querySelectorAll(parsed, injected.document);
  if (strict && elements.length > 1)
    throw injected.strictModeViolationError(parsed, elements);
  return elements;
}"#;

/// 檢查元素狀態（click：`['visible','enabled','stable']`、hover：
/// `['visible','stable']`；`injected.checkElementStates`）。
const CHECK_ELEMENT_STATES_SOURCE: &str =
    r#"async (injected, element, states) => injected.checkElementStates(element, states)"#;

/// 安裝 hit target 攔截器（照 `dom.ts` 的 `_performPointerAction`）。
const SETUP_HIT_TARGET_INTERCEPTOR_SOURCE: &str =
    r#"async (injected, element, actionType, hitPoint, blockAllEvents) =>
      injected.setupHitTargetInterceptor(element, actionType, hitPoint, blockAllEvents)"#;

/// 主框架 main world 的 injected script 把手：物件 id 與其建立的 context id。
pub(crate) struct InjectedScript {
    /// `(context_id, object_id)`；`None` = 尚未建立或已隨 context 摧毀。
    state: Mutex<Option<(i64, String)>>,
}

impl Default for InjectedScript {
    fn default() -> Self {
        InjectedScript {
            state: Mutex::new(None),
        }
    }
}

impl InjectedScript {
    /// 取得（必要時建立）injected script 的 objectId。
    ///
    /// 建立需要 await（等 context、送 `Runtime.evaluate`），所以鎖只在讀寫
    /// 少數欄位時持有；並行建立時以先落庫者為準，多餘的物件回收。
    pub(crate) async fn object_id(&self, page: &PageShared) -> Result<String, DriverError> {
        if let Some((_, object_id)) = self.locked_state() {
            return Ok(object_id);
        }
        let context_id = page.wait_main_world_context().await?;
        let expression = format!(
            "(() => {{\n        const module = {{}};\n        {INJECTED_SCRIPT_SOURCE}\n        return new (module.exports.InjectedScript())(globalThis, {INJECTED_SCRIPT_OPTIONS});\n        }})()"
        );
        let reply = page
            .send(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "contextId": context_id,
                    "returnByValue": false,
                }),
            )
            .await?;
        // `Runtime.evaluate` 的例外（injected script 本身不該拋錯）：照 crExecutionContext
        // 的 `JavaScriptErrorInEvaluate` 轉成 Error（訊息第一行）。
        if let Some(details) = reply.get("exceptionDetails") {
            return Err(DriverError::new(
                "Error",
                first_line_of(&super::page::get_exception_message(details)),
            ));
        }
        let object_id = reply
            .get("result")
            .and_then(|result| result.get("objectId"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                DriverError::new(
                    "Error",
                    "Runtime.evaluate response for the injected script is missing 'objectId'",
                )
            })?
            .to_string();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match state.take() {
            // 並行建立：沿用先落庫者，回收這次的物件（fire-and-forget，照 `_sendMayFail`）。
            Some(existing) => {
                let context = existing.0;
                let stale = object_id;
                *state = Some(existing.clone());
                drop(state);
                if context == context_id {
                    page.send_may_fail(
                        "Runtime.releaseObject",
                        json!({"objectId": stale}),
                    );
                }
                Ok(existing.1)
            }
            None => {
                *state = Some((context_id, object_id.clone()));
                Ok(object_id)
            }
        }
    }

    /// 目前的 `(context_id, object_id)`（鎖內讀取）。
    fn locked_state(&self) -> Option<(i64, String)> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// execution context 摧毀／清空：丟棄建立的物件（下次使用重建）。
    ///
    /// `context_id` 為 `Some` 時只在該 context 對得上時清（`executionContextDestroyed`
    /// 帶單一 id）；`None` 為 `executionContextsCleared`，一律清。
    pub(crate) fn invalidate(&self, context_id: Option<i64>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match (state.as_ref(), context_id) {
            (Some((created, _)), Some(destroyed)) if *created == destroyed => {
                *state = None;
            }
            (Some(_), None) => {
                *state = None;
            }
            _ => {}
        }
    }

    /// 釋放（`Runtime.releaseObject`）不再使用的 injected 物件（關閉前清理）。
    pub(crate) async fn release(&self, page: &PageShared) {
        let Some((_, object_id)) = self.locked_state() else {
            return;
        };
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        let _ = page
            .send(
                "Runtime.releaseObject",
                json!({"objectId": object_id}),
            )
            .await;
    }
}

/// 在 injected object 上以 `Runtime.callFunctionOn` 呼叫一個宣告。
///
/// CDP 不會把呼叫目標傳進參數，Playwright（crExecutionContext 的
/// `_evaluateWithArguments`）在 `arguments` 開頭另外放
/// `{"objectId": <injected object>}`——宣告以 `(injected, …)` 接收。
/// `args` 是其餘參數（`functionDeclaration` 的第 1 個起）。
/// `return_by_value` 為 false 時回傳 CDP 的 RemoteObject（可能帶 objectId）；
/// 例外照 crExecutionContext 的 `getExceptionMessage` + `rewriteError` 語意：
/// JS 例外轉 Error（第一行），協定錯誤（context 已摧毀等）轉 destroyed 訊息。
pub(crate) async fn call_function_on(
    page: &PageShared,
    injected: &InjectedScript,
    declaration: &str,
    args: Value,
    return_by_value: bool,
) -> Result<Value, DriverError> {
    let object_id = injected.object_id(page).await?;
    let mut arguments = Vec::new();
    arguments.push(json!({"objectId": object_id}));
    if let Value::Array(extra) = args {
        arguments.extend(extra);
    }
    let reply = page
        .send(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": declaration,
                "arguments": arguments,
                "returnByValue": return_by_value,
                "awaitPromise": true,
                "userGesture": true,
            }),
        )
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        // 照 crExecutionContext 的 `rewriteError`：session 已結束（「Target page,
        // context or browser has been closed」）照原樣上拋；其餘協定錯誤（context
        // 已摧毀等，例如「Cannot find context with specified id」）一律視為
        // execution context 被導覽摧毀。
        Err(error) if error == DriverError::target_closed() => return Err(error),
        Err(_) => {
            return Err(DriverError::new(
                "Error",
                "Execution context was destroyed, most likely because of a navigation.",
            ));
        }
    };
    if let Some(details) = reply.get("exceptionDetails") {
        return Err(DriverError::new(
            "Error",
            first_line_of(&super::page::get_exception_message(details)),
        ));
    }
    Ok(reply.get("result").cloned().unwrap_or(Value::Null))
}

/// 解析選擇器：回傳 `(count, element)`；`element` 為 RemoteObject
/// （元素缺失時 `type: "undefined"`）。strict 違規直接拋 Error（第一行）。
pub(crate) async fn resolve_elements(
    page: &PageShared,
    injected: &InjectedScript,
    selector: &str,
    strict: bool,
    all: bool,
) -> Result<(i64, Option<Value>), DriverError> {
    let declaration = if all {
        // `evaluateAll`：回傳陣列本身（RemoteObject）。
        RESOLVE_ALL_SOURCE
    } else {
        RESOLVE_ELEMENTS_SOURCE
    };
    let result = call_function_on(
        page,
        injected,
        declaration,
        json!([{"value": {"selector": selector, "strict": strict}}]),
        false,
    )
    .await?;
    if all {
        // 陣列本身：缺失（undefined）時 count 為 0。
        let count = if result
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|r#type| r#type == "undefined")
        {
            0
        } else {
            1
        };
        return Ok((count, Some(result)));
    }
    let count = result.get("value").and_then(|value| value.get("count")).and_then(Value::as_i64).unwrap_or(0);
    let element = result
        .get("objectId")
        .filter(|_| result.get("value").and_then(|value| value.get("element")).is_some())
        .map(|object_id| json!({"objectId": object_id}));
    Ok((count, element))
}

/// 遠端物件是否為字串，回傳其值（`error:notconnected` 之類的哨兵判讀用）。
pub(crate) fn remote_string(remote: &Value) -> Option<&str> {
    if remote.get("type").and_then(Value::as_str) == Some("string") {
        return remote.get("value").and_then(Value::as_str);
    }
    None
}

/// 遠端物件是否為 undefined（missing / void 回傳值）。
pub(crate) fn remote_is_undefined(remote: &Value) -> bool {
    remote.get("type").and_then(Value::as_str) == Some("undefined")
}

/// 遠端物件的 value 欄位（缺失時 Null；`{type: "null"}`／`{value: null}` 都算 null）。
pub(crate) fn remote_value(remote: &Value) -> Value {
    remote.get("value").cloned().unwrap_or(Value::Null)
}

/// 訊息的第一行（X7：`message` 只保留第一行；多行訊息的 Call log 由呼叫端捨棄）。
pub(crate) fn first_line_of(content: &str) -> String {
    content
        .split('\n')
        .next()
        .unwrap_or(content)
        .trim_end_matches('\r')
        .to_string()
}
