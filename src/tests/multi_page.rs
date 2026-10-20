//! 多頁面、popup 自動關閉、頁面 crash 隔離與瀏覽器 SIGKILL 的 e2e（S1.8、G6）。
//!
//! 四項（計畫 §8 的 S1.8）：
//! 1. 三頁並發運作：`new_page()` ×3 → 各自導覽 mock 頁（file://），URL、console
//!    診斷歸屬與 `screenshot()` 各自正確，全部 close 後無殘留。
//! 2. popup 自動關閉：照 pw-trace 的語意（§7.3.4–§7.3.5），opener 的 `on_popup`
//!    收到後由 listener 呼叫 popup 的 `close()`；opener 頁面不受影響。
//! 3. 一頁 crash 不影響其他頁面：注入方式用 `goto("chrome://crash")`（計畫 §7.4
//!    pw-errors 錄製採用的方式；`Page.crash` 指令需要頁面 session 直送，而現有
//!    API 未暴露 session id）；`on_crash`／`is_crashed()` 生效，B／C 照常運作，
//!    瀏覽器程序還活著。
//! 4. SIGKILL 瀏覽器主程序：`on_disconnected` 在 1 秒內觸發、頁面標記關閉、
//!    之後的操作回已關閉錯誤，且無孤兒程序殘留。
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use serde_json::Value;

use crate::browser::cdp::context::BrowserAdapter;
use crate::browser::cdp::launcher::{BrowserLauncher, registered_process_groups};
use crate::browser::cdp::page::{PageDiagnostic, PageShared, WaitUntil};

/// 事件到齊的輪詢上限（本機檔案頁，事件應即時到達）。
const POLL_DEADLINE: Duration = Duration::from_secs(5);

/// 收尾完成的輪詢上限（背景任務需要一點時間；照 browser_launch 的上限）。
const CLEANUP_DEADLINE: Duration = Duration::from_secs(3);

/// PNG 檔頭（`screenshot()` 應回真正的 PNG）。
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

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

