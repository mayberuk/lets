use yamlpatch::{Op as PatchOp, Style};
use yamlpath::{FeatureKind, QueryError};

use super::{Applied, Op, Path, Segment, TransformOp, Value};
use crate::error::{CheckLayer, UnsupportedReason};

pub fn apply(file: &std::path::Path, content: &str, ops: &[Op]) -> Result<Applied, crate::Error> {
    let mut document =
        yamlpath::Document::new(content.to_owned()).map_err(|err| crate::Error::CheckFailed {
            path: file.to_path_buf(),
            layer: CheckLayer::Structured,
            detail: err.to_string(),
        })?;
    let crlf = uniformly_crlf(content);
    let mut touched = Vec::with_capacity(ops.len());

    for op in ops {
        let (entry, line) = match op {
            Op::Set {
                path,
                raw_key,
                value,
            } => {
                let (path, key) = resolved(&document, file, path, raw_key)?;
                // The last step is exempt: `Replace`/`Remove` rewrite `y: *x` without following it.
                refuse_alias_steps(
                    &document,
                    file,
                    &path,
                    raw_key,
                    path.0.len().saturating_sub(1),
                )?;
                let value = typed(&document, file, &path, raw_key, value);
                // Spliced: `yamlpatch` picks its own quotes, or a block scalar for a line break.
                if let (Value::Json(serde_json::Value::String(text)), Some(quote)) =
                    (value.as_ref(), quote_of(&document, &path))
                {
                    let text = quoted(quote, text);
                    if let Some(exact) = exact_text(&document, file, &path, raw_key, &text)? {
                        document = exact;
                    }
                } else {
                    let change = set_patch(&document, file, &path, raw_key, &value)?;
                    document = patched(&document, file, change, raw_key)?;
                    if let Value::Number(raw) = value.as_ref()
                        && let Some(exact) = exact_text(&document, file, &path, raw_key, raw)?
                    {
                        document = exact;
                    }
                }
                (
                    TransformOp::Set { key },
                    line_at(&document, &path, raw_key)?,
                )
            },
            Op::Delete { path, raw_key } => {
                let (path, key) = resolved(&document, file, path, raw_key)?;
                refuse_alias_steps(
                    &document,
                    file,
                    &path,
                    raw_key,
                    path.0.len().saturating_sub(1),
                )?;
                // Read before `Remove` erases the entry the caller wants reported.
                let line = line_at(&document, &path, raw_key)?;
                document = removed(&document, file, &path, raw_key)?;
                (TransformOp::Delete { key }, line)
            },
            Op::Append {
                path,
                raw_key,
                value,
            } => {
                let (path, key) = resolved(&document, file, path, raw_key)?;
                let (after, line) = appended(&document, file, &path, raw_key, value)?;
                document = after;
                (TransformOp::Append { key }, line)
            },
        };
        touched.push((entry, line));
    }

    let text = if crlf {
        to_crlf(document.source())
    } else {
        document.source().to_owned()
    };
    Ok(Applied { text, touched })
}

/// `yamlpatch` writes new lines with `\n` whatever the document used; rewriting them is byte-safe
/// only when every ending was already CRLF.
fn uniformly_crlf(content: &str) -> bool {
    let bytes = content.as_bytes();
    let mut any = false;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            any = true;
            if index == 0 || bytes[index - 1] != b'\r' {
                return false;
            }
        }
    }
    any
}

fn to_crlf(source: &str) -> String {
    let mut out = String::with_capacity(source.len() + source.len() / 32);
    for line in source.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        out.push_str(body.strip_suffix('\r').unwrap_or(body));
        if line.ends_with('\n') {
            out.push_str("\r\n");
        }
    }
    out
}

/// `--set` upserts, which `yamlpatch` splits in two: `Replace` needs the route to resolve,
/// and `Add` rejects a key that exists.
fn set_patch<'a>(
    document: &yamlpath::Document,
    file: &std::path::Path,
    path: &Path,
    raw_key: &str,
    value: &Value,
) -> Result<yamlpatch::Patch<'a>, crate::Error> {
    let route = route(&path.0);
    if document.query_exists(&route) {
        return Ok(yamlpatch::Patch {
            route,
            operation: PatchOp::Replace(yaml_value(value, raw_key)?),
        });
    }

    let Some(Segment::Key(key)) = path.0.last() else {
        return Err(not_found(
            raw_key,
            Some("a sequence index cannot be created by --set".to_owned()),
        ));
    };
    // `Add` classifies the parent with `Feature::kind`, so it is checked here first.
    let parent = parent_route(path);
    let container = if parent.is_empty() {
        document.top_feature().ok()
    } else {
        document.query_exact(&parent).ok().flatten()
    };
    let container = container.ok_or_else(|| not_found(raw_key, None))?;
    if !matches!(
        kind_of(document, &container, file, raw_key)?,
        FeatureKind::BlockMapping | FeatureKind::FlowMapping
    ) {
        return Err(not_found(
            raw_key,
            Some("its parent is not a mapping".to_owned()),
        ));
    }
    Ok(yamlpatch::Patch {
        route: parent,
        operation: PatchOp::Add {
            key: key.clone(),
            value: yaml_value(value, raw_key)?,
        },
    })
}

fn resolved(
    document: &yamlpath::Document,
    file: &std::path::Path,
    path: &Path,
    raw_key: &str,
) -> Result<(Path, String), crate::Error> {
    super::resolve_selectors(
        file,
        path,
        raw_key,
        |prefix, key| {
            let sequence_route = route(prefix);
            let sequence = document
                .query_exact(&sequence_route)
                .ok()
                .flatten()
                .ok_or_else(|| not_found(raw_key, None))?;
            if !matches!(
                kind_of(document, &sequence, file, raw_key)?,
                FeatureKind::BlockSequence | FeatureKind::FlowSequence
            ) {
                return Err(not_found(raw_key, Some("not a sequence".to_owned())));
            }
            let mut fields = Vec::new();
            while document
                .query_exact(&sequence_route.with_key(fields.len()))
                .is_ok()
            {
                let field = sequence_route.with_keys([
                    yamlpath::Component::from(fields.len()),
                    yamlpath::Component::from(key.to_owned()),
                ]);
                fields.push(
                    document
                        .query_exact(&field)
                        .ok()
                        .flatten()
                        .and_then(|feature| scalar_text(document, &feature, file, raw_key)),
                );
            }
            Ok(fields)
        },
        |element| {
            document
                .query_exact(&route(element))
                .ok()
                .flatten()
                .map_or(1, |feature| {
                    line_of(document.source(), feature.location.byte_span.0)
                })
        },
    )
}

