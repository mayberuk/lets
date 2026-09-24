//! Structured formats are validated by the parser `transform` mutates with. The tree-sitter
//! error-node comparison is never called syntax: a recovering grammar parses some invalid
//! programs clean (`echo hi\n}` has zero error nodes; `bash -n` exits 2).

use std::fs::File;
use std::io::{Read as _, Seek as _};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::Error;
use crate::output::plural_suffix;
use crate::{grammars, matcher};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckKind {
    Structured(StructuredLang),
    Structural(grammars::Language),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuredLang {
    Json,
    Toml,
    Yaml,
    Frontmatter,
}

impl StructuredLang {
    fn label(self) -> &'static str {
        match self {
            StructuredLang::Json => "json",
            StructuredLang::Toml => "toml",
            StructuredLang::Yaml => "yaml",
            StructuredLang::Frontmatter => "frontmatter",
        }
    }
}

/// `check: {label} {status}` is the footer sentence. The error counts are each tree's totals;
/// which of them were retained lives in `status` alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer1 {
    pub label: &'static str,
    pub status: String,
    pub ok: bool,
    pub errors_before: u32,
    pub errors_after: u32,
}

/// `bytes` and `edited_start` matter only for markdown, where an edit inside the frontmatter
/// fence is a YAML edit.
pub fn checker_for(path: &Path, bytes: &[u8], edited_start: usize) -> Option<CheckKind> {
    match path.extension()?.to_str()? {
        "json" => Some(CheckKind::Structured(StructuredLang::Json)),
        "toml" => Some(CheckKind::Structured(StructuredLang::Toml)),
        "yaml" | "yml" => Some(CheckKind::Structured(StructuredLang::Yaml)),
        "md" | "markdown" => Some(markdown_kind(bytes, edited_start)),
        other => grammars::from_extension(other).map(CheckKind::Structural),
    }
}

/// `edits` must name every replaced span, ascending and non-overlapping: a span the check does
/// not know about reads as this edit's own damage.
pub fn layer1(
    kind: CheckKind,
    before: &[u8],
    after: &[u8],
    edits: &[(matcher::Span, usize)],
) -> Layer1 {
    match kind {
        CheckKind::Structured(lang) => structured(lang, before, after),
        CheckKind::Structural(lang) => structural(lang, before, after, edits),
    }
}

fn markdown_kind(bytes: &[u8], edited_start: usize) -> CheckKind {
    match frontmatter(bytes) {
        Some(fence) if fence.contains(&edited_start) => {
            CheckKind::Structured(StructuredLang::Frontmatter)
        },
        _ => CheckKind::Structural(grammars::Language::Markdown),
    }
}

/// Exclusive of both fence lines.
fn frontmatter(bytes: &[u8]) -> Option<Range<usize>> {
    const FENCE: &[u8] = b"---";

    let mut offset = 0;
    let mut opened: Option<usize> = None;
    while offset < bytes.len() {
        let eol = bytes[offset..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |i| offset + i);
        let mut content_end = eol;
        if content_end > offset && bytes[content_end - 1] == b'\r' {
            content_end -= 1;
        }
        let is_fence = &bytes[offset..content_end] == FENCE;
        let next_line = (eol + 1).min(bytes.len());
        match opened {
            None if !is_fence => return None,
            None => opened = Some(next_line),
            Some(open) if is_fence => return Some(open..offset),
            Some(_) => {},
        }
        offset = next_line;
    }
    None
}

fn structured(lang: StructuredLang, before: &[u8], after: &[u8]) -> Layer1 {
    let errors_before = u32::from(!parses(lang, before));
    let errors_after = u32::from(!parses(lang, after));
    let ok = errors_after == 0;
    Layer1 {
        label: lang.label(),
        status: if ok { "ok" } else { "invalid" }.to_owned(),
        ok,
        errors_before,
        errors_after,
    }
}

fn parses(lang: StructuredLang, bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    match lang {
        StructuredLang::Json => jsonc_parser::parse_to_ast(
            text,
            &jsonc_parser::CollectOptions::default(),
            &json_options(),
        )
        .is_ok(),
        StructuredLang::Toml => text.parse::<toml_edit::DocumentMut>().is_ok(),
        StructuredLang::Yaml => yamlpath::Document::new(text).is_ok(),
        // A `.md` whose closing fence the edit removed has no frontmatter left to be valid.
        StructuredLang::Frontmatter => {
            frontmatter(bytes).is_some_and(|fence| yamlpath::Document::new(&text[fence]).is_ok())
        },
    }
}

