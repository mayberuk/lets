use std::io::Write as _;
use std::path::Path;

use super::permissions::Sources;
use super::{bash, check};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Always carries a runnable `lets` command: a block without one leaves the agent stuck.
    Block {
        reason: String,
    },
    /// `command` replaces the whole Bash command, silently: `lets`'s own footer names anything
    /// the replacement left out.
    Rewrite {
        command: String,
    },
}

/// The hook's answer to one event. `PreToolUse` gets a `Verdict` (`bash::classify_command`'s own
/// exhaustive matches only ever see those three cases); `PostToolUse` cannot block or rewrite —
/// the tool already ran — so its only possible answer is a note, kept out of `Verdict` itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Decision(Verdict),
    Context(String),
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
struct RawEvent {
    hook_event_name: String,
    tool_name: String,
    cwd: String,
    tool_input: RawToolInput,
    turn_id: Option<serde::de::IgnoredAny>,
}

impl RawEvent {
    /// Codex CLI sends `turn_id` and Claude Code does not.
    fn is_codex(&self) -> bool {
        self.turn_id.is_some()
    }
}

/// Codex CLI 0.154 appends `. Command: <original>` to a deny reason; ending on this line keeps
/// that suffix off the `run: ` line.
const CODEX_TAIL: &str = "\nthe command above replaces the original";

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
struct RawToolInput {
    /// A Bash command, or Codex's `apply_patch` patch text: Codex 0.154 hands a hook the patch as
    /// `command` (codex-rs/core/src/tools/handlers/apply_patch.rs, `post_tool_use_payload`).
    command: Option<String>,
    /// `Edit`/`Write`'s edited-file field, on both harnesses.
    file_path: Option<String>,
}

/// The first `*** Update File: `/`*** Add File: ` line: a best-effort check on one file, not a
/// claim every file a multi-hunk patch touched was checked.
fn patched_file(patch: &str) -> Option<String> {
    patch
        .lines()
        .find_map(|line| {
            line.strip_prefix("*** Update File: ")
                .or_else(|| line.strip_prefix("*** Add File: "))
        })
        .map(str::to_owned)
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct HookOutput<'a> {
    hook_specific_output: HookSpecificOutput<'a>,
}

/// A Claude Code rewrite carries no `permissionDecision`: "allow" would skip the prompt the user's
/// settings give the rewritten command, while none leaves it to "the normal permission
/// evaluation" (code.claude.com/docs/en/agent-sdk/hooks, "Modify tool input"). Codex honours
/// `updatedInput` only beside "allow", and still runs its own approval and sandbox on the result
/// (codex-rs/hooks/src/engine/output_parser.rs).
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

/// With `LETS_HOOK_LOG` naming a file, appends the answer's kind to it, so a trial can count
/// rewrites that leave no trace in the transcript.
pub fn classify(stdin: &[u8]) -> Answer {
    let answer = classify_with(stdin, Sources::from_env);
    if let Some(log) = std::env::var_os("LETS_HOOK_LOG").filter(|log| !log.is_empty()) {
        log_verdict(Path::new(&log), &answer);
    }
    answer
}

/// An I/O error is dropped: the log never changes what the hook answers.
fn log_verdict(log: &Path, answer: &Answer) {
    let kind = match answer {
        Answer::Decision(Verdict::Allow) => "allow",
        Answer::Decision(Verdict::Block { .. }) => "block",
        Answer::Decision(Verdict::Rewrite { .. }) => "rewrite",
        Answer::Context(_) => "context",
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        let _ = file.write_all(format!("{{\"verdict\":\"{kind}\"}}\n").as_bytes());
    }
}

/// `sources` is called only for a Claude Code event, whose settings a rewrite must honour. One
/// event is one command: Codex sends each call of a code-mode batch as its own event.
fn classify_with(stdin: &[u8], sources: impl FnOnce() -> Sources) -> Answer {
    guarded(|| {
        let Ok(event) = serde_json::from_slice::<RawEvent>(stdin) else {
            return Answer::Decision(Verdict::Allow);
        };
        match event.hook_event_name.as_str() {
            "PreToolUse" => Answer::Decision(classify_pre_tool_use(&event, sources)),
            "PostToolUse" => classify_post_tool_use(&event),
            _ => Answer::Decision(Verdict::Allow),
        }
    })
}

