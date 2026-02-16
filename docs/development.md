# Development Guide

This guide covers building, testing, debugging, and iterating on `dbr` --
everything a developer needs to contribute to the project.

---

## Prerequisites

- **Rust toolchain** -- `rustup` with stable Rust 1.70+. Install from
  https://rustup.rs
- **Docker Desktop** (macOS) or Docker Engine 20.10+ (Linux) -- required for
  building the Linux binary and for the self-contained e2e test container

Verify your setup:

```bash
rustc --version     # Rust compiler
cargo --version     # Cargo package manager
docker --version    # Docker
```

---

## Building

### Host binary (macOS native)

```bash
cargo build --release
```

Produces `target/release/dbr` -- a macOS (arm64 or x86_64) binary for the
host daemon.

### Linux container binary (Docker approach)

On Apple Silicon (arm64) macOS, the `cross` crate does not work reliably for
the `aarch64-unknown-linux-musl` target. The working approach uses a Docker
container running the official Rust Alpine image:

```bash
docker run --rm \
  -v "$(pwd)":/src \
  -w /src \
  rust:1.93-alpine \
  sh -c 'apk add musl-dev && cargo build --release'
```

This builds a statically-linked Linux aarch64 binary at `target/release/dbr`
that runs inside ARM64 devcontainers. The same `target/release/dbr` path is
used because Docker mounts the project directory.

**Why not `cross`?** The `cross` crate uses Docker/QEMU to cross-compile, but
on Apple Silicon it has known issues with the `aarch64-unknown-linux-musl`
target. The direct Docker approach is simpler and more reliable.

**Important:** After a Docker build, `target/release/dbr` is a Linux binary.
If you need to run the host daemon, rebuild the macOS binary with
`cargo build --release` first.

### Build order for testing

When iterating, the typical order is:

1. Build the macOS host binary: `cargo build --release`
2. Build the Linux container binary via Docker (above)
3. Note: step 2 overwrites `target/release/dbr` with a Linux binary. If you
   need the host binary again, repeat step 1.

The `scripts/dev-test.sh` script handles this automatically.

---

## Running Locally

### Starting the host daemon

```bash
# Idempotent -- starts the daemon if not already running
./target/release/dbr ensure

# Or start directly with options
./target/release/dbr host-daemon --log-level debug
```

The host daemon binds two ports on loopback:
- Control channel: `127.0.0.1:19285`
- Data channel: `127.0.0.1:19286`

### Deploying to a container

Find your running devcontainer:

```bash
docker ps --format '{{.Names}}' | grep devcontainer | grep app
```

Copy the Linux binary and start the container daemon:

```bash
CONTAINER="your_project_devcontainer-app-1"

docker cp ./target/release/dbr "$CONTAINER:/usr/local/bin/dbr"
docker exec -d "$CONTAINER" dbr container-daemon
```

### Verifying

```bash
# Check status from the host
./target/release/dbr status

# JSON output for scripting
./target/release/dbr status --json
```

---

## The Dev-Test Script

`scripts/dev-test.sh` automates the full build-deploy-test cycle. It is the
primary validation tool for changes.

The script is **fully self-contained** -- it spins up its own minimal Alpine
test container from `tests/e2e/`, runs all tests against it, and tears it down
on exit. No external devcontainers are required or touched.

### Test container

The test environment lives in `tests/e2e/`:

- **`Dockerfile`** -- Alpine 3.21 with `python3` and `bash`. Stays alive with
  `sleep infinity`. No Rust toolchain needed (the prebuilt binary is copied in).
- **`docker-compose.yml`** -- Project name `dbr-e2e`, service `dbr-test-app`.
  Adds `extra_hosts: ["host.docker.internal:host-gateway"]` for container-to-host
  connectivity. Produces a container named `dbr-e2e-dbr-test-app-1`.

The container is started at the beginning of each run and torn down by the
cleanup trap, even on failure or `Ctrl-C`.

### What it does

1. **Build phase** -- Builds the macOS host binary and the Linux container
   binary using the Docker approach
2. **Test container phase** -- Starts the self-contained test container via
   `docker compose up -d --build`, waits for it to be running
3. **Deploy phase** -- Copies the Linux binary into the test container,
   verifies it runs
4. **Pre-flight** -- Verifies the test container can reach the host via
   `host.docker.internal`
5. **Host daemon tests** -- Starts the host daemon, verifies the control port
   is listening, checks `dbr status`
