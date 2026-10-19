#!/usr/bin/env python3
"""比對兩版日誌的結構（R10）：每個事件出現過的層級與欄位（含巢狀欄位與 JSON 型別）。

用法：
  python3 tools/bench/log-schema.py <日誌.jsonl>                  列出結構
  python3 tools/bench/log-schema.py <舊版.jsonl> <Rust 版.jsonl>  比較；有「差異」時 exit 1
輸入是 docker compose logs --no-log-prefix 的輸出；非 JSON 的行只計數。
每行輸出：<分類>\t<事件>\t<說明>。分類：
  允許    §6.2 X5、X6 的差異
  差異    同一事件的層級或欄位（含型別）不同；或 Rust 版出現 doc/spec/log-events.txt 沒有的事件
  僅單邊  事件只出現在其中一版，通常是兩次執行遇到的情況不同
「差異」與「僅單邊」要逐項修正，或在 A/B 報告附上舊版程式碼位置，說明只是執行情境不同。
"""
import json
import sys
from collections import defaultdict
from pathlib import Path

RESERVED = {'level', 'event', 'time'}
ALLOWED_FIELDS = {'stack', 'processHeapUsedBytes', 'processExternalBytes', 'nodeVersion', 'playwrightVersion'}
ALLOWED_OLD_ONLY_EVENTS = {'unhandled_rejection'}
EVENTS_FILE = Path('doc/spec/log-events.txt')
MAX_DEPTH = 4


def kind(value):
    if value is None:
        return 'null'
    if isinstance(value, bool):
        return 'bool'
    if isinstance(value, (int, float)):
        return 'number'
    if isinstance(value, str):
        return 'string'
    return 'array' if isinstance(value, list) else 'object'


def walk(value, path, fields, depth=0):
    fields.add(f'{path}:{kind(value)}')
    if depth >= MAX_DEPTH:
        return
    if isinstance(value, dict):
        for key, child in value.items():
            walk(child, f'{path}.{key}', fields, depth + 1)
    elif isinstance(value, list):
        for child in value:
            walk(child, f'{path}[]', fields, depth + 1)


def load(path):
    schema = defaultdict(lambda: {'count': 0, 'levels': set(), 'fields': set()})
    skipped = 0
    with open(path, encoding='utf-8', errors='replace') as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                record = json.loads(line)
            except ValueError:
                skipped += 1
                continue
            if not isinstance(record, dict) or not isinstance(record.get('event'), str):
                skipped += 1
                continue
            entry = schema[record['event']]
            entry['count'] += 1
            entry['levels'].add(str(record.get('level')))
            for key, value in record.items():
                if key not in RESERVED:
                    walk(value, key, entry['fields'])
    if skipped:
        print(f'{path}：略過 {skipped} 行非 JSON 日誌', file=sys.stderr)
    return schema


def is_allowed_field(field):
    path = field.rsplit(':', 1)[0].replace('[]', '')
    return any(part in ALLOWED_FIELDS for part in path.split('.'))


def show(schema):
    for event in sorted(schema):
        entry = schema[event]
        levels = ','.join(sorted(entry['levels']))
        print(f"{event}\t{entry['count']}\t{levels}\t{' '.join(sorted(entry['fields']))}")


def compare(old, new):
    known = set(EVENTS_FILE.read_text(encoding='utf-8').split()) if EVENTS_FILE.exists() else None
    failures = 0

    def report(category, event, text):
        nonlocal failures
        failures += category == '差異'
        print(f'{category}\t{event}\t{text}')

    for event in sorted(set(old) | set(new)):
        a, b = old.get(event), new.get(event)
        if a is None:
            if known is not None and event not in known:
                report('差異', event, f"不在 log-events.txt 的事件（Rust 版 {b['count']} 筆）")
            else:
                report('僅單邊', event, f"只在 Rust 版（{b['count']} 筆）")
            continue
        if b is None:
            category = '允許' if event in ALLOWED_OLD_ONLY_EVENTS else '僅單邊'
            report(category, event, f"只在舊版（{a['count']} 筆）")
            continue
        if a['levels'] != b['levels']:
            levels_a, levels_b = ','.join(sorted(a['levels'])), ','.join(sorted(b['levels']))
            report('差異', event, f'層級：舊版 {levels_a}；Rust 版 {levels_b}')
        for side, fields in (('舊版', a['fields'] - b['fields']), (' Rust 版', b['fields'] - a['fields'])):
            for field in sorted(fields):
                report('允許' if is_allowed_field(field) else '差異', event, f'欄位只在{side}：{field}')
    print(f'事件種類：舊版 {len(old)}、Rust 版 {len(new)}；差異 {failures} 項', file=sys.stderr)
    return failures


def main():
    if len(sys.argv) == 2:
        show(load(sys.argv[1]))
    elif len(sys.argv) == 3:
        sys.exit(1 if compare(load(sys.argv[1]), load(sys.argv[2])) else 0)
    else:
        print(__doc__, file=sys.stderr)
        sys.exit(2)


if __name__ == '__main__':
    main()
