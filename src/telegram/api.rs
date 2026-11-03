//! Telegram REST client：`telegram/TelegramApiClient.ts` 逐函式移植
//! （§4.2，S4.2）。
//!
//! - 端點 `https://api.telegram.org/bot<token>/<method>`、各送訊請求
//!   [`SEND_TIMEOUT_MS`] 10 秒、`getUpdates` 的總 deadline 為
//!   `timeoutSeconds * 1000 + SEND_TIMEOUT_MS`（涵蓋 request 與回應 body 的
//!   讀取，舊版以 `AbortSignal.any` 合成單一 deadline）、`text.slice(0, 4096)`
//!   與 `caption.slice(0, 1024)`（以 Unicode 純量近似 UTF-16 單位，差異見下方註解）。
//! - HTTP 用 reqwest，照舊版 Node fetch（undici）的行為：只用 HTTP/1.1、
//!   不讀 `http_proxy` 等系統 proxy 環境變數、任何 redirect 都讓請求失敗
//!   （舊版 `redirect: 'error'`）。
//! - 舊測試以注入 fetch mock 介入；依計畫的既有模式（S4.3 Discord），本移植
//!   的假 HTTP 改以 `base_url` 指向本機 ephemeral server，測試以錄到的原始
//!   請求逐欄位斷言 URL／headers／body。
//! - 逾時／中止分類照舊版 `call` 的 catch：外部訊號取消（catch 先看
//!   `signal.aborted`）→ `Telegram request aborted`（`aborted`）；deadline
//!   到期（舊版 deadline abort 的 reason 為 `TimeoutError`，本移植對應
//!   reqwest 逾時）→ `Telegram request timed out`（`timeout`）；其他傳輸
//!   失敗 → `Telegram network request failed`（`network`）。回應 body 的
//!   讀取／解析失敗照舊版 json catch：非逾時 → `Telegram returned invalid
//!   JSON`（`invalid_response`，附 HTTP status）。errorCategory 集合
//!   `aborted / http / invalid_response / network / timeout / unknown` 照抄。
//! - 回應檢查照舊版：`!response.ok || payload.ok !== true ||
//!   payload.result === undefined` 時依 `description` 欄位的有無選訊息
//!   （`Telegram API rejected <method>`／`Telegram API returned HTTP <status>`）。
//!   舊版把 `result` 原樣回傳，形狀不對會在 bot 端爆 TypeError；本移植在
//!   `getUpdates` 的 result 解析失敗（如 `result: null`）以 `invalid_response`
//!   分類丟出（見 [`TelegramApiClient::get_updates`]），呼叫端結果仍是對應的
//!   poll 失敗與重試，流程等價。
#![allow(dead_code)]

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::logging::redact::js_number_to_string;

/// 舊版 `SEND_TIMEOUT_MS`。
const SEND_TIMEOUT_MS: Duration = Duration::from_millis(10_000);
/// 舊版 `https://api.telegram.org`。
const DEFAULT_API_BASE: &str = "https://api.telegram.org";
/// 舊版 `text.slice(0, 4_096)` 的上限。
const MAX_TEXT_LENGTH: usize = 4_096;
/// 舊版 `caption.slice(0, 1_024)` 的上限。
const MAX_CAPTION_LENGTH: usize = 1_024;

/// 對應舊版 `TelegramMessage`；`chat.id` 照舊版是 `number | string`。
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct TelegramMessage {
    #[serde(rename = "message_id")]
    pub(crate) message_id: f64,
    pub(crate) text: Option<String>,
    pub(crate) chat: TelegramChat,
}

/// 對應舊版 `TelegramMessage.chat`。
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct TelegramChat {
    pub(crate) id: TelegramChatId,
}

/// 對應舊版 `chat.id: number | string`。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TelegramChatId {
    Number(f64),
    String(String),
}

impl TelegramChatId {
    /// 舊版 `String(message.chat.id)`：數字照 `String(number)`、字串照原值。
    pub(crate) fn to_chat_id_string(&self) -> String {
        match self {
            Self::Number(value) => js_number_to_string(*value),
            Self::String(value) => value.clone(),
        }
    }
}

impl<'de> Deserialize<'de> for TelegramChatId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::Number(number) => {
                let converted = number
                    .as_f64()
                    .ok_or_else(|| serde::de::Error::custom("chat.id 必須是可形成 f64 的數字"))?;
                Ok(Self::Number(converted))
            }
            serde_json::Value::String(text) => Ok(Self::String(text)),
            // 舊版在兩種型別之外的值會讓 bot 端的 `String(chat.id)` 生效但
            // 顯示 `[object Object]`；本移植在解析端拒收（invalid_response
            // 分類，見模組文件）。
            _ => Err(serde::de::Error::custom(
                "chat.id must be a number or string",
            )),
        }
    }
}

