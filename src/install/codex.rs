use std::path::Path;

use crate::Outcome;
use crate::error::Error;
use crate::install::pathguard;
use crate::install::settings::{self, HookEntry, InstallStatus};
use crate::output::Format;
use crate::verbs::hooks::{
    CLASSIFY_COMMAND, LEGACY_PARAGRAPH, hook_line, is_classify, raw, uninstall_report,
};

const MANUAL_STEP_TEXT: &str = "add this to ~/.codex/AGENTS.md by hand";
const MANUAL_STEP_TOKEN: &str = "agents_md=manual";

// Whether `codex exec` fails open on an untrusted `hooks.json` was never exercised live.
const FAIL_OPEN_CAVEAT: &str = "codex exec's fail-open behavior for an untrusted hooks.json is \
    unverified \u{b7} trust the project, or pass --dangerously-bypass-hook-trust, to be sure the \
    hook runs";

const CLASSIFY_ENTRY: HookEntry<'static> = HookEntry {
    event: "PreToolUse",
    matcher: Some("Bash"),
    command: CLASSIFY_COMMAND,
    is_ours: is_classify,
};

fn report(format: Format, status: &InstallStatus) -> String {
    let manual_step = match format {
        Format::Text => MANUAL_STEP_TEXT,
        Format::Json | Format::Jsonl => MANUAL_STEP_TOKEN,
    };
    [
        hook_line(format, "PreToolUse", status),
        LEGACY_PARAGRAPH.trim_end().to_owned(),
        manual_step.to_owned(),
        FAIL_OPEN_CAVEAT.to_owned(),
    ]
    .join("\n")
        + "\n"
}

pub fn install(format: Format, dir: &Path, path_var: &str, runtime: &Path) -> Outcome {
    if let Err(error) = pathguard::refuse_unless_resolved(path_var) {
        return Outcome::failed("hooks install", error);
    }
    if let Err(source) = std::fs::create_dir_all(dir) {
        return Outcome::failed("hooks install", Error::Io {
            path: dir.to_path_buf(),
            source,
        });
    }

    let merged = settings::merge_hook_entries(&dir.join("hooks.json"), &[CLASSIFY_ENTRY], runtime);
    let status = match merged {
        Ok(statuses) => statuses.into_iter().next().expect("one entry was passed"),
        Err(error) => return Outcome::failed("hooks install", error),
    };
    Outcome::ok(raw(report(format, &status)))
}

