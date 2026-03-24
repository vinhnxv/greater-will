#!/bin/bash
# Mock Claude script for testing gw mock mode.
#
# Simulates a Claude Code session running an arc phase.
# Usage: mock-claude.sh <plan_file>
#
# Environment variables:
#   GW_GROUP_NAME  - Phase group being executed (A-G)
#   GW_GROUP_LABEL - Human-readable group label
#   GW_PLAN        - Plan file path

set -e

PLAN="${1:-unknown}"
GROUP="${GW_GROUP_NAME:-unknown}"
LABEL="${GW_GROUP_LABEL:-unknown}"

echo "[MOCK-CLAUDE] Starting mock session"
echo "[MOCK-CLAUDE] Plan: $PLAN"
echo "[MOCK-CLAUDE] Group: $GROUP ($LABEL)"
echo "[MOCK-CLAUDE] Time: $(date -Iseconds)"
echo ""

# Simulate phase execution
echo "[MOCK-CLAUDE] Running phase: ${GROUP}_phase_1"
sleep 1
echo "[MOCK-CLAUDE] Phase ${GROUP}_phase_1 complete"

echo ""
echo "[MOCK-CLAUDE] Session complete"
echo "[MOCK-CLAUDE] Exit code: 0"

exit 0
