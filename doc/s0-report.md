# S0 基準報告

- 舊版 commit：e0e079a87f06d5608a9e96ff43de2efc8c53cd23
- 觀察期間：YYYY-MM-DD HH:MM – YYYY-MM-DD HH:MM（日誌 s0-logs-*.jsonl 不進 repo）
- 主機：Docker 版本、cgroup 版本、架構（S0.2）
- APP_UID／APP_GID：（S0.2）
- 設定摘要：頻道數、max_concurrent_streams、資源阻擋、Bot 啟用狀況（不含 token）

## 資源（snapshot_stats.py）

| 頁數 | 樣本 | 控制端 RSS 中位數 | 整容器記憶體中位數 | RAM 占比 | 控制端 CPU | 整容器 CPU | CPU 占比 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| | | | | | | | |

## 事件次數

| 事件 | 次數 |
| --- | --- |
| reward_claimed | |
| browser_restarted | |
| browser_disconnected | |
| container_restart_requested | |
| scheduler_stall_detected | |

## 狀態

- RestartCount／OOMKilled（S0.5）：
- docker stats（S0.5）：
- image 大小（S0.1，gzip -1 後位元組數）：
- 日誌結構：doc/spec/log-schema-s0.tsv 的事件數、非 JSON 行數
- chromium-args-prod.txt 與 chromium-args.txt 的差異（S1.2 後補）：

## Go／No-Go

- 建議：Go 或 No-Go
- 依據：控制端 RSS 中位數 ___ MiB（條件：≥ 100 MiB，或使用者仍要 R3）
- 人工決定（日期、決定者）：
