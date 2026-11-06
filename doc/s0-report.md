# S0 基準報告

- 舊版 commit：e0e079a87f06d5608a9e96ff43de2efc8c53cd23（正式主機 `twitch-watchdog:local` image 即此基準；A/B 的 A 側運行兼作本報告資料來源）
- 觀察期間：2026-11-05 13:42 – 2026-11-06 17:53（Asia/Taipei；UTC 2026-11-05T05:42:05Z–11-06T09:53:44Z，28.2 小時；日誌 s0-logs-1.jsonl 不進 repo，匯出於 repo 外層 ab-export/）
- 主機：Docker 29.9.2、cgroup v2、x86_64（abtserver-debian，192.168.1.134）
- APP_UID／APP_GID：1001／1001
- 設定摘要：頻道 18 個、`max_concurrent_streams: 3`、`resource_telemetry_interval_seconds: 300`（生效節奏實測 300 秒）、`log_level: info`（註：與 B 側的 debug 不同，使用者裁示不重跑，詳 doc/ab-report.md 差異說明 1）、Telegram/Discord 啟用（token 不記錄）

## 資源（snapshot_stats.py）

| 頁數 | 樣本 | 控制端 RSS 中位數 | 整容器記憶體中位數 | RAM 占比 | 控制端 CPU | 整容器 CPU | CPU 占比 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 0 | 103 | 456 MiB | 601 MiB | 75.8% | 0.002 核 | 0.008 核 | 26.7% |
| 1 | 107 | 457 MiB | 1,002 MiB | 45.6% | 0.014 核 | 0.851 核 | 1.6% |
| 2 | 34 | 459 MiB | 1,448 MiB | 31.7% | 0.020 核 | 1.365 核 | 1.4% |
| 3 | 95 | 425 MiB | 1,907 MiB | 22.3% | 0.026 核 | 1.542 核 | 1.7% |

- S0 建議條件「控制端 RSS 中位數 ≥ 100 MiB」成立（425–459 MiB）——Rust 移植的記憶體動機成立，R3（image）亦然

## 事件次數

| 事件 | 次數 |
| --- | --- |
| reward_claimed | 429 |
| browser_restarted | 0 |
| browser_disconnected | 0 |
| container_restart_requested | 0 |
| scheduler_stall_detected | 0 |

- 補充：`page_crashed` 23 次（23/23 於 35–174 秒重啟並恢復領取，`browserGeneration` 全程 1）、`reward_claim_failed` 1（teardown 競態）、`telegram_poll_failed` 15／`telegram_send_failed` 3（環境網路抖動）

## 狀態

- RestartCount／OOMKilled（S0.5）：0 ／ false
- docker stats（S0.5，2026-11-06 17:5x CST 當下瞬值）：CPU 270.74%、記憶體 1.347 GiB／6 GiB（22.45%）
- image 大小（S0.1，gzip -1 後位元組數）：946,571,018
- 日誌結構：doc/spec/log-schema-s0.tsv（23 種事件；info 級可見子集，debug 級事件因 log_level 不出現）
- Chromium 參數（S0.3，修正後 pattern 抓取）：45 個參數，與 doc/spec/chromium-args.txt 逐字一致（含 `--remote-debugging-pipe`）；headless shell 主程序數 1（`--type=` 子程序 8）
