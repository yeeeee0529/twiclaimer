//! CDP launcher：啟動 headless shell、握手與瀏覽器生命週期（§7.3.1、X9）。
//!
//! launch 流程（[`BrowserLauncher::launch`]）：解析執行檔 → 建暫存目錄 → 組參數 →
//! spawn → 建 [`Connection`] → 依錄製順序握手（`Browser.getVersion`、
//! `Target.setAutoAttach`），整體 180 秒逾時；逾時或任何失敗都整群 `SIGKILL`。
//! 成功後的 [`LaunchedBrowser`] 提供 close（直接寫 `Browser.close`）、disconnected
//! 觸發一次與 drop／取消的孤兒防線（§7.3.1）。
//!
//! 程序面機制：
//! - Chromium 參數組合：`doc/spec/chromium-args.txt` 逐字、依序使用，只換 user-data-dir。
//! - 暫存目錄：profile 與下載目錄都以 `twiclaimer-*` 為前綴（X9），瀏覽器結束後刪除。
//! - process group：`process_group(0)` + `kill_on_drop(true)`，並登記在全域 registry，
//!   程序結束前對所有存活的 group 送 `SIGKILL`（§7.3.1、§7.9）。
//! - `--remote-debugging-pipe`：兩條 CLOEXEC pipe，在 `pre_exec` 內 `dup2` 到子程序的
//!   fd 3（讀指令）與 fd 4（寫回應）；fd 撞號時以 `fcntl` 清 `FD_CLOEXEC`（DEC-007）。
//!   Linux 上另設定 `PR_SET_PDEATHSIG`（G8）。
//! - CDP trace 檔：`TWICLAIMER_CDP_TRACE`（§7.3.7），未設定時零額外成本。
//!
//! 以下 API 尚無正式呼叫端，等對應步驟接上後移除 allow：
//! - `BrowserTempDirs::artifacts` 的讀取（S1.4，`downloadPath`）。
//! - `kill_all_registered`（§7.9，app 結束前的防線）。
//! - `BrowserLauncher::launch_with`（測試注入假執行檔與較短逾時用）。
//! - `DisconnectedState::on_disconnected`、`LaunchedBrowser::get_version`／
//!   `process_id`／`temp_dir_paths`（S1.6 起的 listener 與 G1 e2e）。
//! - `LaunchedBrowser` 的 session 通知接收端（S1.4 上層消費）。
#![allow(dead_code)]

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::unix::pipe::{Receiver, Sender};
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot, watch};

use super::connection::{CdpError, Connection, SessionNotice};

/// S1.2 從 `$REF` 錄製的 Chromium 參數（逐字、依序使用；`--user-data-dir=<tmp>` 是遮蔽佔位）。
pub(crate) const CHROMIUM_ARGS: &str = include_str!("../../../doc/spec/chromium-args.txt");

/// 錄製時遮蔽 user-data-dir 的佔位整行。
const USER_DATA_DIR_PLACEHOLDER: &str = "--user-data-dir=<tmp>";

/// 子程序讀指令的固定 fd（`--remote-debugging-pipe` 的約定）。
const CDP_COMMAND_FD: i32 = 3;

/// 子程序寫回應的固定 fd。
const CDP_RESPONSE_FD: i32 = 4;

/// launch 的整體逾時（§7.3.1：180 秒內沒有握手回應就終止）。
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(180);

/// launcher 階段的錯誤：訊息格式照 Playwright（§7.3.1）。
#[derive(Debug)]
pub(crate) struct LaunchError(String);

impl LaunchError {
    fn new(message: impl Into<String>) -> Self {
        LaunchError(message.into())
    }

