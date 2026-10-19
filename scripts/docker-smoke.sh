#!/usr/bin/env bash

set -Eeuo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

SMOKE_IMAGE="${SMOKE_IMAGE:-twitch-watchdog:smoke}"
SMOKE_TEST_IMAGE="${SMOKE_TEST_IMAGE:-twitch-watchdog:smoke-test}"
SMOKE_CONTAINER_NAME="${SMOKE_CONTAINER_NAME:-twitch-watchdog-smoke}"
SMOKE_COMPOSE_PROJECT_NAME="twitch-watchdog-smoke-$$"
SMOKE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/twitch-watchdog-smoke.XXXXXX")"
SMOKE_CONFIG_PATH="${SMOKE_DIR}/config.yml"
SMOKE_STATE_DIR="${SMOKE_DIR}/browser-state"
COMPOSE=(
  docker compose
  --project-name "${SMOKE_COMPOSE_PROJECT_NAME}"
  -f docker-compose.yml
  -f test/docker/docker-compose.smoke.yml
)

export SMOKE_IMAGE
export SMOKE_CONTAINER_NAME
export SMOKE_CONFIG_PATH
export SMOKE_STATE_DIR

cleanup() {
  "${COMPOSE[@]}" down --remove-orphans >/dev/null 2>&1 || true
  rm -rf "${SMOKE_DIR}"
}
trap cleanup EXIT

wait_for_event_count() {
  local event="$1"
  local expected_count="$2"
  local logs
  local count

  for _attempt in $(seq 1 60); do
    logs="$("${COMPOSE[@]}" logs --no-color twitch-watchdog 2>&1 || true)"
    count="$(grep -c "\"event\":\"${event}\"" <<<"${logs}" || true)"
    if (( count >= expected_count )); then
      return 0
    fi

    if [[ -z "$("${COMPOSE[@]}" ps --status running --quiet twitch-watchdog)" ]]; then
      printf '錯誤：等待 %s 時容器已停止。\n%s\n' "${event}" "${logs}" >&2
      return 1
    fi
    sleep 1
  done

  printf '錯誤：等待 %s 累計 %s 次逾時。\n%s\n' \
    "${event}" "${expected_count}" "${logs}" >&2
  return 1
}

mkdir -p "${SMOKE_STATE_DIR}"
cat >"${SMOKE_CONFIG_PATH}" <<'YAML'
channels:
  - smoke_channel_one
  - smoke_channel_two
check_interval_seconds: 60
max_concurrent_streams: 2
headless: true
storage_state_path: /data/browser-state/storage-state.json
log_level: info
browser:
  navigation_timeout_ms: 30000
  page_health_check_interval_seconds: 30
  reward_check_interval_seconds: 15
  restart_on_crash: true
twitch_api:
  client_id: docker-smoke-client-id
  access_token: docker-smoke-access-token
YAML
cat >"${SMOKE_STATE_DIR}/storage-state.json" <<'JSON'
{"cookies":[],"origins":[]}
JSON
chmod 755 "${SMOKE_DIR}" "${SMOKE_STATE_DIR}"
chmod 666 "${SMOKE_CONFIG_PATH}"
chmod 644 "${SMOKE_STATE_DIR}/storage-state.json"

docker compose -f docker-compose.yml config --format json \
  | node test/docker/verify-compose.mjs service

"${COMPOSE[@]}" config --format json \
  | node test/docker/verify-compose.mjs smoke

docker build --target smoke-test --tag "${SMOKE_TEST_IMAGE}" .
docker build --target production --tag "${SMOKE_IMAGE}" .

