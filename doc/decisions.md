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

## DEC-011 `extract-spec.sh` 再補抓標題在 `])(` 下一行的 `it.each`

- 日期：2026-10-19（步驟：S2.5）
- 問題：`stream-selector.test.ts:154` 的 `it.each([0, -1, 1.5, Number.NaN, Number.POSITIVE_INFINITY])(` 把標題放在下一行——DEC-010 的 `\])\(` grep 抓不到（該行以 `it.each(` 開頭）。移植的 5 個案例測試只能暫用非標準前綴（port-coverage 抓不到，不影響關卡，但偏離 §2.1）。
- 決定：`extract-spec.sh` 加 awk 狀態機，抓「`])(` 之後的下一行是引號標題」的兩種形式（案例表跨行、案例表同行收尾），併入 TSV；重產 TSV 後把 5 個測試的註解改回標準 `// 原測試：<標題>`。
- 理由：同 DEC-010——TSV 要涵蓋所有舊測試標題，嚴格模式才能使用；移植註解的格式要一致。
- 影響：TSV 列數再增加；日後新形式比照補抓。


## DEC-012 saphyr 與 yaml 套件判定不同的情況（S2.2 YAML 載入）

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


## DEC-013 config 形狀型別在使用端先行定義（S2.1／S2.5）

- 日期：2026-10-19（步驟：S2.1 剩餘＋S2.5 剩餘）
- 問題：`ChannelRecoveryPolicy` 與 `ResourceGuardPolicy` 的介面需要 `BrowserRecoveryConfig`／`BrowserSessionStartConfig`／`ResourceGuardPolicyConfig`／`ResourceGuardEffectiveThresholds`／`CgroupSnapshot`／`CgroupMemoryEvents`，這些型別原屬舊版 `config/AppConfig.ts`、`config/resourceGuardThresholds.ts`、`app/CgroupV2Reader.ts`，移植前尚未落地（S2.2 loader 進行中）。
- 決定：先在 `src/sessions/recovery.rs`／`src/app/resource_guard.rs` 定義同形狀的 `pub(crate)` 型別並附模組文件；S2.2 loader 與 S3.2 CgroupV2Reader 落地後改為重用 config 模組的型別、刪除本處定義。
- 理由：並行分工的檔案邊界（禁止改 `src/config/`）不應阻擋純邏輯模組的移植；同形狀欄位（名稱、型別）與舊版一致，統一時只改 import 與建構處。
- 影響：S2.2 合併時需要一次統一 refactor；合併前 `resource_guard.rs` 內也移植了 `mib_to_bytes`／`bytes_to_mib_number`（thresholds 移植後會重複，統一時刪除）。其他次要差異：`truncate_text` 在 `diagnostics.rs` 與 `channel_url.rs` 各有一份（UTF-16 語意、各測正確，統一時合併）；Rust `trim` 不含 `U+FEFF`（僅影響無測試覆蓋的邊界）；空 `pageCrashBackoffSeconds` 舊版得 NaN、Rust 以 0 秒對應（u64 界域）。

## DEC-014 `probe` 子命令以 bin 內 `#[path]` 掛載模組樹（S1.4）

- 日期：2026-10-19（步驟：S1.4）
- 問題：`twiclaimer probe` 需要 `pub(crate)` 的 cdp／logging 型別，但 bin 與 lib 是兩個 crate；`lib.rs` 在該步驟的禁改清單（無法改 `pub(crate)` 為 `pub`）。
- 決定：在 `src/main.rs` 以 `#[path]` 把 `browser`／`logging` 模組樹掛進 bin crate（同一批原始檔 lib／bin 各編譯一次），S3.6 接線 `AppRunner` 時把 probe 改為呼叫 lib 的公開 API、移除 bin 內掛載。
- 理由：probe 的唯一目的（G2／G3 錄製比對）要與正式碼共用同一份實作；`#[path]` 是暫行手段，不改變 lib 的 API 形狀。
- 影響：bin 編譯時間變長；若 lib 的模組可見性後續放寬（S3），此段跟著移除。
- 另：`scripts/compare-traces.sh` 原用 `"$name：…"`，macOS 內建 bash 3.2 解析多 byte 變數名會誤報 unbound variable，已改 `${name}：…`（bash 3.2 相容寫法）。

## DEC-015 `StreamPlaybackOptimizer` 的 force click 差異（S1.7）

