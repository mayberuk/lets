use std::path::{Path, PathBuf};

use crate::cli::{HookTarget, HooksCmd};
use crate::error::Error;
use crate::install::settings::{self, HookEntry, InstallStatus};
use crate::install::{claude_mod, pathguard};
use crate::output::{Body, Format, Response};
use crate::{Outcome, lock};

/// The earlier, shorter paragraph: Codex still gets it, having no Read/Edit tools to name, and
/// `is_guide` matches commands built from it so old installs upgrade in place.
pub(crate) const LEGACY_PARAGRAPH: &str = "For reading, finding and editing files, use \
    `lets` (run `lets guide` once) instead of `cat`,\n`grep` or `sed -n`. It reads several files \
    or ranges in one call, returns bounded numbered\noutput, and its edits return the changed \
    region — so do not follow a `lets` call with a `cat` or\n`sed -n` to check the result.\n";

/// Delivered by the lets mod (`LETS_TABLE` in `mod/hooks/steer.ts`), never as
/// `--append-system-prompt-file` text: that cut tool-call batching to 0 of 192 model requests,
/// against about 26% for plain Claude Code.
pub(crate) const CLAUDE_CODE_PARAGRAPH: &str = r"# File work: use `lets` through Bash

| Instead of | Run |
|---|---|
| several `cat`/`sed -n`/`grep` calls, Read | `lets show a.ts b.ts:10-40 c.ts#computeFee` |
| `sed -i 's/a/b/'`, Edit | `lets edit f.ts --old a --new b` |
| edit JSON/YAML/TOML | `lets transform f.json --set version=1.4.0` |
| `cat > new.ts <<'EOF'` | `lets write new.ts <<'EOF'` |

Several edits and the build in one call; each `old` is exact text that occurs once:

```
lets edit --from - --check @auto <<'LETS'
@@ a.ts
<<<<<<< old
cap = 10
======= new
cap = 20
>>>>>>>
<<<<<<< old
floor = 1
======= new
floor = 2
>>>>>>>
LETS
```

For the first lines only, pass `--head N` to `lets show` or `lets find` instead of piping to `head`: the footer still names the cut. Do not add `2>/dev/null`: it hides the fix. Keep Read for images and PDFs; use plain Bash for anything else that is not reading, searching or editing files.";

/// Both harnesses report a hook command that cannot run as an error on every call it is bound to,
/// so like the `SessionStart` command this exits 0 and prints nothing while `lets` is missing,
/// crashing or mid-update.
pub(crate) const GUARDED_CLASSIFY_COMMAND: &str =
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

pub(crate) fn claude_dir() -> PathBuf {
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

/// The part of `session_start_command()` before the paragraph text: fixed across every release
/// that has shipped the heading, so matching on it (not the whole paragraph) lets a shortened or
/// reworded paragraph from an earlier install still upgrade in place instead of staying behind as
/// a second, foreign hook.
const SESSION_START_PARAGRAPH_PREFIX: &str =
    "if command -v lets >/dev/null 2>&1; then printf '%s\\n' '# File work: use `lets` through Bash";
const SESSION_START_PARAGRAPH_SUFFIX: &str = "'; fi";

/// Every earlier `SessionStart` form counts as ours, so an upgrade replaces it instead of adding a
/// second hook. The printed-paragraph form matches on the guard and heading only, not the full
/// paragraph text, since that text has changed between releases.
fn is_guide(command: &str) -> bool {
    (command.starts_with(SESSION_START_PARAGRAPH_PREFIX)
        && command.ends_with(SESSION_START_PARAGRAPH_SUFFIX))
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

fn mod_line(format: Format, status: &InstallStatus) -> String {
    match (format, status) {
        (Format::Text, InstallStatus::Installed) => "added the lets mod".to_owned(),
        (Format::Text, InstallStatus::Updated) => "updated the lets mod".to_owned(),
        (Format::Text, InstallStatus::AlreadyInstalled) => {
            "the lets mod was already installed".to_owned()
        },
        (Format::Json | Format::Jsonl, InstallStatus::Installed) => "mod=installed".to_owned(),
        (Format::Json | Format::Jsonl, InstallStatus::Updated) => "mod=updated".to_owned(),
        (Format::Json | Format::Jsonl, InstallStatus::AlreadyInstalled) => {
            "mod=already_installed".to_owned()
        },
    }
}

fn carried_by_the_mod_line(format: Format, event: &str) -> String {
    match format {
        Format::Text => {
            format!("removed the {event} hook \u{b7} the lets mod carries its text now")
        },
        Format::Json | Format::Jsonl => format!("{event}=removed"),
    }
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

/// What an install has changed so far, so a failure part-way still names everything that landed.
#[derive(Default)]
struct Landed {
    pre_tool_use: Option<InstallStatus>,
    lets_mod: Option<InstallStatus>,
    session_start_removed: bool,
    subagent_start_removed: bool,
    post_tool_use_removed: bool,
}

impl Landed {
    fn report(&self, format: Format, dir: &Path) -> String {
        let mut lines = Vec::new();
        lines.extend(
            self.pre_tool_use
                .as_ref()
                .map(|status| hook_line(format, "PreToolUse", status)),
        );
        lines.extend(
            self.lets_mod
                .as_ref()
                .map(|status| mod_line(format, status)),
        );
        if self.session_start_removed {
            lines.push(carried_by_the_mod_line(format, "SessionStart"));
        }
        if self.subagent_start_removed {
            lines.push(carried_by_the_mod_line(format, "SubagentStart"));
        }
        lines.extend(retired_post_tool_use_note(
            format,
            self.post_tool_use_removed,
        ));
        lines.extend(stale_system_append_note(format, dir));
        lines.join("\n") + "\n"
    }

    fn is_empty(&self) -> bool {
        self.pre_tool_use.is_none()
            && self.lets_mod.is_none()
            && !self.session_start_removed
            && !self.subagent_start_removed
            && !self.post_tool_use_removed
    }

    fn failed(&self, format: Format, dir: &Path, error: Error) -> Outcome {
        if self.is_empty() {
            Outcome::failed("hooks install", error)
        } else {
            Outcome::partial(raw(self.report(format, dir)), error)
        }
    }
}

pub(crate) fn raw(text: String) -> Response {
    let mut response = Response::empty("hooks");
    response.body = Body::Raw {
        field: "hooks",
        text,
    };
    response
}

pub(crate) const PRE_TOOL_USE: HookEntry<'static> = HookEntry {
    event: "PreToolUse",
    matcher: Some("Bash"),
    command: GUARDED_CLASSIFY_COMMAND,
    is_ours: is_classify,
    carry_over: keep_absolute_path,
};

/// The lets mod carries the paragraph now; kept only so `install` and `uninstall` remove every
/// form an earlier build left behind.
fn retired_session_start(command: &str) -> HookEntry<'_> {
    HookEntry {
        event: "SessionStart",
        matcher: Some(SESSION_START_MATCHER),
        command,
        is_ours: is_guide,
        carry_over: settings::nothing_to_carry_over,
    }
}

fn retired_subagent_start(command: &str) -> HookEntry<'_> {
    HookEntry {
        event: "SubagentStart",
        matcher: None,
        command,
        is_ours: is_subagent_start,
        carry_over: settings::nothing_to_carry_over,
    }
}

