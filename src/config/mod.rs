//! `config` 模組：對應舊版 `src/config`（§4.2）。
//!
//! - [`yaml`]：`parseYaml` 的移植（JS 相容的 [`yaml::JsValue`]）。
//! - [`model`]：`AppConfig` 的型別（舊版 `AppConfig.ts`）。
//! - [`loader`]：`YamlConfigLoader` 的移植（路徑、`${VAR}` 代換、環境覆寫、
//!   欄位驗證與 X1／X2）。
//! - [`thresholds`]：resource guard 門檻的縮放與壓低（舊版
//!   `resourceGuardThresholds.ts`）。
//! - [`errors`]：三種錯誤類別與 `load` 的合併錯誤。
//! - `writeback`（`RuntimeConfigManager`）屬 S2.3 加入。
pub(crate) mod errors;
pub(crate) mod loader;
pub(crate) mod model;
pub(crate) mod thresholds;
pub(crate) mod yaml;

/// golden 比對的共用輔助（測試專用）。
#[cfg(test)]
pub(crate) mod golden;
