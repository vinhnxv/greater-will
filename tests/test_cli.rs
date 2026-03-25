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
        .args(["run", "--dry-run", "--group", "Z", "tests/fixtures/sample-plan.md"])
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
fn test_status_stub() {
    gw()
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("not implemented"));
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
