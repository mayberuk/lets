//! A new file has no "before" to revert to, so unlike `edit`, a failing structural check leaves
//! the write in place.

use std::io::Read as _;
use std::path::Path;

use crate::cli::{Global, WriteArgs};
use crate::error::{Error, RefusedReason, UnsupportedReason};
use crate::output::{
    Body, CheckResult, Format, Omission, Response, Stats, WriteOutcome, WriteResult,
};
use crate::{Outcome, atomic, check, fs, lock, matcher};

pub fn run(args: &WriteArgs, global: &Global, _format: Format) -> Outcome {
    let mut content = Vec::new();
    if let Err(source) = std::io::stdin().lock().read_to_end(&mut content) {
        return Outcome::failed("write", Error::Io {
            path: args.path.clone(),
            source,
        });
    }
    decide(args, global, &content)
}

fn decide(args: &WriteArgs, global: &Global, content: &[u8]) -> Outcome {
    if content.is_empty() && !args.empty {
        return Outcome::failed("write", Error::Refused {
            path: args.path.clone(),
            reason: RefusedReason::EmptyInput,
        });
    }
    if let Some(err) = too_large(&args.path, content.len(), global.max_file_bytes) {
        return Outcome::failed("write", err);
    }
    if let Err(err) = fs::guard_scope(&args.path, global.allow_outside) {
        return Outcome::failed("write", err);
    }

    let resolved = match atomic::resolve_symlink(&args.path) {
        Ok(path) => path,
        Err(source) => {
            return Outcome::failed("write", Error::Io {
                path: args.path.clone(),
                source,
            });
        },
    };
    let exists = match std::fs::symlink_metadata(&resolved) {
        Ok(_) => true,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => false,
        Err(source) => {
            return Outcome::failed("write", Error::Io {
                path: args.path.clone(),
                source,
            });
        },
    };

    if exists && !args.force {
        return refuse_exists(args, &resolved);
    }
    create(args, global, content, exists.then_some(resolved.as_path()))
}

fn too_large(path: &Path, len: usize, limit: u64) -> Option<Error> {
    let bytes = u64::try_from(len).unwrap_or(u64::MAX);
    (bytes > limit).then(|| Error::Unsupported {
        path: path.to_path_buf(),
        reason: UnsupportedReason::TooLarge { bytes, limit },
    })
}

/// Holding the lock across the read keeps a concurrent `--force` write from landing between this
/// read and the sha it reports.
fn refuse_exists(args: &WriteArgs, resolved: &Path) -> Outcome {
    let _lock = match lock::Lock::acquire(&args.path, &lock::runtime_dir()) {
        Ok(lock) => lock,
        Err(err) => return Outcome::failed("write", err),
    };
    let existing = match std::fs::read(resolved) {
        Ok(bytes) => bytes,
        Err(source) => {
            return Outcome::failed("write", Error::Io {
                path: args.path.clone(),
                source,
            });
        },
    };
    let lines = count_lines(&existing);
    let bytes = existing.len();
    let sha = atomic::hash12(&existing);

    let mut response = Response::empty("write");
    response.stats = Stats::new(lines, bytes);
    response.body = Body::Write(WriteResult {
        path: args.path.clone(),
        outcome: WriteOutcome::Exists,
        lines,
        bytes,
        sha: sha.clone(),
        check: None,
    });
    Outcome::partial(response, Error::Refused {
        path: args.path.clone(),
        reason: RefusedReason::Exists {
            lines,
            sha: sha.as_str().to_owned(),
        },
    })
}

/// Reads `overwriting` under the lock, so `--force` reports the state it actually destroyed.
fn create(
    args: &WriteArgs,
    global: &Global,
    content: &[u8],
    overwriting: Option<&Path>,
) -> Outcome {
    let _lock = match lock::Lock::acquire(&args.path, &lock::runtime_dir()) {
        Ok(lock) => lock,
        Err(err) => return Outcome::failed("write", err),
    };
    if let Err(err) = atomic::check_before_write(&args.path, global.max_file_bytes) {
        return Outcome::failed("write", err);
    }
    let prior = match overwriting.map(std::fs::read).transpose() {
        Ok(prior) => prior,
        Err(source) => {
            return Outcome::failed("write", Error::Io {
                path: args.path.clone(),
                source,
            });
        },
    };
    if let Err(err) = atomic::write_atomic(&args.path, content, None) {
        return Outcome::failed("write", err);
    }

    let (check, omitted) = if global.no_check {
        (None, Vec::new())
    } else {
        checked(&args.path, content)
    };
    let lines = count_lines(content);
    let bytes = content.len();
    let sha = atomic::hash12(content);

    let mut response = Response::empty("write");
    response.stats = Stats::new(lines, bytes);
    response.omitted = omitted;
    response.body = Body::Write(WriteResult {
        path: args.path.clone(),
        outcome: match prior {
            Some(bytes) => WriteOutcome::Overwritten {
                prior_lines: count_lines(&bytes),
                prior_sha: atomic::hash12(&bytes),
            },
            None => WriteOutcome::Created,
        },
        lines,
        bytes,
        sha,
        check,
    });
    Outcome::ok(response)
}

