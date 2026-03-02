# CLI Developer Guide

This guide walks through setting up Devcontainer Bridge (`dbr`) for
terminal-driven devcontainer workflows. If you use the devcontainer CLI,
`docker compose`, tmux, and shell aliases instead of VS Code, this is for you.

---

## Prerequisites

- macOS or Linux host
- Docker Desktop (macOS) or Docker Engine 20.10+ (Linux)
- The `devcontainer` CLI (`npm install -g @devcontainers/cli`)
- `dbr` binary installed on both the host and inside your devcontainers

---

## Quick Start

### 1. Install the host binary

Download the binary for your platform from GitHub Releases, or use the install
script:

```bash
curl -fsSL https://github.com/bradleybeddoes/devcontainer-bridge/releases/latest/download/install.sh | bash
```

This places `dbr` at `~/.local/bin/dbr` (or `/usr/local/bin/dbr`). Verify:

```bash
dbr --version
```

### 2. Install inside devcontainers

Add the devcontainer feature to your project's `devcontainer.json`:

```jsonc
{
  "features": {
    "ghcr.io/bradleybeddoes/devcontainer-bridge/dbr:latest": {}
  }
}
```

This installs the `dbr` binary at `/usr/local/bin/dbr` and creates the
`dbr-open` hardlink. The container daemon starts automatically via the
feature's entrypoint, which runs on every container start.

### 3. Start the host daemon

```bash
dbr ensure
```

This generates an authentication token at `~/.config/dbr/auth-token` on first
run and starts the host daemon if it is not already running. It is idempotent --
running it multiple times is safe.

That's it. The container daemon is already running. Ports that processes bind
inside the container are now automatically forwarded to `localhost` on the host.

---

## Shell Integration

The recommended way to use `dbr` is to add `dbr ensure` to your existing
container startup shell function so the host daemon starts transparently:

```bash
# Example: add dbr ensure before devcontainer up
dbr ensure
devcontainer up --workspace-folder "$folder"
```

The container daemon starts automatically via the devcontainer feature — no
manual launch is needed on either `devcontainer up` or `docker compose restart`.

---

## Authentication

The control channel uses a shared-secret token to prevent unauthorized
container registrations.

### How it works

1. `dbr ensure` (or `dbr host-daemon`) generates a random 64-character hex
   token on first run, stored at `~/.config/dbr/auth-token` (mode 0600).
