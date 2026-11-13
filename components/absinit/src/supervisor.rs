//! The decisions: which service to start, restart or stop, and when.
//!
//! The supervisor never touches the system. It is told what happened
//! (a process was spawned, exited or became ready; time passed; shutdown was
//! requested) and answers with [`Action`]s for the runtime to carry out.
//! Time is passed in, so tests drive it with a simulated clock.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crate::config::{Ready, Restart, Service};
use crate::plan::Plan;

/// A service that stays up this long is healthy again; its failure count
/// starts over.
pub const STABLE_AFTER: Duration = Duration::from_secs(10);
/// Failures in a row after which a service is given up on.
pub const MAX_FAILURES: u32 = 5;
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// How a process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl ExitStatus {
    pub fn code(code: i32) -> Self {
        ExitStatus {
            code: Some(code),
            signal: None,
        }
    }

    pub fn signal(signal: i32) -> Self {
        ExitStatus {
            code: None,
            signal: Some(signal),
        }
    }

    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

impl std::fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.code, self.signal) {
            (Some(code), _) => write!(f, "exit code {code}"),
            (None, Some(signal)) => write!(f, "signal {signal}"),
            (None, None) => f.write_str("unknown status"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Terminate,
    Kill,
}

/// Something for the runtime to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Start the service's command, then report back with
    /// [`Supervisor::spawned`] or [`Supervisor::spawn_failed`].
    Spawn(String),
    Signal {
        service: String,
        pid: u32,
        signal: Signal,
    },
    Log(String),
}

/// Where a service is in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Waiting for its dependencies.
    Waiting,
    /// Asked the runtime to start it.
    Spawning,
    /// Running but not ready yet (`ready = "exited"` or `"notify"`).
    Starting {
        pid: u32,
        /// When a `notify` service must be ready by.
        ready_by: Option<Instant>,
        timed_out: bool,
    },
    /// Running and ready.
    Running { pid: u32, since: Instant },
    /// Ready with nothing running: a finished task, a reached milestone, or
    /// a service that exited successfully and is not restarted.
    Done,
    /// Waiting to be restarted.
    Backoff { until: Instant },
    /// Given up on.
    Failed(String),
    /// Never started.
    Skipped(String),
    /// Asked to stop, at shutdown.
    Stopping {
        pid: u32,
        kill_at: Instant,
        killed: bool,
    },
    /// Stopped at shutdown.
    Stopped,
}

impl State {
    fn is_ready(&self) -> bool {
        matches!(self, State::Running { .. } | State::Done)
    }

    /// Will never become ready (as far as starting dependents goes).
    fn is_hopeless(&self) -> bool {
        matches!(self, State::Failed(_) | State::Skipped(_))
    }

    fn pid(&self) -> Option<u32> {
        match self {
            State::Starting { pid, .. }
            | State::Running { pid, .. }
            | State::Stopping { pid, .. } => Some(*pid),
            _ => None,
        }
    }

    /// Has a process, or is about to.
    fn is_active(&self) -> bool {
        self.pid().is_some() || matches!(self, State::Spawning)
    }
}

struct Entry {
    service: Service,
    state: State,
    /// Failures in a row.
    failures: u32,
}

pub struct Supervisor {
    entries: BTreeMap<String, Entry>,
    /// For each service, those that `require` it or come `after` it.
    dependents: BTreeMap<String, BTreeSet<String>>,
    shutting_down: bool,
    actions: Vec<Action>,
}

impl Supervisor {
    pub fn new(plan: Plan, now: Instant) -> Supervisor {
        let mut dependents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for service in plan.services.values() {
            for dependency in service.requires.iter().chain(&service.after) {
                dependents
                    .entry(dependency.clone())
                    .or_default()
                    .insert(service.name.clone());
            }
        }

        let mut actions = Vec::new();
        let entries = plan
            .services
            .into_iter()
            .map(|(name, service)| {
                let state = match plan.skipped.get(&name) {
                    Some(reason) => {
                        actions.push(Action::Log(format!("skipping {name}: {reason}")));
                        State::Skipped(reason.clone())
                    }
                    None => State::Waiting,
                };
                let entry = Entry {
                    service,
                    state,
                    failures: 0,
                };
                (name, entry)
            })
            .collect();

        let mut supervisor = Supervisor {
            entries,
            dependents,
            shutting_down: false,
            actions,
        };
        supervisor.advance(now);
        supervisor
    }

