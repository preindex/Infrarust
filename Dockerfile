# syntax=docker/dockerfile:1
ARG RUST_VERSION=1.99.0
ARG ALPINE_VERSION=3.24
ARG NODE_VERSION=24.21.0

# Stage 1: Build the admin frontend
FROM docker.io/library/node:${NODE_VERSION}-alpine${ALPINE_VERSION} AS frontend-builder

WORKDIR /frontend
COPY plugins/infrarust-plugin-admin-api/frontend/ ./
RUN yarn install --frozen-lockfile
RUN yarn generate

# Stage 2: Build Rust binary
FROM docker.io/library/rust:${RUST_VERSION}-alpine${ALPINE_VERSION} AS builder

RUN apk add --no-cache \
    musl-dev \
    pkgconfig \
    openssl-dev \
    openssl-libs-static \
    git \
    build-base

WORKDIR /usr/crates/infrarust

COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY plugins/ ./plugins/
COPY tools/ ./tools/

COPY --from=frontend-builder /frontend/.output/public/ \
     ./plugins/infrarust-plugin-admin-api/frontend/.output/public/

ENV OPENSSL_STATIC=1
ENV RUSTFLAGS="-C target-feature=+crt-static"

# Enable the native SVC plugin for the packaged image; standalone builds opt in separately.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/usr/crates/infrarust/target \
    ARCH=$(uname -m) && \
    case "$ARCH" in \
        x86_64) \
            TARGET="x86_64-unknown-linux-musl" \
            ;; \
        aarch64) \
            TARGET="aarch64-unknown-linux-musl" \
            ;; \
        armv7l) \
            TARGET="armv7-unknown-linux-musleabihf" \
            ;; \
        *) \
            echo "Unsupported architecture: $ARCH" && exit 1 \
            ;; \
    esac && \
    echo "Building for target: $TARGET on architecture: $ARCH" && \
    rustup target add "$TARGET" && \
    cargo build --release --target "$TARGET" -p infrarust --features "docker,default-plugins,plugin-svc" && \
    cp "target/$TARGET/release/infrarust" /usr/local/bin/infrarust && \
    echo "Build completed successfully"

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/usr/crates/infrarust/plugins/infrarust-plugin-floodgate/wasm/target \
    rustup target add wasm32-wasip2 && \
    cargo build --release \
      --manifest-path plugins/infrarust-plugin-floodgate/wasm/Cargo.toml \
      --target wasm32-wasip2 && \
    install -D -m 0644 \
      plugins/infrarust-plugin-floodgate/wasm/target/wasm32-wasip2/release/infrarust_plugin_floodgate_wasm.wasm \
      /usr/local/share/infrarust/plugins/floodgate.wasm

RUN mkdir -p /image-root/app/cache/wasm && \
    mkdir -p /image-root/app/plugins/floodgate && \
    chown -R 65532:65532 /image-root/app/plugins && \
    chown -R 65532:65532 /image-root/app/cache && \
    chmod 0700 /image-root/app/cache /image-root/app/cache/wasm

# Stage 3: Combined Infrarust + Geyser runtime
FROM docker.io/library/eclipse-temurin:21-jre-alpine AS runtime

RUN apk add --no-cache bash \
    && mkdir -p /app/config /app/geyser /app/plugins/floodgate /app/cache/wasm /opt/geyser \
    && chown -R 65532:65532 /app /opt/geyser

ARG GEYSER_VERSION=2.12.0
ARG GEYSER_BUILD=1271
ARG GEYSER_SHA256=df30afb9970f3a71291ee6c96b5fb21e59fbf6d48a3c0c3f4877fcb096c4e314

RUN apk add --no-cache curl \
    && curl -fsSL \
      "https://download.geysermc.org/v2/projects/geyser/versions/${GEYSER_VERSION}/builds/${GEYSER_BUILD}/downloads/standalone" \
      -o /opt/geyser/Geyser-Standalone.jar \
    && echo "${GEYSER_SHA256}  /opt/geyser/Geyser-Standalone.jar" | sha256sum -c - \
    && apk del curl \
    && chown 65532:65532 /opt/geyser/Geyser-Standalone.jar \
    && chmod 0700 /app/cache /app/cache/wasm

COPY --from=builder /usr/local/bin/infrarust /sbin/infrarust
COPY --from=builder /image-root/app /app
COPY --from=builder /usr/local/share/infrarust/plugins/floodgate.wasm /app/plugins/floodgate.wasm
COPY Dockerfile.combined-entrypoint.sh /usr/local/bin/container-entrypoint

RUN chmod 0755 /usr/local/bin/container-entrypoint \
    && chown -R 65532:65532 /app /opt/geyser

WORKDIR /app
VOLUME ["/app/config", "/app/geyser"]
EXPOSE 25565/tcp 25565/udp 19132/udp 8080/tcp

USER 65532:65532

ENTRYPOINT ["/usr/local/bin/container-entrypoint"]
CMD ["--config", "/app/config/infrarust.toml", "--plugins-dir", "/app/plugins", "--servers-dir", "/app/config/servers"]
