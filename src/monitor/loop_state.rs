//! Arc loop state reader for `arc-phase-loop.local.md`.
//!
//! This file is Rune's single source of truth for the active arc session.
//! It contains the checkpoint path, plan file, branch, and other metadata
//! as YAML frontmatter. Reading this file is O(1) — no directory scanning needed.
//!
//! Ported from torrent's `monitor::read_arc_loop_state`.

use std::path::{Path, PathBuf};
use tracing::debug;

/// Parsed state from `.rune/arc-phase-loop.local.md`.
#[derive(Debug, Clone, PartialEq)]
pub struct ArcLoopState {
    pub active: bool,
    pub checkpoint_path: String,
    pub plan_file: String,
    pub config_dir: String,
    pub owner_pid: String,
    pub session_id: String,
    pub branch: String,
    pub iteration: u32,
    pub max_iterations: u32,
}

/// A single field change between two `ArcLoopState` snapshots.
#[derive(Debug, Clone)]
pub struct LoopStateChange {
    pub field: &'static str,
    pub old_value: String,
    pub new_value: String,
    /// Whether this change is unexpected during a normal arc run.
    pub anomalous: bool,
}

impl ArcLoopState {
    /// Compare two states and return all field-level differences.
    ///
    /// Fields are classified as normal (iteration increment) or anomalous
    /// (branch change, pid change, plan change mid-run).
    pub fn diff(&self, new: &ArcLoopState) -> Vec<LoopStateChange> {
        let mut changes = Vec::new();

        if self.active != new.active {
            changes.push(LoopStateChange {
                field: "active",
                old_value: self.active.to_string(),
                new_value: new.active.to_string(),
                anomalous: false, // active → false is expected on shutdown
            });
        }
        if self.iteration != new.iteration {
            changes.push(LoopStateChange {
                field: "iteration",
                old_value: self.iteration.to_string(),
                new_value: new.iteration.to_string(),
                anomalous: new.iteration < self.iteration, // decrement is anomalous
            });
        }
        if self.checkpoint_path != new.checkpoint_path {
            changes.push(LoopStateChange {
                field: "checkpoint_path",
                old_value: self.checkpoint_path.clone(),
                new_value: new.checkpoint_path.clone(),
                anomalous: true, // shouldn't change during a run
            });
        }
        if self.plan_file != new.plan_file {
            changes.push(LoopStateChange {
                field: "plan_file",
                old_value: self.plan_file.clone(),
                new_value: new.plan_file.clone(),
                anomalous: true, // should never change
            });
        }
        if self.branch != new.branch {
            changes.push(LoopStateChange {
                field: "branch",
                old_value: self.branch.clone(),
                new_value: new.branch.clone(),
                anomalous: true, // should never change mid-run
            });
        }
        if self.owner_pid != new.owner_pid {
            changes.push(LoopStateChange {
                field: "owner_pid",
                old_value: self.owner_pid.clone(),
                new_value: new.owner_pid.clone(),
                anomalous: true, // implies session restart from outside gw
            });
        }
        if self.session_id != new.session_id {
            changes.push(LoopStateChange {
                field: "session_id",
                old_value: self.session_id.clone(),
                new_value: new.session_id.clone(),
                anomalous: true,
            });
        }
        if self.max_iterations != new.max_iterations {
            changes.push(LoopStateChange {
                field: "max_iterations",
                old_value: self.max_iterations.to_string(),
                new_value: new.max_iterations.to_string(),
                anomalous: true, // shouldn't change mid-run
            });
        }
        // config_dir changes are not tracked — it's immutable and not interesting.

        changes
    }

    /// Resolve `checkpoint_path` to an absolute path relative to `working_dir`.
    pub fn resolve_checkpoint_path(&self, working_dir: &Path) -> PathBuf {
        if self.checkpoint_path.starts_with('/') {
            PathBuf::from(&self.checkpoint_path)
        } else {
            working_dir.join(&self.checkpoint_path)
        }
    }

    /// Extract the arc-{id} directory name from the checkpoint path.
    pub fn arc_id(&self) -> Option<&str> {
        // checkpoint_path is like ".rune/arc/arc-1774560563000/checkpoint.json"
        let path = Path::new(&self.checkpoint_path);
        path.parent()?.file_name()?.to_str()
    }
}

/// Result of reading `arc-phase-loop.local.md`.
#[derive(Debug, Clone)]
pub enum LoopStateRead {
    /// File does not exist or is unparseable.
    Missing,
    /// File exists but `active: false` (leftover from a previous run).
    Inactive,
    /// File exists with `active: true` and valid content.
    Active(ArcLoopState),
}

