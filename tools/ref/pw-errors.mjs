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
