//! Walks only displayed top-level statements: `program`/`list` children and a pipeline's last
//! stage. A substitution or an unrecognised node kind is never visited, which is allow.

use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use tree_sitter::{Node, Parser};

use super::{Verdict, bre};
use crate::grammars::{self, Language};

#[derive(Debug, PartialEq, Eq)]
enum Finding {
    Show(String),
    Find {
        flags: Vec<String>,
        pattern: String,
        paths: Vec<String>,
        translated: bool,
    },
    /// Always rendered with `--all`: its only source is `sed -i 's/…/…/g'`.
    Edit {
        paths: Vec<String>,
        old: String,
        new: String,
    },
    Write {
        path: String,
    },
}

/// Flags that consume the next argument, so their value is never read as a path.
const COUNT_FLAGS: &[&str] = &["-n", "-c", "--lines", "--bytes"];
/// Complete only because `find_flags` has already refused every other value-taking flag.
const FIND_VALUE_FLAGS: &[&str] = &[
    "-A",
    "-B",
    "-C",
    "--after-context",
    "--before-context",
    "--context",
];
const XARGS_FLAGS: &[&str] = &[
    "-n",
    "-I",
    "-P",
    "-d",
    "-s",
    "-E",
    "--max-args",
    "--replace",
    "--max-procs",
    "--delimiter",
];

pub fn classify_command(command: &str, cwd: &str) -> Verdict {
    let cwd = normalize(Path::new(cwd));
    // A relative cwd would make every path look in-tree.
    if !cwd.is_absolute() {
        return Verdict::Allow;
    }
    let mut parser = Parser::new();
    if parser
        .set_language(grammars::language(Language::Bash))
        .is_err()
    {
        return Verdict::Allow;
    }
    let Some(tree) = parser.parse(command, None) else {
        return Verdict::Allow;
    };
    let root = tree.root_node();
    if root.has_error() || joins_lines(root, command) {
        return Verdict::Allow;
    }
    let top = top_level_statements(root);
    let mut changes = Vec::new();
    directory_changes(root, command, &mut changes);
    let (base, cd, statements) = match changes.as_slice() {
        [] => (
            cwd.clone(),
            None,
            top.into_iter().map(Statement::displayed).collect(),
        ),
        [only] => match leading_cd(*only, &top, command, &cwd) {
            Some(start) => start,
            None => return Verdict::Allow,
        },
        _ => return Verdict::Allow,
    };
    let dirs = Dirs {
        root: &cwd,
        base: &base,
    };

    let mut findings = Vec::new();
    for Statement { node, redirected } in statements {
        match redirected {
            Some(owner) => {
                classify_redirected(owner, Some(node), command, dirs, None, &mut findings);
            },
            None => classify_displayed(node, command, dirs, None, &mut findings),
        }
    }
    let (mut notes, prefix) = match cd {
        Some(operand) => (
            vec![format!("`cd {operand}` kept")],
            format!("cd {operand} && "),
        ),
        None => (Vec::new(), String::new()),
    };
    if findings.iter().any(|finding| {
        matches!(finding, Finding::Find {
            translated: true,
            ..
        })
    }) {
        notes.push("grep pattern translated to lets regex".to_owned());
    }
    match reason(&findings, &notes, &prefix) {
        Some(reason) => Verdict::Block { reason },
        None => Verdict::Allow,
    }
}

/// `base` is where operands resolve, after a leading `cd`; `root`, the event's cwd, bounds them.
#[derive(Clone, Copy)]
struct Dirs<'a> {
    root: &'a Path,
    base: &'a Path,
}

/// `redirected` is the list tree-sitter-bash hangs the redirects on in
/// `cd src && cat > f <<'EOF'`, though bash binds them to `cat`.
struct Statement<'t> {
    node: Node<'t>,
    redirected: Option<Node<'t>>,
}

impl<'t> Statement<'t> {
    fn displayed(node: Node<'t>) -> Statement<'t> {
        Statement {
            node,
            redirected: None,
        }
    }
}

fn leading_cd<'t>(
    cd: Node<'t>,
    top: &[Node<'t>],
    src: &str,
    cwd: &Path,
) -> Option<(PathBuf, Option<String>, Vec<Statement<'t>>)> {
    let (first, later) = top.split_first()?;
    let mut statements = Vec::new();
    if *first != cd {
        if first.kind() != "redirected_statement" {
            return None;
        }
        let body = first.child_by_field_name("body")?;
        if body.kind() != "list" {
            return None;
        }
        let flattened = top_level_statements(body);
        let (head, rest) = flattened.split_first()?;
        let (last, middle) = rest.split_last()?;
        if *head != cd {
            return None;
        }
        statements.extend(middle.iter().copied().map(Statement::displayed));
        statements.push(Statement {
            node: *last,
            redirected: Some(*first),
        });
    }
    statements.extend(later.iter().copied().map(Statement::displayed));

    if let Some(next) = statements.first() {
        let gap = src.get(cd.end_byte()..next.node.start_byte())?;
        let operator = gap.trim();
        if !(operator == "&&" || operator == ";" || operator.is_empty() && gap.contains('\n')) {
            return None;
        }
    }
    if cd.child_by_field_name("name").and_then(|n| text(n, src)) != Some("cd") {
        return None;
    }
    let mut cursor = cd.walk();
    let arguments: Vec<Node> = cd.children_by_field_name("argument", &mut cursor).collect();
    let [argument] = arguments.as_slice() else {
        return None;
    };
    let operand = literal(*argument, src)?;
    if operand.glob || operand.text.is_empty() || operand.text.starts_with(['-', '~']) {
        return None;
    }
    let base = normalize(&cwd.join(&operand.text));
    if !base.starts_with(cwd) {
        return None;
    }
    Some((base, Some(shell_quote(&operand.text)), statements))
}

/// Searches substitutions and compound bodies too: a `cd` there still moves later statements.
fn directory_changes<'t>(node: Node<'t>, src: &str, found: &mut Vec<Node<'t>>) {
    if node.kind() == "command"
        && matches!(
            node.child_by_field_name("name").and_then(|n| text(n, src)),
            Some("cd" | "pushd" | "popd")
        )
    {
        found.push(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        directory_changes(child, src, found);
    }
}

/// tree-sitter-bash 0.25.1 joins a line onto a 3+ stage pipeline with no error node:
/// `ls | sort | head -4` then `echo x 2>/dev/null` parses as `head -4 echo x`.
fn joins_lines(node: Node, src: &str) -> bool {
    let command = node.kind() == "command";
    let mut cursor = node.walk();
    let mut previous_end = None;
    for child in node.children(&mut cursor) {
        if let Some(end) = previous_end
            && src.get(end..child.start_byte()).is_none_or(|gap| {
                gap.match_indices('\n')
                    .any(|(at, _)| !gap[..at].ends_with('\\'))
            })
        {
            return true;
        }
        if joins_lines(child, src) {
            return true;
        }
        previous_end = command.then(|| child.end_byte());
    }
    false
}

fn top_level_statements(node: Node) -> Vec<Node> {
    match node.kind() {
        "program" | "list" => {
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .filter(Node::is_named)
                .flat_map(top_level_statements)
                .collect()
        },
        _ => vec![node],
    }
}

fn classify_displayed(
    node: Node,
    src: &str,
    dirs: Dirs,
    producer: Option<&str>,
    findings: &mut Vec<Finding>,
) {
    match node.kind() {
        "pipeline" => {
            let mut cursor = node.walk();
            let stages: Vec<Node> = node.children(&mut cursor).filter(Node::is_named).collect();
            if let Some(finding) = cat_head_pipeline(&stages, src, dirs) {
                findings.push(finding);
                return;
            }
            let Some((displayed, upstream)) = stages.split_last() else {
                return;
            };
            let upstream = upstream
                .iter()
                .filter_map(|stage| text(*stage, src))
                .collect::<Vec<_>>()
                .join(" | ");
            classify_displayed(*displayed, src, dirs, Some(&upstream), findings);
        },
        "redirected_statement" => classify_redirected(
            node,
            node.child_by_field_name("body"),
            src,
            dirs,
            producer,
            findings,
        ),
        "command" => classify_simple(node, src, dirs, producer, findings),
        _ => {},
    }
}

/// `body` differs from `node`'s own body only in the list shape `Statement` describes.
fn classify_redirected(
    node: Node,
    body: Option<Node>,
    src: &str,
    dirs: Dirs,
    producer: Option<&str>,
    findings: &mut Vec<Finding>,
) {
    let redirects = collect_redirects(node);
    let heredoc = redirects
        .iter()
        .any(|redirect| redirect.kind() == "heredoc_redirect");
    let write = redirects
        .iter()
        .find_map(|redirect| stdout_write(*redirect, src));

    if let Some((destination, operator)) = write {
        // Nothing is displayed; only a bare `cat` fed by a heredoc maps to `lets write`.
        if !destination.glob && in_tree(dirs, &destination.text).is_some() {
            let replaceable = heredoc
                && matches!(operator, ">" | ">|" | "&>")
                && body.is_some_and(|body| is_bare_cat(body, src));
            if replaceable {
                findings.push(Finding::Write {
                    path: shell_quote(&destination.text),
                });
            }
        }
        return;
    }

    // Heredoc-to-stdin is exempt outright, before any command name is looked at.
    if heredoc {
        return;
    }
    if redirects
        .iter()
        .any(|redirect| hides_stdout(*redirect, src))
    {
        return;
    }
    if let Some(body) = body {
        classify_displayed(body, src, dirs, producer, findings);
    }
}

/// `cat <<'EOF' > f` nests the file redirect inside the heredoc node, unlike `cat > f <<'EOF'`.
fn collect_redirects(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    let mut redirects: Vec<Node> = node
        .children_by_field_name("redirect", &mut cursor)
        .collect();
    let nested: Vec<Node> = redirects
        .iter()
        .filter(|redirect| redirect.kind() == "heredoc_redirect")
        .flat_map(|redirect| {
            let mut inner = redirect.walk();
            redirect
                .children_by_field_name("redirect", &mut inner)
                .collect::<Vec<_>>()
        })
        .collect();
    redirects.extend(nested);
    redirects
}

fn is_bare_cat(body: Node, src: &str) -> bool {
    if body.kind() != "command" {
        return false;
    }
    if body.child_by_field_name("name").and_then(|n| text(n, src)) != Some("cat") {
        return false;
    }
    let mut cursor = body.walk();
    body.children_by_field_name("argument", &mut cursor)
        .next()
        .is_none()
}

/// `file_redirect` also covers `<` and `>&`, so the operator, an unnamed child, decides.
/// tree-sitter-bash 0.25.1 lexes each operator as one token.
fn stdout_write<'a>(redirect: Node, src: &'a str) -> Option<(Word, &'a str)> {
    if redirect.kind() != "file_redirect" {
        return None;
    }
    // The both-stream operators carry no `descriptor` node.
    let descriptor = redirect
        .child_by_field_name("descriptor")
        .and_then(|descriptor| text(descriptor, src));
    if descriptor.is_some_and(|descriptor| descriptor != "1") {
        return None;
    }
    let mut cursor = redirect.walk();
    let operator = redirect.children(&mut cursor).find_map(|child| {
        (!child.is_named())
            .then(|| text(child, src))
            .flatten()
            .filter(|token| matches!(*token, ">" | ">>" | "&>" | "&>>" | ">|"))
    })?;
    let destination = literal(redirect.child_by_field_name("destination")?, src)?;
    Some((destination, operator))
}

