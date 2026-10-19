//! `config` 模組：對應舊版 `src/config`（§4.2）。
//!
//! S2.2 前半先提供 YAML 載入（[`yaml`]）與 [`errors`]；
//! `model`、`loader`、`writeback`、`thresholds` 於後續步驟加入。
pub(crate) mod errors;
pub(crate) mod writeback;
pub(crate) mod yaml;
