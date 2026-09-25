use std::cmp::Reverse;
use std::ops::Range;

use crate::output::{Line, Omission, TargetBlock};

/// Always `1 <= start <= end <= total`; a file with no lines has no `Bounds`, hence `Option`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub start: usize,
    pub end: usize,
}

impl Bounds {
    fn checked(start: usize, end: usize, total: usize) -> Option<Bounds> {
        (start >= 1 && start <= end && end <= total).then_some(Bounds { start, end })
    }
}

pub fn window(total: usize, size: usize) -> (Option<Bounds>, Option<Omission>) {
    if total <= size {
        return (Bounds::checked(1, total, total), None);
    }
    (
        Bounds::checked(1, size, total),
        Some(Omission::Window {
            shown: (1, size),
            total,
        }),
    )
}

pub fn context(center: usize, before: usize, after: usize, total: usize) -> Option<Bounds> {
    if center == 0 || center > total {
        return None;
    }
    Bounds::checked(
        center.saturating_sub(before).max(1),
        (center + after).min(total),
        total,
    )
}

pub fn range(a: usize, b: usize, total: usize) -> Option<Bounds> {
    let (start, end) = if a > b { (b, a) } else { (a, b) };
    Bounds::checked(start, end.min(total), total)
}

/// One minified line must not flood the context, or push `find` into refusing a search, alone.
pub const LINE_DISPLAY_CAP: usize = 1_000;
pub const HIT_CONTEXT_BYTES: usize = 200;

const CUT_MARK: char = '\u{2026}';

/// `hits[i]` is line `i`'s first match span, markers included; the caller passes it because a `«`
/// in the text may be the file's own.
pub fn cut_long_lines(lines: &mut [Line], hits: &[Option<Range<usize>>]) -> usize {
    let mut cut = 0;
    for (index, line) in lines.iter_mut().enumerate() {
        let text = &line.text;
        if text.len() <= LINE_DISPLAY_CAP {
            continue;
        }
        let (start, end) = hits
            .get(index)
            .cloned()
            .flatten()
            .and_then(|hit| hit_window(text, hit))
            .unwrap_or((0, text.floor_char_boundary(LINE_DISPLAY_CAP)));
        let mut kept = String::with_capacity(end - start + 2 * CUT_MARK.len_utf8());
        if start > 0 {
            kept.push(CUT_MARK);
        }
        kept.push_str(&text[start..end]);
        if end < text.len() {
            kept.push(CUT_MARK);
        }
        line.text = kept.into();
        cut += 1;
    }
    cut
}

fn hit_window(text: &str, hit: Range<usize>) -> Option<(usize, usize)> {
    text.get(hit.clone())?;
    let start = text.ceil_char_boundary(hit.start.saturating_sub(HIT_CONTEXT_BYTES));
    let end = text.floor_char_boundary(hit.end + HIT_CONTEXT_BYTES);
    let end = if end - start > LINE_DISPLAY_CAP {
        text.floor_char_boundary(start + LINE_DISPLAY_CAP)
    } else {
        end
    };
    Some((start, end))
}

pub fn trim_to_bytes(mut lines: Vec<Line>, budget_bytes: usize) -> (Vec<Line>, bool) {
    let mut used = 0;
    for (kept, line) in lines.iter().enumerate() {
        used += line.text.len() + 1;
        if used > budget_bytes {
            lines.truncate(kept);
            return (lines, true);
        }
    }
    (lines, false)
}

/// Counts each line's newline, so a trim and the cost line agree on the same block.
pub fn content_bytes(lines: &[Line]) -> usize {
    lines.iter().map(|line| line.text.len() + 1).sum()
}

/// A `--budget` flag is a token count; find and show both convert it to bytes at this ratio.
pub const BYTES_PER_TOKEN: usize = 4;

