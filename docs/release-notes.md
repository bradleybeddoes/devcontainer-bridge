# v0.5.1

A fix-only release. No new commands, flags, or configuration, and the wire
protocol is unchanged, so a v0.5.1 host and a v0.5.0 container interoperate in
either direction.

## A container daemon with a rejected token no longer respawns forever

The devcontainer feature's entrypoint restarts the daemon whenever it exits
unexpectedly, but the daemon exits deliberately when the host rejects its
authentication token, because retrying cannot help. The two combined into a
five-second respawn loop that also re-registered with the host on every
attempt. The daemon now signals this as a permanent failure and the supervisor
stops.

⚠️ This one needs **both halves upgraded**: the v0.5.1 binary and devcontainer
feature **0.3.1**. A new binary under the old feature, or the new feature with
an old binary, still loops.

## Idle containers stop burning CPU

The container daemon resolved a process name for every listening port on every
scan, once a second, walking `/proc/{pid}/fd` from the start each time. It only
ever uses the name the first time it forwards a port, so in steady state all of
that work was discarded, and ports owned by another user could never resolve at
all. An idle container with four forwarded ports spent **4.2% of a CPU core**
on it permanently.

Ports whose names are already known, and sockets the daemon has no permission
to inspect, are now skipped before any walk happens. Measured floor is roughly
a 140x reduction. Port detection is unchanged and still runs every second.

## A slow browser no longer blocks port forwarding

Handling `OpenUrl` held the browser lock for as long as the browser process
ran. Forward, unforward and container cleanup take that same lock, so one slow
`open` or `xdg-open` stalled port forwarding for every connected container,
with no bound on the wait. The lock is now released before the browser runs,
the wait is capped at ten seconds, and a browser that outlasts the cap is
terminated instead of being left running.

## Upgrade

Install the v0.5.1 host binary and update the devcontainer feature, then
rebuild the container:

```bash
curl -fsSL https://github.com/bradleybeddoes/devcontainer-bridge/releases/download/v0.5.1/install.sh | DBR_VERSION=v0.5.1 bash
```

Feature references pinned to `dbr:0` or `dbr:latest` pick up **0.3.1** on the
next rebuild with no edit. A reference pinned to an exact feature version needs
updating to `0.3.1`.

Written by Claude (claude-opus-5).
