# syntax=docker/dockerfile:1.7

# -----------------------------------------
# Builder (always on BUILDPLATFORM)
# -----------------------------------------
FROM --platform=$BUILDPLATFORM rust:1.93 AS builder

ARG BUILDPLATFORM
ARG TARGETPLATFORM

ENV CARGO_HOME=/usr/local/cargo
ENV CARGO_TERM_COLOR=always

# Determine Rust target triple for the requested TARGETPLATFORM
RUN set -eux; \
  case "$TARGETPLATFORM" in \
    "linux/arm/v7")  echo "armv7-unknown-linux-gnueabihf" > /rust_platform.txt ;; \
    "linux/arm64")   echo "aarch64-unknown-linux-gnu" > /rust_platform.txt ;; \
    "linux/amd64")   echo "x86_64-unknown-linux-gnu" > /rust_platform.txt ;; \
    *) echo "Unsupported TARGETPLATFORM=$TARGETPLATFORM" >&2; exit 1 ;; \
  esac; \
  echo "BUILDPLATFORM=$BUILDPLATFORM TARGETPLATFORM=$TARGETPLATFORM rust_target=$(cat /rust_platform.txt)"

# Add target stdlib
RUN rustup target add "$(cat /rust_platform.txt)"

WORKDIR /src

# Copy only manifests first (better caching)
COPY Cargo.toml Cargo.lock* ./
COPY .cargo/ ./.cargo/

# If the repo ever ends up with edition="2026" (which Cargo doesn't support),
# patch it to edition="2024" so builds won't explode.
RUN set -eux; \
  if grep -qE 'edition\s*=\s*"\s*2026\s*"' Cargo.toml; then \
    echo "Patching Cargo.toml edition 2026 -> 2024"; \
    sed -i 's/edition[[:space:]]*=[[:space:]]*"2026"/edition = "2024"/' Cargo.toml; \
  fi

# Fetch deps with cache mounts
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo fetch

# Now copy the rest of the source
COPY src ./src
COPY templates ./templates
COPY set_version.sh version.txt* ./

# Set version (if your project uses this)
RUN set -eux; \
  if [ -f ./set_version.sh ]; then sh ./set_version.sh; fi

# Build with cache mounts
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --target "$(cat /rust_platform.txt)"

# -----------------------------------------
# Runtime (runs on TARGETPLATFORM)
# -----------------------------------------
FROM --platform=$TARGETPLATFORM debian:bookworm-slim AS app

ARG TARGETPLATFORM
ENV DEBIAN_FRONTEND=noninteractive

# Core runtime deps
RUN set -eux; \
  apt-get update; \
  apt-get install -y --no-install-recommends \
    ca-certificates \
    ffmpeg \
    curl \
    unzip \
  ; \
  rm -rf /var/lib/apt/lists/*

# Install yt-dlp as a standalone binary (no pip / no PEP668 drama)
RUN set -eux; \
  curl -fsSL -o /usr/local/bin/yt-dlp \
    "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp"; \
  chmod 0755 /usr/local/bin/yt-dlp; \
  /usr/local/bin/yt-dlp --version

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

# Rumble-friendly ffmpeg wrapper (FIXED: single RUN + properly terminated heredoc)
RUN set -eux; \
  mv /usr/bin/ffmpeg /usr/bin/ffmpeg.real; \
  cat > /usr/local/bin/ffmpeg <<'EOF'
#!/bin/sh
set -eu
ARGS="$*"
if echo "$ARGS" | grep -Eiq '(rumble\.com|rmbl\.ws|sp\.rmbl\.ws)'; then
  exec /usr/bin/ffmpeg.real -headers "Referer: https://rumble.com" -headers "Origin: https://rumble.com" "$@"
else
  exec /usr/bin/ffmpeg.real "$@"
fi
EOF
  chmod 0755 /usr/local/bin/ffmpeg

# Copy built app + templates
COPY --from=builder /src/target/*/release/app /usr/local/bin/vod2pod
COPY --from=builder /src/templates/ /templates/

RUN set -eux; \
  /usr/local/bin/vod2pod --version >/dev/null 2>&1 || true; \
  deno --version >/dev/null 2>&1 || true; \
  yt-dlp --version >/dev/null 2>&1 || true

EXPOSE 8080
CMD ["/usr/local/bin/vod2pod"]
