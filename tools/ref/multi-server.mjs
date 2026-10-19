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