6. **Container daemon tests** -- Starts the container daemon in the test
   container, verifies registration
7. **Port forwarding tests** -- Starts a TCP listener in the container, waits
   for port detection, verifies the port appears in status, tests data transfer
   through the forwarded port, stops the listener, verifies the port disappears
8. **Browser opening tests** -- Tests the `dbr open` command from inside the
   container (valid and invalid URLs)
9. **Idempotency tests** -- Verifies `dbr ensure` can be called multiple times
10. **Cleanup** -- Kills all daemons, tears down the test container via
   `docker compose down`, reports pass/fail summary

### Flags

| Flag | Description |
|------|-------------|
| `--skip-build` | Skip the build phase (use existing binaries) |
| `--help` | Show usage |

### Example usage

```bash
# Full cycle
scripts/dev-test.sh

# After code changes, skip the first build to iterate faster
scripts/dev-test.sh --skip-build

# Typical iteration loop:
#   1. Edit source code
#   2. cargo build --release          (host binary)
#   3. scripts/dev-test.sh --skip-build  (test with existing Linux binary)
#   -- or --
#   3. scripts/dev-test.sh            (full rebuild + test)
```

### Example output

```
=== Build Phase (skipped) ===

[SKIP]  Build skipped via --skip-build

=== Test Container ===

[INFO]  Starting test container...
[PASS]  Test container is running

=== Deploy Phase ===

[INFO]  Deploying to dbr-e2e-dbr-test-app-1...
[PASS]  Deployed and verified in dbr-e2e-dbr-test-app-1

=== Pre-flight: Container Connectivity ===

[PASS]  Container dbr-e2e-dbr-test-app-1 has working outbound connectivity

=== Test Phase: Host Daemon ===

[PASS]  Control port 19285 is listening
[PASS]  dbr status works (host daemon responding)

=== Test Phase: Container Daemons ===

[PASS]  Container dbr-e2e-dbr-test-app-1 appears registered (status shows data)

=== Test Phase: Port Forwarding ===

[PASS]  Port 18888 detected and appears in dbr status
[PASS]  dbr status --json returns valid JSON with correct structure
[PASS]  Data passed through forwarded port successfully
[PASS]  Port 18888 removed from status after listener stopped

=== Test Phase: Browser Opening (OpenUrl) ===

[PASS]  dbr open https URL succeeded (exit 0, full OpenUrl->OpenUrlAck round-trip)
[PASS]  dbr open http URL succeeded (exit 0, full OpenUrl->OpenUrlAck round-trip)
[PASS]  dbr open correctly rejected ftp:// URL (exit 1)
[PASS]  dbr open correctly rejected empty URL (exit 1)
[PASS]  dbr open correctly rejected javascript: URL (exit 1)

=== Test Phase: Idempotency ===

[PASS]  dbr ensure detected running daemon ('already running')
[PASS]  dbr status still works after ensure

=== Cleanup ===

[INFO]  Stopping container daemon in dbr-e2e-dbr-test-app-1
[INFO]  Stopping host daemon
[INFO]  Tearing down test container...

=== Test Summary ===

  Passed:  17
  Failed:  0
  Skipped: 1

RESULT: PASSED
```

---

## Manual Testing Workflow

For targeted testing or debugging specific features.

### Port forwarding

```bash
# Terminal 1: Start host daemon with debug logging
./target/release/dbr host-daemon --log-level debug

# Terminal 2: Deploy and start container daemon
CONTAINER="your_project_devcontainer-app-1"
docker cp ./target/release/dbr "$CONTAINER:/usr/local/bin/dbr"
docker exec -d "$CONTAINER" dbr container-daemon --log-level debug

# Terminal 3: Start a listener inside the container
docker exec -it "$CONTAINER" nc -l -p 8080

# Terminal 4: Connect from the host
nc 127.0.0.1 8080

# Type in terminal 4 -- text should appear in terminal 3
# Check status:
./target/release/dbr status
```

### Browser opening

```bash
# From inside the container (container daemon must be running):
docker exec "$CONTAINER" dbr open http://localhost:8080/test

# The host daemon will call `open` (macOS) to open the URL in a browser.
# Check the host daemon logs for the OpenUrl/OpenUrlAck exchange.
```

### Port conflict resolution

```bash
# Start two containers, both with a service on port 8080
# Container 1 gets host port 8080
# Container 2 gets host port 8081 (next available)
./target/release/dbr status
# Shows the mapping for each container
```