    /// 錯誤訊息（已含 Playwright 格式的前綴）。
    pub(crate) fn message(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LaunchError {}

impl From<io::Error> for LaunchError {
    fn from(error: io::Error) -> Self {
        LaunchError::new(error.to_string())
    }
}

impl From<Errno> for LaunchError {
    fn from(error: Errno) -> Self {
        LaunchError::new(error.to_string())
    }
}

/// 從環境變數 `CHROME_PATH` 解析瀏覽器執行檔（§7.3.1）。
///
/// 未設定（或空字串）時回 Playwright 同款錯誤，路徑部分為空字串：
/// `browserType.launch: Executable doesn't exist at `。
pub(crate) fn resolve_executable() -> Result<PathBuf, LaunchError> {
    match std::env::var_os("CHROME_PATH") {
        Some(path) if !path.is_empty() => Ok(PathBuf::from(path)),
        _ => Err(LaunchError::new(
            "browserType.launch: Executable doesn't exist at ",
        )),
    }
}

/// 組合瀏覽器參數：輸入是 `doc/spec/chromium-args.txt` 的內容（每行一個參數）。
///
/// 只把 `--user-data-dir=<tmp>` 佔位那行換成 `--user-data-dir=<實際暫存目錄>`，
/// 其餘行逐字保留（含順序與重複）。佔位不存在時不做任何替換（照原樣回傳）；
/// 空行（含檔尾換行造成者）一律略過，不會產生空參數。執行檔路徑不在參數裡。
pub(crate) fn compose_browser_args(raw_args: &str, user_data_dir: &Path) -> Vec<String> {
    raw_args
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            if line == USER_DATA_DIR_PLACEHOLDER {
                format!("--user-data-dir={}", user_data_dir.display())
            } else {
                line.to_string()
            }
        })
        .collect()
}

/// 瀏覽器的兩個暫存目錄（X9）。`Drop` 時自動刪除；也可用 `TempDir::close` 手動刪除。
pub(crate) struct BrowserTempDirs {
    /// 瀏覽器 profile 目錄（`twiclaimer-profile-*`）。
    pub(crate) profile: TempDir,
    /// 下載暫存目錄（`twiclaimer-artifacts-*`，§7.3.3 的 `downloadPath`）。
    pub(crate) artifacts: TempDir,
}

/// 建立 profile 與下載暫存目錄：都建在 `std::env::temp_dir()`（正式環境 `HOME=/tmp`）。
pub(crate) fn create_temp_dirs() -> io::Result<BrowserTempDirs> {
    let profile = tempfile::Builder::new()
        .prefix("twiclaimer-profile-")
        .tempdir_in(std::env::temp_dir())?;
    let artifacts = tempfile::Builder::new()
        .prefix("twiclaimer-artifacts-")
        .tempdir_in(std::env::temp_dir())?;
    Ok(BrowserTempDirs { profile, artifacts })
}

/// 全域 registry：所有存活的 process group id（§7.3.1：程序結束前全部 SIGKILL）。
static REGISTERED_PROCESS_GROUPS: OnceLock<Mutex<Vec<i32>>> = OnceLock::new();

/// registry 本體（測試也會用來檢查登記狀態）。
pub(crate) fn registered_process_groups() -> &'static Mutex<Vec<i32>> {
    REGISTERED_PROCESS_GROUPS.get_or_init(|| Mutex::new(Vec::new()))
}

/// 登記一個存活的 process group id。
///
/// 非正數直接忽略（`killpg(0)` 會打到自己的 group，絕不能發生）；
/// 重複登記只留一份。
pub(crate) fn register_process_group(pgid: i32) {
    if pgid <= 0 {
        return;
    }
    let mut groups = registered_process_groups()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !groups.contains(&pgid) {
        groups.push(pgid);
    }
}

/// 解除登記（瀏覽器正常關閉並回收後呼叫）。
pub(crate) fn unregister_process_group(pgid: i32) {
    let mut groups = registered_process_groups()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    groups.retain(|registered| *registered != pgid);
}

/// 對所有登記中的 process group 送 `SIGKILL`，確保不留孤兒（§7.3.1）。
///
/// 已不存在的 group（ESRCH）與其他錯誤一律忽略：這是結束前最後一道防線，
/// 失敗也無處回報。不清除登記，由結束流程逐一 [`unregister_process_group`]。
pub(crate) fn kill_all_registered() {
    let groups = registered_process_groups()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for &pgid in groups.iter() {
        let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
    }
}

