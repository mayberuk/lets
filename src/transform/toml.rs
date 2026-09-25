use std::path::Path;

use toml_edit::{
    Array, ArrayOfTables, Document, DocumentMut, InlineTable, Item, Table, TableLike,
    Value as TomlValue,
};

use super::{Applied, Op, Segment, TransformOp, Value};
use crate::error::CheckLayer;

pub fn apply(file: &Path, content: &str, ops: &[Op]) -> Result<Applied, crate::Error> {
    let mut doc = content
        .parse::<DocumentMut>()
        .map_err(|err| crate::Error::CheckFailed {
            path: file.to_path_buf(),
            layer: CheckLayer::Structured,
            detail: err.to_string(),
        })?;

    let mut touched = Vec::with_capacity(ops.len());
    for op in ops {
        match op {
            Op::Set {
                path,
                raw_key,
                value,
            } => {
                let (path, key) = resolved(file, &doc, path, raw_key, "--set", true)?;
                set(&mut doc, &path.0, raw_key, value)?;
                let line = line_of(&doc.to_string(), &path.0);
                touched.push((TransformOp::Set { key }, line));
            },
            Op::Delete { path, raw_key } => {
                let (path, key) = resolved(file, &doc, path, raw_key, "--delete", false)?;
                let line = line_of(&doc.to_string(), &path.0);
                delete(&mut doc, &path.0, raw_key)?;
                touched.push((TransformOp::Delete { key }, line));
            },
            Op::Append {
                path,
                raw_key,
                value,
            } => {
                let (path, key) = resolved(file, &doc, path, raw_key, "--append", true)?;
                let at = append(&mut doc, &path.0, raw_key, value)?;
                let line = line_of(&doc.to_string(), &at);
                touched.push((TransformOp::Append { key }, line));
            },
        }
    }

    Ok(Applied {
        text: doc.to_string(),
        touched,
    })
}

enum Cursor<'a> {
    Table(&'a mut dyn TableLike),
    Array(&'a mut Array),
    Tables(&'a mut ArrayOfTables),
}

fn not_found(raw_key: &str) -> crate::Error {
    crate::Error::NotFound {
        target: raw_key.to_owned(),
        what: "path".to_owned(),
        nearest: None,
    }
}

/// Never creates or pads an array, matching jq and yq; only a missing table key is created.
fn navigate<'a>(
    root: &'a mut dyn TableLike,
    segments: &[Segment],
    create: bool,
) -> Option<Cursor<'a>> {
    let mut cursor = Cursor::Table(root);
    for i in 0..segments.len().saturating_sub(1) {
        let need_array = matches!(segments[i + 1], Segment::Index(_));
        cursor = step(cursor, &segments[i], need_array, create)?;
    }
    Some(cursor)
}

fn step<'a>(
    cursor: Cursor<'a>,
    segment: &Segment,
    need_array: bool,
    create: bool,
) -> Option<Cursor<'a>> {
    match (cursor, segment) {
        (Cursor::Table(table), Segment::Key(key)) => {
            if need_array {
                // `[[servers]]` is an `Item::ArrayOfTables`, which `as_array_mut` never matches.
                let item = table.get_mut(key)?;
                if item.is_array_of_tables() {
                    item.as_array_of_tables_mut().map(Cursor::Tables)
                } else {
                    item.as_array_mut().map(Cursor::Array)
                }
            } else if create {
                let item = table.entry(key).or_insert(Item::Table(Table::new()));
                item.as_table_like_mut().map(Cursor::Table)
            } else {
                table.get_mut(key)?.as_table_like_mut().map(Cursor::Table)
            }
        },
        (Cursor::Array(arr), Segment::Index(index)) => {
            let value = arr.get_mut(*index)?;
            if need_array {
                value.as_array_mut().map(Cursor::Array)
            } else {
                value.as_inline_table_mut().map(|t| Cursor::Table(t))
            }
        },
        (Cursor::Tables(tables), Segment::Index(index)) => {
            tables.get_mut(*index).map(|table| Cursor::Table(table))
        },
        // Unreachable: navigate's lookahead pairs each cursor with its segment kind.
        _ => None,
    }
}

