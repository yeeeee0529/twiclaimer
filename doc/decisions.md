# 決策紀錄

每筆一個小節，編號遞增。不修改舊紀錄；要推翻時新增一筆，寫明「取代 DEC-00N」。

## DEC-001 本機建置環境：`cc` 被損壞的 swiftly shim 攔截

- 日期：2026-10-19（步驟：S1.1）
- 問題：`PATH` 上第一個 `cc`／`clang` 是 `/Users/liuminkai/.swiftly/bin/cc`（swiftly 的 shim），但它指向未安裝的 Swift 6.2.1 toolchain，導致 cargo 的連結與 aws-lc-sys 的 C 編譯全部失敗。
- 決定：不改動使用者的 swiftly 設定；在本機執行 cargo 時固定 export `CC=/usr/bin/cc` 與 `CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=/usr/bin/cc`（Apple clang 21）。
- 理由：機器層級的環境問題，不進 repo；等使用者修好 swiftly 後這兩個變數即可移除。
- 影響：本機 `scripts/check.sh` 與所有 cargo 呼叫都要帶這兩個變數；Docker（Linux）不受影響。需要使用者知情。

## DEC-002 開發機使用 Node v26.10.0 與略過 Playwright 瀏覽器下載

- 日期：2026-10-19（步驟：S1.1）
- 問題：本機 Node 是 v26.10.0（計畫 §3 指定 Node 24）；`npm install` 時 playwright 套件的 postinstall 會下載整套瀏覽器。
- 決定：以 Node v26 執行（`engines.node >=24` 仍滿足）；`npm install` 加 `PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1`，瀏覽器改由 `scripts/fetch-headless-shell.sh`（CHROME_PATH）與 `$REF` 的 `npx playwright install --only-shell chromium` 提供。
- 理由：Node 工具測試與登入工具不需要安裝流程下載瀏覽器；登入工具本來就在使用者本機擇期執行（D2）。
- 影響：`npm run twitch:login` 前需先 `npx playwright install chromium`；若登入工具行為與 Node 24 有差異再回報。

## DEC-003 `.gitignore` 額外排除 `.DS_Store`

- 日期：2026-10-19（步驟：S1.1）
- 問題：計畫的 `.gitignore` 清單未含 macOS 的 `.DS_Store`，工作目錄已出現此檔。
- 決定：在計畫清單之外加一行 `.DS_Store`。
- 理由：macOS 必然產生，進版控只會造成雜訊。
- 影響：僅本機開發體驗，無行為差異。

## DEC-004 `src/browser/js/` 的載入規則（註解剝除與 trim）

- 日期：2026-10-19（步驟：S1.2）
- 問題：§4.2 要求頁面內 JS 逐字複製到 `src/browser/js/`，又要求「每個檔案開頭註明來源檔案與函式」。`Page.addScriptToEvaluateOnNewDocument` 的 `source` 在 `cdp-seq.py` 以 sha256 遮蔽比對（G2），註解會改變雜湊；`chat-block-init.js` 尤其敏感。
- 決定：每個檔案開頭放 `//` 註解標頭；Rust 端以 `include_str!` 載入後，先剝除開頭連續的 `//` 註解行，再 `trim()` 前後空白，才是送進瀏覽器的確切文字。`chat-block-init.js` 註解後的內容與 `buildChatBlockInitScript()` 的輸出逐位元組相同（不含檔尾換行）。
- 理由：同時滿足可追溯性與 G2 的逐位元組比對。
- 影響：S1.6 實作聊天室封鎖時要照此規則載入；S1.7 的 evaluate 函式同規則（其文字不出現在 seq 比對中，風險較低）。

## DEC-005 `gen-config-golden.mjs` 的案例目錄必須傳絕對路徑

- 日期：2026-10-19（步驟：S1.2）
- 問題：`gen-config-golden.mjs` 在產生設定 golden 前會 `process.chdir(resolve(dir))`（讓 AppConfig 的相對路徑與案例目錄無關）；若 `dir` 傳相對路徑，chdir 之後自身的 `join(dir, …)` 會落在錯誤位置，`writeFileSync` 以 ENOENT 失敗。
- 決定：不改工具（附錄檔案逐字保留），呼叫時一律傳絕對路徑：`node tools/ref/gen-config-golden.mjs "$REF" "$(pwd)/testdata/golden/config"`。
- 理由：工具在沙箱以絕對路徑實測過；相對路徑是未涵蓋的使用方式。
- 影響：S1.2 與日後補產 golden 的指令要記得加 `$(pwd)`；已在計畫指令之外的使用注意事項，寫入本檔供日後查閱。

## DEC-006 `sanitizeBrowserUrl`／`joinSanitizedUrl` 的實際定義位置

