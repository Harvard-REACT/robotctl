//! Robot identity: handles the robot's hostname and ROS Domain ID. This information is stored in `robot.conf` and derived into `/etc/hostname` and `/etc/hosts`, and the running hostname is updated to match.
//! The path to `robot.conf` is configurable and is read from [`Paths::robot_conf()`].

use std::fmt;
#[cfg(test)]
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;
use std::str::FromStr;

use anyhow::Result;
use thiserror::Error;

use crate::conf;
use crate::config::Paths;
use crate::log;

const ROBOT_ID_KEY: &str = "ROBOT_ID";
const ROS_DOMAIN_ID_KEY: &str = "ROS_DOMAIN_ID";

const ROS_DOMAIN_ID_MAX: u8 = 232;

#[derive(Debug, Clone, Error)]
pub enum RobotIdValidationError {
    #[error("Robot ID must be lowercase. Found character '{char}' at position {pos}.")]
    Lowercase { char: char, pos: usize },

    #[error(
        "Robot ID may only contain letters, digits, and hyphens. Found character '{char}' at position {pos}."
    )]
    InvalidChar { char: char, pos: usize },

    #[error(
        "Robot ID must be between 1 and 63 characters long. Supplied string has length {length}."
    )]
    InvalidLength { length: usize },

    #[error("Robot ID must not start with a hyphen.")]
    StartsWithHyphen,

    #[error("Robot ID must not end in a hyphen.")]
    EndsWithHyphen,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RobotId(String);

impl RobotId {
    /// Validates a string to ensure it is a valid RobotId according to the following rules:
    /// - Must be lowercase
    /// - May only contain letters, digits, and hyphens
    /// - Must be between 1 and 63 characters long
    /// - Must not start or end with a hyphen
    pub fn new(id: &str) -> Result<Self, RobotIdValidationError> {
        // One pass, uppercase checked first so "GoPiGo-01" reports the more useful "must be
        // lowercase" rather than "invalid character".
        for (pos, char) in id.chars().enumerate() {
            if char.is_ascii_uppercase() {
                return Err(RobotIdValidationError::Lowercase { char, pos });
            }

            if !char.is_ascii_lowercase() && !char.is_ascii_digit() && char != '-' {
                return Err(RobotIdValidationError::InvalidChar { char, pos });
            }
        }

        // Every character is ASCII by the time we get here, so byte length and character count
        // agree and `len()` is an honest answer.
        if id.is_empty() || id.len() > 63 {
            return Err(RobotIdValidationError::InvalidLength { length: id.len() });
        }

        if id.starts_with('-') {
            return Err(RobotIdValidationError::StartsWithHyphen);
        }

        if id.ends_with('-') {
            return Err(RobotIdValidationError::EndsWithHyphen);
        }

        Ok(RobotId(id.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RobotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Error)]
pub enum RosDomainIdValidationError {
    #[error("ROS domain ID must be between 0 and {ROS_DOMAIN_ID_MAX} inclusive. Found {value}.")]
    OutOfRange { value: u8 },

    #[error("ROS domain ID must be an integer between 0 and {ROS_DOMAIN_ID_MAX}. Found '{value}'.")]
    NotAnInteger { value: String },
}

/// The valid range is 0..=232, which fits a `u8` exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RosDomainId(u8);

impl RosDomainId {
    /// Validates a ROS domain ID: must be between 0 and 232 inclusive.
    ///
    /// Values above 101 are valid but risk port collisions with the host OS
    pub fn new(value: u8) -> Result<Self, RosDomainIdValidationError> {
        if value > ROS_DOMAIN_ID_MAX {
            return Err(RosDomainIdValidationError::OutOfRange { value });
        }

        Ok(RosDomainId(value))
    }

    pub fn value(&self) -> u8 {
        self.0
    }
}

impl FromStr for RosDomainId {
    type Err = RosDomainIdValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parsed =
            s.trim()
                .parse::<u8>()
                .map_err(|_| RosDomainIdValidationError::NotAnInteger {
                    value: s.to_string(),
                })?;

        Self::new(parsed)
    }
}

impl fmt::Display for RosDomainId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.value())
    }
}

/// The contents of `robot.conf`, resolved against the defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobotConfig {
    pub robot_id: RobotId,
    pub ros_domain_id: RosDomainId,
}

