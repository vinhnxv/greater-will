#!/bin/bash
# Regression suite: Runs all integration tests with pass/fail summary.
#
# Usage: ./tests/integration/regression-suite.sh
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "========================================="
echo "  Greater-Will Integration Test Suite"
echo "========================================="
echo ""

PASS=0
FAIL=0
SKIP=0
RESULTS=()

run_test() {
    local name="$1"
    local script="$2"

    if [ ! -f "$script" ]; then
        echo "SKIP: $name (script not found: $script)"
        SKIP=$((SKIP + 1))
        RESULTS+=("SKIP  $name")
        return
    fi

    echo "--- Running: $name ---"
    if bash "$script"; then
        PASS=$((PASS + 1))
        RESULTS+=("PASS  $name")
    else
        FAIL=$((FAIL + 1))
        RESULTS+=("FAIL  $name")
    fi
    echo ""
}

# Run all tests
run_test "mock-pipeline"    "$SCRIPT_DIR/test-mock-pipeline.sh"
run_test "crash-recovery"   "$SCRIPT_DIR/test-crash-recovery.sh"
run_test "zero-zombies"     "$SCRIPT_DIR/test-zero-zombies.sh"
run_test "instance-lock"    "$SCRIPT_DIR/test-instance-lock.sh"

# Summary
echo "========================================="
echo "  Results: $PASS passed, $FAIL failed, $SKIP skipped"
echo "========================================="
for r in "${RESULTS[@]}"; do
    echo "  $r"
done
echo ""

if [ "$FAIL" -gt 0 ]; then
    echo "SUITE FAILED"
    exit 1
else
    echo "SUITE PASSED"
    exit 0
fi
