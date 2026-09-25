use super::bash;
use super::permissions::Sources;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Always carries a runnable `lets` command: a block without one leaves the agent stuck.
    Block {
        reason: String,
    },
    /// `command` replaces the whole Bash command; `reason` tells the agent what ran instead.
    Rewrite {
        command: String,
        reason: String,
    },
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
struct RawEvent {
    tool_name: String,
    cwd: String,
    tool_input: RawToolInput,
    /// Codex CLI sends it and Claude Code does not, so it marks a Codex event.
    turn_id: Option<serde::de::IgnoredAny>,
}

/// Codex CLI 0.154 appends `. Command: <original>` to a deny reason; ending on this line keeps
/// that suffix off the `run: ` line.
const CODEX_TAIL: &str = "\nthe command above replaces the original";

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
struct RawToolInput {
    command: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct HookOutput<'a> {
    hook_specific_output: HookSpecificOutput<'a>,
}

/// A rewrite carries no `permissionDecision`: "allow" would skip the prompt the user's settings
/// give the rewritten command, while none leaves it to "the normal permission evaluation"
/// (code.claude.com/docs/en/agent-sdk/hooks, "Modify tool input").
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct HookSpecificOutput<'a> {
    hook_event_name: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    permission_decision: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    permission_decision_reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_input: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    additional_context: Option<&'a str>,
}

pub fn classify(stdin: &[u8]) -> Verdict {
    classify_with(stdin, Sources::from_env)
}

/// `sources` is called only for a Claude Code event, whose settings a rewrite must honour.
fn classify_with(stdin: &[u8], sources: impl FnOnce() -> Sources) -> Verdict {
    guarded(|| {
        let Ok(event) = serde_json::from_slice::<RawEvent>(stdin) else {
            return Verdict::Allow;
        };
        if event.tool_name != "Bash" {
            return Verdict::Allow;
        }
        let claude_code = event.turn_id.is_none();
        let sources = claude_code.then(sources);
        let verdict =
            bash::classify_command(&event.tool_input.command, &event.cwd, sources.as_ref());
        match verdict {
            Verdict::Block { mut reason } if !claude_code => {
                reason.push_str(CODEX_TAIL);
                Verdict::Block { reason }
            },
            verdict => verdict,
        }
    })
}

/// A serialization failure renders empty, which Claude Code reads as no objection. `event` is the
/// hook's stdin: `updatedInput` replaces the whole tool input, so a rewrite copies every other
/// field from it.
pub fn render(verdict: &Verdict, event: &[u8]) -> String {
    let specific = match verdict {
        Verdict::Allow => return String::new(),
        Verdict::Block { reason } => HookSpecificOutput {
            hook_event_name: "PreToolUse",
            permission_decision: Some("deny"),
            permission_decision_reason: Some(reason),
            updated_input: None,
            additional_context: None,
        },
        Verdict::Rewrite { command, reason } => {
            let Some(input) = rewritten_input(event, command) else {
                return String::new();
            };
            HookSpecificOutput {
                hook_event_name: "PreToolUse",
                permission_decision: None,
                permission_decision_reason: None,
                updated_input: Some(input),
                additional_context: Some(reason),
            }
        },
    };
    let output = HookOutput {
        hook_specific_output: specific,
    };
    match serde_json::to_string(&output) {
        Ok(json) => format!("{json}\n"),
        Err(_) => String::new(),
    }
}

fn rewritten_input(
    event: &[u8],
    command: &str,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let serde_json::Value::Object(mut event) = serde_json::from_slice(event).ok()? else {
        return None;
    };
    let serde_json::Value::Object(mut input) = event.remove("tool_input")? else {
        return None;
    };
    input.insert("command".to_owned(), command.into());
    Some(input)
}

