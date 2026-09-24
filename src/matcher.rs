//! `--old` is matched as exact bytes; the normalise table is a fallback the caller opts into,
//! and exact and folded results are never mixed in one answer.

use std::path::Path;

use crate::error::{CANDIDATE_CAP, Candidate};
use crate::normalize;

// A candidate row is a diagnostic snippet: one hit on a multi-megabyte minified line must not
// copy the whole line (measured: 540,000 hits on a 1 MiB line).
const CANDIDATE_TEXT_CAP_BYTES: usize = 1024;

// A single-line file has no prior best to seed a ceiling, so its one line would run the full
// O(probe × line) table (measured: a 2 MB line, 641 ms unoptimized).
const NEAREST_LINE_COMPARE_CAP_BYTES: usize = 1024;

/// Always in *original* bytes: a folded match is mapped back before it becomes a `Span`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct FindOptions {
    pub all: bool,
    pub normalize: bool,
}

/// `normalized` means folding produced this match, not merely that `--normalize` was passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Located {
    pub span: Span,
    pub normalized: bool,
}

#[derive(Debug)]
pub struct Nearest {
    pub candidate: Candidate,
    pub distance: usize,
}

#[derive(Debug)]
pub enum Found {
    Unique(Located),
    All(Vec<Located>),
    Ambiguous(Vec<Candidate>),
    NotFound {
        nearest: Option<Nearest>,
        normalized_hint: Option<Span>,
    },
}

pub fn find(path: &Path, haystack: &[u8], needle: &[u8], opts: FindOptions) -> Found {
    let exact = find_all_exact(haystack, needle);
    if !exact.is_empty() {
        return resolve(path, haystack, &exact, false, opts.all);
    }
    if opts.normalize {
        let folded = find_all_folded(haystack, needle);
        if !folded.is_empty() {
            return resolve(path, haystack, &folded, true, opts.all);
        }
    }
    Found::NotFound {
        nearest: nearest_line(path, haystack, needle),
        normalized_hint: if opts.normalize {
            None
        } else {
            find_all_folded(haystack, needle).first().copied()
        },
    }
}

/// Bytes outside the span are copied verbatim, so CRLF, BOM and every untouched byte survive by
/// construction rather than by a check.
pub fn splice(haystack: &[u8], span: Span, replacement: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len() - (span.end - span.start) + replacement.len());
    out.extend_from_slice(&haystack[..span.start]);
    out.extend_from_slice(replacement);
    out.extend_from_slice(&haystack[span.end..]);
    out
}

pub fn line_of(haystack: &[u8], offset: usize) -> usize {
    haystack
        .iter()
        .take(offset)
        .filter(|&&b| b == b'\n')
        .count()
        + 1
}

fn resolve(path: &Path, haystack: &[u8], spans: &[Span], normalized: bool, all: bool) -> Found {
    if all {
        return Found::All(
            spans
                .iter()
                .map(|&span| Located { span, normalized })
                .collect(),
        );
    }
    match spans {
        [span] => Found::Unique(Located {
            span: *span,
            normalized,
        }),
        many => Found::Ambiguous(build_ambiguous(path, haystack, many)),
    }
}

/// One forward pass: a `candidate_at` scan per match is O(offset) each, which turned 540,000
/// hits on a 1 MiB line into a multi-minute hang.
fn build_ambiguous(path: &Path, haystack: &[u8], spans: &[Span]) -> Vec<Candidate> {
    let mut cursor = LineCursor::new();
    spans
        .iter()
        .enumerate()
        .map(|(index, span)| {
            let (line, line_start) = cursor.advance(haystack, span.start);
            // Rows past `CANDIDATE_CAP` render as `(+N more)`, so their text is never built.
            let text = if index < CANDIDATE_CAP {
                line_text(haystack, line_start, span.start, CANDIDATE_TEXT_CAP_BYTES)
            } else {
                String::new()
            };
            Candidate {
                path: path.to_path_buf(),
                line,
                text,
            }
        })
        .collect()
}

