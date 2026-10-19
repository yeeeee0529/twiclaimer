import { spawnSync } from 'node:child_process';
import path from 'node:path';

import { describe, expect, it } from 'vitest';

describe('resource-snapshots-to-csv', () => {
  it('保留 cgroup CPU 累計時間欄位', () => {
    const scriptPath = path.join(
      process.cwd(),
      'scripts/resource-snapshots-to-csv.mjs',
    );
    const snapshot = {
      event: 'runtime_resource_snapshot',
      time: '2026-07-19T00:00:00.000Z',
      cgroupCpuUsageUsec: '9007199254740992',
      cgroupCpuUserUsec: 700,
      cgroupCpuSystemUsec: 300,
    };

    const result = spawnSync(process.execPath, [scriptPath], {
      encoding: 'utf8',
      input: `${JSON.stringify({ event: 'ignored' })}\n${JSON.stringify(snapshot)}\n`,
    });

    expect(result.status).toBe(0);
    expect(result.stderr).toBe('');
    const [headerLine, valueLine] = result.stdout.trim().split('\n');
    const headers = headerLine?.split(',') ?? [];
    const values = valueLine?.split(',') ?? [];
    expect(values[headers.indexOf('cgroupCpuUsageUsec')]).toBe(
      '9007199254740992',
    );
    expect(values[headers.indexOf('cgroupCpuUserUsec')]).toBe('700');
    expect(values[headers.indexOf('cgroupCpuSystemUsec')]).toBe('300');
  });
});
