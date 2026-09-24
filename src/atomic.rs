use std::io::Write as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use crate::error::{Error, UnsupportedReason};
use crate::output::Sha12;

/// What a shell redirection creates, before the umask; `tempfile` alone would give 0o600.
const NEW_FILE_MODE: u32 = 0o666;

pub fn hash12(bytes: &[u8]) -> Sha12 {
    Sha12::parse(blake3::hash(bytes).to_hex().as_str())
        .expect("a blake3 hex digest is 64 chars, always >= the 12-hex floor")
}

/// Returns the checked bytes so the caller splices what it verified; a re-read reopens the race.
pub fn verify_if(path: &Path, expected: &Sha12) -> Result<Vec<u8>, Error> {
    let bytes = std::fs::read(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let actual = hash12(&bytes);
    if actual.as_str() == expected.as_str() {
        return Ok(bytes);
    }
    Err(Error::Changed {
        path: path.to_path_buf(),
        expected: expected.as_str().to_owned(),
        actual: actual.as_str().to_owned(),
    })
}

/// Checks the symlink's referent, the file the rename replaces, never the link itself.
pub fn check_before_write(path: &Path, max_file_bytes: u64) -> Result<(), Error> {
    let resolved = resolve_symlink(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = match std::fs::symlink_metadata(&resolved) {
        Ok(metadata) => metadata,
        // `lets write` creates files; a path with nothing at it has no identity to refuse.
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(Error::Io {
                path: resolved,
                source,
            });
        },
    };
    if metadata.is_file() && metadata.nlink() > 1 {
        return Err(Error::Unsupported {
            path: resolved,
            reason: UnsupportedReason::Hardlink,
        });
    }
    if metadata.len() > max_file_bytes {
        return Err(Error::Unsupported {
            path: resolved,
            reason: UnsupportedReason::TooLarge {
                bytes: metadata.len(),
                limit: max_file_bytes,
            },
        });
    }
    Ok(())
}

pub fn resolve_symlink(path: &Path) -> std::io::Result<PathBuf> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(path.to_path_buf()),
        Err(err) => return Err(err),
    };
    if metadata.file_type().is_symlink() {
        return std::fs::canonicalize(path);
    }
    Ok(path.to_path_buf())
}

/// Runs during validation, so a batch holding one such file writes nothing.
pub fn refuse_unwritable(path: &Path) -> Result<(), Error> {
    let referent = resolve_symlink(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if let Ok(metadata) = std::fs::metadata(&referent) {
        if metadata.permissions().readonly() {
            return Err(Error::ReadOnly {
                path: path.to_path_buf(),
            });
        }
        probe_writable(path, &referent)?;
    }
    Ok(())
}

/// Probes beside the referent because that is where `write_atomic` puts its temp file.
fn probe_writable(path: &Path, referent: &Path) -> Result<(), Error> {
    let parent = referent
        .parent()
        .expect("a probed path names a file, so its path has a parent");
    tempfile::Builder::new()
        .prefix(".lets-probe")
        .tempfile_in(parent)
        .map(drop)
        .map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })
}

