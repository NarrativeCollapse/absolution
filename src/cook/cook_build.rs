//! Building a recipe: preparing its sysroot, running the build script and
//! working out its runtime dependencies.

use std::{
    collections::{BTreeSet, VecDeque},
    path::{Path, PathBuf},
    process::Command,
};

use pkg::{Package, PackageName};
use pkgar::{PackageFile, Transaction};
use pkgar_core::PackageSrc;
use pkgar_keys::PublicKeyFile;

use crate::config::CookConfig;
use crate::cook::{
    PUBLIC_KEY_PATH, fetch, fetch_repo, fs,
    package::{package_source_paths, package_target},
    pty::PtyOut,
    redoxer,
    script::{BUILD_POSTSCRIPT, BUILD_PRESCRIPT, SHARED_PRESCRIPT},
};
use crate::recipe::{
    AutoDeps, BuildCompiler, BuildKind, CookRecipe, OptionalPackageRecipe, Recipe,
};
use crate::{Error, Result, log_to_pty, wrap_io_err};

/// Libraries every system has; never reported as missing or as dependencies.
const PREINSTALLED_LIBRARIES: [&str; 3] = ["libc.so.6", "libgcc_s.so.1", "libstdc++.so.6"];

/// Directories of a stage that can hold dynamically linked files.
const BINARY_DIRS: [&str; 4] = ["usr/bin", "usr/games", "usr/lib", "usr/libexec"];

/// A dependency's package name and the archive it is installed from.
type DepArchives = BTreeSet<(PackageName, PathBuf)>;

pub struct BuildResult {
    /// One stage dir per optional package, then the main stage dir
    pub stage_dirs: Vec<PathBuf>,
    /// Runtime dependencies detected from the build output
    pub auto_deps: BTreeSet<PackageName>,
    /// True when nothing had to be rebuilt
    pub cached: bool,
}

impl BuildResult {
    pub fn new(stage_dirs: Vec<PathBuf>, auto_deps: BTreeSet<PackageName>) -> Self {
        BuildResult {
            stage_dirs,
            auto_deps,
            cached: false,
        }
    }

    pub fn cached(stage_dirs: Vec<PathBuf>, auto_deps: BTreeSet<PackageName>) -> Self {
        BuildResult {
            stage_dirs,
            auto_deps,
            cached: true,
        }
    }
}

/// Every regular file below the binary directories of the stage dirs, paired
/// with the stage dir it belongs to. Symlinks are not followed.
fn staged_binaries(stage_dirs: &[PathBuf]) -> BTreeSet<(&PathBuf, PathBuf)> {
    let mut files = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut pending: VecDeque<(&PathBuf, PathBuf)> = stage_dirs
        .iter()
        .flat_map(|stage| BINARY_DIRS.iter().map(move |dir| (stage, stage.join(dir))))
        .collect();

    while let Some((stage, dir)) = pending.pop_front() {
        if !visited.insert(dir.clone()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_file() {
                files.insert((stage, entry.path()));
            } else if file_type.is_dir() {
                pending.push_back((stage, entry.path()));
            }
        }
    }
    files
}

/// The shared libraries (`DT_NEEDED`) required by the ELF files in the
/// stage dirs.
fn needed_libraries(stage_dirs: &[PathBuf], verbose: bool, logger: &PtyOut) -> BTreeSet<String> {
    let mut needed = BTreeSet::new();
    for (stage, path) in staged_binaries(stage_dirs) {
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let cache = object::ReadCache::new(file);
        let Ok(elf) = object::build::elf::Builder::read(&cache) else {
            continue;
        };
        let Some(dynamic) = elf.dynamic_data() else {
            continue;
        };
        let shown_path = path.strip_prefix(stage).unwrap_or(&path).display();
        for entry in dynamic {
            let object::build::elf::Dynamic::String { tag, val } = entry else {
                continue;
            };
            let Ok(value) = str::from_utf8(val) else {
                continue;
            };
            match *tag {
                object::elf::DT_NEEDED => {
                    if verbose {
                        log_to_pty!(logger, "DEBUG: {shown_path} needs {value}");
                    }
                    needed.insert(value.to_string());
                }
                object::elf::DT_RPATH | object::elf::DT_RUNPATH if verbose => {
                    log_to_pty!(logger, "DEBUG: {shown_path} uses runpath {value}");
                }
                _ => {}
            }
        }
    }
    needed
}

