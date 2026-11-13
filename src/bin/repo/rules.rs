//! Working out which recipes a command applies to and, for each of them,
//! whether it is built from source, downloaded, kept as is or ignored.

use std::collections::{BTreeMap, HashMap, HashSet};

use cookbook::config::{RecipeLock, get_config};
use cookbook::cook::fetch_repo;
use cookbook::recipe::{
    CookRecipe, RecipeRule, SourceRecipe, recipes_flatten_package_names, recipes_mark_as_deps,
    validate_filesystem_rules,
};
use cookbook::{Result, staged_pkg};
use pkg::PackageName;
use redox_installer::PackageConfig;

use crate::cli::{AllOption, Cli, CliCommand, CliConfig, bail_options_err};

/// The rules given by `cookbook.lock` and the image configuration.
struct RuleSet {
    /// Rules set explicitly for a recipe, keyed by name without prefix
    explicit: HashMap<PackageName, RecipeRule>,
    /// The rule for recipes nothing else decides
    default_rule: RecipeRule,
    /// Everything the image builds from source, with all dependencies
    source_names: HashSet<PackageName>,
    /// Everything the image downloads, with all dependencies
    binary_names: HashSet<PackageName>,
}

impl RuleSet {
    fn new(
        lock: &BTreeMap<String, RecipeLock>,
        filesystem: Option<&redox_installer::Config>,
        default_rule: RecipeRule,
    ) -> Result<Self> {
        // The lock wins over the image configuration.
        let mut explicit = HashMap::new();
        for (name, recipe_lock) in lock {
            let (Ok(name), Some(rule)) = (PackageName::new(name), &recipe_lock.fsrule) else {
                continue;
            };
            explicit.insert(name, rule.parse::<RecipeRule>()?);
        }

        let mut source_roots = Vec::new();
        let mut binary_roots = Vec::new();
        for (name, package_config) in filesystem.iter().flat_map(|conf| &conf.packages) {
            let Ok(name) = PackageName::new(name) else {
                continue;
            };
            let rule = match (explicit.get(&name), package_config) {
                (Some(rule), _) => *rule,
                (None, PackageConfig::Build(rule)) => {
                    let rule: RecipeRule = rule.parse()?;
                    explicit.insert(name.clone(), rule);
                    rule
                }
                (None, _) => default_rule,
            };
            match rule {
                RecipeRule::Source | RecipeRule::Local => source_roots.push(name),
                RecipeRule::Binary => binary_roots.push(name),
                RecipeRule::Ignore => {}
            }
        }

        let source_names = CookRecipe::get_all_deps_names_recursive(&source_roots, true)?;
        let binary_names = CookRecipe::get_all_deps_names_recursive(&binary_roots, false)?;
        Ok(RuleSet {
            explicit,
            default_rule,
            source_names: source_names.into_iter().collect(),
            binary_names: binary_names.into_iter().collect(),
        })
    }

    /// The rule for `name`, whose source belongs to recipe `canon`.
    ///
    /// Returns `None` for a host recipe that would be downloaded: there are
    /// no pre-built host packages, so such a recipe keeps what it had.
    fn rule_for(&self, name: &PackageName, canon: &str) -> Option<RecipeRule> {
        let rule = match self.explicit.get(canon) {
            Some(rule) => *rule,
            None => match (
                self.source_names.contains(name),
                self.binary_names.contains(name),
            ) {
                // Needed both ways: go against the default so that the
                // explicitly configured side wins.
                (true, true) => self.default_rule.inverted_default(),
                (true, false) => RecipeRule::Source,
                (false, true) => RecipeRule::Binary,
                (false, false) => self.default_rule,
            },
        };
        (!(name.is_host() && rule == RecipeRule::Binary)).then_some(rule)
    }

    /// The rule each of `names` starts with, before dependencies are known.
    fn initial_rule(&self, name: &PackageName) -> RecipeRule {
        if let Some(rule) = self.explicit.get(name) {
            *rule
        } else if self.source_names.contains(name) {
            RecipeRule::Source
        } else if self.binary_names.contains(name) {
            RecipeRule::Binary
        } else {
            self.default_rule
        }
    }
}

/// Whether the remote repository has a package for `name`.
fn has_binary_package(name: &PackageName) -> Result<bool> {
    fetch_repo::has_binary_package(name.as_str())
}

fn note_no_binary(config: &CliConfig, name: &PackageName) {
    if config.cook.verbose {
        println!("DEBUG: Recipe {:?} has no binary package", name.as_str());
    }
}

/// Decide the rule of each requested recipe.
fn rules_by_name(
    rules: &RuleSet,
    names: Vec<PackageName>,
    config: &CliConfig,
) -> Result<BTreeMap<PackageName, RecipeRule>> {
    let mut by_name = BTreeMap::new();
    for name in names {
        let mut rule = rules
            .rule_for(&name, name.without_prefix())
            .unwrap_or_else(|| rules.initial_rule(&name));
        if rule == RecipeRule::Binary && !has_binary_package(&name)? {
            note_no_binary(config, &name);
            rule = RecipeRule::Source;
        }
        by_name.insert(name, rule);
    }
    Ok(by_name)
}