- 日期：2026-10-19（步驟：S2.5）
- 問題：計畫 §4.2 將 `browser/channel-url.ts` 對應到 `browser/channel_url.rs`，但 `sanitizeBrowserUrl`／`joinSanitizedUrl` 實際定義在 `src/browser/browser-diagnostics.ts`；`channel-url.ts` 只有 `createChannelUrl` 等函式。
- 決定：消毒函式照實際定義位置（`browser-diagnostics.ts`）移植到 `src/browser/channel_url.rs`（檔名照 §4.2 的對應表），並一併移植其依賴 `classifyHostname`。`EndpointCategory`、`SanitizedEndpoint` 設為 `pub(crate)` 供 S2.1 的 `browser-diagnostics` 純函式重用。
- 理由：以舊版程式碼為行為依據（§0 順位 2），檔案位置照計畫的模組對應表。
- 影響：S2.1 移植 `browser-diagnostics.ts` 其餘函式時直接引用，不重複定義。

## DEC-007 launcher 的 macOS pipe 退路與 `dup2` 同 fd 邊界

- 日期：2026-10-19（步驟：S1.3）
- 問題：
  1. nix 0.31.3 的 `unistd::pipe2` 在 macOS 被 cfg 排除（macOS libc 沒有 `pipe2`），規格文字（`pipe2(O_CLOEXEC)`）照字面寫在 macOS 編不過。
  2. `--remote-debugging-pipe` 需要 `dup2` 到子程序 fd 3／4；當 pipe 的 fd 恰好已是 3（或 4）時，`dup2(3, 3)` 是 no-op 且**不會**清 `FD_CLOEXEC`，exec 後瀏覽器會讀不到指令。
- 決定：
  1. 新增 `create_cloexec_pipe()`：Linux 走 `pipe2(O_CLOEXEC)`（與規格一致）；macOS 退回 `pipe()` + `fcntl(F_SETFD, FD_CLOEXEC)`。macOS 有極小的競態窗口（pipe 建立到 fcntl 之間若 fork 會外洩 fd），僅影響本機開發，正式環境（Linux）不受影響。
  2. `dup2_to_fixed_fd()` 在 oldfd == newfd 時改用 `fcntl(F_SETFD, 0)` 清旗標（async-signal-safe），其餘照常 `dup2`。
- 理由：Docker 正式環境是 Linux，規格語意（O_CLOEXEC、fd 3/4 清 CLOEXEC）在 Linux 完全一致；macOS 只是開發便利。
- 影響：G1／G8 的 e2e 在 Linux 驗證；macOS 上 `spawn_browser` 只保證編譯與 API 正確。Linux 分支本機無法執行，留待 Docker test stage。

## DEC-008 Connection 的寫入佇列、close 語意與瀏覽器 session 事件

- 日期：2026-10-19（步驟：S1.3）
- 問題與決定（三項）：
  1. 寫入佇列用 unbounded mpsc：指令訊息小，bounded 會迫使 `send_command` 在 Router 鎖內 await 背壓，風險大於效益。
  2. `close()` 以 abort 結束讀取 task（先 `fail_all_pending` 再 abort）：讀取 task 可能卡在生命週期事件的 `send().await`，只有 abort 能保證 close 收斂；寫入 task 走 graceful 收尾，之後 `Browser.close` 要靠它送出（§7.3.1）。
  3. 瀏覽器 session（無 `sessionId`）的生命週期事件目前直接丟棄。查 `crBrowser.ts`，`Browser.downloadWillBegin`／`downloadProgress` 在舊版由瀏覽器 session 消費——S1.4 若需要，幫 Router 加瀏覽器 session 管道（須有消費者，否則會堵住讀取迴圈）。
- 理由：守住 G2 的「每個 session 內送出順序」與 D13 的鎖紀律（不在持有鎖時 await 瀏覽器或網路操作——生命週期事件稀少，評估可接受；若 S1.4 實測造成卡頓再改為鎖外派送）。
- 影響：S1.4 接 launcher 時要提供事件消費者；close 的取消語意要在 G1 e2e 驗證（取消、drop 皆不留孤兒）。

## DEC-009 日誌 regex 的引號值邊界差異（潛在）

