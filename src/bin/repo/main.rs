//! `repo`: fetch, build, package and install recipes.

use std::{env, process};

use cookbook::config::{get_config, init_config};
use cookbook::cook::fs::create_dir;
use cookbook::cook::ident;
use cookbook::recipe::CookRecipe;
use cookbook::{Error, Result};

mod app;
mod cli;
mod commands;
mod rules;

use app::{RecipeStatus, TuiApp, run_tui_cook};
use cli::{Cli, CliCommand, CliConfig, REPO_HELP_STR, bail_options_err};
use commands::*;

fn main() {
    if let Err(e) = run() {
        match e {
            Error::Options(message) => eprintln!("{message}\n{REPO_HELP_STR}"),
            e => eprintln!("{e}"),
        }
        process::exit(1);
    }
}

fn run() -> Result<()> {
    init_config()?;
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|arg| arg == "--help" || arg == "-h") {
        bail_options_err!("");
    }

    let current_dir = env::current_dir().map_err(|e| Error::from_io_error(e, "Getting cwd"))?;
    let cli = Cli::parse(args, current_dir, get_config().cook.clone())?;
    if let Some(logs_dir) = &cli.config.logs_dir {
        create_dir(&logs_dir.join(redoxer::target()))?;
        create_dir(&logs_dir.join(redoxer::host_target()))?;
    }
    let recipes = rules::select_recipes(&cli)?;
    let Cli {
        config, command, ..
    } = cli;

    if command.is_building() || command == CliCommand::ChangeRuleLocal {
        ident::init_ident();
    }

    if let Some(tree_options) = command.tree_options() {
        return handle_tree(&recipes, tree_options, &config);
    }
    if command.is_change_rule() {
        return handle_change_rule(&recipes, &config, command);
    }
    match command {
        CliCommand::Push => handle_push(&recipes, &config),
        CliCommand::Cook if config.cook.tui => {
            cook_with_tui(&config, &recipes)?;
            publish_packages(&recipes, &config.repo_dir)
        }
        _ => {
            run_sequentially(&config, command, &recipes)?;
            if command == CliCommand::Cook {
                return publish_packages(&recipes, &config.repo_dir);
            }
            if config.cook.verbose && recipes.len() > 1 {
                println!(
                    "\nCommand '{command}' completed for {} recipes.",
                    recipes.len()
                );
            }
            Ok(())
        }
    }
}

/// Cook all recipes in the TUI, then report how each one went.
fn cook_with_tui(config: &CliConfig, recipes: &[CookRecipe]) -> Result<()> {
    let command = CliCommand::Cook;
    let app = run_tui_cook(config.clone(), recipes.to_vec())?;
    if let TuiApp {
        dump_logs_on_exit: Some((name, log)),
        ..
    } = app
    {
        print_failure_log(&log);
        print_outcome(command, &name, Outcome::Failed);
        return Err(Error::Other("Execution has failed".into()));
    }

    for (recipe, status) in app.recipes {
        match status {
            RecipeStatus::Cached => print_outcome(command, &recipe.name, Outcome::Cached),
            RecipeStatus::Done => print_outcome(command, &recipe.name, Outcome::Successful),
            RecipeStatus::Failed(log) => {
                print_failure_log(&log);
                print_outcome(command, &recipe.name, Outcome::Failed);
            }
            // The TUI only returns normally once every recipe has finished.
            unfinished => {
                return Err(Error::Other(format!(
                    "{} was left unfinished ({unfinished:?})",
                    recipe.name
                )));
            }
        }
    }
    Ok(())
}

/// Run a fetch, cook or clean command over the recipes one at a time,
/// stopping at the first failure unless in nonstop mode.
fn run_sequentially(config: &CliConfig, command: CliCommand, recipes: &[CookRecipe]) -> Result<()> {
    for recipe in recipes {
        let result = if command.is_cleaning() {
            handle_clean(recipe, config, command)
        } else {
            fetch_or_cook(config, command, recipe)
        };
        match result {
            Ok(cached) => print_cached_or_successful(command, &recipe.name, cached),
            Err(e) if config.cook.nonstop => {
                if config.cook.verbose {
                    eprintln!("{e}");
                }
                if let Err(e) = handle_nonstop_fail(recipe) {
                    eprintln!("{e}");
                }
                print_outcome(command, &recipe.name, Outcome::Failed);
            }
            Err(e) => {
                print_outcome(command, &recipe.name, Outcome::Failed);
                return Err(e);
            }
        }
    }
    Ok(())
}
