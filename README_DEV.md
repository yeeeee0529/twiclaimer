# twiclaimer 開發者文件

本文件提供本機開發、測試與維護資訊。使用者部署流程請見 [README.md](README.md)。

## 登入異常與健康檢查重試 / Login failures and health retries

- `page_health_failed.reason: login_missing` 表示頁面顯示一般登入按鈕；`login_required` 表示明確的登入阻擋。兩者都不視為健康觀看，也不代表忠誠點數正在累積。播放器錯誤與離線仍保留各自原因。
  `login_missing` means the page shows the ordinary login button; `login_required` means an explicit login gate. Neither counts as healthy viewing or proves points are accruing. Player errors and offline states retain their own reasons.
- 登入異常不會立即重整頁面；達既有連續健康失敗門檻後仍會關閉該頁。健康失敗造成的 session 失效會沿用 `browser.session_start.failure_backoff_seconds`（預設 `[30, 60, 120]`），之後以 `maximum_cooldown_seconds`（預設 900 秒）延後重試；實際重試時間取決於下一次排程。這與啟動失敗共用計數，短暫成功開頁不會清除，持續穩定達 `browser.recovery.stable_reset_seconds` 後才重設。
  Login failures skip the immediate health-triggered reload. The existing consecutive-failure threshold still closes the page. Health invalidations share the session-start failure counter and configured backoffs (default 30, 60, 120 seconds, then 900 seconds); retry occurs on the next eligible scheduler tick. Opening a page briefly does not reset the counter; the configured stable period does.
- `channel_health_recovery_backoff` 記錄頻道、累計失敗數與下次可重試時間 `retryAtMs`。此路徑不計為 page crash，也不觸發崩潰通知或瀏覽器崩潰回收。
  `channel_health_recovery_backoff` reports the channel, accumulated failures, and next eligible retry time (`retryAtMs`). This path does not count as a page crash or trigger crash notifications/browser crash recovery.
- 若持續出現登入異常，請在本機執行 `npm run twitch:login` 手動重新登入並匯出狀態，將檔案放回部署所掛載的位置後重啟服務。請勿分享登入狀態檔。HTTP 429 仍保留 warn，重試退避不會解除 Twitch 的限流，也無法自動恢復登入。
  For persistent login failures, manually reauthenticate with `npm run twitch:login`, replace the deployed state file, and restart the service. Do not share that file. HTTP 429 remains a warning; backoff cannot remove Twitch rate limits or restore authentication.


## 專案結構

- `src/config`：YAML 設定（yaml 2.9.0 相容語意）、環境變數覆寫、runtime config 持久化（位元組範圍寫回）。
- `src/credentials`：storageState 與 Twitch API 設定檢查。
- `src/twitch`：Twitch Helix API client 與 live status provider。
- `src/browser`：精簡 CDP client（`--remote-debugging-pipe`，初始化順序與錯誤訊息對齊 Playwright）、觀看 session、畫質最佳化與 Bonus Points 領取；含 Twitch 內容警示確認閘。
  A minimal CDP client speaking `--remote-debugging-pipe` with Playwright-aligned init order and error messages, plus watch sessions, quality optimization, Bonus Points claiming, and the content-warning gate.
- `src/logging`：JSON Lines logger（欄位順序、遮罩規則、JS 數字格式與舊版相同）。
- `src/sessions`：多頻道 session reconcile（`SessionManager`／`ReconcileCoordinator`）。
- `src/scheduler`：輪詢排程、stream selection 與不可重入控制（`WatchdogScheduler`）。
- `src/telegram`：Telegram Bot API（reqwest 長輪詢）與指令處理。
- `src/discord`：Discord REST API 與 slash command 處理；gateway 以 twilight `Shard` 啟動（不開 cache）。
  Discord REST API and slash command handling; the gateway runs on a twilight `Shard` without cache.
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
Smoke validates the merged Compose config, builds the test and production targets, checks the image contains only `twiclaimer` + headless shell + `config.example.yml` (no `config.yml`, `.env`, storage state, sources, or tests), verifies missing-config failure with `config_error`, starts with the smoke override, checks cgroup limits, `service_stopped` on restart/SIGTERM, no leftover headless-shell processes, and prints the image size. Fake credentials and `network_mode: none` mean it never touches Twitch.

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
Service logs are written as JSON Lines to stdout; production Docker deployments can inspect them through Docker logs.