fn scalar_text(
    document: &yamlpath::Document,
    feature: &yamlpath::Feature,
    file: &std::path::Path,
    raw_key: &str,
) -> Option<String> {
    if kind_of(document, feature, file, raw_key).ok()? != FeatureKind::Scalar {
        return None;
    }
    let text = document.extract(feature).trim();
    match Style::from_feature(feature, document) {
        Style::DoubleQuoted if !text.contains('\n') => double_quoted(text),
        Style::SingleQuoted if !text.contains('\n') => text
            .strip_prefix('\'')?
            .strip_suffix('\'')
            .map(|inner| inner.replace("''", "'")),
        Style::PlainScalar => Some(text.to_owned()),
        _ => None,
    }
}

/// Decoded here, as `yamlpatch`'s YAML crate is not a dependency: JSON's escapes plus YAML 1.2's
/// (§ 5.7) such as `\x65`, `\e` and `\_`. An escape YAML does not define never matches.
fn double_quoted(text: &str) -> Option<String> {
    let inner = text.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        out.push(match chars.next()? {
            '0' => '\0',
            'a' => '\u{7}',
            'b' => '\u{8}',
            't' | '\t' => '\t',
            'n' => '\n',
            'v' => '\u{b}',
            'f' => '\u{c}',
            'r' => '\r',
            'e' => '\u{1b}',
            'N' => '\u{85}',
            '_' => '\u{a0}',
            'L' => '\u{2028}',
            'P' => '\u{2029}',
            'x' => hex_char(&mut chars, 2)?,
            'u' => hex_char(&mut chars, 4)?,
            'U' => hex_char(&mut chars, 8)?,
            same @ (' ' | '"' | '/' | '\\') => same,
            _ => return None,
        });
    }
    Some(out)
}

fn hex_char(chars: &mut std::str::Chars, digits: usize) -> Option<char> {
    let mut code = 0u32;
    for _ in 0..digits {
        code = code * 16 + chars.next()?.to_digit(16)?;
    }
    char::from_u32(code)
}

/// `None` also for a value reached through an alias: its quotes belong to the anchor.
fn quote_of(document: &yamlpath::Document, path: &Path) -> Option<char> {
    let route = route(&path.0);
    let value = document.query_exact(&route).ok().flatten()?;
    let entry = document.query_pretty(&route).ok()?;
    let (start, end) = value.location.byte_span;
    if start < entry.location.byte_span.0 || end > entry.location.byte_span.1 {
        return None;
    }
    document
        .extract(&value)
        .chars()
        .next()
        .filter(|quote| matches!(quote, '"' | '\''))
}

/// A single-quoted scalar escapes only `'` and folds a line break into a space, so text holding a
/// control character is written double-quoted, as a JSON string is a valid double-quoted scalar.
fn quoted(quote: char, text: &str) -> String {
    if quote == '\'' && !text.chars().any(char::is_control) {
        return format!("'{}'", text.replace('\'', "''"));
    }
    serde_json::to_string(text).expect("a string serialises to JSON")
}

/// An absent key, a non-string, or one `kind_of` would refuse keeps the value as typed.
fn typed<'v>(
    document: &yamlpath::Document,
    file: &std::path::Path,
    path: &Path,
    raw_key: &str,
    value: &'v Value,
) -> std::borrow::Cow<'v, Value> {
    let text = value
        .string_form()
        .filter(|_| !matches!(value, Value::Json(serde_json::Value::String(_))));
    let holds_string = document
        .query_exact(&route(&path.0))
        .ok()
        .flatten()
        .is_some_and(|feature| {
            kind_of(document, &feature, file, raw_key).ok() == Some(FeatureKind::Scalar)
                && holds_string(document, &feature)
        });
    match text.filter(|_| holds_string) {
        Some(text) => {
            std::borrow::Cow::Owned(Value::Json(serde_json::Value::String(text.into_owned())))
        },
        None => std::borrow::Cow::Borrowed(value),
    }
}

fn holds_string(document: &yamlpath::Document, scalar: &yamlpath::Feature) -> bool {
    match Style::from_feature(scalar, document) {
        Style::PlainScalar => plain_is_string(document.extract(scalar)),
        Style::DoubleQuoted
        | Style::SingleQuoted
        | Style::MultilineLiteralScalar
        | Style::MultilineFoldedScalar => true,
        _ => false,
    }
}

/// The YAML 1.2 core schema (`3.9` float, `~` null, `yes` string) is the grammar's node kinds.
fn plain_is_string(text: &str) -> bool {
    let mut parser = tree_sitter::Parser::new();
    let language = crate::grammars::language(crate::grammars::Language::Yaml);
    let Some(tree) = parser
        .set_language(language)
        .ok()
        .and_then(|()| parser.parse(text, None))
    else {
        return false;
    };
    let mut node = tree.root_node();
    while let Some(child) = node.named_child(0) {
        node = child;
    }
    !matches!(
        node.kind(),
        "boolean_scalar" | "float_scalar" | "integer_scalar" | "null_scalar"
    )
}

/// Spliced: `yamlpatch` writes `3.10` as `3.1` and picks its own quotes, and `RewriteFragment`
/// needs a crate this one lacks. The re-parse must hold exactly `text`; `None` if it already does.
fn exact_text(
    document: &yamlpath::Document,
    file: &std::path::Path,
    path: &Path,
    raw_key: &str,
    text: &str,
) -> Result<Option<yamlpath::Document>, crate::Error> {
    let route = route(&path.0);
    let Some(written) = document.query_exact(&route).ok().flatten() else {
        return Ok(None);
    };
    if document.extract(&written) == text {
        return Ok(None);
    }
    let (start, end) = written.location.byte_span;
    let source = document.source();
    let mut spliced = String::with_capacity(source.len() + text.len());
    spliced.push_str(&source[..start]);
    spliced.push_str(text);
    spliced.push_str(&source[end..]);
    let after = yamlpath::Document::new(spliced)
        .map_err(|err| not_in_place(file, raw_key, &format!("the value does not parse: {err}")))?;
    let landed = after
        .query_exact(&route)
        .ok()
        .flatten()
        .is_some_and(|feature| after.extract(&feature) == text);
    if !landed {
        return Err(not_in_place(
            file,
            raw_key,
            "the value could not be written as typed",
        ));
    }
    Ok(Some(after))
}

/// `yamlpatch` drops a key with its last child and every ancestor that empties; JSON and TOML keep
/// the emptied parent, so this does too.
fn removed(
    document: &yamlpath::Document,
    file: &std::path::Path,
    path: &Path,
    raw_key: &str,
) -> Result<yamlpath::Document, crate::Error> {
    let removal = yamlpatch::Patch {
        route: route(&path.0),
        operation: PatchOp::Remove,
    };
    let after = patched(document, file, removal, raw_key)?;
    let parent = parent_route(path);
    if path.0.len() < 2 || after.query_exists(&parent) {
        return Ok(after);
    }
    let empty = match path.0.last() {
        Some(Segment::Index(_)) => serde_json::json!([]),
        _ => serde_json::json!({}),
    };
    let emptying = yamlpatch::Patch {
        route: parent,
        operation: PatchOp::Replace(yaml_value(&Value::Json(empty), raw_key)?),
    };
    patched(document, file, emptying, raw_key)
}

