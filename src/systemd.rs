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
