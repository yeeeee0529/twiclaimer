//! launch↔Connection 接線與瀏覽器生命週期的程序級測試（S1.3、G1）。
//!
//! 兩類：
//! 1. 假瀏覽器腳本（不需要 `CHROME_PATH`，正常 `cargo test` 就會跑）：從 fd 3 讀 CDP
//!    指令、對 fd 4 回應，涵蓋握手順序、180 秒逾時訊息、drop／取消的孤兒防線、
//!    disconnected 觸發一次。
//! 2. 真 headless shell 的 e2e（標 `#[ignore = "需要 CHROME_PATH"]`）：版本、
//!    錄製參數比對（G1）與不留殘留。
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::Value;

use crate::browser::cdp::connection::CdpError;
use crate::browser::cdp::launcher::{
    BrowserLauncher, CHROMIUM_ARGS, compose_browser_args, registered_process_groups,
};

/// 假瀏覽器程序結束與清理的輪詢上限（背景任務需要一點時間）。
const CLEANUP_DEADLINE: Duration = Duration::from_secs(3);

/// 假瀏覽器：回應握手指令、收到 `Browser.close` 就結束；收到的訊息記到 profile 目錄。
/// Shebang 用 bash：`read -d` 是 bash 擴充，Ubuntu 的 `/bin/sh`（dash）不支援，
/// 會讓腳本啟動即失敗（macOS 的 `/bin/sh` 是 bash，本機測不出）。
const RESPONDING_SCRIPT: &str = r#"#!/bin/bash
user_data_dir=
for arg in "$@"; do
  case "$arg" in --user-data-dir=*) user_data_dir=${arg#--user-data-dir=} ;; esac
done
echo $$ > "$user_data_dir/browser.pid"
while IFS= read -r -d '' msg <&3; do
  printf '%s\n' "$msg" >> "$user_data_dir/received.log"
  id=$(printf '%s' "$msg" | sed -n 's/^.*"id":\([0-9][0-9]*\).*$/\1/p')
  case "$msg" in
    *Browser.getVersion*) printf '{"id":%s,"result":{"product":"HeadlessChrome/151.0.7922.34"}}\0' "$id" >&4 ;;
    *Target.setAutoAttach*) printf '{"id":%s,"result":{}}\0' "$id" >&4 ;;
    *Browser.close*) exit 0 ;;
  esac
done
"#;

/// 假瀏覽器：回應握手，但**不理會** `Browser.close`（模擬關不掉的瀏覽器）。
/// Shebang 用 bash：理由同 `RESPONDING_SCRIPT`。
const STUBBORN_SCRIPT: &str = r#"#!/bin/bash
while IFS= read -r -d '' msg <&3; do
  id=$(printf '%s' "$msg" | sed -n 's/^.*"id":\([0-9][0-9]*\).*$/\1/p')
  case "$msg" in
    *Browser.getVersion*) printf '{"id":%s,"result":{"product":"HeadlessChrome/1.2.3.4"}}\0' "$id" >&4 ;;
    *Target.setAutoAttach*) printf '{"id":%s,"result":{}}\0' "$id" >&4 ;;
  esac
done
"#;

/// 假瀏覽器：什麼都不回應（模擬啟動後掛住的瀏覽器，觸發整體逾時）。
const HANGING_SCRIPT: &str = r#"#!/bin/sh
sleep 60
"#;

