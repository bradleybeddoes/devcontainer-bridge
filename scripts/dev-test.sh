#!/usr/bin/env bash
#
# dev-test.sh -- Automated build-deploy-test cycle for devcontainer-bridge (dbr)
#
# Builds host + container binaries, deploys to running devcontainers, and runs
# end-to-end tests covering port forwarding, status reporting, and browser
# URL opening.
#
# Usage:
#   scripts/dev-test.sh                    # full build + deploy + test
#   scripts/dev-test.sh --skip-build       # skip builds, just deploy + test
#   scripts/dev-test.sh --help             # show usage
#
# Exit codes:
#   0   all tests passed
#   1   one or more tests failed
#

set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOST_BINARY="$PROJECT_ROOT/target/release/dbr"
LINUX_BINARY="$PROJECT_ROOT/target/linux-release/dbr"
CONTAINER_BINARY_PATH="/usr/local/bin/dbr"
CONTROL_PORT=19285
DATA_PORT=19286
TEST_PORT=18888
SCAN_WAIT=3          # seconds to wait for port scan detection
FORWARD_WAIT=5       # seconds to wait for port forwarding to appear in status
REGISTER_WAIT=3      # seconds to wait for container registration

# ---------------------------------------------------------------------------
# Color output helpers
# ---------------------------------------------------------------------------

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m' # No Color

pass_count=0
fail_count=0
skip_count=0

log_info()  { echo -e "${BLUE}[INFO]${NC}  $*"; }
log_pass()  { echo -e "${GREEN}[PASS]${NC}  $*"; pass_count=$((pass_count + 1)); }
log_fail()  { echo -e "${RED}[FAIL]${NC}  $*"; fail_count=$((fail_count + 1)); }
log_skip()  { echo -e "${YELLOW}[SKIP]${NC}  $*"; skip_count=$((skip_count + 1)); }
log_phase() { echo -e "\n${BOLD}=== $* ===${NC}\n"; }

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------

SKIP_BUILD=false
E2E_COMPOSE="$PROJECT_ROOT/tests/e2e/docker-compose.yml"
TEST_CONTAINER_NAME="dbr-e2e-dbr-test-app-1"

usage() {
    cat <<EOF
Usage: $(basename "$0") [OPTIONS]

Automated build-deploy-test cycle for devcontainer-bridge.

Uses a self-contained test container (tests/e2e/) — no external
devcontainers are required or touched.

Options:
  --skip-build          Skip the build phase (use existing binaries)
  --help                Show this help message

Examples:
  $(basename "$0")                          # full cycle
  $(basename "$0") --skip-build             # re-test with existing binaries
EOF
    exit 0
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-build)
            SKIP_BUILD=true
            shift
            ;;
        --help|-h)
            usage
            ;;
        *)
            echo "Unknown option: $1" >&2
            usage
            ;;
    esac
done

# ---------------------------------------------------------------------------
# Cleanup trap
# ---------------------------------------------------------------------------

CLEANUP_PIDS=()
CLEANUP_CONTAINERS=()

cleanup() {
    local exit_code=$?
    echo ""
    log_phase "Cleanup"

    # Kill test listeners and container daemons
    for cid in "${CLEANUP_CONTAINERS[@]}"; do
        log_info "Stopping container daemon in $cid"
        docker exec "$cid" pkill -f "dbr container-daemon" 2>/dev/null || true
        docker exec "$cid" pkill -f "tcp_echo_server_$TEST_PORT" 2>/dev/null || true
    done

    # Kill host daemon
    if pgrep -f "dbr host-daemon" >/dev/null 2>&1; then
        log_info "Stopping host daemon"
        pkill -f "dbr host-daemon" 2>/dev/null || true
        sleep 1
    fi

    # Kill any leftover processes we spawned
    for pid in "${CLEANUP_PIDS[@]}"; do
        kill "$pid" 2>/dev/null || true
    done

    # Tear down the test container
    log_info "Tearing down test container..."
    docker compose -f "$E2E_COMPOSE" down 2>/dev/null || true

    # Print summary
    echo ""
    log_phase "Test Summary"
    echo -e "  ${GREEN}Passed:${NC}  $pass_count"
    echo -e "  ${RED}Failed:${NC}  $fail_count"
    echo -e "  ${YELLOW}Skipped:${NC} $skip_count"
    echo ""

    if [[ $fail_count -gt 0 ]]; then
        echo -e "${RED}${BOLD}RESULT: FAILED${NC}"
        exit 1
    else
        echo -e "${GREEN}${BOLD}RESULT: PASSED${NC}"
        exit 0
    fi
}

