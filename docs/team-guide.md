# Team Adoption Guide

This guide covers how to introduce Devcontainer Bridge (`dbr`) to a team that
uses shared `devcontainer.json` files, where some developers use VS Code and
others use the devcontainer CLI with terminal workflows.

---

## Adding the Devcontainer Feature

Add the `dbr` feature to your shared `devcontainer.json`:

```jsonc
{
  "features": {
    "ghcr.io/bradleybeddoes/devcontainer-bridge/dbr:latest": {}
  }
}
```

This installs two files inside the container:

- `/usr/local/bin/dbr` -- the main binary
- `/usr/local/bin/dbr-open` -- a hardlink used for browser integration

The feature **only installs the binary**. It does not:

- Start any background process
- Set any environment variables
- Modify any configuration files
- Listen on any ports
- Interfere with VS Code's port forwarding

The binary is completely inert unless a developer explicitly starts the
container daemon.

---

## Impact on VS Code Users

**None.**

VS Code has its own built-in port forwarding and browser opening. The `dbr`
binary installed by the feature sits unused, exactly like `vim` or `curl` --
present but not running.

Specifically:

- **No background processes** -- `dbr` does not start automatically. It must be
  invoked explicitly by a terminal developer's shell function (e.g., `dcup`).
- **No port conflicts** -- VS Code's port forwarding and `dbr` operate
  independently. Even if both were running (unlikely), they bind to different
  mechanisms (VS Code uses its own tunnel; `dbr` uses TCP on loopback).
- **No environment changes** -- The feature does not set `BROWSER` or any other
  variable. Terminal developers set `BROWSER=dbr-open` in their personal
  dotfiles, which VS Code does not load.
- **No container startup impact** -- The feature runs only at build time (a
  single binary copy). It adds zero time to container startup.
- **No network changes** -- The feature does not modify Docker networking,
  published ports, or `forwardPorts` in `devcontainer.json`.

VS Code developers can safely ignore the feature entirely. They will not notice
it is there.

---

## How Terminal Developers Activate It

Terminal developers who use the devcontainer CLI and tmux add two things to
their personal setup (not shared config):

1. **Shell functions** (`dcup`/`dctmux` aliases) that start the host and
   container daemons. See the [CLI Guide](cli-guide.md) for details.

2. **Dotfiles** with `export BROWSER=dbr-open` in their shell profile for
   browser integration.

Both are personal -- they live in the developer's dotfiles repo and local shell
configuration, not in any shared project file.

---

## FAQ

### Does this conflict with VS Code port forwarding?

No. VS Code uses its own internal mechanism to forward ports (over its Remote
Development tunnel). `dbr` uses a separate TCP control channel on
`127.0.0.1:19285`. The two systems are completely independent and do not
interact.

If both VS Code and `dbr` happened to forward the same container port, both
would work. In practice this does not occur because terminal developers do not
run VS Code, and VS Code developers do not start the `dbr` container daemon.

### Does this add latency to container startup?

Negligible. The devcontainer feature downloads a ~3MB static binary during
`devcontainer build` (cached in the Docker layer). On subsequent container
starts there is zero additional startup cost -- the binary is already in the
image.

### What if the devcontainer already publishes ports via Docker?

If `devcontainer.json` includes `forwardPorts` or Docker Compose publishes
ports (e.g., `ports: ["8080:8080"]`), `dbr` handles this gracefully:

- `dbr` detects that the host port is already in use and assigns an alternative
  host port automatically.
- `dbr status` shows the mapping so you can see which host port to use.
- The Docker-published port continues to work as before.

This means existing port configurations are not disrupted.

### What if two containers use the same port?

When multiple containers are running and both listen on the same port (e.g.,
port 8080):

- The first container to register gets the matching host port (8080 on host).
- Subsequent containers are assigned the next available port (e.g., 8081).
- `dbr status` shows all mappings clearly.

### Can I use a different control port?

Yes. If port 19285 conflicts with something in your environment:

```bash
# Host side
dbr ensure --control-port 19300 --data-port 19301

# Container side (via environment variable)
export DCBRIDGE_HOST_PORT=19300
```

### Do I need to modify Docker networking?

No. All `dbr` connections are initiated from the container to the host (via
`host.docker.internal`), which works out of the box with Docker Desktop and
Docker Engine 20.10+. No additional Docker networking configuration is needed.

### Is this secure?

Yes. All ports are bound to `127.0.0.1` (loopback only) -- the same security
model used by Docker Desktop, `kubectl port-forward`, and SSH local forwarding.
See [security.md](security.md) for the full threat model and security
guarantees.

### What happens if a developer removes the feature?

Nothing breaks. Terminal developers will need to install `dbr` manually inside
their containers (or add it via their personal dotfiles). VS Code developers
are unaffected since they never used it.

### What container images does it support?

`dbr` is a static binary with no runtime dependencies. It works in any Linux
container, including minimal images like `alpine`, `debian-slim`, and
`ubuntu`. The devcontainer feature provides both `amd64` and `arm64` binaries
and selects the correct one automatically.
