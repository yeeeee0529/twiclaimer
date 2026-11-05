# Twitch Watchdog → Rust（twiclaimer）：Agent 執行計畫

給執行移植的 coding agent。依本文件從 S0 做到 S5。標【人工】的步驟需要人：先準備好指令與說明，再停下來等回覆。

- 基準：舊版 `twitch-watchdog-docker` commit `e0e079a87f06d5608a9e96ff43de2efc8c53cd23`（2026-09-14）、Playwright 1.62.1、Chrome for Testing headless shell `151.0.7922.34`。
- 新專案：`twiclaimer`（獨立 repo、單一 Cargo package，產出同名 binary）。
- 已確認的範圍：常駐服務只支援 Chromium；登入工具維持 Node + Playwright；Telegram 與 Discord 都在 v1；`config.yml` 寫回、完整瀏覽器診斷、CSV 匯出腳本都照舊。

## 0. 使用方式

- 依 §8 的階段與步驟順序執行。每一步：實作 → 執行該步的驗證 → 更新 `doc/progress.md` → commit（訊息格式 `S1.3: 摘要`）。
- 行為的依據順序如下。§7 與舊版程式碼不一致時，照舊版做，並在 `doc/decisions.md` 記一筆。
  1. §6.2 允許的差異（只有這些可以和舊版不同）
  2. 舊版程式碼 `$OLD/src`
  3. 舊版測試 `$OLD/test`
  4. 本文件 §7 的摘要
  5. `$OLD/README.md`、`$OLD/README_DEV.md`、`$OLD/test/ACCEPTANCE.md`
- 取代 Playwright 的部分（CDP 層）以 Playwright 1.62.1 的實際行為為準：用附錄 A 的工具從 `$REF` 錄製，Rust 版產生同格式的輸出再比對。需要細節時讀 `/tmp/playwright-src`（§3）的原始碼。
- 附錄的檔案逐字放進 repo 對應路徑，不要改寫；要修改時先寫 `decisions.md`。
- 需要解讀或取捨時寫進 `doc/decisions.md`（格式見附錄 D），不要只寫在 commit 訊息。
- 每個階段結束時回報：完成項目、關卡結果、實際耗時、`decisions.md` 新增項目、下一階段的風險。

## 1. 目標與範圍

### 1.1 需求

| 編號 | 需求 | 驗收 |
| --- | --- | --- |
| R1 | 記憶體 | Rust 控制端 RSS ≤60 MiB（實際設定、兩個 Bot 都啟用）；相同頁數下，整容器記憶體中位數 ≤ 舊版的 105% |
| R2 | CPU | 相同頁數下，控制端平均核心數低於舊版；整容器 ≤ 舊版的 105% |
| R3 | image 大小 | 正式 image ≤ 舊版的 1/3（兩版用同一種量法，見 §10） |
| R4 | 常駐服務只支援 Chromium | `browser.engine: firefox` 啟動時報錯（§6.2）；寫進 CHANGELOG |
| R5 | 登入工具維持多瀏覽器 | `npm run twitch:login` 照常支援 Chrome／Edge／Brave／Firefox；匯出的 storageState 可被 Rust 版載入 |
| R6 | Telegram 與 Discord 都保留 | 指令、授權、截圖、通知與舊版相同 |
| R7 | 核心行為不退化 | 24 小時內有成功領取 Bonus；沒有新增 OOM、crash loop 或孤兒程序 |
| R8 | 設定寫回照舊 | Bot 修改的 `channels`／`max_concurrent_streams` 寫回 `config.yml`；其他內容（含註解）保留 |
| R9 | 瀏覽器診斷照舊 | 失敗請求、HTTP 非成功回應、console 的分級、消毒與日誌欄位與舊版相同 |
| R10 | 日誌相容 | 事件名稱、層級、欄位與舊版相同（§6.2 除外）；`snapshot_stats.py` 與 `npm run benchmark:csv` 兩版通用 |

### 1.2 非目標

- 讓領點更快。要更快領到 Bonus，調低 `browser.reward_check_interval_seconds` 即可。
- 常駐服務支援 Firefox 或 `headless: false`。
- 升級 Chromium、加新功能、順手修舊版 bug。發現 bug 時記在 `decisions.md`，維持舊行為。

### 1.3 預期與工期

- 記憶體：控制端 150–213 MiB → 20–60 MiB，約占整容器的 3–9%。
- CPU：上限是 S0 量到的控制端占比；Chromium 解碼影片的成本不變。
- image：現行 `mcr.microsoft.com/playwright:v1.62.1-noble` 壓縮後約 949 MB；只留 headless shell 的 Ubuntu image 粗估 200–250 MB。這部分收益大多來自拿掉多餘的瀏覽器，Rust 額外省的是 Node 執行環境（壓縮後約 50 MB）。

| 階段 | 估計 | 內容 |
| --- | --- | --- |
| S0 基準 | 0.5 天 + 24 小時觀察 | 舊版資源基準、Chromium 參數、image 大小、Go/No-Go |
| S1 CDP 原型 | 4–5 天 | 專案骨架、規格抽取與錄製、CDP 驅動、關卡 G1–G9 |
| S2 核心邏輯 | 4–6 天 | 日誌、設定（含寫回）、憑證、選台、復原政策、resource guard |
| S3 常駐服務 | 7–9 天 | Twitch API、監控、BrowserManager、ChannelSession、SessionManager、排程、AppRunner |
| S4 Bot | 2–4 天 | Telegram、Discord |
| S5 切換 | 2–3 天 + 24–72 小時 | image、文件、A/B 比較、切換與觀察 |

合計約 20–28 個工作天，另加觀察時間。

## 2. 執行規則

### 2.1 必須

- 模組一對一對應舊版（§4.2）。型別與函式名稱照轉（`setChannels` → `set_channels`），方便逐行對照。
- 日誌事件名稱、層級、欄位名稱逐字沿用（`doc/spec/log-events.txt`）。
- 移植的測試在 Rust 測試函式上一行加 `// 原測試：<舊標題>`，標題與 `doc/spec/ts-test-titles.tsv` 完全相同；一個舊測試拆成多個 Rust 測試時，每個都加。不移植的測試寫進 `doc/test-port.md` 並附理由。
- 時間、檔案系統、HTTP、程序、瀏覽器都經 trait 注入。單元測試用 fake 與 `tokio::time::pause()`，不等真實時間；單一單元測試超過 1 秒視為錯誤。
- 所有 channel 都有上限；不在持有鎖時 `await` 瀏覽器或網路操作。
- 每一步結束跑 `scripts/check.sh`，全綠才 commit。
- `Cargo.lock` 進版控；建置一律加 `--locked`。

### 2.2 不得

- 自動化測試連線真實 Twitch、Helix、Telegram、Discord；把真實 token、storageState 或正式環境 log 放進 repo。
- 修改 `$OLD`；改寫已推送的 git 歷史。
- 加入 §5 以外的執行期依賴。需要時先寫 `decisions.md`；會引入新的 C 函式庫，或讓 release binary 增加超過 1 MiB 時，停下來問。
- 使用 chromiumoxide、headless_chrome、fantoccini 這類現成瀏覽器驅動（D8）。
- 在正式程式碼使用 `unwrap()`／`expect()`；唯一例外是附註解說明的不變式。
- 停用測試或加 `#[ignore]`。唯一例外是需要真實 Chromium 的 e2e，統一寫 `#[ignore = "需要 CHROME_PATH"]`。

### 2.3 停止條件

遇到以下情況，停下來回報，等人工決定：

- 某個關卡失敗，同一個問題已試過 3 種不同修法。
- 某階段實際耗時超過估計上限的 1.5 倍。
- 需要 §6.2 以外的行為差異，或必須修改舊版 bug 才能讓測試通過。
- 需要真實帳號、憑證、Docker 主機或正式環境（【人工】步驟）。
- S1 的 G1–G9 任一項做不到且沒有替代方案。這時建議放棄重寫，改做 TypeScript 版的 image 瘦身。

## 3. 環境

```text
<工作目錄>/
├── twitch-watchdog-docker/   舊版（$OLD，唯讀）
└── twiclaimer/               新 repo
```

每個 shell 都要設定（`CHROME_PATH` 在 S1.1 放好 `scripts/` 之後才能設定）：

```bash
export OLD="$(cd ../twitch-watchdog-docker && pwd)"
export REF=/tmp/twitch-watchdog-ref
export CHROME_PATH="$(scripts/fetch-headless-shell.sh)"
```

一次性準備：

```bash
test "$(git -C "$OLD" rev-parse HEAD)" = e0e079a87f06d5608a9e96ff43de2efc8c53cd23
git clone --quiet "$OLD" "$REF"
git -C "$REF" checkout --quiet --detach e0e079a87f06d5608a9e96ff43de2efc8c53cd23
(cd "$REF" && npm ci && npm run build && npx playwright install --only-shell chromium)
git clone --quiet --depth 1 --branch v1.62.1 https://github.com/microsoft/playwright /tmp/playwright-src
```

- `$REF` 是舊版的建置副本，供附錄 A 的工具呼叫舊版實作產生 golden。不要在 `$OLD` 執行 `npm ci`，以免改動舊 repo。
- Linux 套件：Ubuntu 上執行 `(cd "$REF" && npx playwright install-deps chromium)`，或安裝 §11 的套件清單。
- macOS 可以跑單元測試與 e2e。cgroup、`/proc` 相關程式碼只在 Linux 編譯與測試（`#[cfg(target_os = "linux")]`），由 Docker test stage 驗證。
- Rust：`rust-toolchain.toml` 固定 `1.99.1`（components `rustfmt`、`clippy`）；twilight 0.17 需要 1.89 以上。
- Node 24：供 `$REF`、附錄 A 的工具與登入工具使用。

## 4. 專案結構

### 4.1 目錄

```text
twiclaimer/
├── Cargo.toml  Cargo.lock  rust-toolchain.toml
├── Dockerfile  .dockerignore  docker-compose.yml  seccomp_profile.json  config.example.yml
├── README.md  README_DEV.md  CHANGELOG.md
├── package.json  package-lock.json   Node 工具（登入、CSV、compose 驗證），不進正式 image
├── chrome/SHA256SUMS                  headless shell 的 SHA256（附錄 B）
├── src/
│   ├── main.rs                        參數（--version、probe）、啟動 app
│   ├── lib.rs
│   ├── app/  config/  credentials/  logging/  twitch/  scheduler/  sessions/
│   ├── browser/                       對應舊 src/browser
│   │   ├── js/                        頁面內執行的 JS（從 $REF/dist 逐字複製）
│   │   └── cdp/                       取代 Playwright：transport、session 路由、launcher、page、locator、input、network
│   │       └── js/                    Playwright 的 injected script（附錄 A 取出）與其 LICENSE、NOTICE
│   ├── notifications/  telegram/  discord/
│   ├── testing/                       fake（#[cfg(test)]）
│   └── tests/                         跨模組整合測試與需要 Chromium 的 e2e（#[cfg(test)]）
├── testdata/
│   ├── mock-pages/                    複製自 $OLD/test/mock-pages
│   ├── fixtures/                      複製自 $OLD/test/fixtures，另加 storage-state.fake.json
│   └── golden/config/                 附錄 A 產生的設定 golden
├── scripts/                           check.sh、port-coverage.sh、fetch-headless-shell.sh、compare-traces.sh、
│                                      docker-smoke.sh、twitch-login.mjs、lib/、resource-snapshots-to-csv.mjs
├── test-node/                         Node 工具的 Vitest 測試；docker/ 放 compose 驗證
├── tools/ref/  tools/bench/           附錄 A、C
└── doc/
    ├── progress.md  decisions.md  test-port.md  s0-report.md  ab-report.md   附錄 D
    └── spec/                          從舊版抽出的規格與錄製結果（S1.2）
```

- 測試都放在 crate 內：模組測試在 `src/**/tests.rs`，跨模組與 e2e 在 `src/tests/`。這樣可以直接使用內部型別與 `src/testing/` 的 fake，不需要額外的 feature。
- e2e 以 `file://` 開啟 `testdata/mock-pages`，或使用測試內建的本機 HTTP server。

### 4.2 模組對應

| 舊版（`$OLD/src`） | Rust（`src`） | 備註 |
| --- | --- | --- |
| `index.ts`、`app/index.ts`、`app/createApplication.ts` | `main.rs`、`app/create.rs` | 組裝相依 |
| `app/AppRunner.ts`、`app/installProcessHandlers.ts` | `app/runner.rs`、`app/process.rs` | §7.9 |
| `app/CgroupV2Reader.ts`、`app/RuntimeResourceMonitor.ts` | `app/cgroup.rs`、`app/resource_monitor.rs` | Linux 專用部分加 cfg |
| `app/RuntimeDiagnostics.ts` | `app/diagnostics.rs` | fingerprint 以 golden 驗證 |
| `app/ResourceGuardPolicy.ts`、`app/ContainerRestartController.ts`、`app/SchedulerStallWatchdog.ts` | `app/resource_guard.rs`、`app/restart.rs`、`app/stall_watchdog.rs` | |
| `config/*` | `config/{model,loader,yaml,writeback,thresholds,errors}.rs` | §7.1、§7.2 |
| `credentials/*` | `credentials/{validator,storage_state,errors}.rs` | |
| `logging/*` | `logging/{logger,redact}.rs` | §7.7 |
| `twitch/*` | `twitch/{api,live_status,errors}.rs` | |
| `scheduler/*` | `scheduler/{selector,watchdog}.rs` | |
| `sessions/*` | `sessions/{manager,recovery,reconcile}.rs` | |
| `browser/types.ts` | `browser/types.rs` | trait 對應 types.ts 的介面（D12） |
| `browser/BrowserManager.ts`、`browser/ChannelSession.ts` | `browser/manager.rs`、`browser/session.rs` | |
| `browser/ChannelHealthChecker.ts`、`ChannelPointsReader.ts`、`RewardClaimer.ts` | `browser/health.rs`、`points.rs`、`reward.rs` | |
| `browser/ChatDisabler.ts`、`SideNavCollapser.ts`、`StreamPlaybackOptimizer.ts` | `browser/chat.rs`、`side_nav.rs`、`playback.rs` | |
| `browser/browser-diagnostics.ts`、`safe-logging.ts`、`channel-url.ts` | `browser/diagnostics.rs`、`safe_logging.rs`、`channel_url.rs` | |
| `browser/adapters/*` | `browser/cdp/*` | §7.3–§7.5 |
| `notifications/*` | `notifications/*` | |
| `telegram/*`、`discord/*` | `telegram/{api,bot}.rs`、`discord/{api,bot}.rs` | §7.8 |
| `$OLD/scripts/*.mjs`、`$OLD/scripts/lib/*` | 原路徑（Node） | D2、D7 |
| `$OLD/scripts/docker-smoke.sh`、`$OLD/test/docker/*` | `scripts/docker-smoke.sh`、`test-node/docker/*` | §11.3 |

頁面內執行的 JS（`page.evaluate` 的函式、init script）一律從 `$REF/dist` 編譯後的檔案逐字複製到 `src/browser/js/`，再用 `include_str!` 載入。Playwright 送進瀏覽器的就是這段文字；init script 的內容會在 G2 以雜湊比對。

## 5. 依賴

```toml
[package]
name = "twiclaimer"
version = "0.1.0"
edition = "2024"
rust-version = "1.89"
publish = false

[dependencies]
async-trait = "0.1.92"
base64 = "0.23.1"
futures-util = "0.3.34"
libc = "0.2.191"
nix = { version = "0.31.3", features = ["fs", "process", "resource", "signal"] }
regex = "1.13.1"
reqwest = { version = "0.13.5", default-features = false, features = ["rustls", "json", "multipart", "query"] }
rustls = "0.23.46"
ryu-js = "1.0.3"
saphyr-parser = "0.2.2"
serde = { version = "1.0.229", features = ["derive"] }
serde_json = { version = "1.0.154", features = ["preserve_order"] }
sha2 = "0.11.1"
tempfile = "3.27.0"
time = { version = "0.3.55", features = ["formatting", "macros"] }
tokio = { version = "1.53.3", features = ["fs", "io-util", "macros", "net", "process", "rt", "signal", "sync", "time"] }
tokio-util = { version = "0.7.20", features = ["rt"] }
twilight-gateway = { version = "0.17.1", default-features = false, features = ["rustls-platform-verifier"] }
twilight-model = "0.17.1"
url = "2.5.8"
uuid = { version = "1.28.0", features = ["v4"] }

[dev-dependencies]
tokio = { version = "1.53.3", features = ["test-util"] }

[profile.release]
codegen-units = 1
lto = "thin"
strip = true
```

| crate | 用途與注意事項 |
| --- | --- |
| tokio | `current_thread` runtime（D13），`max_blocking_threads(4)` |
| tokio-util | `CancellationToken`、`TaskTracker` |
| serde_json | `preserve_order`：日誌欄位維持插入順序；CDP 訊息與 golden 比對 |
| saphyr-parser | YAML 事件與位置；載入與寫回都用它（D9）。API 以 docs.rs 的 0.2.2 版為準 |
| reqwest | Helix、Telegram、Discord REST。不開 `http2` 與 `system-proxy`，與舊版 Node fetch 相同（HTTP/1.1、不讀 proxy 環境變數） |
| rustls | 和 reqwest 共用 aws-lc-rs provider。`main` 開頭呼叫 `rustls::crypto::aws_lc_rs::default_provider().install_default()`，避免 provider 不明確時 panic |
| twilight-gateway | Discord Gateway。關掉預設的 `twilight-http`（REST 用 reqwest）與 `zstd`（不壓縮） |
| ryu-js | 依 JS `Number#toString` 輸出 f64（日誌、fingerprint、console 文字） |
| nix、libc | `setpgid`／`killpg`、pipe、`getrusage`、`sysconf` |
| url | WHATWG URL，對應舊版的 `new URL()` |
| time | `toISOString()` 格式：`YYYY-MM-DDTHH:MM:SS.sssZ`（UTC、固定 3 位毫秒） |

不在清單內：tracing（日誌自己寫，D10）、hyper／axum（測試 HTTP server 用 `tokio::net::TcpListener` 手寫）、YAML 序列化套件（寫回只取代片段，§7.2）。

## 6. 決策與允許的差異

### 6.1 決策

| 編號 | 決策 | 理由 |
| --- | --- | --- |
| D1 | 常駐服務只支援 Chromium（R4） | Chromium 已是預設；雙瀏覽器相容是工期最大的變數 |
| D2 | 登入工具維持 Node + Playwright，不進正式 image（R5） | 在使用者本機執行，改寫對 R1–R3 沒有幫助；從 Firefox 匯出需要 Playwright |
| D3 | Telegram 與 Discord 都在 v1（R6）。Discord 用 twilight-gateway 且不開 cache；Telegram 用 reqwest 長輪詢 | 心跳、重連、resume 交給函式庫；不開 cache 以守住 R1 |
| D4 | 使用與 Playwright 1.62.1 相同的 headless shell `151.0.7922.34`；啟動參數與 CDP 初始化流程照 Playwright | 瀏覽器不變，A/B 比較只剩控制端一個變因 |
| D5 | `config.yml` 寫回照舊（R8），只取代值的文字（§7.2） | 使用者沿用同一份設定檔；保留註解 |
| D6 | 瀏覽器診斷照舊（R9） | 維持現有除錯能力與日誌 |
| D7 | CSV 匯出腳本照舊，保留 Node 版 | Rust 版沿用同名欄位，腳本不用改 |
| D8 | 自己寫精簡 CDP client，走 `--remote-debugging-pipe`；不用 chromiumoxide 等現成驅動 | 只需要 Playwright 的一小部分，而且初始化順序、auto-attach、錯誤訊息都要和 Playwright 一致才能逐項比對 |
| D9 | YAML 用 saphyr-parser 的事件與位置，自建 JS 相容的值與寫回 | 寫回需要節點位置；載入語意要對齊 yaml 2.9.0 |
| D10 | 日誌自己寫 JSON Lines logger，不用 tracing | 欄位順序、遮罩規則、JS 數字格式要與舊版相同 |
| D11 | Golden 由附錄 A 的工具呼叫 `$REF` 的舊版實作產生，不手寫預期值 | 避免把理解錯誤寫進預期值 |
| D12 | 瀏覽器抽象分兩層：`browser/types.rs` 對應 types.ts 的 `BrowserLauncher`、`BrowserAdapter`、`BrowserContextAdapter`、`BrowserPageAdapter`；`Page`／`Locator` trait 只含舊版用到的 Playwright API（§7.4） | 舊測試用 fake Page／Locator 寫成，介面一致才能逐項移植 |
| D13 | tokio `current_thread`；channel 都有上限；停止以 `CancellationToken` 傳遞；每個頁面只有一個擁有者 task | 舊版是單執行緒；守住 R1 |
| D14 | 單一 crate（lib + bin），不拆 workspace | 規模不大，拆分只增加維護成本 |

