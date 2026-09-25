use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::Read;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, mpsc};

use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::overrides::{Override, OverrideBuilder};
use ignore::{DirEntry, IncrementalIgnore, WalkBuilder, WalkState};
use regex::{Regex, RegexBuilder};
use tree_sitter::{Node, Query};

use crate::cli::{FindArgs, Global};
use crate::error::Error;
use crate::grammars::{self, Language};
use crate::hook::bre;
use crate::output::{
    Body, ByteLimit, CountRow, ExpandedHits, Footer, IgnoredDirs, Line, Marker, NamedDirs,
    Omission, Response, Stats, TargetBlock,
};
use crate::{Outcome, fs, symbols, window};

pub fn run(args: &FindArgs, global: &Global, _format: crate::output::Format) -> Outcome {
    run_with(args, global, None)
}

/// A grep habit like `a\|b` is a literal bar to `regex`, so a pattern with no hits is retried as
/// grep reads it, and the footer names that reading.
fn run_with(args: &FindArgs, global: &Global, traversal: Option<Traversal>) -> Outcome {
    let (literal, hits) = search(args, global, traversal);
    if hits != Some(0) || args.fixed_string {
        return literal;
    }
    let Some(read_as) = bre::grep_reading(&args.pattern) else {
        return literal;
    };
    let grep_args = FindArgs {
        pattern: read_as.clone(),
        ..args.clone()
    };
    let (mut grep_style, grep_hits) = search(&grep_args, global, traversal);
    if grep_hits.is_some_and(|hits| hits > 0) {
        grep_style.response.omitted.insert(0, Omission::GrepStyle {
            pattern: args.pattern.clone(),
            read_as,
        });
        return grep_style;
    }
    Outcome {
        error: literal.error.map(|error| tried_grep_style(error, &read_as)),
        ..literal
    }
}

fn tried_grep_style(error: Error, read_as: &str) -> Error {
    match error {
        Error::NoHits { pattern, .. } => Error::NoHits {
            pattern,
            grep_style: Some(read_as.to_owned()),
        },
        Error::Several { errors } => Error::Several {
            errors: errors
                .into_iter()
                .map(|error| tried_grep_style(error, read_as))
                .collect(),
        },
        other => other,
    }
}

fn search(
    args: &FindArgs,
    global: &Global,
    traversal: Option<Traversal>,
) -> (Outcome, Option<usize>) {
    let (search_paths, missing) = match search_roots(args, global.allow_outside) {
        Ok(roots) => roots,
        Err(err) => return (Outcome::failed("find", err), None),
    };
    let unresolved: Vec<Omission> = missing
        .iter()
        .map(|(typed, error)| Omission::Unresolved {
            target: typed.display().to_string(),
            error: error.slug(),
        })
        .collect();
    let missing: Vec<Error> = missing.into_iter().map(|(_, error)| error).collect();
    if search_paths.is_empty() {
        let error = Error::all(missing).expect("a root is either searched or missing");
        return (Outcome::failed("find", error), None);
    }
    let matcher = match build_matcher(args) {
        Ok(matcher) => matcher,
        Err(err) => return (Outcome::failed("find", err), None),
    };
    let spans = match build_spans(args) {
        Ok(spans) => spans,
        Err(err) => return (Outcome::failed("find", err), None),
    };
    let overrides = match build_overrides(&args.globs) {
        Ok(overrides) => overrides,
        Err(err) => return (Outcome::failed("find", err), None),
    };
    let search = Search {
        args,
        matcher: &matcher,
        spans: &spans,
        max_file_bytes: global.max_file_bytes,
    };

    let mut walk = Walk::default();
    let mut found = Found::default();
    let (builder, filters) = walker(&search_paths, !args.hidden, !global.no_ignore, &overrides);
    match traversal.unwrap_or_else(|| traversal_of(&search_paths)) {
        Traversal::Sequential => search.sequential(builder, filters, &mut walk, &mut found),
        Traversal::Parallel { threads } => search.parallel(
            builder,
            filters.as_ref(),
            threads,
            &search_paths,
            &mut walk,
            &mut found,
        ),
    }
    let walked = walk_omissions(args, &walk);
    let hits = found.total_hits;
    let mut outcome = if args.files {
        let mut files = found.files;
        files.sort();
        respond_files(files, walk.searched, &args.pattern, walked)
    } else if args.count {
        let mut counts = found.counts;
        counts.sort_by(|a, b| a.path.cmp(&b.path));
        respond_counts(counts, found.total_hits, &args.pattern, walked)
    } else {
        respond_targets(found, &mut walk, &search, global, walked)
    };
    // The walker skips a missing root silently, so it is named here and its error comes first.
    if !missing.is_empty() {
        outcome.response.omitted.extend(unresolved);
        let mut errors = missing;
        errors.extend(outcome.error.take());
        outcome.error = Error::all(errors);
    }
    (outcome, Some(hits))
}

/// Drops every root another root contains, or `find x . src` would count `src`'s hits twice. The
/// typed form survives: a canonical root is absolute, and differs between machines.
fn search_roots(args: &FindArgs, allow_outside: bool) -> Result<SearchRoots, Error> {
    let typed: Vec<PathBuf> = if args.paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        args.paths.iter().map(PathBuf::from).collect()
    };

    let mut guarded = Vec::with_capacity(typed.len());
    let mut missing = Vec::new();
    for path in typed {
        let canonical = fs::guard_scope(&path, allow_outside)?;
        // Before the containment pass, which would fold a missing `nope` into `.`.
        match std::fs::metadata(&path) {
            Ok(_) => guarded.push((canonical, path)),
            Err(source) => missing.push((path.clone(), Error::Io { path, source })),
        }
    }
    // Sorted, a path follows every path containing it, so only the last kept root needs checking.
    guarded.sort();

    let mut roots: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(guarded.len());
    for (canonical, typed) in guarded {
        if roots
            .last()
            .is_some_and(|(kept, _)| canonical.starts_with(kept))
        {
            continue;
        }
        roots.push((canonical, typed));
    }
    Ok((roots.into_iter().map(|(_, typed)| typed).collect(), missing))
}

type SearchRoots = (Vec<PathBuf>, Vec<(PathBuf, Error)>);

fn base_pattern(args: &FindArgs) -> String {
    if args.fixed_string {
        regex::escape(&args.pattern)
    } else {
        args.pattern.clone()
    }
}

fn build_matcher(args: &FindArgs) -> Result<RegexMatcher, Error> {
    RegexMatcherBuilder::new()
        .case_insensitive(args.ignore_case)
        .word(args.word)
        .build(&base_pattern(args))
        .map_err(|err| invalid_pattern(&args.pattern, &err.to_string()))
}

/// Compiled a second time for the match offsets: a `Sink` gets only the line, and the offsets'
/// trait would be an 18th direct dependency. `-w` adds `grep-regex`'s own half word boundaries.
fn build_spans(args: &FindArgs) -> Result<Regex, Error> {
    let base = base_pattern(args);
    let pattern = if args.word {
        format!(r"\b{{start-half}}(?:{base})\b{{end-half}}")
    } else {
        base
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(args.ignore_case)
        .build()
        .map_err(|err| invalid_pattern(&args.pattern, &err.to_string()))
}

fn invalid_pattern(pattern: &str, error: &str) -> Error {
    Error::InvalidPattern {
        pattern: pattern.to_owned(),
        message: summary_of(error),
    }
}

/// A regex error's `Display` is a four-line diagram; stderr carries only its `error:` line.
fn summary_of(error: &str) -> String {
    error
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("error: "))
        .unwrap_or_else(|| error.lines().next().unwrap_or_default())
        .to_owned()
}

/// Relative to the working directory, the frame of every typed root and rendered path.
fn build_overrides(globs: &[String]) -> Result<Override, Error> {
    if globs.is_empty() {
        return Ok(Override::empty());
    }
    let mut builder = OverrideBuilder::new(".");
    for glob in globs {
        builder.add(glob).map_err(|err| invalid_glob(glob, &err))?;
    }
    builder
        .build()
        .map_err(|err| invalid_glob(&globs.join(", "), &err))
}

fn invalid_glob(glob: &str, error: &ignore::Error) -> Error {
    let message = match error {
        ignore::Error::Glob { err, .. } => err.clone(),
        other => other.to_string(),
    };
    Error::InvalidPattern {
        pattern: glob.to_owned(),
        message,
    }
}

/// BOM sniffing would transcode UTF-16 and search it, so it is off and `Utf16Guard` refuses one.
fn build_searcher(args: &FindArgs) -> Searcher {
    let before = args.before.or(args.context).unwrap_or(0);
    let after = args.after.or(args.context).unwrap_or(0);
    SearcherBuilder::new()
        .before_context(before)
        .after_context(after)
        .binary_detection(BinaryDetection::quit(b'\0'))
        .bom_sniffing(false)
        .build()
}

/// `ignore` reports nothing it filtered out, so the walk runs with its gitignore and dotfile
/// filters off and each traversal applies `Filters`, `ignore`'s own matchers, in their place.
/// `None` when neither filter is on.
fn walker(
    paths: &[PathBuf],
    hidden: bool,
    gitignore: bool,
    globs: &Override,
) -> (WalkBuilder, Option<Filters>) {
    let mut builder = WalkBuilder::new(&paths[0]);
    for path in &paths[1..] {
        builder.add(path);
    }
    builder
        // As with `rg -g`, a matching glob outranks gitignore and the dotfile rule for that file.
        .overrides(globs.clone())
        .hidden(false)
        .ignore(gitignore)
        .git_ignore(gitignore)
        .git_global(gitignore)
        .git_exclude(gitignore);
    // Built while the builder's filters are on: a matcher snapshots them.
    let filters = (hidden || gitignore).then(|| {
        let matchers = builder.build_matchers();
        Filters {
            roots: paths.to_vec(),
            fresh: matchers.clone(),
            matchers,
            hidden,
            parents: 0,
            parent: PathBuf::new(),
        }
    });
    builder
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        // `ignore` never applies this predicate to a root entry, so every consumer re-tests it.
        .filter_entry(|entry| !in_git_dir(entry.path()));
    (builder, filters)
}

/// A sequential walk cannot skip a directory it has yielded, so there the filters run as the entry
/// predicate, replacing the walker's `.git` one. Rejections land in the returned list.
fn judge_as_predicate(
    builder: &mut WalkBuilder,
    filters: Option<Filters>,
) -> Arc<Mutex<Vec<Rejection>>> {
    let rejected = Arc::new(Mutex::new(Vec::new()));
    let Some(filters) = filters else {
        return rejected;
    };
    let filters = Mutex::new(filters);
    let sink = Arc::clone(&rejected);
    builder.filter_entry(move |entry| {
        if in_git_dir(entry.path()) {
            return false;
        }
        let verdict = filters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .judge(entry);
        let Some(rejection) = verdict else {
            return true;
        };
        sink.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(rejection);
        false
    });
    rejected
}

/// One matcher per root, in `roots` order, built with the dotfile rule off: a match is
/// gitignore's, and the dotfile rule applies only where gitignore said nothing, as in `ignore`.
/// A matcher keeps the compiled rules of every directory it judged a child of and never drops
/// one, so each copy goes back to `fresh`, the matchers as built, every `MATCHER_PARENTS_MAX`
/// directories: its memory is bounded by that times the tree's depth, not by the tree.
#[derive(Clone)]
struct Filters {
    roots: Vec<PathBuf>,
    matchers: Vec<IncrementalIgnore>,
    fresh: Vec<IncrementalIgnore>,
    hidden: bool,
    /// Directories whose children this copy judged since it was last fresh.
    parents: usize,
    parent: PathBuf,
}

/// Measured on the musl build over a 75,661-directory tree at 24 threads, p50 and peak RSS: 205 ms
/// and 96 MB at 16, 190 ms and 102 MB at 64, 189 ms and 114 MB at 256; never reset, 378 MB.
const MATCHER_PARENTS_MAX: usize = 64;

enum Rejection {
    GitignoredDir(PathBuf),
    HiddenDir(PathBuf),
    Gitignored,
    Dotfile,
    /// A symlink or special file: never a search candidate, so no bucket claims it.
    NotAFile,
}

impl Filters {
    /// `ignore` never filters a root entry, whatever its name.
    fn judge(&mut self, entry: &DirEntry) -> Option<Rejection> {
        if entry.depth() == 0 {
            return None;
        }
        if let Some(parent) = entry.path().parent()
            && parent.as_os_str() != self.parent.as_os_str()
        {
            self.parents += 1;
            if self.parents > MATCHER_PARENTS_MAX {
                self.matchers.clone_from(&self.fresh);
                self.parents = 0;
            }
            parent.clone_into(&mut self.parent);
        }
        let kind = entry.file_type();
        let is_dir = kind.is_some_and(|kind| kind.is_dir());
        let root = root_index(entry.path(), &self.roots);
        let relative = entry
            .path()
            .strip_prefix(&self.roots[root])
            .unwrap_or(entry.path());
        let matched = self.matchers[root].matched(relative, is_dir);
        let dotfile = self.hidden
            && matched.is_none()
            && entry.file_name().as_encoded_bytes().first() == Some(&b'.');
        if !matched.is_ignore() && !dotfile {
            return None;
        }
        Some(if is_dir && dotfile {
            Rejection::HiddenDir(display_path(entry.path()))
        } else if is_dir {
            Rejection::GitignoredDir(display_path(entry.path()))
        } else if !kind.is_some_and(|kind| kind.is_file()) {
            Rejection::NotAFile
        } else if dotfile {
            Rejection::Dotfile
        } else {
            Rejection::Gitignored
        })
    }
}

fn in_git_dir(path: &Path) -> bool {
    path.components().any(|part| part.as_os_str() == ".git")
}

fn display_path(path: &Path) -> PathBuf {
    path.strip_prefix(".")
        .map_or_else(|_| path.to_path_buf(), Path::to_path_buf)
}

struct HitSink<'a> {
    spans: &'a Regex,
    hits: usize,
    lines: Vec<Line>,
    /// Per line, the first wrapped match in its text: what `window::cut_long_lines` keeps.
    first_match: Vec<Option<Range<usize>>>,
    /// Ascending: looked up by binary search.
    lossy_lines: Vec<usize>,
    binary: bool,
}

impl<'a> HitSink<'a> {
    fn new(spans: &'a Regex) -> HitSink<'a> {
        HitSink {
            spans,
            hits: 0,
            lines: Vec::new(),
            first_match: Vec::new(),
            lossy_lines: Vec::new(),
            binary: false,
        }
    }

    fn push(&mut self, bytes: &[u8], number: Option<u64>, marker: Marker) -> &mut Line {
        let (line, lossy) = sink_line(bytes, number, marker);
        if lossy {
            self.lossy_lines.push(line.number);
        }
        self.lines.push(line);
        self.lines.last_mut().expect("just pushed")
    }
}

impl Sink for HitSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        self.hits += 1;
        let spans = self.spans;
        let line = self.push(mat.bytes(), mat.line_number(), Marker::Hit);
        let (wrapped, first) = wrap_matches(&line.text, spans);
        line.text = wrapped.into();
        self.first_match.push(first);
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        ctx: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        self.push(ctx.bytes(), ctx.line_number(), Marker::Context);
        self.first_match.push(None);
        Ok(true)
    }

    fn binary_data(&mut self, _searcher: &Searcher, _offset: u64) -> Result<bool, Self::Error> {
        self.binary = true;
        Ok(false)
    }
}

/// Wrapped `«»` so matches survive a pipe; spliced last span first, so no splice moves a later
/// span's offsets.
fn wrap_matches(text: &str, spans: &Regex) -> (String, Option<Range<usize>>) {
    let found: Vec<(usize, usize)> = spans
        .find_iter(text)
        .filter(|found| !found.is_empty())
        .map(|found| (found.start(), found.end()))
        .collect();
    let Some(&(first_start, first_end)) = found.first() else {
        return (text.to_owned(), None);
    };
    let marks = '\u{ab}'.len_utf8() + '\u{bb}'.len_utf8();
    let mut wrapped = String::with_capacity(text.len() + found.len() * marks);
    wrapped.push_str(text);
    for (start, end) in found.into_iter().rev() {
        wrapped.insert(end, '\u{bb}');
        wrapped.insert(start, '\u{ab}');
    }
    (wrapped, Some(first_start..first_end + marks))
}

/// rg searches bytes, so a file is never refused over its encoding; only a shown line is decoded.
fn sink_line(bytes: &[u8], number: Option<u64>, marker: Marker) -> (Line, bool) {
    let end = bytes
        .iter()
        .rposition(|byte| !matches!(byte, b'\n' | b'\r'))
        .map_or(0, |last| last + 1);
    let (text, lossy) = match String::from_utf8_lossy(&bytes[..end]) {
        Cow::Borrowed(text) => (text.to_owned(), false),
        Cow::Owned(text) => (text, true),
    };
    let line = Line {
        number: usize::try_from(number.unwrap_or(0)).unwrap_or(usize::MAX),
        marker,
        text: text.into(),
    };
    (line, lossy)
}

fn hit_block(path: PathBuf, lines: Vec<Line>) -> TargetBlock {
    TargetBlock {
        target: path.display().to_string(),
        path,
        span: None,
        window: None,
        not_shown: None,
        resolver: None,
        crlf: false,
        lossy_lines: Vec::new(),
        sha: None,
        lines,
    }
}

fn no_match_error(pattern: &str) -> Error {
    Error::NoHits {
        pattern: pattern.to_owned(),
        grep_style: None,
    }
}

fn hit_summary(hits: usize, files: usize, searched: usize) -> String {
    format!(
        "{hits} hit{h} in {files} file{f} \u{b7} searched {searched} file{s}",
        h = plural(hits),
        f = plural(files),
        s = plural(searched),
    )
}

/// Bounds the over-cap file map, so a large-repo miss reads in one call, not 1,000 names.
const TOP_FILES_SHOWN: usize = 10;

/// Bounds the over-cap preview as `TOP_FILES_SHOWN` bounds the file map beside it.
const BUSIEST_HITS_SHOWN: usize = 10;

fn respond_targets(
    found: Found,
    walk: &mut Walk,
    search: &Search<'_>,
    global: &Global,
    walked: Vec<Omission>,
) -> Outcome {
    let args = search.args;
    if found.total_hits > args.cap {
        return respond_over_cap(found, walk, args, global, walked);
    }
    let Found {
        total_hits,
        mut blocks,
        first_matches,
        ..
    } = found;
    let matched_files = blocks.len();
    let searched = walk.searched;
    let mut response = Response::empty("find");

    let mut long_lines_cut: usize = blocks
        .iter_mut()
        .zip(&first_matches)
        .map(|(block, first_match)| window::cut_long_lines(&mut block.lines, first_match))
        .sum();
    if expands(args, global, total_hits, matched_files) {
        let (limit, limit_bytes) = match global.budget {
            Some(budget) => (
                ByteLimit::Budget(budget),
                budget.saturating_mul(window::BYTES_PER_TOKEN),
            ),
            None => (ByteLimit::MaxBytes(global.max_bytes), global.max_bytes),
        };
        // Context gets only what the bare hits leave, so no trim or refusal can cost a hit line.
        let bytes_left = limit_bytes.saturating_sub(content_size(&blocks).1);
        let expansion = expand(&blocks, search, limit, bytes_left);
        long_lines_cut += expansion.long_lines_cut;
        response.omitted.extend(expansion.named());
        for ((block, lines), lossy) in blocks.iter_mut().zip(expansion.blocks).zip(expansion.lossy)
        {
            block.lines = lines;
            if !lossy.is_empty() {
                let known = walk.lossy.entry(block.path.clone()).or_default();
                known.extend(lossy);
                known.sort_unstable();
                known.dedup();
            }
        }
    }

    if let Some(budget) = global.budget {
        response
            .omitted
            .extend(window::trim_to_budget(&mut blocks, budget));
    }
    if long_lines_cut > 0 {
        response.omitted.push(Omission::LongLinesCut {
            lines: long_lines_cut,
        });
    }
    let lossy_lines = lossy_shown(&blocks, &walk.lossy);
    if lossy_lines > 0 {
        response
            .omitted
            .push(Omission::LossyLines { lines: lossy_lines });
    }
    response.omitted.extend(walked);
    let (lines, bytes) = content_size(&blocks);
    response.stats = Stats::new(lines, bytes);
    response.footer = Footer {
        summary: hit_summary(total_hits, matched_files, searched),
    };
    response.body = Body::Targets(blocks);

    // Refused whole rather than trimmed, as in `show`: the caller decides what to ask for next.
    if global.budget.is_none() && bytes > global.max_bytes {
        return Outcome::failed("find", Error::OverBudget {
            bytes: u64::try_from(bytes).unwrap_or(u64::MAX),
            limit: u64::try_from(global.max_bytes).unwrap_or(u64::MAX),
        });
    }
    if total_hits == 0 {
        return Outcome::partial(response, no_match_error(&args.pattern));
    }
    Outcome::ok(response)
}

