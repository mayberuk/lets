use std::fs::{File, Permissions, TryLockError};
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::{Error, UnsupportedReason};

/// A bounded wait, no queue: a lock still held after this exits `locked`.
const LOCK_WAIT: Duration = Duration::from_secs(2);
const LOCK_WAIT_INITIAL_BACKOFF: Duration = Duration::from_millis(10);
const LOCK_WAIT_MAX_BACKOFF: Duration = Duration::from_millis(200);

/// Owner-only: `TMPDIR` may be shared, and another account must not see which paths are locked.
const LOCKS_DIR_MODE: u32 = 0o700;

pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .or_else(|| std::env::var_os("TMPDIR"))
        .map_or_else(std::env::temp_dir, PathBuf::from)
}

/// Refuses a directory this user does not own, or another account decides where locks land.
fn prepare_locks_dir(runtime: &Path) -> Result<PathBuf, Error> {
    let dir = runtime.join("lets").join("locks");

    std::fs::create_dir_all(&dir).map_err(|source| unavailable(&dir, &source.to_string()))?;
    let metadata =
        std::fs::symlink_metadata(&dir).map_err(|source| unavailable(&dir, &source.to_string()))?;
    if metadata.file_type().is_symlink() {
        return Err(unavailable(&dir, "lock directory is a symlink"));
    }
    // A fresh file carries our uid; reading it directly would need a `libc` dependency.
    let probe = tempfile::NamedTempFile::new_in(&dir)
        .map_err(|source| unavailable(&dir, &source.to_string()))?;
    let own_uid = probe
        .as_file()
        .metadata()
        .map_err(|source| unavailable(&dir, &source.to_string()))?
        .uid();
    drop(probe);
    if metadata.uid() != own_uid {
        return Err(unavailable(&dir, "lock directory is owned by another user"));
    }
    std::fs::set_permissions(&dir, Permissions::from_mode(LOCKS_DIR_MODE))
        .map_err(|source| unavailable(&dir, &source.to_string()))?;
    Ok(dir)
}

// A target `write` will create does not exist yet, so its parent is canonicalized instead.
fn canonical_target(target: &Path) -> io::Result<PathBuf> {
    if let Ok(path) = std::fs::canonicalize(target) {
        return Ok(path);
    }
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no file name"))?;
    Ok(std::fs::canonicalize(parent)?.join(name))
}

pub fn lock_path(target: &Path, runtime: &Path) -> Result<PathBuf, Error> {
    let canonical = canonical_target(target).map_err(|source| Error::Io {
        path: target.to_path_buf(),
        source,
    })?;
    let digest = blake3::hash(canonical.as_os_str().as_bytes());
    Ok(prepare_locks_dir(runtime)?.join(digest.to_hex().to_string()))
}

#[derive(Debug)]
pub struct Lock {
    file: File,
    path: PathBuf,
}

impl Lock {
    pub fn acquire(target: &Path, runtime: &Path) -> Result<Lock, Error> {
        let path = lock_path(target, runtime)?;
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|source| unavailable(&path, &source.to_string()))?;
        wait_for_lock(file, path, target)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

// Sleep is capped to the time left, so one try always happens at the deadline itself.
fn wait_for_lock(file: File, path: PathBuf, target: &Path) -> Result<Lock, Error> {
    let start = Instant::now();
    let mut backoff = LOCK_WAIT_INITIAL_BACKOFF;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Lock { file, path }),
            Err(TryLockError::Error(source)) => {
                return Err(unavailable(target, &source.to_string()));
            },
            Err(TryLockError::WouldBlock) => {},
        }
        let elapsed = start.elapsed();
        if elapsed >= LOCK_WAIT {
            return Err(Error::Locked {
                path: target.to_path_buf(),
            });
        }
        let remaining = LOCK_WAIT.checked_sub(elapsed).unwrap_or(Duration::ZERO);
        std::thread::sleep(backoff.min(remaining));
        backoff = (backoff * 2).min(LOCK_WAIT_MAX_BACKOFF);
    }
}

fn unavailable(path: &Path, detail: &str) -> Error {
    Error::Unsupported {
        path: path.to_path_buf(),
        reason: UnsupportedReason::LockUnavailable {
            detail: detail.to_owned(),
        },
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Closing the fd would release it too; the explicit unlock does not rely on that.
        let _ = self.file.unlock();
    }
}

