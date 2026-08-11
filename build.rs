//! Compiles a hardware profile into the binary's built-in defaults.
//!
//! Every knob in `config::Paths` / `config::SuperviseTuning` has a built-in default and an
//! environment variable that overrides it at runtime. A *profile* sits between the two: a
//! shell-style `KEY=value` file, keyed by the same variable names, whose values are compiled in.
//! One image, one profile -- `profiles/gopigo.conf` for the GoPiGo images, `profiles/locobot.conf`
//! for the LoCoBot -- so a flashed rootfs carries the right defaults even with no environment
//! file present at all, and the same source tree builds both.
//!
//! Select one at build time:
//!
//!     ROBOTCTL_PROFILE=gopigo cargo build --release             # profiles/gopigo.conf
//!     ROBOTCTL_PROFILE_FILE=../meta-x/robotctl.conf cargo build # out-of-tree profile
//!
//! With neither set the built-in defaults below are used unchanged, which is what a plain
//! `cargo build` and the test suite do.
//!
//! A profile only needs the keys it changes; anything it omits keeps the built-in default.
//!
//! Values are validated *here* rather than at runtime, because the alternative to failing the
//! build is shipping a robot a binary that warns (or panics) on every invocation.

use std::collections::BTreeMap;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

const PROFILE_VAR: &str = "ROBOTCTL_PROFILE";

const PROFILE_FILE_VAR: &str = "ROBOTCTL_PROFILE_FILE";

const PROFILE_DIR: &str = "profiles";

#[derive(Clone, Copy)]
enum Kind {
    /// Rendered as `&str`. An empty value is rejected: every one of these has a meaningful
    /// default, and `config` treats an empty *environment* value as "unset" for the same reason.
    Str,
    /// Rendered as `&str`, empty allowed -- for the knobs where empty means something.
    OptionalStr,
    /// Rendered as `&str`, validated against the `RobotId` rules.
    RobotId,
    /// Rendered as `u8`, validated against the ROS domain ID range.
    RosDomainId,
    /// Rendered as `u32`.
    U32,
}

struct Knob {
    /// The environment variable, which is also the profile key.
    var: &'static str,
    /// The constant `config` reads.
    konst: &'static str,
    kind: Kind,
    /// The built-in default, in the same textual form a profile would write.
    value: &'static str,
    doc: &'static str,
}

