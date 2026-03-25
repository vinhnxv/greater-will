#!/bin/bash
# gw-watchdog.sh — Auto-restart gw on crash with resume capability.
#
# Features:
#   - caffeinate -i to prevent macOS sleep during execution
#   - Max 5 restarts with 30s cooldown between restarts
#   - First run: normal execution; subsequent runs: replay from checkpoint
#   - Logs to .gw/logs/watchdog.log
#
# Usage: ./gw-watchdog.sh <plan_file> [extra gw args...]
set -uo pipefail

# --- Configuration ---
MAX_RESTARTS=5
COOLDOWN_SECS=30
LOG_DIR=".gw/logs"
LOG_FILE="$LOG_DIR/watchdog.log"

# --- Argument handling ---
if [ $# -lt 1 ]; then
    echo "Usage: $0 <plan_file> [extra gw args...]"
    echo "Example: $0 plans/my-plan.md --group A"
    exit 1
fi

PLAN="$1"
shift
EXTRA_ARGS=("$@")

# --- Setup ---
mkdir -p "$LOG_DIR"

GW_BIN="./target/release/gw"
[ -f "$GW_BIN" ] || GW_BIN="./target/debug/gw"

if [ ! -f "$GW_BIN" ]; then
    echo "ERROR: gw binary not found. Run 'cargo build --release' first."
    exit 1
fi

log() {
    local msg="[$(date -Iseconds)] $1"
    echo "$msg" | tee -a "$LOG_FILE"
}

# --- Caffeinate wrapper ---
# Prevent macOS from sleeping during long arc runs
CAFFEINATE_PID=""
if command -v caffeinate &>/dev/null; then
    caffeinate -i -w $$ &
    CAFFEINATE_PID=$!
    log "caffeinate started (PID=$CAFFEINATE_PID)"
fi

cleanup() {
    if [ -n "$CAFFEINATE_PID" ]; then
        kill "$CAFFEINATE_PID" 2>/dev/null || true
    fi
    log "watchdog exiting"
}
trap cleanup EXIT

# --- Main loop ---
RESTART_COUNT=0
FIRST_RUN=true

log "=== gw-watchdog starting ==="
log "Plan: $PLAN"
log "Max restarts: $MAX_RESTARTS"
log "Cooldown: ${COOLDOWN_SECS}s"

while [ "$RESTART_COUNT" -le "$MAX_RESTARTS" ]; do
    if [ "$FIRST_RUN" = true ]; then
        log "Starting gw (initial run)"
        "$GW_BIN" run "$PLAN" "${EXTRA_ARGS[@]}" 2>&1 | tee -a "$LOG_FILE"
        EXIT_CODE=${PIPESTATUS[0]}
        FIRST_RUN=false
    else
        # Find latest checkpoint for resume
        CHECKPOINT=$(find .gw/checkpoints -name "*.json" -type f 2>/dev/null | sort -r | head -1)

        if [ -n "$CHECKPOINT" ]; then
            log "Resuming from checkpoint: $CHECKPOINT"
            "$GW_BIN" replay "$CHECKPOINT" 2>&1 | tee -a "$LOG_FILE"
            EXIT_CODE=${PIPESTATUS[0]}
        else
            log "No checkpoint found, re-running from scratch"
            "$GW_BIN" run "$PLAN" "${EXTRA_ARGS[@]}" 2>&1 | tee -a "$LOG_FILE"
            EXIT_CODE=${PIPESTATUS[0]}
        fi
    fi

    # Check exit status
    if [ "$EXIT_CODE" -eq 0 ]; then
        log "gw completed successfully (exit=0)"
        exit 0
    fi

    RESTART_COUNT=$((RESTART_COUNT + 1))
    log "gw crashed (exit=$EXIT_CODE), restart $RESTART_COUNT/$MAX_RESTARTS"

    if [ "$RESTART_COUNT" -gt "$MAX_RESTARTS" ]; then
        log "ERROR: Max restarts ($MAX_RESTARTS) exceeded. Giving up."
        exit 1
    fi

    log "Cooling down for ${COOLDOWN_SECS}s..."
    sleep "$COOLDOWN_SECS"
done

log "ERROR: Unexpected exit from watchdog loop"
exit 1
