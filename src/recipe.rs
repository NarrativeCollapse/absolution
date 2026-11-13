//! The `recipe.toml` schema and dependency resolution between recipes.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use pkg::{PackageError, PackageName};
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use crate::{bail_other_err, cook::package as cook_package, staged_pkg};

/// Specifies how to obtain the source for a recipe.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum SourceRecipe {
    /// Reuse the source directory of another recipe.
    ///
    /// Useful when a single source repo contains multiple projects which each
    /// have their own recipe to build them.
    SameAs {
        /// Relative path to the recipe whose source dir is reused
        same_as: String,
    },
    /// A directory on the build machine, copied into the recipe.
    Path { path: String },
    /// A git repository.
    Git {
        /// The repository URL, such as https://example.com/project.git
        git: String,
        /// The URL of the upstream repository, for forks
        upstream: Option<String>,
        /// The branch to track. Defaults to the remote's default branch
        branch: Option<String>,
        /// The tag or commit to build. Specify it for reproducible builds
        rev: Option<String>,
        /// Whether to make a treeless clone. Defaults to the global setting
        shallow_clone: Option<bool>,
        /// Patch files, relative to the recipe, applied in order
        #[serde(default)]
        patches: Vec<String>,
        /// Shell script run in the source directory after patching
        script: Option<String>,
    },
    /// A tar archive.
    Tar {
        /// The archive URL
        tar: String,
        /// The blake3 hash of the archive. Without it the download cannot be
        /// verified
        blake3: Option<String>,
        /// Patch files, relative to the recipe, applied in order
        #[serde(default)]
        patches: Vec<String>,
        /// Shell script run in the source directory after patching
        script: Option<String>,
    },
}

/// `[source]` as written: every key any source kind accepts, so that a
/// misspelled or misplaced key is an error instead of being silently ignored.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSource {
    same_as: Option<String>,
    path: Option<String>,
    git: Option<String>,
    tar: Option<String>,
    upstream: Option<String>,
    branch: Option<String>,
    rev: Option<String>,
    shallow_clone: Option<bool>,
    blake3: Option<String>,
    patches: Option<Vec<String>>,
    script: Option<String>,
}

impl TryFrom<RawSource> for SourceRecipe {
    type Error = String;

    fn try_from(raw: RawSource) -> Result<Self, String> {
        let kinds: Vec<&str> = [
            ("same_as", raw.same_as.is_some()),
            ("path", raw.path.is_some()),
            ("git", raw.git.is_some()),
            ("tar", raw.tar.is_some()),
        ]
        .into_iter()
        .filter_map(|(kind, present)| present.then_some(kind))
        .collect();
        let kind = match kinds[..] {
            [kind] => kind,
            [] => return Err("source needs one of `git`, `tar`, `path` or `same_as`".into()),
            _ => return Err(format!("source sets more than one of {kinds:?}")),
        };

        // Every key that is only meaningful for some kinds, with those kinds.
        let extras = [
            ("upstream", raw.upstream.is_some(), &["git"][..]),
            ("branch", raw.branch.is_some(), &["git"]),
            ("rev", raw.rev.is_some(), &["git"]),
            ("shallow_clone", raw.shallow_clone.is_some(), &["git"]),
            ("blake3", raw.blake3.is_some(), &["tar"]),
            ("patches", raw.patches.is_some(), &["git", "tar"]),
            ("script", raw.script.is_some(), &["git", "tar"]),
        ];
        for (key, present, valid_for) in extras {
            if present && !valid_for.contains(&kind) {
                return Err(format!("`{key}` is not valid for a `{kind}` source"));
            }
        }
        if let Some(hash) = &raw.blake3
            && (hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(format!("`blake3` must be 64 hex digits, got {hash:?}"));
        }

        let patches = raw.patches.unwrap_or_default();
        Ok(match (raw.same_as, raw.path, raw.git, raw.tar) {
            (Some(same_as), ..) => SourceRecipe::SameAs { same_as },
            (_, Some(path), ..) => SourceRecipe::Path { path },
            (_, _, Some(git), _) => SourceRecipe::Git {
                git,
                upstream: raw.upstream,
                branch: raw.branch,
                rev: raw.rev,
                shallow_clone: raw.shallow_clone,
                patches,
                script: raw.script,
            },
            (_, _, _, Some(tar)) => SourceRecipe::Tar {
                tar,
                blake3: raw.blake3,
                patches,
                script: raw.script,
            },
            (None, None, None, None) => unreachable!("exactly one kind is set"),
        })
    }
}

impl<'de> Deserialize<'de> for SourceRecipe {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        RawSource::deserialize(deserializer)?
            .try_into()
            .map_err(D::Error::custom)
    }
}

/// Specifies how to build a recipe.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(tag = "template", rename_all = "lowercase")]
pub enum BuildKind {
    /// Nothing to build (meta packages)
    #[default]
    None,
    /// Download the built package from the remote repository
    Remote,
    /// Build and install using cargo
    Cargo {
        /// Path relative to the source dir where cargo is run
        cargopath: Option<String>,
        /// Additional flags for cargo
        cargoflags: Vec<String>,
        /// Workspace packages to build. Binary names are not prefixed
        cargopackages: Vec<String>,
        /// Examples to build. Binary names are prefixed
        cargoexamples: Vec<String>,
        /// Whether to drop `--locked`, common for libraries
        clearlocked: bool,
        /// Whether to prefix package binaries with the recipe name
        cargopackagesprefixed: bool,
    },
    /// Build and install using configure and make
    Configure { configureflags: Vec<String> },
    /// Build and install using cmake
    Cmake { cmakeflags: Vec<String> },
    /// Build and install using meson
    Meson { mesonflags: Vec<String> },
    /// Build and install using pip
    Python {
        pyprojectpath: Option<String>,
        pipflags: Vec<String>,
        /// Run `setup.py install` first. Needed by projects that fail with
        /// errors like "invalid command 'dist_info'".
        legacysetup: bool,
    },
    /// Build and install using a custom script
    Custom { script: String },
}

