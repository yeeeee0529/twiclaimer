//! `scheduler` 模組：對應舊版 `src/scheduler`（§4.2）。
//! S2.5 先移植選台器；`watchdog`（WatchdogScheduler）屬 S3.5 另行移植。
pub(crate) mod selector;
pub(crate) mod watchdog;
