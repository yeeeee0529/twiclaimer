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

## DEC-001 標題

- 日期：YYYY-MM-DD（步驟：S1.2）
- 問題：
- 決定：
- 理由：
- 影響：檔案、測試、§6.2 的允許差異；需要人工確認時寫明
