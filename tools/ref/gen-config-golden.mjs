// 直接呼叫舊版實作（$REF/dist）產生設定相關的 golden。$REF 須先 npm ci && npm run build。
// 用法：node tools/ref/gen-config-golden.mjs <REF> <案例目錄>
// 三種案例（依副檔名判斷）：
// - 寫回：<名稱>.input.yml ＋ <名稱>.ops.json
//     ops.json：{"initial":{"channels":[...],"maxConcurrentStreams":N},"ops":[{"setChannels":[...]}|{"setMaxConcurrentStreams":N}|{"replaceFile":"..."}]}
//     → <名稱>.expected.yml（最後的檔案內容）＋ <名稱>.expected.json（每個 op 的結果、updateConfig 呼叫、寫檔次數）
// - 解析：<名稱>.load.yml → <名稱>.load.expected.json（dist/config/yaml.js 的 parseYaml）
// - 設定：<名稱>.config.yml［＋ <名稱>.env.json］→ <名稱>.config.expected.json
//     （YamlConfigLoader.load 的 AppConfig、debug 日誌、createSafeConfigFingerprint 的結果）
// known-differences.json（由 make-yaml-cases.sh 產生）列出的案例仍照舊產生 golden，Rust 測試改比對該檔的 expected。
// Infinity／NaN 以 {"$number":"Infinity"} 表示；錯誤以 {"name","message"} 表示。
import { existsSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const [ref, dir] = process.argv.slice(2);
if (!ref || !dir) {
  console.error('用法：node tools/ref/gen-config-golden.mjs <REF> <案例目錄>');
  process.exit(2);
}
const load = (path) => import(pathToFileURL(resolve(ref, 'dist', path)).href);
const { YamlRuntimeConfigManager } = await load('config/RuntimeConfigManager.js');
const { parseYaml } = await load('config/yaml.js');
const { YamlConfigLoader } = await load('config/ConfigLoader.js');
const { createSafeConfigFingerprint } = await load('app/RuntimeDiagnostics.js');

const encode = (_key, value) =>
  typeof value === 'number' && !Number.isFinite(value) ? { $number: String(value) } : value;
const json = (value) => `${JSON.stringify(value, encode, 2)}\n`;
const failure = (error) => ({
  error: {
    name: error instanceof Error ? error.name : 'UnknownError',
    message: error instanceof Error ? error.message : String(error),
  },
});
const files = readdirSync(dir).sort();
const read = (name) => readFileSync(join(dir, name), 'utf8');

for (const name of files.filter((file) => file.endsWith('.input.yml'))) {
  const base = name.slice(0, -'.input.yml'.length);
  const spec = JSON.parse(read(`${base}.ops.json`));
  let content = read(name);
  let writes = 0;
  const updates = [];
  const manager = new YamlRuntimeConfigManager({
    configPath: 'config.yml',
    initialConfig: spec.initial,
    target: { updateConfig: async (config) => void updates.push(structuredClone(config)) },
    readFile: async () => content,
    writeFile: async (_path, data) => {
      writes += 1;
      content = data;
    },
  });
  const results = [];
  for (const op of spec.ops) {
    if ('replaceFile' in op) {
      content = op.replaceFile;
      results.push({ replaced: true });
      continue;
    }
    try {
      const config = 'setChannels' in op
        ? await manager.setChannels(op.setChannels)
        : await manager.setMaxConcurrentStreams(op.setMaxConcurrentStreams);
      results.push({ value: structuredClone(config) });
    } catch (error) {
      results.push(failure(error));
    }
  }
  writeFileSync(join(dir, `${base}.expected.yml`), content);
  writeFileSync(join(dir, `${base}.expected.json`), json({ results, updates, writes }));
  console.log(`${base}: ${results.map((r) => (r.error ? r.error.name : 'ok')).join(', ')}`);
}

for (const name of files.filter((file) => file.endsWith('.load.yml'))) {
  const base = name.slice(0, -'.load.yml'.length);
  let result;
  try {
    result = { value: parseYaml(read(name)) ?? null };
  } catch (error) {
    result = failure(error);
  }
  writeFileSync(join(dir, `${base}.load.expected.json`), json(result));
  console.log(`${base}: ${result.error ? result.error.message : 'ok'}`);
}

process.chdir(resolve(dir)); // 讓 AppConfig 內的相對路徑與案例目錄無關
for (const name of files.filter((file) => file.endsWith('.config.yml'))) {
  const base = name.slice(0, -'.config.yml'.length);
  const envFile = `${base}.env.json`;
  const env = existsSync(envFile) ? JSON.parse(readFileSync(envFile, 'utf8')) : {};
  const logs = [];
  const loader = new YamlConfigLoader({ debug: (event, fields) => void logs.push({ event, fields }) });
  let result;
  try {
    const value = await loader.load(name, env);
    result = { value, fingerprint: createSafeConfigFingerprint(value), logs };
  } catch (error) {
    result = { ...failure(error), logs };
  }
  writeFileSync(join(dir, `${base}.config.expected.json`), json(result));
  console.log(`${base}: ${result.error ? result.error.message : 'ok'}`);
}
