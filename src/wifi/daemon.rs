//! Owned child processes: hostapd, dnsmasq, wpa_supplicant.
//! `wifi status` still reports honestly without access to these handles, because it asks `iw`
//! what the interface is actually doing rather than reading our bookkeeping. See `net`.

use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::log;

/// How long a daemon gets to exit after SIGTERM before it is SIGKILLed.
const TERM_GRACE: Duration = Duration::from_secs(5);
const TERM_POLL: Duration = Duration::from_millis(100);

pub struct Daemon {
    name: &'static str,
    child: Child,
}

impl Daemon {
    /// Spawns a daemon in the foreground (no `-B`, no `--daemon`), so it stays a child of this
    /// process and systemd's cgroup teardown reaps it along with us.
    ///
    /// stdout/stderr are inherited so hostapd's and wpa_supplicant's own diagnostics land in the
    /// journal under the supervisor's unit
    pub fn spawn(name: &'static str, program: &str, args: &[&str]) -> Result<Self> {
        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .spawn()
            .with_context(|| format!("could not start {name} (is `{program}` installed?)"))?;

        log::info(format!("started {name} (pid {})", child.id()));

        Ok(Daemon { name, child })
    }

    /// Whether the daemon is still alive, reaping it if it has exited.
    pub fn is_running(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                log::warn(format!("{} exited on its own ({status})", self.name));
                false
            }
            Err(err) => {
                log::warn(format!("could not check on {}: {err}", self.name));
                false
            }
        }
    }

    /// SIGTERM, then SIGKILL if it is still alive after the grace period.
    pub fn stop(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }

        // Safety: `self.child` has not been waited to completion (checked above), so its PID is
        // still reserved by the kernel and cannot have been recycled onto another process.
        let pid = self.child.id() as libc::pid_t;
        unsafe { libc::kill(pid, libc::SIGTERM) };

        let deadline = Instant::now() + TERM_GRACE;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                log::info(format!("stopped {}", self.name));
                return;
            }
            sleep(TERM_POLL);
        }

        log::warn(format!(
            "{} did not exit within {}s of SIGTERM; killing it",
            self.name,
            TERM_GRACE.as_secs()
        ));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}
