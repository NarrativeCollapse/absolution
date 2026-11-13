//! Command line parsing for `repo`.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use cookbook::config::CookConfig;
use cookbook::cook::tree::{DisplayOptions, TreeOptions};
use cookbook::recipe::RecipeRule;
use cookbook::{Error, Result};
use pkg::PackageName;

pub const REPO_HELP_STR: &str = r#"
    Usage: repo <command> [flags] <recipe1> <recipe2> ...

    command list:
        fetch        download recipe sources
        cook         build recipe packages
        unfetch      delete recipe sources
        clean        delete recipe artifacts
        clean-target delete recipe artifacts for one target
        push         extract package into sysroot
        repo-list    show list of recipes
        cook-list    show list of recipes to build
        push-list    show list of recipes to package
        capture-rev  write lock to git recipes
        change-rule  override rule to recipes
        change-rule-local  override rule to specific recipes

    common flags:
        --cookbook=<cookbook_dir>  the "recipes" folder, default to $PWD/recipes
        --repo=<repo_dir>          the "repo" folder, default to $PWD/repo
        --with-package-deps        include package deps (always implied in push command)
        --all                      apply to all recipes in <cookbook_dir>
        --all-compiled             apply to all compiled recipes in <cookbook_dir>
        --all-binaries             apply to all compiled recipes in <cookbook_dir> that is configured as "binary"
        --category=<category>      apply to all recipes in <cookbook_dir>/<category>
        --filesystem=<filesystem>  override recipes config using installer file
        --repo-binary              override recipes config to use repo_binary
        --sysroot=<sysroot_dir>    used in "push", the "root" dir, default to $PWD/sysroot
        --no-metadata              used in "push", do not write pkgar_head or etc dir
        --display=<format>         used in "*-list", either "name", "path", "csv", "tree"
        --set-rule=<rule>          used in "change-rule", set wanted config rule
        --rollback                 used in "capture-rev", allow git to rollback
        --unset                    used in "capture-rev" and "change-rule", unset locks

    cook env and their defaults (booleans accept true/false or 1/0):
        CI=                          set to any value to disable TUI
        COOKBOOK_LOGS=               whether to capture build logs (default is !CI)
        COOKBOOK_OFFLINE=false       prevent internet access if possible
                                        ignored when command "fetch" is used
        COOKBOOK_NONSTOP=false       keep running even a recipe build failed
        COOKBOOK_COMPRESSED=false    build packages in compressed format
        COOKBOOK_GIT_TREELESS=false  clone sources as treeless by default
        COOKBOOK_VERBOSE=true        print success/error on each recipe
        COOKBOOK_VERBOSE_CMD=false   add -x to bash build script
        COOKBOOK_CLEAN_BUILD=false   remove build directory before building
        COOKBOOK_CLEAN_TARGET=false  remove target directory after building
        COOKBOOK_WRITE_FILETREE=false whether to write stage files tree
        COOKBOOK_MAKE_JOBS=          override build jobs count from nproc
        COOKBOOK_WEB=false           whether to generate package web files
"#;

macro_rules! bail_options_err {
    ($($arg:tt)*) => {
        return Err(cookbook::Error::Options(format!($($arg)*)))
    };
}
pub(crate) use bail_options_err;