- 日期：2026-11-03（步驟：S1.7）
- 問題：舊版在 quality menu 與 quality option 使用 `click({ force: true })`，會跳過部分 actionability checks；目前 Rust `Locator::click` 的 API 只有 `timeout`，沒有 force 參數。
- 決定：先用 regular `click(Some(CONTROL_WAIT_TIMEOUT_MS))`。cdp 層的 `pointer.rs` 已實作 Playwright click 的重試、element state、捲動、動作點與 hit-target 攔截機制；先不為 force 參數擴充 locator API。若 e2e 顯示選單被覆蓋或動畫中點不到，再回頭評估 force click API。
- 理由：保持 locator API 最小面積，並避免在現階段重複 cdp pointer 層已涵蓋的邏輯。
- 影響：極端 UI 狀態下，quality menu／option 點擊可能比舊版更容易逾時；錯誤仍只記 `stream_quality_skipped`，不影響觀看。
- 另：bin 的暫行掛載新增只含 `config::model` 的 `config` 模組（不掛整個 config），
  因為 S1.7 的 browser 模組需要該型別；S3.6 接線後與 browser／logging 一起移除。

## DEC-016 移植基準更新：納入舊版 `306b4ba`、`d49363d` 兩個 commit（取代 §0 的基準設定）

- 日期：2026-11-03（恢復移植時）
- 問題：計畫 §0 定基準為舊版 `e0e079a`；其後舊版另有 `306b4ba`（Telegram 查詢停頓時保持回應）與 `d49363d`（登入健康檢查失敗後退避：ChannelHealthChecker／ChannelSession／SessionManager），合計新增 11 個 test 宣告（10 個唯一標題），且 `doc/spec/ts-test-titles.tsv` 是從 `e0e079a` 抽出的。尚未移植的步驟（S3.4 的 ChannelSession／ChannelHealthChecker、S3.5、S4.2）都以舊版原始檔為行為依據，會直接踩到新版行為；S1.7 的四個頁面操作模組、SideNavCollapser、Discord 與 notifications 不受這兩個 commit 影響（對照過 commit 清單）。
- 決定（使用者裁示 2026-11-03）：**兩個 commit 都納入**。尚未移植模組（S3.4 剩餘、S3.5、S4.2）以舊版 `d49363d`（HEAD）為行為依據；重跑 `tools/ref/extract-spec.sh`（433 標題、432 唯一、log-events 149→152，新增 `channel_health_recovery_backoff`／`telegram_query_failed`／`telegram_query_timeout`）。已完成的移植不需要回頭改（對照過兩個 commit 沒有動到已移植模組的原始檔）；已完成的單元測試綠燈也不受影響。若後續舊版再出現新 commit，沿用同一辦法：重抽清單、記 decisions、以 HEAD 為行為依據。
- 理由：R7／A／B 的對照組是「正在跑的舊版」；停在舊基準會移植到退化的行為，S5 之後還要再補一次雙重工作。
- 影響：S3.4／S3.5／S4.2 的關卡要再一次 `port-coverage --strict`（新標題）；log-events.txt 新增 3 個事件名稱，§7.7「事件名稱只能使用 log-events.txt」以更新後清單為準。`$OLD` 讀取時以 working tree 現況（`d49363d`）為準，不要 checkout。

## DEC-017 `Locator::evaluate` 的 undefined 回值不再視為「沒有元素」（S1.7 e2e 揭露後修正 cdp 層）

- 日期：2026-11-03（步驟：S1.7 e2e）
- 問題：移植 reward-claimer e2e 時發現 `src/browser/cdp/locator.rs` 的 `evaluate` 把使用者函式回傳 `undefined` 誤判為「元素不存在」而輪詢至逾時（舊 e2e 中「移除屬性／改 class／動態補按鈕／pointerEvents='none'」四個案例原本都會以 `locator.evaluate: Timeout 1000ms exceeded.` 失敗）。
- 決定：依 Playwright 1.62.1 實際行為修正（`$OLD/node_modules/playwright-core` bundle 的 `locator.ts`／`frames.ts`：client 端 `_withElement` 先輪詢到元素 attach，元素存在後就直接 `parseResult` 回傳，不再輪詢）。實作改為 `EVALUATE_ELEMENT_SOURCE` 回 `{ elementFound, result }`，Rust 端僅在 `elementFound === false` 時輪詢。找不到元素的逾時訊息（`locator.evaluate: Timeout Nms exceeded.`）與 `doc/spec/pw-errors.jsonl` 的 `locator-evaluate-missing` 案例維持不變，既有的 errors e2e 與 S1.5／S1.6／S1.8／S3.3 的 e2e 全部照綠。
- 理由：§7.4 的「頁面內邏輯直接使用 Playwright 的 injected script」的精神，就是以 Playwright 實際運作為依據；「未找到元素」與「函式回傳 undefined」在原始語意中不同。
- 影響：`src/browser/cdp/locator.rs`；本文件與計畫 §7.4 表格「等到元素存在；strict」的語意補充——之後 evaluate-with-element 一類 API 也要遵守同一語意。
- 取捨說明：兩個並行代理各提出一種修法（輪詢改在 Rust 端顯式判斷 `elementFound`，或沿用本檔既有 `await_first`＋`{ result: … }` 包裝）。採後者：與 S1.5 既有 textContent／getAttribute／dispatchEvent 的 source 形狀一致（Playwright frames.ts 對 source 就是 `{ result: … }` 包法），程式量最小。合併後限定樹跑了全量單元與全量 e2e（46 個）全綠。
- 債：兩個修復目前只有 e2e 涵蓋；`src/browser/cdp/locator/tests.rs` 與 `stream_playback_optimizer/tests.rs` 的單元測試尚未鎖住 evaluate－undefined 與 `first()` 組合行為，之後補。