/// 是否有 cmdline 含指定字樣的程序（`pgrep -f`）。
fn has_process_matching(pattern: &str) -> bool {
    Command::new("pgrep")
        .arg("-f")
        .arg(pattern)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// process group 是否存活（`killpg(pgid, 0)`）。
fn process_group_alive(pgid: i32) -> bool {
    killpg(Pid::from_raw(pgid), None).is_ok()
}

/// registry 是否登記著某個 pgid。
fn registered_contains(pgid: i32) -> bool {
    registered_process_groups().lock().unwrap().contains(&pgid)
}

/// mock 頁的 file:// URL（`testdata/mock-pages` 內的檔名）。
fn mock_file_url(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata/mock-pages")
        .join(name);
    url::Url::from_file_path(&path)
        .unwrap()
        .as_str()
        .to_string()
}

/// 已 launch 的 e2e 瀏覽器：adapter 與收尾驗證所需的程序／目錄資訊。
struct E2eBrowser {
    adapter: Arc<BrowserAdapter>,
    /// 主程序 pid（== process group id；launcher 的 `process_id()`）。
    pgid: u32,
    profile: PathBuf,
    artifacts: PathBuf,
}

impl E2eBrowser {
    /// profile 目錄的檔名（孤兒檢查的 `pgrep -f` pattern；每次 launch 唯一）。
    fn profile_name(&self) -> String {
        self.profile
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    /// 驗證收尾完成：程序群組不存在、暫存目錄已刪、registry 已解除、無殘留程序。
    async fn assert_cleaned_up(&self) {
        let pgid = self.pgid as i32;
        assert!(
            eventually(CLEANUP_DEADLINE, || !process_group_alive(pgid)).await,
            "程序群組 {pgid} 應已結束"
        );
        assert!(
            eventually(CLEANUP_DEADLINE, || !self.profile.exists()
                && !self.artifacts.exists())
            .await,
            "暫存目錄應已刪除"
        );
        assert!(
            eventually(CLEANUP_DEADLINE, || !registered_contains(pgid)).await,
            "registry 應已解除登記"
        );
        let profile_name = self.profile_name();
        assert!(
            eventually(CLEANUP_DEADLINE, || !has_process_matching(&profile_name)).await,
            "不該殘留使用此 profile 的程序"
        );
    }
}

/// 啟動真 headless shell 並包成 adapter；回傳收尾驗證所需的資訊。
async fn launch_e2e_browser() -> E2eBrowser {
    let launched = BrowserLauncher::new().launch().await.unwrap();
    let pgid = launched.process_id().unwrap();
    let (profile, artifacts) = launched.temp_dir_paths();
    let profile = profile.to_path_buf();
    let artifacts = artifacts.to_path_buf();
    let adapter = BrowserAdapter::new(launched);
    E2eBrowser {
        adapter,
        pgid,
        profile,
        artifacts,
    }
}

/// 登記一個頁面的 console 診斷 sink（只收 Console 事件）。
fn console_sink(page: &Arc<PageShared>) -> Arc<Mutex<Vec<PageDiagnostic>>> {
    let sink = Arc::new(Mutex::new(Vec::new()));
    let listener = Arc::clone(&sink);
    let _cancel = page.on_diagnostic(move |diagnostic| {
        if matches!(diagnostic, PageDiagnostic::Console { .. }) {
            listener.lock().unwrap().push(diagnostic.clone());
        }
    });
    sink
}

/// sink 內是否已有含指定文字的 console 診斷。
fn console_marker_seen(sink: &Mutex<Vec<PageDiagnostic>>, marker: &str) -> bool {
    sink.lock()
        .unwrap()
        .iter()
        .any(|diagnostic| matches!(diagnostic, PageDiagnostic::Console { text, .. } if text.contains(marker)))
}

/// 登記一個頁面的 crash 計數。
fn crash_counter(page: &Arc<PageShared>) -> Arc<AtomicUsize> {
    let counter = Arc::new(AtomicUsize::new(0));
    let listener = Arc::clone(&counter);
    let _cancel = page.on_crash(move || {
        listener.fetch_add(1, Ordering::SeqCst);
    });
    counter
}

// 原測試（S1.8 第一項）：三個頁面同時運作，URL、診斷與截圖各自正確
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn three_pages_run_concurrently_with_isolated_state_and_diagnostics() {
    let e2e = launch_e2e_browser().await;
    let context = e2e.adapter.new_context(None, (1280, 720)).await.unwrap();

    // 三頁並發建立（§7.3.4：各自 createTarget → auto-attach 初始化完成才返回）。
    let (page_a, page_b, page_c) =
        tokio::join!(context.new_page(), context.new_page(), context.new_page());
    let page_a = page_a.unwrap().shared;
    let page_b = page_b.unwrap().shared;
    let page_c = page_c.unwrap().shared;

    // console 診斷歸屬：sink 各自登記在自己的頁面（§7.3.5 的 listener 派送）。
    let console_a = console_sink(&page_a);
    let console_b = console_sink(&page_b);
    let console_c = console_sink(&page_c);

    let url_a = mock_file_url("multi-console-a.html");
    let url_b = mock_file_url("multi-console-b.html");
    let url_c = mock_file_url("multi-console-c.html");
    let (result_a, result_b, result_c) = tokio::join!(
        page_a.goto(&url_a, WaitUntil::DomContentLoaded),
        page_b.goto(&url_b, WaitUntil::DomContentLoaded),
        page_c.goto(&url_c, WaitUntil::DomContentLoaded),
    );
    result_a.unwrap();
    result_b.unwrap();
    result_c.unwrap();

    // URL 各自正確（導覽 commit 後的 file:// URL）。
    assert_eq!(page_a.url(), url_a);
    assert_eq!(page_b.url(), url_b);
    assert_eq!(page_c.url(), url_c);

    // console 診斷歸屬正確：各頁只收到自己的 marker（到齊後再比對缺席）。
    assert!(
        eventually(POLL_DEADLINE, || console_marker_seen(
            &console_a,
            "multi-console-marker-a"
        ))
        .await,
        "頁 A 應收到自己的 console 診斷"
    );
    assert!(
        eventually(POLL_DEADLINE, || console_marker_seen(
            &console_b,
            "multi-console-marker-b"
        ))
        .await,
        "頁 B 應收到自己的 console 診斷"
    );
    assert!(
        eventually(POLL_DEADLINE, || console_marker_seen(
            &console_c,
            "multi-console-marker-c"
        ))
        .await,
        "頁 C 應收到自己的 console 診斷"
    );
    assert!(
        !console_marker_seen(&console_a, "multi-console-marker-b")
            && !console_marker_seen(&console_a, "multi-console-marker-c"),
        "頁 A 不得收到其他頁的 console 診斷：{:?}",
        console_a.lock().unwrap()
    );
    assert!(
        !console_marker_seen(&console_b, "multi-console-marker-a")
            && !console_marker_seen(&console_b, "multi-console-marker-c"),
        "頁 B 不得收到其他頁的 console 診斷：{:?}",
        console_b.lock().unwrap()
    );
    assert!(
        !console_marker_seen(&console_c, "multi-console-marker-a")
            && !console_marker_seen(&console_c, "multi-console-marker-b"),
        "頁 C 不得收到其他頁的 console 診斷：{:?}",
        console_c.lock().unwrap()
    );

    // 截圖各自產出 PNG（viewport 範圍）。
    for (name, page) in [("A", &page_a), ("B", &page_b), ("C", &page_c)] {
        let png = page.screenshot().await.unwrap();
        let png_len = png.len();
        assert!(
            png.starts_with(PNG_MAGIC) && png_len > 100,
            "頁 {name} 的截圖應是非空 PNG，實得 {png_len} bytes"
        );
    }

    // 全部 close() 乾淨、無殘留程序。
    page_a.close().await.unwrap();
    page_b.close().await.unwrap();
    page_c.close().await.unwrap();
    assert!(page_a.is_closed() && page_b.is_closed() && page_c.is_closed());
    context.close().await.unwrap();
    e2e.adapter.close().await;
    e2e.assert_cleaned_up().await;
}

// 原測試（S1.8 第二項）：popup 自動關閉——on_popup 收到後由 listener 關閉
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn popup_received_via_on_popup_is_closed_and_opener_keeps_working() {
    let e2e = launch_e2e_browser().await;
    let context = e2e.adapter.new_context(None, (1280, 720)).await.unwrap();
    let opener = context.new_page().await.unwrap().shared;

    // 照 pw-trace 的 listener：收到 popup 即 `popup.close()`（§7.3.4 的語意）。
    let popups = Arc::new(Mutex::new(Vec::<Arc<PageShared>>::new()));
    let sink = Arc::clone(&popups);
    let _cancel = opener.on_popup(move |popup| {
        sink.lock().unwrap().push(Arc::clone(popup));
        let popup = Arc::clone(popup);
        tokio::spawn(async move {
            let _ = popup.close().await;
        });
    });

    let url = mock_file_url("multi-popup.html");
    opener
        .goto(&url, WaitUntil::DomContentLoaded)
        .await
        .unwrap();

    // popup 事件到齊（window.open 在載入後 150ms；on_popup 在 popup 初始化完成時觸發）。
    assert!(
        eventually(POLL_DEADLINE, || !popups.lock().unwrap().is_empty()).await,
        "opener 應收到 on_popup"
    );
    let popup = Arc::clone(&popups.lock().unwrap()[0]);
    assert_eq!(
        popup.url(),
        "about:blank",
        "popup 應是 about:blank 的新頁面"
    );

    // listener 關閉之後 popup 頁面應關閉。
    assert!(
        eventually(POLL_DEADLINE, || popup.is_closed()).await,
        "popup 應被 listener 關閉"
    );

    // opener 頁面正常：未關閉、URL 不變、evaluate 可用。
    assert!(!opener.is_closed(), "opener 頁面不得被關閉");
    assert_eq!(opener.url(), url);
    let answer = opener.evaluate("1 + 1".to_string()).await.unwrap();
    assert_eq!(
        answer.get("value").and_then(Value::as_f64),
        Some(2.0),
        "opener 的 evaluate 應正常運作：{answer:?}"
    );

    opener.close().await.unwrap();
    context.close().await.unwrap();
    e2e.adapter.close().await;
    e2e.assert_cleaned_up().await;
}

// 原測試（S1.8 第三項）：一個頁面 crash 不影響其他頁面
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn page_crash_is_isolated_from_other_pages() {
    let e2e = launch_e2e_browser().await;
    let context = e2e.adapter.new_context(None, (1280, 720)).await.unwrap();
    let (page_a, page_b, page_c) =
        tokio::join!(context.new_page(), context.new_page(), context.new_page());
    let page_a = page_a.unwrap().shared;
    let page_b = page_b.unwrap().shared;
    let page_c = page_c.unwrap().shared;

    let url_a = mock_file_url("multi-console-a.html");
    let url_b = mock_file_url("multi-console-b.html");
    let url_c = mock_file_url("multi-console-c.html");
    page_a
        .goto(&url_a, WaitUntil::DomContentLoaded)
        .await
        .unwrap();
    page_b
        .goto(&url_b, WaitUntil::DomContentLoaded)
        .await
        .unwrap();
    page_c
        .goto(&url_c, WaitUntil::DomContentLoaded)
        .await
        .unwrap();

    // crash 計數：三頁都先登記，之後斷言只有 A 觸發。
    let crashed_a = crash_counter(&page_a);
    let crashed_b = crash_counter(&page_b);
    let crashed_c = crash_counter(&page_c);

    // 注入 crash：`goto("chrome://crash")`（計畫 §7.4 pw-errors 錄製的方式；
    // `Page.crash` 需要頁面 session 直送而現有 API 未暴露 session id）。
    let crash_result = page_a
        .goto("chrome://crash", WaitUntil::DomContentLoaded)
        .await;
    assert!(crash_result.is_err(), "chrome://crash 的導覽應失敗");

    // on_crash 觸發一次、is_crashed() 為真。
    assert!(
        eventually(POLL_DEADLINE, || crashed_a.load(Ordering::SeqCst) == 1).await,
        "頁 A 應觸發 on_crash"
    );
    assert_eq!(crashed_a.load(Ordering::SeqCst), 1, "on_crash 只觸發一次");
    assert!(page_a.is_crashed(), "is_crashed() 應為真");

    // A 的後續操作回 `Target crashed` 類錯誤（10 秒防線：不得卡死）。
    //
    // 觀察過的行為（CDP trace）：crashed 頁 session 的 `Runtime.evaluate` 永遠等不到
    // 回應（crashed renderer 無人處理；Playwright 在 session 層本地擋成
    // `Target crashed `），而 `Page.navigate` 由 browser process 處理仍會回應。
    // `goto` 的等待入口先檢查 crashed 旗標（§7.3.5 的 `lifecycle_stop_error`），
    // 因此以 goto 驗證「後續操作回 Target crashed」。evaluate／screenshot 的
    // 卡死問題記入回報（需要 session 層的 crashed 短線路）。
    let post_crash = tokio::time::timeout(
        Duration::from_secs(10),
        page_a.goto("about:blank", WaitUntil::DomContentLoaded),
    )
    .await;
    let error = post_crash
        .expect("crashed 頁的 goto 不得卡超過 10 秒")
        .expect_err("crashed 頁的 goto 應失敗");
    assert!(
        error.message.contains("Target crashed"),
        "錯誤應為 Target crashed 類：{} / {}",
        error.name,
        error.message
    );

    // B／C 不受影響：沒有 crash、URL 可讀、操作正常。
    assert_eq!(
        crashed_b.load(Ordering::SeqCst),
        0,
        "頁 B 不應觸發 on_crash"
    );
    assert_eq!(
        crashed_c.load(Ordering::SeqCst),
        0,
        "頁 C 不應觸發 on_crash"
    );
    assert_eq!(page_b.url(), url_b);
    assert_eq!(page_c.url(), url_c);
    let answer_b = page_b.evaluate("1 + 1".to_string()).await.unwrap();
    assert_eq!(answer_b.get("value").and_then(Value::as_f64), Some(2.0));
    let answer_c = page_c.evaluate("1 + 1".to_string()).await.unwrap();
    assert_eq!(answer_c.get("value").and_then(Value::as_f64), Some(2.0));

    // 瀏覽器程序還活著（kill 0 探測）。
    let alive = kill(Pid::from_raw(e2e.pgid as i32), None).is_ok();
    assert!(alive, "瀏覽器程序應還活著");

    // 收尾：context 與瀏覽器關閉（含 crashed 頁的 context 也要能收乾淨）。
    //
    // 注意：不對 crashed 頁呼叫 `close()`——實測它會卡死（§7.3.4 的實作跳過
    // `Target.closeTarget` 只等關閉通知，但 crashed target 不會自己結束；Playwright
    // 的 `_closePage` 一律送 `Target.closeTarget`）。此卡死問題記入回報，待 session
    // 層補上 crashed 語意後一併整合。
    context.close().await.unwrap();
    e2e.adapter.close().await;
    e2e.assert_cleaned_up().await;
}

// 原測試（S1.8 第四項）：SIGKILL 瀏覽器 → 1 秒內 on_disconnected
#[tokio::test]
#[ignore = "需要 CHROME_PATH"]
async fn sigkill_browser_reports_disconnected_within_one_second() {
    let e2e = launch_e2e_browser().await;
    let context = e2e.adapter.new_context(None, (1280, 720)).await.unwrap();
    let page = context.new_page().await.unwrap().shared;
    let url = mock_file_url("multi-console-b.html");
    page.goto(&url, WaitUntil::DomContentLoaded).await.unwrap();

    // listener：記下 disconnected 觸發時間與 on_close 次數。
    let fired_at = Arc::new(Mutex::new(None::<Instant>));
    let sink = Arc::clone(&fired_at);
    let _cancel = e2e.adapter.on_disconnected(move || {
        *sink.lock().unwrap() = Some(Instant::now());
    });
    let close_count = Arc::new(AtomicUsize::new(0));
    let close_sink = Arc::clone(&close_count);
    let _cancel_close = page.on_close(move || {
        close_sink.fetch_add(1, Ordering::SeqCst);
    });

    // SIGKILL 瀏覽器主程序（pid == process group id；launcher 已暴露 process_id）。
    let pid = Pid::from_raw(e2e.pgid as i32);
    let kill_at = Instant::now();
    kill(pid, Signal::SIGKILL).unwrap();

    // on_disconnected 在 1 秒內觸發（S1.8 的第四項）。
    assert!(
        eventually(Duration::from_secs(2), || fired_at
            .lock()
            .unwrap()
            .is_some())
        .await,
        "on_disconnected 應觸發"
    );
    let elapsed = fired_at.lock().unwrap().unwrap().duration_since(kill_at);
    assert!(
        elapsed < Duration::from_secs(1),
        "on_disconnected 應在 1 秒內觸發，實際 {elapsed:?}"
    );

    // 頁面標記關閉（斷線 → 監看任務收尾 → did_close_target）且 on_close 觸發一次。
    assert!(
        eventually(Duration::from_secs(2), || page.is_closed()).await,
        "頁面應標記關閉"
    );
    assert_eq!(close_count.load(Ordering::SeqCst), 1, "on_close 應觸發一次");

    // 之後的操作回 `Target page, context or browser has been closed`（§7.3.1）。
    let error = page.evaluate("1".to_string()).await.unwrap_err();
    assert_eq!(error.name, "Error");
    assert_eq!(
        error.message,
        "Target page, context or browser has been closed"
    );

    // 無孤兒程序：kill 的是主程序，子程序應隨瀏覽器結束一併離開。
    let profile_name = e2e.profile_name();
    assert!(
        eventually(Duration::from_secs(5), || !has_process_matching(
            &profile_name
        ))
        .await,
        "不得殘留使用此 profile 的程序"
    );

    // 斷線後 close() 仍要能完成（等收尾訊號）。
    e2e.adapter.close().await;
    e2e.assert_cleaned_up().await;
}
