//! `scheduler` 模組：對應舊版 `src/scheduler`（§4.2）。
//! `selector`（選台自由函式）在 S2.5 移植；`watchdog`（WatchdogScheduler＋
//! 各依賴接縫）在 S3.5 移植。
pub(crate) mod selector;
pub(crate) mod watchdog;
