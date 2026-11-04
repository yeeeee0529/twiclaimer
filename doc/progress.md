# 進度

- 目前狀態：**進行中（2026-11-04）**——S3.5 完成（s35‑manager／s35‑coord 兩代理平行移植、主會話合併驗證）、S4 關卡完成，S3.6 尚未派單。已完成步驟：S1.1–S1.8、S2.1–S2.6、S3.1–S3.5、S4.1–S4.3、S1 關卡（本機可驗部分）、S2 關卡、S4 關卡；S1.7 完成。S3.6、S3 關卡、S5 未完成。執行中的代理：無
- 測試移植（2026-11-04 以 `scripts/port-coverage.sh` 量測）：舊清單 434 列（唯一 433；基準 `d49363d`＋DEC‑020 修 extract‑spec 補抓；log‑events 154）：已處置 416、缺 17、未知標題 0；缺的集中在 S3.6（app‑runner、integration；`session_start_timeout` 等 4 個逾時事件名不在 log‑events，S3.6 重抽時補）
- DEC-016：使用者裁示**納入**舊版 `306b4ba`、`d49363d` 兩個 commit（基準改為 `d49363d`），重跑 `extract-spec.sh`（+10 唯一標題、log‑events +3 事件）；已完成的移植不回頭改（兩 commit 未動到已移植模組）
- DEC-017：`Locator::evaluate` 的 undefined 回值不再視為「沒有元素」（S1.7 e2e 揭露，依 Playwright 1.62.1 實際行為修正 `src/browser/cdp/locator.rs`）；existing e2e 全綠
- 觀察到的既有不穩定：
  - `tests::browser_launch::cancelling_close_kills_a_browser_that_ignores_browser_close`（S1.3 假瀏覽器測試，非 ignore）在全套平行跑時失敗過一次（process group 收屍時序），單跑與後續全跑皆通過；持續觀察
  - `tests::browser_diagnostics::page_adapter_converts…`（S1.6 e2e）2026-10-20 在 `--ignored --test-threads=2` 全量跑失敗一次，單跑與重跑全量皆過
  - `tests::multi_page::sigkill…`（S1.8 e2e）同日同模式失敗一次——查出是 S1.8 寫時尚未有 X7 前綴的**過時斷言**（已依 pw-errors.jsonl 修正為 `page.evaluate: …`），修正後全量重跑全過
  - `tests::browser_diagnostics::browser_manager_logs…`（S3.3 e2e）2026-10-20 首次全量跑失敗一次，單跑與重跑全量皆過
  - 共同特徵：都在平行負載下的 e2e 時序敏感斷言；若再現就收緊 `eventually` 的輪詢視窗
- 待決策：舊版（`twitch-watchdog-docker`）在基準 `e0e079a` 後另有 `306b4ba`、`d49363d` 兩個 commit，動到尚未移植的 ChannelSession／SessionManager／Telegram，並新增 11 個 test 宣告（含 `it.each`）；`doc/spec/ts-test-titles.tsv` 於 2026-10-19 抽出，未包含這些。移植 S3.4／S3.5／S4 前先決定是否納入
- 卡住的項目：無

## 階段

| 階段 | 估計上限 | 停止門檻 | 已用 | 狀態 |
| --- | --- | --- | --- | --- |
| S0 | 0.5 天 + 24 小時 | 0.75 天 | 0 | 未開始 |
| S1 | 5 天 | 7.5 天 | 2.1 | 步驟＋關卡完成（關卡為本機可驗部分；G4【人工】、G8/G9 Linux 部分留 Docker） |
| S2 | 6 天 | 9 天 | 1.5 | 步驟＋關卡完成（`REQUIRE_E2E=1` 等價檢查＋strict 114/114） |
| S3 | 9 天 | 13.5 天 | 1.1 | 進行中（S3.1–S3.5 完成；S3.6 尚未派單） |
| S4 | 4 天 | 6 天 | 0.1 | 步驟＋關卡完成（S4.1–S4.3＋關卡：等價檢查＋strict 32/32） |
| S5 | 3 天 + 24–72 小時 | 4.5 天 | 0 | 未開始 |

「已用」為已完成步驟「耗時」欄的累計；S1–S3 有平行執行，與牆鐘時間不同。

## 步驟