impl BuildKind {
    /// The value of the `template` key selecting this kind.
    pub fn template(&self) -> &'static str {
        match self {
            BuildKind::None => "none",
            BuildKind::Remote => "remote",
            BuildKind::Cargo { .. } => "cargo",
            BuildKind::Configure { .. } => "configure",
            BuildKind::Cmake { .. } => "cmake",
            BuildKind::Meson { .. } => "meson",
            BuildKind::Python { .. } => "python",
            BuildKind::Custom { .. } => "custom",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct BuildRecipe {
    #[serde(flatten)]
    pub kind: BuildKind,
    pub dependencies: Vec<PackageName>,
    pub compiler: Option<BuildCompiler>,
    #[serde(rename = "dev-dependencies")]
    pub dev_dependencies: Vec<PackageName>,
}

/// `[build]` as written; see [`RawSource`] for why.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawBuild {
    template: Option<String>,
    dependencies: Vec<PackageName>,
    #[serde(rename = "dev-dependencies")]
    dev_dependencies: Vec<PackageName>,
    compiler: Option<BuildCompiler>,
    script: Option<String>,
    cargopath: Option<String>,
    cargoflags: Option<Vec<String>>,
    cargopackages: Option<Vec<String>>,
    cargoexamples: Option<Vec<String>>,
    clearlocked: Option<bool>,
    cargopackagesprefixed: Option<bool>,
    configureflags: Option<Vec<String>>,
    cmakeflags: Option<Vec<String>>,
    mesonflags: Option<Vec<String>>,
    pyprojectpath: Option<String>,
    pipflags: Option<Vec<String>>,
    legacysetup: Option<bool>,
}

impl TryFrom<RawBuild> for BuildRecipe {
    type Error = String;

    fn try_from(mut raw: RawBuild) -> Result<Self, String> {
        let template = raw.template.take().unwrap_or_else(|| "none".into());
        let kind = match template.as_str() {
            "none" => BuildKind::None,
            "remote" => BuildKind::Remote,
            "cargo" => BuildKind::Cargo {
                cargopath: raw.cargopath.take(),
                cargoflags: raw.cargoflags.take().unwrap_or_default(),
                cargopackages: raw.cargopackages.take().unwrap_or_default(),
                cargoexamples: raw.cargoexamples.take().unwrap_or_default(),
                clearlocked: raw.clearlocked.take().unwrap_or_default(),
                cargopackagesprefixed: raw.cargopackagesprefixed.take().unwrap_or_default(),
            },
            "configure" => BuildKind::Configure {
                configureflags: raw.configureflags.take().unwrap_or_default(),
            },
            "cmake" => BuildKind::Cmake {
                cmakeflags: raw.cmakeflags.take().unwrap_or_default(),
            },
            "meson" => BuildKind::Meson {
                mesonflags: raw.mesonflags.take().unwrap_or_default(),
            },
            "python" => BuildKind::Python {
                pyprojectpath: raw.pyprojectpath.take(),
                pipflags: raw.pipflags.take().unwrap_or_default(),
                legacysetup: raw.legacysetup.take().unwrap_or_default(),
            },
            "custom" => BuildKind::Custom {
                script: raw
                    .script
                    .take()
                    .ok_or("the `custom` template needs a `script`")?,
            },
            other => {
                return Err(format!(
                    "unknown build template {other:?}, expected one of none, remote, cargo, \
                     configure, cmake, meson, python or custom"
                ));
            }
        };

        // Whatever was not consumed above belongs to a different template.
        let leftovers = [
            ("script", raw.script.is_some()),
            ("cargopath", raw.cargopath.is_some()),
            ("cargoflags", raw.cargoflags.is_some()),
            ("cargopackages", raw.cargopackages.is_some()),
            ("cargoexamples", raw.cargoexamples.is_some()),
            ("clearlocked", raw.clearlocked.is_some()),
            ("cargopackagesprefixed", raw.cargopackagesprefixed.is_some()),
            ("configureflags", raw.configureflags.is_some()),
            ("cmakeflags", raw.cmakeflags.is_some()),
            ("mesonflags", raw.mesonflags.is_some()),
            ("pyprojectpath", raw.pyprojectpath.is_some()),
            ("pipflags", raw.pipflags.is_some()),
            ("legacysetup", raw.legacysetup.is_some()),
        ];
        if let Some((key, _)) = leftovers.iter().find(|(_, present)| *present) {
            return Err(format!(
                "`{key}` is not valid for the `{template}` build template"
            ));
        }

        Ok(BuildRecipe {
            kind,
            dependencies: raw.dependencies,
            compiler: raw.compiler,
            dev_dependencies: raw.dev_dependencies,
        })
    }
}

impl<'de> Deserialize<'de> for BuildRecipe {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        RawBuild::deserialize(deserializer)?
            .try_into()
            .map_err(D::Error::custom)
    }
}

impl BuildRecipe {
    pub fn new(kind: BuildKind) -> Self {
        Self {
            kind,
            ..Default::default()
        }
    }

    /// Download the package instead of building it. Meta packages have
    /// nothing to download and are left alone.
    pub fn set_as_remote(&mut self) {
        if self.kind == BuildKind::None {
            return;
        }
        self.kind = BuildKind::Remote;
        self.dev_dependencies = Vec::new();
    }