---

## Testing Changes

### Unit and integration tests

```bash
# Run all tests
cargo test

# With output visible
cargo test -- --nocapture

# Run a specific test
cargo test test_name

# Run only integration tests
cargo test --test integration
```

### Linting

```bash
# Clippy (must pass with zero warnings)
cargo clippy -- -D warnings

# Format check
cargo fmt --check

# Auto-format
cargo fmt
```

### Iteration cycle

The fastest iteration loop depends on what you changed:

**Protocol or host-side changes:**
```bash
cargo build --release && ./target/release/dbr host-daemon --log-level debug
# Test manually or with dev-test.sh --skip-build
```

**Container-side changes:**
```bash
# Rebuild Linux binary
docker run --rm -v "$(pwd)":/src -w /src rust:1.93-alpine \
  sh -c 'apk add musl-dev && cargo build --release'

# Redeploy
docker cp ./target/release/dbr "$CONTAINER:/usr/local/bin/dbr"
docker exec "$CONTAINER" pkill -f "dbr container-daemon" || true
docker exec -d "$CONTAINER" dbr container-daemon --log-level debug
```

**Full validation:**
```bash
scripts/dev-test.sh
```

---

## Debugging

### Log levels

Both daemons support five log levels: `trace`, `debug`, `info`, `warn`,
`error`.

```bash
# Debug logging (recommended for development)
./target/release/dbr host-daemon --log-level debug

# Trace logging (very verbose, includes per-byte I/O)
docker exec -d "$CONTAINER" dbr container-daemon --log-level trace
```

### Viewing daemon output

By default, both daemons log to stderr. When started in background mode
(`docker exec -d`), you will not see output. Options:

```bash
# Write logs to a file
./target/release/dbr host-daemon --log-level debug --log-file /tmp/dbr-host.log
tail -f /tmp/dbr-host.log

# Container daemon with log file
docker exec -d "$CONTAINER" dbr container-daemon \
  --log-level debug --log-file /tmp/dbr-container.log
docker exec "$CONTAINER" tail -f /tmp/dbr-container.log

# JSON format for structured log analysis
./target/release/dbr host-daemon --log-format json --log-file /tmp/dbr-host.log
```

### Common issues

**"could not connect to host daemon"**

The host daemon is not running. Start it with `dbr ensure` or
`dbr host-daemon`.

**Container daemon cannot reach host**

Check connectivity from inside the container:

```bash
docker exec "$CONTAINER" sh -c "echo > /dev/tcp/host.docker.internal/19285"
```

If this fails, `host.docker.internal` may not be resolving. Try the gateway IP:

```bash
docker exec "$CONTAINER" ip route | grep default | awk '{print $3}'
# Use that IP with --host-addr
```

**Port forwarding not detected**

The container daemon scans `/proc/net/tcp` every 1 second. Verify the process
is actually listening:

```bash
docker exec "$CONTAINER" ss -tlnp
# or
docker exec "$CONTAINER" cat /proc/net/tcp
```

Check that the port is not excluded via `--exclude-ports`.

**Binary architecture mismatch**

If the binary copied into the container fails with "Exec format error", the
binary architecture does not match the container. On Apple Silicon, containers
are aarch64 Linux. Verify by running:

```bash
docker exec "$CONTAINER" uname -m
# Should output: aarch64
file ./target/release/dbr
# Should include: ELF 64-bit LSB ... ARM aarch64
```

Rebuild using the Docker approach if there is a mismatch.

**Port already in use**

If the host daemon fails to bind port 19285:

```bash
lsof -i :19285 -n -P
# Kill the conflicting process or use alternate ports:
./target/release/dbr ensure --control-port 19300 --data-port 19301
```

---

## Architecture for Testers

Understanding a few architectural details helps when writing or debugging tests.

### Port detection delay

The container daemon scans `/proc/net/tcp` every 1 second (configurable via
`--scan-interval`). After a process starts listening on a port, there is a
delay of up to 1 scan interval before the port is detected and a Forward
message is sent to the host. Tests should wait 2-3 seconds after starting a
listener before checking `dbr status`.

### What `dbr status` shows

`dbr status` sends a `ListRequest` to the host daemon and prints all active
forwards. Each entry shows:

- **Container** -- hostname of the container
- **Port** -- the port inside the container
- **Host Port** -- the port bound on the host (may differ if there was a
  conflict)