| 步驟 | 狀態 | 完成日期 | 耗時 | 驗證結果 | commit |
| --- | --- | --- | --- | --- | --- |
| S0.1–S0.7 | 未開始 | | | | |
| S1.1 | 完成 | 2026-10-19 | 0.2 天 | `scripts/check.sh` 全綠（fmt／clippy／test、npm test 7 個、port-coverage 8 已處置／0 未知）；`extract-spec.sh` 產出 397 列測試標題與 149 個日誌事件（DEC-010／011 修正抽取後為 423 列、422 個唯一標題） | 3f6eee5 |
| S1.2 | 完成 | 2026-10-19 | 0.2 天 | `pw-errors.jsonl` 33 列；9 組錄製均有 `.seq` 與 `.diag.jsonl`；`chromium-args.txt` 45 個參數含 `--remote-debugging-pipe`；二次錄製 `git diff` 零差異；`src/browser/js/` 6 個頁面內 JS 逐字複製並通過 Node 語法檢查 | 35d355d |
| S1.3 | 完成 | 2026-10-19 | 0.5 天 | transport（6）＋ connection 路由（13）＋ Connection 讀寫接線（10）＋ launcher 程序面機制（16）＝45 個測試；本段完成 launch 握手（`getVersion` → `setAutoAttach`，180 秒整體逾時、Playwright 同款訊息）、`LaunchedBrowser` 的 close／drop／取消／EOF 防線（一律整群 `SIGKILL`、背景收屍）與 disconnected 觸發一次、macOS 的 `dup2` 退路（DEC-007）。83 個單元＋3 個 e2e（真實 headless shell：`get_version()` 為 `151.0.7922.34`、錄製參數逐行比對、close／drop 後無殘留程序與暫存目錄）全過，clippy 乾淨；G8 的 Linux 部分留 Docker test stage | a9f94fc |
| S1.4 | 完成 | 2026-10-19 | 0.5 天 | context（cookies 轉換、localStorage 暫時頁、setDownloadBehavior）＋page 初始化序列＋嵌套 auto-attach（page／iframe OOPIF／worker／popup）＋資源阻擋／聊天室封鎖（init script 雜湊逐位元組）＋`goto`／`screenshot`／`close`＋**probe 子命令**（`#[path]` 掛載，DEC-014）。**G2 通過：9 組錄製 CDP 順序全部相同；diag-diff 診斷也 9 組全部相同（G3 比對工具零差異）**，並以修好的 repo 腳本（bash 3.2 相容）重跑確認。債：page.rs／context.rs／probe.rs 尚無單元測試（S1.6 補 listener 測試時一併補）；G4 的 trace 匯出仍屬【人工】 | 
| S1.5 | 完成 | 2026-10-20 | 0.4 天 | injected script 生命週期（X8）＋locator 全套 API（count／isVisible／waitFor／getAttribute／textContent／evaluate／evaluateAll／dispatchEvent／click／hover，selector 組合照 `locator.ts`）＋click/hover 重試機制（`[0,20,100,100,500]` 間隔、checkElementStates、scrollIntoView 輪替、getContentQuads＋LayoutMetrics、hit target 攔截器、訊號屏障）＋`reload`／`mouse.move`／`evaluate(fn, arg)`＋abort 機制（`AbortError`）。**G5：pw-errors.jsonl 33/33 全過**（e2e 5.1 秒）。三個代理接力完成（前兩個 session 中斷，第四棒收尾）；期間新發現並修復：`send` 的 `is_closed()` 快速失敗、crashed session 短線路（`Target crashed ` 帶尾端空白，S1.8 待整合項 1）、`close()` crashed 時仍送 `Target.closeTarget`（待整合項 2）。積壓整合：事件泵 GraphQL 抽出已改接 `diagnostics.rs` 純函式（UTF-16 截斷語意，G3 回歸 9/9 綠）。S1.8 的 sigkill e2e 斷言依 X7 加上 `page.evaluate: ` 前綴（pw-errors.jsonl 為準） | 
| S1.6 | 完成 | 2026-10-19 | 0.2 天 | `PlaywrightBrowserPageAdapter` 移植（`src/browser/page_adapter.rs`）：三個診斷 listener、`sanitizeBrowserUrl`／`joinSanitizedUrl`／redact／UTF-16 截斷與省略條件照舊版；**9 個舊測試全移植（port-coverage strict 9/9）**＋1 補測；e2e `browser_diagnostics`（本機 TCP 伺服器取代 `page.route`）通過且連跑 4 次穩定。extraction 照 DEC-006 重用 `diagnostics.rs` 純函式；吞錯以 `catch_unwind` 表達。債：page.rs 事件泵的內聯 GraphQL 抽出改接純函式，留到 S1.5 合併後由主會話處理；e2e 第 2 個在 S3.3 | 
| S1.7 | 完成 | 2026-11-03 | 0.15 天 | 模組＋單元（reward‑claimer 19、channel‑points‑reader 7、stream‑playback‑optimizer 6）由代理先行寫成，主會話統一（fmt／clippy／測試全綠）；e2e 17/17（reward 10、points 4、chat‑blocking 2、playback 1）兩個代理平行移植，合併後全量 e2e 46/46 綠。期間揭錯並修正：`Locator::evaluate` 的 undefined 回值誤判（DEC‑017，採 `await_first`＋`{result: …}` 包法）＋`select_quality` 的 `first()` 作用範圍抄錯（DEC‑018）。strict：S1 檔案 61/61、S2 檔案 114/114、未知 0。債：evaluate／first() 修正尚無單元測試鎖定（DEC‑017 附註） | 7cbaff1、a167a4e、5a92ddb 及兩次 e2e 合併 |
| S1.8 | 完成（macOS 可驗部分） | 2026-10-19 | 0.1 天 | 4 個 e2e（`src/tests/multi_page.rs`）：3 頁併發＋診斷歸屬與截圖隔離、popup 收到即關（`on_popup`→`close()`，opener 不受影響）、`goto("chrome://crash")` 注入 crash（A `on_crash` 恰一次、B／C 不受影響）、SIGKILL 後 `on_disconnected` < 1 秒＋無孤兒；8 個 e2e 重跑 4 次全綠。**G9 的 RSS ≤ 40 MiB 只能在 Docker 量測**（macOS 無 `/proc`；probe `--hold 3` 已實測 exit 0）。待整合（S1.5 合併時一併處理）：crashed 頁 session 需照 Playwright `_markAsCrashed()` 短線路回 `Target crashed `；`PageShared::close()` crashed 時仍要送 `Target.closeTarget` | 
| S1 關卡 | 完成（本機可驗部分） | 2026-11-03 | 0.1 天 | G1 ✔（S1.3：參數逐行、`151.0.7922.34`、close／取消／drop 無孤兒）；G2 ✔ 9/9 seq 相同；G3 ✔ 9/9 diag 相同；G5 ✔ pw‑errors 33/33；G6 ✔（S1.8 e2e）；G7 ✔ 18/18；G8/G9 的 Linux 部分與 G9 RSS 量測留 Docker；**G4【人工】未做**（需使用者用 `npm run twitch:login` 匯出並跑 probe 截圖確認） | — |
| S2.1 | 完成 | 2026-10-19 | 0.3 天 | 消毒層（13）＋ Logger 本體（11）＋ `browser-diagnostics` 純函式（`extract_graph_ql_operation_names`／`decide_*_level`／`truncate_text`，10 個舊測試；另 6 個已由 channel_url 處置）＋ `safe_logging.rs`（舊碼無專屬測試，補測 3 個）。port-coverage strict 35/35。本步驟全部完成 | 5213e0a |
| S2.2 | 完成 | 2026-10-19 | 0.5 天 | `parse_yaml`（13/13 golden）＋`ConfigLoader`（欄位驗證順序／訊息、`${VAR}`、15 個 env 覆寫、X1 browser.engine、X2 headless）＋`model.rs`（f64 忠實 JS number、`to_json()`）＋thresholds（5）＋**9/9 config golden**（fingerprint 留 S2.6）。config.test.ts 24＋11 it.each＋thresholds 5 全移植（X2 檢查放 buildConfig 最後以守住原驗證順序）。寫回屬 S2.3 | 0ead197 |
| S2.3 | 完成 | 2026-10-19 | 0.2 天 | 位元組範圍取代引擎（saphyr Marker 是字元索引非位元組——建字元→位元組對照表；flow/block span、隱式空值、anchor 名稱掃描、CRLF／BOM 保留）＋`YamlRuntimeConfigManager`（Mutex 串行、驗證→persist→記憶體→target）；**22/22 golden**＋4/4 舊測試。`ConfigValidationError` 已統一回 `config::errors`（暫定義從 writeback.rs 移除） | 
| S2.4 | 完成 | 2026-10-19 | 0.1 天 | `FileCredentialValidator`＋`StorageState` 解析＋四個專用錯誤（訊息逐字）；10 個舊測試全移植（port-coverage strict 10/10）；fixture 以 `include_str!` 嵌入記憶體、不碰檔案系統；`js_trim` 照 JS 字元集 | 
| S2.5 | 完成 | 2026-10-19 | 0.3 天 | `channel_url.rs`（18）＋選台器（16）＋復原政策（7/7）＋resource guard 政策（12/12）＋safe-logging 補測（3）全部完成；port-coverage strict 全過。config 形狀的型別（BrowserRecoveryConfig／ResourceGuardPolicyConfig 等）暫在使用端定義，等 S2.2 loader 合併後統一 | 5213e0a |
| S2.6 | 完成 | 2026-10-19 | 0.1 天 | `create_safe_config_fingerprint`（SHA-256 hex、JSON.stringify 逐字重現：鍵序照建立順序、數字用 js_number_to_string）＋`create_runtime_startup_diagnostics`；3 個舊測試全移植（fingerprint 與舊版 TS 實作逐字相同）。golden c01/c02/c03/c05/c06 已加 fingerprint 比對（全相符）；c04/c07 錯誤案例、c08/c09 X1/X2 不比 | 
| S2 關卡 | 完成 | 2026-11-03 | 0.1 天 | `REQUIRE_E2E=1` 等價檢查全過（fmt、clippy `-D warnings`、cargo test 單元全綠、全量 e2e 46/46、npm test 21 passed）＋`port-coverage --strict` S2 十個檔 114/114、未知 0 | — |
| S3.1 | 完成 | 2026-10-20 | 0.2 天 | `src/twitch/{api,live_status,errors}.rs`：Helix client＋transport trait 注入（預設 `ReqwestTransport`：HTTP/1.1、不讀 proxy 環境變數，照舊版 Node fetch；2 個整合測試用本機 ephemeral server）＋token 單飛（watch channel）＋直播狀態解析；**22 個舊測試全移植（port-coverage strict 22/22）**＋7 個直接測試；逾時用 `start_paused` 虛擬時間驗證 | 
| S3.2 | 完成 | 2026-10-20 | 0.3 天 | `app/{cgroup,resource_monitor,restart,stall_watchdog}.rs`：**24 個舊測試全移植（port-coverage strict 24/24）**；cgroup root／時鐘／Docker client／fs 以 DI 注入（`RecoveryClock` 前例），邏輯本體在 macOS 可測、僅平台預設路徑（`/sys/fs/cgroup`、`/proc`）cfg(linux)——Docker test stage 驗證；`CgroupSnapshot`／`CgroupMemoryEvents` 依 DEC-013 從 `resource_guard.rs` 移到 `app/cgroup.rs`（resource_guard 改 re-import）。代理回報亂碼中斷、未 commit——四模組程式碼完整留存，主會話清掉 12 個機械性 clippy 錯誤後收尾 commit（`6f073c7`） | 
| S3.3 | 完成 | 2026-10-20 | 0.3 天 | `browser/types.rs`（D12：trait 對應 `types.ts` 介面；具體型別的 impl 全在新檔，cdp／page_adapter 零改動）＋`browser/manager.rs`（生命週期、重啟退避、generation、診斷分級轉發、writeback `update_config` 接點）；**49 個舊測試全移植（port-coverage strict 49/49）**＋e2e `browser_manager_logs…`（manager 以 channel 與 generation 記錄消毒後診斷）通過；`ExclusiveQueue` 交棒時跳過已取消的等待者（排隊 future 被取消會卡隊列——真 bug 修復）。代理無回報文字但 3 個增量 commit＋主會話收尾 commit 完整留存 | 
| S3.4 | 完成 | 2026-11-03 | 0.2 天 | SideNavCollapser 4/4（先前合併）＋ChannelHealthChecker 5＋ChannelSession 38（另 `明確辨識 %s 頁面` 由 DEC‑020 補抽＋標題轉正規 `// 原測試：`）單元、channel‑session e2e 8/8；合併後 main 全量：單元 745 passed／54 ignored（lib＋bin 各 +44，session 經 DEC‑014 掛載重編）、全量 e2e 54/54、npm test 21、strict 三檔（channel‑session／channel‑health‑checker／spec）51/51、clippy `-D warnings` 零警示。以基準 `d49363d`（DEC‑016）：`CHANNEL_HEALTH_SELECTORS` 對齊、`channel_url` 補四個函式、`ManagedSessionPage` 轉接 cdp；介面取捨與行為特例記 DEC‑020 | wt/s34‑sidenav、wt/s34‑session 合併（4bfbe47） |
| S3.5 | 完成 | 2026-11-04 | 0.2 天 | 前置整併 `a048a2a`（DEC‑013 佔位型別落地＋`displayValue`／`secondsUntilRefresh` 契約收斂，記 DEC‑022）後派兩代理平行：`src/sessions/manager.rs`（76 行 placeholder → 1928 行）＋`src/sessions/reconcile.rs`（514 行）＋`src/scheduler/watchdog.rs`（25 行 placeholder → 1081 行）；**49 個舊測試全移植（strict 三檔 49/49、未知 0）**＋6 個補充／接縫測試；機制對應記 DEC‑023／024（cancel-once、per‑channel 獨佔佇列、操作逾時 detached、coalescing worker、5 秒 stop 逾時、watch flight、Unknown catch-all；4 個逾時事件名待 S3.6 補進 log‑events）。合併後 main 全量：單元 826 passed／54 ignored、e2e 54/54、npm 21、clippy `-D warnings` 零警示 | 18a69c5、25d1c06 合併 df99ded、4e04e82 |
| S3.6 | 未開始 | | | | |
| S3 關卡 | 未開始 | | | | |
| S4.1 | 完成 | 2026-11-03 | | `src/notifications/`：`bot_notifications`（restart／crash 三則訊息文字逐字照舊，含全形標點與 emoji）＋`BotCommandContext` trait（方法照舊版形狀；scheduler／sessions 型別依 DEC‑013／DEC‑019 在使用端先行定義，統一債留 S3.4／S3.5）。由 s43‑discord 代理移植，`check.sh` 綠 | e42ab8f |
| S4.2 | 完成 | 2026-11-03 | | `src/telegram/`：API client（reqwest 只開 h1、無 proxy、拒導向、六分類錯誤逐字、長輪詢合成 deadline 用 `start_paused` 驗證）＋`DefaultTelegramBot`（offset 遞增 poll、退避 `min(30_000, 1000*2^(n-1))`、15 指令與授權、`306b4ba` 的 `MAX_ACTIVE_QUERIES=2` 名額＋30 秒 `telegram_query_timeout`／`telegram_query_failed` 語意逐項照舊）；6＋17 個舊測試全移植（strict 23/23、0 未知）；取捨記 DEC‑021（payload 病態形狀的分類差、`js_number_value` 等）。合併後 main 全量：單元 768 passed／54 ignored、e2e 54/54、npm 21、覆蓋率 367/433（缺 66）、`check.sh` 全綠（e2e＋npm 由主會話補跑） | f121aa0、da7f888 合併（fb01ecc） |
| S4.3 | 完成 | 2026-11-03 | | `src/discord/`：REST client（reqwest 只開 h1、不讀 proxy、10 秒逾時、`redirect error`、multipart photo、錯誤訊息逐字）＋`DefaultDiscordBot`（twilight Shard：identify properties、`Intents::empty()`、無 cache、啟停廣播訊息與 15 個指令逐字照舊）。9 個舊測試全移植（strict discord 9/9、0 未知）＋X4 fake‑shard 等 11 個新測試；twilight 介面取捨記 DEC‑019。合併後 main 全量：657 passed／46 ignored、`check.sh` 綠 | e34dbef |
| S4 關卡 | 完成 | 2026-11-04 | 0.1 天 | S4 步驟合併後的 main tree（含 S3.5）：`REQUIRE_E2E=1` 等價檢查全過（fmt、clippy `-D warnings`、cargo test 單元 826 passed／54 ignored、全量 e2e 54/54、npm test 21）＋`port-coverage --strict` S4 四檔（telegram‑api 6／telegram‑bot 17／discord‑api 2／discord‑bot 7）32/32、未知 0（重複標題警告為 config.test.ts 既有 it.each，test‑port.md 已註記） | 3c821e0 |
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