fn respond_over_cap(
    found: Found,
    walk: &Walk,
    args: &FindArgs,
    global: &Global,
    walked: Vec<Omission>,
) -> Outcome {
    let Found {
        total_hits,
        blocks,
        first_matches,
        file_hits,
        ..
    } = found;
    let matched_files = blocks.len();
    let mut response = Response::empty("find");
    let mut top_files = file_hits;
    top_files.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.path.cmp(&b.path)));
    let preview = top_files.first().and_then(|busiest| {
        busiest_preview(blocks, first_matches, busiest, walk, global, args.cap)
    });
    let shown = top_files.len().min(TOP_FILES_SHOWN);
    let more = top_files.len() - shown;
    top_files.truncate(shown);

    let mut omitted = vec![Omission::HitCap {
        hits: total_hits,
        cap: args.cap,
    }];
    omitted.extend(walked);
    if let Some((block, preview_omitted)) = preview {
        omitted.extend(preview_omitted);
        let (lines, bytes) = content_size(std::slice::from_ref(&block));
        response.stats = Stats::new(lines, bytes);
        response.body = Body::Targets(vec![block]);
    }
    if !top_files.is_empty() {
        omitted.push(Omission::TopFiles { shown });
    }
    response.omitted = omitted;
    response.top_files = top_files;
    response.top_files_more = more;
    response.footer = Footer {
        summary: hit_summary(total_hits, matched_files, walk.searched),
    };
    Outcome::partial(response, Error::OverCap {
        hits: total_hits,
        files: matched_files,
        cap: args.cap,
    })
}

fn lossy_shown(blocks: &[TargetBlock], lossy: &BTreeMap<PathBuf, Vec<usize>>) -> usize {
    blocks
        .iter()
        .filter_map(|block| {
            let lossy = lossy.get(&block.path)?;
            Some(
                block
                    .lines
                    .iter()
                    .filter(|line| lossy.binary_search(&line.number).is_ok())
                    .count(),
            )
        })
        .sum()
}

/// Hit lines only, so `-C` cannot crowd the hits out of the preview, and never more than `--cap`
/// asked for. Left out, rather than refused, when it alone is over `--max-bytes`: the over-cap
/// exit stays the answer.
fn busiest_preview(
    mut blocks: Vec<TargetBlock>,
    mut first_matches: Vec<Vec<Option<Range<usize>>>>,
    busiest: &CountRow,
    walk: &Walk,
    global: &Global,
    cap: usize,
) -> Option<(TargetBlock, Vec<Omission>)> {
    let shown = BUSIEST_HITS_SHOWN.min(cap);
    if shown == 0 {
        return None;
    }
    let index = blocks.iter().position(|block| block.path == busiest.path)?;
    let mut block = blocks.swap_remove(index);
    let (mut lines, first_match): (Vec<Line>, Vec<Option<Range<usize>>>) =
        std::mem::take(&mut block.lines)
            .into_iter()
            .zip(first_matches.swap_remove(index))
            .filter(|(line, _)| line.marker == Marker::Hit)
            .take(shown)
            .unzip();
    let cut = window::cut_long_lines(&mut lines, &first_match);
    block.lines = lines;
    let mut preview = [block];
    let trimmed = match global.budget {
        Some(budget) => window::trim_to_budget(&mut preview, budget),
        None if window::content_bytes(&preview[0].lines) > global.max_bytes => return None,
        None => Vec::new(),
    };
    let mut omitted = vec![Omission::BusiestFile {
        shown: preview[0].lines.len(),
        hits: busiest.count,
    }];
    omitted.extend(trimmed);
    if cut > 0 {
        omitted.push(Omission::LongLinesCut { lines: cut });
    }
    let lossy = lossy_shown(&preview, &walk.lossy);
    if lossy > 0 {
        omitted.push(Omission::LossyLines { lines: lossy });
    }
    let [block] = preview;
    Some((block, omitted))
}

/// Measured over the 2026-09-24 agent trials: a Sonnet session viewed a file right after a search
/// named it 5.2 times. A result this small prints its hits in context instead; the hit, file,
/// symbol and window limits are sized to that follow-up view.
const EXPAND_MAX_HITS: usize = 10;
const EXPAND_MAX_FILES: usize = 3;
/// A symbol outside these lengths gets `EXPAND_AROUND` lines either side of the hit instead: a
/// longer one is a view of its own, and a one- or two-line one (a `key: value` pair, a one-line
/// function) adds no context.
const EXPAND_SYMBOL_MIN_LINES: usize = 3;
const EXPAND_SYMBOL_MAX_LINES: usize = 40;
const EXPAND_AROUND: usize = 5;
/// About 1.9k tokens, what the same trials measured one agent request to cost: the expansion is
/// never dearer than the view it saves.
const EXPAND_MAX_LINES: usize = 80;
/// A larger file is not parsed, so its hits get `EXPAND_AROUND` lines. Parsing grows about
/// linearly with size: on the musl dist build (2026-09-24, load 1.6) one expanded hit took
/// 27/33/16 ms p50 and 20/23/25 MB peak RSS at 128 KiB of TypeScript, one-line minified JS and
/// pretty JSON, about the `FIND_EXPANDED` gate; 256 KiB took 49/62/25 ms. Bare, 2 ms.
const EXPAND_PARSE_MAX_BYTES: usize = 128 * 1024;

/// `--json` and `--jsonl` feed programs, which take the hits, not a reader's context.
fn expands(args: &FindArgs, global: &Global, hits: usize, files: usize) -> bool {
    !(args.no_expand || global.json || global.jsonl)
        && args.after.is_none()
        && args.before.is_none()
        && args.context.is_none()
        && (1..=EXPAND_MAX_HITS).contains(&hits)
        && files <= EXPAND_MAX_FILES
}

/// Hits are taken in render order, so which stay bare past a limit is deterministic.
fn expand(
    blocks: &[TargetBlock],
    search: &Search<'_>,
    limit: ByteLimit,
    bytes_left: usize,
) -> Expansion {
    let mut expansion = Expansion {
        blocks: Vec::with_capacity(blocks.len()),
        lossy: Vec::with_capacity(blocks.len()),
        long_lines_cut: 0,
        symbols: 0,
        windows: 0,
        unparsed: 0,
        unexpanded: 0,
        over_limit: 0,
        lines_left: EXPAND_MAX_LINES,
        bytes_left,
        limit,
    };
    let mut queries = Vec::new();
    for block in blocks {
        let raw = std::fs::read(&block.path).ok().filter(|raw| {
            u64::try_from(raw.len()).is_ok_and(|bytes| bytes <= search.max_file_bytes)
        });
        let (lines, lossy) = match raw {
            Some(raw) => expansion.block(block, &raw, search.spans, &mut queries),
            // Gone or grown since the search read it: its hits print as found.
            None => (block.lines.clone(), Vec::new()),
        };
        expansion.blocks.push(lines);
        expansion.lossy.push(lossy);
    }
    expansion
}

struct Expansion {
    /// One entry per block, in block order; so is `lossy`, the non-UTF-8 lines each block gained.
    blocks: Vec<Vec<Line>>,
    lossy: Vec<Vec<usize>>,
    long_lines_cut: usize,
    symbols: usize,
    windows: usize,
    unparsed: usize,
    unexpanded: usize,
    over_limit: usize,
    lines_left: usize,
    /// What `limit` leaves once every bare hit line is counted.
    bytes_left: usize,
    limit: ByteLimit,
}

/// Once per call, however many files of the language expand: compiling the TypeScript query costs
/// about as much as parsing a 2,000-line file, 3.4 and 4.1 ms.
fn compiled(queries: &mut Vec<(Language, Query)>, lang: Language) -> Option<&Query> {
    if !queries.iter().any(|(compiled, _)| *compiled == lang) {
        queries.push((lang, symbols::query(lang)?));
    }
    queries
        .iter()
        .find(|(compiled, _)| *compiled == lang)
        .map(|(_, query)| query)
}

impl Expansion {
    fn named(&self) -> Option<Omission> {
        (self.symbols + self.windows + self.unexpanded + self.over_limit > 0).then_some(
            Omission::Expanded(ExpandedHits {
                symbols: self.symbols,
                windows: self.windows,
                around: EXPAND_AROUND,
                unparsed: self.unparsed,
                parse_max_kib: EXPAND_PARSE_MAX_BYTES / 1024,
                unexpanded: self.unexpanded,
                line_cap: EXPAND_MAX_LINES,
                over_limit: self.over_limit,
                limit: self.limit,
            }),
        )
    }

    /// Each hit takes its symbol if that fits both limits, else its window if that does, else
    /// stays bare. A hit whose window is its own line alone is left out of every count: nothing
    /// could be added.
    fn block(
        &mut self,
        block: &TargetBlock,
        raw: &[u8],
        spans: &Regex,
        queries: &mut Vec<(Language, Query)>,
    ) -> (Vec<Line>, Vec<usize>) {
        let content = String::from_utf8_lossy(raw);
        let text = Text::new(&content);
        let lang = block
            .path
            .extension()
            .and_then(|ext| ext.to_str())
            .and_then(grammars::from_extension);
        let unparsed = lang.is_some() && raw.len() > EXPAND_PARSE_MAX_BYTES;
        let lang = lang.filter(|_| !unparsed);
        let query = lang.and_then(|lang| compiled(queries, lang));
        let mut enclosing = Enclosing::new(lang, query, &content);
        let raw_lines: Vec<&[u8]> = raw.split(|byte| *byte == b'\n').collect();
        let mut rendered = BTreeMap::new();
        let mut covered: Vec<(usize, usize)> = Vec::new();
        for hit in block.lines.iter().map(|line| line.number) {
            if hit == 0 || hit > text.total {
                continue;
            }
            let around = (
                hit.saturating_sub(EXPAND_AROUND).max(1),
                (hit + EXPAND_AROUND).min(text.total),
            );
            if around.0 == around.1 {
                continue;
            }
            let symbol = enclosing
                .symbol(hit, &text, spans)
                .map(|(start, end)| (start, end.min(text.total)))
                .filter(|(start, end)| {
                    (EXPAND_SYMBOL_MIN_LINES..=EXPAND_SYMBOL_MAX_LINES).contains(&(end + 1 - start))
                });
            let mut over_lines = false;
            let mut taken = None;
            for range in symbol.into_iter().chain([around]) {
                let (lines, bytes) = added(block, &raw_lines, &mut rendered, &covered, range);
                if lines <= self.lines_left && bytes <= self.bytes_left {
                    self.lines_left -= lines;
                    self.bytes_left -= bytes;
                    taken = Some(range);
                    break;
                }
                over_lines = lines > self.lines_left;
            }
            match taken {
                Some(range) => {
                    covered.push(range);
                    if symbol == Some(range) {
                        self.symbols += 1;
                    } else {
                        self.windows += 1;
                        self.unparsed += usize::from(unparsed);
                    }
                },
                None if covers(&covered, hit) => {},
                None if over_lines => self.unexpanded += 1,
                None => self.over_limit += 1,
            }
        }
        self.render(block, &raw_lines, &mut rendered, &covered)
    }

    fn render(
        &mut self,
        block: &TargetBlock,
        raw_lines: &[&[u8]],
        rendered: &mut BTreeMap<usize, ContextLine>,
        covered: &[(usize, usize)],
    ) -> (Vec<Line>, Vec<usize>) {
        let mut shown: Vec<usize> = covered
            .iter()
            .flat_map(|&(start, end)| start..=end)
            .chain(block.lines.iter().map(|line| line.number))
            .collect();
        shown.sort_unstable();
        shown.dedup();
        let mut lines = Vec::with_capacity(shown.len());
        let mut lossy = Vec::new();
        for number in shown {
            if let Ok(hit) = block
                .lines
                .binary_search_by_key(&number, |line| line.number)
            {
                lines.push(block.lines[hit].clone());
                continue;
            }
            let shown = rendered
                .remove(&number)
                .unwrap_or_else(|| context_line(raw_lines, number));
            if shown.lossy {
                lossy.push(number);
            }
            self.long_lines_cut += shown.cut;
            lines.push(shown.line);
        }
        (lines, lossy)
    }
}

struct ContextLine {
    line: Line,
    lossy: bool,
    cut: usize,
}

fn context_line(raw_lines: &[&[u8]], number: usize) -> ContextLine {
    let (mut line, lossy) = sink_line(
        raw_lines[number - 1],
        u64::try_from(number).ok(),
        Marker::Context,
    );
    let cut = window::cut_long_lines(std::slice::from_mut(&mut line), &[None]);
    ContextLine { line, lossy, cut }
}

/// The lines `range` shows past those already covered, and the bytes they add as rendered: a hit
/// line is in the answer already. Each context line is rendered once, into `rendered`.
fn added(
    block: &TargetBlock,
    raw_lines: &[&[u8]],
    rendered: &mut BTreeMap<usize, ContextLine>,
    covered: &[(usize, usize)],
    (start, end): (usize, usize),
) -> (usize, usize) {
    let mut lines = 0;
    let mut bytes = 0;
    for number in (start..=end).filter(|number| !covers(covered, *number)) {
        lines += 1;
        if block
            .lines
            .binary_search_by_key(&number, |line| line.number)
            .is_err()
        {
            let shown = rendered
                .entry(number)
                .or_insert_with(|| context_line(raw_lines, number));
            bytes += shown.line.text.len() + 1;
        }
    }
    (lines, bytes)
}

fn covers(covered: &[(usize, usize)], line: usize) -> bool {
    covered
        .iter()
        .any(|&(start, end)| (start..=end).contains(&line))
}

/// Line starts in the lossy text the grammar parses, numbered as the searcher numbers lines.
struct Text<'a> {
    content: &'a str,
    starts: Vec<usize>,
    total: usize,
}

impl<'a> Text<'a> {
    fn new(content: &'a str) -> Text<'a> {
        let starts: Vec<usize> = std::iter::once(0)
            .chain(content.match_indices('\n').map(|(at, _)| at + 1))
            .collect();
        let total = if content.is_empty() || content.ends_with('\n') {
            starts.len() - 1
        } else {
            starts.len()
        };
        Text {
            content,
            starts,
            total,
        }
    }

    fn line(&self, number: usize) -> &'a str {
        let start = self.starts[number - 1];
        let end = self
            .starts
            .get(number)
            .map_or(self.content.len(), |next| next - 1);
        let line = &self.content[start..end];
        line.strip_suffix('\r').unwrap_or(line)
    }
}

/// The syntax tree only proposes names; the definitions `show path#symbol` resolves decide which
/// of them is a symbol and where it ends. One parse serves both.
struct Enclosing<'a, 'q> {
    lang: Option<Language>,
    content: &'a str,
    tree: Option<tree_sitter::Tree>,
    query: Option<&'q Query>,
    /// Per top-level item, keyed by its byte span: running the query over the whole file cost
    /// half a parse on 2,000 lines of TypeScript.
    items: BTreeMap<(usize, usize), Vec<symbols::Defined<'a>>>,
    /// Markdown headings, which have no query: keyed by name, built on the first lookup.
    sections: Option<BTreeMap<&'a str, Vec<(usize, usize)>>>,
}

impl<'a, 'q> Enclosing<'a, 'q> {
    fn new(
        lang: Option<Language>,
        query: Option<&'q Query>,
        content: &'a str,
    ) -> Enclosing<'a, 'q> {
        let tree = lang.zip(query).and_then(|(lang, _)| {
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(grammars::language(lang)).ok()?;
            parser.parse(content, None)
        });
        Enclosing {
            lang,
            content,
            tree,
            query,
            items: BTreeMap::new(),
            sections: None,
        }
    }

    /// The smallest symbol holding line `hit`, 1-based and inclusive.
    fn symbol(&mut self, hit: usize, text: &Text<'_>, spans: &Regex) -> Option<(usize, usize)> {
        match self.lang? {
            Language::Markdown => self.section(hit, text),
            _ => self.definition(hit, text, spans),
        }
    }

    /// A symbol spanning more rows than the limit only holds larger ones, so the walk stops there.
    /// A definition ending past the top-level item holding the hit is not a candidate, as it was
    /// not when each lookup parsed only up to that item's end. One holding the hit's line lies in a
    /// top-level item that overlaps the bytes from the line's start to that end.
    fn definition(&mut self, hit: usize, text: &Text<'_>, spans: &Regex) -> Option<(usize, usize)> {
        let line = text.line(hit);
        let (start, end) = spans
            .find_iter(line)
            .find(|found| !found.is_empty())
            .map_or(
                (line.len() - line.trim_start().len(), line.len()),
                |found| (found.start(), found.end()),
            );
        let at = text.starts[hit - 1];
        let query = self.query?;
        let root = self.tree.as_ref()?.root_node();
        let target = root.named_descendant_for_byte_range(at + start, at + end)?;
        // Root first. `Node::parent` descends from the root on every call, so climbing with it is
        // quadratic in depth: 25 s for one hit in 40 KB of nested JSON arrays.
        let mut ancestors = vec![root];
        while let Some(&last) = ancestors.last()
            && last != target
        {
            let next = last
                .child_with_descendant(target)
                .filter(|next| *next != last)?;
            ancestors.push(next);
        }
        let item = ancestors.get(1).copied().unwrap_or(root);
        let items = self
            .content
            .get(..item.end_byte())
            .unwrap_or(self.content)
            .len();
        let mut holding: Option<((usize, usize), (usize, usize))> = None;
        for &node in ancestors.iter().rev() {
            let rows = node.end_position().row - node.start_position().row + 1;
            if rows > EXPAND_SYMBOL_MAX_LINES + 1 {
                return None;
            }
            if definition_like(node.kind())
                && let Some(name) = declared_name(node, self.content)
            {
                let (first, last) = *holding.get_or_insert_with(|| {
                    let mut walk = root.walk();
                    let mut span = ((usize::MAX, 0), (0, 0));
                    for child in root
                        .children(&mut walk)
                        .filter(|child| child.start_byte() <= items && child.end_byte() >= at)
                    {
                        let key = (child.start_byte(), child.end_byte());
                        span = (span.0.min(key), key);
                        self.items
                            .entry(key)
                            .or_insert_with(|| symbols::definitions(query, child, self.content));
                    }
                    span
                });
                if let Some(found) = smallest(
                    self.items
                        .range(first..=last.max(first))
                        .flat_map(|(_, defined)| defined)
                        .filter(|found| found.name == name && found.end <= items)
                        .map(|found| (found.line, found.end_line)),
                    hit,
                ) {
                    return Some(found);
                }
            }
        }
        None
    }

    /// The nearest heading above the hit that `symbols` also reads as one; a `#` in a fence is not.
    fn section(&mut self, hit: usize, text: &Text<'_>) -> Option<(usize, usize)> {
        let first = hit.saturating_sub(EXPAND_SYMBOL_MAX_LINES - 1).max(1);
        (first..=hit).rev().find_map(|number| {
            let line = text.line(number);
            let level = line.bytes().take_while(|byte| *byte == b'#').count();
            if !(1..=6).contains(&level) || !line[level..].starts_with(char::is_whitespace) {
                return None;
            }
            let content = self.content;
            let sections = self.sections.get_or_insert_with(|| {
                let mut sections: BTreeMap<&str, Vec<(usize, usize)>> = BTreeMap::new();
                for (name, first, last) in symbols::markdown_sections(content) {
                    sections.entry(name).or_default().push((first, last));
                }
                sections
            });
            smallest(sections.get(line[level..].trim())?.iter().copied(), hit)
        })
    }
}

fn smallest(spans: impl Iterator<Item = (usize, usize)>, hit: usize) -> Option<(usize, usize)> {
    spans
        .filter(|&(start, end)| (start..=end).contains(&hit))
        .min_by_key(|&(start, end)| (end - start, start))
}

/// Every symbol query's definition node is named one of these ways.
fn definition_like(kind: &str) -> bool {
    [
        "declaration",
        "definition",
        "item",
        "spec",
        "pair",
        "table",
        "method",
        "class",
        "module",
    ]
    .iter()
    .any(|part| kind.contains(part))
}

/// The name a symbol query captures: a `name` or `key` field, the innermost C `declarator`, or a
/// TOML key. A qualified name's own `name` field is its last segment.
fn declared_name<'a>(node: Node<'_>, content: &'a str) -> Option<&'a str> {
    let mut named = node
        .child_by_field_name("name")
        .or_else(|| node.child_by_field_name("key"))
        .or_else(|| {
            let mut declarator = node.child_by_field_name("declarator")?;
            while let Some(inner) = declarator.child_by_field_name("declarator") {
                declarator = inner;
            }
            Some(declarator)
        })
        .or_else(|| {
            node.named_children(&mut node.walk())
                .find(|child| child.kind().ends_with("_key"))
        })?;
    while let Some(inner) = named.child_by_field_name("name") {
        named = inner;
    }
    let text = named.utf8_text(content.as_bytes()).ok()?;
    Some(if named.kind() == "string" {
        text.trim_matches('"')
    } else {
        text
    })
}

