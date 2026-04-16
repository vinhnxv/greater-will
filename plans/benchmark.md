---
type: chore
feature: benchmark-nop
created: 2026-04-15
status: scaffold
purpose: synthetic no-op plan used by daemon IPC and queue-fsync benchmarks
---

# Benchmark no-op plan

This plan is intentionally minimal. Its purpose is to serve as the
payload for `gw submit` calls during synthetic load tests — most
notably the burst-enqueue methodology described in
`plans/2026-04-15-perf-queue-fsync-outside-mutex-plan.md` § T7
(VIGIL-5).

The plan body is a single no-op task so that:

1. The daemon's enqueue/queue-persistence code paths run end-to-end.
2. No real work happens in the spawned tmux session, so a benchmark
   loop can issue many submits without consuming CPU on phase
   execution.
3. The on-disk `queue.json` size grows linearly with submit count so
   serialization cost is predictable.

## Tasks

### T1 — Return immediately

No-op task used for daemon IPC benchmarking. The runner is expected to
exit cleanly without taking any action. Used as the payload for
`gw submit plans/benchmark.md --repo /tmp/bench$N` invocations during
synthetic load tests.

## Usage

Burst submit (PERF-003 mutex-hold p99 measurement):

```bash
RUST_LOG=greater_will::daemon::registry=debug,greater_will::daemon::server=debug gw daemon start &
for i in $(seq 1 50); do
  gw submit plans/benchmark.md --repo /tmp/bench$((i % 4)) &
done
wait
gw daemon stop
```

Extract `registry_lock_held` span durations from the structured log
and compute p50/p95/p99. Acceptance per the PERF-003 plan: p99
mutex-hold ≤ 1 ms (stage-only, no fsync).
