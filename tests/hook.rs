//! A block's replacement is executed: the original (or the case's oracle) and each `run:` line
//! run in separate fresh tree copies, and their answers are compared.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use lets::hook::{self, Answer, Verdict};
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};
use regex::Regex;
use support::sandbox::{Sandbox, sandbox};
use tempfile::TempDir;

struct Run {
    code: Option<i32>,
    out: String,
    err: String,
}

fn run_hook(event: &[u8]) -> Run {
    run_hook_with(event, &[])
}

fn run_hook_with(event: &[u8], env: &[(&str, &str)]) -> Run {
    let home = TempDir::new().expect("a temp HOME");
    run_hook_in(event, home.path(), env)
}

/// `home` is the child's HOME and working directory.
fn run_hook_in(event: &[u8], home: &Path, env: &[(&str, &str)]) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_lets"))
        .args(["hook", "classify"])
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_RUNTIME_DIR", home)
        .env("LETS_NO_STATS", "1")
        // Either would point the classifier at the runner's own Claude Code settings.
        .env_remove("CLAUDE_PROJECT_DIR")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("LETS_HOOK_LOG")
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the built binary runs");
    child
        .stdin
        .take()
        .expect("a piped stdin handle")
        .write_all(event)
        .expect("the child accepts its event on stdin");
    let output = child.wait_with_output().expect("the child exits");
    Run {
        code: output.status.code(),
        out: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        err: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

/// The extra `tool_input` fields are ones Claude Code sends, which a rewrite must hand back.
fn event(cwd: &Path, command: &str) -> Vec<u8> {
    serde_json::json!({
        "session_id": "corpus",
        "cwd": display(cwd),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": { "command": command, "description": "corpus case", "timeout": 60000 },
    })
    .to_string()
    .into_bytes()
}

/// `turn_id` is the field that marks Codex CLI, which gets no rewrite.
fn codex_event(cwd: &Path, command: &str) -> Vec<u8> {
    serde_json::json!({
        "session_id": "corpus",
        "turn_id": "corpus-turn",
        "cwd": display(cwd),
        "hook_event_name": "PreToolUse",
        "model": "gpt-5.5",
        "permission_mode": "bypassPermissions",
        "tool_name": "Bash",
        "tool_input": { "command": command },
        "tool_use_id": "call_corpus",
    })
    .to_string()
    .into_bytes()
}

fn display(path: &Path) -> &str {
    path.to_str().expect("sandbox paths are utf-8")
}

#[derive(Clone, Copy)]
enum Compare {
    Read,
    /// The original printed `cat -n`/`nl -ba` lines: each number and its line must match.
    NumberedRead,
    Search,
    SearchFiles,
    SearchCount,
    Write,
    /// A command whose other statements run beside the reads a rewrite put `lets show` in for.
    Chain,
}

/// `reason` is a block's first reason line; a rewrite carries none, since it runs silently.
struct Replacement {
    reason: Option<String>,
    runs: Vec<String>,
    compare: Compare,
    oracle: Option<String>,
}

enum Expected {
    Allow,
    Block(Replacement),
    /// Its one `run: ` line is the command `updatedInput` carries, on Claude Code and Codex alike.
    Rewrite(Replacement),
}

struct Case {
    command: String,
    expected: Expected,
    xfail: bool,
}

fn case_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/hook")
}

fn scenario_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios")
}

/// `base` is the git working tree the scope guard needs.
fn hook_tree() -> Sandbox {
    let tree = sandbox("base");
    copy_tree(&case_root().join("fixture"), tree.path());
    tree
}

fn copy_tree(source: &Path, target: &Path) {
    for entry in std::fs::read_dir(source).expect("the hook fixture overlay is readable") {
        let entry = entry.expect("a fixture entry");
        let to = target.join(entry.file_name());
        if entry.file_type().expect("a fixture entry type").is_dir() {
            std::fs::create_dir_all(&to).expect("a fixture subdirectory");
            copy_tree(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).expect("a fixture file copies");
        }
    }
}

/// Strict on order and leftovers: a misspelt field parsed as absent would silently drop its
/// check.
fn parse_case(text: &str) -> Result<Case, String> {
    let mut lines = text.lines().peekable();
    if lines.next() != Some("COMMAND") {
        return Err("the first line must be `COMMAND`".to_owned());
    }
    let command = read_block(&mut lines, "COMMAND")?.join("\n");
    let expected = match lines.next() {
        Some("VERDICT allow") => Expected::Allow,
        Some("VERDICT block") => Expected::Block(parse_replacement(&mut lines, "block")?),
        Some("VERDICT rewrite") => {
            let replacement = parse_replacement(&mut lines, "rewrite")?;
            if replacement.runs.len() != 1 {
                return Err("a rewrite case's REPLACEMENT is exactly one `run: ` line".to_owned());
            }
            Expected::Rewrite(replacement)
        },
        other => return Err(format!("unknown VERDICT line {other:?}")),
    };
    let xfail = match lines.next_if(|line| line.starts_with("XFAIL")) {
        Some(line) if line["XFAIL".len()..].trim().is_empty() => {
            return Err("XFAIL names the requirement the classifier misses".to_owned());
        },
        Some(_) => true,
        None => false,
    };
    if let Some(line) = lines.next() {
        return Err(format!("unexpected line {line:?}"));
    }
    Ok(Case {
        command,
        expected,
        xfail,
    })
}

fn parse_replacement<'a, I: Iterator<Item = &'a str>>(
    lines: &mut std::iter::Peekable<I>,
    verdict: &str,
) -> Result<Replacement, String> {
    let reason_line = lines.next_if(|line| line.starts_with("REASON "));
    let reason = match (verdict, reason_line) {
        ("block", Some(line)) => Some(line["REASON ".len()..].to_owned()),
        ("block", None) => return Err("a block case names its REASON".to_owned()),
        (_, Some(_)) => {
            return Err(format!(
                "a {verdict} case has no REASON: a rewrite runs with no message"
            ));
        },
        (_, None) => None,
    };
    if lines.next() != Some("REPLACEMENT") {
        return Err(format!("a {verdict} case names its REPLACEMENT"));
    }
    let runs: Vec<String> = read_block(lines, "REPLACEMENT")?
        .into_iter()
        .map(str::to_owned)
        .collect();
    if runs.is_empty() || !runs.iter().all(|line| line.starts_with("run: ")) {
        return Err("REPLACEMENT holds one or more `run: ` lines and nothing else".to_owned());
    }
    let compare = match lines.next().and_then(|line| line.strip_prefix("COMPARE ")) {
        Some("read") => Compare::Read,
        Some("numbered-read") => Compare::NumberedRead,
        Some("search") => Compare::Search,
        Some("search-files") => Compare::SearchFiles,
        Some("search-count") => Compare::SearchCount,
        Some("write") => Compare::Write,
        Some("chain") => Compare::Chain,
        other => return Err(format!("unknown COMPARE {other:?}")),
    };
    let oracle = match lines.next_if_eq(&"ORACLE") {
        Some(_) => Some(read_block(lines, "ORACLE")?.join("\n")),
        None => None,
    };
    Ok(Replacement {
        reason,
        runs,
        compare,
        oracle,
    })
}

fn read_block<'a>(
    lines: &mut impl Iterator<Item = &'a str>,
    field: &str,
) -> Result<Vec<&'a str>, String> {
    let mut collected = Vec::new();
    for line in lines {
        if line == "===END===" {
            return Ok(collected);
        }
        collected.push(line);
    }
    Err(format!("{field} is not closed by `===END===`"))
}

fn check_case(case: &Case) -> Vec<String> {
    let tree = hook_tree();
    let run = run_hook(&event(tree.path(), &case.command));
    if run.code != Some(0) {
        return vec![format!(
            "classify exited {:?}, stderr:\n{}",
            run.code, run.err
        )];
    }
    let mut failures = Vec::new();
    let (expected, runs) = match &case.expected {
        Expected::Allow => {
            return if run.out.is_empty() {
                Vec::new()
            } else {
                vec![format!("expected allow (empty stdout), got:\n{}", run.out)]
            };
        },
        Expected::Block(expected) => {
            let reason = match deny_reason(&run.out) {
                Ok(reason) => reason,
                Err(failure) => return vec![failure],
            };
            if reason.lines().next() != expected.reason.as_deref() {
                failures.push(format!(
                    "the reason's first line is not {:?}:\n{reason}",
                    expected.reason
                ));
            }
            let runs: Vec<String> = reason
                .lines()
                .filter(|line| line.starts_with("run: "))
                .map(str::to_owned)
                .collect();
            (expected, runs)
        },
        Expected::Rewrite(expected) => {
            let command = match rewrite_of(&run.out, &case.command) {
                Ok(command) => command,
                Err(failure) => return vec![failure],
            };
            let codex = codex_rewrites_the_same(&tree, &case.command, &command);
            if !codex.is_empty() {
                failures.push("the Codex-shaped event:".to_owned());
                failures.extend(codex);
            }
            (expected, vec![format!("run: {command}")])
        },
    };

    if runs != expected.runs {
        failures.push(format!(
            "the run lines differ from REPLACEMENT\n--- expected\n{}\n--- actual\n{}",
            expected.runs.join("\n"),
            runs.join("\n")
        ));
    }
    let oracle = expected.oracle.as_deref().unwrap_or(&case.command);
    let runs: Vec<&str> = runs.iter().map(String::as_str).collect();
    let status = match case.expected {
        Expected::Rewrite(_) => Status::Same,
        _ => Status::Zero,
    };
    failures.extend(execute(
        expected.compare,
        oracle,
        &case.command,
        &tree,
        &runs,
        status,
    ));
    failures
}

