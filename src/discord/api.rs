//! Discord REST client：`discord/DiscordApiClient.ts` 逐函式移植（§4.2，S4.3）。
//!
//! - 端點路徑、方法、headers 與逾時照舊版常數：API 基底 [`DISCORD_API_BASE`]、
//!   各送訊請求 [`SEND_TIMEOUT_MS`] 10 秒、content 截 2000 字（舊版
//!   `content.slice(0, 2000)`，以 Unicode 純量近似 UTF-16 單位，差異見模組文件）、
//!   interaction callback type 4／5。
//! - HTTP 用 reqwest，照舊版 Node fetch（undici）的行為：只用 HTTP/1.1
//!   （`http1_only()`，不開 http2）、不讀 `http_proxy` 等系統 proxy 環境變數
//!   （`no_proxy()`）、任何 redirect 都讓請求失敗（舊版 `redirect: 'error'`）。
//!   舊版的逾時由呼叫端以 `AbortSignal.timeout` 注入 fetch；本移植的
//!   `get_gateway_bot` 由呼叫端傳入 timeout（對應舊版 bot 的 10 秒 signal），
//!   其餘請求固定 [`SEND_TIMEOUT_MS`]。
//! - 假 HTTP 依計畫不注入 transport，改由 `base_url` 指向本機 ephemeral server
//!   （測試以錄到的原始請求逐欄位斷言 URL／headers／body；official
//!   `fetch: typeof` 介入縫在 Node 專屬、Rust 端以 base_url 取代）。
//! - 逾時的錯誤分類照舊版 `call`／`callForm` 的 catch：逾時（signal 中止）
//!   重新拋出同類逾時錯誤、其他傳輸失敗包成『Discord network request failed』；
//!   本移植以 [`DiscordApiErrorKind::Timeout`] 與 [`DiscordApiErrorKind::Network`]
//!   呈現（舊版逾時不是 `DiscordApiError`，本移植共用同一枚舉以簡化型別，
//!   訊息不會外洩）。回應解析照舊版 `parseDiscordResponse`：204 回空、
//!   JSON 解析失敗時若狀態為成功回空、否則丟 `Discord returned invalid JSON`、
//!   非成功狀態且 payload 帶 `message` 欄位丟 `Discord API rejected request`、
//!   其他非成功狀態丟 `Discord API returned HTTP <status>`。
#![allow(dead_code)]

use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;

/// 對應舊版 `DISCORD_API_BASE`。
pub(crate) const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
/// 舊版 `SEND_TIMEOUT_MS`。
const SEND_TIMEOUT_MS: Duration = Duration::from_millis(10_000);
/// 舊版 `INTERACTION_CALLBACK_CHANNEL_MESSAGE`。
const INTERACTION_CALLBACK_CHANNEL_MESSAGE: u8 = 4;
/// 舊版 `INTERACTION_CALLBACK_DEFERRED_CHANNEL_MESSAGE`。
const INTERACTION_CALLBACK_DEFERRED_CHANNEL_MESSAGE: u8 = 5;
/// 舊版 `content.slice(0, 2000)` 的上限。
const MAX_CONTENT_LENGTH: usize = 2_000;

/// 對應舊版 `DiscordApplicationCommandOption`。
///
/// `type` 是 Rust 保留字，命名為 `kind`；值照舊版只有 3（string）與 4（integer）。
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct DiscordApplicationCommandOption {
    /// 舊版 `type: 3 | 4`。
    #[serde(rename = "type")]
    pub(crate) kind: u8,
    pub(crate) name: String,
    pub(crate) description: String,
    /// 舊版 `required?: boolean`；`None` 時序列化省略屬性（照 `JSON.stringify`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) required: Option<bool>,
}

/// 對應舊版 `DiscordApplicationCommand`。
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct DiscordApplicationCommand {
    pub(crate) name: String,
    pub(crate) description: String,
    /// 舊版 `options?: readonly DiscordApplicationCommandOption[]`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) options: Option<Vec<DiscordApplicationCommandOption>>,
}

