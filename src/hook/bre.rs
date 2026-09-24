//! grep's basic regex swaps operators and literals against the `regex` crate (`\|` and `|`,
//! `\(` and `(`, `\+` and `+`), and `a\|b` carried across reports no hits with confidence, so a
//! pattern is translated exactly or not at all. A `grep -E` pattern is refused where they part.

/// Apple's grep compiles with libc's TRE in `REG_ENHANCED` mode and FreeBSD's with libregex: both
/// read `\|`, `\+` and `\?` as GNU does, but not a `$` before `\|` or a bound over 255.
const BSD_GREP: bool = cfg!(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
));

/// These BSDs' grep calls plain POSIX `regcomp`, which reads BRE `\|`, `\+` and `\?` as literals.
const BRE_GNU_OPERATORS: bool = !cfg!(any(
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
));

/// grep's own `RE_DUP_MAX` (glibc 32,767; Darwin `sys/syslimits.h` and the BSDs 255): grep
/// rejects a larger bound itself.
const MAX_INTERVAL_BOUND: u32 = if BSD_GREP { 255 } else { 32_767 };

/// The `regex` crate also accepts `ascii` and `word`, which grep rejects.
const POSIX_CLASSES: [&str; 12] = [
    "alnum", "alpha", "blank", "cntrl", "digit", "graph", "lower", "print", "punct", "space",
    "upper", "xdigit",
];

/// `Nothing` is a branch start, where grep reads `*` as a literal and the `regex` crate errors.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Last {
    Nothing,
    Anchor,
    Atom,
    Quantifier,
}

/// Only a pattern using one of grep's operator escapes is a grep habit worth a second search.
pub(crate) fn grep_reading(pattern: &str) -> Option<String> {
    let mut chars = pattern.chars();
    let mut operator_escape = false;
    while let Some(c) = chars.next() {
        if c == '\\' && matches!(chars.next(), Some('|' | '(' | ')' | '{' | '}' | '+' | '?')) {
            operator_escape = true;
            break;
        }
    }
    if !operator_escape {
        return None;
    }
    translate(pattern).filter(|reading| reading != pattern)
}

pub(super) fn translate(pattern: &str) -> Option<String> {
    if pattern.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(pattern.len() + 8);
    let mut last = Last::Nothing;
    let mut depth = 0usize;
    let mut chars = pattern.char_indices().peekable();

    while let Some((at, c)) = chars.next() {
        last = match c {
            '\\' => match chars.next()?.1 {
                '|' | '+' | '?' if !BRE_GNU_OPERATORS => return None,
                // An empty branch matches every line to GNU grep; BSD regex rejects it, or reads a
                // `\|` that opens a branch as a literal bar.
                '|' | ')' if last == Last::Nothing => return None,
                '|' => {
                    out.push('|');
                    Last::Nothing
                },
                '(' => {
                    depth += 1;
                    out.push('(');
                    Last::Nothing
                },
                ')' => {
                    depth = depth.checked_sub(1)?;
                    out.push(')');
                    Last::Atom
                },
                operator @ ('+' | '?') => {
                    repeatable(last)?;
                    out.push(operator);
                    Last::Quantifier
                },
                '{' => {
                    repeatable(last)?;
                    interval(&mut chars, &mut out)?;
                    Last::Quantifier
                },
                escaped @ ('.' | '*' | '[' | ']' | '^' | '$' | '\\') => {
                    out.push('\\');
                    out.push(escaped);
                    Last::Atom
                },
                '/' => {
                    out.push('/');
                    Last::Atom
                },
                _ => return None,
            },
            '*' if matches!(last, Last::Nothing | Last::Anchor) => {
                out.push_str(r"\*");
                Last::Atom
            },
            '*' => {
                repeatable(last)?;
                out.push('*');
                Last::Quantifier
            },
            '^' if last == Last::Nothing => {
                out.push('^');
                Last::Anchor
            },
            // Apple's TRE reads a `$` before `\|` as a literal, FreeBSD's libregex as an anchor.
            '$' if BSD_GREP && pattern[at + 1..].starts_with(r"\|") => return None,
            '$' if ends_a_branch(&pattern[at + 1..]) => {
                out.push('$');
                Last::Anchor
            },
            // GNU grep 3.7 reads this `$` by what follows: `a.$)` matches `ab$)`, `a.$)*` `ab`.
            '$' if pattern[at + 1..].starts_with([')', '|']) => return None,
            '[' => {
                bracket(&mut chars, &mut out)?;
                Last::Atom
            },
            literal @ ('(' | ')' | '{' | '}' | '+' | '?' | '|') => {
                bracketed(literal, &mut out);
                Last::Atom
            },
            literal @ ('^' | '$') => {
                out.push('\\');
                out.push(literal);
                Last::Atom
            },
            other => {
                out.push(other);
                Last::Atom
            },
        };
    }
    (depth == 0 && last != Last::Nothing).then_some(out)
}