/// A rewrite runs in the original's place, so it must exit as the original did, zero or not: a
/// chain or `set -e` that stops on a failed search is legitimate. A block's `run:` lines run on
/// their own, so each must succeed.
#[derive(Clone, Copy)]
enum Status {
    Same,
    Zero,
}

/// The rewrite's command, after checking the JSON carries no permission decision and no message,
/// and hands back every other field of the tool input unchanged.
fn rewrite_of(stdout: &str, original: &str) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|error| format!("expected a rewrite, stdout is not JSON ({error}):\n{stdout}"))?;
    let output = &value["hookSpecificOutput"];
    if output.get("permissionDecision").is_some() {
        return Err(format!(
            "a rewrite carries no permissionDecision, so the user's rules still judge it:\n{stdout}"
        ));
    }
    let kept = serde_json::json!({ "description": "corpus case", "timeout": 60000 });
    rewritten_command(output, stdout, original, kept)
}

/// Codex honours `updatedInput` only beside "allow", and sends no field but `command`.
fn codex_rewrites_the_same(tree: &Sandbox, original: &str, command: &str) -> Vec<String> {
    let run = run_hook(&codex_event(tree.path(), original));
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&run.out) else {
        return vec![format!(
            "expected a rewrite, stdout is not JSON:\n{}",
            run.out
        )];
    };
    let output = &value["hookSpecificOutput"];
    if output["permissionDecision"] != "allow" {
        return vec![format!(
            "a Codex rewrite carries permissionDecision \"allow\":\n{}",
            run.out
        )];
    }
    match rewritten_command(output, &run.out, original, serde_json::json!({})) {
        Ok(codex) if codex == command => Vec::new(),
        Ok(codex) => vec![format!("Codex got `{codex}`, Claude Code got `{command}`")],
        Err(failure) => vec![failure],
    }
}

/// `kept` is every tool input field but `command`, which must come back unchanged.
fn rewritten_command(
    output: &serde_json::Value,
    stdout: &str,
    original: &str,
    kept: serde_json::Value,
) -> Result<String, String> {
    if output["hookEventName"] != "PreToolUse" {
        return Err(format!("hookEventName is not PreToolUse:\n{stdout}"));
    }
    for silent in ["additionalContext", "permissionDecisionReason"] {
        if output.get(silent).is_some() {
            return Err(format!("a rewrite carries no {silent}:\n{stdout}"));
        }
    }
    let input = &output["updatedInput"];
    let Some(command) = input["command"].as_str() else {
        return Err(format!("no updatedInput.command:\n{stdout}"));
    };
    let mut kept = kept;
    kept["command"] = command.into();
    if *input != kept {
        return Err(format!(
            "updatedInput dropped or changed a field of the tool input `{original}` sent:\n{stdout}"
        ));
    }
    Ok(command.to_owned())
}

fn deny_reason(stdout: &str) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|error| format!("expected a block, stdout is not JSON ({error}):\n{stdout}"))?;
    let output = &value["hookSpecificOutput"];
    let decision = output["permissionDecision"].as_str();
    if decision != Some("deny") {
        return Err(format!("permissionDecision was {decision:?}, not \"deny\""));
    }
    Ok(output["permissionDecisionReason"]
        .as_str()
        .unwrap_or_default()
        .to_owned())
}

/// grep names the file only when searching several, and rg numbers only on a terminal; forcing
/// `-Hn` changes how a hit prints, never which lines hit.
const HIT_FORMAT: &str = "grep() { command grep -Hn \"$@\"; }\nrg() { command rg -Hn \"$@\"; }\n";

/// `replaced` is as fresh as the oracle's copy: the classifier saw it only as a `cwd` string.
fn execute(
    compare: Compare,
    oracle: &str,
    command: &str,
    replaced: &Sandbox,
    runs: &[&str],
    status: Status,
) -> Vec<String> {
    let original = hook_tree();
    let expected = match compare {
        Compare::Search => original.bash(&format!("{HIT_FORMAT}{oracle}")),
        _ => original.bash(oracle),
    };
    let mut failures = Vec::new();
    if matches!(status, Status::Zero) && expected.code != 0 {
        failures.push(format!(
            "the oracle `{oracle}` exited {}, stderr:\n{}",
            expected.code, expected.err
        ));
    }
    let mut answer = String::new();
    let mut numbered = String::new();
    for run in runs {
        let line = run.strip_prefix("run: ").unwrap_or(run);
        let script = with_heredoc(line, command);
        let step = replaced.bash(&script);
        let wanted = match status {
            Status::Same => expected.code,
            Status::Zero => 0,
        };
        if step.code != wanted {
            failures.push(format!(
                "`{line}` exited {}, the original {wanted}, stderr:\n{}",
                step.code, step.err
            ));
        }
        answer.push_str(&step.out);
        if script.contains(NO_NUMBERS) {
            numbered.push_str(&replaced.bash(&script.replace(NO_NUMBERS, "")).out);
        } else {
            numbered.push_str(&step.out);
        }
    }
    let unnumbered = runs.iter().all(|run| run.contains(NO_NUMBERS));
    let headerless = unnumbered && runs.iter().all(|run| run.contains(" --no-header"));
    let mismatch = match compare {
        Compare::Read if headerless => compare_exact(&expected.out, &answer),
        Compare::Read if unnumbered => compare_unnumbered_read(&expected.out, &answer),
        Compare::Read => compare_read(&expected.out, &answer),
        Compare::NumberedRead => compare_numbered_read(&expected.out, &answer),
        Compare::Search => compare_search(&expected.out, &numbered),
        Compare::SearchFiles => compare_search_files(&expected.out, &numbered),
        Compare::SearchCount => compare_search_count(&expected.out, &numbered),
        Compare::Write => compare_write(&tree_bytes(original.path()), &tree_bytes(replaced.path())),
        Compare::Chain => compare_chain(&expected.out, &answer),
    };
    failures.extend(mismatch.err());
    if runs.iter().any(|run| run.contains(NO_NUMBERS)) {
        failures.extend(compare_gutter_only(&numbered, &answer).err());
    }
    failures
}

const NO_NUMBERS: &str = " --no-numbers";

/// `--no-numbers` only drops each gutter and separates non-adjacent hit groups with `--`, as
/// grep does; every other byte matches the numbered run.
fn compare_gutter_only(numbered: &str, unnumbered: &str) -> Result<(), String> {
    let gutter = Regex::new(r"^ *\d+[ :-]\t").expect("a valid pattern");
    let plain = |output: &str| -> Vec<String> {
        output
            .lines()
            .filter(|line| *line != "--")
            .map(|line| gutter.replace(line, "").into_owned())
            .collect()
    };
    let (stripped, shown) = (plain(numbered), plain(unnumbered));
    if stripped == shown {
        return Ok(());
    }
    Err(format!(
        "--no-numbers changed more than the gutter\n--- numbered, gutter stripped\n{}\n--- \
         unnumbered\n{}",
        stripped.join("\n"),
        shown.join("\n")
    ))
}

/// A read of one file with no header and no gutter prints exactly what the original printed.
fn compare_exact(oracle: &str, replacement: &str) -> Result<(), String> {
    if oracle == replacement {
        return Ok(());
    }
    Err(format!(
        "read mismatch\n--- the original printed\n{oracle:?}\n--- the replacement printed\n{replacement:?}"
    ))
}

/// Several files keep their `── ` headers; every other line is the original's.
fn compare_unnumbered_read(oracle: &str, replacement: &str) -> Result<(), String> {
    let printed: Vec<&str> = oracle.lines().collect();
    let shown: Vec<&str> = replacement
        .lines()
        .filter(|line| !line.starts_with("── "))
        .collect();
    if printed == shown {
        return Ok(());
    }
    Err(format!(
        "read mismatch\n--- the original printed\n{}\n--- the replacement shows\n{}",
        printed.join("\n"),
        shown.join("\n")
    ))
}

