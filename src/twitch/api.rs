//! Twitch Helix API client：`twitch/TwitchApiClient.ts` 逐函式移植（§4.2，S3.1）。
//!
//! - 端點、query 參數、回應欄位、錯誤訊息、重試與逾時值全部照舊版常數：
//!   30 秒 request timeout（[`DEFAULT_REQUEST_TIMEOUT_MS`]）、response body 上限
//!   1 MB（[`MAX_RESPONSE_BODY_BYTES`]）、rate limit fallback 上限 5 分鐘
//!   （[`MAX_RATE_LIMIT_FALLBACK_MS`]）、token 每小時驗證
//!   （[`DEFAULT_TOKEN_VALIDATION_INTERVAL_MS`]）、到期前 24 小時換發
//!   （[`DEFAULT_TOKEN_REFRESH_THRESHOLD_SECONDS`]）、每次最多 100 個
//!   `user_login`（[`TWITCH_HELIX_MAX_USER_LOGINS`]）。
//! - HTTP transport 以 [`TwitchHttpTransport`] 注入（對應舊版 options 的
//!   `fetch`），預設實作 [`ReqwestTransport`]。選 trait 注入而非本機 stub
//!   server 的理由：舊測試直接 mock `fetch`——要逐欄位斷言 URL／headers／
//!   body、按呼叫次序回不同回應、注入網路層錯誤（`AbortError` 與 network 的
//!   區分）與「永不完成」的請求（逾時），trait 注入能逐一重現；本機
//!   stub server（`src/tests/browser_diagnostics.rs` 的模式）做不到上述區分。
//! - 預設 [`ReqwestTransport`] 照舊版 Node fetch（undici）的行為：只用
//!   HTTP/1.1（`http1_only()`）、不讀 `http_proxy` 等系統 proxy 環境變數
//!   （`no_proxy()`）、任何 redirect 都讓請求失敗（`redirect: 'error'`）、
//!   不另設 user agent。
//! - 逾時對應舊版單一 `AbortController`／`AbortSignal.timeout`：涵蓋取得
//!   回應 headers 與讀完 response body。Rust 版在 client 端以
//!   `tokio::time::timeout` 包住整段（transport 只負責收發）；逾時測試用
//!   `start_paused` 的虛擬時間，不等真實時間。舊版 401 重試分支會先
//!   `finish()` 清掉計時再換發 token——本移植的逾時範圍在回傳
//!   [`StreamsAttempt::NeedsTokenRefresh`] 時同步結束，換發不在逾時範圍內。
//! - token 維護的單一飛行（single-flight）照舊版 `tokenMaintenanceFlight`：
//!   第二個呼叫端等同一個 flight 的結果（含失敗），不重複換發。flight 由第一個
//!   呼叫端 inline 執行（舊版 Promise 無此限制；若該任務在維護途中被取消，
//!   watch 會因 sender 消失而結束、joiner 沿用現有 token，屬 Rust 版的取捨）。
//! - 舊版 `Date.parse` 的寬鬆格式只對應到 RFC 3339（Helix `started_at` 的
//!   合約格式）；`Date.parse` 額外接受的格式（如 `June 14, 2026`）不支援。
//! - body 大小上限照舊版逐 chunk 檢查（串流讀取，超限即中止），Content-Length
//!   的預檢查也照舊版（只接受純數字值）。
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;
use time::UtcOffset;
use time::format_description::StaticFormatDescription;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use url::Url;

use super::errors::{
    TwitchApiAuthError, TwitchApiClientInitError, TwitchApiError, TwitchApiRateLimitError,
    TwitchApiTemporaryError, TwitchApiTemporaryErrorReason,
};
use super::live_status::{ChannelLiveStatus, LiveStatusProvider};
use crate::logging::logger::{JsonLineLogger, LogEvents};
use crate::logging::redact::SanitizedValue;

/// 對應舊版 `TWITCH_HELIX_BASE_URL`。
pub(crate) const TWITCH_HELIX_BASE_URL: &str = "https://api.twitch.tv/helix/";
/// 對應舊版 `TWITCH_HELIX_MAX_USER_LOGINS`：單次請求最多幾個 `user_login`。
pub(crate) const TWITCH_HELIX_MAX_USER_LOGINS: usize = 100;
/// 對應舊版 `TWITCH_OAUTH_BASE_URL`。
pub(crate) const TWITCH_OAUTH_BASE_URL: &str = "https://id.twitch.tv/oauth2/";

/// 舊版 `DEFAULT_REQUEST_TIMEOUT_MS`。
const DEFAULT_REQUEST_TIMEOUT_MS: f64 = 30_000.0;
/// 舊版 `DEFAULT_TOKEN_VALIDATION_INTERVAL_MS`（60 × 60 × 1_000）。
const DEFAULT_TOKEN_VALIDATION_INTERVAL_MS: f64 = 60.0 * 60.0 * 1_000.0;
/// 舊版 `DEFAULT_TOKEN_REFRESH_THRESHOLD_SECONDS`（24 × 60 × 60）。
const DEFAULT_TOKEN_REFRESH_THRESHOLD_SECONDS: f64 = 24.0 * 60.0 * 60.0;
/// 舊版 `MAX_RATE_LIMIT_FALLBACK_MS`（5 分鐘）。
const MAX_RATE_LIMIT_FALLBACK_MS: u64 = 5 * 60 * 1_000;
/// 舊版 `MAX_RESPONSE_BODY_BYTES`。
const MAX_RESPONSE_BODY_BYTES: usize = 1_000_000;
/// 舊版 `MAX_TIMER_DELAY_MS`（JS `setTimeout` 的上限 2^31 − 1，照舊版檢查保留）。
const MAX_TIMER_DELAY_MS: f64 = 2_147_483_647.0;

/// 舊版 `toISOString()` 的格式：UTC 的 `YYYY-MM-DDTHH:MM:SS.sssZ`
/// （`[subsecond digits:3]` 輸出固定 3 位毫秒，與 `Date.toISOString()` 相同；
/// 格式與 `logging` 模組同一組，獨立定義以維持模組邊界）。
const ISO_UTC_FORMAT: StaticFormatDescription =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// 舊版 `Number.isSafeInteger` 的上限（2^53 − 1）。
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// 對應舊版 options 的 `now: () => Date`：回傳記錄當下的時間（預設 UTC 現在）。
#[allow(clippy::type_complexity)]
pub(crate) type TwitchApiClock = Box<dyn Fn() -> OffsetDateTime + Send + Sync>;

