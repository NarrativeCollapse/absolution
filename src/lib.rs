//! The cookbook: fetches, builds and packages recipes into a package
//! repository and installs them into a system image.

pub mod config;
pub mod cook;
mod error;
pub mod recipe;
pub mod staged_pkg;
pub mod web;

pub use error::{Error, Result};
pub(crate) use error::{bail_other_err, wrap_io_err};

pub(crate) use cook::pty::log_to_pty;

/// Maximum depth of a dependency chain before it is reported as a cycle.
pub const WALK_DEPTH: usize = 16;

/// Where recipes built with the "remote" kind download pre-built packages.
/// Point it elsewhere with a mirror in `cookbook.toml`.
pub const REMOTE_PKG_SOURCE: &str = "https://static.redox-os.org/pkg";

/// Whether the cookbook is running on Absolution itself (self-hosting).
/// The compiler still calls the target OS "redox".
pub fn self_hosted() -> bool {
    cfg!(target_os = "redox")
}

/// The cross target sub-directory requested through `COOKBOOK_CROSS_TARGET`.
pub fn cross_target() -> Option<String> {
    std::env::var("COOKBOOK_CROSS_TARGET")
        .ok()
        .filter(|s| !s.is_empty())
}
