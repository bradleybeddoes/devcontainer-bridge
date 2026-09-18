#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
entrypoint="$repo_root/features/dbr/entrypoint.sh"
test_root=$(mktemp -d)

# Matches only the supervising shell the entrypoint backgrounds, never a real
# daemon process, so a failed assertion cannot leave a retry loop running.
supervisor_pattern='dbr container-daemon --log-level warn && break'

stop_supervisors() {
  pkill -f "$supervisor_pattern" >/dev/null 2>&1 || true
}

# BSD pgrep has no -c, so count matches portably.
supervisor_count() {
  pgrep -f "$supervisor_pattern" 2>/dev/null | wc -l | tr -d ' '
}

trap 'stop_supervisors; rm -rf "$test_root"' EXIT

mock_dir="$test_root/bin"
mkdir -p "$mock_dir"

# Shortening the retry delay keeps the test fast while still bounding a
# runaway loop if an assertion fails before the supervisor is stopped.
cat > "$mock_dir/sleep" <<'EOF'
#!/bin/sh
exec /bin/sleep 0.1
EOF

# Records every invocation and fails until the attempt named by DBR_SUCCEED_ON.
cat > "$mock_dir/dbr" <<'EOF'
#!/bin/sh
echo "$*" >> "$DBR_CALLS"
[ "$(wc -l < "$DBR_CALLS")" -ge "$DBR_SUCCEED_ON" ]
EOF

chmod 0755 "$mock_dir/sleep" "$mock_dir/dbr"
export PATH="$mock_dir:$PATH"
export DBR_CALLS="$test_root/calls"

calls() {
  if [ -f "$DBR_CALLS" ]; then
    wc -l < "$DBR_CALLS" | tr -d ' '
  else
    echo 0
  fi
}

await_calls() {
  local want=$1 waited=0
  while [ "$(calls)" -lt "$want" ]; do
    if [ "$waited" -ge 100 ]; then
      echo "FAIL: expected $want daemon invocations, saw $(calls)" >&2
      exit 1
    fi
    /bin/sleep 0.1
    waited=$((waited + 1))
  done
}

start_case() {
  stop_supervisors
  : > "$DBR_CALLS"
}

# A daemon that keeps failing is restarted until it succeeds.
export DBR_SUCCEED_ON=3
start_case
output=$("$entrypoint" /bin/echo passthrough)
[ "$output" = "passthrough" ] || {
  echo "FAIL: entrypoint did not exec its arguments (got '$output')" >&2
  exit 1
}
await_calls 3
while read -r line; do
  [ "$line" = "container-daemon --log-level warn" ] || {
    echo "FAIL: unexpected daemon invocation '$line'" >&2
    exit 1
  }
done < "$DBR_CALLS"
echo "PASS: crashed daemon is restarted until it succeeds"

# A clean exit is a requested shutdown, so the daemon is not restarted.
export DBR_SUCCEED_ON=1
start_case
"$entrypoint" /bin/echo >/dev/null
await_calls 1
/bin/sleep 1
[ "$(calls)" -eq 1 ] || {
  echo "FAIL: daemon restarted after a clean exit ($(calls) invocations)" >&2
  exit 1
}
echo "PASS: cleanly exited daemon is not restarted"

# A daemon that is already supervised is not started a second time.
export DBR_SUCCEED_ON=99
start_case
"$entrypoint" /bin/echo >/dev/null
await_calls 1
before=$(calls)
"$entrypoint" /bin/echo >/dev/null
/bin/sleep 0.5
after=$(calls)
[ "$after" -gt "$before" ] || {
  echo "FAIL: supervisor stopped retrying" >&2
  exit 1
}
[ "$(supervisor_count)" -eq 1 ] || {
  echo "FAIL: a second supervisor was started ($(supervisor_count) running)" >&2
  exit 1
}
echo "PASS: a running supervisor is not duplicated"

echo "entrypoint tests passed"