### 6.2 允許的差異

只有下表可以和舊版不同。每一項都要有測試，並寫進 CHANGELOG。

| 編號 | 項目 | 舊版 | Rust 版 |
| --- | --- | --- | --- |
| X1 | `browser.engine: firefox` | 支援 | `ConfigValidationError`：`設定欄位 browser.engine 必須是 chromium` |
| X2 | `headless: false`（含 `HEADLESS=false`） | 開啟有畫面的瀏覽器 | `ConfigValidationError`：`設定欄位 headless 必須是 true`（檢查覆寫後的值） |
| X3 | `config.yml` 寫回格式 | 整份重新序列化，可能改變引號、縮排、flow 樣式，並刪除被取代節點內與同行的註解 | 只取代兩個值的文字，其餘位元組不變；寫入的值、錯誤與訊息相同 |
| X4 | Discord Gateway 斷線 | 不重連，Bot 停擺到服務重啟 | twilight 自動重連與 resume |
| X5 | 日誌欄位 | 含 Node 專屬欄位 | Error 沒有 `stack`；不輸出 `processHeapUsedBytes`、`processExternalBytes`、`nodeVersion`、`playwrightVersion` |
| X6 | 未處理的錯誤 | `uncaught_exception`、`unhandled_rejection` | 沒有 `unhandled_rejection`；panic 記為 `uncaught_exception`，後續流程與 exit code 相同 |
| X7 | 瀏覽器操作的錯誤訊息 | Playwright 訊息含 `Call log:` 多行 | 只有第一行，內容與 Playwright 相同（§7.4） |
| X8 | Locator 的頁面內腳本 | Playwright 的 injected script 在 utility world 執行 | 同一份 injected script（`tools/ref/dump-injected.mjs` 取出）在 main world 執行，以 handle 持有、不設全域變數 |
| X9 | 暫存目錄名稱 | `playwright_chromiumdev_profile-*` 等 | `twiclaimer-*` |

## 7. 規格

### 7.1 YAML 載入（取代 `parseYaml`：yaml 2.9.0 `parseDocument` + `toJS`）

舊版在 `config/yaml.ts`。Rust 版用 saphyr-parser 的事件建立 JS 相容的值：

```rust
enum JsValue { Null, Bool(bool), Number(f64), String(String), Array(Vec<JsValue>), Object(Vec<(String, JsValue)>) }
```

`Object` 保留鍵的順序；重複的鍵在建立時就報錯。

- 標量採 YAML 1.2 core schema（yaml 套件預設）：
  - null：`~`、`null`、`Null`、`NULL`、空值。
  - bool：`true`、`True`、`TRUE`、`false`、`False`、`FALSE`。`yes`、`on` 是字串。
  - int：十進位 `[-+]?[0-9]+`（`010` → 10、`+12` → 12）、`0o[0-7]+`、`0x[0-9a-fA-F]+`。`1_000` 是字串。
  - float：`[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?`、`[-+]?\.(inf|Inf|INF)`、`\.nan|\.NaN|\.NAN`。
  - 數字一律存成 f64（超過 2^53 的整數照 JS 失去精度）。
  - 加引號或 block scalar 一律是字串。
  - tag：`!!str` → 字串；`!!int`、`!!float`、`!!bool`、`!!null` 依該型別解析；其他 tag（如 `!custom`）當成沒有 tag 的字串。
- map 的鍵：標量鍵依 JS `String()` 轉成字串（null 鍵為空字串）。
- 錯誤都是 `ConfigParseError`，訊息 `設定檔格式錯誤：YAML 語法無效`，不含行號（舊版 `prettyErrors: false`，所以 `linePos` 永遠是 undefined）：
  - 語法錯誤、多份文件、重複的鍵（`uniqueKeys: true`）。
  - alias 次數過多：照 `$REF/node_modules/yaml/dist/nodes/Alias.js`。每個 anchor 的計數從 1 開始，每解析一次 alias 加 1；計數 × anchor 節點內的 alias 倍數（沒有巢狀 alias 時為 1）> 100 就失敗。
- alias 指向尚未建立完成的祖先節點時，訊息為 `設定檔格式錯誤：YAML 不可包含循環 alias`。
- `<<` 是普通的鍵（YAML 1.2 不支援 merge key）；UTF-8 BOM 先去掉。
- 驗證：`testdata/golden/config/*.load.yml` 全數通過。saphyr 與 yaml 套件判定不同、golden 又沒涵蓋的情況，在 `tools/ref/make-yaml-cases.sh` 補案例，重新產生 golden 後再決定，並記入 `decisions.md`。

設定載入（`ConfigLoader.ts`）逐函式移植，欄位驗證的順序與訊息不變，並加上 X1、X2：

- 設定檔路徑為 `(env.CONFIG_PATH ?? path) || DEFAULT`。檔案不存在 → `ConfigFileNotFoundError`（`找不到設定檔：<path>`）；其他讀取錯誤 → `ConfigParseError`（`無法讀取設定檔 <path>`）。
- `${VAR}` 只替換字串值、不替換鍵；未定義的變數換成空字串。
- 錯誤類別的 `name` 與訊息前綴：`ConfigFileNotFoundError`；`ConfigParseError`（`設定檔格式錯誤：`）；`ConfigValidationError`（`設定欄位 <field> <message>`）。
- 驗證：`testdata/golden/config/*.config.yml`。結果比對 `value`、`logs`（debug 事件）與 `fingerprint`（§7.5 的 `createSafeConfigFingerprint`）；`known-differences.json` 列出的案例改比對該檔的 `expected`（X1、X2）。
- 數字比較一律先轉成 f64（golden 的整數與 Rust 的 f64 不能直接比較 `serde_json::Value`）。

### 7.2 設定寫回（`RuntimeConfigManager.ts`，X3）

操作與驗證逐行照舊版：

- `set_channels`、`set_max_concurrent_streams` 以 `tokio::sync::Mutex` 串行執行（對應 `runExclusive`）。
- 驗證訊息、頻道去重（`toLocaleLowerCase('en-US')` 不分大小寫，保留第一個）、`max_concurrent_streams` 自動降為頻道數，都和舊版相同。
- 順序：驗證 → `persist(next)` → 更新記憶體 → `target.update_config(next)`。`persist` 失敗時記憶體不變。
- 每次 `persist` 都重新讀檔，寫入記憶體中的 `channels` 與 `max_concurrent_streams`（外部改過檔案也一樣，見案例 19）。

`persist` 的演算法：

1. 讀檔（UTF-8）並用 saphyr 解析，記下每個節點的位元組範圍。有任何解析錯誤（含重複的鍵）→ `ConfigParseError`：`設定檔格式錯誤：無法更新含有語法錯誤的設定檔`。根節點不是 map（含空檔、只有註解）→ `設定檔格式錯誤：設定檔根節點必須是物件`。
2. 只找根層的 `channels`、`max_concurrent_streams` 鍵。
3. 產生新的值文字：
   - `channels` 原本是 block sequence：取代第一個 `-` 到最後一個項目內容結尾；沿用原本 `-` 的縮排。
   - `channels` 原本是 flow sequence：取代 `[` 到 `]`，輸出 `[a, b]`。
   - `channels` 原本是其他值或空值：換成 block sequence，縮排為鍵的欄位 + 2。
   - `max_concurrent_streams` 原本是標量：只換標量本身的文字，保留 anchor 與 tag（案例 15）。其他情況換掉整個節點。
   - 鍵不存在時加在檔尾，順序為先 `channels` 後 `max_concurrent_streams`（案例 04）。
   - 頻道名稱可以用 plain scalar，除非它會被 core schema 解析成非字串（例如 `null`、`true`、`123`、`0x1F`、`1e3`），這時改用雙引號。
   - 被取代的範圍內，若有 anchor 被範圍外的 alias 引用 → `Error`：`Unresolved alias (the anchor must be set before the alias): <anchor>`（案例 08）。
   - 換行沿用原檔（有 `\r\n` 就用 `\r\n`）；檔尾沒有換行時補上一個。
4. 以截斷後覆寫的方式寫回同一個檔案（同 `fs.writeFile`），不要用暫存檔再 rename：`config.yml` 是單檔 bind mount，rename 會失敗。

驗證（`testdata/golden/config/*.input.yml`）：

- `results`、`updates`、`writes` 與 `*.expected.json` 相同。
- 有寫檔時，Rust 輸出與 `*.expected.yml` 解析後的值相同（用 §7.1 的載入器）。
- 原檔中與取代範圍沒有交集的行，必須依原順序逐字出現在輸出裡（輸入行是輸出行的子序列）。

### 7.3 CDP 驅動（取代 Playwright，D8）

原則：送出的 CDP 指令（method、params、每個 session 內的順序）與 `$REF` 錄到的 Playwright 1.62.1 相同，以 G2 驗證。下列順序取自錄製結果；實作時以 `doc/spec/traces/*.seq` 為準。參考原始碼：`/tmp/playwright-src/packages/playwright-core/src/server/chromium/` 的 `chromium.ts`、`crConnection.ts`、`crBrowser.ts`、`crPage.ts`、`crNetworkManager.ts`、`crExecutionContext.ts`、`crInput.ts`，以及 `server/` 的 `frames.ts`、`dom.ts`、`page.ts`、`network.ts`。複製 Playwright 原始碼時保留 Apache-2.0 授權聲明。

#### 7.3.1 啟動與關閉（`BrowserLauncher::launch`）

- 執行檔：`CHROME_PATH`。未設定或不可執行時，錯誤訊息 `browserType.launch: Executable doesn't exist at <path>`（未設定時 `<path>` 為空字串）。
- 參數：逐字、依序使用 `doc/spec/chromium-args.txt`（S1.2 從 `$REF` 錄製，S0 再與正式環境核對），只把 `--user-data-dir` 換成自己的暫存目錄。G1 以測試比對。
- 暫存目錄：profile 用 `tempfile::Builder::new().prefix("twiclaimer-profile-")`，下載目錄用 `twiclaimer-artifacts-`，都建在 `std::env::temp_dir()`（正式環境 `HOME=/tmp`）。瀏覽器結束後刪除。
- 程序：`tokio::process::Command`，`process_group(0)`、`kill_on_drop(true)`、stdin 為 null；stdout／stderr 持續讀出後丟棄。
- 管道：`--remote-debugging-pipe`。用 `nix::unistd::pipe2(O_CLOEXEC)` 建兩條 pipe，在 `pre_exec` 內 `dup2` 到子程序的 fd 3（讀指令）與 fd 4（寫回應），`pre_exec` 內只能用 async-signal-safe 的呼叫。父程序端用 `tokio::net::unix::pipe::{Sender, Receiver}::from_owned_fd`。每則訊息是一個 JSON 物件加 `\0`。
- 連線後依錄製順序送出 `Browser.getVersion`、`Target.setAutoAttach {autoAttach: true, waitForDebuggerOnStart: true, flatten: true}`。180 秒內沒有回應 → 終止整個 process group，錯誤 `browserType.launch: Timeout 180000ms exceeded.`。
- `get_version()`：`product` 去掉 `/` 之前的字（`HeadlessChrome/151.0.7922.34` → `151.0.7922.34`），與 Playwright `browser.version()` 相同。
- `close()`：直接寫入 `Browser.close`（Playwright 也是直接寫 pipe，所以不出現在 pw:protocol 紀錄），等程序結束後刪除暫存目錄。`close()` 本身不設逾時，逾時由呼叫端照舊版處理。
- 保證不留孤兒：`close()` 的 future 被取消、呼叫端逾時，或 browser handle 被 drop 時，一律 `killpg(SIGKILL)` 並在背景回收。所有存活的 process group 登記在全域 registry，程序結束前全部 SIGKILL（§7.9）。
- `on_disconnected`：pipe EOF、讀取錯誤或程序結束時觸發一次。之後所有呼叫都回 `Target page, context or browser has been closed`。

#### 7.3.2 連線與 session

- 一條 pipe，多個 session。沒有 `sessionId` 的是 browser session；`Target.attachedToTarget`（flatten）建立子 session，`Target.detachedFromTarget` 結束子 session。
- 指令 id 遞增；回應依 id 交給等待者。CDP 錯誤 `{code, message}` 轉成內部的 `ProtocolError`，再依 §7.4 的表轉成 Playwright 訊息。
- session 結束時，該 session 未完成的指令全部失敗（訊息見 §7.4：已關閉或已 crash）。
- 每個頁面一條有上限的事件 channel（1024）。診斷事件（`Network.*`、`Log.entryAdded`、`Runtime.consoleAPICalled`）用 `try_send`，滿了就丟棄並計數；生命週期事件用 `send().await`，不可丟。
- 事件處理只更新狀態與通知 listener，不在讀取迴圈內等待其他指令。

#### 7.3.3 Context 與 storageState（`BrowserAdapter::new_context`）

依錄製順序（browser session）：

1. `Target.createBrowserContext {disposeOnDetach: true}`。
2. `Browser.setDownloadBehavior {behavior: "allowAndName", browserContextId, downloadPath: <下載暫存目錄>, eventsEnabled: true}`。
3. 讀 storageState 檔。`cookies` 以 `Storage.setCookies {cookies, browserContextId}` 設定，欄位照 `network.ts` 的 `rewriteCookies` 與 `crBrowser.ts` 的 `addCookies` 轉換（`expires: -1` 照原樣傳入，與錄製結果相同）。
4. 有 `localStorage` 的 origin：照 Playwright 開一個暫時頁面（錄製結果的 `page#1`），以 `Fetch.enable` 攔截導覽、`Fetch.fulfillRequest` 回 `<html></html>`，逐一導向各 origin 寫入 localStorage，最後 `Target.closeTarget`。
5. 記下 viewport，供之後的頁面初始化使用。

`BrowserContextAdapter::close` 送 `Target.disposeBrowserContext`。

#### 7.3.4 頁面與 target 初始化

- `new_page()`：`Target.createTarget {browserContextId, url: "about:blank"}`，等 auto-attach 的 session 初始化完成才回傳。
- 頁面 session 依序送出：`Browser.getWindowForTarget`、`Page.enable`、`Page.getFrameTree`、`Log.enable`、`Page.setLifecycleEventsEnabled`、`Runtime.enable`、`Network.enable`、（開啟資源阻擋時：`Network.setCacheDisabled`、`Fetch.enable`）、`Target.setAutoAttach`、`Emulation.setFocusEmulationEnabled`、`Browser.setWindowBounds`、`Emulation.setDeviceMetricsOverride`、`Page.setFontFamilies`、`Emulation.setEmulatedMedia`、每個 init script 一個 `Page.addScriptToEvaluateOnNewDocument {source, runImmediately: true}`、`Runtime.runIfWaitingForDebugger`。參數照錄製結果。
- 錄製結果的參數會隨 viewport 改變，例如 `Emulation.setDeviceMetricsOverride` 的寬高；正式設定的 viewport 來自 `browser.viewport_width`／`viewport_height`。
- auto-attach 依 `targetInfo.type` 處理（照 `crBrowser.ts` 的 `_onAttachedToTarget` 與 `crPage.ts`）：
  - `page`：新頁面；`openerId` 是既有頁面時，初始化完成後對 opener 觸發 popup。popup 也套用 context 的 init script 與資源阻擋。
  - `iframe`（OOPIF）：照 `crPage.ts` 的 frame session 初始化。headless shell 不一定把跨站 iframe 拆成獨立 target，兩種情況都要能運作。
  - `worker`：依序 `Runtime.enable`、`Network.enable`、（阻擋時 `Network.setCacheDisabled`）、`Runtime.runIfWaitingForDebugger`、`Target.setAutoAttach`。
  - 其他類型：照 `crBrowser.ts` 處理。
- iframe 與 worker 的 console、網路事件都歸屬到所屬頁面（與 Playwright 相同）。
- `page.close()`：browser session 送 `Target.closeTarget {targetId}`，等 target 結束。

#### 7.3.5 事件（`BrowserPageAdapter` 的 listener）

| listener | 來源 |
| --- | --- |
| `on_crash` | `Inspector.targetCrashed` |
| `on_close` | 頁面 target 結束（`Target.detachedFromTarget`），或 `close()` 完成 |
| `on_popup` | `openerId` 為本頁的新頁面完成初始化 |
| `on_request_failed` | `Network.loadingFailed`（§7.5） |
| `on_response` | `Network.responseReceived`；redirect 時由 `Network.requestWillBeSent.redirectResponse` 產生前一個 request 的 response |
| `on_console` | `Runtime.consoleAPICalled` 與 `Log.entryAdded`（§7.5） |
| browser `on_disconnected` | pipe EOF、讀取錯誤、程序結束 |

listener 的註冊回傳取消函式（對應舊版的 `() => void`），listener 內的錯誤不得影響其他 listener 與頁面生命週期。

#### 7.3.6 資源阻擋與聊天室封鎖

- `configure_resource_blocking`：三個選項都關閉時不做任何事。任一開啟時，對 context 內所有頁面、popup、worker 啟用 Fetch 攔截（`Network.setCacheDisabled {cacheDisabled: true}`、`Fetch.enable {handleAuthRequests: true, patterns: [{urlPattern: "*", requestStage: "Request"}]}`）。
- `Fetch.requestPaused`：依舊版 `shouldBlockRequest`（`image`、`font`、`spade.twitch.tv`／`science.twitch.tv`）決定 `Fetch.failRequest {errorReason: "BlockedByClient"}` 或 `Fetch.continueRequest`；`Fetch.authRequired` 照 `crNetworkManager.ts` 處理。被擋的請求在診斷中的 `failureText` 是 `net::ERR_BLOCKED_BY_CLIENT.Inspector`。
- `requestPaused` 與 `requestWillBeSent` 的先後不固定，要照 `crNetworkManager.ts` 用 `networkId` 對應兩者。
- `configure_chat_blocking(true)`：把舊版 `buildChatBlockInitScript()` 的輸出加入 context 的 init script。之後建立的每個頁面、popup、OOPIF 都要套用。

#### 7.3.7 錄製與 probe

- `TWICLAIMER_CDP_TRACE=<檔案>`：第一行寫 `<launching> <執行檔> <參數…>`，之後每則送出與收到的訊息各寫一行 `SEND ► <json>`／`◀ RECV <json>`，格式與 Playwright 的 `DEBUG=pw:protocol` 相同，供 `tools/ref/cdp-seq.py` 解析。未設定時不得有額外成本。
- `twiclaimer probe <storage-state.json> <頁面> [--hold 秒數] [--screenshot 檔案]`：開發用子命令，流程與 `tools/ref/pw-trace.mjs` 完全相同（launch → context（viewport 1280×720）→ 資源阻擋 → 聊天室封鎖 → newPage → 註冊診斷 listener → 導覽 → 等待 → 截圖 → 關閉），stdout 輸出同格式的 JSON Lines。
  - `<頁面>`：`testdata/mock-pages` 的檔名，或 http(s) URL（`multi` 頁面請先執行 `node tools/ref/multi-server.mjs` 取得 URL）。
  - 環境變數 `TRACE_BLOCKING`、`TRACE_DISABLE_CHAT` 的意義與 pw-trace 相同。
  - `--hold` 讓頁面多停留指定秒數，結束前輸出 `{"kind":"rss","bytes":…}`（`/proc/self/statm` 的 RSS），供 G9 使用。
  - `--screenshot` 把截圖寫到檔案，供 G4 的人工確認。

### 7.4 Page 與 Locator（`Page`／`Locator` trait）

只實作舊版用到的 API（下表）。分工照 Playwright：

- 頁面內的邏輯（選擇器解析、元素狀態、hit target、事件派送、strict 錯誤訊息）直接使用 Playwright 1.62.1 的 injected script。S1.2 用 `tools/ref/dump-injected.mjs` 從 `$REF` 取出，逐字存成 `src/browser/cdp/js/injected-script.js`，同目錄放 `$REF/node_modules/playwright-core/` 的 `LICENSE` 與 `NOTICE`。版本、位元組數與 SHA-256 記入 `decisions.md`。
- Rust 端照 `frames.ts`、`dom.ts`、`page.ts`、`input.ts` 移植等待、重試、逾時與錯誤訊息。
- 只支援主框架（舊版沒有用 frame locator）。injected script 在主框架 main world 第一次需要時建立（X8）：以 `Runtime.evaluate` 執行 `(() => { const module = {}; <source> return new (module.exports.InjectedScript())(globalThis, <options>); })()`，保留回傳的 objectId，不設全域變數。收到 `Runtime.executionContextDestroyed`／`Runtime.executionContextsCleared` 時丟棄。`<options>` 照 1.62.1 `dom.ts` 的 `injectedScript()` 填 Chromium 的值，其中 `isUtilityWorld: false`、`customEngines: []`。
- 選擇器字串照 client 端的 `locator.ts` 組合：`locator(a).locator(b)` → `a >> b`；`first()` → `a >> nth=0`；`nth(i)` → `a >> nth=<i>`。頁面內一律先 `injected.parseSelector(selector)`，再 `injected.querySelectorAll(parsed, document)`。
- 預設逾時：一般操作 30 000 ms（舊版沒有呼叫 `setDefaultTimeout`）；導覽用 `setDefaultNavigationTimeout` 設定的值。