/// 對應舊版 `TelegramUpdate`。
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct TelegramUpdate {
    #[serde(rename = "update_id")]
    pub(crate) update_id: f64,
    pub(crate) message: Option<TelegramMessage>,
}

/// 對應舊版 `TelegramBotCommand`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct TelegramBotCommand {
    pub(crate) command: String,
    pub(crate) description: String,
}

/// 對應舊版 `TelegramReplyKeyboardMarkup` 的按鈕（`text`）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct TelegramKeyboardButton {
    pub(crate) text: String,
}

/// 對應舊版 `TelegramReplyKeyboardMarkup`；欄位順序照舊版 `COMMAND_KEYBOARD`
/// 物件字面的建立順序（`keyboard`、`is_persistent`、`resize_keyboard`），
/// `None` 的欄位序列化省略（照 `JSON.stringify` 的 undefined 行為）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct TelegramReplyKeyboardMarkup {
    pub(crate) keyboard: Vec<Vec<TelegramKeyboardButton>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) is_persistent: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) resize_keyboard: Option<bool>,
}

/// 對應舊版 `TelegramSendMessageOptions`（唯一的 `reply_markup` 欄位）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TelegramSendMessageOptions {
    pub(crate) reply_markup: Option<TelegramReplyKeyboardMarkup>,
}

/// 對應舊版 `TelegramApi` 介面。
///
/// 舊版各方法的逾時由呼叫端以 `AbortSignal.timeout` 注入 fetch；本移植把逾時
/// 帶進 [`TelegramApiClient`]。`getUpdates` 另接收外部訊號（舊版 caller 的
/// abort signal），以 [`CancellationToken`] 呈現：取消即中止請求並回
/// `aborted` 分類。
#[async_trait]
pub(crate) trait TelegramApi: Send + Sync {
    /// 舊版 `getUpdates(offset, timeoutSeconds, signal)`。
    async fn get_updates(
        &self,
        offset: Option<f64>,
        timeout_seconds: f64,
        cancel: &CancellationToken,
    ) -> Result<Vec<TelegramUpdate>, TelegramApiError>;

    /// 舊版 `setMyCommands(commands)`。
    async fn set_my_commands(
        &self,
        commands: &[TelegramBotCommand],
    ) -> Result<(), TelegramApiError>;

    /// 舊版 `sendMessage(chatId, text, options?)`；`options` 為 [`None`] 對應
    /// 舊版的預設值 `{}`（reply_markup 缺席）。
    async fn send_message(
        &self,
        chat_id: &str,
        text: &str,
        options: Option<&TelegramSendMessageOptions>,
    ) -> Result<(), TelegramApiError>;

    /// 舊版 `sendPhoto(chatId, image, filename, caption)`。
    async fn send_photo(
        &self,
        chat_id: &str,
        image: &[u8],
        filename: &str,
        caption: &str,
    ) -> Result<(), TelegramApiError>;
}

/// 對應舊版 `TelegramApiClientOptions`。
pub(crate) struct TelegramApiClientOptions {
    pub(crate) bot_token: String,
    /// 舊版無此參數（舊測試注入 fetch）；本移植以本機 ephemeral server 取代，
    /// 測試用 `Some(base)` 覆寫，正式環境用 `None`（[`DEFAULT_API_BASE`]）。
    pub(crate) base_url: Option<String>,
}

/// 對應舊版 `TelegramApiError`（`name` 恆為 `TelegramApiError`）。
///
/// `method` 照舊版是第一個參數的公開欄位；餘下欄位供 log 描述組裝
/// （[`TelegramApiError::describe`]）與 [`Display`]（照舊版
/// `String(error)`＝`name: message`）。
#[derive(Clone, Debug)]
pub(crate) struct TelegramApiError {
    pub(crate) method: &'static str,
    message: String,
    /// 舊版 `Error.name`（`describe` 的 `errorClass`）：正式客戶端固定
    /// `TelegramApiError`；[`TelegramApiError::generic`] 供測試替身呈現
    /// 舊測試的 generic `Error`。
    pub(crate) error_class: &'static str,
    pub(crate) category: TelegramErrorCategory,
    /// 舊版 `httpStatus?: number`（`undefined` → [`None`]）。
    pub(crate) http_status: Option<u16>,
}

