//! The error type shared by the whole cookbook.

use std::fmt::{self, Display};
use std::io;
use std::path::PathBuf;
use std::process::{Command, ExitStatus};

/// Errors produced by the cookbook.
///
/// I/O contexts are short gerunds ("Reading file", "Renaming"); the `Display`
/// implementation appends "failed", the path and the underlying cause.
#[derive(Debug)]
pub enum Error {
    /// An I/O operation failed, optionally on a known path.
    Io {
        source: io::Error,
        path: Option<PathBuf>,
        context: &'static str,
    },
    /// An I/O operation between two paths (copy, rename) failed.
    FileIo {
        source: io::Error,
        src: PathBuf,
        dst: PathBuf,
        context: &'static str,
    },
    /// A child process exited unsuccessfully.
    Command(Box<Command>, ExitStatus),
    Package(pkg::PackageError),
    PackageBackend(Box<pkg::backend::Error>),
    Pkgar(Box<pkgar::Error>),
    /// Invalid command line usage; the caller prints the help text.
    Options(String),
    Other(String),
}

impl Error {
    pub fn from_last_io_error(context: &'static str) -> Error {
        Self::from_io_error(io::Error::last_os_error(), context)
    }

    pub fn from_io_error(source: io::Error, context: &'static str) -> Error {
        Error::Io {
            source,
            path: None,
            context,
        }
    }

    pub fn command(command: Command, status: ExitStatus) -> Error {
        Error::Command(Box::new(command), status)
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io {
                source,
                path: Some(path),
                context,
            } => write!(f, "{context} failed at \"{}\": {source}", path.display()),
            Error::Io {
                source,
                path: None,
                context,
            } => write!(f, "{context} failed: {source}"),
            Error::FileIo {
                source,
                src,
                dst,
                context,
            } => write!(
                f,
                "{context} failed from \"{}\" to \"{}\": {source}",
                src.display(),
                dst.display(),
            ),
            Error::Command(command, status) => {
                write!(
                    f,
                    "Failed to run [{command:?}]: exited with status {status}"
                )
            }
            Error::Package(error) => write!(f, "Package error: {error}"),
            Error::PackageBackend(error) => write!(f, "Package backend error: {error}"),
            Error::Pkgar(error) => write!(f, "Package archive error: {error}"),
            Error::Other(message) | Error::Options(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } | Error::FileIo { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Build a closure that wraps an [`io::Error`] with a context, and optionally
/// one path (`path, context`) or two (`src, dst, context`).
macro_rules! wrap_io_err {
    ($context:expr) => {
        |source| $crate::Error::Io {
            source,
            path: None,
            context: $context,
        }
    };
    ($path:expr, $context:expr) => {
        |source| $crate::Error::Io {
            source,
            path: Some($path.to_path_buf()),
            context: $context,
        }
    };
    ($src:expr, $dst:expr, $context:expr) => {
        |source| $crate::Error::FileIo {
            source,
            src: $src.to_path_buf(),
            dst: $dst.to_path_buf(),
            context: $context,
        }
    };
}

/// Return early with [`Error::Other`] built from a format string.
macro_rules! bail_other_err {
    ($($arg:tt)*) => {
        return Err($crate::Error::Other(format!($($arg)*)))
    };
}

pub(crate) use {bail_other_err, wrap_io_err};

impl From<&'static str> for Error {
    fn from(value: &'static str) -> Self {
        Error::Other(value.to_string())
    }
}

impl From<String> for Error {
    fn from(value: String) -> Self {
        Error::Other(value)
    }
}

impl From<pkg::PackageError> for Error {
    fn from(value: pkg::PackageError) -> Self {
        Error::Package(value)
    }
}

impl From<pkg::backend::Error> for Error {
    fn from(value: pkg::backend::Error) -> Self {
        match value {
            pkg::backend::Error::IO(source, path, context) => Error::Io {
                source,
                path: Some(path),
                context,
            },
            pkg::backend::Error::Download(pkg::net_backend::DownloadError::IO(source)) => {
                Error::Io {
                    source,
                    path: None,
                    context: "Package network I/O",
                }
            }
            error => Error::PackageBackend(Box::new(error)),
        }
    }
}

impl From<pkgar::Error> for Error {
    fn from(value: pkgar::Error) -> Self {
        match value {
            pkgar::Error::Io {
                source,
                path,
                context,
            } => Error::Io {
                source,
                path,
                context,
            },
            error => Error::Pkgar(Box::new(error)),
        }
    }
}

impl From<pkgar_keys::Error> for Error {
    fn from(value: pkgar_keys::Error) -> Self {
        Error::Pkgar(Box::new(pkgar::Error::Keys(value)))
    }
}

impl From<walkdir::Error> for Error {
    fn from(value: walkdir::Error) -> Self {
        let path = value.path().map(|p| p.to_path_buf());
        match value.into_io_error() {
            Some(source) => Error::Io {
                source,
                path,
                context: "Walking directory",
            },
            None => Error::Other(format!("File system loop found at {path:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn io_error_displays_context_and_path() {
        let err =
            wrap_io_err!(std::path::Path::new("/x"), "Reading file")(io::Error::other("boom"));
        assert_eq!(err.to_string(), "Reading file failed at \"/x\": boom");
        assert!(err.source().is_some());
    }

    #[test]
    fn other_error_has_no_source() {
        let err = Error::from("bad 1");
        assert_eq!(err.to_string(), "bad 1");
        assert!(err.source().is_none());
    }
}
