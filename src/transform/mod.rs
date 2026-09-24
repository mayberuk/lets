use std::fmt::Write as _;

pub mod frontmatter;
pub mod json;
pub mod toml;
pub mod yaml;

pub use crate::output::{TransformFormat as Format, TransformOp};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Key(String),
    Index(usize),
    /// `[key=value]`: the element whose `key` scalar's string form equals `value`.
    Attr {
        key: String,
        value: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path(pub Vec<Segment>);

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Json(serde_json::Value),
    /// As typed: `serde_json` without `arbitrary_precision` would print `3.10` back as `3.1`.
    Number(String),
    /// As typed: `serde_json`'s map sorts keys, so `container` reads the typed order from here.
    Container(String),
}

#[derive(Debug, Clone)]
pub enum Op {
    Set {
        path: Path,
        raw_key: String,
        value: Value,
    },
    Delete {
        path: Path,
        raw_key: String,
    },
    Append {
        path: Path,
        raw_key: String,
        value: Value,
    },
}

#[derive(Debug)]
pub struct Applied {
    pub text: String,
    /// Each op with its 1-based line, in apply order.
    pub touched: Vec<(TransformOp, usize)>,
}

impl Value {
    /// What a scalar is written as over an existing string, which stays a string; `None` for `null`
    /// and containers, which replace it as typed.
    pub fn string_form(&self) -> Option<std::borrow::Cow<'_, str>> {
        match self {
            Value::Number(raw) => Some(raw.as_str().into()),
            Value::Json(serde_json::Value::String(text)) => Some(text.as_str().into()),
            Value::Json(serde_json::Value::Bool(flag)) => Some(flag.to_string().into()),
            Value::Json(serde_json::Value::Number(number)) => Some(number.to_string().into()),
            Value::Json(_) | Value::Container(_) => None,
        }
    }
}

pub fn container(raw: &str) -> jsonc_parser::ast::Value<'_> {
    jsonc_parser::parse_to_ast(
        raw,
        &jsonc_parser::CollectOptions::default(),
        &jsonc_parser::ParseOptions::default(),
    )
    .ok()
    .and_then(|parsed| parsed.value)
    .expect("a container is text serde_json parsed, and jsonc-parser reads a superset of JSON")
}

/// Values listed in a zero-match error.
const SEEN_CAP: usize = 20;

/// `fields(prefix, key)` gives, per element of the array at `prefix`, `key`'s scalar string form or
/// `None`; `line(element)` places a candidate.
pub fn resolve_selectors(
    file: &std::path::Path,
    path: &Path,
    raw_key: &str,
    mut fields: impl FnMut(&[Segment], &str) -> Result<Vec<Option<String>>, crate::Error>,
    mut line: impl FnMut(&[Segment]) -> usize,
) -> Result<(Path, String), crate::Error> {
    let selectors = path
        .0
        .iter()
        .filter(|segment| matches!(segment, Segment::Attr { .. }))
        .count();
    if selectors == 0 {
        return Ok((path.clone(), raw_key.to_owned()));
    }
    let spans = selector_spans(raw_key);
    let rewrite = spans.len() == selectors;
    let mut spans = spans.into_iter();
    let mut resolved = Vec::with_capacity(path.0.len());
    let mut key_text = String::with_capacity(raw_key.len());
    let mut copied = 0;

    for segment in &path.0 {
        let Segment::Attr { key, value } = segment else {
            resolved.push(segment.clone());
            continue;
        };
        if let Some((start, end)) = spans.next().filter(|_| rewrite) {
            key_text.push_str(&raw_key[copied..start]);
            copied = end;
        }
        let elements = fields(&resolved, key)?;
        let matches: Vec<usize> = elements
            .iter()
            .enumerate()
            .filter(|(_, field)| field.as_deref() == Some(value.as_str()))
            .map(|(index, _)| index)
            .collect();
        let index = match matches[..] {
            [index] => index,
            [] => {
                return Err(crate::Error::NotFound {
                    target: raw_key.to_owned(),
                    what: format!("an element with {key}={value:?} ({})", seen(&elements, key)),
                    nearest: None,
                });
            },
            _ => {
                let candidates = matches
                    .iter()
                    .map(|&index| {
                        let mut element = resolved.clone();
                        element.push(Segment::Index(index));
                        crate::error::Candidate {
                            path: file.to_path_buf(),
                            line: line(&element),
                            text: format!("{key_text}[{index}].{key}={value:?}"),
                        }
                    })
                    .collect();
                return Err(crate::Error::Ambiguous {
                    target: raw_key.to_owned(),
                    candidates,
                });
            },
        };
        key_text.push('[');
        key_text.push_str(&index.to_string());
        key_text.push(']');
        resolved.push(Segment::Index(index));
    }
    if !rewrite {
        return Ok((Path(resolved), raw_key.to_owned()));
    }
    key_text.push_str(&raw_key[copied..]);
    Ok((Path(resolved), key_text))
}