struct LineCursor {
    scanned: usize,
    line: usize,
    line_start: usize,
}

impl LineCursor {
    fn new() -> Self {
        Self {
            scanned: 0,
            line: 1,
            line_start: 0,
        }
    }

    /// `offset` must be `>=` every prior `offset` passed to this cursor.
    fn advance(&mut self, haystack: &[u8], offset: usize) -> (usize, usize) {
        for (i, &b) in haystack[self.scanned..offset].iter().enumerate() {
            if b == b'\n' {
                self.line += 1;
                self.line_start = self.scanned + i + 1;
            }
        }
        self.scanned = offset;
        (self.line, self.line_start)
    }
}

/// Bounded to `cap` bytes, so one huge line costs fixed work however many candidates land on it.
fn line_text(haystack: &[u8], line_start: usize, offset: usize, cap: usize) -> String {
    let cap_end = (line_start + cap).min(haystack.len());
    let search_from = offset.min(cap_end);
    let mut end = haystack[search_from..cap_end]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(cap_end, |newline| search_from + newline);
    // A CRLF file's line ends `\r\n`; without this the CR prints bare into the candidate row.
    if end > line_start && haystack[end - 1] == b'\r' {
        end -= 1;
    }
    String::from_utf8_lossy(&haystack[line_start..end]).into_owned()
}

/// Non-overlapping. An empty needle matches nothing: `--old` has no empty-string case.
fn find_all_exact(haystack: &[u8], needle: &[u8]) -> Vec<Span> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut from = 0;
    while from + needle.len() <= haystack.len() {
        let Some(offset) = haystack[from..]
            .windows(needle.len())
            .position(|window| window == needle)
        else {
            break;
        };
        let start = from + offset;
        spans.push(Span {
            start,
            end: start + needle.len(),
        });
        from = start + needle.len();
    }
    spans
}

/// The needle is never folded: file content is folded against the string the agent typed.
fn find_all_folded(haystack: &[u8], needle: &[u8]) -> Vec<Span> {
    if !normalize::contains_foldable(haystack) {
        return Vec::new();
    }
    // Non-UTF-8 input has no folded form: the fallback is unavailable, not an error.
    let (Ok(text), Ok(_)) = (std::str::from_utf8(haystack), std::str::from_utf8(needle)) else {
        return Vec::new();
    };
    let folded = normalize::fold(text);
    find_all_exact(folded.text.as_bytes(), needle)
        .into_iter()
        .map(|span| {
            let (start, end) = folded.original_span(span.start, span.end);
            Span { start, end }
        })
        .collect()
}

fn nearest_line(path: &Path, haystack: &[u8], needle: &[u8]) -> Option<Nearest> {
    if haystack.is_empty() {
        return None;
    }
    let probe = match needle.iter().position(|&b| b == b'\n') {
        Some(end) => &needle[..end],
        None => needle,
    };

    let mut best: Option<Nearest> = None;
    let mut offset = 0;
    for line in haystack.split(|&b| b == b'\n') {
        // Skips the phantom line `split` yields after a final `\n`; nearest names a real line.
        if line.is_empty() {
            offset += 1;
            continue;
        }
        // Distance is at least the length difference, so a line that cannot win is skipped whole.
        let floor = probe.len().abs_diff(line.len());
        if best.as_ref().is_none_or(|nearest| floor < nearest.distance) {
            let bounded_line = &line[..line.len().min(NEAREST_LINE_COMPARE_CAP_BYTES)];
            let ceiling = best.as_ref().map_or(usize::MAX, |nearest| nearest.distance);
            let distance = levenshtein(probe, bounded_line, ceiling);
            if best
                .as_ref()
                .is_none_or(|nearest| distance < nearest.distance)
            {
                best = Some(Nearest {
                    candidate: candidate_at(path, haystack, offset),
                    distance,
                });
            }
        }
        offset += line.len() + 1;
    }
    best
}

