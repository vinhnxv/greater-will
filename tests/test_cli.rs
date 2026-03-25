//! CLI integration tests for the `gw` binary.
//!
//! Uses `assert_cmd` to test the compiled binary as a black box.

use assert_cmd::Command;
use predicates::prelude::*;

fn gw() -> Command {
    Command::cargo_bin("gw").expect("Failed to find gw binary")
}

#[test]
fn test_help_shows_subcommands() {
    gw()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("run"))
        .stdout(predicate::str::contains("status"))
        .stdout(predicate::str::contains("replay"))
        .stdout(predicate::str::contains("clean"));
}

#[test]
fn test_version_prints_version() {
    gw()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("gw"));
}

#[test]
fn test_run_help_shows_flags() {
    gw()
        .args(["run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--dry-run"))
        .stdout(predicate::str::contains("--mock"))
        .stdout(predicate::str::contains("--group"))
        .stdout(predicate::str::contains("--config-dir"));
}

#[test]
fn test_run_requires_plan_argument() {
    gw()
        .arg("run")
        .assert()
        .failure()
        .stderr(predicate::str::contains("required"));
}

#[test]
fn test_run_dry_run_with_sample_plan() {
    gw()
        .args(["run", "--dry-run", "tests/fixtures/sample-plan.md"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[DRY]"))
        .stdout(predicate::str::contains("Group A"))
        .stdout(predicate::str::contains("Group G"));
}

#[test]
fn test_run_dry_run_with_group_filter() {
    gw()
        .args(["run", "--dry-run", "--group", "C", "tests/fixtures/sample-plan.md"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Group C"))
        // Other groups should be filtered out
        .stdout(predicate::str::contains("Group A").not());
}

#[test]
fn test_run_invalid_group_error() {
    gw()
        .args(["run", "--multi-group", "--allow-dirty", "--group", "Z", "tests/fixtures/sample-plan.md"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Unknown group 'Z'"));
}

#[test]
fn test_replay_with_fixture() {
    gw()
        .args(["replay", "tests/fixtures/checkpoint-v27.json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("CHECKPOINT STATUS"));
}

#[test]
fn test_replay_missing_file() {
    gw()
        .args(["replay", "nonexistent-checkpoint.json"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
}

#[test]
fn test_status_no_active_batch() {
    gw()
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("No active batch found"));
}

#[test]
fn test_run_mock_missing_script() {
    gw()
        .args(["run", "--mock", "/nonexistent/mock.sh", "tests/fixtures/sample-plan.md"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Mock script not found"));
}

#[test]
fn test_run_nonexistent_plan_glob() {
    gw()
        .args(["run", "--dry-run", "nonexistent-plans/*.md"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("No plan files matched"));
}

// === Batch-related CLI tests ===

#[test]
fn test_run_multiple_plans_dry_run() {
    gw()
        .args([
            "run",
            "--dry-run",
            "tests/fixtures/sample-plan.md",
            "tests/fixtures/fast-plan.md",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("[DRY]"))
        .stdout(predicate::str::contains("sample-plan.md"))
        .stdout(predicate::str::contains("fast-plan.md"));
}

#[test]
fn test_run_plan_glob_expansion() {
    gw()
        .args(["run", "--dry-run", "tests/fixtures/*-plan.md"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[DRY]"));
}

#[test]
fn test_run_dry_run_with_fast_plan() {
    gw()
        .args(["run", "--dry-run", "tests/fixtures/fast-plan.md"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[DRY]"))
        .stdout(predicate::str::contains("Group A"));
}

#[test]
fn test_run_dry_run_with_failing_plan() {
    gw()
        .args(["run", "--dry-run", "tests/fixtures/failing-plan.md"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[DRY]"));
}

#[test]
fn test_run_mock_with_nonexecutable_script() {
    // Create a temp file that is not executable
    let tmp = tempfile::NamedTempFile::new().expect("create temp file");
    std::fs::write(tmp.path(), "#!/bin/bash\nexit 0").expect("write temp");

    // Remove execute permission
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o644);
        std::fs::set_permissions(tmp.path(), perms).expect("set perms");
    }

    gw()
        .args([
            "run",
            "--mock",
            tmp.path().to_str().unwrap(),
            "tests/fixtures/sample-plan.md",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not executable"));
}

#[test]
fn test_replay_help_shows_checkpoint_arg() {
    gw()
        .args(["replay", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("checkpoint"));
}

#[test]
fn test_clean_runs_successfully() {
    gw()
        .arg("clean")
        .assert()
        .success();
}
