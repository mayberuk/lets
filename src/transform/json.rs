use std::borrow::Cow;
use std::fmt::Write as _;
use std::ops::Range;
use std::path::Path;

use jsonc_parser::cst::{CstArray, CstInputValue, CstNode, CstObject, CstRootNode};
use jsonc_parser::tokens::Token;
use jsonc_parser::{ParseOptions, Scanner, ScannerOptions, ast};

use super::{Applied, Op, Segment, TransformOp, Value};
use crate::error::CheckLayer;

const BOM: char = '\u{feff}';

pub fn apply(file: &Path, content: &str, ops: &[Op]) -> Result<Applied, crate::Error> {
    // `jsonc-parser` rejects a leading byte-order mark and `ParseOptions` has no switch for it.
    let (bom, body) = match content.strip_prefix(BOM) {
        Some(body) => (true, body),
        None => (false, content),
    };
    if let [op @ Op::Set { .. }] = ops
        && let Some(applied) = spliced_set(file, bom, body, op)?
    {
        return Ok(applied);
    }
    through_cst(file, bom, body, ops)
}

/// The CST costs ~50 bytes per source byte (439 MB peak for an 8 MiB file), so a scalar-over-scalar
/// `--set` is spliced instead. `None` defers to the CST, whose scalar rendering the splice borrows.
fn spliced_set(
    file: &Path,
    bom: bool,
    body: &str,
    op: &Op,
) -> Result<Option<Applied>, crate::Error> {
    let Op::Set {
        path,
        raw_key,
        value,
    } = op
    else {
        return Ok(None);
    };
    let line_at = |byte: usize| body[..byte].matches('\n').count() + 1;
    let mut unread = false;
    let path = super::resolve_dotted(
        file,
        path,
        raw_key,
        |arg| super::flag_hint("--set", true, arg),
        |segments| {
            if let Ok(found) = locate(body, segments) {
                found.is_some()
            } else {
                unread = true;
                false
            }
        },
        |segments| match locate(body, segments) {
            Ok(Some((_, span))) => line_at(span.start),
            _ => 1,
        },
    )?;
    if unread {
        return Ok(None);
    }
    let resolved = super::resolve_selectors(
        file,
        &path,
        raw_key,
        |prefix, key| match fields_at(body, prefix, key) {
            Ok(Some(fields)) => Ok(fields),
            Ok(None) => Err(not_found(raw_key)),
            Err(Unread) => {
                unread = true;
                Err(not_found(raw_key))
            },
        },
        |element| match locate(body, element) {
            Ok(Some((_, span))) => line_at(span.start),
            _ => 1,
        },
    );
    if unread {
        return Ok(None);
    }
    let (path, key) = resolved?;
    let Ok(Some((existing, range))) = locate(body, &path.0) else {
        return Ok(None);
    };
    let is_string = match existing {
        Token::String(_) => true,
        Token::Number(_) | Token::Boolean(_) | Token::Null => false,
        _ => return Ok(None),
    };
    let input = typed(is_string, value);
    if matches!(input, CstInputValue::Array(_) | CstInputValue::Object(_)) {
        return Ok(None);
    }
    let scalar =
        CstRootNode::parse("null", &ParseOptions::default()).expect("`null` is a JSON document");
    scalar.set_value(input);
    let line = line_at(range.start);

    let replacement = scalar.to_string();
    let mut text = String::with_capacity(body.len() + replacement.len() + BOM.len_utf8());
    if bom {
        text.push(BOM);
    }
    text.push_str(&body[..range.start]);
    text.push_str(&replacement);
    text.push_str(&body[range.end..]);
    Ok(Some(Applied {
        text,
        touched: vec![(TransformOp::Set { key }, line)],
    }))
}

/// A document `walk` does not read; the CST decides instead.
struct Unread;

enum Step<'t> {
    Key(Cow<'t, str>),
    Index(usize),
}

impl Step<'_> {
    fn is(&self, segment: &Segment) -> bool {
        match (self, segment) {
            (Step::Key(key), Segment::Key(want)) => key == want,
            (Step::Index(index), Segment::Index(want)) => index == want,
            _ => false,
        }
    }
}

fn along(path: &[Step], route: &[Segment]) -> bool {
    path.len() <= route.len()
        && path
            .iter()
            .zip(route)
            .all(|(step, segment)| step.is(segment))
}

/// `jsonc-parser` refuses nesting past 512 ranges, and an object level opens two.
const WALK_DEPTH: usize = 128;