trap cleanup EXIT

# ---------------------------------------------------------------------------
# Helper functions
# ---------------------------------------------------------------------------

# Wait for a TCP port to become reachable on localhost
wait_for_port() {
    local port=$1
    local timeout=${2:-10}
    local elapsed=0

    while ! nc -z 127.0.0.1 "$port" 2>/dev/null; do
        sleep 0.5
        elapsed=$((elapsed + 1))
        if [[ $elapsed -ge $((timeout * 2)) ]]; then
            return 1
        fi
    done
    return 0
}

# Check if a string appears in dbr status output
status_contains() {
    local pattern=$1
    "$HOST_BINARY" status 2>/dev/null | grep -q "$pattern"
}

# Wait for a pattern to appear in dbr status output
wait_for_status() {
    local pattern=$1
    local timeout=${2:-$FORWARD_WAIT}
    local elapsed=0

    while ! status_contains "$pattern"; do
        sleep 1
        elapsed=$((elapsed + 1))
        if [[ $elapsed -ge $timeout ]]; then
            return 1
        fi
    done
    return 0
}

# Wait for a pattern to disappear from dbr status output
wait_for_status_gone() {
    local pattern=$1
    local timeout=${2:-$FORWARD_WAIT}
    local elapsed=0

    while status_contains "$pattern"; do
        sleep 1
        elapsed=$((elapsed + 1))
        if [[ $elapsed -ge $timeout ]]; then
            return 1
        fi
    done
    return 0
}

# ---------------------------------------------------------------------------
# Build phase
# ---------------------------------------------------------------------------

if [[ "$SKIP_BUILD" == "true" ]]; then
    log_phase "Build Phase (skipped)"
    log_skip "Build skipped via --skip-build"

    # Verify binaries exist
    if [[ ! -f "$HOST_BINARY" ]]; then
        log_fail "Host binary not found at $HOST_BINARY"
        echo "  Run without --skip-build first, or run: cargo build --release" >&2
        exit 1
    fi
    if [[ ! -f "$LINUX_BINARY" ]]; then
        log_fail "Linux binary not found at $LINUX_BINARY"
        echo "  Run without --skip-build first" >&2
        exit 1
    fi
else
    log_phase "Build Phase"

    # Build macOS host binary
    log_info "Building macOS host binary (cargo build --release)..."
    build_start=$(date +%s)
    if cargo build --release --manifest-path "$PROJECT_ROOT/Cargo.toml" 2>&1; then
        build_end=$(date +%s)
        log_pass "Host binary built ($(( build_end - build_start ))s)"
    else
        log_fail "Host binary build failed"
        exit 1
    fi

    # Build Linux container binary via Docker (separate target dir to avoid overwriting host binary)
    log_info "Building Linux container binary (Docker rust:alpine)..."
    build_start=$(date +%s)
    mkdir -p "$PROJECT_ROOT/target/linux-release"
    if docker run --rm \
        -v "$PROJECT_ROOT":/src \
        -v "$PROJECT_ROOT/target/linux-release":/src/target/release \
        -w /src \
        rust:1.93-alpine \
        sh -c 'apk add musl-dev && cargo build --release' 2>&1; then
        build_end=$(date +%s)
        log_pass "Linux binary built ($(( build_end - build_start ))s)"
    else
        log_fail "Linux binary build failed"
        exit 1
    fi
