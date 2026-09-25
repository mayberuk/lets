//! The CLI flags and a `--from -` spec both build `EditSpec`s for `batch.rs`'s lock → validate →
//! write engine; nothing here searches, locks or writes on its own.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::check::{self, CommandVerdict};
use crate::cli::{EditArgs, Global};
use crate::error::{Candidate, CheckLayer, Error, ExpectReason, UnsupportedReason};
use crate::output::{
    Anchor, AnchorSide, Body, CheckResult, EditKind, EditResult, Format, Line, Marker, Omission,
    Region, Response, Sha12, ShaPair, plural_suffix,
};
use crate::{
    Outcome, atomic, batch, fs, grammars, lock, matcher, normalize, output, symbols, target, window,
};

const VERB: &str = "edit";

// Opus's edit echo had a median of 2,392 bytes against 92 for the Edit tool, and re-reads after
// an edit did not fall: trimmed context and a truncated span keep the verification without the
// bulk.
const CONTEXT_LINES: usize = 1;
const SPAN_TRUNCATE_LINES: usize = 6;
const SPAN_EDGE_LINES: usize = 2;

const BOM: &[u8] = b"\xef\xbb\xbf";

// The window `fs::read` sniffs. An edit reads its own bytes instead, because only the matched
// span has to be UTF-8.
const BINARY_SNIFF_WINDOW: usize = 8192;

#[derive(Debug)]
struct EditSpec {
    raw: String,
    file: PathBuf,
    kind: target::Kind,
    op: EditOp,
    if_sha: Option<Sha12>,
    normalize: bool,
    literal_newlines: bool,
}

#[derive(Debug)]
enum EditOp {
    Replace {
        old: Vec<u8>,
        new: Vec<u8>,
        all: bool,
    },
    ReplaceRange {
        expect: Option<String>,
        expect_all: Option<String>,
        new: Vec<u8>,
    },
    Insert {
        side: AnchorSide,
        anchor: String,
        new: Vec<u8>,
    },
}

/// `edits` pairs each replaced span with its replacement's length, which `check::layer1` compares.
struct Applied {
    after: Vec<u8>,
    edits: Vec<(matcher::Span, usize)>,
    kind: EditKind,
    match_kind: String,
    changed: Vec<(usize, usize)>,
    marker: Marker,
    normalized: bool,
    crlf: bool,
    span_guessed: bool,
}

/// The closing brace never matched, so the edit may have searched the wrong span.
const GUESSED_SPAN: &str = "span guessed (plaintext heuristic)";

pub fn run(args: &EditArgs, global: &Global, _format: Format) -> Outcome {
    match build_specs(args) {
        Ok(specs) => apply(&specs, args, global),
        Err(error) => Outcome::failed(VERB, error),
    }
}

fn apply(specs: &[EditSpec], args: &EditArgs, global: &Global) -> Outcome {
    let mut canonical = Vec::with_capacity(specs.len());
    for spec in specs {
        match fs::guard_scope(&spec.file, global.allow_outside) {
            Ok(path) => canonical.push(path),
            Err(error) => return Outcome::failed(VERB, error),
        }
    }

    let grouped = group(specs, &canonical);
    let several_files = grouped.len() > 1;
    let mut omitted = Vec::new();
    let mut stash = None;
    let mut failures: Vec<PathBuf> = Vec::new();
    let validated =
        batch::lock_and_validate(&grouped, &lock::runtime_dir(), |path, edits, _lock| {
            let mut own = Vec::new();
            let plan = validate(edits, path, global, &mut own, &mut stash)
                .inspect_err(|_| failures.push(path.to_path_buf()));
            merge_omissions(&mut omitted, own, path, several_files);
            plan
        });
    let locked = match validated {
        Ok(locked) => locked,
        Err(errors) => {
            let failure =
                Error::all(errors).expect("lock_and_validate returns Err only with an error");
            // A multi-edit failure names every file to fix in the footer, not one stashed span.
            let stash = stash.filter(|_| specs.len() == 1);
            return rejected(specs.len(), failure, &failures, stash, omitted);
        },
    };

    let checkers = match args.check.as_deref().filter(|_| !global.no_check) {
        None => Vec::new(),
        Some(value) => {
            let files: Vec<PathBuf> = locked.plans.iter().map(|plan| plan.path.clone()).collect();
            match Checker::resolve(value, &files, &mut omitted) {
                Ok(checkers) => checkers,
                Err(error) => return Outcome::failed(VERB, error),
            }
        },
    };
    let timeout = Duration::from_secs(args.check_timeout);
    // Layer 2: the baseline runs before any write, the verdict once after the whole batch, never
    // between two files of it.
    let baselines: Vec<CommandVerdict> = checkers
        .iter()
        .map(|checker| checker.run(timeout))
        .collect();

    let landed = match batch::write_in_order(&locked.plans) {
        Ok(landed) => landed,
        Err(Error::PartialBatch {
            written,
            failed,
            detail,
        }) => {
            let summary = format!(
                "{} of {} file{} written \u{b7} {} failed",
                written.len(),
                locked.plans.len(),
                plural_suffix(locked.plans.len()),
                failed.display()
            );
            return partial_batch(summary, written, failed, detail, omitted);
        },
        Err(error) => return Outcome::failed(VERB, error),
    };

    let mut command_check = match settle_all(&checkers, &baselines, timeout, &mut omitted) {
        Ok(command_check) => command_check,
        Err((label, excerpt)) => {
            return reverted(&locked.plans, &landed, &label, &excerpt, omitted);
        },
    };

    let mut results: Vec<EditResult> = locked
        .plans
        .into_iter()
        .flat_map(|plan| plan.detail)
        .collect();
    bound_output(&mut results, global.max_bytes, &mut omitted);
    let mut response = Response::empty(VERB);
    // A lone result's footer line is the response's, so layer 2 replaces layer 1 there; a batch
    // carries it in the summary.
    if results.len() == 1 && command_check.is_some() {
        results[0].check = command_check.take();
    }
    if results.len() != 1 {
        response.footer.summary = summary(&results, command_check.as_ref());
    }
    fill_stats(&mut response, &results);
    response.omitted = omitted;
    response.body = Body::Edit(results);
    Outcome::ok(response)
}

/// `command` is what the footer names, a literal or a preset's template, never the absolute paths
/// it runs with. `place` is set only when a preset split the batch into several manifest groups.
struct Checker {
    command: String,
    dir: PathBuf,
    files: Vec<PathBuf>,
    place: Option<String>,
}

impl Checker {
    fn resolve(
        value: &str,
        files: &[PathBuf],
        omitted: &mut Vec<Omission>,
    ) -> Result<Vec<Checker>, Error> {
        if !value.starts_with('@') {
            return Ok(vec![Checker {
                command: value.to_owned(),
                dir: PathBuf::from("."),
                files: files.to_vec(),
                place: None,
            }]);
        }
        // A preset runs in its manifest's directory, where a path as typed does not resolve.
        let canonical: Vec<PathBuf> = files
            .iter()
            .map(|file| std::fs::canonicalize(file).unwrap_or_else(|_| file.clone()))
            .collect();
        let resolved = check::preset_groups(value, &canonical, &fs::tree_root())?;
        if resolved.groups.is_empty() {
            omitted.push(Omission::CheckSkipped {
                reason: resolved.reason,
            });
            return Ok(Vec::new());
        }
        for file in &resolved.unmatched {
            let typed = canonical
                .iter()
                .position(|seen| seen == file)
                .map_or(file, |index| &files[index]);
            omitted.push(Omission::CheckSkipped {
                reason: format!("{}: {}", typed.display(), resolved.reason),
            });
        }
        let several = resolved.groups.len() > 1;
        Ok(resolved
            .groups
            .into_iter()
            .map(|(preset, files)| Checker {
                place: several.then(|| place(&preset.dir)),
                command: preset.command,
                dir: preset.dir,
                files,
            })
            .collect())
    }

    fn run(&self, timeout: Duration) -> CommandVerdict {
        check::run_command_in(&self.command, &self.files, timeout, &self.dir)
    }

    fn status(&self, verdict: &str) -> String {
        match &self.place {
            Some(place) => format!("{verdict} in {place}"),
            None => verdict.to_owned(),
        }
    }

    fn placed(&self, reason: String) -> String {
        match &self.place {
            Some(place) => format!("{place}: {reason}"),
            None => reason,
        }
    }

    fn label(&self) -> String {
        match &self.place {
            Some(place) => format!("`{}` in {place}", self.command),
            None => format!("`{}`", self.command),
        }
    }
}

/// A trailing `/` so `./` reads as a directory; stdout never carries an absolute path.
fn place(dir: &Path) -> String {
    let cwd = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .unwrap_or_default();
    let common = cwd
        .ancestors()
        .find(|ancestor| dir.starts_with(ancestor))
        .unwrap_or(Path::new(""));
    let mut relative = PathBuf::new();
    for _ in cwd.strip_prefix(common).unwrap_or(&cwd).components() {
        relative.push("..");
    }
    relative.push(dir.strip_prefix(common).unwrap_or(dir));
    if relative.as_os_str().is_empty() {
        "./".to_owned()
    } else {
        format!("{}/", relative.display())
    }
}

/// Returns at the first new failure, before any omission is recorded, since the caller reverts.
fn settle_all(
    checkers: &[Checker],
    baselines: &[CommandVerdict],
    timeout: Duration,
    omitted: &mut Vec<Omission>,
) -> Result<Option<CheckResult>, (String, String)> {
    let mut verdicts = Vec::with_capacity(checkers.len());
    for (checker, base) in checkers.iter().zip(baselines) {
        let verdict = settle(checker, timeout, base);
        // Validation-atomic: one group's new failure reverts every file, even ones that passed.
        if let CommandVerdict::Failed(excerpt) = &verdict {
            return Err((checker.label(), excerpt.clone()));
        }
        verdicts.push(verdict);
    }
    let mut command_check: Option<CheckResult> = None;
    for (checker, verdict) in checkers.iter().zip(verdicts) {
        let cmd = checker.command.as_str();
        match verdict {
            CommandVerdict::Ok => match &mut command_check {
                None => {
                    command_check = Some(CheckResult {
                        layer: cmd.to_owned(),
                        status: checker.status("ok"),
                        errors_before: 0,
                        errors_after: 0,
                    });
                },
                Some(ok) if ok.layer == cmd => {
                    write!(ok.status, ", {}", checker.status("ok")).unwrap();
                },
                Some(ok) => write!(ok.status, ", {cmd} {}", checker.status("ok")).unwrap(),
            },
            CommandVerdict::Failed(_) => {},
            CommandVerdict::Inconclusive(reason) => omitted.push(Omission::CheckInconclusive {
                layer: cmd.to_owned(),
                reason: checker.placed(reason),
            }),
            CommandVerdict::Skipped(reason) => omitted.push(Omission::CheckSkipped {
                reason: checker.placed(reason),
            }),
        }
    }
    Ok(command_check)
}

fn settle(checker: &Checker, timeout: Duration, baseline: &CommandVerdict) -> CommandVerdict {
    match baseline {
        // No baseline to compare against, so the post-write run is not made.
        CommandVerdict::Skipped(reason) => CommandVerdict::Skipped(reason.clone()),
        CommandVerdict::Inconclusive(reason) => CommandVerdict::Inconclusive(reason.clone()),
        _ => checker.run(timeout).against_baseline(baseline),
    }
}

/// Stdout still owes the footer naming which files landed, though no edit is rendered.
fn partial_batch(
    summary: String,
    written: Vec<PathBuf>,
    failed: PathBuf,
    detail: String,
    mut omitted: Vec<Omission>,
) -> Outcome {
    let mut response = Response::empty(VERB);
    response.footer.summary = summary;
    omitted.push(Omission::PartialBatch {
        written: written.clone(),
    });
    response.omitted = omitted;
    response.body = Body::Edit(Vec::new());
    Outcome::partial(response, Error::PartialBatch {
        written,
        failed,
        detail,
    })
}

fn reverted(
    plans: &[batch::Plan<Vec<EditResult>>],
    landed: &[PathBuf],
    checker: &str,
    excerpt: &str,
    omitted: Vec<Omission>,
) -> Outcome {
    let reverted = batch::revert(plans, landed);
    let first = landed.first().cloned().unwrap_or_default();
    if reverted.not_restored.is_empty() {
        // The checker's first line is status, not content, so no budget trims it.
        let said = if excerpt.is_empty() {
            String::new()
        } else {
            format!(": {excerpt}")
        };
        return Outcome::failed(VERB, Error::CheckFailed {
            path: first,
            layer: CheckLayer::Command,
            detail: format!(
                "{checker} passed before the batch and failed after it{said} \u{b7} {} file{} \
                 reverted",
                landed.len(),
                plural_suffix(landed.len())
            ),
        });
    }
    // Only files still holding the new bytes count as written; restored ones are named beside.
    let written: Vec<PathBuf> = reverted
        .not_restored
        .iter()
        .map(|(path, _)| path.clone())
        .collect();
    let mut summary = format!(
        "{checker} failed after {} file{} landed",
        landed.len(),
        plural_suffix(landed.len())
    );
    if !reverted.restored.is_empty() {
        write!(
            summary,
            " \u{b7} restored {}",
            path_list(&reverted.restored)
        )
        .unwrap();
    }
    write!(summary, " \u{b7} not restored {}", path_list(&written)).unwrap();
    let reasons: Vec<String> = reverted
        .not_restored
        .iter()
        .map(|(path, kind)| format!("{} ({kind})", path.display()))
        .collect();
    partial_batch(
        summary,
        written.clone(),
        written[0].clone(),
        format!(
            "{checker} failed after the batch landed and the revert could not restore {}",
            reasons.join(", ")
        ),
        omitted,
    )
}

fn path_list(paths: &[PathBuf]) -> String {
    let names: Vec<String> = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    names.join(", ")
}

fn rejected(
    total: usize,
    failure: Error,
    failed: &[PathBuf],
    stash: Option<EditResult>,
    omitted: Vec<Omission>,
) -> Outcome {
    let mut response = Response::empty(VERB);
    response.omitted = omitted;
    if let Some(result) = stash {
        fill_stats(&mut response, std::slice::from_ref(&result));
        response.body = Body::Edit(vec![result]);
        return Outcome::partial(response, failure);
    }
    if total == 1 {
        return Outcome::failed(VERB, failure);
    }
    // Every file to fix is named, since fixing only the first and re-running fails on the next.
    response.footer.summary = format!(
        "0 of {total} applied \u{b7} nothing written \u{b7} fix {} and re-run",
        if failed.is_empty() {
            "the failing edit".to_owned()
        } else {
            path_list(failed)
        }
    );
    response.body = Body::Edit(Vec::new());
    Outcome::partial(response, failure)
}

fn fill_stats(response: &mut Response, results: &[EditResult]) {
    response.stats.lines = results
        .iter()
        .map(|result| {
            result
                .region
                .as_ref()
                .map_or(0, |region| region.lines.len())
        })
        .sum();
    let mut header = String::new();
    response.stats.bytes = results
        .iter()
        .map(|result| {
            header.clear();
            output::push_edit_header(&mut header, result);
            header.len()
                + result
                    .region
                    .as_ref()
                    .map_or(0, |region| window::content_bytes(&region.lines))
        })
        .sum();
}

/// With several files a skip line names its file, since `no grammar for .txt` reads the same for
/// each, and one file's repeats collapse to one line.
fn merge_omissions(omitted: &mut Vec<Omission>, own: Vec<Omission>, path: &Path, several: bool) {
    for omission in own {
        match omission {
            Omission::CheckSkipped { reason } if several => {
                let reason = format!("{}: {reason}", path.display());
                let seen = omitted.iter().any(
                    |seen| matches!(seen, Omission::CheckSkipped { reason: had } if *had == reason),
                );
                if !seen {
                    omitted.push(Omission::CheckSkipped { reason });
                }
            },
            Omission::CrlfMatched
                if omitted
                    .iter()
                    .any(|seen| matches!(seen, Omission::CrlfMatched)) => {},
            Omission::RegionGap {
                not_shown,
                path: gap_path,
            } if several && gap_path.is_none() => {
                omitted.push(Omission::RegionGap {
                    not_shown,
                    path: Some(path.to_path_buf()),
                });
            },
            other => omitted.push(other),
        }
    }
}