- 日期：2026-10-19（步驟：S2.1）
- 問題：JS 的 `.` 不吃 `\r`、`\u2028`、`\u2029`；Rust `regex` 的 `.` 只不吃 `\n`。`redactSensitiveString` 的 quoted-value 樣式（`"…"|^'…'` 內的跳脫比對）在「值內含反斜線＋`\r`」的輸入上，兩版比對範圍可能不同。
- 決定：暫不特殊處理（舊測試輸入不含此情境）；維持 §7.7 的轉換規則（`\b`→`(?-u:\b)`、`\s`→明確字元類別、`i`→`(?i)`），四條樣式各有專屬測試（輸入取自舊測試，含 `\u{feff}`／`\u{85}`／`cafétoken=` 三個差異點）。
- 理由：真實 log 輸入出現 `\` 緊接 `\r` 的機率極低；等 R10 的 log-schema 比對發生實際差異再處理。
- 影響：A/B 比較（§10）若出現相關欄位差異，回來查此筆。

## DEC-010 `extract-spec.sh` 補抓跨行的 `it.each` 標題

- 日期：2026-10-19（步驟：S2.5）
- 問題：`test/unit/browser-diagnostics.test.ts:101` 的 endpoint 分類測試用 `it.each([...])('%s 分類為 %s', …)`，案例表跨行，單行的抽標題 regex（`.each\([^)]*\)\(\s*['"\`][^'"\`]+`）抓不到該標題；移植測試以 `// 原測試：<插值標題>` 標註時被 `port-coverage.sh` 判為未知標題（12 個）。
- 決定：
  1. `tools/ref/extract-spec.sh` 補一條 grep，抓 `it.each(…` 續行 `\])\('標題'` 的形式（`^\s*\]\)\(\s*['"\`][^'"\`]+`），併入 `ts-test-titles.tsv`。
  2. 重新產生 TSV（列數因此與計畫的 397 不同，差異記於本筆）。
  3. Rust 端 12 個分類測試的註解統一改為模板標題原文 `%s 分類為 %s`（一個 it.each 拆多個 Rust 測試，每個都標同一行，符合 §2.1）。
- 理由：附錄工具的修改先記錄（§0）；TSV 應涵蓋所有舊測試標題，否則 port-coverage 的嚴格模式無法使用。
- 影響：`ts-test-titles.tsv` 列數變更；port-coverage 的 known 集合擴大。若日後再發現其他多行形式，比照補抓。

## DEC-011 saphyr 與 yaml 套件判定不同的情況（S2.2 YAML 載入）

- 日期：2026-10-19（步驟：S2.2）
- 問題：§7.1 要求以 saphyr-parser 重現 yaml 2.x（舊版依賴）的行為；兩套實作在下列情況判定不同，且 golden 的 `l*.load.yml` 未涵蓋（皆以 node 對舊版 yaml 套件實測確認）：
  1. `%YAML 1.1` 指示詞：yaml 套件會整份改用 1.1 schema（`yes`／`on` 變 bool、`0o17` 變字串）；saphyr 只發事件、無 schema 概念。Rust 版照 §7.1 一律用 core schema，同一份檔案可能因此與舊版不同（`a: yes` → `"yes"` vs `true`）。
  2. `:` 後接 tab（`a:\t1`）：yaml 套件接受（strict 關閉），saphyr 報 ScanError（`':' must be followed by a valid YAML whitespace`）。Rust 版（走 saphyr）回「設定檔格式錯誤：YAML 語法無效」。
  3. 只有 `...`（doc-end）而沒有文件：yaml 套件的 composer 報錯（`Unexpected doc-end without preceding document`）；saphyr 忽略、不發任何文件事件。Rust 版回 null。
  4. 檔案中段的 UTF-8 BOM：yaml 套件在任何位置都丟棄 byte-order-mark token；Rust 版只去開頭 BOM，中段的 U+FEFF 會留在標量文字裡。
  5. 指向不存在 anchor 的 alias：yaml 套件在 toJS 階段丟 ReferenceError（`Unresolved alias (the anchor must be set before the alias): x`），saphyr 在掃描階段就報錯；對外訊息同為「設定檔格式錯誤：YAML 語法無效」，僅觸發時機不同。§7.2 寫回時的錯誤訊息照 yaml 套件原文輸出，不受影響。
- 決定：暫不處理，維持 Rust 版現行行為。
- 理由：設定檔（config.yml）不會出現 %YAML 指示詞、tab 縮排或 doc-end 開頭檔；要改行為前應先在 `tools/ref/make-yaml-cases.sh` 補案例、重產 golden（§7.1）。
- 影響：A/B 比對（§10）若在設定檔出現差異，先回來查此筆。另一個規格解讀：alias 鍵或「解析型別不同但字串化後相同」的鍵（如 `? 1` 與 `? "1"`、兩個 `.nan`），yaml 套件在 toJS 時覆寫原位置、不報錯；§7.1 的「重複的鍵在建立時就報錯」由組合階段的 `uniqueKeys` 檢查（僅比較標量鍵的解析值）實現，Rust 版照 yaml 套件行為覆寫，沒有為此新增第二種重複鍵錯誤。
