---
type: fix
name: daemon-detection-parity
scope: tactical
effort: L
priority: critical
depends_on: []
---

# Plan: Daemon Monitor Detection Parity — Reduce False Positive Kills

## Summary

The daemon's `DaemonRunMonitor` (run_monitor.rs) triggers false-positive crash
detections that kill healthy tmux sessions, forcing unnecessary recovery cycles.
This plan ports 6 detection features from the foreground monitor (monitor.rs) to
eliminate the root cause of excessive daemon "crashes".

**Key insight**: The daemon doesn't crash more — it **detects incorrectly** more,
then kills sessions that were actually healthy, then has to recover them. Fixing
detection reduces recovery needs.

## Motivation

Observed symptoms:
- Daemon runs require 2-3x more recovery cycles than equivalent foreground runs
- `gw logs` shows patterns: "idle for Ns" → kill → recovery → same again
- Overnight batch runs accumulate unnecessary restart_cooldown delays
- Queue throughput degrades as crash loop detector fills up with false positives

Root cause chain:
```
capture_pane fails (tmux contention)
  → last_activity not updated
  → idle threshold exceeded
  → pending_kill set (Stuck)
  → no recovery signal (capture still failing)
  → kill gate fires after 5 min silence
  → session killed → Stuck outcome
  → recovery attempt → same pattern repeats
```

## Gap Analysis

### DET-1: Session-vanish disambiguation missing checkpoint check — HIGH

**Foreground** (`monitor.rs:349-420`) uses 4-step disambiguation when tmux vanishes:
1. Age < 30s → crash (MIN_SESSION_DURATION_SECS)
2. Process dead + `checkpoint.is_complete()` → Completed
3. Process dead + checkpoint has in-progress phase → Crashed with phase context
4. Process dead + no checkpoint + age > 5 min → Crashed (no evidence)

**Daemon** (`run_monitor.rs:603-625`) uses only 2-step:
1. Age < 300s → crash (MIN_COMPLETION_AGE_SECS — 10x longer than foreground!)
2. `completion_detected_at == None` → crash
3. Else → Completed

**Missing**: No checkpoint-based verification. If Claude finishes and tmux exits
before `is_pipeline_complete()` text appears in the pane (race: tmux closes → pane
gone → `check_pane_completion` never fires → `completion_detected_at` stays `None`),
daemon declares crash on a successful run.

**Also**: MIN_COMPLETION_AGE_SECS is 300s (5 min) in daemon vs 30s in foreground.
Young sessions (30s-300s) that legitimately crash are misclassified as "too young"
for 4.5 minutes longer than foreground, delaying recovery.

### DET-2: Missing transition gap escalation — HIGH

**Foreground** (`monitor.rs:605-618, 1327-1376`) has 3-level transition gap handling:
- **Nudge** at 5 min (`TRANSITION_NUDGE_SECS = 300`) — "please continue working"
- **Warn** at 8 min (`TRANSITION_WARN_SECS = 480`) — "are you stuck between phases?"
- **Kill** at 11 min (`TRANSITION_KILL_SECS = 660`) — route to kill gate

Detection via `PhaseNav::is_transitioning()` — checks checkpoint for no `in_progress`
phase with completed phases existing. This means foreground **knows** the session is
between phases and tolerates it.

**Daemon** (`run_monitor.rs:372-373`):
```rust
// 14-15. Transition gap / failed phase escalation
// TODO(shard-4): port from monitor.rs:640-931
```

**Not ported.** Daemon has no concept of "transitioning between phases". When Claude
spends 5+ minutes between phases (normal for phase teardown → phase setup), daemon's
`check_idle()` triggers instead:
- `idle_nudge_secs` (phase-dependent, ~180-600s) → nudge
- `idle_kill_secs` (phase-dependent, ~600-1800s) → pending_kill → Stuck

This is **the primary source of false kills** — transition gaps are natural pauses
that foreground tolerates but daemon kills.

### DET-3: Missing failed phase escalation — HIGH

