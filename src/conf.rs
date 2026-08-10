//! Shell-style `KEY=value` config files, and the file I/O every config write shares.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result};

/// Reads one key out of shell-style `KEY=value` text.
pub fn parse_value(text: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");

    text.lines()
        .filter_map(|line| line.trim().strip_prefix(&prefix))
        .map(|value| strip_quotes(value.trim()).to_string())
        .next_back()
}

fn strip_quotes(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// Replaces every `KEY=` line, appending if there was none.
pub fn upsert_key(existing: Option<&str>, key: &str, value: &str) -> String {
    let assignment = format!("{key}={value}");

    let Some(existing) = existing else {
        return format!("{assignment}\n");
    };

    let prefix = format!("{key}=");
    let mut out = String::with_capacity(existing.len() + assignment.len() + 1);
    let mut replaced = false;

    for line in existing.lines() {
        if line.starts_with(&prefix) {
            out.push_str(&assignment);
            replaced = true;
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }

    if !replaced {
        out.push_str(&assignment);
        out.push('\n');
    }

    out
}

pub fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes only if the content would actually change. Returns whether it wrote.
pub fn write_if_changed(path: &Path, contents: &str) -> Result<bool> {
    if read_optional(path)?.as_deref() == Some(contents) {
        return Ok(false);
    }

    write_atomically(path, contents)?;
    Ok(true)
}

/// Write-to-temp-then-rename, so a reader never sees a half-written config and a crash mid-write
/// cannot leave an empty `robot.conf` behind.
pub fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;

    fs::create_dir_all(parent)
        .map_err(|err| annotate_permission(err, parent))
        .with_context(|| format!("creating {}", parent.display()))?;

    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));

    fs::write(&tmp, contents)
        .map_err(|err| annotate_permission(err, &tmp))
        .with_context(|| format!("writing {}", tmp.display()))?;

    fs::rename(&tmp, path).with_context(|| {
        let _ = fs::remove_file(&tmp);
        format!("replacing {}", path.display())
    })?;

    Ok(())
}

fn annotate_permission(err: std::io::Error, path: &Path) -> anyhow::Error {
    if err.kind() == ErrorKind::PermissionDenied {
        anyhow::anyhow!(
            "permission denied writing {} - this command must be run as root",
            path.display()
        )
    } else {
        err.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_last_assignment_and_strips_quotes() {
        let text = "# a comment\nROBOT_ID=first\nOTHER=x\nROBOT_ID=\"second\"\n";

        assert_eq!(parse_value(text, "ROBOT_ID").as_deref(), Some("second"));
        assert_eq!(parse_value(text, "OTHER").as_deref(), Some("x"));
        assert_eq!(parse_value(text, "MISSING"), None);
    }

    #[test]
    fn parses_single_quoted_and_indented_values() {
        let text = "  AP_PSK='hunter2hunter2'\n";

        assert_eq!(
            parse_value(text, "AP_PSK").as_deref(),
            Some("hunter2hunter2")
        );
    }

    #[test]
    fn parses_an_empty_value_as_empty_not_missing() {
        // Load-bearing for the fallback AP: AP_PSK="" means "open network", which must be
        // distinguishable from AP_PSK being absent entirely.
        assert_eq!(parse_value("AP_PSK=\"\"\n", "AP_PSK").as_deref(), Some(""));
        assert_eq!(parse_value("AP_PSK=\n", "AP_PSK").as_deref(), Some(""));
    }

    #[test]
    fn upsert_preserves_comments_and_unrelated_keys() {
        let existing = "# identity\nROBOT_ID=old\nROS_DOMAIN_ID=7\n";

        assert_eq!(
            upsert_key(Some(existing), "ROBOT_ID", "new"),
            "# identity\nROBOT_ID=new\nROS_DOMAIN_ID=7\n"
        );
    }

    #[test]
    fn upsert_appends_a_missing_key_and_creates_a_missing_file() {
        assert_eq!(
            upsert_key(Some("ROBOT_ID=x\n"), "ROS_DOMAIN_ID", "3"),
            "ROBOT_ID=x\nROS_DOMAIN_ID=3\n"
        );
        assert_eq!(upsert_key(None, "ROBOT_ID", "x"), "ROBOT_ID=x\n");
    }
}