#[derive(Clone)]
pub struct CliConfig {
    //FIXME: unused, redox-pkg hardcodes this to $PWD/recipes
    pub cookbook_dir: PathBuf,
    pub repo_dir: PathBuf,
    pub sysroot_dir: PathBuf,
    pub logs_dir: Option<PathBuf>,
    /// Restrict to recipes below this directory, relative to the tree root
    pub category: Option<PathBuf>,
    pub filesystem: Option<redox_installer::Config>,
    pub set_rule: Option<RecipeRule>,
    pub display: DisplayOptions,
    pub unset: bool,
    pub no_metadata: bool,
    pub with_rollback: bool,
    pub with_package_deps: bool,
    pub all: Option<AllOption>,
    /// Treat recipes without an explicit rule as "binary"
    pub repo_binary: bool,
    pub cook: CookConfig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CliCommand {
    Fetch,
    Cook,
    Unfetch,
    Clean,
    CleanTarget,
    Push,
    CookList,
    PushList,
    RepoList,
    CaptureRev,
    ChangeRule,
    ChangeRuleLocal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllOption {
    All,
    AllCompiled,
    AllBinaries,
}

impl CliCommand {
    /// Commands that only print a listing.
    pub fn is_informational(self) -> bool {
        self.tree_options().is_some()
    }

    pub fn tree_options(self) -> Option<TreeOptions> {
        match self {
            CliCommand::CookList => Some(TreeOptions::Cook),
            CliCommand::PushList => Some(TreeOptions::Push),
            CliCommand::RepoList => Some(TreeOptions::Repo),
            _ => None,
        }
    }

    pub fn is_change_rule(self) -> bool {
        matches!(
            self,
            CliCommand::ChangeRuleLocal | CliCommand::CaptureRev | CliCommand::ChangeRule
        )
    }

    /// Commands that operate on a recipe together with its build dependencies.
    pub fn is_building(self) -> bool {
        matches!(
            self,
            CliCommand::Fetch
                | CliCommand::Cook
                | CliCommand::CookList
                | CliCommand::CaptureRev
                | CliCommand::ChangeRule
        )
    }

    pub fn is_pushing(self) -> bool {
        matches!(self, CliCommand::Push | CliCommand::PushList)
    }

    pub fn is_cleaning(self) -> bool {
        matches!(
            self,
            CliCommand::Clean | CliCommand::CleanTarget | CliCommand::Unfetch
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CliCommand::Fetch => "fetch",
            CliCommand::Cook => "cook",
            CliCommand::Unfetch => "unfetch",
            CliCommand::Clean => "clean",
            CliCommand::CleanTarget => "clean-target",
            CliCommand::Push => "push",
            CliCommand::PushList => "push-list",
            CliCommand::CookList => "cook-list",
            CliCommand::RepoList => "repo-list",
            CliCommand::CaptureRev => "capture-rev",
            CliCommand::ChangeRule => "change-rule",
            CliCommand::ChangeRuleLocal => "change-rule-local",
        }
    }
}

impl FromStr for CliCommand {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "fetch" => CliCommand::Fetch,
            // "rebuild" is what repo_editor calls it
            "cook" | "rebuild" => CliCommand::Cook,
            "unfetch" => CliCommand::Unfetch,
            "clean" => CliCommand::Clean,
            "clean-target" => CliCommand::CleanTarget,
            "push" => CliCommand::Push,
            // "find" is used by scripts and implies --display=path
            "repo-list" | "find" => CliCommand::RepoList,
            "push-list" => CliCommand::PushList,
            "cook-list" => CliCommand::CookList,
            "capture-rev" => CliCommand::CaptureRev,
            "change-rule" => CliCommand::ChangeRule,
            "change-rule-local" => CliCommand::ChangeRuleLocal,
            _ => bail_options_err!("Unknown command {s:?}"),
        })
    }
}

impl fmt::Display for CliCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The parsed command line.
pub struct Cli {
    pub config: CliConfig,
    pub command: CliCommand,
    /// Recipes named on the command line
    pub recipe_names: Vec<PackageName>,
}