/// A trimmed target keeps its first line: an empty block is a header the footer cannot explain.
pub fn trim_to_budget(blocks: &mut [TargetBlock], budget: usize) -> Vec<Omission> {
    let limit = budget.saturating_mul(BYTES_PER_TOKEN);
    let mut omissions = Vec::new();
    let mut trimmed = vec![false; blocks.len()];
    loop {
        let total: usize = blocks.iter().map(|block| content_bytes(&block.lines)).sum();
        if total <= limit {
            return omissions;
        }
        let largest = blocks
            .iter()
            .enumerate()
            .filter(|(index, _)| !trimmed[*index])
            .max_by_key(|(index, block)| (content_bytes(&block.lines), Reverse(*index)))
            .map(|(index, _)| index);
        let Some(index) = largest else {
            return omissions;
        };
        trimmed[index] = true;

        let block = &mut blocks[index];
        let end_before = block.lines.last().map_or(0, |line| line.number);
        let bytes = content_bytes(&block.lines);
        let floor = block.lines.first().map_or(0, |line| line.text.len() + 1);
        let keep = bytes.saturating_sub(total - limit).max(floor);
        if keep >= bytes {
            continue;
        }
        let (kept, shortened) = trim_to_bytes(std::mem::take(&mut block.lines), keep);
        block.lines = kept;
        if !shortened {
            continue;
        }
        let last = block.lines.last().map_or(0, |line| line.number);
        if let Some(span) = block.span.as_mut() {
            span.end = last;
        }
        // A `find` hit block has no `span`, so it gets no "not shown" tail.
        let file_lines = block.span.map_or(0, |span| span.total);
        block.not_shown = (last < file_lines).then_some((last + 1, file_lines));
        omissions.push(Omission::Budget {
            budget,
            trimmed_target: block.target.clone(),
            not_shown: block.span.map(|_| (last + 1, end_before)),
        });
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::output::Marker;

    fn bounds(start: usize, end: usize) -> Bounds {
        Bounds { start, end }
    }

    #[test]
    fn window_243_200_shows_the_first_200_and_names_what_is_left() {
        let (found, omission) = window(243, 200);
        assert_eq!(found, Some(bounds(1, 200)));
        match omission {
            Some(Omission::Window { shown, total }) => {
                assert_eq!(shown, (1, 200));
                assert_eq!(total, 243);
            },
            other => panic!("expected Omission::Window, got {other:?}"),
        }
    }

    #[test]
    fn total_at_or_under_size_shows_everything_with_no_omission() {
        let (found, omission) = window(50, 200);
        assert_eq!(found, Some(bounds(1, 50)));
        assert!(omission.is_none());

        let (found, omission) = window(200, 200);
        assert_eq!(found, Some(bounds(1, 200)));
        assert!(omission.is_none());
    }

    #[test]
    fn an_empty_file_has_nothing_to_show_and_nothing_to_omit() {
        let (found, omission) = window(0, 200);
        assert_eq!(found, None);
        assert!(omission.is_none());
        assert_eq!(context(1, 0, 0, 0), None);
        assert_eq!(range(1, 1, 0), None);
    }

    #[test]
    fn context_41_0_30_212_is_bounds_41_71() {
        assert_eq!(context(41, 0, 30, 212), Some(bounds(41, 71)));
    }

    #[test]
    fn context_start_clamps_to_1() {
        assert_eq!(context(5, 30, 0, 212), Some(bounds(1, 5)));
    }

    #[test]
    fn context_around_a_line_the_file_lacks_is_nothing_to_show() {
        assert_eq!(context(0, 2, 2, 212), None);
        assert_eq!(context(213, 2, 2, 212), None);
        assert_eq!(context(400, 300, 0, 212), None);
    }

    #[test]
    fn range_549_556_812_is_bounds_549_556() {
        assert_eq!(range(549, 556, 812), Some(bounds(549, 556)));
    }

    #[test]
    fn range_end_clamps_to_total() {
        assert_eq!(range(5, 900, 812), Some(bounds(5, 812)));
    }

    #[test]
    fn range_swaps_reversed_bounds() {
        assert_eq!(range(556, 549, 812), Some(bounds(549, 556)));
    }

    #[test]
    fn a_range_wholly_past_the_end_is_nothing_to_show() {
        assert_eq!(range(813, 900, 812), None);
    }

    #[test]
    fn line_zero_is_not_a_line() {
        assert_eq!(range(0, 5, 812), None);
        assert_eq!(range(0, 0, 812), None);
    }

    fn line(number: usize, text: &str) -> Line {
        Line {
            number,
            marker: Marker::None,
            text: text.to_owned().into(),
        }
    }

    #[test]
    fn trim_to_bytes_stops_before_the_line_that_would_exceed_budget() {
        let lines = vec![line(1, "1234"), line(2, "1234"), line(3, "1234")];
        let (kept, trimmed) = trim_to_bytes(lines, 10);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[1].number, 2);
        assert!(trimmed);
    }

    #[test]
    fn trim_to_bytes_keeps_everything_under_budget() {
        let lines = vec![line(1, "12"), line(2, "12")];
        let (kept, trimmed) = trim_to_bytes(lines, 100);
        assert_eq!(kept.len(), 2);
        assert!(!trimmed);
    }

    fn cut_one(text: &str, hit: Option<Range<usize>>) -> (String, usize) {
        let mut lines = vec![line(1, text)];
        let cut = cut_long_lines(&mut lines, &[hit]);
        (lines.remove(0).text.into_owned(), cut)
    }

    fn pair_at(start: usize, pair: &str) -> Range<usize> {
        start..start + pair.len()
    }

    #[test]
    fn a_line_at_or_under_the_cap_is_left_alone() {
        for len in [999, 1_000] {
            let text = "a".repeat(len);
            assert_eq!(cut_one(&text, None), (text.clone(), 0));
            assert_eq!(cut_one(&text, Some(0..1)), (text, 0));
        }
    }

    #[test]
    fn a_line_one_byte_over_the_cap_keeps_the_cap_and_marks_the_cut() {
        let (text, cut) = cut_one(&"a".repeat(1_001), None);
        assert_eq!(text, format!("{}…", "a".repeat(1_000)));
        assert_eq!(cut, 1);
    }

    #[test]
    fn a_cut_lands_on_a_char_boundary() {
        let (text, _) = cut_one(&"é".repeat(600), None);
        assert_eq!(text, format!("{}…", "é".repeat(500)));

        // Byte 1,000 falls inside an `é` here, so the cut has to step back one byte.
        let (text, _) = cut_one(&format!("a{}", "é".repeat(600)), None);
        assert_eq!(text, format!("a{}…", "é".repeat(499)));
    }

    fn letters(len: usize) -> String {
        (b'a'..=b'z').cycle().take(len).map(char::from).collect()
    }

    #[test]
    fn a_hit_keeps_200_bytes_each_side_of_its_pair() {
        let (before, after) = (letters(3_000), letters(1_990));
        let text = format!("{before}«needle»{after}");
        assert_eq!(text.len(), 5_000);

        let (kept, cut) = cut_one(&text, Some(pair_at(before.len(), "«needle»")));
        assert_eq!(
            kept,
            format!("…{}«needle»{}…", &before[2_800..], &after[..200])
        );
        assert_eq!(cut, 1);
    }

    #[test]
    fn a_hit_near_the_start_has_no_leading_mark() {
        let text = format!("{}«needle»{}", "p".repeat(10), "q".repeat(1_500));
        let (kept, _) = cut_one(&text, Some(pair_at(10, "«needle»")));
        assert_eq!(
            kept,
            format!("{}«needle»{}…", "p".repeat(10), "q".repeat(200))
        );
    }

    #[test]
    fn a_literal_guillemet_before_the_hit_does_not_move_the_window_off_it() {
        let text = format!("x«{}«needle»{}", "a".repeat(2_000), "b".repeat(50));
        let (kept, _) = cut_one(&text, Some(pair_at(2_003, "«needle»")));
        assert_eq!(
            kept,
            format!("…{}«needle»{}", "a".repeat(200), "b".repeat(50))
        );
    }

    #[test]
    fn a_line_with_no_hit_falls_back_to_the_plain_cut() {
        let (kept, cut) = cut_one(&"z".repeat(1_500), None);
        assert_eq!(kept, format!("{}…", "z".repeat(1_000)));
        assert_eq!(cut, 1);
    }

    #[test]
    fn a_hit_window_over_the_cap_keeps_the_cap_from_its_start() {
        let pair = format!("«{}»", "m".repeat(1_500));
        let text = format!("{}{pair}", "b".repeat(300));
        let (kept, _) = cut_one(&text, Some(pair_at(300, &pair)));
        // 200 context bytes and the two-byte `«` leave 798 of the cap for the match.
        assert_eq!(kept, format!("…{}«{}…", "b".repeat(200), "m".repeat(798)));
    }

    #[test]
    fn the_count_names_only_the_lines_that_were_cut() {
        let mut lines = vec![
            line(1, "short"),
            line(2, &"x".repeat(1_200)),
            line(3, &"y".repeat(1_000)),
            line(4, &"z".repeat(4_000)),
        ];
        assert_eq!(cut_long_lines(&mut lines, &[]), 2);
        assert_eq!(lines[0].text, "short");
        assert_eq!(lines[2].text, "y".repeat(1_000));
    }

    fn long_text() -> impl Strategy<Value = String> {
        let mixed = prop::sample::select(vec!['a', 'é', '日', '🦀', '«', '»', ' ']);
        prop_oneof![
            prop::collection::vec(any::<char>(), 0..1_500),
            prop::collection::vec(mixed, 0..1_500),
        ]
        .prop_map(String::from_iter)
    }

    proptest! {
        #[test]
        fn a_cut_line_stays_within_the_cap_and_a_short_one_is_untouched(
            text in long_text(),
            around_hit in any::<bool>(),
            a in any::<prop::sample::Index>(),
            b in any::<prop::sample::Index>(),
        ) {
            let boundaries: Vec<usize> =
                text.char_indices().map(|(at, _)| at).chain([text.len()]).collect();
            let (a, b) = (*a.get(&boundaries), *b.get(&boundaries));
            let hit = around_hit.then(|| a.min(b)..a.max(b));
            let (kept, cut) = cut_one(&text, hit);
            prop_assert!(std::str::from_utf8(kept.as_bytes()).is_ok());
            prop_assert!(kept.len() <= LINE_DISPLAY_CAP + 2 * '…'.len_utf8());
            if text.len() <= LINE_DISPLAY_CAP {
                prop_assert_eq!(kept, text);
                prop_assert_eq!(cut, 0);
            } else {
                prop_assert_eq!(cut, 1);
            }
        }

        #[test]
        fn range_either_holds_the_invariant_or_shows_nothing(
            total in 0usize..1000,
            a in 0usize..1500,
            b in 0usize..1500,
        ) {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            match range(a, b, total) {
                Some(found) => {
                    prop_assert!(1 <= found.start && found.start <= found.end && found.end <= total);
                    prop_assert_eq!(found.start, lo);
                    prop_assert_eq!(found.end, hi.min(total));
                },
                None => prop_assert!(lo == 0 || lo > total),
            }
        }

        #[test]
        fn context_either_holds_the_invariant_or_shows_nothing(
            total in 0usize..1000,
            center in 0usize..1500,
            before in 0usize..100,
            after in 0usize..100,
        ) {
            match context(center, before, after, total) {
                Some(found) => {
                    prop_assert!(1 <= found.start && found.start <= found.end && found.end <= total);
                    prop_assert!(found.start <= center && center <= found.end);
                },
                None => prop_assert!(center == 0 || center > total),
            }
        }

        #[test]
        fn window_under_total_shows_size_and_omits(
            total in 1usize..1000,
            size in 1usize..999,
        ) {
            prop_assume!(size < total);
            let (found, omission) = window(total, size);
            prop_assert_eq!(found, Some(Bounds { start: 1, end: size }));
            prop_assert!(omission.is_some());
        }
    }
}
