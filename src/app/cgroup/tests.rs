//! CgroupV2Reader 的單元測試：移植舊版 `test/unit/cgroup-v2-reader.test.ts`
//! 的 7 個測試。以 `tempfile` 假檔案樹（或記憶體對照表）餵資料，不讀真的
//! `/sys/fs/cgroup`。
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use super::{
    CgroupFileSystem, CgroupMemoryEvents, CgroupReaderAvailability, CgroupSnapshot, CgroupV2Error,
    CgroupV2Reader, CgroupV2ReaderOptions,
};

/// 對應舊測試 helper `createFixtureRoot()`：tempfile 的暫存目錄（離開範圍自動
/// 清理）。
fn create_fixture_root() -> tempfile::TempDir {
    tempfile::tempdir().unwrap_or_else(|error| panic!("建立暫存目錄失敗：{error}"))
}

/// 舊測試 1 的 `CgroupFileSystem`：記憶體對照表 + 讀取記錄（basename）。
struct MapFileSystem {
    files: HashMap<String, String>,
    reads: Arc<Mutex<Vec<String>>>,
}

impl MapFileSystem {
    fn new(files: &[(&str, &str)], reads: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            files: files
                .iter()
                .map(|(path, content)| (path.to_string(), content.to_string()))
                .collect(),
            reads,
        }
    }
}

#[async_trait::async_trait]
impl CgroupFileSystem for MapFileSystem {
    async fn read_text(&self, path: &str) -> io::Result<String> {
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        self.reads.lock().unwrap().push(name);
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::other("missing fixture"))
    }

    async fn access(&self, path: &str) -> io::Result<()> {
        if self.files.contains_key(path) {
            Ok(())
        } else {
            Err(io::Error::other("missing fixture"))
        }
    }
}

fn sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values
}

// 原測試：policy snapshot 只讀三個 guard counter，full snapshot 才讀完整欄位
#[tokio::test]
async fn policy_snapshot_reads_only_three_guard_counters() {
    let root = "/test/cgroup";
    let files: Vec<(String, String)> = vec![
        (format!("{root}/memory.current"), "100\n".to_string()),
        (
            format!("{root}/memory.events"),
            "high 1\nmax 0\noom 0\noom_kill 0\n".to_string(),
        ),
        (format!("{root}/memory.swap.current"), "5\n".to_string()),
        (format!("{root}/memory.max"), "1000\n".to_string()),
        (format!("{root}/memory.peak"), "200\n".to_string()),
        (format!("{root}/pids.current"), "10\n".to_string()),
        (format!("{root}/cpu.stat"), "usage_usec 50\n".to_string()),
    ];
    let reads = Arc::new(Mutex::new(Vec::new()));
    let fs = MapFileSystem::new(
        &files
            .iter()
            .map(|(path, content)| (path.as_str(), content.as_str()))
            .collect::<Vec<_>>(),
        Arc::clone(&reads),
    );
    let reader = CgroupV2Reader::new(CgroupV2ReaderOptions {
        fs: Some(Arc::new(fs)),
        sys_fs_cgroup_path: Some(root.to_string()),
        now: Some(Arc::new(|| 123)),
        ..CgroupV2ReaderOptions::default()
    });

    let policy = reader.read_policy_snapshot().await.unwrap();
    assert_eq!(
        policy,
        CgroupSnapshot {
            sampled_at_monotonic_ms: 123,
            memory_current_bytes: 100,
            memory_max_bytes: None,
            memory_peak_bytes: None,
            swap_current_bytes: Some(5),
            pids_current: None,
            pids_max: None,
            events: CgroupMemoryEvents {
                high: 1,
                max: 0,
                oom: 0,
                oom_kill: 0,
            },
            cpu: None,
        }
    );
    assert_eq!(
        sorted(reads.lock().unwrap().clone()),
        vec![
            "memory.current".to_string(),
            "memory.events".to_string(),
            "memory.swap.current".to_string(),
        ]
    );

    reads.lock().unwrap().clear();
    reader.read_snapshot().await.unwrap();
    assert_eq!(
        sorted(reads.lock().unwrap().clone()),
        vec![
            "cpu.stat".to_string(),
            "memory.current".to_string(),
            "memory.events".to_string(),
            "memory.max".to_string(),
            "memory.peak".to_string(),
            "memory.swap.current".to_string(),
            "pids.current".to_string(),
            "pids.max".to_string(),
        ]
    );
}