/// 在測試專屬的暫存目錄寫出可執行的假瀏覽器腳本。
fn write_script(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("fake-browser.sh");
    fs::write(&path, body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// process group 是否存活（`killpg(pgid, 0)`；pid == pgid）。
fn process_group_alive(pgid: i32) -> bool {
    nix::sys::signal::killpg(Pid::from_raw(pgid), None).is_ok()
}

/// 是否有 cmdline 含指定字樣的程序（`pgrep -f`；可涵蓋 process group 內的子程序）。
fn has_process_matching(pattern: &str) -> bool {
    Command::new("pgrep")
        .arg("-f")
        .arg(pattern)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// 輪詢直到條件成立或超時。
async fn eventually(timeout: Duration, check: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if check() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// registry 是否登記著某個 pgid。
fn registered_contains(pgid: i32) -> bool {
    registered_process_groups().lock().unwrap().contains(&pgid)
}

/// 驗證收尾完成：程序群組不存在、暫存目錄已刪、registry 已解除。
async fn assert_fully_cleaned_up(profile: &Path, artifacts: &Path, pgid: i32) {
    assert!(
        eventually(CLEANUP_DEADLINE, || !process_group_alive(pgid)).await,
        "程序群組 {pgid} 應已結束"
    );
    assert!(
        eventually(CLEANUP_DEADLINE, || !profile.exists()
            && !artifacts.exists())
        .await,
        "暫存目錄應已刪除"
    );
    assert!(
        eventually(CLEANUP_DEADLINE, || !registered_contains(pgid)).await,
        "registry 應已解除登記"
    );
    // 整個 process group（含 renderer 等子程序）都不該殘留。
    let profile_name = profile.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        eventually(CLEANUP_DEADLINE, || !has_process_matching(&profile_name)).await,
        "不該殘留使用此 profile 的程序"
    );
}

// ===== 假瀏覽器（不需 CHROME_PATH）=====

#[tokio::test]
async fn launch_completes_handshake_in_recorded_order() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), RESPONDING_SCRIPT);

    let browser = BrowserLauncher::new()
        .launch_with(script, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(browser.get_version(), "151.0.7922.34");
    assert!(!browser.is_disconnected());

    // 錄製順序（§7.3.1）：先 `Browser.getVersion` 再 `Target.setAutoAttach`。
    let (profile, artifacts) = browser.temp_dir_paths();
    let profile = profile.to_path_buf();
    let artifacts = artifacts.to_path_buf();
    let received = fs::read_to_string(profile.join("received.log")).unwrap();
    let methods: Vec<String> = received
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .unwrap()
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert_eq!(methods, vec!["Browser.getVersion", "Target.setAutoAttach"]);

    let pgid = browser.process_id().unwrap() as i32;
    browser.close().await;
    // close() 返回時收尾已完成，不需輪詢程序與目錄；registry 也要解除。
    assert!(!process_group_alive(pgid));
    assert!(!profile.exists(), "profile 目錄應已刪除");
    assert!(!artifacts.exists(), "下載目錄應已刪除");
    assert!(!registered_contains(pgid), "registry 應已解除登記");
}

#[tokio::test]
async fn launch_timeout_reports_playwright_message_and_kills_browser() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), HANGING_SCRIPT);

    let error = match BrowserLauncher::new()
        .launch_with(script, Duration::from_millis(250))
        .await
    {
        Ok(_browser) => panic!("在 250ms 內不該完成握手"),
        Err(error) => error,
    };
    assert_eq!(
        error.message(),
        "browserType.launch: Timeout 250ms exceeded."
    );

    // 逾時防線：不留孤兒（正式碼用 180 秒，訊息由實際逾時產生，同格式）。
    let dir_str = dir.path().display().to_string();
    assert!(
        eventually(CLEANUP_DEADLINE, || !has_process_matching(&dir_str)).await,
        "逾時後不得殘留假瀏覽器程序"
    );
    // 暫存目錄的刪除與 registry 解除走同一個監看任務（其他測試已驗證），此處不重複。
}

#[tokio::test]
async fn dropping_the_handle_kills_the_browser() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), RESPONDING_SCRIPT);

    let browser = BrowserLauncher::new()
        .launch_with(script, Duration::from_secs(10))
        .await
        .unwrap();
    let pgid = browser.process_id().unwrap() as i32;
    let (profile, artifacts) = browser.temp_dir_paths();
    let profile = profile.to_path_buf();
    let artifacts = artifacts.to_path_buf();
    // 不呼叫 close()，直接 drop：應 SIGKILL 整個 process group 並背景清理。
    drop(browser);

    assert_fully_cleaned_up(&profile, &artifacts, pgid).await;
}

#[tokio::test]
async fn cancelling_close_kills_a_browser_that_ignores_browser_close() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), STUBBORN_SCRIPT);

    let browser = BrowserLauncher::new()
        .launch_with(script, Duration::from_secs(10))
        .await
        .unwrap();
    let pgid = browser.process_id().unwrap() as i32;
    let (profile, artifacts) = browser.temp_dir_paths();
    let profile = profile.to_path_buf();
    let artifacts = artifacts.to_path_buf();

    // close() 的 future 被取消（呼叫端逾時的等價情境）：未來被 drop 後把手也一併
    // 釋放 → killpg。瀏覽器不理會 Browser.close，只能靠 killpg 收掉。
    let close_future = browser.close();
    drop(close_future);
    drop(browser);

    assert_fully_cleaned_up(&profile, &artifacts, pgid).await;
}

