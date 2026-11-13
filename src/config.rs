//! Cookbook configuration.
//!
//! Settings come from three places, in decreasing priority: the `[cook]`
//! table of `cookbook.toml`, `COOKBOOK_*` environment variables, and built-in
//! defaults. `cookbook.lock` holds per-recipe overrides written by the
//! `capture-rev` and `change-rule` commands.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::Path,
    sync::OnceLock,
};

use serde::{Deserialize, Serialize};

use crate::{Error, Result, wrap_io_err};

const CONFIG_PATH: &str = "cookbook.toml";
const LOCK_PATH: &str = "cookbook.lock";

/// Mirror applied when `cookbook.toml` configures none.
const DEFAULT_MIRROR: (&str, &str) = ("ftp.gnu.org/gnu", "mirrors.ocf.berkeley.edu/gnu");

/// The `[cook]` table of `cookbook.toml`. Every key is optional.
#[derive(Debug, Default, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
struct CookSettings {
    offline: Option<bool>,
    jobs: Option<usize>,
    tui: Option<bool>,
    logs: Option<bool>,
    nonstop: Option<bool>,
    compressed: Option<bool>,
    git_treeless: Option<bool>,
    verbose: Option<bool>,
    verbose_cmd: Option<bool>,
    clean_build: Option<bool>,
    clean_target: Option<bool>,
    write_filetree: Option<bool>,
}

/// Fully resolved build settings.
#[derive(Debug, Clone, PartialEq)]
pub struct CookConfig {
    /// Avoid network access where possible.
    pub offline: bool,
    /// Number of parallel build jobs handed to make, ninja and cargo.
    pub jobs: usize,
    /// Use the TUI, which allows parallel builds. Off when `CI` is set.
    pub tui: bool,
    /// Write build logs to `build/logs`. Defaults to the value of `tui`.
    pub logs: bool,
    /// Keep going after a recipe fails to build.
    pub nonstop: bool,
    /// Compress package archives.
    pub compressed: bool,
    /// Clone git sources as treeless clones unless the recipe says otherwise.
    pub git_treeless: bool,
    /// Print progress details. Build failures are printed regardless.
    pub verbose: bool,
    /// Trace build script commands (`bash -x`, `make V=1`).
    pub verbose_cmd: bool,
    /// Wipe the build directory before every build.
    pub clean_build: bool,
    /// Wipe everything except package archives after every build.
    pub clean_target: bool,
    /// Write the `stage.files` listing of each package.
    pub write_filetree: bool,
}

impl CookConfig {
    /// Resolve settings from the config file, then `env`, then defaults.
    fn resolve(file: CookSettings, env: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        let flag = |key: &str, from_file: Option<bool>, default: bool| -> Result<bool> {
            match (from_file, env(key)) {
                (Some(value), _) => Ok(value),
                (None, Some(raw)) => parse_bool(key, &raw),
                (None, None) => Ok(default),
            }
        };

        let jobs = match (file.jobs, env("COOKBOOK_MAKE_JOBS")) {
            (Some(jobs), _) => jobs,
            (None, Some(raw)) => raw.trim().parse().map_err(|_| {
                Error::Other(format!(
                    "COOKBOOK_MAKE_JOBS={raw:?} is not a number of jobs"
                ))
            })?,
            (None, None) => std::thread::available_parallelism().map_or(1, usize::from),
        };
        if jobs == 0 {
            return Err(Error::Other("The number of jobs must be at least 1".into()));
        }

        let in_ci = env("CI").is_some_and(|value| !value.is_empty());
        let tui = file.tui.unwrap_or(!in_ci);
        let clean_target = flag("COOKBOOK_CLEAN_TARGET", file.clean_target, false)?;
        let web = flag("COOKBOOK_WEB", None, false)?;

        Ok(CookConfig {
            offline: flag("COOKBOOK_OFFLINE", file.offline, false)?,
            jobs,
            tui,
            logs: flag("COOKBOOK_LOGS", file.logs, tui)?,
            nonstop: flag("COOKBOOK_NONSTOP", file.nonstop, false)?,
            compressed: flag("COOKBOOK_COMPRESSED", file.compressed, false)?,
            git_treeless: flag("COOKBOOK_GIT_TREELESS", file.git_treeless, false)?,
            verbose: flag("COOKBOOK_VERBOSE", file.verbose, true)?,
            verbose_cmd: flag("COOKBOOK_VERBOSE_CMD", file.verbose_cmd, false)?,
            clean_build: flag("COOKBOOK_CLEAN_BUILD", file.clean_build, false)?,
            clean_target,
            write_filetree: flag(
                "COOKBOOK_WRITE_FILETREE",
                file.write_filetree,
                clean_target || web,
            )?,
        })
    }
}

/// Parse a boolean environment variable. An empty value counts as unset-false
/// so that `VAR= make` behaves like not setting it.
fn parse_bool(key: &str, raw: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "" | "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(Error::Other(format!(
            "{key}={raw:?} is not a boolean, expected true/false or 1/0"
        ))),
    }
}

