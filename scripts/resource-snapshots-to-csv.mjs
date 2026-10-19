import process from 'node:process';
import { createInterface } from 'node:readline';

const columns = [
  'time',
  'processCpuUserUs',
  'processCpuSystemUs',
  'processRssBytes',
  'processHeapUsedBytes',
  'processExternalBytes',
  'processMaxRssKb',
  'activeChannelCount',
  'browserPageCount',
  'cgroupMemoryCurrentBytes',
  'cgroupMemoryPeakBytes',
  'cgroupSwapCurrentBytes',
  'cgroupPidsCurrent',
  'cgroupCpuUsageUsec',
  'cgroupCpuUserUsec',
  'cgroupCpuSystemUsec',
  'cgroupMemoryEventsHigh',
  'cgroupMemoryEventsMax',
  'cgroupMemoryEventsOom',
  'cgroupMemoryEventsOomKill',
  'resourceGuardState',
  'browserRecycleInFlight',
];

process.stdout.write(`${columns.join(',')}\n`);
const lines = createInterface({
  input: process.stdin,
  crlfDelay: Number.POSITIVE_INFINITY,
});

for await (const line of lines) {
  let record;
  try {
    record = JSON.parse(line);
  } catch {
    continue;
  }
  if (record?.event !== 'runtime_resource_snapshot') {
    continue;
  }
  process.stdout.write(
    `${columns.map((column) => csv(record[column])).join(',')}\n`,
  );
}

function csv(value) {
  const text = value === undefined ? '' : String(value);
  return /[",\n\r]/u.test(text)
    ? `"${text.replaceAll('"', '""')}"`
    : text;
}