## DEC-018 `StreamPlaybackOptimizer` 的 `first()` 作用範圍抄錯（S1.7 e2e 揭露後修正）

- 日期：2026-11-03（步驟：S1.7 e2e）
- 問題：移植 e2e 後發現 `stream_playback_optimizer.rs` 把 `.first()` 直接組進 options locator 的 selector（`a >> nth=0`），導致 `evaluateAll`／`nth` 全部被限制在第一個元素；舊版 `first()` 只用於可見性檢查。實測會在 30 秒內穩定走到 `supported_quality_missing`（第一個選項通常是 Auto，使用者目標畫質永遠「不存在」），正式環境的畫質調整會靜默失效；原單元測試的 fake locator `first()` 不做 selector 組合，抓不到。
- 決定：照舊版原始檔還原——`first()` 只包可見性檢查用的 locator，`evaluateAll`／`nth` 用完整 options locator（+2−1 行）。
- 理由：§0 行為依據順位 2（舊版程式碼）；e2e 是唯一能在真 CDP 層抓到該組合差異的地方。
- 影響：`src/browser/stream_playback_optimizer.rs`；playback-optimizer e2e 1/1 綠，單元 6/6 綠。

## DEC-020 S3.4 主體的介面對應、舊版行為特例與 extract-spec 補抓補漏（ChannelSession／ChannelHealthChecker）

- 日期：2026-11-03（步驟：S3.4 主體；s34-session 代理移植、主會話合併並記錄）
- 問題：ChannelSession／ChannelHealthChecker 以舊版 `d49363d` 為基準（DEC-016），移植時遇到（1）依賴界面的落地方式、（2）測試標題與日誌事件清單在本輪重抽後仍有兩個形狀抓不到、（3）數處舊版行為特例需要在決策落實。
- 決定：
  1. session 依賴的模組都以窄介面接：`SessionBrowserManager`（僅 `createPage`／`closePage`）、`SessionRewardClaimer`、`SessionStreamPlaybackOptimizer`（舊版「可拋錯」以 `Err` 表達；`hasClaimableBonus` 沿用 reward 模組對錯誤折為 `false` 的實作）、`ChannelHealthEvaluator`（舊 `(page, url) => Promise` 的 trait 化，呼叫端 catch 折為 `health_check_error` 路徑由 Result 錯誤表達）、`SessionPage`／`SessionLocator`（`BrowserPage` 型別缺少 url／goto／evaluate。正式端 `ManagedSessionBrowserManager`／`ManagedSessionPage` 轉接既有 manager／cdp `PageShared`，測試端直接實作 fake（對應舊測試 fake））
  2. 舊 JS 機制對應：flight promise → `FlightCell<T>`（tokio watch；`T: Clone`，故 `RewardClaimResult` 加 `Clone`）；`now: () => Date` → `SessionClock = Arc<dyn Fn() -> u64>`（epoch 毫秒，照 `ManagerClock` 前例）；`ChannelHealthReason` TS union → superset enum（含 `live`）；`lifecycleTail`／`pageOperationTail` 以 manager 的 `ExclusiveQueue` 重現
  3. 清單補抽補漏並修正 `tools/ref/extract-spec.sh`：`明確辨識 %s 頁面`（it.each 案例表以 `] as const)('…'` 同行收尾）與 `session_page_operation_drain_timeout`／`service_start_failed`（層級字串單獨一行的多行 safeLog／事件即三元式字面）原先都抓不到。補兩個抓取形狀：`as const)(` 標題型、多行呼叫的「層級行下一行取最後一個單引號事件名」（事件經 `LOG_EVENTS.*` identifier 的自然由既有 LOG_EVENTS 抓取涵蓋）。重抽後：TSV 433→434 列（唯一 433）、log-events 152→154；代理對 `明確辨識 %s 頁面` 的暫行註解改回標準 `// 原測試：` 標題。覆蓋率主會話量測：344／433 已處置、缺 89、未知 0
  4. 舊版行為特例照舊保留並記錄：`start()` 的 guard 把 `failed` 狀態列為 `return`（session 在 failed 後無法重啟，僅 stop 後重建；照舊碼）；`page_health_timer_failed`／`reward_timer_failed` 是舊 timer callback catch 分支的記點，Rust 版對應回呼不會 reject（不可達），事件名照舊保留、無呼叫點；`refreshNow` 手動路徑照舊把錯誤外拋（不記 `page_refresh_failed`）；`session_maintenance_completed` 的 `queuedMs`／`durationMs` 以 `std::time::Instant` 對應舊 `performance.now()` 差值（不會跟隨 paused 模擬時鐘，與舊測試的「任意數字」斷言相容）；正式 `ManagedSessionPage` 對點數讀取契約只回 `balance`（省略 `displayValue`，舊測試斷言對應為 `Available{balance:3500}`）
  5. `CHANNEL_HEALTH_SELECTORS` 對齊 `d49363d`：`login_required` 移除登入按鈕、新增 `logged_out`，頁內 `channel-health-snapshot.js` 加 `loggedOut`（逐字照 `$OLD` dist）——DEC-016 基準的一致套用，非行為分歧