    pub fn set_as_none(&mut self) {
        self.kind = BuildKind::None;
        self.dependencies = Vec::new();
        self.dev_dependencies = Vec::new();
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Serialize)]
pub enum BuildCompiler {
    #[serde(rename = "gcc")]
    Gcc,
    #[serde(rename = "clang")]
    Clang,
    #[serde(rename = "gcc-no-lto")]
    GccNoLTO,
    #[serde(rename = "clang-no-lto")]
    ClangNoLTO,
    #[serde(rename = "no-lto")]
    NoLTO,
    #[serde(rename = "wasm")]
    Wasm,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PackageRecipe {
    /// Packages needed at run time
    pub dependencies: Vec<PackageName>,
    pub version: Option<String>,
    pub description: Option<String>,
}

/// A package split out of a recipe's build output.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OptionalPackageRecipe {
    pub name: String,
    pub dependencies: Vec<PackageName>,
    /// Globs, relative to the stage dir, of the files that belong to it
    pub files: Vec<String>,
}

/// Everything required to build a package.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Recipe {
    /// How to obtain the source
    pub source: Option<SourceRecipe>,
    /// How to build it
    pub build: BuildRecipe,
    /// How to package it
    pub package: PackageRecipe,
    /// Additional packages split out of the build output
    #[serde(rename = "optional-packages")]
    pub optional_packages: Vec<OptionalPackageRecipe>,
}

impl Recipe {
    /// Read and validate a `recipe.toml`.
    pub fn load(file: &Path) -> Result<Recipe, PackageError> {
        if !file.is_file() {
            return Err(PackageError::FileMissing(file.to_path_buf()));
        }
        let toml = fs::read_to_string(file)
            .map_err(|err| PackageError::FileError(err.raw_os_error(), file.to_path_buf()))?;
        toml::from_str(&toml).map_err(|err| PackageError::Parse(err, Some(file.to_path_buf())))
    }

    /// The optional packages followed by the mandatory one (`None`), which
    /// comes last because it receives the files no optional package claimed.
    pub fn get_packages_list(&self) -> Vec<Option<&OptionalPackageRecipe>> {
        self.optional_packages
            .iter()
            .map(Some)
            .chain(std::iter::once(None))
            .collect()
    }
}

/// Where a recipe's package comes from, as chosen by the image configuration
/// or `cookbook.lock`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecipeRule {
    /// Fetch the source and build it
    Source,
    /// Download the built package
    Binary,
    /// Build the source that is already there, keeping local changes
    Local,
    /// Do not build or install it
    Ignore,
}

impl RecipeRule {
    pub const fn as_str(self) -> &'static str {
        match self {
            RecipeRule::Source => "source",
            RecipeRule::Binary => "binary",
            RecipeRule::Local => "local",
            RecipeRule::Ignore => "ignore",
        }
    }

    pub fn from_repo_binary(repo_binary: bool) -> Self {
        if repo_binary {
            RecipeRule::Binary
        } else {
            RecipeRule::Source
        }
    }

    /// The opposite of a default rule: source for binary and vice versa.
    pub fn inverted_default(self) -> Self {
        if self == RecipeRule::Binary {
            RecipeRule::Source
        } else {
            RecipeRule::Binary
        }
    }
}

impl fmt::Display for RecipeRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RecipeRule {
    type Err = crate::Error;

    fn from_str(value: &str) -> crate::Result<Self> {
        Ok(match value {
            "source" => Self::Source,
            "binary" => Self::Binary,
            "local" => Self::Local,
            "ignore" => Self::Ignore,
            other => bail_other_err!(
                "Invalid rule {other:?}, expected 'source', 'local', 'binary' or 'ignore'"
            ),
        })
    }
}

/// A recipe together with where it lives and how it is to be cooked.
#[derive(Debug, Clone, PartialEq)]
pub struct CookRecipe {
    pub name: PackageName,
    pub dir: PathBuf,
    pub recipe: Recipe,
    pub target: &'static str,
    /// True when the recipe was pulled in as a dependency rather than
    /// requested; its source is then not checked for updates.
    pub is_deps: bool,
    /// The rule applied to this recipe, once one has been.
    pub rule: Option<RecipeRule>,
    /// Whether the revision is pinned by `cookbook.lock`
    pub pinned: bool,
}

impl CookRecipe {
    pub fn new(name: PackageName, dir: PathBuf, mut recipe: Recipe) -> Self {
        let target = cook_package::package_target(&name);
        if name.is_host() {
            // A host recipe is built for the build machine, so are its
            // dependencies, except those explicitly marked for the target.
            let this_name = name.without_host().to_string();
            let to_host = |dep: PackageName| {
                if dep.is_host() {
                    (dep.name() != this_name).then_some(dep)
                } else if dep.is_target() {
                    None
                } else {
                    Some(dep.with_host())
                }
            };
            let build = &mut recipe.build;
            build.dependencies = std::mem::take(&mut build.dependencies)
                .into_iter()
                .filter_map(to_host)
                .collect();
            build.dev_dependencies = std::mem::take(&mut build.dev_dependencies)
                .into_iter()
                .filter_map(to_host)
                .collect();
            // Host packages are never installed, so they have no runtime deps.
            recipe.package.dependencies = Vec::new();
        }
        Self {
            name,
            dir,
            recipe,
            target,
            is_deps: false,
            rule: None,
            pinned: false,
        }
    }

    /// A recipe with no directory or content, for names that have no recipe.
    pub fn dummy(name: &PackageName) -> Self {
        Self {
            name: name.clone(),
            dir: PathBuf::new(),
            recipe: Recipe::default(),
            target: redoxer::target(),
            is_deps: false,
            rule: None,
            pinned: false,
        }
    }

