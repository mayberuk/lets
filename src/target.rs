//! `path`, `path:40`, `path:40-80`, `path@'regex'[+N]`, `path#name`. An existing file wins before
//! any metacharacter is read, so `C#.md:2` is line 2 of `C#.md`; otherwise `#` beats `@` beats a
//! digit-suffixed `:`.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Whole,
    Line(usize),
    Range(usize, usize),
    Regex { pattern: String, occurrence: usize },
    Symbol(Vec<String>),
}

#[derive(Debug, Clone)]
pub struct Target {
    pub raw: String,
    pub path: PathBuf,
    pub kind: Kind,
}

pub fn parse(raw: &str) -> Target {
    if !raw.contains(['#', '@', ':']) {
        return parse_lexical(raw);
    }
    if Path::new(raw).is_file() {
        return Target {
            raw: raw.to_owned(),
            path: PathBuf::from(raw),
            kind: Kind::Whole,
        };
    }
    let longest_file_prefix = raw
        .char_indices()
        .rev()
        .filter_map(|(at, delimiter)| Some((at, suffix_kind(&raw[at..], delimiter)?)))
        .find(|(at, _)| Path::new(&raw[..*at]).is_file());
    match longest_file_prefix {
        Some((at, kind)) => Target {
            raw: raw.to_owned(),
            path: PathBuf::from(&raw[..at]),
            kind,
        },
        None => parse_lexical(raw),
    }
}

/// The whole suffix must parse: a real file followed by an unreadable scrap is not a target.
fn suffix_kind(suffix: &str, delimiter: char) -> Option<Kind> {
    match delimiter {
        '#' => Some(parse_symbol(suffix, 0).1),
        '@' => quoted_regex(suffix).and_then(|(at, kind)| (at == 0).then_some(kind)),
        ':' => parse_number_suffix(&suffix[1..]),
        _ => None,
    }
}

pub fn parse_lexical(raw: &str) -> Target {
    let (path, kind) = if let Some(hash) = raw.find('#') {
        parse_symbol(raw, hash)
    } else if let Some((at, kind)) = quoted_regex(raw) {
        (raw[..at].to_owned(), kind)
    } else if let Some(colon) = raw.rfind(':') {
        match parse_number_suffix(&raw[colon + 1..]) {
            Some(kind) => (raw[..colon].to_owned(), kind),
            None => (raw.to_owned(), Kind::Whole),
        }
    } else {
        (raw.to_owned(), Kind::Whole)
    };
    Target {
        raw: raw.to_owned(),
        path: PathBuf::from(path),
        kind,
    }
}

fn parse_symbol(raw: &str, hash: usize) -> (String, Kind) {
    let remainder = &raw[hash + 1..];
    let segments = match remainder.strip_prefix('\'') {
        // Markdown headings: `.` and spaces are literal, one segment.
        Some(quoted) => vec![quoted[..quoted.find('\'').unwrap_or(quoted.len())].to_owned()],
        None => remainder.split('.').map(str::to_owned).collect(),
    };
    (raw[..hash].to_owned(), Kind::Symbol(segments))
}

fn quoted_regex(raw: &str) -> Option<(usize, Kind)> {
    raw.match_indices('@').find_map(|(at, _)| {
        let body = raw[at + 1..].strip_prefix('\'')?;
        let close = body.rfind('\'')?;
        // A scrap after the quote leaves `@` literal. `+0` stays 0, so the verb reports not found
        // rather than quietly showing the first match.
        let suffix = &body[close + 1..];
        let occurrence = if suffix.is_empty() {
            1
        } else {
            suffix.strip_prefix('+')?.parse::<usize>().ok()?
        };
        Some((at, Kind::Regex {
            pattern: body[..close].to_owned(),
            occurrence,
        }))
    })
}

fn parse_number_suffix(suffix: &str) -> Option<Kind> {
    if let Some((a, b)) = suffix.split_once('-') {
        if !digits_only(a) || !digits_only(b) {
            return None;
        }
        let (a, b) = (a.parse::<usize>().ok()?, b.parse::<usize>().ok()?);
        return Some(if a <= b {
            Kind::Range(a, b)
        } else {
            Kind::Range(b, a)
        });
    }
    digits_only(suffix)
        .then(|| suffix.parse::<usize>().ok())
        .flatten()
        .map(Kind::Line)
}

