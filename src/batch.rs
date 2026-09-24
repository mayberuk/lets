use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::{atomic, lock};

#[derive(Debug)]
pub struct Plan<T> {
    pub path: PathBuf,
    pub before: Vec<u8>,
    pub after: Vec<u8>,
    pub mode: Option<u32>,
    pub detail: T,
}

#[derive(Debug)]
pub struct Locked<T> {
    pub locks: Vec<lock::Lock>,
    pub plans: Vec<Plan<T>>,
}

/// Releases every lock on `Err`. A failed lock lookup skips `validate_one`, so each entry is
/// matched by its own path, never by call order.
pub fn lock_and_validate<E, T>(
    entries: &[(PathBuf, E)],
    runtime: &Path,
    mut validate_one: impl FnMut(&Path, &E, &lock::Lock) -> Result<Plan<T>, Error>,
) -> Result<Locked<T>, Vec<Error>> {
    let paths: Vec<PathBuf> = entries.iter().map(|(path, _)| path.clone()).collect();
    let locks = lock::acquire_all(&paths, runtime).map_err(|err| vec![err])?;

    let mut plans = Vec::with_capacity(entries.len());
    let mut errors = Vec::new();
    for (target, entry) in entries {
        // Cannot fail on an unchanged tree, but a batch reports and unwinds rather than aborting.
        let held = lock::lock_path(target, runtime).and_then(|key| {
            locks
                .iter()
                .find(|held| held.path() == key.as_path())
                .ok_or_else(|| Error::Io {
                    path: target.clone(),
                    source: std::io::Error::other("no lock is held for this target"),
                })
        });
        match held.and_then(|held| validate_one(target, entry, held)) {
            Ok(plan) => plans.push(plan),
            Err(err) => errors.push(err),
        }
    }

    if errors.is_empty() {
        Ok(Locked { locks, plans })
    } else {
        Err(errors)
    }
}

pub fn write_in_order<T>(plans: &[Plan<T>]) -> Result<Vec<PathBuf>, Error> {
    let mut written = Vec::with_capacity(plans.len());
    for plan in plans {
        if let Err(err) = atomic::write_atomic(&plan.path, &plan.after, plan.mode) {
            return Err(Error::PartialBatch {
                written,
                failed: plan.path.clone(),
                detail: failure_kind(err),
            });
        }
        written.push(plan.path.clone());
    }
    Ok(written)
}

/// The io error names `tempfile`'s randomly named temp file, and stderr must be deterministic.
fn failure_kind(error: Error) -> String {
    match error {
        Error::Io { source, .. } => source.kind().to_string(),
        other => other.to_string(),
    }
}

/// Each landed path is in exactly one list: a restored file holds `before`, the rest `after`.
#[derive(Debug, Default)]
pub struct Reverted {
    pub restored: Vec<PathBuf>,
    pub not_restored: Vec<(PathBuf, String)>,
}

pub fn revert<T>(plans: &[Plan<T>], landed: &[PathBuf]) -> Reverted {
    let mut reverted = Reverted::default();
    for plan in plans.iter().filter(|plan| landed.contains(&plan.path)) {
        match atomic::write_atomic(&plan.path, &plan.before, plan.mode) {
            Ok(()) => reverted.restored.push(plan.path.clone()),
            Err(error) => reverted
                .not_restored
                .push((plan.path.clone(), failure_kind(error))),
        }
    }
    reverted
}

#[cfg(test)]
mod tests {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn plan(path: &Path, before: &[u8], after: &[u8]) -> Plan<()> {
        Plan {
            path: path.to_path_buf(),
            before: before.to_vec(),
            after: after.to_vec(),
            mode: None,
            detail: (),
        }
    }

    #[test]
    fn lock_and_validate_takes_one_lock_when_two_targets_resolve_to_the_same_canonical_path() {
        let runtime = tempfile::tempdir().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let direct = fixture.path().join("a.txt");
        std::fs::write(&direct, b"content").unwrap();
        std::fs::create_dir_all(fixture.path().join("sub")).unwrap();
        let indirect = fixture.path().join("sub").join("..").join("a.txt");

        let locked = lock_and_validate(
            &[(direct.clone(), ()), (indirect, ())],
            runtime.path(),
            |path, (), _| Ok(plan(path, b"content", b"content")),
        )
        .unwrap();

        assert_eq!(locked.locks.len(), 1);
        assert_eq!(locked.plans.len(), 2);
    }

