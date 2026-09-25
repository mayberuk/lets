//! The footer is a contract: a narrowing it does not name did not happen. Its counts live only in
//! `Response::omitted` and `Response::stats`, so no second count can disagree.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::ser::SerializeStruct as _;
use serde::{Serialize, Serializer};

use crate::error::CANDIDATE_CAP;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
    Jsonl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOptions {
    pub numbers: bool,
    pub quiet: bool,
    pub cost_first: bool,
}

#[derive(Debug, Serialize)]
pub struct Response {
    #[serde(skip)]
    pub verb: &'static str,
    #[serde(flatten)]
    pub body: Body,
    #[serde(skip)]
    pub footer: Footer,
    pub omitted: Vec<Omission>,
    pub stats: Stats,
    /// `find` over the hit cap: the busiest files, so the bare total is never all a caller learns.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub top_files: Vec<CountRow>,
    #[serde(skip_serializing_if = "is_zero")]
    pub top_files_more: usize,
}

impl Response {
    pub fn empty(verb: &'static str) -> Response {
        Response {
            verb,
            body: Body::Targets(Vec::new()),
            footer: Footer {
                summary: String::new(),
            },
            omitted: Vec::new(),
            stats: Stats::new(0, 0),
            top_files: Vec::new(),
            top_files_more: 0,
        }
    }

    /// A summary alone counts: `find` over the hit cap has no body but still owes its count.
    pub fn has_output(&self) -> bool {
        let body = match &self.body {
            Body::Targets(blocks) => !blocks.is_empty(),
            Body::Files(paths) => !paths.is_empty(),
            Body::Counts(rows) => !rows.is_empty(),
            Body::Raw { text, .. } => !text.is_empty(),
            Body::Edit(results) => !results.is_empty(),
            Body::Write(_) | Body::Stats(_) | Body::Update(_) => true,
            Body::Transform(results) => !results.is_empty(),
        };
        body || !self.footer.summary.is_empty()
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Body {
    Targets(Vec<TargetBlock>),
    Files(#[serde(serialize_with = "lossy_paths")] Vec<PathBuf>),
    Counts(Vec<CountRow>),
    Raw { field: &'static str, text: String },
    Edit(Vec<EditResult>),
    Write(WriteResult),
    Transform(Vec<TransformResult>),
    Stats(StatsReport),
    Update(UpdateCheck),
}

/// Both versions normalised to `major.minor.patch[-pre]`, so equal strings are equal versions.
#[derive(Debug, Serialize)]
pub struct UpdateCheck {
    pub current: String,
    pub latest: String,
    pub update_available: bool,
}

#[derive(Debug, Serialize)]
pub struct TargetBlock {
    pub target: String,
    #[serde(serialize_with = "lossy_path")]
    pub path: PathBuf,
    /// `None` for a `find` hit block, whose header is the bare path.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub span: Option<Span>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_shown: Option<(usize, usize)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolver: Option<Resolver>,
    /// `show` renders `\r\n` as a plain break, so only this tells a reader `--old` spans CRLF.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub crlf: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lossy_lines: Vec<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<Sha12>,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub total: usize,
}

/// Counts only, never command text, a path or file content: a transcript can hold any of them.
#[derive(Debug, Serialize)]
pub struct StatsReport {
    pub sessions: usize,
    pub bash_calls: usize,
    pub lets_calls: BTreeMap<String, usize>,
    pub hook_blocks: usize,
    pub blocks_followed: usize,
    pub calls_saved: usize,
    pub read_calls: usize,
    pub read_bytes: u64,
    #[serde(skip_serializing_if = "StatsSkips::is_empty")]
    pub skipped: StatsSkips,
}

#[derive(Debug, Default, Serialize)]
pub struct StatsSkips {
    #[serde(skip_serializing_if = "is_zero")]
    pub malformed_lines: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub non_utf8_lines: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub unreadable_files: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub walk_errors: usize,
}

impl StatsSkips {
    pub fn is_empty(&self) -> bool {
        self.phrases().next().is_none()
    }

    fn phrases(&self) -> impl Iterator<Item = String> {
        [
            (self.malformed_lines, "malformed line"),
            (self.non_utf8_lines, "non-UTF-8 line"),
            (self.unreadable_files, "unreadable file"),
            (self.walk_errors, "walk error"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, kind)| format!("{n} {kind}{}", if n == 1 { "" } else { "s" }))
    }
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes &T"
)]
fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Debug, Serialize)]
pub struct CountRow {
    pub count: usize,
    #[serde(serialize_with = "lossy_path")]
    pub path: PathBuf,
}

/// `serde_json` refuses a non-UTF-8 `PathBuf`, so JSON carries the lossy name the text shows.
/// A path `find` walked to can be any bytes; one from an argument was already UTF-8.
fn lossy_path<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(&path.display())
}

fn lossy_paths<S: Serializer>(paths: &[PathBuf], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(paths.iter().map(|path| path.display().to_string()))
}

#[derive(Debug, Serialize)]
pub struct EditResult {
    pub path: PathBuf,
    #[serde(flatten)]
    pub kind: EditKind,
    pub lines: Vec<usize>,
    #[serde(rename = "match")]
    pub match_kind: String,
    /// Set only when a plaintext `#name`'s end was a guess, under the key `show` uses for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolver: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<Region>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check: Option<CheckResult>,
    pub sha: ShaPair,
    /// The whole edit still renders, while the file on disk is the one the edit started from.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub reverted: bool,
}

/// Untagged so a replacement's `--json` keeps `"replacements":1` as a top-level key.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum EditKind {
    Replaced {
        #[serde(rename = "replacements")]
        count: usize,
    },
    Inserted {
        #[serde(rename = "inserted")]
        lines: usize,
        anchor: Anchor,
    },
}

/// `line` is printed only when `at`, the anchor as the caller named it, is not already a line.
#[derive(Debug, Serialize)]
pub struct Anchor {
    pub side: AnchorSide,
    pub at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorSide {
    After,
    Before,
}

impl fmt::Display for AnchorSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AnchorSide::After => "after",
            AnchorSide::Before => "before",
        })
    }
}

impl fmt::Display for Anchor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.side, self.at)?;
        match self.line {
            Some(line) => write!(f, " (line {line})"),
            None => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransformFormat {
    Json,
    Yaml,
    Toml,
    Frontmatter,
}

impl fmt::Display for TransformFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TransformFormat::Json => "json",
            TransformFormat::Yaml => "yaml",
            TransformFormat::Toml => "toml",
            TransformFormat::Frontmatter => "frontmatter",
        })
    }
}

/// Never grouped: `--json` carries one object per op, and `set a, b` is text-only.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TransformOp {
    Set { key: String },
    Delete { key: String },
    Append { key: String },
}

#[derive(Debug, Serialize)]
pub struct TransformResult {
    pub path: PathBuf,
    pub format: TransformFormat,
    pub operations: Vec<TransformOp>,
    pub lines: Vec<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<Region>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check: Option<CheckResult>,
    pub sha: ShaPair,
}

#[derive(Debug, Serialize)]
pub struct Region {
    pub start: usize,
    pub end: usize,
    pub lines: Vec<Line>,
}

#[derive(Debug, Serialize)]
pub struct ShaPair {
    pub before: Sha12,
    pub after: Sha12,
}

#[derive(Debug, Serialize)]
pub struct CheckResult {
    pub layer: String,
    pub status: String,
    pub errors_before: u32,
    pub errors_after: u32,
}

#[derive(Debug, Serialize)]
pub struct WriteResult {
    pub path: PathBuf,
    pub outcome: WriteOutcome,
    pub lines: usize,
    pub bytes: usize,
    pub sha: Sha12,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check: Option<CheckResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteOutcome {
    Created,
    /// The prior line count and hash are the only record of what `--force` destroyed.
    Overwritten {
        prior_lines: usize,
        prior_sha: Sha12,
    },
    Exists,
}

#[derive(Debug, Clone, Serialize)]
pub struct Line {
    pub number: usize,
    pub marker: Marker,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Marker {
    None,
    Added,
    Replaced,
    Hit,
    Context,
    /// Stands for lines the block does not carry, so it takes no line number of its own.
    Gap,
    Deleted,
}

impl Marker {
    fn glyph(self) -> char {
        match self {
            Marker::None => ' ',
            Marker::Added => '+',
            Marker::Replaced => '~',
            Marker::Hit => ':',
            Marker::Context | Marker::Deleted => '-',
            Marker::Gap => '\u{b7}',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolver {
    TreeSitter,
    Heuristic(&'static str),
}

impl fmt::Display for Resolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Resolver::TreeSitter => f.write_str("tree-sitter"),
            Resolver::Heuristic(detail) => write!(f, "heuristic ({detail})"),
        }
    }
}

impl Serialize for Resolver {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Sha12(String);

impl Sha12 {
    /// 12 hex is the floor: 4 hex is 65k values, and a stale-guard that collides is not a guard.
    pub fn parse(hex: &str) -> Option<Sha12> {
        if hex.len() < 12 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return None;
        }
        Some(Sha12(hex[..12].to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

const DEFAULT_TOKEN_RATIO: usize = 4;

#[derive(Debug, Clone, Copy)]
pub struct Stats {
    pub lines: usize,
    pub bytes: usize,
    /// Bytes per estimated token; `None` drops the estimate.
    pub token_ratio: Option<usize>,
}

impl Stats {
    pub fn new(lines: usize, bytes: usize) -> Stats {
        Stats {
            lines,
            bytes,
            token_ratio: Some(DEFAULT_TOKEN_RATIO),
        }
    }

    /// Zero bytes is no cost at all rather than `~0 tokens`.
    pub fn tokens_est(&self) -> Option<usize> {
        match (self.bytes, self.token_ratio) {
            (0, _) | (_, None) => None,
            (bytes, Some(ratio)) => Some(bytes / ratio),
        }
    }
}

impl Serialize for Stats {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut stats = serializer.serialize_struct("Stats", 3)?;
        stats.serialize_field("lines", &self.lines)?;
        stats.serialize_field("bytes", &self.bytes)?;
        stats.serialize_field("tokens_est", &self.tokens_est())?;
        stats.end()
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Omission {
    Window {
        shown: (usize, usize),
        total: usize,
    },
    /// `not_shown` is the range the budget cut, not one a window hid; `None` for a `find` block.
    Budget {
        budget: usize,
        trimmed_target: String,
        not_shown: Option<(usize, usize)>,
    },
    HitCap {
        hits: usize,
        cap: usize,
    },
    /// Separate from `HitCap`, which stderr renders alone; this reaches only the stdout footer.
    TopFiles {
        shown: usize,
    },
    /// Over the hit cap, the busiest file's first hits are printed, so the bare total is not all
    /// a caller sees.
    BusiestFile {
        shown: usize,
        hits: usize,
    },
    Expanded(ExpandedHits),
    /// The counts are files a filter rejected directly. A rejected directory is named instead,
    /// never entered, so the files under it are counted nowhere.
    Ignored {
        gitignore: usize,
        hidden: usize,
        other: usize,
        dirs: IgnoredDirs,
    },
    CheckSkipped {
        reason: String,
    },
    CheckInconclusive {
        layer: String,
        reason: String,
    },
    XattrsDropped {
        path: PathBuf,
    },
    Normalized,
    RegionGap {
        not_shown: Vec<(usize, usize)>,
    },
    PartialBatch {
        written: Vec<PathBuf>,
    },
    /// `error` is the slug the target's stderr line carries.
    Unresolved {
        target: String,
        error: &'static str,
    },
    LongLinesCut {
        lines: usize,
    },
    OutputTrimmed {
        limit: usize,
        lines: usize,
    },
    Skipped {
        binary: usize,
        too_large: usize,
        unreadable: usize,
    },
    LossyLines {
        lines: usize,
    },
    Unreadable {
        #[serde(serialize_with = "lossy_path")]
        path: PathBuf,
    },
    CrlfMatched,
    SelectorResolved {
        selector: String,
        resolved: String,
    },
    Glob {
        patterns: Vec<String>,
    },
    /// A regex that matched nothing as written, searched again the way grep reads it.
    GrepStyle {
        pattern: String,
        read_as: String,
    },
}

/// `find` hits printed with their enclosing symbol or `around` lines either side; `unexpanded`
/// hits stayed bare once the expanded lines reached `line_cap`.
#[derive(Debug, Serialize)]
pub struct ExpandedHits {
    pub symbols: usize,
    pub windows: usize,
    pub around: usize,
    pub unexpanded: usize,
    pub line_cap: usize,
}

/// A zero part is left out, as in `write_parts`.
impl fmt::Display for ExpandedHits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts = [
            (self.symbols, "to enclosing symbols".to_owned()),
            (self.windows, format!("to \u{b1}{} lines", self.around)),
            (
                self.unexpanded,
                format!("not expanded ({}-line cap)", self.line_cap),
            ),
        ];
        let mut open = false;
        for (n, what) in parts {
            if n == 0 {
                continue;
            }
            f.write_str(if open { " \u{b7} " } else { "expanded " })?;
            write!(f, "{n} hit{} {what}", plural_suffix(n))?;
            open = true;
        }
        Ok(())
    }
}

/// Per source, because the source is what says whether `--no-ignore` or `--hidden` brings a
/// directory back. A name is a lossy string, as a failed target's is: `serde_json` refuses a
/// non-UTF-8 `PathBuf`.
#[derive(Debug, Default, Serialize)]
pub struct IgnoredDirs {
    pub gitignore: NamedDirs,
    pub hidden: NamedDirs,
}

#[derive(Debug, Default, Serialize)]
pub struct NamedDirs {
    pub named: Vec<String>,
    pub more: usize,
}

impl fmt::Display for Omission {
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per omission keeps every footer phrase in one place"
    )]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Omission::Window { shown, total } => write!(f, ":{}-{total} not shown", shown.1 + 1),
            Omission::Budget {
                budget,
                trimmed_target,
                not_shown,
            } => {
                write!(f, "budget {budget} trimmed {trimmed_target}")?;
                match not_shown {
                    Some((from, to)) => write!(f, " (:{from}-{to} not shown)"),
                    None => Ok(()),
                }
            },
            Omission::HitCap { cap, .. } => write!(
                f,
                "over the {cap}-hit cap \u{b7} narrow the pattern or the paths, or --files"
            ),
            Omission::TopFiles { shown } => {
                write!(f, "top {shown} file{} shown", plural_suffix(*shown))
            },
            Omission::BusiestFile { shown, hits } => {
                let s = plural_suffix(*hits);
                write!(
                    f,
                    "first {shown} of {hits} hit{s} in the busiest file shown"
                )
            },
            Omission::Expanded(expanded) => expanded.fmt(f),
            Omission::Ignored {
                gitignore,
                hidden,
                other,
                dirs,
            } => {
                let files = [
                    ("gitignore", *gitignore),
                    ("hidden", *hidden),
                    ("other", *other),
                ];
                write_ignored(f, files, dirs)
            },
            Omission::CheckSkipped { reason } => write!(f, "check: skipped ({reason})"),
            Omission::CheckInconclusive { layer, reason } => {
                write!(f, "check: {layer} inconclusive ({reason})")
            },
            Omission::XattrsDropped { .. } => f.write_str("xattrs dropped"),
            Omission::Normalized => f.write_str("normalized"),
            Omission::RegionGap { not_shown } => {
                for (i, (from, to)) in not_shown.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" \u{b7} ")?;
                    }
                    write!(f, ":{from}-{to} not shown")?;
                }
                Ok(())
            },
            Omission::PartialBatch { written } => {
                write!(
                    f,
                    "wrote {} file{}",
                    written.len(),
                    plural_suffix(written.len())
                )?;
                for (i, path) in written.iter().enumerate() {
                    f.write_str(if i == 0 { ": " } else { ", " })?;
                    write!(f, "{}", path.display())?;
                }
                Ok(())
            },
            Omission::Unresolved { target, error } => write!(f, "{target} failed ({error})"),
            Omission::LongLinesCut { lines } => {
                write!(f, "{lines} long line{} cut", plural_suffix(*lines))
            },
            Omission::OutputTrimmed { limit, lines } => write!(
                f,
                "output over --max-bytes {limit}: {lines} line{} not shown",
                plural_suffix(*lines)
            ),
            Omission::Skipped {
                binary,
                too_large,
                unreadable,
            } => write_parts(f, "skipped", [
                ("binary", *binary),
                ("too large", *too_large),
                ("unreadable", *unreadable),
            ]),
            Omission::LossyLines { lines } => {
                write!(
                    f,
                    "{lines} non-UTF-8 line{} shown lossily",
                    plural_suffix(*lines)
                )
            },
            Omission::Unreadable { path } => write!(f, "{} unreadable", path.display()),
            Omission::CrlfMatched => f.write_str("--old matched as CRLF"),
            Omission::SelectorResolved { selector, resolved } => {
                write!(f, "{selector} \u{2192} {resolved}")
            },
            Omission::Glob { patterns } => write!(f, "glob {}", patterns.join(", ")),
            Omission::GrepStyle { pattern, read_as } => write!(
                f,
                "\u{ab}{pattern}\u{bb} had no hits, read grep-style as \u{ab}{read_as}\u{bb}"
            ),
        }
    }
}

