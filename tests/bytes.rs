//! Every assertion reads the file back with `std::fs::read`: a `String` round-trip would
//! normalise the very bytes under test.

use std::fmt::Write as _;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use tempfile::TempDir;

struct Run {
    code: Option<i32>,
    out: String,
    err: String,
}

fn last_line(stream: &str) -> &str {
    stream.lines().last().unwrap_or_default()
}

/// `git init`-ed so the scope guard sees a working tree.
struct Sandbox {
    dir: TempDir,
    home: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = TempDir::new().expect("fixture tempdir");
        let mut git = Command::new("git");
        // A pre-commit hook's inherited `GIT_*` would point this `git init` at the real repo.
        for (name, _) in std::env::vars_os() {
            if name.to_str().is_some_and(|name| name.starts_with("GIT_")) {
                git.env_remove(name);
            }
        }
        let status = git
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .expect("git runs");
        assert!(status.success(), "git init failed");
        Sandbox {
            dir,
            home: TempDir::new().expect("home tempdir"),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn write(&self, name: &str, bytes: &[u8]) {
        std::fs::write(self.path(name), bytes).expect("write fixture file");
    }

    fn read(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.path(name)).expect("read fixture file back")
    }

    fn mode(&self, name: &str) -> u32 {
        std::fs::metadata(self.path(name))
            .expect("stat fixture file")
            .permissions()
            .mode()
            & 0o777
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lets"));
        command
            .args(args)
            .current_dir(self.dir.path())
            .env("HOME", self.home.path())
            .env("XDG_RUNTIME_DIR", self.home.path())
            .env("LETS_NO_STATS", "1")
            .env("LETS_TOKEN_RATIO", "4");
        command
    }

    fn lets(&self, args: &[&str]) -> Run {
        let output = self.command(args).output().expect("the built binary runs");
        Run {
            code: output.status.code(),
            out: String::from_utf8(output.stdout).expect("utf-8 stdout"),
            err: String::from_utf8(output.stderr).expect("utf-8 stderr"),
        }
    }

    /// Content goes through stdin so no shell or `OsStr` can normalise it as an argument.
    fn lets_stdin(&self, args: &[&str], content: &[u8]) -> Run {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the built binary runs");
        child
            .stdin
            .take()
            .expect("a piped stdin")
            .write_all(content)
            .expect("stdin accepts the content");
        let output = child.wait_with_output().expect("the binary exits");
        Run {
            code: output.status.code(),
            out: String::from_utf8(output.stdout).expect("utf-8 stdout"),
            err: String::from_utf8(output.stderr).expect("utf-8 stderr"),
        }
    }
}

