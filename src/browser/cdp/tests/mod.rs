//! 錯誤對照（X7、G5）與 crash 行為的 e2e：重放 `doc/spec/pw-errors.jsonl` 的
//! 33 案例，比對 `name` 與 `message`（只比第一行，port 正規化）。
//!
//! 伺服器照 `tools/ref/pw-errors.mjs`：`/` 回四按鈕樣本頁、`/hang` 永不回應、
//! `/once` 只回應第一次；另有立刻關閉的 listener 當作連線被拒的 port。
mod errors;