/// A paid trial found the `PostToolUse` check cost +0.86% [-3.46%, +3.01%] versus a model that
/// still ran its own build in 21 of 23 hook-on sessions anyway, so a fresh install skips it. Kept
/// only so `install` and `uninstall` can remove one an earlier build left behind.
pub(crate) const RETIRED_POST_TOOL_USE: HookEntry<'static> = HookEntry {
    event: "PostToolUse",
    matcher: Some("Edit|Write"),
    command: GUARDED_CLASSIFY_COMMAND,
    is_ours: is_classify,
    carry_over: keep_absolute_path,
};

/// `removed`: whether an earlier build's retired `PostToolUse` entry was just deleted.
pub(crate) fn retired_post_tool_use_note(format: Format, removed: bool) -> Option<String> {
    removed.then(|| match format {
        Format::Text => "removed the PostToolUse hook \u{b7} the check trial showed no benefit, \
                          so it is no longer installed by default"
            .to_owned(),
        Format::Json | Format::Jsonl => "PostToolUse=removed".to_owned(),
    })
}

fn removed_lines(format: Format, entries: &[HookEntry], removed: &[bool]) -> Vec<String> {
    entries
        .iter()
        .zip(removed)
        .filter(|(_, removed)| **removed)
        .map(|(entry, _)| match format {
            Format::Text => format!("removed the {} hook", entry.event),
            Format::Json | Format::Jsonl => format!("{}=removed", entry.event),
        })
        .collect()
}