/// `cat -n` and `nl -ba` print `%6d\t`; `lets show` right-aligns the number, then a marker
/// column, then a tab.
fn compare_numbered_read(oracle: &str, replacement: &str) -> Result<(), String> {
    let original = Regex::new(r"^ *(\d+)\t(.*)$").expect("a valid pattern");
    let lets = Regex::new(r"^ *(\d+) \t(.*)$").expect("a valid pattern");
    let pairs = |output: &str, pattern: &Regex| -> Result<Vec<(u64, String)>, String> {
        output
            .lines()
            .filter(|line| !line.starts_with("── "))
            .map(|line| {
                pattern
                    .captures(line)
                    .map(|found| (number(&found[1]), found[2].to_owned()))
                    .ok_or_else(|| format!("numbered read mismatch: {line:?} has no number"))
            })
            .collect()
    };
    let (printed, shown) = (pairs(oracle, &original)?, pairs(replacement, &lets)?);
    if printed == shown {
        return Ok(());
    }
    Err(format!(
        "numbered read mismatch\n--- the original printed\n{printed:?}\n--- the replacement \
         shows\n{shown:?}"
    ))
}

/// A heredoc write's replacement means the same only when handed the same heredoc.
fn with_heredoc(line: &str, command: &str) -> String {
    if !line.contains("lets write") {
        return line.to_owned();
    }
    let mut lines = command.lines();
    let Some(delimiter) = lines.by_ref().find_map(heredoc_delimiter) else {
        return line.to_owned();
    };
    let mut script = format!("{line} <<'{delimiter}'\n");
    for body in lines.take_while(|body| body.trim() != delimiter) {
        script.push_str(body);
        script.push('\n');
    }
    script.push_str(&delimiter);
    script
}

fn shown_content(output: &str) -> Result<Vec<&str>, String> {
    output
        .lines()
        .filter(|line| !line.starts_with("── "))
        .map(|line| {
            let numbered = line.trim_start_matches(' ');
            let after = numbered.trim_start_matches(|c: char| c.is_ascii_digit());
            let mut marker = after.chars();
            marker
                .next()
                .filter(|_| after.len() < numbered.len())
                .and_then(|_| marker.as_str().strip_prefix('\t'))
                .ok_or_else(|| {
                    format!("read mismatch: {line:?} is not a numbered `lets show` line")
                })
        })
        .collect()
}

fn compare_read(oracle: &str, replacement: &str) -> Result<(), String> {
    let shown = shown_content(replacement)?;
    let printed: Vec<&str> = oracle.lines().collect();
    if printed == shown {
        return Ok(());
    }
    Err(format!(
        "read mismatch\n--- the original printed\n{}\n--- the replacement shows\n{}",
        printed.join("\n"),
        shown.join("\n")
    ))
}

/// Both sides lose every `── ` header and footer, and a `cat -n` or `lets show` gutter becomes its
/// bare number and a tab. A `lets show` gutter the original did not print is dropped, since a
/// deny's replacement may number a read the original did not; one it did print must carry the same
/// number. Only a line or a number the replacement added, dropped or changed is a mismatch.
fn compare_chain(oracle: &str, replacement: &str) -> Result<(), String> {
    let gutter = Regex::new(r"^ *(\d+) ?\t").expect("a valid pattern");
    let lets_gutter = Regex::new(r"^ *(\d+) \t(.*)$").expect("a valid pattern");
    let lines = |output: &str| -> Vec<String> {
        output
            .lines()
            .filter(|line| !line.starts_with("── "))
            .map(|line| gutter.replace(line, "$1\t").into_owned())
            .collect()
    };
    let printed = lines(oracle);
    let shown: Vec<String> = replacement
        .lines()
        .filter(|line| !line.starts_with("── "))
        .zip(
            printed
                .iter()
                .map(String::as_str)
                .chain(std::iter::repeat("")),
        )
        .map(|(line, original)| match lets_gutter.captures(line) {
            Some(gutter) if original == format!("{}\t{}", &gutter[1], &gutter[2]) => {
                original.to_owned()
            },
            Some(gutter) => gutter[2].to_owned(),
            None => line.to_owned(),
        })
        .collect();
    if printed == shown {
        return Ok(());
    }
    Err(format!(
        "chain mismatch\n--- the original printed\n{}\n--- the replacement printed\n{}",
        printed.join("\n"),
        shown.join("\n")
    ))
}

fn without_dot_slash(path: &str) -> &str {
    path.strip_prefix("./").unwrap_or(path)
}

fn compare_search(oracle: &str, replacement: &str) -> Result<(), String> {
    let grep_hit = Regex::new(r"^(.+?):(\d+):").expect("a valid pattern");
    let find_hit = Regex::new(r"^ *(\d+):\t").expect("a valid pattern");
    let printed: BTreeSet<(String, u64)> = oracle
        .lines()
        .filter_map(|line| grep_hit.captures(line))
        .map(|hit| (without_dot_slash(&hit[1]).to_owned(), number(&hit[2])))
        .collect();
    let mut found = BTreeSet::new();
    let mut path = "";
    for line in replacement.lines() {
        if let Some(header) = line.strip_prefix("── ") {
            path = without_dot_slash(header);
        } else if let Some(hit) = find_hit.captures(line) {
            found.insert((path.to_owned(), number(&hit[1])));
        }
    }
    if printed == found {
        return Ok(());
    }
    Err(format!(
        "search mismatch: the original hit {printed:?}, the replacement hit {found:?}"
    ))
}

fn number(digits: &str) -> u64 {
    digits.parse().expect("the pattern matched digits only")
}

fn compare_search_files(oracle: &str, replacement: &str) -> Result<(), String> {
    let printed: BTreeSet<&str> = oracle
        .lines()
        .filter(|line| !line.is_empty())
        .map(without_dot_slash)
        .collect();
    let found: BTreeSet<&str> = replacement
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with("── "))
        .collect();
    if printed == found {
        return Ok(());
    }
    Err(format!(
        "search-files mismatch: the original listed {printed:?}, the replacement listed {found:?}"
    ))
}

fn compare_search_count(oracle: &str, replacement: &str) -> Result<(), String> {
    let printed: BTreeSet<(&str, &str)> = oracle
        .lines()
        .filter_map(|line| line.rsplit_once(':'))
        .map(|(path, count)| (without_dot_slash(path), count))
        .collect();
    let found: BTreeSet<(&str, &str)> = replacement
        .lines()
        .filter(|line| !line.starts_with("── "))
        .filter_map(|line| line.trim_start().split_once("  "))
        .map(|(count, path)| (path, count))
        .collect();
    if printed == found {
        return Ok(());
    }
    Err(format!(
        "search-count mismatch: the original counted {printed:?}, the replacement counted {found:?}"
    ))
}

/// Scaffolding and git bookkeeping differ between two copies by construction.
const NOT_WRITTEN_BY_A_CASE: [&str; 4] = [".git", ".home", ".run", ".config"];

fn tree_bytes(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    collect_files(root, "", &mut files);
    files
}

fn collect_files(directory: &Path, prefix: &str, files: &mut BTreeMap<String, Vec<u8>>) {
    for entry in std::fs::read_dir(directory).expect("a sandbox directory is readable") {
        let entry = entry.expect("a sandbox entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        if prefix.is_empty() && NOT_WRITTEN_BY_A_CASE.contains(&name.as_str()) {
            continue;
        }
        let relative = format!("{prefix}{name}");
        let kind = entry.file_type().expect("a sandbox entry type");
        if kind.is_dir() {
            collect_files(&entry.path(), &format!("{relative}/"), files);
        } else if kind.is_file() {
            files.insert(
                relative,
                std::fs::read(entry.path()).expect("a sandbox file is readable"),
            );
        }
    }
}

fn compare_write(
    original: &BTreeMap<String, Vec<u8>>,
    replaced: &BTreeMap<String, Vec<u8>>,
) -> Result<(), String> {
    let differing: Vec<&str> = original
        .keys()
        .chain(replaced.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| original.get(*path) != replaced.get(*path))
        .map(String::as_str)
        .collect();
    if differing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "write mismatch: these files differ between the original and the replacement: {}",
        differing.join(", ")
    ))
}

fn names(path: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|_| panic!("no {} naming what this tier owns", path.display()));
    let mut names: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect();
    names.sort();
    names
}

const LISTS: [&str; 3] = ["required.txt", "today-verbs.txt", "today-allow.txt"];

/// Cases run from the tree; `required.txt` only guards that a named case has not vanished.
fn case_names(root: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| path.extension().is_some_and(|extension| extension == "txt"))
        .filter(|path| {
            !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| LISTS.contains(&name))
        })
        .filter_map(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .collect();
    found.sort();
    found
}

