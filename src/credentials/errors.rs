//! 登入狀態檔的錯誤型別：對應舊版 `credentials/errors.ts` 逐類別移植。
//!
//! - [`StorageStateNotFoundError`]：檔案不存在。
//! - [`StorageStateUnreadableError`]：其他讀取錯誤（訊息不含底層錯誤，
//!   照舊版避免敏感資料進入公開錯誤）。
//! - [`StorageStateParseError`]：JSON 語法錯誤。
//! - [`StorageStateFormatError`]：JSON 形狀不符（頂層或 `cookies`／`origins`）。
//! - [`CredentialValidationError`]：`FileCredentialValidator::validate` 可拋出的
//!   四種錯誤（Rust 沒有例外，用 enum 合併；`name()`／`message()` 對應舊版
//!   `Error.name`／`Error.message`）。
#![allow(dead_code)]

use core::fmt;

/// 登入狀態檔不存在的錯誤（對應舊版 `StorageStateNotFoundError`）。
#[derive(Debug)]
pub(crate) struct StorageStateNotFoundError {
    /// 找不到的登入狀態檔路徑（對應舊版 `storageStatePath`）。
    storage_state_path: String,
    /// 完整訊息（對應舊版 `Error.message`）。
    message: String,
}

impl StorageStateNotFoundError {
    /// 建立錯誤；記下路徑並組出完整訊息（照舊版建構子）。
    pub(crate) fn new(storage_state_path: impl Into<String>) -> Self {
        let storage_state_path = storage_state_path.into();
        Self {
            message: format!(
                "找不到登入狀態檔案：{storage_state_path}。\
                 請重新匯出 Playwright storageState 並掛載到容器"
            ),
            storage_state_path,
        }
    }

    /// 找不到的登入狀態檔路徑（對應舊版 `storageStatePath`）。
    pub(crate) fn storage_state_path(&self) -> &str {
        &self.storage_state_path
    }

    /// 錯誤類別名稱（對應舊版 `Error.name`）。
    pub(crate) fn name(&self) -> &'static str {
        "StorageStateNotFoundError"
    }

    /// 完整訊息（對應舊版 `Error.message`）。
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for StorageStateNotFoundError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StorageStateNotFoundError {}

/// 登入狀態檔無法讀取的錯誤（對應舊版 `StorageStateUnreadableError`）。
///
/// 舊版不吃進底層錯誤（連訊息都不帶），使底層即使含敏感資料也不會外洩。
#[derive(Debug)]
pub(crate) struct StorageStateUnreadableError {
    /// 無法讀取的登入狀態檔路徑（對應舊版 `storageStatePath`）。
    storage_state_path: String,
    /// 完整訊息（對應舊版 `Error.message`）。
    message: String,
}

impl StorageStateUnreadableError {
    /// 建立錯誤；記下路徑並組出完整訊息（照舊版建構子）。
    pub(crate) fn new(storage_state_path: impl Into<String>) -> Self {
        let storage_state_path = storage_state_path.into();
        Self {
            message: format!(
                "無法讀取登入狀態檔案：{storage_state_path}。\
                 請確認檔案已掛載，且執行服務的使用者具有讀取權限"
            ),
            storage_state_path,
        }
    }

    /// 無法讀取的登入狀態檔路徑（對應舊版 `storageStatePath`）。
    pub(crate) fn storage_state_path(&self) -> &str {
        &self.storage_state_path
    }

    /// 錯誤類別名稱（對應舊版 `Error.name`）。
    pub(crate) fn name(&self) -> &'static str {
        "StorageStateUnreadableError"
    }

    /// 完整訊息（對應舊版 `Error.message`）。
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for StorageStateUnreadableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StorageStateUnreadableError {}

/// 登入狀態檔 JSON 語法錯誤（對應舊版 `StorageStateParseError`）。
#[derive(Debug)]
pub(crate) struct StorageStateParseError {
    /// 解析失敗的登入狀態檔路徑（對應舊版 `storageStatePath`）。
    storage_state_path: String,
    /// 完整訊息（對應舊版 `Error.message`）。
    message: String,
}

impl StorageStateParseError {
    /// 建立錯誤；記下路徑並組出完整訊息（照舊版建構子）。
    pub(crate) fn new(storage_state_path: impl Into<String>) -> Self {
        let storage_state_path = storage_state_path.into();
        Self {
            message: format!(
                "登入狀態檔案格式錯誤：{storage_state_path}。\
                 請重新匯出有效的 Playwright storageState JSON"
            ),
            storage_state_path,
        }
    }

