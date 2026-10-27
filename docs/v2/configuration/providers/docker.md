---
title: Docker Provider
description: Auto-discover Minecraft servers from Docker containers using labels, with real-time updates when containers start or stop.
---

# Docker Provider

The Docker provider discovers Minecraft servers from running containers. Add `infrarust.*` labels to your containers and Infrarust registers them automatically. When a container starts or stops, the proxy updates its routing table in real time.

## Enabling the provider

Add a `[docker]` section to your `infrarust.toml`:

```toml
[docker]
```

That's it. All fields have defaults. Infrarust connects to the local Docker socket and starts scanning for labeled containers.

### Provider options

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `mode` | `"container"` or `"swarm"` | `"container"` | `container` discovers standalone containers. `swarm` discovers Swarm services (see [Swarm mode](#swarm-mode)). |
| `endpoint` | string | `"unix:///var/run/docker.sock"` | Docker daemon socket. Use `tcp://host:2375` for remote daemons. |
| `network` | string | none | In `container` mode: preferred network for address resolution. In `swarm` mode: the overlay network whose service VIP is the backend address. **Required** in `swarm` mode. |
| `poll_interval` | duration | `"30s"` | Swarm mode: interval between full service snapshots, which are authoritative. Not used in `container` mode. |
| `reconnect_delay` | duration | `"5s"` | Initial delay before reconnecting after a daemon disconnection. |

Full example:

```toml
[docker]
endpoint = "unix:///var/run/docker.sock"
network = "minecraft"
poll_interval = "30s"
reconnect_delay = "5s"
```

## Container labels

Infrarust scans for containers with the label `infrarust.enable=true`. All other labels are optional.

| Label | Type | Default | Description |
|-------|------|---------|-------------|
| `infrarust.enable` | `"true"` | required | Must be `"true"` for the container to be discovered. |
| `infrarust.domains` | comma-separated | `<container_name>.docker.local` | Domains that route to this server. |
| `infrarust.port` | integer | `25565` | Minecraft port inside the container. |
| `infrarust.proxy_mode` | string | `passthrough` | One of: `passthrough`, `client_only`, `offline`, `server_only`, `zero_copy`. |
| `infrarust.send_proxy_protocol` | `"true"` or `"1"` | `false` | Send PROXY protocol headers to the backend. |
| `infrarust.name` | string | none | Human-readable name for server switching. |
| `infrarust.network` | string | none | Network group for server switching. |
| `infrarust.motd.text` | string | none | Custom MOTD text shown in the server list. |

If you omit `infrarust.domains`, the container name becomes the domain with a `.docker.local` suffix. A container named `mc-survival` gets the domain `mc-survival.docker.local`.

## Address resolution

Infrarust resolves the container address in this order:

1. The IP from the preferred Docker network (set via `network` in `[docker]` config), or the first available network IP.
2. Port bindings from the host config (e.g., `0.0.0.0:25565` mapped port).
3. The container name as a hostname (last resort).

The resolved IP is combined with `infrarust.port` (default 25565) to form the backend address.

## docker-compose examples

### Single server

```yaml
services:
  infrarust:
    image: ghcr.io/shadowner/infrarust:latest
    ports:
      - "25565:25565"
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock:ro
      - ./infrarust.toml:/app/infrarust.toml:ro
    restart: unless-stopped

  mc-survival:
    image: itzg/minecraft-server
    environment:
      EULA: "TRUE"
      TYPE: "PAPER"
    labels:
      infrarust.enable: "true"
      infrarust.domains: "survival.example.com"
    restart: unless-stopped
```

With this setup, players connecting to `survival.example.com` are routed to the `mc-survival` container on port 25565. No static server config files needed.

### Multiple servers on a shared network

```yaml
networks:
  minecraft:
    driver: bridge

services:
  infrarust:
    image: ghcr.io/shadowner/infrarust:latest
    ports:
      - "25565:25565"
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock:ro
      - ./infrarust.toml:/app/infrarust.toml:ro
    networks:
      - minecraft
    restart: unless-stopped

  lobby:
    image: itzg/minecraft-server
    environment:
      EULA: "TRUE"
    labels:
      infrarust.enable: "true"
      infrarust.domains: "mc.example.com"
      infrarust.proxy_mode: "client_only"
      infrarust.network: "main"
    networks:
      - minecraft
    restart: unless-stopped

  survival:
    image: itzg/minecraft-server
    environment:
      EULA: "TRUE"
      TYPE: "PAPER"
      DIFFICULTY: "hard"
    labels:
      infrarust.enable: "true"
      infrarust.domains: "survival.mc.example.com"
      infrarust.proxy_mode: "client_only"
      infrarust.network: "main"
      infrarust.motd.text: "§aSurvival §7| §fWelcome!"
    networks:
      - minecraft
    restart: unless-stopped

  creative:
    image: itzg/minecraft-server
    environment:
      EULA: "TRUE"
      TYPE: "PAPER"
      GAMEMODE: "creative"
    labels:
      infrarust.enable: "true"
      infrarust.domains: "creative.mc.example.com"
      infrarust.proxy_mode: "client_only"
      infrarust.network: "main"
      infrarust.port: "25565"
    networks:
      - minecraft
    restart: unless-stopped
```

The matching `infrarust.toml`:

```toml
[docker]
network = "minecraft"
```

Setting `network = "minecraft"` tells Infrarust to prefer IPs from the `minecraft` Docker network when resolving container addresses. This avoids issues with containers that are attached to multiple networks.

### Custom port

If your server runs on a non-standard port inside the container:

```yaml
  modded:
    image: itzg/minecraft-server
    environment:
      EULA: "TRUE"
      SERVER_PORT: "25566"
    labels:
      infrarust.enable: "true"
      infrarust.domains: "modded.example.com"
      infrarust.port: "25566"
```

### PROXY protocol

If your backend expects PROXY protocol headers (for IP forwarding):

```yaml
  backend:
    image: itzg/minecraft-server
    labels:
      infrarust.enable: "true"
      infrarust.domains: "play.example.com"
      infrarust.send_proxy_protocol: "true"
```

## Swarm mode

Set `mode = "swarm"` to discover Docker Swarm services instead of standalone containers. Infrarust reads the labels of each **service** (its `Spec.Labels`, which is what Compose writes from `deploy.labels`) and routes to the service's **VIP** on the overlay network named by `network`. Routing goes through the VIP, so you do not need to know which node or task runs the server.

```toml
[docker]
mode = "swarm"
network = "mc-overlay"
```

Infrarust must run on a Swarm **manager** node, because it uses the manager API. It must also be attached to the same overlay network as the Minecraft services, so that the VIP is reachable.

### Stack example

```yaml
services:
  infrarust:
    image: ghcr.io/shadowner/infrarust:latest
    ports:
      - "25565:25565"
    volumes:
      - ./infrarust.toml:/app/infrarust.toml:ro
      - /var/run/docker.sock:/var/run/docker.sock:ro
    networks:
      - mc-overlay
    deploy:
      placement:
        constraints:
          - node.role == manager

  mc-survival:
    image: itzg/minecraft-server
    environment:
      EULA: "TRUE"
    networks:
      - mc-overlay
    deploy:
      replicas: 1
      update_config:
        order: stop-first
      labels:
        infrarust.enable: "true"
        infrarust.domains: "survival.example.com"
        infrarust.port: "25565"
        infrarust.proxy_mode: "client_only"
        infrarust.motd.text: "Survival"

networks:
  mc-overlay:
    name: mc-overlay
    driver: overlay
```

Set `name:` on the network, as above, so that the configured `network` matches the real overlay name. Without it, Compose prefixes the name with the stack name (for example `mystack_mc-overlay`).

::: warning
Put the labels under `deploy.labels`, not the top-level `labels:` key. In swarm mode Infrarust reads only service labels, and a top-level `labels:` entry only sets container labels, which swarm mode ignores.
:::

### Supported and skipped services

Infrarust registers each labelled service that it can route safely. It skips the rest, logs the reason once per change, and keeps polling:

| Service shape | Result |
|---------------|--------|
| Replicated, 0 or 1 replica, VIP on the configured overlay | Registered. Zero-replica services are registered too, and connections fail until they are scaled up. |
| 2 or more replicas | Skipped. |
| `global` mode | Skipped. |
| Replicated or global jobs | Skipped. |
| `endpoint_mode: dnsrr` | Skipped (no VIP). |
| `update_config.order: start-first` | Skipped, because two backends can briefly overlap. Use `stop-first`. |
| Not attached to the configured overlay | Skipped. |
| Server name has characters other than `a-z`, `0-9`, `_`, `-`, `.` | Skipped. Set `infrarust.name` to a valid name. |

The poll is authoritative. A service that stops matching (for example, it is scaled to two replicas) is removed from routing. If a poll fails, for example because the node is not a manager, the proxy keeps the routes it already has and logs a warning.

The provider ID for a swarm service is `docker@swarm:<service-id>`, which stays the same when the service is renamed.

### Service labels

These labels go under `deploy.labels`. They are the container labels above, plus the service-level settings below.

| Label | Type | Default | Description |
|-------|------|---------|-------------|
| `infrarust.enable` | `"true"` | required | Opts the service in. |
| `infrarust.domains` | comma-separated | `<service_name>.docker.local` | Domains that route to this server. |
| `infrarust.port` | integer | `25565` | Minecraft port inside the service's containers. |
| `infrarust.name` | string | service name | Server name. Only `a-z`, `0-9`, `_`, `-`, `.` are allowed. |
| `infrarust.proxy_mode` | string | `passthrough` | One of: `passthrough`, `client_only`, `offline`, `server_only`, `zero_copy`. |
| `infrarust.send_proxy_protocol` | `"true"` or `"1"` | `false` | Send PROXY protocol headers to the backend. |
| `infrarust.network` | string | none | Switch network group, for server switching. |
| `infrarust.forwarding_mode` | string | none | One of: `none`, `bungeecord`, `bungeeguard`, `velocity`. |
| `infrarust.domain_rewrite` | string | `none` | `none`, `from_backend`, or an explicit domain. |
| `infrarust.balance` | string | `first_available` | One of: `first_available`, `round_robin`, `least_conn`. |
| `infrarust.max_players` | integer | `0` (unlimited) | Maximum player count. |
| `infrarust.limbo_handlers` | comma-separated | none | Limbo handler IDs, run in order. |
| `infrarust.timeouts.connect` / `.read` / `.write` | duration (`"5s"`) | global defaults | Per-server timeouts. |
| `infrarust.motd.text` | string | none | MOTD for the `online` state (alias for `infrarust.motd.online.text`). |
| `infrarust.motd.<state>.text` | string | none | MOTD for `online`, `sleeping`, `starting`, `crashed`, `stopping`, or `unreachable`. |
| `infrarust.motd.<state>.version_name` | string | none | Version name shown with that MOTD state. |
| `infrarust.motd.<state>.max_players` | integer | none | Player count shown with that MOTD state. |

An invalid value is logged and ignored. Only that setting falls back to its default. Labels never carry secrets: server-manager credentials and favicon data cannot be set from labels, so put them in the server config file instead.

### Swarm example with service labels

```yaml
  mc-creative:
    image: itzg/minecraft-server
    environment:
      EULA: "TRUE"
      GAMEMODE: "creative"
    networks:
      - mc-overlay
    deploy:
      replicas: 1
      update_config:
        order: stop-first
      labels:
        infrarust.enable: "true"
        infrarust.domains: "creative.example.com"
        infrarust.proxy_mode: "client_only"
        infrarust.network: "main"
        infrarust.forwarding_mode: "velocity"
        infrarust.max_players: "100"
        infrarust.timeouts.connect: "3s"
        infrarust.motd.text: "§bCreative"
        infrarust.motd.sleeping.text: "§7Sleeping, join to wake up"
        infrarust.motd.sleeping.version_name: "Creative"
```

## How discovery works

In `container` mode (the default), on startup Infrarust lists all running containers with `infrarust.enable=true` and registers them. It then subscribes to the Docker event stream and reacts to six events: `start`, `stop`, `die`, `destroy`, `pause`, and `unpause`.

When a container starts or unpauses, Infrarust inspects it, parses the labels, resolves the address, and adds it to the router. When a container stops, dies, pauses, or is destroyed, Infrarust removes it.

If the connection to the Docker daemon drops, Infrarust reconnects with exponential backoff (starting at `reconnect_delay`, capping at 60 seconds). After reconnecting, it re-scans all containers to catch any changes that happened while disconnected.

::: warning
The Docker feature must be compiled in. If you see a warning like "docker config found but docker feature is not enabled", rebuild Infrarust with the `docker` feature flag. The official Docker image includes it by default.
:::

## Mixing providers

Docker and file providers work side by side. You can define some servers as `.toml` files in `servers/` and discover others from Docker containers. Both feed into the same routing table.

::: tip
Docker-discovered servers get a provider ID in the format `docker@<container_name>`. Swarm services get `docker@swarm:<service-id>`. File-based servers use `file@<filename>`. These IDs are visible in logs and the console.
:::
