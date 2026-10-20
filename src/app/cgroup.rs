//! cgroup v2 讀取器：`app/CgroupV2Reader.ts` 逐函式移植（§4.2，S3.2）。
//!
//! 取捨（macOS 可驗證性，照計畫 §122 與 `sessions/recovery.rs`、`credentials`
//! 的 DI 前例）：
//! - 邏輯本體（根路徑解析、檔案格式解析）完全可移植：cgroup 根與 `/proc`
//!   路徑全部以參數注入，檔案讀取以 [`CgroupFileSystem`] trait 注入。測試用
//!   `tempfile` 假檔案樹（或記憶體對照表）在 macOS 全跑，不讀真的
//!   `/sys/fs/cgroup`（計畫 §8 的 S3.2 註記）。
//! - 平台預設路徑（`/sys/fs/cgroup`、`/proc/self/…`）照舊版常數保留；只有
//!   Linux 有真實內容，macOS 上 probe 會回報不可用。預設組裝的
//!   `#[cfg(target_os = "linux")]` 切分放在呼叫端薄層（S3.6 的
//!   createApplication），由 Docker test stage 驗證。
//! - JS `bigint` 的計數以 `u64` 呈現（實際 cgroup 計數不會超出 u64）。
//! - 舊版錯誤的 `cause` 鏈不重現：所有消費端只讀 `error.message`
//!   （[`CgroupV2Error`] 的 `Display` 即該訊息，逐字照舊版）。

#![allow(dead_code)]

use std::io;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use async_trait::async_trait;
use tokio::sync::OnceCell;

/// 舊版 `CgroupMemoryEvents`（`app/CgroupV2Reader.ts`）：memory.events 計數器
/// （JS bigint 以 u64 呈現，cgroup 計數不會超出）。
///
/// 型別自 S3.2 起定義在此；`app/resource_guard.rs` 以 `use` 重用（DEC-013）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CgroupMemoryEvents {
    pub high: u64,
    pub max: u64,
    pub oom: u64,
    pub oom_kill: u64,
}

/// 舊版 `CgroupCpuStat`：cpu.stat 的可移植欄位（未出現的為 `None`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CgroupCpuStat {
    pub usage_usec: u64,
    pub user_usec: Option<u64>,
    pub system_usec: Option<u64>,
}

/// 舊版 `CgroupSnapshot`：一次 cgroup 取樣的完整欄位（欄位與舊版物件建立的
/// 順序一致；`None` 對應舊版的 `undefined`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CgroupSnapshot {
    pub sampled_at_monotonic_ms: u64,
    pub memory_current_bytes: u64,
    pub memory_max_bytes: Option<u64>,
    pub memory_peak_bytes: Option<u64>,
    pub swap_current_bytes: Option<u64>,
    pub pids_current: Option<u64>,
    pub pids_max: Option<u64>,
    pub events: CgroupMemoryEvents,
    pub cpu: Option<CgroupCpuStat>,
}

/// 舊版 `CgroupReaderAvailability`（tagged union）：probe 的結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CgroupReaderAvailability {
    Available { root_path: String },
    Unavailable { reason: String },
}

/// 舊版 `CgroupV2UnavailableError`／`CgroupV2ReadError`：`Display` 輸出舊版的
/// `error.message` 逐字（消費端只讀 message；`cause` 鏈不重現）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CgroupV2Error {
    /// 舊版 `CgroupV2UnavailableError`。
    Unavailable(String),
    /// 舊版 `CgroupV2ReadError`。
    Read(String),
}

impl std::fmt::Display for CgroupV2Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CgroupV2Error::Unavailable(message) | CgroupV2Error::Read(message) => {
                formatter.write_str(message)
            }
        }
    }
}

impl std::error::Error for CgroupV2Error {}

/// 舊版 `CgroupFileSystem`：`(readFile(path, 'utf8'), access(path))` 的注入
/// 介面。正式程式用 [`TokioCgroupFileSystem`]，測試注入記憶體對照表或
/// `tempfile` 樹。
#[async_trait]
pub(crate) trait CgroupFileSystem: Send + Sync {
    /// 對應 `readFile(filePath, 'utf8')`。
    async fn read_text(&self, path: &str) -> io::Result<String>;