常用 log 查詢：
Common log queries:

```bash
docker compose logs -f twitch-watchdog
docker compose logs --no-log-prefix twitch-watchdog
docker compose logs --no-log-prefix twitch-watchdog | rg 'reward_claim_failure|container_restart_requested|scheduler_stall_detected'
```

忠誠點數領取復原相關事件：
Reward claim recovery events:

- `reward_claim_failed`：單次忠誠點數領取失敗。
  Single reward claim attempt failed.
- `reward_claim_failure_threshold`：同一頻道連續失敗達復原門檻。
  A channel reached the consecutive failure recovery threshold.
- `reward_claim_failure_recovery_refresh`：因連續失敗觸發該頻道頁面重整。
  A channel page refresh was triggered by consecutive reward failures.
- `container_restart_requested`：重整後仍連續失敗，程序將以非 0 狀態結束，交由 Docker restart policy 重啟容器。
  Reward failures continued after the recovery refresh, so the process exits non-zero and Docker restart policy restarts the container.
- `scheduler_stall_detected`：排程檢查長時間停留在 in-flight，容器級 watchdog 會 flush log 後以非 0 狀態結束程序，交由 Docker restart policy 重啟容器。
  A scheduler check remained in flight past the watchdog threshold, so the service flushes logs and exits non-zero for Docker restart policy recovery.

`/points` 只讀取既有 active session 的 `[data-test-selector="community-points-summary"]`，不會開啟額外 Twitch 頁面。點數讀取會進入同一 Page 的 maintenance queue，避免與健康檢查、獎勵領取、畫質調整或 reload 競爭。
`/points` reads `[data-test-selector="community-points-summary"]` only from existing active sessions and does not open additional Twitch pages. Balance reads enter the same per-Page maintenance queue to avoid competing with health checks, reward claims, quality enforcement, or reloads.

Session 啟動相關事件：
Session startup events:

- `session_start_retry_scheduled`：啟動 session 時遇到瀏覽器/page 剛關閉或 Twitch 頁面導覽逾時，會短暫等待後重試一次。
  A session start hit a just-closed browser/page or Twitch page navigation timeout, so the manager waits briefly and retries once.
- `session_start_failed`：session 啟動最終失敗；該頻道不會留在 active registry，其他頻道會繼續處理。
  Session startup ultimately failed; that channel is not kept in the active registry, and other channels continue processing.
- `session_start_timeout`：逾時後會呼叫 session 的取消啟動路徑，立即要求 BrowserManager 關閉已建立的 page，再執行一般失敗清理。
  After a startup timeout, the manager invokes the session cancellation path, immediately asks BrowserManager to close any created page, and then performs normal failed-start cleanup.
- `browser_navigation_failure_recorded`：debug 級事件；記錄短重試後的最終 `page.goto` 逾時與目前頻道／全域計數。成功啟動會清除該頻道先前的逾時計數。
  Debug-level event recording a final `page.goto` timeout after the short retry and its current channel/global counts. A successful startup clears prior timeout counts for that channel.
- `browser_navigation_failure_recycle_requested`：5 分鐘內同一頻道達 2 次，或全部頻道合計達 3 次最終導覽逾時，因此回收共用 browser。事件 observer 在 SessionManager reconcile lock 之外執行，避免 browser invalidation 與 session lock 形成循環等待。
  The shared browser is recycled after two final navigation timeouts for one channel or three across all channels within five minutes. The event observer runs outside the SessionManager reconcile lock to avoid a browser-invalidation/session-lock cycle.
- `browser_navigation_failure_recycle_failed`：導覽逾時要求的 browser recycle 失敗，直接升級至 fatal container recovery。
  A navigation-timeout-triggered browser recycle failed and escalated directly to fatal container recovery.
- `browser_navigation_failure_loop`：10 分鐘內累積 3 次 browser failure recovery，不再反覆回收瀏覽器，改由 `ContainerRestartController` 要求 Docker 重啟容器。
  Three browser-failure recoveries accumulated within ten minutes, so the browser is no longer recycled again and `ContainerRestartController` requests a Docker container restart.

