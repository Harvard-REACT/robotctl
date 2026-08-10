//! The thin boundary between the WiFi state machine and `ip`/`iw`.

use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// What the driver reports the interface is currently doing.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterfaceMode {
    /// Station mode — associated (or trying to associate) with someone else's AP.
    Client,
    /// This robot is the access point.
    AccessPoint,
    /// `iw` reported a type we don't drive (monitor, mesh, ...).
    Other,
}

/// Runs a command, returning its stdout on success and a useful error on failure.
pub fn output(program: &str, args: &[&str]) -> Result<String> {
    let result = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("could not run `{program}` (is it installed?)"))?;

    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        bail!(
            "`{program} {}` failed ({}): {}",
            args.join(" "),
            result.status,
            stderr.trim()
        );
    }

    Ok(String::from_utf8_lossy(&result.stdout).into_owned())
}

/// Runs a command for its effect, discarding stdout.
pub fn run(program: &str, args: &[&str]) -> Result<()> {
    output(program, args).map(|_| ())
}

/// Runs a command whose failure is not worth aborting for — teardown steps, mostly, where the
/// thing being torn down may already be gone. Mirrors the shell's trailing `|| true`.
pub fn run_best_effort(program: &str, args: &[&str]) {
    if let Err(err) = run(program, args) {
        crate::log::warn(format!("{err:#}"));
    }
}

pub fn interface_exists(interface: &str) -> bool {
    output("ip", &["link", "show", interface]).is_ok()
}

/// Errors unless the interface exists, naming the knob to fix if it doesn't.
pub fn require_interface(interface: &str) -> Result<()> {
    if !interface_exists(interface) {
        bail!(
            "WiFi interface '{interface}' does not exist. \
             Set ROBOTCTL_WIFI_INTERFACE to the correct NIC."
        );
    }
    Ok(())
}

pub fn flush_addresses(interface: &str) -> Result<()> {
    run("ip", &["addr", "flush", "dev", interface])
}

pub fn link_up(interface: &str) -> Result<()> {
    run("ip", &["link", "set", interface, "up"])
}

pub fn link_down(interface: &str) -> Result<()> {
    run("ip", &["link", "set", interface, "down"])
}

pub fn add_address(interface: &str, address: &str) -> Result<()> {
    run("ip", &["addr", "add", address, "dev", interface])
}

/// The interface's current IPv4 address, if it has one.
pub fn ipv4_address(interface: &str) -> Option<String> {
    let text = output("ip", &["-4", "addr", "show", "dev", interface]).ok()?;
    parse_inet_address(&text)
}

pub fn interface_mode(interface: &str) -> Option<InterfaceMode> {
    let text = output("iw", &["dev", interface, "info"]).ok()?;
    parse_iw_type(&text)
}

/// The SSID this interface is currently associated with, in client mode.
pub fn associated_ssid(interface: &str) -> Option<String> {
    let text = output("iw", &["dev", interface, "link"]).ok()?;
    parse_iw_link_ssid(&text)
}

/// Extracts `inet <addr>/<prefix>` from `ip -4 addr show` output.
fn parse_inet_address(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("inet "))
        .and_then(|rest| rest.split_whitespace().next())
        .map(str::to_string)
}

/// Extracts the `type` line from `iw dev <if> info` output.
fn parse_iw_type(text: &str) -> Option<InterfaceMode> {
    let kind = text
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("type "))?;

    Some(match kind.trim() {
        "managed" | "station" => InterfaceMode::Client,
        "AP" => InterfaceMode::AccessPoint,
        _ => InterfaceMode::Other,
    })
}

/// Extracts the SSID from `iw dev <if> link` output, or `None` when not associated.
fn parse_iw_link_ssid(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("SSID: "))
        .map(|ssid| ssid.trim().to_string())
        .filter(|ssid| !ssid.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_inet_address() {
        let text = "3: wlan-ctrl: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc noqueue state UP\n   \
                    \x20inet 192.168.1.50/24 brd 192.168.1.255 scope global dynamic wlan-ctrl\n       \
                    valid_lft 84456sec preferred_lft 84456sec\n";

        assert_eq!(parse_inet_address(text).as_deref(), Some("192.168.1.50/24"));
    }

    #[test]
    fn reports_no_address_when_the_interface_has_none() {
        // `ip -4 addr show` still prints the link line for an interface with no address, so
        // "command succeeded" is not the same as "has an address".
        let text =
            "3: wlan-ctrl: <BROADCAST,MULTICAST> mtu 1500 qdisc noop state DOWN group default\n";

        assert_eq!(parse_inet_address(text), None);
    }

    #[test]
    fn parses_the_ap_address_form() {
        let text = "3: wlan-ctrl: <BROADCAST,MULTICAST,UP> mtu 1500\n    \
                    inet 192.168.50.1/24 scope global wlan-ctrl\n";

        assert_eq!(parse_inet_address(text).as_deref(), Some("192.168.50.1/24"));
    }

    #[test]
    fn parses_iw_interface_types() {
        let managed = "Interface wlan-ctrl\n\tifindex 3\n\twdev 0x1\n\taddr aa:bb:cc:dd:ee:ff\n\t\
                       ssid REACTDREAM\n\ttype managed\n\twiphy 0\n";
        let ap = "Interface wlan-ctrl\n\tifindex 3\n\taddr aa:bb:cc:dd:ee:ff\n\tssid gopigo-07-setup\n\t\
                  type AP\n\twiphy 0\n";
        let monitor = "Interface wlan-ctrl\n\tifindex 3\n\ttype monitor\n";

        assert_eq!(parse_iw_type(managed), Some(InterfaceMode::Client));
        assert_eq!(parse_iw_type(ap), Some(InterfaceMode::AccessPoint));
        assert_eq!(parse_iw_type(monitor), Some(InterfaceMode::Other));
        assert_eq!(parse_iw_type("Interface wlan-ctrl\n"), None);
    }

    #[test]
    fn parses_the_associated_ssid() {
        let connected = "Connected to aa:bb:cc:dd:ee:ff (on wlan-ctrl)\n\tSSID: REACTDREAM\n\t\
                         freq: 2437\n\tsignal: -50 dBm\n\ttx bitrate: 72.2 MBit/s\n";

        assert_eq!(parse_iw_link_ssid(connected).as_deref(), Some("REACTDREAM"));
        assert_eq!(parse_iw_link_ssid("Not connected.\n"), None);
    }
}
