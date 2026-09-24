//! A target that cannot be resolved fails alone: the rest still render, every failure is
//! named in the footer and on stderr, and the first in argument order sets the exit.

use std::path::Path;

use crate::cli::{Global, ShowArgs};
use crate::error::{Candidate, Error, UnsupportedReason};
use crate::hook::bre;
use crate::output::{
    Body, Format, Line, Marker, Omission, Resolver, Response, Sha12, Span, TargetBlock,
};
use crate::window::Bounds;
use crate::{Outcome, atomic, fs, grammars, symbols, target, window};

pub fn run(args: &ShowArgs, global: &Global, _format: Format) -> Outcome {
    let mut response = Response::empty("show");
    let mut blocks = Vec::with_capacity(args.targets.len());
    let mut errors = Vec::new();

    for raw in &args.targets {
        match resolve(raw, args, global, &mut response.omitted) {
            Ok(block) => blocks.push(block),
            Err(error) => {
                response.omitted.push(Omission::Unresolved {
                    target: raw.clone(),
                    error: error.slug(),
                });
                errors.push(error);
            },
        }
    }

    // Ahead of the budget and `--max-bytes` checks, so one minified line is cut, not refused.
    let cut: usize = blocks
        .iter_mut()
        .map(|block| window::cut_long_lines(&mut block.lines, &[]))
        .sum();
    if cut > 0 {
        response.omitted.push(Omission::LongLinesCut { lines: cut });
    }

    if let Some(budget) = global.budget {
        response
            .omitted
            .extend(window::trim_to_budget(&mut blocks, budget));
        for block in &mut blocks {
            let last = block.lines.last().map_or(0, |line| line.number);
            block.lossy_lines.retain(|line| *line <= last);
        }
    }

    response.stats.lines = blocks.iter().map(|block| block.lines.len()).sum();
    response.stats.bytes = blocks
        .iter()
        .map(|block| window::content_bytes(&block.lines))
        .sum();

    let Some(failure) = Error::all(errors) else {
        response.footer.summary = summary(blocks.len(), response.stats.lines);
        response.body = Body::Targets(blocks);
        return over_max_bytes(global, response.stats.bytes).map_or_else(
            || Outcome::ok(response),
            |over| Outcome::failed("show", over),
        );
    };
    if blocks.is_empty() {
        return Outcome::failed("show", failure);
    }
    response.footer.summary = summary(blocks.len(), response.stats.lines);
    response.body = Body::Targets(blocks);
    over_max_bytes(global, response.stats.bytes).map_or_else(
        || Outcome::partial(response, failure),
        |over| Outcome::failed("show", over),
    )
}

/// Refused whole, not trimmed: the caller picks what to ask for instead of paying for a cut answer.
fn over_max_bytes(global: &Global, bytes: usize) -> Option<Error> {
    if global.budget.is_some() || bytes <= global.max_bytes {
        return None;
    }
    Some(Error::OverBudget {
        bytes: u64::try_from(bytes).unwrap_or(u64::MAX),
        limit: u64::try_from(global.max_bytes).unwrap_or(u64::MAX),
    })
}

