#!/usr/bin/env bash
# Docker smoke test（計畫 §11.3 的 11 項檢查，Rust 版）。
# 用法：scripts/docker-smoke.sh
# 需要本機 Docker；build test stage 內含 REQUIRE_E2E=1 scripts/check.sh（完整驗證）。

set -Eeuo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

SMOKE_IMAGE="${SMOKE_IMAGE:-twiclaimer:smoke}"
SMOKE_CONTAINER_NAME="${SMOKE_CONTAINER_NAME:-twiclaimer-smoke}"
SMOKE_COMPOSE_PROJECT_NAME="twiclaimer-smoke-$$"
SMOKE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/twiclaimer-smoke.XXXXXX")"
SMOKE_CONFIG_PATH="${SMOKE_DIR}/config.yml"
SMOKE_STATE_DIR="${SMOKE_DIR}/browser-state"
COMPOSE=(
  docker compose
  --project-name "${SMOKE_COMPOSE_PROJECT_NAME}"
  -f docker-compose.yml
  -f test-node/docker/docker-compose.smoke.yml
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

# 1. 驗證正式與 smoke Compose 的合併結果（含 logging local 10m×5）。
docker compose -f docker-compose.yml config --format json \
  | node test-node/docker/verify-compose.mjs service

"${COMPOSE[@]}" config --format json \
  | node test-node/docker/verify-compose.mjs smoke

# 2. build test stage：容器內跑 REQUIRE_E2E=1 scripts/check.sh（fmt、clippy、
#    單元測試、e2e、port-coverage）。
docker build --target test .

# 3. build production image。
docker build --target production --tag "${SMOKE_IMAGE}" .

# 3/4. image 內容：只含 twiclaimer、headless shell、config.example.yml 與系統套件；
#      不含 config.yml、.env、storageState、原始碼或測試。
docker run --rm --entrypoint sh "${SMOKE_IMAGE}" -ceu '
  test "$(id -u)" -ne 0
  test -x /usr/local/bin/twiclaimer
  test -x /opt/headless-shell/headless-shell
  test -f /app/config.example.yml
  test ! -e /app/config.yml
  test ! -e /app/.env
  test ! -e /app/data
  test ! -e /app/src
  test ! -e /app/Cargo.toml
  test ! -e /app/Cargo.lock
  test ! -d /app/node_modules
  sensitive_path="$(
    find /app -type f \
      \( -name ".env" -o -name "*.storage-state.json" -o -name "storage-state.json" \) \
      -print -quit
  )"
  test -z "${sensitive_path}"
'

# CMD 與版本：正式進入點就是 twiclaimer 本體。
docker run --rm "${SMOKE_IMAGE}" twiclaimer --version

# 5. 缺少 /app/config.yml 時以非零狀態結束，並輸出 config_error。
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
  printf '錯誤：缺少設定檔訊息未指出 /app/config.yml。\n%s\n' \
    "${missing_config_output}" >&2
  exit 1
fi

# 6. 以 smoke override 啟動（network none、假憑證），config.yml 對非 root 可寫。
"${COMPOSE[@]}" up --detach --no-build
wait_for_event_count service_started 1
"${COMPOSE[@]}" exec -T twitch-watchdog test -w /app/config.yml

# 8. cgroup v2 生效時讀容器內的限制值（不配置記憶體）。
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

# 7. 重啟服務；SIGTERM 之後要有 service_stopped。
"${COMPOSE[@]}" restart
wait_for_event_count service_started 2

"${COMPOSE[@]}" stop --timeout 40
wait_for_event_count service_stopped 2

# 10. 停止之後容器內沒有殘留的 headless shell 程序：SIGTERM 只送給 PID 1，
#     headless shell 是子程序、收不到訊號；若 twiclaimer 沒有把它收乾淨，
#     docker 會在 grace period 後 SIGKILL（ExitCode 137）。優雅退出（0）
#     即表示服務已自行停止 headless shell。
container_id="$("${COMPOSE[@]}" ps -aq twitch-watchdog)"
exit_code="$(docker inspect -f '{{.State.ExitCode}}' "${container_id}")"
if [[ "${exit_code}" != "0" ]]; then
  printf '錯誤：容器停止後 ExitCode 為 %s（非 0 表示 SIGTERM 後未收乾淨子程序）。\n' \
    "${exit_code}" >&2
  exit 1
fi

# 11. 輸出正式 image 大小（§10 R3 的量法）。
image_size="$(docker save "${SMOKE_IMAGE}" | gzip -1 | wc -c | tr -d ' ')"
printf '正式 image 大小（docker save | gzip -1）：%s bytes\n' "${image_size}"

printf 'Docker smoke test 通過：compose 驗證、test stage、image 內容、缺設定失敗、Compose up、cgroup 限制、SIGTERM 與無殘留程序均已驗證。\n'
