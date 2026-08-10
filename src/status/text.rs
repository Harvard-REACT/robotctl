//! Human-readable `robotctl status` output.

use std::fmt::Write;

use super::{Filesystem, Status};

pub fn render(status: &Status) -> String {
    let mut out = String::new();

    section(&mut out, "identity");
    field(&mut out, "robot_id", &status.identity.robot_id);
    field(
        &mut out,
        "ros_domain_id",
        &status.identity.ros_domain_id.to_string(),
    );
    field(
        &mut out,
        "hostname",
        status.identity.hostname.as_deref().unwrap_or("unknown"),
    );
    if !status.identity.hostname_matches {
        let _ = writeln!(
            out,
            "  !! hostname does not match robot_id -- run `robotctl id apply` as root"
        );
    }

    section(&mut out, "image");
    if status.image.is_empty() {
        let _ = writeln!(out, "  (no image-version file)");
    }
    for (key, value) in &status.image {
        field(&mut out, key, value);
    }

    section(&mut out, "system");
    field(
        &mut out,
        "kernel",
        status.system.kernel.as_deref().unwrap_or("unknown"),
    );
    field(
        &mut out,
        "uptime",
        &status
            .system
            .uptime_seconds
            .map(format_duration)
            .unwrap_or_else(|| "unknown".to_string()),
    );
    field(
        &mut out,
        "time_utc",
        status.system.time_utc.as_deref().unwrap_or("unknown"),
    );

    section(&mut out, "slots");
    field(
        &mut out,
        "booted",
        status
            .rauc
            .booted_slot
            .as_deref()
            .unwrap_or("unknown (not an A/B image?)"),
    );
    field(
        &mut out,
        "primary",
        status.rauc.primary_slot.as_deref().unwrap_or("unknown"),
    );
    for (slot, state) in &status.rauc.slot_states {
        field(&mut out, &format!("slot {slot}"), state);
    }

    section(&mut out, "disk");
    for filesystem in &status.filesystems {
        field(
            &mut out,
            &filesystem.mount,
            &describe_filesystem(filesystem),
        );
    }

    section(&mut out, "wifi");
    field(&mut out, "interface", &status.wifi.interface);
    if !status.wifi.exists {
        let _ = writeln!(out, "  (interface does not exist)");
    }
    if let Some(mode) = &status.wifi.mode {
        field(&mut out, "mode", mode);
    }
    if let Some(ssid) = &status.wifi.ssid {
        field(&mut out, "ssid", ssid);
    }
    if let Some(security) = &status.wifi.security {
        field(&mut out, "security", security);
    }
    field(
        &mut out,
        "address",
        status.wifi.address.as_deref().unwrap_or("none"),
    );
    field(
        &mut out,
        "client.conf",
        if status.wifi.client_conf_present {
            "present"
        } else {
            "missing"
        },
    );

    section(&mut out, "network");
    for interface in &status.network.interfaces {
        field(&mut out, &interface.name, &interface.addresses.join(", "));
    }
    field(
        &mut out,
        "default route",
        status.network.default_route.as_deref().unwrap_or("none"),
    );

    section(&mut out, "units");
    for unit in &status.units {
        field(&mut out, &unit.name, &unit.state);
    }

    section(&mut out, "failed units");
    if status.failed_units.is_empty() {
        let _ = writeln!(out, "  none");
    }
    for unit in &status.failed_units {
        let _ = writeln!(out, "  {unit}");
    }

    out
}

fn section(out: &mut String, title: &str) {
    if !out.is_empty() {
        out.push('\n');
    }
    let _ = writeln!(out, "-- {title} --");
}

fn field(out: &mut String, key: &str, value: &str) {
    let _ = writeln!(out, "  {key:<34} {value}");
}

fn describe_filesystem(filesystem: &Filesystem) -> String {
    let used = filesystem
        .total_bytes
        .saturating_sub(filesystem.available_bytes);

    let percent = (used * 100)
        .checked_div(filesystem.total_bytes)
        .unwrap_or(0);

    format!(
        "{} free of {} ({percent}% used)",
        format_bytes(filesystem.available_bytes),
        format_bytes(filesystem.total_bytes)
    )
}

/// Binary units, one decimal place, matching what `df -h` shows.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("T", 1 << 40),
        ("G", 1 << 30),
        ("M", 1 << 20),
        ("K", 1 << 10),
    ];

    for (suffix, scale) in UNITS {
        if bytes >= scale {
            return format!("{:.1}{suffix}", bytes as f64 / scale as f64);
        }
    }

    format!("{bytes}B")
}

/// Largest two units only: "3d 4h" rather than "3d 4h 12m 7s", because the point is a glance at
/// how long the robot has been up, not a stopwatch reading.
pub fn format_duration(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;

    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {}s", seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_byte_counts_like_df() {
        assert_eq!(format_bytes(0), "0B");
        assert_eq!(format_bytes(999), "999B");
        assert_eq!(format_bytes(1 << 10), "1.0K");
        assert_eq!(format_bytes(1536), "1.5K");
        assert_eq!(format_bytes(4 * (1 << 30)), "4.0G");
        assert_eq!(format_bytes(3 * (1 << 40)), "3.0T");
    }

    #[test]
    fn formats_durations_to_two_units() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(90), "1m 30s");
        assert_eq!(format_duration(3_600), "1h 0m");
        assert_eq!(format_duration(90_000), "1d 1h");
    }

    #[test]
    fn describes_usage_as_free_of_total() {
        let filesystem = Filesystem {
            mount: "/data".to_string(),
            total_bytes: 4 * (1 << 30),
            available_bytes: 1 << 30,
        };

        assert_eq!(
            describe_filesystem(&filesystem),
            "1.0G free of 4.0G (75% used)"
        );
    }

    #[test]
    fn a_zero_sized_filesystem_does_not_divide_by_zero() {
        let filesystem = Filesystem {
            mount: "/proc".to_string(),
            total_bytes: 0,
            available_bytes: 0,
        };

        assert_eq!(describe_filesystem(&filesystem), "0B free of 0B (0% used)");
    }

    #[test]
    fn renders_a_full_report_without_a_robot() {
        let status = super::super::tests::example_status();
        let rendered = render(&status);

        assert!(rendered.contains("robot_id                           gopigo-07"));
        assert!(rendered.contains("booted                             A"));
        assert!(rendered.contains("mode                               client"));
        assert!(rendered.contains("wlan-ctrl                          192.168.1.50/24"));
        assert!(rendered.contains("uptime                             1d 1h"));
    }

    #[test]
    fn warns_when_the_hostname_has_not_been_applied() {
        let mut status = super::super::tests::example_status();
        status.identity.hostname = Some("gopigo3-rpi5".to_string());
        status.identity.hostname_matches = false;

        assert!(render(&status).contains("run `robotctl id apply` as root"));
    }

    #[test]
    fn reports_absence_rather_than_omitting_sections() {
        let mut status = super::super::tests::example_status();
        status.image.clear();
        status.failed_units.clear();

        let rendered = render(&status);

        assert!(rendered.contains("(no image-version file)"));
        assert!(rendered.contains("none"));
    }
}
