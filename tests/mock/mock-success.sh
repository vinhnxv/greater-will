#!/bin/bash
# Mock script that always succeeds after a brief pause.
# Usage: mock-success.sh <plan_file>
#
# Environment variables:
#   GW_GROUP_NAME  - Phase group being executed (A-G)
#   GW_GROUP_LABEL - Human-readable group label
#   GW_PLAN        - Plan file path

PLAN="${1:-unknown}"
GROUP="${GW_GROUP_NAME:-unknown}"
LABEL="${GW_GROUP_LABEL:-unknown}"

echo "[MOCK-SUCCESS] Plan: $PLAN"
echo "[MOCK-SUCCESS] Group: $GROUP ($LABEL)"
sleep 1
echo "[MOCK-SUCCESS] Done."
exit 0