/// A file count of zero is left out, as a zero part is, and so is a source that pruned nothing.
fn write_ignored(
    f: &mut fmt::Formatter<'_>,
    files: [(&str, usize); 3],
    dirs: &IgnoredDirs,
) -> fmt::Result {
    let counted = files.iter().any(|(_, n)| *n > 0);
    if counted {
        write_parts(f, "ignored", files)?;
    }
    let sources = [("gitignore", &dirs.gitignore), ("hidden", &dirs.hidden)];
    let mut open = false;
    for (source, list) in sources {
        if list.named.is_empty() && list.more == 0 {
            continue;
        }
        f.write_str(match (open, counted) {
            (true, _) => " \u{b7} ",
            (false, true) => " \u{b7} ignored dirs (",
            (false, false) => "ignored dirs (",
        })?;
        open = true;
        f.write_str(source)?;
        for (i, dir) in list.named.iter().enumerate() {
            f.write_str(if i == 0 { " " } else { ", " })?;
            write_escaped(f, dir)?;
            f.write_str("/")?;
        }
        if list.more > 0 {
            write!(f, " (+{} more)", list.more)?;
        }
    }
    if open { f.write_str(")") } else { Ok(()) }
}

/// A directory name can hold a newline, which would end the one-line footer early.
fn write_escaped(f: &mut fmt::Formatter<'_>, name: &str) -> fmt::Result {
    for c in name.chars() {
        if c.is_control() {
            write!(f, "{}", c.escape_debug())?;
        } else {
            f.write_char(c)?;
        }
    }
    Ok(())
}

/// A zero part is left out: naming a source that removed nothing reads as a narrowing.
fn write_parts(f: &mut fmt::Formatter<'_>, label: &str, parts: [(&str, usize); 3]) -> fmt::Result {
    write!(f, "{label} {}", parts.iter().map(|(_, n)| n).sum::<usize>())?;
    let mut open = false;
    for (name, n) in parts {
        if n == 0 {
            continue;
        }
        f.write_str(if open { " · " } else { " (" })?;
        write!(f, "{name} {n}")?;
        open = true;
    }
    if open { f.write_str(")") } else { Ok(()) }
}

#[derive(Debug)]
pub struct Footer {
    pub summary: String,
}

pub fn render(resp: &Response, format: Format, opts: &RenderOptions) -> String {
    match format {
        Format::Text => render_text(resp, *opts),
        Format::Json => render_json(resp),
        Format::Jsonl => render_jsonl(resp),
    }
}

pub fn write_stdout(rendered: &str) -> std::io::Result<()> {
    emit(&mut std::io::stdout().lock(), rendered)
}

pub fn write_error(message: &str, slug: &str) -> std::io::Result<()> {
    emit(&mut std::io::stderr().lock(), &error_text(message, slug))
}

/// `LETS_TOKEN_RATIO` overrides ÷ 4 because a tokenizer change moves the true ratio by ~30%.
pub fn token_ratio(no_stats: Option<&OsStr>, ratio: Option<&OsStr>) -> Option<usize> {
    if no_stats.is_some_and(|v| v == "1") {
        return None;
    }
    let ratio = ratio
        .and_then(OsStr::to_str)
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|r| *r > 0)
        .unwrap_or(DEFAULT_TOKEN_RATIO);
    Some(ratio)
}

fn emit(sink: &mut impl std::io::Write, text: &str) -> std::io::Result<()> {
    sink.write_all(text.as_bytes())?;
    sink.flush()
}

fn error_text(message: &str, slug: &str) -> String {
    format!("{message}\nERROR_CODE={slug}\n")
}

fn render_text(resp: &Response, opts: RenderOptions) -> String {
    let mut out = String::with_capacity(capacity_hint(resp));
    let width = number_width(resp);
    let cost = resp.stats.tokens_est().map(cost_segment);
    match &resp.body {
        Body::Raw { text, .. } => out.push_str(text),
        Body::Targets(blocks) => {
            // `show --all` ignores the budget, so the bill comes before the body.
            if opts.cost_first
                && let Some(cost) = &cost
            {
                writeln!(out, "── {cost}").unwrap();
            }
            for block in blocks {
                push_target_header(&mut out, block);
                push_lines(&mut out, &block.lines, width, opts);
            }
            if !opts.quiet {
                push_top_files(&mut out, &resp.top_files, resp.top_files_more);
            }
            push_footer(
                &mut out,
                &[&resp.footer.summary],
                &resp.omitted,
                cost.as_deref(),
            );
        },
        Body::Files(paths) => {
            if !opts.quiet {
                for path in paths {
                    writeln!(out, "{}", path.display()).unwrap();
                }
            }
            push_footer(
                &mut out,
                &[&resp.footer.summary],
                &resp.omitted,
                cost.as_deref(),
            );
        },
        Body::Counts(rows) => {
            push_footer(
                &mut out,
                &[&resp.footer.summary],
                &resp.omitted,
                cost.as_deref(),
            );
            push_count_rows(&mut out, rows, opts);
        },
        Body::Edit(results) => {
            push_edit(&mut out, resp, results, width, opts, cost.as_deref());
        },
        Body::Write(result) => push_write(&mut out, result, &resp.omitted),
        Body::Transform(results) => {
            push_transform(&mut out, resp, results, width, opts, cost.as_deref());
        },
        Body::Stats(report) => push_stats(&mut out, report),
        Body::Update(check) => push_update(&mut out, check),
    }
    out
}

fn push_target_header(out: &mut String, block: &TargetBlock) {
    write!(out, "── {}", block.target).unwrap();
    if let Some(span) = &block.span {
        let (start, end, total) = (span.start, span.end, span.total);
        write!(out, "  ({start}-{end} of {total}").unwrap();
        if let Some(window) = block.window {
            write!(out, " · window {window}").unwrap();
        }
        if let Some((from, to)) = block.not_shown {
            write!(out, " · :{from}-{to} not shown").unwrap();
        }
        if let Some(resolver) = &block.resolver {
            write!(out, " · via {resolver}").unwrap();
        }
        out.push(')');
    }
    if block.crlf {
        out.push_str(" · crlf");
    }
    if !block.lossy_lines.is_empty() {
        push_lossy_lines(out, &block.lossy_lines);
    }
    if let Some(sha) = &block.sha {
        write!(out, " · sha:{}", sha.as_str()).unwrap();
    }
    out.push('\n');
}

/// Enough to go and look; a mostly Latin-1 file would otherwise list every line.
const LOSSY_LINES_LISTED: usize = 5;

fn push_lossy_lines(out: &mut String, lines: &[usize]) {
    out.push_str(if lines.len() == 1 {
        " · non-UTF-8 line "
    } else {
        " · non-UTF-8 lines "
    });
    for (i, line) in lines.iter().take(LOSSY_LINES_LISTED).enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write!(out, "{line}").unwrap();
    }
    if let Some(more) = lines
        .len()
        .checked_sub(LOSSY_LINES_LISTED)
        .filter(|n| *n > 0)
    {
        write!(out, " (+{more} more)").unwrap();
    }
}

// A tab, not spaces: in a trial a model read a two-space gutter as indentation and copied it into
// `--old`. A tab also matches `cat -n`'s number-then-tab shape.
fn push_lines(out: &mut String, lines: &[Line], width: usize, opts: RenderOptions) {
    if opts.quiet {
        return;
    }
    for line in lines {
        let (marker, text) = (line.marker.glyph(), &line.text);
        if !opts.numbers {
            out.push_str(text);
        } else if line.marker == Marker::Gap {
            write!(out, "{:>width$}{marker}\t{text}", "").unwrap();
        } else {
            push_padded(out, line.number, width);
            out.push(marker);
            out.push('\t');
            out.push_str(text);
        }
        out.push('\n');
    }
}