- 理由：§0 行為依據順位（舊版原始檔優先）；清單工具的補抓是 DEC-016「以 HEAD 為行為依據、重抽清單」程序的健全化，不是新行為
- 理由：§0 行為依據順位（舊版原始檔優先）；清單工具的補抓是 DEC-016「以 HEAD 為行為依據、重抽清單」程序的健全化，不是新行為
- 影響：`src/browser/{health,session}.rs`＋測試、`src/browser/channel_url.rs`（補 channel_pattern／create_channel_url／is_expected_channel_url／stable_jitter 逐字搬）、`src/tests/channel_session.rs`、`tools/ref/extract-spec.sh`、`doc/spec/*`。合併後全套：單元 745 passed／54 ignored（lib＋bin 各 +44——session 模組經 DEC-014 `#[path]` 掛載重跑）、e2e 全量 54/54、npm 21、strict（channel-session＋channel-health-checker＋e2e spec）51/51

## DEC-021 Telegram 的介面對應與 `306b4ba` 快查名額語意（S4.2）

- 日期：2026-11-03（步驟：S4.2；s42-telegram 代理移植、主會話合併並記錄）
- 問題：Telegram 以基準 `d49363d`（含 `306b4ba` 的查詢停頓保持回應）移植；Node 的 AbortSignal／fetch 注入與 JSON 型別層在 Rust 下的對應需要取捨。
- 決定：
  1. **typed result 解析差異（僅病態回應）**：舊版 `call<T>` 原樣回傳 `result`（形狀不對讓 bot 端爆 TypeError，poll catch 收 `errorClass:'Error'`）；Rust 在 `getUpdates` 把 result 解析成 `Vec<TelegramUpdate>`，失敗以 `invalid_response`（無 status）丟出 → poll 走 `telegram_poll_failed`（`TelegramApiError`）＋重試。流程等價，遠端回傳怪形狀時的 log 分類不同（`telegram_poll_failed` vs `telegram_command_failed`）
  2. **`chat.id` 拒收**：`number | string` 以外的值舊版 `String(chat.id)` 顯示 `[object Object]`，Rust 解析端拒收（`invalid_response`）
  3. **deadline／訊號分層**：`AbortSignal.any([signal, AbortSignal.timeout(秒數＋10s)])` → reqwest `.timeout()`（映射 `timeout` 分類、訊息 `'Telegram request timed out'`）＋caller 訊號的 `select!`（`aborted`）；`maybe_cancel` 於任務失敗且訊號已中止時偏置 abort（照舊 catch 先看 `signal.aborted`）
  4. **請求 body 數字格式**：`JSON.stringify(25)`＝`25`，serde_json 的 f64 會輸出 `25.0`——api.rs 新增 `js_number_value`（ryu-js，格式同 `js_number_to_string`）用於 `offset`／`timeout`，測試對 body 逐位元組斷言鎖住；`js_number_value` 與 `SanitizedValue::Number` 的統一是債
  5. **generic Error 測試 seam**：bot 測試的 `new Error('…')` 以 `TelegramApiError::generic`（`error_class:'Error'`）呈現，讓 `telegram_command_failed` 的逐欄位斷言成立；正式碼不產生此值
  6. **截斷以 Unicode 純量近似 UTF-16**：`text.slice(0,4096)`／`caption.slice(0,1024)`——同 DEC-019 `content.slice(0,2000)` 同類取捨
  7. **`toLocaleLowerCase('en-US')` → `to_lowercase`**（頻道比對、指令 parse）——DEC-009／DEC-019 同類
  8. **`update_id`／`message_id`／`balance`／`maxConcurrentStreams` 皆 f64**（照 DEC-013 的 JS number 形狀）
  9. **`306b4ba` 快查名額語意逐項照舊**：名額 `MAX_ACTIVE_QUERIES=2` 在底層頁面工作結束前保留；30 秒為「finally 的 clearTimeout」與「shutdown abort listener」兩來源解除——移植為 `shutdown.child_token()` 的 deadline token；逾時回覆一旦開始送出不再被取消（照舊）；逾時／失敗的 callback 次序（abort → `telegram_query_timeout {command}` → 未 await 的逾時回覆→ 送訊失敗記 `telegram_send_failed {retryInMs:0}`）與 `throwIfAborted()` 檢查點（points 查詢後、screenshot 各頻道前後）照舊
