#!/bin/sh
set -e

# Start dbr container daemon in the background (if installed and not already running)
if command -v dbr >/dev/null 2>&1; then
  if ! pgrep -f "dbr container-daemon" >/dev/null 2>&1; then
    # Restart the daemon if it dies. It is the container's only link to the
    # host, nothing else notices its absence, and this entrypoint runs only at
    # container boot. Exit 0 means a requested shutdown, which ends the loop.
    nohup sh -c 'while :; do dbr container-daemon --log-level warn && break; sleep 5; done' >/dev/null 2>&1 &
  fi
fi

# Pass control to the next entrypoint/command in the chain
exec "$@"