fn seen(elements: &[Option<String>], key: &str) -> String {
    if elements.is_empty() {
        return "the array has no elements".to_owned();
    }
    let mut distinct: Vec<&str> = Vec::new();
    for field in elements.iter().flatten() {
        if !distinct.contains(&field.as_str()) {
            distinct.push(field);
        }
    }
    if distinct.is_empty() {
        return format!("no element has a scalar {key} field");
    }
    let mut out = "values seen: ".to_owned();
    for (i, field) in distinct.iter().take(SEEN_CAP).enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "{field:?}");
    }
    if distinct.len() > SEEN_CAP {
        let _ = write!(out, ", +{} more", distinct.len() - SEEN_CAP);
    }
    out
}

/// Brackets included; a bracket inside a quoted key is not a selector.
fn selector_spans(raw: &str) -> Vec<(usize, usize)> {
    let bytes = raw.as_bytes();
    let mut spans = Vec::new();
    let mut quote = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(open) = quote {
            if b == open {
                quote = None;
            }
        } else if matches!(b, b'"' | b'\'') && (i == 0 || matches!(bytes[i - 1], b'.' | b'[')) {
            quote = Some(b);
        } else if b == b'[' {
            let inner = &raw[i + 1..];
            if let Some(close) = find_top_level(inner, b']') {
                if find_top_level(&inner[..close], b'=').is_some() {
                    spans.push((i, i + close + 2));
                }
                i += close + 2;
                continue;
            }
        }
        i += 1;
    }
    spans
}

fn malformed(raw: &str, segment: &str) -> crate::Error {
    crate::Error::NotFound {
        target: raw.to_owned(),
        what: format!("malformed path segment {segment:?}"),
        nearest: None,
    }
}

/// Outside quotes and `[...]`. A quote opens only at the start or after `.`, `[` or `=`, so an
/// apostrophe in a bare key stays a plain byte; the grammar has no escapes.
fn find_top_level(s: &str, want: u8) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut quote = None;
    for (i, &b) in bytes.iter().enumerate() {
        if let Some(open) = quote {
            if b == open {
                quote = None;
            }
            continue;
        }
        if depth == 0 && b == want {
            return Some(i);
        }
        match b {
            b'"' | b'\'' if i == 0 || matches!(bytes[i - 1], b'.' | b'[' | b'=') => quote = Some(b),
            b'[' => depth += 1,
            b']' => depth = depth.saturating_sub(1),
            _ => {},
        }
    }
    None
}

pub fn split_flag(flag: &str) -> Result<(&str, &str), crate::Error> {
    let eq = find_top_level(flag, b'=').ok_or_else(|| crate::Error::NotFound {
        target: flag.to_owned(),
        what: "path=value".to_owned(),
        nearest: None,
    })?;
    Ok((&flag[..eq], &flag[eq + 1..]))
}

fn unquote(s: &str) -> Option<&str> {
    let open = s.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    let inner = s.strip_prefix(open)?.strip_suffix(open)?;
    (!inner.contains(open)).then_some(inner)
}

fn bracket_segment(content: &str) -> Option<Segment> {
    if let Some(eq) = find_top_level(content, b'=') {
        let key = &content[..eq];
        let value = &content[eq + 1..];
        if key.is_empty() || key.contains(['"', '\'', '[', ']']) {
            return None;
        }
        let value = unquote(value).or_else(|| {
            let bare = !value.is_empty() && !value.contains(['.', ']', '=', ' ', '"', '\'']);
            bare.then_some(value)
        })?;
        return Some(Segment::Attr {
            key: key.to_owned(),
            value: value.to_owned(),
        });
    }
    if content.is_empty() || !content.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    content.parse().ok().map(Segment::Index)
}