fn patched(
    document: &yamlpath::Document,
    file: &std::path::Path,
    patch: yamlpatch::Patch,
    raw_key: &str,
) -> Result<yamlpath::Document, crate::Error> {
    yamlpatch::apply_yaml_patches(document, &[patch]).map_err(|err| match err {
        yamlpatch::Error::Query(
            QueryError::ExpectedMapping(_)
            | QueryError::ExpectedList(_)
            | QueryError::ExhaustedMapping(_)
            | QueryError::ExhaustedList(..),
        ) => not_found(raw_key, None),
        // Its message ends in a `Route { .. }` debug dump the caller never typed.
        yamlpatch::Error::InvalidOperation(_) => {
            not_in_place(file, raw_key, "yamlpatch cannot change the value there")
        },
        other => not_in_place(file, raw_key, &other.to_string()),
    })
}

/// `yamlpatch` appends to a block sequence only, so a flow sequence goes through `flow_appended`.
fn appended(
    document: &yamlpath::Document,
    file: &std::path::Path,
    path: &Path,
    raw_key: &str,
    value: &Value,
) -> Result<(yamlpath::Document, usize), crate::Error> {
    // The whole path counts: an append into `tags: *s` grows `s`.
    refuse_alias_steps(document, file, path, raw_key, path.0.len())?;
    let sequence = document
        .query_exact(&route(&path.0))
        .ok()
        .flatten()
        .ok_or_else(|| not_found(raw_key, None))?;
    match kind_of(document, &sequence, file, raw_key)? {
        FeatureKind::BlockSequence => {
            let append = yamlpatch::Patch {
                route: route(&path.0),
                operation: PatchOp::Append {
                    value: yaml_value(value, raw_key)?,
                },
            };
            let after = patched(document, file, append, raw_key)?;
            let grown = after
                .query_exact(&route(&path.0))
                .ok()
                .flatten()
                .ok_or_else(|| not_found(raw_key, None))?;
            let source = after.source();
            let line = line_of(
                source,
                source[..grown.location.byte_span.1].trim_end().len(),
            );
            Ok((after, line))
        },
        FeatureKind::FlowSequence => flow_appended(document, &sequence, file, path, raw_key, value),
        _ => Err(not_found(raw_key, Some("not a sequence".to_owned()))),
    }
}

/// An item's span cannot place the splice: an alias item's (`[*x]`) is the anchor's, elsewhere. The
/// re-parse must show the same items plus this one, grown by exactly the spliced bytes.
fn flow_appended(
    document: &yamlpath::Document,
    sequence: &yamlpath::Feature,
    file: &std::path::Path,
    path: &Path,
    raw_key: &str,
    value: &Value,
) -> Result<(yamlpath::Document, usize), crate::Error> {
    let item = yamlpatch::serialize_flow(&yaml_value(value, raw_key)?)
        .map_err(|err| not_in_place(file, raw_key, &err.to_string()))?;
    let source = document.source();
    let bytes = source.as_bytes();
    let (open, end) = sequence.location.byte_span;
    let close = end - 1;
    if bytes[open] != b'[' || bytes[close] != b']' {
        return Err(not_in_place(
            file,
            raw_key,
            "the flow sequence does not open and close with its own brackets",
        ));
    }

    let mut at = close;
    while at > open + 1
        && (bytes[at - 1].is_ascii_whitespace() || document.offset_inside_comment(at - 1))
    {
        at -= 1;
    }
    let text = if at == open + 1 {
        item.clone()
    } else if bytes[at - 1] == b',' {
        format!(" {item}")
    } else {
        format!(", {item}")
    };
    let mut spliced = String::with_capacity(source.len() + text.len());
    spliced.push_str(&source[..at]);
    spliced.push_str(&text);
    spliced.push_str(&source[at..]);
    let after = yamlpath::Document::new(spliced).map_err(|err| {
        not_in_place(
            file,
            raw_key,
            &format!("the appended item does not parse: {err}"),
        )
    })?;

    let route = route(&path.0);
    let before = item_texts(document, &route);
    let mut grown = item_texts(&after, &route);
    let last = grown.pop().flatten();
    let same_brackets = after
        .query_exact(&route)
        .ok()
        .flatten()
        .is_some_and(|feature| feature.location.byte_span == (open, end + text.len()));
    if last != Some(item.as_str()) || grown != before || !same_brackets {
        return Err(not_in_place(
            file,
            raw_key,
            "the item could not be added to that flow sequence alone",
        ));
    }
    let line = line_of(after.source(), at + text.len() - item.len());
    Ok((after, line))
}

/// `query_exact` follows an alias to the anchor while `query_pretty` stays on the entry, so a step
/// whose value falls outside its own entry was reached through an alias.
fn refuse_alias_steps(
    document: &yamlpath::Document,
    file: &std::path::Path,
    path: &Path,
    raw_key: &str,
    steps: usize,
) -> Result<(), crate::Error> {
    for depth in 1..=steps.min(path.0.len()) {
        let step = route(&path.0[..depth]);
        let (Some(value), Ok(entry)) = (
            document.query_exact(&step).ok().flatten(),
            document.query_pretty(&step),
        ) else {
            break;
        };
        let (start, end) = value.location.byte_span;
        if start < entry.location.byte_span.0 || end > entry.location.byte_span.1 {
            return Err(not_in_place(
                file,
                raw_key,
                "it is reached through an alias; change the anchored value instead",
            ));
        }
    }
    Ok(())
}

/// `Feature::kind` hits `unreachable!` on a tag, an unskipped anchor, an alias or a comment, and a
/// release-profile panic aborts with no `ERROR_CODE`.
fn kind_of(
    document: &yamlpath::Document,
    feature: &yamlpath::Feature,
    file: &std::path::Path,
    raw_key: &str,
) -> Result<FeatureKind, crate::Error> {
    match document.extract(feature).as_bytes().first() {
        Some(b'!') => Err(not_in_place(file, raw_key, "a tagged YAML node")),
        Some(b'&' | b'*' | b'#') => Err(not_in_place(
            file,
            raw_key,
            "a YAML node that opens with an anchor, alias or comment",
        )),
        _ => Ok(feature.kind()),
    }
}

fn route<'a>(segments: &[Segment]) -> yamlpath::Route<'a> {
    yamlpath::Route::from(segments.iter().map(component).collect::<Vec<_>>())
}