/// `>&file` and `>&-` take stdout off the transcript; `>&2` still reaches the tool result.
fn hides_stdout(redirect: Node, src: &str) -> bool {
    if redirect.kind() != "file_redirect" {
        return false;
    }
    let mut cursor = redirect.walk();
    let duplicates = redirect
        .children(&mut cursor)
        .any(|child| !child.is_named() && matches!(text(child, src), Some(">&" | ">&-")));
    if !duplicates {
        return false;
    }
    redirect
        .child_by_field_name("destination")
        .and_then(|destination| text(destination, src))
        .is_none_or(|target| !target.bytes().all(|byte| byte.is_ascii_digit()))
}

fn classify_simple(
    node: Node,
    src: &str,
    dirs: Dirs,
    producer: Option<&str>,
    findings: &mut Vec<Finding>,
) {
    let Some(head) = node.child_by_field_name("name").and_then(|n| text(n, src)) else {
        return;
    };
    let mut cursor = node.walk();
    let Some(words) = node
        .children_by_field_name("argument", &mut cursor)
        .map(|argument| literal(argument, src))
        .collect::<Option<Vec<Word>>>()
    else {
        return;
    };
    let arguments = texts(&words);

    match head {
        "cat" => classify_cat(&words, &arguments, dirs, findings),
        "head" | "tail" => {
            // `show` has no byte or follow mode.
            if arguments
                .iter()
                .any(|a| a.starts_with("-c") || a.starts_with("--bytes"))
            {
                return;
            }
            if head == "tail"
                && arguments
                    .iter()
                    .any(|a| matches!(*a, "-f" | "-F" | "--follow" | "--retry"))
            {
                return;
            }
            let count = line_count(&arguments);
            // A tail range needs the file's length, which this classifier never reads.
            if head == "tail" && count.is_some() {
                return;
            }
            let suffix = match count {
                None => String::new(),
                // `head -n -5` (all but the last 5) has no range form.
                Some(value) => {
                    let Some(lines) = number(value).filter(|lines| *lines > 0) else {
                        return;
                    };
                    format!(":1-{lines}")
                },
            };
            let Some(operands) = operands(&words, COUNT_FLAGS) else {
                return;
            };
            show(&operands, &suffix, dirs, findings);
        },
        "sed" => classify_sed(&words, &arguments, dirs, findings),
        "grep" | "rg" => classify_find(head, &words, &arguments, dirs, producer, findings),
        "xargs" => {
            let Some(operands) = operands(&words, XARGS_FLAGS) else {
                return;
            };
            if operands.first().map(|operand| operand.text.as_str()) != Some("cat") {
                return;
            }
            let Some(producer) = producer else {
                return;
            };
            findings.push(Finding::Show(format!("$({producer})")));
        },
        _ => {},
    }
}

fn classify_find(
    head: &str,
    words: &[Word],
    arguments: &[&str],
    dirs: Dirs,
    producer: Option<&str>,
    findings: &mut Vec<Finding>,
) {
    let Some(Search {
        flags,
        syntax,
        recursive,
    }) = find_flags(arguments, head)
    else {
        return;
    };
    let Some(operands) = operands(words, FIND_VALUE_FLAGS) else {
        return;
    };
    let Some((pattern, candidates)) = operands.split_first() else {
        return;
    };
    // A pathless `grep`, and an `rg` after a pipe, read stdin rather than the tree.
    if candidates.is_empty() && (head == "grep" || producer.is_some()) {
        return;
    }
    if !candidates.is_empty()
        && !candidates
            .iter()
            .any(|candidate| in_tree(dirs, &candidate.text).is_some())
    {
        return;
    }
    // `lets find` takes its paths as plain paths, never through the target grammar.
    let Some(paths) = candidates
        .iter()
        .map(|path| path_argument(path, "", None))
        .collect::<Option<Vec<String>>>()
    else {
        return;
    };
    // Without `-r`, grep errors on a directory `lets find` would walk, and on a missing file.
    if head == "grep"
        && !recursive
        && !candidates
            .iter()
            .all(|candidate| names_only_files(candidate, dirs.base))
    {
        return;
    }
    let written = &pattern.text;
    let translated = match syntax {
        Syntax::Basic => bre::translate(written),
        Syntax::Extended => bre::translate_extended(written),
        Syntax::Native => Some(written.clone()),
    };
    let Some(pattern) = translated else {
        return;
    };
    findings.push(Finding::Find {
        flags,
        translated: pattern != *written,
        pattern,
        paths,
    });
}

fn classify_cat(words: &[Word], arguments: &[&str], dirs: Dirs, findings: &mut Vec<Finding>) {
    if !cat_flags_are_display_neutral(arguments) {
        return;
    }
    let Some(operands) = operands(words, &[]) else {
        return;
    };
    show(&operands, "", dirs, findings);
}

fn cat_flags_are_display_neutral(arguments: &[&str]) -> bool {
    for &argument in arguments {
        if !argument.starts_with('-') || argument == "-" || argument == "--" {
            continue;
        }
        if let Some(long) = argument.strip_prefix("--") {
            let name = long.split('=').next().unwrap_or(long);
            if !matches!(name, "number" | "number-nonblank") {
                return false;
            }
            continue;
        }
        if argument[1..].contains(|c| !matches!(c, 'n' | 'b' | 'u')) {
            return false;
        }
    }
    true
}

/// `cat f | head -N` as `f:1-N`; any other shape falls through to the last-stage rule.
fn cat_head_pipeline(stages: &[Node], src: &str, dirs: Dirs) -> Option<Finding> {
    let [cat_stage, head_stage] = stages else {
        return None;
    };
    if cat_stage.kind() != "command" || head_stage.kind() != "command" {
        return None;
    }
    if cat_stage
        .child_by_field_name("name")
        .and_then(|n| text(n, src))
        != Some("cat")
    {
        return None;
    }
    let mut cursor = cat_stage.walk();
    let cat_words = cat_stage
        .children_by_field_name("argument", &mut cursor)
        .map(|argument| literal(argument, src))
        .collect::<Option<Vec<Word>>>()?;
    if !cat_flags_are_display_neutral(&texts(&cat_words)) {
        return None;
    }
    let operands = operands(&cat_words, &[])?;
    let [path] = operands.as_slice() else {
        return None;
    };
    in_tree(dirs, &path.text)?;

    if head_stage
        .child_by_field_name("name")
        .and_then(|n| text(n, src))
        != Some("head")
    {
        return None;
    }
    let mut cursor = head_stage.walk();
    let head_words = head_stage
        .children_by_field_name("argument", &mut cursor)
        .map(|argument| literal(argument, src))
        .collect::<Option<Vec<Word>>>()?;
    let lines = head_sole_count(&texts(&head_words))?;

    path_argument(path, &format!(":1-{lines}"), Some(dirs.base)).map(Finding::Show)
}

fn head_sole_count(arguments: &[&str]) -> Option<u64> {
    let lines = match arguments {
        [flag, value] if *flag == "-n" || *flag == "--lines" => number(value)?,
        [only] => match only
            .strip_prefix("--lines=")
            .or_else(|| only.strip_prefix("-n"))
        {
            Some(value) => number(value)?,
            None => number(only.strip_prefix('-')?)?,
        },
        _ => return None,
    };
    (lines > 0).then_some(lines)
}

fn classify_sed(words: &[Word], arguments: &[&str], dirs: Dirs, findings: &mut Vec<Finding>) {
    if arguments.iter().any(|a| is_in_place_flag(a)) {
        // `-i.bak`, or any flag beside `-i`, has no single `lets edit` meaning.
        let bare = arguments.iter().any(|&a| a == "-i" || a == "--in-place")
            && arguments
                .iter()
                .all(|&a| !a.starts_with('-') || a == "-i" || a == "--in-place");
        if bare {
            classify_sed_i(words, dirs, findings);
        }
        return;
    }
    classify_sed_n(words, arguments, dirs, findings);
}

