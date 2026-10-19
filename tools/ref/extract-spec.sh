#!/usr/bin/env bash
# 從舊版 repo 抽出規格清單：測試標題（ts-test-titles.tsv）與日誌事件名稱（log-events.txt）。
# 用法：tools/ref/extract-spec.sh <舊版 repo> [輸出目錄，預設 doc/spec]
set -euo pipefail
old="$(cd "${1:?用法：extract-spec.sh <舊版 repo> [輸出目錄]}" && pwd)"
out="${2:-doc/spec}"
mkdir -p "${out}"

# 每列：<檔案>:<行號><TAB><標題>（含 it.each／test.each）
# DEC-010：第二條 grep 補抓案例表跨行的 it.each（`])('標題'` 續行）。
{
  grep -rnoE "^\s*(it|test)(\.each\([^)]*\))?\(\s*['\"\`][^'\"\`]+" "${old}/test" --include=*.ts \
    | sed -E "s/:[[:space:]]*(it|test)(\.each\([^)]*\))?\([[:space:]]*['\"\`]/\t/"
  grep -rnoE "^\s*\]\)\(\s*['\"\`][^'\"\`]+" "${old}/test" --include=*.ts \
    | sed -E "s/:[[:space:]]*\]\)\([[:space:]]*['\"\`]/\t/" || true
} | sed -E "s#^${old}/##" \
  | LC_ALL=C sort -t: -k1,1 -k2,2n > "${out}/ts-test-titles.tsv"

# logger.debug／info／warn／error('事件')、('層級', '事件')、event: '事件'，以及 Logger.ts 的 LOG_EVENTS
{
  grep -rhoE "\b(debug|info|warn|error)\(\s*'[a-z][a-z0-9_]+'" "${old}/src" || true
  grep -rhoE "'(debug|info|warn|error)',\s*'[a-z][a-z0-9_]+'" "${old}/src" || true
  grep -rhoE "\bevent: '[a-z][a-z0-9_]+'" "${old}/src" || true
  awk '/export const LOG_EVENTS/,/^}/' "${old}/src/logging/Logger.ts"
} | grep -oE "'[a-z][a-z0-9_]+'" | tr -d "'" | grep -vxE 'debug|info|warn|error' | LC_ALL=C sort -u > "${out}/log-events.txt"

echo "測試標題：$(wc -l < "${out}/ts-test-titles.tsv")"
echo "日誌事件：$(wc -l < "${out}/log-events.txt")"
cut -f1 "${out}/ts-test-titles.tsv" | cut -d: -f1 | sort | uniq -c | sort -k2