/// No prior content: the whole of `content` is one inserted span.
fn checked(path: &Path, content: &[u8]) -> (Option<CheckResult>, Vec<Omission>) {
    let Some(kind) = check::checker_for(path, content, 0) else {
        let ext = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
        return (None, vec![Omission::CheckSkipped {
            reason: format!("no grammar for .{ext}"),
        }]);
    };
    let layer1 = check::layer1(kind, &[], content, &[(
        matcher::Span { start: 0, end: 0 },
        content.len(),
    )]);
    (
        Some(CheckResult {
            // Unlike `edit`, a failure keeps its label: nothing reverts, so a bare
            // `check: failed` would read as a revert.
            layer: layer1.label.to_owned(),
            status: if layer1.ok {
                layer1.status
            } else {
                format!("{} \u{b7} file kept", layer1.status)
            },
            errors_before: layer1.errors_before,
            errors_after: layer1.errors_after,
        }),
        Vec::new(),
    )
}

/// `str::lines()` counting on raw bytes, since `content` may not be UTF-8.
fn count_lines(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let segments = bytes.split(|&b| b == b'\n').count();
    if bytes.ends_with(b"\n") {
        segments - 1
    } else {
        segments
    }
}

#[cfg(test)]
mod tests {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt as _;

    use tempfile::TempDir;

    use super::*;
    use crate::output::{Format, RenderOptions};
    use crate::own_process::repo;

    fn global(max_file_bytes: u64) -> Global {
        Global {
            json: false,
            jsonl: false,
            budget: None,
            max_bytes: 65536,
            max_file_bytes,
            no_ignore: false,
            allow_outside: false,
            no_check: false,
            quiet: false,
        }
    }

    fn args(path: &Path, force: bool, empty: bool) -> WriteArgs {
        WriteArgs {
            path: path.to_path_buf(),
            force,
            empty,
        }
    }

    fn text(outcome: &Outcome) -> String {
        crate::output::render(&outcome.response, Format::Text, &RenderOptions {
            numbers: true,
            quiet: false,
        })
    }