fn classify_pre_tool_use(event: &RawEvent, sources: impl FnOnce() -> Sources) -> Verdict {
    if event.tool_name != "Bash" {
        return Verdict::Allow;
    }
    let Some(command) = event.tool_input.command.as_deref() else {
        return Verdict::Allow;
    };
    let claude_code = !event.is_codex();
    let sources = claude_code.then(sources);
    let verdict = bash::classify_command(command, &event.cwd, sources.as_ref());
    match verdict {
        Verdict::Block { mut reason } if !claude_code => {
            reason.push_str(CODEX_TAIL);
            Verdict::Block { reason }
        },
        verdict => verdict,
    }
}

/// Serves Claude Code's `Edit`/`Write` and Codex's `apply_patch` alike: the dispatch on
/// `tool_name` only decides how to find the edited path, which is all that differs between them.
fn classify_post_tool_use(event: &RawEvent) -> Answer {
    let path = match event.tool_name.as_str() {
        "Edit" | "Write" => event.tool_input.file_path.clone(),
        "apply_patch" => event.tool_input.command.as_deref().and_then(patched_file),
        _ => None,
    };
    let Some(path) = path else {
        return Answer::Decision(Verdict::Allow);
    };
    match check::evaluate(Path::new(&path), Path::new(&event.cwd)) {
        Some(text) => Answer::Context(text),
        None => Answer::Decision(Verdict::Allow),
    }
}