/// 對應舊版 `TwitchApiClientOptions`；transport、時鐘等以 `Option` 注入
/// （`None` 用預設，對應舊版的省略參數）。
pub(crate) struct TwitchApiClientOptions {
    /// 舊版 `clientId`（必填）。
    pub(crate) client_id: String,
    /// 舊版 `accessToken`（可省略；省略或空字串時需有 `clientSecret`）。
    pub(crate) access_token: Option<String>,
    /// 舊版 `clientSecret`（可省略）。
    pub(crate) client_secret: Option<String>,
    /// 舊版 `checkIntervalSeconds`（必填；rate limit fallback 的計算基準）。
    pub(crate) check_interval_seconds: f64,
    /// 舊版 `logger`（必填）。
    pub(crate) logger: Arc<dyn TwitchApiLogger>,
    /// 舊版 `fetch`；`None` 用預設的 [`ReqwestTransport`]。
    pub(crate) transport: Option<Arc<dyn TwitchHttpTransport>>,
    /// 舊版 `baseUrl`（Helix 端點基底）。
    pub(crate) base_url: Option<String>,
    /// 舊版 `oauthBaseUrl`（validate／token 端點基底）。
    pub(crate) oauth_base_url: Option<String>,
    /// 舊版 `requestTimeoutMs`。
    pub(crate) request_timeout_ms: Option<f64>,
    /// 舊版 `tokenValidationIntervalMs`。
    pub(crate) token_validation_interval_ms: Option<f64>,
    /// 舊版 `tokenRefreshThresholdSeconds`。
    pub(crate) token_refresh_threshold_seconds: Option<f64>,
    /// 舊版 `now`；`None` 用系統時鐘。
    pub(crate) now: Option<TwitchApiClock>,
}

/// 對應舊版 `HelixStream`：`streams` 端點單筆的必要欄位。
struct HelixStream {
    id: String,
    user_login: String,
    title: String,
    started_at: String,
    viewer_count: u64,
}

/// 舊版 `validateToken` 的回傳：`{ valid: true, expiresIn }` 或 `{ valid: false }`。
enum TokenValidation {
    Valid { expires_in: f64 },
    Invalid,
}

/// `requestStreamsAttempt` 的結果：串流清單，或 401 時需要換發 token 後重試
/// （對應舊版 `requestStreamsAttempt` 內的遞迴重試； Rust 版以 loop 呈現）。
enum StreamsAttempt {
    Streams(Vec<HelixStream>),
    NeedsTokenRefresh,
}

/// 舊版 `tokenMaintenanceFlight` 的完成信號：完成時以
/// [`watch::Sender::send_replace`] 送出結果（含失敗，對應 joiner 等同一個
/// Promise）；sender 消失（runner 被取消）時 joiner 取 `None` 沿用現有 token。
type TokenMaintenanceFlightSender =
    Arc<tokio::sync::watch::Sender<Option<Result<(), TwitchApiError>>>>;

/// 對應舊版 `Logger` 介面（client 用到的 debug／info／warn／error；欄位值為
/// 消毒前的 [`SanitizedValue`]，消毒在 Logger 本體進行）。舊版 `flush` 由
/// Logger 本體提供，client 不呼叫，trait 不含。
pub(crate) trait TwitchApiLogger: Send + Sync {
    /// 對應 `debug(event, fields?)`。
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>);

    /// 對應 `info(event, fields?)`。
    fn info(&self, event: &str, fields: Option<&SanitizedValue>);

    /// 對應 `warn(event, fields?)`。
    fn warn(&self, event: &str, fields: Option<&SanitizedValue>);

    /// 對應 `error(event, fields?)`。
    fn error(&self, event: &str, fields: Option<&SanitizedValue>);
}

/// 對應舊版 `JsonLineLogger` 滿足 `Logger` 介面的轉接（S3.6 接線用）。
impl TwitchApiLogger for JsonLineLogger {
    fn debug(&self, event: &str, fields: Option<&SanitizedValue>) {
        // inherent 的 debug 同簽名；以完整路徑呼叫避免誤呼本 impl 造成遞迴。
        JsonLineLogger::debug(self, event, fields);
    }

    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        JsonLineLogger::info(self, event, fields);
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        JsonLineLogger::warn(self, event, fields);
    }

    fn error(&self, event: &str, fields: Option<&SanitizedValue>) {
        JsonLineLogger::error(self, event, fields);
    }
}

/// transport 失敗的分類（對應舊版 fetch rejection 的可辨識形態）。
///
/// 舊版 client 只區分「逾時觸發的中止」（`timedOut` 旗標）、其他 `AbortError`
/// 與其餘網路層失敗；[`ReqwestTransport`] 不會產生 [`TwitchTransportError::Aborted`]
/// （舊版正式碼也沒有外部中止來源），供注入的測試 transport 模擬舊測試的
/// `AbortError`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TwitchTransportError {
    /// 舊版逾時（`AbortController`／`AbortSignal.timeout` 觸發）。
    Timeout,
    /// 舊版 `AbortError`（非逾時的中止）。
    Aborted,
    /// 其他網路層失敗（含 `redirect: 'error'` 的拒絕）。
    Network,
}

/// 對應舊版 `fetch(url, init)` 的 init：URL、方法、headers 與
/// form-urlencoded body（舊版 `URLSearchParams`）。
pub(crate) struct TwitchHttpRequest {
    /// 完整 URL 字串。
    pub(crate) url: String,
    /// HTTP 方法（`'GET'`／`'POST'`）。
    pub(crate) method: &'static str,
    /// headers（插入順序照舊版物件建立的順序）。
    pub(crate) headers: Vec<(String, String)>,
    /// form-urlencoded 的 body；GET 為 [`None`]。
    pub(crate) body: Option<String>,
}

/// 對應舊版 `Headers`：大小寫不敏感的 `get`（舊版查詢
/// `'Ratelimit-Reset'`／`'Content-Length'`）。
pub(crate) struct TwitchHttpHeaders(Vec<(String, String)>);

impl TwitchHttpHeaders {
    /// 以（name, value）配對建立。
    pub(crate) fn new(headers: Vec<(String, String)>) -> Self {
        Self(headers)
    }