impl DiscordApplicationCommand {
    /// 無參數指令的便捷建構子（舊版取得 commands 定義時的物件字面）。
    pub(crate) fn plain(name: &str, description: &str) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            options: None,
        }
    }
}

/// 對應舊版 `DiscordGatewayBot`／`DiscordGatewayResponse`：
/// `GET /gateway/bot` 用到的欄位（`url` 在舊版可能是 undefined，照舊以
/// [`Option`] 呈現；本移植的 gateway 連線 URL 走 twilight 預設值，不使用它）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiscordGatewayResponse {
    pub(crate) url: Option<String>,
}

/// 對應舊版 `DiscordApiError`（`name` 恆為 `DiscordApiError`）。
///
/// `method` 照舊版是第一個參數（HTTP 方法字串）的公開欄位。
#[derive(Clone, Debug)]
pub(crate) struct DiscordApiError {
    pub(crate) method: &'static str,
    kind: DiscordApiErrorKind,
}

impl DiscordApiError {
    pub(crate) fn new(method: &'static str, kind: DiscordApiErrorKind) -> Self {
        Self { method, kind }
    }

    /// 對應舊版 `Error.name`。
    pub(crate) fn name(&self) -> &'static str {
        "DiscordApiError"
    }

    /// 對應舊版 `Error.message`（kind 的訊息映射照舊版字串）。
    pub(crate) fn message(&self) -> String {
        match self.kind {
            DiscordApiErrorKind::Timeout => {
                // 舊版逾時重新拋出 `AbortSignal.timeout` 的 TimeoutError；
                // 本移植共用同一枚舉，訊息以同語意輸出。
                "The operation was aborted due to timeout".to_string()
            }
            DiscordApiErrorKind::Network => "Discord network request failed".to_string(),
            DiscordApiErrorKind::InvalidJson => "Discord returned invalid JSON".to_string(),
            DiscordApiErrorKind::Rejected => "Discord API rejected request".to_string(),
            DiscordApiErrorKind::UnexpectedStatus(status) => {
                format!("Discord API returned HTTP {status}")
            }
        }
    }
}

impl core::fmt::Display for DiscordApiError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // 舊版 `String(error)` 輸出 `${name}: ${message}`。
        write!(f, "{}: {}", self.name(), self.message())
    }
}

impl std::error::Error for DiscordApiError {}

/// [`DiscordApiError`] 的訊息分類（見模組文件）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DiscordApiErrorKind {
    Timeout,
    Network,
    InvalidJson,
    Rejected,
    UnexpectedStatus(u16),
}

/// 對應舊版 `DiscordApi` 介面（S4.3 的 bot 與測試以 trait object 注入）。
#[async_trait]
pub(crate) trait DiscordApi: Send + Sync {
    /// 舊版 `getGatewayBot(signal)`；`timeout` 對應呼叫端注入的 abort 訊號
    /// （bot 端為 10 秒）。
    async fn get_gateway_bot(
        &self,
        timeout: Duration,
    ) -> Result<DiscordGatewayResponse, DiscordApiError>;

    /// 舊版 `registerCommands(applicationId, guildId, commands)`；
    /// `guildId` 為空字串時走全域 commands 路徑。
    async fn register_commands(
        &self,
        application_id: &str,
        guild_id: &str,
        commands: &[DiscordApplicationCommand],
    ) -> Result<(), DiscordApiError>;

    /// 舊版 `sendMessage(channelId, content)`。
    async fn send_message(&self, channel_id: &str, content: &str) -> Result<(), DiscordApiError>;

    /// 舊版 `sendInteractionResponse(interactionId, interactionToken, content)`。
    async fn send_interaction_response(
        &self,
        interaction_id: &str,
        interaction_token: &str,
        content: &str,
    ) -> Result<(), DiscordApiError>;

    /// 舊版 `deferInteractionResponse(interactionId, interactionToken)`。
    async fn defer_interaction_response(
        &self,
        interaction_id: &str,
        interaction_token: &str,
    ) -> Result<(), DiscordApiError>;

    /// 舊版 `editInteractionResponse(applicationId, interactionToken, content)`。
    async fn edit_interaction_response(
        &self,
        application_id: &str,
        interaction_token: &str,
        content: &str,
    ) -> Result<(), DiscordApiError>;