/// Reads `robot.conf`, falling back to the configured defaults for anything missing or invalid.
pub fn load(paths: &Paths) -> Result<RobotConfig> {
    let path = paths.robot_conf();
    let Some(text) = conf::read_optional(&path)? else {
        log::info(format!(
            "{} does not exist; using defaults (robot is unprovisioned)",
            path.display()
        ));
        return Ok(RobotConfig {
            robot_id: paths.default_robot_id().clone(),
            ros_domain_id: paths.default_ros_domain_id(),
        });
    };

    let robot_id = match conf::parse_value(&text, ROBOT_ID_KEY) {
        None => paths.default_robot_id().clone(),
        Some(raw) => RobotId::new(&raw).unwrap_or_else(|err| {
            log::warn(format!(
                "{}: invalid {ROBOT_ID_KEY}='{raw}'. {err} Using '{}'.",
                path.display(),
                paths.default_robot_id()
            ));
            paths.default_robot_id().clone()
        }),
    };

    let ros_domain_id = match conf::parse_value(&text, ROS_DOMAIN_ID_KEY) {
        None => paths.default_ros_domain_id(),
        Some(raw) => raw.parse::<RosDomainId>().unwrap_or_else(|err| {
            log::warn(format!(
                "{}: invalid {ROS_DOMAIN_ID_KEY}='{raw}'. {err} Using {}.",
                path.display(),
                paths.default_ros_domain_id()
            ));
            paths.default_ros_domain_id()
        }),
    };

    Ok(RobotConfig {
        robot_id,
        ros_domain_id,
    })
}

/// Sets one key in `robot.conf`, preserving every other line.
fn set_key(path: &Path, key: &str, value: &str) -> Result<()> {
    let existing = conf::read_optional(path)?;
    let updated = conf::upsert_key(existing.as_deref(), key, value);
    conf::write_atomically(path, &updated)
}

/// Re-derives every hostname artifact from `config`, then applies the running hostname.
///
/// Idempotent by construction: files are compared before being written, so the steady-state boot
/// costs zero writes to the rootfs. Safe to run on every boot and by hand at any time.
pub fn apply(paths: &Paths, config: &RobotConfig) -> Result<()> {
    apply_files(paths, config)?;
    apply_runtime_hostname(&config.robot_id);
    Ok(())
}

fn apply_files(paths: &Paths, config: &RobotConfig) -> Result<()> {
    let hostname_path = paths.hostname_path();
    if conf::write_if_changed(hostname_path, &render_hostname_file(&config.robot_id))? {
        log::info(format!(
            "{} now reads '{}'",
            hostname_path.display(),
            config.robot_id
        ));
    }

    let hosts_path = paths.hosts_path();
    let existing = conf::read_optional(hosts_path)?;
    let rendered = render_hosts_file(existing.as_deref(), &config.robot_id);
    if conf::write_if_changed(hosts_path, &rendered)? {
        log::info(format!(
            "{} now maps 127.0.1.1 to '{}'",
            hosts_path.display(),
            config.robot_id
        ));
    }

    Ok(())
}

fn render_hostname_file(robot_id: &RobotId) -> String {
    format!("{robot_id}\n")
}

