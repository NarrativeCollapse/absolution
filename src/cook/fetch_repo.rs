//! Access to the remote repository of pre-built packages.

use std::{
    cell::RefCell,
    io::{PipeWriter, Write},
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, SystemTime},
};

use pkg::{
    PackageName, RepoManager, Repository,
    callback::{Callback, PlainCallback, SilentCallback},
    net_backend::{CurlBackend, DownloadBackend},
};

use crate::{Error, Result, config::get_config, cook::fs};

/// Where repository metadata, keys and downloads are kept.
const REMOTES_DIR: &str = "build/remotes";

/// How long a downloaded package index is trusted before refreshing it.
const INDEX_MAX_AGE: Duration = Duration::from_secs(8 * 3600);

// Fetching is single threaded and `RepoManager` is not `Send`, so the
// connection is kept per thread.
thread_local! {
    static BINARY_REPO: RefCell<Option<(RepoManager, Repository)>> = const { RefCell::new(None) };
}

/// The cached package index of `remote`, read and written at the same path.
fn index_cache_path(remote: &str) -> PathBuf {
    Path::new(REMOTES_DIR).join(format!("repo_{remote}_{}.toml", redoxer::target()))
}

/// Load the cached index, unless it is missing, unreadable or (when online)
/// older than [`INDEX_MAX_AGE`].
fn load_cached_index(path: &Path, offline: bool) -> Option<Repository> {
    if !offline {
        let age = SystemTime::now()
            .duration_since(fs::modified(path).ok()?)
            .unwrap_or_default();
        if age > INDEX_MAX_AGE {
            return None;
        }
    }
    Repository::from_toml(&std::fs::read_to_string(path).ok()?).ok()
}

fn download_index(repo: &RepoManager, cache_path: &Path) -> Result<Repository> {
    let (toml, _) = repo.get_package_toml(&PackageName::new("repo")?)?;
    let index = Repository::from_toml(&toml)?;
    fs::serialize_and_write(cache_path, &index)?;
    Ok(index)
}

fn init_binary_repo() -> Result<(RepoManager, Repository)> {
    let offline = get_config().cook.offline;
    let backend = CurlBackend::new().map_err(|e| {
        Error::Other(format!(
            "Unable to set up downloads, is curl installed? {e}"
        ))
    })?;
    let mut repo = RepoManager::new(
        Rc::new(RefCell::new(SilentCallback::new())),
        Box::new(backend),
    );
    let remote_source = crate::config::translate_mirror(crate::REMOTE_PKG_SOURCE);
    repo.add_remote(&remote_source, redoxer::target())?;
    repo.set_download_path(PathBuf::from(REMOTES_DIR));
    repo.sync_keys()?;

    let cache_path = index_cache_path(&repo.remotes[0]);
    let index = match load_cached_index(&cache_path, offline) {
        Some(index) => index,
        None if offline => {
            eprintln!("No cached package index, all recipes will build from source");
            Repository::default()
        }
        None => download_index(&repo, &cache_path).unwrap_or_else(|e| {
            eprintln!("Unable to load the package index, all recipes will build from source: {e}");
            Repository::default()
        }),
    };

    // Downloads report progress, unlike the setup above.
    repo.callback = Rc::new(RefCell::new(PlainCallback::new()));
    Ok((repo, index))
}

fn with_binary_repo<T>(f: impl FnOnce(&RepoManager, &Repository) -> T) -> Result<T> {
    BINARY_REPO.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(init_binary_repo()?);
        }
        let (repo, index) = slot.as_ref().expect("initialized above");
        Ok(f(repo, index))
    })
}

/// The remote repository connection and its package index.
pub fn get_binary_repo() -> Result<(RepoManager, Repository)> {
    with_binary_repo(|repo, index| (repo.clone(), index.clone()))
}

/// Whether the remote repository has a package called `name`.
pub fn has_binary_package(name: &str) -> Result<bool> {
    with_binary_repo(|_, index| index.packages.contains_key(name))
}

/// The file holding the public key remote packages are signed with.
pub fn get_binary_pubkey() -> Result<PathBuf> {
    with_binary_repo(|repo, _| {
        Path::new(REMOTES_DIR).join(format!("pub_key_{}.toml", repo.remotes[0]))
    })
}

/// Download progress written to a recipe's log instead of the terminal.
pub struct PlainPtyCallback {
    size: u64,
    unknown_size: bool,
    pos: u64,
    fetch_processed: usize,
    fetch_total: usize,
    download_file: Option<String>,
    log: PipeWriter,
}

impl PlainPtyCallback {
    pub fn new(log: PipeWriter) -> Self {
        Self {
            size: 0,
            unknown_size: false,
            pos: 0,
            fetch_processed: 0,
            fetch_total: 0,
            download_file: None,
            log,
        }
    }
}

const RESET_LINE: &str = "\r\x1b[2K";
const MEBIBYTE: f64 = 1_048_576.0;

impl Callback for PlainPtyCallback {
    fn fetch_start(&mut self, initial_count: usize) {
        self.fetch_total = 0;
        self.fetch_processed = 0;
        self.fetch_package_increment(0, initial_count);
    }

    fn fetch_package_name(&mut self, pkg_name: &PackageName) {
        // Continues the line started by fetch_package_increment.
        let _ = write!(&self.log, " {}", pkg_name.as_str());
    }

    fn fetch_package_increment(&mut self, added_processed: usize, added_count: usize) {
        self.fetch_processed += added_processed;
        self.fetch_total += added_count;
        let _ = write!(
            &self.log,
            "{RESET_LINE}Fetching: [{}/{}]",
            self.fetch_processed, self.fetch_total
        );
    }

    fn fetch_end(&mut self) {
        let outcome = if self.fetch_processed == self.fetch_total {
            "complete"
        } else {
            "incomplete"
        };
        let _ = writeln!(&self.log, "{RESET_LINE}Fetch {outcome}.");
    }

    fn download_start(&mut self, length: u64, file: &str) {
        self.size = length;
        self.unknown_size = length == 0;
        self.pos = 0;
        if !self.unknown_size {
            let _ = write!(&self.log, "{RESET_LINE}Downloading {file}");
            self.download_file = Some(file.to_string());
        }
    }

    fn download_increment(&mut self, downloaded: u64) {
        self.pos += downloaded;
        if self.unknown_size {
            self.size += downloaded;
            return;
        }
        let _ = write!(
            &self.log,
            "{RESET_LINE}Downloading {} [{:.2} MiB / {:.2} MiB]",
            self.download_file.as_deref().unwrap_or(""),
            self.pos as f64 / MEBIBYTE,
            self.size as f64 / MEBIBYTE,
        );
    }

    fn download_end(&mut self) {
        if !self.unknown_size {
            let _ = writeln!(&self.log);
            self.download_file = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_or_missing_index_is_not_loaded_online() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("repo.toml");
        assert!(load_cached_index(&path, false).is_none());
        assert!(load_cached_index(&path, true).is_none());

        fs::serialize_and_write(&path, &Repository::default()).unwrap();
        assert!(load_cached_index(&path, false).is_some());

        let old = SystemTime::now() - INDEX_MAX_AGE - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(load_cached_index(&path, false).is_none());
        // Offline, an old index is better than none.
        assert!(load_cached_index(&path, true).is_some());
    }
}
