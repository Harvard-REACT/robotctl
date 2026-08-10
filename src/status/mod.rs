//! `robotctl status` — one report describing what this robot currently is and is doing.

mod json;
mod text;

use std::ffi::CString;
use std::fs;
use std::path::Path;

use anyhow::Result;
use serde::{Serialize, Serializer};

use crate::conf;
use crate::config::Paths;
use crate::id;
use crate::wifi::config_gen;
use crate::wifi::net::{self, InterfaceMode};

/// Units worth reporting whether or not they exist.
const WATCHED_UNITS: &[&str] = &[
    "robotctl-wifi.service",
    "robot-wifi-supervisor.service",
    "robot-wifi-client.service",
    "robot-wifi-fallback-ap.service",
    "docker.service",
    "robot-experiments.service",
    "robot-update-health-check.service",
];

/// Filesystems worth reporting, in the order someone debugging would want them.
const WATCHED_MOUNTS: &[&str] = &["/", "/data", "/boot"];

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub identity: Identity,
    /// Key/value pairs in the order the build wrote them, not a map, so that order survives.
    /// Serialized *as* a JSON object by [`pairs_as_map`] — a `Vec` of pairs would otherwise come
    /// out as an array of two-element arrays.
    #[serde(serialize_with = "pairs_as_map")]
    pub image: Vec<(String, String)>,
    pub system: System,
    pub rauc: RaucSummary,
    pub filesystems: Vec<Filesystem>,
    pub wifi: WifiSummary,
    pub network: Network,
    #[serde(serialize_with = "units_as_map")]
    pub units: Vec<Unit>,
    pub failed_units: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Identity {
    pub robot_id: String,
    pub ros_domain_id: u16,
    pub hostname: Option<String>,
    /// False when the running hostname disagrees with `robot.conf` — i.e. `id apply` has not run.
    pub hostname_matches: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct System {
    pub kernel: Option<String>,
    pub uptime_seconds: Option<u64>,
    pub time_utc: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Network {
    pub interfaces: Vec<Interface>,
    pub default_route: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Filesystem {
    pub mount: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Interface {
    pub name: String,
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WifiSummary {
    pub interface: String,
    pub exists: bool,
    pub mode: Option<String>,
    pub ssid: Option<String>,
    pub security: Option<String>,
    pub address: Option<String>,
    pub client_conf_present: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RaucSummary {
    /// The slot actually running, from the kernel command line.
    pub booted_slot: Option<String>,
    /// The slot a plain reboot would land on, from the tryboot backend.
    pub primary_slot: Option<String>,
    #[serde(serialize_with = "pairs_as_map")]
    pub slot_states: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Unit {
    pub name: String,
    pub state: String,
}

/// Serializes ordered pairs as a JSON object, preserving insertion order.
///
/// `collect_map` writes entries in iteration order, which a `BTreeMap` field would not — and
/// `image`'s order is the order the build wrote its keys in, which is worth keeping.
fn pairs_as_map<S: Serializer>(
    pairs: &[(String, String)],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_map(pairs.iter().map(|(key, value)| (key, value)))
}

fn units_as_map<S: Serializer>(units: &[Unit], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_map(units.iter().map(|unit| (&unit.name, &unit.state)))
}

pub fn print(paths: &Paths, as_json: bool) -> Result<()> {
    let status = gather(paths);

    if as_json {
        println!("{}", json::render(&status)?);
    } else {
        print!("{}", text::render(&status));
    }

    Ok(())
}

/// Probes the system. Never fails as a whole — each probe degrades to `None` on its own.
pub fn gather(paths: &Paths) -> Status {
    let identity = id::load(paths).ok();
    let hostname = read_trimmed(Path::new("/proc/sys/kernel/hostname"));

    let robot_id = identity
        .as_ref()
        .map(|identity| identity.robot_id.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    Status {
        identity: Identity {
            hostname_matches: hostname.as_deref() == Some(robot_id.as_str()),
            robot_id,
            ros_domain_id: identity
                .as_ref()
                .map(|identity| identity.ros_domain_id.value().into())
                .unwrap_or(0),
            hostname,
        },
        image: image_version(paths.image_version()),
        system: System {
            kernel: read_trimmed(Path::new("/proc/sys/kernel/osrelease")),
            uptime_seconds: uptime_seconds(),
            time_utc: time_utc(),
        },
        rauc: rauc(paths),
        filesystems: filesystems(),
        wifi: wifi(paths),
        network: Network {
            interfaces: interfaces(),
            default_route: default_route(),
        },
        units: units(),
        failed_units: failed_units(),
    }
}

/// Reads `/etc/robot/image-version`, preserving the order the build wrote its keys in.
fn image_version(path: &Path) -> Vec<(String, String)> {
    let Ok(Some(text)) = conf::read_optional(path) else {
        return Vec::new();
    };

    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect()
}

fn uptime_seconds() -> Option<u64> {
    parse_uptime(&fs::read_to_string("/proc/uptime").ok()?)
}

/// `/proc/uptime` is "<uptime> <idle>", both in seconds with two decimals.
fn parse_uptime(text: &str) -> Option<u64> {
    text.split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .map(|s| s as u64)
}

fn filesystems() -> Vec<Filesystem> {
    WATCHED_MOUNTS
        .iter()
        .filter_map(|mount| statvfs(mount))
        .collect()
}

/// Space on one mount point, via `statvfs(3)`.
///
/// Direct rather than parsing `df` output: `df`'s human-readable columns are already rounded, so
/// re-deriving exact byte counts from them for `--json` would be lossy.
fn statvfs(mount: &str) -> Option<Filesystem> {
    let path = CString::new(mount).ok()?;
    let mut raw: libc::statvfs = unsafe { std::mem::zeroed() };

    // Safety: `path` is a valid NUL-terminated C string that outlives the call, and `raw` is a
    // properly aligned, fully initialised `statvfs` the kernel may write into.
    if unsafe { libc::statvfs(path.as_ptr(), &mut raw) } != 0 {
        return None;
    }

    // f_frsize is the fragment size, which is what f_blocks and f_bavail are counted in.
    let unit = raw.f_frsize as u64;

    Some(Filesystem {
        mount: mount.to_string(),
        total_bytes: raw.f_blocks as u64 * unit,
        // f_bavail, not f_bfree: the latter includes root-reserved blocks that ordinary writes
        // cannot use, which would overstate how much room a robot actually has for logs.
        available_bytes: raw.f_bavail as u64 * unit,
    })
}

fn interfaces() -> Vec<Interface> {
    net::output("ip", &["-o", "-4", "addr", "show"])
        .map(|text| parse_ip_addresses(&text))
        .unwrap_or_default()
}

fn parse_ip_addresses(text: &str) -> Vec<Interface> {
    let mut interfaces: Vec<Interface> = Vec::new();

    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (_index, name) = (fields.next(), fields.next());
        let Some(name) = name else { continue };

        let Some(address) = fields.by_ref().skip_while(|field| *field != "inet").nth(1) else {
            continue;
        };

        match interfaces.iter_mut().find(|iface| iface.name == name) {
            Some(existing) => existing.addresses.push(address.to_string()),
            None => interfaces.push(Interface {
                name: name.to_string(),
                addresses: vec![address.to_string()],
            }),
        }
    }

    interfaces
}

fn default_route() -> Option<String> {
    let text = net::output("ip", &["route", "show", "default"]).ok()?;
    parse_default_route(&text)
}

fn parse_default_route(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| line.starts_with("default "))
        .map(str::to_string)
}

fn wifi(paths: &Paths) -> WifiSummary {
    let interface = paths.wifi_interface().to_string();
    let exists = net::interface_exists(&interface);

    let mode = exists.then(|| net::interface_mode(&interface)).flatten();

    let security = match mode {
        Some(InterfaceMode::AccessPoint) => id::load(paths)
            .ok()
            .and_then(|identity| config_gen::load_ap_config(paths, &identity.robot_id).ok())
            .map(|config| config.security().to_string()),
        _ => None,
    };

    WifiSummary {
        ssid: match mode {
            Some(InterfaceMode::Client) => net::associated_ssid(&interface),
            Some(InterfaceMode::AccessPoint) => id::load(paths)
                .ok()
                .and_then(|identity| config_gen::load_ap_config(paths, &identity.robot_id).ok())
                .map(|config| config.ssid.to_string()),
            _ => None,
        },
        mode: mode.map(|mode| {
            match mode {
                InterfaceMode::Client => "client",
                InterfaceMode::AccessPoint => "fallback AP",
                InterfaceMode::Other => "other",
            }
            .to_string()
        }),
        address: exists.then(|| net::ipv4_address(&interface)).flatten(),
        client_conf_present: paths.wifi_client_conf().is_file(),
        interface,
        exists,
        security,
    }
}

fn rauc(paths: &Paths) -> RaucSummary {
    let booted_slot = fs::read_to_string("/proc/cmdline")
        .ok()
        .and_then(|cmdline| parse_rauc_slot(&cmdline));

    let backend = paths.tryboot_backend();
    let call = |args: &[&str]| -> Option<String> {
        if !backend.exists() {
            return None;
        }
        net::output(&backend.to_string_lossy(), args)
            .ok()
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
    };

    let primary_slot = call(&["get-primary"]);

    // Only the read-only verbs, and only for the two slots this layout defines.
    let slot_states = ["A", "B"]
        .iter()
        .filter_map(|slot| call(&["get-state", slot]).map(|state| (slot.to_string(), state)))
        .collect();

    RaucSummary {
        booted_slot,
        primary_slot,
        slot_states,
    }
}

/// Extracts `rauc.slot=<bootname>` from the kernel command line.
///
/// The command line is authoritative for "what actually booted" in a way nothing in userspace
/// is: it was fixed by the bootloader before any of this ran, so it cannot have been changed by
/// an update that happened since.
fn parse_rauc_slot(cmdline: &str) -> Option<String> {
    cmdline
        .split_whitespace()
        .find_map(|field| field.strip_prefix("rauc.slot="))
        .map(str::to_string)
        .filter(|slot| !slot.is_empty())
}

fn units() -> Vec<Unit> {
    WATCHED_UNITS
        .iter()
        .map(|name| Unit {
            name: name.to_string(),
            state: crate::systemd::is_active(name),
        })
        .collect()
}

fn failed_units() -> Vec<String> {
    net::output(
        "systemctl",
        &["list-units", "--state=failed", "--no-legend", "--plain"],
    )
    .map(|text| parse_failed_units(&text))
    .unwrap_or_default()
}

fn parse_failed_units(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| name.contains('.'))
        .map(str::to_string)
        .collect()
}

/// System time in UTC, via `date`.
///
/// Shelled out rather than formatted here: turning a `SystemTime` into a calendar date means
/// implementing civil-date arithmetic, and this is a display string in a status report.
fn time_utc() -> Option<String> {
    net::output("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .ok()
        .map(|text| text.trim().to_string())
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A fully-populated `Status` describing a healthy robot.
    ///
    /// Shared with the `text` and `json` renderer tests: the point of separating gathering from
    /// formatting is that both renderers can be exercised against a fixed input with no robot,
    /// no root, and no processes involved.
    pub(super) fn example_status() -> Status {
        Status {
            identity: Identity {
                robot_id: "gopigo-07".to_string(),
                ros_domain_id: 7,
                hostname: Some("gopigo-07".to_string()),
                hostname_matches: true,
            },
            image: vec![
                ("image_basename".to_string(), "gopigo-image".to_string()),
                ("distro_version".to_string(), "0.1-dev".to_string()),
            ],
            system: System {
                kernel: Some("6.12.0-rpi".to_string()),
                uptime_seconds: Some(90_000),
                time_utc: Some("2026-08-10T12:00:00Z".to_string()),
            },
            rauc: RaucSummary {
                booted_slot: Some("A".to_string()),
                primary_slot: Some("A".to_string()),
                slot_states: vec![
                    ("A".to_string(), "good".to_string()),
                    ("B".to_string(), "good".to_string()),
                ],
            },
            filesystems: vec![Filesystem {
                mount: "/data".to_string(),
                total_bytes: 4 * (1 << 30),
                available_bytes: 1 << 30,
            }],
            wifi: WifiSummary {
                interface: "wlan-ctrl".to_string(),
                exists: true,
                mode: Some("client".to_string()),
                ssid: Some("REACTDREAM".to_string()),
                security: None,
                address: Some("192.168.1.50/24".to_string()),
                client_conf_present: true,
            },
            network: Network {
                interfaces: vec![Interface {
                    name: "wlan-ctrl".to_string(),
                    addresses: vec!["192.168.1.50/24".to_string()],
                }],
                default_route: Some("default via 192.168.1.1 dev wlan-ctrl".to_string()),
            },
            units: vec![Unit {
                name: "robotctl-wifi.service".to_string(),
                state: "active".to_string(),
            }],
            failed_units: vec!["broken.service".to_string()],
        }
    }

    #[test]
    fn parses_the_booted_slot_from_the_kernel_command_line() {
        let cmdline = "console=serial0,115200 root=/dev/mmcblk0p2 rauc.slot=A rootwait\n";
        assert_eq!(parse_rauc_slot(cmdline).as_deref(), Some("A"));

        let slot_b = "root=/dev/mmcblk0p3 rauc.slot=B quiet\n";
        assert_eq!(parse_rauc_slot(slot_b).as_deref(), Some("B"));
    }

    #[test]
    fn reports_no_slot_when_the_command_line_has_none() {
        // A non-A/B build (qemuarm64) has no rauc.slot at all, and must not be reported as if
        // it were running slot "".
        assert_eq!(parse_rauc_slot("console=ttyAMA0 root=/dev/sda2\n"), None);
        assert_eq!(parse_rauc_slot("rauc.slot= quiet\n"), None);
    }

    #[test]
    fn parses_uptime() {
        assert_eq!(parse_uptime("12345.67 98765.43\n"), Some(12345));
        assert_eq!(parse_uptime("0.42 0.11\n"), Some(0));
        assert_eq!(parse_uptime("garbage\n"), None);
    }

    #[test]
    fn parses_ip_addresses_grouping_by_interface() {
        let text = "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever\n\
                    3: wlan-ctrl    inet 192.168.1.50/24 brd 192.168.1.255 scope global dynamic wlan-ctrl\\  valid_lft 8000sec\n\
                    3: wlan-ctrl    inet 10.0.0.5/24 scope global secondary wlan-ctrl\\       valid_lft forever\n";

        let interfaces = parse_ip_addresses(text);

        assert_eq!(interfaces.len(), 2);
        assert_eq!(interfaces[0].name, "lo");
        assert_eq!(interfaces[0].addresses, ["127.0.0.1/8"]);
        assert_eq!(interfaces[1].name, "wlan-ctrl");
        assert_eq!(interfaces[1].addresses, ["192.168.1.50/24", "10.0.0.5/24"]);
    }

    #[test]
    fn parses_the_default_route() {
        let text = "default via 192.168.1.1 dev wlan-ctrl proto dhcp metric 600\n";

        assert_eq!(
            parse_default_route(text).as_deref(),
            Some("default via 192.168.1.1 dev wlan-ctrl proto dhcp metric 600")
        );
        assert_eq!(parse_default_route(""), None);
    }

    #[test]
    fn parses_failed_units() {
        let text = "  robot-update-health-check.service loaded failed failed GoPiGo health gate\n  \
                    docker.service loaded failed failed Docker\n";

        assert_eq!(
            parse_failed_units(text),
            ["robot-update-health-check.service", "docker.service"]
        );
        assert_eq!(parse_failed_units(""), Vec::<String>::new());
    }
}