fn rg_on_path() -> bool {
    Command::new("rg")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn corpus_failures(root: &Path) -> Vec<String> {
    let present = case_names(root);
    let mut failures: Vec<String> = names(&root.join("required.txt"))
        .into_iter()
        .filter(|required| !present.contains(required))
        .map(|required| format!("{required}: named in required.txt but absent from disk"))
        .collect();

    for case_name in &present {
        let path = root.join(format!("{case_name}.txt"));
        let text = std::fs::read_to_string(&path).expect("a case found by the walk is readable");
        let case = match parse_case(&text) {
            Ok(case) => case,
            Err(error) => {
                failures.push(format!("{case_name}: {error}"));
                continue;
            },
        };
        let found = check_case(&case);
        match (case.xfail, found.is_empty()) {
            (true, true) => failures.push(format!("{case_name}: marked XFAIL but passes")),
            (false, false) => {
                failures.extend(
                    found
                        .iter()
                        .map(|failure| format!("{case_name}: {failure}")),
                );
            },
            _ => {},
        }
    }
    failures
}

#[test]
fn every_case_in_the_tree_classifies_as_declared() {
    let root = case_root();
    assert!(
        !case_names(&root).is_empty(),
        "an emptied corpus is red, not green"
    );
    assert!(
        !names(&root.join("required.txt")).is_empty(),
        "an emptied required.txt lets any case vanish"
    );
    assert!(
        rg_on_path(),
        "rg is not on PATH: the rg cases run it as their oracle, so this tier cannot pass without it"
    );

    let failures = corpus_failures(&root);

    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}

#[test]
fn a_case_named_in_required_txt_but_absent_fails_by_name() {
    let root = TempDir::new().expect("a temp case root");
    std::fs::write(
        root.path().join("required.txt"),
        "# the list itself is not a case\ndeleted-by-someone\n",
    )
    .expect("a temp required.txt is writable");

    let failures = corpus_failures(root.path());

    assert_eq!(failures, vec![
        "deleted-by-someone: named in required.txt but absent from disk".to_owned()
    ]);
}

fn case_root_holding(name: &str, case: &str) -> TempDir {
    let root = TempDir::new().expect("a temp case root");
    std::fs::write(root.path().join("required.txt"), format!("{name}\n"))
        .expect("a temp required.txt is writable");
    std::fs::write(root.path().join(format!("{name}.txt")), case).expect("a temp case is writable");
    root
}

const SHOW_A: &str = "COMMAND\ncat src/a.ts\n===END===\nVERDICT rewrite\nREPLACEMENT\nrun: lets \
                      show src/a.ts --all --no-header --no-numbers\n===END===\nCOMPARE read\n";

const EDIT_C: &str = "COMMAND\nsed -i 's/foo/baz/g' src/c.ts\n===END===\nVERDICT block\nREASON \
                      lets edit replaces the exact text and shows the changed lines.\nREPLACEMENT\n\
                      run: lets edit src/c.ts --old 'foo' --new 'baz' --all\n===END===\nCOMPARE \
                      write\n";

/// A stale xfail marker would hide the day a case starts passing.
#[test]
fn an_xfail_case_that_passes_fails_by_name() {
    let root = case_root_holding(
        "passing-read",
        &format!("{SHOW_A}XFAIL a case that meets every check\n"),
    );

    let failures = corpus_failures(root.path());

    assert_eq!(failures, vec![
        "passing-read: marked XFAIL but passes".to_owned()
    ]);
}

#[test]
fn a_failing_case_fails_by_name_unless_it_is_marked_xfail() {
    let passing = case_root_holding("passing-read", SHOW_A);
    assert_eq!(corpus_failures(passing.path()), Vec::<String>::new());

    let wrong = SHOW_A.replace("run: lets show src/a.ts", "run: lets show src/b.ts");
    assert_ne!(wrong, SHOW_A);
    let failing = case_root_holding("wrong-read", &wrong);
    let failures = corpus_failures(failing.path());
    assert!(
        !failures.is_empty()
            && failures
                .iter()
                .all(|failure| failure.starts_with("wrong-read: ")),
        "{failures:#?}"
    );

    let marked = case_root_holding(
        "wrong-read",
        &format!("{wrong}XFAIL the replacement names the other file\n"),
    );
    assert_eq!(corpus_failures(marked.path()), Vec::<String>::new());
}

#[test]
fn a_block_case_with_no_reason_fails_by_name() {
    let passing = case_root_holding("with-reason", EDIT_C);
    assert_eq!(corpus_failures(passing.path()), Vec::<String>::new());

    let no_reason: String = EDIT_C
        .split_inclusive('\n')
        .filter(|line| !line.starts_with("REASON "))
        .collect();
    let root = case_root_holding("no-reason", &no_reason);

    let failures = corpus_failures(root.path());

    assert_eq!(failures, vec![
        "no-reason: a block case names its REASON".to_owned()
    ]);
}

#[test]
fn a_rewrite_case_with_a_reason_fails_by_name() {
    let with_reason = SHOW_A.replace(
        "VERDICT rewrite\n",
        "VERDICT rewrite\nREASON lets show reads several files and ranges in one call.\n",
    );
    assert_ne!(with_reason, SHOW_A);
    let root = case_root_holding("with-reason", &with_reason);

    let failures = corpus_failures(root.path());

    assert_eq!(failures, vec![
        "with-reason: a rewrite case has no REASON: a rewrite runs with no message".to_owned()
    ]);
}

#[test]
fn a_rewrite_case_with_two_run_lines_fails_by_name() {
    let two = SHOW_A.replace(
        "run: lets show src/a.ts --all --no-header --no-numbers\n",
        "run: lets show src/a.ts --all\nrun: lets show src/b.ts --all\n",
    );
    assert_ne!(two, SHOW_A);
    let root = case_root_holding("two-runs", &two);

    let failures = corpus_failures(root.path());

    assert_eq!(failures, vec![
        "two-runs: a rewrite case's REPLACEMENT is exactly one `run: ` line".to_owned()
    ]);
}

/// A rewrite runs unseen, so the window a bare `lets show` applies would silently drop lines.
#[test]
fn a_windowed_show_of_a_file_over_the_window_is_a_read_mismatch() {
    let command = "cat long.txt";
    let replaced = hook_tree();
    let failures = execute(
        Compare::Read,
        command,
        command,
        &replaced,
        &["run: lets show long.txt"],
        Status::Zero,
    );
    assert!(
        failures
            .iter()
            .any(|failure| failure.starts_with("read mismatch")),
        "{failures:#?}"
    );

    let replaced = hook_tree();
    let failures = execute(
        Compare::Read,
        command,
        command,
        &replaced,
        &["run: lets show long.txt --all"],
        Status::Zero,
    );
    assert_eq!(failures, Vec::<String>::new());
}

/// Claude Code leaves the rewritten command to the user's permission rules; Codex needs "allow"
/// beside `updatedInput` and still runs its own approval on the result.
#[test]
fn a_codex_event_gets_the_rewrite_claude_code_gets_with_allow() {
    let tree = hook_tree();
    let claude = run_hook(&event(tree.path(), "cat src/a.ts"));
    assert_eq!(
        rewrite_of(&claude.out, "cat src/a.ts"),
        Ok("lets show src/a.ts --all --no-header --no-numbers".to_owned())
    );

    let codex = run_hook(&codex_event(tree.path(), "cat src/a.ts"));

    assert_eq!(codex.code, Some(0));
    let parsed: serde_json::Value = serde_json::from_str(&codex.out).expect("one JSON line");
    assert_eq!(
        parsed,
        serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "updatedInput": {"command": "lets show src/a.ts --all --no-header --no-numbers"},
        }})
    );
    assert_eq!(
        codex_rewrites_the_same(
            &tree,
            "cat src/a.ts",
            "lets show src/a.ts --all --no-header --no-numbers"
        ),
        Vec::<String>::new()
    );
}

/// A command with no rewrite is still Codex's deny, with its runnable replacement.
#[test]
fn a_codex_event_with_no_rewrite_denies_with_the_replacement() {
    let tree = hook_tree();
    let run = run_hook(&codex_event(tree.path(), "sed -i 's/foo/baz/g' src/c.ts"));

    assert_eq!(
        deny_reason(&run.out),
        Ok(
            "lets edit replaces the exact text and shows the changed lines.\nrun: lets edit \
             src/c.ts --old 'foo' --new 'baz' --all\nthe command above replaces the original"
                .to_owned()
        )
    );
}

#[test]
fn rewrite_json_that_decides_permission_speaks_or_drops_an_input_field_is_refused() {
    let good = r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{"command":"lets show a.ts --all","description":"corpus case","timeout":60000}}}"#;
    assert_eq!(
        rewrite_of(good, "cat a.ts"),
        Ok("lets show a.ts --all".to_owned())
    );

    let allow = good.replace(
        r#""hookEventName":"PreToolUse","#,
        r#""hookEventName":"PreToolUse","permissionDecision":"allow","#,
    );
    let context = good.replace(
        r#""hookEventName":"PreToolUse","#,
        r#""hookEventName":"PreToolUse","additionalContext":"ran instead: lets show a.ts --all","#,
    );
    let dropped = good.replace(r#","description":"corpus case""#, "");
    for bad in [allow, context, dropped] {
        assert!(rewrite_of(&bad, "cat a.ts").is_err(), "{bad}");
    }
}