/// A serialization failure renders empty, which the caller reads as no objection. `event` is the
/// hook's stdin: `updatedInput` replaces the whole tool input, so a rewrite copies every other
/// field from it.
pub fn render(answer: &Answer, event: &[u8]) -> String {
    let specific = match answer {
        Answer::Decision(Verdict::Allow) => return String::new(),
        Answer::Decision(Verdict::Block { reason }) => HookSpecificOutput {
            hook_event_name: "PreToolUse",
            permission_decision: Some("deny"),
            permission_decision_reason: Some(reason),
            updated_input: None,
            additional_context: None,
        },
        Answer::Decision(Verdict::Rewrite { command }) => {
            let Some((input, codex)) = rewritten_input(event, command) else {
                return String::new();
            };
            HookSpecificOutput {
                hook_event_name: "PreToolUse",
                permission_decision: codex.then_some("allow"),
                permission_decision_reason: None,
                updated_input: Some(input),
                additional_context: None,
            }
        },
        Answer::Context(text) => HookSpecificOutput {
            hook_event_name: "PostToolUse",
            permission_decision: None,
            permission_decision_reason: None,
            updated_input: None,
            additional_context: Some(text),
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

/// The tool input with its command replaced, and whether the event came from Codex.
fn rewritten_input(
    event: &[u8],
    command: &str,
) -> Option<(serde_json::Map<String, serde_json::Value>, bool)> {
    let codex = serde_json::from_slice::<RawEvent>(event).ok()?.is_codex();
    let serde_json::Value::Object(mut event) = serde_json::from_slice(event).ok()? else {
        return None;
    };
    let serde_json::Value::Object(mut input) = event.remove("tool_input")? else {
        return None;
    };
    input.insert("command".to_owned(), command.into());
    Some((input, codex))
}

/// The release profile aborts on panic instead: the child dies on a signal, not the exit 2 a
/// `PreToolUse` hook blocks with, so a crash is still allow.
fn guarded(classify: impl FnOnce() -> Answer) -> Answer {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(classify))
        .unwrap_or(Answer::Decision(Verdict::Allow))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::{Answer, Sources, Verdict, classify_with, guarded, log_verdict, render};

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

        fn classify(&self, event: &[u8]) -> Answer {
            classify_with(event, || Sources {
                home: Some(self.0.path().join(".home")),
                config: None,
                project: None,
                managed: self.0.path().join(".managed"),
            })
        }
    }

    fn decision(verdict: Verdict) -> Answer {
        Answer::Decision(verdict)
    }

    /// `sed -i` has no rewrite, so it stays a deny on both harnesses, with the same reason.
    const EDIT: &str = "sed -i 's/cap/limit/g' README.md && ls";

    #[test]
    fn a_claude_code_read_of_two_repo_files_rewrites_to_one_show_of_both() {
        let repo = Repo::new();
        let event = repo.event("Bash", "cat src/a.ts && cat src/b.ts");

        assert_eq!(
            repo.classify(&event),
            decision(Verdict::Rewrite {
                command: "lets show src/a.ts src/b.ts --all --no-numbers".to_owned()
            })
        );
    }

    #[test]
    fn a_codex_read_is_the_same_rewrite_claude_code_gets() {
        let repo = Repo::new();

        for command in ["cat src/a.ts", "rg cap README.md && ls"] {
            assert_eq!(
                repo.classify(&repo.codex_event(command)),
                repo.classify(&repo.event("Bash", command)),
                "{command}"
            );
        }
        assert_eq!(
            repo.classify(&repo.codex_event("cat src/a.ts")),
            decision(Verdict::Rewrite {
                command: "lets show src/a.ts --all --no-header --no-numbers".to_owned()
            })
        );
    }

    #[test]
    fn a_codex_rewrite_renders_allow_beside_updated_input() {
        let repo = Repo::new();
        let event = repo.codex_event("nl -ba src/a.ts | sed -n '1,1p'");
        let answer = repo.classify(&event);

        let rendered = render(&answer, &event);

        let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
        assert_eq!(
            parsed,
            serde_json::json!({"hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "allow",
                "updatedInput": {"command": "lets show src/a.ts:1 --no-header"},
            }})
        );
    }

    #[test]
    fn the_same_rewrite_on_claude_code_renders_no_permission_decision() {
        let repo = Repo::new();
        let event = repo.event("Bash", "nl -ba src/a.ts | sed -n '1,1p'");
        let answer = repo.classify(&event);

        let rendered = render(&answer, &event);

        let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
        assert_eq!(
            parsed,
            serde_json::json!({"hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "updatedInput": {"command": "lets show src/a.ts:1 --no-header"},
            }})
        );
    }

    #[test]
    fn two_codex_events_back_to_back_classify_independently() {
        let repo = Repo::new();

        let first = repo.classify(&repo.codex_event("cat src/a.ts"));
        let second = repo.classify(&repo.codex_event("sed -i 's/cap/limit/g' src/b.ts"));
        let third = repo.classify(&repo.codex_event("ls src"));

        assert_eq!(
            first,
            decision(Verdict::Rewrite {
                command: "lets show src/a.ts --all --no-header --no-numbers".to_owned()
            })
        );
        assert!(
            block_reason(second).contains("\nrun: lets edit src/b.ts --old 'cap' --new 'limit'")
        );
        assert_eq!(third, decision(Verdict::Allow));
    }

    #[test]
    fn a_rewrite_renders_updated_input_with_no_permission_decision() {
        let repo = Repo::new();
        let event = format!(
            r#"{{"session_id":"s","cwd":"{}","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{{"command":"cat src/a.ts","description":"Read a","timeout":5000}}}}"#,
            repo.cwd()
        );
        let answer = repo.classify(event.as_bytes());

        let rendered = render(&answer, event.as_bytes());

        let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
        assert_eq!(
            parsed,
            serde_json::json!({"hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "updatedInput": {
                    "command": "lets show src/a.ts --all --no-header --no-numbers",
                    "description": "Read a",
                    "timeout": 5000,
                },
            }})
        );
        assert_eq!(rendered.lines().count(), 1);
    }

    /// Classify and render answer "is this Codex" with the same test, so a `null` `turn_id` gets
    /// a Claude Code rewrite from both.
    #[test]
    fn a_null_turn_id_renders_the_claude_code_rewrite_it_was_classified_as() {
        let repo = Repo::new();
        let event = format!(
            r#"{{"session_id":"s","turn_id":null,"cwd":"{}","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{{"command":"cat src/a.ts"}}}}"#,
            repo.cwd()
        );
        let answer = repo.classify(event.as_bytes());

        let rendered = render(&answer, event.as_bytes());

        let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
        assert_eq!(
            parsed,
            serde_json::json!({"hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "updatedInput": {"command": "lets show src/a.ts --all --no-header --no-numbers"},
            }})
        );
    }

    #[test]
    fn a_rewrite_whose_event_cannot_be_read_back_renders_nothing() {
        let rewrite = decision(Verdict::Rewrite {
            command: "lets show a.ts --all".to_owned(),
        });

        assert_eq!(render(&rewrite, b"not json"), "");
        assert_eq!(render(&rewrite, br#"{"tool_input":"cat a.ts"}"#), "");
        assert_eq!(render(&rewrite, br#"{"cwd":"/repo"}"#), "");
    }

    #[test]
    fn a_chain_whose_search_status_an_operator_reads_is_rewritten_to_exit_as_grep_would() {
        let repo = Repo::new();

        assert_eq!(
            repo.classify(&repo.event("Bash", "cat src/a.ts && rg cap src/b.ts && ls")),
            decision(Verdict::Rewrite {
                command: "lets show src/a.ts --all --no-header --no-numbers && lets find 'cap' \
                          src/b.ts --no-numbers -s --cap-exit-0 && ls"
                    .to_owned()
            })
        );
    }

    #[test]
    fn a_search_is_rewritten_on_both_harnesses() {
        let repo = Repo::new();
        let rewrite = decision(Verdict::Rewrite {
            command: "lets find 'cap' README.md --no-numbers".to_owned(),
        });

        assert_eq!(
            repo.classify(&repo.event("Bash", "rg cap README.md")),
            rewrite
        );
        assert_eq!(
            repo.classify(&repo.codex_event("rg cap README.md")),
            rewrite
        );
    }

    fn block_reason(answer: Answer) -> String {
        let Answer::Decision(Verdict::Block { reason }) = answer else {
            panic!("expected a deny, got {answer:?}");
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
        let original = "sed -i 's/cap/limit/g' README.md";
        let reason = block_reason(repo.classify(&repo.codex_event(original)));
        let delivered = format!("{reason}. Command: {original}");

        assert_eq!(
            run_line(&delivered),
            "lets edit README.md --old 'cap' --new 'limit' --all",
            "{delivered}"
        );
    }

    #[test]
    fn a_claude_code_reason_still_ends_on_its_run_line() {
        let repo = Repo::new();
        let reason = block_reason(repo.classify(&repo.event("Bash", EDIT)));

        assert_eq!(
            reason.lines().last(),
            Some("run: lets edit README.md --old 'cap' --new 'limit' --all && ls"),
            "{reason}"
        );
        assert!(!reason.contains("the command above"), "{reason}");
    }

    #[test]
    fn the_claude_code_reason_under_codexs_suffix_would_break_the_run_line() {
        let repo = Repo::new();
        let reason = block_reason(repo.classify(&repo.event("Bash", EDIT)));
        let delivered = format!("{reason}. Command: {EDIT}");

        assert_ne!(
            run_line(&delivered),
            "lets edit README.md --old 'cap' --new 'limit' --all && ls"
        );
    }

    #[test]
    fn a_codex_reason_is_the_claude_code_reason_plus_the_tail() {
        let repo = Repo::new();
        let claude = block_reason(repo.classify(&repo.event("Bash", EDIT)));
        let codex = block_reason(repo.classify(&repo.codex_event(EDIT)));

        assert_eq!(codex, format!("{claude}{}", super::CODEX_TAIL));
    }

    #[test]
    fn bytes_that_are_not_json_are_allow() {
        let repo = Repo::new();
        assert_eq!(repo.classify(b"not json at all"), decision(Verdict::Allow));
        assert_eq!(repo.classify(b""), decision(Verdict::Allow));
    }

    #[test]
    fn a_tool_other_than_bash_is_allow() {
        let repo = Repo::new();
        assert_eq!(
            repo.classify(&repo.event("Read", "cat src/a.ts")),
            decision(Verdict::Allow)
        );
        assert_eq!(
            repo.classify(&repo.event("bash", "cat src/a.ts")),
            decision(Verdict::Allow)
        );
    }

    #[test]
    fn an_event_missing_a_field_this_classifier_reads_is_allow() {
        let repo = Repo::new();
        let no_command =
            br#"{"hook_event_name":"PreToolUse","cwd":"/repo","tool_name":"Bash","tool_input":{}}"#;
        let no_cwd =
            br#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cat src/a.ts"}}"#;
        let wrong_type = br#"{"hook_event_name":"PreToolUse","cwd":"/repo","tool_name":"Bash","tool_input":{"command":7}}"#;

        assert_eq!(repo.classify(no_command), decision(Verdict::Allow));
        assert_eq!(repo.classify(no_cwd), decision(Verdict::Allow));
        assert_eq!(repo.classify(wrong_type), decision(Verdict::Allow));
    }

    #[test]
    fn a_panic_inside_the_walk_is_allow() {
        assert_eq!(
            guarded(|| panic!("the grammar blew up")),
            decision(Verdict::Allow)
        );
    }

    #[test]
    fn the_guard_returns_a_verdict_that_did_not_panic_unchanged() {
        let block = decision(Verdict::Block {
            reason: "run: lets show a.ts".to_owned(),
        });
        assert_eq!(guarded(|| block.clone()), block);
    }

    #[test]
    fn allow_renders_nothing_at_all() {
        let repo = Repo::new();
        assert_eq!(
            render(
                &decision(Verdict::Allow),
                &repo.event("Bash", "cat src/a.ts")
            ),
            ""
        );
    }

    #[test]
    fn a_block_renders_one_deny_json_line() {
        let rendered = render(
            &decision(Verdict::Block {
                reason: "run: lets show a.ts".to_owned(),
            }),
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
            &decision(Verdict::Block {
                reason: "run: lets find '\"a\\b\"' src/a.ts".to_owned(),
            }),
            b"",
        );

        assert_eq!(rendered.lines().count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
        assert_eq!(
            parsed["hookSpecificOutput"]["permissionDecisionReason"],
            "run: lets find '\"a\\b\"' src/a.ts"
        );
    }

    #[test]
    fn a_context_renders_additional_context_with_no_decision_field() {
        let rendered = render(&Answer::Context("check: rust ok".to_owned()), b"");

        assert_eq!(
            rendered,
            "{\"hookSpecificOutput\":{\"hookEventName\":\"PostToolUse\",\
             \"additionalContext\":\"check: rust ok\"}}\n"
        );
    }

    #[test]
    fn each_logged_verdict_appends_one_line_naming_its_kind() {
        let dir = TempDir::new().expect("a temp directory");
        let log = dir.path().join("hook.jsonl");

        for answer in [
            decision(Verdict::Rewrite {
                command: "lets show a.ts --all".to_owned(),
            }),
            decision(Verdict::Allow),
            decision(Verdict::Block {
                reason: "run: lets show a.ts".to_owned(),
            }),
            Answer::Context("check: rust ok".to_owned()),
        ] {
            log_verdict(&log, &answer);
        }

        assert_eq!(
            std::fs::read_to_string(&log).expect("the log was created"),
            "{\"verdict\":\"rewrite\"}\n{\"verdict\":\"allow\"}\n{\"verdict\":\"block\"}\n\
             {\"verdict\":\"context\"}\n"
        );
    }

    #[test]
    fn a_log_that_cannot_be_opened_is_dropped_without_a_panic() {
        let dir = TempDir::new().expect("a temp directory");

        log_verdict(dir.path(), &decision(Verdict::Allow));
        log_verdict(
            &dir.path().join("missing/hook.jsonl"),
            &decision(Verdict::Allow),
        );

        assert!(!dir.path().join("missing").exists());
    }
}
