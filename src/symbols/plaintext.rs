use super::SymbolMatch;
use crate::output::Resolver;

const DEFINITION_KEYWORDS: &[&str] = &[
    "fn",
    "func",
    "function",
    "def",
    "class",
    "struct",
    "interface",
    "enum",
    "type",
    "fun",
    "trait",
    "impl",
    "object",
    "module",
    "sub",
    "proc",
    "record",
    "const",
    "let",
    "val",
    "var",
];

pub fn resolve(content: &str, path: &[String]) -> Vec<SymbolMatch> {
    let Some((name, outer)) = path.split_last().filter(|(n, _)| !n.is_empty()) else {
        return Vec::new();
    };
    let lines = line_spans(content);
    let mut found = Vec::new();
    for (index, &(start, end)) in lines.iter().enumerate() {
        let line = &content[start..end];
        let Some(at) = hit(line, name) else {
            continue;
        };
        if !outer.is_empty() && !enclosed_by(content, &lines, index, outer) {
            continue;
        }
        let (end, end_line, end_guessed) =
            if let Some((end, end_line)) = brace_end(content, &lines, index, start + at) {
                (end, end_line, false)
            } else {
                let last = indented_end(content, &lines, index);
                let last = closer_after(content, &lines, index, last).unwrap_or(last);
                (lines[last].1, last, true)
            };
        found.push(SymbolMatch {
            start: start + indent_width(line),
            end,
            line: index + 1,
            end_line: end_line + 1,
            text: line.to_owned(),
            resolver: Resolver::Heuristic("plaintext"),
            end_guessed,
        });
    }
    found
}

fn hit(line: &str, name: &str) -> Option<usize> {
    line.match_indices(name).map(|(at, _)| at).find(|&at| {
        let after = &line[at + name.len()..];
        let whole = !line[..at].ends_with(is_word) && !after.starts_with(is_word);
        whole && (after.starts_with('(') || after_keyword(&line[..at]))
    })
}

/// An enclosing line has a `{` still open at the hit, or is less indented than every line between.
fn enclosed_by(content: &str, lines: &[(usize, usize)], index: usize, outer: &[String]) -> bool {
    let (start, end) = lines[index];
    let mut indent = indent_width(&content[start..end]);
    let mut unopened = 0usize;
    let mut wanted = outer.iter().rev().peekable();
    for &(start, end) in lines[..index].iter().rev() {
        let Some(segment) = wanted.peek() else {
            break;
        };
        let line = &content[start..end];
        let mut opens_block = false;
        for byte in line.bytes().rev() {
            match byte {
                b'}' => unopened += 1,
                b'{' if unopened > 0 => unopened -= 1,
                b'{' => opens_block = true,
                _ => {},
            }
        }
        let outdented = !line.trim().is_empty() && indent_width(line) < indent;
        if !opens_block && !outdented {
            continue;
        }
        indent = indent.min(indent_width(line));
        if has_word(line, segment) {
            wanted.next();
        }
    }
    wanted.peek().is_none()
}

fn has_word(line: &str, word: &str) -> bool {
    !word.is_empty()
        && line.match_indices(word).any(|(at, _)| {
            !line[..at].ends_with(is_word) && !line[at + word.len()..].starts_with(is_word)
        })
}

fn after_keyword(before: &str) -> bool {
    let trimmed = before.trim_end();
    if trimmed.len() == before.len() {
        return false;
    }
    let word_start = trimmed
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or(trimmed.len(), |(at, _)| at);
    DEFINITION_KEYWORDS.contains(&&trimmed[word_start..])
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A `{` elsewhere on the next line is a body statement; only a leading one opens the definition.
fn brace_end(
    content: &str,
    lines: &[(usize, usize)],
    index: usize,
    name_at: usize,
) -> Option<(usize, usize)> {
    let (_, hit_end) = lines[index];
    let open = if let Some(offset) = content[name_at..hit_end].find('{') {
        name_at + offset
    } else {
        let &(start, end) = lines[index + 1..]
            .iter()
            .find(|&&(start, end)| !content[start..end].trim().is_empty())?;
        let line = &content[start..end];
        line.trim_start()
            .starts_with('{')
            .then(|| start + indent_width(line))?
    };
    let mut depth = 0usize;
    for (offset, byte) in content.as_bytes()[open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    let close = open + offset;
                    let line = lines.partition_point(|&(start, _)| start <= close) - 1;
                    return Some((close + 1, line));
                }
            },
            _ => {},
        }
    }
    None
}

