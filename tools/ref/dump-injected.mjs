// 從 $REF 安裝的 playwright-core 取出 injected script 原始碼（Playwright 在頁面內用來解析選擇器、
// 檢查元素狀態與 hit target 的程式），逐字寫到 stdout。Rust 版以 include_str! 嵌入（§7.4）。
// 用法：node tools/ref/dump-injected.mjs <REF> > src/browser/cdp/js/injected-script.js
// stderr 印出 playwright-core 版本、位元組數與 SHA-256，寫進 doc/decisions.md。
import { createHash } from 'node:crypto';
import { existsSync, readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join, resolve } from 'node:path';
import vm from 'node:vm';

const [ref] = process.argv.slice(2);
if (!ref) {
  console.error('用法：node tools/ref/dump-injected.mjs <REF>');
  process.exit(2);
}
const require = createRequire(join(resolve(ref), 'package.json'));
const coreDir = dirname(require.resolve('playwright-core/package.json'));
const { version } = JSON.parse(readFileSync(join(coreDir, 'package.json'), 'utf8'));

function fromBundle(file) {
  // 新版把 lib 打包成單檔：source = '<JS 字串>'，位於 injectedScriptSource.ts 的 __esm 區塊。
  const bundle = readFileSync(file, 'utf8');
  const marker = bundle.indexOf('src/generated/injectedScriptSource.ts"() {');
  if (marker < 0) throw new Error(`${file} 找不到 injectedScriptSource`);
  const start = bundle.indexOf(" = '", marker) + 3;
  let end = start + 1;
  while (bundle[end] !== "'") end += bundle[end] === '\\' ? 2 : 1;
  return vm.runInNewContext(bundle.slice(start, end + 1));
}

const legacy = join(coreDir, 'lib/generated/injectedScriptSource.js');
const source = existsSync(legacy) ? require(legacy).source : fromBundle(join(coreDir, 'lib/coreBundle.js'));
if (typeof source !== 'string' || !source.includes('InjectedScript')) {
  throw new Error('取出的內容不是 injected script');
}
process.stdout.write(source);
const sha256 = createHash('sha256').update(source).digest('hex');
console.error(`playwright-core ${version}：${Buffer.byteLength(source)} bytes，sha256 ${sha256}`);