impl TelegramApiError {
    /// 對應舊版建構子。
    pub(crate) fn new(
        method: &'static str,
        message: impl Into<String>,
        category: TelegramErrorCategory,
        http_status: Option<u16>,
    ) -> Self {
        Self {
            method,
            message: message.into(),
            error_class: "TelegramApiError",
            category,
            http_status,
        }
    }

    /// 舊測試替身用的 generic `Error` 佔位（舊碼 `new Error(…)`：
    /// `instanceof TelegramApiError` 不成立、`Error.name` 為 `'Error'`，
    /// `describe` 的 `httpStatus` 輸出 `null`；正式碼不會使用）。
    pub(crate) fn generic(message: &str) -> Self {
        Self {
            method: "unknown",
            message: message.to_string(),
            error_class: "Error",
            category: TelegramErrorCategory::Unknown,
            http_status: None,
        }
    }

    /// 對應舊版 `Error.name`。
    pub(crate) fn name(&self) -> &'static str {
        self.error_class
    }

    /// 對應舊版 `Error.message`。
    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    /// 對應舊版 `describeTelegramError` 的 `TelegramApiError` 分支：
    /// `{errorClass: name, httpStatus: httpStatus ?? null, errorCategory}`。
    pub(crate) fn describe(&self) -> TelegramErrorDescriptor {
        TelegramErrorDescriptor {
            error_class: self.error_class,
            http_status: self.http_status,
            error_category: self.category,
        }
    }
}

impl core::fmt::Display for TelegramApiError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // 舊版 `String(error)` 輸出 `${name}: ${message}`。
        write!(f, "{}: {}", self.name(), self.message())
    }
}

impl std::error::Error for TelegramApiError {}

/// 對應舊版 `TelegramErrorCategory`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TelegramErrorCategory {
    Aborted,
    Http,
    InvalidResponse,
    Network,
    Timeout,
    Unknown,
}

impl TelegramErrorCategory {
    /// 舊版欄位值的字面（log 字串）。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Aborted => "aborted",
            Self::Http => "http",
            Self::InvalidResponse => "invalid_response",
            Self::Network => "network",
            Self::Timeout => "timeout",
            Self::Unknown => "unknown",
        }
    }
}

/// 對應舊版 `describeTelegramError` 的 `TelegramErrorDescriptor` 值
/// （log 欄位 `errorClass`／`httpStatus`（缺值 `null`）／`errorCategory`）。
pub(crate) struct TelegramErrorDescriptor {
    pub(crate) error_class: &'static str,
    /// 舊版 `number | null`：實例無 `httpStatus` 時輸出 `null`
    /// （Rust 端以 [`None`] 對應，log 組裝時放 null）。
    pub(crate) http_status: Option<u16>,
    pub(crate) error_category: TelegramErrorCategory,
}

/// 對應舊版 `TelegramApiClient`。
pub(crate) struct TelegramApiClient {
    bot_token: String,
    base_url: String,
    http: reqwest::Client,
}

/// `redirect: 'error'` 的錯誤值（reqwest 使用；twitch／discord transport 的
/// 同一模式）。
#[derive(Debug)]
struct RedirectRejected;

impl core::fmt::Display for RedirectRejected {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("redirect is not allowed")
    }
}

impl std::error::Error for RedirectRejected {}

/// [`maybe_cancel`] 的任務結果：`Done` 為任務正常完成；`Failed` 為任務端
/// 傳輸失敗（分類由呼叫端依階段決定）；`Cancelled` 為外部訊號先於任務、
/// 或任務失敗時訊號已中止（照舊版 catch 先看 `signal.aborted` 的偏置）。
enum TransportOutcome<T> {
    Done(T),
    Failed(reqwest::Error),
    Cancelled,
}

/// 舊版 `AbortSignal.any` 的分層表達：`cancel` 存在時任務與取消並行。
async fn maybe_cancel<T>(
    cancel: Option<&CancellationToken>,
    task: impl core::future::Future<Output = Result<T, reqwest::Error>>,
) -> TransportOutcome<T> {
    match cancel {
        None => match task.await {
            Ok(value) => TransportOutcome::Done(value),
            Err(error) => TransportOutcome::Failed(error),
        },
        Some(token) => {
            tokio::select! {
                outcome = task => match outcome {
                    Ok(value) => TransportOutcome::Done(value),
                    Err(error) => {
                        // 舊版 catch 先看 signal.aborted：兩者同時成立時分類為 abort。
                        if token.is_cancelled() {
                            TransportOutcome::Cancelled
                        } else {
                            TransportOutcome::Failed(error)
                        }
                    }
                },
                _ = token.cancelled() => TransportOutcome::Cancelled,
            }
        }
    }
}

