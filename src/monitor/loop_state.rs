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
#[derive(Debug, Clone)]
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

impl ArcLoopState {
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

/// Read and parse `arc-phase-loop.local.md` from the project directory.
///
/// Returns `None` if:
/// - File doesn't exist
/// - File can't be parsed
/// - `active` is not `true`
pub fn read_arc_loop_state(working_dir: &Path) -> Option<ArcLoopState> {
    let loop_file = working_dir.join(".rune").join("arc-phase-loop.local.md");
    let contents = std::fs::read_to_string(&loop_file).ok()?;

    let yaml = extract_frontmatter(&contents)?;

    let active = parse_yaml_bool(&yaml, "active")?;
    if !active {
        debug!("arc-phase-loop.local.md exists but active=false");
        return None;
    }

    Some(ArcLoopState {
        active,
        checkpoint_path: parse_yaml_str(&yaml, "checkpoint_path")?,
        plan_file: parse_yaml_str(&yaml, "plan_file")?,
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
    if !trimmed.starts_with("---") {
        return None;
    }
    let after_first = &trimmed[3..];
    let end = after_first.find("---")?;
    Some(after_first[..end].to_string())
}

/// Parse a simple `key: value` from YAML content.
fn parse_yaml_str(yaml: &str, key: &str) -> Option<String> {
    for line in yaml.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(key) {
            if let Some(value) = rest.strip_prefix(':') {
                let val = value.trim();
                let val = val.trim_matches('"').trim_matches('\'');
                if val.is_empty() || val == "null" {
                    return None;
                }
                return Some(val.to_string());
            }
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
        let state = read_arc_loop_state(dir.path()).unwrap();
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
    fn test_read_inactive_returns_none() {
        let dir = TempDir::new().unwrap();
        write_loop_state(dir.path(), "\
---
active: false
checkpoint_path: .rune/arc/arc-123/checkpoint.json
plan_file: plans/test.md
---
");
        assert!(read_arc_loop_state(dir.path()).is_none());
    }

    #[test]
    fn test_missing_file_returns_none() {
        let dir = TempDir::new().unwrap();
        assert!(read_arc_loop_state(dir.path()).is_none());
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
}
