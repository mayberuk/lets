use std::io::Read;
use std::process::{Command, Stdio};

use tempfile::TempDir;

const GUIDE: &str = include_str!("../docs/guide.md");
const AGENTS: &str = include_str!("../docs/agents.md");

struct Run {
    code: Option<i32>,
    out: String,
    err: String,
}

fn lets(args: &[&str]) -> Run {
    let home = TempDir::new().expect("a temp HOME");
    let output = Command::new(env!("CARGO_BIN_EXE_lets"))
        .args(args)
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("XDG_RUNTIME_DIR", home.path())
        .env("LETS_NO_STATS", "1")
        .env("LETS_TOKEN_RATIO", "4")
        .output()
        .expect("the built binary runs");
    Run {
        code: output.status.code(),
        out: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        err: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

fn last_line(stream: &str) -> &str {
    stream.lines().last().unwrap_or_default()
}

#[test]
fn guide_prints_the_bytes_of_docs_guide_md() {
    let run = lets(&["guide"]);

    assert_eq!(run.code, Some(0));
    assert_eq!(
        run.out, GUIDE,
        "the guide screen is its file's bytes: no wrapping, no terminal width"
    );
    assert!(run.err.is_empty(), "{}", run.err);
}

#[test]
fn guide_json_carries_the_same_text_in_one_object() {
    let run = lets(&["guide", "--json"]);

    assert_eq!(run.code, Some(0));
    assert_eq!(run.out.lines().count(), 1, "one object, not one per line");
    let value: serde_json::Value = serde_json::from_str(&run.out).expect("valid json");
    assert_eq!(value["guide"], GUIDE);
}

/// Splits on whitespace and the exit table's `·`, so `40` in `f.ts:40-80` never reads as exit
/// `4`.
fn has_token(text: &str, token: &str) -> bool {
    text.split(|c: char| c.is_whitespace() || c == '\u{b7}')
        .any(|word| word == token)
}

#[test]
fn guide_names_every_verb_the_target_grammar_and_every_exit_code() {
    let run = lets(&["guide"]);
    assert_eq!(run.code, Some(0));

    for verb in ["show", "find", "edit", "transform", "write"] {
        assert!(
            has_token(&run.out, verb),
            "missing verb {verb}\n{}",
            run.out
        );
    }
    for token in ["f.ts:40-80", "f.ts@", "f.ts#"] {
        assert!(
            run.out.contains(token),
            "missing target token {token}\n{}",
            run.out
        );
    }
    assert!(
        run.out.contains("do not cat or sed -n"),
        "the after-an-edit line is the only place this warning is repeated"
    );

    let exit_start = run
        .out
        .lines()
        .position(|line| has_token(line, "0") && line.contains("done"))
        .expect("exit 0 is documented as \"done\"");
    let exit_block: String = run
        .out
        .lines()
        .skip(exit_start)
        .take_while(|line| !line.trim_start().starts_with("after"))
        .collect::<Vec<_>>()
        .join("\n");
    for code in 0..=8 {
        assert!(
            has_token(&exit_block, &code.to_string()),
            "exit block missing code {code}\n{exit_block}"
        );
    }

    let lines: Vec<&str> = run.out.lines().collect();
    assert!(lines.len() <= 25, "{} lines, over one screen", lines.len());
    for line in &lines {
        assert!(
            line.chars().count() <= 100,
            "{line:?} is {} columns",
            line.chars().count()
        );
    }
}

#[test]
fn version_prints_the_crate_version() {
    let run = lets(&["version"]);

    assert_eq!(run.code, Some(0));
    assert_eq!(run.out, format!("lets {}\n", env!("CARGO_PKG_VERSION")));
    assert!(run.err.is_empty(), "{}", run.err);
}

#[test]
fn version_json_carries_the_bare_semver() {
    let run = lets(&["version", "--json"]);

    assert_eq!(run.code, Some(0));
    let value: serde_json::Value = serde_json::from_str(&run.out).expect("valid json");
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn the_version_flag_and_the_version_verb_print_the_same_line() {
    let flag = lets(&["--version"]);
    let verb = lets(&["version"]);

    assert_eq!(flag.code, Some(0));
    assert_eq!(flag.out, verb.out);
}

#[test]
fn show_on_a_missing_file_exits_1_not_64() {
    let run = lets(&["show", "a.ts"]);

    assert_eq!(run.code, Some(1));
    assert_eq!(last_line(&run.err), "ERROR_CODE=not_found");
}

#[test]
fn an_unknown_verb_exits_64_not_clap_s_own_2() {
    let run = lets(&["bogus"]);

    assert_eq!(
        run.code,
        Some(64),
        "2 is the spec's ambiguous target, never a typo"
    );
    assert!(run.out.is_empty(), "{}", run.out);
    assert_eq!(last_line(&run.err), "ERROR_CODE=usage");
}

#[test]
fn a_missing_verb_and_a_missing_argument_are_both_usage_failures() {
    for args in [vec![], vec!["show"]] {
        let run = lets(&args);

        assert_eq!(run.code, Some(64), "{args:?}");
        assert_eq!(last_line(&run.err), "ERROR_CODE=usage", "{args:?}");
    }
}

#[test]
fn help_exits_0_on_stdout() {
    let run = lets(&["--help"]);

    assert_eq!(run.code, Some(0), "the control for the usage exits above");
    assert!(run.out.contains("lets"), "{}", run.out);
    assert!(run.err.is_empty(), "{}", run.err);
}

#[test]
fn agents_md_quotes_its_own_discovery_paragraph_verbatim() {
    let claude_code_paragraph = r#"> # File work: use `lets` through Bash
>
> | Instead of | Run |
> |---|---|
> | `cat a.ts b.ts`, Read | `lets show a.ts b.ts` |
> | `sed -n '40,80p' f.ts` | `lets show f.ts:40-80` |
> | find one function | `lets show f.ts#computeFee` |
> | `grep -n -A 5 'x' f.ts` | `lets show "f.ts@'x'" -A 5` |
> | `grep -rn 'x' src`, `rg x src` | `lets find 'x' src` |
> | `sed -i 's/a/b/'`, Edit | `lets edit f.ts --old a --new b` |
> | edit JSON/YAML/TOML | `lets transform f.json --set version=1.4.0` |
> | `cat > new.ts <<'EOF'` | `lets write new.ts <<'EOF'` |
>
> Several edits in one call; each `old` is exact text that occurs once:
>
> ```
> lets edit --from - <<'LETS'
> @@ a.ts
> <<<<<<< old
> cap = 10
> ======= new
> cap = 20
> >>>>>>>
> <<<<<<< old
> floor = 1
> ======= new
> floor = 2
> >>>>>>>
> LETS
> ```
>
> Do not pipe `lets` through `head`/`tail` or add `2>/dev/null`: it cuts the footer and hides the fix. Keep Read for images and PDFs; use plain Bash for anything else that is not reading, searching or editing files."#;

    assert!(
        AGENTS.contains(claude_code_paragraph),
        "docs/agents.md's Claude Code excerpt has drifted from its own text"
    );

    let subagent_start = AGENTS
        .split("## The SubagentStart line")
        .nth(1)
        .expect("docs/agents.md has a SubagentStart section");
    assert!(
        subagent_start.contains(claude_code_paragraph),
        "the SubagentStart section must quote the same paragraph as the Claude Code section, \
         not a separately authored paraphrase"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn help_redirected_to_a_full_device_exits_nonzero_with_io_error() {
    let home = TempDir::new().expect("a temp HOME");
    let dev_full = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .expect("/dev/full exists on Linux and always reports out of space");

    let output = Command::new(env!("CARGO_BIN_EXE_lets"))
        .arg("--help")
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("XDG_RUNTIME_DIR", home.path())
        .env("LETS_NO_STATS", "1")
        .env("LETS_TOKEN_RATIO", "4")
        .stdout(Stdio::from(dev_full))
        .stderr(Stdio::piped())
        .output()
        .expect("the built binary runs");

    assert_ne!(
        output.status.code(),
        Some(0),
        "a stdout write that fails must not read back as success"
    );
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert_eq!(last_line(&stderr), "ERROR_CODE=io_error", "{stderr}");
}

/// `lets show big.ts | head` is ordinary use, not a failure worth an exit code or stderr.
#[test]
fn a_reader_that_closes_the_pipe_without_reading_still_exits_0() {
    let home = TempDir::new().expect("a temp HOME");
    let mut child = Command::new(env!("CARGO_BIN_EXE_lets"))
        .arg("guide")
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("XDG_RUNTIME_DIR", home.path())
        .env("LETS_NO_STATS", "1")
        .env("LETS_TOKEN_RATIO", "4")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the built binary runs");

    drop(child.stdout.take().expect("a piped stdout handle"));

    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("a piped stderr handle")
        .read_to_string(&mut stderr)
        .expect("utf-8 stderr");
    let status = child.wait().expect("the child exits");

    assert_eq!(
        status.code(),
        Some(0),
        "a closed reader is not a failure to report"
    );
    assert!(stderr.is_empty(), "{stderr}");
}