fn set(
    doc: &mut DocumentMut,
    segments: &[Segment],
    raw_key: &str,
    value: &Value,
) -> Result<(), crate::Error> {
    let cursor = navigate(doc.as_table_mut(), segments, true).ok_or_else(|| not_found(raw_key))?;
    match (cursor, segments.last()) {
        (Cursor::Table(table), Some(Segment::Key(key))) => {
            match table.get_mut(key) {
                Some(Item::Value(existing)) => {
                    let mut replacement = typed(existing, value, raw_key)?;
                    *replacement.decor_mut() = existing.decor().clone();
                    *existing = replacement;
                },
                _ => {
                    table.insert(key, Item::Value(to_toml_value(value, raw_key)?));
                },
            }
            Ok(())
        },
        (Cursor::Array(arr), Some(Segment::Index(index))) => {
            let slot = arr.get_mut(*index).ok_or_else(|| not_found(raw_key))?;
            let mut replacement = typed(slot, value, raw_key)?;
            *replacement.decor_mut() = slot.decor().clone();
            *slot = replacement;
            Ok(())
        },
        _ => Err(not_found(raw_key)),
    }
}

fn typed(existing: &TomlValue, value: &Value, raw_key: &str) -> Result<TomlValue, crate::Error> {
    match value.string_form().filter(|_| existing.is_str()) {
        Some(text) => Ok(TomlValue::from(text.into_owned())),
        None => to_toml_value(value, raw_key),
    }
}

fn delete(doc: &mut DocumentMut, segments: &[Segment], raw_key: &str) -> Result<(), crate::Error> {
    let cursor = navigate(doc.as_table_mut(), segments, false).ok_or_else(|| not_found(raw_key))?;
    match (cursor, segments.last()) {
        (Cursor::Table(table), Some(Segment::Key(key))) => table
            .remove(key)
            .map(|_| ())
            .ok_or_else(|| not_found(raw_key)),
        (Cursor::Array(arr), Some(Segment::Index(index))) => {
            if *index < arr.len() {
                arr.remove(*index);
                Ok(())
            } else {
                Err(not_found(raw_key))
            }
        },
        _ => Err(not_found(raw_key)),
    }
}

/// Reports an array-of-tables append on the new table, whose `[[header]]` is the line written.
fn append(
    doc: &mut DocumentMut,
    segments: &[Segment],
    raw_key: &str,
    value: &Value,
) -> Result<Vec<Segment>, crate::Error> {
    let cursor = navigate(doc.as_table_mut(), segments, false).ok_or_else(|| not_found(raw_key))?;
    let array = match (cursor, segments.last()) {
        (Cursor::Table(table), Some(Segment::Key(key))) => {
            let item = table.get_mut(key).ok_or_else(|| not_found(raw_key))?;
            if let Some(tables) = item.as_array_of_tables_mut() {
                tables.push(appended_table(value, raw_key)?);
                let mut at = segments.to_vec();
                at.push(Segment::Index(tables.len() - 1));
                return Ok(at);
            }
            item.as_array_mut()
        },
        (Cursor::Array(arr), Some(Segment::Index(index))) => {
            arr.get_mut(*index).and_then(TomlValue::as_array_mut)
        },
        _ => None,
    }
    .ok_or_else(|| not_found(raw_key))?;
    array.push(to_toml_value(value, raw_key)?);
    Ok(segments.to_vec())
}

/// Built bare: `toml_edit` lays out the `[[header]]` and blank line of a table it did not parse.
fn appended_table(value: &Value, raw_key: &str) -> Result<Table, crate::Error> {
    let refused = || crate::Error::NotFound {
        target: raw_key.to_owned(),
        what: "a JSON object to append (an array of tables takes only objects)".to_owned(),
        nearest: None,
    };
    let mut table = Table::new();
    match value {
        Value::Container(raw) => {
            let jsonc_parser::ast::Value::Object(object) = super::container(raw) else {
                return Err(refused());
            };
            for prop in &object.properties {
                table.insert(
                    prop.name.as_str(),
                    Item::Value(typed_to_toml(&prop.value, raw_key)?),
                );
            }
        },
        Value::Json(serde_json::Value::Object(map)) => {
            for (key, field) in map {
                table.insert(key, Item::Value(json_to_toml(field, raw_key)?));
            }
        },
        _ => return Err(refused()),
    }
    Ok(table)
}

