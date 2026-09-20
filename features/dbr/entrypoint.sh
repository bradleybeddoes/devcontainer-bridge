#!/bin/sh
set -e

# Start dbr container daemon in the background (if installed and not already running)
if command -v dbr >/dev/null 2>&1; then
  if ! pgrep -f "dbr container-daemon" >/dev/null 2>&1; then
    # Restart the daemon if it dies. It is the container's only link to the
    # host, nothing else notices its absence, and this entrypoint runs only at
    # container boot. Exit 0 is a requested shutdown and exit 2 is a failure
    # retrying cannot fix (a rejected token); both end the loop.
    nohup sh -c 'while :; do
  dbr container-daemon --log-level warn
  status=$?
  case "$status" in 0|2) break ;; esac
  sleep 5
done' >/dev/null 2>&1 &
  fi
fi

# Pass control to the next entrypoint/command in the chain
exec "$@"