    /// 對應 `Headers.get(name)`：大小寫不敏感。
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// 對應舊版 `Response.body` 的讀取介面（`ReadableStreamDefaultReader.read()`）。
#[async_trait]
pub(crate) trait TwitchHttpBody: Send {
    /// 回傳下一個 chunk；[`None`] 對應 `done`。
    async fn next_chunk(&mut self) -> Option<Result<Vec<u8>, TwitchTransportError>>;
}

/// 對應舊版 `Response`：headers 已收齊、body 以串流讀取（舊版讀 body 時
/// abort signal 仍在，本移植的逾時由 client 端涵蓋整段）。
pub(crate) struct TwitchHttpResponse {
    /// HTTP 狀態碼。
    pub(crate) status: u16,
    /// 回應 headers。
    pub(crate) headers: TwitchHttpHeaders,
    /// 回應 body（串流）。
    pub(crate) body: Box<dyn TwitchHttpBody>,
}

/// 對應舊版 options 的 `fetch: typeof fetch`：送出請求並取得回應
/// （含 transport 層失敗的分類）。
#[async_trait]
pub(crate) trait TwitchHttpTransport: Send + Sync {
    /// 對應 `fetch(url, init)`。
    async fn fetch(
        &self,
        request: TwitchHttpRequest,
    ) -> Result<TwitchHttpResponse, TwitchTransportError>;
}

/// `redirect: 'error'` 的錯誤值（transport 失敗只分類，訊息不外流）。
#[derive(Debug)]
struct RedirectRejected;

impl core::fmt::Display for RedirectRejected {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("redirect is not allowed")
    }
}

impl std::error::Error for RedirectRejected {}

/// 預設 transport（舊版 `globalThis.fetch`）：以 reqwest 照舊版 Node fetch 行為設定。
pub(crate) struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// 建立預設 transport；reqwest client 無法建立時回傳錯誤（舊版沒有對應的
    /// 失敗模式，呼叫端以 [`TwitchApiClientInitError::TypeError`] 對應）。
    pub(crate) fn new() -> Result<Self, reqwest::Error> {
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
        Ok(Self { client })
    }
}

/// 對應舊版 reqwest 錯誤的處理：逾時與其餘（含 redirect 拒絕）都分類為
/// transport 失敗；`Aborted` 不會由本 transport 產生。
fn map_reqwest_error(error: reqwest::Error) -> TwitchTransportError {
    if error.is_timeout() {
        TwitchTransportError::Timeout
    } else {
        TwitchTransportError::Network
    }
}

#[async_trait]
impl TwitchHttpTransport for ReqwestTransport {
    async fn fetch(
        &self,
        request: TwitchHttpRequest,
    ) -> Result<TwitchHttpResponse, TwitchTransportError> {
        let mut builder = match request.method {
            "GET" => self.client.get(&request.url),
            "POST" => self.client.post(&request.url),
            // client 只會送 GET／POST；防衛性分支照其他網路層失敗處理。
            _ => return Err(TwitchTransportError::Network),
        };
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = request.body {
            builder = builder.body(body);
        }
        let response = builder.send().await.map_err(map_reqwest_error)?;

        let status = response.status().as_u16();
        let headers = TwitchHttpHeaders::new(
            response
                .headers()
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_string(),
                        String::from_utf8_lossy(value.as_bytes()).into_owned(),
                    )
                })
                .collect(),
        );
        Ok(TwitchHttpResponse {
            status,
            headers,
            body: Box::new(ReqwestBody { response }),
        })
    }
}

/// [`ReqwestTransport`] 的 body：以 reqwest 的串流逐 chunk 提供。
struct ReqwestBody {
    response: reqwest::Response,
}

#[async_trait]
impl TwitchHttpBody for ReqwestBody {
    async fn next_chunk(&mut self) -> Option<Result<Vec<u8>, TwitchTransportError>> {
        match self.response.chunk().await {
            Ok(Some(bytes)) => Some(Ok(bytes.to_vec())),
            Ok(None) => None,
            Err(error) => Some(Err(map_reqwest_error(error))),
        }
    }
}

/// 對應舊版 `TwitchApiClient`。
pub(crate) struct TwitchApiClient {
    /// 舊版 `options.clientId`。
    client_id: String,
    /// 舊版 `options.clientSecret`。
    client_secret: Option<String>,
    /// 舊版 `options.checkIntervalSeconds`。
    check_interval_seconds: f64,
    /// 舊版 `options.logger`。
    logger: Arc<dyn TwitchApiLogger>,
    /// 舊版 `endpoint`：`new URL('streams', baseUrl)`。
    endpoint: Url,
    /// 舊版 `validateEndpoint`：`new URL('validate', oauthBaseUrl)`。
    validate_endpoint: Url,
    /// 舊版 `tokenEndpoint`：`new URL('token', oauthBaseUrl)`。
    token_endpoint: Url,
    /// 舊版 `fetchFunction`。
    transport: Arc<dyn TwitchHttpTransport>,
    /// 舊版 `requestTimeoutMs`。
    request_timeout_ms: f64,
    /// 舊版 `tokenValidationIntervalMs`。
    token_validation_interval_ms: f64,
    /// 舊版 `tokenRefreshThresholdSeconds`。
    token_refresh_threshold_seconds: f64,
    /// 舊版 `now`。
    now: TwitchApiClock,
    /// 舊版 `accessToken` 與 `lastValidatedAt`（Rust 版放共享的 token 狀態）。
    token_state: Mutex<TokenState>,
    /// 舊版 `tokenMaintenanceFlight` 的單一飛行槽位。
    maintenance_flight: Mutex<Option<TokenMaintenanceFlightSender>>,
}

/// 舊版 `accessToken` 與 `lastValidatedAt` 的共享狀態。
struct TokenState {
    access_token: String,
    /// epoch 毫秒（舊版 `Date.getTime()`）。
    last_validated_at: Option<i64>,
}

impl TwitchApiClient {
    /// 對應舊版建構子；選項驗證照舊版順序，失敗回傳
    /// [`TwitchApiClientInitError`]（對應拋出的 `TypeError`／`RangeError`）。
    pub(crate) fn new(options: TwitchApiClientOptions) -> Result<Self, TwitchApiClientInitError> {
        require_non_empty(&options.client_id, "clientId")?;
        if let Some(secret) = options.client_secret.as_deref() {
            // 舊版：clientSecret 非 undefined 且非 '' 時也要非空白。
            if !secret.is_empty() {
                require_non_empty(secret, "clientSecret")?;
            }
        }
        let access_token = options.access_token.clone().unwrap_or_default();
        let client_secret_present = options
            .client_secret
            .as_deref()
            .is_some_and(|secret| !secret.is_empty());
        if access_token.is_empty() && !client_secret_present {
            // 舊版：`accessToken or clientSecret is required`。
            return Err(TwitchApiClientInitError::TypeError(
                "accessToken or clientSecret is required".to_string(),
            ));
        }
        require_positive_finite(options.check_interval_seconds, "checkIntervalSeconds")?;

        let request_timeout_ms = options
            .request_timeout_ms
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS);
        require_positive_finite(request_timeout_ms, "requestTimeoutMs")?;
        if request_timeout_ms > MAX_TIMER_DELAY_MS {
            return Err(TwitchApiClientInitError::RangeError(format!(
                "requestTimeoutMs must be at most {}",
                // 顯示成整數（照舊版樣板字串）。
                MAX_TIMER_DELAY_MS as i64
            )));
        }

