//! WiFi: client mode, the fallback access point, and the supervisor that switches between them.

pub mod ap;
pub mod client;
pub mod config_gen;
pub mod daemon;
pub mod net;
pub mod supervise;

use anyhow::Result;

use crate::config::{Paths, SuperviseTuning};
use crate::id;
use crate::log;
use crate::systemd;
use crate::wifi::net::InterfaceMode;

/// Backs `robotctl wifi start`. Long-running: it returns only on error.
pub fn start(paths: &Paths) -> Result<()> {
    supervise::run(paths, &SuperviseTuning::from_env())
}

/// Backs `robotctl wifi stop`: stops the supervisor, then puts the interface down.
pub fn stop(paths: &Paths) -> Result<()> {
    let unit = paths.wifi_unit();
    let interface = paths.wifi_interface();

    match systemd::is_active(unit).as_str() {
        "active" | "activating" => {
            log::info(format!("stopping {unit}"));
            systemd::stop(unit)?;
        }
        // A supervisor started by hand is not something this can reach: it is not in a cgroup
        // systemd will tear down, and hunting for `wpa_supplicant` by name risks killing a
        // process that has nothing to do with this robot's control interface.
        state => log::info(format!(
            "{unit} is {state}; if `robotctl wifi start` is running by hand, stop it there"
        )),
    }

    if !net::interface_exists(interface) {
        log::info(format!("{interface} does not exist; nothing to tear down"));
        return Ok(());
    }

    net::flush_addresses(interface)?;
    net::link_down(interface)?;
    log::info(format!("{interface} is down"));

    Ok(())
}

pub fn print_status(paths: &Paths) -> Result<()> {
    let interface = paths.wifi_interface();

    println!("interface:     {interface}");

    if !net::interface_exists(interface) {
        println!("state:         interface does not exist");
        println!("               (set ROBOTCTL_WIFI_INTERFACE to the correct NIC)");
        return Ok(());
    }

    let mode = net::interface_mode(interface);
    println!(
        "mode:          {}",
        match mode {
            Some(InterfaceMode::Client) => "client",
            Some(InterfaceMode::AccessPoint) => "fallback AP",
            Some(InterfaceMode::Other) => "other (not driven by robotctl)",
            None => "unknown (is `iw` installed?)",
        }
    );

    match mode {
        Some(InterfaceMode::Client) => match net::associated_ssid(interface) {
            Some(ssid) => println!("ssid:          {ssid}"),
            None => println!("ssid:          not associated"),
        },

        Some(InterfaceMode::AccessPoint) => {
            let identity = id::load(paths)?;
            match config_gen::load_ap_config(paths, &identity.robot_id) {
                Ok(config) => {
                    println!("ssid:          {}", config.ssid);
                    println!("security:      {}", config.security());
                }
                Err(err) => println!("ap config:     unreadable ({err:#})"),
            }
        }

        _ => {}
    }

    match net::ipv4_address(interface) {
        Some(address) => println!("address:       {address}"),
        None => println!("address:       none"),
    }

    let client_conf = paths.wifi_client_conf();
    println!(
        "client.conf:   {}",
        if client_conf.is_file() {
            client_conf.display().to_string()
        } else {
            format!("{} (missing)", client_conf.display())
        }
    );

    Ok(())
}