fn push_padded(out: &mut String, n: usize, width: usize) {
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    let mut rest = n;
    loop {
        at -= 1;
        digits[at] = b'0' + u8::try_from(rest % 10).expect("a decimal digit");
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    let len = digits.len() - at;
    for _ in len..width {
        out.push(' ');
    }
    out.push_str(std::str::from_utf8(&digits[at..]).expect("ASCII digits"));
}

fn push_count_rows(out: &mut String, rows: &[CountRow], opts: RenderOptions) {
    if opts.quiet {
        return;
    }
    let width = rows.iter().map(|r| digits(r.count)).max().unwrap_or(1);
    for row in rows {
        let (count, path) = (row.count, row.path.display());
        writeln!(out, "{count:>width$}  {path}").unwrap();
    }
}

/// A tab, not `--count`'s two spaces, to share the gutter of the rest of the output.
fn push_top_files(out: &mut String, rows: &[CountRow], more: usize) {
    if rows.is_empty() {
        return;
    }
    let width = rows.iter().map(|r| digits(r.count)).max().unwrap_or(1);
    for row in rows {
        let (count, path) = (row.count, row.path.display());
        writeln!(out, "{count:>width$}\t{path}").unwrap();
    }
    if more > 0 {
        writeln!(out, "\u{2026} {more} more file{}", plural_suffix(more)).unwrap();
    }
}

fn push_edit(
    out: &mut String,
    resp: &Response,
    results: &[EditResult],
    width: usize,
    opts: RenderOptions,
    cost: Option<&str>,
) {
    if !opts.quiet {
        for result in results {
            push_edit_header(out, result);
            if let Some(region) = &result.region {
                push_lines(out, &region.lines, width, opts);
            }
        }
    }
    // A batch has no single check to report; the verb composes the aggregate into the summary.
    if let [only] = results {
        let check = only.check.as_ref().map(check_line).unwrap_or_default();
        // After a revert, the same hash twice would read as a change that did not happen.
        let sha = if only.sha.before == only.sha.after {
            format!("sha:{}", only.sha.before.as_str())
        } else {
            format!(
                "sha:{}→{}",
                only.sha.before.as_str(),
                only.sha.after.as_str()
            )
        };
        let unchanged = if only.reverted { "file unchanged" } else { "" };
        push_footer(out, &[&check, unchanged, &sha], &resp.omitted, cost);
    } else {
        push_footer(out, &[&resp.footer.summary], &resp.omitted, cost);
    }
}

/// A failure has no `layer`, so it prints the verdict alone: `check: failed → reverted`.
fn check_line(check: &CheckResult) -> String {
    if check.layer.is_empty() {
        format!("check: {}", check.status)
    } else {
        format!("check: {} {}", check.layer, check.status)
    }
}

/// `edit` sizes its cost estimate with this, so the estimate and the header cannot disagree.
pub fn push_edit_header(out: &mut String, result: &EditResult) {
    write!(out, "── {}", result.path.display()).unwrap();
    match &result.kind {
        EditKind::Replaced { count } => {
            write!(out, " · {count} replacement{}", plural_suffix(*count)).unwrap();
            push_line_runs(out, &result.lines);
        },
        EditKind::Inserted { lines, anchor } => {
            write!(
                out,
                " · inserted {lines} line{} {anchor}",
                plural_suffix(*lines)
            )
            .unwrap();
        },
    }
    if !result.match_kind.is_empty() {
        write!(out, " · {}", result.match_kind).unwrap();
    }
    if let Some(resolver) = result.resolver {
        write!(out, " · {resolver}").unwrap();
    }
    if result.reverted {
        out.push_str(" · REVERTED");
    }
    out.push('\n');
}

/// A pair prints as two numbers, since `2-3` saves nothing over `2, 3`. The list stops at the
/// candidate cap, or `--all` over a whole file would list every line.
fn push_line_runs(out: &mut String, lines: &[usize]) {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for &line in lines {
        match runs.last_mut() {
            Some((_, last)) if line == *last || line == *last + 1 => *last = line,
            _ => runs.push((line, line)),
        }
    }
    let mut entries: Vec<(usize, usize)> = Vec::with_capacity(runs.len());
    for (first, last) in runs {
        if last - first == 1 {
            entries.push((first, first));
            entries.push((last, last));
        } else {
            entries.push((first, last));
        }
    }
    let Some(&(first, last)) = entries.first() else {
        return;
    };
    out.push_str(if entries.len() == 1 && first == last {
        " · line "
    } else {
        " · lines "
    });
    for (i, (first, last)) in entries.iter().take(CANDIDATE_CAP).enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        if first == last {
            write!(out, "{first}").unwrap();
        } else {
            write!(out, "{first}-{last}").unwrap();
        }
    }
    let more: usize = entries
        .iter()
        .skip(CANDIDATE_CAP)
        .map(|(first, last)| last - first + 1)
        .sum();
    if more > 0 {
        write!(out, " (+{more} more)").unwrap();
    }
}

fn push_transform(
    out: &mut String,
    resp: &Response,
    results: &[TransformResult],
    width: usize,
    opts: RenderOptions,
    cost: Option<&str>,
) {
    if !opts.quiet {
        for result in results {
            push_transform_header(out, result);
            if let Some(region) = &result.region {
                push_lines(out, &region.lines, width, opts);
            }
        }
    }
    // A partial batch can leave one result, whose summary names the file that failed.
    if let [only] = results {
        let check = only.check.as_ref().map(check_line).unwrap_or_default();
        let sha = format!(
            "sha:{}\u{2192}{}",
            only.sha.before.as_str(),
            only.sha.after.as_str()
        );
        push_footer(
            out,
            &[&resp.footer.summary, &check, &sha],
            &resp.omitted,
            cost,
        );
    } else {
        push_footer(out, &[&resp.footer.summary], &resp.omitted, cost);
    }
}

/// Kinds render in the order each first appeared on the command line, not a fixed one.
fn push_transform_header(out: &mut String, result: &TransformResult) {
    write!(out, "── {} · {}", result.path.display(), result.format).unwrap();
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    for op in &result.operations {
        let (verb, key) = match op {
            TransformOp::Set { key } => ("set", key),
            TransformOp::Delete { key } => ("delete", key),
            TransformOp::Append { key } => ("append", key),
        };
        match groups.iter_mut().find(|(seen, _)| *seen == verb) {
            Some((_, keys)) => keys.push(key),
            None => groups.push((verb, vec![key])),
        }
    }
    for (i, (verb, keys)) in groups.iter().enumerate() {
        out.push_str(if i == 0 { " · " } else { ", " });
        write!(out, "{verb} {}", keys.join(", ")).unwrap();
    }
    let lines: BTreeSet<usize> = result.lines.iter().copied().collect();
    if !lines.is_empty() {
        out.push_str(if lines.len() == 1 {
            " · line "
        } else {
            " · lines "
        });
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            write!(out, "{line}").unwrap();
        }
    }
    out.push('\n');
}

fn push_write(out: &mut String, result: &WriteResult, omitted: &[Omission]) {
    let (path, lines, sha) = (result.path.display(), result.lines, result.sha.as_str());
    match &result.outcome {
        // The refusal is a non-zero exit, not an answer, so it carries no footer and no cost.
        WriteOutcome::Exists => {
            let suffix = plural_suffix(lines);
            writeln!(
                out,
                "── {path} exists ({lines} line{suffix}, sha:{sha}) · pass --force to overwrite"
            )
            .unwrap();
        },
        WriteOutcome::Created | WriteOutcome::Overwritten { .. } => {
            let (suffix, bytes) = (plural_suffix(lines), result.bytes);
            // Both hashes, as `edit` prints them: one would hide that the file had two states.
            let (verb, hashes) = match &result.outcome {
                WriteOutcome::Overwritten { prior_sha, .. } => {
                    ("overwritten", format!("{}→{sha}", prior_sha.as_str()))
                },
                _ => ("created", sha.to_owned()),
            };
            writeln!(
                out,
                "── {path} · {verb} · {lines} line{suffix} · {bytes} bytes · sha:{hashes}"
            )
            .unwrap();
            if let Some(check) = &result.check {
                writeln!(out, "── {}", check_line(check)).unwrap();
            }
            push_footer(out, &[], omitted, None);
        },
    }
}

fn push_update(out: &mut String, check: &UpdateCheck) {
    let UpdateCheck {
        current, latest, ..
    } = check;
    if check.update_available {
        writeln!(out, "lets {current} \u{b7} latest {latest}").unwrap();
    } else if current == latest {
        writeln!(out, "lets {current} is the latest release").unwrap();
    } else {
        writeln!(
            out,
            "lets {current} is newer than the latest release, {latest}"
        )
        .unwrap();
    }
}

/// No header, and a footer only when the scan skipped something: the rows are the counts.
fn push_stats(out: &mut String, report: &StatsReport) {
    let lets_total: usize = report.lets_calls.values().sum();
    let mut rows: Vec<(&str, &str, u64)> = Vec::with_capacity(8 + report.lets_calls.len());
    rows.push(("", "sessions", report.sessions as u64));
    rows.push(("", "bash calls", report.bash_calls as u64));
    rows.push(("", "lets calls", lets_total as u64));
    for (verb, n) in &report.lets_calls {
        rows.push(("  ", verb, *n as u64));
    }
    rows.push(("", "hook blocks", report.hook_blocks as u64));
    rows.push(("", "blocks followed", report.blocks_followed as u64));
    rows.push(("", "calls saved", report.calls_saved as u64));
    rows.push(("", "read calls", report.read_calls as u64));
    rows.push(("", "read bytes", report.read_bytes));

    let label_width = rows
        .iter()
        .map(|(indent, label, _)| indent.len() + label.len())
        .max()
        .unwrap_or(0);
    let count_width = rows
        .iter()
        .map(|(_, _, n)| n.checked_ilog10().map_or(1, |d| d as usize + 1))
        .max()
        .unwrap_or(1);
    for (indent, label, n) in rows {
        let width = label_width - indent.len();
        writeln!(out, "{indent}{label:<width$}  {n:>count_width$}").unwrap();
    }
    if !report.skipped.is_empty() {
        let phrases: Vec<String> = report.skipped.phrases().collect();
        writeln!(out, "── skipped: {}", phrases.join(", ")).unwrap();
    }
}

fn push_footer(out: &mut String, lead: &[&str], omitted: &[Omission], cost: Option<&str>) {
    let lead = lead.iter().filter(|s| !s.is_empty());
    if lead.clone().count() == 0 && omitted.is_empty() && cost.is_none() {
        return;
    }
    out.push_str("── ");
    let mut first = true;
    for segment in lead {
        push_segment(out, &mut first);
        out.push_str(segment);
    }
    for omission in omitted {
        push_segment(out, &mut first);
        write!(out, "{omission}").unwrap();
    }
    if let Some(cost) = cost {
        push_segment(out, &mut first);
        out.push_str(cost);
    }
    out.push('\n');
}

fn push_segment(out: &mut String, first: &mut bool) {
    if *first {
        *first = false;
    } else {
        out.push_str(" · ");
    }
}

/// Under 100 tokens a tenth-of-a-thousand reading rounds to `0.0k`, which states nothing.
fn cost_segment(tokens: usize) -> String {
    if tokens < 100 {
        return format!("~{tokens} tokens");
    }
    let tenths = (tokens + 50) / 100;
    format!("~{}.{}k tokens", tenths / 10, tenths % 10)
}

pub fn plural_suffix(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn number_width(resp: &Response) -> usize {
    let widest = match &resp.body {
        Body::Targets(blocks) => blocks
            .iter()
            .flat_map(|b| b.lines.iter())
            .map(|l| l.number)
            .max(),
        Body::Edit(results) => results
            .iter()
            .filter_map(|r| r.region.as_ref())
            .flat_map(|r| r.lines.iter())
            .map(|l| l.number)
            .max(),
        Body::Transform(results) => results
            .iter()
            .flat_map(|r| r.region.iter().flat_map(|g| g.lines.iter()))
            .map(|l| l.number)
            .max(),
        Body::Files(_)
        | Body::Counts(_)
        | Body::Raw { .. }
        | Body::Write(_)
        | Body::Stats(_)
        | Body::Update(_) => None,
    };
    digits(widest.unwrap_or(0))
}

fn digits(n: usize) -> usize {
    let (mut n, mut width) = (n, 1);
    while n >= 10 {
        n /= 10;
        width += 1;
    }
    width
}

fn capacity_hint(resp: &Response) -> usize {
    let lines = match &resp.body {
        Body::Targets(blocks) => blocks.iter().map(|b| b.lines.len()).sum::<usize>(),
        Body::Edit(results) => results
            .iter()
            .map(|r| r.region.as_ref().map_or(0, |g| g.lines.len()))
            .sum(),
        Body::Transform(results) => results
            .iter()
            .flat_map(|r| r.region.iter().flat_map(|g| g.lines.iter()))
            .count(),
        Body::Files(paths) => paths.len(),
        Body::Counts(rows) => rows.len(),
        Body::Raw { text, .. } => return text.len(),
        Body::Write(_) => 0,
        Body::Stats(report) => return (10 + report.lets_calls.len()) * 40,
        Body::Update(check) => return check.current.len() + check.latest.len() + 48,
    };
    resp.stats.bytes + lines * 8 + 256
}

fn render_json(resp: &Response) -> String {
    let body = match &resp.body {
        Body::Targets(_) | Body::Files(_) | Body::Counts(_) => serde_json::to_string(resp),
        Body::Raw { field, text } => serde_json::to_string(&BTreeMap::from([(*field, text)])),
        Body::Edit(results) => match results.as_slice() {
            [only] => json_with_tail(only, resp),
            many => json_with_tail(&Edits { edits: many }, resp),
        },
        Body::Write(result) => json_with_tail(result, resp),
        Body::Transform(results) => match results.as_slice() {
            [only] => json_with_tail(only, resp),
            many => json_with_tail(&Transforms { transforms: many }, resp),
        },
        Body::Stats(report) => serde_json::to_string(report),
        Body::Update(check) => serde_json::to_string(check),
    };
    let mut out = body.expect("the output model holds no non-string map key and no float");
    out.push('\n');
    out
}

fn json_with_tail<T: Serialize>(inner: &T, resp: &Response) -> serde_json::Result<String> {
    serde_json::to_string(&WithTail {
        inner,
        omitted: &resp.omitted,
        stats: &resp.stats,
    })
}

#[derive(Serialize)]
struct WithTail<'a, T: Serialize> {
    #[serde(flatten)]
    inner: &'a T,
    omitted: &'a [Omission],
    stats: &'a Stats,
}