/// Per-recipe overrides stored in `cookbook.lock`.
#[derive(Debug, Default, Clone, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct RecipeLock {
    /// Build rule override: "source", "binary", "local" or "ignore".
    pub fsrule: Option<String>,
    /// Pinned git revision.
    pub gitrev: Option<String>,
}

impl RecipeLock {
    pub fn is_empty(&self) -> bool {
        self.fsrule.is_none() && self.gitrev.is_none()
    }
}

const LOCK_HEADER: &str = "# This file is generated automatically.\n\
# All configuration here overrides anything from recipes or config directory.\n";

/// The contents of `cookbook.lock`.
#[derive(Debug, Default, Clone, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct CookLock {
    pub recipes: BTreeMap<String, RecipeLock>,
}

impl CookLock {
    /// Load the lock file, or an empty lock when it does not exist.
    pub fn load() -> Result<Self> {
        Ok(read_optional_toml(Path::new(LOCK_PATH))?.unwrap_or_default())
    }

    pub fn save(&self) -> Result<()> {
        let body = toml::to_string(self)
            .map_err(|e| Error::Other(format!("Unable to serialize {LOCK_PATH}: {e}")))?;
        let path = Path::new(LOCK_PATH);
        fs::write(path, format!("{LOCK_HEADER}\n{body}"))
            .map_err(wrap_io_err!(path, "Writing lock"))
    }
}

/// URL prefix rewrites, keyed by scheme-less prefix.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Mirrors(HashMap<String, String>);

impl Mirrors {
    /// Rewrite `url` using the longest configured prefix that matches it on a
    /// path boundary. URLs matching no prefix are returned unchanged.
    pub fn translate(&self, url: &str) -> String {
        let (scheme, rest) = ["https://", "http://"]
            .iter()
            .find_map(|scheme| url.strip_prefix(scheme).map(|rest| (*scheme, rest)))
            .unwrap_or(("", url));

        let best = self
            .0
            .iter()
            .filter(|(prefix, _)| match rest.strip_prefix(prefix.as_str()) {
                Some(tail) => tail.is_empty() || tail.starts_with('/') || prefix.ends_with('/'),
                None => false,
            })
            .max_by_key(|(prefix, _)| prefix.len());

        match best {
            Some((prefix, mirror)) => format!("{scheme}{mirror}{}", &rest[prefix.len()..]),
            None => url.to_string(),
        }
    }
}

/// `cookbook.toml` as written by the user.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ConfigFile {
    cook: CookSettings,
    mirrors: HashMap<String, String>,
}

/// The complete, resolved configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct CookbookConfig {
    pub cook: CookConfig,
    pub mirrors: Mirrors,
    pub recipe_lock: BTreeMap<String, RecipeLock>,
}

impl CookbookConfig {
    /// Load `cookbook.toml` and `cookbook.lock` from the working directory.
    pub fn load() -> Result<Self> {
        let file: ConfigFile = read_optional_toml(Path::new(CONFIG_PATH))?.unwrap_or_default();
        let lock = CookLock::load()?;
        Self::resolve(file, lock, &|key| std::env::var(key).ok())
    }

    fn resolve(
        file: ConfigFile,
        lock: CookLock,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let mut mirrors = file.mirrors;
        if mirrors.is_empty() {
            mirrors.insert(DEFAULT_MIRROR.0.into(), DEFAULT_MIRROR.1.into());
        }
        Ok(CookbookConfig {
            cook: CookConfig::resolve(file.cook, env)?,
            mirrors: Mirrors(mirrors),
            recipe_lock: lock.recipes,
        })
    }
}

fn read_optional_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(wrap_io_err!(path, "Reading config")(e)),
    };
    toml::from_str(&content)
        .map(Some)
        .map_err(|e| Error::Other(format!("Unable to parse {}: {e}", path.display())))
}

static CONFIG: OnceLock<CookbookConfig> = OnceLock::new();

/// Load the configuration and make it available through [`get_config`].
/// Must be called once, before anything else, by every binary.
pub fn init_config() -> Result<()> {
    let config = CookbookConfig::load()?;
    CONFIG
        .set(config)
        .map_err(|_| Error::Other("Configuration is initialized twice".into()))
}

/// The global configuration.
///
/// # Panics
/// Panics if [`init_config`] has not been called.
pub fn get_config() -> &'static CookbookConfig {
    CONFIG.get().expect("Configuration is not initialized")
}

/// The global configuration, or `None` before [`init_config`] (unit tests).
pub fn try_get_config() -> Option<&'static CookbookConfig> {
    CONFIG.get()
}

