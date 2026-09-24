//! The sandbox normalises HOME, the runtime dir and the crate version, and `LETS_NO_STATS=1`
//! keeps `tokens_est` `null`, so no snapshot moves between machines.

mod support;

use support::sandbox::sandbox;

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
        "target", "path", "start", "end", "total", "resolver", "sha", "lines",
    ] {
        assert!(
            run.out.contains(&format!("\"{key}\":")),
            "missing {key} in {}",
            run.out
        );
    }
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

/// `big.ts` is 212 lines, over the default 200-line window.
#[test]
fn show_a_truncated_whole_file_target_as_json() {
    let sandbox = sandbox("read");

    let run = sandbox.lets(["show", "big.ts", "--json"]);

    assert_eq!(run.code, 0);
    assert!(run.err.is_empty(), "{}", run.err);
    assert!(run.out.contains("\"window\":200"));
    assert!(run.out.contains("\"not_shown\":[201,212]"));
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
    assert!(run.out.contains("\"targets\":[]"), "{}", run.out);
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