    /// 舊版 `sendInteractionPhoto(applicationId, interactionToken, image,
    /// filename, content)`。
    async fn send_interaction_photo(
        &self,
        application_id: &str,
        interaction_token: &str,
        image: &[u8],
        filename: &str,
        content: &str,
    ) -> Result<(), DiscordApiError>;
}

/// 對應舊版 `DiscordApiClientOptions`；`base_url` 為測試接縫（見模組文件）。
pub(crate) struct DiscordApiClientOptions {
    pub(crate) bot_token: String,
    /// 舊版無此參數（舊測試注入 fetch）；本移植以本機 ephemeral server 取代，
    /// 測試用 `Some(base)` 覆寫，正式環境用 `None`（[`DISCORD_API_BASE`]）。
    pub(crate) base_url: Option<String>,
}

/// 對應舊版 `DiscordApiClient`。
pub(crate) struct DiscordApiClient {
    bot_token: String,
    base_url: String,
    http: reqwest::Client,
}

impl DiscordApiClient {
    /// 對應舊版建構子；reqwest client 建立失敗時回傳錯誤（舊版 `globalThis.fetch`
    /// 必在，無對應失敗路徑；呼叫端組裝時視為啟動失敗，對應舊版 fetch 缺失的
    /// `Error('WebSocket is not available…')` 級別）。
    pub(crate) fn new(options: DiscordApiClientOptions) -> Result<Self, reqwest::Error> {
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
                .unwrap_or_else(|| DISCORD_API_BASE.to_string()),
            http: client,
        })
    }

    /// 舊版 `sendMessage`／`sendInteractionResponse` 等的 `content.slice(0, 2000)`。
    ///
    /// JS 的 slice 以 UTF-16 code units 計，Rust 無此字串模型，以 Unicode 純量
    /// （char）計數近似；差異只在「第 2000 個單位恰為 astral 字元的代理對中點」
    /// 這種邊界輸入，實際通知文字不會發生。
    fn truncate_content(content: &str) -> String {
        content.chars().take(MAX_CONTENT_LENGTH).collect()
    }

    /// 對應舊版 `call<T>`（JSON body；`body` 為 [`None`] 時不帶 body 與
    /// `content-type`）。`timeout` 對應舊版 fetch 的 abort 訊號。
    async fn call(
        &self,
        method: reqwest::Method,
        method_name: &'static str,
        route: &str,
        body: Option<String>,
        timeout: Duration,
        authenticated: bool,
    ) -> Result<Option<Value>, DiscordApiError> {
        let mut builder = self
            .http
            .request(method, format!("{}{}", self.base_url, route));
        if authenticated {
            builder = builder.header("authorization", format!("Bot {}", self.bot_token));
        }
        if let Some(body) = body {
            builder = builder
                .header("content-type", "application/json")
                .body(body);
        }
        builder = builder.timeout(timeout);

        // 舊版 catch：傳輸失敗分類——逾時上拋逾時、其他包 DiscordApiError。
        let response = match builder.send().await {
            Ok(response) => response,
            Err(error) => {
                return Err(self.transport_error(method_name, error.is_timeout()));
            }
        };
        parse_discord_response(response, method_name).await
    }

    /// 對應舊版 `callForm<T>`（multipart body）。
    async fn call_form(
        &self,
        method: reqwest::Method,
        method_name: &'static str,
        route: &str,
        form: reqwest::multipart::Form,
        timeout: Duration,
        authenticated: bool,
    ) -> Result<Option<Value>, DiscordApiError> {
        let mut builder = self
            .http
            .request(method, format!("{}{}", self.base_url, route));
        if authenticated {
            builder = builder.header("authorization", format!("Bot {}", self.bot_token));
        }
        builder = builder.multipart(form).timeout(timeout);

        let response = match builder.send().await {
            Ok(response) => response,
            Err(error) => {
                return Err(self.transport_error(method_name, error.is_timeout()));
            }
        };
        parse_discord_response(response, method_name).await
    }

    /// 舊版 catch 的錯誤分類：逾時對應舊版重新拋出的 TimeoutError（本移植以
    /// [`DiscordApiErrorKind::Timeout`] 同枚舉呈現），其餘分類為
    /// `Discord network request failed`。
    fn transport_error(&self, method: &'static str, timeout: bool) -> DiscordApiError {
        let kind = if timeout {
            DiscordApiErrorKind::Timeout
        } else {
            DiscordApiErrorKind::Network
        };
        DiscordApiError::new(method, kind)
    }
}

