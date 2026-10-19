#!/usr/bin/env bash
# 產生設定 golden 的輸入案例；之後用 gen-config-golden.mjs 跑舊版實作產生預期結果。
# 用法：tools/ref/make-yaml-cases.sh <輸出目錄> [舊版 repo 路徑]
#   有給舊版路徑時，另外複製 test/fixtures/config 與 config.example.yml 作為「設定」案例。
set -euo pipefail
dir="${1:?用法：make-yaml-cases.sh <輸出目錄> [舊版 repo 路徑]}"
old="${2:-}"
[[ -z "${old}" ]] || old="$(cd "${old}" && pwd)"
mkdir -p "${dir}"
cd "${dir}"
w() { cat > "$1"; }                      # 以 heredoc 寫入檔案
ops() { printf '%s\n' "$2" > "$1.ops.json"; }  # 寫回案例的初始狀態與操作序列

# ---- 寫回案例：<名稱>.input.yml ＋ <名稱>.ops.json ----
w 01-basic.input.yml <<'Y'
# keep this comment
channels:
  - first
  - second
max_concurrent_streams: 2
log_level: debug
twitch_api:
  client_id: ${TWITCH_CLIENT_ID}
Y
ops 01-basic '{"initial":{"channels":["first","second"],"maxConcurrentStreams":2},"ops":[{"setChannels":["second","third"]}]}'
w 02-flow.input.yml <<'Y'
channels: [first, second] # inline
max_concurrent_streams: 2
Y
ops 02-flow '{"initial":{"channels":["first","second"],"maxConcurrentStreams":2},"ops":[{"setChannels":["first","third"]}]}'
w 03-inner-comments.input.yml <<'Y'
channels: # 依優先序
  # main
  - first # primary
  - second
max_concurrent_streams: 2 # 上限
log_level: info
Y
ops 03-inner-comments '{"initial":{"channels":["first","second"],"maxConcurrentStreams":2},"ops":[{"setChannels":["first"]}]}'
w 04-missing-max.input.yml <<'Y'
channels:
  - a
log_level: info
Y
ops 04-missing-max '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a","b"]}]}'
w 05-quoting.input.yml <<'Y'
channels:
  - a
max_concurrent_streams: 1
Y
ops 05-quoting '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["123","true","null","1e3","0x1F","1_000","Yes","on","NULL","007","0o17","Inf","_x"]},{"setMaxConcurrentStreams":2}]}'
printf '# c\r\nchannels:\r\n  - a\r\nmax_concurrent_streams: 1\r\nlog_level: info\r\n' > 06-crlf.input.yml
ops 06-crlf '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["b"]}]}'
w 07-doc-markers.input.yml <<'Y'
---
# header
channels: [a]
max_concurrent_streams: 1
# trailing
Y
ops 07-doc-markers '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a","b"]},{"setMaxConcurrentStreams":2}]}'
w 08-anchor.input.yml <<'Y'
channels: &ch
  - a
mirror: *ch
max_concurrent_streams: 1
Y
ops 08-anchor '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["b"]}]}'
w 09-indent4-long.input.yml <<'Y'
twitch_api:
    client_id: "${TWITCH_CLIENT_ID}"
    client_secret: '${TWITCH_CLIENT_SECRET}'
channels:
    - a
max_concurrent_streams: 1
note: this is a very long plain scalar value that goes beyond eighty characters for sure yes indeed
ids: [123456789012345678, "987"]
Y
ops 09-indent4-long '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a","b"]},{"setMaxConcurrentStreams":2}]}'
printf 'channels: [a\n' > 10-syntax-error.input.yml
ops 10-syntax-error '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a"]}]}'
printf -- '- a\n' > 11-root-seq.input.yml
ops 11-root-seq '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a"]}]}'
printf 'channels: [a]\nchannels: [b]\n' > 12-duplicate-key.input.yml
ops 12-duplicate-key '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a"]}]}'
printf 'channels:\n  - a\n\n\nmax_concurrent_streams: 1\n\n# end\n' > 13-blank-lines.input.yml
ops 13-blank-lines '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a","b"]},{"setMaxConcurrentStreams":2}]}'
w 14-anchor-elsewhere.input.yml <<'Y'
defaults: &d
  x: 1
other: *d
channels:
  - a
max_concurrent_streams: 1
Y
ops 14-anchor-elsewhere '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a","b"]},{"setMaxConcurrentStreams":2}]}'
w 15-max-anchor.input.yml <<'Y'
channels:
  - a
max_concurrent_streams: &m 1
copy: *m
Y
ops 15-max-anchor '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setMaxConcurrentStreams":1}]}'
w 16-nested-keys.input.yml <<'Y'
telegram:
  channels: keep-me
  max_concurrent_streams: 9
channels:
  - a
max_concurrent_streams: 1
Y
ops 16-nested-keys '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["b","c"]},{"setMaxConcurrentStreams":2}]}'
w 17-last-line-no-newline.input.yml <<'Y'
channels:
  - a
Y
printf 'max_concurrent_streams: 1' >> 17-last-line-no-newline.input.yml
ops 17-last-line-no-newline '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a","b"]},{"setMaxConcurrentStreams":2}]}'

w 18-validation.input.yml <<'Y'
channels:
  - a
  - b
max_concurrent_streams: 2
Y
ops 18-validation '{"initial":{"channels":["a","b"],"maxConcurrentStreams":2},"ops":[{"setChannels":[]},{"setChannels":["bad-name"]},{"setChannels":["abcdefghijklmnopqrstuvwxyz"]},{"setChannels":[" A ","a","B_1"]},{"setMaxConcurrentStreams":0},{"setMaxConcurrentStreams":3},{"setMaxConcurrentStreams":1.5},{"setMaxConcurrentStreams":1}]}'
w 19-external-edit.input.yml <<'Y'
channels:
  - a