/// 舊版 `'Telegram request timed out'`（deadline 到期；reqwest 逾時分類，
/// 對應舊版 deadline abort 的 reason 為 `TimeoutError`）。
fn timeout_api_error(method: &'static str) -> TelegramApiError {
    TelegramApiError::new(
        method,
        "Telegram request timed out",
        TelegramErrorCategory::Timeout,
        None,
    )
}

/// 舊版 `'Telegram request aborted'`（外部訊號中止；對應舊版 reason 非
/// `TimeoutError` 的 abort）。
fn aborted_api_error(method: &'static str) -> TelegramApiError {
    TelegramApiError::new(
        method,
        "Telegram request aborted",
        TelegramErrorCategory::Aborted,
        None,
    )
}

/// 舊版 `'Telegram network request failed'`。
fn network_api_error(method: &'static str) -> TelegramApiError {
    TelegramApiError::new(
        method,
        "Telegram network request failed",
        TelegramErrorCategory::Network,
        None,
    )
}

/// 舊版 `'Telegram returned invalid JSON'`（json catch 分類，附 HTTP 狀態）。
fn invalid_json_api_error(method: &'static str, status: u16) -> TelegramApiError {
    TelegramApiError::new(
        method,
        "Telegram returned invalid JSON",
        TelegramErrorCategory::InvalidResponse,
        Some(status),
    )
}

