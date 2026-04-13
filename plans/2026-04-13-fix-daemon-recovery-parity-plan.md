---
type: fix
name: daemon-recovery-parity
scope: tactical
effort: M
priority: high
depends_on: []
---

# Plan: Daemon Recovery Parity — Close Gaps with Foreground Orchestrator

## Summary

The daemon's `handle_monitor_outcome` (heartbeat.rs) is missing several recovery
safeguards that the foreground orchestrator (orchestrator.rs) implements. This plan
closes 6 gaps to bring daemon recovery to full parity with foreground, preventing
wasted retries, stuck queues, and inconsistent crash handling.

## Motivation

**Why these gaps matter:**

- **GAP 1 (per-class max_retries)**: Daemon retries `Stuck` errors 5 times when
  foreground stops at 2. Each retry burns API tokens, compute time, and delays the
  queue — for an error class where more retries don't help.
- **GAP 6 (missing drain on crash loop)**: When crash loop is detected, the queue
  for that repo gets permanently stuck — no `drain_if_available()` call to advance
  to the next plan.
- **GAP 2 (no healthy_tick)**: Crash counters never reset from stability, only from
  successful completion. A run that succeeds after 1 hour of healthy execution still
  carries crash history from hours ago.
- **GAP 5 (flat cooldown)**: `Crashed/Stuck/Timeout` outcomes all get 60s cooldown
  regardless of error characteristics.

## Gap Analysis

### GAP 1: Missing per-error-class `max_retries()` — HIGH

**Foreground** (`orchestrator.rs:486`):
```rust
if crash_detector.total_restarts() >= error_class.max_retries() {
    return /* stop */;
}
```

**Daemon** (`heartbeat.rs:606-658`): Only checks `CrashLoopDetector`. No per-class limit.

**Consequence**: Daemon allows 5 retries for ALL error classes. Foreground limits:
- `Stuck` → 2, `PermissionBlocked` → 2
- `Crash` → 3, `Timeout` → 3, `NetworkError` → 3
- `ApiOverload` → 4

**Fix location**: `heartbeat.rs`, both the `ErrorDetected` path (line ~606) and the
`Crashed/Stuck/Timeout` path (line ~661). The `ErrorDetected` path has the error_class
directly. The `Crashed/Stuck/Timeout` path needs a mapping: `Crashed` → `ErrorClass::Crash`,
`Stuck` → `ErrorClass::Stuck`, `Timeout` → `ErrorClass::Timeout`.

### GAP 2: Missing `record_healthy_tick()` — MEDIUM

**Foreground** (`orchestrator.rs:347-349`):
```rust
if attempt_duration >= Duration::from_secs(config.watchdog.crash_stability_secs) {
    crash_detector.record_healthy_tick();
}
```

**Daemon**: Never calls `record_healthy_tick()`. The detector is recreated from disk
each time `handle_monitor_outcome` runs, so in-memory stability tracking is impossible.

**Root cause**: Daemon creates a new `CrashLoopDetector` per outcome event instead of
keeping one alive per run. This is GAP 3, and fixing GAP 2 requires addressing GAP 3.

**Fix location**: `heartbeat.rs` — either persist healthy state to disk alongside crash
history, or keep a per-run detector in the `HeartbeatMonitor`'s monitor map.

### GAP 3: Detector recreated each outcome — LOW (enables GAP 2 fix)

**Foreground**: Single `CrashLoopDetector` lives for the entire orchestrator loop.

**Daemon** (`heartbeat.rs:616, 686`):
```rust
let mut detector = CrashLoopDetector::from_watchdog(&watchdog);
detector.load_history(&repo_dir);
```

New detector created from disk each time. Works for crash counting (persist/load is
correct), but prevents `record_healthy_tick()` because `healthy_since` is never
persisted.

**Fix**: Store a `CrashLoopDetector` in a `HashMap<String, CrashLoopDetector>` keyed
by `run_id` within the heartbeat monitor, OR persist `healthy_since` epoch to the
crash history JSON.

### GAP 4: Dual `crash_restarts` sources — LOW

**Foreground**: `crash_detector.total_restarts()` is the single source of truth.

**Daemon**: Two independent counters:
- `entry.crash_restarts` (registry) — incremented in `spawn_recovery_with_command` (line 914)
- `detector.total_restarts()` (disk) — from persisted crash history

`entry.crash_restarts` is used for backoff calculation (`heartbeat.rs:643`).
`detector` is used for crash loop check. These can diverge if a crash happens between
persist and spawn, or if daemon restarts mid-recovery.

