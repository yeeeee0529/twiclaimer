# twiclaimer

twiclaimer 是可用 Docker 長時間執行的 Twitch 觀看輔助服務（Twitch Watchdog 的 Rust 版）。它會透過 Twitch API 檢查指定頻道是否開台，依照設定優先序開啟觀看頁面，並自動領取 Bonus Channel Points。

本專案不會自動登入 Twitch，也不會要求你提供 Twitch 密碼。你需要自行登入 Twitch 後匯出 Playwright `storageState`，並提供 Twitch Developer App 的 Client ID 與 Client Secret。

## 登入異常與健康檢查重試 / Login failures and health retries

- `page_health_failed.reason: login_missing` 表示頁面顯示一般登入按鈕；`login_required` 表示明確的登入阻擋。兩者都不視為健康觀看，也不代表忠誠點數正在累積。播放器錯誤與離線仍保留各自原因。
  `login_missing` means the page shows the ordinary login button; `login_required` means an explicit login gate. Neither counts as healthy viewing or proves points are accruing. Player errors and offline states retain their own reasons.
- 登入異常不會立即重整頁面；達既有連續健康失敗門檻後仍會關閉該頁。健康失敗造成的 session 失效會沿用 `browser.session_start.failure_backoff_seconds`（預設 `[30, 60, 120]`），之後以 `maximum_cooldown_seconds`（預設 900 秒）延後重試；實際重試時間取決於下一次排程。這與啟動失敗共用計數，短暫成功開頁不會清除，持續穩定達 `browser.recovery.stable_reset_seconds` 後才重設。
  Login failures skip the immediate health-triggered reload. The existing consecutive-failure threshold still closes the page. Health invalidations share the session-start failure counter and configured backoffs (default 30, 60, 120 seconds, then 900 seconds); retry occurs on the next eligible scheduler tick. Opening a page briefly does not reset the counter; the configured stable period does.
- `channel_health_recovery_backoff` 記錄頻道、累計失敗數與下次可重試時間 `retryAtMs`。此路徑不計為 page crash，也不觸發崩潰通知或瀏覽器崩潰回收。
  `channel_health_recovery_backoff` reports the channel, accumulated failures, and next eligible retry time (`retryAtMs`). This path does not count as a page crash or trigger crash notifications/browser crash recovery.
- 若持續出現登入異常，請在本機執行 `npm run twitch:login` 手動重新登入並匯出狀態，將檔案放回部署所掛載的位置後重啟服務。請勿分享登入狀態檔。HTTP 429 仍保留 warn，重試退避不會解除 Twitch 的限流，也無法自動恢復登入。
  For persistent login failures, manually reauthenticate with `npm run twitch:login`, replace the deployed state file, and restart the service. Do not share that file. HTTP 429 remains a warning; backoff cannot remove Twitch rate limits or restore authentication.


## 功能

- 監控多個 Twitch 頻道開台狀態。
- 依 `channels` 順序與 `max_concurrent_streams` 決定實際觀看頻道。
- 使用 Chromium 播放 Twitch直播。
- 自動確認 Twitch 直播頁的內容警示（`Start Watching`）後繼續觀看。
  Automatically accepts Twitch channel content warnings (`Start Watching`) before continuing playback.
- 啟動與頁面重整後自動收合 Twitch 左側推薦頻道欄，減少非必要畫面內容。
  Automatically collapses Twitch's recommended-channel sidebar after startup and page reloads to reduce nonessential page content.
- 自動領取 Bonus Channel Points。
- Bonus Channel Points 連續領取失敗 10 次時，會先重整該頻道頁面；重整後若再次連續失敗 10 次，會結束程序並交由 Docker restart policy 重啟容器。
  If Bonus Channel Points claiming fails 10 times in a row, the service first refreshes that channel page; if it fails 10 more times after the refresh, it exits so Docker can restart the container through the configured restart policy.
- 排程檢查若長時間卡在執行中，容器級 watchdog 會記錄 `scheduler_stall_detected` 並結束程序，交由 Docker restart policy 重啟容器。
  If scheduler checks remain in flight for too long, the container-level watchdog logs `scheduler_stall_detected` and exits so Docker restart policy restarts the container.