#[test]
fn hook_log_names_a_file_that_gets_one_verdict_line_per_call() {
    let tree = hook_tree();
    let logs = TempDir::new().expect("a temp log directory");
    let log = logs.path().join("hook.jsonl");

    for command in ["cat src/a.ts", "ls", "sed -i 's/foo/baz/g' src/c.ts"] {
        let run = run_hook_with(&event(tree.path(), command), &[(
            "LETS_HOOK_LOG",
            log.to_str().expect("a utf-8 temp path"),
        )]);
        assert_eq!(run.code, Some(0));
    }

    assert_eq!(
        std::fs::read_to_string(&log).expect("the log was created"),
        "{\"verdict\":\"rewrite\"}\n{\"verdict\":\"allow\"}\n{\"verdict\":\"block\"}\n"
    );
}

#[test]
fn without_hook_log_no_file_is_written() {
    let tree = hook_tree();
    let home = TempDir::new().expect("a temp HOME");

    let run = run_hook_in(&event(tree.path(), "cat src/a.ts"), home.path(), &[]);

    assert!(run.out.contains("updatedInput"), "{}", run.out);
    assert_eq!(
        std::fs::read_dir(home.path())
            .expect("the temp HOME is readable")
            .count(),
        0,
        "the hook wrote into its working directory"
    );
}

#[test]
fn a_hook_log_that_cannot_be_written_changes_nothing() {
    let tree = hook_tree();
    let logs = TempDir::new().expect("a temp log directory");
    let unwritable = logs.path().join("missing/hook.jsonl");
    let event = event(tree.path(), "cat src/a.ts");

    let logged = run_hook_with(&event, &[(
        "LETS_HOOK_LOG",
        unwritable.to_str().expect("a utf-8 temp path"),
    )]);
    let plain = run_hook(&event);

    assert_eq!(logged.code, Some(0));
    assert_eq!(logged.out, plain.out);
    assert_eq!(logged.err, plain.err);
}

/// A whole-file `show` is the replacement the classifier gives for `sed -n '5p'` today.
#[test]
fn a_single_line_sed_replaced_by_a_whole_file_show_is_a_read_mismatch() {
    let command = "sed -n '5p' src/n.txt";
    let replaced = hook_tree();
    let failures = execute(
        Compare::Read,
        command,
        command,
        &replaced,
        &["run: lets show src/n.txt"],
        Status::Zero,
    );
    assert!(
        failures
            .iter()
            .any(|failure| failure.starts_with("read mismatch")),
        "{failures:#?}"
    );

    let replaced = hook_tree();
    let failures = execute(
        Compare::Read,
        command,
        command,
        &replaced,
        &["run: lets show src/n.txt:5"],
        Status::Zero,
    );
    assert_eq!(failures, Vec::<String>::new());
}

#[test]
fn a_read_whose_content_differs_by_one_line_is_a_mismatch() {
    let printed = "line 9\nline 10\n";
    let shown = "── src/a.ts:9-10  (9-10 of 30) · sha:0123456789ab\n 9 \tline 9\n10 \tline \
                 10\n── showed 1 target · 2 lines\n";

    assert_eq!(compare_read(printed, shown), Ok(()));
    let off = shown.replace("10 \tline 10", "10 \tline 11");
    assert!(
        compare_read(printed, &off).is_err_and(|failure| failure.starts_with("read mismatch")),
        "{off}"
    );
}

#[test]
fn a_chain_replacement_that_drops_or_shifts_a_cat_n_number_is_a_mismatch() {
    let command = "cat -n src/a.ts; git status";
    for wrong in [
        "run: lets show src/a.ts --all --no-header --no-numbers; git status",
        "run: lets show src/a.ts:2-30 --no-header; git status",
    ] {
        let failures = execute(
            Compare::Chain,
            command,
            command,
            &hook_tree(),
            &[wrong],
            Status::Same,
        );
        assert!(
            failures
                .iter()
                .any(|failure| failure.starts_with("chain mismatch")),
            "{wrong}: {failures:#?}"
        );
    }

    let failures = execute(
        Compare::Chain,
        command,
        command,
        &hook_tree(),
        &["run: lets show src/a.ts --all --no-header; git status"],
        Status::Same,
    );
    assert_eq!(failures, Vec::<String>::new());
}

/// grep exits 0 on any hit, so what `&&` runs after it must still run when `lets find` passes its
/// 50-hit cap: 59 lines under `src/` hold an `e`.
#[test]
fn a_rewritten_search_past_the_cap_lets_the_chain_run_on_as_grep_did() {
    let original = "grep -rn e src && echo ran-after";
    let tree = hook_tree();
    let rewritten =
        rewrite_of(&run_hook(&event(tree.path(), original)).out, original).expect("a rewrite");
    assert!(rewritten.contains(" --cap-exit-0 && "), "{rewritten}");

    let grep = tree.bash(original);
    let lets = tree.bash(&rewritten);

    assert_eq!((grep.code, grep.out.ends_with("ran-after\n")), (0, true));
    assert_eq!(
        (lets.code, lets.out.ends_with("ran-after\n")),
        (0, true),
        "{}",
        lets.err
    );
    let uncapped = tree.bash(&rewritten.replace(" --cap-exit-0", ""));
    assert_eq!(
        (uncapped.code, uncapped.out.contains("ran-after")),
        (1, false)
    );
}

#[test]
fn a_chain_replacement_that_drops_a_statement_or_changes_a_line_is_a_mismatch() {
    let command = "cat src/a.ts; git status";
    for dropped in [
        "run: lets show src/a.ts --all",
        "run: lets show src/b.ts --all; git status",
    ] {
        let failures = execute(
            Compare::Chain,
            command,
            command,
            &hook_tree(),
            &[dropped],
            Status::Same,
        );
        assert!(
            failures
                .iter()
                .any(|failure| failure.starts_with("chain mismatch")),
            "{dropped}: {failures:#?}"
        );
    }

    let failures = execute(
        Compare::Chain,
        command,
        command,
        &hook_tree(),
        &["run: lets show src/a.ts --all; git status"],
        Status::Same,
    );
    assert_eq!(failures, Vec::<String>::new());
}

/// The replacements print the same lines; only the exit status tells them apart.
#[test]
fn a_rewrite_that_exits_otherwise_than_the_original_is_a_mismatch() {
    let command = "cat src/a.ts && false";
    let failures = execute(
        Compare::Chain,
        command,
        command,
        &hook_tree(),
        &["run: lets show src/a.ts --all --no-header --no-numbers && true"],
        Status::Same,
    );
    assert!(
        failures
            .iter()
            .any(|failure| failure.contains("exited 0, the original 1")),
        "{failures:#?}"
    );

    let failures = execute(
        Compare::Chain,
        command,
        command,
        &hook_tree(),
        &["run: lets show src/a.ts --all --no-header --no-numbers && false"],
        Status::Same,
    );
    assert_eq!(failures, Vec::<String>::new());
}

#[test]
fn a_search_whose_hits_differ_by_one_is_a_mismatch() {
    let printed = "src/c.ts:1:const fooBar = 1\n./src/c.ts:3:let x = foo + fooBar\n";
    let found = "── src/c.ts\n1:\tconst «fooBar» = 1\n2-\tconst foo = 2\n3:\tlet x = foo + \
                 «fooBar»\n── 2 hits in 1 file · searched 1 file\n";

    assert_eq!(compare_search(printed, found), Ok(()));
    let extra = format!("{printed}src/c.ts:2:const foo = 2\n");
    assert!(
        compare_search(&extra, found).is_err_and(|failure| failure.starts_with("search mismatch")),
        "a context line is not a hit, so the oracle's third hit is unmatched"
    );
}

#[test]
fn a_file_list_or_a_count_that_differs_is_a_mismatch() {
    let listed = "notes.md\nsrc/c.ts\n── 2 files · searched 9 files\n";
    assert_eq!(
        compare_search_files("./src/c.ts\nnotes.md\n", listed),
        Ok(())
    );
    assert!(compare_search_files("src/c.ts\n", listed).is_err());

    let counted = "── 4 hits in 2 files\n 3  src/c.ts\n11  src/grep.txt\n";
    assert_eq!(
        compare_search_count("src/grep.txt:11\nsrc/c.ts:3\n", counted),
        Ok(())
    );
    assert!(compare_search_count("src/grep.txt:11\nsrc/c.ts:2\n", counted).is_err());
}

