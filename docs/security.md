# Security Model

This document describes the threat model, security guarantees, and audit guidance for Devcontainer Bridge (`dbr`).

## Threat Model

### What we protect against

- **Network exposure of forwarded ports.** All listeners bind to loopback only (`127.0.0.1` / `[::1]`). A forwarded port is never reachable from the network.
- **Arbitrary command execution from containers.** The host daemon accepts a fixed set of protocol messages. A container cannot instruct the host to run arbitrary commands. The only host-side actions are binding loopback ports and opening validated URLs.
- **URL-based attacks.** Only `http://` and `https://` URLs are accepted. Schemes like `file://`, `javascript:`, and `ftp://` are rejected. URLs are length-capped and rate-limited.
- **Resource exhaustion.** Control messages are capped at 64 KB. Per-container forward limits (128), total container limits (64), and rate limiting on URL opens (5/sec) prevent a runaway or malicious container from overwhelming the host.
- **Protocol abuse.** Malformed JSON, oversized messages, and unexpected message types are handled gracefully without crashing.

### What is out of scope

- **Malicious code on the host.** If an attacker has code execution on the host, they can already access anything on loopback. `dbr` does not make this worse.
- **Container-to-container isolation.** Containers are isolated by Docker networking. `dbr` does not create cross-container communication paths.
- **Encrypted transport.** The control and data channels use plaintext TCP on loopback. Since both endpoints are on the same machine (or within the Docker Desktop VM), TLS is unnecessary — the same trust boundary as Docker Desktop port publishing, `kubectl port-forward`, and SSH `-L` tunnels.
- **Authentication bypass.** Token authentication secures the control channel against unauthorized registrations. Disabling it with `--no-auth` is intended only for local development and testing.

## Two-Tier Binding Model

`dbr` uses a two-tier binding model that balances container reachability with host security:

### Tier 1: Control and Data ports (container-reachable)

| Listener | Default bind | Port | Source |
|----------|-------------|------|--------|
| Control channel | auto-detected | 19285 | `src/control.rs` |
| Data channel | auto-detected | 19286 | `src/host/mod.rs` |

The bind address for control and data ports is **auto-detected** at startup:

1. **`--bind-addr` explicitly set** -- uses the specified address, no auto-detection.
2. **`--no-docker-detect` flag set** -- binds to `127.0.0.1`, no auto-detection.
3. **Docker detected** (via `docker info`) -- binds to `0.0.0.0` (all interfaces) so containers can reach the host via Docker Desktop's gateway IP (`host.docker.internal`).
4. **No Docker detected** -- binds to `127.0.0.1` (loopback only).

This auto-detection means the daemon only exposes ports on all interfaces when Docker is actually running, minimizing unnecessary network exposure. Binding to `0.0.0.0` is **required** for Docker Desktop on macOS, where containers reach the host via a gateway IP (`host.docker.internal` resolves to an address like `192.168.65.254`), not via `127.0.0.1`.

When bound to `0.0.0.0`, this is safe because:

- The control protocol is designed for **untrusted clients**: all messages are validated, bounded (64 KB), and parsed strictly. Unknown message types, oversized fields, and malformed JSON are rejected.
- **Resource limits** prevent abuse: max 64 containers, max 128 forwards per container, max 1024 pending connections, 5 URL opens/sec rate limit.
- **No privileged operations** are exposed: the only host-side actions are binding loopback ports (Tier 2) and opening validated HTTP/HTTPS URLs.
- The `--bind-addr` and `--no-docker-detect` flags allow explicit control over the bind address.

### Tier 2: Forwarded ports (loopback-only)

| Listener | Bind address | Port | Source |
|----------|-------------|------|--------|
| Forwarded ports | `[::1]` then `127.0.0.1` | per-port | `src/host/listener.rs:48-62` |

Forwarded ports **always** bind to loopback only (`[::1]` with `127.0.0.1` fallback), regardless of the `--bind-addr` setting. These ports expose container services to the host user and must never be network-accessible.