fn parent_route<'a>(path: &Path) -> yamlpath::Route<'a> {
    route(path.0.split_last().map_or(&[][..], |(_, rest)| rest))
}

fn component<'a>(segment: &Segment) -> yamlpath::Component<'a> {
    match segment {
        Segment::Key(key) => yamlpath::Component::from(key.clone()),
        Segment::Index(index) => yamlpath::Component::from(*index),
        // `resolved` turns every selector into an `Index` first; a stray one fails to resolve.
        Segment::Attr { .. } => yamlpath::Component::from(usize::MAX),
    }
}

/// The key's line, not the value's: for `key:` over a nested block the two differ.
fn line_at(
    document: &yamlpath::Document,
    path: &Path,
    raw_key: &str,
) -> Result<usize, crate::Error> {
    let route = route(&path.0);
    let byte = match path.0.last() {
        Some(Segment::Key(_)) => document
            .query_key_only(&route)
            .ok()
            .map(|feature| feature.location.byte_span.0),
        _ => document
            .query_exact(&route)
            .ok()
            .flatten()
            .map(|feature| feature.location.byte_span.0),
    };
    Ok(line_of(
        document.source(),
        byte.ok_or_else(|| not_found(raw_key, None))?,
    ))
}

fn item_texts<'d>(
    document: &'d yamlpath::Document,
    sequence: &yamlpath::Route,
) -> Vec<Option<&'d str>> {
    let mut items = Vec::new();
    while let Ok(item) = document.query_exact(&sequence.with_key(items.len())) {
        items.push(item.map(|feature| document.extract(&feature)));
    }
    items
}

fn line_of(source: &str, byte: usize) -> usize {
    source[..byte].bytes().filter(|byte| *byte == b'\n').count() + 1
}

/// Generic: `yamlpatch::Op`'s value type is not re-exported, so each call site's `Op` fixes `T`.
fn yaml_value<T: serde::de::DeserializeOwned>(
    value: &Value,
    raw_key: &str,
) -> Result<T, crate::Error> {
    let parsed = match value {
        Value::Json(json) => serde_json::from_value(json.clone()),
        Value::Number(raw) | Value::Container(raw) => serde_json::from_str(raw),
    };
    parsed.map_err(|err| not_found(raw_key, Some(err.to_string())))
}

fn not_found(what: &str, reason: Option<String>) -> crate::Error {
    crate::Error::NotFound {
        target: match reason {
            Some(reason) => format!("the YAML document ({reason})"),
            None => "the YAML document".to_owned(),
        },
        what: what.to_owned(),
        nearest: None,
    }
}

