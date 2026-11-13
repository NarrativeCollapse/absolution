//! The work behind each `repo` command.

use std::collections::HashMap;
use std::io::{Write, stderr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::{env, fs, thread};

use cookbook::config::{CookLock, get_config};
use cookbook::cook::cook_build::{build, get_stage_dirs, remove_stage_dir};
use cookbook::cook::fetch::{FetchResult, fetch, fetch_offline};
use cookbook::cook::fs::{create_target_dir, remove_all, run_command};
use cookbook::cook::git;
use cookbook::cook::package::{package, package_handle_push};
use cookbook::cook::pty::{PtyOut, flush_pty, write_to_pty};
use cookbook::cook::tree::{self, DisplayOptions, TreeData, TreeItem, TreeOptions, WalkTreeEntry};
use cookbook::cook::tui::join_logs;
use cookbook::recipe::{CookRecipe, RecipeRule, SourceRecipe};
use cookbook::{Error, Result};
use pkg::{PackageName, PackageState};
use termion::{color, style};

use crate::app::{StatusUpdate, TuiApp, setup_logger};
use crate::cli::{CliCommand, CliConfig};

/// How a command went for one recipe.
#[derive(Clone, Copy)]
pub enum Outcome {
    Failed,
    Successful,
    Cached,
}

/// Print the one-line, colored result of `command` for `recipe`.
pub fn print_outcome(command: CliCommand, recipe: &PackageName, outcome: Outcome) {
    let (color, text) = match outcome {
        Outcome::Failed => (196, "failed "),
        Outcome::Successful => (46, "successful"),
        Outcome::Cached => (45, "cached"),
    };
    eprintln!(
        "{}{}{command} {} - {text}{}{}",
        style::Bold,
        color::Fg(color::AnsiValue(color)),
        recipe.as_str(),
        color::Fg(color::Reset),
        style::Reset,
    );
}

pub fn print_cached_or_successful(command: CliCommand, recipe: &PackageName, cached: bool) {
    let outcome = if cached {
        Outcome::Cached
    } else {
        Outcome::Successful
    };
    print_outcome(command, recipe, outcome);
}

/// Write a failure's log text to stderr, followed by a blank line.
pub fn print_failure_log(log: &str) {
    let _ = stderr().write_all(log.as_bytes());
    let _ = stderr().write_all(b"\n\n");
}

/// Run `fetch` or `cook` for one recipe without the TUI, capturing its
/// output into a log file when logging is enabled. Returns whether the
/// result was cached.
pub fn fetch_or_cook(config: &CliConfig, command: CliCommand, recipe: &CookRecipe) -> Result<bool> {
    let run = |logger: &PtyOut| -> Result<bool> {
        let is_cook = command == CliCommand::Cook;
        let fetched = handle_fetch(recipe, config, is_cook, logger)?;
        if is_cook {
            handle_cook(recipe, config, fetched.source_dir, logger)
        } else {
            Ok(fetched.cached)
        }
    };
    let Some(logs_dir) = &config.logs_dir else {
        return run(&None);
    };

    // The log collector reuses the TUI's bookkeeping without drawing it.
    let (status_tx, status_rx) = mpsc::channel::<StatusUpdate>();
    let (mut stdout_writer, mut stderr_writer) = setup_logger(&status_tx, &recipe.name);
    let mut app = TuiApp::new(vec![recipe.clone()]);
    app.dump_logs_anyway = config.cook.verbose;
    let dump_failure = !app.dump_logs_anyway;
    let collector = thread::spawn(move || {
        while let Ok(update) = status_rx.recv() {
            match &update {
                StatusUpdate::CookThreadFinished => break,
                StatusUpdate::FailCook(failed, _) => {
                    if let (Some(logs), line) = app.get_recipe_log(&failed.name) {
                        println!("{}", join_logs(logs, line));
                    }
                }
                _ => app.update_status(update),
            }
        }
    });

    let mut logger = Some((&mut stdout_writer, &mut stderr_writer));
    let result = run(&logger);
    if let Err(e) = &result {
        write_to_pty(&logger, &format!("\n{e}"));
    }
    // The log of a cached success says nothing worth keeping.
    if !matches!(result, Ok(true)) {
        flush_pty(&mut logger);
        let log_path = logs_dir.join(format!("{}/{}.log", recipe.target, recipe.name.name()));
        // The collector may already be gone; a lost log is not an error.
        let _ = status_tx.send(StatusUpdate::FlushLog(recipe.name.clone(), log_path));
        if dump_failure && result.is_err() {
            let _ = status_tx.send(StatusUpdate::FailCook(recipe.clone(), String::new()));
        }
    }
    let _ = status_tx.send(StatusUpdate::CookThreadFinished);
    let _ = collector.join();
    result
}

/// Hand the built packages to `repo_builder`, which updates the repository.
pub fn publish_packages(recipes: &[CookRecipe], repo_dir: &Path) -> Result<()> {
    let exe = env::current_exe().map_err(|e| Error::from_io_error(e, "Getting exe path"))?;
    let repo_builder = exe.with_file_name("repo_builder");
    let mut command = Command::new(repo_builder);
    command.arg(repo_dir).args(
        recipes
            .iter()
            .filter(|recipe| !recipe.is_deps)
            .map(|recipe| recipe.name.as_str()),
    );
    run_command(command, &None)
}

pub fn handle_fetch(
    recipe: &CookRecipe,
    config: &CliConfig,
    allow_offline: bool,
    logger: &PtyOut,
) -> Result<FetchResult> {
    if config.cook.offline && allow_offline {
        fetch_offline(recipe, logger)
    } else {
        fetch(recipe, !recipe.is_deps, logger)
    }
}

/// Build and package a fetched recipe. Returns whether the build was cached.
pub fn handle_cook(
    recipe: &CookRecipe,
    config: &CliConfig,
    source_dir: PathBuf,
    logger: &PtyOut,
) -> Result<bool> {
    let target_dir = create_target_dir(&recipe.dir, recipe.target)?;
    let build_result = build(
        &recipe.dir,
        &source_dir,
        &target_dir,
        recipe,
        &config.cook,
        logger,
    )?;
    package(recipe, &build_result, &config.cook, logger)?;

    for stage_dir in build_result.stage_dirs.iter().filter(|dir| dir.is_dir()) {
        if config.cook.write_filetree {
            let mut listing = Vec::new();
            tree::walk_file_tree(stage_dir, "", &mut listing)
                .map_err(|e| Error::from_io_error(e, "Walking files tree"))?;
            listing.push(String::new()); // trailing newline
            fs::write(stage_dir.with_added_extension("files"), listing.join("\n"))
                .map_err(|e| Error::from_io_error(e, "Writing files tree"))?;
        }
        if config.cook.clean_target {
            remove_all(stage_dir)?;
        }
    }
    Ok(build_result.cached)
}

/// After a failure in nonstop mode, remove the recipe's stale packages so
/// that `repo_builder` does not publish them.
pub fn handle_nonstop_fail(recipe: &CookRecipe) -> Result<()> {
    let stage_dirs = get_stage_dirs(&recipe.recipe.optional_packages, &recipe.target_dir());
    stage_dirs.iter().try_for_each(|dir| remove_stage_dir(dir))
}

/// Remove build artifacts and, for `unfetch`, sources. Returns true when
/// there was nothing to remove.
pub fn handle_clean(recipe: &CookRecipe, config: &CliConfig, command: CliCommand) -> Result<bool> {
    let unfetch = command == CliCommand::Unfetch;
    let mut removed = false;
    let mut remove = |path: &Path| -> Result<()> {
        if path.exists() || path.is_symlink() {
            remove_all(path)?;
            removed = true;
        }
        Ok(())
    };

    let mut target_dir = recipe.dir.join("target");
    if command == CliCommand::CleanTarget {
        target_dir = target_dir.join(redoxer::target());
    }
    // A downloaded recipe's target dir holds the downloads themselves.
    if !(unfetch && recipe.rule == Some(RecipeRule::Binary)) {
        remove(&target_dir)?;
    }

    if unfetch {
        remove(&recipe.dir.join("source"))?;
        // The archive is kept when it can be verified against the recipe,
        // unless everything is being unfetched.
        let unverifiable = matches!(
            &recipe.recipe.source,
            Some(SourceRecipe::Tar { blake3: None, .. })
        );
        let tar = recipe.dir.join("source.tar");
        if tar.is_file() && (unverifiable || config.all.is_some()) {
            remove(&tar)?;
        }
    }
    Ok(!removed)
}

/// Install built packages, with their runtime dependencies, into the sysroot.
pub fn handle_push(recipes: &[CookRecipe], config: &CliConfig) -> Result<()> {
    if !config.sysroot_dir.is_dir() {
        return Err(Error::Other(format!(
            "{} does not exist. Please run `make mount` first.",
            config.sysroot_dir.display()
        )));
    }
    let recipe_map: HashMap<&PackageName, &CookRecipe> =
        recipes.iter().map(|r| (&r.name, r)).collect();

    let push_one = |item: TreeItem| -> Result<bool> {
        let name = &item.recipe.name;
        if name.is_host() {
            return Ok(true);
        }
        let result = match item.entry {
            WalkTreeEntry::Built(_) => push_package(item.recipe, config),
            WalkTreeEntry::NotBuilt => Err(Error::Other(format!(
                "Package {} has not been built",
                name.name()
            ))),
            WalkTreeEntry::Deduped | WalkTreeEntry::Missing => return Ok(false),
        };
        match result {
            Ok(cached) => {
                print_cached_or_successful(CliCommand::Push, name, cached);
                Ok(cached)
            }
            Err(e) => {
                print_outcome(CliCommand::Push, name, Outcome::Failed);
                if get_config().cook.nonstop {
                    Ok(true)
                } else {
                    Err(e)
                }
            }
        }
    };

    let mut data = TreeData::new();
    for recipe in recipes {
        tree::walk_tree_entry(
            &recipe.name,
            &recipe_map,
            None,
            "",
            false,
            TreeOptions::Push,
            &mut data,
            &push_one,
        )?;
    }

    if config.cook.verbose {
        println!(
            "\nPushed {} of {}",
            tree::format_size(data.total_size),
            count(data.total_count as usize, "package"),
        );
    }
    Ok(())
}

/// Install one package. Returns true if it was already installed.
fn push_package(recipe: &CookRecipe, config: &CliConfig) -> Result<bool> {
    let sysroot = &config.sysroot_dir;
    let archive_path = recipe.stage_paths().1;
    if config.no_metadata {
        return package_handle_push(None, &archive_path, sysroot);
    }
    let mut state = PackageState::from_sysroot(sysroot)?;
    let cached = package_handle_push(Some(&mut state), &archive_path, sysroot)?;
    if !cached {
        state
            .to_sysroot(sysroot)
            .map_err(|e| Error::from_io_error(e, "Writing package state"))?;
    }
    Ok(cached)
}

fn count(number: usize, noun: &str) -> String {
    let plural = if number == 1 { "" } else { "s" };
    format!("{number} {noun}{plural}")
}

/// Print the recipes as a list or dependency tree.
pub fn handle_tree(recipes: &[CookRecipe], options: TreeOptions, config: &CliConfig) -> Result<()> {
    let recipe_map: HashMap<&PackageName, &CookRecipe> =
        recipes.iter().map(|r| (&r.name, r)).collect();
    let roots: Vec<PackageName> = recipes.iter().map(|r| r.name.clone()).collect();
    let data = tree::display_tree_entry(&roots, &recipe_map, options, config.display)?;

    if matches!(config.display, DisplayOptions::Tree) {
        match options {
            TreeOptions::Repo => {}
            TreeOptions::Cook => println!(
                "\nBuild summary: {} need build with total of {}",
                data.total_notbuilt,
                count(data.visited.len(), "recipe"),
            ),
            TreeOptions::Push => println!(
                "\nEstimated image size: {} of {}",
                tree::format_size(data.total_size),
                count(data.visited.len(), "package"),
            ),
        }
    }
    Ok(())
}

/// Update `cookbook.lock`: set or unset recipe rules (`change-rule`) or
/// pinned git revisions (`capture-rev`).
pub fn handle_change_rule(
    recipes: &[CookRecipe],
    config: &CliConfig,
    command: CliCommand,
) -> Result<()> {
    let capture_rev = command == CliCommand::CaptureRev;
    let mut lock = get_config().recipe_lock.clone();
    // Rolling back pins each source to its state when the tree was committed.
    let tree_date = if capture_rev && config.with_rollback {
        Some(git::commit_date(Path::new("."))?)
    } else {
        None
    };

    for recipe in recipes {
        if capture_rev && !matches!(recipe.recipe.source, Some(SourceRecipe::Git { .. })) {
            continue;
        }
        // Host packages are always built from source.
        if !capture_rev && recipe.name.is_host() {
            continue;
        }
        let canon = recipe.canon_recipe_name();
        let lock_key = canon.without_prefix();
        let mut recipe_lock = lock.get(lock_key).cloned().unwrap_or_default();

        let unchanged = if capture_rev {
            if config.unset {
                recipe_lock.gitrev.take().is_none()
            } else {
                match captured_revision(recipe, config, tree_date.as_deref()) {
                    Ok(rev) => recipe_lock.gitrev.replace(rev.clone()) == Some(rev),
                    Err(e) => {
                        eprintln!("Skipping {}: {e}", recipe.name.as_str());
                        continue;
                    }
                }
            }
        } else if config.unset {
            recipe_lock.fsrule.take().is_none()
        } else {
            let Some(rule) = config.set_rule.or(recipe.rule) else {
                eprintln!("Skipping {}: no rule to set", recipe.name.as_str());
                continue;
            };
            let rule = rule.to_string();
            recipe_lock.fsrule.replace(rule.clone()) == Some(rule)
        };

        if recipe_lock.is_empty() {
            lock.remove(lock_key);
        } else {
            lock.insert(lock_key.to_string(), recipe_lock);
        }
        // A recipe whose rule changed must be rebuilt from scratch.
        let nothing_cleaned = if !unchanged && !capture_rev {
            handle_clean(recipe, config, CliCommand::Clean)?
        } else {
            true
        };
        print_cached_or_successful(command, &recipe.name, unchanged && nothing_cleaned);
    }

    CookLock { recipes: lock }.save()
}

/// The revision to pin `recipe` to: its current HEAD or, when rolling back,
/// the last commit made before `tree_date`.
fn captured_revision(
    recipe: &CookRecipe,
    config: &CliConfig,
    tree_date: Option<&str>,
) -> Result<String> {
    let source_dir = recipe.dir.join("source");
    match tree_date {
        Some(date) => {
            // Fetch first: the checkout may track something else by now.
            handle_fetch(recipe, config, false, &None)?;
            git::rev_before_date(&source_dir, date)
        }
        None => {
            if !git::is_checkout(&source_dir) {
                return Err(Error::Other("the source has not been fetched".into()));
            }
            Ok(git::head(&source_dir)?.rev)
        }
    }
}
