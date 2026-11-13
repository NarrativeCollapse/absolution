//! End-to-end tests: the real binary supervising real processes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_absinit");

/// A running absinit over a temporary services directory, killed on drop.
struct Init {
    child: Child,
    dir: tempfile::TempDir,
}

impl Init {
    fn start(services: &[(&str, String)]) -> Init {
        let dir = tempfile::tempdir().unwrap();
        let services_dir = dir.path().join("services");
        fs::create_dir(&services_dir).unwrap();
        for (name, text) in services {
            fs::write(services_dir.join(format!("{name}.toml")), text).unwrap();
        }
        let stderr = fs::File::create(dir.path().join("absinit.log")).unwrap();
        let child = Command::new(BIN)
            .arg("--services")
            .arg(&services_dir)
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .unwrap();
        Init { child, dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.path(name)).unwrap_or_default()
    }

    /// Wait until `name` satisfies `done`, or fail with both logs.
    fn wait_for(&self, name: &str, what: &str, done: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if done(&self.read(name)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "timed out waiting for {what}\n--- {name} ---\n{}\n--- absinit.log ---\n{}",
            self.read(name),
            self.read("absinit.log")
        );
    }

    /// Send SIGTERM and wait for absinit to exit.
    fn shut_down(mut self) -> (std::process::ExitStatus, String) {
        unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return (status, self.read("absinit.log"));
            }
            assert!(
                Instant::now() < deadline,
                "absinit did not exit:\n{}",
                self.read("absinit.log")
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Init {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A shell service. `script` may use $LOG, which appends to the shared log.
fn sh(log: &Path, script: &str, extra: &str) -> String {
    let script = script.replace("$LOG", &log.display().to_string());
    format!(
        "command = [\"/bin/sh\", \"-c\", {}]\nenvironment = {{ PATH = \"/usr/bin:/bin\" }}\n{extra}",
        toml::Value::String(script)
    )
}

/// A long-running service that logs when it starts and when it is stopped.
/// It reports readiness only after logging, so that services depending on it
/// start strictly afterwards; with `ready = "started"` they could start as
/// soon as its process exists.
fn daemon(log: &Path, name: &str, extra: &str) -> String {
    sh(
        log,
        &format!(
            "trap 'echo stop-{name} >> $LOG; exit 0' TERM; echo start-{name} >> $LOG; \
             printf x > /dev/fd/$ABSOLUTION_NOTIFY_FD; while :; do sleep 0.05; done"
        ),
        &format!("ready = \"notify\"\n{extra}"),
    )
}

fn lines(text: &str) -> Vec<&str> {
    text.lines().collect()
}

#[test]
fn starts_in_order_and_stops_in_reverse() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("events");
    let init = Init::start(&[
        (
            "setup",
            sh(&log, "echo setup >> $LOG", "ready = \"exited\""),
        ),
        ("base", "requires = [\"setup\"]".to_string()),
        ("a", daemon(&log, "a", "requires = [\"base\"]")),
        ("b", daemon(&log, "b", "requires = [\"a\"]")),
        ("c", daemon(&log, "c", "after = [\"b\", \"absent\"]")),
    ]);
    let wait_log = |what: &str, done: &dyn Fn(&str) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !done(&fs::read_to_string(&log).unwrap_or_default()) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}:\n{}",
                init.read("absinit.log")
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    wait_log("all started", &|text| text.contains("start-c"));
    assert_eq!(
        lines(&fs::read_to_string(&log).unwrap()),
        ["setup", "start-a", "start-b", "start-c"]
    );

    let (status, absinit_log) = init.shut_down();
    assert!(status.success(), "{absinit_log}");
    let events = fs::read_to_string(&log).unwrap();
    let stops: Vec<&str> = lines(&events)
        .into_iter()
        .filter(|l| l.starts_with("stop-"))
        .collect();
    assert_eq!(stops, ["stop-c", "stop-b", "stop-a"], "{absinit_log}");
    assert!(absinit_log.contains("reached base"), "{absinit_log}");
}

#[test]
fn notify_readiness_gates_dependents() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("events");
    let init = Init::start(&[
        (
            "db",
            sh(
                &log,
                "trap 'exit 0' TERM; sleep 0.3; echo db-ready >> $LOG; \
                 printf x > /dev/fd/$ABSOLUTION_NOTIFY_FD; while :; do sleep 0.05; done",
                "ready = \"notify\"",
            ),
        ),
        (
            "app",
            sh(
                &log,
                "echo app >> $LOG",
                "ready = \"exited\"\nrequires = [\"db\"]",
            ),
        ),
    ]);
    init.wait_for("absinit.log", "app to finish", |text| {
        text.contains("finished app")
    });
    assert_eq!(
        lines(&fs::read_to_string(&log).unwrap()),
        ["db-ready", "app"]
    );
    let (status, absinit_log) = init.shut_down();
    assert!(status.success());
    assert!(absinit_log.contains("db is ready"), "{absinit_log}");
}