| 舊版用法 | 行為 | 依據（Playwright 原始碼） |
| --- | --- | --- |
| `page.goto(url, {waitUntil: 'domcontentloaded', signal})` | `Page.navigate`。回應有 `errorText` 就失敗；否則等新文件的 `DOMContentLoaded`，同文件導覽立即完成。`signal` 中止時回 `AbortError` | `frames.ts` 的 `goto` |
| `page.reload({waitUntil: 'domcontentloaded', signal})` | `Page.reload`，之後同 `goto` | `page.ts` 的 `reload` |
| `page.url()` | 主框架目前的 URL（`Page.frameNavigated`、`Page.navigatedWithinDocument`） | `frames.ts` |
| `page.setDefaultNavigationTimeout(ms)` | 只影響 `goto`／`reload` | `timeoutSettings.ts` |
| `page.evaluate(fn, arg)` | 主框架 main world：`Runtime.callFunctionOn {functionDeclaration, executionContextId, arguments: [{value: arg}], returnByValue: true, awaitPromise: true}` | `crExecutionContext.ts` |
| `page.screenshot({type: 'png', fullPage: false})` | viewport 範圍的 PNG：隱藏文字游標、等字型載入，再以 clip 呼叫 `Page.captureScreenshot` | `screenshotter.ts`、`crPage.ts` |
| `page.mouse.move(320, 320)` | 一個 `Input.dispatchMouseEvent {type: 'mouseMoved'}` | `input.ts`、`crInput.ts` |
| `locator.count()` | 不等待，回傳符合的數量 | `frames.ts` 的 `queryCount` |
| `locator.isVisible()` | 不等待；strict；沒有元素時回 false | `frames.ts` 的 `isVisible` |
| `locator.waitFor({state: 'visible', timeout})` | 輪詢到可見為止；strict | `frames.ts` 的 `waitForSelector` |
| `locator.getAttribute(name)` | 等到元素存在；strict | `frames.ts` 的 `getAttribute` |
| `locator.evaluate(fn, arg)` | 等到元素存在；strict；在 main world 以元素為第一個參數呼叫 | client `locator.ts` 的 `_withElement` |
| `locator.evaluateAll(fn)` | 不等待；以所有元素的陣列呼叫 | `frames.ts` 的 `evalOnSelectorAll` |
| `locator.dispatchEvent('click', undefined, {timeout})` | 等到元素存在；呼叫 `injected.dispatchEvent` | `frames.ts` 的 `dispatchEvent` |
| `locator.click({timeout})`、`locator.hover({timeout})` | 見下方步驟 | `dom.ts` |

click 與 hover 的步驟（`frames.ts` 的 `_retryWithProgressIfNotConnected` → `dom.ts` 的 `_retryPointerAction`、`_performPointerAction`）：

1. 解析選擇器並取得唯一元素。沒有元素時照原始碼的間隔輪詢；超過一個時回 strict 錯誤。
2. 進入重試迴圈。第 n 次重試前等待 `[0, 20, 100, 100, 500]` 的第 n 項，之後固定 500 ms：
   1. `injected.checkElementStates(element, states)`：click 為 `['visible', 'enabled', 'stable']`，hover 為 `['visible', 'stable']`。
   2. 捲動：第 0 次用 `DOM.scrollIntoViewIfNeeded`，之後依序以 `scrollIntoView` 的 end、center、start 對齊。
   3. 計算點擊位置：`_clickablePoint`（`DOM.getContentQuads`、`Page.getLayoutMetrics`）。
   4. `injected.setupHitTargetInterceptor(...)`，參數照 `_performPointerAction`；回傳描述字串時重試。
   5. click 依序送出 `mouseMoved`、`mousePressed`、`mouseReleased`（`button: 'left'`、`clickCount: 1`）；hover 只送 `mouseMoved`。
   6. 呼叫 interceptor 的 `stop()`；回傳描述字串時重試。
   7. click 照 `frames.ts` 的 `waitForSignalsCreatedBy` 等待動作觸發的導覽（含 `crPage.ts` 的 `inputActionEpilogue`）；hover 不等。
3. 元素在途中被移除（`error:notconnected`）時回到第 1 步。
4. 整個操作受 `timeout` 限制，逾時錯誤見下表。
5. 不實作：`performActionPreChecks`（舊版沒有 locator handler）、`_markAsTargetElement`（只供 trace）、iframe 內的 hit target 檢查（只支援主框架）。

選擇器：舊版所有選擇器常數（`CHANNEL_HEALTH_SELECTORS`、`COMMUNITY_POINTS_BALANCE_SELECTOR`、`COMMUNITY_POINTS_CLAIM_BUTTON_SELECTOR`、`COMMUNITY_POINTS_SUMMARY_SELECTOR`、`REWARD_BUTTON_LIKE_SELECTOR`、`SIDE_NAV_SELECTOR`、`SIDE_NAV_TOGGLE_SELECTOR`、`SETTINGS_BUTTON_SELECTOR`、`QUALITY_MENU_SELECTOR`、`QUALITY_OPTION_SELECTOR`，以及直接寫在程式裡的 `video`、`[data-a-target="video-player"], video`）集中在 `src/browser/selectors.rs`。e2e 測試 `selectors_parse` 對每一個呼叫 `injected.parseSelector`，都不得拋錯。

#### 錯誤（X7）

錯誤帶 `name` 與 `message` 兩個欄位；舊版的日誌與 `SessionManager` 都會讀取。`message` 為 `<api>: <內容>`，`<api>` 是 `page.goto`、`page.reload`、`page.evaluate`、`page.screenshot`、`locator.<方法>`、`browserContext.newPage` 之一，只保留第一行。

| 情況 | name | message |
| --- | --- | --- |
| 逾時 | `TimeoutError` | `<api>: Timeout <ms>ms exceeded.` |
| strict 違規 | `Error` | `<api>: Error: strict mode violation: locator('button') resolved to 4 elements:`（`Error:` 之後的文字由 injected script 的 `strictModeViolationError` 產生） |
| 導覽失敗 | `Error` | `page.goto: net::ERR_CONNECTION_REFUSED at <url>` |
| 中止 | `AbortError` | `page.goto: This operation was aborted` |
| 頁面內例外 | `Error` | `page.evaluate: Error: boom`（例外 `description` 的第一行） |
| 導覽毀掉 execution context | `Error` | `page.evaluate: Execution context was destroyed, most likely because of a navigation.` |
| crash 之後 | `Error` | `page.evaluate: Target crashed` 再加一個結尾空白（以 `pw-errors.jsonl` 為準） |
| 頁面、context 或瀏覽器已關閉 | `Error` | `<api>: Target page, context or browser has been closed` |

- 完整清單以 `doc/spec/pw-errors.jsonl` 為準。e2e 測試 `src/browser/cdp/tests/errors.rs` 用相同的 HTML 與步驟重跑每個案例，比對 `name` 與 `message`（`127.0.0.1:<port>` 先正規化）；`ok: true` 的案例必須成功。
- `SessionManager.ts` 依賴這些文字：`/Target page, context or browser has been closed/`、`/browser(?: manager)? (?:has been )?closed/`、`/context .*closed/`、`/page .*closed/`、`/target .*crashed/`、`/page\.goto: Timeout \d+ms exceeded/`，以及 `導向非預期 Twitch URL`、`Browser Manager 尚未啟動`。移植後這些分類要用 CDP 層實際產生的錯誤測過一次。

### 7.5 瀏覽器診斷（R9）

三個診斷 listener 照 `PlaywrightBrowserPageAdapter.ts` 移植：欄位、選用欄位的省略條件、消毒（`sanitizeBrowserUrl` + `joinSanitizedUrl`、`redactSensitiveString`）與截斷長度都相同；listener 內的錯誤一律吞掉。

| 事件 | 欄位 |
| --- | --- |
| requestfailed | `url`、`method`、`resourceType`、`failureText`（有值才輸出）、`graphQlOperationNames`（非空才輸出） |
| response | `url`、`method`、`resourceType`、`status`、`statusText`、`graphQlOperationNames`（非空才輸出） |
| console | `type`、`text`（消毒後截斷）、`sourceUrl`（非空才輸出）、`lineNumber`、`columnNumber` |

Playwright 物件的值改由 CDP 事件取得，規則照 `crNetworkManager.ts`、`crPage.ts`、`crExecutionContext.ts`：

| Playwright 值 | CDP 來源 |
| --- | --- |
| `request.url()` | `Network.requestWillBeSent` 的 `request.url` 加上 `request.urlFragment` |
| `request.method()` | `request.method` |
| `request.resourceType()` | 事件的 `type` 轉小寫；沒有時為 `other` |
| `request.postData()` | `request.postDataEntries` 各段 base64 解碼後串接，再以 UTF-8 解碼；沒有時為 null |
| `request.failure().errorText` | `Network.loadingFailed` 的 `errorText` |
| `response.url()`、`status()`、`statusText()` | `Network.responseReceived` 的 `response`；redirect 時取下一個 `requestWillBeSent` 的 `redirectResponse` |
| console 的 `type()`、`text()`、`location()` | `Runtime.consoleAPICalled`：`type` 照原樣；每個參數以 `renderPreview` 轉成文字，以一個空白連接；位置取 `stackTrace.callFrames[0]` 的 `url`、`lineNumber`、`columnNumber`（0 起算） |
| 同上 | `Log.entryAdded`：照 `crPage.ts` 的 `_onLogEntryAdded`，`type` 為 `entry.level`，`text` 為 `entry.text`，欄號為 0 |

- iframe 與 worker 的事件歸屬到所屬頁面（§7.3.4）。
- `renderPreview` 的細節以錄製結果為準。`multi` 頁面已涵蓋：`{a: 1}`、`[1, 2]`、`undefined`、`null`、`NaN`、`-0` 顯示為 `0`、BigInt 顯示為 `undefined`、`Symbol(s)`、`() => 1`、DOM 節點顯示為 `JSHandle@node`、`Map(1)`、巢狀物件顯示為 `{nested: Object}`。
- 下游的分級與日誌（`browser-diagnostics.ts`）在 S2.1 移植純函式，S3.3 接到 `BrowserManager`。
- 驗證：S1.6 移植 `playwright-browser-page-adapter` 的 9 個測試（以假 CDP 事件餵入）；G3 以 `tools/ref/diag-diff.py` 比對 `doc/spec/traces/*.diag.jsonl` 與 probe 的輸出。

### 7.6 常數

| 項目 | 值 |
| --- | --- |
| 一般操作的預設逾時 | 30 000 ms（Playwright 預設） |
| 導覽逾時 | `browser.navigation_timeout_ms`（`setDefaultNavigationTimeout`） |
| 瀏覽器啟動逾時 | 180 000 ms（Playwright 預設） |
| 操作重試間隔 | `[0, 20, 100, 100, 500]` ms |
| 選擇器輪詢間隔 | 照 `frames.ts` |
| viewport | `browser.viewport_width` × `browser.viewport_height` |

其他常數（各種逾時、重試次數、間隔、截斷長度、上限）從舊版原始碼逐字搬到同名的 Rust 常數（`CONTENT_WARNING_SETTLE_TIMEOUT_MS`、`REWARD_CLAIM_CLICK_TIMEOUT_MS` 等），不要重新估算。

### 7.7 日誌（`logging/*`，D10、R10）

- 每筆一行 JSON：`{"level","event","time",…欄位}`，欄位依插入順序；欄位裡的 `level`、`event`、`time` 丟棄。`time` 為 UTC 的 `YYYY-MM-DDTHH:MM:SS.sssZ`。
- 最低層級與 `LEVEL_PRIORITY` 照舊版，預設 `info`。輸出到 stdout；`flush()` 等到 stdout 寫完。
- 消毒照 `sanitizeValue`、`sanitizeError`、`sanitizeMap`、`sanitizeObject`、`isSensitiveKey`、`redactSensitiveString` 逐函式移植：
  - 字串一律經過 `redactSensitiveString`；敏感鍵的值換成 `[REDACTED]`。
  - 非有限的數字轉成字串；f64 用 ryu-js 輸出，與 `JSON.stringify` 相同（`1e21`、`0.1`，`-0` 輸出 `0`）。
  - `[Circular]`、`[Undefined]`、`[Accessor]`、`[Unserializable]`、`[Invalid Date]` 的常數與觸發條件照舊版；Rust 型別不可能出現的情況（getter、symbol）不用模擬。
  - Error 輸出 `{name, message, cause?, …其他欄位}`，沒有 `stack`（X5）。
- 正規表示式從 TypeScript 轉成 `regex` crate 時：
  - JS 的 `\b` 只認 ASCII 字元，改寫成 `(?-u:\b)`。
  - JS 的 `\s` 改寫成明確的字元類別：`[\t\n\x0B\x0C\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]`（Rust 的 `\s` 多了 `\u{85}`、少了 `\u{feff}`）。
  - 旗標照原式（`i` → `(?i)`）。每個轉換過的正規表示式都要有測試，輸入取自舊版測試。
- 事件名稱只能使用 `doc/spec/log-events.txt` 裡的名稱。

### 7.8 Telegram 與 Discord（`telegram/*`、`discord/*`，D3、R6）

共通：

- 指令、授權檢查、回覆與通知文字逐字照舊（含全形標點與換行）。點數照 `toLocaleString('en-US')` 加千分位逗號，例如 `12,345 點`。
- 設定更新失敗時回覆 `設定更新失敗，請確認 config.yml 可寫入。`；錯誤是 `ConfigValidationError` 時改回覆它的訊息。兩者都記 `telegram_config_update_failed`／`discord_config_update_failed`（欄位 `errorType`）。
- HTTP 用 reqwest，每個請求的逾時照舊版（`SEND_TIMEOUT_MS` 10 000 等）；逾時與中止的分類照 `TelegramApiClient.ts`。

Telegram：`TelegramApiClient`、`TelegramBot` 逐函式移植。`getUpdates` 的長輪詢秒數來自 `telegram.polling_timeout_seconds`，`allowed_updates: ["message"]`，`offset` 與失敗後的重試間隔照舊版。

Discord：

- REST（`DiscordApiClient`）用 reqwest，照舊版的路徑與逾時。
- 啟動照 `DiscordBot.start`：`registerCommands`（失敗只記 warn `discord_command_registration_failed`）→ `GET /gateway/bot`（10 秒；失敗則 Bot 啟動失敗，與舊版相同）→ 建立 twilight `Shard` → 廣播啟動訊息。連線 URL 用 twilight 的預設值。
- twilight 設定：`ShardId::ONE`、`Intents::empty()`（舊版 `intents: 0`）、identify properties 為 `twitch-watchdog`／`twitch-watchdog`／`std::env::consts::OS`（同 Node 的 `process.platform`，Linux 都是 `linux`）、`EventTypeFlags::INTERACTION_CREATE`、不開 cache。
- 接收錯誤記 warn `discord_gateway_error`；處理 interaction 失敗記 warn `discord_gateway_message_failed`。可恢復的斷線交給 twilight 重連與 resume（X4）；致命錯誤（例如 token 無效）結束接收迴圈並記 `discord_gateway_error`，不影響常駐服務。
- 停止照 `DiscordBot.stop`：先關閉 gateway（close code 1000），再廣播 `Twitch Watchdog 已停止\n原因：<reason>`。

### 7.9 啟動、關閉與退出（`app/runner.rs`、`app/process.rs`）

- 啟動階段照 `AppRunner.ts`：`config` → `logger` → `credential` → `runtime` → `browser` → `integration` → `scheduler` → 記 `service_started`。失敗時：`config` 階段記 `config_error`，其他階段記 `service_start_failed {phase, error}`；接著執行 cleanup（reason `startup_failed:<phase>`），exit code 1。
- 停止順序：`scheduler.stop` → `reconcile_coordinator.stop(reason)` → `session_manager.stop_all(reason)` → `browser_manager.stop` → integration 依啟動的相反順序 `stop(reason)` → 記 `service_stopped {reason}` → `logger.flush`。單一元件失敗記 `service_stop_component_failed` 後繼續；flush 失敗記 `logger_flush_failed`。
- 訊號：`tokio::signal` 接 SIGINT、SIGTERM，以訊號名稱為 reason 呼叫 stop，exit code 0。stop 本身失敗時記 `service_stop_failed {reason, error}`，exit code 1。
- panic（取代 `uncaughtException`，X6）：panic hook 記 `uncaught_exception {error}`、把 exit code 設為 1，再以 reason `uncaught_exception` 呼叫 stop。release profile 不得設 `panic = "abort"`；app 跑在 spawn 出來的 task 裡，讓 panic 成為 `JoinError`，不會直接結束程序。
- `ContainerRestartController`、`SchedulerStallWatchdog` 要求結束程序時，exit code 與時機照舊版。
- 程序結束前：對 registry 內所有存活的瀏覽器 process group 送 SIGKILL（§7.3.1），flush stdout，再 `std::process::exit(code)`。
- 整個停止流程要在 compose 的 `stop_grace_period`（40 秒）內完成；S3.6 的整合測試以 fake 元件驗證順序與 exit code。

## 8. 階段與步驟

每個階段結束時：跑該階段的關卡 → 依 §0 回報 → 等人工確認後才進下一階段。S0 的 24 小時觀察期間可以先做 S1.1、S1.2。

### S0 基準（0.5 天 + 24 小時）

agent 準備指令與 `doc/s0-report.md`（附錄 D）。在 Docker 主機上執行的步驟都是【人工】，指令在舊版目錄執行；S0.6 由 agent 在 twiclaimer 執行（需先完成 S1.1）。

S0.1【人工】確認版本，保留回復用的 image，量 image 大小（R3 的基準）：

```bash
git rev-parse HEAD        # 必須是 e0e079a87f06d5608a9e96ff43de2efc8c53cd23
docker tag twitch-watchdog:local twitch-watchdog:rollback-e0e079a
docker save twitch-watchdog:rollback-e0e079a | gzip -1 | wc -c
```

S0.2【人工】主機與容器使用者（後兩個值記為 `APP_UID`、`APP_GID`，§11 使用）：

```bash
docker info --format '{{.ServerVersion}} {{.CgroupVersion}}'
uname -m
docker compose exec -T twitch-watchdog id -u
docker compose exec -T twitch-watchdog id -g
```

S0.3【人工】正式環境的 Chromium 參數（找出主要的 headless shell 程序，排除 `--type=` 子程序）：

```bash
docker compose exec -T twitch-watchdog sh -c '
  for p in /proc/[0-9]*; do
    a=$(tr "\0" "\n" < "$p/cmdline" 2>/dev/null) || continue
    case "$a" in *headless_shell*|*headless-shell*) ;; *) continue ;; esac
    case "$a" in *--type=*) continue ;; esac
    printf "%s\n" "$a"; break
  done' | tail -n +2 | sed -E 's#^--user-data-dir=.*#--user-data-dir=<tmp>#' > chromium-args-prod.txt
```

S0.4【人工】照平常的設定跑 24 小時（先確認 `resource_telemetry_interval_seconds` 不是 0）。日誌每 12 小時匯出一次，避免被 log rotation 蓋掉：

```bash
docker compose logs --no-log-prefix --since 13h twitch-watchdog > s0-logs-<n>.jsonl
```

S0.5【人工】24 小時後的狀態：

```bash
docker inspect --format '{{.RestartCount}} {{.State.OOMKilled}}' twitch-watchdog
docker stats --no-stream twitch-watchdog
```

S0.6 人工把 `s0-logs-*.jsonl`、`chromium-args-prod.txt` 與 S0.1、S0.2、S0.5 的輸出交給 agent。agent 把 `chromium-args-prod.txt` 放到 `doc/spec/`，分析日誌並填 `doc/s0-report.md`：