/// Every compiled-in default, and the only place their values are written down.
///
/// Platform-neutral on purpose: what is specific to a robot belongs in that robot's profile, not
/// here. The keys are documented for humans in `docs/API.md` and in each `profiles/*.conf`.
const KNOBS: &[Knob] = &[
    Knob {
        var: "ROBOTCTL_DATA_DIR",
        konst: "DEFAULT_DATA_DIR",
        kind: Kind::Str,
        value: "/data",
        doc: "Partition root. Only `experiments/` hangs off it today.",
    },
    Knob {
        var: "ROBOTCTL_CONFIG_DIR",
        konst: "DEFAULT_CONFIG_DIR",
        kind: Kind::Str,
        value: "/data/robot",
        doc: "The subtree `robotctl` owns: `robot.conf` and `wifi/`.",
    },
    Knob {
        var: "ROBOTCTL_HOSTNAME_PATH",
        konst: "DEFAULT_HOSTNAME_PATH",
        kind: Kind::Str,
        value: "/etc/hostname",
        doc: "Derived output, written and never read back.",
    },
    Knob {
        var: "ROBOTCTL_HOSTS_PATH",
        konst: "DEFAULT_HOSTS_PATH",
        kind: Kind::Str,
        value: "/etc/hosts",
        doc: "Same, for the `127.0.1.1` line.",
    },
    Knob {
        var: "ROBOTCTL_RUN_DIR",
        konst: "DEFAULT_RUN_DIR",
        kind: Kind::Str,
        value: "/run/robot-wifi",
        doc: "Generated hostapd/dnsmasq configs and pidfiles.",
    },
    Knob {
        var: "ROBOTCTL_WIFI_INTERFACE",
        konst: "DEFAULT_WIFI_INTERFACE",
        kind: Kind::Str,
        value: "wlan-ctrl",
        doc: "The WiFi NIC `wifi` drives, in both client and AP mode. Names a role, not a probe \
              order: the image's udev `.link` files are what map it to hardware.",
    },
    Knob {
        var: "ROBOTCTL_WIFI_UNIT",
        konst: "DEFAULT_WIFI_UNIT",
        kind: Kind::Str,
        value: "robotctl-wifi.service",
        doc: "The systemd unit running `robotctl wifi start`. `wifi stop` needs to know what to \
              stop, and `status` reports its health.",
    },
    Knob {
        var: "ROBOTCTL_WIFI_STATE_HOOK",
        konst: "DEFAULT_WIFI_STATE_HOOK",
        kind: Kind::OptionalStr,
        value: "",
        doc: "Command run on every supervisor state change, with the state name as its only \
              argument. Empty -- the platform-neutral default -- runs nothing. This is the whole \
              of what `robotctl` knows about status indicators: what a robot does with the state \
              belongs to that robot's own image.",
    },
    Knob {
        var: "ROBOTCTL_MDNS_UNIT",
        konst: "DEFAULT_MDNS_UNIT",
        kind: Kind::OptionalStr,
        value: "avahi-daemon.service",
        doc: "The mDNS responder to nudge after a hostname change. Empty disables the nudge -- \
              one of the two knobs (with ROBOTCTL_WIFI_STATE_HOOK) where empty does not mean \
              \"use the default\".",
    },
    Knob {
        var: "ROBOTCTL_IMAGE_VERSION_PATH",
        konst: "DEFAULT_IMAGE_VERSION_PATH",
        kind: Kind::Str,
        value: "/etc/robot/image-version",
        doc: "Written into the rootfs at build time by the image recipe, so it describes the slot \
              that is running, not the robot.",
    },
    Knob {
        var: "ROBOTCTL_TRYBOOT_BACKEND",
        konst: "DEFAULT_TRYBOOT_BACKEND",
        kind: Kind::Str,
        value: "/usr/libexec/rauc/tryboot-backend",
        doc: "RAUC's custom bootloader backend. `status` calls only its read-only verbs.",
    },
    Knob {
        var: "ROBOTCTL_DEFAULT_ROBOT_ID",
        konst: "DEFAULT_ROBOT_ID",
        kind: Kind::RobotId,
        value: "unprovisioned",
        doc: "Fallback identity, used only until a `robot.conf` exists.",
    },
    Knob {
        var: "ROBOTCTL_DEFAULT_ROS_DOMAIN_ID",
        konst: "DEFAULT_ROS_DOMAIN_ID",
        kind: Kind::RosDomainId,
        value: "0",
        doc: "Fallback ROS domain ID, same situation.",
    },
    Knob {
        var: "ROBOTCTL_WIFI_CONNECT_TIMEOUT_SEC",
        konst: "DEFAULT_WIFI_CONNECT_TIMEOUT_SEC",
        kind: Kind::U32,
        value: "45",
        doc: "How long one client association + DHCP attempt may take before it counts as failed.",
    },
    Knob {
        var: "ROBOTCTL_WIFI_CLIENT_ATTEMPTS",
        konst: "DEFAULT_WIFI_CLIENT_ATTEMPTS",
        kind: Kind::U32,
        value: "3",
        doc: "Consecutive failed client attempts before giving up and raising the fallback AP.",
    },
    Knob {
        var: "ROBOTCTL_WIFI_RETRY_SEC",
        konst: "DEFAULT_WIFI_RETRY_SEC",
        kind: Kind::U32,
        value: "5",
        doc: "Pause between client attempts within one cycle.",
    },
    Knob {
        var: "ROBOTCTL_WIFI_LINK_POLL_SEC",
        konst: "DEFAULT_WIFI_LINK_POLL_SEC",
        kind: Kind::U32,
        value: "10",
        doc: "How often to re-check that an established client connection is still up.",
    },
    Knob {
        var: "ROBOTCTL_WIFI_AP_RETRY_SEC",
        konst: "DEFAULT_WIFI_AP_RETRY_SEC",
        kind: Kind::U32,
        value: "300",
        doc: "How often to drop the fallback AP and re-try the client network.",
    },
];

