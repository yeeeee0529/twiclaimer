# 進度

- 目前狀態：**暫停中（使用者指示）**。已完成步驟：S1.1–S1.6、S1.8、S2.1–S2.6、S3.1–S3.3；未開始：S1.7、S3.4–S3.6、S4、S5。S0.1–S0.7 尚未執行，S1／S2／S3 關卡尚未記錄通過；無執行中的代理
- 測試移植（2026-11-03 以 `scripts/port-coverage.sh` 量測）：舊清單 422 筆（標題去重）已處置 230、缺 192、未知標題 0；缺少的 192 筆全屬 S1.7、S3.4–S3.6、S4
- DEC-013 統一已完成（`c187b42`）：`sessions/recovery.rs` 與 `app/resource_guard.rs` 改用 `config::model`／`config::thresholds`（thresholds 的 bytes 以 u128 對應 JS bigint，使用處經 u128::from 無損轉換）；`CgroupSnapshot`／`CgroupMemoryEvents` 已於 S3.2 移到 `app/cgroup.rs`（`resource_guard.rs` 改為 re-import）
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
| S1 | 5 天 | 7.5 天 | 2.1 | 進行中（S1.7 未開始；其餘完成） |
| S2 | 6 天 | 9 天 | 1.5 | 步驟完成，關卡未記錄 |
| S3 | 9 天 | 13.5 天 | 0.8 | 進行中（S3.1–S3.3 完成；S3.4–S3.6 未開始） |
| S4 | 4 天 | 6 天 | 0 | 未開始 |
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
| S1.7 | 未開始 | | | | |
| S1.8 | 完成（macOS 可驗部分） | 2026-10-19 | 0.1 天 | 4 個 e2e（`src/tests/multi_page.rs`）：3 頁併發＋診斷歸屬與截圖隔離、popup 收到即關（`on_popup`→`close()`，opener 不受影響）、`goto("chrome://crash")` 注入 crash（A `on_crash` 恰一次、B／C 不受影響）、SIGKILL 後 `on_disconnected` < 1 秒＋無孤兒；8 個 e2e 重跑 4 次全綠。**G9 的 RSS ≤ 40 MiB 只能在 Docker 量測**（macOS 無 `/proc`；probe `--hold 3` 已實測 exit 0）。待整合（S1.5 合併時一併處理）：crashed 頁 session 需照 Playwright `_markAsCrashed()` 短線路回 `Target crashed `；`PageShared::close()` crashed 時仍要送 `Target.closeTarget` | 
| S1 關卡 | 未開始 | | | | |
| S2.1 | 完成 | 2026-10-19 | 0.3 天 | 消毒層（13）＋ Logger 本體（11）＋ `browser-diagnostics` 純函式（`extract_graph_ql_operation_names`／`decide_*_level`／`truncate_text`，10 個舊測試；另 6 個已由 channel_url 處置）＋ `safe_logging.rs`（舊碼無專屬測試，補測 3 個）。port-coverage strict 35/35。本步驟全部完成 | 5213e0a |
| S2.2 | 完成 | 2026-10-19 | 0.5 天 | `parse_yaml`（13/13 golden）＋`ConfigLoader`（欄位驗證順序／訊息、`${VAR}`、15 個 env 覆寫、X1 browser.engine、X2 headless）＋`model.rs`（f64 忠實 JS number、`to_json()`）＋thresholds（5）＋**9/9 config golden**（fingerprint 留 S2.6）。config.test.ts 24＋11 it.each＋thresholds 5 全移植（X2 檢查放 buildConfig 最後以守住原驗證順序）。寫回屬 S2.3 | 0ead197 |
| S2.3 | 完成 | 2026-10-19 | 0.2 天 | 位元組範圍取代引擎（saphyr Marker 是字元索引非位元組——建字元→位元組對照表；flow/block span、隱式空值、anchor 名稱掃描、CRLF／BOM 保留）＋`YamlRuntimeConfigManager`（Mutex 串行、驗證→persist→記憶體→target）；**22/22 golden**＋4/4 舊測試。`ConfigValidationError` 已統一回 `config::errors`（暫定義從 writeback.rs 移除） | 
| S2.4 | 完成 | 2026-10-19 | 0.1 天 | `FileCredentialValidator`＋`StorageState` 解析＋四個專用錯誤（訊息逐字）；10 個舊測試全移植（port-coverage strict 10/10）；fixture 以 `include_str!` 嵌入記憶體、不碰檔案系統；`js_trim` 照 JS 字元集 | 
| S2.5 | 完成 | 2026-10-19 | 0.3 天 | `channel_url.rs`（18）＋選台器（16）＋復原政策（7/7）＋resource guard 政策（12/12）＋safe-logging 補測（3）全部完成；port-coverage strict 全過。config 形狀的型別（BrowserRecoveryConfig／ResourceGuardPolicyConfig 等）暫在使用端定義，等 S2.2 loader 合併後統一 | 5213e0a |
| S2.6 | 完成 | 2026-10-19 | 0.1 天 | `create_safe_config_fingerprint`（SHA-256 hex、JSON.stringify 逐字重現：鍵序照建立順序、數字用 js_number_to_string）＋`create_runtime_startup_diagnostics`；3 個舊測試全移植（fingerprint 與舊版 TS 實作逐字相同）。golden c01/c02/c03/c05/c06 已加 fingerprint 比對（全相符）；c04/c07 錯誤案例、c08/c09 X1/X2 不比 | 
| S2 關卡 | 未開始 | | | | |
| S3.1 | 完成 | 2026-10-20 | 0.2 天 | `src/twitch/{api,live_status,errors}.rs`：Helix client＋transport trait 注入（預設 `ReqwestTransport`：HTTP/1.1、不讀 proxy 環境變數，照舊版 Node fetch；2 個整合測試用本機 ephemeral server）＋token 單飛（watch channel）＋直播狀態解析；**22 個舊測試全移植（port-coverage strict 22/22）**＋7 個直接測試；逾時用 `start_paused` 虛擬時間驗證 | 
| S3.2 | 完成 | 2026-10-20 | 0.3 天 | `app/{cgroup,resource_monitor,restart,stall_watchdog}.rs`：**24 個舊測試全移植（port-coverage strict 24/24）**；cgroup root／時鐘／Docker client／fs 以 DI 注入（`RecoveryClock` 前例），邏輯本體在 macOS 可測、僅平台預設路徑（`/sys/fs/cgroup`、`/proc`）cfg(linux)——Docker test stage 驗證；`CgroupSnapshot`／`CgroupMemoryEvents` 依 DEC-013 從 `resource_guard.rs` 移到 `app/cgroup.rs`（resource_guard 改 re-import）。代理回報亂碼中斷、未 commit——四模組程式碼完整留存，主會話清掉 12 個機械性 clippy 錯誤後收尾 commit（`6f073c7`） | 
| S3.3 | 完成 | 2026-10-20 | 0.3 天 | `browser/types.rs`（D12：trait 對應 `types.ts` 介面；具體型別的 impl 全在新檔，cdp／page_adapter 零改動）＋`browser/manager.rs`（生命週期、重啟退避、generation、診斷分級轉發、writeback `update_config` 接點）；**49 個舊測試全移植（port-coverage strict 49/49）**＋e2e `browser_manager_logs…`（manager 以 channel 與 generation 記錄消毒後診斷）通過；`ExclusiveQueue` 交棒時跳過已取消的等待者（排隊 future 被取消會卡隊列——真 bug 修復）。代理無回報文字但 3 個增量 commit＋主會話收尾 commit 完整留存 | 
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
