//! `PostToolUse` sees a file only after a native `Edit`/`Write` (or Codex's `apply_patch`)
//! already ran, so there is no captured "before" the way `lets edit`'s own transaction has.

use std::fs::File;
use std::io::{Read as _, Seek as _};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::check::{self, CheckKind};
use crate::grammars::Language;
use crate::matcher::Span;

/// 1 MiB: a 200 KB random `.rs` took 8.1 s to check on a debug build.
const MAX_CHECKED_BYTES: u64 = 1 << 20;

/// `layer1` gets before == after, one edit spanning the whole file: the same shape `write.rs`
/// uses for a file with no prior state, since no real before-state is available here either.
/// Only a regular file is read: opening a named pipe blocks until something writes to it.
pub fn evaluate(path: &Path, cwd: &Path) -> Option<String> {
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let metadata = std::fs::metadata(&resolved).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_CHECKED_BYTES {
        return None;
    }
    let bytes = std::fs::read(&resolved).ok()?;
    let kind = check::checker_for(&resolved, &bytes, 0)?;
    let whole = Span {
        start: 0,
        end: bytes.len(),
    };
    let layer1 = check::layer1(kind, &bytes, &bytes, &[(whole, 0)]);
    let structural = format!("check: {} {}", layer1.label, layer1.status);
    if !layer1.ok || kind != CheckKind::Structural(Language::Go) {
        return Some(structural);
    }
    Some(match go_check(&resolved, "go", GO_BUILD_TIMEOUT) {
        None => structural,
        Some(GoCheck::Ok(tool)) => format!("{tool}: ok"),
        Some(GoCheck::Failed(tool, output)) => format!("{tool}:\n{output}"),
        Some(GoCheck::TimedOut(tool)) => timed_out(tool, GO_BUILD_TIMEOUT, &structural),
    })
}

/// The structural result stands in for the build, and the line before it says why.
fn timed_out(tool: &str, timeout: Duration, structural: &str) -> String {
    format!(
        "{tool}: timed out after {} s\n{structural}",
        timeout.as_secs()
    )
}

/// 10 s: long enough for a small package's own build, short enough that a hung compiler does not
/// stall the agent's next turn. A build catches what a syntax-only pass cannot: exactly the check
/// a model runs by hand after an edit, which this hook exists to save.
const GO_BUILD_TIMEOUT: Duration = Duration::from_secs(10);

/// Bounds a compiler's own error block to what a rewrite prompt can afford to carry.
const GO_BUILD_MAX_LINES: usize = 20;

const POLL: Duration = Duration::from_millis(20);

/// Every file the package's build or vet compiles under the current `GOOS`, `GOARCH`, build tags
/// and `CGO_ENABLED`, one base name per line.
const GO_LIST_FILES: &str = r#"{{join .GoFiles "\n"}}{{"\n"}}{{join .CgoFiles "\n"}}{{"\n"}}{{join .TestGoFiles "\n"}}{{"\n"}}{{join .XTestGoFiles "\n"}}"#;