/// The dependencies that provide a shared library the stage dirs link to.
fn auto_deps_from_dynamic_linking(
    stage_dirs: &[PathBuf],
    dep_pkgars: &DepArchives,
    logger: &PtyOut,
) -> BTreeSet<PackageName> {
    let verbose = crate::config::try_get_config().is_none_or(|config| config.cook.verbose);
    let needed = needed_libraries(stage_dirs, verbose, logger);
    let mut missing = needed.clone();
    for preinstalled in PREINSTALLED_LIBRARIES {
        missing.remove(preinstalled);
    }

    let mut deps = BTreeSet::new();
    if let Ok(key_file) = PublicKeyFile::open(PUBLIC_KEY_PATH) {
        for (dep, archive_path) in dep_pkgars {
            let Ok(mut package) = PackageFile::new(archive_path, &key_file.pkey) else {
                continue;
            };
            let Ok(entries) = package.read_entries() else {
                continue;
            };
            for entry in entries {
                let Ok(entry_path) = pkgar::ext::EntryExt::check_path(&entry) else {
                    continue;
                };
                let library = ["lib", "usr/lib"]
                    .iter()
                    .find_map(|prefix| entry_path.strip_prefix(prefix).ok())
                    .and_then(|path| path.to_str());
                let Some(library) = library else {
                    continue;
                };
                if needed.contains(library) {
                    if verbose {
                        log_to_pty!(logger, "DEBUG: {dep} provides {library}");
                    }
                    deps.insert(dep.with_prefix(pkg::PackagePrefix::Any));
                    missing.remove(library);
                }
            }
        }
    }

    if verbose {
        for library in missing {
            log_to_pty!(logger, "INFO: missing {library}");
        }
    }
    deps
}

/// Build `cook_recipe`, or reuse its previous build when nothing changed.
pub fn build(
    recipe_dir: &Path,
    source_dir: &Path,
    target_dir: &Path,
    cook_recipe: &CookRecipe,
    cook_config: &CookConfig,
    logger: &PtyOut,
) -> Result<BuildResult> {
    let recipe = &cook_recipe.recipe;
    let name = &cook_recipe.name;
    let sysroot_dir = get_sub_target_dir(target_dir, "sysroot");
    let toolchain_dir = get_sub_target_dir(target_dir, "toolchain");
    let auto_deps_file = get_sub_target_dir(target_dir, "auto_deps.toml");
    let build_dir = get_sub_target_dir(target_dir, "build");
    let stage_dirs = get_stage_dirs(&recipe.optional_packages, target_dir);
    let stage_pkgars: Vec<PathBuf> = stage_dirs
        .iter()
        .map(|dir| dir.with_added_extension("pkgar"))
        .collect();

    if recipe.build.kind == BuildKind::None {
        // Meta packages have nothing to build.
        return Ok(BuildResult::new(stage_dirs, BTreeSet::new()));
    }

    let mut dep_pkgars = DepArchives::new();
    let mut dep_host_pkgars = DepArchives::new();
    let direct_deps = [
        &recipe.build.dependencies[..],
        &recipe.build.dev_dependencies[..],
    ]
    .concat();
    for dependency in CookRecipe::get_build_deps_recursive(&direct_deps, false)? {
        let (_, pkgar, _) = dependency.stage_paths();
        if dependency.name.is_host() {
            dep_host_pkgars.insert((dependency.name, pkgar));
        } else {
            dep_pkgars.insert((dependency.name, pkgar));
        }
    }
    let auto_deps = |cached: bool| {
        build_auto_deps(
            recipe,
            &auto_deps_file,
            &stage_dirs,
            cached,
            cook_config,
            &dep_pkgars,
            logger,
        )
    };

    // A dependency that is already built is not checked for source changes.
    if cook_recipe.is_deps
        && auto_deps_file.is_file()
        && stage_pkgars.iter().all(|pkgar| pkgar.is_file())
    {
        log_to_pty!(logger, "DEBUG: using cached build, not checking source");
        return Ok(BuildResult::cached(stage_dirs.clone(), auto_deps(true)?));
    }

    if recipe.build.kind == BuildKind::Remote {
        return build_remote(stage_dirs, stage_pkgars, recipe, target_dir, logger);
    }

    // Host recipes build against host packages; target recipes build against
    // target packages and use host packages as their toolchain.
    let sysroot_deps = if name.is_host() {
        &dep_host_pkgars
    } else {
        &dep_pkgars
    };
    let toolchain_deps =
        (!name.is_host() && !dep_host_pkgars.is_empty()).then_some(&dep_host_pkgars);
    let keep_tag = cook_config.clean_target;
    let sysroot_cached = build_deps_dir(logger, &sysroot_dir, sysroot_deps, false, keep_tag)?;
    let toolchain_cached = match toolchain_deps {
        Some(deps) => build_deps_dir(logger, &toolchain_dir, deps, false, keep_tag)?,
        None => true,
    };

    let up_to_date = sysroot_cached
        && toolchain_cached
        && !build_is_source_newer(
            logger,
            recipe_dir,
            source_dir,
            &auto_deps_file,
            &stage_pkgars,
        );
    if up_to_date {
        log_to_pty!(logger, "DEBUG: using cached build");
        return Ok(BuildResult::cached(stage_dirs.clone(), auto_deps(true)?));
    }

    if auto_deps_file.is_file() {
        fs::remove_all(&auto_deps_file)?;
    }
    for stage_dir in &stage_dirs {
        remove_stage_dir(stage_dir)?;
    }
    if cook_config.clean_target {
        // A previous clean emptied these directories and left only their
        // tags behind; they have to be repopulated before building.
        if sysroot_cached {
            build_deps_dir(logger, &sysroot_dir, sysroot_deps, true, true)?;
        }
        if let Some(deps) = toolchain_deps
            && toolchain_cached
        {
            build_deps_dir(logger, &toolchain_dir, deps, true, true)?;
        }
    }

    let stage_dir = stage_dirs
        .last()
        .expect("the main stage dir is always present");
    if !stage_dir.is_dir() {
        let stage_dir_tmp = target_dir.join("stage.tmp");
        fs::create_dir_clean(&stage_dir_tmp)?;
        if cook_config.clean_build || !build_dir.is_dir() {
            fs::create_dir_clean(&build_dir)?;
        }

        let command = build_command(
            cook_recipe,
            cook_config,
            BuildDirs {
                build: &build_dir,
                recipe: recipe_dir,
                stage: &stage_dir_tmp,
                source: source_dir,
                sysroot: &sysroot_dir,
                toolchain: &toolchain_dir,
            },
        )?;
        let script = format!(
            "{BUILD_PRESCRIPT}\n{SHARED_PRESCRIPT}\n{}\n{BUILD_POSTSCRIPT}",
            build_script(&recipe.build.kind)
        );
        fs::run_command_stdin(command, script.as_bytes(), logger)?;

        split_optional_packages(recipe, &stage_dir_tmp, &stage_dirs)?;
        // Only a complete stage ever exists under its final name.
        fs::rename(&stage_dir_tmp, stage_dir)?;
    }

    if cook_config.clean_target {
        fs::remove_all(&build_dir)?;
        clean_deps_dir(&sysroot_dir)?;
        if toolchain_dir.is_dir() {
            clean_deps_dir(&toolchain_dir)?;
        }
        // The stage dirs are removed after packaging.
    }

    Ok(BuildResult::new(stage_dirs.clone(), auto_deps(false)?))
}