/// Only comments and trailing commas, which `.json` files here carry; the default options would
/// pass `{'a': +0xFF}` as JSON.
fn json_options() -> jsonc_parser::ParseOptions {
    jsonc_parser::ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

/// The range alone is not enough: two zero-width `MISSING` tokens can land at one byte after an
/// edit that closes one construct and breaks another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ErrorNode {
    kind: &'static str,
    start: usize,
    end: usize,
    /// A `None` start opened inside the replaced bytes, so it must never compare equal.
    ancestor: Option<(&'static str, Option<usize>)>,
}

impl ErrorNode {
    fn same_node(self, other: ErrorNode) -> bool {
        self.kind == other.kind && self.start == other.start && self.end == other.end
    }
}

/// tree-sitter-md costs about 81 bytes of RSS per input byte: about 85 MB here, under the 128 MB
/// ceiling for editing an 8 MiB file.
const REGION_CHECK_MIN_BYTES: usize = 1024 * 1024;

/// Also the distance within which two regions merge into one parse.
const REGION_SIDE_CAP_BYTES: usize = 256 * 1024;

const LABEL: &str = "structure";

/// `Changed` carries an after-side byte, so a caller that parsed a slice can name the file line.
enum Verdict {
    Failed,
    Changed(usize),
    Retained(usize),
}

struct Compared {
    verdict: Verdict,
    errors_before: usize,
    errors_after: usize,
}

fn structural(
    lang: grammars::Language,
    before: &[u8],
    after: &[u8],
    edits: &[(matcher::Span, usize)],
) -> Layer1 {
    if before.len().max(after.len()) > REGION_CHECK_MIN_BYTES {
        return structural_regions(lang, before, after, edits);
    }
    let Some(compared) = compare(lang, before, after, edits) else {
        return no_tree(lang);
    };
    let status = match compared.verdict {
        Verdict::Failed => {
            return failed(compared.errors_before, compared.errors_after);
        },
        Verdict::Changed(at) => changed(matcher::line_of(after, at)),
        Verdict::Retained(0) => "ok".to_owned(),
        Verdict::Retained(n) => format!(
            "ok in edited region ({n} pre-existing error{} elsewhere)",
            plural_suffix(n)
        ),
    };
    Layer1 {
        label: LABEL,
        status,
        ok: true,
        errors_before: count(compared.errors_before),
        errors_after: count(compared.errors_after),
    }
}

/// Keeping the edit is the safe direction when no parse happened, so this reports, not reverts.
fn no_tree(lang: grammars::Language) -> Layer1 {
    Layer1 {
        label: LABEL,
        status: format!(
            "inconclusive (the {} parser returned no tree)",
            grammars::name(lang)
        ),
        ok: true,
        errors_before: 0,
        errors_after: 0,
    }
}

fn failed(errors_before: usize, errors_after: usize) -> Layer1 {
    Layer1 {
        label: LABEL,
        status: "failed".to_owned(),
        ok: false,
        errors_before: count(errors_before),
        errors_after: count(errors_after),
    }
}

fn changed(line: usize) -> String {
    format!("inconclusive (line {line}'s construct changed)")
}

struct Window {
    before: Range<usize>,
    after: Range<usize>,
    edits: Range<usize>,
}

/// One tree at a time, so peak memory follows the largest window, not the file. Retention also
/// absorbs the error nodes slice edges produce identically on both sides.
fn structural_regions(
    lang: grammars::Language,
    before: &[u8],
    after: &[u8],
    edits: &[(matcher::Span, usize)],
) -> Layer1 {
    let windows = windows(lang, before, edits);
    let (mut errors_before, mut errors_after, mut retained) = (0usize, 0usize, 0usize);
    let mut inconclusive: Option<String> = None;
    for window in &windows {
        if window.before.len() > REGION_CHECK_MIN_BYTES
            || window.after.len() > REGION_CHECK_MIN_BYTES
        {
            inconclusive.get_or_insert_with(|| "inconclusive (edited span over 1 MiB)".to_owned());
            continue;
        }
        let shifted: Vec<(matcher::Span, usize)> = edits[window.edits.clone()]
            .iter()
            .map(|(span, new_len)| {
                (
                    matcher::Span {
                        start: span.start - window.before.start,
                        end: span.end - window.before.start,
                    },
                    *new_len,
                )
            })
            .collect();
        let Some(compared) = compare(
            lang,
            &before[window.before.clone()],
            &after[window.after.clone()],
            &shifted,
        ) else {
            inconclusive.get_or_insert_with(|| no_tree(lang).status);
            continue;
        };
        errors_before += compared.errors_before;
        errors_after += compared.errors_after;
        match compared.verdict {
            Verdict::Failed => return failed(errors_before, errors_after),
            Verdict::Changed(at) => {
                inconclusive.get_or_insert_with(|| {
                    changed(matcher::line_of(after, window.after.start + at))
                });
            },
            Verdict::Retained(n) => retained += n,
        }
    }

    let status = inconclusive.unwrap_or_else(|| region_ok(after, &windows, retained));
    Layer1 {
        label: LABEL,
        status,
        ok: true,
        errors_before: count(errors_before),
        errors_after: count(errors_after),
    }
}

fn region_ok(after: &[u8], windows: &[Window], retained: usize) -> String {
    let (Some(first), Some(last)) = (windows.first(), windows.last()) else {
        return "ok".to_owned();
    };
    let from = matcher::line_of(after, first.after.start);
    let to = matcher::line_of(
        after,
        last.after.end.saturating_sub(1).max(last.after.start),
    );
    let place = if windows.len() == 1 {
        format!("region :{from}-{to}")
    } else {
        format!("{} regions :{from}-{to}", windows.len())
    };
    if retained == 0 {
        return format!("ok in {place} (file over 1 MiB)");
    }
    format!(
        "ok in {place} (file over 1 MiB, {retained} pre-existing error{} elsewhere in the region{})",
        plural_suffix(retained),
        plural_suffix(windows.len())
    )
}

/// An unmerged neighbour is cut at the next edit's first line, since widening is only context;
/// edits sharing a line always merge, since a cut there would split an edit.
fn windows(
    lang: grammars::Language,
    before: &[u8],
    edits: &[(matcher::Span, usize)],
) -> Vec<Window> {
    struct Merging {
        bytes: Range<usize>,
        members: Range<usize>,
        edited_end: usize,
        added: usize,
        removed: usize,
    }
    let boundary = |line: usize| is_boundary(lang, before.get(line).copied());
    let mut merged: Vec<Merging> = Vec::new();
    for (index, (span, new_len)) in edits.iter().enumerate() {
        let first = line_start(before, span.start);
        let last = line_end(before, span.end.saturating_sub(1).max(span.start));
        let mut start = widen_up(before, first, boundary);
        let end = widen_down(before, last, boundary);
        let (added, removed) = (*new_len, span.end - span.start);
        if let Some(open) = merged.last_mut()
            && start <= open.bytes.end + REGION_SIDE_CAP_BYTES
        {
            let bytes = open.bytes.start..open.bytes.end.max(end);
            let after_len = bytes.len() + open.added + added - open.removed - removed;
            if first < open.edited_end
                || (bytes.len() <= REGION_CHECK_MIN_BYTES && after_len <= REGION_CHECK_MIN_BYTES)
            {
                open.bytes = bytes;
                open.members.end = index + 1;
                open.edited_end = open.edited_end.max(last);
                open.added += added;
                open.removed += removed;
                continue;
            }
            open.bytes.end = open.bytes.end.min(first);
            start = start.max(open.bytes.end);
        }
        merged.push(Merging {
            bytes: start..end,
            members: index..index + 1,
            edited_end: last,
            added,
            removed,
        });
    }

    let delta = |count: usize| totals(&edits[..count], |_| true);
    merged
        .into_iter()
        .map(|Merging { bytes, members, .. }| {
            let (added, removed) = delta(members.start);
            let start = bytes.start + added - removed;
            let (added, removed) = delta(members.end);
            let end = bytes.end + added - removed;
            Window {
                before: bytes,
                after: start..end,
                edits: members,
            }
        })
        .collect()
}

/// In every bundled grammar a top-level item starts at column 0 with anything but a closing
/// bracket; markdown's boundary is an ATX heading.
fn is_boundary(lang: grammars::Language, first: Option<u8>) -> bool {
    let Some(byte) = first else {
        return false;
    };
    if lang == grammars::Language::Markdown {
        return byte == b'#';
    }
    !byte.is_ascii_whitespace() && !matches!(byte, b'}' | b')' | b']')
}

fn line_start(bytes: &[u8], at: usize) -> usize {
    bytes[..at]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |newline| newline + 1)
}

/// The start of the line after the one holding `at`.
fn line_end(bytes: &[u8], at: usize) -> usize {
    bytes
        .get(at..)
        .and_then(|rest| rest.iter().position(|&b| b == b'\n'))
        .map_or(bytes.len(), |newline| at + newline + 1)
}

fn widen_up(bytes: &[u8], mut start: usize, boundary: impl Fn(usize) -> bool) -> usize {
    let limit = start.saturating_sub(REGION_SIDE_CAP_BYTES);
    while start > 0 && !boundary(start) {
        let previous = line_start(bytes, start - 1);
        if previous < limit {
            break;
        }
        start = previous;
    }
    start
}

fn widen_down(bytes: &[u8], mut end: usize, boundary: impl Fn(usize) -> bool) -> usize {
    let limit = end + REGION_SIDE_CAP_BYTES;
    while end < bytes.len() && !boundary(end) {
        let next = line_end(bytes, end);
        if next > limit {
            break;
        }
        end = next;
    }
    end
}

fn compare(
    lang: grammars::Language,
    before: &[u8],
    after: &[u8],
    edits: &[(matcher::Span, usize)],
) -> Option<Compared> {
    // One tree at a time: holding both doubled the peak (about 80 bytes of RSS per input byte).
    let before_errors = parse_errors(lang, before)?;
    let after_errors = parse_errors(lang, after)?;
    let compared = |verdict| Compared {
        verdict,
        errors_before: before_errors.len(),
        errors_after: after_errors.len(),
    };

    let mapped: Vec<ErrorNode> = before_errors
        .iter()
        .filter(|node| {
            edits
                .iter()
                .all(|(edited, _)| node.end <= edited.start || node.start >= edited.end)
        })
        .map(|node| map(*node, edits))
        .collect();
    let edited_after = after_spans(edits);

    let mut retained = 0usize;
    let mut changed_at = None;
    for node in &after_errors {
        // Tested on the node, not the line typed: an unbalanced brace reports at end of file.
        let outside = edited_after
            .iter()
            .all(|span| node.end <= span.start || node.start >= span.end);
        let candidates = mapped.iter().filter(|m| m.same_node(*node));
        if !outside || candidates.clone().next().is_none() {
            return Some(compared(Verdict::Failed));
        }
        if candidates.clone().any(|m| m.ancestor == node.ancestor) {
            retained += 1;
        } else {
            changed_at.get_or_insert(node.start);
        }
    }

    Some(compared(match changed_at {
        Some(at) => Verdict::Changed(at),
        None => Verdict::Retained(retained),
    }))
}

/// Exact only because the spans ascend and never overlap.
fn after_spans(edits: &[(matcher::Span, usize)]) -> Vec<matcher::Span> {
    let (mut added, mut removed) = (0usize, 0usize);
    let mut spans = Vec::with_capacity(edits.len());
    for (edited, new_len) in edits {
        let start = edited.start + added - removed;
        spans.push(matcher::Span {
            start,
            end: start + new_len,
        });
        added += new_len;
        removed += edited.end - edited.start;
    }
    spans
}

/// Callers keep only edits below the offset they shift, so adding first never underflows.
fn totals(
    edits: &[(matcher::Span, usize)],
    keep: impl Fn(matcher::Span) -> bool,
) -> (usize, usize) {
    edits.iter().filter(|(edited, _)| keep(*edited)).fold(
        (0, 0),
        |(added, removed), (edited, new_len)| {
            (added + new_len, removed + (edited.end - edited.start))
        },
    )
}