```bash
cat s0-logs-*.jsonl | awk '!seen[$0]++' > s0-logs.jsonl
python3 tools/bench/snapshot_stats.py < s0-logs.jsonl
python3 tools/bench/log-schema.py s0-logs.jsonl > doc/spec/log-schema-s0.tsv
for e in reward_claimed browser_restarted browser_disconnected container_restart_requested scheduler_stall_detected; do
  printf '%s\t%s\n' "$e" "$(grep -c "\"event\":\"$e\"" s0-logs.jsonl)"
done
diff doc/spec/chromium-args-prod.txt doc/spec/chromium-args.txt   # S1.2 之後執行；必須沒有差異
```

S0.7【人工】Go／No-Go：agent 在報告中給建議，人工決定。

報告內容：各頁數的控制端 RSS 中位數、整容器記憶體中位數、CPU 占比、上面 5 種事件的次數、RestartCount／OOMKilled、image 大小、`APP_UID`／`APP_GID`。Go 的建議條件：控制端 RSS 中位數 ≥ 100 MiB，或使用者仍要 R3。`s0-logs*.jsonl` 不進 repo（含正式環境資料）。

### S1 CDP 原型（4–5 天）

目標：用 Rust 驅動 headless shell，完成舊版瀏覽器層的全部行為，並通過 G1–G9。任一關卡做不到又沒有替代方案時，依 §2.3 停止。

#### S1.1 專案骨架

1. 建 repo、`Cargo.toml`（§5）、`rust-toolchain.toml`、`.gitignore`（`target/`、`node_modules/`、`.cache/`、`doc/spec/traces/*.pw.log`、`s0-logs*.jsonl`）。
2. 附錄 A–D 的檔案逐字放到對應路徑，`chmod +x` 所有 `.sh` 與 `.py`。
3. 從舊版複製：

   ```bash
   cp -R "$OLD/test/mock-pages" testdata/mock-pages
   mkdir -p testdata/fixtures && cp -R "$OLD/test/fixtures/." testdata/fixtures/
   mkdir -p scripts/lib test-node/docker
   cp "$OLD/scripts/twitch-login.mjs" "$OLD/scripts/resource-snapshots-to-csv.mjs" "$OLD/scripts/docker-smoke.sh" scripts/
   cp -R "$OLD/scripts/lib/." scripts/lib/
   cp "$OLD/test/unit/default-browser.test.ts" "$OLD/test/unit/resource-snapshots-to-csv.test.ts" test-node/
   cp -R "$OLD/test/docker/." test-node/docker/
   cp "$OLD/seccomp_profile.json" "$OLD/config.example.yml" .
   ```

4. `package.json`：`private`、`"type": "module"`、`engines.node >=24`；`dependencies` 只有 `playwright 1.62.1`，`devDependencies` 只有 `vitest 4.1.8`；scripts 為 `twitch:login`、`benchmark:csv`（指令同舊版）與 `test`（`vitest run test-node`）。修正 `test-node/` 內的 import 路徑後執行 `npm install` 產生 `package-lock.json`。
5. `src/main.rs` 先只有 `--version`（輸出 `twiclaimer <版本>`）。
6. 執行 `tools/ref/extract-spec.sh "$OLD"`（產生 `doc/spec/ts-test-titles.tsv` 397 列與 `log-events.txt`；`check.sh` 需要前者）。
7. 驗證：`cargo generate-lockfile` 後 `scripts/check.sh` 全綠；`npm test` 跑過 7 個 Node 測試。

#### S1.2 規格抽取與錄製

```bash
tools/ref/make-yaml-cases.sh testdata/golden/config "$OLD"
node tools/ref/gen-config-golden.mjs "$REF" testdata/golden/config
node tools/ref/pw-errors.mjs "$REF" > doc/spec/pw-errors.jsonl
mkdir -p src/browser/cdp/js
node tools/ref/dump-injected.mjs "$REF" > src/browser/cdp/js/injected-script.js
cp "$REF/node_modules/playwright-core/LICENSE" "$REF/node_modules/playwright-core/NOTICE" src/browser/cdp/js/
tools/ref/record-traces.sh "$REF"                    # doc/spec/traces/、doc/spec/chromium-args.txt
```

- 逐一看過輸出：`pw-errors.jsonl` 33 列、9 組錄製都有 `.seq` 與 `.diag.jsonl`、`chromium-args.txt` 含 `--remote-debugging-pipe`。
- 連續執行 `record-traces.sh` 兩次，`git diff --stat doc/spec` 必須沒有變化（確認錄製結果穩定）；不穩定的項目記入 `decisions.md` 並在比對工具中處理。
- 從 `$REF/dist` 複製頁面內執行的 JS 到 `src/browser/js/`（§4.2），每個檔案開頭註明來源檔案與函式。
- `doc/spec/` 與 `testdata/golden/` 進版控（`*.pw.log` 除外）。

#### S1.3 transport 與 launcher（§7.3.1、§7.3.2）

- `cdp/transport.rs`：pipe、`\0` 分隔、讀寫 task；`cdp/connection.rs`：指令 id、session 路由、事件 channel；`cdp/launcher.rs`：參數、暫存目錄、process group、registry、`TWICLAIMER_CDP_TRACE`。
- Linux 在 `pre_exec` 另外呼叫 `prctl(PR_SET_PDEATHSIG, SIGKILL)`，控制端被 SIGKILL 時瀏覽器也會結束（G8）。
- 單元測試：訊息被切成多段、一次讀到多則、超過 16 MiB 的訊息（截圖）、session 結束時未完成的指令失敗、事件 channel 滿時丟棄診斷事件但不丟生命週期事件。
- 移植 `playwright-browser-launcher` 的 2 個 Chromium 測試（firefox 那個依 D1 不移植）。
- 關卡 G1、G8。

#### S1.4 context、頁面初始化、auto-attach、probe（§7.3.3–§7.3.5、§7.3.7）

- storageState、`new_page`、worker／iframe／popup 的 attach、`close`，以及 `twiclaimer probe`。
- 關卡 G2、G4。G2 先用 `scripts/compare-traces.sh live-b0` 一組對到相同，再擴大到全部。

#### S1.5 Locator、輸入與錯誤（§7.4）

- injected script 的建立與重建、selector 組合、表內每個 API、click／hover 步驟、screenshot、錯誤對照。
- e2e：`selectors_parse`、`errors.rs`（G5），以及每個 API 至少一個 mock 頁面測試（可見、隱藏、disabled、被遮住、動畫中、導覽中被移除）。

#### S1.6 診斷與資源阻擋（§7.3.6、§7.5）

- 三個診斷 listener、`Fetch` 攔截、聊天室封鎖 init script。
- 移植 `playwright-browser-page-adapter` 的 9 個單元測試，以及 e2e `browser-diagnostics` 的第 1 個（`page adapter 將失敗請求、非成功回應與 console 轉為消毒後的診斷`）。
- 關卡 G3。

#### S1.7 頁面操作模組

- 移植 `ChatDisabler`、`ChannelPointsReader`、`StreamPlaybackOptimizer`、`RewardClaimer` 與其單元測試（`reward-claimer` 19、`channel-points-reader` 5、`stream-playback-optimizer` 6）。
- 移植 e2e：`chat-blocking` 2、`channel-points-reader` 4、`playback-optimizer` 1、`reward-claimer` 10。
- 關卡 G7。

#### S1.8 多頁面、crash 與記憶體

- 3 個頁面同時運作；popup 自動關閉；一個頁面 crash 不影響其他頁面；SIGKILL 瀏覽器後 1 秒內觸發 `on_disconnected`。
- 關卡 G6、G9。

#### S1 關卡

| 關卡 | 驗證內容 | 通過條件與指令 |
| --- | --- | --- |
| G1 | 啟動參數與關閉 | e2e：參數與 `doc/spec/chromium-args.txt` 逐行相同（`--user-data-dir` 除外）；`get_version()` 為 `151.0.7922.34`；`close()`、取消、drop 之後都沒有殘留程序；`CHROME_PATH` 錯誤時的訊息 |
| G2 | CDP 指令順序 | `scripts/compare-traces.sh` 全部顯示「CDP 順序相同」 |
| G3 | 診斷事件 | `scripts/compare-traces.sh` 全部顯示「診斷相同」 |
| G4 | storageState | e2e：假 storageState 的 cookie 與 localStorage 在頁面內讀得到。【人工】用 `npm run twitch:login` 從 Chrome 與 Firefox 各匯出一份，執行 `twiclaimer probe <檔案> https://www.twitch.tv/directory/following --screenshot /tmp/g4-<瀏覽器>.png`，人工確認截圖是登入狀態 |
| G5 | 錯誤訊息 | e2e `errors.rs` 通過 `doc/spec/pw-errors.jsonl` 全部案例 |
| G6 | 多頁面與異常 | S1.8 的 e2e 全過 |
| G7 | 頁面操作 | S1.6、S1.7 的 18 個 e2e 全過 |
| G8 | 不留孤兒程序 | Linux e2e：`probe --hold 30` 期間對 twiclaimer 送 SIGKILL，5 秒內 `pgrep -f twiclaimer-profile-` 沒有結果 |
| G9 | 控制端記憶體 | `twiclaimer probe <假 storageState> <multi URL> --hold 60` 輸出的 `rss` ≤ 40 MiB（R1 的 60 MiB 在 S5 以完整服務驗證） |

階段關卡：`REQUIRE_E2E=1 scripts/check.sh` 全綠，以及：

```bash
scripts/port-coverage.sh --strict test/unit/playwright-browser-launcher.test.ts \
  test/unit/playwright-browser-page-adapter.test.ts test/unit/reward-claimer.test.ts \
  test/unit/channel-points-reader.test.ts test/unit/stream-playback-optimizer.test.ts \
  test/e2e/chat-blocking.spec.ts test/e2e/channel-points-reader.spec.ts \
  test/e2e/playback-optimizer.spec.ts test/e2e/reward-claimer.spec.ts
```

### S2 核心邏輯（4–6 天）

| 步驟 | 內容 | 移植的測試 |
| --- | --- | --- |
| S2.1 | `logging/*`（§7.7）、`browser-diagnostics.ts` 的純函式 | `logging` 11、`browser-diagnostics` 12 |
| S2.2 | YAML 載入（§7.1）、`ConfigLoader`、resource guard 門檻 | `config` 24、`resource-guard-thresholds` 5；`*.load.yml`、`*.config.yml` golden |
| S2.3 | 設定寫回（§7.2） | `runtime-config-manager` 4；`*.input.yml` golden |
| S2.4 | `credentials/*` | `credential` 9 |
| S2.5 | 選台、復原政策、resource guard 政策、`channel-url`、`safe-logging` | `stream-selector` 11、`channel-recovery-policy` 7、`resource-guard-policy` 12 |
| S2.6 | `RuntimeDiagnostics`（fingerprint） | `runtime-diagnostics` 3；golden 的 `fingerprint` |

- `config` 測試的 fixture `valid.yml` 改成 `headless: true`；`明確指定 firefox 仍受支援` 改為斷言 X1 的錯誤，並在 `test-port.md` 註明。
- 關卡：`REQUIRE_E2E=1 scripts/check.sh` 全綠；`scripts/port-coverage.sh --strict` 加上本階段的 10 個測試檔。

### S3 常駐服務（7–9 天）

| 步驟 | 內容 | 移植的測試 |
| --- | --- | --- |
| S3.1 | `twitch/*`（Helix、直播狀態） | `twitch-api` 17 |
| S3.2 | cgroup、resource monitor、container restart、scheduler stall watchdog | `cgroup-v2-reader` 7、`runtime-resource-monitor` 7、`container-restart-controller` 7、`scheduler-stall-watchdog` 3 |
| S3.3 | `BrowserManager`（含診斷分級、重啟、invalidation） | `browser-manager` 49；e2e `browser-diagnostics` 第 2 個 |
| S3.4 | `ChannelSession`、`ChannelHealthChecker`、`SideNavCollapser` | `channel-session` 37、`channel-health-checker` 4、`side-nav-collapser` 4；e2e `channel-session` 6 |
| S3.5 | `SessionManager`、`ReconcileCoordinator`、`WatchdogScheduler` | `session-manager` 33、`reconcile-coordinator` 2、`watchdog-scheduler` 11 |
| S3.6 | `AppRunner`、`createApplication`、程序處理（§7.9） | `app-runner` 13；integration `browser-invalidation` 1、`startup-prerequisites` 2、`watchdog-flow` 1 |

- S3.2 的 cgroup 與 `/proc` 測試用 `tempfile` 建假檔案樹，不讀真的 `/sys/fs/cgroup`。
- S3.5 用 S1.5 實測到的錯誤訊息驗證 `SessionManager` 的錯誤分類（§7.4）。
- 關卡：`REQUIRE_E2E=1 scripts/check.sh` 全綠；`scripts/port-coverage.sh --strict` 加上本階段的測試檔；`cargo build --release --locked` 後 `twiclaimer --version` 可執行。

### S4 Bot（2–4 天）

| 步驟 | 內容 | 移植的測試 |
| --- | --- | --- |
| S4.1 | `notifications/*` | 由 Telegram／Discord 的測試涵蓋 |
| S4.2 | Telegram（§7.8） | `telegram-api` 4、`telegram-bot` 14 |
| S4.3 | Discord（§7.8） | `discord-api` 2、`discord-bot` 7 |

- Discord gateway 以 trait 包住 twilight `Shard`，單元測試用 fake 送 interaction；不連真的 Discord。
- 新增 X4 的測試：fake shard 斷線後恢復，Bot 繼續處理指令。
- 關卡：`REQUIRE_E2E=1 scripts/check.sh` 全綠；`scripts/port-coverage.sh --strict` 加上本階段的測試檔。

### S5 打包與切換（2–3 天 + 24–72 小時）

1. Dockerfile、`.dockerignore`、`docker-compose.yml`（§11、§12）。`docker build --target test .` 通過（內含 `REQUIRE_E2E=1 scripts/check.sh`）。
2. `scripts/docker-smoke.sh` 改成 Rust 版並通過 §11.3 的檢查；量正式 image 大小（§10 的量法）。
3. 文件：`README.md`、`README_DEV.md`、`CHANGELOG.md`（列出 X1–X9 與升級步驟）。
4. `scripts/port-coverage.sh --strict`（不帶參數，全部 397 個）通過。
5.【人工】A/B 比較 24 小時（§10）。agent 準備指令、分析結果、寫 `doc/ab-report.md`。
6.【人工】切換並觀察 24–72 小時（§12）；未達標準就依 §12 回復。

## 9. 測試移植

舊版 43 個測試檔、397 個測試（`doc/spec/ts-test-titles.tsv`）。除下表註明的以外全部移植；處置寫在 `doc/test-port.md`，由 `scripts/port-coverage.sh` 檢查。

| 舊測試檔 | 數量 | 階段 | 備註 |
| --- | --- | --- | --- |
| `e2e/browser-diagnostics.spec.ts` | 2 | S1.6、S3.3 | 第 1 個在 S1.6，第 2 個在 S3.3 |
| `e2e/channel-points-reader.spec.ts` | 4 | S1.7 | |
| `e2e/channel-session.spec.ts` | 6 | S3.4 | |
| `e2e/chat-blocking.spec.ts` | 2 | S1.7 | |
| `e2e/playback-optimizer.spec.ts` | 1 | S1.7 | |
| `e2e/reward-claimer.spec.ts` | 10 | S1.7 | |
| `integration/browser-invalidation.test.ts` | 1 | S3.6 | |
| `integration/startup-prerequisites.test.ts` | 2 | S3.6 | |
| `integration/watchdog-flow.test.ts` | 1 | S3.6 | |
| `unit/app-runner.test.ts` | 13 | S3.6 | |
| `unit/browser-diagnostics.test.ts` | 12 | S2.1 | |
| `unit/browser-manager.test.ts` | 49 | S3.3 | |
| `unit/cgroup-v2-reader.test.ts` | 7 | S3.2 | |
| `unit/channel-health-checker.test.ts` | 4 | S3.4 | |
| `unit/channel-points-reader.test.ts` | 5 | S1.7 | |
| `unit/channel-recovery-policy.test.ts` | 7 | S2.5 | |
| `unit/channel-session.test.ts` | 37 | S3.4 | |
| `unit/config.test.ts` | 24 | S2.2 | 3 個依 X1、X2 調整，見下方 |
| `unit/container-restart-controller.test.ts` | 7 | S3.2 | |
| `unit/credential.test.ts` | 9 | S2.4 | |
| `unit/default-browser.test.ts` | 6 | S1.1 | 保留 Node（`test-node/`） |
| `unit/discord-api.test.ts` | 2 | S4.3 | |
| `unit/discord-bot.test.ts` | 7 | S4.3 | |
| `unit/logging.test.ts` | 11 | S2.1 | |
| `unit/playwright-browser-launcher.test.ts` | 3 | S1.3 | firefox 那個不移植（D1） |
| `unit/playwright-browser-page-adapter.test.ts` | 9 | S1.6 | |
| `unit/reconcile-coordinator.test.ts` | 2 | S3.5 | |
| `unit/resource-guard-policy.test.ts` | 12 | S2.5 | |
| `unit/resource-guard-thresholds.test.ts` | 5 | S2.2 | |
| `unit/resource-snapshots-to-csv.test.ts` | 1 | S1.1 | 保留 Node（`test-node/`） |
| `unit/reward-claimer.test.ts` | 19 | S1.7 | |
| `unit/runtime-config-manager.test.ts` | 4 | S2.3 | |
| `unit/runtime-diagnostics.test.ts` | 3 | S2.6 | |
| `unit/runtime-resource-monitor.test.ts` | 7 | S3.2 | |
| `unit/scheduler-stall-watchdog.test.ts` | 3 | S3.2 | |
| `unit/session-manager.test.ts` | 33 | S3.5 | |
| `unit/side-nav-collapser.test.ts` | 4 | S3.4 | |
| `unit/stream-playback-optimizer.test.ts` | 6 | S1.7 | |
| `unit/stream-selector.test.ts` | 11 | S2.5 | |
| `unit/telegram-api.test.ts` | 4 | S4.2 | |
| `unit/telegram-bot.test.ts` | 14 | S4.2 | |
| `unit/twitch-api.test.ts` | 17 | S3.1 | |
| `unit/watchdog-scheduler.test.ts` | 11 | S3.5 | |

`config.test.ts` 依 X1、X2 調整（在 `test-port.md` 註明「已移植（依 X1／X2 調整）」）：

- fixture `valid.yml` 改成 `headless: true`；`載入合法 YAML 並將 snake_case 轉為 camelCase` 的預期值跟著改。
- fixture `env.yml` 改成 `headless: false`；`替換 ${ENV_VAR} 且讓直接環境覆寫優先` 改用 `HEADLESS: 'TRUE'`，預期 `headless` 為 true（仍驗證環境變數優先）。
- `明確指定 firefox 仍受支援` 改為斷言 X1 的錯誤。

移植規則：

- 一個 `it.each` 對應一個 Rust 測試，案例表照抄；`// 原測試：` 寫 `it.each` 的標題原文。
- 舊測試的 fake（`test/helpers/*`、各檔內的 fake page／locator／logger）移到 `src/testing/`，行為逐行照舊。
- 需要 Chromium 的測試標 `#[ignore = "需要 CHROME_PATH"]`；mock 頁面的路徑用 `env!("CARGO_MANIFEST_DIR")` 組合。
- 舊 e2e 用 `page.route` 製造失敗或攔截請求的地方，改用測試內建的本機 HTTP server 回應對應的狀態碼，或用 CDP 層的 Fetch 攔截；不得為了測試在正式 API 加功能。
- 時間相關的測試一律 `#[tokio::test(start_paused = true)]` 或注入的 fake clock。

## 10. A/B 比較與採用條件

- A：S0 的 24 小時資料。S0 已超過 2 週，或設定、頻道清單有改，就重跑舊版 24 小時。
- B：Rust 版用同一份設定、同一台主機跑 24 小時。兩版不同時執行（同一個帳號會互相干擾）。
- 日誌匯出方式同 S0.4，存成 `ab-logs-<n>.jsonl`；分析指令同 S0.6，結果寫進 `doc/ab-report.md`。