/// The dialects share every operator, so this refuses where they part: `\d`, `\s` and `\w` are
/// classes to the `regex` crate and a letter or a GNU extension to grep.
pub(super) fn translate_extended(pattern: &str) -> Option<String> {
    if pattern.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(pattern.len() + 8);
    let mut last = Last::Nothing;
    let mut depth = 0usize;
    let mut chars = pattern.char_indices().peekable();

    while let Some((_, c)) = chars.next() {
        last = match c {
            '\\' => match chars.next()?.1 {
                literal @ ('(' | ')' | '+' | '?' | '{' | '}' | '|') => {
                    bracketed(literal, &mut out);
                    Last::Atom
                },
                escaped @ ('.' | '[' | ']' | '*' | '^' | '$' | '\\') => {
                    out.push('\\');
                    out.push(escaped);
                    Last::Atom
                },
                '/' => {
                    out.push('/');
                    Last::Atom
                },
                _ => return None,
            },
            '|' | ')' if last == Last::Nothing => return None,
            '|' => {
                out.push('|');
                Last::Nothing
            },
            '(' => {
                depth += 1;
                out.push('(');
                Last::Nothing
            },
            ')' => {
                depth = depth.checked_sub(1)?;
                out.push(')');
                Last::Atom
            },
            quantifier @ ('*' | '+' | '?') => {
                repeatable(last)?;
                out.push(quantifier);
                Last::Quantifier
            },
            '{' => {
                repeatable(last)?;
                extended_interval(&mut chars, &mut out)?;
                Last::Quantifier
            },
            // A lone `}` is a literal to grep, and to the `regex` crate inside a class.
            '}' => {
                bracketed('}', &mut out);
                Last::Atom
            },
            anchor @ ('^' | '$') => {
                out.push(anchor);
                Last::Anchor
            },
            '[' => {
                bracket(&mut chars, &mut out)?;
                Last::Atom
            },
            other => {
                out.push(other);
                Last::Atom
            },
        };
    }
    (depth == 0 && last != Last::Nothing).then_some(out)
}

/// A class, not a backslash escape: `lets find` retries a no-hit pattern in grep's reading,
/// where `\|` would turn back into alternation.
fn bracketed(literal: char, out: &mut String) {
    out.push('[');
    out.push(literal);
    out.push(']');
}

fn extended_interval(
    chars: &mut std::iter::Peekable<std::str::CharIndices>,
    out: &mut String,
) -> Option<()> {
    let low = digits(chars)?;
    let high = if chars.next_if(|&(_, c)| c == ',').is_some() {
        if chars.peek().is_some_and(|&(_, c)| c == '}') {
            None
        } else {
            Some(digits(chars)?)
        }
    } else {
        Some(low)
    };
    if chars.next()?.1 != '}' || high.is_some_and(|high| high < low) {
        return None;
    }
    out.push('{');
    out.push_str(&low.to_string());
    match high {
        Some(high) if high == low => {},
        Some(high) => {
            out.push(',');
            out.push_str(&high.to_string());
        },
        None => out.push(','),
    }
    out.push('}');
    Some(())
}

