//! GitHub issue integration for `gw run`.
//!
//! Enables `gw run https://github.com/owner/repo/issues/123` by:
//! 1. Detecting GitHub issue URLs in the plan arguments
//! 2. Fetching issue details via the `gh` CLI
//! 3. Generating a plan file from the issue content
//! 4. Returning the generated plan path for the existing pipeline

use color_eyre::eyre::{self, Context};
use color_eyre::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Parsed GitHub issue URL components.
#[derive(Debug, Clone, PartialEq)]
pub struct GitHubIssue {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl GitHubIssue {
    /// Canonical `owner/repo#number` format.
    pub fn short_ref(&self) -> String {
        format!("{}/{}#{}", self.owner, self.repo, self.number)
    }
}

/// Check whether a string looks like a GitHub issue URL.
///
/// Accepts both HTTPS URLs and `owner/repo#123` shorthand:
/// - `https://github.com/owner/repo/issues/123`
/// - `http://github.com/owner/repo/issues/123`
/// - `owner/repo#123`
pub fn is_github_issue_url(input: &str) -> bool {
    parse_github_issue_url(input).is_some()
}

/// Parse a GitHub issue URL or shorthand into its components.
///
/// Returns `None` if the input doesn't match any recognized format.
pub fn parse_github_issue_url(input: &str) -> Option<GitHubIssue> {
    let trimmed = input.trim();

    // Try URL format: https://github.com/owner/repo/issues/123
    if let Some(rest) = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
    {
        let parts: Vec<&str> = rest.split('/').collect();
        // Expected: [owner, repo, "issues", number]
        if parts.len() >= 4 && parts[2] == "issues" {
            let number = parts[3].parse::<u64>().ok()?;
            if !parts[0].is_empty() && !parts[1].is_empty() {
                return Some(GitHubIssue {
                    owner: parts[0].to_string(),
                    repo: parts[1].to_string(),
                    number,
                });
            }
        }
        return None;
    }

    // Try shorthand format: owner/repo#123
    if let Some((repo_part, num_str)) = trimmed.split_once('#') {
        if let Some((owner, repo)) = repo_part.split_once('/') {
            let number = num_str.parse::<u64>().ok()?;
            if !owner.is_empty() && !repo.is_empty() && !owner.contains('/') {
                return Some(GitHubIssue {
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                    number,
                });
            }
        }
    }

    None
}

/// Fetched issue data from GitHub.
#[derive(Debug, Clone)]
pub struct IssueData {
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub url: String,
}

/// Fetch issue details using the `gh` CLI.
///
/// Requires `gh` to be installed and authenticated.
pub fn fetch_issue(issue: &GitHubIssue) -> Result<IssueData> {
    // Verify gh is available
    preflight_gh()?;

    let repo_slug = format!("{}/{}", issue.owner, issue.repo);

    let output = Command::new("gh")
        .args([
            "issue",
            "view",
            &issue.number.to_string(),
            "--repo",
            &repo_slug,
            "--json",
            "title,body,labels,url",
        ])
        .output()
        .wrap_err("Failed to execute `gh issue view`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eyre::bail!(
            "Failed to fetch issue {}: {}",
            issue.short_ref(),
            stderr.trim()
        );
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .wrap_err("Failed to parse `gh` JSON output")?;

    let title = json["title"]
        .as_str()
        .unwrap_or("Untitled")
        .to_string();

    let body = json["body"]
        .as_str()
        .unwrap_or("")
        .to_string();

    let labels = json["labels"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v["name"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let url = json["url"]
        .as_str()
        .unwrap_or("")
        .to_string();

    Ok(IssueData {
        title,
        body,
        labels,
        url,
    })
}

/// Generate a plan file from a GitHub issue.
///
/// Creates `plans/<date>-issue-<number>-<slug>.md` in the working directory.
/// Returns the path to the generated plan file.
pub fn generate_plan_from_issue(
    issue: &GitHubIssue,
    data: &IssueData,
    working_dir: &Path,
) -> Result<PathBuf> {
    let plans_dir = working_dir.join("plans");
    std::fs::create_dir_all(&plans_dir)
        .wrap_err_with(|| format!("Failed to create plans directory: {}", plans_dir.display()))?;

    // Build filename: plans/YYYY-MM-DD-issue-123-slug.md
    let date = chrono::Local::now().format("%Y-%m-%d");
    let slug = slugify(&data.title, 50);
    let filename = format!("{}-issue-{}-{}.md", date, issue.number, slug);
    let plan_path = plans_dir.join(&filename);

    // Don't overwrite if it already exists (idempotent re-runs)
    if plan_path.exists() {
        tracing::info!(path = %plan_path.display(), "Plan file already exists, reusing");
        return Ok(plan_path);
    }

    let content = format_plan(&data.title, &data.body, &data.labels, &data.url, issue);

    std::fs::write(&plan_path, &content)
        .wrap_err_with(|| format!("Failed to write plan file: {}", plan_path.display()))?;

    tracing::info!(path = %plan_path.display(), "Generated plan from issue {}", issue.short_ref());

    Ok(plan_path)
}

/// Resolve a list of plan arguments, converting any GitHub issue URLs
/// into generated plan file paths.
///
/// Non-URL arguments are passed through unchanged.
pub fn resolve_github_urls(
    plans: &[String],
    working_dir: &Path,
) -> Result<Vec<String>> {
    let mut resolved = Vec::with_capacity(plans.len());

    for plan in plans {
        if let Some(issue) = parse_github_issue_url(plan) {
            println!("[gw] Detected GitHub issue: {}", issue.short_ref());
            println!("[gw] Fetching issue details...");

            let data = fetch_issue(&issue)
                .wrap_err_with(|| format!("Failed to fetch {}", issue.short_ref()))?;

            println!("[gw] Issue: {}", data.title);

            let plan_path = generate_plan_from_issue(&issue, &data, working_dir)?;
            println!("[gw] Generated plan: {}", plan_path.display());

            resolved.push(plan_path.to_string_lossy().to_string());
        } else {
            resolved.push(plan.clone());
        }
    }

    Ok(resolved)
}

// --- Internal helpers ---

/// Check that `gh` CLI is available and authenticated.
fn preflight_gh() -> Result<()> {
    let check = Command::new("gh").args(["auth", "status"]).output();

    match check {
        Err(_) => {
            eyre::bail!(
                "`gh` CLI is required but not found.\n\
                 Install: https://cli.github.com/\n\
                 Then run: gh auth login"
            );
        }
        Ok(out) if !out.status.success() => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            eyre::bail!(
                "`gh` CLI is not authenticated.\n\
                 Run: gh auth login\n\
                 Details: {}",
                stderr.trim()
            );
        }
        Ok(_) => Ok(()),
    }
}

/// Convert a title into a URL-safe slug, truncated to `max_len`.
fn slugify(title: &str, max_len: usize) -> String {
    let slug: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();

    // Collapse repeated dashes and trim
    let mut result = String::new();
    let mut prev_dash = false;
    for c in slug.chars() {
        if c == '-' {
            if !prev_dash && !result.is_empty() {
                result.push('-');
            }
            prev_dash = true;
        } else {
            result.push(c);
            prev_dash = false;
        }
    }

    // Trim trailing dash
    let result = result.trim_end_matches('-').to_string();

    if result.len() > max_len {
        // Cut at last dash before max_len to avoid mid-word cuts
        match result[..max_len].rfind('-') {
            Some(pos) => result[..pos].to_string(),
            None => result[..max_len].to_string(),
        }
    } else {
        result
    }
}

/// Format the plan markdown content from issue data.
fn format_plan(
    title: &str,
    body: &str,
    labels: &[String],
    url: &str,
    issue: &GitHubIssue,
) -> String {
    let labels_str = if labels.is_empty() {
        String::new()
    } else {
        format!("\nlabels: [{}]", labels.iter().map(|l| format!("\"{}\"", l)).collect::<Vec<_>>().join(", "))
    };

    format!(
        r#"---
title: "{title}"
source: github-issue
issue: "{short_ref}"
url: "{url}"{labels_str}
---

# {title}

> Source: [{short_ref}]({url})

## Description

{body}

## Acceptance Criteria

<!-- Extracted from the issue. Review and adjust before running. -->

- [ ] Implementation satisfies the issue requirements
- [ ] Tests cover the new behavior
- [ ] No regressions introduced
"#,
        title = title.replace('"', r#"\""#),
        short_ref = issue.short_ref(),
        url = url,
        labels_str = labels_str,
        body = if body.is_empty() { "_No description provided._" } else { body },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_https_url() {
        let url = "https://github.com/True-Digital-Vietnam/thechoice/issues/717";
        let issue = parse_github_issue_url(url).unwrap();
        assert_eq!(issue.owner, "True-Digital-Vietnam");
        assert_eq!(issue.repo, "thechoice");
        assert_eq!(issue.number, 717);
    }

    #[test]
    fn test_parse_http_url() {
        let url = "http://github.com/owner/repo/issues/42";
        let issue = parse_github_issue_url(url).unwrap();
        assert_eq!(issue.owner, "owner");
        assert_eq!(issue.repo, "repo");
        assert_eq!(issue.number, 42);
    }

    #[test]
    fn test_parse_shorthand() {
        let input = "owner/repo#123";
        let issue = parse_github_issue_url(input).unwrap();
        assert_eq!(issue.owner, "owner");
        assert_eq!(issue.repo, "repo");
        assert_eq!(issue.number, 123);
    }

    #[test]
    fn test_parse_not_issue_url() {
        assert!(parse_github_issue_url("plans/my-plan.md").is_none());
        assert!(parse_github_issue_url("https://github.com/owner/repo").is_none());
        assert!(parse_github_issue_url("https://github.com/owner/repo/pull/1").is_none());
        assert!(parse_github_issue_url("random string").is_none());
    }

    #[test]
    fn test_parse_url_with_trailing_content() {
        // URLs may have trailing slashes or query params — we only need the number
        let url = "https://github.com/org/repo/issues/99";
        let issue = parse_github_issue_url(url).unwrap();
        assert_eq!(issue.number, 99);
    }

    #[test]
    fn test_short_ref() {
        let issue = GitHubIssue {
            owner: "org".into(),
            repo: "project".into(),
            number: 42,
        };
        assert_eq!(issue.short_ref(), "org/project#42");
    }

    #[test]
    fn test_slugify_basic() {
        assert_eq!(slugify("Hello World!", 50), "hello-world");
        assert_eq!(slugify("Fix: bug in auth---flow", 50), "fix-bug-in-auth-flow");
    }

    #[test]
    fn test_slugify_truncate() {
        let long = "this-is-a-really-long-title-that-should-be-truncated-somewhere";
        let result = slugify(long, 30);
        assert!(result.len() <= 30);
        assert!(!result.ends_with('-'));
    }

    #[test]
    fn test_is_github_issue_url() {
        assert!(is_github_issue_url("https://github.com/o/r/issues/1"));
        assert!(is_github_issue_url("o/r#1"));
        assert!(!is_github_issue_url("plans/test.md"));
        assert!(!is_github_issue_url("https://example.com"));
    }

    #[test]
    fn test_format_plan_output() {
        let issue = GitHubIssue {
            owner: "org".into(),
            repo: "repo".into(),
            number: 42,
        };
        let content = format_plan(
            "Fix login bug",
            "Users can't login when...",
            &["bug".into(), "auth".into()],
            "https://github.com/org/repo/issues/42",
            &issue,
        );
        assert!(content.contains("title: \"Fix login bug\""));
        assert!(content.contains("source: github-issue"));
        assert!(content.contains("org/repo#42"));
        assert!(content.contains("Users can't login when..."));
        assert!(content.contains("labels: [\"bug\", \"auth\"]"));
    }

    #[test]
    fn test_format_plan_empty_body() {
        let issue = GitHubIssue {
            owner: "o".into(),
            repo: "r".into(),
            number: 1,
        };
        let content = format_plan("Title", "", &[], "", &issue);
        assert!(content.contains("_No description provided._"));
    }
}
