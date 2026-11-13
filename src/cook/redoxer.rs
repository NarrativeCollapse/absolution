//! Locating the `cookbook_redoxer` wrapper, which runs cargo and build
//! scripts inside the cross-compilation environment.

use std::path::{Path, PathBuf};

use crate::{Error, Result, self_hosted};

const LOCAL_BUILD: &str = "target/release/cookbook_redoxer";

/// The `cookbook_redoxer` to run: the one built in this tree if there is one,
/// otherwise, when self-hosting, the installed one.
pub fn cookbook_redoxer() -> Result<PathBuf> {
    let local = Path::new(LOCAL_BUILD);
    if local.is_file() {
        return local
            .canonicalize()
            .map_err(crate::wrap_io_err!(local, "Resolving path"));
    }
    if self_hosted() {
        return Ok(PathBuf::from("cookbook_redoxer"));
    }
    Err(Error::Other(format!(
        "{LOCAL_BUILD} is missing; build the cookbook with `cargo build --release` first"
    )))
}

/// Whether the cross toolchain's cargo is installed.
pub fn toolchain_has_cargo() -> bool {
    std::env::var_os("REDOXER_TOOLCHAIN")
        .is_some_and(|dir| PathBuf::from(dir).join("bin/cargo").is_file())
}
