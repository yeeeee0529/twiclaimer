//! CDP launcher：啟動 headless shell、管理 process group 與暫存目錄（§7.3.1、X9）。
//!
//! 本檔負責「程序面機制」：
//! - Chromium 參數組合：`doc/spec/chromium-args.txt` 逐字、依序使用，只換 user-data-dir。
//! - 暫存目錄：profile 與下載目錄都以 `twiclaimer-*` 為前綴（X9），瀏覽器結束後刪除。
//! - process group：`process_group(0)` + `kill_on_drop(true)`，並登記在全域 registry，
//!   程序結束前對所有存活的 group 送 `SIGKILL`（§7.3.1、§7.9）。
//! - `--remote-debugging-pipe`：兩條 `O_CLOEXEC` pipe，Linux 上在 `pre_exec` 內
//!   `dup2` 到子程序的 fd 3（讀指令）與 fd 4（寫回應），並設定 `PR_SET_PDEATHSIG`（G8）。
//! - CDP trace 檔：`TWICLAIMER_CDP_TRACE`（§7.3.7），未設定時零額外成本。
//!
//! S1.3 進行中：真正的 launch 流程（等 `Browser.getVersion`、180 秒逾時、close）之後接上。
// S1.3 進行中：spawn 相關函式還沒有正式呼叫端，完成後移除。
#![allow(dead_code)]

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};

use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tempfile::TempDir;
use tokio::net::unix::pipe::{Receiver, Sender};
use tokio::process::Child;

/// S1.2 從 `$REF` 錄製的 Chromium 參數（逐字、依序使用；`--user-data-dir=<tmp>` 是遮蔽佔位）。
const CHROMIUM_ARGS: &str = include_str!("../../../doc/spec/chromium-args.txt");

/// 錄製時遮蔽 user-data-dir 的佔位整行。
const USER_DATA_DIR_PLACEHOLDER: &str = "--user-data-dir=<tmp>";

/// 子程序讀指令的固定 fd（`--remote-debugging-pipe` 的約定）。
#[cfg(target_os = "linux")]
const CDP_COMMAND_FD: i32 = 3;

/// 子程序寫回應的固定 fd。
#[cfg(target_os = "linux")]
const CDP_RESPONSE_FD: i32 = 4;

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
fn registered_process_groups() -> &'static Mutex<Vec<i32>> {
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
/// macOS 不做 fd 3／4 的 `dup2`（只 Linux 需要），但 pipe 建立與其餘設定在兩個平台都相同。
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

    #[cfg(target_os = "linux")]
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
/// 否則 exec 後該 fd 會被 CLOEXEC 關掉，瀏覽器讀不到指令。
#[cfg(target_os = "linux")]
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
/// 只在 `pre_exec` 內呼叫（async-signal-safe）。
#[cfg(target_os = "linux")]
fn set_pdeathsig_in_child() -> io::Result<()> {
    // 安全性：prctl 是系統呼叫包裝，async-signal-safe。
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// CDP trace 寫入器（§7.3.7）：`TWICLAIMER_CDP_TRACE` 有設定時寫檔，未設定時為 no-op。
pub(crate) enum TraceWriter {
    /// 未設定環境變數：不開檔、不寫入，零額外成本。
    Disabled,
    /// 寫入 trace 檔，每行立即 flush（格式與 Playwright 的 `DEBUG=pw:protocol` 相同）。
    File(std::fs::File),
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

#[cfg(test)]
mod tests;