/// The shell fragment that builds a recipe of the given kind.
///
/// Flags are placed in double quotes, so `${COOKBOOK_SYSROOT}` and friends
/// expand, and so does anything else the shell expands inside double quotes.
fn build_script(kind: &BuildKind) -> String {
    fn flags(array: &str, flags: &[String]) -> String {
        let lines: Vec<String> = flags.iter().map(|flag| format!("  \"{flag}\"")).collect();
        format!("{array}+=(\n{}\n)\n", lines.join("\n"))
    }

    match kind {
        BuildKind::Cargo {
            cargopath,
            cargoflags,
            cargopackages,
            cargoexamples,
            clearlocked,
            cargopackagesprefixed,
        } => {
            let mut script = String::from("DYNAMIC_INIT\n");
            if *clearlocked {
                script += "COOKBOOK_CARGO_FLAGS=()\n";
            }
            script += &flags("COOKBOOK_CARGO_FLAGS", cargoflags);
            script += &format!(
                "COOKBOOK_CARGO_PATH={}\n",
                cargopath.as_deref().unwrap_or(".")
            );
            if cargopackages.is_empty() && cargoexamples.is_empty() {
                script += "cookbook_cargo\n";
            }
            if !cargopackages.is_empty() {
                let function = if *cargopackagesprefixed {
                    "cookbook_cargo_packages_prefixed"
                } else {
                    "cookbook_cargo_packages"
                };
                script += &format!("{function} {}\n", cargopackages.join(" "));
            }
            if !cargoexamples.is_empty() {
                script += &format!("cookbook_cargo_examples {}\n", cargoexamples.join(" "));
            }
            script
        }
        BuildKind::Configure { configureflags } => format!(
            "DYNAMIC_INIT\n{}cookbook_configure",
            flags("COOKBOOK_CONFIGURE_FLAGS", configureflags),
        ),
        BuildKind::Cmake { cmakeflags } => format!(
            "DYNAMIC_INIT\n{}cookbook_cmake",
            flags("COOKBOOK_CMAKE_FLAGS", cmakeflags),
        ),
        BuildKind::Meson { mesonflags } => format!(
            "DYNAMIC_INIT\n{}cookbook_meson",
            flags("COOKBOOK_MESON_FLAGS", mesonflags),
        ),
        BuildKind::Python {
            pyprojectpath,
            legacysetup,
            pipflags,
        } => {
            let mut script = String::from("DYNAMIC_INIT\n");
            if *legacysetup {
                script += "COOKBOOK_PYTHON_LEGACY_SETUP=1\n";
            }
            if let Some(path) = pyprojectpath {
                script += &format!("COOKBOOK_PYTHON_PYPROJECT_PATH={path}\n");
            }
            script += &flags("COOKBOOK_PYTHON_PIP_FLAGS", pipflags);
            script + "cookbook_python"
        }
        BuildKind::Custom { script } => script.clone(),
        BuildKind::None | BuildKind::Remote => String::new(),
    }
}