#[tokio::test]
async fn on_disconnected_fires_once_when_browser_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), RESPONDING_SCRIPT);

    let browser = BrowserLauncher::new()
        .launch_with(script, Duration::from_secs(10))
        .await
        .unwrap();

    let fired = Arc::new(AtomicUsize::new(0));
    let listener = Arc::clone(&fired);
    let _cancel = browser.on_disconnected(move || {
        listener.fetch_add(1, Ordering::SeqCst);
    });

    // 模擬瀏覽器 crash：SIGKILL 整個 process group（§7.3.1：程序結束時觸發一次）。
    let pgid = browser.process_id().unwrap() as i32;
    killpg(Pid::from_raw(pgid), Signal::SIGKILL).unwrap();

    assert!(
        eventually(Duration::from_secs(2), || fired.load(Ordering::SeqCst) == 1).await,
        "disconnected 應在程序結束時觸發一次"
    );
    assert_eq!(fired.load(Ordering::SeqCst), 1, "只觸發一次");

    // 之後所有呼叫回 `Target page, context or browser has been closed`（§7.3.1）。
    let reply = browser
        .send_command(None, "Browser.getVersion", serde_json::json!({}))
        .await;
    assert_eq!(reply, Err(CdpError::TargetClosed));

    // 斷線後 close() 仍要能完成（不再寫 Browser.close）。
    browser.close().await;
}

// ===== 真 headless shell 的 e2e（G1）=====

/// 取得程序的完整命令列（macOS／Linux 的 `ps -o args=`）。
fn process_command_line(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-ww", "-o", "args="])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    Some(text.trim_end().to_string())
}

// 原測試：default engine 使用 Chromium 並回傳共用 adapter
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn launch_with_real_headless_shell_reports_version_and_leaves_nothing_behind() {
    let browser = BrowserLauncher::new().launch().await.unwrap();
    assert_eq!(
        browser.get_version(),
        "151.0.7922.34",
        "G1：get_version() 應為下載的 headless shell 版本"
    );

    let pid = browser.process_id().unwrap();
    let (profile, artifacts) = browser.temp_dir_paths();
    let profile = profile.to_path_buf();
    let artifacts = artifacts.to_path_buf();
    assert!(profile.is_dir(), "profile 目錄應已建立");
    assert!(artifacts.is_dir(), "下載目錄應已建立");

    browser.close().await;
    assert_fully_cleaned_up(&profile, &artifacts, pid as i32).await;
}

// 原測試：chromium engine 使用 Chromium 並回傳同一 adapter
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn launch_uses_the_recorded_chromium_args_verbatim() {
    let browser = BrowserLauncher::new().launch().await.unwrap();

    // G1：實際命令列與 `doc/spec/chromium-args.txt` 逐行相同（--user-data-dir 換成自己的暫存目錄）。
    let pid = browser.process_id().unwrap();
    let command_line = process_command_line(pid).unwrap();
    let mut tokens = command_line.split_whitespace();
    let executable = tokens.next().unwrap().to_string();
    let actual_args: Vec<String> = tokens.map(str::to_string).collect();

    // 第一個 token 是執行檔（CHROME_PATH），其餘是參數。
    assert_eq!(
        executable,
        std::env::var("CHROME_PATH").unwrap(),
        "應以 CHROME_PATH 指定的執行檔啟動"
    );
    let user_data_dir = actual_args
        .iter()
        .find_map(|arg| arg.strip_prefix("--user-data-dir="))
        .unwrap_or_else(|| panic!("應有恰好一個 --user-data-dir：{actual_args:?}"))
        .to_string();
    let expected_args = compose_browser_args(CHROMIUM_ARGS, Path::new(&user_data_dir));
    assert_eq!(actual_args, expected_args, "參數應逐字、依序相同");

    let (profile, artifacts) = browser.temp_dir_paths();
    let profile = profile.to_path_buf();
    let artifacts = artifacts.to_path_buf();
    browser.close().await;
    assert_fully_cleaned_up(&profile, &artifacts, pid as i32).await;
}

/// 新增 e2e（G1：drop 不留孤兒）：真瀏覽器 drop 後整個 process group 結束。
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn dropping_a_real_browser_leaves_no_processes_behind() {
    let browser = BrowserLauncher::new().launch().await.unwrap();
    let pgid = browser.process_id().unwrap() as i32;
    let (profile, artifacts) = browser.temp_dir_paths();
    let profile = profile.to_path_buf();
    let artifacts = artifacts.to_path_buf();
    drop(browser);

    assert_fully_cleaned_up(&profile, &artifacts, pgid).await;
}