- 理由：§0 行為依據順位（舊版原始檔優先）；以上差異僅出現於真實 Discord／Telegram payload 不會產生的形狀
- 影響：`src/telegram/{api,bot}.rs`＋測試、`src/lib.rs`（一行註冊）；log 事件全取自 log-events.txt（154 版）。S3.6 接線備註：`DefaultTelegramBotOptions` 需要 `AppConfig`（`channels`／`max_concurrent_streams`／`telegram.polling_timeout_seconds`）＋`Arc<TelegramApiClient>`＋`Arc<dyn BotCommandContext>`；integration stop 的 reason 沿用 `startup_failed:<phase>` 慣例（bot 對未啟動的 stop 為 no-op，照舊）。合併後 main 全量：單元 768 passed／54 ignored、e2e 54/54、npm 21、strict telegram 23/23、覆蓋率 367／433（缺 66）

## DEC-019 Discord／twilight 介面與測試替身的差異（S4.1／S4.3）

- 日期：2026-11-03（步驟：S4.1／S4.3；由 s43-discord 代理移植、主會話記錄）
- 問題：舊版 `DiscordBot` 直接用 ws（自組 identify op 框、heartbeat 由 `DiscordBotTimer` 接縫注入、`isDiscordInteraction` 驗 payload 形狀），HTTP 用 fetch 注入以便測試；Rust 改以 twilight crate 的 `Shard` 啟動 gateway，介面形狀與舊版不完全相同。
- 決定：
  1. gateway 事件以 twilight typed 事件餵 bot（`next_event(EventTypeFlags::INTERACTION_CREATE)`）。畸形 payload 在舊版由 `isDiscordInteraction` 靜默丟棄，Rust 版會變成 twilight 的 Deserializing 錯誤——記 `discord_gateway_error` 後繼續收事件；欄位錯型（如 `guild_id`）也不再照舊版進 log。僅此一差。
  2. 授權清單比對以 twilight `Id.get().to_string()` 對 config 字串集合（與舊版字串 set 語意在數值等同寫法上等價，差異僅前導零）。真實 payload 本就帶 twilight 必填欄位（`application_id`、`authorizing_integration_owners`、`entitlements`、`type` 等），測試 fixture 補實（interaction id 由 `interaction-2` 改為數字 `2`）。
  3. 舊測試對 identify `{op:2…}`／heartbeat `{op:1,d:null}` 的 op 框斷言，由 `build_shard_config` 的設定值驗證取代（identify properties `twitch-watchdog`／`twitch-watchdog`／`std::env::consts::OS`、`Intents::empty()`、不開 cache）；`DiscordBotTimer` 接縫移除，heartbeat／resume／重連由 twilight 內部處理。斷線行為（X4 fake shard：斷→`discord_gateway_error`→持續收指令）保留同語意。
  4. 逾時分類：舊版 `AbortSignal.timeout` 的 rejection 是 TimeoutError 而非 `DiscordApiError`；Rust 版以同一 enum 的 `Timeout` kind 呈現。無消費端觀察訊息，行為等價。
  5. 錯誤模型：context 的鬆散 promise reject 以 `BotCommandError { message }` 表達；config 寫回錯誤以 `ConfigWriteError`（`name()` 對映舊版 `Error.name`）。
  6. `get_gateway_bot` 回傳 `url: Option<String>`（舊版不驗 schema）；實際連線 URL 改用 twilight 預設值（§7.8 的 gateway 條目）。