/// The directories a build script works with.
struct BuildDirs<'a> {
    build: &'a Path,
    recipe: &'a Path,
    stage: &'a Path,
    source: &'a Path,
    sysroot: &'a Path,
    toolchain: &'a Path,
}

/// The command that runs a build script, with the `COOKBOOK_*` environment.
fn build_command(
    cook_recipe: &CookRecipe,
    cook_config: &CookConfig,
    dirs: BuildDirs,
) -> Result<Command> {
    let absolute = |path: &Path| {
        path.canonicalize()
            .map_err(wrap_io_err!(path, "Resolving path"))
    };
    let name = &cook_recipe.name;
    let build_dir = absolute(dirs.build)?;
    let sysroot_dir = absolute(dirs.sysroot)?;
    let wrapper = redoxer::cookbook_redoxer()?;

    let mut command = Command::new(&wrapper);
    command.arg("env").arg("bash");
    command.arg(if cook_config.verbose_cmd { "-ex" } else { "-e" });
    command.current_dir(&build_dir);
    command.env("COOKBOOK_REDOXER", &wrapper);
    command.env("TARGET", package_target(name));
    command.env("COOKBOOK_BUILD", &build_dir);
    command.env("COOKBOOK_NAME", name.name());
    command.env("COOKBOOK_HOST_TARGET", ::redoxer::host_target());
    command.env("COOKBOOK_RECIPE", absolute(dirs.recipe)?);
    command.env("COOKBOOK_ROOT", absolute(Path::new("."))?);
    command.env("COOKBOOK_STAGE", absolute(dirs.stage)?);
    command.env("COOKBOOK_SOURCE", absolute(dirs.source)?);
    command.env("COOKBOOK_SYSROOT", &sysroot_dir);
    if let Ok(toolchain_dir) = dirs.toolchain.canonicalize() {
        command.env("COOKBOOK_TOOLCHAIN", toolchain_dir);
    } else if name.is_host() {
        command.env("COOKBOOK_TOOLCHAIN", &sysroot_dir);
    }

    if let Some(compiler) = &cook_recipe.recipe.build.compiler {
        use BuildCompiler::*;
        match compiler {
            Gcc | GccNoLTO => {
                command.env("REDOXER_USE_CLANG", "0");
            }
            Clang | ClangNoLTO | Wasm => {
                command.env("REDOXER_USE_CLANG", "1");
            }
            NoLTO => {}
        }
        if matches!(compiler, ClangNoLTO | GccNoLTO | NoLTO) {
            command.env("REDOXER_USE_LTO", "0");
        }
        if matches!(compiler, Wasm) {
            command.env("TARGET", "wasm32-wasip1");
        }
    }

    command.env("COOKBOOK_MAKE_JOBS", cook_config.jobs.to_string());
    // The scripts test these for being non-empty, so unset means "off".
    let is_cargo = matches!(cook_recipe.recipe.build.kind, BuildKind::Cargo { .. });
    for (key, enabled) in [
        ("COOKBOOK_VERBOSE", cook_config.verbose),
        ("COOKBOOK_VERBOSE_CMD", cook_config.verbose_cmd),
        ("COOKBOOK_OFFLINE", cook_config.offline && is_cargo),
    ] {
        if enabled {
            command.env(key, "1");
        } else {
            command.env_remove(key);
        }
    }
    if let Ok(ident) = fetch::fetch_get_source_info(cook_recipe) {
        command.env("COOKBOOK_SOURCE_IDENT", ident.source_identifier);
        command.env("COOKBOOK_COMMIT_IDENT", ident.commit_identifier);
    }
    Ok(command)
}

