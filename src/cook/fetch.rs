//! Obtaining recipe sources: git clones, tar archives, local paths and
//! pre-built packages from the remote repository.

use std::{
    cell::RefCell,
    collections::BTreeSet,
    fs::File,
    path::{Path, PathBuf},
    process::Command,
    rc::Rc,
};

use pkg::{SourceIdentifier, net_backend::DownloadBackendWriter};
use walkdir::WalkDir;

use crate::cook::{
    fetch_repo::{self, PlainPtyCallback},
    fs::*,
    git,
    package::{get_package_name, package_source_paths},
    pty::PtyOut,
    redoxer,
    script::SHARED_PRESCRIPT,
};
use crate::{
    Error, Result, bail_other_err,
    config::{get_config, translate_mirror},
    is_redox, log_to_pty,
    recipe::{BuildKind, CookRecipe, RecipeRule, SourceRecipe},
    wrap_io_err,
};

/// How many `same_as` hops are followed before assuming a cycle.
const MAX_SAME_AS_DEPTH: usize = 8;

/// Identifier recorded for sources that have no meaningful revision.
const LOCAL_SOURCE_IDENT: &str = "local_source";
/// Identifier recorded for tar sources whose recipe has no hash.
const UNHASHED_TAR_IDENT: &str = "no_tar_blake3_hash_info";

pub struct FetchResult {
    pub source_dir: PathBuf,
    /// The git commit or tar hash of the source
    pub source_ident: String,
    /// The combined hash of the patches and prepare script
    pub patches_ident: String,
    /// True when the source was already up to date
    pub cached: bool,
}

impl FetchResult {
    pub fn new(
        source_dir: PathBuf,
        source_ident: String,
        patches_ident: String,
        cached: bool,
    ) -> Self {
        Self {
            source_dir,
            source_ident,
            patches_ident,
            cached,
        }
    }

    pub fn cached(source_dir: PathBuf, source_ident: String, patches_ident: String) -> Self {
        Self::new(source_dir, source_ident, patches_ident, true)
    }

    /// Record the identifiers in the recipe's `source_info.toml`.
    fn record(self, recipe: &CookRecipe) -> Result<Self> {
        let ident = crate::cook::ident::get_ident();
        write_source_info(
            recipe,
            &SourceIdentifier {
                commit_identifier: ident.commit.clone(),
                time_identifier: ident.time.clone(),
                source_identifier: self.source_ident.clone(),
                patch_identifier: self.patches_ident.clone(),
            },
        )?;
        Ok(self)
    }
}

/// Fetch without network access: every source must already be on disk.
pub fn fetch_offline(recipe: &CookRecipe, logger: &PtyOut) -> Result<FetchResult> {
    fetch_offline_at_depth(recipe, logger, 0)
}

fn fetch_offline_at_depth(
    recipe: &CookRecipe,
    logger: &PtyOut,
    depth: usize,
) -> Result<FetchResult> {
    let recipe_dir = &recipe.dir;
    let source_dir = recipe_dir.join("source");
    match recipe.recipe.build.kind {
        // Meta packages have no source.
        BuildKind::None => {
            return FetchResult::cached(source_dir, String::new(), String::new()).record(recipe);
        }
        BuildKind::Remote => return fetch_remote(recipe_dir, recipe, true, source_dir, logger),
        _ => {}
    }

    let result = match &recipe.recipe.source {
        Some(SourceRecipe::Path { .. }) | None => return fetch(recipe, true, logger),
        Some(SourceRecipe::SameAs { same_as }) => {
            let canon = fetch_resolve_canon(same_as, recipe, depth)?;
            let result = fetch_offline_at_depth(&canon, logger, depth + 1)?;
            fetch_make_symlink(&source_dir, same_as)?;
            result
        }
        Some(SourceRecipe::Git {
            patches, script, ..
        }) => {
            offline_check_exists(&source_dir)?;
            require_checkout(&source_dir)?;
            let head = git::head(&source_dir)?;
            let patches_ident = patches_ident(recipe_dir, patches, script)?;
            FetchResult::cached(source_dir, head.rev, patches_ident)
        }
        Some(SourceRecipe::Tar {
            blake3,
            patches,
            script,
            ..
        }) => {
            let source_ident = blake3.clone().unwrap_or(UNHASHED_TAR_IDENT.into());
            let patches_ident = patches_ident(recipe_dir, patches, script)?;
            let cached = source_dir.is_dir();
            if !cached {
                let source_tar = recipe_dir.join("source.tar");
                offline_check_exists(&source_tar)?;
                let actual = get_file_blake3(&source_tar)?;
                match blake3 {
                    Some(expected) if *expected == actual => {}
                    Some(expected) => bail_other_err!(
                        "{:?} has blake3 {actual:?}, but the recipe expects {expected:?}",
                        source_tar.display()
                    ),
                    // An archive that cannot be verified is not unpacked.
                    None => bail_other_err!(
                        "Please add blake3 = {actual:?} to {:?}",
                        recipe_dir.join("recipe.toml").display()
                    ),
                }
                extract_tar_source(
                    recipe_dir,
                    &source_tar,
                    &source_dir,
                    patches,
                    script,
                    logger,
                )?;
            }
            FetchResult::new(source_dir, source_ident, patches_ident, cached)
        }
    };

    result.record(recipe)
}

