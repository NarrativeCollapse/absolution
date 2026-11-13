//! File system, process and hashing helpers with path-carrying errors.

use std::{
    collections::BTreeSet,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::SystemTime,
};

use serde::{Serialize, de::DeserializeOwned};
use walkdir::{DirEntry, WalkDir};

use crate::{
    Error, Result, bail_other_err,
    config::translate_mirror,
    cook::pty::{PtyOut, spawn_to_pipe},
    wrap_io_err,
};

// Directories and files

/// Remove a file, or a directory and everything in it.
pub fn remove_all(path: &Path) -> Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .map_err(wrap_io_err!(path, "Removing"))
}

/// Create a directory and its missing parents.
pub fn create_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(wrap_io_err!(dir, "Creating directory"))
}

/// Create a directory, emptying it first if it already exists.
pub fn create_dir_clean(dir: &Path) -> Result<()> {
    if dir.is_dir() {
        remove_all(dir)?;
    }
    create_dir(dir)
}

/// Create and return `<recipe_dir>/target/<target>`.
pub fn create_target_dir(recipe_dir: &Path, target: &str) -> Result<PathBuf> {
    let target_dir = recipe_dir.join("target").join(target);
    create_dir(&target_dir)?;
    Ok(target_dir)
}

/// Top-level directories of a local source tree that are not source: build
/// output and version control.
pub const NOT_SOURCE: [&str; 2] = ["target", ".git"];

/// Copy a local source tree into `dst`, leaving out [`NOT_SOURCE`].
pub fn copy_source_tree(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        if NOT_SOURCE.iter().any(|name| entry.file_name() == *name) {
            continue;
        }
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Like [`modified_dir`] for a local source tree, leaving out [`NOT_SOURCE`].
pub fn modified_source_tree(dir: &Path) -> Result<SystemTime> {
    modified_dir_filtered(dir, |entry| {
        entry.depth() != 1 || !NOT_SOURCE.iter().any(|name| entry.file_name() == *name)
    })
}

/// Recursively copy the contents of `src` into `dst`.
pub fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Move files out of `src` into other directory trees.
///
/// `destination` is called with each file's path relative to `src`. When it
/// returns a directory, the file is moved there under the same relative path;
/// when it returns `None` the file stays where it is.
pub fn move_files_matching<'a>(
    src: &Path,
    destination: &dyn Fn(&Path) -> Option<&'a Path>,
) -> io::Result<()> {
    fn visit<'a>(
        root: &Path,
        dir: &Path,
        destination: &dyn Fn(&Path) -> Option<&'a Path>,
    ) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                visit(root, &path, destination)?;
                continue;
            }
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            if let Some(dst_root) = destination(relative) {
                let dst = dst_root.join(relative);
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(&path, &dst)?;
            }
        }
        Ok(())
    }
    visit(src, src, destination)
}

pub fn symlink(original: impl AsRef<Path>, link: impl AsRef<Path>) -> Result<()> {
    std::os::unix::fs::symlink(&original, &link)
        .map_err(wrap_io_err!(link.as_ref(), "Creating symlink"))
}

pub fn rename(src: &Path, dst: &Path) -> Result<()> {
    fs::rename(src, dst).map_err(wrap_io_err!(src, dst, "Renaming"))
}

pub fn read_to_string(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(wrap_io_err!(path, "Reading file"))
}

/// Whether the entries of `dir` are exactly `expected`, ignoring dotfiles.
pub fn check_files_present(dir: &Path, expected: &BTreeSet<&str>) -> Result<bool> {
    let mut matches = 0;
    for entry in fs::read_dir(dir).map_err(wrap_io_err!(dir, "Listing directory"))? {
        let entry = entry.map_err(wrap_io_err!(dir, "Reading directory entry"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if expected.contains(name) {
            matches += 1;
        } else if !name.starts_with('.') {
            return Ok(false);
        }
    }
    Ok(matches == expected.len())
}

/// Fail with an offline-mode explanation when `path` does not exist.
pub fn offline_check_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        bail_other_err!(
            "{:?} does not exist and cannot be fetched in offline mode",
            path.display()
        );
    }
    Ok(())
}

// Modification times

/// Whether `src` was modified after `dst`. A missing `dst` counts as older;
/// a missing `src` is never newer.
pub fn modified_is_newer(src: &Path, dst: &Path) -> bool {
    match (modified(src), modified(dst)) {
        (Ok(src), Ok(dst)) => src > dst,
        (Ok(_), Err(_)) => true,
        (Err(_), _) => false,
    }
}

pub fn modified(path: &Path) -> Result<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map_err(wrap_io_err!(path, "Reading modified time"))
}

/// The newest modification time among `paths`, all of which must exist.
pub fn modified_newest(paths: &[PathBuf]) -> Result<SystemTime> {
    let mut newest = SystemTime::UNIX_EPOCH;
    for path in paths {
        newest = newest.max(modified(path)?);
    }
    Ok(newest)
}