### Configuring the bind address

Use `--bind-addr` or `--no-docker-detect` to control which interfaces the control and data ports listen on:

```bash
# Default: auto-detect (0.0.0.0 if Docker running, 127.0.0.1 otherwise)
dbr host-daemon

# Explicit bind address (overrides auto-detection)
dbr host-daemon --bind-addr 0.0.0.0

# Restrict to loopback only (skip Docker detection)
dbr host-daemon --no-docker-detect

# Restrict to loopback only (explicit)
dbr host-daemon --bind-addr 127.0.0.1
```

### Why this model is sufficient

The two-tier approach follows established patterns:

- **Docker Desktop** — publishes container ports to `localhost` only, but its own API socket listens on all interfaces within the VM
- **kubectl port-forward** — binds to `127.0.0.1` by default for user-facing ports
- **SSH local forwarding (`-L`)** — binds to loopback by default, but `GatewayPorts` allows binding to all interfaces when needed

On a typical developer workstation, the control and data ports accept only the `dbr` protocol (not arbitrary user traffic), making network exposure low-risk. Forwarded ports, which expose actual container services, remain loopback-only.

## Token Authentication

The control channel is protected by a shared-secret token that must be presented on every `Register` message.

### Token lifecycle

1. **Generation** — On first run, `dbr ensure` (or `dbr host-daemon`) generates a random 64-character hex token using the OS CSPRNG (`getrandom`).
2. **Storage** — The token is written to `~/.config/dbr/auth-token` with file mode `0600` (owner read/write only).
3. **Validation** — When a container sends `Register`, the host daemon compares the provided `auth_token` against its own token. Mismatches result in `RegisterAck{success: false}` and the connection is closed.

### Token resolution chain

Both daemons and CLI commands resolve the token using the same precedence:

| Priority | Source |
|----------|--------|
| 1 | `--auth-token <TOKEN>` CLI flag |
| 2 | `DCBRIDGE_AUTH_TOKEN` environment variable |
| 3 | `--auth-token-file <PATH>` CLI flag |
| 4 | Default file: `~/.config/dbr/auth-token` |

### Disabling authentication

Pass `--no-auth` to `dbr host-daemon` or `dbr ensure` to accept any `Register` regardless of token. This is intended for local development and testing only.

### Devcontainer feature integration

The devcontainer feature's entrypoint script reads the auth token from the default file path. When the host and container share the same home directory mount (common in devcontainer setups), no additional configuration is needed.

For containers without a shared home directory, pass the token via environment variable:

```bash
# In devcontainer.json or docker-compose.yml
"containerEnv": {
  "DCBRIDGE_AUTH_TOKEN": "${localEnv:DCBRIDGE_AUTH_TOKEN}"
}
```

## Unix Socket Forwarding Security

Unix socket forwarding bridges host-side sockets into containers. Several controls limit the attack surface.

### Path allowlist

Only sockets matching the configured `watch_paths` glob patterns are forwarded. The default configuration has an empty `watch_paths` list, meaning **no sockets are forwarded unless explicitly configured**.

### Symlink protection

The socket scanner uses `lstat` (`symlink_metadata` in Rust) and never follows symlinks. A symlink pointing to a socket outside the watch path is ignored. This prevents symlink-based path traversal attacks.

### Path length limit

Socket paths are limited to 108 characters (the Unix `sun_path` limit). Paths exceeding this are rejected.

### Mirror socket permissions

Container-side mirror sockets are created with file mode `0600` (owner read/write only). Other users in the container cannot connect to forwarded sockets.

### Resource limits

| Limit | Default | Configurable |
|-------|---------|-------------|
| Max forwarded sockets | 16 | `max_socket_forwards` in config TOML |
| Scan interval | 5000ms | `scan_interval_ms` in config TOML |
| Socket path length | 108 chars | No (Unix kernel limit) |

## URL Validation

When a container sends an `OpenUrl` message, the host daemon validates the URL before passing it to `open` (macOS) or `xdg-open` (Linux).

