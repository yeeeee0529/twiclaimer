# 進度

- 目前步驟：S1.5 Locator（`wt/s15-locator`）＋S1.6 診斷 adapter（`wt/s16-adapter`）兩個子代理並行；S1.4 與 S2 階段（S2.1–S2.6）全部完成
- DEC-013 統一已完成（`c187b42`）：`sessions/recovery.rs` 與 `app/resource_guard.rs` 改用 `config::model`／`config::thresholds`（thresholds 的 bytes 以 u128 對應 JS bigint，使用處經 u128::from 無損轉換）；`CgroupSnapshot`／`CgroupMemoryEvents` 仍在 `app/resource_guard.rs`，等 S3.2 reader 落地時移過去
- 觀察到的既有不穩定：`tests::browser_launch::cancelling_close_kills_a_browser_that_ignores_browser_close`（S1.3 假瀏覽器測試，非 ignore）在全套平行跑時失敗過一次（process group 收屍時序），單跑與後續全跑皆通過；持續觀察，若再現就檢查收屍等待的確定性
- 卡住的項目：無

## 階段

| 階段 | 估計上限 | 停止門檻 | 已用 | 狀態 |
| --- | --- | --- | --- | --- |
| S0 | 0.5 天 + 24 小時 | 0.75 天 | | 未開始 |
| S1 | 5 天 | 7.5 天 | 0.4 天 | 進行中 |
| S2 | 6 天 | 9 天 | | 未開始 |
| S3 | 9 天 | 13.5 天 | | 未開始 |
| S4 | 4 天 | 6 天 | | 未開始 |
| S5 | 3 天 + 24–72 小時 | 4.5 天 | | 未開始 |

## 步驟

| 步驟 | 狀態 | 完成日期 | 耗時 | 驗證結果 | commit |
| --- | --- | --- | --- | --- | --- |
| S0.1–S0.7 | 未開始 | | | | |
| S1.1 | 完成 | 2026-10-19 | 0.2 天 | `scripts/check.sh` 全綠（fmt／clippy／test、npm test 7 個、port-coverage 8 已處置／0 未知）；`extract-spec.sh` 產出 397 列測試標題與 149 個日誌事件 | S1.1 |
| S1.2 | 完成 | 2026-10-19 | 0.2 天 | `pw-errors.jsonl` 33 列；9 組錄製均有 `.seq` 與 `.diag.jsonl`；`chromium-args.txt` 45 個參數含 `--remote-debugging-pipe`；二次錄製 `git diff` 零差異；`src/browser/js/` 6 個頁面內 JS 逐字複製並通過 Node 語法檢查 | S1.2 |
| S1.3 | 完成 | 2026-10-19 | 0.5 天 | transport（6）＋ connection 路由（13）＋ Connection 讀寫接線（10）＋ launcher 程序面機制（16）＝45 個測試；本段完成 launch 握手（`getVersion` → `setAutoAttach`，180 秒整體逾時、Playwright 同款訊息）、`LaunchedBrowser` 的 close／drop／取消／EOF 防線（一律整群 `SIGKILL`、背景收屍）與 disconnected 觸發一次、macOS 的 `dup2` 退路（DEC-007）。83 個單元＋3 個 e2e（真實 headless shell：`get_version()` 為 `151.0.7922.34`、錄製參數逐行比對、close／drop 後無殘留程序與暫存目錄）全過，clippy 乾淨；G8 的 Linux 部分留 Docker test stage | a9f94fc |
| S1.4 | 完成 | 2026-10-19 | 0.5 天 | context（cookies 轉換、localStorage 暫時頁、setDownloadBehavior）＋page 初始化序列＋嵌套 auto-attach（page／iframe OOPIF／worker／popup）＋資源阻擋／聊天室封鎖（init script 雜湊逐位元組）＋`goto`／`screenshot`／`close`＋**probe 子命令**（`#[path]` 掛載，DEC-014）。**G2 通過：9 組錄製 CDP 順序全部相同；diag-diff 診斷也 9 組全部相同（G3 比對工具零差異）**，並以修好的 repo 腳本（bash 3.2 相容）重跑確認。債：page.rs／context.rs／probe.rs 尚無單元測試（S1.6 補 listener 測試時一併補）；G4 的 trace 匯出仍屬【人工】 | 
| S1.5 | 未開始 | | | | |
| S1.6 | 未開始 | | | | |
| S1.7 | 未開始 | | | | |
| S1.8 | 未開始 | | | | |
| S1 關卡 | 未開始 | | | | |
| S2.1 | 完成 | 2026-10-19 | 0.3 天 | 消毒層（13）＋ Logger 本體（11）＋ `browser-diagnostics` 純函式（`extract_graph_ql_operation_names`／`decide_*_level`／`truncate_text`，10 個舊測試；另 6 個已由 channel_url 處置）＋ `safe_logging.rs`（舊碼無專屬測試，補測 3 個）。port-coverage strict 35/35。本步驟全部完成 | 5213e0a |
| S2.2 | 完成 | 2026-10-19 | 0.5 天 | `parse_yaml`（13/13 golden）＋`ConfigLoader`（欄位驗證順序／訊息、`${VAR}`、15 個 env 覆寫、X1 browser.engine、X2 headless）＋`model.rs`（f64 忠實 JS number、`to_json()`）＋thresholds（5）＋**9/9 config golden**（fingerprint 留 S2.6）。config.test.ts 24＋11 it.each＋thresholds 5 全移植（X2 檢查放 buildConfig 最後以守住原驗證順序）。寫回屬 S2.3 | 0ead197 |
| S2.3 | 完成 | 2026-10-19 | 0.2 天 | 位元組範圍取代引擎（saphyr Marker 是字元索引非位元組——建字元→位元組對照表；flow/block span、隱式空值、anchor 名稱掃描、CRLF／BOM 保留）＋`YamlRuntimeConfigManager`（Mutex 串行、驗證→persist→記憶體→target）；**22/22 golden**＋4/4 舊測試。`ConfigValidationError` 已統一回 `config::errors`（暫定義從 writeback.rs 移除） | 
| S2.4 | 完成 | 2026-10-19 | 0.1 天 | `FileCredentialValidator`＋`StorageState` 解析＋四個專用錯誤（訊息逐字）；10 個舊測試全移植（port-coverage strict 10/10）；fixture 以 `include_str!` 嵌入記憶體、不碰檔案系統；`js_trim` 照 JS 字元集 | 
| S2.5 | 完成 | 2026-10-19 | 0.3 天 | `channel_url.rs`（18）＋選台器（16）＋復原政策（7/7）＋resource guard 政策（12/12）＋safe-logging 補測（3）全部完成；port-coverage strict 全過。config 形狀的型別（BrowserRecoveryConfig／ResourceGuardPolicyConfig 等）暫在使用端定義，等 S2.2 loader 合併後統一 | 5213e0a |
| S2.6 | 完成 | 2026-10-19 | 0.1 天 | `create_safe_config_fingerprint`（SHA-256 hex、JSON.stringify 逐字重現：鍵序照建立順序、數字用 js_number_to_string）＋`create_runtime_startup_diagnostics`；3 個舊測試全移植（fingerprint 與舊版 TS 實作逐字相同）。golden c01/c02/c03/c05/c06 已加 fingerprint 比對（全相符）；c04/c07 錯誤案例、c08/c09 X1/X2 不比 | 
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
