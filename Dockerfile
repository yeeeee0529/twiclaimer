# syntax=docker/dockerfile:1.7
ARG UBUNTU_IMAGE=ubuntu:noble
ARG RUST_VERSION=1.99.1

# 執行期共用：Playwright 1.62.1 nativeDeps.ts 的 ubuntu24.04 chromium 套件，加上字型
FROM ${UBUNTU_IMAGE} AS runtime-base
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
      libasound2t64 libatk-bridge2.0-0t64 libatk1.0-0t64 libatspi2.0-0t64 libcairo2 libcups2t64 \
      libdbus-1-3 libdrm2 libgbm1 libglib2.0-0t64 libnspr4 libnss3 libpango-1.0-0 libx11-6 libxcb1 \
      libxcomposite1 libxdamage1 libxext6 libxfixes3 libxkbcommon0 libxrandr2 \
      libfontconfig1 libfreetype6 fonts-liberation fonts-noto-color-emoji fonts-ipafont-gothic \
      fonts-wqy-zenhei fonts-tlwg-loma-otf fonts-freefont-ttf fonts-unifont \
 && rm -rf /var/lib/apt/lists/*

FROM runtime-base AS chrome
ARG TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends curl unzip && rm -rf /var/lib/apt/lists/*
COPY scripts/fetch-headless-shell.sh /build/scripts/
COPY chrome/SHA256SUMS /build/chrome/
RUN case "${TARGETARCH}" in amd64) p=linux64 ;; arm64) p=linux-arm64 ;; *) exit 1 ;; esac \
 && exe="$(REQUIRE_SHA256=1 /build/scripts/fetch-headless-shell.sh --dest /opt/headless-shell "${p}")" \
 && ln -s "${exe}" /opt/headless-shell/headless-shell

FROM runtime-base AS build
ARG RUST_VERSION
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl build-essential pkg-config cmake \
 && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:${PATH}
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal \
      --default-toolchain "${RUST_VERSION}" -c rustfmt -c clippy
WORKDIR /src
COPY . .
RUN grep -q "channel = \"${RUST_VERSION}\"" rust-toolchain.toml
# 不掛載 /src/target 快取：陳舊的 thin LTO bitcode 曾使 rustc（LLVM）以 SIGILL 崩潰
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release --locked && cp target/release/twiclaimer /usr/local/bin/twiclaimer

FROM build AS test
COPY --from=chrome /opt/headless-shell /opt/headless-shell
ENV CHROME_PATH=/opt/headless-shell/headless-shell
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/src/target \
    REQUIRE_E2E=1 scripts/check.sh

FROM runtime-base AS production
ARG APP_UID=1000
ARG APP_GID=1000
COPY --from=chrome /opt/headless-shell /opt/headless-shell
COPY --from=build /usr/local/bin/twiclaimer /usr/local/bin/twiclaimer
COPY config.example.yml /app/config.example.yml
RUN ! ldd /opt/headless-shell/headless-shell | grep -q 'not found' \
 && ! ldd /usr/local/bin/twiclaimer | grep -q 'not found' \
 && twiclaimer --version
ENV CONFIG_PATH=/app/config.yml \
    CHROME_PATH=/opt/headless-shell/headless-shell \
    HOME=/tmp XDG_CONFIG_HOME=/tmp/.config XDG_CACHE_HOME=/tmp/.cache
WORKDIR /app
USER ${APP_UID}:${APP_GID}
CMD ["twiclaimer"]
