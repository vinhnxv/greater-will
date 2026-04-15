# Daemon Architecture

## Async Model

The daemon runs on a **tokio** multi-threaded runtime. A central
`HeartbeatMonitor` drives a periodic tick (every 10 s) that inspects all
active runs, captures tmux pane output, updates phase tracking, and
manages per-run `DaemonRunMonitor` watchdog tasks.

## INV-19: No Blocking I/O on the Reactor Thread — AIRTIGHT

The core invariant is that **no blocking system call** (filesystem writes,
`std::process::Command` for tmux) may execute directly on a tokio worker
thread. Violating this stalls the entire heartbeat tick and all IPC
handlers that share the runtime.

**Status: AIRTIGHT** — as of the refactoring completed in commits
`5a732ca` through `6a6d2f9`, every blocking I/O site in `src/daemon/` is
hoisted off the reactor. Both filesystem and tmux process-spawning paths
are covered.

### Audit Findings Addressed

| Finding   | Description                                        | Resolution              |
|-----------|----------------------------------------------------|-------------------------|
| CP1-6/7/8 | Blocking `append_event` / `append_pane_log` on reactor | Hoisted to `spawn_blocking` |
| FRINGE-1  | `scan_artifact_dir` blocking during run startup    | Async wrapper added     |
| BUG-2     | Registry lock held across fsync                    | Lock dropped before I/O |
| INV-7     | Registry lock leak on early return                 | Guard scoping fixed     |
| INV-19    | Mutex held across fsync in heartbeat               | Split into sync helper  |
| EMB-001   | Warmup double-read in `poll_loop_state`            | Derive stale status from initial read |
| EMB-005   | Unbounded tmux `capture_pane` parallelism          | `Semaphore(4)` bound    |

## spawn_blocking Hoisting Pattern

All blocking operations are moved off the reactor via
`tokio::task::spawn_blocking`. The pattern has two variants depending on
whether the caller needs the result:

### Fire-and-forget (best-effort writes)

Used for event logging and pane log appends where the write is
best-effort and errors are logged but not propagated:

```rust
// events.rs — append_event
if tokio::runtime::Handle::try_current().is_ok() {
    let _ = tokio::task::spawn_blocking(move || {
        append_event_sync(&run_id, &event, &message);
    });
} else {
    // Fallback for unit tests / CLI one-shots without a runtime
    append_event_sync(run_id, event, message);
}
```

The `try_current()` guard ensures the function remains callable from
non-async contexts (tests, CLI utilities) by falling back to synchronous
inline execution.

**Applies to:** `append_event` (events.rs), `append_pane_log` (heartbeat.rs)

### Awaited result

Used when the async caller needs the output (e.g. tmux pane content,
artifact directory listing, session existence checks):

```rust
// run_monitor.rs — capture_pane, scan_artifact_dir, has_session, etc.
let result = tokio::task::spawn_blocking(move || {
    blocking_call(&args)
}).await?;
```

**Applies to:** `capture_pane`, `scan_artifact_dir`, `has_session`,
`get_claude_pid`, `poll_checkpoint`, `poll_loop_state` (run_monitor.rs,
heartbeat.rs, executor.rs)

## Bounded Concurrency for tmux Captures

Tmux `capture_pane` calls are fanned out in a `JoinSet` with an
`Arc<Semaphore>` bound of **4** (`TMUX_CAPTURE_PARALLELISM` in
heartbeat.rs:36). This prevents thread-pool exhaustion when many runs are
active simultaneously — without the bound, a heartbeat tick monitoring
*N* runs would spawn *N* blocking tasks that each invoke
`std::process::Command`, potentially saturating tokio's blocking thread
pool and starving other spawn_blocking callers.

```rust
// heartbeat.rs
const TMUX_CAPTURE_PARALLELISM: usize = 4;

let sem = Arc::new(Semaphore::new(TMUX_CAPTURE_PARALLELISM));
// Each capture task acquires a permit before spawning
capture_set.spawn_blocking(move || { /* capture_pane */ });
```

The semaphore is created per heartbeat tick and scoped to the capture
fan-out, so it does not interfere with other spawn_blocking work.

## Module Extraction

`events.rs` was extracted from `heartbeat.rs` (commit `1159dac`) to break
a module dependency cycle (`heartbeat → server → heartbeat` via
`drain_if_available`). Keeping event helpers in a leaf module lets
executor, reconciler, run_monitor, and heartbeat all depend on `events`
without depending on each other.

## References

- Source audit: `src/daemon/heartbeat.rs` (PERF-001, EMB-005)
- Source audit: `src/daemon/events.rs` (CP1-6/7/8)
- Source audit: `src/daemon/run_monitor.rs` (FRINGE-1, EMB-001)
- Invariant tracking: INV-19 (registry lock + mutex-across-fsync fix)
- Invariant tracking: INV-7 (registry lock leak on early return)