/// Apply rules to recipes, including the dependencies pulled in since
/// [`rules_by_name`] ran.
fn apply_rules(rules: &RuleSet, recipes: &mut [CookRecipe], config: &CliConfig) -> Result<()> {
    for recipe in recipes.iter_mut().filter(|recipe| recipe.rule.is_none()) {
        let canon = recipe.canon_recipe_name();
        if let Some(rule) = rules.rule_for(&recipe.name, canon.without_prefix()) {
            recipe.apply_rule(rule);
        }
    }

    // A recipe cannot be downloaded if the remote does not have it.
    for recipe in recipes.iter_mut() {
        if recipe.rule == Some(RecipeRule::Binary) && !has_binary_package(&recipe.name)? {
            note_no_binary(config, &recipe.name);
            recipe.rule = Some(RecipeRule::Source);
            recipe.reload_recipe()?;
        }
    }
    Ok(())
}

/// Expand `--all`, `--all-*` and `--category` into recipes.
fn recipes_from_selection(
    config: &CliConfig,
    command: CliCommand,
) -> Result<BTreeMap<PackageName, CookRecipe>> {
    let paths = match (config.all, config.category.is_some()) {
        (Some(AllOption::All), _) | (None, true) => staged_pkg::list(),
        // Compiled recipes are the ones listed in the built repository.
        _ => staged_pkg::list_repo(&config.repo_dir)?,
    };
    let paths: Vec<_> = match &config.category {
        Some(category) => paths
            .into_iter()
            .filter(|path| path.starts_with(category))
            .collect(),
        None => paths.into_iter().collect(),
    };
    if paths.is_empty() {
        bail_options_err!(
            "No recipes found from the combination.\nTry pass both --all and --category=name"
        );
    }

    let mut recipes = BTreeMap::new();
    for path in paths {
        match CookRecipe::from_path(&path, !command.is_cleaning(), false) {
            Ok(recipe) => {
                recipes.insert(recipe.name.clone(), recipe);
            }
            // With --all, a broken recipe is skipped rather than fatal.
            Err(_) if config.all == Some(AllOption::All) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(recipes)
}

/// The recipes `cli` applies to, in the order they have to be processed,
/// with rules and pinned revisions applied.
pub fn select_recipes(cli: &Cli) -> Result<Vec<CookRecipe>> {
    let Cli {
        config, command, ..
    } = cli;
    let command = *command;
    let mut recipe_names = cli.recipe_names.clone();
    let mut selected = BTreeMap::new();

    if config.all.is_some() || config.category.is_some() {
        selected = recipes_from_selection(config, command)?;
        recipe_names.extend(selected.keys().cloned());
    } else if recipe_names.is_empty() {
        let Some(filesystem) = &config.filesystem else {
            bail_options_err!(
                "Error: No recipe names or filesystem config provided and --all flag was not used."
            );
        };
        recipe_names = filesystem
            .packages
            .keys()
            .filter_map(|name| PackageName::new(name.as_str()).ok())
            .collect();
    }

    if command.is_cleaning() {
        // Cleaning needs neither dependencies nor rules.
        return Ok(if selected.is_empty() {
            CookRecipe::from_list(recipe_names)?
        } else {
            selected.into_values().collect()
        });
    }

    let lock = &get_config().recipe_lock;
    let rules = RuleSet::new(
        lock,
        config.filesystem.as_ref(),
        RecipeRule::from_repo_binary(config.repo_binary),
    )?;
    let with_deps = command.is_building() || command.is_pushing();

    // Installing a package means installing its runtime dependencies too.
    let wanted = if with_deps {
        CookRecipe::get_package_deps_recursive(&recipe_names, true)?
    } else {
        recipe_names.clone()
    };
    let by_name = rules_by_name(&rules, wanted, config)?;
    let names_with = |wanted: &[RecipeRule]| -> Vec<PackageName> {
        by_name
            .iter()
            .filter(|(_, rule)| wanted.contains(rule))
            .map(|(name, _)| name.clone())
            .collect()
    };
    let source_names = names_with(&[RecipeRule::Source, RecipeRule::Local]);
    let binary_names = names_with(&[RecipeRule::Binary]);
    let ignore_names = names_with(&[RecipeRule::Ignore]);

    // Recipes built from source need their build dependencies (and, when
    // building, their dev dependencies); downloaded ones only what they link.
    let mut recipes = if config.all == Some(AllOption::AllBinaries) {
        Vec::new()
    } else if with_deps {
        CookRecipe::get_build_deps_recursive(&source_names, command.is_building())?
    } else {
        CookRecipe::from_list(source_names)?
    };
    recipes.extend(if with_deps {
        CookRecipe::get_build_deps_recursive(&binary_names, false)?
    } else {
        CookRecipe::from_list(binary_names)?
    });
    recipes.extend(CookRecipe::from_list(ignore_names)?);
    let mut recipes = recipes_flatten_package_names(recipes);

    apply_rules(&rules, &mut recipes, config)?;
    validate_filesystem_rules(&recipes)?;
    pin_revisions(lock, &mut recipes);

    // Without --with-package-deps only the named recipes are checked for
    // source updates; listings never update anything.
    if !config.with_package_deps || command.is_informational() {
        recipes_mark_as_deps(&recipe_names, &mut recipes);
    }
    Ok(recipes)
}

/// Replace the revision of git recipes with the one pinned in the lock.
fn pin_revisions(lock: &BTreeMap<String, RecipeLock>, recipes: &mut [CookRecipe]) {
    for recipe in recipes {
        let Some(gitrev) = lock
            .get(recipe.name.without_prefix())
            .and_then(|recipe_lock| recipe_lock.gitrev.clone())
        else {
            continue;
        };
        match &mut recipe.recipe.source {
            Some(SourceRecipe::Git { rev, branch, .. }) => {
                *rev = Some(gitrev);
                *branch = None;
            }
            _ => println!(
                "DEBUG: Recipe {:?} has a pinned git revision but its source is not git",
                recipe.name.as_str()
            ),
        }
        recipe.pinned = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(text: &str) -> PackageName {
        PackageName::new(text).unwrap()
    }

    fn rules(default_rule: RecipeRule) -> RuleSet {
        RuleSet {
            explicit: HashMap::from([
                (name("pinned"), RecipeRule::Local),
                (name("prebuilt"), RecipeRule::Binary),
            ]),
            default_rule,
            source_names: HashSet::from([name("src"), name("both"), name("host:src")]),
            binary_names: HashSet::from([name("bin"), name("both"), name("host:bin")]),
        }
    }

    #[test]
    fn explicit_rules_win() {
        let rules = rules(RecipeRule::Source);
        assert_eq!(
            rules.rule_for(&name("pinned"), "pinned"),
            Some(RecipeRule::Local)
        );
        // A recipe sharing another recipe's source follows that recipe.
        assert_eq!(
            rules.rule_for(&name("alias"), "pinned"),
            Some(RecipeRule::Local)
        );
    }

    #[test]
    fn membership_decides_otherwise() {
        let rules = rules(RecipeRule::Source);
        assert_eq!(
            rules.rule_for(&name("src"), "src"),
            Some(RecipeRule::Source)
        );
        assert_eq!(
            rules.rule_for(&name("bin"), "bin"),
            Some(RecipeRule::Binary)
        );
        assert_eq!(
            rules.rule_for(&name("other"), "other"),
            Some(RecipeRule::Source)
        );
    }

    #[test]
    fn needed_both_ways_goes_against_the_default() {
        assert_eq!(
            rules(RecipeRule::Source).rule_for(&name("both"), "both"),
            Some(RecipeRule::Binary)
        );
        assert_eq!(
            rules(RecipeRule::Binary).rule_for(&name("both"), "both"),
            Some(RecipeRule::Source)
        );
    }

    #[test]
    fn host_recipes_are_never_downloaded() {
        let rules = rules(RecipeRule::Binary);
        assert_eq!(rules.rule_for(&name("host:bin"), "bin"), None);
        assert_eq!(rules.rule_for(&name("host:other"), "other"), None);
        assert_eq!(rules.rule_for(&name("host:prebuilt"), "prebuilt"), None);
        assert_eq!(
            rules.rule_for(&name("host:src"), "src"),
            Some(RecipeRule::Source)
        );
    }

    #[test]
    fn lock_rules_are_validated() {
        let lock = BTreeMap::from([(
            "bash".to_string(),
            RecipeLock {
                fsrule: Some("bogus".into()),
                gitrev: None,
            },
        )]);
        assert!(RuleSet::new(&lock, None, RecipeRule::Source).is_err());

        let lock = BTreeMap::from([(
            "bash".to_string(),
            RecipeLock {
                fsrule: Some("ignore".into()),
                gitrev: None,
            },
        )]);
        let rules = RuleSet::new(&lock, None, RecipeRule::Source).unwrap();
        assert_eq!(rules.initial_rule(&name("bash")), RecipeRule::Ignore);
        assert_eq!(rules.initial_rule(&name("zsh")), RecipeRule::Source);
    }

    #[test]
    fn pinning_replaces_revision_and_branch() {
        let mut recipe = CookRecipe::dummy(&name("host:bash"));
        recipe.recipe.source = Some(SourceRecipe::Git {
            git: "https://x/bash".into(),
            upstream: None,
            branch: Some("main".into()),
            rev: None,
            shallow_clone: None,
            patches: vec![],
            script: None,
        });
        let mut unpinned = CookRecipe::dummy(&name("zsh"));
        unpinned.recipe.source = recipe.recipe.source.clone();
        let lock = BTreeMap::from([(
            "bash".to_string(),
            RecipeLock {
                fsrule: None,
                gitrev: Some("abc123".into()),
            },
        )]);

        let mut recipes = [recipe, unpinned.clone()];
        pin_revisions(&lock, &mut recipes);
        assert!(recipes[0].pinned);
        assert!(matches!(
            &recipes[0].recipe.source,
            Some(SourceRecipe::Git { rev: Some(rev), branch: None, .. }) if rev == "abc123"
        ));
        assert_eq!(recipes[1], unpinned);
    }
}