    /// The actions decided since the last call.
    pub fn take_actions(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.actions)
    }

    pub fn state(&self, name: &str) -> Option<&State> {
        self.entries.get(name).map(|entry| &entry.state)
    }

    pub fn service(&self, name: &str) -> Option<&Service> {
        self.entries.get(name).map(|entry| &entry.service)
    }

    /// The runtime started the service's process.
    pub fn spawned(&mut self, name: &str, pid: u32, now: Instant) {
        let shutting_down = self.shutting_down;
        let Some(entry) = self.entries.get_mut(name) else {
            return;
        };
        if entry.state != State::Spawning {
            return;
        }
        let Some(command) = &entry.service.command else {
            return;
        };
        if shutting_down {
            // Shutdown began while it was being started: stop it right away.
            entry.state = State::Stopping {
                pid,
                kill_at: now + command.stop_timeout,
                killed: false,
            };
            self.actions.push(Action::Signal {
                service: name.to_string(),
                pid,
                signal: Signal::Terminate,
            });
            return;
        }
        entry.state = match command.ready {
            Ready::Started => {
                self.actions
                    .push(Action::Log(format!("started {name} (pid {pid})")));
                State::Running { pid, since: now }
            }
            Ready::Exited => State::Starting {
                pid,
                ready_by: None,
                timed_out: false,
            },
            Ready::Notify => State::Starting {
                pid,
                ready_by: Some(now + command.ready_timeout),
                timed_out: false,
            },
        };
        self.advance(now);
    }

    /// The runtime could not start the service's process.
    pub fn spawn_failed(&mut self, name: &str, error: &str, now: Instant) {
        if self.entries.get(name).map(|e| &e.state) == Some(&State::Spawning) {
            self.fail(name, format!("could not start: {error}"), now);
            self.advance(now);
        }
    }

    /// A `notify` service signalled that it is ready.
    pub fn notified(&mut self, name: &str, now: Instant) {
        let Some(entry) = self.entries.get_mut(name) else {
            return;
        };
        if let State::Starting {
            pid,
            timed_out: false,
            ..
        } = entry.state
        {
            entry.state = State::Running { pid, since: now };
            self.actions
                .push(Action::Log(format!("{name} is ready (pid {pid})")));
            self.advance(now);
        }
    }

    /// A process exited. Processes that are not services (orphans reaped by
    /// process 1) are ignored.
    pub fn exited(&mut self, pid: u32, status: ExitStatus, now: Instant) {
        let Some(name) = self
            .entries
            .iter()
            .find(|(_, entry)| entry.state.pid() == Some(pid))
            .map(|(name, _)| name.clone())
        else {
            return;
        };
        let shutting_down = self.shutting_down;
        let entry = self.entries.get_mut(&name).expect("found above");
        let Some(command) = entry.service.command.clone() else {
            return;
        };

        match entry.state.clone() {
            State::Stopping { .. } => {
                entry.state = State::Stopped;
                self.actions
                    .push(Action::Log(format!("stopped {name} ({status})")));
            }
            _ if shutting_down => {
                entry.state = State::Stopped;
                self.actions.push(Action::Log(format!(
                    "{name} exited during shutdown ({status})"
                )));
            }
            State::Starting { timed_out, .. } => {
                if timed_out {
                    let seconds = command.ready_timeout.as_secs();
                    self.fail(&name, format!("not ready within {seconds}s"), now);
                } else if command.ready == Ready::Exited && status.success() {
                    entry.state = State::Done;
                    entry.failures = 0;
                    self.actions.push(Action::Log(format!("finished {name}")));
                } else if command.ready == Ready::Exited {
                    self.fail(&name, format!("failed ({status})"), now);
                } else {
                    self.fail(&name, format!("exited before it was ready ({status})"), now);
                }
            }
            State::Running { since, .. } => {
                if now.saturating_duration_since(since) >= STABLE_AFTER {
                    entry.failures = 0;
                }
                let restart = match command.restart {
                    Restart::Always => true,
                    Restart::OnFailure => !status.success(),
                    Restart::Never => false,
                };
                if restart {
                    self.fail(&name, format!("exited ({status})"), now);
                } else if status.success() {
                    entry.state = State::Done;
                    self.actions
                        .push(Action::Log(format!("{name} exited ({status})")));
                } else {
                    entry.state = State::Failed(format!("exited ({status})"));
                    self.actions
                        .push(Action::Log(format!("{name} failed: exited ({status})")));
                }
            }
            _ => {}
        }

        if self.shutting_down {
            self.continue_shutdown(now);
        } else {
            self.advance(now);
        }
    }

    /// Stop everything, in reverse dependency order.
    pub fn shutdown(&mut self, now: Instant) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        self.actions.push(Action::Log("stopping services".into()));
        for entry in self.entries.values_mut() {
            if matches!(entry.state, State::Waiting | State::Backoff { .. }) {
                entry.state = State::Stopped;
            }
        }
        self.continue_shutdown(now);
    }

    /// Time passed: restart services whose backoff ended, and kill processes
    /// that missed their deadline.
    pub fn tick(&mut self, now: Instant) {
        let mut restarted = false;
        for (name, entry) in &mut self.entries {
            match &mut entry.state {
                State::Backoff { until } if *until <= now && !self.shutting_down => {
                    entry.state = State::Spawning;
                    self.actions.push(Action::Log(format!("restarting {name}")));
                    self.actions.push(Action::Spawn(name.clone()));
                    restarted = true;
                }
                State::Starting {
                    pid,
                    ready_by: Some(deadline),
                    timed_out,
                } if *deadline <= now && !*timed_out => {
                    *timed_out = true;
                    self.actions.push(Action::Log(format!(
                        "{name} did not become ready, killing it"
                    )));
                    self.actions.push(Action::Signal {
                        service: name.clone(),
                        pid: *pid,
                        signal: Signal::Kill,
                    });
                }
                State::Stopping {
                    pid,
                    kill_at,
                    killed,
                } if *kill_at <= now && !*killed => {
                    *killed = true;
                    self.actions.push(Action::Log(format!(
                        "{name} did not stop in time, killing it"
                    )));
                    self.actions.push(Action::Signal {
                        service: name.clone(),
                        pid: *pid,
                        signal: Signal::Kill,
                    });
                }
                _ => {}
            }
        }
        if restarted {
            self.advance(now);
        }
    }

    /// The earliest time [`Supervisor::tick`] has something to do.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.entries
            .values()
            .filter_map(|entry| match entry.state {
                State::Backoff { until } if !self.shutting_down => Some(until),
                State::Starting {
                    ready_by: Some(deadline),
                    timed_out: false,
                    ..
                } => Some(deadline),
                State::Stopping {
                    kill_at,
                    killed: false,
                    ..
                } => Some(kill_at),
                _ => None,
            })
            .min()
    }

    /// Shutdown was requested and every process has ended.
    pub fn finished(&self) -> bool {
        self.shutting_down && self.entries.values().all(|entry| !entry.state.is_active())
    }

    /// Count a failure and either schedule a restart or give up.
    fn fail(&mut self, name: &str, reason: String, now: Instant) {
        let shutting_down = self.shutting_down;
        let entry = self.entries.get_mut(name).expect("known service");
        let restart = entry
            .service
            .command
            .as_ref()
            .map_or(Restart::Never, |command| command.restart);
        entry.failures += 1;
        entry.state = if shutting_down {
            State::Stopped
        } else if restart == Restart::Never {
            self.actions
                .push(Action::Log(format!("{name} failed: {reason}")));
            State::Failed(reason)
        } else if entry.failures >= MAX_FAILURES {
            let failures = entry.failures;
            let reason = format!("{reason}; gave up after {failures} failures in a row");
            self.actions
                .push(Action::Log(format!("{name} failed: {reason}")));
            State::Failed(reason)
        } else {
            let delay = backoff(entry.failures);
            self.actions.push(Action::Log(format!(
                "{name} {reason}; restarting in {}s",
                delay.as_secs()
            )));
            State::Backoff { until: now + delay }
        };
    }

    /// Start every waiting service whose dependencies allow it, and skip
    /// those whose requirements failed. Repeats until nothing changes, since
    /// reaching a milestone can release more services.
    fn advance(&mut self, now: Instant) {
        if self.shutting_down {
            return;
        }
        loop {
            let mut changed = false;
            let names: Vec<String> = self
                .entries
                .iter()
                .filter(|(_, entry)| entry.state == State::Waiting)
                .map(|(name, _)| name.clone())
                .collect();
            for name in names {
                let service = &self.entries[&name].service;
                let state_of = |dependency: &String| self.entries.get(dependency).map(|e| &e.state);

                if let Some(dependency) = service
                    .requires
                    .iter()
                    .find(|d| state_of(d).is_some_and(State::is_hopeless))
                {
                    let reason = format!("requires {dependency}, which failed");
                    self.actions
                        .push(Action::Log(format!("skipping {name}: {reason}")));
                    self.entries.get_mut(&name).expect("known").state = State::Skipped(reason);
                    changed = true;
                    continue;
                }

                let requirements_ready = service
                    .requires
                    .iter()
                    .all(|d| state_of(d).is_some_and(State::is_ready));
                let predecessors_settled = service.after.iter().all(|d| match state_of(d) {
                    None => true,
                    Some(state) => state.is_ready() || state.is_hopeless(),
                });
                if !(requirements_ready && predecessors_settled) {
                    continue;
                }

                let entry = self.entries.get_mut(&name).expect("known");
                if entry.service.command.is_some() {
                    entry.state = State::Spawning;
                    self.actions.push(Action::Spawn(name));
                } else {
                    entry.state = State::Done;
                    self.actions.push(Action::Log(format!("reached {name}")));
                }
                changed = true;
            }
            if !changed {
                break;
            }
        }
        let _ = now;
    }

    /// Ask each running service to stop once everything depending on it has
    /// stopped.
    fn continue_shutdown(&mut self, now: Instant) {
        let to_stop: Vec<(String, u32, Duration)> = self
            .entries
            .iter()
            .filter_map(|(name, entry)| {
                let pid = match entry.state {
                    State::Starting { pid, .. } | State::Running { pid, .. } => pid,
                    _ => return None,
                };
                let dependents_down = self.dependents.get(name).is_none_or(|dependents| {
                    dependents.iter().all(|dependent| {
                        self.entries
                            .get(dependent)
                            .is_none_or(|e| !e.state.is_active())
                    })
                });
                let timeout = entry.service.command.as_ref()?.stop_timeout;
                dependents_down.then(|| (name.clone(), pid, timeout))
            })
            .collect();
        for (name, pid, timeout) in to_stop {
            self.actions.push(Action::Log(format!("stopping {name}")));
            self.actions.push(Action::Signal {
                service: name.clone(),
                pid,
                signal: Signal::Terminate,
            });
            self.entries.get_mut(&name).expect("known").state = State::Stopping {
                pid,
                kill_at: now + timeout,
                killed: false,
            };
        }
    }
}