- 預設將直播畫質維持在 `160p` 並靜音，降低長時間執行資源用量。
- 可選用 Telegram Bot 或 Discord Bot 查詢狀態與忠誠點數、管理頻道、暫停/恢復排程及取得截圖。
  Optional Telegram Bot or Discord Bot integrations can query status and channel-point balances, manage channels, pause/resume checks, and capture screenshots.

不支援 Twitch Drops 自動領取、自動輸入帳號密碼、多帳號批量管理、CAPTCHA 繞過、反偵測或規避平台限制。
Accepting Twitch content warnings only clicks the visible confirmation for the logged-in account; it does not bypass login, age gates, CAPTCHA, or platform restrictions.

## 前置需求

- Docker Engine 與 Docker Compose plugin。
- 一個 Twitch 帳號。
- Twitch Developer Console 建立的 App Client ID 與 Client Secret。
- 若要在本機匯出登入狀態：Node.js 24 以上與 npm 11。

正式容器以 `ubuntu:noble` 為底，搭配與 Playwright `1.62.1` 相同版本的 Chromium headless shell（`151.0.7922.34`）。正式 image 不含 Node.js、原始碼與測試。
The production image is based on `ubuntu:noble` and ships the Chromium headless shell `151.0.7922.34` (the same build Playwright `1.62.1` uses). It contains no Node.js, source code, or tests.

## 快速開始

建立設定檔與 storageState 目錄：

```bash
cp config.example.yml config.yml
mkdir -p data/browser-state
chmod 700 data data/browser-state
chmod 600 config.yml
```

編輯 `config.yml`：

```yaml
channels:
  - streamer_one
  - streamer_two

check_interval_seconds: 60
max_concurrent_streams: 1
headless: true
storage_state_path: /data/browser-state/storage-state.json
log_level: info

twitch_api:
  client_id: 你的ClientID
  client_secret: 你的ClientSecret

browser:
  engine: chromium
  stream_quality: 160p
  page_refresh_interval_seconds: 0
  resource_telemetry_interval_seconds: 60

telegram:
  enabled: false

discord:
  enabled: false
```

重點設定：

- `channels`：Twitch login name 清單，順序就是觀看優先序。
- `check_interval_seconds`：Twitch API 輪詢間隔，最小 30 秒。
- `max_concurrent_streams`：最大同時觀看數。
- `storage_state_path`：容器內 Playwright storageState 路徑。
- `browser.engine`：只支援 `chromium`；設定 `firefox` 會在載入設定時以 `ConfigValidationError` 失敗。
  Only `chromium` is supported; setting `firefox` fails config validation (`ConfigValidationError`).
- `headless`：只支援 `true`；設定 `false`（含 `HEADLESS=false` 覆寫）會在載入設定時失敗。
  Only `true` is supported; `false` (including a `HEADLESS=false` override) fails config validation.
- `browser.stream_quality`：預設 `160p`；設為 `auto` 可停用強制畫質。
- `browser.page_refresh_interval_seconds`：預設 `0`（關閉定時重整以降低瀏覽器記憶體壓力）；設正值可啟用定時重整並依頻道錯開。此功能會保留，因 Twitch 可能在重整後提供可領取的忠誠點數按鈕；手動 `/refresh_now` 仍可用。
  Defaults to `0` (scheduled refresh off to reduce browser memory pressure). Positive values enable staggered scheduled refresh. This remains supported because Twitch may expose a claimable loyalty-points button after reload; manual `/refresh_now` also remains available.
- Session 啟動在短重試後仍發生 `page.goto` 導覽逾時時，5 分鐘內同一頻道 2 次或全部頻道合計 3 次會回收共用瀏覽器；10 分鐘內 3 次此類 browser failure recovery 會要求 Docker 重啟容器。定時／手動頁面重整逾時不納入此計數。
  If session startup still ends in a `page.goto` timeout after its short retry, two failures for one channel or three across all channels within five minutes recycle the shared browser. Three such browser-failure recoveries within ten minutes request a Docker container restart. Scheduled/manual reload timeouts are not counted.