    pub fn from_name(name: PackageName) -> Result<Self, PackageError> {
        let dir = staged_pkg::find(name.name())
            .ok_or_else(|| PackageError::PackageNotFound(name.clone()))?;
        let recipe = Recipe::load(&dir.join("recipe.toml"))?;
        Ok(Self::new(name, dir.to_path_buf(), recipe))
    }

    pub fn from_list(names: Vec<PackageName>) -> Result<Vec<Self>, PackageError> {
        names.into_iter().map(Self::from_name).collect()
    }

    /// Load the recipe in `dir`. With `read_recipe` false the recipe content
    /// is left empty, which is enough for cleaning.
    pub fn from_path(dir: &Path, read_recipe: bool, is_host: bool) -> Result<Self, PackageError> {
        let dir_name = dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| PackageError::PackageNameInvalid(dir.display().to_string()))?;
        let mut name = PackageName::new(dir_name)?;
        if is_host {
            name = name.with_host();
        }
        let recipe = if read_recipe {
            Recipe::load(&dir.join("recipe.toml"))?
        } else {
            Recipe::default()
        };
        Ok(Self::new(name, dir.to_path_buf(), recipe))
    }

    /// The build dependencies of `names`, each before its dependents.
    /// `names` themselves are included.
    pub fn get_build_deps_recursive(
        names: &[PackageName],
        include_dev: bool,
    ) -> Result<Vec<Self>, PackageError> {
        DepWalk {
            follow_dev: include_dev,
            follow_package: false,
            collect_build: true,
            collect_package: false,
            collect_roots: true,
        }
        .resolve(names, &Self::from_name)
    }

    /// The runtime dependencies of `names`, each before its dependents.
    ///
    /// Build dependencies are walked too, because a library linked at build
    /// time can itself have runtime dependencies, but they are not returned.
    pub fn get_package_deps_recursive(
        names: &[PackageName],
        include_names: bool,
    ) -> Result<Vec<PackageName>, PackageError> {
        let recipes = DepWalk {
            follow_dev: false,
            follow_package: true,
            collect_build: false,
            collect_package: true,
            collect_roots: include_names,
        }
        .resolve(names, &Self::from_name)?;
        Ok(recipes.into_iter().map(|recipe| recipe.name).collect())
    }

    /// Every build and runtime dependency of `names`, and `names`.
    pub fn get_all_deps_names_recursive(
        names: &[PackageName],
        include_dev: bool,
    ) -> Result<Vec<PackageName>, PackageError> {
        let recipes = DepWalk {
            follow_dev: include_dev,
            follow_package: true,
            collect_build: true,
            collect_package: true,
            collect_roots: true,
        }
        .resolve(names, &Self::from_name)?;
        Ok(recipes.into_iter().map(|recipe| recipe.name).collect())
    }

    /// Re-read `recipe.toml`, then re-apply the current rule. Pinned recipes
    /// are left alone since their revision does not come from the file.
    pub fn reload_recipe(&mut self) -> Result<(), PackageError> {
        if self.pinned {
            return Ok(());
        }
        self.recipe = Self::from_path(&self.dir, true, self.name.is_host())?.recipe;
        if let Some(rule) = self.rule {
            self.apply_rule(rule);
        }
        Ok(())
    }

    /// Returns the stage dir, the package archive and the package metadata.
    pub fn stage_paths(&self) -> (PathBuf, PathBuf, PathBuf) {
        let optional = self.name.suffix().map(|suffix| OptionalPackageRecipe {
            name: suffix.to_string(),
            ..Default::default()
        });
        cook_package::package_stage_paths(optional.as_ref(), &self.target_dir())
    }

    /// The target dir, which differs between host and target recipes.
    pub fn target_dir(&self) -> PathBuf {
        self.dir.join("target").join(self.target)
    }

    /// Adjust the recipe so that fetching and building it follows `rule`.
    pub fn apply_rule(&mut self, rule: RecipeRule) {
        match rule {
            RecipeRule::Source => {}
            RecipeRule::Local => self.recipe.source = None,
            RecipeRule::Binary => {
                self.recipe.source = None;
                self.recipe.build.set_as_remote();
            }
            // Only valid when nothing built from source depends on it; see
            // `validate_filesystem_rules`.
            RecipeRule::Ignore => {
                self.recipe.source = None;
                self.recipe.build.set_as_none();
            }
        }
        self.rule = Some(rule);
    }

    /// The package version: the declared one, else one found in the source
    /// URL, revision or branch, else the version in the source's Cargo.toml.
    /// Meta packages have the empty version.
    pub fn guess_version(&self) -> Option<String> {
        let recipe = &self.recipe;
        if recipe.build.kind == BuildKind::None {
            return Some(String::new());
        }
        if let Some(version) = &recipe.package.version {
            return Some(version.clone());
        }

        let mut source_recipe_dir = self.dir.clone();
        match &recipe.source {
            Some(SourceRecipe::Tar { tar, .. }) => {
                if let Some(version) = extract_version(tar) {
                    return Some(version);
                }
            }
            Some(SourceRecipe::Git { branch, rev, .. }) => {
                let from_ref = [rev, branch]
                    .into_iter()
                    .flatten()
                    .find_map(|text| extract_version(text));
                if let Some(version) = from_ref {
                    return Some(version);
                }
            }
            Some(SourceRecipe::SameAs { same_as }) => {
                source_recipe_dir = self.dir.join(same_as);
            }
            Some(SourceRecipe::Path { .. }) | None => {}
        }

        cargo_manifest_version(&source_recipe_dir.join("source/Cargo.toml"))
    }

    /// The name of the recipe that owns the source this recipe builds.
    pub fn canon_recipe_name(&self) -> PackageName {
        if let Some(SourceRecipe::SameAs { same_as }) = &self.recipe.source {
            let canon = Path::new(same_as)
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| PackageName::new(name).ok());
            match canon {
                Some(name) => return name,
                None => eprintln!(
                    "WARNING: the same_as path of recipe {} is not valid",
                    self.name.as_str()
                ),
            }
        }
        self.name.clone()
    }
}