**Fix**: After GAP 1 fix, use `entry.crash_restarts` consistently as the canonical
counter for per-class max_retries check (it's already available in registry). Keep
`CrashLoopDetector` for sliding-window rapid-crash detection only.

### GAP 5: Flat cooldown for Crashed/Stuck/Timeout — MEDIUM

**Foreground**: All error types route through `error_class.backoff_for_attempt(attempt)`.

**Daemon**:
- `ErrorDetected` path → `error_class.backoff_for_attempt()` ✅
- `Crashed/Stuck/Timeout` path → flat `restart_cooldown_secs` (60s) ❌

**Fix location**: `heartbeat.rs:710-721`. Map outcome to implicit error class, use
`backoff_for_attempt()` instead of flat cooldown.

### GAP 6: Missing drain on crash loop stop — HIGH

**Daemon outcome paths that call `drain_if_available()`:**
- `Completed` → ✅ `drain_if_available(registry, &dir, false)` (line 560)
- `Failed` → ✅ `drain_if_available(registry, &dir, true)` (line 573)
- `ErrorDetected` + `StopCrashLoop` → ❌ missing
- `Crashed/Stuck/Timeout` + `StopCrashLoop` → ❌ missing

**Consequence**: When a run enters crash loop, it's marked `Failed` but the queue
for that repo is never drained. Subsequent queued plans sit forever.

**Fix location**: `heartbeat.rs:620-631` and `heartbeat.rs:690-703`. Add
`drain_if_available()` after marking `Failed` on crash loop detection.

### GAP 7: Timeout retry semantic differs — DESIGN DECISION (no fix needed)

**Foreground**: `SessionOutcome::Timeout` is unconditionally terminal — returns
`PipelineResult { success: false }` immediately, no retry.

**Daemon**: `DaemonRunOutcome::Timeout` is retried (grouped with Crashed/Stuck).

**Verdict**: Intentional asymmetry. Daemon retries timeouts because headless runs
benefit from automatic recovery. The per-class `max_retries()` limit from GAP 1 fix
will cap timeout retries at 3 (via `ErrorClass::Timeout`). No code change needed,
but document the design decision.

## Tasks

### Task 1: Add drain on crash loop detection (GAP 6) — S

**File**: `src/daemon/heartbeat.rs`

**Changes**:

1. In `ErrorDetected` → `StopCrashLoop` path (around line 620-631):
   - After `append_event`, extract `repo_dir` from registry
   - Call `drain_if_available(Arc::clone(&registry), &repo_dir, true).await`

2. In `Crashed/Stuck/Timeout` → `StopCrashLoop` path (around line 690-703):
   - Same pattern: extract `repo_dir`, call `drain_if_available`

**Note**: `repo_dir` is already extracted earlier in both paths (line 608, 677).
Just need to pass it to `drain_if_available` before returning.

**Test**: Add test `crash_loop_drains_queue` — register two runs for same repo,
first enters crash loop, verify second gets drained.

### Task 2: Add per-error-class max_retries check (GAP 1) — M

**File**: `src/daemon/heartbeat.rs`

**Changes**:

1. In `ErrorDetected` path (after `CrashLoopDecision::AllowRestart`, around line 634):
   ```rust
   // Per-error-class retry limit (parity with foreground orchestrator)
   let crash_count = {
       let reg = registry.lock().await;
       reg.get(run_id).map(|e| e.crash_restarts).unwrap_or(0)
   };
   if crash_count >= error_class.max_retries() {
       let mut reg = registry.lock().await;
       let repo_dir = reg.get(run_id).map(|e| e.repo_dir.clone());
       let _ = reg.update_status(
           run_id, RunStatus::Failed, None,
           Some(format!("Max retries ({}) for {:?}: {}",
               error_class.max_retries(), error_class, reason)),
       );
       drop(reg);
       append_event(run_id, "max_retries", &format!("{:?}: {}", error_class, reason));
       if let Some(dir) = repo_dir {
           drain_if_available(Arc::clone(&registry), &dir, true).await;
       }
       return;
   }
   ```

2. In `Crashed/Stuck/Timeout` path (after `AllowRestart`, around line 704):
   Map outcome kind to implicit error class, then same check:
   ```rust
   let implicit_class = match outcome_kind {
       "crashed" => ErrorClass::Crash,
       "stuck" => ErrorClass::Stuck,
       "timeout" => ErrorClass::Timeout,
       _ => ErrorClass::Crash, // fallback
   };
   let crash_count = {
       let reg = registry.lock().await;
       reg.get(run_id).map(|e| e.crash_restarts).unwrap_or(0)
   };
   if crash_count >= implicit_class.max_retries() {
       // ... same pattern as above
   }
   ```

**Lock ordering**: Follows existing BACK-011 convention — acquire registry, extract,
drop, then proceed. No simultaneous lock holding.

**Test**: Add test `error_class_max_retries_stops_daemon_recovery` — simulate
`Stuck` outcome, verify it stops after 2 retries (not 5).

### Task 3: Use per-class backoff for Crashed/Stuck/Timeout (GAP 5) — S

**File**: `src/daemon/heartbeat.rs`

**Changes**: Replace flat cooldown (line 710-721) with per-class backoff:

```rust
// Per-class backoff instead of flat cooldown (parity with foreground)
let implicit_class = match outcome_kind {
    "crashed" => ErrorClass::Crash,
    "stuck" => ErrorClass::Stuck,
    "timeout" => ErrorClass::Timeout,
    _ => ErrorClass::Crash,
};
let crash_count = {
    let reg = registry.lock().await;
    reg.get(run_id).map(|e| e.crash_restarts).unwrap_or(0)
};
let cooldown = implicit_class.backoff_for_attempt(crash_count);
```

**Note**: This reuses the same `outcome_kind` → `ErrorClass` mapping from Task 2.
Extract the mapping into a helper to avoid duplication:

```rust
/// Map daemon outcome kind to implicit error class for retry policy.
fn implicit_error_class(outcome_kind: &str) -> ErrorClass {
    match outcome_kind {
        "crashed" => ErrorClass::Crash,
        "stuck" => ErrorClass::Stuck,
        "timeout" => ErrorClass::Timeout,
        _ => ErrorClass::Crash,
    }
}
```

**Test**: Add test verifying `Stuck` gets 30s backoff, `Crash` gets 60s.

### Task 4: Refactor outcome_kind to enum (DRY prerequisite) — S

**Rationale**: Tasks 2 and 3 both need `outcome_kind` → `ErrorClass` mapping.
Currently `outcome_kind` is `&str` ("crashed", "stuck", "timeout"). Using a string
for type dispatch is fragile. Refactor to use the `ErrorClass` directly.

**File**: `src/daemon/heartbeat.rs`

**Changes**: In the `Crashed/Stuck/Timeout` match arm (line 661-724):

```rust
outcome @ (DaemonRunOutcome::Crashed { .. }
| DaemonRunOutcome::Stuck { .. }
| DaemonRunOutcome::Timeout { .. }) => {
    let (implicit_class, reason) = match outcome {
        DaemonRunOutcome::Crashed { reason } => (ErrorClass::Crash, reason),
        DaemonRunOutcome::Stuck { reason } => (ErrorClass::Stuck, reason),
        DaemonRunOutcome::Timeout { reason } => (ErrorClass::Timeout, reason),
        _ => unreachable!(),
    };
    let outcome_kind = match implicit_class {
        ErrorClass::Crash => "crashed",
        ErrorClass::Stuck => "stuck",
        ErrorClass::Timeout => "timeout",
        _ => "crashed",
    };
    // ... rest uses implicit_class for max_retries + backoff,
    //     outcome_kind for append_event strings
}
```

### Task 5: Persist healthy_since for stability reset (GAP 2 + 3) — M

**Files**: `src/engine/crash_loop.rs`, `src/daemon/heartbeat.rs`

**Changes to `crash_loop.rs`**:

1. Add `last_healthy_epoch` to `CrashHistoryRecord`:
   ```rust
   struct CrashHistoryRecord {
       crash_epochs: Vec<u64>,
       total_restarts: u32,
       window_secs: u64,
       last_healthy_epoch: Option<u64>,  // NEW
   }
   ```

2. Update `persist()` to write `last_healthy_epoch`:
   ```rust
   let last_healthy_epoch = self.healthy_since.map(|since| {
       let age = now_instant.duration_since(since).as_secs();
       now_epoch.saturating_sub(age)
   });
   ```

3. Update `load_history()` to restore `healthy_since`:
   ```rust
   if let Some(epoch) = record.last_healthy_epoch {
       let age = now_epoch.saturating_sub(epoch);
       if age < self.stability_period.as_secs() {
           self.healthy_since = Some(now_instant - Duration::from_secs(age));
       }
   }
   ```

4. Add `pub fn record_healthy_runtime(&mut self, duration_secs: u64)`:
   New method that simulates healthy time without needing a live `Instant`:
   ```rust
   pub fn record_healthy_runtime(&mut self, duration_secs: u64) {
       if duration_secs >= self.stability_period.as_secs() {
           self.crash_times.clear();
           self.healthy_since = Some(Instant::now());
       } else if self.healthy_since.is_none() {
           self.healthy_since = Some(Instant::now());
       }
   }
   ```

**Changes to `heartbeat.rs`**:

In the `Crashed/Stuck/Timeout` path, after creating the detector and loading history,
check how long the run was alive before crashing:
```rust
// Check if session ran long enough to count as healthy (GAP 2 parity)
let run_uptime = {
    let reg = registry.lock().await;
    reg.get(run_id)
        .and_then(|e| e.started_at)
        .map(|s| s.elapsed().as_secs())
        .unwrap_or(0)
};
detector.record_healthy_runtime(run_uptime);
```

**Backward compatibility**: `last_healthy_epoch: Option<u64>` is optional — old
crash history files without this field will deserialize correctly (serde defaults
`Option` to `None`). No migration needed.

**Test**: Add tests for `record_healthy_runtime` and persist/load roundtrip with
`last_healthy_epoch`.

### Task 6: Update existing tests + add integration tests — S

**File**: `src/daemon/heartbeat.rs` (tests module)

1. **`test_crash_loop_drains_queue`** (Task 1):
   Verify `drain_if_available` is called when crash loop detected.

2. **`test_per_class_max_retries`** (Task 2):
   Set `entry.crash_restarts = 2`, trigger `Stuck` outcome, verify run marked Failed.

3. **`test_implicit_class_backoff`** (Task 3):
   Verify `Crashed` → 60s, `Stuck` → 30s, `Timeout` → 30s.

4. **`test_healthy_runtime_resets_crashes`** (Task 5):
   Create detector with 4 crashes, record 1800s healthy runtime, verify crashes cleared.

5. **`test_crash_history_roundtrip_with_healthy`** (Task 5):
   Persist detector with `healthy_since`, load into new detector, verify restored.

## Implementation Order

```
Task 4 (refactor outcome_kind to enum)
  ↓
Task 1 (drain on crash loop)     ← can be done independently
  ↓
Task 2 (per-class max_retries)   ← uses refactored enum from Task 4
  ↓
Task 3 (per-class backoff)       ← uses same enum, adjacent code
  ↓
Task 5 (healthy_tick persistence) ← separate concern, touches crash_loop.rs
  ↓
Task 6 (tests)                   ← after all logic changes
```

Tasks 1 and 4 can run in parallel. Tasks 2 and 3 are sequential (share same code region).
Task 5 is independent of 1-4 but should be last to minimize merge conflicts.

### GAP 7: Timeout is retried in daemon but terminal in foreground — DESIGN DECISION

**Foreground** (`orchestrator.rs:413-420`): `SessionOutcome::Timeout` returns
`PipelineResult { success: false }` immediately. No retry, no crash detector, terminal.

**Daemon** (`heartbeat.rs:661-724`): `DaemonRunOutcome::Timeout` is grouped with
`Crashed` and `Stuck` in the same match arm — it enters `CrashLoopDetector` and
triggers `attempt_recovery()`.

**Analysis**: This is a **deliberate design choice**, not a bug. Daemon runs are
headless overnight jobs where a timeout might be caused by transient system load or
API slowness. Retrying with `--resume` (which skips completed phases) is more useful
than failing permanently. Foreground runs have a human watching who can manually
restart if needed.

**Decision**: Keep current daemon behavior (retry on timeout). But with Task 2's
per-class `max_retries()` check, `Timeout` maps to `ErrorClass::Timeout` which has
`max_retries() == 3`, so daemon will now stop after 3 timeout retries instead of 5.
This is a reasonable middle ground.

**Note for Task 4**: The `implicit_error_class` mapping should document this
intentional asymmetry:
```rust
/// Note: Foreground treats Timeout as terminal (no retry).
/// Daemon retries because headless runs benefit from automatic recovery.
/// Per-class max_retries (3) still limits total attempts.
"timeout" => ErrorClass::Timeout,
```

## Risk Assessment

| Risk | Mitigation |
|------|-----------|
| Lock ordering violation (BACK-011) | Follow existing pattern: acquire → extract → drop → proceed |
| Serde backward compat on CrashHistoryRecord | `Option<u64>` defaults to `None` for old files |
| `outcome_kind` string matching fragile | Task 4 replaces with enum match |
| Drain on crash loop could trigger spawn failure | Existing `drain_if_available` already handles spawn errors with circuit breaker |
| Registry `crash_restarts` vs detector `total_restarts` divergence | GAP 1 fix uses `entry.crash_restarts` (canonical), detector only for sliding window |

## Acceptance Criteria

- [ ] `StopCrashLoop` paths call `drain_if_available` (queue doesn't get stuck)
- [ ] `Stuck` error stops after 2 retries in daemon (was 5)
- [ ] `PermissionBlocked` stops after 2 retries in daemon (was 5)
- [ ] `Crashed` outcome uses 60s backoff (was flat `restart_cooldown_secs`)
- [ ] `Stuck` outcome uses 30s backoff (was flat `restart_cooldown_secs`)
- [ ] Long-running healthy sessions reset crash counter via persistence
- [ ] Old crash history files (without `last_healthy_epoch`) load correctly
- [ ] All new code follows BACK-011 lock ordering convention
- [ ] `cargo test` passes with new tests covering all 6 gaps
- [ ] `cargo clippy` clean
