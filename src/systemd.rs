use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

pub fn is_active(unit: &str) -> String {
    match Command::new("systemctl")
        .args(["is-active", unit])
        .stdin(Stdio::null())
        .output()
    {
        Ok(result) => {
            let state = String::from_utf8_lossy(&result.stdout).trim().to_string();
            if state.is_empty() {
                "unknown".to_string()
            } else {
                state
            }
        }
        Err(_) => "unknown".to_string(),
    }
}

pub fn try_restart_async(unit: &str) {
    if unit.is_empty() {
        return;
    }

    let result = Command::new("systemctl")
        .args(["--no-block", "try-restart", unit])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    match result {
        Ok(status) if status.success() => crate::log::info(format!("asked {unit} to restart")),
        _ => crate::log::info(format!("could not restart {unit}")),
    }
}

pub fn stop(unit: &str) -> Result<()> {
    let status = Command::new("systemctl")
        .args(["stop", unit])
        .stdin(Stdio::null())
        .status()
        .context("could not run `systemctl` (is this a systemd system?)")?;

    if !status.success() {
        bail!("`systemctl stop {unit}` exited with {status}");
    }

    Ok(())
}