- `browser.resource_telemetry_interval_seconds`：預設 60 秒輸出 `runtime_resource_snapshot`（含 cgroup 欄位）。
  Defaults to 60 seconds for `runtime_resource_snapshot` (includes cgroup fields).
- `browser.resource_guard`：容器級記憶體防護（見下方）。
  Container-wide memory guard (see below).

### 資源防護與 Docker 限制 / Resource guard and Docker limits

Docker Compose 預設限制（約 **3 同時觀看** 的基線）：

| 設定 | 值 | 說明 |
| --- | --- | --- |
| `mem_limit` | 6g | 容器 RAM 上限 |
| `memswap_limit` | 7g | RAM+swap 合計上限（約 1g swap） |
| `pids_limit` | 512 | 行程／執行緒上限 |

若 `max_concurrent_streams` 明顯高於 3（例如 5），請提高 Compose 記憶體上限（建議約 10g/11g）後再 recreate 容器。
If `max_concurrent_streams` is much higher than 3 (e.g. 5), raise Compose memory limits (about 10g/11g recommended) and recreate the container.

`browser.resource_guard` 以 **整容器 cgroup** 記憶體為準（含瀏覽器），不是只看 twiclaimer 控制端程序。YAML 中的 MiB 門檻是 **N=`baseline_streams`（預設 3）錨點**；`scale_with_streams: true` 時：

```text
effective = base_memory_mib + (anchor - base_memory_mib) * (max_concurrent_streams / baseline_streams)
```

預設行為：

- 到達 warning 門檻：記錄 `resource_guard_warning`（有 hysteresis）。
- 連續 sample 達 recycle 門檻：呼叫 `BrowserManager.restart`（`resource_guard_browser_recycle_requested`）。
- emergency / swap / cgroup OOM 事件：記錄並以 exit code 1 結束程序，由 Docker restart。
- Browser recycle 進行中仍持續採樣 emergency / swap / cgroup OOM；一般 warning、recycle 與成長率規則會暫停，避免 session 回填誤判。
- 過快成長（`fast_memory_growth`）：僅在 **目前記憶體已達 warning 以上**，且視窗內成長量達門檻時才重開容器；browser 重啟後預設 120 秒內忽略此規則（避免 session 回填被誤判）。
- 無 cgroup v2 時只記 `cgroup_metrics_unavailable`，仍依賴 Docker hard limit。

Thresholds are container-wide cgroup memory (including the browser), not twiclaimer-only. YAML MiB values are anchors for `baseline_streams` (default 3) and scale with `max_concurrent_streams` when enabled. While a browser recycle is in flight, emergency memory, swap, and cgroup OOM signals remain active; ordinary warning, recycle, and growth-rate rules pause to avoid refill false positives. `fast_memory_growth` requires both a rate spike and absolute memory at/above the warning waterline; browser restarts apply a short rate-rule grace so session refill is not fatal. Missing cgroup v2 degrades to process metrics only; Docker hard limits remain the host protection.

查詢 cgroup 遙測：

```bash
docker compose logs --no-log-prefix twitch-watchdog \
  | rg 'runtime_resource_snapshot|resource_guard_|cgroup_metrics_unavailable|container_restart_requested'
```

## Twitch API 設定

到 Twitch Developer Console 建立應用程式，取得 Client ID 與 Client Secret，並填入 `config.yml`：

```yaml
twitch_api:
  client_id: 你的ClientID
  client_secret: 你的ClientSecret
```

服務會用 Client Credentials flow 自動取得 App Access Token，並在 token 無效、接近過期或 Helix API 回傳 HTTP 401 時重新取得。Token 只保存在程序記憶體，不會寫入 `config.yml`。

`twitch_api.access_token` 只作為沒有 Client Secret 時的手動備援。

## 匯出 Twitch 登入狀態

本專案不會自動登入，也不會開啟你的日常瀏覽器 profile。登入輔助工具維持 Node.js 版（`scripts/twitch-login.mjs`），建議在本機桌面環境執行：