| 需求 | 量法 | 通過條件 |
| --- | --- | --- |
| R1 | `snapshot_stats.py` 依頁數分組 | Rust 控制端 RSS 中位數 ≤ 60 MiB；相同頁數下整容器記憶體中位數 ≤ A 的 105% |
| R2 | 同上 | 控制端平均 CPU 低於 A；整容器 ≤ A 的 105% |
| R3 | 兩版都用 S0.1 的 `docker save` 指令（gzip -1 壓縮後的位元組數） | ≤ A 的 1/3 |
| R6 | 【人工】Telegram 與 Discord 各執行一次每個指令，並收到啟動、停止通知 | 回覆與舊版相同 |
| R7 | `reward_claimed` 次數、`docker inspect` 的 RestartCount／OOMKilled、每 6 小時執行一次 S0.3 的指令 | 有成功領取；沒有 OOM；RestartCount 不增加；每次只有一個主要 headless shell 程序 |
| R8 | 【人工】用 Bot 修改 `channels` 與 `max_concurrent_streams` 各一次，`diff` 前後的 `config.yml` | 只有這兩個值的文字改變 |
| R9、R10 | `log-schema.py s0-logs.jsonl ab-logs.jsonl` | 只有「允許」，或每個「差異」「僅單邊」都在報告中附上舊版程式碼位置，說明只是執行情境不同 |

全部通過才進入 S5.6 的切換；有未通過的項目時停下來回報（§2.3）。

## 11. Docker

### 11.1 Dockerfile

```dockerfile
# syntax=docker/dockerfile:1.7
ARG UBUNTU_IMAGE=ubuntu:noble
ARG RUST_VERSION=1.99.1

# 執行期共用：Playwright 1.62.1 nativeDeps.ts 的 ubuntu24.04 chromium 套件，加上字型
FROM ${UBUNTU_IMAGE} AS runtime-base
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
      libasound2t64 libatk-bridge2.0-0t64 libatk1.0-0t64 libatspi2.0-0t64 libcairo2 libcups2t64 \
      libdbus-1-3 libdrm2 libgbm1 libglib2.0-0t64 libnspr4 libnss3 libpango-1.0-0 libx11-6 libxcb1 \
      libxcomposite1 libxdamage1 libxext6 libxfixes3 libxkbcommon0 libxrandr2 \
      libfontconfig1 libfreetype6 fonts-liberation fonts-noto-color-emoji fonts-ipafont-gothic \
      fonts-wqy-zenhei fonts-tlwg-loma-otf fonts-freefont-ttf fonts-unifont \
 && rm -rf /var/lib/apt/lists/*

FROM runtime-base AS chrome
ARG TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends curl unzip && rm -rf /var/lib/apt/lists/*
COPY scripts/fetch-headless-shell.sh /build/scripts/
COPY chrome/SHA256SUMS /build/chrome/
RUN case "${TARGETARCH}" in amd64) p=linux64 ;; arm64) p=linux-arm64 ;; *) exit 1 ;; esac \
 && exe="$(REQUIRE_SHA256=1 /build/scripts/fetch-headless-shell.sh --dest /opt/headless-shell "${p}")" \
 && ln -s "${exe}" /opt/headless-shell/headless-shell

FROM runtime-base AS build
ARG RUST_VERSION
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl build-essential pkg-config cmake \
 && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:${PATH}
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal \
      --default-toolchain "${RUST_VERSION}" -c rustfmt -c clippy
WORKDIR /src
COPY . .
RUN grep -q "channel = \"${RUST_VERSION}\"" rust-toolchain.toml
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/twiclaimer /usr/local/bin/twiclaimer

FROM build AS test
COPY --from=chrome /opt/headless-shell /opt/headless-shell
ENV CHROME_PATH=/opt/headless-shell/headless-shell
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/src/target \
    REQUIRE_E2E=1 scripts/check.sh

FROM runtime-base AS production
ARG APP_UID=1000
ARG APP_GID=1000
COPY --from=chrome /opt/headless-shell /opt/headless-shell
COPY --from=build /usr/local/bin/twiclaimer /usr/local/bin/twiclaimer
COPY config.example.yml /app/config.example.yml
RUN ! ldd /opt/headless-shell/headless-shell | grep -q 'not found' \
 && ! ldd /usr/local/bin/twiclaimer | grep -q 'not found' \
 && twiclaimer --version
ENV CONFIG_PATH=/app/config.yml \
    CHROME_PATH=/opt/headless-shell/headless-shell \
    HOME=/tmp XDG_CONFIG_HOME=/tmp/.config XDG_CACHE_HOME=/tmp/.cache
WORKDIR /app
USER ${APP_UID}:${APP_GID}
CMD ["twiclaimer"]
```

- `APP_UID`／`APP_GID` 用 S0.2 的值，讓 bind mount 的 `config.yml` 可以寫回（R8）。
- 主機是 arm64 時，S1.1 先執行 `scripts/fetch-headless-shell.sh linux-arm64` 記錄 SHA256 並提交，否則 `REQUIRE_SHA256=1` 會失敗。
- 正式 image 不含 Node、原始碼與測試。

### 11.2 .dockerignore

```text
target
.cache
node_modules
.git
config.yml
.env
data
s0-logs*.jsonl
ab-logs*.jsonl
doc/spec/traces/*.pw.log
```

### 11.3 Smoke（`scripts/docker-smoke.sh`）

沿用舊版的 9 項檢查，改成 Rust 版的內容，另加 2 項：

1. `node test-node/docker/verify-compose.mjs` 驗證正式與 smoke Compose 的合併結果。
2. `docker build --target test .` 通過。
3. 正式 image 只含 `/usr/local/bin/twiclaimer`、`/opt/headless-shell`、`/app/config.example.yml` 與系統套件。
4. image 不含 `config.yml`、`.env`、storageState、原始碼或測試。
5. 缺少 `/app/config.yml` 時以非零狀態結束，並輸出 `config_error`。
6. 以 smoke override 啟動，`config.yml` 對非 root 使用者可寫。
7. 重啟服務，SIGTERM 之後有 `service_stopped`。
8. `mem_limit`、`memswap_limit`、`pids_limit` 生效；有 cgroup v2 時讀容器內的 `memory.max`、`pids.max`。
9. logging driver 為 `local`，每檔 `10m`、最多 `5` 個。
10. 停止與重啟之後，容器內沒有殘留的 headless shell 程序。
11. 輸出正式 image 的大小（§10 的量法）。

## 12. 部署與回復

`docker-compose.yml` 與舊版相同，只改 image 與建置參數：

- service 與 `container_name` 維持 `twitch-watchdog`，維運指令不用改。
- `image: twiclaimer:local`；`build.target: production`；`build.args` 為 `APP_UID`、`APP_GID`。
- `init`、`read_only`、`no-new-privileges`、`seccomp`、`tmpfs /tmp 512m`、`shm_size 1gb`、`stop_grace_period 40s`、`mem_limit 6g`、`memswap_limit 7g`、`pids_limit 512`、logging `local` 10m×5、`config.yml`（可寫）與 `data/browser-state`（唯讀）的 bind mount 都不變。

切換【人工】：

```bash
cd twiclaimer
docker compose build --build-arg APP_UID=<S0.2> --build-arg APP_GID=<S0.2>
cp -a ../twitch-watchdog-docker/config.yml ./config.yml
mkdir -p data && cp -a ../twitch-watchdog-docker/data/browser-state ./data/
(cd ../twitch-watchdog-docker && docker compose down)
docker compose up -d
docker compose logs -f --no-log-prefix twitch-watchdog    # 確認 service_started
```

回復【人工】：

```bash
cd twiclaimer && docker compose down
diff config.yml ../twitch-watchdog-docker/config.yml      # Bot 改過設定時，先把這份複製回舊版目錄
cd ../twitch-watchdog-docker
docker tag twitch-watchdog:rollback-e0e079a twitch-watchdog:local
docker compose up -d --no-build
```

觀察期（24–72 小時）出現以下任一情況就回復，並把日誌交給 agent 分析：

- OOMKilled，或 RestartCount 在 24 小時內增加超過 2 次。
- 有頻道在直播、舊版在相同情況下有領取，Rust 版卻連續 6 小時沒有 `reward_claimed`。
- `error` 層級事件的頻率超過 A 的 2 倍。
- Bot 沒有回應，或出現殘留的 headless shell 程序。

## 13. 風險

| 風險 | 影響 | 對策 |
| --- | --- | --- |
| CDP 行為與 Playwright 有細微差異 | 領取或健康檢查失敗 | G2、G3 的錄製比對；頁面內邏輯直接用 Playwright 的 injected script；e2e 用舊版 mock 頁面 |
| click 後的導覽等待、auto-attach 等 Playwright 內部流程複雜 | S1 超時 | 照原始碼逐段移植；超過估計 1.5 倍時依 §2.3 停止 |
| YAML 語意與 yaml 2.9.0 不同 | 設定被誤讀 | golden 由舊版實作產生；新發現的情況補案例 |
| 移植期間 Twitch 改版 | 兩版一起失效 | 選擇器集中在 `selectors.rs`；修正先在舊版確認，再同步到 Rust 版並記入 `decisions.md` |
| twilight 的重連行為與舊版不同 | Discord 行為改變 | X4 列為允許的差異；fake shard 測試 |
| linux-arm64 只有 Playwright 自建的 headless shell | arm64 主機無法建置 | S0.2 確認架構；必要時在 S1.1 記錄 SHA256 |
| 固定 Chromium 151 | 之後的安全更新 | 升級時用新版 Playwright 重新錄製 G1–G5，作為獨立工作 |
| aws-lc-rs 在 Docker 內建置失敗 | image 建不出來 | build stage 已裝 cmake 與 build-essential；仍失敗時停下來回報 |

## 附錄

- 每個檔案的小標題就是它在 repo 內的路徑。S1.1 步驟 2 把附錄 A–D 逐字放到該路徑，再 `chmod +x` 所有 `.sh` 與 `.py`。
- 附錄 A–C 的工具已在沙箱用舊版程式碼與 Playwright 1.63 實測過。`$REF` 用的是 1.62.1，所以錄製與錯誤訊息以 S1.2 實際產生的為準；和本文件不同時（例如 §7.4 錯誤表的訊息），照實際輸出做並記入 `decisions.md`。
- 附錄 D 是範本，放好後由 agent 填寫。

### 附錄 A：tools/ref（呼叫舊版實作產生規格與 golden）

| 檔案 | 用途 | 使用時機 |
| --- | --- | --- |
| `extract-spec.sh` | 抽出舊測試標題（`ts-test-titles.tsv`）與日誌事件名稱（`log-events.txt`） | S1.1 |
| `make-yaml-cases.sh` | 產生 YAML 解析、設定載入、寫回的輸入案例 | S1.2；補案例時 |
| `gen-config-golden.mjs` | 用 `$REF` 的舊版實作產生設定 golden | S1.2；補案例時 |
| `pw-errors.mjs` | 取樣 Playwright 的錯誤名稱與訊息（G5） | S1.2 |
| `dump-injected.mjs` | 取出 Playwright 的 injected script（§7.4） | S1.2 |
| `record-traces.sh` | 錄製 9 組 CDP 指令順序與診斷事件、Chromium 參數（G1–G3） | S1.2 |
| `pw-trace.mjs` | 單次錄製 | 由 `record-traces.sh` 呼叫 |
| `multi-server.mjs` | multi 測試頁的本機伺服器 | 由 `record-traces.sh`、`compare-traces.sh` 呼叫 |
| `cdp-seq.py` | 把 CDP 紀錄轉成各 session 的指令清單 | 由 `record-traces.sh`、`compare-traces.sh` 呼叫 |
| `diag-diff.py` | 比對兩份診斷事件 | 由 `compare-traces.sh` 呼叫 |

#### tools/ref/extract-spec.sh

```bash
#!/usr/bin/env bash
# 從舊版 repo 抽出規格清單：測試標題（ts-test-titles.tsv）與日誌事件名稱（log-events.txt）。
# 用法：tools/ref/extract-spec.sh <舊版 repo> [輸出目錄，預設 doc/spec]
set -euo pipefail
old="$(cd "${1:?用法：extract-spec.sh <舊版 repo> [輸出目錄]}" && pwd)"
out="${2:-doc/spec}"
mkdir -p "${out}"

# 每列：<檔案>:<行號><TAB><標題>（含 it.each／test.each）
grep -rnoE "^\s*(it|test)(\.each\([^)]*\))?\(\s*['\"\`][^'\"\`]+" "${old}/test" --include=*.ts \
  | sed -E "s#^${old}/##; s/:[[:space:]]*(it|test)(\.each\([^)]*\))?\([[:space:]]*['\"\`]/\t/" \
  | LC_ALL=C sort -t: -k1,1 -k2,2n > "${out}/ts-test-titles.tsv"

# logger.debug／info／warn／error('事件')、('層級', '事件')、event: '事件'，以及 Logger.ts 的 LOG_EVENTS
{
  grep -rhoE "\b(debug|info|warn|error)\(\s*'[a-z][a-z0-9_]+'" "${old}/src" || true
  grep -rhoE "'(debug|info|warn|error)',\s*'[a-z][a-z0-9_]+'" "${old}/src" || true
  grep -rhoE "\bevent: '[a-z][a-z0-9_]+'" "${old}/src" || true
  awk '/export const LOG_EVENTS/,/^}/' "${old}/src/logging/Logger.ts"
} | grep -oE "'[a-z][a-z0-9_]+'" | tr -d "'" | grep -vxE 'debug|info|warn|error' | LC_ALL=C sort -u > "${out}/log-events.txt"

echo "測試標題：$(wc -l < "${out}/ts-test-titles.tsv")"
echo "日誌事件：$(wc -l < "${out}/log-events.txt")"
cut -f1 "${out}/ts-test-titles.tsv" | cut -d: -f1 | sort | uniq -c | sort -k2
```

#### tools/ref/make-yaml-cases.sh

```bash
#!/usr/bin/env bash
# 產生設定 golden 的輸入案例；之後用 gen-config-golden.mjs 跑舊版實作產生預期結果。
# 用法：tools/ref/make-yaml-cases.sh <輸出目錄> [舊版 repo 路徑]
#   有給舊版路徑時，另外複製 test/fixtures/config 與 config.example.yml 作為「設定」案例。
set -euo pipefail
dir="${1:?用法：make-yaml-cases.sh <輸出目錄> [舊版 repo 路徑]}"
old="${2:-}"
[[ -z "${old}" ]] || old="$(cd "${old}" && pwd)"
mkdir -p "${dir}"
cd "${dir}"
w() { cat > "$1"; }                      # 以 heredoc 寫入檔案
ops() { printf '%s\n' "$2" > "$1.ops.json"; }  # 寫回案例的初始狀態與操作序列

# ---- 寫回案例：<名稱>.input.yml ＋ <名稱>.ops.json ----
w 01-basic.input.yml <<'Y'
# keep this comment
channels:
  - first
  - second
max_concurrent_streams: 2
log_level: debug
twitch_api:
  client_id: ${TWITCH_CLIENT_ID}
Y
ops 01-basic '{"initial":{"channels":["first","second"],"maxConcurrentStreams":2},"ops":[{"setChannels":["second","third"]}]}'
w 02-flow.input.yml <<'Y'
channels: [first, second] # inline
max_concurrent_streams: 2
Y
ops 02-flow '{"initial":{"channels":["first","second"],"maxConcurrentStreams":2},"ops":[{"setChannels":["first","third"]}]}'
w 03-inner-comments.input.yml <<'Y'
channels: # 依優先序
  # main
  - first # primary
  - second
max_concurrent_streams: 2 # 上限
log_level: info
Y
ops 03-inner-comments '{"initial":{"channels":["first","second"],"maxConcurrentStreams":2},"ops":[{"setChannels":["first"]}]}'
w 04-missing-max.input.yml <<'Y'
channels:
  - a
log_level: info
Y
ops 04-missing-max '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a","b"]}]}'
w 05-quoting.input.yml <<'Y'
channels:
  - a
max_concurrent_streams: 1
Y
ops 05-quoting '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["123","true","null","1e3","0x1F","1_000","Yes","on","NULL","007","0o17","Inf","_x"]},{"setMaxConcurrentStreams":2}]}'
printf '# c\r\nchannels:\r\n  - a\r\nmax_concurrent_streams: 1\r\nlog_level: info\r\n' > 06-crlf.input.yml
ops 06-crlf '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["b"]}]}'
w 07-doc-markers.input.yml <<'Y'
---
# header
channels: [a]
max_concurrent_streams: 1
# trailing
Y
ops 07-doc-markers '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a","b"]},{"setMaxConcurrentStreams":2}]}'
w 08-anchor.input.yml <<'Y'
channels: &ch
  - a
mirror: *ch
max_concurrent_streams: 1
Y
ops 08-anchor '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["b"]}]}'
w 09-indent4-long.input.yml <<'Y'
twitch_api:
    client_id: "${TWITCH_CLIENT_ID}"
    client_secret: '${TWITCH_CLIENT_SECRET}'
channels:
    - a
max_concurrent_streams: 1
note: this is a very long plain scalar value that goes beyond eighty characters for sure yes indeed
ids: [123456789012345678, "987"]
Y
ops 09-indent4-long '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a","b"]},{"setMaxConcurrentStreams":2}]}'
printf 'channels: [a\n' > 10-syntax-error.input.yml
ops 10-syntax-error '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a"]}]}'
printf -- '- a\n' > 11-root-seq.input.yml
ops 11-root-seq '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a"]}]}'
printf 'channels: [a]\nchannels: [b]\n' > 12-duplicate-key.input.yml
ops 12-duplicate-key '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a"]}]}'
printf 'channels:\n  - a\n\n\nmax_concurrent_streams: 1\n\n# end\n' > 13-blank-lines.input.yml
ops 13-blank-lines '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a","b"]},{"setMaxConcurrentStreams":2}]}'
w 14-anchor-elsewhere.input.yml <<'Y'
defaults: &d
  x: 1
other: *d
channels:
  - a
max_concurrent_streams: 1
Y
ops 14-anchor-elsewhere '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a","b"]},{"setMaxConcurrentStreams":2}]}'
w 15-max-anchor.input.yml <<'Y'
channels:
  - a
max_concurrent_streams: &m 1
copy: *m
Y
ops 15-max-anchor '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setMaxConcurrentStreams":1}]}'
w 16-nested-keys.input.yml <<'Y'
telegram:
  channels: keep-me
  max_concurrent_streams: 9
channels:
  - a
max_concurrent_streams: 1
Y
ops 16-nested-keys '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["b","c"]},{"setMaxConcurrentStreams":2}]}'
w 17-last-line-no-newline.input.yml <<'Y'
channels:
  - a
Y
printf 'max_concurrent_streams: 1' >> 17-last-line-no-newline.input.yml
ops 17-last-line-no-newline '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a","b"]},{"setMaxConcurrentStreams":2}]}'

w 18-validation.input.yml <<'Y'
channels:
  - a
  - b
max_concurrent_streams: 2
Y
ops 18-validation '{"initial":{"channels":["a","b"],"maxConcurrentStreams":2},"ops":[{"setChannels":[]},{"setChannels":["bad-name"]},{"setChannels":["abcdefghijklmnopqrstuvwxyz"]},{"setChannels":[" A ","a","B_1"]},{"setMaxConcurrentStreams":0},{"setMaxConcurrentStreams":3},{"setMaxConcurrentStreams":1.5},{"setMaxConcurrentStreams":1}]}'
w 19-external-edit.input.yml <<'Y'
channels:
  - a
max_concurrent_streams: 1
Y
ops 19-external-edit '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a","b"]},{"replaceFile":"channels:\n  - x\nmax_concurrent_streams: 1\nlog_level: warn\n"},{"setMaxConcurrentStreams":2},{"replaceFile":"channels: [\n"},{"setMaxConcurrentStreams":1}]}'

: > 20-empty.input.yml
ops 20-empty '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a","b"]}]}'
printf '# only a comment\n' > 21-comment-only.input.yml
ops 21-comment-only '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setMaxConcurrentStreams":1}]}'
printf 'channels: [a, b]  # 行尾註解\nmax_concurrent_streams: 2 # 上限\n' > 22-flow-trailing-comment.input.yml
ops 22-flow-trailing-comment '{"initial":{"channels":["a","b"],"maxConcurrentStreams":2},"ops":[{"setChannels":["c"]}]}'

# ---- 載入案例：<名稱>.load.yml ----
printf 'a: 1\na: 2\n' > l01-dup.load.yml
printf 'a: 1\n---\nb: 2\n' > l02-multidoc.load.yml
{ echo 'x: &a [1]'; for i in $(seq 0 100); do echo "k${i}: *a"; done; } > l03-alias-bomb.load.yml
{ echo 'x: &a [1]'; for i in $(seq 0 98); do echo "k${i}: *a"; done; } > l03b-alias-99.load.yml
{ echo 'x: &a [1]'; for i in $(seq 0 99); do echo "k${i}: *a"; done; } > l03c-alias-100.load.yml
printf 'a: &x\n  b: *x\n' > l04-cycle.load.yml
printf 'base: &b {x: 1}\nm:\n  <<: *b\n  y: 2\n' > l05-merge.load.yml
w l06-scalars.load.yml <<'Y'
a: yes
b: on
c: 1_000
d: 0x1F
e: .inf
f: ~
g: 123456789012345678
h: "${HOME}"
i: 0o17
j: 1e3
k: TRUE
l: -.INF
m: .nan
n: 0.5
o: +12
p: 010
q: ''
r:
Y
: > l07-empty.load.yml
printf 'a: [1, 2\n' > l08-syntax.load.yml
printf 'a: !!str 123\nb: !custom value\nc: !!int "7"\n' > l09-tags.load.yml
printf 'a: |\n  line1\n  line2\nb: >-\n  folded\n  text\nc: "esc\\tq\\u00e9"\n' > l10-block-scalars.load.yml
printf '\xef\xbb\xbfa: 1\n' > l11-bom.load.yml

# ---- 設定案例：<名稱>.config.yml［＋ <名稱>.env.json］（需要舊版 repo 路徑）----
if [[ -n "${old}" ]]; then
  # Rust 版只接受 headless: true（§6.2），共用案例先把舊 fixture 改成 true；舊值另列為已知差異 c08。
  sed 's/^headless: false$/headless: true/' "${old}/test/fixtures/config/valid.yml" > c01-valid.config.yml
  cp "${old}/test/fixtures/config/defaults.yml" c02-defaults.config.yml
  # 檔案寫 false、環境變數寫 TRUE：驗證覆寫後才檢查 headless。
  sed 's/^headless: true$/headless: false/' "${old}/test/fixtures/config/env.yml" > c03-env.config.yml
  echo '{"PRIMARY_CHANNEL":"environment_channel","FILE_CLIENT_ID":"file-client-id","TWITCH_CLIENT_ID":"override-client-id","TWITCH_ACCESS_TOKEN":"override-access-token","LOG_LEVEL":"warn","HEADLESS":"TRUE"}' > c03-env.env.json
  cp "${old}/test/fixtures/config/parse-error.yml" c04-parse-error.config.yml
  cp "${old}/config.example.yml" c05-example.config.yml
  cp c01-valid.config.yml c06-bots.config.yml
  echo '{"TELEGRAM_ENABLED":"true","TELEGRAM_BOT_TOKEN":"123456:test-token","TELEGRAM_ALLOWED_CHAT_IDS":"42, -100123,42","DISCORD_ENABLED":"true","DISCORD_BOT_TOKEN":"discord-token","DISCORD_APPLICATION_ID":"123456789012345678","DISCORD_GUILD_ID":"987654321098765432","DISCORD_ALLOWED_CHANNEL_IDS":"111111111111111111, 222222222222222222,111111111111111111","DISCORD_ALLOW_DIRECT_MESSAGES":"true","DISCORD_ALLOWED_USER_IDS":"333333333333333333,333333333333333333"}' > c06-bots.env.json
  cp c01-valid.config.yml c07-bad-headless.config.yml
  echo '{"HEADLESS":"yes"}' > c07-bad-headless.env.json
  # 已知差異（§6.2）：舊版接受，Rust 版必須回報下列錯誤。
  cp "${old}/test/fixtures/config/valid.yml" c08-headless-false.config.yml
  sed 's/^  engine: chromium$/  engine: firefox/' c01-valid.config.yml > c09-firefox.config.yml
  w known-differences.json <<'J'
{
  "c08-headless-false": {
    "reason": "§6.2：不支援 headless: false",
    "expected": { "error": { "name": "ConfigValidationError", "message": "設定欄位 headless 必須是 true" } }
  },
  "c09-firefox": {
    "reason": "§6.2：只支援 chromium",
    "expected": { "error": { "name": "ConfigValidationError", "message": "設定欄位 browser.engine 必須是 chromium" } }
  }
}
J
fi
ls | wc -l
```

