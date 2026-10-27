---
title: Docker Deployment
description: Run Infrarust in Docker with volume mounts, exposed ports, environment variables, Docker Compose, and automatic server discovery from container labels.
---

# Docker Deployment

The runtime image contains the Infrarust proxy and a pinned Geyser Standalone process. Both run as uid `65532`; Geyser configuration and Floodgate keys are supplied through runtime mounts. Geyser starts automatically when its config is mounted; without that file, the image runs Infrarust only. Published images support x86_64 and aarch64.

The same image is published to two registries, with the same tags:

| Registry | Image |
|----------|-------|
| GitHub Container Registry | `ghcr.io/shadowner/infrarust` |
| Docker Hub | `shadowner/infrarust` |

Each release is tagged with its version (for example `2.0.0`) and with `latest`. The examples below use the GitHub image; the Docker Hub one works the same.

## Exposed ports

The image exposes these ports:

| Port | Purpose |
|------|---------|
| `25565/tcp` | Infrarust Java proxy |
| `25565/udp` | Native SVC listener (included by default) |
| `19132/udp` | Geyser Bedrock listener |
| `8080/tcp` | Web admin API and dashboard (optional, see below) |

Publish TCP and UDP separately. This Docker image explicitly enables the optional `plugin-svc` Cargo feature, so its native SVC listener is on by default in the image (standalone builds must opt in). Publish `-p 25565:25565/tcp -p 25565:25565/udp -p 19132:19132/udp` for Java, voice chat, and Bedrock.