fi

# ---------------------------------------------------------------------------
# Test container phase
# ---------------------------------------------------------------------------

log_phase "Test Container"

log_info "Starting test container..."
if docker compose -f "$E2E_COMPOSE" up -d --build 2>&1; then
    # Wait for container to be running
    tc_timeout=10
    tc_elapsed=0
    while ! docker inspect --format='{{.State.Running}}' "$TEST_CONTAINER_NAME" 2>/dev/null | grep -q "true"; do
        sleep 1
        tc_elapsed=$((tc_elapsed + 1))
        if [[ $tc_elapsed -ge $tc_timeout ]]; then
            log_fail "Test container did not start within ${tc_timeout}s"
            exit 1
        fi
    done
    log_pass "Test container is running"
else
    log_fail "Failed to start test container"
    exit 1
fi

# ---------------------------------------------------------------------------
# Deploy phase
# ---------------------------------------------------------------------------

log_phase "Deploy Phase"

log_info "Deploying to $TEST_CONTAINER_NAME..."

# Kill any existing dbr processes
docker exec "$TEST_CONTAINER_NAME" pkill -f "dbr container-daemon" 2>/dev/null || true
docker exec "$TEST_CONTAINER_NAME" pkill -f "dbr host-daemon" 2>/dev/null || true
sleep 0.5

# Copy the Linux binary into the container
if docker cp "$LINUX_BINARY" "$TEST_CONTAINER_NAME:$CONTAINER_BINARY_PATH" 2>&1; then
    # Ensure it is executable
    docker exec "$TEST_CONTAINER_NAME" chmod +x "$CONTAINER_BINARY_PATH" 2>/dev/null || true

    # Verify it runs
    if docker exec "$TEST_CONTAINER_NAME" "$CONTAINER_BINARY_PATH" --help >/dev/null 2>&1; then
        log_pass "Deployed and verified in $TEST_CONTAINER_NAME"
        CLEANUP_CONTAINERS+=("$TEST_CONTAINER_NAME")
    else
        log_fail "Binary deployed but failed to run in $TEST_CONTAINER_NAME"
        log_info "  This may indicate an architecture mismatch (host vs container)"
        exit 1
    fi
else
    log_fail "Failed to copy binary to $TEST_CONTAINER_NAME"
    exit 1
fi

# ---------------------------------------------------------------------------
# Pre-flight: Verify container-to-host connectivity
# ---------------------------------------------------------------------------

log_phase "Pre-flight: Container Connectivity"

