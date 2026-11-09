//! `twiclaimer`：Twitch Watchdog 的 Rust 版（S1.1 骨架，模組依 §4.2 逐步加入）。
pub mod app;
pub mod browser;
pub mod config;
pub mod credentials;
pub mod discord;
pub mod logging;
pub mod notifications;
pub mod scheduler;
pub mod sessions;
pub mod telegram;
pub(crate) mod twitch;

use std::sync::Arc;

use async_trait::async_trait;

use crate::app::create::{CreateApplicationOptions, create_application};
use crate::app::process::{
    InstallProcessHandlersOptions, install_process_handlers, process_exit_code,
};
use crate::app::runner::AppRunner;
use crate::browser::cdp::launcher::kill_all_registered;
use crate::logging::logger::{JsonLineLoggerOptions, LogLevel, create_logger};

/// 程序結束前把日誌寫入執行緒佇列內的行寫完並 flush stdout（§7.9）。
/// 最多等 2 秒：stdout 卡住時不阻塞程序結束。
pub fn flush_log_output() {
    use crate::logging::logger::{LogWriter, StdoutWriter};
    let _ = StdoutWriter.flush();
}

/// `probe` 子命令的進入點（bin 的公開面；實作在 `browser::cdp::probe`，§7.3.7。
/// DEC-014 的收尾：bin 不再以 `#[path]` 掛載模組樹，改呼叫 lib 的公開 API）。
pub async fn run_probe(args: &[String]) -> Result<(), String> {
    browser::cdp::probe::run(args).await
}

/// 舊版 `src/index.ts` 的 `main()`（§4.2：bin 無參數時的正式啟動路徑）：
/// bootstrap logger → `createApplication`（正式相依）→ `installProcessHandlers`
/// → `app.start()`。start 失敗對應舊版 catch 的 `process.exitCode = 1`；
/// 成功時等待任一停止路徑（訊號／panic）完成 `stop()`，回傳程序 exit code。
///
/// 舊版靠事件迴圈排空結束程序；Rust 以 [`StopNotifyingRunner`] 在 `stop()`
/// 完成點通知本函式收尾。致命路徑（restart controller／stall watchdog）直接
/// `process::exit`，不經此處。呼叫端（bin）負責最後的 flush 與 `process::exit`。
pub async fn run_service() -> i32 {
    // 舊版 `createLogger({ level: 'info' })`。
    let bootstrap_logger = Arc::new(create_logger(JsonLineLoggerOptions {
        level: Some(LogLevel::Info),
        ..JsonLineLoggerOptions::default()
    }));
    let (stop_done_sender, mut stop_done_receiver) = tokio::sync::watch::channel(false);
    // 舊版：`createApplication({ configPath, env: process.env, bootstrapLogger })`。
    // `configPath`／`env` 的預設鏈（`CONFIG_PATH ?? DEFAULT_CONFIG_PATH`、真實
    // 程序環境）與 `None` 欄位的預設相同；其餘注入接縫不覆寫（正式相依）。
    let app = create_application(CreateApplicationOptions {
        config_path: None,
        env: None,
        bootstrap_logger: Some(Arc::clone(&bootstrap_logger)),
        config_loader: None,
        credential_validator: None,
        logger_factory: None,
        runtime_factory: None,
    });
    let runner: Arc<dyn AppRunner> = Arc::new(StopNotifyingRunner {
        inner: app,
        stop_done: stop_done_sender,
    });
    // 舊版 index.ts 不保留 `removeProcessHandlers`：handlers 與程序同壽命。
    // drop remover 即「永不撤銷」（訊號任務 detach、panic hook 保留）。
    drop(install_process_handlers(InstallProcessHandlersOptions {
        app: Arc::clone(&runner),
        // 舊版：`installProcessHandlers({ app, logger: bootstrapLogger })`。
        logger: bootstrap_logger,
        target: None,
    }));
    let exit_code = match runner.start().await {
        Ok(()) => {
            // 舊版：`main()` 返回後事件迴圈持續到服務停止；等任一停止路徑的
            // `stop()` 完成，再以程序層 exit code 收尾（§7.9）。
            let _ = stop_done_receiver.changed().await;
            process_exit_code()
        }
        // 舊版 catch：`process.exitCode = 1`。
        Err(_) => 1,
    };
    // §7.9：程序結束前，對 registry 內所有存活的瀏覽器 process group 送
    // SIGKILL（§7.3.1 的防線）。
    kill_all_registered();
    exit_code
}

/// `run_service` 的包裝：委派 [`AppRunner`]，並在 `stop()` 完成時通知服務
/// 主流程（對映舊版「事件迴圈在全部停止後排空」的結束時機）。
struct StopNotifyingRunner {
    inner: Arc<dyn AppRunner>,
    stop_done: tokio::sync::watch::Sender<bool>,
}

#[async_trait]
impl AppRunner for StopNotifyingRunner {
    async fn start(&self) -> Result<(), String> {
        self.inner.start().await
    }

    async fn stop(&self, reason: &str) -> Result<(), String> {
        let result = self.inner.stop(reason).await;
        let _ = self.stop_done.send(true);
        result
    }
}

/// 跨模組整合測試與 e2e（計畫 §4.1）。
#[cfg(test)]
mod tests;
