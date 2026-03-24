#!/bin/bash
# Mock script that simulates a stuck session.
# Used for testing timeout handling.
#
# Runs for 5 minutes unless killed.

echo "[MOCK-STUCK] Starting stuck simulation..."
echo "[MOCK-STUCK] Will run for 5 minutes or until killed"

# Sleep in small increments to allow signal handling
for i in $(seq 1 300); do
    echo "[MOCK-STUCK] Still running... ${i}s"
    sleep 1
done

echo "[MOCK-STUCK] Completed (should not reach here in tests)"
exit 0