/// Move the files each optional package claims out of the freshly built
/// stage into that package's own stage dir.
fn split_optional_packages(recipe: &Recipe, stage: &Path, stage_dirs: &[PathBuf]) -> Result<()> {
    let mut claims = Vec::new();
    for (optional, stage_dir) in recipe.optional_packages.iter().zip(stage_dirs) {
        fs::create_dir_clean(stage_dir)?;
        for pattern in &optional.files {
            let glob = globset::Glob::new(pattern)
                .map_err(|e| Error::Other(format!("Invalid files glob {pattern:?}: {e}")))?;
            claims.push((glob.compile_matcher(), stage_dir.as_path()));
        }
    }
    fs::move_files_matching(stage, &|path| {
        claims
            .iter()
            .find(|(glob, _)| glob.is_match(path))
            .map(|(_, dir)| *dir)
    })
    .map_err(wrap_io_err!(stage, "Splitting optional packages"))
}

/// Whether the source or the recipe changed after the packages were made.
fn build_is_source_newer(
    logger: &PtyOut,
    recipe_dir: &Path,
    source_dir: &Path,
    auto_deps_file: &Path,
    stage_pkgars: &[PathBuf],
) -> bool {
    if !auto_deps_file.is_file() {
        return true;
    }
    let Ok(stage_modified) = fs::modified_newest(stage_pkgars) else {
        return true;
    };
    // A source dir that cannot be read (meta sources) cannot have changed.
    let Ok(source_modified) = fs::modified_dir_ignore_git(source_dir) else {
        return false;
    };
    let recipe_modified = fs::modified(&recipe_dir.join("recipe.toml")).ok();

    if source_modified > stage_modified {
        log_to_pty!(logger, "DEBUG: updating build: source is newer");
        true
    } else if recipe_modified.is_some_and(|modified| modified > stage_modified) {
        log_to_pty!(logger, "DEBUG: updating build: recipe is newer");
        true
    } else {
        false
    }
}

/// Remove a stage dir and the package files derived from it.
pub fn remove_stage_dir(stage_dir: &Path) -> Result<()> {
    if stage_dir.is_dir() {
        fs::remove_all(stage_dir)?;
    }
    for extension in ["pkgar", "toml", "files"] {
        let derived = stage_dir.with_added_extension(extension);
        if derived.is_file() {
            fs::remove_all(&derived)?;
        }
    }
    Ok(())
}

/// The stage dir of each optional package, followed by the main stage dir,
/// which keeps the files no optional package claimed.
pub fn get_stage_dirs(optional: &[OptionalPackageRecipe], target_dir: &Path) -> Vec<PathBuf> {
    optional
        .iter()
        .map(|package| format!("stage.{}", package.name))
        .chain(std::iter::once("stage".to_string()))
        .map(|dir| get_sub_target_dir(target_dir, &dir))
        .collect()
}

/// `sub_path` inside the target dir, under the cross target when one is set.
pub fn get_sub_target_dir(target_dir: &Path, sub_path: &str) -> PathBuf {
    match crate::cross_target() {
        Some(cross_target) => target_dir.join(cross_target).join(sub_path),
        None => target_dir.join(sub_path),
    }
}

/// Make `deps_dir` a sysroot containing exactly `dep_pkgars`.
///
/// Each installed package leaves a tag holding its hash in `.tags`, so an
/// unchanged set of dependencies is recognised without reinstalling. Returns
/// whether the existing directory could be kept.
///
/// With `clean_target` the directory is emptied after each build and only the
/// tags survive. `.tags-init` marks a directory that is actually populated:
/// `check_init_tag` requires it, `write_init_tag` creates it.
fn build_deps_dir(
    logger: &PtyOut,
    deps_dir: &Path,
    dep_pkgars: &DepArchives,
    check_init_tag: bool,
    write_init_tag: bool,
) -> Result<bool> {
    if dep_pkgars.is_empty() && !deps_dir.join(".tags").is_dir() {
        // Nothing to verify, so no key is needed to create the empty layout.
        populate_deps_dir(deps_dir, dep_pkgars, &Default::default(), write_init_tag)?;
        return Ok(false);
    }
    let public_key = match PublicKeyFile::open(PUBLIC_KEY_PATH) {
        Ok(key_file) => key_file.pkey,
        Err(_) if dep_pkgars.is_empty() => Default::default(),
        Err(e) => return Err(e.into()),
    };
    let dir_name = deps_dir.file_name().unwrap_or_default().display();

    let tags_dir = deps_dir.join(".tags");
    if tags_dir.is_dir() {
        let expected = dep_pkgars
            .iter()
            .map(|(name, _)| name.without_prefix())
            .collect();
        let mut cached = fs::check_files_present(&tags_dir, &expected)?;
        if cached {
            for (name, pkgar_path) in dep_pkgars {
                let tag = fs::read_to_string(&tags_dir.join(name.without_prefix()))?;
                let Ok(tag_hash) = blake3::Hash::from_hex(tag.trim()) else {
                    log_to_pty!(
                        logger,
                        "DEBUG: updating {dir_name:?}: {name} has no valid tag"
                    );
                    cached = false;
                    continue;
                };
                let pkgar_hash = PackageFile::new(pkgar_path, &public_key)?.header().blake3;
                if *tag_hash.as_bytes() != pkgar_hash {
                    log_to_pty!(logger, "DEBUG: updating {dir_name:?}: {name} is updated");
                    cached = false;
                }
            }
        }
        if cached && (!check_init_tag || deps_dir.join(".tags-init").exists()) {
            return Ok(true);
        }
        fs::remove_all(deps_dir)?;
    }

    populate_deps_dir(deps_dir, dep_pkgars, &public_key, write_init_tag)?;
    Ok(false)
}

