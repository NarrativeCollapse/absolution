//! Checks over every recipe and image configuration in the tree, so that a
//! typo in one of them fails CI instead of a build many hours in.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use cookbook::recipe::Recipe;

fn tree_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every `recipe.toml` under `recipes/`, keyed by recipe name.
fn recipe_files() -> BTreeMap<String, PathBuf> {
    let mut files = BTreeMap::new();
    let mut duplicates = Vec::new();
    for entry in walkdir::WalkDir::new(tree_root().join("recipes"))
        .into_iter()
        // Fetched sources and build output are not part of the tree.
        .filter_entry(|entry| entry.file_name() != "source" && entry.file_name() != "target")
    {
        let entry = entry.unwrap();
        if entry.file_name() != "recipe.toml" {
            continue;
        }
        let dir = entry.path().parent().unwrap();
        let name = dir.file_name().unwrap().to_str().unwrap().to_string();
        if let Some(other) = files.insert(name.clone(), entry.path().to_path_buf()) {
            duplicates.push(format!("{name}: {} and {}", other.display(), dir.display()));
        }
    }
    assert!(
        duplicates.is_empty(),
        "duplicate recipe names:\n{}",
        duplicates.join("\n")
    );
    files
}

fn is_wip(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == "wip")
}

/// The recipe a dependency such as `host:gcc13.cxx` refers to.
fn recipe_of(dependency: &str) -> &str {
    let unprefixed = dependency.rsplit(':').next().unwrap();
    unprefixed.split('.').next().unwrap()
}

/// Whether a recipe is a placeholder that says what to package but not yet
/// how: a `custom` build without a script, or a template that does not exist
/// yet. Only work-in-progress recipes may be placeholders.
fn is_placeholder(path: &Path) -> bool {
    const TEMPLATES: [&str; 8] = [
        "none",
        "remote",
        "cargo",
        "configure",
        "cmake",
        "meson",
        "python",
        "custom",
    ];
    let Ok(recipe) = std::fs::read_to_string(path)
        .unwrap()
        .parse::<toml::Table>()
    else {
        return false;
    };
    let Some(build) = recipe.get("build").and_then(|build| build.as_table()) else {
        return false;
    };
    match build.get("template").and_then(|template| template.as_str()) {
        Some("custom") => !build.contains_key("script"),
        Some(template) => !TEMPLATES.contains(&template),
        None => false,
    }
}

#[test]
fn every_recipe_parses() {
    let failures: Vec<String> = recipe_files()
        .values()
        .filter(|path| !(is_wip(path) && is_placeholder(path)))
        .filter_map(|path| Recipe::load(path).err().map(|e| e.to_string()))
        .collect();
    assert!(
        failures.is_empty(),
        "{} recipes do not parse:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn dependencies_of_supported_recipes_exist() {
    let files = recipe_files();
    let mut missing = Vec::new();
    for (name, path) in files.iter().filter(|(_, path)| !is_wip(path)) {
        let Ok(recipe) = Recipe::load(path) else {
            continue; // reported by every_recipe_parses
        };
        let optional = recipe
            .optional_packages
            .iter()
            .flat_map(|p| &p.dependencies);
        let dependencies = recipe
            .build
            .dependencies
            .iter()
            .chain(&recipe.build.dev_dependencies)
            .chain(&recipe.package.dependencies)
            .chain(optional);
        for dependency in dependencies {
            let target = recipe_of(dependency.as_str());
            // An empty name refers to the recipe's own optional package.
            if !target.is_empty() && !files.contains_key(target) {
                missing.push(format!("{name} depends on missing recipe {target}"));
            }
        }
    }
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}

/// Supported recipes whose archive is downloaded without a hash to check
/// it against. Do not add to this list; add `blake3` to the recipe instead,
/// and remove a recipe from here once it has one.
const KNOWN_UNVERIFIED_TARS: [&str; 7] = [
    "iperf3", "jq", "libedit", "libtiff", "liburcu", "mpc", "nginx",
];

#[test]
fn supported_tar_sources_are_verified() {
    use cookbook::recipe::SourceRecipe;
    let unverified: Vec<String> = recipe_files()
        .into_iter()
        .filter(|(_, path)| !is_wip(path))
        .filter(|(_, path)| {
            matches!(
                Recipe::load(path).ok().and_then(|recipe| recipe.source),
                Some(SourceRecipe::Tar { blake3: None, .. })
            )
        })
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        unverified, KNOWN_UNVERIFIED_TARS,
        "the set of tar sources without a blake3 hash changed"
    );
}

/// The `[packages]` keys of an image configuration, following `include`.
fn config_packages(path: &Path, seen: &mut BTreeSet<PathBuf>) -> BTreeSet<String> {
    let mut packages = BTreeSet::new();
    if !seen.insert(path.to_path_buf()) {
        return packages;
    }
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let config: toml::Table = text
        .parse()
        .unwrap_or_else(|e| panic!("cannot parse {}: {e}", path.display()));
    for include in config
        .get("include")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
    {
        let included = path.parent().unwrap().join(include.as_str().unwrap());
        packages.extend(config_packages(&included, seen));
    }
    if let Some(table) = config.get("packages").and_then(|value| value.as_table()) {
        packages.extend(table.keys().cloned());
    }
    packages
}

