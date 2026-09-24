use super::{Applied, Op};

const FENCE: &str = "---";

const NO_OPENING_FENCE: &str = "no opening `---` fence on line 1";

/// The opening fence is line 1, and `detect` rejects a `.md` whose first line is not a fence.
const BODY_LINE_OFFSET: usize = 1;

pub fn apply(file: &std::path::Path, content: &str, ops: &[Op]) -> Result<Applied, crate::Error> {
    let body = body_range(content).map_err(|what| crate::Error::NotFound {
        target: "the document".to_owned(),
        what: what.to_owned(),
        nearest: None,
    })?;

    let applied = super::yaml::apply(file, &content[body.clone()], ops).map_err(|mut err| {
        match &mut err {
            crate::Error::Ambiguous { candidates, .. } => {
                for candidate in candidates {
                    candidate.line += BODY_LINE_OFFSET;
                }
            },
            crate::Error::NotFound {
                nearest: Some(candidate),
                ..
            } => candidate.line += BODY_LINE_OFFSET,
            _ => {},
        }
        err
    })?;

    // The fences are spliced from the original bytes, so a CRLF document keeps its line endings.
    let mut text = String::with_capacity(content.len() - body.len() + applied.text.len());
    text.push_str(&content[..body.start]);
    text.push_str(&applied.text);
    text.push_str(&content[body.end..]);

    Ok(Applied {
        text,
        touched: applied
            .touched
            .into_iter()
            .map(|(op, line)| (op, line + BODY_LINE_OFFSET))
            .collect(),
    })
}

fn body_range(content: &str) -> Result<std::ops::Range<usize>, &'static str> {
    let mut offset = 0;
    let mut body_start = None;

    for line in content.split_inclusive('\n') {
        let text = line.strip_suffix('\n').unwrap_or(line);
        let text = text.strip_suffix('\r').unwrap_or(text);

        if text == FENCE {
            match body_start {
                None => body_start = Some(offset + line.len()),
                Some(start) => return Ok(start..offset),
            }
        } else if body_start.is_none() {
            return Err(NO_OPENING_FENCE);
        }
        offset += line.len();
    }

    if body_start.is_none() {
        return Err(NO_OPENING_FENCE);
    }
    Err("no closing `---` fence")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transform::{TransformOp, parse_path, parse_value};

    const NOTE: &str = "---\ntitle: Notes\ntags: [moc]\nlast_updated: 2026-07-12\n---\n# Notes\n\nBody text unrelated to the frontmatter.\n";

    fn note() -> std::path::PathBuf {
        std::path::Path::new("tests/fixtures/transform/docs/note.md").to_path_buf()
    }

    fn set(key: &str, raw_value: &str) -> Op {
        Op::Set {
            path: parse_path(key).unwrap(),
            raw_key: key.to_owned(),
            value: parse_value(raw_value),
        }
    }

    #[test]
    fn set_changes_one_frontmatter_line_and_leaves_the_body_byte_identical() {
        let applied = apply(&note(), NOTE, &[set("last_updated", "2026-09-16")]).unwrap();

        assert_eq!(
            applied.text,
            "---\ntitle: Notes\ntags: [moc]\nlast_updated: 2026-09-16\n---\n# Notes\n\nBody text \
             unrelated to the frontmatter.\n"
        );
    }

    #[test]
    fn a_frontmatter_line_is_numbered_from_the_top_of_the_file() {
        let applied = apply(&note(), NOTE, &[set("last_updated", "2026-09-16")]).unwrap();

        assert!(matches!(applied.touched.as_slice(), [(
            TransformOp::Set { .. },
            4
        )]));
    }

    #[test]
    fn a_document_with_no_closing_fence_is_rejected() {
        let err = apply(&note(), "---\ntitle: Notes\n# Notes\n", &[set(
            "title", "x",
        )])
        .unwrap_err();
        assert!(err.to_string().contains("no closing `---` fence"), "{err}");
    }

    #[test]
    fn a_fence_that_is_not_the_first_line_is_rejected() {
        let err = apply(&note(), "# Notes\n\n---\ntitle: Notes\n---\n", &[set(
            "title", "x",
        )])
        .unwrap_err();
        assert!(
            err.to_string().contains("no opening `---` fence on line 1"),
            "{err}"
        );
    }

    fn note_crlf() -> String {
        NOTE.replace('\n', "\r\n")
    }

    #[test]
    fn crlf_fences_and_body_round_trip() {
        let applied = apply(&note(), &note_crlf(), &[set("last_updated", "2026-09-16")]).unwrap();

        assert_eq!(
            applied.text,
            note_crlf().replace("2026-07-12", "2026-09-16")
        );
    }

    #[test]
    fn a_key_added_to_crlf_frontmatter_takes_the_crlf_ending_too() {
        let applied = apply(&note(), &note_crlf(), &[set("author", "ada")]).unwrap();

        assert_eq!(
            applied.text,
            note_crlf().replace(
                "last_updated: 2026-07-12\r\n---",
                "last_updated: 2026-07-12\r\nauthor: ada\r\n---"
            )
        );
    }

    #[test]
    fn a_horizontal_rule_after_the_frontmatter_stays_in_the_body() {
        let applied = apply(
            &note(),
            "---\ntitle: Notes\n---\n# Notes\n\n---\n\nMore.\n",
            &[set("title", "Renamed")],
        )
        .unwrap();

        assert_eq!(
            applied.text,
            "---\ntitle: Renamed\n---\n# Notes\n\n---\n\nMore.\n"
        );
    }

    const TWO_WEBS: &str = "---\ntitle: Stack\nservices:\n  - name: web\n    image: a\n  - name: \
                            web\n    image: b\n---\n# Stack\n";

    fn candidate_lines(err: &crate::Error) -> Vec<usize> {
        let crate::Error::Ambiguous { candidates, .. } = err else {
            panic!("{err:?}");
        };
        candidates.iter().map(|candidate| candidate.line).collect()
    }

    #[test]
    fn an_ambiguous_selector_names_the_file_s_lines_not_the_body_s() {
        let err = apply(&note(), TWO_WEBS, &[set("services[name=web].image", "c")]).unwrap_err();
        assert_eq!(candidate_lines(&err), [4, 6]);
    }

    #[test]
    fn the_same_ambiguity_in_a_yaml_file_is_not_shifted() {
        let body = &TWO_WEBS["---\n".len()..TWO_WEBS.find("---\n# Stack").unwrap()];
        let err = crate::transform::yaml::apply(std::path::Path::new("stack.yaml"), body, &[set(
            "services[name=web].image",
            "c",
        )])
        .unwrap_err();
        assert_eq!(candidate_lines(&err), [3, 5]);
    }

    #[test]
    fn an_empty_frontmatter_block_is_rejected_rather_than_silently_creating_a_key() {
        let err = apply(&note(), "---\n---\n# Notes\n", &[set("title", "x")]).unwrap_err();
        assert!(err.to_string().contains("title"), "{err}");
    }
}
