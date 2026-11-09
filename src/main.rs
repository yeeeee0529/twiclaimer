//! twiclaimer CLI（§4.2）：無參數啟動正式服務（舊版 `index.ts` 的 `main()`，
//! 經 lib 的 `run_service`）、`--version` 與開發用 `probe` 子命令（§7.3.7）。
//!
//! probe 與服務都經 lib 的公開 API（`twiclaimer::run_probe`／
//! `twiclaimer::run_service`）呼叫（DEC-014 收尾：bin 不再以 `#[path]` 掛載
//! 模組樹）。

fn main() {
    // 計畫（rustls 依賴列）：main 開頭安裝 aws-lc-rs provider，避免 provider
    // 不明確時 panic；重複安裝的 Err 無害。
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version") => println!("twiclaimer {}", env!("CARGO_PKG_VERSION")),
        Some("probe") => {
            if let Err(message) = run_probe(&args[1..]) {
                eprintln!("{message}");
                std::process::exit(1);
            }
        }
        Some(other) => {
            eprintln!("unknown command: {other}");
            print_usage();
            std::process::exit(2);
        }
        // 舊版 index.ts：無參數即啟動服務。
        None => start_service(),
    }
}

/// 以 current_thread runtime（D13）執行服務；結束前 flush stdout（§7.9）。
fn start_service() {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(4)
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cannot start the async runtime: {error}");
            std::process::exit(1);
        }
    };
    let exit_code = runtime.block_on(twiclaimer::run_service());
    // §7.9：程序結束前 flush 日誌（寫入執行緒的佇列與 stdout；有上限，不會卡住結束）。
    twiclaimer::flush_log_output();
    std::process::exit(exit_code);
}

/// 以 current_thread runtime（D13）執行 probe。
fn run_probe(args: &[String]) -> Result<(), String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("probe: cannot start the async runtime: {error}"))?
        .block_on(twiclaimer::run_probe(args))
}

/// 用法說明（stdout；錯誤訊息走 stderr）。
fn print_usage() {
    println!(
        "usage:\n  twiclaimer --version\n  twiclaimer probe <storage-state.json> <page> [--hold <seconds>] [--screenshot <file>]"
    );
}