impl Cli {
    /// Parse the arguments following the program name. `current_dir` is the
    /// root of the tree and `cook` the settings from the configuration.
    pub fn parse(args: Vec<String>, current_dir: PathBuf, cook: CookConfig) -> Result<Self> {
        let mut config = CliConfig {
            cookbook_dir: current_dir.join("recipes"),
            repo_dir: current_dir.join("repo"),
            sysroot_dir: current_dir.join("sysroot"),
            logs_dir: cook.logs.then(|| current_dir.join("build/logs")),
            category: None,
            filesystem: None,
            set_rule: None,
            display: DisplayOptions::Tree,
            unset: false,
            no_metadata: false,
            with_rollback: false,
            with_package_deps: false,
            all: None,
            repo_binary: false,
            cook,
        };
        let mut command: Option<String> = None;
        let mut recipe_names = Vec::new();

        for arg in args {
            if let Some((key, value)) = arg.split_once('=').filter(|_| arg.starts_with("--")) {
                match key {
                    "--cookbook" => config.cookbook_dir = PathBuf::from(value),
                    "--repo" => config.repo_dir = PathBuf::from(value),
                    "--sysroot" => config.sysroot_dir = PathBuf::from(value),
                    "--category" => config.category = Some(PathBuf::from(value)),
                    "--set-rule" => config.set_rule = Some(value.parse()?),
                    "--display" => config.display = value.parse()?,
                    "--filesystem" => {
                        let filesystem = redox_installer::Config::from_file(&PathBuf::from(value))
                            .map_err(|e| Error::Other(format!("{e:?}")))?;
                        config.filesystem = Some(filesystem);
                    }
                    _ => bail_options_err!("Error: Unknown flag with value: {arg}"),
                }
            } else if let Some(category) = arg.strip_prefix("--category-") {
                // make cannot pass `/` in a pattern stem, so `.` stands in.
                config.category = Some(PathBuf::from(category.replace('.', "/")));
            } else if arg.starts_with('-') {
                match arg.as_str() {
                    "--repo-binary" => config.repo_binary = true,
                    "--with-package-deps" => config.with_package_deps = true,
                    "--no-metadata" => config.no_metadata = true,
                    "--rollback" => config.with_rollback = true,
                    "--unset" => config.unset = true,
                    "--all" => config.all = Some(AllOption::All),
                    "--all-compiled" => config.all = Some(AllOption::AllCompiled),
                    "--all-binaries" => config.all = Some(AllOption::AllBinaries),
                    _ => bail_options_err!("Error: Unknown flag: {arg}"),
                }
            } else if command.is_none() {
                command = Some(arg);
            } else {
                recipe_names.push(PackageName::new(arg)?);
            }
        }

        // Categories are given relative to the recipes directory.
        config.category = config.category.map(|c| PathBuf::from("recipes").join(c));

        let Some(command) = command else {
            bail_options_err!("Error: No command specified");
        };
        // repo_editor passes "change-rule <rule>" as a single argument.
        let command = match command.split_once(' ') {
            Some((name @ ("change-rule" | "change-rule-local"), rule)) => {
                config.set_rule = Some(rule.trim().parse()?);
                name.parse()?
            }
            _ => {
                if command == "find" {
                    config.display = DisplayOptions::Path;
                }
                command.parse::<CliCommand>()?
            }
        };
        if command.is_informational() {
            // Listings go to stdout; keep progress chatter out of them.
            config.cook.verbose = false;
        }

        Ok(Cli {
            config,
            command,
            recipe_names,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cook() -> CookConfig {
        CookConfig {
            offline: false,
            jobs: 1,
            tui: false,
            logs: false,
            nonstop: false,
            compressed: false,
            git_treeless: false,
            verbose: true,
            verbose_cmd: false,
            clean_build: false,
            clean_target: false,
            write_filetree: false,
        }
    }

    fn parse(args: &[&str]) -> Result<Cli> {
        Cli::parse(
            args.iter().map(|s| s.to_string()).collect(),
            PathBuf::from("/tree"),
            cook(),
        )
    }

    #[test]
    fn command_names_round_trip() {
        for command in [
            CliCommand::Fetch,
            CliCommand::Cook,
            CliCommand::Unfetch,
            CliCommand::Clean,
            CliCommand::CleanTarget,
            CliCommand::Push,
            CliCommand::CookList,
            CliCommand::PushList,
            CliCommand::RepoList,
            CliCommand::CaptureRev,
            CliCommand::ChangeRule,
            CliCommand::ChangeRuleLocal,
        ] {
            assert_eq!(command.to_string().parse::<CliCommand>().unwrap(), command);
        }
        assert_eq!("rebuild".parse::<CliCommand>().unwrap(), CliCommand::Cook);
        assert!(matches!(
            "nope".parse::<CliCommand>(),
            Err(Error::Options(_))
        ));
    }

    #[test]
    fn parses_command_recipes_and_flags() {
        let cli = parse(&[
            "cook",
            "--with-package-deps",
            "bash",
            "--repo-binary",
            "host:zlib",
            "--sysroot=/mnt",
        ])
        .unwrap();
        assert_eq!(cli.command, CliCommand::Cook);
        let names: Vec<&str> = cli.recipe_names.iter().map(|n| n.as_str()).collect();
        assert_eq!(names, ["bash", "host:zlib"]);
        assert!(cli.config.with_package_deps);
        assert!(cli.config.repo_binary);
        assert_eq!(cli.config.sysroot_dir, PathBuf::from("/mnt"));
        assert_eq!(cli.config.repo_dir, PathBuf::from("/tree/repo"));
        assert!(cli.config.cook.verbose);
        assert_eq!(cli.config.logs_dir, None);
    }

    #[test]
    fn category_flags_are_relative_to_recipes() {
        let cli = parse(&["clean", "--category=libs"]).unwrap();
        assert_eq!(cli.config.category, Some(PathBuf::from("recipes/libs")));
        let cli = parse(&["clean", "--category-wip.net"]).unwrap();
        assert_eq!(cli.config.category, Some(PathBuf::from("recipes/wip/net")));
    }

    #[test]
    fn listings_are_quiet_and_find_prints_paths() {
        let cli = parse(&["find", "bash"]).unwrap();
        assert_eq!(cli.command, CliCommand::RepoList);
        assert!(matches!(cli.config.display, DisplayOptions::Path));
        assert!(!cli.config.cook.verbose);
    }

    #[test]
    fn rules_are_validated_when_parsed() {
        let cli = parse(&["change-rule", "--set-rule=binary", "bash"]).unwrap();
        assert_eq!(cli.config.set_rule, Some(RecipeRule::Binary));
        let cli = parse(&["change-rule-local local", "bash"]).unwrap();
        assert_eq!(cli.command, CliCommand::ChangeRuleLocal);
        assert_eq!(cli.config.set_rule, Some(RecipeRule::Local));
        assert!(parse(&["change-rule", "--set-rule=bogus"]).is_err());
    }

    #[test]
    fn bad_usage_is_an_options_error() {
        for args in [
            &["--bogus", "cook"][..],
            &["cook", "--bogus=1"],
            &["cook", "-x"],
            &["--all"],
        ] {
            assert!(
                matches!(parse(args), Err(Error::Options(_))),
                "{args:?} should be rejected"
            );
        }
        assert!(parse(&["cook", "bad/name"]).is_err());
    }
}