fn modified_dir_filtered(dir: &Path, filter: impl FnMut(&DirEntry) -> bool) -> Result<SystemTime> {
    let mut newest = modified(dir)?;
    for entry in WalkDir::new(dir).into_iter().filter_entry(filter) {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            continue;
        }
        let time = metadata
            .modified()
            .map_err(wrap_io_err!(entry.path(), "Reading modified time"))?;
        newest = newest.max(time);
    }
    Ok(newest)
}

/// The newest modification time of `dir` itself and of any file inside it.
pub fn modified_dir(dir: &Path) -> Result<SystemTime> {
    modified_dir_filtered(dir, |_| true)
}

/// Like [`modified_dir`], skipping `.git` directories.
pub fn modified_dir_ignore_git(dir: &Path) -> Result<SystemTime> {
    modified_dir_filtered(dir, |entry| entry.file_name() != ".git")
}

// Processes

/// Run a command to completion, failing if it exits unsuccessfully.
pub fn run_command(mut command: Command, logger: &PtyOut) -> Result<()> {
    let status = spawn_to_pipe(&mut command, logger)?
        .wait()
        .map_err(wrap_io_err!("Waiting for command"))?;
    if !status.success() {
        return Err(Error::command(command, status));
    }
    Ok(())
}

/// Like [`run_command`], feeding `stdin_data` to the command's stdin.
pub fn run_command_stdin(mut command: Command, stdin_data: &[u8], logger: &PtyOut) -> Result<()> {
    command.stdin(Stdio::piped());
    let mut child = spawn_to_pipe(&mut command, logger)?;

    let Some(mut stdin) = child.stdin.take() else {
        bail_other_err!("stdin of {command:?} is not captured");
    };
    let written = stdin.write_all(stdin_data);
    // Close stdin so the child sees EOF, and reap it even if writing failed.
    drop(stdin);
    let status = child.wait().map_err(wrap_io_err!("Waiting for command"))?;

    if !status.success() {
        return Err(Error::command(command, status));
    }
    written.map_err(wrap_io_err!("Writing to stdin"))
}

/// Download `url` to `dest` unless `dest` already exists. The download goes
/// to `<dest>.tmp` first so an interrupted transfer is resumed, not trusted.
pub fn download_wget(url: &str, dest: &Path, logger: &PtyOut) -> Result<()> {
    if dest.is_file() {
        return Ok(());
    }
    let mut dest_tmp = dest.as_os_str().to_owned();
    dest_tmp.push(".tmp");
    let dest_tmp = PathBuf::from(dest_tmp);

    let mut command = Command::new("wget");
    command.arg(translate_mirror(url));
    command.arg("--continue").arg("-O").arg(&dest_tmp);
    run_command(command, logger)?;
    rename(&dest_tmp, dest)
}

// TOML

pub fn serialize_and_write<T: Serialize>(path: &Path, content: &T) -> Result<()> {
    let toml = toml::to_string(content).map_err(|e| {
        Error::Other(format!(
            "Failed to serialize content for {:?}: {e}",
            path.display()
        ))
    })?;
    fs::write(path, toml).map_err(wrap_io_err!(path, "Writing file"))
}

pub fn read_toml<T: DeserializeOwned>(path: &Path) -> Result<T> {
    toml::from_str(&read_to_string(path)?)
        .map_err(|e| Error::Package(pkg::PackageError::Parse(e, Some(path.to_path_buf()))))
}

// Hashing

fn file_blake3(path: &Path) -> Result<blake3::Hash> {
    let mut file = fs::File::open(path).map_err(wrap_io_err!(path, "Opening file for blake3"))?;
    let mut hasher = blake3::Hasher::new();
    hasher
        .update_reader(&mut file)
        .map_err(wrap_io_err!(path, "Reading file for blake3"))?;
    Ok(hasher.finalize())
}

/// The blake3 hash of a file, as lowercase hex.
pub fn get_file_blake3(path: &Path) -> Result<String> {
    file_blake3(path).map(|hash| hash.to_hex().to_string())
}

