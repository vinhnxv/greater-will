//! Styled console tags for CLI output.
//!
//! Provides consistent `[OK]`, `[FAIL]`, `[SKIP]`, etc. tags
//! using the `console` crate for colored terminal output.
//!
//! # Example
//!
//! ```ignore
//! use greater_will::output::tags::tag;
//!
//! println!("{} Plan executed successfully", tag("OK"));
//! println!("{} Phase timeout after 30m", tag("FAIL"));
//! ```

use console::Style;

/// Return a styled `[TAG]` string for the given kind.
///
/// Supported kinds: `OK`, `FAIL`, `SKIP`, `RUN`, `WARN`, `DONE`.
/// Unknown kinds render with the default (uncolored) style.
pub fn tag(kind: &str) -> String {
    let style = match kind {
        "OK" => Style::new().green().bold(),
        "FAIL" => Style::new().red().bold(),
        "SKIP" => Style::new().yellow(),
        "RUN" => Style::new().cyan().bold(),
        "WARN" => Style::new().yellow().bold(),
        "DONE" => Style::new().green().bold(),
        _ => Style::new(),
    };
    style.apply_to(format!("[{}]", kind)).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tag_contains_kind() {
        // Tags should always contain the kind text, regardless of styling
        assert!(tag("OK").contains("OK"));
        assert!(tag("FAIL").contains("FAIL"));
        assert!(tag("SKIP").contains("SKIP"));
        assert!(tag("RUN").contains("RUN"));
        assert!(tag("WARN").contains("WARN"));
        assert!(tag("DONE").contains("DONE"));
    }

    #[test]
    fn test_tag_unknown_kind() {
        let t = tag("CUSTOM");
        assert!(t.contains("CUSTOM"));
    }
}