/// Keys in typed order and numbers in typed text, which `json_to_toml`'s source has lost.
fn typed_to_toml(
    value: &jsonc_parser::ast::Value,
    raw_key: &str,
) -> Result<TomlValue, crate::Error> {
    use jsonc_parser::ast::Value as Typed;
    match value {
        Typed::NullKeyword(_) => json_to_toml(&serde_json::Value::Null, raw_key),
        Typed::BooleanLit(flag) => Ok(TomlValue::from(flag.value)),
        Typed::NumberLit(number) => number_from_text(number.value, raw_key),
        Typed::StringLit(text) => Ok(TomlValue::from(text.value.to_string())),
        Typed::Array(array) => {
            let mut out = Array::new();
            for element in &array.elements {
                out.push(typed_to_toml(element, raw_key)?);
            }
            Ok(TomlValue::Array(out))
        },
        Typed::Object(object) => {
            let mut out = InlineTable::new();
            for prop in &object.properties {
                out.insert(prop.name.as_str(), typed_to_toml(&prop.value, raw_key)?);
            }
            Ok(TomlValue::InlineTable(out))
        },
    }
}

/// Parsed from the typed text: going through f64 would turn 3.10 into 3.1.
fn to_toml_value(value: &Value, raw_key: &str) -> Result<TomlValue, crate::Error> {
    match value {
        Value::Json(json) => json_to_toml(json, raw_key),
        Value::Number(raw) => number_from_text(raw, raw_key),
        Value::Container(raw) => typed_to_toml(&super::container(raw), raw_key),
    }
}

/// Only an integer past `i64` can fail, as `serde_json` already made an oversized float a string;
/// the typed value is at fault, so it is a usage error.
fn number_from_text(raw: &str, raw_key: &str) -> Result<TomlValue, crate::Error> {
    raw.parse::<TomlValue>().map_err(|_| crate::Error::Usage {
        message: format!(
            "the value for {raw_key}: {raw} is out of range for TOML, whose integers are \
                 64-bit signed (-9223372036854775808 to 9223372036854775807)"
        ),
    })
}

/// TOML has no null. Nothing is written yet, so this is a missing value (exit 1), not a check
/// failure, whose exit-3 wording promises a reverted write.
fn json_to_toml(json: &serde_json::Value, raw_key: &str) -> Result<TomlValue, crate::Error> {
    match json {
        serde_json::Value::Null => Err(crate::Error::NotFound {
            target: raw_key.to_owned(),
            what: "a TOML value (TOML has no null)".to_owned(),
            nearest: None,
        }),
        serde_json::Value::Bool(b) => Ok(TomlValue::from(*b)),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Ok(TomlValue::from(i)),
            None => Ok(TomlValue::from(n.as_f64().unwrap_or_default())),
        },
        serde_json::Value::String(s) => Ok(TomlValue::from(s.clone())),
        serde_json::Value::Array(items) => {
            let mut array = Array::new();
            for item in items {
                array.push(json_to_toml(item, raw_key)?);
            }
            Ok(TomlValue::Array(array))
        },
        serde_json::Value::Object(map) => {
            let mut table = InlineTable::new();
            for (k, v) in map {
                table.insert(k, json_to_toml(v, raw_key)?);
            }
            Ok(TomlValue::InlineTable(table))
        },
    }
}

fn resolved(
    file: &Path,
    doc: &DocumentMut,
    path: &super::Path,
    raw_key: &str,
    flag: &str,
    has_value: bool,
) -> Result<(super::Path, String), crate::Error> {
    let path = super::resolve_dotted(
        file,
        path,
        raw_key,
        |arg| super::flag_hint(flag, has_value, arg),
        |segments| value_exists(doc, segments),
        |segments| line_of(&doc.to_string(), segments),
    )?;
    super::resolve_selectors(
        file,
        &path,
        raw_key,
        |prefix, key| {
            let mut node = Node::Table(doc.as_table());
            for segment in prefix {
                node = descend(node, segment).ok_or_else(|| not_found(raw_key))?;
            }
            match node {
                Node::Array(array) => Ok(array
                    .iter()
                    .map(|element| {
                        element
                            .as_inline_table()
                            .and_then(|table| table.get(key))
                            .and_then(field_text)
                    })
                    .collect()),
                Node::Tables(tables) => Ok(tables
                    .iter()
                    .map(|table| table.get(key).and_then(Item::as_value).and_then(field_text))
                    .collect()),
                Node::Table(_) => Err(not_found(raw_key)),
            }
        },
        |element| line_of(&doc.to_string(), element),
    )
}

fn field_text(value: &TomlValue) -> Option<String> {
    match value {
        TomlValue::String(text) => Some(text.value().clone()),
        TomlValue::Integer(number) => Some(number.display_repr().into_owned()),
        TomlValue::Float(number) => Some(number.display_repr().into_owned()),
        TomlValue::Boolean(flag) => Some(flag.display_repr().into_owned()),
        TomlValue::Datetime(date) => Some(date.display_repr().into_owned()),
        TomlValue::Array(_) | TomlValue::InlineTable(_) => None,
    }
}

