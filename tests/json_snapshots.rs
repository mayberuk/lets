//! The sandbox normalises HOME, the runtime dir and the crate version, and `LETS_NO_STATS=1`
//! keeps `tokens_est` `null`, so no snapshot moves between machines.

mod support;

use support::sandbox::{Run, Sandbox, sandbox};

fn last_line(stream: &str) -> &str {
    stream.lines().last().unwrap_or_default()
}

#[test]
fn show_a_usage_symbol_target_as_json() {
    let sandbox = sandbox("read");

    let run = sandbox.lets(["show", "big.ts#usage", "--json"]);

    assert_eq!(run.code, 0);
    assert!(run.err.is_empty(), "{}", run.err);
    for key in [
        "target", "path", "start", "end", "total", "resolver", "lines",
    ] {
        assert!(
            run.out.contains(&format!("\"{key}\":")),
            "missing {key} in {}",
            run.out
        );
    }
    assert!(
        !run.out.contains("\"sha\""),
        "a read carries no sha: {}",
        run.out
    );
    insta::assert_snapshot!(run.out);
}

#[test]
fn show_a_line_range_as_jsonl_carries_no_sha() {
    let sandbox = sandbox("read");

    let run = sandbox.lets(["show", "big.ts:10-12", "--jsonl"]);

    assert_eq!(run.code, 0);
    assert!(run.err.is_empty(), "{}", run.err);
    assert_eq!(run.out.lines().count(), 2, "one target record, one tail");
    assert!(!run.out.contains("\"sha\""), "{}", run.out);
}

/// `store.go` defines `Open` at 44-46 and `Store.Open` at 213-215, and nothing else.
#[test]
fn show_an_outline_as_json() {
    let sandbox = sandbox("read");

    let run = sandbox.lets(["show", "store.go", "--outline", "--json"]);

    assert_eq!(run.code, 0);
    assert!(run.err.is_empty(), "{}", run.err);
    assert!(run.out.contains(
        "\"entries\":[{\"sig\":\"func Open(path string) (*Store, error) {\",\"line\":44,\"end_line\":46},\
         {\"sig\":\"func (s *Store) Open(ctx context.Context) error {\",\"line\":213,\"end_line\":215}]"
    ), "{}", run.out);
    assert!(!run.out.contains("\"sha\""), "{}", run.out);
    insta::assert_snapshot!(run.out);
}

#[test]
fn show_a_whole_file_target_under_the_window_as_json() {
    let sandbox = sandbox("read");

    let run = sandbox.lets(["show", "small.md", "--json"]);

    assert_eq!(run.code, 0);
    assert!(run.err.is_empty(), "{}", run.err);
    assert!(!run.out.contains("\"window\""));
    assert!(!run.out.contains("\"not_shown\""));
    insta::assert_snapshot!(run.out);
}

/// `big.ts` is 212 lines, over the default 100-line window.
#[test]
fn show_a_truncated_whole_file_target_as_json() {
    let sandbox = sandbox("read");

    let run = sandbox.lets(["show", "big.ts", "--json"]);

    assert_eq!(run.code, 0);
    assert!(run.err.is_empty(), "{}", run.err);
    assert!(run.out.contains("\"window\":100"));
    assert!(run.out.contains("\"not_shown\":[101,212]"));
    insta::assert_snapshot!(run.out);
}

#[test]
fn find_one_hit_as_json() {
    let sandbox = sandbox("read");

    let run = sandbox.lets(["find", "export function usage", "--json"]);

    assert_eq!(run.code, 0);
    assert!(run.err.is_empty(), "{}", run.err);
    insta::assert_snapshot!(run.out);
}

#[test]
fn find_one_hit_as_jsonl() {
    let sandbox = sandbox("read");

    let run = sandbox.lets(["find", "export function usage", "--jsonl"]);

    assert_eq!(run.code, 0);
    assert!(run.err.is_empty(), "{}", run.err);
    assert_eq!(
        run.out.lines().count(),
        2,
        "one hit record, one tail record"
    );
    insta::assert_snapshot!(run.out);
}

#[test]
fn find_over_the_hit_cap_as_json() {
    let sandbox = sandbox("read");

    let run = sandbox.lets(["find", "needle", "many-hits.txt", "--json"]);

    assert_eq!(run.code, 1);
    assert_eq!(last_line(&run.err), "ERROR_CODE=over_cap");
    assert!(
        run.out.contains(
            "{\"busiest_file\":{\"shown\":10,\"hits\":64}},{\"top_files\":{\"shown\":1}}"
        ),
        "{}",
        run.out
    );
    assert_eq!(
        run.out.matches("\"marker\":\"hit\"").count(),
        10,
        "the busiest file's first ten hits, and nothing else: {}",
        run.out
    );
    insta::assert_snapshot!(run.out);
}