### Validation rules

1. **Scheme whitelist:** Only `http://` and `https://` are accepted (case-insensitive on both the container client and host daemon). All other schemes (`file://`, `ftp://`, `javascript:`, `data:`, etc.) are rejected with `BrowserError::InvalidScheme`.
2. **Length cap:** URLs longer than 2048 characters are rejected with `BrowserError::UrlTooLong`.
3. **Control character rejection:** URLs containing ASCII control characters (newlines, null bytes, tabs, etc.) are rejected with `BrowserError::InvalidCharacters`. This prevents log injection and argument confusion.
4. **Rate limiting:** A sliding window allows at most 5 URL opens per second. Excess requests are rejected with `BrowserError::RateLimited`.

### Command injection prevention

The URL is passed as a single argument to `Command::new("open").arg(url)` (or `xdg-open`). It is **not** passed through a shell, so shell metacharacters in the URL cannot cause command injection. See `src/host/browser.rs:144-150`.

### Port rewriting

When a container port is forwarded to a different host port (due to conflicts), `localhost:PORT` and `127.0.0.1:PORT` in URLs are rewritten to use the correct host port. Only these two host patterns are rewritten — external hostnames are never modified.

## Control Channel Hardening

### Message size limits

Every control message is bounded to **64 KB** (`MAX_MESSAGE_SIZE` in `src/control.rs:17`). The `read_message` function uses a bounded read strategy: it checks buffer length against the limit before allocating, preventing memory exhaustion from a peer that sends data without a newline terminator.

### Protocol strictness

- Messages must be valid JSON matching a known `Message` variant (internally tagged with `"type"`).
- Unknown `"type"` values are deserialization errors.
- Unknown fields within a known message type are deserialization errors (`deny_unknown_fields` is enforced on the `Message` enum and `ForwardInfo`).
- Missing required fields are deserialization errors.
- Port values (`u16`) that are negative or exceed 65535 are rejected by serde deserialization.
- The data channel handshake (`ConnectReady`) uses the same bounded read, then switches to raw TCP proxying — no further JSON parsing occurs on data connections.

### Field length limits

- `container_id` and `hostname` in `Register` messages are limited to 256 characters. Oversized identifiers are rejected with `RegisterAck{success: false}`.
- `conn_id` values in `ConnectReady` and `ConnectFailed` are limited to 128 characters. Oversized values are silently dropped.

### Connection limits

| Limit | Value | Location |
|-------|-------|----------|
| Max containers | 64 | `src/host/mod.rs:47` |
| Max forwards per container | 128 | `src/host/mod.rs:50` |
| Max pending connections | 1024 | `src/host/proxy.rs:65` |
| Control message size | 64 KB | `src/control.rs:17` |
| Container ID / hostname length | 256 chars | `src/host/mod.rs:53` |
| Connection ID length | 128 chars | `src/host/mod.rs:56` |
| URL length | 2048 chars | `src/host/browser.rs:15` |
| URL open rate | 5/sec | `src/host/browser.rs:18` |
| ConnectRequest timeout | 10 sec | `src/host/proxy.rs:20` |
| Heartbeat interval | 30 sec | `src/host/mod.rs:38` |
| Missed pongs before disconnect | 3 | `src/host/mod.rs:41` |
| Max forwarded sockets | 16 | `config.toml` `[socket_forwarding]` |
| Socket path length | 108 chars | Unix `sun_path` limit |

### Heartbeat and dead connection detection

The host daemon sends `Ping` messages every 30 seconds on each container's control connection. If 3 consecutive pings go unanswered, the container is considered dead and all its forwards are torn down. This prevents leaked listeners from accumulating when containers crash without a clean disconnect.

## No Elevated Privileges

Neither daemon requires root or elevated privileges:

- **Container daemon** reads `/proc/net/tcp` and `/proc/net/tcp6`, which are world-readable in Linux.
- **Container daemon** binds no ports — it only initiates outbound TCP connections.
- **Host daemon** binds to unprivileged ports (19285, 19286, and forwarded ports >= 1024 by default).
- **No Docker socket access.** The container daemon does not need `/var/run/docker.sock`. It communicates with the host daemon over TCP only.

