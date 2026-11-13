//! Carrying out the supervisor's decisions with processes, pipes and
//! signals. Only POSIX interfaces are used, so this runs unchanged on
//! Absolution and on development hosts.

use std::fs::File;
use std::io::Read;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Ready;
use crate::supervisor::{Action, ExitStatus, Signal, Supervisor};

/// The environment variable telling a `notify` service where to write.
pub const NOTIFY_FD_VAR: &str = "ABSOLUTION_NOTIFY_FD";

enum Event {
    Exited { pid: u32, status: ExitStatus },
    Notified(String),
    Shutdown,
}

/// Write end of the pipe the signal handler uses to wake the main loop.
static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(_signal: libc::c_int) {
    let fd = SIGNAL_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = 1u8;
        // write() is async-signal-safe; a full pipe already means "wake up".
        unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
    }
}

fn log(message: &str) {
    eprintln!("absinit: {message}");
}

/// A pipe whose descriptors are closed on exec.
fn cloexec_pipe() -> std::io::Result<(RawFd, RawFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    for fd in fds {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok((fds[0], fds[1]))
}

/// Run the supervisor until shutdown completes. As process 1, keep reaping
/// orphans afterwards instead of returning.
pub fn run(mut supervisor: Supervisor) {
    let is_pid1 = std::process::id() == 1;
    let (events, receiver) = mpsc::channel();
    start_reaper(events.clone());
    if let Err(e) = start_signal_listener(events.clone()) {
        log(&format!("cannot handle shutdown signals: {e}"));
    }

    loop {
        perform(&mut supervisor, &events);
        if supervisor.finished() {
            log("all services stopped");
            if is_pid1 {
                // Powering off is not implemented yet; keep reaping.
                loop {
                    thread::sleep(Duration::from_secs(3600));
                }
            }
            return;
        }

        let event = match supervisor.next_deadline() {
            Some(deadline) => {
                let timeout = deadline.saturating_duration_since(Instant::now());
                match receiver.recv_timeout(timeout) {
                    Ok(event) => Some(event),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            None => match receiver.recv() {
                Ok(event) => Some(event),
                Err(_) => return,
            },
        };

        let now = Instant::now();
        match event {
            Some(Event::Exited { pid, status }) => supervisor.exited(pid, status, now),
            Some(Event::Notified(name)) => supervisor.notified(&name, now),
            Some(Event::Shutdown) => supervisor.shutdown(now),
            None => supervisor.tick(now),
        }
    }
}

/// Carry out actions until the supervisor has no more, since spawning one
/// service can make it decide more.
fn perform(supervisor: &mut Supervisor, events: &Sender<Event>) {
    loop {
        let actions = supervisor.take_actions();
        if actions.is_empty() {
            return;
        }
        for action in actions {
            match action {
                Action::Log(message) => log(&message),
                Action::Signal { pid, signal, .. } => {
                    let signal = match signal {
                        Signal::Terminate => libc::SIGTERM,
                        Signal::Kill => libc::SIGKILL,
                    };
                    unsafe { libc::kill(pid as libc::pid_t, signal) };
                }
                Action::Spawn(name) => {
                    let now = Instant::now();
                    match spawn(supervisor, &name, events) {
                        Ok(pid) => supervisor.spawned(&name, pid, now),
                        Err(e) => supervisor.spawn_failed(&name, &e, now),
                    }
                }
            }
        }
    }
}

/// Start a service's process with a clean environment.
fn spawn(supervisor: &Supervisor, name: &str, events: &Sender<Event>) -> Result<u32, String> {
    let command = supervisor
        .service(name)
        .and_then(|service| service.command.as_ref())
        .ok_or("service has no command")?;

    let mut process = Command::new(&command.argv[0]);
    process
        .args(&command.argv[1..])
        .env_clear()
        .envs(&command.environment)
        .current_dir(&command.working_directory)
        .stdin(Stdio::null());

    let notify = if command.ready == Ready::Notify {
        let (read_fd, write_fd) = cloexec_pipe().map_err(|e| e.to_string())?;
        process.env(NOTIFY_FD_VAR, write_fd.to_string());
        // In the child only, keep the write end open across exec.
        unsafe {
            process.pre_exec(move || {
                if libc::fcntl(write_fd, libc::F_SETFD, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Some((read_fd, write_fd))
    } else {
        None
    };

    let spawned = process.spawn();
    if let Some((read_fd, write_fd)) = notify {
        // The parent's copy of the write end would keep the pipe open forever.
        unsafe { libc::close(write_fd) };
        match &spawned {
            Ok(_) => watch_notify(name.to_string(), read_fd, events.clone()),
            Err(_) => unsafe {
                libc::close(read_fd);
            },
        }
    }
    // The reaper collects the exit status, so the Child handle is dropped
    // without waiting.
    spawned.map(|child| child.id()).map_err(|e| e.to_string())
}

/// Report readiness when the service writes to its notify pipe.
fn watch_notify(name: String, read_fd: RawFd, events: Sender<Event>) {
    thread::spawn(move || {
        let mut pipe = unsafe { File::from_raw_fd(read_fd) };
        let mut byte = [0u8; 1];
        if matches!(pipe.read(&mut byte), Ok(1)) {
            let _ = events.send(Event::Notified(name));
        }
    });
}

/// Collect the exit status of every child, including orphans when running
/// as process 1.
fn start_reaper(events: Sender<Event>) {
    thread::spawn(move || {
        loop {
            let mut status = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
            if pid > 0 {
                let status = std::process::ExitStatus::from_raw(status);
                let status = ExitStatus {
                    code: status.code(),
                    signal: status.signal(),
                };
                if events
                    .send(Event::Exited {
                        pid: pid as u32,
                        status,
                    })
                    .is_err()
                {
                    return;
                }
                continue;
            }
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => {}
                // No children right now; check again shortly.
                _ => thread::sleep(Duration::from_millis(50)),
            }
        }
    });
}

/// Turn SIGTERM and SIGINT into a shutdown request.
fn start_signal_listener(events: Sender<Event>) -> std::io::Result<()> {
    let (read_fd, write_fd) = cloexec_pipe()?;
    SIGNAL_PIPE.store(write_fd, Ordering::Relaxed);
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
    thread::spawn(move || {
        let mut pipe = unsafe { File::from_raw_fd(read_fd) };
        let mut byte = [0u8; 1];
        while matches!(pipe.read(&mut byte), Ok(1)) {
            if events.send(Event::Shutdown).is_err() {
                return;
            }
        }
    });
    Ok(())
}
