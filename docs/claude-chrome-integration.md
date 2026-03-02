# Chrome for Claude Integration

Chrome for Claude (CFC) lets Claude Code read browser tabs, take screenshots,
and navigate pages via the `--claude-in-chrome-mcp` MCP server. Inside
devcontainers, the MCP server cannot reach the Chrome extension on the host.
`dbr` bridges this gap through Unix socket forwarding and a feature flag
override.

---

## Architecture

### Native Host Socket

The Chrome extension communicates with Claude Code through a native messaging
host binary. The lifecycle:

1. Chrome launches `chrome-native-host` via native messaging
2. The binary creates a Unix socket at
   `/tmp/claude-mcp-browser-bridge-<user>/<PID>.sock`
3. Claude Code's MCP server connects to this socket
4. Protocol: 4-byte little-endian length-prefixed JSON messages
5. Socket is recreated each time Chrome restarts (new PID)

### Two Connection Modes

The MCP server has two ways to reach the Chrome extension:

| Mode | Trigger | Transport |
|------|---------|-----------|
| **Cloud bridge** | `tengu_copper_bridge: true` | WebSocket to `wss://bridge.claudeusercontent.com` |
| **Local socket** | `tengu_copper_bridge: false` | Scans `/tmp/claude-mcp-browser-bridge-<user>/` for `.sock` files |

Client selection logic (in `CrR()`):
- If bridge config exists and flag is true -> WebSocket bridge client
- Else -> scan `getSocketPaths()` for local sockets, connect to first valid one

### Security Validation

When connecting via local socket, the MCP server calls
`validateSocketSecurity()` which checks:

- Parent directory has mode `0700`
- Socket file has mode `0600`
- Socket owner UID matches the current user

It uses `stat()` (follows symlinks), not `lstat()`. This means `dbr`'s mirror
sockets pass validation as long as they have correct permissions — which the
container daemon sets automatically.

---

## How dbr Bridges the Gap

### Socket Forwarding Configuration

Add the native host socket glob pattern to `~/.config/dbr/config.toml` on the
host:

```toml
[socket_forwarding]
watch_paths = ["/tmp/claude-mcp-browser-bridge-*/*.sock"]
```

When `dbr` discovers a matching socket on the host, it sends a `SocketForward`
message to the container daemon, which creates a mirror socket at the same path
inside the container. Connections to the mirror socket are bridged back to the
original host socket via `dbr`'s reverse data channel.

### Username Mismatch

The native host creates sockets under the **host** username:
```
/tmp/claude-mcp-browser-bridge-bradleybeddoes/12345.sock
```

Claude Code inside the container looks under the **container** username:
```
/tmp/claude-mcp-browser-bridge-vscode/
```

Since the MCP server's `validateSocketSecurity()` uses `stat()` (follows
symlinks), a symlink resolves this:

```bash
ln -sfn /tmp/claude-mcp-browser-bridge-bradleybeddoes \
        /tmp/claude-mcp-browser-bridge-vscode
```

The MCP server follows the symlink and finds the mirror socket.

---

## The Feature Flag Problem

### GrowthBook Cache

Claude Code uses GrowthBook for feature flags, cached locally in
`~/.claude.json` under `cachedGrowthBookFeatures`. The flag
`tengu_copper_bridge` controls which connection mode the MCP server uses.

Inside containers, this flag defaults to `true`, causing the MCP server to
attempt the cloud WebSocket bridge. The Chrome extension on the host is **not**
connected to this bridge, so the MCP server reports "No Chrome extension
connected" even when `dbr` has successfully forwarded the native host socket.

### The Race Condition

The GrowthBook SDK refreshes cached flags in the background after Claude Code
starts. This means:

1. You edit `~/.claude.json` to set `tengu_copper_bridge: false`
2. Claude Code starts and reads the patched value (local socket mode)
3. GrowthBook SDK refreshes in the background and **overwrites** the flag back
   to `true`
4. Next MCP server restart uses bridge mode again

The fix: patch the file **immediately before** launching Claude Code, every
time. The MCP server reads the flag at startup, so as long as the patch is
applied before the process starts, it works for that session.

### The Override

A shell wrapper function patches the flag before each launch:

```bash
claude-chrome() {
  python3 -c "
import json
f = '$HOME/.claude.json'
try:
    d = json.load(open(f))
except (FileNotFoundError, json.JSONDecodeError):
    d = {}
d.setdefault('cachedGrowthBookFeatures', {})['tengu_copper_bridge'] = False
json.dump(d, open(f, 'w'), indent=2)
" 2>/dev/null
  command claude "$@"
}
```

This sets `tengu_copper_bridge` to `false` before every launch, forcing the
local socket client which finds `dbr`'s forwarded native host socket.

---

## Troubleshooting

### "No Chrome extension connected"

The MCP server is using bridge mode instead of local socket mode.

1. Check the flag value:
   ```bash
   python3 -c "import json; print(json.load(open('$HOME/.claude.json')).get('cachedGrowthBookFeatures', {}).get('tengu_copper_bridge'))"
   ```
2. If it prints `True` or `None`, the override is not being applied. Use the
   `claude-chrome` wrapper function.
3. If it prints `False`, the MCP server may have already started with the old
   value. Restart Claude Code.

### Socket not found in container

1. Check `dbr status` — the Socket Forwards section should list the native host
   socket:
   ```
   Socket Forwards
   Host Path                                                    Container Path
   /tmp/claude-mcp-browser-bridge-user/12345.sock               /tmp/claude-mcp-browser-bridge-user/12345.sock
   ```
2. If no sockets are listed, verify `watch_paths` includes the correct glob
   pattern in `~/.config/dbr/config.toml`.
3. Verify Chrome is running on the host with the Claude Code extension
   installed. The native host socket only exists when Chrome is running.

### Permission denied on mirror socket

The MCP server's `validateSocketSecurity()` requires:
- Parent directory: mode `0700`
- Socket file: mode `0600`
- Owner UID matches current user

`dbr` creates mirror sockets with `0600` permissions. If the parent directory
has wrong permissions:

```bash
chmod 700 /tmp/claude-mcp-browser-bridge-$(whoami)
```

### Socket disappears after Chrome restart

When Chrome restarts, the native host binary gets a new PID, creating a new
socket (e.g., `67890.sock` instead of `12345.sock`). `dbr` detects this
automatically:

1. The old socket disappears — `dbr` sends `SocketUnforward`
2. The new socket appears — `dbr` sends `SocketForward`
3. The container daemon creates a new mirror socket

No action needed. The MCP server reconnects automatically when it detects the
socket change.

### Feature flag keeps resetting

GrowthBook refreshes cached flags in the background. The `claude-chrome`
wrapper applies the override before each launch, which is sufficient because
the MCP server reads the flag at startup.

If the flag resets mid-session (e.g., after a long-running Claude Code
session), the MCP server may switch back to bridge mode on its next internal
restart. Exiting and relaunching with `claude-chrome` resolves this.

### Extension detection

Claude Code caches whether the Chrome extension is installed in
`~/.claude.json` under `cachedChromeExtensionInstalled`. If this is `false` or
missing, Claude Code may not attempt to start the MCP server at all. Verify:

```bash
python3 -c "import json; print(json.load(open('$HOME/.claude.json')).get('cachedChromeExtensionInstalled'))"
```

If `false`, install the Claude Code Chrome extension on the host and restart
Claude Code so it detects the extension.
