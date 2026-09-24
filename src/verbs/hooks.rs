use std::path::{Path, PathBuf};

use crate::cli::{HookTarget, HooksCmd};
use crate::error::Error;
use crate::install::pathguard;
use crate::install::settings::{self, HookEntry, InstallStatus};
use crate::output::{Body, Format, Response};
use crate::{Outcome, lock};

/// The earlier, shorter paragraph: Codex still gets it, having no Read/Edit tools to name, and
/// `is_guide` matches commands built from it so old installs upgrade in place.
pub(crate) const LEGACY_PARAGRAPH: &str = "For reading, finding and editing files, use \
    `lets` (run `lets guide` once) instead of `cat`,\n`grep` or `sed -n`. It reads several files \
    or ranges in one call, returns bounded numbered\noutput, and its edits return the changed \
    region — so do not follow a `lets` call with a `cat` or\n`sed -n` to check the result.\n";

/// Delivered only as a `SessionStart` print: as `--append-system-prompt-file` text it cut tool-call
/// batching to 0 of 192 model requests, against about 26% for plain Claude Code.
pub(crate) const CLAUDE_CODE_PARAGRAPH: &str = r#"# File work: use `lets` through Bash

`lets` is installed here. It is the dedicated tool for reading, searching and editing text files, and it runs through Bash, so it fits both "prefer the dedicated tool" and "work through Bash". Use it wherever you would otherwise reach for `cat`, `head`, `tail`, `sed -n`, `grep`, `rg`, `sed -i`, `cat > file`, or the Read tool on a text file.

Why it is worth the switch: one call covers several files or ranges, every line comes back numbered, the output is bounded, and the footer names anything it left out. An edit returns the changed lines with a parse check, and that output is the verification, so no follow-up read is needed.

| Instead of | Run |
|---|---|
| `cat a.ts`, `cat a.ts b.ts`, Read | `lets show a.ts b.ts` |
| `sed -n '40,80p' f.ts`, Read with an offset | `lets show f.ts:40-80` |
| reading a whole file to find one function | `lets show f.ts#computeFee` (a markdown section: `f.md#'Setup'`) |
| `grep -n -A 5 'pattern' f.ts` | `lets show "f.ts@'pattern'" -A 5` |
| `grep -rn 'x' src`, `rg x src` | `lets find 'x' src` (`-i`, `-w`, `-F`, `-C 3`, `--files`, `--count` work) |
| `sed -i 's/a/b/'`, the Edit tool | `lets edit f.ts --old 'a' --new 'b'` (`--old` is any exact substring that occurs once; `--all` for every match) |
| the same replacement in many files, e.g. a rename | `lets edit a.go b.go c.go --old 'oldName' --new 'newName' --all` |
| editing JSON, YAML or TOML | `lets transform package.json --set version=1.4.0 --append plugins=b` |
| `cat > new.ts <<'EOF'` | `lets write new.ts <<'EOF'` |

For a multi-line edit, or edits across several files, send one batch on stdin. Nothing inside it needs escaping, and either every edit lands or none does. Each `old` block is the shortest exact text that occurs once, not necessarily whole lines:

```
lets edit --from - <<'LETS'
@@ a.ts
<<<<<<< old
cap = 10
======= new
cap = 20
>>>>>>>
@@ b.ts insert-after @'^import'
======= new
import x from 'y'
>>>>>>>
LETS
```

`lets show` and `lets find` only read, so they are as safe to run in parallel as Read calls: when you need several reads or searches that do not depend on each other, send them as separate Bash calls in the same response, or name every file in one `lets show`. Each round trip re-reads the whole conversation, so fewer responses is what saves cost.

Each output line is a line number, a tab, then the file's line byte for byte. Run `lets` without `| head`, `| tail` or `2>/dev/null`: the output is already bounded, its last line names what was left out, and a failed call prints its fix on stderr, ending with an `ERROR_CODE=` line. Over 50 hits, `lets find` prints no hit lines but lists the files holding the most hits, so narrow the path to one of them or make the pattern more specific. When another program will parse the result, add `--json`.

Exit codes:
- 1: nothing matched. `edit` shows the nearest match and any indentation difference; `show` still prints the targets it did find.
- 2: `--old` matched more than once, and every candidate is listed as `path:line`. Lengthen `--old` or narrow the target to a line range such as `f.ts:40-40`.
- 3: the edit broke the file's syntax, so it was put back and the file is unchanged.
- 8: a batch partly landed, and the footer names which files changed.

Keep using Read for images and PDFs, and plain Bash for work that is not reading, searching or editing files: git, builds, tests, `ls`."#;

/// Codex's entry stays bare: whether Codex runs a hook command through a shell is unverified.
pub(crate) const CLASSIFY_COMMAND: &str = "lets hook classify";