/// Reads a strict subset of what `jsonc-parser` accepts by default, so a document it walks to the
/// end parses too; anything else, a missing comma the parser would allow included, is `Unread`.
fn walk<'t>(
    body: &'t str,
    mut visit: impl FnMut(&[Step<'t>], &Token<'t>, Range<usize>) -> Result<(), Unread>,
) -> Result<(), Unread> {
    let mut tokens = Tokens(Scanner::new(body, &ScannerOptions::default()));
    let mut path: Vec<Step<'t>> = Vec::new();
    let mut arrays: Vec<bool> = Vec::new();
    let mut value = tokens.need()?;
    'value: loop {
        visit(&path, &value.0, value.1.clone())?;
        match value.0 {
            Token::OpenBrace | Token::OpenBracket => {
                let array = value.0 == Token::OpenBracket;
                if arrays.len() == WALK_DEPTH {
                    return Err(Unread);
                }
                arrays.push(array);
                let first = tokens.need()?;
                match (array, first.0) {
                    (false, Token::CloseBrace) | (true, Token::CloseBracket) => {
                        arrays.pop();
                    },
                    (true, token) => {
                        path.push(Step::Index(0));
                        value = (token, first.1);
                        continue 'value;
                    },
                    (false, name) => {
                        path.push(tokens.member(name)?);
                        value = tokens.need()?;
                        continue 'value;
                    },
                }
            },
            Token::String(_) | Token::Number(_) | Token::Boolean(_) | Token::Null => {},
            _ => return Err(Unread),
        }
        while let Some(&array) = arrays.last() {
            let close = if array {
                Token::CloseBracket
            } else {
                Token::CloseBrace
            };
            let done = path.pop();
            let (separator, _) = tokens.need()?;
            if separator == close {
                arrays.pop();
                continue;
            }
            if separator != Token::Comma {
                return Err(Unread);
            }
            let after = tokens.need()?;
            if after.0 == close {
                arrays.pop();
                continue;
            }
            match (done, array) {
                (Some(Step::Index(index)), true) => {
                    path.push(Step::Index(index + 1));
                    value = after;
                },
                (_, false) => {
                    path.push(tokens.member(after.0)?);
                    value = tokens.need()?;
                },
                _ => return Err(Unread),
            }
            continue 'value;
        }
        return match tokens.next()? {
            None => Ok(()),
            Some(_) => Err(Unread),
        };
    }
}

struct Tokens<'t>(Scanner<'t>);

impl<'t> Tokens<'t> {
    fn next(&mut self) -> Result<Option<(Token<'t>, Range<usize>)>, Unread> {
        loop {
            match self.0.scan().map_err(|_| Unread)? {
                Some(Token::CommentLine(_) | Token::CommentBlock(_)) => {},
                Some(token) => return Ok(Some((token, self.0.token_start()..self.0.token_end()))),
                None => return Ok(None),
            }
        }
    }

    fn need(&mut self) -> Result<(Token<'t>, Range<usize>), Unread> {
        self.next()?.ok_or(Unread)
    }

    fn member(&mut self, name: Token<'t>) -> Result<Step<'t>, Unread> {
        let key = match name {
            Token::String(key) => key,
            Token::Word(key) => Cow::Borrowed(key),
            _ => return Err(Unread),
        };
        match self.need()? {
            (Token::Colon, _) => Ok(Step::Key(key)),
            _ => Err(Unread),
        }
    }
}

/// A duplicated key defers to the CST, which reads the first occurrence; an unfinished walk cannot
/// tell which one that is.
fn locate<'t>(
    body: &'t str,
    route: &[Segment],
) -> Result<Option<(Token<'t>, Range<usize>)>, Unread> {
    let mut found = None;
    let mut visits = vec![0u8; route.len()];
    walk(body, |path, token, span| {
        if path.is_empty() || !along(path, route) {
            return Ok(());
        }
        let seen = &mut visits[path.len() - 1];
        if *seen > 0 {
            return Err(Unread);
        }
        *seen = 1;
        if path.len() == route.len() {
            found = Some((token.clone(), span));
        }
        Ok(())
    })?;
    Ok(found)
}

/// Within an element the first `key` counts, as the CST's `get` reads it.
fn fields_at(
    body: &str,
    prefix: &[Segment],
    key: &str,
) -> Result<Option<Vec<Option<String>>>, Unread> {
    let mut visits = vec![0u8; prefix.len()];
    let mut is_array = false;
    let mut fields: Vec<Option<String>> = Vec::new();
    let mut read: Option<usize> = None;
    walk(body, |path, token, _| {
        let depth = path.len().min(prefix.len());
        if !along(&path[..depth], prefix) {
            return Ok(());
        }
        match &path[depth..] {
            [] if !path.is_empty() => {
                let seen = &mut visits[path.len() - 1];
                if *seen > 0 {
                    return Err(Unread);
                }
                *seen = 1;
                if path.len() == prefix.len() {
                    is_array = *token == Token::OpenBracket;
                }
            },
            [Step::Index(_)] => fields.push(None),
            [Step::Index(index), Step::Key(name)] if name == key && read != Some(*index) => {
                read = Some(*index);
                fields[*index] = match token {
                    Token::String(text) => Some(text.to_string()),
                    Token::Number(text) => Some((*text).to_owned()),
                    Token::Boolean(flag) => Some(flag.to_string()),
                    _ => None,
                };
            },
            _ => {},
        }
        Ok(())
    })?;
    Ok(is_array.then_some(fields))
}