#[derive(Debug, PartialEq, Eq)]
enum GoCheck {
    Ok(&'static str),
    Failed(&'static str, String),
    TimedOut(&'static str),
}

/// `go build` skips `_test.go` files, so a test file gets `go vet`, which type-checks them. Only a
/// file `go list` names is checked: a build that leaves the file out (a `_windows.go` suffix, a
/// build constraint, a leading `_`, cgo turned off) says nothing about it. `None` for a directory
/// outside any module, a missing `go`, a spawn error or a failed `go list`: none is evidence the
/// code is wrong, so each falls back to the structural check. `go list` and the build share one
/// `timeout`.
fn go_check(file: &Path, program: &str, timeout: Duration) -> Option<GoCheck> {
    let dir = file.parent()?;
    if !in_go_module(dir) {
        return None;
    }
    let name = file.file_name()?.to_str()?;
    let deadline = Instant::now() + timeout;
    match run_build(dir, program, &["list", "-e", "-f", GO_LIST_FILES], timeout)? {
        Outcome::Passed(listed) if listed.lines().any(|line| line == name) => {},
        Outcome::TimedOut => return Some(GoCheck::TimedOut("go list")),
        Outcome::Passed(_) | Outcome::Failed(_) => return None,
    }
    let artifact = tempfile::NamedTempFile::new().ok()?;
    let target = artifact.path().to_str()?;
    let (tool, args): (&'static str, &[&str]) = if name.ends_with("_test.go") {
        ("go vet", &["vet", "."])
    } else {
        ("go build", &["build", "-o", target, "."])
    };
    let left = deadline.saturating_duration_since(Instant::now());
    Some(match run_build(dir, program, args, left)? {
        Outcome::Passed(_) => GoCheck::Ok(tool),
        Outcome::Failed(output) => GoCheck::Failed(tool, excerpt(&output)),
        Outcome::TimedOut => GoCheck::TimedOut(tool),
    })
}

fn in_go_module(dir: &Path) -> bool {
    dir.ancestors()
        .any(|ancestor| ancestor.join("go.mod").is_file())
}

/// What the command wrote to stdout and stderr, in one stream.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Passed(String),
    Failed(String),
    TimedOut,
}

/// `None` for a spawn or wait failure. The command leads its own process group, so a timeout
/// kills the compilers `go` started along with it.
fn run_build(dir: &Path, program: &str, args: &[&str], timeout: Duration) -> Option<Outcome> {
    use std::os::unix::process::CommandExt as _;
    let capture = tempfile::tempfile().ok()?;
    let stdout = Stdio::from(capture.try_clone().ok()?);
    let stderr = Stdio::from(capture.try_clone().ok()?);
    let mut child = Command::new(program)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .ok()?;

    let mut capture = capture;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                return Some(Outcome::Passed(read(&mut capture)));
            },
            Ok(Some(_)) => return Some(Outcome::Failed(read(&mut capture))),
            Err(_) => return None,
            Ok(None) => {},
        }
        if Instant::now() >= deadline {
            kill_group(&mut child);
            return Some(Outcome::TimedOut);
        }
        std::thread::sleep(POLL);
    }
}

