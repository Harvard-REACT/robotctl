//! Experiment stacks: `docker compose` against `/data/experiments/<name>/docker-compose.yml`.

use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use thiserror::Error;

use crate::conf;
use crate::config::Paths;
use crate::log;

const ENABLED_CONF_TEMPLATE: &str = "\
# One experiment per line = enabled. Lines starting with # and blank
# lines are ignored. Each name must have a matching directory:
#   <experiments-dir>/<name>/docker-compose.yml
#
# A name may name the docker compose profiles to activate for it, as a
# comma-separated list after a colon:
#   my-experiment:gpu,debug
#   my-experiment:*        every profile the compose file declares
#
# Those apply when `robotctl experiments start` runs with no arguments, as
# it does at boot. Naming an experiment on the command line *replaces* them
# for that run: `robotctl experiments start my-experiment` activates no
# profiles at all, and `... my-experiment:gpu` activates only gpu.
#
# Example:
# my-experiment
";

/// Written when `experiments.conf` doesn't exist
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
const ALL_PROFILES: &str = "*";

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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
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

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ProfileNameError {
    #[error("must not be empty")]
    Empty,

    #[error("must start with a letter or a digit (found '{found}')")]
    InvalidStart { found: char },

    #[error("may only contain letters, digits, '.', '_' and '-' (found '{found}')")]
    InvalidCharacter { found: char },
}

/// A validated docker compose profile name.
///
/// Compose's schema is `[a-zA-Z0-9][a-zA-Z0-9_.-]*`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileName(String);