The minimum forwarded port is configurable (default: 1024) to prevent privileged port forwarding without explicit opt-in.

## Auditability

All security-relevant events are logged with structured fields:

- Container registration and disconnection (with container ID and hostname)
- Port forward and unforward (with container ID, port, process name, PID)
- URL opens (with original and rewritten URL)
- Port conflicts and alternative port assignment
- Rate limit rejections
- Heartbeat timeouts
- Connection errors

### JSON log format

For integration with SIEM or log aggregation tools, use `--log-format json`:

```
dbr host-daemon --log-format json --log-file /var/log/dbr.json
```

Each log line is a JSON object with `timestamp`, `level`, `target`, `message`, and structured fields.

### Log levels

| Level | What is logged |
|-------|---------------|
| `error` | Unrecoverable failures (bind errors, internal errors) |
| `warn` | Rejected operations (rate limits, invalid URLs, oversized messages) |
| `info` | Normal operations (register, forward, unforward, URL open, disconnect) |
| `debug` | Connection-level details (accept, bridge, heartbeat) |
| `trace` | Raw protocol messages (for debugging) |

## Configuration Hardening

The TOML config file parser (`~/.config/dbr/config.toml`) uses `deny_unknown_fields` to reject unrecognized field names. This prevents silent misconfiguration from typos — a field like `contrl_port` (misspelled) will cause an error instead of being silently ignored with the default value taking effect.

## Supply Chain Security

- **Static binaries:** Release binaries are statically linked (musl on Linux), with no runtime dependencies.
- **SHA256 checksums:** Every release publishes checksums alongside the binaries for verification.
- **No unsafe code:** The project targets zero `unsafe` blocks. All memory safety is enforced by the Rust compiler.
- **Dependency auditing:** `cargo audit` and `cargo deny check` are run in CI to catch known vulnerabilities and license issues in dependencies.

### Minimal dependency tree

The project uses a focused set of well-maintained crates:

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime |
| `serde` + `serde_json` | Protocol serialization |
| `clap` | CLI parsing |
| `tracing` + `tracing-subscriber` | Structured logging |
| `thiserror` | Error types |
| `uuid` | Connection ID generation |
| `getrandom` | Cryptographic random token generation |
| `glob` | Unix socket path pattern matching |

## Verification Commands

### Verify binding addresses

After starting the host daemon, confirm the two-tier binding model:

**macOS:**
```bash
# Show all dbr listeners
lsof -iTCP -sTCP:LISTEN -P -n | grep dbr
```

**Linux:**
```bash
# Show all listeners bound by the dbr process
ss -tlnp | grep dbr
```

Expected output:
- **Control port (19285)** and **data port (19286)** should be on `*:PORT` or `0.0.0.0:PORT` (when Docker detected or `--bind-addr 0.0.0.0` used) or `127.0.0.1:PORT` (when no Docker detected, `--no-docker-detect` used, or `--bind-addr 127.0.0.1` used).
- **Forwarded ports** should always show `127.0.0.1:PORT` or `[::1]:PORT`. If you see `0.0.0.0` on a forwarded port, something is wrong.

### Verify no Docker socket access

```bash
# Inside the container — should NOT be present
ls -la /var/run/docker.sock
# Expected: No such file or directory
```

### Verify no unsafe code

```bash
# From the project root
cargo geiger
# Or search manually:
grep -r "unsafe" src/ --include="*.rs"
```

### Audit dependencies

```bash
cargo audit
cargo deny check
```

### Verify message size enforcement

```bash
# Send an oversized message to the control port — should be rejected
python3 -c "import socket; s=socket.socket(); s.connect(('127.0.0.1',19285)); s.send(b'x'*70000+b'\n'); print(s.recv(1024))"
```

The daemon should handle this gracefully without crashing or allocating excessive memory.
