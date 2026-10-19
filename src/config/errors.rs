//! 設定檔的錯誤型別：對應舊版 `config/errors.ts`。
//!
//! S2.2 前半先提供 `ConfigParseError`；`ConfigFileNotFoundError`、
//! `ConfigValidationError` 隨 ConfigLoader 步驟（§7.1 下半）加入。
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
