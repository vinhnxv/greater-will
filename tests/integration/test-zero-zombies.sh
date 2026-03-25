#!/bin/bash
# Integration test: Zero zombie processes after execution.
#
# Runs a mock pipeline, then checks that no orphaned tmux sessions
# or background processes remain from the gw run.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

echo "=== Test: zero-zombies ==="

# Setup
cd "$REPO_ROOT"
rm -rf .gw/

# Build
cargo build --release 2>/dev/null || cargo build

BIN="./target/release/gw"
[ -f "$BIN" ] || BIN="./target/debug/gw"

chmod +x tests/mock/mock-success.sh

# Record tmux sessions before
BEFORE=$(tmux list-sessions 2>/dev/null | grep -c "gw-" || echo "0")

# Execute mock pipeline
$BIN run --mock tests/mock/mock-success.sh tests/fixtures/sample-plan.md >/dev/null 2>&1 || true

# Small delay for session cleanup
sleep 2

# Check for orphaned gw tmux sessions
AFTER=$(tmux list-sessions 2>/dev/null | grep -c "gw-" || echo "0")

if [ "$AFTER" -gt "$BEFORE" ]; then
    echo "FAIL: Found $((AFTER - BEFORE)) orphaned gw-* tmux sessions"
    tmux list-sessions 2>/dev/null | grep "gw-"
    exit 1
fi

# Check for orphaned gw processes
GW_PROCS=$(pgrep -f "target/(release|debug)/gw" 2>/dev/null | wc -l | tr -d ' ')
if [ "$GW_PROCS" -gt 0 ]; then
    echo "FAIL: Found $GW_PROCS orphaned gw processes"
    pgrep -af "target/(release|debug)/gw" 2>/dev/null || true
    exit 1
fi

echo "PASS: zero-zombies (no orphaned sessions or processes)"
