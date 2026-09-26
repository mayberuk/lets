use std::path::Path;

use crate::Outcome;
use crate::error::Error;
use crate::install::pathguard;
use crate::install::settings::{self, HookEntry, InstallStatus};
use crate::output::Format;
use crate::verbs::hooks::{
    GUARDED_CLASSIFY_COMMAND, LEGACY_PARAGRAPH, hook_line, is_classify, raw, uninstall_report,
};

/// The same short file-work table `CLAUDE_CODE_PARAGRAPH` carries, minus the two rows naming a
/// Read/Edit tool Codex has none of. `docs/agents.md`'s own Codex section is the source of truth;
/// keep this constant identical to it (enforced by
/// `codex_session_start_paragraph_is_agents_md_s_own_words` below).
const CODEX_SESSION_START_PARAGRAPH: &str = r"# File work: use `lets` through Bash

| Instead of | Run |
|---|---|
| several `cat`/`sed -n`/`grep` calls | `lets show a.ts b.ts:10-40 c.ts#computeFee` |
| a definitions-only skim | `lets show f.ts --outline` |
| `sed -i 's/a/b/'` | `lets edit f.ts --old a --new b` |
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
LETS
```";

/// Codex treats non-JSON-looking stdout on exit 0 as `additionalContext` directly, for both
/// `SessionStart` and `SubagentStart` (`codex-rs/hooks/src/events/session_start.rs`
/// `parse_completed`, verified via ctx7 against `openai/codex`) — no `hookSpecificOutput` wrapper
/// needed the way Claude Code's `SubagentStart` needs. Same guard as `session_start_command()`: a
/// missing or mid-update `lets` still exits 0 and prints nothing.
fn codex_start_command() -> String {
    let paragraph = CODEX_SESSION_START_PARAGRAPH.replace('\'', r"'\''");
    format!("{CODEX_START_OPEN}{paragraph}{CODEX_START_CLOSE}")
}

const CODEX_START_OPEN: &str = "if command -v lets >/dev/null 2>&1; then printf '%s\\n' '";
const CODEX_START_CLOSE: &str = "'; fi";

/// Recognises this printf form by its guard and heading, stable across a reworded paragraph, and
/// the earlier, broken `lets hook classify` form (`classify` has no `SessionStart`/`SubagentStart`
/// dispatch branch, so that form delivered no context), so an install over either replaces it
/// instead of adding a second entry.
fn is_codex_start(command: &str) -> bool {
    let heading = CODEX_SESSION_START_PARAGRAPH
        .lines()
        .next()
        .unwrap_or_default();
    command
        .strip_prefix(CODEX_START_OPEN)
        .and_then(|rest| rest.strip_suffix(CODEX_START_CLOSE))
        .is_some_and(|paragraph| paragraph.starts_with(heading))
        || is_classify(command)
}

const CLASSIFY_ENTRY: HookEntry<'static> = HookEntry {
    event: "PreToolUse",
    matcher: Some("Bash"),
    command: GUARDED_CLASSIFY_COMMAND,
    is_ours: is_classify,
    carry_over: settings::nothing_to_carry_over,
};

/// No `SESSION_START_MATCHER`: Codex's `SessionStart` carries no `source` field to match on the
/// way Claude Code's does. The command is a guarded `printf` of the paragraph, not
/// `lets hook classify` — `classify` has no `SessionStart`/`SubagentStart` dispatch branch, the
/// same reason Claude Code's own `SessionStart`/`SubagentStart` entries never call it either.
fn session_start_entry(start_command: &str) -> HookEntry<'_> {
    HookEntry {
        event: "SessionStart",
        matcher: None,
        command: start_command,
        is_ours: is_codex_start,
        carry_over: settings::nothing_to_carry_over,
    }
}

fn subagent_start_entry(start_command: &str) -> HookEntry<'_> {
    HookEntry {
        event: "SubagentStart",
        matcher: None,
        command: start_command,
        is_ours: is_codex_start,
        carry_over: settings::nothing_to_carry_over,
    }
}

/// Codex names the tool `apply_patch` in a hook's stdin and matcher, with `Edit` and `Write` only
/// as matcher aliases (codex-rs/core/src/tools/hook_names.rs, 0.154).
const CHECK_ENTRY: HookEntry<'static> = HookEntry {
    event: "PostToolUse",
    matcher: Some("apply_patch"),
    command: GUARDED_CLASSIFY_COMMAND,
    is_ours: is_classify,
    carry_over: settings::nothing_to_carry_over,
};

fn entries(start_command: &str) -> [HookEntry<'_>; 4] {
    [
        CLASSIFY_ENTRY,
        session_start_entry(start_command),
        subagent_start_entry(start_command),
        CHECK_ENTRY,
    ]
}

/// Verified against the source, not the docs: an unapproved hook is skipped entirely and the
/// command runs unhooked (hooks/src/engine/discovery.rs:713-718).
const FAIL_OPEN_FACT: &str = "an unapproved hook is skipped entirely \u{b7} Codex runs the \
    command unhooked, never through lets, until it is approved";

/// `unapproved` names the entries Codex does not yet trust: the install is approved only when none
/// is left, and a partial approval names the rest.
fn approval_line(format: Format, entries: usize, unapproved: &[&str]) -> String {
    let partial = !unapproved.is_empty() && unapproved.len() < entries;
    match format {
        Format::Text if unapproved.is_empty() => "hook: installed and approved".to_owned(),
        Format::Text => {
            let named = if partial {
                format!(": {}", unapproved.join(", "))
            } else {
                String::new()
            };
            format!(
                "hook: installed, not yet approved{named} \u{b7} open Codex and choose 'Trust all \
                 and continue' when prompted, press t in the hooks browser, or pass \
                 --dangerously-bypass-hook-trust for one run"
            )
        },
        Format::Json | Format::Jsonl if unapproved.is_empty() => "trust=approved".to_owned(),
        Format::Json | Format::Jsonl if partial => {
            format!("trust=not_approved:{}", unapproved.join(","))
        },
        Format::Json | Format::Jsonl => "trust=not_approved".to_owned(),
    }
}

fn report(
    format: Format,
    entries: &[HookEntry],
    statuses: &[InstallStatus; 4],
    unapproved: &[&str],
) -> String {
    [
        hook_line(format, "PreToolUse", &statuses[0]),
        hook_line(format, "SessionStart", &statuses[1]),
        hook_line(format, "SubagentStart", &statuses[2]),
        hook_line(format, "PostToolUse", &statuses[3]),
        approval_line(format, entries.len(), unapproved),
        LEGACY_PARAGRAPH.trim_end().to_owned(),
        FAIL_OPEN_FACT.to_owned(),
    ]
    .join("\n")
        + "\n"
}

const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// The eight FIPS 180-4 working variables, kept as its own array so the round loop below indexes
/// `v[A]..v[H]` instead of eight single-letter locals.
const A: usize = 0;
const B: usize = 1;
const C: usize = 2;
const D: usize = 3;
const E: usize = 4;
const F: usize = 5;
const G: usize = 6;
const H: usize = 7;

/// FIPS 180-4 SHA-256, no crate: this crate's `sha2` would be an 18th-plus direct dependency past
/// `deps-gate.sh`'s limit (already at 18/18), and this only runs once per `hooks install codex`,
/// never on a hot path.
fn sha256_hex(message: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut hash: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];

    let bit_len = (message.len() as u64) * 8;
    let mut padded = message.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let (blocks, remainder) = padded.as_chunks::<64>();
    debug_assert!(
        remainder.is_empty(),
        "padding always lands on a 64-byte boundary"
    );
    for block in blocks {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().expect("4 bytes"));
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut v = hash;
        for (i, k) in SHA256_K.iter().enumerate() {
            let s1 = v[E].rotate_right(6) ^ v[E].rotate_right(11) ^ v[E].rotate_right(25);
            let ch = (v[E] & v[F]) ^ ((!v[E]) & v[G]);
            let temp1 = v[H]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(*k)
                .wrapping_add(w[i]);
            let s0 = v[A].rotate_right(2) ^ v[A].rotate_right(13) ^ v[A].rotate_right(22);
            let maj = (v[A] & v[B]) ^ (v[A] & v[C]) ^ (v[B] & v[C]);
            let temp2 = s0.wrapping_add(maj);

            v[H] = v[G];
            v[G] = v[F];
            v[F] = v[E];
            v[E] = v[D].wrapping_add(temp1);
            v[D] = v[C];
            v[C] = v[B];
            v[B] = v[A];
            v[A] = temp1.wrapping_add(temp2);
        }

        for (word, delta) in hash.iter_mut().zip(v) {
            *word = word.wrapping_add(delta);
        }
    }

    hash.iter()
        .fold(String::with_capacity(64), |mut hex, word| {
            write!(hex, "{word:08x}").expect("writing to a String never fails");
            hex
        })
}

fn event_name_key(event: &str) -> &'static str {
    match event {
        "PreToolUse" => "pre_tool_use",
        "SessionStart" => "session_start",
        "SubagentStart" => "subagent_start",
        "PostToolUse" => "post_tool_use",
        other => {
            unreachable!("hooks install codex only builds entries for known events, got {other}")
        },
    }
}

/// The hash Codex itself computes for this entry: SHA-256 of the canonical JSON of
/// `NormalizedHookIdentity` (`config/src/fingerprint.rs` `version_for_toml`,
/// `hooks/src/engine/discovery.rs:769-791`, verified via ctx7). `#[serde(flatten)]` on `group`
/// puts `matcher`/`hooks` beside `event_name`, not nested under a `"group"` key. `timeout` is
/// Codex's own 600s default for a non-`SessionEnd` hook since lets never sets one; `serde_json`'s
/// default `Map` is a `BTreeMap`, already the sorted-key form Codex's `canonical_json` produces.
fn entry_trust_hash(entry: &HookEntry) -> String {
    let handler = serde_json::json!({
        "async": false,
        "command": entry.command,
        "timeout": 600,
        "type": "command",
    });
    let mut identity = serde_json::json!({
        "event_name": event_name_key(entry.event),
        "hooks": [handler],
    });
    if let Some(matcher) = entry.matcher {
        identity["matcher"] = serde_json::json!(matcher);
    }
    let canonical = serde_json::to_vec(&identity).expect("a JSON value always serialises");
    format!("sha256:{}", sha256_hex(&canonical))
}