/// 舊版 `content.slice(0, N)`：以 Unicode 純量（char）近似 UTF-16 單位
/// （照 discord 的 `truncate_content`；差異只在「第 N 個單位恰為 astral
/// 字元代理對中點」的邊界輸入，實務文字不會發生）。
fn truncate_unicode(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

/// 對應舊版 `JSON.stringify` 的數字輸出：整數的 f64 不輸出 `.0`（照
/// [`js_number_to_string`] 的 ryu-js 形式）；非有限值照 `JSON.stringify`
/// 輸出 `null`。
fn js_number_value(value: f64) -> JsonValue {
    if !value.is_finite() {
        return JsonValue::Null;
    }
    // ryu-js 的輸出（如 `25`）必為合法 JSON 數字；防衛性失敗照 null。
    serde_json::from_str(&js_number_to_string(value)).unwrap_or(JsonValue::Null)
}

impl TelegramApiClient {
    /// 對應舊版建構子；reqwest client 無法建立時回傳錯誤（舊版
    /// `globalThis.fetch` 必在，無對應失敗路徑；呼叫端組裝時視為啟動失敗）。
    pub(crate) fn new(options: TelegramApiClientOptions) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            // 舊版 Node fetch（undici）：HTTP/1.1、不讀系統 proxy 環境變數。
            .http1_only()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                // 舊版 `redirect: 'error'`：任何 redirect 都讓 fetch reject。
                if attempt.previous().is_empty() {
                    attempt.follow()
                } else {
                    attempt.error(RedirectRejected)
                }
            }))
            .build()?;
        Ok(Self {
            bot_token: options.bot_token,
            base_url: options
                .base_url
                .unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
            http: client,
        })
    }

    /// 舊版 `getUpdates` 的 deadline：`timeoutSeconds * 1_000 +
    /// SEND_TIMEOUT_MS`。涵蓋 request 與 body 讀取（舊版單一
    /// `AbortSignal.timeout`）。
    fn get_updates_deadline(timeout_seconds: f64) -> Duration {
        let milliseconds = timeout_seconds * 1_000.0 + SEND_TIMEOUT_MS.as_millis() as f64;
        // timeoutSeconds 由 config 驗證端把關；防衛性處理非有限值（照舊版
        // `AbortSignal.timeout(NaN)` 會 RangeError，呼叫端以送訊逾時
        // [`SEND_TIMEOUT_MS`] 的等價底線收斂）。
        let bounded = if milliseconds.is_finite() && milliseconds > 0.0 {
            milliseconds
        } else {
            SEND_TIMEOUT_MS.as_millis() as f64
        };
        Duration::from_millis(bounded as u64)
    }

    /// 舊版 `call<T>` 的共用主體（fetch 的 catch 分類 → body 讀取的 json
    /// catch 分類 → envelope 檢查）。`deadline` 照舊版各呼叫端的
    /// `AbortSignal.timeout`；`cancel` 為 `getUpdates` 的外部訊號（舊版以
    /// `AbortSignal.any` 合成，本移植分層表達：deadline 由 builder 的
    /// `.timeout` 呈現、外部訊號由此參數呈現）。
    async fn call_json(
        &self,
        method_name: &'static str,
        body: Option<String>,
        deadline: Duration,
        cancel: Option<&CancellationToken>,
    ) -> Result<JsonValue, TelegramApiError> {
        let url = format!("{}/bot{}/{}", self.base_url, self.bot_token, method_name);
        let mut builder = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .timeout(deadline);
        if let Some(body) = body {
            builder = builder.body(body);
        }

        // 舊版 `fetch` 的 catch：訊號中止（外部或 deadline）→ abort 錯誤；
        // 其他傳輸失敗 → `Telegram network request failed`。
        let response = match maybe_cancel(cancel, builder.send()).await {
            TransportOutcome::Done(response) => response,
            TransportOutcome::Failed(error) => {
                if error.is_timeout() {
                    return Err(timeout_api_error(method_name));
                }
                return Err(network_api_error(method_name));
            }
            TransportOutcome::Cancelled => return Err(aborted_api_error(method_name)),
        };

        let status = response.status().as_u16();
        // 舊版 `await response.json()` 的 catch（單一 catch 涵蓋讀取與解析）：
        // 訊號中止 → abort 錯誤；reqwest 的 deadline → timeout；其他讀取或
        // 解析失敗 → invalid_response（附 status，與網路降級分歧照舊版）。
        let text = match maybe_cancel(cancel, response.text()).await {
            TransportOutcome::Done(text) => text,
            TransportOutcome::Failed(error) => {
                if error.is_timeout() {
                    return Err(timeout_api_error(method_name));
                }
                return Err(invalid_json_api_error(method_name, status));
            }
            TransportOutcome::Cancelled => return Err(aborted_api_error(method_name)),
        };

        let payload = match serde_json::from_str::<JsonValue>(&text) {
            Ok(payload) => payload,
            Err(_) => return Err(invalid_json_api_error(method_name, status)),
        };

        check_envelope(method_name, status, &payload)
    }

    /// 舊版 `callForm<T>`（multipart；無外部訊號，逾時為
    /// [`SEND_TIMEOUT_MS`]）與共用尾段（同 [`Self::call_json`] 的 json catch
    /// 與 envelope 檢查）。
    async fn call_form(
        &self,
        method_name: &'static str,
        form: reqwest::multipart::Form,
    ) -> Result<JsonValue, TelegramApiError> {
        let url = format!("{}/bot{}/{}", self.base_url, self.bot_token, method_name);
        let builder = self
            .http
            .post(&url)
            .multipart(form)
            .timeout(SEND_TIMEOUT_MS);

        let response = match builder.send().await {
            Ok(response) => response,
            Err(error) => {
                if error.is_timeout() {
                    return Err(timeout_api_error(method_name));
                }
                return Err(network_api_error(method_name));
            }
        };

        let status = response.status().as_u16();
        let text = match response.text().await {
            Ok(text) => text,
            Err(error) => {
                if error.is_timeout() {
                    return Err(timeout_api_error(method_name));
                }
                return Err(invalid_json_api_error(method_name, status));
            }
        };

        let payload = match serde_json::from_str::<JsonValue>(&text) {
            Ok(payload) => payload,
            Err(_) => return Err(invalid_json_api_error(method_name, status)),
        };

        check_envelope(method_name, status, &payload)
    }
}

/// 對應舊版 `payload = await response.json()` 之後的檢查：
/// `!response.ok || payload.ok !== true || payload.result === undefined`
/// 時依 `description` 欄位的有無選訊息；通過則回傳 `payload.result`。
///
/// `ok` 缺欄位或型別不對也照舊版視為不成立（`undefined !== true`）；
/// `result` 為 `null` 時照舊版通過檢查（`null !== undefined`），typed 層的
/// 解析失敗以 invalid_response 丟出（見 [`TelegramApiClient::get_updates`]）。
fn check_envelope(
    method_name: &'static str,
    status: u16,
    payload: &JsonValue,
) -> Result<JsonValue, TelegramApiError> {
    let response_ok = (200..300).contains(&status);
    let payload_ok = payload.get("ok").and_then(JsonValue::as_bool) == Some(true);
    let has_result = payload.get("result").is_some();
    if !response_ok || !payload_ok || !has_result {
        let message = if payload.get("description").is_none() {
            format!("Telegram API returned HTTP {status}")
        } else {
            format!("Telegram API rejected {method_name}")
        };
        return Err(TelegramApiError::new(
            method_name,
            message,
            TelegramErrorCategory::Http,
            Some(status),
        ));
    }
    Ok(payload.get("result").cloned().unwrap_or(JsonValue::Null))
}