/// An insertion pushes a node that *starts* at its point and leaves one that *ends* there. An
/// ancestor that opened inside the replaced bytes has no new position.
fn map(node: ErrorNode, edits: &[(matcher::Span, usize)]) -> ErrorNode {
    let (added, removed) = totals(edits, |edit| {
        node.start >= edit.end && node.end > edit.start
    });
    let (start, end) = (node.start + added - removed, node.end + added - removed);
    ErrorNode {
        kind: node.kind,
        start,
        end,
        ancestor: node.ancestor.map(|(kind, at)| {
            let at = at.and_then(|at| {
                if edits
                    .iter()
                    .any(|(edit, _)| edit.start <= at && at < edit.end)
                {
                    return None;
                }
                let (added, removed) = totals(edits, |edit| edit.end <= at);
                Some(at + added - removed)
            });
            (kind, at)
        }),
    }
}

fn parse_errors(lang: grammars::Language, bytes: &[u8]) -> Option<Vec<ErrorNode>> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(grammars::language(lang)).ok()?;
    parser.parse(bytes, None).map(|tree| error_nodes(&tree))
}

/// No pruning on `has_error`, which would rely on it covering `MISSING` nodes; the parse costs
/// far more than this walk.
fn error_nodes(tree: &tree_sitter::Tree) -> Vec<ErrorNode> {
    let mut cursor = tree.walk();
    let mut found = Vec::new();
    let mut descending = true;
    loop {
        if descending {
            let node = cursor.node();
            if node.is_error() || node.is_missing() {
                found.push(ErrorNode {
                    kind: node.kind(),
                    start: node.start_byte(),
                    end: node.end_byte(),
                    ancestor: named_ancestor(node),
                });
            }
            if cursor.goto_first_child() {
                continue;
            }
        }
        if cursor.goto_next_sibling() {
            descending = true;
            continue;
        }
        if !cursor.goto_parent() {
            return found;
        }
        descending = false;
    }
}

fn named_ancestor(node: tree_sitter::Node) -> Option<(&'static str, Option<usize>)> {
    let mut parent = node.parent();
    while let Some(ancestor) = parent {
        if ancestor.is_named() {
            return Some((ancestor.kind(), Some(ancestor.start_byte())));
        }
        parent = ancestor.parent();
    }
    None
}

fn count(nodes: usize) -> u32 {
    u32::try_from(nodes).unwrap_or(u32::MAX)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandVerdict {
    Ok,
    Inconclusive(String),
    Failed(String),
    Skipped(String),
}

impl CommandVerdict {
    /// `0 → non-zero` is the one transition that reverts; a failure with no clean baseline is
    /// kept and named.
    #[must_use]
    pub fn against_baseline(self, baseline: &CommandVerdict) -> CommandVerdict {
        let why = match (baseline, &self) {
            (CommandVerdict::Ok, _)
            | (
                _,
                CommandVerdict::Ok | CommandVerdict::Inconclusive(_) | CommandVerdict::Skipped(_),
            ) => return self,
            (CommandVerdict::Failed(_), _) => "failed before and after".to_owned(),
            (CommandVerdict::Inconclusive(why) | CommandVerdict::Skipped(why), _) => {
                format!("no baseline exit code: {why}")
            },
        };
        CommandVerdict::Inconclusive(why)
    }
}

/// Only has to keep the kill from being visibly late against a 60 s default timeout.
const POLL: Duration = Duration::from_millis(5);

/// A timeout is `Inconclusive`, never a failure: a slow checker must not become a silent revert.
pub fn run_command(command: &str, files: &[PathBuf], timeout: Duration) -> CommandVerdict {
    if !command.starts_with('@') {
        return run_command_in(command, files, timeout, Path::new("."));
    }
    let canonical: Vec<PathBuf> = files
        .iter()
        .map(|file| std::fs::canonicalize(file).unwrap_or_else(|_| file.clone()))
        .collect();
    let resolved = match preset_groups(command, &canonical, &crate::fs::tree_root()) {
        Ok(resolved) => resolved,
        Err(error) => return CommandVerdict::Skipped(error.to_string()),
    };
    if resolved.groups.is_empty() {
        return CommandVerdict::Skipped(resolved.reason);
    }
    // A file no manifest covers turns `ok` into a named skip, not a claim it was checked.
    let typed = resolved
        .unmatched
        .first()
        .and_then(|file| canonical.iter().position(|seen| seen == file))
        .map(|index| &files[index]);
    let mut worst = match typed {
        Some(file) => CommandVerdict::Skipped(format!("{}: {}", file.display(), resolved.reason)),
        None => CommandVerdict::Ok,
    };
    for (preset, files) in &resolved.groups {
        let verdict = run_command_in(&preset.command, files, timeout, &preset.dir);
        if severity(&verdict) > severity(&worst) {
            worst = verdict;
        }
    }
    worst
}

fn severity(verdict: &CommandVerdict) -> u8 {
    match verdict {
        CommandVerdict::Ok => 0,
        CommandVerdict::Skipped(_) => 1,
        CommandVerdict::Inconclusive(_) => 2,
        CommandVerdict::Failed(_) => 3,
    }
}

/// A preset runs in its manifest's directory, not the caller's.
pub fn run_command_in(
    command: &str,
    files: &[PathBuf],
    timeout: Duration,
    dir: &Path,
) -> CommandVerdict {
    let name = checker_name(command);
    if !on_path(name) {
        return CommandVerdict::Skipped(format!("{name} absent"));
    }
    let (mut captured, stdout, stderr) = match capture() {
        Ok(parts) => parts,
        Err(err) => return CommandVerdict::Inconclusive(err.to_string()),
    };
    // The checker must not write to stdout, the answer, nor read the batch spec still on stdin.
    let spawned = Command::new("sh")
        .arg("-c")
        .arg(substitute(command, files))
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(err) => return CommandVerdict::Inconclusive(err.to_string()),
    };

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return CommandVerdict::Ok,
            Ok(Some(_)) => return CommandVerdict::Failed(excerpt(&mut captured)),
            Err(err) => return CommandVerdict::Inconclusive(err.to_string()),
            Ok(None) => {},
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return CommandVerdict::Inconclusive("timed out".to_owned());
        }
        std::thread::sleep(POLL);
    }
}

/// A file, not a pipe: nothing reads until exit, so a full pipe would block until the timeout.
fn capture() -> std::io::Result<(File, Stdio, Stdio)> {
    let sink = tempfile::tempfile()?;
    let stdout = Stdio::from(sink.try_clone()?);
    let stderr = Stdio::from(sink.try_clone()?);
    Ok((sink, stdout, stderr))
}

/// Not bounded by `--max-bytes`: the excerpt is status, not content.
const EXCERPT_BYTES: u64 = 1024;

fn excerpt(captured: &mut File) -> String {
    let mut head = Vec::new();
    if captured.rewind().is_err() {
        return String::new();
    }
    if captured.take(EXCERPT_BYTES).read_to_end(&mut head).is_err() {
        return String::new();
    }
    let first = head.split(|&byte| byte == b'\n').next().unwrap_or(&head);
    String::from_utf8_lossy(first).trim().to_owned()
}

fn checker_name(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or(command)
}