fn main() {
    println!("cargo::rerun-if-env-changed={PROFILE_VAR}");
    println!("cargo::rerun-if-env-changed={PROFILE_FILE_VAR}");

    let profile = select_profile();

    let overrides = match &profile {
        Some(profile) => {
            // Editing the profile must rebuild, the same way editing a source file does.
            println!("cargo::rerun-if-changed={}", profile.path.display());
            read_profile(&profile.path)
        }
        None => BTreeMap::new(),
    };

    let generated = generate(profile.as_ref(), &overrides);

    let out =
        PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR")).join("defaults.rs");

    fs::write(&out, generated)
        .unwrap_or_else(|err| fail(format!("writing {}: {err}", out.display())));
}

struct Profile {
    /// The stem, for `--version`. `profiles/gopigo.conf` and an out-of-tree `gopigo.conf` both
    /// report `gopigo`.
    name: String,
    path: PathBuf,
    /// How it was selected, for the generated file's header comment.
    origin: String,
}

fn select_profile() -> Option<Profile> {
    let name = non_empty(PROFILE_VAR);
    let file = non_empty(PROFILE_FILE_VAR);

    match (name, file) {
        (Some(name), None) => {
            // A name is a name, not a path: silently reaching outside `profiles/` would make
            // `ROBOTCTL_PROFILE=../../etc/something` build something nobody could find again.
            if name.contains('/') || name.contains('\\') || name.starts_with('.') {
                fail(format!(
                    "{PROFILE_VAR}='{name}' is a path, not a profile name. Use \
                     {PROFILE_VAR}=<name> for {PROFILE_DIR}/<name>.conf, or {PROFILE_FILE_VAR} \
                     for a profile outside this repo."
                ));
            }

            let path = manifest_dir()
                .join(PROFILE_DIR)
                .join(format!("{name}.conf"));

            if !path.is_file() {
                fail(format!(
                    "{PROFILE_VAR}='{name}' but {} does not exist. Available profiles: {}.",
                    path.display(),
                    available_profiles()
                ));
            }

            Some(Profile {
                name,
                path,
                origin: PROFILE_VAR.to_string(),
            })
        }

        (None, Some(file)) => {
            // Relative to the manifest, not to cargo's cwd: build scripts run with cwd set to the
            // manifest directory today, but that is not something to depend on.
            let path = manifest_dir().join(&file);

            if !path.is_file() {
                fail(format!(
                    "{PROFILE_FILE_VAR}='{file}' does not exist (looked at {}).",
                    path.display()
                ));
            }

            Some(Profile {
                name: profile_name_from_path(&path),
                path,
                origin: PROFILE_FILE_VAR.to_string(),
            })
        }

        (Some(name), Some(file)) => fail(format!(
            "{PROFILE_VAR}='{name}' and {PROFILE_FILE_VAR}='{file}' are both set. Pick one -- \
             there is no sensible precedence between \"the profile named X\" and \"the profile \
             at path Y\"."
        )),

        (None, None) => None,
    }
}

fn profile_name_from_path(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "custom".to_string())
}