        let base_url = options.base_url.as_deref().unwrap_or(TWITCH_HELIX_BASE_URL);
        let endpoint = resolve_relative(base_url, "streams")?;
        let oauth_base_url = options
            .oauth_base_url
            .as_deref()
            .unwrap_or(TWITCH_OAUTH_BASE_URL);
        let validate_endpoint = resolve_relative(oauth_base_url, "validate")?;
        let token_endpoint = resolve_relative(oauth_base_url, "token")?;

        let token_validation_interval_ms = options
            .token_validation_interval_ms
            .unwrap_or(DEFAULT_TOKEN_VALIDATION_INTERVAL_MS);
        let token_refresh_threshold_seconds = options
            .token_refresh_threshold_seconds
            .unwrap_or(DEFAULT_TOKEN_REFRESH_THRESHOLD_SECONDS);
        require_positive_finite(token_validation_interval_ms, "tokenValidationIntervalMs")?;
        require_positive_finite(
            token_refresh_threshold_seconds,
            "tokenRefreshThresholdSeconds",
        )?;

        let transport = match options.transport {
            Some(transport) => transport,
            None => Arc::new(ReqwestTransport::new().map_err(|_| {
                // 舊版沒有對應的失敗模式（globalThis.fetch 必在）；
                // reqwest client 無法建立時以同類錯誤回報。
                TwitchApiClientInitError::TypeError(
                    "failed to initialize HTTP transport".to_string(),
                )
            })?),
        };

