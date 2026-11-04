//! `app` 模組：對應舊版 `src/app`（§4.2）。
pub(crate) mod cgroup;
pub(crate) mod create;
pub(crate) mod diagnostics;
pub(crate) mod process;
pub(crate) mod resource_guard;
pub(crate) mod resource_monitor;
pub(crate) mod restart;
pub(crate) mod runner;
pub(crate) mod stall_watchdog;