/// Claude Code reports a hook command that cannot run as an error on every Bash call, so like the
/// `SessionStart` command this exits 0 and prints nothing while `lets` is missing.
const GUARDED_CLASSIFY_COMMAND: &str =
    "if command -v lets >/dev/null 2>&1; then lets hook classify; fi";

/// The same guard for an install that named `lets` by absolute path, which keeps that path.
fn classify_guarded_at(program: &str) -> String {
    let program = quote_path(program);
    format!("if [ -x {program} ]; then {program} hook classify; fi")
}

fn quote_path(path: &str) -> String {
    if path
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"/._+-".contains(&byte))
    {
        path.to_owned()
    } else {
        format!("'{}'", path.replace('\'', r"'\''"))
    }
}

/// The program any installed classify command runs, guarded or not.
fn classify_program(command: &str) -> Option<String> {
    if command == GUARDED_CLASSIFY_COMMAND {
        return Some("lets".to_owned());
    }
    if runs_lets_with(command, &["hook", "classify"]) {
        return command.split_whitespace().next().map(str::to_owned);
    }
    let (quoted, rest) = command.strip_prefix("if [ -x ")?.split_once(" ]; then ")?;
    if rest != format!("{quoted} hook classify; fi") {
        return None;
    }
    let program = match quoted
        .strip_prefix('\'')
        .and_then(|inner| inner.strip_suffix('\''))
    {
        Some(inner) => inner.replace(r"'\''", "'"),
        None => quoted.to_owned(),
    };
    let path = Path::new(&program);
    let named = quote_path(&program) == quoted
        && path.is_absolute()
        && path.file_name() == Some("lets".as_ref());
    named.then_some(program)
}

/// A path that is not an executable file keeps the guard false and the hook silently off, so
/// only an executable one is kept; any other falls back to the `PATH` lookup.
fn keep_absolute_path(existing: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt as _;
    classify_program(existing)
        .filter(|program| {
            Path::new(program).is_absolute()
                && std::fs::metadata(program).is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
        })
        .map(|program| classify_guarded_at(&program))
}

/// An earlier install's form, kept only so `is_guide` can upgrade it.
const GUIDE_ONLY_COMMAND: &str = "if command -v lets >/dev/null 2>&1; then lets guide; fi";

/// Claude Code shows any exit but 0 or 2 as a hook error, so a missing or mid-update `lets` exits
/// 0 and prints nothing.
fn session_start_command() -> String {
    let paragraph = CLAUDE_CODE_PARAGRAPH.replace('\'', r"'\''");
    format!("if command -v lets >/dev/null 2>&1; then printf '%s\\n' '{paragraph}'; fi")
}

/// Installed while a `--append-system-prompt-file` alias still existed.
fn legacy_env_guarded_guide_command() -> String {
    let paragraph = LEGACY_PARAGRAPH.trim_end().replace('\'', r"'\''");
    format!(
        "if command -v lets >/dev/null 2>&1; then [ -n \"${{LETS_SYSTEM_APPEND:-}}\" ] || printf \
         '%s\\n\\n' '{paragraph}'; lets guide; fi"
    )
}

fn legacy_unconditional_guide_command() -> String {
    let paragraph = LEGACY_PARAGRAPH.trim_end().replace('\'', r"'\''");
    format!(
        "if command -v lets >/dev/null 2>&1; then printf '%s\\n\\n' '{paragraph}'; lets guide; fi"
    )
}

/// Compaction drops injected context, so the guide is injected on every session start, not only
/// `startup`.
const SESSION_START_MATCHER: &str = "startup|resume|clear|compact|fork";

/// Claude Code takes `SubagentStart` context only from `hookSpecificOutput`, and `hookEventName` is
/// that object's required discriminator (code.claude.com/docs/en/hooks, `SubagentStart`).
const SUBAGENT_START_PREFIX: &str =
    r#"printf '%s' '{"hookSpecificOutput":{"hookEventName":"SubagentStart","additionalContext":"#;

fn claude_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude")
}

/// Empty counts as unset and a relative value is refused: either would resolve against the current
/// directory and drop `hooks.json` into that project.
fn codex_dir() -> Result<PathBuf, Error> {
    match std::env::var_os("CODEX_HOME") {
        Some(value) if !value.is_empty() => {
            let dir = PathBuf::from(value);
            if dir.is_absolute() {
                Ok(dir)
            } else {
                Err(Error::Usage {
                    message: format!(
                        "CODEX_HOME={} is a relative path \u{b7} set it to an absolute path, or \
                         unset it to install under ~/.codex",
                        dir.display()
                    ),
                })
            }
        },
        _ => Ok(PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".codex")),
    }
}