/// The delay before restart number `failures`: 1s, 2s, 4s, ... up to 30s.
fn backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    (BACKOFF_FIRST * 2u32.pow(doublings)).min(BACKOFF_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use std::collections::BTreeSet;

    /// Build a supervisor from `(name, file contents)` pairs.
    fn supervisor(files: &[(&str, &str)], now: Instant) -> Supervisor {
        let services = files
            .iter()
            .map(|(name, text)| {
                let service = config::parse(name, text).unwrap().unwrap();
                (name.to_string(), service)
            })
            .collect();
        Supervisor::new(Plan::new(services, &BTreeSet::new()), now)
    }

    fn spawns(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::Spawn(name) => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    fn signals(actions: &[Action]) -> Vec<(String, Signal)> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::Signal {
                    service, signal, ..
                } => Some((service.clone(), *signal)),
                _ => None,
            })
            .collect()
    }

    const DAEMON: &str = r#"command = ["/bin/daemon"]"#;

    #[test]
    fn starts_in_dependency_order() {
        let t0 = Instant::now();
        let mut sup = supervisor(
            &[
                ("net", "command = [\"/bin/dhcp\"]\nready = \"exited\""),
                ("online", "requires = [\"net\"]"),
                ("sshd", "command = [\"/bin/sshd\"]\nrequires = [\"online\"]"),
                (
                    "web",
                    "command = [\"/bin/web\"]\nrequires = [\"online\"]\nafter = [\"sshd\"]",
                ),
                ("log", DAEMON),
            ],
            t0,
        );
        assert_eq!(spawns(&sup.take_actions()), ["log", "net"]);

        sup.spawned("log", 10, t0);
        sup.spawned("net", 11, t0);
        assert!(spawns(&sup.take_actions()).is_empty());

        // The one-off task finishing releases the milestone, then sshd.
        sup.exited(11, ExitStatus::code(0), t0);
        assert_eq!(sup.state("online"), Some(&State::Done));
        assert_eq!(spawns(&sup.take_actions()), ["sshd"]);

        // web waits for sshd to be running.
        sup.spawned("sshd", 12, t0);
        assert_eq!(spawns(&sup.take_actions()), ["web"]);
    }

    #[test]
    fn after_does_not_require_success() {
        let t0 = Instant::now();
        let mut sup = supervisor(
            &[
                ("flaky", "command = [\"/bin/x\"]\nready = \"exited\""),
                (
                    "web",
                    "command = [\"/bin/web\"]\nafter = [\"flaky\", \"absent\"]",
                ),
            ],
            t0,
        );
        assert_eq!(spawns(&sup.take_actions()), ["flaky"]);
        sup.spawned("flaky", 1, t0);
        sup.exited(1, ExitStatus::code(3), t0);
        assert!(matches!(sup.state("flaky"), Some(State::Failed(_))));
        assert_eq!(spawns(&sup.take_actions()), ["web"]);
    }

    #[test]
    fn failed_requirement_skips_dependents() {
        let t0 = Instant::now();
        let mut sup = supervisor(
            &[
                ("db", "command = [\"/bin/db\"]\nready = \"exited\""),
                ("app", "command = [\"/bin/app\"]\nrequires = [\"db\"]"),
                ("cron", "command = [\"/bin/cron\"]\nrequires = [\"app\"]"),
            ],
            t0,
        );
        sup.take_actions();
        sup.spawned("db", 1, t0);
        sup.exited(1, ExitStatus::code(1), t0);
        assert_eq!(
            sup.state("app"),
            Some(&State::Skipped("requires db, which failed".into()))
        );
        assert_eq!(
            sup.state("cron"),
            Some(&State::Skipped("requires app, which failed".into()))
        );
        assert!(spawns(&sup.take_actions()).is_empty());
    }

    #[test]
    fn restarts_with_backoff_then_gives_up() {
        let t0 = Instant::now();
        let mut sup = supervisor(&[("d", DAEMON)], t0);
        sup.take_actions();
        let mut now = t0;
        let mut delays = Vec::new();
        for attempt in 0..MAX_FAILURES {
            let pid = 100 + attempt;
            sup.spawned("d", pid, now);
            now += Duration::from_secs(1);
            sup.exited(pid, ExitStatus::code(1), now);
            match sup.state("d").unwrap().clone() {
                State::Backoff { until } => {
                    delays.push((until - now).as_secs());
                    // Nothing happens before the deadline.
                    sup.tick(until - Duration::from_millis(1));
                    assert!(spawns(&sup.take_actions()).is_empty());
                    assert_eq!(sup.next_deadline(), Some(until));
                    now = until;
                    sup.tick(now);
                    assert_eq!(spawns(&sup.take_actions()), ["d"]);
                }
                State::Failed(reason) => {
                    assert!(reason.contains("gave up after 5"), "{reason}");
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(delays, [1, 2, 4, 8]);
        assert!(matches!(sup.state("d"), Some(State::Failed(_))));
    }

    #[test]
    fn staying_up_resets_the_failure_count() {
        let t0 = Instant::now();
        let mut sup = supervisor(&[("d", DAEMON)], t0);
        sup.take_actions();
        sup.spawned("d", 1, t0);
        sup.exited(1, ExitStatus::code(1), t0);
        sup.tick(t0 + Duration::from_secs(1));
        sup.spawned("d", 2, t0 + Duration::from_secs(1));
        // Up long enough to count as healthy: the next delay is 1s again.
        let later = t0 + Duration::from_secs(1) + STABLE_AFTER;
        sup.exited(2, ExitStatus::signal(9), later);
        assert_eq!(
            sup.state("d"),
            Some(&State::Backoff {
                until: later + Duration::from_secs(1)
            })
        );
    }

    #[test]
    fn restart_policies() {
        let t0 = Instant::now();
        let mut sup = supervisor(
            &[
                ("never", "command = [\"/bin/x\"]\nrestart = \"never\""),
                ("onfail", DAEMON),
                ("always", "command = [\"/bin/x\"]\nrestart = \"always\""),
            ],
            t0,
        );
        sup.take_actions();
        sup.spawned("always", 1, t0);
        sup.spawned("never", 2, t0);
        sup.spawned("onfail", 3, t0);
        sup.exited(1, ExitStatus::code(0), t0);
        sup.exited(2, ExitStatus::code(1), t0);
        sup.exited(3, ExitStatus::code(0), t0);
        assert!(matches!(sup.state("always"), Some(State::Backoff { .. })));
        assert_eq!(
            sup.state("never"),
            Some(&State::Failed("exited (exit code 1)".into()))
        );
        assert_eq!(sup.state("onfail"), Some(&State::Done));
    }

    #[test]
    fn notify_readiness_and_timeout() {
        let t0 = Instant::now();
        let mut sup = supervisor(
            &[
                (
                    "db",
                    "command = [\"/bin/db\"]\nready = \"notify\"\nready_timeout = 5",
                ),
                ("app", "command = [\"/bin/app\"]\nrequires = [\"db\"]"),
            ],
            t0,
        );
        sup.take_actions();
        sup.spawned("db", 7, t0);
        assert!(spawns(&sup.take_actions()).is_empty(), "app waits for db");
        sup.notified("db", t0 + Duration::from_secs(1));
        assert_eq!(spawns(&sup.take_actions()), ["app"]);

        // A second instance that never notifies is killed, then restarted.
        let mut sup = supervisor(
            &[(
                "db",
                "command = [\"/bin/db\"]\nready = \"notify\"\nready_timeout = 5",
            )],
            t0,
        );
        sup.take_actions();
        sup.spawned("db", 8, t0);
        assert_eq!(sup.next_deadline(), Some(t0 + Duration::from_secs(5)));
        sup.tick(t0 + Duration::from_secs(5));
        assert_eq!(
            signals(&sup.take_actions()),
            [("db".to_string(), Signal::Kill)]
        );
        sup.exited(8, ExitStatus::signal(9), t0 + Duration::from_secs(5));
        assert!(matches!(sup.state("db"), Some(State::Backoff { .. })));
    }

    #[test]
    fn exiting_before_ready_is_a_failure() {
        let t0 = Instant::now();
        let mut sup = supervisor(
            &[(
                "db",
                "command = [\"/bin/db\"]\nready = \"notify\"\nrestart = \"never\"",
            )],
            t0,
        );
        sup.take_actions();
        sup.spawned("db", 1, t0);
        sup.exited(1, ExitStatus::code(0), t0);
        assert_eq!(
            sup.state("db"),
            Some(&State::Failed(
                "exited before it was ready (exit code 0)".into()
            ))
        );
    }

    #[test]
    fn spawn_failure_is_a_failure() {
        let t0 = Instant::now();
        let mut sup = supervisor(
            &[("d", "command = [\"/bin/missing\"]\nrestart = \"never\"")],
            t0,
        );
        sup.take_actions();
        sup.spawn_failed("d", "No such file or directory", t0);
        assert_eq!(
            sup.state("d"),
            Some(&State::Failed(
                "could not start: No such file or directory".into()
            ))
        );
    }

    #[test]
    fn shutdown_stops_dependents_first_and_kills_stragglers() {
        let t0 = Instant::now();
        let mut sup = supervisor(
            &[
                ("net", DAEMON),
                (
                    "sshd",
                    "command = [\"/bin/sshd\"]\nrequires = [\"net\"]\nstop_timeout = 3",
                ),
                ("web", "command = [\"/bin/web\"]\nafter = [\"sshd\"]"),
                (
                    "later",
                    "command = [\"/bin/x\"]\nrequires = [\"web\"]\nready = \"exited\"",
                ),
            ],
            t0,
        );
        sup.take_actions();
        sup.spawned("net", 1, t0);
        sup.spawned("sshd", 2, t0);
        sup.spawned("web", 3, t0);
        sup.take_actions();
        assert_eq!(sup.state("later"), Some(&State::Spawning));
        sup.spawned("later", 4, t0);
        sup.take_actions();

        sup.shutdown(t0);
        // Only the services nothing running depends on are stopped first.
        assert_eq!(
            signals(&sup.take_actions()),
            [("later".to_string(), Signal::Terminate)]
        );
        sup.exited(4, ExitStatus::signal(15), t0);
        assert_eq!(
            signals(&sup.take_actions()),
            [("web".to_string(), Signal::Terminate)]
        );
        sup.exited(3, ExitStatus::code(0), t0);
        assert_eq!(
            signals(&sup.take_actions()),
            [("sshd".to_string(), Signal::Terminate)]
        );

        // sshd ignores SIGTERM: it is killed after its stop_timeout.
        assert_eq!(sup.next_deadline(), Some(t0 + Duration::from_secs(3)));
        sup.tick(t0 + Duration::from_secs(3));
        assert_eq!(
            signals(&sup.take_actions()),
            [("sshd".to_string(), Signal::Kill)]
        );
        assert!(!sup.finished());
        sup.exited(2, ExitStatus::signal(9), t0 + Duration::from_secs(3));
        assert_eq!(
            signals(&sup.take_actions()),
            [("net".to_string(), Signal::Terminate)]
        );
        sup.exited(1, ExitStatus::code(0), t0 + Duration::from_secs(3));
        assert!(sup.finished());
        assert!(
            spawns(&sup.take_actions()).is_empty(),
            "nothing restarts during shutdown"
        );
    }

    #[test]
    fn shutdown_while_spawning_or_backing_off() {
        let t0 = Instant::now();
        let mut sup = supervisor(&[("a", DAEMON), ("b", DAEMON)], t0);
        sup.take_actions();
        sup.spawned("a", 1, t0);
        sup.exited(1, ExitStatus::code(1), t0);
        assert!(matches!(sup.state("a"), Some(State::Backoff { .. })));

        sup.shutdown(t0);
        assert_eq!(sup.state("a"), Some(&State::Stopped));
        assert!(!sup.finished(), "b is still being spawned");
        sup.spawned("b", 2, t0);
        assert_eq!(
            signals(&sup.take_actions()),
            [("b".to_string(), Signal::Terminate)]
        );
        sup.exited(2, ExitStatus::code(0), t0);
        assert!(sup.finished());
        assert_eq!(sup.next_deadline(), None);
    }

    #[test]
    fn orphans_are_ignored() {
        let t0 = Instant::now();
        let mut sup = supervisor(&[("a", DAEMON)], t0);
        sup.take_actions();
        sup.spawned("a", 1, t0);
        sup.exited(999, ExitStatus::code(0), t0);
        assert_eq!(sup.state("a"), Some(&State::Running { pid: 1, since: t0 }));
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let secs: Vec<u64> = (1..=8).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30, 30]);
    }
}
