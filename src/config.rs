//! Path and default resolution — the only module in this binary that reads `std::env`.

use std::env;
use std::path::{Path, PathBuf};

use crate::id::{RobotId, RosDomainId};
use crate::log;

const DEFAULT_DATA_DIR: &str = "/data";
const DEFAULT_CONFIG_DIR: &str = "/data/robot";
const DEFAULT_HOSTNAME_PATH: &str = "/etc/hostname";
const DEFAULT_HOSTS_PATH: &str = "/etc/hosts";

/// Generated hostapd/dnsmasq configs and pidfiles.
const DEFAULT_RUN_DIR: &str = "/run/robot-wifi";

const DEFAULT_WIFI_INTERFACE: &str = "wlan-ctrl";

/// The systemd unit running `robotctl wifi start`. Stage 6 ships it; `wifi stop` needs to know
/// what to stop, and `status` reports its health.
const DEFAULT_WIFI_UNIT: &str = "robotctl-wifi.service";

/// Written into the rootfs at build time by `gopigo-image.bb`, so it describes the slot that is
/// running, not the robot.
const DEFAULT_IMAGE_VERSION_PATH: &str = "/etc/robot/image-version";

const DEFAULT_TRYBOOT_BACKEND: &str = "/usr/libexec/rauc/tryboot-backend";

const DEFAULT_ROBOT_ID: &str = "unprovisioned";
const DEFAULT_ROS_DOMAIN_ID: u8 = 0;

/// Resolved configuration, built once at startup and passed down.
#[derive(Debug, Clone)]
pub struct Paths {
    data_dir: PathBuf,
    config_dir: PathBuf,
    hostname_path: PathBuf,
    hosts_path: PathBuf,
    wifi_run_dir: PathBuf,
    wifi_interface: String,
    wifi_unit: String,
    image_version_path: PathBuf,
    tryboot_backend: PathBuf,
    default_robot_id: RobotId,
    default_ros_domain_id: RosDomainId,
}

impl Paths {
    /// Resolves every knob from the environment, falling back to the documented default.
    ///
    /// Never fails: an unset variable takes the default silently, and a *malformed* one (an
    /// invalid `ROBOTCTL_DEFAULT_ROBOT_ID`, say) warns and takes the default too.
    pub fn from_env() -> Self {
        Paths {
            data_dir: path_from_env("ROBOTCTL_DATA_DIR", DEFAULT_DATA_DIR),
            config_dir: path_from_env("ROBOTCTL_CONFIG_DIR", DEFAULT_CONFIG_DIR),
            hostname_path: path_from_env("ROBOTCTL_HOSTNAME_PATH", DEFAULT_HOSTNAME_PATH),
            hosts_path: path_from_env("ROBOTCTL_HOSTS_PATH", DEFAULT_HOSTS_PATH),
            wifi_run_dir: path_from_env("ROBOTCTL_RUN_DIR", DEFAULT_RUN_DIR),
            wifi_interface: string_from_env("ROBOTCTL_WIFI_INTERFACE", DEFAULT_WIFI_INTERFACE),
            wifi_unit: string_from_env("ROBOTCTL_WIFI_UNIT", DEFAULT_WIFI_UNIT),
            image_version_path: path_from_env(
                "ROBOTCTL_IMAGE_VERSION_PATH",
                DEFAULT_IMAGE_VERSION_PATH,
            ),
            tryboot_backend: path_from_env("ROBOTCTL_TRYBOOT_BACKEND", DEFAULT_TRYBOOT_BACKEND),
            default_robot_id: default_robot_id_from_env(),
            default_ros_domain_id: default_ros_domain_id_from_env(),
        }
    }

    /// Identity: `ROBOT_ID` and `ROS_DOMAIN_ID`, in shell-sourceable `KEY=value` form.
    pub fn robot_conf(&self) -> PathBuf {
        self.config_dir.join("robot.conf")
    }

    pub fn hostname_path(&self) -> &Path {
        &self.hostname_path
    }

    pub fn hosts_path(&self) -> &Path {
        &self.hosts_path
    }

    pub fn wifi_client_conf(&self) -> PathBuf {
        self.config_dir.join("wifi/client.conf")
    }

    pub fn wifi_ap_conf(&self) -> PathBuf {
        self.config_dir.join("wifi/fallback-ap.conf")
    }

    pub fn experiments_dir(&self) -> PathBuf {
        self.data_dir.join("experiments")
    }

    pub fn wifi_run_dir(&self) -> &Path {
        &self.wifi_run_dir
    }

    pub fn wifi_interface(&self) -> &str {
        &self.wifi_interface
    }

    pub fn wifi_unit(&self) -> &str {
        &self.wifi_unit
    }

    pub fn image_version(&self) -> &Path {
        &self.image_version_path
    }

    pub fn tryboot_backend(&self) -> &Path {
        &self.tryboot_backend
    }

    pub fn default_robot_id(&self) -> &RobotId {
        &self.default_robot_id
    }

    pub fn default_ros_domain_id(&self) -> RosDomainId {
        self.default_ros_domain_id
    }