fn bound_output(results: &mut [EditResult], limit: usize, omitted: &mut Vec<Omission>) {
    let cut: usize = results
        .iter_mut()
        .filter_map(|result| result.region.as_mut())
        .map(|region| window::cut_long_lines(&mut region.lines, &[]))
        .sum();
    if cut > 0 {
        omitted.push(Omission::LongLinesCut { lines: cut });
    }
    let mut bytes: usize = results
        .iter()
        .filter_map(|result| result.region.as_ref())
        .map(|region| window::content_bytes(&region.lines))
        .sum();
    if bytes <= limit {
        return;
    }
    let mut dropped = 0;
    for result in results.iter_mut().rev() {
        let Some(region) = result.region.as_mut() else {
            continue;
        };
        while bytes > limit {
            let Some(line) = region.lines.pop() else {
                break;
            };
            bytes -= line.text.len() + 1;
            dropped += usize::from(line.number > 0);
        }
        while region
            .lines
            .last()
            .is_some_and(|line| line.marker == Marker::Gap)
        {
            region.lines.pop();
        }
        match region.lines.last() {
            Some(last) => region.end = last.number,
            None => result.region = None,
        }
        if bytes <= limit {
            break;
        }
    }
    omitted.push(Omission::OutputTrimmed {
        limit,
        lines: dropped,
    });
}

fn summary(results: &[EditResult], command: Option<&CheckResult>) -> String {
    let files: BTreeSet<&Path> = results.iter().map(|result| result.path.as_path()).collect();
    let mut out = format!(
        "{} file{} \u{b7} {} edit{} \u{b7} all applied",
        files.len(),
        plural_suffix(files.len()),
        results.len(),
        plural_suffix(results.len())
    );
    // A `Vec`, not a map, keeps the footer in first-seen order.
    let mut counts: Vec<(String, usize)> = Vec::new();
    for result in results {
        let Some(check) = &result.check else { continue };
        let label = format!("{} {}", check.layer, check.status);
        match counts.iter_mut().find(|(seen, _)| *seen == label) {
            Some((_, n)) => *n += 1,
            None => counts.push((label, 1)),
        }
    }
    for (i, (label, n)) in counts.iter().enumerate() {
        out.push_str(if i == 0 {
            " \u{b7} checks: "
        } else {
            " \u{b7} "
        });
        write!(out, "{label} \u{d7}{n}").unwrap();
    }
    if let Some(check) = command {
        write!(out, " \u{b7} check: {} {}", check.layer, check.status).unwrap();
    }
    out
}

/// Keyed by the target as typed, which is what the output names; `canonical` decides sameness.
fn group<'a>(specs: &'a [EditSpec], canonical: &[PathBuf]) -> Vec<(PathBuf, Vec<&'a EditSpec>)> {
    let mut keys: Vec<&Path> = Vec::new();
    let mut grouped: Vec<(PathBuf, Vec<&'a EditSpec>)> = Vec::new();
    for (spec, key) in specs.iter().zip(canonical) {
        if let Some(index) = keys.iter().position(|seen| *seen == key.as_path()) {
            grouped[index].1.push(spec);
        } else {
            keys.push(key.as_path());
            grouped.push((spec.file.clone(), vec![spec]));
        }
    }
    grouped
}

/// `stash` carries the layer-1 failure's `EditResult` out: `batch.rs` sees only `Vec<Error>`.
fn validate(
    specs: &[&EditSpec],
    path: &Path,
    global: &Global,
    omitted: &mut Vec<Omission>,
    stash: &mut Option<EditResult>,
) -> Result<batch::Plan<Vec<EditResult>>, Error> {
    let mut folded: Option<batch::Plan<Vec<EditResult>>> = None;
    for spec in specs {
        // Each edit matches the bytes the previous one produced. One plan per file carries the
        // whole fold: two plans for one path would write the second over the first.
        let carried = folded.as_ref().map(|plan| plan.after.as_slice());
        let step = one(spec, path, global, carried, omitted, stash)?;
        match &mut folded {
            Some(plan) => {
                plan.after = step.after;
                plan.detail.push(step.detail);
            },
            None => {
                folded = Some(batch::Plan {
                    path: step.path,
                    before: step.before,
                    after: step.after,
                    mode: step.mode,
                    detail: vec![step.detail],
                });
            },
        }
    }
    folded.ok_or_else(|| not_found(&path.display().to_string(), "an edit".to_owned()))
}

fn one(
    spec: &EditSpec,
    path: &Path,
    global: &Global,
    carried: Option<&[u8]>,
    omitted: &mut Vec<Omission>,
    stash: &mut Option<EditResult>,
) -> Result<batch::Plan<EditResult>, Error> {
    let (before, applied) = prepare(spec, path, global, carried)?;

    let checked = if global.no_check {
        omitted.push(Omission::CheckSkipped {
            reason: "--no-check".to_owned(),
        });
        None
    } else if let Some(kind) = check::checker_for(path, &before, edited_start(&applied)) {
        Some(check::layer1(kind, &before, &applied.after, &applied.edits))
    } else {
        omitted.push(Omission::CheckSkipped {
            reason: no_grammar_reason(path),
        });
        None
    };
    if applied.normalized {
        omitted.push(Omission::Normalized);
    }
    if applied.crlf {
        omitted.push(Omission::CrlfMatched);
    }

    let failed = checked.as_ref().is_some_and(|layer1| !layer1.ok);
    let annotation = checked.as_ref().filter(|layer1| !layer1.ok).map(annotation);
    let before_sha = atomic::hash12(&before);
    let after_sha = if failed {
        before_sha.clone()
    } else {
        atomic::hash12(&applied.after)
    };
    let result = EditResult {
        path: path.to_path_buf(),
        lines: applied
            .changed
            .iter()
            .flat_map(|(first, last)| *first..=*last)
            .collect(),
        match_kind: if failed {
            String::new()
        } else {
            applied.match_kind.clone()
        },
        resolver: applied.span_guessed.then_some(GUESSED_SPAN),
        region: region(
            &applied.after,
            &applied.changed,
            applied.marker,
            annotation.as_deref(),
            failed,
            omitted,
        ),
        check: checked.as_ref().map(|layer1| CheckResult {
            layer: if failed {
                String::new()
            } else {
                layer1.label.to_owned()
            },
            status: if failed {
                format!("{} \u{2192} reverted", layer1.status)
            } else {
                layer1.status.clone()
            },
            errors_before: layer1.errors_before,
            errors_after: layer1.errors_after,
        }),
        sha: ShaPair {
            before: before_sha,
            after: after_sha,
        },
        reverted: failed,
        kind: applied.kind,
    };

    if let Some(layer1) = checked.as_ref().filter(|layer1| !layer1.ok) {
        *stash = Some(result);
        // Layer 1 runs on in-memory bytes, so "reverted" here means nothing was ever written.
        return Err(Error::CheckFailed {
            path: path.to_path_buf(),
            layer: layer_of(layer1.label),
            detail: layer1.status.clone(),
        });
    }
    Ok(batch::Plan {
        path: path.to_path_buf(),
        before,
        after: applied.after,
        mode: None,
        detail: result,
    })
}

/// The frontmatter side is decided by where the edit starts, and `--all` starts at its first match.
fn edited_start(applied: &Applied) -> usize {
    applied.edits.first().map_or(0, |(span, _)| span.start)
}

fn prepare(
    spec: &EditSpec,
    path: &Path,
    global: &Global,
    carried: Option<&[u8]>,
) -> Result<(Vec<u8>, Applied), Error> {
    atomic::check_before_write(path, global.max_file_bytes)?;
    atomic::refuse_unwritable(path)?;
    // `--if` names the file's own hash, so it is verified against disk even after an earlier edit
    // in this batch folded into `carried`.
    let read = if let Some(folded) = carried {
        if let Some(sha) = &spec.if_sha {
            atomic::verify_if(path, sha)?;
        }
        Ok(folded.to_vec())
    } else if let Some(sha) = &spec.if_sha {
        // `verify_if` returns the bytes it hashed under the lock; the splice runs on exactly those.
        atomic::verify_if(path, sha)
    } else {
        std::fs::read(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })
    };
    let before = read.map_err(|error| match (&error, target::meant(&spec.raw)) {
        (Error::Io { source, .. }, Some(meant))
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Error::MistypedTarget {
                target: spec.raw.clone(),
                meant,
            }
        },
        _ => error,
    })?;
    if before[..before.len().min(BINARY_SNIFF_WINDOW)].contains(&0) {
        return Err(Error::Unsupported {
            path: path.to_path_buf(),
            reason: UnsupportedReason::Binary,
        });
    }

    let starts = line_starts(&before);
    let applied = match &spec.op {
        EditOp::Replace { old, new, all } => replace(spec, path, &before, &starts, old, new, *all),
        EditOp::ReplaceRange {
            expect,
            expect_all,
            new,
        } => replace_range(
            spec,
            &before,
            &starts,
            expect.as_deref(),
            expect_all.as_deref(),
            new,
        ),
        EditOp::Insert { side, anchor, new } => insert(spec, &before, &starts, *side, anchor, new),
    };
    let applied = applied.map_err(|error| match error {
        Error::NotFound { .. } if carried.is_none() && before.is_empty() => Error::EmptyFile {
            path: path.to_path_buf(),
        },
        other => other,
    })?;
    Ok((before, applied))
}

/// tree-sitter carries no diagnostic text, so this names the broken promise, not a parser message.
fn annotation(layer1: &check::Layer1) -> String {
    if layer1.label == "structure" {
        "\u{2190} parse error".to_owned()
    } else {
        format!("\u{2190} invalid {}", layer1.label)
    }
}

fn layer_of(label: &str) -> CheckLayer {
    if label == "structure" {
        CheckLayer::Structure
    } else {
        CheckLayer::Structured
    }
}

fn no_grammar_reason(path: &Path) -> String {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) => format!("no grammar for .{ext}"),
        None => format!("no grammar for {}", path.display()),
    }
}