fn through_cst(file: &Path, bom: bool, body: &str, ops: &[Op]) -> Result<Applied, crate::Error> {
    let root = CstRootNode::parse(body, &ParseOptions::default()).map_err(|err| {
        crate::Error::CheckFailed {
            path: file.to_path_buf(),
            layer: CheckLayer::Structured,
            detail: err.to_string(),
        }
    })?;

    let mut touched = Vec::with_capacity(ops.len());
    for op in ops {
        match op {
            Op::Set {
                path,
                raw_key,
                value,
            } => {
                let (path, key) = resolved(file, &root, path, raw_key, "--set", true)?;
                let node = set(&root, &path.0, raw_key, value)?;
                touched.push((TransformOp::Set { key }, line_number(&node)));
            },
            Op::Delete { path, raw_key } => {
                let (path, key) = resolved(file, &root, path, raw_key, "--delete", false)?;
                let line = delete(&root, &path.0, raw_key)?;
                touched.push((TransformOp::Delete { key }, line));
            },
            Op::Append {
                path,
                raw_key,
                value,
            } => {
                let (path, key) = resolved(file, &root, path, raw_key, "--append", true)?;
                let line = append(&root, &path.0, raw_key, value)?;
                touched.push((TransformOp::Append { key }, line));
            },
        }
    }

    let mut text = String::with_capacity(body.len() + BOM.len_utf8() + 64);
    if bom {
        text.push(BOM);
    }
    let _ = write!(text, "{root}");
    Ok(Applied { text, touched })
}

fn resolved(
    file: &Path,
    root: &CstRootNode,
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
        |segments| value_exists(root, segments),
        |segments| value_line(root, segments),
    )?;
    super::resolve_selectors(
        file,
        &path,
        raw_key,
        |prefix, key| {
            let array = array_at(root, prefix).ok_or_else(|| not_found(raw_key))?;
            Ok(array
                .elements()
                .iter()
                .map(|element| field_text(element, key))
                .collect())
        },
        |element| {
            let (index, prefix) = match element.split_last() {
                Some((Segment::Index(index), prefix)) => (*index, prefix),
                _ => return 1,
            };
            array_at(root, prefix)
                .and_then(|array| array.elements().into_iter().nth(index))
                .map_or(1, |node| line_number(&node))
        },
    )
}

/// Whether `segments` (absolute from the root) names a value already in the document, container
/// or scalar; `navigate` with `create: false` leaves everything else it touches unmodified.
fn value_exists(root: &CstRootNode, segments: &[Segment]) -> bool {
    let Some((last, _)) = segments.split_last() else {
        return true;
    };
    let Some(root_obj) = root.object_value() else {
        return false;
    };
    let Some(cursor) = navigate(root_obj, segments, false) else {
        return false;
    };
    match (cursor, last) {
        (Cursor::Obj(obj), Segment::Key(key)) => obj.get(key).is_some(),
        (Cursor::Arr(arr), Segment::Index(index)) => {
            arr.elements().into_iter().nth(*index).is_some()
        },
        _ => false,
    }
}

/// The line of the value `value_exists` already confirmed is there; 1 is the floor for one it
/// cannot re-locate (unreachable in practice, since the caller only asks after `value_exists`).
fn value_line(root: &CstRootNode, segments: &[Segment]) -> usize {
    let Some((last, _)) = segments.split_last() else {
        return 1;
    };
    let Some(node) = root.object_value().and_then(|root_obj| {
        let cursor = navigate(root_obj, segments, false)?;
        match (cursor, last) {
            (Cursor::Obj(obj), Segment::Key(key)) => obj.get(key)?.value(),
            (Cursor::Arr(arr), Segment::Index(index)) => arr.elements().into_iter().nth(*index),
            _ => None,
        }
    }) else {
        return 1;
    };
    line_number(&node)
}

/// The trailing `Index(0)` makes `navigate` stop on the array itself.
fn array_at(root: &CstRootNode, prefix: &[Segment]) -> Option<CstArray> {
    let mut probe = prefix.to_vec();
    probe.push(Segment::Index(0));
    match navigate(root.object_value()?, &probe, false)? {
        Cursor::Arr(array) => Some(array),
        Cursor::Obj(_) => None,
    }
}

fn field_text(element: &CstNode, key: &str) -> Option<String> {
    let value = element.as_object()?.get(key)?.value()?;
    if let Some(text) = value.as_string_lit() {
        return text.decoded_value().ok();
    }
    if value.as_number_lit().is_some() {
        return Some(value.to_string());
    }
    value.as_boolean_lit().map(|flag| flag.value().to_string())
}

enum Cursor {
    Obj(CstObject),
    Arr(CstArray),
}

fn not_found(raw_key: &str) -> crate::Error {
    crate::Error::NotFound {
        target: raw_key.to_owned(),
        what: "path".to_owned(),
        nearest: None,
    }
}

/// With `create`, a missing object key is created; an array element never is (jq and yq agree).
fn navigate(root_obj: CstObject, segments: &[Segment], create: bool) -> Option<Cursor> {
    let mut cursor = Cursor::Obj(root_obj);
    for i in 0..segments.len().saturating_sub(1) {
        let need_array = matches!(segments[i + 1], Segment::Index(_));
        cursor = step(cursor, &segments[i], need_array, create)?;
    }
    Some(cursor)
}

fn step(cursor: Cursor, segment: &Segment, need_array: bool, create: bool) -> Option<Cursor> {
    match (cursor, segment) {
        (Cursor::Obj(obj), Segment::Key(key)) => {
            if need_array {
                obj.array_value(key).map(Cursor::Arr)
            } else if create {
                obj.object_value_or_create(key).map(Cursor::Obj)
            } else {
                obj.object_value(key).map(Cursor::Obj)
            }
        },
        (Cursor::Arr(arr), Segment::Index(index)) => {
            let node = arr.elements().into_iter().nth(*index)?;
            if need_array {
                node.as_array().map(Cursor::Arr)
            } else {
                node.as_object().map(Cursor::Obj)
            }
        },
        // Unreachable: the lookahead in `navigate` matched each cursor to its segment's kind.
        _ => None,
    }
}