/// A bare or stacked quantifier is a literal to grep and an error or another repeat to `regex`.
fn repeatable(last: Last) -> Option<()> {
    (last == Last::Atom).then_some(())
}

fn ends_a_branch(rest: &str) -> bool {
    rest.is_empty() || rest.starts_with(r"\)") || rest.starts_with(r"\|")
}

/// grep's basic syntax has no `\{,n\}`.
fn interval(
    chars: &mut std::iter::Peekable<std::str::CharIndices>,
    out: &mut String,
) -> Option<()> {
    let low = digits(chars)?;
    let high = if chars.next_if(|&(_, c)| c == ',').is_some() {
        if chars.peek().is_some_and(|&(_, c)| c == '\\') {
            None
        } else {
            Some(digits(chars)?)
        }
    } else {
        Some(low)
    };
    if chars.next()?.1 != '\\' || chars.next()?.1 != '}' {
        return None;
    }
    if high.is_some_and(|high| high < low) {
        return None;
    }
    let (low, high) = (low.to_string(), high.map(|high| high.to_string()));
    out.push('{');
    out.push_str(&low);
    match high {
        Some(high) if high == low => {},
        Some(high) => {
            out.push(',');
            out.push_str(&high);
        },
        None => out.push(','),
    }
    out.push('}');
    Some(())
}

fn digits(chars: &mut std::iter::Peekable<std::str::CharIndices>) -> Option<u32> {
    let mut value: Option<u32> = None;
    while let Some((_, digit)) = chars.next_if(|&(_, c)| c.is_ascii_digit()) {
        let next = value.unwrap_or(0) * 10 + digit.to_digit(10)?;
        if next > MAX_INTERVAL_BOUND {
            return None;
        }
        value = Some(next);
    }
    value
}

