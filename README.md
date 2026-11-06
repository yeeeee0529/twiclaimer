# twiclaimer

在 Docker 裡長時間掛著 Twitch 直播，並自動領取 Bonus Channel Points。可選用 Telegram 或 Discord 遙控。

Keeps Twitch streams open in Docker and claims Bonus Channel Points automatically, with optional Telegram or Discord control.

完整說明 / Full documentation：[繁體中文](docs/zh-TW/README.md) · [English](docs/en/README.md)

## 需求 / Requirements

- Docker Engine 與 Docker Compose plugin
- Twitch 帳號，以及 Twitch Developer App 的 Client ID 與 Client Secret
- 本機匯出登入狀態用：Node.js 24 以上、npm 11

twiclaimer 不會自動登入，也不會要求 Twitch 密碼。你自行登入後匯出 Playwright `storageState`。
It never logs in for you or asks for your password. You log in yourself and export a Playwright `storageState`.

## 快速開始 / Quick start

```bash
# 1. 設定檔 / config
cp config.example.yml config.yml
mkdir -p data/browser-state
chmod 700 data data/browser-state && chmod 600 config.yml
# 編輯 config.yml：channels 與 twitch_api / edit channels and twitch_api

# 2. 匯出登入狀態 / export login state
npm ci
npm run twitch:login

# 3. 啟動 / start
docker compose build
docker compose up -d
docker compose logs -f twitch-watchdog
```

## 狀態 / Status

Rust 版 0.1.0，功能與設定欄位對齊 Node.js 版。差異與升級步驟見 [CHANGELOG.md](CHANGELOG.md)，開發與測試見 [README_DEV.md](README_DEV.md)。
Rust edition 0.1.0, aligned with the Node.js edition. See [CHANGELOG.md](CHANGELOG.md) for differences and upgrade steps, and [README_DEV.md](README_DEV.md) for development.

不支援 Twitch Drops、自動輸入密碼、多帳號、CAPTCHA 繞過與反偵測。
Not supported: Twitch Drops, password automation, multiple accounts, CAPTCHA bypass, anti-detection.
