use super::bash;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Always carries a runnable `lets` command: a block without one leaves the agent stuck.
    Block {
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

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct HookSpecificOutput<'a> {
    hook_event_name: &'static str,
    permission_decision: &'static str,
    permission_decision_reason: &'a str,
}

pub fn classify(stdin: &[u8]) -> Verdict {
    guarded(|| {
        let Ok(event) = serde_json::from_slice::<RawEvent>(stdin) else {
            return Verdict::Allow;
        };
        if event.tool_name != "Bash" {
            return Verdict::Allow;
        }
        let verdict = bash::classify_command(&event.tool_input.command, &event.cwd);
        match verdict {
            Verdict::Block { mut reason } if event.turn_id.is_some() => {
                reason.push_str(CODEX_TAIL);
                Verdict::Block { reason }
            },
            verdict => verdict,
        }
    })
}

/// A serialization failure renders empty, which Claude Code reads as no objection.
pub fn render(verdict: &Verdict) -> String {
    let Verdict::Block { reason } = verdict else {
        return String::new();
    };
    let output = HookOutput {
        hook_specific_output: HookSpecificOutput {
            hook_event_name: "PreToolUse",
            permission_decision: "deny",
            permission_decision_reason: reason,
        },
    };
    match serde_json::to_string(&output) {
        Ok(json) => format!("{json}\n"),
        Err(_) => String::new(),
    }
}

/// The release profile aborts on panic instead: the child dies on a signal, not the exit 2 a
/// `PreToolUse` hook blocks with, so a crash is still allow.
fn guarded(classify: impl FnOnce() -> Verdict) -> Verdict {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(classify)).unwrap_or(Verdict::Allow)
}

#[cfg(test)]
mod tests {
    use super::{Verdict, classify, guarded, render};

    fn event(tool: &str, command: &str) -> Vec<u8> {
        format!(
            r#"{{"session_id":"s","transcript_path":"/t","cwd":"/repo","hook_event_name":"PreToolUse","tool_name":"{tool}","tool_input":{{"command":"{command}"}}}}"#
        )
        .into_bytes()
    }

    #[test]
    fn a_well_formed_bash_event_over_a_repo_read_blocks() {
        let event = event("Bash", "cat src/a.ts && cat src/b.ts");
        let Verdict::Block { reason } = classify(&event) else {
            panic!("a displayed repo read is on `.claude/rules/hook.md`'s Block list");
        };
        assert!(
            reason.contains("run: lets show src/a.ts src/b.ts"),
            "{reason}"
        );
    }

    const CODEX_EVENT: &[u8] = br#"{"session_id":"00000000-0000-7000-8000-000000000001","turn_id":"00000000-0000-7000-8000-000000000002","transcript_path":"/home/user/.codex/sessions/2026/09/22/rollout.jsonl","cwd":"/repo","hook_event_name":"PreToolUse","model":"gpt-5.5","permission_mode":"bypassPermissions","tool_name":"Bash","tool_input":{"command":"cat README.md"},"tool_use_id":"call_example"}"#;

    fn block_reason(verdict: Verdict) -> String {
        let Verdict::Block { reason } = verdict else {
            panic!("a displayed repo read is on `.claude/rules/hook.md`'s Block list");
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
        let reason = block_reason(classify(CODEX_EVENT));
        let delivered = format!("{reason}. Command: cat README.md");

        assert_eq!(run_line(&delivered), "lets show README.md", "{delivered}");
    }

    #[test]
    fn a_claude_code_reason_still_ends_on_its_run_line() {
        let reason = block_reason(classify(&event("Bash", "cat README.md")));

        assert_eq!(
            reason.lines().last(),
            Some("run: lets show README.md"),
            "{reason}"
        );
        assert!(!reason.contains("the command above"), "{reason}");
    }

    #[test]
    fn the_claude_code_reason_under_codexs_suffix_would_break_the_run_line() {
        let reason = block_reason(classify(&event("Bash", "cat README.md")));
        let delivered = format!("{reason}. Command: cat README.md");

        assert_ne!(run_line(&delivered), "lets show README.md");
    }

    #[test]
    fn a_codex_reason_is_the_claude_code_reason_plus_the_tail() {
        let claude = block_reason(classify(&event("Bash", "cat README.md")));
        let codex = block_reason(classify(CODEX_EVENT));

        assert_eq!(codex, format!("{claude}{}", super::CODEX_TAIL));
    }

    #[test]
    fn bytes_that_are_not_json_are_allow() {
        assert_eq!(classify(b"not json at all"), Verdict::Allow);
        assert_eq!(classify(b""), Verdict::Allow);
    }

    #[test]
    fn a_tool_other_than_bash_is_allow() {
        assert_eq!(classify(&event("Read", "cat src/a.ts")), Verdict::Allow);
        assert_eq!(classify(&event("bash", "cat src/a.ts")), Verdict::Allow);
    }

    #[test]
    fn an_event_missing_a_field_this_classifier_reads_is_allow() {
        let no_command = br#"{"cwd":"/repo","tool_name":"Bash","tool_input":{}}"#;
        let no_cwd = br#"{"tool_name":"Bash","tool_input":{"command":"cat src/a.ts"}}"#;
        let wrong_type = br#"{"cwd":"/repo","tool_name":"Bash","tool_input":{"command":7}}"#;

        assert_eq!(classify(no_command), Verdict::Allow);
        assert_eq!(classify(no_cwd), Verdict::Allow);
        assert_eq!(classify(wrong_type), Verdict::Allow);
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
        assert_eq!(render(&Verdict::Allow), "");
    }

    #[test]
    fn a_block_renders_one_deny_json_line() {
        let rendered = render(&Verdict::Block {
            reason: "run: lets show a.ts".to_owned(),
        });

        assert_eq!(
            rendered,
            "{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\
             \"permissionDecision\":\"deny\",\
             \"permissionDecisionReason\":\"run: lets show a.ts\"}}\n"
        );
    }

    #[test]
    fn a_reason_with_json_metacharacters_stays_one_parseable_line() {
        let rendered = render(&Verdict::Block {
            reason: "run: lets find '\"a\\b\"' src/a.ts".to_owned(),
        });

        assert_eq!(rendered.lines().count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
        assert_eq!(
            parsed["hookSpecificOutput"]["permissionDecisionReason"],
            "run: lets find '\"a\\b\"' src/a.ts"
        );
    }
}