enum Node<'a> {
    Table(&'a dyn TableLike),
    Array(&'a Array),
    Tables(&'a ArrayOfTables),
}

/// `DocumentMut` carries no spans, so the line comes from re-parsing as a `Document`. Line 1 is the
/// floor: a line 0 names no line.
fn line_of(text: &str, segments: &[Segment]) -> usize {
    Document::parse(text)
        .ok()
        .and_then(|doc| span_start(doc.as_table(), segments))
        .map_or(1, |start| {
            text[..start].bytes().filter(|byte| *byte == b'\n').count() + 1
        })
}

fn span_start(root: &Table, segments: &[Segment]) -> Option<usize> {
    let (last, parents) = segments.split_last()?;
    let mut node = Node::Table(root);
    for segment in parents {
        node = descend(node, segment)?;
    }
    match (node, last) {
        (Node::Table(table), Segment::Key(key)) => {
            table.get_key_value(key).and_then(|(key, _)| key.span())
        },
        (Node::Array(array), Segment::Index(index)) => array.get(*index).and_then(TomlValue::span),
        (Node::Tables(tables), Segment::Index(index)) => tables.get(*index).and_then(Table::span),
        _ => None,
    }
    .map(|span| span.start)
}

/// Whether `segments` (absolute from the root) names a value already in the document, table or
/// scalar; never creates or descends into one, matching `navigate`'s own read-only pass.
fn value_exists(doc: &DocumentMut, segments: &[Segment]) -> bool {
    let Some((last, parents)) = segments.split_last() else {
        return true;
    };
    let mut node = Node::Table(doc.as_table());
    for segment in parents {
        let Some(next) = descend(node, segment) else {
            return false;
        };
        node = next;
    }
    match (node, last) {
        (Node::Table(table), Segment::Key(key)) => table.get(key).is_some(),
        (Node::Array(array), Segment::Index(index)) => array.get(*index).is_some(),
        (Node::Tables(tables), Segment::Index(index)) => tables.get(*index).is_some(),
        _ => false,
    }
}

fn descend<'a>(node: Node<'a>, segment: &Segment) -> Option<Node<'a>> {
    match (node, segment) {
        (Node::Table(table), Segment::Key(key)) => item_node(table.get(key)?),
        (Node::Array(array), Segment::Index(index)) => value_node(array.get(*index)?),
        (Node::Tables(tables), Segment::Index(index)) => {
            tables.get(*index).map(|table| Node::Table(table))
        },
        _ => None,
    }
}

fn item_node(item: &Item) -> Option<Node<'_>> {
    if let Some(tables) = item.as_array_of_tables() {
        return Some(Node::Tables(tables));
    }
    if let Some(array) = item.as_array() {
        return Some(Node::Array(array));
    }
    item.as_table_like().map(Node::Table)
}

