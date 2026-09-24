//! A block's replacement is executed: the original (or the case's oracle) and each `run:` line
//! run in separate fresh tree copies, and their answers are compared.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use lets::hook::{self, Verdict};
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
    let home = TempDir::new().expect("a temp HOME");
    let mut child = Command::new(env!("CARGO_BIN_EXE_lets"))
        .args(["hook", "classify"])
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("XDG_RUNTIME_DIR", home.path())
        .env("LETS_NO_STATS", "1")
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

fn event(cwd: &Path, command: &str) -> Vec<u8> {
    serde_json::json!({
        "session_id": "corpus",
        "cwd": display(cwd),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": { "command": command },
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
    Search,
    SearchFiles,
    SearchCount,
    Write,
}

enum Expected {
    Allow,
    Block {
        reason: String,
        replacement: Vec<String>,
        compare: Compare,
        oracle: Option<String>,
    },
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
        Some("VERDICT block") => {
            let Some(reason_line) = lines.next_if(|line| line.starts_with("REASON ")) else {
                return Err("a block case names its REASON".to_owned());
            };
            let reason = reason_line["REASON ".len()..].to_owned();
            if lines.next() != Some("REPLACEMENT") {
                return Err("a block case names its REPLACEMENT".to_owned());
            }
            let replacement: Vec<String> = read_block(&mut lines, "REPLACEMENT")?
                .into_iter()
                .map(str::to_owned)
                .collect();
            if replacement.is_empty() || !replacement.iter().all(|line| line.starts_with("run: ")) {
                return Err(
                    "REPLACEMENT holds one or more `run: ` lines and nothing else".to_owned(),
                );
            }
            let compare = match lines.next().and_then(|line| line.strip_prefix("COMPARE ")) {
                Some("read") => Compare::Read,
                Some("search") => Compare::Search,
                Some("search-files") => Compare::SearchFiles,
                Some("search-count") => Compare::SearchCount,
                Some("write") => Compare::Write,
                other => return Err(format!("unknown COMPARE {other:?}")),
            };
            let oracle = match lines.next_if_eq(&"ORACLE") {
                Some(_) => Some(read_block(&mut lines, "ORACLE")?.join("\n")),
                None => None,
            };
            Expected::Block {
                reason,
                replacement,
                compare,
                oracle,
            }
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
    let Expected::Block {
        reason: expected_reason,
        replacement,
        compare,
        oracle,
    } = &case.expected
    else {
        return if run.out.is_empty() {
            Vec::new()
        } else {
            vec![format!("expected allow (empty stdout), got:\n{}", run.out)]
        };
    };
    let reason = match deny_reason(&run.out) {
        Ok(reason) => reason,
        Err(failure) => return vec![failure],
    };

    let mut failures = Vec::new();
    if reason.lines().next() != Some(expected_reason.as_str()) {
        failures.push(format!(
            "the reason's first line is not {expected_reason:?}:\n{reason}"
        ));
    }
    let runs: Vec<&str> = reason
        .lines()
        .filter(|line| line.starts_with("run: "))
        .collect();
    if runs != *replacement {
        failures.push(format!(
            "the run lines differ from REPLACEMENT\n--- expected\n{}\n--- actual\n{}",
            replacement.join("\n"),
            runs.join("\n")
        ));
    }
    let oracle = oracle.as_deref().unwrap_or(&case.command);
    failures.extend(execute(*compare, oracle, &case.command, &tree, &runs));
    failures
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
) -> Vec<String> {
    let original = hook_tree();
    let expected = match compare {
        Compare::Search => original.bash(&format!("{HIT_FORMAT}{oracle}")),
        _ => original.bash(oracle),
    };
    let mut failures = Vec::new();
    if expected.code != 0 {
        failures.push(format!(
            "the oracle `{oracle}` exited {}, stderr:\n{}",
            expected.code, expected.err
        ));
    }
    let mut answer = String::new();
    for run in runs {
        let line = run.strip_prefix("run: ").unwrap_or(run);
        let script = with_heredoc(line, command);
        let step = replaced.bash(&script);
        if step.code != 0 {
            failures.push(format!(
                "`{line}` exited {}, stderr:\n{}",
                step.code, step.err
            ));
        }
        answer.push_str(&step.out);
    }
    let mismatch = match compare {
        Compare::Read => compare_read(&expected.out, &answer),
        Compare::Search => compare_search(&expected.out, &answer),
        Compare::SearchFiles => compare_search_files(&expected.out, &answer),
        Compare::SearchCount => compare_search_count(&expected.out, &answer),
        Compare::Write => compare_write(&tree_bytes(original.path()), &tree_bytes(replaced.path())),
    };
    failures.extend(mismatch.err());
    failures
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

const SHOW_A: &str = "COMMAND\ncat src/a.ts\n===END===\nVERDICT block\nREASON lets show reads \
                      several files and ranges in one call.\nREPLACEMENT\nrun: lets show \
                      src/a.ts\n===END===\nCOMPARE read\n";

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
    let no_reason = SHOW_A.replacen(
        "REASON lets show reads several files and ranges in one call.\n",
        "",
        1,
    );
    let root = case_root_holding("no-reason", &no_reason);

    let failures = corpus_failures(root.path());

    assert_eq!(failures, vec![
        "no-reason: a block case names its REASON".to_owned()
    ]);
}

/// A whole-file `show` is the replacement the classifier gives for `sed -n '5p'` today.
#[test]
fn a_single_line_sed_replaced_by_a_whole_file_show_is_a_read_mismatch() {
    let command = "sed -n '5p' src/n.txt";
    let replaced = hook_tree();
    let failures = execute(Compare::Read, command, command, &replaced, &[
        "run: lets show src/n.txt",
    ]);
    assert!(
        failures
            .iter()
            .any(|failure| failure.starts_with("read mismatch")),
        "{failures:#?}"
    );

    let replaced = hook_tree();
    let failures = execute(Compare::Read, command, command, &replaced, &[
        "run: lets show src/n.txt:5",
    ]);
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
fn a_blocked_plain_grep_finds_exactly_what_grep_finds() {
    blocked_grep_agrees_with_grep("");
}

#[test]
fn a_blocked_extended_grep_finds_exactly_what_grep_finds() {
    blocked_grep_agrees_with_grep("-E ");
}

/// grep is the oracle: a pattern it rejects (exit 2) must be allowed, and one it accepts must
/// block with a search hitting the same lines, or be allowed.
fn blocked_grep_agrees_with_grep(dialect: &str) {
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
fn a_blocked_grep_whose_literal_matches_nothing_finds_nothing_too() {
    let tree = hook_tree();
    for (dialect, pattern) in [("", "||2"), ("-E ", r"2\?a")] {
        let command = format!("grep {dialect}'{pattern}' src/grep.txt");
        assert!(
            matches!(
                hook::classify(&event(tree.path(), &command)),
                Verdict::Block { .. }
            ),
            "{command} is blocked, so its replacement is what runs"
        );
        if let Err(failure) = agrees_with_grep(&tree, dialect, pattern) {
            panic!("{failure}");
        }
    }
}

fn agrees_with_grep(tree: &Sandbox, dialect: &str, pattern: &str) -> Result<(), TestCaseError> {
    let command = format!("grep {dialect}'{pattern}' src/grep.txt");
    let Verdict::Block { reason } = hook::classify(&event(tree.path(), &command)) else {
        return Ok(());
    };
    let runs: Vec<&str> = reason
        .lines()
        .filter_map(|line| line.strip_prefix("run: "))
        .collect();
    let [replacement] = runs.as_slice() else {
        return Err(TestCaseError::fail(format!(
            "{command}: expected one run line\n{reason}"
        )));
    };
    let grep = tree.bash(&format!("grep -Hn {dialect}'{pattern}' src/grep.txt"));
    let found = tree.bash(replacement);
    prop_assert!(
        matches!(grep.code, 0 | 1),
        "{command} blocked, but grep rejects the pattern (exit {}): {}",
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
    compare_search(&grep.out, &found.out)
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
    for quote in ['\'', '"'] {
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
            let verdict = hook::classify(&event(tree.path(), &statement));
            match verdict {
                Verdict::Allow if listed => {},
                Verdict::Allow => failures.push(format!(
                    "{id}: the classifier allows it, but it is not named in \
                     tests/hook/today-allow.txt:\n{statement}"
                )),
                Verdict::Block { reason } if listed => failures.push(format!(
                    "{id}: named in tests/hook/today-allow.txt as allow, but the classifier \
                     blocks it:\n{statement}\nreason: {reason}"
                )),
                Verdict::Block { reason } if reason.is_empty() => {
                    failures.push(format!("{id}: blocked with an empty reason:\n{statement}"));
                },
                Verdict::Block { .. } => {},
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
