//! `twiclaimer probe` 子命令（§7.3.7）：開發用重跑器，流程與 `tools/ref/pw-trace.mjs`
//! 完全相同，供 `scripts/compare-traces.sh`（G2、G3）比對錄製結果。
//!
//! 流程：launch → newContext（storageState、viewport 1280×720）→ 資源阻擋
//! （`TRACE_BLOCKING`，三個旗標全開或全關）→ 聊天室封鎖（`TRACE_DISABLE_CHAT != "0"`）→
//! newPage → 診斷 listener（stdout JSON Lines）→ `goto`（`domcontentloaded`、逾時
//! 30 000 ms）→ 等待（http 2 000 ms、file 500 ms）→ `--hold` 時多停留並輸出 RSS →
//! 截圖（`--screenshot` 寫檔）→ 頁面關閉 → context 關閉 → 瀏覽器關閉。
//!
//! 診斷事件的欄位照 §7.5（S1.4 版）：`url` 以 `sanitizeBrowserUrl` +
//! `joinSanitizedUrl` 消毒（port、query、fragment 全丟）；`failureText`、console
//! 文字與 GraphQL operation 名稱已在事件泵內 redact 與截斷；選用欄位（
//! `failureText`、`graphQlOperationNames`、`sourceUrl`）只在有值時輸出。精修與
//! listener 單元測試在 S1.6 進行。
//!
//! CDP 紀錄沿用 launcher 的 `TWICLAIMER_CDP_TRACE`（§7.3.7）；未設定時無額外成本。
//!
//! 以下 API 的呼叫端在 bin crate（`src/main.rs` 以 `#[path]` 掛入模組樹）；
//! lib crate 的測試建構不會用到，allow 只為此存在。
#![allow(dead_code)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use super::context::{BrowserAdapter, DriverError, ResourceBlockingOptions};
use super::launcher::BrowserLauncher;
use super::page::{PageDiagnostic, WaitUntil};

/// 導覽逾時（pw-trace 的 `page.setDefaultNavigationTimeout(30_000)`）。
const NAVIGATION_TIMEOUT_MS: u64 = 30_000;

/// 錄製時的 viewport（pw-trace 的 `viewport: { width: 1280, height: 720 }`）。
const VIEWPORT: (u32, u32) = (1280, 720);

/// mock 頁面目錄（§7.3.7：`testdata/mock-pages` 的檔名）。
const MOCK_PAGES_DIR: &str = "testdata/mock-pages";

/// probe 的參數（CLI 解析結果）。
struct ProbeOptions {
    /// storageState 檔路徑。
    storage_state: PathBuf,
    /// 頁面：`testdata/mock-pages` 的檔名或 http(s) URL。
    page: String,
    /// `--hold`：頁面多停留的秒數（G8、G9 用）。
    hold: Option<u64>,
    /// `--screenshot`：截圖輸出檔（G4 的人工確認用）。
    screenshot: Option<PathBuf>,
}

/// probe 的進入點：解析參數、跑完整流程；錯誤以訊息字串回給呼叫端（exit 1）。
pub(crate) async fn run(args: &[String]) -> Result<(), String> {
    let options = parse_args(args)?;
    let target_url = resolve_page(&options.page)?;

    // launch → context → 資源阻擋 → 聊天室封鎖 → newPage（照 pw-trace 的順序）。
    let launched = BrowserLauncher::new()
        .launch()
        .await
        .map_err(|error| error.message().to_string())?;
    let browser = BrowserAdapter::new(launched);
    let result = run_probe_flow(&browser, &options, &target_url).await;
    // 流程失敗也照 pw-trace 收尾：關瀏覽器（失敗路徑多半已斷線，close 為 no-op）。
    browser.close().await;
    result
}

