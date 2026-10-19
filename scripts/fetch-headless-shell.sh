#!/usr/bin/env bash
# 下載與 Playwright 1.62.1 相同版本的 headless shell，並輸出可執行檔路徑。
# 用法：scripts/fetch-headless-shell.sh [--dest 目錄] [linux64|linux-arm64|mac-arm64|mac-x64]
# - 未指定平台時依本機判斷。
# - SHA256 記錄在 chrome/SHA256SUMS（格式：<sha256> <版本> <平台>）。
#   檔案裡沒有該平台時會記錄首次下載的值；設定 REQUIRE_SHA256=1（Docker build）則直接失敗。
set -euo pipefail

CHROME_VERSION=151.0.7922.34   # Playwright 1.62.1 browsers.json：chromium-headless-shell
PW_CHROMIUM_REVISION=1234      # linux-arm64 沒有 Chrome for Testing 版本，改用 Playwright 的同版建置

root="$(cd "$(dirname "$0")/.." && pwd)"
dest=""
platform=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dest) dest="$2"; shift 2 ;;
    *) platform="$1"; shift ;;
  esac
done
if [[ -z "${platform}" ]]; then
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) platform=linux64 ;;
    Linux-aarch64 | Linux-arm64) platform=linux-arm64 ;;
    Darwin-arm64) platform=mac-arm64 ;;
    Darwin-x86_64) platform=mac-x64 ;;
    *) echo "不支援的平台：$(uname -s)-$(uname -m)" >&2; exit 1 ;;
  esac
fi
case "${platform}" in
  linux64 | mac-arm64 | mac-x64)
    url="https://storage.googleapis.com/chrome-for-testing-public/${CHROME_VERSION}/${platform}/chrome-headless-shell-${platform}.zip"
    dir="chrome-headless-shell-${platform}"
    bin="chrome-headless-shell" ;;
  linux-arm64)
    url="https://cdn.playwright.dev/builds/chromium/${PW_CHROMIUM_REVISION}/chromium-headless-shell-linux-arm64.zip"
    dir="chrome-linux"
    bin="headless_shell" ;;
  *) echo "未知平台：${platform}" >&2; exit 1 ;;
esac

dest="${dest:-${root}/.cache/headless-shell/${CHROME_VERSION}/${platform}}"
exe="${dest}/${dir}/${bin}"
if [[ ! -x "${exe}" ]]; then
  tmp="$(mktemp -d)"
  trap 'rm -rf "${tmp}"' EXIT
  curl -fsSL --retry 3 -o "${tmp}/shell.zip" "${url}"
  if command -v sha256sum > /dev/null; then
    sum="$(sha256sum "${tmp}/shell.zip")"
  else
    sum="$(shasum -a 256 "${tmp}/shell.zip")"
  fi
  sum="${sum%% *}"
  sums="${root}/chrome/SHA256SUMS"
  key="${CHROME_VERSION} ${platform}"
  recorded="$(grep -E " ${key//./\\.}\$" "${sums}" 2> /dev/null | cut -d' ' -f1 || true)"
  if [[ -n "${recorded}" ]]; then
    [[ "${recorded}" == "${sum}" ]] || { echo "SHA256 不符（${key}）：預期 ${recorded}，實際 ${sum}" >&2; exit 1; }
  elif [[ "${REQUIRE_SHA256:-0}" == "1" ]]; then
    echo "chrome/SHA256SUMS 缺少 ${key}" >&2
    exit 1
  else
    mkdir -p "$(dirname "${sums}")"
    echo "${sum} ${key}" >> "${sums}"
    echo "已記錄首次下載的 SHA256：${sum} ${key}（請提交 chrome/SHA256SUMS）" >&2
  fi
  mkdir -p "${dest}"
  unzip -q "${tmp}/shell.zip" -d "${dest}"
fi
echo "${exe}"
