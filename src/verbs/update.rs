use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::Outcome;
use crate::error::Error;
use crate::install::release::{self, Version};
use crate::install::{claude_mod, settings};
use crate::output::{Body, Format, Response, UpdateCheck};
use crate::verbs::hooks;

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

const REFRESH_COMMAND: &str = "lets hooks install claude-code";

fn update_text(format: Format) -> String {
    match format {
        Format::Text => "replaced with the latest release\n".to_owned(),
        Format::Json | Format::Jsonl => "done".to_owned(),
    }
}

fn mod_text(format: Format, refreshed: Result<String, String>) -> String {
    match (format, refreshed) {
        (Format::Text, Ok(relayed)) => format!("refreshed the lets mod\n{relayed}"),
        (Format::Text, Err(reason)) => format!(
            "the lets mod was not refreshed: {reason} \u{b7} run `{REFRESH_COMMAND}` to refresh it\n"
        ),
        (Format::Json | Format::Jsonl, Ok(relayed)) => {
            format!("\nmod=refreshed\n{}", relayed.trim_end())
        },
        (Format::Json | Format::Jsonl, Err(reason)) => {
            format!("\nmod=not_refreshed\nreason={reason}\nfix={REFRESH_COMMAND}")
        },
    }
}

pub fn run(check_only: bool, force: bool, format: Format) -> Outcome {
    if check_only {
        check(Path::new("curl"))
    } else {
        update(
            format,
            force,
            release::install_dir(&std::env::var("PATH").unwrap_or_default()),
            Path::new("curl"),
            &claude_mod::install_dir(),
            &hooks::claude_dir().join("settings.json"),
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

enum Updated {
    Current(UpdateCheck),
    Installed(PathBuf),
}

/// Local refusals come before the network, so a shadowed `lets` on PATH is named without waiting
/// on GitHub. A running build newer than the latest release is never downgraded.
fn install_unless_current(
    force: bool,
    install_dir: Result<PathBuf, Error>,
    curl: &Path,
) -> Result<Updated, Error> {
    let url = release::installer_url(REPOSITORY)?;
    release::target_triple()?;
    let install_dir = install_dir?;
    if !force {
        let check = compare(curl)?;
        if !check.update_available {
            return Ok(Updated::Current(check));
        }
    }
    release::download_and_install(&url, &install_dir, curl)?;
    Ok(Updated::Installed(install_dir.join("lets")))
}

/// Only a mod that settings still load is refreshed: a full install also writes the `PreToolUse`
/// hook and the plugin directory entry, and would put back one the user removed on purpose.
fn settings_load_mod(settings_path: &Path, mod_dir: &Path) -> Result<(), String> {
    let mod_dir = mod_dir
        .to_str()
        .ok_or_else(|| format!("{} is not valid UTF-8", mod_dir.display()))?;
    match settings::loads_plugin_dir_and_hook(settings_path, mod_dir, &hooks::PRE_TOOL_USE) {
        Ok(true) => Ok(()),
        Ok(false) => Err("Claude Code settings no longer load it".to_owned()),
        Err(error) => Err(format!("Claude Code settings could not be read: {error}")),
    }
}

/// This process embeds the old mod files, so the new binary rewrites them itself through
/// `hooks install`; writing them from here would leave the previous release's mod in place.
fn refresh_mod(
    binary: &Path,
    format: Format,
    settings_path: &Path,
    mod_dir: &Path,
) -> Result<String, String> {
    settings_load_mod(settings_path, mod_dir)?;
    let mut command = Command::new(binary);
    command.args(["hooks", "install", "claude-code"]);
    if !matches!(format, Format::Text) {
        command.arg("--json");
    }
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("could not run {}: {error}", binary.display()))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let first_line = stderr.lines().map(str::trim).find(|line| !line.is_empty());
    Err(match first_line {
        Some(line) => format!("{}: {line}", output.status),
        None => output.status.to_string(),
    })
}

fn update(
    format: Format,
    force: bool,
    install_dir: Result<PathBuf, Error>,
    curl: &Path,
    mod_dir: &Path,
    settings_path: &Path,
) -> Outcome {
    match install_unless_current(force, install_dir, curl) {
        Ok(Updated::Current(check)) => reported(check),
        Ok(Updated::Installed(binary)) => {
            let mut text = update_text(format);
            if claude_mod::is_ours(mod_dir) {
                text.push_str(&mod_text(
                    format,
                    refresh_mod(&binary, format, settings_path, mod_dir),
                ));
            }
            let mut response = Response::empty("update");
            response.body = Body::Raw {
                field: "update",
                text,
            };
            Outcome::ok(response)
        },
        Err(error) => Outcome::failed("update", error),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use tempfile::TempDir;

    use super::*;
    use crate::error::InstallRefusedReason;

    const MOD_FILES: [&str; 5] = [
        ".claude-plugin/plugin.json",
        "hooks/hooks.json",
        "hooks/register.ts",
        "hooks/steer.ts",
        "tsconfig.json",
    ];

    /// Logs every curl call's arguments to `calls`. The downloaded installer drops a stub `lets`
    /// into the install directory that records its own arguments to `stub-argv`.
    struct FakeCurl {
        bin: TempDir,
        log: TempDir,
    }

    impl FakeCurl {
        fn new(latest_tag: &str) -> FakeCurl {
            let bin = TempDir::new().unwrap();
            let log = TempDir::new().unwrap();
            let calls = log.path().join("calls");
            let curl = bin.path().join("curl");
            std::fs::write(
                &curl,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{calls}'\ncase \"$*\" in\n*-sSI*) \
                     printf 'location: https://github.com/mayberuk/lets/releases/tag/{latest_tag}\\r\\n' \
                     ;;\n*) while [ $# -gt 0 ]; do case \"$1\" in -o) out=\"$2\"; shift;; esac; \
                     shift; done; cp '{installer}' \"$out\" ;;\nesac\n",
                    calls = calls.display(),
                    installer = log.path().join("installer.sh").display(),
                ),
            )
            .unwrap();
            std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::create_dir(log.path().join("installed")).unwrap();
            let fake = FakeCurl { bin, log };
            fake.write_installer(&format!(
                "touch '{}'\ncp '{}' \"$LETS_UNMANAGED_INSTALL/lets\"\nchmod 755 \
                 \"$LETS_UNMANAGED_INSTALL/lets\"\n",
                fake.log.path().join("installer-ran").display(),
                fake.log.path().join("stub").display(),
            ));
            fake.write_stub(0);
            fake
        }

        fn write_installer(&self, script: &str) {
            std::fs::write(self.log.path().join("installer.sh"), script).unwrap();
        }

        fn write_stub(&self, exit: i32) {
            std::fs::write(
                self.log.path().join("stub"),
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\nprintf 'added the SessionStart \
                     hook\\n'\nprintf 'boom: no settings\\n' >&2\nexit {exit}\n",
                    self.log.path().join("stub-argv").display()
                ),
            )
            .unwrap();
        }

        fn failing_installer(self) -> FakeCurl {
            self.write_installer("exit 3\n");
            self
        }

        fn failing_stub(self) -> FakeCurl {
            self.write_stub(1);
            self
        }

        fn curl(&self) -> PathBuf {
            self.bin.path().join("curl")
        }

        fn path_var(&self) -> &str {
            self.bin.path().to_str().unwrap()
        }

        fn install_dir(&self) -> PathBuf {
            self.log.path().join("installed")
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

        fn stub_argv(&self) -> Option<String> {
            std::fs::read_to_string(self.log.path().join("stub-argv")).ok()
        }

        fn mod_dir(&self) -> PathBuf {
            self.log.path().join("data/lets/claude-code")
        }

        fn with_stale_mod(self) -> FakeCurl {
            claude_mod::write(&self.mod_dir()).unwrap();
            std::fs::write(self.mod_dir().join("hooks/steer.ts"), "stale").unwrap();
            self
        }

        fn settings_path(&self) -> PathBuf {
            self.log.path().join("home/.claude/settings.json")
        }

        fn write_settings(&self, plugin_dirs: Option<&str>, hook: bool) {
            let env = plugin_dirs.map_or_else(String::new, |dirs| {
                format!(r#""env": {{"CLAUDE_CODE_PLUGIN_DIRS": "{dirs}"}},"#)
            });
            let hooks = if hook {
                r#"{"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "lets hook classify"}]}]}"#
            } else {
                "{}"
            };
            let settings = self.settings_path();
            std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
            std::fs::write(settings, format!("{{{env} \"hooks\": {hooks}}}")).unwrap();
        }

        fn with_loaded_mod(self) -> FakeCurl {
            let fake = self.with_stale_mod();
            fake.write_settings(Some(fake.mod_dir().to_str().unwrap()), true);
            fake
        }

        fn mod_bytes(&self) -> Vec<Option<Vec<u8>>> {
            MOD_FILES
                .iter()
                .map(|relative| std::fs::read(self.mod_dir().join(relative)).ok())
                .collect()
        }

        fn run(&self, format: Format, force: bool) -> Outcome {
            update(
                format,
                force,
                Ok(self.install_dir()),
                &self.curl(),
                &self.mod_dir(),
                &self.settings_path(),
            )
        }
    }

    fn newer_than_current() -> String {
        let current = Version::current().to_string();
        let (major, rest) = current.split_once('.').unwrap();
        format!("v{}.{rest}", major.parse::<u64>().unwrap() + 1)
    }

    fn current_tag() -> String {
        format!("v{}", env!("CARGO_PKG_VERSION"))
    }

    fn body_check(outcome: &Outcome) -> &UpdateCheck {
        match &outcome.response.body {
            Body::Update(check) => check,
            other => panic!("expected an update check body, got {other:?}"),
        }
    }

    fn body_text(outcome: &Outcome) -> &str {
        match &outcome.response.body {
            Body::Raw { text, .. } => text,
            other => panic!("expected a raw body, got {other:?}"),
        }
    }

    #[test]
    fn check_reports_current_when_the_latest_tag_is_this_version() {
        let fake = FakeCurl::new(&current_tag());

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
        let fake = FakeCurl::new(&current_tag());

        let outcome = fake.run(Format::Text, false);

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

        let outcome = fake.run(Format::Text, false);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(body_check(&outcome).latest, "0.0.0");
        assert!(!fake.installer_ran());
    }

    #[test]
    fn update_when_a_newer_release_exists_downloads_and_runs_the_installer() {
        let fake = FakeCurl::new(&newer_than_current());

        let outcome = fake.run(Format::Text, false);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(fake.installer_ran());
        let calls = fake.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls[1].contains("-o"), "{calls:?}");
    }

    #[test]
    fn force_skips_the_check_and_downloads_even_when_current() {
        let fake = FakeCurl::new(&current_tag());

        let outcome = fake.run(Format::Text, true);

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
            release::install_dir(fake.path_var()),
            &fake.curl(),
            &fake.mod_dir(),
            &fake.settings_path(),
        );

        assert!(matches!(
            outcome.error,
            Some(Error::InstallRefused {
                reason: InstallRefusedReason::DifferentLetsOnPath { .. }
            })
        ));
        assert!(fake.calls().is_empty());
        assert!(!fake.installer_ran());
    }

    #[test]
    fn a_malformed_latest_tag_fails_the_update_without_downloading() {
        let fake = FakeCurl::new("nightly");

        let outcome = fake.run(Format::Text, false);

        assert!(matches!(outcome.error, Some(Error::UpdateFailed { .. })));
        assert!(!fake.installer_ran());
    }

    #[test]
    fn a_successful_update_runs_the_installed_binary_as_hooks_install_claude_code() {
        let fake = FakeCurl::new(&newer_than_current()).with_loaded_mod();

        let outcome = fake.run(Format::Text, false);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(fake.installer_ran());
        assert_eq!(
            fake.stub_argv().as_deref(),
            Some("hooks install claude-code\n")
        );
        assert_eq!(
            body_text(&outcome),
            "replaced with the latest release\nrefreshed the lets mod\nadded the SessionStart \
             hook\n"
        );
    }

    #[test]
    fn the_running_process_never_writes_the_mod_files_itself() {
        let fake = FakeCurl::new(&newer_than_current()).with_loaded_mod();
        let before = fake.mod_bytes();

        fake.run(Format::Text, false);

        assert_eq!(fake.mod_bytes(), before);
        assert_eq!(
            std::fs::read_to_string(fake.mod_dir().join("hooks/steer.ts")).unwrap(),
            "stale"
        );
    }

    #[test]
    fn a_successful_update_touches_no_settings_file() {
        let fake = FakeCurl::new(&newer_than_current()).with_loaded_mod();
        let before = std::fs::read(fake.settings_path()).unwrap();

        let outcome = fake.run(Format::Text, false);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(std::fs::read(fake.settings_path()).unwrap(), before);
    }

    #[test]
    fn json_names_the_refresh_as_a_stable_token() {
        let fake = FakeCurl::new(&newer_than_current()).with_loaded_mod();

        let outcome = fake.run(Format::Json, false);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(
            body_text(&outcome),
            "done\nmod=refreshed\nadded the SessionStart hook"
        );
        assert_eq!(
            fake.stub_argv().as_deref(),
            Some("hooks install claude-code --json\n")
        );
    }

    const NOT_LOADED: &str = "the lets mod was not refreshed: Claude Code settings no longer load \
                              it \u{b7} run `lets hooks install claude-code` to refresh it\n";

    #[test]
    fn a_plugin_dir_entry_the_user_removed_is_not_put_back() {
        let fake = FakeCurl::new(&newer_than_current()).with_stale_mod();
        fake.write_settings(None, true);

        let outcome = fake.run(Format::Text, false);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(fake.stub_argv().is_none());
        assert_eq!(
            body_text(&outcome),
            format!("replaced with the latest release\n{NOT_LOADED}")
        );
    }

    #[test]
    fn a_plugin_dir_entry_for_another_directory_is_not_ours_to_refresh() {
        let fake = FakeCurl::new(&newer_than_current()).with_stale_mod();
        fake.write_settings(Some("/elsewhere"), true);

        let outcome = fake.run(Format::Text, false);

        assert!(fake.stub_argv().is_none());
        assert!(body_text(&outcome).ends_with(NOT_LOADED));
    }

    #[test]
    fn a_pre_tool_use_hook_the_user_removed_is_not_put_back() {
        let fake = FakeCurl::new(&newer_than_current()).with_stale_mod();
        fake.write_settings(Some(fake.mod_dir().to_str().unwrap()), false);

        let outcome = fake.run(Format::Text, false);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(fake.stub_argv().is_none());
        assert_eq!(
            body_text(&outcome),
            format!("replaced with the latest release\n{NOT_LOADED}")
        );
    }

    #[test]
    fn a_missing_settings_file_runs_nothing_and_names_the_fix() {
        let fake = FakeCurl::new(&newer_than_current()).with_stale_mod();

        let outcome = fake.run(Format::Text, false);

        assert!(fake.stub_argv().is_none());
        assert!(body_text(&outcome).ends_with(NOT_LOADED));
    }

    #[test]
    fn unparsable_settings_run_nothing_and_name_the_fix() {
        let fake = FakeCurl::new(&newer_than_current()).with_stale_mod();
        std::fs::create_dir_all(fake.settings_path().parent().unwrap()).unwrap();
        std::fs::write(fake.settings_path(), "{ not json").unwrap();

        let outcome = fake.run(Format::Text, false);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(fake.stub_argv().is_none());
        let text = body_text(&outcome);
        assert!(text.contains("the lets mod was not refreshed"), "{text}");
        assert!(text.contains("`lets hooks install claude-code`"), "{text}");
    }

    #[test]
    fn a_settings_change_in_json_is_a_stable_not_refreshed_token() {
        let fake = FakeCurl::new(&newer_than_current()).with_stale_mod();
        fake.write_settings(None, false);

        let outcome = fake.run(Format::Json, false);

        assert!(fake.stub_argv().is_none());
        assert_eq!(
            body_text(&outcome),
            "done\nmod=not_refreshed\nreason=Claude Code settings no longer load it\nfix=lets \
             hooks install claude-code"
        );
    }

    #[test]
    fn a_successful_update_with_no_mod_installed_runs_nothing_and_creates_none() {
        let fake = FakeCurl::new(&newer_than_current());

        let outcome = fake.run(Format::Json, false);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(fake.installer_ran());
        assert!(fake.stub_argv().is_none());
        assert!(!fake.mod_dir().exists());
        assert!(!fake.log.path().join("data").exists());
        assert_eq!(body_text(&outcome), "done");
    }

    #[test]
    fn a_mod_directory_that_is_not_ours_is_left_alone_and_the_stub_does_not_run() {
        let fake = FakeCurl::new(&newer_than_current());
        let manifest = fake.mod_dir().join(".claude-plugin/plugin.json");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(&manifest, r#"{"name": "other"}"#).unwrap();

        let outcome = fake.run(Format::Text, false);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(fake.stub_argv().is_none());
        assert_eq!(body_text(&outcome), "replaced with the latest release\n");
    }

    #[test]
    fn an_empty_mod_directory_does_not_run_the_stub() {
        let fake = FakeCurl::new(&newer_than_current());
        std::fs::create_dir_all(fake.mod_dir()).unwrap();

        let outcome = fake.run(Format::Text, false);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(fake.stub_argv().is_none());
    }

    #[test]
    fn a_failing_refresh_keeps_the_update_successful_and_names_the_fix() {
        let fake = FakeCurl::new(&newer_than_current())
            .with_loaded_mod()
            .failing_stub();

        let outcome = fake.run(Format::Text, false);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(
            fake.stub_argv().as_deref(),
            Some("hooks install claude-code\n")
        );
        let text = body_text(&outcome);
        assert!(
            text.starts_with("replaced with the latest release\n"),
            "{text}"
        );
        assert!(text.contains("the lets mod was not refreshed"), "{text}");
        assert!(text.contains("boom: no settings"), "{text}");
        assert!(text.contains("`lets hooks install claude-code`"), "{text}");
        assert!(!text.contains("refreshed the lets mod\n"), "{text}");
    }

    #[test]
    fn a_failing_refresh_in_json_carries_the_fix_token() {
        let fake = FakeCurl::new(&newer_than_current())
            .with_loaded_mod()
            .failing_stub();

        let outcome = fake.run(Format::Json, false);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let text = body_text(&outcome);
        assert!(text.contains("mod=not_refreshed"), "{text}");
        assert!(
            text.contains("fix=lets hooks install claude-code"),
            "{text}"
        );
    }

    #[test]
    fn an_installer_that_exits_nonzero_never_runs_the_stub_and_leaves_every_mod_file_alone() {
        let fake = FakeCurl::new(&newer_than_current())
            .with_stale_mod()
            .failing_installer();
        let before = fake.mod_bytes();

        let outcome = fake.run(Format::Text, false);

        assert!(matches!(outcome.error, Some(Error::UpdateFailed { .. })));
        assert!(fake.stub_argv().is_none());
        assert!(!fake.installer_ran());
        assert_eq!(fake.mod_bytes(), before);
        assert!(before.iter().all(Option::is_some));
    }

    #[test]
    fn a_failed_update_leaves_the_mod_files_untouched() {
        let fake = FakeCurl::new("nightly").with_stale_mod();
        let before = fake.mod_bytes();

        let outcome = fake.run(Format::Text, false);

        assert!(matches!(outcome.error, Some(Error::UpdateFailed { .. })));
        assert!(fake.stub_argv().is_none());
        assert_eq!(fake.mod_bytes(), before);
    }

    #[test]
    fn an_update_skipped_as_current_leaves_the_mod_files_untouched() {
        let fake = FakeCurl::new(&current_tag()).with_stale_mod();
        let before = fake.mod_bytes();

        let outcome = fake.run(Format::Text, false);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(!fake.installer_ran());
        assert!(fake.stub_argv().is_none());
        assert_eq!(fake.mod_bytes(), before);
    }
}
