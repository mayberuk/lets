use std::path::Path;

use crate::Outcome;
use crate::error::Error;
use crate::install::release::{self, Version};
use crate::output::{Body, Format, Response, UpdateCheck};

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

fn update_text(format: Format) -> String {
    match format {
        Format::Text => "replaced with the latest release\n".to_owned(),
        Format::Json | Format::Jsonl => "done".to_owned(),
    }
}

pub fn run(check_only: bool, force: bool, format: Format) -> Outcome {
    if check_only {
        check(Path::new("curl"))
    } else {
        update(
            format,
            force,
            &std::env::var("PATH").unwrap_or_default(),
            Path::new("curl"),
        )
    }
}

fn compare(curl: &Path) -> Result<UpdateCheck, Error> {
    let current = Version::current();
    let latest = release::latest(REPOSITORY, curl)?;
    Ok(UpdateCheck {
        update_available: latest > current,
        current: current.to_string(),
        latest: latest.to_string(),
    })
}

fn reported(check: UpdateCheck) -> Outcome {
    let available = check.update_available.then(|| Error::UpdateAvailable {
        current: check.current.clone(),
        latest: check.latest.clone(),
    });
    let mut response = Response::empty("update");
    response.body = Body::Update(check);
    match available {
        Some(error) => Outcome::partial(response, error),
        None => Outcome::ok(response),
    }
}

fn check(curl: &Path) -> Outcome {
    match compare(curl) {
        Ok(check) => reported(check),
        Err(error) => Outcome::failed("update", error),
    }
}

/// Local refusals come before the network, so a shadowed `lets` on PATH is named without waiting
/// on GitHub. A running build newer than the latest release is never downgraded.
fn install_unless_current(
    force: bool,
    path_var: &str,
    curl: &Path,
) -> Result<Option<UpdateCheck>, Error> {
    let url = release::installer_url(REPOSITORY)?;
    release::target_triple()?;
    let install_dir = release::install_dir(path_var)?;
    if !force {
        let check = compare(curl)?;
        if !check.update_available {
            return Ok(Some(check));
        }
    }
    release::download_and_install(&url, &install_dir, curl)?;
    Ok(None)
}

