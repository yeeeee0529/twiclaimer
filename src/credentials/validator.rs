//! 憑證驗證器：`FileCredentialValidator` 逐函式移植（§4.2，行為依據舊版
//! `credentials/CredentialValidator.ts`）。
//!
//! - [`FileCredentialValidator::validate`]：讀取 storageState → 解析形狀 →
//!   計算不含憑證內容的安全摘要 [`CredentialValidationResult`] → 記
//!   `credential_checked`（cookies 為空時先記 `credential_storage_state_empty_cookies`
//!   警告，但允許啟動）。
//! - 讀檔以 [`CredentialReadFile`] 注入（對應舊版同名的
//!   `(path, encoding) => Promise<string>`）；舊版唯一使用的 encoding 是
//!   `'utf8'`，Rust 版固定 UTF-8、省略該參數。錯誤以 [`io::Error`] 對應
//!   Node 的 ErrnoException（`ENOENT` ↔ [`io::ErrorKind::NotFound`]）。
//! - 日誌介面 [`CredentialLogger`] 對應舊版同名介面；正式程式用
//!   [`JsonLineLogger`](crate::logging::logger::JsonLineLogger) 的轉接 impl。
//! - 舊版結果物件的 `Object.freeze` 由 Rust 的所有權模型保證（透過參照無法
//!   修改），不另做對應實作。
#![allow(dead_code)]

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;

use super::errors::{
    CredentialValidationError, StorageStateNotFoundError, StorageStateUnreadableError,
};
use super::storage_state::parse_storage_state;
use crate::config::model::AppConfig;
use crate::logging::logger::{JsonLineLogger, LogEvents};
use crate::logging::redact::SanitizedValue;

/// 對應舊版 `CredentialValidationResult`：不含任何憑證內容的安全摘要。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CredentialValidationResult {
    pub(crate) storage_state_path: String,
    pub(crate) has_cookies: bool,
    pub(crate) twitch_api_configured: bool,
}

/// 對應舊版 `CredentialLogger`：驗證器的日誌介面（欄位值為消毒前的
/// [`SanitizedValue`]，消毒在 Logger 本體進行）。
pub(crate) trait CredentialLogger: Send + Sync {
    /// 對應 `info(event, fields?)`。
    fn info(&self, event: &str, fields: Option<&SanitizedValue>);

    /// 對應 `warn(event, fields?)`。
    fn warn(&self, event: &str, fields: Option<&SanitizedValue>);
}

/// 舊版預設參數的 `NOOP_LOGGER`。
struct NoopCredentialLogger;

impl CredentialLogger for NoopCredentialLogger {
    fn info(&self, _event: &str, _fields: Option<&SanitizedValue>) {}
    fn warn(&self, _event: &str, _fields: Option<&SanitizedValue>) {}
}

/// 對應舊版 `CredentialReadFile` 的回傳值 `Promise<string>`
/// （`Send` 供 S3.6 的 tokio 任務使用）。
pub(crate) type CredentialReadFileFuture = Pin<Box<dyn Future<Output = io::Result<String>> + Send>>;

/// 對應舊版 `CredentialReadFile`：`(path, encoding) => Promise<string>` 的
/// 讀檔注入介面。
///
/// 舊版只在 `validate` 內以 `'utf8'` 呼叫，Rust 版固定 UTF-8、省略 encoding
/// 參數（介面形狀的對應調整，行為相同）。
#[allow(clippy::type_complexity)]
pub(crate) type CredentialReadFile = Box<dyn Fn(&str) -> CredentialReadFileFuture + Send + Sync>;

/// 舊版 `DEFAULT_READ_FILE`：以 tokio 的檔案系統 API 讀取 UTF-8 文字。
fn default_read_file(path: &str) -> CredentialReadFileFuture {
    let path = path.to_string();
    Box::pin(async move { tokio::fs::read_to_string(&path).await })
}

/// 對應舊版 `FileCredentialValidatorOptions`；`None` 用預設
/// （no-op logger、tokio 讀檔）。
#[derive(Default)]
pub(crate) struct FileCredentialValidatorOptions {
    pub(crate) logger: Option<Arc<dyn CredentialLogger>>,
    pub(crate) read_file: Option<CredentialReadFile>,
}

/// 對應舊版 `FileCredentialValidator`。
pub(crate) struct FileCredentialValidator {
    /// 舊版 `logger`（建構參數）。
    logger: Arc<dyn CredentialLogger>,
    /// 舊版 `readFile`（建構參數）。
    read_file: CredentialReadFile,
}

impl Default for FileCredentialValidator {
    /// 舊版建構子的預設參數（`options = {}`）。
    fn default() -> Self {
        Self::new(FileCredentialValidatorOptions::default())
    }
}