impl LoopStateRead {
    /// Returns the active state if present, `None` otherwise.
    pub fn active(&self) -> Option<&ArcLoopState> {
        match self {
            LoopStateRead::Active(s) => Some(s),
            _ => None,
        }
    }

    /// Returns `true` if the file physically exists on disk (active or inactive).
    pub fn file_exists(&self) -> bool {
        !matches!(self, LoopStateRead::Missing)
    }
}

/// Read and parse `arc-phase-loop.local.md` from the project directory.
///
/// Returns `Missing` if the file doesn't exist or can't be parsed,
/// `Inactive` if the file exists but `active` is `false`,
/// `Active(state)` if the file is active and valid.
pub fn read_arc_loop_state(working_dir: &Path) -> LoopStateRead {
    let loop_file = working_dir.join(".rune").join("arc-phase-loop.local.md");
    let contents = match std::fs::read_to_string(&loop_file) {
        Ok(c) => c,
        Err(_) => return LoopStateRead::Missing,
    };

    let yaml = match extract_frontmatter(&contents) {
        Some(y) => y,
        None => return LoopStateRead::Missing,
    };

    let active = match parse_yaml_bool(&yaml, "active") {
        Some(a) => a,
        None => return LoopStateRead::Missing,
    };

    if !active {
        debug!("arc-phase-loop.local.md exists but active=false");
        return LoopStateRead::Inactive;
    }

    let checkpoint_path = match parse_yaml_str(&yaml, "checkpoint_path") {
        Some(p) if !p.is_empty() => p,
        _ => {
            debug!("arc-phase-loop state has empty checkpoint_path");
            return LoopStateRead::Missing;
        }
    };
    let plan_file = match parse_yaml_str(&yaml, "plan_file") {
        Some(p) if !p.is_empty() => p,
        _ => {
            debug!("arc-phase-loop state has empty plan_file");
            return LoopStateRead::Missing;
        }
    };

    LoopStateRead::Active(ArcLoopState {
        active,
        checkpoint_path,
        plan_file,
        config_dir: parse_yaml_str(&yaml, "config_dir").unwrap_or_default(),
        owner_pid: parse_yaml_str(&yaml, "owner_pid").unwrap_or_default(),
        session_id: parse_yaml_str(&yaml, "session_id").unwrap_or_default(),
        branch: parse_yaml_str(&yaml, "branch").unwrap_or_default(),
        iteration: parse_yaml_str(&yaml, "iteration")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        max_iterations: parse_yaml_str(&yaml, "max_iterations")
            .and_then(|s| s.parse().ok())
            .unwrap_or(50),
    })
}

/// Extract YAML frontmatter content between `---` markers.
fn extract_frontmatter(content: &str) -> Option<String> {
    let trimmed = content.trim();
    let after_first = trimmed.strip_prefix("---")?;
    let end = after_first.find("---")?;
    Some(after_first[..end].to_string())
}

/// Parse a simple `key: value` from YAML content.
fn parse_yaml_str(yaml: &str, key: &str) -> Option<String> {
    let prefix = format!("{}:", key);
    for line in yaml.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix(&prefix) {
            let val = value.trim();
            let val = val.trim_matches('"').trim_matches('\'');
            if val.is_empty() || val == "null" {
                return None;
            }
            return Some(val.to_string());
        }
    }
    None
}