fn push_segment(raw: &str, segment: &str, segments: &mut Vec<Segment>) -> Result<(), crate::Error> {
    let (key, mut rest) = if let Some(open @ ('"' | '\'')) = segment.chars().next() {
        let close = segment[1..]
            .find(open)
            .ok_or_else(|| malformed(raw, segment))?
            + 1;
        (&segment[1..close], &segment[close + 1..])
    } else {
        let key_end = segment.find('[').unwrap_or(segment.len());
        let key = &segment[..key_end];
        if key.is_empty() || key.contains(']') {
            return Err(malformed(raw, segment));
        }
        (key, &segment[key_end..])
    };
    segments.push(Segment::Key(key.to_owned()));

    while let Some(inner) = rest.strip_prefix('[') {
        let close = find_top_level(inner, b']').ok_or_else(|| malformed(raw, segment))?;
        segments.push(bracket_segment(&inner[..close]).ok_or_else(|| malformed(raw, segment))?);
        rest = &inner[close + 1..];
    }
    if rest.is_empty() {
        Ok(())
    } else {
        Err(malformed(raw, segment))
    }
}

/// Hand-scanned rather than a `regex`: quotes and brackets nest, which a regex cannot track.
pub fn parse_path(raw: &str) -> Result<Path, crate::Error> {
    let bare = raw.trim_start_matches('.');
    if bare.len() != raw.len() && !bare.is_empty() {
        return Err(crate::Error::NotFound {
            target: raw.to_owned(),
            what: format!("path (keys take no leading dot: --set {bare}=\u{2026})"),
            nearest: None,
        });
    }
    let mut segments = Vec::new();
    let mut rest = raw;
    loop {
        let end = find_top_level(rest, b'.').unwrap_or(rest.len());
        push_segment(raw, &rest[..end], &mut segments)?;
        if end == rest.len() {
            return Ok(Path(segments));
        }
        rest = &rest[end + 1..];
    }
}

pub fn parse_value(raw: &str) -> Value {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Number(_)) => Value::Number(raw.trim().to_owned()),
        Ok(serde_json::Value::Object(_) | serde_json::Value::Array(_)) => {
            Value::Container(raw.trim().to_owned())
        },
        Ok(value) => Value::Json(value),
        Err(_) => Value::Json(serde_json::Value::String(raw.to_owned())),
    }
}