/// Parses shell-style `KEY=value` text, with the same syntax `src/conf.rs` accepts at runtime:
/// `#` comments, blank lines, optional surrounding quotes, last assignment wins.
///
/// Stricter in one direction: an unknown key is an error here. At runtime an unknown environment
/// variable is just an environment variable, but in a file whose entire purpose is to set these
/// keys, a key nothing reads is a typo, and a typo that builds is a robot with the wrong defaults.
fn read_profile(path: &Path) -> BTreeMap<String, String> {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|err| fail(format!("reading {}: {err}", path.display())));

    let mut values = BTreeMap::new();

    for (index, line) in text.lines().enumerate() {
        let line = line.trim();

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            fail(format!(
                "{}:{}: '{line}' is not a KEY=value assignment, a comment, or blank.",
                path.display(),
                index + 1
            ));
        };

        let key = key.trim().to_string();

        if !KNOBS.iter().any(|knob| knob.var == key) {
            fail(format!(
                "{}:{}: '{key}' is not a robotctl setting. Known keys: {}.",
                path.display(),
                index + 1,
                KNOBS
                    .iter()
                    .map(|knob| knob.var)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        values.insert(key, strip_quotes(value.trim()).to_string());
    }

    values
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

fn generate(profile: Option<&Profile>, overrides: &BTreeMap<String, String>) -> String {
    let version = env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");

    let (name, source) = match profile {
        Some(profile) => (
            profile.name.as_str(),
            format!("{} (via {})", profile.path.display(), profile.origin),
        ),
        None => ("built-in", "no profile selected".to_string()),
    };

    let mut out = String::new();

    let _ = writeln!(
        out,
        "// @generated by build.rs -- do not edit.\n\
         // Profile: {name}\n\
         // Source:  {source}\n"
    );

    let _ = writeln!(
        out,
        "/// Version string for `--version`, naming the profile this binary was built with, \
         because\n\
         /// \"which defaults does the flashed image have\" is otherwise unanswerable from the \
         robot.\n\
         pub const LONG_VERSION: &str = {:?};\n",
        format!("{version} (profile: {name})")
    );

    for knob in KNOBS {
        let raw = overrides
            .get(knob.var)
            .map(String::as_str)
            .unwrap_or(knob.value);

        let from_profile = overrides.contains_key(knob.var);
        let (ty, literal) = render(knob, raw, profile);

        let _ = writeln!(
            out,
            "/// {}\n\
             ///\n\
             /// Overridable at runtime with `{}`. {}\n\
             pub const {}: {ty} = {literal};\n",
            knob.doc,
            knob.var,
            if from_profile {
                format!("Set by the `{name}` profile.")
            } else {
                "Built-in default.".to_string()
            },
            knob.konst,
        );
    }

    out
}

/// Validates one value and renders it as a Rust type and literal.
///
/// The `RobotId` / `RosDomainId` rules are restated here because a build script cannot use the
/// crate it builds. `config`'s `compiled_in_defaults_are_valid` test runs the real constructors
/// over these constants, so a divergence between the two fails `cargo test` rather than a robot.
fn render(knob: &Knob, raw: &str, profile: Option<&Profile>) -> (&'static str, String) {
    let reject = |reason: String| -> ! {
        match profile {
            Some(profile) => fail(format!(
                "{}: {}='{raw}' {reason}",
                profile.path.display(),
                knob.var
            )),
            None => fail(format!(
                "built-in default for {}='{raw}' {reason}",
                knob.var
            )),
        }
    };

    match knob.kind {
        Kind::Str => {
            if raw.is_empty() {
                reject("must not be empty.".to_string());
            }
            ("&str", format!("{raw:?}"))
        }

        Kind::OptionalStr => ("&str", format!("{raw:?}")),

        Kind::RobotId => {
            for (pos, char) in raw.chars().enumerate() {
                if char.is_ascii_uppercase() {
                    reject(format!(
                        "must be lowercase; found '{char}' at position {pos}."
                    ));
                }

                if !char.is_ascii_lowercase() && !char.is_ascii_digit() && char != '-' {
                    reject(format!(
                        "may only contain letters, digits and hyphens; found '{char}' at \
                         position {pos}."
                    ));
                }
            }

            if raw.is_empty() || raw.len() > 63 {
                reject(format!(
                    "must be between 1 and 63 characters long; it is {}.",
                    raw.len()
                ));
            }

            if raw.starts_with('-') || raw.ends_with('-') {
                reject("must not start or end with a hyphen.".to_string());
            }

            ("&str", format!("{raw:?}"))
        }

        Kind::RosDomainId => {
            let value = raw
                .parse::<u16>()
                .unwrap_or_else(|_| reject("is not an integer.".to_string()));

            if value > 232 {
                reject("must be between 0 and 232 inclusive.".to_string());
            }

            ("u8", value.to_string())
        }

        Kind::U32 => {
            let value = raw
                .parse::<u32>()
                .unwrap_or_else(|_| reject("is not a non-negative integer.".to_string()));

            ("u32", value.to_string())
        }
    }
}

fn available_profiles() -> String {
    let Ok(entries) = fs::read_dir(manifest_dir().join(PROFILE_DIR)) else {
        return "none".to_string();
    };

    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "conf"))
        .map(|path| profile_name_from_path(&path))
        .collect();

    names.sort();

    if names.is_empty() {
        return "none".to_string();
    }

    names.join(", ")
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"))
}

fn non_empty(var: &str) -> Option<String> {
    env::var(var)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Fails the build with one legible line. `cargo::error` rather than a panic: the message is the
/// whole point, and a panic buries it under a backtrace note.
fn fail(message: String) -> ! {
    println!("cargo::error={message}");
    process::exit(1);
}
