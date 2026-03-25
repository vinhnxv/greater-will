#!/bin/bash
# Integration test: Full mock pipeline execution.
#
# Runs gw with mock-success.sh against a sample plan and verifies
# that all phase groups complete successfully.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

echo "=== Test: mock-pipeline ==="

# Setup
cd "$REPO_ROOT"
rm -rf .gw/

# Build (release if available, else debug)
cargo build --release 2>/dev/null || cargo build

BIN="./target/release/gw"
[ -f "$BIN" ] || BIN="./target/debug/gw"

# Ensure mock script is executable
chmod +x tests/mock/mock-success.sh

# Execute: dry run first to validate the plan parses
echo "--- Dry run ---"
$BIN run --dry-run tests/fixtures/sample-plan.md
echo "PASS: dry-run succeeded"

# Execute: mock run with success script
echo "--- Mock run ---"
OUTPUT=$($BIN run --mock tests/mock/mock-success.sh tests/fixtures/sample-plan.md 2>&1) || {
    echo "FAIL: mock pipeline exited non-zero"
    echo "$OUTPUT"
    exit 1
}

# Assert: check that mock output contains expected markers
echo "$OUTPUT" | grep -q "\[MOCK\]" || {
    echo "FAIL: output missing [MOCK] prefix"
    exit 1
}

echo "$OUTPUT" | grep -q "All groups complete" || {
    echo "FAIL: output missing completion message"
    exit 1
}

echo "PASS: mock-pipeline"
