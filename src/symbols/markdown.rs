use super::SymbolMatch;
use crate::output::Resolver;

/// Not tree-sitter: an `atx_heading` node spans its own line alone, not the section under it.
pub(super) fn resolve(content: &str, path: &[String]) -> Vec<SymbolMatch> {
    let [wanted] = path else {
        return Vec::new();
    };
    let lines = line_spans(content);
    sections(content, &lines)
        .into_iter()
        .filter(|(text, _, _)| *text == wanted.as_str())
        .map(|(_, index, last)| {
            let (start, end) = lines[index];
            SymbolMatch {
                start,
                end: lines[last].1,
                line: index + 1,
                end_line: last + 1,
                text: content[start..end].to_owned(),
                resolver: Resolver::Heuristic("heading"),
                end_guessed: false,
            }
        })
        .collect()
}

/// Every heading's text with its section's 1-based first and last lines, in one pass.
pub(super) fn spans(content: &str) -> Vec<(&str, usize, usize)> {
    sections(content, &line_spans(content))
        .into_iter()
        .map(|(text, index, last)| (text, index + 1, last + 1))
        .collect()
}

/// A section runs to the line before the next heading of its level or higher. A heading is
/// passed over only by the nearest earlier heading of each shallower level, so the pass is linear.
fn sections<'a>(content: &'a str, lines: &[(usize, usize)]) -> Vec<(&'a str, usize, usize)> {
    let headings = headings(content, lines);
    headings
        .iter()
        .enumerate()
        .map(|(i, &(index, level, text))| {
            let next = headings[i + 1..]
                .iter()
                .find(|(_, deeper, _)| *deeper <= level)
                .map(|&(start, _, _)| start);
            (text, index, next.map_or(lines.len() - 1, |start| start - 1))
        })
        .collect()
}

fn headings<'a>(content: &'a str, lines: &[(usize, usize)]) -> Vec<(usize, usize, &'a str)> {
    let mut headings = Vec::new();
    let mut fence: Option<&str> = None;
    for (index, &(start, end)) in lines.iter().enumerate() {
        let line = &content[start..end];
        if let Some(open) = fence {
            if line.trim_start().starts_with(open) {
                fence = None;
            }
            continue;
        }
        if let Some(open) = opening_fence(line) {
            fence = Some(open);
            continue;
        }
        let level = line.bytes().take_while(|b| *b == b'#').count();
        if (1..=6).contains(&level) && line[level..].starts_with(char::is_whitespace) {
            headings.push((index, level, line[level..].trim()));
        }
    }
    headings
}

fn opening_fence(line: &str) -> Option<&'static str> {
    let trimmed = line.trim_start();
    ["```", "~~~"]
        .into_iter()
        .find(|fence| trimmed.starts_with(fence))
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
