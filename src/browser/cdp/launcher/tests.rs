//! launcher 的單元測試：參數組合、暫存目錄、registry、trace 檔與版本解析（S1.3）。
//! 這批是新寫的單元測試（非移植），只涵蓋純函式與本機可驗證的行為；
//! 需要真的 spawn 程序與瀏覽器的測試在 `src/tests/browser_launch.rs`。
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::CHROMIUM_ARGS;
use super::USER_DATA_DIR_PLACEHOLDER;
use super::{
    DisconnectedState, TraceWriter, compose_browser_args, create_temp_dirs, kill_all_registered,
    parse_browser_version, register_process_group, registered_process_groups, resolve_executable,
    unregister_process_group,
};

/// 測試用的假 pgid：刻意取 i32 上限附近，不可能是機器上真實存在的 process group。
const FAKE_PGID_A: i32 = 2_147_483_646;
const FAKE_PGID_B: i32 = 2_147_483_645;

/// registry 是否登記著某個 pgid。
fn registered_contains(pgid: i32) -> bool {
    registered_process_groups().lock().unwrap().contains(&pgid)
}

/// 某個 pgid 在 registry 出現幾次。
fn registered_count(pgid: i32) -> usize {
    registered_process_groups()
        .lock()
        .unwrap()
        .iter()
        .filter(|&&registered| registered == pgid)
        .count()
}

#[test]
fn compose_args_replaces_user_data_dir_placeholder() {
    let raw = "--headless\n--user-data-dir=<tmp>\n--mute-audio\n";
    let args = compose_browser_args(raw, Path::new("/tmp/twiclaimer-profile-x"));
    assert_eq!(
        args,
        vec![
            "--headless".to_string(),
            "--user-data-dir=/tmp/twiclaimer-profile-x".to_string(),
            "--mute-audio".to_string(),
        ]
    );
}

#[test]
fn compose_args_preserves_order_and_verbatim_lines() {
    let raw = "--a\n--b=1,2\n--a\n--headless\n";
    let args = compose_browser_args(raw, Path::new("/p"));
    assert_eq!(
        args,
        vec![
            "--a".to_string(),
            "--b=1,2".to_string(),
            // 重複的行逐字保留，順序不變。
            "--a".to_string(),
            "--headless".to_string(),
        ]
    );
}

#[test]
fn compose_args_on_recorded_args_replaces_only_placeholder() {
    // 錄製檔的前置條件：恰好一個遮蔽佔位行。
    assert_eq!(
        CHROMIUM_ARGS
            .lines()
            .filter(|line| *line == USER_DATA_DIR_PLACEHOLDER)
            .count(),
        1
    );

    let args = compose_browser_args(CHROMIUM_ARGS, Path::new("/tmp/twiclaimer-profile-x"));
    // 逐字、依序：除了佔位行，其餘每一行都要原樣出現在輸出中。
    let expected: Vec<&str> = CHROMIUM_ARGS
        .lines()
        .filter(|line| *line != USER_DATA_DIR_PLACEHOLDER)
        .collect();
    let actual: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|arg| !arg.starts_with("--user-data-dir="))
        .collect();
    assert_eq!(actual, expected);

    // user-data-dir 恰好一個，且換成實際暫存目錄。
    let user_data_dirs: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|arg| arg.starts_with("--user-data-dir="))
        .collect();
    assert_eq!(
        user_data_dirs,
        vec!["--user-data-dir=/tmp/twiclaimer-profile-x"]
    );
    assert_eq!(args.len(), expected.len() + 1);
}

#[test]
fn compose_args_without_placeholder_keeps_lines_verbatim() {
    // 佔位不存在時不做任何替換：連看起來像 user-data-dir 的行都逐字保留。
    let raw = "--headless\n--user-data-dir=/recorded/real/path\n--mute-audio";
    let args = compose_browser_args(raw, Path::new("/tmp/ours"));
    assert_eq!(
        args,
        vec![
            "--headless".to_string(),
            "--user-data-dir=/recorded/real/path".to_string(),
            "--mute-audio".to_string(),
        ]
    );
}

