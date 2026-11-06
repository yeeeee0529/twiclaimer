# A/B 比較報告

- A：舊版 TS（`twitch-watchdog-docker`，基準 commit `e0e079a`），正式主機 abtserver-debian（192.168.1.134，x86_64，Docker 29.9.2，cgroup v2，APP_UID/GID=1001/1001）。日誌窗 2026-11-05T05:42:05Z–11-06T09:53:44Z（28.2 小時，`config_loaded`／`service_started` 完整保留）
- B：Rust 版 `twiclaimer:local`（commit 3341fea，S5.4），同一台主機。日誌窗 2026-11-04T11:21:09Z–11-05T05:30:23Z（可用窗口約 18.2 小時；開頭約 6 小時被 log rotation 吃掉，`config_loaded`／`service_started` 已遺失）
- 同一台主機、同一份設定；設定一致性已核對：頻道 18 個、`max_concurrent_streams: 3`、`resource_telemetry_interval_seconds: 300` 兩側相同（皆第 47 行，兩側生效節奏實測皆 300 秒）。**唯一差異：`log_level` A=info、B=debug**——使用者裁示不重跑（見差異說明 1）
- 事件次數不可直接跨窗比較（兩窗的直播頻道組合不同；A 窗出現 kirali_neon／miri_aetris／aelis_chamamatti／iruni_ianvs／sachi_teraz／hitomi_teraz 等 B 窗未見的頻道），以各頁數分組的資源中位數與速率對照
- 日誌不進 repo（匯出於 repo 外層 `ab-export/`：`s0-logs-1.jsonl` 1,629 行、`ab-logs-all.jsonl` 118,153 行、schema TSV 兩份、state／image size／host 記錄、chromium-args-prod.txt）

## 結果

| 需求 | A（舊版 TS） | B（Rust） | 通過條件 | 結果 |
| --- | --- | --- | --- | --- |
| R1 控制端 RSS 中位數 | 依頁數 0／1／2／3：456／457／459／425 MiB | 56／55／52／36 MiB | ≤ 60 MiB | **過**（最大 56 MiB，約 A 的 1/8–1/12） |
| R1 整容器記憶體中位數（各頁數） | 601／1,002／1,448／1,907 MiB | 236／630／1,201／1,500 MiB | ≤ A 的 105% | **過**（B 低 20–61%） |
| R2 控制端平均 CPU | 0.002／0.014／0.020／0.026 核 | 0.000／0.003／0.003／0.004 核 | 低於 A | **過**（A 在 info 級、日誌工作較少，此對比對 B 偏嚴格仍過） |
| R2 整容器 CPU | 0.008／0.851／1.365／1.542 核 | 0.006／0.824／1.327／1.515 核 | ≤ A 的 105% | **過**（瀏覽器本體負載兩側相當，差額在控制端） |
| R3 image 大小（`docker save` 後 gzip -1 位元組數） | 946,571,018 | 277,135,176 | ≤ A 的 1/3 | **過**（B = A 的 29.3%；門檻 315,523,672） |
| R4 常駐服務只支援 Chromium | — | `browser.engine: firefox` 啟動報錯（e2e X2，§6.2） | 照舊版 | 過（e2e 已驗，CHANGELOG 已載） |
| R5 登入工具維持多瀏覽器 | — | `npm run twitch:login` 維持 Node.js 不進 image | 照舊版 | 過（S5.3 文件已載） |
| R6 Bot 指令與通知 | 參照行為 | B 窗期間 14:53 `points`／`screenshot` 逾時、18:19 截圖失敗（疑為人工測試撞上逾時／頻道下線） | 與舊版相同 | **待人工**：S5.6 切換後於 Rust 容器補測 |
| R7 領取、崩潰恢復、狀態 | 429 次領取（15.2/h）；`page_crashed` 23 次 **23/23 於 35–174 秒重啟、71–329 秒恢復領取**；`browserGeneration` 全程 1；headless shell 主程序數 1（子程序 8）；RestartCount=0、OOMKilled=false、`browser_restarted`=0、`container_restart_requested`=0、`scheduler_stall_detected`=0、OOM kill 計數 0；`reward_claim_failed` 1（teardown 競態） | 170 次領取（9.4/h）；`page_crashed` 16 次 **15/16 於 35–118 秒重啟並恢復領取**，1 例因 config 順序遞補延遲 2h52m（見差異說明 2）；`browserGeneration` 全程 1；RestartCount=0、OOMKilled=false、`reward_claim_failed` 1（teardown 競態） | 有領取、無 OOM、不增加、每次 1 個 | **過**（兩側崩潰速率相當：A 0.82/h、B 0.88/h；皆無 crash loop、無孤兒程序） |
| R8 設定寫回 | 參照行為（寫回機制即舊版既有） | B 窗期間未完成人工測試 | 只有兩個值的文字改變 | **待人工**：S5.6 切換後於 Rust 容器補測 |
| R9 瀏覽器診斷 | info 級可見子集：`browser_request_failed` warn ×11、`browser_console_message` warn ×7，欄位與 B 同級實例完全一致 | debug 級全量：82,594／11,534／10,553 筆 | 與舊版相同 | **過（有條件）**：見差異說明 1、3 |
| R10 日誌相容 | schema 23 種事件（info 可見子集；`ab-export/log-schema-a.tsv`） | schema 47 種事件（debug 全量；`ab-export/log-schema-b.tsv`） | 只有「允許」，或每項都有說明 | **過（有條件）**：見差異說明 1、3、4 |