fn respond_files(
    files: Vec<PathBuf>,
    searched: usize,
    pattern: &str,
    walked: Vec<Omission>,
) -> Outcome {
    let matched = files.len();
    let mut response = Response::empty("find");
    response.omitted = walked;
    let bytes = files
        .iter()
        .map(|path| path.display().to_string().len() + 1)
        .sum();
    response.stats = Stats::new(matched, bytes);
    response.footer = Footer {
        summary: format!(
            "{matched} file{} \u{b7} searched {searched} file{}",
            plural(matched),
            plural(searched)
        ),
    };
    response.body = Body::Files(files);

    if matched == 0 {
        return Outcome::partial(response, no_match_error(pattern));
    }
    Outcome::ok(response)
}

fn respond_counts(
    counts: Vec<CountRow>,
    total_hits: usize,
    pattern: &str,
    walked: Vec<Omission>,
) -> Outcome {
    let matched = counts.len();
    let mut response = Response::empty("find");
    response.omitted = walked;
    let bytes = counts
        .iter()
        .map(|row| format!("{}  {}\n", row.count, row.path.display()).len())
        .sum();
    response.stats = Stats::new(matched, bytes);
    response.footer = Footer {
        summary: format!(
            "{total_hits} hit{h} in {matched} file{f}",
            h = plural(total_hits),
            f = plural(matched),
        ),
    };
    response.body = Body::Counts(counts);

    if matched == 0 {
        return Outcome::partial(response, no_match_error(pattern));
    }
    Outcome::ok(response)
}

fn content_size(blocks: &[TargetBlock]) -> (usize, usize) {
    let mut lines = 0;
    let mut bytes = 0;
    for block in blocks {
        lines += block.lines.len();
        bytes += window::content_bytes(&block.lines);
    }
    (lines, bytes)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// `gitignored` and `dotfiles` count files a filter rejected directly: a file under a rejected
/// directory is covered by that directory's name, since the walk never enters it.
#[derive(Default)]
struct Walk {
    searched: usize,
    binary: usize,
    too_large: usize,
    unreadable: usize,
    unreadable_dirs: Vec<PathBuf>,
    lossy: BTreeMap<PathBuf, Vec<usize>>,
    gitignored: usize,
    dotfiles: usize,
    gitignored_dirs: Vec<PathBuf>,
    hidden_dirs: Vec<PathBuf>,
}

impl Walk {
    /// Rows are the only order-dependent state, so callers hand files over in render order.
    fn record(&mut self, found: &mut Found, args: &FindArgs, path: &Path, outcome: FileOutcome) {
        let (lossy_lines, hits, lines, first_match) = match outcome {
            FileOutcome::Skipped(skip) => {
                match skip {
                    Skip::Binary => self.binary += 1,
                    Skip::TooLarge => self.too_large += 1,
                    Skip::Unreadable => self.unreadable += 1,
                }
                return;
            },
            FileOutcome::Searched {
                lossy_lines,
                hits,
                lines,
                first_match,
            } => (lossy_lines, hits, lines, first_match),
        };
        self.searched += 1;
        if hits == 0 {
            return;
        }
        let display = display_path(path);
        found.total_hits += hits;
        if !lossy_lines.is_empty() {
            self.lossy.insert(display.clone(), lossy_lines);
        }
        if args.files {
            found.files.push(display);
        } else if args.count {
            found.counts.push(CountRow {
                count: hits,
                path: display,
            });
        } else {
            // Kept so an over-cap response can map the busiest files without a second walk.
            found.file_hits.push(CountRow {
                count: hits,
                path: display.clone(),
            });
            found.blocks.push(hit_block(display, lines));
            found.first_matches.push(first_match);
        }
    }

    fn reject(&mut self, rejection: Rejection) {
        match rejection {
            Rejection::GitignoredDir(dir) => self.gitignored_dirs.push(dir),
            Rejection::HiddenDir(dir) => self.hidden_dirs.push(dir),
            Rejection::Gitignored => self.gitignored += 1,
            Rejection::Dotfile => self.dotfiles += 1,
            Rejection::NotAFile => {},
        }
    }

    /// A file the walk reached but could not search is named in `Skipped`, so `other` stays zero.
    fn ignored(&self) -> Option<Omission> {
        if self.gitignored + self.dotfiles == 0
            && self.gitignored_dirs.is_empty()
            && self.hidden_dirs.is_empty()
        {
            return None;
        }
        Some(Omission::Ignored {
            gitignore: self.gitignored,
            hidden: self.dotfiles,
            other: 0,
            dirs: name_dirs(&self.gitignored_dirs, &self.hidden_dirs),
        })
    }

    fn skipped(&self) -> Option<Omission> {
        (self.binary + self.too_large + self.unreadable > 0).then_some(Omission::Skipped {
            binary: self.binary,
            too_large: self.too_large,
            unreadable: self.unreadable,
        })
    }
}

#[derive(Default)]
struct Found {
    total_hits: usize,
    blocks: Vec<TargetBlock>,
    /// Per block, each line's first wrapped match: long lines are cut once the response knows
    /// which lines it prints.
    first_matches: Vec<Vec<Option<Range<usize>>>>,
    files: Vec<PathBuf>,
    counts: Vec<CountRow>,
    /// Default mode only: the over-cap map, built when `blocks` goes unused.
    file_hits: Vec<CountRow>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Traversal {
    Sequential,
    /// `threads` of 0 lets `ignore` pick.
    Parallel {
        threads: usize,
    },
}

/// The most entries a tree may hold and still be walked on one thread. Measured on the musl build
/// over flat trees of three-line files, p50: one thread 4.7 ms against the pool's 6.6 ms at 400
/// files, 15.9 ms against 9.4 ms at 2,000. Starting the pool costs about 2.5 ms.
const SEQUENTIAL_WALK_MAX_ENTRIES: usize = 512;

/// The most directories the size probe lists. A listing costs about 13 µs on the musl build, and
/// probing this repository's root to the entry budget listed 90 of them, 1.3 ms.
const SEQUENTIAL_WALK_MAX_DIRS: usize = 8;

/// A file root is one read, so any file root keeps the sequential walk, which needs no sort. So
/// does a small tree; the probe stops listing once the tree is known to be large.
fn traversal_of(roots: &[PathBuf]) -> Traversal {
    if !roots.iter().all(|root| root.is_dir()) {
        return Traversal::Sequential;
    }
    let mut budget = SEQUENTIAL_WALK_MAX_ENTRIES;
    let mut unlisted: std::collections::VecDeque<PathBuf> = roots.iter().cloned().collect();
    let mut listed = 0;
    while let Some(dir) = unlisted.pop_front() {
        if listed == SEQUENTIAL_WALK_MAX_DIRS {
            return Traversal::Parallel { threads: 0 };
        }
        listed += 1;
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_name() == ".git" {
                continue;
            }
            let Some(left) = budget.checked_sub(1) else {
                return Traversal::Parallel { threads: 0 };
            };
            budget = left;
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                unlisted.push_back(entry.path());
            }
        }
    }
    Traversal::Sequential
}

enum FileOutcome {
    Searched {
        lossy_lines: Vec<usize>,
        hits: usize,
        lines: Vec<Line>,
        first_match: Vec<Option<Range<usize>>>,
    },
    Skipped(Skip),
}

enum Skip {
    Binary,
    TooLarge,
    Unreadable,
}

struct Search<'a> {
    args: &'a FindArgs,
    matcher: &'a RegexMatcher,
    spans: &'a Regex,
    max_file_bytes: u64,
}

impl Search<'_> {
    fn sequential(
        &self,
        mut builder: WalkBuilder,
        filters: Option<Filters>,
        walk: &mut Walk,
        found: &mut Found,
    ) {
        let rejected = judge_as_predicate(&mut builder, filters);
        let mut engine = build_searcher(self.args);
        // Readdir order differs between two machines holding the same tree.
        for entry in builder.sort_by_file_path(Path::cmp).build() {
            match entry {
                Err(err) => walk.unreadable_dirs.push(display_path(&error_path(&err))),
                Ok(entry) if is_candidate(&entry) => {
                    let outcome = self.file(entry.path(), &mut engine);
                    walk.record(found, self.args, entry.path(), outcome);
                },
                Ok(_) => {},
            }
        }
        let rejected =
            std::mem::take(&mut *rejected.lock().unwrap_or_else(PoisonError::into_inner));
        for rejection in rejected {
            walk.reject(rejection);
        }
    }

    /// Workers finish in scheduler order, so every file is collected and sorted into the sequential
    /// walk's order before any is recorded. Each worker judges entries with its own copy of the
    /// filters: one shared behind a lock serialised the workers, 19 ms against 8.5 ms for `rg` at
    /// this repository's root. A rejected directory is listed but never entered.
    fn parallel(
        &self,
        mut builder: WalkBuilder,
        filters: Option<&Filters>,
        threads: usize,
        roots: &[PathBuf],
        walk: &mut Walk,
        found: &mut Found,
    ) {
        let (tx, rx) = mpsc::channel();
        builder.threads(threads).build_parallel().run(|| {
            let tx = tx.clone();
            let mut engine = build_searcher(self.args);
            let mut filters = filters.cloned();
            Box::new(move |entry| {
                let visit = match entry {
                    Err(err) => Visit::Unlistable(error_path(&err)),
                    Ok(entry) => {
                        let rejection = filters.as_mut().and_then(|filters| filters.judge(&entry));
                        match rejection {
                            Some(rejection) => {
                                let next = if matches!(
                                    rejection,
                                    Rejection::GitignoredDir(_) | Rejection::HiddenDir(_)
                                ) {
                                    WalkState::Skip
                                } else {
                                    WalkState::Continue
                                };
                                tx.send(Visit::Rejected(rejection))
                                    .expect("the receiver is dropped only after the walk returns");
                                return next;
                            },
                            None if is_candidate(&entry) => {
                                let outcome = self.file(entry.path(), &mut engine);
                                Visit::File(entry.into_path(), outcome)
                            },
                            None => return WalkState::Continue,
                        }
                    },
                };
                tx.send(visit)
                    .expect("the receiver is dropped only after the walk returns");
                WalkState::Continue
            })
        });
        drop(tx);

        let mut files = Vec::new();
        let mut dirs = Vec::new();
        for visit in rx {
            match visit {
                Visit::File(path, outcome) => files.push((root_index(&path, roots), path, outcome)),
                Visit::Unlistable(dir) => dirs.push((root_index(&dir, roots), dir)),
                Visit::Rejected(rejection) => walk.reject(rejection),
            }
        }
        files.sort_unstable_by(|(root_a, a, _), (root_b, b, _)| {
            root_a.cmp(root_b).then_with(|| path_order(a, b))
        });
        dirs.sort_unstable();
        walk.unreadable_dirs
            .extend(dirs.iter().map(|(_, dir)| display_path(dir)));
        for (_, path, outcome) in files {
            walk.record(found, self.args, &path, outcome);
        }
    }

    /// Streamed, not read whole: a file with no hit costs no allocation of its size.
    fn file(&self, path: &Path, engine: &mut Searcher) -> FileOutcome {
        let Ok(file) = std::fs::File::open(path) else {
            return FileOutcome::Skipped(Skip::Unreadable);
        };
        let Ok(metadata) = file.metadata() else {
            return FileOutcome::Skipped(Skip::Unreadable);
        };
        // Re-checked on the handle, as `fs::read_regular` does, so a FIFO swapped in cannot block.
        if !metadata.file_type().is_file() {
            return FileOutcome::Skipped(Skip::Binary);
        }
        if metadata.len() > self.max_file_bytes {
            return FileOutcome::Skipped(Skip::TooLarge);
        }
        let mut source = Utf16Guard::new(file);
        let mut sink = HitSink::new(self.spans);
        if engine
            .search_reader(self.matcher, &mut source, &mut sink)
            .is_err()
        {
            return FileOutcome::Skipped(Skip::Unreadable);
        }
        if sink.binary || source.utf16 {
            return FileOutcome::Skipped(Skip::Binary);
        }
        FileOutcome::Searched {
            lossy_lines: sink.lossy_lines,
            hits: sink.hits,
            lines: sink.lines,
            first_match: sink.first_match,
        }
    }
}

/// `Path::cmp`'s order without splitting components: `/` sorts below every other byte.
fn path_order(a: &Path, b: &Path) -> std::cmp::Ordering {
    let key = |byte: &u8| if *byte == b'/' { 0 } else { *byte };
    let (a, b) = (
        a.as_os_str().as_encoded_bytes(),
        b.as_os_str().as_encoded_bytes(),
    );
    let common = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    a[common..].iter().map(key).cmp(b[common..].iter().map(key))
}

/// Longest match wins: one typed root can prefix another's spelling (`src` and `src/../lib`).
fn root_index(path: &Path, roots: &[PathBuf]) -> usize {
    roots
        .iter()
        .enumerate()
        .filter(|(_, root)| path.starts_with(root))
        .max_by_key(|(_, root)| root.components().count())
        .map_or(0, |(index, _)| index)
}

enum Visit {
    File(PathBuf, FileOutcome),
    Unlistable(PathBuf),
    Rejected(Rejection),
}

fn is_candidate(entry: &DirEntry) -> bool {
    entry.file_type().is_some_and(|kind| kind.is_file()) && !in_git_dir(entry.path())
}

/// Ends the read at a UTF-16 BOM: the searcher sees a file's lines, never its first bytes.
struct Utf16Guard {
    file: std::fs::File,
    head: [u8; 2],
    seen: usize,
    utf16: bool,
}

impl Utf16Guard {
    fn new(file: std::fs::File) -> Utf16Guard {
        Utf16Guard {
            file,
            head: [0; 2],
            seen: 0,
            utf16: false,
        }
    }
}

impl Read for Utf16Guard {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.utf16 {
            return Ok(0);
        }
        let read = self.file.read(buf)?;
        if self.seen < self.head.len() {
            let take = (self.head.len() - self.seen).min(read);
            self.head[self.seen..self.seen + take].copy_from_slice(&buf[..take]);
            self.seen += take;
            if matches!(self.head, [0xfe, 0xff] | [0xff, 0xfe]) && self.seen == self.head.len() {
                self.utf16 = true;
                return Ok(0);
            }
        }
        Ok(read)
    }
}

fn walk_omissions(args: &FindArgs, walk: &Walk) -> Vec<Omission> {
    let mut walked = Vec::new();
    if !args.globs.is_empty() {
        walked.push(Omission::Glob {
            patterns: args.globs.clone(),
        });
    }
    walked.extend(walk.ignored());
    walked.extend(walk.skipped());
    walked.extend(
        walk.unreadable_dirs
            .iter()
            .map(|path| Omission::Unreadable { path: path.clone() }),
    );
    walked
}

/// `ignore::Error` has no path accessor; an unlistable directory is `WithPath` under `WithDepth`.
fn error_path(error: &ignore::Error) -> PathBuf {
    match error {
        ignore::Error::WithPath { path, .. } => path.clone(),
        ignore::Error::WithDepth { err, .. } => error_path(err),
        other => PathBuf::from(other.to_string()),
    }
}

/// Bounds the footer as `TOP_FILES_SHOWN` bounds the file map: a monorepo can reject a
/// `node_modules/` per package.
const IGNORED_DIRS_NAMED: usize = 5;