- **Process** -- name of the process listening on the port (if resolvable)
- **Since** -- how long ago the forward was established

### How to verify forwarding works

The full verification sequence:

1. Start a listener in the container (e.g., `nc -l -p 8080`)
2. Wait 2-3 seconds for the scanner to detect it
3. Check `dbr status` -- the port should appear
4. Connect to the host port from the host (e.g., `nc 127.0.0.1 8080`)
5. Send data in both directions -- it should pass through transparently
6. Stop the container listener
7. Wait 2-3 seconds
8. Check `dbr status` -- the port should be gone

### Reverse connection model

All TCP connections flow container-to-host. When a client connects to a
forwarded port on the host, the host daemon asks the container daemon to open
a **new** reverse connection back to the host data port. This is important to
understand when debugging connection issues -- the container must be able to
reach `host.docker.internal:19286`.

---

## Releasing

Releases involve two independent artifacts: the **dbr binary** (GitHub Releases)
and the **devcontainer feature** (GHCR OCI artifact). The feature's install
script downloads binaries from GitHub Releases, so a binary release must exist
before the feature is useful.

### Release pipeline overview

```
1. Push a v* tag          ──→  Release workflow builds binaries + creates GitHub Release
2. Trigger Publish Feature ──→  Publishes devcontainer feature OCI artifact to GHCR
```

### Step 1: Create a binary release

Tag the commit and push. The `release.yml` workflow triggers automatically on
`v*` tags:

```bash
git tag v0.1.0
git push origin v0.1.0
```

The workflow builds four binaries:

| Target | OS | Arch | Use |
|--------|----|------|-----|
| `x86_64-unknown-linux-musl` | Linux | x86_64 | Containers on Intel/AMD |
| `aarch64-unknown-linux-musl` | Linux | arm64 | Containers on Apple Silicon |
| `x86_64-apple-darwin` | macOS | x86_64 | Intel Mac host |
| `aarch64-apple-darwin` | macOS | arm64 | Apple Silicon host |

Each binary has a corresponding `.sha256` checksum file. The workflow creates a
GitHub Release with auto-generated release notes and attaches all artifacts.

### Step 2: Publish the devcontainer feature

After the GitHub Release exists, publish the devcontainer feature to GHCR:

1. Go to **Actions** → **Publish Feature** → **Run workflow**
2. The `devcontainers/action@v1` packages `features/dbr/` as an OCI artifact
3. The feature is published to `ghcr.io/bradleybeddoes/devcontainer-bridge/dbr`

The feature's `install.sh` downloads the correct binary for the container's
architecture from the GitHub Release at install time.

### How the feature version works

The feature has its own version in `features/dbr/devcontainer-feature.json` (e.g.,
`1.0.0`), which is independent of the binary version (e.g., `v0.1.0`). The
feature version controls the OCI artifact tag:

```jsonc
// Uses feature version 1.x, installs the latest binary release
"ghcr.io/bradleybeddoes/devcontainer-bridge/dbr:1": {}

// Pins a specific binary version
"ghcr.io/bradleybeddoes/devcontainer-bridge/dbr:1": {
  "version": "v0.1.0"
}
```

Bump the feature version in `devcontainer-feature.json` when the feature itself
changes (e.g., new options, install script updates). You do **not** need to bump
it for every binary release -- the feature defaults to downloading the latest
binary.

### When to re-publish the feature

- **New binary release only** (no feature changes) -- no need to re-publish.
  The feature's `install.sh` resolves `latest` via the GitHub API at container
  build time.
- **Feature definition changes** (new options, install script fixes) -- bump the
  version in `devcontainer-feature.json` and re-run the Publish Feature workflow.

### Verifying a release

After both workflows complete:

```bash
# Verify the GitHub Release has the expected artifacts
gh release view v0.1.0

# Verify the feature is available on GHCR
docker run --rm ghcr.io/bradleybeddoes/devcontainer-bridge/dbr:1 cat /devcontainer-feature.json

# Test in a real devcontainer by adding the feature to devcontainer.json
# and rebuilding the container
```

### GitHub repository settings

The Publish Feature workflow requires the repository to have **packages: write**
permission for `GITHUB_TOKEN`. For private repositories, ensure:

1. **Settings** → **Actions** → **General** → **Workflow permissions** is set to
   **Read and write permissions**
2. The GHCR package visibility matches your needs (private by default for
   private repos)
