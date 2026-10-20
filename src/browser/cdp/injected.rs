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
//! 在 injected object 上呼叫）。本檔放呼叫的骨架與回傳值解析；各 API 的
//! 重試／逾時流程在 [`super::locator`]。
//!
//! 以下 API 尚無正式呼叫端，等對應步驟接上後移除 allow：
//! - `InjectedScript::release`（關閉前清理）。
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
/// （先 `parseSelector`）、在主文件上查詢、依需要拋出 strict 違規，回傳數量。
const RESOLVE_ELEMENTS_SOURCE: &str = r#"(injected, { selector, strict }) => {
  const parsed = injected.parseSelector(selector);
  const elements = injected.querySelectorAll(parsed, injected.document);
  if (strict && elements.length > 1)
    throw injected.strictModeViolationError(parsed, elements);
  return elements.length;
}"#;

/// 解析選擇器：回傳**元素本身**（缺失時 undefined；`returnByValue: false` →
/// RemoteObject 帶 objectId，click／hover 的 DOM 指令需要）。
const RESOLVE_ELEMENT_SOURCE: &str = r#"(injected, { selector, strict }) => {
  const parsed = injected.parseSelector(selector);
  const elements = injected.querySelectorAll(parsed, injected.document);
  if (strict && elements.length > 1)
    throw injected.strictModeViolationError(parsed, elements);
  return elements[0] ?? undefined;
}"#;

/// 檢查元素狀態（click：`['visible','enabled','stable']`、hover：
/// `['visible','stable']`；照 `_performPointerAction` 的 `injected.checkElementStates`。
/// 回 undefined（成立）、'error:notconnected' 或 `{missingState}`）。
pub(crate) const CHECK_ELEMENT_STATES_SOURCE: &str =
    r#"(injected, node, states) => injected.checkElementStates(node, states)"#;

/// 安裝 hit target 攔截器（照 `_performPointerAction`；回 `{stop}` 物件、
/// 蓋住元素的描述字串或 'error:notconnected'，`returnByValue: false`）。
pub(crate) const SETUP_HIT_TARGET_INTERCEPTOR_SOURCE: &str = r#"(injected, node, actionType, hitPoint, blockAllEvents) =>
      injected.setupHitTargetInterceptor(node, actionType, hitPoint, blockAllEvents)"#;

/// 停止攔截器（照 `_performPointerAction` 的 `h.stop()`；回 'done' 或
/// `{hitTargetDescription}`——第一個事件沒落在目標元素上）。
pub(crate) const STOP_HIT_TARGET_INTERCEPTOR_SOURCE: &str =
    r#"(interceptor) => interceptor.stop()"#;