#[test]
fn compose_args_skips_empty_lines() {
    let raw = "\n--headless\n\n--mute-audio\n\n";
    let args = compose_browser_args(raw, Path::new("/p"));
    assert_eq!(
        args,
        vec!["--headless".to_string(), "--mute-audio".to_string()]
    );
}

#[test]
fn create_temp_dirs_use_expected_prefixes() {
    let dirs = create_temp_dirs().unwrap();
    for (dir, prefix) in [
        (&dirs.profile, "twiclaimer-profile-"),
        (&dirs.artifacts, "twiclaimer-artifacts-"),
    ] {
        let name = dir.path().file_name().unwrap().to_string_lossy();
        assert!(name.starts_with(prefix), "{name:?} 應以 {prefix:?} 開頭");
        assert!(
            dir.path().is_dir(),
            "暫存目錄應已建立：{}",
            dir.path().display()
        );
    }
    // 兩個目錄彼此不同。
    assert_ne!(dirs.profile.path(), dirs.artifacts.path());
}

#[test]
fn temp_dirs_are_removed_on_drop() {
    let dirs = create_temp_dirs().unwrap();
    let profile_path = dirs.profile.path().to_path_buf();
    let artifacts_path = dirs.artifacts.path().to_path_buf();
    drop(dirs);
    assert!(!profile_path.exists(), "profile 目錄應在 drop 時刪除");
    assert!(!artifacts_path.exists(), "下載目錄應在 drop 時刪除");
}

#[test]
fn trace_writer_writes_launching_send_recv_lines() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("cdp.log");
    let mut writer = TraceWriter::new(Some(&trace_path)).unwrap();

    writer.write_launching(
        Path::new("/opt/headless-shell/chrome-headless-shell"),
        &["--headless".to_string(), "--mute-audio".to_string()],
    );
    writer.record_send("{\"id\":1}");
    writer.record_recv("{\"id\":1,\"result\":{}}");
    drop(writer);

    let content = fs::read_to_string(&trace_path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(
        lines,
        vec![
            "<launching> /opt/headless-shell/chrome-headless-shell --headless --mute-audio",
            "SEND ► {\"id\":1}",
            "◀ RECV {\"id\":1,\"result\":{}}",
        ]
    );
}

#[test]
fn trace_writer_disabled_is_noop() {
    let mut writer = TraceWriter::new(None).unwrap();
    assert!(matches!(writer, TraceWriter::Disabled));
    // 呼叫各種寫入都不應 panic。
    writer.write_launching(Path::new("/x"), &[]);
    writer.record_send("{}");
    writer.record_recv("{}");
}

#[test]
fn trace_writer_from_env_follows_environment() {
    let dir = tempfile::tempdir().unwrap();

    // 未設定：no-op。
    unsafe { std::env::remove_var("TWICLAIMER_CDP_TRACE") };
    assert!(matches!(TraceWriter::from_env(), TraceWriter::Disabled));

    // 有設定：開新檔（既有內容截斷）。
    let trace_path = dir.path().join("env.log");
    fs::write(&trace_path, b"stale").unwrap();
    unsafe { std::env::set_var("TWICLAIMER_CDP_TRACE", &trace_path) };
    let mut writer = TraceWriter::from_env();
    assert!(matches!(writer, TraceWriter::File(_)));
    writer.record_send("{\"id\":2}");
    drop(writer);
    assert_eq!(
        fs::read_to_string(&trace_path).unwrap(),
        "SEND ► {\"id\":2}\n"
    );

    // 開檔失敗（上層目錄不存在）：退化為 no-op，不讓啟動流程失敗。
    let missing = dir.path().join("nope").join("cdp.log");
    unsafe { std::env::set_var("TWICLAIMER_CDP_TRACE", &missing) };
    assert!(matches!(TraceWriter::from_env(), TraceWriter::Disabled));

    unsafe { std::env::remove_var("TWICLAIMER_CDP_TRACE") };
}

