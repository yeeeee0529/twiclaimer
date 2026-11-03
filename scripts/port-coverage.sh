#!/usr/bin/env bash
# 檢查舊測試是否都有處置：Rust 測試內的「// 原測試：<標題>」註解，或 doc/test-port.md 標為「不移植」「保留 Node」的列。
# 用法（在 twiclaimer 根目錄）：
#   scripts/port-coverage.sh                    回報進度；只有「未知標題」（打錯字）時 exit 1
#   scripts/port-coverage.sh --strict [舊檔案…]  另外要求指定的舊測試檔（未指定＝全部）沒有缺漏
#     例：scripts/port-coverage.sh --strict test/unit/config.test.ts test/unit/stream-selector.test.ts
set -euo pipefail
cd "$(dirname "$0")/.."
strict=0
if [[ "${1:-}" == "--strict" ]]; then
  strict=1
  shift
fi
titles=doc/spec/ts-test-titles.tsv
[[ -f $titles ]] || { echo "找不到 $titles（先執行 tools/ref/extract-spec.sh）" >&2; exit 2; }
tmp=$(mktemp -d "${TMPDIR:-/tmp}/twicover.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
trim() { sed -E 's/^[[:space:]]+//; s/[[:space:]]+$//'; }

cut -f2 "$titles" | trim | sort > "$tmp/all"
sort -u "$tmp/all" > "$tmp/known"
if [[ $(wc -l < "$tmp/all") -ne $(wc -l < "$tmp/known") ]]; then
  echo "警告：舊測試標題有重複，請在 test-port.md 註明：" >&2
  uniq -d "$tmp/all" >&2
fi
if [[ $# -gt 0 ]]; then
  printf '%s\n' "$@" > "$tmp/files"
  awk -F'\t' 'NR == FNR { want[$0] = 1; next } { f = $1; sub(/:[0-9]+$/, "", f); if (f in want) print $2 }' \
    "$tmp/files" "$titles" | trim | sort -u > "$tmp/expected"
  [[ -s $tmp/expected ]] || { echo "指定的舊測試檔沒有任何標題：$*" >&2; exit 2; }
else
  cp "$tmp/known" "$tmp/expected"
fi

{ grep -rhoE '// 原測試：.+$' src tests 2>/dev/null || true; } | sed -E 's#^// 原測試：##' | trim | sort -u > "$tmp/ported"
if [[ -f doc/test-port.md ]]; then
  # 表格欄位：| 舊檔案 | 原測試 | 處置 | Rust 測試／理由 |
  awk -F'|' 'NF >= 6 { t = $3; d = $4; r = $5
    gsub(/^[ \t]+|[ \t]+$/, "", t); gsub(/^[ \t]+|[ \t]+$/, "", d); gsub(/^[ \t]+|[ \t]+$/, "", r)
    if ((d == "不移植" || d == "保留 Node") && r != "") print t }' doc/test-port.md | sort -u > "$tmp/skipped"
else
  : > "$tmp/skipped"
fi
sort -u "$tmp/ported" "$tmp/skipped" > "$tmp/covered"

missing=$(comm -23 "$tmp/expected" "$tmp/covered")
unknown=$(comm -13 "$tmp/known" "$tmp/covered")
n_missing=$(printf '%s' "$missing" | grep -c . || true)
n_unknown=$(printf '%s' "$unknown" | grep -c . || true)
echo "舊測試 $(wc -l < "$tmp/expected") 筆：已處置 $(comm -12 "$tmp/expected" "$tmp/covered" | wc -l)、缺 ${n_missing}、未知標題 ${n_unknown}"
if [[ $n_missing -ne 0 ]]; then
  if [[ $strict -eq 1 ]]; then
    echo "== 缺少處置 =="
    printf '%s\n' "$missing"
  else
    echo "（加 --strict 列出缺少處置的標題）"
  fi
fi
[[ $n_unknown -eq 0 ]] || { echo "== 未知標題（打錯字或不在清單）=="; printf '%s\n' "$unknown"; }
[[ $n_unknown -eq 0 && ( $strict -eq 0 || $n_missing -eq 0 ) ]]
