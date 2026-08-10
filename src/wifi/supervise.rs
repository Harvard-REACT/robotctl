//! The client <-> fallback-AP state machine.

use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::config::{Paths, SuperviseTuning};
use crate::id;
use crate::log;
use crate::wifi::ap::{self, ApSession};
use crate::wifi::client::{self, ClientFailure, ClientSession};
use crate::wifi::config_gen;
use crate::wifi::net;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Attempting to associate with a configured network.
    TryClient,
    /// Associated, with an address. Being watched.
    ClientUp,
    /// Serving the recovery AP, periodically re-trying the client network.
    FallbackAp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A client attempt succeeded and we have an address.
    ClientConnected,
    /// Every client attempt in this cycle failed.
    ClientFailed,
    /// There is no `client.conf` at all, so there is nothing to retry with.
    ClientConfigMissing,
    /// An established client connection lost its address.
    LinkLost,
    /// The fallback AP has been up long enough to be worth interrupting for a client retry.
    ApRetryDue,
    /// hostapd or dnsmasq exited on its own.
    ApDied,
    /// The retry interval elapsed but there is nothing to retry with, so the AP keeps serving.
    ApHeld,
}

pub fn next_state(current: State, event: Event) -> State {
    match (current, event) {
        (_, Event::ClientConnected) => State::ClientUp,

        // Both a failed attempt and missing credentials raise the AP. The difference between
        // them is how long we wait before trying again, which the driver decides, not the table.
        (_, Event::ClientFailed) | (_, Event::ClientConfigMissing) => State::FallbackAp,

        // A dropped connection retries the client immediately rather than falling straight back
        // to the AP: the usual cause is a brief outage, and the client path is the one that
        // makes the robot reachable on the real network.
        (_, Event::LinkLost) => State::TryClient,

        (_, Event::ApRetryDue) => State::TryClient,

        // Restart the AP by re-entering the state; the driver notices it has no live session.
        // `ApHeld` re-enters the same way but leaves the running session alone.
        (_, Event::ApDied) | (_, Event::ApHeld) => State::FallbackAp,
    }
}

/// Runs the supervisor. Does not return under normal operation.
///
/// Teardown on shutdown is left to systemd: the daemons are children in this unit's cgroup, so
/// `systemctl stop` reaps them with us. The interface may keep its address until the next start,
/// which is harmless because every entry path flushes it first.
pub fn run(paths: &Paths, tuning: &SuperviseTuning) -> Result<()> {
    let interface = paths.wifi_interface().to_string();

    log::info(format!(
        "WiFi supervisor starting on {interface} \
         (connect timeout {}s, {} attempts, AP retry every {}s)",
        tuning.connect_timeout_sec, tuning.client_attempts, tuning.ap_retry_sec
    ));

    net::require_interface(&interface)?;

    let mut supervisor = Supervisor {
        paths,
        tuning,
        interface,
        client: None,
        ap: None,
    };

    let mut state = State::TryClient;
    let mut announced_ready = false;

    loop {
        let event = supervisor.step(state)?;
        let next = next_state(state, event);

        if next != state {
            log::info(format!("{state:?} -> {next:?} ({event:?})"));
        }

        if !announced_ready && matches!(next, State::ClientUp | State::FallbackAp) {
            notify_ready(next);
            announced_ready = true;
        }

        state = next;
    }
}

/// Tells systemd this unit is ready, for `Type=notify`.
///
/// This exists because of an ordering regression the port would otherwise introduce.
/// `robot-experiments.service` carries `After=robot-wifi-supervisor.service` specifically so its
/// image pull runs once WiFi is up. That worked because the shell supervisor was `Type=oneshot`,
/// where `After=` means "after it exited" — i.e. after WiFi had been decided. This supervisor is
/// long-running, so under `Type=simple` the same `After=` would mean "after the process was
/// spawned", and every pull would race a down interface, silently fall back to the local image,
/// and never refresh anything. `Type=notify` plus this call restores the original meaning.
///
/// Best-effort by design: not running under systemd is the normal case when testing by hand.
fn notify_ready(state: State) {
    let Some(socket) = crate::config::systemd_notify_socket() else {
        return;
    };

    let message = format!("READY=1\nSTATUS=WiFi settled: {state:?}\n");

    let sent = std::os::unix::net::UnixDatagram::unbound()
        .and_then(|datagram| datagram.send_to(message.as_bytes(), &socket));

    match sent {
        Ok(_) => log::info(format!("notified systemd ready ({state:?})")),
        Err(err) => log::warn(format!(
            "could not notify systemd readiness on {}: {err}",
            socket.display()
        )),
    }
}

struct Supervisor<'a> {
    paths: &'a Paths,
    tuning: &'a SuperviseTuning,
    interface: String,
    client: Option<ClientSession>,
    ap: Option<ApSession>,
}