只有 session 啟動的最終 `page.goto` 逾時納入上述 breaker。`page_refresh_interval_seconds` 定時重整與 `/refresh_now` 手動重整仍完整保留，且其 `page.reload` 逾時不會污染啟動失敗計數。
Only final session-start `page.goto` timeouts enter this breaker. Scheduled `page_refresh_interval_seconds` reloads and manual `/refresh_now` remain fully supported, and their `page.reload` timeouts do not affect startup-failure counters.

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
To diagnose stuck scheduler ticks, page crashes, or browser resource cleanup issues, set `log_level` to `debug` in `config.yml` and restart the service:

```yaml
log_level: debug
```

常用診斷查詢：
Useful diagnostic queries:

```bash
docker compose logs --no-log-prefix twitch-watchdog \
  | rg 'scheduler_tick_|scheduler_stall_detected|session_(reconcile|invalidate|start_attempt)|browser_(page_invalidation|resource_close|navigation_failure|request_failed|http_response|console_message)|page_crashed|page_closed|page_refresh_failed|resource_guard_|cgroup_|runtime_resource_snapshot|container_restart_requested'
```

重點事件：
Important events:

- `scheduler_tick_started` / `scheduler_tick_selection` / `scheduler_tick_completed`：確認每輪排程是否有開始、選出哪些觀看頻道、以及是否完成。
  Confirm whether each scheduler tick started, which active channels were selected, and whether the tick completed.
- `scheduler_tick_failed`：排程 tick 發生未預期錯誤，會包含 `tickId`、耗時與已遮罩錯誤訊息。
  Indicates an unexpected scheduler tick failure with `tickId`, duration, and a redacted error message.
- `scheduler_stall_watchdog_started` / `scheduler_stall_detected`：確認容器級 watchdog 的檢查間隔、卡住門檻，以及是否已要求容器重啟。
  Confirms the container-level watchdog interval, stall threshold, and whether it requested container restart recovery.
- `session_reconcile_started` / `session_reconcile_completed`：確認 SessionManager 是否進入 reconcile，以及 start/stop 後的 active session 清單。
  Confirms whether SessionManager entered reconcile and what active sessions remained after start/stop work.
- `session_invalidate_started` / `session_invalidate_completed`：確認 page crash 或 browser restart 是否真的移除 session。
  Confirms whether a page crash or browser restart actually removed the affected session.
- `browser_page_invalidation_started` / `browser_page_invalidation_completed` / `browser_page_invalidation_notified`：確認 BrowserManager 是否收到 page crash/close，是否刪除 page registry，以及是否通知 SessionManager。
  Confirms whether BrowserManager received page crash/close, removed the page registry entry, and notified SessionManager.
- `browser_resource_close_started` / `browser_resource_close_completed` / `browser_page_close_timeout`：確認卡住的是 page、context 或 browser close；timeout 事件代表清理超過保護上限。
  Confirms whether page, context, or browser close is stuck; timeout events mean cleanup exceeded the guard limit.
- `resource_guard_enabled` / `resource_guard_warning` / `resource_guard_browser_recycle_*` / `resource_guard_container_restart_requested`：cgroup 記憶體政策決策與回收。
  Resource-guard decisions and browser recycle / container restart requests from cgroup memory policy.
- `cgroup_metrics_unavailable` / `resource_guard_limit_clamped`：cgroup 不可用，或縮放門檻被 `memory.max` 壓低。
  Cgroup metrics unavailable, or scaled thresholds clamped under `memory.max`.
- `runtime_resource_snapshot`：包含控制端程序欄位（RSS、CPU 時間）以及 cgroup 記憶體、CPU 累計時間與程序數欄位（例如 `cgroupMemoryCurrentBytes`、`cgroupCpuUsageUsec`、`cgroupPidsCurrent`）。與 Node.js 版的差異見 CHANGELOG 的 X5。
  Includes controller-process fields (RSS, CPU time) plus cgroup memory, cumulative CPU-time, and process-count fields (for example, `cgroupMemoryCurrentBytes`, `cgroupCpuUsageUsec`, and `cgroupPidsCurrent`). For differences from the Node.js edition see X5 in the CHANGELOG.