#[test]
fn image_configs_only_name_existing_recipes() {
    let files = recipe_files();
    let mut missing = Vec::new();
    for entry in walkdir::WalkDir::new(tree_root().join("config")) {
        let entry = entry.unwrap();
        if entry.path().extension().is_none_or(|ext| ext != "toml") {
            continue;
        }
        for package in config_packages(entry.path(), &mut BTreeSet::new()) {
            if !files.contains_key(recipe_of(&package)) {
                missing.push(format!("{}: {package}", entry.path().display()));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "configs name missing recipes:\n{}",
        missing.join("\n")
    );
}

/// Dependencies of every kind, as recipe names.
fn recipe_dependencies(recipe: &Recipe) -> Vec<String> {
    let optional = recipe
        .optional_packages
        .iter()
        .flat_map(|p| &p.dependencies);
    recipe
        .build
        .dependencies
        .iter()
        .chain(&recipe.build.dev_dependencies)
        .chain(&recipe.package.dependencies)
        .chain(optional)
        .map(|dependency| recipe_of(dependency.as_str()).to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

#[test]
fn supported_recipes_depend_only_on_supported_recipes() {
    let files = recipe_files();
    let mut problems = Vec::new();
    for (name, path) in files.iter().filter(|(_, path)| !is_wip(path)) {
        let Ok(recipe) = Recipe::load(path) else {
            continue; // reported by every_recipe_parses
        };
        for dependency in recipe_dependencies(&recipe) {
            if files.get(&dependency).is_some_and(|path| is_wip(path)) {
                problems.push(format!(
                    "{name} depends on work-in-progress recipe {dependency}; \
                     move {dependency} out of recipes/wip once it builds"
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn image_configs_only_name_supported_recipes() {
    let files = recipe_files();
    let mut problems = Vec::new();
    for entry in walkdir::WalkDir::new(tree_root().join("config")) {
        let entry = entry.unwrap();
        if entry.path().extension().is_none_or(|ext| ext != "toml") {
            continue;
        }
        let relative = entry.path().strip_prefix(tree_root()).unwrap();
        // The CI configs, and the "full" image built from them, list
        // experimental packages on purpose, to try building them.
        if relative
            .file_name()
            .is_some_and(|name| name == "ci.toml" || name == "full.toml")
        {
            continue;
        }
        for package in config_packages(entry.path(), &mut BTreeSet::new()) {
            let recipe = recipe_of(&package);
            if files.get(recipe).is_some_and(|path| is_wip(path)) {
                problems.push(format!(
                    "{}: {package} is a work-in-progress recipe",
                    relative.display()
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The `[packages]` keys a config lists itself, ignoring its includes.
fn own_config_packages(path: &Path) -> BTreeSet<String> {
    let config: toml::Table = std::fs::read_to_string(path).unwrap().parse().unwrap();
    config
        .get("packages")
        .and_then(|value| value.as_table())
        .map(|table| table.keys().cloned().collect())
        .unwrap_or_default()
}

#[test]
fn server_group_matches_server_image() {
    let root = tree_root();
    let mut expected = own_config_packages(&root.join("config/minimal.toml"));
    expected.extend(own_config_packages(&root.join("config/server.toml")));
    let group = Recipe::load(&root.join("recipes/groups/server/recipe.toml")).unwrap();
    let actual: BTreeSet<String> = group
        .package
        .dependencies
        .iter()
        .map(|name| name.as_str().to_string())
        .collect();
    assert_eq!(
        actual, expected,
        "recipes/groups/server must list the packages of config/minimal.toml and config/server.toml"
    );
}

#[test]
fn supported_recipe_descriptions_use_this_os_name() {
    let offenders: Vec<String> = recipe_files()
        .into_iter()
        .filter(|(_, path)| !is_wip(path))
        .filter_map(|(name, path)| {
            let description = Recipe::load(&path).ok()?.package.description?;
            // RedoxFS and relibc are component names, not the OS name.
            let os_name = description.replace("RedoxFS", "");
            os_name
                .contains("Redox")
                .then(|| format!("{name}: {description}"))
        })
        .collect();
    assert!(offenders.is_empty(), "{}", offenders.join("\n"));
}

#[test]
fn installed_system_identifies_as_absolution() {
    let base = std::fs::read_to_string(tree_root().join("config/base.toml")).unwrap();
    assert!(base.contains("NAME=\"Absolution\""));
    assert!(base.contains("ID=\"absolution\""));
    // Installed systems must not fetch packages from another OS's server.
    assert!(
        !base.contains("/etc/pkg.d/"),
        "base.toml configures a package server"
    );
    assert!(!base.to_lowercase().contains("redox-os.org"));
}
