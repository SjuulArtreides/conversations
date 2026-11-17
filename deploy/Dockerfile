# syntax=docker/dockerfile:1@sha256:4edf897a3ffa55b89f906fc8cc78afdb3f1834cc9c7083565e611a8a7d5fe99e
# Multi-stage build: React SPA (node) + Rust server, into a minimal runtime image.
# Produces a single image that serves the JSON API AND the built SPA (static).
#
# ARCH: BuildKit selects ARM64 or x86-64 from --platform. Each backend stage
# runs on its target architecture (natively or under configured emulation).
# The server is built as a fully static
# musl binary and shipped on distroless/static — there is no Debian userland in
# the runtime image, so OS-package CVEs (openssl/gnutls/perl/etc.) cannot apply.
# Build both architectures into one registry manifest, for example:
#   docker buildx build --platform linux/amd64,linux/arm64 \
#     -f deploy/Dockerfile -t <registry>/conversations:<version> --push .
# Use an image matching your server's CPU architecture.

# ---------- Stage 1: build the React frontend ----------
FROM --platform=$BUILDPLATFORM node:22-bookworm-slim@sha256:5ec23b7c622b00547753bc6ffd78085dd868a68aeb41c8e0a1418f033e13d96d AS frontend
WORKDIR /app/frontend
# Install deps first (cache layer). `npm ci` is reproducible and fails loudly
# on lockfile drift (the committed package-lock.json is the source of truth).
COPY frontend/package.json frontend/package-lock.json ./
COPY frontend/scripts/patch-xterm-serialize.cjs ./scripts/
RUN npm ci
COPY frontend/ ./
RUN npm run build
# Output: /app/frontend/dist

# ---------- Stage 2: build the Rust server (static musl) ----------
FROM rust:1.96-slim-bookworm@sha256:e18a79fc84dfcfc3ab5ba72290398a644c135c97eaa881447fddc354ee4701a3 AS backend
WORKDIR /app
# musl-tools provides musl-gcc, used both to link the static binary and to
# compile the bundled SQLite C (libsqlite3-sys) and ring's C/asm against musl.
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config musl-tools musl-dev && \
    rm -rf /var/lib/apt/lists/*
# Select musl and request crt-static explicitly for the shell-free runtime.
# Fail explicitly for unsupported architectures instead of building a wrong image.
ARG TARGETARCH
RUN case "$TARGETARCH" in \
      arm64) target=aarch64-unknown-linux-musl ;; \
      amd64) target=x86_64-unknown-linux-musl ;; \
      *) echo "Unsupported target architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac && \
    printf '%s' "$target" > /rust-target && \
    rustup target add "$target"
ENV CC_aarch64_unknown_linux_musl=musl-gcc \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C target-feature=+crt-static" \
    CC_x86_64_unknown_linux_musl=musl-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=cc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C target-feature=+crt-static"
# On x86-64, Rust supplies the self-contained musl startup objects to the
# normal compiler driver. musl-gcc's linker specs interfere with static PIE.
# Copy the workspace and build the server binary in release.
COPY Cargo.toml Cargo.lock* ./
COPY crates/ ./crates/
COPY vendor/ ./vendor/
# reqwest/sqlx use rustls (AWS-LC/ring, no OpenSSL). SQLite is bundled C.
RUN target="$(cat /rust-target)" && \
    cargo build --locked --release --target "$target" --bin conversations-server
# Verify the runtime has no loader dependency, then strip and normalize its path.
RUN target="$(cat /rust-target)" && \
    readelf -l "target/$target/release/conversations-server" > /server-program-headers && \
    if grep -q INTERP /server-program-headers; then \
      echo "Server binary requires a dynamic loader; static runtime cannot run it" >&2; exit 1; \
    fi && \
    strip "target/$target/release/conversations-server" && \
    cp "target/$target/release/conversations-server" /conversations-server

# ---------- Stage 3: runtime ----------
# distroless/static ships only ca-certificates, tzdata, /etc/passwd and a
# nonroot (uid 65532) user — no shell, no package manager, no openssl/gnutls/
# perl. The static binary needs nothing else. k8s overrides the user via
# securityContext (runAsUser: 10001); /data is provided by the mounted PVC.
FROM gcr.io/distroless/static-debian12:nonroot@sha256:afa5c872c891853ca7fcf1f12c3edb23f7eeef36189728842dd51042ff57f7ab AS runtime
WORKDIR /app
COPY --from=backend /conversations-server /usr/local/bin/conversations-server
COPY --from=frontend /app/frontend/dist /app/static
ENV BIND_ADDR=0.0.0.0:8080 \
    DATABASE_URL="sqlite:///data/conversations.db?mode=rwc" \
    STATIC_DIR=/app/static \
    RUST_LOG=info
EXPOSE 8080
# K8s probes: liveness → /healthz (DB-free), readiness → /readyz (SELECT 1).
ENTRYPOINT ["/usr/local/bin/conversations-server"]