/// Shallowest first, so a nested rejection never hides a top-level `target/`, with the path
/// breaking ties, since a parallel walk rejects directories in scheduler order. Each source that
/// pruned anything keeps its shallowest name, so the footer names a directory per flag.
fn name_dirs(gitignored: &[PathBuf], hidden: &[PathBuf]) -> IgnoredDirs {
    let order = |a: &&PathBuf, b: &&PathBuf| {
        let depth = |dir: &PathBuf| dir.components().count();
        depth(a).cmp(&depth(b)).then_with(|| a.cmp(b))
    };
    let mut sources: [Vec<&PathBuf>; 2] = [gitignored.iter().collect(), hidden.iter().collect()];
    for dirs in &mut sources {
        dirs.sort_unstable_by(order);
    }
    let mut taken = sources.each_ref().map(|dirs| usize::from(!dirs.is_empty()));
    while taken.iter().sum::<usize>() < IGNORED_DIRS_NAMED {
        let next = [0, 1]
            .into_iter()
            .filter(|&source| taken[source] < sources[source].len())
            .min_by(|&a, &b| order(&sources[a][taken[a]], &sources[b][taken[b]]));
        let Some(source) = next else { break };
        taken[source] += 1;
    }
    let named = |source: usize| NamedDirs {
        named: sources[source][..taken[source]]
            .iter()
            .map(|dir| dir.display().to_string())
            .collect(),
        more: sources[source].len() - taken[source],
    };
    IgnoredDirs {
        gitignore: named(0),
        hidden: named(1),
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;
    use crate::output::{Format, Marker, RenderOptions};
    use crate::own_process::{Workdir, repo, workdir};

    fn many_hits(n: usize) -> String {
        let mut lines = String::new();
        for i in 1..=n {
            writeln!(lines, "needle {i}").expect("String writes never fail");
        }
        lines
    }

    fn find_args(pattern: &str, paths: &[&str]) -> FindArgs {
        FindArgs {
            pattern: pattern.to_owned(),
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            fixed_string: false,
            ignore_case: false,
            word: false,
            cap: 50,
            files: false,
            count: false,
            globs: Vec::new(),
            hidden: false,
            after: None,
            before: None,
            context: None,
            no_expand: false,
            grep: crate::cli::GrepCompat::default(),
        }
    }

    fn global_args() -> Global {
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

    fn write(dir: &Workdir, name: &str, content: &str) {
        std::fs::write(dir.path().join(name), content).expect("test fixture writes");
    }

    fn run_find(args: &FindArgs, global: &Global) -> Outcome {
        run(args, global, Format::Text)
    }

    /// The committed tree is flat, so one pass over its entries copies it whole.
    fn read_fixture() -> Option<Workdir> {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/read");
        let dir = repo()?;
        for entry in std::fs::read_dir(&source).expect("the committed read fixture tree") {
            let entry = entry.expect("a fixture entry");
            assert!(
                entry.file_type().expect("a fixture entry type").is_file(),
                "the read fixture tree is flat: {}",
                entry.path().display()
            );
            std::fs::copy(entry.path(), dir.path().join(entry.file_name()))
                .expect("copying a fixture file");
        }
        Some(dir)
    }

    fn ignored_bucket(outcome: &Outcome) -> (usize, usize, usize) {
        match outcome.response.omitted.as_slice() {
            [
                Omission::Ignored {
                    gitignore,
                    hidden,
                    other,
                    ..
                },
            ] => (*gitignore, *hidden, *other),
            other => panic!("expected exactly one Omission::Ignored, got {other:?}"),
        }
    }

    fn named(outcome: &Outcome) -> Vec<String> {
        outcome
            .response
            .omitted
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn line_numbers(block: &TargetBlock) -> Vec<usize> {
        block.lines.iter().map(|line| line.number).collect()
    }

    #[test]
    fn a_few_hits_render_one_block_per_file_with_hit_and_context_markers() {
        let Some(dir) = repo() else { return };
        write(&dir, "a.txt", "one\nneedle here\nthree\n");
        write(&dir, "b.txt", "nothing to see\n");

        let mut args = find_args("needle", &[]);
        args.context = Some(1);
        let outcome = run_find(&args, &global_args());

        assert!(outcome.error.is_none(), "under the cap is a clean exit");
        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        assert_eq!(blocks.len(), 1, "only a.txt has a hit");
        let block = &blocks[0];
        assert_eq!(block.target, "a.txt");
        assert!(block.span.is_none(), "a find hit block names no range");
        assert!(block.sha.is_none());
        assert_eq!(block.lines.len(), 3);
        assert_eq!(block.lines[0].marker, Marker::Context);
        assert_eq!(block.lines[1].marker, Marker::Hit);
        assert_eq!(block.lines[1].number, 2);
        assert_eq!(block.lines[1].text, "\u{ab}needle\u{bb} here");
        assert_eq!(block.lines[2].marker, Marker::Context);
        assert_eq!(
            outcome.response.footer.summary,
            "1 hit in 1 file · searched 2 files"
        );
    }

    #[test]
    fn hits_exceeding_the_cap_print_only_the_busiest_file_s_first_ten_and_name_the_true_counts() {
        let Some(dir) = repo() else { return };
        let lines = many_hits(55);
        write(&dir, "many.txt", &lines);

        let mut args = find_args("needle", &[]);
        args.cap = 50;
        let outcome = run_find(&args, &global_args());

        let err = outcome
            .error
            .as_ref()
            .expect("over cap is a terminal error");
        assert_eq!(err.slug(), "over_cap");
        assert!(matches!(err, Error::OverCap {
            hits: 55,
            files: 1,
            cap: 50
        }));
        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        let [preview] = blocks.as_slice() else {
            panic!("the busiest file alone is previewed: {blocks:?}");
        };
        assert_eq!(preview.target, "many.txt");
        assert_eq!(line_numbers(preview), (1..=10).collect::<Vec<_>>());
        assert!(preview.lines.iter().all(|line| line.marker == Marker::Hit));
        assert_eq!(
            outcome.response.footer.summary,
            "55 hits in 1 file · searched 1 file"
        );
        assert!(
            named(&outcome).contains(&"first 10 of 55 hits in the busiest file shown".to_owned()),
            "{:?}",
            named(&outcome)
        );
        assert!(
            outcome
                .response
                .omitted
                .iter()
                .any(|o| matches!(o, Omission::HitCap { hits: 55, cap: 50 })),
            "{:?}",
            outcome.response.omitted
        );
    }

    #[test]
    fn over_cap_lists_the_top_ten_files_by_hit_count_then_names_the_rest() {
        let Some(dir) = repo() else { return };
        for (name, hits) in [
            ("j.txt", 10),
            ("i.txt", 9),
            ("h.txt", 8),
            ("g.txt", 7),
            ("f.txt", 6),
            ("e.txt", 5),
            ("d.txt", 4),
            ("c.txt", 3),
            ("b.txt", 2),
            ("a.txt", 1),
            ("k.txt", 1),
            ("l.txt", 1),
            ("m.txt", 1),
        ] {
            write(&dir, name, &many_hits(hits));
        }

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        let err = outcome
            .error
            .as_ref()
            .expect("over cap is a terminal error");
        assert_eq!(err.slug(), "over_cap");
        assert!(matches!(err, Error::OverCap {
            hits: 58,
            files: 13,
            cap: 50
        }));
        assert_eq!(
            outcome.response.top_files.len(),
            10,
            "{:?}",
            outcome.response.top_files
        );
        let rows: Vec<(usize, &str)> = outcome
            .response
            .top_files
            .iter()
            .map(|row| (row.count, row.path.to_str().expect("ascii test paths")))
            .collect();
        assert_eq!(rows, vec![
            (10, "j.txt"),
            (9, "i.txt"),
            (8, "h.txt"),
            (7, "g.txt"),
            (6, "f.txt"),
            (5, "e.txt"),
            (4, "d.txt"),
            (3, "c.txt"),
            (2, "b.txt"),
            (1, "a.txt"),
        ]);
        assert_eq!(
            outcome.response.top_files_more, 3,
            "k.txt, l.txt and m.txt tie at the cut and are not shown"
        );

        let rendered = crate::output::render(&outcome.response, Format::Text, &RenderOptions {
            numbers: true,
            quiet: false,
            cost_first: false,
        });
        let preview = (1..=10).fold(String::new(), |mut preview, n| {
            writeln!(preview, "{n:>2}:\t\u{ab}needle\u{bb} {n}").expect("a String write");
            preview
        });
        // Nine preview lines of 13 bytes and one of 14 are 131 bytes: ~32 tokens at ÷ 4.
        assert_eq!(
            rendered,
            "\u{2500}\u{2500} j.txt\n".to_owned()
                + &preview
                + "10\tj.txt\n\
             \u{20}9\ti.txt\n\
             \u{20}8\th.txt\n\
             \u{20}7\tg.txt\n\
             \u{20}6\tf.txt\n\
             \u{20}5\te.txt\n\
             \u{20}4\td.txt\n\
             \u{20}3\tc.txt\n\
             \u{20}2\tb.txt\n\
             \u{20}1\ta.txt\n\
             \u{2026} 3 more files\n\
             \u{2500}\u{2500} 58 hits in 13 files \u{b7} searched 13 files \u{b7} over the \
             50-hit cap \u{b7} narrow the pattern or the paths, or --files \u{b7} first 10 of \
             10 hits in the busiest file shown \u{b7} top 10 files shown \u{b7} ~32 tokens\n"
        );
    }

    #[test]
    fn over_cap_with_ten_or_fewer_files_lists_them_all_with_no_more_line() {
        let Some(dir) = repo() else { return };
        write(&dir, "z.txt", &many_hits(3));
        write(&dir, "a.txt", &many_hits(2));
        write(&dir, "m.txt", &many_hits(1));

        let mut args = find_args("needle", &[]);
        args.cap = 2;
        let outcome = run_find(&args, &global_args());

        assert!(outcome.error.is_some(), "6 hits over a cap of 2");
        assert_eq!(outcome.response.top_files.len(), 3);
        assert_eq!(outcome.response.top_files_more, 0);
        let rendered = crate::output::render(&outcome.response, Format::Text, &RenderOptions {
            numbers: true,
            quiet: false,
            cost_first: false,
        });
        assert!(
            !rendered.contains("more file"),
            "no more-files line when every matched file is shown: {rendered}"
        );
        // The preview holds no more than the cap of 2: two lines of 13 bytes, ~6 tokens at ÷ 4.
        assert_eq!(
            rendered,
            "\u{2500}\u{2500} z.txt\n\
             1:\t\u{ab}needle\u{bb} 1\n\
             2:\t\u{ab}needle\u{bb} 2\n\
             3\tz.txt\n\
             2\ta.txt\n\
             1\tm.txt\n\
             \u{2500}\u{2500} 6 hits in 3 files \u{b7} searched 3 files \u{b7} over the 2-hit \
             cap \u{b7} narrow the pattern or the paths, or --files \u{b7} first 2 of 3 hits in \
             the busiest file shown \u{b7} top 3 files shown \u{b7} ~6 tokens\n"
        );
    }

    #[test]
    fn under_the_cap_carries_no_top_files_map() {
        let Some(dir) = repo() else { return };
        write(&dir, "a.txt", "needle here\n");

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        assert!(outcome.error.is_none());
        assert!(outcome.response.top_files.is_empty());
        assert_eq!(outcome.response.top_files_more, 0);
        assert!(
            !outcome
                .response
                .omitted
                .iter()
                .any(|o| matches!(o, Omission::TopFiles { .. }))
        );
    }

    #[test]
    fn count_mode_over_cap_still_reports_every_file_with_no_top_files_map() {
        let Some(dir) = repo() else { return };
        write(&dir, "many.txt", &many_hits(55));

        let mut args = find_args("needle", &[]);
        args.cap = 50;
        args.count = true;
        let outcome = run_find(&args, &global_args());

        assert!(outcome.error.is_none(), "--count applies no cap");
        let Body::Counts(rows) = &outcome.response.body else {
            panic!("expected Body::Counts");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].count, 55);
        assert!(outcome.response.top_files.is_empty());
    }

    #[test]
    fn files_mode_lists_every_matching_path_with_no_cap_applied() {
        let Some(dir) = repo() else { return };
        let lines = many_hits(55);
        write(&dir, "many.txt", &lines);
        write(&dir, "one.txt", "needle once\n");

        let mut args = find_args("needle", &[]);
        args.cap = 50;
        args.files = true;
        let outcome = run_find(&args, &global_args());

        assert!(outcome.error.is_none(), "--files applies no cap");
        let Body::Files(paths) = &outcome.response.body else {
            panic!("expected Body::Files");
        };
        assert_eq!(paths, &[
            PathBuf::from("many.txt"),
            PathBuf::from("one.txt")
        ]);
        assert!(
            outcome.response.top_files.is_empty(),
            "--files applies no cap and builds no map"
        );
        assert!(
            !outcome
                .response
                .omitted
                .iter()
                .any(|o| matches!(o, Omission::HitCap { .. }))
        );
        assert_eq!(
            outcome.response.footer.summary,
            "2 files · searched 2 files"
        );
    }

    #[test]
    fn count_mode_reports_one_row_per_file_sorted_by_path() {
        let Some(dir) = repo() else { return };
        write(&dir, "b.txt", "needle\nneedle\n");
        write(&dir, "a.txt", "needle\n");

        let mut args = find_args("needle", &[]);
        args.count = true;
        let outcome = run_find(&args, &global_args());

        assert!(outcome.error.is_none());
        let Body::Counts(rows) = &outcome.response.body else {
            panic!("expected Body::Counts");
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].path, PathBuf::from("a.txt"));
        assert_eq!(rows[0].count, 1);
        assert_eq!(rows[1].path, PathBuf::from("b.txt"));
        assert_eq!(rows[1].count, 2);
        assert_eq!(outcome.response.footer.summary, "3 hits in 2 files");
    }

    #[test]
    fn zero_hits_is_a_terminal_not_found_with_an_empty_footer_count() {
        let Some(dir) = repo() else { return };
        write(&dir, "a.txt", "nothing relevant\n");

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        let err = outcome.error.as_ref().expect("no match is terminal");
        assert_eq!(err.slug(), "not_found");
        assert_eq!(err.to_string(), "no hits for \u{ab}needle\u{bb}");
        assert_eq!(
            outcome.response.footer.summary,
            "0 hits in 0 files · searched 1 file"
        );
    }

    // `nope2` sits under `.`, so the containment pass would otherwise fold it away.
    #[test]
    fn every_missing_search_root_is_named_and_the_others_are_still_searched() {
        let Some(dir) = repo() else { return };
        write(&dir, "a.txt", "needle\n");

        let outcome = run_find(
            &find_args("needle", &["nope1", ".", "nope2"]),
            &global_args(),
        );

        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        assert_eq!(blocks.len(), 1, "the root that exists is searched");
        let named: Vec<&str> = outcome
            .response
            .omitted
            .iter()
            .filter_map(|omission| match omission {
                Omission::Unresolved {
                    target,
                    error: "not_found",
                } => Some(target.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(named, ["nope1", "nope2"]);
        let err = outcome
            .error
            .as_ref()
            .expect("a missing root fails the call");
        assert_eq!(err.slug(), "not_found");
        let stderr = err.to_string();
        assert!(
            stderr.contains("nope1") && stderr.contains("nope2"),
            "{stderr}"
        );
    }

    #[test]
    fn with_every_search_root_present_nothing_is_named_as_failed() {
        let Some(dir) = repo() else { return };
        write(&dir, "a.txt", "needle\n");
        write(&dir, "nope1", "x\n");
        write(&dir, "nope2", "x\n");

        let outcome = run_find(
            &find_args("needle", &["nope1", ".", "nope2"]),
            &global_args(),
        );

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(
            !outcome
                .response
                .omitted
                .iter()
                .any(|omission| matches!(omission, Omission::Unresolved { .. }))
        );
    }

    #[test]
    fn a_search_path_outside_the_working_tree_is_refused_before_any_match() {
        let in_repo = workdir(|parent| {
            let repo_dir = parent.join("repo");
            std::fs::create_dir(&repo_dir).expect("repo dir");
            std::fs::create_dir(repo_dir.join(".git")).expect("fake .git dir");
            std::fs::write(parent.join("outside.txt"), "needle\n").expect("outside file");
            repo_dir
        });
        if in_repo.is_none() {
            return;
        }

        let outcome = run_find(&find_args("needle", &["../outside.txt"]), &global_args());

        assert!(!outcome.response.has_output(), "nothing is searched");
        assert!(matches!(outcome.error, Some(Error::OutsideTree { .. })));
    }

    #[test]
    fn gitignore_and_hidden_exclusions_are_named_and_the_flags_that_undo_them_zero_the_bucket() {
        let Some(dir) = repo() else { return };
        write(&dir, "visible.txt", "needle\n");
        write(&dir, "ignored.txt", "needle\n");
        write(&dir, ".hidden.txt", "needle\n");
        write(&dir, ".gitignore", "ignored.txt\n");

        let default_outcome = run_find(&find_args("needle", &[]), &global_args());
        match default_outcome.response.omitted.as_slice() {
            [
                Omission::Ignored {
                    gitignore,
                    hidden,
                    other,
                    ..
                },
            ] => {
                assert_eq!(*gitignore, 1, "ignored.txt only");
                assert_eq!(*hidden, 2, ".hidden.txt and .gitignore");
                assert_eq!(*other, 0);
            },
            other => panic!("expected exactly one Omission::Ignored, got {other:?}"),
        }

        let mut hidden_args = find_args("needle", &[]);
        hidden_args.hidden = true;
        let hidden_outcome = run_find(&hidden_args, &global_args());
        match hidden_outcome.response.omitted.as_slice() {
            [
                Omission::Ignored {
                    gitignore, hidden, ..
                },
            ] => {
                assert_eq!(*hidden, 0, "--hidden zeroes the hidden bucket");
                assert_eq!(*gitignore, 1);
            },
            other => panic!("expected exactly one Omission::Ignored, got {other:?}"),
        }

        let mut global = global_args();
        global.no_ignore = true;
        let no_ignore_outcome = run_find(&find_args("needle", &[]), &global);
        match no_ignore_outcome.response.omitted.as_slice() {
            [
                Omission::Ignored {
                    gitignore, hidden, ..
                },
            ] => {
                assert_eq!(*gitignore, 0, "--no-ignore zeroes the gitignore bucket");
                assert_eq!(*hidden, 2);
            },
            other => panic!("expected exactly one Omission::Ignored, got {other:?}"),
        }

        let mut both_args = find_args("needle", &[]);
        both_args.hidden = true;
        let mut both_global = global_args();
        both_global.no_ignore = true;
        let both_outcome = run_find(&both_args, &both_global);
        assert!(
            both_outcome.response.omitted.is_empty(),
            "no filter is active, so nothing is omitted: {:?}",
            both_outcome.response.omitted
        );
        assert_eq!(
            both_outcome.response.footer.summary,
            "3 hits in 3 files · searched 4 files"
        );
    }

    #[test]
    fn fixed_string_escapes_a_pattern_that_would_otherwise_be_a_regex_metacharacter() {
        let Some(dir) = repo() else { return };
        write(&dir, "a.txt", "a(b)c\n");
        write(&dir, "b.txt", "abc\n");

        let mut args = find_args("a(b)c", &[]);
        args.fixed_string = true;
        let outcome = run_find(&args, &global_args());

        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].target, "a.txt");
        assert_eq!(
            blocks[0].lines[0].text, "\u{ab}a(b)c\u{bb}",
            "the marked span is the escaped literal, not a group match"
        );
    }

    #[test]
    fn an_unparsable_regex_fails_before_any_walk_with_no_response_content() {
        let Some(_dir) = repo() else { return };

        let outcome = run_find(&find_args("(unclosed", &[]), &global_args());

        assert!(!outcome.response.has_output());
        let Some(error @ Error::InvalidPattern { pattern, message }) = &outcome.error else {
            panic!("expected Error::InvalidPattern, got {:?}", outcome.error);
        };
        assert_eq!(pattern, "(unclosed");
        assert_eq!(message, "unclosed group");
        assert_eq!(
            error.to_string(),
            "invalid pattern: unclosed group",
            "stderr carries one line before ERROR_CODE=, not a four-line diagram"
        );
    }

    #[test]
    fn a_parsable_group_is_not_an_invalid_pattern() {
        let Some(dir) = repo() else { return };
        write(&dir, "a.txt", "closed\n");

        let outcome = run_find(&find_args("(closed)", &[]), &global_args());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
    }

    #[test]
    fn word_boundary_matching_excludes_a_substring_hit() {
        let Some(dir) = repo() else { return };
        write(&dir, "a.txt", "needleworks\n");
        write(&dir, "b.txt", "a needle here\n");

        let mut args = find_args("needle", &[]);
        args.word = true;
        let outcome = run_find(&args, &global_args());

        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].target, "b.txt");
        assert_eq!(
            blocks[0].lines[0].text, "a \u{ab}needle\u{bb} here",
            "the marked span obeys the same word boundaries the search did"
        );
    }

    #[test]
    fn case_insensitive_matches_a_differently_cased_pattern() {
        let Some(dir) = repo() else { return };
        write(&dir, "a.txt", "NEEDLE\n");

        let mut args = find_args("needle", &[]);
        args.ignore_case = true;
        let outcome = run_find(&args, &global_args());

        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        assert_eq!(blocks.len(), 1);
    }

    #[test]
    fn count_mode_names_the_ignored_bucket_its_walk_left_out() {
        let Some(dir) = repo() else { return };
        write(&dir, "visible.txt", "needle\n");
        write(&dir, "ignored.txt", "needle\n");
        write(&dir, ".hidden.txt", "needle\n");
        write(&dir, ".gitignore", "ignored.txt\n");

        let mut args = find_args("needle", &[]);
        args.count = true;
        let outcome = run_find(&args, &global_args());

        let Body::Counts(rows) = &outcome.response.body else {
            panic!("expected Body::Counts");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, PathBuf::from("visible.txt"));
        assert_eq!(
            ignored_bucket(&outcome),
            (1, 2, 0),
            "ignored.txt; .hidden.txt and .gitignore"
        );
        assert_eq!(outcome.response.footer.summary, "1 hit in 1 file");
    }

    #[test]
    fn count_mode_names_no_bucket_when_no_filter_left_anything_out() {
        let Some(dir) = repo() else { return };
        write(&dir, "visible.txt", "needle\n");

        let mut args = find_args("needle", &[]);
        args.count = true;
        let outcome = run_find(&args, &global_args());

        assert!(
            outcome.response.omitted.is_empty(),
            "{:?}",
            outcome.response.omitted
        );
    }

    #[test]
    fn a_file_over_max_file_bytes_is_named_as_skipped_too_large() {
        let Some(dir) = repo() else { return };
        write(&dir, "small.txt", "needle\n");
        write(&dir, "big.txt", &many_hits(20));
        let mut global = global_args();
        global.max_file_bytes = 20;

        let outcome = run_find(&find_args("needle", &[]), &global);

        assert_eq!(
            named(&outcome),
            ["skipped 1 (too large 1)"],
            "big.txt is out of reach of --max-file-bytes, and the footer owes the caller that"
        );
        assert_eq!(
            outcome.response.footer.summary,
            "1 hit in 1 file · searched 1 file"
        );
    }

    #[test]
    fn the_same_files_under_the_default_cap_are_named_in_no_bucket() {
        let Some(dir) = repo() else { return };
        write(&dir, "small.txt", "needle\n");
        write(&dir, "big.txt", &many_hits(20));

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        assert!(
            outcome.response.omitted.is_empty(),
            "{:?}",
            outcome.response.omitted
        );
        assert_eq!(
            outcome.response.footer.summary,
            "21 hits in 2 files · searched 2 files"
        );
    }

    /// Returns whether the mode took effect: uid 0 and some mounts read a file whatever its mode.
    fn locked(dir: &Workdir, name: &str) -> bool {
        use std::os::unix::fs::PermissionsExt as _;

        let path = dir.path().join(name);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
            .expect("a fixture file this process owns");
        std::fs::read(&path).is_err()
    }

    #[test]
    fn a_file_this_process_cannot_open_is_named_as_skipped_unreadable() {
        let Some(dir) = repo() else { return };
        write(&dir, "readable.txt", "needle\n");
        write(&dir, "locked.txt", "needle\n");
        if !locked(&dir, "locked.txt") {
            eprintln!(
                "skipped (a_file_this_process_cannot_open_is_named_as_skipped_unreadable): mode \
                 0o000 did not refuse this reader"
            );
            return;
        }

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        let targets: Vec<&str> = blocks.iter().map(|block| block.target.as_str()).collect();
        assert_eq!(
            targets,
            ["readable.txt"],
            "the locked file has no content to show"
        );
        assert_eq!(
            named(&outcome),
            ["skipped 1 (unreadable 1)"],
            "locked.txt was walked and could not be opened, and a skip the footer does not name \
             did not happen"
        );
        assert_eq!(
            outcome.response.footer.summary,
            "1 hit in 1 file · searched 1 file"
        );
    }

    #[test]
    fn the_same_two_files_both_readable_are_named_in_no_bucket() {
        let Some(dir) = repo() else { return };
        write(&dir, "readable.txt", "needle\n");
        write(&dir, "locked.txt", "needle\n");

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        assert!(
            outcome.response.omitted.is_empty(),
            "{:?}",
            outcome.response.omitted
        );
        assert_eq!(
            outcome.response.footer.summary,
            "2 hits in 2 files · searched 2 files"
        );
    }

    #[test]
    fn a_root_another_root_already_contains_is_searched_once() {
        let Some(dir) = repo() else { return };
        std::fs::create_dir(dir.path().join("sub")).expect("a nested directory");
        write(&dir, "top.txt", "needle at the top\n");
        write(&dir, "sub/nested.txt", "needle below\n");

        let overlapping = run_find(&find_args("needle", &[".", "sub"]), &global_args());

        let Body::Targets(blocks) = &overlapping.response.body else {
            panic!("expected Body::Targets");
        };
        let targets: Vec<&str> = blocks.iter().map(|block| block.target.as_str()).collect();
        assert_eq!(targets, ["sub/nested.txt", "top.txt"]);
        assert!(blocks.iter().all(|block| block.lines.len() == 1));
        assert_eq!(
            overlapping.response.footer.summary,
            "2 hits in 2 files · searched 2 files"
        );
        assert!(
            overlapping.response.omitted.is_empty(),
            "{:?}",
            overlapping.response.omitted
        );

        let duplicated = run_find(&find_args("needle", &["sub", "sub"]), &global_args());

        let Body::Targets(blocks) = &duplicated.response.body else {
            panic!("expected Body::Targets");
        };
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].lines.len(), 1);
        assert_eq!(
            duplicated.response.footer.summary,
            "1 hit in 1 file · searched 1 file"
        );
    }

    #[test]
    fn files_mode_lists_a_file_reachable_from_two_roots_once() {
        let Some(dir) = repo() else { return };
        std::fs::create_dir(dir.path().join("sub")).expect("a nested directory");
        write(&dir, "sub/nested.txt", "needle below\n");

        let mut args = find_args("needle", &[".", "sub"]);
        args.files = true;
        let outcome = run_find(&args, &global_args());

        let Body::Files(paths) = &outcome.response.body else {
            panic!("expected Body::Files");
        };
        assert_eq!(paths, &[PathBuf::from("sub/nested.txt")]);
        assert_eq!(outcome.response.footer.summary, "1 file · searched 1 file");
    }

    fn lines_with_needles(total: usize, hits: &[usize]) -> String {
        let mut text = String::new();
        for n in 1..=total {
            let word = if hits.contains(&n) {
                "needle"
            } else {
                "filler"
            };
            writeln!(text, "{word} on line {n}").expect("String writes never fail");
        }
        text
    }

    #[test]
    fn context_windows_that_overlap_render_one_run_with_no_repeated_line() {
        let Some(dir) = repo() else { return };
        write(&dir, "a.txt", &lines_with_needles(10, &[3, 6]));

        let mut args = find_args("needle", &[]);
        args.context = Some(2);
        let outcome = run_find(&args, &global_args());

        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        assert_eq!(blocks.len(), 1);
        assert_eq!(line_numbers(&blocks[0]), vec![1, 2, 3, 4, 5, 6, 7, 8]);
        let hits: Vec<usize> = blocks[0]
            .lines
            .iter()
            .filter(|line| line.marker == Marker::Hit)
            .map(|line| line.number)
            .collect();
        assert_eq!(hits, vec![3, 6]);
        assert_eq!(
            outcome.response.footer.summary,
            "2 hits in 1 file · searched 1 file"
        );
    }

    #[test]
    fn context_windows_far_apart_stay_two_runs_inside_one_block() {
        let Some(dir) = repo() else { return };
        write(&dir, "a.txt", &lines_with_needles(30, &[2, 22]));

        let mut args = find_args("needle", &[]);
        args.context = Some(1);
        let outcome = run_find(&args, &global_args());

        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        assert_eq!(blocks.len(), 1, "one block per file, however many runs");
        assert_eq!(line_numbers(&blocks[0]), vec![1, 2, 3, 21, 22, 23]);
    }

    #[test]
    fn hits_exactly_at_the_cap_print_their_content_and_one_more_does_not() {
        let Some(dir) = repo() else { return };
        write(&dir, "many.txt", &many_hits(50));
        let mut args = find_args("needle", &[]);
        args.cap = 50;

        let at_cap = run_find(&args, &global_args());

        assert!(
            at_cap.error.is_none(),
            "the cap is inclusive: {:?}",
            at_cap.error
        );
        let Body::Targets(blocks) = &at_cap.response.body else {
            panic!("expected Body::Targets");
        };
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].lines.len(), 50);
        assert!(
            at_cap.response.omitted.is_empty(),
            "{:?}",
            at_cap.response.omitted
        );

        write(&dir, "many.txt", &many_hits(51));
        let over_cap = run_find(&args, &global_args());

        assert!(over_cap.error.is_some(), "one hit past the cap is terminal");
        let Body::Targets(blocks) = &over_cap.response.body else {
            panic!("expected Body::Targets");
        };
        assert_eq!(
            blocks
                .iter()
                .map(|block| block.lines.len())
                .collect::<Vec<_>>(),
            [BUSIEST_HITS_SHOWN],
            "past the cap, only the busiest file's first hits print"
        );
        assert!(
            over_cap
                .response
                .omitted
                .iter()
                .any(|o| matches!(o, Omission::HitCap { hits: 51, cap: 50 })),
            "{:?}",
            over_cap.response.omitted
        );
    }

    #[test]
    fn every_match_on_a_hit_line_is_wrapped_and_context_lines_are_left_alone() {
        let Some(dir) = repo() else { return };
        write(&dir, "a.txt", "quiet\nneedle and needle again\nquiet too\n");

        let mut args = find_args("needle", &[]);
        args.context = Some(1);
        let outcome = run_find(&args, &global_args());

        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        let texts: Vec<&str> = blocks[0].lines.iter().map(|line| &*line.text).collect();
        assert_eq!(texts, [
            "quiet",
            "\u{ab}needle\u{bb} and \u{ab}needle\u{bb} again",
            "quiet too"
        ]);
    }

    #[test]
    fn files_and_count_modes_render_no_wrapped_text_at_all() {
        let Some(dir) = repo() else { return };
        write(&dir, "a.txt", "needle here\n");

        for mode in [(true, false), (false, true)] {
            let mut args = find_args("needle", &[]);
            (args.files, args.count) = mode;
            let outcome = run_find(&args, &global_args());

            let rendered = crate::output::render(&outcome.response, Format::Text, &RenderOptions {
                numbers: true,
                quiet: false,
                cost_first: false,
            });
            assert!(
                !rendered.contains('\u{ab}') && !rendered.contains('\u{bb}'),
                "{rendered}"
            );
        }
    }

    #[test]
    fn a_git_directory_is_never_a_search_candidate_whatever_the_flags_say() {
        let Some(dir) = repo() else { return };
        write(&dir, "tracked.txt", "needle\n");
        std::fs::write(dir.path().join(".git/HEAD"), "needle\n").expect("a file inside .git");

        let mut args = find_args("needle", &[]);
        args.hidden = true;
        let mut global = global_args();
        global.no_ignore = true;
        let outcome = run_find(&args, &global);

        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        let targets: Vec<&str> = blocks.iter().map(|block| block.target.as_str()).collect();
        assert_eq!(targets, ["tracked.txt"]);
        assert_eq!(
            outcome.response.footer.summary,
            "1 hit in 1 file · searched 1 file"
        );
        assert!(
            outcome.response.omitted.is_empty(),
            "a file that is never a candidate belongs to no bucket: {:?}",
            outcome.response.omitted
        );
    }

    #[test]
    fn a_gitignore_that_re_includes_the_dotfiles_leaves_no_bucket_at_all() {
        let Some(dir) = repo() else { return };
        write(&dir, ".env", "needle\n");
        write(&dir, ".gitignore", "!.gitignore\n!.env\n");

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        let targets: Vec<&str> = blocks.iter().map(|block| block.target.as_str()).collect();
        assert_eq!(targets, [".env"], "the whitelist re-includes the dotfile");
        assert!(
            outcome.response.omitted.is_empty(),
            "nothing was left out, so no bucket may name anything: {:?}",
            outcome.response.omitted
        );
        assert_eq!(
            outcome.response.footer.summary,
            "1 hit in 1 file · searched 2 files"
        );
    }

    #[test]
    fn the_same_dotfiles_without_the_whitelist_are_the_hidden_bucket() {
        let Some(dir) = repo() else { return };
        write(&dir, ".env", "needle\n");
        write(&dir, ".gitignore", "# nothing re-included\n");

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        assert_eq!(ignored_bucket(&outcome), (0, 2, 0), ".env and .gitignore");
        assert_eq!(
            outcome.response.footer.summary,
            "0 hits in 0 files · searched 0 files"
        );
    }

    #[test]
    fn a_file_that_is_both_hidden_and_gitignored_is_counted_once() {
        let Some(dir) = repo() else { return };
        write(&dir, "visible.txt", "needle\n");
        write(&dir, ".secret.log", "needle\n");
        write(&dir, ".gitignore", ".secret.log\n");

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        let (gitignore, hidden, other) = ignored_bucket(&outcome);
        assert_eq!(
            (gitignore, hidden, other),
            (1, 1, 0),
            ".secret.log is named by .gitignore though it is a dotfile too; .gitignore itself is \
             only ever a dotfile"
        );
        assert_eq!(
            gitignore + hidden + other,
            2,
            "the buckets add up to the files the search did not cover, each counted once"
        );
    }

    #[test]
    fn hits_past_max_bytes_with_no_budget_are_refused_whole() {
        let Some(dir) = repo() else { return };
        write(&dir, "a.txt", &many_hits(20));
        let mut global = global_args();
        global.max_bytes = 100;

        let outcome = run_find(&find_args("needle", &[]), &global);

        assert!(
            !outcome.response.has_output(),
            "an over-budget answer lands nowhere"
        );
        let Some(Error::OverBudget { limit, .. }) = &outcome.error else {
            panic!("expected Error::OverBudget, got {:?}", outcome.error);
        };
        assert_eq!(*limit, 100);
    }

    #[test]
    fn the_same_hits_under_a_budget_are_trimmed_and_the_budget_is_named() {
        let Some(dir) = repo() else { return };
        write(&dir, "a.txt", &many_hits(20));
        let mut global = global_args();
        global.max_bytes = 100;
        global.budget = Some(20);

        let outcome = run_find(&find_args("needle", &[]), &global);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        let kept = blocks[0].lines.len();
        assert!((1..20).contains(&kept), "trimmed to {kept} of 20 lines");
        assert!(
            matches!(
                outcome.response.omitted.as_slice(),
                [Omission::Budget { budget: 20, trimmed_target, not_shown: None }]
                    if trimmed_target == "a.txt"
            ),
            "{:?}",
            outcome.response.omitted
        );
        assert!(
            blocks[0].not_shown.is_none() && blocks[0].span.is_none(),
            "a hit block names no range, so it has no `not shown` tail either"
        );
    }

    #[test]
    fn a_binary_file_holding_the_pattern_bytes_is_skipped_and_named_as_binary() {
        let Some(dir) = read_fixture() else { return };
        let raw = std::fs::read(dir.path().join("binary.bin")).expect("the binary fixture");
        let pattern = "NUL";
        assert!(
            String::from_utf8_lossy(&raw).contains(pattern),
            "the fixture must hold the pattern, or its absence from the answer proves nothing"
        );

        // `ignore` reads the developer's own global gitignore, which would otherwise decide what
        // the walk reaches.
        let mut global = global_args();
        global.no_ignore = true;

        let outcome = run_find(&find_args(pattern, &[]), &global);

        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        assert!(blocks.is_empty(), "binary bytes are not text to search");
        assert_eq!(
            named(&outcome),
            ["ignored 2 (hidden 2)", "skipped 1 (binary 1)"],
            ".gitignore and .hidden-file are hidden; binary.bin is the one binary skip"
        );
        assert!(matches!(outcome.error, Some(Error::NoHits { .. })));

        let control = run_find(&find_args("Bottom line", &[]), &global);
        assert!(control.error.is_none(), "{:?}", control.error);
    }

    fn latin1_source() -> Vec<u8> {
        b"fn good() {}\nlet s = \"\xff\xfe\";\nfn target_fn() { 1 }\n".to_vec()
    }

    #[test]
    fn a_file_with_an_invalid_utf8_byte_is_searched_lossily_and_its_shown_lines_are_named() {
        let Some(dir) = repo() else { return };
        std::fs::write(dir.path().join("m.rs"), latin1_source()).expect("a fixture write");
        let args = FindArgs {
            no_expand: true,
            ..find_args("target_fn|let s", &[])
        };

        let outcome = run_find(&args, &global_args());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        let texts: Vec<(usize, &str)> = blocks[0]
            .lines
            .iter()
            .map(|line| (line.number, &*line.text))
            .collect();
        assert_eq!(texts, [
            (2, "\u{ab}let s\u{bb} = \"\u{fffd}\u{fffd}\";"),
            (3, "fn \u{ab}target_fn\u{bb}() { 1 }"),
        ]);
        assert_eq!(
            named(&outcome),
            ["1 non-UTF-8 line shown lossily"],
            "both lines are shown and only line 2 holds invalid bytes"
        );
        assert_eq!(
            outcome.response.footer.summary,
            "2 hits in 1 file · searched 1 file"
        );
        let json = crate::output::render(&outcome.response, Format::Json, &RenderOptions {
            numbers: true,
            quiet: false,
            cost_first: false,
        });
        assert!(json.contains("\"lossy_lines\":{\"lines\":1}"), "{json}");
    }

    #[test]
    fn the_same_file_with_valid_utf8_names_no_lossy_lines() {
        let Some(dir) = repo() else { return };
        write(
            &dir,
            "m.rs",
            "fn good() {}\nlet s = \"ok\";\nfn target_fn() { 1 }\n",
        );
        let args = FindArgs {
            no_expand: true,
            ..find_args("target_fn|let s", &[])
        };

        let outcome = run_find(&args, &global_args());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(
            outcome.response.omitted.is_empty(),
            "{:?}",
            outcome.response.omitted
        );
    }

    #[test]
    fn a_nul_past_the_sniff_window_or_a_utf16_bom_is_skipped_as_binary() {
        let Some(dir) = repo() else { return };
        let mut late_nul = b"needle \xe9\n".to_vec();
        late_nul.extend(std::iter::repeat_n(b'x', 9000));
        late_nul.push(0);
        std::fs::write(dir.path().join("late-nul.dat"), late_nul).expect("a fixture write");
        std::fs::write(dir.path().join("bom.txt"), b"\xff\xfeneedle\n").expect("a fixture write");

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        assert!(matches!(outcome.error, Some(Error::NoHits { .. })));
        assert_eq!(named(&outcome), ["skipped 2 (binary 2)"]);
        assert_eq!(
            outcome.response.footer.summary,
            "0 hits in 0 files · searched 0 files"
        );
    }

    /// Valid UTF-8 either way, so only the NUL can make it binary.
    fn hit_then_nul(nul: usize, with_nul: bool) -> Vec<u8> {
        let mut bytes = b"needle\n".to_vec();
        bytes.resize(nul, b'a');
        bytes.push(if with_nul { 0 } else { b'a' });
        bytes.push(b'\n');
        bytes
    }

    /// The last byte of `fs::read`'s sniff window, the first byte past it, and a byte past the
    /// first buffer the searcher fills, after the hit on line 1 was already read.
    fn nul_positions() -> [(&'static str, usize); 3] {
        let window = crate::fs::BINARY_SNIFF_WINDOW;
        [
            ("inside.dat", window - 1),
            ("outside.dat", window),
            ("far.dat", 200_000),
        ]
    }

    #[test]
    fn a_nul_anywhere_is_binary_to_find_on_either_side_of_the_sniff_window() {
        // Unlike `show`, which keeps `fs::read`'s 8 KiB window, a NUL anywhere is binary.
        let Some(dir) = repo() else { return };
        for (name, at) in nul_positions() {
            std::fs::write(dir.path().join(name), hit_then_nul(at, true)).expect("a fixture");
        }

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        assert!(matches!(outcome.error, Some(Error::NoHits { .. })));
        assert_eq!(named(&outcome), ["skipped 3 (binary 3)"]);
        assert_eq!(
            outcome.response.footer.summary,
            "0 hits in 0 files \u{b7} searched 0 files"
        );
    }

    #[test]
    fn the_same_files_without_the_nul_are_searched() {
        let Some(dir) = repo() else { return };
        for (name, at) in nul_positions() {
            std::fs::write(dir.path().join(name), hit_then_nul(at, false)).expect("a fixture");
        }
        let args = FindArgs {
            no_expand: true,
            ..find_args("needle", &[])
        };

        let outcome = run_find(&args, &global_args());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(
            outcome.response.omitted.is_empty(),
            "{:?}",
            outcome.response.omitted
        );
        assert_eq!(
            outcome.response.footer.summary,
            "3 hits in 3 files \u{b7} searched 3 files"
        );
    }

    /// Returns whether the mode took effect: uid 0 lists a directory whatever its mode says.
    fn locked_dir(dir: &Workdir, name: &str) -> bool {
        use std::os::unix::fs::PermissionsExt as _;

        let path = dir.path().join(name);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
            .expect("a fixture directory this process owns");
        std::fs::read_dir(&path).is_err()
    }

    fn unlock_dir(dir: &Workdir, name: &str) {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::set_permissions(
            dir.path().join(name),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("restoring the fixture directory so the sandbox can be removed");
    }

    #[test]
    fn a_directory_the_walk_cannot_list_is_named_unreadable_and_the_rest_is_searched() {
        let Some(dir) = repo() else { return };
        std::fs::create_dir(dir.path().join("locked")).expect("a nested directory");
        write(&dir, "locked/g.txt", "zzz findme\n");
        write(&dir, "a.txt", "findme\n");
        let listable = !locked_dir(&dir, "locked");

        let outcome = run_find(&find_args("findme", &[]), &global_args());
        unlock_dir(&dir, "locked");
        if listable {
            eprintln!(
                "skipped (a_directory_the_walk_cannot_list_is_named_unreadable_and_the_rest_is_\
                 searched): mode 0o000 did not refuse this reader"
            );
            return;
        }

        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        let targets: Vec<&str> = blocks.iter().map(|block| block.target.as_str()).collect();
        assert_eq!(targets, ["a.txt"]);
        assert_eq!(named(&outcome), ["locked unreadable"]);
        let json = crate::output::render(&outcome.response, Format::Json, &RenderOptions {
            numbers: true,
            quiet: false,
            cost_first: false,
        });
        assert!(
            json.contains("\"unreadable\":{\"path\":\"locked\"}"),
            "{json}"
        );
    }

    #[test]
    fn the_same_directory_listable_is_searched_and_named_nowhere() {
        let Some(dir) = repo() else { return };
        std::fs::create_dir(dir.path().join("locked")).expect("a nested directory");
        write(&dir, "locked/g.txt", "zzz findme\n");
        write(&dir, "a.txt", "findme\n");

        let outcome = run_find(&find_args("findme", &[]), &global_args());

        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        let targets: Vec<&str> = blocks.iter().map(|block| block.target.as_str()).collect();
        assert_eq!(targets, ["a.txt", "locked/g.txt"]);
        assert!(
            outcome.response.omitted.is_empty(),
            "{:?}",
            outcome.response.omitted
        );
    }

    #[test]
    fn a_glob_narrows_the_walk_to_matching_paths_and_is_named() {
        let Some(dir) = repo() else { return };
        std::fs::create_dir(dir.path().join("src")).expect("a nested directory");
        write(&dir, "a.ts", "needle\n");
        write(&dir, "src/b.ts", "needle\n");
        write(&dir, "c.txt", "needle\n");

        let mut args = find_args("needle", &[]);
        args.globs = vec!["*.ts".to_owned()];
        let outcome = run_find(&args, &global_args());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        let targets: Vec<&str> = blocks.iter().map(|block| block.target.as_str()).collect();
        assert_eq!(targets, ["a.ts", "src/b.ts"]);
        assert_eq!(
            named(&outcome),
            ["glob *.ts"],
            "c.txt was left out by the glob, which is named, not by a filter"
        );
        assert_eq!(
            outcome.response.footer.summary,
            "2 hits in 2 files · searched 2 files"
        );
    }

    #[test]
    fn with_no_glob_every_non_ignored_file_is_searched_and_no_glob_is_named() {
        let Some(dir) = repo() else { return };
        std::fs::create_dir(dir.path().join("src")).expect("a nested directory");
        write(&dir, "a.ts", "needle\n");
        write(&dir, "src/b.ts", "needle\n");
        write(&dir, "c.txt", "needle\n");

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        let targets: Vec<&str> = blocks.iter().map(|block| block.target.as_str()).collect();
        assert_eq!(targets, ["a.ts", "c.txt", "src/b.ts"]);
        assert!(
            outcome.response.omitted.is_empty(),
            "{:?}",
            outcome.response.omitted
        );
    }

    fn gitignored_log(dir: &Workdir) {
        write(dir, ".gitignore", "x.log\n");
        write(dir, "x.log", "needle\n");
        write(dir, "a.txt", "needle\n");
    }

    #[test]
    fn a_glob_outranks_gitignore_for_a_file_it_matches() {
        let Some(dir) = repo() else { return };
        gitignored_log(&dir);

        let mut args = find_args("needle", &[]);
        args.globs = vec!["*.log".to_owned()];
        let outcome = run_find(&args, &global_args());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        let targets: Vec<&str> = blocks.iter().map(|block| block.target.as_str()).collect();
        assert_eq!(targets, ["x.log"]);
        assert_eq!(
            named(&outcome),
            ["glob *.log"],
            "x.log was searched, so no ignore bucket may claim it"
        );
    }

    #[test]
    fn without_the_glob_the_gitignored_file_is_not_found_and_is_counted_as_gitignore() {
        let Some(dir) = repo() else { return };
        gitignored_log(&dir);

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        let targets: Vec<&str> = blocks.iter().map(|block| block.target.as_str()).collect();
        assert_eq!(targets, ["a.txt"]);
        assert_eq!(
            ignored_bucket(&outcome),
            (1, 1, 0),
            "x.log is gitignore's; .gitignore itself is hidden"
        );
    }

    #[test]
    fn a_gitignored_directory_is_named_while_a_glob_is_active() {
        // A glob matches files, never the directory above one, so `node_modules/` is pruned first.
        let Some(dir) = repo() else { return };
        std::fs::create_dir(dir.path().join("node_modules")).expect("a nested directory");
        write(&dir, ".gitignore", "node_modules/\n");
        write(&dir, "node_modules/dep.log", "needle\n");
        write(&dir, "x.log", "needle\n");

        let mut args = find_args("needle", &[]);
        args.globs = vec!["*.log".to_owned()];
        let outcome = run_find(&args, &global_args());

        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        let targets: Vec<&str> = blocks.iter().map(|block| block.target.as_str()).collect();
        assert_eq!(targets, ["x.log"]);
        assert_eq!(named(&outcome), [
            "glob *.log",
            "ignored dirs (gitignore node_modules/)"
        ]);
        assert_eq!(
            outcome.response.footer.summary,
            "1 hit in 1 file \u{b7} searched 1 file"
        );
    }

    /// `target/locked` is unlistable: had the walk entered `target/`, it would be named unreadable.
    fn gitignored_target(dir: &Workdir) -> bool {
        for sub in ["target", "target/debug", "target/locked", ".cache", "src"] {
            std::fs::create_dir(dir.path().join(sub)).expect("a nested directory");
        }
        write(dir, ".gitignore", "target/\n*.log\n");
        write(dir, "target/debug/build.rs", "needle\n");
        write(dir, "target/locked/deep.rs", "needle\n");
        write(dir, ".cache/entry.txt", "needle\n");
        write(dir, "src/main.rs", "needle\n");
        write(dir, "src/run.log", "needle\n");
        locked_dir(dir, "target/locked")
    }

    #[test]
    fn a_gitignored_target_directory_is_named_and_never_entered() {
        let Some(dir) = repo() else { return };
        let modes_bite = gitignored_target(&dir);

        let sequential = run_with(
            &find_args("needle", &[]),
            &global_args(),
            Some(Traversal::Sequential),
        );
        let parallel = run_with(
            &find_args("needle", &[]),
            &global_args(),
            Some(Traversal::Parallel { threads: 4 }),
        );
        unlock_dir(&dir, "target/locked");

        let Body::Targets(blocks) = &sequential.response.body else {
            panic!("expected Body::Targets");
        };
        let targets: Vec<&str> = blocks.iter().map(|block| block.target.as_str()).collect();
        assert_eq!(targets, ["src/main.rs"]);
        assert_eq!(
            named(&sequential),
            [
                "ignored 2 (gitignore 1 \u{b7} hidden 1) \u{b7} ignored dirs (gitignore target/ \u{b7} \
              hidden .cache/)"
            ],
            "src/run.log is gitignore's and .gitignore is a dotfile; the files under .cache/ and \
             target/ are counted nowhere"
        );
        assert_eq!(rendered(&parallel), rendered(&sequential));
        if !modes_bite {
            eprintln!(
                "skipped (a_gitignored_target_directory_is_named_and_never_entered, unreadable \
                 control): mode 0o000 did not refuse this reader"
            );
        }
    }

    #[test]
    fn the_same_tree_searched_with_no_ignore_enters_target_and_names_its_unreadable_directory() {
        let Some(dir) = repo() else { return };
        let modes_bite = gitignored_target(&dir);
        let mut global = global_args();
        global.no_ignore = true;

        let outcome = run_find(&find_args("needle", &[]), &global);
        unlock_dir(&dir, "target/locked");
        if !modes_bite {
            eprintln!(
                "skipped (the_same_tree_searched_with_no_ignore_enters_target_and_names_its_\
                 unreadable_directory): mode 0o000 did not refuse this reader"
            );
            return;
        }

        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        let targets: Vec<&str> = blocks.iter().map(|block| block.target.as_str()).collect();
        assert_eq!(targets, [
            "src/main.rs",
            "src/run.log",
            "target/debug/build.rs"
        ]);
        assert_eq!(named(&outcome), [
            "ignored 1 (hidden 1) \u{b7} ignored dirs (hidden .cache/)",
            "target/locked unreadable",
        ]);
    }

    #[test]
    fn with_hidden_no_dotfile_or_hidden_directory_is_named() {
        let Some(dir) = repo() else { return };
        gitignored_target(&dir);
        let mut args = find_args("needle", &[]);
        args.hidden = true;

        let outcome = run_find(&args, &global_args());
        unlock_dir(&dir, "target/locked");

        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        let targets: Vec<&str> = blocks.iter().map(|block| block.target.as_str()).collect();
        assert_eq!(targets, [".cache/entry.txt", "src/main.rs"]);
        assert_eq!(named(&outcome), [
            "ignored 1 (gitignore 1) \u{b7} ignored dirs (gitignore target/)"
        ]);
    }

    #[test]
    fn only_the_first_five_ignored_directories_are_named_in_path_order_within_a_depth() {
        let Some(dir) = repo() else { return };
        let names = ["g", "b", "f", "a", "e", "c", "d"];
        for name in names {
            std::fs::create_dir(dir.path().join(name)).expect("a nested directory");
            write(&dir, &format!("{name}/x.txt"), "needle\n");
        }
        write(&dir, ".gitignore", "/[a-g]/\n");
        write(&dir, "kept.txt", "needle\n");

        let parallel = Some(Traversal::Parallel { threads: 8 });
        let outcome = run_with(&find_args("needle", &[]), &global_args(), parallel);

        let Some(Omission::Ignored { dirs, .. }) = outcome
            .response
            .omitted
            .iter()
            .find(|omission| matches!(omission, Omission::Ignored { .. }))
        else {
            panic!(
                "expected an Omission::Ignored: {:?}",
                outcome.response.omitted
            );
        };
        assert_eq!(dirs.gitignore.named, ["a", "b", "c", "d", "e"]);
        assert_eq!(dirs.gitignore.more, 2, "f/ and g/");
        assert!(dirs.hidden.named.is_empty() && dirs.hidden.more == 0);
        assert_eq!(named(&outcome), [
            "ignored 1 (hidden 1) \u{b7} ignored dirs (gitignore a/, b/, c/, d/, e/ (+2 more))"
        ]);
    }

    #[test]
    fn a_top_level_ignored_directory_is_named_before_a_nested_one_it_sorts_after() {
        let Some(dir) = repo() else { return };
        for sub in ["a", "a/deep", "z"] {
            std::fs::create_dir(dir.path().join(sub)).expect("a nested directory");
        }
        write(&dir, "a/deep/x.txt", "needle\n");
        write(&dir, "z/x.txt", "needle\n");
        write(&dir, "a/kept.txt", "needle\n");
        write(&dir, ".gitignore", "deep/\n/z/\n");

        let outcome = run_with(
            &find_args("needle", &[]),
            &global_args(),
            Some(Traversal::Sequential),
        );

        assert_eq!(named(&outcome), [
            "ignored 1 (hidden 1) \u{b7} ignored dirs (gitignore z/, a/deep/)"
        ]);
    }

    fn footer_of(outcome: &Outcome) -> String {
        let text = crate::output::render(&outcome.response, Format::Text, &RenderOptions {
            numbers: true,
            quiet: true,
            cost_first: false,
        });
        let footer = text.lines().last().unwrap_or_default();
        footer
            .rsplit_once(" \u{b7} ~")
            .map_or(footer, |(omissions, _cost)| omissions)
            .to_owned()
    }

    fn json_dirs(outcome: &Outcome) -> serde_json::Value {
        let json = crate::output::render(&outcome.response, Format::Json, &RenderOptions {
            numbers: true,
            quiet: true,
            cost_first: false,
        });
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let ignored = value["omitted"]
            .as_array()
            .and_then(|omitted| omitted.iter().find_map(|omission| omission.get("ignored")))
            .unwrap_or_else(|| panic!("expected an ignored omission: {json}"));
        ignored["dirs"].clone()
    }

    /// Six gitignored top-level directories would fill the five names alone.
    fn six_gitignored_and_one_nested_hidden(dir: &Workdir) {
        for sub in ["a", "b", "c", "d", "e", "f", "x", "x/.h"] {
            std::fs::create_dir(dir.path().join(sub)).expect("a nested directory");
            write(dir, &format!("{sub}/n.txt"), "needle\n");
        }
        write(dir, ".gitignore", "/[a-f]/\n");
    }

    #[test]
    fn each_source_that_pruned_a_directory_keeps_a_name_when_the_other_fills_the_five() {
        let Some(dir) = repo() else { return };
        six_gitignored_and_one_nested_hidden(&dir);

        for traversal in [Traversal::Sequential, Traversal::Parallel { threads: 4 }] {
            let outcome = run_with(&find_args("needle", &[]), &global_args(), Some(traversal));

            assert_eq!(
                named(&outcome),
                [
                    "ignored 1 (hidden 1) \u{b7} ignored dirs (gitignore a/, b/, c/, d/ (+2 more) \
                  \u{b7} hidden x/.h/)"
                ],
                "{traversal:?}: .gitignore is the hidden file; x/.h/ is deeper than all six, yet \
                 the only name that says --hidden brings a directory back"
            );
        }
    }

    #[test]
    fn with_hidden_the_gitignored_directories_alone_take_all_five_names() {
        let Some(dir) = repo() else { return };
        six_gitignored_and_one_nested_hidden(&dir);
        let mut args = find_args("needle", &[]);
        args.hidden = true;

        let outcome = run_find(&args, &global_args());

        assert_eq!(named(&outcome), [
            "ignored dirs (gitignore a/, b/, c/, d/, e/ (+1 more))"
        ]);
    }

    #[test]
    fn with_no_ignore_the_hidden_directory_alone_is_named() {
        let Some(dir) = repo() else { return };
        six_gitignored_and_one_nested_hidden(&dir);
        let mut global = global_args();
        global.no_ignore = true;

        let outcome = run_find(&find_args("needle", &[]), &global);

        assert_eq!(named(&outcome), [
            "ignored 1 (hidden 1) \u{b7} ignored dirs (hidden x/.h/)"
        ]);
    }

    /// `None` where the filesystem refuses a name that is not UTF-8, as APFS does.
    #[cfg(unix)]
    fn gitignored_dir_named(dir: &Workdir, name: &[u8]) -> Option<()> {
        use std::os::unix::ffi::OsStrExt as _;
        let path = dir.path().join(std::ffi::OsStr::from_bytes(name));
        std::fs::create_dir(&path).ok()?;
        std::fs::write(path.join("n.txt"), "needle\n").expect("test fixture writes");
        write(dir, "kept.txt", "needle\n");
        write(dir, ".gitignore", "bad*\n");
        Some(())
    }

    #[cfg(unix)]
    #[test]
    fn a_pruned_directory_whose_name_is_not_utf8_reaches_json_lossily_as_text_shows_it() {
        let Some(dir) = repo() else { return };
        if gitignored_dir_named(&dir, b"bad\xffdir").is_none() {
            eprintln!(
                "skipped (a_pruned_directory_whose_name_is_not_utf8_reaches_json_lossily_as_text_\
                 shows_it): the filesystem refused a non-UTF-8 name"
            );
            return;
        }

        for traversal in [Traversal::Sequential, Traversal::Parallel { threads: 4 }] {
            let outcome = run_with(&find_args("needle", &[]), &global_args(), Some(traversal));

            assert_eq!(
                json_dirs(&outcome)["gitignore"],
                serde_json::json!({"named": ["bad\u{fffd}dir"], "more": 0}),
                "{traversal:?}"
            );
            assert_eq!(
                footer_of(&outcome),
                "\u{2500}\u{2500} 1 hit in 1 file \u{b7} searched 1 file \u{b7} ignored 1 (hidden 1) \
                 \u{b7} ignored dirs (gitignore bad\u{fffd}dir/)",
                "{traversal:?}: .gitignore is the hidden file"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_pruned_directory_whose_name_is_utf8_reaches_json_as_is() {
        let Some(dir) = repo() else { return };
        gitignored_dir_named(&dir, b"bad\xc3\xa9dir").expect("a UTF-8 name");

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        assert_eq!(
            json_dirs(&outcome)["gitignore"],
            serde_json::json!({"named": ["bad\u{e9}dir"], "more": 0})
        );
    }

    fn hidden_dir_named(dir: &Workdir, name: &str) {
        std::fs::create_dir(dir.path().join(name)).expect("a nested directory");
        write(dir, &format!("{name}/n.txt"), "needle\n");
        write(dir, "kept.txt", "needle\n");
    }

    #[test]
    fn a_newline_in_a_pruned_directory_name_is_escaped_and_the_footer_stays_one_line() {
        let Some(dir) = repo() else { return };
        hidden_dir_named(&dir, ".a\nb");

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        assert_eq!(
            footer_of(&outcome),
            "\u{2500}\u{2500} 1 hit in 1 file \u{b7} searched 1 file \u{b7} ignored dirs (hidden \
             .a\\nb/)"
        );
        assert_eq!(
            json_dirs(&outcome)["hidden"],
            serde_json::json!({"named": [".a\nb"], "more": 0}),
            "JSON escapes the newline itself, so the name reaches it whole"
        );
    }

    #[test]
    fn a_pruned_directory_name_with_no_control_character_is_footed_as_is() {
        let Some(dir) = repo() else { return };
        hidden_dir_named(&dir, ".a b");

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        assert_eq!(
            footer_of(&outcome),
            "\u{2500}\u{2500} 1 hit in 1 file \u{b7} searched 1 file \u{b7} ignored dirs (hidden \
             .a b/)"
        );
    }

    #[test]
    fn an_unparsable_glob_is_an_invalid_pattern_before_any_walk() {
        let Some(dir) = repo() else { return };
        write(&dir, "a.ts", "needle\n");

        let mut args = find_args("needle", &[]);
        args.globs = vec!["*.[ts".to_owned()];
        let outcome = run_find(&args, &global_args());

        assert!(!outcome.response.has_output());
        let Some(Error::InvalidPattern { pattern, .. }) = &outcome.error else {
            panic!("expected Error::InvalidPattern, got {:?}", outcome.error);
        };
        assert_eq!(pattern, "*.[ts");
    }

    #[test]
    fn a_hit_on_a_line_over_the_display_cap_is_cut_around_the_match_and_named() {
        // Over the default 65,536 `--max-bytes`, so an uncut line would refuse the whole search.
        let Some(dir) = repo() else { return };
        let line = format!("{}needle{}\n", "x".repeat(40_000), "y".repeat(30_000));
        write(&dir, "min.js", &line);

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        let expected = format!(
            "\u{2026}{}\u{ab}needle\u{bb}{}\u{2026}",
            "x".repeat(200),
            "y".repeat(200)
        );
        assert_eq!(blocks[0].lines[0].text, expected);
        assert_eq!(named(&outcome), ["1 long line cut"]);
    }

    #[test]
    fn a_hit_on_a_line_within_the_display_cap_is_shown_whole() {
        let Some(dir) = repo() else { return };
        let text = format!("{}needle{}", "x".repeat(450), "y".repeat(444));
        write(&dir, "a.js", &format!("{text}\n"));

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        assert_eq!(
            blocks[0].lines[0].text,
            format!("{}\u{ab}needle\u{bb}{}", "x".repeat(450), "y".repeat(444))
        );
        assert!(
            outcome.response.omitted.is_empty(),
            "{:?}",
            outcome.response.omitted
        );
    }

    /// `src/` plus `files` files under it: `files + 1` entries.
    fn tree_of(files: usize) -> tempfile::TempDir {
        let root = tempfile::TempDir::new().expect("temp dir");
        std::fs::create_dir(root.path().join("src")).expect("a nested directory");
        for n in 0..files {
            std::fs::write(root.path().join(format!("src/f{n}.txt")), "needle\n")
                .expect("test fixture writes");
        }
        root
    }

    #[test]
    fn a_directory_tree_over_the_entry_budget_takes_the_parallel_walk() {
        let root = tree_of(SEQUENTIAL_WALK_MAX_ENTRIES);

        assert_eq!(
            traversal_of(&[root.path().to_path_buf()]),
            Traversal::Parallel { threads: 0 }
        );
    }

    #[test]
    fn a_directory_tree_at_the_entry_budget_is_walked_on_one_thread() {
        let root = tree_of(SEQUENTIAL_WALK_MAX_ENTRIES - 1);

        assert_eq!(
            traversal_of(&[root.path().to_path_buf()]),
            Traversal::Sequential
        );
    }

    #[test]
    fn a_tree_of_more_directories_than_the_probe_lists_takes_the_parallel_walk() {
        let root = tempfile::TempDir::new().expect("temp dir");
        for n in 0..SEQUENTIAL_WALK_MAX_DIRS {
            std::fs::create_dir(root.path().join(format!("d{n}"))).expect("a nested directory");
        }

        assert_eq!(
            traversal_of(&[root.path().to_path_buf()]),
            Traversal::Parallel { threads: 0 },
            "the root and its {SEQUENTIAL_WALK_MAX_DIRS} subdirectories are one listing too many"
        );
    }

    #[test]
    fn a_tree_of_as_many_directories_as_the_probe_lists_is_walked_on_one_thread() {
        let root = tempfile::TempDir::new().expect("temp dir");
        for n in 1..SEQUENTIAL_WALK_MAX_DIRS {
            std::fs::create_dir(root.path().join(format!("d{n}"))).expect("a nested directory");
        }

        assert_eq!(
            traversal_of(&[root.path().to_path_buf()]),
            Traversal::Sequential
        );
    }

    #[test]
    fn a_git_directory_counts_nothing_toward_the_entry_budget() {
        let root = tree_of(SEQUENTIAL_WALK_MAX_ENTRIES - 1);
        std::fs::create_dir(root.path().join(".git")).expect("a nested directory");
        for n in 0..10 {
            std::fs::write(root.path().join(format!(".git/o{n}")), "x").expect("fixture writes");
        }

        assert_eq!(
            traversal_of(&[root.path().to_path_buf()]),
            Traversal::Sequential
        );
    }

    #[test]
    fn a_root_that_is_an_explicit_file_keeps_the_walk_sequential() {
        let root = tree_of(SEQUENTIAL_WALK_MAX_ENTRIES);
        std::fs::write(root.path().join("a.ts"), "needle\n").expect("test fixture writes");
        let roots = [root.path().to_path_buf(), root.path().join("a.ts")];

        assert_eq!(traversal_of(&roots), Traversal::Sequential);
    }

    /// `a` and `a-b` order differently by path component (`a` first) than by raw bytes (`-` sorts
    /// before `/`), so the tree pins which order a hit list takes.
    const DIRS: [&str; 13] = [
        "a", "a-b", "b", "d00", "d01", "d02", "d03", "d04", "d05", "d06", "d07", "d08", "d09",
    ];
    const FILES_PER_DIR: usize = 30;

    fn is_hit(dir: usize, file: usize) -> bool {
        (dir * FILES_PER_DIR + file).is_multiple_of(11)
    }

    /// Returns whether the permission modes took effect.
    fn many_file_tree(dir: &Workdir) -> bool {
        for (d, name) in DIRS.iter().enumerate() {
            std::fs::create_dir(dir.path().join(name)).expect("a nested directory");
            for f in 0..FILES_PER_DIR {
                let third = if is_hit(d, f) {
                    "needle three"
                } else {
                    "three"
                };
                write(
                    dir,
                    &format!("{name}/f{f:02}.txt"),
                    &format!("one\ntwo\n{third}\nfour\n"),
                );
            }
        }
        for name in ["a-b", "d03"] {
            std::fs::write(
                dir.path().join(name).join("latin.txt"),
                b"one\nneedle \xe9t\xe9\nthree\n",
            )
            .expect("test fixture writes");
        }
        for n in 0..3 {
            std::fs::write(dir.path().join(format!("d05/z{n}.bin")), b"needle\0\n")
                .expect("test fixture writes");
        }
        for n in 0..2 {
            write(
                dir,
                &format!("d07/big{n}.txt"),
                &format!("needle\n{}\n", "x".repeat(2000)),
            );
        }
        let mut modes_bite = true;
        for sealed in ["d02/sealed", "d09/sealed"] {
            std::fs::create_dir(dir.path().join(sealed)).expect("a nested directory");
            write(dir, &format!("{sealed}/hidden.txt"), "needle\n");
            modes_bite &= locked_dir(dir, sealed);
        }
        write(dir, "d08/locked.txt", "needle\n");
        modes_bite && locked(dir, "d08/locked.txt")
    }

    fn rendered(outcome: &Outcome) -> String {
        let opts = RenderOptions {
            numbers: true,
            quiet: false,
            cost_first: false,
        };
        let mut both = crate::output::render(&outcome.response, Format::Text, &opts);
        both.push_str(&crate::output::render(
            &outcome.response,
            Format::Json,
            &opts,
        ));
        both
    }

    /// Three times the reset limit of sibling directories, each with its own `.gitignore`, so a
    /// copy's rules must come back right after every reset.
    fn many_ignoring_directories(dir: &Workdir) -> usize {
        let dirs = 3 * MATCHER_PARENTS_MAX;
        write(dir, ".gitignore", "gen/\n");
        for i in 0..dirs {
            let sub = format!("p{i:03}");
            std::fs::create_dir_all(dir.path().join(&sub).join("gen")).expect("nested directories");
            write(dir, &format!("{sub}/.gitignore"), "*.skip\n");
            for name in ["keep.txt", "x.skip", "gen/g.txt"] {
                write(dir, &format!("{sub}/{name}"), "needle\n");
            }
        }
        dirs
    }

    /// `ignore` exposes no count of the directories a matcher holds, so its `Debug` form is read.
    fn compiled_dirs(matchers: &[IncrementalIgnore]) -> usize {
        matchers
            .iter()
            .map(|matcher| format!("{matcher:?}").matches("Allowed(").count())
            .sum()
    }

    fn judge_sorted_walk(mut judge: impl FnMut(&DirEntry)) {
        let roots = [PathBuf::from(".")];
        let (mut builder, _) = walker(&roots, true, true, &Override::empty());
        for entry in builder.sort_by_file_path(Path::cmp).build().flatten() {
            judge(&entry);
        }
    }

    #[test]
    fn a_filters_copy_ends_a_walk_holding_at_most_the_parent_limit_of_directories() {
        let Some(dir) = repo() else { return };
        let dirs = many_ignoring_directories(&dir);
        let roots = [PathBuf::from(".")];
        let (_, filters) = walker(&roots, true, true, &Override::empty());
        let mut filters = filters.expect("both filters are on");

        judge_sorted_walk(|entry| {
            filters.judge(entry);
        });

        let held = compiled_dirs(&filters.matchers);
        assert!(
            held <= MATCHER_PARENTS_MAX,
            "held {held} of {dirs} directories"
        );
    }

    #[test]
    fn a_bare_matcher_on_the_same_tree_ends_it_holding_every_directory() {
        let Some(dir) = repo() else { return };
        let dirs = many_ignoring_directories(&dir);
        let roots = [PathBuf::from(".")];
        let (_, filters) = walker(&roots, true, true, &Override::empty());
        let mut matchers = filters.expect("both filters are on").matchers;

        judge_sorted_walk(|entry| {
            if entry.depth() > 0 {
                let relative = entry.path().strip_prefix(".").unwrap_or(entry.path());
                let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
                matchers[0].matched(relative, is_dir);
            }
        });

        let held = compiled_dirs(&matchers);
        assert!(
            held >= dirs,
            "held {held} of {dirs} directories: the measure no longer sees the growth"
        );
    }

    #[test]
    fn every_directory_s_own_gitignore_still_applies_after_its_matcher_is_reset() {
        let Some(dir) = repo() else { return };
        let dirs = many_ignoring_directories(&dir);
        let args = FindArgs {
            files: true,
            ..find_args("needle", &[])
        };
        let expected: Vec<PathBuf> = (0..dirs)
            .map(|i| PathBuf::from(format!("p{i:03}/keep.txt")))
            .collect();

        for traversal in [Traversal::Sequential, Traversal::Parallel { threads: 4 }] {
            let outcome = run_with(&args, &global_args(), Some(traversal));

            let Body::Files(files) = &outcome.response.body else {
                panic!("expected Body::Files");
            };
            assert_eq!(files, &expected, "{traversal:?}");
            assert_eq!(
                ignored_bucket(&outcome),
                (dirs, dirs + 1, 0),
                "{traversal:?}: every x.skip is its directory's gitignore's, every .gitignore is \
                 hidden"
            );
        }
    }

    #[test]
    fn the_same_tree_with_no_ignore_lists_every_skip_and_gen_file() {
        let Some(dir) = repo() else { return };
        let dirs = many_ignoring_directories(&dir);
        let args = FindArgs {
            files: true,
            ..find_args("needle", &[])
        };
        let mut global = global_args();
        global.no_ignore = true;

        let outcome = run_with(&args, &global, Some(Traversal::Parallel { threads: 4 }));

        let Body::Files(files) = &outcome.response.body else {
            panic!("expected Body::Files");
        };
        assert_eq!(
            files.len(),
            3 * dirs,
            "keep.txt, x.skip and gen/g.txt per directory"
        );
    }

    #[test]
    fn a_parallel_walk_prints_the_sequential_walks_bytes_on_every_run_and_thread_count() {
        let Some(dir) = repo() else { return };
        let modes_bite = many_file_tree(&dir);
        let mut global = global_args();
        global.max_file_bytes = 1024;
        let mut targets = find_args("needle", &[]);
        targets.context = Some(1);
        let files = FindArgs {
            files: true,
            ..find_args("needle", &[])
        };
        let count = FindArgs {
            count: true,
            ..find_args("needle", &[])
        };

        let mut runs = Vec::new();
        for args in [&targets, &files, &count] {
            let sequential = run_with(args, &global, Some(Traversal::Sequential));
            let mut parallel = Vec::new();
            for threads in [1, 2, 3, 8, 0] {
                for _ in 0..3 {
                    let outcome = run_with(args, &global, Some(Traversal::Parallel { threads }));
                    parallel.push((threads, rendered(&outcome)));
                }
            }
            runs.push((sequential, parallel));
        }
        unlock_dir(&dir, "d02/sealed");
        unlock_dir(&dir, "d09/sealed");
        if !modes_bite {
            eprintln!(
                "skipped (a_parallel_walk_prints_the_sequential_walks_bytes_on_every_run_and_\
                 thread_count): mode 0o000 did not refuse this reader"
            );
            return;
        }

        let (sequential, _) = &runs[0];
        let Body::Targets(blocks) = &sequential.response.body else {
            panic!("expected Body::Targets");
        };
        let mut expected = Vec::new();
        for (d, name) in DIRS.iter().enumerate() {
            for f in (0..FILES_PER_DIR).filter(|f| is_hit(d, *f)) {
                expected.push(format!("{name}/f{f:02}.txt"));
            }
            if ["a-b", "d03"].contains(name) {
                expected.push(format!("{name}/latin.txt"));
            }
        }
        let got: Vec<&str> = blocks.iter().map(|block| block.target.as_str()).collect();
        assert_eq!(got, expected, "hits in path-component order");
        assert_eq!(named(sequential), [
            "2 non-UTF-8 lines shown lossily",
            "skipped 6 (binary 3 \u{b7} too large 2 \u{b7} unreadable 1)",
            "d02/sealed unreadable",
            "d09/sealed unreadable",
        ]);
        assert_eq!(
            sequential.response.footer.summary,
            format!(
                "{n} hits in {n} files \u{b7} searched 392 files",
                n = expected.len()
            )
        );

        for (sequential, parallel) in &runs {
            let want = rendered(sequential);
            for (threads, got) in parallel {
                assert_eq!(got, &want, "threads = {threads}");
            }
        }
    }

    #[test]
    fn roots_typed_in_mixed_forms_print_the_sequential_walks_bytes_at_every_thread_count() {
        let Some(dir) = repo() else { return };
        let modes_bite = many_file_tree(&dir);
        let mut global = global_args();
        global.max_file_bytes = 1024;
        // `./d09` sorts before `a` as typed (`.` is a component before any name) and after it
        // as a root, which is the order the sequential walk takes the roots in.
        let mut mixed = find_args("needle", &["./d09", "a", "./a-b", "d02"]);
        mixed.context = Some(1);
        let mixed_sequential = run_with(&mixed, &global, Some(Traversal::Sequential));
        let mixed_parallel: Vec<(usize, String)> = [1, 2, 8]
            .into_iter()
            .map(|threads| {
                let outcome = run_with(&mixed, &global, Some(Traversal::Parallel { threads }));
                (threads, rendered(&outcome))
            })
            .collect();
        unlock_dir(&dir, "d02/sealed");
        unlock_dir(&dir, "d09/sealed");
        if !modes_bite {
            eprintln!(
                "skipped (roots_typed_in_mixed_forms_print_the_sequential_walks_bytes_at_every_\
                 thread_count): mode 0o000 did not refuse this reader"
            );
            return;
        }

        let Body::Targets(blocks) = &mixed_sequential.response.body else {
            panic!("expected Body::Targets");
        };
        let mut expected = Vec::new();
        for name in ["a", "a-b", "d02", "d09"] {
            let d = DIRS
                .iter()
                .position(|dir| *dir == name)
                .expect("a fixture dir");
            for f in (0..FILES_PER_DIR).filter(|f| is_hit(d, *f)) {
                expected.push(format!("{name}/f{f:02}.txt"));
            }
            if name == "a-b" {
                expected.push("a-b/latin.txt".to_owned());
            }
        }
        let got: Vec<&str> = blocks.iter().map(|block| block.target.as_str()).collect();
        assert_eq!(got, expected, "root by root, in the roots' canonical order");
        assert_eq!(named(&mixed_sequential), [
            "1 non-UTF-8 line shown lossily",
            "d02/sealed unreadable",
            "d09/sealed unreadable",
        ]);
        let want = rendered(&mixed_sequential);
        for (threads, got) in &mixed_parallel {
            assert_eq!(got, &want, "mixed roots, threads = {threads}");
        }
    }

    #[test]
    fn the_same_tree_with_one_more_hit_prints_different_bytes() {
        let Some(dir) = repo() else { return };
        for name in ["a", "a-b"] {
            std::fs::create_dir(dir.path().join(name)).expect("a nested directory");
            write(&dir, &format!("{name}/f.txt"), "needle\n");
        }
        let args = find_args("needle", &[]);
        let parallel = Some(Traversal::Parallel { threads: 4 });

        let before = rendered(&run_with(&args, &global_args(), parallel));
        write(&dir, "a/g.txt", "needle\n");
        let after = run_with(&args, &global_args(), parallel);

        assert_ne!(rendered(&after), before);
        let Body::Targets(blocks) = &after.response.body else {
            panic!("expected Body::Targets");
        };
        let targets: Vec<&str> = blocks.iter().map(|block| block.target.as_str()).collect();
        assert_eq!(targets, ["a/f.txt", "a/g.txt", "a-b/f.txt"]);
    }

    /// Every flag pair `find` can run with, as `(--hidden, --no-ignore)`.
    const FLAG_PAIRS: [(bool, bool); 4] =
        [(false, false), (true, false), (false, true), (true, true)];

    fn sorted(mut files: Vec<PathBuf>) -> Vec<PathBuf> {
        files.sort();
        files
    }

    /// The files `find` opens, from `walker` and the filters each traversal applies, through both.
    fn files_find_walks(root: &Path, hidden: bool, no_ignore: bool) -> Vec<PathBuf> {
        let roots = [root.to_path_buf()];
        let (mut builder, filters) = walker(&roots, !hidden, !no_ignore, &Override::empty());
        let (tx, rx) = mpsc::channel();
        builder.build_parallel().run(|| {
            let tx = tx.clone();
            let mut filters = filters.clone();
            Box::new(move |entry| {
                let Ok(entry) = entry else {
                    return WalkState::Continue;
                };
                match filters.as_mut().and_then(|filters| filters.judge(&entry)) {
                    Some(Rejection::GitignoredDir(_) | Rejection::HiddenDir(_)) => WalkState::Skip,
                    Some(_) => WalkState::Continue,
                    None => {
                        if is_candidate(&entry) {
                            tx.send(display_path(entry.path()))
                                .expect("the receiver outlives the walk");
                        }
                        WalkState::Continue
                    },
                }
            })
        });
        drop(tx);
        let parallel = sorted(rx.into_iter().collect());

        judge_as_predicate(&mut builder, filters);
        let sequential = sorted(
            builder
                .build()
                .filter_map(Result::ok)
                .filter(is_candidate)
                .map(|entry| display_path(entry.path()))
                .collect(),
        );
        assert_eq!(parallel, sequential, "parallel against sequential");
        sequential
    }

    /// `ignore`'s own walker with its own filters on: the set `find` searched before it applied
    /// the filters itself.
    fn files_the_standard_walker_yields(
        root: &Path,
        hidden: bool,
        no_ignore: bool,
    ) -> Vec<PathBuf> {
        sorted(
            WalkBuilder::new(root)
                .hidden(!hidden)
                .ignore(!no_ignore)
                .git_ignore(!no_ignore)
                .git_global(!no_ignore)
                .git_exclude(!no_ignore)
                .filter_entry(|entry| !in_git_dir(entry.path()))
                .build()
                .filter_map(Result::ok)
                .filter(is_candidate)
                .map(|entry| display_path(entry.path()))
                .collect(),
        )
    }

    /// `None` when `rg` is not installed.
    fn files_rg_lists(root: &Path, hidden: bool, no_ignore: bool) -> Option<Vec<PathBuf>> {
        let mut rg = std::process::Command::new("rg");
        rg.arg("--files");
        if hidden {
            rg.arg("--hidden");
        }
        if no_ignore {
            rg.arg("--no-ignore");
        }
        let run = rg.arg(root).output().ok()?;
        let listed = String::from_utf8(run.stdout).expect("UTF-8 fixture paths");
        Some(sorted(
            listed
                .lines()
                .map(|line| display_path(Path::new(line)))
                .filter(|path| !in_git_dir(path))
                .collect(),
        ))
    }

    fn assert_the_walk_matches_the_standard_walker_and_rg(root: &Path, test: &str) {
        for (hidden, no_ignore) in FLAG_PAIRS {
            let flags = format!("--hidden {hidden}, --no-ignore {no_ignore}");
            let ours = files_find_walks(root, hidden, no_ignore);
            assert_eq!(
                ours,
                files_the_standard_walker_yields(root, hidden, no_ignore),
                "{flags}"
            );
            match files_rg_lists(root, hidden, no_ignore) {
                Some(rg) => assert_eq!(ours, rg, "rg --files, {flags}"),
                None => eprintln!("skipped ({test}, rg arm): rg absent"),
            }
        }
    }

    /// Each file holds `needle`, so `find needle --files` lists every file the search opened.
    fn layered_ignore_rules(dir: &Workdir) {
        for sub in ["sub", "sub/deeper", "build", "secret", ".hidden"] {
            std::fs::create_dir(dir.path().join(sub)).expect("a nested directory");
        }
        for (name, text) in [
            (".gitignore", "*.log\nbuild/\n!.env\n"),
            ("sub/.gitignore", "!keep.log\n*.tmp\n"),
            (".ignore", "secret/\n"),
        ] {
            write(dir, name, &format!("{text}# needle\n"));
        }
        for name in [
            "a.txt",
            "top.log",
            ".env",
            ".config.toml",
            "sub/code.rs",
            "sub/keep.log",
            "sub/x.tmp",
            "sub/deeper/y.tmp",
            "sub/deeper/z.log",
            "sub/deeper/z.rs",
            "build/out.txt",
            "secret/s.txt",
            ".hidden/h.txt",
        ] {
            write(dir, name, "needle\n");
        }
    }

    #[test]
    fn nested_gitignore_negation_ignore_file_and_hidden_directory_walk_as_rg_does() {
        let Some(dir) = repo() else { return };
        layered_ignore_rules(&dir);

        assert_the_walk_matches_the_standard_walker_and_rg(
            Path::new("."),
            "nested_gitignore_negation_ignore_file_and_hidden_directory_walk_as_rg_does",
        );
        let expected: Vec<PathBuf> = [
            ".env",
            "a.txt",
            "sub/code.rs",
            "sub/deeper/z.rs",
            "sub/keep.log",
        ]
        .map(PathBuf::from)
        .to_vec();
        assert_eq!(
            files_find_walks(Path::new("."), false, false),
            expected,
            "!.env re-includes a dotfile; sub/'s !keep.log outranks the root's *.log, but not \
             for sub/deeper/z.log"
        );
    }

    #[test]
    fn find_files_on_layered_ignore_rules_lists_what_rg_files_lists() {
        let Some(dir) = repo() else { return };
        layered_ignore_rules(&dir);
        let mut args = find_args("needle", &[]);
        args.files = true;
        let Some(rg) = files_rg_lists(Path::new("."), false, false) else {
            eprintln!(
                "skipped (find_files_on_layered_ignore_rules_lists_what_rg_files_lists): rg absent"
            );
            return;
        };

        for traversal in [Traversal::Sequential, Traversal::Parallel { threads: 4 }] {
            let outcome = run_with(&args, &global_args(), Some(traversal));

            let Body::Files(files) = &outcome.response.body else {
                panic!("expected Body::Files");
            };
            assert_eq!(files, &rg, "{traversal:?}");
            assert_eq!(
                named(&outcome),
                [
                    "ignored 8 (gitignore 4 \u{b7} hidden 4) \u{b7} ignored dirs (gitignore build/, \
                     secret/ \u{b7} hidden .hidden/)"
                ],
                "{traversal:?}"
            );
        }
    }

    #[test]
    fn the_generated_corpus_walks_as_rg_does() {
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/corpus");
        if !corpus.join(".manifest").is_file() {
            eprintln!(
                "skipped (the_generated_corpus_walks_as_rg_does): no generated corpus; run `cargo \
                 run --manifest-path tests/corpusgen/Cargo.toml --release`"
            );
            return;
        }
        assert_the_walk_matches_the_standard_walker_and_rg(
            &corpus,
            "the_generated_corpus_walks_as_rg_does",
        );
    }

    /// `computeFee` is lines 3-6; the `total` method calling it is lines 9-15.
    const FEE_TS: &str = "import { rates } from './rates'\n\
                          \n\
                          export function computeFee(amount: number): number {\n\
                          \x20 const rate = rates.base\n\
                          \x20 return amount * rate\n\
                          }\n\
                          \n\
                          export class Cart {\n\
                          \x20 total(items: number[]): number {\n\
                          \x20   let sum = 0\n\
                          \x20   for (const item of items) {\n\
                          \x20     sum += computeFee(item)\n\
                          \x20   }\n\
                          \x20   return sum\n\
                          \x20 }\n\
                          }\n\
                          \n\
                          export function other() {\n\
                          \x20 return 1\n\
                          }\n";

    fn only_block(outcome: &Outcome) -> &TargetBlock {
        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        let [block] = blocks.as_slice() else {
            panic!("expected one block, got {blocks:?}");
        };
        block
    }

    fn hit_numbers(block: &TargetBlock) -> Vec<usize> {
        block
            .lines
            .iter()
            .filter(|line| line.marker == Marker::Hit)
            .map(|line| line.number)
            .collect()
    }

    #[test]
    fn two_hits_in_a_typescript_file_print_each_enclosing_function_and_name_the_expansion() {
        let Some(dir) = repo() else { return };
        std::fs::create_dir(dir.path().join("src")).expect("a src dir");
        write(&dir, "src/fee.ts", FEE_TS);

        let outcome = run_find(&find_args("computeFee", &["src"]), &global_args());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let block = only_block(&outcome);
        assert_eq!(line_numbers(block), [3, 4, 5, 6, 9, 10, 11, 12, 13, 14, 15]);
        assert_eq!(hit_numbers(block), [3, 12]);
        assert_eq!(block.lines[1].text, "  const rate = rates.base");
        assert_eq!(block.lines[1].marker, Marker::Context);
        assert_eq!(named(&outcome), ["expanded 2 hits to enclosing symbols"]);
        assert_eq!(
            outcome.response.footer.summary,
            "2 hits in 1 file \u{b7} searched 1 file"
        );
    }

    #[test]
    fn a_name_defined_twice_expands_each_hit_to_the_definition_holding_it() {
        let Some(dir) = repo() else { return };
        write(
            &dir,
            "open.ts",
            "export function open(): number {\n  return needle(1)\n}\n\nexport class Store {\n  \
             open(): number {\n    return needle(2)\n  }\n}\n",
        );

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        assert_eq!(line_numbers(only_block(&outcome)), [1, 2, 3, 6, 7, 8]);
        assert_eq!(named(&outcome), ["expanded 2 hits to enclosing symbols"]);
    }

    #[test]
    fn no_expand_prints_the_same_two_hits_bare() {
        let Some(dir) = repo() else { return };
        std::fs::create_dir(dir.path().join("src")).expect("a src dir");
        write(&dir, "src/fee.ts", FEE_TS);
        let args = FindArgs {
            no_expand: true,
            ..find_args("computeFee", &["src"])
        };

        let outcome = run_find(&args, &global_args());

        assert_eq!(line_numbers(only_block(&outcome)), [3, 12]);
        assert!(named(&outcome).is_empty(), "{:?}", named(&outcome));
    }

    #[test]
    fn a_context_flag_keeps_its_own_window_and_nothing_is_expanded() {
        let Some(dir) = repo() else { return };
        std::fs::create_dir(dir.path().join("src")).expect("a src dir");
        write(&dir, "src/fee.ts", FEE_TS);
        let mut args = find_args("computeFee", &["src"]);
        args.context = Some(2);

        let outcome = run_find(&args, &global_args());

        assert_eq!(line_numbers(only_block(&outcome)), [
            1, 2, 3, 4, 5, 10, 11, 12, 13, 14
        ]);
        assert!(named(&outcome).is_empty(), "{:?}", named(&outcome));
    }

    #[test]
    fn json_output_carries_the_hits_alone() {
        let Some(dir) = repo() else { return };
        std::fs::create_dir(dir.path().join("src")).expect("a src dir");
        write(&dir, "src/fee.ts", FEE_TS);
        let mut global = global_args();
        global.json = true;

        let outcome = run_find(&find_args("computeFee", &["src"]), &global);

        assert_eq!(line_numbers(only_block(&outcome)), [3, 12]);
        assert!(named(&outcome).is_empty(), "{:?}", named(&outcome));
    }

    #[test]
    fn eleven_hits_print_hit_lines_only_and_ten_are_expanded() {
        let Some(dir) = repo() else { return };
        let spaced = |hits: usize| -> String {
            (1..=hits).fold(String::new(), |mut text, n| {
                writeln!(text, "needle {n}\nfiller\nfiller\nfiller").expect("a String write");
                text
            })
        };
        write(&dir, "a.txt", &spaced(11));

        let eleven = run_find(&find_args("needle", &[]), &global_args());

        let block = only_block(&eleven);
        assert_eq!(line_numbers(block), hit_numbers(block));
        assert_eq!(block.lines.len(), 11);
        assert!(named(&eleven).is_empty(), "{:?}", named(&eleven));

        write(&dir, "a.txt", &spaced(10));
        let ten = run_find(&find_args("needle", &[]), &global_args());

        assert_eq!(hit_numbers(only_block(&ten)).len(), 10);
        assert!(only_block(&ten).lines.len() > 10);
        assert_eq!(named(&ten)[0], "expanded 10 hits to \u{b1}5 lines");
    }

    #[test]
    fn a_hit_in_a_fourth_file_leaves_every_file_bare_and_three_files_expand() {
        let Some(dir) = repo() else { return };
        for name in ["a.txt", "b.txt", "c.txt"] {
            write(&dir, name, "above\nneedle\nbelow\n");
        }

        let three = run_find(&find_args("needle", &[]), &global_args());
        assert_eq!(named(&three), ["expanded 3 hits to \u{b1}5 lines"]);

        write(&dir, "d.txt", "above\nneedle\nbelow\n");
        let four = run_find(&find_args("needle", &[]), &global_args());

        let Body::Targets(blocks) = &four.response.body else {
            panic!("expected Body::Targets");
        };
        assert_eq!(blocks.len(), 4);
        for block in blocks {
            assert_eq!(line_numbers(block), [2], "{}", block.target);
        }
        assert!(named(&four).is_empty(), "{:?}", named(&four));
    }

    /// A function of `lines` lines, `needle` on its middle line.
    fn one_function(lines: usize) -> String {
        let mut text = "export function big(x: number): number {\n".to_owned();
        for n in 2..lines {
            let word = if n == lines / 2 { "needle" } else { "filler" };
            writeln!(text, "  const {word}{n} = x + {n}").expect("a String write");
        }
        text.push_str("}\n");
        text
    }

    #[test]
    fn a_symbol_of_forty_one_lines_falls_back_to_five_lines_either_side() {
        let Some(dir) = repo() else { return };
        write(&dir, "big.ts", &one_function(41));

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        assert_eq!(
            line_numbers(only_block(&outcome)),
            (15..=25).collect::<Vec<_>>()
        );
        assert_eq!(named(&outcome), ["expanded 1 hit to \u{b1}5 lines"]);
    }

    #[test]
    fn a_symbol_of_forty_lines_prints_whole() {
        let Some(dir) = repo() else { return };
        write(&dir, "big.ts", &one_function(40));

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        assert_eq!(
            line_numbers(only_block(&outcome)),
            (1..=40).collect::<Vec<_>>()
        );
        assert_eq!(named(&outcome), ["expanded 1 hit to enclosing symbols"]);
    }

    #[test]
    fn a_file_with_no_grammar_gets_five_lines_either_side_clamped_to_the_file() {
        let Some(dir) = repo() else { return };
        let text = (1..=20).fold(String::new(), |mut text, n| {
            let word = if n == 2 { "needle" } else { "line" };
            writeln!(text, "{word} {n}").expect("a String write");
            text
        });
        write(&dir, "notes.txt", &text);

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        assert_eq!(line_numbers(only_block(&outcome)), [1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(hit_numbers(only_block(&outcome)), [2]);
    }

    /// Ten twelve-line functions, `needle` on the second line of each.
    fn ten_functions() -> String {
        let mut text = String::new();
        for f in 0..10 {
            writeln!(text, "export function f{f}(x: number): number {{").expect("a String write");
            writeln!(text, "  const needle = x + {f}").expect("a String write");
            for n in 0..9 {
                writeln!(text, "  x = x + {n}").expect("a String write");
            }
            text.push_str("}\n\n");
        }
        text
    }

    #[test]
    fn ten_hits_in_ten_symbols_expand_to_at_most_eighty_lines_and_name_the_rest() {
        let Some(dir) = repo() else { return };
        write(&dir, "ten.ts", &ten_functions());

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        // Six whole functions are 72 lines. The seventh (79-90) would make 84, but its hit's
        // window, 75-85, adds only 78-85 past the sixth: the eight lines left. The last three
        // hits then fit nothing.
        let block = only_block(&outcome);
        assert_eq!(hit_numbers(block), [
            2, 15, 28, 41, 54, 67, 80, 93, 106, 119
        ]);
        let shown: Vec<usize> = (1..=77)
            .filter(|line| line % 13 != 0)
            .chain(78..=85)
            .chain([93, 106, 119])
            .collect();
        assert_eq!(line_numbers(block), shown);
        assert_eq!(named(&outcome), [
            "expanded 6 hits to enclosing symbols \u{b7} 1 hit to \u{b1}5 lines \u{b7} 3 hits not \
             expanded (80-line cap)"
        ]);
    }

    #[test]
    fn with_room_for_every_symbol_no_hit_falls_back_or_stays_bare() {
        let Some(dir) = repo() else { return };
        let six: String =
            ten_functions()
                .lines()
                .take(6 * 13)
                .fold(String::new(), |mut text, line| {
                    writeln!(text, "{line}").expect("a String write");
                    text
                });
        write(&dir, "six.ts", &six);

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        assert_eq!(only_block(&outcome).lines.len(), 6 * 12);
        assert_eq!(named(&outcome), ["expanded 6 hits to enclosing symbols"]);
    }

    #[test]
    fn a_markdown_hit_prints_its_section_and_a_fenced_hash_is_not_a_heading() {
        let Some(dir) = repo() else { return };
        write(
            &dir,
            "doc.md",
            "# Title\n\nintro\n\n## Setup\n\n```sh\n# not a heading\nrun needle\n```\n\n## Next\n\ntail\n",
        );

        // Named, not walked: a machine-wide gitignore can hold `*.md`.
        let outcome = run_find(&find_args("needle", &["doc.md"]), &global_args());

        assert_eq!(
            line_numbers(only_block(&outcome)),
            (5..=11).collect::<Vec<_>>()
        );
        assert_eq!(named(&outcome), ["expanded 1 hit to enclosing symbols"]);
    }

    #[test]
    fn a_non_utf8_line_the_expansion_adds_is_named_as_shown_lossily() {
        let Some(dir) = repo() else { return };
        std::fs::write(dir.path().join("l.txt"), b"caf\xe9\nneedle\nplain\n").expect("a fixture");

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        let block = only_block(&outcome);
        assert_eq!(line_numbers(block), [1, 2, 3]);
        assert_eq!(block.lines[0].text, "caf\u{fffd}");
        assert_eq!(named(&outcome), [
            "expanded 1 hit to \u{b1}5 lines",
            "1 non-UTF-8 line shown lossily"
        ]);
    }

    #[test]
    fn over_the_cap_with_context_the_preview_holds_hit_lines_only() {
        let Some(dir) = repo() else { return };
        write(&dir, "many.txt", &many_hits(55));
        let mut args = find_args("needle", &[]);
        args.context = Some(1);

        let outcome = run_find(&args, &global_args());

        assert!(matches!(
            outcome.error,
            Some(Error::OverCap { hits: 55, .. })
        ));
        let block = only_block(&outcome);
        assert_eq!(line_numbers(block), (1..=10).collect::<Vec<_>>());
        assert_eq!(hit_numbers(block), line_numbers(block));
    }

    /// `[`×`depth`, a hit, then `]`×`depth`, inside a three-line `outer` pair when `keyed`.
    fn deep_json(depth: usize, keyed: bool) -> String {
        let (open, close) = ("[".repeat(depth), "]".repeat(depth));
        if keyed {
            format!("{{\"outer\": {open}\n\"needle\"\n{close}\n}}\n")
        } else {
            format!("{open}\n\"needle\"\n{close}\n")
        }
    }

    #[test]
    fn a_hit_twenty_thousand_arrays_deep_finds_the_pair_above_them_in_linear_time() {
        let Some(dir) = repo() else { return };
        write(&dir, "deep.json", &deep_json(20_000, true));

        let started = std::time::Instant::now();
        let outcome = run_find(&find_args("needle", &["deep.json"]), &global_args());

        // Climbing one `Node::parent` at a time took 25 s here in a release build.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        // The pair runs from its key to the last `]`; the window would add the object's `}` too.
        assert_eq!(line_numbers(only_block(&outcome)), [1, 2, 3]);
        assert_eq!(named(&outcome)[0], "expanded 1 hit to enclosing symbols");
    }

    #[test]
    fn the_same_depth_with_no_pair_above_gets_five_lines_either_side() {
        let Some(dir) = repo() else { return };
        write(&dir, "deep.json", &deep_json(20_000, false));

        let outcome = run_find(&find_args("needle", &["deep.json"]), &global_args());

        // Every node spans the file's 3 lines and none is a definition: the window is the file.
        assert_eq!(line_numbers(only_block(&outcome)), [1, 2, 3]);
        assert_eq!(named(&outcome)[0], "expanded 1 hit to \u{b1}5 lines");
    }

    /// `bytes` long: expression statements, then a 12-line function holding `needle` on its 2nd
    /// line.
    fn padded_function(bytes: usize) -> String {
        let function = "export function f(x: number): number {\n  const needle = x\n".to_owned()
            + &"  x = x + 1\n".repeat(9)
            + "}\n";
        let filler = "00000000;\n";
        let mut text = String::new();
        while text.len() + function.len() + filler.len() + 3 <= bytes {
            text.push_str(filler);
        }
        text.push_str(&"0".repeat(bytes - text.len() - function.len() - 2));
        text.push_str(";\n");
        text + &function
    }

    #[test]
    fn a_file_one_byte_over_the_parse_ceiling_gets_five_lines_and_the_footer_says_why() {
        let Some(dir) = repo() else { return };
        let text = padded_function(EXPAND_PARSE_MAX_BYTES + 1);
        assert_eq!(text.len(), EXPAND_PARSE_MAX_BYTES + 1);
        write(&dir, "big.ts", &text);
        let total = text.lines().count();

        let outcome = run_find(&find_args("needle", &["big.ts"]), &global_args());

        let hit = total - 10;
        assert_eq!(hit_numbers(only_block(&outcome)), [hit]);
        assert_eq!(
            line_numbers(only_block(&outcome)),
            (hit - 5..=hit + 5).collect::<Vec<_>>()
        );
        assert_eq!(named(&outcome), [
            "expanded 1 hit to \u{b1}5 lines (1 not parsed: file over 128 KiB)"
        ]);
    }

    #[test]
    fn a_file_at_the_parse_ceiling_is_parsed_and_its_hit_prints_the_function() {
        let Some(dir) = repo() else { return };
        let text = padded_function(EXPAND_PARSE_MAX_BYTES);
        assert_eq!(text.len(), EXPAND_PARSE_MAX_BYTES);
        write(&dir, "big.ts", &text);
        let total = text.lines().count();

        let outcome = run_find(&find_args("needle", &["big.ts"]), &global_args());

        assert_eq!(
            line_numbers(only_block(&outcome)),
            (total - 11..=total).collect::<Vec<_>>()
        );
        assert_eq!(named(&outcome), ["expanded 1 hit to enclosing symbols"]);
    }

    /// Two 31-line functions, a hit on the second line of each: lines 2 and 34.
    fn two_functions() -> String {
        let mut text = String::new();
        for f in 0..2 {
            writeln!(text, "fn f{f}() {{").expect("a String write");
            writeln!(
                text,
                "    let needle_{f} = \"padding padding padding padding padding\";"
            )
            .expect("a String write");
            for n in 0..28 {
                writeln!(
                    text,
                    "    let v{n} = \"{n} padding padding padding padding padding padding\";"
                )
                .expect("a String write");
            }
            text.push_str("}\n\n");
        }
        text
    }

    fn with_budget(budget: usize) -> Global {
        Global {
            budget: Some(budget),
            ..global_args()
        }
    }

    #[test]
    fn a_budget_with_room_for_the_hits_alone_keeps_every_hit_and_names_both_unexpanded() {
        let Some(dir) = repo() else { return };
        write(&dir, "two.rs", &two_functions());

        let outcome = run_find(&find_args("needle_", &[]), &with_budget(60));

        // 240 bytes: the two hit lines take 132, and each hit's window adds about 340 more, so
        // nothing but the hits fits and nothing is trimmed.
        let block = only_block(&outcome);
        assert_eq!(line_numbers(block), [2, 34]);
        assert_eq!(hit_numbers(block), [2, 34]);
        assert_eq!(named(&outcome), ["2 hits not expanded (budget 60)"]);
    }

    #[test]
    fn a_budget_with_room_for_one_function_expands_the_first_and_gives_the_second_its_window() {
        let Some(dir) = repo() else { return };
        write(&dir, "two.rs", &two_functions());

        let outcome = run_find(&find_args("needle_", &[]), &with_budget(700));

        // 2,800 bytes: the hits (132) and the first function (1,896) fit, the second function
        // does not, and its window, 29-39, adds 32-39 past the first: 341 bytes.
        let block = only_block(&outcome);
        assert_eq!(hit_numbers(block), [2, 34]);
        assert_eq!(line_numbers(block), (1..=39).collect::<Vec<_>>());
        assert!(window::content_bytes(&block.lines) <= 700 * window::BYTES_PER_TOKEN);
        assert_eq!(named(&outcome), [
            "expanded 1 hit to enclosing symbols \u{b7} 1 hit to \u{b1}5 lines"
        ]);
    }

    #[test]
    fn a_budget_with_room_for_both_functions_expands_both_and_trims_nothing() {
        let Some(dir) = repo() else { return };
        write(&dir, "two.rs", &two_functions());

        let outcome = run_find(&find_args("needle_", &[]), &with_budget(2000));

        let block = only_block(&outcome);
        assert_eq!(
            line_numbers(block),
            (1..=31).chain(33..=63).collect::<Vec<_>>()
        );
        assert_eq!(named(&outcome), ["expanded 2 hits to enclosing symbols"]);
    }

    #[test]
    fn max_bytes_with_room_for_the_hits_alone_names_them_unexpanded() {
        let Some(dir) = repo() else { return };
        write(&dir, "two.rs", &two_functions());
        let global = Global {
            max_bytes: 300,
            ..global_args()
        };

        let outcome = run_find(&find_args("needle_", &[]), &global);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(line_numbers(only_block(&outcome)), [2, 34]);
        assert_eq!(named(&outcome), ["2 hits not expanded (max-bytes 300)"]);
    }

    #[test]
    fn the_default_max_bytes_leaves_room_to_expand_both_functions() {
        let Some(dir) = repo() else { return };
        write(&dir, "two.rs", &two_functions());

        let outcome = run_find(&find_args("needle_", &[]), &global_args());

        assert_eq!(only_block(&outcome).lines.len(), 62);
        assert_eq!(named(&outcome), ["expanded 2 hits to enclosing symbols"]);
    }

    fn preview_of(cap: usize, global: &Global) -> (Option<Vec<usize>>, Vec<String>) {
        let mut args = find_args("needle", &[]);
        args.cap = cap;
        let outcome = run_find(&args, global);
        assert!(matches!(outcome.error, Some(Error::OverCap { .. })));
        let lines = match &outcome.response.body {
            Body::Targets(blocks) => match blocks.as_slice() {
                [block] => Some(line_numbers(block)),
                _ => None,
            },
            _ => None,
        };
        (lines, named(&outcome))
    }

    #[test]
    fn a_cap_under_ten_previews_no_more_hits_than_the_cap() {
        let Some(dir) = repo() else { return };
        write(&dir, "n.txt", &many_hits(30));

        let (lines, named) = preview_of(5, &global_args());

        assert_eq!(lines, Some(vec![1, 2, 3, 4, 5]));
        assert!(named.contains(&"first 5 of 30 hits in the busiest file shown".to_owned()));
    }

    #[test]
    fn a_cap_of_zero_previews_nothing_and_names_no_preview() {
        let Some(dir) = repo() else { return };
        write(&dir, "n.txt", &many_hits(30));

        let (lines, named) = preview_of(0, &global_args());

        assert_eq!(lines, None);
        assert!(
            !named.iter().any(|named| named.contains("busiest file")),
            "{named:?}"
        );
    }

    #[test]
    fn a_cap_over_ten_previews_ten() {
        let Some(dir) = repo() else { return };
        write(&dir, "n.txt", &many_hits(30));

        let (lines, named) = preview_of(20, &global_args());

        assert_eq!(lines, Some((1..=10).collect()));
        assert!(named.contains(&"first 10 of 30 hits in the busiest file shown".to_owned()));
    }

    fn twenty_long_hits(dir: &Workdir) {
        for name in ["a", "b", "c"] {
            let text = (0..20).fold(String::new(), |mut text, n| {
                writeln!(text, "needle {name} {n} {}", "x".repeat(50)).expect("a String write");
                text
            });
            write(dir, &format!("{name}.txt"), &text);
        }
        write(dir, "z.txt", "needle z\nneedle z\nneedle z\n");
    }

    #[test]
    fn a_budget_that_trims_the_preview_names_the_lines_it_kept() {
        let Some(dir) = repo() else { return };
        twenty_long_hits(&dir);

        let (lines, named) = preview_of(50, &with_budget(30));

        // 120 bytes hold one 65-byte preview line, not two.
        assert_eq!(lines, Some(vec![1]));
        assert!(
            named.contains(&"first 1 of 20 hits in the busiest file shown".to_owned()),
            "{named:?}"
        );
    }

    #[test]
    fn with_no_budget_the_same_preview_names_all_ten() {
        let Some(dir) = repo() else { return };
        twenty_long_hits(&dir);

        let (lines, named) = preview_of(50, &global_args());

        assert_eq!(lines, Some((1..=10).collect()));
        assert!(named.contains(&"first 10 of 20 hits in the busiest file shown".to_owned()));
    }

    #[test]
    fn a_hit_in_a_one_line_function_gets_five_lines_either_side() {
        let Some(dir) = repo() else { return };
        write(
            &dir,
            "one.rs",
            "use a;\nuse b;\n\nfn one() -> u32 { NEEDLE }\n\nfn two() {\n    3\n}\n",
        );

        let outcome = run_find(&find_args("NEEDLE", &["one.rs"]), &global_args());

        assert_eq!(
            line_numbers(only_block(&outcome)),
            (1..=8).collect::<Vec<_>>()
        );
        assert_eq!(named(&outcome), ["expanded 1 hit to \u{b1}5 lines"]);
    }

    #[test]
    fn a_leaf_key_in_yaml_toml_and_json_gets_five_lines_either_side() {
        let Some(dir) = repo() else { return };
        let above = "a1: 1\na2: 2\na3: 3\na4: 3\na5: 5\na6: 6\n";
        write(&dir, "c.yaml", &format!("{above}x: NEEDLE\n{above}"));
        write(
            &dir,
            "c.toml",
            &format!(
                "[package]\n{}version = \"NEEDLE\"\n",
                above.replace(':', " =")
            ),
        );
        write(
            &dir,
            "c.json",
            "{\n\"a\": 1,\n\"b\": 2,\n\"c\": 3,\n\"x\": \"NEEDLE\",\n\"d\": 4\n}\n",
        );

        let outcome = run_find(&find_args("NEEDLE", &[]), &global_args());

        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        let shown: Vec<(&str, Vec<usize>)> = blocks
            .iter()
            .map(|block| (block.target.as_str(), line_numbers(block)))
            .collect();
        assert_eq!(shown, [
            ("c.json", (1..=7).collect::<Vec<_>>()),
            ("c.toml", (3..=8).collect()),
            ("c.yaml", (2..=12).collect()),
        ]);
        assert_eq!(named(&outcome), ["expanded 3 hits to \u{b1}5 lines"]);
    }

    #[test]
    fn a_three_line_function_prints_whole_and_a_two_line_one_gets_its_window() {
        let Some(dir) = repo() else { return };
        let pad = "0;\n1;\n2;\n3;\n4;\n5;\n";
        write(
            &dir,
            "three.ts",
            &format!("{pad}function f() {{\n  needle()\n}}\n{pad}"),
        );
        write(
            &dir,
            "two.ts",
            &format!("{pad}function g() {{ needle()\n}}\n{pad}"),
        );

        let outcome = run_find(&find_args("needle", &[]), &global_args());

        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        assert_eq!(line_numbers(&blocks[0]), [7, 8, 9], "{}", blocks[0].target);
        assert_eq!(
            line_numbers(&blocks[1]),
            (2..=12).collect::<Vec<_>>(),
            "{}",
            blocks[1].target
        );
        assert_eq!(named(&outcome), [
            "expanded 1 hit to enclosing symbols \u{b7} 1 hit to \u{b1}5 lines"
        ]);
    }

    #[test]
    fn a_toml_table_expands_to_its_own_lines_and_not_the_next_header() {
        let Some(dir) = repo() else { return };
        write(
            &dir,
            "c.toml",
            "[package]\nname = \"x\"\nversion = \"1\"\n\n[deps]\na = \"1\"\n",
        );

        let outcome = run_find(&find_args("package", &["c.toml"]), &global_args());

        assert_eq!(line_numbers(only_block(&outcome)), [1, 2, 3, 4]);
        assert_eq!(named(&outcome), ["expanded 1 hit to enclosing symbols"]);
    }
}