#[async_trait]
impl TelegramApi for TelegramApiClient {
    async fn get_updates(
        &self,
        offset: Option<f64>,
        timeout_seconds: f64,
        cancel: &CancellationToken,
    ) -> Result<Vec<TelegramUpdate>, TelegramApiError> {
        // 舊版 body 的物件字面順序：`{offset?, timeout, allowed_updates}`。
        let mut body = serde_json::Map::new();
        if let Some(offset) = offset {
            body.insert("offset".to_string(), js_number_value(offset));
        }
        body.insert("timeout".to_string(), js_number_value(timeout_seconds));
        body.insert(
            "allowed_updates".to_string(),
            JsonValue::Array(vec![JsonValue::String("message".to_string())]),
        );

        let result = self
            .call_json(
                "getUpdates",
                Some(serde_json::Value::Object(body).to_string()),
                Self::get_updates_deadline(timeout_seconds),
                Some(cancel),
            )
            .await?;
        // 舊版 result 原樣回傳；typed 解析失敗（如 `result: null`）以
        // invalid_response 分類丟出（照模組文件的取捨）。
        serde_json::from_value::<Vec<TelegramUpdate>>(result).map_err(|_| {
            TelegramApiError::new(
                "getUpdates",
                "Telegram returned invalid JSON",
                TelegramErrorCategory::InvalidResponse,
                None,
            )
        })
    }

    async fn set_my_commands(
        &self,
        commands: &[TelegramBotCommand],
    ) -> Result<(), TelegramApiError> {
        // 舊版 `{commands}`；元素欄位照 `JSON.stringify` 的建立順序。
        let body = serde_json::json!({ "commands": commands }).to_string();
        self.call_json("setMyCommands", Some(body), SEND_TIMEOUT_MS, None)
            .await?;
        Ok(())
    }

    async fn send_message(
        &self,
        chat_id: &str,
        text: &str,
        options: Option<&TelegramSendMessageOptions>,
    ) -> Result<(), TelegramApiError> {
        // 舊版 `{chat_id, text: text.slice(0, 4_096), ...options}`：reply_markup
        // 在最後（spread 於 chat_id／text 之後）。
        let mut body = serde_json::Map::new();
        body.insert(
            "chat_id".to_string(),
            JsonValue::String(chat_id.to_string()),
        );
        body.insert(
            "text".to_string(),
            JsonValue::String(truncate_unicode(text, MAX_TEXT_LENGTH)),
        );
        if let Some(options) = options
            && let Some(reply_markup) = &options.reply_markup
        {
            let value = serde_json::to_value(reply_markup).map_err(|_| {
                // 全為字串／布林欄位的防衛性分支（序列化理論上不會失敗）。
                TelegramApiError::new(
                    "sendMessage",
                    "Telegram network request failed",
                    TelegramErrorCategory::Network,
                    None,
                )
            })?;
            body.insert("reply_markup".to_string(), value);
        }

        self.call_json(
            "sendMessage",
            Some(serde_json::Value::Object(body).to_string()),
            SEND_TIMEOUT_MS,
            None,
        )
        .await?;
        Ok(())
    }

    async fn send_photo(
        &self,
        chat_id: &str,
        image: &[u8],
        filename: &str,
        caption: &str,
    ) -> Result<(), TelegramApiError> {
        // 舊版 FormData：chat_id、caption（slice 1024）、photo（Blob type
        // 'image/png' ＋ filename；part 的 content-type 由 reqwest 轉成
        // part header）。
        let form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .text("caption", truncate_unicode(caption, MAX_CAPTION_LENGTH))
            .part(
                "photo",
                reqwest::multipart::Part::bytes(image.to_vec())
                    .file_name(filename.to_string())
                    .mime_str("image/png")
                    .map_err(|_| {
                        // 舊版 FormData 字面的固定 type 無對應失敗路徑；
                        // reqwest 只在非法 header 值時失敗，視為傳輸層錯誤。
                        network_api_error("sendPhoto")
                    })?,
            );
        self.call_form("sendPhoto", form).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