fn subagent_start_command() -> String {
    let context = serde_json::to_string(CLAUDE_CODE_PARAGRAPH).expect("a string always serialises");
    let tail = format!("{context}}}}}").replace('\'', r"'\''");
    format!("{SUBAGENT_START_PREFIX}{tail}'")
}

pub(crate) fn is_classify(command: &str) -> bool {
    classify_program(command).is_some()
}

/// Every earlier `SessionStart` form counts as ours, so an upgrade replaces it instead of adding a
/// second hook.
fn is_guide(command: &str) -> bool {
    command == session_start_command()
        || command == legacy_env_guarded_guide_command()
        || command == legacy_unconditional_guide_command()
        || command == GUIDE_ONLY_COMMAND
        || runs_lets_with(command, &["guide"])
}

fn runs_lets_with(command: &str, args: &[&str]) -> bool {
    let mut words = command.split_whitespace();
    words
        .next()
        .is_some_and(|program| Path::new(program).file_name() == Some("lets".as_ref()))
        && words.eq(args.iter().copied())
}

fn is_subagent_start(command: &str) -> bool {
    command.starts_with(SUBAGENT_START_PREFIX)
}

pub(crate) fn hook_line(format: Format, label: &str, status: &InstallStatus) -> String {
    match (format, status) {
        (Format::Text, InstallStatus::Installed) => format!("added the {label} hook"),
        (Format::Text, InstallStatus::Updated) => format!("updated the {label} hook"),
        (Format::Text, InstallStatus::AlreadyInstalled) => {
            format!("the {label} hook was already installed")
        },
        (Format::Json | Format::Jsonl, InstallStatus::Installed) => format!("{label}=installed"),
        (Format::Json | Format::Jsonl, InstallStatus::Updated) => format!("{label}=updated"),
        (Format::Json | Format::Jsonl, InstallStatus::AlreadyInstalled) => {
            format!("{label}=already_installed")
        },
    }
}

fn hook_lines(format: Format, statuses: &[InstallStatus]) -> [String; 3] {
    [
        hook_line(format, "PreToolUse", &statuses[0]),
        hook_line(format, "SubagentStart", &statuses[1]),
        hook_line(format, "SessionStart", &statuses[2]),
    ]
}

/// An earlier install's `system-append.md` is named, not deleted: a shell alias may still point
/// `--append-system-prompt-file` at it.
fn stale_system_append_note(format: Format, dir: &Path) -> Option<String> {
    dir.join("system-append.md").exists().then(|| {
        match format {
            Format::Text => {
                "~/.claude/system-append.md is no longer used \u{b7} remove any \
                              --append-system-prompt-file alias for it"
            },
            Format::Json | Format::Jsonl => "system_append=unused",
        }
        .to_owned()
    })
}

fn report(format: Format, statuses: &[InstallStatus], stale_note: Option<&str>) -> String {
    let [pre, sub, session] = hook_lines(format, statuses);
    let mut lines = vec![pre, sub, session];
    lines.extend(stale_note.map(str::to_owned));
    lines.join("\n") + "\n"
}

pub(crate) fn raw(text: String) -> Response {
    let mut response = Response::empty("hooks");
    response.body = Body::Raw {
        field: "hooks",
        text,
    };
    response
}

fn claude_code_entries<'a>(
    subagent_command: &'a str,
    session_command: &'a str,
) -> [HookEntry<'a>; 3] {
    [
        HookEntry {
            event: "PreToolUse",
            matcher: Some("Bash"),
            command: GUARDED_CLASSIFY_COMMAND,
            is_ours: is_classify,
            carry_over: keep_absolute_path,
        },
        HookEntry {
            event: "SubagentStart",
            matcher: None,
            command: subagent_command,
            is_ours: is_subagent_start,
            carry_over: settings::nothing_to_carry_over,
        },
        HookEntry {
            event: "SessionStart",
            matcher: Some(SESSION_START_MATCHER),
            command: session_command,
            is_ours: is_guide,
            carry_over: settings::nothing_to_carry_over,
        },
    ]
}

pub(crate) fn uninstall_report(format: Format, entries: &[HookEntry], removed: &[bool]) -> String {
    let lines: Vec<String> = entries
        .iter()
        .zip(removed)
        .filter(|(_, removed)| **removed)
        .map(|(entry, _)| match format {
            Format::Text => format!("removed the {} hook", entry.event),
            Format::Json | Format::Jsonl => format!("{}=removed", entry.event),
        })
        .collect();
    if lines.is_empty() {
        match format {
            Format::Text => "nothing to remove\n",
            Format::Json | Format::Jsonl => "nothing_to_remove\n",
        }
        .to_owned()
    } else {
        lines.join("\n") + "\n"
    }
}

