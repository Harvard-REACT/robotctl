//! Experiment stacks: `docker compose` against `/data/experiments/<name>/docker-compose.yml`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use thiserror::Error;

use crate::conf;
use crate::config::Paths;
use crate::log;

const ENABLED_CONF_TEMPLATE: &str = "\
# One experiment name per line = enabled. Lines starting with # and blank
# lines are ignored. Each name must have a matching directory:
#   <experiments-dir>/<name>/docker-compose.yml
#
# Example:
# my-experiment
";

/// Written when `experiments.conf` doesn't exist, so the setting is discoverable on the robot
/// rather than being a flag someone has to know about.
const SETTINGS_TEMPLATE: &str = "\
# Settings for `robotctl experiments`, in shell-style KEY=value form.
#
# IGNORE_PULL_FAILURES: what to do when `docker compose pull` fails for an
# experiment. The default, false, refuses to start that stack -- `compose up`
# would otherwise silently run whatever stale image is already on this robot,
# so results would be attributed to code that never ran here.
#
# Set this to true on robots that operate on RF-isolated networks, where the
# registry is unreachable by design and images are side-loaded over ssh. There
# the local image is the correct one and a failed pull means nothing.
#
# `robotctl experiments start --ignore-pull-failures[=true|false]` overrides
# this for a single run.
IGNORE_PULL_FAILURES=false
";

const IGNORE_PULL_FAILURES_KEY: &str = "IGNORE_PULL_FAILURES";

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ExperimentNameError {
    #[error("must not be empty")]
    Empty,

    #[error("must not contain a path separator")]
    PathSeparator,

    #[error("must not be a relative path component ('.' or '..')")]
    RelativeComponent,

    #[error("must not start with '.'")]
    HiddenName,

    #[error("may only contain letters, digits, '.', '_' and '-' (found '{found}')")]
    InvalidCharacter { found: char },
}

/// A validated experiment name — safe to join onto a directory path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExperimentName(String);