/// The release profile aborts on panic instead: the child dies on a signal, not the exit 2 a
/// `PreToolUse` hook blocks with, so a crash is still allow.
fn guarded(classify: impl FnOnce() -> Verdict) -> Verdict {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(classify)).unwrap_or(Verdict::Allow)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::{Sources, Verdict, classify_with, guarded, render};

    /// A repository whose every settings tier lives inside it, so no test reads the real HOME.
    struct Repo(TempDir);

    impl Repo {
        fn new() -> Repo {
            let dir = TempDir::new().expect("a temp repository");
            for file in ["src/a.ts", "src/b.ts", "README.md"] {
                let path = dir.path().join(file);
                std::fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
                std::fs::write(path, "cap\n").expect("a file");
            }
            std::fs::create_dir(dir.path().join(".git")).expect("a .git directory");
            Repo(dir)
        }

        fn cwd(&self) -> &str {
            self.0.path().to_str().expect("a utf-8 temp path")
        }

        fn event(&self, tool: &str, command: &str) -> Vec<u8> {
            format!(
                r#"{{"session_id":"s","transcript_path":"/t","cwd":"{}","hook_event_name":"PreToolUse","tool_name":"{tool}","tool_input":{{"command":"{command}"}}}}"#,
                self.cwd()
            )
            .into_bytes()
        }

        /// The shape Codex CLI 0.154 sends; `turn_id` is what marks it.
        fn codex_event(&self, command: &str) -> Vec<u8> {
            format!(
                r#"{{"session_id":"00000000-0000-7000-8000-000000000001","turn_id":"00000000-0000-7000-8000-000000000002","transcript_path":"/home/user/.codex/sessions/2026/09/22/rollout.jsonl","cwd":"{}","hook_event_name":"PreToolUse","model":"gpt-5.5","permission_mode":"bypassPermissions","tool_name":"Bash","tool_input":{{"command":"{command}"}},"tool_use_id":"call_example"}}"#,
                self.cwd()
            )
            .into_bytes()
        }

        fn classify(&self, event: &[u8]) -> Verdict {
            classify_with(event, || Sources {
                home: Some(self.0.path().join(".home")),
                config: None,
                project: None,
                managed: self.0.path().join(".managed"),
            })
        }
    }

    /// A search stays a deny on Claude Code, so it carries the same reason on both harnesses.
    const SEARCH: &str = "rg cap README.md";

    #[test]
    fn a_claude_code_read_of_two_repo_files_rewrites_to_one_show_of_both() {
        let repo = Repo::new();
        let event = repo.event("Bash", "cat src/a.ts && cat src/b.ts");
        let Verdict::Rewrite { command, reason } = repo.classify(&event) else {
            panic!("a whole-file read of repo files on Claude Code is a rewrite");
        };
        assert_eq!(command, "lets show src/a.ts src/b.ts --all");
        assert_eq!(
            reason,
            "lets show reads several files and ranges in one call.\nran instead: lets show \
             src/a.ts src/b.ts --all"
        );
    }

    #[test]
    fn a_codex_read_is_the_deny_it_was_before_rewrites_existed() {
        let repo = Repo::new();
        let reason = block_reason(repo.classify(&repo.codex_event("cat src/a.ts")));

        assert_eq!(
            reason,
            "lets show reads several files and ranges in one call.\nrun: lets show src/a.ts\nthe \
             command above replaces the original"
        );
    }

    #[test]
    fn a_rewrite_renders_updated_input_with_no_permission_decision() {
        let repo = Repo::new();
        let event = format!(
            r#"{{"session_id":"s","cwd":"{}","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{{"command":"cat src/a.ts","description":"Read a","timeout":5000}}}}"#,
            repo.cwd()
        );
        let verdict = repo.classify(event.as_bytes());

        let rendered = render(&verdict, event.as_bytes());

        let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
        assert_eq!(
            parsed,
            serde_json::json!({"hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "updatedInput": {
                    "command": "lets show src/a.ts --all",
                    "description": "Read a",
                    "timeout": 5000,
                },
                "additionalContext": "lets show reads several files and ranges in one call.\n\
                                      ran instead: lets show src/a.ts --all",
            }})
        );
        assert_eq!(rendered.lines().count(), 1);
    }

    #[test]
    fn a_rewrite_whose_event_cannot_be_read_back_renders_nothing() {
        let rewrite = Verdict::Rewrite {
            command: "lets show a.ts --all".to_owned(),
            reason: "ran instead: lets show a.ts --all".to_owned(),
        };

        assert_eq!(render(&rewrite, b"not json"), "");
        assert_eq!(render(&rewrite, br#"{"tool_input":"cat a.ts"}"#), "");
        assert_eq!(render(&rewrite, br#"{"cwd":"/repo"}"#), "");
    }

    #[test]
    fn a_read_that_needs_two_commands_stays_a_deny_on_claude_code() {
        let repo = Repo::new();
        let reason =
            block_reason(repo.classify(&repo.event("Bash", "cat src/a.ts && rg cap src/b.ts")));

        assert!(
            reason.ends_with("\nrun: lets show src/a.ts --all && lets find 'cap' src/b.ts"),
            "{reason}"
        );
    }

    fn block_reason(verdict: Verdict) -> String {
        let Verdict::Block { reason } = verdict else {
            panic!("expected a deny, got {verdict:?}");
        };
        reason
    }

    fn run_line(reason: &str) -> &str {
        reason
            .lines()
            .find_map(|line| line.strip_prefix("run: "))
            .expect("every block carries a `run: ` line")
    }

    #[test]
    fn a_codex_reason_with_codexs_command_suffix_keeps_the_run_line_runnable() {
        let repo = Repo::new();
        let reason = block_reason(repo.classify(&repo.codex_event("cat README.md")));
        let delivered = format!("{reason}. Command: cat README.md");

        assert_eq!(run_line(&delivered), "lets show README.md", "{delivered}");
    }

    #[test]
    fn a_claude_code_reason_still_ends_on_its_run_line() {
        let repo = Repo::new();
        let reason = block_reason(repo.classify(&repo.event("Bash", SEARCH)));

        assert_eq!(
            reason.lines().last(),
            Some("run: lets find 'cap' README.md"),
            "{reason}"
        );
        assert!(!reason.contains("the command above"), "{reason}");
    }

    #[test]
    fn the_claude_code_reason_under_codexs_suffix_would_break_the_run_line() {
        let repo = Repo::new();
        let reason = block_reason(repo.classify(&repo.event("Bash", SEARCH)));
        let delivered = format!("{reason}. Command: {SEARCH}");

        assert_ne!(run_line(&delivered), "lets find 'cap' README.md");
    }

    #[test]
    fn a_codex_reason_is_the_claude_code_reason_plus_the_tail() {
        let repo = Repo::new();
        let claude = block_reason(repo.classify(&repo.event("Bash", SEARCH)));
        let codex = block_reason(repo.classify(&repo.codex_event(SEARCH)));

        assert_eq!(codex, format!("{claude}{}", super::CODEX_TAIL));
    }

    #[test]
    fn bytes_that_are_not_json_are_allow() {
        let repo = Repo::new();
        assert_eq!(repo.classify(b"not json at all"), Verdict::Allow);
        assert_eq!(repo.classify(b""), Verdict::Allow);
    }

    #[test]
    fn a_tool_other_than_bash_is_allow() {
        let repo = Repo::new();
        assert_eq!(
            repo.classify(&repo.event("Read", "cat src/a.ts")),
            Verdict::Allow
        );
        assert_eq!(
            repo.classify(&repo.event("bash", "cat src/a.ts")),
            Verdict::Allow
        );
    }

    #[test]
    fn an_event_missing_a_field_this_classifier_reads_is_allow() {
        let repo = Repo::new();
        let no_command = br#"{"cwd":"/repo","tool_name":"Bash","tool_input":{}}"#;
        let no_cwd = br#"{"tool_name":"Bash","tool_input":{"command":"cat src/a.ts"}}"#;
        let wrong_type = br#"{"cwd":"/repo","tool_name":"Bash","tool_input":{"command":7}}"#;

        assert_eq!(repo.classify(no_command), Verdict::Allow);
        assert_eq!(repo.classify(no_cwd), Verdict::Allow);
        assert_eq!(repo.classify(wrong_type), Verdict::Allow);
    }

    #[test]
    fn a_panic_inside_the_walk_is_allow() {
        assert_eq!(guarded(|| panic!("the grammar blew up")), Verdict::Allow);
    }

    #[test]
    fn the_guard_returns_a_verdict_that_did_not_panic_unchanged() {
        let block = Verdict::Block {
            reason: "run: lets show a.ts".to_owned(),
        };
        assert_eq!(guarded(|| block.clone()), block);
    }

    #[test]
    fn allow_renders_nothing_at_all() {
        let repo = Repo::new();
        assert_eq!(
            render(&Verdict::Allow, &repo.event("Bash", "cat src/a.ts")),
            ""
        );
    }

    #[test]
    fn a_block_renders_one_deny_json_line() {
        let rendered = render(
            &Verdict::Block {
                reason: "run: lets show a.ts".to_owned(),
            },
            b"",
        );

        assert_eq!(
            rendered,
            "{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\
             \"permissionDecision\":\"deny\",\
             \"permissionDecisionReason\":\"run: lets show a.ts\"}}\n"
        );
    }

    #[test]
    fn a_reason_with_json_metacharacters_stays_one_parseable_line() {
        let rendered = render(
            &Verdict::Block {
                reason: "run: lets find '\"a\\b\"' src/a.ts".to_owned(),
            },
            b"",
        );

        assert_eq!(rendered.lines().count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
        assert_eq!(
            parsed["hookSpecificOutput"]["permissionDecisionReason"],
            "run: lets find '\"a\\b\"' src/a.ts"
        );
    }
}
