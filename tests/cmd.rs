//! A case with neither an `<id>.in/` tree nor an `[fs] base` runs unsandboxed against the repo
//! root, so it must not write.

mod support;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

/// A throwaway git working tree: in place, the fixtures sit under this repo's `.gitignore`,
/// which re-includes all of `tests/`, so a `find` case depended on the checkout around it.
const CASE_TREE: &str = "target/cmd-fixtures/read";

/// nextest captures a passing test's stderr, so skips also go to a file `just test` prints.
const SKIPPED_LOG: &str = "target/cmd-skipped.txt";

fn verb_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir("tests/cmd")
        .expect("tests/cmd exists")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

fn unmet_requirements(dir: &Path) -> Vec<String> {
    let verb = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("?");
    let path = dir.join("required.txt");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return vec![format!(
            "{verb}: no required.txt naming the cases that may not vanish"
        )];
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter(|id| !dir.join(id).exists())
        .map(|id| format!("{verb}/{id}: named in required.txt but absent"))
        .collect()
}

/// `path` is the `PATH` the cases run with, not the ambient one, so a bundled checker under
/// `tests/checkers/` counts as present.
fn unavailable_requirements(dirs: &[PathBuf], path: &str) -> Vec<(String, String)> {
    let mut missing = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut declarations: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "requires"))
            .collect();
        declarations.sort();
        for declaration in declarations {
            let declared = std::fs::read_to_string(&declaration).unwrap_or_default();
            let absent = declared
                .lines()
                .map(str::trim)
                .filter(|tool| !tool.is_empty())
                .find(|tool| !on_path(tool, path));
            let Some(tool) = absent else { continue };
            // The case file itself: a bare `<id>.*` glob also matches the `<id>.in`/`<id>.out`
            // directories, and trycmd lists whatever it is handed.
            let case = ["trycmd", "toml"]
                .into_iter()
                .map(|extension| declaration.with_extension(extension))
                .find(|candidate| candidate.is_file());
            if let Some(case) = case {
                missing.push((case.display().to_string(), tool.to_owned()));
            }
        }
    }
    missing
}

fn on_path(name: &str, path: &str) -> bool {
    std::env::split_paths(path).any(|directory| directory.join(name).is_file())
}

/// `register_bin` resolves only the name a case runs; `--check <name>` goes through `sh`, so it
/// must be on `PATH`.
fn path_with_checkers() -> String {
    let checkers = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/checkers");
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let joined =
        std::env::join_paths(std::iter::once(checkers).chain(std::env::split_paths(&inherited)))
            .expect("no PATH entry contains the separator");
    joined.to_string_lossy().into_owned()
}