impl ExperimentName {
    pub fn new(name: &str) -> Result<Self, ExperimentNameError> {
        if name.is_empty() {
            return Err(ExperimentNameError::Empty);
        }

        if name.contains('/') || name.contains('\\') {
            return Err(ExperimentNameError::PathSeparator);
        }

        if name == "." || name == ".." {
            return Err(ExperimentNameError::RelativeComponent);
        }

        // A leading dot would let an entry address `.ssh`-style hidden directories, and reads
        // as a mistake in a file that is meant to list experiment names.
        if name.starts_with('.') {
            return Err(ExperimentNameError::HiddenName);
        }

        if let Some(found) = name
            .chars()
            .find(|c| !c.is_ascii_alphanumeric() && !matches!(c, '.' | '_' | '-'))
        {
            return Err(ExperimentNameError::InvalidCharacter { found });
        }

        Ok(ExperimentName(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExperimentName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The result of reading `enabled.conf`: the names to act on, and everything rejected on the way.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EnabledList {
    pub names: Vec<ExperimentName>,
    pub warnings: Vec<String>,
}

/// Parses `enabled.conf`: one name per line, `#` comments and blank lines ignored.
///
/// Duplicates are dropped with a warning.
pub fn parse_enabled(text: &str) -> EnabledList {
    let mut list = EnabledList::default();

    for (number, line) in text.lines().enumerate() {
        let line = line.trim();

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        match ExperimentName::new(line) {
            Err(err) => list
                .warnings
                .push(format!("line {}: '{line}' {err}", number + 1)),

            Ok(name) if list.names.contains(&name) => list.warnings.push(format!(
                "line {}: '{name}' is listed more than once",
                number + 1
            )),

            Ok(name) => list.names.push(name),
        }
    }

    list
}

/// How `start` treats a failed `docker compose pull`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StartOptions {
    /// A one-run override of `IGNORE_PULL_FAILURES` in `experiments.conf`.
    ///
    /// `None` means "whatever the file says", which is the normal case — the persistent setting
    /// belongs on the robot, since whether its registry is reachable is a property of where that
    /// robot is deployed, not of who is invoking the command.
    pub ignore_pull_failures: Option<bool>,
}

/// Reads `IGNORE_PULL_FAILURES` from `experiments.conf`, defaulting to false (a failed pull is
/// fatal). An unparseable value warns and takes the default rather than guessing.
fn ignore_pull_failures(settings_path: &Path) -> Result<bool> {
    let Some(text) = conf::read_optional(settings_path)? else {
        return Ok(false);
    };

    let Some(raw) = conf::parse_value(&text, IGNORE_PULL_FAILURES_KEY) else {
        return Ok(false);
    };

    Ok(parse_bool(&raw).unwrap_or_else(|| {
        log::warn(format!(
            "{}: {IGNORE_PULL_FAILURES_KEY}='{raw}' is not a boolean; treating a failed pull as fatal",
            settings_path.display()
        ));
        false
    }))
}

/// Accepts the spellings people actually write in a hand-edited config file.
fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// Pulls images and brings every enabled stack up.
///
/// A failed pull is fatal by default, and the experiment is *not* started.
///
/// Experiment images are cross-built on a workstation and pushed to a registry on the lab LAN —
/// a robot never builds them. `up -d` will happily start whatever stale image is already in
/// /data/docker, so treating a failed pull as a warning means a robot that missed a deploy keeps
/// running an old experiment indefinitely with nothing to indicate the results are from the
/// wrong code. Refusing to start is the louder failure, and the recoverable one.
///
/// Failures are collected rather than raised at the first one: an unreachable registry should
/// not stop the *other* experiments from being attempted. The command still exits non-zero, so
/// the systemd unit fails and `robotctl status` reports it.
pub fn start(paths: &Paths, options: StartOptions) -> Result<()> {
    let settings_path = paths.experiments_dir().join("experiments.conf");
    ensure_file(&settings_path, SETTINGS_TEMPLATE)?;

    let ignore_pull_failures = match options.ignore_pull_failures {
        Some(override_value) => {
            log::info(format!(
                "{IGNORE_PULL_FAILURES_KEY}={override_value} for this run (command line)"
            ));
            override_value
        }
        None => ignore_pull_failures(&settings_path)?,
    };

    let mut failed: Vec<String> = Vec::new();

    for_each_enabled(paths, |dir, name| {
        log::info(format!("pulling images for experiment '{name}'"));

        if let Err(err) = compose(dir, name, &["pull", "--quiet"]) {
            if !ignore_pull_failures {
                log::error(format!(
                    "pull failed for '{name}': {err:#}. Not starting it — the local image may be \
                     out of date. Set {IGNORE_PULL_FAILURES_KEY}=true in {} to start it anyway \
                     on this robot, or pass --ignore-pull-failures for one run.",
                    settings_path.display()
                ));
                failed.push(name.to_string());
                return;
            }

            log::warn(format!(
                "pull failed for '{name}' ({err:#}); starting with the local image \
                 ({IGNORE_PULL_FAILURES_KEY}=true)"
            ));
        }

        log::info(format!("starting experiment '{name}'"));
        if let Err(err) = compose(dir, name, &["up", "-d", "--remove-orphans"]) {
            log::warn(format!("failed to start experiment '{name}': {err:#}"));
        }
    })?;

    if !failed.is_empty() {
        anyhow::bail!(
            "could not pull images for {} of the enabled experiments: {}",
            failed.len(),
            failed.join(", ")
        );
    }

    Ok(())
}

pub fn stop(paths: &Paths) -> Result<()> {
    for_each_enabled(paths, |dir, name| {
        log::info(format!("stopping experiment '{name}'"));
        if let Err(err) = compose(dir, name, &["down"]) {
            log::warn(format!("failed to stop experiment '{name}': {err:#}"));
        }
    })
}

pub fn status(paths: &Paths) -> Result<()> {
    for_each_enabled(paths, |dir, name| {
        println!("== {name} ==");
        if let Err(err) = compose(dir, name, &["ps"]) {
            log::warn(format!("could not query experiment '{name}': {err:#}"));
        }
    })
}

/// Reads `enabled.conf` and runs `action` for every enabled experiment that has a compose file.
///
/// Every per-experiment failure is a non-fatal warning.
fn for_each_enabled(paths: &Paths, mut action: impl FnMut(&Path, &ExperimentName)) -> Result<()> {
    require_docker()?;

    let dir = paths.experiments_dir();
    let conf_path = dir.join("enabled.conf");
    ensure_file(&conf_path, ENABLED_CONF_TEMPLATE)?;

    let text = conf::read_optional(&conf_path)?.unwrap_or_default();
    let list = parse_enabled(&text);

    for problem in &list.warnings {
        log::warn(format!("{}: {problem}", conf_path.display()));
    }

    if list.names.is_empty() {
        log::info(format!("no experiments enabled in {}", conf_path.display()));
        return Ok(());
    }

    for name in &list.names {
        if !compose_file(&dir, name).is_file() {
            log::warn(format!(
                "skipping '{name}': no compose file at {}",
                compose_file(&dir, name).display()
            ));
            continue;
        }

        action(&dir, name);
    }

    Ok(())
}

/// Creates `path` from `template` if it does not exist yet, so a fresh robot gets a file that
/// explains itself rather than nothing at all. Ported from the shell's `ensure_enabled_conf`.
fn ensure_file(path: &Path, template: &str) -> Result<()> {
    if path.is_file() {
        return Ok(());
    }

    conf::write_atomically(path, template)
        .with_context(|| format!("creating {}", path.display()))?;

    log::info(format!("created {}", path.display()));

    Ok(())
}

fn compose_file(dir: &Path, name: &ExperimentName) -> PathBuf {
    dir.join(name.as_str()).join("docker-compose.yml")
}

/// Runs `docker compose` for one experiment, with output inherited.
fn compose(dir: &Path, name: &ExperimentName, args: &[&str]) -> Result<()> {
    let project_dir = dir.join(name.as_str());

    let status = Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(compose_file(dir, name))
        .arg("--project-directory")
        .arg(&project_dir)
        .args(args)
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("running `docker compose {}`", args.join(" ")))?;

    if !status.success() {
        anyhow::bail!("`docker compose {}` exited with {status}", args.join(" "));
    }

    Ok(())
}

fn require_docker() -> Result<()> {
    Command::new("docker")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("required command not found: docker")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn names(list: &EnabledList) -> Vec<&str> {
        list.names.iter().map(ExperimentName::as_str).collect()
    }

    #[test]
    fn accepts_ordinary_names() {
        for name in ["obstacle-avoidance", "slam2", "a", "line_follow", "v1.2"] {
            assert!(ExperimentName::new(name).is_ok(), "rejected {name:?}");
        }
    }

    #[test]
    fn rejects_names_that_would_escape_the_experiments_directory() {
        use ExperimentNameError as E;

        // The whole reason this validation exists: enabled.conf is hand-edited on /data and its
        // contents are joined onto a path.
        assert_eq!(ExperimentName::new(".."), Err(E::RelativeComponent));
        assert_eq!(ExperimentName::new("../../etc"), Err(E::PathSeparator));
        assert_eq!(ExperimentName::new("/etc/passwd"), Err(E::PathSeparator));
        assert_eq!(ExperimentName::new("a/b"), Err(E::PathSeparator));
        assert_eq!(ExperimentName::new(".hidden"), Err(E::HiddenName));
        assert_eq!(ExperimentName::new(""), Err(E::Empty));
    }

    #[test]
    fn rejects_shell_and_whitespace_characters() {
        use ExperimentNameError as E;

        assert_eq!(
            ExperimentName::new("two words"),
            Err(E::InvalidCharacter { found: ' ' })
        );
        assert_eq!(
            ExperimentName::new("rm;reboot"),
            Err(E::InvalidCharacter { found: ';' })
        );
        assert_eq!(
            ExperimentName::new("sub$(cmd)"),
            Err(E::InvalidCharacter { found: '$' })
        );
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let text = "# a comment\n\n  \nobstacle-avoidance\n\t\n# another\nslam\n";

        let list = parse_enabled(text);

        assert_eq!(names(&list), ["obstacle-avoidance", "slam"]);
        assert!(list.warnings.is_empty());
    }

    #[test]
    fn trims_surrounding_whitespace_from_names() {
        let list = parse_enabled("  obstacle-avoidance  \n");

        assert_eq!(names(&list), ["obstacle-avoidance"]);
        assert!(list.warnings.is_empty());
    }

    #[test]
    fn drops_duplicates_and_says_so() {
        let list = parse_enabled("slam\nobstacle\nslam\n");

        assert_eq!(names(&list), ["slam", "obstacle"]);
        assert_eq!(list.warnings.len(), 1);
        assert!(list.warnings[0].contains("line 3"));
        assert!(list.warnings[0].contains("more than once"));
    }

    #[test]
    fn one_bad_line_does_not_discard_the_good_ones() {
        let list = parse_enabled("good-one\n../escape\nanother-good\n");

        assert_eq!(names(&list), ["good-one", "another-good"]);
        assert_eq!(list.warnings.len(), 1);
        assert!(list.warnings[0].contains("line 2"));
    }

    #[test]
    fn an_empty_file_enables_nothing_without_complaining() {
        assert_eq!(parse_enabled(""), EnabledList::default());
        assert_eq!(parse_enabled("# only comments\n"), EnabledList::default());
    }

    #[test]
    fn the_shipped_template_parses_to_nothing_enabled() {
        // The auto-created file must not accidentally enable its own example.
        assert_eq!(parse_enabled(ENABLED_CONF_TEMPLATE), EnabledList::default());
    }

    #[test]
    fn creates_enabled_conf_when_it_is_missing() {
        let tmp = TempDir::new("experiments-conf");
        let path = tmp.path().join("experiments/enabled.conf");

        ensure_file(&path, ENABLED_CONF_TEMPLATE).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, ENABLED_CONF_TEMPLATE);
        assert_eq!(parse_enabled(&written), EnabledList::default());
    }

    #[test]
    fn does_not_overwrite_an_existing_enabled_conf() {
        let tmp = TempDir::new("experiments-keep");
        let path = tmp.path().join("experiments/enabled.conf");
        conf::write_atomically(&path, "slam\n").unwrap();

        ensure_file(&path, ENABLED_CONF_TEMPLATE).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "slam\n");
    }