/// Which dependency edges to follow, and which recipes to return.
///
/// Build dependencies are always followed. A recipe is returned when it is
/// reached through an edge kind that is collected, or is a collected root.
#[derive(Debug, Clone, Copy)]
struct DepWalk {
    follow_dev: bool,
    follow_package: bool,
    collect_build: bool,
    collect_package: bool,
    collect_roots: bool,
}

impl DepWalk {
    /// Walk the dependency graph depth first and return the collected recipes
    /// in dependency order. Each recipe is loaded and expanded only once.
    fn resolve(
        self,
        roots: &[PackageName],
        load: &dyn Fn(PackageName) -> Result<CookRecipe, PackageError>,
    ) -> Result<Vec<CookRecipe>, PackageError> {
        let mut state = WalkState {
            walk: self,
            load,
            loaded: BTreeMap::new(),
            collected: BTreeSet::new(),
            order: Vec::new(),
            stack: Vec::new(),
        };
        for root in roots {
            state.visit(root, self.collect_roots)?;
        }
        let WalkState {
            mut loaded, order, ..
        } = state;
        Ok(order
            .into_iter()
            .filter_map(|name| loaded.remove(&name))
            .collect())
    }
}

struct WalkState<'a> {
    walk: DepWalk,
    load: &'a dyn Fn(PackageName) -> Result<CookRecipe, PackageError>,
    /// Every recipe expanded so far
    loaded: BTreeMap<PackageName, CookRecipe>,
    collected: BTreeSet<PackageName>,
    /// Collected names, dependencies first
    order: Vec<PackageName>,
    /// The chain of recipes currently being expanded
    stack: Vec<PackageName>,
}

impl WalkState<'_> {
    fn visit(&mut self, name: &PackageName, collect: bool) -> Result<(), PackageError> {
        if self.stack.contains(name) {
            let mut chain: std::collections::VecDeque<_> = self.stack.iter().cloned().collect();
            chain.push_back(name.clone());
            return Err(PackageError::Recursion(chain));
        }

        if !self.loaded.contains_key(name) {
            let recipe = (self.load)(name.clone())?;
            self.stack.push(name.clone());

            let build = &recipe.recipe.build;
            let mut edges: Vec<(&PackageName, bool)> = Vec::new();
            edges.extend(
                build
                    .dependencies
                    .iter()
                    .map(|d| (d, self.walk.collect_build)),
            );
            if self.walk.follow_dev {
                let dev = build.dev_dependencies.iter();
                edges.extend(dev.map(|d| (d, self.walk.collect_build)));
            }
            if self.walk.follow_package {
                let package = recipe.recipe.package.dependencies.iter();
                edges.extend(package.map(|d| (d, self.walk.collect_package)));
            }
            for (dependency, collect_dependency) in edges {
                self.visit(dependency, collect_dependency)?;
            }

            self.stack.pop();
            self.loaded.insert(name.clone(), recipe);
        }

        if collect && self.collected.insert(name.clone()) {
            self.order.push(name.clone());
        }
        Ok(())
    }
}

/// Mark every recipe that is not one of `names` as a dependency.
pub fn recipes_mark_as_deps(names: &[PackageName], recipes: &mut [CookRecipe]) {
    for recipe in recipes.iter_mut() {
        recipe.is_deps = !names.contains(&recipe.name);
    }
}

/// Collapse optional packages (`name.suffix`) into the recipe that builds
/// them, keeping the first occurrence of each recipe.
pub fn recipes_flatten_package_names(recipes: Vec<CookRecipe>) -> Vec<CookRecipe> {
    let mut seen = BTreeSet::new();
    recipes
        .into_iter()
        .filter_map(|mut recipe| {
            let mut name = recipe.name.with_suffix(None);
            if recipe.name.is_host() {
                name = name.with_host();
            }
            seen.insert(name.clone()).then(|| {
                recipe.name = name;
                recipe
            })
        })
        .collect()
}

/// Fail early if a recipe configured as `ignore` is still required as a
/// build dependency of another recipe that will be built from source.
pub fn validate_filesystem_rules(recipes: &[CookRecipe]) -> crate::Result<()> {
    let ignored: BTreeSet<&str> = recipes
        .iter()
        .filter(|recipe| recipe.rule == Some(RecipeRule::Ignore))
        .map(|recipe| recipe.name.without_prefix())
        .collect();
    if ignored.is_empty() {
        return Ok(());
    }

    for recipe in recipes {
        let build = &recipe.recipe.build;
        if recipe.rule == Some(RecipeRule::Ignore)
            || matches!(build.kind, BuildKind::None | BuildKind::Remote)
        {
            // Not built from source, so its build dependencies are irrelevant.
            continue;
        }
        let needed: Vec<&str> = build
            .dependencies
            .iter()
            .chain(&build.dev_dependencies)
            .map(|dep| dep.without_prefix())
            .filter(|dep| ignored.contains(dep))
            .collect();
        if !needed.is_empty() {
            bail_other_err!(
                "Invalid filesystem config: {needed:?} are configured as \"ignore\" but are \
                 required as build dependencies by {:?}",
                recipe.name.as_str(),
            );
        }
    }
    Ok(())
}

/// The automatically detected runtime dependencies of a built recipe.
#[derive(Serialize, Deserialize)]
pub struct AutoDeps {
    pub packages: BTreeSet<PackageName>,
}

