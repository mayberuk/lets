//! The hook's verdict on these commands is the hook corpus's job, so nothing here synthesizes a
//! `PreToolUse` event.

mod support;

use std::path::{Path, PathBuf};

use regex::Regex;
use support::sandbox::sandbox;
use tempfile::TempDir;

/// Only the `lets` arm is mandatory: the one-call equivalent is why a scenario exists.
const ARMS: [&str; 2] = ["today", "lets"];

const SEPARATOR: &str = "# ---";

/// Keeps `NN-<slug>.<stream>` well inside the 255-byte filename limit.
const SLUG_MAX: usize = 40;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Compare,
    Overwrite,
}

/// A block, not a line: cutting a heredoc, `if` or `for` apart replays what no agent typed.
struct Step {
    slug: String,
    body: String,
}

fn scenario_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios")
}

#[test]
fn every_scenario_replays_byte_for_byte() {
    let root = scenario_root();
    let mode = match std::env::var("LETS_GOLDEN").as_deref() {
        Ok("overwrite") => Mode::Overwrite,
        _ => Mode::Compare,
    };

    assert!(
        !directories(&root).is_empty(),
        "no scenarios under {} — an emptied tier is red, not green",
        root.display()
    );

    let failures = replay(&root, mode);

    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}

#[test]
fn no_committed_golden_carries_a_path_a_date_a_version_or_a_token_estimate() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let groups = [
        goldens(&scenario_root()),
        suffixed(&manifest.join("tests/cmd"), &["stdout", "stderr"]),
        suffixed(&manifest.join("tests/snapshots"), &["snap"]),
    ];

    let mut failures = Vec::new();
    for group in &groups {
        assert!(!group.is_empty(), "a tier with no golden to check is red");
        for golden in group {
            let text = std::fs::read_to_string(golden).expect("a golden is utf-8");
            failures.extend(offences(golden, &text));
        }
    }

    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn a_golden_carrying_each_forbidden_thing_is_reported_by_name() {
    let root = TempDir::new().expect("a temp golden directory");
    let golden = root.path().join("01-step.out");
    let text = format!(
        "{}/tests/fixtures/read/small.md\n/tmp/build-3f2/out\nlets {}\nbuilt 2026-09-21\n── \
         showed 1 target · ~0.9k tokens\n",
        env!("CARGO_MANIFEST_DIR"),
        env!("CARGO_PKG_VERSION"),
    );
    std::fs::write(&golden, &text).expect("a temp golden is writable");

    let found = offences(
        &golden,
        &std::fs::read_to_string(&golden).expect("it reads back"),
    );

    assert_eq!(found.len(), 5, "{found:#?}");
    for rule in [
        "checkout path",
        "temp path",
        "crate version",
        "date",
        "token estimate",
    ] {
        assert!(
            found.iter().any(|failure| failure.contains(rule)),
            "{rule} went unreported: {found:#?}"
        );
    }
}

fn offences(golden: &Path, text: &str) -> Vec<String> {
    let date = Regex::new(r"\d{4}-\d{2}-\d{2}").expect("a valid pattern");
    let tokens = Regex::new(r"~[0-9]+(\.[0-9]+k)? tokens").expect("a valid pattern");
    let named = golden.display();
    [
        text.contains(env!("CARGO_MANIFEST_DIR"))
            .then(|| format!("{named} carries the checkout path")),
        text.contains("/tmp/")
            .then(|| format!("{named} carries a temp path")),
        text.contains(env!("CARGO_PKG_VERSION"))
            .then(|| format!("{named} carries the crate version")),
        date.is_match(text)
            .then(|| format!("{named} carries a date")),
        tokens
            .is_match(text)
            .then(|| format!("{named} carries a token estimate")),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn suffixed(root: &Path, suffixes: &[&str]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            found.extend(suffixed(&path, suffixes));
        } else if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| suffixes.contains(&extension))
        {
            found.push(path);
        }
    }
    found.sort();
    found
}

fn replay(root: &Path, mode: Mode) -> Vec<String> {
    let mut failures = Vec::new();
    for verb in directories(root) {
        let directory = root.join(&verb);
        let scenarios = directories(&directory);
        failures.extend(unmet_requirements(&directory, &verb, &scenarios));
        for name in &scenarios {
            failures.extend(replay_scenario(
                &directory.join(name),
                &format!("{verb}/{name}"),
                mode,
            ));
        }
    }
    failures
}