/// 建立一條 CLOEXEC pipe：Linux 用 `pipe2(O_CLOEXEC)` 原子設定；
/// macOS 沒有 `pipe2`，改用 `pipe` + `fcntl` 逐端補上（與 spawn 之間有極小的競態窗口，僅影響本機開發）。
fn create_cloexec_pipe() -> nix::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    #[cfg(target_os = "linux")]
    let pipe = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC);

    #[cfg(not(target_os = "linux"))]
    let pipe = {
        use nix::fcntl::{FcntlArg, FdFlag, fcntl};
        let (read_fd, write_fd) = nix::unistd::pipe()?;
        fcntl(&read_fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
        fcntl(&write_fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
        Ok((read_fd, write_fd))
    };
    pipe
}

/// spawn 成功後的瀏覽器控制把手（launch 流程之後接上）。
pub(crate) struct SpawnedBrowser {
    /// 子程序（`kill_on_drop(true)`：把手被 drop 時送 SIGKILL，不留孤兒）。
    pub(crate) child: Child,
    /// 父端寫指令（子程序 fd 3 的另一端），每則訊息是一個 JSON 加 `\0`。
    pub(crate) command_sender: Sender,
    /// 父端讀回應（子程序 fd 4 的另一端）。
    pub(crate) response_receiver: Receiver,
}

/// 以 `--remote-debugging-pipe` 啟動瀏覽器：設定 process group、CDP 管道與輸出傾印。
///
/// 兩個平台都在 `pre_exec` 內把 pipe `dup2` 到子程序的 fd 3／4；fd 撞號時以
/// `fcntl` 清 `FD_CLOEXEC`（DEC-007）。Linux 另外設定 `PR_SET_PDEATHSIG`（G8）。
pub(crate) fn spawn_browser(
    executable: &Path,
    args: &[String],
) -> Result<SpawnedBrowser, LaunchError> {
    let mut command = tokio::process::Command::new(executable);
    command.args(args);
    // process_group(0)：子程序自成 process group（pgid == pid），關閉時整群 SIGKILL。
    command.process_group(0);
    // launch 流程被取消、逾時或把手被 drop 時不留孤兒（§7.3.1）。
    command.kill_on_drop(true);
    // CDP 走 fd 3／4 的 pipe，stdin 不用。
    command.stdin(Stdio::null());
    // stdout／stderr 持續讀出後丟棄，避免 pipe 滿了塞住瀏覽器。
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    // 兩條 CLOEXEC pipe：父端保留寫指令與讀回應端，子程序端在 pre_exec 內 dup2 到固定 fd。
    let (cmd_read, cmd_write) = create_cloexec_pipe()?;
    let (resp_read, resp_write) = create_cloexec_pipe()?;

    {
        use std::os::fd::AsRawFd as _;
        let cmd_read_fd = cmd_read.as_raw_fd();
        let resp_write_fd = resp_write.as_raw_fd();
        // 安全性：閉包只在 fork 後、exec 前的子程序內執行，僅呼叫 async-signal-safe 的系統呼叫
        //（dup2、fcntl、prctl），不做任何配置或鎖。
        unsafe {
            command.pre_exec(move || {
                dup2_to_fixed_fd(cmd_read_fd, CDP_COMMAND_FD)?;
                dup2_to_fixed_fd(resp_write_fd, CDP_RESPONSE_FD)?;
                set_pdeathsig_in_child()
            });
        }
    }

    let mut child = command.spawn()?;

    // 父端立即關閉子程序端：子程序結束時父端才能即時看到 EOF（on_disconnected）。
    drop(cmd_read);
    drop(resp_write);

    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(drain_stream(stdout));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(drain_stream(stderr));
    }

    let command_sender = Sender::from_owned_fd(cmd_write)?;
    let response_receiver = Receiver::from_owned_fd(resp_read)?;

    // process_group(0) 之下 pgid == pid；登記到全域 registry，結束前統一 SIGKILL（§7.3.1）。
    if let Some(pid) = child.id() {
        register_process_group(pid as i32);
    }

    Ok(SpawnedBrowser {
        child,
        command_sender,
        response_receiver,
    })
}

/// 把 pipe 內容持續讀出後丟棄（stdout／stderr 不需要內容，但不能讓 pipe 滿住）。
async fn drain_stream<R>(mut stream: R)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt as _;
    let mut buffer = [0u8; 8192];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