fn set(
    root: &CstRootNode,
    segments: &[Segment],
    raw_key: &str,
    value: &Value,
) -> Result<CstNode, crate::Error> {
    let root_obj = root
        .object_value_or_create()
        .ok_or_else(|| not_found(raw_key))?;
    let cursor = navigate(root_obj, segments, true).ok_or_else(|| not_found(raw_key))?;
    let node = match (cursor, segments.last()) {
        (Cursor::Obj(obj), Some(Segment::Key(key))) => match obj.get(key) {
            Some(prop) => {
                let is_string = prop
                    .value()
                    .is_some_and(|node| node.as_string_lit().is_some());
                let cst_value = typed(is_string, value);
                prop.set_value(cst_value);
                prop.value()
            },
            None => obj.append(key, to_cst_value(value)).value(),
        },
        (Cursor::Arr(arr), Some(Segment::Index(index))) => {
            arr.elements().into_iter().nth(*index).and_then(|target| {
                let cst_value = typed(target.as_string_lit().is_some(), value);
                replace_value(&target, cst_value)
            })
        },
        _ => None,
    };
    node.ok_or_else(|| not_found(raw_key))
}

fn typed(existing_is_string: bool, value: &Value) -> CstInputValue {
    let text = existing_is_string.then(|| value.string_form()).flatten();
    match text {
        Some(text) => CstInputValue::String(text.into_owned()),
        None => to_cst_value(value),
    }
}

fn delete(root: &CstRootNode, segments: &[Segment], raw_key: &str) -> Result<usize, crate::Error> {
    let root_obj = root.object_value().ok_or_else(|| not_found(raw_key))?;
    let cursor = navigate(root_obj, segments, false).ok_or_else(|| not_found(raw_key))?;
    match (cursor, segments.last()) {
        (Cursor::Obj(obj), Some(Segment::Key(key))) => {
            let prop = obj.get(key).ok_or_else(|| not_found(raw_key))?;
            let value = prop.value().ok_or_else(|| not_found(raw_key))?;
            let line = line_number(&value);
            prop.remove();
            Ok(line)
        },
        (Cursor::Arr(arr), Some(Segment::Index(index))) => {
            let node = arr
                .elements()
                .into_iter()
                .nth(*index)
                .ok_or_else(|| not_found(raw_key))?;
            let line = line_number(&node);
            node.remove();
            Ok(line)
        },
        _ => Err(not_found(raw_key)),
    }
}

fn append(
    root: &CstRootNode,
    segments: &[Segment],
    raw_key: &str,
    value: &Value,
) -> Result<usize, crate::Error> {
    let root_obj = root.object_value().ok_or_else(|| not_found(raw_key))?;
    let cursor = navigate(root_obj, segments, false).ok_or_else(|| not_found(raw_key))?;
    let array = match (cursor, segments.last()) {
        (Cursor::Obj(obj), Some(Segment::Key(key))) => obj.array_value(key),
        (Cursor::Arr(arr), Some(Segment::Index(index))) => arr
            .elements()
            .into_iter()
            .nth(*index)
            .and_then(|node| node.as_array()),
        _ => None,
    }
    .ok_or_else(|| not_found(raw_key))?;
    let node = array.append(to_cst_value(value));
    Ok(line_number(&node))
}

fn replace_value(node: &CstNode, value: CstInputValue) -> Option<CstNode> {
    if let Some(n) = node.as_object() {
        return n.replace_with(value);
    }
    if let Some(n) = node.as_array() {
        return n.replace_with(value);
    }
    if let Some(n) = node.as_string_lit() {
        return n.replace_with(value);
    }
    if let Some(n) = node.as_number_lit() {
        return n.replace_with(value);
    }
    if let Some(n) = node.as_boolean_lit() {
        return n.replace_with(value);
    }
    if let Some(n) = node.as_null_keyword() {
        return n.replace_with(value);
    }
    if let Some(n) = node.as_word_lit() {
        return n.replace_with(value);
    }
    None
}

fn to_cst_value(value: &Value) -> CstInputValue {
    match value {
        Value::Json(json) => json_to_cst(json),
        Value::Number(raw) => CstInputValue::Number(raw.clone()),
        Value::Container(raw) => typed_to_cst(&super::container(raw)),
    }
}

/// Keeps typed key order and number text, both lost in `json_to_cst`'s `serde_json` source.
fn typed_to_cst(value: &ast::Value) -> CstInputValue {
    match value {
        ast::Value::NullKeyword(_) => CstInputValue::Null,
        ast::Value::BooleanLit(flag) => CstInputValue::Bool(flag.value),
        ast::Value::NumberLit(number) => CstInputValue::Number(number.value.to_owned()),
        ast::Value::StringLit(text) => CstInputValue::String(text.value.to_string()),
        ast::Value::Array(array) => {
            CstInputValue::Array(array.elements.iter().map(typed_to_cst).collect())
        },
        ast::Value::Object(object) => CstInputValue::Object(
            object
                .properties
                .iter()
                .map(|prop| (prop.name.as_str().to_owned(), typed_to_cst(&prop.value)))
                .collect(),
        ),
    }
}