fn replay_scenario(directory: &Path, id: &str, mode: Mode) -> Vec<String> {
    let Ok(fixture) = std::fs::read_to_string(directory.join("fixture")) else {
        return vec![format!("{id}: no `fixture` file naming the tree to copy")];
    };
    let fixture = fixture.trim();

    let mut failures = Vec::new();
    if !directory.join("lets").is_dir() {
        failures.push(format!(
            "{id}: no `lets` arm — a scenario without the one-call form proves nothing"
        ));
    }
    for arm in ARMS {
        let arm_directory = directory.join(arm);
        if arm_directory.is_dir() {
            failures.extend(replay_arm(
                &arm_directory,
                &format!("{id}/{arm}"),
                fixture,
                mode,
            ));
        }
    }
    failures
}

/// One sandbox for the whole arm: step 2 reads what step 1 wrote.
fn replay_arm(directory: &Path, id: &str, fixture: &str, mode: Mode) -> Vec<String> {
    let Ok(script) = std::fs::read_to_string(directory.join("script.sh")) else {
        return vec![format!("{id}: no script.sh")];
    };
    let steps = steps(&script);
    if steps.is_empty() {
        return vec![format!("{id}: script.sh has no steps")];
    }

    let expected = directory.join("expected");
    let sandbox = sandbox(fixture);
    let mut failures = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let name = format!("{:02}-{}", index + 1, step.slug);
        let run = sandbox.bash(&step.body);
        let exit = format!("{}\n", run.code);
        for (stream, actual) in [
            ("out", run.out.as_str()),
            ("err", run.err.as_str()),
            ("exit", exit.as_str()),
        ] {
            let golden = expected.join(format!("{name}.{stream}"));
            match mode {
                Mode::Overwrite => {
                    std::fs::create_dir_all(&expected).expect("the expected directory");
                    std::fs::write(&golden, actual).expect("a golden is writable");
                },
                Mode::Compare => match std::fs::read_to_string(&golden) {
                    Ok(want) if want == actual => {},
                    Ok(want) => failures.push(format!(
                        "{id} step {name}: {stream} differs\n--- expected\n{want}--- actual\n{actual}"
                    )),
                    Err(_) => failures.push(format!(
                        "{id} step {name}: no golden at {}; record it with LETS_GOLDEN=overwrite",
                        golden.display()
                    )),
                },
            }
        }
    }
    failures
}

/// A missing `required.txt` is a failure too, or deleting the list would delete its scenarios
/// silently.
fn unmet_requirements(directory: &Path, verb: &str, present: &[String]) -> Vec<String> {
    let path = directory.join("required.txt");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return vec![format!(
            "{verb}: no required.txt naming the scenarios that may not vanish"
        )];
    };
    text.lines()
        .map(str::trim)
        .filter(|name| !name.is_empty() && !name.starts_with('#'))
        .filter(|name| !present.iter().any(|found| found == name))
        .map(|name| format!("{verb}/{name}: named in required.txt but absent"))
        .collect()
}

fn directories(path: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn goldens(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            found.extend(goldens(&path));
        } else if path
            .parent()
            .is_some_and(|parent| parent.ends_with("expected"))
        {
            found.push(path);
        }
    }
    found
}

/// Blank and `#` lines inside a heredoc are the agent's data, not step separators.
fn steps(script: &str) -> Vec<Step> {
    let mut steps = Vec::new();
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
        if line == SEPARATOR || line.trim().is_empty() {
            close(&mut steps, &mut block);
        } else if !line.trim_start().starts_with('#') {
            open = heredoc_delimiter(line);
            block.push(line);
        }
    }
    close(&mut steps, &mut block);
    steps
}

fn close(steps: &mut Vec<Step>, block: &mut Vec<&str>) {
    if block.is_empty() {
        return;
    }
    steps.push(Step {
        slug: slug(block[0]),
        body: format!("{}\n", block.join("\n")),
    });
    block.clear();
}

/// A `script.sh` is authored, so `<<` is always a heredoc, never a shift or quoted text.
fn heredoc_delimiter(line: &str) -> Option<String> {
    let after = line.split_once("<<")?.1;
    let after = after.strip_prefix('-').unwrap_or(after);
    // `<<<` is a here-string: its word is on the same line and it opens nothing.
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

fn slug(line: &str) -> String {
    let mut slug = String::new();
    for character in line.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-')
        .chars()
        .take(SLUG_MAX)
        .collect::<String>()
        .trim_end_matches('-')
        .to_owned()
}

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("a file has a parent")).expect("a directory");
    std::fs::write(&path, contents).expect("a scenario file");
}

/// Built per test, so no deliberately broken scenario lives in the committed corpus.
fn tree(files: &[(&str, &str)]) -> TempDir {
    let root = TempDir::new().expect("a temp scenario root");
    for (relative, contents) in files {
        write(root.path(), relative, contents);
    }
    root
}