        Ok(Self {
            client_id: options.client_id,
            client_secret: options.client_secret,
            check_interval_seconds: options.check_interval_seconds,
            logger: options.logger,
            endpoint,
            validate_endpoint,
            token_endpoint,
            transport,
            request_timeout_ms,
            token_validation_interval_ms,
            token_refresh_threshold_seconds,
            now: options
                .now
                .unwrap_or_else(|| Box::new(OffsetDateTime::now_utc)),
            token_state: Mutex::new(TokenState {
                access_token,
                last_validated_at: None,
            }),
            maintenance_flight: Mutex::new(None),
        })
    }

    /// 對應舊版 `getLiveStatuses`：分批查詢（每批 ≤ 100 個 `user_login`）、
    /// 依設定的順序回傳每個頻道的直播狀態。
    pub(crate) async fn get_live_statuses(
        &self,
        channels: &[String],
    ) -> Result<Vec<ChannelLiveStatus>, TwitchApiError> {
        if channels.is_empty() {
            return Ok(Vec::new());
        }

        // 舊版：liveStreams 以正規化後的 login 為鍵（後批覆蓋前批）。
        let mut live_streams: HashMap<String, HelixStream> = HashMap::new();
        for batch in channels.chunks(TWITCH_HELIX_MAX_USER_LOGINS) {
            for stream in self.request_streams(batch).await? {
                live_streams.insert(normalize_channel(&stream.user_login), stream);
            }
        }

        let checked_at = iso_utc_string((self.now)());

        Ok(channels
            .iter()
            .map(|channel| {
                let checked_at = checked_at.clone();
                match live_streams.get(&normalize_channel(channel)) {
                    Some(stream) => ChannelLiveStatus {
                        channel: channel.clone(),
                        is_live: true,
                        stream_id: Some(stream.id.clone()),
                        title: Some(stream.title.clone()),
                        started_at: Some(stream.started_at.clone()),
                        viewer_count: Some(stream.viewer_count),
                        checked_at,
                    },
                    None => ChannelLiveStatus {
                        channel: channel.clone(),
                        is_live: false,
                        stream_id: None,
                        title: None,
                        started_at: None,
                        viewer_count: None,
                        checked_at,
                    },
                }
            })
            .collect())
    }

    /// 對應舊版 `requestStreams`：先做（非強制的）token 維護，再查詢串流。
    async fn request_streams(
        &self,
        channels: &[String],
    ) -> Result<Vec<HelixStream>, TwitchApiError> {
        self.maintain_token(false).await?;
        // 舊版：401 換發 token 後只重試一次。
        let mut allow_auth_retry = true;
        loop {
            match self
                .request_streams_once(channels, allow_auth_retry)
                .await?
            {
                StreamsAttempt::Streams(streams) => return Ok(streams),
                StreamsAttempt::NeedsTokenRefresh => {
                    // 舊版：await this.maintainToken(true) 的失敗會往上拋。
                    self.maintain_token(true).await?;
                    allow_auth_retry = false;
                }
            }
        }
    }

    /// 對應舊版 `requestStreamsAttempt` 的外層（逾時計時從送出請求起算，
    /// 涵蓋 body 讀取）。
    async fn request_streams_once(
        &self,
        channels: &[String],
        allow_auth_retry: bool,
    ) -> Result<StreamsAttempt, TwitchApiError> {
        // 舊版在組 fetch init 時讀取 this.accessToken 當下的值。
        let access_token = self.token_state().access_token.clone();
        let request = TwitchHttpRequest {
            url: self.build_streams_url(channels).to_string(),
            method: "GET",
            headers: vec![
                ("Client-Id".to_string(), self.client_id.clone()),
                (
                    "Authorization".to_string(),
                    format!("Bearer {access_token}"),
                ),
            ],
            body: None,
        };

        match tokio::time::timeout(
            self.timeout_duration(),
            self.execute_streams_request(request, allow_auth_retry),
        )
        .await
        {
            Ok(result) => result,
            // 舊版逾時（fetch 或 body 讀取被 abort）→ temporary 'timeout'。
            Err(_) => Err(self.temporary_error(TwitchApiTemporaryErrorReason::Timeout, None)),
        }
    }

    /// 對應舊版 `requestStreamsAttempt` 的內層：狀態分類、body 讀取與 schema 檢查。
    async fn execute_streams_request(
        &self,
        request: TwitchHttpRequest,
        allow_auth_retry: bool,
    ) -> Result<StreamsAttempt, TwitchApiError> {
        let mut response = self
            .transport
            .fetch(request)
            .await
            .map_err(|error| self.temporary_error(transport_reason(error, false), None))?;
        let status = response.status;

        if status == 401 || status == 403 {
            if status == 401 && allow_auth_retry && self.can_refresh() {
                // 舊版：request.finish()（清逾時計時）→ maintainToken(true) → 重試。
                // 逾時計時隨本函式回傳結束，換發在逾時範圍外。
                return Ok(StreamsAttempt::NeedsTokenRefresh);
            }
            self.logger.error(
                LogEvents::TWITCH_API_AUTH_FAILED,
                Some(&status_code_fields(status)),
            );
            return Err(TwitchApiAuthError::new(status).into());
        }

        if status == 429 {
            let error = self.create_rate_limit_error(&response.headers);
            // 舊版欄位：statusCode、retryAt（ISO）、retryAfterMs。
            let fields = SanitizedValue::Object(vec![
                ("statusCode".to_string(), SanitizedValue::Number(429.0)),
                (
                    "retryAt".to_string(),
                    SanitizedValue::String(iso_utc_string_from_epoch_ms(error.retry_at_ms())),
                ),
                (
                    "retryAfterMs".to_string(),
                    SanitizedValue::Number(error.retry_after_ms() as f64),
                ),
            ]);
            self.logger.warn("twitch_api_rate_limited", Some(&fields));
            return Err(error.into());
        }

        if status >= 500 {
            return Err(self.temporary_error(TwitchApiTemporaryErrorReason::Server, Some(status)));
        }

        if status != 200 {
            return Err(self.temporary_error(TwitchApiTemporaryErrorReason::Http, Some(status)));
        }

        // 舊版 try { readLimitedJson + parseHelixStreams } catch 的範圍：
        // body 讀取、大小上限與 schema 檢查；body 期間的 transport 失敗
        // 照舊版的分類（逾時 → timeout、abort → aborted、其餘 → invalid-response）。
        let payload = match read_limited_json(
            &response.headers,
            response.body.as_mut(),
            MAX_RESPONSE_BODY_BYTES,
        )
        .await
        {
            Ok(payload) => payload,
            Err(BodyReadError::TooLarge | BodyReadError::Parse) => {
                return Err(self.temporary_error(
                    TwitchApiTemporaryErrorReason::InvalidResponse,
                    Some(status),
                ));
            }
            Err(BodyReadError::Transport(error)) => {
                return Err(self.temporary_error(transport_reason(error, true), Some(status)));
            }
        };

        let streams = match parse_helix_streams(&payload) {
            Ok(streams) => streams,
            Err(_) => {
                return Err(self.temporary_error(
                    TwitchApiTemporaryErrorReason::InvalidResponse,
                    Some(status),
                ));
            }
        };
        Ok(StreamsAttempt::Streams(streams))
    }

    /// 對應舊版 URL 組裝：endpoint 複製後以與 `URLSearchParams` 相同的
    /// form-urlencoded 序列化附加 `user_login`（query 以外皆不動）。
    fn build_streams_url(&self, channels: &[String]) -> Url {
        let mut url = self.endpoint.clone();
        {
            let mut pairs = url.query_pairs_mut();
            for channel in channels {
                pairs.append_pair("user_login", channel);
            }
        }
        url
    }

    /// 舊版 `canRefresh`：有 clientSecret 就能換發。
    fn can_refresh(&self) -> bool {
        self.client_secret
            .as_deref()
            .is_some_and(|secret| !secret.is_empty())
    }

    /// 舊版 `maintainToken`：單一飛行；已有 flight 時等它並取同一份結果
    /// （含失敗），不重複執行。
    async fn maintain_token(&self, force_refresh: bool) -> Result<(), TwitchApiError> {
        if !self.can_refresh() {
            return Ok(());
        }

        // 舊版：回傳同一個 flight Promise（結果含失敗）。guard 只在同步區塊內使用，
        // 不跨 await（Send 限制）。
        let joined = {
            let slot = self.maintenance_flight();
            slot.as_ref().map(|sender| sender.subscribe())
        };
        if let Some(mut receiver) = joined {
            if receiver.borrow_and_update().is_none() {
                // sender 消失（runner 被取消）時 changed() 立即回錯，取 None 沿用現狀。
                let _ = receiver.changed().await;
            }
            return receiver.borrow_and_update().clone().unwrap_or(Ok(()));
        }

        // 建立新的 flight 槽位（上個區塊到這裡沒有 await，兩次檢查之間無競爭）。
        let sender: TokenMaintenanceFlightSender = {
            let mut slot = self.maintenance_flight();
            let sender = Arc::new(tokio::sync::watch::channel(None).0);
            *slot = Some(Arc::clone(&sender));
            sender
        };

        let result = self.run_token_maintenance(force_refresh).await;
        // 舊版 flight.then 的清理：先把結果送給 joiners，再移除槽位
        // （僅當槽位還是本 flight）。
        sender.send_replace(Some(result.clone()));
        {
            let mut slot = self.maintenance_flight();
            if slot
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &sender))
            {
                *slot = None;
            }
        }
        result
    }

    /// 對應舊版 `runTokenMaintenance`：非強制時先看是否該驗證；有 token 時
    /// 驗證，驗證通過且離到期夠遠就沿用，否則換發。
    async fn run_token_maintenance(&self, force_refresh: bool) -> Result<(), TwitchApiError> {
        if !force_refresh && !self.should_validate_token().await {
            return Ok(());
        }

        if !force_refresh && !self.token_state().access_token.is_empty() {
            let validation = self.validate_token().await;
            if let TokenValidation::Valid { expires_in } = validation
                && expires_in > self.token_refresh_threshold_seconds
            {
                self.token_state().last_validated_at = Some(self.now_ms());
                self.logger.debug(
                    "twitch_api_token_validated",
                    Some(&expires_in_fields(expires_in)),
                );
                return Ok(());
            }
        }

        self.issue_app_access_token().await
    }

    /// 對應舊版 `shouldValidateToken`：從未驗證過，或距上次驗證已達間隔。
    async fn should_validate_token(&self) -> bool {
        let last_validated_at = self.token_state().last_validated_at;
        match last_validated_at {
            None => true,
            Some(last_ms) => {
                (self.now_ms().saturating_sub(last_ms) as f64) >= self.token_validation_interval_ms
            }
        }
    }

    /// 對應舊版 `validateToken`：任何失敗（transport、逾時、形狀不符）都回
    /// invalid；逾時涵蓋 fetch 與 body 讀取。
    async fn validate_token(&self) -> TokenValidation {
        let access_token = self.token_state().access_token.clone();
        let request = TwitchHttpRequest {
            url: self.validate_endpoint.to_string(),
            method: "GET",
            headers: vec![("Authorization".to_string(), format!("OAuth {access_token}"))],
            body: None,
        };

        let payload: Option<serde_json::Value> =
            tokio::time::timeout(self.timeout_duration(), async {
                let mut response = match self.transport.fetch(request).await {
                    Ok(response) => response,
                    Err(_) => return None,
                };
                if response.status != 200 {
                    return None;
                }
                read_all_json(response.body.as_mut()).await
            })
            .await
            .unwrap_or_default();
        let Some(payload) = payload else {
            return TokenValidation::Invalid;
        };

        let client_id_matches = payload.get("client_id").and_then(serde_json::Value::as_str)
            == Some(self.client_id.as_str());
        let expires_in = payload
            .get("expires_in")
            .and_then(serde_json::Value::as_f64)
            .filter(|value| is_safe_integer(*value) && *value >= 0.0);
        match (client_id_matches, expires_in) {
            (true, Some(expires_in)) => TokenValidation::Valid { expires_in },
            _ => TokenValidation::Invalid,
        }
    }

    /// 對應舊版 `issueAppAccessToken`：以 client credentials 換發 app access
    /// token；transport 失敗（含逾時）→ temporary `'network'`。
    async fn issue_app_access_token(&self) -> Result<(), TwitchApiError> {
        // 舊版 URLSearchParams body：插入順序 client_id、client_secret、grant_type。
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", &self.client_id)
            .append_pair("client_secret", self.client_secret.as_deref().unwrap_or(""))
            .append_pair("grant_type", "client_credentials")
            .finish();
        let request = TwitchHttpRequest {
            url: self.token_endpoint.to_string(),
            method: "POST",
            headers: vec![(
                "content-type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            )],
            body: Some(body),
        };

        enum IssueTokenOutcome {
            Issued {
                access_token: String,
                expires_in: f64,
            },
            Failure(TwitchApiError),
        }

        let outcome =
            tokio::time::timeout(self.timeout_duration(), async {
                let mut response = match self.transport.fetch(request).await {
                    Ok(response) => response,
                    // 舊版 catch：任何 transport 失敗（含逾時）→ temporary 'network'。
                    Err(_) => {
                        return IssueTokenOutcome::Failure(
                            self.temporary_error(TwitchApiTemporaryErrorReason::Network, None),
                        );
                    }
                };
                let status = response.status;
                if status != 200 {
                    // 舊版：403 以外的狀態都以 401 呈現，但 log 記錄實際狀態。
                    let error = TwitchApiAuthError::new(if status == 403 { 403 } else { 401 });
                    self.logger.error(
                        LogEvents::TWITCH_API_AUTH_FAILED,
                        Some(&status_code_fields(status)),
                    );
                    return IssueTokenOutcome::Failure(error.into());
                }
                // 舊版 response.json()（無大小限制）；讀取或解析失敗 → invalid-response。
                let Some(payload) = read_all_json(response.body.as_mut()).await else {
                    return IssueTokenOutcome::Failure(self.temporary_error(
                        TwitchApiTemporaryErrorReason::InvalidResponse,
                        Some(status),
                    ));
                };
                let access_token = payload
                    .get("access_token")
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty());
                let expires_in = payload
                    .get("expires_in")
                    .and_then(serde_json::Value::as_f64)
                    .filter(|value| is_safe_integer(*value) && *value > 0.0);
                match (access_token, expires_in) {
                    (Some(access_token), Some(expires_in)) => IssueTokenOutcome::Issued {
                        access_token: access_token.to_string(),
                        expires_in,
                    },
                    _ => IssueTokenOutcome::Failure(self.temporary_error(
                        TwitchApiTemporaryErrorReason::InvalidResponse,
                        Some(status),
                    )),
                }
            })
            .await;

        match outcome {
            Ok(IssueTokenOutcome::Issued {
                access_token,
                expires_in,
            }) => {
                let mut state = self.token_state();
                state.access_token = access_token;
                state.last_validated_at = Some(self.now_ms());
                drop(state);
                self.logger.info(
                    "twitch_api_token_refreshed",
                    Some(&expires_in_fields(expires_in)),
                );
                Ok(())
            }
            Ok(IssueTokenOutcome::Failure(error)) => Err(error),
            // 舊版逾時 → fetch reject → catch → temporary 'network'。
            Err(_) => Err(self.temporary_error(TwitchApiTemporaryErrorReason::Network, None)),
        }
    }

    /// 舊版 `createRateLimitError`：優先使用 `Ratelimit-Reset` 的 epoch seconds
    /// （clamp 到不早於現在、上限 5 分鐘），無法解析時以輪詢間隔的 2 倍計算。
    fn create_rate_limit_error(&self, headers: &TwitchHttpHeaders) -> TwitchApiRateLimitError {
        let now_ms = self.now_ms();
        match parse_reset_epoch_seconds(headers.get("Ratelimit-Reset")) {
            Some(reset_epoch_seconds) => {
                // 舊版 requestedRetryAfterMs = max(0, reset * 1000 − now)。
                let requested_retry_after_ms = (reset_epoch_seconds
                    .saturating_mul(1_000)
                    .saturating_sub(now_ms))
                .max(0);
                let retry_after_ms = requested_retry_after_ms as u64;
                let retry_after_ms = retry_after_ms.min(MAX_RATE_LIMIT_FALLBACK_MS);
                TwitchApiRateLimitError::new(now_ms + retry_after_ms as i64, retry_after_ms)
            }
            None => {
                // 舊版 fallback：checkIntervalSeconds × 2 × 1000（與 5 分鐘取小）。
                let retry_after_ms = (self.check_interval_seconds * 2.0 * 1_000.0) as u64;
                let retry_after_ms = retry_after_ms.min(MAX_RATE_LIMIT_FALLBACK_MS);
                TwitchApiRateLimitError::new(now_ms + retry_after_ms as i64, retry_after_ms)
            }
        }
    }

    /// 舊版 `createTemporaryError`：建立 temporary 錯誤並記
    /// `twitch_api_temporary_error`（有 status 時欄位加 `statusCode`）。
    fn temporary_error(
        &self,
        reason: TwitchApiTemporaryErrorReason,
        status_code: Option<u16>,
    ) -> TwitchApiError {
        let mut fields = vec![(
            "reason".to_string(),
            SanitizedValue::String(reason.as_str().to_string()),
        )];
        if let Some(status) = status_code {
            fields.push((
                "statusCode".to_string(),
                SanitizedValue::Number(f64::from(status)),
            ));
        }
        self.logger.warn(
            "twitch_api_temporary_error",
            Some(&SanitizedValue::Object(fields)),
        );
        TwitchApiError::Temporary(TwitchApiTemporaryError::new(reason, status_code))
    }

    /// 舊版 `Date.getTime()`：epoch 毫秒（奈秒截斷到毫秒）。
    fn now_ms(&self) -> i64 {
        offset_date_time_epoch_ms((self.now)())
    }

    /// 舊版 `requestTimeoutMs` 的 [`Duration`]（建構子已保證正有限且 ≤ 上限）。
    fn timeout_duration(&self) -> Duration {
        Duration::from_secs_f64(self.request_timeout_ms / 1_000.0)
    }

    /// 共享的 token 狀態守護；守護範圍內不會 panic，中毒（理論上不可達）時取回
    /// 內部值照常使用。
    fn token_state(&self) -> MutexGuard<'_, TokenState> {
        self.token_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 共享的單一飛行槽位守護（同 [`TwitchApiClient::token_state`] 的中毒處理）。
    fn maintenance_flight(&self) -> MutexGuard<'_, Option<TokenMaintenanceFlightSender>> {
        self.maintenance_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 對應舊版 `TwitchApiClient implements LiveStatusProvider`。
#[async_trait]
impl LiveStatusProvider for TwitchApiClient {
    async fn get_live_statuses(
        &self,
        channels: &[String],
    ) -> Result<Vec<ChannelLiveStatus>, TwitchApiError> {
        // inherent 的 get_live_statuses 同簽名；以完整路徑呼叫避免誤呼本 impl 造成遞迴。
        TwitchApiClient::get_live_statuses(self, channels).await
    }
}

/// 舊版 `readLimitedJson` 的錯誤：大小上限（`TypeError`）、JSON 解析
/// （`SyntaxError`）或 body 串流的 transport 失敗。
enum BodyReadError {
    TooLarge,
    Parse,
    Transport(TwitchTransportError),
}

/// 對應舊版 `readLimitedJson`：Content-Length 預檢查、逐 chunk 累計不超過
/// `maximum_bytes`、讀完後解析 JSON。
async fn read_limited_json(
    headers: &TwitchHttpHeaders,
    body: &mut dyn TwitchHttpBody,
    maximum_bytes: usize,
) -> Result<serde_json::Value, BodyReadError> {
    if let Some(content_length) = headers.get("Content-Length") {
        // 舊版：/^\d+$/u 且 Number(contentLength) > maximumBytes。
        if is_ascii_digits(content_length)
            && content_length.parse::<f64>().unwrap_or(f64::INFINITY) > maximum_bytes as f64
        {
            return Err(BodyReadError::TooLarge);
        }
    }

    let mut total_bytes = 0usize;
    let mut buffer: Vec<u8> = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        let chunk = chunk.map_err(BodyReadError::Transport)?;
        total_bytes += chunk.len();
        if total_bytes > maximum_bytes {
            // 舊版 reader.cancel() 後丟 TypeError；Rust 丟棄（drop）即取消。
            return Err(BodyReadError::TooLarge);
        }
        buffer.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&buffer).map_err(|_| BodyReadError::Parse)
}