#[test]
fn required_cases_are_present() {
    let mut failures = Vec::new();
    for dir in verb_dirs() {
        failures.extend(unmet_requirements(&dir));
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn a_case_named_in_required_txt_but_absent_fails_by_name() {
    let dir = TempDir::new().expect("a temp verb directory");
    std::fs::write(
        dir.path().join("required.txt"),
        "kept.trycmd\ndeleted.trycmd\n",
    )
    .expect("required.txt");
    std::fs::write(dir.path().join("kept.trycmd"), "").expect("the present case");

    let failures = unmet_requirements(dir.path());

    assert_eq!(failures.len(), 1, "{failures:#?}");
    assert!(
        failures[0].contains("deleted.trycmd") && failures[0].contains("but absent"),
        "{failures:#?}"
    );
}

#[test]
fn a_verb_directory_whose_required_cases_are_all_present_reports_nothing() {
    let dir = TempDir::new().expect("a temp verb directory");
    std::fs::write(dir.path().join("required.txt"), "kept.trycmd\n").expect("required.txt");
    std::fs::write(dir.path().join("kept.trycmd"), "").expect("the present case");

    assert!(unmet_requirements(dir.path()).is_empty());
}

#[test]
fn a_case_requiring_an_absent_tool_is_named_with_the_tool_that_decided_it() {
    let dir = TempDir::new().expect("a temp verb directory");
    std::fs::write(dir.path().join("real-linter.requires"), "no-such-linter\n")
        .expect("the declaration");
    std::fs::write(dir.path().join("real-linter.trycmd"), "").expect("the case");

    let missing = unavailable_requirements(&[dir.path().to_path_buf()], &path_with_checkers());

    assert_eq!(missing.len(), 1, "{missing:#?}");
    assert!(
        missing[0].0.ends_with("real-linter.trycmd"),
        "the case must be named: {}",
        missing[0].0
    );
    assert_eq!(missing[0].1, "no-such-linter");
}

#[test]
fn a_case_requiring_a_tool_on_path_is_left_to_run() {
    let dir = TempDir::new().expect("a temp verb directory");
    std::fs::write(dir.path().join("shell.requires"), "sh\n").expect("the declaration");
    std::fs::write(dir.path().join("shell.trycmd"), "").expect("the case");

    assert!(
        unavailable_requirements(&[dir.path().to_path_buf()], &path_with_checkers()).is_empty()
    );
}

/// `checker-pass` is reachable only through the augmented `PATH`.
#[test]
fn a_case_requiring_a_bundled_checker_runs_and_the_ambient_path_alone_would_have_skipped_it() {
    let dir = TempDir::new().expect("a temp verb directory");
    std::fs::write(dir.path().join("bundled.requires"), "checker-pass\n").expect("the declaration");
    std::fs::write(dir.path().join("bundled.trycmd"), "").expect("the case");
    let dirs = [dir.path().to_path_buf()];

    let with_checkers = unavailable_requirements(&dirs, &path_with_checkers());
    let ambient = std::env::var("PATH").expect("the test runner has a PATH");

    assert!(with_checkers.is_empty(), "{with_checkers:#?}");
    assert_eq!(
        unavailable_requirements(&dirs, &ambient)
            .into_iter()
            .map(|(_, tool)| tool)
            .collect::<Vec<_>>(),
        vec!["checker-pass".to_owned()]
    );
}

#[test]
fn a_declaration_of_several_tools_is_skipped_by_the_one_that_is_absent() {
    let dir = TempDir::new().expect("a temp verb directory");
    std::fs::write(
        dir.path().join("toolchain.requires"),
        "checker-pass\nno-such-linter\n",
    )
    .expect("the declaration");
    std::fs::write(dir.path().join("toolchain.trycmd"), "").expect("the case");

    let missing = unavailable_requirements(&[dir.path().to_path_buf()], &path_with_checkers());

    assert_eq!(missing.len(), 1, "{missing:#?}");
    assert_eq!(missing[0].1, "no-such-linter");
}

#[test]
fn cmd() {
    support::sandbox::prepare(
        "read",
        &Path::new(env!("CARGO_MANIFEST_DIR")).join(CASE_TREE),
    );

    // A temp HOME makes a case that starts using HOME fail loudly. `ignore` reads a global
    // gitignore under `XDG_CONFIG_HOME`, so the runner's own excludes would steer `find`.
    let home = TempDir::new().expect("a temp HOME");
    let home = home.path().to_str().expect("a utf-8 temp path").to_owned();
    // Not under HOME: `cli/home-is-sandboxed` asserts HOME stays empty.
    let runtime = TempDir::new().expect("a temp XDG_RUNTIME_DIR");
    let runtime = runtime
        .path()
        .to_str()
        .expect("a utf-8 temp path")
        .to_owned();

    let path = path_with_checkers();
    let cases = trycmd::TestCases::new();
    cases
        .case("tests/cmd/*/*.trycmd")
        .case("tests/cmd/*/*.toml")
        .register_bin("lets", trycmd::cargo::cargo_bin!("lets"))
        .register_bin("sh", Path::new("/bin/sh"))
        .insert_var("[VERSION]", env!("CARGO_PKG_VERSION"))
        .expect("[VERSION] is a valid trycmd variable name")
        .env("LETS_NO_STATS", "1")
        .env("LETS_TOKEN_RATIO", "4")
        .env("HOME", &home)
        .env("CODEX_HOME", "")
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_CONFIG_HOME", &home)
        .env("PATH", &path);

    // trycmd names every skipped case as it runs; the tool that decided it is only known here.
    let skipped = unavailable_requirements(&verb_dirs(), &path);
    record_skips(&skipped);
    for (case, tool) in skipped {
        eprintln!("skipped ({tool} absent): {case}");
        cases.skip(&case);
    }
}

/// Written even when empty, so a stale file from an earlier run is never read as this one's.
fn record_skips(skipped: &[(String, String)]) {
    let mut text = String::new();
    for (case, tool) in skipped {
        writeln!(text, "skipped ({tool} absent): {case}").expect("a String never fails to write");
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(SKIPPED_LOG);
    std::fs::write(&path, text).unwrap_or_else(|error| {
        panic!("recording skips to {}: {error}", path.display());
    });
}

/// The tree must be a git working tree, or `ignore` skips every `.gitignore` in it.
#[test]
fn a_prepared_tree_is_a_throwaway_working_tree() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tree = manifest.join("target/cmd-fixtures/read-probe");

    support::sandbox::prepare("read", &tree);

    assert!(
        tree.join(".git").is_dir(),
        "not a work tree: {}",
        tree.display()
    );
    assert!(tree.join("small.md").is_file());
    assert!(
        !manifest.join("tests/fixtures/read/.git").exists(),
        "the committed fixture tree may never become a repository of its own"
    );
}

/// No `.trycmd` can carry this: mode 000 does not survive the fixture copy.
#[test]
fn stats_names_an_unreadable_transcript_in_its_footer() {
    use std::os::unix::fs::PermissionsExt as _;

    let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"lets show a.ts"}}]}}"#;
    let sandbox = support::sandbox::sandbox("read");
    let dir = sandbox.path().join("transcripts");
    std::fs::create_dir(&dir).expect("the transcripts dir");
    std::fs::write(dir.join("readable.jsonl"), format!("{line}\n")).expect("a readable session");
    let locked = dir.join("locked.jsonl");
    std::fs::write(&locked, format!("{line}\n")).expect("the session made unreadable");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");
    if std::fs::File::open(&locked).is_ok() {
        eprintln!(
            "skipped (running as root, mode 000 does not deny a read): \
             stats_names_an_unreadable_transcript_in_its_footer"
        );
        return;
    }

    let text = sandbox.lets(["stats", "--dir", "transcripts"]);
    let json = sandbox.lets(["stats", "--dir", "transcripts", "--json"]);

    assert_eq!(text.code, 0, "{}", text.err);
    assert!(text.out.starts_with("sessions         1\n"), "{}", text.out);
    assert!(
        text.out.ends_with("\n── skipped: 1 unreadable file\n"),
        "{}",
        text.out
    );
    assert_eq!(json.code, 0, "{}", json.err);
    let report: serde_json::Value = serde_json::from_str(&json.out).expect("one JSON object");
    assert_eq!(report["sessions"], 1);
    assert_eq!(
        report["skipped"],
        serde_json::json!({"unreadable_files": 1})
    );
}

