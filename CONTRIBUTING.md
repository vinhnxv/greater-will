# Contributing to greater-will

This document collects conventions that are not obvious from reading the
code. Each section names a load-bearing pattern, the invariant it
protects, and where to look when extending it.

## Queue and status mutations — staged/flush convention

Two persistent stores, two mirrored conventions. Both move the fsync
outside the registry mutex (`INV-19`).

### `meta.json` per run — `stage_status_locked` / `flush_status`

- Mutation methods that update a `RunEntry` inside the registry mutex
  call [`stage_status_locked`](src/daemon/registry.rs) to capture a
  `StagedStatus` value while holding the lock.
- Callers drop the mutex guard, then invoke
  [`flush_status`](src/daemon/registry.rs) on every staged value to
  fsync `~/.gw/runs/<id>/meta.json`.
- See `executor::drain_if_available` for the canonical pattern in
  production.

### `queue.json` — `stage_queue_locked` / `flush_queue` (PERF-003)

The same shape, applied to the global pending-queue file:

| API form | When to use | Example |
|----------|-------------|---------|
| Sync wrapper (`enqueue_run`, `drain_next`, `record_queue_failure`, `record_queue_success`, `clear_queue`) | Tests, daemon shutdown path, single-threaded contexts. Gated behind `#[cfg_attr(not(test), allow(dead_code))]` because production paths take the staged form. | `reg.enqueue_run(p)?` |
| `*_staged` (`enqueue_run_staged`, …) | Single-mutation production paths under an async lock | `let (pos, snap) = reg.enqueue_run_staged(p)?; drop(reg); RunRegistry::flush_queue(&snap)?` |
| `*_without_snapshot` (`pub(crate)`) + trailing `stage_queue_locked` | Multi-mutation production paths (the **canonical** form) | `executor::drain_if_available` |

#### Rules

1. **When adding a new mutation method, ship a `*_staged` sibling that
   returns `QueueSnapshot`.** The sync wrapper must delegate to the
   staged variant + `flush_queue`, never duplicate the mutation logic.
2. **Type-level `#[must_use]` on `QueueSnapshot` is load-bearing.** Do
   not remove. It catches tuple-destructure discards (`let (_pos,
   _snap) = …`) that a function-level `#[must_use]` would silently
   permit.
3. **The idempotency guard is not a substitute for flushing.** A
   `QueueSnapshot` dropped without `flush_queue` triggers a
   production-visible `tracing::error!("BUG: QueueSnapshot dropped …")`
   line. Investigate any such log entry as an invariant violation.
4. **For multi-mutation scopes, prefer `*_without_snapshot` + a single
   trailing `stage_queue_locked`** over picking which `*_staged`
   tuple's snapshot supersedes another. The canonical form is
   demonstrated in `executor::drain_if_available` and tested by
   `multi_mutation_single_flush_captures_cumulative_state`.
5. **Shutdown stays sync.** `daemon::mod.rs` flushes the final queue
   under the lock at exit time — there is no concurrency pressure at
   that point and the simple sync contract is appropriate. Do not
   migrate the shutdown call.

#### Audit query

A single `rg` query covers both halves of INV-19:

```bash
rg 'stage_.*_locked' src/daemon/registry.rs
```

Should match both `stage_status_locked` and `stage_queue_locked`.

#### History

- `meta.json` side: landed via
  [`fix-daemon-registry-lock-and-fsync-plan.md`](plans/archived/2026-04-15-fix-daemon-registry-lock-and-fsync-plan.md)
  (commit `5a732ca`).
- `queue.json` side: landed via
  `plans/2026-04-15-perf-queue-fsync-outside-mutex-plan.md`.

## Bash command conventions

- Use `rg` (ripgrep) for text/literal search; never `grep` or `find`.
- Use `ast-grep` for structural code patterns and refactors.
- Use `fd` instead of `find` for file discovery.
- Quote all glob expansions when invoking from non-zsh shells.

## Test conventions

- All daemon tests use `with_temp_gw_home` to isolate `~/.gw/` per test.
- Integration tests that touch the registry must serialise via the
  shared `gw_home_test_mutex` — a poisoned guard recovers via
  `into_inner()` rather than propagating the lock-poison cascade.
- Tracing-output assertions use the in-module `LogCapture` MakeWriter
  (see `daemon::registry::tests`) — no third-party `tracing-test`
  dependency required.