/// 舊版 `response.json()` 的對應（validate／token 端點用，無大小限制）；
/// 讀取或解析失敗回 [`None`]。
async fn read_all_json(body: &mut dyn TwitchHttpBody) -> Option<serde_json::Value> {
    let mut buffer: Vec<u8> = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        buffer.extend_from_slice(&chunk.ok()?);
    }
    serde_json::from_slice(&buffer).ok()
}

/// 舊版 `parseHelixStreams`：`data` 必須是物件內的陣列、每筆照 schema 解析、
/// 正規化後的 `user_login` 不得重複。
fn parse_helix_streams(payload: &serde_json::Value) -> Result<Vec<HelixStream>, HelixParseError> {
    let data = payload
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or(HelixParseError)?;

    let mut streams = Vec::with_capacity(data.len());
    for value in data {
        streams.push(parse_helix_stream(value)?);
    }

    let mut user_logins = HashSet::new();
    for stream in &streams {
        if !user_logins.insert(normalize_channel(&stream.user_login)) {
            return Err(HelixParseError);
        }
    }

    Ok(streams)
}

/// 舊版 schema 檢查失敗（`TypeError('Invalid Twitch API response')`）的
/// 內部錯誤值；呼叫端轉成 temporary `'invalid-response'`。
#[derive(Debug)]
struct HelixParseError;