fn replace(
    spec: &EditSpec,
    path: &Path,
    before: &[u8],
    starts: &[usize],
    old: &[u8],
    new: &[u8],
    all: bool,
) -> Result<Applied, Error> {
    let (scope, span_guessed) = scope_span(spec, before, starts)?;
    let shift = matcher::line_of(before, scope.start) - 1;
    let endings = Endings::of(before);
    let bare_lf_needle = has_bare_lf(old) && !spec.literal_newlines;
    // `show` hides `\r`, so an agent types LF. Only the needle is translated, never the splice, and
    // only on a file with no bare LF, where it cannot pick the wrong bytes.
    let crlf = bare_lf_needle && endings == Endings::Crlf;
    let translated;
    let needle = if crlf {
        translated = crlf_needle(old);
        translated.as_slice()
    } else {
        old
    };
    let found = matcher::find(
        path,
        &before[scope.start..scope.end],
        needle,
        matcher::FindOptions {
            all,
            normalize: spec.normalize,
        },
    );
    if bare_lf_needle
        && endings == Endings::Mixed
        && matches!(found, matcher::Found::NotFound { .. })
    {
        return Err(Error::MixedEndings {
            path: path.to_path_buf(),
        });
    }
    let (spans, normalized) = match found {
        matcher::Found::Unique(located) => (vec![located.span], located.normalized),
        matcher::Found::All(located) => (
            located.iter().map(|one| one.span).collect(),
            located.iter().any(|one| one.normalized),
        ),
        matcher::Found::Ambiguous(candidates) => {
            return Err(Error::Ambiguous {
                target: spec.raw.clone(),
                candidates: candidates.into_iter().map(|c| shifted(c, shift)).collect(),
            });
        },
        matcher::Found::NotFound {
            nearest,
            normalized_hint,
        } => {
            // A normalized match already names the fix; an indent hint would be a weaker second
            // one.
            let indent = normalized_hint
                .is_none()
                .then(|| indent_hint(before, scope, old))
                .flatten();
            return Err(Error::NotFound {
                target: spec.raw.clone(),
                // Both hints ride in `what`: `NotFound` is sized against `result_large_err`.
                what: match (normalized_hint, indent) {
                    (Some(hint), _) => format!(
                        "--old (a normalized match exists at line {} \u{2014} pass --normalize)",
                        matcher::line_of(before, scope.start + hint.start)
                    ),
                    (None, Some(hint)) => format!("--old ({hint})"),
                    (None, None) => "--old".to_owned(),
                },
                nearest: nearest.map(|near| shifted(near.candidate, shift)),
            });
        },
    };
    let spans: Vec<matcher::Span> = spans
        .iter()
        .map(|span| matcher::Span {
            start: span.start + scope.start,
            end: span.end + scope.start,
        })
        .collect();

    let replacement = joined(new, before, spec.literal_newlines);
    let after = splice_all(before, &spans, &replacement);

    let mut changed = Vec::with_capacity(spans.len());
    let (mut grown_new, mut grown_old) = (0usize, 0usize);
    for span in &spans {
        // Spans are disjoint and ascending, so earlier splices shift this start by their growth.
        let start = span.start + grown_new - grown_old;
        changed.push(span_lines(&after, start, replacement.len()));
        grown_new += replacement.len();
        grown_old += span.end - span.start;
    }
    Ok(Applied {
        after,
        // Not their union: it would swallow what lies between two `--all` matches, and layer 1
        // reads an error inside the edited span as this edit's own.
        edits: spans
            .iter()
            .map(|span| (*span, replacement.len()))
            .collect(),
        kind: EditKind::Replaced { count: spans.len() },
        match_kind: match_kind(normalized, &before[spans[0].start..spans[0].end], needle),
        changed,
        marker: Marker::Replaced,
        normalized,
        crlf,
        span_guessed,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Endings {
    None,
    Lf,
    Crlf,
    Mixed,
}

impl Endings {
    fn of(bytes: &[u8]) -> Endings {
        let (mut crlf, mut lf) = (false, false);
        for (index, _) in bytes.iter().enumerate().filter(|(_, byte)| **byte == b'\n') {
            if index > 0 && bytes[index - 1] == b'\r' {
                crlf = true;
            } else {
                lf = true;
            }
        }
        match (crlf, lf) {
            (false, false) => Endings::None,
            (false, true) => Endings::Lf,
            (true, false) => Endings::Crlf,
            (true, true) => Endings::Mixed,
        }
    }
}

fn has_bare_lf(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .enumerate()
        .any(|(index, byte)| *byte == b'\n' && (index == 0 || bytes[index - 1] != b'\r'))
}

fn crlf_needle(old: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(old.len() + old.len() / 8);
    for (index, byte) in old.iter().enumerate() {
        if *byte == b'\n' && (index == 0 || old[index - 1] != b'\r') {
            out.push(b'\r');
        }
        out.push(*byte);
    }
    out
}

/// `Some` only when every line of `old` matches consecutive `scope` lines with leading whitespace
/// ignored, and at least one line's whitespace differs.
fn indent_hint(before: &[u8], scope: matcher::Span, old: &[u8]) -> Option<String> {
    let hay = &before[scope.start..scope.end];
    let hay_starts = line_starts(hay);
    let hay_text = std::str::from_utf8(hay).ok()?;
    let needle_text = std::str::from_utf8(old).ok()?;
    let needle_lines: Vec<&str> = needle_text
        .split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .collect();
    let count = needle_lines.len();
    if hay_starts.len() < count {
        return None;
    }
    let file_line = |window: usize, offset: usize| -> &str {
        let start = hay_starts[window + offset];
        let end = hay_starts
            .get(window + offset + 1)
            .copied()
            .unwrap_or(hay_text.len());
        hay_text[start..end].trim_end_matches(['\n', '\r'])
    };
    for (window, &window_start) in hay_starts
        .iter()
        .enumerate()
        .take(hay_starts.len() - count + 1)
    {
        let mut diffs: Vec<(usize, &str, &str)> = Vec::new();
        let all_rest_match = (0..count).all(|offset| {
            let (file_ws, file_rest) = split_indent(file_line(window, offset));
            let (needle_ws, needle_rest) = split_indent(needle_lines[offset]);
            if file_rest != needle_rest {
                return false;
            }
            if file_ws != needle_ws {
                let line = matcher::line_of(before, scope.start + hay_starts[window + offset]);
                diffs.push((line, file_ws, needle_ws));
            }
            true
        });
        if !all_rest_match || diffs.is_empty() {
            continue;
        }
        if count == 1 {
            let (line, file_ws, needle_ws) = diffs[0];
            return Some(format!(
                "matches line {line} except leading whitespace: {}",
                indent_phrase(file_ws, needle_ws)
            ));
        }
        let first = matcher::line_of(before, scope.start + window_start);
        let last = matcher::line_of(before, scope.start + hay_starts[window + count - 1]);
        return Some(indent_hint_lines(first, last, &diffs));
    }
    None
}

/// Far below `error::CANDIDATE_CAP`: the hint sits inline in a one-line `NotFound.what`.
const INDENT_HINT_LINES: usize = 5;

fn indent_hint_lines(first: usize, last: usize, diffs: &[(usize, &str, &str)]) -> String {
    let mut out = format!("matches lines {first}-{last} except leading whitespace: ");
    for (i, (line, file_ws, needle_ws)) in diffs.iter().take(INDENT_HINT_LINES).enumerate() {
        if i > 0 {
            out.push_str("; ");
        }
        let _ = write!(out, "line {line} {}", indent_phrase(file_ws, needle_ws));
    }
    if let Some(more) = diffs
        .len()
        .checked_sub(INDENT_HINT_LINES)
        .filter(|n| *n > 0)
    {
        let _ = write!(out, " (+{more} more)");
    }
    out
}

fn split_indent(line: &str) -> (&str, &str) {
    let rest = line.trim_start_matches([' ', '\t']);
    (&line[..line.len() - rest.len()], rest)
}

enum Indent {
    None,
    Spaces(usize),
    Tabs(usize),
    Mixed(usize),
}

fn classify_indent(ws: &str) -> Indent {
    if ws.is_empty() {
        Indent::None
    } else if ws.bytes().all(|b| b == b' ') {
        Indent::Spaces(ws.len())
    } else if ws.bytes().all(|b| b == b'\t') {
        Indent::Tabs(ws.len())
    } else {
        Indent::Mixed(ws.chars().count())
    }
}

fn indent_words(indent: &Indent) -> String {
    match indent {
        Indent::None => "no indentation".to_owned(),
        Indent::Spaces(n) => format!("{n} space{}", plural_suffix(*n)),
        Indent::Tabs(n) => format!("{n} tab{}", plural_suffix(*n)),
        Indent::Mixed(n) => format!("{n} mixed-whitespace char{}", plural_suffix(*n)),
    }
}

fn indent_phrase(file_ws: &str, needle_ws: &str) -> String {
    let file_kind = classify_indent(file_ws);
    let needle_kind = classify_indent(needle_ws);
    let needle_words = match (&file_kind, &needle_kind) {
        (Indent::Spaces(_), Indent::Spaces(n)) | (Indent::Tabs(_), Indent::Tabs(n)) => {
            n.to_string()
        },
        _ => indent_words(&needle_kind),
    };
    format!(
        "the file indents it {}, --old has {needle_words}",
        indent_words(&file_kind)
    )
}

fn match_kind(normalized: bool, matched: &[u8], needle: &[u8]) -> String {
    if !normalized {
        return "exact".to_owned();
    }
    let pair = std::str::from_utf8(matched)
        .ok()
        .zip(std::str::from_utf8(needle).ok())
        .and_then(|(matched, needle)| {
            matched
                .chars()
                .zip(needle.chars())
                .find(|(had, typed)| had != typed)
                .map(|(had, _)| (had, normalize::fold_char(had)))
        });
    match pair {
        Some((had, folded)) => format!("normalized ({had} \u{2192} {folded})"),
        None => "normalized".to_owned(),
    }
}

fn replace_range(
    spec: &EditSpec,
    before: &[u8],
    starts: &[usize],
    expect: Option<&str>,
    expect_all: Option<&str>,
    new: &[u8],
) -> Result<Applied, Error> {
    let (first, last) = match &spec.kind {
        target::Kind::Line(line) => (*line, *line),
        target::Kind::Range(from, to) => (*from, *to),
        _ => {
            return Err(not_found(
                &spec.raw,
                "a :line or :a-b target for --expect".to_owned(),
            ));
        },
    };
    if first == 0 || last > starts.len() {
        return Err(not_found(&spec.raw, format!(":{first}-{last}")));
    }
    // A matching first line says nothing about a later one another process may have changed.
    if last > first && expect.is_some() && expect_all.is_none() && spec.if_sha.is_none() {
        return Err(Error::ExpectRefused {
            target: spec.raw.clone(),
            reason: ExpectReason::Range { first, last },
        });
    }
    if let Some(expect) = expect {
        let actual = line_text(before, starts, first);
        if actual.trim() != expect.trim() {
            return Err(Error::ExpectRefused {
                target: spec.raw.clone(),
                reason: ExpectReason::Mismatch {
                    line: first,
                    actual,
                },
            });
        }
    }
    let span = line_span(before, starts, first, last);
    if let Some(whole) = expect_all {
        let actual = String::from_utf8_lossy(&before[span.start..span.end]);
        if actual.trim_end_matches(['\r', '\n']) != whole.trim_end_matches(['\r', '\n']) {
            return Err(Error::Ambiguous {
                target: spec.raw.clone(),
                candidates: candidates_for(spec, before, starts, first, last),
            });
        }
    }

    let mut replacement = joined(new, before, spec.literal_newlines);
    if span.end > span.start && before[span.end - 1] == b'\n' {
        let ending = dominant_ending(before);
        if !replacement.ends_with(b"\n") {
            replacement.extend_from_slice(ending);
        }
    }
    let after = matcher::splice(before, span, &replacement);
    let mut match_kind = String::new();
    if expect.is_some() {
        match_kind.push_str("expect matched");
    }
    if expect_all.is_some() {
        match_kind.push_str(if match_kind.is_empty() {
            "expect-all matched"
        } else {
            " \u{b7} expect-all matched"
        });
    }
    if spec.if_sha.is_some() {
        match_kind.push_str(if match_kind.is_empty() {
            "sha matched"
        } else {
            " \u{b7} sha matched"
        });
    }
    Ok(Applied {
        edits: vec![(span, replacement.len())],
        kind: EditKind::Replaced { count: 1 },
        match_kind,
        changed: vec![span_lines(&after, span.start, replacement.len())],
        marker: Marker::Replaced,
        normalized: false,
        crlf: false,
        after,
        span_guessed: false,
    })
}

fn insert(
    spec: &EditSpec,
    before: &[u8],
    starts: &[usize],
    side: AnchorSide,
    anchor: &str,
    new: &[u8],
) -> Result<Applied, Error> {
    let (line, anchor) = anchor_line(spec, before, side, anchor, new)?;
    // The insert supplies the terminator, so a `new` that ends its own line gets no extra blank.
    let new = new
        .strip_suffix(b"\n")
        .map_or(new, |rest| rest.strip_suffix(b"\r").unwrap_or(rest));
    let text = joined(new, before, spec.literal_newlines);
    let lines = text.split(|&byte| byte == b'\n').count();
    let ending = dominant_ending(before);
    // `text_at` is where the new text lands: the marked line is the one typed, never the anchor.
    let (at, replacement, text_at) = match side {
        AnchorSide::After => {
            let at = starts.get(line).copied().unwrap_or(before.len());
            if at == before.len() && !before.ends_with(b"\n") {
                // The anchor ends a file with no final newline; separator first keeps it that way.
                (at, [ending, &text[..]].concat(), at + ending.len())
            } else {
                (at, [&text[..], ending].concat(), at)
            }
        },
        AnchorSide::Before => {
            let mut at = starts
                .get(line - 1)
                .copied()
                .ok_or_else(|| not_found(&spec.raw, format!(":{line}")))?;
            // After the BOM, which encoding sniffers expect at byte 0.
            if at == 0 && before.starts_with(BOM) {
                at = BOM.len();
            }
            (at, [&text[..], ending].concat(), at)
        },
    };
    let span = matcher::Span { start: at, end: at };
    let after = matcher::splice(before, span, &replacement);
    let first = matcher::line_of(&after, text_at);
    Ok(Applied {
        after,
        edits: vec![(span, replacement.len())],
        kind: EditKind::Inserted { lines, anchor },
        match_kind: String::new(),
        changed: vec![(first, first + lines - 1)],
        marker: Marker::Added,
        normalized: false,
        crlf: false,
        span_guessed: false,
    })
}

/// A shell eats the inner quotes of `@'…'`, so an `@` anchor that did not parse as a regex is
/// re-spelled with them.
fn anchor_target(file: &Path, anchor: &str) -> target::Target {
    let parsed = target::parse(&format!("{}{anchor}", file.display()));
    match (&parsed.kind, anchor.strip_prefix('@')) {
        (target::Kind::Whole, Some(pattern)) => {
            target::parse(&format!("{}@'{pattern}'", file.display()))
        },
        _ => parsed,
    }
}

fn anchor_line(
    spec: &EditSpec,
    before: &[u8],
    side: AnchorSide,
    anchor: &str,
    new: &[u8],
) -> Result<(usize, Anchor), Error> {
    let parsed = anchor_target(&spec.file, anchor);
    let content = utf8(before, &spec.file)?;
    match &parsed.kind {
        target::Kind::Regex {
            pattern,
            occurrence,
        } => {
            let found = target::find_regex(content, pattern, *occurrence)
                .map_err(|_| not_found(&spec.raw, format!("{anchor} (invalid regex)")))?
                .ok_or_else(|| not_found(&spec.raw, anchor.to_owned()))?;
            Ok((found.line, Anchor {
                side,
                at: format!("line {}", found.line),
                line: None,
            }))
        },
        target::Kind::Symbol(segments) => {
            let (found, hint) = resolve_symbol(&spec.file, content, segments);
            match found.as_slice() {
                [] => Err(not_found(&spec.raw, missing(anchor, hint.as_ref()))),
                [one] => {
                    if side == AnchorSide::After && one.end_guessed {
                        return Err(Error::GuessedSpan {
                            target: format!("{}{anchor}", spec.file.display()),
                            command: insert_after_line(&spec.file, content, one.end_line, new),
                        });
                    }
                    let line = match side {
                        AnchorSide::After => one.end_line,
                        AnchorSide::Before => one.line,
                    };
                    Ok((line, Anchor {
                        side,
                        at: anchor.to_owned(),
                        line: Some(line),
                    }))
                },
                many => Err(Error::Ambiguous {
                    target: spec.raw.clone(),
                    candidates: many
                        .iter()
                        .map(|found| Candidate {
                            path: spec.file.clone(),
                            line: found.line,
                            text: found.text.clone(),
                        })
                        .collect(),
                }),
            }
        },
        _ => Err(not_found(
            &spec.raw,
            format!("{anchor} (an insert anchor is @'regex' or #symbol)"),
        )),
    }
}

/// An `@'regex'` anchor for exactly line `line`, with the occurrence that skips identical lines
/// above it.
fn insert_after_line(file: &Path, content: &str, line: usize, new: &[u8]) -> String {
    let text = content.lines().nth(line - 1).unwrap_or_default();
    let pattern = format!("^{}$", regex::escape(text));
    let occurrence = content
        .lines()
        .take(line)
        .filter(|candidate| *candidate == text)
        .count();
    let anchor = if occurrence > 1 {
        format!("@'{pattern}'+{occurrence}")
    } else {
        format!("@'{pattern}'")
    };
    format!(
        "lets edit {} --insert-after {} --new {}",
        shell_word(&file.display().to_string()),
        shell_word(&anchor),
        shell_word(&String::from_utf8_lossy(new)),
    )
}

fn shell_word(text: &str) -> String {
    let bare = !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/=+:@,".contains(&b));
    if bare {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// The `bool` is whether the plaintext heuristic guessed the span's end.
fn scope_span(
    spec: &EditSpec,
    before: &[u8],
    starts: &[usize],
) -> Result<(matcher::Span, bool), Error> {
    match &spec.kind {
        target::Kind::Whole => Ok((
            matcher::Span {
                start: 0,
                end: before.len(),
            },
            false,
        )),
        target::Kind::Line(line) => {
            bounded(spec, before, starts, *line, *line).map(|span| (span, false))
        },
        target::Kind::Range(first, last) => {
            bounded(spec, before, starts, *first, *last).map(|span| (span, false))
        },
        target::Kind::Regex {
            pattern,
            occurrence,
        } => {
            let content = utf8(before, &spec.file)?;
            let found = target::find_regex(content, pattern, *occurrence)
                .map_err(|_| not_found(&spec.raw, format!("@'{pattern}' (invalid regex)")))?
                .ok_or_else(|| not_found(&spec.raw, format!("@'{pattern}'")))?;
            bounded(spec, before, starts, found.line, found.line).map(|span| (span, false))
        },
        target::Kind::Symbol(segments) => {
            let content = utf8(before, &spec.file)?;
            let symbol = format!("#{}", segments.join("."));
            let (found, hint) = resolve_symbol(&spec.file, content, segments);
            match found.as_slice() {
                [] => Err(not_found(&spec.raw, missing(&symbol, hint.as_ref()))),
                [one] => Ok((
                    matcher::Span {
                        start: one.start,
                        end: one.end,
                    },
                    one.end_guessed,
                )),
                many => Err(Error::Ambiguous {
                    target: spec.raw.clone(),
                    candidates: many
                        .iter()
                        .map(|found| Candidate {
                            path: spec.file.clone(),
                            line: found.line,
                            text: found.text.clone(),
                        })
                        .collect(),
                }),
            }
        },
    }
}

fn bounded(
    spec: &EditSpec,
    before: &[u8],
    starts: &[usize],
    first: usize,
    last: usize,
) -> Result<matcher::Span, Error> {
    if first == 0 || last > starts.len() {
        return Err(not_found(&spec.raw, format!(":{first}-{last}")));
    }
    Ok(line_span(before, starts, first, last))
}

fn region(
    after: &[u8],
    changed: &[(usize, usize)],
    marker: Marker,
    annotation: Option<&str>,
    failed: bool,
    omitted: &mut Vec<Omission>,
) -> Option<Region> {
    let text = String::from_utf8_lossy(after);
    let mut all: Vec<&str> = Vec::with_capacity(fs::count_byte(after, b'\n') + 1);
    all.extend(text.lines());
    let mut wanted: BTreeSet<usize> = BTreeSet::new();
    for (first, last) in changed {
        let from = first.saturating_sub(CONTEXT_LINES).max(1);
        let to = (last + CONTEXT_LINES).min(all.len());
        // A reverted edit's `after` was never written, so a truncated range names lines `show`
        // can never open: echo every attempted line instead.
        if !failed && last - first + 1 > SPAN_TRUNCATE_LINES {
            let head_end = (first + SPAN_EDGE_LINES - 1).min(*last);
            let tail_start = last.saturating_sub(SPAN_EDGE_LINES - 1).max(*first);
            wanted.extend(from..=head_end);
            wanted.extend(tail_start..=to);
        } else {
            wanted.extend(from..=to);
        }
    }
    let (start, end) = (*wanted.first()?, *wanted.last()?);
    let annotated = changed.first().map(|(first, _)| *first);
    let mut not_shown: Vec<(usize, usize)> = Vec::new();
    let mut lines: Vec<Line> = Vec::new();
    let mut previous: Option<usize> = None;
    for number in wanted {
        // A hole between two `--all` context windows, or a truncated span's hidden middle, is
        // carried on the row and again in the footer.
        if let Some(gap) = previous
            .filter(|p| number > p + 1)
            .map(|p| (p + 1, number - 1))
        {
            not_shown.push(gap);
            lines.push(Line {
                number: 0,
                marker: Marker::Gap,
                text: format!(":{}-{} not shown", gap.0, gap.1).into(),
            });
        }
        let touched = changed
            .iter()
            .any(|(first, last)| (*first..=*last).contains(&number));
        let mut text = all.get(number - 1).copied().unwrap_or_default().to_owned();
        if let Some(note) = annotation.filter(|_| Some(number) == annotated) {
            text.push_str("          ");
            text.push_str(note);
        }
        lines.push(Line {
            number,
            marker: if touched { marker } else { Marker::None },
            text: text.into(),
        });
        previous = Some(number);
    }
    if !not_shown.is_empty() {
        omitted.push(Omission::RegionGap {
            not_shown,
            path: None,
        });
    }
    Some(Region { start, end, lines })
}

/// `spans` are disjoint and ascending. One pass into a buffer sized up front: splicing one span
/// at a time copied the whole file once per span.
fn splice_all(before: &[u8], spans: &[matcher::Span], replacement: &[u8]) -> Vec<u8> {
    let removed: usize = spans.iter().map(|span| span.end - span.start).sum();
    let mut after = Vec::with_capacity(before.len() - removed + spans.len() * replacement.len());
    let mut kept_from = 0;
    for span in spans {
        after.extend_from_slice(&before[kept_from..span.start]);
        after.extend_from_slice(replacement);
        kept_from = span.end;
    }
    after.extend_from_slice(&before[kept_from..]);
    after
}

fn span_lines(after: &[u8], start: usize, len: usize) -> (usize, usize) {
    let first = matcher::line_of(after, start);
    if len == 0 {
        return (first, first);
    }
    (first, matcher::line_of(after, start + len - 1).max(first))
}

fn line_starts(bytes: &[u8]) -> Vec<usize> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut starts = Vec::with_capacity(fs::count_byte(bytes, b'\n') + 1);
    starts.push(0);
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' && index + 1 < bytes.len() {
            starts.push(index + 1);
        }
    }
    starts
}

/// Includes the last line's newline, so a replacement carries its terminator rather than joining
/// two lines.
fn line_span(before: &[u8], starts: &[usize], first: usize, last: usize) -> matcher::Span {
    matcher::Span {
        start: starts[first - 1],
        end: starts.get(last).copied().unwrap_or(before.len()),
    }
}

fn line_text(before: &[u8], starts: &[usize], line: usize) -> String {
    let span = line_span(before, starts, line, line);
    String::from_utf8_lossy(&before[span.start..span.end])
        .trim_end_matches(['\r', '\n'])
        .to_owned()
}

fn candidates_for(
    spec: &EditSpec,
    before: &[u8],
    starts: &[usize],
    first: usize,
    last: usize,
) -> Vec<Candidate> {
    (first..=last)
        .map(|line| Candidate {
            path: spec.file.clone(),
            line,
            text: line_text(before, starts, line),
        })
        .collect()
}

/// A match found inside a `:a-b` scope carries a line number relative to that slice.
fn shifted(candidate: Candidate, shift: usize) -> Candidate {
    Candidate {
        line: candidate.line + shift,
        ..candidate
    }
}

fn not_found(raw: &str, what: String) -> Error {
    Error::NotFound {
        target: raw.to_owned(),
        what,
        nearest: None,
    }
}

fn utf8<'a>(bytes: &'a [u8], path: &Path) -> Result<&'a str, Error> {
    std::str::from_utf8(bytes).map_err(|_| Error::Unsupported {
        path: path.to_path_buf(),
        reason: UnsupportedReason::NonUtf8Region,
    })
}