/// Find the first `1.2` or `1.2.3` style version number in `text`.
pub fn extract_version(text: &str) -> Option<String> {
    static VERSION: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"\d+(\.\d+){1,2}").expect("valid regex"));
    VERSION.find(text).map(|found| found.as_str().to_string())
}

fn cargo_manifest_version(path: &Path) -> Option<String> {
    let manifest: toml::Table = fs::read_to_string(path).ok()?.parse().ok()?;
    let version = |table: &toml::Value| {
        table
            .get("version")
            .and_then(|version| version.as_str())
            .map(str::to_string)
    };
    manifest.get("package").and_then(version).or_else(|| {
        manifest
            .get("workspace")
            .and_then(|workspace| workspace.get("package"))
            .and_then(version)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(text: &str) -> PackageName {
        PackageName::new(text).unwrap()
    }

    fn parse(toml: &str) -> Result<Recipe, toml::de::Error> {
        toml::from_str(toml)
    }

    fn parse_err(toml: &str) -> String {
        parse(toml).unwrap_err().to_string()
    }

    #[test]
    fn git_cargo_recipe() {
        let recipe = parse(
            r#"
            [source]
            git = "https://gitlab.redox-os.org/redox-os/acid.git"
            branch = "master"
            rev = "06344744d3d55a5ac9a62a6059cb363d40699bbc"

            [build]
            template = "cargo"
        "#,
        )
        .unwrap();

        assert_eq!(
            recipe,
            Recipe {
                source: Some(SourceRecipe::Git {
                    git: "https://gitlab.redox-os.org/redox-os/acid.git".to_string(),
                    upstream: None,
                    branch: Some("master".to_string()),
                    rev: Some("06344744d3d55a5ac9a62a6059cb363d40699bbc".to_string()),
                    patches: Vec::new(),
                    script: None,
                    shallow_clone: None,
                }),
                build: BuildRecipe::new(BuildKind::Cargo {
                    cargopath: None,
                    cargoflags: Vec::new(),
                    cargopackages: Vec::new(),
                    cargoexamples: Vec::new(),
                    clearlocked: false,
                    cargopackagesprefixed: false,
                }),
                ..Default::default()
            }
        );
    }

    #[test]
    fn tar_custom_recipe() {
        let recipe = parse(
            r#"
            [source]
            tar = "http://downloads.xiph.org/releases/ogg/libogg-1.3.3.tar.xz"
            blake3 = "8220c0e4082fa26c07b10bfe31f641d2e33ebe1d1bb0b20221b7016bc8b78a3a"

            [build]
            template = "custom"
            script = "make"
        "#,
        )
        .unwrap();

        assert_eq!(
            recipe,
            Recipe {
                source: Some(SourceRecipe::Tar {
                    tar: "http://downloads.xiph.org/releases/ogg/libogg-1.3.3.tar.xz".to_string(),
                    blake3: Some(
                        "8220c0e4082fa26c07b10bfe31f641d2e33ebe1d1bb0b20221b7016bc8b78a3a"
                            .to_string()
                    ),
                    patches: Vec::new(),
                    script: None,
                }),
                build: BuildRecipe::new(BuildKind::Custom {
                    script: "make".to_string()
                }),
                ..Default::default()
            }
        );
    }

    #[test]
    fn meta_recipe() {
        let recipe = parse("[package]\ndependencies = [\"gcc13\"]\n").unwrap();
        assert_eq!(
            recipe,
            Recipe {
                package: PackageRecipe {
                    dependencies: vec![name("gcc13")],
                    ..Default::default()
                },
                ..Default::default()
            }
        );
    }

    #[test]
    fn misspelled_keys_are_rejected() {
        // `b3sum` instead of `blake3` used to leave the tarball unverified.
        let err = parse_err("[source]\ntar = \"https://x/y.tar\"\nb3sum = \"00\"\n");
        assert!(err.contains("b3sum"), "{err}");
        assert!(parse_err("[sorce]\ntar = \"https://x/y.tar\"\n").contains("sorce"));
        assert!(parse_err("[package]\ndepends = []\n").contains("depends"));
        assert!(parse_err("[build]\ntemplate = \"cargo\"\ncargoflag = []\n").contains("cargoflag"));
        assert!(parse_err("[[optional-packages]]\nname = \"x\"\nfile = []\n").contains("file"));
    }

    #[test]
    fn source_kind_must_be_unambiguous() {
        assert!(parse_err("[source]\n").contains("needs one of"));
        let both = parse_err("[source]\ngit = \"a\"\ntar = \"b\"\n");
        assert!(both.contains("more than one"), "{both}");
        let misplaced = parse_err("[source]\ntar = \"b\"\nrev = \"1\"\n");
        assert!(misplaced.contains("`rev` is not valid"), "{misplaced}");
        let hash = parse_err("[source]\ntar = \"b\"\nblake3 = \"abc\"\n");
        assert!(hash.contains("64 hex digits"), "{hash}");
    }

    #[test]
    fn build_keys_must_match_template() {
        let unknown = parse_err("[build]\ntemplate = \"go\"\n");
        assert!(unknown.contains("unknown build template"), "{unknown}");
        let misplaced = parse_err("[build]\ntemplate = \"cmake\"\nmesonflags = []\n");
        assert!(
            misplaced.contains("`mesonflags` is not valid"),
            "{misplaced}"
        );
        let stray = parse_err("[build]\ntemplate = \"cargo\"\nscript = \"x\"\n");
        assert!(stray.contains("`script` is not valid"), "{stray}");
        assert!(parse_err("[build]\ntemplate = \"custom\"\n").contains("needs a `script`"));
    }

    #[test]
    fn recipe_round_trips_through_toml() {
        let recipe = parse(
            r#"
            [source]
            tar = "https://x/y-1.2.tar"
            patches = ["a.patch"]

            [build]
            template = "cmake"
            cmakeflags = ["-DX=1"]
            dependencies = ["zlib"]
        "#,
        )
        .unwrap();
        let again = parse(&toml::to_string(&recipe).unwrap()).unwrap();
        assert_eq!(recipe, again);
    }

    #[test]
    fn rule_parses_and_displays() {
        for rule in [
            RecipeRule::Source,
            RecipeRule::Binary,
            RecipeRule::Local,
            RecipeRule::Ignore,
        ] {
            assert_eq!(rule.to_string().parse::<RecipeRule>().unwrap(), rule);
        }
        assert!("bogus".parse::<RecipeRule>().is_err());
        assert_eq!(RecipeRule::Binary.inverted_default(), RecipeRule::Source);
        assert_eq!(RecipeRule::Source.inverted_default(), RecipeRule::Binary);
    }

    #[test]
    fn rules_rewrite_the_recipe() {
        let recipe = parse(
            "[source]\ngit = \"https://x/y\"\n[build]\ntemplate = \"cargo\"\ndependencies = [\"zlib\"]\n",
        )
        .unwrap();
        let cook = |rule| {
            let mut cook = CookRecipe::dummy(&name("y"));
            cook.recipe = recipe.clone();
            cook.apply_rule(rule);
            cook
        };

        assert_eq!(cook(RecipeRule::Source).recipe, recipe);
        let local = cook(RecipeRule::Local);
        assert_eq!(local.recipe.source, None);
        assert_eq!(local.recipe.build, recipe.build);
        let binary = cook(RecipeRule::Binary);
        assert_eq!(binary.recipe.build.kind, BuildKind::Remote);
        assert_eq!(binary.recipe.build.dependencies, vec![name("zlib")]);
        let ignore = cook(RecipeRule::Ignore);
        assert_eq!(ignore.recipe.build, BuildRecipe::default());
        assert_eq!(ignore.rule, Some(RecipeRule::Ignore));
    }

    #[test]
    fn host_recipes_depend_on_host_packages() {
        let recipe = parse(
            "[build]\ntemplate = \"custom\"\nscript = \"\"\n\
             dependencies = [\"zlib\", \"host:tool\", \"host:self\", \"target:libc\"]\n\
             [package]\ndependencies = [\"runtime\"]\n",
        )
        .unwrap();
        let host = CookRecipe::new(name("host:self"), PathBuf::new(), recipe);
        assert_eq!(
            host.recipe.build.dependencies,
            vec![name("host:zlib"), name("host:tool")]
        );
        assert!(host.recipe.package.dependencies.is_empty());
    }

    type Names = &'static [&'static str];

    /// Build a loader over an in-memory graph: `(name, build, dev, package)`.
    fn graph(
        nodes: &'static [(&'static str, Names, Names, Names)],
    ) -> impl Fn(PackageName) -> Result<CookRecipe, PackageError> {
        move |wanted: PackageName| {
            let (_, build, dev, package) = nodes
                .iter()
                .find(|node| node.0 == wanted.as_str())
                .ok_or_else(|| PackageError::PackageNotFound(wanted.clone()))?;
            let mut recipe = CookRecipe::dummy(&wanted);
            recipe.recipe.build.dependencies = build.iter().map(|d| name(d)).collect();
            recipe.recipe.build.dev_dependencies = dev.iter().map(|d| name(d)).collect();
            recipe.recipe.package.dependencies = package.iter().map(|d| name(d)).collect();
            Ok(recipe)
        }
    }

    fn names(recipes: Vec<CookRecipe>) -> Vec<String> {
        recipes
            .into_iter()
            .map(|recipe| recipe.name.to_string())
            .collect()
    }

    const BUILD_DEPS: DepWalk = DepWalk {
        follow_dev: false,
        follow_package: false,
        collect_build: true,
        collect_package: false,
        collect_roots: true,
    };
    const PACKAGE_DEPS: DepWalk = DepWalk {
        follow_dev: false,
        follow_package: true,
        collect_build: false,
        collect_package: true,
        collect_roots: false,
    };

    #[test]
    fn build_deps_come_before_dependents_once() {
        let load = graph(&[
            ("app", &["libb", "liba"], &["devtool"], &["runtime"]),
            ("liba", &["libc"], &[], &[]),
            ("libb", &["libc", "liba"], &[], &[]),
            ("libc", &[], &[], &[]),
            ("devtool", &[], &[], &[]),
            ("runtime", &[], &[], &[]),
        ]);
        let order = names(BUILD_DEPS.resolve(&[name("app")], &load).unwrap());
        assert_eq!(order, ["libc", "liba", "libb", "app"]);

        let with_dev = DepWalk {
            follow_dev: true,
            ..BUILD_DEPS
        };
        let order = names(with_dev.resolve(&[name("app")], &load).unwrap());
        assert_eq!(order, ["libc", "liba", "libb", "devtool", "app"]);
    }

    #[test]
    fn package_deps_are_found_through_build_deps() {
        let load = graph(&[
            ("app", &["lib"], &[], &["shell"]),
            ("lib", &[], &[], &["data"]),
            ("shell", &[], &[], &[]),
            ("data", &[], &[], &[]),
        ]);
        let order = names(PACKAGE_DEPS.resolve(&[name("app")], &load).unwrap());
        // `lib` is linked in, not installed; its data files are.
        assert_eq!(order, ["data", "shell"]);

        let with_roots = DepWalk {
            collect_roots: true,
            ..PACKAGE_DEPS
        };
        let order = names(with_roots.resolve(&[name("app")], &load).unwrap());
        assert_eq!(order, ["data", "shell", "app"]);
    }

    #[test]
    fn a_recipe_reached_both_ways_is_collected() {
        let load = graph(&[("app", &["lib"], &[], &["lib"]), ("lib", &[], &[], &[])]);
        let order = names(PACKAGE_DEPS.resolve(&[name("app")], &load).unwrap());
        assert_eq!(order, ["lib"]);
    }

    #[test]
    fn cycles_are_reported_with_their_chain() {
        let load = graph(&[
            ("a", &["b"], &[], &[]),
            ("b", &["c"], &[], &[]),
            ("c", &["a"], &[], &[]),
        ]);
        match BUILD_DEPS.resolve(&[name("a")], &load) {
            Err(PackageError::Recursion(chain)) => {
                let chain: Vec<_> = chain.iter().map(|n| n.as_str()).collect();
                assert_eq!(chain, ["a", "b", "c", "a"]);
            }
            other => panic!("expected a recursion error, got {other:?}"),
        }
    }

    #[test]
    fn deep_and_wide_graphs_resolve() {
        // A chain deeper than the old depth limit of 16.
        static CHAIN: std::sync::LazyLock<Vec<(String, Vec<String>)>> =
            std::sync::LazyLock::new(|| {
                (0..40)
                    .map(|i| (format!("p{i}"), vec![format!("p{}", i + 1)]))
                    .chain(std::iter::once(("p40".to_string(), vec![])))
                    .collect()
            });
        let load = |wanted: PackageName| {
            let (_, deps) = CHAIN
                .iter()
                .find(|(n, _)| n == wanted.as_str())
                .ok_or_else(|| PackageError::PackageNotFound(wanted.clone()))?;
            let mut recipe = CookRecipe::dummy(&wanted);
            recipe.recipe.build.dependencies = deps.iter().map(|d| name(d)).collect();
            Ok(recipe)
        };
        let order = names(BUILD_DEPS.resolve(&[name("p0")], &load).unwrap());
        assert_eq!(order.len(), 41);
        assert_eq!(order.first().unwrap(), "p40");
        assert_eq!(order.last().unwrap(), "p0");
    }

    #[test]
    fn missing_dependency_is_an_error() {
        let load = graph(&[("app", &["ghost"], &[], &[])]);
        assert!(matches!(
            BUILD_DEPS.resolve(&[name("app")], &load),
            Err(PackageError::PackageNotFound(_))
        ));
    }

    #[test]
    fn flatten_merges_optional_packages() {
        let recipes = ["gcc13", "gcc13.cxx", "host:gcc13.cxx", "zlib"]
            .iter()
            .map(|n| CookRecipe::dummy(&name(n)))
            .collect();
        let flat = names(recipes_flatten_package_names(recipes));
        assert_eq!(flat, ["gcc13", "host:gcc13", "zlib"]);
    }

    #[test]
    fn versions_are_extracted() {
        assert_eq!(
            extract_version("libogg-1.3.3.tar.xz").as_deref(),
            Some("1.3.3")
        );
        assert_eq!(extract_version("release-1.16").as_deref(), Some("1.16"));
        assert_eq!(
            extract_version("06344744d3d55a5ac9a62a6059cb363d40699bbc"),
            None
        );

        let mut cook = CookRecipe::dummy(&name("x"));
        assert_eq!(cook.guess_version().as_deref(), Some(""));
        cook.recipe.build = BuildRecipe::new(BuildKind::Custom { script: "".into() });
        assert_eq!(cook.guess_version(), None);
        cook.recipe.source = Some(SourceRecipe::Tar {
            tar: "https://x/y-2.4.tar.gz".into(),
            blake3: None,
            patches: vec![],
            script: None,
        });
        assert_eq!(cook.guess_version().as_deref(), Some("2.4"));
        cook.recipe.package.version = Some("9".into());
        assert_eq!(cook.guess_version().as_deref(), Some("9"));
    }

    fn built(text: &str) -> CookRecipe {
        let mut recipe = CookRecipe::dummy(&name(text));
        recipe.recipe.build = BuildRecipe::new(BuildKind::Custom {
            script: "make".to_string(),
        });
        recipe
    }

    fn ignored(text: &str) -> CookRecipe {
        let mut recipe = CookRecipe::dummy(&name(text));
        recipe.rule = Some(RecipeRule::Ignore);
        recipe
    }

    #[test]
    fn ignore_required_by_build_dep_is_rejected() {
        let mut dependent = built("libvorbis");
        dependent.recipe.build.dependencies = vec![name("libogg"), name("libfoo")];
        let err = validate_filesystem_rules(&[ignored("libogg"), ignored("libfoo"), dependent])
            .unwrap_err()
            .to_string();
        for expected in ["libogg", "libfoo", "libvorbis"] {
            assert!(err.contains(expected), "unexpected error: {err}");
        }
    }

    #[test]
    fn ignore_required_by_dev_dep_is_rejected() {
        let mut dependent = built("libvorbis");
        dependent.recipe.build.dev_dependencies = vec![name("libogg")];
        let err = validate_filesystem_rules(&[ignored("libogg"), dependent]).unwrap_err();
        assert!(err.to_string().contains("libogg"));
    }

    #[test]
    fn ignore_leaf_is_allowed() {
        // e.g. pruning a package from a meta package: nothing builds against it.
        let mut meta = CookRecipe::dummy(&name("server"));
        meta.recipe.package.dependencies = vec![name("bash")];
        assert!(validate_filesystem_rules(&[ignored("bash"), meta]).is_ok());
        assert!(validate_filesystem_rules(&[]).is_ok());
    }
}
