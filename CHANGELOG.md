# Changelog

本檔記錄 twiclaimer（Twitch Watchdog 的 Rust 版）與 Node.js 版（基準 `d49363d`）之間的差異與切換步驟。使用說明見 [README.md](README.md) 與 [完整說明](docs/zh-TW/README.md)；開發與測試見 [README_DEV.md](README_DEV.md)。

## Unreleased

- 新增 GitHub Actions（`.github/workflows/docker-image.yml`）：push 到 `main` 先跑 Docker test stage，通過後才把正式 image 發布到 `ghcr.io/yeeeee0529/twiclaimer`（`latest`＋commit SHA，`APP_UID`／`APP_GID` = 1001）。`docker-compose.yml` 的 image 可由 `TWICLAIMER_IMAGE` 覆寫，預設仍為 `twiclaimer:local`。伺服器用 `docker compose pull` 取代本機編譯。
- `Dockerfile`：release build 不再掛載 `/src/target` 快取。

## 0.1.0 (2026-11-04)

Twitch Watchdog 的 Rust 移植首版。以舊版原始碼為行為依據逐模組移植（設定、日誌、CDP 驅動、session、排程、Telegram、Discord），舊行為、設定檔欄位與日誌事件名稱逐字對齊；僅下列 X1–X10 為允許的差異。

### 允許的差異

| 編號 | 項目 | 舊版（Node.js） | twiclaimer（Rust） |
| --- | --- | --- | --- |
| X1 | `browser.engine: firefox` | 支援 | `ConfigValidationError`：`設定欄位 browser.engine 必須是 chromium` |
| X2 | `headless: false`（含 `HEADLESS=false`） | 開啟有畫面的瀏覽器 | `ConfigValidationError`：`設定欄位 headless 必須是 true`（檢查覆寫後的值） |
| X3 | `config.yml` 寫回格式 | 整份重新序列化，可能改變引號、縮排、flow 樣式，並刪除被取代節點內與同行的註解 | 只取代兩個值的文字，其餘位元組不變；寫入的值、錯誤與訊息相同 |
| X4 | Discord Gateway 斷線 | 不重連，Bot 停擺到服務重啟 | twilight 自動重連與 resume |
| X5 | 日誌欄位 | 含 Node 專屬欄位 | Error 沒有 `stack`；不輸出 `processHeapUsedBytes`、`processExternalBytes`、`nodeVersion`、`playwrightVersion` |
| X6 | 未處理的錯誤 | `uncaught_exception`、`unhandled_rejection` | 沒有 `unhandled_rejection`；panic 記為 `uncaught_exception`，後續流程與 exit code 相同 |
| X7 | 瀏覽器操作的錯誤訊息 | Playwright 訊息含 `Call log:` 多行 | 只有第一行，內容與 Playwright 相同 |
| X8 | Locator 的頁面內腳本 | Playwright 的 injected script 在 utility world 執行 | 同一份 injected script（`tools/ref/dump-injected.mjs` 取出）在 main world 執行，以 handle 持有、不設全域變數 |
| X9 | 暫存目錄名稱 | `playwright_chromiumdev_profile-*` 等 | `twiclaimer-*` |
| X10 | Bot 訊息文字 | 啟停通知為「Twitch Watchdog 已啟動／已停止」；推播與狀態回覆前綴 🔴、⚫、🎁、⚠️，單句回覆以句號結尾 | 改為「twiclaimer 已啟動／已停止」；推播改為純文字，單句回覆不加句號。`/status` 頻道清單保留狀態圓點 |

### 執行環境

- Binary：`twiclaimer`（Rust 1.99.1，`cargo build --release --locked`）。
- 瀏覽器：Chromium headless shell `151.0.7922.34`（Playwright `1.62.1` 同版），經 `--remote-debugging-pipe` 驅動；正式 image 以 `ubuntu:noble` 為底，不含 Node.js、原始碼與測試。
- 登入工具與 CSV 匯出維持 Node.js（`npm run twitch:login`／`npm run benchmark:csv`），不進正式 image。

### 升級步驟（從 twitch-watchdog-docker 切換）

以下假設舊版目錄 `twitch-watchdog-docker` 與本專案目錄 `twiclaimer` 並列在同一個上層目錄。

```bash
cd twiclaimer
docker compose build --build-arg APP_UID=<舊容器的 uid> --build-arg APP_GID=<舊容器的 gid>
cp -a ../twitch-watchdog-docker/config.yml ./config.yml
mkdir -p data && cp -a ../twitch-watchdog-docker/data/browser-state ./data/
(cd ../twitch-watchdog-docker && docker compose down)
docker compose up -d
docker compose logs -f --no-log-prefix twitch-watchdog    # 確認 service_started
```

`APP_UID`／`APP_GID` 填入主機上 `config.yml` 的擁有者 uid 與 gid（與舊版容器內使用者相同），讓 Bot 的設定寫回可以寫入 `config.yml`。`container_name` 維持 `twitch-watchdog`，維運指令不用改。

### 回復

```bash
cd twiclaimer && docker compose down
diff config.yml ../twitch-watchdog-docker/config.yml      # Bot 改過設定時，先把這份複製回舊版目錄
cd ../twitch-watchdog-docker
docker tag twitch-watchdog:rollback-e0e079a twitch-watchdog:local
docker compose up -d --no-build
```

切換後的觀察期內若出現 OOMKilled、RestartCount 異常增加、長時間沒有 `reward_claimed`、`error` 頻率超過基準 2 倍或 Bot 無回應，依 README_DEV.md 的診斷章節收集日誌並回復到舊版。