// 原測試：從 cgroup 根目錄讀取 cpu/memory/swap/pids/events
#[tokio::test]
async fn reads_cpu_memory_swap_pids_events_from_cgroup_root() {
    let root = create_fixture_root();
    let root_path = root.path().to_string_lossy().to_string();
    std::fs::write(root.path().join("memory.current"), "123456789\n").unwrap();
    std::fs::write(root.path().join("memory.max"), "6442450944\n").unwrap();
    std::fs::write(root.path().join("memory.peak"), "234567890\n").unwrap();
    std::fs::write(root.path().join("memory.swap.current"), "111\n").unwrap();
    std::fs::write(root.path().join("pids.current"), "42\n").unwrap();
    std::fs::write(root.path().join("pids.max"), "512\n").unwrap();
    std::fs::write(
        root.path().join("cpu.stat"),
        "usage_usec 987654\nuser_usec 700000\nsystem_usec 287654\nnr_periods 12\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("memory.events"),
        "low 0\nhigh 2\nmax 1\noom 3\noom_kill 4\nunknown 9\n",
    )
    .unwrap();

    let reader = CgroupV2Reader::new(CgroupV2ReaderOptions {
        sys_fs_cgroup_path: Some(root_path.clone()),
        now: Some(Arc::new(|| 1_000)),
        ..CgroupV2ReaderOptions::default()
    });

    assert_eq!(
        reader.probe().await,
        CgroupReaderAvailability::Available { root_path }
    );

    let snapshot = reader.read_snapshot().await.unwrap();
    assert_eq!(
        snapshot,
        CgroupSnapshot {
            sampled_at_monotonic_ms: 1_000,
            memory_current_bytes: 123_456_789,
            memory_max_bytes: Some(6_442_450_944),
            memory_peak_bytes: Some(234_567_890),
            swap_current_bytes: Some(111),
            pids_current: Some(42),
            pids_max: Some(512),
            cpu: Some(super::CgroupCpuStat {
                usage_usec: 987_654,
                user_usec: Some(700_000),
                system_usec: Some(287_654),
            }),
            events: CgroupMemoryEvents {
                high: 2,
                max: 1,
                oom: 3,
                oom_kill: 4,
            },
        }
    );
}

// 原測試：cpu.stat 缺少或 usage_usec 格式錯誤時視為不可用
#[tokio::test]
async fn cpu_stat_with_invalid_usage_usec_is_unavailable() {
    let root = create_fixture_root();
    std::fs::write(root.path().join("memory.current"), "100\n").unwrap();
    std::fs::write(
        root.path().join("cpu.stat"),
        "usage_usec invalid\nuser_usec 10\n",
    )
    .unwrap();

    let snapshot = CgroupV2Reader::new(CgroupV2ReaderOptions {
        sys_fs_cgroup_path: Some(root.path().to_string_lossy().to_string()),
        ..CgroupV2ReaderOptions::default()
    })
    .read_snapshot()
    .await
    .unwrap();

    assert_eq!(snapshot.cpu, None);
}