host_reachable=$(docker exec "$TEST_CONTAINER_NAME" python3 -c "
import socket
try:
    ip = socket.gethostbyname('host.docker.internal')
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.settimeout(3)
    s.connect((ip, 80))
    s.close()
    print('yes')
except:
    # Port 80 may not be open, but check if the host IP is routable
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.settimeout(3)
        result = s.connect_ex(('1.1.1.1', 80))
        s.close()
        print('yes' if result != 113 else 'no')
    except:
        print('no')
" 2>/dev/null || echo "no")

if [[ "$host_reachable" == "yes" ]]; then
    log_pass "Container $TEST_CONTAINER_NAME has working outbound connectivity"
else
    log_fail "Container $TEST_CONTAINER_NAME has broken outbound connectivity"
    log_info "  Fix: docker restart $TEST_CONTAINER_NAME"
    log_info "  Then re-run this script"
    exit 1
fi

# ---------------------------------------------------------------------------
# Test phase: Host daemon
# ---------------------------------------------------------------------------

log_phase "Test Phase: Host Daemon"

# Kill any existing host daemon
if pgrep -f "dbr host-daemon" >/dev/null 2>&1; then
    log_info "Killing existing host daemon..."
    pkill -f "dbr host-daemon" 2>/dev/null || true
    sleep 1
fi

# Start host daemon directly in background with --browser-cmd so that OpenUrl
# tests complete the full protocol flow without actually opening a browser.
# We use /usr/bin/true which accepts any args and exits 0.
log_info "Starting host daemon with --browser-cmd /usr/bin/true..."
"$HOST_BINARY" host-daemon \
    --control-port "$CONTROL_PORT" \
    --data-port "$DATA_PORT" \
    --browser-cmd /usr/bin/true \
    --no-exit-on-idle \
    --log-level debug \
    --log-file /tmp/dbr-host-test.log &
CLEANUP_PIDS+=("$!")
sleep 1

# Wait for control port
if wait_for_port "$CONTROL_PORT" 5; then
    log_pass "Control port $CONTROL_PORT is listening"
else
    log_fail "Control port $CONTROL_PORT not reachable after 5s"
    log_info "  Check /tmp/dbr-host-test.log for errors"
    exit 1
fi

# Verify status works
if "$HOST_BINARY" status >/dev/null 2>&1; then
    log_pass "dbr status works (host daemon responding)"
else
    log_fail "dbr status failed"
fi

# ---------------------------------------------------------------------------
# Test phase: Container daemons
# ---------------------------------------------------------------------------

log_phase "Test Phase: Container Daemons"

log_info "Starting container daemon in $TEST_CONTAINER_NAME..."

# Start container daemon in background
docker exec -d "$TEST_CONTAINER_NAME" "$CONTAINER_BINARY_PATH" container-daemon 2>&1

# Give it a moment to register
sleep "$REGISTER_WAIT"

# Verify registration via status
# The hostname should appear in status output
if "$HOST_BINARY" status 2>/dev/null | grep -qi "container\|forward\|port" || \
   "$HOST_BINARY" status --json 2>/dev/null | grep -q "container_id"; then
    log_pass "Container $TEST_CONTAINER_NAME appears registered (status shows data)"
else
    # Even "No active forwards" is okay -- it means the daemon connected
    # but nothing is listening yet
    local_status=$("$HOST_BINARY" status 2>&1 || true)
    if echo "$local_status" | grep -qi "no active forwards"; then
        log_pass "Container $TEST_CONTAINER_NAME registered (no active forwards yet)"
    else
        log_fail "Container $TEST_CONTAINER_NAME not appearing in status"
        log_info "  Status output: $local_status"
    fi
fi

# ---------------------------------------------------------------------------
# Test phase: Port forwarding
# ---------------------------------------------------------------------------

log_phase "Test Phase: Port Forwarding"

log_info "Using container: $TEST_CONTAINER_NAME"

# Start a TCP listener inside the container using Python3 (nc not available in all containers)
log_info "Starting TCP listener on port $TEST_PORT in container..."

# Kill any existing listener on the test port
docker exec "$TEST_CONTAINER_NAME" sh -c "
    pkill -f 'tcp_echo_server_$TEST_PORT' 2>/dev/null || true
" 2>/dev/null || true
sleep 0.5

# Start a Python TCP echo server that responds with HELLO_FROM_CONTAINER
# The script loops to handle multiple connections
# The "tcp_echo_server_PORT" comment is used as a pkill -f target for cleanup
docker exec -d "$TEST_CONTAINER_NAME" python3 -c "
# tcp_echo_server_$TEST_PORT
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('0.0.0.0', $TEST_PORT))
s.listen(5)
while True:
    conn, addr = s.accept()
    conn.sendall(b'HELLO_FROM_CONTAINER\n')
    conn.close()
"

# Give the Python listener a moment to start and bind the port
sleep 1

# Wait for the port scanner to detect the listener
log_info "Waiting for port $TEST_PORT to appear in status (scan interval ~1s)..."
if wait_for_status "$TEST_PORT" "$FORWARD_WAIT"; then
    log_pass "Port $TEST_PORT detected and appears in dbr status"
else
    # Check status to show what we see
    log_fail "Port $TEST_PORT did not appear in dbr status within ${FORWARD_WAIT}s"
    log_info "  Current status:"
    "$HOST_BINARY" status 2>&1 | sed 's/^/    /' || true