/// 對應舊版 `parseHelixStream`。
fn parse_helix_stream(value: &serde_json::Value) -> Result<HelixStream, HelixParseError> {
    // 舊版 isRecord：物件且非陣列。
    if !value.is_object() {
        return Err(HelixParseError);
    }

    let id = require_response_string(value.get("id"), false)?;
    let user_login = require_response_string(value.get("user_login"), false)?;
    let title = require_response_string(value.get("title"), true)?;
    let started_at = require_response_timestamp(value.get("started_at"))?;
    let viewer_count = require_viewer_count(value.get("viewer_count"))?;

    Ok(HelixStream {
        id,
        user_login,
        title,
        started_at,
        viewer_count,
    })
}

/// 對應舊版 `requireResponseString`：必須是字串；`allowEmpty` 為 false 時不得為空。
fn require_response_string(
    field: Option<&serde_json::Value>,
    allow_empty: bool,
) -> Result<String, HelixParseError> {
    let value = match field {
        Some(serde_json::Value::String(value)) => value,
        _ => return Err(HelixParseError),
    };
    if !allow_empty && value.is_empty() {
        return Err(HelixParseError);
    }
    Ok(value.clone())
}

/// 對應舊版 `requireResponseTimestamp`：字串且 `Date.parse` 可解析。
fn require_response_timestamp(
    field: Option<&serde_json::Value>,
) -> Result<String, HelixParseError> {
    let timestamp = require_response_string(field, false)?;
    if !is_parseable_date(&timestamp) {
        return Err(HelixParseError);
    }
    Ok(timestamp)
}

/// 舊版 `Number.isNaN(Date.parse(timestamp))` 的對應：以 RFC 3339 判定可否
/// 解析（Helix `started_at` 的合約格式；`Date.parse` 的其他寬鬆格式不支援）。
fn is_parseable_date(value: &str) -> bool {
    OffsetDateTime::parse(value, &Rfc3339).is_ok()
}

/// 對應舊版 `requireViewerCount`：必須是非負的安全整數。
fn require_viewer_count(field: Option<&serde_json::Value>) -> Result<u64, HelixParseError> {
    let value = match field {
        Some(serde_json::Value::Number(number)) => number.as_f64(),
        _ => return Err(HelixParseError),
    };
    let Some(value) = value else {
        return Err(HelixParseError);
    };
    if !(is_safe_integer(value) && value >= 0.0) {
        return Err(HelixParseError);
    }
    Ok(value as u64)
}