**Foreground** (`monitor.rs:585-604, 1379-1430`) detects failed phases via checkpoint:
- `PhaseNav::has_failure()` → start timer
- **Nudge** at 6 min (`FAILED_NUDGE_SECS = 360`) — "please continue"
- **Warn** at 9 min (`FAILED_WARN_SECS = 540`) — "check and continue or retry"
- **Kill** at 12 min (`FAILED_KILL_SECS = 720`) — route to kill gate

When Rune's internal retry mechanism kicks in, `has_failure()` clears → timers reset.
This gives Rune **12 minutes** to self-heal a failed phase before gw intervenes.

**Daemon**: No failed phase detection. A failed phase that Rune is actively retrying
shows as "idle screen" to the daemon → early kill. Rune gets its session killed
mid-self-heal.

### DET-4: Missing loop stall correlation — MEDIUM

**Foreground** (`monitor.rs:1251-1271`) has `LOOP_STALL_WARN_SECS = 600`:
- Loop state stale + checkpoint stale → "arc may be stuck" (real problem)
- Loop state stale + checkpoint updating → "phase in progress" (just long phase)

This correlation prevents false alarms when a single phase runs longer than expected
(loop state doesn't change mid-phase, but checkpoint does).

**Daemon**: No loop stall logic. Loop state staleness feeds into generic idle
detection without checkpoint correlation.

### DET-5: capture_pane failure causes idle accumulation — MEDIUM

**Daemon** (`run_monitor.rs:330-333`):
```rust
let pane_content = match self.capture_pane().await {
    Some(c) => c,
    None => continue, // skips ALL remaining checks
};
```

When `capture_pane` returns `None` (tmux busy, session briefly unavailable):
- `update_activity_from_pane` is skipped → `last_activity` stays stale
- `check_idle` still runs next iteration with accumulated staleness
- Multiple consecutive failures compound: 3 failures × 5s poll = 15s of phantom idle

**Multi-run contention**: With N daemon runs, N monitors call `capture_pane` via
`spawn_blocking` every 5s. Under load, tmux server response time increases,
increasing failure rate, compounding idle accumulation.

**Foreground**: Same logic but single-threaded → no contention → failures rare.

### DET-6: MIN_COMPLETION_AGE_SECS too high (300s vs 30s) — LOW

**Foreground**: `MIN_SESSION_DURATION_SECS = 30` — sessions under 30s are crash.

**Daemon**: `MIN_COMPLETION_AGE_SECS = 300` — sessions under 5 min are crash.

A session that completes in 2 minutes (e.g., simple single-phase task) would be
incorrectly classified as "crashed" by daemon but correctly as "completed" by
foreground (if checkpoint confirms).

This is partially mitigated by `completion_detected_at` (pane text detection), but
combined with DET-1 (missing checkpoint check), makes the false positive rate higher.

## Tasks

### Task 1: Port transition gap escalation (DET-2) — L

**Files**: `src/daemon/run_monitor.rs`

This is the highest-impact fix. Requires:

1. **Add state fields** to `DaemonRunMonitor`:
   ```rust
   // Transition/failed gap tracking
   in_transition_since: Option<Instant>,
   transition_nudge_count: u32,
   in_failed_since: Option<Instant>,
   failed_nudge_count: u32,
   ```
   Note: These fields already exist as stubs (lines 178-184) with `#[allow(dead_code)]`.
   Remove the `#[allow(dead_code)]` and activate them.

2. **Port `PhaseNav` integration** into `poll_checkpoint()`:
   Currently daemon's `poll_checkpoint` updates `current_phase` and `phase_started_at`.
   Add `PhaseNav` analysis:
   ```rust
   use crate::monitor::phase_nav;

   // After checkpoint read:
   let nav = phase_nav::PhaseNav::from_checkpoint(&checkpoint);

   if nav.has_failure() {
       if self.in_failed_since.is_none() {
           self.in_failed_since = Some(Instant::now());
           self.failed_nudge_count = 0;
       }
       self.in_transition_since = None;
       self.transition_nudge_count = 0;
   } else if nav.is_transitioning() {
       if self.in_transition_since.is_none() {
           self.in_transition_since = Some(Instant::now());
           self.transition_nudge_count = 0;
       }
       self.in_failed_since = None;
       self.failed_nudge_count = 0;
   } else {
       // Phase running — clear all gap timers
       self.in_transition_since = None;
       self.transition_nudge_count = 0;
       self.in_failed_since = None;
       self.failed_nudge_count = 0;
   }
   ```

3. **Add `check_transition_gap()` method** (new step 14):
   ```rust
   async fn check_transition_gap(&mut self) {
       let Some(start) = self.in_transition_since else { return };
       let gap_secs = start.elapsed().as_secs();

       if gap_secs > phase_nav::TRANSITION_KILL_SECS && self.pending_kill.is_none() {
           self.pending_kill = Some(PendingKillRequest {
               reason: format!("Transition gap {}s > {}s", gap_secs, phase_nav::TRANSITION_KILL_SECS),
               outcome: KillOutcomeKind::Stuck,
               started_at: Instant::now(),
               nudge_sent: false,
           });
       } else if gap_secs > phase_nav::TRANSITION_WARN_SECS && self.transition_nudge_count < 2 {
           self.transition_nudge_count = 2;
           self.send_nudge("are you stuck between phases? please continue to the next phase").await;
       } else if gap_secs > phase_nav::TRANSITION_NUDGE_SECS && self.transition_nudge_count < 1 {
           self.transition_nudge_count = 1;
           self.send_nudge("please continue working").await;
       }
   }
   ```

4. **Add `check_failed_phase()` method** (new step 15):
   Same pattern with `FAILED_NUDGE_SECS` / `FAILED_WARN_SECS` / `FAILED_KILL_SECS`.

5. **Guard idle detection** — when in transition or failed state, skip `check_idle()`:
   ```rust
   // 16. Idle detection + nudge — skip if transition/failed gap is active
   if self.in_transition_since.is_none() && self.in_failed_since.is_none() {
       self.check_idle().await;
   }
   ```
   This is the **critical interaction**: transition-aware escalation replaces idle
   detection during inter-phase gaps, preventing false Stuck kills.

**Test**: Verify that a 7-minute transition gap triggers nudge (not kill), and that
idle detection is suppressed during transitions.

### Task 2: Add checkpoint-based session vanish disambiguation (DET-1) — M

**File**: `src/daemon/run_monitor.rs`

**Changes** to `check_session_alive()`:

```rust
async fn check_session_alive(&mut self) -> Option<DaemonRunOutcome> {
    if !self.has_session().await {
        let age = self.run_started_at.elapsed().as_secs();

        // Step 1: Too young (use foreground's 30s, not 300s)
        if age < 30 {
            return Some(DaemonRunOutcome::Crashed {
                reason: format!("tmux session vanished at age {}s (below min)", age),
            });
        }

        // Step 2: Check checkpoint for definitive answer
        if let Some(checkpoint) = self.read_checkpoint_sync().await {
            if checkpoint.is_complete() {
                return Some(DaemonRunOutcome::Completed);
            }
            let phase = checkpoint.inferred_phase_name()
                .unwrap_or_else(|| checkpoint.current_phase().unwrap_or("unknown"));
            return Some(DaemonRunOutcome::Crashed {
                reason: format!("session ended during phase '{}' after {}s", phase, age),
            });
        }

        // Step 3: No checkpoint — fall back to completion_detected_at
        if self.completion_detected_at.is_some() {
            return Some(DaemonRunOutcome::Completed);
        }

        // Step 4: No evidence of completion
        return Some(DaemonRunOutcome::Crashed {
            reason: "tmux session vanished without completion evidence".to_string(),
        });
    }
    None
}
```

**Helper**: Add `read_checkpoint_sync()` — wraps `read_cached_checkpoint` in
`spawn_blocking` (same pattern as `has_session`):
```rust
async fn read_checkpoint_sync(&mut self) -> Option<Checkpoint> {
    let dir = self.repo_dir.clone();
    let mut path = self.cached_checkpoint_path.clone();
    tokio::task::spawn_blocking(move || {
        read_cached_checkpoint(&dir, &mut path)
    })
    .await
    .ok()
    .flatten()
}
```

**Test**: Verify session vanish with complete checkpoint → Completed (not Crashed).

### Task 3: Handle capture_pane failure gracefully (DET-5) — S

**File**: `src/daemon/run_monitor.rs`

**Changes**: When `capture_pane` fails, don't skip all checks. At minimum, still
run session-alive, kill gate, and status logging:

```rust
// 4. Capture pane + activity tracking
let pane_content = self.capture_pane().await;

if let Some(ref content) = pane_content {
    self.update_activity_from_pane(content);

    // 5. Auto-accept permission prompts
    if self.check_prompt_accept(content) {
        continue;
    }

    // 10. Pane text completion
    self.check_pane_completion(content);

    // 11. Error evidence detection
    self.evaluate_error_evidence(content);
}
// Steps that DON'T need pane content continue regardless:

// 6. Completion grace period
if let Some(outcome) = self.check_completion_grace() {
    return outcome;
}

// 7. Checkpoint polling + phase tracking
self.poll_checkpoint();

// 8. Loop state tracking
if let Some(outcome) = self.poll_loop_state() {
    return outcome;
}

// 9. Artifact dir scan
self.scan_artifacts();

// 12. Process liveness
if let Some(outcome) = self.check_process_liveness() {
    return outcome;
}

// 13. Per-phase timeout
self.check_phase_timeout();

// 14-15. Transition/failed gap (Task 1)
self.check_transition_gap().await;
self.check_failed_phase().await;

// 16. Idle detection + nudge
if self.in_transition_since.is_none() && self.in_failed_since.is_none() {
    self.check_idle().await;
}

// 17. Unified kill gate
if let Some(outcome) = self.evaluate_kill_gate().await {
    return outcome;
}

// 18. Periodic status logging
self.log_status();
```

**Key change**: Checks 6-9, 12-18 now run even when pane capture fails. This means:
- Checkpoint/artifact/loop state still provide activity signals
- Kill gate recovery checks still fire (can cancel pending kills)
- Process liveness still detected

**Test**: Simulate 3 consecutive `capture_pane` failures, verify idle timer
doesn't accumulate and checkpoint activity still cancels pending kills.

### Task 4: Add loop stall correlation (DET-4) — S

**File**: `src/daemon/run_monitor.rs`

**Add method** `check_loop_stall()` in the status logging section:

```rust
fn check_loop_stall(&self) {
    const LOOP_STALL_WARN_SECS: u64 = 600;

    if !self.loop_state_ever_seen {
        return;
    }

    let loop_stall = self.last_loop_state_change.elapsed().as_secs();
    if loop_stall <= LOOP_STALL_WARN_SECS {
        return;
    }

    let cp_stale = self.last_checkpoint_activity.elapsed().as_secs();
    if cp_stale > LOOP_STALL_WARN_SECS {
        warn!(
            run_id = %self.run_id,
            loop_stall_secs = loop_stall,
            checkpoint_stale_secs = cp_stale,
            "Loop state AND checkpoint both stale — arc may be stuck"
        );
    } else {
        debug!(
            run_id = %self.run_id,
            loop_stall_secs = loop_stall,
            "Loop state stale but checkpoint still updating — phase in progress"
        );
    }
}
```

Call from `log_status()` (runs every 30s).

**Test**: Verify stale loop + updating checkpoint logs "phase in progress" (not stuck).

### Task 5: Reduce MIN_COMPLETION_AGE_SECS (DET-6) — S

**File**: `src/daemon/run_monitor.rs`

Change:
```rust
const MIN_COMPLETION_AGE_SECS: u64 = 300; // 5 min
```
To:
```rust
const MIN_COMPLETION_AGE_SECS: u64 = 60; // 1 min (aligned closer to foreground's 30s)
```

**Rationale**: Not reducing to 30s because daemon has no `MIN_SESSION_DURATION_SECS`
guard (foreground uses 30s for "too quick" detection and separate 300s for
"vanish = completion?" heuristic). With checkpoint-based disambiguation (Task 2),
the age threshold matters less — checkpoint is the definitive signal. Use 60s as
conservative compromise.

### Task 6: Tests — M

**File**: `src/daemon/run_monitor.rs` (tests module)

Tests needed:

1. **`test_session_vanish_with_complete_checkpoint`** (Task 2):
   Session gone + checkpoint complete → Completed (not Crashed)

2. **`test_session_vanish_without_checkpoint`** (Task 2):
   Session gone + no checkpoint + no completion signal → Crashed

3. **`test_transition_gap_suppresses_idle`** (Task 1):
   In transition state → `check_idle` skipped → no false Stuck

4. **`test_transition_nudge_escalation`** (Task 1):
   5 min → nudge, 8 min → warn, 11 min → kill gate

5. **`test_failed_phase_gives_rune_time`** (Task 1):
   Failed phase → 6 min nudge, 12 min kill (not idle kill at ~10 min)

6. **`test_capture_pane_failure_doesnt_accumulate_idle`** (Task 3):
   3 consecutive capture failures → checkpoint activity still resets idle

7. **`test_loop_stall_correlation`** (Task 4):
   Loop stale + checkpoint fresh → debug log (not warning)

## Implementation Order

```
Task 1 (transition/failed gap — largest, highest impact)
  ↓
Task 3 (capture_pane failure handling — restructure poll loop)
  ↓  ← Task 3 must be after Task 1 because it references new check methods
Task 2 (checkpoint-based vanish disambiguation)
  ↓
Task 4 (loop stall correlation — small, independent)
  ↓
Task 5 (reduce MIN_COMPLETION_AGE_SECS — trivial)
  ↓
Task 6 (tests — after all logic)
```

Tasks 2, 4, and 5 could run in parallel after Task 1+3.

## Relationship to Recovery Parity Plan

This plan (detection) and `fix-daemon-recovery-parity-plan` (recovery) are
complementary:

```
fix-daemon-detection-parity (this plan)
  → Fewer false positive kills
  → Fewer unnecessary recoveries
  → Less crash_restarts accumulation

fix-daemon-recovery-parity
  → When real crashes happen, recovery is smarter
  → Per-class limits prevent over-retry
  → Queue drains properly on crash loop
```

**Recommended order**: Implement detection fixes FIRST. This reduces the volume
of recovery events, making the recovery fixes less urgent and easier to validate.

## Risk Assessment

| Risk | Mitigation |
|------|-----------|
| PhaseNav depends on checkpoint format | PhaseNav already stable in foreground — same checkpoint schema |
| Suppressing idle during transition could hide real stalls | Transition kill (11 min) is still shorter than idle_kill (10-30 min phase-dependent) |
| checkpoint read in `check_session_alive` adds I/O | Only runs when session already vanished — rare path, one-time read |
| Restructured poll loop (Task 3) is large diff | Keep same step numbers for traceability; each check is an independent method |
| Multi-run contention (DET-5 root cause) not fully solved | Task 3 mitigates symptoms; fundamental fix would be tmux command serialization (future work) |

## Acceptance Criteria

- [ ] Transition gaps (5-11 min) produce nudge/warn instead of idle kill
- [ ] Failed phases get 12 min of Rune self-heal time before kill
- [ ] Session vanish with complete checkpoint → Completed (not Crashed)
- [ ] `capture_pane` failure doesn't cause idle accumulation
- [ ] Loop stall distinguishes "long phase" from "truly stuck"
- [ ] MIN_COMPLETION_AGE_SECS reduced from 300s to 60s
- [ ] `cargo test` passes with new test cases
- [ ] `cargo clippy` clean
- [ ] Overnight batch run shows measurable reduction in false recovery cycles