/// 把 fd 複製到子程序的固定 fd（fd 3／4），只在 `pre_exec` 內呼叫（async-signal-safe）。
///
/// oldfd 與目標相同時 `dup2` 是 no-op、不會清 CLOEXEC，需改用 `fcntl` 清掉，
/// 否則 exec 後該 fd 會被 CLOEXEC 關掉，瀏覽器讀不到指令（DEC-007）。
fn dup2_to_fixed_fd(fd: i32, target: i32) -> io::Result<()> {
    let result = if fd == target {
        // 安全性：fcntl 的 F_SETFD 是系統呼叫包裝，async-signal-safe。
        unsafe { libc::fcntl(fd, libc::F_SETFD, 0) }
    } else {
        // 安全性：dup2 是系統呼叫包裝，async-signal-safe。
        unsafe { libc::dup2(fd, target) }
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// 在子程序內設定 `PR_SET_PDEATHSIG`（SIGKILL）：控制端被 SIGKILL 時瀏覽器一併結束（G8）。
/// 只在 `pre_exec` 內呼叫（async-signal-safe）。macOS 沒有對應機制，回傳成功。
#[cfg(target_os = "linux")]
fn set_pdeathsig_in_child() -> io::Result<()> {
    // 安全性：prctl 是系統呼叫包裝，async-signal-safe。
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
fn set_pdeathsig_in_child() -> io::Result<()> {
    Ok(())
}

/// CDP trace 寫入器（§7.3.7）：`TWICLAIMER_CDP_TRACE` 有設定時寫檔，未設定時為 no-op。
///
/// 連線層（讀寫 task 與指令送出）與 launcher（`<launching>` 行）共用同一個
/// 寫入器（[`SharedTrace`]），鎖內寫入以保持 SEND／RECV 的實際順序。
pub(crate) enum TraceWriter {
    /// 未設定環境變數：不開檔、不寫入，零額外成本。
    Disabled,
    /// 寫入 trace 檔，每行立即 flush（格式與 Playwright 的 `DEBUG=pw:protocol` 相同）。
    File(std::fs::File),
}

/// 各端共用的 trace 寫入器（鎖保護；停用時鎖開銷可忽略）。
pub(crate) type SharedTrace = Arc<std::sync::Mutex<TraceWriter>>;

/// 依環境變數建立共用的 trace 寫入器。
pub(crate) fn shared_trace_from_env() -> SharedTrace {
    Arc::new(std::sync::Mutex::new(TraceWriter::from_env()))
}

impl TraceWriter {
    /// 測試用建構子：直接給 trace 檔路徑（開新檔，既有內容截斷）或 `None`（no-op）。
    pub(crate) fn new(path: Option<&Path>) -> io::Result<TraceWriter> {
        match path {
            Some(path) => {
                let file = OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open(path)?;
                Ok(TraceWriter::File(file))
            }
            None => Ok(TraceWriter::Disabled),
        }
    }

    /// 依環境變數 `TWICLAIMER_CDP_TRACE` 建立寫入器。
    ///
    /// 有設定但開檔失敗時退化為 no-op：trace 只是輔助診斷，不讓啟動流程失敗。
    pub(crate) fn from_env() -> TraceWriter {
        match std::env::var_os("TWICLAIMER_CDP_TRACE") {
            Some(path) if !path.is_empty() => {
                TraceWriter::new(Some(Path::new(&path))).unwrap_or(TraceWriter::Disabled)
            }
            _ => TraceWriter::Disabled,
        }
    }

    /// 第一行：`<launching> <執行檔> <參數…>`（參數以空格連接）。要在啟動時最先呼叫。
    pub(crate) fn write_launching(&mut self, executable: &Path, args: &[String]) {
        let mut parts = Vec::with_capacity(args.len() + 2);
        parts.push("<launching>".to_string());
        parts.push(executable.display().to_string());
        parts.extend(args.iter().cloned());
        self.write_line(&parts.join(" "));
    }

    /// 送出訊息：`SEND ► <json>`。
    pub(crate) fn record_send(&mut self, json: &str) {
        self.write_line(&format!("SEND ► {json}"));
    }

    /// 收到訊息：`◀ RECV <json>`。
    pub(crate) fn record_recv(&mut self, json: &str) {
        self.write_line(&format!("◀ RECV {json}"));
    }

    /// 寫一行並立即 flush；寫入失敗不影響主要流程。
    fn write_line(&mut self, line: &str) {
        let TraceWriter::File(file) = self else {
            return;
        };
        let _ = file.write_all(line.as_bytes());
        let _ = file.write_all(b"\n");
        let _ = file.flush();
    }
}

/// 取瀏覽器版本：`Browser.getVersion` 的 product 去掉 `/` 之前的部分（§7.3.1）。
///
/// `HeadlessChrome/151.0.7922.34` → `151.0.7922.34`（取第一個 `/` 之後）；
/// 沒有 `/` 時原樣回傳。
pub(crate) fn parse_browser_version(product: &str) -> String {
    match product.split_once('/') {
        Some((_product, version)) => version.to_string(),
        None => product.to_string(),
    }
}

/// 對整個 process group 送 `SIGKILL`（pgid 非正數時不送：`killpg(0)` 會打到自己）。
pub(crate) fn kill_process_group(pgid: i32) {
    if pgid > 0 {
        let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
    }
}

/// launch 期間的防線：任何非成功路徑（整體逾時、future 被 drop、握手失敗）離開時，
/// 把 process group 整群 `SIGKILL`，不留孤兒（§7.3.1）。成功時先 [`LaunchGuard::disarm`]。
struct LaunchGuard {
    pgid: i32,
    armed: bool,
}

impl LaunchGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for LaunchGuard {
    fn drop(&mut self) {
        if self.armed {
            kill_process_group(self.pgid);
        }
    }
}

/// 斷線 listener 的取消函式（對應舊版回傳的 `() => void`，§7.3.5）。
pub(crate) type CancelListener = Box<dyn Fn() + Send + Sync>;

/// 瀏覽器斷線狀態（§7.3.1：pipe EOF、讀取錯誤或程序結束時觸發一次）。
#[derive(Default)]
pub(crate) struct DisconnectedState {
    disconnected: AtomicBool,
    /// 註冊的 listener（對應 Playwright 的 `browser.on('disconnected')`，§7.3.5）。
    listeners: Mutex<Vec<Arc<dyn Fn() + Send + Sync>>>,
}

impl DisconnectedState {
    /// 是否已斷線。
    pub(crate) fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::SeqCst)
    }

    /// 註冊 listener，回傳取消函式（§7.3.5）。
    ///
    /// 已斷線後才註冊的 listener 不會再觸發（照 Playwright EventEmitter 的語意）。
    pub(crate) fn on_disconnected(
        self: &Arc<Self>,
        listener: impl Fn() + Send + Sync + 'static,
    ) -> CancelListener {
        let mut listeners = self
            .listeners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry: Arc<dyn Fn() + Send + Sync> = Arc::new(listener);
        listeners.push(Arc::clone(&entry));
        let state = Arc::clone(self);
        Box::new(move || {
            let mut listeners = state
                .listeners
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            listeners.retain(|registered| !Arc::ptr_eq(registered, &entry));
        })
    }

    /// 標記斷線並觸發所有 listener（重複呼叫只觸發一次）。
    ///
    /// listener 在鎖外逐一呼叫；listener 內的 panic 被攔下，不影響其他 listener
    /// 與收尾流程（§7.3.5：listener 的錯誤不得影響生命週期）。
    pub(crate) fn mark_disconnected(&self) {
        if self.disconnected.swap(true, Ordering::SeqCst) {
            return;
        }
        let listeners = {
            let mut guard = self
                .listeners
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *guard)
        };
        for listener in listeners {
            // listener 是外部提供的程式碼：攔下 panic 讓收尾繼續。
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener()));
        }
    }
}

