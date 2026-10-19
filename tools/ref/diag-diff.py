#!/usr/bin/env python3
"""比對兩份診斷事件（pw-trace.mjs 與 twiclaimer probe 的 stdout，JSON Lines）。

事件順序不固定，所以當成 multiset 比較。關頁時才發生的 net::ERR_ABORTED 與時序有關，先排除。
mock-pages 的絕對路徑（舊版在 $REF/test，Rust 版在 testdata）先換成 <mock-pages>/。
用法：tools/ref/diag-diff.py <舊版.jsonl> <Rust.jsonl>；完全相同時 exit 0。
"""
import json
import re
import sys
from collections import Counter

TIMING_DEPENDENT = {'net::ERR_ABORTED'}


MOCK_PAGES = re.compile(r'(?:file://)?/[^\s"]*?/mock-pages/')


def normalize(value):
    if isinstance(value, str):
        return MOCK_PAGES.sub('<mock-pages>/', value)
    if isinstance(value, list):
        return [normalize(item) for item in value]
    if isinstance(value, dict):
        return {key: normalize(item) for key, item in value.items()}
    return value


def load(path):
    events = Counter()
    with open(path, encoding='utf-8') as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            event = json.loads(line)
            if event.get('kind') == 'requestfailed' and event.get('failureText') in TIMING_DEPENDENT:
                continue
            events[json.dumps(normalize(event), ensure_ascii=False, sort_keys=True)] += 1
    return events


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    old, new = load(sys.argv[1]), load(sys.argv[2])
    only_old, only_new = old - new, new - old
    for title, diff in (('只在舊版', only_old), ('只在 Rust 版', only_new)):
        for event, count in sorted(diff.items()):
            print(f'{title} ×{count}: {event}')
    print(f'舊版 {sum(old.values())} 筆、Rust 版 {sum(new.values())} 筆、差異 {sum(only_old.values()) + sum(only_new.values())} 筆')
    sys.exit(1 if only_old or only_new else 0)


if __name__ == '__main__':
    main()
