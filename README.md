# Greater Will

External arc controller for [Rune](https://github.com/vinhnxv/rune) workflows. Runs `/rune:arc` pipelines in managed tmux sessions with crash recovery, stuck detection, and context injection.

Named after the Greater Will from Elden Ring — the outer god that orchestrates from beyond.

## What it does

```
gw run plans/feat-auth.md
```

Greater-Will spawns a Claude Code session in tmux, sends `/rune:arc {plan}`, and watches over it:

```
┌────────────────────────────────────────────────┐
│  1. SPAWN    tmux session + claude code         │
│  2. INJECT   workspace context via hook (elden) │
│  3. DISPATCH /rune:arc plans/feat.md            │
│  4. MONITOR  poll for completion / stuck / crash│
│  5. RECOVER  restart with --resume on crash     │
│  6. CLEANUP  kill session + process tree        │
└────────────────────────────────────────────────┘
```

## Install

```bash
cargo install --path .
```

### Prerequisites

- **tmux** — `brew install tmux` (macOS) or `apt install tmux` (Linux)
- **claude** CLI — Claude Code must be in PATH
- **Rune plugin** — installed in Claude Code for `/rune:arc` support

## Quick Start

```bash
# 1. Install hooks (one-time setup)
gw elden --install

# 2. Run a plan
gw run plans/my-feature.md

# 3. Check status
gw status

# 4. Resume after crash
gw run plans/my-feature.md --resume
```

## Commands

### `gw run` — Execute plans

```bash
# Single plan (default: single-session mode)
gw run plans/feat.md

# Multiple plans (batch, sequential)
gw run plans/*.md

# Resume interrupted run
gw run plans/feat.md --resume

# Dry run — print execution plan, no sessions spawned
gw run plans/feat.md --dry-run

# Mock mode — test with a script instead of real Claude
gw run plans/feat.md --mock tests/mock/mock-success.sh

# Multi-group mode (legacy) — 7 sessions, one per phase group
gw run plans/feat.md --multi-group
gw run plans/feat.md --multi-group --group C
```

### `gw elden` — Hook context injection

Injects workspace state into Claude Code's context window via the `SessionStart` hook. Claude Code captures stdout from hook commands and feeds it to the model.

```bash
# Install hooks into .claude/settings.json
gw elden --install

# Check hook status
gw elden --status

# Remove hooks
gw elden --uninstall
```

When installed, every new Claude Code session in this project automatically receives:
- Current batch progress (plan index, pass/fail counts)
- Arc checkpoint state (current phase, completion percentage)
- Resume instructions
- Protocol reminders (lifecycle management, timeout behavior)

On context compaction or session resume, a brief 2-line summary is injected instead to conserve context.

### `gw status` — Show batch progress

```bash
gw status
```

Displays the active batch state: elapsed time, plan progress, pass/fail/skip counts.

### `gw clean` — Cleanup

```bash
gw clean
```

Kills all `gw-*` tmux sessions and owned Claude processes. Safe — only targets processes within Greater-Will sessions, never touches external Claude Code instances.

### `gw replay` — Resume from checkpoint

```bash
gw replay .rune/arc/arc-12345/checkpoint.json
```

## Execution Modes

### Single-session (default)

One tmux session runs the entire `/rune:arc` pipeline. Rune's own stop hook drives phase iteration internally. Greater-Will acts as a watchdog:

- **Crash recovery**: auto-restarts with `--resume` (up to 3 retries)
- **Stuck detection**: nudge after 5 min idle, kill after 10 min
- **Timeout**: 6-hour pipeline hard cap

### Multi-group (`--multi-group`)

Splits the 41 arc phases into 7 groups (A-G), each in a separate tmux session. Designed for future use when Rune supports per-phase-group execution.

| Group | Phases | Timeout |
|-------|--------|---------|
| A | forge, plan_review, verification | 30 min |
| B | semantic, design, task_decomposition | 30 min |
| C | work | 90 min |
| D | work_qa, gap_analysis, inspect | 60 min |
| E | code_review, mend | 60 min |
| F | design_iteration, test | 60 min |
| G | pre_ship, ship, merge | 30 min |

## Process Safety

Greater-Will **only kills processes it owns**. The cleanup system uses two safety layers:

1. **Tmux session scoping** — only touches `gw-*` prefixed tmux sessions
2. **Owned PID registry** — tracks PIDs spawned by `spawn_claude_session()`

This means `gw clean` and pre-phase cleanup will never kill:
- Your interactive `claude` CLI sessions
- Claude Code Desktop app
- VS Code / JetBrains Claude extensions
- Other tools that spawn Claude Code

## Architecture

```
src/
├── commands/
│   ├── run.rs          # Run command (single-session + multi-group routing)
│   ├── elden.rs        # Hook context injection + install/uninstall
│   ├── status.rs       # Batch status display
│   ├── clean.rs        # Cleanup command
│   └── replay.rs       # Checkpoint resume
├── engine/
│   ├── single_session.rs  # Single-session pipeline executor
│   ├── phase_executor.rs  # Multi-group phase executor
│   ├── completion.rs      # 4-layer completion detection
│   └── retry.rs           # Retry with exponential backoff
├── session/
│   ├── spawn.rs        # Tmux session creation + Ink workaround
│   ├── detect.rs       # Prompt detection + output velocity
│   └── tmux.rs         # Tmux wrapper
├── cleanup/
│   ├── process.rs      # Process tree management + owned PID registry
│   ├── tmux_cleanup.rs # Stale gw-* session cleanup
│   └── health.rs       # System health gates (RAM/CPU)
├── batch/
│   ├── queue.rs        # Batch plan queue manager
│   ├── state.rs        # Persistent batch state (atomic writes)
│   └── lock.rs         # Instance lock (PID-based)
├── checkpoint/         # Arc checkpoint read/write/migration
├── config/             # CLI args + phase group config (TOML)
├── monitor/            # Nudge manager
├── log/                # Structured logging + JSONL
├── output/             # Progress bars + tags
└── scanner/            # Plan file discovery
```

## Configuration

Phase groups are defined in `config/default-phases.toml`. Override with a project-local `greater-will.toml` or `--config-dir` flag.

## License

MIT