/// Any path through a cell costs at least `|i-j|`, so once a row's minimum reaches `ceiling` no
/// completion can beat it, and the caller never needs the exact value past that point.
fn levenshtein(a: &[u8], b: &[u8], ceiling: usize) -> usize {
    if a.is_empty() {
        return b.len();
    }
    let mut previous: Vec<usize> = (0..=a.len()).collect();
    let mut current = vec![0; a.len() + 1];
    for &bb in b {
        current[0] = previous[0] + 1;
        let mut row_min = current[0];
        for (j, &ab) in a.iter().enumerate() {
            current[j + 1] = (previous[j] + usize::from(ab != bb))
                .min(previous[j + 1] + 1)
                .min(current[j] + 1);
            row_min = row_min.min(current[j + 1]);
        }
        if row_min >= ceiling {
            return ceiling;
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[a.len()]
}

/// Both boundary scans stop at `CANDIDATE_TEXT_CAP_BYTES`, so one 2 MB line costs fixed work.
fn candidate_at(path: &Path, haystack: &[u8], offset: usize) -> Candidate {
    let offset = offset.min(haystack.len());
    let scan_start = offset.saturating_sub(CANDIDATE_TEXT_CAP_BYTES);
    let start = haystack[scan_start..offset]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(scan_start, |newline| scan_start + newline + 1);
    let scan_end = (offset + CANDIDATE_TEXT_CAP_BYTES).min(haystack.len());
    let mut end = haystack[offset..scan_end]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(scan_end, |newline| offset + newline);
    if end > start && haystack[end - 1] == b'\r' {
        end -= 1;
    }
    Candidate {
        path: path.to_path_buf(),
        line: line_of(haystack, offset),
        text: String::from_utf8_lossy(&haystack[start..end]).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const EXACT: FindOptions = FindOptions {
        all: false,
        normalize: false,
    };
    const EXACT_ALL: FindOptions = FindOptions {
        all: true,
        normalize: false,
    };
    const FOLDING: FindOptions = FindOptions {
        all: false,
        normalize: true,
    };

    fn file_with(placed: &[(usize, &str)], total: usize) -> String {
        let mut lines: Vec<String> = (1..=total).map(|n| format!("// filler {n}")).collect();
        for &(line, text) in placed {
            lines[line - 1] = text.to_owned();
        }
        let mut out = lines.join("\n");
        out.push('\n');
        out
    }

    fn usage_ts() -> String {
        file_with(&[(42, "const cap = 10")], 61)
    }

    fn notes_md() -> String {
        file_with(
            &[(
                17,
                "In that case the agent \u{2013} not the user \u{2013} decides.",
            )],
            20,
        )
    }

    #[test]
    fn a_single_exact_occurrence_is_unique_at_its_original_bytes() {
        let haystack = usage_ts();
        let bytes = haystack.as_bytes();

        let found = find(
            Path::new("src/store/usage.ts"),
            bytes,
            b"const cap = 10",
            EXACT,
        );

        let Found::Unique(located) = found else {
            panic!("expected Unique, got {found:?}")
        };
        assert_eq!(line_of(bytes, located.span.start), 42);
        assert!(!located.normalized);
        assert_eq!(
            &bytes[located.span.start..located.span.end],
            b"const cap = 10"
        );

        let edited = splice(bytes, located.span, b"const cap = 20");
        let edited = String::from_utf8(edited).unwrap();
        assert_eq!(edited.lines().nth(41).unwrap(), "const cap = 20");
        assert_eq!(edited.lines().count(), haystack.lines().count());
    }

    #[test]
    fn three_occurrences_without_all_are_ambiguous_with_one_candidate_per_line() {
        let haystack = file_with(
            &[
                (43, "  if (n > cap) return"),
                (57, "  if (!id) return"),
                (61, "  return total"),
            ],
            61,
        );

        let found = find(
            Path::new("src/store/usage.ts"),
            haystack.as_bytes(),
            b"return",
            EXACT,
        );

        let Found::Ambiguous(candidates) = found else {
            panic!("expected Ambiguous, got {found:?}")
        };
        assert_eq!(candidates.iter().map(|c| c.line).collect::<Vec<_>>(), vec![
            43, 57, 61
        ]);
        assert_eq!(candidates[0].text, "  if (n > cap) return");
        assert_eq!(candidates[1].text, "  if (!id) return");
        assert_eq!(candidates[2].text, "  return total");
        assert!(
            candidates
                .iter()
                .all(|c| c.path == Path::new("src/store/usage.ts"))
        );
    }

    #[test]
    fn all_returns_every_occurrence_in_file_order() {
        let haystack = file_with(
            &[
                (12, "import { usageCap } from './config'"),
                (42, "  const cap = usageCap"),
                (57, "  if (usageCap > 0) return"),
                (88, "export default usageCap"),
            ],
            88,
        );
        let bytes = haystack.as_bytes();

        let found = find(
            Path::new("src/store/usage.ts"),
            bytes,
            b"usageCap",
            EXACT_ALL,
        );

        let Found::All(located) = found else {
            panic!("expected All, got {found:?}")
        };
        assert_eq!(
            located
                .iter()
                .map(|l| line_of(bytes, l.span.start))
                .collect::<Vec<_>>(),
            vec![12, 42, 57, 88]
        );
        assert!(located.iter().all(|l| !l.normalized));
        assert!(
            located
                .iter()
                .all(|l| &bytes[l.span.start..l.span.end] == b"usageCap")
        );
    }

    #[test]
    fn all_over_a_single_occurrence_still_returns_a_one_element_list() {
        let haystack = usage_ts();

        let found = find(
            Path::new("src/store/usage.ts"),
            haystack.as_bytes(),
            b"const cap = 10",
            EXACT_ALL,
        );

        let Found::All(located) = found else {
            panic!("expected All, got {found:?}")
        };
        assert_eq!(located.len(), 1);
    }

    #[test]
    fn occurrences_never_overlap() {
        let found = find(Path::new("f"), b"aaaa", b"aa", EXACT_ALL);

        let Found::All(located) = found else {
            panic!("expected All, got {found:?}")
        };
        assert_eq!(
            located
                .iter()
                .map(|l| (l.span.start, l.span.end))
                .collect::<Vec<_>>(),
            vec![(0, 2), (2, 4)]
        );
    }

    #[test]
    fn a_miss_names_the_nearest_line() {
        let haystack = usage_ts();

        let found = find(
            Path::new("src/store/usage.ts"),
            haystack.as_bytes(),
            b"const cap = 15",
            EXACT,
        );

        let Found::NotFound {
            nearest: Some(nearest),
            normalized_hint,
        } = found
        else {
            panic!("expected NotFound with a nearest line, got {found:?}")
        };
        assert_eq!(nearest.candidate.line, 42);
        assert_eq!(nearest.candidate.text, "const cap = 10");
        assert_eq!(nearest.distance, 1);
        assert!(normalized_hint.is_none());
    }

    #[test]
    fn two_lines_equally_near_keep_the_lower_line_number() {
        // The agent re-runs against the candidate, so a tie must resolve the same way each time.
        let haystack = file_with(&[(12, "const cap = 10"), (42, "const cap = 16")], 61);

        let found = find(
            Path::new("src/store/usage.ts"),
            haystack.as_bytes(),
            b"const cap = 15",
            EXACT,
        );

        let Found::NotFound {
            nearest: Some(nearest),
            ..
        } = found
        else {
            panic!("expected NotFound with a nearest line, got {found:?}")
        };
        assert_eq!(nearest.distance, 1);
        assert_eq!(nearest.candidate.line, 12);
        assert_eq!(nearest.candidate.text, "const cap = 10");
    }

    #[test]
    fn a_miss_in_a_file_with_no_bytes_names_no_nearest_line() {
        let found = find(Path::new("empty.ts"), b"", b"const cap = 10", EXACT);

        assert!(matches!(found, Found::NotFound {
            nearest: None,
            normalized_hint: None,
        }));
    }

    #[test]
    fn a_miss_in_an_unrelated_file_still_names_its_closest_line() {
        let haystack = "package main\n\nfunc main() {}\n";

        let found = find(
            Path::new("main.go"),
            haystack.as_bytes(),
            b"const cap = 15",
            EXACT,
        );

        let Found::NotFound {
            nearest: Some(nearest),
            ..
        } = found
        else {
            panic!("expected NotFound with a nearest line, got {found:?}")
        };
        assert_eq!(nearest.candidate.text, "func main() {}");
        assert!(nearest.distance > 1);
    }

    #[test]
    fn nearest_line_can_exceed_probe_length_when_nothing_is_close() {
        // No line is within `probe.len()` edits, and `nearest` still names the least-bad one.
        let haystack = "abcdefgh\nijklmnop\n";

        let found = find(Path::new("f"), haystack.as_bytes(), b"xxx", EXACT);

        let Found::NotFound {
            nearest: Some(nearest),
            ..
        } = found
        else {
            panic!("expected NotFound with a nearest line, got {found:?}")
        };
        assert!(nearest.distance > b"xxx".len());
    }

    #[test]
    fn a_trailing_newline_never_yields_the_phantom_empty_line_as_nearest() {
        // The phantom line past EOF is nearer than any of these long lines.
        let haystack = "  const somethingRatherLongHere1 = computeTheValue(1, options);\n\
                         const somethingRatherLongHere2 = computeTheValue(2, options);\n";
        let real_line_count = haystack.lines().count();

        let found = find(Path::new("f"), haystack.as_bytes(), b"cap", EXACT);

        let Found::NotFound {
            nearest: Some(nearest),
            ..
        } = found
        else {
            panic!("expected NotFound with a nearest line, got {found:?}")
        };
        assert!(nearest.candidate.line <= real_line_count);
        assert!(!nearest.candidate.text.is_empty());
    }

    #[test]
    fn a_trailing_newline_does_not_shift_reported_line_numbers() {
        let haystack = "aaa\nbbb\nccc\n";
        let real_line_count = haystack.lines().count();

        let found = find(Path::new("f"), haystack.as_bytes(), b"ccx", EXACT);

        let Found::NotFound {
            nearest: Some(nearest),
            ..
        } = found
        else {
            panic!("expected NotFound with a nearest line, got {found:?}")
        };
        assert_eq!(nearest.candidate.line, 3);
        assert_eq!(nearest.candidate.text, "ccc");
        assert!(nearest.candidate.line <= real_line_count);
    }

    #[test]
    fn an_empty_needle_matches_nothing() {
        let haystack = usage_ts();

        let found = find(
            Path::new("src/store/usage.ts"),
            haystack.as_bytes(),
            b"",
            EXACT_ALL,
        );

        assert!(matches!(found, Found::NotFound { .. }));
    }

    #[test]
    fn a_needle_longer_than_the_file_matches_nothing() {
        let found = find(Path::new("f"), b"cap", b"const cap = 10", EXACT);

        assert!(matches!(found, Found::NotFound { .. }));
    }

    #[test]
    fn an_exact_miss_over_smart_punctuation_hints_at_the_folded_span() {
        let haystack = notes_md();
        let bytes = haystack.as_bytes();

        let found = find(
            Path::new("docs/notes.md"),
            bytes,
            b"the agent - not the user - decides",
            EXACT,
        );

        let Found::NotFound {
            normalized_hint: Some(hint),
            ..
        } = found
        else {
            panic!("expected NotFound with a normalized hint, got {found:?}")
        };
        assert_eq!(line_of(bytes, hint.start), 17);
        assert_eq!(
            &bytes[hint.start..hint.end],
            "the agent \u{2013} not the user \u{2013} decides".as_bytes()
        );
    }

    #[test]
    fn the_same_needle_with_normalize_matches_and_splices_only_the_folded_span() {
        let haystack = notes_md();
        let bytes = haystack.as_bytes();

        let found = find(
            Path::new("docs/notes.md"),
            bytes,
            b"the agent - not the user - decides",
            FOLDING,
        );

        let Found::Unique(located) = found else {
            panic!("expected Unique, got {found:?}")
        };
        assert!(located.normalized);
        assert_eq!(line_of(bytes, located.span.start), 17);

        let edited = String::from_utf8(splice(bytes, located.span, b"the agent decides")).unwrap();
        assert_eq!(
            edited.lines().nth(16).unwrap(),
            "In that case the agent decides."
        );
        for (before, after) in haystack
            .lines()
            .zip(edited.lines())
            .enumerate()
            .filter_map(|(n, pair)| if n == 16 { None } else { Some(pair) })
        {
            assert_eq!(before, after);
        }
    }

    #[test]
    fn normalize_with_neither_an_exact_nor_a_folded_match_hints_at_nothing() {
        let haystack = notes_md();

        let found = find(
            Path::new("docs/notes.md"),
            haystack.as_bytes(),
            b"the maintainer decides",
            FOLDING,
        );

        let Found::NotFound {
            normalized_hint, ..
        } = found
        else {
            panic!("expected NotFound, got {found:?}")
        };
        assert!(normalized_hint.is_none());
    }

    #[test]
    fn an_exact_match_is_never_traded_for_a_folded_one() {
        let haystack = "a - b\na \u{2013} b\n";

        let found = find(
            Path::new("notes.md"),
            haystack.as_bytes(),
            b"a - b",
            FOLDING,
        );

        let Found::Unique(located) = found else {
            panic!("expected Unique, got {found:?}")
        };
        assert!(!located.normalized);
        assert_eq!(located.span, Span { start: 0, end: 5 });
    }

    #[test]
    fn a_haystack_that_is_not_utf8_still_matches_exactly_and_folds_to_nothing() {
        let mut haystack = b"const cap = 10\n".to_vec();
        haystack.extend_from_slice(&[0xff, 0xfe]);
        haystack.extend_from_slice("\u{2013}\n".as_bytes());

        let found = find(Path::new("f"), &haystack, b"const cap = 10", FOLDING);
        assert!(matches!(found, Found::Unique(_)));

        let found = find(Path::new("f"), &haystack, b"cap - 10", FOLDING);
        let Found::NotFound {
            normalized_hint, ..
        } = found
        else {
            panic!("expected NotFound, got {found:?}")
        };
        assert!(normalized_hint.is_none());
    }

    #[test]
    fn splice_keeps_the_line_endings_on_both_sides_of_the_span() {
        let haystack = "a\r\nconst cap = 10\r\nb\r\n".as_bytes();
        let found = find(Path::new("f"), haystack, b"const cap = 10", EXACT);

        let Found::Unique(located) = found else {
            panic!("expected Unique, got {found:?}")
        };
        assert_eq!(
            splice(haystack, located.span, b"const cap = 20"),
            "a\r\nconst cap = 20\r\nb\r\n".as_bytes()
        );
    }

    #[test]
    fn a_candidate_row_carries_no_carriage_return_from_a_crlf_file() {
        let haystack = "cap\r\ncap\r\n".as_bytes();

        let found = find(Path::new("f"), haystack, b"cap", EXACT);

        let Found::Ambiguous(candidates) = found else {
            panic!("expected Ambiguous, got {found:?}")
        };
        assert_eq!(candidates[0].text, "cap");
        assert_eq!(candidates[1].text, "cap");
        assert_eq!(candidates[1].line, 2);
    }

    #[test]
    fn line_of_counts_the_newlines_before_the_offset() {
        let haystack = b"one\ntwo\nthree\n";

        assert_eq!(line_of(haystack, 0), 1);
        assert_eq!(line_of(haystack, 3), 1);
        assert_eq!(line_of(haystack, 4), 2);
        assert_eq!(line_of(haystack, 8), 3);
        assert_eq!(line_of(haystack, haystack.len()), 4);
    }

    #[test]
    fn ambiguous_candidates_resolve_quickly_on_a_large_file() {
        // A per-match scan made this O(n²); 200 ms is slack over the 15 ms `edit` gate.
        use std::fmt::Write as _;
        let mut haystack = String::new();
        for n in 1..=3000 {
            let _ = writeln!(
                haystack,
                "  const someIdentifier{n} = computeTheValue({n}, options);"
            );
        }

        let start = std::time::Instant::now();
        let found = find(Path::new("f"), haystack.as_bytes(), b"const", EXACT);
        let elapsed = start.elapsed();

        let Found::Ambiguous(candidates) = found else {
            panic!("expected Ambiguous, got {found:?}")
        };
        assert_eq!(candidates.len(), 3000);
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "took {elapsed:?}"
        );
    }

    #[test]
    fn a_2mb_single_line_miss_stays_fast() {
        // Unbounded, this costs 641 ms; 40 ms leaves generous slack over the capped cost.
        let mut haystack = "x".repeat(2_000_000);
        haystack.push('\n');

        let start = std::time::Instant::now();
        let nearest = nearest_line(Path::new("f"), haystack.as_bytes(), b"const cap = 10");
        let elapsed = start.elapsed();

        assert!(nearest.is_some());
        assert!(
            elapsed < std::time::Duration::from_millis(40),
            "took {elapsed:?}"
        );
    }

    fn table_mixed_text() -> impl Strategy<Value = String> {
        let alphabet = prop_oneof![
            Just('\u{2018}'),
            Just('\u{2019}'),
            Just('\u{201C}'),
            Just('\u{201D}'),
            Just('\u{2013}'),
            Just('\u{2014}'),
            Just('\u{00A0}'),
            Just('a'),
            Just('Z'),
            Just(' '),
            Just('\n'),
            Just('é'),
            Just('€'),
        ];
        proptest::collection::vec(alphabet, 1..40).prop_map(|cs| cs.into_iter().collect())
    }

    /// `X` never appears in `a`/`b`, so a separator can neither hide nor create an occurrence.
    fn repeated_needle() -> impl Strategy<Value = (Vec<u8>, Vec<u8>, usize)> {
        (
            proptest::collection::vec(prop_oneof![Just(b'a'), Just(b'b')], 1..8),
            proptest::collection::vec(Just(b'X'), 1..4),
            1usize..6,
        )
    }

    /// Written independently of `levenshtein`, so the oracle cannot share its bug.
    fn reference_levenshtein(a: &str, b: &str) -> usize {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        let mut table = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for (i, row) in table.iter_mut().enumerate() {
            row[0] = i;
        }
        for (j, cell) in table[0].iter_mut().enumerate() {
            *cell = j;
        }
        for i in 1..=a.len() {
            for j in 1..=b.len() {
                let cost = usize::from(a[i - 1] != b[j - 1]);
                table[i][j] = (table[i - 1][j] + 1)
                    .min(table[i][j - 1] + 1)
                    .min(table[i - 1][j - 1] + cost);
            }
        }
        table[a.len()][b.len()]
    }

    proptest! {
        #[test]
        fn splice_then_reverse_restores_the_original_bytes(
            haystack in proptest::collection::vec(any::<u8>(), 0..64),
            a in any::<usize>(),
            b in any::<usize>(),
            replacement in proptest::collection::vec(any::<u8>(), 0..16),
        ) {
            let (i, j) = (a % (haystack.len() + 1), b % (haystack.len() + 1));
            let span = Span { start: i.min(j), end: i.max(j) };
            let removed = haystack[span.start..span.end].to_vec();

            let edited = splice(&haystack, span, &replacement);
            let restored = splice(
                &edited,
                Span { start: span.start, end: span.start + replacement.len() },
                &removed,
            );

            prop_assert_eq!(restored, haystack);
        }

        #[test]
        fn all_returns_exactly_as_many_spans_as_there_are_occurrences(
            (needle, separator, copies) in repeated_needle()
        ) {
            let mut haystack = Vec::new();
            for copy in 0..copies {
                if copy > 0 {
                    haystack.extend_from_slice(&separator);
                }
                haystack.extend_from_slice(&needle);
            }

            let found = find(Path::new("f"), &haystack, &needle, EXACT_ALL);

            let Found::All(located) = found else {
                return Err(TestCaseError::fail(format!("expected All, got {found:?}")));
            };
            prop_assert_eq!(located.len(), copies);
            for hit in located {
                prop_assert_eq!(&haystack[hit.span.start..hit.span.end], &needle[..]);
            }
        }

        /// Lines come from `str::lines()` and distances from the reference, not the code tested.
        #[test]
        fn the_nearest_line_is_the_first_real_line_at_the_smallest_distance(
            haystack in "(?s)[a-c \n]{1,60}",
            needle in "[a-c ]{1,8}",
        ) {
            let found = find(Path::new("f"), haystack.as_bytes(), needle.as_bytes(), EXACT);
            let Found::NotFound { nearest, .. } = found else {
                return Ok(());
            };

            let real_lines: Vec<(usize, &str)> = haystack
                .lines()
                .enumerate()
                .map(|(i, line)| (i + 1, line))
                .filter(|&(_, line)| !line.is_empty())
                .collect();
            let smallest = real_lines
                .iter()
                .map(|&(line_number, line)| (line_number, line, reference_levenshtein(&needle, line)))
                .min_by_key(|&(_, _, distance)| distance);

            match (nearest, smallest) {
                (None, None) => {},
                (Some(nearest), Some((line_number, line, distance))) => {
                    prop_assert_eq!(nearest.distance, distance);
                    prop_assert_eq!(nearest.candidate.line, line_number);
                    prop_assert_eq!(nearest.candidate.text.as_str(), line.trim_end_matches('\r'));
                },
                (nearest, smallest) => {
                    return Err(TestCaseError::fail(format!(
                        "nearest {nearest:?} disagreed with the independent oracle {smallest:?}"
                    )));
                },
            }
        }

        #[test]
        fn a_normalized_match_touches_no_byte_outside_its_mapped_span(
            text in table_mixed_text(),
            a in any::<usize>(),
            b in any::<usize>(),
        ) {
            prop_assume!(normalize::contains_foldable(text.as_bytes()));
            let folded = normalize::fold(&text);
            let bounds: Vec<usize> = folded
                .text
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(folded.text.len()))
                .collect();
            let (i, j) = (a % bounds.len(), b % bounds.len());
            let (lo, hi) = (bounds[i.min(j)], bounds[i.max(j)]);
            prop_assume!(lo < hi);
            let needle = &folded.text.as_bytes()[lo..hi];

            let found = find(Path::new("f"), text.as_bytes(), needle, FindOptions {
                all: true,
                normalize: true,
            });

            let Found::All(located) = found else {
                return Err(TestCaseError::fail(format!("expected All, got {found:?}")));
            };
            prop_assert!(!located.is_empty());
            let span = located[0].span;
            let replacement = b"REPLACED";
            let edited = splice(text.as_bytes(), span, replacement);

            prop_assert_eq!(&edited[..span.start], &text.as_bytes()[..span.start]);
            prop_assert_eq!(&edited[span.start + replacement.len()..], &text.as_bytes()[span.end..]);
            // The span covers whole original characters, never part of a neighbour's bytes.
            let matched = normalize::fold(&text[span.start..span.end]);
            prop_assert_eq!(matched.text.as_bytes(), needle);
        }
    }
}