    /// 對應 `access(filePath)`：存在回 `Ok(())`、不存在回 `Err`。
    async fn access(&self, path: &str) -> io::Result<()>;
}

/// 舊版建構子的預設 fs：`node:fs/promises` 的對應（tokio 的檔案系統 API）。
pub(crate) struct TokioCgroupFileSystem;

#[async_trait]
impl CgroupFileSystem for TokioCgroupFileSystem {
    async fn read_text(&self, path: &str) -> io::Result<String> {
        tokio::fs::read_to_string(path).await
    }

    async fn access(&self, path: &str) -> io::Result<()> {
        // 舊版 `access()`：存在即成功（跟隨符號連結），否則 reject。
        if std::path::Path::new(path).try_exists()? {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no such file or directory, access '{path}'"),
            ))
        }
    }
}

/// 舊版 options 的 `now: () => number`：回傳毫秒；預設 `performance.now()` 的
/// 對應（單調毫秒，見 [`monotonic_now_ms`]）。以 `Arc` 呈現以便 reader 與
/// monitor 共用同一個時鐘。
pub(crate) type CgroupClock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// 舊版 `CgroupV2ReaderOptions`：fs、時鐘與三個預設路徑皆可注入。
#[derive(Default)]
pub(crate) struct CgroupV2ReaderOptions {
    pub fs: Option<Arc<dyn CgroupFileSystem>>,
    pub now: Option<CgroupClock>,
    pub proc_self_cgroup_path: Option<String>,
    pub proc_self_mountinfo_path: Option<String>,
    pub sys_fs_cgroup_path: Option<String>,
}

/// 對應舊版 `CgroupV2Reader`：讀取目前程序樹的 cgroup v2 CPU／memory／pids
/// 計數器。缺少的選用檔案視為不可用（而非零）。
pub(crate) struct CgroupV2Reader {
    fs: Arc<dyn CgroupFileSystem>,
    now: CgroupClock,
    proc_self_cgroup_path: String,
    proc_self_mountinfo_path: String,
    sys_fs_cgroup_path: String,
    /// 舊版 `rootPathPromise`：根路徑只探測一次；失敗時重設（下次再探）。
    root_path_cell: OnceCell<String>,
}

impl CgroupV2Reader {
    pub(crate) fn new(options: CgroupV2ReaderOptions) -> Self {
        Self {
            fs: options
                .fs
                .unwrap_or_else(|| Arc::new(TokioCgroupFileSystem)),
            now: options.now.unwrap_or_else(|| Arc::new(monotonic_now_ms)),
            proc_self_cgroup_path: options
                .proc_self_cgroup_path
                .unwrap_or_else(|| DEFAULT_PROC_SELF_CGROUP.to_string()),
            proc_self_mountinfo_path: options
                .proc_self_mountinfo_path
                .unwrap_or_else(|| DEFAULT_PROC_SELF_MOUNTINFO.to_string()),
            sys_fs_cgroup_path: options
                .sys_fs_cgroup_path
                .unwrap_or_else(|| DEFAULT_SYS_FS_CGROUP.to_string()),
            root_path_cell: OnceCell::new(),
        }
    }

    /// 對應舊版 `probe()`。
    pub(crate) async fn probe(&self) -> CgroupReaderAvailability {
        match self.resolve_root_path().await {
            Ok(root_path) => CgroupReaderAvailability::Available { root_path },
            Err(error) => CgroupReaderAvailability::Unavailable {
                // 舊版：`error instanceof Error ? error.message : 'cgroup v2 is unavailable'`。
                reason: error.to_string(),
            },
        }
    }

    /// 對應舊版 `readSnapshot()`。
    pub(crate) async fn read_snapshot(&self) -> Result<CgroupSnapshot, CgroupV2Error> {
        let root_path = self.resolve_root_path().await?;
        let memory_current_bytes = self
            .read_required_counter(&join_path(&root_path, "memory.current"), "memory.current")
            .await?;
        let events = self
            .read_memory_events(&join_path(&root_path, "memory.events"))
            .await?;
        let swap_current_bytes = self
            .read_optional_counter(&join_path(&root_path, "memory.swap.current"))
            .await?;
        let memory_max_bytes = self
            .read_optional_counter(&join_path(&root_path, "memory.max"))
            .await?;
        let memory_peak_bytes = self
            .read_optional_counter(&join_path(&root_path, "memory.peak"))
            .await?;
        let pids_current = self
            .read_optional_counter(&join_path(&root_path, "pids.current"))
            .await?;
        let pids_max = self
            .read_optional_counter(&join_path(&root_path, "pids.max"))
            .await?;
        let cpu = self
            .read_cpu_stat(&join_path(&root_path, "cpu.stat"))
            .await?;

        Ok(CgroupSnapshot {
            sampled_at_monotonic_ms: (self.now)(),
            memory_current_bytes,
            memory_max_bytes,
            memory_peak_bytes,
            swap_current_bytes,
            pids_current,
            pids_max,
            events,
            cpu,
        })
    }