docker run --rm --entrypoint sh "${SMOKE_IMAGE}" -ceu '
  test "$(id -u)" -ne 0
  test -f /app/dist/index.js
  test -f /app/package.json
  test -f /app/package-lock.json
  test -f /app/config.example.yml
  test ! -e /app/config.yml
  test ! -e /app/.env
  test ! -e /app/data
  test ! -e /app/src
  test ! -e /app/test
  test ! -d /app/node_modules/typescript
  test ! -d /app/node_modules/vitest
  test ! -d /app/node_modules/@playwright/test
  sensitive_path="$(
    find /app -type f \
      \( -name ".env" -o -name "*.storage-state.json" -o -name "storage-state.json" \) \
      -print -quit
  )"
  test -z "${sensitive_path}"
'

docker run --rm --entrypoint node "${SMOKE_IMAGE}" -e '
  const major = Number(process.versions.node.split(".")[0]);
  if (major < 24) {
    throw new Error(`Node ${process.versions.node} 不符合 >=24`);
  }
  const playwright = require("/app/node_modules/playwright/package.json");
  if (playwright.version !== "1.62.1") {
    throw new Error(`Playwright 版本不正確：${playwright.version}`);
  }
'

set +e
missing_config_output="$(docker run --rm "${SMOKE_IMAGE}" 2>&1)"
missing_config_status=$?
set -e
if [[ ${missing_config_status} -eq 0 ]]; then
  printf '錯誤：缺少設定檔時容器不應成功結束。\n' >&2
  exit 1
fi
if [[ "${missing_config_output}" != *'"event":"config_error"'* ]]; then
  printf '錯誤：缺少設定檔時未輸出 config_error。\n%s\n' \
    "${missing_config_output}" >&2
  exit 1
fi
if [[ "${missing_config_output}" != *'/app/config.yml'* ]]; then
  printf '錯誤：缺少設定檔訊息未指出 /app/config.yml。\n' >&2
  exit 1
fi

"${COMPOSE[@]}" up --detach --no-build
wait_for_event_count service_started 1
"${COMPOSE[@]}" exec -T twitch-watchdog test -w /app/config.yml

# Verify effective cgroup v2 limits when available (do not allocate memory).
if "${COMPOSE[@]}" exec -T twitch-watchdog sh -ceu '
  test -r /sys/fs/cgroup/memory.max
'; then
  memory_max="$("${COMPOSE[@]}" exec -T twitch-watchdog cat /sys/fs/cgroup/memory.max | tr -d "\r\n")"
  if [[ "${memory_max}" != "6442450944" ]]; then
    printf '錯誤：memory.max 應為 6442450944，實際為 %s。\n' "${memory_max}" >&2
    exit 1
  fi
  if "${COMPOSE[@]}" exec -T twitch-watchdog test -r /sys/fs/cgroup/memory.swap.max; then
    swap_max="$("${COMPOSE[@]}" exec -T twitch-watchdog cat /sys/fs/cgroup/memory.swap.max | tr -d "\r\n")"
    if [[ "${swap_max}" != "1073741824" && "${swap_max}" != "6442450944" ]]; then
      # Prefer swap-only 1 GiB; some runtimes report combined values.
      printf '警告：memory.swap.max 為 %s（預期 1073741824）。\n' "${swap_max}" >&2
    fi
  fi
  if "${COMPOSE[@]}" exec -T twitch-watchdog test -r /sys/fs/cgroup/pids.max; then
    pids_max="$("${COMPOSE[@]}" exec -T twitch-watchdog cat /sys/fs/cgroup/pids.max | tr -d "\r\n")"
    if [[ "${pids_max}" != "512" ]]; then
      printf '錯誤：pids.max 應為 512，實際為 %s。\n' "${pids_max}" >&2
      exit 1
    fi
  fi
else
  printf '警告：容器內無法讀取 cgroup v2 memory.max，略過有效資源限制檢查。\n' >&2
fi

"${COMPOSE[@]}" restart
wait_for_event_count service_started 2

"${COMPOSE[@]}" stop --timeout 40
wait_for_event_count service_stopped 2

printf 'Docker smoke test 通過：build、image 內容、缺設定失敗、Compose up、cgroup 限制、SIGTERM 與 restart 均已驗證。\n'
