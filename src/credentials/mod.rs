//! `credentials` 模組：對應舊版 `src/credentials`（§4.2）。
//!
//! - [`validator`]：`FileCredentialValidator` 與 `validate` 流程，含
//!   `CredentialLogger`／`CredentialReadFile` 注入介面與舊版
//!   `CredentialValidator` 介面的 trait 對應。
//! - [`storage_state`]：`StorageState` 與 `parseStorageState`（舊版放在
//!   `CredentialValidator.ts` 內，依 §4.2 拆成獨立檔案）。
//! - [`errors`]：四種錯誤類別（舊版 `errors.ts`）與 `validate` 的合併錯誤。
//!
//! 舊版 `index.ts` 的 re-export 對應 Rust 的明確路徑呼叫，不另做 re-export
//! （`config` 模組同此記法）。
pub(crate) mod errors;
pub(crate) mod storage_state;
pub(crate) mod validator;