2. The container daemon reads the token from the same path (via the
   devcontainer feature's shared home directory) or from `DCBRIDGE_AUTH_TOKEN`.
3. On `Register`, the host validates the token. Mismatches are rejected.

### Token resolution

Both daemons resolve the token in this order:

1. `--auth-token <TOKEN>` CLI flag
2. `DCBRIDGE_AUTH_TOKEN` environment variable
3. `--auth-token-file <PATH>` CLI flag
4. Default file: `~/.config/dbr/auth-token`

### Without the devcontainer feature

If the container does not share the host's home directory, pass the token
explicitly:

```bash
# Via environment variable (recommended for Docker Compose)
export DCBRIDGE_AUTH_TOKEN="$(cat ~/.config/dbr/auth-token)"

# Via CLI flag
dbr container-daemon --auth-token-file /path/to/token
```

### Disabling authentication

For local development and testing:

```bash
dbr host-daemon --no-auth
dbr ensure --no-auth
```

---

## Unix Socket Forwarding

Host-side Unix sockets can be forwarded into containers for tools like SSH
agents, Chrome CDP debugging sockets, and GPG agents.

### Configuration

Add watch patterns to `~/.config/dbr/config.toml`:

```toml
[socket_forwarding]
watch_paths = ["/tmp/*.sock", "/run/user/1000/gnupg/S.gpg-agent"]
scan_interval_ms = 5000
max_socket_forwards = 16
container_path_prefix = "/tmp"
```

Or pass globs on the command line:

```bash
dbr host-daemon --socket-watch-paths "/tmp/*.sock,/run/user/1000/**/*.sock"
```

### Checking socket status

```bash
dbr status
```

The output includes a "Socket Forwards" section when sockets are active:

```
Container       Port   Host Port  Process    Since
myapp_dev       8080   8080       node       2m ago

Socket Forwards
Host Path                                          Container Path
/tmp/chrome-remote-debug.sock                      /tmp/chrome-remote-debug.sock
```

---

## Browser Integration with `BROWSER=dbr-open`

Many tools inside containers try to open URLs in a browser (e.g., OAuth flows,
documentation links). In a headless container this fails silently. `dbr` solves
this by forwarding the URL to the host daemon, which opens it in the host
browser.

### Setup

Add the following to your shell profile inside the container (e.g., via your
personal dotfiles `.zshrc` or `.bashrc`):

```bash
export BROWSER=dbr-open
```

The `dbr-open` hardlink is created automatically by the devcontainer feature at
`/usr/local/bin/dbr-open`. When a tool calls `dbr-open <URL>`, it runs
`dbr open <URL>` under the hood.

### How it works

1. A tool inside the container (e.g., `npm start`, `python -m webbrowser`)
   invokes `$BROWSER <url>`, which calls `dbr-open`.
2. `dbr-open` sends an `OpenUrl` message to the host daemon over the control
   channel.
3. The host daemon calls `open` (macOS) or `xdg-open` (Linux) on the host.
4. If the URL contains a `localhost` port that has been remapped (e.g.,
   container port 3000 forwarded to host port 3001), the URL is automatically
   rewritten before opening.

### Which tools respect `BROWSER`

- Node.js `open` package
- Python `webbrowser` module
- Rust `open` crate
- Most CLI tools that open a browser

### Optional: replace `xdg-open`

Some tools call `xdg-open` directly instead of reading `$BROWSER`. To cover
these, symlink `dbr-open` as `xdg-open`:

```bash
ln -sf /usr/local/bin/dbr-open /usr/local/bin/xdg-open
```

Only `http://` and `https://` URLs are accepted. Other schemes are rejected by
the host daemon.

---

## Checking Status with `dbr status`

See what ports are currently forwarded:

```bash
dbr status
```

Output:

```
Container       Port   Host Port  Process    Since
myapp_dev       8080   8080       node       2m ago
myapp_dev       39821  39821      mcp-auth   5s ago
other_proj      8080   8081       python     10m ago
```

For machine-readable output:

```bash
dbr status --json
```

This works from inside containers too — `--host` auto-resolves via
`DCBRIDGE_HOST` env var, then `host.docker.internal` DNS, then `127.0.0.1`.

If the host daemon is on a non-default port:

```bash
dbr status --control-port 19300
```

---

## Manual Port Forwarding

While automatic detection covers most cases, you can manually forward or
unforward ports:

```bash
# Forward a specific port
dbr forward 5432

# Remove a forward
dbr unforward 5432
```

---

## CLI Reference

### `dbr host-daemon`

Run the host-side daemon. Binds control and data ports on loopback.

| Flag | Default | Description |
|------|---------|-------------|
| `--control-port` | 19285 | Control channel port |
| `--data-port` | 19286 | Data channel port |
| `--bind-addr` | auto-detected | Bind address for control and data ports |
| `--no-docker-detect` | false | Skip Docker detection, bind to loopback |
| `--log-level` | info | Log level (trace, debug, info, warn, error) |
| `--log-format` | text | Log format (text or json) |
| `--log-file` | -- | Optional file path for log output |
| `--exit-on-idle` | false | Exit when the last container disconnects |
| `--browser-cmd` | -- | Custom browser command (overrides `open`/`xdg-open`) |
| `--auth-token` | -- | Authentication token |
| `--auth-token-file` | -- | Path to file containing auth token |
| `--no-auth` | false | Disable token authentication |
| `--socket-watch-paths` | -- | Comma-separated glob patterns for Unix sockets |
| `--socket-container-path-prefix` | -- | Prefix for container socket paths |
| `--socket-scan-interval-ms` | -- | Socket scan interval in milliseconds |
| `--no-socket-forwarding` | false | Disable socket forwarding |

### `dbr container-daemon`

Run the container-side daemon inside a devcontainer.

| Flag | Default | Description |
|------|---------|-------------|
| `--host-addr` | auto-detected | Host address (overrides auto-detection) |
| `--scan-interval` | 1000 | Port scan interval in milliseconds |
| `--exclude-ports` | -- | Comma-separated ports to never forward |
| `--log-level` | info | Log level (trace, debug, info, warn, error) |
| `--log-format` | text | Log format (text or json) |
| `--log-file` | -- | Optional file path for log output |
| `--auth-token` | -- | Authentication token |
| `--auth-token-file` | -- | Path to file containing auth token |

### `dbr ensure`

Start the host daemon if it is not already running. Safe to call repeatedly.

| Flag | Default | Description |
|------|---------|-------------|
| `--control-port` | 19285 | Control channel port |
| `--data-port` | 19286 | Data channel port |
| `--host` | auto-resolved | Host daemon address (IP or hostname) |
| `--auth-token` | -- | Authentication token |
| `--auth-token-file` | -- | Path to file containing auth token |
| `--no-auth` | false | Disable auth when spawning a new daemon |

### `dbr stop`

Stop a running host daemon by sending a shutdown message.

| Flag | Default | Description |
|------|---------|-------------|
| `--control-port` | 19285 | Control channel port |
| `--host` | auto-resolved | Host daemon address (IP or hostname) |
| `--auth-token` | -- | Authentication token |
| `--auth-token-file` | -- | Path to file containing auth token |

### `dbr restart`

Stop the running host daemon (if any) and start a fresh one. Equivalent to
`dbr stop` followed by `dbr ensure`. Useful after upgrading `dbr` or when the
daemon's auth token is out of sync with the token file.

| Flag | Default | Description |
|------|---------|-------------|
| `--control-port` | 19285 | Control channel port |
| `--data-port` | 19286 | Data channel port |
| `--host` | auto-resolved | Host daemon address (IP or hostname) |
| `--auth-token` | -- | Authentication token |
| `--auth-token-file` | -- | Path to file containing auth token |
| `--no-auth` | false | Disable auth when spawning a new daemon |

### `dbr status`

Show active port forwards across all containers.

| Flag | Default | Description |
|------|---------|-------------|
| `--control-port` | 19285 | Host daemon control port |
| `--host` | auto-resolved | Host daemon address (IP or hostname) |
| `--json` | false | Output as JSON |
| `--auth-token` | -- | Authentication token |
| `--auth-token-file` | -- | Path to file containing auth token |

### `dbr forward <PORT>`

Manually forward a container port.

| Flag | Default | Description |
|------|---------|-------------|
| `--control-port` | 19285 | Host daemon control port |
| `--host` | auto-resolved | Host daemon address (IP or hostname) |
| `--auth-token` | -- | Authentication token |
| `--auth-token-file` | -- | Path to file containing auth token |

### `dbr unforward <PORT>`

Manually remove a port forward.

| Flag | Default | Description |
|------|---------|-------------|
| `--control-port` | 19285 | Host daemon control port |
| `--host` | auto-resolved | Host daemon address (IP or hostname) |
| `--auth-token` | -- | Authentication token |
| `--auth-token-file` | -- | Path to file containing auth token |

### `dbr open <URL>`

Open a URL in the host browser. Only `http://` and `https://` URLs are accepted.

| Flag | Default | Description |
|------|---------|-------------|
| `--control-port` | 19285 | Host daemon control port |
| `--auth-token` | -- | Authentication token |
| `--auth-token-file` | -- | Path to file containing auth token |

---

## Host Address Resolution

The container daemon determines how to reach the host using this resolution
chain (first match wins):

1. `--host-addr` CLI flag
2. `DCBRIDGE_HOST` environment variable
3. `host.docker.internal` DNS (Docker Desktop, Docker Engine 20.10+)
4. Docker gateway IP from the container's default route
5. Fails with an actionable error listing what was tried

In most setups, step 3 works automatically with no configuration needed.

---

## Troubleshooting

### Port is not being forwarded

1. Check that the container daemon is running:
   ```bash
   docker compose -p myproject exec app pgrep -f "dbr container-daemon"
   ```
2. Check `dbr status` on the host to see active forwards.
3. Verify the process is actually listening inside the container:
   ```bash
   docker compose -p myproject exec app ss -tlnp
   ```
4. Check if the port is excluded. If you passed `--exclude-ports`, make sure
   the port is not in the list.
5. Check container daemon logs:
   ```bash
   docker compose -p myproject logs app 2>&1 | grep dbr
   ```

### Port is forwarded but connection refused on host

1. Verify the host listener is bound:
   ```bash
   lsof -i :8080 -n -P
   ```
2. Check that nothing else claimed the port before `dbr` could bind it.
   `dbr status` shows which host port was assigned -- it may differ from the
   container port if there was a conflict.

### Browser is not opening on the host

1. Confirm `BROWSER=dbr-open` is set inside the container:
   ```bash
   echo $BROWSER
   ```
2. Verify `dbr-open` exists:
   ```bash
   which dbr-open
   ```
3. Test manually:
   ```bash
   dbr open https://example.com
   ```
4. Check that the host daemon is running (`dbr status` from the host).

### Host daemon won't start -- port in use

If `dbr ensure` fails with a port conflict:

```
Error: Port 19285 is in use by another process.
Use --control-port to specify an alternative, and set DCBRIDGE_HOST_PORT
in your container environment to match.
```

Find what is using the port:

```bash
lsof -i :19285 -n -P
```

Either stop that process or use alternate ports:

```bash
dbr ensure --control-port 19300 --data-port 19301
```

Then set the environment variable in the container so the container daemon
connects to the right port:

```bash
export DCBRIDGE_HOST_PORT=19300
```

### Container daemon cannot reach host

If the container daemon fails to connect:

1. Test connectivity from inside the container:
   ```bash
   docker compose -p myproject exec app \
     bash -c "echo > /dev/tcp/host.docker.internal/19285"
   ```
2. If `host.docker.internal` does not resolve, set the host address explicitly:
   ```bash
   docker compose -p myproject exec -d app \
     dbr container-daemon --host-addr 172.17.0.1
   ```
3. Find your Docker gateway IP:
   ```bash
   docker compose -p myproject exec app \
     ip route | grep default | awk '{print $3}'
   ```

### Containers with egress firewall rules

If your devcontainer applies iptables egress filtering (e.g., HIPAA/SOC2 compliant
default-deny policies), you must allow the container daemon to reach the host
daemon's control and data ports. Only two fixed ports are needed — all forwarded
ports and browser URLs are tunnelled through these two channels:

```bash
# Resolve the host IP (host.docker.internal)
DOCKER_HOST_IP=$(getent hosts host.docker.internal | awk '{print $1}' || true)

if [ -n "$DOCKER_HOST_IP" ]; then
    iptables -A OUTPUT -d "$DOCKER_HOST_IP" -p tcp --dport 19285:19286 -j ACCEPT
    iptables -A INPUT -s "$DOCKER_HOST_IP" -p tcp --sport 19285:19286 \
        -m state --state ESTABLISHED -j ACCEPT
fi
```

These rules must be added **before** the default DROP policy. If you use
non-default ports (`--control-port` / `--data-port`), adjust accordingly.

### Auth token mismatch

If the container daemon fails to register with "authentication failed":

1. Verify the token matches on both sides:
   ```bash
   # Host
   cat ~/.config/dbr/auth-token

   # Container
   docker exec "$CONTAINER" cat ~/.config/dbr/auth-token
   ```
2. If tokens differ, copy the host token to the container or pass it via
   environment variable:
   ```bash
   export DCBRIDGE_AUTH_TOKEN="$(cat ~/.config/dbr/auth-token)"
   ```
3. For testing, disable authentication:
   ```bash
   dbr host-daemon --no-auth
   ```

### Socket not forwarded

If a host socket is not appearing in the container:

1. Verify `watch_paths` in `~/.config/dbr/config.toml` includes a matching glob.
2. Check that the socket exists on the host:
   ```bash
   ls -la /tmp/*.sock
   ```
3. Check `dbr status` for the socket forwards section.
4. Verify the host daemon was started with socket forwarding enabled
   (not `--no-socket-forwarding`).
5. Check host daemon logs for socket scan activity:
   ```bash
   dbr host-daemon --log-level debug
   ```

### Checking logs

Both daemons log to stderr by default. For the host daemon, use `--log-file`
for persistent logs:

```bash
dbr host-daemon --log-file ~/.config/dbr/daemon.log
```

Increase verbosity for debugging:

```bash
dbr host-daemon --log-level debug
dbr container-daemon --log-level debug
```

Use JSON format for structured log analysis:

```bash
dbr host-daemon --log-format json --log-file ~/.config/dbr/daemon.log
```
