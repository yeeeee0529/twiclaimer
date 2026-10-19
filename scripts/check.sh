#!/usr/bin/env bash
# 每一步完成後執行的檢查（本機與 Docker test stage 共用）。
# 用法：scripts/check.sh
#   CHROME_PATH 有值時另跑標記 #[ignore] 的 e2e（需要真的 headless shell）。
#   REQUIRE_E2E=1 時缺少 CHROME_PATH 直接失敗（階段關卡與 Docker test stage 使用）。
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked

if [[ -n "${CHROME_PATH:-}" ]]; then
  cargo test --locked -- --ignored --test-threads=2
elif [[ "${REQUIRE_E2E:-0}" == "1" ]]; then
  echo "缺少 CHROME_PATH：先執行 export CHROME_PATH=\"\$(scripts/fetch-headless-shell.sh)\"" >&2
  exit 1
else
  echo "略過 e2e：未設定 CHROME_PATH" >&2
fi

if [[ -d node_modules ]]; then
  npm test
elif [[ "${REQUIRE_NODE_TOOLS:-0}" == "1" ]]; then
  echo "缺少 node_modules：先執行 npm ci" >&2
  exit 1
else
  echo "略過 Node 工具測試：未執行 npm ci" >&2
fi

scripts/port-coverage.sh