/// launch 成功後的瀏覽器控制把手（§7.3.1）。
///
/// 生命週期：
/// - [`LaunchedBrowser::close`]：直接寫出 `Browser.close`，等程序結束、暫存目錄刪除後返回；
///   不設逾時（呼叫端負責逾時）。
/// - future 被取消或把手被 drop：`SIGKILL` 整個 process group，背景收屍與清理（不留孤兒）。
/// - pipe EOF、讀取錯誤或程序結束：disconnected 觸發一次；之後的 CDP 指令回
///   `Target page, context or browser has been closed`。
pub(crate) struct LaunchedBrowser {
    connection: Connection,
    /// attach／detach 通知接收端（S1.4 上層持有並消費）。
    notices: mpsc::Receiver<SessionNotice>,
    /// process group id（== 主程序 pid；非正數表示 pid 未取得，等同無防線目標）。
    pgid: i32,
    /// 主程序的 pid（e2e 與診斷用）。
    pid: Option<u32>,
    /// `Browser.getVersion` 的版本（`browser.version()`）。
    version: String,
    /// `Browser.getVersion` 的完整 userAgent（字型族表依平台選擇）。
    user_agent: String,
    /// 收尾完成的訊號：`true` 表示程序已結束、暫存目錄已刪、registry 已解除。
    finalized: watch::Receiver<bool>,
    disconnected: Arc<DisconnectedState>,
    /// 暫存目錄路徑（實體由監看任務持有並在結束後刪除）。
    profile_path: PathBuf,
    artifacts_path: PathBuf,
    /// close() 只送一次 `Browser.close` 的防重入旗標。
    close_started: AtomicBool,
}