/// Install `dep_pkgars` into a fresh `<deps_dir>.tmp` and move it into place.
fn populate_deps_dir(
    deps_dir: &Path,
    dep_pkgars: &DepArchives,
    public_key: &pkgar_core::PublicKey,
    write_init_tag: bool,
) -> Result<()> {
    if deps_dir.is_dir() {
        fs::remove_all(deps_dir)?;
    }
    let deps_dir_tmp = deps_dir.with_added_extension("tmp");
    fs::create_dir_clean(&deps_dir_tmp)?;
    let tags_dir = deps_dir_tmp.join(".tags");
    let usr_dir = deps_dir_tmp.join("usr");
    fs::create_dir(&tags_dir)?;

    for folder in ["bin", "include", "lib", "share"] {
        // Merged /usr: sysroot/<folder> is a link to sysroot/usr/<folder>.
        fs::create_dir(&usr_dir.join(folder))?;
        fs::symlink(Path::new("usr").join(folder), deps_dir_tmp.join(folder))?;
    }

    for (name, archive_path) in dep_pkgars {
        let mut package = PackageFile::new(archive_path, public_key)?;
        Transaction::install(&mut package, &deps_dir_tmp)?.commit()?;
        let hash = blake3::Hash::from_bytes(package.header().blake3).to_hex();
        let tag_file = tags_dir.join(name.without_prefix());
        std::fs::write(&tag_file, hash.as_bytes())
            .map_err(wrap_io_err!(tag_file, "Writing tag"))?;
    }

    if write_init_tag {
        let init_tag = deps_dir_tmp.join(".tags-init");
        std::fs::write(&init_tag, "").map_err(wrap_io_err!(init_tag, "Writing init tag"))?;
    }

    fs::rename(&deps_dir_tmp, deps_dir)
}

/// Empty a deps dir, keeping only its tags so that a later run can still
/// tell whether the dependencies changed.
fn clean_deps_dir(deps_dir: &Path) -> Result<()> {
    let tags_dir = deps_dir.join(".tags");
    let tags_dir_tmp = deps_dir.with_added_extension("tags");
    fs::rename(&tags_dir, &tags_dir_tmp)?;
    fs::remove_all(deps_dir)?;
    fs::create_dir(deps_dir)?;
    fs::rename(&tags_dir_tmp, &tags_dir)
}

/// The runtime dependencies of a built recipe: the packages providing the
/// shared libraries it links to, plus the runtime dependencies of what it
/// linked statically. Cached in `auto_deps.toml`.
fn build_auto_deps(
    recipe: &Recipe,
    auto_deps_path: &Path,
    stage_dirs: &[PathBuf],
    cached: bool,
    cook_config: &CookConfig,
    dep_pkgars: &DepArchives,
    logger: &PtyOut,
) -> Result<BTreeSet<PackageName>> {
    if auto_deps_path.is_file() {
        if cached {
            return Ok(fs::read_toml::<AutoDeps>(auto_deps_path)?.packages);
        }
        if cook_config.verbose {
            log_to_pty!(logger, "DEBUG: updating {}", auto_deps_path.display());
        }
        fs::remove_all(auto_deps_path)?;
    }

    let mut packages = auto_deps_from_dynamic_linking(stage_dirs, dep_pkgars, logger);
    let linked_statically: Vec<PackageName> = dep_pkgars
        .iter()
        .map(|(name, _)| name)
        .filter(|name| recipe.build.dependencies.contains(name) && !packages.contains(*name))
        .cloned()
        .collect();
    // Best effort: a dependency without a recipe contributes nothing.
    if let Ok(inherited) = CookRecipe::get_package_deps_recursive(&linked_statically, false) {
        packages.extend(inherited);
    }

    let auto_deps = AutoDeps { packages };
    fs::serialize_and_write(auto_deps_path, &auto_deps)?;
    Ok(auto_deps.packages)
}