#[async_trait]
impl DiscordApi for DiscordApiClient {
    async fn get_gateway_bot(
        &self,
        timeout: Duration,
    ) -> Result<DiscordGatewayResponse, DiscordApiError> {
        let payload = self
            .call(
                reqwest::Method::GET,
                "GET",
                "/gateway/bot",
                None,
                timeout,
                true,
            )
            .await?;
        // 舊版 `payload as DiscordGatewayResponse` 不驗 schema（url 可能 undefined）。
        Ok(DiscordGatewayResponse {
            url: payload.and_then(|payload| {
                payload
                    .get("url")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }),
        })
    }

    async fn register_commands(
        &self,
        application_id: &str,
        guild_id: &str,
        commands: &[DiscordApplicationCommand],
    ) -> Result<(), DiscordApiError> {
        let route = if guild_id.is_empty() {
            format!("/applications/{application_id}/commands")
        } else {
            format!("/applications/{application_id}/guilds/{guild_id}/commands")
        };
        // 舊版以 JSON.stringify(commands) 序列化；欄位順序與略過未定義屬性由
        // serde derive 加上 preserve_order 呈現。序列化失敗僅在防衛性分支
        // （內容皆為字串／數字／布林，理論上不可達；分類照傳輸層錯誤）。
        let body = Some(
            serde_json::to_string(commands)
                .map_err(|_| DiscordApiError::new("PUT", DiscordApiErrorKind::Network))?,
        );
        self.call(
            reqwest::Method::PUT,
            "PUT",
            &route,
            body,
            SEND_TIMEOUT_MS,
            true,
        )
        .await?;
        Ok(())
    }

    async fn send_message(&self, channel_id: &str, content: &str) -> Result<(), DiscordApiError> {
        let body =
            Some(serde_json::json!({ "content": Self::truncate_content(content) }).to_string());
        self.call(
            reqwest::Method::POST,
            "POST",
            &format!("/channels/{channel_id}/messages"),
            body,
            SEND_TIMEOUT_MS,
            true,
        )
        .await?;
        Ok(())
    }

    async fn send_interaction_response(
        &self,
        interaction_id: &str,
        interaction_token: &str,
        content: &str,
    ) -> Result<(), DiscordApiError> {
        let body = Some(
            serde_json::json!({
                "type": INTERACTION_CALLBACK_CHANNEL_MESSAGE,
                "data": { "content": Self::truncate_content(content) },
            })
            .to_string(),
        );
        self.call(
            reqwest::Method::POST,
            "POST",
            &format!("/interactions/{interaction_id}/{interaction_token}/callback"),
            body,
            SEND_TIMEOUT_MS,
            true,
        )
        .await?;
        Ok(())
    }

    async fn defer_interaction_response(
        &self,
        interaction_id: &str,
        interaction_token: &str,
    ) -> Result<(), DiscordApiError> {
        let body = Some(
            serde_json::json!({ "type": INTERACTION_CALLBACK_DEFERRED_CHANNEL_MESSAGE })
                .to_string(),
        );
        self.call(
            reqwest::Method::POST,
            "POST",
            &format!("/interactions/{interaction_id}/{interaction_token}/callback"),
            body,
            SEND_TIMEOUT_MS,
            true,
        )
        .await?;
        Ok(())
    }

    async fn edit_interaction_response(
        &self,
        application_id: &str,
        interaction_token: &str,
        content: &str,
    ) -> Result<(), DiscordApiError> {
        let body =
            Some(serde_json::json!({ "content": Self::truncate_content(content) }).to_string());
        // 舊版 authenticated = false（webhook token 本身即授權）。
        self.call(
            reqwest::Method::PATCH,
            "PATCH",
            &format!("/webhooks/{application_id}/{interaction_token}/messages/@original"),
            body,
            SEND_TIMEOUT_MS,
            false,
        )
        .await?;
        Ok(())
    }

