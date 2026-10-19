import process from 'node:process';

const mode = process.argv[2] ?? 'service';
let source = '';
for await (const chunk of process.stdin) {
  source += chunk;
}
const model = JSON.parse(source);
const service = model.services?.['twitch-watchdog'];

assert(service !== undefined, '缺少 twitch-watchdog service');
assert(service.build?.context !== undefined, '缺少 build context');
assert(service.build?.target === 'production', 'build target 必須是 production');
assert(
  service.container_name ===
    (mode === 'smoke'
      ? process.env.SMOKE_CONTAINER_NAME
      : 'twitch-watchdog'),
  'container_name 不正確',
);
assert(service.init === true, '必須啟用 init');
assert(service.read_only === true, 'root filesystem 必須唯讀');
assert(service.logging?.driver === 'local', 'Logging driver must be local');
assert(
  service.logging?.options?.['max-size'] === '10m',
  'Logging max-size must be 10m',
);
assert(
  service.logging?.options?.['max-file'] === '5',
  'Logging max-file must be 5',
);
assert(
  service.security_opt?.includes('no-new-privileges:true'),
  '必須啟用 no-new-privileges',
);
assert(
  service.security_opt?.some((option) =>
    option.startsWith('seccomp=') &&
    option.endsWith('/seccomp_profile.json'),
  ),
  '必須套用瀏覽器 seccomp profile',
);
assert(
  service.environment?.CONFIG_PATH === '/app/config.yml',
  'CONFIG_PATH 不正確',
);
assert(
  service.shm_size === '1073741824' ||
    service.shm_size === 1_073_741_824,
  'shm_size 必須是 1 GiB',
);

const MEM_LIMIT_BYTES = 6 * 1024 ** 3;
const MEMSWAP_LIMIT_BYTES = 7 * 1024 ** 3;
assert(
  asNumber(service.mem_limit) === MEM_LIMIT_BYTES ||
    service.mem_limit === '6g' ||
    service.mem_limit === '6442450944',
  'mem_limit 必須是 6 GiB',
);
assert(
  asNumber(service.memswap_limit) === MEMSWAP_LIMIT_BYTES ||
    service.memswap_limit === '7g' ||
    service.memswap_limit === '7516192768',
  'memswap_limit 必須是 7 GiB (RAM+swap combined)',
);
assert(
  asNumber(service.pids_limit) === 512 || service.pids_limit === '512',
  'pids_limit 必須是 512',
);
assert(
  service.oom_kill_disable !== true &&
    service.oom_kill_disable !== 'true',
  '不得停用 OOM killer',
);

const volumes = service.volumes ?? [];
assertWritableBind(volumes, '/app/config.yml');
assertReadOnlyBind(volumes, '/data/browser-state');

if (mode === 'smoke') {
  assert(service.network_mode === 'none', 'smoke 必須停用網路');
  assert(service.restart === 'no', 'smoke restart policy 必須停用');
} else {
  assert(
    service.restart === 'unless-stopped',
    '正式服務 restart policy 必須是 unless-stopped',
  );
}

function assertReadOnlyBind(volumes, target) {
  const volume = volumes.find((candidate) => candidate.target === target);
  assert(volume !== undefined, `缺少 ${target} volume`);
  assert(volume.type === 'bind', `${target} 必須是 bind mount`);
  assert(volume.read_only === true, `${target} 必須是唯讀掛載`);
}

function assertWritableBind(volumes, target) {
  const volume = volumes.find((candidate) => candidate.target === target);
  assert(volume !== undefined, `缺少 ${target} volume`);
  assert(volume.type === 'bind', `${target} 必須是 bind mount`);
  assert(volume.read_only !== true, `${target} 必須允許寫入`);
}

function asNumber(value) {
  if (typeof value === 'number' && Number.isFinite(value)) {
    return value;
  }
  if (typeof value === 'string' && value.trim() !== '' && !Number.isNaN(Number(value))) {
    return Number(value);
  }
  return undefined;
}

function assert(condition, message) {
  if (!condition) {
    throw new Error(message);
  }
}
