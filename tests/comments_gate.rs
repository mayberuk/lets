mod support;

use std::fs;
use std::path::PathBuf;

use support::sandbox::scrubbed;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run_gate(paths: &[&str]) -> (i32, String) {
    run_gate_in(&repo_root(), paths)
}

fn run_gate_in(dir: &std::path::Path, paths: &[&str]) -> (i32, String) {
    let output = scrubbed("sh", dir)
        .arg(repo_root().join("scripts/comments-gate.sh"))
        .args(paths)
        .output()
        .expect("scripts/comments-gate.sh runs under sh");
    let code = output.status.code().expect("the gate exits, never signals");
    let stderr = String::from_utf8(output.stderr).expect("the gate prints utf-8");
    (code, stderr)
}

const EXPECTED: &[(&str, &str)] = &[
    ("todo.rs", "TODO-style marker"),
    ("banner.rs", "banner"),
    ("plan_reference.rs", "plan reference"),
    (
        "history_wording.rs",
        "history of the change, not a fact about the code",
    ),
    ("commented_out_code.rs", "commented-out code"),
    ("block_too_long.rs", "block of 7 comment lines"),
    ("bang_block_too_long.rs", "block of 7 comment lines"),
    ("standalone_id.rs", "standalone id"),
    ("doc_pointer_line.rs", "doc pointer with a line number"),
    ("non_shipping_doc.rs", "pointer to a non-shipping doc"),
    ("dated_history_wording.rs", "dated/history wording"),
    ("duplicated_comment.rs", "duplicated comment"),
    ("trailing_comment.rs", "trailing comment"),
    ("hash_todo.sh", "TODO-style marker"),
    ("hash_banner.toml", "banner"),
    ("hash_trailing_comment.yml", "trailing comment"),
    ("hash_commented_out_code.py", "commented-out code"),
    ("justfile", "TODO-style marker"),
    ("used_to.rs", "dated/history wording"),
    ("no_longer.rs", "dated/history wording"),
    ("bug_b1.rs", "dated/history wording"),
    ("the_plan.rs", "dated/history wording"),
    ("multi_violation.rs", "TODO-style marker"),
    ("multi_violation.rs", "plan reference"),
    ("attribute_trailing_comment.rs", "trailing comment"),
];

#[test]
fn every_violating_fixture_is_reported_and_the_gate_exits_1() {
    let (code, stderr) = run_gate(&["tests/comments_gate/violating"]);

    assert_eq!(code, 1, "the violating fixtures passed the gate:\n{stderr}");
    for (file, rule) in EXPECTED {
        let line_reports_it = stderr
            .lines()
            .any(|line| line.contains(file) && line.contains(rule));
        assert!(
            line_reports_it,
            "expected a `{rule}` hit naming {file}, got:\n{stderr}"
        );
    }
}

#[test]
fn legitimate_comments_exit_0() {
    let (code, stderr) = run_gate(&["tests/comments_gate/clean"]);

    assert_eq!(code, 0, "a legitimate comment tripped the gate:\n{stderr}");
    assert!(stderr.is_empty(), "{stderr}");
}

#[test]
fn a_path_argument_checks_only_that_path() {
    let (code, stderr) = run_gate(&["tests/comments_gate/violating/todo.rs"]);

    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("todo.rs"), "{stderr}");
    assert!(
        !stderr.contains("banner.rs"),
        "a single-file argument scanned a sibling file:\n{stderr}"
    );
}

#[test]
fn a_path_argument_is_not_filtered_by_the_default_fixture_exclusion() {
    let (code, stderr) = run_gate(&["tests/comments_gate/violating"]);

    assert_eq!(
        code, 1,
        "the gate excludes tests/comments_gate/ by default so its own fixtures stay silent \
         on a whole-repo run, but an explicit path argument must still be checked:\n{stderr}"
    );
}

#[test]
fn heading_and_percentile_labels_are_not_standalone_ids() {
    let (code, stderr) = run_gate(&["tests/comments_gate/clean/percentile_and_heading.rs"]);

    assert_eq!(
        code, 0,
        "H2 and P99 tripped the standalone-id rule:\n{stderr}"
    );
    assert!(stderr.is_empty(), "{stderr}");
}

#[test]
fn an_md_mention_with_no_line_number_is_not_a_doc_pointer() {
    let (code, stderr) = run_gate(&["tests/comments_gate/clean/md_without_line_number.rs"]);

    assert_eq!(
        code, 0,
        "a bare .md mention tripped the doc-pointer rule:\n{stderr}"
    );
    assert!(stderr.is_empty(), "{stderr}");
}

#[test]
fn a_six_line_comment_block_is_under_the_cap_for_both_comment_styles() {
    let (code, stderr) = run_gate(&[
        "tests/comments_gate/clean/block_of_6.rs",
        "tests/comments_gate/clean/bang_block_of_6.rs",
    ]);

    assert_eq!(
        code, 0,
        "a 6-line block tripped the block-length rule:\n{stderr}"
    );
    assert!(stderr.is_empty(), "{stderr}");
}