    /// 解析失敗的登入狀態檔路徑（對應舊版 `storageStatePath`）。
    pub(crate) fn storage_state_path(&self) -> &str {
        &self.storage_state_path
    }

    /// 錯誤類別名稱（對應舊版 `Error.name`）。
    pub(crate) fn name(&self) -> &'static str {
        "StorageStateParseError"
    }

    /// 完整訊息（對應舊版 `Error.message`）。
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for StorageStateParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StorageStateParseError {}

/// 登入狀態檔 JSON 形狀不符的錯誤（對應舊版 `StorageStateFormatError`）。
#[derive(Debug)]
pub(crate) struct StorageStateFormatError {
    /// 格式錯誤的登入狀態檔路徑（對應舊版 `storageStatePath`）。
    storage_state_path: String,
    /// 完整訊息（對應舊版 `Error.message`）。
    message: String,
}

impl StorageStateFormatError {
    /// 建立錯誤；`detail` 為形狀說明（照舊版建構子；舊版不另外保存 detail 屬性）。
    pub(crate) fn new(storage_state_path: impl Into<String>, detail: impl Into<String>) -> Self {
        let storage_state_path = storage_state_path.into();
        Self {
            message: format!(
                "登入狀態檔案格式錯誤：{storage_state_path}。{}；\
                 請重新匯出有效的 Playwright storageState JSON",
                detail.into(),
            ),
            storage_state_path,
        }
    }

    /// 格式錯誤的登入狀態檔路徑（對應舊版 `storageStatePath`）。
    pub(crate) fn storage_state_path(&self) -> &str {
        &self.storage_state_path
    }

    /// 錯誤類別名稱（對應舊版 `Error.name`）。
    pub(crate) fn name(&self) -> &'static str {
        "StorageStateFormatError"
    }

    /// 完整訊息（對應舊版 `Error.message`）。
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for StorageStateFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StorageStateFormatError {}

/// [`crate::credentials::validator::FileCredentialValidator`] 的 `validate` 可拋出的
/// 錯誤：對應舊版 validate 直接拋出的四種例外類別。
#[derive(Debug)]
pub(crate) enum CredentialValidationError {
    /// 登入狀態檔不存在。
    NotFound(StorageStateNotFoundError),
    /// 其他讀取錯誤。
    Unreadable(StorageStateUnreadableError),
    /// JSON 語法錯誤。
    Parse(StorageStateParseError),
    /// JSON 形狀不符。
    Format(StorageStateFormatError),
}

impl CredentialValidationError {
    /// 錯誤類別名稱（對應舊版 `Error.name`）。
    pub(crate) fn name(&self) -> &'static str {
        match self {
            CredentialValidationError::NotFound(error) => error.name(),
            CredentialValidationError::Unreadable(error) => error.name(),
            CredentialValidationError::Parse(error) => error.name(),
            CredentialValidationError::Format(error) => error.name(),
        }
    }

    /// 完整訊息（對應舊版 `Error.message`）。
    pub(crate) fn message(&self) -> &str {
        match self {
            CredentialValidationError::NotFound(error) => error.message(),
            CredentialValidationError::Unreadable(error) => error.message(),
            CredentialValidationError::Parse(error) => error.message(),
            CredentialValidationError::Format(error) => error.message(),
        }
    }

    /// 出錯的登入狀態檔路徑（對應舊版 `storageStatePath` 屬性；四種錯誤都有）。
    pub(crate) fn storage_state_path(&self) -> &str {
        match self {
            CredentialValidationError::NotFound(error) => error.storage_state_path(),
            CredentialValidationError::Unreadable(error) => error.storage_state_path(),
            CredentialValidationError::Parse(error) => error.storage_state_path(),
            CredentialValidationError::Format(error) => error.storage_state_path(),
        }
    }
}

impl From<StorageStateNotFoundError> for CredentialValidationError {
    fn from(error: StorageStateNotFoundError) -> Self {
        CredentialValidationError::NotFound(error)
    }
}

impl From<StorageStateUnreadableError> for CredentialValidationError {
    fn from(error: StorageStateUnreadableError) -> Self {
        CredentialValidationError::Unreadable(error)
    }
}

impl From<StorageStateParseError> for CredentialValidationError {
    fn from(error: StorageStateParseError) -> Self {
        CredentialValidationError::Parse(error)
    }
}

impl From<StorageStateFormatError> for CredentialValidationError {
    fn from(error: StorageStateFormatError) -> Self {
        CredentialValidationError::Format(error)
    }
}

impl fmt::Display for CredentialValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for CredentialValidationError {}