fn nothing_or(format: Format, lines: &[String]) -> String {
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

pub(crate) fn uninstall_report(format: Format, entries: &[HookEntry], removed: &[bool]) -> String {
    nothing_or(format, &removed_lines(format, entries, removed))
}

/// Settings take a `:`-joined string, which a non-UTF-8 path cannot be written into faithfully.
fn mod_dir_text(mod_dir: &Path) -> Result<&str, Error> {
    mod_dir.to_str().ok_or_else(|| Error::Usage {
        message: format!(
            "the lets mod directory {} is not valid UTF-8 \u{b7} set XDG_DATA_HOME to a UTF-8 \
             absolute path",
            mod_dir.display()
        ),
    })
}

fn mod_partial(format: Format, written: Vec<PathBuf>, failed: PathBuf, detail: String) -> Outcome {
    let mut lines: Vec<String> = written
        .iter()
        .map(|path| match format {
            Format::Text => format!("wrote the lets mod file {}", path.display()),
            Format::Json | Format::Jsonl => format!("mod_file_written={}", path.display()),
        })
        .collect();
    match format {
        Format::Text => {
            lines.push(format!(
                "could not write the lets mod file {} \u{b7} {detail}",
                failed.display()
            ));
            lines.push("settings.json was left untouched".to_owned());
        },
        Format::Json | Format::Jsonl => {
            lines.push(format!("mod_file_failed={}", failed.display()));
            lines.push(format!("mod_file_error={detail}"));
            lines.push("settings=untouched".to_owned());
        },
    }
    let error = Error::PartialBatch {
        written,
        failed,
        detail,
    };
    Outcome::partial(raw(lines.join("\n") + "\n"), error)
}

/// The settings shape is checked before the mod files are written, so a settings file the install
/// would refuse leaves both untouched.
fn install(format: Format, dir: &Path, mod_dir: &Path, path_var: &str, runtime: &Path) -> Outcome {
    if let Err(error) = pathguard::refuse_unless_resolved(path_var) {
        return Outcome::failed("hooks install", error);
    }
    let settings_path = dir.join("settings.json");
    let checked = mod_dir_text(mod_dir).and_then(|text| {
        settings::check_shape(&settings_path)?;
        Ok(text)
    });
    let mod_dir_text = match checked {
        Ok(text) => text,
        Err(error) => return Outcome::failed("hooks install", error),
    };
    if let Err(source) = std::fs::create_dir_all(dir) {
        return Outcome::failed("hooks install", Error::Io {
            path: dir.to_path_buf(),
            source,
        });
    }

    let files_before = claude_mod::any_file_present(mod_dir);
    let files_changed = match claude_mod::write(mod_dir) {
        Ok(changed) => changed,
        Err(Error::PartialBatch {
            written,
            failed,
            detail,
        }) => return mod_partial(format, written, failed, detail),
        Err(error) => return Outcome::failed("hooks install", error),
    };
    let mut landed = Landed {
        lets_mod: files_changed.then_some(if files_before {
            InstallStatus::Updated
        } else {
            InstallStatus::Installed
        }),
        ..Landed::default()
    };

    match settings::merge_hook_entries(&settings_path, &[PRE_TOOL_USE], runtime) {
        Ok(statuses) => landed.pre_tool_use = statuses.into_iter().next(),
        Err(error) => return landed.failed(format, dir, error),
    }
    let session_command = session_start_command();
    let subagent_command = subagent_start_command();
    let retired = [
        retired_session_start(&session_command),
        retired_subagent_start(&subagent_command),
        RETIRED_POST_TOOL_USE,
    ];
    match settings::remove_hook_entries(&settings_path, &retired, runtime) {
        Ok(removed) => {
            landed.session_start_removed = removed[0];
            landed.subagent_start_removed = removed[1];
            landed.post_tool_use_removed = removed[2];
        },
        Err(error) => return landed.failed(format, dir, error),
    }
    match settings::merge_plugin_dir(&settings_path, mod_dir_text, runtime) {
        Ok(entry_added) => {
            landed.lets_mod = Some(if !files_before && entry_added {
                InstallStatus::Installed
            } else if !files_changed && !entry_added {
                InstallStatus::AlreadyInstalled
            } else {
                InstallStatus::Updated
            });
        },
        Err(error) => return landed.failed(format, dir, error),
    }
    Outcome::ok(raw(landed.report(format, dir)))
}

/// No PATH check: removing the entries needs no working `lets`, and a machine where it broke is
/// where uninstalling matters most.
fn uninstall(format: Format, dir: &Path, mod_dir: &Path, runtime: &Path) -> Outcome {
    let settings_path = dir.join("settings.json");
    let session_command = session_start_command();
    let subagent_command = subagent_start_command();
    let entries = [
        PRE_TOOL_USE,
        retired_session_start(&session_command),
        retired_subagent_start(&subagent_command),
        RETIRED_POST_TOOL_USE,
    ];
    let mut lines = match settings::remove_hook_entries(&settings_path, &entries, runtime) {
        Ok(removed) => removed_lines(format, &entries, &removed),
        Err(error) => return Outcome::failed("hooks uninstall", error),
    };
    let failed = |lines: &[String], error: Error| {
        if lines.is_empty() {
            Outcome::failed("hooks uninstall", error)
        } else {
            Outcome::partial(raw(nothing_or(format, lines)), error)
        }
    };
    let line = |text: &str, token: &str| match format {
        Format::Text => text.to_owned(),
        Format::Json | Format::Jsonl => token.to_owned(),
    };

    let mod_dir_entry = mod_dir.to_str().unwrap_or_default();
    let removed_entries = settings::remove_plugin_dirs_where(&settings_path, runtime, |entry| {
        (!mod_dir_entry.is_empty() && settings::same_dir(entry, mod_dir_entry))
            || (Path::new(entry).is_absolute() && claude_mod::is_ours(Path::new(entry)))
    });
    let removed_entries = match removed_entries {
        Ok(removed) => removed,
        Err(error) => {
            if !lines.is_empty() {
                lines.push(line("the lets mod was not removed", "mod=remaining"));
            }
            return failed(&lines, error);
        },
    };

    let mut tree_removed = false;
    let mut left: Vec<&str> = Vec::new();
    let trees = std::iter::once(mod_dir_entry)
        .filter(|tree| !tree.is_empty())
        .chain(removed_entries.iter().map(String::as_str));
    for tree in trees {
        let tree_path = Path::new(tree);
        let deletable = (!mod_dir_entry.is_empty() && settings::same_dir(tree, mod_dir_entry))
            || claude_mod::in_installer_layout(tree_path);
        if !deletable {
            left.push(tree);
            continue;
        }
        match claude_mod::remove(tree_path) {
            Ok(removed) => tree_removed |= removed,
            Err(error) => {
                if !removed_entries.is_empty() {
                    lines.push(line(
                        "removed the lets mod from CLAUDE_CODE_PLUGIN_DIRS",
                        "mod_entry=removed",
                    ));
                }
                if !lines.is_empty() {
                    lines.push(line(
                        "the lets mod files were not removed",
                        "mod_files=remaining",
                    ));
                }
                return failed(&lines, error);
            },
        }
    }
    if tree_removed || removed_entries.len() > left.len() {
        lines.push(line("removed the lets mod", "mod=removed"));
    } else if !removed_entries.is_empty() {
        lines.push(line(
            "removed the lets mod from CLAUDE_CODE_PLUGIN_DIRS",
            "mod_entry=removed",
        ));
    }
    for tree in left {
        lines.push(line(
            &format!("left the lets mod files at {tree}"),
            &format!("mod_files_left={tree}"),
        ));
    }
    Outcome::ok(raw(nothing_or(format, &lines)))
}

pub fn run(cmd: &HooksCmd, format: Format) -> Outcome {
    match cmd {
        HooksCmd::Install {
            target: HookTarget::ClaudeCode,
        } => install(
            format,
            &claude_dir(),
            &claude_mod::install_dir(),
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
        } => uninstall(
            format,
            &claude_dir(),
            &claude_mod::install_dir(),
            &lock::runtime_dir(),
        ),
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
        data: TempDir,
        runtime: TempDir,
        path_dirs: Vec<TempDir>,
    }

    impl Sandbox {
        fn new() -> Sandbox {
            Sandbox {
                home: TempDir::new().unwrap(),
                data: TempDir::new().unwrap(),
                runtime: TempDir::new().unwrap(),
                path_dirs: Vec::new(),
            }
        }

        fn mod_dir(&self) -> PathBuf {
            self.data.path().join("lets/claude-code")
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
                &self.mod_dir(),
                path.to_str().unwrap(),
                self.runtime.path(),
            )
        }

        fn uninstall(&self, format: Format) -> Outcome {
            uninstall(
                format,
                &self.home.path().join(".claude"),
                &self.mod_dir(),
                self.runtime.path(),
            )
        }

        fn uninstall_from(&self, mod_dir: &Path, format: Format) -> Outcome {
            uninstall(
                format,
                &self.home.path().join(".claude"),
                mod_dir,
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

        fn write_settings(&self, settings: &serde_json::Value) {
            std::fs::create_dir_all(self.claude("")).unwrap();
            std::fs::write(self.claude("settings.json"), settings.to_string()).unwrap();
        }
    }

    const MOD_FILES: [&str; 5] = [
        ".claude-plugin/plugin.json",
        "hooks/hooks.json",
        "hooks/register.ts",
        "hooks/steer.ts",
        "tsconfig.json",
    ];

    fn body_text(outcome: &Outcome) -> &str {
        match &outcome.response.body {
            Body::Raw { text, .. } => text,
            other => panic!("expected a raw body, got {other:?}"),
        }
    }

    #[test]
    fn first_install_wires_pre_tool_use_and_the_mod_and_no_start_hooks() {
        let sandbox = Sandbox::new().with_lets_on_path(true);

        let outcome = sandbox.install(Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(!sandbox.claude("system-append.md").exists());
        let settings = sandbox.settings_json();
        assert_eq!(
            settings["hooks"],
            serde_json::json!({"PreToolUse": [
                {"matcher": "Bash", "hooks": [{
                    "type": "command",
                    "command": "if command -v lets >/dev/null 2>&1; then lets hook classify; fi"
                }]}
            ]})
        );
        assert_eq!(
            settings["env"],
            serde_json::json!({"CLAUDE_CODE_PLUGIN_DIRS": sandbox.mod_dir().to_str().unwrap()})
        );
        for relative in MOD_FILES {
            assert!(sandbox.mod_dir().join(relative).is_file(), "{relative}");
        }
        assert_eq!(
            body_text(&outcome),
            "added the PreToolUse hook\nadded the lets mod\n"
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
    fn a_second_install_reports_everything_already_installed_with_identical_bytes() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        sandbox.install(Format::Text);
        let settings_before = std::fs::read(sandbox.claude("settings.json")).unwrap();
        let mod_before: Vec<Vec<u8>> = MOD_FILES
            .iter()
            .map(|relative| std::fs::read(sandbox.mod_dir().join(relative)).unwrap())
            .collect();

        let outcome = sandbox.install(Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(
            settings_before,
            std::fs::read(sandbox.claude("settings.json")).unwrap()
        );
        for (relative, before) in MOD_FILES.iter().zip(mod_before) {
            assert_eq!(
                std::fs::read(sandbox.mod_dir().join(relative)).unwrap(),
                before,
                "{relative}"
            );
        }
        assert_eq!(
            body_text(&outcome),
            "the PreToolUse hook was already installed\nthe lets mod was already installed\n"
        );
    }

    #[test]
    fn a_changed_mod_file_or_a_missing_plugin_dir_entry_reports_the_mod_updated() {
        let changed_file = Sandbox::new().with_lets_on_path(true);
        changed_file.install(Format::Text);
        std::fs::write(changed_file.mod_dir().join("hooks/steer.ts"), "stale").unwrap();

        let missing_entry = Sandbox::new().with_lets_on_path(true);
        missing_entry.install(Format::Text);
        let mut settings = missing_entry.settings_json();
        settings.as_object_mut().unwrap().remove("env");
        missing_entry.write_settings(&settings);

        for sandbox in [changed_file, missing_entry] {
            let outcome = sandbox.install(Format::Text);

            assert_eq!(
                body_text(&outcome),
                "the PreToolUse hook was already installed\nupdated the lets mod\n"
            );
        }
    }

    #[test]
    fn mod_files_left_from_an_earlier_install_without_the_entry_report_updated_not_added() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        claude_mod::write(&sandbox.mod_dir()).unwrap();

        let outcome = sandbox.install(Format::Json);

        assert_eq!(body_text(&outcome), "PreToolUse=installed\nmod=updated\n");
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

        assert!(body_text(&outcome).ends_with("added the lets mod\n"));
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
        assert!(!sandbox.data.path().join("lets").exists());
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
        assert!(!sandbox.data.path().join("lets").exists());
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
        assert!(!sandbox.data.path().join("lets").exists());
    }

    #[test]
    fn a_plugin_dirs_value_of_the_wrong_shape_fails_before_hooks_or_mod_files_land() {
        for existing in [
            r#"{"env": {"CLAUDE_CODE_PLUGIN_DIRS": 1}}"#,
            r#"{"env": ["CLAUDE_CODE_PLUGIN_DIRS=/opt/a"]}"#,
        ] {
            let sandbox = Sandbox::new().with_lets_on_path(true);
            std::fs::create_dir_all(sandbox.claude("")).unwrap();
            std::fs::write(sandbox.claude("settings.json"), existing).unwrap();

            let outcome = sandbox.install(Format::Text);

            assert!(
                matches!(outcome.error, Some(Error::Io { .. })),
                "{existing}"
            );
            assert_eq!(
                std::fs::read_to_string(sandbox.claude("settings.json")).unwrap(),
                existing
            );
            assert!(!sandbox.data.path().join("lets").exists(), "{existing}");
        }
    }

    #[test]
    fn a_settings_failure_after_the_mod_landed_is_partial_and_names_the_mod() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        let existing = r#"{"hooks": {"PreToolUse": null}}"#;
        std::fs::create_dir_all(sandbox.claude("")).unwrap();
        std::fs::write(sandbox.claude("settings.json"), existing).unwrap();

        let outcome = sandbox.install(Format::Text);

        assert!(matches!(outcome.error, Some(Error::Io { .. })));
        assert_eq!(body_text(&outcome), "added the lets mod\n");
        assert_eq!(
            std::fs::read_to_string(sandbox.claude("settings.json")).unwrap(),
            existing
        );
        assert!(sandbox.mod_dir().join("hooks/steer.ts").is_file());
    }

    #[test]
    fn json_format_reports_installed_then_already_installed_as_stable_tokens() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        let first = sandbox.install(Format::Json);
        assert_eq!(body_text(&first), "PreToolUse=installed\nmod=installed\n");

        let outcome = sandbox.install(Format::Json);

        assert_eq!(
            body_text(&outcome),
            "PreToolUse=already_installed\nmod=already_installed\n"
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

    fn settings_with_post_tool_use(sandbox: &Sandbox, command: &str) {
        std::fs::create_dir_all(sandbox.claude("")).unwrap();
        let entry = serde_json::json!({
            "matcher": "Edit|Write",
            "hooks": [{"type": "command", "command": command}]
        });
        std::fs::write(
            sandbox.claude("settings.json"),
            serde_json::json!({"hooks": {"PostToolUse": [entry]}}).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn install_does_not_add_a_post_tool_use_entry() {
        let sandbox = Sandbox::new().with_lets_on_path(true);

        let outcome = sandbox.install(Format::Text);

        assert!(
            sandbox.settings_json()["hooks"]
                .get("PostToolUse")
                .is_none()
        );
        assert!(!body_text(&outcome).contains("PostToolUse"));
    }

    #[test]
    fn install_removes_an_earlier_builds_post_tool_use_entry_guarded_or_not() {
        for existing in ["lets hook classify", GUARDED] {
            let sandbox = Sandbox::new().with_lets_on_path(true);
            settings_with_post_tool_use(&sandbox, existing);

            let first = sandbox.install(Format::Text);
            let second = sandbox.install(Format::Text);

            assert!(
                body_text(&first).contains("removed the PostToolUse hook"),
                "{existing}: {}",
                body_text(&first)
            );
            assert!(!body_text(&second).contains("PostToolUse"), "{existing}");
            assert!(
                sandbox.settings_json()["hooks"]
                    .get("PostToolUse")
                    .is_none(),
                "{existing}"
            );
        }
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

    /// `CLAUDE_CODE_PARAGRAPH` as released in v0.0.1 (`git show aa7ddd1:src/verbs/hooks.rs`),
    /// before this branch shortened it. Kept only so the upgrade tests below can reconstruct that
    /// release's exact `SessionStart`/`SubagentStart` commands.
    const V0_0_1_CLAUDE_CODE_PARAGRAPH: &str = r#"# File work: use `lets` through Bash

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

    /// Same guard-and-`printf` shape `session_start_command()` still uses; only the paragraph
    /// text has changed since v0.0.1.
    fn v0_0_1_session_start_command() -> String {
        let paragraph = V0_0_1_CLAUDE_CODE_PARAGRAPH.replace('\'', r"'\''");
        format!("if command -v lets >/dev/null 2>&1; then printf '%s\\n' '{paragraph}'; fi")
    }

    /// Same `SUBAGENT_START_PREFIX`-wrapped JSON shape `subagent_start_command()` still uses;
    /// only the paragraph text has changed since v0.0.1.
    fn v0_0_1_subagent_start_command() -> String {
        let context = serde_json::to_string(V0_0_1_CLAUDE_CODE_PARAGRAPH).unwrap();
        let tail = format!("{context}}}}}").replace('\'', r"'\''");
        format!("{SUBAGENT_START_PREFIX}{tail}'")
    }

    #[test]
    fn v0_0_1_session_start_command_is_recognised_as_ours_by_is_guide() {
        assert!(is_guide(&v0_0_1_session_start_command()));
    }

    fn assert_install_removes_the_session_start_hook(old_command: &str) {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        sandbox.write_settings(&serde_json::json!({"hooks": {"SessionStart": [{
            "matcher": SESSION_START_MATCHER,
            "hooks": [{"type": "command", "command": old_command}]
        }]}}));

        let outcome = sandbox.install(Format::Text);

        assert_eq!(
            body_text(&outcome),
            "added the PreToolUse hook\nadded the lets mod\nremoved the SessionStart hook \
             \u{b7} the lets mod carries its text now\n",
            "{old_command}"
        );
        assert!(
            sandbox.settings_json()["hooks"]
                .get("SessionStart")
                .is_none(),
            "{old_command}"
        );
    }

    #[test]
    fn install_removes_the_session_start_hook_the_last_release_wrote() {
        assert_install_removes_the_session_start_hook(&session_start_command());
    }

    #[test]
    fn install_removes_a_v0_0_1_session_start_hook() {
        assert_install_removes_the_session_start_hook(&v0_0_1_session_start_command());
    }

    #[test]
    fn install_removes_a_bare_lets_guide_session_start_hook() {
        assert_install_removes_the_session_start_hook("lets guide");
    }

    #[test]
    fn install_removes_a_guide_only_guarded_session_start_hook() {
        assert_install_removes_the_session_start_hook(GUIDE_ONLY_COMMAND);
    }

    #[test]
    fn install_removes_a_legacy_env_guarded_guide_session_start_hook() {
        assert_install_removes_the_session_start_hook(&legacy_env_guarded_guide_command());
    }

    #[test]
    fn install_removes_a_legacy_unconditional_guide_session_start_hook() {
        assert_install_removes_the_session_start_hook(&legacy_unconditional_guide_command());
    }

    #[test]
    fn install_removes_every_subagent_start_form_and_reports_it() {
        for old_command in [subagent_start_command(), v0_0_1_subagent_start_command()] {
            let sandbox = Sandbox::new().with_lets_on_path(true);
            sandbox.write_settings(&serde_json::json!({"hooks": {"SubagentStart": [{
                "hooks": [{"type": "command", "command": old_command}]
            }]}}));

            let text = sandbox.install(Format::Text);
            let json = {
                let again = Sandbox::new().with_lets_on_path(true);
                again.write_settings(&serde_json::json!({"hooks": {"SubagentStart": [{
                    "hooks": [{"type": "command", "command": old_command}]
                }]}}));
                again.install(Format::Json)
            };

            assert!(
                body_text(&text).contains(
                    "removed the SubagentStart hook \u{b7} the lets mod carries its text now\n"
                ),
                "{}",
                body_text(&text)
            );
            assert_eq!(
                body_text(&json),
                "PreToolUse=installed\nmod=installed\nSubagentStart=removed\n"
            );
            assert!(
                sandbox.settings_json()["hooks"]
                    .get("SubagentStart")
                    .is_none()
            );
        }
    }

    #[test]
    fn a_session_start_hook_with_our_wrapper_but_not_our_heading_or_tail_is_kept() {
        let different_heading = "if command -v lets >/dev/null 2>&1; then printf '%s\\n' \
                                  '# Something else entirely\\n\\nbody'; fi";
        let different_tail = format!("{SESSION_START_PARAGRAPH_PREFIX}\\n\\nbody'; fi; echo done");
        assert!(!is_guide(different_heading));
        assert!(!is_guide(&different_tail));

        for foreign in [different_heading, different_tail.as_str()] {
            let sandbox = Sandbox::new().with_lets_on_path(true);
            let theirs = serde_json::json!({
                "matcher": SESSION_START_MATCHER,
                "hooks": [{"type": "command", "command": foreign}]
            });
            sandbox
                .write_settings(&serde_json::json!({"hooks": {"SessionStart": [theirs.clone()]}}));

            let outcome = sandbox.install(Format::Text);

            assert!(outcome.error.is_none(), "{:?}", outcome.error);
            assert!(!body_text(&outcome).contains("SessionStart"), "{foreign}");
            assert_eq!(
                sandbox.settings_json()["hooks"]["SessionStart"],
                serde_json::json!([theirs]),
                "{foreign}"
            );
        }
    }

    #[test]
    fn an_existing_session_start_or_subagent_start_hook_of_another_tool_is_kept() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        let session = serde_json::json!([{
            "matcher": "startup",
            "hooks": [{"type": "command", "command": "echo hello"}]
        }]);
        let subagent = serde_json::json!([{
            "hooks": [{"type": "command", "command": "printf '%s' '{}'"}]
        }]);
        sandbox.write_settings(&serde_json::json!({"hooks": {
            "SessionStart": session.clone(),
            "SubagentStart": subagent.clone(),
        }}));

        let outcome = sandbox.install(Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(
            body_text(&outcome),
            "added the PreToolUse hook\nadded the lets mod\n"
        );
        let hooks = &sandbox.settings_json()["hooks"];
        assert_eq!(hooks["SessionStart"], session);
        assert_eq!(hooks["SubagentStart"], subagent);
    }

    #[test]
    fn uninstall_names_the_hook_and_the_mod_then_finds_nothing_the_second_time() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        sandbox.install(Format::Text);

        let first = sandbox.uninstall(Format::Text);
        let second = sandbox.uninstall(Format::Text);

        assert!(first.error.is_none(), "{:?}", first.error);
        assert_eq!(
            body_text(&first),
            "removed the PreToolUse hook\nremoved the lets mod\n"
        );
        assert_eq!(body_text(&second), "nothing to remove\n");
        assert_eq!(sandbox.settings_json(), serde_json::json!({}));
        assert!(!sandbox.mod_dir().exists());
    }

    #[test]
    fn uninstall_keeps_another_plugin_dir_entry_and_a_file_lets_did_not_write() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        sandbox.write_settings(&serde_json::json!({
            "env": {"CLAUDE_CODE_PLUGIN_DIRS": "/opt/other-mod"}
        }));
        sandbox.install(Format::Text);
        std::fs::write(sandbox.mod_dir().join("notes.txt"), "mine").unwrap();

        let outcome = sandbox.uninstall(Format::Text);

        assert_eq!(
            body_text(&outcome),
            "removed the PreToolUse hook\nremoved the lets mod\n"
        );
        assert_eq!(
            sandbox.settings_json(),
            serde_json::json!({"env": {"CLAUDE_CODE_PLUGIN_DIRS": "/opt/other-mod"}})
        );
        assert_eq!(
            std::fs::read_to_string(sandbox.mod_dir().join("notes.txt")).unwrap(),
            "mine"
        );
        for relative in MOD_FILES {
            assert!(!sandbox.mod_dir().join(relative).exists(), "{relative}");
        }
    }

    #[test]
    fn uninstall_over_an_earlier_release_removes_both_start_hooks() {
        let sandbox = Sandbox::new();
        sandbox.write_settings(&serde_json::json!({"hooks": {
            "PreToolUse": [{"matcher": "Bash", "hooks": [
                {"type": "command", "command": GUARDED_CLASSIFY_COMMAND}
            ]}],
            "SubagentStart": [{"hooks": [
                {"type": "command", "command": subagent_start_command()}
            ]}],
            "SessionStart": [{"matcher": SESSION_START_MATCHER, "hooks": [
                {"type": "command", "command": session_start_command()}
            ]}],
        }}));

        let outcome = sandbox.uninstall(Format::Text);

        assert_eq!(
            body_text(&outcome),
            "removed the PreToolUse hook\nremoved the SessionStart hook\nremoved the SubagentStart \
             hook\n"
        );
        assert_eq!(sandbox.settings_json(), serde_json::json!({}));
    }

    #[test]
    fn an_uninstall_failure_after_the_hook_was_removed_is_partial_and_names_it() {
        let sandbox = Sandbox::new();
        sandbox.write_settings(&serde_json::json!({
            "env": ["CLAUDE_CODE_PLUGIN_DIRS=/opt/a"],
            "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [
                {"type": "command", "command": GUARDED_CLASSIFY_COMMAND}
            ]}]},
        }));

        let outcome = sandbox.uninstall(Format::Text);

        assert!(matches!(outcome.error, Some(Error::Io { .. })));
        assert_eq!(
            body_text(&outcome),
            "removed the PreToolUse hook\nthe lets mod was not removed\n"
        );
    }

    #[test]
    fn uninstall_also_removes_an_earlier_builds_retired_post_tool_use_entry() {
        for existing in ["lets hook classify", GUARDED] {
            let sandbox = Sandbox::new();
            settings_with_post_tool_use(&sandbox, existing);

            let outcome = sandbox.uninstall(Format::Text);

            assert_eq!(
                body_text(&outcome),
                "removed the PostToolUse hook\n",
                "{existing}"
            );
            assert_eq!(sandbox.settings_json(), serde_json::json!({}), "{existing}");
        }
    }

    #[test]
    fn uninstall_reports_stable_tokens_in_json() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        sandbox.install(Format::Text);

        let first = sandbox.uninstall(Format::Json);
        let second = sandbox.uninstall(Format::Json);

        assert_eq!(body_text(&first), "PreToolUse=removed\nmod=removed\n");
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
            v0_0_1_session_start_command(),
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

    /// Raised from 1,050 when the batch example was added back: a paid-trial run without it saw
    /// 6 of 8 sessions hit `edit --from -`, get `ERROR_CODE=usage`, and fall back to Python.
    #[test]
    fn the_session_start_paragraph_stays_under_its_1_000_byte_budget() {
        let printed = format!("{CLAUDE_CODE_PARAGRAPH}\n");
        assert!(
            printed.len() <= 1_000,
            "printed paragraph is {} bytes, over the 1,000-byte SessionStart budget",
            printed.len()
        );
    }

    #[test]
    fn the_session_start_paragraph_shows_the_batch_example_and_drops_the_cat_sentence() {
        assert!(CLAUDE_CODE_PARAGRAPH.contains("lets edit --from -"));
        assert!(CLAUDE_CODE_PARAGRAPH.contains("--check @auto <<'LETS'"));
        assert!(CLAUDE_CODE_PARAGRAPH.contains("<<<<<<< old"));
        assert!(CLAUDE_CODE_PARAGRAPH.contains("======= new"));
        assert!(CLAUDE_CODE_PARAGRAPH.contains(">>>>>>>"));
        assert!(
            !CLAUDE_CODE_PARAGRAPH.contains("Exact `cat`, `head -n` and `sed -n` reads become"),
            "this sentence led agents to write `cat` inside compound commands, which then got \
             denied"
        );
        assert!(
            !CLAUDE_CODE_PARAGRAPH.contains("`cat a.ts b.ts`, Read"),
            "a bare cat is now silently rewritten, so re-teaching it wastes the paragraph's \
             budget on something the agent never has to know"
        );
        assert!(
            !CLAUDE_CODE_PARAGRAPH.contains("--outline"),
            "a 2026-09-26 trial found no session ever ran --outline, so the row is no longer \
             worth its budget"
        );
    }

    #[test]
    fn the_paragraph_teaches_head_n_instead_of_forbidding_a_pipe() {
        assert!(CLAUDE_CODE_PARAGRAPH.contains("--head N"));
        assert!(CLAUDE_CODE_PARAGRAPH.contains("2>/dev/null"));
        assert!(
            !CLAUDE_CODE_PARAGRAPH.contains("Do not pipe"),
            "the hook rewrites `lets … | head -N` into `--head N`, so forbidding the pipe is \
             wrong advice"
        );
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

    const C6_LAST_LINE: &str = "For the first lines only, pass `--head N` to `lets show` or \
        `lets find` instead of piping to `head`: the footer still names the cut. Do not add \
        `2>/dev/null`: it hides the fix. Keep Read for images and PDFs; use plain Bash for \
        anything else that is not reading, searching or editing files.";

    #[test]
    fn the_paragraph_ends_with_the_exact_head_n_line_and_nothing_after_it() {
        assert_eq!(
            CLAUDE_CODE_PARAGRAPH.lines().next_back(),
            Some(C6_LAST_LINE)
        );
        assert!(CLAUDE_CODE_PARAGRAPH.ends_with(C6_LAST_LINE));
        assert_eq!(CLAUDE_CODE_PARAGRAPH.matches("--head N").count(), 1);
    }

    #[test]
    fn install_removes_every_retired_start_form_and_duplicates_but_keeps_a_foreign_hook() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        let foreign_session = serde_json::json!({"type": "command", "command": "echo mine"});
        let foreign_subagent = serde_json::json!({"type": "command", "command": "echo theirs"});
        let command = |text: String| serde_json::json!({"type": "command", "command": text});
        sandbox.write_settings(&serde_json::json!({"hooks": {
            "SessionStart": [
                {"matcher": SESSION_START_MATCHER, "hooks": [
                    command(session_start_command()),
                    foreign_session.clone(),
                    command(v0_0_1_session_start_command()),
                    command("lets guide".to_owned()),
                    command(session_start_command()),
                ]},
                {"matcher": SESSION_START_MATCHER, "hooks": [
                    command(GUIDE_ONLY_COMMAND.to_owned()),
                    command(legacy_env_guarded_guide_command()),
                ]},
            ],
            "SubagentStart": [
                {"hooks": [
                    command(subagent_start_command()),
                    command(v0_0_1_subagent_start_command()),
                    foreign_subagent.clone(),
                    command(subagent_start_command()),
                ]},
            ],
        }}));

        let outcome = sandbox.install(Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(
            body_text(&outcome),
            "added the PreToolUse hook\nadded the lets mod\nremoved the SessionStart hook \
             \u{b7} the lets mod carries its text now\nremoved the SubagentStart hook \u{b7} \
             the lets mod carries its text now\n"
        );
        let hooks = &sandbox.settings_json()["hooks"];
        assert_eq!(
            hooks["SessionStart"],
            serde_json::json!([{"matcher": SESSION_START_MATCHER, "hooks": [foreign_session]}])
        );
        assert_eq!(
            hooks["SubagentStart"],
            serde_json::json!([{"hooks": [foreign_subagent]}])
        );
        assert_eq!(hooks["PreToolUse"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_mod_write_failure_leaves_settings_untouched_and_reports_nothing_landed() {
        for planted in [Some("{\n  // mine\n  \"model\": \"opus\"\n}\n"), None] {
            let sandbox = Sandbox::new().with_lets_on_path(true);
            std::fs::write(
                sandbox.data.path().join("lets"),
                "a file where the directory goes",
            )
            .unwrap();
            if let Some(text) = planted {
                std::fs::create_dir_all(sandbox.claude("")).unwrap();
                std::fs::write(sandbox.claude("settings.json"), text).unwrap();
            }

            let outcome = sandbox.install(Format::Text);

            assert!(
                matches!(outcome.error, Some(Error::Io { .. })),
                "{planted:?}"
            );
            assert!(matches!(&outcome.response.body, Body::Targets(targets) if targets.is_empty()));
            match planted {
                Some(text) => assert_eq!(
                    std::fs::read_to_string(sandbox.claude("settings.json")).unwrap(),
                    text
                ),
                None => assert!(!sandbox.claude("settings.json").exists()),
            }
            assert!(!sandbox.mod_dir().exists());
        }
    }

    #[test]
    fn a_mod_write_failure_after_a_file_landed_is_partial_and_names_both_sides() {
        let existing = "{\n  // mine\n  \"model\": \"opus\"\n}\n";
        for (format, expected) in [
            (
                Format::Text,
                "wrote the lets mod file .claude-plugin/plugin.json\ncould not write the lets mod \
                 file hooks/hooks.json \u{b7} entity already exists\nsettings.json was left untouched\n",
            ),
            (
                Format::Json,
                "mod_file_written=.claude-plugin/plugin.json\nmod_file_failed=hooks/hooks.json\n\
                 mod_file_error=entity already exists\nsettings=untouched\n",
            ),
        ] {
            let sandbox = Sandbox::new().with_lets_on_path(true);
            std::fs::create_dir_all(sandbox.claude("")).unwrap();
            std::fs::write(sandbox.claude("settings.json"), existing).unwrap();
            std::fs::create_dir_all(sandbox.mod_dir()).unwrap();
            std::fs::write(sandbox.mod_dir().join("hooks"), "a file where hooks/ goes").unwrap();

            let outcome = sandbox.install(format);

            assert!(
                matches!(&outcome.error, Some(Error::PartialBatch { .. })),
                "{:?}",
                outcome.error
            );
            assert_eq!(body_text(&outcome), expected);
            assert_eq!(
                std::fs::read_to_string(sandbox.claude("settings.json")).unwrap(),
                existing
            );
            assert!(
                sandbox
                    .mod_dir()
                    .join(".claude-plugin/plugin.json")
                    .is_file()
            );
        }
    }

    #[test]
    fn a_retirement_failure_after_the_hook_landed_is_partial_and_names_what_landed() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        sandbox.write_settings(&serde_json::json!({"hooks": {"SessionStart": "oops"}}));

        let outcome = sandbox.install(Format::Text);

        assert!(matches!(outcome.error, Some(Error::Io { .. })));
        assert_eq!(
            body_text(&outcome),
            "added the PreToolUse hook\nadded the lets mod\n"
        );
        let hooks = &sandbox.settings_json()["hooks"];
        assert_eq!(hooks["SessionStart"], "oops");
        assert_eq!(hooks["PreToolUse"].as_array().unwrap().len(), 1);
        assert!(sandbox.settings_json().get("env").is_none());
    }

    #[test]
    fn a_plugin_dir_failure_after_every_hook_step_names_each_line_that_landed() {
        let landed = Landed {
            pre_tool_use: Some(InstallStatus::Updated),
            lets_mod: Some(InstallStatus::Installed),
            session_start_removed: true,
            subagent_start_removed: true,
            post_tool_use_removed: true,
        };
        let failure = Error::Usage {
            message: "boom".to_owned(),
        };

        let outcome = landed.failed(Format::Text, Path::new("/nonexistent"), failure);

        assert!(matches!(outcome.error, Some(Error::Usage { .. })));
        assert_eq!(
            body_text(&outcome),
            "updated the PreToolUse hook\nadded the lets mod\nremoved the SessionStart hook \
             \u{b7} the lets mod carries its text now\nremoved the SubagentStart hook \u{b7} \
             the lets mod carries its text now\nremoved the PostToolUse hook \u{b7} the check \
             trial showed no benefit, so it is no longer installed by default\n"
        );
    }

    #[test]
    fn a_failure_with_nothing_landed_has_no_stdout() {
        let outcome =
            Landed::default().failed(Format::Text, Path::new("/nonexistent"), Error::Usage {
                message: "boom".to_owned(),
            });

        assert!(matches!(&outcome.response.body, Body::Targets(targets) if targets.is_empty()));
    }

    #[test]
    fn uninstall_removes_the_entry_and_tree_of_a_mod_installed_under_another_data_home() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        sandbox.write_settings(&serde_json::json!({
            "env": {"CLAUDE_CODE_PLUGIN_DIRS": "/opt/other-mod"}
        }));
        sandbox.install(Format::Text);
        let elsewhere = TempDir::new().unwrap();
        let current_env_dir = elsewhere.path().join("lets/claude-code");

        let outcome = sandbox.uninstall_from(&current_env_dir, Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(
            body_text(&outcome),
            "removed the PreToolUse hook\nremoved the lets mod\n"
        );
        assert_eq!(
            sandbox.settings_json(),
            serde_json::json!({"env": {"CLAUDE_CODE_PLUGIN_DIRS": "/opt/other-mod"}})
        );
        assert!(!sandbox.mod_dir().exists());
        assert!(sandbox.data.path().join("lets").is_dir());
    }

    fn checkout_mod(
        root: &Path,
        relative: &str,
        plugin_name: &str,
    ) -> (PathBuf, Vec<(PathBuf, String)>) {
        let dir = root.join(relative);
        let mut files = Vec::new();
        for file in MOD_FILES {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let text = if file == ".claude-plugin/plugin.json" {
                format!(r#"{{"name": "{plugin_name}"}}"#)
            } else {
                format!("uncommitted edit in {file}\n")
            };
            std::fs::write(&path, &text).unwrap();
            files.push((path, text));
        }
        (dir, files)
    }

    fn assert_files_untouched(files: &[(PathBuf, String)]) {
        for (path, text) in files {
            assert_eq!(&std::fs::read_to_string(path).unwrap(), text, "{path:?}");
        }
    }

    #[test]
    fn uninstall_drops_the_entry_of_a_checkout_mod_but_keeps_its_files_and_names_the_path() {
        let sandbox = Sandbox::new();
        let (checkout, files) = checkout_mod(sandbox.data.path(), "repo/mod", "lets");
        sandbox.write_settings(&serde_json::json!({
            "env": {"CLAUDE_CODE_PLUGIN_DIRS": checkout.to_str().unwrap()}
        }));

        let text = sandbox.uninstall(Format::Text);
        let message = format!(
            "removed the lets mod from CLAUDE_CODE_PLUGIN_DIRS\nleft the lets mod files at {}\n",
            checkout.display()
        );

        assert!(text.error.is_none(), "{:?}", text.error);
        assert_eq!(body_text(&text), message);
        assert_eq!(sandbox.settings_json(), serde_json::json!({}));
        assert_files_untouched(&files);
    }

    #[test]
    fn a_json_uninstall_names_the_files_it_left_with_a_stable_token() {
        let sandbox = Sandbox::new();
        let (checkout, files) = checkout_mod(sandbox.data.path(), "repo/mod", "lets");
        sandbox.write_settings(&serde_json::json!({
            "env": {"CLAUDE_CODE_PLUGIN_DIRS": checkout.to_str().unwrap()}
        }));

        let json = sandbox.uninstall(Format::Json);

        assert_eq!(
            body_text(&json),
            format!("mod_entry=removed\nmod_files_left={}\n", checkout.display())
        );
        assert_files_untouched(&files);
    }

    #[test]
    fn uninstall_deletes_a_mod_in_the_installer_layout_and_keeps_one_that_is_not() {
        let sandbox = Sandbox::new();
        let (laid_out, _) = checkout_mod(sandbox.data.path(), "elsewhere/lets/claude-code", "lets");
        let (checkout, files) = checkout_mod(sandbox.data.path(), "repo/mod", "lets");
        let value = format!("{}:{}", checkout.display(), laid_out.display());
        sandbox.write_settings(&serde_json::json!({"env": {"CLAUDE_CODE_PLUGIN_DIRS": value}}));

        let text = sandbox.uninstall(Format::Text);

        assert_eq!(
            body_text(&text),
            format!(
                "removed the lets mod\nleft the lets mod files at {}\n",
                checkout.display()
            )
        );
        assert!(!laid_out.exists());
        assert_files_untouched(&files);
    }

    #[test]
    fn uninstall_keeps_the_entry_and_files_of_a_layout_dir_whose_plugin_json_names_another_plugin()
    {
        let sandbox = Sandbox::new();
        let (foreign, files) =
            checkout_mod(sandbox.data.path(), "theirs/lets/claude-code", "other");
        let value = foreign.to_str().unwrap();
        sandbox.write_settings(&serde_json::json!({"env": {"CLAUDE_CODE_PLUGIN_DIRS": value}}));

        let text = sandbox.uninstall(Format::Text);

        assert_eq!(body_text(&text), "nothing to remove\n");
        assert_eq!(
            sandbox.settings_json()["env"]["CLAUDE_CODE_PLUGIN_DIRS"],
            value
        );
        assert_files_untouched(&files);
    }

    #[test]
    fn uninstall_keeps_an_entry_whose_plugin_json_names_another_plugin_or_is_relative() {
        let sandbox = Sandbox::new();
        let foreign = sandbox.data.path().join("theirs");
        std::fs::create_dir_all(foreign.join(".claude-plugin")).unwrap();
        std::fs::write(
            foreign.join(".claude-plugin/plugin.json"),
            r#"{"name": "other"}"#,
        )
        .unwrap();
        let value = format!("{}:relative/dir::", foreign.display());
        sandbox.write_settings(&serde_json::json!({"env": {"CLAUDE_CODE_PLUGIN_DIRS": value}}));

        let outcome = sandbox.uninstall(Format::Text);

        assert_eq!(body_text(&outcome), "nothing to remove\n");
        assert_eq!(
            sandbox.settings_json()["env"]["CLAUDE_CODE_PLUGIN_DIRS"],
            value
        );
        assert!(foreign.join(".claude-plugin/plugin.json").is_file());
    }

    #[test]
    fn an_uninstall_that_cannot_remove_the_tree_is_partial_and_names_what_landed_and_what_remains()
    {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        sandbox.install(Format::Text);
        std::fs::write(
            sandbox.mod_dir().join(".claude-plugin/types"),
            "not a directory",
        )
        .unwrap();

        let text = sandbox.uninstall(Format::Text);

        assert!(
            matches!(text.error, Some(Error::Io { .. })),
            "{:?}",
            text.error
        );
        assert_eq!(
            body_text(&text),
            "removed the PreToolUse hook\nremoved the lets mod from CLAUDE_CODE_PLUGIN_DIRS\nthe \
             lets mod files were not removed\n"
        );
        assert_eq!(sandbox.settings_json(), serde_json::json!({}));
        assert!(sandbox.mod_dir().join(".claude-plugin/types").exists());
    }

    #[test]
    fn a_json_uninstall_that_cannot_remove_the_tree_reports_stable_tokens() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        sandbox.install(Format::Text);
        std::fs::write(
            sandbox.mod_dir().join(".claude-plugin/types"),
            "not a directory",
        )
        .unwrap();

        let json = sandbox.uninstall(Format::Json);

        assert!(json.error.is_some());
        assert_eq!(
            body_text(&json),
            "PreToolUse=removed\nmod_entry=removed\nmod_files=remaining\n"
        );
    }

    #[test]
    fn an_uninstall_tree_failure_with_nothing_else_landed_is_a_plain_failure() {
        let sandbox = Sandbox::new();
        sandbox.write_settings(&serde_json::json!({}));
        claude_mod::write(&sandbox.mod_dir()).unwrap();
        std::fs::write(
            sandbox.mod_dir().join(".claude-plugin/types"),
            "not a directory",
        )
        .unwrap();

        let outcome = sandbox.uninstall(Format::Text);

        assert!(matches!(outcome.error, Some(Error::Io { .. })));
        assert!(matches!(&outcome.response.body, Body::Targets(targets) if targets.is_empty()));
    }
}