    /// Builds a `Paths` rooted entirely inside `root` — no real system path is reachable from
    /// the result. Test-only on purpose: production code must go through [`Paths::from_env`].
    #[cfg(test)]
    pub fn for_test(root: &Path) -> Self {
        Paths {
            data_dir: root.join("data"),
            config_dir: root.join("data/robot"),
            hostname_path: root.join("etc/hostname"),
            hosts_path: root.join("etc/hosts"),
            wifi_run_dir: root.join("run/robot-wifi"),
            wifi_interface: DEFAULT_WIFI_INTERFACE.to_string(),
            wifi_unit: DEFAULT_WIFI_UNIT.to_string(),
            image_version_path: root.join("etc/robot/image-version"),
            tryboot_backend: root.join("usr/libexec/rauc/tryboot-backend"),
            default_robot_id: builtin_default_robot_id(),
            default_ros_domain_id: builtin_default_ros_domain_id(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SuperviseTuning {
    /// How long one client association + DHCP attempt may take before it counts as failed.
    pub connect_timeout_sec: u32,
    /// Consecutive failed client attempts before giving up and raising the fallback AP.
    pub client_attempts: u32,
    /// Pause between client attempts within one cycle.
    pub retry_sec: u32,
    /// How often to re-check that an established client connection is still up.
    pub link_poll_sec: u32,
    /// How often to drop the fallback AP and re-try the client network.
    ///
    /// Deliberately long: retrying means taking the AP down for the duration of the attempt,
    /// so a short interval would make the recovery AP repeatedly vanish under whoever is
    /// connected to it trying to fix the robot.
    pub ap_retry_sec: u32,
}

impl SuperviseTuning {
    pub fn from_env() -> Self {
        SuperviseTuning {
            connect_timeout_sec: u32_from_env("ROBOTCTL_WIFI_CONNECT_TIMEOUT_SEC", 45),
            client_attempts: u32_from_env("ROBOTCTL_WIFI_CLIENT_ATTEMPTS", 3).max(1),
            retry_sec: u32_from_env("ROBOTCTL_WIFI_RETRY_SEC", 5),
            link_poll_sec: u32_from_env("ROBOTCTL_WIFI_LINK_POLL_SEC", 10).max(1),
            ap_retry_sec: u32_from_env("ROBOTCTL_WIFI_AP_RETRY_SEC", 300).max(1),
        }
    }
}

pub fn systemd_notify_socket() -> Option<PathBuf> {
    let socket = env::var("NOTIFY_SOCKET").ok()?;

    if socket.is_empty() || socket.starts_with('@') {
        return None;
    }

    Some(PathBuf::from(socket))
}

fn path_from_env(var: &str, default: &str) -> PathBuf {
    match env::var(var) {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
        _ => PathBuf::from(default),
    }
}

fn string_from_env(var: &str, default: &str) -> String {
    match env::var(var) {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => default.to_string(),
    }
}

fn u32_from_env(var: &str, default: u32) -> u32 {
    let Ok(raw) = env::var(var) else {
        return default;
    };

    raw.trim().parse::<u32>().unwrap_or_else(|_| {
        log::warn(format!(
            "{var}='{raw}' is not a non-negative integer. Falling back to {default}."
        ));
        default
    })
}

fn default_robot_id_from_env() -> RobotId {
    let Ok(raw) = env::var("ROBOTCTL_DEFAULT_ROBOT_ID") else {
        return builtin_default_robot_id();
    };

    RobotId::new(&raw).unwrap_or_else(|err| {
        log::warn(format!(
            "ROBOTCTL_DEFAULT_ROBOT_ID='{raw}': {err} Falling back to '{DEFAULT_ROBOT_ID}'."
        ));
        builtin_default_robot_id()
    })
}

fn default_ros_domain_id_from_env() -> RosDomainId {
    let Ok(raw) = env::var("ROBOTCTL_DEFAULT_ROS_DOMAIN_ID") else {
        return builtin_default_ros_domain_id();
    };

    raw.parse::<RosDomainId>().unwrap_or_else(|err| {
        log::warn(format!(
            "ROBOTCTL_DEFAULT_ROS_DOMAIN_ID='{raw}': {err} \
             Falling back to {DEFAULT_ROS_DOMAIN_ID}."
        ));
        builtin_default_ros_domain_id()
    })
}

fn builtin_default_robot_id() -> RobotId {
    RobotId::new(DEFAULT_ROBOT_ID).expect("compiled-in DEFAULT_ROBOT_ID must be a valid robot ID")
}

fn builtin_default_ros_domain_id() -> RosDomainId {
    RosDomainId::new(DEFAULT_ROS_DOMAIN_ID)
        .expect("compiled-in DEFAULT_ROS_DOMAIN_ID must be a valid ROS domain ID")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_in_defaults_are_valid() {
        assert_eq!(builtin_default_robot_id().as_str(), "unprovisioned");
        assert_eq!(builtin_default_ros_domain_id().value(), 0);
    }

    #[test]
    fn test_paths_stay_inside_the_given_root() {
        let paths = Paths::for_test(Path::new("/tmp/robotctl-example"));

        assert!(paths.robot_conf().starts_with("/tmp/robotctl-example"));
        assert!(paths.hostname_path().starts_with("/tmp/robotctl-example"));
        assert!(paths.hosts_path().starts_with("/tmp/robotctl-example"));
    }

    #[test]
    fn robot_conf_hangs_off_the_config_dir() {
        let paths = Paths::for_test(Path::new("/tmp/robotctl-example"));

        assert_eq!(
            paths.robot_conf(),
            Path::new("/tmp/robotctl-example/data/robot/robot.conf")
        );
    }
}
