//! Scanner module for config directory discovery and plan file scanning.
//!
//! Adapted from Torrent's scanner.rs with Torrent-specific TUI code removed.
//!
//! ## Functionality
//! - `scan_config_dirs`: Discover Claude config directories (~/.claude, ~/.claude-*, env vars)
//! - `scan_plans`: Parse plan files with YAML frontmatter metadata extraction

use color_eyre::{eyre::eyre, Result};
use glob::glob;
use std::fs;
use std::path::{Path, PathBuf};

/// A discovered Claude config directory (e.g. ~/.claude or ~/.claude-work).
#[derive(Debug, Clone)]
pub struct ConfigDir {
    pub path: PathBuf,
    pub label: String,
}

/// A plan file discovered in plans/*.md.
#[derive(Debug, Clone)]
pub struct PlanFile {
    pub path: PathBuf,
    pub name: String,
    pub title: String,
    /// Date extracted from frontmatter `date:` or filename pattern (YYYY-MM-DD).
    pub date: Option<String>,
}

/// Scan $HOME for Claude config directories, plus any extra custom paths.
///
/// Auto-discovered:
/// - ~/.claude/ (default)
/// - ~/.claude-*/ (custom accounts)
/// - $CLAUDE_CONFIG_DIR (if set and valid)
///
/// Extra paths from `--config-dir` / `-c` CLI arguments are also included.
///
/// Filters: must be a directory containing settings.json or projects/ subdirectory.
/// Deduplicates by canonical path.
pub fn scan_config_dirs(extra_dirs: &[PathBuf]) -> Result<Vec<ConfigDir>> {
    let home = dirs::home_dir().ok_or_else(|| eyre!("cannot resolve home directory"))?;
    let mut configs = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Helper: add a config dir if valid and not already seen.
    let mut try_add = |path: PathBuf, label: String| {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        if is_valid_config_dir(&path) && seen.insert(canonical) {
            configs.push(ConfigDir { path, label });
        }
    };

    // 1. Check ~/.claude/ (default)
    let default_dir = home.join(".claude");
    try_add(default_dir, ".claude (default)".into());

    // 2. Check ~/.claude-*/ (custom accounts)
    let home_str = home.to_string_lossy();
    let pattern = format!("{}/.claude-*/", home_str);
    for path in (glob(&pattern).map_err(|e| eyre!("invalid glob pattern: {e}"))?).flatten() {
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        try_add(path, label);
    }

    // 3. Check $CLAUDE_CONFIG_DIR env var
    if let Ok(env_dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !env_dir.is_empty() {
            let path = resolve_path(&env_dir, &home);
            let label = format!(
                "{} (env)",
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| env_dir.clone())
            );
            try_add(path, label);
        }
    }

    // 4. Extra dirs from --config-dir / -c CLI arguments
    for extra in extra_dirs {
        let path = if extra.is_absolute() {
            extra.clone()
        } else {
            resolve_path(&extra.to_string_lossy(), &home)
        };
        let label = format!(
            "{} (custom)",
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
        );
        try_add(path, label);
    }

    Ok(configs)
}

/// Resolve a path string, expanding `~` to the home directory.
fn resolve_path(raw: &str, home: &Path) -> PathBuf {
    if raw.starts_with('~') {
        PathBuf::from(raw.replacen('~', &home.to_string_lossy(), 1))
    } else {
        PathBuf::from(raw)
    }
}

/// A valid config dir must contain settings.json or a projects/ subdirectory.
fn is_valid_config_dir(path: &Path) -> bool {
    path.is_dir() && (path.join("settings.json").exists() || path.join("projects").is_dir())
}

/// Scan plans/*.md in the given directory (non-recursive).
pub fn scan_plans(cwd: &Path) -> Result<Vec<PlanFile>> {
    let pattern = cwd.join("plans").join("*.md");
    let pattern_str = pattern.to_string_lossy();
    let mut plans = Vec::new();

    for entry in glob(&pattern_str).map_err(|e| eyre!("invalid glob pattern: {e}"))? {
        match entry {
            Ok(path) if path.is_file() => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let (title, fm_date) = extract_plan_metadata(&path);
                let title = title.unwrap_or_else(|| name.clone());
                let date = fm_date.or_else(|| extract_date_from_filename(&name));
                plans.push(PlanFile {
                    path,
                    name,
                    title,
                    date,
                });
            }
            _ => {}
        }
    }

    plans.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(plans)
}

