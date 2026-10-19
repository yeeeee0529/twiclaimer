#!/usr/bin/env bash
# S1.2：用 $REF（舊版 + Playwright 1.62.1）錄製 CDP 指令順序與診斷事件。
# 產生 doc/spec/traces/<名稱>.{diag.jsonl,pw.log,seq} 與 doc/spec/chromium-args.txt（每行一個參數）。
# 名稱：<頁面>-b<0|1>[-chat0]；b1 = 資源阻擋全開，chat0 = 不注入聊天室封鎖。scripts/compare-traces.sh 依名稱重跑。
# 用法（在 twiclaimer 根目錄）：tools/ref/record-traces.sh <REF>
set -euo pipefail
ref="$(cd "${1:?用法：record-traces.sh <REF>}" && pwd)"
cd "$(dirname "$0")/../.."
state=testdata/fixtures/storage-state.fake.json
out=doc/spec/traces
mkdir -p "$out"

record() { # <名稱> <頁面> <TRACE_BLOCKING> <TRACE_DISABLE_CHAT>
  TRACE_BLOCKING=$3 TRACE_DISABLE_CHAT=$4 DEBUG=pw:browser,pw:protocol \
    node tools/ref/pw-trace.mjs "$ref" "$state" "$2" > "$out/$1.diag.jsonl" 2> "$out/$1.pw.log"
  python3 tools/ref/cdp-seq.py "$out/$1.pw.log" > "$out/$1.seq"
  echo "$1：CDP 指令 $(wc -l < "$out/$1.seq")、診斷事件 $(wc -l < "$out/$1.diag.jsonl")"
}

for page in live content-warning reward-available multi; do
  target="$page.html"
  [[ $page != multi ]] || target=multi
  for blocking in 0 1; do
    record "$page-b$blocking" "$target" "$blocking" 1
  done
done
record live-b0-chat0 live.html 0 0

# <launching> 那一行：去掉執行檔，--user-data-dir 換成 <tmp>
grep -m1 '<launching>' "$out/live-b0.pw.log" | sed -E 's/.*<launching> //' | tr ' ' '\n' | tail -n +2 \
  | sed -E 's#^--user-data-dir=.*#--user-data-dir=<tmp>#' > doc/spec/chromium-args.txt
echo "Chromium 參數：$(wc -l < doc/spec/chromium-args.txt) 個"
