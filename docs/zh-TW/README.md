# twiclaimer 完整說明

[English](../en/README.md) · [快速總覽](../../README.md) · [開發者文件](../../README_DEV.md) · [變更紀錄](../../CHANGELOG.md)

twiclaimer 是可用 Docker 長時間執行的 Twitch 觀看輔助服務（Twitch Watchdog 的 Rust 版）。它透過 Twitch API 檢查指定頻道是否開台，依設定的優先序開啟觀看頁面，並自動領取 Bonus Channel Points。

本專案不會自動登入 Twitch，也不會要求你提供 Twitch 密碼。你需要自行登入 Twitch 後匯出 Playwright `storageState`，並提供 Twitch Developer App 的 Client ID 與 Client Secret。

以下指令都在專案根目錄執行。

## 目錄

- [功能](#功能)
- [前置需求](#前置需求)
- [快速開始](#快速開始)
- [資源防護與 Docker 限制](#資源防護與-docker-限制)
- [Twitch API 設定](#twitch-api-設定)
- [匯出 Twitch 登入狀態](#匯出-twitch-登入狀態)
- [啟動與維運](#啟動與維運)
- [Telegram 管理](#telegram-管理)
- [Discord 管理](#discord-管理)
- [疑難排解](#疑難排解)
- [安全與限制](#安全與限制)

## 功能

- 監控多個 Twitch 頻道的開台狀態，依 `channels` 順序與 `max_concurrent_streams` 決定實際觀看的頻道。
- 以 Chromium 播放直播，預設畫質 `160p` 並靜音，降低長時間執行的資源用量。
- 自動領取 Bonus Channel Points。
- 自動確認直播頁的內容警示（`Start Watching`），只點擊已登入帳號看得到的確認按鈕。
- 啟動與頁面重整後自動收合左側推薦頻道欄。
- 領取連續失敗 10 次時先重整該頻道頁面；重整後再連續失敗 10 次，程序結束並交由 Docker restart policy 重啟容器。
- 排程檢查長時間卡在執行中時，容器級 watchdog 會記錄 `scheduler_stall_detected` 並結束程序，由 Docker restart policy 重啟容器。
- 可選用 Telegram Bot 或 Discord Bot，查詢狀態與忠誠點數、管理頻道、暫停或恢復排程、取得截圖。

不支援 Twitch Drops 自動領取、自動輸入帳號密碼、多帳號批量管理、CAPTCHA 繞過、反偵測與規避平台限制。

## 前置需求

- Docker Engine 與 Docker Compose plugin
- 一個 Twitch 帳號
- Twitch Developer Console 建立的 App Client ID 與 Client Secret
- 若要在本機匯出登入狀態：Node.js 24 以上與 npm 11

正式容器以 `ubuntu:noble` 為底，搭配與 Playwright `1.62.1` 同版的 Chromium headless shell（`151.0.7922.34`），不含 Node.js、原始碼與測試。

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

### 設定重點

| 欄位 | 說明 |
| --- | --- |
| `channels` | Twitch login name 清單，順序就是觀看優先序 |
| `check_interval_seconds` | Twitch API 輪詢間隔，最小 30 秒 |
| `max_concurrent_streams` | 最大同時觀看數 |
| `storage_state_path` | 容器內 Playwright storageState 的路徑 |
| `browser.engine` | 只支援 `chromium`；設為 `firefox` 會在載入設定時以 `ConfigValidationError` 失敗 |
| `headless` | 只支援 `true`；設為 `false`（含 `HEADLESS=false` 覆寫）會在載入設定時失敗 |
| `browser.stream_quality` | 預設 `160p`；設為 `auto` 可停用強制畫質 |
| `browser.page_refresh_interval_seconds` | 預設 `0`，不做定時重整以降低記憶體壓力；設為正值會依頻道錯開重整。Twitch 可能在重整後才顯示可領取的按鈕，所以保留此功能。手動 `/refresh_now` 不受影響 |
| `browser.resource_telemetry_interval_seconds` | 預設 60 秒輸出一次 `runtime_resource_snapshot`（含 cgroup 欄位） |
| `browser.resource_guard` | 容器級記憶體防護，見下一節 |

Session 啟動在短重試後仍發生 `page.goto` 導覽逾時時：5 分鐘內同一頻道 2 次，或全部頻道合計 3 次，會回收共用瀏覽器；10 分鐘內發生 3 次這類回收，會要求 Docker 重啟容器。定時與手動重整的逾時不納入計數。

## 資源防護與 Docker 限制

Docker Compose 預設限制以約 3 個同時觀看為基線：

| 設定 | 值 | 說明 |
| --- | --- | --- |
| `mem_limit` | 6g | 容器 RAM 上限 |
| `memswap_limit` | 7g | RAM 與 swap 合計上限（約 1g swap） |
| `pids_limit` | 512 | 行程與執行緒上限 |

`max_concurrent_streams` 明顯高於 3（例如 5）時，請把記憶體上限提高到約 10g／11g，再 recreate 容器。

`browser.resource_guard` 以整個容器的 cgroup 記憶體（含瀏覽器）為準，不只看 twiclaimer 控制端程序。YAML 內的 MiB 門檻是 `baseline_streams`（預設 3）的錨點；`scale_with_streams: true` 時依下式縮放：

```text
effective = base_memory_mib + (anchor - base_memory_mib) * (max_concurrent_streams / baseline_streams)
```

預設行為：

- 到達 warning 門檻：記錄 `resource_guard_warning`（有 hysteresis）。
- 連續取樣達 recycle 門檻：回收瀏覽器（`resource_guard_browser_recycle_requested`）。
- emergency、swap 或 cgroup OOM 事件：記錄後以 exit code 1 結束程序，由 Docker 重啟。
- 瀏覽器回收期間仍持續檢查 emergency、swap 與 cgroup OOM；一般 warning、recycle 與成長率規則暫停，避免 session 回填被誤判。
- `fast_memory_growth` 只在目前記憶體已達 warning 以上，且視窗內成長量達門檻時才重啟容器；瀏覽器重啟後 120 秒內忽略此規則。
- 沒有 cgroup v2 時只記錄 `cgroup_metrics_unavailable`，保護仍靠 Docker 的 hard limit。

查詢 cgroup 遙測：

```bash
docker compose logs --no-log-prefix twitch-watchdog \
  | rg 'runtime_resource_snapshot|resource_guard_|cgroup_metrics_unavailable|container_restart_requested'
```

## Twitch API 設定

到 Twitch Developer Console 建立應用程式，取得 Client ID 與 Client Secret，填入 `config.yml`：

```yaml
twitch_api:
  client_id: 你的ClientID
  client_secret: 你的ClientSecret
```

服務以 Client Credentials flow 取得 App Access Token，並在 token 無效、接近過期或 Helix API 回傳 HTTP 401 時重新取得。Token 只存在程序記憶體，不會寫入 `config.yml`。

`twitch_api.access_token` 只在沒有 Client Secret 時作為手動備援。

## 匯出 Twitch 登入狀態

登入輔助工具是 Node.js 版（`scripts/twitch-login.mjs`），請在本機桌面環境執行。流程：

1. 偵測系統預設瀏覽器。
2. 以臨時 profile 開啟，登入階段不啟用 remote debugging 或 Playwright 旗標。
3. 你手動登入後在終端機按 Enter。
4. 工具以同一個臨時 profile 短暫重開，經 CDP 匯出 Playwright `storageState`。

它不會開啟你的日常瀏覽器 profile。

安裝本機依賴：

```bash
npm ci
mkdir -p data/browser-state
chmod 700 data data/browser-state
```

系統沒有 Chrome 或 Edge 時，可另裝 Playwright Firefox 作為後備：

```bash
npx playwright install firefox
```

執行登入輔助工具：

```bash
npm run twitch:login
```

在開啟的瀏覽器視窗中手動登入 Twitch（含 2FA）。工具偵測到 `auth-token` 後會自動寫入 `data/browser-state/storage-state.json` 並設定權限，也可以在終端機按 Enter 立即存檔。

瀏覽器選擇順序：

1. 系統預設瀏覽器（Playwright 可驅動時，例如 Chrome、Edge、Firefox）
2. Google Chrome
3. Microsoft Edge
4. 系統 Firefox
5. Playwright Firefox

Playwright 無法驅動 Safari，預設為 Safari 時會改用上述後備瀏覽器。可用 `--browser chrome|msedge|firefox` 強制指定。

進階備援（不建議作為預設流程）：

```bash
npx playwright codegen \
  --browser=firefox \
  --save-storage=data/browser-state/storage-state.json \
  https://www.twitch.tv/
```

`storage-state.json` 等同 Twitch 登入憑證。不要提交 Git、放進 Docker image、貼到 issue 或分享給他人。懷疑外洩時，請在 Twitch 登出所有裝置或撤銷 session，再重新匯出。

## 啟動與維運

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

`docker compose build` 以 `APP_UID`／`APP_GID`（預設 1000）建立容器內的非 root 使用者。主機上 `config.yml` 的擁有者不是 1000:1000 時，請加上 `--build-arg APP_UID=<uid> --build-arg APP_GID=<gid>`，讓 Bot 的設定寫回可以寫入。

Compose 以可寫的 bind mount 掛載 `config.yml`，供 Telegram 與 Discord 的管理指令保存設定；`data/browser-state` 維持唯讀。`config.yml` 必須能被 `APP_UID`／`APP_GID` 的使用者寫入，請用擁有者或群組權限處理，不要用全域可寫權限部署。

常用操作：

```bash
docker compose restart twitch-watchdog
docker compose stop
docker compose down
```

### 日誌

服務日誌是每行一筆的 JSON（JSON Lines），輸出到 stdout，可用 Docker logs 查看與保存：

```bash
docker compose logs -f twitch-watchdog
docker compose logs --no-log-prefix twitch-watchdog
```

排查排程卡住、page crash 或瀏覽器清理問題時，先把 `config.yml` 的 `log_level` 改為 `debug` 再重啟：

```bash
docker compose logs --no-log-prefix twitch-watchdog \
  | rg 'scheduler_tick_|scheduler_stall_detected|session_(reconcile|invalidate|start_attempt)|browser_(page_invalidation|resource_close|navigation_failure)|page_crashed|page_closed|page_refresh_failed'
```

### 瀏覽器診斷事件

每個受管理的頻道頁面都會安裝請求與 console 診斷，輸出三種事件：

| 事件 | 層級 | 內容 |
| --- | --- | --- |
| `browser_request_failed` | warn／debug | 記錄 `requestfailed` 的 `endpointCategory`、`host`、`path`、`method`、`resourceType`、`failureText`，GraphQL 請求另附 `graphQlOperationNames`。第三方追蹤與分析（`third_party`、`twitch_other`、`unknown`）以及被瀏覽器中斷的請求（`net::ERR_ABORTED`，例如換畫質時被取消的 segment）記為 debug，其餘 Twitch 核心端點失敗記為 warn |
| `browser_http_response` | warn／debug | 非成功的 HTTP 回應記為 warn。`log_level: debug` 時，成功的 Twitch bootstrap 回應（document、script、GraphQL、API）記為 debug。例行的 media segment、圖片、字型與第三方分析不記錄 |
| `browser_console_message` | warn／debug | console `error` 記為 warn；來自例行噪音主機（`third_party`、`twitch_other`、`unknown`）或符合已知噪音（如 `SpadeClient send error`、`failed integrity check`）的 error 記為 debug；`warning` 記為 debug；一般 `log` 不記錄 |

所有事件都帶 `channel`、`browserGeneration` 與 `pageGeneration`。URL 只保留 hostname 與 pathname，刻意排除 query string、cookie、request／response body 與 GraphQL variables。

```bash
docker compose logs --no-log-prefix twitch-watchdog \
  | rg 'browser_(request_failed|http_response|console_message)|page_health_failed|browser_disconnected'
```

## Telegram 管理

Telegram 整合使用 Bot API 長輪詢，不需要開放 HTTP port。

1. 在 Telegram 與 [@BotFather](https://t.me/BotFather) 對話，用 `/newbot` 建立 bot 並取得 token。
2. 對你的 bot 傳送任意訊息。
3. 用 Bot API 的 `getUpdates` 取得 `message.chat.id`。
4. 在 `config.yml` 設定：

```yaml
telegram:
  enabled: true
  bot_token: 你的BotToken
  allowed_chat_ids:
    - "你的ChatID"
```

Chat ID 必須是加引號的整數字串，例如 `"5009748887"` 或 `"-1001234567890"`，不是 `@username`。只有清單內的 chat 可以查詢或控制服務。

### 指令

| 指令 | 說明 |
| --- | --- |
| `/status` | 服務、開台與觀看狀態 |
| `/channels` | 監控頻道與最近一次狀態 |
| `/refresh` | 正在觀看頻道的播放器重整倒數 |
| `/refresh_now [頻道名稱]` | 立即重整所有觀看中頻道，或只重整指定頻道 |
| `/points [頻道名稱]` | 顯示所有觀看中頻道的忠誠點數，或只顯示指定頻道 |
| `/config` | 目前頻道與最大同時觀看數 |
| `/channel_add 頻道名稱` | 新增監控頻道 |
| `/channel_remove 頻道名稱` | 移除監控頻道 |
| `/channels_set 頻道一,頻道二` | 取代完整頻道清單 |
| `/max_streams 數量` | 調整最大同時觀看數 |
| `/check` | 立即檢查 Twitch 狀態 |
| `/pause` | 暫停自動檢查 |
| `/resume` | 恢復自動檢查 |
| `/screenshot [頻道名稱]` | 回傳所有觀看中頻道的截圖，或只回傳指定頻道 |
| `/help` | 顯示指令說明 |

`/points` 與 `/screenshot` 在背景執行，不會阻塞 `/status` 或後續收訊。同時最多處理兩筆；30 秒未完成會回覆逾時，底層頁面操作結束前仍占用名額，避免反覆重試堆積。逾時或服務停止後，晚到的結果不再傳送。持續收到忙碌回覆時，請查看日誌中的 `telegram_query_timeout` 與頁面操作紀錄。

收訊的網路逾時是 `polling_timeout_seconds` 加 10 秒。逾時會記錄 `telegram_poll_failed`，並依既有退避機制重試。

### 主動推播

服務啟動與停止、開台與下播、忠誠點數領取成功或 click 失敗、page crash、瀏覽器重啟、容器即將重啟（致命錯誤）。定時與手動重整不會推播。

Bot token 具有管理能力，不得提交 Git 或貼到任何公開位置。

## Discord 管理

Discord 整合以 Gateway WebSocket 接收 slash command，並用 Discord REST API 回覆；不需要開放 HTTP port。

1. 到 Discord Developer Portal 建立 Application，新增 Bot 並取得 bot token。
2. 在 OAuth2 URL Generator 勾選 `bot` 與 `applications.commands`，Bot Permissions 至少需要 `Send Messages` 與 `Attach Files`。
3. 邀請 bot 到你的 server，並複製 Application ID 與允許使用指令的 channel ID。
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

`guild_id` 選填。填入時 slash command 註冊為 guild command，通常較快生效；留空則註冊為 global command，可能要等較久才會出現。

Channel ID 必須是加引號的數字 snowflake ID，例如 `"1520430059771400353"`，不是 `#general`。只有清單內的 channel 可以查詢或控制服務。

要允許私訊，設定 `allow_direct_messages: true`，並在 `allowed_user_ids` 填入允許控制服務的 user ID：

```yaml
discord:
  allow_direct_messages: true
  allowed_user_ids:
    - "你的DiscordUserID"
```

User ID 同樣必須是加引號的數字 snowflake ID。私訊只接受清單內的使用者；server channel 白名單仍照常套用。

### Slash command

指令與 Telegram 相同，參數以 `名稱:值` 傳入：

| 指令 | 說明 |
| --- | --- |
| `/status`、`/channels`、`/refresh`、`/config`、`/check`、`/pause`、`/resume`、`/help` | 同 Telegram |
| `/refresh_now [channel:頻道名稱]` | 立即重整所有觀看中頻道，或只重整指定頻道 |
| `/points [channel:頻道名稱]` | 顯示忠誠點數 |
| `/channel_add channel:頻道名稱` | 新增監控頻道 |
| `/channel_remove channel:頻道名稱` | 移除監控頻道 |
| `/channels_set channels:頻道一,頻道二` | 取代完整頻道清單 |
| `/max_streams count:數量` | 調整最大同時觀看數 |
| `/screenshot [channel:頻道名稱]` | 回傳觀看中頻道的截圖 |

主動推播的內容與 Telegram 相同。Discord bot token 具有管理能力，不得提交 Git 或貼到任何公開位置。

## 疑難排解

### 登入異常

- `page_health_failed.reason: login_missing` 表示頁面顯示一般登入按鈕，`login_required` 表示明確的登入阻擋。兩者都不算健康觀看，也不代表忠誠點數正在累積。播放器錯誤與離線仍保留各自的原因。
- 登入異常不會立即重整頁面；達到既有的連續健康失敗門檻後，仍會關閉該頁。
- 健康失敗造成的 session 失效，沿用 `browser.session_start.failure_backoff_seconds`（預設 `[30, 60, 120]`），之後以 `maximum_cooldown_seconds`（預設 900 秒）延後重試，實際重試時間取決於下一次排程。它與啟動失敗共用計數，短暫成功開頁不會清除，穩定達 `browser.recovery.stable_reset_seconds` 才重設。
- `channel_health_recovery_backoff` 記錄頻道、累計失敗數與下次可重試時間 `retryAtMs`。此路徑不算 page crash，也不觸發崩潰通知或瀏覽器崩潰回收。

持續出現登入異常時，在本機執行 `npm run twitch:login` 重新登入並匯出狀態，把檔案放回部署掛載的位置，再重啟服務。HTTP 429 仍記為 warn；重試退避不會解除 Twitch 的限流，也無法自動恢復登入。

## 安全與限制

- 僅限用於你自己擁有或獲授權使用的 Twitch 帳號。
- 本工具不保證任何使用方式符合平台規則。
- 不要把 Twitch 或其他不受信任網站的內容視為可信輸入。
- Twitch DOM 與忠誠點數 selector 可能變更；找不到按鈕時，服務會繼續觀看並等待下一次檢查。
