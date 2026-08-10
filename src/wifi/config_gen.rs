//! Fallback-AP configuration: loading, validation, and hostapd/dnsmasq generation.

use std::fmt;
use std::net::Ipv4Addr;
use std::str::FromStr;

use anyhow::{Context, Result};
use thiserror::Error;

use crate::conf;
use crate::config::Paths;
use crate::id::RobotId;
use crate::log;

/// 802.11 caps the SSID at 32 octets.
const SSID_MAX_BYTES: usize = 32;

/// WPA2-PSK passphrase bounds, from the standard.
const PSK_MIN_CHARS: usize = 8;
const PSK_MAX_CHARS: usize = 63;

/// `hw_mode=g` is hardcoded in the generated config, so only the 2.4 GHz channels are usable.
const CHANNEL_MAX: u8 = 14;

const DEFAULT_SSID_SUFFIX: &str = "-setup";
const DEFAULT_ADDRESS: &str = "192.168.50.1";
const DEFAULT_NETMASK: &str = "255.255.255.0";
const DEFAULT_NETMASK_CIDR: u8 = 24;
const DEFAULT_DHCP_RANGE_START: &str = "192.168.50.10";
const DEFAULT_DHCP_RANGE_END: &str = "192.168.50.50";
const DEFAULT_DHCP_LEASE_TIME: &str = "12h";
const DEFAULT_CHANNEL: u8 = 6;
const DEFAULT_COUNTRY: &str = "US";

#[derive(Debug, Clone, Error)]
pub enum ApConfigError {
    #[error("AP_SSID must not be empty.")]
    EmptySsid,

    #[error("AP_SSID must be {SSID_MAX_BYTES} bytes or fewer. '{ssid}' is {len}.")]
    SsidTooLong { ssid: String, len: usize },