impl LaunchedBrowser {
    /// 瀏覽器版本（Playwright 的 `browser.version()`）。
    pub(crate) fn get_version(&self) -> &str {
        &self.version
    }

    /// `Browser.getVersion` 的完整 userAgent。
    pub(crate) fn user_agent(&self) -> &str {
        &self.user_agent
    }

    /// 依 userAgent 判斷平台（Playwright `FrameSession` 的 `_platform()`）：
    /// Windows／Macintosh／其餘為 linux，供字型族表選擇。
    pub(crate) fn platform(&self) -> &'static str {
        if self.user_agent.contains("Windows") {
            "win"
        } else if self.user_agent.contains("Macintosh") {
            "mac"
        } else {
            "linux"
        }
    }

    /// 瀏覽器主程序的 pid（== process group id）。
    pub(crate) fn process_id(&self) -> Option<u32> {
        self.pid
    }

    /// 兩個暫存目錄的路徑（profile、下載；G1 e2e 驗證清理用）。
    pub(crate) fn temp_dir_paths(&self) -> (&Path, &Path) {
        (&self.profile_path, &self.artifacts_path)
    }

    /// 是否已斷線（pipe EOF、讀取錯誤或程序結束）。
    pub(crate) fn is_disconnected(&self) -> bool {
        self.disconnected.is_disconnected()
    }

    /// 註冊斷線 listener，回傳取消函式（§7.3.5）。
    pub(crate) fn on_disconnected(
        &self,
        listener: impl Fn() + Send + Sync + 'static,
    ) -> CancelListener {
        self.disconnected.on_disconnected(listener)
    }

    /// 送出一則 CDP 指令，等回應（§7.3.2）。連線已斷時回 [`CdpError::TargetClosed`]。
    pub(crate) async fn send_command(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, CdpError> {
        // 回應接收端被丟棄（連線收尾時清理等待者）視同 TargetClosed。
        match self
            .connection
            .send_command(session, method, params)
            .await
            .await
        {
            Ok(result) => result,
            Err(_) => Err(CdpError::TargetClosed),
        }
    }

    /// 登記一個子 session（嵌套 auto-attach），取得事件接收端（S1.4）。
    pub(crate) async fn register_session(
        &self,
        session_id: &str,
    ) -> mpsc::Receiver<super::connection::Event> {
        self.connection.register_session(session_id).await
    }

    /// 結束一個子 session：未完成的指令全部失敗（S1.4）。
    pub(crate) async fn unregister_session(&self, session_id: &str) {
        let _ = self.connection.unregister_session(session_id).await;
    }

    /// 取走 attach／detach 通知接收端（[`super::context::BrowserAdapter`] 建構時呼叫一次；
    /// 之後的通知由該監看任務消費）。
    pub(crate) fn take_notices(&mut self) -> mpsc::Receiver<SessionNotice> {
        let (dead_tx, dead_rx) = mpsc::channel(1);
        drop(dead_tx); // 佔位接收端：取走後送進來的通知會被丟棄。
        std::mem::replace(&mut self.notices, dead_rx)
    }

    /// 關閉瀏覽器（§7.3.1）：直接寫出 `Browser.close`（不經 Router 的等待者，照
    /// Playwright 寫 pipe），等程序結束、暫存目錄刪除、registry 解除後返回。
    ///
    /// 不設逾時——逾時由呼叫端負責；把手被 drop（含未來的呼叫端逾時取消）時由
    /// [`Drop`] 對整個 process group 送 `SIGKILL`，背景收屍。
    pub(crate) async fn close(&self) {
        // 只送一次（重複 close 是 no-op，照 Playwright）。
        if self.close_started.swap(true, Ordering::SeqCst) {
            self.wait_finalized().await;
            return;
        }
        // 程序還在（連線還活著）才寫；連線已斷時寫入也會失敗，只是噪音。
        if !self.disconnected.is_disconnected() {
            self.connection.send_unguarded("Browser.close", json!({}));
        }
        self.wait_finalized().await;
    }

    /// 等監看任務的收尾完成訊號。
    async fn wait_finalized(&self) {
        // clone 出自有權的接收端（`changed()` 需要 `&mut`），等監看任務收尾完成。
        // 連線已斷（EOF）時讀寫 task 已自然收尾。
        let mut finalized = self.finalized.clone();
        while !*finalized.borrow_and_update() {
            if finalized.changed().await.is_err() {
                // 監看任務已消失（不該發生）：不再等，drop 時仍會 killpg 兜底。
                break;
            }
        }
    }
}

