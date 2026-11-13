//! Service files: parsing, validation and loading from directories.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// Where packages install service files.
pub const VENDOR_DIR: &str = "/usr/lib/absolution/services";
/// Where the administrator overrides them.
pub const ADMIN_DIR: &str = "/etc/absolution/services";

const DEFAULT_STOP_TIMEOUT: u64 = 10;
const DEFAULT_READY_TIMEOUT: u64 = 60;

/// When a service counts as ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Ready {
    /// As soon as its process is running.
    Started,
    /// When its process exits successfully (a one-off task).
    Exited,
    /// When it writes a byte to the descriptor in `ABSOLUTION_NOTIFY_FD`.
    Notify,
}

/// When a service is restarted after its process ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    Never,
    /// After a non-zero exit or a signal.
    OnFailure,
    Always,
}

/// What to run for a service.
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    /// Program (an absolute path) followed by its arguments.
    pub argv: Vec<String>,
    pub ready: Ready,
    pub restart: Restart,
    pub environment: BTreeMap<String, String>,
    pub working_directory: PathBuf,
    pub stop_timeout: Duration,
    pub ready_timeout: Duration,
}

/// A validated service.
#[derive(Debug, Clone, PartialEq)]
pub struct Service {
    pub name: String,
    pub description: String,
    /// `None` for a milestone, which only groups requirements.
    pub command: Option<Command>,
    pub requires: Vec<String>,
    pub after: Vec<String>,
}

/// A service file as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceFile {
    description: Option<String>,
    command: Option<Vec<String>>,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    after: Vec<String>,
    ready: Option<Ready>,
    restart: Option<Restart>,
    environment: Option<BTreeMap<String, String>>,
    working_directory: Option<PathBuf>,
    stop_timeout: Option<u64>,
    ready_timeout: Option<u64>,
    #[serde(default)]
    disabled: bool,
}

/// Whether `name` is a valid service name.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
}

/// Parse the service file `<name>.toml`. Returns `None` for a disabled
/// service.
pub fn parse(name: &str, text: &str) -> Result<Option<Service>, String> {
    if !valid_name(name) {
        return Err(format!(
            "invalid service name {name:?}: use lowercase letters, digits, '.', '_' and '-'"
        ));
    }
    let file: ServiceFile = toml::from_str(text).map_err(|e| e.to_string())?;
    if file.disabled {
        return Ok(None);
    }

    for dependency in file.requires.iter().chain(&file.after) {
        if !valid_name(dependency) {
            return Err(format!("invalid dependency name {dependency:?}"));
        }
        if dependency == name {
            return Err("a service cannot depend on itself".into());
        }
    }

    let command = match file.command {
        None => {
            let command_only = [
                ("ready", file.ready.is_some()),
                ("restart", file.restart.is_some()),
                ("environment", file.environment.is_some()),
                ("working_directory", file.working_directory.is_some()),
                ("stop_timeout", file.stop_timeout.is_some()),
                ("ready_timeout", file.ready_timeout.is_some()),
            ];
            if let Some((key, _)) = command_only.iter().find(|(_, set)| *set) {
                return Err(format!("`{key}` needs a `command`"));
            }
            None
        }
        Some(argv) => {
            let Some(program) = argv.first() else {
                return Err("`command` is empty".into());
            };
            if !program.starts_with('/') {
                return Err(format!(
                    "the program {program:?} must be an absolute path; init has no PATH"
                ));
            }
            let ready = file.ready.unwrap_or(Ready::Started);
            let restart = file.restart.unwrap_or(match ready {
                Ready::Exited => Restart::Never,
                Ready::Started | Ready::Notify => Restart::OnFailure,
            });
            if ready == Ready::Exited && restart == Restart::Always {
                return Err(
                    "`restart = \"always\"` cannot be used with `ready = \"exited\"`".into(),
                );
            }
            if file.ready_timeout.is_some() && ready != Ready::Notify {
                return Err("`ready_timeout` only applies to `ready = \"notify\"`".into());
            }
            let seconds = |value: Option<u64>, default: u64, key: &str| match value {
                Some(0) => Err(format!("`{key}` must be at least 1 second")),
                value => Ok(Duration::from_secs(value.unwrap_or(default))),
            };
            let working_directory = file.working_directory.unwrap_or_else(|| PathBuf::from("/"));
            if !working_directory.is_absolute() {
                return Err("`working_directory` must be an absolute path".into());
            }
            Some(Command {
                argv,
                ready,
                restart,
                environment: file.environment.unwrap_or_default(),
                working_directory,
                stop_timeout: seconds(file.stop_timeout, DEFAULT_STOP_TIMEOUT, "stop_timeout")?,
                ready_timeout: seconds(file.ready_timeout, DEFAULT_READY_TIMEOUT, "ready_timeout")?,
            })
        }
    };

    Ok(Some(Service {
        name: name.to_string(),
        description: file.description.unwrap_or_else(|| name.to_string()),
        command,
        requires: file.requires,
        after: file.after,
    }))
}

/// The services found in a set of directories.
#[derive(Debug, Default)]
pub struct Loaded {
    pub services: BTreeMap<String, Service>,
    /// Files that could not be used, with the reason. Their services are
    /// absent from `services`.
    pub errors: Vec<(PathBuf, String)>,
}