fn indented_end(content: &str, lines: &[(usize, usize)], index: usize) -> usize {
    let (start, end) = lines[index];
    let depth = indent_width(&content[start..end]);
    let mut last = index;
    for (offset, &(start, end)) in lines[index + 1..].iter().enumerate() {
        let line = &content[start..end];
        if line.trim().is_empty() {
            continue;
        }
        if indent_width(line) <= depth {
            break;
        }
        last = index + 1 + offset;
    }
    last
}

/// Without these, the span of `function f() … end` stops above `end` and an insert lands inside.
const CLOSERS: &[&str] = &["end", "end)", "end,", "}", "};", "},", "fi", "done", "esac"];

fn closer_after(
    content: &str,
    lines: &[(usize, usize)],
    index: usize,
    last: usize,
) -> Option<usize> {
    if last == index {
        return None;
    }
    let (start, end) = lines[index];
    let depth = indent_width(&content[start..end]);
    let (offset, line) = lines[last + 1..]
        .iter()
        .map(|&(start, end)| &content[start..end])
        .enumerate()
        .find(|(_, line)| !line.trim().is_empty())?;
    (indent_width(line) == depth && CLOSERS.contains(&line.trim())).then_some(last + 1 + offset)
}

fn indent_width(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn line_spans(content: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0;
    for (i, byte) in content.bytes().enumerate() {
        if byte != b'\n' {
            continue;
        }
        let end = if i > start && content.as_bytes()[i - 1] == b'\r' {
            i - 1
        } else {
            i
        };
        spans.push((start, end));
        start = i + 1;
    }
    if start < content.len() {
        spans.push((start, content.len()));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(content: &str, path: &[&str]) -> Vec<SymbolMatch> {
        let path: Vec<String> = path.iter().map(|s| (*s).to_owned()).collect();
        resolve(content, &path)
    }

    const KOTLIN: &str = "\
package demo

fun greet(name: String): String {
    val prefix = \"hi\"
    return \"$prefix $name\"
}

val limit = 20
";

    #[test]
    fn a_keyword_definition_with_a_brace_body_spans_to_its_matching_brace() {
        let found = find(KOTLIN, &["greet"]);

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].line, found[0].end_line), (3, 6));
        assert_eq!(
            &KOTLIN[found[0].start..found[0].end],
            "fun greet(name: String): String {\n    val prefix = \"hi\"\n    return \
             \"$prefix $name\"\n}"
        );
        assert_eq!(found[0].text, "fun greet(name: String): String {");
        assert_eq!(found[0].resolver, Resolver::Heuristic("plaintext"));
        assert!(!found[0].end_guessed, "a matched brace is a known end");
    }

    #[test]
    fn a_keyword_definition_with_no_body_is_its_one_line_and_its_end_is_a_guess() {
        let found = find(KOTLIN, &["limit"]);

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].line, found[0].end_line), (8, 8));
        assert_eq!(&KOTLIN[found[0].start..found[0].end], "val limit = 20");
        assert!(found[0].end_guessed);
    }

    const PYTHONISH: &str = "\
proc build(target):
    step one

    step two
after
";

    #[test]
    fn with_no_brace_the_span_runs_over_the_more_indented_lines_and_is_a_guess() {
        let found = find(PYTHONISH, &["build"]);

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].line, found[0].end_line), (1, 4));
        assert_eq!(
            &PYTHONISH[found[0].start..found[0].end],
            "proc build(target):\n    step one\n\n    step two"
        );
        assert!(found[0].end_guessed);
    }

    const LUA: &str = "\
function setup()
  return 1
end

function run_job()
  local ok = true
  return ok
