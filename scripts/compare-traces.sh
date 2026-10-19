#!/usr/bin/env bash
# G2、G3：用 twiclaimer probe 重跑 S1.2 的每個錄製，比對 CDP 指令順序（.seq）與診斷事件（.diag.jsonl）。
# 用法：scripts/compare-traces.sh [名稱…]   預設為 doc/spec/traces/*.seq 全部
#   名稱格式：<頁面>-b<0|1>[-chat0]，例如 live-b0、multi-b1、live-b0-chat0
#   BIN 指定執行檔（預設 target/debug/twiclaimer）；KEEP=1 保留輸出目錄。
set -euo pipefail
cd "$(dirname "$0")/.."
bin="${BIN:-target/debug/twiclaimer}"
traces=doc/spec/traces
state=testdata/fixtures/storage-state.fake.json
out="$(mktemp -d)"
server=""
multi=""
cleanup() {
  if [[ -n $server ]]; then
    kill "$server" 2>/dev/null || true
    wait "$server" 2>/dev/null || true
  fi
  if [[ ${KEEP:-0} == 1 ]]; then echo "輸出保留在 $out" >&2; else rm -rf "$out"; fi
}
trap cleanup EXIT

start_multi() {
  [[ -z $server ]] || return 0
  node tools/ref/multi-server.mjs > "$out/multi.url" &
  server=$!
  for _ in $(seq 100); do
    [[ -s $out/multi.url ]] && break
    sleep 0.1
  done
  multi="$(head -n1 "$out/multi.url")"
  [[ -n $multi ]] || { echo "multi-server 沒有啟動" >&2; exit 2; }
}

names=("$@")
if [[ ${#names[@]} -eq 0 ]]; then
  for file in "$traces"/*.seq; do names+=("$(basename "$file" .seq)"); done
fi

failed=0
for name in "${names[@]}"; do
  if [[ ! $name =~ ^(.+)-b([01])(-chat0)?$ ]]; then
    echo "無法解析名稱：$name" >&2
    exit 2
  fi
  page="${BASH_REMATCH[1]}"
  blocking="${BASH_REMATCH[2]}"
  chat=1
  [[ -z ${BASH_REMATCH[3]} ]] || chat=0
  if [[ $page == multi ]]; then
    start_multi
    target="$multi"
  else
    target="$page.html"
  fi
  if ! TRACE_BLOCKING=$blocking TRACE_DISABLE_CHAT=$chat TWICLAIMER_CDP_TRACE="$out/$name.log" \
    "$bin" probe "$state" "$target" > "$out/$name.diag.jsonl"; then
    echo "${name}：probe 失敗"
    failed=1
    continue
  fi
  python3 tools/ref/cdp-seq.py "$out/$name.log" > "$out/$name.seq"
  result="${name}："
  if diff -u "$traces/$name.seq" "$out/$name.seq" > "$out/$name.seq.diff"; then
    result+="CDP 順序相同"
  else
    result+="CDP 順序不同（$out/$name.seq.diff）"
    failed=1
  fi
  if python3 tools/ref/diag-diff.py "$traces/$name.diag.jsonl" "$out/$name.diag.jsonl" > "$out/$name.diag.diff"; then
    result+="；診斷相同"
  else
    result+="；診斷不同（$out/$name.diag.diff）"
    failed=1
  fi
  echo "$result"
done
if [[ $failed != 0 ]]; then
  KEEP=1
  exit 1
fi