/// Rewrites the `127.0.1.1` line and nothing else, appending it if absent.
///
/// A missing file is materialised with a `localhost` line as well, matching the shell. Every
/// other line is preserved verbatim.
fn render_hosts_file(existing: Option<&str>, robot_id: &RobotId) -> String {
    let entry = format!("127.0.1.1\t{robot_id}");

    let Some(existing) = existing else {
        return format!("127.0.0.1\tlocalhost\n{entry}\n");
    };

    let mut out = String::with_capacity(existing.len() + entry.len() + 1);
    let mut replaced = false;

    for line in existing.lines() {
        // Matches the shell's `/^127\.0\.1\.1[[:space:]]+/`: the address must be followed by
        // whitespace, so a hypothetical 127.0.1.10 is left alone.
        let is_entry = line
            .strip_prefix("127.0.1.1")
            .is_some_and(|rest| rest.starts_with([' ', '\t']));

        if is_entry {
            out.push_str(&entry);
            replaced = true;
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }

    if !replaced {
        out.push_str(&entry);
        out.push('\n');
    }

    out
}

/// Sets the *running* hostname only.
fn apply_runtime_hostname(robot_id: &RobotId) {
    let attempts: [(&str, Vec<&str>); 2] = [
        (
            "hostnamectl",
            vec!["--transient", "set-hostname", robot_id.as_str()],
        ),
        ("hostname", vec![robot_id.as_str()]),
    ];

    for (program, args) in &attempts {
        match Command::new(program).args(args).status() {
            Ok(status) if status.success() => return,
            Ok(status) => log::warn(format!(
                "{program} exited with {status}; trying next fallback"
            )),
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => log::warn(format!("could not run {program}: {err}")),
        }
    }

    log::warn(
        "Could not update the running hostname. Persistent hostname files were still updated.",
    );
}

pub fn robot_id(paths: &Paths) -> Result<RobotId> {
    Ok(load(paths)?.robot_id)
}

pub fn ros_domain_id(paths: &Paths) -> Result<RosDomainId> {
    Ok(load(paths)?.ros_domain_id)
}

/// Writes `ROBOT_ID` and re-derives every artifact from it.
pub fn set_robot_id(paths: &Paths, robot_id: &RobotId) -> Result<()> {
    set_key(&paths.robot_conf(), ROBOT_ID_KEY, robot_id.as_str())?;
    apply(paths, &load(paths)?)
}

/// Writes `ROS_DOMAIN_ID` and re-derives every artifact from it.
pub fn set_ros_domain_id(paths: &Paths, ros_domain_id: RosDomainId) -> Result<()> {
    set_key(
        &paths.robot_conf(),
        ROS_DOMAIN_ID_KEY,
        &ros_domain_id.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn robot_id(id: &str) -> RobotId {
        RobotId::new(id).expect("test fixture must be a valid robot ID")
    }

    #[test]
    fn accepts_valid_robot_ids() {
        for id in ["gopigo-01", "a", "unprovisioned", "r2d2", &"a".repeat(63)] {
            assert!(RobotId::new(id).is_ok(), "expected {id:?} to be accepted");
        }
    }

    #[test]
    fn rejects_invalid_robot_ids() {
        use RobotIdValidationError as E;

        /// Each case asserts *which* error a bad ID produces, not merely that it was rejected —
        /// the error text is user-facing, so "rejected for the wrong reason" is a real bug.
        type Case = (&'static str, fn(&E) -> bool);

        let cases: [Case; 7] = [
            ("", |err| matches!(err, E::InvalidLength { length: 0 })),
            ("-gopigo", |err| matches!(err, E::StartsWithHyphen)),
            ("gopigo-", |err| matches!(err, E::EndsWithHyphen)),
            ("GoPiGo", |err| {
                matches!(err, E::Lowercase { char: 'G', pos: 0 })
            }),
            ("gopigo.01", |err| {
                matches!(err, E::InvalidChar { char: '.', pos: 6 })
            }),
            ("gopigo_01", |err| {
                matches!(err, E::InvalidChar { char: '_', pos: 6 })
            }),
            ("gopigo 01", |err| {
                matches!(err, E::InvalidChar { char: ' ', pos: 6 })
            }),
        ];

        for (input, expected_error) in cases {
            let err = RobotId::new(input).expect_err(&format!("expected {input:?} to be rejected"));
            assert!(
                expected_error(&err),
                "unexpected error for {input:?}: {err}"
            );
        }
    }

    #[test]
    fn rejects_robot_ids_longer_than_63_characters() {
        let too_long = "a".repeat(64);
        assert!(matches!(
            RobotId::new(&too_long),
            Err(RobotIdValidationError::InvalidLength { length: 64 })
        ));
    }

    #[test]
    fn ros_domain_id_range() {
        assert_eq!(RosDomainId::new(0).unwrap().value(), 0);
        assert_eq!(RosDomainId::new(232).unwrap().value(), 232);
        assert!(RosDomainId::new(233).is_err());
    }

    #[test]
    fn ros_domain_id_parses_from_text() {
        use RosDomainIdValidationError as E;

        assert_eq!("0".parse::<RosDomainId>().unwrap().value(), 0);
        assert_eq!("232".parse::<RosDomainId>().unwrap().value(), 232);
        assert_eq!(" 7 ".parse::<RosDomainId>().unwrap().value(), 7);

        // The whole reason parsing goes through u32: every oversized value must still report
        // the real rule, not an integer-width error, and must stay distinguishable from text.
        assert!(matches!(
            "233".parse::<RosDomainId>(),
            Err(E::OutOfRange { value: 233 })
        ));
        assert!(matches!(
            "abc".parse::<RosDomainId>(),
            Err(E::NotAnInteger { .. })
        ));
        assert!(matches!(
            "-1".parse::<RosDomainId>(),
            Err(E::NotAnInteger { .. })
        ));
        assert!(matches!(
            "".parse::<RosDomainId>(),
            Err(E::NotAnInteger { .. })
        ));
    }

    #[test]
    fn hosts_rewrite_touches_only_the_127_0_1_1_line() {
        // The IPv6 block below is exactly what the built gopigo-image ships.
        let existing = "127.0.0.1\tlocalhost\n\
                        ::1     localhost ip6-localhost ip6-loopback\n\
                        ff02::1 ip6-allnodes\n\
                        127.0.1.1 gopigo3-rpi5\n";

        let rendered = render_hosts_file(Some(existing), &robot_id("gopigo-07"));

        assert_eq!(
            rendered,
            "127.0.0.1\tlocalhost\n\
             ::1     localhost ip6-localhost ip6-loopback\n\
             ff02::1 ip6-allnodes\n\
             127.0.1.1\tgopigo-07\n"
        );
    }

    #[test]
    fn hosts_rewrite_appends_when_there_is_no_entry() {
        let rendered = render_hosts_file(Some("127.0.0.1\tlocalhost\n"), &robot_id("gopigo-07"));

        assert_eq!(rendered, "127.0.0.1\tlocalhost\n127.0.1.1\tgopigo-07\n");
    }

    #[test]
    fn hosts_rewrite_leaves_similar_addresses_alone() {
        let existing = "127.0.1.10\tsomething-else\n";

        let rendered = render_hosts_file(Some(existing), &robot_id("gopigo-07"));

        assert_eq!(
            rendered,
            "127.0.1.10\tsomething-else\n127.0.1.1\tgopigo-07\n"
        );
    }

    #[test]
    fn hosts_rewrite_materialises_a_missing_file() {
        let rendered = render_hosts_file(None, &robot_id("gopigo-07"));

        assert_eq!(rendered, "127.0.0.1\tlocalhost\n127.0.1.1\tgopigo-07\n");
    }

    #[test]
    fn load_falls_back_to_defaults_when_robot_conf_is_missing() {
        let tmp = TempDir::new("load-missing");
        let paths = Paths::for_test(tmp.path());

        let config = load(&paths).unwrap();

        assert_eq!(config.robot_id.as_str(), "unprovisioned");
        assert_eq!(config.ros_domain_id.value(), 0);
    }

    #[test]
    fn load_falls_back_per_key_on_invalid_values() {
        let tmp = TempDir::new("load-invalid");
        let paths = Paths::for_test(tmp.path());
        conf::write_atomically(&paths.robot_conf(), "ROBOT_ID=Not Valid\nROS_DOMAIN_ID=9\n")
            .unwrap();

        let config = load(&paths).unwrap();

        // The bad key falls back; the good one is still honoured.
        assert_eq!(config.robot_id.as_str(), "unprovisioned");
        assert_eq!(config.ros_domain_id.value(), 9);
    }

    #[test]
    fn set_robot_id_writes_config_and_derives_hostname_files() {
        let tmp = TempDir::new("set-robot-id");
        let paths = Paths::for_test(tmp.path());

        set_key(&paths.robot_conf(), ROBOT_ID_KEY, "gopigo-07").unwrap();
        apply_files(&paths, &load(&paths).unwrap()).unwrap();

        assert_eq!(
            fs::read_to_string(paths.robot_conf()).unwrap(),
            "ROBOT_ID=gopigo-07\n"
        );
        assert_eq!(
            fs::read_to_string(paths.hostname_path()).unwrap(),
            "gopigo-07\n"
        );
        assert_eq!(
            fs::read_to_string(paths.hosts_path()).unwrap(),
            "127.0.0.1\tlocalhost\n127.0.1.1\tgopigo-07\n"
        );
    }

    #[test]
    fn apply_is_idempotent_and_does_not_rewrite_unchanged_files() {
        let tmp = TempDir::new("apply-idempotent");
        let paths = Paths::for_test(tmp.path());
        let config = RobotConfig {
            robot_id: robot_id("gopigo-07"),
            ros_domain_id: RosDomainId::new(0).unwrap(),
        };

        apply_files(&paths, &config).unwrap();
        let first = fs::read_to_string(paths.hosts_path()).unwrap();
        let stamp = fs::metadata(paths.hostname_path())
            .unwrap()
            .modified()
            .unwrap();

        apply_files(&paths, &config).unwrap();

        assert_eq!(fs::read_to_string(paths.hosts_path()).unwrap(), first);
        assert_eq!(
            fs::metadata(paths.hostname_path())
                .unwrap()
                .modified()
                .unwrap(),
            stamp,
            "second apply rewrote an unchanged file"
        );
    }

    #[test]
    fn apply_after_a_rename_replaces_the_old_entry_rather_than_adding_one() {
        let tmp = TempDir::new("apply-rename");
        let paths = Paths::for_test(tmp.path());

        set_robot_id_files_only(&paths, "gopigo-07");
        set_robot_id_files_only(&paths, "gopigo-08");

        assert_eq!(
            fs::read_to_string(paths.hosts_path()).unwrap(),
            "127.0.0.1\tlocalhost\n127.0.1.1\tgopigo-08\n"
        );
        assert_eq!(
            fs::read_to_string(paths.hostname_path()).unwrap(),
            "gopigo-08\n"
        );
    }

    /// `set_robot_id` without the runtime-hostname side effect, which needs root and a real
    /// system to be meaningful.
    fn set_robot_id_files_only(paths: &Paths, id: &str) {
        set_key(&paths.robot_conf(), ROBOT_ID_KEY, id).unwrap();
        apply_files(paths, &load(paths).unwrap()).unwrap();
    }
}