    #[test]
    fn parses_the_booleans_people_actually_write() {
        for yes in ["true", "TRUE", "yes", "on", "1", " true "] {
            assert_eq!(parse_bool(yes), Some(true), "{yes:?}");
        }
        for no in ["false", "FALSE", "no", "off", "0"] {
            assert_eq!(parse_bool(no), Some(false), "{no:?}");
        }
        for bad in ["", "maybe", "2"] {
            assert_eq!(parse_bool(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_failed_pull_is_fatal_when_there_is_no_settings_file() {
        let tmp = TempDir::new("experiments-nosettings");

        assert!(!ignore_pull_failures(&tmp.path().join("experiments.conf")).unwrap());
    }

    #[test]
    fn the_settings_file_can_turn_pull_failures_non_fatal() {
        let tmp = TempDir::new("experiments-settings");
        let path = tmp.path().join("experiments.conf");

        conf::write_atomically(&path, "IGNORE_PULL_FAILURES=true\n").unwrap();
        assert!(ignore_pull_failures(&path).unwrap());

        conf::write_atomically(&path, "IGNORE_PULL_FAILURES=false\n").unwrap();
        assert!(!ignore_pull_failures(&path).unwrap());
    }

    #[test]
    fn an_unparseable_setting_falls_back_to_fatal() {
        // Failing safe matters here: the fallback must be the strict behaviour, so a typo
        // cannot quietly re-enable "run whatever image is lying around".
        let tmp = TempDir::new("experiments-badsetting");
        let path = tmp.path().join("experiments.conf");
        conf::write_atomically(&path, "IGNORE_PULL_FAILURES=maybe\n").unwrap();

        assert!(!ignore_pull_failures(&path).unwrap());
    }

    #[test]
    fn the_shipped_settings_template_is_the_strict_default() {
        let tmp = TempDir::new("experiments-template");
        let path = tmp.path().join("experiments.conf");
        ensure_file(&path, SETTINGS_TEMPLATE).unwrap();

        assert!(!ignore_pull_failures(&path).unwrap());
    }

    #[test]
    fn compose_file_path_is_built_from_the_validated_name() {
        let name = ExperimentName::new("slam").unwrap();

        assert_eq!(
            compose_file(Path::new("/data/experiments"), &name),
            Path::new("/data/experiments/slam/docker-compose.yml")
        );
    }
}