- `session_maintenance_completed` / `session_maintenance_skipped`：健康檢查、獎勵領取、點數讀取、截圖、畫質維持與頁面重整的排隊時間、執行時間、結果或略過原因。同一頁面的操作會依序執行；停止 session 時最多等待 queue 5 秒，逾時記錄 `session_page_operation_drain_timeout` 後強制關閉 page。
  Reports queue time, execution time, outcome, or skip reason for health checks, reward claims, point reads, screenshots, quality enforcement, and reloads. Operations on the same page run sequentially; session shutdown drains the queue for up to five seconds, then logs `session_page_operation_drain_timeout` and force-closes the page.
- `side_nav_collapsed` / `side_nav_collapse_skipped`：啟動或 reload 後的 Twitch 左側欄收合結果；找不到按鈕或側欄已收合時不會輸出失敗事件。
  Reports Twitch sidebar collapse outcomes after startup or reload. A missing toggle or an already-collapsed sidebar is not treated as a failure.
- `browser_request_failed`（warn / debug）：`requestfailed`。欄位：`channel`、`browserGeneration`、`pageGeneration`、`endpointCategory`、`host`、`path`、`method`、`resourceType`、`failureText`，GraphQL 請求另附 `graphQlOperationNames`。正常沒有 HTTP status。第三方追蹤/分析（`third_party`、`twitch_other`、`unknown`）與被瀏覽器中斷的請求（`net::ERR_ABORTED`，例如換畫質時被取消的 segment）以 debug 記錄；其餘 Twitch 核心端點失敗（document、script、GraphQL、API、anti-abuse、非中斷型 media）以 warn 記錄。
  `requestfailed` with `channel`, `browserGeneration`, `pageGeneration`, `endpointCategory`, `host`, `path`, `method`, `resourceType`, `failureText`, plus `graphQlOperationNames` for GraphQL requests. There is normally no HTTP status. Third-party tracking/analytics (`third_party`, `twitch_other`, `unknown`) and browser-aborted requests (`net::ERR_ABORTED`, for example segments cancelled by a quality switch) log at debug; other Twitch core endpoint failures (document, script, GraphQL, API, anti-abuse, and non-aborted media) log at warn.
- `browser_http_response`：非成功回應記錄（500+ 全部 warn；400–499 僅 document、script、fetch/XHR、GraphQL 或 Twitch anti-abuse 請求以 warn 記錄）；`log_level: debug` 時，成功的 Twitch bootstrap 回應（document、script、GraphQL、API）以 debug 記錄。例行 media segment、圖片、字型與第三方分析不記錄。
  Non-success responses are logged (500+ warn for all; 400–499 warn only for document, script, fetch/XHR, GraphQL, or Twitch anti-abuse requests). With `log_level: debug`, successful Twitch bootstrap responses (document, script, GraphQL, API) log at debug. Routine media segments, images, fonts, and third-party analytics are excluded.
- `browser_console_message`：console `error` 對應 warn；來源為例行噪音主機（`third_party`、`twitch_other`、`unknown`）或符合已知噪音 pattern（如 `SpadeClient send error`、`failed integrity check`）的 error 對應 debug；`warning` 對應 debug；一般 `log`/`info`/`debug`/table/timing/trace 不記錄。欄位：`consoleType`、`message`（bounded 且經通用遮罩）、`sourceHost`/`sourcePath`（如有）、`lineNumber`/`columnNumber`（如有）。console argument 一律不序列化。
  Console `error` maps to warn; errors whose source is a routine-noise host (`third_party`, `twitch_other`, `unknown`) or that match a known noise pattern (e.g. `SpadeClient send error`, `failed integrity check`) map to debug; `warning` maps to debug; ordinary `log`/`info`/`debug`/table/timing/trace messages are ignored. Fields: `consoleType`, `message` (bounded and generically redacted), `sourceHost`/`sourcePath` when available, and `lineNumber`/`columnNumber` when available. Console arguments are never serialized.