/// With no bundled grammar `#name` falls back to the plaintext heuristic, and only then is the
/// `@'regex'` hint `Some`: a heuristic miss does not prove the name absent.
fn resolve_symbol(
    path: &Path,
    content: &str,
    segments: &[String],
) -> (Vec<symbols::SymbolMatch>, Option<String>) {
    let extension = path.extension().unwrap_or_default().to_string_lossy();
    if let Some(lang) = grammars::from_extension(&extension) {
        (symbols::resolve(lang, content, segments), None)
    } else {
        let name = regex::escape(segments.last().map_or("", String::as_str));
        (symbols::plaintext::resolve(content, segments), Some(name))
    }
}

fn missing(base: &str, hint: Option<&String>) -> String {
    hint.map_or_else(|| base.to_owned(), |name| format!("{base} (try @'{name}')"))
}

fn dominant_ending(bytes: &[u8]) -> &'static [u8] {
    let (mut newlines, mut crlf, mut previous) = (0usize, 0usize, 0u8);
    for byte in bytes {
        if *byte == b'\n' {
            newlines += 1;
            crlf += usize::from(previous == b'\r');
        }
        previous = *byte;
    }
    if crlf * 2 > newlines { b"\r\n" } else { b"\n" }
}

/// Unless `literal`, lines take the file's dominant ending, so a CRLF file gains no lone `\n`.
fn joined(new: &[u8], before: &[u8], literal: bool) -> Vec<u8> {
    if literal {
        return new.to_vec();
    }
    let ending = dominant_ending(before);
    let mut out = Vec::with_capacity(new.len());
    for (index, piece) in new.split(|byte| *byte == b'\n').enumerate() {
        if index > 0 {
            out.extend_from_slice(ending);
        }
        out.extend_from_slice(piece.strip_suffix(b"\r").unwrap_or(piece));
    }
    out
}

fn build_specs(args: &EditArgs) -> Result<Vec<EditSpec>, Error> {
    if let Some(from) = args.from.as_deref() {
        if args.target.is_some() {
            return Err(usage(
                "--from - takes no positional target \u{b7} put every edit in the batch on \
                 stdin"
                    .to_owned(),
            ));
        }
        if from != "-" {
            return Err(not_found(
                from,
                "an edit spec (`--from` reads `-`, stdin, only)".to_owned(),
            ));
        }
        return parse_batch(&stdin_text("--from -")?).map_err(|error| with_batch_example(&error));
    }
    from_args(args)
}

/// Trial sessions that got the batch format wrong had only the parse failure to go on, called
/// `lets guide`, and gave up rather than retry; the example runs as written.
fn with_batch_example(error: &Error) -> Error {
    usage(format!("{error}\n\n{BATCH_EXAMPLE}"))
}

const BATCH_EXAMPLE: &str = "lets edit --from - <<'LETS'\n\
@@ a.ts\n\
<<<<<<< old\n\
cap = 10\n\
======= new\n\
cap = 20\n\
>>>>>>>\n\
<<<<<<< old\n\
floor = 1\n\
======= new\n\
floor = 2\n\
>>>>>>>\n\
LETS";

fn stdin_text(flag: &str) -> Result<String, Error> {
    let mut raw = Vec::new();
    std::io::stdin()
        .read_to_end(&mut raw)
        .map_err(|source| Error::Io {
            path: PathBuf::from("-"),
            source,
        })?;
    String::from_utf8(raw).map_err(|_| usage(format!("{flag} input is not valid UTF-8")))
}

/// A value of `-` reads all of stdin, and only one flag may: a second claimant beside
/// `--expect-all` would silently starve one of them.
fn dash_conflict(args: &EditArgs) -> Option<&'static str> {
    let old_dash = args.old.as_deref() == Some("-");
    let new_dash = args.new.as_deref() == Some("-");
    match (old_dash, new_dash, args.expect_all) {
        (true, true, _) => Some("--old - and --new - both read stdin"),
        (true, false, true) => Some("--old - and --expect-all both read stdin"),
        (false, true, true) => Some("--new - and --expect-all both read stdin"),
        _ => None,
    }
}

/// Called once from `from_args`, so several targets share one read of stdin.
fn resolve_dash(
    value: Option<&str>,
    flag: &str,
    allow_empty: bool,
) -> Result<Option<String>, Error> {
    match value {
        Some("-") => {
            let text = stdin_text(flag)?;
            if text.is_empty() && !allow_empty {
                Err(usage(format!(
                    "{flag} - read empty stdin \u{b7} there is nothing to match"
                )))
            } else {
                Ok(Some(text))
            }
        },
        Some(other) => Ok(Some(other.to_owned())),
        None => Ok(None),
    }
}

/// `--if`, `--expect` and `--expect-all` pin one file, so none survives several targets.
fn from_args(args: &EditArgs) -> Result<Vec<EditSpec>, Error> {
    let raw = args
        .target
        .as_deref()
        .ok_or_else(|| not_found(VERB, "a target".to_owned()))?;
    if let Some(conflict) = dash_conflict(args) {
        return Err(usage(format!(
            "{conflict} \u{b7} put every edit in the batch on stdin instead: lets edit --from -"
        )));
    }
    // Only the replace form reads `--old`; resolving it otherwise could consume or refuse stdin.
    let is_replace = args.insert_after.is_none()
        && args.insert_before.is_none()
        && args.expect.is_none()
        && !args.expect_all;
    let old = if is_replace {
        resolve_dash(args.old.as_deref(), "--old", false)?
    } else {
        None
    };
    // Empty `--new` is a deletion, so only `--old -` enforces non-empty stdin.
    let new = resolve_dash(args.new.as_deref(), "--new", true)?;
    if args.more_targets.is_empty() {
        return Ok(vec![one_spec(args, raw, old, new.as_ref())?]);
    }
    let mut targets = vec![raw];
    targets.extend(args.more_targets.iter().map(String::as_str));
    if let Some(flag) = exclusive_flag(args) {
        return Err(usage(format!(
            "{flag} takes one target \u{b7} {} were given",
            targets.len()
        )));
    }
    dedup_targets(targets)
        .into_iter()
        .map(|raw| one_spec(args, raw, old.clone(), new.as_ref()))
        .collect()
}

/// A file spelled twice lands the op once rather than failing against its own change. A path
/// that does not exist yet stays its own key, since there is nothing to canonicalize.
fn dedup_targets(targets: Vec<&str>) -> Vec<&str> {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut out = Vec::new();
    for raw in targets {
        let path = target::parse(raw).path;
        let key = std::fs::canonicalize(&path).unwrap_or(path);
        if !seen.contains(&key) {
            seen.push(key);
            out.push(raw);
        }
    }
    out
}

fn exclusive_flag(args: &EditArgs) -> Option<&'static str> {
    if args.if_sha.is_some() {
        Some("--if")
    } else if args.expect.is_some() {
        Some("--expect")
    } else if args.expect_all {
        Some("--expect-all")
    } else {
        None
    }
}

fn one_spec(
    args: &EditArgs,
    raw: &str,
    old: Option<String>,
    new: Option<&String>,
) -> Result<EditSpec, Error> {
    let parsed = target::parse(raw);
    let new = || {
        required(
            new.cloned(),
            format!("{raw}: --new is required \u{b7} --new '' deletes what the edit matches"),
        )
    };
    let op = if let Some(anchor) = args.insert_after.as_deref() {
        EditOp::Insert {
            side: AnchorSide::After,
            anchor: anchor.to_owned(),
            new: new()?,
        }
    } else if let Some(anchor) = args.insert_before.as_deref() {
        EditOp::Insert {
            side: AnchorSide::Before,
            anchor: anchor.to_owned(),
            new: new()?,
        }
    } else if args.expect.is_some() || args.expect_all {
        EditOp::ReplaceRange {
            expect: args.expect.clone(),
            expect_all: if args.expect_all {
                Some(stdin_text("--expect-all")?)
            } else {
                None
            },
            new: new()?,
        }
    } else {
        EditOp::Replace {
            old: required(
                old,
                format!(
                    "{raw}: --old is required \u{b7} or --expect with a :a-b range, or \
                     --insert-after/--insert-before"
                ),
            )?,
            new: new()?,
            all: args.all,
        }
    };
    Ok(EditSpec {
        raw: raw.to_owned(),
        file: parsed.path,
        kind: parsed.kind,
        op,
        if_sha: parse_if(args.if_sha.as_deref(), raw)?,
        normalize: args.normalize,
        literal_newlines: args.literal_newlines,
    })
}

/// A missing flag is malformed usage (exit 64), never a failed match (exit 1).
fn required(value: Option<String>, message: String) -> Result<Vec<u8>, Error> {
    value.map(String::into_bytes).ok_or_else(|| usage(message))
}

fn parse_if(raw: Option<&str>, target: &str) -> Result<Option<Sha12>, Error> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    Sha12::parse(raw.strip_prefix("sha:").unwrap_or(raw))
        .map(Some)
        .ok_or_else(|| {
            usage(format!(
                "{target}: --if takes sha: and 12 or more hex digits"
            ))
        })
}

fn usage(message: String) -> Error {
    Error::Usage { message }
}

fn parse_batch(text: &str) -> Result<Vec<EditSpec>, Error> {
    let first = text
        .lines()
        .map(str::trim_start)
        .find(|line| !line.is_empty())
        .ok_or_else(|| usage("no edits".to_owned()))?;
    if first.starts_with('{') {
        return parse_jsonl(text);
    }
    if first.starts_with("@@") {
        return parse_fenced(text);
    }
    Err(usage(
        "expected a JSON object (`{`) or a `@@ <path>` block".to_owned(),
    ))
}

const JSONL_KEYS: [&str; 7] = [
    "file",
    "old",
    "new",
    "insert_after",
    "insert_before",
    "if",
    "normalize",
];

fn parse_jsonl(text: &str) -> Result<Vec<EditSpec>, Error> {
    let mut specs = Vec::new();
    let mut errors = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match jsonl_line(line, index + 1) {
            Ok(spec) => specs.push(spec),
            Err(line_errors) => errors.extend(line_errors),
        }
    }
    if let Some(failure) = Error::all(errors) {
        return Err(failure);
    }
    Ok(specs)
}

fn string_field(
    object: &serde_json::Map<String, serde_json::Value>,
    name: &str,
    number: usize,
    errors: &mut Vec<Error>,
) -> Option<String> {
    match object.get(name) {
        None => None,
        Some(serde_json::Value::String(text)) => Some(text.clone()),
        Some(_) => {
            errors.push(usage(format!("line {number}: \"{name}\" must be a string")));
            None
        },
    }
}