Geyser requires a runtime config at `/app/config/geyser/config.yml`. Start from [`plugins/infrarust-plugin-floodgate/examples/geyser/config.yml`](https://github.com/shadowner/Infrarust/blob/main/plugins/infrarust-plugin-floodgate/examples/geyser/config.yml), set `remote.address` to `127.0.0.1`, and set `floodgate-key-file` to `/app/config/floodgate/key.pem`. Configure Infrarust to bind Java traffic on `0.0.0.0:25565`. Mount the same Floodgate key read-only for both services; never put the key in the image or source control.

The built-in SVC plugin reads `/app/plugins/svc-proxy/config.toml` because the Docker examples use `plugins_dir = "/app/plugins"`. Create this file as `./config/plugins/svc-proxy/config.toml` on the host and mount that directory at `/app/plugins/svc-proxy`. It listens on `0.0.0.0:25565/udp` by default, uses the client hostname as its public voice host when `public_voice_host` is empty, and sniffs each backend voice port from the SVC `voicechat:secret` channel. Set `backend_port_override` only when a backend needs a fixed port; `max_compatibility_version` defaults to `20`.

```toml
# ./config/plugins/svc-proxy/config.toml (mounted at /app/plugins/svc-proxy/config.toml)
listen = "0.0.0.0:25565"
public_voice_host = ""
max_compatibility_version = 20
# Optional fixed backend port; omit to sniff it from the SVC secret channel.
# backend_port_override = 24454
```

The Floodgate WASM adapter is built into the image at `/app/plugins/floodgate.wasm`. Mount only its data directory and the SVC data directory; mounting a volume over `/app/plugins` would hide the baked WASM file. The Floodgate plugin needs the `filesystem-extended` grant to read its key through a read-only guest mount:

```toml
# in ./config/infrarust.toml
plugins_dir = "/app/plugins"

[plugins.floodgate]
enabled = true
permissions = ["filesystem-extended"]

[[plugins.floodgate.wasm.mounts]]
host = "/app/config/floodgate"
guest = "/keys"
read_only = true
```

Its adapter config lives at `./config/plugins/floodgate/config.toml` on the host, mounted at `/app/plugins/floodgate/config.toml`. Set `key_file` to the guest path `/keys/key.pem`, and allow only Geyser's TCP peer in `trusted_sources`. With Geyser connecting to `127.0.0.1` inside the same container, the peer is `127.0.0.1/32`:

```toml
# ./config/plugins/floodgate/config.toml
key_file = "/keys/key.pem"
trusted_sources = ["127.0.0.1/32"]
```

## Directory structure

Infrarust inside Docker expects a config volume mounted at `/app/config`. The layout:

```
config/
├── infrarust.toml
├── geyser/
│   └── config.yml
├── plugins/
│   ├── floodgate/
│   │   └── config.toml
│   └── svc-proxy/
│       └── config.toml
└── servers/
    └── survival.toml
```

Store the Floodgate `key.pem` in a separate host directory (for example `./secrets/floodgate/key.pem`) and mount that directory read-only. The Infrarust Floodgate plugin and Geyser must use the same key.

Set `servers_dir` and `plugins_dir` in your `infrarust.toml` to paths inside the container:

```toml
bind = "0.0.0.0:25565"
servers_dir = "/app/config/servers"
plugins_dir = "/app/plugins"
```

The combined image runs as uid `65532`. Make `./config` and both plugin config directories readable and searchable by that uid. The Floodgate key directory and `key.pem` must also be readable by uid `65532`. The bind-mounted `./geyser-data` directory must be writable by uid `65532` (for example, set its owner to `65532:65532` and mode to `0700`).

WASM plugins are compiled once and cached in `/app/cache/wasm`, the default `[wasm] cache_dir` resolved from the image's working directory `/app`. The image creates that directory for the user it runs as (`65532`), readable and writable by that user only, so the cache needs no configuration. It lives in the container's writable layer: it survives a restart of the container, and a new container, after pulling a new image for example, compiles the plugins again at its first start. To keep the cache across containers, mount a named volume at `/app/cache`; Docker fills a new named volume with the directory from the image, owner included:

```yaml
services:
  infrarust:
    volumes:
      - ./config:/app/config
      - wasm-cache:/app/cache
      - ./config/plugins/floodgate:/app/plugins/floodgate:ro
      - ./config/plugins/svc-proxy:/app/plugins/svc-proxy:ro

volumes:
  wasm-cache:
```

A bind mount at `/app/cache` needs a host directory that uid `65532` can write, ideally owned by it with mode `0700` to keep other users out; otherwise the proxy logs `AOT cache directory cannot be written` and compiles the plugins at each start. Do not place the cache under `plugins_dir`: the proxy refuses to start with `cache_dir` inside it.

## Running with docker run

```bash
docker run -d \
  --name infrarust \
  -p 25565:25565/tcp \
  -p 25565:25565/udp \
  -p 19132:19132/udp \
  -v ./config:/app/config \
  -v ./secrets/floodgate:/app/config/floodgate:ro \
  -v ./config/plugins/floodgate:/app/plugins/floodgate:ro \
  -v ./config/plugins/svc-proxy:/app/plugins/svc-proxy:ro \
  -v ./geyser-data:/app/geyser \
  ghcr.io/shadowner/infrarust:latest \
  --config /app/config/infrarust.toml
```

The `--config` flag tells Infrarust where to find `infrarust.toml` inside the container. If you don't pass it, the binary looks for `infrarust.toml` in the working directory (`/app`).

To also expose the web admin API, add `-p 8080:8080` and set `bind = "0.0.0.0:8080"` (or any non-loopback address) in the `[web]` section of your config. The API requires an `api_key` when bound to a non-loopback address.

## Docker Compose

A minimal `docker-compose.yml`:

```yaml
services:
  infrarust:
    image: ghcr.io/shadowner/infrarust:latest
    command: ["--config", "/app/config/infrarust.toml"]
    ports:
      - "25565:25565/tcp"
      - "25565:25565/udp"
      - "19132:19132/udp"
    volumes:
      - ./config:/app/config
      - ./secrets/floodgate:/app/config/floodgate:ro
      - ./config/plugins/floodgate:/app/plugins/floodgate:ro
      - ./config/plugins/svc-proxy:/app/plugins/svc-proxy:ro
      - ./geyser-data:/app/geyser
    restart: unless-stopped
    stop_grace_period: 45s
```

`docker stop` sends SIGTERM and kills the container after 10 seconds by default. Set `stop_grace_period: 45s` (or use `docker stop -t 45`) so the supervisor can forward SIGTERM to both processes and wait for Infrarust's drain and plugin shutdown. After 40 seconds, the supervisor force-stops any child that remains; a second SIGTERM also force-stops both children. Set `GEYSER_ENABLED=false` to run Java/SVC without starting Geyser, or leave it at `auto` to start Geyser only when its config file exists.

## Environment variables

Infrarust reads these environment variables at runtime:

| Variable | Description |
|----------|-------------|
| `RUST_LOG` | Log level filter. Overrides the `--log-level` CLI flag. Accepts `trace`, `debug`, `info`, `warn`, `error`, or module-level filters like `infrarust_core=debug`. |
| `INFRARUST_COLOR` | `auto` (default), `always` or `never`. Containers started without `-t` have no terminal, so `auto` prints plain text; use `always` for panels like Pterodactyl that display colors. See [Colors](../reference/cli#colors). |
| `GEYSER_CONFIG_FILE` | Geyser config source path. Defaults to `/app/config/geyser/config.yml`. |
| `GEYSER_DATA_DIR` | Geyser working directory. Defaults to `/app/geyser`. |
| `GEYSER_JAVA_OPTS` | Additional Java options, appended after the default `-Xms128M -Xmx512M`. |
| `GEYSER_ENABLED` | `auto` (default) starts Geyser when its config exists; `true` requires it; `false` disables Geyser. |

Set it in your Compose file or `docker run`:

```bash
docker run -d \
  --name infrarust \
  -p 25565:25565/tcp \
  -p 25565:25565/udp \
  -p 19132:19132/udp \
  -e RUST_LOG=debug \
  -v ./config:/app/config \
  -v ./secrets/floodgate:/app/config/floodgate:ro \
  -v ./config/plugins/floodgate:/app/plugins/floodgate:ro \
  -v ./config/plugins/svc-proxy:/app/plugins/svc-proxy:ro \
  -v ./geyser-data:/app/geyser \
  ghcr.io/shadowner/infrarust:latest \
  --config /app/config/infrarust.toml
```

Or in Compose:

```yaml
services:
  infrarust:
    image: ghcr.io/shadowner/infrarust:latest
    command: ["--config", "/app/config/infrarust.toml"]
    ports:
      - "25565:25565/tcp"
      - "25565:25565/udp"
      - "19132:19132/udp"
    volumes:
      - ./config:/app/config
      - ./secrets/floodgate:/app/config/floodgate:ro
      - ./config/plugins/floodgate:/app/plugins/floodgate:ro
      - ./config/plugins/svc-proxy:/app/plugins/svc-proxy:ro
      - ./geyser-data:/app/geyser
    environment:
      RUST_LOG: "info"
    restart: unless-stopped
```

## CLI flags

These flags are passed after the image name in `docker run`, or in the `command` field of Compose:

| Flag | Default | Description |
|------|---------|-------------|
| `-c, --config <PATH>` | `infrarust.toml` | Path to the proxy configuration file |
| `-b, --bind <ADDR>` | from config | Override the bind address |
| `-l, --log-level <LEVEL>` | `info` | Log level filter (overridden by `RUST_LOG`) |
| `--plugins-dir <PATH>` | from config | Override the plugins directory path |

## Auto-discovery with Docker labels

When Infrarust runs alongside your Minecraft servers in Docker, it can discover them automatically through container labels. No static server config files needed.

Add a `[docker]` section to your `infrarust.toml`:

```toml
servers_dir = "/app/config/servers"
plugins_dir = "/app/plugins"

[docker]
```

Then label your Minecraft containers:

```yaml
services:
  infrarust:
    image: ghcr.io/shadowner/infrarust:latest
    command: ["--config", "/app/config/infrarust.toml"]
    ports:
      - "25565:25565/tcp"
      - "25565:25565/udp"
      - "19132:19132/udp"
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock:ro
      - ./config:/app/config
      - ./secrets/floodgate:/app/config/floodgate:ro
      - ./config/plugins/floodgate:/app/plugins/floodgate:ro
      - ./config/plugins/svc-proxy:/app/plugins/svc-proxy:ro
      - ./geyser-data:/app/geyser
    restart: unless-stopped

  survival:
    image: itzg/minecraft-server
    environment:
      EULA: "TRUE"
      TYPE: "PAPER"
    labels:
      infrarust.enable: "true"
      infrarust.domains: "survival.example.com"
    restart: unless-stopped
```

Infrarust scans for containers with `infrarust.enable=true`, reads their labels, resolves their network address, and registers them as backend servers. When containers start or stop, the routing table updates in real time.

::: warning
Mounting the Docker socket (`/var/run/docker.sock`) gives Infrarust read access to all containers on the host. Mount it read-only (`:ro`) and understand the security implications before deploying this in production.
:::

The full set of container labels and provider options is documented in [Docker Provider](../configuration/providers/docker.md).

## Networking

When Infrarust and your Minecraft servers share a Docker network, containers communicate by internal IP. This is the recommended setup because it avoids port publishing on the host for backend servers.

```yaml
networks:
  minecraft:
    driver: bridge

services:
  infrarust:
    image: ghcr.io/shadowner/infrarust:latest
    command: ["--config", "/app/config/infrarust.toml"]
    ports:
      - "25565:25565/tcp"
      - "25565:25565/udp"
      - "19132:19132/udp"
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock:ro
      - ./config:/app/config
      - ./secrets/floodgate:/app/config/floodgate:ro
      - ./config/plugins/floodgate:/app/plugins/floodgate:ro
      - ./config/plugins/svc-proxy:/app/plugins/svc-proxy:ro
      - ./geyser-data:/app/geyser
    networks:
      - minecraft
    restart: unless-stopped

  survival:
    image: itzg/minecraft-server
    environment:
      EULA: "TRUE"
    labels:
      infrarust.enable: "true"
      infrarust.domains: "survival.example.com"
    networks:
      - minecraft
    restart: unless-stopped
```

Tell Infrarust which Docker network to prefer for address resolution:

```toml
[docker]
network = "minecraft"
```

Without this setting, Infrarust picks the first available network IP from each container. If your containers are on multiple networks, set `network` to avoid resolving to the wrong IP.

## Building the image yourself

The included `Dockerfile` builds the admin frontend with Node, compiles the static Infrarust binary with Rust, and places it alongside the pinned Geyser Standalone JAR in a Temurin 21 Alpine runtime. The runtime runs both processes as uid `65532` and supervises them together.

```bash
docker build -t infrarust .
```

The build targets the host architecture. Release images support `linux/amd64` and `linux/arm64` through Docker buildx:

```bash
docker buildx build --platform linux/amd64,linux/arm64 -t infrarust .
```

## Connecting to external servers

If your Minecraft servers run outside Docker (bare metal, VMs, another host), you don't need the Docker provider or socket mount. Use static server config files instead:

```yaml
services:
  infrarust:
    image: ghcr.io/shadowner/infrarust:latest
    command: ["--config", "/app/config/infrarust.toml"]
    ports:
      - "25565:25565/tcp"
      - "25565:25565/udp"
      - "19132:19132/udp"
    volumes:
      - ./config:/app/config
      - ./secrets/floodgate:/app/config/floodgate:ro
      - ./config/plugins/floodgate:/app/plugins/floodgate:ro
      - ./config/plugins/svc-proxy:/app/plugins/svc-proxy:ro
      - ./geyser-data:/app/geyser
    restart: unless-stopped
```

Define your servers as `.toml` files in `config/servers/`:

```toml
# config/servers/survival.toml
domains = ["survival.example.com"]
addresses = ["192.168.1.50:25565"]
```

Infrarust watches the `servers/` directory and picks up new or changed files without a restart.