/// Load every `*.toml` file from `dirs`, in order: a later directory's file
/// replaces an earlier one of the same name. A missing directory is fine.
pub fn load(dirs: &[PathBuf]) -> Loaded {
    let mut loaded = Loaded::default();
    for dir in dirs {
        let mut files: Vec<PathBuf> = match fs::read_dir(dir) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                loaded.errors.push((dir.clone(), e.to_string()));
                continue;
            }
        };
        files.sort();
        for path in files {
            load_file(&path, &mut loaded);
        }
    }
    loaded
}

fn load_file(path: &Path, loaded: &mut Loaded) {
    let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
        loaded
            .errors
            .push((path.to_path_buf(), "file name is not UTF-8".into()));
        return;
    };
    // A broken override must not fall back to the file it overrides.
    loaded.services.remove(name);
    let result = fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|text| parse(name, &text));
    match result {
        Ok(Some(service)) => {
            loaded.services.insert(name.to_string(), service);
        }
        Ok(None) => {}
        Err(reason) => loaded.errors.push((path.to_path_buf(), reason)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> Service {
        parse("svc", text).unwrap().unwrap()
    }

    fn err(text: &str) -> String {
        parse("svc", text).unwrap_err()
    }

    #[test]
    fn defaults() {
        let service = ok(r#"command = ["/bin/daemon", "-f"]"#);
        assert_eq!(service.description, "svc");
        let command = service.command.unwrap();
        assert_eq!(command.argv, ["/bin/daemon", "-f"]);
        assert_eq!(command.ready, Ready::Started);
        assert_eq!(command.restart, Restart::OnFailure);
        assert_eq!(command.working_directory, PathBuf::from("/"));
        assert_eq!(command.stop_timeout, Duration::from_secs(10));

        let oneshot = ok("command = [\"/bin/setup\"]\nready = \"exited\"");
        assert_eq!(oneshot.command.unwrap().restart, Restart::Never);
    }

    #[test]
    fn milestones_have_no_command() {
        let milestone = ok("description = \"Network is up\"\nrequires = [\"dhcp\"]");
        assert_eq!(milestone.command, None);
        assert_eq!(milestone.requires, ["dhcp"]);
        assert!(err("requires = [\"x\"]\nrestart = \"always\"").contains("needs a `command`"));
    }

    #[test]
    fn rejects_mistakes() {
        assert!(err("comand = [\"/bin/x\"]").contains("comand"));
        assert!(err("command = []").contains("empty"));
        assert!(err("command = [\"daemon\"]").contains("absolute path"));
        assert!(err("command = [\"/bin/x\"]\nready = \"soon\"").contains("soon"));
        assert!(
            err("command = [\"/bin/x\"]\nready = \"exited\"\nrestart = \"always\"")
                .contains("cannot be used")
        );
        assert!(err("command = [\"/bin/x\"]\nready_timeout = 5").contains("notify"));
        assert!(err("command = [\"/bin/x\"]\nstop_timeout = 0").contains("at least 1"));
        assert!(err("command = [\"/bin/x\"]\nworking_directory = \"var\"").contains("absolute"));
        assert!(err("requires = [\"Bad Name\"]").contains("invalid dependency"));
        assert!(err("after = [\"svc\"]").contains("itself"));
        assert!(
            parse("Bad", "")
                .unwrap_err()
                .contains("invalid service name")
        );
    }

    #[test]
    fn disabled_services_are_masked() {
        assert_eq!(parse("svc", "disabled = true").unwrap(), None);
    }

    #[test]
    fn names() {
        for good in ["a", "sshd", "net-dhcp", "0day", "x.y_z"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in ["", "-a", ".a", "A", "a b", "a/b"] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn later_directories_override_and_errors_do_not_fall_back() {
        let tmp = tempfile::tempdir().unwrap();
        let vendor = tmp.path().join("vendor");
        let admin = tmp.path().join("admin");
        fs::create_dir_all(&vendor).unwrap();
        fs::create_dir_all(&admin).unwrap();
        fs::write(vendor.join("a.toml"), "command = [\"/bin/a\"]").unwrap();
        fs::write(vendor.join("b.toml"), "command = [\"/bin/b\"]").unwrap();
        fs::write(vendor.join("c.toml"), "command = [\"/bin/c\"]").unwrap();
        fs::write(vendor.join("notes.txt"), "ignored").unwrap();
        fs::write(admin.join("a.toml"), "command = [\"/usr/bin/a2\"]").unwrap();
        fs::write(admin.join("b.toml"), "disabled = true").unwrap();
        fs::write(admin.join("c.toml"), "command = \"oops\"").unwrap();

        let loaded = load(&[vendor, admin.clone(), tmp.path().join("missing")]);
        let names: Vec<&str> = loaded.services.keys().map(String::as_str).collect();
        assert_eq!(names, ["a"]);
        assert_eq!(
            loaded.services["a"].command.as_ref().unwrap().argv,
            ["/usr/bin/a2"]
        );
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(loaded.errors[0].0, admin.join("c.toml"));
    }
}
