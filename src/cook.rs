//! Fetching, building and packaging recipes.

/// The key packages built here are signed with, created on first use.
pub const SECRET_KEY_PATH: &str = "build/id_ed25519.toml";
/// The public half of [`SECRET_KEY_PATH`].
pub const PUBLIC_KEY_PATH: &str = "build/id_ed25519.pub.toml";

// Named `cook_build` to avoid confusion with a cargo build script.
pub mod cook_build;
pub mod fetch;
pub mod fetch_repo;
pub mod fs;
pub mod git;
pub mod ident;
pub mod package;
pub mod pty;
pub mod redoxer;
pub mod script;
pub mod tree;
pub mod tui;