/// The events of the entries whose hash no `trusted_hash` in `config.toml` matches. Read-only:
/// Codex deliberately requires a human's own review before trusting a hook (Non-goals), so this
/// never writes `trusted_hash`. A missing or unparsable `config.toml` trusts nothing, never an
/// error — same fail-open posture as everything else in this module.
fn unapproved<'e>(config_path: &Path, entries: &[HookEntry<'e>]) -> Vec<&'e str> {
    let trusted: Vec<String> = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|text| text.parse::<toml_edit::DocumentMut>().ok())
        .and_then(|doc| {
            let state = doc.get("hooks")?.as_table()?.get("state")?.as_table()?;
            Some(
                state
                    .iter()
                    .filter_map(|(_, value)| value.get("trusted_hash")?.as_str().map(str::to_owned))
                    .collect(),
            )
        })
        .unwrap_or_default();
    entries
        .iter()
        .filter(|entry| !trusted.contains(&entry_trust_hash(entry)))
        .map(|entry| entry.event)
        .collect()
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

    let start_command = codex_start_command();
    let entries = entries(&start_command);
    let merged = settings::merge_hook_entries(&dir.join("hooks.json"), &entries, runtime);
    let statuses: [InstallStatus; 4] = match merged {
        Ok(statuses) => statuses
            .try_into()
            .unwrap_or_else(|_| unreachable!("four entries were passed")),
        Err(error) => return Outcome::failed("hooks install", error),
    };
    let unapproved = unapproved(&dir.join("config.toml"), &entries);
    Outcome::ok(raw(report(format, &entries, &statuses, &unapproved)))
}

