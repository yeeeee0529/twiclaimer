# A/B 比較報告

- A：待測（舊版 24 小時；兼作 S0 基準補測 S0.1／S0.2／S0.4／S0.6）
- B：Rust 版 `twiclaimer:local`（commit 3341fea，S5.4），日誌窗 2026-11-04T11:21:09Z–2026-11-05T05:30:23Z（可用窗口約 18 小時；開頭約 6 小時被 log rotation 吃掉，`config_loaded`／`service_started` 已遺失）
- 同一台主機、同一份設定；設定摘要：`log_level: debug`、`resource_telemetry_interval_seconds: 300`、`max_concurrent_streams: 3`、設定頻道 18 個。設定差異：無
- 比較時以兩側可用窗口對齊（B 約 18 小時），並於結果註明
- 日誌 ab-logs-*.jsonl 不進 repo（匯出於 repo 外層 `ab-export/`）

## 結果

| 需求 | A（舊版） | B（Rust） | 通過條件 | 結果 |
| --- | --- | --- | --- | --- |
| R1 控制端 RSS 中位數 | 待測 | 依頁數 0／1／2／3：56／55／52／36 MiB | ≤ 60 MiB | B 過關；整體待 A |
| R1 整容器記憶體中位數（各頁數） | 待測 | 0 頁 236 MiB／1 頁 630 MiB／2 頁 1,201 MiB／3 頁 1,500 MiB | ≤ A 的 105% | 待 A |
| R2 控制端平均 CPU | 待測 | 0.000–0.004 核（依頁數） | 低於 A | 待 A |
| R2 整容器 CPU | 待測 | 0 頁 0.006／1 頁 0.824／2 頁 1.327／3 頁 1.515 核 | ≤ A 的 105% | 待 A |
| R3 image 大小 | 待測 | 277,135,176 bytes（`docker save` 後 gzip -1 壓縮位元組數） | ≤ A 的 1/3 | 待 A |
| R6 Bot 指令與通知 | 待人工 | 見觀察 3：14:53 兩次指令逾時、18:19 截圖失敗 | 與舊版相同 | 待人工 |
| R7 領取、RestartCount、OOMKilled、headless shell 程序數 | 待測 | `reward_claimed` 170、`reward_claim_failed` 1（teardown 競態）、`page_crashed` 16（15/16 於 2 分鐘內重啟並恢復領取）、RestartCount 0、OOMKilled false、`browser_restarted` 0、`container_restart_requested` 0、`scheduler_stall_detected` 0、`resource_guard_warning` 0、OOM kill 計數全 0 | 有領取、無 OOM、不增加、每次 1 個 | B 初步過關；headless shell 程序數與 chromium 參數待重抓（觀察 6） |
| R8 設定寫回 | 待人工 | | 只有兩個值的文字改變 | 待人工 |
| R9、R10 log-schema.py | 待測 | B 側 schema 已產出（47 種事件；`ab-export/log-schema-b.tsv`） | 只有「允許」，或每項都有說明 | 待 A |

## 差異說明（B 側觀察，待 A 側對照）

1. **yawnii_aelis 12:32 崩潰後約 2 小時 52 分未回歸觀看**：崩潰當下 recovery 被 30 秒 backoff defer（`retryAtMs` 12:32:42），但 12:33:21 nemesisxdfp 上線，依 config 順序遞補名額，yawnii_aelis 被擠出 selector 前 3 名；直到 15:24 xiangtsai51121 下線釋出名額，5 秒內 probe＋重啟成功（attempt 1、5.7 秒）並隨即恢復領取。舊版 `SessionManager.ts` 的 reconcile 同樣只對 desiredChannels 內的頻道呼叫 `beginRecoveryProbe`（第 190–203 行），selector 亦為純 config 順序——**兩版共有語意，非移植回歸**。
2. **`telegram_send_failed` ×3**（13:20:04、15:17:58、20:05:24）：皆發生在 `reward_claimed` 後數秒，為領取通知發送；`errorCategory: network`、`httpStatus: null`（連線層級錯誤）。兩版 `broadcast` 語意相同：失敗只記 log、不重試、不中斷迴圈——3 則通知丟失（nemesisxdfp 13:20、koyuki_teraz 15:17、yuzumi_neon 20:05）。可對照 Telegram 聊天紀錄確認。
3. **`telegram_query_timeout` ×2**（14:53:25 `points`、14:53:44 `screenshot`）與 **`session_screenshot_failed` ×1**（18:19:27 nemesisxdfp「頻道頁面目前無法截圖」，同毫秒有 `session_page_operation_drain_timeout`，該頻道 18:19:22 剛下線）：時間點疑似人工測試；若為 R6 測試，該幾次指令逾時／失敗，需擇期重測。
4. **`page_health_failed` ×2**（`url_mismatch`、`consecutiveFailures: 1`）：皆自行恢復（其一 70 秒後頻道即下線）。
5. **`page_refresh_failed` ×3**：2 次 `page.reload: Target crashed`（崩潰當下，隨後 crash 流程接手）、1 次 reload 30 秒逾時。皆未造成領取中斷。
6. **`chromium-args-rust.txt` 為空**：S0.3 抓取 pattern（`*headless_shell*|*chrome-headless-shell*`）匹配不到容器內實際路徑 `/opt/headless-shell/headless-shell`（Dockerfile symlink，連字號）——已修正計畫與 pattern 為 `*headless_shell*|*headless-shell*`（DEC-027）。需在 Rust 容器有活躍頁面時重抓。
7. 崩潰集中於 11:23–19:04（多頻道同時直播時段，live 4–7 個），19:04 後至窗末零崩潰；16 次崩潰分散於 7 個頻道，無 crash loop。`browserGeneration` 全程為 1（瀏覽器未重啟）。

## 建議

- 待 A 側資料齊後填寫。
- 人工決定（日期、決定者）：