    async fn send_interaction_photo(
        &self,
        application_id: &str,
        interaction_token: &str,
        image: &[u8],
        filename: &str,
        content: &str,
    ) -> Result<(), DiscordApiError> {
        // 舊版 payload_json：JSON.stringify({ content, attachments: [{ id: 0, filename }] })；鍵序照物件建立順序。
        let payload_json = serde_json::json!({
            "content": Self::truncate_content(content),
            "attachments": [{ "id": 0, "filename": filename }],
        })
        .to_string();
        let form = reqwest::multipart::Form::new()
            .text("payload_json", payload_json)
            // 舊版 Blob 的 type: 'image/png' 與檔名轉為 part headers。
            .part(
                "files[0]",
                reqwest::multipart::Part::bytes(image.to_vec())
                    .file_name(filename.to_string())
                    .mime_str("image/png")
                    .map_err(|_| {
                        // 舊版無對應失敗路徑（FormData 字面帶固定 type）；reqwest
                        // 只在非法 header 值時失敗，視為傳輸層錯誤。
                        DiscordApiError::new("POST", DiscordApiErrorKind::Network)
                    })?,
            );
        // 舊版 authenticated = false（webhook token 本身即授權）。
        self.call_form(
            reqwest::Method::POST,
            "POST",
            &format!("/webhooks/{application_id}/{interaction_token}"),
            form,
            SEND_TIMEOUT_MS,
            false,
        )
        .await?;
        Ok(())
    }
}

/// `redirect: 'error'` 的錯誤值（reqwest 使用；twitch transport 的同一模式）。
#[derive(Debug)]
struct RedirectRejected;

impl core::fmt::Display for RedirectRejected {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("redirect is not allowed")
    }
}

impl std::error::Error for RedirectRejected {}

/// 對應舊版 `parseDiscordResponse`：狀態 204 回 None；JSON 解析失敗時成功
/// 狀態回 None、其他丟 invalid JSON；非成功狀態依 payload 是否
/// 帶 `message` 欄位分類。
async fn parse_discord_response(
    response: reqwest::Response,
    method: &'static str,
) -> Result<Option<Value>, DiscordApiError> {
    let status = response.status();
    if status == reqwest::StatusCode::NO_CONTENT {
        return Ok(None);
    }

    // 舊版 `payload = await response.json()`：讀取與解析合在一個 catch（連線層
    // 失敗與 SyntaxError 同樣走 ok 判斷）。
    let body_result = response.text().await;
    let payload = match read_and_parse_json(body_result) {
        Err(_) => {
            // 舊版 catch：狀態成功 → 回 undefined；否則丟 invalid JSON。
            if status.is_success() {
                return Ok(None);
            }
            return Err(DiscordApiError::new(
                method,
                DiscordApiErrorKind::InvalidJson,
            ));
        }
        Ok(payload) => payload,
    };

    if status.is_success() {
        return Ok(Some(payload));
    }
    if is_discord_error_payload(&payload) {
        Err(DiscordApiError::new(method, DiscordApiErrorKind::Rejected))
    } else {
        Err(DiscordApiError::new(
            method,
            DiscordApiErrorKind::UnexpectedStatus(status.as_u16()),
        ))
    }
}

/// 合體的 body 讀取與 JSON 解析（舊版 `await response.json()` 的單一 catch）；
/// 失敗的類別不外流（照舊版以 ok/status 判斷）。
fn read_and_parse_json(body_result: Result<String, reqwest::Error>) -> Result<Value, ()> {
    let body = body_result.map_err(|_| ())?;
    serde_json::from_str::<Value>(&body).map_err(|_| ())
}

/// 對應舊版 `isDiscordErrorPayload`：物件且帶 `message` 欄位（值可以是任意
/// 型別）。
fn is_discord_error_payload(payload: &Value) -> bool {
    payload.is_object() && payload.get("message").is_some()
}

#[cfg(test)]
mod tests;