#[test]
fn parse_browser_version_strips_product_prefix() {
    assert_eq!(
        parse_browser_version("HeadlessChrome/151.0.7922.34"),
        "151.0.7922.34"
    );
    // 取第一個 `/` 之後。
    assert_eq!(parse_browser_version("A/B/C"), "B/C");
}

#[test]
fn parse_browser_version_returns_input_without_slash() {
    assert_eq!(parse_browser_version("Chrome"), "Chrome");
    assert_eq!(parse_browser_version(""), "");
}

#[test]
fn registry_register_and_unregister() {
    register_process_group(FAKE_PGID_A);
    register_process_group(FAKE_PGID_B);
    assert!(registered_contains(FAKE_PGID_A));
    assert!(registered_contains(FAKE_PGID_B));

    // 重複註冊只留一份。
    register_process_group(FAKE_PGID_A);
    assert_eq!(registered_count(FAKE_PGID_A), 1);

    unregister_process_group(FAKE_PGID_A);
    assert!(!registered_contains(FAKE_PGID_A));
    assert!(registered_contains(FAKE_PGID_B));

    unregister_process_group(FAKE_PGID_B);
    assert!(!registered_contains(FAKE_PGID_B));
}

#[test]
fn registry_kill_all_ignores_missing_groups() {
    // 假 pgid 必不存在：killpg 回 ESRCH，kill_all 不得 panic 也不得誤殺其他程序。
    register_process_group(FAKE_PGID_A);
    kill_all_registered();
    kill_all_registered();
    unregister_process_group(FAKE_PGID_A);
    kill_all_registered();
}

#[test]
fn registry_register_ignores_non_positive_pgid() {
    // 非正數 pgid 不得登記：killpg(0) 會打到自己的 process group。
    register_process_group(0);
    register_process_group(-7);
    assert!(!registered_contains(0));
    assert!(!registered_contains(-7));
}

#[test]
fn resolve_executable_follows_chrome_path() {
    // 未設定：Playwright 同款錯誤，路徑部分為空字串（含前綴尾端的空格）。
    unsafe { std::env::remove_var("CHROME_PATH") };
    let error = resolve_executable().unwrap_err();
    assert_eq!(
        error.message(),
        "browserType.launch: Executable doesn't exist at "
    );

    // 有設定：回傳該路徑。
    let expected = PathBuf::from("/opt/headless-shell/chrome-headless-shell");
    unsafe { std::env::set_var("CHROME_PATH", expected.as_os_str()) };
    assert_eq!(resolve_executable().unwrap(), expected);

    // 空字串視同未設定。
    unsafe { std::env::set_var("CHROME_PATH", "") };
    assert!(resolve_executable().is_err());

    unsafe { std::env::remove_var("CHROME_PATH") };
}

#[test]
fn disconnected_state_fires_listeners_once() {
    let state = Arc::new(DisconnectedState::default());
    let fired = Arc::new(AtomicUsize::new(0));
    let listener = Arc::clone(&fired);
    let _cancel = state.on_disconnected(move || {
        listener.fetch_add(1, Ordering::SeqCst);
    });

    assert!(!state.is_disconnected(), "一開始未斷線");
    state.mark_disconnected();
    state.mark_disconnected();
    assert_eq!(fired.load(Ordering::SeqCst), 1, "重複標記只觸發一次");
    assert!(state.is_disconnected());

    // 已斷線後才註冊的 listener 不觸發（Playwright EventEmitter 語意）。
    let late = Arc::new(AtomicUsize::new(0));
    let late_listener = Arc::clone(&late);
    let _late_cancel = state.on_disconnected(move || {
        late_listener.fetch_add(1, Ordering::SeqCst);
    });
    assert_eq!(late.load(Ordering::SeqCst), 0);
}

#[test]
fn disconnected_state_cancel_removes_listener() {
    let state = Arc::new(DisconnectedState::default());
    let fired = Arc::new(AtomicUsize::new(0));
    let listener = Arc::clone(&fired);
    let cancel = state.on_disconnected(move || {
        listener.fetch_add(1, Ordering::SeqCst);
    });

    cancel();
    state.mark_disconnected();
    assert_eq!(fired.load(Ordering::SeqCst), 0, "取消後不觸發");
}
