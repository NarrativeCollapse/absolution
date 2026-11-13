use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use absinit::config::{self, ADMIN_DIR, VENDOR_DIR};
use absinit::plan::Plan;
use absinit::runtime;
use absinit::supervisor::Supervisor;

const USAGE: &str = "usage: absinit [--services DIR]... [--check]";

fn main() -> ExitCode {
    let mut dirs = Vec::new();
    let mut check = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--services" => match args.next() {
                Some(dir) => dirs.push(PathBuf::from(dir)),
                None => return usage_error("--services needs a directory"),
            },
            "--check" => check = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => return usage_error(&format!("unknown argument {other:?}")),
        }
    }
    if dirs.is_empty() {
        dirs = vec![PathBuf::from(VENDOR_DIR), PathBuf::from(ADMIN_DIR)];
    }

    let loaded = config::load(&dirs);
    for (path, reason) in &loaded.errors {
        eprintln!("absinit: {}: {reason}", path.display());
    }
    let missing: BTreeSet<String> = loaded
        .errors
        .iter()
        .filter_map(|(path, _)| path.file_stem()?.to_str().map(str::to_string))
        .collect();
    let plan = Plan::new(loaded.services, &missing);

    if check {
        for name in plan.services.keys() {
            match plan.skipped.get(name) {
                Some(reason) => println!("{name}: skipped: {reason}"),
                None => println!("{name}: ok"),
            }
        }
        let problems = loaded.errors.len() + plan.skipped.len();
        return if problems == 0 {
            ExitCode::SUCCESS
        } else {
            eprintln!("absinit: {problems} problem(s)");
            ExitCode::FAILURE
        };
    }

    runtime::run(Supervisor::new(plan, Instant::now()));
    ExitCode::SUCCESS
}

fn usage_error(message: &str) -> ExitCode {
    eprintln!("absinit: {message}\n{USAGE}");
    ExitCode::from(2)
}
