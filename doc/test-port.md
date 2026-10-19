# 測試移植處置

已移植的測試以 Rust 測試上一行的 `// 原測試：` 註解為準，不列在這裡。本表只列不移植、保留 Node，以及預期值依 §6.2 調整的測試。

| 舊檔案 | 原測試 | 處置 | Rust 測試／理由 |
| --- | --- | --- | --- |
| test/unit/config.test.ts | %s 時拒絕設定 | 標題重複 | 同名 it.each 有兩組（config.test.ts:223 與 661）；移植時以同一標題標註、案例表分別照抄（DEC-010 補抓後才出現的重複） |
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