pub fn uninstall(format: Format, dir: &Path, runtime: &Path) -> Outcome {
    let entries = [CLASSIFY_ENTRY];
    match settings::remove_hook_entries(&dir.join("hooks.json"), &entries, runtime) {
        Ok(removed) => Outcome::ok(raw(uninstall_report(format, &entries, &removed))),
        Err(error) => Outcome::failed("hooks uninstall", error),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::error::InstallRefusedReason;
    use crate::output::Body;

    struct Sandbox {
        home: TempDir,
        runtime: TempDir,
        path_dirs: Vec<TempDir>,
    }

    impl Sandbox {
        fn new() -> Sandbox {
            Sandbox {
                home: TempDir::new().unwrap(),
                runtime: TempDir::new().unwrap(),
                path_dirs: Vec::new(),
            }
        }

        fn with_lets_on_path(mut self, this: bool) -> Sandbox {
            use std::os::unix::fs::PermissionsExt as _;
            let dir = TempDir::new().unwrap();
            let lets = dir.path().join("lets");
            if this {
                std::os::unix::fs::symlink(std::env::current_exe().unwrap(), &lets).unwrap();
            } else {
                std::fs::write(&lets, b"#!/bin/sh\n").unwrap();
                std::fs::set_permissions(&lets, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            self.path_dirs.push(dir);
            self
        }

        fn codex_dir(&self) -> std::path::PathBuf {
            self.home.path().join(".codex")
        }

        fn install(&self, format: Format) -> Outcome {
            let path = std::env::join_paths(self.path_dirs.iter().map(TempDir::path)).unwrap();
            install(
                format,
                &self.codex_dir(),
                path.to_str().unwrap(),
                self.runtime.path(),
            )
        }

        fn uninstall(&self, format: Format) -> Outcome {
            uninstall(format, &self.codex_dir(), self.runtime.path())
        }

        fn hooks_json(&self) -> serde_json::Value {
            serde_json::from_str(
                &std::fs::read_to_string(self.codex_dir().join("hooks.json")).unwrap(),
            )
            .unwrap()
        }
    }

    fn body_text(outcome: &Outcome) -> &str {
        match &outcome.response.body {
            Body::Raw { text, .. } => text,
            other => panic!("expected a raw body, got {other:?}"),
        }
    }

    #[test]
    fn first_install_writes_the_pretooluse_entry_and_exits_ok() {
        let sandbox = Sandbox::new().with_lets_on_path(true);

        let outcome = sandbox.install(Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(
            sandbox.hooks_json(),
            serde_json::json!({"hooks": {"PreToolUse": [
                {"matcher": "Bash", "hooks": [{"type": "command", "command": "lets hook classify"}]}
            ]}})
        );
        let text = body_text(&outcome);
        assert!(text.starts_with("added the PreToolUse hook\n"));
        assert!(text.contains(LEGACY_PARAGRAPH.trim_end()));
        assert!(text.ends_with(&format!("{FAIL_OPEN_CAVEAT}\n")));
    }

    #[test]
    fn a_second_install_reports_already_installed_with_identical_bytes() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        sandbox.install(Format::Text);
        let before = std::fs::read(sandbox.codex_dir().join("hooks.json")).unwrap();

        let outcome = sandbox.install(Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(
            before,
            std::fs::read(sandbox.codex_dir().join("hooks.json")).unwrap()
        );
        assert!(body_text(&outcome).starts_with("the PreToolUse hook was already installed\n"));
    }

    #[test]
    fn a_different_lets_first_on_path_refuses_before_writing_anything() {
        let sandbox = Sandbox::new()
            .with_lets_on_path(false)
            .with_lets_on_path(true);

        let outcome = sandbox.install(Format::Text);

        assert!(matches!(
            outcome.error,
            Some(Error::InstallRefused {
                reason: InstallRefusedReason::DifferentLetsOnPath { .. }
            })
        ));
        assert!(!sandbox.codex_dir().join("hooks.json").exists());
    }

    #[test]
    fn this_lets_absent_from_path_refuses_before_writing_anything() {
        let mut sandbox = Sandbox::new();
        sandbox.path_dirs.push(TempDir::new().unwrap());

        let outcome = sandbox.install(Format::Text);

        assert!(matches!(
            outcome.error,
            Some(Error::InstallRefused {
                reason: InstallRefusedReason::NotOnPath { .. }
            })
        ));
        assert!(!sandbox.codex_dir().join("hooks.json").exists());
    }

    #[test]
    fn json_format_reports_installed_then_already_installed_as_stable_tokens() {
        let sandbox = Sandbox::new().with_lets_on_path(true);

        let first = sandbox.install(Format::Json);
        assert!(body_text(&first).starts_with("PreToolUse=installed\n"));
        assert!(body_text(&first).contains(MANUAL_STEP_TOKEN));

        let second = sandbox.install(Format::Json);
        assert!(body_text(&second).starts_with("PreToolUse=already_installed\n"));
    }

    #[test]
    fn an_unrelated_hooks_json_entry_is_kept_beside_ours() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        std::fs::create_dir_all(sandbox.codex_dir()).unwrap();
        std::fs::write(
            sandbox.codex_dir().join("hooks.json"),
            serde_json::json!({"hooks": {"PreToolUse": [
                {"matcher": "Write", "hooks": [{"type": "command", "command": "echo teammate"}]}
            ]}})
            .to_string(),
        )
        .unwrap();

        let outcome = sandbox.install(Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let entries = sandbox.hooks_json()["hooks"]["PreToolUse"].clone();
        assert_eq!(entries.as_array().map(Vec::len), Some(2));
        assert_eq!(entries[0]["hooks"][0]["command"], "echo teammate");
        assert_eq!(entries[1]["hooks"][0]["command"], "lets hook classify");
    }

    #[test]
    fn a_stale_classify_entry_at_an_old_path_is_updated_in_place_not_duplicated() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        std::fs::create_dir_all(sandbox.codex_dir()).unwrap();
        std::fs::write(
            sandbox.codex_dir().join("hooks.json"),
            serde_json::json!({"hooks": {"PreToolUse": [
                {"matcher": "Bash", "hooks": [
                    {"type": "command", "command": "/opt/old-release/bin/lets hook classify"}
                ]}
            ]}})
            .to_string(),
        )
        .unwrap();

        let outcome = sandbox.install(Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let entries = sandbox.hooks_json()["hooks"]["PreToolUse"].clone();
        assert_eq!(entries.as_array().map(Vec::len), Some(1));
        assert_eq!(entries[0]["hooks"][0]["command"], "lets hook classify");
        assert!(body_text(&outcome).starts_with("updated the PreToolUse hook\n"));
    }

    #[test]
    fn uninstall_removes_ours_keeps_a_teammate_s_entry_and_finds_nothing_the_second_time() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        std::fs::create_dir_all(sandbox.codex_dir()).unwrap();
        let existing = "{\n  \"hooks\": {\n    \"PreToolUse\": [\n      {\"matcher\": \"Write\", \
                        \"hooks\": [{\"type\": \"command\", \"command\": \"echo teammate\"}]}\n    \
                        ]\n  }\n}\n";
        std::fs::write(sandbox.codex_dir().join("hooks.json"), existing).unwrap();
        sandbox.install(Format::Text);

        let first = sandbox.uninstall(Format::Text);
        let second = sandbox.uninstall(Format::Text);

        assert!(first.error.is_none(), "{:?}", first.error);
        assert_eq!(body_text(&first), "removed the PreToolUse hook\n");
        assert_eq!(body_text(&second), "nothing to remove\n");
        assert_eq!(
            std::fs::read_to_string(sandbox.codex_dir().join("hooks.json")).unwrap(),
            existing
        );
    }

    #[test]
    fn uninstall_needs_no_lets_on_path_and_creates_no_file() {
        let sandbox = Sandbox::new();

        let outcome = sandbox.uninstall(Format::Json);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(body_text(&outcome), "nothing_to_remove\n");
        assert!(!sandbox.codex_dir().exists());
    }
}
