# twiclaimer documentation

[繁體中文](../zh-TW/README.md) · [Quick overview](../../README.md) · [Developer guide](../../README_DEV.md) · [Changelog](../../CHANGELOG.md)

twiclaimer is a Docker-friendly service that keeps Twitch channels open and claims Bonus Channel Points for you (the Rust edition of Twitch Watchdog). It checks the Twitch API to see which of your channels are live, opens watch pages in your configured priority order, and claims Bonus Channel Points automatically.

It never logs in to Twitch for you and never asks for your Twitch password. You log in yourself, export a Playwright `storageState`, and supply the Client ID and Client Secret of a Twitch Developer App.

Run every command below from the project root.

## Contents

- [Features](#features)
- [Requirements](#requirements)
- [Quick start](#quick-start)
- [Resource guard and Docker limits](#resource-guard-and-docker-limits)
- [Twitch API setup](#twitch-api-setup)
- [Export your Twitch login state](#export-your-twitch-login-state)
- [Run and operate](#run-and-operate)
- [Telegram control](#telegram-control)
- [Discord control](#discord-control)
- [Troubleshooting](#troubleshooting)
- [Safety and limits](#safety-and-limits)

## Features

- Monitors several Twitch channels and watches the ones that `channels` order and `max_concurrent_streams` select.
- Plays streams in Chromium at `160p`, muted by default, to keep long-running resource use low.
- Claims Bonus Channel Points automatically.
- Accepts channel content warnings (`Start Watching`) by clicking the visible confirmation for the logged-in account.
- Collapses the recommended-channel sidebar after startup and page reloads.
- After 10 consecutive claim failures, refreshes that channel page. If 10 more fail after the refresh, the process exits and the Docker restart policy restarts the container.
- If a scheduler check stays in flight too long, the container-level watchdog logs `scheduler_stall_detected` and exits so Docker restarts the container.
- Optional Telegram or Discord bot: query status and channel-point balances, manage channels, pause or resume checks, and capture screenshots.

Not supported: Twitch Drops claiming, automatic password entry, multi-account management, CAPTCHA bypass, anti-detection, and evading platform restrictions.

## Requirements

- Docker Engine and the Docker Compose plugin
- A Twitch account
- A Client ID and Client Secret from an app created in the Twitch Developer Console
- To export login state locally: Node.js 24 or later and npm 11

The production image is based on `ubuntu:noble` and ships the Chromium headless shell `151.0.7922.34`, the same build Playwright `1.62.1` uses. It contains no Node.js, source code, or tests.

## Quick start

Create the config file and the storageState directory:

```bash
cp config.example.yml config.yml
mkdir -p data/browser-state
chmod 700 data data/browser-state
chmod 600 config.yml
```

Edit `config.yml`:

```yaml
channels:
  - streamer_one
  - streamer_two

check_interval_seconds: 60
max_concurrent_streams: 1
headless: true
storage_state_path: /data/browser-state/storage-state.json
log_level: info

twitch_api:
  client_id: your-client-id
  client_secret: your-client-secret

browser:
  engine: chromium
  stream_quality: 160p
  page_refresh_interval_seconds: 0
  resource_telemetry_interval_seconds: 60

telegram:
  enabled: false

discord:
  enabled: false
```

### Key settings

| Field | Description |
| --- | --- |
| `channels` | Twitch login names. The order is the watch priority |
| `check_interval_seconds` | Twitch API polling interval, minimum 30 seconds |
| `max_concurrent_streams` | Maximum number of streams watched at once |
| `storage_state_path` | Path of the Playwright storageState inside the container |
| `browser.engine` | Only `chromium` is supported. `firefox` fails config validation (`ConfigValidationError`) |
| `headless` | Only `true` is supported. `false`, including a `HEADLESS=false` override, fails config validation |
| `browser.stream_quality` | Defaults to `160p`. Set `auto` to stop forcing a quality |
| `browser.page_refresh_interval_seconds` | Defaults to `0`: no scheduled refresh, which lowers browser memory pressure. A positive value enables staggered per-channel refresh. The feature stays because Twitch may show a claimable button only after a reload. Manual `/refresh_now` is unaffected |
| `browser.resource_telemetry_interval_seconds` | Emits `runtime_resource_snapshot` (with cgroup fields) every 60 seconds by default |
| `browser.resource_guard` | Container-wide memory guard, described in the next section |

If session startup still ends in a `page.goto` timeout after its short retry, two failures for one channel, or three across all channels, within five minutes recycle the shared browser. Three such recoveries within ten minutes request a Docker container restart. Timeouts from scheduled or manual reloads are not counted.

## Resource guard and Docker limits

The Docker Compose defaults are sized for about three concurrent streams:

| Setting | Value | Meaning |
| --- | --- | --- |
| `mem_limit` | 6g | Container RAM limit |
| `memswap_limit` | 7g | Combined RAM and swap limit (about 1g of swap) |
| `pids_limit` | 512 | Process and thread limit |

If `max_concurrent_streams` is well above 3 (for example 5), raise the memory limits to about 10g/11g and recreate the container.

`browser.resource_guard` measures the whole container's cgroup memory, browser included, not just the twiclaimer controller process. The MiB values in YAML are anchors for `baseline_streams` (default 3). With `scale_with_streams: true` they scale as:

```text
effective = base_memory_mib + (anchor - base_memory_mib) * (max_concurrent_streams / baseline_streams)
```

Default behavior:

- Warning threshold reached: logs `resource_guard_warning` (with hysteresis).
- Recycle threshold reached on consecutive samples: recycles the browser (`resource_guard_browser_recycle_requested`).
- Emergency memory, swap, or cgroup OOM events: logs, then exits with code 1 so Docker restarts the container.
- While a browser recycle is in flight, emergency memory, swap, and cgroup OOM signals stay active. Ordinary warning, recycle, and growth-rate rules pause so session refill is not misread.
- `fast_memory_growth` fires only when current memory is already at or above the warning level and growth within the window exceeds the threshold. After a browser restart, the rule is ignored for 120 seconds.
- Without cgroup v2, only `cgroup_metrics_unavailable` is logged and Docker's hard limits remain the protection.

Query cgroup telemetry:

```bash
docker compose logs --no-log-prefix twitch-watchdog \
  | rg 'runtime_resource_snapshot|resource_guard_|cgroup_metrics_unavailable|container_restart_requested'
```

## Twitch API setup

Create an application in the Twitch Developer Console, copy its Client ID and Client Secret, and put them in `config.yml`:

```yaml
twitch_api:
  client_id: your-client-id
  client_secret: your-client-secret
```

The service uses the Client Credentials flow to obtain an App Access Token. It fetches a new one when the token is invalid, close to expiry, or the Helix API returns HTTP 401. The token lives only in process memory and is never written to `config.yml`.

`twitch_api.access_token` is a manual fallback for when no Client Secret is available.

## Export your Twitch login state

The login helper is the Node.js tool `scripts/twitch-login.mjs`. Run it on a local desktop machine. It works like this:

1. Detects your default browser.
2. Opens it with an isolated temporary profile. No remote-debugging or Playwright flags are used during login.
3. You log in by hand, then press Enter in the terminal.
4. The tool relaunches the same temporary profile briefly and exports the Playwright `storageState` over CDP.

It never touches your everyday browser profile.

Install the local dependencies:

```bash
npm ci
mkdir -p data/browser-state
chmod 700 data data/browser-state
```

If the system has no Chrome or Edge, install Playwright Firefox as a fallback:

```bash
npx playwright install firefox
```

Run the helper:

```bash
npm run twitch:login
```

Log in to Twitch in the window that opens, including 2FA. Once the tool sees `auth-token` it writes `data/browser-state/storage-state.json` and sets its permissions. You can also press Enter in the terminal to save immediately.

Browser order:

1. The system default browser, when Playwright can drive it (for example Chrome, Edge, Firefox)
2. Google Chrome
3. Microsoft Edge
4. System Firefox
5. Playwright Firefox

Playwright cannot drive Safari, so the helper falls back to the browsers above if Safari is the default. Force one with `--browser chrome|msedge|firefox`.

Advanced fallback (not recommended as the default flow):

```bash
npx playwright codegen \
  --browser=firefox \
  --save-storage=data/browser-state/storage-state.json \
  https://www.twitch.tv/
```

`storage-state.json` is equivalent to a Twitch login credential. Do not commit it, bake it into a Docker image, attach it to an issue, or share it. If you suspect it leaked, sign out of all devices or revoke the session on Twitch, then export again.

## Run and operate

Check the required files first:

```bash
test -r config.yml
test -r data/browser-state/storage-state.json
docker compose config
```

Build and start:

```bash
docker compose build
docker compose up -d
docker compose logs -f twitch-watchdog
```

`docker compose build` creates the in-container non-root user from `APP_UID`/`APP_GID` (default 1000). If the host `config.yml` is not owned by 1000:1000, add `--build-arg APP_UID=<uid> --build-arg APP_GID=<gid>` so bot config writeback can persist changes.

Compose mounts `config.yml` as a writable bind mount so Telegram and Discord management commands can save settings; `data/browser-state` stays read-only. `config.yml` must be writable by the `APP_UID`/`APP_GID` user. Use owner or group permissions, not world-writable permissions.

Common operations:

```bash
docker compose restart twitch-watchdog
docker compose stop
docker compose down
```

### Logs

Service logs are JSON Lines on stdout. View or retain them through Docker logs:

```bash
docker compose logs -f twitch-watchdog
docker compose logs --no-log-prefix twitch-watchdog
```

To diagnose stuck scheduler ticks, page crashes, or browser cleanup, set `log_level` to `debug` in `config.yml` and restart:

```bash
docker compose logs --no-log-prefix twitch-watchdog \
  | rg 'scheduler_tick_|scheduler_stall_detected|session_(reconcile|invalidate|start_attempt)|browser_(page_invalidation|resource_close|navigation_failure)|page_crashed|page_closed|page_refresh_failed'
```

### Browser diagnostic events

Every managed channel page installs request and console diagnostics, which emit three events:

| Event | Level | Content |
| --- | --- | --- |
| `browser_request_failed` | warn / debug | A `requestfailed` with `endpointCategory`, `host`, `path`, `method`, `resourceType`, `failureText`, and `graphQlOperationNames` for GraphQL requests. Third-party tracking and analytics (`third_party`, `twitch_other`, `unknown`) and browser-aborted requests (`net::ERR_ABORTED`, for example segments cancelled by a quality switch) log at debug. Other Twitch core endpoint failures log at warn |
| `browser_http_response` | warn / debug | Non-success HTTP responses log at warn. With `log_level: debug`, successful Twitch bootstrap responses (document, script, GraphQL, API) log at debug. Routine media segments, images, fonts, and third-party analytics are not logged |
| `browser_console_message` | warn / debug | Console `error` logs at warn. Errors from routine-noise hosts (`third_party`, `twitch_other`, `unknown`) or matching known noise (such as `SpadeClient send error`, `failed integrity check`) log at debug. `warning` logs at debug. Ordinary `log` messages are ignored |

All events carry `channel`, `browserGeneration`, and `pageGeneration`. URLs keep only the hostname and pathname; query strings, cookies, request and response bodies, and GraphQL variables are deliberately excluded.

```bash
docker compose logs --no-log-prefix twitch-watchdog \
  | rg 'browser_(request_failed|http_response|console_message)|page_health_failed|browser_disconnected'
```

## Telegram control

The Telegram integration uses Bot API long polling and needs no exposed HTTP port.

1. Talk to [@BotFather](https://t.me/BotFather) on Telegram, create a bot with `/newbot`, and copy the token.
2. Send any message to your bot.
3. Call the Bot API `getUpdates` method to read `message.chat.id`.
4. Configure `config.yml`:

```yaml
telegram:
  enabled: true
  bot_token: your-bot-token
  allowed_chat_ids:
    - "your-chat-id"
```

Chat IDs must be quoted integer strings such as `"5009748887"` or `"-1001234567890"`, not `@username`. Only listed chats can query or control the service.

### Commands

| Command | Description |
| --- | --- |
| `/status` | Service, live, and watch status |
| `/channels` | Monitored channels and their latest status |
| `/refresh` | Player refresh countdown for watched channels |
| `/refresh_now [channel]` | Refresh all watched channels now, or one channel |
| `/points [channel]` | Channel-point balances for all watched channels, or one channel |
| `/config` | Current channels and maximum concurrent streams |
| `/channel_add channel` | Add a monitored channel |
| `/channel_remove channel` | Remove a monitored channel |
| `/channels_set a,b` | Replace the whole channel list |
| `/max_streams count` | Set the maximum concurrent streams |
| `/check` | Check Twitch status now |
| `/pause` | Pause automatic checks |
| `/resume` | Resume automatic checks |
| `/screenshot [channel]` | Screenshots of all watched channels, or one channel |
| `/help` | Show command help |

`/points` and `/screenshot` run in the background so `/status` and polling stay responsive. At most two run at once. After 30 seconds the bot replies with a timeout, and the query keeps its slot until the underlying page operation finishes, which prevents retry buildup. Late results are dropped after a timeout or shutdown. If busy replies persist, check `telegram_query_timeout` and the page-operation logs.

Polling has a network deadline of `polling_timeout_seconds` plus 10 seconds. A timeout logs `telegram_poll_failed` and retries with the existing backoff.

### Proactive pushes

Service start and stop, stream online and offline, reward claimed or click failure, page crash, browser restart, and an upcoming container restart (fatal path). Scheduled and manual refreshes do not push.

A bot token grants management capability. Do not commit it or post it publicly.

## Discord control

The Discord integration receives slash commands through the Gateway WebSocket and replies through the REST API. No exposed HTTP port is required.

1. In the Discord Developer Portal, create an Application, add a Bot, and copy the bot token.
2. In the OAuth2 URL Generator, select `bot` and `applications.commands`. The bot needs at least `Send Messages` and `Attach Files`.
3. Invite the bot to your server, then copy the Application ID and the IDs of the channels allowed to use commands.
4. Configure `config.yml`:

```yaml
discord:
  enabled: true
  bot_token: your-discord-bot-token
  application_id: your-application-id
  guild_id: your-guild-id
  allowed_channel_ids:
    - "your-channel-id"
  allow_direct_messages: false
  allowed_user_ids: []
```

`guild_id` is optional. When set, slash commands are registered as guild commands and usually appear sooner. When empty, they are global commands and can take longer to appear.

Channel IDs must be quoted numeric snowflake IDs such as `"1520430059771400353"`, not `#general`. Only listed channels can query or control the service.

To allow direct messages, set `allow_direct_messages: true` and list authorized user IDs in `allowed_user_ids`:

```yaml
discord:
  allow_direct_messages: true
  allowed_user_ids:
    - "your-discord-user-id"
```

User IDs must also be quoted numeric snowflake IDs. Direct messages are accepted only from listed users, and the server channel allowlist still applies.

### Slash commands

The commands match Telegram's. Arguments are passed as `name:value`:

| Command | Description |
| --- | --- |
| `/status`, `/channels`, `/refresh`, `/config`, `/check`, `/pause`, `/resume`, `/help` | Same as Telegram |
| `/refresh_now [channel:name]` | Refresh all watched channels now, or one channel |
| `/points [channel:name]` | Channel-point balances |
| `/channel_add channel:name` | Add a monitored channel |
| `/channel_remove channel:name` | Remove a monitored channel |
| `/channels_set channels:a,b` | Replace the whole channel list |
| `/max_streams count:number` | Set the maximum concurrent streams |
| `/screenshot [channel:name]` | Screenshots of watched channels |

Proactive pushes are the same as on Telegram. A Discord bot token grants management capability. Do not commit it or post it publicly.

## Troubleshooting

### Login failures

- `page_health_failed.reason: login_missing` means the page shows the ordinary login button. `login_required` means an explicit login gate. Neither counts as healthy viewing or proves points are accruing. Player errors and offline states keep their own reasons.
- A login failure does not trigger an immediate reload. The existing consecutive-failure threshold still closes the page.
- Session invalidation caused by health failures reuses `browser.session_start.failure_backoff_seconds` (default `[30, 60, 120]`), then retries after `maximum_cooldown_seconds` (default 900 seconds). The actual retry time depends on the next scheduler tick. It shares the failure counter with startup failures. Opening a page briefly does not reset it; staying stable for `browser.recovery.stable_reset_seconds` does.
- `channel_health_recovery_backoff` reports the channel, accumulated failures, and the next eligible retry time `retryAtMs`. This path is not a page crash and triggers neither crash notifications nor browser crash recovery.

If login failures persist, run `npm run twitch:login` locally to log in and export the state again, put the file back where the deployment mounts it, and restart the service. HTTP 429 still logs at warn. Backoff cannot lift Twitch rate limits or restore authentication.

## Safety and limits

- Use only with Twitch accounts you own or are authorized to use.
- The tool does not guarantee that any particular usage complies with platform rules.
- Do not treat content from Twitch or other untrusted sites as trusted input.
- Twitch's DOM and channel-point selectors can change. If a button cannot be found, the service keeps watching and waits for the next check.