#[test]
fn failing_services_are_restarted() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("events");
    let counter = tmp.path().join("counter");
    let script = format!(
        "trap 'exit 0' TERM; n=$(cat {c} 2>/dev/null || echo 0); n=$((n + 1)); echo $n > {c}; \
         echo try-$n >> $LOG; [ $n -ge 3 ] || exit 1; while :; do sleep 0.05; done",
        c = counter.display()
    );
    let init = Init::start(&[("flaky", sh(&log, &script, ""))]);
    init.wait_for("absinit.log", "the third attempt", |_| {
        fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("try-3")
    });
    let (status, absinit_log) = init.shut_down();
    assert!(status.success());
    assert_eq!(
        lines(&fs::read_to_string(&log).unwrap()),
        ["try-1", "try-2", "try-3"]
    );
    assert!(absinit_log.contains("restarting in 1s"), "{absinit_log}");
    assert!(absinit_log.contains("restarting in 2s"), "{absinit_log}");
}

#[test]
fn services_get_only_their_own_environment_and_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("events");
    let workdir = tmp.path().join("work");
    fs::create_dir(&workdir).unwrap();
    let service = format!(
        "command = [\"/bin/sh\", \"-c\", \"pwd > {log}; env | sort >> {log}\"]\n\
         environment = {{ GREETING = \"hello\" }}\nworking_directory = \"{dir}\"\nready = \"exited\"",
        log = log.display(),
        dir = workdir.display()
    );
    let init = Init::start(&[("show", service)]);
    init.wait_for("absinit.log", "the task", |text| {
        text.contains("finished show")
    });
    let output = fs::read_to_string(&log).unwrap();
    let mut output_lines = output.lines();
    assert_eq!(output_lines.next(), Some(workdir.to_str().unwrap()));
    let variables: Vec<&str> = output_lines
        .map(|line| line.split('=').next().unwrap())
        // The shell sets a few variables of its own.
        .filter(|name| !["PWD", "SHLVL", "_", "OLDPWD"].contains(name))
        .collect();
    assert_eq!(variables, ["GREETING"], "{output}");
    init.shut_down();
}

#[test]
fn check_reports_problems_without_starting_anything() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("good.toml"), "command = [\"/bin/true\"]").unwrap();
    fs::write(dir.path().join("typo.toml"), "comand = [\"/bin/true\"]").unwrap();
    fs::write(
        dir.path().join("needy.toml"),
        "command = [\"/bin/true\"]\nrequires = [\"typo\"]",
    )
    .unwrap();

    let output = Command::new(BIN)
        .args(["--check", "--services"])
        .arg(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stdout.contains("good: ok"), "{stdout}");
    assert!(
        stdout.contains("needy: skipped: requires typo, which could not be loaded"),
        "{stdout}"
    );
    assert!(
        stderr.contains("typo.toml") && stderr.contains("comand"),
        "{stderr}"
    );

    fs::remove_file(dir.path().join("typo.toml")).unwrap();
    fs::remove_file(dir.path().join("needy.toml")).unwrap();
    let output = Command::new(BIN)
        .args(["--check", "--services"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
}