fn summary(targets: usize, lines: usize) -> String {
    format!(
        "showed {targets} target{} · {lines} line{}",
        plural(targets),
        plural(lines)
    )
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn resolve(
    raw: &str,
    args: &ShowArgs,
    global: &Global,
    omitted: &mut Vec<Omission>,
) -> Result<TargetBlock, Error> {
    let parsed = target::parse(raw);
    // Containment is checked on the canonical path but the typed one is read, so messages carry
    // no machine-specific absolute path.
    fs::guard_scope(&parsed.path, global.allow_outside)?;
    let file =
        read_text(&parsed.path, global.max_file_bytes).map_err(|error| mistyped(raw, error))?;
    let newlines = count_byte(file.content.as_bytes(), b'\n');
    let total = newlines + usize::from(!file.content.is_empty() && !file.content.ends_with('\n'));

    // `window::Bounds` has no zero-line range, so an empty file gets a bare header and no span.
    if total == 0 && matches!(parsed.kind, target::Kind::Whole) {
        return Ok(TargetBlock {
            target: raw.to_owned(),
            path: parsed.path,
            span: None,
            window: None,
            not_shown: None,
            resolver: None,
            crlf: false,
            lossy_lines: Vec::new(),
            sha: Some(file.sha),
            lines: Vec::new(),
        });
    }

    let (bounds, truncation, resolver) = match &parsed.kind {
        target::Kind::Whole => {
            let size = if args.all { total } else { args.window };
            let (bounds, omission) = window::window(total, size);
            // Recorded even when nothing is left: `--window 0` is a narrowing the footer names.
            let named = omission.is_some();
            omitted.extend(omission);
            let bounds = bounds.ok_or_else(|| not_found(raw, "content".to_owned()))?;
            (bounds, named.then_some(size), None)
        },
        target::Kind::Line(line) => (
            around(*line, args, total).ok_or_else(|| not_found(raw, format!(":{line}")))?,
            None,
            None,
        ),
        target::Kind::Range(start, end) => (
            window::range(*start, *end, total)
                .ok_or_else(|| not_found(raw, format!(":{start}-{end}")))?,
            None,
            None,
        ),
        target::Kind::Regex {
            pattern,
            occurrence,
        } => {
            let what = format!("@'{pattern}'");
            // `Error` has no malformed-pattern variant, so a bad pattern is a not-found saying why.
            let found = target::find_regex(&file.content, pattern, *occurrence)
                .map_err(|_| not_found(raw, format!("{what} (invalid regex)")))?;
            let found = match found {
                Some(found) => found,
                None => grep_style(raw, &file.content, pattern, *occurrence, omitted)?,
            };
            (
                around(found.line, args, total)
                    .ok_or_else(|| not_found(raw, format!("@'{pattern}'")))?,
                None,
                None,
            )
        },
        target::Kind::Symbol(segments) => {
            let (bounds, resolver) = symbol(raw, &parsed, segments, &file.content, total)?;
            (bounds, None, Some(resolver))
        },
    };

    let lines = lines_in(&file.content, bounds);
    Ok(TargetBlock {
        target: raw.to_owned(),
        path: parsed.path,
        span: Some(Span {
            start: bounds.start,
            end: bounds.end,
            total,
        }),
        window: truncation,
        not_shown: truncation.map(|_| (bounds.end + 1, total)),
        resolver,
        crlf: mostly_crlf(&file.content, newlines),
        lossy_lines: file
            .lossy_lines
            .into_iter()
            .filter(|line| (bounds.start..=bounds.end).contains(line))
            .collect(),
        sha: Some(file.sha),
        lines,
    })
}

fn mistyped(raw: &str, error: Error) -> Error {
    let missing =
        matches!(&error, Error::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound);
    if !missing {
        return error;
    }
    target::meant(raw).map_or(error, |meant| Error::MistypedTarget {
        target: raw.to_owned(),
        meant,
    })
}

/// `str::lines` drops the `\r` of a CRLF ending, so the header is the only place it shows.
fn mostly_crlf(content: &str, lf: usize) -> bool {
    if count_byte(content.as_bytes(), b'\r') == 0 {
        return false;
    }
    let crlf = content.matches("\r\n").count();
    crlf > lf - crlf
}

/// A `u8` lane per byte, summed per 255-byte chunk so no lane overflows: LLVM vectorises it.
fn count_byte(hay: &[u8], needle: u8) -> usize {
    hay.chunks(255)
        .map(|chunk| {
            usize::from(
                chunk
                    .iter()
                    .fold(0u8, |sum, byte| sum + u8::from(*byte == needle)),
            )
        })
        .sum()
}

struct Text {
    content: String,
    sha: Sha12,
    lossy_lines: Vec<usize>,
}

fn read_text(path: &Path, max_file_bytes: u64) -> Result<Text, Error> {
    match fs::read(path, max_file_bytes) {
        Ok(file) => Ok(Text {
            sha: file.sha(),
            content: file.content,
            lossy_lines: Vec::new(),
        }),
        Err(Error::Unsupported {
            reason: UnsupportedReason::NonUtf8Region,
            ..
        }) => match read_lossy(path, max_file_bytes)? {
            LossyOutcome::Binary => Err(Error::Unsupported {
                path: path.to_path_buf(),
                reason: UnsupportedReason::Binary,
            }),
            LossyOutcome::Text(text) => Ok(text),
        },
        Err(error) => Err(error),
    }
}

enum LossyOutcome {
    Binary,
    Text(Text),
}

/// Unlike `fs::read`, decodes invalid UTF-8 lossily so a Latin-1 file `edit` can change stays
/// readable; only a NUL or a UTF-16 BOM is binary. The sha is the raw bytes', for `edit --if`.
fn read_lossy(path: &Path, max_file_bytes: u64) -> Result<LossyOutcome, Error> {
    let raw = std::fs::read(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let bytes = u64::try_from(raw.len()).unwrap_or(u64::MAX);
    if bytes > max_file_bytes {
        return Err(Error::Unsupported {
            path: path.to_path_buf(),
            reason: UnsupportedReason::TooLarge {
                bytes,
                limit: max_file_bytes,
            },
        });
    }
    if raw.contains(&0) || raw.starts_with(&[0xfe, 0xff]) || raw.starts_with(&[0xff, 0xfe]) {
        return Ok(LossyOutcome::Binary);
    }
    // `\n` never occurs inside a multi-byte sequence, so raw-byte line numbers match `str::lines`.
    let lossy_lines = raw
        .split(|byte| *byte == b'\n')
        .enumerate()
        .filter(|(_, line)| std::str::from_utf8(line).is_err())
        .map(|(index, _)| index + 1)
        .collect();
    Ok(LossyOutcome::Text(Text {
        sha: atomic::hash12(&raw),
        content: String::from_utf8_lossy(&raw).into_owned(),
        lossy_lines,
    }))
}

fn symbol(
    raw: &str,
    parsed: &target::Target,
    segments: &[String],
    content: &str,
    total: usize,
) -> Result<(Bounds, Resolver), Error> {
    let extension = parsed
        .path
        .extension()
        .unwrap_or_default()
        .to_string_lossy();
    let symbol = format!("#{}", segments.join("."));
    let (found, missing) = if let Some(lang) = grammars::from_extension(&extension) {
        (symbols::resolve(lang, content, segments), symbol)
    } else {
        // A heuristic miss is not proof of absence, so the refusal offers a regex target.
        let name = regex::escape(segments.last().map_or("", String::as_str));
        (
            symbols::plaintext::resolve(content, segments),
            format!("{symbol} (try @'{name}')"),
        )
    };
    match found.as_slice() {
        [] => Err(not_found(raw, missing)),
        // A node that swallows the final newline (the last TOML table, a trailing YAML mapping)
        // reports an `end_line` past the file, so `window::range` clamps it.
        [one] => Ok((
            window::range(one.line, one.end_line, total).ok_or_else(|| not_found(raw, missing))?,
            one.resolver.clone(),
        )),
        many => Err(Error::Ambiguous {
            target: raw.to_owned(),
            candidates: many
                .iter()
                .map(|found| Candidate {
                    path: parsed.path.clone(),
                    line: found.line,
                    text: found.text.clone(),
                })
                .collect(),
        }),
    }
}

fn around(center: usize, args: &ShowArgs, total: usize) -> Option<Bounds> {
    window::context(
        center,
        args.before.or(args.context).unwrap_or(0),
        args.after.or(args.context).unwrap_or(0),
        total,
    )
}

/// A grep operator escape such as `\|` is a literal to the `regex` crate, so a miss is retried
/// under the grep reading.
fn grep_style(
    raw: &str,
    content: &str,
    pattern: &str,
    occurrence: usize,
    omitted: &mut Vec<Omission>,
) -> Result<target::RegexMatch, Error> {
    let what = format!("@'{pattern}'");
    // Fewer matches than `+N` asked for is not the grep habit: the pattern did match.
    let matched_nothing = matches!(target::find_regex(content, pattern, 1), Ok(None));
    let Some(read_as) = bre::grep_reading(pattern).filter(|_| matched_nothing) else {
        return Err(not_found(raw, what));
    };
    match target::find_regex(content, &read_as, occurrence) {
        Ok(Some(found)) => {
            omitted.push(Omission::GrepStyle {
                pattern: pattern.to_owned(),
                read_as,
            });
            Ok(found)
        },
        _ => Err(not_found(
            raw,
            format!("{what} (or grep-style @'{read_as}')"),
        )),
    }
}

fn not_found(raw: &str, what: String) -> Error {
    Error::NotFound {
        target: raw.to_owned(),
        what,
        nearest: None,
    }
}

fn lines_in(content: &str, bounds: Bounds) -> Vec<Line> {
    content
        .lines()
        .enumerate()
        .skip(bounds.start - 1)
        .take(bounds.end + 1 - bounds.start)
        .map(|(index, text)| Line {
            number: index + 1,
            marker: Marker::None,
            text: text.to_owned(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;
    use crate::output::{Format, RenderOptions};
    use crate::own_process::repo;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).expect("test fixture writes");
        path
    }

    fn numbered(lines: usize) -> String {
        use std::fmt::Write as _;
        (1..=lines).fold(String::new(), |mut text, n| {
            writeln!(text, "line {n}").expect("a String accepts a write");
            text
        })
    }

    fn args(targets: &[&str]) -> ShowArgs {
        ShowArgs {
            targets: targets.iter().map(|t| (*t).to_owned()).collect(),
            window: 200,
            all: false,
            after: None,
            before: None,
            context: None,
            no_numbers: false,
        }
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

    fn show(args: &ShowArgs, global: &Global) -> Outcome {
        run(args, global, Format::Text)
    }

    fn blocks(outcome: &Outcome) -> &[TargetBlock] {
        match &outcome.response.body {
            Body::Targets(blocks) => blocks,
            other => panic!("show renders targets, got {other:?}"),
        }
    }

    fn only(outcome: &Outcome) -> &TargetBlock {
        match blocks(outcome) {
            [one] => one,
            many => panic!("expected one block, got {}", many.len()),
        }
    }

    fn header(outcome: &Outcome) -> String {
        let rendered = crate::output::render(&outcome.response, Format::Text, &RenderOptions {
            numbers: true,
            quiet: false,
            cost_first: false,
        });
        rendered
            .lines()
            .next()
            .expect("a rendered block has a header")
            .to_owned()
    }

    #[test]
    fn a_whole_file_inside_the_window_shows_every_line_and_names_no_narrowing() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "small.md", &numbered(64));

        let outcome = show(&args(&["small.md"]), &global());

        let block = only(&outcome);
        assert_eq!(
            block.span,
            Some(Span {
                start: 1,
                end: 64,
                total: 64
            })
        );
        assert_eq!(block.window, None);
        assert_eq!(block.not_shown, None);
        assert_eq!(block.lines.len(), 64);
        assert_eq!(block.lines[63].number, 64);
        assert!(block.sha.is_some());
        assert!(outcome.response.omitted.is_empty());
        assert!(outcome.error.is_none(), "every target resolved");
        assert_eq!(
            outcome.response.footer.summary,
            "showed 1 target · 64 lines"
        );
    }

    #[test]
    fn a_whole_file_over_the_window_names_the_range_the_window_and_the_rest() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "big.md", &numbered(243));

        let outcome = show(&args(&["big.md"]), &global());

        let block = only(&outcome);
        assert_eq!(
            block.span,
            Some(Span {
                start: 1,
                end: 200,
                total: 243
            })
        );
        assert_eq!(block.window, Some(200));
        assert_eq!(block.not_shown, Some((201, 243)));
        assert_eq!(block.lines.len(), 200);
        assert!(matches!(outcome.response.omitted.as_slice(), [
            Omission::Window {
                shown: (1, 200),
                total: 243
            }
        ]));
        assert!(
            header(&outcome).contains("(1-200 of 243 · window 200 · :201-243 not shown)"),
            "{}",
            header(&outcome)
        );
    }

    #[test]
    fn all_drops_the_window_and_leaves_nothing_unshown() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "big.md", &numbered(243));
        let mut args = args(&["big.md"]);
        args.all = true;

        let outcome = show(&args, &global());

        let block = only(&outcome);
        assert_eq!(
            block.span,
            Some(Span {
                start: 1,
                end: 243,
                total: 243
            })
        );
        assert_eq!(block.window, None, "--all applies no window");
        assert_eq!(block.not_shown, None);
        assert_eq!(block.lines.len(), 243);
        assert!(outcome.response.omitted.is_empty());
        assert!(outcome.error.is_none());
    }

    const USAGE_TS: &str = "import { config } from './config'\n\
                            \n\
                            export function usage(id: string) {\n\
                            \x20 const cap = config.cap\n\
                            \x20 return cap\n\
                            }\n\
                            \n\
                            export function other() {}\n";

    #[test]
    fn a_symbol_that_resolves_once_shows_its_span_and_names_the_resolver() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "usage.ts", USAGE_TS);

        let outcome = show(&args(&["usage.ts#usage"]), &global());

        let block = only(&outcome);
        assert_eq!(
            block.span,
            Some(Span {
                start: 3,
                end: 6,
                total: 8
            })
        );
        assert_eq!(block.resolver, Some(Resolver::TreeSitter));
        assert_eq!(block.window, None);
        assert_eq!(
            block.not_shown, None,
            "a symbol names its own bounds; nothing narrowed them"
        );
        assert_eq!(block.lines.first().map(|line| line.number), Some(3));
        assert_eq!(block.lines.last().map(|line| line.number), Some(6));
        assert_eq!(
            header(&outcome),
            format!(
                "── usage.ts#usage  (3-6 of 8 · via tree-sitter) · sha:{}",
                block.sha.as_ref().expect("a read file has a sha").as_str()
            )
        );
    }

    #[test]
    fn a_markdown_heading_resolves_through_the_heading_heuristic() {
        let Some(dir) = repo() else { return };
        write(
            dir.path(),
            "census.md",
            "# Title\n\nintro\n\n## Bottom line\n\nthe claim\n\n## After\n\ntail\n",
        );

        let outcome = show(&args(&["census.md#'Bottom line'"]), &global());

        let block = only(&outcome);
        assert_eq!(block.resolver, Some(Resolver::Heuristic("heading")));
        assert_eq!(
            block.span,
            Some(Span {
                start: 5,
                end: 8,
                total: 11
            })
        );
        assert!(outcome.error.is_none());
    }

    const STORE_GO: &str = "package store\n\
                            \n\
                            func Open(path string) (*Store, error) { return nil, nil }\n\
                            \n\
                            type Store struct{}\n\
                            \n\
                            func (s *Store) Open() error { return nil }\n";

    #[test]
    fn an_ambiguous_symbol_lists_every_candidate_and_renders_no_block() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "store.go", STORE_GO);

        let outcome = show(&args(&["store.go#Open"]), &global());

        assert!(blocks(&outcome).is_empty(), "an ambiguity shows no content");
        let Some(Error::Ambiguous { candidates, .. }) = &outcome.error else {
            panic!("expected Error::Ambiguous, got {:?}", outcome.error);
        };
        assert_eq!(candidates.len(), 2);
        let rendered = outcome
            .error
            .as_ref()
            .expect("the ambiguity is the terminal error")
            .to_string();
        assert!(
            rendered
                .contains("store.go:3\tfunc Open(path string) (*Store, error) { return nil, nil }"),
            "{rendered}"
        );
        assert!(
            rendered.contains("store.go:7\tfunc (s *Store) Open() error { return nil }"),
            "{rendered}"
        );
    }

    #[test]
    fn qualifying_the_receiver_resolves_the_same_name_to_one_symbol() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "store.go", STORE_GO);

        let outcome = show(&args(&["store.go#Store.Open"]), &global());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(only(&outcome).span.map(|span| span.start), Some(7));
    }

    const GREET_KT: &str = "package demo\n\
                            \n\
                            fun greet(name: String): String {\n\
                            \x20   return \"hi $name\"\n\
                            }\n";

    #[test]
    fn a_symbol_in_a_file_with_no_bundled_grammar_resolves_through_the_plaintext_heuristic() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "greet.kt", GREET_KT);

        let outcome = show(&args(&["greet.kt#greet"]), &global());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let block = only(&outcome);
        assert_eq!(
            block.span,
            Some(Span {
                start: 3,
                end: 5,
                total: 5
            })
        );
        assert_eq!(block.resolver, Some(Resolver::Heuristic("plaintext")));
        assert!(
            header(&outcome).contains("(3-5 of 5 · via heuristic (plaintext))"),
            "{}",
            header(&outcome)
        );
    }

    #[test]
    fn a_plaintext_miss_is_not_found_and_hands_back_a_regex_target() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "app.vue", "<template><div/></template>\n");

        let outcome = show(&args(&["app.vue#render"]), &global());

        assert!(blocks(&outcome).is_empty());
        let Some(Error::NotFound { what, .. }) = &outcome.error else {
            panic!("expected Error::NotFound, got {:?}", outcome.error);
        };
        assert_eq!(what, "#render (try @'render')");
        assert_eq!(
            outcome.error.as_ref().expect("the terminal error").slug(),
            "not_found"
        );
    }

    #[test]
    fn two_plaintext_hits_are_ambiguous_with_both_candidates() {
        let Some(dir) = repo() else { return };
        write(
            dir.path(),
            "door.kt",
            "fun open() {}\n\nclass Door {\n  fun open() {}\n}\n",
        );

        let outcome = show(&args(&["door.kt#open"]), &global());

        let Some(Error::Ambiguous { candidates, .. }) = &outcome.error else {
            panic!("expected Error::Ambiguous, got {:?}", outcome.error);
        };
        assert_eq!(candidates.iter().map(|c| c.line).collect::<Vec<_>>(), [
            1, 4
        ]);
    }

    #[test]
    fn a_plaintext_qualifier_nothing_encloses_is_not_found_rather_than_the_top_level_name() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "g.kt", GREET_KT);

        let outcome = show(&args(&["g.kt#Nope.greet"]), &global());

        assert!(blocks(&outcome).is_empty());
        let Some(Error::NotFound { what, .. }) = &outcome.error else {
            panic!("expected Error::NotFound, got {:?}", outcome.error);
        };
        assert_eq!(what, "#Nope.greet (try @'greet')");
    }

    #[test]
    fn a_plaintext_qualifier_that_encloses_the_hit_picks_it_out_of_two() {
        let Some(dir) = repo() else { return };
        write(
            dir.path(),
            "door.kt",
            "fun open() {}\n\nclass Door {\n  fun open() {}\n}\n",
        );

        let outcome = show(&args(&["door.kt#Door.open"]), &global());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(
            only(&outcome).span,
            Some(Span {
                start: 4,
                end: 4,
                total: 5
            })
        );
    }

    #[test]
    fn a_grammar_miss_keeps_the_bare_symbol_with_no_regex_hint() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "usage.ts", USAGE_TS);

        let outcome = show(&args(&["usage.ts#render"]), &global());

        let Some(Error::NotFound { what, .. }) = &outcome.error else {
            panic!("expected Error::NotFound, got {:?}", outcome.error);
        };
        assert_eq!(what, "#render");
    }

    fn latin1() -> Vec<u8> {
        let mut bytes = Vec::new();
        for n in 1..=8 {
            bytes.extend_from_slice(format!("line {n}").as_bytes());
            if n == 3 || n == 7 {
                bytes.extend_from_slice(b" caf\xe9");
            }
            bytes.push(b'\n');
        }
        bytes
    }

    #[test]
    fn a_non_utf8_file_renders_lossily_and_its_header_names_the_lines() {
        let Some(dir) = repo() else { return };
        let raw = latin1();
        std::fs::write(dir.path().join("latin1.txt"), &raw).expect("a Latin-1 fixture");

        let outcome = show(&args(&["latin1.txt"]), &global());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let block = only(&outcome);
        assert_eq!(block.lines.len(), 8);
        assert_eq!(block.lines[2].text, "line 3 caf\u{fffd}");
        assert_eq!(block.lossy_lines, [3, 7]);
        let sha = block.sha.as_ref().expect("a read file has a sha");
        assert_eq!(
            sha.as_str(),
            &blake3::hash(&raw).to_hex().as_str()[..12],
            "the sha is the bytes on disk, the one `edit --if` compares"
        );
        assert_eq!(
            header(&outcome),
            format!(
                "── latin1.txt  (1-8 of 8) · non-UTF-8 lines 3, 7 · sha:{}",
                sha.as_str()
            )
        );
    }

    #[test]
    fn only_the_lossy_lines_inside_the_shown_range_are_named() {
        let Some(dir) = repo() else { return };
        std::fs::write(dir.path().join("latin1.txt"), latin1()).expect("a Latin-1 fixture");

        let outcome = show(&args(&["latin1.txt:5-8", "latin1.txt:1-2"]), &global());

        let shown = blocks(&outcome);
        assert_eq!(shown[0].lossy_lines, [7]);
        assert!(shown[1].lossy_lines.is_empty());
    }

    #[test]
    fn a_valid_utf8_file_names_no_lossy_lines() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "cafe.txt", "line 1\nline 2 café\n");

        let outcome = show(&args(&["cafe.txt"]), &global());

        assert!(only(&outcome).lossy_lines.is_empty());
        assert!(
            !header(&outcome).contains("non-UTF-8"),
            "{}",
            header(&outcome)
        );
    }

    #[test]
    fn a_non_utf8_file_with_a_nul_past_the_sniff_window_is_still_binary() {
        // The lossy path checks every byte, not only the window `fs::read` sniffs.
        let Some(dir) = repo() else { return };
        let mut raw = vec![b'a'; 9_000];
        raw.extend_from_slice(b"\xe9\x00\n");
        std::fs::write(dir.path().join("late.bin"), raw).expect("a binary fixture");

        let outcome = show(&args(&["late.bin"]), &global());

        let Some(Error::Unsupported { reason, .. }) = &outcome.error else {
            panic!("expected Error::Unsupported, got {:?}", outcome.error);
        };
        assert!(matches!(reason, UnsupportedReason::Binary), "{reason:?}");
    }

    fn nul_at(at: usize) -> Vec<u8> {
        let mut bytes = vec![b'a'; at];
        bytes.extend_from_slice(b"\0\n");
        bytes
    }

    #[test]
    fn a_nul_on_the_last_byte_of_the_sniff_window_is_binary_to_show() {
        let Some(dir) = repo() else { return };
        let at = crate::fs::BINARY_SNIFF_WINDOW - 1;
        std::fs::write(dir.path().join("edge.dat"), nul_at(at)).expect("a fixture");

        let outcome = show(&args(&["edge.dat"]), &global());

        assert!(
            matches!(
                outcome.error,
                Some(Error::Unsupported {
                    reason: UnsupportedReason::Binary,
                    ..
                })
            ),
            "{:?}",
            outcome.error
        );
    }

    #[test]
    fn a_nul_on_the_first_byte_past_the_sniff_window_is_shown_as_text() {
        let Some(dir) = repo() else { return };
        let at = crate::fs::BINARY_SNIFF_WINDOW;
        std::fs::write(dir.path().join("late.dat"), nul_at(at)).expect("a fixture");

        let outcome = show(&args(&["late.dat"]), &global());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(only(&outcome).lines.len(), 1);
    }

    #[test]
    fn a_utf16_bom_is_binary_not_lossy_text() {
        let Some(dir) = repo() else { return };
        std::fs::write(dir.path().join("wide.txt"), b"\xff\xfeh\xe9llo\n").expect("a fixture");

        let outcome = show(&args(&["wide.txt"]), &global());

        assert!(
            matches!(
                outcome.error,
                Some(Error::Unsupported {
                    reason: UnsupportedReason::Binary,
                    ..
                })
            ),
            "{:?}",
            outcome.error
        );
    }

    #[test]
    fn an_unreadable_file_is_named_as_the_caller_typed_it() {
        let Some(dir) = repo() else { return };
        std::fs::write(dir.path().join("binary.bin"), b"\x00\x01NUL").expect("a binary fixture");

        let outcome = show(&args(&["binary.bin"]), &global());

        let Some(Error::Unsupported { path, .. }) = &outcome.error else {
            panic!("expected Error::Unsupported, got {:?}", outcome.error);
        };
        assert_eq!(path.as_path(), Path::new("binary.bin"));
        assert_eq!(
            outcome
                .error
                .as_ref()
                .expect("the terminal error")
                .to_string(),
            "binary.bin is unsupported: binary file"
        );
    }

    #[test]
    fn a_symbol_the_file_does_not_define_is_not_found() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "usage.ts", USAGE_TS);

        let outcome = show(&args(&["usage.ts#missing"]), &global());

        let Some(Error::NotFound { what, target, .. }) = &outcome.error else {
            panic!("expected Error::NotFound, got {:?}", outcome.error);
        };
        assert_eq!(what, "#missing");
        assert_eq!(target, "usage.ts#missing");
    }

    #[test]
    fn a_path_outside_the_working_tree_is_refused_and_shows_nothing() {
        let Some(_repo) = repo() else { return };
        let outside = TempDir::new().expect("a second, unrelated temp dir");
        write(outside.path(), "elsewhere.md", "secret\n");
        let target = outside
            .path()
            .join("elsewhere.md")
            .to_str()
            .expect("temp paths are UTF-8")
            .to_owned();

        let outcome = show(&args(&[&target]), &global());

        assert!(blocks(&outcome).is_empty());
        assert!(
            matches!(outcome.error, Some(Error::OutsideTree { .. })),
            "{:?}",
            outcome.error
        );
    }

    #[test]
    fn allow_outside_reads_the_same_path() {
        let Some(_repo) = repo() else { return };
        let outside = TempDir::new().expect("a second, unrelated temp dir");
        write(outside.path(), "elsewhere.md", "secret\n");
        let target = outside
            .path()
            .join("elsewhere.md")
            .to_str()
            .expect("temp paths are UTF-8")
            .to_owned();
        let mut global = global();
        global.allow_outside = true;

        let outcome = show(&args(&[&target]), &global);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(only(&outcome).lines.len(), 1);
    }

    fn rendered(outcome: &Outcome, format: Format) -> String {
        crate::output::render(&outcome.response, format, &RenderOptions {
            numbers: true,
            quiet: false,
            cost_first: false,
        })
    }

    #[test]
    fn every_missing_target_is_named_in_the_footer_json_and_stderr() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "good.ts", "a\nb\nc\n");

        let outcome = show(&args(&["nope1.ts", "good.ts", "nope2.ts"]), &global());

        assert_eq!(blocks(&outcome).len(), 1);
        let footer = rendered(&outcome, Format::Text)
            .lines()
            .last()
            .expect("a footer")
            .to_owned();
        assert!(
            footer.starts_with(
                "── showed 1 target · 3 lines · nope1.ts failed (not_found) · nope2.ts failed \
                 (not_found)"
            ),
            "{footer}"
        );
        let json: serde_json::Value =
            serde_json::from_str(&rendered(&outcome, Format::Json)).expect("valid JSON");
        assert_eq!(
            json["omitted"],
            serde_json::json!([
                {"unresolved": {"target": "nope1.ts", "error": "not_found"}},
                {"unresolved": {"target": "nope2.ts", "error": "not_found"}},
            ])
        );
        let error = outcome.error.expect("exit 1");
        assert_eq!(error.slug(), "not_found");
        let stderr = error.to_string();
        assert!(
            stderr.contains("nope1.ts") && stderr.contains("nope2.ts"),
            "{stderr}"
        );
    }

    #[test]
    fn with_every_target_resolved_nothing_is_named_as_failed() {
        let Some(dir) = repo() else { return };
        for name in ["nope1.ts", "good.ts", "nope2.ts"] {
            write(dir.path(), name, "a\n");
        }

        let outcome = show(&args(&["nope1.ts", "good.ts", "nope2.ts"]), &global());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(outcome.response.omitted.is_empty());
        assert!(!rendered(&outcome, Format::Text).contains("failed"));
    }

    #[test]
    fn a_budget_trim_under_a_window_names_the_range_the_budget_cut() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "big.txt", &numbered(450));
        let mut global = global();
        global.budget = Some(100);

        let outcome = show(&args(&["big.txt"]), &global);

        let last = blocks(&outcome)[0]
            .lines
            .last()
            .expect("a trimmed target keeps a line")
            .number;
        assert!(last < 200, "the budget cut inside the window: {last}");
        let text = rendered(&outcome, Format::Text);
        let footer = text.lines().last().expect("a footer");
        assert!(
            footer.contains(":201-450 not shown")
                && footer.contains(&format!(
                    "budget 100 trimmed big.txt (:{}-200 not shown)",
                    last + 1
                )),
            "{footer}"
        );
    }

    #[test]
    fn with_no_budget_the_footer_names_only_the_window() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "big.txt", &numbered(450));

        let outcome = show(&args(&["big.txt"]), &global());

        let text = rendered(&outcome, Format::Text);
        let footer = text.lines().last().expect("a footer");
        assert!(footer.contains(":201-450 not shown"), "{footer}");
        assert!(!footer.contains("budget"), "{footer}");
    }

    #[test]
    fn a_failing_second_target_still_leaves_the_first_one_shown() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "a.md", &numbered(3));

        let outcome = show(&args(&["a.md", "missing.md"]), &global());

        let shown = blocks(&outcome);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].target, "a.md");
        assert_eq!(shown[0].lines.len(), 3);
        assert!(
            matches!(outcome.error, Some(Error::Io { .. })),
            "{:?}",
            outcome.error
        );
        assert_eq!(outcome.response.footer.summary, "showed 1 target · 3 lines");
    }

    #[test]
    fn every_failing_target_is_the_call_s_error_in_argument_order() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "a.md", &numbered(3));
        let outside = TempDir::new().expect("a second, unrelated temp dir");
        write(outside.path(), "elsewhere.md", "secret\n");
        let escaped = outside
            .path()
            .join("elsewhere.md")
            .to_str()
            .expect("temp paths are UTF-8")
            .to_owned();

        let outcome = show(&args(&["a.md", "a.md:900", &escaped]), &global());

        assert_eq!(blocks(&outcome).len(), 1);
        match &outcome.error {
            Some(Error::Several { errors }) => assert!(
                matches!(errors.as_slice(), [
                    Error::NotFound { .. },
                    Error::OutsideTree { .. }
                ]),
                "the line target precedes the escaped path: {errors:?}"
            ),
            other => panic!("expected both failures, got {other:?}"),
        }
        let error = outcome.error.as_ref().expect("a failure");
        assert_eq!(error.slug(), "not_found", "the first failure sets the slug");
    }

    #[test]
    fn every_target_failing_leaves_no_response_body_at_all() {
        let Some(_dir) = repo() else { return };

        let outcome = show(&args(&["gone.md", "also-gone.md"]), &global());

        assert!(!outcome.response.has_output(), "nothing resolved");
        let error = outcome.error.expect("a failure");
        assert!(
            matches!(&error, Error::Several { errors } if errors.len() == 2),
            "{error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains("gone.md:") && message.contains("also-gone.md:"),
            "stderr names both: {message}"
        );
    }

    #[test]
    fn a_line_target_widens_only_with_its_own_context_flags() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "a.md", &numbered(20));
        let mut args = args(&["a.md:10"]);
        args.context = Some(2);

        let outcome = show(&args, &global());

        assert_eq!(
            only(&outcome).span,
            Some(Span {
                start: 8,
                end: 12,
                total: 20
            })
        );

        let bare = show(&self::args(&["a.md:10"]), &global());
        assert_eq!(
            only(&bare).span,
            Some(Span {
                start: 10,
                end: 10,
                total: 20
            })
        );
    }

    #[test]
    fn before_and_after_override_the_symmetric_context_flag() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "a.md", &numbered(20));
        let mut args = args(&["a.md:10"]);
        args.context = Some(5);
        args.before = Some(1);
        args.after = Some(2);

        let outcome = show(&args, &global());

        assert_eq!(
            only(&outcome).span,
            Some(Span {
                start: 9,
                end: 12,
                total: 20
            })
        );
    }

    #[test]
    fn context_flags_do_not_widen_a_range_or_a_whole_file() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "a.md", &numbered(20));
        let mut args = args(&["a.md:5-6", "a.md"]);
        args.context = Some(4);

        let outcome = show(&args, &global());

        let shown = blocks(&outcome);
        assert_eq!(
            shown[0].span.map(|span| (span.start, span.end)),
            Some((5, 6))
        );
        assert_eq!(
            shown[1].span.map(|span| (span.start, span.end)),
            Some((1, 20))
        );
    }

    #[test]
    fn a_range_past_the_last_line_is_not_found_rather_than_an_empty_block() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "a.md", &numbered(20));

        let outcome = show(&args(&["a.md:30-40"]), &global());

        assert!(blocks(&outcome).is_empty());
        let Some(Error::NotFound { what, .. }) = &outcome.error else {
            panic!("expected Error::NotFound, got {:?}", outcome.error);
        };
        assert_eq!(what, ":30-40");
    }

    #[test]
    fn a_range_overlapping_the_end_clamps_to_the_last_line() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "a.md", &numbered(20));

        let outcome = show(&args(&["a.md:18-40"]), &global());

        assert_eq!(
            only(&outcome).span,
            Some(Span {
                start: 18,
                end: 20,
                total: 20
            })
        );
    }

    #[test]
    fn an_empty_file_resolves_to_a_block_with_no_lines() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "empty.md", "");

        let outcome = show(&args(&["empty.md"]), &global());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let block = only(&outcome);
        assert_eq!(block.span, None);
        assert!(block.lines.is_empty());
        assert!(outcome.response.omitted.is_empty());
        let sha = block.sha.as_ref().expect("a read file has a sha");
        assert_eq!(sha.as_str(), &blake3::hash(b"").to_hex().as_str()[..12]);
        assert_eq!(
            rendered(&outcome, Format::Text),
            format!(
                "── empty.md · sha:{}\n── showed 1 target · 0 lines\n",
                sha.as_str()
            )
        );
    }

    #[test]
    fn a_file_holding_one_blank_line_resolves_through_the_ordinary_window() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "blank.md", "\n");

        let outcome = show(&args(&["blank.md"]), &global());

        let block = only(&outcome);
        assert_eq!(
            block.span,
            Some(Span {
                start: 1,
                end: 1,
                total: 1
            })
        );
        assert_eq!(block.lines.len(), 1);
        assert_eq!(block.lines[0].text, "");
    }

    #[test]
    fn a_line_target_in_an_empty_file_is_still_not_found() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "empty.md", "");

        let outcome = show(&args(&["empty.md:1"]), &global());

        assert!(
            matches!(&outcome.error, Some(Error::NotFound { what, .. }) if what == ":1"),
            "{:?}",
            outcome.error
        );
    }

    #[test]
    fn a_regex_target_centres_on_its_match_and_names_the_pattern_when_it_misses() {
        let Some(dir) = repo() else { return };
        write(
            dir.path(),
            "compose.ts",
            "import x from 'y'\n\nexport async function compose(req: Request) {\n  return x\n}\n",
        );
        let mut args = args(&["compose.ts@'export async function compose'"]);
        args.after = Some(2);

        let outcome = show(&args, &global());

        assert_eq!(
            only(&outcome).span,
            Some(Span {
                start: 3,
                end: 5,
                total: 5
            })
        );

        let missed = show(&self::args(&["compose.ts@'no such line'"]), &global());
        let Some(Error::NotFound { what, .. }) = &missed.error else {
            panic!("expected Error::NotFound, got {:?}", missed.error);
        };
        assert_eq!(what, "@'no such line'");
    }

    #[test]
    fn an_uncompilable_pattern_says_so_instead_of_reading_as_absent() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "a.md", &numbered(3));

        let outcome = show(&args(&["a.md@'(unclosed'"]), &global());

        let Some(Error::NotFound { what, .. }) = &outcome.error else {
            panic!("expected Error::NotFound, got {:?}", outcome.error);
        };
        assert_eq!(what, "@'(unclosed' (invalid regex)");
    }

    #[test]
    fn a_budget_trims_the_largest_target_first_and_names_it_with_the_budget() {
        // A 20-token budget is 80 bytes: room for the 4-line target, not the 40-line one.
        let Some(dir) = repo() else { return };
        write(dir.path(), "big.md", &numbered(40));
        write(dir.path(), "small.md", &numbered(4));
        let mut global = global();
        global.budget = Some(20);

        let outcome = show(&args(&["small.md", "big.md"]), &global);

        let shown = blocks(&outcome);
        assert_eq!(shown[0].target, "small.md");
        assert_eq!(shown[0].lines.len(), 4, "the smaller target is untouched");
        assert_eq!(shown[0].not_shown, None);
        assert!(
            shown[1].lines.len() < 40,
            "the larger target is the one trimmed"
        );
        let last = shown[1]
            .lines
            .last()
            .expect("a trimmed target keeps at least one line")
            .number;
        assert_eq!(shown[1].span.map(|span| span.end), Some(last));
        assert_eq!(shown[1].not_shown, Some((last + 1, 40)));
        assert!(
            matches!(
                outcome.response.omitted.as_slice(),
                [Omission::Budget { budget: 20, trimmed_target, not_shown: Some(cut) }]
                    if trimmed_target == "big.md" && *cut == (last + 1, 40)
            ),
            "{:?}",
            outcome.response.omitted
        );
        assert_eq!(
            outcome.response.stats.bytes,
            window::content_bytes(&shown[0].lines) + window::content_bytes(&shown[1].lines),
            "the cost line counts what survived the trim"
        );
        assert!(outcome.error.is_none(), "a trim is not a failure");
    }

    #[test]
    fn a_budget_the_answer_already_fits_trims_nothing() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "big.md", &numbered(40));
        write(dir.path(), "small.md", &numbered(4));
        let mut global = global();
        global.budget = Some(5000);

        let outcome = show(&args(&["small.md", "big.md"]), &global);

        assert_eq!(blocks(&outcome)[1].lines.len(), 40);
        assert!(outcome.response.omitted.is_empty());
    }

    #[test]
    fn content_over_max_bytes_with_no_budget_is_refused_whole() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "a.md", &numbered(40));
        let mut global = global();
        global.max_bytes = 100;

        let outcome = show(&args(&["a.md"]), &global);

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
    fn a_budget_takes_max_bytes_out_of_the_decision() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "a.md", &numbered(40));
        let mut global = global();
        global.max_bytes = 100;
        global.budget = Some(20);

        let outcome = show(&args(&["a.md"]), &global);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert!(!blocks(&outcome).is_empty());
    }

    #[test]
    fn a_file_over_max_file_bytes_is_unsupported() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "a.md", &numbered(40));
        let mut global = global();
        global.max_file_bytes = 10;

        let outcome = show(&args(&["a.md"]), &global);

        assert!(
            matches!(outcome.error, Some(Error::Unsupported { .. })),
            "{:?}",
            outcome.error
        );
    }

    #[test]
    fn three_targets_pluralise_the_summary_and_sum_their_lines() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "a.md", &numbered(3));
        write(dir.path(), "b.md", &numbered(4));

        let outcome = show(&args(&["a.md", "b.md", "a.md:1-2"]), &global());

        assert_eq!(
            outcome.response.footer.summary,
            "showed 3 targets · 9 lines"
        );
        assert_eq!(outcome.response.stats.lines, 9);
    }

    #[test]
    fn a_window_that_narrows_a_target_to_nothing_is_still_named_in_the_footer() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "a.md", &numbered(20));
        let mut args = args(&["a.md:1", "a.md"]);
        args.window = 0;

        let outcome = show(&args, &global());

        assert_eq!(blocks(&outcome).len(), 1, "the line target still resolves");
        assert!(
            matches!(outcome.response.omitted.as_slice(), [
                Omission::Window { total: 20, .. },
                Omission::Unresolved { target, error: "not_found" },
            ] if target == "a.md"),
            "a narrowing the footer does not name did not happen: {:?}",
            outcome.response.omitted
        );
        assert!(matches!(outcome.error, Some(Error::NotFound { .. })));
    }

    /// tree-sitter-toml ends the last table on the row after the file's last line.
    const LAST_TABLE_TOML: &str = "[server]\nport = 8080\n\n[client]\nretries = 3\n";

    #[test]
    fn a_symbol_ending_past_the_last_line_is_clamped_to_the_file() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "config.toml", LAST_TABLE_TOML);

        let outcome = show(&args(&["config.toml#client"]), &global());

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let block = only(&outcome);
        let span = block.span.expect("a resolved target names its span");
        assert_eq!(span.total, 5, "the fixture has five lines");
        assert_eq!(span, Span {
            start: 4,
            end: 5,
            total: 5
        });
        assert!(
            span.end <= span.total,
            "a span may never name a line the file lacks: {span:?}"
        );
        assert_eq!(
            block.lines.len(),
            span.end + 1 - span.start,
            "the header's range and the rendered lines must agree"
        );
        assert_eq!(block.lines.last().map(|line| line.number), Some(5));
        assert!(
            header(&outcome).contains("(4-5 of 5 · via tree-sitter)"),
            "{}",
            header(&outcome)
        );
    }

    #[test]
    fn two_targets_that_both_overflow_the_budget_are_each_named_once() {
        // A 20-token budget is 80 bytes, so trimming one 40-line target still leaves the pair over.
        let Some(dir) = repo() else { return };
        write(dir.path(), "a.md", &numbered(40));
        write(dir.path(), "b.md", &numbered(40));
        let mut global = global();
        global.budget = Some(20);

        let outcome = show(&args(&["a.md", "b.md"]), &global);

        let shown = blocks(&outcome);
        assert!(
            shown[0].lines.len() < 40 && shown[1].lines.len() < 40,
            "{shown:?}"
        );
        let trimmed: Vec<&str> = outcome
            .response
            .omitted
            .iter()
            .filter_map(|omission| match omission {
                Omission::Budget {
                    budget: 20,
                    trimmed_target,
                    ..
                } => Some(trimmed_target.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(trimmed, ["a.md", "b.md"], "{:?}", outcome.response.omitted);
        assert_eq!(outcome.response.omitted.len(), 2);
        for block in shown {
            let last = block
                .lines
                .last()
                .expect("a trimmed target keeps a line")
                .number;
            assert_eq!(block.not_shown, Some((last + 1, 40)));
        }
    }

    #[test]
    fn the_same_call_answers_byte_identically_twice() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "a.md", &numbered(20));
        let opts = RenderOptions {
            numbers: true,
            quiet: false,
            cost_first: false,
        };

        let once = crate::output::render(
            &show(&args(&["a.md", "a.md#missing"]), &global()).response,
            Format::Text,
            &opts,
        );
        let twice = crate::output::render(
            &show(&args(&["a.md", "a.md#missing"]), &global()).response,
            Format::Text,
            &opts,
        );

        assert_eq!(once, twice);
    }

    fn named(outcome: &Outcome) -> Vec<String> {
        outcome
            .response
            .omitted
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn a_line_over_the_display_cap_is_cut_and_named_instead_of_refused() {
        // Uncut, the 5,000-byte line alone would exceed `--max-bytes 2000` and refuse with exit 4.
        let Some(dir) = repo() else { return };
        write(
            dir.path(),
            "min.js",
            &format!("{}\nshort\n", "x".repeat(5_000)),
        );
        let mut global = global();
        global.max_bytes = 2_000;

        let outcome = show(&args(&["min.js"]), &global);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let block = only(&outcome);
        assert_eq!(
            block.lines[0].text,
            format!("{}\u{2026}", "x".repeat(1_000))
        );
        assert_eq!(block.lines[1].text, "short");
        assert_eq!(named(&outcome), ["1 long line cut"]);
    }

    #[test]
    fn lines_within_the_display_cap_are_shown_whole_and_no_cut_is_named() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "a.js", &format!("{}\n", "x".repeat(900)));

        let outcome = show(&args(&["a.js"]), &global());

        assert_eq!(only(&outcome).lines[0].text, "x".repeat(900));
        assert!(named(&outcome).is_empty(), "{:?}", named(&outcome));
    }

    #[test]
    fn a_comma_range_on_a_real_file_names_the_range_form() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "a.ts", &numbered(80));

        let outcome = show(&args(&["a.ts:40,60"]), &global());

        let error = outcome.error.as_ref().expect("the target names no file");
        assert_eq!(error.slug(), "not_found");
        assert_eq!(
            error.to_string(),
            "a.ts:40,60: no such file \u{b7} did you mean a.ts:40-60"
        );
    }

    #[test]
    fn an_unquoted_regex_on_a_real_file_names_the_quoted_form() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "a.ts", "const cap = 10;\n");

        let outcome = show(&args(&["a.ts@cap"]), &global());

        assert_eq!(
            outcome.error.as_ref().map(ToString::to_string).as_deref(),
            Some("a.ts@cap: no such file \u{b7} did you mean \"a.ts@'cap'\"")
        );
    }

    #[test]
    fn a_missing_file_with_no_real_prefix_keeps_the_plain_not_found() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "a.ts", &numbered(80));

        let outcome = show(&args(&["nope.ts:40,60", "nope.ts"]), &global());

        let Some(Error::Several { errors }) = &outcome.error else {
            panic!("expected both targets to fail, got {:?}", outcome.error);
        };
        for error in errors {
            assert!(
                matches!(error, Error::Io { .. }),
                "a plain not-found, not a hint: {error:?}"
            );
            assert!(!error.to_string().contains("did you mean"), "{error}");
        }
    }

    #[test]
    fn a_file_whose_endings_are_mostly_crlf_says_so_in_its_header() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "win.txt", "one\r\ntwo\r\nthree\n");

        let outcome = show(&args(&["win.txt"]), &global());

        assert!(only(&outcome).crlf);
        assert!(
            header(&outcome).starts_with("── win.txt  (1-3 of 3) \u{b7} crlf \u{b7} sha:"),
            "{}",
            header(&outcome)
        );
        let texts: Vec<&str> = only(&outcome)
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect();
        assert_eq!(texts, ["one", "two", "three"]);
    }

    #[test]
    fn a_file_whose_endings_are_mostly_lf_names_no_crlf() {
        let Some(dir) = repo() else { return };
        write(dir.path(), "unix.txt", "one\ntwo\n");
        write(dir.path(), "mixed.txt", "one\r\ntwo\nthree\n");

        let outcome = show(&args(&["unix.txt", "mixed.txt"]), &global());

        for block in blocks(&outcome) {
            assert!(!block.crlf, "{}", block.target);
        }
        assert!(!header(&outcome).contains("crlf"), "{}", header(&outcome));
    }
}