#[test]
fn stats_over_readable_transcripts_names_no_skip() {
    let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"lets show a.ts"}}]}}"#;
    let sandbox = support::sandbox::sandbox("read");
    let dir = sandbox.path().join("transcripts");
    std::fs::create_dir(&dir).expect("the transcripts dir");
    for name in ["readable.jsonl", "locked.jsonl"] {
        std::fs::write(dir.join(name), format!("{line}\n")).expect("a readable session");
    }

    let text = sandbox.lets(["stats", "--dir", "transcripts"]);
    let json = sandbox.lets(["stats", "--dir", "transcripts", "--json"]);

    assert_eq!(text.code, 0, "{}", text.err);
    assert!(text.out.starts_with("sessions         2\n"), "{}", text.out);
    assert!(!text.out.contains("skipped"), "{}", text.out);
    let report: serde_json::Value = serde_json::from_str(&json.out).expect("one JSON object");
    assert!(report.get("skipped").is_none(), "{}", json.out);
}

/// Line 7's `    done` also ends the block at line 3, and `--new` holds a `'`, so landing on
/// line 7 needs both the occurrence and the quoting to survive bash.
#[test]
fn the_guessed_span_refusal_names_a_command_that_inserts_after_the_span_s_last_line() {
    let sandbox = support::sandbox::sandbox("read");
    let before =
        "step build\n    run make\n    done\n\ndef deploy\n    run make\n    done\n\nstep test\n";
    std::fs::write(sandbox.path().join("jobs.pipeline"), before).expect("the plaintext file");

    let refused = sandbox.lets([
        "edit",
        "jobs.pipeline",
        "--insert-after",
        "#deploy",
        "--new",
        "    echo 'shipped'",
    ]);

    assert_eq!(refused.code, 1, "{}", refused.err);
    assert_eq!(
        sandbox.read("jobs.pipeline"),
        before,
        "a refusal writes nothing"
    );
    let marker = "anchor on its last line: ";
    let message = refused.err.lines().next().expect("the refusal line");
    assert!(message.starts_with("jobs.pipeline#deploy: "), "{message}");
    let command = &message[message.find(marker).expect("a suggested command") + marker.len()..];

    let ran = sandbox.bash(command);

    assert_eq!(ran.code, 0, "{command}\n{}", ran.err);
    assert_eq!(
        sandbox.read("jobs.pipeline"),
        "step build\n    run make\n    done\n\ndef deploy\n    run make\n    done\n    echo \
         'shipped'\n\nstep test\n"
    );
}

