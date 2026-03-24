#!/bin/bash
# Mock script that simulates a crash.
# Used for testing error handling.

echo "[MOCK-CRASH] Starting crash simulation..."
echo "[MOCK-CRASH] About to crash in 2 seconds"
sleep 2
echo "[MOCK-CRASH] CRASHING NOW!"

# Exit with non-zero code
exit 1
