//! Client (station) mode: associate with a configured network and get a DHCP lease.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::config::{Paths, SuperviseTuning};
use crate::log;
use crate::wifi::daemon::Daemon;
use crate::wifi::net;

/// udhcpc's per-attempt timeout, in seconds. The number of attempts is derived from the overall
/// connect timeout so that the two knobs stay consistent — same arithmetic as the shell.
const DHCP_ATTEMPT_TIMEOUT_SEC: u32 = 5;

/// Why a client attempt did not result in a working connection.
///
/// `NoConfig` is separated from the rest because it is not a failure to retry: without
/// credentials, trying again in five seconds cannot possibly work, and the supervisor should go
/// straight to the fallback AP.
#[derive(Debug)]
pub enum ClientFailure {
    NoConfig { path: PathBuf },
    Failed(anyhow::Error),
}

/// A live client connection: the wpa_supplicant process plus the address we ended up with.
pub struct ClientSession {
    /// Held so wpa_supplicant stays alive for as long as the connection is meant to;
    /// dropping this session stops it.
    _supplicant: Daemon,
    pub address: String,
}

/// Brings the interface up as a station and waits for a DHCP lease.
pub fn connect(paths: &Paths, tuning: &SuperviseTuning) -> Result<ClientSession, ClientFailure> {
    let interface = paths.wifi_interface();
    let config = paths.wifi_client_conf();

    if !config.is_file() {
        return Err(ClientFailure::NoConfig { path: config });
    }

    connect_inner(tuning, interface, &config).map_err(ClientFailure::Failed)
}

fn connect_inner(
    tuning: &SuperviseTuning,
    interface: &str,
    config: &Path,
) -> Result<ClientSession> {
    net::require_interface(interface)?;
    net::flush_addresses(interface)?;
    net::link_up(interface)?;

    let config = config.to_string_lossy().into_owned();
    let supplicant = Daemon::spawn(
        "wpa_supplicant",
        "wpa_supplicant",
        &["-i", interface, "-c", &config],
    )?;

    let attempts = tuning
        .connect_timeout_sec
        .div_ceil(DHCP_ATTEMPT_TIMEOUT_SEC)
        .max(1);

    let dhcp = net::run(
        "udhcpc",
        &[
            "-i",
            interface,
            "-n",
            "-q",
            "-T",
            &DHCP_ATTEMPT_TIMEOUT_SEC.to_string(),
            "-t",
            &attempts.to_string(),
        ],
    );

    if let Err(err) = dhcp {
        log::warn(format!(
            "DHCP did not obtain a lease on {interface}: {err:#}"
        ));
    }

    match net::ipv4_address(interface) {
        Some(address) => Ok(ClientSession {
            _supplicant: supplicant,
            address,
        }),
        None => bail!("associated but got no IPv4 address on {interface}"),
    }
}

pub fn teardown(interface: &str) {
    net::run_best_effort("ip", &["addr", "flush", "dev", interface]);
}
