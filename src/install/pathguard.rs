use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use crate::error::{Error, InstallRefusedReason};

#[derive(Debug, PartialEq, Eq)]
pub enum Resolution {
    This,
    Other(PathBuf),
    Nothing,
}

pub fn resolve(current_exe: &Path, path_var: &str) -> Resolution {
    let first = std::env::split_paths(path_var)
        .map(|dir| dir.join("lets"))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        });
    match first.map(|found| found.canonicalize().unwrap_or(found)) {
        Some(found) if found == current_exe => Resolution::This,
        Some(found) => Resolution::Other(found),
        None => Resolution::Nothing,
    }
}

/// Returns the running binary's canonical path.
pub fn refuse_if_shadowed(path_var: &str) -> Result<PathBuf, Error> {
    let current = current_exe()?;
    match resolve(&current, path_var) {
        Resolution::Other(found) => Err(Error::InstallRefused {
            reason: InstallRefusedReason::DifferentLetsOnPath { found, current },
        }),
        Resolution::This | Resolution::Nothing => Ok(current),
    }
}

/// The installed hooks run a bare `lets`, so `PATH` has to resolve it to this binary.
pub fn refuse_unless_resolved(path_var: &str) -> Result<(), Error> {
    let current = current_exe()?;
    match resolve(&current, path_var) {
        Resolution::This => Ok(()),
        Resolution::Other(found) => Err(Error::InstallRefused {
            reason: InstallRefusedReason::DifferentLetsOnPath { found, current },
        }),
        Resolution::Nothing => Err(Error::InstallRefused {
            reason: InstallRefusedReason::NotOnPath { current },
        }),
    }
}

// `Error::Io` needs a path and there is no file here, so it names the subject instead.
fn current_exe() -> Result<PathBuf, Error> {
    let to_io = |source: std::io::Error| Error::Io {
        path: PathBuf::from("current executable"),
        source,
    };
    std::env::current_exe()
        .map_err(to_io)?
        .canonicalize()
        .map_err(to_io)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_lets(dir: &Path, mode: u32) -> PathBuf {
        let path = dir.join("lets");
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path.canonicalize().unwrap()
    }

    fn joined(dirs: &[&Path]) -> String {
        std::env::join_paths(dirs).unwrap().into_string().unwrap()
    }

    #[test]
    fn this_binary_alone_on_path_resolves_to_this() {
        let dir = tempfile::tempdir().unwrap();
        let current = write_lets(dir.path(), 0o755);

        assert_eq!(resolve(&current, &joined(&[dir.path()])), Resolution::This);
    }

    #[test]
    fn a_different_lets_earlier_on_path_is_the_one_that_resolves() {
        let other_dir = tempfile::tempdir().unwrap();
        let current_dir = tempfile::tempdir().unwrap();
        let other = write_lets(other_dir.path(), 0o755);
        let current = write_lets(current_dir.path(), 0o755);

        let path_var = joined(&[other_dir.path(), current_dir.path()]);

        assert_eq!(resolve(&current, &path_var), Resolution::Other(other));
    }

    #[test]
    fn a_different_lets_later_on_path_is_never_reached() {
        let current_dir = tempfile::tempdir().unwrap();
        let other_dir = tempfile::tempdir().unwrap();
        let current = write_lets(current_dir.path(), 0o755);
        write_lets(other_dir.path(), 0o755);

        let path_var = joined(&[current_dir.path(), other_dir.path()]);

        assert_eq!(resolve(&current, &path_var), Resolution::This);
    }

    #[test]
    fn a_non_executable_lets_earlier_on_path_is_skipped_as_the_shell_skips_it() {
        let stale_dir = tempfile::tempdir().unwrap();
        let current_dir = tempfile::tempdir().unwrap();
        write_lets(stale_dir.path(), 0o644);
        let current = write_lets(current_dir.path(), 0o755);

        let path_var = joined(&[stale_dir.path(), current_dir.path()]);

        assert_eq!(resolve(&current, &path_var), Resolution::This);
    }

    #[test]
    fn no_lets_anywhere_on_path_resolves_to_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let current = write_lets(dir.path(), 0o755);
        let empty_dir = tempfile::tempdir().unwrap();

        assert_eq!(
            resolve(&current, &joined(&[empty_dir.path()])),
            Resolution::Nothing
        );
    }

    #[test]
    fn a_symlink_on_path_to_this_binary_resolves_to_this() {
        let real_dir = tempfile::tempdir().unwrap();
        let link_dir = tempfile::tempdir().unwrap();
        let current = write_lets(real_dir.path(), 0o755);
        std::os::unix::fs::symlink(&current, link_dir.path().join("lets")).unwrap();

        assert_eq!(
            resolve(&current, &joined(&[link_dir.path()])),
            Resolution::This
        );
    }
}