end
";

    #[test]
    fn a_lone_closer_at_the_headers_indentation_ends_a_guessed_span() {
        let found = find(LUA, &["run_job"]);

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].line, found[0].end_line), (5, 8));
        assert_eq!(
            &LUA[found[0].start..found[0].end],
            "function run_job()\n  local ok = true\n  return ok\nend"
        );
        assert!(
            found[0].end_guessed,
            "a closer is still a guess, not a parse"
        );
    }

    #[test]
    fn every_listed_closer_joins_the_span_and_a_trailing_word_does_not() {
        for closer in ["end", "end)", "end,", "}", "};", "},", "fi", "done", "esac"] {
            let source = format!("  proc go()\n    step\n  {closer}\nafter\n");
            let found = find(&source, &["go"]);
            assert_eq!(found[0].end_line, 3, "{closer}");
        }
        let found = find("proc go()\n  step\nend -- go\n", &["go"]);
        assert_eq!(
            found[0].end_line, 2,
            "a closer with more on its line is not lone"
        );
    }

    #[test]
    fn a_closer_at_another_indentation_keeps_the_indented_guess() {
        let source = "  function run_job()\n    return ok\nend\n";
        let found = find(source, &["run_job"]);

        assert_eq!((found[0].line, found[0].end_line), (1, 2));
        assert!(found[0].end_guessed);
    }

    #[test]
    fn a_header_with_no_indented_body_opens_no_block_for_a_closer_to_end() {
        let found = find("val limit = 20\nend\n", &["limit"]);

        assert_eq!((found[0].line, found[0].end_line), (1, 1));
    }

    #[test]
    fn a_brace_matched_span_ignores_a_closer_after_its_brace() {
        let source = "fun render() {\n    draw()\n}\nend\n";
        let found = find(source, &["render"]);

        assert_eq!((found[0].line, found[0].end_line), (1, 3));
        assert!(!found[0].end_guessed);
    }

    #[test]
    fn a_brace_leading_the_next_line_opens_the_body() {
        let source = "void run()\n{\n    go();\n}\nrest\n";
        let found = find(source, &["run"]);

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].line, found[0].end_line), (1, 4));
        assert!(!found[0].end_guessed);
    }

    #[test]
    fn a_brace_later_on_the_next_line_is_a_statement_not_the_opener() {
        let source = "def load(path):\n    seen = {}\n    return seen\nrest\n";
        let found = find(source, &["load"]);

        assert_eq!((found[0].line, found[0].end_line), (1, 3));
        assert!(found[0].end_guessed);
    }

    #[test]
    fn a_name_before_a_paren_is_a_hit_without_a_keyword() {
        let source = "static int count(void) {\n  return 1;\n}\n";
        let found = find(source, &["count"]);

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].line, found[0].end_line), (1, 3));
    }

    #[test]
    fn every_qualifying_line_is_returned_for_the_caller_to_call_ambiguous() {
        let source = "fun open() {}\n\nclass Door {\n  fun open() {}\n}\n";
        let found = find(source, &["open"]);

        assert_eq!(
            found.iter().map(|m| m.line).collect::<Vec<_>>(),
            [1, 4],
            "{found:?}"
        );
    }

    #[test]
    fn a_name_neither_after_a_keyword_nor_before_a_paren_is_not_a_hit() {
        assert!(find("x = greet\nreturn greet\n", &["greet"]).is_empty());
        assert!(find("fn greeter() {}\nfn pregreet() {}\n", &["greet"]).is_empty());
        assert!(find("undef greet\n", &["greet"]).is_empty());
    }

    #[test]
    fn a_qualifier_no_enclosing_line_names_is_not_found() {
        assert!(find(KOTLIN, &["Nope", "greet"]).is_empty());
    }

    const DOOR: &str = "fun open() {}\n\nclass Door {\n  fun close() {\n  }\n  fun open() {}\n}\n";

    #[test]
    fn a_qualified_name_resolves_only_inside_the_block_its_qualifier_opens() {
        let found = find(DOOR, &["Door", "open"]);

        assert_eq!(found.iter().map(|m| m.line).collect::<Vec<_>>(), [6]);
        assert_eq!(found[0].text, "  fun open() {}");
    }

    #[test]
    fn a_qualifier_on_a_sibling_block_does_not_enclose() {
        assert!(find(DOOR, &["close", "open"]).is_empty());
    }

    #[test]
    fn outer_segments_must_enclose_in_order_and_indentation_counts_as_enclosing() {
        let source = "module Shop\n  class Door\n    def open(x)\n    end\n  end\nend\n";

        assert_eq!(
            find(source, &["Shop", "Door", "open"])
                .iter()
                .map(|m| m.line)
                .collect::<Vec<_>>(),
            [3]
        );
        assert!(find(source, &["Door", "Shop", "open"]).is_empty());
    }

    #[test]
    fn an_empty_path_names_nothing() {
        assert!(resolve(KOTLIN, &[]).is_empty());
    }
}