#[test]
fn show_an_ambiguous_symbol_exits_2_with_every_candidate_on_stderr() {
    let sandbox = sandbox("read");

    let run = sandbox.lets(["show", "store.go#Open", "--json"]);

    assert_eq!(run.code, 2);
    assert!(run.out.contains("\"slug\":\"ambiguous\""), "{}", run.out);
    assert_eq!(last_line(&run.err), "ERROR_CODE=ambiguous");
    insta::assert_snapshot!(run.out);
}

#[test]
fn show_json_with_a_not_found_target_exits_1_and_still_prints_the_resolved_one() {
    let sandbox = sandbox("read");

    let run = sandbox.lets(["show", "small.md", "missing.ts", "--json"]);

    assert_eq!(run.code, 1);
    assert_eq!(last_line(&run.err), "ERROR_CODE=not_found");
    insta::assert_snapshot!(run.out);
}

#[test]
fn show_a_missing_target_alone_renders_the_error_object_on_stdout() {
    let sandbox = sandbox("read");

    let run = sandbox.lets(["show", "nope.txt", "--json"]);

    assert_eq!(run.code, 1);
    assert!(run.out.contains("\"slug\":\"not_found\""), "{}", run.out);
    assert!(run.out.contains("\"omitted\":[]"), "{}", run.out);
    assert_eq!(last_line(&run.err), "ERROR_CODE=not_found");
    insta::assert_snapshot!(run.out);
}

#[test]
fn show_a_missing_target_alone_under_jsonl_renders_the_error_object_on_one_line() {
    let sandbox = sandbox("read");

    let run = sandbox.lets(["show", "nope.txt", "--jsonl"]);

    assert_eq!(run.code, 1);
    assert_eq!(run.out.lines().count(), 1, "{}", run.out);
    let object: serde_json::Value = serde_json::from_str(&run.out).expect("one JSON object");
    assert_eq!(object["error"]["slug"], "not_found");
    assert_eq!(object["omitted"], serde_json::json!([]));
    assert_eq!(last_line(&run.err), "ERROR_CODE=not_found");
}

#[test]
fn a_json_call_rejected_at_argument_parsing_prints_the_usage_error_object() {
    let sandbox = sandbox("edit");

    let run = sandbox.lets([
        "--json", "edit", "usage.ts", "--old", "x", "--new", "y", "--if", "sha:abc",
    ]);

    assert_eq!(run.code, 64);
    assert_eq!(run.out.lines().count(), 1, "{}", run.out);
    let object: serde_json::Value = serde_json::from_str(&run.out).expect("one JSON object");
    assert_eq!(object["error"]["slug"], "usage");
    assert_eq!(object["omitted"], serde_json::json!([]));
    let stderr_text = run
        .err
        .trim_end()
        .trim_end_matches("ERROR_CODE=usage")
        .trim_end();
    assert_eq!(object["error"]["message"], stderr_text);
    assert_eq!(last_line(&run.err), "ERROR_CODE=usage");
}

#[test]
fn a_text_call_rejected_at_argument_parsing_leaves_stdout_empty() {
    let sandbox = sandbox("edit");

    let run = sandbox.lets([
        "edit", "usage.ts", "--old", "x", "--new", "y", "--if", "sha:abc",
    ]);

    assert_eq!(run.code, 64);
    assert!(run.out.is_empty(), "{}", run.out);
    assert_eq!(last_line(&run.err), "ERROR_CODE=usage");
}

#[test]
fn a_json_flag_after_the_operand_separator_is_an_operand_and_asks_for_no_object() {
    let sandbox = sandbox("edit");

    let run = sandbox.lets(["show", "--bogus", "--", "--json"]);

    assert_eq!(run.code, 64);
    assert!(run.out.is_empty(), "{}", run.out);
}

#[test]
fn edit_with_no_match_renders_the_error_object_on_stdout() {
    let sandbox = sandbox("edit");

    let run = sandbox.lets(["edit", "usage.ts", "--old", "zzz", "--new", "y", "--json"]);

    assert_eq!(run.code, 1);
    assert!(run.out.contains("\"slug\":\"not_found\""), "{}", run.out);
    assert!(run.out.contains("\"omitted\":[]"), "{}", run.out);
    assert_eq!(last_line(&run.err), "ERROR_CODE=not_found");
    insta::assert_snapshot!(run.out);
}