/// `mode: None` keeps an existing file's bits; a new file gets `NEW_FILE_MODE` under the umask.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<(), Error> {
    let resolved = resolve_symlink(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    // Renaming over the link would split it from its referent.
    let parent = resolved
        .parent()
        .expect("a write target names a file, so its path has a parent");
    let existing_mode = std::fs::metadata(&resolved)
        .ok()
        .map(|metadata| metadata.permissions().mode() & 0o7777);

    let mut builder = tempfile::Builder::new();
    if mode.is_none() && existing_mode.is_none() {
        builder.permissions(std::fs::Permissions::from_mode(NEW_FILE_MODE));
    }
    let mut temp = builder.tempfile_in(parent).map_err(|source| Error::Io {
        path: resolved.clone(),
        source,
    })?;
    temp.write_all(bytes).map_err(|source| Error::Io {
        path: resolved.clone(),
        source,
    })?;
    // Set before persist, so the file is never visible at the target path with the wrong bits.
    if let Some(bits) = mode.or(existing_mode) {
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(bits)).map_err(
            |source| Error::Io {
                path: resolved.clone(),
                source,
            },
        )?;
    }
    temp.persist(&resolved).map_err(|err| Error::Io {
        path: resolved.clone(),
        source: err.error,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_mode(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("file exists")
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn write_atomic_persists_exact_bytes_including_crlf_and_bom() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("f.txt");
        let bytes = b"\xEF\xBB\xBFfirst\r\nsecond\r\n";

        write_atomic(&target, bytes, None).expect("write succeeds");

        assert_eq!(std::fs::read(&target).expect("read back"), bytes);
    }

    #[test]
    fn write_atomic_with_mode_sets_that_mode_before_rename_completes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("f.sh");

        write_atomic(&target, b"content", Some(0o640)).expect("write succeeds");

        assert_eq!(read_mode(&target), 0o640);
    }

    #[test]
    fn write_atomic_with_no_mode_preserves_an_existing_targets_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("f.sh");
        std::fs::write(&target, b"old").expect("write");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
            .expect("set_permissions");

        write_atomic(&target, b"content", None).expect("write succeeds");

        assert_eq!(read_mode(&target), 0o755);
    }

    #[test]
    fn write_atomic_with_no_mode_creates_a_new_file_at_the_umasked_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("f.txt");
        // `File::create` gets 0o666 minus the umask, the rule `NEW_FILE_MODE` claims.
        let reference = dir.path().join("reference.txt");
        std::fs::File::create(&reference).expect("create");

        write_atomic(&target, b"content", None).expect("write succeeds");

        assert_eq!(read_mode(&target), read_mode(&reference));
    }

    #[test]
    fn write_atomic_through_a_symlink_updates_the_referent_and_keeps_the_link() {
        let dir = tempfile::tempdir().expect("tempdir");
        let referent = dir.path().join("real.txt");
        std::fs::write(&referent, b"old").expect("write");
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&referent, &link).expect("symlink");

        write_atomic(&link, b"new", None).expect("write succeeds");

        assert_eq!(std::fs::read(&referent).expect("read referent"), b"new");
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("link still there")
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn a_failed_write_leaves_the_original_bytes_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("f.txt");
        std::fs::write(&target, b"original").expect("write");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500))
            .expect("make the directory unwritable");

        let result = write_atomic(&target, b"replacement", None);

        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("restore");
        assert!(matches!(result, Err(Error::Io { .. })));
        assert_eq!(std::fs::read(&target).expect("read back"), b"original");
    }

    #[test]
    fn check_before_write_refuses_a_hardlinked_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = dir.path().join("a.txt");
        let linked = dir.path().join("b.txt");
        std::fs::write(&original, b"content").expect("write");
        std::fs::hard_link(&original, &linked).expect("hard_link");

        let err = check_before_write(&original, 1024).expect_err("hardlink is refused");

        assert!(matches!(err, Error::Unsupported {
            reason: UnsupportedReason::Hardlink,
            ..
        }));
    }

    #[test]
    fn check_before_write_refuses_a_symlink_to_a_hardlinked_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = dir.path().join("a.txt");
        let linked = dir.path().join("b.txt");
        std::fs::write(&original, b"content").expect("write");
        std::fs::hard_link(&original, &linked).expect("hard_link");
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&original, &link).expect("symlink");

        let err = check_before_write(&link, 1024).expect_err("the referent is hardlinked");

        assert!(matches!(err, Error::Unsupported {
            reason: UnsupportedReason::Hardlink,
            ..
        }));
    }

    #[test]
    fn check_before_write_allows_a_single_link_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"content").expect("write");

        check_before_write(&path, 1024).expect("a plain file with one link is not refused");
    }

    #[test]
    fn check_before_write_refuses_a_file_one_byte_over_the_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, vec![0u8; 11]).expect("write");

        let err = check_before_write(&path, 10).expect_err("over the limit is refused");

        assert!(matches!(err, Error::Unsupported {
            reason: UnsupportedReason::TooLarge {
                bytes: 11,
                limit: 10,
            },
            ..
        }));
    }

    #[test]
    fn check_before_write_refuses_a_symlink_to_a_file_over_the_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, vec![0u8; 11]).expect("write");
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&path, &link).expect("symlink");

        let err = check_before_write(&link, 10).expect_err("the referent is over the limit");

        assert!(matches!(err, Error::Unsupported {
            reason: UnsupportedReason::TooLarge {
                bytes: 11,
                limit: 10,
            },
            ..
        }));
    }

    #[test]
    fn check_before_write_allows_a_file_exactly_at_the_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, vec![0u8; 10]).expect("write");

        check_before_write(&path, 10).expect("exactly at the limit is not refused");
    }

    #[test]
    fn check_before_write_allows_a_path_that_does_not_exist_yet() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("new.txt");

        check_before_write(&path, 10).expect("write creates files, so there is nothing to refuse");
    }

    #[test]
    fn resolve_symlink_on_a_symlink_returns_the_referents_canonical_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let referent = dir.path().join("real.txt");
        std::fs::write(&referent, b"content").expect("write");
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&referent, &link).expect("symlink");

        let resolved = resolve_symlink(&link).expect("resolves");

        assert_eq!(resolved, referent.canonicalize().expect("canonicalize"));
    }

    #[test]
    fn resolve_symlink_on_a_path_that_does_not_exist_yet_returns_it_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("new.txt");

        assert_eq!(resolve_symlink(&path).expect("resolves"), path);
    }

    #[test]
    fn resolve_symlink_on_a_plain_file_returns_it_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("real.txt");
        std::fs::write(&path, b"content").expect("write");

        assert_eq!(resolve_symlink(&path).expect("resolves"), path);
    }

    #[test]
    fn verify_if_returns_the_bytes_it_hashed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"content").expect("write");

        let bytes = verify_if(&path, &hash12(b"content")).expect("matching hash verifies");

        assert_eq!(bytes, b"content");
    }

    fn entries(dir: &Path) -> Vec<std::ffi::OsString> {
        let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(dir)
            .expect("directory readable")
            .map(|entry| entry.expect("entry readable").file_name())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn probe_writable_on_a_writable_parent_returns_ok_and_leaves_no_file_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("a.txt");
        std::fs::write(&target, b"content").expect("write");
        let before = entries(dir.path());

        probe_writable(&target, &target).expect("a writable parent probes ok");

        assert_eq!(entries(dir.path()), before);
    }

    #[test]
    fn probe_writable_on_an_unwritable_parent_fails_with_permission_denied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("a.txt");
        std::fs::write(&target, b"content").expect("write");
        let before = entries(dir.path());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500))
            .expect("make the directory unwritable");

        let err = probe_writable(&target, &target);

        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("restore");
        match err {
            Err(Error::Io { path, source }) => {
                assert_eq!(path, target);
                assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
            },
            other => panic!("expected Error::Io, got {other:?}"),
        }
        assert_eq!(entries(dir.path()), before);
    }

    #[test]
    fn probe_writable_on_a_symlink_whose_referents_parent_is_unwritable_fails() {
        let outer = tempfile::tempdir().expect("tempdir");
        let inner = outer.path().join("inner");
        std::fs::create_dir(&inner).expect("mkdir");
        let referent = inner.join("real.txt");
        std::fs::write(&referent, b"content").expect("write");
        let link = outer.path().join("link.txt");
        std::os::unix::fs::symlink(&referent, &link).expect("symlink");
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o500))
            .expect("make the referent's directory unwritable");

        let err = probe_writable(&link, &resolve_symlink(&link).expect("resolves"));

        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o700)).expect("restore");
        assert!(matches!(err, Err(Error::Io { .. })), "{err:?}");
    }

    #[test]
    fn verify_if_fails_naming_both_the_expected_and_the_actual_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"content").expect("write");
        let stale = hash12(b"content");
        std::fs::write(&path, b"changed").expect("mutate between the two calls");

        let err = verify_if(&path, &stale).expect_err("stale hash is refused");

        match err {
            Error::Changed {
                expected, actual, ..
            } => {
                assert_eq!(expected, hash12(b"content").as_str());
                assert_eq!(actual, hash12(b"changed").as_str());
            },
            other => panic!("expected Error::Changed, got {other:?}"),
        }
    }
}