fn value_node(value: &TomlValue) -> Option<Node<'_>> {
    if let Some(array) = value.as_array() {
        return Some(Node::Array(array));
    }
    value.as_inline_table().map(|table| Node::Table(table))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transform::{parse_path, parse_value};

    fn fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/transform")
            .join(name);
        std::fs::read_to_string(path).expect("fixture reads")
    }

    fn at(name: &str) -> std::path::PathBuf {
        Path::new("tests/fixtures/transform").join(name)
    }

    fn set_op(path: &str, value: &str) -> Op {
        Op::Set {
            path: parse_path(path).unwrap(),
            raw_key: path.to_owned(),
            value: parse_value(value),
        }
    }

    fn delete_op(path: &str) -> Op {
        Op::Delete {
            path: parse_path(path).unwrap(),
            raw_key: path.to_owned(),
        }
    }

    fn append_op(path: &str, value: &str) -> Op {
        Op::Append {
            path: parse_path(path).unwrap(),
            raw_key: path.to_owned(),
            value: parse_value(value),
        }
    }

    #[test]
    fn set_on_an_existing_scalar_changes_only_that_line() {
        let content = fixture("config/app.toml");
        let applied = apply(&at("config/app.toml"), &content, &[set_op(
            "server.port",
            "9090",
        )])
        .unwrap();

        let expected = content.replace("port = 8080", "port = 9090");
        assert_eq!(applied.text, expected);
        assert!(matches!(
            &applied.touched[..],
            [(TransformOp::Set { key }, 2)] if key == "server.port"
        ));
    }

    #[test]
    fn set_keeps_the_touched_value_s_spacing_and_trailing_comment() {
        let content = fixture("config/service.toml");
        let applied = apply(&at("config/service.toml"), &content, &[set_op(
            "server.timeout",
            "45",
        )])
        .unwrap();

        assert_eq!(
            applied.text,
            content.replace("30 # seconds", "45 # seconds")
        );
        assert_eq!(applied.touched[0].1, 6);
    }

    #[test]
    fn a_key_inside_an_inline_table_reports_its_own_line() {
        let content = fixture("config/service.toml");
        let applied = apply(&at("config/service.toml"), &content, &[set_op(
            "database.port",
            "5433",
        )])
        .unwrap();

        assert_eq!(applied.text, content.replace("port = 5432", "port = 5433"));
        assert_eq!(applied.touched[0].1, 1);
    }

    #[test]
    fn a_dotted_root_key_reports_its_own_line() {
        let content = fixture("config/service.toml");
        let applied = apply(&at("config/service.toml"), &content, &[set_op(
            "meta.owner",
            "\"infra\"",
        )])
        .unwrap();

        assert_eq!(
            applied.text,
            content.replace("owner = \"platform\"", "owner = \"infra\"")
        );
        assert_eq!(applied.touched[0].1, 2);
    }

    #[test]
    fn an_array_of_tables_element_is_addressable_by_index() {
        let content = fixture("config/service.toml");
        let applied = apply(&at("config/service.toml"), &content, &[set_op(
            "servers[0].port",
            "9090",
        )])
        .unwrap();

        assert_eq!(applied.text, content.replace("port = 9000", "port = 9090"));
        assert_eq!(applied.touched[0].1, 10);
    }

    #[test]
    fn an_out_of_range_array_of_tables_index_is_never_vivified() {
        let content = fixture("config/service.toml");
        let err = apply(&at("config/service.toml"), &content, &[set_op(
            "servers[5].port",
            "9090",
        )])
        .unwrap_err();

        assert!(matches!(err, crate::Error::NotFound { .. }));
        assert!(err.to_string().contains("servers[5].port"));
    }

    #[test]
    fn append_on_a_non_array_path_errors_naming_it() {
        let content = fixture("config/app.toml");
        let err = apply(&at("config/app.toml"), &content, &[append_op(
            "server.port",
            "\"x\"",
        )])
        .unwrap_err();

        assert!(matches!(err, crate::Error::NotFound { .. }));
        assert!(err.to_string().contains("server.port"));
    }

    #[test]
    fn append_on_an_existing_array_adds_the_element() {
        let content = "a = [1, 2]\n".to_owned();
        let applied = apply(&at("a.toml"), &content, &[append_op("a", "3")]).unwrap();

        let doc = applied.text.parse::<DocumentMut>().unwrap();
        let array = doc["a"].as_array().unwrap();
        assert_eq!(
            array.iter().map(TomlValue::as_integer).collect::<Vec<_>>(),
            [Some(1), Some(2), Some(3)]
        );
    }

    #[test]
    fn append_of_a_bare_word_adds_a_quoted_string() {
        let content = "a = [1, 2]\n".to_owned();
        let applied = apply(&at("a.toml"), &content, &[append_op("a", "gh")]).unwrap();

        let doc = applied.text.parse::<DocumentMut>().unwrap();
        let array = doc["a"].as_array().unwrap();
        assert_eq!(array.len(), 3);
        assert_eq!(array.get(2).and_then(TomlValue::as_str), Some("gh"));
        assert!(applied.text.contains("\"gh\""), "{}", applied.text);
    }

    #[test]
    fn append_of_a_json_object_adds_an_inline_table() {
        let content = "a = [{ n = 1 }]\n".to_owned();
        let applied = apply(&at("a.toml"), &content, &[append_op("a", "{\"n\":2}")]).unwrap();

        let doc = applied.text.parse::<DocumentMut>().unwrap();
        let array = doc["a"].as_array().unwrap();
        let added = array.get(1).and_then(TomlValue::as_inline_table).unwrap();
        assert_eq!(added.get("n").and_then(TomlValue::as_integer), Some(2));
    }

    #[test]
    fn malformed_toml_fails_structured_check_before_any_op() {
        let content = "[server\nport = 8080\n";
        let err = apply(&at("config/unparsable.toml"), content, &[set_op(
            "server.port",
            "9090",
        )])
        .unwrap_err();

        assert!(matches!(err, crate::Error::CheckFailed {
            layer: CheckLayer::Structured,
            ..
        }));
        assert!(
            err.to_string()
                .contains("tests/fixtures/transform/config/unparsable.toml"),
            "{err}"
        );
    }

    #[test]
    fn delete_on_an_existing_key_removes_it() {
        let content = "a = 1\nb = 2\n".to_owned();
        let applied = apply(&at("a.toml"), &content, &[delete_op("b")]).unwrap();

        let doc = applied.text.parse::<DocumentMut>().unwrap();
        assert!(!doc.contains_key("b"));
        assert_eq!(doc["a"].as_integer(), Some(1));
    }

    #[test]
    fn delete_on_an_absent_key_errors_naming_it() {
        let content = "a = 1\n".to_owned();
        let err = apply(&at("a.toml"), &content, &[delete_op("missing")]).unwrap_err();

        assert!(matches!(err, crate::Error::NotFound { .. }));
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn set_on_an_absent_key_creates_a_table_for_the_missing_parent() {
        let content = "x = 1\n".to_owned();
        let applied = apply(&at("a.toml"), &content, &[set_op("a.b", "1")]).unwrap();

        let doc = applied.text.parse::<DocumentMut>().unwrap();
        assert_eq!(doc["x"].as_integer(), Some(1));
        assert_eq!(doc["a"]["b"].as_integer(), Some(1));
    }

    #[test]
    fn set_on_a_missing_array_index_never_vivifies_the_slot() {
        let content = "a = [1, 2]\n".to_owned();
        let err = apply(&at("a.toml"), &content, &[set_op("a[5]", "1")]).unwrap_err();

        assert!(matches!(err, crate::Error::NotFound { .. }));
        assert_eq!(err.to_string(), "path not found in a[5]");
    }

    #[test]
    fn a_json_null_value_is_rejected_without_corrupting_the_document() {
        let content = "a = 1\n".to_owned();
        let err = apply(&at("a.toml"), &content, &[set_op("a", "null")]).unwrap_err();

        assert!(matches!(err, crate::Error::NotFound { .. }), "{err:?}");
        assert!(err.to_string().contains("TOML has no null"), "{err}");
    }

    #[test]
    fn a_number_set_over_an_existing_string_stays_a_string() {
        let content = "version = \"3.9\"\nbuild = 7\n";
        let applied = apply(&at("a.toml"), content, &[set_op("version", "3.10")]).unwrap();
        assert_eq!(applied.text, "version = \"3.10\"\nbuild = 7\n");
    }

    #[test]
    fn a_number_set_over_an_existing_number_is_written_as_typed() {
        let content = "version = \"3.9\"\nbuild = 7\n";
        let applied = apply(&at("a.toml"), content, &[set_op("build", "3.10")]).unwrap();
        assert_eq!(applied.text, "version = \"3.9\"\nbuild = 3.10\n");
    }

    #[test]
    fn a_new_key_s_number_changes_only_its_own_line() {
        let content = fixture("config/app.toml");
        let applied = apply(&at("config/app.toml"), &content, &[set_op(
            "server.added",
            "3.10",
        )])
        .unwrap();
        assert_eq!(
            applied.text,
            content.replace("port = 8080\n", "port = 8080\nadded = 3.10\n")
        );
        assert_eq!(applied.touched[0].1, 3);
    }

    #[test]
    fn a_new_key_s_boolean_is_a_boolean_not_a_string() {
        let applied = apply(&at("a.toml"), "a = 1\n", &[set_op("b", "true")]).unwrap();
        assert_eq!(applied.text, "a = 1\nb = true\n");
    }

    #[test]
    fn an_attribute_selector_sets_only_the_matching_table() {
        let content = fixture("config/service.toml");
        let applied = apply(&at("config/service.toml"), &content, &[set_op(
            "servers[name=beta].port",
            "9091",
        )])
        .unwrap();
        assert_eq!(applied.text, content.replace("port = 9001", "port = 9091"));
        assert!(matches!(
            &applied.touched[..],
            [(TransformOp::Set { key }, 14)] if key == "servers[1].port"
        ));
    }

    #[test]
    fn an_attribute_selector_resolves_inside_an_inline_array() {
        let content = "deps = [{ name = \"a\", v = 1 }, { name = \"b\", v = 2 }]\n";
        let applied = apply(&at("a.toml"), content, &[set_op("deps[name=b].v", "3")]).unwrap();
        assert_eq!(
            applied.text,
            "deps = [{ name = \"a\", v = 1 }, { name = \"b\", v = 3 }]\n"
        );
        assert!(matches!(
            &applied.touched[..],
            [(TransformOp::Set { key }, _)] if key == "deps[1].v"
        ));
    }

    #[test]
    fn a_selector_with_no_match_is_not_found_and_lists_the_values_seen() {
        let content = fixture("config/service.toml");
        let err = apply(&at("config/service.toml"), &content, &[set_op(
            "servers[name=gamma].port",
            "1",
        )])
        .unwrap_err();
        assert_eq!(err.slug(), "not_found");
        assert_eq!(
            err.to_string(),
            "an element with name=\"gamma\" (values seen: \"alpha\", \"beta\") not found in \
             servers[name=gamma].port"
        );
    }

    #[test]
    fn a_selector_matching_two_tables_is_ambiguous_and_lists_both_indexes() {
        let content = "[[servers]]\nname = \"dup\"\n\n[[servers]]\nname = \"dup\"\n";
        let err = apply(&at("a.toml"), content, &[set_op(
            "servers[name=dup].port",
            "1",
        )])
        .unwrap_err();
        assert_eq!(err.slug(), "ambiguous");
        let crate::Error::Ambiguous { target, candidates } = &err else {
            panic!("{err:?}");
        };
        assert_eq!(target, "servers[name=dup].port");
        let found: Vec<(usize, &str)> = candidates
            .iter()
            .map(|candidate| (candidate.line, candidate.text.as_str()))
            .collect();
        assert_eq!(found, [
            (1, "servers[0].name=\"dup\""),
            (4, "servers[1].name=\"dup\"")
        ]);
    }

    const BINS: &str = "[package]\nname = \"x\"\n\n[[bin]]\nname = \"a\"\npath = \"a.rs\" # \
                        first\n";

    #[test]
    fn an_object_appended_to_an_array_of_tables_adds_a_table_after_the_last() {
        let applied = apply(&at("Cargo.toml"), BINS, &[append_op(
            "bin",
            r#"{"name":"b","path":"b.rs"}"#,
        )])
        .unwrap();
        assert_eq!(
            applied.text,
            format!("{BINS}\n[[bin]]\nname = \"b\"\npath = \"b.rs\"\n")
        );
        assert_eq!(applied.touched[0].1, 8);
    }

    #[test]
    fn an_appended_table_lands_after_the_last_of_its_array_not_after_a_later_table() {
        let content = "[[bin]]\nname = \"a\"\n\n[deps]\nx = 1\n";
        let applied = apply(&at("Cargo.toml"), content, &[append_op(
            "bin",
            r#"{"name":"b"}"#,
        )])
        .unwrap();
        assert_eq!(
            applied.text,
            "[[bin]]\nname = \"a\"\n\n[[bin]]\nname = \"b\"\n\n[deps]\nx = 1\n"
        );
        assert_eq!(applied.touched[0].1, 4);
    }

    #[test]
    fn an_appended_table_keeps_the_key_order_typed() {
        let applied = apply(&at("Cargo.toml"), BINS, &[append_op(
            "bin",
            r#"{"path":"b.rs","name":"b","edition":"2024"}"#,
        )])
        .unwrap();
        assert_eq!(
            applied.text,
            format!("{BINS}\n[[bin]]\npath = \"b.rs\"\nname = \"b\"\nedition = \"2024\"\n")
        );
    }

    #[test]
    fn the_same_keys_typed_the_other_way_round_land_the_other_way_round() {
        let applied = apply(&at("Cargo.toml"), BINS, &[append_op(
            "bin",
            r#"{"edition":"2024","name":"b","path":"b.rs"}"#,
        )])
        .unwrap();
        assert_eq!(
            applied.text,
            format!("{BINS}\n[[bin]]\nedition = \"2024\"\nname = \"b\"\npath = \"b.rs\"\n")
        );
    }

    #[test]
    fn an_object_appended_to_a_plain_array_is_an_inline_table_in_typed_order() {
        let content = "a = [{ n = 1 }]\n";
        let applied = apply(&at("a.toml"), content, &[append_op(
            "a",
            r#"{"z":"last-typed-first","n":2.50}"#,
        )])
        .unwrap();
        assert_eq!(
            applied.text,
            "a = [{ n = 1 }, { z = \"last-typed-first\", n = 2.50 }]\n"
        );
        assert!(applied.text.parse::<DocumentMut>().is_ok());
    }

    #[test]
    fn a_non_object_appended_to_an_array_of_tables_is_refused_naming_the_object_it_needs() {
        let err = apply(&at("Cargo.toml"), BINS, &[append_op("bin", "b.rs")]).unwrap_err();
        assert_eq!(err.slug(), "not_found");
        assert_eq!(
            err.to_string(),
            "a JSON object to append (an array of tables takes only objects) not found in bin"
        );
    }

    /// `top` and `a` each sit in one array only, so a read against the wrong array finds them.
    const DEEP: &str = "[[plugins]]\nname = \"top\"\nv = 5\n\n[outer]\nplugins = [{ name = \"alpha\", v \
                        = 10 }, { name = \"gitty\", v = 11 }]\n\n[[groups]]\nname = \"one\"\nplugins = \
                        [{ name = \"a\", v = 1 }, { name = \"b\", v = 2 }]\n\n[[groups]]\nname = \
                        \"two\"\n\n[[groups.plugins]]\nname = \"b\"\nv = 3\n\n[[groups.plugins]]\nname \
                        = \"c\"\nv = 4\n";

    fn deep(op: &Op) -> Result<Applied, crate::Error> {
        apply(&at("deep.toml"), DEEP, std::slice::from_ref(op))
    }

    #[test]
    fn a_selector_under_a_multi_segment_prefix_resolves_in_that_array() {
        let applied = deep(&set_op("outer.plugins[name=gitty].v", "99")).unwrap();
        assert_eq!(applied.text, DEEP.replace("v = 11", "v = 99"));
        assert!(matches!(
            &applied.touched[..],
            [(TransformOp::Set { key }, 6)] if key == "outer.plugins[1].v"
        ));
    }

    #[test]
    fn a_selector_under_a_multi_segment_prefix_misses_a_value_only_a_shallower_array_holds() {
        let err = deep(&set_op("outer.plugins[name=top].v", "99")).unwrap_err();
        assert_eq!(err.slug(), "not_found");
        assert!(
            err.to_string()
                .contains("(values seen: \"alpha\", \"gitty\")"),
            "{err}"
        );
    }

    #[test]
    fn a_selector_inside_a_table_of_another_array_resolves_in_that_table() {
        for (path, key, line, from, to) in [
            (
                "groups[name=two].plugins[name=b].v",
                "groups[1].plugins[0].v",
                17,
                "v = 3",
                "v = 99",
            ),
            (
                "groups[1].plugins[name=c].v",
                "groups[1].plugins[1].v",
                21,
                "v = 4",
                "v = 99",
            ),
            (
                "groups[name=one].plugins[name=b].v",
                "groups[0].plugins[1].v",
                10,
                "v = 2",
                "v = 99",
            ),
        ] {
            let applied = deep(&set_op(path, "99")).unwrap();
            assert_eq!(applied.text, DEEP.replace(from, to), "{path}");
            assert!(
                matches!(
                    &applied.touched[..],
                    [(TransformOp::Set { key: written }, at)] if written == key && *at == line
                ),
                "{path}: {:?}",
                applied.touched
            );
        }
    }

    #[test]
    fn a_selector_inside_a_table_of_another_array_misses_a_value_only_a_sibling_holds() {
        let err = deep(&set_op("groups[name=two].plugins[name=a].v", "99")).unwrap_err();
        assert_eq!(err.slug(), "not_found");
        assert!(
            err.to_string().contains("(values seen: \"b\", \"c\")"),
            "{err}"
        );
    }

    #[test]
    fn an_integer_past_i64_is_a_usage_error_naming_the_limit() {
        for path in ["b", "new"] {
            let err = apply(&at("a.toml"), "b = 1\n", &[set_op(
                path,
                "99999999999999999999",
            )])
            .unwrap_err();
            assert!(matches!(err, crate::Error::Usage { .. }), "{err:?}");
            assert_eq!(
                err.to_string(),
                format!(
                    "the value for {path}: 99999999999999999999 is out of range for TOML, \
                     whose integers are 64-bit signed (-9223372036854775808 to \
                     9223372036854775807)"
                )
            );
        }
    }

    #[test]
    fn an_integer_past_i64_over_a_string_is_written_as_the_string() {
        let applied = apply(&at("a.toml"), "b = \"x\"\n", &[set_op(
            "b",
            "99999999999999999999",
        )])
        .unwrap();
        assert_eq!(applied.text, "b = \"99999999999999999999\"\n");
    }

    #[test]
    fn the_largest_i64_is_written_as_typed() {
        let applied = apply(&at("a.toml"), "b = 1\n", &[set_op(
            "b",
            "9223372036854775807",
        )])
        .unwrap();
        assert_eq!(applied.text, "b = 9223372036854775807\n");
        let applied = apply(&at("a.toml"), "b = 1\n", &[set_op(
            "b",
            "-9223372036854775808",
        )])
        .unwrap();
        assert_eq!(applied.text, "b = -9223372036854775808\n");
    }
}