#[test]
fn json_and_jsonl_together_exit_64() {
    let sandbox = sandbox("read");

    let run = sandbox.lets(["show", "small.md", "--json", "--jsonl"]);

    assert_eq!(run.code, 64);
    assert!(run.out.contains("\"slug\":\"usage\""), "{}", run.out);
    assert_eq!(last_line(&run.err), "ERROR_CODE=usage");
    insta::assert_snapshot!(run.err);
}

#[test]
fn edit_one_replacement_as_json() {
    let sandbox = sandbox("edit");

    let run = sandbox.lets([
        "edit",
        "usage.ts",
        "--old",
        "const cap = 10",
        "--new",
        "const cap = 20",
        "--json",
    ]);

    assert_eq!(run.code, 0);
    assert!(run.err.is_empty(), "{}", run.err);
    for key in ["replacements", "match", "check", "sha", "omitted", "stats"] {
        assert!(
            run.out.contains(&format!("\"{key}\":")),
            "missing {key} in {}",
            run.out
        );
    }
    insta::assert_snapshot!(run.out);
}

#[test]
fn edit_reverted_by_a_layer_one_failure_as_json() {
    let sandbox = sandbox("edit");

    let run = sandbox.lets(["edit", "lib.rs", "--old", "fn", "--new", "f", "--json"]);

    assert_eq!(run.code, 3);
    assert_eq!(last_line(&run.err), "ERROR_CODE=check_failed");
    assert!(run.out.contains("\"reverted\":true"), "{}", run.out);
    assert!(
        !sandbox.read("lib.rs").contains("pub f usage"),
        "the reverted file keeps its original bytes"
    );
    insta::assert_snapshot!(run.out);
}

/// Through `bash` because `write` takes its content on stdin.
#[test]
fn write_a_created_file_as_json() {
    let sandbox = sandbox("edit");

    let run = sandbox.bash("printf 'echo hi\\n' | lets write fresh.sh --json");

    assert_eq!(run.code, 0);
    assert!(run.err.is_empty(), "{}", run.err);
    for key in ["outcome", "lines", "bytes", "sha", "omitted", "stats"] {
        assert!(
            run.out.contains(&format!("\"{key}\":")),
            "missing {key} in {}",
            run.out
        );
    }
    insta::assert_snapshot!(run.out);
}

#[test]
fn write_an_existing_path_without_force_as_json() {
    let sandbox = sandbox("edit");

    let run = sandbox.bash("printf 'echo hi\\n' | lets write run.sh --json");

    assert_eq!(run.code, 1);
    assert_eq!(last_line(&run.err), "ERROR_CODE=exists");
    assert!(run.out.contains("\"outcome\":\"exists\""), "{}", run.out);
    insta::assert_snapshot!(run.out);
}

/// `None` where the filesystem refuses the name: APFS refuses one that is not UTF-8.
#[cfg(unix)]
fn hit_file_named(sandbox: &Sandbox, name: &[u8]) -> Option<()> {
    use std::os::unix::ffi::OsStrExt as _;
    std::fs::write(
        sandbox.path().join(std::ffi::OsStr::from_bytes(name)),
        "zyzzyva\n",
    )
    .ok()
}

/// Every JSON form of a hit carries the name the text header shows: 0xff becomes U+FFFD, as
/// `Path::display` renders it.
#[cfg(unix)]
fn assert_every_json_form_names(sandbox: &Sandbox, shown: &str) {
    let text = sandbox.lets(["find", "zyzzyva"]);
    assert_eq!(text.code, 0, "{}", text.err);
    assert_eq!(
        text.out.lines().next(),
        Some(format!("\u{2500}\u{2500} {shown}").as_str())
    );

    let json = sandbox.lets(["find", "zyzzyva", "--json"]);
    assert_eq!(json.code, 0, "{}", json.err);
    let object: serde_json::Value = serde_json::from_str(&json.out).expect("one JSON object");
    assert_eq!(object["targets"][0]["path"], shown);
    assert_eq!(object["targets"][0]["target"], shown);

    let files = sandbox.lets(["find", "zyzzyva", "--files", "--json"]);
    assert_eq!(files.code, 0, "{}", files.err);
    let object: serde_json::Value = serde_json::from_str(&files.out).expect("one JSON object");
    assert_eq!(object["files"], serde_json::json!([shown]));

    let counts = sandbox.lets(["find", "zyzzyva", "--count", "--json"]);
    assert_eq!(counts.code, 0, "{}", counts.err);
    let object: serde_json::Value = serde_json::from_str(&counts.out).expect("one JSON object");
    assert_eq!(
        object["counts"],
        serde_json::json!([{"count": 1, "path": shown}])
    );

    for (args, key) in [
        (&["find", "zyzzyva", "--jsonl"][..], Some("path")),
        (&["find", "zyzzyva", "--files", "--jsonl"][..], None),
        (&["find", "zyzzyva", "--count", "--jsonl"][..], Some("path")),
    ] {
        let run = sandbox.lets(args);
        assert_eq!(run.code, 0, "{args:?}: {}", run.err);
        let first: serde_json::Value =
            serde_json::from_str(run.out.lines().next().expect("a first record"))
                .expect("a JSON record");
        let name = key.map_or(&first, |key| &first[key]);
        assert_eq!(name, shown, "{args:?}");
    }
}