#[derive(Serialize)]
struct Edits<'a> {
    edits: &'a [EditResult],
}

#[derive(Serialize)]
struct Transforms<'a> {
    transforms: &'a [TransformResult],
}

fn render_jsonl(resp: &Response) -> String {
    let mut out = String::with_capacity(capacity_hint(resp));
    match &resp.body {
        Body::Targets(blocks) => {
            for block in blocks {
                push_json_line(&mut out, block);
            }
        },
        Body::Files(paths) => {
            for path in paths {
                push_json_line(&mut out, &path.display().to_string());
            }
        },
        Body::Counts(rows) => {
            for row in rows {
                push_json_line(&mut out, row);
            }
        },
        Body::Edit(results) => {
            for result in results {
                push_json_line(&mut out, result);
            }
        },
        Body::Transform(results) => {
            for result in results {
                push_json_line(&mut out, result);
            }
        },
        Body::Raw { .. } | Body::Write(_) | Body::Stats(_) | Body::Update(_) => {
            return render_json(resp);
        },
    }
    push_json_line(&mut out, &Tail {
        stats: &resp.stats,
        omitted: &resp.omitted,
        top_files: &resp.top_files,
        top_files_more: resp.top_files_more,
    });
    out
}

pub fn render_error(resp: &Response, slug: &str, message: &str) -> String {
    let mut out = serde_json::to_string(&ErrorJson {
        error: ErrorFields { slug, message },
        omitted: &resp.omitted,
        stats: &resp.stats,
    })
    .expect("the error object holds only strings, the omissions and the stats");
    out.push('\n');
    out
}

#[derive(Serialize)]
struct ErrorJson<'a> {
    error: ErrorFields<'a>,
    omitted: &'a [Omission],
    stats: &'a Stats,
}

#[derive(Serialize)]
struct ErrorFields<'a> {
    slug: &'a str,
    message: &'a str,
}

fn push_json_line<T: Serialize + ?Sized>(out: &mut String, value: &T) {
    out.push_str(&serde_json::to_string(value).expect("the output model serializes"));
    out.push('\n');
}

#[derive(Serialize)]
struct Tail<'a> {
    stats: &'a Stats,
    omitted: &'a [Omission],
    #[serde(skip_serializing_if = "slice_is_empty")]
    top_files: &'a [CountRow],
    #[serde(skip_serializing_if = "is_zero")]
    top_files_more: usize,
}