瀏覽器診斷 URL 只保留 hostname 與 pathname；query string、fragment、credentials、port、cookie、request/response body 與 GraphQL variables 一律不記錄。console 與失敗訊息會先套用通用 `redactSensitiveString()` 再依上限裁切。`endpointCategory` 分類為 `twitch_document`、`twitch_javascript`、`twitch_graphql`、`twitch_anti_abuse`、`twitch_api`、`twitch_media`、`twitch_other`、`third_party`、`unknown`，規則刻意保守，分類只描述觀測到的端點。
Browser diagnostics keep only the URL hostname and pathname; query strings, fragments, credentials, ports, cookies, request/response bodies, and GraphQL variables are never logged. Console and failure messages pass through the generic `redactSensitiveString()` before bounded truncation. `endpointCategory` classifies endpoints as `twitch_document`, `twitch_javascript`, `twitch_graphql`, `twitch_anti_abuse`, `twitch_api`, `twitch_media`, `twitch_other`, `third_party`, or `unknown`; rules are intentionally conservative and only describe the observed endpoint.

`cgroupCpuUsageUsec`、`cgroupCpuUserUsec` 與 `cgroupCpuSystemUsec` 是容器自啟動以來的累計微秒數；比較相鄰 snapshot 的差值可計算整個 cgroup（包含瀏覽器）的 CPU 使用量。`npm run benchmark:csv` 會保留這些欄位。
`cgroupCpuUsageUsec`, `cgroupCpuUserUsec`, and `cgroupCpuSystemUsec` are cumulative microseconds since the container started. Compare deltas between adjacent snapshots to calculate whole-cgroup CPU usage, including the browser. `npm run benchmark:csv` preserves these fields.

啟用 resource guard 時，高頻政策採樣只讀 `memory.current`、`memory.events` 與 `memory.swap.current`；啟動及 `resource_telemetry_interval_seconds` 週期才讀取包含 `memory.max`、`memory.peak`、`pids.current` 與 `cpu.stat` 的完整 snapshot。
When the resource guard is enabled, high-frequency policy samples read only `memory.current`, `memory.events`, and `memory.swap.current`. Startup and `resource_telemetry_interval_seconds` intervals use full snapshots that also include `memory.max`, `memory.peak`, `pids.current`, and `cpu.stat`.

Browser recycle 不會阻塞下一次 guard 採樣。Recycle 進行中只評估 emergency memory、emergency swap 與 cgroup OOM/max event；一般 warning、recycle 與 fast-growth 判斷暫停，避免 browser/session refill 造成誤判。
Browser recycle does not block subsequent guard samples. While recycle is in flight, only emergency memory, emergency swap, and cgroup OOM/max events are evaluated; ordinary warning, recycle, and fast-growth decisions pause to avoid browser/session refill false positives.

健康狀態與獎勵候選會以批次 DOM snapshot 讀取；播放器解析度已符合設定時不再開啟 Twitch 畫質選單。這些最佳化不改變既有設定或對外介面。
Health state and reward candidates are read through batched DOM snapshots. When the active video resolution already matches the configured quality, the Twitch quality menu is not opened. These optimizations do not change existing configuration or public interfaces.

貼回問題 log 時，請保留同一段時間內的 `scheduler_tick_*`、`session_*`、`browser_*`、`page_*`、`resource_guard_*` 與 `runtime_resource_snapshot` 事件。
When sharing logs for debugging, include `scheduler_tick_*`, `session_*`, `browser_*`, `page_*`, `resource_guard_*`, and `runtime_resource_snapshot` events from the same time window.

### Resource guard thresholds

- Scheduled `page_refresh_interval_seconds` default is `0`.
- Compose defaults: `mem_limit=6g`, `memswap_limit=7g`, `pids_limit=512` (baseline ~3 concurrent streams).
- YAML `browser.resource_guard.*_memory_mib` values are anchors for `baseline_streams` (default 3).
- Effective thresholds scale with `max_concurrent_streams` when `scale_with_streams: true`:
  `effective = base + (anchor - base) * (N / baseline)`.
- Swap / OOM event / fast-growth rules do not scale with N.
- `fast_memory_growth` requires both: (1) growth ≥ `fast_growth_mib` within `fast_growth_window_seconds`, and (2) current cgroup memory ≥ effective warning. Low absolute refill after browser restart is not fatal.
- After any successful browser restart (resource-guard recycle, crash-loop recycle, disconnect recovery), rate growth is suppressed for `post_browser_restart_rate_grace_seconds` (default 120) and sample history is cleared.
- Changing `max_concurrent_streams` at runtime via bot does not recompute resource-guard thresholds until process restart.

### Container restart and browser cleanup