/// Extract plan title and date from a markdown file with YAML frontmatter.
///
/// Priority for title:
/// 1. First markdown `# heading` OUTSIDE frontmatter and fenced code blocks
/// 2. `title:` field from YAML frontmatter
/// 3. None (caller falls back to filename)
///
/// Returns (title, date) where date comes from frontmatter `date:` field.
fn extract_plan_metadata(path: &Path) -> (Option<String>, Option<String>) {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (None, None),
    };

    let mut fm_title: Option<String> = None;
    let mut fm_date: Option<String> = None;
    let mut in_frontmatter = false;
    let mut frontmatter_done = false;
    let mut in_code_block = false;
    let mut first_line = true;

    for line in content.lines() {
        let trimmed = line.trim();

        // Detect YAML frontmatter (must start on line 1)
        if first_line && trimmed == "---" {
            in_frontmatter = true;
            first_line = false;
            continue;
        }
        first_line = false;

        // End of frontmatter
        if in_frontmatter && trimmed == "---" {
            in_frontmatter = false;
            frontmatter_done = true;
            continue;
        }

        // Extract title/date from frontmatter
        if in_frontmatter {
            if let Some(val) = trimmed.strip_prefix("title:") {
                fm_title = Some(val.trim().trim_matches('"').trim_matches('\'').to_string());
            } else if let Some(val) = trimmed.strip_prefix("date:") {
                fm_date = Some(val.trim().trim_matches('"').trim_matches('\'').to_string());
            }
            continue;
        }

        // Skip content before frontmatter closes (shouldn't happen but guard)
        if !frontmatter_done && !in_frontmatter {
            // No frontmatter in this file — proceed normally
            frontmatter_done = true;
        }

        // Track fenced code blocks (``` or ~~~)
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_block = !in_code_block;
            continue;
        }

        // Only look for headings outside code blocks
        if !in_code_block {
            if let Some(heading) = trimmed.strip_prefix("# ") {
                let heading = heading.trim().to_string();
                if !heading.is_empty() {
                    return (Some(heading), fm_date);
                }
            }
        }
    }

    // No markdown heading found — fall back to frontmatter title
    (fm_title, fm_date)
}