    /// 對應舊版 `readPolicySnapshot()`：僅讀取高頻 resource guard 決策需要的
    /// counters（舊版註解）。
    pub(crate) async fn read_policy_snapshot(&self) -> Result<CgroupSnapshot, CgroupV2Error> {
        let root_path = self.resolve_root_path().await?;
        let memory_current_bytes = self
            .read_required_counter(&join_path(&root_path, "memory.current"), "memory.current")
            .await?;
        let events = self
            .read_memory_events(&join_path(&root_path, "memory.events"))
            .await?;
        let swap_current_bytes = self
            .read_optional_counter(&join_path(&root_path, "memory.swap.current"))
            .await?;

        Ok(CgroupSnapshot {
            sampled_at_monotonic_ms: (self.now)(),
            memory_current_bytes,
            memory_max_bytes: None,
            memory_peak_bytes: None,
            swap_current_bytes,
            pids_current: None,
            pids_max: None,
            events,
            cpu: None,
        })
    }

    /// 對應舊版 `resolveRootPath`：快取探測結果；失敗時下次重新探測。
    async fn resolve_root_path(&self) -> Result<String, CgroupV2Error> {
        // tokio 的 `get_or_try_init` 在錯誤時保持未初始化，與舊版把
        // rootPathPromise 重設為 undefined 相同。
        self.root_path_cell
            .get_or_try_init(|| self.discover_root_path())
            .await
            .map(|root_path| root_path.to_string())
    }

    /// 對應舊版 `discoverRootPath`。
    async fn discover_root_path(&self) -> Result<String, CgroupV2Error> {
        let direct_memory_current = join_path(&self.sys_fs_cgroup_path, "memory.current");
        if self.exists(&direct_memory_current).await {
            return Ok(self.sys_fs_cgroup_path.clone());
        }

        let relative_path = self.read_cgroup_relative_path().await?;
        let mount_root = self.read_cgroup2_mount_root().await?;
        // 舊版：`path.posix.join(mountRoot, relativePath.replace(/^\//u, ''))`。
        let candidate = posix_join(
            &mount_root,
            relative_path.strip_prefix('/').unwrap_or(&relative_path),
        );
        let candidate_memory_current = join_path(&candidate, "memory.current");
        if !self.exists(&candidate_memory_current).await {
            return Err(CgroupV2Error::Unavailable(format!(
                "cgroup v2 memory.current not found at {candidate_memory_current}"
            )));
        }
        Ok(candidate)
    }

    /// 對應舊版 `readCgroupRelativePath`：從 /proc/self/cgroup 找 v2 層級
    /// （`0::/relative/path`）。
    async fn read_cgroup_relative_path(&self) -> Result<String, CgroupV2Error> {
        let source = self
            .fs
            .read_text(&self.proc_self_cgroup_path)
            .await
            .map_err(|_| {
                CgroupV2Error::Unavailable(format!("unable to read {}", self.proc_self_cgroup_path))
            })?;

        for line in source.split('\n') {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // cgroup v2: 0::/relative/path（舊版樣式 `^0::(.*)$`）。
            if let Some(captured) = trimmed.strip_prefix("0::") {
                let relative = if captured.is_empty() { "/" } else { captured };
                return Ok(if relative.starts_with('/') {
                    relative.to_string()
                } else {
                    format!("/{relative}")
                });
            }
        }

        Err(CgroupV2Error::Unavailable(
            "no cgroup v2 hierarchy entry in /proc/self/cgroup".to_string(),
        ))
    }