max_concurrent_streams: 1
Y
ops 19-external-edit '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a","b"]},{"replaceFile":"channels:\n  - x\nmax_concurrent_streams: 1\nlog_level: warn\n"},{"setMaxConcurrentStreams":2},{"replaceFile":"channels: [\n"},{"setMaxConcurrentStreams":1}]}'

: > 20-empty.input.yml
ops 20-empty '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setChannels":["a","b"]}]}'
printf '# only a comment\n' > 21-comment-only.input.yml
ops 21-comment-only '{"initial":{"channels":["a"],"maxConcurrentStreams":1},"ops":[{"setMaxConcurrentStreams":1}]}'
printf 'channels: [a, b]  # 行尾註解\nmax_concurrent_streams: 2 # 上限\n' > 22-flow-trailing-comment.input.yml
ops 22-flow-trailing-comment '{"initial":{"channels":["a","b"],"maxConcurrentStreams":2},"ops":[{"setChannels":["c"]}]}'

# ---- 載入案例：<名稱>.load.yml ----
printf 'a: 1\na: 2\n' > l01-dup.load.yml
printf 'a: 1\n---\nb: 2\n' > l02-multidoc.load.yml
{ echo 'x: &a [1]'; for i in $(seq 0 100); do echo "k${i}: *a"; done; } > l03-alias-bomb.load.yml
{ echo 'x: &a [1]'; for i in $(seq 0 98); do echo "k${i}: *a"; done; } > l03b-alias-99.load.yml
{ echo 'x: &a [1]'; for i in $(seq 0 99); do echo "k${i}: *a"; done; } > l03c-alias-100.load.yml
printf 'a: &x\n  b: *x\n' > l04-cycle.load.yml
printf 'base: &b {x: 1}\nm:\n  <<: *b\n  y: 2\n' > l05-merge.load.yml
w l06-scalars.load.yml <<'Y'
a: yes
b: on
c: 1_000
d: 0x1F
e: .inf
f: ~
g: 123456789012345678
h: "${HOME}"
i: 0o17
j: 1e3
k: TRUE
l: -.INF
m: .nan
n: 0.5
o: +12
p: 010
q: ''
r:
Y
: > l07-empty.load.yml
printf 'a: [1, 2\n' > l08-syntax.load.yml
printf 'a: !!str 123\nb: !custom value\nc: !!int "7"\n' > l09-tags.load.yml
printf 'a: |\n  line1\n  line2\nb: >-\n  folded\n  text\nc: "esc\\tq\\u00e9"\n' > l10-block-scalars.load.yml
printf '\xef\xbb\xbfa: 1\n' > l11-bom.load.yml

# ---- 設定案例：<名稱>.config.yml［＋ <名稱>.env.json］（需要舊版 repo 路徑）----
if [[ -n "${old}" ]]; then
  # Rust 版只接受 headless: true（§6.2），共用案例先把舊 fixture 改成 true；舊值另列為已知差異 c08。
  sed 's/^headless: false$/headless: true/' "${old}/test/fixtures/config/valid.yml" > c01-valid.config.yml
  cp "${old}/test/fixtures/config/defaults.yml" c02-defaults.config.yml
  # 檔案寫 false、環境變數寫 TRUE：驗證覆寫後才檢查 headless。
  sed 's/^headless: true$/headless: false/' "${old}/test/fixtures/config/env.yml" > c03-env.config.yml
  echo '{"PRIMARY_CHANNEL":"environment_channel","FILE_CLIENT_ID":"file-client-id","TWITCH_CLIENT_ID":"override-client-id","TWITCH_ACCESS_TOKEN":"override-access-token","LOG_LEVEL":"warn","HEADLESS":"TRUE"}' > c03-env.env.json
  cp "${old}/test/fixtures/config/parse-error.yml" c04-parse-error.config.yml
  cp "${old}/config.example.yml" c05-example.config.yml
  cp c01-valid.config.yml c06-bots.config.yml
  echo '{"TELEGRAM_ENABLED":"true","TELEGRAM_BOT_TOKEN":"123456:test-token","TELEGRAM_ALLOWED_CHAT_IDS":"42, -100123,42","DISCORD_ENABLED":"true","DISCORD_BOT_TOKEN":"discord-token","DISCORD_APPLICATION_ID":"123456789012345678","DISCORD_GUILD_ID":"987654321098765432","DISCORD_ALLOWED_CHANNEL_IDS":"111111111111111111, 222222222222222222,111111111111111111","DISCORD_ALLOW_DIRECT_MESSAGES":"true","DISCORD_ALLOWED_USER_IDS":"333333333333333333,333333333333333333"}' > c06-bots.env.json
  cp c01-valid.config.yml c07-bad-headless.config.yml
  echo '{"HEADLESS":"yes"}' > c07-bad-headless.env.json
  # 已知差異（§6.2）：舊版接受，Rust 版必須回報下列錯誤。
  cp "${old}/test/fixtures/config/valid.yml" c08-headless-false.config.yml
  sed 's/^  engine: chromium$/  engine: firefox/' c01-valid.config.yml > c09-firefox.config.yml
  w known-differences.json <<'J'
{
  "c08-headless-false": {
    "reason": "§6.2：不支援 headless: false",
    "expected": { "error": { "name": "ConfigValidationError", "message": "設定欄位 headless 必須是 true" } }
  },
  "c09-firefox": {
    "reason": "§6.2：只支援 chromium",
    "expected": { "error": { "name": "ConfigValidationError", "message": "設定欄位 browser.engine 必須是 chromium" } }
  }
}
J
fi
ls | wc -l