impl Drop for LaunchedBrowser {
    fn drop(&mut self) {
        // 把手被 drop（含 close() 的 future 被取消）：整個 process group `SIGKILL`；
        // 收屍、暫存目錄刪除與 registry 解除由監看任務在背景完成（§7.3.1 不留孤兒）。
        kill_process_group(self.pgid);
    }
}

/// 瀏覽器啟動器（D1：只支援 Chromium headless shell；`headless` 等選項由錄製參數固定）。
#[derive(Default)]
pub(crate) struct BrowserLauncher;

impl BrowserLauncher {
    pub(crate) fn new() -> Self {
        BrowserLauncher
    }

    /// 以 `CHROME_PATH` 啟動瀏覽器並完成握手（§7.3.1）。
    pub(crate) async fn launch(&self) -> Result<LaunchedBrowser, LaunchError> {
        match resolve_executable() {
            Ok(executable) => self.launch_with(executable, LAUNCH_TIMEOUT).await,
            Err(error) => Err(error),
        }
    }

    /// 以指定執行檔與逾時啟動（測試注入假執行檔與較短逾時用）。
    pub(crate) async fn launch_with(
        &self,
        executable: PathBuf,
        timeout: Duration,
    ) -> Result<LaunchedBrowser, LaunchError> {
        match tokio::time::timeout(timeout, self.launch_attempt(executable)).await {
            Ok(result) => result,
            Err(_) => Err(LaunchError::new(format!(
                "browserType.launch: Timeout {}ms exceeded.",
                timeout.as_millis()
            ))),
        }
    }