    /// 對應舊版 `readCgroup2MountRoot`：從 /proc/self/mountinfo 找 cgroup2 的
    /// 掛載點。
    async fn read_cgroup2_mount_root(&self) -> Result<String, CgroupV2Error> {
        let source = self
            .fs
            .read_text(&self.proc_self_mountinfo_path)
            .await
            .map_err(|_| {
                CgroupV2Error::Unavailable(format!(
                    "unable to read {}",
                    self.proc_self_mountinfo_path
                ))
            })?;

        for line in source.split('\n') {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // mountinfo: ... mountPoint filesystemType ...（舊版註解）
            let Some(separator) = trimmed.find(" - ") else {
                continue;
            };
            let left: Vec<&str> = trimmed[..separator].split(' ').collect();
            let right: Vec<&str> = trimmed[separator + 3..].split(' ').collect();
            if right.first().copied() != Some("cgroup2") {
                continue;
            }
            if let Some(mount_point) = left.get(4).filter(|mount_point| !mount_point.is_empty()) {
                return Ok((*mount_point).to_string());
            }
        }

        Err(CgroupV2Error::Unavailable(
            "no cgroup2 mount in mountinfo".to_string(),
        ))
    }

    /// 對應舊版 `readMemoryEvents`：檔案缺失時回全零（舊版 `DEFAULT_EVENTS`）。
    async fn read_memory_events(
        &self,
        file_path: &str,
    ) -> Result<CgroupMemoryEvents, CgroupV2Error> {
        let Some(source) = self.read_optional_text(file_path).await else {
            return Ok(DEFAULT_EVENTS);
        };

        let mut events = DEFAULT_EVENTS;
        for line in source.split('\n') {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // 舊版：`const [key, rawValue] = trimmed.split(/\s+/u)`。
            let mut parts = trimmed.split_whitespace();
            let (Some(key), Some(raw_value)) = (parts.next(), parts.next()) else {
                continue;
            };
            let Ok(value) = parse_non_negative_big_int(raw_value, &format!("memory.events.{key}"))
            else {
                continue;
            };
            match key {
                "high" => events.high = value,
                "max" => events.max = value,
                "oom" => events.oom = value,
                "oom_kill" => events.oom_kill = value,
                _ => {}
            }
        }

        Ok(events)
    }

    /// 對應舊版 `readCpuStat`：缺檔或缺少 `usage_usec` 時整個 CPU 區塊不可用。
    async fn read_cpu_stat(&self, file_path: &str) -> Result<Option<CgroupCpuStat>, CgroupV2Error> {
        let Some(source) = self.read_optional_text(file_path).await else {
            return Ok(None);
        };

        let mut counters = std::collections::HashMap::new();
        for line in source.split('\n') {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let mut parts = trimmed.split_whitespace();
            let (Some(key), Some(raw_value)) = (parts.next(), parts.next()) else {
                continue;
            };
            // 忽略損壞的可選 CPU counter，避免中斷記憶體防護（舊版註解）。
            if let Ok(value) = parse_non_negative_big_int(raw_value, &format!("cpu.stat.{key}")) {
                counters.insert(key, value);
            }
        }

        let Some(usage_usec) = counters.get("usage_usec").copied() else {
            return Ok(None);
        };
        let user_usec = counters.get("user_usec").copied();
        let system_usec = counters.get("system_usec").copied();
        Ok(Some(CgroupCpuStat {
            usage_usec,
            user_usec,
            system_usec,
        }))
    }

    /// 對應舊版 `readRequiredCounter`：必要檔案；讀取失敗包成
    /// 「failed to read {label}」，解析錯誤原樣上拋。
    async fn read_required_counter(
        &self,
        file_path: &str,
        label: &str,
    ) -> Result<u64, CgroupV2Error> {
        match self.fs.read_text(file_path).await {
            Ok(source) => parse_non_negative_big_int(source.trim(), label),
            Err(_) => Err(CgroupV2Error::Read(format!("failed to read {label}"))),
        }
    }

    /// 對應舊版 `readOptionalCounter`：缺檔、空值或 `max`（memory.max 無上限）
    /// 皆視為不可用；解析失敗同樣視為不可用。
    async fn read_optional_counter(&self, file_path: &str) -> Result<Option<u64>, CgroupV2Error> {
        let Some(source) = self.read_optional_text(file_path).await else {
            return Ok(None);
        };
        let trimmed = source.trim();
        if trimmed.is_empty() || trimmed == "max" {
            // memory.max may be "max" when unlimited — treat as unavailable.
            return Ok(None);
        }
        Ok(parse_non_negative_big_int(trimmed, file_name(file_path)).ok())
    }

