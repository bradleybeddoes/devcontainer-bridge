# Devcontainer Bridge (`dbr`)

## Project overview

Devcontainer Bridge is a dual-daemon tool that transparently forwards TCP ports and opens browser URLs between Linux devcontainers and the macOS/Linux host. It solves the gap left by the devcontainer CLI (vs VS Code) where container-bound ports are unreachable from the host and browser-opening requests fail in headless containers.

**Primary use case:** Tools like Atlassian MCP's OAuth flow that bind a random port, open the host browser, and expect the callback on `localhost:PORT` — all of which fail without this bridge.

### How it works

Two daemons cooperate via a pair of TCP channels on loopback:

- **Host daemon** (`dbr host-daemon`) — long-lived process on the host. Binds control and data ports, accepts registrations from containers, binds per-port listeners for forwarded ports, and opens URLs in the host browser.
- **Container daemon** (`dbr container-daemon`) — runs inside each devcontainer. Polls `/proc/net/tcp` for new listeners, sends Forward/Unforward messages, and handles reverse data connections for proxying.

All TCP connections are initiated **container to host** (reverse connection model). This is required because macOS Docker Desktop cannot route to container IPs — containers run in a Linux VM with no direct host-to-container networking.

---

## Architecture

### Channels

| Channel | Default port | Default bind | Purpose |
|---------|-------------|-------------|---------|
| Control | `19285` | auto-detected | JSON-line protocol for registration, Forward/Unforward, OpenUrl, Ping/Pong, ListRequest/ListResponse |
| Data | `19286` | auto-detected | Reverse data connections from containers for TCP proxying |