/// Rewrite `url` through the configured mirrors.
pub fn translate_mirror(url: &str) -> String {
    match try_get_config() {
        Some(config) => config.mirrors.translate(url),
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn env_of(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |key| {
            vars.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    fn mirrors() -> Mirrors {
        Mirrors(HashMap::from([
            ("ftp.gnu.org/gnu".into(), "example.com/gnu".into()),
            ("github.com/foo/bar".into(), "github.com/baz/bar".into()),
            ("github.com/a".into(), "github.com/b".into()),
        ]))
    }

    #[test]
    fn parses_cook_table() {
        let file: ConfigFile = toml::from_str("[cook]\noffline = true\n").unwrap();
        assert_eq!(file.cook.offline, Some(true));
        assert_eq!(file.cook.jobs, None);
    }

    #[test]
    fn rejects_unknown_cook_key() {
        assert!(toml::from_str::<ConfigFile>("[cook]\nofline = true\n").is_err());
    }

    #[test]
    fn defaults() {
        let cook = CookConfig::resolve(CookSettings::default(), &no_env).unwrap();
        assert_eq!(
            cook,
            CookConfig {
                offline: false,
                jobs: std::thread::available_parallelism().map_or(1, usize::from),
                tui: true,
                logs: true,
                nonstop: false,
                compressed: false,
                git_treeless: false,
                verbose: true,
                verbose_cmd: false,
                clean_build: false,
                clean_target: false,
                write_filetree: false,
            }
        );
    }

    #[test]
    fn env_accepts_numeric_and_word_booleans() {
        let env = env_of(&[
            ("COOKBOOK_VERBOSE_CMD", "1"),
            ("COOKBOOK_VERBOSE", "false"),
            ("COOKBOOK_NONSTOP", "yes"),
            ("COOKBOOK_MAKE_JOBS", "3"),
        ]);
        let cook = CookConfig::resolve(CookSettings::default(), &env).unwrap();
        assert!(cook.verbose_cmd);
        assert!(!cook.verbose);
        assert!(cook.nonstop);
        assert_eq!(cook.jobs, 3);
    }

    #[test]
    fn env_rejects_garbage() {
        let env = env_of(&[("COOKBOOK_OFFLINE", "maybe")]);
        assert!(CookConfig::resolve(CookSettings::default(), &env).is_err());
        let env = env_of(&[("COOKBOOK_MAKE_JOBS", "0")]);
        assert!(CookConfig::resolve(CookSettings::default(), &env).is_err());
    }

    #[test]
    fn file_overrides_env() {
        let file = CookSettings {
            offline: Some(false),
            ..Default::default()
        };
        let env = env_of(&[("COOKBOOK_OFFLINE", "true")]);
        assert!(!CookConfig::resolve(file, &env).unwrap().offline);
    }

    #[test]
    fn ci_disables_tui_and_logs() {
        let env = env_of(&[("CI", "1")]);
        let cook = CookConfig::resolve(CookSettings::default(), &env).unwrap();
        assert!(!cook.tui);
        assert!(!cook.logs);
    }

    #[test]
    fn clean_target_and_web_imply_filetree() {
        for key in ["COOKBOOK_CLEAN_TARGET", "COOKBOOK_WEB"] {
            let env = move |k: &str| (k == key).then(|| "true".to_string());
            let cook = CookConfig::resolve(CookSettings::default(), &env).unwrap();
            assert!(cook.write_filetree, "{key} should imply write_filetree");
        }
    }

    #[test]
    fn default_mirror_is_added_only_when_none_configured() {
        let config =
            CookbookConfig::resolve(ConfigFile::default(), CookLock::default(), &no_env).unwrap();
        assert_eq!(
            config.mirrors.translate("https://ftp.gnu.org/gnu/bash.tar"),
            "https://mirrors.ocf.berkeley.edu/gnu/bash.tar"
        );
    }

    #[test]
    fn mirror_exact_match() {
        assert_eq!(mirrors().translate("ftp.gnu.org/gnu"), "example.com/gnu");
        assert_eq!(
            mirrors().translate("github.com/foo/bar"),
            "github.com/baz/bar"
        );
    }

    #[test]
    fn mirror_prefix_match_keeps_scheme() {
        assert_eq!(
            mirrors().translate("https://github.com/a/c"),
            "https://github.com/b/c"
        );
        assert_eq!(
            mirrors().translate("http://ftp.gnu.org/gnu/bash/bash-5.2.15.tar.gz"),
            "http://example.com/gnu/bash/bash-5.2.15.tar.gz"
        );
    }

    #[test]
    fn mirror_longest_prefix_wins() {
        assert_eq!(
            mirrors().translate("https://github.com/foo/bar/baz"),
            "https://github.com/baz/bar/baz"
        );
    }

    #[test]
    fn mirror_matches_only_on_path_boundary() {
        assert_eq!(
            mirrors().translate("https://github.com/apple/x"),
            "https://github.com/apple/x"
        );
    }

    #[test]
    fn mirror_no_match() {
        assert_eq!(
            mirrors().translate("www.rust-lang.org"),
            "www.rust-lang.org"
        );
        assert_eq!(
            mirrors().translate("http://github.com/unrelated/repo"),
            "http://github.com/unrelated/repo"
        );
    }
}