#[test]
fn edit_replacing_one_line_in_a_crlf_file_leaves_every_other_lines_crlf_intact() {
    let sandbox = Sandbox::new();
    sandbox.write("crlf.txt", b"alpha\r\nbeta\r\ngamma\r\n");

    let run = sandbox.lets(&["edit", "crlf.txt", "--old", "beta", "--new", "BETA"]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert_eq!(
        sandbox.read("crlf.txt"),
        b"alpha\r\nBETA\r\ngamma\r\n",
        "only the matched word changes; every other \\r\\n survives byte-for-byte"
    );
}

#[test]
fn the_same_edit_on_a_plain_lf_file_gains_no_carriage_return() {
    let sandbox = Sandbox::new();
    sandbox.write("lf.txt", b"alpha\nbeta\ngamma\n");

    let run = sandbox.lets(&["edit", "lf.txt", "--old", "beta", "--new", "BETA"]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    let after = sandbox.read("lf.txt");
    assert_eq!(after, b"alpha\nBETA\ngamma\n");
    assert!(!after.contains(&b'\r'), "{after:?}");
}

#[test]
fn insert_before_line_one_lands_after_an_existing_bom() {
    let sandbox = Sandbox::new();
    sandbox.write("bom.txt", b"\xef\xbb\xbffirst\nsecond\n");

    let run = sandbox.lets(&[
        "edit",
        "bom.txt",
        "--insert-before",
        "@'first'",
        "--new",
        "NEWLINE",
    ]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    let expected = [&b"\xef\xbb\xbf"[..], b"NEWLINE\n", b"first\nsecond\n"].concat();
    assert_eq!(
        sandbox.read("bom.txt"),
        expected,
        "the BOM stays first; the inserted line goes after it, never before"
    );
}

/// Proves the BOM case's 3-byte offset is the BOM's width, not a fixed skip.
#[test]
fn insert_before_line_one_with_no_bom_lands_at_the_very_start() {
    let sandbox = Sandbox::new();
    sandbox.write("nobom.txt", b"first\nsecond\n");

    let run = sandbox.lets(&[
        "edit",
        "nobom.txt",
        "--insert-before",
        "@'first'",
        "--new",
        "NEWLINE",
    ]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert_eq!(sandbox.read("nobom.txt"), b"NEWLINE\nfirst\nsecond\n");
}

/// The euro sign (3 bytes) right before the folded curly apostrophe (3 bytes) is what a wrong
/// fold-site offset would split.
#[test]
fn normalize_replaces_a_fold_site_without_splitting_the_preceding_multibyte_character() {
    let sandbox = Sandbox::new();
    sandbox.write("fold.txt", b"\xe2\x82\xac\xe2\x80\x99s known");

    let run = sandbox.lets(&[
        "edit",
        "fold.txt",
        "--normalize",
        "--old",
        "'s",
        "--new",
        "IS",
    ]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    let expected = [&b"\xe2\x82\xac"[..], b"IS known"].concat();
    assert_eq!(
        sandbox.read("fold.txt"),
        expected,
        "the euro sign's 3 bytes must survive whole; only the curly apostrophe and the s go"
    );
}

#[test]
fn the_same_curly_needle_without_normalize_is_not_found_and_writes_nothing() {
    let sandbox = Sandbox::new();
    let before = b"\xe2\x82\xac\xe2\x80\x99s known".to_vec();
    sandbox.write("fold_ctl.txt", &before);

    let run = sandbox.lets(&["edit", "fold_ctl.txt", "--old", "'s", "--new", "IS"]);

    assert_eq!(run.code, Some(1));
    assert_eq!(last_line(&run.err), "ERROR_CODE=not_found");
    assert_eq!(sandbox.read("fold_ctl.txt"), before);
}

#[test]
fn edit_on_a_0o640_file_leaves_its_mode_bits_unchanged() {
    let sandbox = Sandbox::new();
    sandbox.write("mode.txt", b"alpha\nbeta\n");
    std::fs::set_permissions(
        sandbox.path("mode.txt"),
        std::fs::Permissions::from_mode(0o640),
    )
    .expect("chmod fixture");

    let run = sandbox.lets(&["edit", "mode.txt", "--old", "beta", "--new", "BETA"]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert_eq!(sandbox.read("mode.txt"), b"alpha\nBETA\n");
    assert_eq!(sandbox.mode("mode.txt"), 0o640);
}

/// A hardcoded default mode could pass only one of the two mode cases.
#[test]
fn edit_on_a_0o755_file_leaves_that_different_mode_unchanged_too() {
    let sandbox = Sandbox::new();
    sandbox.write("exec.txt", b"alpha\nbeta\n");
    std::fs::set_permissions(
        sandbox.path("exec.txt"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod fixture");

    let run = sandbox.lets(&["edit", "exec.txt", "--old", "beta", "--new", "BETA"]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert_eq!(sandbox.mode("exec.txt"), 0o755);
}

#[test]
fn edit_through_a_symlink_writes_the_referent_and_the_link_survives() {
    let sandbox = Sandbox::new();
    sandbox.write("real.txt", b"alpha\nbeta\n");
    std::os::unix::fs::symlink(sandbox.path("real.txt"), sandbox.path("link.txt"))
        .expect("symlink fixture");

    let run = sandbox.lets(&["edit", "link.txt", "--old", "beta", "--new", "BETA"]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert_eq!(sandbox.read("real.txt"), b"alpha\nBETA\n");
    let link_meta = std::fs::symlink_metadata(sandbox.path("link.txt")).expect("link survives");
    assert!(link_meta.file_type().is_symlink());
    assert_eq!(
        std::fs::read_link(sandbox.path("link.txt")).expect("readlink"),
        sandbox.path("real.txt")
    );
}

#[test]
fn edit_through_a_symlink_to_a_hardlinked_referent_is_refused_before_any_write() {
    let sandbox = Sandbox::new();
    sandbox.write("a.txt", b"alpha\nbeta\n");
    std::fs::hard_link(sandbox.path("a.txt"), sandbox.path("b.txt")).expect("hard_link fixture");
    std::os::unix::fs::symlink(sandbox.path("a.txt"), sandbox.path("link.txt"))
        .expect("symlink fixture");

    let run = sandbox.lets(&["edit", "link.txt", "--old", "beta", "--new", "BETA"]);

    assert_eq!(run.code, Some(7));
    assert_eq!(last_line(&run.err), "ERROR_CODE=unsupported_file");
    assert_eq!(sandbox.read("a.txt"), b"alpha\nbeta\n");
    assert_eq!(sandbox.read("b.txt"), b"alpha\nbeta\n");
}

#[test]
fn edit_on_a_hardlinked_file_exits_7_and_leaves_both_links_content_unchanged() {
    let sandbox = Sandbox::new();
    sandbox.write("a.txt", b"alpha\nbeta\n");
    std::fs::hard_link(sandbox.path("a.txt"), sandbox.path("b.txt")).expect("hard_link fixture");

    let run = sandbox.lets(&["edit", "a.txt", "--old", "beta", "--new", "BETA"]);

    assert_eq!(run.code, Some(7));
    assert_eq!(last_line(&run.err), "ERROR_CODE=unsupported_file");
    assert_eq!(sandbox.read("a.txt"), b"alpha\nbeta\n");
    assert_eq!(sandbox.read("b.txt"), b"alpha\nbeta\n");
}

#[test]
fn edit_on_a_single_link_file_succeeds() {
    let sandbox = Sandbox::new();
    sandbox.write("single.txt", b"alpha\nbeta\n");

    let run = sandbox.lets(&["edit", "single.txt", "--old", "beta", "--new", "BETA"]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert_eq!(sandbox.read("single.txt"), b"alpha\nBETA\n");
}

#[test]
fn write_stores_crlf_content_byte_for_byte() {
    let sandbox = Sandbox::new();

    let run = sandbox.lets_stdin(&["write", "crlf.txt"], b"alpha\r\nbeta\r\n");

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert_eq!(
        sandbox.read("crlf.txt"),
        b"alpha\r\nbeta\r\n",
        "write creates a file from stdin's exact bytes; no ending is rewritten"
    );
}

/// Without this, a `write` that turned every ending into CRLF would pass the case above.
#[test]
fn write_stores_lf_content_with_no_carriage_return() {
    let sandbox = Sandbox::new();

    let run = sandbox.lets_stdin(&["write", "lf.txt"], b"alpha\nbeta\n");

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert_eq!(sandbox.read("lf.txt"), b"alpha\nbeta\n");
}

#[test]
fn write_force_onto_a_0o755_file_keeps_that_mode() {
    let sandbox = Sandbox::new();
    sandbox.write("hook.sh", b"echo old\n");
    std::fs::set_permissions(
        sandbox.path("hook.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod fixture");

    let run = sandbox.lets_stdin(&["write", "hook.sh", "--force"], b"echo new\n");

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert_eq!(sandbox.read("hook.sh"), b"echo new\n");
    assert_eq!(sandbox.mode("hook.sh"), 0o755);
}

/// A hardcoded 0o755 would pass the case above and fail here.
#[test]
fn write_force_onto_a_0o600_file_keeps_that_different_mode_too() {
    let sandbox = Sandbox::new();
    sandbox.write("secret.txt", b"old\n");
    std::fs::set_permissions(
        sandbox.path("secret.txt"),
        std::fs::Permissions::from_mode(0o600),
    )
    .expect("chmod fixture");

    let run = sandbox.lets_stdin(&["write", "secret.txt", "--force"], b"new\n");

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert_eq!(sandbox.mode("secret.txt"), 0o600);
}

/// Over 2 MiB of 45-byte functions numbered from 1, so function `k` is lines `3k-2..3k`.
fn two_mib_of_functions() -> Vec<u8> {
    let text = (1..=46_604).fold(String::new(), |mut text, k| {
        write!(text, "export function f{k:05}() {{\n  return v{k:05}\n}}\n").expect("infallible");
        text
    });
    assert!(text.len() > 2 * 1024 * 1024);
    text.into_bytes()
}

#[test]
fn edit_in_a_file_over_one_mib_checks_the_enclosing_function_and_names_its_lines() {
    let sandbox = Sandbox::new();
    sandbox.write("big.ts", &two_mib_of_functions());

    let run = sandbox.lets(&[
        "edit",
        "big.ts",
        "--old",
        "return v05000",
        "--new",
        "return v05001",
    ]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert!(
        run.out
            .contains("check: structure ok in region :14998-15000 (file over 1 MiB)"),
        "{}",
        run.out
    );
}

#[test]
fn an_unclosed_brace_in_a_file_over_one_mib_reverts_it_byte_for_byte() {
    let sandbox = Sandbox::new();
    let generated = two_mib_of_functions();
    sandbox.write("big.ts", &generated);

    let run = sandbox.lets(&[
        "edit",
        "big.ts",
        "--old",
        "return v05000",
        "--new",
        "return v05000 {",
    ]);

    assert_eq!(run.code, Some(3), "{}", run.err);
    assert_eq!(last_line(&run.err), "ERROR_CODE=check_failed");
    assert!(
        sandbox.read("big.ts") == generated,
        "the revert restored every byte"
    );
}

/// Near the 8 MiB `--max-file-bytes` default; unit `i` from 0 spans lines `4i+1..=4i+4`.
#[test]
fn edit_in_a_markdown_file_near_eight_mib_checks_only_its_heading_section() {
    let sandbox = Sandbox::new();
    let mut text = String::new();
    for index in 0.. {
        let unit = format!("## unit {index}\n\nParagraph body for unit {index}.\n\n");
        if text.len() + unit.len() > 8_000_000 {
            break;
        }
        text.push_str(&unit);
    }
    sandbox.write("large.md", text.as_bytes());

    let run = sandbox.lets(&[
        "edit",
        "large.md",
        "--old",
        "Paragraph body for unit 1000.",
        "--new",
        "Paragraph body for unit 1000!",
    ]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert!(
        run.out
            .contains("check: structure ok in region :4001-4004 (file over 1 MiB)"),
        "{}",
        run.out
    );
}

const CRLF_NOTE: &str = "--old matched as CRLF";

/// `show` hides `\r`, so an LF-typed multi-line `--old` is how an agent spells lines it read.
#[test]
fn an_lf_typed_multi_line_old_matches_a_crlf_file_and_keeps_every_crlf() {
    let sandbox = Sandbox::new();
    sandbox.write("c.txt", b"a\r\nb\r\nc\r\n");

    let run = sandbox.lets(&["edit", "c.txt", "--old", "b\nc", "--new", "B\nC"]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert_eq!(sandbox.read("c.txt"), b"a\r\nB\r\nC\r\n");
    assert!(run.out.contains(CRLF_NOTE), "{}", run.out);
}

#[test]
fn a_single_line_old_on_a_crlf_file_carries_no_crlf_note() {
    let sandbox = Sandbox::new();
    sandbox.write("c.txt", b"a\r\nb\r\nc\r\n");

    let run = sandbox.lets(&["edit", "c.txt", "--old", "b", "--new", "B"]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert_eq!(sandbox.read("c.txt"), b"a\r\nB\r\nc\r\n");
    assert!(!run.out.contains(CRLF_NOTE), "{}", run.out);
}

/// A mixed-ending file is never translated: `b\r\nc` is not what `b\nc` names.
#[test]
fn a_multi_line_miss_on_a_mixed_ending_file_names_crlf_and_writes_nothing() {
    let sandbox = Sandbox::new();
    sandbox.write("m.txt", b"a\r\nb\r\nc\n");

    let run = sandbox.lets(&["edit", "m.txt", "--old", "b\nc", "--new", "B\nC"]);

    assert_eq!(run.code, Some(1), "{}", run.out);
    assert!(run.err.contains("CRLF"), "{}", run.err);
    assert_eq!(last_line(&run.err), "ERROR_CODE=mixed_endings");
    assert_eq!(sandbox.read("m.txt"), b"a\r\nb\r\nc\n");
}

#[test]
fn a_multi_line_old_matching_a_mixed_files_own_lf_applies_untranslated() {
    let sandbox = Sandbox::new();
    sandbox.write("m.txt", b"a\r\nb\nc\r\n");

    let run = sandbox.lets(&["edit", "m.txt", "--old", "b\nc", "--new", "B\nC"]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert!(!run.out.contains(CRLF_NOTE), "{}", run.out);
}

#[test]
fn a_multi_line_miss_on_an_lf_file_has_no_crlf_wording() {
    let sandbox = Sandbox::new();
    sandbox.write("l.txt", b"a\nb\nc\n");

    let run = sandbox.lets(&["edit", "l.txt", "--old", "b\nx", "--new", "B\nX"]);

    assert_eq!(run.code, Some(1), "{}", run.out);
    assert_eq!(last_line(&run.err), "ERROR_CODE=not_found");
    assert!(!run.err.contains("CRLF"), "{}", run.err);
    assert_eq!(sandbox.read("l.txt"), b"a\nb\nc\n");
}

#[test]
fn edit_on_a_0o444_file_exits_7_read_only_and_changes_neither_bytes_nor_mode() {
    let sandbox = Sandbox::new();
    sandbox.write("ro.ts", b"const cap = 10\n");
    std::fs::set_permissions(
        sandbox.path("ro.ts"),
        std::fs::Permissions::from_mode(0o444),
    )
    .expect("chmod 0444");

    let run = sandbox.lets(&["edit", "ro.ts", "--old", "cap = 10", "--new", "cap = 20"]);

    assert_eq!(run.code, Some(7), "{}", run.err);
    assert_eq!(last_line(&run.err), "ERROR_CODE=read_only");
    assert!(run.err.contains("chmod u+w"), "{}", run.err);
    assert_eq!(sandbox.read("ro.ts"), b"const cap = 10\n");
    assert_eq!(sandbox.mode("ro.ts"), 0o444);
}

#[test]
fn edit_on_a_0o644_file_applies() {
    let sandbox = Sandbox::new();
    sandbox.write("rw.ts", b"const cap = 10\n");
    std::fs::set_permissions(
        sandbox.path("rw.ts"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("chmod 0644");

    let run = sandbox.lets(&["edit", "rw.ts", "--old", "cap = 10", "--new", "cap = 20"]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert_eq!(sandbox.read("rw.ts"), b"const cap = 20\n");
}

#[test]
fn transform_on_a_0o444_file_exits_7_read_only_and_changes_neither_bytes_nor_mode() {
    let sandbox = Sandbox::new();
    sandbox.write("ro.json", b"{\"a\": 1}\n");
    std::fs::set_permissions(
        sandbox.path("ro.json"),
        std::fs::Permissions::from_mode(0o444),
    )
    .expect("chmod 0444");

    let run = sandbox.lets(&["transform", "ro.json", "--set", "a=2"]);

    assert_eq!(run.code, Some(7), "{}", run.err);
    assert_eq!(last_line(&run.err), "ERROR_CODE=read_only");
    assert!(
        run.err.contains("run chmod u+w ro.json first"),
        "{}",
        run.err
    );
    assert_eq!(sandbox.read("ro.json"), b"{\"a\": 1}\n");
    assert_eq!(sandbox.mode("ro.json"), 0o444);
}

#[test]
fn transform_on_a_0o644_file_applies() {
    let sandbox = Sandbox::new();
    sandbox.write("rw.json", b"{\"a\": 1}\n");
    std::fs::set_permissions(
        sandbox.path("rw.json"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("chmod 0644");

    let run = sandbox.lets(&["transform", "rw.json", "--set", "a=2"]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert_eq!(sandbox.read("rw.json"), b"{\"a\": 2}\n");
    assert_eq!(sandbox.mode("rw.json"), 0o644);
}

const TWO_FILE_BATCH: &[u8] = b"{\"file\":\"a.ts\",\"old\":\"cap = 10\",\"new\":\"cap = 20\"}\n\
{\"file\":\"d/b.ts\",\"old\":\"cap = 10\",\"new\":\"cap = 20\"}\n";

fn two_file_batch(sandbox: &Sandbox, b_mode: u32, dir_mode: u32) -> Run {
    std::fs::create_dir(sandbox.path("d")).expect("second file's directory");
    sandbox.write("a.ts", b"const cap = 10\n");
    sandbox.write("d/b.ts", b"const cap = 10\n");
    std::fs::set_permissions(
        sandbox.path("d/b.ts"),
        std::fs::Permissions::from_mode(b_mode),
    )
    .expect("chmod b.ts");
    std::fs::set_permissions(sandbox.path("d"), std::fs::Permissions::from_mode(dir_mode))
        .expect("chmod d");

    let run = sandbox.lets_stdin(&["edit", "--from", "-"], TWO_FILE_BATCH);

    std::fs::set_permissions(sandbox.path("d"), std::fs::Permissions::from_mode(0o755))
        .expect("restore d");
    std::fs::set_permissions(
        sandbox.path("d/b.ts"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("restore b.ts");
    run
}

#[test]
fn a_batch_holding_one_read_only_file_writes_neither_file() {
    let sandbox = Sandbox::new();

    let run = two_file_batch(&sandbox, 0o444, 0o755);

    assert_eq!(run.code, Some(7), "{}", run.err);
    assert_eq!(last_line(&run.err), "ERROR_CODE=read_only");
    assert_eq!(sandbox.read("a.ts"), b"const cap = 10\n");
    assert_eq!(sandbox.read("d/b.ts"), b"const cap = 10\n");
}

/// Each atomic write needs a temp file in its own directory first, so the refusal comes before
/// either file is written, not only before its rename.
#[test]
fn a_batch_whose_second_directory_refuses_new_files_exits_7_and_writes_neither() {
    let sandbox = Sandbox::new();

    let run = two_file_batch(&sandbox, 0o644, 0o555);

    assert_eq!(run.code, Some(7), "{}", run.err);
    assert_eq!(last_line(&run.err), "ERROR_CODE=io_error");
    assert_eq!(sandbox.read("a.ts"), b"const cap = 10\n");
    assert_eq!(sandbox.read("d/b.ts"), b"const cap = 10\n");
}

#[test]
fn the_same_batch_with_writable_files_and_directories_applies_both() {
    let sandbox = Sandbox::new();

    let run = two_file_batch(&sandbox, 0o644, 0o755);

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert_eq!(sandbox.read("a.ts"), b"const cap = 20\n");
    assert_eq!(sandbox.read("d/b.ts"), b"const cap = 20\n");
}

#[test]
fn a_single_edit_in_a_directory_that_refuses_new_files_is_io_error_not_partial_batch() {
    let sandbox = Sandbox::new();
    std::fs::create_dir(sandbox.path("d")).expect("the target's directory");
    sandbox.write("d/b.ts", b"const cap = 10\n");
    std::fs::set_permissions(sandbox.path("d"), std::fs::Permissions::from_mode(0o555))
        .expect("chmod d");

    let run = sandbox.lets(&["edit", "d/b.ts", "--old", "cap = 10", "--new", "cap = 20"]);

    std::fs::set_permissions(sandbox.path("d"), std::fs::Permissions::from_mode(0o755))
        .expect("restore d");
    assert_eq!(run.code, Some(7), "{}", run.err);
    assert_eq!(last_line(&run.err), "ERROR_CODE=io_error");
    assert_eq!(sandbox.read("d/b.ts"), b"const cap = 10\n");
}

#[test]
fn an_edit_on_an_empty_file_names_lets_write_force() {
    let sandbox = Sandbox::new();
    sandbox.write("empty.py", b"");

    let run = sandbox.lets(&["edit", "empty.py", "--old", "x", "--new", "y"]);

    assert_eq!(run.code, Some(1), "{}", run.out);
    assert_eq!(last_line(&run.err), "ERROR_CODE=empty_file");
    assert!(run.err.contains("lets write --force"), "{}", run.err);
    assert_eq!(sandbox.read("empty.py"), b"");
}

#[test]
fn a_miss_on_a_non_empty_file_keeps_not_found_with_no_write_hint() {
    let sandbox = Sandbox::new();
    sandbox.write("full.py", b"a = 1\n");

    let run = sandbox.lets(&["edit", "full.py", "--old", "x", "--new", "y"]);

    assert_eq!(run.code, Some(1), "{}", run.out);
    assert_eq!(last_line(&run.err), "ERROR_CODE=not_found");
    assert!(!run.err.contains("lets write --force"), "{}", run.err);
}

/// A stand-in `cargo` that logs where it ran: a real `cargo check` would build a project, and
/// the preset's contract is only the command and the directory.
struct CargoTree {
    sandbox: Sandbox,
    tools: TempDir,
    log: PathBuf,
}

impl CargoTree {
    fn new() -> Self {
        let sandbox = Sandbox::new();
        std::fs::create_dir_all(sandbox.path("sub/src")).expect("sub/src");
        sandbox.write("sub/Cargo.toml", b"[package]\nname = \"sub\"\n");
        sandbox.write("sub/src/lib.rs", b"pub fn cap() -> usize {\n    10\n}\n");
        let tools = TempDir::new().expect("tool dir");
        let log = sandbox.path("cargo.log");
        let script = tools.path().join("cargo");
        std::fs::write(
            &script,
            format!("#!/bin/sh\npwd >> '{}'\nexit 0\n", log.display()),
        )
        .expect("fake cargo");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake cargo");
        CargoTree {
            sandbox,
            tools,
            log,
        }
    }

    fn edit_with_check(&self, check: &str) -> Run {
        let output = self
            .sandbox
            .command(&[
                "edit",
                "sub/src/lib.rs",
                "--old",
                "10",
                "--new",
                "20",
                "--check",
                check,
            ])
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.tools.path().display()),
            )
            .output()
            .expect("the built binary runs");
        Run {
            code: output.status.code(),
            out: String::from_utf8(output.stdout).expect("utf-8 stdout"),
            err: String::from_utf8(output.stderr).expect("utf-8 stderr"),
        }
    }
}

#[test]
fn check_cargo_runs_the_checker_in_the_directory_holding_cargo_toml() {
    let tree = CargoTree::new();

    let run = tree.edit_with_check("@cargo");

    assert_eq!(run.code, Some(0), "{}", run.err);
    let sub = std::fs::canonicalize(tree.sandbox.path("sub")).expect("sub exists");
    let log = std::fs::read_to_string(&tree.log).expect("the fake cargo ran");
    let dirs: Vec<&str> = log.lines().collect();
    assert_eq!(
        dirs,
        [sub.display().to_string(), sub.display().to_string()],
        "the baseline and the post-write run both ran in sub"
    );
    assert!(
        run.out
            .contains("check: cargo check --workspace --quiet --all-targets ok"),
        "{}",
        run.out
    );
}

/// The common parent holds no manifest, so a walk from the batch's common directory finds
/// nothing.
fn two_crates(body: &str) -> (Sandbox, TempDir, PathBuf) {
    let sandbox = Sandbox::new();
    for dir in ["a", "b"] {
        std::fs::create_dir_all(sandbox.path(dir)).expect("crate dir");
        sandbox.write(&format!("{dir}/Cargo.toml"), b"[package]\n");
        sandbox.write(&format!("{dir}/n.txt"), b"x\n");
    }
    let tools = TempDir::new().expect("tool dir");
    let log = sandbox.path("cargo.log");
    let script = tools.path().join("cargo");
    std::fs::write(
        &script,
        format!("#!/bin/sh\npwd >> '{}'\n{body}\n", log.display()),
    )
    .expect("fake cargo");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("chmod fake cargo");
    (sandbox, tools, log)
}

fn edit_both_crates(sandbox: &Sandbox, tools: &TempDir) -> Run {
    let output = sandbox
        .command(&[
            "edit", "a/n.txt", "b/n.txt", "--old", "x", "--new", "y", "--check", "@cargo",
        ])
        .env("PATH", format!("{}:/usr/bin:/bin", tools.path().display()))
        .output()
        .expect("the built binary runs");
    Run {
        code: output.status.code(),
        out: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        err: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

#[test]
fn check_cargo_over_two_crates_runs_once_in_each_crate() {
    let (sandbox, tools, log) = two_crates("exit 0");

    let run = edit_both_crates(&sandbox, &tools);

    assert_eq!(run.code, Some(0), "{}", run.err);
    let dir = |name: &str| {
        std::fs::canonicalize(sandbox.path(name))
            .expect("crate exists")
            .display()
            .to_string()
    };
    let ran = std::fs::read_to_string(&log).expect("the fake cargo ran");
    assert_eq!(
        ran.lines().collect::<Vec<_>>(),
        [dir("a"), dir("b"), dir("a"), dir("b")],
        "a baseline in each crate, then a post-write run in each, in directory order"
    );
    assert!(
        run.out
            .contains("check: cargo check --workspace --quiet --all-targets ok in a/, ok in b/"),
        "{}",
        run.out
    );
    assert!(!run.out.contains("found no Cargo.toml"), "{}", run.out);
    assert_eq!(sandbox.read("a/n.txt"), b"y\n");
    assert_eq!(sandbox.read("b/n.txt"), b"y\n");
}

#[test]
fn one_crates_new_failure_reverts_both_crates_files() {
    // `b`'s check passes on `x` and fails once the edit has written `y`; `a`'s always passes.
    let (sandbox, tools, _log) =
        two_crates("[ \"${PWD##*/}\" = b ] && grep -q y n.txt && exit 1\nexit 0");

    let run = edit_both_crates(&sandbox, &tools);

    assert_eq!(run.code, Some(3), "{}", run.out);
    assert_eq!(last_line(&run.err), "ERROR_CODE=check_failed");
    assert!(
        run.err.contains(
            "`cargo check --workspace --quiet --all-targets` in b/ passed before the batch and failed after it"
        ),
        "{}",
        run.err
    );
    assert_eq!(sandbox.read("a/n.txt"), b"x\n");
    assert_eq!(sandbox.read("b/n.txt"), b"x\n");
}

#[test]
fn check_go_on_a_tree_with_no_go_mod_is_skipped_by_name_and_runs_nothing() {
    let tree = CargoTree::new();

    let run = tree.edit_with_check("@go");

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert!(
        run.out.contains("check: skipped (@go found no go.mod)"),
        "{}",
        run.out
    );
    assert!(!tree.log.exists(), "no checker ran");
    assert_eq!(
        tree.sandbox.read("sub/src/lib.rs"),
        b"pub fn cap() -> usize {\n    20\n}\n"
    );
}

#[test]
fn a_batch_over_two_files_with_no_grammar_names_each_file_in_one_skip_line() {
    let sandbox = Sandbox::new();
    sandbox.write("a.txt", b"a1\n");
    sandbox.write("b.txt", b"b1\n");

    let run = sandbox.lets_stdin(
        &["edit", "--from", "-"],
        b"{\"file\":\"a.txt\",\"old\":\"a1\",\"new\":\"a2\"}\n\
          {\"file\":\"b.txt\",\"old\":\"b1\",\"new\":\"b2\"}\n\
          {\"file\":\"a.txt\",\"old\":\"a2\",\"new\":\"a3\"}\n",
    );

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert_eq!(
        run.out
            .matches("check: skipped (a.txt: no grammar for .txt)")
            .count(),
        1,
        "{}",
        run.out
    );
    assert_eq!(
        run.out
            .matches("check: skipped (b.txt: no grammar for .txt)")
            .count(),
        1,
        "{}",
        run.out
    );
}

#[test]
fn a_single_file_edit_keeps_the_unnamed_skip_line() {
    let sandbox = Sandbox::new();
    sandbox.write("a.txt", b"a1\n");

    let run = sandbox.lets(&["edit", "a.txt", "--old", "a1", "--new", "a2"]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert!(
        run.out.contains("check: skipped (no grammar for .txt)"),
        "{}",
        run.out
    );
    assert!(!run.out.contains("a.txt: no grammar"), "{}", run.out);
}

#[test]
fn an_edit_on_a_two_mib_one_line_file_prints_at_most_max_bytes() {
    let sandbox = Sandbox::new();
    let mut minified = vec![b'y'; 2 * 1024 * 1024];
    minified.extend_from_slice(b"needle\n");
    sandbox.write("min.js", &minified);

    let run = sandbox.lets(&["edit", "min.js", "--old", "needle", "--new", "N"]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert!(run.out.len() <= 65_536, "{} bytes", run.out.len());
    assert!(run.out.contains("1 long line cut"), "{}", &run.out[..200]);
    assert!(
        run.out.contains('\u{2026}'),
        "the cut line ends in an ellipsis"
    );
}

#[test]
fn an_edited_line_under_the_display_cap_is_shown_whole() {
    let sandbox = Sandbox::new();
    let line = format!("{}needle\n", "y".repeat(900));
    sandbox.write("short.js", line.as_bytes());

    let run = sandbox.lets(&["edit", "short.js", "--old", "needle", "--new", "N"]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert!(!run.out.contains("long line"), "{}", run.out);
    assert!(run.out.contains(&format!("{}N", "y".repeat(900))));
}

/// 10 one-line `--all` matches on 21 lines, each `±1`-line context window overlapping the next,
/// merge into one contiguous 21-line region with no span over `SPAN_TRUNCATE_LINES`: only the
/// byte budget can cut it. Every rendered line is a uniform 8 bytes (7-byte text + `\n`), so
/// popping from the tail removes exactly 9 lines before 168 bytes reaches `--max-bytes 100`
/// (168 - 9*8 = 96 <= 100; 168 - 8*8 = 104 > 100), leaving line 12 (a changed line) last.
#[test]
fn a_region_over_max_bytes_is_cut_to_fit_and_the_trim_is_named() {
    let sandbox = Sandbox::new();
    let before: Vec<String> = (1..=21)
        .map(|n| {
            if n % 2 == 0 {
                "old0000".to_owned()
            } else {
                format!("keep{:03}", (n + 1) / 2)
            }
        })
        .collect();
    sandbox.write("t.txt", format!("{}\n", before.join("\n")).as_bytes());

    let run = sandbox.lets(&[
        "edit",
        "t.txt",
        "--old",
        "old0000",
        "--new",
        "new0000",
        "--all",
        "--max-bytes",
        "100",
    ]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert!(
        run.out
            .contains("output over --max-bytes 100: 9 lines not shown"),
        "{}",
        run.out
    );
    assert!(run.out.contains("12~\tnew0000"), "{}", run.out);
    assert!(!run.out.contains("keep007"), "{}", run.out);
    assert_eq!(
        sandbox.read("t.txt").len(),
        21 * 8,
        "the file holds every line"
    );
}

#[test]
fn a_region_within_max_bytes_is_not_trimmed() {
    let sandbox = Sandbox::new();
    sandbox.write("t.txt", b"x = 1\n");
    let new: Vec<String> = (1..=20).map(|n| format!("v{n:02} = 0")).collect();

    let run = sandbox.lets(&[
        "edit",
        "t.txt",
        "--old",
        "x = 1",
        "--new",
        &new.join("\n"),
        "--max-bytes",
        "160",
    ]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert!(!run.out.contains("output over"), "{}", run.out);
    assert!(run.out.contains("20~\tv20 = 0"), "{}", run.out);
}

/// The header and footer sit outside `--max-bytes`, so the 512-byte slack below covers both;
/// the header stays one `lines 1-3000` range.
fn three_thousand_x_lines() -> Vec<u8> {
    (1..=3000)
        .fold(String::new(), |mut text, n| {
            writeln!(text, "{n} x").expect("infallible");
            text
        })
        .into_bytes()
}

#[test]
fn an_all_edit_over_every_line_stays_near_max_bytes() {
    let sandbox = Sandbox::new();
    sandbox.write("f.txt", &three_thousand_x_lines());

    let run = sandbox.lets(&[
        "edit",
        "f.txt",
        "--old",
        " x",
        "--new",
        " y",
        "--all",
        "--max-bytes",
        "200",
    ]);

    assert_eq!(run.code, Some(0), "{}", run.err);
    assert!(run.out.len() <= 200 + 512, "{} bytes", run.out.len());
    assert!(
        run.out
            .starts_with("── f.txt · 3000 replacements · lines 1-3000 · exact\n"),
        "{}",
        run.out
    );
    assert!(
        run.out.contains("output over --max-bytes 200"),
        "{}",
        run.out
    );
}

#[test]
fn an_edits_cost_estimate_counts_its_header() {
    let sandbox = Sandbox::new();
    sandbox.write("f.txt", &three_thousand_x_lines());
    let args = [
        "edit",
        "f.txt",
        "--old",
        " x",
        "--new",
        " y",
        "--all",
        "--max-bytes",
        "200",
    ];

    let text = sandbox.lets(&args);
    sandbox.write("f.txt", &three_thousand_x_lines());
    let json = sandbox.lets(&[&args[..], &["--json"]].concat());

    assert_eq!(json.code, Some(0), "{}", json.err);
    let header = text.out.lines().next().expect("a header line").len() + 1;
    let object: serde_json::Value = serde_json::from_str(&json.out).expect("one JSON object");
    let content: usize = object["region"]["lines"]
        .as_array()
        .expect("the rendered region")
        .iter()
        .map(|line| line["text"].as_str().expect("line text").len() + 1)
        .sum();
    assert_eq!(object["stats"]["bytes"], header + content, "{}", json.out);
}