/// probe 的主流程（launch 之後的部分；錯誤照舊版逐層上拋）。
async fn run_probe_flow(
    browser: &BrowserAdapter,
    options: &ProbeOptions,
    target_url: &str,
) -> Result<(), String> {
    let context = browser
        .new_context(Some(options.storage_state.as_path()), VIEWPORT)
        .await
        .map_err(driver_message)?;

    let blocking = trace_blocking_enabled();
    context
        .configure_resource_blocking(ResourceBlockingOptions {
            block_images: blocking,
            block_fonts: blocking,
            block_known_tracking: blocking,
        })
        .await
        .map_err(driver_message)?;
    context
        .configure_chat_blocking(trace_chat_blocking_enabled())
        .await
        .map_err(driver_message)?;

    let page = context.new_page().await.map_err(driver_message)?;

    // 診斷 listener：stdout JSON Lines（§7.5 的 CDP 來源翻譯；S1.6 精修）。
    let on_diagnostic = page.shared.on_diagnostic(|diagnostic| {
        emit_line(&diagnostic_json(diagnostic));
    });
    let on_popup = page.shared.on_popup(|popup| {
        emit_line(&json!({ "kind": "popup" }));
        // 照 pw-trace：popup 初始化完成即關閉（`void popup.close()`）。
        let popup = std::sync::Arc::clone(popup);
        tokio::spawn(async move {
            let _ = popup.close().await;
        });
    });

    page.shared
        .set_default_navigation_timeout(NAVIGATION_TIMEOUT_MS);
    page.shared
        .goto(target_url, WaitUntil::DomContentLoaded)
        .await
        .map_err(driver_message)?;

    // 等待：pw-trace 的 `waitForTimeout(http ? 2000 : 500)`。
    let settle = if target_url.starts_with("http") {
        Duration::from_millis(2_000)
    } else {
        Duration::from_millis(500)
    };
    tokio::time::sleep(settle).await;
    if let Some(seconds) = options.hold {
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        if let Some(bytes) = current_rss_bytes() {
            // G9 的 RSS 取樣（`/proc/self/statm`；非 Linux 沒有該檔，不輸出）。
            emit_line(&json!({ "kind": "rss", "bytes": bytes }));
        }
    }

    let png = page.shared.screenshot().await.map_err(driver_message)?;
    if let Some(path) = &options.screenshot {
        std::fs::write(path, png)
            .map_err(|error| format!("cannot write screenshot {}: {error}", path.display()))?;
    }

    // 收尾照 pw-trace：adapter.close → context.close → browser.close。
    page.shared.close().await.map_err(driver_message)?;
    drop((on_diagnostic, on_popup));
    context.close().await.map_err(driver_message)?;
    Ok(())
}

/// 解析 CLI 參數：兩個位置參數（storageState、頁面）與 `--hold`／`--screenshot`。
fn parse_args(args: &[String]) -> Result<ProbeOptions, String> {
    let mut positional: Vec<&String> = Vec::new();
    let mut hold = None;
    let mut screenshot = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--hold" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "probe: --hold requires a value in seconds".to_string())?;
                hold = Some(value.parse::<u64>().map_err(|_| {
                    format!("probe: --hold expects an integer number of seconds, got {value}")
                })?);
            }
            "--screenshot" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "probe: --screenshot requires a file path".to_string())?;
                screenshot = Some(PathBuf::from(value));
            }
            other if other.starts_with("--") => {
                return Err(format!("probe: unknown option {other}"));
            }
            _ => positional.push(&args[index]),
        }
        index += 1;
    }
    let [storage_state, page] = positional.as_slice() else {
        return Err(format!(
            "probe: expected <storage-state.json> <page>, got {} positional arguments\nusage: twiclaimer probe <storage-state.json> <page> [--hold <seconds>] [--screenshot <file>]",
            positional.len()
        ));
    };
    Ok(ProbeOptions {
        storage_state: PathBuf::from(storage_state),
        page: page.to_string(),
        hold,
        screenshot,
    })
}