/// One hash identifying an ordered list of files (relative to `dir`) followed
/// by an ordered list of literal strings. Returns an empty string when there
/// is nothing to hash, which is how an absent identifier is stored.
pub fn get_combined_blake3(dir: &Path, files: &[String], contents: &[String]) -> Result<String> {
    if files.is_empty() && contents.is_empty() {
        return Ok(String::new());
    }

    // Hash the fixed-size hash of each item, tagged by kind, so that neither
    // reordering items nor moving bytes between adjacent items goes unnoticed.
    let mut hasher = blake3::Hasher::new();
    for file in files {
        hasher.update(b"file");
        hasher.update(file_blake3(&dir.join(file))?.as_bytes());
    }
    for content in contents {
        hasher.update(b"text");
        hasher.update(blake3::hash(content.as_bytes()).as_bytes());
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn combined_hash_is_empty_without_inputs() {
        assert_eq!(get_combined_blake3(Path::new("."), &[], &[]).unwrap(), "");
    }

    #[test]
    fn combined_hash_depends_on_order_content_and_kind() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("a.patch"), "aaa");
        write(&tmp.path().join("b.patch"), "bbb");
        let hash = |files: &[&str], contents: &[&str]| {
            let files: Vec<String> = files.iter().map(|s| s.to_string()).collect();
            let contents: Vec<String> = contents.iter().map(|s| s.to_string()).collect();
            get_combined_blake3(tmp.path(), &files, &contents).unwrap()
        };

        let ab = hash(&["a.patch", "b.patch"], &[]);
        assert_eq!(ab, hash(&["a.patch", "b.patch"], &[]));
        assert_ne!(ab, hash(&["b.patch", "a.patch"], &[]));
        assert_ne!(ab, hash(&["a.patch"], &[]));
        assert_ne!(hash(&["a.patch"], &[]), hash(&["a.patch", "a.patch"], &[]));
        assert_ne!(hash(&["a.patch"], &[]), hash(&[], &["aaa"]));
        assert_ne!(hash(&[], &["x"]), hash(&[], &["y"]));

        let missing = get_combined_blake3(tmp.path(), &["nope".to_string()], &[]);
        assert!(missing.is_err());
    }

    #[test]
    fn move_files_matching_moves_only_selected_files() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("stage");
        let dev = tmp.path().join("stage.dev");
        write(&src.join("usr/include/a.h"), "");
        write(&src.join("usr/bin/tool"), "");

        move_files_matching(&src, &|relative| {
            relative.starts_with("usr/include").then_some(dev.as_path())
        })
        .unwrap();

        assert!(dev.join("usr/include/a.h").is_file());
        assert!(!src.join("usr/include/a.h").exists());
        assert!(src.join("usr/bin/tool").is_file());
    }

    #[test]
    fn check_files_present_requires_exact_set() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("a"), "");
        write(&tmp.path().join(".hidden"), "");
        assert!(check_files_present(tmp.path(), &BTreeSet::from(["a"])).unwrap());
        assert!(!check_files_present(tmp.path(), &BTreeSet::from(["a", "b"])).unwrap());
        assert!(!check_files_present(tmp.path(), &BTreeSet::new()).unwrap());
    }

    #[test]
    fn modified_dir_ignore_git_skips_git_files() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("main.c"), "");
        write(&tmp.path().join(".git/index"), "");
        // Make the .git file unmistakably the newest thing in the tree.
        let future = SystemTime::now() + std::time::Duration::from_secs(3600);
        fs::File::options()
            .write(true)
            .open(tmp.path().join(".git/index"))
            .unwrap()
            .set_modified(future)
            .unwrap();

        assert_eq!(modified_dir(tmp.path()).unwrap(), future);
        assert!(modified_dir_ignore_git(tmp.path()).unwrap() < future);
    }

    #[test]
    fn source_trees_leave_out_build_output() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("component");
        write(&src.join("src/main.rs"), "fn main() {}");
        write(&src.join("src/target/kept.rs"), "");
        write(&src.join("target/debug/huge"), "build output");
        write(&src.join(".git/HEAD"), "ref");

        let dst = tmp.path().join("source");
        copy_source_tree(&src, &dst).unwrap();
        assert!(dst.join("src/main.rs").is_file());
        // Only the top-level build directory is left out.
        assert!(dst.join("src/target/kept.rs").is_file());
        assert!(!dst.join("target").exists());
        assert!(!dst.join(".git").exists());

        // Building the component must not make its source look newer.
        let before = modified_source_tree(&src).unwrap();
        let future = SystemTime::now() + std::time::Duration::from_secs(3600);
        fs::File::options()
            .write(true)
            .open(src.join("target/debug/huge"))
            .unwrap()
            .set_modified(future)
            .unwrap();
        assert_eq!(modified_source_tree(&src).unwrap(), before);
    }

    #[test]
    fn run_command_stdin_feeds_input_and_reports_failure() {
        let mut ok = Command::new("sh");
        ok.arg("-s");
        run_command_stdin(ok, b"exit 0\n", &None).unwrap();

        let mut fail = Command::new("sh");
        fail.arg("-s");
        let err = run_command_stdin(fail, b"exit 3\n", &None).unwrap_err();
        assert!(matches!(err, Error::Command(..)), "{err}");
    }

    #[test]
    fn toml_round_trip() {
        type Map = std::collections::BTreeMap<String, i32>;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.toml");
        let value = Map::from([("k".to_string(), 1)]);
        serialize_and_write(&path, &value).unwrap();
        assert_eq!(read_toml::<Map>(&path).unwrap(), value);
        assert!(read_toml::<Map>(&tmp.path().join("missing.toml")).is_err());
    }
}