/// `kill` the program, not a signal call: this crate has no `libc` to send one to a group.
fn kill_group(child: &mut std::process::Child) {
    let group = format!("-{}", child.id());
    let killed = Command::new("kill")
        .args(["-KILL", "--", &group])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !killed {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn read(capture: &mut File) -> String {
    let mut bytes = Vec::new();
    if capture.rewind().is_err() || capture.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// go builds packages in parallel and prints each one's `# <package>` block as it finishes, so
/// the blocks are sorted by that header before the cut: the same errors must survive every run.
fn excerpt(output: &str) -> String {
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    for line in output.lines() {
        match blocks.last_mut() {
            Some(block) if !line.starts_with("# ") => block.push(line),
            _ => blocks.push(vec![line]),
        }
    }
    blocks.sort_by_key(|block| {
        block
            .first()
            .copied()
            .and_then(|line| line.strip_prefix("# "))
    });
    let mut lines = blocks.concat();
    let cut = lines.len().saturating_sub(GO_BUILD_MAX_LINES);
    lines.truncate(GO_BUILD_MAX_LINES);
    let excerpt = lines.join("\n");
    if cut == 0 {
        return excerpt;
    }
    format!("{excerpt}\n\u{2026} {cut} more lines")
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Instant;

    use tempfile::TempDir;

    use super::{
        Duration, GO_BUILD_TIMEOUT, GoCheck, Outcome, evaluate, excerpt, go_check, in_go_module,
        run_build, timed_out,
    };

    #[test]
    fn a_slow_command_past_its_timeout_reports_the_timeout_instead_of_hanging() {
        let dir = TempDir::new().expect("a temp directory");
        let start = Instant::now();

        let outcome = run_build(dir.path(), "sleep", &["2"], Duration::from_millis(100));

        assert_eq!(outcome, Some(Outcome::TimedOut));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "the timeout did not cut the wait short: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn the_go_timeout_is_ten_seconds() {
        assert_eq!(GO_BUILD_TIMEOUT, Duration::from_secs(10));
        assert_eq!(
            timed_out("go build", GO_BUILD_TIMEOUT, "check: structure ok"),
            "go build: timed out after 10 s\ncheck: structure ok"
        );
    }

    /// The control for the timeout: a build that finishes in time reports its own result.
    #[test]
    fn a_go_that_times_out_names_the_timeout_and_one_that_finishes_does_not() {
        let module = module_with("main.go", "package main\n\nfunc main() {}\n");
        let slow = stub_go(&module, "slow-go", "main.go", "sleep 5");
        let quick = stub_go(&module, "quick-go", "main.go", "exit 0");
        let file = module.path().join("main.go");

        assert_eq!(
            go_check(&file, &slow, Duration::from_millis(200)),
            Some(GoCheck::TimedOut("go build"))
        );
        assert_eq!(
            go_check(&file, &quick, Duration::from_secs(5)),
            Some(GoCheck::Ok("go build"))
        );
    }

    #[test]
    fn a_go_list_that_times_out_is_named_as_the_timeout() {
        let module = module_with("main.go", "package main\n");
        let stuck = script(&module, "stuck-go", "sleep 5");

        assert_eq!(
            go_check(
                &module.path().join("main.go"),
                &stuck,
                Duration::from_millis(200)
            ),
            Some(GoCheck::TimedOut("go list"))
        );
    }

    /// `sleep` runs as a child of the stub's shell, as a compiler does of `go`.
    #[test]
    fn a_timeout_kills_the_programs_children_too() {
        let module = module_with("main.go", "package main\n");
        let marker = format!("31.{}", std::process::id());
        let stub = stub_go(
            &module,
            "forking-go",
            "main.go",
            &format!("sleep {marker}; :"),
        );

        let outcome = go_check(
            &module.path().join("main.go"),
            &stub,
            Duration::from_millis(300),
        );

        assert_eq!(outcome, Some(GoCheck::TimedOut("go build")));
        let pattern = format!("^sleep {marker}$");
        let alive = || {
            std::process::Command::new("pgrep")
                .args(["-f", &pattern])
                .output()
                .expect("pgrep runs")
                .status
                .success()
        };
        let start = Instant::now();
        while alive() && start.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive(), "`sleep {marker}` outlived the timeout");
    }

    #[test]
    fn a_test_file_is_checked_with_go_vet_and_any_other_with_go_build() {
        let module = module_with("main_test.go", "package main\n");
        let echo = stub_go(
            &module,
            "echo-go",
            "main.go\nmain_test.go",
            "echo \"$1\"; exit 1",
        );

        assert_eq!(
            go_check(&module.path().join("main_test.go"), &echo, GO_BUILD_TIMEOUT),
            Some(GoCheck::Failed("go vet", "vet".to_owned()))
        );
        assert_eq!(
            go_check(&module.path().join("main.go"), &echo, GO_BUILD_TIMEOUT),
            Some(GoCheck::Failed("go build", "build".to_owned()))
        );
    }

    /// A file the build leaves out would read "ok" for code nothing compiled.
    #[test]
    fn a_file_go_list_does_not_name_falls_back_to_the_structural_check() {
        let module = module_with("bad_windows.go", "package main\n");
        let stub = stub_go(&module, "listing-go", "main.go\n\nmain_test.go", "exit 0");
        let failing = script(&module, "failing-go", "exit 1");

        assert_eq!(
            go_check(
                &module.path().join("bad_windows.go"),
                &stub,
                GO_BUILD_TIMEOUT
            ),
            None
        );
        assert_eq!(
            go_check(
                &module.path().join("main_windows.go"),
                &stub,
                GO_BUILD_TIMEOUT
            ),
            None,
            "a name only containing a listed one is not listed"
        );
        assert_eq!(
            go_check(&module.path().join("main.go"), &stub, GO_BUILD_TIMEOUT),
            Some(GoCheck::Ok("go build"))
        );
        assert_eq!(
            go_check(&module.path().join("main.go"), &failing, GO_BUILD_TIMEOUT),
            None
        );
    }

    #[test]
    fn a_missing_program_falls_back_instead_of_erroring() {
        let dir = TempDir::new().expect("a temp directory");

        let outcome = run_build(
            dir.path(),
            "lets-hook-check-missing-binary",
            &[],
            GO_BUILD_TIMEOUT,
        );

        assert_eq!(outcome, None);
    }

    #[test]
    fn a_command_reports_its_captured_output_either_way() {
        let dir = TempDir::new().expect("a temp directory");

        let passed = run_build(dir.path(), "sh", &["-c", "echo fine"], GO_BUILD_TIMEOUT);
        let failed = run_build(
            dir.path(),
            "sh",
            &["-c", "echo boom >&2; exit 1"],
            GO_BUILD_TIMEOUT,
        );

        assert_eq!(passed, Some(Outcome::Passed("fine\n".to_owned())));
        assert_eq!(failed, Some(Outcome::Failed("boom\n".to_owned())));
    }

    fn numbers(range: std::ops::RangeInclusive<u32>) -> String {
        range.map(|n| n.to_string()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn output_past_twenty_lines_is_cut_to_twenty_and_the_cut_is_named() {
        assert_eq!(
            excerpt(&numbers(1..=25)),
            format!("{}\n\u{2026} 5 more lines", numbers(1..=20))
        );
        assert_eq!(excerpt(&numbers(1..=20)), numbers(1..=20));
    }

    /// go prints each package's block as its build finishes, in an order that changes run to run.
    #[test]
    fn package_blocks_are_ordered_by_their_header_before_the_cut() {
        let finished = "go: warning\n# m/cc\nc.go:1\n# m/aa\na.go:2\na.go:1\n# m/bb\nb.go:1\n";

        assert_eq!(
            excerpt(finished),
            "go: warning\n# m/aa\na.go:2\na.go:1\n# m/bb\nb.go:1\n# m/cc\nc.go:1"
        );
    }

    #[test]
    fn a_directory_with_no_go_mod_up_its_ancestry_is_not_a_module() {
        let dir = TempDir::new().expect("a temp directory");

        assert!(!in_go_module(dir.path()));
    }

    #[test]
    fn a_directory_under_a_go_mod_is_a_module() {
        let root = TempDir::new().expect("a temp directory");
        std::fs::write(root.path().join("go.mod"), "module fixture\n").expect("a go.mod");
        let nested = root.path().join("pkg");
        std::fs::create_dir(&nested).expect("a nested directory");

        assert!(in_go_module(&nested));
    }

    #[test]
    fn evaluate_resolves_a_relative_path_against_cwd() {
        let dir = TempDir::new().expect("a temp directory");
        std::fs::write(dir.path().join("valid.json"), b"{}\n").expect("a fixture file");

        let text = evaluate(Path::new("valid.json"), dir.path());

        assert_eq!(text.as_deref(), Some("check: json ok"));
    }

    #[test]
    fn evaluate_returns_none_for_an_unrecognized_extension() {
        let dir = TempDir::new().expect("a temp directory");
        std::fs::write(dir.path().join("notes.txt"), b"hi\n").expect("a fixture file");

        assert_eq!(evaluate(Path::new("notes.txt"), dir.path()), None);
    }

    #[test]
    fn evaluate_returns_none_for_a_file_that_cannot_be_read() {
        let dir = TempDir::new().expect("a temp directory");

        assert_eq!(evaluate(Path::new("missing.json"), dir.path()), None);
    }

    /// The thread outlives a failing assertion only until the test process exits.
    #[test]
    fn a_named_pipe_is_skipped_instead_of_blocking_the_hook() {
        let dir = TempDir::new().expect("a temp directory");
        let fifo = dir.path().join("pipe.json");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo runs");
        assert!(made.success());
        let (sender, receiver) = std::sync::mpsc::channel();
        let cwd = dir.path().to_path_buf();

        std::thread::spawn(move || {
            let _ = sender.send(evaluate(Path::new("pipe.json"), &cwd));
        });

        assert_eq!(receiver.recv_timeout(Duration::from_secs(5)), Ok(None));
        std::fs::write(dir.path().join("valid.json"), b"{}\n").expect("a regular file");
        assert_eq!(
            evaluate(Path::new("valid.json"), dir.path()).as_deref(),
            Some("check: json ok")
        );
    }

    #[test]
    fn a_file_over_one_mib_is_skipped_and_one_at_the_cap_is_checked() {
        let dir = TempDir::new().expect("a temp directory");
        let broken = |len: usize| {
            let mut bytes = b"{".to_vec();
            bytes.resize(len, b' ');
            bytes
        };
        std::fs::write(dir.path().join("over.json"), broken((1 << 20) + 1)).expect("a big file");
        std::fs::write(dir.path().join("at.json"), broken(1 << 20)).expect("a big file");

        assert_eq!(evaluate(Path::new("over.json"), dir.path()), None);
        assert_eq!(
            evaluate(Path::new("at.json"), dir.path()).as_deref(),
            Some("check: json invalid")
        );
    }

    fn module_with(file: &str, content: &str) -> TempDir {
        let module = TempDir::new().expect("a temp module");
        std::fs::write(module.path().join("go.mod"), "module fixture\n").expect("a go.mod");
        std::fs::write(module.path().join(file), content).expect("a go file");
        module
    }

    /// A `go` whose `go list` names `listed` and whose build or vet runs `body`.
    fn stub_go(dir: &TempDir, name: &str, listed: &str, body: &str) -> String {
        script(
            dir,
            name,
            &format!("if [ \"$1\" = list ]; then printf '%s\\n' '{listed}'; exit 0; fi\n{body}"),
        )
    }

    /// An executable standing in for `go`, so a test decides how long it runs and what it says.
    fn script(dir: &TempDir, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("a script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("an executable script");
        path.to_str().expect("a utf-8 temp path").to_owned()
    }
}
