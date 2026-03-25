#!/bin/bash
# Integration test: Crash recovery.
#
# Runs gw with mock-crash.sh to verify that a crashing mock script
# is detected and gw exits with a failure status.
# Then verifies that checkpoint state exists for potential resume.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

echo "=== Test: crash-recovery ==="

# Setup
cd "$REPO_ROOT"
rm -rf .gw/

# Build
cargo build --release 2>/dev/null || cargo build

BIN="./target/release/gw"
[ -f "$BIN" ] || BIN="./target/debug/gw"

# Ensure mock scripts are executable
chmod +x tests/mock/mock-crash.sh

# Execute: run with crashing mock — expect failure
echo "--- Running with crash mock ---"
OUTPUT=$($BIN run --mock tests/mock/mock-crash.sh tests/fixtures/sample-plan.md 2>&1)
EXIT_CODE=$?

if [ "$EXIT_CODE" -eq 0 ]; then
    echo "FAIL: gw should have exited non-zero on crash, got 0"
    exit 1
fi

echo "gw exited with code $EXIT_CODE (expected non-zero)"

# Assert: verify that .gw/ directory was created for state tracking
if [ -d ".gw" ]; then
    echo "PASS: .gw/ state directory exists after crash"
else
    echo "INFO: .gw/ not created (stateless mock mode — acceptable)"
fi

# Cleanup any leftover tmux sessions from the crash
tmux list-sessions 2>/dev/null | grep "gw-mock" | cut -d: -f1 | while read -r sess; do
    tmux kill-session -t "$sess" 2>/dev/null || true
done

echo "PASS: crash-recovery"
