use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::check::{self, CommandVerdict};
use crate::cli::{Global, OpKind, TransformArgs};
use crate::error::{CheckLayer, Error, UnsupportedReason};
use crate::output::{
    Body, CheckResult, Format, Line, Marker, Omission, Region, Response, Sha12, ShaPair,
    TransformFormat, TransformOp, TransformResult, plural_suffix,
};
use crate::transform::{self, Op, Value};
use crate::{Outcome, atomic, batch, fs, lock, window};

const VERB: &str = "transform";

const CONTEXT_LINES: usize = 2;

const DELETED_NOTE: &str = "          (deleted)";

#[derive(Debug)]
struct Target {
    file: PathBuf,
    ops: Vec<Op>,
    if_sha: Option<Sha12>,
}

pub fn run(args: &TransformArgs, global: &Global, _format: Format) -> Outcome {
    match targets(args) {
        Ok(targets) => apply(&targets, args, global),
        Err(error) => Outcome::failed(VERB, error),
    }
}

fn apply(targets: &[Target], args: &TransformArgs, global: &Global) -> Outcome {
    let mut canonical = Vec::with_capacity(targets.len());
    for target in targets {
        match fs::guard_scope(&target.file, global.allow_outside) {
            Ok(path) => canonical.push(path),
            Err(error) => return Outcome::failed(VERB, error),
        }
    }

    let grouped = group(targets, &canonical);
    let mut omitted = Vec::new();
    let mut failures: Vec<PathBuf> = Vec::new();
    let validated =
        batch::lock_and_validate(&grouped, &lock::runtime_dir(), |path, group, _lock| {
            validate(group, path, global, &mut omitted)
                .inspect_err(|_| failures.push(path.to_path_buf()))
        });
    let locked = match validated {
        Ok(locked) => locked,
        Err(errors) => {
            let failure =
                Error::all(errors).expect("lock_and_validate returns Err only with an error");
            return rejected(grouped.len(), failure, &failures, omitted);
        },
    };
    if global.no_check {
        omitted.push(Omission::CheckSkipped {
            reason: "--no-check".to_owned(),
        });
    }

    let files: Vec<PathBuf> = locked.plans.iter().map(|plan| plan.path.clone()).collect();
    let command = args.check.as_deref().filter(|_| !global.no_check);
    let timeout = Duration::from_secs(args.check_timeout);
    let baseline = command.map(|cmd| check::run_command(cmd, &files, timeout));

    let landed = match batch::write_in_order(&locked.plans) {
        Ok(landed) => landed,
        Err(Error::PartialBatch {
            written,
            failed,
            detail,
        }) => {
            let total = locked.plans.len();
            let results = locked
                .plans
                .into_iter()
                .filter(|plan| written.contains(&plan.path))
                .map(|plan| plan.detail)
                .collect();
            let summary = format!(
                "{} of {total} file{} written \u{b7} {} failed",
                written.len(),
                plural_suffix(total),
                failed.display()
            );
            return partial_batch(summary, results, written, failed, detail, omitted);
        },
        Err(error) => return Outcome::failed(VERB, error),
    };

    let mut command_check = None;
    if let (Some(cmd), Some(base)) = (command, baseline.as_ref()) {
        let layer = checker_name(cmd).to_owned();
        match settle(cmd, &files, timeout, base) {
            CommandVerdict::Ok => {
                command_check = Some(CheckResult {
                    layer,
                    status: "ok".to_owned(),
                    errors_before: 0,
                    errors_after: 0,
                });
            },
            CommandVerdict::Failed(excerpt) => {
                return reverted(locked.plans, &landed, cmd, &excerpt, omitted);
            },
            // Only a `0 → non-zero` transition is evidence that this write broke something.
            CommandVerdict::Inconclusive(reason) => {
                omitted.push(Omission::CheckInconclusive { layer, reason });
            },
            CommandVerdict::Skipped(reason) => omitted.push(Omission::CheckSkipped { reason }),
        }
    }

    let mut results: Vec<TransformResult> =
        locked.plans.into_iter().map(|plan| plan.detail).collect();
    let mut response = Response::empty(VERB);
    // One result has one check slot, so layer 2's verdict replaces layer 1's; a batch lists both.
    if results.len() == 1 && command_check.is_some() {
        results[0].check = command_check.take();
    }
    if results.len() != 1 {
        response.footer.summary = summary(&results, command_check.as_ref());
    }
    fill_stats(&mut response, &results);
    response.omitted = omitted;
    response.body = Body::Transform(results);
    Outcome::ok(response)
}

fn settle(
    cmd: &str,
    files: &[PathBuf],
    timeout: Duration,
    baseline: &CommandVerdict,
) -> CommandVerdict {
    match baseline {
        CommandVerdict::Skipped(reason) => CommandVerdict::Skipped(reason.clone()),
        CommandVerdict::Inconclusive(reason) => CommandVerdict::Inconclusive(reason.clone()),
        _ => check::run_command(cmd, files, timeout).against_baseline(baseline),
    }
}

fn checker_name(cmd: &str) -> &str {
    cmd.split_whitespace().next().unwrap_or(cmd)
}

fn partial_batch(
    summary: String,
    results: Vec<TransformResult>,
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
    fill_stats(&mut response, &results);
    response.omitted = omitted;
    response.body = Body::Transform(results);
    Outcome::partial(response, Error::PartialBatch {
        written,
        failed,
        detail,
    })
}