#[test]
fn a_write_whose_trees_differ_by_one_byte_is_a_mismatch() {
    let original = TempDir::new().expect("a temp tree");
    let replaced = TempDir::new().expect("a temp tree");
    for (root, head) in [(&original, "main"), (&replaced, "other")] {
        std::fs::create_dir_all(root.path().join("src")).expect("a temp directory");
        std::fs::create_dir_all(root.path().join(".git")).expect("a temp directory");
        std::fs::write(root.path().join("src/b.ts"), "export const b = 2;\n").expect("a temp file");
        std::fs::write(root.path().join(".git/HEAD"), head).expect("a temp file");
    }

    assert_eq!(
        compare_write(&tree_bytes(original.path()), &tree_bytes(replaced.path())),
        Ok(()),
        "git's own bookkeeping is not something the command wrote"
    );
    std::fs::write(replaced.path().join("src/b.ts"), "export const b = 3;\n").expect("a temp file");
    assert_eq!(
        compare_write(&tree_bytes(original.path()), &tree_bytes(replaced.path())),
        Err(
            "write mismatch: these files differ between the original and the replacement: \
             src/b.ts"
                .to_owned()
        )
    );
}

#[test]
fn a_lets_write_replacement_is_handed_the_commands_first_heredoc() {
    assert_eq!(
        with_heredoc(
            "lets write --force out.ts",
            "cat <<'EOF' > out.ts\nfirst\n\nlast\nEOF"
        ),
        "lets write --force out.ts <<'EOF'\nfirst\n\nlast\nEOF"
    );
    assert_eq!(
        with_heredoc("lets show src/a.ts", "cat > out.ts <<'EOF'\nx\nEOF"),
        "lets show src/a.ts"
    );
}

/// Each is an operator in one regex dialect and a literal in the other, plus plain ones.
const PATTERN_ALPHABET: [char; 17] = [
    'a', 'b', 'x', '(', ')', '{', '}', '+', '?', '|', '\\', '.', '*', '2', ',', '^', '$',
];

#[test]
fn a_replaced_plain_grep_finds_exactly_what_grep_finds() {
    replaced_grep_agrees_with_grep("");
}

#[test]
fn a_replaced_extended_grep_finds_exactly_what_grep_finds() {
    replaced_grep_agrees_with_grep("-E ");
}

/// grep is the oracle: a pattern it rejects (exit 2) must be allowed, and one it accepts must
/// be rewritten or blocked with a search hitting the same lines, or be allowed.
fn replaced_grep_agrees_with_grep(dialect: &str) {
    let tree = hook_tree();
    let patterns =
        proptest::collection::vec(proptest::sample::select(&PATTERN_ALPHABET[..]), 1..=6)
            .prop_map(|characters| characters.into_iter().collect::<String>());
    let mut runner = TestRunner::new(ProptestConfig {
        cases: 64,
        failure_persistence: None,
        ..ProptestConfig::default()
    });

    let outcome = runner.run(&patterns, |pattern| {
        agrees_with_grep(&tree, dialect, &pattern)
    });

    if let Err(failure) = outcome {
        panic!("{failure}");
    }
}

/// `\|` and `\?` match nothing as a hook literal, but `find`'s grep-style fallback reads them
/// as grep operators.
#[test]
fn a_replaced_grep_whose_literal_matches_nothing_finds_nothing_too() {
    let tree = hook_tree();
    for (dialect, pattern) in [("", "||2"), ("-E ", r"2\?a")] {
        let command = format!("grep {dialect}'{pattern}' src/grep.txt");
        assert!(
            matches!(
                hook::classify(&event(tree.path(), &command)),
                Answer::Decision(Verdict::Rewrite { .. })
            ),
            "{command} is rewritten, so its replacement is what runs"
        );
        if let Err(failure) = agrees_with_grep(&tree, dialect, pattern) {
            panic!("{failure}");
        }
    }
}

fn agrees_with_grep(tree: &Sandbox, dialect: &str, pattern: &str) -> Result<(), TestCaseError> {
    let command = format!("grep {dialect}'{pattern}' src/grep.txt");
    let replacement = match hook::classify(&event(tree.path(), &command)) {
        Answer::Decision(Verdict::Allow) => return Ok(()),
        Answer::Decision(Verdict::Rewrite { command, .. }) => command,
        Answer::Decision(Verdict::Block { reason }) => {
            let runs: Vec<&str> = reason
                .lines()
                .filter_map(|line| line.strip_prefix("run: "))
                .collect();
            let [run] = runs.as_slice() else {
                return Err(TestCaseError::fail(format!(
                    "{command}: expected one run line\n{reason}"
                )));
            };
            (*run).to_owned()
        },
        Answer::Context(text) => {
            return Err(TestCaseError::fail(format!(
                "{command}: a Bash PreToolUse event does not answer with a PostToolUse note:\n{text}"
            )));
        },
    };
    // `lets find` is smart case: a pattern with no uppercase letter matches ignoring case.
    let smart_case = if pattern.chars().any(char::is_uppercase) {
        ""
    } else {
        "-i "
    };
    let grep = tree.bash(&format!(
        "grep -Hn {smart_case}{dialect}'{pattern}' src/grep.txt"
    ));
    let found = tree.bash(&replacement.replace(NO_NUMBERS, ""));
    prop_assert!(
        matches!(grep.code, 0 | 1),
        "{command} replaced, but grep rejects the pattern (exit {}): {}",
        grep.code,
        grep.err
    );
    prop_assert_eq!(
        found.code,
        grep.code,
        "{} → `{}`: {}",
        command,
        replacement,
        found.err
    );
    let shown = tree.bash(&replacement);
    prop_assert_eq!(shown.code, found.code, "{} → `{}`", command, replacement);
    compare_gutter_only(&found.out, &shown.out)
        .and_then(|()| compare_search(&grep.out, &found.out))
        .map_err(|mismatch| TestCaseError::fail(format!("{command} → `{replacement}`: {mismatch}")))
}

#[test]
fn malformed_json_on_stdin_is_silently_allowed() {
    let run = run_hook(b"not json at all");
    assert_eq!(run.code, Some(0));
    assert!(run.out.is_empty(), "{}", run.out);
}

/// The command is one the Bash classifier blocks, so only the tool name can allow it.
#[test]
fn a_tool_other_than_bash_is_silently_allowed() {
    let tree = hook_tree();
    let read = serde_json::json!({
        "session_id": "corpus",
        "cwd": display(tree.path()),
        "hook_event_name": "PreToolUse",
        "tool_name": "Read",
        "tool_input": { "command": "cat src/a.ts" },
    })
    .to_string();

    let run = run_hook(read.as_bytes());

    assert_eq!(run.code, Some(0));
    assert!(run.out.is_empty(), "{}", run.out);
}

#[test]
fn empty_stdin_is_silently_allowed() {
    let run = run_hook(b"");
    assert_eq!(run.code, Some(0));
    assert!(run.out.is_empty(), "{}", run.out);
}

/// Driven by the tree, so a verb's scenarios are classified the day they land.
fn today_scripts(root: &Path) -> Vec<(String, String, PathBuf)> {
    let mut scripts = Vec::new();
    for verb in directories(root) {
        for scenario in directories(&root.join(&verb)) {
            let directory = root.join(&verb).join(&scenario);
            if directory.join("today/script.sh").is_file() {
                scripts.push((verb.clone(), scenario, directory));
            }
        }
    }
    scripts
}

fn directories(path: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    found
}

/// The step rules of `tests/scenarios.rs`, so a heredoc body is never cut at its own blank
/// lines or comments.
fn script_statements(script: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut block: Vec<&str> = Vec::new();
    let mut open: Option<String> = None;

    for line in script.lines() {
        if let Some(delimiter) = &open {
            block.push(line);
            if line.trim() == delimiter {
                open = None;
            }
            continue;
        }
        if line == "# ---" || line.trim().is_empty() {
            close_statement(&mut statements, &mut block);
        } else if !line.trim_start().starts_with('#') {
            open = heredoc_delimiter(line);
            block.push(line);
        }
    }
    close_statement(&mut statements, &mut block);
    statements
}

fn close_statement(statements: &mut Vec<String>, block: &mut Vec<&str>) {
    if block.is_empty() {
        return;
    }
    statements.push(block.join("\n"));
    block.clear();
}

fn heredoc_delimiter(line: &str) -> Option<String> {
    let after = line.split_once("<<")?.1;
    let after = after.strip_prefix('-').unwrap_or(after);
    if after.starts_with('<') {
        return None;
    }
    let after = after.trim_start();
    for quote in ['\'', '\"'] {
        if let Some(rest) = after.strip_prefix(quote) {
            return rest.split_once(quote).map(|(word, _)| word.to_owned());
        }
    }
    let word: String = after
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    (!word.is_empty()).then_some(word)
}