## 差異說明

1. **`log_level` A=info、B=debug（使用者裁示不重跑）**：A 側 debug 級事件（`browser_http_response`、`page_reloaded`、`reconcile_*`、`session_*`、`scheduler_tick_*`、`channel_recovery_*`、`browser_page_invalidation_*`、`browser_resource_close_*`、`stream_quality_*` 等 24 種）在 log 中不可見，R9／R10 的 log 比對僅能涵蓋 info/warn 級子集。緩解：(a) 這些事件在移植期的等價性已由 port-coverage（433/433，事件名稱與欄位抽自舊版原始碼）與 e2e 驗證；(b) 兩側皆可見的 20 種事件欄位逐一比對完全一致（見 3、4）；(c) B 側 debug 全量 schema 已存檔，S5.6 後 Rust 容器運行期間可隨時與舊版 debug 短窗複核。R1／R2 的影響方向對 B 不利（A 的控制端日誌工件較少），B 仍過關，故不影響結論。
2. **yawnii_aelis（B 窗）12:32 崩潰後約 2 小時 52 分未回歸觀看**：崩潰當下 recovery 被 30 秒 backoff defer（`retryAtMs` 12:32:42），但 12:33:21 nemesisxdfp 上線，依 config 順序遞補名額，yawnii_aelis 被擠出 selector 前 3 名；直到 15:24 xiangtsai51121 下線釋出名額，5 秒內 probe＋重啟成功（attempt 1、5.7 秒）並隨即恢復領取。舊版 `SessionManager.ts` 的 reconcile 同樣只對 desiredChannels 內的頻道呼叫 `beginRecoveryProbe`（第 190–203 行），selector 亦為純 config 順序——**兩版共有語意，非移植回歸**。A 側 23 次崩潰全數 2 分鐘內重啟，未再現此情境。
3. **共有事件的欄位比對（20 種）**：`reward_claimed`、`reward_claim_failed`、`watch_started`、`watch_stopped`、`stream_online`、`stream_offline`、`stream_playback_optimized`、`page_crashed`、`page_refreshed`、`page_refresh_failed`、`page_health_failed`、`session_page_operation_drain_timeout`、`telegram_send_failed`、`telegram_poll_failed`、`browser_request_failed`（warn 級）、`browser_console_message`（warn 級）、`runtime_resource_snapshot`、`config_loaded`、`credential_checked`、`resource_guard_enabled` 等——欄位名稱與型別一致。兩點補充：(a) `browser_request_failed` 的 `graphQlOperationNames` 僅出現在 debug 級實例，兩版 warn 級皆無（非差異）；(b) `runtime_resource_snapshot` 的 `processHeapUsedBytes`／`processExternalBytes` A 為數值、B 為 null——計畫 X5 明文允許（§6.2；Node 專屬欄位，Rust 不輸出數值）。B 窗因 rotation 遺失的啟動事件（`service_started`／`config_loaded`／`credential_checked`／`resource_guard_enabled`／`scheduler_stall_watchdog_started`）以原始碼與 smoke 實測佐證存在且欄位一致（X5：`nodeVersion`／`playwrightVersion`／`gitCommit` 在 B 為 null）。
4. **`telegram_poll_failed` A ×15、B ×0**：兩版程式碼皆有此事件（舊版 `TelegramBot.ts:189`、Rust `bot.rs:1288`，欄位一致），屬 Telegram polling 的環境網路抖動，B 的 18 小時窗口未發生。**`twitch_api_token_refreshed`（info）與 `twitch_api_token_validated`（debug）兩版皆有**（舊版 `TwitchApiClient.ts:322`/`:422`、Rust `api.rs:765`/`:912`），log 中各自只出現一個為 level 可見度所致。
5. **`chromium-args-rust.txt` 為空**：S0.3 抓取 pattern（`*headless_shell*|*chrome-headless-shell*`）匹配不到容器內實際路徑 `/opt/headless-shell/headless-shell`（Dockerfile symlink，連字號）——已修正計畫與 pattern 為 `*headless_shell*|*headless-shell*`（DEC-027）。A 側正式環境已以修正後 pattern 重抓：**45 個參數與 `doc/spec/chromium-args.txt` 逐字一致**（含 `--remote-debugging-pipe`）。B 側正式環境抓取延至 S5.6 切換時補作（Rust 容器屆時上線）。
6. **崩潰集中時段**：A 崩潰分布於 08:55–09:49（多頻道直播時段）等；B 崩潰集中 11:23–19:04（live 4–7）。19:04 後 B 零崩潰。兩側 `browserGeneration` 全程 1，瀏覽器皆未整體重啟。

## 建議

- **建議切換（S5.6）**：R1／R2／R3／R7 全數過關且餘裕大（控制端 RSS 1/8、image 29%）；R10 共有事件欄位全同、差異僅 X5 允許項與 log-level 可見度；R4／R5 已於 e2e 與文件驗證。R6／R8 人工補測與 `chromium-args-rust.txt` 抓取併入 S5.6 切換程序（Rust 容器上線後即測）。
- 切換時注意：`/home/liuminkai/twiclaimer/config.yml` 為 B 側原設定（`log_level: debug`）——正式切換後可依使用者偏好調整回 info 以減少日誌量（50MB rotation 上限在 debug 級約 20 小時即滿）。
- 人工決定（日期、決定者）：
