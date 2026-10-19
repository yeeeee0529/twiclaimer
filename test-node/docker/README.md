# Docker smoke test / Docker 煙霧測試

執行 / Run:

```bash
./scripts/docker-smoke.sh
```

測試會：

1. 驗證正式與 smoke Compose 合併結果。
2. 建置 `smoke-test` target，於映像內執行 lint、build、Vitest 與 Playwright E2E。
3. 建置 production image，確認只含 production dependencies、`dist` 與必要 metadata。
4. 確認 image 不含 `config.yml`、`.env`、storageState、原始碼或測試。
5. 確認缺少 `/app/config.yml` 時 production command 會以非零狀態清楚失敗。
6. 使用測試專用 Compose override 啟動服務，確認 `config.yml` 對非 root 使用者可寫。
7. 重啟服務並確認 SIGTERM 會輸出 `service_stopped`。
8. 驗證 Compose `mem_limit` / `memswap_limit` / `pids_limit`，並在 cgroup v2 可用時讀取容器內 `memory.max` 與 `pids.max`。
9. 驗證 Compose 使用 `local` logging driver，並將每個 log 檔限制為 `10m`、最多保留 `5` 個檔案。

Smoke Compose 使用假憑證、空 storageState 與 `network_mode: none`。應用程式無法連線 Helix 或 Twitch 網站，因此測試不依賴也不會存取正式服務。

The test performs the following checks:

1. Validates the merged production and smoke Compose configurations.
2. Builds the `smoke-test` target and runs lint, build, Vitest, and Playwright E2E inside the image.
3. Builds the production image and verifies that it contains only production dependencies, `dist`, and required metadata.
4. Confirms that the image excludes `config.yml`, `.env`, storageState, source files, and tests.
5. Confirms that the production command exits clearly with a non-zero status when `/app/config.yml` is missing.
6. Starts the service with the smoke-only Compose override and confirms that `config.yml` is writable by the non-root user.
7. Restarts the service and confirms that SIGTERM emits `service_stopped`.
8. Validates Compose `mem_limit`, `memswap_limit`, and `pids_limit`; when cgroup v2 is available, it also reads `memory.max` and `pids.max` inside the container.
9. Validates the Compose `local` logging driver with a `10m` per-file limit and at most `5` retained files.

The smoke Compose configuration uses fake credentials, an empty storageState, and `network_mode: none`. The application cannot reach Helix or Twitch, so the test neither depends on nor accesses production services.
