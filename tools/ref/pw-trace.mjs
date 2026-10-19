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