- `ContainerRestartController` is the single-flight fatal exit path (`container_restart_requested` once, flush ≤5s, exit code 1).
- Sources: resource guard, scheduler stall, reward escalation, browser fatal recovery.
- Browser close timeout is **not** success. Page close hang schedules a full browser recycle asynchronously (no same-context page replace; avoids SessionManager/BrowserManager lock cycles).
- Replacement browser launches only after the old browser reports disconnected; otherwise container restart.
- Automatic restart exhaustion and crash-loop windows escalate to container restart.
- Final session-start navigation timeouts use a five-minute breaker: 2 for one channel or 3 globally recycle the browser. Three browser-failure recoveries within ten minutes escalate to container restart.
- Scheduled/manual reload remains enabled when configured and is deliberately excluded from the session-start navigation breaker.

## Telegram 指令隔離與逾時 / Telegram query isolation and deadlines

- `/points` 與 `/screenshot` 使用最多兩筆的背景工作，收訊不等待頁面操作；管理指令維持既有順序。
  `/points` and `/screenshot` use at most two background flights; polling does not await page operations. Management commands preserve their existing order.
- 30 秒後記錄 `telegram_query_timeout` 並回覆逾時。底層工作沒有取消能力，因此結束前仍保留名額；不會釋放 Page maintenance queue 或讓同頁操作重疊。晚到的結果會由取消訊號抑制。
  After 30 seconds, log `telegram_query_timeout` and reply with a timeout. Underlying work cannot be cancelled, so its slot remains occupied until settlement. The Page maintenance queue remains intact; late results are suppressed by an abort signal.
- 停止服務時取消回覆與計時器，不等待卡住的查詢。查詢失敗記錄 `telegram_query_failed`，回覆失敗記錄 `telegram_send_failed`，均不記錄 chat ID、token 或原始錯誤內容。
  Shutdown cancels replies and timers without awaiting stuck queries. Query failures log `telegram_query_failed`; reply failures log `telegram_send_failed`, without chat IDs, tokens, or raw errors.
- `getUpdates` 使用停止訊號與「長輪詢秒數 + 10 秒」逾時訊號的組合；請求及回應本文讀取逾時皆保留 `timeout` 錯誤分類，沿用輪詢重試退避。
  `getUpdates` combines shutdown cancellation with a long-poll duration plus 10-second deadline. Request and response-body timeouts retain the `timeout` classification and existing polling retry backoff.
- 回歸測試：`src/telegram/{api,bot}.rs` 的單元測試，涵蓋卡住的查詢、後續輪詢與 `/status`、逾時、名額限制、晚到結果、停止與網路重試。
  Regression tests live in `src/telegram/{api,bot}.rs`, covering stuck queries, subsequent polling and `/status`, deadlines, capacity limits, late results, shutdown, and network retries.

## 維護原則

- 不要重新加入 Twitch Drops 自動領取或舊 GraphQL claim 流程。
- 不要實作自動輸入 Twitch 帳號密碼、多帳號批量管理、CAPTCHA 繞過、反偵測或平台限制規避。
- `storage-state.json`、`config.yml`、token、cookie、Telegram Chat ID、Discord Channel ID 與 Discord User ID 不得提交。
  Do not commit `storage-state.json`, `config.yml`, tokens, cookies, Telegram Chat IDs, Discord Channel IDs, or Discord User IDs.
- Twitch 播放器 DOM 不是穩定公開 API；selector 變更應集中在 `src/browser` 並以 mock pages 覆蓋。
- 播放最佳化或 Bonus Points 領取失敗不得中止觀看 session。
- Twitch 內容警示確認失敗應回報明確健康檢查原因，不得誤判為登入或離線。
  Content-warning confirmation failures should report a specific health reason and must not be treated as login or offline states.
- Twitch API 暫時失敗時，不得把既有 active session 全部當成離線關閉。

## 版本控制注意事項

`.gitignore` 已排除本機依賴、建置輸出（`target/`）、測試輸出、憑證、登入狀態、benchmark CSV、headless shell 快取（`.cache/`）與開發用途 `doc` 文件。若某些檔案已被 Git 追蹤，`.gitignore` 不會自動取消追蹤；需要移除時請另外使用 `git rm --cached` 並確認團隊希望這麼做。
