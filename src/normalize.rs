//! The smart-punctuation table, and only that table — no NFKC.

pub fn fold_char(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' => '\'',
        '\u{201C}' | '\u{201D}' => '"',
        '\u{2013}' | '\u{2014}' => '-',
        '\u{00A0}' => ' ',
        _ => c,
    }
}

/// `delta` is the original's cumulative byte surplus once this site is passed, so an offset
/// maps back as `original = folded + delta` of the last site before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldSite {
    pub folded: usize,
    pub delta: usize,
}

pub struct Folded {
    pub text: String,
    /// Ascending; one entry per folded character, so an all-ASCII file allocates nothing.
    pub sites: Vec<FoldSite>,
}

impl Folded {
    /// `folded` must be a char boundary of `text`.
    pub fn original_offset(&self, folded: usize) -> usize {
        let past = self.sites.partition_point(|site| site.folded < folded);
        folded
            + if past == 0 {
                0
            } else {
                self.sites[past - 1].delta
            }
    }

    /// Both ends land on original scalar boundaries, so a folded character maps back whole.
    pub fn original_span(&self, start: usize, end: usize) -> (usize, usize) {
        (self.original_offset(start), self.original_offset(end))
    }
}

pub fn fold(original: &str) -> Folded {
    let mut text = String::with_capacity(original.len());
    let mut sites = Vec::new();
    let mut delta = 0usize;
    for c in original.chars() {
        let ascii = fold_char(c);
        if ascii != c {
            delta += c.len_utf8() - ascii.len_utf8();
            sites.push(FoldSite {
                folded: text.len(),
                delta,
            });
        }
        text.push(ascii);
    }
    Folded { text, sites }
}

/// Lets a caller skip allocating a `Folded` for the common all-ASCII file.
pub fn contains_foldable(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0xc2 if bytes.get(i + 1) == Some(&0xa0) => return true,
            0xe2 if bytes.get(i + 1) == Some(&0x80)
                && matches!(
                    bytes.get(i + 2),
                    Some(0x93 | 0x94 | 0x98 | 0x99 | 0x9c | 0x9d)
                ) =>
            {
                return true;
            },
            _ => {},
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const TABLE: [char; 7] = [
        '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2013}', '\u{2014}', '\u{00A0}',
    ];

    #[test]
    fn fold_char_maps_each_table_entry_to_its_ascii_form() {
        assert_eq!(fold_char('\u{2018}'), '\'');
        assert_eq!(fold_char('\u{2019}'), '\'');
        assert_eq!(fold_char('\u{201C}'), '"');
        assert_eq!(fold_char('\u{201D}'), '"');
        assert_eq!(fold_char('\u{2013}'), '-');
        assert_eq!(fold_char('\u{2014}'), '-');
        assert_eq!(fold_char('\u{00A0}'), ' ');
    }

    #[test]
    fn fold_char_leaves_an_unmapped_character_unchanged() {
        assert_eq!(fold_char('é'), 'é');
        assert_eq!(fold_char('a'), 'a');
    }

    #[test]
    fn fold_maps_mixed_text_and_each_span_slices_back_to_the_original_char() {
        let original = "say \u{2018}hi\u{2019} \u{2014} caf\u{00E9}\u{00A0}now";
        let folded = fold(original);

        assert_eq!(folded.text, "say 'hi' - caf\u{00E9} now");
        assert_eq!(folded.sites.len(), 4);

        for (i, c) in folded.text.char_indices() {
            let (start, end) = folded.original_span(i, i + c.len_utf8());
            let original_char = original[start..end].chars().next().unwrap();
            assert_eq!(original[start..end].chars().count(), 1);
            assert_eq!(fold_char(original_char), c);
        }
    }

    #[test]
    fn fold_of_an_all_ascii_string_records_no_fold_site() {
        let folded = fold("plain ascii text\r\n");

        assert_eq!(folded.text, "plain ascii text\r\n");
        assert!(folded.sites.is_empty());
    }

    #[test]
    fn original_span_of_a_folded_dash_covers_all_three_of_its_original_bytes() {
        let original = "a\u{2014}b";
        let folded = fold(original);

        assert_eq!(folded.text, "a-b");
        assert_eq!(folded.original_span(1, 2), (1, 4));
        assert_eq!(folded.original_span(0, 3), (0, 5));
    }

    #[test]
    fn contains_foldable_true_for_each_table_character_on_its_own() {
        for c in TABLE {
            let text = format!("plain {c} text");
            assert!(
                contains_foldable(text.as_bytes()),
                "U+{:04X} is in the table",
                c as u32
            );
        }
    }

    #[test]
    fn contains_foldable_false_for_the_table_neighbours_and_for_empty_input() {
        // Each neighbour shares a lead byte with a table entry, catching an off-by-one.
        assert!(!contains_foldable("\u{00A1}Hola!".as_bytes()));
        assert!(!contains_foldable("a\u{2012}b".as_bytes()));
        assert!(!contains_foldable("a\u{2017}b".as_bytes()));
        assert!(!contains_foldable(b""));
    }

    #[test]
    fn contains_foldable_false_with_no_table_character_including_unrelated_multibyte_text() {
        assert!(!contains_foldable("plain ascii text".as_bytes()));
        assert!(!contains_foldable("café résumé".as_bytes()));
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
        proptest::collection::vec(alphabet, 0..40).prop_map(|cs| cs.into_iter().collect())
    }

    proptest! {
        #[test]
        fn folding_changes_only_the_table_characters_and_records_one_site_for_each(
            text in table_mixed_text()
        ) {
            let folded = fold(&text);
            prop_assert_eq!(folded.text.chars().count(), text.chars().count());
            prop_assert_eq!(
                folded.sites.len(),
                text.chars().filter(|&c| fold_char(c) != c).count()
            );
            for (folded_char, original_char) in folded.text.chars().zip(text.chars()) {
                prop_assert_eq!(folded_char, fold_char(original_char));
            }
        }

        #[test]
        fn a_folded_span_maps_back_to_original_bytes_that_fold_to_it(
            text in table_mixed_text(),
            a in any::<usize>(),
            b in any::<usize>(),
        ) {
            let folded = fold(&text);
            let bounds: Vec<usize> = folded
                .text
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(folded.text.len()))
                .collect();
            let (i, j) = (a % bounds.len(), b % bounds.len());
            let (lo, hi) = (bounds[i.min(j)], bounds[i.max(j)]);

            let (start, end) = folded.original_span(lo, hi);

            prop_assert_eq!(fold(&text[start..end]).text, folded.text[lo..hi].to_owned());
        }

        #[test]
        fn folding_an_all_ascii_string_is_identity_with_an_empty_map(
            text in "[ -~\n\r\t]{0,64}"
        ) {
            let folded = fold(&text);
            prop_assert!(folded.sites.is_empty());
            prop_assert_eq!(folded.text, text);
        }
    }
}
