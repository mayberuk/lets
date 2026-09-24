//! Only counts and verb names can reach `StatsReport`, so no transcript text, path or content can
//! leak: the guarantee is the struct's shape, not a filter.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use ignore::WalkBuilder;
use serde_json::Value;

use crate::Outcome;
use crate::cli::StatsArgs;
use crate::output::{Body, Format, Response, StatsReport, StatsSkips};

pub fn run(args: &StatsArgs, _format: Format) -> Outcome {
    let dir = args.dir.clone().unwrap_or_else(default_dir);
    let report = scan_dir(&dir, args.since);
    let mut response = Response::empty("stats");
    response.body = Body::Stats(report);
    Outcome::ok(response)
}

fn default_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join(".claude")
        .join("projects")
}

struct ToolUse {
    name: String,
}

fn scan_dir(dir: &Path, since: Option<Duration>) -> StatsReport {
    let mut report = StatsReport {
        sessions: 0,
        bash_calls: 0,
        lets_calls: BTreeMap::new(),
        hook_blocks: 0,
        blocks_followed: 0,
        calls_saved: 0,
        read_calls: 0,
        read_bytes: 0,
        skipped: StatsSkips::default(),
    };
    if !dir.is_dir() {
        return report;
    }
    let cutoff = since.map(|window| SystemTime::now() - window);
    let mut files: Vec<PathBuf> = Vec::new();
    let walk = WalkBuilder::new(dir)
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .build();
    for entry in walk {
        let Ok(entry) = entry else {
            report.skipped.walk_errors += 1;
            continue;
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file())
            || entry.path().extension() != Some(OsStr::new("jsonl"))
        {
            continue;
        }
        match cutoff.map(|cutoff| modified_since(entry.path(), cutoff)) {
            None | Some(Ok(true)) => files.push(entry.into_path()),
            Some(Ok(false)) => {},
            Some(Err(_)) => report.skipped.unreadable_files += 1,
        }
    }
    // Readdir order is not stable across runs; sorting keeps the scan deterministic.
    files.sort();
    for file in files {
        scan_file(&file, &mut report);
    }
    report
}

fn modified_since(path: &Path, cutoff: SystemTime) -> std::io::Result<bool> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map(|modified| modified >= cutoff)
}

/// Bad lines are skipped, not fatal: a transcript is foreign input. Only malformed and non-UTF-8
/// lines count into `report.skipped`.
fn scan_file(path: &Path, report: &mut StatsReport) {
    let Ok(file) = File::open(path) else {
        report.skipped.unreadable_files += 1;
        return;
    };
    report.sessions += 1;
    let mut pending: HashMap<String, ToolUse> = HashMap::new();
    let mut pending_block_verb: Option<String> = None;
    let mut reader = BufReader::new(file);
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        match reader.read_until(b'\n', &mut bytes) {
            Ok(0) => break,
            Ok(_) => {},
            // Retrying a read error would repeat it, so the rest of the file is given up.
            Err(_) => {
                report.skipped.unreadable_files += 1;
                break;
            },
        }
        let Ok(line) = std::str::from_utf8(&bytes) else {
            report.skipped.non_utf8_lines += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            report.skipped.malformed_lines += 1;
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                handle_assistant(&value, &mut pending, &mut pending_block_verb, report);
            },
            Some("user") => handle_user(&value, &mut pending, &mut pending_block_verb, report),
            _ => {},
        }
    }
}