/// `end` also closes `setup` above `run_job`, so the suggested command lands at top level only
/// if the guessed span reaches the second `end` and the occurrence picks it over the first.
#[test]
fn the_guessed_span_refusal_on_a_lua_function_inserts_after_its_end_at_top_level() {
    let sandbox = support::sandbox::sandbox("read");
    let before = "function setup()\n  return 1\nend\n\nfunction run_job()\n  local ok = \
                  true\n  return true\nend\n\nprint(1)\n";
    std::fs::write(sandbox.path().join("job.lua"), before).expect("the plaintext file");

    let refused = sandbox.lets([
        "edit",
        "job.lua",
        "--insert-after",
        "#run_job",
        "--new",
        "function extra() end",
    ]);

    assert_eq!(refused.code, 1, "{}", refused.err);
    assert_eq!(sandbox.read("job.lua"), before, "a refusal writes nothing");
    let marker = "anchor on its last line: ";
    let message = refused.err.lines().next().expect("the refusal line");
    let command = &message[message.find(marker).expect("a suggested command") + marker.len()..];

    let ran = sandbox.bash(command);

    assert_eq!(ran.code, 0, "{command}\n{}", ran.err);
    assert_eq!(
        sandbox.read("job.lua"),
        "function setup()\n  return 1\nend\n\nfunction run_job()\n  local ok = true\n  return \
         true\nend\nfunction extra() end\n\nprint(1)\n"
    );
}

/// A `.toml` case can't unset `LETS_NO_STATS`: trycmd merges the suite env back in after
/// `[env] remove`.
#[test]
fn show_all_prints_the_cost_line_before_the_first_header() {
    let sandbox = support::sandbox::sandbox("read");

    let run = sandbox.bash("unset LETS_NO_STATS; lets show big.ts --all");

    assert_eq!(run.code, 0);
    let first = run.out.lines().next().expect("show --all has output");
    assert!(
        first.starts_with("── ~") && first.ends_with(" tokens"),
        "the cost line must lead: {first}"
    );
}

/// Shows the case above proves `--all`'s effect, not just that stats are on.
#[test]
fn show_without_all_keeps_the_header_first() {
    let sandbox = support::sandbox::sandbox("read");

    let run = sandbox.bash("unset LETS_NO_STATS; lets show big.ts");

    assert_eq!(run.code, 0);
    let first = run.out.lines().next().expect("show has output");
    assert!(
        first.starts_with("── big.ts"),
        "the header must lead: {first}"
    );
}

/// grep's `-n -r -R -E -H` are no-ops for `find` and `-l`/`-c` are `--files`/`--count`; each
/// pattern takes a different exit path (hits, over the hit cap, no hit).
#[test]
fn grep_spellings_print_what_the_native_flags_print() {
    let sandbox = support::sandbox::sandbox("read");

    for pattern in ["const", "filler", "zz-no-hit-zz"] {
        let pairs = [
            (
                vec![
                    "find",
                    "-n",
                    "-r",
                    "-R",
                    "-E",
                    "-H",
                    "--line-number",
                    pattern,
                ],
                vec!["find", pattern],
            ),
            (vec!["find", "-l", pattern, "."], vec![
                "find", "--files", pattern, ".",
            ]),
            (vec!["find", "-c", pattern, "."], vec![
                "find", "--count", pattern, ".",
            ]),
        ];
        for (grep, native) in pairs {
            let (grep_run, native_run) = (sandbox.lets(&grep), sandbox.lets(&native));
            assert_eq!(grep_run.out, native_run.out, "{grep:?} vs {native:?}");
            assert_eq!(grep_run.code, native_run.code, "{grep:?} vs {native:?}");
        }
    }
}

/// So the equalities above compare modes that differ, not one mode printed three ways.
#[test]
fn short_l_and_c_differ_from_a_plain_find() {
    let sandbox = support::sandbox::sandbox("read");
    let plain = sandbox.lets(["find", "const", "."]);
    assert_eq!(plain.code, 0);

    let files = sandbox.lets(["find", "-l", "const", "."]);
    let count = sandbox.lets(["find", "-c", "const", "."]);

    assert_ne!(files.out, plain.out);
    assert_ne!(count.out, plain.out);
    assert_ne!(files.out, count.out);
}

