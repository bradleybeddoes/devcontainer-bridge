# Development Container Bridge (`dbr`)

Automatic port forwarding, Unix socket bridging, and browser URL opening between devcontainers and the host machine for CLI users.

## The Problem

The [devcontainer CLI](https://github.com/devcontainers/cli) lacks three features that VS Code provides transparently:

1. **Port forwarding** — When a process inside a devcontainer listens on a port (e.g., a dev server on `:3000`), VS Code automatically makes it accessible on the host. The devcontainer CLI does not.
2. **Browser opening** — When a container process tries to open a URL (e.g., an OAuth callback), VS Code opens it in the host browser. The devcontainer CLI cannot.
3. **Unix socket forwarding** — Host-side Unix sockets (SSH agents, Chrome CDP debugging sockets, GPG agents) are inaccessible from inside containers. Tools that communicate via Unix sockets fail silently.

This breaks workflows like OAuth flows (which bind a random port, open a browser, and expect a callback on `localhost`), dev servers, and any tool that needs host-side access.

**`dbr` fixes all three**, with zero changes to shared `devcontainer.json` files.

> **VS Code users are not impacted.** The `dbr` binary is inert unless explicitly started. It does not set global environment variables, start background processes, or interfere with VS Code's own port forwarding. Teams can safely include the devcontainer feature — it's like having `nvim` installed but unused.

## Architecture

```
┌───────────────────── Host Machine (macOS/Linux) ────────────────────┐
│                                                                     │
│  dbr host-daemon (long-lived, auto-started)                         │
│  ├─ Control: :19285 (JSON-lines protocol)                           │
│  ├─ Data:    :19286 (reverse data connections)                      │
│  ├─ Accepts control connections from multiple containers            │
│  ├─ Binds loopback:PORT for each forwarded port                     │
│  ├─ Bridges client connections ↔ reverse data connections           │
│  ├─ Opens URLs in host browser (open/xdg-open)                      │
│  ├─ Scans for host Unix sockets (glob patterns)                      │
│  └─ Bridges socket connections ↔ reverse data connections            │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
          ▲ All connections initiated container → host
          │ via host.docker.internal
┌─────────┴────────── Devcontainer (Linux) ───────────────────────────┐
│                                                                     │
│  dbr container-daemon                                               │
│  ├─ Polls /proc/net/tcp every 1s for new listeners                  │
│  ├─ Sends Forward/Unforward to host via control channel             │
│  ├─ Opens reverse data connections for proxied traffic              │
│  ├─ Creates mirror Unix sockets for host-side sockets                │
│  ├─ Bridges mirror socket connections back to host                   │
│  └─ Reconnects automatically on connection loss                     │
│                                                                     │
│  BROWSER=dbr-open (set in personal dotfiles)                        │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

All TCP connections flow **container -> host** (reverse connection model). This is required because macOS Docker Desktop runs containers inside a Linux VM — the host cannot initiate connections into the container.

Control and data ports bind to an auto-detected address: `0.0.0.0` when Docker is running (so containers can reach the host), `127.0.0.1` otherwise. Override with `--bind-addr` or `--no-docker-detect`. Forwarded per-port listeners always bind to loopback only.

### TCP Data Flow

1. Client connects to `host:8080`
2. Host daemon sends `ConnectRequest` to container via control channel
3. Container daemon connects to `localhost:8080` inside the container
4. Container daemon opens a reverse TCP connection to `host:19286` (data channel)
5. Host daemon bridges the client and data connections bidirectionally
6. When either side closes, both connections tear down

### Socket Forwarding Data Flow

1. Host daemon discovers a Unix socket matching a configured glob pattern
2. Host sends `SocketForward` to container via control channel
3. Container creates a mirror `UnixListener` at the mapped path (mode 0600)
4. Client in container connects to the mirror socket
5. Container sends `SocketConnectRequest` to host via control channel
6. Host connects to the original Unix socket
7. Container opens a reverse TCP connection to `host:19286` (data channel)
8. Host bridges the Unix socket and data connection bidirectionally

## Quick Start

### 1. Install the host binary (macOS/Linux)

```bash
curl -fsSL https://github.com/bradleybeddoes/devcontainer-bridge/releases/latest/download/install.sh | bash
```

Or build from source:

```bash
cargo install --path .
```

### 2. Install in your devcontainer

Add the devcontainer feature to your project's `devcontainer.json`:

```jsonc
{
  "features": {
    "ghcr.io/bradleybeddoes/devcontainer-bridge/dbr:latest": {}
  }
}
```

This installs the `dbr` binary and creates the `dbr-open` hardlink. The container daemon starts automatically via the feature's entrypoint — no manual setup required.

### 3. Start the host daemon

```bash
dbr ensure   # starts host daemon if not already running, generates auth token
```

`dbr ensure` automatically generates an authentication token at `~/.config/dbr/auth-token` on first run. The devcontainer feature passes this token to the container daemon automatically.

If you're not using the devcontainer feature, pass the token manually:

```bash
dbr container-daemon --auth-token-file ~/.config/dbr/auth-token
```

### 4. Configure your personal dotfiles (container side)

Add to your `~/.zshrc` or `~/.bashrc` inside the container (via your personal dotfiles):

```bash
export BROWSER=dbr-open
```

### 5. Verify

```bash
# On the host, check active forwards
dbr status
```

```
Container       Port   Host Port  Process    Since
myapp_dev       8080   8080       node       2m ago
myapp_dev       39821  39821      mcp-auth   5s ago

Socket Forwards
Host Path                                          Container Path
/tmp/chrome-remote-debug.sock                      /tmp/chrome-remote-debug.sock
/run/user/1000/gnupg/S.gpg-agent                   /tmp/gnupg/S.gpg-agent
```

## CLI Usage

```
dbr host-daemon       Start the host-side daemon (--no-auth, --socket-watch-paths)
dbr container-daemon  Start the container-side daemon (--auth-token)
dbr ensure            Start host daemon if not already running (--no-auth)
dbr stop              Stop a running host daemon (--auth-token)
dbr restart           Stop + start the host daemon (--no-auth)
dbr status            Show active port and socket forwards (--auth-token)
dbr forward PORT      Manually forward a port (--auth-token)
dbr unforward PORT    Manually remove a port forward (--auth-token)
dbr open URL          Open a URL in the host browser (--auth-token)
```

### Host Daemon

```bash
dbr host-daemon [--bind-addr ADDR] [--no-docker-detect]
                [--control-port PORT] [--data-port PORT]
                [--browser-cmd COMMAND]
                [--auth-token TOKEN] [--auth-token-file PATH] [--no-auth]
                [--socket-watch-paths GLOBS]
                [--socket-container-path-prefix PATH]
                [--socket-scan-interval-ms MS]
                [--no-socket-forwarding]
                [--log-level LEVEL] [--log-format text|json]
                [--log-file PATH] [--exit-on-idle]
```

By default, the host daemon auto-detects whether to bind to `0.0.0.0` (Docker running) or `127.0.0.1` (no Docker). Use `--bind-addr` to set an explicit address, or `--no-docker-detect` to force loopback. Use `--browser-cmd` to override the default browser command (e.g., `/usr/bin/true` for headless testing). The daemon stays running after the last container disconnects; pass `--exit-on-idle` to exit instead.

Authentication is enabled by default. A token is generated at `~/.config/dbr/auth-token` on first run. Pass `--no-auth` to disable.

Socket forwarding is configured via `--socket-watch-paths` (comma-separated glob patterns) or the `[socket_forwarding]` section in `~/.config/dbr/config.toml`. Pass `--no-socket-forwarding` to disable.

### Container Daemon

```bash
dbr container-daemon [--host-addr ADDR] [--scan-interval MS]
                     [--exclude-ports 22,5432]
                     [--auth-token TOKEN] [--auth-token-file PATH]
                     [--log-level LEVEL] [--log-format text|json]
                     [--log-file PATH]
```

The container daemon resolves the host address in this order:
1. `--host-addr` flag
2. `DCBRIDGE_HOST` environment variable
3. `host.docker.internal` DNS
4. Docker gateway IP from the container's default route

Authentication token resolution order: `--auth-token` flag > `DCBRIDGE_AUTH_TOKEN` env var > `--auth-token-file` flag > default file (`~/.config/dbr/auth-token`).

### Ensure

```bash
dbr ensure [--control-port PORT] [--data-port PORT]
           [--host ADDR]
           [--auth-token TOKEN] [--auth-token-file PATH] [--no-auth]
```

Starts the host daemon if not already running. If a daemon is already running, verifies it is healthy via Ping/Pong. Generates the auth token file before spawning if one doesn't exist.

### Stop

```bash
dbr stop [--control-port PORT] [--host ADDR]
         [--auth-token TOKEN] [--auth-token-file PATH]
```

Sends a shutdown message to a running host daemon. Requires a valid auth token (unless the daemon was started with `--no-auth`).

### Restart

```bash
dbr restart [--control-port PORT] [--data-port PORT]
            [--host ADDR]
            [--auth-token TOKEN] [--auth-token-file PATH] [--no-auth]
```

Stops the running host daemon (if any) and starts a fresh one. Equivalent to `dbr stop` followed by `dbr ensure`. Useful after upgrading `dbr` or when the daemon's auth token is out of sync.

### Status

```bash
dbr status [--control-port PORT] [--host ADDR] [--json]
           [--auth-token TOKEN] [--auth-token-file PATH]
```

### Forward / Unforward

```bash
dbr forward PORT [--control-port PORT] [--host ADDR]
                 [--auth-token TOKEN] [--auth-token-file PATH]

dbr unforward PORT [--control-port PORT] [--host ADDR]
                   [--auth-token TOKEN] [--auth-token-file PATH]
```

### Open

```bash
dbr open URL [--control-port PORT]
             [--auth-token TOKEN] [--auth-token-file PATH]
```

### Browser Integration

Set `BROWSER=dbr-open` in your container shell profile. Most tools (Node.js `open`, Python `webbrowser`, Rust `open` crate) respect this variable. For tools that call `xdg-open` directly:

```bash
ln -sf /usr/local/bin/dbr-open /usr/local/bin/xdg-open
```

URLs are rewritten automatically — if container port `3000` is forwarded to host port `3001`, `http://localhost:3000/callback` becomes `http://localhost:3001/callback`.

## Unix Socket Forwarding

Host-side Unix sockets can be forwarded into containers, enabling tools like SSH agents, Chrome CDP, and GPG agents to work transparently.

### Configuration

Add watch patterns to `~/.config/dbr/config.toml`:

```toml
[socket_forwarding]
watch_paths = ["/tmp/claude-mcp-browser-bridge-*/*.sock", "/tmp/*.sock", "/run/user/1000/gnupg/S.gpg-agent"]
scan_interval_ms = 5000
max_socket_forwards = 16
container_path_prefix = "/tmp"
```

Or via CLI flags:

```bash
dbr host-daemon --socket-watch-paths "/tmp/*.sock,/run/user/1000/**/*.sock"
```

Setting `watch_paths` auto-enables socket forwarding. When `watch_paths` is empty (the default), no sockets are forwarded.

## Shell Integration

Add `dbr ensure` to your container startup workflow so the host daemon is
running before the container boots:

```bash
# Start host daemon (idempotent), then launch the devcontainer
dbr ensure
devcontainer up --workspace-folder "$folder"
```

The container daemon starts automatically via the devcontainer feature — no
manual launch needed.

## Multi-Container Support

One host daemon serves all running devcontainers. When multiple containers forward the same port, conflicts are resolved automatically:

- First container gets `host_port == container_port` (8080 -> 8080)
- Subsequent containers get the next available port (8080 -> 8081)
- `dbr status` shows the full mapping

## Security

- **Two-tier binding model** — Forwarded per-port listeners bind to **loopback only** (`127.0.0.1` / `[::1]`), never `0.0.0.0`. Control and data ports use auto-detected binding (`0.0.0.0` when Docker is detected, `127.0.0.1` otherwise), overridable via `--bind-addr` or `--no-docker-detect`
- Only `http://` and `https://` URLs accepted for browser opening
- No Docker socket access required
- No elevated privileges needed
- Rate limiting on browser opens and resource caps on connections, containers, and forwards
- All events logged with timestamps and container IDs
- Zero `unsafe` Rust code
- **Token authentication** — A random 64-character hex token is required on the control channel. Generated automatically on first run, stored at `~/.config/dbr/auth-token` with 0600 permissions. Disable with `--no-auth` for testing
- **Socket path allowlist** — Only sockets matching configured `watch_paths` globs are forwarded. Default: no sockets forwarded
- **Mirror socket permissions** — Container-side mirror sockets are created with mode 0600

See [docs/security.md](docs/security.md) for the full threat model and security guarantees.

## Building from Source

```bash
# Build
cargo build --release

# Run unit + integration tests
cargo test

# Run end-to-end tests (self-contained, no external devcontainers needed)
scripts/dev-test.sh

# Lint
cargo clippy -- -D warnings
cargo fmt --check
```

### Cross-Compilation

Static Linux binaries (for use inside containers):

```bash
# Linux aarch64 (for ARM64 containers, e.g., Apple Silicon)
docker run --rm -v "$(pwd)":/src -w /src rust:1.93-alpine \
  sh -c 'apk add musl-dev && cargo build --release'
```

On Apple Silicon macOS, `cross` does not work reliably for `aarch64-unknown-linux-musl`. The Docker approach above is recommended.

## Documentation

- [Architecture & Protocol](docs/architecture.md) — reverse connection model, protocol spec, data flow
- [Security Model](docs/security.md) — threat model, security guarantees, audit guidance
- [CLI Developer Guide](docs/cli-guide.md) — terminal workflow setup, troubleshooting
- [Team Adoption Guide](docs/team-guide.md) — adding to shared configs, VS Code compatibility FAQ
- [Development Guide](docs/development.md) — building, testing, debugging, and iterating on `dbr`
- [Chrome for Claude Integration](docs/claude-chrome-integration.md) — using Chrome for Claude from devcontainers

## License

See [LICENSE](LICENSE) for details.
