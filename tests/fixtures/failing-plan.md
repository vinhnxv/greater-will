---
type: feat
name: failing-test-plan
scope: MICRO
---

# Failing Test Plan

A plan designed to fail at Group C for testing error handling and crash recovery.

## Tasks

### Task 1: Setup (Group A-B)

Basic setup that should succeed.

### Task 2: Intentional Failure (Group C)

This task is designed to trigger a failure in the mock script
when GW_GROUP_NAME=C.

### Task 3: Should Not Run (Group D-G)

These tasks should be skipped because Group C failed.

## Acceptance Criteria

- [ ] Groups A and B succeed
- [ ] Group C fails with non-zero exit
- [ ] Groups D-G are skipped
