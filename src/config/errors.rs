//! 設定檔的錯誤型別：對應舊版 `config/errors.ts` 逐類別移植。
//!
//! - [`ConfigParseError`]：YAML 解析與讀檔錯誤，訊息前綴「設定檔格式錯誤：」。
//! - [`ConfigFileNotFoundError`]：設定檔不存在，訊息「找不到設定檔：<path>」。
//! - [`ConfigValidationError`]：欄位驗證錯誤，訊息「設定欄位 <field> <message>」。
//! - [`ConfigLoadError`]：`YamlConfigLoader::load` 可拋出的三種錯誤（Rust 沒有
//!   例外，用 enum 合併；`name()`／`message()` 對應舊版 `Error.name`／`Error.message`）。
#![allow(dead_code)]

use core::fmt;

/// 設定檔解析錯誤（對應舊版 `ConfigParseError`）。
///
/// 訊息一律以「設定檔格式錯誤：」開頭（舊版建構子會加上前綴），且不含行號
/// （舊版 `parseDocument` 帶 `prettyErrors: false`，`linePos` 永遠是 undefined）。
#[derive(Debug)]
pub(crate) struct ConfigParseError {
    /// 完整訊息（含「設定檔格式錯誤：」前綴；對應舊版 `Error.message`）。
    message: String,
}

impl ConfigParseError {
    /// 建立錯誤；`message` 為不含「設定檔格式錯誤：」前綴的訊息（照舊版建構子）。
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: format!("設定檔格式錯誤：{}", message.into()),
        }
    }

    /// 錯誤類別名稱（對應舊版 `Error.name`）。
    pub(crate) fn name(&self) -> &'static str {
        "ConfigParseError"
    }

    /// 完整訊息（對應舊版 `Error.message`）。
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ConfigParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigParseError {}

/// 設定檔不存在的錯誤（對應舊版 `ConfigFileNotFoundError`）。
#[derive(Debug)]
pub(crate) struct ConfigFileNotFoundError {
    /// 找不到的設定檔路徑（對應舊版 `configPath`）。
    config_path: String,
    /// 完整訊息（`找不到設定檔：<path>`）。
    message: String,
}

impl ConfigFileNotFoundError {
    /// 建立錯誤；記下找不到的路徑並組出完整訊息（照舊版建構子）。
    pub(crate) fn new(config_path: impl Into<String>) -> Self {
        let config_path = config_path.into();
        Self {
            message: format!("找不到設定檔：{config_path}"),
            config_path,
        }
    }

    /// 找不到的設定檔路徑（對應舊版 `configPath`）。
    pub(crate) fn config_path(&self) -> &str {
        &self.config_path
    }

    /// 錯誤類別名稱（對應舊版 `Error.name`）。
    pub(crate) fn name(&self) -> &'static str {
        "ConfigFileNotFoundError"
    }

    /// 完整訊息（對應舊版 `Error.message`）。
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ConfigFileNotFoundError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigFileNotFoundError {}

/// 欄位驗證錯誤（對應舊版 `ConfigValidationError`）。
#[derive(Debug)]
pub(crate) struct ConfigValidationError {
    /// 出錯的欄位名稱（對應舊版 `field`，如 `browser.engine`）。
    field: String,
    /// 完整訊息（`設定欄位 <field> <message>`）。
    message: String,
}

impl ConfigValidationError {
    /// 建立錯誤；`message` 為欄位層級的說明，完整訊息加上前綴（照舊版建構子）。
    pub(crate) fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        let field = field.into();
        Self {
            message: format!("設定欄位 {field} {}", message.into()),
            field,
        }
    }

    /// 出錯的欄位名稱（對應舊版 `field`）。
    pub(crate) fn field(&self) -> &str {
        &self.field
    }

    /// 錯誤類別名稱（對應舊版 `Error.name`）。
    pub(crate) fn name(&self) -> &'static str {
        "ConfigValidationError"
    }

    /// 完整訊息（對應舊版 `Error.message`）。
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ConfigValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigValidationError {}

/// [`crate::config::loader::YamlConfigLoader`] 的 `load` 可拋出的錯誤：
/// 對應舊版 load 直接拋出的三種例外類別。
#[derive(Debug)]
pub(crate) enum ConfigLoadError {
    /// 設定檔不存在。
    FileNotFound(ConfigFileNotFoundError),
    /// 讀取或 YAML 解析錯誤。
    Parse(ConfigParseError),
    /// 欄位驗證錯誤。
    Validation(ConfigValidationError),
}

impl ConfigLoadError {
    /// 錯誤類別名稱（對應舊版 `Error.name`）。
    pub(crate) fn name(&self) -> &'static str {
        match self {
            ConfigLoadError::FileNotFound(error) => error.name(),
            ConfigLoadError::Parse(error) => error.name(),
            ConfigLoadError::Validation(error) => error.name(),
        }
    }

    /// 完整訊息（對應舊版 `Error.message`）。
    pub(crate) fn message(&self) -> &str {
        match self {
            ConfigLoadError::FileNotFound(error) => error.message(),
            ConfigLoadError::Parse(error) => error.message(),
            ConfigLoadError::Validation(error) => error.message(),
        }
    }

    /// 出錯的欄位名稱；只有驗證錯誤有（對應舊版 `field` 屬性）。
    pub(crate) fn field(&self) -> Option<&str> {
        match self {
            ConfigLoadError::Validation(error) => Some(error.field()),
            _ => None,
        }
    }
}

impl From<ConfigParseError> for ConfigLoadError {
    fn from(error: ConfigParseError) -> Self {
        ConfigLoadError::Parse(error)
    }
}

impl From<ConfigValidationError> for ConfigLoadError {
    fn from(error: ConfigValidationError) -> Self {
        ConfigLoadError::Validation(error)
    }
}

impl fmt::Display for ConfigLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for ConfigLoadError {}