/// Parse a boolean `key: true/false` from YAML content.
fn parse_yaml_bool(yaml: &str, key: &str) -> Option<bool> {
    let val = parse_yaml_str(yaml, key)?;
    match val.as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_loop_state(dir: &Path, content: &str) {
        let rune_dir = dir.join(".rune");
        std::fs::create_dir_all(&rune_dir).unwrap();
        std::fs::write(rune_dir.join("arc-phase-loop.local.md"), content).unwrap();
    }

    #[test]
    fn test_read_active_loop_state() {
        let dir = TempDir::new().unwrap();
        write_loop_state(dir.path(), "\
---
active: true
iteration: 5
max_iterations: 65
checkpoint_path: .rune/arc/arc-1774560563000/checkpoint.json
plan_file: plans/my-plan.md
branch: rune/arc-my-plan
arc_flags: --resume
config_dir: /Users/test/.claude
owner_pid: 12345
session_id: abc-def-123
---
");
        let result = read_arc_loop_state(dir.path());
        assert!(result.file_exists());
        let state = result.active().unwrap();
        assert!(state.active);
        assert_eq!(state.checkpoint_path, ".rune/arc/arc-1774560563000/checkpoint.json");
        assert_eq!(state.plan_file, "plans/my-plan.md");
        assert_eq!(state.branch, "rune/arc-my-plan");
        assert_eq!(state.owner_pid, "12345");
        assert_eq!(state.iteration, 5);
        assert_eq!(state.max_iterations, 65);
        assert_eq!(state.arc_id(), Some("arc-1774560563000"));
    }

    #[test]
    fn test_read_inactive_returns_inactive() {
        let dir = TempDir::new().unwrap();
        write_loop_state(dir.path(), "\
---
active: false
checkpoint_path: .rune/arc/arc-123/checkpoint.json
plan_file: plans/test.md
---
");
        let result = read_arc_loop_state(dir.path());
        assert!(result.file_exists());
        assert!(result.active().is_none());
        assert!(matches!(result, LoopStateRead::Inactive));
    }

    #[test]
    fn test_missing_file_returns_missing() {
        let dir = TempDir::new().unwrap();
        let result = read_arc_loop_state(dir.path());
        assert!(!result.file_exists());
        assert!(result.active().is_none());
        assert!(matches!(result, LoopStateRead::Missing));
    }

    #[test]
    fn test_resolve_checkpoint_path_relative() {
        let state = ArcLoopState {
            active: true,
            checkpoint_path: ".rune/arc/arc-123/checkpoint.json".into(),
            plan_file: "plans/test.md".into(),
            config_dir: String::new(),
            owner_pid: String::new(),
            session_id: String::new(),
            branch: String::new(),
            iteration: 0,
            max_iterations: 50,
        };
        let resolved = state.resolve_checkpoint_path(Path::new("/project"));
        assert_eq!(resolved, PathBuf::from("/project/.rune/arc/arc-123/checkpoint.json"));
    }

    #[test]
    fn test_resolve_checkpoint_path_absolute() {
        let state = ArcLoopState {
            active: true,
            checkpoint_path: "/abs/path/checkpoint.json".into(),
            plan_file: "plans/test.md".into(),
            config_dir: String::new(),
            owner_pid: String::new(),
            session_id: String::new(),
            branch: String::new(),
            iteration: 0,
            max_iterations: 50,
        };
        let resolved = state.resolve_checkpoint_path(Path::new("/project"));
        assert_eq!(resolved, PathBuf::from("/abs/path/checkpoint.json"));
    }

    fn make_state(iteration: u32) -> ArcLoopState {
        ArcLoopState {
            active: true,
            checkpoint_path: ".rune/arc/arc-123/checkpoint.json".into(),
            plan_file: "plans/test.md".into(),
            config_dir: "/home/user/.claude".into(),
            owner_pid: "1234".into(),
            session_id: "sess-abc".into(),
            branch: "rune/arc-test".into(),
            iteration,
            max_iterations: 50,
        }
    }

    #[test]
    fn test_diff_no_changes() {
        let a = make_state(1);
        let b = make_state(1);
        assert!(a.diff(&b).is_empty());
    }

    #[test]
    fn test_diff_iteration_increment() {
        let a = make_state(1);
        let b = make_state(2);
        let changes = a.diff(&b);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].field, "iteration");
        assert_eq!(changes[0].old_value, "1");
        assert_eq!(changes[0].new_value, "2");
        assert!(!changes[0].anomalous);
    }

    #[test]
    fn test_diff_iteration_decrement_is_anomalous() {
        let a = make_state(5);
        let b = make_state(3);
        let changes = a.diff(&b);
        assert_eq!(changes.len(), 1);
        assert!(changes[0].anomalous);
    }

    #[test]
    fn test_diff_branch_change_is_anomalous() {
        let a = make_state(1);
        let mut b = make_state(1);
        b.branch = "rune/arc-other".into();
        let changes = a.diff(&b);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].field, "branch");
        assert!(changes[0].anomalous);
    }

    #[test]
    fn test_diff_multiple_changes() {
        let a = make_state(1);
        let mut b = make_state(3);
        b.owner_pid = "5678".into();
        let changes = a.diff(&b);
        assert_eq!(changes.len(), 2);
        let fields: Vec<&str> = changes.iter().map(|c| c.field).collect();
        assert!(fields.contains(&"iteration"));
        assert!(fields.contains(&"owner_pid"));
    }

    #[test]
    fn test_partial_eq() {
        let a = make_state(1);
        let b = make_state(1);
        assert_eq!(a, b);

        let c = make_state(2);
        assert_ne!(a, c);
    }
}