/// Fetch the source of `recipe`, updating it when `check_source` is set.
pub fn fetch(recipe: &CookRecipe, check_source: bool, logger: &PtyOut) -> Result<FetchResult> {
    fetch_at_depth(recipe, check_source, logger, 0)
}

fn fetch_at_depth(
    recipe: &CookRecipe,
    check_source: bool,
    logger: &PtyOut,
    depth: usize,
) -> Result<FetchResult> {
    let recipe_dir = &recipe.dir;
    let source_dir = recipe_dir.join("source");

    // A "local" recipe keeps whatever source is there. If there is none yet,
    // fetch it once the normal way.
    let fetched_once;
    let recipe = if recipe.rule == Some(RecipeRule::Local) && !source_dir.exists() {
        let mut as_source = recipe.clone();
        as_source.rule = Some(RecipeRule::Source);
        as_source.reload_recipe()?;
        fetched_once = as_source;
        &fetched_once
    } else {
        recipe
    };

    match recipe.recipe.build.kind {
        BuildKind::None => {
            return FetchResult::cached(source_dir, String::new(), String::new()).record(recipe);
        }
        BuildKind::Remote => return fetch_remote(recipe_dir, recipe, false, source_dir, logger),
        _ => {}
    }

    let previous = fetch_get_source_info(recipe).ok();
    let result = match &recipe.recipe.source {
        Some(SourceRecipe::SameAs { same_as }) => {
            let canon = fetch_resolve_canon(same_as, recipe, depth)?;
            let result = fetch_at_depth(&canon, check_source, logger, depth + 1)?;
            fetch_make_symlink(&source_dir, same_as)?;
            result
        }
        Some(SourceRecipe::Path { path }) => fetch_path(Path::new(path), source_dir, logger)?,
        Some(SourceRecipe::Git {
            git,
            upstream: _,
            branch,
            rev,
            patches,
            script,
            shallow_clone,
        }) => GitFetch {
            recipe_dir,
            source_dir,
            url: translate_mirror(git),
            branch: branch.as_deref(),
            rev: rev.as_deref(),
            treeless: shallow_clone.unwrap_or_else(|| get_config().cook.git_treeless),
            patches,
            script,
            logger,
        }
        .run(previous, check_source)?,
        Some(SourceRecipe::Tar {
            tar,
            blake3,
            patches,
            script,
        }) => fetch_tar(
            recipe_dir,
            source_dir,
            tar,
            blake3.as_deref(),
            patches,
            script,
            previous,
            check_source,
            logger,
        )?,
        None => {
            if !source_dir.is_dir() {
                log_to_pty!(
                    logger,
                    "WARNING: Recipe without source section expected source dir at '{}'",
                    source_dir.display(),
                );
                create_dir(&source_dir)?;
            }
            FetchResult::cached(source_dir, LOCAL_SOURCE_IDENT.into(), String::new())
        }
    };

    if let BuildKind::Cargo { cargopath, .. } = &recipe.recipe.build.kind
        && !result.cached
    {
        fetch_cargo(&result.source_dir, cargopath.as_deref(), logger)?;
    }

    result.record(recipe)
}

