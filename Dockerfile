# syntax=docker/dockerfile:1.7

# -----------------------------------------
# Builder (build on Bookworm to match runtime glibc)
# -----------------------------------------
FROM --platform=$BUILDPLATFORM rust:1.93-bookworm AS builder

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

# copy the built binary to a stable path (avoid wildcard COPY surprises)
RUN set -eux; \
  BIN="/src/target/$(cat /rust_platform.txt)/release/app"; \
  test -f "$BIN"; \
  cp "$BIN" /out/vod2pod

# -----------------------------------------
# Runtime (Bookworm)
# -----------------------------------------
FROM --platform=$TARGETPLATFORM debian:bookworm-slim AS app

ARG TARGETPLATFORM
ENV DEBIAN_FRONTEND=noninteractive

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

# install yt-dlp via pip (so your sidecar can keep it updated)
RUN set -eux; \
  python3 -m pip install --no-cache-dir --break-system-packages -U pip; \
  python3 -m pip install --no-cache-dir --break-system-packages -U yt-dlp

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
  rm -f /tmp/deno.zip

# Rumble-friendly ffmpeg wrapper (single RUN so Dockerfile parses correctly)
RUN set -eux; \
  mv /usr/bin/ffmpeg /usr/bin/ffmpeg.real; \
  cat > /usr/local/bin/ffmpeg <<'EOF'; \
#!/bin/sh
set -eu
ARGS="$*"
if echo "$ARGS" | grep -Eiq '(rumble\.com|rmbl\.ws|sp\.rmbl\.ws)'; then
  exec /usr/bin/ffmpeg.real -headers "Referer: https://rumble.com" -headers "Origin: https://rumble.com" "$@"
else
  exec /usr/bin/ffmpeg.real "$@"
fi
EOF \
  chmod 0755 /usr/local/bin/ffmpeg

COPY --from=builder /out/vod2pod /usr/local/bin/vod2pod
COPY --from=builder /src/templates/ /templates/

EXPOSE 8080
CMD ["/usr/local/bin/vod2pod"]
