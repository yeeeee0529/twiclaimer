//! S1.1 骨架：只處理 `--version`；`probe` 子命令與 app 啟動在後續步驟加入（§4.2）。
fn main() {
    if std::env::args().any(|arg| arg == "--version") {
        println!("twiclaimer {}", env!("CARGO_PKG_VERSION"));
    }
}