fn digits_only(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

#[derive(Debug, PartialEq, Eq)]
pub struct RegexMatch {
    pub line: usize,
    pub text: String,
}

pub fn find_regex(
    content: &str,
    pattern: &str,
    occurrence: usize,
) -> Result<Option<RegexMatch>, regex::Error> {
    let re = regex::Regex::new(pattern)?;
    let mut seen = 0;
    for (index, text) in content.lines().enumerate() {
        if re.is_match(text) {
            seen += 1;
            if seen == occurrence {
                return Ok(Some(RegexMatch {
                    line: index + 1,
                    text: text.to_owned(),
                }));
            }
        }
    }
    Ok(None)
}

/// A failed target whose prefix is a real file is almost always a habit from another tool.
pub fn meant(raw: &str) -> Option<String> {
    if Path::new(raw).is_file() {
        return None;
    }
    if let Some(at) = raw.rfind('@') {
        let (path, rest) = (&raw[..at], &raw[at + 1..]);
        let (pattern, occurrence) = split_occurrence(rest);
        if !pattern.is_empty() && !pattern.starts_with('\'') && Path::new(path).is_file() {
            return Some(format!("\"{path}@'{pattern}'{occurrence}\""));
        }
    }
    if let Some(colon) = raw.rfind(':') {
        let (path, suffix) = (&raw[..colon], &raw[colon + 1..]);
        if Path::new(path).is_file() {
            if let Some(range) = comma_range(suffix) {
                return Some(format!("{path}:{range}"));
            }
            if is_identifier(suffix) {
                return Some(format!("{path}#{suffix}"));
            }
        }
        // `path:40:60`, grep's `file:line:col` shape as much as a range, so only suggested.
        if let Some((file, line)) = path.rsplit_once(':')
            && digits_only(line)
            && digits_only(suffix)
            && Path::new(file).is_file()
        {
            return Some(format!("{file}:{line}-{suffix}"));
        }
    }
    None
}

/// A `-N` stays in the pattern: the grammar has no `-N` form to move it to.
fn split_occurrence(rest: &str) -> (&str, &str) {
    match rest.rfind('+') {
        Some(plus) if digits_only(&rest[plus + 1..]) => rest.split_at(plus),
        _ => (rest, ""),
    }
}

/// `path:40,60`, sed's comma habit for a range.
fn comma_range(suffix: &str) -> Option<String> {
    let (a, b) = suffix.split_once(',')?;
    (digits_only(a) && digits_only(b)).then(|| format!("{a}-{b}"))
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {},
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn path(t: &Target) -> &str {
        t.path.to_str().expect("fixture paths are UTF-8")
    }

    #[test]
    fn whole_file_with_no_metacharacter() {
        let t = parse("src/store/usage.ts");
        assert_eq!(t.kind, Kind::Whole);
        assert_eq!(path(&t), "src/store/usage.ts");
    }

    #[test]
    fn single_line() {
        let t = parse("a.ts:40");
        assert_eq!(t.kind, Kind::Line(40));
        assert_eq!(path(&t), "a.ts");
    }

    #[test]
    fn line_range() {
        let t = parse("a.ts:40-80");
        assert_eq!(t.kind, Kind::Range(40, 80));
        assert_eq!(path(&t), "a.ts");
    }

    #[test]
    fn reversed_range_bounds_are_swapped() {
        let t = parse("a.ts:80-40");
        assert_eq!(t.kind, Kind::Range(40, 80));
    }

    #[test]
    fn regex_target_defaults_to_first_occurrence() {
        let t = parse("src/server/compose.ts@'export async function compose'");
        assert_eq!(t.kind, Kind::Regex {
            pattern: "export async function compose".to_owned(),
            occurrence: 1,
        });
        assert_eq!(path(&t), "src/server/compose.ts");
    }

    #[test]
    fn regex_target_with_explicit_occurrence() {
        let t = parse("a.ts@'x'+2");
        assert_eq!(t.kind, Kind::Regex {
            pattern: "x".to_owned(),
            occurrence: 2,
        });
    }

    #[test]
    fn single_symbol_segment() {
        let t = parse("src/store/usage.ts#usage");
        assert_eq!(t.kind, Kind::Symbol(vec!["usage".to_owned()]));
        assert_eq!(path(&t), "src/store/usage.ts");
    }

    #[test]
    fn nested_symbol_splits_on_dot() {
        let t = parse("internal/store/store.go#Store.Open");
        assert_eq!(
            t.kind,
            Kind::Symbol(vec!["Store".to_owned(), "Open".to_owned()])
        );
    }

    #[test]
    fn quoted_symbol_keeps_interior_dots_and_spaces_literal() {
        let t = parse("census.md#'Bottom line'");
        assert_eq!(t.kind, Kind::Symbol(vec!["Bottom line".to_owned()]));
        assert_eq!(path(&t), "census.md");
    }

    #[test]
    fn colon_suffix_without_digits_is_whole_not_a_line() {
        let t = parse("a.ts:not-a-line");
        assert_eq!(t.kind, Kind::Whole);
        assert_eq!(path(&t), "a.ts:not-a-line");
    }

    #[test]
    fn a_quoteless_at_is_an_ordinary_path_byte() {
        let t = parse("node_modules/@types/node/index.d.ts");
        assert_eq!(t.kind, Kind::Whole);
        assert_eq!(path(&t), "node_modules/@types/node/index.d.ts");
    }

    #[test]
    fn an_unterminated_quote_is_not_a_regex_target() {
        let t = parse("a.ts@'unterminated");
        assert_eq!(t.kind, Kind::Whole);
        assert_eq!(path(&t), "a.ts@'unterminated");
    }

    #[test]
    fn equal_range_bounds_are_the_one_line() {
        assert_eq!(parse("a.ts:40-40").kind, Kind::Range(40, 40));
    }

    #[test]
    fn a_hash_wins_over_an_at_in_the_same_string() {
        let t = parse("src/@scope/pkg.ts@'export'#Store.Open");
        assert_eq!(
            t.kind,
            Kind::Symbol(vec!["Store".to_owned(), "Open".to_owned()])
        );
        assert_eq!(path(&t), "src/@scope/pkg.ts@'export'");
    }

    #[test]
    fn an_at_wins_over_a_digit_suffixed_colon_in_the_same_string() {
        let t = parse("src/a.ts:40@'cap'");
        assert_eq!(t.kind, Kind::Regex {
            pattern: "cap".to_owned(),
            occurrence: 1,
        });
        assert_eq!(path(&t), "src/a.ts:40");
    }

    #[test]
    fn three_segments_nest_outermost_first() {
        let t = parse("src/app.ts#Outer.Inner.render");
        assert_eq!(
            t.kind,
            Kind::Symbol(vec![
                "Outer".to_owned(),
                "Inner".to_owned(),
                "render".to_owned(),
            ])
        );
    }

    #[test]
    fn an_empty_target_is_a_whole_empty_path() {
        let t = parse("");
        assert_eq!(t.kind, Kind::Whole);
        assert_eq!(path(&t), "");
    }

    #[test]
    fn occurrence_zero_names_no_match_and_does_not_become_the_first() {
        let t = parse("a.ts@'x'+0");
        assert_eq!(t.kind, Kind::Regex {
            pattern: "x".to_owned(),
            occurrence: 0,
        });
        assert_eq!(
            find_regex("x\nx\n", "x", 0).expect("pattern compiles"),
            None
        );
    }

    #[test]
    fn find_regex_returns_the_nth_matching_line() {
        let content = "one\ntwo\nmatch a\nfour\nmatch b\nsix\nmatch c\n";
        let found = find_regex(content, "^match", 2).expect("pattern compiles");
        assert_eq!(
            found,
            Some(RegexMatch {
                line: 5,
                text: "match b".to_owned(),
            })
        );
    }

    #[test]
    fn find_regex_beyond_the_last_match_is_none() {
        let content = "one\nmatch a\n";
        let found = find_regex(content, "^match", 2).expect("pattern compiles");
        assert_eq!(found, None);
    }

    #[test]
    fn find_regex_propagates_a_compile_error() {
        assert!(find_regex("anything", "(unclosed", 1).is_err());
    }

    #[test]
    fn an_existing_file_named_with_a_hash_wins_over_the_symbol_parse() {
        let dir = TempDir::new().expect("temp dir");
        let file = dir.path().join("C#.md");
        std::fs::write(&file, "one\ntwo\nthree\n").expect("fixture write");
        let raw = file.to_str().expect("temp paths are UTF-8");

        let t = parse(raw);

        assert_eq!(t.kind, Kind::Whole);
        assert_eq!(t.path, file);
    }

    #[test]
    fn with_no_such_file_the_same_string_parses_as_a_symbol() {
        let dir = TempDir::new().expect("temp dir");
        let missing = dir.path().join("C#.md");
        let raw = missing.to_str().expect("temp paths are UTF-8");

        let t = parse(raw);

        assert_eq!(t.kind, Kind::Symbol(vec![String::new(), "md".to_owned()]));
        assert_eq!(t.path, dir.path().join("C"));
    }

    #[test]
    fn parse_lexical_reads_the_same_string_as_a_symbol_even_when_the_file_exists() {
        let dir = TempDir::new().expect("temp dir");
        let file = dir.path().join("C#.md");
        std::fs::write(&file, "one\n").expect("fixture write");
        let raw = file.to_str().expect("temp paths are UTF-8");

        let t = parse_lexical(raw);

        assert_eq!(t.kind, Kind::Symbol(vec![String::new(), "md".to_owned()]));
    }

    #[test]
    fn an_empty_dot_segment_stays_a_segment_so_the_symbol_cannot_resolve() {
        let segments = |raw: &str| match parse_lexical(raw).kind {
            Kind::Symbol(segments) => segments,
            other => panic!("{raw} parsed as {other:?}"),
        };
        assert_eq!(segments("a.rs#.main"), ["", "main"]);
        assert_eq!(segments("a.rs#main."), ["main", ""]);
        assert_eq!(segments("a.rs#A..b"), ["A", "", "b"]);
        assert_eq!(segments("f#"), [""]);
    }

    fn fixture(dir: &TempDir, name: &str) -> PathBuf {
        let file = dir.path().join(name);
        std::fs::write(&file, "a\nb\n").expect("fixture write");
        file
    }

    fn raw_in(dir: &TempDir, typed: &str) -> String {
        format!("{}/{typed}", dir.path().display())
    }

    #[test]
    fn an_existing_file_prefix_takes_the_line_suffix_after_it() {
        let dir = TempDir::new().expect("temp dir");
        let file = fixture(&dir, "C#.md");

        let t = parse(&raw_in(&dir, "C#.md:2"));

        assert_eq!(t.kind, Kind::Line(2));
        assert_eq!(t.path, file);
    }

    #[test]
    fn an_existing_file_prefix_takes_the_regex_and_symbol_suffixes_after_it() {
        let dir = TempDir::new().expect("temp dir");
        let file = fixture(&dir, "C#.md");

        let regex = parse(&raw_in(&dir, "C#.md@'b'+2"));
        assert_eq!(regex.kind, Kind::Regex {
            pattern: "b".to_owned(),
            occurrence: 2,
        });
        assert_eq!(regex.path, file);

        let symbol = parse(&raw_in(&dir, "C#.md#Intro"));
        assert_eq!(symbol.kind, Kind::Symbol(vec!["Intro".to_owned()]));
        assert_eq!(symbol.path, file);
    }

    #[test]
    fn the_longest_existing_file_prefix_wins() {
        let dir = TempDir::new().expect("temp dir");
        fixture(&dir, "a");
        let longer = fixture(&dir, "a#b");

        let t = parse(&raw_in(&dir, "a#b:2"));

        assert_eq!(t.kind, Kind::Line(2));
        assert_eq!(t.path, longer);
    }

    #[test]
    fn a_shorter_file_prefix_reads_everything_after_it_as_the_suffix() {
        let dir = TempDir::new().expect("temp dir");
        let file = fixture(&dir, "a");

        let t = parse(&raw_in(&dir, "a#b:2"));

        assert_eq!(t.kind, Kind::Symbol(vec!["b:2".to_owned()]));
        assert_eq!(t.path, file);
    }

    #[test]
    fn a_file_prefix_before_a_malformed_suffix_leaves_the_lexical_parse() {
        let dir = TempDir::new().expect("temp dir");
        fixture(&dir, "C#.md");

        let t = parse(&raw_in(&dir, "C#.md:abc"));

        assert_eq!(
            t.kind,
            Kind::Symbol(vec![String::new(), "md:abc".to_owned()])
        );
        assert_eq!(t.path, dir.path().join("C"));
    }

    #[test]
    fn with_no_file_prefix_a_symbol_target_parses_as_it_always_did() {
        let dir = TempDir::new().expect("temp dir");
        let raw = raw_in(&dir, "a.rs#main");

        let t = parse(&raw);

        assert_eq!(t.kind, Kind::Symbol(vec!["main".to_owned()]));
        assert_eq!(t.path, dir.path().join("a.rs"));
    }

    #[test]
    fn a_colon_file_still_wins_over_the_line_form() {
        let dir = TempDir::new().expect("temp dir");
        let file = dir.path().join("a:1");
        std::fs::write(&file, "one\ntwo\n").expect("fixture write");
        let raw = file.to_str().expect("temp paths are UTF-8");

        let t = parse(raw);

        assert_eq!(t.kind, Kind::Whole);
        assert_eq!(t.path, file);
    }

    #[test]
    fn meant_suggests_the_dash_range_for_the_sed_comma_habit() {
        let dir = TempDir::new().expect("temp dir");
        let file = dir.path().join("P.rs");
        std::fs::write(&file, "content\n").expect("fixture write");
        let raw = format!("{}:40,60", file.display());

        assert_eq!(meant(&raw), Some(format!("{}:40-60", file.display())));
    }

    #[test]
    fn meant_suggests_the_dash_range_for_the_colon_range_habit() {
        let dir = TempDir::new().expect("temp dir");
        let file = fixture(&dir, "f.go");
        let raw = format!("{}:3610:3640", file.display());

        assert_eq!(meant(&raw), Some(format!("{}:3610-3640", file.display())));
    }

    #[test]
    fn meant_is_none_for_the_colon_range_habit_on_a_missing_file() {
        let dir = TempDir::new().expect("temp dir");
        let raw = format!("{}:3:5", dir.path().join("nope.go").display());

        assert_eq!(meant(&raw), None);
    }

    #[test]
    fn meant_is_none_for_a_colon_pair_that_is_not_two_numbers() {
        let dir = TempDir::new().expect("temp dir");
        let file = fixture(&dir, "f.go");

        assert_eq!(meant(&format!("{}:3:x", file.display())), None);
        assert_eq!(meant(&format!("{}:x:5", file.display())), None);
    }

    #[test]
    fn meant_suggests_the_symbol_form_for_a_colon_identifier() {
        let dir = TempDir::new().expect("temp dir");
        let file = dir.path().join("P.rs");
        std::fs::write(&file, "content\n").expect("fixture write");
        let raw = format!("{}:computeTotal", file.display());

        assert_eq!(
            meant(&raw),
            Some(format!("{}#computeTotal", file.display()))
        );
    }

    #[test]
    fn meant_suggests_quoting_an_unquoted_regex() {
        let dir = TempDir::new().expect("temp dir");
        let file = dir.path().join("P.rs");
        std::fs::write(&file, "content\n").expect("fixture write");
        let raw = format!("{}@computeTotal", file.display());

        assert_eq!(
            meant(&raw),
            Some(format!("\"{}@'computeTotal'\"", file.display()))
        );
    }

    #[test]
    fn meant_keeps_an_occurrence_suffix_outside_the_quotes() {
        let dir = TempDir::new().expect("temp dir");
        let file = fixture(&dir, "f.rs");
        let raw = format!("{}@foo+3", file.display());

        assert_eq!(meant(&raw), Some(format!("\"{}@'foo'+3\"", file.display())));
    }

    #[test]
    fn meant_splits_at_the_last_at_so_a_scoped_directory_stays_in_the_path() {
        let dir = TempDir::new().expect("temp dir");
        std::fs::create_dir(dir.path().join("@types")).expect("fixture dir");
        let file = fixture(&dir, "@types/P.ts");
        let raw = format!("{}@foo", file.display());

        assert_eq!(meant(&raw), Some(format!("\"{}@'foo'\"", file.display())));
    }

    #[test]
    fn meant_is_none_for_a_bare_trailing_at() {
        let dir = TempDir::new().expect("temp dir");
        let file = fixture(&dir, "f.rs");

        assert_eq!(meant(&format!("{}@", file.display())), None);
        assert_eq!(meant(&format!("{}@+3", file.display())), None);
    }

    #[test]
    fn meant_is_none_when_the_whole_string_is_itself_an_existing_file() {
        let dir = TempDir::new().expect("temp dir");
        let file = dir.path().join("a:1");
        std::fs::write(&file, "content\n").expect("fixture write");
        let raw = file.to_str().expect("temp paths are UTF-8");

        assert_eq!(meant(raw), None);
    }

    #[test]
    fn meant_is_none_when_the_part_before_the_metacharacter_is_not_a_file() {
        let dir = TempDir::new().expect("temp dir");
        let raw = format!("{}:40,60", dir.path().join("missing.rs").display());

        assert_eq!(meant(&raw), None);
    }

    #[test]
    fn meant_is_none_for_a_negative_line_suffix() {
        let dir = TempDir::new().expect("temp dir");
        let file = dir.path().join("P.rs");
        std::fs::write(&file, "content\n").expect("fixture write");
        let raw = format!("{}:-20", file.display());

        assert_eq!(meant(&raw), None);
    }
}