// 原測試：memory.max 為 max 時視為不可用
#[tokio::test]
async fn memory_max_with_max_value_is_unavailable() {
    let root = create_fixture_root();
    std::fs::write(root.path().join("memory.current"), "100\n").unwrap();
    std::fs::write(root.path().join("memory.max"), "max\n").unwrap();
    std::fs::write(
        root.path().join("memory.events"),
        "high 0\nmax 0\noom 0\noom_kill 0\n",
    )
    .unwrap();

    let snapshot = CgroupV2Reader::new(CgroupV2ReaderOptions {
        sys_fs_cgroup_path: Some(root.path().to_string_lossy().to_string()),
        now: Some(Arc::new(|| 0)),
        ..CgroupV2ReaderOptions::default()
    })
    .read_snapshot()
    .await
    .unwrap();

    assert_eq!(snapshot.memory_max_bytes, None);
}

// 原測試：缺少 memory.current 時標記為不可用
#[tokio::test]
async fn missing_memory_current_marks_unavailable() {
    let root = create_fixture_root();
    let root_path = root.path().to_string_lossy().to_string();
    let probe = CgroupV2Reader::new(CgroupV2ReaderOptions {
        sys_fs_cgroup_path: Some(root_path),
        proc_self_cgroup_path: Some(
            root.path()
                .join("missing-cgroup")
                .to_string_lossy()
                .to_string(),
        ),
        proc_self_mountinfo_path: Some(
            root.path()
                .join("missing-mountinfo")
                .to_string_lossy()
                .to_string(),
        ),
        ..CgroupV2ReaderOptions::default()
    })
    .probe()
    .await;

    assert!(matches!(
        probe,
        CgroupReaderAvailability::Unavailable { .. }
    ));
}

// 原測試：可透過 mountinfo + /proc/self/cgroup 解析命名空間路徑
#[tokio::test]
async fn resolves_namespaced_path_via_mountinfo_and_proc_self_cgroup() {
    let root = create_fixture_root();
    let mount_root = root.path().join("mount");
    let relative = "/docker/abc";
    let cgroup_dir = mount_root.join("docker/abc");
    std::fs::create_dir_all(&cgroup_dir).unwrap();
    std::fs::write(cgroup_dir.join("memory.current"), "50\n").unwrap();
    std::fs::write(
        cgroup_dir.join("memory.events"),
        "high 0\nmax 0\noom 0\noom_kill 0\n",
    )
    .unwrap();

    let empty_sys = root.path().join("sys-empty");
    std::fs::create_dir(&empty_sys).unwrap();

    let proc_cgroup = root.path().join("cgroup");
    let proc_mountinfo = root.path().join("mountinfo");
    std::fs::write(&proc_cgroup, format!("0::{relative}\n")).unwrap();
    std::fs::write(
        &proc_mountinfo,
        format!(
            "1 0 0:0 / {} rw - cgroup2 cgroup2 rw\n",
            mount_root.to_string_lossy()
        ),
    )
    .unwrap();

    let reader = CgroupV2Reader::new(CgroupV2ReaderOptions {
        sys_fs_cgroup_path: Some(empty_sys.to_string_lossy().to_string()),
        proc_self_cgroup_path: Some(proc_cgroup.to_string_lossy().to_string()),
        proc_self_mountinfo_path: Some(proc_mountinfo.to_string_lossy().to_string()),
        now: Some(Arc::new(|| 5)),
        ..CgroupV2ReaderOptions::default()
    });

    let snapshot = reader.read_snapshot().await.unwrap();
    assert_eq!(snapshot.memory_current_bytes, 50);
    assert_eq!(snapshot.sampled_at_monotonic_ms, 5);
}

// 原測試：memory.current 格式錯誤時拋出讀取錯誤
#[tokio::test]
async fn invalid_memory_current_raises_read_error() {
    let root = create_fixture_root();
    std::fs::write(root.path().join("memory.current"), "not-a-number\n").unwrap();

    let outcome = CgroupV2Reader::new(CgroupV2ReaderOptions {
        sys_fs_cgroup_path: Some(root.path().to_string_lossy().to_string()),
        ..CgroupV2ReaderOptions::default()
    })
    .read_snapshot()
    .await;

    assert_eq!(
        outcome,
        Err(CgroupV2Error::Read(
            "memory.current is not a non-negative integer".to_string()
        ))
    );
}