fn reverted(
    plans: Vec<batch::Plan<TransformResult>>,
    landed: &[PathBuf],
    cmd: &str,
    excerpt: &str,
    omitted: Vec<Omission>,
) -> Outcome {
    let reverted = batch::revert(&plans, landed);
    let first = landed.first().cloned().unwrap_or_default();
    if reverted.not_restored.is_empty() {
        let said = if excerpt.is_empty() {
            String::new()
        } else {
            format!(": {excerpt}")
        };
        return Outcome::failed(VERB, Error::CheckFailed {
            path: first,
            layer: CheckLayer::Command,
            detail: format!(
                "`{cmd}` passed before the batch and failed after it{said} \u{b7} {} file{} \
                 reverted",
                landed.len(),
                plural_suffix(landed.len())
            ),
        });
    }
    let written: Vec<PathBuf> = reverted
        .not_restored
        .iter()
        .map(|(path, _)| path.clone())
        .collect();
    let mut summary = format!(
        "`{cmd}` failed after {} file{} landed",
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
    let results = plans
        .into_iter()
        .filter(|plan| written.contains(&plan.path))
        .map(|plan| plan.detail)
        .collect();
    partial_batch(
        summary,
        results,
        written.clone(),
        written[0].clone(),
        format!(
            "`{cmd}` failed after the batch landed and the revert could not restore {}",
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

/// Names every failing file: fixing only the first and re-running would fail on the next.
fn rejected(total: usize, failure: Error, failed: &[PathBuf], omitted: Vec<Omission>) -> Outcome {
    if total == 1 {
        return Outcome::failed(VERB, failure);
    }
    let mut response = Response::empty(VERB);
    response.omitted = omitted;
    response.footer.summary = format!(
        "0 of {total} applied \u{b7} nothing written \u{b7} fix {} and re-run",
        if failed.is_empty() {
            "the failing target".to_owned()
        } else {
            path_list(failed)
        }
    );
    response.body = Body::Transform(Vec::new());
    Outcome::partial(response, failure)
}

fn fill_stats(response: &mut Response, results: &[TransformResult]) {
    let lines = || {
        results
            .iter()
            .filter_map(|result| result.region.as_ref())
            .map(|region| region.lines.as_slice())
    };
    response.stats.lines = lines().map(<[Line]>::len).sum();
    response.stats.bytes = lines().map(window::content_bytes).sum();
}

fn summary(results: &[TransformResult], command: Option<&CheckResult>) -> String {
    let changes: usize = results.iter().map(|result| result.operations.len()).sum();
    let mut out = format!(
        "{} file{} \u{b7} {changes} change{} \u{b7} all applied",
        results.len(),
        plural_suffix(results.len()),
        plural_suffix(changes)
    );
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

/// Sorted by canonical path, the write order, so which files land before a failure never depends
/// on the order the batch listed them.
fn group<'a>(targets: &'a [Target], canonical: &[PathBuf]) -> Vec<(PathBuf, Vec<&'a Target>)> {
    let mut grouped: Vec<(&Path, PathBuf, Vec<&'a Target>)> = Vec::new();
    for (target, key) in targets.iter().zip(canonical) {
        match grouped.iter_mut().find(|(seen, ..)| *seen == key.as_path()) {
            Some((_, _, same)) => same.push(target),
            None => grouped.push((key.as_path(), target.file.clone(), vec![target])),
        }
    }
    grouped.sort_by(|a, b| a.0.cmp(b.0));
    grouped
        .into_iter()
        .map(|(_, typed, targets)| (typed, targets))
        .collect()
}

/// Runs under the file's lock, so nothing changes between the `--if` check and the write.
fn validate(
    group: &[&Target],
    path: &Path,
    global: &Global,
    omitted: &mut Vec<Omission>,
) -> Result<batch::Plan<TransformResult>, Error> {
    atomic::check_before_write(path, global.max_file_bytes)?;
    atomic::refuse_unwritable(path)?;
    let file = fs::read(path, global.max_file_bytes)?;
    let format = transform::detect(path, &file.content).ok_or_else(|| Error::Unsupported {
        path: path.to_path_buf(),
        reason: UnsupportedReason::NotStructured,
    })?;
    // Not `atomic::verify_if`, which re-reads: the splice must run on exactly the bytes hashed.
    let actual = atomic::hash12(file.content.as_bytes());
    for target in group {
        if let Some(expected) = target.if_sha.as_ref().filter(|sha| **sha != actual) {
            return Err(Error::Changed {
                path: path.to_path_buf(),
                expected: expected.as_str().to_owned(),
                actual: actual.as_str().to_owned(),
            });
        }
    }

    // One op at a time, so every line number can be carried forward to the written file.
    let mut rows: Vec<Row> = Vec::with_capacity(fs::count_byte(file.content.as_bytes(), b'\n') + 1);
    rows.extend(file.content.lines().map(Row::Kept));
    let mut anchors: Vec<(TransformOp, usize)> = Vec::new();
    // Borrowed until the first op rather than cloned: each document held is up to 8 MiB.
    let mut text: Option<String> = None;
    let mut resolutions: Vec<(String, String)> = Vec::new();
    for op in group.iter().flat_map(|target| target.ops.iter()) {
        let before = text.as_deref().unwrap_or(&file.content);
        let step = apply_format(format, path, before, std::slice::from_ref(op))?;
        let (entry, line) = step
            .touched
            .into_iter()
            .next()
            .expect("an editor reports one line per op it applied");
        if let Some(resolution) = selector_resolution(op, &entry)
            && !resolutions.contains(&resolution)
        {
            resolutions.push(resolution);
        }
        let hunk = Hunk::between(before, &step.text);
        for (_, anchor) in &mut anchors {
            *anchor = hunk.follow(*anchor);
        }
        // A delete's row sits where its removed lines were, just below any line the same op
        // rewrote, such as a parent left as `{}`.
        let anchor = match entry {
            TransformOp::Delete { .. } if hunk.removed > hunk.added => hunk.start + hunk.added + 1,
            _ => line,
        };
        anchors.push((entry, anchor));
        hunk.splice(&mut rows);
        text = Some(step.text);
    }
    let text = text.unwrap_or_else(|| file.content.clone());
    omitted.extend(
        resolutions
            .into_iter()
            .map(|(selector, resolved)| Omission::SelectorResolved { selector, resolved }),
    );
    let mut written: Vec<&str> = Vec::with_capacity(fs::count_byte(text.as_bytes(), b'\n') + 1);
    written.extend(text.lines());
    let mut lines: Vec<usize> = anchors.iter().map(|(_, line)| *line).collect();
    lines.sort_unstable();
    lines.dedup();

    let result = TransformResult {
        path: path.to_path_buf(),
        format,
        region: region(&written, &rows, &lines, omitted),
        operations: anchors.into_iter().map(|(op, _)| op).collect(),
        lines,
        // The editors parse before they mutate and re-emit through the same parser, so reaching
        // here is the validator's own answer for layer 1.
        check: (!global.no_check).then(|| CheckResult {
            layer: format.to_string(),
            status: "ok".to_owned(),
            errors_before: 0,
            errors_after: 0,
        }),
        sha: ShaPair {
            before: actual,
            after: atomic::hash12(text.as_bytes()),
        },
    };
    Ok(batch::Plan {
        path: path.to_path_buf(),
        before: file.content.into_bytes(),
        after: text.into_bytes(),
        mode: None,
        detail: result,
    })
}

/// A `[key=value]` selector is not the key the op wrote, so the footer names both.
fn selector_resolution(op: &Op, entry: &TransformOp) -> Option<(String, String)> {
    let (Op::Set { path, raw_key, .. }
    | Op::Delete { path, raw_key }
    | Op::Append { path, raw_key, .. }) = op;
    let (TransformOp::Set { key } | TransformOp::Delete { key } | TransformOp::Append { key }) =
        entry;
    path.0
        .iter()
        .any(|segment| matches!(segment, transform::Segment::Attr { .. }))
        .then(|| (raw_key.clone(), key.clone()))
}

fn apply_format(
    format: TransformFormat,
    path: &Path,
    content: &str,
    ops: &[Op],
) -> Result<transform::Applied, Error> {
    match format {
        TransformFormat::Json => transform::json::apply(path, content, ops),
        TransformFormat::Yaml => transform::yaml::apply(path, content, ops),
        TransformFormat::Toml => transform::toml::apply(path, content, ops),
        TransformFormat::Frontmatter => transform::frontmatter::apply(path, content, ops),
    }
}

/// `Written` carries the original line it stands in for; a removed line stays as `Gone`, rendered
/// just above the line that now sits where it was.
#[derive(Debug, Clone, Copy)]
enum Row<'a> {
    Kept(&'a str),
    Written(Option<&'a str>),
    Gone(&'a str),
}

/// An editor rewrites one entry, so an op's bytes differ in one run of lines.
#[derive(Debug, Clone, Copy)]
struct Hunk {
    start: usize,
    removed: usize,
    added: usize,
}

impl Hunk {
    /// Walks from both ends: collecting two line vectors of an 8 MiB file raises the peak.
    fn between(before: &str, after: &str) -> Hunk {
        let (before_len, after_len) = (before.lines().count(), after.lines().count());
        let start = before
            .lines()
            .zip(after.lines())
            .take_while(|(b, a)| b == a)
            .count();
        let end = before
            .lines()
            .rev()
            .zip(after.lines().rev())
            .take(before_len.min(after_len) - start)
            .take_while(|(b, a)| b == a)
            .count();
        Hunk {
            start,
            removed: before_len - start - end,
            added: after_len - start - end,
        }
    }

    /// Where a 1-based line of the document before this hunk sits after it. A line the hunk
    /// removed moves to where the removed rows render.
    fn follow(self, line: usize) -> usize {
        let index = line - 1;
        if index < self.start {
            line
        } else if index >= self.start + self.removed {
            line + self.added - self.removed
        } else if index - self.start < self.added {
            line
        } else {
            self.start + self.added + 1
        }
    }

    /// A removed and an added line pair up as a rewrite. A line only an earlier op had added
    /// vanishes unnamed, since the file never held it.
    fn splice(self, rows: &mut Vec<Row<'_>>) {
        let from = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| !matches!(row, Row::Gone(_)))
            .nth(self.start)
            .map_or(rows.len(), |(at, _)| at);
        let mut to = from;
        let mut taken = Vec::with_capacity(self.removed);
        let mut segment = Vec::with_capacity(self.added + self.removed);
        while taken.len() < self.removed {
            match rows[to] {
                Row::Gone(_) => segment.push(rows[to]),
                row => taken.push(row),
            }
            to += 1;
        }
        for pair in 0..self.added {
            segment.push(match taken.get(pair) {
                Some(Row::Kept(was)) => Row::Written(Some(was)),
                Some(Row::Written(was)) => Row::Written(*was),
                _ => Row::Written(None),
            });
        }
        for row in taken.iter().skip(self.added) {
            if let Row::Kept(was) | Row::Written(Some(was)) = row {
                segment.push(Row::Gone(was));
            }
        }
        rows.splice(from..to, segment);
    }
}

/// A deleted row shares its number with the written line now there and renders just above it.
/// `anchors` are shown even where an op left its line's bytes unchanged.
fn region(
    written: &[&str],
    rows: &[Row],
    anchors: &[usize],
    omitted: &mut Vec<Omission>,
) -> Option<Region> {
    let mut changed: Vec<(usize, Marker)> = Vec::new();
    let mut deleted: Vec<(usize, String)> = Vec::new();
    let mut number = 1;
    for row in rows {
        match row {
            Row::Kept(_) => number += 1,
            Row::Written(was) => {
                match was {
                    None => changed.push((number, Marker::Added)),
                    Some(was) if *was == written[number - 1] => {},
                    Some(_) => changed.push((number, Marker::Replaced)),
                }
                number += 1;
            },
            Row::Gone(text) => deleted.push((number, format!("{text}{DELETED_NOTE}"))),
        }
    }

    let total = written.len();
    let mut shown: BTreeSet<usize> = BTreeSet::new();
    for line in changed
        .iter()
        .map(|(line, _)| *line)
        .chain(anchors.iter().copied())
    {
        shown.extend(line.saturating_sub(CONTEXT_LINES).max(1)..=(line + CONTEXT_LINES).min(total));
    }
    for (line, _) in &deleted {
        let below = (line + CONTEXT_LINES - 1).min(total);
        shown.extend(line.saturating_sub(CONTEXT_LINES).max(1)..=below);
    }
    let mut numbers = shown.clone();
    numbers.extend(deleted.iter().map(|(line, _)| *line));
    let (start, end) = (*numbers.first()?, *numbers.last()?);

    let mut not_shown: Vec<(usize, usize)> = Vec::new();
    let mut lines: Vec<Line> = Vec::new();
    let mut previous: Option<usize> = None;
    for number in numbers {
        // A multi-line delete can number its row past the written file's end, and lines absent
        // from the written file are not a gap.
        if let Some(gap) = previous
            .filter(|p| number > p + 1 && *p < total)
            .map(|p| (p + 1, (number - 1).min(total)))
        {
            not_shown.push(gap);
            lines.push(Line {
                number: 0,
                marker: Marker::Gap,
                text: format!(":{}-{} not shown", gap.0, gap.1).into(),
            });
        }
        for (_, text) in deleted.iter().filter(|(line, _)| *line == number) {
            lines.push(Line {
                number,
                marker: Marker::Deleted,
                text: text.clone().into(),
            });
        }
        if shown.contains(&number) {
            let marker = changed
                .iter()
                .find(|(line, _)| *line == number)
                .map_or(Marker::None, |(_, marker)| *marker);
            lines.push(Line {
                number,
                marker,
                text: written[number - 1].to_owned().into(),
            });
        }
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

fn targets(args: &TransformArgs) -> Result<Vec<Target>, Error> {
    match args.from.as_deref() {
        None => single(args).map(|target| vec![target]),
        Some("-") => {
            if args.file.is_some()
                || !args.set.is_empty()
                || !args.delete.is_empty()
                || !args.append.is_empty()
                || args.if_sha.is_some()
            {
                return Err(not_found(
                    "-",
                    "`--from -` alone, with the file, operations and `if` in each JSONL line",
                ));
            }
            parse_batch(&stdin_text()?)
        },
        Some(other) => Err(not_found(
            other,
            "a batch spec (`--from` reads `-`, stdin, only)",
        )),
    }
}

fn single(args: &TransformArgs) -> Result<Target, Error> {
    const ORDER: &str = "`Cli::parse_args` records one kind per operation flag";
    let file = args.file.clone().ok_or_else(|| not_found(VERB, "a file"))?;
    let raw = file.display().to_string();
    let flags = args.set.len() + args.delete.len() + args.append.len();
    assert_eq!(args.order.len(), flags, "{ORDER}");
    let (mut sets, mut deletes, mut appends) =
        (args.set.iter(), args.delete.iter(), args.append.iter());
    let mut ops = Vec::with_capacity(flags);
    for kind in &args.order {
        ops.push(match kind {
            OpKind::Set => set_flag(sets.next().expect(ORDER)),
            OpKind::Delete => delete_op(deletes.next().expect(ORDER)),
            OpKind::Append => append_flag(appends.next().expect(ORDER)),
        }?);
    }
    if ops.is_empty() {
        return Err(not_found(
            &raw,
            "an operation (--set, --delete or --append)",
        ));
    }
    Ok(Target {
        file,
        ops,
        if_sha: parse_if(args.if_sha.as_deref(), &raw)?,
    })
}

fn set_flag(flag: &str) -> Result<Op, Error> {
    let (key, value) =
        transform::split_flag(flag).map_err(|_| not_found(flag, "--set path=value"))?;
    Ok(Op::Set {
        path: transform::parse_path(key)?,
        raw_key: key.to_owned(),
        value: transform::parse_value(value),
    })
}

fn delete_op(key: &str) -> Result<Op, Error> {
    Ok(Op::Delete {
        path: transform::parse_path(key)?,
        raw_key: key.to_owned(),
    })
}

/// A trailing `[]` comes off first: the path grammar has no empty index.
fn append_flag(flag: &str) -> Result<Op, Error> {
    let (key, value) =
        transform::split_flag(flag).map_err(|_| not_found(flag, "--append path=value"))?;
    let key = key.strip_suffix("[]").unwrap_or(key);
    Ok(Op::Append {
        path: transform::parse_path(key)?,
        raw_key: key.to_owned(),
        value: transform::parse_value(value),
    })
}

fn parse_if(raw: Option<&str>, target: &str) -> Result<Option<Sha12>, Error> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    Sha12::parse(raw.strip_prefix("sha:").unwrap_or(raw))
        .map(Some)
        .ok_or_else(|| not_found(target, "--if sha (must be 12+ hex)"))
}

fn not_found(target: &str, what: &str) -> Error {
    Error::NotFound {
        target: target.to_owned(),
        what: what.to_owned(),
        nearest: None,
    }
}

fn malformed(detail: &str) -> Error {
    Error::Usage {
        message: format!("malformed --from - input: {detail}"),
    }
}

fn stdin_text() -> Result<String, Error> {
    let mut raw = Vec::new();
    std::io::stdin()
        .read_to_end(&mut raw)
        .map_err(|source| Error::Io {
            path: PathBuf::from("-"),
            source,
        })?;
    String::from_utf8(raw).map_err(|_| Error::Unsupported {
        path: PathBuf::from("-"),
        reason: UnsupportedReason::NonUtf8Region,
    })
}

/// Within a line the ops run set, delete, append: a JSON object has no typed order to follow.
fn parse_batch(text: &str) -> Result<Vec<Target>, Error> {
    let mut targets = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let number = index + 1;
        let at = |detail: &str| malformed(&format!("line {number}: {detail}"));
        let value: serde_json::Value =
            serde_json::from_str(line).map_err(|error| at(&error.to_string()))?;
        let object = value.as_object().ok_or_else(|| at("not a JSON object"))?;
        if let Some(unknown) = object
            .keys()
            .find(|key| !matches!(key.as_str(), "file" | "set" | "delete" | "append" | "if"))
        {
            return Err(at(&format!("unknown key {unknown:?}")));
        }
        let file = object
            .get("file")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| at("no \"file\""))?;

        let mut ops = Vec::new();
        if let Some(set) = object.get("set") {
            let set = set
                .as_object()
                .ok_or_else(|| at("\"set\" is not an object"))?;
            for (key, value) in set {
                ops.push(Op::Set {
                    path: transform::parse_path(key)?,
                    raw_key: key.clone(),
                    value: Value::Json(value.clone()),
                });
            }
        }
        if let Some(delete) = object.get("delete") {
            let delete = delete
                .as_array()
                .ok_or_else(|| at("\"delete\" is not an array"))?;
            for key in delete {
                let key = key
                    .as_str()
                    .ok_or_else(|| at("a \"delete\" entry is not a string"))?;
                ops.push(delete_op(key)?);
            }
        }
        if let Some(append) = object.get("append") {
            let append = append
                .as_object()
                .ok_or_else(|| at("\"append\" is not an object"))?;
            for (key, value) in append {
                let key = key.strip_suffix("[]").unwrap_or(key);
                ops.push(Op::Append {
                    path: transform::parse_path(key)?,
                    raw_key: key.to_owned(),
                    value: Value::Json(value.clone()),
                });
            }
        }
        if ops.is_empty() {
            return Err(at("no \"set\", \"delete\" or \"append\""));
        }
        targets.push(Target {
            file: PathBuf::from(file),
            ops,
            if_sha: parse_if(object.get("if").and_then(serde_json::Value::as_str), file)?,
        });
    }
    if targets.is_empty() {
        return Err(malformed("no targets"));
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tempfile::TempDir;

    use super::*;
    use crate::output::RenderOptions;
    use crate::own_process::{Workdir, repo};

    const APP: &str = "{\n  \"name\": \"demo\",\n  \"features\": {\n    \"e2e\": true,\n    \
                       \"lint\": true\n  },\n  \"review\": {\n    \"threads\": 2\n  }\n}\n";

    const SETTINGS: &str = "# who may run what\nallow:\n  - git\n  - rg\nlegacy:\n  token: \
                            abc # rotate me\n  keep: 1\n";

    fn write(dir: &Workdir, name: &str, content: &str) -> PathBuf {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("fixture directory");
        }
        std::fs::write(&path, content).expect("test fixture writes");
        path
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).expect("fixture is readable")
    }

    fn args(file: &str) -> TransformArgs {
        TransformArgs {
            file: Some(PathBuf::from(file)),
            set: Vec::new(),
            delete: Vec::new(),
            append: Vec::new(),
            from: None,
            if_sha: None,
            check: None,
            check_timeout: 60,
            order: Vec::new(),
        }
    }

    fn setting(file: &str, sets: &[&str]) -> TransformArgs {
        let flags: Vec<(OpKind, &str)> = sets.iter().map(|set| (OpKind::Set, *set)).collect();
        typed(file, &flags)
    }

    fn typed(file: &str, flags: &[(OpKind, &str)]) -> TransformArgs {
        let mut args = args(file);
        for (kind, flag) in flags {
            let into = match kind {
                OpKind::Set => &mut args.set,
                OpKind::Delete => &mut args.delete,
                OpKind::Append => &mut args.append,
            };
            into.push((*flag).to_owned());
            args.order.push(*kind);
        }
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

    fn transform(args: &TransformArgs, global: &Global) -> Outcome {
        match targets(args) {
            Ok(targets) => without_cost(apply(&targets, args, global)),
            Err(error) => Outcome::failed(VERB, error),
        }
    }

    fn without_cost(mut outcome: Outcome) -> Outcome {
        outcome.response.stats.token_ratio = None;
        outcome
    }

    fn batch(text: &str) -> Outcome {
        let targets = parse_batch(text).expect("the batch parses");
        let mut args = args("-");
        args.file = None;
        args.from = Some("-".to_owned());
        without_cost(apply(&targets, &args, &global()))
    }

    fn rendered(outcome: &Outcome) -> String {
        crate::output::render(&outcome.response, Format::Text, &RenderOptions {
            numbers: true,
            quiet: false,
        })
    }

    fn slug(outcome: &Outcome) -> &'static str {
        outcome.error.as_ref().map_or("ok", Error::slug)
    }

    fn sha_pair(before: &str, after: &str) -> String {
        format!(
            "sha:{}\u{2192}{}",
            atomic::hash12(before.as_bytes()).as_str(),
            atomic::hash12(after.as_bytes()).as_str()
        )
    }

    #[test]
    fn a_set_writes_the_file_and_reports_region_check_and_sha() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "app.json", APP);
        let expected = APP
            .replace("\"e2e\": true", "\"e2e\": false")
            .replace("\"threads\": 2", "\"threads\": 3");

        let outcome = transform(
            &setting("app.json", &["features.e2e=false", "review.threads=3"]),
            &global(),
        );

        assert_eq!(slug(&outcome), "ok");
        assert_eq!(read(&path), expected);
        let text = rendered(&outcome);
        assert!(
            text.starts_with(
                "── app.json · json · set features.e2e, review.threads · lines 4, 8\n"
            ),
            "{text}"
        );
        assert!(text.contains("\n 4~\t    \"e2e\": false,\n"), "{text}");
        assert!(text.contains("\n 8~\t    \"threads\": 3\n"), "{text}");
        assert!(text.contains("\n 5 \t    \"lint\": true\n"), "{text}");
        assert!(
            text.ends_with(&format!(
                "── check: json ok · {}\n",
                sha_pair(APP, &expected)
            )),
            "{text}"
        );
    }

    #[test]
    fn deleting_a_missing_key_exits_not_found_and_writes_nothing() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "app.json", APP);

        let outcome = transform(
            &typed("app.json", &[(OpKind::Delete, "features.absent")]),
            &global(),
        );

        assert_eq!(slug(&outcome), "not_found");
        assert_eq!(read(&path), APP);
    }

    #[test]
    fn a_target_whose_lock_is_held_is_refused_and_left_alone() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "app.json", APP);
        let held = crate::lock::Lock::acquire(&path, &crate::lock::runtime_dir())
            .expect("the test takes the lock first");

        let outcome = transform(&setting("app.json", &["name=x"]), &global());

        assert_eq!(slug(&outcome), "locked");
        assert_eq!(read(&path), APP);
        drop(held);
        let again = transform(&setting("app.json", &["name=x"]), &global());
        assert_eq!(
            slug(&again),
            "ok",
            "the same call applies once the lock is free"
        );
    }

    #[test]
    fn one_invalid_file_in_a_batch_writes_none_of_them() {
        let Some(dir) = repo() else { return };
        let a = write(&dir, "a.json", "{\n  \"version\": \"1.0.0\"\n}\n");
        let b = write(&dir, "b.json", "{\n  \"version\": \"1.0.0\"\n}\n");

        let outcome = batch(
            "{\"file\":\"a.json\",\"set\":{\"version\":\"2.0.0\"}}\n\
             {\"file\":\"b.json\",\"delete\":[\"absent\"]}\n",
        );

        assert_eq!(slug(&outcome), "not_found");
        assert_eq!(read(&a), "{\n  \"version\": \"1.0.0\"\n}\n");
        assert_eq!(read(&b), "{\n  \"version\": \"1.0.0\"\n}\n");
        assert!(
            rendered(&outcome).contains("0 of 2 applied · nothing written · fix b.json and re-run"),
            "{}",
            rendered(&outcome)
        );
    }

    #[test]
    fn a_valid_batch_writes_every_file_and_sums_the_checks() {
        let Some(dir) = repo() else { return };
        let a = write(&dir, "a.json", "{\n  \"version\": \"1.0.0\"\n}\n");
        let b = write(
            &dir,
            "b.json",
            "{\n  \"metadata\": {\n    \"version\": \"1\"\n  }\n}\n",
        );

        let outcome = batch(
            "{\"file\":\"a.json\",\"set\":{\"version\":\"1.9.0\"}}\n\
             {\"file\":\"b.json\",\"set\":{\"metadata.version\":\"1.48.180\"}}\n",
        );

        assert_eq!(slug(&outcome), "ok");
        assert_eq!(read(&a), "{\n  \"version\": \"1.9.0\"\n}\n");
        assert_eq!(
            read(&b),
            "{\n  \"metadata\": {\n    \"version\": \"1.48.180\"\n  }\n}\n"
        );
        let text = rendered(&outcome);
        assert!(
            text.contains("── a.json · json · set version · line 2\n"),
            "{text}"
        );
        assert!(
            text.contains("── b.json · json · set metadata.version · line 3\n"),
            "{text}"
        );
        assert!(
            text.contains("── 2 files · 2 changes · all applied · checks: json ok ×2"),
            "{text}"
        );
    }

    const TWO_DIRS: &str = "{\"file\":\"d2/b.json\",\"set\":{\"version\":\"2\"}}\n\
                            {\"file\":\"d1/a.json\",\"set\":{\"version\":\"2\"}}\n";

    #[test]
    fn an_unwritable_directory_is_refused_before_any_file_is_written() {
        let Some(dir) = repo() else { return };
        let a = write(&dir, "d1/a.json", "{\n  \"version\": \"1\"\n}\n");
        let b = write(&dir, "d2/b.json", "{\n  \"version\": \"1\"\n}\n");
        std::fs::set_permissions(dir.path().join("d2"), Permissions::from_mode(0o500)).unwrap();

        let outcome = batch(TWO_DIRS);

        std::fs::set_permissions(dir.path().join("d2"), Permissions::from_mode(0o700)).unwrap();
        assert_eq!(slug(&outcome), "io_error");
        assert_eq!(read(&a), "{\n  \"version\": \"1\"\n}\n");
        assert_eq!(read(&b), "{\n  \"version\": \"1\"\n}\n");
    }

    // The baseline `--check` makes `d2` unwritable after validation. The batch lists `d2` first,
    // but sorted write order lands `d1/a.json` before `d2/b.json` fails.
    #[test]
    fn a_later_write_failure_exits_8_names_what_landed_and_leaves_the_rest() {
        let Some(dir) = repo() else { return };
        let a = write(&dir, "d1/a.json", "{\n  \"version\": \"1\"\n}\n");
        let b = write(&dir, "d2/b.json", "{\n  \"version\": \"1\"\n}\n");
        let mut args = args("-");
        args.file = None;
        args.from = Some("-".to_owned());
        args.check = Some("chmod 500 d2".to_owned());

        let targets = parse_batch(TWO_DIRS).expect("the batch parses");
        let outcome = without_cost(apply(&targets, &args, &global()));

        std::fs::set_permissions(dir.path().join("d2"), Permissions::from_mode(0o700)).unwrap();
        assert_eq!(slug(&outcome), "partial_batch");
        assert_eq!(read(&a), "{\n  \"version\": \"2\"\n}\n");
        assert_eq!(read(&b), "{\n  \"version\": \"1\"\n}\n");
        let text = rendered(&outcome);
        assert!(
            text.contains("── d1/a.json · json · set version · line 2\n"),
            "{text}"
        );
        assert!(!text.contains("── d2/b.json ·"), "{text}");
        assert!(
            text.contains("1 of 2 files written · d2/b.json failed"),
            "{text}"
        );
        assert!(text.contains("wrote 1 file: d1/a.json"), "{text}");
    }

    #[test]
    fn a_checker_that_passes_before_and_fails_after_reverts_and_exits_check_failed() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "app.json", APP);
        let mut args = setting("app.json", &["review.threads=3"]);
        args.check = Some("grep -q '\"threads\": 2' app.json".to_owned());

        let outcome = transform(&args, &global());

        assert_eq!(slug(&outcome), "check_failed");
        assert_eq!(read(&path), APP, "reverted under the same lock");
        let message = outcome.error.expect("a failure").to_string();
        assert!(message.contains("1 file reverted"), "{message}");
    }

    #[test]
    fn a_checker_that_fails_before_and_after_keeps_the_write_and_says_inconclusive() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "app.json", APP);
        let mut args = setting("app.json", &["review.threads=3"]);
        args.check = Some("false".to_owned());

        let outcome = transform(&args, &global());

        assert_eq!(slug(&outcome), "ok");
        assert_eq!(read(&path), APP.replace("\"threads\": 2", "\"threads\": 3"));
        let text = rendered(&outcome);
        assert!(
            text.contains("check: false inconclusive (failed before and after)"),
            "{text}"
        );
    }

    #[test]
    fn a_checker_that_passes_both_times_is_the_reported_check() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "app.json", APP);
        let mut args = setting("app.json", &["review.threads=3"]);
        args.check = Some("true --ignored-arg".to_owned());

        let outcome = transform(&args, &global());

        assert_eq!(slug(&outcome), "ok");
        assert!(read(&path).contains("\"threads\": 3"));
        let text = rendered(&outcome);
        assert!(text.contains("── check: true ok · sha:"), "{text}");
        assert!(!text.contains("json ok"), "{text}");
    }

    #[test]
    fn a_target_outside_the_tree_exits_6_before_it_is_read() {
        let Some(_dir) = repo() else { return };
        let elsewhere = TempDir::new().expect("a second temp dir");
        let outside = elsewhere.path().join("app.json");
        std::fs::write(&outside, APP).unwrap();
        let missing = elsewhere.path().join("missing.json");

        let refused = transform(
            &setting(&outside.display().to_string(), &["name=x"]),
            &global(),
        );
        let unread = transform(
            &setting(&missing.display().to_string(), &["name=x"]),
            &global(),
        );

        assert_eq!(slug(&refused), "outside_tree");
        assert_eq!(read(&outside), APP);
        assert_eq!(slug(&unread), "outside_tree");
    }

    #[test]
    fn a_target_outside_the_tree_applies_with_allow_outside() {
        let Some(_dir) = repo() else { return };
        let elsewhere = TempDir::new().expect("a second temp dir");
        let outside = elsewhere.path().join("app.json");
        std::fs::write(&outside, APP).unwrap();
        let mut global = global();
        global.allow_outside = true;

        let outcome = transform(
            &setting(&outside.display().to_string(), &["name=x"]),
            &global,
        );

        assert_eq!(slug(&outcome), "ok");
        assert!(read(&outside).contains("\"name\": \"x\""));
    }

    #[test]
    fn a_delete_renders_the_original_line_marked_deleted() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "settings.yaml", SETTINGS);
        let args = typed("settings.yaml", &[
            (OpKind::Append, "allow[]=gh"),
            (OpKind::Delete, "legacy.token"),
        ]);

        let outcome = transform(&args, &global());

        assert_eq!(slug(&outcome), "ok");
        let after = read(&path);
        assert!(after.contains("  - gh\n"), "{after}");
        assert!(!after.contains("token"), "{after}");
        assert!(after.starts_with("# who may run what\n"), "{after}");
        let Body::Transform(results) = &outcome.response.body else {
            panic!("a transform body")
        };
        let deleted: Vec<&Line> = results[0]
            .region
            .as_ref()
            .expect("a region")
            .lines
            .iter()
            .filter(|line| line.marker == Marker::Deleted)
            .collect();
        assert_eq!(deleted.len(), 1);
        assert_eq!(
            deleted[0].text,
            "  token: abc # rotate me          (deleted)"
        );
        let text = rendered(&outcome);
        assert!(
            text.contains("-\t  token: abc # rotate me          (deleted)\n"),
            "{text}"
        );
        assert!(text.contains("+\t  - gh\n"), "{text}");
    }

    #[test]
    fn a_set_renders_the_new_line_with_no_deleted_annotation() {
        let Some(dir) = repo() else { return };
        write(&dir, "settings.yaml", SETTINGS);

        let outcome = transform(&setting("settings.yaml", &["legacy.keep=2"]), &global());

        assert_eq!(slug(&outcome), "ok");
        let text = rendered(&outcome);
        assert!(text.contains("7~\t  keep: 2\n"), "{text}");
        assert!(!text.contains("(deleted)"), "{text}");
    }

    #[test]
    fn a_block_deleted_at_the_end_names_every_removed_line_and_no_gap() {
        let Some(dir) = repo() else { return };
        let before = "allow:\n  - ls\nlegacy:\n  token: x\n  keep: 1\n";
        write(&dir, "s.yaml", before);

        let outcome = transform(&typed("s.yaml", &[(OpKind::Delete, "legacy")]), &global());

        assert_eq!(slug(&outcome), "ok");
        assert_eq!(
            rendered(&outcome),
            format!(
                "── s.yaml · yaml · delete legacy · line 3\n\
                 1 \tallow:\n\
                 2 \t  - ls\n\
                 3-\tlegacy:          (deleted)\n\
                 3-\t  token: x          (deleted)\n\
                 3-\t  keep: 1          (deleted)\n\
                 ── check: yaml ok · {}\n",
                sha_pair(before, "allow:\n  - ls\n")
            )
        );
    }

    #[test]
    fn two_changes_further_apart_than_their_context_name_the_gap() {
        let after = (1..=12).fold(String::new(), |mut after, n| {
            writeln!(after, "k{n}: {n}").unwrap();
            after
        });
        let written: Vec<&str> = after.lines().collect();
        let mut rows: Vec<Row> = written.iter().map(|line| Row::Kept(line)).collect();
        rows[0] = Row::Written(Some("k1: 0"));
        rows[11] = Row::Written(Some("k12: 0"));
        let mut omitted = Vec::new();

        region(&written, &rows, &[1, 12], &mut omitted).expect("a region");

        assert!(
            matches!(omitted.as_slice(), [Omission::RegionGap { not_shown, .. }] if not_shown == &[(4, 9)]),
            "{omitted:?}"
        );
    }

    #[test]
    fn an_if_sha_that_does_not_match_exits_changed_and_writes_nothing() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "app.json", APP);
        let mut args = setting("app.json", &["name=x"]);
        args.if_sha = Some("sha:000000000000".to_owned());

        let outcome = transform(&args, &global());

        assert_eq!(slug(&outcome), "changed");
        assert_eq!(read(&path), APP);
    }

    #[test]
    fn an_if_sha_that_matches_applies() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "app.json", APP);
        let mut args = setting("app.json", &["name=x"]);
        args.if_sha = Some(format!("sha:{}", atomic::hash12(APP.as_bytes()).as_str()));

        let outcome = transform(&args, &global());

        assert_eq!(slug(&outcome), "ok");
        assert!(read(&path).contains("\"name\": \"x\""));
    }

    #[test]
    fn an_if_that_is_not_12_hex_is_refused_before_anything_is_read() {
        let Some(_dir) = repo() else { return };
        let mut args = setting("app.json", &["name=x"]);
        args.if_sha = Some("sha:abc".to_owned());

        let outcome = transform(&args, &global());

        assert_eq!(slug(&outcome), "not_found");
        let message = outcome.error.expect("a failure").to_string();
        assert!(message.contains("--if sha (must be 12+ hex)"), "{message}");
    }

    #[test]
    fn an_unrecognised_extension_is_unsupported() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "notes.txt", "name: demo\n");

        let outcome = transform(&setting("notes.txt", &["name=x"]), &global());

        assert_eq!(slug(&outcome), "unsupported_file");
        assert_eq!(read(&path), "name: demo\n");
    }

    #[test]
    fn a_text_file_in_no_structured_format_is_refused_as_that_not_as_binary() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "usage.ts", "export const cap = 10;\n");

        let outcome = transform(&setting("usage.ts", &["a=1"]), &global());

        assert_eq!(slug(&outcome), "unsupported_file");
        let message = outcome.error.expect("a failure").to_string();
        assert_eq!(
            message,
            "usage.ts is unsupported: not JSON, YAML, TOML or Markdown with frontmatter"
        );
        assert_eq!(read(&path), "export const cap = 10;\n");
    }

    #[test]
    fn a_toml_null_is_not_found_and_writes_nothing() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "app.toml", "[server]\nport = 8080\n");

        let outcome = transform(&setting("app.toml", &["server.port=null"]), &global());

        assert_eq!(slug(&outcome), "not_found");
        assert_eq!(read(&path), "[server]\nport = 8080\n");
    }

    #[test]
    fn a_toml_set_lands_and_names_its_layer() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "app.toml", "[server]\nport = 8080\n");

        let outcome = transform(&setting("app.toml", &["server.port=9090"]), &global());

        assert_eq!(slug(&outcome), "ok");
        assert_eq!(read(&path), "[server]\nport = 9090\n");
        assert!(rendered(&outcome).contains("── check: toml ok · sha:"));
    }

    #[test]
    fn the_documented_append_form_and_the_bracket_form_write_the_same_bytes() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "settings.yaml", SETTINGS);
        let bare = transform(
            &typed("settings.yaml", &[(OpKind::Append, "allow=gh")]),
            &global(),
        );
        assert_eq!(slug(&bare), "ok", "{:?}", bare.error);
        let after_bare = read(&path);

        write(&dir, "settings.yaml", SETTINGS);
        let bracket = transform(
            &typed("settings.yaml", &[(OpKind::Append, "allow[]=gh")]),
            &global(),
        );

        assert_eq!(slug(&bracket), "ok", "{:?}", bracket.error);
        assert_eq!(after_bare, read(&path));
        assert_eq!(
            after_bare,
            "# who may run what\nallow:\n  - git\n  - rg\n  - gh\nlegacy:\n  token: abc # rotate \
             me\n  keep: 1\n"
        );
    }

    #[test]
    fn appending_to_a_mapping_is_refused_and_writes_nothing() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "settings.yaml", SETTINGS);

        let outcome = transform(
            &typed("settings.yaml", &[(OpKind::Append, "legacy=gh")]),
            &global(),
        );

        assert_eq!(slug(&outcome), "not_found");
        assert_eq!(read(&path), SETTINGS);
        let message = outcome.error.expect("a failure").to_string();
        assert!(message.contains("not a sequence"), "{message}");
        assert!(!message.contains("Route"), "no debug text: {message}");
    }

    #[test]
    fn no_operation_at_all_is_refused() {
        let Some(_dir) = repo() else { return };
        assert_eq!(slug(&transform(&args("app.json"), &global())), "not_found");
    }

    #[test]
    fn a_batch_line_with_an_unknown_key_is_refused() {
        let error = parse_batch("{\"file\":\"a.json\",\"sett\":{\"a\":1}}\n").unwrap_err();
        assert!(
            error.to_string().contains("unknown key \"sett\""),
            "{error}"
        );
    }

    #[test]
    fn from_stdin_beside_a_file_is_refused() {
        let mut args = args("a.json");
        args.from = Some("-".to_owned());
        let error = targets(&args).unwrap_err();
        assert_eq!(error.slug(), "not_found");
    }

    #[test]
    fn operations_run_in_the_order_they_were_typed() {
        let Some(dir) = repo() else { return };
        write(&dir, "settings.yaml", SETTINGS);
        let append_first = typed("settings.yaml", &[
            (OpKind::Append, "allow[]=gh"),
            (OpKind::Delete, "legacy.token"),
        ]);
        let text = rendered(&transform(&append_first, &global()));
        assert!(
            text.starts_with("── settings.yaml · yaml · append allow, delete legacy.token ·"),
            "{text}"
        );

        write(&dir, "settings.yaml", SETTINGS);
        let delete_first = typed("settings.yaml", &[
            (OpKind::Delete, "legacy.token"),
            (OpKind::Append, "allow[]=gh"),
        ]);
        let text = rendered(&transform(&delete_first, &global()));
        assert!(
            text.starts_with("── settings.yaml · yaml · delete legacy.token, append allow ·"),
            "{text}"
        );
    }

    // Typed order decides: set, set, delete would leave no `b` at all.
    #[test]
    fn a_kind_repeated_around_another_runs_in_typed_order() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "o.yaml", "a: 0\nb: 1\nc: 3\n");

        let outcome = transform(
            &typed("o.yaml", &[
                (OpKind::Set, "a=1"),
                (OpKind::Delete, "b"),
                (OpKind::Set, "b=2"),
            ]),
            &global(),
        );

        assert_eq!(slug(&outcome), "ok");
        assert_eq!(read(&path), "a: 1\nc: 3\nb: 2\n");
    }

    fn body_of(name: &str, before: &str, flags: &[(OpKind, &str)], after: &str) -> Option<String> {
        let dir = repo()?;
        let path = write(&dir, name, before);
        let outcome = transform(&typed(name, flags), &global());
        assert_eq!(slug(&outcome), "ok");
        assert_eq!(read(&path), after);
        let text = rendered(&outcome);
        let footer = text.rfind("── check:").expect("a check footer");
        Some(text[..footer].to_owned())
    }

    const ABC_JSON: &str = "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}\n";

    #[test]
    fn a_set_followed_by_a_delete_above_it_marks_the_line_where_it_ends_up_json() {
        let Some(body) = body_of(
            "o.json",
            ABC_JSON,
            &[(OpKind::Set, "c=9"), (OpKind::Delete, "a")],
            "{\n  \"b\": 2,\n  \"c\": 9\n}\n",
        ) else {
            return;
        };
        assert_eq!(
            body,
            "── o.json · json · set c, delete a · lines 2, 3\n\
             1 \t{\n\
             2-\t  \"a\": 1,          (deleted)\n\
             2 \t  \"b\": 2,\n\
             3~\t  \"c\": 9\n\
             4 \t}\n"
        );
    }

    #[test]
    fn a_delete_followed_by_a_set_below_it_marks_the_same_line_json() {
        let Some(body) = body_of(
            "o.json",
            ABC_JSON,
            &[(OpKind::Delete, "a"), (OpKind::Set, "c=9")],
            "{\n  \"b\": 2,\n  \"c\": 9\n}\n",
        ) else {
            return;
        };
        assert_eq!(
            body,
            "── o.json · json · delete a, set c · lines 2, 3\n\
             1 \t{\n\
             2-\t  \"a\": 1,          (deleted)\n\
             2 \t  \"b\": 2,\n\
             3~\t  \"c\": 9\n\
             4 \t}\n"
        );
    }

    #[test]
    fn a_set_followed_by_a_delete_above_it_marks_the_line_where_it_ends_up_yaml() {
        let Some(body) = body_of(
            "o.yaml",
            "a: 1\nb: 2\nc: 3\n",
            &[(OpKind::Set, "c=9"), (OpKind::Delete, "a")],
            "b: 2\nc: 9\n",
        ) else {
            return;
        };
        assert_eq!(
            body,
            "── o.yaml · yaml · set c, delete a · lines 1, 2\n\
             1-\ta: 1          (deleted)\n\
             1 \tb: 2\n\
             2~\tc: 9\n"
        );
    }

    #[test]
    fn a_set_followed_by_a_delete_above_it_marks_the_line_where_it_ends_up_toml() {
        let Some(body) = body_of(
            "o.toml",
            "a = 1\nb = 2\nc = 3\n",
            &[(OpKind::Set, "c=9"), (OpKind::Delete, "a")],
            "b = 2\nc = 9\n",
        ) else {
            return;
        };
        assert_eq!(
            body,
            "── o.toml · toml · set c, delete a · lines 1, 2\n\
             1-\ta = 1          (deleted)\n\
             1 \tb = 2\n\
             2~\tc = 9\n"
        );
    }

    #[test]
    fn three_ops_each_land_on_the_written_files_line_json() {
        let Some(body) = body_of(
            "o.json",
            ABC_JSON,
            &[
                (OpKind::Set, "c=9"),
                (OpKind::Delete, "a"),
                (OpKind::Delete, "b"),
            ],
            "{\n  \"c\": 9\n}\n",
        ) else {
            return;
        };
        assert_eq!(
            body,
            "── o.json · json · set c, delete a, b · line 2\n\
             1 \t{\n\
             2-\t  \"a\": 1,          (deleted)\n\
             2-\t  \"b\": 2,          (deleted)\n\
             2~\t  \"c\": 9\n\
             3 \t}\n"
        );
    }

    #[test]
    fn three_ops_each_land_on_the_written_files_line_yaml() {
        let Some(body) = body_of(
            "o.yaml",
            "a: 1\nb: 2\nc: 3\n",
            &[
                (OpKind::Set, "c=9"),
                (OpKind::Delete, "a"),
                (OpKind::Delete, "b"),
            ],
            "c: 9\n",
        ) else {
            return;
        };
        assert_eq!(
            body,
            "── o.yaml · yaml · set c, delete a, b · line 1\n\
             1-\ta: 1          (deleted)\n\
             1-\tb: 2          (deleted)\n\
             1~\tc: 9\n"
        );
    }

    #[test]
    fn three_ops_each_land_on_the_written_files_line_toml() {
        let Some(body) = body_of(
            "o.toml",
            "a = 1\nb = 2\nc = 3\n",
            &[
                (OpKind::Set, "c=9"),
                (OpKind::Delete, "a"),
                (OpKind::Delete, "b"),
            ],
            "c = 9\n",
        ) else {
            return;
        };
        assert_eq!(
            body,
            "── o.toml · toml · set c, delete a, b · line 1\n\
             1-\ta = 1          (deleted)\n\
             1-\tb = 2          (deleted)\n\
             1~\tc = 9\n"
        );
    }

    #[test]
    fn a_key_set_then_deleted_shows_only_its_original_line_deleted() {
        let Some(body) = body_of(
            "o.yaml",
            "a: 1\nb: 2\n",
            &[(OpKind::Set, "a=3"), (OpKind::Delete, "a")],
            "b: 2\n",
        ) else {
            return;
        };
        assert_eq!(
            body,
            "── o.yaml · yaml · set a, delete a · line 1\n\
             1-\ta: 1          (deleted)\n\
             1 \tb: 2\n"
        );
    }

    #[test]
    fn deleting_a_yaml_keys_only_child_marks_the_parent_rewritten() {
        let before = "allow:\n  - ls\nlegacy:\n  token: abc123 # remove me\n";
        let Some(body) = body_of(
            "settings.yaml",
            before,
            &[(OpKind::Delete, "legacy.token")],
            "allow:\n  - ls\nlegacy: {}\n",
        ) else {
            return;
        };
        assert_eq!(
            body,
            "── settings.yaml · yaml · delete legacy.token · line 4\n\
             1 \tallow:\n\
             2 \t  - ls\n\
             3~\tlegacy: {}\n\
             4-\t  token: abc123 # remove me          (deleted)\n"
        );
    }

    #[test]
    fn deleting_a_json_keys_only_child_marks_the_collapsed_parent() {
        let Some(body) = body_of(
            "o.json",
            "{\n  \"legacy\": {\n    \"token\": \"x\"\n  },\n  \"b\": 1\n}\n",
            &[(OpKind::Delete, "legacy.token")],
            "{\n  \"legacy\": {},\n  \"b\": 1\n}\n",
        ) else {
            return;
        };
        assert_eq!(
            body,
            "── o.json · json · delete legacy.token · line 3\n\
             1 \t{\n\
             2~\t  \"legacy\": {},\n\
             3-\t    \"token\": \"x\"          (deleted)\n\
             3-\t  },          (deleted)\n\
             3 \t  \"b\": 1\n\
             4 \t}\n"
        );
    }

    /// The checker passes once, then fails; `sabotage` runs first so the revert can fail.
    fn three_landed_then_checked(dir: &Workdir, sabotage: &str) -> Outcome {
        for name in ["a", "b", "c"] {
            write(dir, &format!("{name}/x.json"), "{\n  \"v\": 1\n}\n");
        }
        write(
            dir,
            "chk.sh",
            &format!("if [ -e ran ]; then {sabotage}exit 1; fi\n: > ran\n"),
        );
        let targets = parse_batch(
            "{\"file\":\"c/x.json\",\"set\":{\"v\":2}}\n\
             {\"file\":\"a/x.json\",\"set\":{\"v\":2}}\n\
             {\"file\":\"b/x.json\",\"set\":{\"v\":2}}\n",
        )
        .expect("the batch parses");
        let mut args = args("-");
        args.file = None;
        args.from = Some("-".to_owned());
        args.check = Some("sh chk.sh".to_owned());

        let outcome = without_cost(apply(&targets, &args, &global()));

        for name in ["a", "b", "c"] {
            std::fs::set_permissions(dir.path().join(name), Permissions::from_mode(0o700)).unwrap();
        }
        outcome
    }

    #[test]
    fn a_revert_that_cannot_restore_every_file_names_each_files_final_state() {
        let Some(dir) = repo() else { return };

        let outcome = three_landed_then_checked(&dir, "chmod 500 a c; ");

        assert_eq!(read(&dir.path().join("a/x.json")), "{\n  \"v\": 2\n}\n");
        assert_eq!(read(&dir.path().join("b/x.json")), "{\n  \"v\": 1\n}\n");
        assert_eq!(read(&dir.path().join("c/x.json")), "{\n  \"v\": 2\n}\n");
        let text = rendered(&outcome);
        assert!(
            text.contains(
                "── `sh chk.sh` failed after 3 files landed · restored b/x.json · not restored \
                 a/x.json, c/x.json · wrote 2 files: a/x.json, c/x.json"
            ),
            "{text}"
        );
        assert!(
            text.contains("── a/x.json · json · set v · line 2\n"),
            "{text}"
        );
        assert!(
            text.contains("── c/x.json · json · set v · line 2\n"),
            "{text}"
        );
        assert!(!text.contains("── b/x.json"), "{text}");
        let error = outcome.error.expect("a failure");
        assert_eq!(error.slug(), "partial_batch");
        assert!(
            error.to_string().contains(
                "could not restore a/x.json (permission denied), c/x.json (permission denied)"
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
                read(&dir.path().join(format!("{name}/x.json"))),
                "{\n  \"v\": 1\n}\n"
            );
        }
        let error = outcome.error.expect("a failure");
        assert_eq!(error.slug(), "check_failed");
        assert!(error.to_string().contains("3 files reverted"), "{error}");
    }

    #[test]
    fn a_rejected_batch_names_every_failing_file() {
        let Some(dir) = repo() else { return };
        let json = "{\n  \"v\": 1\n}\n";
        for name in ["a.json", "b.json", "c.json"] {
            write(&dir, name, json);
        }

        let outcome = batch(
            "{\"file\":\"a.json\",\"delete\":[\"absent\"]}\n\
             {\"file\":\"b.json\",\"set\":{\"v\":2}}\n\
             {\"file\":\"c.json\",\"set\":{\"v\":2},\"if\":\"sha:000000000000\"}\n",
        );

        assert_eq!(slug(&outcome), "not_found");
        let text = rendered(&outcome);
        assert!(
            text.contains("0 of 3 applied · nothing written · fix a.json, c.json and re-run"),
            "{text}"
        );
        for name in ["a.json", "b.json", "c.json"] {
            assert_eq!(read(&dir.path().join(name)), json);
        }
    }

    #[test]
    fn a_later_file_that_does_not_parse_writes_none_of_the_batch() {
        let Some(dir) = repo() else { return };
        let a = write(&dir, "a.json", "{\n  \"v\": 1\n}\n");
        write(&dir, "b.json", "{\n  \"v\": \n");

        let outcome = batch(
            "{\"file\":\"a.json\",\"set\":{\"v\":2}}\n\
             {\"file\":\"b.json\",\"set\":{\"v\":2}}\n",
        );

        assert_eq!(slug(&outcome), "check_failed");
        assert_eq!(read(&a), "{\n  \"v\": 1\n}\n");
        assert!(
            rendered(&outcome).contains("0 of 2 applied · nothing written · fix b.json and re-run"),
            "{}",
            rendered(&outcome)
        );
    }

    const PLUGINS: &str = "{\n  \"plugins\": [\n    {\"name\": \"alpha\", \"version\": \"1.0\"},\n    \
                           {\"name\": \"beta\", \"version\": \"1.1\"},\n    {\"name\": \"gitty\", \
                           \"version\": \"0.9\"}\n  ]\n}\n";

    fn selector_pairs(outcome: &Outcome) -> Vec<(&str, &str)> {
        outcome
            .response
            .omitted
            .iter()
            .filter_map(|omission| match omission {
                Omission::SelectorResolved { selector, resolved } => {
                    Some((selector.as_str(), resolved.as_str()))
                },
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_selector_names_its_resolved_index_in_the_footer_and_the_json() {
        let Some(dir) = repo() else { return };
        let path = write(&dir, "plugins.json", PLUGINS);

        let outcome = transform(
            &setting("plugins.json", &["plugins[name=gitty].version=2.0"]),
            &global(),
        );

        assert_eq!(slug(&outcome), "ok");
        assert_eq!(
            read(&path),
            PLUGINS.replace("\"version\": \"0.9\"", "\"version\": \"2.0\"")
        );
        assert_eq!(selector_pairs(&outcome), [(
            "plugins[name=gitty].version",
            "plugins[2].version"
        )]);
        let text = rendered(&outcome);
        assert!(
            text.contains("plugins[name=gitty].version \u{2192} plugins[2].version"),
            "{text}"
        );
        let json = crate::output::render(&outcome.response, Format::Json, &RenderOptions {
            numbers: true,
            quiet: false,
        });
        assert!(
            json.contains(
                "\"selector_resolved\":{\"selector\":\"plugins[name=gitty].version\",\
                 \"resolved\":\"plugins[2].version\"}"
            ),
            "{json}"
        );
    }

    #[test]
    fn the_same_selector_twice_is_named_once() {
        let Some(dir) = repo() else { return };
        write(&dir, "plugins.json", PLUGINS);

        let outcome = transform(
            &setting("plugins.json", &[
                "plugins[name=gitty].version=2.0",
                "plugins[name=gitty].version=2.1",
                "plugins[name=beta].version=1.2",
            ]),
            &global(),
        );

        assert_eq!(slug(&outcome), "ok");
        assert_eq!(selector_pairs(&outcome), [
            ("plugins[name=gitty].version", "plugins[2].version"),
            ("plugins[name=beta].version", "plugins[1].version"),
        ]);
    }

    #[test]
    fn a_path_without_a_selector_names_no_resolution() {
        let Some(dir) = repo() else { return };
        write(&dir, "plugins.json", PLUGINS);

        let outcome = transform(
            &setting("plugins.json", &["plugins[2].version=2.0"]),
            &global(),
        );

        assert_eq!(slug(&outcome), "ok");
        assert!(selector_pairs(&outcome).is_empty());
        let text = rendered(&outcome);
        assert!(!text.contains("version \u{2192}"), "{text}");
    }

    #[test]
    fn malformed_from_stdin_input_is_a_usage_error_naming_the_line() {
        for (input, line) in [
            (
                "{\"file\":\"a.json\",\"set\":{\"v\":2}}\nnot json\n",
                "line 2: ",
            ),
            ("[1]\n", "line 1: not a JSON object"),
            ("{\"set\":{\"v\":2}}\n", "line 1: no \"file\""),
            (
                "{\"file\":\"a.json\",\"sett\":{\"v\":2}}\n",
                "line 1: unknown key \"sett\"",
            ),
            (
                "{\"file\":\"a.json\",\"set\":[1]}\n",
                "line 1: \"set\" is not an object",
            ),
            (
                "{\"file\":\"a.json\"}\n",
                "line 1: no \"set\", \"delete\" or \"append\"",
            ),
            ("\n\n", "no targets"),
        ] {
            let error = parse_batch(input).unwrap_err();
            assert_eq!(error.slug(), "usage", "{input:?}");
            assert!(error.to_string().contains(line), "{input:?}: {error}");
        }
    }

    #[test]
    fn well_formed_from_stdin_input_still_parses() {
        let targets = parse_batch("{\"file\":\"a.json\",\"set\":{\"v\":2},\"delete\":[\"w\"]}\n")
            .expect("a well-formed batch parses");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].file, PathBuf::from("a.json"));
        assert_eq!(targets[0].ops.len(), 2);
    }

    static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
    static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

    /// Measurement only: the shipped binary keeps the default allocator.
    struct Counting;

    fn grew(by: usize) {
        let live = LIVE_BYTES.fetch_add(by, Ordering::Relaxed) + by;
        PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
    }

    // SAFETY: every method forwards its arguments to `System` unchanged and returns its result,
    // so each `GlobalAlloc` obligation is `System`'s own; the counters touch no allocated memory.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            grew(layout.size());
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            grew(layout.size());
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            if new_size >= layout.size() {
                grew(new_size - layout.size());
            } else {
                LIVE_BYTES.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static COUNTING: Counting = Counting;

    /// The peak-RSS gate for editing an 8 MiB file is 128 MB; the heap high-water mark is the
    /// share of it a unit test can read deterministically.
    const PEAK_HEAP_CEILING: usize = 128 * 1024 * 1024;

    /// Exactly the 8 MiB `max_file_bytes`, with `tail` on the last line but one.
    fn eight_mib_json() -> (String, usize) {
        const SIZE: usize = 8 * 1024 * 1024;
        const HEAD: &str = "{\n  \"version\": \"1.0\",\n  \"items\": [\n";
        const TAIL: &str = "\n  ],\n  \"tail\": 1\n}\n";
        let mut text = String::with_capacity(SIZE);
        text.push_str(HEAD);
        let mut rows = 0;
        while text.len() + 64 + TAIL.len() < SIZE {
            if rows > 0 {
                text.push_str(",\n");
            }
            write!(text, "    {{\"id\": {rows}, \"name\": \"item-{rows}\"}}").unwrap();
            rows += 1;
        }
        let pad = SIZE - text.len() - TAIL.len() - ",\n    \"\"".len();
        write!(text, ",\n    \"{}\"", "x".repeat(pad)).unwrap();
        text.push_str(TAIL);
        assert_eq!(text.len(), SIZE);
        let tail_line = text.lines().count() - 1;
        (text, tail_line)
    }

    // The flag lands on `tail`, after every item, so locating it walks the whole document.
    #[test]
    fn a_scalar_set_on_an_8_mib_json_file_peaks_under_128_mib_of_heap() {
        let Some(dir) = repo() else { return };
        let (content, tail_line) = eight_mib_json();
        let path = write(&dir, "big.json", &content);
        let expected = content.replace("\"tail\": 1\n", "\"tail\": 2\n");
        drop(content);

        let baseline = LIVE_BYTES.load(Ordering::Relaxed);
        PEAK_BYTES.store(baseline, Ordering::Relaxed);
        let outcome = transform(&setting("big.json", &["tail=2"]), &global());
        let peak = PEAK_BYTES.load(Ordering::Relaxed) - baseline;

        assert_eq!(slug(&outcome), "ok");
        assert_eq!(read(&path), expected);
        let Body::Transform(results) = &outcome.response.body else {
            panic!("a transform body")
        };
        assert_eq!(results[0].lines, [tail_line]);
        assert!(
            peak < PEAK_HEAP_CEILING,
            "peak heap {peak} bytes, ceiling {PEAK_HEAP_CEILING}"
        );
    }

    // The shape that costs a parser the most per byte: four million numbers on one line.
    #[test]
    fn a_scalar_set_on_an_8_mib_one_line_number_array_peaks_under_128_mib_of_heap() {
        const SIZE: usize = 8 * 1024 * 1024;
        const HEAD: &str = "{\"v\": 1, \"a\": [";
        const TAIL: &str = "0], \"t\": 1}\n";
        let Some(dir) = repo() else { return };
        let mut content = String::with_capacity(SIZE);
        content.push_str(HEAD);
        while content.len() + 2 + TAIL.len() <= SIZE {
            content.push_str("1,");
        }
        content.push_str(&" ".repeat(SIZE - content.len() - TAIL.len()));
        content.push_str(TAIL);
        assert_eq!(content.len(), SIZE);
        let path = write(&dir, "dense.json", &content);
        let expected = content.replace("\"t\": 1}", "\"t\": 2}");
        drop(content);

        let baseline = LIVE_BYTES.load(Ordering::Relaxed);
        PEAK_BYTES.store(baseline, Ordering::Relaxed);
        let outcome = transform(&setting("dense.json", &["t=2"]), &global());
        let peak = PEAK_BYTES.load(Ordering::Relaxed) - baseline;

        assert_eq!(slug(&outcome), "ok");
        assert_eq!(read(&path), expected);
        assert!(
            peak < PEAK_HEAP_CEILING,
            "peak heap {peak} bytes, ceiling {PEAK_HEAP_CEILING}"
        );
    }

    // Each set is spliced on its own; the second is located in the text the first produced.
    #[test]
    fn two_sets_on_an_8_mib_json_file_both_land() {
        let Some(dir) = repo() else { return };
        let (content, tail_line) = eight_mib_json();
        let path = write(&dir, "big.json", &content);
        let expected = content
            .replace("\"version\": \"1.0\"", "\"version\": \"2.0\"")
            .replace("\"tail\": 1\n", "\"tail\": 2\n");

        let outcome = transform(&setting("big.json", &["version=2.0", "tail=2"]), &global());

        assert_eq!(slug(&outcome), "ok");
        assert_eq!(read(&path), expected);
        let Body::Transform(results) = &outcome.response.body else {
            panic!("a transform body")
        };
        assert_eq!(results[0].lines, [2, tail_line]);
    }

    // A new key has no scalar to splice, so this set goes through the CST.
    #[test]
    fn a_new_key_on_an_8_mib_json_file_goes_through_the_cst_and_lands_after_the_last_key() {
        let Some(dir) = repo() else { return };
        let (content, tail_line) = eight_mib_json();
        let path = write(&dir, "big.json", &content);
        let expected = content.replace("\"tail\": 1\n}", "\"tail\": 1,\n  \"added\": 1\n}");
        assert_eq!(expected.len(), content.len() + ",\n  \"added\": 1".len());
        drop(content);

        let outcome = transform(&setting("big.json", &["added=1"]), &global());

        assert_eq!(slug(&outcome), "ok");
        assert_eq!(read(&path), expected);
        let Body::Transform(results) = &outcome.response.body else {
            panic!("a transform body")
        };
        assert_eq!(results[0].lines, [tail_line + 1]);
    }
}