fn is_in_place_flag(argument: &str) -> bool {
    argument == "-i"
        || argument.starts_with("-i")
        || argument == "--in-place"
        || argument.starts_with("--in-place=")
}

fn classify_sed_i(words: &[Word], dirs: Dirs, findings: &mut Vec<Finding>) {
    let Some(operands) = operands(words, &[]) else {
        return;
    };
    let [script, paths @ ..] = operands.as_slice() else {
        return;
    };
    if paths.is_empty() || script.glob {
        return;
    }
    if paths
        .iter()
        .any(|path| path.glob || in_tree(dirs, &path.text).is_none())
    {
        return;
    }
    // sed edits a repeated operand once per mention where `lets edit` makes one pass, and it
    // refuses a directory.
    let mut resolved: Vec<PathBuf> = paths
        .iter()
        .map(|path| normalize(&dirs.base.join(&path.text)))
        .collect();
    if resolved.iter().any(|path| path.is_dir()) {
        return;
    }
    resolved.sort();
    if resolved.windows(2).any(|pair| pair[0] == pair[1]) {
        return;
    }
    let Some((old, new)) = literal_substitution(&script.text, "g") else {
        return;
    };
    let Some(paths) = paths
        .iter()
        .map(|path| path_argument(path, "", Some(dirs.base)))
        .collect::<Option<Vec<String>>>()
    else {
        return;
    };
    findings.push(Finding::Edit { paths, old, new });
}

/// One file only: sed numbers lines across the concatenated stream of every file.
fn classify_sed_n(words: &[Word], arguments: &[&str], dirs: Dirs, findings: &mut Vec<Finding>) {
    let mut quiet = false;
    let mut extended = false;
    for &argument in arguments {
        if argument == "--" {
            break;
        }
        let Some(flag) = argument.strip_prefix('-').filter(|flag| !flag.is_empty()) else {
            continue;
        };
        match flag {
            "-quiet" | "-silent" => quiet = true,
            "-regexp-extended" => extended = true,
            short if short.chars().all(|c| matches!(c, 'n' | 'E' | 'r')) => {
                quiet |= short.contains('n');
                extended |= short.contains(['E', 'r']);
            },
            _ => return,
        }
    }
    if !quiet {
        // Without `-n`, sed prints a transform, not a display.
        return;
    }
    let Some(operands) = operands(words, &[]) else {
        return;
    };
    let [script, path] = operands.as_slice() else {
        return;
    };
    if script.glob || in_tree(dirs, &path.text).is_none() {
        return;
    }

    let symbol = if extended {
        None
    } else {
        sed_symbol_target(&script.text, &path.text)
    };
    let suffix = match symbol {
        Some(name) => format!("#{name}"),
        None => match print_line(&script.text).or_else(|| print_range(&script.text)) {
            Some(range) => format!(":{range}"),
            None => return,
        },
    };
    if let Some(target) = path_argument(path, &suffix, Some(dirs.base)) {
        findings.push(Finding::Show(target));
    }
}

/// `None` when an operand is `-`, stdin rather than a path.
fn operands<'w>(words: &'w [Word], value_flags: &[&str]) -> Option<Vec<&'w Word>> {
    let mut operands = Vec::new();
    let mut skip = false;
    let mut flags_ended = false;
    for word in words {
        let argument = &word.text;
        if skip {
            skip = false;
            continue;
        }
        if !flags_ended {
            if argument == "--" {
                flags_ended = true;
                continue;
            }
            if argument == "-" {
                return None;
            }
            if argument.starts_with('-') {
                skip = value_flags.contains(&argument.as_str());
                continue;
            }
        }
        operands.push(word);
    }
    Some(operands)
}

/// Names out-of-tree operands too: a replacement reading fewer files would hide an omission.
fn show(operands: &[&Word], suffix: &str, dirs: Dirs, findings: &mut Vec<Finding>) {
    if !operands
        .iter()
        .any(|operand| in_tree(dirs, &operand.text).is_some())
    {
        return;
    }
    let Some(targets) = operands
        .iter()
        .map(|operand| path_argument(operand, suffix, Some(dirs.base)))
        .collect::<Option<Vec<String>>>()
    else {
        return;
    };
    findings.extend(targets.into_iter().map(Finding::Show));
}