pub fn acquire_all(targets: &[PathBuf], runtime: &Path) -> Result<Vec<Lock>, Error> {
    let mut ordered = Vec::with_capacity(targets.len());
    for target in targets {
        let canonical = canonical_target(target).map_err(|source| Error::Io {
            path: target.clone(),
            source,
        })?;
        ordered.push((canonical, target));
    }
    ordered.sort_by(|a, b| a.0.cmp(&b.0));
    // Two paths to one file share a lock, and re-acquiring one this process holds `WouldBlock`s.
    ordered.dedup_by(|a, b| a.0 == b.0);

    let mut locks = Vec::with_capacity(ordered.len());
    for (_, target) in ordered {
        locks.push(Lock::acquire(target, runtime)?);
    }
    Ok(locks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unavailable_path(err: &Error) -> &Path {
        match err {
            Error::Unsupported {
                path,
                reason: UnsupportedReason::LockUnavailable { .. },
            } => path,
            other => panic!("expected LockUnavailable, got {other:?}"),
        }
    }

    fn unavailable_detail(err: &Error) -> &str {
        match err {
            Error::Unsupported {
                reason: UnsupportedReason::LockUnavailable { detail },
                ..
            } => detail,
            other => panic!("expected LockUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn lock_path_is_identity_for_the_same_file_reached_two_ways() {
        let runtime = tempfile::tempdir().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let direct = fixture.path().join("a.txt");
        std::fs::write(&direct, b"hi").unwrap();
        let indirect = fixture.path().join("sub").join("..").join("a.txt");
        std::fs::create_dir_all(fixture.path().join("sub")).unwrap();

        assert_eq!(
            lock_path(&direct, runtime.path()).unwrap(),
            lock_path(&indirect, runtime.path()).unwrap()
        );
    }

    #[test]
    fn lock_path_succeeds_for_a_not_yet_existing_file_under_an_existing_dir() {
        let runtime = tempfile::tempdir().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("new-file.txt");

        assert!(lock_path(&target, runtime.path()).is_ok());
    }

    #[test]
    fn the_lock_directory_is_created_owner_only() {
        let runtime = tempfile::tempdir().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("a.txt");
        std::fs::write(&target, b"hi").unwrap();

        let path = lock_path(&target, runtime.path()).unwrap();

        let mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn a_lock_directory_that_is_a_symlink_is_refused() {
        let runtime = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(runtime.path().join("lets")).unwrap();
        let locks = runtime.path().join("lets").join("locks");
        std::os::unix::fs::symlink(elsewhere.path(), &locks).unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("a.txt");
        std::fs::write(&target, b"hi").unwrap();

        let err = Lock::acquire(&target, runtime.path()).unwrap_err();

        assert_eq!(unavailable_path(&err), locks);
        assert_eq!(unavailable_detail(&err), "lock directory is a symlink");
    }

    #[test]
    fn a_read_only_runtime_dir_is_lock_unavailable_naming_the_lock_directory() {
        let runtime = tempfile::tempdir().unwrap();
        std::fs::set_permissions(runtime.path(), Permissions::from_mode(0o500)).unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("a.txt");
        std::fs::write(&target, b"hi").unwrap();

        let start = Instant::now();
        let err = Lock::acquire(&target, runtime.path()).unwrap_err();
        let elapsed = start.elapsed();

        assert_eq!(
            unavailable_path(&err),
            runtime.path().join("lets").join("locks")
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "a lock directory that cannot be created fails without waiting: {elapsed:?}"
        );
        std::fs::set_permissions(runtime.path(), Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn acquire_succeeds_on_an_unlocked_target() {
        let runtime = tempfile::tempdir().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("a.txt");
        std::fs::write(&target, b"hi").unwrap();

        assert!(Lock::acquire(&target, runtime.path()).is_ok());
    }

    #[test]
    fn second_acquire_on_a_target_held_for_the_whole_test_waits_the_full_2s_and_is_locked() {
        let runtime = tempfile::tempdir().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("a.txt");
        std::fs::write(&target, b"hi").unwrap();

        let held = Lock::acquire(&target, runtime.path()).unwrap();
        let start = Instant::now();
        let err = Lock::acquire(&target, runtime.path()).unwrap_err();
        let elapsed = start.elapsed();

        assert!(matches!(err, Error::Locked { .. }), "{err:?}");
        assert!(
            elapsed >= Duration::from_secs(2) && elapsed < Duration::from_secs(4),
            "{elapsed:?}"
        );
        drop(held);
    }

    #[test]
    fn a_lock_released_after_300ms_lets_a_waiting_acquire_succeed_no_sooner() {
        let runtime = tempfile::tempdir().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("a.txt");
        std::fs::write(&target, b"hi").unwrap();

        let held = Lock::acquire(&target, runtime.path()).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });

        let start = Instant::now();
        let acquired = Lock::acquire(&target, runtime.path()).unwrap();
        let elapsed = start.elapsed();

        releaser.join().unwrap();
        assert!(elapsed >= Duration::from_millis(300), "{elapsed:?}");
        drop(acquired);
    }

    #[test]
    fn dropping_a_lock_lets_a_later_acquire_on_the_same_target_succeed() {
        let runtime = tempfile::tempdir().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("a.txt");
        std::fs::write(&target, b"hi").unwrap();

        let held = Lock::acquire(&target, runtime.path()).unwrap();
        drop(held);

        assert!(Lock::acquire(&target, runtime.path()).is_ok());
    }

    #[test]
    fn acquire_all_locks_in_canonical_path_order_regardless_of_input_order() {
        let runtime = tempfile::tempdir().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let a = fixture.path().join("a.txt");
        let z = fixture.path().join("z.txt");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&z, b"z").unwrap();
        let canonical_a = std::fs::canonicalize(&a).unwrap();
        let canonical_z = std::fs::canonicalize(&z).unwrap();
        assert!(
            canonical_a < canonical_z,
            "fixture order must match the assertion below"
        );

        let locks = acquire_all(&[z.clone(), a.clone()], runtime.path()).unwrap();

        assert_eq!(locks[0].path(), lock_path(&a, runtime.path()).unwrap());
        assert_eq!(locks[1].path(), lock_path(&z, runtime.path()).unwrap());
    }

    #[test]
    fn acquire_all_takes_one_lock_when_the_same_path_is_named_twice() {
        let runtime = tempfile::tempdir().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let a = fixture.path().join("a.txt");
        std::fs::write(&a, b"a").unwrap();

        let locks = acquire_all(&[a.clone(), a.clone()], runtime.path()).unwrap();

        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].path(), lock_path(&a, runtime.path()).unwrap());
    }

    #[test]
    fn acquire_all_takes_one_lock_when_one_file_is_reached_two_ways() {
        let runtime = tempfile::tempdir().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let direct = fixture.path().join("a.txt");
        std::fs::write(&direct, b"a").unwrap();
        std::fs::create_dir_all(fixture.path().join("sub")).unwrap();
        let indirect = fixture.path().join("sub").join("..").join("a.txt");

        let locks = acquire_all(&[direct.clone(), indirect], runtime.path()).unwrap();

        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].path(), lock_path(&direct, runtime.path()).unwrap());
    }

    #[test]
    fn acquire_all_releases_the_locks_it_took_before_a_later_target_failed() {
        let runtime = tempfile::tempdir().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let a = fixture.path().join("a.txt");
        let z = fixture.path().join("z.txt");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&z, b"z").unwrap();
        let canonical_a = std::fs::canonicalize(&a).unwrap();
        let canonical_z = std::fs::canonicalize(&z).unwrap();
        assert!(
            canonical_a < canonical_z,
            "fixture order must match the pre-held target below"
        );

        // Pre-hold the last target: pre-holding the first would pass even if every lock leaked.
        let pre_held = Lock::acquire(&z, runtime.path()).unwrap();

        let err = acquire_all(&[z.clone(), a.clone()], runtime.path()).unwrap_err();
        assert!(matches!(err, Error::Locked { .. }), "{err:?}");

        let regained = Lock::acquire(&a, runtime.path())
            .expect("the lock taken before the failure was released");
        drop(regained);
        drop(pre_held);
    }
}