fn update(format: Format, force: bool, path_var: &str, curl: &Path) -> Outcome {
    match install_unless_current(force, path_var, curl) {
        Ok(Some(check)) => reported(check),
        Ok(None) => {
            let mut response = Response::empty("update");
            response.body = Body::Raw {
                field: "update",
                text: update_text(format),
            };
            Outcome::ok(response)
        },
        Err(error) => Outcome::failed("update", error),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;
    use crate::error::InstallRefusedReason;

    /// Logs every call's arguments to `calls`, one per line.
    struct FakeCurl {
        bin: TempDir,
        log: TempDir,
    }

    impl FakeCurl {
        fn new(latest_tag: &str) -> FakeCurl {
            let bin = TempDir::new().unwrap();
            let log = TempDir::new().unwrap();
            let calls = log.path().join("calls");
            let ran = log.path().join("installer-ran");
            let curl = bin.path().join("curl");
            std::fs::write(
                &curl,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{calls}'\ncase \"$*\" in\n*-sSI*) \
                     printf 'location: https://github.com/mayberuk/lets/releases/tag/{latest_tag}\\r\\n' \
                     ;;\n*) while [ $# -gt 0 ]; do case \"$1\" in -o) out=\"$2\"; shift;; esac; \
                     shift; done; printf 'touch %s\\n' '{ran}' > \"$out\" ;;\nesac\n",
                    calls = calls.display(),
                    ran = ran.display(),
                ),
            )
            .unwrap();
            std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
            FakeCurl { bin, log }
        }

        fn curl(&self) -> PathBuf {
            self.bin.path().join("curl")
        }

        fn path_var(&self) -> &str {
            self.bin.path().to_str().unwrap()
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.log.path().join("calls"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        fn installer_ran(&self) -> bool {
            self.log.path().join("installer-ran").exists()
        }
    }

    fn newer_than_current() -> String {
        let current = Version::current().to_string();
        let (major, rest) = current.split_once('.').unwrap();
        format!("v{}.{rest}", major.parse::<u64>().unwrap() + 1)
    }

    fn body_check(outcome: &Outcome) -> &UpdateCheck {
        match &outcome.response.body {
            Body::Update(check) => check,
            other => panic!("expected an update check body, got {other:?}"),
        }
    }

    #[test]
    fn check_reports_current_when_the_latest_tag_is_this_version() {
        let fake = FakeCurl::new(&format!("v{}", env!("CARGO_PKG_VERSION")));

        let outcome = check(&fake.curl());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let reported = body_check(&outcome);
        assert!(!reported.update_available);
        assert_eq!(reported.current, env!("CARGO_PKG_VERSION"));
        assert_eq!(reported.latest, env!("CARGO_PKG_VERSION"));
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn check_reports_update_available_with_both_versions_when_the_latest_is_newer() {
        let newer = newer_than_current();
        let fake = FakeCurl::new(&newer);

        let outcome = check(&fake.curl());

        match &outcome.error {
            Some(Error::UpdateAvailable { current, latest }) => {
                assert_eq!(current, env!("CARGO_PKG_VERSION"));
                assert_eq!(latest, newer.trim_start_matches('v'));
            },
            other => panic!("expected update_available, got {other:?}"),
        }
        assert!(body_check(&outcome).update_available);
        assert!(!fake.installer_ran());
    }

    #[test]
    fn update_when_already_current_checks_and_does_not_download() {
        let fake = FakeCurl::new(&format!("v{}", env!("CARGO_PKG_VERSION")));

        let outcome = update(Format::Text, false, fake.path_var(), &fake.curl());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(!body_check(&outcome).update_available);
        let calls = fake.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].contains("-sSI"), "{calls:?}");
        assert!(!fake.installer_ran());
    }

    #[test]
    fn update_when_the_running_build_is_newer_than_the_latest_does_not_downgrade() {
        let fake = FakeCurl::new("v0.0.0");

        let outcome = update(Format::Text, false, fake.path_var(), &fake.curl());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(body_check(&outcome).latest, "0.0.0");
        assert!(!fake.installer_ran());
    }

    #[test]
    fn update_when_a_newer_release_exists_downloads_and_runs_the_installer() {
        let fake = FakeCurl::new(&newer_than_current());

        let outcome = update(Format::Text, false, fake.path_var(), &fake.curl());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(fake.installer_ran());
        let calls = fake.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls[1].contains("-o"), "{calls:?}");
    }

    #[test]
    fn force_skips_the_check_and_downloads_even_when_current() {
        let fake = FakeCurl::new(&format!("v{}", env!("CARGO_PKG_VERSION")));

        let outcome = update(Format::Text, true, fake.path_var(), &fake.curl());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(fake.installer_ran());
        let calls = fake.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(!calls[0].contains("-sSI"), "{calls:?}");
    }

    #[test]
    fn a_different_lets_first_on_path_is_refused_before_any_network_call() {
        let fake = FakeCurl::new(&newer_than_current());
        let other = fake.bin.path().join("lets");
        std::fs::write(&other, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o755)).unwrap();

        let outcome = update(Format::Text, false, fake.path_var(), &fake.curl());

        assert!(matches!(
            outcome.error,
            Some(Error::InstallRefused {
                reason: InstallRefusedReason::DifferentLetsOnPath { .. }
            })
        ));
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn a_malformed_latest_tag_fails_the_update_without_downloading() {
        let fake = FakeCurl::new("nightly");

        let outcome = update(Format::Text, false, fake.path_var(), &fake.curl());

        assert!(matches!(outcome.error, Some(Error::UpdateFailed { .. })));
        assert!(!fake.installer_ran());
    }
}