impl FileCredentialValidator {
    /// 對應舊版建構子；沒有預設值的使用者明確傳入。
    pub(crate) fn new(options: FileCredentialValidatorOptions) -> Self {
        Self {
            logger: options
                .logger
                .unwrap_or_else(|| Arc::new(NoopCredentialLogger)),
            read_file: options
                .read_file
                .unwrap_or_else(|| Box::new(default_read_file)),
        }
    }

    /// 對應舊版 `validate`：讀檔 → 解析 → 摘要 → 日誌。
    pub(crate) async fn validate(
        &self,
        config: &AppConfig,
    ) -> Result<CredentialValidationResult, CredentialValidationError> {
        let source = self.read_storage_state(&config.storage_state_path).await?;
        let storage_state = parse_storage_state(&source, &config.storage_state_path)?;
        // 舊版 `(storageState.cookies?.length ?? 0) > 0`。
        let has_cookies = storage_state.has_cookies();
        let result = CredentialValidationResult {
            storage_state_path: config.storage_state_path.clone(),
            has_cookies,
            twitch_api_configured: is_twitch_api_configured(config),
        };
        // 舊版 warn／info 都帶同一形狀的欄位物件（鍵名與順序照舊版）。
        let fields = result_fields(&result);

        if !has_cookies {
            self.logger
                .warn("credential_storage_state_empty_cookies", Some(&fields));
        }

        self.logger
            .info(LogEvents::CREDENTIAL_CHECKED, Some(&fields));

        Ok(result)
    }

    /// 對應舊版 `readStorageState`：不存在與其他讀取錯誤分開回報；
    /// 底層錯誤不進入公開錯誤（避免敏感資料外洩）。
    async fn read_storage_state(
        &self,
        storage_state_path: &str,
    ) -> Result<String, CredentialValidationError> {
        match (self.read_file)(storage_state_path).await {
            Ok(source) => Ok(source),
            // 舊版 `isNodeError(error) && error.code === 'ENOENT'`。
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Err(CredentialValidationError::NotFound(
                    StorageStateNotFoundError::new(storage_state_path),
                ))
            }
            Err(_) => Err(CredentialValidationError::Unreadable(
                StorageStateUnreadableError::new(storage_state_path),
            )),
        }
    }
}

/// 對應舊版 `CredentialValidator` 介面（S3.6 `AppRunner` 的注入點；D12 介面 →
/// trait）。
#[async_trait]
pub(crate) trait CredentialValidator: Send + Sync {
    /// 對應 `validate(config)`。
    async fn validate(
        &self,
        config: &AppConfig,
    ) -> Result<CredentialValidationResult, CredentialValidationError>;
}

#[async_trait]
impl CredentialValidator for FileCredentialValidator {
    async fn validate(
        &self,
        config: &AppConfig,
    ) -> Result<CredentialValidationResult, CredentialValidationError> {
        // inherent 的 validate 同簽名；以完整路徑呼叫避免誤呼本 impl 造成遞迴。
        FileCredentialValidator::validate(self, config).await
    }
}

/// 對應舊版 `JsonLineLogger` 滿足 `CredentialLogger` 介面的轉接（S3.6 接線用）。
impl CredentialLogger for JsonLineLogger {
    fn info(&self, event: &str, fields: Option<&SanitizedValue>) {
        // inherent 的 info 同簽名；以完整路徑呼叫避免誤呼本 impl 造成遞迴。
        JsonLineLogger::info(self, event, fields);
    }

    fn warn(&self, event: &str, fields: Option<&SanitizedValue>) {
        JsonLineLogger::warn(self, event, fields);
    }
}

/// 對應舊版摘要的 `twitchApiConfigured` 運算式：`clientId.trim().length > 0 &&
/// (accessToken.trim().length > 0 || clientSecret.trim().length > 0)`。
fn is_twitch_api_configured(config: &AppConfig) -> bool {
    !js_trim(&config.twitch_api.client_id).is_empty()
        && (!js_trim(&config.twitch_api.access_token).is_empty()
            || !js_trim(&config.twitch_api.client_secret).is_empty())
}

/// 舊版 `String.prototype.trim` 會移除的字元：JS 的 WhiteSpace 加行終止符。
/// 與 Rust `str::trim` 的差異只有 U+0085（Rust 會移除、JS 不會）與
/// U+FEFF（JS 會移除、Rust 不會），照舊版行為實作（§7.7 的同一組字元類別）。
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
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

/// 舊版記錄用的欄位物件（鍵名照舊版 `storageStatePath`、`hasCookies`、
/// `twitchApiConfigured`；插入順序即 JSON 輸出順序）。
fn result_fields(result: &CredentialValidationResult) -> SanitizedValue {
    SanitizedValue::Object(vec![
        (
            "storageStatePath".to_string(),
            SanitizedValue::String(result.storage_state_path.clone()),
        ),
        (
            "hasCookies".to_string(),
            SanitizedValue::Bool(result.has_cookies),
        ),
        (
            "twitchApiConfigured".to_string(),
            SanitizedValue::Bool(result.twitch_api_configured),
        ),
    ])
}

#[cfg(test)]
mod tests;
