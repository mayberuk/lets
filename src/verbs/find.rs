use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::Read;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::overrides::{Override, OverrideBuilder};
use ignore::{DirEntry, WalkBuilder, WalkState};
use regex::{Regex, RegexBuilder};

use crate::cli::{FindArgs, Global};
use crate::error::Error;
use crate::hook::bre;
use crate::output::{Body, CountRow, Footer, Line, Marker, Omission, Response, Stats, TargetBlock};
use crate::{Outcome, fs, window};

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
    let builder = walker(&search_paths, !args.hidden, !global.no_ignore, &overrides);
    let traversal = traversal.unwrap_or_else(|| traversal_of(&search_paths));
    // The membership walks need nothing from the search, so they run beside it.
    let pools = std::thread::scope(|scope| {
        let pools = Pools::spawn(scope, args, global, &search_paths, &overrides);
        match traversal {
            Traversal::Sequential => search.sequential(builder, &mut walk, &mut found),
            Traversal::Parallel { threads } => {
                search.parallel(builder, threads, &search_paths, &mut walk, &mut found);
            },
        }
        pools.map(Pools::join)
    });
    let walked = walk_omissions(args, global, &walk, pools.as_ref());
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
        respond_targets(found, &walk, args, global, walked)
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

fn walker(paths: &[PathBuf], hidden: bool, gitignore: bool, globs: &Override) -> WalkBuilder {
    let mut builder = WalkBuilder::new(&paths[0]);
    for path in &paths[1..] {
        builder.add(path);
    }
    builder
        // As with `rg -g`, a matching glob outranks gitignore and the dotfile rule for that file.
        .overrides(globs.clone())
        .hidden(hidden)
        .ignore(gitignore)
        .git_ignore(gitignore)
        .git_global(gitignore)
        .git_exclude(gitignore)
        // `ignore` never applies this predicate to a root entry, so every consumer re-tests it.
        .filter_entry(|entry| !in_git_dir(entry.path()));
    builder
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
        line.text = wrapped;
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
        text,
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

fn respond_targets(
    found: Found,
    walk: &Walk,
    args: &FindArgs,
    global: &Global,
    walked: Vec<Omission>,
) -> Outcome {
    let Found {
        total_hits,
        long_lines_cut,
        mut blocks,
        file_hits,
        ..
    } = found;
    let matched_files = blocks.len();
    let searched = walk.searched;
    let mut response = Response::empty("find");

    if total_hits > args.cap {
        let mut top_files = file_hits;
        top_files.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.path.cmp(&b.path)));
        let shown = top_files.len().min(TOP_FILES_SHOWN);
        let more = top_files.len() - shown;
        top_files.truncate(shown);

        let mut omitted = vec![Omission::HitCap {
            hits: total_hits,
            cap: args.cap,
        }];
        omitted.extend(walked);
        if !top_files.is_empty() {
            omitted.push(Omission::TopFiles { shown });
        }
        response.omitted = omitted;
        response.top_files = top_files;
        response.top_files_more = more;
        response.footer = Footer {
            summary: hit_summary(total_hits, matched_files, searched),
        };
        return Outcome::partial(response, Error::OverCap {
            hits: total_hits,
            files: matched_files,
            cap: args.cap,
        });
    }

    if let Some(budget) = global.budget {
        response.omitted = window::trim_to_budget(&mut blocks, budget);
    }
    if long_lines_cut > 0 {
        response.omitted.push(Omission::LongLinesCut {
            lines: long_lines_cut,
        });
    }
    let lossy_lines: usize = blocks
        .iter()
        .filter_map(|block| {
            let lossy = walk.lossy.get(&block.path)?;
            Some(
                block
                    .lines
                    .iter()
                    .filter(|line| lossy.binary_search(&line.number).is_ok())
                    .count(),
            )
        })
        .sum();
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

/// `reached` holds every file the walk yielded, read or not, so a file it refused is never also
/// charged to a filter.
#[derive(Default)]
struct Walk {
    reached: usize,
    searched: usize,
    binary: usize,
    too_large: usize,
    unreadable: usize,
    unreadable_dirs: Vec<PathBuf>,
    lossy: BTreeMap<PathBuf, Vec<usize>>,
}