fn not_in_place(file: &std::path::Path, raw_key: &str, why: &str) -> crate::Error {
    crate::Error::Unsupported {
        path: file.to_path_buf(),
        reason: UnsupportedReason::NotInPlace {
            key: raw_key.to_owned(),
            why: why.to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transform::{parse_path, parse_value};

    const SETTINGS: &str = "allow:\n  - ls\nlegacy:\n  token: abc123 # remove me\n";

    fn at(name: &str) -> std::path::PathBuf {
        std::path::Path::new("tests/fixtures/transform").join(name)
    }

    fn settings(ops: &[Op]) -> Result<Applied, crate::Error> {
        apply(&at("config/settings.yaml"), SETTINGS, ops)
    }

    fn set(key: &str, raw_value: &str) -> Op {
        Op::Set {
            path: parse_path(key).unwrap(),
            raw_key: key.to_owned(),
            value: parse_value(raw_value),
        }
    }

    fn delete(key: &str) -> Op {
        Op::Delete {
            path: parse_path(key).unwrap(),
            raw_key: key.to_owned(),
        }
    }

    /// The verb strips `allow[]`'s trailing `[]`; `parse_path` rejects an empty index.
    fn append(key: &str, raw_value: &str) -> Op {
        Op::Append {
            path: parse_path(key).unwrap(),
            raw_key: format!("{key}[]"),
            value: parse_value(raw_value),
        }
    }

    #[test]
    fn append_adds_a_sequence_item_and_leaves_an_unrelated_comment_alone() {
        let applied = settings(&[append("allow", "gh")]).unwrap();

        assert_eq!(
            applied.text,
            "allow:\n  - ls\n  - gh\nlegacy:\n  token: abc123 # remove me\n"
        );
        assert_eq!(applied.touched.len(), 1);
        assert_eq!(applied.touched[0].1, 3);
    }

    fn appended_to(content: &str, value: &str) -> Applied {
        apply(&at("a.yaml"), content, &[append("tags", value)]).unwrap()
    }

    #[test]
    fn a_flow_sequence_append_inserts_after_the_last_item() {
        let applied = appended_to("tags: [moc]\nname: a\n", "wiki");
        assert_eq!(applied.text, "tags: [moc, wiki]\nname: a\n");
        assert_eq!(applied.touched[0].1, 1);
    }

    #[test]
    fn an_empty_flow_sequence_takes_the_item_inside_its_brackets() {
        assert_eq!(appended_to("tags: []\n", "wiki").text, "tags: [wiki]\n");
    }

    #[test]
    fn a_flow_sequence_append_keeps_quoting_spacing_and_a_trailing_comment() {
        let applied = appended_to("tags: [ 'a b' , \"c\" ]  # keep\n", "wiki");
        assert_eq!(applied.text, "tags: [ 'a b' , \"c\", wiki ]  # keep\n");
    }

    #[test]
    fn a_multi_line_flow_sequence_append_lands_after_the_last_item_not_in_a_comment() {
        let applied = appended_to("tags: [\n  a, # one\n  b\n]\n", "wiki");
        assert_eq!(applied.text, "tags: [\n  a, # one\n  b, wiki\n]\n");
        assert_eq!(applied.touched[0].1, 3, "the line the item landed on");
    }

    #[test]
    fn a_flow_sequence_append_quotes_a_value_that_needs_it() {
        let applied = appended_to("tags: [moc]\n", "a b");
        assert_eq!(applied.text, "tags: [moc, \"a b\"]\n");
    }

    fn applied_to(content: &str, op: Op) -> Result<Applied, crate::Error> {
        apply(&at("a.yaml"), content, &[op])
    }

    fn assert_not_in_place(err: &crate::Error, key: &str) {
        assert_eq!(err.slug(), "unsupported_file", "{err}");
        assert!(
            err.to_string()
                .contains(&format!("{key} cannot be changed in place")),
            "{err}"
        );
    }

    #[test]
    fn an_alias_last_item_takes_the_new_item_after_it_and_the_anchor_is_untouched() {
        let applied = appended_to("x: &x q\ntags: [*x]\n", "wiki");
        assert_eq!(applied.text, "x: &x q\ntags: [*x, wiki]\n");
        assert_eq!(applied.touched[0].1, 2);
    }

    #[test]
    fn an_alias_to_an_anchored_mapping_as_the_last_item_leaves_the_mapping_alone() {
        let applied = appended_to("x: &x\n  k: v\ntags: [*x]\n", "wiki");
        assert_eq!(applied.text, "x: &x\n  k: v\ntags: [*x, wiki]\n");
        assert_eq!(applied.touched[0].1, 3);
    }

    #[test]
    fn an_alias_to_a_quoted_anchor_after_a_plain_item_still_appends() {
        let applied = appended_to("x: &x \"q\"\ntags: [a, *x]\n", "wiki");
        assert_eq!(applied.text, "x: &x \"q\"\ntags: [a, *x, wiki]\n");
    }

    #[test]
    fn an_anchored_flow_sequence_appends_inside_its_own_brackets() {
        let applied = appended_to("tags: &t [a]\nother: *t\n", "wiki");
        assert_eq!(applied.text, "tags: &t [a, wiki]\nother: *t\n");
    }

    #[test]
    fn a_sequence_that_is_an_alias_is_refused_and_names_the_key() {
        let err = applied_to("tags: &t [a]\nother: *t\n", append("other", "wiki")).unwrap_err();
        assert_not_in_place(&err, "other[]");
    }

    #[test]
    fn a_block_sequence_that_is_an_alias_is_refused_too() {
        let err = appended_err("s: &s\n  - a\ntags: *s\n");
        assert_not_in_place(&err, "tags[]");
    }

    #[test]
    fn a_sequence_reached_through_an_aliased_parent_is_refused() {
        let content = "base: &b\n  tags: [a]\nderived: *b\n";
        let err = applied_to(content, append("derived.tags", "wiki")).unwrap_err();
        assert_not_in_place(&err, "derived.tags[]");
    }

    const ALIASED: &str = "x: &x\n  k: v\ny: *x\n";

    #[test]
    fn a_set_through_an_aliased_parent_is_refused_and_the_anchor_is_untouched() {
        let err = applied_to(ALIASED, set("y.k2", "1")).unwrap_err();
        assert_not_in_place(&err, "y.k2");
    }

    #[test]
    fn a_delete_through_an_aliased_parent_is_refused() {
        let err = applied_to(ALIASED, delete("y.k")).unwrap_err();
        assert_not_in_place(&err, "y.k");
    }

    #[test]
    fn an_append_through_an_aliased_parent_is_refused() {
        let content = "x: &x\n  l: [a]\ny: *x\n";
        let err = applied_to(content, append("y.l", "z")).unwrap_err();
        assert_not_in_place(&err, "y.l[]");
    }

    #[test]
    fn a_set_on_the_anchor_s_own_path_adds_the_key_under_the_anchor() {
        let applied = applied_to(ALIASED, set("x.k2", "1")).unwrap();
        assert_eq!(applied.text, "x: &x\n  k: v\n  k2: 1\ny: *x\n");
    }

    #[test]
    fn a_delete_on_the_anchor_s_own_path_removes_only_that_key() {
        let content = "x: &x\n  k: v\n  l: w\ny: *x\n";
        let applied = applied_to(content, delete("x.k")).unwrap();
        assert_eq!(applied.text, "x: &x\n  l: w\ny: *x\n");
    }

    #[test]
    fn a_set_or_delete_of_the_alias_entry_itself_rewrites_only_that_entry() {
        assert_eq!(
            applied_to(ALIASED, set("y", "1")).unwrap().text,
            "x: &x\n  k: v\ny: 1\n"
        );
        assert_eq!(
            applied_to(ALIASED, delete("y")).unwrap().text,
            "x: &x\n  k: v\n"
        );
    }

    fn appended_err(content: &str) -> crate::Error {
        applied_to(content, append("tags", "wiki")).unwrap_err()
    }

    #[test]
    fn a_nested_flow_sequence_takes_the_item_as_the_outer_sequence_s_last() {
        let applied = appended_to("tags: [[a, b], [c]]\n", "wiki");
        assert_eq!(applied.text, "tags: [[a, b], [c], wiki]\n");
    }

    #[test]
    fn an_index_path_appends_inside_the_inner_flow_sequence() {
        let applied = applied_to("tags: [[a, b], [c]]\n", append("tags[0]", "wiki")).unwrap();
        assert_eq!(applied.text, "tags: [[a, b, wiki], [c]]\n");
    }

    #[test]
    fn quoted_items_holding_a_comma_or_bracket_and_a_comment_after_the_bracket_survive() {
        let applied = appended_to("tags: ['a,]', \"b]\"]  # c ]\n", "wiki");
        assert_eq!(applied.text, "tags: ['a,]', \"b]\", wiki]  # c ]\n");
    }

    #[test]
    fn an_empty_flow_sequence_with_inner_space_takes_the_item_just_inside_the_bracket() {
        assert_eq!(
            appended_to("tags: [ ]  # x\n", "wiki").text,
            "tags: [wiki ]  # x\n"
        );
    }

    #[test]
    fn a_multi_line_flow_sequence_appends_before_the_last_item_s_comment() {
        let applied = appended_to("tags: [\n  a,\n  b # last\n]\n", "wiki");
        assert_eq!(applied.text, "tags: [\n  a,\n  b, wiki # last\n]\n");
        assert_eq!(applied.touched[0].1, 3);
    }

    #[test]
    fn a_trailing_comma_is_reused_rather_than_doubled() {
        let applied = appended_to("tags: [\n  a,\n  b,\n]\n", "wiki");
        assert_eq!(applied.text, "tags: [\n  a,\n  b, wiki\n]\n");
    }

    #[test]
    fn a_tagged_flow_sequence_is_refused_rather_than_crashing() {
        assert_not_in_place(&appended_err("tags: !!seq [a]\n"), "tags[]");
        assert_not_in_place(&appended_err("tags: !!seq []\n"), "tags[]");
    }

    #[test]
    fn a_tagged_block_sequence_is_refused_rather_than_crashing() {
        assert_not_in_place(&appended_err("tags: !!seq\n  - a\n"), "tags[]");
    }

    #[test]
    fn a_key_added_under_a_tagged_mapping_is_refused_rather_than_crashing() {
        let err = applied_to("legacy: !!map\n  token: x\n", set("legacy.added", "1")).unwrap_err();
        assert_not_in_place(&err, "legacy.added");
    }

    #[test]
    fn a_key_added_to_an_anchored_root_is_refused_rather_than_crashing() {
        let err = applied_to("&r\na: 1\n", set("b", "1")).unwrap_err();
        assert_not_in_place(&err, "b");
    }

    #[test]
    fn a_key_yamlpatch_will_not_add_in_place_is_unsupported_not_missing() {
        let err = applied_to("m: {\n  a: 1\n}\n", set("m.b", "2")).unwrap_err();
        assert_not_in_place(&err, "m.b");
        assert!(!err.to_string().contains("Route"), "{err}");
    }

    #[test]
    fn a_key_under_a_scalar_parent_is_still_not_found() {
        let err = applied_to("x: [a]\n", set("x[0].k", "1")).unwrap_err();
        assert_eq!(err.slug(), "not_found", "{err}");
    }

    #[test]
    fn appending_to_a_key_that_is_absent_is_not_found_with_no_debug_text() {
        let err = apply(&at("a.yaml"), "name: a\n", &[append("tags", "wiki")]).unwrap_err();
        assert_eq!(err.slug(), "not_found");
        assert!(!err.to_string().contains("Route"), "{err}");
    }

    #[test]
    fn appending_to_a_mapping_is_rejected() {
        let err = settings(&[append("legacy", "gh")]).unwrap_err();
        assert!(err.to_string().contains("legacy[]"), "{err}");
    }

    #[test]
    fn deleting_the_only_child_leaves_the_parent_as_an_empty_mapping() {
        let applied = settings(&[delete("legacy.token")]).unwrap();

        assert_eq!(applied.text, "allow:\n  - ls\nlegacy: {}\n");
        assert_eq!(applied.touched[0].1, 4);
    }

    #[test]
    fn deleting_the_only_child_two_levels_down_keeps_every_ancestor() {
        let applied = apply(&at("nested.yaml"), "a:\n  b:\n    c: 1\nz: 2\n", &[delete(
            "a.b.c",
        )])
        .unwrap();

        assert_eq!(applied.text, "a:\n  b: {}\nz: 2\n");
    }

    #[test]
    fn deleting_the_only_item_leaves_the_sequence_empty() {
        let applied = settings(&[delete("allow[0]")]).unwrap();

        assert_eq!(
            applied.text,
            "allow: []\nlegacy:\n  token: abc123 # remove me\n"
        );
    }

    #[test]
    fn deleting_a_child_with_a_sibling_removes_only_its_own_line() {
        let content = "legacy:\n  token: abc123 # remove me\n  keep: 1\n";
        let applied = apply(&at("config/settings.yaml"), content, &[delete(
            "legacy.token",
        )])
        .unwrap();

        assert_eq!(applied.text, "legacy:\n  keep: 1\n");
        assert_eq!(applied.touched[0].1, 2);
    }

    #[test]
    fn deleting_an_absent_key_is_rejected() {
        let err = settings(&[delete("legacy.absent")]).unwrap_err();
        assert!(err.to_string().contains("legacy.absent"), "{err}");
    }

    #[test]
    fn set_rewrites_an_existing_value_and_keeps_its_comment() {
        let applied = settings(&[set("legacy.token", "xyz789")]).unwrap();

        assert_eq!(
            applied.text,
            "allow:\n  - ls\nlegacy:\n  token: xyz789 # remove me\n"
        );
        assert_eq!(applied.touched[0].1, 4);
    }

    #[test]
    fn set_on_an_absent_key_adds_it_to_its_parent_mapping() {
        let applied = settings(&[set("legacy.added", "1")]).unwrap();

        assert!(applied.text.contains("added: 1"), "{}", applied.text);
        assert!(applied.text.contains("token: abc123 # remove me"));
    }

    #[test]
    fn set_on_an_absent_top_level_key_adds_it_at_the_root() {
        let applied = settings(&[set("version", "2")]).unwrap();

        assert!(applied.text.contains("version: 2"), "{}", applied.text);
    }

    #[test]
    fn set_under_an_absent_parent_is_rejected() {
        let err = settings(&[set("missing.parent.key", "1")]).unwrap_err();
        assert!(err.to_string().contains("missing.parent.key"), "{err}");
    }

    #[test]
    fn a_json_value_keeps_its_own_type_through_the_yaml_round_trip() {
        let applied = settings(&[set("legacy.added", "false")]).unwrap();
        assert!(applied.text.contains("added: false"), "{}", applied.text);
    }

    #[test]
    fn a_boolean_set_over_an_existing_string_stays_a_string() {
        let applied = settings(&[set("legacy.token", "false")]).unwrap();
        assert!(
            applied.text.contains("token: 'false' # remove me"),
            "{}",
            applied.text
        );
    }

    #[test]
    fn a_sequence_index_resolves_as_a_path_segment() {
        let applied = settings(&[set("allow[0]", "rg")]).unwrap();

        assert_eq!(
            applied.text,
            "allow:\n  - rg\nlegacy:\n  token: abc123 # remove me\n"
        );
        assert_eq!(applied.touched[0].1, 2);
    }

    #[test]
    fn several_ops_apply_in_order_and_each_reports_its_own_line() {
        let applied = settings(&[append("allow", "gh"), delete("legacy.token")]).unwrap();

        assert!(applied.text.contains("  - gh"));
        assert!(!applied.text.contains("remove me"));
        assert_eq!(applied.touched[0].1, 3);
        assert_eq!(applied.touched[1].1, 5);
    }

    #[test]
    fn malformed_yaml_is_rejected_before_any_op_runs() {
        let err = apply(
            &at("config/unparsable.yaml"),
            "allow:\n  - ls\n  bad: [\n",
            &[delete("allow")],
        )
        .unwrap_err();

        assert!(
            matches!(err, crate::Error::CheckFailed {
                layer: CheckLayer::Structured,
                ..
            }),
            "{err:?}"
        );
        assert!(
            err.to_string()
                .contains("tests/fixtures/transform/config/unparsable.yaml"),
            "{err}"
        );
    }

    #[test]
    fn an_absent_key_is_still_reported_as_a_missing_path_not_a_check_failure() {
        let err = settings(&[delete("legacy.absent")]).unwrap_err();
        assert!(matches!(err, crate::Error::NotFound { .. }), "{err:?}");
    }

    fn settings_crlf() -> String {
        SETTINGS.replace('\n', "\r\n")
    }

    #[test]
    fn crlf_lines_outside_the_edit_survive_byte_for_byte() {
        let applied = apply(&at("config/settings.yaml"), &settings_crlf(), &[set(
            "legacy.token",
            "xyz789",
        )])
        .unwrap();

        assert_eq!(
            applied.text,
            "allow:\r\n  - ls\r\nlegacy:\r\n  token: xyz789 # remove me\r\n"
        );
    }

    #[test]
    fn an_appended_item_takes_the_crlf_ending_the_rest_of_the_document_uses() {
        let applied = apply(&at("config/settings.yaml"), &settings_crlf(), &[append(
            "allow", "gh",
        )])
        .unwrap();

        assert_eq!(
            applied.text,
            "allow:\r\n  - ls\r\n  - gh\r\nlegacy:\r\n  token: abc123 # remove me\r\n"
        );
    }

    #[test]
    fn a_key_added_to_a_crlf_document_takes_the_crlf_ending_too() {
        let applied = apply(&at("config/settings.yaml"), &settings_crlf(), &[set(
            "legacy.added",
            "1",
        )])
        .unwrap();

        assert_eq!(
            applied.text,
            "allow:\r\n  - ls\r\nlegacy:\r\n  token: abc123 # remove me\r\n  added: 1\r\n"
        );
    }

    #[test]
    fn a_mixed_ending_document_keeps_every_ending_it_had() {
        let mixed = "allow:\r\n  - ls\nlegacy:\r\n  token: abc123 # remove me\r\n";
        let applied = apply(&at("config/settings.yaml"), mixed, &[set(
            "legacy.token",
            "xyz789",
        )])
        .unwrap();

        assert_eq!(
            applied.text,
            "allow:\r\n  - ls\nlegacy:\r\n  token: xyz789 # remove me\r\n"
        );
    }

    const SERVICES: &str = "version: \"3.9\"\nbuild: 7\nlabel: abc\nservices:\n  - name: \
                            api\n  - name: web\n    image: web:1\n  - name: 'db'\n    image: \
                            db:1\ntwins:\n  - name: dup\n  - name: dup\n";

    fn services(ops: &[Op]) -> Result<Applied, crate::Error> {
        apply(&at("services.yaml"), SERVICES, ops)
    }

    #[test]
    fn a_number_set_over_an_existing_quoted_string_stays_a_string_in_its_quotes() {
        let applied = services(&[set("version", "3.10")]).unwrap();
        assert_eq!(
            applied.text,
            SERVICES.replace("version: \"3.9\"", "version: \"3.10\"")
        );
        assert_eq!(applied.touched[0].1, 1);
    }

    #[test]
    fn a_string_set_over_a_double_quoted_scalar_changes_only_the_bytes_inside_the_quotes() {
        for (value, written) in [
            ("3.10", "\"3.10\""),
            ("abc", "\"abc\""),
            ("it's \"hi\"", "\"it's \\\"hi\\\"\""),
            ("a\nb", "\"a\\nb\""),
        ] {
            let applied = apply(&at("a.yaml"), "a: \"3.9\" # pin\nb: 1\n", &[set(
                "a", value,
            )])
            .unwrap();
            assert_eq!(
                applied.text,
                format!("a: {written} # pin\nb: 1\n"),
                "{value}"
            );
        }
    }

    #[test]
    fn a_string_set_over_a_single_quoted_scalar_keeps_its_single_quotes() {
        for (value, written) in [("3.10", "'3.10'"), ("abc", "'abc'"), ("it's", "'it''s'")] {
            let applied = apply(&at("a.yaml"), "a: 'x' # pin\nb: 1\n", &[set("a", value)]).unwrap();
            assert_eq!(
                applied.text,
                format!("a: {written} # pin\nb: 1\n"),
                "{value}"
            );
        }
    }

    #[test]
    fn a_line_break_set_over_a_single_quoted_scalar_is_written_double_quoted() {
        let applied = apply(&at("a.yaml"), "a: 'x'\n", &[set("a", "a\nb")]).unwrap();
        assert_eq!(applied.text, "a: \"a\\nb\"\n");
    }

    #[test]
    fn a_string_set_over_a_plain_scalar_stays_plain() {
        let applied = apply(&at("a.yaml"), "a: abc # pin\nb: 1\n", &[set("a", "xyz")]).unwrap();
        assert_eq!(applied.text, "a: xyz # pin\nb: 1\n");
    }

    #[test]
    fn a_string_set_over_an_alias_to_a_quoted_anchor_leaves_the_anchor_alone() {
        let applied = apply(&at("a.yaml"), "x: &x \"a\"\ny: *x\n", &[set("y", "b")]).unwrap();
        assert_eq!(applied.text, "x: &x \"a\"\ny: b\n");
    }

    #[test]
    fn a_number_set_over_an_existing_plain_string_stays_a_string() {
        let applied = services(&[set("label", "3.10")]).unwrap();
        let single = SERVICES.replace("label: abc", "label: '3.10'");
        let double = SERVICES.replace("label: abc", "label: \"3.10\"");
        assert!(
            applied.text == single || applied.text == double,
            "{}",
            applied.text
        );
    }

    #[test]
    fn a_number_set_over_an_existing_number_is_written_as_typed() {
        let applied = services(&[set("build", "3.10")]).unwrap();
        assert_eq!(applied.text, SERVICES.replace("build: 7", "build: 3.10"));
        assert_eq!(applied.touched[0].1, 2);
    }

    #[test]
    fn a_plain_scalar_s_type_follows_the_core_schema() {
        let content = "a: yes\nb: true\nc: ~\n";
        let applied = apply(&at("a.yaml"), content, &[
            set("a", "false"),
            set("b", "false"),
            set("c", "5"),
        ])
        .unwrap();
        assert_eq!(applied.text, "a: 'false'\nb: false\nc: 5\n");
    }

    #[test]
    fn a_new_key_s_number_keeps_its_typed_text() {
        let applied = apply(&at("a.yaml"), "a: 1\n", &[set("b", "3.10")]).unwrap();
        assert_eq!(applied.text, "a: 1\nb: 3.10\n");
        assert_eq!(applied.touched[0].1, 2);
    }

    #[test]
    fn a_new_key_s_boolean_is_a_boolean_not_a_string() {
        let applied = apply(&at("a.yaml"), "a: 1\n", &[set("b", "true")]).unwrap();
        assert_eq!(applied.text, "a: 1\nb: true\n");
    }

    fn b_path() -> Path {
        parse_path("b").unwrap()
    }

    #[test]
    fn a_number_yamlpatch_reformatted_is_spliced_back_to_its_typed_text() {
        let document = yamlpath::Document::new("a: 1\nb: 3.1\n").unwrap();
        let exact = exact_text(&document, &at("a.yaml"), &b_path(), "b", "3.10")
            .unwrap()
            .expect("3.1 differs from the typed 3.10");
        assert_eq!(exact.source(), "a: 1\nb: 3.10\n");
    }

    #[test]
    fn a_number_already_written_as_typed_builds_no_second_document() {
        let document = yamlpath::Document::new("a: 1\nb: 7\n").unwrap();
        let exact = exact_text(&document, &at("a.yaml"), &b_path(), "b", "7").unwrap();
        assert!(exact.is_none());
    }

    #[test]
    fn an_attribute_selector_sets_only_the_matching_item() {
        let applied = services(&[set("services[name=web].image", "web:2")]).unwrap();
        assert_eq!(
            applied.text,
            SERVICES.replace("image: web:1", "image: web:2")
        );
        assert!(matches!(
            &applied.touched[..],
            [(TransformOp::Set { key }, 7)] if key == "services[1].image"
        ));
    }

    #[test]
    fn a_selector_compares_a_quoted_field_by_its_value() {
        let applied = services(&[set("services[name=db].image", "db:2")]).unwrap();
        assert_eq!(applied.text, SERVICES.replace("image: db:1", "image: db:2"));
        assert!(matches!(
            &applied.touched[..],
            [(TransformOp::Set { key }, 9)] if key == "services[2].image"
        ));
    }

    const ESCAPED: &str = "services:\n  - name: api\n  - name: \"w\\x65b\"\n    image: web:1\n";

    #[test]
    fn a_selector_matches_a_double_quoted_field_by_its_yaml_decoded_value() {
        let applied = apply(&at("a.yaml"), ESCAPED, &[set(
            "services[name=web].image",
            "web:2",
        )])
        .unwrap();
        assert_eq!(applied.text, ESCAPED.replace("web:1", "web:2"));
        assert!(matches!(
            &applied.touched[..],
            [(TransformOp::Set { key }, 4)] if key == "services[1].image"
        ));
    }

    #[test]
    fn a_selector_does_not_match_a_double_quoted_field_by_its_escaped_text() {
        let err = apply(&at("a.yaml"), ESCAPED, &[set(
            "services[name=w\\x65b].image",
            "web:2",
        )])
        .unwrap_err();
        assert_eq!(err.slug(), "not_found");
        assert!(
            err.to_string().contains("(values seen: \"api\", \"web\")"),
            "{err}"
        );
    }

    #[test]
    fn yaml_s_own_double_quoted_escapes_decode_to_their_characters() {
        for (text, decoded) in [
            (r#""plain""#, "plain"),
            (r#""\x41\u00e9\U0001F600""#, "A\u{e9}\u{1f600}"),
            (
                r#""\0\a\b\t\n\v\f\r\e""#,
                "\0\u{7}\u{8}\t\n\u{b}\u{c}\r\u{1b}",
            ),
            (
                r#""\ \"\/\\\N\_\L\P""#,
                " \"/\\\u{85}\u{a0}\u{2028}\u{2029}",
            ),
            ("\"a\\\tb\"", "a\tb"),
        ] {
            assert_eq!(double_quoted(text).as_deref(), Some(decoded), "{text}");
        }
    }

    #[test]
    fn an_escape_yaml_does_not_define_or_a_short_hex_decodes_to_nothing() {
        for text in [
            r#""\q""#,
            r#""\x4""#,
            r#""\xZZ""#,
            r#""\uD800""#,
            r#""trailing\""#,
        ] {
            assert_eq!(double_quoted(text), None, "{text}");
        }
    }

    #[test]
    fn a_selector_adds_a_key_to_the_matching_item() {
        let applied = services(&[set("services[name=api].image", "api:1")]).unwrap();
        assert_eq!(
            applied.text,
            SERVICES.replace("name: api\n", "name: api\n    image: api:1\n")
        );
    }

    #[test]
    fn a_selector_with_no_match_is_not_found_and_lists_the_values_seen() {
        let err = services(&[set("services[name=gone].image", "x")]).unwrap_err();
        assert_eq!(err.slug(), "not_found");
        assert_eq!(
            err.to_string(),
            "an element with name=\"gone\" (values seen: \"api\", \"web\", \"db\") not found in \
             services[name=gone].image"
        );
    }

    #[test]
    fn a_selector_on_a_mapping_is_not_found() {
        let err = services(&[set("build[name=x].image", "x")]).unwrap_err();
        assert_eq!(err.slug(), "not_found", "{err}");
    }

    #[test]
    fn a_selector_matching_two_items_is_ambiguous_and_lists_both_indexes() {
        let err = services(&[delete("twins[name=dup]")]).unwrap_err();
        assert_eq!(err.slug(), "ambiguous");
        let crate::Error::Ambiguous { target, candidates } = &err else {
            panic!("{err:?}");
        };
        assert_eq!(target, "twins[name=dup]");
        let found: Vec<(usize, &str)> = candidates
            .iter()
            .map(|candidate| (candidate.line, candidate.text.as_str()))
            .collect();
        assert_eq!(found, [
            (11, "twins[0].name=\"dup\""),
            (12, "twins[1].name=\"dup\"")
        ]);
    }

    /// Only the root `plugins` has `top`, only group one has `a`: a wrong-sequence read finds them.
    const DEEP: &str = "plugins:\n  - name: top\n    v: 5\nouter:\n  plugins:\n    - name: alpha\n      \
                        v: 10\n    - name: gitty\n      v: 11\ngroups:\n  - name: one\n    \
                        plugins:\n      - name: a\n        v: 1\n      - name: b\n        v: 2\n  \
                        - name: two\n    plugins:\n      - name: b\n        v: 3\n      - name: \
                        c\n        v: 4\n";

    fn deep(op: Op) -> Result<Applied, crate::Error> {
        apply(&at("deep.yaml"), DEEP, &[op])
    }

    #[test]
    fn a_selector_under_a_multi_segment_prefix_resolves_in_that_sequence() {
        let applied = deep(set("outer.plugins[name=gitty].v", "99")).unwrap();
        assert_eq!(applied.text, DEEP.replace("v: 11", "v: 99"));
        assert!(matches!(
            &applied.touched[..],
            [(TransformOp::Set { key }, 9)] if key == "outer.plugins[1].v"
        ));
    }

    #[test]
    fn a_selector_under_a_multi_segment_prefix_misses_a_value_only_a_shallower_sequence_holds() {
        let err = deep(set("outer.plugins[name=top].v", "99")).unwrap_err();
        assert_eq!(err.slug(), "not_found");
        assert!(
            err.to_string()
                .contains("(values seen: \"alpha\", \"gitty\")"),
            "{err}"
        );
    }

    #[test]
    fn a_selector_inside_an_item_of_another_sequence_resolves_in_that_item() {
        for (path, key) in [
            (
                "groups[name=two].plugins[name=b].v",
                "groups[1].plugins[0].v",
            ),
            ("groups[1].plugins[name=b].v", "groups[1].plugins[0].v"),
        ] {
            let applied = deep(set(path, "99")).unwrap();
            assert_eq!(applied.text, DEEP.replace("v: 3", "v: 99"), "{path}");
            assert!(
                matches!(
                    &applied.touched[..],
                    [(TransformOp::Set { key: written }, 20)] if written == key
                ),
                "{path}: {:?}",
                applied.touched
            );
        }
    }

    #[test]
    fn a_selector_inside_an_item_of_another_sequence_misses_a_value_only_a_sibling_holds() {
        let err = deep(set("groups[name=two].plugins[name=a].v", "99")).unwrap_err();
        assert_eq!(err.slug(), "not_found");
        assert!(
            err.to_string().contains("(values seen: \"b\", \"c\")"),
            "{err}"
        );
    }
}
