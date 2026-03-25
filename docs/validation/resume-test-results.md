# Resume Checkpoint Manipulation — Validation Results

## Status: PENDING

> **Task 0 from Shard 2 plan**: Validate that Rune arc correctly resumes
> from an externally-edited checkpoint.

## Test Protocol

1. Run a real `/rune:arc` on a test plan — let it complete forge + forge_qa
2. Copy `.rune/arc/{id}/checkpoint.json` to a backup
3. In the backup, manually edit: set `plan_review.status = "pending"`, verify `phase_sequence` points to plan_review
4. Start a NEW Claude Code session (fresh terminal)
5. Run `/rune:arc {same plan} --resume`
6. Observe: does arc skip forge/forge_qa and start at plan_review?

## Success Criteria

- Arc resumes at the correct phase in a fresh session
- Phases before target are correctly recognized as completed/skipped
- No session_nonce/owner_pid validation failures

## Failure Criteria

- Arc re-runs from beginning
- Arc crashes on resume
- Arc validates session_nonce/owner_pid and rejects the checkpoint

## Test Results

| Date | Tester | Arc ID | Result | Notes |
|------|--------|--------|--------|-------|
| _pending_ | — | — | — | Test not yet executed |

## Go/No-Go Decision

- [ ] **GO**: Checkpoint manipulation approach works — proceed with PhaseGroupExecutor wiring
- [ ] **NO-GO (Option B)**: Let arc run all phases in one session; greater-will monitors checkpoint and kills session after target group completes
- [ ] **NO-GO (Option C)**: Use `/rune:arc {plan} --resume --start-phase {phase}` if such a flag exists

## Fallback Notes

_Document any fallback approach details here after testing._