fn jsonl_line(line: &str, number: usize) -> Result<EditSpec, Vec<Error>> {
    let value: serde_json::Value = serde_json::from_str(line)
        .map_err(|error| vec![usage(format!("line {number}: {error}"))])?;
    let Some(object) = value.as_object() else {
        return Err(vec![usage(format!(
            "line {number}: expected a JSON object"
        ))]);
    };

    let mut errors = Vec::new();
    for key in object.keys() {
        if !JSONL_KEYS.contains(&key.as_str()) {
            errors.push(usage(format!("line {number}: unknown key \"{key}\"")));
        }
    }
    let file = string_field(object, "file", number, &mut errors);
    let old = string_field(object, "old", number, &mut errors);
    let new = string_field(object, "new", number, &mut errors);
    let insert_after = string_field(object, "insert_after", number, &mut errors);
    let insert_before = string_field(object, "insert_before", number, &mut errors);
    let if_field = string_field(object, "if", number, &mut errors);

    if !object.contains_key("file") {
        errors.push(usage(format!("line {number}: no \"file\"")));
    }
    if !object.contains_key("new") {
        errors.push(usage(format!("line {number}: no \"new\"")));
    }
    if !matches!(
        object.get("normalize"),
        None | Some(serde_json::Value::Bool(_))
    ) {
        errors.push(usage(format!(
            "line {number}: \"normalize\" must be a bool"
        )));
    }

    let anchor_keys: Vec<&str> = ["old", "insert_after", "insert_before"]
        .into_iter()
        .filter(|key| object.contains_key(*key))
        .collect();
    if anchor_keys.len() != 1 {
        errors.push(usage(format!(
            "line {number}: exactly one of \"old\", \"insert_after\" or \"insert_before\" is \
             required \u{b7} found {}",
            if anchor_keys.is_empty() {
                "none".to_owned()
            } else {
                anchor_keys.join(", ")
            }
        )));
    }

    let if_sha = match parse_if(if_field.as_deref(), &format!("line {number}")) {
        Ok(sha) => sha,
        Err(error) => {
            errors.push(error);
            None
        },
    };

    if !errors.is_empty() {
        return Err(errors);
    }

    let new_bytes = new.unwrap_or_default().into_bytes();
    let op = match anchor_keys[0] {
        "insert_after" => EditOp::Insert {
            side: AnchorSide::After,
            anchor: insert_after.expect("checked above"),
            new: new_bytes,
        },
        "insert_before" => EditOp::Insert {
            side: AnchorSide::Before,
            anchor: insert_before.expect("checked above"),
            new: new_bytes,
        },
        _ => EditOp::Replace {
            old: old.expect("checked above").into_bytes(),
            new: new_bytes,
            all: false,
        },
    };
    let file = file.expect("checked above");
    let parsed = target::parse(&file);
    Ok(EditSpec {
        raw: file,
        file: parsed.path,
        kind: parsed.kind,
        op,
        if_sha,
        normalize: object
            .get("normalize")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        literal_newlines: false,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Outside,
    Old,
    New,
}

/// The `@@` line's number, its file, and its insert anchor when the block inserts.
type FenceHeader = (usize, String, Option<(AnchorSide, String)>);

/// A header stays active across every block that follows it until the next `@@` line or the end
/// of the input, so `blocks_under_header` (not `header` itself) is what a completed batch checks.
fn parse_fenced(text: &str) -> Result<Vec<EditSpec>, Error> {
    let mut specs = Vec::new();
    let mut errors = Vec::new();
    let mut header: Option<FenceHeader> = None;
    let mut blocks_under_header: usize = 0;
    let mut old: Option<Vec<&str>> = None;
    let mut new: Option<Vec<&str>> = None;
    let mut section = Section::Outside;
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        if let Some(rest) = line.strip_prefix("@@ ") {
            if let Some((header_line, ..)) = &header
                && (section != Section::Outside || blocks_under_header == 0)
            {
                errors.push(usage(format!(
                    "line {header_line}: unterminated `@@` block \u{b7} a new one opens at line \
                     {number}"
                )));
            }
            let (file, anchor) = fence_header(rest);
            header = Some((number, file, anchor));
            blocks_under_header = 0;
            old = None;
            new = None;
            section = Section::Outside;
        } else if line.trim_end() == "<<<<<<< old" {
            if let Some((header_line, _, Some(_))) = &header {
                errors.push(usage(format!(
                    "line {number}: `<<<<<<< old` is not allowed in the insert block opened at \
                     line {header_line}"
                )));
            }
            old = Some(Vec::new());
            section = Section::Old;
        } else if line.trim_end() == "======= new" {
            new = Some(Vec::new());
            section = Section::New;
        } else if line.trim_end() == ">>>>>>>" {
            match &header {
                Some((header_line, file, anchor)) => {
                    blocks_under_header += 1;
                    // Two inserts under one anchor race for "right after it": the second landed
                    // adjoins the anchor too, ahead of the first, not after it as written.
                    if blocks_under_header > 1
                        && let Some((side, at)) = anchor
                    {
                        errors.push(usage(format!(
                            "line {number}: `@@ {file} insert-{side} {at}` already opened one \
                             insert block \u{b7} a second block under the same header is \
                             ambiguous \u{b7} repeat `@@ {file} insert-{side} {at}` before it"
                        )));
                    }
                    match fenced_spec(*header_line, file, anchor.clone(), old.take(), new.take()) {
                        Ok(spec) => specs.push(spec),
                        Err(error) => errors.push(error),
                    }
                },
                None => errors.push(usage(format!(
                    "line {number}: `>>>>>>>` outside a `@@` block"
                ))),
            }
            section = Section::Outside;
        } else {
            match section {
                Section::Old => old.get_or_insert_default().push(line),
                Section::New => new.get_or_insert_default().push(line),
                Section::Outside if line.trim().is_empty() => {},
                Section::Outside => {
                    errors.push(usage(format!(
                        "line {number}: `{line}` is outside a `@@` block"
                    )));
                },
            }
        }
    }
    if let Some((header_line, ..)) = &header
        && (section != Section::Outside || blocks_under_header == 0)
    {
        errors.push(usage(format!(
            "line {header_line}: unterminated `@@` block"
        )));
    }
    if let Some(failure) = Error::all(errors) {
        return Err(failure);
    }
    Ok(specs)
}

fn fence_header(rest: &str) -> (String, Option<(AnchorSide, String)>) {
    for (marker, side) in [
        (" insert-after ", AnchorSide::After),
        (" insert-before ", AnchorSide::Before),
    ] {
        if let Some((file, anchor)) = rest.split_once(marker) {
            return (
                file.trim().to_owned(),
                Some((side, anchor.trim().to_owned())),
            );
        }
    }
    (rest.trim().to_owned(), None)
}

fn fenced_spec(
    header_line: usize,
    file: &str,
    anchor: Option<(AnchorSide, String)>,
    old: Option<Vec<&str>>,
    new: Option<Vec<&str>>,
) -> Result<EditSpec, Error> {
    let Some(new) = new else {
        return Err(usage(format!(
            "line {header_line}: {file}: no `======= new` section"
        )));
    };
    let new = new.join("\n").into_bytes();
    let op = if let Some((side, anchor)) = anchor {
        EditOp::Insert { side, anchor, new }
    } else {
        let Some(old) = old else {
            return Err(usage(format!(
                "line {header_line}: {file}: no `<<<<<<< old` section"
            )));
        };
        EditOp::Replace {
            old: old.join("\n").into_bytes(),
            new,
            all: false,
        }
    };
    let parsed = target::parse(file);
    Ok(EditSpec {
        raw: file.to_owned(),
        file: parsed.path,
        kind: parsed.kind,
        op,
        if_sha: None,
        normalize: false,
        literal_newlines: false,
    })
}

#[cfg(test)]
mod tests {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt as _;

    use tempfile::TempDir;

    use super::*;
    use crate::output::{Format, RenderOptions};
    use crate::own_process::{Workdir, repo};

    const USAGE: &str = "export function usage(id: string) {\n  const now = Date.now()\n  const \
                         cap = 10\n  if (n > cap) return\n  return total\n}\n";

    fn write(dir: &Workdir, name: &str, content: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, content).expect("test fixture writes");
        path
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).expect("fixture is readable")
    }

    #[test]
    fn splice_all_replaces_every_span_and_keeps_every_other_byte() {
        let span = |start, end| matcher::Span { start, end };
        assert_eq!(
            splice_all(b"aXbXXc", &[span(1, 2), span(3, 5)], b"--"),
            b"a--b--c"
        );
        assert_eq!(
            splice_all(b"abcdef", &[span(0, 1), span(5, 6)], b""),
            b"bcde"
        );
        assert_eq!(
            splice_all(b"\r\nkeep\r\n", &[span(2, 6)], b"k"),
            b"\r\nk\r\n"
        );
        assert_eq!(splice_all(b"same", &[], b"unused"), b"same");
    }

    fn edit_args(target: &str) -> EditArgs {
        EditArgs {
            target: Some(target.to_owned()),
            more_targets: Vec::new(),
            old: None,
            new: None,
            all: false,
            expect: None,
            expect_all: false,
            insert_after: None,
            insert_before: None,
            from: None,
            if_sha: None,
            normalize: false,
            literal_newlines: false,
            check: None,
            check_timeout: 60,
        }
    }

    fn replacing(target: &str, old: &str, new: &str) -> EditArgs {
        let mut args = edit_args(target);
        args.old = Some(old.to_owned());
        args.new = Some(new.to_owned());
        args
    }

    fn global() -> Global {
        Global {
            json: false,
            jsonl: false,
            budget: None,
            max_bytes: 65536,
            max_file_bytes: 8_388_608,
            no_ignore: false,
            allow_outside: false,
            no_check: false,
            quiet: false,
        }
    }

    fn rendered(outcome: &Outcome) -> String {
        crate::output::render(&outcome.response, Format::Text, &RenderOptions {
            numbers: true,
            quiet: false,
        })
    }

    fn error_of(outcome: Outcome) -> Error {
        outcome.error.expect("this case fails")
    }

    #[test]
    fn unique_old_replaces_it_and_reports_one_exact_match() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", USAGE);

        let outcome = run(
            &replacing("usage.ts", "const cap = 10", "const cap = 20"),
            &global(),
            Format::Text,
        );

        assert!(outcome.error.is_none(), "a unique match applies");
        assert_eq!(read(&path), USAGE.replace("cap = 10", "cap = 20"));
        let text = rendered(&outcome);
        assert!(
            text.contains(
                "\u{2500}\u{2500} usage.ts \u{b7} 1 replacement \u{b7} line 3 \u{b7} exact"
            ),
            "{text}"
        );
        assert!(text.contains("3~\t  const cap = 20"), "{text}");
        assert!(text.contains("2 \t  const now = Date.now()"), "{text}");
        assert!(text.contains("check: structure ok \u{b7} sha:"), "{text}");
    }

    #[test]
    fn old_that_is_not_there_names_the_nearest_line_and_writes_nothing() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", USAGE);

        let error = error_of(run(
            &replacing("usage.ts", "const cap = 15", "const cap = 20"),
            &global(),
            Format::Text,
        ));

        assert_eq!(read(&path), USAGE);
        assert_eq!(error.slug(), "not_found");
        assert!(error.to_string().contains("nearest: line 3"), "{error}");
    }

    #[test]
    fn old_indented_two_spaces_too_far_names_the_line_and_both_indents() {
        let Some(dir) = repo() else { return };
        let path = write(
            &dir,
            "greet.js",
            "function greet() {\n        console.warn('hi')\n}\n",
        );

        let error = error_of(run(
            &replacing(
                "greet.js",
                "          console.warn('hi')",
                "          console.warn('bye')",
            ),
            &global(),
            Format::Text,
        ));

        assert_eq!(
            read(&path),
            "function greet() {\n        console.warn('hi')\n}\n"
        );
        assert_eq!(error.slug(), "not_found");
        assert!(
            error.to_string().contains(
                "matches line 2 except leading whitespace: the file indents it 8 \
                 spaces, --old has 10"
            ),
            "{error}"
        );
    }

    #[test]
    fn multiline_old_indented_too_far_names_every_line() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "config.yaml", "top:\n    a: 1\n    b: 2\n");

        let error = error_of(run(
            &replacing(
                "config.yaml",
                "      a: 1\n      b: 2",
                "      a: 9\n      b: 9",
            ),
            &global(),
            Format::Text,
        ));

        assert_eq!(read(&path), "top:\n    a: 1\n    b: 2\n");
        assert!(
            error.to_string().contains(
                "matches lines 2-3 except leading whitespace: line 2 the file indents it 4 \
                 spaces, --old has 6; line 3 the file indents it 4 spaces, --old has 6"
            ),
            "{error}"
        );
    }

    #[test]
    fn multiline_old_with_only_the_first_line_fixed_names_the_lines_still_off() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "config.yaml", "top:\n  a: 1\n    b: 2\n    c: 3\n");

        let error = error_of(run(
            &replacing(
                "config.yaml",
                "  a: 1\n      b: 2\n      c: 3",
                "  a: 9\n      b: 9\n      c: 9",
            ),
            &global(),
            Format::Text,
        ));

        assert_eq!(read(&path), "top:\n  a: 1\n    b: 2\n    c: 3\n");
        assert!(
            error.to_string().contains(
                "matches lines 2-4 except leading whitespace: line 3 the file indents it 4 \
                 spaces, --old has 6; line 4 the file indents it 4 spaces, --old has 6"
            ),
            "{error}"
        );
        assert!(
            !error.to_string().contains("line 2 the file indents"),
            "line 2 already matched and must not be named: {error}"
        );
    }

    #[test]
    fn a_block_wider_than_the_cap_tallies_the_rest_as_plus_n_more() {
        let Some(dir) = repo() else { return };
        let file_lines = (0..7).fold(String::new(), |mut lines, n| {
            writeln!(lines, "    item{n}: {n}").unwrap();
            lines
        });
        let path = write(&dir, "config.yaml", &format!("top:\n{file_lines}"));
        let old: String = (0..7)
            .map(|n| format!("      item{n}: {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = old.replace("item", "changed");

        let error = error_of(run(
            &replacing("config.yaml", &old, &new),
            &global(),
            Format::Text,
        ));

        assert_eq!(read(&path), format!("top:\n{file_lines}"));
        let rendered = error.to_string();
        assert!(
            rendered.contains("matches lines 2-8 except leading whitespace"),
            "{rendered}"
        );
        for line in 2..=6 {
            assert!(
                rendered.contains(&format!(
                    "line {line} the file indents it 4 spaces, --old has 6"
                )),
                "line {line} should be named: {rendered}"
            );
        }
        assert!(
            !rendered.contains("line 7 the file indents")
                && !rendered.contains("line 8 the file indents"),
            "lines past the cap are tallied, not named: {rendered}"
        );
        assert!(rendered.contains("(+2 more)"), "{rendered}");
    }

    #[test]
    fn old_indented_with_spaces_against_a_tab_names_both_kinds() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "job.mk", "build:\n\techo hi\n");

        let error = error_of(run(
            &replacing("job.mk", "  echo hi", "  echo bye"),
            &global(),
            Format::Text,
        ));

        assert_eq!(read(&path), "build:\n\techo hi\n");
        assert!(
            error.to_string().contains(
                "matches line 2 except leading whitespace: the file indents it 1 tab, \
                 --old has 2 spaces"
            ),
            "{error}"
        );
    }

    #[test]
    fn a_miss_that_differs_beyond_whitespace_has_no_indent_hint() {
        let Some(dir) = repo() else { return };
        write(
            &dir,
            "greet.js",
            "function greet() {\n        console.warn('hi')\n}\n",
        );

        let error = error_of(run(
            &replacing("greet.js", "          console.warn('bye')", "x"),
            &global(),
            Format::Text,
        ));

        assert!(
            !error.to_string().contains("except leading whitespace"),
            "{error}"
        );
    }

    #[test]
    fn matching_indentation_applies_cleanly_with_no_indent_hint() {
        let Some(dir) = repo() else { return };
        let path = write(
            &dir,
            "greet.js",
            "function greet() {\n        console.warn('hi')\n}\n",
        );

        let outcome = run(
            &replacing(
                "greet.js",
                "        console.warn('hi')",
                "        console.warn('bye')",
            ),
            &global(),
            Format::Text,
        );

        assert!(outcome.error.is_none());
        assert_eq!(
            read(&path),
            "function greet() {\n        console.warn('bye')\n}\n"
        );
    }

    #[test]
    fn exact_miss_with_a_folded_match_names_the_line_and_the_flag() {
        let Some(dir) = repo() else { return };
        let path = write(
            &dir,
            "notes.md",
            "# notes\n\nIn that case the agent \u{2013} not the user \u{2013} decides.\n",
        );

        let error = error_of(run(
            &replacing(
                "notes.md",
                "the agent - not the user - decides",
                "the agent decides",
            ),
            &global(),
            Format::Text,
        ));

        assert!(!read(&path).contains("the agent decides"));
        assert!(
            error
                .to_string()
                .contains("a normalized match exists at line 3 \u{2014} pass --normalize"),
            "{error}"
        );
    }

    #[test]
    fn normalize_replaces_the_folded_span_and_names_it() {
        let Some(dir) = repo() else { return };
        let path = write(
            &dir,
            "notes.md",
            "# notes\n\nIn that case the agent \u{2013} not the user \u{2013} decides.\n",
        );
        let mut args = replacing(
            "notes.md",
            "the agent - not the user - decides",
            "the agent decides",
        );
        args.normalize = true;

        let outcome = run(&args, &global(), Format::Text);

        assert!(outcome.error.is_none());
        assert_eq!(read(&path), "# notes\n\nIn that case the agent decides.\n");
        let text = rendered(&outcome);
        assert!(text.contains("normalized (\u{2013} \u{2192} -)"), "{text}");
        assert!(text.contains("check: structure ok \u{b7} sha:"), "{text}");
        assert!(text.contains("\u{b7} normalized"), "{text}");
    }

    #[test]
    fn old_that_matches_twice_without_all_lists_both_and_writes_nothing() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", USAGE);

        let error = error_of(run(
            &replacing("usage.ts", "return", "return undefined"),
            &global(),
            Format::Text,
        ));

        assert_eq!(read(&path), USAGE);
        assert_eq!(error.slug(), "ambiguous");
        let message = error.to_string();
        assert!(message.contains("usage.ts:4"), "{message}");
        assert!(message.contains("usage.ts:5"), "{message}");
    }

    #[test]
    fn all_replaces_every_occurrence_and_counts_them() {
        let Some(dir) = repo() else { return };
        let path = write(
            &dir,
            "usage.ts",
            "import { usageCap } from './config'\n\nconst a = usageCap\nconst b = usageCap\n",
        );
        let mut args = replacing("usage.ts", "usageCap", "usageLimit");
        args.all = true;

        let outcome = run(&args, &global(), Format::Text);

        assert!(outcome.error.is_none());
        assert_eq!(read(&path).matches("usageLimit").count(), 3);
        assert!(!read(&path).contains("usageCap"));
        let text = rendered(&outcome);
        assert!(
            text.contains("3 replacements \u{b7} lines 1, 3, 4"),
            "{text}"
        );
    }

    #[test]
    fn expect_that_matches_the_line_replaces_the_range() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", USAGE);
        let mut args = edit_args("usage.ts:3");
        args.expect = Some("const cap = 10".to_owned());
        args.new = Some("  const cap = 20".to_owned());

        let outcome = run(&args, &global(), Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(
            read(&path),
            USAGE.replace("  const cap = 10", "  const cap = 20")
        );
        assert!(rendered(&outcome).contains("expect matched"), "{outcome:?}");
    }

    #[test]
    fn expect_that_does_not_match_exits_2_and_shows_the_line() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", USAGE);
        let mut args = edit_args("usage.ts:3");
        args.expect = Some("const cap = 99".to_owned());
        args.new = Some("  const cap = 20".to_owned());

        let error = error_of(run(&args, &global(), Format::Text));

        assert_eq!(read(&path), USAGE);
        assert_eq!(error.slug(), "expect_refused");
        let message = error.to_string();
        assert!(message.contains("it reads:   const cap = 10"), "{message}");
        assert!(
            message.contains("--expect-all") && message.contains("--if sha:"),
            "{message}"
        );
    }

    #[test]
    fn multi_line_range_with_expect_alone_is_refused_before_the_write() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", USAGE);
        let mut args = edit_args("usage.ts:3-4");
        args.expect = Some("const cap = 10".to_owned());
        args.new = Some("  const cap = 20".to_owned());

        let error = error_of(run(&args, &global(), Format::Text));

        assert_eq!(read(&path), USAGE);
        assert_eq!(error.slug(), "expect_refused");
        let message = error.to_string();
        assert!(
            message.contains("line 3 only") && message.contains("runs to line 4"),
            "{message}"
        );
        assert!(
            message.contains("--expect-all") && message.contains("--if sha:"),
            "{message}"
        );
    }

    #[test]
    fn multi_line_range_with_expect_and_if_sha_applies() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", USAGE);
        let mut args = edit_args("usage.ts:3-4");
        args.expect = Some("const cap = 10".to_owned());
        args.new = Some("  const cap = 20\n  if (n > cap) throw new CapError()".to_owned());
        args.if_sha = Some(format!("sha:{}", atomic::hash12(USAGE.as_bytes()).as_str()));

        let outcome = run(&args, &global(), Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(read(&path).contains("throw new CapError()"));
        let text = rendered(&outcome);
        assert!(text.contains("expect matched \u{b7} sha matched"), "{text}");
    }

    // clap refuses an unknown preset first; this is the library's own guard behind it.
    #[test]
    fn an_unknown_preset_is_a_usage_error_before_any_write() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", USAGE);
        let mut args = replacing("usage.ts", "const cap = 10", "const cap = 20");
        args.check = Some("@nope".to_owned());

        let error = error_of(run(&args, &global(), Format::Text));

        assert_eq!(read(&path), USAGE);
        assert_eq!(error.slug(), "usage");
    }

    #[test]
    fn old_without_new_is_a_usage_error_naming_new_and_writes_nothing() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", USAGE);
        let mut args = edit_args("usage.ts");
        args.old = Some("const cap = 10".to_owned());

        let error = error_of(run(&args, &global(), Format::Text));

        assert_eq!(read(&path), USAGE);
        assert_eq!(error.slug(), "usage");
        assert!(error.to_string().contains("--new is required"), "{error}");
    }

    #[test]
    fn new_without_old_expect_or_an_insert_is_a_usage_error_naming_old() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", USAGE);
        let mut args = edit_args("usage.ts:3");
        args.new = Some("x".to_owned());

        let error = error_of(run(&args, &global(), Format::Text));

        assert_eq!(read(&path), USAGE);
        assert_eq!(error.slug(), "usage");
        assert!(error.to_string().contains("--old is required"), "{error}");
    }

    #[test]
    fn old_with_an_empty_new_deletes_the_match() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", USAGE);

        let outcome = run(
            &replacing("usage.ts", "  const now = Date.now()\n", ""),
            &global(),
            Format::Text,
        );

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(read(&path), USAGE.replace("  const now = Date.now()\n", ""));
    }

    #[test]
    fn old_dash_and_new_dash_together_conflict() {
        let mut args = edit_args("a.ts");
        args.old = Some("-".to_owned());
        args.new = Some("-".to_owned());

        assert!(dash_conflict(&args).is_some());
    }

    #[test]
    fn new_dash_with_expect_all_conflicts() {
        let mut args = edit_args("a.ts");
        args.new = Some("-".to_owned());
        args.expect_all = true;

        assert!(dash_conflict(&args).is_some());
    }

    #[test]
    fn old_dash_with_expect_all_conflicts() {
        let mut args = edit_args("a.ts");
        args.old = Some("-".to_owned());
        args.expect_all = true;

        assert!(dash_conflict(&args).is_some());
    }

    #[test]
    fn a_lone_dash_flag_has_no_conflict() {
        let mut old_only = edit_args("a.ts");
        old_only.old = Some("-".to_owned());
        assert!(dash_conflict(&old_only).is_none());

        let mut new_only = edit_args("a.ts");
        new_only.new = Some("-".to_owned());
        assert!(dash_conflict(&new_only).is_none());

        assert!(dash_conflict(&edit_args("a.ts")).is_none());
    }

    #[test]
    fn a_known_preset_with_no_manifest_applies_and_names_the_skip() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", USAGE);
        let mut args = replacing("usage.ts", "const cap = 10", "const cap = 20");
        args.check = Some("@auto".to_owned());

        let outcome = run(&args, &global(), Format::Text);

        assert!(outcome.error.is_none(), "{outcome:?}");
        assert_eq!(read(&path), USAGE.replace("cap = 10", "cap = 20"));
        assert!(
            rendered(&outcome).contains("check: skipped (@auto found no manifest)"),
            "{outcome:?}"
        );
    }

    #[test]
    fn insert_after_a_regex_anchor_lands_on_the_next_line() {
        let Some(dir) = repo() else { return };
        let path = write(
            &dir,
            "app.ts",
            "import React from 'react'\n\nexport const App = () => null\n",
        );
        let mut args = edit_args("app.ts");
        args.insert_after = Some("@^import .* from".to_owned());
        args.new = Some("import { usage } from './store/usage'".to_owned());

        let outcome = run(&args, &global(), Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(
            read(&path),
            "import React from 'react'\nimport { usage } from './store/usage'\n\nexport const App \
             = () => null\n"
        );
        let text = rendered(&outcome);
        assert!(text.contains("inserted 1 line after line 1"), "{text}");
        assert!(text.contains("2+\timport { usage }"), "{text}");
    }

    #[test]
    fn an_insert_whose_new_ends_in_a_newline_adds_that_one_line_only() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "m.py", "import os\nx = 1\n");
        let specs = parse_batch(
            "{\"file\":\"m.py\",\"insert_after\":\"@'^import'\",\"new\":\"import sys\\n\"}\n",
        )
        .expect("the JSONL form parses");

        let outcome = apply(&specs, &edit_args("-"), &global());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(read(&path), "import os\nimport sys\nx = 1\n");
        assert!(
            rendered(&outcome).contains("inserted 1 line after line 1"),
            "{}",
            rendered(&outcome)
        );
    }

    #[test]
    fn an_insert_before_whose_new_ends_in_a_crlf_adds_one_line_to_a_crlf_file() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "a.ts", "const a = 1;\r\nconst b = 2;\r\n");
        let mut args = edit_args("a.ts");
        args.insert_before = Some("@^const b".to_owned());
        args.new = Some("const c = 3;\r\n".to_owned());

        let outcome = run(&args, &global(), Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(
            read(&path),
            "const a = 1;\r\nconst c = 3;\r\nconst b = 2;\r\n"
        );
    }

    #[test]
    fn an_insert_of_two_lines_ending_in_a_newline_adds_exactly_two() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "m.py", "import os\nx = 1\n");
        let mut args = edit_args("m.py");
        args.insert_after = Some("@^import".to_owned());
        args.new = Some("import sys\n\n".to_owned());

        let outcome = run(&args, &global(), Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(read(&path), "import os\nimport sys\n\nx = 1\n");
        assert!(rendered(&outcome).contains("inserted 2 lines after line 1"));
    }

    #[test]
    fn insert_after_an_anchor_that_matches_nothing_writes_nothing() {
        let Some(dir) = repo() else { return };
        let before = "import React from 'react'\n";
        let path = write(&dir, "app.ts", before);
        let mut args = edit_args("app.ts");
        args.insert_after = Some("@^require".to_owned());
        args.new = Some("import { usage } from './store/usage'".to_owned());

        let error = error_of(run(&args, &global(), Format::Text));

        assert_eq!(read(&path), before);
        assert_eq!(error.slug(), "not_found");
    }

    #[test]
    fn insert_before_a_symbol_anchor_names_the_symbol_and_its_line() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", USAGE);
        let mut args = edit_args("usage.ts");
        args.insert_before = Some("#usage".to_owned());
        args.new = Some("/** Returns the running total for id. */".to_owned());

        let outcome = run(&args, &global(), Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(
            read(&path).starts_with("/** Returns the running total for id. */\nexport function")
        );
        let text = rendered(&outcome);
        assert!(
            text.contains("inserted 1 line before #usage (line 1)"),
            "{text}"
        );
    }

    #[test]
    fn a_layer_1_failure_renders_the_edit_reverted_and_leaves_the_file_alone() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", USAGE);

        let outcome = run(
            &replacing("usage.ts", "return total", "return total)"),
            &global(),
            Format::Text,
        );

        assert_eq!(read(&path), USAGE, "layer 1 runs before any write");
        let text = rendered(&outcome);
        assert!(text.contains("\u{b7} REVERTED"), "{text}");
        assert!(
            text.contains("return total)          \u{2190} parse error"),
            "{text}"
        );
        let sha = atomic::hash12(USAGE.as_bytes());
        assert!(
            text.contains(&format!(
                "check: failed \u{2192} reverted \u{b7} file unchanged \u{b7} sha:{}",
                sha.as_str()
            )),
            "{text}"
        );
        let error = error_of(outcome);
        assert_eq!(error.slug(), "check_failed");
    }

    #[test]
    fn a_layer_1_pass_lands_the_edit() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", USAGE);

        let outcome = run(
            &replacing("usage.ts", "return total", "return total + 1"),
            &global(),
            Format::Text,
        );

        assert!(outcome.error.is_none());
        assert!(read(&path).contains("return total + 1"));
        assert!(
            rendered(&outcome).contains("check: structure ok"),
            "{outcome:?}"
        );
    }

    #[test]
    fn if_sha_that_does_not_match_writes_nothing() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", USAGE);
        let mut args = replacing("usage.ts", "const cap = 10", "const cap = 20");
        args.if_sha = Some("sha:000000000000".to_owned());

        let error = error_of(run(&args, &global(), Format::Text));

        assert_eq!(read(&path), USAGE);
        assert_eq!(error.slug(), "changed");
    }

    #[test]
    fn if_sha_that_matches_applies_the_edit() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", USAGE);
        let mut args = replacing("usage.ts", "const cap = 10", "const cap = 20");
        args.if_sha = Some(format!("sha:{}", atomic::hash12(USAGE.as_bytes()).as_str()));

        let outcome = run(&args, &global(), Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(read(&path).contains("const cap = 20"));
    }

    #[test]
    fn a_target_outside_the_working_tree_is_refused() {
        let Some(dir) = repo() else { return };
        let outside = TempDir::new().expect("temp dir outside the repo");
        let path = outside.path().join("zshrc");
        std::fs::write(&path, "alias x=y\n").expect("fixture writes");
        drop(dir);

        let error = error_of(run(
            &replacing(&path.display().to_string(), "alias x", "alias z"),
            &global(),
            Format::Text,
        ));

        assert_eq!(read(&path), "alias x=y\n");
        assert_eq!(error.slug(), "outside_tree");
    }

    #[test]
    fn a_target_outside_the_working_tree_applies_with_allow_outside() {
        let Some(_dir) = repo() else { return };
        let outside = TempDir::new().expect("temp dir outside the repo");
        let path = outside.path().join("zshrc");
        std::fs::write(&path, "alias x=y\n").expect("fixture writes");
        let mut global = global();
        global.allow_outside = true;

        let outcome = run(
            &replacing(&path.display().to_string(), "alias x", "alias z"),
            &global,
            Format::Text,
        );

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(read(&path), "alias z=y\n");
    }

    #[test]
    fn a_jsonl_batch_applies_every_edit() {
        let Some(dir) = repo() else { return };
        let a = write(&dir, "a.ts", "const cap = 10\n");
        let c = write(&dir, "c.ts", "import x from 'y'\n");
        let specs = parse_batch(
            "{\"file\":\"a.ts\",\"old\":\"const cap = 10\",\"new\":\"const cap = 20\"}\n\
             {\"file\":\"c.ts\",\"insert_after\":\"@'^import'\",\"new\":\"import { limit } from \
             './a'\"}\n",
        )
        .expect("the JSONL form parses");

        let outcome = apply(&specs, &edit_args("-"), &global());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(read(&a), "const cap = 20\n");
        assert_eq!(read(&c), "import x from 'y'\nimport { limit } from './a'\n");
    }

    #[test]
    fn a_fenced_batch_applies_every_edit_and_counts_them() {
        let Some(dir) = repo() else { return };
        let a = write(&dir, "a.ts", "const cap = 10\n");
        let b = write(&dir, "b.ts", "import { cap } from './a'\n");
        let c = write(&dir, "c.ts", "import x from 'y'\n");
        let specs = parse_batch(
            "@@ a.ts\n\
             <<<<<<< old\n\
             const cap = 10\n\
             ======= new\n\
             const cap = 20\n\
             >>>>>>>\n\
             @@ b.ts\n\
             <<<<<<< old\n\
             import { cap }\n\
             ======= new\n\
             import { cap, limit }\n\
             >>>>>>>\n\
             @@ c.ts insert-after @'^import'\n\
             ======= new\n\
             import { limit } from './a'\n\
             >>>>>>>\n",
        )
        .expect("the fence form parses");

        let outcome = apply(&specs, &edit_args("-"), &global());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(read(&a), "const cap = 20\n");
        assert_eq!(read(&b), "import { cap, limit } from './a'\n");
        assert_eq!(read(&c), "import x from 'y'\nimport { limit } from './a'\n");
        let text = rendered(&outcome);
        assert!(
            text.contains(
                "3 files \u{b7} 3 edits \u{b7} all applied \u{b7} checks: structure ok \u{d7}3"
            ),
            "{text}"
        );
    }

    #[test]
    fn one_ambiguous_edit_in_a_batch_writes_none_of_it() {
        let Some(dir) = repo() else { return };
        let a = write(&dir, "a.ts", "const cap = 10\n");
        let b_before = "import { cap } from './a'\nimport { cap } from './legacy'\n";
        let b = write(&dir, "b.ts", b_before);
        let c = write(&dir, "c.ts", "import x from 'y'\n");
        let specs = parse_batch(
            "{\"file\":\"a.ts\",\"old\":\"const cap = 10\",\"new\":\"const cap = 20\"}\n\
             {\"file\":\"b.ts\",\"old\":\"import { cap }\",\"new\":\"import { cap, limit }\"}\n\
             {\"file\":\"c.ts\",\"old\":\"import x\",\"new\":\"import y\"}\n",
        )
        .expect("the JSONL form parses");

        let outcome = apply(&specs, &edit_args("-"), &global());

        assert_eq!(read(&a), "const cap = 10\n");
        assert_eq!(read(&b), b_before);
        assert_eq!(read(&c), "import x from 'y'\n");
        let text = rendered(&outcome);
        assert!(
            text.contains("0 of 3 applied \u{b7} nothing written \u{b7} fix b.ts and re-run"),
            "{text}"
        );
        assert_eq!(error_of(outcome).slug(), "ambiguous");
    }

    #[test]
    fn a_batch_with_two_failing_edits_names_both_files_and_both_failures() {
        let Some(dir) = repo() else { return };
        let a = write(&dir, "a.ts", "const cap = 10\n");
        let b = write(&dir, "b.ts", "const floor = 1\n");
        let c = write(&dir, "c.ts", "const top = 9\n");
        let specs = parse_batch(
            "{\"file\":\"a.ts\",\"old\":\"const cap = 10\",\"new\":\"const cap = 20\"}\n\
             {\"file\":\"b.ts\",\"old\":\"zzz\",\"new\":\"y\"}\n\
             {\"file\":\"c.ts\",\"old\":\"qqq\",\"new\":\"y\"}\n",
        )
        .expect("the JSONL form parses");

        let outcome = apply(&specs, &edit_args("-"), &global());

        assert_eq!(read(&a), "const cap = 10\n");
        assert_eq!(read(&b), "const floor = 1\n");
        assert_eq!(read(&c), "const top = 9\n");
        let text = rendered(&outcome);
        assert!(
            text.contains("0 of 3 applied \u{b7} nothing written \u{b7} fix b.ts, c.ts and re-run"),
            "{text}"
        );
        let error = error_of(outcome);
        assert_eq!(error.slug(), "not_found");
        let stderr = error.to_string();
        assert!(
            stderr.contains("in b.ts") && stderr.contains("in c.ts"),
            "{stderr}"
        );
    }

    #[test]
    fn a_jsonl_batch_entry_with_a_whitespace_only_miss_gets_the_indent_hint() {
        let Some(dir) = repo() else { return };
        write(&dir, "a.ts", "function f() {\n        return 1\n}\n");
        let specs = parse_batch(
            "{\"file\":\"a.ts\",\"old\":\"          return 1\",\"new\":\"          return 2\"}\n",
        )
        .expect("the JSONL form parses");

        let outcome = apply(&specs, &edit_args("-"), &global());

        let error = error_of(outcome);
        assert!(
            error.to_string().contains(
                "matches line 2 except leading whitespace: the file indents it 8 \
                 spaces, --old has 10"
            ),
            "{error}"
        );
    }

    #[test]
    fn a_fenced_batch_entry_with_a_whitespace_only_miss_gets_the_indent_hint() {
        let Some(dir) = repo() else { return };
        write(&dir, "a.ts", "function f() {\n        return 1\n}\n");
        let ten = " ".repeat(10);
        let text =
            format!("@@ a.ts\n<<<<<<< old\n{ten}return 1\n======= new\n{ten}return 2\n>>>>>>>\n");
        let specs = parse_batch(&text).expect("the fence form parses");

        let outcome = apply(&specs, &edit_args("-"), &global());

        let error = error_of(outcome);
        assert!(
            error.to_string().contains(
                "matches line 2 except leading whitespace: the file indents it 8 \
                 spaces, --old has 10"
            ),
            "{error}"
        );
    }

    #[test]
    fn two_edits_on_one_file_both_land_however_the_path_is_spelled() {
        let Some(dir) = repo() else { return };
        let a = write(&dir, "a.ts", "const cap = 10\nconst floor = 1\n");
        let specs = parse_batch(
            "{\"file\":\"a.ts\",\"old\":\"const cap = 10\",\"new\":\"const cap = 20\"}\n\
             {\"file\":\"./a.ts\",\"old\":\"const floor = 1\",\"new\":\"const floor = 2\"}\n",
        )
        .expect("the JSONL form parses");

        let outcome = apply(&specs, &edit_args("-"), &global());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(read(&a), "const cap = 20\nconst floor = 2\n");
        let text = rendered(&outcome);
        assert!(
            text.contains("1 file \u{b7} 2 edits \u{b7} all applied"),
            "{text}"
        );
    }

    fn multi_target_args(targets: &[&str], old: &str, new: &str) -> EditArgs {
        let mut args = replacing(targets[0], old, new);
        args.more_targets = targets[1..].iter().map(|t| (*t).to_owned()).collect();
        args
    }

    #[test]
    fn several_targets_with_the_same_old_and_new_change_every_file() {
        let Some(dir) = repo() else { return };
        let a = write(&dir, "a.ts", "const cap = 10\n");
        let b = write(&dir, "b.ts", "const cap = 10\n");

        let outcome = run(
            &multi_target_args(&["a.ts", "b.ts"], "const cap = 10", "const cap = 20"),
            &global(),
            Format::Text,
        );

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(read(&a), "const cap = 20\n");
        assert_eq!(read(&b), "const cap = 20\n");
        let text = rendered(&outcome);
        assert!(
            text.contains("2 files \u{b7} 2 edits \u{b7} all applied"),
            "{text}"
        );
    }

    #[test]
    fn one_of_several_targets_missing_the_old_string_writes_none_of_the_batch() {
        let Some(dir) = repo() else { return };
        let a = write(&dir, "a.ts", "const cap = 10\n");
        let b = write(&dir, "b.ts", "no match here\n");

        let outcome = run(
            &multi_target_args(&["a.ts", "b.ts"], "const cap = 10", "const cap = 20"),
            &global(),
            Format::Text,
        );

        let error = error_of(outcome);
        assert_eq!(error.slug(), "not_found");
        assert_eq!(read(&a), "const cap = 10\n");
        assert_eq!(read(&b), "no match here\n");
    }

    #[test]
    fn several_targets_with_if_are_refused_before_any_write() {
        let Some(dir) = repo() else { return };
        let a = write(&dir, "a.ts", "const cap = 10\n");
        let b = write(&dir, "b.ts", "const cap = 10\n");
        let mut args = multi_target_args(&["a.ts", "b.ts"], "const cap = 10", "const cap = 20");
        args.if_sha = Some("sha:000000000000".to_owned());

        let outcome = run(&args, &global(), Format::Text);

        let error = error_of(outcome);
        assert_eq!(error.slug(), "usage");
        assert!(error.to_string().contains("--if"), "{error}");
        assert_eq!(read(&a), "const cap = 10\n");
        assert_eq!(read(&b), "const cap = 10\n");
    }

    #[test]
    fn a_from_dash_with_a_positional_target_is_refused() {
        let mut args = edit_args("a.ts");
        args.from = Some("-".to_owned());

        let error = error_of(run(&args, &global(), Format::Text));

        assert_eq!(error.slug(), "usage");
    }

    #[test]
    fn a_target_named_twice_by_a_different_spelling_is_edited_once() {
        let Some(dir) = repo() else { return };
        let a = write(&dir, "a.ts", "const cap = 10\n");

        let outcome = run(
            &multi_target_args(&["a.ts", "./a.ts"], "const cap = 10", "const cap = 20"),
            &global(),
            Format::Text,
        );

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(read(&a), "const cap = 20\n");
        let text = rendered(&outcome);
        assert!(text.contains("structure ok"), "{text}");
        assert!(!text.contains("all applied"), "{text}");
    }

    #[test]
    fn the_second_edit_on_a_file_matches_what_the_first_one_made_of_it() {
        let Some(dir) = repo() else { return };
        let a = write(&dir, "a.ts", "const cap = 10\n");
        let specs = parse_batch(
            "{\"file\":\"a.ts\",\"old\":\"const cap = 10\",\"new\":\"const cap = 20\"}\n\
             {\"file\":\"a.ts\",\"old\":\"const cap = 20\",\"new\":\"const cap = 30\"}\n",
        )
        .expect("the JSONL form parses");

        let outcome = apply(&specs, &edit_args("-"), &global());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(read(&a), "const cap = 30\n");
    }

    #[test]
    fn an_ambiguous_second_edit_on_one_file_writes_none_of_the_batch() {
        let Some(dir) = repo() else { return };
        let before = "const cap = 10\nconst floor = 1\nconst floor = 1\n";
        let a = write(&dir, "a.ts", before);
        let specs = parse_batch(
            "{\"file\":\"a.ts\",\"old\":\"const cap = 10\",\"new\":\"const cap = 20\"}\n\
             {\"file\":\"a.ts\",\"old\":\"const floor = 1\",\"new\":\"const floor = 2\"}\n",
        )
        .expect("the JSONL form parses");

        let outcome = apply(&specs, &edit_args("-"), &global());

        assert_eq!(read(&a), before, "validation-atomic: nothing is written");
        let text = rendered(&outcome);
        assert!(
            text.contains("0 of 2 applied \u{b7} nothing written \u{b7} fix a.ts and re-run"),
            "{text}"
        );
        assert_eq!(error_of(outcome).slug(), "ambiguous");
    }

    #[test]
    fn an_unterminated_fence_block_is_refused() {
        let error = parse_batch("@@ a.ts\n<<<<<<< old\nconst cap = 10\n").unwrap_err();

        assert_eq!(error.slug(), "usage");
        assert!(
            error.to_string().contains("unterminated `@@` block"),
            "{error}"
        );
    }

    #[test]
    fn one_header_covers_three_blocks_under_it() {
        let specs = parse_batch(
            "@@ a.ts\n\
             <<<<<<< old\n\
             one\n\
             ======= new\n\
             1\n\
             >>>>>>>\n\
             <<<<<<< old\n\
             two\n\
             ======= new\n\
             2\n\
             >>>>>>>\n\
             <<<<<<< old\n\
             three\n\
             ======= new\n\
             3\n\
             >>>>>>>\n",
        )
        .expect("three blocks under one header parse");

        assert_eq!(specs.len(), 3);
        assert!(specs.iter().all(|spec| spec.file == Path::new("a.ts")));
    }

    #[test]
    fn blank_lines_between_a_header_and_its_first_block_are_ignored() {
        let specs = parse_batch(
            "@@ a.ts\n\
             \n\
             \n\
             <<<<<<< old\n\
             cap = 10\n\
             ======= new\n\
             cap = 20\n\
             >>>>>>>\n",
        )
        .expect("blank lines before the first block parse");

        assert_eq!(specs.len(), 1);
    }

    // A second insert-after under one header would race the first for "right after the anchor":
    // whichever the matcher runs second lands closer to it, reversing the written order.
    #[test]
    fn a_second_block_under_one_insert_header_is_refused_as_ambiguous() {
        let error = parse_batch(
            "@@ c.ts insert-after @'^import'\n\
             ======= new\n\
             import a\n\
             >>>>>>>\n\
             ======= new\n\
             import b\n\
             >>>>>>>\n",
        )
        .unwrap_err();

        assert_eq!(error.slug(), "usage");
        assert!(error.to_string().contains("ambiguous"), "{error}");
    }

    #[test]
    fn a_project_checker_that_passes_both_times_lands_the_edit() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", USAGE);
        let mut args = replacing("usage.ts", "const cap = 10", "const cap = 20");
        args.check = Some("true".to_owned());

        let outcome = run(&args, &global(), Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(read(&path).contains("const cap = 20"));
        assert!(rendered(&outcome).contains("check: true ok"), "{outcome:?}");
    }

    // The checker passes only while the old text is there, so the write is what breaks it.
    #[test]
    fn a_project_checker_that_fails_after_the_write_reverts_the_file() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", USAGE);
        let mut args = replacing("usage.ts", "const cap = 10", "const cap = 20");
        args.check = Some("grep -q 'const cap = 10' usage.ts".to_owned());

        let error = error_of(run(&args, &global(), Format::Text));

        assert_eq!(
            read(&path),
            USAGE,
            "the batch is reverted under its own lock"
        );
        assert_eq!(error.slug(), "check_failed");
        assert!(error.to_string().contains("1 file reverted"), "{error}");
    }

    #[test]
    fn a_layer_2_failure_carries_the_checkers_own_first_line() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", USAGE);
        let mut args = replacing("usage.ts", "const cap = 10", "const cap = 20");
        args.check = Some(
            "grep -q 'const cap = 10' usage.ts || { echo 'usage.ts(3,9): cap moved'; echo second; \
             exit 1; }"
                .to_owned(),
        );

        let error = error_of(run(&args, &global(), Format::Text));

        assert_eq!(
            read(&path),
            USAGE,
            "the batch is reverted under its own lock"
        );
        let message = error.to_string();
        assert!(
            message.contains("failed after it: usage.ts(3,9): cap moved \u{b7} 1 file reverted"),
            "{message}"
        );
    }

    // The temp-file probe catches this during validation; exit 8 is left to failures no probe can
    // foresee.
    #[test]
    fn a_batch_whose_second_directory_refuses_new_files_writes_nothing_and_exits_7() {
        let Some(dir) = repo() else { return };
        std::fs::create_dir(dir.path().join("d1")).expect("first target's directory");
        std::fs::create_dir(dir.path().join("d2")).expect("second target's directory");
        let a = write(&dir, "d1/a.ts", "const cap = 10\n");
        let b = write(&dir, "d2/b.ts", "const cap = 10\n");
        let specs = parse_batch(
            "{\"file\":\"d1/a.ts\",\"old\":\"const cap = 10\",\"new\":\"const cap = 20\"}\n\
             {\"file\":\"d2/b.ts\",\"old\":\"const cap = 10\",\"new\":\"const cap = 20\"}\n",
        )
        .expect("the JSONL form parses");
        std::fs::set_permissions(dir.path().join("d2"), Permissions::from_mode(0o500)).unwrap();

        let outcome = apply(&specs, &edit_args("-"), &global());

        std::fs::set_permissions(dir.path().join("d2"), Permissions::from_mode(0o700)).unwrap();

        assert_eq!(read(&a), "const cap = 10\n");
        assert_eq!(read(&b), "const cap = 10\n");
        let text = rendered(&outcome);
        assert!(
            text.contains("0 of 2 applied \u{b7} nothing written \u{b7} fix d2/b.ts and re-run"),
            "{text}"
        );
        assert_eq!(error_of(outcome).slug(), "io_error");
    }

    #[test]
    fn a_batch_whose_directories_accept_new_files_applies_both() {
        let Some(dir) = repo() else { return };
        std::fs::create_dir(dir.path().join("d1")).expect("first target's directory");
        std::fs::create_dir(dir.path().join("d2")).expect("second target's directory");
        let a = write(&dir, "d1/a.ts", "const cap = 10\n");
        let b = write(&dir, "d2/b.ts", "const cap = 10\n");
        let specs = parse_batch(
            "{\"file\":\"d1/a.ts\",\"old\":\"const cap = 10\",\"new\":\"const cap = 20\"}\n\
             {\"file\":\"d2/b.ts\",\"old\":\"const cap = 10\",\"new\":\"const cap = 20\"}\n",
        )
        .expect("the JSONL form parses");

        let outcome = apply(&specs, &edit_args("-"), &global());

        assert!(outcome.error.is_none(), "{outcome:?}");
        assert_eq!(read(&a), "const cap = 20\n");
        assert_eq!(read(&b), "const cap = 20\n");
    }

    fn three_landed_then_checked(dir: &Workdir, sabotage: &str) -> Outcome {
        for name in ["a", "b", "c"] {
            std::fs::create_dir(dir.path().join(name)).expect("target directory");
            write(dir, &format!("{name}/f.ts"), "const cap = 10\n");
        }
        write(
            dir,
            "chk.sh",
            &format!("if [ -e ran ]; then {sabotage}exit 1; fi\n: > ran\n"),
        );
        let specs = parse_batch(
            "{\"file\":\"c/f.ts\",\"old\":\"cap = 10\",\"new\":\"cap = 20\"}\n\
             {\"file\":\"a/f.ts\",\"old\":\"cap = 10\",\"new\":\"cap = 20\"}\n\
             {\"file\":\"b/f.ts\",\"old\":\"cap = 10\",\"new\":\"cap = 20\"}\n",
        )
        .expect("the JSONL form parses");
        let mut args = edit_args("-");
        args.check = Some("sh chk.sh".to_owned());

        let outcome = apply(&specs, &args, &global());

        for name in ["a", "b", "c"] {
            std::fs::set_permissions(dir.path().join(name), Permissions::from_mode(0o700)).unwrap();
        }
        outcome
    }

    #[test]
    fn a_revert_that_cannot_restore_every_file_names_each_files_final_state() {
        let Some(dir) = repo() else { return };

        let outcome = three_landed_then_checked(&dir, "chmod 500 a c; ");

        assert_eq!(read(&dir.path().join("a/f.ts")), "const cap = 20\n");
        assert_eq!(read(&dir.path().join("b/f.ts")), "const cap = 10\n");
        assert_eq!(read(&dir.path().join("c/f.ts")), "const cap = 20\n");
        let text = rendered(&outcome);
        assert!(
            text.contains(
                "`sh chk.sh` failed after 3 files landed \u{b7} restored b/f.ts \u{b7} not \
                 restored c/f.ts, a/f.ts \u{b7} wrote 2 files: c/f.ts, a/f.ts"
            ),
            "{text}"
        );
        let error = error_of(outcome);
        assert_eq!(error.slug(), "partial_batch");
        assert!(
            error.to_string().contains(
                "could not restore c/f.ts (permission denied), a/f.ts (permission denied)"
            ),
            "{error}"
        );
    }

    #[test]
    fn a_revert_that_restores_every_file_exits_check_failed() {
        let Some(dir) = repo() else { return };

        let outcome = three_landed_then_checked(&dir, "");

        for name in ["a", "b", "c"] {
            assert_eq!(
                read(&dir.path().join(format!("{name}/f.ts"))),
                "const cap = 10\n"
            );
        }
        let error = error_of(outcome);
        assert_eq!(error.slug(), "check_failed");
        assert!(error.to_string().contains("3 files reverted"), "{error}");
    }

    #[test]
    fn all_across_a_pre_existing_error_keeps_the_edit_and_counts_the_error() {
        let Some(dir) = repo() else { return };
        let before = "const A: usize = usageCap;\nfn broken( {\n}\nconst B: usize = usageCap;\n";
        let path = write(&dir, "a.rs", before);
        let mut args = replacing("a.rs", "usageCap", "usageLimit");
        args.all = true;

        let outcome = run(&args, &global(), Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(read(&path), before.replace("usageCap", "usageLimit"));
        assert!(
            rendered(&outcome)
                .contains("check: structure ok in edited region (1 pre-existing error elsewhere)"),
            "{}",
            rendered(&outcome)
        );
    }

    #[test]
    fn all_whose_replacements_break_the_file_reverts_every_one_of_them() {
        let Some(dir) = repo() else { return };
        let before = "fn a() -> usize { 1 }\nfn b() -> usize { 1 }\n";
        let path = write(&dir, "a.rs", before);
        let mut args = replacing("a.rs", "1 }", "1 )");
        args.all = true;

        let outcome = run(&args, &global(), Format::Text);

        assert_eq!(read(&path), before, "layer 1 runs before any write");
        assert_eq!(error_of(outcome).slug(), "check_failed");
    }

    #[test]
    fn two_matches_further_apart_than_their_context_name_the_lines_between_them() {
        let Some(dir) = repo() else { return };
        let mut before = String::from("const a = usageCap\n");
        for n in 2..=41 {
            writeln!(before, "const x{n} = {n}").unwrap();
        }
        before.push_str("const b = usageCap\n");
        for n in 43..=50 {
            writeln!(before, "const y{n} = {n}").unwrap();
        }
        write(&dir, "a.ts", &before);
        let mut args = replacing("a.ts", "usageCap", "usageLimit");
        args.all = true;

        let outcome = run(&args, &global(), Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let text = rendered(&outcome);
        assert!(text.contains("\u{b7}\t:3-40 not shown"), "{text}");
        assert!(text.contains("\u{b7} :3-40 not shown\n"), "{text}");
        let Body::Edit(results) = &outcome.response.body else {
            panic!("a replacement renders as Body::Edit");
        };
        let region = results[0]
            .region
            .as_ref()
            .expect("a replacement has a region");
        let numbers: Vec<usize> = region
            .lines
            .iter()
            .filter(|line| line.marker != Marker::Gap)
            .map(|line| line.number)
            .collect();
        assert_eq!(region.start, *numbers.first().unwrap());
        assert_eq!(region.end, *numbers.last().unwrap());
        let json = crate::output::render(&outcome.response, Format::Json, &RenderOptions {
            numbers: true,
            quiet: false,
        });
        assert!(json.contains("\"marker\":\"gap\""), "{json}");
        assert!(json.contains("\"region_gap\""), "{json}");
    }

    #[test]
    fn two_matches_inside_one_context_window_render_one_block_with_no_gap() {
        let Some(dir) = repo() else { return };
        write(
            &dir,
            "a.ts",
            "const a = usageCap\nconst x = 1\nconst b = usageCap\n",
        );
        let mut args = replacing("a.ts", "usageCap", "usageLimit");
        args.all = true;

        let outcome = run(&args, &global(), Format::Text);

        let Body::Edit(results) = &outcome.response.body else {
            panic!("a replacement renders as Body::Edit");
        };
        let region = results[0]
            .region
            .as_ref()
            .expect("a replacement has a region");
        assert!(
            region.lines.iter().all(|line| line.marker != Marker::Gap),
            "{region:?}"
        );
        assert!(!rendered(&outcome).contains("not shown"), "{outcome:?}");
    }

    #[test]
    fn inserting_after_the_last_line_of_a_file_with_no_trailing_newline_marks_the_new_line() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "f.txt", "x\ny");
        let mut args = edit_args("f.txt");
        args.insert_after = Some("@y".to_owned());
        args.new = Some("z".to_owned());

        let outcome = run(&args, &global(), Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(std::fs::read(&path).unwrap(), b"x\ny\nz");
        let Body::Edit(results) = &outcome.response.body else {
            panic!("an insert renders as Body::Edit");
        };
        assert_eq!(results[0].lines, vec![3]);
        let region = results[0].region.as_ref().expect("an insert has a region");
        let added: Vec<usize> = region
            .lines
            .iter()
            .filter(|line| line.marker == Marker::Added)
            .map(|line| line.number)
            .collect();
        assert_eq!(added, vec![3]);
    }

    #[test]
    fn inserting_after_the_last_line_of_a_file_that_ends_with_a_newline_marks_the_new_line() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "f.txt", "x\ny\n");
        let mut args = edit_args("f.txt");
        args.insert_after = Some("@y".to_owned());
        args.new = Some("z".to_owned());

        let outcome = run(&args, &global(), Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(std::fs::read(&path).unwrap(), b"x\ny\nz\n");
        let Body::Edit(results) = &outcome.response.body else {
            panic!("an insert renders as Body::Edit");
        };
        assert_eq!(results[0].lines, vec![3]);
    }

    // A lone `\n` in a CRLF file is invisible to assertions on line text, so this reads bytes.
    #[test]
    fn a_multi_line_replacement_in_a_crlf_file_joins_with_crlf() {
        let Some(dir) = repo() else { return };
        let path = write(
            &dir,
            "a.ts",
            "const a = 1\r\nconst cap = 10\r\nconst b = 2\r\n",
        );

        let outcome = run(
            &replacing("a.ts", "const cap = 10", "const cap = 20\nconst floor = 1"),
            &global(),
            Format::Text,
        );

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"const a = 1\r\nconst cap = 20\r\nconst floor = 1\r\nconst b = 2\r\n"
        );
    }

    #[test]
    fn a_multi_line_replacement_in_an_lf_file_joins_with_lf() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "a.ts", "const a = 1\nconst cap = 10\nconst b = 2\n");

        let outcome = run(
            &replacing("a.ts", "const cap = 10", "const cap = 20\nconst floor = 1"),
            &global(),
            Format::Text,
        );

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"const a = 1\nconst cap = 20\nconst floor = 1\nconst b = 2\n"
        );
    }

    #[test]
    fn an_insert_before_the_first_line_of_a_bom_file_lands_after_the_bom() {
        let Some(dir) = repo() else { return };
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"\xEF\xBB\xBFfirst\nsecond\n").expect("test fixture writes");
        let mut args = edit_args("f.txt");
        args.insert_before = Some("@first".to_owned());
        args.new = Some("zero".to_owned());

        let outcome = run(&args, &global(), Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"\xEF\xBB\xBFzero\nfirst\nsecond\n"
        );
    }

    #[test]
    fn an_insert_before_the_first_line_of_a_file_with_no_bom_lands_at_byte_zero() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "f.txt", "first\nsecond\n");
        let mut args = edit_args("f.txt");
        args.insert_before = Some("@first".to_owned());
        args.new = Some("zero".to_owned());

        let outcome = run(&args, &global(), Format::Text);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(std::fs::read(&path).unwrap(), b"zero\nfirst\nsecond\n");
    }

    #[test]
    fn the_jsonl_form_maps_if_normalize_and_insert_before_onto_the_spec() {
        let specs = parse_batch(
            "{\"file\":\"a.md\",\"old\":\"a\",\"new\":\"b\",\"normalize\":true,\"if\":\"sha:0123456789ab\"}\n\
             {\"file\":\"b.ts\",\"insert_before\":\"#usage\",\"new\":\"// doc\"}\n",
        )
        .expect("the JSONL form parses");

        assert!(specs[0].normalize);
        assert_eq!(
            specs[0].if_sha.as_ref().map(Sha12::as_str),
            Some("0123456789ab")
        );
        assert!(
            matches!(&specs[1].op, EditOp::Insert { side: AnchorSide::Before, anchor, .. }
                if anchor == "#usage"),
            "{:?}",
            specs[1].op
        );
    }

    #[test]
    fn a_jsonl_object_that_names_none_of_them_takes_the_defaults() {
        let specs = parse_batch("{\"file\":\"a.ts\",\"old\":\"a\",\"new\":\"b\"}\n")
            .expect("the JSONL form parses");

        assert!(!specs[0].normalize);
        assert!(specs[0].if_sha.is_none());
        assert!(
            matches!(specs[0].op, EditOp::Replace { all: false, .. }),
            "{:?}",
            specs[0].op
        );
    }

    // A misspelled key would silently default "new" to empty and delete the match.
    #[test]
    fn a_jsonl_line_with_an_unknown_key_is_refused_and_writes_nothing() {
        let Some(dir) = repo() else { return };
        let original = "const cap = 10\n";
        let path = write(&dir, "a.ts", original);

        let error = parse_batch(
            "{\"file\":\"a.ts\",\"old\":\"const cap = 10\",\"neww\":\"const cap = 20\"}\n",
        )
        .unwrap_err();

        assert_eq!(error.slug(), "usage");
        assert!(error.to_string().contains("line 1"), "{error}");
        assert_eq!(read(&path), original);
    }

    #[test]
    fn a_jsonl_line_missing_new_is_refused_and_writes_nothing() {
        let Some(dir) = repo() else { return };
        let original = "const cap = 10\n";
        let path = write(&dir, "a.ts", original);

        let error = parse_batch("{\"file\":\"a.ts\",\"old\":\"const cap = 10\"}\n").unwrap_err();

        assert_eq!(error.slug(), "usage");
        assert!(error.to_string().contains("line 1"), "{error}");
        assert_eq!(read(&path), original);
    }

    #[test]
    fn a_jsonl_line_with_a_non_string_new_is_refused() {
        let error =
            parse_batch("{\"file\":\"a.ts\",\"old\":\"const cap = 10\",\"new\":2}\n").unwrap_err();

        assert_eq!(error.slug(), "usage");
        assert!(error.to_string().contains("line 1"), "{error}");
    }

    #[test]
    fn a_jsonl_line_with_both_old_and_insert_after_is_refused() {
        let error = parse_batch(
            "{\"file\":\"a.ts\",\"old\":\"x\",\"insert_after\":\"@'^x'\",\"new\":\"y\"}\n",
        )
        .unwrap_err();

        assert_eq!(error.slug(), "usage");
        assert!(error.to_string().contains("line 1"), "{error}");
    }

    #[test]
    fn two_malformed_jsonl_lines_both_name_their_line() {
        let error = parse_batch(
            "{\"file\":\"a.ts\",\"old\":\"x\",\"neww\":\"y\"}\n\
             {\"file\":\"b.ts\",\"new\":2}\n",
        )
        .unwrap_err();

        assert_eq!(error.slug(), "usage");
        let text = error.to_string();
        assert!(text.contains("line 1"), "{text}");
        assert!(text.contains("line 2"), "{text}");
    }

    #[test]
    fn a_jsonl_if_shorter_than_12_hex_is_refused_as_usage() {
        let error =
            parse_batch("{\"file\":\"a.ts\",\"old\":\"x\",\"new\":\"y\",\"if\":\"sha:abc\"}\n")
                .unwrap_err();

        assert_eq!(error.slug(), "usage");
        assert!(error.to_string().contains("line 1"), "{error}");
    }

    #[test]
    fn a_jsonl_new_of_empty_string_deletes_the_match() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "a.ts", "const cap = 10\n");
        let specs = parse_batch("{\"file\":\"a.ts\",\"old\":\"const cap = 10\\n\",\"new\":\"\"}\n")
            .expect("an empty \"new\" is a legal deletion");

        let outcome = apply(&specs, &edit_args("-"), &global());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(read(&path), "");
    }

    #[test]
    fn a_fenced_block_missing_new_is_refused_and_names_the_header_line() {
        let error = parse_batch("@@ a.ts\n<<<<<<< old\nconst cap = 10\n>>>>>>>\n").unwrap_err();

        assert_eq!(error.slug(), "usage");
        assert!(error.to_string().contains("line 1"), "{error}");
    }

    #[test]
    fn a_fenced_insert_block_with_an_old_section_is_refused() {
        let error = parse_batch(
            "@@ a.ts insert-after @'^x'\n\
             <<<<<<< old\n\
             x\n\
             ======= new\n\
             y\n\
             >>>>>>>\n",
        )
        .unwrap_err();

        assert_eq!(error.slug(), "usage");
        assert!(error.to_string().contains("line 2"), "{error}");
    }

    #[test]
    fn a_fenced_replace_block_with_no_old_section_is_refused() {
        let error = parse_batch("@@ a.ts\n======= new\nconst cap = 20\n>>>>>>>\n").unwrap_err();

        assert_eq!(error.slug(), "usage");
        assert!(error.to_string().contains("line 1"), "{error}");
    }

    #[test]
    fn text_outside_a_fenced_block_is_refused_and_names_its_own_line() {
        let error =
            parse_batch("@@ a.ts\n<<<<<<< old\nx\n======= new\ny\n>>>>>>>\nstray\n").unwrap_err();

        assert_eq!(error.slug(), "usage");
        assert!(error.to_string().contains("line 7"), "{error}");
    }

    #[test]
    fn a_from_input_that_is_neither_json_nor_a_fence_block_is_refused() {
        let error = parse_batch("\n  a.ts --old x --new y\n").unwrap_err();

        assert_eq!(error.slug(), "usage");
        assert!(
            error
                .to_string()
                .contains("expected a JSON object (`{`) or a `@@ <path>` block"),
            "{error}"
        );
    }

    #[test]
    fn a_hardlinked_target_is_refused_and_both_paths_keep_their_bytes() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", USAGE);
        let linked = dir.path().join("also-usage.ts");
        std::fs::hard_link(&path, &linked).expect("hard link");

        let error = error_of(run(
            &replacing("usage.ts", "const cap = 10", "const cap = 20"),
            &global(),
            Format::Text,
        ));

        assert_eq!(read(&path), USAGE);
        assert_eq!(read(&linked), USAGE);
        assert_eq!(error.slug(), "unsupported_file");
    }

    #[test]
    fn the_guessed_span_command_escapes_anchors_counts_and_quotes() {
        let content = "a {\n}\nb\n}\n";

        let command = insert_after_line(Path::new("f.txt"), content, 4, b"x'y");

        assert_eq!(
            command,
            r"lets edit f.txt --insert-after '@'\''^\}$'\''+2' --new 'x'\''y'"
        );
    }

    #[test]
    fn a_unique_last_line_gets_no_occurrence_suffix() {
        let command = insert_after_line(Path::new("src/g.kt"), "val limit = 20\n", 1, b"x");

        assert_eq!(
            command,
            r"lets edit src/g.kt --insert-after '@'\''^val limit = 20$'\''' --new x"
        );
    }
}