fn fetch_path(path: &Path, source_dir: PathBuf, logger: &PtyOut) -> Result<FetchResult> {
    let cached = source_dir.is_dir() && modified_dir(path)? <= modified_dir(&source_dir)?;
    if !cached {
        log_to_pty!(
            logger,
            "DEBUG: {:?} is newer than {:?}",
            path.display(),
            source_dir.display()
        );
        copy_dir_all(path, &source_dir).map_err(wrap_io_err!(
            path,
            source_dir,
            "Copying source"
        ))?;
    }
    Ok(FetchResult::new(
        source_dir,
        LOCAL_SOURCE_IDENT.into(),
        String::new(),
        cached,
    ))
}

fn require_checkout(source_dir: &Path) -> Result<()> {
    if !git::is_checkout(source_dir) {
        bail_other_err!(
            "{:?} is not a git repository, but the recipe has a git source",
            source_dir.display()
        );
    }
    Ok(())
}

/// One git source and how the recipe wants it checked out.
struct GitFetch<'a> {
    recipe_dir: &'a Path,
    source_dir: PathBuf,
    /// The repository URL, after mirror translation
    url: String,
    branch: Option<&'a str>,
    rev: Option<&'a str>,
    treeless: bool,
    patches: &'a [String],
    script: &'a Option<String>,
    logger: &'a PtyOut<'a>,
}