// `&&[T]`: `skip_serializing_if` passes a reference to the `&[CountRow]` field.
fn slice_is_empty<T>(slice: &&[T]) -> bool {
    slice.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> RenderOptions {
        RenderOptions {
            numbers: true,
            quiet: false,
            cost_first: false,
        }
    }

    fn quiet() -> RenderOptions {
        RenderOptions {
            numbers: true,
            quiet: true,
            cost_first: false,
        }
    }

    fn sha(hex: &str) -> Sha12 {
        Sha12::parse(hex).expect("fixture sha is 12 lowercase hex")
    }

    fn line(number: usize, marker: Marker, text: &str) -> Line {
        Line {
            number,
            marker,
            text: text.to_owned(),
        }
    }

    fn block(target: &str, start: usize, end: usize, total: usize) -> TargetBlock {
        let mut b = bare_block(target);
        b.span = Some(Span { start, end, total });
        b
    }

    fn bare_block(target: &str) -> TargetBlock {
        TargetBlock {
            target: target.to_owned(),
            path: PathBuf::from(target),
            span: None,
            window: None,
            not_shown: None,
            resolver: None,
            crlf: false,
            lossy_lines: Vec::new(),
            sha: None,
            lines: Vec::new(),
        }
    }

    fn response(verb: &'static str, body: Body, bytes: usize) -> Response {
        Response {
            verb,
            body,
            footer: Footer {
                summary: String::new(),
            },
            omitted: Vec::new(),
            stats: Stats::new(0, bytes),
            top_files: Vec::new(),
            top_files_more: 0,
        }
    }

    fn targets(blocks: Vec<TargetBlock>, bytes: usize) -> Response {
        response("show", Body::Targets(blocks), bytes)
    }

    fn first_line(rendered: &str) -> &str {
        rendered
            .lines()
            .next()
            .expect("rendered output has a first line")
    }

    fn keys(value: &serde_json::Value) -> Vec<String> {
        value
            .as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect()
    }

    #[test]
    fn whole_file_header_names_range_total_and_sha() {
        let mut b = block("plugins/example/hooks/hooks.json", 1, 64, 64);
        b.sha = Some(sha("3e9a3e9a3e9a"));
        b.lines = vec![line(1, Marker::None, "{")];
        let out = render(&targets(vec![b], 0), Format::Text, &opts());

        assert_eq!(
            first_line(&out),
            "── plugins/example/hooks/hooks.json  (1-64 of 64) · sha:3e9a3e9a3e9a"
        );
    }

    #[test]
    fn windowed_header_names_the_window_and_the_lines_not_shown() {
        let mut b = block("agents/mine-refuter.md", 1, 200, 243);
        b.window = Some(200);
        b.not_shown = Some((201, 243));
        b.sha = Some(sha("9c029c029c02"));
        let out = render(&targets(vec![b], 0), Format::Text, &opts());

        assert_eq!(
            first_line(&out),
            "── agents/mine-refuter.md  (1-200 of 243 · window 200 · :201-243 not shown) · sha:9c029c029c02"
        );
    }

    #[test]
    fn symbol_header_names_the_resolver() {
        let mut tree_sitter = block("src/store/usage.ts#usage", 38, 61, 212);
        tree_sitter.resolver = Some(Resolver::TreeSitter);
        tree_sitter.sha = Some(sha("e77be77be77b"));
        let out = render(&targets(vec![tree_sitter], 0), Format::Text, &opts());
        assert_eq!(
            first_line(&out),
            "── src/store/usage.ts#usage  (38-61 of 212 · via tree-sitter) · sha:e77be77be77b"
        );

        let mut heuristic = block("census.md#'Bottom line'", 39, 60, 353);
        heuristic.resolver = Some(Resolver::Heuristic("heading"));
        heuristic.sha = Some(sha("a1c4a1c4a1c4"));
        let out = render(&targets(vec![heuristic], 0), Format::Text, &opts());
        assert_eq!(
            first_line(&out),
            "── census.md#'Bottom line'  (39-60 of 353 · via heuristic (heading)) · sha:a1c4a1c4a1c4"
        );
    }

    #[test]
    fn numbers_are_right_aligned_to_the_widest_number_in_the_response() {
        let mut first = block("a.ts", 9, 10, 1234);
        first.lines = vec![
            line(9, Marker::None, "kept"),
            line(10, Marker::Added, "added"),
        ];
        let mut second = block("b.ts", 1234, 1234, 1234);
        second.lines = vec![line(1234, Marker::Replaced, "replaced")];
        let out = render(&targets(vec![first, second], 0), Format::Text, &opts());

        assert!(out.contains("\n   9 \tkept\n"), "{out}");
        assert!(out.contains("\n  10+\tadded\n"), "{out}");
        assert!(out.contains("\n1234~\treplaced\n"), "{out}");
    }

    #[test]
    fn the_gutter_separator_is_one_tab_and_content_indentation_survives_untouched() {
        let mut b = block("a.ts", 23, 25, 40);
        b.lines = vec![
            line(23, Marker::None, "    case \"applepay\":"),
            line(24, Marker::Replaced, "\tswitch (kind) {"),
        ];
        let out = render(&targets(vec![b], 0), Format::Text, &opts());

        assert!(
            out.contains("\n23 \t    case \"applepay\":\n"),
            "the file's own 4-space indent must survive right after the tab: {out}"
        );
        assert!(
            out.contains("\n24~\t\tswitch (kind) {\n"),
            "the file's own tab indent must survive right after the separator tab: {out}"
        );
    }

    #[test]
    fn no_numbers_drops_the_number_and_the_marker_column() {
        let mut b = block("a.ts", 9, 10, 1234);
        b.lines = vec![
            line(9, Marker::None, "kept"),
            line(1234, Marker::Replaced, "replaced"),
        ];
        let out = render(&targets(vec![b], 0), Format::Text, &RenderOptions {
            numbers: false,
            quiet: false,
            cost_first: false,
        });

        assert!(out.contains("\nkept\nreplaced\n"), "{out}");
    }

    #[test]
    fn quiet_keeps_the_header_and_the_footer_and_drops_the_content() {
        let mut b = block("a.ts", 1, 2, 2);
        b.sha = Some(sha("0e1f0e1f0e1f"));
        b.lines = vec![line(1, Marker::None, "one"), line(2, Marker::None, "two")];
        let mut resp = targets(vec![b], 0);
        resp.footer.summary = "showed 1 file".to_owned();

        assert_eq!(
            render(&resp, Format::Text, &quiet()),
            "── a.ts  (1-2 of 2) · sha:0e1f0e1f0e1f\n── showed 1 file\n"
        );
    }

    #[test]
    fn footer_names_every_omission_in_push_order_then_the_cost() {
        let mut resp = targets(vec![], 3600);
        resp.footer.summary = "find 'onBack' · 4 hits in 2 files · searched 31 files".to_owned();
        resp.omitted = vec![Omission::HitCap { hits: 312, cap: 50 }, Omission::Ignored {
            gitignore: 9,
            hidden: 3,
            other: 0,
            dirs: IgnoredDirs::default(),
        }];
        let out = render(&resp, Format::Text, &opts());

        assert_eq!(
            out,
            "── find 'onBack' · 4 hits in 2 files · searched 31 files · over the 50-hit cap · \
             narrow the pattern or the paths, or --files · ignored 12 (gitignore 9 · hidden 3) · \
             ~0.9k tokens\n"
        );
    }

    #[test]
    fn cost_first_puts_the_cost_line_before_the_first_header() {
        let mut b = block("a.ts", 9, 10, 1234);
        b.lines = vec![line(9, Marker::None, "kept")];
        let out = render(&targets(vec![b], 3600), Format::Text, &RenderOptions {
            numbers: true,
            quiet: false,
            cost_first: true,
        });

        assert_eq!(first_line(&out), "── ~0.9k tokens", "{out}");
    }

    #[test]
    fn without_cost_first_the_header_stays_first() {
        let mut b = block("a.ts", 9, 10, 1234);
        b.lines = vec![line(9, Marker::None, "kept")];
        let out = render(&targets(vec![b], 3600), Format::Text, &opts());

        assert!(
            first_line(&out).starts_with("── a.ts"),
            "the header leads unless --all asked for the cost first: {out}"
        );
    }

    #[test]
    fn reversing_the_push_order_reverses_the_footer_segments() {
        let mut resp = targets(vec![], 3600);
        resp.footer.summary = "find 'onBack'".to_owned();
        resp.omitted = vec![
            Omission::Ignored {
                gitignore: 9,
                hidden: 3,
                other: 0,
                dirs: IgnoredDirs::default(),
            },
            Omission::HitCap { hits: 312, cap: 50 },
        ];
        let out = render(&resp, Format::Text, &opts());

        assert_eq!(
            out,
            "── find 'onBack' · ignored 12 (gitignore 9 · hidden 3) · over the 50-hit cap · \
             narrow the pattern or the paths, or --files · ~0.9k tokens\n"
        );
    }

    fn named(names: &[&str], more: usize) -> NamedDirs {
        NamedDirs {
            named: names.iter().map(|name| (*name).to_owned()).collect(),
            more,
        }
    }

    fn ignored(files: (usize, usize), gitignore: NamedDirs, hidden: NamedDirs) -> Omission {
        Omission::Ignored {
            gitignore: files.0,
            hidden: files.1,
            other: 0,
            dirs: IgnoredDirs { gitignore, hidden },
        }
    }

    #[test]
    fn ignored_directories_follow_the_file_counts_grouped_by_the_source_that_pruned_them() {
        assert_eq!(
            ignored((1, 2), named(&["target"], 0), named(&[".github"], 0)).to_string(),
            "ignored 3 (gitignore 1 \u{b7} hidden 2) \u{b7} ignored dirs (gitignore target/ \u{b7} \
             hidden .github/)"
        );
    }

    #[test]
    fn a_source_that_pruned_no_directory_is_not_named() {
        assert_eq!(
            ignored((0, 0), named(&["target", "build"], 0), named(&[], 0)).to_string(),
            "ignored dirs (gitignore target/, build/)"
        );
        assert_eq!(
            ignored((0, 0), named(&[], 0), named(&[".github"], 0)).to_string(),
            "ignored dirs (hidden .github/)"
        );
    }

    #[test]
    fn ignored_files_alone_print_no_directory_list() {
        assert_eq!(
            ignored((1, 2), named(&[], 0), named(&[], 0)).to_string(),
            "ignored 3 (gitignore 1 \u{b7} hidden 2)"
        );
    }

    #[test]
    fn each_source_counts_the_directories_it_pruned_past_the_named_ones() {
        assert_eq!(
            ignored((0, 0), named(&["a", "b", "c", "d"], 3), named(&[".e"], 2)).to_string(),
            "ignored dirs (gitignore a/, b/, c/, d/ (+3 more) \u{b7} hidden .e/ (+2 more))"
        );
    }

    #[test]
    fn a_control_character_in_a_directory_name_is_escaped_onto_the_footer_line() {
        let out = ignored((0, 0), named(&[], 0), named(&[".a\nb", ".c\td"], 0)).to_string();

        assert_eq!(out, "ignored dirs (hidden .a\\nb/, .c\\td/)");
        assert!(!out.contains('\n'), "{out:?}");
    }

    #[test]
    fn a_directory_name_with_no_control_character_is_written_as_is() {
        assert_eq!(
            ignored((0, 0), named(&["caf\u{e9} dir"], 0), named(&[], 0)).to_string(),
            "ignored dirs (gitignore caf\u{e9} dir/)"
        );
    }

    #[test]
    fn ignored_directories_reach_json_per_source_as_names_and_a_more_count() {
        let mut resp = targets(vec![], 0);
        resp.omitted = vec![ignored((0, 1), named(&["target"], 2), named(&[".a\nb"], 0))];
        let value: serde_json::Value =
            serde_json::from_str(&render(&resp, Format::Json, &opts())).expect("valid JSON");

        assert_eq!(
            value["omitted"][0]["ignored"],
            serde_json::json!({
                "gitignore": 0,
                "hidden": 1,
                "other": 0,
                "dirs": {
                    "gitignore": {"named": ["target"], "more": 2},
                    "hidden": {"named": [".a\nb"], "more": 0},
                },
            })
        );
    }

    #[test]
    fn every_omission_renders_the_words_its_worked_example_uses() {
        assert_eq!(
            Omission::Window {
                shown: (1, 200),
                total: 243,
            }
            .to_string(),
            ":201-243 not shown"
        );
        assert_eq!(
            Omission::Budget {
                budget: 3000,
                trimmed_target: "index.tsx".to_owned(),
                not_shown: None,
            }
            .to_string(),
            "budget 3000 trimmed index.tsx"
        );
        // Names the cut range, so a window's own `:x-y not shown` is never read as the budget's.
        assert_eq!(
            Omission::Budget {
                budget: 100,
                trimmed_target: "big.txt".to_owned(),
                not_shown: Some((59, 200)),
            }
            .to_string(),
            "budget 100 trimmed big.txt (:59-200 not shown)"
        );
        assert_eq!(
            Omission::Unresolved {
                target: "nope2.ts".to_owned(),
                error: "not_found",
            }
            .to_string(),
            "nope2.ts failed (not_found)"
        );
        assert_eq!(
            Omission::CheckInconclusive {
                layer: "structure".to_owned(),
                reason: "line 41's construct changed".to_owned(),
            }
            .to_string(),
            "check: structure inconclusive (line 41's construct changed)"
        );
        assert_eq!(
            Omission::CheckInconclusive {
                layer: "tsc".to_owned(),
                reason: "failed before and after".to_owned(),
            }
            .to_string(),
            "check: tsc inconclusive (failed before and after)"
        );
        assert_eq!(
            Omission::XattrsDropped {
                path: PathBuf::from("scripts/new-check.sh"),
            }
            .to_string(),
            "xattrs dropped"
        );
        assert_eq!(
            Omission::HitCap { hits: 312, cap: 50 }.to_string(),
            "over the 50-hit cap · narrow the pattern or the paths, or --files"
        );
    }

    #[test]
    fn no_stats_drops_the_cost_and_keeps_every_omission() {
        let mut resp = targets(vec![], 3600);
        resp.stats.token_ratio = token_ratio(Some(OsStr::new("1")), None);
        resp.footer.summary = "showed 1 file".to_owned();
        resp.omitted = vec![Omission::Normalized, Omission::CheckSkipped {
            reason: "no grammar for .vue".to_owned(),
        }];
        let out = render(&resp, Format::Text, &opts());

        assert_eq!(
            out, "── showed 1 file · normalized · check: skipped (no grammar for .vue)\n",
            "a budget or a stats switch may never trim the footer"
        );
        assert_eq!(resp.stats.tokens_est(), None);
        assert!(
            render(&resp, Format::Json, &opts()).contains("\"tokens_est\":null"),
            "both renderings read the same accessor"
        );
    }

    #[test]
    fn token_ratio_eight_halves_the_default_estimate() {
        let mut resp = targets(vec![], 3600);
        resp.stats.token_ratio = token_ratio(None, Some(OsStr::new("8")));
        assert_eq!(resp.stats.tokens_est(), Some(450));

        resp.footer.summary = "showed 1 file".to_owned();
        assert_eq!(
            render(&resp, Format::Text, &opts()),
            "── showed 1 file · ~0.5k tokens\n"
        );
    }

    #[test]
    fn the_text_cost_and_the_json_cost_come_from_one_number() {
        // Token estimates are bytes ÷ 4, so 3600 bytes is 900 tokens.
        let mut resp = targets(vec![block("a.ts", 1, 1, 1)], 3600);
        resp.footer.summary = "showed 1 file".to_owned();

        let text = render(&resp, Format::Text, &opts());
        let json: serde_json::Value =
            serde_json::from_str(&render(&resp, Format::Json, &opts())).expect("valid json");

        assert_eq!(json["stats"]["tokens_est"], 900);
        assert!(text.ends_with("· ~0.9k tokens\n"), "{text}");
        assert_eq!(cost_segment(900), "~0.9k tokens");
    }

    #[test]
    fn cost_segment_uses_thousands_only_where_a_tenth_reads() {
        assert_eq!(cost_segment(210), "~0.2k tokens");
        assert_eq!(cost_segment(2600), "~2.6k tokens");
        assert_eq!(cost_segment(99), "~99 tokens");
    }

    #[test]
    fn sha12_parse_refuses_short_and_non_lowercase_hex() {
        assert_eq!(Sha12::parse("abc"), None);
        assert_eq!(Sha12::parse("E77BE77BE77B"), None);
        assert_eq!(Sha12::parse("e77be77be77g"), None);
        assert_eq!(
            Sha12::parse("e77be77be77b").map(|s| s.as_str().to_owned()),
            Some("e77be77be77b".to_owned())
        );
        let long = "e77be77be77b0123456789ab";
        assert_eq!(
            Sha12::parse(long).map(|s| s.as_str().to_owned()),
            Some("e77be77be77b".to_owned())
        );
    }

    #[test]
    fn a_summary_with_no_targets_renders_the_footer_alone_and_still_counts_as_output() {
        let mut resp = targets(vec![], 0);
        resp.footer.summary = "312 hits in 47 files".to_owned();
        resp.omitted = vec![Omission::HitCap { hits: 312, cap: 50 }];

        assert!(resp.has_output());
        assert_eq!(
            render(&resp, Format::Text, &opts()),
            "── 312 hits in 47 files · over the 50-hit cap · narrow the pattern or the paths, or \
             --files\n"
        );

        let empty = Response::empty("show");
        assert!(
            !empty.has_output(),
            "nothing to say means nothing on stdout"
        );
        assert_eq!(render(&empty, Format::Text, &opts()), "");
    }

    #[test]
    fn raw_body_renders_verbatim_with_no_header_or_footer() {
        let text = "lets — Locate · Edit · Transform · Show\n\n  show   <target>...\n";
        let mut resp = Response::empty("guide");
        resp.body = Body::Raw {
            field: "guide",
            text: text.to_owned(),
        };
        resp.footer.summary = "showed 1 file".to_owned();
        resp.stats.bytes = 3600;

        let rendered = render(&resp, Format::Text, &opts());
        assert_eq!(rendered, text);
        assert!(!rendered.contains("──"));
        assert_eq!(
            render(&resp, Format::Json, &opts()),
            format!("{{\"guide\":{}}}\n", json_str(text))
        );
    }

    fn json_str(text: &str) -> String {
        serde_json::to_string(text).expect("a string serializes")
    }

    #[test]
    fn a_hit_block_heads_its_file_without_a_range_and_marks_hits_against_context() {
        let mut b = bare_block(".claude/plans/rt-fe-wire-and-retire-BRIEF.md");
        b.lines = vec![
            line(549, Marker::Hit, "## Back navigation"),
            line(550, Marker::Context, "The «onBack» handler …"),
        ];
        let mut resp = response("find", Body::Targets(vec![b]), 3600);
        resp.footer.summary = "4 hits in 2 files · searched 31 files".to_owned();

        assert_eq!(
            render(&resp, Format::Text, &opts()),
            "── .claude/plans/rt-fe-wire-and-retire-BRIEF.md\n\
             549:\t## Back navigation\n\
             550-\tThe «onBack» handler …\n\
             ── 4 hits in 2 files · searched 31 files · ~0.9k tokens\n"
        );
    }

    #[test]
    fn files_only_lists_bare_paths_and_still_owes_its_footer() {
        let mut resp = response(
            "find",
            Body::Files(vec![
                PathBuf::from("src/server/compose.ts"),
                PathBuf::from("src/server/errors.ts"),
            ]),
            0,
        );
        resp.footer.summary = "47 files · searched 212 files".to_owned();

        assert_eq!(
            render(&resp, Format::Text, &opts()),
            "src/server/compose.ts\n\
             src/server/errors.ts\n\
             ── 47 files · searched 212 files\n"
        );
    }

    #[test]
    fn count_rows_follow_the_summary_and_right_align_the_count() {
        let mut resp = response(
            "find",
            Body::Counts(vec![
                CountRow {
                    count: 9,
                    path: PathBuf::from("plugins/example/dp/cmd/dp/main.go"),
                },
                CountRow {
                    count: 2,
                    path: PathBuf::from("plugins/gitty/scripts/push-commit.sh"),
                },
            ]),
            0,
        );
        resp.footer.summary = "14 hits in 6 files".to_owned();

        assert_eq!(
            render(&resp, Format::Text, &opts()),
            "── 14 hits in 6 files\n9  plugins/example/dp/cmd/dp/main.go\n2  plugins/gitty/scripts/push-commit.sh\n"
        );
    }

    fn edit_result(path: &str, replacements: usize, lines: Vec<usize>) -> EditResult {
        EditResult {
            path: PathBuf::from(path),
            kind: EditKind::Replaced {
                count: replacements,
            },
            lines,
            match_kind: String::new(),
            resolver: None,
            region: None,
            check: None,
            sha: ShaPair {
                before: sha("e77be77be77b"),
                after: sha("b410b410b410"),
            },
            reverted: false,
        }
    }

    fn structure_ok() -> CheckResult {
        CheckResult {
            layer: "structure".to_owned(),
            status: "ok".to_owned(),
            errors_before: 0,
            errors_after: 0,
        }
    }

    fn edit_response(results: Vec<EditResult>, bytes: usize) -> Response {
        response("edit", Body::Edit(results), bytes)
    }

    #[test]
    fn one_edit_renders_its_region_and_a_check_and_sha_footer() {
        let mut result = edit_result("src/store/usage.ts", 1, vec![42]);
        result.match_kind = "exact".to_owned();
        result.region = Some(Region {
            start: 40,
            end: 44,
            lines: vec![
                line(40, Marker::None, "export function usage() {"),
                line(42, Marker::Replaced, "  const cap = 20"),
                line(44, Marker::None, "}"),
            ],
        });
        result.check = Some(structure_ok());

        let out = render(&edit_response(vec![result], 840), Format::Text, &opts());

        assert_eq!(
            out,
            "── src/store/usage.ts · 1 replacement · line 42 · exact\n\
             40 \texport function usage() {\n\
             42~\t  const cap = 20\n\
             44 \t}\n\
             ── check: structure ok · sha:e77be77be77b→b410b410b410 · ~0.2k tokens\n"
        );
    }

    #[test]
    fn a_quiet_edit_renders_the_footer_and_nothing_else() {
        let mut result = edit_result("src/store/usage.ts", 1, vec![42]);
        result.match_kind = "exact".to_owned();
        result.region = Some(Region {
            start: 40,
            end: 44,
            lines: vec![line(42, Marker::Replaced, "  const cap = 20")],
        });
        result.check = Some(structure_ok());

        let out = render(&edit_response(vec![result], 840), Format::Text, &quiet());

        assert_eq!(
            out,
            "── check: structure ok · sha:e77be77be77b→b410b410b410 · ~0.2k tokens\n"
        );
    }

    fn edit_header(lines: Vec<usize>) -> String {
        let mut out = String::new();
        push_edit_header(&mut out, &edit_result("f.txt", lines.len(), lines));
        out
    }

    #[test]
    fn consecutive_edited_lines_collapse_into_ranges() {
        assert_eq!(
            edit_header(vec![4, 5, 6, 9]),
            "── f.txt · 4 replacements · lines 4-6, 9\n"
        );
        assert_eq!(
            edit_header((1..=3000).collect()),
            "── f.txt · 3000 replacements · lines 1-3000\n"
        );
    }

    #[test]
    fn a_line_list_over_twenty_entries_names_how_many_lines_it_left_out() {
        // 25 isolated lines: the first 20 print and 5 are left out.
        let lines: Vec<usize> = (1..=25).map(|n| n * 10).collect();

        let header = edit_header(lines);

        let listed: Vec<String> = (1..=20).map(|n| (n * 10).to_string()).collect();
        assert_eq!(
            header,
            format!(
                "── f.txt · 25 replacements · lines {} (+5 more)\n",
                listed.join(", ")
            )
        );
    }

    #[test]
    fn ranges_past_the_cap_count_every_line_they_hold() {
        // 21 runs of three lines: 20 print, and the 21st run's 3 lines are the `more`.
        let lines: Vec<usize> = (0..21)
            .flat_map(|run| run * 10 + 1..=run * 10 + 3)
            .collect();

        let header = edit_header(lines);

        assert!(header.ends_with(", 191-193 (+3 more)\n"), "{header}");
        assert!(!header.contains("201"), "{header}");
    }

    #[test]
    fn a_short_line_list_prints_as_it_always_did() {
        assert_eq!(
            edit_header(vec![1, 8]),
            "── f.txt · 2 replacements · lines 1, 8\n"
        );
        assert_eq!(
            edit_header(vec![2, 3]),
            "── f.txt · 2 replacements · lines 2, 3\n"
        );
        assert_eq!(
            edit_header(vec![42]),
            "── f.txt · 1 replacement · line 42\n"
        );
    }

    #[test]
    fn a_batch_edit_drops_the_per_result_check_line_for_an_aggregate_summary() {
        let mut first = edit_result("src/a.ts", 1, vec![42]);
        first.check = Some(structure_ok());
        let second = edit_result("src/b.ts", 1, vec![3]);
        let third = EditResult {
            kind: EditKind::Inserted {
                lines: 1,
                anchor: Anchor {
                    side: AnchorSide::After,
                    at: "line 1".to_owned(),
                    line: None,
                },
            },
            ..edit_result("src/c.ts", 0, vec![2])
        };
        let mut resp = edit_response(vec![first, second, third], 1600);
        resp.footer.summary =
            "3 files · 3 edits · all applied · checks: structure ok ×3".to_owned();

        let out = render(&resp, Format::Text, &opts());

        assert_eq!(
            out,
            "── src/a.ts · 1 replacement · line 42\n\
             ── src/b.ts · 1 replacement · line 3\n\
             ── src/c.ts · inserted 1 line after line 1\n\
             ── 3 files · 3 edits · all applied · checks: structure ok ×3 · ~0.4k tokens\n"
        );
        assert!(
            !out.contains("sha:"),
            "a batch has no single before/after pair to report"
        );
    }

    #[test]
    fn a_reverted_edit_renders_the_region_it_would_have_made_and_says_the_file_did_not_move() {
        let mut result = edit_result("src/store/usage.ts", 1, vec![61]);
        result.sha = ShaPair {
            before: sha("2b772b772b77"),
            after: sha("2b772b772b77"),
        };
        result.reverted = true;
        result.region = Some(Region {
            start: 61,
            end: 61,
            lines: vec![line(
                61,
                Marker::None,
                "  return total)          ← parse error: unexpected ')'",
            )],
        });
        result.check = Some(CheckResult {
            layer: String::new(),
            status: "failed → reverted".to_owned(),
            errors_before: 0,
            errors_after: 1,
        });

        let out = render(&edit_response(vec![result], 0), Format::Text, &opts());

        assert_eq!(
            out,
            "── src/store/usage.ts · 1 replacement · line 61 · REVERTED\n\
             61 \t  return total)          ← parse error: unexpected ')'\n\
             ── check: failed → reverted · file unchanged · sha:2b772b772b77\n"
        );
    }

    #[test]
    fn an_applied_edit_carries_neither_the_revert_marker_nor_the_unchanged_segment() {
        let mut result = edit_result("src/store/usage.ts", 1, vec![42]);
        result.check = Some(structure_ok());
        let resp = edit_response(vec![result], 0);

        let text = render(&resp, Format::Text, &opts());
        assert!(!text.contains("REVERTED"), "{text}");
        assert!(!text.contains("file unchanged"), "{text}");
        assert!(text.contains("sha:e77be77be77b→b410b410b410"), "{text}");

        let value: serde_json::Value =
            serde_json::from_str(&render(&resp, Format::Json, &opts())).expect("valid json");
        assert_eq!(value.get("reverted"), None);
    }

    #[test]
    fn a_reverted_edit_carries_the_flag_in_json() {
        let mut result = edit_result("src/store/usage.ts", 1, vec![61]);
        result.reverted = true;
        let value: serde_json::Value = serde_json::from_str(&render(
            &edit_response(vec![result], 0),
            Format::Json,
            &opts(),
        ))
        .expect("valid json");

        assert_eq!(value["reverted"], true);
    }

    #[test]
    fn several_replacements_list_every_line() {
        let result = edit_result("src/store/usage.ts", 4, vec![12, 42, 57, 88]);
        let out = render(&edit_response(vec![result], 0), Format::Text, &opts());

        assert_eq!(
            first_line(&out),
            "── src/store/usage.ts · 4 replacements · lines 12, 42, 57, 88"
        );
    }

    #[test]
    fn an_insert_names_its_anchor_instead_of_a_replacement_count() {
        let after_a_line = EditResult {
            kind: EditKind::Inserted {
                lines: 1,
                anchor: Anchor {
                    side: AnchorSide::After,
                    at: "line 3".to_owned(),
                    line: None,
                },
            },
            ..edit_result("src/app.ts", 0, vec![4])
        };
        let out = render(&edit_response(vec![after_a_line], 0), Format::Text, &opts());
        assert_eq!(
            first_line(&out),
            "── src/app.ts · inserted 1 line after line 3"
        );

        let before_a_symbol = EditResult {
            kind: EditKind::Inserted {
                lines: 1,
                anchor: Anchor {
                    side: AnchorSide::Before,
                    at: "#usage".to_owned(),
                    line: Some(38),
                },
            },
            ..edit_result("src/store/usage.ts", 0, vec![38])
        };
        let out = render(
            &edit_response(vec![before_a_symbol], 0),
            Format::Text,
            &opts(),
        );
        assert_eq!(
            first_line(&out),
            "── src/store/usage.ts · inserted 1 line before #usage (line 38)"
        );
    }

    #[test]
    fn one_edit_in_json_keeps_the_documented_keys_and_adds_omitted_and_stats() {
        let mut result = edit_result("src/store/usage.ts", 1, vec![42]);
        result.match_kind = "normalized (– → -)".to_owned();
        result.check = Some(structure_ok());
        let mut resp = edit_response(vec![result], 840);
        resp.omitted = vec![Omission::Normalized];

        let text = render(&resp, Format::Text, &opts());
        let value: serde_json::Value =
            serde_json::from_str(&render(&resp, Format::Json, &opts())).expect("valid json");

        assert!(
            text.contains("── check: structure ok · sha:e77be77be77b→b410b410b410 · normalized · "),
            "{text}"
        );
        assert_eq!(keys(&value), [
            "check",
            "lines",
            "match",
            "omitted",
            "path",
            "replacements",
            "sha",
            "stats"
        ]);
        assert_eq!(value["replacements"], 1);
        assert_eq!(value["sha"]["before"], "e77be77be77b");
        assert_eq!(value["omitted"], serde_json::json!(["normalized"]));
        assert_eq!(value["stats"]["tokens_est"], 210);
    }

    #[test]
    fn an_insert_in_json_carries_the_anchor_in_place_of_a_replacement_count() {
        let result = EditResult {
            kind: EditKind::Inserted {
                lines: 1,
                anchor: Anchor {
                    side: AnchorSide::Before,
                    at: "#usage".to_owned(),
                    line: Some(38),
                },
            },
            ..edit_result("src/store/usage.ts", 0, vec![38])
        };
        let value: serde_json::Value = serde_json::from_str(&render(
            &edit_response(vec![result], 840),
            Format::Json,
            &opts(),
        ))
        .expect("valid json");

        assert_eq!(value["inserted"], 1);
        assert_eq!(value["anchor"]["side"], "before");
        assert_eq!(value["anchor"]["at"], "#usage");
        assert_eq!(value["anchor"]["line"], 38);
        assert_eq!(value.get("replacements"), None);
    }

    #[test]
    fn a_batch_edit_in_json_names_the_files_that_landed() {
        let mut resp = edit_response(
            vec![
                edit_result("src/a.ts", 1, vec![42]),
                edit_result("src/b.ts", 1, vec![3]),
            ],
            1600,
        );
        resp.omitted = vec![Omission::PartialBatch {
            written: vec![PathBuf::from("src/a.ts")],
        }];

        let value: serde_json::Value =
            serde_json::from_str(&render(&resp, Format::Json, &opts())).expect("valid json");

        assert_eq!(keys(&value), ["edits", "omitted", "stats"]);
        assert_eq!(value["edits"].as_array().expect("an array").len(), 2);
        assert_eq!(
            value["omitted"][0]["partial_batch"]["written"][0],
            "src/a.ts"
        );
        assert_eq!(value["stats"]["tokens_est"], 400);
    }

    fn write_result(outcome: WriteOutcome) -> WriteResult {
        WriteResult {
            path: PathBuf::from("scripts/new-check.sh"),
            outcome,
            lines: 2,
            bytes: 34,
            sha: sha("0e1f0e1f0e1f"),
            check: None,
        }
    }

    fn write_response(result: WriteResult, bytes: usize) -> Response {
        response("write", Body::Write(result), bytes)
    }

    #[test]
    fn a_created_file_names_its_lines_bytes_sha_and_check() {
        let mut result = write_result(WriteOutcome::Created);
        result.check = Some(structure_ok());
        let out = render(&write_response(result, 34), Format::Text, &opts());

        assert_eq!(
            out,
            "── scripts/new-check.sh · created · 2 lines · 34 bytes · sha:0e1f0e1f0e1f\n\
             ── check: structure ok\n"
        );
    }

    #[test]
    fn a_created_file_with_no_check_still_names_what_was_dropped() {
        let mut resp = write_response(write_result(WriteOutcome::Created), 34);
        resp.omitted = vec![Omission::XattrsDropped {
            path: PathBuf::from("scripts/new-check.sh"),
        }];

        assert_eq!(
            render(&resp, Format::Text, &opts()),
            "── scripts/new-check.sh · created · 2 lines · 34 bytes · sha:0e1f0e1f0e1f\n\
             ── xattrs dropped\n"
        );

        let value: serde_json::Value =
            serde_json::from_str(&render(&resp, Format::Json, &opts())).expect("valid json");
        assert_eq!(keys(&value), [
            "bytes", "lines", "omitted", "outcome", "path", "sha", "stats"
        ]);
        assert_eq!(
            value["omitted"][0]["xattrs_dropped"]["path"],
            "scripts/new-check.sh"
        );
        assert_eq!(value["stats"]["tokens_est"], 8);
    }

    #[test]
    fn an_existing_file_is_a_refusal_and_carries_no_footer() {
        let out = render(
            &write_response(write_result(WriteOutcome::Exists), 3600),
            Format::Text,
            &opts(),
        );

        assert_eq!(
            out,
            "── scripts/new-check.sh exists (2 lines, sha:0e1f0e1f0e1f) · pass --force to overwrite\n"
        );
    }

    #[test]
    fn targets_json_carries_every_text_field_but_the_verb_and_the_footer() {
        let mut b = block("src/store/usage.ts#usage", 38, 61, 212);
        b.path = PathBuf::from("src/store/usage.ts");
        b.resolver = Some(Resolver::TreeSitter);
        b.sha = Some(sha("e77be77be77b"));
        b.lines = vec![line(
            38,
            Marker::None,
            "export function usage(id: string) {",
        )];
        let mut resp = targets(vec![b], 840);
        resp.footer.summary = "showed 1 symbol".to_owned();
        resp.stats.lines = 24;

        let out = render(&resp, Format::Json, &opts());
        let value: serde_json::Value = serde_json::from_str(&out).expect("valid json");

        assert_eq!(keys(&value), ["omitted", "stats", "targets"]);
        let target = &value["targets"][0];
        assert_eq!(keys(target), [
            "end", "lines", "path", "resolver", "sha", "start", "target", "total"
        ]);
        assert_eq!(target["resolver"], "tree-sitter");
        assert_eq!(value["stats"]["tokens_est"], 210);
        assert_eq!(value["omitted"], serde_json::json!([]));
    }

    #[test]
    fn a_hit_block_in_json_has_no_range_keys() {
        let mut b = bare_block(".claude/plans/rt-fe-wire.md");
        b.lines = vec![line(549, Marker::Hit, "## Back navigation")];
        let value: serde_json::Value = serde_json::from_str(&render(
            &response("find", Body::Targets(vec![b]), 0),
            Format::Json,
            &opts(),
        ))
        .expect("valid json");

        let target = &value["targets"][0];
        assert_eq!(keys(target), ["lines", "path", "target"]);
        assert_eq!(target["lines"][0]["marker"], "hit");
    }

    #[test]
    fn jsonl_is_one_object_per_target_then_a_stats_and_omitted_record() {
        let mut first = block("a.ts", 1, 1, 1);
        first.lines = vec![line(1, Marker::None, "a")];
        let second = block("b.ts", 1, 1, 1);
        let mut resp = targets(vec![first, second], 840);
        resp.omitted = vec![Omission::Normalized];

        let out = render(&resp, Format::Jsonl, &opts());
        let records: Vec<serde_json::Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).expect("valid json"))
            .collect();

        assert_eq!(records.len(), 3);
        assert_eq!(records[0]["target"], "a.ts");
        assert_eq!(records[1]["target"], "b.ts");
        assert_eq!(records[2]["stats"]["tokens_est"], 210);
        assert_eq!(records[2]["omitted"], serde_json::json!(["normalized"]));
    }

    #[test]
    fn the_same_response_renders_byte_identically_and_without_colour() {
        let mut b = block("a.ts", 1, 2, 2);
        b.sha = Some(sha("0e1f0e1f0e1f"));
        b.lines = vec![line(1, Marker::Added, "one"), line(2, Marker::None, "two")];
        let mut resp = targets(vec![b], 840);
        resp.footer.summary = "showed 1 file".to_owned();
        resp.omitted = vec![Omission::XattrsDropped {
            path: PathBuf::from("a.ts"),
        }];

        let once = render(&resp, Format::Text, &opts());
        let twice = render(&resp, Format::Text, &opts());

        assert_eq!(once, twice);
        assert!(!once.contains('\u{1b}'));
    }

    #[test]
    fn an_error_puts_the_code_on_the_last_line() {
        let text = error_text("── src/store/usage.ts · --old not found", "not_found");

        assert_eq!(
            text,
            "── src/store/usage.ts · --old not found\nERROR_CODE=not_found\n"
        );
        assert_eq!(text.lines().last(), Some("ERROR_CODE=not_found"));
    }

    const EMIT_FIXTURE: &str = "LETS_OUTPUT_EMIT_FIXTURE";

    #[test]
    fn emit_fixture() {
        if std::env::var_os(EMIT_FIXTURE).is_none() {
            return;
        }
        write_stdout("── scripts/new-check.sh · created\n").expect("stdout accepts the answer");
        write_error("── src/store/usage.ts · --old not found", "not_found")
            .expect("stderr accepts the diagnostic");
    }

    #[test]
    fn the_error_goes_to_stderr_alone_through_the_real_handles() {
        let run = std::process::Command::new(std::env::current_exe().expect("the test binary"))
            .args(["--exact", "output::tests::emit_fixture", "--nocapture"])
            .env(EMIT_FIXTURE, "1")
            .output()
            .expect("the test binary re-runs itself");
        let (out, err) = (
            String::from_utf8(run.stdout).expect("utf-8 stdout"),
            String::from_utf8(run.stderr).expect("utf-8 stderr"),
        );

        assert!(out.contains("── scripts/new-check.sh · created"), "{out}");
        assert!(
            !out.contains("ERROR_CODE") && !out.contains("--old not found"),
            "stdout stays parseable: {out}"
        );
        assert!(
            err.contains("── src/store/usage.ts · --old not found"),
            "{err}"
        );
        assert_eq!(err.lines().last(), Some("ERROR_CODE=not_found"));
    }

    fn transform_result(
        path: &str,
        format: TransformFormat,
        operations: Vec<TransformOp>,
        lines: Vec<usize>,
    ) -> TransformResult {
        TransformResult {
            path: PathBuf::from(path),
            format,
            operations,
            lines,
            region: None,
            check: None,
            sha: ShaPair {
                before: sha("5a5a5a5a5a5a"),
                after: sha("6b6b6b6b6b6b"),
            },
        }
    }

    fn set(key: &str) -> TransformOp {
        TransformOp::Set {
            key: key.to_owned(),
        }
    }

    fn delete(key: &str) -> TransformOp {
        TransformOp::Delete {
            key: key.to_owned(),
        }
    }

    fn append(key: &str) -> TransformOp {
        TransformOp::Append {
            key: key.to_owned(),
        }
    }

    fn layer_ok(layer: &str) -> CheckResult {
        CheckResult {
            layer: layer.to_owned(),
            status: "ok".to_owned(),
            errors_before: 0,
            errors_after: 0,
        }
    }

    fn transform_response(results: Vec<TransformResult>) -> Response {
        response("transform", Body::Transform(results), 0)
    }

    #[test]
    fn one_transform_renders_header_region_and_a_check_and_sha_footer() {
        let mut result = transform_result(
            "coding/wiki/projects-moc.md",
            TransformFormat::Frontmatter,
            vec![set("last_updated")],
            vec![4],
        );
        result.region = Some(Region {
            start: 4,
            end: 4,
            lines: vec![line(4, Marker::Replaced, "last_updated: 2026-09-16")],
        });
        result.check = Some(layer_ok("frontmatter"));
        result.sha = ShaPair {
            before: sha("1b2c1b2c1b2c"),
            after: sha("77d077d077d0"),
        };

        assert_eq!(
            render(&transform_response(vec![result]), Format::Text, &opts()),
            "── coding/wiki/projects-moc.md · frontmatter · set last_updated · line 4\n\
             4~\tlast_updated: 2026-09-16\n\
             ── check: frontmatter ok · sha:1b2c1b2c1b2c→77d077d077d0\n"
        );
    }

    #[test]
    fn two_sets_group_under_one_verb_in_command_line_order() {
        let mut result = transform_result(
            ".claude/dispatch-config.json",
            TransformFormat::Json,
            vec![set("features.e2e"), set("review.threads")],
            vec![8, 14],
        );
        result.region = Some(Region {
            start: 8,
            end: 14,
            lines: vec![
                line(8, Marker::Replaced, "    \"e2e\": false,"),
                line(14, Marker::Replaced, "    \"threads\": 3"),
            ],
        });
        result.check = Some(layer_ok("json"));
        result.sha = ShaPair {
            before: sha("aa10aa10aa10"),
            after: sha("bb21bb21bb21"),
        };

        assert_eq!(
            render(&transform_response(vec![result]), Format::Text, &opts()),
            "── .claude/dispatch-config.json · json · set features.e2e, review.threads · lines 8, \
             14\n \
             8~\t    \"e2e\": false,\n\
             14~\t    \"threads\": 3\n\
             ── check: json ok · sha:aa10aa10aa10→bb21bb21bb21\n"
        );
    }

    #[test]
    fn an_append_before_a_delete_renders_in_that_order_with_the_deleted_glyph() {
        let mut result = transform_result(
            "config.yaml",
            TransformFormat::Yaml,
            vec![append("allow"), delete("legacy.token")],
            vec![12, 30],
        );
        result.region = Some(Region {
            start: 12,
            end: 30,
            lines: vec![
                line(12, Marker::Added, "  - gh"),
                line(30, Marker::Deleted, "  token: abc          (deleted)"),
            ],
        });
        result.check = Some(layer_ok("yaml"));

        assert_eq!(
            render(&transform_response(vec![result]), Format::Text, &opts()),
            "── config.yaml · yaml · append allow, delete legacy.token · lines 12, 30\n\
             12+\t  - gh\n\
             30-\t  token: abc          (deleted)\n\
             ── check: yaml ok · sha:5a5a5a5a5a5a→6b6b6b6b6b6b\n"
        );
    }

    // A second `set` after a `delete` joins its kind's first group.
    #[test]
    fn kinds_render_in_first_occurrence_order_not_a_fixed_one() {
        let reversed = transform_result(
            "config.yaml",
            TransformFormat::Yaml,
            vec![delete("legacy.token"), append("allow")],
            vec![30, 12],
        );
        let out = render(&transform_response(vec![reversed]), Format::Text, &opts());
        assert_eq!(
            first_line(&out),
            "── config.yaml · yaml · delete legacy.token, append allow · lines 12, 30"
        );

        let interleaved = transform_result(
            "a.json",
            TransformFormat::Json,
            vec![set("b"), delete("c"), set("a")],
            vec![3, 3, 1],
        );
        let out = render(
            &transform_response(vec![interleaved]),
            Format::Text,
            &opts(),
        );
        assert_eq!(
            first_line(&out),
            "── a.json · json · set b, a, delete c · lines 1, 3"
        );
    }

    #[test]
    fn a_transform_batch_prints_every_header_and_one_aggregate_footer() {
        let mut first = transform_result(
            "plugins/gitty/.claude-plugin/plugin.json",
            TransformFormat::Json,
            vec![set("version")],
            vec![3],
        );
        first.check = Some(layer_ok("json"));
        let mut second = transform_result(
            ".claude-plugin/marketplace.json",
            TransformFormat::Json,
            vec![set("metadata.version")],
            vec![5],
        );
        second.check = Some(layer_ok("json"));
        let mut resp = transform_response(vec![first, second]);
        resp.footer.summary = "2 files · 2 changes · all applied · checks: json ok ×2".to_owned();

        let out = render(&resp, Format::Text, &opts());

        assert_eq!(
            out,
            "── plugins/gitty/.claude-plugin/plugin.json · json · set version · line 3\n\
             ── .claude-plugin/marketplace.json · json · set metadata.version · line 5\n\
             ── 2 files · 2 changes · all applied · checks: json ok ×2\n"
        );
        assert!(!out.contains("sha:"), "a batch has no single sha pair");
        assert!(!out.contains("check: json"), "no per-result check line");
    }

    // A batch that only partly landed can leave one result with a summary set.
    #[test]
    fn one_landed_result_keeps_its_check_and_sha_after_the_summary() {
        let mut only = transform_result("a.json", TransformFormat::Json, vec![set("v")], vec![2]);
        only.check = Some(layer_ok("json"));
        let mut resp = transform_response(vec![only]);
        resp.footer.summary = "1 of 2 files written · b.json failed".to_owned();
        resp.omitted = vec![Omission::PartialBatch {
            written: vec![PathBuf::from("a.json")],
        }];

        assert_eq!(
            render(&resp, Format::Text, &opts()).lines().last(),
            Some(
                "── 1 of 2 files written · b.json failed · check: json ok · \
                 sha:5a5a5a5a5a5a→6b6b6b6b6b6b · wrote 1 file: a.json"
            )
        );
    }

    fn sorted_keys(value: &serde_json::Value) -> Vec<String> {
        let mut keys = keys(value);
        keys.sort();
        keys
    }

    #[test]
    fn one_transform_in_json_is_flat_and_omits_absent_region_and_check() {
        let result = transform_result(
            "config.yaml",
            TransformFormat::Yaml,
            vec![append("allow"), delete("legacy.token")],
            vec![12, 30],
        );
        let value: serde_json::Value = serde_json::from_str(&render(
            &transform_response(vec![result]),
            Format::Json,
            &opts(),
        ))
        .expect("valid json");

        assert_eq!(sorted_keys(&value), [
            "format",
            "lines",
            "omitted",
            "operations",
            "path",
            "sha",
            "stats"
        ]);
        assert_eq!(value["format"], "yaml");
        assert_eq!(
            value["operations"],
            serde_json::json!([
                {"op": "append", "key": "allow"},
                {"op": "delete", "key": "legacy.token"}
            ])
        );
        assert_eq!(value["lines"], serde_json::json!([12, 30]));
        assert_eq!(value["sha"]["after"], "6b6b6b6b6b6b");
    }

    #[test]
    fn a_transform_with_region_and_check_carries_both_json_keys() {
        let mut result =
            transform_result("a.toml", TransformFormat::Toml, vec![set("port")], vec![2]);
        result.region = Some(Region {
            start: 2,
            end: 2,
            lines: vec![line(2, Marker::Replaced, "port = 9090")],
        });
        result.check = Some(layer_ok("toml"));
        let value: serde_json::Value = serde_json::from_str(&render(
            &transform_response(vec![result]),
            Format::Json,
            &opts(),
        ))
        .expect("valid json");

        assert_eq!(value["check"]["layer"], "toml");
        assert_eq!(value["region"]["lines"][0]["marker"], "replaced");
        assert_eq!(value["format"], "toml");
    }

    #[test]
    fn a_transform_batch_in_json_nests_under_transforms_and_jsonl_is_one_per_result() {
        let resp = transform_response(vec![
            transform_result("a.json", TransformFormat::Json, vec![set("v")], vec![2]),
            transform_result("b.md", TransformFormat::Frontmatter, vec![set("v")], vec![
                3,
            ]),
        ]);

        let value: serde_json::Value =
            serde_json::from_str(&render(&resp, Format::Json, &opts())).expect("valid json");
        assert_eq!(sorted_keys(&value), ["omitted", "stats", "transforms"]);
        assert_eq!(value["transforms"][1]["format"], "frontmatter");

        let records: Vec<serde_json::Value> = render(&resp, Format::Jsonl, &opts())
            .lines()
            .map(|l| serde_json::from_str(l).expect("valid json"))
            .collect();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0]["path"], "a.json");
        assert_eq!(records[1]["path"], "b.md");
        assert!(records[2].get("stats").is_some());
    }

    #[test]
    fn transform_format_displays_the_words_the_header_and_check_line_use() {
        let words: Vec<String> = [
            TransformFormat::Json,
            TransformFormat::Yaml,
            TransformFormat::Toml,
            TransformFormat::Frontmatter,
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(words, ["json", "yaml", "toml", "frontmatter"]);
    }

    #[test]
    fn an_empty_transform_body_with_no_summary_writes_nothing() {
        let resp = transform_response(vec![]);
        assert!(!resp.has_output());
        assert!(
            transform_response(vec![transform_result(
                "a.json",
                TransformFormat::Json,
                vec![set("v")],
                vec![1]
            )])
            .has_output()
        );
    }

    #[test]
    fn a_partial_batch_names_the_files_that_landed() {
        let omission = Omission::PartialBatch {
            written: vec![PathBuf::from("src/a.ts"), PathBuf::from("src/b.ts")],
        };
        assert_eq!(omission.to_string(), "wrote 2 files: src/a.ts, src/b.ts");
    }

    #[test]
    fn every_new_omission_renders_its_contract_words() {
        let cases = [
            (Omission::LongLinesCut { lines: 1 }, "1 long line cut"),
            (Omission::LongLinesCut { lines: 2 }, "2 long lines cut"),
            (
                Omission::OutputTrimmed {
                    limit: 65536,
                    lines: 1,
                },
                "output over --max-bytes 65536: 1 line not shown",
            ),
            (
                Omission::OutputTrimmed {
                    limit: 65536,
                    lines: 12,
                },
                "output over --max-bytes 65536: 12 lines not shown",
            ),
            (
                Omission::LossyLines { lines: 1 },
                "1 non-UTF-8 line shown lossily",
            ),
            (
                Omission::LossyLines { lines: 3 },
                "3 non-UTF-8 lines shown lossily",
            ),
            (
                Omission::Unreadable {
                    path: PathBuf::from("vendor/locked"),
                },
                "vendor/locked unreadable",
            ),
            (Omission::CrlfMatched, "--old matched as CRLF"),
            (
                Omission::SelectorResolved {
                    selector: "plugins[name=gitty]".to_owned(),
                    resolved: "plugins[2]".to_owned(),
                },
                "plugins[name=gitty] → plugins[2]",
            ),
            (
                Omission::Glob {
                    patterns: vec!["*.ts".to_owned()],
                },
                "glob *.ts",
            ),
            (
                Omission::Glob {
                    patterns: vec!["*.ts".to_owned(), "*.tsx".to_owned()],
                },
                "glob *.ts, *.tsx",
            ),
            (
                Omission::GrepStyle {
                    pattern: r"a\|b".to_owned(),
                    read_as: "a|b".to_owned(),
                },
                r"«a\|b» had no hits, read grep-style as «a|b»",
            ),
        ];
        for (omission, expected) in cases {
            assert_eq!(omission.to_string(), expected);
        }
    }

    #[test]
    fn skipped_names_the_total_and_only_the_parts_that_happened() {
        let skipped = |binary, too_large, unreadable| {
            Omission::Skipped {
                binary,
                too_large,
                unreadable,
            }
            .to_string()
        };
        assert_eq!(
            skipped(2, 1, 3),
            "skipped 6 (binary 2 · too large 1 · unreadable 3)"
        );
        assert_eq!(skipped(2, 0, 1), "skipped 3 (binary 2 · unreadable 1)");
        assert_eq!(skipped(0, 4, 0), "skipped 4 (too large 4)");
        assert_eq!(skipped(0, 0, 0), "skipped 0");
    }

    #[test]
    fn new_omissions_serialize_with_the_enum_s_external_tag() {
        let json =
            serde_json::to_string(&[Omission::LongLinesCut { lines: 2 }, Omission::CrlfMatched])
                .expect("serializes");
        assert_eq!(json, r#"[{"long_lines_cut":{"lines":2}},"crlf_matched"]"#);
    }

    fn header_of(b: TargetBlock) -> String {
        first_line(&render(&targets(vec![b], 0), Format::Text, &opts())).to_owned()
    }

    #[test]
    fn a_crlf_file_names_it_between_the_span_and_the_sha() {
        let mut crlf = block("a.txt", 1, 3, 3);
        crlf.sha = Some(sha("0123456789ab"));
        crlf.crlf = true;
        assert_eq!(
            header_of(crlf),
            "── a.txt  (1-3 of 3) · crlf · sha:0123456789ab"
        );

        let mut lf = block("a.txt", 1, 3, 3);
        lf.sha = Some(sha("0123456789ab"));
        assert_eq!(header_of(lf), "── a.txt  (1-3 of 3) · sha:0123456789ab");

        let mut bare = bare_block("a.txt");
        bare.crlf = true;
        assert_eq!(header_of(bare), "── a.txt · crlf");
    }

    #[test]
    fn lossy_lines_follow_crlf_and_list_at_most_five() {
        let lossy = |lines: Vec<usize>, crlf: bool| {
            let mut b = block("latin1.txt", 1, 40, 40);
            b.sha = Some(sha("0123456789ab"));
            b.crlf = crlf;
            b.lossy_lines = lines;
            header_of(b)
        };
        assert_eq!(
            lossy(vec![3, 7], false),
            "── latin1.txt  (1-40 of 40) · non-UTF-8 lines 3, 7 · sha:0123456789ab"
        );
        assert_eq!(
            lossy(vec![3, 7], true),
            "── latin1.txt  (1-40 of 40) · crlf · non-UTF-8 lines 3, 7 · sha:0123456789ab"
        );
        assert_eq!(
            lossy(vec![3, 7, 9, 12, 15, 20, 31], false),
            "── latin1.txt  (1-40 of 40) · non-UTF-8 lines 3, 7, 9, 12, 15 (+2 more) · \
             sha:0123456789ab"
        );
        assert_eq!(
            lossy(vec![3, 7, 9, 12, 15], false),
            "── latin1.txt  (1-40 of 40) · non-UTF-8 lines 3, 7, 9, 12, 15 · sha:0123456789ab"
        );
        assert_eq!(
            lossy(vec![3], false),
            "── latin1.txt  (1-40 of 40) · non-UTF-8 line 3 · sha:0123456789ab"
        );
    }

    #[test]
    fn crlf_and_lossy_lines_are_json_keys_only_when_they_apply() {
        let target_keys = |b: TargetBlock| {
            let value: serde_json::Value =
                serde_json::from_str(&render(&targets(vec![b], 0), Format::Json, &opts()))
                    .expect("valid json");
            (keys(&value["targets"][0]), value["targets"][0].clone())
        };

        let (plain, _) = target_keys(block("a.txt", 1, 3, 3));
        assert_eq!(plain, ["end", "lines", "path", "start", "target", "total"]);

        let mut marked = block("a.txt", 1, 3, 3);
        marked.crlf = true;
        marked.lossy_lines = vec![3, 7];
        let (marked_keys, target) = target_keys(marked);
        assert_eq!(marked_keys, [
            "crlf",
            "end",
            "lines",
            "lossy_lines",
            "path",
            "start",
            "target",
            "total"
        ]);
        assert_eq!(target["crlf"], true);
        assert_eq!(target["lossy_lines"], serde_json::json!([3, 7]));
    }

    fn stats_report() -> StatsReport {
        let mut lets_calls = BTreeMap::new();
        lets_calls.insert("show".to_owned(), 31);
        lets_calls.insert("find".to_owned(), 9);
        StatsReport {
            sessions: 12,
            bash_calls: 340,
            lets_calls,
            hook_blocks: 5,
            blocks_followed: 4,
            calls_saved: 17,
            read_calls: 88,
            read_bytes: 1_204_000,
            skipped: StatsSkips::default(),
        }
    }

    #[test]
    fn a_stats_report_names_each_non_zero_skip_kind_in_its_footer_and_json() {
        let mut report = stats_report();
        report.skipped = StatsSkips {
            malformed_lines: 2,
            non_utf8_lines: 0,
            unreadable_files: 1,
            walk_errors: 0,
        };
        let resp = response("stats", Body::Stats(report), 0);

        let text = render(&resp, Format::Text, &opts());
        let json = render(&resp, Format::Json, &opts());

        assert!(
            text.ends_with(
                "read bytes       1204000\n── skipped: 2 malformed lines, 1 unreadable file\n"
            ),
            "{text}"
        );
        assert!(
            json.ends_with(",\"skipped\":{\"malformed_lines\":2,\"unreadable_files\":1}}\n"),
            "{json}"
        );
    }

    #[test]
    fn a_stats_report_is_aligned_rows_with_verbs_indented_under_their_sum() {
        let resp = response("stats", Body::Stats(stats_report()), 0);
        assert!(resp.has_output());

        assert_eq!(
            render(&resp, Format::Text, &opts()),
            "sessions              12\n\
             bash calls           340\n\
             lets calls            40\n\
             \x20 find                 9\n\
             \x20 show                31\n\
             hook blocks            5\n\
             blocks followed        4\n\
             calls saved           17\n\
             read calls            88\n\
             read bytes       1204000\n"
        );
    }

    #[test]
    fn a_stats_report_in_json_is_its_own_keys_flat_with_no_tail() {
        let resp = response("stats", Body::Stats(stats_report()), 0);
        let expected = "{\"sessions\":12,\"bash_calls\":340,\"lets_calls\":{\"find\":9,\"show\":31},\
                        \"hook_blocks\":5,\"blocks_followed\":4,\"calls_saved\":17,\
                        \"read_calls\":88,\"read_bytes\":1204000}\n";

        assert_eq!(render(&resp, Format::Json, &opts()), expected);
        assert_eq!(render(&resp, Format::Jsonl, &opts()), expected);
    }

    #[test]
    fn an_all_failed_call_renders_one_error_object_with_the_empty_tail() {
        let resp = Response::empty("show");
        assert_eq!(
            render_error(&resp, "not_found", "nope.ts: no such file"),
            "{\"error\":{\"slug\":\"not_found\",\"message\":\"nope.ts: no such file\"},\
             \"omitted\":[],\"stats\":{\"lines\":0,\"bytes\":0,\"tokens_est\":null}}\n"
        );
    }

    #[test]
    fn the_error_object_carries_the_response_s_omissions_and_escapes_the_message() {
        let mut resp = Response::empty("show");
        resp.omitted = vec![Omission::Unresolved {
            target: "nope.ts".to_owned(),
            error: "not_found",
        }];
        assert_eq!(
            render_error(&resp, "not_found", "no hits for «\"x\"»"),
            "{\"error\":{\"slug\":\"not_found\",\"message\":\"no hits for «\\\"x\\\"»\"},\
             \"omitted\":[{\"unresolved\":{\"target\":\"nope.ts\",\"error\":\"not_found\"}}],\
             \"stats\":{\"lines\":0,\"bytes\":0,\"tokens_est\":null}}\n"
        );
    }
}