    #[error(
        "AP_PSK must be between {PSK_MIN_CHARS} and {PSK_MAX_CHARS} characters, or empty for an open network. Supplied passphrase is {len}."
    )]
    PskLength { len: usize },

    /// hostapd's config format is one directive per line, so an embedded newline in a value
    /// read from `fallback-ap.conf` would append arbitrary directives to the generated file.
    /// The shell had no equivalent check.
    #[error("{field} must not contain control characters.")]
    ControlCharacter { field: &'static str },

    #[error("{field}='{value}' is not a valid IPv4 address.")]
    NotAnIpv4Address { field: &'static str, value: String },

    #[error("{field}='{value}' is not a valid number.")]
    NotANumber { field: &'static str, value: String },

    #[error(
        "AP_CHANNEL must be between 1 and {CHANNEL_MAX} (the generated config is hw_mode=g, i.e. 2.4 GHz). Found {channel}."
    )]
    ChannelOutOfRange { channel: u16 },

    #[error("AP_NETMASK_CIDR must be between 1 and 32. Found {cidr}.")]
    CidrOutOfRange { cidr: u16 },

    #[error("{field} must not be empty.")]
    Empty { field: &'static str },
}

/// A validated SSID: non-empty, at most 32 octets, no control characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ssid(String);

impl Ssid {
    pub fn new(ssid: &str) -> Result<Self, ApConfigError> {
        if ssid.is_empty() {
            return Err(ApConfigError::EmptySsid);
        }

        if ssid.chars().any(char::is_control) {
            return Err(ApConfigError::ControlCharacter { field: "AP_SSID" });
        }

        // Bytes, not characters: the 802.11 limit is 32 octets, and the shell's `${#ssid}`
        // counted characters, which would have let a 32-character non-ASCII SSID through.
        if ssid.len() > SSID_MAX_BYTES {
            return Err(ApConfigError::SsidTooLong {
                ssid: ssid.to_string(),
                len: ssid.len(),
            });
        }

        Ok(Ssid(ssid.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Ssid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A validated WPA2 passphrase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Psk(String);

impl Psk {
    /// Empty input yields `None` — an open fallback AP, which is the shipped default and
    /// deliberate.
    pub fn parse_optional(psk: &str) -> Result<Option<Self>, ApConfigError> {
        if psk.is_empty() {
            return Ok(None);
        }

        if psk.chars().any(char::is_control) {
            return Err(ApConfigError::ControlCharacter { field: "AP_PSK" });
        }

        let len = psk.chars().count();
        if !(PSK_MIN_CHARS..=PSK_MAX_CHARS).contains(&len) {
            return Err(ApConfigError::PskLength { len });
        }

        Ok(Some(Psk(psk.to_string())))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApSecurity {
    Open,
    Wpa2,
}

impl fmt::Display for ApSecurity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Same wording the shell printed, so operator-facing output does not change.
            ApSecurity::Open => f.write_str("open (no passphrase)"),
            ApSecurity::Wpa2 => f.write_str("WPA2"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApConfig {
    pub interface: String,
    pub ssid: Ssid,
    pub psk: Option<Psk>,
    pub address: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub netmask_cidr: u8,
    pub dhcp_range_start: Ipv4Addr,
    pub dhcp_range_end: Ipv4Addr,
    pub dhcp_lease_time: String,
    pub channel: u8,
    pub country: String,
}

impl ApConfig {
    pub fn security(&self) -> ApSecurity {
        match self.psk {
            Some(_) => ApSecurity::Wpa2,
            None => ApSecurity::Open,
        }
    }
}

/// Loads and validates `fallback-ap.conf`, resolving every unset key to its default.
pub fn load_ap_config(paths: &Paths, robot_id: &RobotId) -> Result<ApConfig> {
    let path = paths.wifi_ap_conf();
    let text = conf::read_optional(&path)?.unwrap_or_else(|| {
        log::info(format!(
            "{} does not exist; using built-in fallback AP defaults",
            path.display()
        ));
        String::new()
    });

    let ssid = match conf::parse_value(&text, "AP_SSID") {
        Some(explicit) if !explicit.is_empty() => Ssid::new(&explicit)?,
        // Both unset and explicitly empty fall through to the derived name, matching the
        // shipped fallback-ap.conf, which sets AP_SSID="" and relies on this.
        _ => {
            let suffix = conf::parse_value(&text, "AP_SSID_SUFFIX")
                .unwrap_or_else(|| DEFAULT_SSID_SUFFIX.to_string());
            Ssid::new(&format!("{robot_id}{suffix}")).with_context(|| {
                format!(
                    "deriving the fallback AP SSID from ROBOT_ID='{robot_id}' and \
                     AP_SSID_SUFFIX='{suffix}'"
                )
            })?
        }
    };

    let psk = Psk::parse_optional(&conf::parse_value(&text, "AP_PSK").unwrap_or_default())?;

    Ok(ApConfig {
        interface: paths.wifi_interface().to_string(),
        ssid,
        psk,
        address: ipv4(&text, "AP_ADDRESS", DEFAULT_ADDRESS)?,
        netmask: ipv4(&text, "AP_NETMASK", DEFAULT_NETMASK)?,
        netmask_cidr: cidr(&text)?,
        dhcp_range_start: ipv4(&text, "AP_DHCP_RANGE_START", DEFAULT_DHCP_RANGE_START)?,
        dhcp_range_end: ipv4(&text, "AP_DHCP_RANGE_END", DEFAULT_DHCP_RANGE_END)?,
        dhcp_lease_time: nonempty(&text, "AP_DHCP_LEASE_TIME", DEFAULT_DHCP_LEASE_TIME)?,
        channel: channel(&text)?,
        country: nonempty(&text, "AP_COUNTRY", DEFAULT_COUNTRY)?,
    })
}

fn ipv4(text: &str, field: &'static str, default: &str) -> Result<Ipv4Addr, ApConfigError> {
    let raw = value_or(text, field, default);

    Ipv4Addr::from_str(&raw).map_err(|_| ApConfigError::NotAnIpv4Address { field, value: raw })
}

fn nonempty(text: &str, field: &'static str, default: &str) -> Result<String, ApConfigError> {
    let raw = value_or(text, field, default);

    if raw.is_empty() {
        return Err(ApConfigError::Empty { field });
    }
    if raw.chars().any(char::is_control) {
        return Err(ApConfigError::ControlCharacter { field });
    }

    Ok(raw)
}

fn channel(text: &str) -> Result<u8, ApConfigError> {
    let field = "AP_CHANNEL";
    let raw = value_or(text, field, &DEFAULT_CHANNEL.to_string());

    let parsed = raw
        .parse::<u16>()
        .map_err(|_| ApConfigError::NotANumber { field, value: raw })?;

    if !(1..=u16::from(CHANNEL_MAX)).contains(&parsed) {
        return Err(ApConfigError::ChannelOutOfRange { channel: parsed });
    }

    Ok(parsed as u8)
}

fn cidr(text: &str) -> Result<u8, ApConfigError> {
    let field = "AP_NETMASK_CIDR";
    let raw = value_or(text, field, &DEFAULT_NETMASK_CIDR.to_string());

    let parsed = raw
        .parse::<u16>()
        .map_err(|_| ApConfigError::NotANumber { field, value: raw })?;

    if !(1..=32).contains(&parsed) {
        return Err(ApConfigError::CidrOutOfRange { cidr: parsed });
    }

    Ok(parsed as u8)
}

/// An unset key and an explicitly empty one both take the default, matching shell's
/// `${AP_FOO:-default}` rather than `${AP_FOO-default}`.
fn value_or(text: &str, field: &str, default: &str) -> String {
    match conf::parse_value(text, field) {
        Some(value) if !value.is_empty() => value,
        _ => default.to_string(),
    }
}

pub fn render_hostapd_conf(config: &ApConfig) -> String {
    let mut out = format!(
        "interface={}\n\
         driver=nl80211\n\
         ssid={}\n\
         hw_mode=g\n\
         channel={}\n\
         country_code={}\n\
         ieee80211n=1\n\
         wmm_enabled=1\n\
         \n\
         auth_algs=1\n",
        config.interface,
        config.ssid.as_str(),
        config.channel,
        config.country
    );

    // The absence of every `wpa*` directive is what makes this an open network. Only add WPA2
    // when an operator has explicitly set a passphrase.
    if let Some(psk) = &config.psk {
        out.push_str(&format!(
            "wpa=2\n\
             wpa_key_mgmt=WPA-PSK\n\
             wpa_passphrase={}\n\
             rsn_pairwise=CCMP\n",
            psk.as_str()
        ));
    }

    out
}

pub fn render_dnsmasq_conf(config: &ApConfig) -> String {
    format!(
        "interface={}\n\
         bind-interfaces\n\
         dhcp-range={},{},{},{}\n\
         dhcp-option=3,{}\n\
         dhcp-option=6,{}\n\
         domain-needed\n\
         bogus-priv\n",
        config.interface,
        config.dhcp_range_start,
        config.dhcp_range_end,
        config.netmask,
        config.dhcp_lease_time,
        config.address,
        config.address,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn robot_id(id: &str) -> RobotId {
        RobotId::new(id).expect("test fixture must be a valid robot ID")
    }

    /// Writes `fallback-ap.conf` into a tempdir-rooted `Paths` and loads it.
    fn load_from(contents: &str) -> (TempDir, Result<ApConfig>) {
        let tmp = TempDir::new("ap-config");
        let paths = Paths::for_test(tmp.path());
        conf::write_atomically(&paths.wifi_ap_conf(), contents).unwrap();
        let loaded = load_ap_config(&paths, &robot_id("gopigo-07"));
        (tmp, loaded)
    }

    fn example_config() -> ApConfig {
        let tmp = TempDir::new("ap-defaults");
        let paths = Paths::for_test(tmp.path());
        load_ap_config(&paths, &robot_id("gopigo-07")).unwrap()
    }

    #[test]
    fn ssid_validation() {
        assert!(Ssid::new("gopigo-07-setup").is_ok());
        assert!(Ssid::new(&"a".repeat(32)).is_ok());

        assert!(matches!(Ssid::new(""), Err(ApConfigError::EmptySsid)));
        assert!(matches!(
            Ssid::new(&"a".repeat(33)),
            Err(ApConfigError::SsidTooLong { len: 33, .. })
        ));
        // Eight two-byte characters are 16 octets, so this is about the byte limit, not chars.
        assert!(matches!(
            Ssid::new(&"é".repeat(17)),
            Err(ApConfigError::SsidTooLong { len: 34, .. })
        ));
        assert!(matches!(
            Ssid::new("evil\nwpa=0"),
            Err(ApConfigError::ControlCharacter { field: "AP_SSID" })
        ));
    }

    #[test]
    fn psk_validation() {
        // Empty is valid and means "open network" — the shipped default.
        assert_eq!(Psk::parse_optional("").unwrap(), None);

        assert!(Psk::parse_optional("hunter22").unwrap().is_some());
        assert!(Psk::parse_optional(&"a".repeat(63)).unwrap().is_some());

        assert!(matches!(
            Psk::parse_optional("short"),
            Err(ApConfigError::PskLength { len: 5 })
        ));
        assert!(matches!(
            Psk::parse_optional(&"a".repeat(64)),
            Err(ApConfigError::PskLength { len: 64 })
        ));
        assert!(matches!(
            Psk::parse_optional("pass\nwpa=0"),
            Err(ApConfigError::ControlCharacter { field: "AP_PSK" })
        ));
    }

    #[test]
    fn missing_config_file_yields_defaults_and_a_derived_ssid() {
        let config = example_config();

        assert_eq!(config.ssid.as_str(), "gopigo-07-setup");
        assert_eq!(config.psk, None);
        assert_eq!(config.security(), ApSecurity::Open);
        assert_eq!(config.address, Ipv4Addr::new(192, 168, 50, 1));
        assert_eq!(config.netmask_cidr, 24);
        assert_eq!(config.channel, 6);
        assert_eq!(config.country, "US");
        assert_eq!(config.dhcp_lease_time, "12h");
    }

    #[test]
    fn shipped_fallback_ap_conf_loads() {
        // Verbatim copy of meta-gopigo's files/config/fallback-ap.conf, minus AP_INTERFACE
        // (covered separately below). If this stops loading, the shipped file broke.
        let (_tmp, loaded) = load_from(
            "AP_SSID=\"\"\n\
             AP_SSID_SUFFIX=\"-setup\"\n\
             AP_PSK=\"\"\n\
             AP_ADDRESS=\"192.168.50.1\"\n\
             AP_NETMASK=\"255.255.255.0\"\n\
             AP_NETMASK_CIDR=\"24\"\n\
             AP_DHCP_RANGE_START=\"192.168.50.10\"\n\
             AP_DHCP_RANGE_END=\"192.168.50.50\"\n\
             AP_DHCP_LEASE_TIME=\"12h\"\n\
             AP_CHANNEL=\"6\"\n\
             AP_COUNTRY=\"US\"\n",
        );

        let config = loaded.unwrap();
        assert_eq!(config.ssid.as_str(), "gopigo-07-setup");
        assert_eq!(config.security(), ApSecurity::Open);
    }

    #[test]
    fn explicit_ssid_overrides_the_derived_one() {
        let (_tmp, loaded) = load_from("AP_SSID=\"lab-recovery\"\n");

        assert_eq!(loaded.unwrap().ssid.as_str(), "lab-recovery");
    }

    #[test]
    fn a_passphrase_switches_security_to_wpa2() {
        let (_tmp, loaded) = load_from("AP_PSK=\"hunter2hunter2\"\n");
        let config = loaded.unwrap();

        assert_eq!(config.security(), ApSecurity::Wpa2);
        assert_eq!(config.psk.unwrap().as_str(), "hunter2hunter2");
    }

    #[test]
    fn invalid_values_are_rejected_rather_than_silently_defaulted() {
        for bad in [
            "AP_ADDRESS=\"not-an-ip\"\n",
            "AP_CHANNEL=\"0\"\n",
            "AP_CHANNEL=\"36\"\n",
            "AP_CHANNEL=\"abc\"\n",
            "AP_NETMASK_CIDR=\"33\"\n",
            "AP_PSK=\"short\"\n",
        ] {
            let (_tmp, loaded) = load_from(bad);
            assert!(loaded.is_err(), "expected {bad:?} to be rejected");
        }
    }

    #[test]
    fn a_robot_id_too_long_for_an_ssid_is_reported_not_truncated() {
        let tmp = TempDir::new("ap-long-ssid");
        let paths = Paths::for_test(tmp.path());

        // 63 characters is a valid robot ID, but 63 + "-setup" is well over the 32-byte SSID
        // limit. Truncating would give two robots the same recovery AP name.
        let long = robot_id(&"a".repeat(63));
        let err = load_ap_config(&paths, &long).unwrap_err();

        assert!(
            format!("{err:#}").contains("AP_SSID"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn hostapd_conf_open_network() {
        let config = example_config();

        assert_eq!(
            render_hostapd_conf(&config),
            "interface=wlan-ctrl\n\
             driver=nl80211\n\
             ssid=gopigo-07-setup\n\
             hw_mode=g\n\
             channel=6\n\
             country_code=US\n\
             ieee80211n=1\n\
             wmm_enabled=1\n\
             \n\
             auth_algs=1\n"
        );
    }

    #[test]
    fn hostapd_conf_wpa2() {
        let mut config = example_config();
        config.psk = Some(Psk::parse_optional("hunter2hunter2").unwrap().unwrap());

        assert_eq!(
            render_hostapd_conf(&config),
            "interface=wlan-ctrl\n\
             driver=nl80211\n\
             ssid=gopigo-07-setup\n\
             hw_mode=g\n\
             channel=6\n\
             country_code=US\n\
             ieee80211n=1\n\
             wmm_enabled=1\n\
             \n\
             auth_algs=1\n\
             wpa=2\n\
             wpa_key_mgmt=WPA-PSK\n\
             wpa_passphrase=hunter2hunter2\n\
             rsn_pairwise=CCMP\n"
        );
    }

    #[test]
    fn dnsmasq_conf() {
        let config = example_config();

        assert_eq!(
            render_dnsmasq_conf(&config),
            "interface=wlan-ctrl\n\
             bind-interfaces\n\
             dhcp-range=192.168.50.10,192.168.50.50,255.255.255.0,12h\n\
             dhcp-option=3,192.168.50.1\n\
             dhcp-option=6,192.168.50.1\n\
             domain-needed\n\
             bogus-priv\n"
        );
    }
}