- 理由：twilight 是計畫已選定的 gateway 實作（Cargo.toml 既有依賴）；上述差異都能對回舊版在「真實 Discord payload」下的等價行為，僅 malformed payload 的 log 有差。
- 影響：`src/discord/{api,bot}.rs`、`src/notifications/bot_command_context.rs`。其他已知取捨：twilight 內部兩處 panic 路徑（`ConfigBuilder::new` 的 TLS 載入、`InMemoryQueue::default` 需 runtime）非我方控制，start 在 runtime 內執行不受影響；`format_balance` 對 ≥1e21 照 `toString` 指數形（舊版展開十進位；點數不會到該量級，程式內已註明）；`content.slice(0,2000)` 以 Unicode 純量近似 UTF-16（僅補充字元的邊界情況理論上有差異）。DEC-013 的「使用端先行定義形狀型別」延伸至 notifications 的指令上下文型別（`WatchdogSchedulerSnapshot`／`StreamStatusChange`／`ChannelSessionRefreshStatus`／`SessionRefreshResult`／`SessionScreenshot`／`SessionChannelPointsResult` 等），統一債留到 S3.4／S3.5 落地後一併清。

## DEC-022 S3.5 前置整併：DEC-013 佔位型別落地 / 舊 import 路徑保持 / 契約收斂（S3.5）

- 日期：2026-11-04（步驟：S3.5 前置；主會話直接處理）
- 問題：S3.5 派兩個代理平行移植 `sessions/manager.rs` 與 `scheduler/watchdog.rs`，而 DEC-019 把 scheduler／sessions／notifications 的介面形狀型別全暫放在 `notifications/bot_command_context.rs`（DEC-013 的「使用端先行定義」），兩代理會交叉修改同一檔；同時 DEC-020 定案的兩個型別收斂（`displayValue` 省略、`secondsUntilRefresh` 維度）尚未套用到該檔的佔位形。
- 決定（commit `a048a2a`，派單前落地）：
  1. 共用型別移進正式家位置：`WatchdogSchedulerSnapshot`／`SchedulerChannelStatus`／`StreamStatusChange` → `src/scheduler/watchdog.rs`；SessionManager 查詢結果型別（`SessionRefreshResult`／`SessionScreenshot`／`SessionChannelPointsResult`／`ChannelSessionRefreshStatus` 等）→ `src/sessions/manager.rs`；`ChannelSessionRefreshStatus` 的正式型別以 `src/browser/session.rs` 為準。
  2. `src/notifications/bot_command_context.rs` 改為 `pub(crate) use` 重匯出（5 條），bot／discord／telegram 的既有 import 路徑零改動。`#![allow(dead_code)]`、import 的 `#[allow(unused_imports)]` 留待 S3.6 接線後收。
  3. 契約收斂：`SessionChannelPointsResult::Available` 移除 `display_value` 欄位（DEC-020 的「點數讀取契約只回 `balance`」；舊版唯一形同 `balance.toLocaleString('en-US')` 的消費端不讀本欄）；`ChannelSessionRefreshStatus::seconds_until_refresh` 以 `u64` 維度統一，bots 呼叫端 `format_duration((seconds) as f64)`、discord／telegram 測試字面值同步改。
- 理由：型別搬家不改行為，`pub(crate) use` 是保持測試與模組邊界的最小滲透接縫；先整併再派單，兩個代理的檔案邊界才乾淨。
- 影響：`src/{notifications/bot_command_context,discord/bot,telegram/bot}.rs`（含測試）9 檔 +138/−112；DEC-019 結尾「統一債留 S3.4／S3.5」就此清完。port-coverage 不受此 commit 影響（動到的模組無舊測試直射對應）。

## DEC-023 SessionManager 的機制對應與介面取捨（S3.5）