1. 偵測系統預設瀏覽器  
2. 用**臨時 profile** 以一般 OS 程序開啟（登入階段**不開** remote debugging / Playwright 旗標）  
3. 你手動登入成功後在終端機按 Enter  
4. 工具再用**同一個臨時 profile** 短暫重開並經 CDP 匯出 Playwright `storageState`

Export Twitch login state on a local desktop machine. The login helper stays on Node.js (`scripts/twitch-login.mjs`). Login uses a normal OS browser process with an isolated temporary profile and no remote-debugging flags. After you press Enter, the same profile is relaunched briefly with CDP only to export Playwright `storageState`.

先安裝本機依賴：

```bash
npm ci
mkdir -p data/browser-state
chmod 700 data data/browser-state
```

若系統沒有 Chrome / Edge，可另外安裝 Playwright Firefox 作為後備：

```bash
npx playwright install firefox
```

執行登入輔助工具：

```bash
npm run twitch:login
```

在開啟的瀏覽器視窗中手動登入 Twitch（含 2FA）。工具偵測到 `auth-token` 後會自動寫入 `data/browser-state/storage-state.json` 並設定權限；你也可以在終端機按 Enter 立即存檔。

瀏覽器優先順序：

1. 系統預設瀏覽器（若 Playwright 可驅動，例如 Chrome / Edge / Firefox）
2. Google Chrome
3. Microsoft Edge
4. 系統 Firefox（若找得到）
5. Playwright Firefox

Safari 不能由 Playwright 驅動；若預設是 Safari，會自動改用上述後備瀏覽器。可用 `--browser chrome|msedge|firefox` 強制指定。

進階備援（不建議作為預設流程）：

```bash
npx playwright codegen \
  --browser=firefox \
  --save-storage=data/browser-state/storage-state.json \
  https://www.twitch.tv/
```

`storage-state.json` 等同 Twitch 登入憑證。不要提交 Git、放入 Docker image、上傳到 issue 或分享給他人。若懷疑外洩，請立即在 Twitch 登出所有裝置或撤銷 session，然後重新匯出。

## 啟動服務

啟動前確認必要檔案：

```bash
test -r config.yml
test -r data/browser-state/storage-state.json
docker compose config
```

建置並啟動：

```bash
docker compose build
docker compose up -d
docker compose logs -f twitch-watchdog
```

`docker compose build` 會以 `APP_UID`／`APP_GID`（預設 1000）建立容器內的非 root 使用者；若主機上的 `config.yml` 擁有者不是 1000:1000，請以 `--build-arg APP_UID=<uid> --build-arg APP_GID=<gid>` 對齊，讓 Bot 的設定寫回（R8）可以寫入。
`docker compose build` creates the in-container non-root user from `APP_UID`/`APP_GID` (default 1000). If the host `config.yml` is not owned by 1000:1000, pass `--build-arg APP_UID=<uid> --build-arg APP_GID=<gid>` so bot config writeback can persist changes.

服務 log 以一行一筆 JSON 輸出到 stdout，可使用 Docker logs 查看與保存：
Service logs are emitted as JSON Lines to stdout and can be viewed or retained through Docker logs:

```bash
docker compose logs -f twitch-watchdog
docker compose logs --no-log-prefix twitch-watchdog
```

若要排查排程卡住、page crash 或瀏覽器清理問題，可暫時將 `config.yml` 的 `log_level` 設為 `debug` 後重啟服務。
To diagnose stuck scheduler ticks, page crashes, or browser cleanup issues, temporarily set `log_level` to `debug` in `config.yml` and restart the service.

```bash
docker compose logs --no-log-prefix twitch-watchdog \
  | rg 'scheduler_tick_|scheduler_stall_detected|session_(reconcile|invalidate|start_attempt)|browser_(page_invalidation|resource_close|navigation_failure)|page_crashed|page_closed|page_refresh_failed'
```

### 瀏覽器診斷事件 / Browser diagnostics

每個受管理的頻道頁面都會安裝請求與 console 診斷，輸出三個事件：

