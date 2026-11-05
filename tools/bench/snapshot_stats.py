#!/usr/bin/env python3
"""依頁數分組統計 runtime_resource_snapshot：控制端與整容器的記憶體中位數、平均 CPU 與占比。

用法：cat snapshots-*.jsonl | python3 tools/bench/snapshot_stats.py
多次匯出的檔案可直接串接；同一時間的樣本只算一次。
"""
import json
import statistics
import sys
from collections import defaultdict
from datetime import datetime

MIB = 2**20
rows = {}  # 時間 -> (頁數, 控制端 RSS, 整容器記憶體, 控制端 CPU µs, 整容器 CPU µs)
for line in sys.stdin:
    try:
        r = json.loads(line)
        if r.get('event') != 'runtime_resource_snapshot':
            continue
        t = datetime.fromisoformat(r['time'].replace('Z', '+00:00')).timestamp()
        rows[t] = (
            int(r['browserPageCount']),
            float(r['processRssBytes']),
            float(r['cgroupMemoryCurrentBytes']),
            float(r['processCpuUserUs']) + float(r['processCpuSystemUs']),
            float(r['cgroupCpuUsageUsec']),
        )
    except (ValueError, KeyError, TypeError, AttributeError):
        continue
if not rows:
    sys.exit('找不到 runtime_resource_snapshot')

mem = defaultdict(list)                     # 頁數 -> [(控制端 RSS, 整容器記憶體)]
cpu = defaultdict(lambda: [0.0, 0.0, 0.0])  # 頁數 -> [控制端 CPU µs, 整容器 CPU µs, 秒數]
prev = None
for t in sorted(rows):
    pages, rss, cg_mem, p_cpu, cg_cpu = rows[t]
    mem[pages].append((rss, cg_mem))
    if prev is not None:
        dt, dp, dc = t - prev[0], p_cpu - prev[1], cg_cpu - prev[2]
        if 0 < dt <= 330 and dp >= 0 and dc >= 0:  # 300 秒區間含排程抖動；略過重啟造成的負差值與長時間中斷
            acc = cpu[pages]
            acc[0] += dp
            acc[1] += dc
            acc[2] += dt
    prev = (t, p_cpu, cg_cpu)

print('頁數  樣本  控制端RSS  整容器記憶體  RAM占比  控制端CPU  整容器CPU  CPU占比')
for pages in sorted(mem):
    rss = statistics.median(x for x, _ in mem[pages]) / MIB
    cg = statistics.median(y for _, y in mem[pages]) / MIB
    p, c, s = cpu[pages]
    p_cores = p / (s * 1e6) if s else float('nan')
    c_cores = c / (s * 1e6) if s else float('nan')
    share = p / c if c else float('nan')
    print(f'{pages:>4}  {len(mem[pages]):>4}  {rss:>7.0f}MiB  {cg:>9.0f}MiB  {rss / cg:>6.1%}'
          f'  {p_cores:>7.3f}核  {c_cores:>7.3f}核  {share:>6.1%}')