fn install(format: Format, dir: &Path, path_var: &str, runtime: &Path) -> Outcome {
    if let Err(error) = pathguard::refuse_unless_resolved(path_var) {
        return Outcome::failed("hooks install", error);
    }
    if let Err(source) = std::fs::create_dir_all(dir) {
        return Outcome::failed("hooks install", Error::Io {
            path: dir.to_path_buf(),
            source,
        });
    }

    let settings_path = dir.join("settings.json");
    let subagent_command = subagent_start_command();
    let session_command = session_start_command();
    let merged = settings::merge_hook_entries(
        &settings_path,
        &claude_code_entries(&subagent_command, &session_command),
        runtime,
    );
    match merged {
        Ok(statuses) => Outcome::ok(raw(report(
            format,
            &statuses,
            stale_system_append_note(format, dir).as_deref(),
        ))),
        Err(error) => Outcome::failed("hooks install", error),
    }
}

/// No PATH check: removing the entries needs no working `lets`, and a machine where it broke is
/// where uninstalling matters most.
fn uninstall(format: Format, dir: &Path, runtime: &Path) -> Outcome {
    let subagent_command = subagent_start_command();
    let session_command = session_start_command();
    let entries = claude_code_entries(&subagent_command, &session_command);
    match settings::remove_hook_entries(&dir.join("settings.json"), &entries, runtime) {
        Ok(removed) => Outcome::ok(raw(uninstall_report(format, &entries, &removed))),
        Err(error) => Outcome::failed("hooks uninstall", error),
    }
}