/// Verdict only: each scenario's `lets/` arm already proves the replacement. The cwd is a real
/// copy of the scenario tree, so in-tree paths are judged against it.
fn today_failures(cases: &Path, scenarios: &Path) -> Vec<String> {
    let allow: BTreeSet<String> = names(&cases.join("today-allow.txt")).into_iter().collect();
    let mut used = BTreeSet::new();
    let mut classified: BTreeSet<String> = BTreeSet::new();

    let mut failures = Vec::new();
    for (verb, scenario, directory) in today_scripts(scenarios) {
        classified.insert(verb.clone());
        let Ok(fixture) = std::fs::read_to_string(directory.join("fixture")) else {
            failures.push(format!(
                "{verb}/{scenario}: no `fixture` file naming the tree to classify against"
            ));
            continue;
        };
        let tree = sandbox(fixture.trim());
        let text = std::fs::read_to_string(directory.join("today/script.sh"))
            .expect("a today script is readable");
        for (index, statement) in script_statements(&text).into_iter().enumerate() {
            let id = format!("{verb}/{scenario}/today:{}", index + 1);
            let listed = allow.contains(&id);
            if listed {
                used.insert(id.clone());
            }
            let answer = hook::classify(&event(tree.path(), &statement));
            match answer {
                Answer::Decision(Verdict::Allow) if listed => {},
                Answer::Decision(Verdict::Allow) => failures.push(format!(
                    "{id}: the classifier allows it, but it is not named in \
                     tests/hook/today-allow.txt:\n{statement}"
                )),
                Answer::Decision(
                    Verdict::Block {
                        reason: replacement,
                    }
                    | Verdict::Rewrite {
                        command: replacement,
                    },
                ) if listed => {
                    failures.push(format!(
                        "{id}: named in tests/hook/today-allow.txt as allow, but the classifier \
                         intercepts it:\n{statement}\nreplacement: {replacement}"
                    ));
                },
                Answer::Decision(
                    Verdict::Block {
                        reason: replacement,
                    }
                    | Verdict::Rewrite {
                        command: replacement,
                    },
                ) if !replacement.contains("lets ") => {
                    failures.push(format!(
                        "{id}: intercepted with no lets command:\n{statement}"
                    ));
                },
                Answer::Decision(Verdict::Block { .. } | Verdict::Rewrite { .. }) => {},
                Answer::Context(text) => failures.push(format!(
                    "{id}: a Bash PreToolUse event does not answer with a PostToolUse note:\n{text}"
                )),
            }
        }
    }

    for verb in names(&cases.join("today-verbs.txt")) {
        if !classified.contains(&verb) {
            failures.push(format!(
                "{verb}: no tests/scenarios/{verb}/*/today/script.sh found — its part has not \
                 merged yet"
            ));
        }
    }

    for id in allow.difference(&used) {
        failures.push(format!(
            "{id}: named in tests/hook/today-allow.txt but no such statement exists"
        ));
    }

    failures
}

#[test]
fn every_today_arm_statement_not_allow_listed_blocks() {
    let cases = case_root();
    let scenarios = scenario_root();
    assert!(
        !today_scripts(&scenarios).is_empty(),
        "no today arm under {} — an emptied walk passes vacuously",
        scenarios.display()
    );
    assert!(
        !names(&cases.join("today-verbs.txt")).is_empty(),
        "today-verbs.txt names no verb whose arms must reach this walk"
    );

    let failures = today_failures(&cases, &scenarios);

    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}

#[test]
fn a_verb_named_in_today_verbs_with_no_today_script_fails_by_name() {
    let cases = TempDir::new().expect("a temp case root");
    std::fs::write(cases.path().join("today-verbs.txt"), "edit\nshow\n")
        .expect("a temp today-verbs.txt is writable");
    std::fs::write(cases.path().join("today-allow.txt"), "")
        .expect("a temp today-allow.txt is writable");
    let scenarios = TempDir::new().expect("a temp scenario root");
    let today = scenarios.path().join("show/pages/today");
    std::fs::create_dir_all(&today).expect("a temp today arm");
    std::fs::write(scenarios.path().join("show/pages/fixture"), "base\n")
        .expect("a temp fixture file");
    std::fs::write(today.join("script.sh"), "cat src/usage.ts\n").expect("a temp script.sh");

    let failures = today_failures(cases.path(), scenarios.path());

    assert_eq!(failures, vec![
        "edit: no tests/scenarios/edit/*/today/script.sh found — its part has not merged yet"
            .to_owned()
    ]);
}

/// `PostToolUse` has no command to classify, so its corpus shape names the event kind, the file
/// and the expected `additionalContext` instead of stretching the `COMMAND`/`VERDICT` format built
/// for `PreToolUse`.
fn post_tool_use_event(cwd: &Path, tool_name: &str, tool_input: &serde_json::Value) -> Vec<u8> {
    serde_json::json!({
        "session_id": "corpus",
        "cwd": display(cwd),
        "hook_event_name": "PostToolUse",
        "tool_name": tool_name,
        "tool_input": tool_input,
    })
    .to_string()
    .into_bytes()
}