impl ProfileName {
    pub fn new(name: &str) -> Result<Self, ProfileNameError> {
        let mut chars = name.chars();

        let Some(first) = chars.next() else {
            return Err(ProfileNameError::Empty);
        };

        if !first.is_ascii_alphanumeric() {
            return Err(ProfileNameError::InvalidStart { found: first });
        }

        if let Some(found) =
            chars.find(|c| !c.is_ascii_alphanumeric() && !matches!(c, '.' | '_' | '-'))
        {
            return Err(ProfileNameError::InvalidCharacter { found });
        }

        Ok(ProfileName(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProfileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which compose profiles to activate for one experiment.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Profiles {
    /// No profiles: only services with no `profiles:` key of their own.
    #[default]
    None,
    /// Every profile the compose file declares — compose's `--profile "*"`.
    All,
    Named(Vec<ProfileName>),
}

impl Profiles {
    /// The `--profile` flags to put before the compose verb.
    fn args(&self) -> Vec<String> {
        let names: Vec<&str> = match self {
            Profiles::None => return Vec::new(),
            Profiles::All => vec![ALL_PROFILES],
            Profiles::Named(names) => names.iter().map(ProfileName::as_str).collect(),
        };

        names
            .into_iter()
            .flat_map(|name| ["--profile".to_string(), name.to_string()])
            .collect()
    }

    /// Combines two profile sets for the same experiment named twice in one invocation.
    fn merged(self, other: Profiles) -> Profiles {
        match (self, other) {
            (Profiles::All, _) | (_, Profiles::All) => Profiles::All,
            (Profiles::None, other) => other,
            (mine, Profiles::None) => mine,
            (Profiles::Named(mut mine), Profiles::Named(other)) => {
                for name in other {
                    if !mine.contains(&name) {
                        mine.push(name);
                    }
                }
                Profiles::Named(mine)
            }
        }
    }
}

impl fmt::Display for Profiles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Profiles::None => Ok(()),
            Profiles::All => f.write_str(ALL_PROFILES),
            Profiles::Named(names) => {
                for (index, name) in names.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{name}")?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum TargetError {
    #[error("'{name}' is not a valid experiment name: {source}")]
    Name {
        name: String,
        source: ExperimentNameError,
    },

    #[error("'{profile}' is not a valid compose profile: {source}")]
    Profile {
        profile: String,
        source: ProfileNameError,
    },

    #[error("'{ALL_PROFILES}' already means every profile, so it cannot be combined with others")]
    WildcardWithOthers,
}

/// One experiment to act on, and the profiles to act on it with.
///
/// Parsed from `name[:profile[,profile...]]` — a colon cannot appear in an experiment name, so
/// the suffix is unambiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub name: ExperimentName,
    pub profiles: Profiles,
}

impl Target {
    pub fn new(name: ExperimentName, profiles: Profiles) -> Self {
        Target { name, profiles }
    }

    /// Parses one `name[:profile[,profile...]]` argument.
    ///
    /// A bare `name` and a trailing-colon `name:` both mean "no profiles": the command line is
    /// authoritative, so naming an experiment explicitly never inherits what `enabled.conf`
    /// configured for it.
    pub fn parse(spec: &str) -> Result<Self, TargetError> {
        let spec = spec.trim();

        let (name, suffix) = match spec.split_once(':') {
            Some((name, suffix)) => (name.trim(), Some(suffix.trim())),
            None => (spec, None),
        };

        let name = ExperimentName::new(name).map_err(|source| TargetError::Name {
            name: name.to_string(),
            source,
        })?;

        let profiles = match suffix {
            None | Some("") => Profiles::None,
            Some(ALL_PROFILES) => Profiles::All,
            Some(list) => {
                let mut names: Vec<ProfileName> = Vec::new();

                for entry in list.split(',') {
                    let entry = entry.trim();

                    if entry == ALL_PROFILES {
                        return Err(TargetError::WildcardWithOthers);
                    }

                    let profile =
                        ProfileName::new(entry).map_err(|source| TargetError::Profile {
                            profile: entry.to_string(),
                            source,
                        })?;

                    if !names.contains(&profile) {
                        names.push(profile);
                    }
                }

                Profiles::Named(names)
            }
        };

        Ok(Target::new(name, profiles))
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.profiles {
            Profiles::None => write!(f, "{}", self.name),
            profiles => write!(f, "{} (profiles: {profiles})", self.name),
        }
    }
}

/// The result of reading `enabled.conf`: the targets to act on, and everything rejected on the way.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EnabledList {
    pub targets: Vec<Target>,
    pub warnings: Vec<String>,
}

/// Parses `enabled.conf`: one `name[:profiles]` per line, `#` comments and blank lines ignored.
///
/// Duplicate names are dropped with a warning. Unlike the command line, where naming an
/// experiment twice merges its profile sets, a hand-edited file listing one twice is a mistake
/// worth pointing at rather than quietly reconciling.
pub fn parse_enabled(text: &str) -> EnabledList {
    let mut list = EnabledList::default();

    for (number, line) in text.lines().enumerate() {
        let line = line.trim();

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        match Target::parse(line) {
            Err(err) => list
                .warnings
                .push(format!("line {}: '{line}': {err}", number + 1)),

            Ok(target) if list.targets.iter().any(|other| other.name == target.name) => {
                list.warnings.push(format!(
                    "line {}: '{}' is listed more than once",
                    number + 1,
                    target.name
                ))
            }

            Ok(target) => list.targets.push(target),
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

/// Pulls images and brings the selected stacks up.
///
/// With no targets, that is everything in `enabled.conf`, with the profiles configured there —
/// which is how the boot-time systemd unit invokes it, and the only way profiles can differ
/// per robot, since the unit's `ExecStart` is baked into the image.
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
pub fn start(paths: &Paths, targets: Vec<Target>, options: StartOptions) -> Result<()> {
    require_docker()?;

    let (dir, targets) = select(paths, targets, Fallback::Enabled)?;

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

    for target in &targets {
        let Target { name, profiles } = target;

        log::info(format!("pulling images for experiment '{target}'"));

        if let Err(err) = compose(&dir, name, profiles, &["pull", "--quiet"]) {
            if !ignore_pull_failures {
                log::error(format!(
                    "pull failed for '{name}': {err:#}. Not starting it — the local image may be \
                     out of date. Set {IGNORE_PULL_FAILURES_KEY}=true in {} to start it anyway \
                     on this robot, or pass --ignore-pull-failures for one run.",
                    settings_path.display()
                ));
                failed.push(name.to_string());
                continue;
            }

            log::warn(format!(
                "pull failed for '{name}' ({err:#}); starting with the local image \
                 ({IGNORE_PULL_FAILURES_KEY}=true)"
            ));
        }

        log::info(format!("starting experiment '{target}'"));

        if let Err(err) = compose(&dir, name, profiles, &["up", "-d", "--remove-orphans"]) {
            log::error(format!("failed to start experiment '{name}': {err:#}"));
            failed.push(name.to_string());
        }
    }

    report(&failed, targets.len(), "start")
}

/// Brings the selected stacks down.
///
/// With no targets this sweeps *every* stack directory on disk, not just the enabled ones: an
/// experiment removed from `enabled.conf` while still running would otherwise never be stopped
/// by anything. `down` on a stack that isn't running is a cheap no-op.
///
/// Unless the caller narrowed it to specific profiles, every profile is activated
/// (`--profile "*"`) and orphans are removed, because compose's `down` only removes containers
/// for services in the *active* profile set — so a plain `down` would strand containers that
/// were started under a profile. Stop means stop.
pub fn stop(paths: &Paths, targets: Vec<Target>) -> Result<()> {
    require_docker()?;

    let (dir, targets) = select(paths, targets, Fallback::EverythingOnDisk)?;

    let mut failed: Vec<String> = Vec::new();

    for target in &targets {
        // `Profiles::None` here means the caller said nothing about profiles, not that they want
        // a profile-less teardown — there is no such thing, since the goal is an empty project.
        let (profiles, verb): (Profiles, &[&str]) = match &target.profiles {
            Profiles::Named(_) => (target.profiles.clone(), &["down"]),
            _ => (Profiles::All, &["down", "--remove-orphans"]),
        };

        log::info(format!("stopping experiment '{target}'"));

        if let Err(err) = compose(&dir, &target.name, &profiles, verb) {
            log::error(format!(
                "failed to stop experiment '{}': {err:#}",
                target.name
            ));
            failed.push(target.name.to_string());
        }
    }

    report(&failed, targets.len(), "stop")
}

/// `docker compose ps` for the selected stacks.
///
/// Per-experiment failures stay warnings and the command still exits 0: a report that aborts
/// because one stack could not be queried is least useful exactly when something is wrong.
pub fn status(paths: &Paths, targets: Vec<Target>) -> Result<()> {
    require_docker()?;

    let (dir, targets) = select(paths, targets, Fallback::Enabled)?;

    for target in &targets {
        println!("== {target} ==");

        if let Err(err) = compose(&dir, &target.name, &target.profiles, &["ps"]) {
            log::warn(format!(
                "could not query experiment '{}': {err:#}",
                target.name
            ));
        }
    }

    Ok(())
}

/// What exists on this robot: every stack directory, whether it is enabled, the profiles
/// `enabled.conf` configures for it, and the profiles its compose file declares.
///
/// The available-profiles column comes from `docker compose config --profiles` rather than a
/// YAML parser of our own — same reasoning as everywhere else in this module: compose's own
/// semantics are a large surface, and re-implementing a corner of it is how the two drift.
pub fn list(paths: &Paths) -> Result<()> {
    let dir = paths.experiments_dir();

    let enabled = match conf::read_optional(&dir.join("enabled.conf"))? {
        Some(text) => parse_enabled(&text).targets,
        None => Vec::new(),
    };

    let mut names = on_disk(&dir)?;

    // An enabled name with nothing on disk is exactly what someone running this command wants
    // to be told about, so it gets a row rather than being silently absent.
    for target in &enabled {
        if !names.contains(&target.name) {
            names.push(target.name.clone());
        }
    }
    names.sort();

    if names.is_empty() {
        log::info(format!("no experiments in {}", dir.display()));
        return Ok(());
    }

    let rows: Vec<[String; 4]> = names
        .iter()
        .map(|name| {
            let configured = enabled
                .iter()
                .find(|target| &target.name == name)
                .map(|target| target.profiles.to_string())
                .filter(|profiles| !profiles.is_empty());

            let available = if compose_file(&dir, name).is_file() {
                match declared_profiles(&dir, name) {
                    Ok(profiles) if profiles.is_empty() => "-".to_string(),
                    Ok(profiles) => profiles.join(", "),
                    // Only the column is unknown; the row is still worth printing.
                    Err(_) => "?".to_string(),
                }
            } else {
                "(no compose file)".to_string()
            };

            [
                name.to_string(),
                if enabled.iter().any(|target| &target.name == name) {
                    "yes".to_string()
                } else {
                    "no".to_string()
                },
                configured.unwrap_or_else(|| "-".to_string()),
                available,
            ]
        })
        .collect();

    print_table(
        ["NAME", "ENABLED", "CONFIGURED", "AVAILABLE PROFILES"],
        &rows,
    );

    Ok(())
}

/// Left-aligned columns, sized to their contents. The last column is not padded.
fn print_table(headers: [&str; 4], rows: &[[String; 4]]) {
    let mut widths = headers.map(str::len);

    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }

    let line = |cells: [&str; 4]| {
        let mut out = String::new();
        for (index, cell) in cells.iter().enumerate() {
            if index + 1 == cells.len() {
                out.push_str(cell);
            } else {
                out.push_str(&format!("{cell:<width$}  ", width = widths[index]));
            }
        }
        println!("{}", out.trim_end());
    };

    line(headers);

    for row in rows {
        line([&row[0], &row[1], &row[2], &row[3]]);
    }
}

/// What to act on when the command line named nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fallback {
    /// `enabled.conf`, with the profiles configured there.
    Enabled,
    /// Every stack directory that has a compose file.
    EverythingOnDisk,
}

/// Turns what the command line asked for into the exact list to act on.
///
/// Explicitly named experiments are validated up front and a missing one is fatal *before*
/// anything is started or stopped: an argument is a direct instruction, and half-executing it
/// after a typo is worse than executing none of it. Names that come from `enabled.conf` keep
/// the older, softer treatment — a warning and a skip — because that file is read unattended
/// at boot, where refusing to start the other four stacks helps nobody.
fn select(
    paths: &Paths,
    targets: Vec<Target>,
    fallback: Fallback,
) -> Result<(PathBuf, Vec<Target>)> {
    let dir = paths.experiments_dir();

    if !targets.is_empty() {
        let targets = merge_duplicates(targets);

        let missing: Vec<&Target> = targets
            .iter()
            .filter(|target| !compose_file(&dir, &target.name).is_file())
            .collect();

        if !missing.is_empty() {
            anyhow::bail!(
                "no compose file for {} of the named experiments (nothing was done):\n{}",
                missing.len(),
                missing
                    .iter()
                    .map(|target| format!(
                        "  {}: {} does not exist",
                        target.name,
                        compose_file(&dir, &target.name).display()
                    ))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }

        return Ok((dir, targets));
    }

    let targets = match fallback {
        Fallback::Enabled => enabled_targets(&dir)?,
        Fallback::EverythingOnDisk => {
            let names = on_disk(&dir)?;

            if names.is_empty() {
                log::info(format!("no experiment stacks in {}", dir.display()));
            }

            names
                .into_iter()
                .map(|name| Target::new(name, Profiles::None))
                .collect()
        }
    };

    Ok((dir, targets))
}

/// Reads `enabled.conf`, creating it from the template if this robot has never had one, and
/// drops the entries with no compose file with a warning.
fn enabled_targets(dir: &Path) -> Result<Vec<Target>> {
    let conf_path = dir.join("enabled.conf");
    ensure_file(&conf_path, ENABLED_CONF_TEMPLATE)?;

    let text = conf::read_optional(&conf_path)?.unwrap_or_default();
    let list = parse_enabled(&text);

    for problem in &list.warnings {
        log::warn(format!("{}: {problem}", conf_path.display()));
    }

    if list.targets.is_empty() {
        log::info(format!("no experiments enabled in {}", conf_path.display()));
        return Ok(Vec::new());
    }

    Ok(list
        .targets
        .into_iter()
        .filter(|target| {
            let compose_file = compose_file(dir, &target.name);

            if !compose_file.is_file() {
                log::warn(format!(
                    "skipping '{}': no compose file at {}",
                    target.name,
                    compose_file.display()
                ));
                return false;
            }

            true
        })
        .collect())
}

/// Every experiment directory that has a compose file, in name order.
///
/// Directory names that are not valid experiment names are ignored rather than reported: `/data`
/// is shared with docker, RAUC and journald, and whatever else lands in there is not ours to
/// complain about.
fn on_disk(dir: &Path) -> Result<Vec<ExperimentName>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", dir.display())),
    };

    let mut names: Vec<ExperimentName> = Vec::new();

    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;

        let Some(name) = entry
            .file_name()
            .to_str()
            .and_then(|name| ExperimentName::new(name).ok())
        else {
            continue;
        };

        if compose_file(dir, &name).is_file() {
            names.push(name);
        }
    }

    names.sort();

    Ok(names)
}

/// Collapses an experiment named more than once into one entry whose profiles are the union.
///
/// `start slam:gpu slam:debug` is one `up` with both profiles, not two conflicting ones — a
/// stack can only be brought up once per invocation, so the alternative is picking a winner.
fn merge_duplicates(targets: Vec<Target>) -> Vec<Target> {
    let mut merged: Vec<Target> = Vec::with_capacity(targets.len());

    for target in targets {
        match merged.iter_mut().find(|other| other.name == target.name) {
            Some(existing) => {
                existing.profiles = std::mem::take(&mut existing.profiles).merged(target.profiles)
            }
            None => merged.push(target),
        }
    }

    merged
}

/// Fails the command if anything failed, having already attempted everything else.
fn report(failed: &[String], total: usize, verb: &str) -> Result<()> {
    if failed.is_empty() {
        return Ok(());
    }

    anyhow::bail!(
        "failed to {verb} {} of {total} experiments: {}",
        failed.len(),
        failed.join(", ")
    )
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

/// The full `docker compose` argument list for one experiment.
///
/// `--profile` is a compose-level flag, so it has to precede the verb.
fn compose_args(
    dir: &Path,
    name: &ExperimentName,
    profiles: &Profiles,
    verb: &[&str],
) -> Vec<String> {
    let mut args = vec![
        "compose".to_string(),
        "-f".to_string(),
        compose_file(dir, name).to_string_lossy().into_owned(),
        "--project-directory".to_string(),
        dir.join(name.as_str()).to_string_lossy().into_owned(),
    ];

    args.extend(profiles.args());
    args.extend(verb.iter().map(|arg| arg.to_string()));

    args
}

/// Runs `docker compose` for one experiment, with output inherited.
fn compose(dir: &Path, name: &ExperimentName, profiles: &Profiles, verb: &[&str]) -> Result<()> {
    let status = Command::new("docker")
        .args(compose_args(dir, name, profiles, verb))
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("running `docker compose {}`", verb.join(" ")))?;

    if !status.success() {
        anyhow::bail!("`docker compose {}` exited with {status}", verb.join(" "));
    }

    Ok(())
}

/// The profiles one compose file declares, via compose itself.
fn declared_profiles(dir: &Path, name: &ExperimentName) -> Result<Vec<String>> {
    let output = Command::new("docker")
        .args(compose_args(
            dir,
            name,
            &Profiles::None,
            &["config", "--profiles"],
        ))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("running `docker compose config --profiles`")?;

    if !output.status.success() {
        anyhow::bail!(
            "`docker compose config --profiles` exited with {}",
            output.status
        );
    }

    let mut profiles: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();

    profiles.sort();

    Ok(profiles)
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
        list.targets
            .iter()
            .map(|target| target.name.as_str())
            .collect()
    }