#[test]
fn new_dash_reads_stdin_instead_of_writing_a_literal_dash() {
    let sandbox = support::sandbox::sandbox("read");
    std::fs::write(sandbox.path().join("n.md"), "note\n").expect("the fixture");

    let run = sandbox.bash("printf 'Y' | lets edit n.md --old note --new -");

    assert_eq!(run.code, 0, "{}", run.err);
    // `--old note` matches only the word, so the fixture's trailing newline survives.
    assert_eq!(sandbox.read("n.md"), "Y\n");
}

#[test]
fn new_dash_reads_multiline_stdin_verbatim() {
    let sandbox = support::sandbox::sandbox("read");
    std::fs::write(sandbox.path().join("n.md"), "note\n").expect("the fixture");

    let run = sandbox.bash("lets edit n.md --old note --new - <<'EOF'\nline one\nline two\nEOF");

    assert_eq!(run.code, 0, "{}", run.err);
    // The heredoc's trailing newline, then the fixture's own past the matched `note`.
    assert_eq!(sandbox.read("n.md"), "line one\nline two\n\n");
}

#[test]
fn old_dash_with_content_matches_like_a_literal_old() {
    let sandbox = support::sandbox::sandbox("read");
    std::fs::write(sandbox.path().join("n.md"), "note\n").expect("the fixture");

    let run = sandbox.bash("printf 'note' | lets edit n.md --old - --new done");

    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(sandbox.read("n.md"), "done\n");
}

#[test]
fn old_dash_and_new_dash_together_are_refused_as_usage() {
    let sandbox = support::sandbox::sandbox("read");
    std::fs::write(sandbox.path().join("n.md"), "note\n").expect("the fixture");

    let run = sandbox.bash("printf 'x' | lets edit n.md --old - --new -");

    assert_eq!(run.code, 64, "{}", run.err);
    assert!(run.err.contains("ERROR_CODE=usage"), "{}", run.err);
    assert!(run.err.contains("--from -"), "{}", run.err);
    assert_eq!(sandbox.read("n.md"), "note\n");
}

#[test]
fn old_dash_with_empty_stdin_is_refused_as_usage() {
    let sandbox = support::sandbox::sandbox("read");
    std::fs::write(sandbox.path().join("n.md"), "note\n").expect("the fixture");

    let run = sandbox.bash("printf '' | lets edit n.md --old - --new x");

    assert_eq!(run.code, 64, "{}", run.err);
    assert!(run.err.contains("ERROR_CODE=usage"), "{}", run.err);
    assert_eq!(sandbox.read("n.md"), "note\n");
}

#[test]
fn new_dash_with_empty_stdin_deletes_the_match() {
    let sandbox = support::sandbox::sandbox("read");
    std::fs::write(sandbox.path().join("n.md"), "note\n").expect("the fixture");

    let run = sandbox.bash("printf '' | lets edit n.md --old note --new -");

    assert_eq!(run.code, 0, "{}", run.err);
    assert_eq!(sandbox.read("n.md"), "\n");
}

#[test]
fn generating_examples_twice_produces_byte_identical_output() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let first = TempDir::new().expect("a temp output directory");
    let second = TempDir::new().expect("a temp output directory");

    for out in [first.path(), second.path()] {
        let status = std::process::Command::new("sh")
            .arg(manifest.join("scripts/gen-examples.sh"))
            .arg(out)
            // Run from inside the `cmd` test binary itself: re-running `cargo nextest run --test
            // cmd` here would recurse into this very test.
            .env("LETS_GEN_EXAMPLES_SKIP_TEST", "1")
            .current_dir(manifest)
            .status()
            .expect("gen-examples.sh runs");
        assert!(status.success(), "gen-examples.sh exited non-zero");
    }

    let mut names: Vec<_> = std::fs::read_dir(first.path())
        .expect("the first output directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect();
    names.sort();
    assert!(!names.is_empty(), "gen-examples.sh produced nothing");

    for name in names {
        let a = std::fs::read(first.path().join(&name)).expect("the first run's file");
        let b = std::fs::read(second.path().join(&name)).expect("the second run's file");
        assert_eq!(
            a, b,
            "{name:?} differs between two gen-examples.sh runs on identical input"
        );
    }
}