#[test]
fn a_missing_golden_fails_the_step_by_name() {
    let root = tree(&[
        ("version/reports/fixture", "base\n"),
        ("version/reports/lets/script.sh", "lets version\n"),
        ("version/required.txt", "reports\n"),
    ]);

    let failures = replay(root.path(), Mode::Compare);

    assert!(
        failures.iter().any(
            |failure| failure.contains("step 01-lets-version") && failure.contains("no golden")
        ),
        "{failures:#?}"
    );
}

#[test]
fn overwrite_records_the_goldens_a_compare_pass_then_accepts() {
    let root = tree(&[
        ("version/reports/fixture", "base\n"),
        ("version/reports/lets/script.sh", "lets version\n"),
        ("version/required.txt", "reports\n"),
    ]);
    let expected = root.path().join("version/reports/lets/expected");

    assert!(replay(root.path(), Mode::Overwrite).is_empty());

    assert_eq!(
        std::fs::read_to_string(expected.join("01-lets-version.out")).expect("a recorded golden"),
        "lets [VERSION]\n",
        "the version line is recorded with the version substituted, not baked in"
    );
    assert_eq!(
        std::fs::read_to_string(expected.join("01-lets-version.exit")).expect("a recorded golden"),
        "0\n"
    );
    assert_eq!(
        std::fs::read_to_string(expected.join("01-lets-version.err")).expect("a recorded golden"),
        ""
    );
    assert!(replay(root.path(), Mode::Compare).is_empty());
}

#[test]
fn a_golden_that_disagrees_fails_the_step_by_name() {
    let root = tree(&[
        ("version/reports/fixture", "base\n"),
        ("version/reports/lets/script.sh", "lets version\n"),
        ("version/required.txt", "reports\n"),
        (
            "version/reports/lets/expected/01-lets-version.out",
            "lets 9.9.9\n",
        ),
        ("version/reports/lets/expected/01-lets-version.err", ""),
        ("version/reports/lets/expected/01-lets-version.exit", "0\n"),
    ]);

    let failures = replay(root.path(), Mode::Compare);

    assert_eq!(failures.len(), 1, "{failures:#?}");
    assert!(
        failures[0].contains("step 01-lets-version: out differs"),
        "{failures:#?}"
    );
}

#[test]
fn a_disagreeing_compare_run_leaves_the_golden_on_disk_unchanged() {
    let root = tree(&[
        ("version/reports/fixture", "base\n"),
        ("version/reports/lets/script.sh", "lets version\n"),
        ("version/required.txt", "reports\n"),
        (
            "version/reports/lets/expected/01-lets-version.out",
            "lets 9.9.9\n",
        ),
        ("version/reports/lets/expected/01-lets-version.err", ""),
        ("version/reports/lets/expected/01-lets-version.exit", "0\n"),
    ]);
    let golden = root
        .path()
        .join("version/reports/lets/expected/01-lets-version.out");
    let before = std::fs::read(&golden).expect("the golden before the run");

    let failures = replay(root.path(), Mode::Compare);

    assert_eq!(failures.len(), 1, "{failures:#?}");
    let after = std::fs::read(&golden).expect("the golden after the run");
    assert_eq!(
        before, after,
        "a Mode::Compare run must not rewrite a golden it disagrees with"
    );
}

#[test]
fn a_scenario_named_in_required_txt_but_absent_fails_by_name() {
    let root = tree(&[
        ("version/reports/fixture", "base\n"),
        ("version/reports/lets/script.sh", "lets version\n"),
        (
            "version/reports/lets/expected/01-lets-version.out",
            "lets [VERSION]\n",
        ),
        ("version/reports/lets/expected/01-lets-version.err", ""),
        ("version/reports/lets/expected/01-lets-version.exit", "0\n"),
        ("version/required.txt", "reports\ndeleted-by-someone\n"),
    ]);

    let failures = replay(root.path(), Mode::Compare);

    assert_eq!(failures, vec![
        "version/deleted-by-someone: named in required.txt but absent".to_owned()
    ]);
}