fn additional_context(stdout: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(stdout).ok()?;
    value["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .map(str::to_owned)
}

#[test]
fn an_edit_on_a_clean_file_reports_ok() {
    let tree = hook_tree();
    let event = post_tool_use_event(
        tree.path(),
        "Edit",
        &serde_json::json!({
            "file_path": tree.path().join("clean.rs"),
        }),
    );

    let run = run_hook(&event);

    assert_eq!(run.code, Some(0));
    assert_eq!(
        additional_context(&run.out).as_deref(),
        Some("check: structure ok")
    );
}

#[test]
fn an_edit_that_leaves_a_syntax_error_reports_failed() {
    let tree = hook_tree();
    let file = tree.path().join("clean.rs");
    std::fs::write(&file, "fn main( {\n").expect("the edit already landed on disk");
    let event = post_tool_use_event(
        tree.path(),
        "Edit",
        &serde_json::json!({
            "file_path": file,
        }),
    );

    let run = run_hook(&event);

    assert_eq!(run.code, Some(0));
    assert_eq!(
        additional_context(&run.out).as_deref(),
        Some("check: structure failed")
    );
}

#[test]
fn an_edit_on_an_unrecognized_extension_reports_no_additional_context() {
    let tree = hook_tree();
    let event = post_tool_use_event(
        tree.path(),
        "Edit",
        &serde_json::json!({
            "file_path": tree.path().join("long.txt"),
        }),
    );

    let run = run_hook(&event);

    assert_eq!(run.code, Some(0));
    assert!(run.out.is_empty(), "{}", run.out);
}

/// Codex 0.154 hands `apply_patch`'s patch to a hook as `tool_input.command`
/// (codex-rs/core/src/tools/handlers/apply_patch.rs, `post_tool_use_payload`); the control sends
/// the same patch under a field Codex never uses.
#[test]
fn a_codex_apply_patch_event_on_the_same_clean_file_gets_the_same_ok_result() {
    let tree = hook_tree();
    let patch = "*** Begin Patch\n*** Update File: clean.rs\n*** End Patch\n";
    let codex_event = |tool_input: serde_json::Value| {
        let mut event: serde_json::Value = serde_json::from_slice(&post_tool_use_event(
            tree.path(),
            "apply_patch",
            &tool_input,
        ))
        .expect("a JSON event");
        event["turn_id"] = "corpus-turn".into();
        event.to_string().into_bytes()
    };

    let run = run_hook(&codex_event(serde_json::json!({ "command": patch })));
    let misnamed = run_hook(&codex_event(serde_json::json!({ "input": patch })));

    assert_eq!(run.code, Some(0));
    assert_eq!(
        additional_context(&run.out).as_deref(),
        Some("check: structure ok")
    );
    assert_eq!(misnamed.code, Some(0));
    assert!(misnamed.out.is_empty(), "{}", misnamed.out);
}

/// The gate for every test whose expected value comes from a real `go build`, `go vet` or
/// `go list`, mirroring how `tests/cmd`'s `check-real-golangci-lint` is skipped by name when its
/// tool is absent (CI's macOS runner has no `go` on `PATH`).
fn go_on_path() -> bool {
    Command::new("go")
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// nextest runs each test as its own process, so a `OnceLock` cannot share a warm build cache
/// across them; a fixed directory under the OS temp root does. It is warmed with the fixtures'
/// imports outside any timeout first: on a cold CI cache `go vet` of a test file took over 10 s,
/// so the hook's own timeout fired instead of the result under test.
fn go_build_cache() -> PathBuf {
    let dir = std::env::temp_dir().join("lets-hook-go-build-cache");
    std::fs::create_dir_all(&dir).expect("a shared go build cache directory");
    let _ = Command::new("go")
        .args(["build", "fmt", "os", "testing"])
        .env("GOCACHE", &dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    dir
}

/// `gomod/` carries its own `go.mod`, so an edit inside it runs the real build instead of the
/// structural (tree-sitter) fallback.
#[test]
fn an_edit_on_a_clean_go_file_inside_a_module_runs_go_build_and_reports_ok() {
    if !go_on_path() {
        eprintln!(
            "skipped (go absent): an_edit_on_a_clean_go_file_inside_a_module_runs_go_build_and_reports_ok"
        );
        return;
    }
    let tree = hook_tree();
    let event = post_tool_use_event(
        tree.path(),
        "Edit",
        &serde_json::json!({
            "file_path": tree.path().join("gomod/main.go"),
        }),
    );

    let cache = go_build_cache();
    let run = run_hook_with(&event, &[(
        "GOCACHE",
        cache.to_str().expect("a utf-8 cache path"),
    )]);

    assert_eq!(run.code, Some(0));
    assert_eq!(
        additional_context(&run.out).as_deref(),
        Some("go build: ok")
    );
}

/// Parses cleanly, so only the compiler sees what is wrong.
const GO_TYPE_ERROR: &str = "\nfunc helper() int { return \"s\" }\n";

/// Writes `file` under `gomod/`, then runs the `PostToolUse` check on it with a shared build
/// cache.
fn check_go_file(file: &str, content: &str) -> Option<String> {
    check_go_file_with(file, content, &[])
}

fn check_go_file_with(file: &str, content: &str, env: &[(&str, &str)]) -> Option<String> {
    let tree = hook_tree();
    check_go_file_in(&tree, file, content, env)
}

fn check_go_file_in(
    tree: &Sandbox,
    file: &str,
    content: &str,
    env: &[(&str, &str)],
) -> Option<String> {
    let path = tree.path().join("gomod").join(file);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("a package directory");
    std::fs::write(&path, content).expect("the edit already landed on disk");
    let event = post_tool_use_event(
        tree.path(),
        "Edit",
        &serde_json::json!({
            "file_path": path,
        }),
    );
    let cache = go_build_cache();
    let mut env = env.to_vec();
    env.push(("GOCACHE", cache.to_str().expect("a utf-8 cache path")));
    let run = run_hook_with(&event, &env);
    assert_eq!(run.code, Some(0));
    additional_context(&run.out)
}

#[test]
fn an_edit_that_breaks_go_compilation_reports_the_compilers_error() {
    if !go_on_path() {
        eprintln!(
            "skipped (go absent): an_edit_that_breaks_go_compilation_reports_the_compilers_error"
        );
        return;
    }
    let context = check_go_file(
        "main.go",
        &format!("package main\n\nfunc main() {{}}\n{GO_TYPE_ERROR}"),
    )
    .expect("a failing build reports its output");

    assert!(context.starts_with("go build:\n"), "{context}");
    assert!(context.contains("main.go"), "{context}");
}

/// A file that does not parse is reported as the structural check found it, before any build.
#[test]
fn a_go_file_that_does_not_parse_reports_the_structural_failure_without_a_build() {
    let context = check_go_file(
        "main.go",
        "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"ok\"\n}\n",
    );

    assert_eq!(context.as_deref(), Some("check: structure failed"));
}

/// `go build` compiles no `_test.go` file, so it would report ok on a broken one.
#[test]
fn a_go_test_file_that_does_not_type_check_reports_go_vets_error() {
    if !go_on_path() {
        eprintln!(
            "skipped (go absent): a_go_test_file_that_does_not_type_check_reports_go_vets_error"
        );
        return;
    }
    let context = check_go_file("main_test.go", &format!("package main\n{GO_TYPE_ERROR}"))
        .expect("a failing vet reports its output");

    assert!(context.starts_with("go vet:\n"), "{context}");
    assert!(context.contains("main_test.go"), "{context}");
}

#[test]
fn a_clean_go_test_file_reports_go_vet_ok() {
    if !go_on_path() {
        eprintln!("skipped (go absent): a_clean_go_test_file_reports_go_vet_ok");
        return;
    }
    let context = check_go_file(
        "main_test.go",
        "package main\n\nimport \"testing\"\n\nfunc TestClean(t *testing.T) {}\n",
    );

    assert_eq!(context.as_deref(), Some("go vet: ok"));
}

/// Each file here is one `go build .` leaves out, so "go build: ok" would be a claim about code
/// nothing compiled; the build-tag case above is the fourth way.
#[test]
fn a_go_file_the_build_leaves_out_reports_the_structural_result() {
    if !go_on_path() {
        eprintln!(
            "skipped (go absent): a_go_file_the_build_leaves_out_reports_the_structural_result"
        );
        return;
    }
    let broken = format!("package main\n{GO_TYPE_ERROR}");
    let other_os = if cfg!(target_os = "windows") {
        "bad_linux.go"
    } else {
        "bad_windows.go"
    };
    for file in [other_os, "_draft.go", ".draft.go"] {
        assert_eq!(
            check_go_file(file, &broken).as_deref(),
            Some("check: structure ok"),
            "{file}"
        );
    }
    assert_eq!(
        check_go_file(
            "old.go",
            &format!("// +build ignore\n\npackage main\n{GO_TYPE_ERROR}")
        )
        .as_deref(),
        Some("check: structure ok")
    );
    let cgo = format!("package main\n\nimport \"C\"\n{GO_TYPE_ERROR}");
    assert_eq!(
        check_go_file_with("cgo.go", &cgo, &[("CGO_ENABLED", "0")]).as_deref(),
        Some("check: structure ok")
    );
}

/// go prints each failing package as it finishes, so without an order of its own the 20-line cut
/// keeps different errors from run to run. Each package has 13 errors, of which go prints 10 and
/// "too many errors": 12 lines a block, 36 in all.
#[test]
fn errors_across_packages_come_back_in_the_same_order_every_run() {
    if !go_on_path() {
        eprintln!(
            "skipped (go absent): errors_across_packages_come_back_in_the_same_order_every_run"
        );
        return;
    }
    let tree = hook_tree();
    for package in ["aa", "bb", "cc"] {
        let source: String = std::iter::once(format!("package {package}\n"))
            .chain((1..=13).map(|n| format!("\nfunc f{n}() int {{ return \"s\" }}\n")))
            .collect();
        std::fs::create_dir_all(tree.path().join("gomod").join(package)).expect("a package");
        std::fs::write(
            tree.path()
                .join("gomod")
                .join(package)
                .join(format!("{package}.go")),
            source,
        )
        .expect("a failing package");
    }
    let main = "package main\n\nimport (\n\t_ \"lets-hook-fixture/aa\"\n\t_ \
                \"lets-hook-fixture/bb\"\n\t_ \"lets-hook-fixture/cc\"\n)\n\nfunc main() {}\n";

    let runs: Vec<String> = (0..4)
        .map(|_| check_go_file_in(&tree, "main.go", main, &[]).expect("a failing build"))
        .collect();

    let lines: Vec<&str> = runs[0].lines().collect();
    assert_eq!(lines.len(), 22, "{}", runs[0]);
    assert_eq!(lines[0], "go build:");
    assert_eq!(lines[1], "# lets-hook-fixture/aa");
    assert_eq!(lines[13], "# lets-hook-fixture/bb");
    assert_eq!(lines[21], "\u{2026} 16 more lines");
    assert!(!runs[0].contains("lets-hook-fixture/cc"), "{}", runs[0]);
    assert!(runs.iter().all(|run| *run == runs[0]), "{runs:#?}");
}

/// `//go:build ignore` keeps the file out of the default build, which then says nothing about it.
#[test]
fn a_go_file_behind_a_build_constraint_reports_the_structural_result() {
    if !go_on_path() {
        eprintln!(
            "skipped (go absent): a_go_file_behind_a_build_constraint_reports_the_structural_result"
        );
        return;
    }
    let context = check_go_file(
        "tagged.go",
        &format!("//go:build ignore\n\npackage main\n{GO_TYPE_ERROR}"),
    );

    assert_eq!(context.as_deref(), Some("check: structure ok"));
}

/// A `.go` file outside any module falls back to the structural check the way every other
/// language uses: `go build` would itself fail to find a module here, so nothing is spawned.
#[test]
fn an_edit_on_a_go_file_outside_any_module_falls_back_to_the_structural_check() {
    let tree = hook_tree();
    let event = post_tool_use_event(
        tree.path(),
        "Edit",
        &serde_json::json!({
            "file_path": tree.path().join("a.go"),
        }),
    );

    let run = run_hook(&event);

    assert_eq!(run.code, Some(0));
    assert_eq!(
        additional_context(&run.out).as_deref(),
        Some("check: structure ok")
    );
}

/// The negative control for the Go build path: with no `go` on `PATH`, the hook still answers,
/// falling back to the same structural result a module-less file gets.
#[test]
fn a_missing_go_binary_falls_back_to_the_structural_check() {
    let tree = hook_tree();
    let empty_path = TempDir::new().expect("a temp directory with no `go` on it");
    let event = post_tool_use_event(
        tree.path(),
        "Edit",
        &serde_json::json!({
            "file_path": tree.path().join("gomod/main.go"),
        }),
    );

    let run = run_hook_with(&event, &[(
        "PATH",
        empty_path.path().to_str().expect("a utf-8 temp path"),
    )]);

    assert_eq!(run.code, Some(0));
    assert_eq!(
        additional_context(&run.out).as_deref(),
        Some("check: structure ok")
    );
}
