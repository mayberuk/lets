use std::path::Path;

use crate::Outcome;
use crate::error::Error;
use crate::install::claude_mod;
use crate::install::release::{self, Version};
use crate::output::{Body, Format, Response, UpdateCheck};

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

fn update_text(format: Format) -> String {
    match format {
        Format::Text => "replaced with the latest release\n".to_owned(),
        Format::Json | Format::Jsonl => "done".to_owned(),
    }
}

fn mod_text(format: Format, changed: bool) -> &'static str {
    match (format, changed) {
        (Format::Text, true) => "rewrote the lets mod files\n",
        (Format::Text, false) => "the lets mod files were unchanged\n",
        (Format::Json | Format::Jsonl, true) => "\nmod=rewritten",
        (Format::Json | Format::Jsonl, false) => "\nmod=unchanged",
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
            &claude_mod::install_dir(),
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

/// Writes the mod files only, never `settings.json`: a settings change is left to an explicit
/// `hooks install`.
fn update(format: Format, force: bool, path_var: &str, curl: &Path, mod_dir: &Path) -> Outcome {
    match install_unless_current(force, path_var, curl) {
        Ok(Some(check)) => reported(check),
        Ok(None) => {
            let mut text = update_text(format);
            let rewritten = if mod_dir.exists() {
                claude_mod::write(mod_dir).map(|changed| text.push_str(mod_text(format, changed)))
            } else {
                Ok(())
            };
            let mut response = Response::empty("update");
            response.body = Body::Raw {
                field: "update",
                text,
            };
            match rewritten {
                Ok(()) => Outcome::ok(response),
                Err(error) => Outcome::partial(response, error),
            }
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

        fn mod_dir(&self) -> PathBuf {
            self.log.path().join("data/lets/claude-code")
        }

        fn with_stale_mod(self) -> FakeCurl {
            claude_mod::write(&self.mod_dir()).unwrap();
            std::fs::write(self.mod_dir().join("hooks/steer.ts"), "stale").unwrap();
            self
        }

        fn steer_ts(&self) -> String {
            std::fs::read_to_string(self.mod_dir().join("hooks/steer.ts")).unwrap()
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

        let outcome = update(
            Format::Text,
            false,
            fake.path_var(),
            &fake.curl(),
            &fake.mod_dir(),
        );

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

        let outcome = update(
            Format::Text,
            false,
            fake.path_var(),
            &fake.curl(),
            &fake.mod_dir(),
        );

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(body_check(&outcome).latest, "0.0.0");
        assert!(!fake.installer_ran());
    }

    #[test]
    fn update_when_a_newer_release_exists_downloads_and_runs_the_installer() {
        let fake = FakeCurl::new(&newer_than_current());

        let outcome = update(
            Format::Text,
            false,
            fake.path_var(),
            &fake.curl(),
            &fake.mod_dir(),
        );

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(fake.installer_ran());
        let calls = fake.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls[1].contains("-o"), "{calls:?}");
    }

    #[test]
    fn force_skips_the_check_and_downloads_even_when_current() {
        let fake = FakeCurl::new(&format!("v{}", env!("CARGO_PKG_VERSION")));

        let outcome = update(
            Format::Text,
            true,
            fake.path_var(),
            &fake.curl(),
            &fake.mod_dir(),
        );

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

        let outcome = update(
            Format::Text,
            false,
            fake.path_var(),
            &fake.curl(),
            &fake.mod_dir(),
        );

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

        let outcome = update(
            Format::Text,
            false,
            fake.path_var(),
            &fake.curl(),
            &fake.mod_dir(),
        );

        assert!(matches!(outcome.error, Some(Error::UpdateFailed { .. })));
        assert!(!fake.installer_ran());
    }

    fn body_text(outcome: &Outcome) -> &str {
        match &outcome.response.body {
            Body::Raw { text, .. } => text,
            other => panic!("expected a raw body, got {other:?}"),
        }
    }

    const EMBEDDED_STEER_TS: &str = include_str!("../../mod/hooks/steer.ts");

    #[test]
    fn a_successful_update_rewrites_an_installed_mod_and_says_so() {
        let fake = FakeCurl::new(&newer_than_current()).with_stale_mod();

        let outcome = update(
            Format::Text,
            false,
            fake.path_var(),
            &fake.curl(),
            &fake.mod_dir(),
        );

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(fake.installer_ran());
        assert_eq!(fake.steer_ts(), EMBEDDED_STEER_TS);
        assert_eq!(
            body_text(&outcome),
            "replaced with the latest release\nrewrote the lets mod files\n"
        );
    }

    #[test]
    fn a_successful_update_with_no_mod_installed_creates_none() {
        let fake = FakeCurl::new(&newer_than_current());

        let outcome = update(
            Format::Json,
            false,
            fake.path_var(),
            &fake.curl(),
            &fake.mod_dir(),
        );

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(fake.installer_ran());
        assert!(!fake.mod_dir().exists());
        assert!(!fake.log.path().join("data").exists());
        assert_eq!(body_text(&outcome), "done");
    }

    #[test]
    fn a_failed_update_leaves_the_mod_files_untouched() {
        let fake = FakeCurl::new("nightly").with_stale_mod();

        let outcome = update(
            Format::Text,
            false,
            fake.path_var(),
            &fake.curl(),
            &fake.mod_dir(),
        );

        assert!(matches!(outcome.error, Some(Error::UpdateFailed { .. })));
        assert!(!fake.installer_ran());
        assert_eq!(fake.steer_ts(), "stale");
    }

    #[test]
    fn an_update_skipped_as_current_leaves_the_mod_files_untouched() {
        let fake = FakeCurl::new(&format!("v{}", env!("CARGO_PKG_VERSION"))).with_stale_mod();

        let outcome = update(
            Format::Text,
            false,
            fake.path_var(),
            &fake.curl(),
            &fake.mod_dir(),
        );

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(!fake.installer_ran());
        assert_eq!(fake.steer_ts(), "stale");
    }
}