    #[test]
    fn lock_and_validate_collects_every_validation_error_and_releases_every_lock() {
        let runtime = tempfile::tempdir().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let a = fixture.path().join("a.txt");
        let b = fixture.path().join("b.txt");
        let c = fixture.path().join("c.txt");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        std::fs::write(&c, b"c").unwrap();

        let entries = [(a.clone(), ()), (b.clone(), ()), (c.clone(), ())];
        let errors = lock_and_validate(&entries, runtime.path(), |path, (), _| {
            if path.to_path_buf() == a || path.to_path_buf() == c {
                Err(Error::Io {
                    path: path.to_path_buf(),
                    source: std::io::Error::other("boom"),
                })
            } else {
                Ok(plan(path, b"b", b"b"))
            }
        })
        .unwrap_err();

        assert_eq!(errors.len(), 2);

        let regained = lock::Lock::acquire(&a, runtime.path())
            .expect("lock on a was released after the failure");
        drop(regained);
    }

    #[test]
    fn lock_and_validate_on_full_success_keeps_every_lock_held() {
        let runtime = tempfile::tempdir().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let a = fixture.path().join("a.txt");
        let b = fixture.path().join("b.txt");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();

        let locked = lock_and_validate(
            &[(a.clone(), ()), (b.clone(), ())],
            runtime.path(),
            |path, (), _| Ok(plan(path, b"x", b"x")),
        )
        .unwrap();

        assert_eq!(locked.locks.len(), 2);

        let err = lock::Lock::acquire(&a, runtime.path()).unwrap_err();
        assert!(matches!(err, Error::Locked { .. }), "{err:?}");

        drop(locked);
        let regained =
            lock::Lock::acquire(&a, runtime.path()).expect("lock released once Locked is dropped");
        drop(regained);
    }

    // Deleting the second entry's directory makes its lock lookup fail.
    #[test]
    fn a_target_whose_lock_lookup_fails_does_not_shift_the_entries_after_it() {
        let runtime = tempfile::tempdir().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let dirs: Vec<PathBuf> = ["a", "b", "c"]
            .iter()
            .map(|name| {
                let dir = fixture.path().join(name);
                std::fs::create_dir(&dir).unwrap();
                dir
            })
            .collect();
        let paths: Vec<PathBuf> = dirs
            .iter()
            .map(|dir| {
                let path = dir.join("f.txt");
                std::fs::write(&path, b"x").unwrap();
                path
            })
            .collect();
        let entries: Vec<(PathBuf, &str)> = paths
            .iter()
            .cloned()
            .zip(["a-data", "b-data", "c-data"])
            .collect();

        let mut seen: Vec<(PathBuf, &str)> = Vec::new();
        let errors = lock_and_validate(&entries, runtime.path(), |path, entry, _| {
            seen.push((path.to_path_buf(), *entry));
            if path == paths[0].as_path() {
                std::fs::remove_dir_all(&dirs[1]).unwrap();
            }
            Ok(plan(path, b"x", b"x"))
        })
        .unwrap_err();

        assert_eq!(errors.len(), 1);
        assert_eq!(seen, vec![
            (paths[0].clone(), "a-data"),
            (paths[2].clone(), "c-data"),
        ]);
    }

    #[test]
    fn write_in_order_on_full_success_returns_every_path_that_landed_in_order() {
        let fixture = tempfile::tempdir().unwrap();
        let a = fixture.path().join("a.txt");
        let b = fixture.path().join("b.txt");
        std::fs::write(&a, b"old").unwrap();
        std::fs::write(&b, b"old").unwrap();

        let plans = vec![plan(&a, b"old", b"new-a"), plan(&b, b"old", b"new-b")];

        let landed = write_in_order(&plans).unwrap();

        assert_eq!(landed, vec![a.clone(), b.clone()]);
        assert_eq!(std::fs::read(&a).unwrap(), b"new-a");
        assert_eq!(std::fs::read(&b).unwrap(), b"new-b");
    }