/// Extract date (YYYY-MM-DD) from a plan filename like `2026-03-18-feat-xxx-plan.md`.
fn extract_date_from_filename(name: &str) -> Option<String> {
    if name.len() >= 10 {
        let prefix = &name[..10];
        // Validate pattern: DDDD-DD-DD
        let bytes = prefix.as_bytes();
        if bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes[..4].iter().all(|b| b.is_ascii_digit())
            && bytes[5..7].iter().all(|b| b.is_ascii_digit())
            && bytes[8..10].iter().all(|b| b.is_ascii_digit())
        {
            return Some(prefix.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ── is_valid_config_dir ─────────────────────────────────

    #[test]
    fn test_valid_config_dir_with_settings() {
        let dir = std::env::temp_dir().join("gw-test-config-valid-settings");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("settings.json"), "{}").unwrap();

        assert!(is_valid_config_dir(&dir));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_valid_config_dir_with_projects() {
        let dir = std::env::temp_dir().join("gw-test-config-valid-projects");
        fs::create_dir_all(dir.join("projects")).unwrap();

        assert!(is_valid_config_dir(&dir));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_invalid_config_dir_empty() {
        let dir = std::env::temp_dir().join("gw-test-config-invalid-empty");
        fs::create_dir_all(&dir).unwrap();

        assert!(!is_valid_config_dir(&dir));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_invalid_config_dir_nonexistent() {
        let dir = std::env::temp_dir().join("gw-test-config-nonexistent-xyz");
        assert!(!is_valid_config_dir(&dir));
    }

    // ── extract_date_from_filename ──────────────────────────

    #[test]
    fn test_date_extraction_standard() {
        assert_eq!(
            extract_date_from_filename("2026-03-18-feat-auth-plan.md"),
            Some("2026-03-18".into())
        );
    }

    #[test]
    fn test_date_extraction_short_name() {
        assert_eq!(
            extract_date_from_filename("2026-03-18.md"),
            Some("2026-03-18".into())
        );
    }

    #[test]
    fn test_date_extraction_no_date() {
        assert_eq!(extract_date_from_filename("feat-auth-plan.md"), None);
    }

    #[test]
    fn test_date_extraction_too_short() {
        assert_eq!(extract_date_from_filename("2026.md"), None);
    }

    #[test]
    fn test_date_extraction_invalid_separators() {
        assert_eq!(extract_date_from_filename("2026_03_18-plan.md"), None);
    }

    // ── extract_plan_metadata ───────────────────────────────

    #[test]
    fn test_metadata_heading_over_frontmatter_title() {
        let dir = std::env::temp_dir().join("gw-test-meta-heading");
        fs::create_dir_all(&dir).unwrap();
        let plan = dir.join("plan.md");
        fs::write(
            &plan,
            "---\ntitle: FM Title\ndate: 2026-03-18\n---\n# Real Heading\n",
        )
        .unwrap();

        let (title, date) = extract_plan_metadata(&plan);
        assert_eq!(title, Some("Real Heading".into()));
        assert_eq!(date, Some("2026-03-18".into()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_metadata_frontmatter_only() {
        let dir = std::env::temp_dir().join("gw-test-meta-fm-only");
        fs::create_dir_all(&dir).unwrap();
        let plan = dir.join("plan.md");
        fs::write(&plan, "---\ntitle: FM Title\n---\nNo heading here.\n").unwrap();

        let (title, _) = extract_plan_metadata(&plan);
        assert_eq!(title, Some("FM Title".into()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_metadata_no_frontmatter() {
        let dir = std::env::temp_dir().join("gw-test-meta-no-fm");
        fs::create_dir_all(&dir).unwrap();
        let plan = dir.join("plan.md");
        fs::write(&plan, "# Just a Heading\nSome content.\n").unwrap();

        let (title, date) = extract_plan_metadata(&plan);
        assert_eq!(title, Some("Just a Heading".into()));
        assert_eq!(date, None);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_metadata_heading_in_code_block_ignored() {
        let dir = std::env::temp_dir().join("gw-test-meta-codeblock");
        fs::create_dir_all(&dir).unwrap();
        let plan = dir.join("plan.md");
        fs::write(
            &plan,
            "---\ntitle: FM Title\n---\n```\n# Fake Heading\n```\n# Real Heading\n",
        )
        .unwrap();

        let (title, _) = extract_plan_metadata(&plan);
        assert_eq!(title, Some("Real Heading".into()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_metadata_quoted_title() {
        let dir = std::env::temp_dir().join("gw-test-meta-quoted");
        fs::create_dir_all(&dir).unwrap();
        let plan = dir.join("plan.md");
        fs::write(&plan, "---\ntitle: \"Quoted Title\"\n---\nNo heading.\n").unwrap();

        let (title, _) = extract_plan_metadata(&plan);
        assert_eq!(title, Some("Quoted Title".into()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_metadata_nonexistent_file() {
        let (title, date) = extract_plan_metadata(Path::new("/nonexistent/plan.md"));
        assert_eq!(title, None);
        assert_eq!(date, None);
    }

    // ── scan_plans ──────────────────────────────────────────

    #[test]
    fn test_scan_plans_finds_md_files() {
        let dir = std::env::temp_dir().join("gw-test-scan-plans");
        let plans_dir = dir.join("plans");
        fs::create_dir_all(&plans_dir).unwrap();

        fs::write(
            plans_dir.join("2026-03-18-feat-auth-plan.md"),
            "# Auth Plan\n",
        )
        .unwrap();
        fs::write(
            plans_dir.join("2026-03-17-fix-bug-plan.md"),
            "# Bug Fix\n",
        )
        .unwrap();
        fs::write(plans_dir.join("not-a-plan.txt"), "ignored").unwrap();

        let plans = scan_plans(&dir).unwrap();
        assert_eq!(plans.len(), 2);
        // Sorted by name
        assert_eq!(plans[0].name, "2026-03-17-fix-bug-plan.md");
        assert_eq!(plans[1].name, "2026-03-18-feat-auth-plan.md");
        assert_eq!(plans[0].date, Some("2026-03-17".into()));
        assert_eq!(plans[1].title, "Auth Plan");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_plans_empty_dir() {
        let dir = std::env::temp_dir().join("gw-test-scan-empty");
        fs::create_dir_all(dir.join("plans")).unwrap();

        let plans = scan_plans(&dir).unwrap();
        assert!(plans.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    // ── resolve_path ───────────────────────────────────────────

    #[test]
    fn test_resolve_path_absolute() {
        let home = Path::new("/Users/test");
        let result = resolve_path("/opt/claude-ci", home);
        assert_eq!(result, PathBuf::from("/opt/claude-ci"));
    }

    #[test]
    fn test_resolve_path_tilde() {
        let home = Path::new("/Users/test");
        let result = resolve_path("~/.claude-work", home);
        assert_eq!(result, PathBuf::from("/Users/test/.claude-work"));
    }

    #[test]
    fn test_resolve_path_relative() {
        let home = Path::new("/Users/test");
        let result = resolve_path("configs/claude", home);
        assert_eq!(result, PathBuf::from("configs/claude"));
    }

    // ── scan_config_dirs with extra dirs ────────────────────────

    #[test]
    fn test_scan_config_dirs_with_extra_valid() {
        let dir = std::env::temp_dir().join("gw-test-extra-config-valid");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("settings.json"), "{}").unwrap();

        let configs = scan_config_dirs(&[dir.clone()]).unwrap();
        let found = configs
            .iter()
            .any(|c| c.path == dir && c.label.contains("(custom)"));
        assert!(found, "extra config dir should appear with (custom) label");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_config_dirs_with_extra_invalid() {
        let dir = std::env::temp_dir().join("gw-test-extra-config-invalid-xyz");
        // Don't create it — it's invalid

        let configs = scan_config_dirs(&[dir.clone()]).unwrap();
        let found = configs.iter().any(|c| c.path == dir);
        assert!(!found, "invalid extra dir should be silently skipped");
    }

    #[test]
    fn test_scan_config_dirs_deduplicates() {
        let dir = std::env::temp_dir().join("gw-test-extra-dedup");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("settings.json"), "{}").unwrap();

        // Pass the same dir twice
        let configs = scan_config_dirs(&[dir.clone(), dir.clone()]).unwrap();
        let count = configs.iter().filter(|c| c.path == dir).count();
        assert_eq!(count, 1, "duplicate extra dirs should be deduplicated");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_config_dirs_empty_extras() {
        // No extra dirs — should still work (backward compatible)
        let configs = scan_config_dirs(&[]).unwrap();
        // Just verify it doesn't crash; actual dirs depend on the host
        let _ = configs; // verify no crash; actual dirs depend on the host
    }
}