    #[test]
    fn a_missing_path_is_created_with_stdins_exact_bytes_and_exits_0() {
        let Some(dir) = repo() else { return };
        let path = dir.path().join("scripts/new-check.sh");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        let outcome = decide(
            &args(&path, false, false),
            &global(8_388_608),
            b"#!/usr/bin/env bash\nset -euo pipefail\n",
        );

        assert!(outcome.error.is_none());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"#!/usr/bin/env bash\nset -euo pipefail\n"
        );
    }

    #[test]
    fn a_created_files_response_names_lines_bytes_and_sha() {
        let Some(dir) = repo() else { return };
        let path = dir.path().join("new-check.sh");

        let outcome = decide(
            &args(&path, false, false),
            &global(8_388_608),
            b"#!/usr/bin/env bash\nset -euo pipefail\n",
        );

        let Body::Write(result) = &outcome.response.body else {
            panic!("expected Body::Write");
        };
        assert_eq!(result.outcome, WriteOutcome::Created);
        assert_eq!(result.lines, 2);
        assert_eq!(result.bytes, 38);
        assert_eq!(
            result.sha.as_str(),
            atomic::hash12(&std::fs::read(&path).unwrap()).as_str()
        );
    }

    #[test]
    fn an_existing_path_without_force_is_not_modified_reports_exists_and_exits_1() {
        let Some(dir) = repo() else { return };
        let path = dir.path().join("scripts/new-check.sh");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"old\n").unwrap();

        let outcome = decide(&args(&path, false, false), &global(8_388_608), b"new\n");

        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"old\n",
            "the file must be untouched"
        );
        let error = outcome.error.as_ref().expect("exists is a refusal");
        assert_eq!(error.slug(), "exists");
        let Body::Write(result) = &outcome.response.body else {
            panic!("expected Body::Write on the exists path");
        };
        assert_eq!(result.outcome, WriteOutcome::Exists);
        assert_eq!(result.lines, 1);
        assert_eq!(
            text(&outcome),
            format!(
                "── {} exists (1 line, sha:{}) · pass --force to overwrite\n",
                path.display(),
                atomic::hash12(b"old\n").as_str()
            )
        );
    }

    #[test]
    fn an_existing_path_with_force_is_overwritten_and_exits_0() {
        let Some(dir) = repo() else { return };
        let path = dir.path().join("f.sh");
        std::fs::write(&path, b"old\n").unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o741)).unwrap();

        let outcome = decide(&args(&path, true, false), &global(8_388_608), b"new\n");

        assert!(outcome.error.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"new\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o741
        );
    }

    #[test]
    fn an_overwritten_file_reports_the_prior_line_count_and_hash_it_replaced() {
        let Some(dir) = repo() else { return };
        let path = dir.path().join("f.sh");
        std::fs::write(&path, b"old\nolder\n").unwrap();

        let outcome = decide(&args(&path, true, false), &global(8_388_608), b"new\n");

        let Body::Write(result) = &outcome.response.body else {
            panic!("expected Body::Write");
        };
        assert_eq!(result.outcome, WriteOutcome::Overwritten {
            prior_lines: 2,
            prior_sha: atomic::hash12(b"old\nolder\n"),
        });
        assert_eq!(
            text(&outcome),
            format!(
                "── {} · overwritten · 1 line · 4 bytes · sha:{}→{}\n── check: structure ok\n",
                path.display(),
                atomic::hash12(b"old\nolder\n").as_str(),
                atomic::hash12(b"new\n").as_str()
            )
        );
    }

    #[test]
    fn a_path_that_did_not_exist_reports_created_with_a_single_hash() {
        let Some(dir) = repo() else { return };
        let path = dir.path().join("fresh.sh");

        let outcome = decide(&args(&path, true, false), &global(8_388_608), b"new\n");

        let Body::Write(result) = &outcome.response.body else {
            panic!("expected Body::Write");
        };
        assert_eq!(result.outcome, WriteOutcome::Created);
        assert_eq!(
            text(&outcome),
            format!(
                "── {} · created · 1 line · 4 bytes · sha:{}\n── check: structure ok\n",
                path.display(),
                atomic::hash12(b"new\n").as_str()
            )
        );
    }

    #[test]
    fn empty_stdin_without_empty_flag_refuses_and_creates_nothing() {
        let Some(dir) = repo() else { return };
        let path = dir.path().join("f.txt");

        let outcome = decide(&args(&path, false, false), &global(8_388_608), &[]);

        let error = outcome.error.as_ref().expect("empty stdin is refused");
        assert_eq!(error.slug(), "empty_input");
        assert!(
            !outcome.response.has_output(),
            "the refusal owes nothing on stdout"
        );
        assert!(!path.exists());
    }

    #[test]
    fn empty_stdin_with_empty_flag_creates_a_zero_byte_file_and_exits_0() {
        let Some(dir) = repo() else { return };
        let path = dir.path().join("f.txt");

        let outcome = decide(&args(&path, false, true), &global(8_388_608), &[]);

        assert!(outcome.error.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        let Body::Write(result) = &outcome.response.body else {
            panic!("expected Body::Write");
        };
        assert_eq!(result.lines, 0);
        assert_eq!(result.bytes, 0);
    }

    #[test]
    fn content_over_max_file_bytes_exits_7_and_creates_nothing() {
        let Some(dir) = repo() else { return };
        let path = dir.path().join("f.txt");

        let outcome = decide(&args(&path, false, false), &global(4), b"12345");

        let error = outcome.error.as_ref().expect("over the limit is refused");
        assert!(matches!(error, Error::Unsupported {
            reason: UnsupportedReason::TooLarge { bytes: 5, limit: 4 },
            ..
        }));
        assert!(!path.exists());
    }

    #[test]
    fn a_written_sh_file_with_valid_bash_reports_structure_ok() {
        let Some(dir) = repo() else { return };
        let path = dir.path().join("new-check.sh");

        let outcome = decide(
            &args(&path, false, false),
            &global(8_388_608),
            b"#!/usr/bin/env bash\nset -euo pipefail\n",
        );

        assert_eq!(
            text(&outcome),
            format!(
                "── {} · created · 2 lines · 38 bytes · sha:{}\n── check: structure ok\n",
                path.display(),
                atomic::hash12(b"#!/usr/bin/env bash\nset -euo pipefail\n").as_str()
            )
        );
    }

    #[test]
    fn a_written_sh_file_with_a_syntax_error_still_creates_the_file_and_reports_non_ok() {
        let Some(dir) = repo() else { return };
        let path = dir.path().join("broken.sh");

        let outcome = decide(
            &args(&path, false, false),
            &global(8_388_608),
            b"if [ -z \"$x\" ]; then\n",
        );

        assert!(
            outcome.error.is_none(),
            "write never refuses on a check failure"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"if [ -z \"$x\" ]; then\n");
        let Body::Write(result) = &outcome.response.body else {
            panic!("expected Body::Write");
        };
        let check = result.check.as_ref().expect("a .sh file has a grammar");
        assert_eq!(check.layer, "structure");
        assert!(
            check.status != "ok",
            "an incomplete if-statement is a real error node"
        );
        assert!(
            text(&outcome).contains("── check: structure failed \u{b7} file kept\n"),
            "{}",
            text(&outcome)
        );
    }

    #[test]
    fn a_written_json_file_that_is_not_json_names_the_validator_and_says_the_file_was_kept() {
        let Some(dir) = repo() else { return };
        let path = dir.path().join("broken.json");

        let outcome = decide(
            &args(&path, false, false),
            &global(8_388_608),
            b"{\n  \"window\": 200\n",
        );

        assert!(outcome.error.is_none(), "write never refuses on a check");
        assert_eq!(std::fs::read(&path).unwrap(), b"{\n  \"window\": 200\n");
        assert!(
            text(&outcome).contains("── check: json invalid \u{b7} file kept\n"),
            "{}",
            text(&outcome)
        );
    }

    #[test]
    fn a_written_json_file_that_is_json_reports_json_ok_with_no_outcome_clause() {
        let Some(dir) = repo() else { return };
        let path = dir.path().join("ok.json");

        let outcome = decide(
            &args(&path, false, false),
            &global(8_388_608),
            b"{\n  \"window\": 200\n}\n",
        );

        assert!(
            text(&outcome).contains("── check: json ok\n"),
            "{}",
            text(&outcome)
        );
        assert!(!text(&outcome).contains("file kept"), "{}", text(&outcome));
    }

    #[test]
    fn a_file_with_no_grammar_is_created_and_names_the_skipped_check_in_the_footer() {
        let Some(dir) = repo() else { return };
        let path = dir.path().join("app.vue");

        let outcome = decide(
            &args(&path, false, false),
            &global(8_388_608),
            b"<template/>\n",
        );

        assert!(outcome.error.is_none());
        let Body::Write(result) = &outcome.response.body else {
            panic!("expected Body::Write");
        };
        assert!(result.check.is_none());
        assert!(text(&outcome).contains("check: skipped (no grammar for .vue)"));
    }

    #[test]
    fn no_check_skips_the_structural_check_entirely_with_no_skipped_omission_either() {
        let Some(dir) = repo() else { return };
        let path = dir.path().join("broken.sh");
        let mut opts = global(8_388_608);
        opts.no_check = true;

        let outcome = decide(
            &args(&path, false, false),
            &opts,
            b"if [ -z \"$x\" ]; then\n",
        );

        assert!(outcome.error.is_none());
        let Body::Write(result) = &outcome.response.body else {
            panic!("expected Body::Write");
        };
        assert!(result.check.is_none());
        assert!(outcome.response.omitted.is_empty());
    }

    #[test]
    fn writing_through_a_symlink_updates_the_referent_and_leaves_the_link_in_place() {
        let Some(dir) = repo() else { return };
        let referent = dir.path().join("real.sh");
        std::fs::write(&referent, b"old\n").unwrap();
        let link = dir.path().join("link.sh");
        std::os::unix::fs::symlink(&referent, &link).unwrap();

        let outcome = decide(&args(&link, true, false), &global(8_388_608), b"new\n");

        assert!(outcome.error.is_none());
        assert_eq!(std::fs::read(&referent).unwrap(), b"new\n");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn a_write_outside_the_working_tree_is_refused_and_creates_nothing() {
        let Some(_dir) = repo() else { return };
        let outside = TempDir::new().expect("a temp dir outside the repo");
        let path = outside.path().join("outside.txt");

        let outcome = decide(&args(&path, false, false), &global(8_388_608), b"new\n");

        assert_eq!(
            outcome.error.as_ref().expect("the refusal").slug(),
            "outside_tree"
        );
        assert!(!path.exists(), "nothing is created outside the tree");
    }

    #[test]
    fn a_write_outside_the_working_tree_lands_with_allow_outside() {
        let Some(_dir) = repo() else { return };
        let outside = TempDir::new().expect("a temp dir outside the repo");
        let path = outside.path().join("outside.txt");
        let mut opts = global(8_388_608);
        opts.allow_outside = true;

        let outcome = decide(&args(&path, false, false), &opts, b"new\n");

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(std::fs::read(&path).unwrap(), b"new\n");
    }

    #[test]
    fn count_lines_matches_str_lines_convention() {
        assert_eq!(count_lines(b""), 0);
        assert_eq!(count_lines(b"a"), 1);
        assert_eq!(count_lines(b"a\n"), 1);
        assert_eq!(count_lines(b"a\nb"), 2);
        assert_eq!(count_lines(b"a\nb\n"), 2);
    }

    #[test]
    fn too_large_is_none_exactly_at_the_limit() {
        assert!(too_large(Path::new("f"), 10, 10).is_none());
    }
}
