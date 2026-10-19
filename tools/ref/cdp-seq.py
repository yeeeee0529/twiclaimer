#!/usr/bin/env python3
"""把 CDP 紀錄轉成「每個 session 送出的指令」清單，比對 Playwright 與 Rust 版的初始化流程。

輸入：Playwright 的 DEBUG=pw:protocol 輸出，或 Rust 版 TWICLAIMER_CDP_TRACE 檔（格式相同：
      每行含 `SEND ► {json}` 或 `◀ RECV {json}`）。
輸出：依 session 分組、組內保留送出順序，每行 `<session>\t<method> <遮蔽後的 params>`。
      session 依 attach 順序命名（page#1、worker#1…）；沒有 sessionId 的是 browser。
      會變動的值（各種 id、下載路徑、port、mock-pages 的絕對路徑、init script 內容）會遮蔽或改成雜湊。
略過：evaluate／callFunctionOn 等執行期呼叫、攔截回應、截圖與導覽細節、Playwright utility world，
      以及 Browser.close（Playwright 直接寫入 pipe，不會出現在 pw:protocol 紀錄）。
用法：python3 tools/ref/cdp-seq.py doc/spec/traces/multi-b0.pw.log > doc/spec/traces/multi-b0.seq
      python3 tools/ref/cdp-seq.py /tmp/rust-multi-b0.log > /tmp/rust-multi-b0.seq && diff doc/spec/traces/multi-b0.seq /tmp/rust-multi-b0.seq
      加 --methods-only 只比對 method 名稱。
"""
import hashlib
import json
import re
import sys
from collections import defaultdict

SKIP = re.compile(
    r'^(Runtime\.(callFunctionOn|evaluate|releaseObject|getProperties)|DOM\.\w+|Input\.\w+'
    r'|Fetch\.(continueRequest|failRequest|fulfillRequest)|Page\.(getLayoutMetrics|captureScreenshot)'
    r'|Browser\.close)$'
)
ID_KEYS = {
    'targetId', 'frameId', 'browserContextId', 'sessionId', 'requestId', 'windowId', 'downloadPath',
    'objectId', 'executionContextId', 'contextId', 'uniqueContextId', 'interceptionId', 'loaderId',
    'backendNodeId', 'identifier',
}
UTILITY = '__playwright_utility_world'


def mask(value, key=None):
    if isinstance(value, dict):
        return {k: mask(v, k) for k, v in sorted(value.items())}
    if isinstance(value, list):
        return [mask(v) for v in value]
    if key in ID_KEYS:
        return '<id>'
    if key == 'source' and isinstance(value, str):
        return 'sha256:' + hashlib.sha256(value.encode()).hexdigest()[:12]
    if isinstance(value, str):
        value = re.sub(r'(127\.0\.0\.1|localhost):\d+', r'\1:<port>', value)
        value = re.sub(r'(?:file://)?/[^\s"]*?/mock-pages/', '<mock-pages>/', value)
        value = re.sub(r'@[0-9a-f]{32}$', '@<hash>', value)
    return value


def main():
    args = [a for a in sys.argv[1:] if not a.startswith('--')]
    methods_only = '--methods-only' in sys.argv
    if len(args) != 1:
        sys.exit(__doc__)
    labels = {}
    counts = defaultdict(int)
    groups = defaultdict(list)
    order = ['browser']
    with open(args[0], encoding='utf-8', errors='replace') as stream:
        for line in stream:
            send = line.find('SEND ► ')
            recv = line.find('◀ RECV ')
            if send < 0 and recv < 0:
                continue
            try:
                message = json.loads(line[(send if send >= 0 else recv) + 7:])
            except ValueError:
                continue
            if recv >= 0:
                if message.get('method') == 'Target.attachedToTarget':
                    params = message['params']
                    kind = params['targetInfo']['type']
                    counts[kind] += 1
                    labels[params['sessionId']] = f'{kind}#{counts[kind]}'
                    order.append(labels[params['sessionId']])
                continue
            method = message.get('method', '')
            params = message.get('params', {})
            if SKIP.match(method) or UTILITY in str(params.get('worldName', '')):
                continue
            session = message.get('sessionId')
            label = 'browser' if session is None else labels.get(session, 'unknown')
            if label == 'unknown' and label not in order:
                order.append(label)
            text = method if methods_only else f'{method} {json.dumps(mask(params), ensure_ascii=False, sort_keys=True)}'
            groups[label].append(text)
    for label in order:
        for text in groups.get(label, []):
            print(f'{label}\t{text}')


if __name__ == '__main__':
    main()
