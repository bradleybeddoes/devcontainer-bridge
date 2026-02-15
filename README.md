# Devcontainer Bridge (`dbr`)

Automatic port forwarding and browser URL opening between devcontainers and the host machine.

## The Problem

The [devcontainer CLI](https://github.com/devcontainers/cli) lacks two features that VS Code provides transparently:

1. **Port forwarding** — When a process inside a devcontainer listens on a port (e.g., a dev server on `:3000`), VS Code automatically makes it accessible on the host. The devcontainer CLI does not.
2. **Browser opening** — When a container process tries to open a URL (e.g., an OAuth callback), VS Code opens it in the host browser. The devcontainer CLI cannot.

This breaks workflows like OAuth flows (which bind a random port, open a browser, and expect a callback on `localhost`), dev servers, and any tool that needs host-side access.

**`dbr` fixes both**, with zero changes to shared `devcontainer.json` files.

> **VS Code users are not impacted.** The `dbr` binary is inert unless explicitly started. It does not set global environment variables, start background processes, or interfere with VS Code's own port forwarding. Teams can safely include the devcontainer feature — it's like having `nvim` installed but unused.

## Architecture

```
┌───────────────────── Host Machine (macOS/Linux) ────────────────────┐
│                                                                     │
│  dbr host-daemon (long-lived, auto-started)                         │
│  ├─ Control: 127.0.0.1:19285 (JSON-lines protocol)                 │
│  ├─ Data:    127.0.0.1:19286 (reverse data connections)             │
│  ├─ Accepts control connections from multiple containers            │
│  ├─ Binds loopback:PORT for each forwarded port                     │
│  ├─ Bridges client connections ↔ reverse data connections           │
│  └─ Opens URLs in host browser (open/xdg-open)                     │
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
│  └─ Reconnects automatically on connection loss                     │
│                                                                     │
│  BROWSER=dbr-open (set in personal dotfiles)                        │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

All TCP connections flow **container → host** (reverse connection model). This is required because macOS Docker Desktop runs containers inside a Linux VM — the host cannot initiate connections into the container.

### Data Flow

1. Client connects to `host:8080`
2. Host daemon sends `ConnectRequest` to container via control channel
3. Container daemon connects to `localhost:8080` inside the container
4. Container daemon opens a reverse TCP connection to `host:19286` (data channel)
5. Host daemon bridges the client and data connections bidirectionally
6. When either side closes, both connections tear down

## Quick Start

### 1. Install the host binary (macOS/Linux)

```bash
curl -fsSL https://github.com/yourorg/devcontainer-bridge/releases/latest/download/install.sh | bash
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
    "ghcr.io/yourorg/dbr-feature:latest": {}
  }
}
```

This installs the `dbr` binary and creates the `dbr-open` hardlink. It does **not** start any daemon or modify environment variables.

### 3. Configure your personal dotfiles (container side)

Add to your `~/.zshrc` or `~/.bashrc` inside the container (via your personal dotfiles):

```bash
export BROWSER=dbr-open
```

### 4. Start the daemons

On the host:

```bash
dbr ensure   # starts host daemon if not already running
```

Inside the container:

```bash
dbr container-daemon &
```

Or integrate into your shell aliases (see below).

### 5. Verify

```bash
# On the host, check active forwards
dbr status
```

```
Container       Port   Host Port  Process    Since
myapp_dev       8080   8080       node       2m ago
myapp_dev       39821  39821      mcp-auth   5s ago
```

## CLI Usage

```
dbr host-daemon       Start the host-side daemon
dbr container-daemon  Start the container-side daemon
dbr ensure            Start host daemon if not already running
dbr status            Show active port forwards across all containers
dbr forward PORT      Manually forward a port
dbr unforward PORT    Manually remove a port forward
dbr open URL          Open a URL in the host browser
```

### Host Daemon

```bash
dbr host-daemon [--control-port PORT] [--data-port PORT]
                [--log-level LEVEL] [--log-format text|json]
                [--log-file PATH] [--no-exit-on-idle]
```

By default, the host daemon exits when the last container disconnects. Use `--no-exit-on-idle` to keep it running.

### Container Daemon

```bash
dbr container-daemon [--host-addr ADDR] [--scan-interval MS]
                     [--exclude-ports 22,5432]
                     [--log-level LEVEL] [--log-format text|json]
                     [--log-file PATH]
```

The container daemon resolves the host address in this order:
1. `--host-addr` flag
2. `DCBRIDGE_HOST` environment variable
3. `host.docker.internal` DNS
4. Docker gateway IP from the container's default route

### Browser Integration

Set `BROWSER=dbr-open` in your container shell profile. Most tools (Node.js `open`, Python `webbrowser`, Rust `open` crate) respect this variable. For tools that call `xdg-open` directly:

```bash
ln -sf /usr/local/bin/dbr-open /usr/local/bin/xdg-open
```

URLs are rewritten automatically — if container port `3000` is forwarded to host port `3001`, `http://localhost:3000/callback` becomes `http://localhost:3001/callback`.

## Shell Integration Example

Integrate `dbr` into your existing devcontainer workflow aliases:

```bash
dcup() {
  local folder
  folder=$(_dc_workspace) || return 1

  # Ensure host daemon is running (idempotent)
  dbr ensure

  echo "Rebuilding: $folder"
  devcontainer up --workspace-folder "$folder" --remove-existing-container

  local project
  project=$(_dc_project) || return 1
  _dc_install_dotfiles "$project"

  # Start container daemon in the background
  docker compose -p "$project" exec -d app dbr container-daemon
}
```

## Multi-Container Support

One host daemon serves all running devcontainers. When multiple containers forward the same port, conflicts are resolved automatically:

- First container gets `host_port == container_port` (8080 → 8080)
- Subsequent containers get the next available port (8080 → 8081)
- `dbr status` shows the full mapping

## Security

- All listeners bind to **loopback only** (`127.0.0.1` / `[::1]`) — same security model as Docker Desktop and `kubectl port-forward`
- Only `http://` and `https://` URLs accepted for browser opening
- No Docker socket access required
- No elevated privileges needed
- Rate limiting on forward requests, URL opens, and connect requests
- All events logged with timestamps and container IDs
- Zero `unsafe` Rust code

See [docs/security.md](docs/security.md) for the full threat model and security guarantees.

## Building from Source

```bash
# Build
cargo build --release

# Run tests
cargo test

# Lint
cargo clippy -- -D warnings
cargo fmt --check
```

### Cross-Compilation

Static Linux binaries (for use inside containers):

```bash
cargo build --release --target x86_64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
```

## Documentation

- [Architecture & Protocol](docs/architecture.md) — reverse connection model, protocol spec, data flow
- [Security Model](docs/security.md) — threat model, security guarantees, audit guidance
- [CLI Developer Guide](docs/cli-guide.md) — terminal workflow setup, troubleshooting
- [Team Adoption Guide](docs/team-guide.md) — adding to shared configs, VS Code compatibility FAQ
- [Development Guide](docs/development.md) — building, testing, debugging, and iterating on `dbr`

## License

See [LICENSE](LICENSE) for details.