impl GitFetch<'_> {
    fn run(self, previous: Option<SourceIdentifier>, check_source: bool) -> Result<FetchResult> {
        let patches_ident = patches_ident(self.recipe_dir, self.patches, self.script)?;

        // `fetched` tracks whether origin is already up to date in this run.
        let (cached, fetched) = if !self.source_dir.is_dir() {
            self.clone_repository()?;
            (false, true)
        } else if !check_source {
            (true, false)
        } else {
            require_checkout(&self.source_dir)?;
            self.is_current(previous.as_ref(), &patches_ident)?
        };

        if !cached {
            if !fetched {
                git::fetch_origin(&self.source_dir, &self.url, self.logger)?;
            }
            self.check_out()?;
            fetch_apply_patches(
                self.recipe_dir,
                self.patches,
                self.script,
                &self.source_dir,
                self.logger,
            )?;
        }

        let head = git::head(&self.source_dir)?;
        Ok(FetchResult::new(
            self.source_dir,
            head.rev,
            patches_ident,
            cached,
        ))
    }

    fn git(&self) -> Command {
        let mut command = Command::new("git");
        command.arg("-C").arg(&self.source_dir);
        command
    }

    /// Clone into `source.tmp` and move it into place once complete.
    fn clone_repository(&self) -> Result<()> {
        let source_dir_tmp = self.recipe_dir.join("source.tmp");
        create_dir_clean(&source_dir_tmp)?;

        let mut command = Command::new("git");
        command.arg("clone").arg("--recursive").arg(&self.url);
        if let Some(branch) = self.branch {
            command.arg("--branch").arg(branch);
        }
        if self.treeless {
            command
                .arg("--filter=tree:0")
                .arg("--also-filter-submodules");
        }
        command.arg(&source_dir_tmp);
        if let Err(e) = run_command(command, self.logger) {
            if !is_redox() {
                return Err(e);
            }
            // RedoxFS has a race when `--recursive` clones run on several
            // CPUs; initializing submodules one repository at a time avoids it.
            self.submodules_one_by_one(&source_dir_tmp, &self.submodule_update_args())?;
        }

        rename(&source_dir_tmp, &self.source_dir)
    }

    /// Whether the checkout already matches what the recipe asks for.
    /// Returns `(current, fetched)`.
    fn is_current(
        &self,
        previous: Option<&SourceIdentifier>,
        patches_ident: &str,
    ) -> Result<(bool, bool)> {
        let dir = &self.source_dir;
        let head = git::head(dir)?;
        if !previous.is_some_and(|info| info.is_updated(&head.rev, patches_ident)) {
            return Ok((false, false));
        }

        match (self.rev, &head.branch) {
            // Pinned: current when HEAD is detached at the pinned revision.
            (Some(rev), None) => {
                let current = git::resolve_commit(dir, rev).is_ok_and(|commit| commit == head.rev);
                Ok((current, false))
            }
            // Tracking a branch: current when it is the wanted branch of the
            // wanted remote and nothing new has been pushed to it.
            (None, Some(local_branch)) => {
                let wanted_branch = match self.branch {
                    Some(branch) => Ok(branch.to_string()),
                    None => git::remote_default_branch(dir, "origin"),
                };
                let tracking = git::tracking(dir, local_branch)?;
                let (Ok(wanted_branch), Some(tracking)) = (wanted_branch, tracking) else {
                    log_to_pty!(
                        self.logger,
                        "DEBUG: {local_branch:?} is not tracking a remote"
                    );
                    return Ok((false, false));
                };
                if tracking.remote != "origin"
                    || tracking.url != git::normalize_url(&self.url)
                    || tracking.branch != wanted_branch
                    || *local_branch != wanted_branch
                {
                    return Ok((false, false));
                }

                git::fetch_origin(dir, &self.url, self.logger)?;
                let current = match git::remote_branch_rev(dir, "origin", &wanted_branch) {
                    Ok(remote_rev) => remote_rev == head.rev,
                    Err(e) => {
                        log_to_pty!(self.logger, "{e}");
                        false
                    }
                };
                Ok((current, true))
            }
            // The recipe switched between pinned and tracking.
            _ => Ok((false, false)),
        }
    }

    /// Check out the wanted revision and bring submodules in line with it.
    fn check_out(&self) -> Result<()> {
        if !self.patches.is_empty() || self.script.is_some() {
            // Undo previously applied patches before moving HEAD.
            let mut command = self.git();
            command.args(["reset", "--hard", "--recurse-submodules"]);
            run_command(command, self.logger)?;
        }

        let mut command = self.git();
        match self.rev {
            Some(rev) => {
                command.arg("checkout").arg(rev);
            }
            None => {
                let branch = match self.branch {
                    Some(branch) => branch.to_string(),
                    None => git::remote_default_branch(&self.source_dir, "origin")?,
                };
                command
                    .args(["checkout", "-B", &branch])
                    .arg(format!("remotes/origin/{branch}"));
            }
        }
        run_command(command, self.logger)?;

        let mut command = self.git();
        command.args(["submodule", "sync", "--recursive"]);
        if let Err(e) = run_command(command, self.logger) {
            if !is_redox() {
                return Err(e);
            }
            self.submodules_one_by_one(&self.source_dir, &["sync"])?;
        }

        let update_args = self.submodule_update_args();
        let mut command = self.git();
        command
            .arg("submodule")
            .args(&update_args)
            .arg("--recursive");
        if let Err(e) = run_command(command, self.logger) {
            if !is_redox() {
                return Err(e);
            }
            self.submodules_one_by_one(&self.source_dir, &update_args)?;
        }
        Ok(())
    }

    fn submodule_update_args(&self) -> Vec<&'static str> {
        let mut args = vec!["update", "--init"];
        if self.treeless {
            args.push("--filter=tree:0");
        }
        args
    }

    /// Run `git submodule <args>` in `root` and then in every repository that
    /// appears below it, until no new repository shows up.
    fn submodules_one_by_one(&self, root: &Path, args: &[&str]) -> Result<()> {
        log_to_pty!(
            self.logger,
            "Git submodule {} failed, possibly due to a race condition in RedoxFS; \
             retrying without --recursive.",
            args[0]
        );

        let mut done: BTreeSet<PathBuf> = BTreeSet::new();
        loop {
            let pending: Vec<PathBuf> = WalkDir::new(root)
                .into_iter()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_name() == ".git")
                .filter_map(|entry| entry.path().parent().map(Path::to_path_buf))
                .filter(|repo| !done.contains(repo))
                .collect();
            if pending.is_empty() {
                return Ok(());
            }
            for repo in pending {
                log_to_pty!(self.logger, "==> Processing: {:?}", repo.display());
                let mut command = Command::new("git");
                command.arg("-C").arg(&repo).arg("submodule").args(args);
                run_command(command, self.logger)?;
                done.insert(repo);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn fetch_tar(
    recipe_dir: &Path,
    source_dir: PathBuf,
    url: &str,
    blake3: Option<&str>,
    patches: &[String],
    script: &Option<String>,
    previous: Option<SourceIdentifier>,
    check_source: bool,
    logger: &PtyOut,
) -> Result<FetchResult> {
    let source_tar = recipe_dir.join("source.tar");
    let source_ident = blake3.unwrap_or(UNHASHED_TAR_IDENT).to_string();
    let patches_ident = patches_ident(recipe_dir, patches, script)?;

    // Make sure the archive on disk is the one the recipe names. A stale
    // archive is downloaded again once; a fresh download that does not match
    // the recipe is an error.
    let mut downloaded = false;
    loop {
        if !source_tar.is_file() {
            download_wget(url, &source_tar, logger)?;
            downloaded = true;
        }
        if !check_source {
            break;
        }
        let actual = get_file_blake3(&source_tar)?;
        match blake3 {
            Some(expected) if expected == actual => break,
            Some(expected) if downloaded => bail_other_err!(
                "The downloaded tar has blake3 {actual:?}, but the recipe expects {expected:?}"
            ),
            Some(_) => {
                log_to_pty!(
                    logger,
                    "DEBUG: source tar blake3 differs, downloading again"
                );
                remove_all(&source_tar)?;
            }
            None => {
                log_to_pty!(
                    logger,
                    "WARNING: {:?} is not verified, add blake3 = {actual:?} to the recipe",
                    source_tar.display(),
                );
                break;
            }
        }
    }

    if source_dir.is_dir() {
        let stale_reason = if downloaded {
            Some("tar is newer")
        } else {
            match &previous {
                None => Some("cached info is missing"),
                Some(info) if info.source_identifier != source_ident => {
                    Some("tar is newer than cached info")
                }
                Some(info) if info.patch_identifier != patches_ident => {
                    Some("patches are newer than cached info")
                }
                Some(_) => None,
            }
        };
        if let Some(reason) = stale_reason {
            log_to_pty!(logger, "DEBUG: updating source: {reason}");
            remove_all(&source_dir)?;
        }
    }

    let cached = source_dir.is_dir();
    if !cached {
        extract_tar_source(
            recipe_dir,
            &source_tar,
            &source_dir,
            patches,
            script,
            logger,
        )?;
    }
    Ok(FetchResult::new(
        source_dir,
        source_ident,
        patches_ident,
        cached,
    ))
}

/// Unpack and patch an archive in `source.tmp`, then move it into place, so
/// that an interrupted run never leaves a half-prepared source directory.
fn extract_tar_source(
    recipe_dir: &Path,
    source_tar: &Path,
    source_dir: &Path,
    patches: &[String],
    script: &Option<String>,
    logger: &PtyOut,
) -> Result<()> {
    let source_dir_tmp = recipe_dir.join("source.tmp");
    create_dir_clean(&source_dir_tmp)?;
    fetch_extract_tar(source_tar, &source_dir_tmp, logger)?;
    fetch_apply_patches(recipe_dir, patches, script, &source_dir_tmp, logger)?;
    rename(&source_dir_tmp, source_dir)
}

fn patches_ident(dir: &Path, patches: &[String], script: &Option<String>) -> Result<String> {
    get_combined_blake3(dir, patches, script.as_slice())
}

/// Link `source_dir` to the source of the recipe at `same_as`.
pub(crate) fn fetch_make_symlink(source_dir: &Path, same_as: &str) -> Result<()> {
    if source_dir.is_symlink() {
        return Ok(());
    }
    if source_dir.is_dir() {
        bail_other_err!(
            "{dir:?} is a directory, but the recipe makes it a symlink.\n\
             Remove {dir:?} if you have not made any changes in it",
            dir = source_dir.display(),
        )
    }
    symlink(Path::new(same_as).join("source"), source_dir)
}

/// Load the recipe a `same_as` source points to, with the rule and pinned
/// revision that apply to it.
pub(crate) fn fetch_resolve_canon(
    same_as: &str,
    recipe: &CookRecipe,
    depth: usize,
) -> Result<CookRecipe> {
    if depth >= MAX_SAME_AS_DEPTH {
        bail_other_err!(
            "The same_as chain starting at {:?} is too long; it is probably a cycle",
            recipe.dir.display()
        );
    }
    let canon_dir = recipe.dir.join(same_as);
    if !canon_dir.exists() {
        bail_other_err!("{:?} does not exist", canon_dir.display());
    }
    let mut canon = CookRecipe::from_path(&canon_dir, true, recipe.name.is_host())?;
    if let Some(rule) = recipe.rule {
        canon.apply_rule(rule);
    }
    let pinned = get_config()
        .recipe_lock
        .get(canon.name.without_prefix())
        .and_then(|lock| lock.gitrev.clone());
    if let Some(gitrev) = pinned
        && let Some(SourceRecipe::Git { rev, branch, .. }) = &mut canon.recipe.source
    {
        *rev = Some(gitrev);
        *branch = None;
    }
    Ok(canon)
}

/// Unpack an archive into `dest`, dropping its top-level directory.
pub(crate) fn fetch_extract_tar(source_tar: &Path, dest: &Path, logger: &PtyOut) -> Result<()> {
    let verbose = get_config().cook.verbose;
    let mut command = Command::new("tar");
    if is_redox() {
        command.arg(if verbose { "xvf" } else { "xf" });
    } else {
        command.arg("--extract").arg("--no-same-owner");
        if verbose {
            command.arg("--verbose");
        }
        command.arg("--file");
    }
    command.arg(source_tar);
    command.arg("--directory").arg(dest);
    command.arg("--strip-components").arg("1");
    run_command(command, logger)
}

/// Download the crates a cargo project depends on, so that the build itself
/// can run offline. Skipped when the cross toolchain has no cargo yet.
pub(crate) fn fetch_cargo(
    source_dir: &Path,
    cargopath: Option<&str>,
    logger: &PtyOut,
) -> Result<()> {
    if !redoxer::toolchain_has_cargo() {
        return Ok(());
    }
    let manifest_dir = match cargopath {
        Some(cargopath) => source_dir.join(cargopath),
        None => source_dir.to_path_buf(),
    };

    // Before the cookbook's own wrapper is built, plain cargo can fetch too.
    let program = redoxer::cookbook_redoxer().unwrap_or_else(|_| PathBuf::from("cargo"));
    let mut command = Command::new(program);
    command.arg("fetch").arg("--manifest-path");
    command.arg(manifest_dir.join("Cargo.toml"));
    run_command(command, logger)
}

/// Download the pre-built packages of `recipe` from the remote repository.
pub fn fetch_remote(
    recipe_dir: &Path,
    recipe: &CookRecipe,
    offline_mode: bool,
    source_dir: PathBuf,
    logger: &PtyOut,
) -> Result<FetchResult> {
    let (mut manager, repository) = fetch_repo::get_binary_repo()?;
    let target_dir = create_target_dir(recipe_dir, recipe.target)?;
    if let Some((_, log)) = logger {
        let writer = log.try_clone().map_err(wrap_io_err!("Cloning log pipe"))?;
        manager.set_callback(Rc::new(RefCell::new(PlainPtyCallback::new(writer))));
    }

    let recipe_name = recipe_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::Other(format!("{:?} has no recipe name", recipe_dir.display())))?;

    let download_to = |manager: &mut pkg::RepoManager,
                       file: &str,
                       size: Option<u64>,
                       dest: &Path|
     -> Result<()> {
        if dest.is_file() {
            remove_all(dest)?;
        }
        let out = File::create(dest).map_err(wrap_io_err!(dest, "Creating file"))?;
        let mut writer = DownloadBackendWriter::ToFile(out);
        manager.download(file, size, None, &mut writer)?;
        Ok(())
    };

    let mut result = None;
    let mut cached = true;
    for package in recipe.recipe.get_packages_list() {
        let (_, source_pkgar, source_toml) = package_source_paths(package, &target_dir);
        let source_name = get_package_name(recipe_name, package);
        let Some(repo_blake3) = repository.packages.get(&source_name) else {
            bail_other_err!("Package {source_name} does not exist in the remote repository")
        };

        if offline_mode {
            offline_check_exists(&source_pkgar)?;
            offline_check_exists(&source_toml)?;
        } else {
            if source_toml.is_file() && &read_package_toml(&source_toml)?.blake3 != repo_blake3 {
                log_to_pty!(logger, "DEBUG: Updating source binaries");
                remove_all(&source_toml)?;
                if source_pkgar.is_file() {
                    remove_all(&source_pkgar)?;
                }
            }

            if !source_toml.is_file() {
                // The metadata is moved into place last: its presence marks a
                // complete download.
                let tmp_toml = source_toml.with_added_extension("tmp");
                let tmp_pkgar = source_pkgar.with_added_extension("tmp");
                download_to(
                    &mut manager,
                    &format!("{source_name}.toml"),
                    None,
                    &tmp_toml,
                )?;
                let size = read_package_toml(&tmp_toml)?.network_size;
                download_to(
                    &mut manager,
                    &format!("{source_name}.pkgar"),
                    Some(size),
                    &tmp_pkgar,
                )?;
                rename(&tmp_pkgar, &source_pkgar)?;
                rename(&tmp_toml, &source_toml)?;
                cached = false;
            }
        }

        // The mandatory package comes last and describes the whole recipe.
        if package.is_none() {
            let package_toml = read_package_toml(&source_toml)?;
            write_source_info(recipe, &package_toml.to_ident())?;
            result = Some(FetchResult::new(
                source_dir.clone(),
                package_toml.source_identifier,
                package_toml.patch_identifier,
                cached,
            ));
        }
    }

    result.ok_or_else(|| Error::Other("The recipe has no mandatory package".into()))
}

fn read_package_toml(path: &Path) -> Result<pkg::Package> {
    Ok(pkg::Package::from_toml(&read_to_string(path)?)?)
}

/// Apply the recipe's patches, then run its prepare script, in `source_dir`.
pub(crate) fn fetch_apply_patches(
    recipe_dir: &Path,
    patches: &[String],
    script: &Option<String>,
    source_dir: &Path,
    logger: &PtyOut,
) -> Result<()> {
    for patch_name in patches {
        let patch_file = recipe_dir.join(patch_name);
        if !patch_file.is_file() {
            bail_other_err!("Failed to find patch file {:?}", patch_file.display());
        }
        let patch =
            std::fs::read(&patch_file).map_err(wrap_io_err!(patch_file, "Reading patch"))?;

        let mut command = Command::new("patch");
        command.arg("--directory").arg(source_dir);
        command.arg("--strip=1");
        run_command_stdin(command, &patch, logger)?;
    }
    if let Some(script) = script {
        let mut command = Command::new("bash");
        command.arg("-ex");
        command.current_dir(source_dir);
        run_command_stdin(
            command,
            format!("{SHARED_PRESCRIPT}\n{script}").as_bytes(),
            logger,
        )?;
    }
    Ok(())
}

fn source_info_path(recipe: &CookRecipe) -> PathBuf {
    recipe.target_dir().join("source_info.toml")
}

fn write_source_info(recipe: &CookRecipe, info: &SourceIdentifier) -> Result<()> {
    create_target_dir(&recipe.dir, recipe.target)?;
    serialize_and_write(&source_info_path(recipe), info)
}

/// The identifiers recorded by the last fetch of `recipe`.
pub fn fetch_get_source_info(recipe: &CookRecipe) -> Result<SourceIdentifier> {
    read_toml(&source_info_path(recipe))
}
