# Architecture

This document is a technical deep dive into Devcontainer Bridge (`dbr`). It covers the reverse connection model, the full control protocol specification, port detection internals, TCP proxy implementation, multi-container handling, reconnection behavior, and signal handling.

---

## Reverse Connection Model

All TCP connections flow **container to host**, never host to container. This is the foundational design constraint.

### Why container-to-host?

On **macOS Docker Desktop**, containers run inside a hidden Linux VM. The host cannot route to container IP addresses — there is no `docker0` bridge visible to macOS. This rules out the host initiating connections into the container.

The same limitation applies to Docker Desktop on Windows, and to rootless Docker on Linux where network namespaces isolate containers.

The solution mirrors SSH reverse port forwarding (`ssh -R`): the container daemon initiates all connections outward to the host, and the host daemon multiplexes over those connections. The container reaches the host via `host.docker.internal` (Docker's built-in DNS), a gateway IP fallback, or an explicit address.

### Two host-side ports

The host daemon listens on two TCP ports, bound to all interfaces (`0.0.0.0`) by default so containers can reach them via Docker Desktop's gateway IP:

| Port | Default | Default Bind | Role |
|------|---------|-------------|------|
| **Control** | `19285` | `0.0.0.0` | JSON-line protocol messages (registration, forward/unforward, heartbeats, URL open requests) |
| **Data** | `19286` | `0.0.0.0` | Reverse data connections for TCP proxying (one per client connection) |

The bind address is configurable via `--bind-addr` (e.g., `--bind-addr 127.0.0.1` for loopback-only).

Separating control from data keeps the protocol simple — the control channel is a persistent, framed JSON-line stream, while data connections carry raw TCP bytes after a one-line handshake.

---

## Protocol Specification

All control messages are serialized as **JSON lines** (newline-delimited JSON) using serde's internally-tagged representation with a `"type"` discriminator field. Each message is a single line terminated by `\n`.

### Constraints

- Maximum message size: **64 KB** (65,536 bytes). Messages exceeding this limit are rejected.
- Encoding: UTF-8 JSON.
- Framing: one JSON object per line, terminated by `\n`.
- The control channel enforces bounded reads to prevent memory exhaustion from a peer that sends data without a newline.

### Control Channel Messages (Port 19285)

#### Register

**Direction:** Container to Host

Sent as the first message after TCP connect. Identifies the container to the host daemon.

```json
{"type":"Register","container_id":"abc123def","hostname":"dev"}
```

| Field | Type | Description |
|-------|------|-------------|
| `container_id` | string | Unique identifier for the container. Typically the Docker container short ID, read from `/etc/hostname`. |
| `hostname` | string | Human-readable hostname for display in `dbr status`. |

#### RegisterAck

**Direction:** Host to Container

Acknowledges a `Register` message.

```json
{"type":"RegisterAck","success":true}
```

| Field | Type | Description |
|-------|------|-------------|
| `success` | bool | `true` if registration succeeded. `false` if rejected (e.g., maximum container limit of 64 reached). |

#### Forward

**Direction:** Container to Host

Requests the host to bind a loopback listener and forward traffic for a container port.

```json
{"type":"Forward","port":8080,"protocol":"Tcp","process_name":"node","pid":1234}
```

| Field | Type | Description |
|-------|------|-------------|
| `port` | u16 | The TCP port listening inside the container. |
| `protocol` | enum | Always `"Tcp"` in v1. |
| `process_name` | string or null | Name of the process listening on the port (from `/proc/{pid}/comm`). Optional. |
| `pid` | u32 or null | PID of the listening process. Optional. |

#### ForwardAck

**Direction:** Host to Container

Acknowledges a `Forward` request, reporting the actual host port bound.

```json
{"type":"ForwardAck","port":8080,"success":true,"host_port":8080}
```

| Field | Type | Description |
|-------|------|-------------|
| `port` | u16 | The container port from the original `Forward` request. |
| `success` | bool | Whether the forward was established. |
| `host_port` | u16 | The port bound on the host. May differ from `port` if there was a conflict (e.g., another container already owns that host port). `0` on failure. |

#### Unforward

**Direction:** Container to Host

Requests removal of a port forward. The host tears down the listener and drains active proxy connections (with a configurable timeout, default 5 seconds).

```json
{"type":"Unforward","port":8080}
```

| Field | Type | Description |
|-------|------|-------------|
| `port` | u16 | The container port to stop forwarding. |

#### ConnectRequest

**Direction:** Host to Container

Sent when a client connects to a forwarded port on the host. Instructs the container to open a reverse data connection.

```json
{"type":"ConnectRequest","port":8080,"conn_id":"550e8400-e29b-41d4-a716-446655440000"}
```

| Field | Type | Description |
|-------|------|-------------|
| `port` | u16 | The container port the client wants to reach. |
| `conn_id` | string | UUID v4 identifying this connection. Used to correlate the `ConnectReady` on the data channel. |

**Timeout:** If the container does not respond with a `ConnectReady` (on the data channel) or `ConnectFailed` (on the control channel) within **10 seconds**, the host drops the client connection.

#### ConnectFailed

**Direction:** Container to Host (control channel)

Reports that the container could not fulfill a `ConnectRequest`.

```json
{"type":"ConnectFailed","conn_id":"550e8400-e29b-41d4-a716-446655440000","error":"connection refused"}
```

| Field | Type | Description |
|-------|------|-------------|
| `conn_id` | string | The connection identifier from the original `ConnectRequest`. |
| `error` | string | Human-readable error description. |

#### OpenUrl

**Direction:** Container to Host

Asks the host to open a URL in the host's default browser.

```json
{"type":"OpenUrl","url":"http://localhost:8080/auth/callback"}
```

| Field | Type | Description |
|-------|------|-------------|
| `url` | string | The URL to open. Must use `http://` or `https://` scheme. Maximum 2048 characters. |

The host validates the URL, rewrites `localhost` ports if the container port is mapped to a different host port, then invokes `open` (macOS) or `xdg-open` (Linux). URL opens are rate-limited to 5 per second.

#### OpenUrlAck

**Direction:** Host to Container

```json
{"type":"OpenUrlAck","success":true}
```

| Field | Type | Description |
|-------|------|-------------|
| `success` | bool | Whether the browser was opened. |

#### Ping / Pong

**Direction:** Either

Keepalive heartbeat. The host sends `Ping` every 30 seconds. If 3 consecutive pongs are missed, the container is considered dead and disconnected.

```json
{"type":"Ping"}
{"type":"Pong"}
```

No additional fields.

#### ListRequest

**Direction:** CLI to Host

Requests a snapshot of all active forwards across all containers (used by `dbr status`).

```json
{"type":"ListRequest"}
```

#### ListResponse

**Direction:** Host to CLI

```json
{
  "type": "ListResponse",
  "forwards": [
    {
      "container_id": "abc123",
      "hostname": "dev",
      "port": 8080,
      "host_port": 8080,
      "protocol": "Tcp",
      "process_name": "node",
      "pid": 1234,
      "since": "1707900000"
    }
  ]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `forwards` | array | List of `ForwardInfo` objects. |

Each `ForwardInfo` contains: `container_id`, `hostname`, `port` (container), `host_port`, `protocol`, `process_name` (nullable), `pid` (nullable), `since` (Unix epoch seconds as string).

### Data Channel Handshake (Port 19286)

The data channel is used exclusively for reverse data connections. The protocol is:

1. Container opens a new TCP connection to `host:19286`.
2. Container sends exactly one JSON line:
   ```json
   {"type":"ConnectReady","conn_id":"550e8400-e29b-41d4-a716-446655440000"}\n
   ```
3. The host matches `conn_id` to a pending `ConnectRequest`.
4. After the handshake line, the connection switches to **raw TCP proxying** — no further JSON framing. The host bridges the client socket and this data socket using `tokio::io::copy_bidirectional`.

| Field | Type | Description |
|-------|------|-------------|
| `conn_id` | string | Must match a pending `ConnectRequest`. Unmatched IDs are logged and the connection is dropped. |

The host uses the same bounded `read_message` function for the handshake line, enforcing the 64 KB limit and preventing OOM from a malicious peer.

---

## Port Detection Mechanism

The container daemon detects listening ports by parsing Linux procfs, the same approach VS Code uses.

### `/proc/net/tcp` Format

Each line after the header in `/proc/net/tcp` (and `/proc/net/tcp6`) has this structure:

```
  sl  local_address rem_address   st tx_queue:rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12345 ...
```

Key fields (whitespace-separated, 0-indexed):

| Index | Field | Description |
|-------|-------|-------------|
| 1 | `local_address` | `HEX_IP:HEX_PORT` — IP and port in hexadecimal |
| 3 | `st` | Socket state in hex |
| 9 | `inode` | Socket inode number (for process resolution) |

### Hex Parsing Examples

**Extracting the port** from `local_address`:

```
Field value:    00000000:1F90
Split on ':':   IP=00000000, PORT=1F90
Hex decode:     0x1F90 = 8080
```

More examples:

| Hex Port | Decimal |
|----------|---------|
| `0050` | 80 |
| `1F90` | 8080 |
| `4B59` | 19289 |
| `23F1` | 9201 |
| `0BB8` | 3000 |

**IPv6 addresses** in `/proc/net/tcp6` use 32 hex characters for the IP but the port encoding is identical:

```
00000000000000000000000000000000:1F90  →  port 8080 on [::]
00000000000000000000000001000000:23F1  →  port 9201 on [::1]
```

### Filtering for LISTEN State

Only sockets in state `0A` (hex for TCP state `LISTEN`) are relevant. The scanner skips all other states:

| Hex State | TCP State | Included? |
|-----------|-----------|-----------|
| `0A` | LISTEN | Yes |
| `01` | ESTABLISHED | No |
| `06` | TIME_WAIT | No |
| All others | Various | No |

### Process Name Resolution

After identifying listening ports, the scanner attempts to resolve the process name:

1. Read the socket inode from field index 9 of the `/proc/net/tcp` line.
2. Walk `/proc/{pid}/fd/` directories for all PIDs.
3. For each file descriptor, `readlink` to check if it points to `socket:[{inode}]`.
4. If matched, read `/proc/{pid}/comm` for the process name.

This is **best-effort** — it may fail due to permission restrictions (reading other users' `/proc/{pid}/fd`) or race conditions (process exits between scan and lookup). Failure is non-fatal; the port is still forwarded with `process_name: null`.

### Scan Loop

The scanner runs on a configurable interval (default: **1 second**, set via `--scan-interval`).

Each cycle:

1. Read `/proc/net/tcp` and `/proc/net/tcp6` (at least one must succeed).
2. Parse all LISTEN entries, extract port and inode.
3. Deduplicate ports across tcp and tcp6 (a port in both files is reported once).
4. Exclude self-ports: the control port (19285) and data port (19286) are always excluded.
5. Apply the port filter (exclude list, include/allowlist, process regex).
6. Diff against the previous scan result.
7. Send `Forward` for new ports, `Unforward` for removed ports.

---

## TCP Proxy Implementation

The proxy connects a client on the host to a service inside the container through a reverse data connection.

### Data Flow

```
Client App          Host Daemon              Container Daemon         Container Service
    |                    |                         |                        |
    |--- TCP connect --->|                         |                        |
    |               (forwarded port)               |                        |
    |                    |                         |                        |
    |                    |-- ConnectRequest ------->|                        |
    |                    |   {port, conn_id}        |                        |
    |                    |   (control channel)      |                        |
    |                    |                         |--- TCP connect -------->|
    |                    |                         |   (localhost:port)      |
    |                    |                         |                        |
    |                    |<-------- TCP connect ---|                        |
    |                    |   (data channel 19286)  |                        |
    |                    |                         |                        |
    |                    |<-- ConnectReady ---------|                        |
    |                    |   {conn_id}\n            |                        |
    |                    |   (on data connection)   |                        |
    |                    |                         |                        |
    |<== bidirectional ===========================================+========>|
    |    copy via          copy_bidirectional         copy_bidirectional     |
    |    tokio                                                              |
    |                    |                         |                        |
    |--- close --------->|                         |                        |
    |                    |--- close --------------->|--- close ------------->|
```

### Step-by-Step

1. **Client connects** to a forwarded port on the host (e.g., `localhost:8080`).
2. The per-port listener accepts the connection and sends a `ClientConnection` to the host daemon's main loop via an mpsc channel.
3. The host daemon generates a UUID v4 `conn_id` and registers a **pending connection** (a `oneshot::Sender<TcpStream>`) keyed by `conn_id`.
4. A `ConnectRequest{port, conn_id}` is sent to the container over the control channel.
5. The **container daemon** receives the `ConnectRequest`, spawns a tokio task that:
   - Connects to `127.0.0.1:{port}` inside the container.
   - Opens a new TCP connection to `{host}:19286` (the data port).
   - Sends `ConnectReady{conn_id}\n` as the handshake line.
   - Enters `tokio::io::copy_bidirectional` between the local socket and the data socket.
6. The **host daemon** accepts the data connection, reads the `ConnectReady` handshake, and resolves the pending connection by sending the data `TcpStream` through the oneshot channel.
7. The host bridges the **client socket** and the **data socket** via `tokio::io::copy_bidirectional`.
8. When either side closes, both connections tear down. Byte counts are logged.

### Pending Connection Management

Pending connections are stored in a `HashMap<String, oneshot::Sender<TcpStream>>` behind an `Arc<Mutex<...>>`. Key behaviors:

- **Timeout:** If no `ConnectReady` arrives within 10 seconds, the pending entry is removed and the client connection is dropped.
- **Cancellation:** On `ConnectFailed`, the pending entry is removed so the waiting side receives a `RecvError`.
- **Capacity:** Maximum 1024 pending connections. When the limit is reached, stale entries (whose receiver was dropped due to timeout) are pruned.

### Two-Tier Binding

The host daemon uses a two-tier binding model:

**Control and data ports** bind to all interfaces (`0.0.0.0`) by default, configurable via `--bind-addr`. This is necessary for Docker Desktop on macOS where containers reach the host via a gateway IP, not loopback.

**Forwarded port listeners** always bind to **loopback only**, regardless of `--bind-addr`:

1. Try `[::1]:{port}` first (IPv6 loopback, supports dual-stack on some systems).
2. Fall back to `127.0.0.1:{port}` if IPv6 binding fails.
3. **Never** bind to `0.0.0.0` or `[::]`.

Each listener runs as an independent tokio task with a `watch` channel for shutdown signaling.

---

## Multi-Container Handling

One host daemon serves all running devcontainers concurrently.

### Container Registration

Each container daemon connects to the host control port and sends a `Register` message with its `container_id` (typically the Docker short hostname). The host maintains a `HashMap<String, ContainerState>` tracking:

- Hostname
- Active forwards (`HashMap<u16, ForwardState>`)

### Port Conflict Resolution

When multiple containers forward the same port:

1. First container gets `host_port == container_port` (e.g., 8080 to 8080).
2. Subsequent containers get the next available port. The host scans from the preferred port upward, skipping any port already in a `used_host_ports` map.
3. The alternative host port is reported in `ForwardAck` so the container knows the mapping.
4. `dbr status` shows the mapping for each container.

Example:

```
Container       Port   Host Port  Process    Since
myapp_dev       8080   8080       node       2m ago
other_proj      8080   8081       python     10m ago
```

### Limits

- Maximum concurrent containers: **64**
- Maximum forwards per container: **128**
- Maximum pending data connections: **1024**

### Cleanup on Disconnect

When the host detects EOF on a container's control connection (container stopped or network lost):

1. All forwards for that container are torn down (listener shutdown signal sent).
2. Listener tasks are awaited to ensure ports are freed.
3. Active proxy connections are drained concurrently with a configurable timeout (default 5 seconds).
4. The container is removed from state.
5. If `--exit-on-idle` is enabled and no containers remain, the host daemon exits.

---

## Reconnection Behavior

The container daemon is designed to survive transient host daemon restarts and network interruptions.

### Exponential Backoff

On connection failure or disconnect:

- Initial delay: **100ms**
- Multiplier: **2x** per attempt
- Maximum delay: **5 seconds**
- Reset: backoff resets to 100ms after a successful `RegisterAck`.

### State Preservation Across Reconnects

The container daemon preserves its `forwarded` ports map across reconnection cycles. After reconnecting and re-registering:

1. All previously forwarded ports are re-sent as `Forward` messages.
2. The next scan cycle detects the current state and sends any additional `Forward` or `Unforward` diffs.

This ensures that if the host daemon restarts, all port forwards are re-established without waiting for the next scan to detect them.

### Heartbeat / Keepalive

The host sends `Ping` to each container every **30 seconds**. If **3 consecutive pongs** are missed (90 seconds without a response), the container is considered dead and disconnected.

The container responds to `Ping` with `Pong` immediately. If the container detects a control channel error while sending `Pong`, it enters the reconnection loop.

---

## Signal Handling

### Container Daemon

The container daemon handles three shutdown triggers:

| Signal | Behavior |
|--------|----------|
| `SIGTERM` | Clean shutdown: send `Unforward` for all ports, close control connection, exit. |
| `SIGHUP` | Same as SIGTERM. |
| `SIGINT` (Ctrl+C) | Same as SIGTERM (handled via the external shutdown watch channel). |

Additionally, the container daemon monitors its **parent PID** every 5 seconds. If the parent PID changes from its initial value (indicating reparenting to init, PID 1), the daemon treats this as a container shutdown signal. This handles the case where `docker compose exec -d` does not propagate signals on container stop.

All shutdown triggers are unified through an internal `watch` channel. The session loop sends `Unforward` for every active port before exiting.

### Host Daemon

| Signal | Behavior |
|--------|----------|
| `SIGTERM` | Clean shutdown: signal all port listeners, drain active proxy connections (with timeout), remove PID file, exit. |
| `SIGINT` (Ctrl+C) | Same as SIGTERM. |

On shutdown, the host daemon:

1. Signals all per-port listener tasks to stop accepting new connections.
2. Awaits each listener task to ensure ports are freed.
3. Drains active proxy connections concurrently (polls every 50ms, times out after configurable drain timeout, default 5 seconds).
4. Logs any connections that were forcibly closed after the drain timeout expired.

---

## Host Address Resolution

The container daemon resolves the host address using this chain (first match wins):

| Priority | Source | Example |
|----------|--------|---------|
| 1 | `--host-addr` CLI flag | `--host-addr 192.168.65.2` |
| 2 | `DCBRIDGE_HOST` environment variable | `DCBRIDGE_HOST=host.docker.internal` |
| 3 | DNS lookup of `host.docker.internal` | Works on Docker Desktop and Docker Engine 20.10+ |
| 4 | Gateway IP from `ip route` default route | Parses `default via <IP>` from `ip route` output |
| 5 | Fail with actionable error | Lists all methods that were tried |

---

## Configuration Layering

Configuration is loaded with the following precedence (highest wins):

1. **CLI flags** (e.g., `--control-port`, `--scan-interval`)
2. **Environment variables** (`DCBRIDGE_HOST`, `DCBRIDGE_HOST_PORT`)
3. **Config file** (`~/.config/dbr/config.toml`)
4. **Compiled-in defaults**

Default values:

| Setting | Default |
|---------|---------|
| Control port | 19285 |
| Data port | 19286 |
| Scan interval | 1000ms |
| Log level | info |
| Log format | text |
| Drain timeout | 5s |
| Heartbeat interval | 30s |
| Missed pongs threshold | 3 |
| Connect timeout | 10s |

---

## Port Filtering

The container daemon applies filters to scanned ports before forwarding:

1. **Exclude ports** (`--exclude-ports`): ports in this set are never forwarded. The control and data ports are always auto-excluded.
2. **Include ports** (`--include-ports`): if non-empty, only ports in this set are forwarded (allowlist mode). Merged with `forwardPorts` from `devcontainer.json` if present.
3. **Exclude process** (`--exclude-process`): a regex pattern matched against the process name. Matching ports are excluded.

Filter evaluation order: exclude ports checked first (takes precedence), then include allowlist, then process regex. The default (no filters) forwards all detected listening ports, matching VS Code behavior.

---

## Browser URL Opening

### Container Side (`dbr open` / `dbr-open`)

The `dbr open <URL>` subcommand (and the `dbr-open` hardlink for `BROWSER` env var integration):

1. Validates the URL: must be `http://` or `https://`, max 2048 characters.
2. Connects to the host control port on `127.0.0.1`.
3. Sends `OpenUrl{url}`.
4. Waits for `OpenUrlAck`.

### Host Side

On receiving `OpenUrl`:

1. Validates the URL (scheme whitelist, length cap, case-insensitive scheme check).
2. **Rewrites localhost ports**: if the URL contains `localhost:{port}` or `127.0.0.1:{port}` and that container port is mapped to a different host port, the port is rewritten. For example, if container port 3000 is forwarded to host port 3001, `http://localhost:3000/callback` becomes `http://localhost:3001/callback`.
3. **Rate limiting**: sliding window of 1 second, maximum 5 opens per second. Excess requests are rejected.
4. Opens the URL via `open` (macOS) or `xdg-open` (Linux). The URL is passed as a single process argument (not via shell) to prevent command injection.
5. Returns `OpenUrlAck{success}`.

---

## Idempotent Daemon Startup (`dbr ensure`)

The `ensure` subcommand guarantees the host daemon is running:

1. Try connecting to `127.0.0.1:{control_port}`.
2. **Connection succeeds**: send `Ping`, wait for `Pong` (3s timeout).
   - `Pong` received: daemon already running, exit 0.
   - No `Pong`: port is occupied by a non-dbr process. Fail with actionable error suggesting `--control-port`.
3. **Connection refused**: daemon not running. Spawn `dbr host-daemon` as a background process.
4. Write PID to `~/.config/dbr/daemon.pid`.
5. Poll for readiness (connect + Ping/Pong) every 200ms, up to 5 seconds.
6. If ready, exit 0. If timeout, exit with error.