/// Frontmatter only when the file's first bytes are the fence: `frontmatter.rs` offsets every
/// line by a constant one.
pub fn detect(path: &std::path::Path, content: &str) -> Option<Format> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("json") => Some(Format::Json),
        Some("yaml" | "yml") => Some(Format::Yaml),
        Some("toml") => Some(Format::Toml),
        Some("md" | "mdx") => {
            if content.starts_with("---\n") || content.starts_with("---\r\n") {
                Some(Format::Frontmatter)
            } else {
                None
            }
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotted_path_with_index_parses_to_key_key_index_key() {
        assert_eq!(
            parse_path("a.b[0].c").unwrap(),
            Path(vec![
                Segment::Key("a".into()),
                Segment::Key("b".into()),
                Segment::Index(0),
                Segment::Key("c".into()),
            ])
        );
    }

    #[test]
    fn unbalanced_bracket_errors_naming_the_segment() {
        let err = parse_path("a.b[0.c").unwrap_err();
        assert!(err.to_string().contains("b[0.c"));
    }

    #[test]
    fn non_numeric_index_errors_naming_the_segment() {
        let err = parse_path("a[x]").unwrap_err();
        assert!(err.to_string().contains("a[x]"));
    }

    #[test]
    fn empty_segment_errors() {
        let err = parse_path("a..b").unwrap_err();
        assert!(err.to_string().contains("malformed path segment"));
    }

    #[test]
    fn quoted_key_with_dots_is_one_literal_key() {
        assert_eq!(
            parse_path("\"editor.formatOnSave\"").unwrap(),
            Path(vec![Segment::Key("editor.formatOnSave".into())])
        );
        assert_eq!(
            parse_path("a.'b.c'[1].d").unwrap(),
            Path(vec![
                Segment::Key("a".into()),
                Segment::Key("b.c".into()),
                Segment::Index(1),
                Segment::Key("d".into()),
            ])
        );
    }

    #[test]
    fn unquoted_dotted_key_still_splits() {
        assert_eq!(
            parse_path("editor.formatOnSave").unwrap(),
            Path(vec![
                Segment::Key("editor".into()),
                Segment::Key("formatOnSave".into()),
            ])
        );
    }

    #[test]
    fn unterminated_quote_errors_naming_the_segment() {
        let err = parse_path("a.\"b.c").unwrap_err();
        assert!(
            err.to_string()
                .contains("malformed path segment \"\\\"b.c\"")
        );
    }

    #[test]
    fn text_after_a_closing_quote_errors() {
        let err = parse_path("\"a\"b").unwrap_err();
        assert!(err.to_string().contains("malformed path segment"));
    }

    #[test]
    fn bare_and_quoted_attribute_selectors_parse_to_attr() {
        assert_eq!(
            parse_path("a[name=gitty]").unwrap(),
            Path(vec![Segment::Key("a".into()), Segment::Attr {
                key: "name".into(),
                value: "gitty".into()
            },])
        );
        assert_eq!(
            parse_path("steps[name=\"has space\"].with.ref").unwrap(),
            Path(vec![
                Segment::Key("steps".into()),
                Segment::Attr {
                    key: "name".into(),
                    value: "has space".into()
                },
                Segment::Key("with".into()),
                Segment::Key("ref".into()),
            ])
        );
    }

    #[test]
    fn quoted_selector_value_may_hold_a_dot_bracket_and_equals() {
        assert_eq!(
            parse_path("a[name='x.y]=z'].b").unwrap(),
            Path(vec![
                Segment::Key("a".into()),
                Segment::Attr {
                    key: "name".into(),
                    value: "x.y]=z".into()
                },
                Segment::Key("b".into()),
            ])
        );
    }

    #[test]
    fn unquoted_selector_value_with_a_space_or_dot_errors() {
        assert!(parse_path("a[name=has space]").is_err());
        assert!(parse_path("a[name=1.0]").is_err());
        assert!(parse_path("a[=x]").is_err());
        assert!(parse_path("a[name=]").is_err());
    }

    #[test]
    fn numeric_bracket_still_parses_to_index() {
        assert_eq!(
            parse_path("a[0]").unwrap(),
            Path(vec![Segment::Key("a".into()), Segment::Index(0)])
        );
    }

    #[test]
    fn split_flag_skips_an_equals_inside_a_selector() {
        assert_eq!(
            split_flag("plugins[name=gitty].version=1.0").unwrap(),
            ("plugins[name=gitty].version", "1.0")
        );
        assert_eq!(
            split_flag("plugins[name=\"a=b\"].v=1").unwrap(),
            ("plugins[name=\"a=b\"].v", "1")
        );
    }

    #[test]
    fn split_flag_without_brackets_splits_at_the_first_equals() {
        assert_eq!(split_flag("a.b=c=d").unwrap(), ("a.b", "c=d"));
        assert_eq!(
            split_flag("\"editor.formatOnSave\"=true").unwrap(),
            ("\"editor.formatOnSave\"", "true")
        );
    }

    #[test]
    fn split_flag_without_an_equals_errors_naming_the_flag() {
        let err = split_flag("a.b").unwrap_err();
        assert!(err.to_string().contains("a.b"));
        assert!(split_flag("a[k=v]").is_err());
    }

    fn resolve(
        raw: &str,
        fields: impl FnMut(&[Segment], &str) -> Result<Vec<Option<String>>, crate::Error>,
    ) -> Result<(Path, String), crate::Error> {
        let path = parse_path(raw).unwrap();
        resolve_selectors(std::path::Path::new("f"), &path, raw, fields, |_| 1)
    }

    #[test]
    fn every_selector_in_a_path_is_rewritten_to_its_own_index() {
        let (path, key) = resolve("a[k=x].b[k='y.z'].c", |prefix, _| {
            Ok(match prefix.len() {
                1 => vec![Some("w".into()), Some("x".into())],
                _ => vec![Some("y.z".into())],
            })
        })
        .unwrap();
        assert_eq!(key, "a[1].b[0].c");
        assert_eq!(
            path,
            Path(vec![
                Segment::Key("a".into()),
                Segment::Index(1),
                Segment::Key("b".into()),
                Segment::Index(0),
                Segment::Key("c".into()),
            ])
        );
    }

    #[test]
    fn a_quoted_key_holding_bracket_text_is_not_a_selector() {
        let (_, key) = resolve("\"q[k=v]\".a[k=v]", |_, _| Ok(vec![Some("v".into())])).unwrap();
        assert_eq!(key, "\"q[k=v]\".a[0]");
    }

    #[test]
    fn a_path_without_a_selector_never_reads_the_document() {
        let (path, key) = resolve("a.b[0]", |_, _| panic!("no selector to resolve")).unwrap();
        assert_eq!(key, "a.b[0]");
        assert_eq!(path, parse_path("a.b[0]").unwrap());
    }

    #[test]
    fn values_seen_are_listed_once_each_and_capped() {
        let err = resolve("a[k=none]", |_, _| {
            Ok((0..25)
                .flat_map(|n| [Some(n.to_string()), Some(n.to_string()), None])
                .collect())
        })
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("values seen: \"0\", \"1\""), "{text}");
        assert!(text.contains("\"19\", +5 more)"), "{text}");
        assert!(!text.contains("\"20\""), "{text}");
    }

    #[test]
    fn numeric_value_keeps_its_typed_digits() {
        assert_eq!(parse_value("3.10"), Value::Number("3.10".into()));
        assert_eq!(parse_value("1e5"), Value::Number("1e5".into()));
        assert_eq!(parse_value("-7"), Value::Number("-7".into()));
    }

    #[test]
    fn an_object_keeps_its_typed_text_and_key_order() {
        let raw = r#"{"path": "b.rs", "name": "b"}"#;
        assert_eq!(parse_value(raw), Value::Container(raw.to_owned()));
        let jsonc_parser::ast::Value::Object(object) = container(raw) else {
            panic!("an object")
        };
        let keys: Vec<&str> = object.properties.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(keys, ["path", "name"]);
    }

    #[test]
    fn a_bracketed_word_that_is_not_json_is_a_string() {
        assert_eq!(
            parse_value("{path}"),
            Value::Json(serde_json::Value::String("{path}".into()))
        );
    }

    #[test]
    fn version_with_two_dots_is_a_string_not_a_number() {
        assert_eq!(
            parse_value("3.10.1"),
            Value::Json(serde_json::Value::String("3.10.1".into()))
        );
    }

    #[test]
    fn date_like_value_is_stored_as_a_json_string() {
        assert_eq!(
            parse_value("2026-09-16"),
            Value::Json(serde_json::Value::String("2026-09-16".into()))
        );
    }

    #[test]
    fn false_literal_is_stored_as_a_json_boolean() {
        assert_eq!(
            parse_value("false"),
            Value::Json(serde_json::Value::Bool(false))
        );
    }

    #[test]
    fn json_extension_detects_json_format() {
        assert_eq!(
            detect(std::path::Path::new("app.json"), ""),
            Some(Format::Json)
        );
    }

    #[test]
    fn markdown_without_a_leading_fence_detects_none() {
        assert_eq!(detect(std::path::Path::new("notes.md"), "# Notes\n"), None);
    }

    #[test]
    fn markdown_with_a_leading_fence_detects_frontmatter() {
        assert_eq!(
            detect(std::path::Path::new("notes.md"), "---\ntitle: x\n---\n"),
            Some(Format::Frontmatter)
        );
    }

    #[test]
    fn yaml_and_toml_extensions_detect_their_formats() {
        assert_eq!(
            detect(std::path::Path::new("a.yaml"), ""),
            Some(Format::Yaml)
        );
        assert_eq!(
            detect(std::path::Path::new("a.yml"), ""),
            Some(Format::Yaml)
        );
        assert_eq!(
            detect(std::path::Path::new("a.toml"), ""),
            Some(Format::Toml)
        );
    }

    #[test]
    fn unrecognised_extension_detects_none() {
        assert_eq!(detect(std::path::Path::new("a.vue"), ""), None);
    }

    #[test]
    fn a_leading_dot_is_not_found_and_names_the_path_without_it() {
        let error = parse_path(".version").unwrap_err();
        assert_eq!(error.slug(), "not_found");
        let message = error.to_string();
        assert!(message.contains("keys take no leading dot"), "{message}");
        assert!(message.contains("--set version=\u{2026}"), "{message}");

        let nested = parse_path(".a.b").unwrap_err().to_string();
        assert!(nested.contains("--set a.b=\u{2026}"), "{nested}");
    }

    #[test]
    fn the_key_without_its_dot_parses_and_a_lone_dot_is_malformed() {
        assert_eq!(
            parse_path("version").unwrap(),
            Path(vec![Segment::Key("version".into())])
        );
        let lone = parse_path(".").unwrap_err().to_string();
        assert!(lone.contains("malformed path segment"), "{lone}");
        assert!(!lone.contains("leading dot"), "{lone}");
    }
}