- 日期：2026-11-04（步驟：S3.5；s35‑manager 代理移植、主會話合併並記錄）
- 問題：舊版 `sessions/SessionManager.ts`（845 行）以承諾驅動：WeakSet 的 cancel-once、`runChannelExclusive` 的 map 尾部修剪、`withSessionOperationTimeout`（逾時不取消任務只丟結果）、插入序 Map／Set 迭代序與舊測試逐字日誌，在 Rust（`ChannelSession` 已於 S3.4 落地）下需要機制對應。
- 決定：
  1. **cancel-once（WeakSet）**：以 `Mutex<Vec<Weak<dyn ChannelSession>>>` 呈現——登記前先清已結束的弱參照；live 弱參照以 `std::ptr::addr_eq`（僅 strong_count>0 參與比對；位址相等即同一 session，不比 wide-pointer metadata）；標記在 `cancel_start` 之前與舊碼 `add` 後才呼叫一致
  2. **runChannelExclusive**：每頻道一個 `ExclusiveQueue`（重用 `browser::manager` 的 pub(crate) 版本）＋登記序號 generation：排隊佔位在呼叫當下同步完成（對映 promise 當下接上尾巴）；operation 完成後僅當「Arc 相同且 generation 仍為本任務」才刪 map 項（`get(channel) === tail` 的等價條件）
  3. **withSessionOperationTimeout**：`tokio::spawn`＋`select!`（JoinHandle vs `sleep(timeout_ms)`）；逾時＝JoinHandle drop 後 detached（對映 `operation.catch(() => undefined)`：不取消、結果丟棄），逾時 log 欄位 `{…fields, timeoutMs}`（`timeoutMs` 恆最後）並回 `SessionOperationTimeoutError { event, timeout_ms }`（`Display` = ``"${event} after ${timeoutMs}ms"``，safeErrorMessage 消毒同構）
  4. **SanitizedValue 非 Send**：逾時欄位以 `FnOnce() -> Vec<(&'static str, SanitizedValue)>` closure 傳入、逾時 arm 內才組值（`FatalRecoveryFieldValue` 同款取捨）
  5. **插入序 Map／Set**：`sessions`／`startingSessions`／`configuredChannels`／`stableRecoveryGenerations` 以插入序 `Vec`（不新增 IndexMap 依賴）保住 JS Map 迭代序；`reorderSessions` 照 desired 順序重建
  6. **stopAll 快照時點**：sessions 快照＋清空 → startingSessions 快照，cancellation future 以「呼叫當下的快照」組出（sync 前段語意；曾修掉「輪詢時才抓快照」的偏差版）；captions／stops／pendingStarts 以 `join_all` 併跑，整體包在吞錯的 `session_stop_all_timeout` 內
  7. **cancelPendingStarts 直接 `cancel_start`（不排頻道尾巴）**照舊；stopAll 對起始中頻道的 pendingStarts 空操作才排隊——「cancelStart 不解鎖時 stopAll 靠 timeout 完成」測試賴此序列化
  8. `durationMs` 以 `std::time::Instant` 差值（DEC-020 (4) 前例）
  9. **regex（§7.7）**：`(?i)` 前綴；`storage[\s_-]?` 的 `\s` 逐字展開為 plan §7.7 字元類別（`redact.rs` 的 `JS_WHITESPACE_CLASS` 為 private，本檔以同內容常數複製並註出處）；8 條 recoverable＋3 條 fatal 全有單元輸入（含 `導向非預期 Twitch URL`、`Browser Manager 尚未啟動`、`storage_state`／`storage-state`）
  10. **getChannelPoints 的 catch 在 Rust 不可達**（trait 不回錯、DEC-020 (4)），不設 unreachable catch；`SessionChannelPointsUnavailableReason::Failed` 變體照舊保留於橋接並以字面值測試鎖定
- 理由：§0 行為依據順位（舊版原始檔優先）；每項機制都能對回「真實 runtime（tokio runtime 不重入單一 thread 調度、單一 channel 佇列）」下的等價行為。
- 影響：`src/sessions/manager.rs`（76 行 placeholder → 1928 行）＋`src/sessions/manager/tests.rs`（2322 行，38 個測試＝34 個舊測試移植＋4 個補充：regex 分支／timeout error 形狀／reason 字面值／cancelPendingStarts shutdown 旗標）；`SessionChannelSessionFactory` 的 real-factory impl 放在本檔（`src/browser/session.rs` 零改動）。合併後 main 全量：單元 826 passed／54 ignored（+58；SessionManager 部分 38）、e2e 54/54、npm 21、strict 三檔 49/49、clippy `-D warnings` 零警示。已知差異：**4 個逾時事件名不在 `doc/spec/log-events.txt`**（`session_start_timeout`／`session_start_cancel_timeout`／`session_stop_timeout`／`session_stop_all_timeout`——舊碼以變數轉發，extract-spec 抓不到），照舊版輸出且逐字有測試；S3.6 重抽 log-events 時補上。接線備註：工廠要組 `DefaultChannelSessionFactory`（browser manager／reward claimer／playback optimizer／health evaluator／三 observer／config），recovery 以 `Arc<Mutex<ChannelRecoveryPolicy>>` 注入（方法 `&mut self`、僅同步區上鎖），`on_invalidated` 回呼轉接 `invalidate(channel, reason)`（reason 字串原樣），`on_navigation_outcomes` 接 `reportNavigationOutcomes` 一路；接線後收掉本檔 `#![allow(dead_code)]`。

## DEC-024 ReconcileCoordinator／WatchdogScheduler 的 promise 機制對應（S3.5）