- `browser_request_failed`（warn / debug）：`requestfailed`，記錄 `endpointCategory`、`host`、`path`、`method`、`resourceType`、`failureText` 與（GraphQL 請求時）`graphQlOperationNames`。第三方追蹤/分析（`third_party`、`twitch_other`、`unknown`）與被瀏覽器中斷的請求（`net::ERR_ABORTED`，例如換畫質時被取消的 segment）以 debug 記錄；其餘 Twitch 核心端點失敗以 warn 記錄。
- `browser_http_response`（warn / debug）：非成功 HTTP 回應以 warn 記錄；`log_level: debug` 時，成功的 Twitch bootstrap 回應（document、script、GraphQL、API）以 debug 記錄。例行 media segment、圖片、字型與第三方分析不記錄。
- `browser_console_message`（warn / debug）：console `error` 以 warn 記錄；來源為例行噪音主機（`third_party`、`twitch_other`、`unknown`）或符合已知噪音 pattern（如 `SpadeClient send error`、`failed integrity check`）的 error 以 debug 記錄；`warning` 以 debug 記錄；一般 `log` 訊息不記錄。

所有事件都帶 `channel`、`browserGeneration` 與 `pageGeneration`。URL 只保留 hostname 與 pathname，**刻意排除 query string、cookie、request/response body 與 GraphQL variables**。

Every managed channel page installs request and console diagnostics, emitting three events:

- `browser_request_failed` (warn / debug): `requestfailed` with `endpointCategory`, `host`, `path`, `method`, `resourceType`, `failureText`, and `graphQlOperationNames` for GraphQL requests. Third-party tracking/analytics (`third_party`, `twitch_other`, `unknown`) and browser-aborted requests (`net::ERR_ABORTED`, for example segments cancelled by a quality switch) log at debug; other Twitch core endpoint failures log at warn.
- `browser_http_response` (warn / debug): non-success HTTP responses log at warn; with `log_level: debug`, successful Twitch bootstrap responses (document, script, GraphQL, API) log at debug. Routine media segments, images, fonts, and third-party analytics are excluded.
- `browser_console_message` (warn / debug): console `error` logs at warn; errors whose source is a routine-noise host (`third_party`, `twitch_other`, `unknown`) or that match a known noise pattern (e.g. `SpadeClient send error`, `failed integrity check`) log at debug; `warning` logs at debug; ordinary `log` messages are ignored.

All events carry `channel`, `browserGeneration`, and `pageGeneration`. URLs keep only the hostname and pathname, **deliberately excluding query strings, cookies, request/response bodies, and GraphQL variables**.

```bash
docker compose logs --no-log-prefix twitch-watchdog \
  | rg 'browser_(request_failed|http_response|console_message)|page_health_failed|browser_disconnected'
```

常用操作：

```bash
docker compose restart twitch-watchdog
docker compose stop
docker compose down
```

Compose 會將 `config.yml` 以可寫 bind mount 掛載，供 Telegram/Discord 管理指令持久化設定；`data/browser-state` 仍為唯讀。`config.yml` 必須可由容器內的非 root 使用者（`APP_UID`／`APP_GID`）寫入，請使用擁有者或群組權限處理，不要用全域可寫權限部署正式服務。

## Telegram 管理

Telegram 整合使用 Bot API 長輪詢，不需要開放 HTTP port。
Telegram integration uses Bot API long polling and does not require an exposed HTTP port.