/// "Build" a recipe by unpacking the packages downloaded for it.
pub fn build_remote(
    stage_dirs: Vec<PathBuf>,
    stage_pkgars: Vec<PathBuf>,
    recipe: &Recipe,
    target_dir: &Path,
    logger: &PtyOut,
) -> Result<BuildResult> {
    let (_, _, source_toml) = package_source_paths(None, target_dir);
    let auto_deps_path = target_dir.join("auto_deps.toml");
    let packages = recipe.get_packages_list();

    let mut cached = auto_deps_path.is_file();
    for (package, stage_pkgar) in packages.iter().zip(&stage_pkgars) {
        if !cached {
            break;
        }
        let (_, source_pkgar, _) = package_source_paths(*package, target_dir);
        cached = stage_pkgar.is_file() && !fs::modified_is_newer(&source_pkgar, stage_pkgar);
    }
    if cached {
        log_to_pty!(logger, "DEBUG: using cached build");
        let auto_deps: AutoDeps = fs::read_toml(&auto_deps_path)?;
        return Ok(BuildResult::cached(stage_dirs, auto_deps.packages));
    }

    let public_key_path = fetch_repo::get_binary_pubkey()?;
    for (package, stage_dir) in packages.into_iter().zip(&stage_dirs) {
        let (_, source_pkgar, _) = package_source_paths(package, target_dir);
        fs::create_dir_clean(stage_dir)?;
        pkgar::extract(&public_key_path, &source_pkgar, stage_dir).inspect_err(|e| {
            if matches!(e, pkgar::Error::Core(pkgar_core::Error::Dryoc(_))) {
                hint_incorrect_pkey(logger, &public_key_path, &source_pkgar);
            }
        })?;
    }

    let package_toml = Package::from_toml(&fs::read_to_string(&source_toml)?)?;
    let auto_deps = AutoDeps {
        packages: package_toml.depends.into_iter().collect(),
    };
    fs::serialize_and_write(&auto_deps_path, &auto_deps)?;
    Ok(BuildResult::new(stage_dirs, auto_deps.packages))
}