/// A glob stays unquoted to expand as the original did, but only a portable one with no suffix:
/// a range would bind to the last file. `target` is set when `lets` parses `word` as a target.
fn path_argument(word: &Word, suffix: &str, target: Option<&Path>) -> Option<String> {
    if word.glob {
        let plain = suffix.is_empty()
            && word
                .text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_./*?-".contains(&b));
        return plain.then(|| word.text.clone());
    }
    if let Some(base) = target
        && !names_itself(&word.text, suffix, base)
    {
        return None;
    }
    if suffix.is_empty() {
        return Some(shell_quote(&word.text));
    }
    Some(shell_quote(&format!("{}{suffix}", word.text)))
}

/// An existing file wins the target parse. After a `#`, `@` or `:` in the path, a suffix relies
/// on the longest-prefix rule, which one stat cannot confirm.
fn names_itself(path: &str, suffix: &str, base: &Path) -> bool {
    let is_file = |name: &str| base.join(name).is_file();
    if path.contains(['#', '@', ':']) {
        return suffix.is_empty() && is_file(path);
    }
    suffix.is_empty() || !is_file(&format!("{path}{suffix}"))
}

fn texts(words: &[Word]) -> Vec<&str> {
    words.iter().map(|word| word.text.as_str()).collect()
}

fn line_count<'a>(arguments: &[&'a str]) -> Option<&'a str> {
    let mut arguments = arguments.iter();
    while let Some(&argument) = arguments.next() {
        let value = if argument == "-n" || argument == "--lines" {
            arguments.next().copied().unwrap_or("")
        } else if let Some(value) = argument.strip_prefix("--lines=") {
            value
        } else if let Some(value) = argument.strip_prefix("-n") {
            value
        } else if argument.len() > 1
            && argument.starts_with('-')
            && argument[1..].bytes().all(|byte| byte.is_ascii_digit())
        {
            // The obsolete `head -20` form.
            &argument[1..]
        } else {
            continue;
        };
        return Some(value);
    }
    None
}

/// Digits only: `str::parse` alone would accept `+5`.
fn number(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn print_line(script: &str) -> Option<String> {
    let line = number(script.strip_suffix('p')?)?;
    (line >= 1).then(|| line.to_string())
}

fn print_range(script: &str) -> Option<String> {
    let (start, end) = script.strip_suffix('p')?.split_once(',')?;
    let start = number(start)?;
    let end = number(end)?;
    (start >= 1 && end >= start).then(|| format!("{start}-{end}"))
}

static SYMBOL_IDIOM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^/\^(?:func|def|fn|function|class) ([A-Za-z_][A-Za-z0-9_]*)(?:[( <:{]|\[[( <:{]+\])/,/\^\}/p$",
    )
    .expect("a fixed pattern compiles")
});

/// Brace languages only: a Python `def` never closes on `/^}/`. The name must end, since
/// `/^func Foo/` also opens a range at `func FooBar` and sed prints every range it opens.
fn sed_symbol_target(script: &str, path: &str) -> Option<String> {
    let name = SYMBOL_IDIOM.captures(script)?.get(1)?.as_str().to_owned();
    let extension = Path::new(path).extension()?.to_str()?;
    let language = grammars::from_extension(extension)?;
    matches!(
        language,
        Language::Go | Language::Rust | Language::JavaScript | Language::TypeScript | Language::Tsx
    )
    .then_some(name)
}

/// `lets edit` matches `--old` as bytes, so a regex metacharacter in OLD, or `&`, `\` or a
/// newline in NEW, has no exact translation.
fn literal_substitution(script: &str, expected_flags: &str) -> Option<(String, String)> {
    let mut characters = script.chars();
    if characters.next()? != 's' {
        return None;
    }
    let delimiter = characters.next()?;
    if delimiter.is_alphanumeric() || delimiter.is_whitespace() || delimiter == '\\' {
        return None;
    }
    let parts: Vec<&str> = script[1 + delimiter.len_utf8()..]
        .split(delimiter)
        .collect();
    let [old, new, flags] = parts.as_slice() else {
        return None;
    };
    if *flags != expected_flags || old.is_empty() {
        return None;
    }
    // `+` and `?` are BRE literals, but an agent typing them usually means the ERE quantifier.
    if old.contains(['.', '*', '[', ']', '^', '$', '\\', '&', '+', '?'])
        || new.contains(['\\', '&', '\n'])
    {
        return None;
    }
    Some(((*old).to_owned(), (*new).to_owned()))
}

/// A glob expands in its last component only, and every match must be a regular file.
fn names_only_files(operand: &Word, base: &Path) -> bool {
    if !operand.glob {
        return base.join(&operand.text).is_file();
    }
    let path = Path::new(&operand.text);
    let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
    else {
        return false;
    };
    // A `.`-led glob matches `.` and `..` in bash before 5.2, which `read_dir` never lists.
    if name.starts_with('.') || parent.to_str().is_none_or(|p| p.contains(GLOB_BYTES)) {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(base.join(parent)) else {
        return false;
    };
    let mut matched = false;
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        let Some(entry_name) = entry.file_name().to_str().map(str::to_owned) else {
            return false;
        };
        if entry_name.starts_with('.') || !wildcard_matches(name, &entry_name) {
            continue;
        }
        if !entry.path().is_file() {
            return false;
        }
        matched = true;
    }
    matched
}

fn wildcard_matches(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0, 0);
    let mut resume: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                resume = Some((p, n));
                p += 1;
            },
            Some(&c) if c == '?' || c == name[n] => {
                p += 1;
                n += 1;
            },
            _ => match resume {
                Some((star, from)) => {
                    resume = Some((star, from + 1));
                    p = star + 1;
                    n = from + 1;
                },
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

/// `Native` is `-F`, or `rg`, which compiles the same `regex` crate syntax as `lets find`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Syntax {
    Basic,
    Extended,
    Native,
}

struct Search {
    flags: Vec<String>,
    syntax: Syntax,
    recursive: bool,
}

/// `None` for any flag `lets find` lacks: dropping `-v` or `-o` would search for something else.
fn find_flags(arguments: &[&str], head: &str) -> Option<Search> {
    let mut dialect = None;
    let mut recursive = false;
    let mut ignore_case = false;
    let mut word = false;
    let mut files = false;
    let mut count = false;
    let mut contexts: Vec<String> = Vec::new();
    let mut arguments = arguments.iter();

    while let Some(&argument) = arguments.next() {
        if argument == "--" {
            break;
        }
        let Some(flag) = argument.strip_prefix('-').filter(|flag| !flag.is_empty()) else {
            continue;
        };
        if let Some(long) = flag.strip_prefix('-') {
            let (name, attached) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (long, None),
            };
            match name {
                "fixed-strings" => choose(&mut dialect, 'F')?,
                "extended-regexp" => choose(&mut dialect, 'E')?,
                "basic-regexp" if head == "grep" => choose(&mut dialect, 'G')?,
                "ignore-case" => ignore_case = true,
                "word-regexp" => word = true,
                "files-with-matches" => files = true,
                "count" => count = true,
                "context" | "after-context" | "before-context" => contexts.push(match attached {
                    Some(value) => value.to_owned(),
                    None => (*arguments.next()?).to_owned(),
                }),
                "recursive" if head == "grep" => recursive = true,
                "line-number" | "with-filename" | "no-messages" | "color" | "colour" => {},
                _ => return None,
            }
            continue;
        }
        let mut characters = flag.chars();
        while let Some(character) = characters.next() {
            match character {
                'F' => choose(&mut dialect, 'F')?,
                'i' => ignore_case = true,
                'w' => word = true,
                'l' => files = true,
                'c' => count = true,
                'n' | 'H' | 's' => {},
                // `rg -r` is `--replace` and `rg -E` is `--encoding`.
                'E' | 'G' if head == "grep" => choose(&mut dialect, character)?,
                'r' | 'R' if head == "grep" => recursive = true,
                'A' | 'B' | 'C' => {
                    let attached: String = characters.by_ref().collect();
                    contexts.push(if attached.is_empty() {
                        (*arguments.next()?).to_owned()
                    } else {
                        attached
                    });
                },
                _ => return None,
            }
        }
    }

    let context = match contexts.first() {
        None => None,
        // An asymmetric `-A 3 -B 1` has no `-C n` form.
        Some(first) if contexts.iter().any(|value| value != first) => return None,
        Some(first) => Some(number(first)?),
    };

    let mut flags = Vec::new();
    if dialect == Some('F') {
        flags.push("-F".to_owned());
    }
    if ignore_case {
        flags.push("-i".to_owned());
    }
    if word {
        flags.push("-w".to_owned());
    }
    if let Some(context) = context {
        flags.push(format!("-C {context}"));
    }
    if files {
        flags.push("--files".to_owned());
    }
    if count {
        flags.push("--count".to_owned());
    }
    let syntax = match dialect {
        _ if head != "grep" => Syntax::Native,
        None | Some('G') => Syntax::Basic,
        Some('E') => Syntax::Extended,
        Some(_) => Syntax::Native,
    };
    Some(Search {
        flags,
        syntax,
        recursive,
    })
}

/// grep exits 2 on two different dialect flags ("conflicting matchers specified", GNU grep 3.7).
fn choose(dialect: &mut Option<char>, flag: char) -> Option<()> {
    if dialect.is_some_and(|chosen| chosen != flag) {
        return None;
    }
    *dialect = Some(flag);
    Some(())
}

const SHOW_CLAUSE: &str = "lets show reads several files and ranges in one call";
const FIND_CLAUSE: &str = "lets find returns every hit numbered and grouped by file";
const EDIT_CLAUSE: &str = "lets edit replaces the exact text and shows the changed lines";
const WRITE_CLAUSE: &str = "lets write takes the same heredoc on stdin";

fn reason(findings: &[Finding], notes: &[String], prefix: &str) -> Option<String> {
    if findings.is_empty() {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();
    let mut show: Vec<&str> = Vec::new();
    let mut show_line = None;
    let mut clauses: Vec<&str> = Vec::new();

    for finding in findings {
        match finding {
            Finding::Show(argument) => {
                if !show.contains(&argument.as_str()) {
                    show.push(argument);
                }
                if show_line.is_none() {
                    show_line = Some(lines.len());
                    lines.push(String::new());
                }
                add_clause(&mut clauses, SHOW_CLAUSE);
            },
            Finding::Find {
                flags,
                pattern,
                paths,
                ..
            } => {
                let mut line = String::from("lets find");
                for flag in flags {
                    line.push(' ');
                    line.push_str(flag);
                }
                line.push(' ');
                line.push_str(&quote(pattern));
                for path in paths {
                    line.push(' ');
                    line.push_str(path);
                }
                lines.push(line);
                add_clause(&mut clauses, FIND_CLAUSE);
            },
            Finding::Edit { paths, old, new } => {
                lines.push(format!(
                    "lets edit {} --old {} --new {} --all",
                    paths.join(" "),
                    quote(old),
                    quote(new)
                ));
                add_clause(&mut clauses, EDIT_CLAUSE);
            },
            Finding::Write { path } => {
                lines.push(format!("lets write --force {path}"));
                add_clause(&mut clauses, WRITE_CLAUSE);
            },
        }
    }
    if let Some(at) = show_line {
        lines[at] = format!("lets show {}", show.join(" "));
    }

    let mut reason: String = notes
        .iter()
        .map(String::as_str)
        .chain(clauses)
        .collect::<Vec<_>>()
        .join("; ");
    reason.push('.');
    for line in &lines {
        reason.push_str("\nrun: ");
        reason.push_str(prefix);
        reason.push_str(line);
    }
    Some(reason)
}

fn add_clause<'a>(clauses: &mut Vec<&'a str>, clause: &'a str) {
    if !clauses.contains(&clause) {
        clauses.push(clause);
    }
}

fn quote(pattern: &str) -> String {
    format!("'{}'", pattern.replace('\'', r"'\''"))
}

fn shell_quote(token: &str) -> String {
    let bare = !token.is_empty()
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/=+:@,".contains(&b));
    if bare { token.to_owned() } else { quote(token) }
}

fn text<'a>(node: Node, src: &'a str) -> Option<&'a str> {
    node.utf8_text(src.as_bytes()).ok()
}

/// `text` is what the command receives; `glob` is set when an unquoted part of it expands.
#[derive(Debug, Clone)]
struct Word {
    text: String,
    glob: bool,
}

const GLOB_BYTES: [char; 4] = ['*', '?', '[', '{'];

/// `None` for anything whose value depends on the shell's state: an expansion, a substitution.
fn literal(node: Node, src: &str) -> Option<Word> {
    let quoted = |text: &str| Word {
        text: text.to_owned(),
        glob: false,
    };
    match node.kind() {
        "word" => text(node, src).and_then(unquoted_word),
        "number" => text(node, src).map(quoted),
        "raw_string" => text(node, src).map(|t| quoted(unquote(t, '\''))),
        "string" => {
            let mut cursor = node.walk();
            if node
                .children(&mut cursor)
                .filter(Node::is_named)
                .any(|child| child.kind() != "string_content")
            {
                return None;
            }
            text(node, src).map(|t| Word {
                text: double_quoted(unquote(t, '"')),
                glob: false,
            })
        },
        "concatenation" => {
            let mut cursor = node.walk();
            let parts = node
                .children(&mut cursor)
                .filter(Node::is_named)
                .map(|child| literal(child, src))
                .collect::<Option<Vec<Word>>>()?;
            let glob = parts.iter().any(|part| part.glob);
            // Joined, a quoted `*` beside an unquoted one reads as a second wildcard.
            if glob
                && parts
                    .iter()
                    .any(|part| !part.glob && part.text.contains(GLOB_BYTES))
            {
                return None;
            }
            Some(Word {
                text: parts.into_iter().map(|part| part.text).collect(),
                glob,
            })
        },
        _ => None,
    }
}

/// Bash drops the backslash before any character and joins a backslash-newline. `None` for a
/// trailing lone backslash, or an escaped glob byte beside a live one, which `Word` cannot spell.
fn unquoted_word(raw: &str) -> Option<Word> {
    let mut text = String::with_capacity(raw.len());
    let mut glob = false;
    let mut escaped_glob = false;
    let mut characters = raw.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            glob |= GLOB_BYTES.contains(&character);
            text.push(character);
            continue;
        }
        match characters.next()? {
            '\n' => {},
            escaped => {
                escaped_glob |= GLOB_BYTES.contains(&escaped);
                text.push(escaped);
            },
        }
    }
    (!(glob && escaped_glob)).then_some(Word { text, glob })
}

/// Inside double quotes the backslash goes only before `\`, `"`, `$`, a backtick or a newline.
fn double_quoted(raw: &str) -> String {
    let mut text = String::with_capacity(raw.len());
    let mut characters = raw.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\\' {
            text.push(character);
            continue;
        }
        match characters.next_if(|next| matches!(next, '\\' | '"' | '$' | '`' | '\n')) {
            Some('\n') => {},
            Some(escaped) => text.push(escaped),
            None => text.push('\\'),
        }
    }
    text
}

fn unquote(text: &str, quote: char) -> &str {
    text.strip_prefix(quote)
        .and_then(|rest| rest.strip_suffix(quote))
        .unwrap_or(text)
}

/// Lexical, so a write destination that does not exist yet resolves like one that does.
fn in_tree(dirs: Dirs, candidate: &str) -> Option<String> {
    // `~` needs `$HOME`, which the event does not carry.
    if candidate.starts_with('~') {
        return None;
    }
    let joined = if Path::new(candidate).is_absolute() {
        PathBuf::from(candidate)
    } else {
        dirs.base.join(candidate)
    };
    normalize(&joined)
        .starts_with(dirs.root)
        .then(|| candidate.to_owned())
}

fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {},
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push("..");
                }
            },
            other => normalized.push(other),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::{
        Dirs, Verdict, Word, classify_command, in_tree, normalize, operands, shell_quote,
        wildcard_matches,
    };

    const CWD: &str = "/repo";

    /// Must exist: the classifier stats grep operands without `-r` and paths with `#`, `@` or `:`.
    const FILES: [&str; 11] = [
        "src/a.ts",
        "src/b.ts",
        "src/c.ts",
        "src/n.txt",
        "src/grep.txt",
        "src/sym.go",
        "notes.md",
        "C#.md",
        "v:2",
        "a@b.txt",
        "my dir/a.ts",
    ];

    fn tree() -> TempDir {
        let tree = TempDir::new().expect("a temp tree");
        for file in FILES {
            let path = tree.path().join(file);
            std::fs::create_dir_all(path.parent().expect("a file has a parent"))
                .expect("a temp directory");
            std::fs::write(path, "x\n").expect("a temp file");
        }
        tree
    }

    fn cwd(tree: &TempDir) -> &str {
        tree.path().to_str().expect("a utf-8 temp path")
    }

    fn verdict(command: &str) -> Verdict {
        let tree = tree();
        classify_command(command, cwd(&tree))
    }

    fn blocked(command: &str) -> String {
        match verdict(command) {
            Verdict::Block { reason } => reason,
            Verdict::Allow => panic!("{command:?} was allowed"),
        }
    }

    fn assert_allowed(command: &str) {
        assert_eq!(verdict(command), Verdict::Allow, "{command:?}");
    }

    #[test]
    fn two_displayed_reads_in_one_chain_share_one_show_line() {
        let reason = blocked("cat src/a.ts && cat src/b.ts");

        assert!(
            reason.contains("run: lets show src/a.ts src/b.ts"),
            "{reason}"
        );
        assert_eq!(reason.matches("run: lets show").count(), 1, "{reason}");
    }

    #[test]
    fn a_semicolon_chain_is_the_same_one_line() {
        assert!(blocked("cat src/a.ts; cat src/b.ts").contains("run: lets show src/a.ts src/b.ts"));
    }

    #[test]
    fn the_same_path_read_twice_is_named_once() {
        let reason = blocked("cat src/a.ts && cat src/a.ts");

        assert!(reason.contains("run: lets show src/a.ts"), "{reason}");
        assert_eq!(reason.matches("src/a.ts").count(), 1, "{reason}");
    }

    #[test]
    fn a_lets_call_does_not_shield_a_later_read() {
        let reason = blocked("lets show a.ts && cat b.ts");

        assert!(reason.contains("b.ts"), "{reason}");
        assert!(!reason.contains("a.ts"), "{reason}");
    }

    #[test]
    fn a_read_consumed_by_another_command_is_allowed() {
        assert_allowed("cat a | jq");
        assert_allowed("cat src/a.ts | rg cap");
    }

    #[test]
    fn a_substitution_body_is_never_classified() {
        assert_allowed("echo $(cat VERSION)");
        assert_allowed("diff <(cat src/a.ts) <(cat src/b.ts)");
    }

    #[test]
    fn xargs_cat_blocks_only_when_it_is_the_last_stage() {
        assert_allowed("xargs cat | sort");

        let reason = blocked("find . -name '*.ts' | xargs cat");
        assert!(
            reason.contains("run: lets show $(find . -name '*.ts')"),
            "{reason}"
        );
    }

    #[test]
    fn cat_piped_into_head_blocks_with_the_head_range() {
        assert!(blocked("cat src/a.ts | head -5").contains("run: lets show src/a.ts:1-5"));
        assert!(blocked("cat src/a.ts | head -n 5").contains("run: lets show src/a.ts:1-5"));
    }

    #[test]
    fn every_other_cat_pipeline_shape_is_unaffected() {
        assert_allowed("cat src/a.ts src/b.ts | head -5");
        assert_allowed("cat src/a.ts | tail -5");
        assert_allowed("sort src/a.ts | head -5");
    }

    #[test]
    fn heredoc_to_stdin_is_allowed() {
        assert_allowed("jq . - <<'JSON'\n{}\nJSON");
        assert_allowed("lets edit --from - <<'EOF'\n{\"file\":\"a.ts\"}\nEOF");
    }

    #[test]
    fn edit_new_dash_heredoc_is_allowed() {
        assert_allowed("lets edit n.md --old note --new - <<'EOF'\nreplacement\nEOF");
    }

    #[test]
    fn a_heredoc_exempts_a_body_that_would_otherwise_block() {
        assert_allowed("head -n 5 src/a.ts <<'EOF'\nx\nEOF");
        assert_allowed("sed -n '1,3p' src/a.ts <<'EOF'\nx\nEOF");
        assert!(blocked("head -n 5 src/a.ts").contains("run: lets show src/a.ts:1-5"));
        assert!(blocked("sed -n '1,3p' src/a.ts").contains("run: lets show src/a.ts:1-3"));
    }

    #[test]
    fn a_lets_call_on_its_own_is_allowed() {
        assert_allowed("lets show a.ts");
        assert_allowed("lets show src/a.ts src/b.ts");
        assert_allowed("lets find 'cap' src/");
        assert_allowed("lets edit src/a.ts --old a --new b");
    }

    #[test]
    fn a_heredoc_that_writes_a_file_blocks_with_lets_write() {
        let reason = blocked("cat > scripts/x.sh <<'EOF'\necho hi\nEOF");

        assert!(
            reason.contains("run: lets write --force scripts/x.sh"),
            "{reason}"
        );
        assert!(reason.contains("heredoc"), "{reason}");
    }

    #[test]
    fn both_heredoc_write_orderings_block_with_lets_write() {
        assert!(
            blocked("cat > scripts/x.sh <<'EOF'\necho hi\nEOF")
                .contains("run: lets write --force scripts/x.sh")
        );
        assert!(
            blocked("cat <<'EOF' > scripts/x.sh\necho hi\nEOF")
                .contains("run: lets write --force scripts/x.sh")
        );
    }

    #[test]
    fn a_write_redirect_keeps_the_command_that_produced_the_content() {
        assert_allowed("python -c 'open(\"o\", \"w\")' > out.py");
        assert_allowed("cat src/a.ts > src/b.ts");
        assert_allowed("echo hi >> notes.md");
        assert_allowed("cargo test > results.log");
        assert_allowed("cmd > f");
        assert_allowed("cmd > f 2>&1");
        assert_allowed("cat src/a.ts | tr a b > out.txt");
    }

    #[test]
    fn every_write_redirect_operator_is_a_write() {
        for command in [
            "cat > out.txt <<'EOF'\nx\nEOF",
            "cat >| out.txt <<'EOF'\nx\nEOF",
            "cat &> out.txt <<'EOF'\nx\nEOF",
        ] {
            assert!(
                blocked(command).contains("run: lets write --force out.txt"),
                "{command:?}"
            );
        }
        for command in [
            "cat >> out.txt <<'EOF'\nx\nEOF",
            "cat &>> out.txt <<'EOF'\nx\nEOF",
        ] {
            assert_allowed(command);
        }
    }

    #[test]
    fn a_redirect_of_another_descriptor_leaves_a_displayed_read() {
        for command in ["cat src/a.ts 2> errors.log", "cat src/a.ts 2>/dev/null"] {
            let reason = blocked(command);
            assert!(
                reason.contains("run: lets show src/a.ts"),
                "{command:?}: {reason}"
            );
            assert!(!reason.contains("lets write"), "{command:?}: {reason}");
        }
        assert_allowed("cat src/a.ts > out.txt");
        assert_allowed("cat src/a.ts 1> out.txt");
    }

    #[test]
    fn a_descriptor_duplication_is_not_a_write() {
        assert!(blocked("cat src/a.ts >&2").contains("run: lets show src/a.ts"));
        assert_allowed("cat src/a.ts >& out.txt");
        assert_allowed("cat src/a.ts >&-");
    }

    #[test]
    fn a_command_with_no_redirect_and_no_matched_name_is_allowed() {
        assert_allowed("python -c 'open(\"o\", \"w\").write(\"x\")'");
        assert_allowed("cat < in.txt");
    }

    #[test]
    fn byte_mode_and_follow_mode_are_allowed() {
        assert_allowed("head -c 100 src/a.ts");
        assert_allowed("head -c100 src/a.ts");
        assert_allowed("head --bytes=100 src/a.ts");
        assert_allowed("tail -c 200 src/a.ts");
        assert_allowed("tail -f logs/run.log");
        assert_allowed("tail -F logs/run.log");
        assert_allowed("tail --follow logs/run.log");
        assert_allowed("tail --retry logs/run.log");
    }

    #[test]
    fn cat_display_neutral_flags_still_block() {
        assert!(blocked("cat -n src/a.ts").contains("run: lets show src/a.ts"));
        assert!(blocked("cat -b src/a.ts").contains("run: lets show src/a.ts"));
        assert!(blocked("cat -u src/a.ts").contains("run: lets show src/a.ts"));
        assert!(blocked("cat --number src/a.ts").contains("run: lets show src/a.ts"));
        assert!(blocked("cat --number-nonblank src/a.ts").contains("run: lets show src/a.ts"));
        assert!(blocked("cat -nb src/a.ts").contains("run: lets show src/a.ts"));
    }

    #[test]
    fn cat_formatting_flags_are_allowed() {
        assert_allowed("cat -A src/a.ts");
        assert_allowed("cat -v src/a.ts");
        assert_allowed("cat -e src/a.ts");
        assert_allowed("cat -t src/a.ts");
        assert_allowed("cat -E src/a.ts");
        assert_allowed("cat -T src/a.ts");
        assert_allowed("cat -s src/a.ts");
        assert_allowed("cat --show-all src/a.ts");
        assert_allowed("cat --squeeze-blank src/a.ts");
        assert_allowed("cat -nA src/a.ts");
    }

    #[test]
    fn a_bounded_read_carries_its_range() {
        assert!(blocked("head -n 20 src/a.ts").contains("run: lets show src/a.ts:1-20"));
        assert_eq!(
            blocked("head -n 20 src/a.ts"),
            blocked("head -20 src/a.ts"),
            "both forms name the same one range"
        );
        assert!(blocked("sed -n '5,9p' src/a.ts").contains("run: lets show src/a.ts:5-9"));
        assert!(blocked("cat src/a.ts").contains("run: lets show src/a.ts"));
    }

    #[test]
    fn a_ranged_target_merges_into_the_one_show_line() {
        let reason = blocked("cat src/b.ts && head -n 20 src/a.ts");

        assert!(
            reason.contains("run: lets show src/b.ts src/a.ts:1-20"),
            "{reason}"
        );
        assert_eq!(reason.matches("run: lets show").count(), 1, "{reason}");
    }

    #[test]
    fn a_read_whose_range_needs_the_file_length_is_allowed() {
        assert_allowed("tail -n 5 src/a.ts");
        assert_allowed("tail -5 src/a.ts");
        assert_allowed("head -n -5 src/a.ts");
        assert!(blocked("tail src/a.ts").contains("run: lets show src/a.ts"));
    }

    #[test]
    fn a_global_literal_sed_substitution_blocks_with_replace_all() {
        assert!(
            blocked("sed -i 's/foo/baz/g' src/c.ts")
                .contains("run: lets edit src/c.ts --old 'foo' --new 'baz' --all")
        );
        assert!(
            blocked("sed -i 's|cap = 10|cap = 20|g' src/a.ts")
                .contains("run: lets edit src/a.ts --old 'cap = 10' --new 'cap = 20' --all")
        );
    }

    /// Without `g` sed replaces the first match per line, which no `lets edit` mode reproduces.
    #[test]
    fn a_sed_substitution_without_g_is_allowed() {
        assert_allowed("sed -i 's/foo/baz/' src/a.ts");
    }

    #[test]
    fn a_sed_i_with_a_suffix_or_another_flag_is_allowed() {
        assert_allowed("sed -i.bak 's/foo/baz/g' src/a.ts");
        assert_allowed("sed -i -E 's/foo/baz/g' src/a.ts");
        assert_allowed("sed -i -n 's/foo/baz/g' src/a.ts");
        assert_allowed("sed --in-place=bak 's/foo/baz/g' src/a.ts");
    }

    #[test]
    fn a_global_literal_sed_substitution_over_two_files_blocks_naming_both() {
        assert!(
            blocked("sed -i 's/foo/baz/g' src/a.ts src/b.ts")
                .contains("run: lets edit src/a.ts src/b.ts --old 'foo' --new 'baz' --all")
        );
    }

    #[test]
    fn a_sed_i_over_several_files_with_one_outside_the_tree_is_allowed() {
        assert_allowed("sed -i 's/foo/baz/g' src/a.ts ../outside.ts");
    }

    #[test]
    fn a_sed_i_over_several_files_with_one_a_glob_is_allowed() {
        assert_allowed("sed -i 's/foo/baz/g' src/a.ts src/*.ts");
    }

    #[test]
    fn a_sed_substitution_that_is_not_literal_is_allowed() {
        assert_allowed("sed -i 's/cap.*/x/g' src/a.ts");
        assert_allowed("sed -i 's/a/&x/g' src/a.ts");
        assert_allowed("sed -i '1,3s/a/b/g' src/a.ts");
    }

    #[test]
    fn sed_quiet_blocks_with_lets_show() {
        assert!(blocked("sed -n '1,10p' src/a.ts").contains("run: lets show src/a.ts"));
    }

    #[test]
    fn a_single_line_sed_blocks_with_the_one_line_target() {
        assert!(blocked("sed -n '5p' src/n.txt").contains("run: lets show src/n.txt:5"));
    }

    #[test]
    fn a_single_line_sed_over_two_files_is_allowed() {
        assert_allowed("sed -n '5p' src/a.ts src/b.ts");
    }

    #[test]
    fn a_sed_range_over_a_function_blocks_with_the_symbol_target() {
        assert!(
            blocked("sed -n '/^func Target(/,/^}/p' src/sym.go")
                .contains("run: lets show 'src/sym.go#Target'")
        );
        assert!(
            blocked("sed -n '/^function target(/,/^}/p' src/a.ts")
                .contains("run: lets show 'src/a.ts#target'")
        );
        assert!(
            blocked("sed -n '/^class Widget[ :(]/,/^}/p' src/a.ts")
                .contains("run: lets show 'src/a.ts#Widget'")
        );
    }

    #[test]
    fn a_symbol_idiom_whose_name_does_not_end_is_allowed() {
        assert_allowed("sed -n '/^func Target/,/^}/p' src/sym.go");
        assert_allowed("sed -n '/^func Target[a-z]/,/^}/p' src/sym.go");
        assert_allowed("sed -n -E '/^func Target(/,/^}/p' src/sym.go");
    }

    #[test]
    fn a_sed_range_over_a_python_def_is_allowed() {
        assert_allowed("sed -n '/^def target/,/^}/p' src/sym.py");
    }

    #[test]
    fn a_sed_n_with_another_flag_is_allowed() {
        for command in [
            "sed -n -z '5p' src/n.txt",
            "sed -nz '5p' src/n.txt",
            "sed -n -s '5p' src/n.txt",
            "sed -n -e '5p' src/n.txt",
            "sed -n --expression=5p src/n.txt",
            "sed -n --debug '5p' src/n.txt",
        ] {
            assert_allowed(command);
        }
        assert!(blocked("sed -nE '5p' src/n.txt").contains("run: lets show src/n.txt:5"));
        assert!(blocked("sed --quiet -r '5p' src/n.txt").contains("run: lets show src/n.txt:5"));
    }

    #[test]
    fn every_other_sed_n_script_is_allowed() {
        assert_allowed("sed -n '$p' src/n.txt");
        assert_allowed("sed -n '/n3/,/n5/p' src/n.txt");
        assert_allowed("sed -n 'p' src/n.txt");
        assert_allowed("sed -n '1~2p' src/n.txt");
    }

    #[test]
    fn plain_sed_is_allowed() {
        assert_allowed("sed 's/a/b/' src/a.ts");
        assert_allowed("sed 's/a/b/' src/a.ts | tee out.txt");
    }

    #[test]
    fn a_displayed_search_blocks_with_lets_find() {
        assert!(blocked("grep -n 'cap' src/a.ts").contains("run: lets find 'cap' src/a.ts"));
        assert!(blocked("rg 'cap' src/").contains("run: lets find 'cap' src/"));
    }

    #[test]
    fn a_search_carries_its_match_changing_flags() {
        assert!(blocked("grep -F 'a.b' src/a.ts").contains("run: lets find -F 'a.b' src/a.ts"));
        assert!(
            blocked("grep -i -w 'cap' src/a.ts").contains("run: lets find -i -w 'cap' src/a.ts")
        );
        assert!(blocked("grep -C 3 'cap' src/a.ts").contains("run: lets find -C 3 'cap' src/a.ts"));
        assert!(
            blocked("grep -A 2 -B 2 'cap' src/a.ts").contains("run: lets find -C 2 'cap' src/a.ts")
        );
        assert!(
            blocked("grep -l 'cap' src/a.ts").contains("run: lets find --files 'cap' src/a.ts")
        );
        assert!(
            blocked("grep -c 'cap' src/a.ts").contains("run: lets find --count 'cap' src/a.ts")
        );
        assert!(blocked("rg -iF 'a.b' src/").contains("run: lets find -F -i 'a.b' src/"));
        assert!(
            blocked("grep --ignore-case --context=1 'cap' src/a.ts")
                .contains("run: lets find -i -C 1 'cap' src/a.ts")
        );
    }

    #[test]
    fn a_search_flag_with_no_lets_equivalent_is_allowed() {
        assert_allowed("grep -v 'cap' src/a.ts");
        assert_allowed("grep -o 'cap' src/a.ts");
        assert_allowed("grep -P 'ca.w' src/a.ts");
        assert_allowed("grep -z 'cap' src/a.ts");
        assert_allowed("grep --include=*.ts -r 'cap' src/");
        assert_allowed("grep -A 3 -B 1 'cap' src/a.ts");
        assert_allowed("rg -e 'cap' src/");
    }

    #[test]
    fn a_pathless_search_blocks_only_where_it_searches_the_tree() {
        assert!(blocked("rg 'cap'").contains("run: lets find 'cap'"));
        assert_allowed("grep 'cap'");
        assert_allowed("cat src/a.ts | rg 'cap'");
    }

    #[test]
    fn two_searches_keep_two_find_lines() {
        let reason = blocked("grep 'a' src/a.ts && grep 'b' src/b.ts");

        assert!(reason.contains("run: lets find 'a' src/a.ts"), "{reason}");
        assert!(reason.contains("run: lets find 'b' src/b.ts"), "{reason}");
    }

    #[test]
    fn a_path_outside_the_working_tree_is_allowed() {
        assert_allowed("cat /etc/hosts");
        assert_allowed("cat ~/.config/lets/config.toml");
        assert_allowed("cat ../other/src/a.ts");
        assert_allowed("cat /repository/src/a.ts");
        assert_allowed("sed -i 's/a/b/g' /etc/hosts");
        assert_allowed("echo hi > /tmp/out.txt");
    }

    #[test]
    fn a_path_inside_the_working_tree_blocks() {
        let tree = tree();
        let absolute = format!("{}/src/a.ts", cwd(&tree));
        let Verdict::Block { reason } = classify_command(&format!("cat {absolute}"), cwd(&tree))
        else {
            panic!("an absolute in-tree read was allowed");
        };
        assert!(
            reason.contains(&format!("run: lets show {absolute}")),
            "{reason}"
        );
        assert!(blocked("cat ./src/a.ts").contains("run: lets show ./src/a.ts"));
        assert!(blocked("cat src/../src/a.ts").contains("run: lets show src/../src/a.ts"));
        assert!(blocked("cat src/a.ts < in.txt").contains("run: lets show src/a.ts"));
    }

    #[test]
    fn a_read_with_operands_on_both_sides_of_the_tree_names_every_one() {
        assert!(blocked("cat src/a.ts /etc/hosts").contains("run: lets show src/a.ts /etc/hosts"));
        assert!(
            blocked("grep 'cap' src/a.ts /etc/hosts")
                .contains("run: lets find 'cap' src/a.ts /etc/hosts")
        );
    }

    #[test]
    fn a_read_entirely_outside_the_tree_is_allowed() {
        assert_allowed("cat /etc/hosts /etc/passwd");
        assert_allowed("grep 'cap' /etc/hosts");
    }

    #[test]
    fn a_command_the_grammar_cannot_parse_is_allowed() {
        assert_allowed("cat 'unclosed");
        assert_allowed("cat src/a.ts &&");
    }

    #[test]
    fn a_line_the_grammar_joins_onto_the_command_before_it_is_allowed() {
        assert_allowed("ls | sort | head -4\necho x 2>/dev/null");
        assert_allowed("ls | sort | head -4\ncat src/a.ts 2>/dev/null");
        assert!(
            blocked("ls | sort | head -4; cat src/a.ts 2>/dev/null")
                .contains("run: lets show src/a.ts")
        );
        assert!(blocked("cat \\\n  src/a.ts").contains("run: lets show src/a.ts"));
    }

    /// An accepted gap: the walk never descends into these bodies.
    #[test]
    fn a_body_the_walk_does_not_descend_into_is_allowed() {
        assert_allowed("eval \"cat src/a.ts\"");
        assert_allowed("bash -c 'cat src/a.ts'");
        assert_allowed("(cat src/a.ts)");
        assert_allowed("! cat src/a.ts");
        assert_allowed("if true; then cat src/a.ts; fi");
        assert_allowed("for f in a b; do cat $f; done");
    }

    #[test]
    fn an_argument_this_cannot_resolve_is_allowed() {
        assert_allowed("cat $FILE");
        assert_allowed("cat \"$FILE\"");
        assert_allowed("cat \"$(ls)\"");
        assert_allowed("cat -");
        assert_allowed("cat");
    }

    #[test]
    fn a_quoted_literal_path_blocks() {
        assert!(blocked("cat \"src/a.ts\"").contains("run: lets show src/a.ts"));
        assert!(blocked("cat 'src/a.ts'").contains("run: lets show src/a.ts"));
    }

    #[test]
    fn a_cwd_that_is_not_absolute_is_allowed() {
        assert_eq!(
            classify_command("cat src/a.ts", "repo"),
            Verdict::Allow,
            "relative cwd"
        );
        assert_eq!(
            classify_command("cat src/a.ts", ""),
            Verdict::Allow,
            "no cwd"
        );
    }

    #[test]
    fn every_block_carries_a_runnable_lets_command() {
        let blocking = [
            "cat src/a.ts",
            "cat src/a.ts && cat src/b.ts",
            "cat src/a.ts /etc/hosts",
            "head -n 20 src/a.ts",
            "tail src/a.ts",
            "sed -n '1,10p' src/a.ts",
            "sed -i 's/a/b/g' src/a.ts",
            "grep -n 'cap' src/a.ts",
            "grep -F 'a.b' -i src/a.ts",
            "rg 'cap'",
            "find . | xargs cat",
            "cat > scripts/x.sh <<'EOF'\nhi\nEOF",
            "cat <<'EOF' > out.txt\nhi\nEOF",
        ];
        for command in blocking {
            let reason = blocked(command);
            let run = reason
                .lines()
                .find(|line| line.starts_with("run: "))
                .unwrap_or_else(|| panic!("{command:?} blocked without a run line: {reason}"));
            let verb = ["lets show ", "lets find ", "lets edit ", "lets write "]
                .iter()
                .find(|verb| run.contains(*verb))
                .unwrap_or_else(|| panic!("{command:?}: {run} names no lets verb"));
            assert!(
                run.split(*verb).nth(1).is_some_and(|rest| !rest.is_empty()),
                "{command:?}: {run}"
            );
        }
    }

    #[test]
    fn the_same_command_classifies_the_same_way_every_time() {
        for command in [
            "cat src/a.ts && grep 'x' src/b.ts && cat src/c.ts",
            "cat a | jq",
        ] {
            assert_eq!(verdict(command), verdict(command), "{command:?}");
        }
    }

    #[test]
    fn normalize_resolves_dot_and_dotdot_without_touching_the_filesystem() {
        assert_eq!(
            normalize(Path::new("/repo/./src/../src/a.ts")),
            PathBuf::from("/repo/src/a.ts")
        );
        assert_eq!(
            normalize(Path::new("/repo/../other")),
            PathBuf::from("/other")
        );
    }

    #[test]
    fn in_tree_keeps_the_candidate_as_written() {
        let cwd = Path::new(CWD);
        let dirs = Dirs {
            root: cwd,
            base: cwd,
        };

        assert_eq!(in_tree(dirs, "./src/a.ts"), Some("./src/a.ts".to_owned()));
        assert_eq!(
            in_tree(dirs, "/repo/src/a.ts"),
            Some("/repo/src/a.ts".to_owned())
        );
        assert_eq!(in_tree(dirs, "../other/a.ts"), None);
        assert_eq!(in_tree(dirs, "~/a.ts"), None);
        assert_eq!(in_tree(dirs, "/repository/a.ts"), None);
    }

    #[test]
    fn in_tree_resolves_from_base_and_bounds_by_root() {
        let dirs = Dirs {
            root: Path::new(CWD),
            base: Path::new("/repo/src"),
        };

        assert_eq!(in_tree(dirs, "../notes.md"), Some("../notes.md".to_owned()));
        assert_eq!(in_tree(dirs, "../../notes.md"), None);
    }

    #[test]
    fn a_token_the_shell_would_reinterpret_is_quoted() {
        assert_eq!(shell_quote("src/a.ts"), "src/a.ts");
        assert_eq!(shell_quote("my file.ts"), "'my file.ts'");
        assert_eq!(shell_quote("it's.ts"), r"'it'\''s.ts'");
    }

    #[test]
    fn operands_skip_flags_and_the_values_they_take() {
        let words: Vec<Word> = ["-n", "20", "--", "-weird.ts", "src/a.ts"]
            .iter()
            .map(|a| Word {
                text: (*a).to_owned(),
                glob: false,
            })
            .collect();

        let found = operands(&words, super::COUNT_FLAGS)
            .map(|found| found.iter().map(|w| w.text.as_str()).collect::<Vec<_>>());
        assert_eq!(found, Some(vec!["-weird.ts", "src/a.ts"]));
    }

    #[test]
    fn a_leading_cd_is_kept_on_the_run_line_and_named_first() {
        let reason = blocked("cd src && cat a.ts");

        assert!(reason.starts_with("`cd src` kept; "), "{reason}");
        assert!(
            reason.contains("\nrun: cd src && lets show a.ts"),
            "{reason}"
        );
    }

    #[test]
    fn a_leading_cd_joined_by_a_semicolon_or_newline_is_the_same_block() {
        for command in ["cd src; cat a.ts", "cd src\ncat a.ts"] {
            assert!(
                blocked(command).contains("run: cd src && lets show a.ts"),
                "{command:?}"
            );
        }
    }

    #[test]
    fn a_leading_cd_prefixes_every_run_line() {
        let reason = blocked("cd src && cat a.ts && grep -n foo b.ts");

        assert!(reason.contains("run: cd src && lets show a.ts"), "{reason}");
        assert!(
            reason.contains("run: cd src && lets find 'foo' b.ts"),
            "{reason}"
        );
    }

    #[test]
    fn a_cd_operand_the_shell_would_split_is_quoted_in_the_prefix() {
        let reason = blocked("cd 'my dir' && cat a.ts");

        assert!(reason.starts_with("`cd 'my dir'` kept; "), "{reason}");
        assert!(
            reason.contains("run: cd 'my dir' && lets show a.ts"),
            "{reason}"
        );
    }

    #[test]
    fn a_parent_path_after_a_cd_is_in_tree_when_it_stays_under_cwd() {
        assert!(
            blocked("cd src && cat ../notes.md").contains("run: cd src && lets show ../notes.md")
        );
    }

    #[test]
    fn a_cd_that_leaves_the_tree_allows() {
        assert_allowed("cd /tmp && cat a.ts");
        assert_allowed("cd .. && cat notes.md");
        assert_allowed("cd src && cat ../../notes.md");
    }

    #[test]
    fn a_cd_the_walk_cannot_place_allows() {
        for command in [
            "cat src/a.ts && cd src && cat b.ts",
            "cd - && cat src/a.ts",
            "cd \"$D\" && cat src/a.ts",
            "cd $(dirname src/a.ts) && cat a.ts",
            "cd ~/x && cat a.ts",
            "cd && cat src/a.ts",
            "cd -P src && cat a.ts",
            "cd src || cat a.ts",
            "cd src & cat a.ts",
            "cd src && cd lib && cat a.ts",
            "cd src && (cd lib && cat a.ts)",
            "cd src/* && cat a.ts",
            "pushd src && cat a.ts",
            "popd && cat src/a.ts",
            "cat src/a.ts; popd",
        ] {
            assert_allowed(command);
        }
    }

    #[test]
    fn a_heredoc_write_after_a_leading_cd_is_the_write_replacement() {
        let reason = blocked("cd src && cat > new.ts <<'EOF'\nx\nEOF");

        assert!(
            reason.contains("run: cd src && lets write --force new.ts"),
            "{reason}"
        );
        assert_allowed("cd /tmp/x && cat > a.txt <<'EOF'\nx\nEOF");
        assert_allowed("cd src && jq . - <<'EOF'\nx\nEOF");
    }

    #[test]
    fn a_plain_unquoted_glob_passes_into_the_replacement_unquoted() {
        assert!(blocked("cat src/*.ts").contains("run: lets show src/*.ts"));
        assert!(blocked("cat src/?.ts").contains("run: lets show src/?.ts"));
        assert!(blocked("grep -n foo src/*.ts").contains("run: lets find 'foo' src/*.ts"));
        assert!(blocked("cat \"src\"/*.ts").contains("run: lets show src/*.ts"));
    }

    #[test]
    fn a_quoted_star_stays_quoted() {
        assert!(blocked("cat 'src/*.ts'").contains("run: lets show 'src/*.ts'"));
    }

    #[test]
    fn a_glob_with_no_same_meaning_replacement_allows() {
        for command in [
            "cat app/[slug]/page.tsx",
            "cat src/{a,b}.ts",
            "cat 'my dir'/*.ts",
            "cat src/*'*'.ts",
            "head -n 2 src/*.ts",
            "cat src/*.ts | head -5",
            "sed -n 5p src/*.ts",
            "sed -i 's/a/b/g' src/*.ts",
            "cat > src/*.ts <<'EOF'\nx\nEOF",
        ] {
            assert_allowed(command);
        }
    }

    #[test]
    fn a_path_holding_a_target_metacharacter_blocks_only_when_it_names_a_file() {
        assert!(blocked("cat C#.md").contains("run: lets show 'C#.md'"));
        assert!(blocked("cat v:2").contains("run: lets show v:2"));
        assert!(blocked("cat src/a.ts C#.md").contains("run: lets show src/a.ts 'C#.md'"));
        assert!(blocked("cat a@b.txt").contains("run: lets show a@b.txt"));
        assert!(
            blocked("sed -i 's/a/b/g' C#.md")
                .contains("run: lets edit 'C#.md' --old 'a' --new 'b'")
        );
        for command in [
            "head -n 1 C#.md",
            "sed -n 3p v:2",
            "cat D#.md",
            "cat w:2",
            "cd src && cat C#.md",
        ] {
            assert_allowed(command);
        }
    }

    #[test]
    fn a_find_path_is_not_read_as_a_target() {
        assert!(blocked("grep -n foo C#.md").contains("run: lets find 'foo' 'C#.md'"));
    }

    #[test]
    fn a_grep_without_recursion_blocks_only_on_existing_files() {
        for command in [
            "grep foo src",
            "grep -n foo src/a.ts src",
            "grep foo src/missing.ts",
            "grep -F foo src/missing.ts",
            "grep foo *",
            "grep foo src/*.md",
            "grep foo 'my dir'/*.ts",
        ] {
            assert_allowed(command);
        }
        assert!(blocked("grep -r foo src").contains("run: lets find 'foo' src"));
        assert!(blocked("grep -R foo src").contains("run: lets find 'foo' src"));
        assert!(blocked("grep --recursive foo src").contains("run: lets find 'foo' src"));
        assert!(blocked("grep foo src/*.ts").contains("run: lets find 'foo' src/*.ts"));
        assert!(blocked("grep foo src/?.ts").contains("run: lets find 'foo' src/?.ts"));
        assert!(blocked("rg foo src").contains("run: lets find 'foo' src"));
    }

    #[test]
    fn a_wildcard_matches_the_whole_name() {
        assert!(wildcard_matches("*.ts", "a.ts"));
        assert!(wildcard_matches("?.ts", "a.ts"));
        assert!(wildcard_matches("a*b*c", "aXbYbZc"));
        assert!(wildcard_matches("*", "x"));
        assert!(!wildcard_matches("*.ts", "a.tsx"));
        assert!(!wildcard_matches("?.ts", "ab.ts"));
        assert!(!wildcard_matches("a*b", "ac"));
    }

    #[test]
    fn an_escaped_argument_is_read_as_the_command_receives_it() {
        assert!(
            blocked(r"grep alpha\|beta src/grep.txt")
                .contains("run: lets find 'alpha[|]beta' src/grep.txt")
        );
        assert!(
            blocked(r#"grep "alpha\\|beta" src/grep.txt"#)
                .contains("run: lets find 'alpha|beta' src/grep.txt")
        );
        assert!(blocked(r"cat my\ dir/a.ts").contains("run: lets show 'my dir/a.ts'"));
        assert!(blocked(r"cd my\ dir && cat a.ts").contains("run: cd 'my dir' && lets show a.ts"));
        assert!(blocked(r"cat src/\a.ts").contains("run: lets show src/a.ts"));
        assert!(blocked(r#"cat "src/\a.ts""#).contains(r"run: lets show 'src/\a.ts'"));
    }

    #[test]
    fn an_escape_with_no_exact_reading_is_allowed() {
        assert_allowed(r"cat src/\**.ts");
        assert_allowed(r#"grep "alpha\w" src/grep.txt"#);
    }

    #[test]
    fn an_extended_grep_pattern_the_dialects_read_differently_is_allowed() {
        for command in [
            r"grep -E 'v\d' src/a.ts",
            r"grep -E 'import [^\s]+' src/a.ts",
            "grep -E '*foo' src/a.ts",
            "grep -E 'a{,2}' src/a.ts",
        ] {
            assert_allowed(command);
        }
        assert!(
            blocked("grep -E 'foo|bar' src/a.ts").contains("run: lets find 'foo|bar' src/a.ts")
        );
    }

    #[test]
    fn a_rewritten_plain_grep_pattern_is_named_after_the_cd_note() {
        let reason = blocked(r"cd src && grep 'a\|b' grep.txt");

        assert!(
            reason.starts_with("`cd src` kept; grep pattern translated to lets regex; "),
            "{reason}"
        );
        assert!(
            reason.contains("run: cd src && lets find 'a|b' grep.txt"),
            "{reason}"
        );
    }

    #[test]
    fn a_plain_grep_pattern_the_translation_leaves_unchanged_carries_no_note() {
        let reason = blocked("grep 'cap' src/a.ts");

        assert!(!reason.contains("translated"), "{reason}");
    }

    #[test]
    fn grep_g_is_a_basic_regex_like_plain_grep() {
        assert!(blocked(r"grep -G 'a\|b' src/a.ts").contains(r"run: lets find 'a|b' src/a.ts"));
        assert!(
            blocked(r"grep --basic-regexp 'a\|b' src/a.ts")
                .contains(r"run: lets find 'a|b' src/a.ts")
        );
    }

    #[test]
    fn extended_fixed_and_rg_patterns_pass_unchanged() {
        for (command, run) in [
            ("grep -E 'a|b' src/a.ts", "run: lets find 'a|b' src/a.ts"),
            (
                "grep --extended-regexp 'f(x)' src/a.ts",
                "run: lets find 'f(x)' src/a.ts",
            ),
            ("grep -F 'a|b' src/a.ts", "run: lets find -F 'a|b' src/a.ts"),
            (r"rg 'a\|b' src/", r"run: lets find 'a\|b' src/"),
        ] {
            let reason = blocked(command);
            assert!(reason.contains(run), "{command:?}: {reason}");
            assert!(!reason.contains("translated"), "{command:?}: {reason}");
        }
    }

    #[test]
    fn a_plain_grep_pattern_with_no_exact_translation_allows() {
        assert_allowed(r"grep '\(a\)\1' src/a.ts");
        assert_allowed(r"grep '\<alpha' src/a.ts");
        assert_allowed(r"grep -rn '\+a' src/");
    }

    #[test]
    fn a_plain_grep_star_with_nothing_to_repeat_searches_for_a_star() {
        assert!(blocked(r"grep -rn '*a' src/").contains(r"run: lets find '\*a' src/"));
    }

    #[test]
    fn two_different_grep_dialects_allow() {
        assert_allowed("grep -E -F 'a' src/a.ts");
        assert_allowed("grep -G -E 'a' src/a.ts");
        assert_allowed("grep -FG 'a' src/a.ts");
        assert!(blocked("grep -E -E 'a' src/a.ts").contains("run: lets find 'a' src/a.ts"));
    }
}