/// 以 `scrollIntoView(options)` 的對齊輪替捲動（`_retryPointerAction` 第 n 次
/// 重試用的 forceScrollOptions；元素捲不出 sticky 蓋層時換對齊再試）。
pub(crate) const SCROLL_INTO_VIEW_SOURCE: &str = r#"(node, options) => {
  if (node.nodeType === 1) node.scrollIntoView(options);
  return 'done';
}"#;

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
                    page.send_may_fail("Runtime.releaseObject", json!({"objectId": stale}));
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

    /// 新 execution context 建立（主框架）：舊 context 上的 injected 物件已失效，
    /// 對不上新 id 時清空（照 frames.ts 的 `contextCreated`：舊 context 摧毀）。
    pub(crate) fn invalidate_on_new_context(&self, new_context_id: i64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((created, _)) = state.as_ref()
            && *created != new_context_id
        {
            *state = None;
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
            .send("Runtime.releaseObject", json!({"objectId": object_id}))
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
        // context or browser has been closed」）與 crash（「Target crashed 」）
        // 照原樣上拋；其餘協定錯誤（context 已摧毀等，例如「Cannot find context
        // with specified id」）一律視為 execution context 被導覽摧毀。
        Err(error) if error == DriverError::target_closed() => return Err(error),
        Err(error) if error.is_crashed() => return Err(error),
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

/// 在任意遠端物件上以 `Runtime.callFunctionOn` 呼叫一個宣告（element handle 用）。
pub(crate) async fn call_on_object(
    page: &PageShared,
    declaration: &str,
    object_id: &str,
    args: Value,
    return_by_value: bool,
) -> Result<Value, DriverError> {
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
        .await?;
    if let Some(details) = reply.get("exceptionDetails") {
        return Err(DriverError::new(
            "Error",
            first_line_of(&super::page::get_exception_message(details)),
        ));
    }
    Ok(reply.get("result").cloned().unwrap_or(Value::Null))
}

/// 解析選擇器：回傳符合的元素數量。strict 違規直接拋 Error（第一行）。
pub(crate) async fn resolve_count(
    page: &PageShared,
    injected: &InjectedScript,
    selector: &str,
    strict: bool,
) -> Result<i64, DriverError> {
    let result = call_function_on(
        page,
        injected,
        RESOLVE_ELEMENTS_SOURCE,
        json!([{"value": {"selector": selector, "strict": strict}}]),
        true,
    )
    .await?;
    if remote_is_undefined(&result) {
        return Ok(0);
    }
    Ok(remote_value(&result).as_i64().unwrap_or(0))
}

/// 解析選擇器並回傳**元素本身**的 RemoteObject（帶 objectId；click／hover 的
/// `DOM.scrollIntoViewIfNeeded`／`DOM.getContentQuads` 需要）。缺失時回 None。
pub(crate) async fn resolve_element_object(
    page: &PageShared,
    injected: &InjectedScript,
    selector: &str,
    strict: bool,
) -> Result<Option<Value>, DriverError> {
    let result = call_function_on(
        page,
        injected,
        RESOLVE_ELEMENT_SOURCE,
        json!([{"value": {"selector": selector, "strict": strict}}]),
        false,
    )
    .await?;
    if remote_is_undefined(&result) {
        return Ok(None);
    }
    Ok(Some(result))
}

/// 遠端物件的 objectId 欄位（click／hover 解析出的元素與攔截器把手用）。
pub(crate) fn remote_object_id(remote: &Value) -> Option<&str> {
    remote.get("objectId").and_then(Value::as_str)
}

/// 檢查元素狀態的結果（`injected.checkElementStates` 的三種回傳）。
pub(crate) enum ElementStateCheck {
    /// 所有狀態成立。
    Ready,
    /// 元素已脫離 DOM（可重試；外層會重新解析元素）。
    NotConnected,
    /// 缺少某個狀態（`visible`／`enabled`／`stable` 之一；可重試）。
    Missing(String),
}

/// `injected.checkElementStates(node, states)`（click／hover 的狀態等待；
/// stable 在頁內以連續兩次 rAF 的相同位置判定）。
pub(crate) async fn check_element_states(
    page: &PageShared,
    injected: &InjectedScript,
    element_object_id: &str,
    states: &[&str],
) -> Result<ElementStateCheck, DriverError> {
    let result = call_function_on(
        page,
        injected,
        CHECK_ELEMENT_STATES_SOURCE,
        json!([
            {"objectId": element_object_id},
            {"value": states},
        ]),
        true,
    )
    .await?;
    if remote_is_undefined(&result) {
        return Ok(ElementStateCheck::Ready);
    }
    if let Some(description) = remote_string(&result)
        && description == "error:notconnected"
    {
        return Ok(ElementStateCheck::NotConnected);
    }
    if let Some(state) = remote_value(&result)
        .get("missingState")
        .and_then(Value::as_str)
    {
        return Ok(ElementStateCheck::Missing(state.to_string()));
    }
    Ok(ElementStateCheck::Ready)
}

/// 安裝 hit target 攔截器的結果。
pub(crate) enum HitTargetSetup {
    /// 已安裝（`{stop}` 物件的 objectId；動作後用 `stop` 拆掉）。
    Installed(String),
    /// 元素途中脫離 DOM（可重試）。
    NotConnected,
    /// 初步檢查失敗：元素被描述的元素蓋住（可重試）。
    HitTargetDescription(String),
}

/// `injected.setupHitTargetInterceptor(node, actionType, hitPoint, blockAllEvents)`
/// （照 `_performPointerAction`；`returnByValue: false` 保留 `{stop}` 的 objectId）。
pub(crate) async fn setup_hit_target_interceptor(
    page: &PageShared,
    injected: &InjectedScript,
    element_object_id: &str,
    action_type: &str,
    hit_point: Option<(f64, f64)>,
    block_all_events: bool,
) -> Result<HitTargetSetup, DriverError> {
    let hit_point_value = match hit_point {
        Some((x, y)) => json!({"x": x, "y": y}),
        None => Value::Null,
    };
    let result = call_function_on(
        page,
        injected,
        SETUP_HIT_TARGET_INTERCEPTOR_SOURCE,
        json!([
            {"objectId": element_object_id},
            {"value": action_type},
            {"value": hit_point_value},
            {"value": block_all_events},
        ]),
        false,
    )
    .await?;
    // 有 objectId：回傳的是 `{stop}` 物件。
    if let Some(object_id) = remote_object_id(&result) {
        return Ok(HitTargetSetup::Installed(object_id.to_string()));
    }
    // 字串：'error:notconnected' 或蓋住元素的描述。
    if let Some(description) = remote_string(&result) {
        if description == "error:notconnected" {
            return Ok(HitTargetSetup::NotConnected);
        }
        return Ok(HitTargetSetup::HitTargetDescription(
            description.to_string(),
        ));
    }
    Ok(HitTargetSetup::NotConnected)
}

/// 停止攔截器的結果。
pub(crate) enum HitTargetStop {
    /// 沒有事件落在錯誤的元素上。
    Done,
    /// 第一個事件落在描述的元素上（可重試）。
    HitTargetDescription(String),
}

/// `interceptor.stop()`（照 `_performPointerAction` 動作之後的呼叫；錯誤由
/// 呼叫端吞掉視同 [`HitTargetStop::Done`]）。
pub(crate) async fn stop_hit_target_interceptor(
    page: &PageShared,
    interceptor_object_id: &str,
) -> Result<HitTargetStop, DriverError> {
    let result = call_on_object(
        page,
        STOP_HIT_TARGET_INTERCEPTOR_SOURCE,
        interceptor_object_id,
        json!([]),
        true,
    )
    .await?;
    if let Some(description) = remote_value(&result)
        .get("hitTargetDescription")
        .and_then(Value::as_str)
    {
        return Ok(HitTargetStop::HitTargetDescription(description.to_string()));
    }
    Ok(HitTargetStop::Done)
}

/// 釋放遠端物件（`Runtime.releaseObject`；fire-and-forget 照 `_sendMayFail`，
/// 元素／攔截器把手用完即回收）。
pub(crate) fn release_object(page: &PageShared, object_id: &str) {
    page.send_may_fail("Runtime.releaseObject", json!({"objectId": object_id}));
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