    /// 對應舊版 `readOptionalText`：讀不到（任何錯誤）視為無內容。
    async fn read_optional_text(&self, file_path: &str) -> Option<String> {
        self.fs.read_text(file_path).await.ok()
    }

    /// 對應舊版 `exists`。
    async fn exists(&self, file_path: &str) -> bool {
        self.fs.access(file_path).await.is_ok()
    }
}

/// 舊版 `DEFAULT_EVENTS`：memory.events 檔案缺失時的全零計數。
const DEFAULT_EVENTS: CgroupMemoryEvents = CgroupMemoryEvents {
    high: 0,
    max: 0,
    oom: 0,
    oom_kill: 0,
};

/// 舊版預設 `/proc/self/cgroup`。
const DEFAULT_PROC_SELF_CGROUP: &str = "/proc/self/cgroup";
/// 舊版預設 `/proc/self/mountinfo`。
const DEFAULT_PROC_SELF_MOUNTINFO: &str = "/proc/self/mountinfo";
/// 舊版預設 `/sys/fs/cgroup`（只有 Linux 有內容）。
const DEFAULT_SYS_FS_CGROUP: &str = "/sys/fs/cgroup";

/// 舊版 `path.join(root, name)` 的兩參數情形（root 不為空字串）。
fn join_path(base: &str, name: &str) -> String {
    if base.is_empty() {
        return name.to_string();
    }
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// 舊版 `path.posix.join(base, rel)` 的兩參數情形（掛載點與相對路徑）。
fn posix_join(base: &str, relative: &str) -> String {
    if base == "/" {
        return format!("/{relative}");
    }
    join_path(base, relative)
}

/// 舊版 `path.basename(filePath)`。
fn file_name(file_path: &str) -> &str {
    file_path.rsplit('/').next().unwrap_or(file_path)
}

/// 舊版 `parseNonNegativeBigInt`：`/^\d+$/` 之後轉 bigint；負值檢查在 u64 的
/// 界域內不可能為負（樣式已排除符號），超出 u64 的值實務上不會出現，視為
/// 解析錯誤。
fn parse_non_negative_big_int(source: &str, label: &str) -> Result<u64, CgroupV2Error> {
    let is_digits = !source.is_empty() && source.bytes().all(|byte| byte.is_ascii_digit());
    if !is_digits {
        return Err(CgroupV2Error::Read(format!(
            "{label} is not a non-negative integer"
        )));
    }
    source
        .parse::<u64>()
        .map_err(|_| CgroupV2Error::Read(format!("{label} is not a non-negative integer")))
}

/// 舊版預設 `performance.now()`：單調毫秒（以程序內第一次呼叫為原點，僅供
/// 相對比較；u128 截斷為 u64 在實際壽命內不會發生）。與
/// `app/resource_guard.rs` 的版本各有一份程序層級起點，互不影響。
fn monotonic_now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// 舊版 monitor 端對 reader 的 `Pick<CgroupV2Reader, …>` 注入介面：probe 與
/// 兩種快照。正式程式用 [`CgroupV2Reader`]，測試注入假物件。
#[async_trait]
pub(crate) trait CgroupSnapshotSource: Send + Sync {
    async fn probe(&self) -> CgroupReaderAvailability;
    async fn read_snapshot(&self) -> Result<CgroupSnapshot, CgroupV2Error>;
    async fn read_policy_snapshot(&self) -> Result<CgroupSnapshot, CgroupV2Error>;
}

#[async_trait]
impl CgroupSnapshotSource for CgroupV2Reader {
    async fn probe(&self) -> CgroupReaderAvailability {
        CgroupV2Reader::probe(self).await
    }

    async fn read_snapshot(&self) -> Result<CgroupSnapshot, CgroupV2Error> {
        CgroupV2Reader::read_snapshot(self).await
    }

    async fn read_policy_snapshot(&self) -> Result<CgroupSnapshot, CgroupV2Error> {
        CgroupV2Reader::read_policy_snapshot(self).await
    }
}

#[cfg(test)]
mod tests;
