//! Phase configuration schema for Rune arc phases.
//!
//! This module defines the TOML configuration structure for organizing
//! 41 Rune arc phases into 7 groups (A-G), with timeout settings and
//! skip conditions.
//!
//! # Example
//!
//! ```ignore
//! use greater_will::config::PhaseConfig;
//!
//! let config: PhaseConfig = toml::from_str(toml_str)?;
//! config.validate()?;
//! ```

use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Top-level phase configuration, loaded from TOML.
#[derive(Debug, Deserialize)]
pub struct PhaseConfig {
    pub settings: Settings,
    pub groups: Vec<PhaseGroup>,
}

/// Global settings for phase execution.
#[derive(Debug, Deserialize)]
pub struct Settings {
    /// Default timeout for phase groups (minutes).
    pub default_timeout_min: u32,
    /// Maximum retry attempts per phase.
    pub max_retries_per_phase: u32,
    /// Whether to clean up tmux sessions between phases.
    #[serde(default)]
    pub cleanup_between_phases: bool,
    /// Seconds before nudging an idle session.
    pub idle_nudge_after_sec: u64,
    /// Seconds before killing an idle session.
    pub idle_kill_after_sec: u64,
}

/// A group of related phases that run together.
#[derive(Debug, Clone, Deserialize)]
pub struct PhaseGroup {
    /// Group identifier (A-G).
    pub name: String,
    /// Human-readable label for the group.
    pub label: String,
    /// List of phase names in this group.
    pub phases: Vec<String>,
    /// The skill command to run for this group.
    pub skill_command: String,
    /// Timeout for this group (minutes).
    pub timeout_min: u32,
    /// Optional condition to skip this group.
    #[serde(default)]
    pub skip_if: Option<String>,
}

/// All 41 known Rune arc phases — used for config validation.
///
/// These are the canonical phase names recognized by the Rune arc workflow.
/// Any phase name not in this list will be rejected during validation.
pub const KNOWN_PHASES: &[&str] = &[
    // Group A: forge → verification (5 phases)
    "forge",
    "forge_qa",
    "plan_review",
    "plan_refine",
    "verification",
    // Group B: semantic → task_decomp (4 phases)
    "semantic_verification",
    "design_extraction",
    "design_prototype",
    "task_decomposition",
    // Group C: work (1 phase)
    "work",
    // Group D: work_qa → verify_inspect (13 phases)
    "work_qa",
    "drift_review",
    "storybook_verification",
    "design_verification",
    "design_verification_qa",
    "ux_verification",
    "gap_analysis",
    "gap_analysis_qa",
    "codex_gap_analysis",
    "gap_remediation",
    "inspect",
    "inspect_fix",
    "verify_inspect",
    // Group E: goldmask → verify_mend (7 phases)
    "goldmask_verification",
    "code_review",
    "code_review_qa",
    "goldmask_correlation",
    "mend",
    "mend_qa",
    "verify_mend",
    // Group F: design_iter → test (3 phases)
    "design_iteration",
    "test",
    "test_qa",
    // Group G: coverage → merge (8 phases)
    "test_coverage_critique",
    "deploy_verify",
    "pre_ship_validation",
    "release_quality_check",
    "ship",
    "bot_review_wait",
    "pr_comment_resolution",
    "merge",
];

impl PhaseConfig {
    /// Load configuration from a TOML file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or parsed.
    pub fn from_file(path: &Path) -> color_eyre::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&contents)?;
        Ok(config)
    }

    /// Validate the configuration.
    ///
    /// Checks:
    /// - All phase names are known (in KNOWN_PHASES)
    /// - No duplicate phases across groups
    /// - Warns if any known phases are not assigned to a group
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - An unknown phase name is found
    /// - A phase appears in multiple groups
    pub fn validate(&self) -> color_eyre::Result<()> {
        let mut seen: HashSet<String> = HashSet::new();
        let known: HashSet<&str> = KNOWN_PHASES.iter().copied().collect();

        // Check group names are unique
        let mut group_names: HashSet<&str> = HashSet::new();
        for group in &self.groups {
            if !group_names.insert(&group.name) {
                color_eyre::eyre::bail!("Duplicate group name: {}", group.name);
            }
        }

        // Check phases
        for group in &self.groups {
            for phase in &group.phases {
                // Check if phase is known
                if !known.contains(phase.as_str()) {
                    color_eyre::eyre::bail!(
                        "Unknown phase '{}' in group '{}'. Known phases: {:?}",
                        phase,
                        group.name,
                        KNOWN_PHASES
                    );
                }

                // Check for duplicates
                if !seen.insert(phase.clone()) {
                    color_eyre::eyre::bail!(
                        "Duplicate phase '{}' found in group '{}'",
                        phase,
                        group.name
                    );
                }
            }
        }

        // Warn about missing phases
        let missing: Vec<&str> = known
            .iter()
            .filter(|p| !seen.contains(**p))
            .copied()
            .collect();
        if !missing.is_empty() {
            tracing::warn!("Phases not assigned to any group: {:?}", missing);
        }

        // Validate settings
        if self.settings.default_timeout_min == 0 {
            color_eyre::eyre::bail!("default_timeout_min must be greater than 0");
        }
        if self.settings.idle_kill_after_sec <= self.settings.idle_nudge_after_sec {
            tracing::warn!(
                "idle_kill_after_sec ({}) should be greater than idle_nudge_after_sec ({})",
                self.settings.idle_kill_after_sec,
                self.settings.idle_nudge_after_sec
            );
        }

        Ok(())
    }

    /// Get a phase group by name.
    pub fn get_group(&self, name: &str) -> Option<&PhaseGroup> {
        self.groups.iter().find(|g| g.name == name)
    }

    /// Get the total number of phases across all groups.
    pub fn total_phases(&self) -> usize {
        self.groups.iter().map(|g| g.phases.len()).sum()
    }

    /// Get the total maximum timeout in minutes.
    pub fn total_timeout_min(&self) -> u32 {
        self.groups.iter().map(|g| g.timeout_min).sum()
    }
}