    #[test]
    fn write_in_order_on_the_thirds_failure_names_the_first_two_written_and_the_third_failed() {
        let fixture = tempfile::tempdir().unwrap();
        let dirs: Vec<PathBuf> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| {
                let dir = fixture.path().join(name);
                std::fs::create_dir(&dir).unwrap();
                dir
            })
            .collect();
        let paths: Vec<PathBuf> = dirs
            .iter()
            .map(|dir| {
                let path = dir.join("f.txt");
                std::fs::write(&path, b"old").unwrap();
                path
            })
            .collect();
        std::fs::set_permissions(&dirs[2], Permissions::from_mode(0o500)).unwrap();

        let plans: Vec<Plan<()>> = paths
            .iter()
            .map(|path| plan(path, b"old", b"new"))
            .collect();

        let err = write_in_order(&plans).unwrap_err();
        let again = write_in_order(&plans).unwrap_err();

        std::fs::set_permissions(&dirs[2], Permissions::from_mode(0o700)).unwrap();

        match (err, again) {
            (
                Error::PartialBatch {
                    written,
                    failed,
                    detail,
                },
                Error::PartialBatch {
                    detail: detail_again,
                    ..
                },
            ) => {
                assert_eq!(written, paths[..2]);
                assert_eq!(failed, paths[2]);
                // `tempfile`'s random temp name must not reach stderr; the kind survives two runs.
                assert_eq!(detail, std::io::ErrorKind::PermissionDenied.to_string());
                assert_eq!(detail, detail_again);
            },
            (other, _) => panic!("expected PartialBatch, got {other:?}"),
        }
    }

    #[test]
    fn revert_restores_only_the_landed_paths_before_bytes() {
        let fixture = tempfile::tempdir().unwrap();
        let a = fixture.path().join("a.txt");
        let b = fixture.path().join("b.txt");
        let c = fixture.path().join("c.txt");
        std::fs::write(&a, b"after-a").unwrap();
        std::fs::write(&b, b"after-b").unwrap();
        std::fs::write(&c, b"after-c").unwrap();

        let plans = vec![
            plan(&a, b"before-a", b"after-a"),
            plan(&b, b"before-b", b"after-b"),
            plan(&c, b"before-c", b"after-c"),
        ];

        let reverted = revert(&plans, &[a.clone(), c.clone()]);

        assert!(reverted.not_restored.is_empty());
        assert_eq!(reverted.restored, vec![a.clone(), c.clone()]);
        assert_eq!(std::fs::read(&a).unwrap(), b"before-a");
        assert_eq!(std::fs::read(&b).unwrap(), b"after-b");
        assert_eq!(std::fs::read(&c).unwrap(), b"before-c");
    }

    #[test]
    fn revert_with_no_landed_paths_touches_nothing() {
        let fixture = tempfile::tempdir().unwrap();
        let a = fixture.path().join("a.txt");
        std::fs::write(&a, b"after-a").unwrap();
        let plans = vec![plan(&a, b"before-a", b"after-a")];

        let reverted = revert(&plans, &[]);

        assert!(reverted.restored.is_empty() && reverted.not_restored.is_empty());
        assert_eq!(std::fs::read(&a).unwrap(), b"after-a");
    }

    #[test]
    fn revert_names_the_path_it_could_not_restore_and_keeps_sweeping_the_rest() {
        let fixture = tempfile::tempdir().unwrap();
        let dir_a = fixture.path().join("a");
        let dir_b = fixture.path().join("b");
        std::fs::create_dir(&dir_a).unwrap();
        std::fs::create_dir(&dir_b).unwrap();
        let a = dir_a.join("f.txt");
        let b = dir_b.join("f.txt");
        std::fs::write(&a, b"after").unwrap();
        std::fs::write(&b, b"after").unwrap();
        std::fs::set_permissions(&dir_a, Permissions::from_mode(0o500)).unwrap();

        let plans = vec![plan(&a, b"before", b"after"), plan(&b, b"before", b"after")];

        let reverted = revert(&plans, &[a.clone(), b.clone()]);

        std::fs::set_permissions(&dir_a, Permissions::from_mode(0o700)).unwrap();

        assert_eq!(reverted.restored, vec![b.clone()]);
        assert_eq!(reverted.not_restored.len(), 1);
        assert_eq!(reverted.not_restored[0].0, a);
        assert_eq!(
            reverted.not_restored[0].1,
            std::io::ErrorKind::PermissionDenied.to_string()
        );
        assert_eq!(std::fs::read(&a).unwrap(), b"after");
        assert_eq!(std::fs::read(&b).unwrap(), b"before");
    }
}
