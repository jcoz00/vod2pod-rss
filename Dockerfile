# syntax=docker/dockerfile:1.7

# -----------------------------------------
# Builder (always on BUILDPLATFORM)
# -----------------------------------------
FROM --platform=$BUILDPLATFORM rust:1.93 AS builder

ARG BUILDPLATFORM
ARG TARGETPLATFORM

ENV CARGO_HOME=/usr/local/cargo
ENV CARGO_TERM_COLOR=always

RUN set -eux; \
  case "$TARGETPLATFORM" in \
    "linux/arm/v7")  echo "armv7-unknown-linux-gnueabihf" > /rust_platform.txt ;; \
    "linux/arm64")   echo "aarch64-unknown-linux-gnu" > /rust_platform.txt ;; \
    "linux/amd64")   echo "x86_64-unknown-linux-gnu" > /rust_platform.txt ;; \
    *) echo "Unsupported TARGETPLATFORM=$TARGETPLATFORM" >&2; exit 1 ;; \
  esac; \
  echo "BUILDPLATFORM=$BUILDPLATFORM TARGETPLATFORM=$TARGETPLATFORM rust_target=$(cat /rust_platform.txt)"

RUN rustup target add "$(cat /rust_platform.txt)"

WORKDIR /src

COPY Cargo.toml Cargo.lock* ./
COPY .cargo/ ./.cargo/

RUN set -eux; \
  if grep -qE 'edition\s*=\s*"\s*2026\s*"' Cargo.toml; then \
    echo "Patching Cargo.toml edition 2026 -> 2024"; \
    sed -i 's/edition[[:space:]]*=[[:space:]]*"2026"/edition = "2024"/' Cargo.toml; \
  fi

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo fetch

COPY src ./src
COPY templates ./templates
COPY set_version.sh version.txt* ./

RUN set -eux; \
  if [ -f ./set_version.sh ]; then sh ./set_version.sh; fi

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release --target "$(cat /rust_platform.txt)"

# Export the built binary to a stable location (no wildcard COPY later)
RUN set -eux; \
  TGT="$(cat /rust_platform.txt)"; \
  install -D "/src/target/${TGT}/release/app" /out/vod2pod

# -----------------------------------------
# Runtime (runs on TARGETPLATFORM)
# -----------------------------------------
FROM --platform=$TARGETPLATFORM debian:bookworm-slim AS app

ARG TARGETPLATFORM
ENV DEBIAN_FRONTEND=noninteractive

# Runtime deps + python/pip kept for the updater sidecar
RUN set -eux; \
  apt-get update; \
  apt-get install -y --no-install-recommends \
    ca-certificates \
    ffmpeg \
    python3 \
    python3-pip \
    curl \
    unzip \
  ; \
  rm -rf /var/lib/apt/lists/*

# Install yt-dlp via pip (sidecar will keep it updated)
RUN set -eux; \
  python3 -m pip install --no-cache-dir -U pip --break-system-packages; \
  python3 -m pip install --no-cache-dir -U yt-dlp --break-system-packages; \
  yt-dlp --version; \
  python3 -m yt_dlp --version

# Install Deno appropriate for TARGETPLATFORM
RUN set -eux; \
  case "$TARGETPLATFORM" in \
    "linux/amd64")  DENO_ZIP="deno-x86_64-unknown-linux-gnu.zip" ;; \
    "linux/arm64")  DENO_ZIP="deno-aarch64-unknown-linux-gnu.zip" ;; \
    "linux/arm/v7") DENO_ZIP="deno-armv7-unknown-linux-gnueabihf.zip" ;; \
    *) echo "Unsupported TARGETPLATFORM=$TARGETPLATFORM for deno" >&2; exit 1 ;; \
  esac; \
  curl -fsSL "https://github.com/denoland/deno/releases/latest/download/${DENO_ZIP}" -o /tmp/deno.zip; \
  unzip /tmp/deno.zip -d /usr/local/bin; \
  rm -f /tmp/deno.zip; \
  deno --version

# Rumble-friendly ffmpeg wrapper (NO heredoc; no Dockerfile parse issues)
RUN set -eux; \
  mv /usr/bin/ffmpeg /usr/bin/ffmpeg.real; \
  printf '%s\n' \
'#!/bin/sh' \
'set -eu' \
'ARGS="$*"' \
'if echo "$ARGS" | grep -Eiq "(rumble\.com|rmbl\.ws|sp\.rmbl\.ws)"; then' \
'  exec /usr/bin/ffmpeg.real -headers "Referer: https://rumble.com" -headers "Origin: https://rumble.com" "$@"' \
'else' \
'  exec /usr/bin/ffmpeg.real "$@"' \
'fi' \
  > /usr/local/bin/ffmpeg; \
  chmod 0755 /usr/local/bin/ffmpeg

COPY --from=builder /out/vod2pod /usr/local/bin/vod2pod
COPY --from=builder /src/templates/ /templates/

RUN set -eux; \
  /usr/local/bin/vod2pod --version >/dev/null 2>&1 || true; \
  deno --version >/dev/null 2>&1 || true; \
  python3 -m pip --version >/dev/null 2>&1 || true; \
  yt-dlp --version >/dev/null 2>&1 || true; \
  python3 -m yt_dlp --version >/dev/null 2>&1 || true

EXPOSE 8080
CMD ["/usr/local/bin/vod2pod"]
