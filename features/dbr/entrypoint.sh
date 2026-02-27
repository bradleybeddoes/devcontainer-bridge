#!/bin/sh
set -e

# Start dbr container daemon in the background (if installed and not already running)
if command -v dbr >/dev/null 2>&1; then
  if ! pgrep -f "dbr container-daemon" >/dev/null 2>&1; then
    AUTH_ARGS=""
    if [ -n "$DCBRIDGE_AUTH_TOKEN" ]; then
      AUTH_ARGS="--auth-token $DCBRIDGE_AUTH_TOKEN"
    elif [ -f "${DCBRIDGE_AUTH_TOKEN_FILE:-/run/secrets/dbr-auth-token}" ]; then
      AUTH_ARGS="--auth-token-file ${DCBRIDGE_AUTH_TOKEN_FILE:-/run/secrets/dbr-auth-token}"
    fi
    nohup dbr container-daemon --log-level warn $AUTH_ARGS >/dev/null 2>&1 &
  fi
fi

# Pass control to the next entrypoint/command in the chain
exec "$@"