- 日期：2026-11-04（步驟：S3.5；s35‑coord 代理移植、主會話合併並記錄）
- 問題：舊版 `sessions/ReconcileCoordinator.ts`（coalescing worker、`Promise.race` 的 5 秒 stop）與 `scheduler/WatchdogScheduler.ts`（interval＋rate-limit 窗口、`previousLiveStatuses` 轉換、`handleApiError` catch-all）的 promise／timer 機制在 Rust 的等價重現與介面對應。
- 決定：
  1. **`SchedulerTimer`**：`set_interval(Arc<dyn Fn() + Send + Sync>, f64) -> u64`／`clear_interval(u64)`——把手 opaque 為 `u64`、interval 以 `f64` 承接 JS number（fake assert 精確 45_000.0）；`DefaultSchedulerTimer` 為 tokio spawn 睡眠迴圈＋abort（小數毫秒 floor＋下夾 1ms：Node 截斷小數＋0ms 防 busy-loop，設定驗證保證為正）
  2. **flight promise → watch 信號**：私有 `FlightSignal`（coordinator）／`TickFlight`（scheduler）多等待者（比 `JoinHandle` 多端可等）；完成信號在 finally 段（slot 清除＋可能重啟 worker）**之後**發，對映 promise resolve 順序
  3. **stop 的 5 秒逾時**：`tokio::time::timeout(join!(flight_wait, cancel_pending_starts))`——逾時非錯誤、不清 worker flight（舊 `clearTimeout` 語意）；逾時 drop 的只是「等待的未來」，worker 的 spawn 任務照舊收尾（與 browser manager 對逾時 drop 的註記同款取捨），以 `start_paused`＋`advance` 可測
  4. **錯誤型別**：coordinator 窄介面回 `Result<(), ReconcileError>`（`{ message }`，記點時 redact）——`reconcile_completed {outcome:'failed'}`／`reconcile_submit_failed` 可達；scheduler 側 session seam 重用同型別
  5. **`cancelPendingStarts?.()` 選用性**：`cancel_pending_starts` 以預設 no-op 本體呈現（`Partial<Pick<…>>` 的鏡像，正式 SessionManager 兩者都實作）
  6. **`runOnce` 回傳 `()`**：舊 rejection 來源只有可拋錯的注入接縫（selector／logger 在 Rust 為無失敗型別），`scheduler_tick_failed {reason:'unexpected_error'}` 分支不可達——事件名照 DEC-020 前例保留、無呼叫點；`durationMs` 以 `Instant`（同前例）
  7. **API 錯誤 catch-all**：舊 `handleApiError` 收「非三類錯誤」；Rust twitch 介面把錯誤定型為三類，scheduler 接縫補 `WatchdogLiveStatusError::Unknown(String)`（blanket impl 從任何 `LiveStatusProvider` 自動轉接三類）——保住 `twitch_api_request_failed {kind:'unknown'}` 與 secret-redaction 測試的可移植；`Unknown` 訊息不進日誌（舊分支行為）
  8. **通知語意**：`notify_status_change` 以 `tokio::spawn` 承接（queued-not-awaited，tick 不等）；observer 介面無失敗 → 舊 promise-rejection warn（`stream_status_notification_failed`）不可達、事件名照前例保留
  9. **ISO 輔助**：`iso_utc_string_from_epoch_ms` 為模組內副本（與 twitch／logging 同一組 `time` 靜態格式、各模組獨立定義維持邊界——§7.7），含常數↔字串互測
  10. **fire-and-forget 提交**（無 coordinator 的 `submitReconcile`）：`tokio::spawn`＋錯誤捕獲記 `reconcile_submit_failed {error}`；單元測試的同步前綴（舊 `vi` mock 靠 promise）改以 watch 通知驅動不等真實時間
- 理由：§0 行為依據順位（舊版原始檔優先）；每項對應都能攤平到「真實 runtime」的等價行為，僅記點層 retaining 舊事件名（照既有不可達記點前例）。
- 影響：`src/sessions/reconcile.rs`（514 行；2 個舊測試移植，paused-time shutdown）＋`src/scheduler/watchdog.rs`（25 行 placeholder → 1081 行；`it.each` 模板 2 個展開為 3＋2 案例，合計 16 個舊測試移植＋2 個接縫測試）＋`{sessions,scheduler}/mod.rs` 註冊。strict 三檔 49/49 未知 0（watchdog 13 個標題與 TSV 第 422–434 列逐字一致、reconcile 2 個第 265–266 列一致）。S3.6 接線備註：`TwitchApiClient` 經 blanket impl 直接餵 `SchedulerLiveStatusProvider`（傳具體型別的 `Arc<…>`）；SessionManager 需同時實作 `ReconcileSessionManager`／`SchedulerSessionReconcile`；coordinator 以 `Arc<dyn ReconcileCoordinator>` 持有（`SchedulerReconcileRequest` 已備 trait 物件轉接）；bots 的 `notifyStreamStatus` 實作 `StreamStatusObserver` 轉接；接線後收掉兩檔 module 級 `#![allow(dead_code)]`。債：`scheduler/selector.rs` 的 S2.5 最小 `ChannelLiveStatus` 依其檔內註記待 S3.1 後續清理（watchdog 以 `as_selector_statuses` 逐欄轉接）