#[cfg(unix)]
#[test]
fn a_hit_in_a_file_whose_name_is_not_utf8_reaches_json_as_the_text_header_names_it() {
    let sandbox = sandbox("read");
    if hit_file_named(&sandbox, b"bad\xffhit.txt").is_none() {
        eprintln!(
            "skipped (a_hit_in_a_file_whose_name_is_not_utf8_reaches_json_as_the_text_header_\
             names_it): the filesystem refused a non-UTF-8 name"
        );
        return;
    }

    assert_every_json_form_names(&sandbox, "bad\u{fffd}hit.txt");
}

#[cfg(unix)]
#[test]
fn a_hit_in_a_file_whose_name_is_utf8_reaches_json_as_is() {
    let sandbox = sandbox("read");
    hit_file_named(&sandbox, "caf\u{e9}-hit.txt".as_bytes()).expect("a UTF-8 name is accepted");

    assert_every_json_form_names(&sandbox, "caf\u{e9}-hit.txt");
}

/// `None` where the filesystem refuses the name, or mode 0o000 does not refuse this reader (root).
#[cfg(unix)]
fn run_with_an_unreadable_dir_named(sandbox: &Sandbox, name: &[u8]) -> Option<(Run, Run)> {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    let locked = sandbox.path().join(std::ffi::OsStr::from_bytes(name));
    std::fs::create_dir(&locked).ok()?;
    std::fs::write(locked.join("inside.txt"), "zyzzyva\n").expect("test fixture writes");
    std::fs::write(sandbox.path().join("kept.txt"), "zyzzyva\n").expect("test fixture writes");
    let mode = |bits| std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(bits));
    mode(0o000).expect("chmod the locked directory");
    let refused = std::fs::read_dir(&locked).is_err();
    let runs = (
        sandbox.lets(["find", "zyzzyva"]),
        sandbox.lets(["find", "zyzzyva", "--json"]),
    );
    // Left at 0o000, the sandbox's temp directory could not be removed.
    mode(0o755).expect("restore the locked directory");
    refused.then_some(runs)
}

#[cfg(unix)]
fn assert_the_unreadable_dir_is_named(text: &Run, json: &Run, shown: &str) {
    assert_eq!(text.code, 0, "{}", text.err);
    assert!(
        text.out
            .lines()
            .last()
            .is_some_and(|footer| footer.contains(&format!("{shown} unreadable"))),
        "{}",
        text.out
    );
    assert_eq!(json.code, 0, "{}", json.err);
    let object: serde_json::Value = serde_json::from_str(&json.out).expect("one JSON object");
    let omitted = object["omitted"].as_array().expect("an omitted array");
    assert!(
        omitted.contains(&serde_json::json!({"unreadable": {"path": shown}})),
        "{}",
        json.out
    );
    assert_eq!(object["targets"][0]["path"], "kept.txt");
}

#[cfg(unix)]
#[test]
fn an_unreadable_directory_whose_name_is_not_utf8_reaches_json_as_the_footer_names_it() {
    let sandbox = sandbox("read");
    let Some((text, json)) = run_with_an_unreadable_dir_named(&sandbox, b"locked\xffdir") else {
        eprintln!(
            "skipped (an_unreadable_directory_whose_name_is_not_utf8_reaches_json_as_the_footer_\
             names_it): a non-UTF-8 name or mode 0o000 was not honoured here"
        );
        return;
    };

    assert_the_unreadable_dir_is_named(&text, &json, "locked\u{fffd}dir");
}

#[cfg(unix)]
#[test]
fn an_unreadable_directory_whose_name_is_utf8_reaches_json_as_is() {
    let sandbox = sandbox("read");
    let Some((text, json)) =
        run_with_an_unreadable_dir_named(&sandbox, "locked-caf\u{e9}".as_bytes())
    else {
        eprintln!(
            "skipped (an_unreadable_directory_whose_name_is_utf8_reaches_json_as_is): mode 0o000 \
             did not refuse this reader"
        );
        return;
    };

    assert_the_unreadable_dir_is_named(&text, &json, "locked-caf\u{e9}");
}
