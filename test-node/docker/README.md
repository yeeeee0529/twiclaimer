# Docker smoke test / Docker 煙霧測試

執行 / Run:

```bash
./scripts/docker-smoke.sh
```

測試會：

1. 驗證正式與 smoke Compose 合併結果（含 `local` logging driver、每檔 `10m`、最多 `5` 個檔案）。
2. 建置 `test` target：容器內執行 `REQUIRE_E2E=1 scripts/check.sh`（fmt、clippy、全量測試、e2e、port-coverage）。
3. 建置 production image，確認只含 `twiclaimer` binary、headless shell、`config.example.yml` 與系統套件。
4. 確認 image 不含 `config.yml`、`.env`、storageState、原始碼或測試。
5. 確認缺少 `/app/config.yml` 時 production command 會以非零狀態輸出 `config_error` 清楚失敗。
6. 使用測試專用 Compose override 啟動服務，確認 `config.yml` 對非 root 使用者可寫。
7. 重啟服務並確認 SIGTERM 會輸出 `service_stopped`。
8. 驗證 Compose `mem_limit` / `memswap_limit` / `pids_limit`，並在 cgroup v2 可用時讀取容器內 `memory.max` 與 `pids.max`。
9. 驗證 Compose 使用 `local` logging driver，並將每個 log 檔限制為 `10m`、最多保留 `5` 個檔案。
10. 確認停止之後容器優雅退出（SIGTERM 後沒有殘留的 headless shell 程序；若有殘留會被 SIGKILL 成非 0 exit code）。
11. 輸出正式 image 大小（`docker save` 經 `gzip -1` 壓縮後的位元組數，A/B 比較的量法）。

Smoke Compose 使用假憑證、空 storageState 與 `network_mode: none`。應用程式無法連線 Helix 或 Twitch 網站，因此測試不依賴也不會存取正式服務。

The test performs the following checks:

1. Validates the merged production and smoke Compose configurations (including the `local` logging driver with a `10m` per-file limit and at most `5` retained files).
2. Builds the `test` target, which runs `REQUIRE_E2E=1 scripts/check.sh` inside the image (fmt, clippy, full test suite, e2e, port-coverage).
3. Builds the production image and verifies that it contains only the `twiclaimer` binary, the headless shell, `config.example.yml`, and system packages.
4. Confirms that the image excludes `config.yml`, `.env`, storage state, source files, and tests.
5. Confirms that the production command exits clearly with a non-zero status and a `config_error` event when `/app/config.yml` is missing.
6. Starts the service with the smoke-only Compose override and confirms that `config.yml` is writable by the non-root user.
7. Restarts the service and confirms that SIGTERM emits `service_stopped`.
8. Validates Compose `mem_limit`, `memswap_limit`, and `pids_limit`; when cgroup v2 is available, it also reads `memory.max` and `pids.max` inside the container.
9. Validates the Compose `local` logging driver with a `10m` per-file limit and at most `5` retained files.
10. Confirms the container exits cleanly after stop (no leftover headless-shell processes after SIGTERM; leftovers would be SIGKILLed into a non-zero exit code).
11. Prints the production image size (bytes of `docker save` compressed with `gzip -1`, the A/B comparison metric).

The smoke Compose configuration uses fake credentials, an empty storageState, and `network_mode: none`. The application cannot reach Helix or Twitch, so the test neither depends on nor accesses production services.