/// 舊版 `Number.isSafeInteger` 的對應：有限、整數、絕對值 ≤ 2^53 − 1。
fn is_safe_integer(value: f64) -> bool {
    value.is_finite() && value == value.trunc() && value.abs() <= MAX_SAFE_INTEGER
}

/// 對應舊版 `parseResetEpochSeconds`：接受純數字字串；必須是安全整數且
/// `new Date(seconds * 1000)` 是有效日期，否則 [`None`]。
fn parse_reset_epoch_seconds(value: Option<&str>) -> Option<i64> {
    let value = value?;
    if !is_ascii_digits(value) {
        return None;
    }
    let seconds = value.parse::<f64>().ok()?;
    if !is_safe_integer(seconds) {
        return None;
    }
    // 舊版 `new Date(seconds * 1000)`：epoch ms 不得超出 `Date` 的範圍
    // （±8.64e15 ms），否則 `getTime()` 是 NaN（回傳值照舊版是「秒」）。
    let epoch_ms = seconds * 1_000.0;
    if !epoch_ms.is_finite() || epoch_ms.abs() > 8_640_000_000_000_000.0 {
        return None;
    }
    Some(seconds as i64)
}

/// 舊版 `/^\d+$/u` 的判定：非空且全是 ASCII 數字。
fn is_ascii_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// 對應舊版 `normalizeChannel`：login 不分大小寫。
fn normalize_channel(channel: &str) -> String {
    channel.to_lowercase()
}

/// 對應 transport 失敗的 temporary reason。fetch 階段照 `fetchWithTimeout` 的
/// catch（逾時 → `'timeout'`、abort → `'aborted'`、其他 → `'network'`）；
/// body 階段照舊版 readLimitedJson 外層 catch 的判定順序（`didTimeOut()` 優先、
/// 次為 abort，其餘讀取失敗歸 `'invalid-response'`）。
fn transport_reason(
    error: TwitchTransportError,
    body_phase: bool,
) -> TwitchApiTemporaryErrorReason {
    match error {
        TwitchTransportError::Timeout => TwitchApiTemporaryErrorReason::Timeout,
        TwitchTransportError::Aborted => TwitchApiTemporaryErrorReason::Aborted,
        TwitchTransportError::Network if body_phase => {
            TwitchApiTemporaryErrorReason::InvalidResponse
        }
        TwitchTransportError::Network => TwitchApiTemporaryErrorReason::Network,
    }
}

/// 舊版 `{ statusCode }` 的 log 欄位。
fn status_code_fields(status: u16) -> SanitizedValue {
    SanitizedValue::Object(vec![(
        "statusCode".to_string(),
        SanitizedValue::Number(f64::from(status)),
    )])
}

/// 舊版 `{ expiresInSeconds }` 的 log 欄位。
fn expires_in_fields(expires_in: f64) -> SanitizedValue {
    SanitizedValue::Object(vec![(
        "expiresInSeconds".to_string(),
        SanitizedValue::Number(expires_in),
    )])
}

/// 對應舊版 `new URL(segment, baseUrl)`：baseUrl 缺結尾 `'/'` 時先補上再解析
/// （照舊端點組裝的行為）；解析失敗對應 URL 建構子的 `TypeError`。
fn resolve_relative(base_url: &str, segment: &str) -> Result<Url, TwitchApiClientInitError> {
    let normalized = if base_url.ends_with('/') {
        base_url.to_string()
    } else {
        format!("{base_url}/")
    };
    Url::parse(&normalized)
        .and_then(|base| base.join(segment))
        .map_err(|_| TwitchApiClientInitError::TypeError("Invalid URL".to_string()))
}

/// 舊版 `Date.getTime()`：epoch 毫秒（`unix_timestamp()` 秒 × 1000＋毫秒部分，
/// 奈秒以下截斷，與 `Date` 的整數毫秒相同）。
fn offset_date_time_epoch_ms(datetime: OffsetDateTime) -> i64 {
    datetime.unix_timestamp() * 1_000 + i64::from(datetime.millisecond())
}

/// 舊版 `toISOString()`：UTC 的 `YYYY-MM-DDTHH:MM:SS.sssZ`。
fn iso_utc_string(datetime: OffsetDateTime) -> String {
    // 年份轉換超出 time 支援範圍才會失敗；時鐘提供的 datetime 不可能，
    // 照 redact 的 `compile_pattern` 前例以明確訊息終止。
    let utc = match datetime.checked_to_offset(UtcOffset::UTC) {
        Some(utc) => utc,
        None => panic!("twitch api clock returned a datetime outside the supported range"),
    };
    match utc.format(&ISO_UTC_FORMAT) {
        Ok(formatted) => formatted,
        Err(error) => panic!("failed to format twitch api timestamp: {error}"),
    }
}

/// 對應舊版 `Date` 的 ISO 字串換算（epoch 毫秒 → `toISOString()`）。
///
/// `retryAt` 由現在時間加非負退避組成，不會超出 `Date` 的範圍；防衛性分支
/// 照 unreachable 慣例以明確訊息終止。
fn iso_utc_string_from_epoch_ms(epoch_ms: i64) -> String {
    let datetime = match OffsetDateTime::from_unix_timestamp_nanos(i128::from(epoch_ms) * 1_000_000)
    {
        Ok(datetime) => datetime,
        Err(_) => panic!("twitch api retryAt epoch ms is outside the supported range"),
    };
    iso_utc_string(datetime)
}

/// 舊版 `requireNonEmpty`：trim 後為空就丟 `TypeError`（JS 的 trim 字元集，
/// 與 `str::trim` 的差異見 `credentials` 模組的同一組字元類別）。
fn require_non_empty(value: &str, field: &str) -> Result<(), TwitchApiClientInitError> {
    if js_trim(value).is_empty() {
        return Err(TwitchApiClientInitError::TypeError(format!(
            "{field} must not be empty"
        )));
    }
    Ok(())
}

/// 舊版 `requirePositiveFinite`：必須是正的有限數。
fn require_positive_finite(value: f64, field: &str) -> Result<(), TwitchApiClientInitError> {
    if !value.is_finite() || value <= 0.0 {
        return Err(TwitchApiClientInitError::TypeError(format!(
            "{field} must be a positive number"
        )));
    }
    Ok(())
}

/// 舊版 `String.prototype.trim` 會移除的字元：JS 的 WhiteSpace 加行終止符
/// （U+0085 不移除、U+FEFF 移除，與 Rust `str::trim` 不同）。
fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// 對應舊版 `value.trim()`（去頭尾的 JS 空白）。
fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

#[cfg(test)]
mod tests;