Control and data ports use auto-detected bind addresses: `0.0.0.0` if Docker is running (so containers can reach the host via Docker Desktop's gateway IP), `127.0.0.1` otherwise. Configurable with `--bind-addr` or `--no-docker-detect`.

### Data flow for a proxied connection

```
1. Client connects to host:PORT (forwarded port listener)
2. Host daemon sends ConnectRequest{port, conn_id} via control channel
3. Container daemon connects to localhost:PORT inside container
4. Container daemon opens NEW TCP connection to host data port (19286)
5. Container daemon sends ConnectReady{conn_id} on data connection
6. Host daemon matches conn_id, bridges client <-> data connection
7. Bidirectional copy via tokio::io::copy_bidirectional
8. When either side closes, both connections tear down
```

### Key design decisions

- **Reverse data connections** — All connections flow container-to-host because macOS Docker Desktop cannot route to container IPs (containers live in a Linux VM). Same pattern as SSH `-R` reverse port forwarding.
- **TCP control channel (not Unix socket)** — No Docker volume mount or `devcontainer.json` modification required. Works through `host.docker.internal` DNS.
- **Two ports (control + data)** — Separates the framed JSON-line protocol from raw TCP byte streams cleanly. Control messages stay parseable; data connections switch to raw bytes after a single handshake line.
- **Two-tier binding** — Control and data ports use auto-detected bind addresses (`0.0.0.0` if Docker is running, `127.0.0.1` otherwise), configurable via `--bind-addr` or `--no-docker-detect`. Forwarded per-port listeners always bind to loopback (`[::1]`/`127.0.0.1`) only. The control protocol is hardened against untrusted clients (message limits, field validation, resource caps).
- **Single binary** — `dbr host-daemon` and `dbr container-daemon` are subcommands of the same binary. `dbr-open` is a hardlink for `BROWSER` env var integration. The container daemon is auto-started via the devcontainer feature's entrypoint script.

---

## Module map

```
src/
  main.rs               CLI entrypoint, clap dispatch, tracing init, dbr-open hardlink detection
  dbr/entrypoint.sh     Devcontainer feature entrypoint — starts container daemon on container boot
  cli.rs                Clap subcommand definitions (HostDaemon with --bind-addr/--no-docker-detect, ContainerDaemon, Status, Forward, Unforward, Open, Ensure)
  protocol.rs           All JSON-line message types (Register, Forward, ConnectRequest, OpenUrl, Ping/Pong, etc.)
  control.rs            TCP JSON-line framing (read_message/write_message), ControlListener, ControlConnection, connect()
  config.rs             Config struct, TOML file loading (~/.config/dbr/config.toml), env var layering (DCBRIDGE_HOST, DCBRIDGE_HOST_PORT)
  lib.rs                Crate root — re-exports config, container, control, host, protocol modules

  container/
    mod.rs              Container daemon main loop: host resolution, Register, scan/Forward/Unforward cycle, reconnection with exponential backoff, signal handling, parent PID monitoring
    scanner.rs          /proc/net/tcp + /proc/net/tcp6 parser (hex port extraction, LISTEN state filtering, inode-to-process resolution)
    filter.rs           Port filtering: --exclude-ports, --include-ports, --exclude-process regex, forwardPorts from devcontainer.json
    browser.rs          `dbr open` client: validates URL (http/https, 2048 char cap), connects to host, sends OpenUrl, waits for OpenUrlAck
    data.rs             Reverse data connection handler: on ConnectRequest, connects to local port + opens data connection to host + sends ConnectReady + bridges bidirectionally

  host/
    mod.rs              Host daemon main loop: control listener, data listener, container state management, Forward/Unforward/OpenUrl handling, heartbeat/keepalive (30s Ping, 3 missed Pongs = disconnect), connection draining, multi-container port conflict resolution, exit-on-idle
    listener.rs         Per-port TCP listener: binds [::1] with 127.0.0.1 fallback, accepts client connections, shutdown via watch channel
    proxy.rs            PendingConnections map (conn_id -> oneshot<TcpStream>), register/resolve/cancel pending, bridge_connection with 10s timeout, bidirectional copy
    browser.rs          BrowserOpener: URL validation, localhost port rewriting (container port -> host port), rate limiting (5/sec), open via `open` (macOS) / `xdg-open` (Linux)
    ensure.rs           `dbr ensure` logic: Ping/Pong health check, spawn background daemon if not running, PID file management, port conflict detection with actionable error

tests/
  integration/
    main.rs             Test harness entry
    forwarding.rs       13 integration tests: register/forward/unforward lifecycle, cleanup on disconnect, Ping/Pong, ListRequest/ListResponse, multi-container port conflict, full reverse proxy pipeline, data handshake, reconnection, ConnectFailed handling, bridge timeout
  e2e/
    Dockerfile          Minimal Alpine 3.21 image (python3, bash) for self-contained e2e tests
    docker-compose.yml  Compose definition (project: dbr-e2e, service: dbr-test-app) with host.docker.internal mapping
```

---

## Code conventions

- **Zero `unsafe` blocks** — no exceptions.
- **No `unwrap()`/`expect()` in production paths** — use `thiserror` error types with `?` propagation throughout. `unwrap()` is acceptable only in tests.
- **All public APIs have `///` doc comments** including `# Errors` sections
- **`cargo clippy -- -D warnings`** must pass
- **`cargo fmt`** must pass
- **Two-tier binding model** — control and data ports use auto-detected bind addresses (`0.0.0.0` if Docker is running, `127.0.0.1` otherwise), configurable via `--bind-addr` or `--no-docker-detect`. Forwarded per-port listeners (`bind_loopback`) must always bind to `127.0.0.1` or `[::1]`, never `0.0.0.0` or `[::]`.
- **Structured logging** via `tracing` crate — `info` for lifecycle events (register, forward, disconnect), `debug` for per-connection events, `warn` for recoverable errors, `error` for fatal errors
- **Error types** — each module has its own `thiserror` error enum. Use `#[from]` for transparent wrapping, `#[source]` for explicit chaining.

---

## How to build and test

### Build
```bash
cargo build              # debug build
cargo build --release    # optimized release build
```

### Test
```bash
cargo test               # all unit + integration tests
cargo test -- --nocapture  # with stdout/stderr output
```

### Lint
```bash
cargo clippy -- -D warnings
cargo fmt --check
```

### Cross-compilation (Linux container binary)

On Apple Silicon macOS, `cross` does not work reliably for `aarch64-unknown-linux-musl`. Use the Docker approach instead:

```bash
# Build a Linux aarch64 binary using Docker (works on Apple Silicon)
docker run --rm -v "$(pwd)":/src -w /src rust:1.93-alpine \
  sh -c 'apk add musl-dev && cargo build --release'
```

This produces a Linux binary at `target/release/dbr`. Note that this overwrites the macOS host binary — rebuild with `cargo build --release` if you need the host binary again.

### Deploy to a container
```bash
# Find your devcontainer
docker ps --format '{{.Names}}' | grep devcontainer | grep app

# Copy the Linux binary in
docker cp ./target/release/dbr <container>:/usr/local/bin/dbr

# Start the container daemon (normally auto-started by the devcontainer
# feature's entrypoint, but manual start is needed after a binary swap)
docker exec -d <container> dbr container-daemon
```

### End-to-end validation with `scripts/dev-test.sh`

The automated test script is **fully self-contained** — it spins up its own
minimal Alpine test container (`tests/e2e/`), runs all tests against it, and
tears it down on exit. No external devcontainers are required or touched.

```bash
# Full cycle: build both binaries, deploy to test container, run all tests
scripts/dev-test.sh

# Skip builds (use existing binaries)
scripts/dev-test.sh --skip-build
```

The script tests:
- Test container lifecycle (compose up, health check, compose down)
- Host daemon startup and control port binding
- Container daemon registration
- Automatic port detection and forwarding
- TCP data transfer through forwarded ports
- Port cleanup when listeners stop
- Browser URL opening (OpenUrl protocol flow)
- `dbr ensure` idempotency

Run this after making changes to validate nothing is broken. It exits non-zero if any test fails.

See [docs/development.md](docs/development.md) for the full development guide.

---

## How to add a new protocol message

1. **Define the variant** in `src/protocol.rs` → `Message` enum. Add serde attributes. Include `///` doc comment.
2. **Add serialization tests** in the `#[cfg(test)]` section of `protocol.rs` — roundtrip test at minimum.
3. **Handle in the receiving daemon:**
   - Container-originated messages → handle in `src/host/mod.rs` `handle_container_messages()`
   - Host-originated messages → handle in `src/container/mod.rs` `run_session()` select loop
   - CLI-originated messages → handle in the `first_msg` match in `handle_control_connection()`
4. **Send from the originating side** — add the send call in the appropriate location.
5. **Add integration test** in `tests/integration/forwarding.rs` validating the full round-trip.

---

## How to add a new CLI subcommand

1. **Add the variant** to `Command` enum in `src/cli.rs` with clap attributes (`#[command(name = "...")]`, `#[arg(...)]`).
2. **Add dispatch** in `src/main.rs` `match cli.command { ... }` — create the tokio runtime and call the implementation.
3. **Implement the handler** — either inline in `main.rs` for simple commands, or in the appropriate module (e.g., `host/ensure.rs`).
4. **Test** — unit test the handler logic, integration test the CLI round-trip if it touches the host daemon.

---

## Common extension points

### Adding UDP forwarding
- Add `Protocol::Udp` variant to `protocol.rs`
- Add a UDP scanner alongside TCP in `container/scanner.rs` (parse `/proc/net/udp`)
- Add UDP listener management in `host/listener.rs` (tokio `UdpSocket`)
- Add UDP proxying in `host/proxy.rs` (association-based, not connection-based)

### Adding a new control message
- Follow "How to add a new protocol message" above
- If it requires state, add fields to `ContainerState` or `HostState` in `host/mod.rs`

### Adding new host-side actions (e.g., clipboard, notifications)
- Add a new message type (e.g., `CopyToClipboard`)
- Handle in `host/mod.rs` `handle_container_messages()` match arm
- Implement the host action (similar pattern to `host/browser.rs`)
- Validate/sanitize input on the host side

---

## Security invariants

These MUST be maintained in all changes:

1. **Two-tier binding model** — Control and data listeners (`ControlListener::bind`, data listener in `host/mod.rs`) use auto-detected bind addresses: `0.0.0.0` when Docker is detected, `127.0.0.1` otherwise. Overridable with `--bind-addr` (explicit address) or `--no-docker-detect` (force loopback). Forwarded per-port listeners (`bind_loopback` in `host/listener.rs`) must always bind to `127.0.0.1` or `[::1]`. Never bind forwarded ports to `0.0.0.0` or `[::]`.
2. **URL scheme validation** — Only `http://` and `https://` URLs accepted for browser opening. Validated in both `container/browser.rs` and `host/browser.rs`.
3. **URL length cap** — 2048 characters maximum.
4. **Rate limiting** — Browser opens capped at 5/sec (sliding window in `host/browser.rs`).
5. **Message size limit** — Control messages capped at 64KB (`MAX_MESSAGE_SIZE` in `control.rs`). Bounded reads prevent OOM.
6. **No Docker socket access** — Container daemon reads only `/proc/net/tcp` (world-readable).
7. **No command injection** — URLs passed as arguments to `open`/`xdg-open` via `Command::new().arg()`, never via shell interpolation.
8. **No `unsafe` blocks** — no exceptions.
9. **Resource limits** — Max 64 containers (`MAX_CONTAINERS`), max 128 forwards per container (`MAX_FORWARDS_PER_CONTAINER`), max 1024 pending connections (`MAX_PENDING`).

---

## Testing approach

### Unit tests
- **Protocol roundtrip tests** (`protocol.rs`) — serialize/deserialize every message type, verify tagged JSON format, test malformed input.
- **Scanner fixtures** (`scanner.rs`) — hardcoded `/proc/net/tcp` content with known hex ports, test LISTEN state filtering, malformed line handling, deduplication across tcp/tcp6.
- **Filter logic** (`filter.rs`) — exclude, include, process regex, `devcontainer.json` forwardPorts, combined filters.
- **Control framing** (`control.rs`) — read/write roundtrip via in-memory buffer, EOF detection, oversized message rejection, listener/client connection roundtrip.
- **URL validation** (`container/browser.rs`, `host/browser.rs`) — scheme validation, length limits, port rewriting.
- **Config loading** (`config.rs`) — defaults, env var override, TOML file loading, precedence layering.

### Integration tests (`tests/integration/forwarding.rs`)
- Start real host daemon in-process on random ports
- Test full Register → Forward → ForwardAck → Unforward lifecycle
- Test cleanup on container disconnect (drop connection, verify listeners torn down)
- Test multi-container port conflict resolution
- Test full reverse proxy pipeline with echo server
- Test data port handshake parsing
- Test reconnection/re-registration
- Test ConnectFailed graceful handling
- Test bridge timeout when no data connection arrives

### E2E tests (`tests/e2e/` + `scripts/dev-test.sh`)
- Self-contained: spins up a minimal Alpine container via `docker compose`, no external devcontainers needed
- Container definition: `tests/e2e/Dockerfile` (Alpine 3.21 + python3 + bash) and `tests/e2e/docker-compose.yml` (project `dbr-e2e`, service `dbr-test-app`)
- Tests the full pipeline: build → deploy → host daemon → container daemon registration → port scan detection → TCP forwarding + data transfer → browser URL opening → idempotency → cleanup
- Container is torn down automatically via cleanup trap (`docker compose down`)

### Testing patterns
- Use `find_free_port()` (bind to port 0, return assigned port) to avoid port conflicts between parallel tests
- Use `tcp_pair()` helper for creating connected TCP stream pairs
- Use `tokio::time::timeout` to prevent hung tests
- Host daemon started with `exit_on_idle: true` for self-cleanup, or `abort()` for tests that need manual control

---

## Validating changes end-to-end

After making code changes, follow this sequence to validate:

### 1. Run unit tests and linting

```bash
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

### 2. Run the automated E2E test

```bash
scripts/dev-test.sh
```

This builds both binaries, starts a self-contained test container, deploys to it, and runs all end-to-end tests. The test container is torn down automatically on exit. No external devcontainers are required. It exits non-zero on any failure.

### 3. Build cycle details

The build-deploy-test cycle uses two different build approaches:

- **Host binary (macOS):** `cargo build --release` — standard Cargo build, produces a macOS arm64 binary
- **Container binary (Linux):** `docker run --rm -v $(pwd):/src -w /src rust:1.93-alpine sh -c 'apk add musl-dev && cargo build --release'` — builds inside an Alpine container, produces a Linux aarch64 binary

The `cross` crate is broken on Apple Silicon for the `aarch64-unknown-linux-musl` target. Always use the Docker approach for Linux binaries.

### 4. Quick iteration

For fast iteration when only host-side code changed:
```bash
cargo build --release && ./target/release/dbr host-daemon --log-level debug
```

For container-side changes, rebuild the Linux binary and redeploy:
```bash
docker run --rm -v "$(pwd)":/src -w /src rust:1.93-alpine sh -c 'apk add musl-dev && cargo build --release'
docker cp ./target/release/dbr <container>:/usr/local/bin/dbr
docker exec <container> pkill -f "dbr container-daemon" || true
docker exec -d <container> dbr container-daemon
```

Or use `scripts/dev-test.sh --skip-build` to re-test with existing binaries after a manual redeploy.