impl Supervisor<'_> {
    fn step(&mut self, state: State) -> Result<Event> {
        match state {
            State::TryClient => Ok(self.try_client()),
            State::ClientUp => Ok(self.watch_client()),
            State::FallbackAp => self.serve_ap(),
        }
    }

    /// Attempts to connect, retrying up to `client_attempts` times.
    fn try_client(&mut self) -> Event {
        // The radio cannot be an AP and a station at once, so the AP must be fully down before
        // wpa_supplicant touches the interface.
        self.stop_ap();

        for attempt in 1..=self.tuning.client_attempts {
            match client::connect(self.paths, self.tuning) {
                Ok(session) => {
                    log::info(format!(
                        "connected on {} with address {}",
                        self.interface, session.address
                    ));
                    self.client = Some(session);
                    return Event::ClientConnected;
                }

                Err(ClientFailure::NoConfig { path }) => {
                    log::info(format!(
                        "{} does not exist; no client network to join. \
                         Raising the fallback AP so the robot stays reachable.",
                        path.display()
                    ));
                    return Event::ClientConfigMissing;
                }

                Err(ClientFailure::Failed(err)) => {
                    log::warn(format!(
                        "client attempt {attempt}/{} failed: {err:#}",
                        self.tuning.client_attempts
                    ));
                    // Drop the half-finished attempt before the next one, so a stuck
                    // wpa_supplicant does not compete with its own replacement.
                    self.stop_client();

                    if attempt < self.tuning.client_attempts {
                        sleep(Duration::from_secs(self.tuning.retry_sec.into()));
                    }
                }
            }
        }

        Event::ClientFailed
    }

    /// Watches an established connection, returning once it is no longer usable.
    fn watch_client(&mut self) -> Event {
        loop {
            sleep(Duration::from_secs(self.tuning.link_poll_sec.into()));

            // The address is the same success criterion `connect` used, so "connected" means the
            // same thing whether we just established it or have been holding it for a week.
            if net::ipv4_address(&self.interface).is_none() {
                log::warn(format!("{} lost its IPv4 address", self.interface));
                self.stop_client();
                return Event::LinkLost;
            }
        }
    }

    /// Serves the fallback AP, returning when it is time to re-try the client or the AP died.
    fn serve_ap(&mut self) -> Result<Event> {
        self.stop_client();

        if self.ap.is_none() {
            // Read identity fresh on every AP start rather than caching it: `robotctl id set`
            // may have renamed the robot since the supervisor started, and the recovery AP's
            // name is how someone finds this robot among fifteen others.
            let identity = id::load(self.paths)?;
            let config = config_gen::load_ap_config(self.paths, &identity.robot_id)?;
            self.ap = Some(ap::start(self.paths.wifi_run_dir(), &config)?);
        }

        let poll = Duration::from_secs(self.tuning.link_poll_sec.into());
        let retry_after = Duration::from_secs(self.tuning.ap_retry_sec.into());
        let started = Instant::now();

        loop {
            sleep(poll);

            if let Some(session) = self.ap.as_mut()
                && !session.is_healthy()
            {
                log::warn("fallback AP daemons are no longer running; restarting them");
                self.stop_ap();
                return Ok(Event::ApDied);
            }

            if started.elapsed() < retry_after {
                continue;
            }

            // Only interrupt the AP if there is actually something to retry. Without a
            // client.conf, tearing the AP down every interval would repeatedly disconnect
            // whoever is using it to fix the robot, and could not possibly succeed. Re-checked
            // every interval rather than once, so credentials written over the AP take effect
            // on the next cycle without a restart.
            if !self.paths.wifi_client_conf().is_file() {
                return Ok(Event::ApHeld);
            }

            log::info("taking the fallback AP down to re-try the client network");
            return Ok(Event::ApRetryDue);
        }
    }

    fn stop_client(&mut self) {
        if self.client.take().is_some() {
            client::teardown(&self.interface);
        }
    }

    fn stop_ap(&mut self) {
        // Dropping the session stops hostapd and dnsmasq and flushes the address.
        self.ap = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_successful_connection_always_wins() {
        for state in [State::TryClient, State::ClientUp, State::FallbackAp] {
            assert_eq!(
                next_state(state, Event::ClientConnected),
                State::ClientUp,
                "from {state:?}"
            );
        }
    }

    #[test]
    fn client_failure_raises_the_fallback_ap() {
        assert_eq!(
            next_state(State::TryClient, Event::ClientFailed),
            State::FallbackAp
        );
        assert_eq!(
            next_state(State::TryClient, Event::ClientConfigMissing),
            State::FallbackAp
        );
    }

    #[test]
    fn a_dropped_connection_retries_the_client_rather_than_falling_back() {
        // This is the bug the shell supervisor had: nothing at all happened on a link drop.
        assert_eq!(
            next_state(State::ClientUp, Event::LinkLost),
            State::TryClient
        );
    }

    #[test]
    fn the_fallback_ap_periodically_re_tries_the_client() {
        // The other half of the same bug: the shell never left AP mode once it entered it.
        assert_eq!(
            next_state(State::FallbackAp, Event::ApRetryDue),
            State::TryClient
        );
    }

    #[test]
    fn a_dead_ap_is_restarted() {
        assert_eq!(
            next_state(State::FallbackAp, Event::ApDied),
            State::FallbackAp
        );
    }

    #[test]
    fn every_state_event_pair_has_a_transition() {
        let states = [State::TryClient, State::ClientUp, State::FallbackAp];
        let events = [
            Event::ClientConnected,
            Event::ClientFailed,
            Event::ClientConfigMissing,
            Event::LinkLost,
            Event::ApRetryDue,
            Event::ApDied,
            Event::ApHeld,
        ];

        for state in states {
            for event in events {
                let next = next_state(state, event);
                assert!(states.contains(&next), "{state:?} + {event:?} -> {next:?}");
            }
        }
    }

    #[test]
    fn a_failing_client_cycles_between_retrying_and_the_ap_forever() {
        // Walk the machine the way a robot with an unreachable network would: it must keep
        // alternating rather than settling into either state permanently.
        let mut state = State::TryClient;
        let mut seen_ap = 0;
        let mut seen_try = 0;

        for round in 0..10 {
            state = next_state(state, Event::ClientFailed);
            assert_eq!(state, State::FallbackAp, "round {round}");
            seen_ap += 1;

            state = next_state(state, Event::ApRetryDue);
            assert_eq!(state, State::TryClient, "round {round}");
            seen_try += 1;
        }

        assert_eq!((seen_ap, seen_try), (10, 10));
    }
}