fn content_items(value: &Value) -> impl Iterator<Item = &Value> {
    value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn handle_assistant(
    value: &Value,
    pending: &mut HashMap<String, ToolUse>,
    pending_block_verb: &mut Option<String>,
    report: &mut StatsReport,
) {
    for item in content_items(value) {
        if item.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(name) = item.get("name").and_then(Value::as_str) else {
            continue;
        };
        let command = (name == "Bash")
            .then(|| item.get("input").and_then(|input| input.get("command")))
            .flatten()
            .and_then(Value::as_str)
            .map(str::to_owned);

        // Only the first call after a block decides "followed": a later retry is the agent's own.
        if let Some(expected) = pending_block_verb.take()
            && name == "Bash"
            && let Some(command) = &command
            && let Some((verb, _)) = lets_call(command)
            && verb == expected
        {
            report.blocks_followed += 1;
        }

        if name == "Bash" {
            report.bash_calls += 1;
            if let Some(command) = &command
                && let Some((verb, args)) = lets_call(command)
            {
                *report.lets_calls.entry(verb.to_owned()).or_insert(0) += 1;
                let targets = bare_target_count(verb, &args);
                if targets >= 2 {
                    report.calls_saved += targets - 1;
                }
            }
        }

        pending.insert(id.to_owned(), ToolUse {
            name: name.to_owned(),
        });
    }
}

fn handle_user(
    value: &Value,
    pending: &mut HashMap<String, ToolUse>,
    pending_block_verb: &mut Option<String>,
    report: &mut StatsReport,
) {
    for item in content_items(value) {
        if item.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let Some(id) = item.get("tool_use_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(call) = pending.remove(id) else {
            continue;
        };
        let is_error = item
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = text_of(item.get("content").unwrap_or(&Value::Null));

        match call.name.as_str() {
            "Bash" if is_error => {
                if let Some(run_line) = text.lines().find(|line| line.starts_with("run: ")) {
                    report.hook_blocks += 1;
                    *pending_block_verb =
                        lets_call(&run_line["run: ".len()..]).map(|(verb, _)| verb.to_owned());
                }
            },
            "Read" => {
                report.read_calls += 1;
                report.read_bytes += text.len() as u64;
            },
            _ => {},
        }
    }
}

/// A hook block's content is a plain string; a normal tool result's is a list of text blocks.
fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|block| {
                (block.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| block.get("text").and_then(Value::as_str))
                    .flatten()
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn lets_call(command: &str) -> Option<(&str, Vec<&str>)> {
    let command = strip_leading_cd(command);
    let mut words = command.split_whitespace();
    let program = words.next()?;
    if Path::new(program).file_name() != Some("lets".as_ref()) {
        return None;
    }
    let verb = words.next()?;
    if verb.starts_with('-') {
        return None;
    }
    Some((verb, words.collect()))
}

fn strip_leading_cd(command: &str) -> &str {
    let trimmed = command.trim_start();
    let Some(rest) = trimmed.strip_prefix("cd ") else {
        return trimmed;
    };
    let split = ["&&", ";", "\n"]
        .iter()
        .filter_map(|sep| rest.find(sep).map(|at| (at, sep.len())))
        .min_by_key(|(at, _)| *at);
    match split {
        Some((at, len)) => rest[at + len..].trim_start(),
        None => trimmed,
    }
}

/// `find`'s first bare word is its pattern, so only the paths after it are targets.
fn bare_target_count(verb: &str, args: &[&str]) -> usize {
    let bare = args.iter().take_while(|arg| !arg.starts_with('-')).count();
    if verb == "find" {
        bare.saturating_sub(1)
    } else {
        bare
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write as _;
    use std::time::{Duration, SystemTime};

    use tempfile::TempDir;

    use super::scan_dir;

    fn session(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        let path = dir.join(name);
        let mut file = File::create(&path).expect("a fresh session file");
        for line in lines {
            writeln!(file, "{line}").expect("a transcript line");
        }
        path
    }

    fn tool_use(id: &str, name: &str, command: Option<&str>) -> String {
        let input = command.map_or_else(String::new, |command| {
            format!(r#","input":{{"command":{command:?}}}"#)
        });
        format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"{id}","name":"{name}"{input}}}]}}}}"#
        )
    }

    fn tool_result(id: &str, content: &str, is_error: bool) -> String {
        format!(
            r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{id}","content":{content:?},"is_error":{is_error}}}]}}}}"#
        )
    }

    use std::path::{Path, PathBuf};

    #[test]
    fn a_dir_that_does_not_exist_is_an_all_zero_report_not_a_failure() {
        let missing = Path::new("/no/such/directory/for/lets-stats-tests");
        let report = scan_dir(missing, None);
        assert_eq!(report.sessions, 0);
        assert_eq!(report.bash_calls, 0);
    }

    #[test]
    fn bash_calls_and_lets_calls_by_verb_are_counted_from_tool_use_lines() {
        let dir = TempDir::new().expect("a scratch dir");
        session(dir.path(), "s.jsonl", &[
            &tool_use("t1", "Bash", Some("cat a.ts")),
            &tool_use("t2", "Bash", Some("lets show a.ts")),
            &tool_use("t3", "Bash", Some("lets find x .")),
            &tool_use("t4", "Read", None),
        ]);

        let report = scan_dir(dir.path(), None);

        assert_eq!(report.sessions, 1);
        assert_eq!(report.bash_calls, 3);
        assert_eq!(report.lets_calls.get("show"), Some(&1));
        assert_eq!(report.lets_calls.get("find"), Some(&1));
    }

    #[test]
    fn a_non_lets_bash_call_is_not_counted_as_any_verb() {
        let dir = TempDir::new().expect("a scratch dir");
        session(dir.path(), "s.jsonl", &[&tool_use(
            "t1",
            "Bash",
            Some("grep -rn TODO ."),
        )]);

        let report = scan_dir(dir.path(), None);

        assert_eq!(report.bash_calls, 1);
        assert!(report.lets_calls.is_empty());
    }

    #[test]
    fn a_leading_cd_is_stripped_before_the_verb_is_read() {
        let dir = TempDir::new().expect("a scratch dir");
        session(dir.path(), "s.jsonl", &[&tool_use(
            "t1",
            "Bash",
            Some("cd src && lets show a.ts"),
        )]);

        let report = scan_dir(dir.path(), None);

        assert_eq!(report.lets_calls.get("show"), Some(&1));
    }

    #[test]
    fn a_multi_target_lets_call_saves_targets_minus_one() {
        let dir = TempDir::new().expect("a scratch dir");
        session(dir.path(), "s.jsonl", &[&tool_use(
            "t1",
            "Bash",
            Some("lets show a.ts b.ts c.ts"),
        )]);

        let report = scan_dir(dir.path(), None);

        assert_eq!(report.calls_saved, 2);
    }

    #[test]
    fn a_single_target_lets_call_saves_nothing() {
        let dir = TempDir::new().expect("a scratch dir");
        session(dir.path(), "s.jsonl", &[&tool_use(
            "t1",
            "Bash",
            Some("lets show a.ts"),
        )]);

        let report = scan_dir(dir.path(), None);

        assert_eq!(report.calls_saved, 0);
    }

    #[test]
    fn a_find_with_one_path_after_its_pattern_saves_nothing() {
        let dir = TempDir::new().expect("a scratch dir");
        session(dir.path(), "s.jsonl", &[&tool_use(
            "t1",
            "Bash",
            Some("lets find cap src"),
        )]);

        let report = scan_dir(dir.path(), None);

        assert_eq!(report.calls_saved, 0);
    }

    #[test]
    fn a_find_with_two_paths_after_its_pattern_saves_one() {
        let dir = TempDir::new().expect("a scratch dir");
        session(dir.path(), "s.jsonl", &[&tool_use(
            "t1",
            "Bash",
            Some("lets find cap src lib"),
        )]);

        let report = scan_dir(dir.path(), None);

        assert_eq!(report.calls_saved, 1);
    }

    #[test]
    fn a_flag_before_any_target_saves_nothing() {
        let dir = TempDir::new().expect("a scratch dir");
        session(dir.path(), "s.jsonl", &[&tool_use(
            "t1",
            "Bash",
            Some("lets find --count x ."),
        )]);

        let report = scan_dir(dir.path(), None);

        assert_eq!(report.calls_saved, 0);
    }

    /// Shape copied from a real blocked call: string content, with `run: ` not on the first line.
    #[test]
    fn a_hook_block_is_counted_from_the_real_string_content_shape() {
        let dir = TempDir::new().expect("a scratch dir");
        session(dir.path(), "s.jsonl", &[
            &tool_use(
                "t1",
                "Bash",
                Some("sed -i 's/const cap = 27/const cap = 40/' src/usage.ts"),
            ),
            &tool_result(
                "t1",
                "PreToolUse:Bash hook error: lets replaces this command in one call.\nrun: lets edit src/usage.ts --old 'const cap = 27' --new 'const cap = 40'",
                true,
            ),
        ]);

        let report = scan_dir(dir.path(), None);

        assert_eq!(report.hook_blocks, 1);
    }

    #[test]
    fn a_run_line_with_is_error_false_is_not_a_block() {
        let dir = TempDir::new().expect("a scratch dir");
        session(dir.path(), "s.jsonl", &[
            &tool_use("t1", "Bash", Some("echo hi")),
            &tool_result("t1", "run: lets show a.ts", false),
        ]);

        let report = scan_dir(dir.path(), None);

        assert_eq!(report.hook_blocks, 0);
    }

    #[test]
    fn a_block_followed_by_the_named_verb_is_counted_as_followed() {
        let dir = TempDir::new().expect("a scratch dir");
        session(dir.path(), "s.jsonl", &[
            &tool_use("t1", "Bash", Some("cat a.ts b.ts")),
            &tool_result(
                "t1",
                "PreToolUse:Bash hook error: lets replaces this command in one call.\nrun: cd src && lets show a.ts b.ts",
                true,
            ),
            &tool_use("t2", "Bash", Some("cd src && lets show a.ts b.ts")),
        ]);

        let report = scan_dir(dir.path(), None);

        assert_eq!(report.hook_blocks, 1);
        assert_eq!(report.blocks_followed, 1);
    }

    #[test]
    fn a_block_followed_by_a_different_verb_is_not_counted_as_followed() {
        let dir = TempDir::new().expect("a scratch dir");
        session(dir.path(), "s.jsonl", &[
            &tool_use("t1", "Bash", Some("cat a.ts")),
            &tool_result(
                "t1",
                "PreToolUse:Bash hook error: use lets.\nrun: lets show a.ts",
                true,
            ),
            &tool_use("t2", "Bash", Some("lets find x .")),
        ]);

        let report = scan_dir(dir.path(), None);

        assert_eq!(report.hook_blocks, 1);
        assert_eq!(report.blocks_followed, 0);
    }

    #[test]
    fn read_calls_and_bytes_are_counted_from_paired_tool_results() {
        let dir = TempDir::new().expect("a scratch dir");
        session(dir.path(), "s.jsonl", &[
            &tool_use("t1", "Read", None),
            &tool_result("t1", "twelve bytes!", false),
        ]);

        let report = scan_dir(dir.path(), None);

        assert_eq!(report.read_calls, 1);
        assert_eq!(report.read_bytes, "twelve bytes!".len() as u64);
    }

    #[test]
    fn a_read_result_as_a_list_of_text_blocks_is_measured_too() {
        let dir = TempDir::new().expect("a scratch dir");
        let path = dir.path().join("s.jsonl");
        let mut file = File::create(&path).expect("a fresh session file");
        writeln!(file, "{}", tool_use("t1", "Read", None)).unwrap();
        writeln!(
            file,
            r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":[{{"type":"text","text":"abcde"}}],"is_error":false}}]}}}}"#
        )
        .unwrap();

        let report = scan_dir(dir.path(), None);

        assert_eq!(report.read_calls, 1);
        assert_eq!(report.read_bytes, 5);
    }

    #[test]
    fn a_malformed_json_line_is_skipped_and_the_rest_of_the_file_still_counts() {
        let dir = TempDir::new().expect("a scratch dir");
        session(dir.path(), "s.jsonl", &[
            "not json at all {{{",
            &tool_use("t1", "Bash", Some("lets show a.ts")),
        ]);

        let report = scan_dir(dir.path(), None);

        assert_eq!(report.sessions, 1);
        assert_eq!(report.lets_calls.get("show"), Some(&1));
        assert_eq!(report.skipped.malformed_lines, 1);
    }

    #[test]
    fn a_non_utf8_line_is_counted_as_skipped_and_its_call_is_not_counted() {
        let dir = TempDir::new().expect("a scratch dir");
        let mut bytes = tool_use("t1", "Bash", Some("cat caf@.ts")).into_bytes();
        let at = bytes
            .iter()
            .position(|b| *b == b'@')
            .expect("the placeholder byte");
        bytes[at] = 0xe9;
        bytes.push(b'\n');
        bytes.extend_from_slice(tool_use("t2", "Bash", Some("lets show a.ts")).as_bytes());
        bytes.push(b'\n');
        std::fs::write(dir.path().join("s.jsonl"), bytes).expect("a session with a Latin-1 byte");

        let report = scan_dir(dir.path(), None);

        assert_eq!(report.skipped.non_utf8_lines, 1);
        assert_eq!(report.skipped.malformed_lines, 0);
        assert_eq!(report.bash_calls, 1);
        assert_eq!(report.lets_calls.get("show"), Some(&1));
    }

    #[test]
    fn a_clean_transcript_skips_nothing() {
        let dir = TempDir::new().expect("a scratch dir");
        session(dir.path(), "s.jsonl", &[
            &tool_use("t1", "Bash", Some("lets show a.ts")),
            "",
            &tool_use("t2", "Read", None),
            &tool_result("t2", "abc", false),
        ]);

        let report = scan_dir(dir.path(), None);

        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    }

    #[test]
    fn an_empty_file_is_one_session_with_no_calls() {
        let dir = TempDir::new().expect("a scratch dir");
        session(dir.path(), "s.jsonl", &[]);

        let report = scan_dir(dir.path(), None);

        assert_eq!(report.sessions, 1);
        assert_eq!(report.bash_calls, 0);
    }

    #[test]
    fn a_non_jsonl_file_in_the_dir_is_not_scanned_as_a_session() {
        let dir = TempDir::new().expect("a scratch dir");
        session(dir.path(), "notes.md", &["not a transcript"]);

        let report = scan_dir(dir.path(), None);

        assert_eq!(report.sessions, 0);
    }

    #[test]
    fn since_excludes_a_file_older_than_the_window() {
        let dir = TempDir::new().expect("a scratch dir");
        let old = session(dir.path(), "old.jsonl", &[&tool_use(
            "t1",
            "Bash",
            Some("lets show a.ts"),
        )]);
        let file = File::open(&old).expect("reopen to set its mtime");
        let stale = SystemTime::now() - Duration::from_hours(30 * 24);
        file.set_modified(stale).expect("a settable mtime");
        session(dir.path(), "new.jsonl", &[&tool_use(
            "t2",
            "Bash",
            Some("lets find x ."),
        )]);

        let report = scan_dir(dir.path(), Some(Duration::from_hours(7 * 24)));

        assert_eq!(report.sessions, 1);
        assert!(report.lets_calls.contains_key("find"));
        assert!(!report.lets_calls.contains_key("show"));
    }

    #[test]
    fn no_since_counts_a_file_of_any_age() {
        let dir = TempDir::new().expect("a scratch dir");
        let old = session(dir.path(), "old.jsonl", &[&tool_use(
            "t1",
            "Bash",
            Some("lets show a.ts"),
        )]);
        let file = File::open(&old).expect("reopen to set its mtime");
        let stale = SystemTime::now() - Duration::from_hours(30 * 24);
        file.set_modified(stale).expect("a settable mtime");

        let report = scan_dir(dir.path(), None);

        assert_eq!(report.sessions, 1);
        assert!(report.lets_calls.contains_key("show"));
    }

    #[test]
    fn the_serialized_report_never_contains_a_transcript_marker() {
        let dir = TempDir::new().expect("a scratch dir");
        session(dir.path(), "s.jsonl", &[
            &tool_use(
                "t1",
                "Bash",
                Some("cat /home/definitely-a-secret-marker/file.ts"),
            ),
            &tool_use("t2", "Read", None),
            &tool_result("t2", "MARKER-CONTENT-BYTES", false),
        ]);

        let report = scan_dir(dir.path(), None);
        let json = serde_json::to_string(&report).expect("a report serialises");

        assert!(!json.contains("secret-marker"));
        assert!(!json.contains("MARKER-CONTENT-BYTES"));
    }
}