#### tools/ref/gen-config-golden.mjs

```javascript
// 直接呼叫舊版實作（$REF/dist）產生設定相關的 golden。$REF 須先 npm ci && npm run build。
// 用法：node tools/ref/gen-config-golden.mjs <REF> <案例目錄>
// 三種案例（依副檔名判斷）：
// - 寫回：<名稱>.input.yml ＋ <名稱>.ops.json
//     ops.json：{"initial":{"channels":[...],"maxConcurrentStreams":N},"ops":[{"setChannels":[...]}|{"setMaxConcurrentStreams":N}|{"replaceFile":"..."}]}
//     → <名稱>.expected.yml（最後的檔案內容）＋ <名稱>.expected.json（每個 op 的結果、updateConfig 呼叫、寫檔次數）
// - 解析：<名稱>.load.yml → <名稱>.load.expected.json（dist/config/yaml.js 的 parseYaml）
// - 設定：<名稱>.config.yml［＋ <名稱>.env.json］→ <名稱>.config.expected.json
//     （YamlConfigLoader.load 的 AppConfig、debug 日誌、createSafeConfigFingerprint 的結果）
// known-differences.json（由 make-yaml-cases.sh 產生）列出的案例仍照舊產生 golden，Rust 測試改比對該檔的 expected。
// Infinity／NaN 以 {"$number":"Infinity"} 表示；錯誤以 {"name","message"} 表示。
import { existsSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const [ref, dir] = process.argv.slice(2);
if (!ref || !dir) {
  console.error('用法：node tools/ref/gen-config-golden.mjs <REF> <案例目錄>');
  process.exit(2);
}
const load = (path) => import(pathToFileURL(resolve(ref, 'dist', path)).href);
const { YamlRuntimeConfigManager } = await load('config/RuntimeConfigManager.js');
const { parseYaml } = await load('config/yaml.js');
const { YamlConfigLoader } = await load('config/ConfigLoader.js');
const { createSafeConfigFingerprint } = await load('app/RuntimeDiagnostics.js');

const encode = (_key, value) =>
  typeof value === 'number' && !Number.isFinite(value) ? { $number: String(value) } : value;
const json = (value) => `${JSON.stringify(value, encode, 2)}\n`;
const failure = (error) => ({
  error: {
    name: error instanceof Error ? error.name : 'UnknownError',
    message: error instanceof Error ? error.message : String(error),
  },
});
const files = readdirSync(dir).sort();
const read = (name) => readFileSync(join(dir, name), 'utf8');

for (const name of files.filter((file) => file.endsWith('.input.yml'))) {
  const base = name.slice(0, -'.input.yml'.length);
  const spec = JSON.parse(read(`${base}.ops.json`));
  let content = read(name);
  let writes = 0;
  const updates = [];
  const manager = new YamlRuntimeConfigManager({
    configPath: 'config.yml',
    initialConfig: spec.initial,
    target: { updateConfig: async (config) => void updates.push(structuredClone(config)) },
    readFile: async () => content,
    writeFile: async (_path, data) => {
      writes += 1;
      content = data;
    },
  });
  const results = [];
  for (const op of spec.ops) {
    if ('replaceFile' in op) {
      content = op.replaceFile;
      results.push({ replaced: true });
      continue;
    }
    try {
      const config = 'setChannels' in op
        ? await manager.setChannels(op.setChannels)
        : await manager.setMaxConcurrentStreams(op.setMaxConcurrentStreams);
      results.push({ value: structuredClone(config) });
    } catch (error) {
      results.push(failure(error));
    }
  }
  writeFileSync(join(dir, `${base}.expected.yml`), content);
  writeFileSync(join(dir, `${base}.expected.json`), json({ results, updates, writes }));
  console.log(`${base}: ${results.map((r) => (r.error ? r.error.name : 'ok')).join(', ')}`);
}

for (const name of files.filter((file) => file.endsWith('.load.yml'))) {
  const base = name.slice(0, -'.load.yml'.length);
  let result;
  try {
    result = { value: parseYaml(read(name)) ?? null };
  } catch (error) {
    result = failure(error);
  }
  writeFileSync(join(dir, `${base}.load.expected.json`), json(result));
  console.log(`${base}: ${result.error ? result.error.message : 'ok'}`);
}

process.chdir(resolve(dir)); // 讓 AppConfig 內的相對路徑與案例目錄無關
for (const name of files.filter((file) => file.endsWith('.config.yml'))) {
  const base = name.slice(0, -'.config.yml'.length);
  const envFile = `${base}.env.json`;
  const env = existsSync(envFile) ? JSON.parse(readFileSync(envFile, 'utf8')) : {};
  const logs = [];
  const loader = new YamlConfigLoader({ debug: (event, fields) => void logs.push({ event, fields }) });
  let result;
  try {
    const value = await loader.load(name, env);
    result = { value, fingerprint: createSafeConfigFingerprint(value), logs };
  } catch (error) {
    result = { ...failure(error), logs };
  }
  writeFileSync(join(dir, `${base}.config.expected.json`), json(result));
  console.log(`${base}: ${result.error ? result.error.message : 'ok'}`);
}
```

#### tools/ref/pw-errors.mjs

```javascript
// 用 $REF 的 Playwright 取樣錯誤的 name 與第一行訊息，作為 Rust CDP 層錯誤訊息的規格（§7.4）。
// 用法：node tools/ref/pw-errors.mjs <REF> > doc/spec/pw-errors.jsonl
// 每列：{"case":…,"name":…,"message":…}（只取第一行，不含 Call log）；沒有拋錯時為 {"case":…,"ok":true}。
import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import { resolve } from 'node:path';

const [ref] = process.argv.slice(2);
if (!ref) {
  console.error('用法：node tools/ref/pw-errors.mjs <REF>');
  process.exit(2);
}
const { chromium } = createRequire(resolve(ref, 'package.json'))('playwright');

const HTML = `<!doctype html><button>a</button><button>b</button>
<div id="hidden" style="display:none">x</div><button id="disabled" disabled>d</button>
<button id="covered">c</button><div style="position:fixed;inset:0;background:#fff"></div>`;
let onceServed = false;
const server = createServer((request, response) => {
  if (request.url === '/hang' || (request.url === '/once' && onceServed)) {
    return; // 永不回應，用來觸發導覽逾時
  }
  onceServed ||= request.url === '/once';
  response.writeHead(200, { 'content-type': 'text/html' });
  response.end(HTML);
});
const listen = (srv) => new Promise((done) => srv.listen(0, '127.0.0.1', () => done(srv.address().port)));
const base = `http://127.0.0.1:${await listen(server)}`;
const closed = createServer();
const refusedPort = await listen(closed);
await new Promise((done) => closed.close(done));

const T = { timeout: 300 };
const sample = async (name, action) => {
  try {
    await action();
    console.log(JSON.stringify({ case: name, ok: true }));
  } catch (error) {
    console.log(JSON.stringify({ case: name, name: error.name, message: String(error.message).split('\n')[0] }));
  }
};

const browser = await chromium.launch({ headless: true, executablePath: process.env.PW_EXECUTABLE_PATH || undefined });
const context = await browser.newContext();
// 導覽類案例各用一個新頁面，避免前一個逾時的導覽干擾下一個。
const fresh = async (name, action) => {
  const scratch = await context.newPage();
  await sample(name, () => action(scratch));
  await scratch.close();
};
await fresh('goto-timeout', (p) => p.goto(`${base}/hang`, T));
await fresh('goto-default-navigation-timeout', (p) => {
  p.setDefaultNavigationTimeout(400);
  return p.goto(`${base}/hang`);
});
await fresh('goto-refused', (p) => p.goto(`http://127.0.0.1:${refusedPort}/`));
await fresh('goto-abort-signal', (p) => {
  const controller = new AbortController();
  setTimeout(() => controller.abort(), 100);
  return p.goto(`${base}/hang`, { waitUntil: 'domcontentloaded', signal: controller.signal });
});
await fresh('reload-timeout', async (p) => {
  await p.goto(`${base}/once`);
  await p.reload(T);
});
const page = await context.newPage();
await page.goto(`${base}/`);
await sample('click-strict', () => page.locator('button').click(T));
await sample('click-hidden', () => page.locator('#hidden').click(T));
await sample('click-disabled', () => page.locator('#disabled').click(T));
await sample('click-covered', () => page.locator('#covered').click(T));
await sample('hover-missing', () => page.locator('#missing').hover(T));
await sample('waitfor-missing', () => page.locator('#missing').waitFor(T));
await sample('waitfor-hidden-visible', () => page.locator('#hidden').waitFor({ state: 'visible', ...T }));
await sample('textcontent-missing', () => page.locator('#missing').textContent(T));
await sample('getattribute-strict', () => page.locator('button').getAttribute('id', T));
await sample('isvisible-strict', () => page.locator('button').isVisible());
await sample('isvisible-missing', () => page.locator('#missing').isVisible());
await sample('count-missing', () => page.locator('#missing').count());
await sample('locator-evaluate-missing', () => page.locator('#missing').evaluate((element) => element.id, undefined, T));
await sample('dispatch-missing', () => page.locator('#missing').dispatchEvent('click', undefined, T));
await sample('evaluate-throw', () => page.evaluate(() => { throw new Error('boom'); }));
await sample('evaluate-reject', () => page.evaluate(() => Promise.reject(new TypeError('nope'))));
await sample('getattribute-missing', () => page.locator('#missing').getAttribute('id', T));
await sample('evaluateall-missing', () => page.locator('#missing').evaluateAll((elements) => elements.length));
await sample('evaluate-navigation', () => page.evaluate(() => {
  location.href = '/';
  return new Promise(() => {});
}));
const crashing = await context.newPage();
await sample('goto-crash', () => crashing.goto('chrome://crash'));
await sample('after-crash-evaluate', () => crashing.evaluate(() => 1));
await sample('after-crash-close', () => crashing.close());
await page.close();
await sample('closed-goto', () => page.goto(`${base}/`));
await sample('closed-click', () => page.locator('button').first().click(T));
await sample('closed-evaluate', () => page.evaluate(() => 1));
await sample('closed-screenshot', () => page.screenshot());
await sample('closed-isvisible', () => page.locator('button').first().isVisible());
await browser.close();
await sample('browser-closed-newpage', () => context.newPage());
server.close();
process.exit(0);
```

#### tools/ref/dump-injected.mjs

```javascript
// 從 $REF 安裝的 playwright-core 取出 injected script 原始碼（Playwright 在頁面內用來解析選擇器、
// 檢查元素狀態與 hit target 的程式），逐字寫到 stdout。Rust 版以 include_str! 嵌入（§7.4）。
// 用法：node tools/ref/dump-injected.mjs <REF> > src/browser/cdp/js/injected-script.js
// stderr 印出 playwright-core 版本、位元組數與 SHA-256，寫進 doc/decisions.md。
import { createHash } from 'node:crypto';
import { existsSync, readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join, resolve } from 'node:path';
import vm from 'node:vm';

const [ref] = process.argv.slice(2);
if (!ref) {
  console.error('用法：node tools/ref/dump-injected.mjs <REF>');
  process.exit(2);
}
const require = createRequire(join(resolve(ref), 'package.json'));
const coreDir = dirname(require.resolve('playwright-core/package.json'));
const { version } = JSON.parse(readFileSync(join(coreDir, 'package.json'), 'utf8'));

function fromBundle(file) {
  // 新版把 lib 打包成單檔：source = '<JS 字串>'，位於 injectedScriptSource.ts 的 __esm 區塊。
  const bundle = readFileSync(file, 'utf8');
  const marker = bundle.indexOf('src/generated/injectedScriptSource.ts"() {');
  if (marker < 0) throw new Error(`${file} 找不到 injectedScriptSource`);
  const start = bundle.indexOf(" = '", marker) + 3;
  let end = start + 1;
  while (bundle[end] !== "'") end += bundle[end] === '\\' ? 2 : 1;
  return vm.runInNewContext(bundle.slice(start, end + 1));
}

const legacy = join(coreDir, 'lib/generated/injectedScriptSource.js');
const source = existsSync(legacy) ? require(legacy).source : fromBundle(join(coreDir, 'lib/coreBundle.js'));
if (typeof source !== 'string' || !source.includes('InjectedScript')) {
  throw new Error('取出的內容不是 injected script');
}
process.stdout.write(source);
const sha256 = createHash('sha256').update(source).digest('hex');
console.error(`playwright-core ${version}：${Buffer.byteLength(source)} bytes，sha256 ${sha256}`);
```

#### tools/ref/record-traces.sh

```bash
#!/usr/bin/env bash
# S1.2：用 $REF（舊版 + Playwright 1.62.1）錄製 CDP 指令順序與診斷事件。
# 產生 doc/spec/traces/<名稱>.{diag.jsonl,pw.log,seq} 與 doc/spec/chromium-args.txt（每行一個參數）。
# 名稱：<頁面>-b<0|1>[-chat0]；b1 = 資源阻擋全開，chat0 = 不注入聊天室封鎖。scripts/compare-traces.sh 依名稱重跑。
# 用法（在 twiclaimer 根目錄）：tools/ref/record-traces.sh <REF>
set -euo pipefail
ref="$(cd "${1:?用法：record-traces.sh <REF>}" && pwd)"
cd "$(dirname "$0")/../.."
state=testdata/fixtures/storage-state.fake.json
out=doc/spec/traces
mkdir -p "$out"

record() { # <名稱> <頁面> <TRACE_BLOCKING> <TRACE_DISABLE_CHAT>
  TRACE_BLOCKING=$3 TRACE_DISABLE_CHAT=$4 DEBUG=pw:browser,pw:protocol \
    node tools/ref/pw-trace.mjs "$ref" "$state" "$2" > "$out/$1.diag.jsonl" 2> "$out/$1.pw.log"
  python3 tools/ref/cdp-seq.py "$out/$1.pw.log" > "$out/$1.seq"
  echo "$1：CDP 指令 $(wc -l < "$out/$1.seq")、診斷事件 $(wc -l < "$out/$1.diag.jsonl")"
}

for page in live content-warning reward-available multi; do
  target="$page.html"
  [[ $page != multi ]] || target=multi
  for blocking in 0 1; do
    record "$page-b$blocking" "$target" "$blocking" 1
  done
done
record live-b0-chat0 live.html 0 0

# <launching> 那一行：去掉執行檔，--user-data-dir 換成 <tmp>
grep -m1 '<launching>' "$out/live-b0.pw.log" | sed -E 's/.*<launching> //' | tr ' ' '\n' | tail -n +2 \
  | sed -E 's#^--user-data-dir=.*#--user-data-dir=<tmp>#' > doc/spec/chromium-args.txt