1. 在 Telegram 與 [@BotFather](https://t.me/BotFather) 對話，用 `/newbot` 建立 bot 並取得 token。
2. 對你的 bot 傳送任意訊息。
3. 使用 Telegram Bot API `getUpdates` 取得 `message.chat.id`。
4. 在 `config.yml` 設定：

```yaml
telegram:
  enabled: true
  bot_token: 你的BotToken
  allowed_chat_ids:
    - "你的ChatID"
```

Telegram chat ID 必須是加引號的整數字串，例如 `"5009748887"` 或 `"-1001234567890"`，不是 `@username`。只有清單內的 chat 可以查詢或控制服務。
Telegram chat IDs must be quoted integer strings such as `"5009748887"` or `"-1001234567890"`, not `@username`. Only listed chats can query or control the service.

支援指令：

- `/status`：服務、開台與觀看狀態。
- `/channels`：監控頻道與最近一次狀態。
- `/refresh`：顯示正在觀看頻道的播放器重整倒數。
- `/refresh_now`：立即重整所有觀看中頻道。
- `/refresh_now 頻道名稱`：立即重整指定觀看中頻道。
- `/points`：顯示所有觀看中頻道的忠誠點數。
  Shows channel-point balances for all actively watched channels.
- `/points 頻道名稱`：顯示指定觀看中頻道的忠誠點數。
  Shows the channel-point balance for one actively watched channel.
- `/config`：顯示目前頻道與最大同時觀看數。
- `/channel_add 頻道名稱`：新增監控頻道。
- `/channel_remove 頻道名稱`：移除監控頻道。
- `/channels_set 頻道一,頻道二`：取代完整頻道清單。
- `/max_streams 數量`：調整最大同時觀看數。
- `/check`：立即檢查 Twitch 狀態。
- `/pause`：暫停自動檢查。
- `/resume`：恢復自動檢查。
- `/screenshot`：回傳所有觀看中頻道截圖。
- `/screenshot 頻道名稱`：回傳指定觀看中頻道截圖。
- `/help`：顯示指令說明。

Telegram 的 `/points` 與 `/screenshot` 在背景執行，不會阻塞 `/status` 或後續收訊。同時最多處理兩筆點數／截圖指令，30 秒未完成會回覆逾時；底層頁面操作結束前仍占用名額，避免反覆重試堆積。逾時或服務停止後不再傳送晚到的結果。若持續收到忙碌回覆，請查看服務紀錄中的 `telegram_query_timeout` 與頁面操作紀錄。
Telegram runs `/points` and `/screenshot` in the background so `/status` and polling remain responsive. At most two queries run at once; after 30 seconds the bot replies with a timeout. A query retains its slot until its underlying page operation finishes to prevent retry buildup. Late results are suppressed after timeout or shutdown. If busy replies persist, inspect `telegram_query_timeout` and page-operation logs.

Telegram 收訊的網路逾時為 `polling_timeout_seconds` 加 10 秒；逾時會記錄 `telegram_poll_failed` 並依既有退避機制重試。
Telegram polling has a network deadline of `polling_timeout_seconds` plus 10 seconds. Timeouts log `telegram_poll_failed` and retry with the existing backoff.

Bot 主動推播：服務啟動／停止、開台／下播、忠誠點數成功或 click 失敗、page crash、browser 重啟、容器即將重啟（致命錯誤）。定時／手動重整**不會**推播。
Proactive bot pushes: service start/stop, stream online/offline, reward claimed or click failure, page crash, browser restart, and upcoming container restart (fatal path). Scheduled/manual player refresh does **not** push.

Bot token 具有管理能力，不得提交 Git 或貼到任何公開位置。

## Discord 管理

Discord 整合使用 Gateway WebSocket 接收 slash command，並用 Discord REST API 回覆訊息；不需要開放 HTTP port。
Discord integration receives slash commands through the Discord Gateway WebSocket and replies through the Discord REST API; no exposed HTTP port is required.

1. 到 Discord Developer Portal 建立 Application，新增 Bot，並取得 bot token。
   Create an Application in the Discord Developer Portal, add a Bot, and copy the bot token.
2. 在 Application 的 OAuth2 URL Generator 勾選 `bot` 與 `applications.commands` scopes，Bot Permissions 至少需要 `Send Messages` 與 `Attach Files`。
   In OAuth2 URL Generator, select `bot` and `applications.commands`; the bot needs at least `Send Messages` and `Attach Files`.
3. 邀請 bot 到你的 Discord server，並複製 Application ID 與允許使用指令的 channel ID。
   Invite the bot to your server, then copy the Application ID and allowed channel IDs.
4. 在 `config.yml` 設定：

```yaml
discord:
  enabled: true
  bot_token: 你的DiscordBotToken
  application_id: 你的ApplicationID
  guild_id: 你的GuildID
  allowed_channel_ids:
    - "你的ChannelID"
  allow_direct_messages: false
  allowed_user_ids: []
```

`discord.guild_id` 選填。填入時 slash command 會註冊為 guild command，通常較快生效；留空時會註冊為 global command，可能需要較久才出現在 Discord。
`discord.guild_id` is optional. When set, slash commands are registered as guild commands and usually become available faster; when empty, commands are global and may take longer to appear.

Discord channel ID 必須是加引號的數字 snowflake ID，例如 `"1520430059771400353"`，不是 `#general`。只有清單內的 Discord channel 可以查詢或控制服務。
Discord channel IDs must be quoted numeric snowflake IDs such as `"1520430059771400353"`, not `#general`. Only listed Discord channels can query or control the service.

若要允許 Discord 私訊使用，設定 `discord.allow_direct_messages: true`，並在 `discord.allowed_user_ids` 填入允許控制服務的 Discord user ID。
To allow Discord direct messages, set `discord.allow_direct_messages: true` and put authorized Discord user IDs in `discord.allowed_user_ids`.

```yaml
discord:
  allow_direct_messages: true
  allowed_user_ids:
    - "你的DiscordUserID"
```

Discord user ID 必須是加引號的數字 snowflake ID。私訊只接受清單內的使用者；server channel 白名單仍會照常套用。
Discord user IDs must be quoted numeric snowflake IDs. Direct messages are accepted only from listed users; server channel allowlisting still applies.

支援 slash command：

- `/status`：服務、開台與觀看狀態。
- `/channels`：監控頻道與最近一次狀態。
- `/refresh`：顯示正在觀看頻道的播放器重整倒數。
- `/refresh_now`：立即重整所有觀看中頻道。
- `/refresh_now channel:頻道名稱`：立即重整指定觀看中頻道。
- `/points`：顯示所有觀看中頻道的忠誠點數。
  Shows channel-point balances for all actively watched channels.
- `/points channel:頻道名稱`：顯示指定觀看中頻道的忠誠點數。
  Shows the channel-point balance for one actively watched channel.
- `/config`：顯示目前頻道與最大同時觀看數。
- `/channel_add channel:頻道名稱`：新增監控頻道。
- `/channel_remove channel:頻道名稱`：移除監控頻道。
- `/channels_set channels:頻道一,頻道二`：取代完整頻道清單。
- `/max_streams count:數量`：調整最大同時觀看數。
- `/check`：立即檢查 Twitch 狀態。
- `/pause`：暫停自動檢查。
- `/resume`：恢復自動檢查。
- `/screenshot`：回傳所有觀看中頻道截圖。
- `/screenshot channel:頻道名稱`：回傳指定觀看中頻道截圖。
- `/help`：顯示指令說明。

Bot 主動推播：服務啟動／停止、開台／下播、忠誠點數成功或 click 失敗、page crash、browser 重啟、容器即將重啟（致命錯誤）。定時／手動重整**不會**推播。
Proactive bot pushes: service start/stop, stream online/offline, reward claimed or click failure, page crash, browser restart, and upcoming container restart (fatal path). Scheduled/manual player refresh does **not** push.

Discord bot token 具有管理能力，不得提交 Git 或貼到任何公開位置。
Discord bot tokens grant management capability. Do not commit them to Git or post them publicly.

## 安全與限制

- 僅能用於你自己擁有或獲授權使用的 Twitch 帳號。
- 本工具不保證任何特定使用方式符合平台規則。
- 不要把 Twitch 或其他不受信任網站內容視為可信輸入。
- Twitch DOM 與忠誠點數 selector 可能變更；找不到按鈕時服務會繼續觀看並等待後續檢查。

開發、測試、架構與 smoke test 請見 [README_DEV.md](README_DEV.md)；與 Node.js 版的差異與升級步驟請見 [CHANGELOG.md](CHANGELOG.md)。
For development, testing, architecture, and smoke tests see [README_DEV.md](README_DEV.md); for differences from the Node.js edition and upgrade steps see [CHANGELOG.md](CHANGELOG.md).