impl Walk {
    /// Rows are the only order-dependent state, so callers hand files over in render order.
    fn record(&mut self, found: &mut Found, args: &FindArgs, path: &Path, outcome: FileOutcome) {
        let (lossy_lines, hits, mut lines, first_match) = match outcome {
            FileOutcome::Skipped(skip) => {
                match skip {
                    Skip::Binary => self.binary += 1,
                    Skip::TooLarge => self.too_large += 1,
                    Skip::Unreadable => self.unreadable += 1,
                }
                self.reached += 1;
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
        self.reached += 1;
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
            found.long_lines_cut += window::cut_long_lines(&mut lines, &first_match);
            // Kept so an over-cap response can map the busiest files without a second walk.
            found.file_hits.push(CountRow {
                count: hits,
                path: display.clone(),
            });
            found.blocks.push(hit_block(display, lines));
        }
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
    long_lines_cut: usize,
    blocks: Vec<TargetBlock>,
    files: Vec<PathBuf>,
    counts: Vec<CountRow>,
    /// Default mode only: the over-cap map, built when `blocks` goes unused.
    file_hits: Vec<CountRow>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Traversal {
    Sequential,
    /// `threads` of 0 lets `ignore` choose: the available cores, at most 12.
    Parallel {
        threads: usize,
    },
}

/// A file root is one read, so any file root keeps the sequential walk, which needs no sort.
fn traversal_of(roots: &[PathBuf]) -> Traversal {
    if roots.iter().all(|root| root.is_dir()) {
        Traversal::Parallel { threads: 0 }
    } else {
        Traversal::Sequential
    }
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
    fn sequential(&self, mut builder: WalkBuilder, walk: &mut Walk, found: &mut Found) {
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
    }

    /// Workers finish in scheduler order, so every file is collected and sorted into the sequential
    /// walk's order before any is recorded.
    fn parallel(
        &self,
        mut builder: WalkBuilder,
        threads: usize,
        roots: &[PathBuf],
        walk: &mut Walk,
        found: &mut Found,
    ) {
        let (tx, rx) = mpsc::channel();
        builder.threads(threads).build_parallel().run(|| {
            let tx = tx.clone();
            let mut engine = build_searcher(self.args);
            Box::new(move |entry| {
                let visit = match entry {
                    Err(err) => Visit::Unlistable(error_path(&err)),
                    Ok(entry) if is_candidate(&entry) => {
                        let outcome = self.file(entry.path(), &mut engine);
                        Visit::File(entry.into_path(), outcome)
                    },
                    Ok(_) => return WalkState::Continue,
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

fn walk_omissions(
    args: &FindArgs,
    global: &Global,
    walk: &Walk,
    pools: Option<&Pools<usize>>,
) -> Vec<Omission> {
    let mut walked = Vec::new();
    if !args.globs.is_empty() {
        walked.push(Omission::Glob {
            patterns: args.globs.clone(),
        });
    }
    walked.extend(pools.and_then(|pools| ignored_omission(args, global, walk, pools)));
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

/// The file sets the ignored buckets are differences against; the gitignore-only walk runs only
/// when both filters are active.
struct Pools<T> {
    pool: T,
    gitignore_only: Option<T>,
}

type Walking<'scope> = std::thread::ScopedJoinHandle<'scope, usize>;

impl<'scope> Pools<Walking<'scope>> {
    fn spawn<'env>(
        scope: &'scope std::thread::Scope<'scope, 'env>,
        args: &FindArgs,
        global: &Global,
        paths: &'env [PathBuf],
        globs: &'env Override,
    ) -> Option<Self> {
        if args.hidden && global.no_ignore {
            return None;
        }
        // The glob narrows the pool too: a file it excluded was not left out by a filter.
        let pool = scope.spawn(|| count_files(&walker(paths, false, false, globs)));
        let gitignore_only = (!args.hidden && !global.no_ignore)
            .then(|| scope.spawn(|| count_files(&walker(paths, false, true, globs))));
        Some(Pools {
            pool,
            gitignore_only,
        })
    }

    fn join(self) -> Pools<usize> {
        let join = |handle: Walking<'scope>| {
            handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        };
        Pools {
            pool: join(self.pool),
            gitignore_only: self.gitignore_only.map(join),
        }
    }
}

/// `ignore` applies the dotfile rule only where gitignore said nothing, so a file both walks
/// dropped is gitignore's, and a `!` line re-including a dotfile keeps it out of `hidden`.
fn ignored_omission(
    args: &FindArgs,
    global: &Global,
    walk: &Walk,
    pools: &Pools<usize>,
) -> Option<Omission> {
    // Every filtered walk yields a subset of the unfiltered one, so set differences are counts.
    // `saturating_sub` only masks a violation of that invariant instead of underflowing, so the
    // debug build still catches one.
    debug_assert!(
        walk.reached <= pools.pool,
        "the unfiltered pool ({}) undercounted the filtered walk ({})",
        pools.pool,
        walk.reached
    );
    let excluded = pools.pool.saturating_sub(walk.reached);

    let (gitignore, hidden) = if args.hidden {
        (excluded, 0)
    } else if global.no_ignore {
        (0, excluded)
    } else {
        let gitignore_only = pools
            .gitignore_only
            .expect("spawned whenever neither --hidden nor --no-ignore is set");
        debug_assert!(
            walk.reached <= gitignore_only,
            "the gitignore-only pool ({}) undercounted the filtered walk ({})",
            gitignore_only,
            walk.reached
        );
        let hidden = gitignore_only.saturating_sub(walk.reached);
        (excluded - hidden.min(excluded), hidden.min(excluded))
    };
    // A reached file that could not be searched is named in `Skipped`, so `other` stays zero.
    (gitignore + hidden > 0).then_some(Omission::Ignored {
        gitignore,
        hidden,
        other: 0,
    })
}

fn count_files(builder: &WalkBuilder) -> usize {
    let files = std::sync::atomic::AtomicUsize::new(0);
    builder.clone().threads(0).build_parallel().run(|| {
        Box::new(|entry| {
            if let Ok(entry) = entry
                && is_candidate(&entry)
            {
                files.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            WalkState::Continue
        })
    });
    files.into_inner()
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
    fn hits_exceeding_the_cap_print_no_content_and_name_the_true_counts() {
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
        assert!(blocks.is_empty(), "no hit content over the cap");
        assert_eq!(
            outcome.response.footer.summary,
            "55 hits in 1 file · searched 1 file"
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
        assert_eq!(
            rendered,
            "10\tj.txt\n\
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
             50-hit cap \u{b7} narrow the pattern or the paths, or --files \u{b7} top 10 files \
             shown\n"
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
        assert_eq!(
            rendered,
            "3\tz.txt\n\
             2\ta.txt\n\
             1\tm.txt\n\
             \u{2500}\u{2500} 6 hits in 3 files \u{b7} searched 3 files \u{b7} over the 2-hit \
             cap \u{b7} narrow the pattern or the paths, or --files \u{b7} top 3 files shown\n"
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
        assert!(blocks.is_empty());
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
        let texts: Vec<&str> = blocks[0]
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect();
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

        let outcome = run_find(&find_args("target_fn|let s", &[]), &global_args());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let Body::Targets(blocks) = &outcome.response.body else {
            panic!("expected Body::Targets");
        };
        let texts: Vec<(usize, &str)> = blocks[0]
            .lines
            .iter()
            .map(|line| (line.number, line.text.as_str()))
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

        let outcome = run_find(&find_args("target_fn|let s", &[]), &global_args());

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

        let outcome = run_find(&find_args("needle", &[]), &global_args());

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
    fn a_file_under_a_gitignored_directory_is_still_counted_as_gitignore_while_a_glob_is_active() {
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
        assert_eq!(named(&outcome), ["glob *.log", "ignored 1 (gitignore 1)"]);
        assert_eq!(
            outcome.response.footer.summary,
            "1 hit in 1 file \u{b7} searched 1 file"
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

    #[test]
    fn roots_that_are_all_directories_take_the_parallel_walk() {
        let root = tempfile::TempDir::new().expect("temp dir");
        std::fs::create_dir(root.path().join("src")).expect("a nested directory");
        let roots = [root.path().to_path_buf(), root.path().join("src")];

        assert_eq!(traversal_of(&roots), Traversal::Parallel { threads: 0 });
    }

    #[test]
    fn a_root_that_is_an_explicit_file_keeps_the_walk_sequential() {
        let root = tempfile::TempDir::new().expect("temp dir");
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
}
