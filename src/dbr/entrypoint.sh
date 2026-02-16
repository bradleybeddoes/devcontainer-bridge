#!/bin/sh
set -e

# Start dbr container daemon in the background (if installed and not already running)
if command -v dbr >/dev/null 2>&1; then
  if ! pgrep -f "dbr container-daemon" >/dev/null 2>&1; then
    nohup dbr container-daemon --log-level warn >/dev/null 2>&1 &
  fi
fi

# Pass control to the next entrypoint/command in the chain
exec "$@"