pub fn uninstall(format: Format, dir: &Path, runtime: &Path) -> Outcome {
    let start_command = codex_start_command();
    let entries = entries(&start_command);
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

    #[test]
    fn sha256_matches_the_fips_180_4_known_answer_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            sha256_hex(&b"a".repeat(1_000_000)),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

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

        fn write_config_toml(&self, text: &str) {
            std::fs::create_dir_all(self.codex_dir()).unwrap();
            std::fs::write(self.codex_dir().join("config.toml"), text).unwrap();
        }
    }

    fn body_text(outcome: &Outcome) -> &str {
        match &outcome.response.body {
            Body::Raw { text, .. } => text,
            other => panic!("expected a raw body, got {other:?}"),
        }
    }

    #[test]
    fn first_install_writes_all_four_entries_and_exits_ok() {
        let sandbox = Sandbox::new().with_lets_on_path(true);

        let outcome = sandbox.install(Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let start_command = codex_start_command();
        assert_eq!(
            sandbox.hooks_json(),
            serde_json::json!({"hooks": {
                "PreToolUse": [
                    {"matcher": "Bash", "hooks": [{"type": "command", "command": GUARDED_CLASSIFY_COMMAND}]}
                ],
                "SessionStart": [
                    {"hooks": [{"type": "command", "command": start_command}]}
                ],
                "SubagentStart": [
                    {"hooks": [{"type": "command", "command": start_command}]}
                ],
                "PostToolUse": [
                    {"matcher": "apply_patch", "hooks": [{"type": "command", "command": GUARDED_CLASSIFY_COMMAND}]}
                ],
            }})
        );
        let text = body_text(&outcome);
        assert!(text.starts_with("added the PreToolUse hook\n"));
        assert!(text.contains("added the SessionStart hook\n"));
        assert!(text.contains("added the SubagentStart hook\n"));
        assert!(text.contains("added the PostToolUse hook\n"));
        assert!(text.contains("hook: installed, not yet approved"));
        assert!(text.contains("Trust all and continue"));
        assert!(text.contains(LEGACY_PARAGRAPH.trim_end()));
        assert!(text.ends_with(&format!("{FAIL_OPEN_FACT}\n")));
        assert!(!text.contains("unverified"));
        assert!(!text.contains("AGENTS.md by hand"));
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
        assert!(body_text(&first).contains("trust=not_approved"));

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
        assert_eq!(entries[1]["hooks"][0]["command"], GUARDED_CLASSIFY_COMMAND);
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
        assert_eq!(entries[0]["hooks"][0]["command"], GUARDED_CLASSIFY_COMMAND);
        assert!(body_text(&outcome).starts_with("updated the PreToolUse hook\n"));
    }

    #[test]
    fn uninstall_removes_all_four_keeps_a_teammates_entry_and_finds_nothing_the_second_time() {
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
        assert_eq!(
            body_text(&first),
            "removed the PreToolUse hook\nremoved the SessionStart hook\nremoved the \
             SubagentStart hook\nremoved the PostToolUse hook\n"
        );
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

    #[test]
    fn no_config_toml_reports_not_approved() {
        let sandbox = Sandbox::new().with_lets_on_path(true);

        let outcome = sandbox.install(Format::Text);

        assert!(body_text(&outcome).contains("not yet approved"));
    }

    #[test]
    fn an_unrelated_trusted_hash_still_reports_not_approved() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        sandbox.install(Format::Text);
        sandbox.write_config_toml(
            "[hooks.state.\"some-other-hook\"]\ntrusted_hash = \"sha256:deadbeef\"\n",
        );

        let outcome = sandbox.install(Format::Text);

        assert!(body_text(&outcome).contains("not yet approved"));
    }

    /// Codex keys each trusted hook by a name of its own choosing, so only the hash is matched.
    fn trust(sandbox: &Sandbox, trusted: &[HookEntry]) {
        use std::fmt::Write as _;
        let mut state = String::new();
        for (at, entry) in trusted.iter().enumerate() {
            writeln!(
                state,
                "[hooks.state.\"key-{at}\"]\ntrusted_hash = \"{}\"",
                entry_trust_hash(entry)
            )
            .expect("writing to a String never fails");
        }
        sandbox.write_config_toml(&state);
    }

    #[test]
    fn every_entry_trusted_reports_approved() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        sandbox.install(Format::Text);
        let start_command = codex_start_command();
        trust(&sandbox, &entries(&start_command));

        let text = body_text(&sandbox.install(Format::Text)).to_owned();
        let json = body_text(&sandbox.install(Format::Json)).to_owned();

        assert!(text.contains("\nhook: installed and approved\n"), "{text}");
        assert!(!text.contains("not yet approved"), "{text}");
        assert!(json.contains("\ntrust=approved\n"), "{json}");
    }

    #[test]
    fn some_entries_trusted_reports_not_approved_naming_the_rest() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        sandbox.install(Format::Text);
        let start_command = codex_start_command();
        let [pre, session, subagent, post] = entries(&start_command);
        trust(&sandbox, &[pre, subagent]);

        let text = body_text(&sandbox.install(Format::Text)).to_owned();
        let json = body_text(&sandbox.install(Format::Json)).to_owned();

        assert!(
            text.contains("\nhook: installed, not yet approved: SessionStart, PostToolUse \u{b7} "),
            "{text}"
        );
        assert!(!text.contains("installed and approved"), "{text}");
        assert!(
            json.contains("\ntrust=not_approved:SessionStart,PostToolUse\n"),
            "{json}"
        );
        trust(&sandbox, &[session]);
        let one_trusted = body_text(&sandbox.install(Format::Text)).to_owned();
        assert!(
            one_trusted
                .contains("not yet approved: PreToolUse, SubagentStart, PostToolUse \u{b7} "),
            "{one_trusted}"
        );
        trust(&sandbox, &[post]);
        assert!(
            body_text(&sandbox.install(Format::Text))
                .contains("not yet approved: PreToolUse, SessionStart, SubagentStart \u{b7} ")
        );
    }

    #[test]
    fn an_unparsable_config_toml_fails_open_to_not_approved() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        sandbox.install(Format::Text);
        sandbox.write_config_toml("not valid toml {{{");

        let outcome = sandbox.install(Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(body_text(&outcome).contains("not yet approved"));
    }

    #[test]
    fn the_four_entries_hash_differently_from_each_other() {
        let start_command = codex_start_command();
        let hashes: Vec<String> = entries(&start_command)
            .iter()
            .map(entry_trust_hash)
            .collect();
        let distinct: std::collections::BTreeSet<&String> = hashes.iter().collect();
        assert_eq!(distinct.len(), 4);
        assert!(hashes.iter().all(|hash| hash.starts_with("sha256:")));
    }

    /// Independently derived, not copied from this module's own output: Python's
    /// `sha256(json.dumps(identity, sort_keys=True, separators=(",", ":")))` over the flattened
    /// identity built by hand.
    #[test]
    fn both_classify_hashes_match_an_independent_python_computation() {
        assert_eq!(
            entry_trust_hash(&CLASSIFY_ENTRY),
            "sha256:afda628195f1844bb957a0a448ff08ad4694ad493b49729b977e94114e22db99"
        );
        assert_eq!(
            entry_trust_hash(&CHECK_ENTRY),
            "sha256:c755e13c54729eecd7a971043024c76041824f6c1c42eb19145230149a564c6a"
        );
    }

    #[test]
    fn codex_session_start_paragraph_is_agents_md_s_own_words() {
        let agents = include_str!("../../docs/agents.md");
        let unquoted: String = agents
            .lines()
            .map(|line| {
                line.strip_prefix("> ")
                    .unwrap_or_else(|| line.strip_prefix('>').unwrap_or(line))
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(unquoted.contains(CODEX_SESSION_START_PARAGRAPH));
    }

    #[test]
    fn an_earlier_broken_classify_form_upgrades_in_place_not_duplicated() {
        let sandbox = Sandbox::new().with_lets_on_path(true);
        std::fs::create_dir_all(sandbox.codex_dir()).unwrap();
        std::fs::write(
            sandbox.codex_dir().join("hooks.json"),
            serde_json::json!({"hooks": {
                "SessionStart": [
                    {"hooks": [{"type": "command", "command": "lets hook classify"}]}
                ],
                "SubagentStart": [
                    {"hooks": [{"type": "command", "command": "lets hook classify"}]}
                ],
            }})
            .to_string(),
        )
        .unwrap();

        let outcome = sandbox.install(Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let hooks = sandbox.hooks_json();
        assert_eq!(
            hooks["hooks"]["SessionStart"].as_array().map(Vec::len),
            Some(1)
        );
        assert_eq!(
            hooks["hooks"]["SubagentStart"].as_array().map(Vec::len),
            Some(1)
        );
        let start_command = codex_start_command();
        assert_eq!(
            hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            start_command
        );
        assert_eq!(
            hooks["hooks"]["SubagentStart"][0]["hooks"][0]["command"],
            start_command
        );
        let text = body_text(&outcome);
        assert!(text.contains("updated the SessionStart hook\n"));
        assert!(text.contains("updated the SubagentStart hook\n"));
    }
}