echo "Chromium 參數：$(wc -l < doc/spec/chromium-args.txt) 個"
```

#### tools/ref/pw-trace.mjs

```javascript
// 用舊版程式碼（$REF/dist 的 Playwright adapters + Playwright 1.62.1）重現常駐服務的瀏覽器啟動，
// 產生兩種對照資料：
//   stderr：Chromium 啟動參數與 CDP 指令順序（需設定 DEBUG=pw:browser,pw:protocol）
//   stdout：舊版 page adapter 發出的診斷事件（JSON Lines：requestfailed／response／console／popup）
// 前置：$REF 已 npm ci && npm run build && npx playwright install --only-shell chromium
// 用法：
//   DEBUG=pw:browser,pw:protocol node tools/ref/pw-trace.mjs <REF> <storage-state.json> [頁面] \
//     > doc/spec/traces/<名稱>.diag.jsonl 2> doc/spec/traces/<名稱>.pw.log
//   頁面：test/mock-pages 內的檔名（預設 live.html）、http(s) URL，或 multi（啟動 multi-server.mjs：
//         跨站 iframe、worker、console、404／500、GraphQL POST、圖片與字型、popup）
//   TRACE_BLOCKING=1      → block_images／block_fonts／block_known_tracking 全開（對照 Fetch 攔截路徑）
//   TRACE_DISABLE_CHAT=0  → disable_chat: false（不注入聊天室封鎖 init script）
// storage-state 只用假資料（testdata/fixtures/storage-state.fake.json），不要指向真實登入狀態。
import { existsSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

import { startMultiServer } from './multi-server.mjs';

const [ref, storageState, pageName = 'live.html'] = process.argv.slice(2);
if (!ref || !storageState) {
  console.error('用法：node tools/ref/pw-trace.mjs <REF> <storage-state.json> [頁面]');
  process.exit(2);
}
const launcherJs = resolve(ref, 'dist/browser/adapters/PlaywrightBrowserLauncher.js');
if (!existsSync(launcherJs)) {
  console.error(`找不到 ${launcherJs}；請先在舊版 repo 執行 npm run build`);
  process.exit(2);
}
const { PlaywrightBrowserLauncher } = await import(pathToFileURL(launcherJs).href);

const blocking = process.env.TRACE_BLOCKING === '1';
const disableChat = process.env.TRACE_DISABLE_CHAT !== '0';
const emit = (kind, data) => process.stdout.write(`${JSON.stringify({ kind, ...data })}\n`);

let multi;
let target;
if (pageName === 'multi') {
  multi = await startMultiServer();
  target = multi.url;
} else if (/^https?:\/\//u.test(pageName)) {
  target = pageName;
} else {
  target = pathToFileURL(resolve(ref, 'test/mock-pages', pageName)).href;
}

// 與 BrowserManager.startUnlocked 相同順序：launch({ headless }) → newContext → 資源封鎖 → 聊天室封鎖 → newPage。
// PW_EXECUTABLE_PATH 只供沒有安裝 Playwright 瀏覽器時測試本腳本；正式對照不要設定。
const browser = await new PlaywrightBrowserLauncher('chromium').launch({
  headless: true,
  ...(process.env.PW_EXECUTABLE_PATH ? { executablePath: process.env.PW_EXECUTABLE_PATH } : {}),
});
try {
  const context = await browser.newContext({
    storageState: resolve(storageState),
    viewport: { width: 1280, height: 720 },
  });
  await context.configureResourceBlocking({
    blockImages: blocking,
    blockFonts: blocking,
    blockKnownTracking: blocking,
  });
  await context.configureChatBlocking(disableChat);
  const adapter = await context.newPage();
  adapter.onRequestFailed((diagnostic) => emit('requestfailed', diagnostic));
  adapter.onResponse((diagnostic) => emit('response', diagnostic));
  adapter.onConsole((diagnostic) => emit('console', diagnostic));
  adapter.onPopup((popup) => {
    emit('popup', {});
    void popup.close();
  });
  const { page } = adapter;
  page.setDefaultNavigationTimeout(30_000);
  await page.goto(target, { waitUntil: 'domcontentloaded' });
  await page.waitForTimeout(target.startsWith('http') ? 2_000 : 500);
  await page.screenshot({ type: 'png', fullPage: false });
  await adapter.close();
  await context.close();
} finally {
  await browser.close();
  multi?.close();
}
```

#### tools/ref/multi-server.mjs

```javascript
// pw-trace.mjs 與 twiclaimer probe 共用的 multi 測試頁：跨站 iframe、worker、console（含敏感字串）、
// 404／500、GraphQL POST（operationName）、不安全 port（net::ERR_UNSAFE_PORT）、圖片與字型、popup。
// 用法：node tools/ref/multi-server.mjs   → 印出 http://127.0.0.1:<port>/
// 127.0.0.1 與 localhost 是不同 site；iframe 是否成為獨立 target（OOPIF）取決於 site isolation，
// Rust 版兩種情況都要處理。
import { createServer } from 'node:http';
import process from 'node:process';
import { pathToFileURL } from 'node:url';

const PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=', 'base64');
const pages = {
  '/': (port) => ['text/html', `<!doctype html><title>multi</title>
<link rel="stylesheet" href="/font.css"><img src="/img.png" alt="">
<iframe src="http://localhost:${port}/frame.html"></iframe>
<script>
console.log('main', 1, { a: 1 }, [1, 2], undefined, null);
console.log('%s fmt', 'x', NaN, -0, 10n, true, Symbol('s'), () => 1, document.body, new Map([[1, 2]]), { nested: { deep: [1, { x: 'y' }] } });
console.warn('access_token=abc123 Bearer xyz');
fetch('/missing').catch(() => {});
fetch('/gql', { method: 'POST', body: JSON.stringify([{ operationName: 'PlaybackAccessToken' }, { operationName: 'ChannelPointsContext' }]) }).catch(() => {});
fetch('http://127.0.0.1:1/refused').catch(() => {});
new Worker('/worker.js');
setTimeout(() => window.open('/popup.html'), 200);
</script>`],
  '/frame.html': () => ['text/html', `<script>console.error('frame-error'); fetch('/missing-in-frame').catch(() => {});</script>`],
  '/worker.js': () => ['text/javascript', `console.info('worker-log'); fetch('/missing-in-worker').catch(() => {});`],
  '/popup.html': () => ['text/html', '<title>popup</title>'],
  '/font.css': () => ['text/css', '@font-face { font-family: f; src: url(/font.woff2); } body { font-family: f; }'],
  '/img.png': () => ['image/png', PNG],
};

// 啟動 multi 頁面伺服器（127.0.0.1、隨機 port）。回傳 { url, close }。
export async function startMultiServer() {
  const server = createServer((request, response) => {
    const path = new URL(request.url, 'http://x').pathname;
    const { port } = server.address();
    if (path === '/gql') {
      response.writeHead(500, { 'content-type': 'application/json' }).end('{}');
    } else if (pages[path]) {
      const [type, body] = pages[path](port);
      response.writeHead(200, { 'content-type': type }).end(body);
    } else {
      response.writeHead(404).end();
    }
  });
  await new Promise((done) => server.listen(0, '127.0.0.1', done));
  return {
    url: `http://127.0.0.1:${server.address().port}/`,
    close: () => server.close(),
  };
}

// 直接執行時：印出 URL 後持續服務，供 Rust 的 twiclaimer probe 使用（Ctrl-C 或 SIGTERM 結束）。
if (import.meta.url === pathToFileURL(process.argv[1] ?? '').href) {
  const { url, close } = await startMultiServer();
  console.log(url);
  for (const signal of ['SIGINT', 'SIGTERM']) {
    process.on(signal, () => {
      close();
      process.exit(0);
    });
  }
}
```

#### tools/ref/cdp-seq.py

```python
#!/usr/bin/env python3
"""把 CDP 紀錄轉成「每個 session 送出的指令」清單，比對 Playwright 與 Rust 版的初始化流程。

輸入：Playwright 的 DEBUG=pw:protocol 輸出，或 Rust 版 TWICLAIMER_CDP_TRACE 檔（格式相同：
      每行含 `SEND ► {json}` 或 `◀ RECV {json}`）。
輸出：依 session 分組、組內保留送出順序，每行 `<session>\t<method> <遮蔽後的 params>`。
      session 依 attach 順序命名（page#1、worker#1…）；沒有 sessionId 的是 browser。
      會變動的值（各種 id、下載路徑、port、mock-pages 的絕對路徑、init script 內容）會遮蔽或改成雜湊。
略過：evaluate／callFunctionOn 等執行期呼叫、攔截回應、截圖與導覽細節、Playwright utility world，
      以及 Browser.close（Playwright 直接寫入 pipe，不會出現在 pw:protocol 紀錄）。
用法：python3 tools/ref/cdp-seq.py doc/spec/traces/multi-b0.pw.log > doc/spec/traces/multi-b0.seq
      python3 tools/ref/cdp-seq.py /tmp/rust-multi-b0.log > /tmp/rust-multi-b0.seq && diff doc/spec/traces/multi-b0.seq /tmp/rust-multi-b0.seq
      加 --methods-only 只比對 method 名稱。
"""
import hashlib
import json
import re
import sys
from collections import defaultdict

SKIP = re.compile(
    r'^(Runtime\.(callFunctionOn|evaluate|releaseObject|getProperties)|DOM\.\w+|Input\.\w+'
    r'|Fetch\.(continueRequest|failRequest|fulfillRequest)|Page\.(getLayoutMetrics|captureScreenshot)'
    r'|Browser\.close)$'
)
ID_KEYS = {
    'targetId', 'frameId', 'browserContextId', 'sessionId', 'requestId', 'windowId', 'downloadPath',
    'objectId', 'executionContextId', 'contextId', 'uniqueContextId', 'interceptionId', 'loaderId',
    'backendNodeId', 'identifier',
}
UTILITY = '__playwright_utility_world'


def mask(value, key=None):
    if isinstance(value, dict):
        return {k: mask(v, k) for k, v in sorted(value.items())}
    if isinstance(value, list):
        return [mask(v) for v in value]
    if key in ID_KEYS:
        return '<id>'
    if key == 'source' and isinstance(value, str):
        return 'sha256:' + hashlib.sha256(value.encode()).hexdigest()[:12]
    if isinstance(value, str):
        value = re.sub(r'(127\.0\.0\.1|localhost):\d+', r'\1:<port>', value)
        value = re.sub(r'(?:file://)?/[^\s"]*?/mock-pages/', '<mock-pages>/', value)
        value = re.sub(r'@[0-9a-f]{32}$', '@<hash>', value)
    return value


def main():
    args = [a for a in sys.argv[1:] if not a.startswith('--')]
    methods_only = '--methods-only' in sys.argv
    if len(args) != 1:
        sys.exit(__doc__)
    labels = {}
    counts = defaultdict(int)
    groups = defaultdict(list)
    order = ['browser']
    with open(args[0], encoding='utf-8', errors='replace') as stream:
        for line in stream:
            send = line.find('SEND ► ')
            recv = line.find('◀ RECV ')
            if send < 0 and recv < 0:
                continue
            try:
                message = json.loads(line[(send if send >= 0 else recv) + 7:])
            except ValueError:
                continue
            if recv >= 0:
                if message.get('method') == 'Target.attachedToTarget':
                    params = message['params']
                    kind = params['targetInfo']['type']
                    counts[kind] += 1
                    labels[params['sessionId']] = f'{kind}#{counts[kind]}'
                    order.append(labels[params['sessionId']])
                continue
            method = message.get('method', '')
            params = message.get('params', {})
            if SKIP.match(method) or UTILITY in str(params.get('worldName', '')):
                continue
            session = message.get('sessionId')
            label = 'browser' if session is None else labels.get(session, 'unknown')
            if label == 'unknown' and label not in order:
                order.append(label)
            text = method if methods_only else f'{method} {json.dumps(mask(params), ensure_ascii=False, sort_keys=True)}'
            groups[label].append(text)
    for label in order:
        for text in groups.get(label, []):
            print(f'{label}\t{text}')


if __name__ == '__main__':
    main()
```

#### tools/ref/diag-diff.py

```python
#!/usr/bin/env python3
"""比對兩份診斷事件（pw-trace.mjs 與 twiclaimer probe 的 stdout，JSON Lines）。

事件順序不固定，所以當成 multiset 比較。關頁時才發生的 net::ERR_ABORTED 與時序有關，先排除。
mock-pages 的絕對路徑（舊版在 $REF/test，Rust 版在 testdata）先換成 <mock-pages>/。
用法：tools/ref/diag-diff.py <舊版.jsonl> <Rust.jsonl>；完全相同時 exit 0。
"""
import json
import re
import sys
from collections import Counter

TIMING_DEPENDENT = {'net::ERR_ABORTED'}


MOCK_PAGES = re.compile(r'(?:file://)?/[^\s"]*?/mock-pages/')


def normalize(value):
    if isinstance(value, str):
        return MOCK_PAGES.sub('<mock-pages>/', value)
    if isinstance(value, list):
        return [normalize(item) for item in value]
    if isinstance(value, dict):
        return {key: normalize(item) for key, item in value.items()}
    return value


def load(path):
    events = Counter()
    with open(path, encoding='utf-8') as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            event = json.loads(line)
            if event.get('kind') == 'requestfailed' and event.get('failureText') in TIMING_DEPENDENT:
                continue
            events[json.dumps(normalize(event), ensure_ascii=False, sort_keys=True)] += 1
    return events


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    old, new = load(sys.argv[1]), load(sys.argv[2])
    only_old, only_new = old - new, new - old
    for title, diff in (('只在舊版', only_old), ('只在 Rust 版', only_new)):
        for event, count in sorted(diff.items()):
            print(f'{title} ×{count}: {event}')
    print(f'舊版 {sum(old.values())} 筆、Rust 版 {sum(new.values())} 筆、差異 {sum(only_old.values()) + sum(only_new.values())} 筆')
    sys.exit(1 if only_old or only_new else 0)


if __name__ == '__main__':
    main()
```

### 附錄 B：scripts 與 chrome

| 檔案 | 用途 | 使用時機 |
| --- | --- | --- |
| `scripts/fetch-headless-shell.sh` | 下載並驗證 headless shell `151.0.7922.34`，輸出執行檔路徑 | 開發環境、Docker build |
| `scripts/check.sh` | 格式、clippy、測試、e2e、Node 工具測試、測試處置 | 每一步完成後、階段關卡 |
| `scripts/port-coverage.sh` | 檢查每個舊測試都有處置 | 由 `check.sh` 呼叫；階段關卡加 `--strict` |
| `scripts/compare-traces.sh` | 用 `twiclaimer probe` 重跑錄製並比對（G2、G3） | S1.4 起 |
| `chrome/SHA256SUMS` | headless shell 壓縮檔的 SHA256 | 由 `fetch-headless-shell.sh` 讀取 |

#### scripts/fetch-headless-shell.sh

```bash
#!/usr/bin/env bash
# 下載與 Playwright 1.62.1 相同版本的 headless shell，並輸出可執行檔路徑。
# 用法：scripts/fetch-headless-shell.sh [--dest 目錄] [linux64|linux-arm64|mac-arm64|mac-x64]
# - 未指定平台時依本機判斷。
# - SHA256 記錄在 chrome/SHA256SUMS（格式：<sha256> <版本> <平台>）。
#   檔案裡沒有該平台時會記錄首次下載的值；設定 REQUIRE_SHA256=1（Docker build）則直接失敗。
set -euo pipefail

CHROME_VERSION=151.0.7922.34   # Playwright 1.62.1 browsers.json：chromium-headless-shell
PW_CHROMIUM_REVISION=1234      # linux-arm64 沒有 Chrome for Testing 版本，改用 Playwright 的同版建置

root="$(cd "$(dirname "$0")/.." && pwd)"
dest=""
platform=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dest) dest="$2"; shift 2 ;;
    *) platform="$1"; shift ;;
  esac
done
if [[ -z "${platform}" ]]; then
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) platform=linux64 ;;
    Linux-aarch64 | Linux-arm64) platform=linux-arm64 ;;
    Darwin-arm64) platform=mac-arm64 ;;
    Darwin-x86_64) platform=mac-x64 ;;
    *) echo "不支援的平台：$(uname -s)-$(uname -m)" >&2; exit 1 ;;
  esac
fi
case "${platform}" in
  linux64 | mac-arm64 | mac-x64)
    url="https://storage.googleapis.com/chrome-for-testing-public/${CHROME_VERSION}/${platform}/chrome-headless-shell-${platform}.zip"
    dir="chrome-headless-shell-${platform}"
    bin="chrome-headless-shell" ;;
  linux-arm64)
    url="https://cdn.playwright.dev/builds/chromium/${PW_CHROMIUM_REVISION}/chromium-headless-shell-linux-arm64.zip"
    dir="chrome-linux"
    bin="headless_shell" ;;
  *) echo "未知平台：${platform}" >&2; exit 1 ;;
esac