pub fn run(cmd: &HooksCmd, format: Format) -> Outcome {
    match cmd {
        HooksCmd::Install {
            target: HookTarget::ClaudeCode,
        } => install(
            format,
            &claude_dir(),
            &std::env::var("PATH").unwrap_or_default(),
            &lock::runtime_dir(),
        ),
        HooksCmd::Install {
            target: HookTarget::Codex,
        } => match codex_dir() {
            Ok(dir) => crate::install::codex::install(
                format,
                &dir,
                &std::env::var("PATH").unwrap_or_default(),
                &lock::runtime_dir(),
            ),
            Err(error) => Outcome::failed("hooks install", error),
        },
        HooksCmd::Uninstall {
            target: HookTarget::ClaudeCode,
        } => uninstall(format, &claude_dir(), &lock::runtime_dir()),
        HooksCmd::Uninstall {
            target: HookTarget::Codex,
        } => match codex_dir() {
            Ok(dir) => crate::install::codex::uninstall(format, &dir, &lock::runtime_dir()),
            Err(error) => Outcome::failed("hooks uninstall", error),
        },
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::error::InstallRefusedReason;

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

        /// `this`: a `lets` that is the running binary; otherwise a different executable `lets`.
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

        fn install(&self, format: Format) -> Outcome {
            let path = std::env::join_paths(self.path_dirs.iter().map(TempDir::path)).unwrap();
            install(
                format,
                &self.home.path().join(".claude"),
                path.to_str().unwrap(),
                self.runtime.path(),
            )
        }

        fn uninstall(&self, format: Format) -> Outcome {
            uninstall(
                format,
                &self.home.path().join(".claude"),
                self.runtime.path(),
            )
        }

        fn claude(&self, name: &str) -> PathBuf {
            self.home.path().join(".claude").join(name)
        }

        fn settings_json(&self) -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(self.claude("settings.json")).unwrap())
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
    fn first_install_wires_all_three_hooks_and_writes_no_append_file() {
        let sandbox = Sandbox::new().with_lets_on_path(true);

        let outcome = sandbox.install(Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(!sandbox.claude("system-append.md").exists());
        let settings = sandbox.settings_json();
        assert_eq!(
            settings["hooks"]["PreToolUse"],
            serde_json::json!([
                {"matcher": "Bash", "hooks": [{
                    "type": "command",
                    "command": "if command -v lets >/dev/null 2>&1; then lets hook classify; fi"
                }]}
            ])
        );
        assert_eq!(
            settings["hooks"]["SubagentStart"],
            serde_json::json!([{"hooks": [{"type": "command", "command": subagent_start_command()}]}])
        );
        assert_eq!(
            settings["hooks"]["SessionStart"],
            serde_json::json!([{
                "matcher": "startup|resume|clear|compact|fork",
                "hooks": [{
                    "type": "command",
                    "command": session_start_command()
                }]
            }])
        );
        assert_eq!(
            body_text(&outcome),
            "added the PreToolUse hook\nadded the SubagentStart hook\nadded the SessionStart \
             hook\n"
        );
    }

    #[test]
    fn the_subagent_start_hook_prints_the_documented_output_shape_with_the_paragraph() {
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(subagent_start_command())
            .output()
            .unwrap();

        assert!(out.status.success());
        let printed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(
            printed,
            serde_json::json!({"hookSpecificOutput": {
                "hookEventName": "SubagentStart",
                "additionalContext": CLAUDE_CODE_PARAGRAPH,
            }})
        );
    }

    #[test]
    fn a_second_install_reports_every_hook_already_installed_with_identical_bytes() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        sandbox.install(Format::Text);
        let settings_before = std::fs::read(sandbox.claude("settings.json")).unwrap();

        let outcome = sandbox.install(Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(
            settings_before,
            std::fs::read(sandbox.claude("settings.json")).unwrap()
        );
        assert_eq!(
            body_text(&outcome),
            "the PreToolUse hook was already installed\nthe SubagentStart hook was already \
             installed\nthe SessionStart hook was already installed\n"
        );
    }

    #[test]
    fn a_stale_system_append_md_is_kept_and_named_as_unused() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        std::fs::create_dir_all(sandbox.claude("")).unwrap();
        std::fs::write(sandbox.claude("system-append.md"), "stale paragraph").unwrap();

        let outcome = sandbox.install(Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(
            std::fs::read_to_string(sandbox.claude("system-append.md")).unwrap(),
            "stale paragraph",
            "an old file is named as unused, never deleted or overwritten"
        );
        assert!(body_text(&outcome).ends_with(
            "~/.claude/system-append.md is no longer used \u{b7} remove any \
             --append-system-prompt-file alias for it\n"
        ));

        let json_outcome = sandbox.install(Format::Json);
        assert!(body_text(&json_outcome).ends_with("system_append=unused\n"));
    }

    #[test]
    fn no_system_append_md_means_no_stale_note() {
        let sandbox = Sandbox::new().with_lets_on_path(true);

        let outcome = sandbox.install(Format::Text);

        assert!(body_text(&outcome).ends_with("added the SessionStart hook\n"));
    }

    #[test]
    fn a_different_lets_first_on_path_refuses_naming_it_before_writing_anything() {
        let sandbox = Sandbox::new()
            .with_lets_on_path(false)
            .with_lets_on_path(true);
        let other = sandbox.path_dirs[0]
            .path()
            .join("lets")
            .canonicalize()
            .unwrap();

        let outcome = sandbox.install(Format::Text);

        match &outcome.error {
            Some(Error::InstallRefused {
                reason: InstallRefusedReason::DifferentLetsOnPath { found, .. },
            }) => assert_eq!(found, &other),
            other => panic!("expected path_conflict, got {other:?}"),
        }
        assert!(!sandbox.home.path().join(".claude").exists());
    }

    #[test]
    fn this_lets_first_on_path_installs_even_with_a_different_one_later() {
        let sandbox = Sandbox::new()
            .with_lets_on_path(true)
            .with_lets_on_path(false);

        let outcome = sandbox.install(Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(sandbox.claude("settings.json").is_file());
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
        assert!(!sandbox.home.path().join(".claude").exists());
    }

    #[test]
    fn a_malformed_settings_file_fails_and_writes_nothing() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        std::fs::create_dir_all(sandbox.claude("")).unwrap();
        std::fs::write(sandbox.claude("settings.json"), r#"{"hooks": null}"#).unwrap();

        let outcome = sandbox.install(Format::Text);

        assert!(matches!(outcome.error, Some(Error::Io { .. })));
        assert_eq!(
            std::fs::read_to_string(sandbox.claude("settings.json")).unwrap(),
            r#"{"hooks": null}"#
        );
    }

    #[test]
    fn json_format_reports_installed_then_already_installed_as_stable_tokens() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        let first = sandbox.install(Format::Json);
        assert_eq!(
            body_text(&first),
            "PreToolUse=installed\nSubagentStart=installed\nSessionStart=installed\n"
        );

        let outcome = sandbox.install(Format::Json);

        assert_eq!(
            body_text(&outcome),
            "PreToolUse=already_installed\nSubagentStart=already_installed\nSessionStart=\
             already_installed\n"
        );
    }

    #[test]
    fn only_a_lets_hook_classify_command_is_recognised_as_ours() {
        assert!(is_classify("lets hook classify"));
        assert!(is_classify("/usr/local/bin/lets hook classify"));
        assert!(!is_classify("echo lets hook classify disabled"));
        assert!(!is_classify("lets hook classify --verbose"));
        assert!(!is_classify("notlets hook classify"));
    }

    const GUARDED: &str = "if command -v lets >/dev/null 2>&1; then lets hook classify; fi";
    const GUARDED_AT_PATH: &str =
        "if [ -x /usr/local/bin/lets ]; then /usr/local/bin/lets hook classify; fi";

    #[test]
    fn both_guarded_classify_forms_are_recognised_as_ours_and_near_misses_are_not() {
        assert!(is_classify(GUARDED));
        assert!(is_classify(GUARDED_AT_PATH));
        assert!(is_classify(
            "if [ -x '/opt/my $dir/lets' ]; then '/opt/my $dir/lets' hook classify; fi"
        ));
        assert!(!is_classify(
            "if [ -x /usr/local/bin/lets ]; then /opt/bin/lets hook classify; fi"
        ));
        assert!(!is_classify(
            "if [ -x /usr/bin/notlets ]; then /usr/bin/notlets hook classify; fi"
        ));
        assert!(!is_classify(
            "if [ -x bin/lets ]; then bin/lets hook classify; fi"
        ));
        assert!(!is_classify(
            "if command -v lets >/dev/null 2>&1; then lets hook classify --verbose; fi"
        ));
        assert!(!is_guide(GUARDED));
    }

    fn settings_with_pre_tool_use(sandbox: &Sandbox, command: &str) {
        std::fs::create_dir_all(sandbox.claude("")).unwrap();
        let entry = serde_json::json!({
            "matcher": "Bash",
            "hooks": [{"type": "command", "command": command}]
        });
        std::fs::write(
            sandbox.claude("settings.json"),
            serde_json::json!({"hooks": {"PreToolUse": [entry]}}).to_string(),
        )
        .unwrap();
    }

    fn pre_tool_use_commands(sandbox: &Sandbox) -> Vec<String> {
        sandbox.settings_json()["hooks"]["PreToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|entry| entry["hooks"].as_array().unwrap().clone())
            .map(|hook| hook["command"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn installing_twice_over_the_unguarded_entry_leaves_one_guarded_entry() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        settings_with_pre_tool_use(&sandbox, "lets hook classify");

        let first = sandbox.install(Format::Text);
        let second = sandbox.install(Format::Text);

        assert!(body_text(&first).starts_with("updated the PreToolUse hook\n"));
        assert!(body_text(&second).starts_with("the PreToolUse hook was already installed\n"));
        assert_eq!(pre_tool_use_commands(&sandbox), [GUARDED]);
    }

    /// A `lets` outside `PATH` with `mode`, named by absolute path.
    fn lets_at(dir: &TempDir, mode: u32) -> String {
        use std::os::unix::fs::PermissionsExt as _;
        let lets = dir.path().join("lets");
        std::fs::write(&lets, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&lets, std::fs::Permissions::from_mode(mode)).unwrap();
        lets.to_str().unwrap().to_owned()
    }

    #[test]
    fn an_install_that_named_an_executable_absolute_path_keeps_it_inside_the_guard() {
        let dir = TempDir::new().unwrap();
        let program = lets_at(&dir, 0o755);
        let guarded_at = format!("if [ -x {program} ]; then {program} hook classify; fi");
        for existing in [format!("{program} hook classify"), guarded_at.clone()] {
            let sandbox = Sandbox::new().with_lets_on_path(true);
            settings_with_pre_tool_use(&sandbox, &existing);

            sandbox.install(Format::Text);
            let again = sandbox.install(Format::Text);

            assert_eq!(pre_tool_use_commands(&sandbox), [guarded_at.as_str()]);
            assert!(
                body_text(&again).starts_with("the PreToolUse hook was already installed\n"),
                "{existing}"
            );
        }
    }

    #[test]
    fn an_absolute_path_that_no_longer_runs_lets_is_replaced_by_the_path_lookup() {
        let dir = TempDir::new().unwrap();
        let gone = dir.path().join("gone/lets").to_str().unwrap().to_owned();
        let not_executable = lets_at(&dir, 0o644);
        for program in [gone, not_executable] {
            let unguarded = format!("{program} hook classify");
            let guarded_at = format!("if [ -x {program} ]; then {program} hook classify; fi");
            for existing in [unguarded, guarded_at] {
                let sandbox = Sandbox::new().with_lets_on_path(true);
                settings_with_pre_tool_use(&sandbox, &existing);

                let first = sandbox.install(Format::Text);

                assert_eq!(pre_tool_use_commands(&sandbox), [GUARDED], "{existing}");
                assert!(
                    body_text(&first).starts_with("updated the PreToolUse hook\n"),
                    "{existing}"
                );
            }
        }
    }

    #[test]
    fn an_install_that_named_lets_by_a_relative_path_gets_the_path_lookup_guard() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        settings_with_pre_tool_use(&sandbox, "bin/lets hook classify");

        sandbox.install(Format::Text);

        assert_eq!(pre_tool_use_commands(&sandbox), [GUARDED]);
    }

    #[test]
    fn uninstall_removes_the_unguarded_and_both_guarded_forms() {
        for existing in [
            "lets hook classify",
            "/usr/local/bin/lets hook classify",
            GUARDED,
            GUARDED_AT_PATH,
        ] {
            let sandbox = Sandbox::new();
            settings_with_pre_tool_use(&sandbox, existing);

            let outcome = sandbox.uninstall(Format::Text);

            assert_eq!(
                body_text(&outcome),
                "removed the PreToolUse hook\n",
                "{existing}"
            );
            assert_eq!(sandbox.settings_json(), serde_json::json!({}), "{existing}");
        }
    }

    fn run_guarded(command: &str, path: &std::path::Path) -> std::process::Output {
        use std::io::Write as _;
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .env("PATH", path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // A shell that never starts `lets` exits without reading, so the write may meet EPIPE.
        let _ = child.stdin.take().unwrap().write_all(b"{\"event\":1}");
        child.wait_with_output().unwrap()
    }

    #[test]
    fn a_guarded_classify_command_is_silent_and_exits_zero_when_lets_is_missing() {
        let empty = TempDir::new().unwrap();
        let missing_path = format!(
            "if [ -x {0}/lets ]; then {0}/lets hook classify; fi",
            empty.path().display()
        );

        for command in [GUARDED, missing_path.as_str()] {
            let output = run_guarded(command, empty.path());

            assert_eq!(output.status.code(), Some(0), "{command}");
            assert!(output.stdout.is_empty(), "{command}: {:?}", output.stdout);
            assert!(output.stderr.is_empty(), "{command}: {:?}", output.stderr);
        }
    }

    #[test]
    fn a_guarded_classify_command_hands_its_stdin_to_lets_hook_classify() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = TempDir::new().unwrap();
        let lets = dir.path().join("lets");
        // Builtins only: `PATH` holds nothing but this directory.
        std::fs::write(
            &lets,
            b"#!/bin/sh\necho \"$@\"\nIFS= read -r line\nprintf '%s' \"$line\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&lets, std::fs::Permissions::from_mode(0o755)).unwrap();
        let at_path = classify_guarded_at(lets.to_str().unwrap());

        for command in [GUARDED, at_path.as_str()] {
            let output = run_guarded(command, dir.path());

            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                "hook classify\n{\"event\":1}",
                "{command}"
            );
        }
    }

    #[test]
    fn every_earlier_installed_form_and_the_current_one_are_recognised_as_the_session_start_hook() {
        assert!(is_guide(&session_start_command()));
        assert!(is_guide(&legacy_env_guarded_guide_command()));
        assert!(is_guide(&legacy_unconditional_guide_command()));
        assert!(is_guide(GUIDE_ONLY_COMMAND));
        assert!(is_guide("lets guide"));
        assert!(is_guide("/usr/local/bin/lets guide"));
        assert!(!is_guide("lets guide --json"));
        assert!(!is_guide("echo lets guide"));
        assert!(!is_guide("lets hook classify"));
    }

    #[test]
    fn an_existing_session_start_hook_of_another_tool_is_kept_beside_ours() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        std::fs::create_dir_all(sandbox.claude("")).unwrap();
        let theirs = serde_json::json!({
            "matcher": "startup",
            "hooks": [{"type": "command", "command": "echo hello"}]
        });
        std::fs::write(
            sandbox.claude("settings.json"),
            serde_json::json!({"hooks": {"SessionStart": [theirs.clone()]}}).to_string(),
        )
        .unwrap();

        let outcome = sandbox.install(Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let session = &sandbox.settings_json()["hooks"]["SessionStart"];
        assert_eq!(session[0], theirs);
        assert_eq!(session[1]["hooks"][0]["command"], session_start_command());
        assert_eq!(session.as_array().map(Vec::len), Some(2));
    }

    fn assert_session_start_upgrades_to_the_current_command(old_command: &str) {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        std::fs::create_dir_all(sandbox.claude("")).unwrap();
        let old = serde_json::json!({
            "matcher": SESSION_START_MATCHER,
            "hooks": [{"type": "command", "command": old_command}]
        });
        std::fs::write(
            sandbox.claude("settings.json"),
            serde_json::json!({"hooks": {"SessionStart": [old]}}).to_string(),
        )
        .unwrap();

        let outcome = sandbox.install(Format::Text);

        assert!(body_text(&outcome).contains("updated the SessionStart hook"));
        let session = &sandbox.settings_json()["hooks"]["SessionStart"];
        assert_eq!(session.as_array().map(Vec::len), Some(1));
        assert_eq!(session[0]["hooks"][0]["command"], session_start_command());
    }

    #[test]
    fn a_bare_lets_guide_hook_is_updated_to_the_paragraph_only_command_not_duplicated() {
        assert_session_start_upgrades_to_the_current_command("lets guide");
    }

    #[test]
    fn a_guide_only_guarded_hook_is_updated_to_the_paragraph_only_command_not_duplicated() {
        assert_session_start_upgrades_to_the_current_command(GUIDE_ONLY_COMMAND);
    }

    #[test]
    fn a_legacy_env_guarded_guide_hook_is_updated_to_the_paragraph_only_command_not_duplicated() {
        assert_session_start_upgrades_to_the_current_command(&legacy_env_guarded_guide_command());
    }

    #[test]
    fn a_legacy_unconditional_guide_hook_is_updated_to_the_paragraph_only_command_not_duplicated() {
        assert_session_start_upgrades_to_the_current_command(&legacy_unconditional_guide_command());
    }

    #[test]
    fn uninstall_names_each_removed_hook_then_finds_nothing_the_second_time() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        sandbox.install(Format::Text);

        let first = sandbox.uninstall(Format::Text);
        let second = sandbox.uninstall(Format::Text);

        assert!(first.error.is_none(), "{:?}", first.error);
        assert_eq!(
            body_text(&first),
            "removed the PreToolUse hook\nremoved the SubagentStart hook\nremoved the SessionStart \
             hook\n"
        );
        assert_eq!(body_text(&second), "nothing to remove\n");
        assert_eq!(sandbox.settings_json(), serde_json::json!({}));
    }

    #[test]
    fn uninstall_reports_stable_tokens_in_json() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        sandbox.install(Format::Text);

        let first = sandbox.uninstall(Format::Json);
        let second = sandbox.uninstall(Format::Json);

        assert_eq!(
            body_text(&first),
            "PreToolUse=removed\nSubagentStart=removed\nSessionStart=removed\n"
        );
        assert_eq!(body_text(&second), "nothing_to_remove\n");
    }

    #[test]
    fn uninstall_with_no_settings_file_creates_none() {
        let sandbox = Sandbox::new();

        let outcome = sandbox.uninstall(Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(body_text(&outcome), "nothing to remove\n");
        assert!(!sandbox.claude("settings.json").exists());
    }

    #[test]
    fn uninstall_removes_every_earlier_session_start_form() {
        for old_command in [
            "lets guide".to_owned(),
            GUIDE_ONLY_COMMAND.to_owned(),
            legacy_env_guarded_guide_command(),
            legacy_unconditional_guide_command(),
        ] {
            let sandbox = Sandbox::new();
            std::fs::create_dir_all(sandbox.claude("")).unwrap();
            let old = serde_json::json!({
                "matcher": SESSION_START_MATCHER,
                "hooks": [{"type": "command", "command": old_command}]
            });
            std::fs::write(
                sandbox.claude("settings.json"),
                serde_json::json!({"hooks": {"SessionStart": [old]}}).to_string(),
            )
            .unwrap();

            let outcome = sandbox.uninstall(Format::Text);

            assert_eq!(
                body_text(&outcome),
                "removed the SessionStart hook\n",
                "{old_command}"
            );
            assert_eq!(sandbox.settings_json(), serde_json::json!({}));
        }
    }

    fn run_session_start_command(path: &std::path::Path) -> std::process::Output {
        std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(session_start_command())
            .env("PATH", path)
            .output()
            .unwrap()
    }

    #[test]
    fn the_session_start_command_exits_zero_silently_when_lets_is_not_on_path() {
        let empty = TempDir::new().unwrap();

        let output = run_session_start_command(empty.path());

        assert_eq!(output.status.code(), Some(0));
        assert!(output.stdout.is_empty(), "{:?}", output.stdout);
        assert!(output.stderr.is_empty(), "{:?}", output.stderr);
    }

    #[test]
    fn the_session_start_command_prints_exactly_the_paragraph_when_lets_is_on_path() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = TempDir::new().unwrap();
        let lets = dir.path().join("lets");
        // Exits 9: the command never runs `lets`, only checks that it exists.
        std::fs::write(&lets, b"#!/bin/sh\nexit 9\n").unwrap();
        std::fs::set_permissions(&lets, std::fs::Permissions::from_mode(0o755)).unwrap();

        let output = run_session_start_command(dir.path());

        let expected = format!("{CLAUDE_CODE_PARAGRAPH}\n");
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
        assert_eq!(output.stdout.len(), expected.len());
        assert_eq!(output.status.code(), Some(0));
    }

    #[test]
    fn claude_code_paragraph_is_agents_md_s_own_words() {
        let agents = include_str!("../../docs/agents.md");
        let unquoted: String = agents
            .lines()
            .map(|line| {
                line.strip_prefix("> ")
                    .unwrap_or_else(|| strip_bare_quote(line))
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(unquoted.contains(CLAUDE_CODE_PARAGRAPH));
    }

    /// A blank blockquote line is a bare `>`, with no trailing space.
    fn strip_bare_quote(line: &str) -> &str {
        line.strip_prefix('>').unwrap_or(line)
    }
}