fn json_to_cst(value: &serde_json::Value) -> CstInputValue {
    match value {
        serde_json::Value::Null => CstInputValue::Null,
        serde_json::Value::Bool(b) => CstInputValue::Bool(*b),
        serde_json::Value::Number(n) => CstInputValue::Number(n.to_string()),
        serde_json::Value::String(s) => CstInputValue::String(s.clone()),
        serde_json::Value::Array(items) => {
            CstInputValue::Array(items.iter().map(json_to_cst).collect())
        },
        serde_json::Value::Object(map) => CstInputValue::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), json_to_cst(v)))
                .collect(),
        ),
    }
}

/// `CstNode` exposes no byte offset, so the line is counted from the rendered text of every earlier
/// sibling up to the root.
fn line_number(node: &CstNode) -> usize {
    let mut rendered = String::new();
    let mut newlines = newline_count(&mut rendered, node.previous_siblings());
    let mut ancestor = node.parent();
    while let Some(container) = ancestor {
        newlines += newline_count(&mut rendered, container.previous_siblings());
        ancestor = container.parent();
    }
    newlines + 1
}

/// One caller-owned buffer: `to_string()` per sibling allocates at every ancestor level, per op.
fn newline_count(rendered: &mut String, siblings: impl Iterator<Item = CstNode>) -> usize {
    siblings
        .map(|sibling| {
            rendered.clear();
            let _ = write!(rendered, "{sibling}");
            rendered.matches('\n').count()
        })
        .sum()
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
    fn set_on_an_existing_boolean_changes_only_that_line() {
        let content = fixture("config/app.json");
        let applied = apply(&at("config/app.json"), &content, &[set_op(
            "features.e2e",
            "false",
        )])
        .unwrap();

        let expected = content.replace("\"e2e\": true", "\"e2e\": false");
        assert_eq!(applied.text, expected);
        assert_eq!(applied.touched.len(), 1);
        assert!(matches!(
            &applied.touched[0],
            (TransformOp::Set { key }, 3) if key == "features.e2e"
        ));
    }

    #[test]
    fn set_on_an_absent_key_inserts_it_after_the_last_property() {
        let content = fixture("config/commented.json");
        let applied = apply(&at("config/commented.json"), &content, &[set_op(
            "version", "\"2\"",
        )])
        .unwrap();

        assert_eq!(
            applied.text,
            content.replace("  }\n}", "  },\n  \"version\": \"2\"\n}")
        );
        assert_eq!(applied.touched[0].1, 12);
    }

    #[test]
    fn set_on_an_existing_key_does_not_duplicate_it() {
        let content = fixture("config/commented.json");
        let applied = apply(&at("config/commented.json"), &content, &[set_op(
            "legacy", "false",
        )])
        .unwrap();

        assert_eq!(
            applied.text,
            content.replace("\"legacy\": true", "\"legacy\": false")
        );
        assert_eq!(applied.touched[0].1, 8);
    }

    #[test]
    fn set_on_an_array_index_replaces_that_element() {
        let content = fixture("config/commented.json");
        let applied = apply(&at("config/commented.json"), &content, &[set_op(
            "allow[0]", "\"fd\"",
        )])
        .unwrap();

        assert_eq!(applied.text, content.replace("\"ls\"", "\"fd\""));
        assert_eq!(applied.touched[0].1, 4);
    }

    #[test]
    fn delete_on_an_array_index_removes_that_element() {
        let content = fixture("config/commented.json");
        let applied = apply(&at("config/commented.json"), &content, &[delete_op(
            "allow[1]",
        )])
        .unwrap();

        assert_eq!(
            applied.text,
            content.replace("    \"ls\",\n    \"rg\"\n", "    \"ls\"\n")
        );
        assert_eq!(applied.touched[0].1, 5);
    }

    #[test]
    fn delete_on_an_existing_key_removes_its_whole_line() {
        let content = fixture("config/commented.json");
        let applied = apply(&at("config/commented.json"), &content, &[delete_op(
            "legacy",
        )])
        .unwrap();

        assert_eq!(applied.text, content.replace("  \"legacy\": true,\n", ""));
        assert_eq!(applied.touched[0].1, 8);
    }

    #[test]
    fn delete_on_an_absent_path_errors_naming_it_and_applies_nothing() {
        let content = "{\"a\": 1}".to_owned();
        let err = apply(&at("a.json"), &content, &[
            set_op("a", "9"),
            delete_op("missing"),
        ])
        .unwrap_err();

        assert!(matches!(err, crate::Error::NotFound { .. }));
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn append_on_an_existing_array_adds_the_element() {
        let content = fixture("config/commented.json");
        let applied = apply(&at("config/commented.json"), &content, &[append_op(
            "allow", "\"gh\"",
        )])
        .unwrap();

        assert_eq!(
            applied.text,
            content.replace("    \"rg\"\n", "    \"rg\",\n    \"gh\"\n")
        );
        assert_eq!(applied.touched[0].1, 6);
    }

    #[test]
    fn append_on_a_non_array_path_errors_naming_it() {
        let content = "{\"a\": 1}".to_owned();
        let err = apply(&at("a.json"), &content, &[append_op("a", "3")]).unwrap_err();

        assert!(matches!(err, crate::Error::NotFound { .. }));
    }

    #[test]
    fn malformed_json_fails_structured_check_before_any_op() {
        let content = fixture("broken/invalid.json");
        let err = apply(&at("broken/invalid.json"), &content, &[set_op("a", "1")]).unwrap_err();

        assert!(matches!(err, crate::Error::CheckFailed {
            layer: CheckLayer::Structured,
            ..
        }));
        assert!(
            err.to_string()
                .contains("tests/fixtures/transform/broken/invalid.json"),
            "{err}"
        );
    }

    const PLUGINS: &str = r#"{
  "version": "3.9",
  "build": 7,
  "plugins": [
    { "name": "alpha", "version": "1.0" },
    { "name": "beta", "version": "1.1" },
    { "name": "gitty", "version": "0.9" }
  ],
  "twins": [
    { "name": "dup", "port": 1 },
    { "name": "dup", "port": 2 }
  ]
}
"#;

    fn plugins(ops: &[Op]) -> Result<Applied, crate::Error> {
        apply(&at("plugins.json"), PLUGINS, ops)
    }

    #[test]
    fn a_number_set_over_an_existing_string_stays_a_string() {
        let applied = plugins(&[set_op("version", "3.10")]).unwrap();
        assert_eq!(
            applied.text,
            PLUGINS.replace(r#""version": "3.9""#, r#""version": "3.10""#)
        );
    }

    #[test]
    fn a_boolean_set_over_an_existing_string_stays_a_string() {
        let applied = plugins(&[set_op("version", "false")]).unwrap();
        assert_eq!(
            applied.text,
            PLUGINS.replace(r#""version": "3.9""#, r#""version": "false""#)
        );
    }

    #[test]
    fn null_set_over_an_existing_string_is_written_as_null() {
        let applied = plugins(&[set_op("version", "null")]).unwrap();
        assert_eq!(
            applied.text,
            PLUGINS.replace(r#""version": "3.9""#, r#""version": null"#)
        );
    }

    #[test]
    fn a_number_set_over_an_existing_number_stays_a_number() {
        let applied = plugins(&[set_op("build", "3.10")]).unwrap();
        assert_eq!(
            applied.text,
            PLUGINS.replace(r#""build": 7"#, r#""build": 3.10"#)
        );
    }

    #[test]
    fn a_number_set_over_an_existing_boolean_is_parsed_to_a_number() {
        let content = "{\n  \"on\": true\n}\n";
        let applied = apply(&at("a.json"), content, &[set_op("on", "1")]).unwrap();
        assert_eq!(applied.text, "{\n  \"on\": 1\n}\n");
    }

    #[test]
    fn a_new_key_s_number_keeps_its_typed_text() {
        let content = "{\n  \"a\": 1\n}\n";
        let applied = apply(&at("a.json"), content, &[set_op("b", "3.10")]).unwrap();
        assert_eq!(applied.text, "{\n  \"a\": 1,\n  \"b\": 3.10\n}\n");
    }

    #[test]
    fn a_new_key_s_boolean_is_a_boolean_not_a_string() {
        let content = "{\n  \"a\": 1\n}\n";
        let applied = apply(&at("a.json"), content, &[set_op("b", "true")]).unwrap();
        assert_eq!(applied.text, "{\n  \"a\": 1,\n  \"b\": true\n}\n");
    }

    #[test]
    fn an_attribute_selector_sets_only_the_matching_element() {
        let applied = plugins(&[set_op("plugins[name=gitty].version", "1.0")]).unwrap();
        assert_eq!(
            applied.text,
            PLUGINS.replace(
                r#"{ "name": "gitty", "version": "0.9" }"#,
                r#"{ "name": "gitty", "version": "1.0" }"#
            )
        );
        assert!(matches!(
            &applied.touched[..],
            [(TransformOp::Set { key }, 7)] if key == "plugins[2].version"
        ));
    }

    #[test]
    fn a_selector_compares_a_number_field_by_its_literal_text() {
        let applied = plugins(&[set_op("twins[port=2].name", "solo")]).unwrap();
        assert_eq!(
            applied.text,
            PLUGINS.replace(
                r#"{ "name": "dup", "port": 2 }"#,
                r#"{ "name": "solo", "port": 2 }"#
            )
        );
        assert!(matches!(
            &applied.touched[..],
            [(TransformOp::Set { key }, 11)] if key == "twins[1].name"
        ));
    }

    #[test]
    fn a_selector_resolves_for_delete_and_names_the_index() {
        let applied = plugins(&[delete_op("plugins[name=beta]")]).unwrap();
        assert!(!applied.text.contains("beta"), "{}", applied.text);
        assert!(applied.text.contains("alpha") && applied.text.contains("gitty"));
        assert!(matches!(
            &applied.touched[..],
            [(TransformOp::Delete { key }, 6)] if key == "plugins[1]"
        ));
    }

    #[test]
    fn a_selector_with_no_match_is_not_found_and_lists_the_values_seen() {
        let err = plugins(&[set_op("plugins[name=gone].version", "1")]).unwrap_err();
        assert_eq!(err.slug(), "not_found");
        assert_eq!(
            err.to_string(),
            "an element with name=\"gone\" (values seen: \"alpha\", \"beta\", \"gitty\") not \
             found in plugins[name=gone].version"
        );
    }

    #[test]
    fn a_selector_on_an_empty_array_says_it_has_no_elements() {
        let content = "{\n  \"plugins\": []\n}\n";
        let err = apply(&at("a.json"), content, &[set_op("plugins[name=x].v", "1")]).unwrap_err();
        assert_eq!(err.slug(), "not_found");
        assert!(
            err.to_string().contains("the array has no elements"),
            "{err}"
        );
    }

    #[test]
    fn a_selector_matching_two_elements_is_ambiguous_and_lists_both_indexes() {
        let err = plugins(&[set_op("twins[name=dup].port", "3")]).unwrap_err();
        assert_eq!(err.slug(), "ambiguous");
        let crate::Error::Ambiguous { target, candidates } = &err else {
            panic!("{err:?}");
        };
        assert_eq!(target, "twins[name=dup].port");
        let found: Vec<(usize, &str)> = candidates
            .iter()
            .map(|candidate| (candidate.line, candidate.text.as_str()))
            .collect();
        assert_eq!(found, [
            (10, "twins[0].name=\"dup\""),
            (11, "twins[1].name=\"dup\"")
        ]);
    }

    #[test]
    fn a_leading_byte_order_mark_is_parsed_past_and_kept() {
        let content = "\u{feff}{\n  \"a\": 1\n}\n";
        let applied = apply(&at("a.json"), content, &[set_op("a", "2")]).unwrap();
        assert_eq!(applied.text, "\u{feff}{\n  \"a\": 2\n}\n");
        assert_eq!(applied.touched[0].1, 2);
    }

    #[test]
    fn a_file_without_a_byte_order_mark_gains_none() {
        let content = "{\n  \"a\": 1\n}\n";
        let applied = apply(&at("a.json"), content, &[set_op("a", "2")]).unwrap();
        assert_eq!(applied.text, "{\n  \"a\": 2\n}\n");
    }

    #[test]
    fn an_appended_object_keeps_its_typed_key_order_and_number_text() {
        let content = "{\n  \"p\": []\n}\n";
        let applied = apply(&at("p.json"), content, &[append_op(
            "p",
            r#"{"version": 1.10, "name": "x"}"#,
        )])
        .unwrap();
        let typed = applied
            .text
            .find("\"version\": 1.10")
            .expect("typed text kept");
        let name = applied.text.find("\"name\"").expect("name written");
        assert!(typed < name, "{}", applied.text);
    }

    /// `top` is only in the root `plugins`, `a` only in group one: a wrong-array read finds them.
    const DEEP: &str = r#"{
  "plugins": [
    { "name": "top", "v": 5 }
  ],
  "outer": {
    "plugins": [
      { "name": "alpha", "v": 10 },
      { "name": "gitty", "v": 11 }
    ]
  },
  "groups": [
    { "name": "one", "plugins": [{ "name": "a", "v": 1 }, { "name": "b", "v": 2 }] },
    { "name": "two", "plugins": [{ "name": "b", "v": 3 }, { "name": "c", "v": 4 }] }
  ]
}
"#;

    fn deep(op: &Op) -> Result<Applied, crate::Error> {
        let applied = apply(&at("deep.json"), DEEP, std::slice::from_ref(op));
        assert_eq!(format!("{applied:?}"), by_cst(DEEP, op));
        applied
    }

    #[test]
    fn a_selector_under_a_multi_segment_prefix_resolves_in_that_array() {
        let applied = deep(&set_op("outer.plugins[name=gitty].v", "99")).unwrap();
        assert_eq!(
            applied.text,
            DEEP.replace(r#""name": "gitty", "v": 11"#, r#""name": "gitty", "v": 99"#)
        );
        assert!(matches!(
            &applied.touched[..],
            [(TransformOp::Set { key }, 8)] if key == "outer.plugins[1].v"
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
    fn a_selector_inside_an_element_of_another_array_resolves_in_that_element() {
        for (path, key) in [
            (
                "groups[name=two].plugins[name=b].v",
                "groups[1].plugins[0].v",
            ),
            ("groups[1].plugins[name=b].v", "groups[1].plugins[0].v"),
        ] {
            let applied = deep(&set_op(path, "99")).unwrap();
            assert_eq!(
                applied.text,
                DEEP.replace(r#"{ "name": "b", "v": 3 }"#, r#"{ "name": "b", "v": 99 }"#),
                "{path}"
            );
            assert!(
                matches!(
                    &applied.touched[..],
                    [(TransformOp::Set { key: written }, 13)] if written == key
                ),
                "{path}: {:?}",
                applied.touched
            );
        }
    }

    #[test]
    fn a_selector_inside_an_element_of_another_array_misses_a_value_only_a_sibling_holds() {
        let err = deep(&set_op("groups[name=two].plugins[name=a].v", "99")).unwrap_err();
        assert_eq!(err.slug(), "not_found");
        assert!(
            err.to_string().contains("(values seen: \"b\", \"c\")"),
            "{err}"
        );
    }

    const WALKED: &str = "{\n  // lead\n  \"version\": \"3.9\", /* mid */ build: 7,\n  \
                          \"esc\\\"aped\": \"a\\nb\",\n  \"nested\": {\"list\": [1, [2, 3], \
                          {\"k\": null}, {}, [],],},\n  \"plugins\": [\n    {\"name\": \"alpha\", \
                          \"v\": true},\n    {\"name\": \"gitty\", \"v\": false},\n    {\"name\": \
                          \"gitty\", \"v\": 1},\n  ],\n}\n";

    fn by_cst(content: &str, op: &Op) -> String {
        by_cst_with(false, content, op)
    }

    fn by_cst_with(bom: bool, content: &str, op: &Op) -> String {
        format!(
            "{:?}",
            through_cst(&at("w.json"), bom, content, std::slice::from_ref(op))
        )
    }

    fn by_splice(content: &str, op: &Op) -> Option<String> {
        by_splice_with(false, content, op)
    }

    fn by_splice_with(bom: bool, content: &str, op: &Op) -> Option<String> {
        match spliced_set(&at("w.json"), bom, content, op) {
            Ok(None) => None,
            other => Some(format!("{:?}", other.map(Option::unwrap))),
        }
    }

    const WALKED_SETS: [(&str, &str); 9] = [
        ("version", "3.10"),
        ("version", "null"),
        ("build", "8"),
        ("build", "text"),
        ("'esc\"aped'", "he said \"hi\"\tthen\\left"),
        ("nested.list[0]", "5"),
        ("nested.list[1][1]", "true"),
        ("nested.list[2].k", "\"x\""),
        ("plugins[name=alpha].v", "false"),
    ];

    #[test]
    fn a_spliced_set_writes_what_the_cst_writes() {
        for (path, value) in WALKED_SETS {
            let op = set_op(path, value);
            assert_eq!(
                by_splice(WALKED, &op),
                Some(by_cst(WALKED, &op)),
                "{path}={value}"
            );
        }
    }

    #[test]
    fn a_spliced_set_on_a_crlf_document_writes_what_the_cst_writes() {
        let crlf = WALKED.replace('\n', "\r\n");
        for (path, value) in WALKED_SETS {
            let op = set_op(path, value);
            let spliced = by_splice(&crlf, &op).expect("the walk reads a CRLF document");
            assert_eq!(spliced, by_cst(&crlf, &op), "{path}={value}");
        }
    }

    #[test]
    fn a_spliced_set_behind_a_byte_order_mark_writes_what_the_cst_writes() {
        for (path, value) in WALKED_SETS {
            let op = set_op(path, value);
            let spliced = by_splice_with(true, WALKED, &op).expect("the walk reads WALKED");
            assert_eq!(spliced, by_cst_with(true, WALKED, &op), "{path}={value}");
            let text = spliced_set(&at("w.json"), true, WALKED, &op)
                .unwrap()
                .unwrap()
                .text;
            let body = text.strip_prefix(BOM).expect("the mark is written back");
            assert!(body.starts_with('{'), "{text:?}");
        }
    }

    #[test]
    fn a_spliced_set_skips_comments_as_the_cst_does() {
        for (content, path, before, after) in [
            (
                "{\n  \"a\": /* c */ 1,\n  \"b\": 2\n}\n",
                "a",
                "*/ 1,",
                "*/ 7,",
            ),
            (
                "{\n  /* \"a\": 5 */\n  \"a\": 1\n}\n",
                "a",
                "\"a\": 1",
                "\"a\": 7",
            ),
            (
                "{\n  \"b\": {/* \"a\": 5 */ \"a\": 1}\n}\n",
                "b.a",
                "\"a\": 1}",
                "\"a\": 7}",
            ),
            (
                "{\n  // \"a\": 5\n  \"a\": 1\n}\n",
                "a",
                "\"a\": 1",
                "\"a\": 7",
            ),
        ] {
            let op = set_op(path, "7");
            let spliced = by_splice(content, &op).expect("the walk reads it");
            assert_eq!(spliced, by_cst(content, &op), "{content}");
            let text = spliced_set(&at("w.json"), false, content, &op)
                .unwrap()
                .unwrap()
                .text;
            assert_eq!(text, content.replace(before, after), "{content}");
        }
    }

    #[test]
    fn a_spliced_selector_fails_as_the_cst_fails() {
        for (path, variant) in [
            ("plugins[name=zeta].v", "Err(NotFound"),
            ("plugins[name=gitty].v", "Err(Ambiguous"),
            ("version[name=x].v", "Err(NotFound"),
        ] {
            let op = set_op(path, "1");
            let spliced = by_splice(WALKED, &op).expect("the walk reads WALKED");
            assert_eq!(spliced, by_cst(WALKED, &op), "{path}");
            assert!(spliced.starts_with(variant), "{spliced}");
        }
    }

    #[test]
    fn an_op_or_document_the_walk_does_not_read_is_left_to_the_cst() {
        for (content, path, value) in [
            (WALKED, "nested", "1"),
            (WALKED, "fresh", "1"),
            (WALKED, "nested.list[9]", "1"),
            (WALKED, "version", "{\"a\": 1}"),
            ("{\"a\": {\"b\": 1}, \"a\": {\"b\": 2}}", "a.b", "3"),
            ("{\"a\": 1 \"b\": 2}", "a", "3"),
            ("{1: 2, \"a\": 1}", "a", "3"),
            ("{\"a\": 1} {}", "a", "3"),
            ("{\"a\": }", "a", "3"),
            ("[1]", "a", "3"),
        ] {
            let op = set_op(path, value);
            assert_eq!(by_splice(content, &op), None, "{content} {path}");
            assert_eq!(
                format!(
                    "{:?}",
                    apply(&at("w.json"), content, std::slice::from_ref(&op))
                ),
                by_cst(content, &op),
                "{content} {path}"
            );
        }
    }
}