dest="${dest:-${root}/.cache/headless-shell/${CHROME_VERSION}/${platform}}"
exe="${dest}/${dir}/${bin}"
if [[ ! -x "${exe}" ]]; then
  tmp="$(mktemp -d)"
  trap 'rm -rf "${tmp}"' EXIT
  curl -fsSL --retry 3 -o "${tmp}/shell.zip" "${url}"
  if command -v sha256sum > /dev/null; then
    sum="$(sha256sum "${tmp}/shell.zip")"
  else
    sum="$(shasum -a 256 "${tmp}/shell.zip")"
  fi
  sum="${sum%% *}"
  sums="${root}/chrome/SHA256SUMS"
  key="${CHROME_VERSION} ${platform}"
  recorded="$(grep -E " ${key//./\\.}\$" "${sums}" 2> /dev/null | cut -d' ' -f1 || true)"
  if [[ -n "${recorded}" ]]; then
    [[ "${recorded}" == "${sum}" ]] || { echo "SHA256 不符（${key}）：預期 ${recorded}，實際 ${sum}" >&2; exit 1; }
  elif [[ "${REQUIRE_SHA256:-0}" == "1" ]]; then
    echo "chrome/SHA256SUMS 缺少 ${key}" >&2
    exit 1
  else
    mkdir -p "$(dirname "${sums}")"
    echo "${sum} ${key}" >> "${sums}"
    echo "已記錄首次下載的 SHA256：${sum} ${key}（請提交 chrome/SHA256SUMS）" >&2
  fi
  mkdir -p "${dest}"
  unzip -q "${tmp}/shell.zip" -d "${dest}"
fi
echo "${exe}"
```

#### scripts/check.sh

```bash
#!/usr/bin/env bash
# 每一步完成後執行的檢查（本機與 Docker test stage 共用）。
# 用法：scripts/check.sh
#   CHROME_PATH 有值時另跑標記 #[ignore] 的 e2e（需要真的 headless shell）。
#   REQUIRE_E2E=1 時缺少 CHROME_PATH 直接失敗（階段關卡與 Docker test stage 使用）。
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked

if [[ -n "${CHROME_PATH:-}" ]]; then
  cargo test --locked -- --ignored --test-threads=2
elif [[ "${REQUIRE_E2E:-0}" == "1" ]]; then
  echo "缺少 CHROME_PATH：先執行 export CHROME_PATH=\"\$(scripts/fetch-headless-shell.sh)\"" >&2
  exit 1
else
  echo "略過 e2e：未設定 CHROME_PATH" >&2
fi

if [[ -d node_modules ]]; then
  npm test
elif [[ "${REQUIRE_NODE_TOOLS:-0}" == "1" ]]; then
  echo "缺少 node_modules：先執行 npm ci" >&2
  exit 1
else
  echo "略過 Node 工具測試：未執行 npm ci" >&2
fi

scripts/port-coverage.sh
```

#### scripts/port-coverage.sh

```bash
#!/usr/bin/env bash
# 檢查舊測試是否都有處置：Rust 測試內的「// 原測試：<標題>」註解，或 doc/test-port.md 標為「不移植」「保留 Node」的列。
# 用法（在 twiclaimer 根目錄）：
#   scripts/port-coverage.sh                    回報進度；只有「未知標題」（打錯字）時 exit 1
#   scripts/port-coverage.sh --strict [舊檔案…]  另外要求指定的舊測試檔（未指定＝全部）沒有缺漏
#     例：scripts/port-coverage.sh --strict test/unit/config.test.ts test/unit/stream-selector.test.ts
set -euo pipefail
cd "$(dirname "$0")/.."
strict=0
if [[ "${1:-}" == "--strict" ]]; then
  strict=1
  shift
fi
titles=doc/spec/ts-test-titles.tsv
[[ -f $titles ]] || { echo "找不到 $titles（先執行 tools/ref/extract-spec.sh）" >&2; exit 2; }
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
trim() { sed -E 's/^[[:space:]]+//; s/[[:space:]]+$//'; }

cut -f2 "$titles" | trim | sort > "$tmp/all"
sort -u "$tmp/all" > "$tmp/known"
if [[ $(wc -l < "$tmp/all") -ne $(wc -l < "$tmp/known") ]]; then
  echo "警告：舊測試標題有重複，請在 test-port.md 註明：" >&2
  uniq -d "$tmp/all" >&2
fi
if [[ $# -gt 0 ]]; then
  printf '%s\n' "$@" > "$tmp/files"
  awk -F'\t' 'NR == FNR { want[$0] = 1; next } { f = $1; sub(/:[0-9]+$/, "", f); if (f in want) print $2 }' \
    "$tmp/files" "$titles" | trim | sort -u > "$tmp/expected"
  [[ -s $tmp/expected ]] || { echo "指定的舊測試檔沒有任何標題：$*" >&2; exit 2; }
else
  cp "$tmp/known" "$tmp/expected"
fi

{ grep -rhoE '// 原測試：.+$' src tests 2>/dev/null || true; } | sed -E 's#^// 原測試：##' | trim | sort -u > "$tmp/ported"
if [[ -f doc/test-port.md ]]; then
  # 表格欄位：| 舊檔案 | 原測試 | 處置 | Rust 測試／理由 |
  awk -F'|' 'NF >= 6 { t = $3; d = $4; r = $5
    gsub(/^[ \t]+|[ \t]+$/, "", t); gsub(/^[ \t]+|[ \t]+$/, "", d); gsub(/^[ \t]+|[ \t]+$/, "", r)
    if ((d == "不移植" || d == "保留 Node") && r != "") print t }' doc/test-port.md | sort -u > "$tmp/skipped"
else
  : > "$tmp/skipped"
fi
sort -u "$tmp/ported" "$tmp/skipped" > "$tmp/covered"

missing=$(comm -23 "$tmp/expected" "$tmp/covered")
unknown=$(comm -13 "$tmp/known" "$tmp/covered")
n_missing=$(printf '%s' "$missing" | grep -c . || true)
n_unknown=$(printf '%s' "$unknown" | grep -c . || true)
echo "舊測試 $(wc -l < "$tmp/expected") 筆：已處置 $(comm -12 "$tmp/expected" "$tmp/covered" | wc -l)、缺 ${n_missing}、未知標題 ${n_unknown}"
if [[ $n_missing -ne 0 ]]; then
  if [[ $strict -eq 1 ]]; then
    echo "== 缺少處置 =="
    printf '%s\n' "$missing"
  else
    echo "（加 --strict 列出缺少處置的標題）"
  fi
fi
[[ $n_unknown -eq 0 ]] || { echo "== 未知標題（打錯字或不在清單）=="; printf '%s\n' "$unknown"; }
[[ $n_unknown -eq 0 && ( $strict -eq 0 || $n_missing -eq 0 ) ]]
```

#### scripts/compare-traces.sh

```bash
#!/usr/bin/env bash
# G2、G3：用 twiclaimer probe 重跑 S1.2 的每個錄製，比對 CDP 指令順序（.seq）與診斷事件（.diag.jsonl）。
# 用法：scripts/compare-traces.sh [名稱…]   預設為 doc/spec/traces/*.seq 全部
#   名稱格式：<頁面>-b<0|1>[-chat0]，例如 live-b0、multi-b1、live-b0-chat0
#   BIN 指定執行檔（預設 target/debug/twiclaimer）；KEEP=1 保留輸出目錄。
set -euo pipefail
cd "$(dirname "$0")/.."
bin="${BIN:-target/debug/twiclaimer}"
traces=doc/spec/traces
state=testdata/fixtures/storage-state.fake.json
out="$(mktemp -d)"
server=""
multi=""
cleanup() {
  if [[ -n $server ]]; then
    kill "$server" 2>/dev/null || true
    wait "$server" 2>/dev/null || true
  fi
  if [[ ${KEEP:-0} == 1 ]]; then echo "輸出保留在 $out" >&2; else rm -rf "$out"; fi
}
trap cleanup EXIT

start_multi() {
  [[ -z $server ]] || return 0
  node tools/ref/multi-server.mjs > "$out/multi.url" &
  server=$!
  for _ in $(seq 100); do
    [[ -s $out/multi.url ]] && break
    sleep 0.1
  done
  multi="$(head -n1 "$out/multi.url")"
  [[ -n $multi ]] || { echo "multi-server 沒有啟動" >&2; exit 2; }
}

names=("$@")
if [[ ${#names[@]} -eq 0 ]]; then
  for file in "$traces"/*.seq; do names+=("$(basename "$file" .seq)"); done
fi

failed=0
for name in "${names[@]}"; do
  if [[ ! $name =~ ^(.+)-b([01])(-chat0)?$ ]]; then
    echo "無法解析名稱：$name" >&2
    exit 2
  fi
  page="${BASH_REMATCH[1]}"
  blocking="${BASH_REMATCH[2]}"
  chat=1
  [[ -z ${BASH_REMATCH[3]} ]] || chat=0
  if [[ $page == multi ]]; then
    start_multi
    target="$multi"
  else
    target="$page.html"
  fi
  if ! TRACE_BLOCKING=$blocking TRACE_DISABLE_CHAT=$chat TWICLAIMER_CDP_TRACE="$out/$name.log" \
    "$bin" probe "$state" "$target" > "$out/$name.diag.jsonl"; then
    echo "$name：probe 失敗"
    failed=1
    continue
  fi
  python3 tools/ref/cdp-seq.py "$out/$name.log" > "$out/$name.seq"
  result="$name："
  if diff -u "$traces/$name.seq" "$out/$name.seq" > "$out/$name.seq.diff"; then
    result+="CDP 順序相同"
  else
    result+="CDP 順序不同（$out/$name.seq.diff）"
    failed=1
  fi
  if python3 tools/ref/diag-diff.py "$traces/$name.diag.jsonl" "$out/$name.diag.jsonl" > "$out/$name.diag.diff"; then
    result+="；診斷相同"
  else
    result+="；診斷不同（$out/$name.diag.diff）"
    failed=1
  fi
  echo "$result"
done
if [[ $failed != 0 ]]; then
  KEEP=1
  exit 1
fi
```

#### chrome/SHA256SUMS

linux-arm64 的值依 S0.2 的架構決定是否在 S1.1 補上（§13）。

```text
3cfc2bd00d1bafcf8a68dc74c9c92bb7150ddc8d26ade948a776316e1cec4f14 151.0.7922.34 linux64
```

### 附錄 C：tools/bench（S0 與 A/B 分析）

| 檔案 | 用途 | 使用時機 |
| --- | --- | --- |
| `snapshot_stats.py` | 依頁數統計控制端與整容器的記憶體、CPU（R1、R2） | S0.6、§10 |
| `log-schema.py` | 列出或比較兩版日誌的事件、層級與欄位（R9、R10） | S0.6、§10 |

#### tools/bench/snapshot_stats.py

```python
#!/usr/bin/env python3
"""依頁數分組統計 runtime_resource_snapshot：控制端與整容器的記憶體中位數、平均 CPU 與占比。

用法：cat snapshots-*.jsonl | python3 tools/bench/snapshot_stats.py
多次匯出的檔案可直接串接；同一時間的樣本只算一次。
"""
import json
import statistics
import sys
from collections import defaultdict
from datetime import datetime

MIB = 2**20
rows = {}  # 時間 -> (頁數, 控制端 RSS, 整容器記憶體, 控制端 CPU µs, 整容器 CPU µs)
for line in sys.stdin:
    try:
        r = json.loads(line)
        if r.get('event') != 'runtime_resource_snapshot':
            continue
        t = datetime.fromisoformat(r['time'].replace('Z', '+00:00')).timestamp()
        rows[t] = (
            int(r['browserPageCount']),
            float(r['processRssBytes']),
            float(r['cgroupMemoryCurrentBytes']),
            float(r['processCpuUserUs']) + float(r['processCpuSystemUs']),
            float(r['cgroupCpuUsageUsec']),
        )
    except (ValueError, KeyError, TypeError, AttributeError):
        continue
if not rows:
    sys.exit('找不到 runtime_resource_snapshot')

mem = defaultdict(list)                     # 頁數 -> [(控制端 RSS, 整容器記憶體)]
cpu = defaultdict(lambda: [0.0, 0.0, 0.0])  # 頁數 -> [控制端 CPU µs, 整容器 CPU µs, 秒數]
prev = None
for t in sorted(rows):
    pages, rss, cg_mem, p_cpu, cg_cpu = rows[t]
    mem[pages].append((rss, cg_mem))
    if prev is not None:
        dt, dp, dc = t - prev[0], p_cpu - prev[1], cg_cpu - prev[2]
        if 0 < dt <= 330 and dp >= 0 and dc >= 0:  # 300 秒區間含排程抖動；略過重啟造成的負差值與長時間中斷
            acc = cpu[pages]
            acc[0] += dp
            acc[1] += dc
            acc[2] += dt
    prev = (t, p_cpu, cg_cpu)

print('頁數  樣本  控制端RSS  整容器記憶體  RAM占比  控制端CPU  整容器CPU  CPU占比')
for pages in sorted(mem):
    rss = statistics.median(x for x, _ in mem[pages]) / MIB
    cg = statistics.median(y for _, y in mem[pages]) / MIB
    p, c, s = cpu[pages]
    p_cores = p / (s * 1e6) if s else float('nan')
    c_cores = c / (s * 1e6) if s else float('nan')
    share = p / c if c else float('nan')
    print(f'{pages:>4}  {len(mem[pages]):>4}  {rss:>7.0f}MiB  {cg:>9.0f}MiB  {rss / cg:>6.1%}'
          f'  {p_cores:>7.3f}核  {c_cores:>7.3f}核  {share:>6.1%}')
```

#### tools/bench/log-schema.py

```python
#!/usr/bin/env python3
"""比對兩版日誌的結構（R10）：每個事件出現過的層級與欄位（含巢狀欄位與 JSON 型別）。

用法：
  python3 tools/bench/log-schema.py <日誌.jsonl>                  列出結構
  python3 tools/bench/log-schema.py <舊版.jsonl> <Rust 版.jsonl>  比較；有「差異」時 exit 1
輸入是 docker compose logs --no-log-prefix 的輸出；非 JSON 的行只計數。
每行輸出：<分類>\t<事件>\t<說明>。分類：
  允許    §6.2 X5、X6 的差異
  差異    同一事件的層級或欄位（含型別）不同；或 Rust 版出現 doc/spec/log-events.txt 沒有的事件
  僅單邊  事件只出現在其中一版，通常是兩次執行遇到的情況不同
「差異」與「僅單邊」要逐項修正，或在 A/B 報告附上舊版程式碼位置，說明只是執行情境不同。
"""
import json
import sys
from collections import defaultdict
from pathlib import Path

RESERVED = {'level', 'event', 'time'}
ALLOWED_FIELDS = {'stack', 'processHeapUsedBytes', 'processExternalBytes', 'nodeVersion', 'playwrightVersion'}
ALLOWED_OLD_ONLY_EVENTS = {'unhandled_rejection'}
EVENTS_FILE = Path('doc/spec/log-events.txt')
MAX_DEPTH = 4


def kind(value):
    if value is None:
        return 'null'
    if isinstance(value, bool):
        return 'bool'
    if isinstance(value, (int, float)):
        return 'number'
    if isinstance(value, str):
        return 'string'
    return 'array' if isinstance(value, list) else 'object'


def walk(value, path, fields, depth=0):
    fields.add(f'{path}:{kind(value)}')
    if depth >= MAX_DEPTH:
        return
    if isinstance(value, dict):
        for key, child in value.items():
            walk(child, f'{path}.{key}', fields, depth + 1)
    elif isinstance(value, list):
        for child in value:
            walk(child, f'{path}[]', fields, depth + 1)


def load(path):
    schema = defaultdict(lambda: {'count': 0, 'levels': set(), 'fields': set()})
    skipped = 0
    with open(path, encoding='utf-8', errors='replace') as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                record = json.loads(line)
            except ValueError:
                skipped += 1
                continue
            if not isinstance(record, dict) or not isinstance(record.get('event'), str):
                skipped += 1
                continue
            entry = schema[record['event']]
            entry['count'] += 1
            entry['levels'].add(str(record.get('level')))
            for key, value in record.items():
                if key not in RESERVED:
                    walk(value, key, entry['fields'])
    if skipped:
        print(f'{path}：略過 {skipped} 行非 JSON 日誌', file=sys.stderr)
    return schema


def is_allowed_field(field):
    path = field.rsplit(':', 1)[0].replace('[]', '')
    return any(part in ALLOWED_FIELDS for part in path.split('.'))


def show(schema):
    for event in sorted(schema):
        entry = schema[event]
        levels = ','.join(sorted(entry['levels']))
        print(f"{event}\t{entry['count']}\t{levels}\t{' '.join(sorted(entry['fields']))}")


def compare(old, new):
    known = set(EVENTS_FILE.read_text(encoding='utf-8').split()) if EVENTS_FILE.exists() else None
    failures = 0

    def report(category, event, text):
        nonlocal failures
        failures += category == '差異'
        print(f'{category}\t{event}\t{text}')

    for event in sorted(set(old) | set(new)):
        a, b = old.get(event), new.get(event)
        if a is None:
            if known is not None and event not in known:
                report('差異', event, f"不在 log-events.txt 的事件（Rust 版 {b['count']} 筆）")
            else:
                report('僅單邊', event, f"只在 Rust 版（{b['count']} 筆）")
            continue
        if b is None:
            category = '允許' if event in ALLOWED_OLD_ONLY_EVENTS else '僅單邊'
            report(category, event, f"只在舊版（{a['count']} 筆）")
            continue
        if a['levels'] != b['levels']:
            levels_a, levels_b = ','.join(sorted(a['levels'])), ','.join(sorted(b['levels']))
            report('差異', event, f'層級：舊版 {levels_a}；Rust 版 {levels_b}')
        for side, fields in (('舊版', a['fields'] - b['fields']), (' Rust 版', b['fields'] - a['fields'])):
            for field in sorted(fields):
                report('允許' if is_allowed_field(field) else '差異', event, f'欄位只在{side}：{field}')
    print(f'事件種類：舊版 {len(old)}、Rust 版 {len(new)}；差異 {failures} 項', file=sys.stderr)
    return failures


def main():
    if len(sys.argv) == 2:
        show(load(sys.argv[1]))
    elif len(sys.argv) == 3:
        sys.exit(1 if compare(load(sys.argv[1]), load(sys.argv[2])) else 0)
    else:
        print(__doc__, file=sys.stderr)
        sys.exit(2)


if __name__ == '__main__':
    main()
```

### 附錄 D：範本與 fixture

#### doc/progress.md

狀態只用：未開始、進行中、完成、卡住。每一步 commit 前更新；階段耗時超過「停止門檻」時依 §2.3 停止。

```markdown
# 進度

- 目前步驟：S1.1
- 卡住的項目：無

## 階段

| 階段 | 估計上限 | 停止門檻 | 已用 | 狀態 |
| --- | --- | --- | --- | --- |
| S0 | 0.5 天 + 24 小時 | 0.75 天 | | 未開始 |
| S1 | 5 天 | 7.5 天 | | 未開始 |
| S2 | 6 天 | 9 天 | | 未開始 |
| S3 | 9 天 | 13.5 天 | | 未開始 |
| S4 | 4 天 | 6 天 | | 未開始 |
| S5 | 3 天 + 24–72 小時 | 4.5 天 | | 未開始 |

## 步驟

| 步驟 | 狀態 | 完成日期 | 耗時 | 驗證結果 | commit |
| --- | --- | --- | --- | --- | --- |
| S0.1–S0.7 | 未開始 | | | | |
| S1.1 | 未開始 | | | | |
| S1.2 | 未開始 | | | | |
| S1.3 | 未開始 | | | | |
| S1.4 | 未開始 | | | | |
| S1.5 | 未開始 | | | | |
| S1.6 | 未開始 | | | | |
| S1.7 | 未開始 | | | | |
| S1.8 | 未開始 | | | | |
| S1 關卡 | 未開始 | | | | |
| S2.1 | 未開始 | | | | |
| S2.2 | 未開始 | | | | |
| S2.3 | 未開始 | | | | |
| S2.4 | 未開始 | | | | |
| S2.5 | 未開始 | | | | |
| S2.6 | 未開始 | | | | |
| S2 關卡 | 未開始 | | | | |
| S3.1 | 未開始 | | | | |
| S3.2 | 未開始 | | | | |
| S3.3 | 未開始 | | | | |
| S3.4 | 未開始 | | | | |
| S3.5 | 未開始 | | | | |
| S3.6 | 未開始 | | | | |
| S3 關卡 | 未開始 | | | | |
| S4.1 | 未開始 | | | | |
| S4.2 | 未開始 | | | | |
| S4.3 | 未開始 | | | | |
| S4 關卡 | 未開始 | | | | |
| S5.1 | 未開始 | | | | |
| S5.2 | 未開始 | | | | |
| S5.3 | 未開始 | | | | |
| S5.4 | 未開始 | | | | |
| S5.5 | 未開始 | | | | |
| S5.6 | 未開始 | | | | |

## 階段回報

### S1

- 完成項目：
- 關卡結果：G1–G9 各一行，附指令與輸出摘要
- 實際耗時：
- decisions.md 新增：
- 下一階段的風險：
```

#### doc/decisions.md

以下情況一定要記一筆：§7 與舊版程式碼不一致、修改附錄的檔案、需要解讀或取捨、發現舊版 bug（維持舊行為）、新增 §5 以外的依賴、saphyr 與 yaml 套件判定不同（§7.1）、injected script 的版本／位元組數／SHA-256（S1.2）、錄製不穩定的項目（S1.2）、工具輸出的數量和本文件不同、因 Twitch 改版修改選擇器。

```markdown
# 決策紀錄

每筆一個小節，編號遞增。不修改舊紀錄；要推翻時新增一筆，寫明「取代 DEC-00N」。

## DEC-001 標題

- 日期：YYYY-MM-DD（步驟：S1.2）
- 問題：
- 決定：
- 理由：
- 影響：檔案、測試、§6.2 的允許差異；需要人工確認時寫明
```

#### doc/test-port.md

`scripts/port-coverage.sh` 把「處置」是「不移植」或「保留 Node」、且最後一欄不是空白的列算成已處置。「原測試」必須與 `doc/spec/ts-test-titles.tsv` 完全相同。以下是已知的列，之後有新的不移植項目時先寫 `decisions.md` 再加列。

```markdown
# 測試移植處置

已移植的測試以 Rust 測試上一行的 `// 原測試：` 註解為準，不列在這裡。本表只列不移植、保留 Node，以及預期值依 §6.2 調整的測試。

| 舊檔案 | 原測試 | 處置 | Rust 測試／理由 |
| --- | --- | --- | --- |
| test/unit/default-browser.test.ts | maps common default browser identifiers | 保留 Node | test-node/default-browser.test.ts；登入工具維持 Node（D2） |
| test/unit/default-browser.test.ts | marks Safari as unsupported for automation | 保留 Node | test-node/default-browser.test.ts；登入工具維持 Node（D2） |
| test/unit/default-browser.test.ts | puts a supported default browser first in preference order | 保留 Node | test-node/default-browser.test.ts；登入工具維持 Node（D2） |
| test/unit/default-browser.test.ts | does not put Safari in the launch order as a candidate | 保留 Node | test-node/default-browser.test.ts；登入工具維持 Node（D2） |
| test/unit/default-browser.test.ts | detects Twitch auth-token cookies | 保留 Node | test-node/default-browser.test.ts；登入工具維持 Node（D2） |
| test/unit/default-browser.test.ts | treats chromium family candidates as native launch when executable exists | 保留 Node | test-node/default-browser.test.ts；登入工具維持 Node（D2） |
| test/unit/resource-snapshots-to-csv.test.ts | 保留 cgroup CPU 累計時間欄位 | 保留 Node | test-node/resource-snapshots-to-csv.test.ts；CSV 腳本維持 Node（D7） |
| test/unit/playwright-browser-launcher.test.ts | firefox engine 仍使用 Firefox 並回傳同一 adapter | 不移植 | 常駐服務只支援 Chromium（D1、X1） |
| test/unit/config.test.ts | 載入合法 YAML 並將 snake_case 轉為 camelCase | 已移植（依 X1／X2 調整） | fixture valid.yml 改成 headless: true，預期值跟著改 |
| test/unit/config.test.ts | 替換 ${ENV_VAR} 且讓直接環境覆寫優先 | 已移植（依 X1／X2 調整） | fixture env.yml 改成 headless: false；改用 HEADLESS: 'TRUE'，預期 headless 為 true |
| test/unit/config.test.ts | 明確指定 firefox 仍受支援 | 已移植（依 X1／X2 調整） | 改為斷言 X1 的 ConfigValidationError |
```

#### doc/s0-report.md

```markdown
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
```

#### doc/ab-report.md

```markdown
# A/B 比較報告

- A：期間、來源（S0 資料或重跑）
- B：期間、Rust 版 commit
- 同一台主機、同一份設定；設定差異：無
- 日誌 ab-logs-*.jsonl 不進 repo

## 結果

| 需求 | A（舊版） | B（Rust） | 通過條件 | 結果 |
| --- | --- | --- | --- | --- |
| R1 控制端 RSS 中位數 | | | ≤ 60 MiB | |
| R1 整容器記憶體中位數（各頁數） | | | ≤ A 的 105% | |
| R2 控制端平均 CPU | | | 低於 A | |
| R2 整容器 CPU | | | ≤ A 的 105% | |
| R3 image 大小 | | | ≤ A 的 1/3 | |
| R6 Bot 指令與通知 | | | 與舊版相同 | |
| R7 領取、RestartCount、OOMKilled、headless shell 程序數 | | | 有領取、無 OOM、不增加、每次 1 個 | |
| R8 設定寫回 | | | 只有兩個值的文字改變 | |
| R9、R10 log-schema.py | | | 只有「允許」，或每項都有說明 | |

## 差異說明

log-schema.py 的每個「差異」「僅單邊」各一行：事件、說明、舊版程式碼位置（檔案:行號）、判斷。

## 建議

- 切換或不切換，理由：
- 人工決定（日期、決定者）：
```

#### testdata/fixtures/storage-state.fake.json

`record-traces.sh`、`compare-traces.sh`、G4 的 e2e 與 G9 共用。

```json
{
  "cookies": [
    {
      "name": "auth-token",
      "value": "fake-token",
      "domain": ".twitch.tv",
      "path": "/",
      "expires": -1,
      "httpOnly": false,
      "secure": true,
      "sameSite": "None"
    }
  ],
  "origins": [
    {
      "origin": "https://www.twitch.tv",
      "localStorage": [
        { "name": "fake-key", "value": "fake-value" }
      ]
    }
  ]
}
```