    fn target(spec: &str) -> Target {
        Target::parse(spec).expect("valid target")
    }

    fn profiles(specs: &[&str]) -> Profiles {
        Profiles::Named(
            specs
                .iter()
                .map(|spec| ProfileName::new(spec).expect("valid profile"))
                .collect(),
        )
    }

    /// An experiments directory with a compose file for each of `names`.
    fn experiments_dir(tmp: &TempDir, names: &[&str]) -> PathBuf {
        let paths = Paths::for_test(tmp.path());
        let dir = paths.experiments_dir();

        for name in names {
            conf::write_atomically(&dir.join(name).join("docker-compose.yml"), "services: {}\n")
                .unwrap();
        }

        dir
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
    fn a_bare_name_activates_no_profiles() {
        assert_eq!(
            target("slam"),
            Target::new(ExperimentName::new("slam").unwrap(), Profiles::None)
        );
    }

    #[test]
    fn a_trailing_colon_is_an_explicit_way_to_say_no_profiles() {
        // Same meaning as the bare name, spelled deliberately: "I know enabled.conf configures
        // profiles for this one, and I want none of them."
        assert_eq!(target("slam:"), target("slam"));
    }

    #[test]
    fn parses_a_profile_list() {
        assert_eq!(target("slam:gpu").profiles, profiles(&["gpu"]));
        assert_eq!(
            target("slam:gpu,debug").profiles,
            profiles(&["gpu", "debug"])
        );
        assert_eq!(
            target(" slam : gpu , debug ").profiles,
            profiles(&["gpu", "debug"])
        );
    }

    #[test]
    fn a_profile_repeated_in_one_list_is_listed_once() {
        assert_eq!(target("slam:gpu,gpu").profiles, profiles(&["gpu"]));
    }

    #[test]
    fn the_wildcard_means_every_profile_and_stands_alone() {
        assert_eq!(target("slam:*").profiles, Profiles::All);

        assert_eq!(
            Target::parse("slam:*,gpu"),
            Err(TargetError::WildcardWithOthers)
        );
        assert_eq!(
            Target::parse("slam:gpu,*"),
            Err(TargetError::WildcardWithOthers)
        );
    }

    #[test]
    fn rejects_bad_names_and_bad_profiles_with_the_offending_part_named() {
        let err = Target::parse("../escape:gpu").unwrap_err();
        assert!(err.to_string().contains("../escape"), "{err}");

        let err = Target::parse("slam:gpu,").unwrap_err();
        assert!(err.to_string().contains("compose profile"), "{err}");

        let err = Target::parse("slam:has space").unwrap_err();
        assert!(err.to_string().contains("has space"), "{err}");

        // An extra colon lands in the profile list, where it is invalid.
        assert!(Target::parse("slam:gpu:debug").is_err());
    }

    #[test]
    fn profile_names_follow_composes_own_rules() {
        use ProfileNameError as E;

        assert!(ProfileName::new("gpu-2.0_x").is_ok());
        assert_eq!(ProfileName::new(""), Err(E::Empty));
        assert_eq!(
            ProfileName::new("-gpu"),
            Err(E::InvalidStart { found: '-' })
        );
        assert_eq!(
            ProfileName::new("gpu!"),
            Err(E::InvalidCharacter { found: '!' })
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
    fn enabled_conf_carries_per_experiment_profiles() {
        // The only way a per-robot profile choice can exist: the boot unit's ExecStart is baked
        // into the image, so it cannot carry one.
        let list = parse_enabled("slam:gpu\nnav\nteleop:debug,sim\nviz:*\n");

        assert_eq!(names(&list), ["slam", "nav", "teleop", "viz"]);
        assert_eq!(list.targets[0].profiles, profiles(&["gpu"]));
        assert_eq!(list.targets[1].profiles, Profiles::None);
        assert_eq!(list.targets[2].profiles, profiles(&["debug", "sim"]));
        assert_eq!(list.targets[3].profiles, Profiles::All);
        assert!(list.warnings.is_empty());
    }

    #[test]
    fn drops_duplicates_and_says_so() {
        // Unlike the command line, where a repeated name merges: a file listing one twice is a
        // mistake to point at, not something to reconcile.
        let list = parse_enabled("slam\nobstacle\nslam:gpu\n");

        assert_eq!(names(&list), ["slam", "obstacle"]);
        assert_eq!(list.warnings.len(), 1);
        assert!(list.warnings[0].contains("line 3"));
        assert!(list.warnings[0].contains("more than once"));
    }

    #[test]
    fn one_bad_line_does_not_discard_the_good_ones() {
        let list = parse_enabled("good-one\n../escape\nanother-good\nbad:pro file\n");

        assert_eq!(names(&list), ["good-one", "another-good"]);
        assert_eq!(list.warnings.len(), 2);
        assert!(list.warnings[0].contains("line 2"));
        assert!(list.warnings[1].contains("line 4"));
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

    #[test]
    fn profile_flags_precede_the_verb() {
        // `--profile` is a compose-level flag; after the verb it is a different flag or an error.
        let dir = Path::new("/data/experiments");
        let name = ExperimentName::new("slam").unwrap();

        assert_eq!(
            compose_args(dir, &name, &profiles(&["gpu", "debug"]), &["up", "-d"]),
            [
                "compose",
                "-f",
                "/data/experiments/slam/docker-compose.yml",
                "--project-directory",
                "/data/experiments/slam",
                "--profile",
                "gpu",
                "--profile",
                "debug",
                "up",
                "-d",
            ]
        );
    }

    #[test]
    fn no_profiles_means_no_profile_flags_at_all() {
        let dir = Path::new("/data/experiments");
        let name = ExperimentName::new("slam").unwrap();

        let args = compose_args(dir, &name, &Profiles::None, &["ps"]);

        assert!(!args.contains(&"--profile".to_string()));
        assert_eq!(args.last().unwrap(), "ps");
    }

    #[test]
    fn the_wildcard_becomes_composes_own_star() {
        assert_eq!(Profiles::All.args(), ["--profile", "*"]);
    }

    #[test]
    fn naming_an_experiment_twice_merges_its_profiles() {
        let merged = merge_duplicates(vec![
            target("slam:gpu"),
            target("nav"),
            target("slam:debug"),
        ]);

        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name.as_str(), "slam");
        assert_eq!(merged[0].profiles, profiles(&["gpu", "debug"]));
        assert_eq!(merged[1].name.as_str(), "nav");
    }

    #[test]
    fn merging_the_wildcard_swallows_the_rest() {
        let merged = merge_duplicates(vec![target("slam:gpu"), target("slam:*")]);

        assert_eq!(merged[0].profiles, Profiles::All);
    }

    #[test]
    fn a_named_experiment_that_does_not_exist_stops_everything() {
        // A typo must not half-execute the command: nothing is started, and the message names
        // every missing one rather than just the first.
        let tmp = TempDir::new("experiments-missing");
        let paths = Paths::for_test(tmp.path());
        experiments_dir(&tmp, &["slam"]);

        let err = select(
            &paths,
            vec![target("slam"), target("slma"), target("navv:gpu")],
            Fallback::Enabled,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("slma"), "{err}");
        assert!(err.contains("navv"), "{err}");
        assert!(!err.contains("'slam'"), "{err}");
        assert!(err.contains("nothing was done"), "{err}");
    }

    #[test]
    fn a_named_experiment_need_not_be_enabled() {
        let tmp = TempDir::new("experiments-adhoc");
        let paths = Paths::for_test(tmp.path());
        let dir = experiments_dir(&tmp, &["slam"]);
        conf::write_atomically(&dir.join("enabled.conf"), "# nothing enabled\n").unwrap();

        let (_, targets) = select(&paths, vec![target("slam:gpu")], Fallback::Enabled).unwrap();

        assert_eq!(targets, [target("slam:gpu")]);
    }

    #[test]
    fn naming_nothing_falls_back_to_enabled_conf_with_its_profiles() {
        let tmp = TempDir::new("experiments-enabled");
        let paths = Paths::for_test(tmp.path());
        let dir = experiments_dir(&tmp, &["slam", "nav", "teleop"]);
        conf::write_atomically(&dir.join("enabled.conf"), "slam:gpu\nnav\nghost\n").unwrap();

        let (_, targets) = select(&paths, Vec::new(), Fallback::Enabled).unwrap();

        // `ghost` is enabled but has no compose file: a warning and a skip, not a failure.
        assert_eq!(targets, [target("slam:gpu"), target("nav")]);
    }

    #[test]
    fn stopping_nothing_in_particular_sweeps_every_stack_on_disk() {
        // The point of the sweep: `zeta` was dropped from enabled.conf but is still running, and
        // nothing else would ever bring it down.
        let tmp = TempDir::new("experiments-sweep");
        let paths = Paths::for_test(tmp.path());
        let dir = experiments_dir(&tmp, &["nav", "zeta", "alpha"]);
        conf::write_atomically(&dir.join("enabled.conf"), "nav\n").unwrap();

        let (_, targets) = select(&paths, Vec::new(), Fallback::EverythingOnDisk).unwrap();

        assert_eq!(
            targets,
            [target("alpha"), target("nav"), target("zeta")],
            "swept in name order"
        );
    }

    #[test]
    fn the_disk_sweep_ignores_directories_without_a_compose_file() {
        let tmp = TempDir::new("experiments-junk");
        let dir = experiments_dir(&tmp, &["slam"]);
        fs::create_dir_all(dir.join("not-an-experiment")).unwrap();
        // A plain file in the directory is not a stack either.
        conf::write_atomically(&dir.join("enabled.conf"), "slam\n").unwrap();

        assert_eq!(
            on_disk(&dir).unwrap(),
            [ExperimentName::new("slam").unwrap()]
        );
    }

    #[test]
    fn an_absent_experiments_directory_sweeps_to_nothing() {
        let tmp = TempDir::new("experiments-absent");
        let paths = Paths::for_test(tmp.path());

        let (_, targets) = select(&paths, Vec::new(), Fallback::EverythingOnDisk).unwrap();

        assert!(targets.is_empty());
    }
}