/// A shell builtin reads as absent: a named skip is safe, a false revert is not.
fn on_path(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    if name.contains('/') {
        return Path::new(name).is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
}

/// A path is data, so each is single quoted for `sh -c`, with an embedded `'` spelled `'\''`.
fn substitute(command: &str, files: &[PathBuf]) -> String {
    if !command.contains("{}") {
        return command.to_owned();
    }
    let joined = files
        .iter()
        .map(|path| format!("'{}'", path.display().to_string().replace('\'', r"'\''")))
        .collect::<Vec<_>>()
        .join(" ");
    command.replace("{}", &joined)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preset {
    pub command: String,
    pub dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresetResolution {
    Run(Preset),
    Skipped(String),
}

/// Row order is the tie-break when one directory holds several manifests.
static PRESETS: [(&str, &[&str]); 4] = [
    ("@cargo", &["Cargo.toml"]),
    ("@go", &["go.mod"]),
    ("@tsc", &["tsconfig.json"]),
    ("@py", &["pyproject.toml", "setup.py"]),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetGroups {
    pub groups: Vec<(Preset, Vec<PathBuf>)>,
    pub unmatched: Vec<PathBuf>,
    /// Why a file is unmatched, in the words a footer skip line uses.
    pub reason: String,
}

/// Per file: a batch spanning two crates checks both, where their common parent finds neither.
pub fn preset_groups(name: &str, files: &[PathBuf], root: &Path) -> Result<PresetGroups, Error> {
    let rows = preset_rows(name)?;
    let mut groups: Vec<(Preset, Vec<PathBuf>)> = Vec::new();
    let mut unmatched = Vec::new();
    for file in files {
        match resolve_preset(name, file, root)? {
            PresetResolution::Run(preset) => {
                match groups.iter_mut().find(|(seen, _)| seen.dir == preset.dir) {
                    Some((_, members)) => members.push(file.clone()),
                    None => groups.push((preset, vec![file.clone()])),
                }
            },
            PresetResolution::Skipped(_) => unmatched.push(file.clone()),
        }
    }
    groups.sort_by(|(a, _), (b, _)| a.dir.cmp(&b.dir));
    Ok(PresetGroups {
        groups,
        unmatched,
        reason: not_found_reason(rows),
    })
}

/// Never walks above `root`: a manifest outside the working tree belongs to another project.
pub fn resolve_preset(name: &str, file: &Path, root: &Path) -> Result<PresetResolution, Error> {
    let rows = preset_rows(name)?;
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let start = file.parent().unwrap_or(&root);
    for dir in start.ancestors().take_while(|dir| dir.starts_with(&root)) {
        for (preset, manifests) in rows {
            if manifests
                .iter()
                .any(|manifest| dir.join(manifest).is_file())
            {
                return Ok(PresetResolution::Run(Preset {
                    command: preset_command(preset, dir),
                    dir: dir.to_path_buf(),
                }));
            }
        }
    }
    Ok(PresetResolution::Skipped(not_found_reason(rows)))
}

fn preset_rows(name: &str) -> Result<&'static [(&'static str, &'static [&'static str])], Error> {
    if name == "@auto" {
        return Ok(&PRESETS);
    }
    match PRESETS.iter().position(|(preset, _)| *preset == name) {
        Some(row) => Ok(&PRESETS[row..=row]),
        None => Err(Error::Usage {
            message: format!(
                "unknown preset {name} \u{b7} the presets are @auto, @cargo, @go, @tsc, @py"
            ),
        }),
    }
}

fn not_found_reason(rows: &[(&str, &[&str])]) -> String {
    match rows {
        [(preset, manifests)] => format!("{preset} found no {}", manifests.join(" or ")),
        _ => "@auto found no manifest".to_owned(),
    }
}

fn preset_command(preset: &str, dir: &Path) -> String {
    match preset {
        // From a member's directory, `cargo check` misses siblings still using a renamed item.
        "@cargo" => "cargo check --workspace --quiet --all-targets",
        "@go" => "go build ./...",
        "@tsc" if has_typecheck_script(dir) => "npm run -s typecheck",
        "@tsc" => "npx --no-install tsc --noEmit -p .",
        _ => "python3 -m py_compile {}",
    }
    .to_owned()
}

/// A missing or malformed `package.json` falls through to invoking `tsc` directly.
fn has_typecheck_script(dir: &Path) -> bool {
    std::fs::read(dir.join("package.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_some_and(|json| {
            json.pointer("/scripts/typecheck")
                .is_some_and(serde_json::Value::is_string)
        })
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::path::PathBuf;

    use super::*;

    fn span(start: usize, end: usize) -> matcher::Span {
        matcher::Span { start, end }
    }

    /// The structured validators never look at the span, so one whole-file edit stands in.
    fn whole(bytes: &[u8]) -> (matcher::Span, usize) {
        (span(0, bytes.len()), bytes.len())
    }

    fn structured_check(lang: StructuredLang, before: &str, after: &str) -> Layer1 {
        layer1(
            CheckKind::Structured(lang),
            before.as_bytes(),
            after.as_bytes(),
            &[whole(after.as_bytes())],
        )
    }

    fn insert_and_check(lang: grammars::Language, before: &str, at: usize, new: &str) -> Layer1 {
        let after = format!("{}{new}{}", &before[..at], &before[at..]);
        layer1(
            CheckKind::Structural(lang),
            before.as_bytes(),
            after.as_bytes(),
            &[(span(at, at), new.len())],
        )
    }

    fn edit_and_check(lang: grammars::Language, before: &str, old: &str, new: &str) -> Layer1 {
        let start = before.find(old).expect("the fixture contains `old`");
        let edited = span(start, start + old.len());
        let after = format!("{}{new}{}", &before[..start], &before[edited.end..]);
        layer1(
            CheckKind::Structural(lang),
            before.as_bytes(),
            after.as_bytes(),
            &[(edited, new.len())],
        )
    }

    #[test]
    fn an_edit_that_breaks_json_is_not_ok_and_counts_one_error_after() {
        let check = structured_check(
            StructuredLang::Json,
            "{\n  \"window\": 200\n}\n",
            "{\n  \"window\": 200\n",
        );

        assert!(!check.ok);
        assert_eq!(check.errors_before, 0);
        assert_eq!(check.errors_after, 1);
        assert_eq!(check.label, "json");
        assert_eq!(check.status, "invalid");
    }

    #[test]
    fn an_edit_that_keeps_json_valid_is_ok() {
        let check = structured_check(
            StructuredLang::Json,
            "{\n  \"window\": 200\n}\n",
            "{\n  \"window\": 100\n}\n",
        );

        assert!(check.ok);
        assert_eq!(check.errors_after, 0);
        assert_eq!(
            format!("check: {} {}", check.label, check.status),
            "check: json ok"
        );
    }

    #[test]
    fn json_accepts_the_two_departures_transform_preserves_and_no_others() {
        let jsonc = "{\n  // the default window\n  \"window\": 200,\n}\n";
        assert!(structured_check(StructuredLang::Json, jsonc, jsonc).ok);

        for json5 in [
            "{ window: 200 }\n",
            "{ \"window\": +200 }\n",
            "{ \"window\": 0xFF }\n",
            "{ 'window': 200 }\n",
            "{ \"a\": 1 \"b\": 2 }\n",
        ] {
            assert!(
                !structured_check(StructuredLang::Json, json5, json5).ok,
                "{json5} is not JSON"
            );
        }
    }

    #[test]
    fn a_file_that_was_already_invalid_reports_it_and_still_fails_on_an_invalid_result() {
        // `ok` follows `errors_after` alone: `transform` cannot parse the broken file either.
        let check = structured_check(StructuredLang::Toml, "[package\n", "[package\nname = 1\n");

        assert_eq!(check.errors_before, 1);
        assert_eq!(check.errors_after, 1);
        assert!(!check.ok);
    }

    #[test]
    fn toml_and_yaml_each_report_under_their_own_label() {
        let toml = structured_check(
            StructuredLang::Toml,
            "[package]\nname = \"lets\"\n",
            "[package]\nname = \"lets\"\nversion = \"0.1.0\"\n",
        );
        assert!(toml.ok);
        assert_eq!(toml.label, "toml");

        let broken_toml = structured_check(
            StructuredLang::Toml,
            "[package]\nname = \"lets\"\n",
            "[package]\nname = \n",
        );
        assert!(!broken_toml.ok);

        let yaml = structured_check(
            StructuredLang::Yaml,
            "name: lets\nitems:\n  - one\n",
            "name: lets\nitems:\n  - one\n  - two\n",
        );
        assert!(yaml.ok);
        assert_eq!(yaml.label, "yaml");

        let broken_yaml = structured_check(
            StructuredLang::Yaml,
            "name: lets\n",
            "name: lets\n  bad: [unclosed\n",
        );
        assert!(!broken_yaml.ok);
    }

    #[test]
    fn frontmatter_validates_the_yaml_between_the_fences_and_not_the_prose_below_it() {
        let before = "---\ntags: [moc]\n---\n\n# Heading\n\nprose: not: yaml\n";
        let valid =
            "---\ntags: [moc]\nlast_updated: 2026-09-14\n---\n\n# Heading\n\nprose: not: yaml\n";
        let check = structured_check(StructuredLang::Frontmatter, before, valid);
        assert!(check.ok, "{}", check.status);
        assert_eq!(check.label, "frontmatter");

        let broken = "---\ntags: [moc\n---\n\n# Heading\n";
        assert!(!structured_check(StructuredLang::Frontmatter, before, broken).ok);

        let unfenced = "---\ntags: [moc]\n\n# Heading\n";
        assert!(!structured_check(StructuredLang::Frontmatter, before, unfenced).ok);
    }

    /// Each span on its own, as `--all` hands them over; a single union span cannot express this.
    fn edit_all_and_check(lang: grammars::Language, before: &str, old: &str, new: &str) -> Layer1 {
        let edits: Vec<(matcher::Span, usize)> = before
            .match_indices(old)
            .map(|(start, _)| (span(start, start + old.len()), new.len()))
            .collect();
        assert!(edits.len() > 1, "the fixture has to match more than once");
        let after = before.replace(old, new);
        layer1(
            CheckKind::Structural(lang),
            before.as_bytes(),
            after.as_bytes(),
            &edits,
        )
    }

    #[test]
    fn a_pre_existing_error_between_two_all_matches_is_retained_not_called_new() {
        // The union of the two matches encloses this error, so a union-only check would revert.
        let before = "const A: usize = usageCap;\nfn broken( {\n}\nconst B: usize = usageCap;\n";

        let check = edit_all_and_check(grammars::Language::Rust, before, "usageCap", "usageLimit");

        assert!(check.ok, "{}", check.status);
        assert_eq!(
            check.status,
            "ok in edited region (1 pre-existing error elsewhere)"
        );
    }

    #[test]
    fn an_error_below_two_all_matches_is_matched_at_the_position_both_deltas_left_it() {
        // A shift taken from one span alone would look for the error 5 bytes too far down.
        let before = "const A: usize = usageCap;\nconst B: usize = usageCap;\nfn broken( {\n}\n";

        let check = edit_all_and_check(grammars::Language::Rust, before, "usageCap", "cap");

        assert!(check.ok, "{}", check.status);
        assert_eq!(
            check.status,
            "ok in edited region (1 pre-existing error elsewhere)"
        );
    }

    #[test]
    fn an_all_replacement_that_breaks_the_file_itself_still_fails() {
        let before = "fn a() -> usize { 1 }\nfn b() -> usize { 1 }\n";

        let check = edit_all_and_check(grammars::Language::Rust, before, "1 }", "1 )");

        assert!(!check.ok, "{}", check.status);
        assert!(check.status.contains("failed"), "{}", check.status);
    }

    #[test]
    fn an_edit_that_adds_a_new_error_node_fails() {
        let check = edit_and_check(
            grammars::Language::Rust,
            "fn usage() -> usize {\n    let total = 1;\n    total\n}\n",
            "total\n}",
            "total)\n}",
        );

        assert!(!check.ok);
        assert!(check.status.contains("failed"), "{}", check.status);
        assert_eq!(check.errors_before, 0);
        assert!(check.errors_after > 0);
    }

    #[test]
    fn an_edit_that_adds_no_error_node_is_ok() {
        let check = edit_and_check(
            grammars::Language::Rust,
            "fn usage() -> usize {\n    let total = 1;\n    total\n}\n",
            "let total = 1;",
            "let total = 2;",
        );

        assert!(check.ok);
        assert_eq!(check.status, "ok");
        assert_eq!(check.errors_after, 0);
    }

    #[test]
    fn a_pre_existing_error_elsewhere_survives_the_edit_and_is_counted_not_reverted() {
        let before = "fn broken( {\n}\n\nfn other( {\n}\n\nfn usage() -> usize {\n    1\n}\n";
        let check = edit_and_check(grammars::Language::Rust, before, "    1\n", "    2\n");

        assert!(check.ok);
        assert_eq!(
            check.status,
            "ok in edited region (2 pre-existing errors elsewhere)"
        );
        assert_eq!(check.errors_before, 2);
        assert_eq!(check.errors_after, 2);
    }

    #[test]
    fn one_pre_existing_error_reads_in_the_singular() {
        let before = "fn broken( {\n}\n\nfn usage() -> usize {\n    1\n}\n";
        let check = edit_and_check(grammars::Language::Rust, before, "    1\n", "    2\n");

        assert_eq!(
            check.status,
            "ok in edited region (1 pre-existing error elsewhere)"
        );
    }

    #[test]
    fn a_pre_existing_error_after_the_edit_is_matched_at_its_shifted_position() {
        // The replacement is longer than what it replaces, so the error below it shifts.
        let before = "fn usage() -> usize {\n    1\n}\n\nfn broken( {\n}\n";
        let check = edit_and_check(
            grammars::Language::Rust,
            before,
            "    1\n",
            "    let total = 1;\n    total\n",
        );

        assert!(check.ok, "{}", check.status);
        assert_eq!(
            check.status,
            "ok in edited region (1 pre-existing error elsewhere)"
        );
    }

    /// The `ERROR` node starts at line 2's first byte, where `--insert-before` inserts.
    const STRAY_BRACE_ON_LINE_2: &str = "fn a() {}\n}\nfn b() {}\n";

    #[test]
    fn an_insertion_shifts_a_pre_existing_error_that_starts_at_its_own_point() {
        // An error starting exactly at the insertion point moves too; left in place it would look
        // new, and a file the edit did not break would be reverted.
        let at = STRAY_BRACE_ON_LINE_2
            .find("}\nfn b")
            .expect("the stray brace opens line 2");

        let check = insert_and_check(
            grammars::Language::Rust,
            STRAY_BRACE_ON_LINE_2,
            at,
            "fn c() {}\n",
        );

        assert!(check.ok, "{}", check.status);
        assert_eq!(
            check.status,
            "ok in edited region (1 pre-existing error elsewhere)"
        );
        assert_eq!(check.errors_before, 1);
        assert_eq!(check.errors_after, 1);
    }

    #[test]
    fn an_insertion_that_breaks_the_file_itself_still_fails() {
        let at = STRAY_BRACE_ON_LINE_2.find("}\nfn b").unwrap();

        let check = insert_and_check(grammars::Language::Rust, STRAY_BRACE_ON_LINE_2, at, ")\n");

        assert!(!check.ok, "{}", check.status);
        assert!(check.status.contains("failed"), "{}", check.status);
    }

    #[test]
    fn a_zero_width_missing_token_is_collected_as_an_error_node() {
        // `MISSING` is not an `ERROR` node: an `ERROR`-only walk would call an unclosed `$(` clean.
        let found = parse_errors(grammars::Language::Bash, b"a=$(foo\n").unwrap();

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].kind, ")");
        assert_eq!(found[0].start, found[0].end);
        assert_eq!(found[0].ancestor, Some(("command_substitution", Some(2))));
    }

    /// The trailing `MISSING ")"`'s construct opens at byte 13, inside the first two lines.
    const SUBSTITUTION_OPENED_ABOVE: &str = "AAAAAAAAAA\ny=$(z\necho tail\n";

    #[test]
    fn deleting_the_bytes_a_surviving_errors_construct_opened_in_does_not_panic() {
        // Mapping the ancestor by the length delta alone would subtract past zero.
        let check = edit_and_check(
            grammars::Language::Bash,
            SUBSTITUTION_OPENED_ABOVE,
            "AAAAAAAAAA\ny=$(z\n",
            "",
        );

        assert!(check.ok, "{}", check.status);
        assert_eq!(check.errors_after, 0);
    }

    #[test]
    fn a_retained_error_whose_construct_was_rewritten_in_place_is_inconclusive() {
        // Arithmetic alone would call this `$(` the same construct, but its bytes were replaced.
        let check = edit_and_check(
            grammars::Language::Bash,
            SUBSTITUTION_OPENED_ABOVE,
            "AAAAAAAAAA\ny=$(z\n",
            "y=$(z\n",
        );

        assert!(check.ok);
        assert!(
            check.status.starts_with("inconclusive ("),
            "{}",
            check.status
        );
    }

    /// A zero-delta edit moving the `)` leaves `MISSING ")"` at byte 13 in both trees; only its
    /// ancestor moves, from byte 9 to byte 2.
    const ONE_SUBSTITUTION_CLOSED: &str = "a=$(x)\nb=$(y\n";

    #[test]
    fn a_retained_error_whose_construct_changed_is_inconclusive_and_never_ok() {
        let check = edit_and_check(
            grammars::Language::Bash,
            ONE_SUBSTITUTION_CLOSED,
            "x)\nb=$(y",
            "x\nb=$(y)",
        );

        assert!(check.ok, "an inconclusive check is kept, not reverted");
        assert!(
            check.status.starts_with("inconclusive ("),
            "{}",
            check.status
        );
        assert!(!check.status.contains("ok"), "{}", check.status);
        assert_eq!(check.errors_before, 1);
        assert_eq!(check.errors_after, 1);
    }

    #[test]
    fn a_retained_error_whose_construct_did_not_change_is_ok_in_the_edited_region() {
        let check = edit_and_check(
            grammars::Language::Bash,
            ONE_SUBSTITUTION_CLOSED,
            "a=$(x)",
            "a=$(z)",
        );

        assert!(check.ok);
        assert_eq!(
            check.status,
            "ok in edited region (1 pre-existing error elsewhere)"
        );
    }

    /// 45 bytes each; function `k` is lines `3k-2..3k`, and only its `export` line is a boundary.
    fn functions(count: usize) -> String {
        (1..=count).fold(String::new(), |mut text, k| {
            write!(text, "export function f{k:05}() {{\n  return v{k:05}\n}}\n")
                .expect("infallible");
            text
        })
    }

    /// 24,000 × 45 bytes: over the region threshold by about 30 KB.
    const OVER_ONE_MIB: usize = 24_000;

    fn edit_each_and_check(
        lang: grammars::Language,
        before: &str,
        replacements: &[(&str, &str)],
    ) -> Layer1 {
        let mut after = String::with_capacity(before.len());
        let mut edits = Vec::new();
        let mut copied = 0;
        for (old, new) in replacements {
            let start = before.find(old).expect("the fixture contains `old`");
            after.push_str(&before[copied..start]);
            after.push_str(new);
            copied = start + old.len();
            edits.push((span(start, copied), new.len()));
        }
        after.push_str(&before[copied..]);
        layer1(
            CheckKind::Structural(lang),
            before.as_bytes(),
            after.as_bytes(),
            &edits,
        )
    }

    #[test]
    fn an_edit_in_a_file_over_one_mib_is_checked_in_its_enclosing_function_only() {
        let before = functions(OVER_ONE_MIB);
        assert!(before.len() > REGION_CHECK_MIN_BYTES);

        let check = edit_and_check(
            grammars::Language::TypeScript,
            &before,
            "return v00500",
            "return v00501",
        );

        assert!(check.ok, "{}", check.status);
        assert_eq!(check.label, "structure");
        assert_eq!(check.status, "ok in region :1498-1500 (file over 1 MiB)");
    }

    #[test]
    fn an_unclosed_brace_inside_the_region_of_a_file_over_one_mib_fails() {
        let before = functions(OVER_ONE_MIB);

        let check = edit_and_check(
            grammars::Language::TypeScript,
            &before,
            "return v00500",
            "return v00500 {",
        );

        assert!(!check.ok, "{}", check.status);
        assert_eq!(check.status, "failed");
    }

    #[test]
    fn a_changed_construct_in_a_region_names_the_whole_files_line() {
        // The fixture is its own window, so its `MISSING ")"` at slice byte 13 is line 30,003.
        let lines = (1..=60_000).fold(String::new(), |mut text, n| {
            writeln!(text, "f{n:05}() {{ echo 1; }}").expect("infallible");
            text
        });
        let cut = lines.len() / 2;
        assert_eq!(matcher::line_of(lines.as_bytes(), cut), 30_001);
        let before = format!(
            "{}{ONE_SUBSTITUTION_CLOSED}{}",
            &lines[..cut],
            &lines[cut..]
        );
        assert!(before.len() > REGION_CHECK_MIN_BYTES);

        let check = edit_and_check(grammars::Language::Bash, &before, "x)\nb=$(y", "x\nb=$(y)");

        assert!(check.ok, "an inconclusive check is kept, not reverted");
        assert_eq!(
            check.status,
            "inconclusive (line 30003's construct changed)"
        );
    }

    #[test]
    fn two_edits_more_than_twice_the_side_cap_apart_are_two_regions() {
        let before = functions(OVER_ONE_MIB);

        let check = edit_each_and_check(grammars::Language::TypeScript, &before, &[
            ("return v00100", "return v00101"),
            ("return v20000", "return v20001"),
        ]);

        assert!(check.ok, "{}", check.status);
        assert_eq!(check.status, "ok in 2 regions :298-60000 (file over 1 MiB)");
    }

    #[test]
    fn two_edits_ten_lines_apart_are_one_region() {
        let before = functions(OVER_ONE_MIB);

        let check = edit_each_and_check(grammars::Language::TypeScript, &before, &[
            ("return v00500", "return v00501"),
            ("return v00503", "return v00504"),
        ]);

        assert!(check.ok, "{}", check.status);
        assert_eq!(check.status, "ok in region :1498-1509 (file over 1 MiB)");
    }

    #[test]
    fn a_replacement_over_one_mib_is_inconclusive_without_a_parse() {
        let before = functions(OVER_ONE_MIB);
        let new = format!("return v00501{}", " ".repeat(1_572_864));

        let check = edit_and_check(
            grammars::Language::TypeScript,
            &before,
            "return v00500",
            &new,
        );

        assert!(check.ok, "an inconclusive check is kept, not reverted");
        assert_eq!(check.status, "inconclusive (edited span over 1 MiB)");
        assert_eq!((check.errors_before, check.errors_after), (0, 0));
    }

    #[test]
    fn two_close_edits_whose_merged_window_would_pass_one_mib_are_two_regions() {
        // Indented lines never bound a window, so each widens 256 KiB a side. Merged, A's 912 KB
        // window and B would span about 1.1 MB, over 1 MiB; apart, each is under it.
        let line = |n: usize| format!("  echo {n:07}\n");
        let before: String = (1..=100_000).map(line).collect();
        assert_eq!(line(1).len(), 15);
        let block: String = (30_001..=56_667).map(line).collect();
        let far = line(70_001);
        assert!(before.len() > REGION_CHECK_MIN_BYTES);

        let check = edit_each_and_check(grammars::Language::Bash, &before, &[
            (&block, "  echo removed\n"),
            (&far, "  echo 7000100\n"),
        ]);

        assert!(check.ok, "{}", check.status);
        assert!(
            check.status.starts_with("ok in 2 regions :"),
            "{}",
            check.status
        );
    }

    #[test]
    fn a_file_of_exactly_one_mib_is_checked_whole_and_one_byte_more_by_region() {
        // 23,301 functions are 1,048,545 bytes; a 31-byte comment line makes exactly 1 MiB.
        let body = functions(23_301);
        let exact = format!("{body}//{}\n", "x".repeat(28));
        let over = format!("{body}//{}\n", "x".repeat(29));
        assert_eq!(exact.len(), 1_048_576);
        assert_eq!(over.len(), 1_048_577);

        let check = |before: &str| {
            edit_and_check(
                grammars::Language::TypeScript,
                before,
                "return v00500",
                "return v00501",
            )
            .status
        };

        assert_eq!(check(&exact), "ok");
        assert_eq!(check(&over), "ok in region :1498-1500 (file over 1 MiB)");
    }

    #[test]
    fn an_unrecognised_extension_has_no_checker() {
        for path in ["app.vue", "Gemfile.rb2", "notes", "a.RS", "b.JSON"] {
            assert_eq!(checker_for(Path::new(path), b"", 0), None, "{path}");
        }
    }

    #[test]
    fn every_structured_extension_and_every_grammar_extension_has_its_checker() {
        let cases = [
            ("a.json", CheckKind::Structured(StructuredLang::Json)),
            ("a.toml", CheckKind::Structured(StructuredLang::Toml)),
            ("a.yaml", CheckKind::Structured(StructuredLang::Yaml)),
            ("a.yml", CheckKind::Structured(StructuredLang::Yaml)),
            ("a.rs", CheckKind::Structural(grammars::Language::Rust)),
            (
                "a.ts",
                CheckKind::Structural(grammars::Language::TypeScript),
            ),
            ("a.sh", CheckKind::Structural(grammars::Language::Bash)),
            ("a.go", CheckKind::Structural(grammars::Language::Go)),
        ];
        for (path, expected) in cases {
            assert_eq!(
                checker_for(Path::new(path), b"", 0),
                Some(expected),
                "{path}"
            );
        }
    }

    #[test]
    fn a_markdown_edit_inside_the_frontmatter_fence_is_a_yaml_edit_and_one_below_it_is_not() {
        let path = Path::new("projects-moc.md");
        let body = "---\ntags: [moc]\nlast_updated: 2026-07-12\n---\n\n# Projects\n";

        let inside = body.find("last_updated").unwrap();
        assert_eq!(
            checker_for(path, body.as_bytes(), inside),
            Some(CheckKind::Structured(StructuredLang::Frontmatter))
        );

        let below = body.find("# Projects").unwrap();
        assert_eq!(
            checker_for(path, body.as_bytes(), below),
            Some(CheckKind::Structural(grammars::Language::Markdown))
        );

        let fence = body.rfind("---").unwrap();
        assert_eq!(
            checker_for(path, body.as_bytes(), fence),
            Some(CheckKind::Structural(grammars::Language::Markdown))
        );
    }

    #[test]
    fn a_markdown_file_with_no_frontmatter_is_always_a_markdown_edit() {
        assert_eq!(
            checker_for(
                Path::new("notes.md"),
                b"# Heading\n\n---\n\nmore prose\n",
                0
            ),
            Some(CheckKind::Structural(grammars::Language::Markdown))
        );
    }

    #[test]
    fn frontmatter_spans_only_what_lies_between_the_fences() {
        // `---\n` is 0..4 and `a: 1\n` is 4..9, so the closing fence starts at 9.
        assert_eq!(frontmatter(b"---\na: 1\n---\nbody\n"), Some(4..9));
        assert_eq!(frontmatter(b"---\r\na: 1\r\n---\r\nbody\r\n"), Some(5..11));
        assert_eq!(frontmatter(b"---\n---\n"), Some(4..4));
        assert_eq!(frontmatter(b"# Heading\n---\na: 1\n---\n"), None);
        assert_eq!(
            frontmatter(b"---\na: 1\n"),
            None,
            "an unclosed fence is not frontmatter"
        );
        assert_eq!(frontmatter(b"----\na: 1\n----\n"), None);
        assert_eq!(frontmatter(b""), None);
    }

    fn absent_checker() -> &'static str {
        "lets-no-such-checker-on-path"
    }

    #[test]
    fn a_checker_that_is_not_on_path_is_skipped_by_name() {
        let verdict = run_command(
            &format!("{} --noEmit", absent_checker()),
            &[PathBuf::from("a.ts")],
            Duration::from_secs(5),
        );

        assert_eq!(
            verdict,
            CommandVerdict::Skipped(format!("{} absent", absent_checker()))
        );
    }

    #[test]
    fn a_checker_that_exits_zero_is_ok_and_one_that_exits_non_zero_failed() {
        assert_eq!(
            run_command("true", &[], Duration::from_secs(5)),
            CommandVerdict::Ok
        );
        assert_eq!(
            run_command("false", &[], Duration::from_secs(5)),
            CommandVerdict::Failed(String::new())
        );
    }

    #[test]
    fn a_clean_baseline_and_a_failing_post_write_run_reverts_the_batch() {
        let baseline = run_command("true", &[], Duration::from_secs(5));
        assert_eq!(baseline, CommandVerdict::Ok);

        let after = run_command("false", &[], Duration::from_secs(5)).against_baseline(&baseline);

        assert_eq!(after, CommandVerdict::Failed(String::new()));
    }

    #[test]
    fn a_baseline_that_already_failed_is_inconclusive_and_never_failed() {
        let baseline = run_command("false", &[], Duration::from_secs(5));
        assert_eq!(baseline, CommandVerdict::Failed(String::new()));

        let after = run_command("false", &[], Duration::from_secs(5)).against_baseline(&baseline);

        assert_eq!(
            after,
            CommandVerdict::Inconclusive("failed before and after".to_owned())
        );
        assert!(!matches!(after, CommandVerdict::Failed(_)));
    }

    #[test]
    fn a_baseline_that_already_failed_still_lets_a_clean_post_write_run_be_ok() {
        let baseline = run_command("false", &[], Duration::from_secs(5));

        assert_eq!(
            run_command("true", &[], Duration::from_secs(5)).against_baseline(&baseline),
            CommandVerdict::Ok
        );
    }

    #[test]
    fn a_baseline_with_no_exit_code_never_turns_a_later_failure_into_a_revert() {
        let timed_out = run_command("sleep 30", &[], Duration::from_millis(100));
        assert_eq!(
            timed_out,
            CommandVerdict::Inconclusive("timed out".to_owned())
        );

        let after = run_command("false", &[], Duration::from_secs(5)).against_baseline(&timed_out);

        assert!(!matches!(after, CommandVerdict::Failed(_)), "{after:?}");
        assert_eq!(
            after,
            CommandVerdict::Inconclusive("no baseline exit code: timed out".to_owned())
        );

        let absent = CommandVerdict::Skipped("tsc absent".to_owned());
        assert_eq!(
            run_command("false", &[], Duration::from_secs(5)).against_baseline(&absent),
            CommandVerdict::Inconclusive("no baseline exit code: tsc absent".to_owned())
        );
    }

    #[test]
    fn a_failing_checkers_first_line_comes_back_with_the_verdict() {
        assert_eq!(
            run_command(
                "sh -c 'echo \"a.ts(4,7): error TS2304\"; echo second; exit 1'",
                &[],
                Duration::from_secs(5)
            ),
            CommandVerdict::Failed("a.ts(4,7): error TS2304".to_owned())
        );
        assert_eq!(
            run_command(
                "sh -c 'echo from-stderr >&2; exit 2'",
                &[],
                Duration::from_secs(5)
            ),
            CommandVerdict::Failed("from-stderr".to_owned())
        );
    }

    #[test]
    fn a_checker_that_prints_a_wall_of_text_is_cut_at_one_kib() {
        let verdict = run_command(
            "head -c 4000 /dev/zero | tr '\\0' x; exit 1",
            &[],
            Duration::from_secs(5),
        );

        let CommandVerdict::Failed(excerpt) = verdict else {
            panic!("a non-zero exit is a failure: {verdict:?}");
        };
        assert_eq!(excerpt.len(), 1024);
    }

    #[test]
    fn a_substituted_path_reaches_the_checker_whole_however_it_is_spelled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a $b c.ts");
        std::fs::write(&path, b"const a = 1\n").unwrap();

        assert_eq!(
            run_command(
                "cat {}",
                std::slice::from_ref(&path),
                Duration::from_secs(5)
            ),
            CommandVerdict::Ok
        );

        let missing = dir.path().join("nope $x.ts");
        assert!(matches!(
            run_command("cat {}", &[missing], Duration::from_secs(5)),
            CommandVerdict::Failed(_)
        ));
    }

    #[test]
    fn a_checker_that_runs_past_the_timeout_is_inconclusive_not_failed() {
        let verdict = run_command("sleep 30", &[], Duration::from_millis(100));

        assert_eq!(
            verdict,
            CommandVerdict::Inconclusive("timed out".to_owned())
        );
    }

    #[test]
    fn the_braces_token_becomes_the_edited_files_and_a_command_without_it_is_untouched() {
        let files = [PathBuf::from("src/a.ts"), PathBuf::from("src/b.ts")];

        assert_eq!(
            substitute("eslint --fix {}", &files),
            "eslint --fix 'src/a.ts' 'src/b.ts'"
        );
        assert_eq!(substitute("tsc --noEmit", &files), "tsc --noEmit");
        assert_eq!(substitute("eslint {}", &[]), "eslint ");
    }

    #[test]
    fn the_checker_name_is_the_commands_first_word() {
        assert_eq!(checker_name("tsc --noEmit"), "tsc");
        assert_eq!(checker_name("  go vet ./..."), "go");
        assert_eq!(checker_name("cargo"), "cargo");
    }

    #[test]
    fn a_checkers_own_output_never_reaches_this_processs_stdout() {
        let run = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "check::tests::noisy_checker_fixture",
                "--nocapture",
            ])
            .env("LETS_CHECK_NOISY_FIXTURE", "1")
            .output()
            .unwrap();
        let out = String::from_utf8(run.stdout).unwrap();
        let err = String::from_utf8(run.stderr).unwrap();

        assert!(!out.contains("CHECKER_STDOUT"), "{out}");
        assert!(!err.contains("CHECKER_STDERR"), "{err}");
    }

    #[test]
    fn noisy_checker_fixture() {
        if std::env::var_os("LETS_CHECK_NOISY_FIXTURE").is_none() {
            return;
        }
        assert_eq!(
            run_command(
                "sh -c 'echo CHECKER_STDOUT; echo CHECKER_STDERR >&2'",
                &[],
                Duration::from_secs(5)
            ),
            CommandVerdict::Ok
        );
    }

    /// Canonical, so a resolved directory compares equal whatever the temp dir's spelling.
    fn tree(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let root = std::fs::canonicalize(dir.path()).expect("the temp dir exists");
        for (name, body) in files {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().expect("a relative name has a parent"))
                .expect("parent dirs");
            std::fs::write(&path, body).expect("fixture file");
        }
        (dir, root)
    }

    fn resolved(name: &str, file: &str, root: &Path) -> PresetResolution {
        resolve_preset(name, &root.join(file), root).expect("a known preset resolves")
    }

    fn runs(command: &str, dir: PathBuf) -> PresetResolution {
        PresetResolution::Run(Preset {
            command: command.to_owned(),
            dir,
        })
    }

    #[test]
    fn auto_under_a_cargo_manifest_runs_cargo_check_in_its_directory() {
        let (_dir, root) = tree(&[("proj/Cargo.toml", ""), ("proj/src/lib.rs", "")]);

        assert_eq!(
            resolved("@auto", "proj/src/lib.rs", &root),
            runs(
                "cargo check --workspace --quiet --all-targets",
                root.join("proj")
            )
        );
    }

    #[test]
    fn auto_under_a_go_module_runs_go_build_in_its_directory() {
        let (_dir, root) = tree(&[("svc/go.mod", ""), ("svc/pkg/a.go", "")]);

        assert_eq!(
            resolved("@auto", "svc/pkg/a.go", &root),
            runs("go build ./...", root.join("svc"))
        );
    }

    #[test]
    fn tsc_uses_the_typecheck_script_only_when_package_json_names_one_as_a_string() {
        let (_dir, root) = tree(&[
            ("web/tsconfig.json", "{}"),
            ("web/package.json", r#"{"scripts":{"typecheck":"tsc -b"}}"#),
            ("web/a.ts", ""),
            ("bare/tsconfig.json", "{}"),
            ("bare/package.json", r#"{"scripts":{"typecheck":true}}"#),
            ("bare/a.ts", ""),
            ("broken/tsconfig.json", "{}"),
            ("broken/package.json", "{not json"),
            ("broken/a.ts", ""),
        ]);

        assert_eq!(
            resolved("@auto", "web/a.ts", &root),
            runs("npm run -s typecheck", root.join("web"))
        );
        assert_eq!(
            resolved("@tsc", "bare/a.ts", &root),
            runs("npx --no-install tsc --noEmit -p .", root.join("bare"))
        );
        assert_eq!(
            resolved("@auto", "broken/a.ts", &root),
            runs("npx --no-install tsc --noEmit -p .", root.join("broken"))
        );
    }

    #[test]
    fn pyproject_or_setup_py_runs_py_compile_on_the_edited_files() {
        let (_dir, root) = tree(&[
            ("a/pyproject.toml", ""),
            ("a/m.py", ""),
            ("b/setup.py", ""),
            ("b/m.py", ""),
        ]);

        assert_eq!(
            resolved("@auto", "a/m.py", &root),
            runs("python3 -m py_compile {}", root.join("a"))
        );
        assert_eq!(
            resolved("@py", "b/m.py", &root),
            runs("python3 -m py_compile {}", root.join("b"))
        );
    }

    #[test]
    fn the_nearest_manifest_wins_over_one_further_up() {
        let (_dir, root) = tree(&[
            ("go.mod", ""),
            ("sub/Cargo.toml", ""),
            ("sub/lib.rs", ""),
            ("cmd/main.go", ""),
        ]);

        assert_eq!(
            resolved("@auto", "sub/lib.rs", &root),
            runs(
                "cargo check --workspace --quiet --all-targets",
                root.join("sub")
            )
        );
        assert_eq!(
            resolved("@auto", "cmd/main.go", &root),
            runs("go build ./...", root.clone())
        );
    }

    #[test]
    fn within_one_directory_the_table_order_decides() {
        let (_dir, root) = tree(&[("pyproject.toml", ""), ("go.mod", ""), ("a.go", "")]);

        assert_eq!(
            resolved("@auto", "a.go", &root),
            runs("go build ./...", root.clone())
        );
    }

    #[test]
    fn each_file_walks_up_from_its_own_directory() {
        let (_dir, root) = tree(&[("sub/Cargo.toml", ""), ("sub/a.rs", ""), ("other/b.rs", "")]);

        let both = preset_groups(
            "@auto",
            &[root.join("sub/a.rs"), root.join("other/b.rs")],
            &root,
        )
        .expect("@auto resolves");

        assert_eq!(both, PresetGroups {
            groups: vec![(
                Preset {
                    command: "cargo check --workspace --quiet --all-targets".to_owned(),
                    dir: root.join("sub"),
                },
                vec![root.join("sub/a.rs")],
            )],
            unmatched: vec![root.join("other/b.rs")],
            reason: "@auto found no manifest".to_owned(),
        });
    }

    #[test]
    fn two_crates_are_two_groups_in_directory_order() {
        let (_dir, root) = tree(&[
            ("b/Cargo.toml", ""),
            ("b/n.txt", ""),
            ("a/Cargo.toml", ""),
            ("a/n.txt", ""),
            ("a/m.txt", ""),
        ]);

        let both = preset_groups(
            "@cargo",
            &[
                root.join("b/n.txt"),
                root.join("a/n.txt"),
                root.join("a/m.txt"),
            ],
            &root,
        )
        .expect("@cargo resolves");

        let cargo = |dir: &str| Preset {
            command: "cargo check --workspace --quiet --all-targets".to_owned(),
            dir: root.join(dir),
        };
        assert_eq!(both.groups, vec![
            (cargo("a"), vec![root.join("a/n.txt"), root.join("a/m.txt")]),
            (cargo("b"), vec![root.join("b/n.txt")]),
        ]);
        assert!(both.unmatched.is_empty());
    }

    #[test]
    fn a_manifest_only_above_the_root_is_never_used() {
        let (_dir, parent) = tree(&[("Cargo.toml", ""), ("root/a.rs", "")]);
        let root = parent.join("root");

        assert_eq!(
            resolved("@auto", "a.rs", &root),
            PresetResolution::Skipped("@auto found no manifest".to_owned())
        );
        assert_eq!(
            resolved("@auto", "root/a.rs", &parent),
            runs(
                "cargo check --workspace --quiet --all-targets",
                parent.clone()
            ),
            "control: the same manifest inside the root is found"
        );
    }

    #[test]
    fn a_forced_preset_looks_for_its_own_manifest_only() {
        let (_dir, root) = tree(&[("go.mod", ""), ("a.go", "")]);

        assert_eq!(
            resolved("@cargo", "a.go", &root),
            PresetResolution::Skipped("@cargo found no Cargo.toml".to_owned())
        );
        assert_eq!(
            resolved("@go", "a.go", &root),
            runs("go build ./...", root.clone())
        );
        assert_eq!(
            resolved("@tsc", "a.go", &root),
            PresetResolution::Skipped("@tsc found no tsconfig.json".to_owned())
        );
        assert_eq!(
            resolved("@py", "a.go", &root),
            PresetResolution::Skipped("@py found no pyproject.toml or setup.py".to_owned())
        );
    }

    #[test]
    fn an_unknown_preset_is_a_usage_error_listing_the_five_names() {
        let (_dir, root) = tree(&[("Cargo.toml", ""), ("a.rs", "")]);

        let err = resolve_preset("@nope", &root.join("a.rs"), &root)
            .expect_err("@nope is not in the table");

        assert_eq!(err.slug(), "usage");
        assert_eq!(
            err.to_string(),
            "unknown preset @nope \u{b7} the presets are @auto, @cargo, @go, @tsc, @py"
        );
    }

    #[test]
    fn run_command_in_runs_the_checker_in_the_given_directory() {
        let (_dir, root) = tree(&[("sub/keep", "")]);
        let sub = root.join("sub");

        let verdict = run_command_in("pwd > out", &[], Duration::from_secs(5), &sub);

        assert_eq!(verdict, CommandVerdict::Ok);
        let written = std::fs::read_to_string(sub.join("out")).expect("pwd wrote into sub");
        assert_eq!(written.trim_end(), sub.display().to_string());
        assert!(
            !root.join("out").exists(),
            "control: nothing ran in the parent"
        );
    }

    #[test]
    fn run_command_resolves_a_preset_through_the_table() {
        let Some(repo) = crate::own_process::repo() else {
            return;
        };
        let file = repo.path().join("notes.txt");
        std::fs::write(&file, "x\n").expect("fixture file");

        assert_eq!(
            run_command("@auto", std::slice::from_ref(&file), Duration::from_secs(5)),
            CommandVerdict::Skipped("@auto found no manifest".to_owned())
        );
        assert_eq!(
            run_command("true", &[file], Duration::from_secs(5)),
            CommandVerdict::Ok,
            "control: a literal command runs as typed"
        );
    }
}