/// Resolve the configuration file path with fallback chain.
///
/// Priority:
/// 1. Explicit CLI path (`--config-dir`)
/// 2. Project-local override (`./greater-will.toml`)
/// 3. Default shipped config (`config/default-phases.toml`)
pub fn resolve_config(cli_path: Option<&Path>, cwd: &Path) -> PathBuf {
    // 1. Explicit CLI path takes precedence
    if let Some(p) = cli_path {
        return p.to_path_buf();
    }

    // 2. Check for project-local override
    let local = cwd.join("greater-will.toml");
    if local.exists() {
        return local;
    }

    // 3. Fall back to default shipped config
    cwd.join("config").join("default-phases.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_phases_count() {
        // Should have exactly 41 phases
        assert_eq!(KNOWN_PHASES.len(), 41);
    }

    #[test]
    fn test_known_phases_no_duplicates() {
        let set: HashSet<&str> = KNOWN_PHASES.iter().copied().collect();
        assert_eq!(set.len(), KNOWN_PHASES.len(), "KNOWN_PHASES contains duplicates");
    }

    #[test]
    fn test_validate_accepts_known_phases() {
        let config: PhaseConfig = toml::from_str(
            r#"
[settings]
default_timeout_min = 30
max_retries_per_phase = 3
idle_nudge_after_sec = 180
idle_kill_after_sec = 300

[[groups]]
name = "A"
label = "test"
phases = ["forge", "work"]
skill_command = "/rune:arc"
timeout_min = 30
"#,
        )
        .unwrap();

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_rejects_unknown_phase() {
        let config: PhaseConfig = toml::from_str(
            r#"
[settings]
default_timeout_min = 30
max_retries_per_phase = 3
idle_nudge_after_sec = 180
idle_kill_after_sec = 300

[[groups]]
name = "A"
label = "test"
phases = ["unknown_phase"]
skill_command = "/rune:arc"
timeout_min = 30
"#,
        )
        .unwrap();

        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Unknown phase"));
    }

    #[test]
    fn test_validate_rejects_duplicate_phase() {
        let config: PhaseConfig = toml::from_str(
            r#"
[settings]
default_timeout_min = 30
max_retries_per_phase = 3
idle_nudge_after_sec = 180
idle_kill_after_sec = 300

[[groups]]
name = "A"
label = "test"
phases = ["forge"]
skill_command = "/rune:arc"
timeout_min = 30

[[groups]]
name = "B"
label = "test2"
phases = ["forge"]
skill_command = "/rune:arc"
timeout_min = 30
"#,
        )
        .unwrap();

        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Duplicate phase"));
    }

    #[test]
    fn test_resolve_config_fallback() {
        let cwd = Path::new("/tmp/test");

        // No CLI path, no local file -> default
        let resolved = resolve_config(None, cwd);
        assert_eq!(resolved, cwd.join("config/default-phases.toml"));

        // CLI path takes precedence
        let cli_path = Path::new("/custom/config.toml");
        let resolved = resolve_config(Some(cli_path), cwd);
        assert_eq!(resolved, cli_path);
    }

    #[test]
    fn test_total_phases() {
        let config: PhaseConfig = toml::from_str(
            r#"
[settings]
default_timeout_min = 30
max_retries_per_phase = 3
idle_nudge_after_sec = 180
idle_kill_after_sec = 300

[[groups]]
name = "A"
label = "test"
phases = ["forge", "work"]
skill_command = "/rune:arc"
timeout_min = 30

[[groups]]
name = "B"
label = "test2"
phases = ["test", "test_qa"]
skill_command = "/rune:arc"
timeout_min = 30
"#,
        )
        .unwrap();

        assert_eq!(config.total_phases(), 4);
    }

    #[test]
    fn test_default_config_loads_and_validates() {
        // Load the shipped default config
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
        let config_path = PathBuf::from(manifest_dir)
            .join("config")
            .join("default-phases.toml");

        // Verify file exists
        assert!(config_path.exists(), "Default config file not found at {:?}", config_path);

        // Load and parse
        let config = PhaseConfig::from_file(&config_path)
            .expect("Failed to parse default config");

        // Validate
        config.validate().expect("Default config validation failed");

        // Verify structure
        assert_eq!(config.groups.len(), 7, "Expected 7 groups");
        assert_eq!(config.total_phases(), 41, "Expected 41 phases across all groups");

        // Verify each group has expected phases
        let group_a = config.get_group("A").expect("Group A not found");
        assert_eq!(group_a.phases.len(), 5);

        let group_c = config.get_group("C").expect("Group C not found");
        assert_eq!(group_c.phases.len(), 1);
        assert_eq!(group_c.phases[0], "work");
        assert_eq!(group_c.timeout_min, 90); // work has longer timeout
    }
}