/// The `regex` crate adds syntax grep lacks here: `\` escapes, `[` nests, and `&&`, `--` and `~~`
/// are set operations. A leading `]` is refused rather than carried.
fn bracket(chars: &mut std::iter::Peekable<std::str::CharIndices>, out: &mut String) -> Option<()> {
    out.push('[');
    if let Some((_, caret)) = chars.next_if(|&(_, c)| c == '^') {
        out.push(caret);
    }
    if chars.peek().is_some_and(|&(_, c)| c == ']') {
        return None;
    }
    loop {
        let (_, c) = chars.next()?;
        match c {
            ']' => {
                out.push(']');
                return Some(());
            },
            '\\' => return None,
            '[' => {
                chars.next_if(|&(_, c)| c == ':')?;
                let mut name = String::new();
                while let Some((_, letter)) = chars.next_if(|&(_, c)| c.is_ascii_lowercase()) {
                    name.push(letter);
                }
                if chars.next()?.1 != ':' || chars.next()?.1 != ']' {
                    return None;
                }
                if !POSIX_CLASSES.contains(&name.as_str()) {
                    return None;
                }
                out.push_str("[:");
                out.push_str(&name);
                out.push_str(":]");
            },
            '&' | '-' | '~' if chars.peek().is_some_and(|&(_, next)| next == c) => return None,
            other => out.push(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{grep_reading, translate, translate_extended};

    #[test]
    fn grep_reading_translates_a_pattern_holding_a_bre_operator_escape() {
        assert_eq!(grep_reading(r"Foo\|Bar"), Some("Foo|Bar".to_owned()));
        assert_eq!(grep_reading(r"\(ab\)\+"), Some("(ab)+".to_owned()));
        assert_eq!(grep_reading(r"x\{2\}"), Some("x{2}".to_owned()));
        assert_eq!(grep_reading(r"colou\?r"), Some("colou?r".to_owned()));
    }

    #[test]
    fn grep_reading_is_none_without_a_bre_operator_escape() {
        // `a\\|b` escapes the backslash, not the bar; `\(a` never closes.
        for pattern in ["Foo|Bar", "a.b*c", r"a\.b", r"a\\|b", r"\bword", "", r"\(a"] {
            assert_eq!(grep_reading(pattern), None, "{pattern:?}");
        }
    }

    fn translated(pattern: &str) -> String {
        translate(pattern).unwrap_or_else(|| panic!("{pattern:?} did not translate"))
    }

    fn untranslated(pattern: &str) {
        assert_eq!(translate(pattern), None, "{pattern:?}");
    }

    #[test]
    fn a_pattern_with_no_operator_in_either_dialect_is_unchanged() {
        assert_eq!(translated("alpha"), "alpha");
        assert_eq!(translated("a.b*c"), "a.b*c");
    }

    #[test]
    fn escaped_alternation_becomes_a_bare_pipe_and_a_bare_pipe_a_literal() {
        assert_eq!(translated(r"alpha\|beta"), "alpha|beta");
        assert_eq!(translated("alpha|beta"), "alpha[|]beta");
    }

    #[test]
    fn an_escaped_group_becomes_a_group_and_bare_parens_become_literals() {
        assert_eq!(translated(r"\(alpha\)"), "(alpha)");
        assert_eq!(translated("call(x)"), "call[(]x[)]");
    }

    #[test]
    fn escaped_plus_and_question_are_quantifiers_and_bare_ones_are_literals() {
        assert_eq!(translated(r"a\+b"), "a+b");
        assert_eq!(translated(r"a\?b"), "a?b");
        assert_eq!(translated("a+b"), "a[+]b");
        assert_eq!(translated("a?b"), "a[?]b");
    }

    #[test]
    fn an_escaped_interval_becomes_an_interval_and_bare_braces_become_literals() {
        assert_eq!(translated(r"x\{2\}"), "x{2}");
        assert_eq!(translated(r"x\{2,\}"), "x{2,}");
        assert_eq!(translated(r"x\{2,5\}"), "x{2,5}");
        assert_eq!(translated("x{2}"), "x[{]2[}]");
        assert_eq!(translated("}"), "[}]");
    }

    #[test]
    fn an_interval_that_is_not_digits_or_is_out_of_order_does_not_translate() {
        untranslated(r"x\{,2\}");
        untranslated(r"x\{a\}");
        untranslated(r"x\{2");
        untranslated(r"x\{2,1\}");
        untranslated(r"x\{2\)");
        untranslated(r"x\{40000\}");
    }

    #[test]
    fn escapes_meaning_the_literal_in_both_dialects_are_kept() {
        for kept in [r"\.", r"\*", r"\[", r"\]", r"\^", r"\$", r"\\"] {
            assert_eq!(translated(&format!("a{kept}")), format!("a{kept}"));
        }
        assert_eq!(translated(r"a\/b"), "a/b");
    }

    #[test]
    fn a_back_reference_or_a_gnu_escape_does_not_translate() {
        untranslated(r"\(a\)\1");
        for escape in [
            r"\<", r"\>", r"\w", r"\W", r"\s", r"\S", r"\b", r"\B", r"\`", r"\'", r"\n",
        ] {
            untranslated(&format!("a{escape}"));
        }
        untranslated(r"a\");
    }

    #[test]
    fn a_caret_anchors_only_where_a_branch_starts() {
        assert_eq!(translated("^a"), "^a");
        assert_eq!(translated(r"\(^a\)"), "(^a)");
        assert_eq!(translated(r"a\|^b"), "a|^b");
        assert_eq!(translated("a^b"), r"a\^b");
        assert_eq!(translated("^^"), r"^\^");
    }

    #[test]
    fn a_dollar_anchors_only_where_a_branch_ends() {
        assert_eq!(translated("a$"), "a$");
        assert_eq!(translated(r"\(a$\)"), "(a$)");
        assert_eq!(translated("a$b"), r"a\$b");
        assert_eq!(translated("$$"), r"\$$");
    }

    #[cfg(not(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    #[test]
    fn a_dollar_before_an_escaped_bar_anchors() {
        assert_eq!(translated(r"a$\|b"), "a$|b");
    }

    /// Apple's TRE reads this `$` as a literal and FreeBSD's libregex as an anchor.
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    #[test]
    fn on_bsd_a_dollar_before_an_escaped_bar_does_not_translate() {
        untranslated(r"a$\|b");
        assert_eq!(translated(r"\(a$\)"), "(a$)");
        assert_eq!(translated("a$"), "a$");
    }

    #[cfg(not(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    #[test]
    fn a_bound_over_255_translates_where_grep_allows_32767() {
        assert_eq!(translated(r"x\{256\}"), "x{256}");
        assert_eq!(extended("x{2,1000}"), "x{2,1000}");
    }

    /// `RE_DUP_MAX` is 255 in Darwin's `sys/syslimits.h` and the BSD libcs, so grep rejects more.
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    #[test]
    fn on_bsd_a_bound_over_255_does_not_translate() {
        assert_eq!(translated(r"x\{255\}"), "x{255}");
        untranslated(r"x\{256\}");
        not_extended("x{256}");
        not_extended("x{2,256}");
    }

    #[cfg(any(target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
    #[test]
    fn where_grep_is_posix_only_a_gnu_operator_escape_does_not_translate() {
        for pattern in [r"a\|b", r"a\+", r"a\?"] {
            untranslated(pattern);
        }
        assert_eq!(translated(r"\(a\)\{2\}"), "(a){2}");
    }

    /// GNU grep matches every line; BSD regex rejects the pattern or reads that bar as a literal.
    #[test]
    fn an_empty_branch_or_group_does_not_translate() {
        for pattern in [
            r"\|", r"a\|", r"\|a", r"a\|\|b", r"\(\|a\)", r"\(a\|\)", r"\(\)",
        ] {
            untranslated(pattern);
        }
        for pattern in ["|", "a|", "|a", "a||b", "(|a)", "(a|)", "()"] {
            not_extended(pattern);
        }
        assert_eq!(extended("^|a"), "^|a");
    }

    #[cfg(not(any(target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly")))]
    #[test]
    fn a_branch_holding_only_an_anchor_is_not_empty() {
        assert_eq!(translated(r"^\|a"), "^|a");
        assert_eq!(translated(r"a\|b"), "a|b");
    }

    #[test]
    fn a_dollar_before_a_bare_paren_or_bar_does_not_translate() {
        untranslated("a$)");
        untranslated("a$)*");
        untranslated("a$|b");
        assert_eq!(translated("a$(b"), r"a\$[(]b");
        assert_eq!(translated("a$}"), r"a\$[}]");
    }

    #[test]
    fn a_star_with_nothing_to_repeat_is_a_literal_star() {
        assert_eq!(translated("*a"), r"\*a");
        assert_eq!(translated(r"\(*a\)"), r"(\*a)");
        assert_eq!(translated(r"a\|*b"), r"a|\*b");
        assert_eq!(translated("^*a"), r"^\*a");
        assert_eq!(translated("**a"), r"\**a");
        assert_eq!(translated("a*"), "a*");
        assert_eq!(translated(r"\(a\)*"), "(a)*");
        assert_eq!(translated("a^*"), r"a\^*");
    }

    #[test]
    fn a_quantifier_stacked_on_another_does_not_translate() {
        untranslated("a**");
        untranslated(r"a*\+");
        untranslated(r"a\{2\}*");
        untranslated(r"a\?\{2\}");
        untranslated(r"\+a");
        untranslated(r"\{2\}");
        assert_eq!(translated("a*+"), "a*[+]");
    }

    #[test]
    fn a_bracket_expression_is_copied_through_its_closing_bracket() {
        assert_eq!(translated("[abc]+"), "[abc][+]");
        assert_eq!(translated("[^a-z]"), "[^a-z]");
        assert_eq!(translated("[(|)]"), "[(|)]");
        assert_eq!(translated("[[:digit:]]x"), "[[:digit:]]x");
        assert_eq!(translated("[a-]"), "[a-]");
    }

    #[test]
    fn a_bracket_expression_the_regex_crate_reads_differently_does_not_translate() {
        untranslated(r"[\n]");
        untranslated("[a[b]");
        untranslated("[[.a.]]");
        untranslated("[[:word:]]");
        untranslated("[a&&b]");
        untranslated("[a--b]");
        untranslated("[a~~b]");
        untranslated("[]a]");
        untranslated("[^]a]");
        untranslated("[abc");
    }

    #[test]
    fn unbalanced_groups_do_not_translate() {
        untranslated(r"\(a");
        untranslated(r"a\)");
        assert_eq!(translated("(a"), "[(]a");
    }

    #[test]
    fn the_empty_pattern_does_not_translate() {
        untranslated("");
    }

    fn extended(pattern: &str) -> String {
        translate_extended(pattern).unwrap_or_else(|| panic!("{pattern:?} did not translate"))
    }

    fn not_extended(pattern: &str) {
        assert_eq!(translate_extended(pattern), None, "{pattern:?}");
    }

    #[test]
    fn an_extended_pattern_keeps_the_operators_both_dialects_share() {
        assert_eq!(extended("foo|bar"), "foo|bar");
        assert_eq!(extended("(a|b)+c?d*"), "(a|b)+c?d*");
        assert_eq!(extended("x{2}y{2,}z{2,5}"), "x{2}y{2,}z{2,5}");
        assert_eq!(extended("^a$"), "^a$");
        assert_eq!(extended("[^a-z]"), "[^a-z]");
        assert_eq!(extended(r"a\.b\(c\)\{"), r"a\.b[(]c[)][{]");
        assert_eq!(extended("a}"), "a[}]");
    }

    #[test]
    fn a_backslash_before_an_ordinary_character_is_not_translated() {
        for escape in [r"\d", r"\s", r"\w", r"\b", r"\<", r"\n", r"\1", r"\-"] {
            not_extended(&format!("v{escape}"));
        }
        not_extended(r"a\");
        not_extended(r"(a)\1");
    }

    #[test]
    fn an_extended_quantifier_with_nothing_to_repeat_or_stacked_is_not_translated() {
        for pattern in [
            "*foo", "+a", "?a", "a|*b", "(*a)", "^*", "a**", "a+?", "a{2}*", "{2}",
        ] {
            not_extended(pattern);
        }
    }

    #[test]
    fn an_extended_interval_the_crate_reads_differently_is_not_translated() {
        for pattern in ["a{,2}", "a{x}", "a{2", "a{3,1}", "a{40000}"] {
            not_extended(pattern);
        }
    }

    #[test]
    fn an_extended_bracket_or_group_the_crate_reads_differently_is_not_translated() {
        for pattern in [
            r"import [^\s]+",
            "[a[b]",
            "[a&&b]",
            "[a--b]",
            "[a~~b]",
            "[]a]",
            "()",
            "(a",
            "a)",
        ] {
            not_extended(pattern);
        }
    }

    #[test]
    fn every_translation_here_compiles_as_a_regex() {
        for pattern in [
            r"alpha\|beta",
            "call(x)",
            r"x\{2,5\}",
            "x{2}",
            "a$b",
            "a^*",
            "*a",
            r"\(^*a\)",
            "[(|)]",
            "[[:digit:]]x",
            r"a\/b",
            "}",
        ] {
            let translation = translated(pattern);
            assert!(
                regex::Regex::new(&translation).is_ok(),
                "{pattern:?} → {translation:?}"
            );
        }
    }
}
