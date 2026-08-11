//! Hooks for the WiFi stack

use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::log;

/// How long a hook may run before it is terminated, and the SIGTERM grace after that.
///
/// The supervisor is single-threaded, so this bounds what a wedged hook can cost the robot: at
/// worst `RUN_LIMIT + TERM_GRACE` added to one transition, never a stall.
const RUN_LIMIT: Duration = Duration::from_secs(5);
const TERM_GRACE: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(10);

/// The state name passed to the hook when the WiFi stack is not running at all.
///
/// The supervisor's own states name themselves; see [`super::supervise::State::hook_name`].
pub const OFF: &str = "off";

/// Runs the hook with `state` as its only argument, if one is configured.
///
/// Best-effort: no hook, a hook that does not exist, a hook that fails, and a hook that overruns
/// [`RUN_LIMIT`] are all logged and otherwise ignored.
pub fn notify(hook: &str, state: &str) {
    notify_within(hook, state, RUN_LIMIT, TERM_GRACE);
}

/// [`notify`] with the timeouts spelled out, so the tests can exercise the wedged-hook path
/// without spending [`RUN_LIMIT`] doing it.
fn notify_within(hook: &str, state: &str, run_limit: Duration, term_grace: Duration) {
    if hook.is_empty() {
        return;
    }

    let mut child = match Command::new(hook).arg(state).stdin(Stdio::null()).spawn() {
        Ok(child) => child,
        Err(err) => {
            log::warn(format!("could not run state hook `{hook} {state}`: {err}"));
            return;
        }
    };

    match wait_until(&mut child, Instant::now() + run_limit) {
        Wait::Exited(status) if status.success() => {}
        Wait::Exited(status) => {
            log::warn(format!("state hook `{hook} {state}` exited with {status}"))
        }
        Wait::Running => {
            log::warn(format!(
                "state hook `{hook} {state}` is still running after {:?}; terminating it",
                run_limit
            ));
            terminate(&mut child, term_grace);
        }
        // Nothing was reaped and nothing can safely be signalled, because the PID may no longer
        // be this child's. Leaving it alone is the only correct move.
        Wait::Unwaitable(err) => log::warn(format!(
            "could not wait for state hook `{hook} {state}`: {err}"
        )),
    }
}

enum Wait {
    Exited(std::process::ExitStatus),
    /// Still running at the deadline.
    Running,
    Unwaitable(std::io::Error),
}

fn wait_until(child: &mut Child, deadline: Instant) -> Wait {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Wait::Exited(status),
            Err(err) => return Wait::Unwaitable(err),
            Ok(None) => {}
        }

        if Instant::now() >= deadline {
            return Wait::Running;
        }

        sleep(POLL);
    }
}

/// SIGTERM, then SIGKILL if it is still alive after the grace period. Same escalation as
/// [`super::daemon::Daemon::stop`], and it always reaps, so an overrunning hook cannot leave
/// zombies behind over the lifetime of a long-running supervisor.
fn terminate(child: &mut Child, grace: Duration) {
    // Safety: `child` has not been waited to completion -- `wait_until` returned `Running` -- so
    // its PID is still reserved by the kernel and cannot have been recycled onto another process.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };

    if !matches!(wait_until(child, Instant::now() + grace), Wait::Running) {
        return;
    }

    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;
    use crate::testutil::TempDir;

    /// Writes an executable hook that appends its argument to `record`.
    fn recording_hook(dir: &Path, record: &Path) -> String {
        let hook = dir.join("hook");

        fs::write(
            &hook,
            format!("#!/bin/sh\necho \"$1\" >> {}\n", record.display()),
        )
        .expect("could not write test hook");

        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))
            .expect("could not make test hook executable");

        hook.display().to_string()
    }

    #[test]
    fn the_hook_receives_the_state_as_its_only_argument() {
        let dir = TempDir::new("hook");
        let record = dir.path().join("states");
        let hook = recording_hook(dir.path(), &record);

        notify(&hook, "client-up");
        notify(&hook, OFF);

        assert_eq!(
            fs::read_to_string(&record).expect("hook did not run"),
            "client-up\noff\n"
        );
    }

    #[test]
    fn an_unset_hook_runs_nothing() {
        // Nothing to observe beyond "this did not panic and did not try to spawn `""`", which is
        // the whole behaviour every non-GoPiGo image relies on.
        notify("", "client-up");
    }

    #[test]
    fn a_missing_hook_is_not_fatal() {
        notify("/nonexistent/robotctl-state-hook", "fallback-ap");
    }

    #[test]
    fn a_failing_hook_is_not_fatal() {
        // The TempDir has to outlive the call: dropping it deletes the hook, and a hook that is
        // not there tests nothing.
        let dir = TempDir::new("hook-fails");
        let hook = script(&dir, "exit 3\n");

        notify(&hook, "try-client");
    }

    /// Writes an executable `#!/bin/sh` hook running `body`.
    fn script(dir: &TempDir, body: &str) -> String {
        let hook = dir.path().join("hook");

        fs::write(&hook, format!("#!/bin/sh\n{body}")).expect("could not write test hook");
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))
            .expect("could not make test hook executable");

        hook.display().to_string()
    }

    #[test]
    fn a_hook_that_finishes_inside_its_limit_is_left_alone() {
        let dir = TempDir::new("hook-slow");
        let record = dir.path().join("states");
        let hook = script(&dir, &format!("sleep 0.05\necho done >> {}\n", record.display()));

        notify_within(
            &hook,
            "client-up",
            Duration::from_secs(5),
            Duration::from_millis(100),
        );

        assert_eq!(
            fs::read_to_string(&record).expect("hook was cut short"),
            "done\n"
        );
    }

    #[test]
    fn a_wedged_hook_is_killed_rather_than_waited_on() {
        // Ignores SIGTERM, so this exercises the whole escalation: run limit, SIGTERM, grace,
        // SIGKILL. `sleep 5` rather than a longer one only so a stray child cannot outlive the
        // test run by much.
        let dir = TempDir::new("hook-wedged");
        let hook = script(&dir, "trap '' TERM\nsleep 5\n");

        let started = Instant::now();
        notify_within(
            &hook,
            "fallback-ap",
            Duration::from_millis(150),
            Duration::from_millis(100),
        );
        let elapsed = started.elapsed();

        // The point of the whole mechanism: the supervisor gets control back on the timeouts it
        // set, not on whatever the hook felt like doing.
        assert!(
            elapsed < Duration::from_secs(2),
            "notify blocked for {elapsed:?}"
        );
    }
}