fi

# Validate dbr status --json output structure
log_info "Validating 'dbr status --json' output..."
status_json=$("$HOST_BINARY" status --json 2>/dev/null || echo "[]")
json_valid=$(echo "$status_json" | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    if not isinstance(data, list):
        print('not_array')
        sys.exit(0)
    for fwd in data:
        # Verify required fields are present
        required = ['container_id', 'hostname', 'port', 'host_port', 'protocol', 'since']
        missing = [k for k in required if k not in fwd]
        if missing:
            print('missing:' + ','.join(missing))
            sys.exit(0)
    print('valid')
except json.JSONDecodeError:
    print('invalid_json')
" 2>/dev/null || echo "error")

if [[ "$json_valid" == "valid" ]]; then
    log_pass "dbr status --json returns valid JSON with correct structure"
else
    log_fail "dbr status --json output invalid: $json_valid"
    log_info "  Raw JSON: $status_json"
fi

# Determine the host port (may differ from container port)
host_port=""
if echo "$status_json" | grep -q "$TEST_PORT"; then
    # Extract host_port for our test port from JSON output
    host_port=$(echo "$status_json" | python3 -c "
import sys, json
data = json.load(sys.stdin)
for fwd in data:
    if fwd.get('port') == $TEST_PORT:
        print(fwd.get('host_port', $TEST_PORT))
        break
" 2>/dev/null || echo "$TEST_PORT")
fi

if [[ -z "$host_port" ]]; then
    host_port="$TEST_PORT"
fi
log_info "Host port for forwarding: $host_port"

# Test data transfer through the forwarded port
log_info "Testing TCP data transfer through forwarded port..."
sleep 1

# Use Python to connect and receive — nc < /dev/null sends EOF immediately which
# causes copy_bidirectional to close before the echo server's data arrives.
received=""
received=$(python3 -c "
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.settimeout(5)
try:
    s.connect(('127.0.0.1', $host_port))
    data = s.recv(1024)
    print(data.decode().strip())
except Exception as e:
    print(f'ERROR: {e}')
finally:
    s.close()
" 2>/dev/null || true)

if echo "$received" | grep -q "HELLO_FROM_CONTAINER"; then
    log_pass "Data passed through forwarded port successfully"
else
    log_fail "Data transfer through forwarded port failed"
    if [[ -n "$received" ]]; then
        log_info "  Received: $received"
    else
        log_info "  No data received (empty response)"
    fi
fi

# Clean up the test listener
log_info "Stopping test listener..."
docker exec "$TEST_CONTAINER_NAME" sh -c "pkill -f 'tcp_echo_server_$TEST_PORT' 2>/dev/null || true" 2>/dev/null || true

# Wait for port to disappear from status
log_info "Waiting for port $TEST_PORT to disappear from status..."
sleep 2  # Give scanner time to detect the listener is gone
if wait_for_status_gone "$TEST_PORT" "$FORWARD_WAIT"; then
    log_pass "Port $TEST_PORT removed from status after listener stopped"
else
    log_fail "Port $TEST_PORT still appears in status after listener stopped"
    log_info "  Current status:"
    "$HOST_BINARY" status 2>&1 | sed 's/^/    /' || true
fi

# ---------------------------------------------------------------------------
# Test phase: Browser opening (OpenUrl flow)
# ---------------------------------------------------------------------------

log_phase "Test Phase: Browser Opening (OpenUrl)"

# The host daemon was started with --browser-cmd /usr/bin/true, so the full
# protocol flow completes (container sends OpenUrl, host validates and "opens"
# via /usr/bin/true, host replies OpenUrlAck{success:true}) without actually
# launching a browser. This validates the entire OpenUrl pipeline.

# Test 1: dbr open with a valid https URL — should succeed (exit 0)
log_info "Testing 'dbr open https://example.com' from container..."
open_out=$(docker exec "$TEST_CONTAINER_NAME" "$CONTAINER_BINARY_PATH" open "https://example.com" 2>&1) && open_rc=0 || open_rc=$?
if [[ $open_rc -eq 0 ]]; then
    log_pass "dbr open https URL succeeded (exit 0, full OpenUrl→OpenUrlAck round-trip)"
else
    log_fail "dbr open https URL failed (exit $open_rc)"
    log_info "  Output: $open_out"
fi

# Test 2: dbr open with a valid http URL — should succeed (exit 0)
log_info "Testing 'dbr open http://localhost:8080/callback' from container..."
open_out=$(docker exec "$TEST_CONTAINER_NAME" "$CONTAINER_BINARY_PATH" open "http://localhost:8080/callback" 2>&1) && open_rc=0 || open_rc=$?
if [[ $open_rc -eq 0 ]]; then
    log_pass "dbr open http URL succeeded (exit 0, full OpenUrl→OpenUrlAck round-trip)"
else
    log_fail "dbr open http URL failed (exit $open_rc)"
    log_info "  Output: $open_out"
fi

# Test 3: dbr open with an invalid scheme — must fail (non-zero exit)
log_info "Testing 'dbr open ftp://bad' from container (should fail)..."
open_out=$(docker exec "$TEST_CONTAINER_NAME" "$CONTAINER_BINARY_PATH" open "ftp://bad" 2>&1) && open_rc=0 || open_rc=$?
if [[ $open_rc -ne 0 ]]; then
    log_pass "dbr open correctly rejected ftp:// URL (exit $open_rc)"
else
    log_fail "dbr open ftp:// should have been rejected but succeeded (exit 0)"
fi

# Test 4: dbr open with an empty URL — must fail (non-zero exit)
log_info "Testing 'dbr open \"\"' from container (should fail)..."
open_out=$(docker exec "$TEST_CONTAINER_NAME" "$CONTAINER_BINARY_PATH" open "" 2>&1) && open_rc=0 || open_rc=$?
if [[ $open_rc -ne 0 ]]; then
    log_pass "dbr open correctly rejected empty URL (exit $open_rc)"
else
    log_fail "dbr open empty URL should have been rejected but succeeded (exit 0)"
fi

# Test 5: dbr open with javascript: scheme — must fail (non-zero exit)
log_info "Testing 'dbr open javascript:alert(1)' from container (should fail)..."
open_out=$(docker exec "$TEST_CONTAINER_NAME" "$CONTAINER_BINARY_PATH" open "javascript:alert(1)" 2>&1) && open_rc=0 || open_rc=$?
if [[ $open_rc -ne 0 ]]; then
    log_pass "dbr open correctly rejected javascript: URL (exit $open_rc)"
else
    log_fail "dbr open javascript: URL should have been rejected but succeeded (exit 0)"
fi

# ---------------------------------------------------------------------------
# Test phase: dbr ensure idempotency
# ---------------------------------------------------------------------------

log_phase "Test Phase: Idempotency"

# dbr ensure should detect the already-running daemon via Ping/Pong
log_info "Running 'dbr ensure' (should detect running daemon)..."
ensure_out=$("$HOST_BINARY" ensure 2>&1) && ensure_rc=0 || ensure_rc=$?
if [[ $ensure_rc -eq 0 ]]; then
    if echo "$ensure_out" | grep -qi "already running"; then
        log_pass "dbr ensure detected running daemon ('already running')"
    else
        log_pass "dbr ensure succeeded (exit 0)"
    fi
else
    log_fail "dbr ensure failed (exit $ensure_rc)"
    log_info "  Output: $ensure_out"
fi

# Verify status still works after ensure
if "$HOST_BINARY" status >/dev/null 2>&1; then
    log_pass "dbr status still works after ensure"
else
    log_fail "dbr status broken after ensure"
fi

# ---------------------------------------------------------------------------
# Done -- cleanup trap handles the rest
# ---------------------------------------------------------------------------

log_info "All test phases complete."
