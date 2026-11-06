# twiclaimer 開發者文件

本文件提供本機開發、測試與維護資訊。使用者部署流程請見 [README.md](README.md) 與 [完整說明](docs/zh-TW/README.md)。

登入異常與健康檢查重試的行為，見 [使用者文件的疑難排解](docs/zh-TW/README.md#疑難排解)。

## 專案結構

- `src/config`：YAML 設定（yaml 2.9.0 相容語意）、環境變數覆寫、runtime config 持久化（位元組範圍寫回）。
- `src/credentials`：storageState 與 Twitch API 設定檢查。
- `src/twitch`：Twitch Helix API client 與 live status provider。
- `src/browser`：精簡 CDP client（`--remote-debugging-pipe`，初始化順序與錯誤訊息對齊 Playwright）、觀看 session、畫質最佳化與 Bonus Points 領取；含 Twitch 內容警示確認閘。
- `src/logging`：JSON Lines logger（欄位順序、遮罩規則、JS 數字格式與舊版相同）。
- `src/sessions`：多頻道 session reconcile（`SessionManager`／`ReconcileCoordinator`）。
- `src/scheduler`：輪詢排程、stream selection 與不可重入控制（`WatchdogScheduler`）。
- `src/telegram`：Telegram Bot API（reqwest 長輪詢）與指令處理。
- `src/discord`：Discord REST API 與 slash command 處理；gateway 以 twilight `Shard` 啟動（不開 cache）。
- `src/notifications`：啟停與崩潰通知訊息、bot 指令上下文介面。
- `src/app`：composition root（`createApplication`）、啟停順序（`AppRunner`）、signal handler、程序層 panic hook 與 runtime resource snapshot。
- `src/tests`：跨模組 integration 測試（watchdog flow、browser invalidation、startup prerequisites、多頁 e2e 等）。
- `test-node/`：保留 Node 的單元測試（default browser、CSV 欄位）與 Docker smoke 輔助檔（`docker/`）。
- `testdata/`：設定 golden（由舊版實作產生）。
- `tools/ref/`：對照舊版實作的規格與錄製工具（`extract-spec.sh`、`record-traces.sh`、`compare-traces.sh` 等）。
- `scripts`：維護與觀察用腳本。
  - `scripts/twitch-login.mjs`：本機互動式 Twitch 登入輔助（偵測預設瀏覽器、臨時 profile、匯出 storageState；維持 Node 版）。
  - `scripts/lib/default-browser.mjs`：預設瀏覽器偵測與偏好順序 helper。
  - `scripts/check.sh`：每步驟驗證（fmt、clippy、測試、e2e、npm、port-coverage）。
  - `scripts/fetch-headless-shell.sh`：下載 Chromium headless shell 並核對 `chrome/SHA256SUMS`。

`doc/` 內的計畫、決策（`decisions.md`）、進度（`progress.md`）與規格（`spec/`）是開發用途文件，不是使用者操作手冊。

## 本機需求

- Rust 1.99.1（`rust-toolchain.toml`；rustup 會自動安裝，含 rustfmt 與 clippy）。
- Node.js 24 以上與 npm（僅供登入工具、CSV 匯出與 `test-node/` 測試）。
- Docker Engine 與 Docker Compose plugin。
- e2e 測試需要 Chromium headless shell：`export CHROME_PATH="$(scripts/fetch-headless-shell.sh)"`。

安裝 Node 工具依賴：

```bash
npm ci
```

## 常用命令

```bash
cargo build --release --locked
./target/release/twiclaimer --version
scripts/check.sh                       # fmt + clippy -D warnings + 全量測試 + （CHROME_PATH 有值時）e2e + npm test + port-coverage
cargo test --locked                    # 單元＋integration（不含 e2e）
cargo clippy --all-targets --locked -- -D warnings
npm test                               # test-node 的兩個保留測試
npm run twitch:login
npm run benchmark:csv
scripts/port-coverage.sh --strict      # 舊測試標題覆蓋率（嚴格模式）
```

本機執行已建置版本：

```bash
cargo build --release --locked
CONFIG_PATH="$PWD/config.yml" ./target/release/twiclaimer
```

`twiclaimer probe` 子命令用於 CDP 錄製比對（G2／G3），用法見 `tools/ref/record-traces.sh`。

## Docker 驗證

建置 production image：

```bash
docker compose build
```

執行完整驗證（test stage 內含 `REQUIRE_E2E=1 scripts/check.sh`）：

```bash
docker build --target test .
```

執行 smoke test：

```bash
./scripts/docker-smoke.sh
```

Smoke test 會驗證 Compose 合併結果、build test stage 與 production image、檢查 image 只含 `twiclaimer`＋headless shell＋`config.example.yml`（不含 `config.yml`、`.env`、storageState、原始碼或測試）、缺設定以 `config_error` 失敗、smoke override 啟動、cgroup 限制生效、restart／SIGTERM 的 `service_stopped`、無殘留 headless shell 程序，並輸出 image 大小。測試使用假憑證與 `network_mode: none`，不會連線 Twitch API 或正式 Twitch 網站。

macOS managed sandbox 可能阻擋本機瀏覽器程序；若 e2e 在本機失敗，優先以 Docker smoke 或 Linux 環境驗證。

## 測試策略

- Unit tests：各模組內嵌 `tests.rs`（設定驗證、錯誤分類、敏感資料遮罩、機制對應）。
- Integration tests：`src/tests/`（watchdog flow、browser invalidation、startup prerequisites 等跨模組流程）。
- E2E tests：`#[ignore = "需要 CHROME_PATH"]`，以真實 headless shell 對 mock pages 執行，不得依賴真實 Twitch 網站狀態。
- Docker smoke：驗證 image、compose、安全掛載與 graceful shutdown。
- 測試移植：`doc/spec/ts-test-titles.tsv` 為舊測試標題權威，移植測試以 `// 原測試：<標題>` 標註並由 `scripts/port-coverage.sh` 檢查。

不得用略過測試、降低斷言或刪除有效測試取得綠燈。

## 資源觀察

服務 log 以 JSON Lines 寫到 stdout；正式 Docker 部署可直接使用 Docker logs 查詢。

常用 log 查詢：

```bash
docker compose logs -f twitch-watchdog
docker compose logs --no-log-prefix twitch-watchdog
docker compose logs --no-log-prefix twitch-watchdog | rg 'reward_claim_failure|container_restart_requested|scheduler_stall_detected'
```

忠誠點數領取復原相關事件：

- `reward_claim_failed`：單次忠誠點數領取失敗。
- `reward_claim_failure_threshold`：同一頻道連續失敗達復原門檻。
- `reward_claim_failure_recovery_refresh`：因連續失敗觸發該頻道頁面重整。
- `container_restart_requested`：重整後仍連續失敗，程序將以非 0 狀態結束，交由 Docker restart policy 重啟容器。
- `scheduler_stall_detected`：排程檢查長時間停留在 in-flight，容器級 watchdog 會 flush log 後以非 0 狀態結束程序，交由 Docker restart policy 重啟容器。

`/points` 只讀取既有 active session 的 `[data-test-selector="community-points-summary"]`，不會開啟額外 Twitch 頁面。點數讀取會進入同一 Page 的 maintenance queue，避免與健康檢查、獎勵領取、畫質調整或 reload 競爭。

Session 啟動相關事件：

- `session_start_retry_scheduled`：啟動 session 時遇到瀏覽器/page 剛關閉或 Twitch 頁面導覽逾時，會短暫等待後重試一次。
- `session_start_failed`：session 啟動最終失敗；該頻道不會留在 active registry，其他頻道會繼續處理。
- `session_start_timeout`：逾時後會呼叫 session 的取消啟動路徑，立即要求 BrowserManager 關閉已建立的 page，再執行一般失敗清理。
- `browser_navigation_failure_recorded`：debug 級事件；記錄短重試後的最終 `page.goto` 逾時與目前頻道／全域計數。成功啟動會清除該頻道先前的逾時計數。
- `browser_navigation_failure_recycle_requested`：5 分鐘內同一頻道達 2 次，或全部頻道合計達 3 次最終導覽逾時，因此回收共用 browser。事件 observer 在 SessionManager reconcile lock 之外執行，避免 browser invalidation 與 session lock 形成循環等待。
- `browser_navigation_failure_recycle_failed`：導覽逾時要求的 browser recycle 失敗，直接升級至 fatal container recovery。
- `browser_navigation_failure_loop`：10 分鐘內累積 3 次 browser failure recovery，不再反覆回收瀏覽器，改由 `ContainerRestartController` 要求 Docker 重啟容器。

只有 session 啟動的最終 `page.goto` 逾時納入上述 breaker。`page_refresh_interval_seconds` 定時重整與 `/refresh_now` 手動重整仍完整保留，且其 `page.reload` 逾時不會污染啟動失敗計數。

容器資源：

```bash
docker stats twitch-watchdog --no-stream
docker top twitch-watchdog -eo pid,ppid,rss,comm,args
```

將 runtime resource snapshot 轉為 CSV：

```bash
docker compose logs --no-log-prefix twitch-watchdog \
  | npm run benchmark:csv --silent \
  > benchmark.csv
```

Benchmark 輸出屬於本機開發產物，已由 `.gitignore` 排除。

## 詳細診斷 Log

若要排查排程卡住、page crash 或瀏覽器資源關閉問題，將 `config.yml` 的 `log_level` 設為 `debug` 後重啟服務：

```yaml
log_level: debug
```

常用診斷查詢：

```bash
docker compose logs --no-log-prefix twitch-watchdog \
  | rg 'scheduler_tick_|scheduler_stall_detected|session_(reconcile|invalidate|start_attempt)|browser_(page_invalidation|resource_close|navigation_failure|request_failed|http_response|console_message)|page_crashed|page_closed|page_refresh_failed|resource_guard_|cgroup_|runtime_resource_snapshot|container_restart_requested'
```

重點事件：

- `scheduler_tick_started` / `scheduler_tick_selection` / `scheduler_tick_completed`：確認每輪排程是否有開始、選出哪些觀看頻道、以及是否完成。
- `scheduler_tick_failed`：排程 tick 發生未預期錯誤，會包含 `tickId`、耗時與已遮罩錯誤訊息。
- `scheduler_stall_watchdog_started` / `scheduler_stall_detected`：確認容器級 watchdog 的檢查間隔、卡住門檻，以及是否已要求容器重啟。
- `session_reconcile_started` / `session_reconcile_completed`：確認 SessionManager 是否進入 reconcile，以及 start/stop 後的 active session 清單。
- `session_invalidate_started` / `session_invalidate_completed`：確認 page crash 或 browser restart 是否真的移除 session。
- `browser_page_invalidation_started` / `browser_page_invalidation_completed` / `browser_page_invalidation_notified`：確認 BrowserManager 是否收到 page crash/close，是否刪除 page registry，以及是否通知 SessionManager。
- `browser_resource_close_started` / `browser_resource_close_completed` / `browser_page_close_timeout`：確認卡住的是 page、context 或 browser close；timeout 事件代表清理超過保護上限。
- `resource_guard_enabled` / `resource_guard_warning` / `resource_guard_browser_recycle_*` / `resource_guard_container_restart_requested`：cgroup 記憶體政策決策與回收。
- `cgroup_metrics_unavailable` / `resource_guard_limit_clamped`：cgroup 不可用，或縮放門檻被 `memory.max` 壓低。
- `runtime_resource_snapshot`：包含控制端程序欄位（RSS、CPU 時間）以及 cgroup 記憶體、CPU 累計時間與程序數欄位（例如 `cgroupMemoryCurrentBytes`、`cgroupCpuUsageUsec`、`cgroupPidsCurrent`）。與 Node.js 版的差異見 CHANGELOG 的 X5。
- `session_maintenance_completed` / `session_maintenance_skipped`：健康檢查、獎勵領取、點數讀取、截圖、畫質維持與頁面重整的排隊時間、執行時間、結果或略過原因。同一頁面的操作會依序執行；停止 session 時最多等待 queue 5 秒，逾時記錄 `session_page_operation_drain_timeout` 後強制關閉 page。
- `side_nav_collapsed` / `side_nav_collapse_skipped`：啟動或 reload 後的 Twitch 左側欄收合結果；找不到按鈕或側欄已收合時不會輸出失敗事件。
- `browser_request_failed`（warn / debug）：`requestfailed`。欄位：`channel`、`browserGeneration`、`pageGeneration`、`endpointCategory`、`host`、`path`、`method`、`resourceType`、`failureText`，GraphQL 請求另附 `graphQlOperationNames`。正常沒有 HTTP status。第三方追蹤/分析（`third_party`、`twitch_other`、`unknown`）與被瀏覽器中斷的請求（`net::ERR_ABORTED`，例如換畫質時被取消的 segment）以 debug 記錄；其餘 Twitch 核心端點失敗（document、script、GraphQL、API、anti-abuse、非中斷型 media）以 warn 記錄。
- `browser_http_response`：非成功回應記錄（500+ 全部 warn；400–499 僅 document、script、fetch/XHR、GraphQL 或 Twitch anti-abuse 請求以 warn 記錄）；`log_level: debug` 時，成功的 Twitch bootstrap 回應（document、script、GraphQL、API）以 debug 記錄。例行 media segment、圖片、字型與第三方分析不記錄。
- `browser_console_message`：console `error` 對應 warn；來源為例行噪音主機（`third_party`、`twitch_other`、`unknown`）或符合已知噪音 pattern（如 `SpadeClient send error`、`failed integrity check`）的 error 對應 debug；`warning` 對應 debug；一般 `log`/`info`/`debug`/table/timing/trace 不記錄。欄位：`consoleType`、`message`（bounded 且經通用遮罩）、`sourceHost`/`sourcePath`（如有）、`lineNumber`/`columnNumber`（如有）。console argument 一律不序列化。

瀏覽器診斷 URL 只保留 hostname 與 pathname；query string、fragment、credentials、port、cookie、request/response body 與 GraphQL variables 一律不記錄。console 與失敗訊息會先套用通用 `redactSensitiveString()` 再依上限裁切。`endpointCategory` 分類為 `twitch_document`、`twitch_javascript`、`twitch_graphql`、`twitch_anti_abuse`、`twitch_api`、`twitch_media`、`twitch_other`、`third_party`、`unknown`，規則刻意保守，分類只描述觀測到的端點。

`cgroupCpuUsageUsec`、`cgroupCpuUserUsec` 與 `cgroupCpuSystemUsec` 是容器自啟動以來的累計微秒數；比較相鄰 snapshot 的差值可計算整個 cgroup（包含瀏覽器）的 CPU 使用量。`npm run benchmark:csv` 會保留這些欄位。

啟用 resource guard 時，高頻政策採樣只讀 `memory.current`、`memory.events` 與 `memory.swap.current`；啟動及 `resource_telemetry_interval_seconds` 週期才讀取包含 `memory.max`、`memory.peak`、`pids.current` 與 `cpu.stat` 的完整 snapshot。

Browser recycle 不會阻塞下一次 guard 採樣。Recycle 進行中只評估 emergency memory、emergency swap 與 cgroup OOM/max event；一般 warning、recycle 與 fast-growth 判斷暫停，避免 browser/session refill 造成誤判。

健康狀態與獎勵候選會以批次 DOM snapshot 讀取；播放器解析度已符合設定時不再開啟 Twitch 畫質選單。這些最佳化不改變既有設定或對外介面。

貼回問題 log 時，請保留同一段時間內的 `scheduler_tick_*`、`session_*`、`browser_*`、`page_*`、`resource_guard_*` 與 `runtime_resource_snapshot` 事件。

### Resource guard 門檻

- 定時重整 `page_refresh_interval_seconds` 預設為 `0`。
- Compose 預設：`mem_limit=6g`、`memswap_limit=7g`、`pids_limit=512`（基線約 3 個同時觀看）。
- YAML 的 `browser.resource_guard.*_memory_mib` 是 `baseline_streams`（預設 3）的錨點。
- `scale_with_streams: true` 時，有效門檻隨 `max_concurrent_streams` 縮放：`effective = base + (anchor - base) * (N / baseline)`。
- Swap、OOM 事件與快速成長規則不隨 N 縮放。
- `fast_memory_growth` 需同時滿足：視窗 `fast_growth_window_seconds` 內成長量達 `fast_growth_mib`，且目前 cgroup 記憶體不低於有效 warning 門檻。瀏覽器重啟後的低水位回填不算致命。
- 任何成功的瀏覽器重啟（resource guard 回收、crash loop 回收、斷線復原）後，成長率規則會在 `post_browser_restart_rate_grace_seconds`（預設 120 秒）內停用，並清除取樣歷史。
- 執行中透過 Bot 修改 `max_concurrent_streams`，不會重算 resource guard 門檻，需重啟程序才生效。

### 容器重啟與瀏覽器清理

- `ContainerRestartController` 是唯一的致命結束路徑：`container_restart_requested` 只記錄一次，flush 最多 5 秒，exit code 1。
- 觸發來源：resource guard、排程卡住、領取失敗升級、瀏覽器致命復原。
- 瀏覽器關閉逾時不算成功。page 關閉卡住時，會非同步排程整個瀏覽器的回收（不在同一個 context 內替換 page，避免 SessionManager 與 BrowserManager 的鎖循環）。
- 舊瀏覽器回報斷線後才會啟動替代瀏覽器，否則改為重啟容器。
- 自動重啟次數用盡與 crash loop 視窗會升級為重啟容器。
- Session 啟動的最終導覽逾時使用 5 分鐘斷路器：同一頻道 2 次或全部頻道 3 次會回收瀏覽器；10 分鐘內 3 次瀏覽器失敗復原，升級為重啟容器。
- 已啟用的定時與手動重整刻意排除在此斷路器之外。

## Telegram 指令隔離與逾時

- `/points` 與 `/screenshot` 使用最多兩筆的背景工作，收訊不等待頁面操作；管理指令維持既有順序。
- 30 秒後記錄 `telegram_query_timeout` 並回覆逾時。底層工作沒有取消能力，因此結束前仍保留名額；不會釋放 Page maintenance queue 或讓同頁操作重疊。晚到的結果會由取消訊號抑制。
- 停止服務時取消回覆與計時器，不等待卡住的查詢。查詢失敗記錄 `telegram_query_failed`，回覆失敗記錄 `telegram_send_failed`，均不記錄 chat ID、token 或原始錯誤內容。
- `getUpdates` 使用停止訊號與「長輪詢秒數 + 10 秒」逾時訊號的組合；請求及回應本文讀取逾時皆保留 `timeout` 錯誤分類，沿用輪詢重試退避。
- 回歸測試：`src/telegram/{api,bot}.rs` 的單元測試，涵蓋卡住的查詢、後續輪詢與 `/status`、逾時、名額限制、晚到結果、停止與網路重試。

## 維護原則

- 不要重新加入 Twitch Drops 自動領取或舊 GraphQL claim 流程。
- 不要實作自動輸入 Twitch 帳號密碼、多帳號批量管理、CAPTCHA 繞過、反偵測或平台限制規避。
- `storage-state.json`、`config.yml`、token、cookie、Telegram Chat ID、Discord Channel ID 與 Discord User ID 不得提交。
- Twitch 播放器 DOM 不是穩定公開 API；selector 變更應集中在 `src/browser` 並以 mock pages 覆蓋。
- 播放最佳化或 Bonus Points 領取失敗不得中止觀看 session。
- Twitch 內容警示確認失敗應回報明確健康檢查原因，不得誤判為登入或離線。
- Twitch API 暫時失敗時，不得把既有 active session 全部當成離線關閉。

## 版本控制注意事項

`.gitignore` 已排除本機依賴、建置輸出（`target/`）、測試輸出、憑證、登入狀態、benchmark CSV、headless shell 快取（`.cache/`）與開發用途 `doc` 文件。若某些檔案已被 Git 追蹤，`.gitignore` 不會自動取消追蹤；需要移除時請另外使用 `git rm --cached` 並確認團隊希望這麼做。