/// 解析頁面參數：http(s) URL 照原樣；其餘為 `testdata/mock-pages` 的檔名（file URL）。
fn resolve_page(page: &str) -> Result<String, String> {
    if page.starts_with("http://") || page.starts_with("https://") {
        return Ok(page.to_string());
    }
    let path = Path::new(MOCK_PAGES_DIR).join(page);
    // from_file_path 需要絕對路徑（相對路徑以工作目錄為基準）。
    let path = std::fs::canonicalize(&path)
        .map_err(|error| format!("probe: mock page not found: {} ({error})", path.display()))?;
    url::Url::from_file_path(&path)
        .map(|url| url.to_string())
        .map_err(|_| format!("probe: cannot convert {} to a file URL", path.display()))
}

/// `TRACE_BLOCKING`（pw-trace 語意）：`1` 時資源阻擋三個旗標全開。
fn trace_blocking_enabled() -> bool {
    std::env::var("TRACE_BLOCKING").is_ok_and(|value| value == "1")
}

/// `TRACE_DISABLE_CHAT`（pw-trace 語意）：不等於 `0`（含未設定）時啟用聊天室封鎖。
fn trace_chat_blocking_enabled() -> bool {
    std::env::var("TRACE_DISABLE_CHAT").is_ok_and(|value| value != "0")
}

/// 目前程序的 RSS 位元組（`/proc/self/statm` 第二欄 × 頁面大小；非 Linux 回 None）。
fn current_rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages = statm.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    // 安全性：sysconf 只讀系統常數，無副作用。
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return None;
    }
    Some(resident_pages * page_size as u64)
}

/// 輸出一行 JSON 到 stdout（listener 內的錯誤一律吞掉，§7.3.5）。
fn emit_line(value: &Value) {
    let _ = writeln!(std::io::stdout().lock(), "{value}");
}

/// 診斷事件 → stdout 的 JSON（欄位照 §7.5；`url` 消毒、選用欄位有值才輸出）。
fn diagnostic_json(diagnostic: &PageDiagnostic) -> Value {
    match diagnostic {
        PageDiagnostic::RequestFailed {
            url,
            method,
            resource_type,
            failure_text,
            graphql_operation_names,
        } => {
            let mut event = json!({
                "kind": "requestfailed",
                "url": sanitized_url(url),
                "method": method,
                "resourceType": resource_type,
            });
            if let Some(failure_text) = failure_text {
                event["failureText"] = json!(failure_text);
            }
            if !graphql_operation_names.is_empty() {
                event["graphQlOperationNames"] = json!(graphql_operation_names);
            }
            event
        }
        PageDiagnostic::Response {
            url,
            method,
            resource_type,
            status,
            status_text,
            graphql_operation_names,
        } => {
            let mut event = json!({
                "kind": "response",
                "url": sanitized_url(url),
                "method": method,
                "resourceType": resource_type,
                "status": status,
                "statusText": status_text,
            });
            if !graphql_operation_names.is_empty() {
                event["graphQlOperationNames"] = json!(graphql_operation_names);
            }
            event
        }
        PageDiagnostic::Console {
            r#type,
            text,
            source_url,
            line_number,
            column_number,
        } => {
            let mut event = json!({
                "kind": "console",
                "type": r#type,
                "text": text,
            });
            if let Some(source_url) = source_url {
                event["sourceUrl"] = json!(source_url);
            }
            event["lineNumber"] = json!(line_number);
            event["columnNumber"] = json!(column_number);
            event
        }
    }
}

/// 診斷的 `url` 消毒（照舊版 `sanitizeUrlForDiagnostic`）。
fn sanitized_url(raw_url: &str) -> String {
    super::page::join_sanitized_url(&super::page::sanitize_browser_url(raw_url))
}

/// 驅動層錯誤 → probe 的訊息字串（`name: message`；message 已含內容）。
fn driver_message(error: DriverError) -> String {
    format!("probe: {}: {}", error.name, error.message)
}