#[test]
fn a_duplicated_comment_under_the_length_floor_is_not_reported() {
    let (code, stderr) = run_gate(&["tests/comments_gate/clean/duplicate_under_40.rs"]);

    assert_eq!(
        code, 0,
        "a duplicated comment under the 40-char floor tripped the duplicate rule:\n{stderr}"
    );
    assert!(stderr.is_empty(), "{stderr}");
}

#[test]
fn the_duplicate_report_names_the_first_occurrence_line() {
    let (code, stderr) = run_gate(&["tests/comments_gate/violating/duplicated_comment.rs"]);

    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains(
            "tests/comments_gate/violating/duplicated_comment.rs:4: duplicated comment, \
             also at tests/comments_gate/violating/duplicated_comment.rs:1"
        ),
        "{stderr}"
    );
}

#[test]
fn a_url_scheme_inside_a_string_is_not_a_trailing_comment() {
    let (code, stderr) = run_gate(&["tests/comments_gate/clean/url_in_string.rs"]);

    assert_eq!(
        code, 0,
        "a `://` in a string tripped the trailing-comment rule:\n{stderr}"
    );
    assert!(stderr.is_empty(), "{stderr}");
}

#[test]
fn rust_attributes_are_never_flagged() {
    let (code, stderr) = run_gate(&["tests/comments_gate/clean/rust_attributes.rs"]);

    assert_eq!(code, 0, "a `#[attr]` line tripped the gate:\n{stderr}");
    assert!(stderr.is_empty(), "{stderr}");
}

#[test]
fn a_slash_slash_inside_a_string_is_not_a_trailing_comment() {
    let (code, stderr) = run_gate(&["tests/comments_gate/clean/string_with_slashes.rs"]);

    assert_eq!(
        code, 0,
        "a `//` inside a quoted string tripped the trailing-comment rule:\n{stderr}"
    );
    assert!(stderr.is_empty(), "{stderr}");
}

#[test]
fn a_slash_slash_on_a_multiline_string_continuation_is_not_a_trailing_comment() {
    let (code, stderr) = run_gate(&["tests/comments_gate/clean/multiline_string_with_slashes.rs"]);

    assert_eq!(
        code, 0,
        "a `//` on a backslash-continued string line tripped the trailing-comment rule:\n{stderr}"
    );
    assert!(stderr.is_empty(), "{stderr}");
}

#[test]
fn a_heredoc_body_is_not_checked_for_comments() {
    let (code, stderr) = run_gate(&["tests/comments_gate/clean/heredoc_body_with_hash.sh"]);

    assert_eq!(
        code, 0,
        "a `#` inside a heredoc body tripped the trailing-comment rule:\n{stderr}"
    );
    assert!(stderr.is_empty(), "{stderr}");
}

#[test]
fn violation_reports_use_the_exact_path_and_line() {
    let (_, stderr) = run_gate(&["tests/comments_gate/violating"]);

    assert!(
        stderr.contains("tests/comments_gate/violating/todo.rs:1: TODO-style marker"),
        "{stderr}"
    );
    assert!(
        stderr.contains("tests/comments_gate/violating/multi_violation.rs:1: TODO-style marker"),
        "{stderr}"
    );
    assert!(
        stderr.contains("tests/comments_gate/violating/multi_violation.rs:4: plan reference"),
        "{stderr}"
    );
}

#[test]
fn multiple_path_arguments_report_only_the_violating_one() {
    let (code, stderr) = run_gate(&[
        "tests/comments_gate/clean/lib.rs",
        "tests/comments_gate/violating/todo.rs",
    ]);

    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("todo.rs"), "{stderr}");
    assert!(
        !stderr.contains("clean/lib.rs"),
        "a clean path argument was flagged:\n{stderr}"
    );
}

#[test]
fn no_argument_mode_checks_tracked_files_and_still_excludes_fixture_paths() {
    let dir = tempfile::tempdir().expect("a temp dir for an isolated git repo");
    let root = dir.path();

    let git = |args: &[&str]| {
        let status = scrubbed("git", root).args(args).status().expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "comments-gate-test@example.invalid"]);
    git(&["config", "user.name", "comments-gate-test"]);
    git(&["config", "commit.gpgsign", "false"]);

    fs::write(root.join("clean.sh"), "echo ok\n").expect("write clean.sh");
    fs::write(
        root.join("violating.sh"),
        "# TODO revisit this once the retry limit is finalized\necho bad\n",
    )
    .expect("write violating.sh");
    fs::create_dir_all(root.join("tests/comments_gate/violating"))
        .expect("create the excluded fixture path");
    fs::write(
        root.join("tests/comments_gate/violating/excluded.sh"),
        "# TODO this lives under the excluded fixture path\necho excluded\n",
    )
    .expect("write the excluded fixture");

    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "seed"]);

    let (code, stderr) = run_gate_in(root, &[]);

    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("violating.sh:1: TODO-style marker"),
        "the no-argument mode did not check a tracked file:\n{stderr}"
    );
    assert!(
        !stderr.contains("excluded.sh"),
        "the no-argument mode did not exclude a fixture path:\n{stderr}"
    );
}