    /// 啟動的一次嘗試：暫存目錄、spawn、接線與握手。
    async fn launch_attempt(&self, executable: PathBuf) -> Result<LaunchedBrowser, LaunchError> {
        // trace 的第一行要在啟動時最先寫（§7.3.7；SEND／RECV 紀錄由 Connection 接手）。
        let trace = shared_trace_from_env();
        let temp_dirs = create_temp_dirs()?;
        let args = compose_browser_args(CHROMIUM_ARGS, temp_dirs.profile.path());
        trace
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .write_launching(&executable, &args);

        let spawned = spawn_browser(&executable, &args)?;
        let pid = spawned.child.id();
        let pgid = pid.map(|pid| pid as i32).unwrap_or(0);
        let profile_path = temp_dirs.profile.path().to_path_buf();
        let artifacts_path = temp_dirs.artifacts.path().to_path_buf();

        let (finalized_tx, finalized) = watch::channel(false);
        let disconnected = Arc::new(DisconnectedState::default());

        // 連線接線：讀瀏覽器的回應與事件、寫指令（§7.3.1 的兩條 pipe）。
        let (read_end_tx, read_end_rx) = oneshot::channel();
        let trace = shared_trace_from_env();
        let (connection, notices) = Connection::spawn(
            spawned.response_receiver,
            spawned.command_sender,
            Some(read_end_tx),
            Arc::clone(&trace),
        );
        // pipe EOF／讀取錯誤時：連線結束——整群 SIGKILL（群組多半已死；讀取錯誤時
        // 瀏覽器可能還活著，不能留下孤兒），收屍與暫存目錄刪除由監看任務完成，
        // 並觸發一次 disconnected（§7.3.1）。
        {
            let disconnected = Arc::clone(&disconnected);
            tokio::spawn(async move {
                if read_end_rx.await.is_ok() {
                    kill_process_group(pgid);
                    disconnected.mark_disconnected();
                }
            });
        }
        // 背景監看：程序結束時收屍、觸發 disconnected、刪暫存目錄、解除登記。
        tokio::spawn(monitor_browser_exit(
            spawned.child,
            pgid,
            temp_dirs,
            finalized_tx,
            Arc::clone(&disconnected),
        ));

        // 逾時防線：任何非成功路徑（含未來被 drop／逾時取消）都 SIGKILL process group。
        let mut guard = LaunchGuard { pgid, armed: true };

        // 握手（§7.3.1 錄製順序）：先 `Browser.getVersion`，再 `Target.setAutoAttach`。
        let version_reply = connection
            .send_command(None, "Browser.getVersion", json!({}))
            .await;
        let version_value = await_handshake_reply(version_reply).await?;
        let product = version_value
            .get("product")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                LaunchError::new(
                    "browserType.launch: Browser.getVersion response is missing 'product'",
                )
            })?;
        let user_agent = version_value
            .get("userAgent")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let auto_attach_reply = connection
            .send_command(
                None,
                "Target.setAutoAttach",
                json!({"autoAttach": true, "flatten": true, "waitForDebuggerOnStart": true}),
            )
            .await;
        await_handshake_reply(auto_attach_reply).await?;

        guard.disarm();
        Ok(LaunchedBrowser {
            connection,
            notices,
            pgid,
            pid,
            version: parse_browser_version(product),
            user_agent,
            finalized,
            disconnected,
            profile_path,
            artifacts_path,
            close_started: AtomicBool::new(false),
        })
    }
}

/// 握手失敗（CDP 錯誤或連線結束）轉成 launch 錯誤。
fn handshake_failure(error: CdpError) -> LaunchError {
    LaunchError::new(format!("browserType.launch: {}", error.message()))
}

/// 等一則握手回應：接收端失敗（連線收尾時清理等待者）視同連線結束。
async fn await_handshake_reply(
    reply: oneshot::Receiver<Result<Value, CdpError>>,
) -> Result<Value, LaunchError> {
    match reply.await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(handshake_failure(error)),
        Err(_) => Err(handshake_failure(CdpError::TargetClosed)),
    }
}

/// 背景監看瀏覽器程序：結束時收屍、觸發 disconnected、刪暫存目錄、解除 registry 登記，
/// 最後才送出收尾完成的訊號（[`LaunchedBrowser::close`] 等的就是這個）。
async fn monitor_browser_exit(
    mut child: Child,
    pgid: i32,
    temp_dirs: BrowserTempDirs,
    finalized: watch::Sender<bool>,
    disconnected: Arc<DisconnectedState>,
) {
    // 收屍（kill_on_drop 的 drop 路徑之外，正常結束也要 wait 避免 zombie）。
    let _ = child.wait().await;
    // 先觸發 disconnected（§7.3.1：程序結束時觸發一次；S1.8 要求 1 秒內）。
    disconnected.mark_disconnected();
    // 瀏覽器結束後刪除暫存目錄（§7.3.1、X9）。
    drop(temp_dirs);
    unregister_process_group(pgid);
    // 最後才通知收尾完成：close() 返回時清理已全部完成。
    let _ = finalized.send(true);
}

#[cfg(test)]
mod tests;
