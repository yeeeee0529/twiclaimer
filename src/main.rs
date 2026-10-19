//! twiclaimer CLI（§4.2）：`--version` 與開發用 `probe` 子命令（§7.3.7），
//! 正式服務的啟動在後續步驟加入。
//!
//! probe 需要使用 crate 內部的型別（`browser::cdp` 全是 `pub(crate)`），而 bin 與
//! lib 是兩個 crate、`lib.rs` 不開對外 API；這裡以 `#[path]` 把模組樹直接掛進
//! bin crate（同一批原始檔會被 lib（含測試）與 bin 各編譯一次）。

#[path = "logging/mod.rs"]
mod logging;

#[path = "browser/mod.rs"]
mod browser;

/// probe 的進入點（實作在 `src/browser/cdp/probe.rs`，§7.3.7）。
use browser::cdp::probe;

fn main() {
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
        None => print_usage(),
    }
}

/// 以 current_thread runtime（D13）執行 probe。
fn run_probe(args: &[String]) -> Result<(), String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("probe: cannot start the async runtime: {error}"))?
        .block_on(probe::run(args))
}

/// 用法說明（stdout；錯誤訊息走 stderr）。
fn print_usage() {
    println!(
        "usage:\n  twiclaimer --version\n  twiclaimer probe <storage-state.json> <page> [--hold <seconds>] [--screenshot <file>]"
    );
}
