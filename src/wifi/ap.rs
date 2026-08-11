//! Fallback access point: hostapd for the radio, dnsmasq for DHCP.

use anyhow::{Context, Result};

use crate::conf;
use crate::log;
use crate::wifi::config_gen::{ApConfig, render_dnsmasq_conf, render_hostapd_conf};
use crate::wifi::daemon::Daemon;
use crate::wifi::net;

/// A running fallback AP. Dropping it stops both daemons.
pub struct ApSession {
    hostapd: Daemon,
    dnsmasq: Daemon,
    interface: String,
}

impl ApSession {
    /// Both daemons still alive. A dead hostapd means the AP is gone even though we think we are
    /// in AP mode.
    pub fn is_healthy(&mut self) -> bool {
        let hostapd = self.hostapd.is_running();
        let dnsmasq = self.dnsmasq.is_running();
        hostapd && dnsmasq
    }
}

impl Drop for ApSession {
    fn drop(&mut self) {
        self.hostapd.stop();
        self.dnsmasq.stop();
        net::run_best_effort("ip", &["addr", "flush", "dev", &self.interface]);
    }
}

/// Generates the daemon configs, configures the interface, and starts hostapd and dnsmasq.
pub fn start(run_dir: &std::path::Path, config: &ApConfig) -> Result<ApSession> {
    net::require_interface(&config.interface)?;

    let hostapd_conf = run_dir.join("hostapd.conf");
    let dnsmasq_conf = run_dir.join("dnsmasq.conf");

    conf::write_atomically(&hostapd_conf, &render_hostapd_conf(config))
        .with_context(|| format!("writing {}", hostapd_conf.display()))?;
    conf::write_atomically(&dnsmasq_conf, &render_dnsmasq_conf(config))
        .with_context(|| format!("writing {}", dnsmasq_conf.display()))?;

    let address = format!("{}/{}", config.address, config.netmask_cidr);
    net::flush_addresses(&config.interface)?;
    net::add_address(&config.interface, &address)?;
    net::link_up(&config.interface)?;

    log::info("starting fallback AP:");
    log::info(format!("  interface: {}", config.interface));
    log::info(format!("  ssid:      {}", config.ssid));
    log::info(format!("  security:  {}", config.security()));
    log::info(format!("  address:   {address}"));
    log::info(format!(
        "  dhcp:      {} - {}",
        config.dhcp_range_start, config.dhcp_range_end
    ));

    // dnsmasq first: if hostapd comes up and a client associates before DHCP is listening, that
    // client waits out a lease timeout for no reason.
    let dnsmasq_args = dnsmasq_args(&dnsmasq_conf);
    let dnsmasq = Daemon::spawn(
        "dnsmasq",
        "dnsmasq",
        &dnsmasq_args.iter().map(String::as_str).collect::<Vec<_>>(),
    )?;

    let hostapd = Daemon::spawn("hostapd", "hostapd", &[&hostapd_conf.to_string_lossy()])?;

    Ok(ApSession {
        hostapd,
        dnsmasq,
        interface: config.interface.clone(),
    })
}

fn dnsmasq_args(conf: &std::path::Path) -> Vec<String> {
    vec![
        "--keep-in-foreground".to_string(),
        format!("--conf-file={}", conf.display()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn the_conf_file_is_passed_with_an_equals_sign() {
        let args = dnsmasq_args(Path::new("/run/robot-wifi/dnsmasq.conf"));

        assert_eq!(
            args,
            [
                "--keep-in-foreground",
                "--conf-file=/run/robot-wifi/dnsmasq.conf"
            ]
        );

        // The bug this replaced: a bare `--conf-file` with the path as its own argument.
        assert!(
            !args.iter().any(|arg| arg == "--conf-file"),
            "the path must be attached with '=', not passed as a separate argument"
        );
    }
}