#[test]
fn a_blank_line_and_a_comment_in_required_txt_name_nothing() {
    let root = tree(&[
        ("version/reports/fixture", "base\n"),
        ("version/reports/lets/script.sh", "lets version\n"),
        (
            "version/reports/lets/expected/01-lets-version.out",
            "lets [VERSION]\n",
        ),
        ("version/reports/lets/expected/01-lets-version.err", ""),
        ("version/reports/lets/expected/01-lets-version.exit", "0\n"),
        (
            "version/required.txt",
            "\n# reports covers the one-call form\nreports\n",
        ),
    ]);

    let failures = replay(root.path(), Mode::Compare);

    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn a_verb_directory_without_required_txt_fails() {
    let root = tree(&[
        ("version/reports/fixture", "base\n"),
        ("version/reports/lets/script.sh", "lets version\n"),
        (
            "version/reports/lets/expected/01-lets-version.out",
            "lets [VERSION]\n",
        ),
        ("version/reports/lets/expected/01-lets-version.err", ""),
        ("version/reports/lets/expected/01-lets-version.exit", "0\n"),
    ]);

    let failures = replay(root.path(), Mode::Compare);

    assert_eq!(failures, vec![
        "version: no required.txt naming the scenarios that may not vanish".to_owned()
    ]);
}

#[test]
fn a_scenario_with_only_a_today_arm_fails_and_the_arm_still_replays() {
    let root = tree(&[
        ("show/pages/fixture", "base\n"),
        ("show/pages/today/script.sh", "cat README.md\n"),
        ("show/required.txt", "pages\n"),
    ]);

    let failures = replay(root.path(), Mode::Overwrite);

    assert_eq!(failures, vec![
        "show/pages: no `lets` arm — a scenario without the one-call form proves nothing"
            .to_owned()
    ]);
    assert!(
        std::fs::read_to_string(
            root.path()
                .join("show/pages/today/expected/01-cat-readme-md.out")
        )
        .expect("the today arm replayed anyway")
        .contains("# base fixture"),
        "a today arm is optional, not ignored"
    );
}

#[test]
fn a_heredoc_is_one_step_whatever_its_body_contains() {
    let heredoc = "cat > note.txt <<'EOF'\nfirst\n\n# not a comment, data\n\n# ---\nlast\nEOF\n";

    let steps = steps(&format!("{heredoc}\ncat note.txt\n"));

    assert_eq!(steps.len(), 2, "the heredoc body was cut into steps");
    assert_eq!(
        steps[0].body, heredoc,
        "a blank line, a comment and the separator are data inside a heredoc"
    );
    assert_eq!(steps[1].body, "cat note.txt\n");
}

#[test]
fn bash_runs_a_heredoc_block_as_one_command() {
    let root = tree(&[
        ("write/note/fixture", "base\n"),
        (
            "write/note/lets/script.sh",
            "cat > note.txt <<'EOF'\nfirst\n\n# not a comment, data\nEOF\n\ncat note.txt\n",
        ),
        ("write/required.txt", "note\n"),
    ]);

    assert!(replay(root.path(), Mode::Overwrite).is_empty());

    let expected = root.path().join("write/note/lets/expected");
    assert_eq!(
        std::fs::read_to_string(expected.join("02-cat-note-txt.out")).expect("the second step ran"),
        "first\n\n# not a comment, data\n",
        "the heredoc reached bash whole, blank line and all"
    );
}

#[test]
fn a_comment_outside_a_heredoc_is_stripped_and_the_separator_is_not_one() {
    let script =
        "# why this chain exists\nlets version\n# ---\n# and why this one follows\nlets guide\n";

    let steps = steps(script);

    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].body, "lets version\n");
    assert_eq!(steps[1].body, "lets guide\n");
}

#[test]
fn a_multi_line_construct_stays_one_step() {
    let script = "for f in a b; do\n  echo $f\ndone\n";

    let steps = steps(script);

    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].body, script);
}

#[test]
fn a_here_string_opens_no_heredoc() {
    assert_eq!(
        heredoc_delimiter("lets edit --from - <<'LETS'"),
        Some("LETS".to_owned())
    );
    assert_eq!(heredoc_delimiter("cat <<-\"SH\""), Some("SH".to_owned()));
    assert_eq!(heredoc_delimiter("cat <<EOF"), Some("EOF".to_owned()));
    assert_eq!(heredoc_delimiter("grep x <<<\"$line\""), None);
    assert_eq!(heredoc_delimiter("lets show a.ts"), None);
}

#[test]
fn a_slug_is_the_first_line_reduced_to_a_filename() {
    assert_eq!(slug("lets version > out.txt"), "lets-version-out-txt");
    assert_eq!(slug("lets guide"), "lets-guide");
    assert_eq!(
        slug("lets find 'onBack|back arrow' .claude/plans/ -C 2"),
        "lets-find-onback-back-arrow-claude-plans"
    );
    assert!(slug("x".repeat(80).as_str()).len() <= SLUG_MAX);
}