/// Explain a signature failure by showing both keys involved.
fn hint_incorrect_pkey(logger: &PtyOut, public_key_path: &Path, pkgar_path: &Path) {
    let fingerprint =
        |key: pkgar_core::PublicKey| blake3::Hash::from_bytes(key).to_hex().to_string();
    let (Ok(signed_with), Ok(trusted)) = (
        PublicKeyFile::from_header_path(pkgar_path),
        PublicKeyFile::open(public_key_path),
    ) else {
        return;
    };
    log_to_pty!(
        logger,
        "DEBUG: Package key mismatch:\n - package is signed with: {:?}\n - trusted key is:        {:?}",
        fingerprint(signed_with.pkey),
        fingerprint(trusted.pkey),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix;

    #[test]
    fn file_system_loop_does_not_hang() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("usr/lib")).unwrap();
        unix::fs::symlink(&root, root.join("usr/lib/loop")).unwrap();
        assert_eq!(
            root.canonicalize().unwrap(),
            root.join("usr/lib/loop").canonicalize().unwrap()
        );

        let deps = auto_deps_from_dynamic_linking(&[root], &Default::default(), &None);
        assert!(deps.is_empty());
    }

    #[test]
    fn staged_binaries_finds_nested_files_only_in_binary_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let stage = tmp.path().to_path_buf();
        for file in ["usr/bin/tool", "usr/lib/sub/dir/lib.so", "usr/share/doc"] {
            let path = stage.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let stages = [stage.clone()];
        let found: Vec<PathBuf> = staged_binaries(&stages)
            .into_iter()
            .map(|(_, path)| path.strip_prefix(&stage).unwrap().to_path_buf())
            .collect();
        assert_eq!(
            found,
            [
                PathBuf::from("usr/bin/tool"),
                PathBuf::from("usr/lib/sub/dir/lib.so")
            ]
        );
    }

    #[test]
    fn stage_dirs_end_with_the_main_stage() {
        let optional = [OptionalPackageRecipe {
            name: "dev".into(),
            ..Default::default()
        }];
        let dirs = get_stage_dirs(&optional, Path::new("t"));
        if crate::cross_target().is_none() {
            assert_eq!(
                dirs,
                [PathBuf::from("t/stage.dev"), PathBuf::from("t/stage")]
            );
        }
        assert!(dirs.last().unwrap().ends_with("stage"));
    }

    fn cargo(packages: &[&str], examples: &[&str], prefixed: bool, clearlocked: bool) -> BuildKind {
        BuildKind::Cargo {
            cargopath: None,
            cargoflags: vec!["--features".into(), "x".into()],
            cargopackages: packages.iter().map(|s| s.to_string()).collect(),
            cargoexamples: examples.iter().map(|s| s.to_string()).collect(),
            clearlocked,
            cargopackagesprefixed: prefixed,
        }
    }

    #[test]
    fn cargo_scripts() {
        let plain = build_script(&cargo(&[], &[], false, false));
        assert_eq!(
            plain,
            "DYNAMIC_INIT\nCOOKBOOK_CARGO_FLAGS+=(\n  \"--features\"\n  \"x\"\n)\n\
             COOKBOOK_CARGO_PATH=.\ncookbook_cargo\n"
        );

        let both = build_script(&cargo(&["a", "b"], &["demo"], true, true));
        assert!(both.contains("COOKBOOK_CARGO_FLAGS=()\nCOOKBOOK_CARGO_FLAGS+=("));
        assert!(both.contains("\ncookbook_cargo_packages_prefixed a b\n"));
        assert!(both.ends_with("cookbook_cargo_examples demo\n"));
        assert!(!both.contains("\ncookbook_cargo\n"));

        let unprefixed = build_script(&cargo(&["a"], &[], false, false));
        assert!(unprefixed.contains("\ncookbook_cargo_packages a\n"));
    }

    #[test]
    fn template_scripts() {
        let cmake = build_script(&BuildKind::Cmake {
            cmakeflags: vec!["-DX=${COOKBOOK_SYSROOT}".into()],
        });
        assert_eq!(
            cmake,
            "DYNAMIC_INIT\nCOOKBOOK_CMAKE_FLAGS+=(\n  \"-DX=${COOKBOOK_SYSROOT}\"\n)\ncookbook_cmake"
        );
        let python = build_script(&BuildKind::Python {
            pyprojectpath: Some("sub".into()),
            pipflags: vec![],
            legacysetup: true,
        });
        assert!(python.contains("COOKBOOK_PYTHON_LEGACY_SETUP=1\n"));
        assert!(python.contains("COOKBOOK_PYTHON_PYPROJECT_PATH=sub\n"));
        assert!(python.ends_with("cookbook_python"));
        let custom = build_script(&BuildKind::Custom {
            script: "make".into(),
        });
        assert_eq!(custom, "make");
    }

    #[test]
    fn split_moves_claimed_files() {
        let tmp = tempfile::tempdir().unwrap();
        let stage = tmp.path().join("stage.tmp");
        for file in ["usr/include/a.h", "usr/lib/liba.so"] {
            let path = stage.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let recipe = Recipe {
            optional_packages: vec![OptionalPackageRecipe {
                name: "dev".into(),
                files: vec!["usr/include/**".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let stage_dirs = [tmp.path().join("stage.dev"), tmp.path().join("stage")];
        split_optional_packages(&recipe, &stage, &stage_dirs).unwrap();
        assert!(stage_dirs[0].join("usr/include/a.h").is_file());
        assert!(stage.join("usr/lib/liba.so").is_file());
        assert!(!stage.join("usr/include/a.h").exists());
    }

    #[test]
    fn deps_dir_without_dependencies_has_merged_usr_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let sysroot = tmp.path().join("sysroot");
        let cached = build_deps_dir(&None, &sysroot, &DepArchives::new(), false, true).unwrap();
        assert!(!cached);
        assert!(sysroot.join("usr/lib").is_dir());
        assert_eq!(
            std::fs::read_link(sysroot.join("lib")).unwrap(),
            PathBuf::from("usr/lib")
        );
        assert!(sysroot.join(".tags-init").is_file());

        // A second run recognises the unchanged directory.
        assert!(build_deps_dir(&None, &sysroot, &DepArchives::new(), true, true).unwrap());

        // After a clean only the tags remain, so it must be repopulated.
        clean_deps_dir(&sysroot).unwrap();
        assert!(!sysroot.join("usr").exists());
        assert!(build_deps_dir(&None, &sysroot, &DepArchives::new(), false, true).unwrap());
        assert!(!build_deps_dir(&None, &sysroot, &DepArchives::new(), true, true).unwrap());
        assert!(sysroot.join("usr/lib").is_dir());
    }

    #[test]
    fn remove_stage_dir_removes_derived_files() {
        let tmp = tempfile::tempdir().unwrap();
        let stage = tmp.path().join("stage");
        std::fs::create_dir(&stage).unwrap();
        for extension in ["pkgar", "toml", "files"] {
            std::fs::write(stage.with_added_extension(extension), "").unwrap();
        }
        remove_stage_dir(&stage).unwrap();
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }
}
