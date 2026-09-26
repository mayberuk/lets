//! Walks only displayed top-level statements: `program`/`list` children and a pipeline's last
//! stage. A substitution or an unrecognised node kind is never visited, which is allow.

use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use tree_sitter::{Node, Parser};

use super::permissions::{Access, Rules, Sources};
use super::{Verdict, bre};
use crate::grammars::{self, Language};

/// Each variant keeps the words it names as paths, which Claude Code's own rules are checked
/// against before any `lets` command is offered for them. `numbered` is set when the original
/// printed line numbers, so its rewrite keeps them.
#[derive(Debug)]
enum Finding {
    /// `extent` is `None` when a rewrite may not stand in for the read; `source` is `None` when the
    /// file comes from another command's output.
    Show {
        target: String,
        extent: Option<Extent>,
        source: Option<Word>,
        numbered: bool,
    },
    Find {
        flags: Vec<String>,
        pattern: String,
        paths: Vec<String>,
        operands: Vec<Word>,
        translated: bool,
        numbered: bool,
        /// The original set its own case (`-i`, or rg's `-s`/`-S`), which the rewrite keeps.
        case_chosen: bool,
    },
    /// Always rendered with `--all`: its only source is `sed -i 's/…/…/g'`.
    Edit {
        paths: Vec<String>,
        operands: Vec<Word>,
        old: String,
        new: String,
    },
    Write {
        path: String,
        operand: Word,
    },
}

/// A rewrite runs unseen, so its `lets show` prints every line the read would have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Extent {
    /// Needs `--all`: a bare `lets show` of a file stops at its window.
    Whole,
    /// A line or range target prints exactly those lines, and exits 1 when `first` is past the end.
    Lines { first: usize, last: usize },
}

/// `lets show`'s `--max-bytes` and `--max-file-bytes` defaults, which a rewrite runs under; a test
/// pins both to the parsed CLI.
const SHOW_MAX_BYTES: usize = 65_536;
const SHOW_MAX_FILE_BYTES: u64 = 8_388_608;

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

/// `claude_code` is set for Claude Code, whose settings hold the rules a rewrite or a suggested
/// command must not get around; Codex judges a rewritten command with its own approval and sandbox.
pub fn classify_command(command: &str, cwd: &str, claude_code: Option<&Sources>) -> Verdict {
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
    let mut segments = Vec::with_capacity(statements.len());
    for Statement { node, redirected } in &statements {
        let before = findings.len();
        let replaced = match redirected {
            Some(owner) => {
                classify_redirected(*owner, Some(*node), command, dirs, None, &mut findings)
            },
            None => classify_displayed(*node, command, dirs, None, &mut findings),
        };
        if unreproducible(&findings[before..], dirs) {
            findings.truncate(before);
        }
        segments.push(Segment {
            span: node.start_byte()..redirected.unwrap_or(*node).end_byte(),
            findings: before..findings.len(),
            replaced,
        });
    }
    let (findings, segments) = without_repeated_reads(findings, segments, dirs);
    let (notes, prefix) = notes(cd, &findings);
    if findings.is_empty() {
        return Verdict::Allow;
    }
    if let Some(sources) = claude_code
        && left_to_claude_code(&findings, dirs, sources)
    {
        return Verdict::Allow;
    }
    let operators = operators(&segments, command);
    let whole = segments
        .iter()
        .all(|segment| segment.replaced && !segment.findings.is_empty());
    if whole
        && operators
            .as_ref()
            .is_some_and(|operators| !operators.contains(&"||"))
        && let Some(command) = rewrite(&findings, &prefix, dirs)
    {
        return Verdict::Rewrite { command };
    }
    let drops = segments.iter().any(|segment| segment.findings.is_empty());
    let unread = unread_statuses(&segments, operators.as_deref(), &findings, root, command);
    let replacements = replacements(&segments, &findings, dirs, &unread);
    if operators.is_some()
        && let Some(command) = replacements
            .as_deref()
            .and_then(|replacements| splice_rewrite(command, replacements))
    {
        return Verdict::Rewrite { command };
    }
    // A Codex deny lists each replacement alone unless that would drop a statement. A `run:` line
    // is one line.
    let spliced = replacements
        .filter(|_| {
            (claude_code.is_some() || drops) && segments.len() > 1 && !command.contains('\n')
        })
        .and_then(|replacements| splice(command, &replacements));
    match reason(&findings, &notes, &prefix, spliced.as_deref()) {
        Some(reason) => Verdict::Block { reason },
        None => Verdict::Allow,
    }
}

/// Leaves every statement that reads a file some read in the command also names as typed: one
/// `lets show` prints a file once, however many times it is named.
fn without_repeated_reads(
    findings: Vec<Finding>,
    segments: Vec<Segment>,
    dirs: Dirs,
) -> (Vec<Finding>, Vec<Segment>) {
    let files: Vec<Option<PathBuf>> = findings
        .iter()
        .map(|finding| match finding {
            Finding::Show {
                extent: Some(_),
                source: Some(source),
                ..
            } => std::fs::canonicalize(dirs.base.join(&source.text)).ok(),
            _ => None,
        })
        .collect();
    let repeated = |at: usize| {
        files[at].as_ref().is_some_and(|file| {
            files
                .iter()
                .filter(|other| other.as_ref() == Some(file))
                .count()
                > 1
        })
    };
    if !(0..findings.len()).any(repeated) {
        return (findings, segments);
    }
    let mut findings: Vec<Option<Finding>> = findings.into_iter().map(Some).collect();
    let mut kept = Vec::with_capacity(findings.len());
    let segments = segments
        .into_iter()
        .map(|segment| {
            let start = kept.len();
            if segment.findings.clone().any(repeated) {
                return Segment {
                    findings: start..start,
                    replaced: false,
                    ..segment
                };
            }
            kept.extend(
                findings[segment.findings.clone()]
                    .iter_mut()
                    .filter_map(Option::take),
            );
            Segment {
                findings: start..kept.len(),
                ..segment
            }
        })
        .collect();
    (kept, segments)
}

/// What the reason says before its clauses, and the `cd` every unspliced `run:` line starts with.
fn notes(cd: Option<String>, findings: &[Finding]) -> (Vec<String>, String) {
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
        notes.push(TRANSLATED_NOTE.to_owned());
    }
    (notes, prefix)
}

const TRANSLATED_NOTE: &str = "grep pattern translated to lets regex";

/// Per statement, true when no operator after it, no `$?` and no `set -e` or `ERR` trap reads its
/// exit status. A search whose status is read is rewritten with `--cap-exit-0`, since `lets find`
/// exits 1 over its hit cap where grep exits 0, and with `-s`, since smart case can turn grep's
/// zero hits into some. `operators` is `None` across a join a splice cannot keep.
fn unread_statuses(
    segments: &[Segment],
    operators: Option<&[&str]>,
    findings: &[Finding],
    root: Node,
    src: &str,
) -> Vec<bool> {
    let searches = findings
        .iter()
        .any(|finding| matches!(finding, Finding::Find { .. }));
    let Some(operators) = operators.filter(|_| searches && !acts_on_status(root, src)) else {
        return vec![false; segments.len()];
    };
    (0..segments.len())
        .map(|at| {
            operators
                .get(at)
                .is_none_or(|operator| !matches!(*operator, "&&" | "||"))
        })
        .collect()
}

/// `set`, `shopt`, `trap`, and `?` or `PIPESTATUS` in any expansion syntax, anywhere, even inside
/// a body the walk never classifies: `{ set -e; }` still binds the statements after it.
fn acts_on_status(node: Node, src: &str) -> bool {
    let reads = match node.kind() {
        "command" => matches!(
            node.child_by_field_name("name").and_then(|n| text(n, src)),
            Some("set" | "shopt" | "trap")
        ),
        "special_variable_name" => text(node, src) == Some("?"),
        "variable_name" => text(node, src) == Some("PIPESTATUS"),
        _ => false,
    };
    if reads {
        return true;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(|child| acts_on_status(child, src))
}

/// A top-level statement: its bytes in the command, and the findings classifying it added.
struct Segment {
    span: Range<usize>,
    findings: Range<usize>,
    replaced: bool,
}

/// The `lets` command standing in for one statement; `exact` when it is that statement's rewrite,
/// printing every line or hit the statement would.
struct Replacement {
    span: Range<usize>,
    command: String,
    exact: bool,
}

/// One per statement with findings, or `None` when one needs more than a one-line command: a
/// heredoc write, whose `lets write` takes the heredoc on stdin. `unread` is `unread_statuses`.
fn replacements(
    segments: &[Segment],
    findings: &[Finding],
    dirs: Dirs,
    unread: &[bool],
) -> Option<Vec<Replacement>> {
    let mut replacements = Vec::new();
    for (at, segment) in segments.iter().enumerate() {
        let found = findings.get(segment.findings.clone())?;
        if found.is_empty() {
            continue;
        }
        if found
            .iter()
            .any(|finding| matches!(finding, Finding::Write { .. }))
        {
            return None;
        }
        let (lines, _) = run_lines(found);
        let [line] = <[String; 1]>::try_from(lines).ok()?;
        let (command, exact) = if let [
            Finding::Find {
                operands,
                flags,
                numbered,
                case_chosen,
                ..
            },
        ] = found
        {
            if segment.replaced && searches_in_tree(operands, dirs) {
                let listed = flags
                    .iter()
                    .any(|flag| matches!(flag.as_str(), "--files" | "--count"));
                let status_read = !unread.get(at).copied().unwrap_or(false);
                let mut command = line;
                if !numbered && !listed {
                    command.push_str(" --no-numbers");
                }
                if status_read {
                    if !case_chosen && !flags.iter().any(|flag| flag == "-s") {
                        command.push_str(" -s");
                    }
                    command.push_str(" --cap-exit-0");
                }
                (command, true)
            } else {
                (line, false)
            }
        } else {
            match segment.replaced.then(|| rewrite(found, "", dirs)).flatten() {
                Some(command) => (command, true),
                None => (line, false),
            }
        };
        replacements.push(Replacement {
            span: segment.span.clone(),
            command,
            exact,
        });
    }
    Some(replacements)
}

/// Every path a search names, or the directory it searches when it names none, is inside the tree
/// and not a dotfile, key or credential, judged as `fit` judges a read's.
fn searches_in_tree(operands: &[Word], dirs: Dirs) -> bool {
    let Ok(real_root) = std::fs::canonicalize(dirs.root) else {
        return false;
    };
    let inside = |path: &Path| {
        std::fs::canonicalize(path).is_ok_and(|real| {
            real.strip_prefix(&real_root)
                .is_ok_and(|inside| !is_sensitive(inside))
        })
    };
    if operands.is_empty() {
        return inside(dirs.base);
    }
    operands
        .iter()
        .all(|operand| rewritable(operand, dirs) && inside(&dirs.base.join(&operand.text)))
}

/// The spliced command only when every statement with findings is exact; the operators joining
/// them must already have been checked.
fn splice_rewrite(command: &str, replacements: &[Replacement]) -> Option<String> {
    if !replacements.iter().all(|replacement| replacement.exact) {
        return None;
    }
    splice(command, replacements)
}

/// Every byte outside the replaced spans stays as written, operators and a leading `cd` included.
fn splice(src: &str, replacements: &[Replacement]) -> Option<String> {
    let mut spliced = String::with_capacity(src.len() + 16 * replacements.len());
    let mut at = 0;
    for replacement in replacements {
        spliced.push_str(src.get(at..replacement.span.start)?);
        spliced.push_str(&replacement.command);
        at = replacement.span.end;
    }
    spliced.push_str(src.get(at..)?);
    Some(spliced)
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
        if !runs_next(gap) {
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
    // `cd` follows a symlink, so where the reads then happen is decided on the real directory.
    if let Ok(real) = std::fs::canonicalize(&base)
        && !std::fs::canonicalize(cwd).is_ok_and(|root| real.starts_with(root))
    {
        return None;
    }
    Some((base, Some(shell_quote(&operand.text)), statements))
}

/// `&&`, `;` or a newline: the next statement runs after this one, as a rewrite's one call would.
fn runs_next(gap: &str) -> bool {
    let operator = gap.trim();
    operator == "&&" || operator == ";" || operator.is_empty() && gap.contains('\n')
}

/// The operator joining each pair of statements, or `None` when one is `&`, a comment or anything
/// but `&&`, `||`, `;` or a newline, or when anything but `;` follows the last: a trailing `&`
/// changes when the command runs. One merged `lets show` cannot say `||`; a splice keeps it.
fn operators<'s>(segments: &[Segment], src: &'s str) -> Option<Vec<&'s str>> {
    let mut operators = Vec::with_capacity(segments.len());
    for pair in segments.windows(2) {
        let gap = src.get(pair[0].span.end..pair[1].span.start)?;
        let operator = gap.trim();
        if !runs_next(gap) && operator != "||" {
            return None;
        }
        operators.push(operator);
    }
    let tail = src.get(segments.last()?.span.end..)?;
    matches!(tail.trim(), "" | ";").then_some(operators)
}

/// One `lets show` for every read, or `None` when any finding needs a deny instead. A read of one
/// file drops the header, and a read that printed no numbers drops the gutter, so the output is
/// what the original printed plus a footer only when something was left out.
fn rewrite(findings: &[Finding], prefix: &str, dirs: Dirs) -> Option<String> {
    let mut targets: Vec<&str> = Vec::new();
    let mut reads: Vec<(PathBuf, Extent)> = Vec::new();
    let mut all = false;
    let mut gutter = None;
    for finding in findings {
        let Finding::Show {
            target,
            extent: Some(extent),
            source: Some(source),
            numbered,
        } = finding
        else {
            return None;
        };
        // One call takes one gutter setting, so reads that differ are spliced one call each.
        if *gutter.get_or_insert(*numbered) != *numbered {
            return None;
        }
        all |= *extent == Extent::Whole;
        if !targets.contains(&target.as_str()) {
            targets.push(target);
            reads.push((dirs.base.join(&source.text), *extent));
        }
    }
    if targets.is_empty() || fit(&reads, dirs.root) != Fit::Exact {
        return None;
    }
    let mut command = format!("{prefix}lets show {}", targets.join(" "));
    if all {
        command.push_str(" --all");
    }
    if targets.len() == 1 {
        command.push_str(" --no-header");
    }
    if gutter == Some(false) {
        command.push_str(" --no-numbers");
    }
    Some(command)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fit {
    Exact,
    /// Runs as typed: the original prints something one `lets show` cannot.
    Unreproducible,
    /// Stays a deny: a dotfile, key or credential, or a file outside the tree, once symlinks
    /// resolve.
    Refused,
}

/// Reads each file as `lets show` will, so a rewrite stands only when its output holds every line
/// the read prints: a readable text file, under the byte cap, no line cut, no range starting past
/// the end.
fn fit(reads: &[(PathBuf, Extent)], root: &Path) -> Fit {
    let Ok(real_root) = std::fs::canonicalize(root) else {
        return Fit::Refused;
    };
    let mut real = Vec::with_capacity(reads.len());
    let mut missing = false;
    for (path, extent) in reads {
        let Ok(resolved) = std::fs::canonicalize(path) else {
            missing = true;
            continue;
        };
        match resolved.strip_prefix(&real_root) {
            Ok(inside) if !is_sensitive(inside) => {},
            _ => return Fit::Refused,
        }
        real.push((resolved, *extent));
    }
    if missing {
        return Fit::Unreproducible;
    }
    let mut shown = 0;
    for (path, extent) in real {
        // Every line costs its bytes plus one and drops at most a `\r`, so a whole file costs at
        // least half its size: past that, the read is skipped.
        let may_fit = std::fs::metadata(&path).is_ok_and(|metadata| {
            usize::try_from(metadata.len()).is_ok_and(|bytes| bytes / 2 <= SHOW_MAX_BYTES)
        });
        if extent == Extent::Whole && !may_fit {
            return Fit::Unreproducible;
        }
        let Ok(file) = crate::fs::read(&path, SHOW_MAX_FILE_BYTES) else {
            return Fit::Unreproducible;
        };
        let total = file.content.lines().count();
        let (first, last) = match extent {
            Extent::Whole => (1, total),
            Extent::Lines { first, last } if first <= total => (first, last.min(total)),
            Extent::Lines { .. } => return Fit::Unreproducible,
        };
        for line in file.content.lines().skip(first - 1).take(last + 1 - first) {
            shown += line.len() + 1;
            if line.len() > crate::window::LINE_DISPLAY_CAP || shown > SHOW_MAX_BYTES {
                return Fit::Unreproducible;
            }
        }
    }
    Fit::Exact
}

/// True when every read a statement adds could be rewritten but one `lets show` would print
/// less than it: the statement is then left unclassified, like one this walk does not recognise.
fn unreproducible(found: &[Finding], dirs: Dirs) -> bool {
    let mut reads = Vec::with_capacity(found.len());
    for finding in found {
        let Finding::Show {
            extent: Some(extent),
            source: Some(source),
            ..
        } = finding
        else {
            return false;
        };
        reads.push((dirs.base.join(&source.text), *extent));
    }
    !reads.is_empty() && fit(&reads, dirs.root) == Fit::Unreproducible
}

/// True when a path a finding names is one Claude Code's own deny or ask rules cover, or when those
/// rules cannot be read: the original command then goes to Claude Code as it was typed.
fn left_to_claude_code(findings: &[Finding], dirs: Dirs, sources: &Sources) -> bool {
    let Some(rules) = Rules::load(sources, dirs.root) else {
        return true;
    };
    if rules.is_empty() {
        return false;
    }
    let covered = |word: &Word, access| {
        if word.glob {
            return glob_covered(word, dirs.base, &rules, access);
        }
        // `~` needs a home this classifier never resolves.
        word.text.starts_with('~') || rules.cover(&normalize(&dirs.base.join(&word.text)), access)
    };
    // `xargs cat` names no file, so Claude Code's rules never bound it either.
    findings.iter().any(|finding| match finding {
        Finding::Show { source, .. } => source
            .as_ref()
            .is_some_and(|word| covered(word, Access::Read)),
        Finding::Find { operands, .. } => operands.iter().any(|word| covered(word, Access::Read)),
        Finding::Edit { operands, .. } => operands.iter().any(|word| covered(word, Access::Edit)),
        Finding::Write { operand, .. } => covered(operand, Access::Edit),
    })
}

/// Expands a glob in its last component as bash does, and is true when any match is covered or
/// the glob is one this expansion cannot follow.
fn glob_covered(word: &Word, base: &Path, rules: &Rules, access: Access) -> bool {
    let path = Path::new(&word.text);
    let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
    else {
        return true;
    };
    // `[`, `{`, `(` and `**` need bash's full matcher; a `.`-led glob can reach `..`.
    if name.contains(['[', '{', '(']) || name.contains("**") || name.starts_with('.') {
        return true;
    }
    if parent
        .to_str()
        .is_none_or(|parent| parent.contains(GLOB_BYTES))
    {
        return true;
    }
    let directory = base.join(parent);
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return true;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return true;
        };
        let Some(entry_name) = entry.file_name().to_str().map(str::to_owned) else {
            return true;
        };
        if !entry_name.starts_with('.')
            && wildcard_matches(name, &entry_name)
            && rules.cover(&normalize(&directory.join(&entry_name)), access)
        {
            return true;
        }
    }
    false
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

/// True when the findings it adds stand in for all of `node`: nothing it runs is left out.
fn classify_displayed(
    node: Node,
    src: &str,
    dirs: Dirs,
    producer: Option<&str>,
    findings: &mut Vec<Finding>,
) -> bool {
    match node.kind() {
        "pipeline" => {
            let mut cursor = node.walk();
            let stages: Vec<Node> = node.children(&mut cursor).filter(Node::is_named).collect();
            if let Some(finding) = cat_head_pipeline(&stages, src, dirs)
                .or_else(|| numbered_range_pipeline(&stages, src, dirs))
            {
                findings.push(finding);
                return true;
            }
            let Some((displayed, upstream)) = stages.split_last() else {
                return false;
            };
            let upstream = upstream
                .iter()
                .filter_map(|stage| text(*stage, src))
                .collect::<Vec<_>>()
                .join(" | ");
            classify_displayed(*displayed, src, dirs, Some(&upstream), findings);
            false
        },
        "redirected_statement" => classify_redirected(
            node,
            node.child_by_field_name("body"),
            src,
            dirs,
            producer,
            findings,
        ),
        "command" => {
            classify_simple(node, src, dirs, producer, findings);
            is_plain(node)
        },
        _ => false,
    }
}

/// A prefix assignment or redirect changes what the command does, and a rewrite would drop it.
fn is_plain(command: Node) -> bool {
    let mut cursor = command.walk();
    let arguments = command
        .children_by_field_name("argument", &mut cursor)
        .count();
    command.named_child_count() == arguments + 1
}

/// Dropping `2>/dev/null` or `2>&1` only lets stderr reach the tool result.
fn only_moves_stderr(redirect: Node, src: &str) -> bool {
    if redirect.kind() != "file_redirect" {
        return false;
    }
    let field = |name| {
        redirect
            .child_by_field_name(name)
            .and_then(|child| text(child, src))
    };
    let mut cursor = redirect.walk();
    let operator = redirect
        .children(&mut cursor)
        .find(|child| !child.is_named())
        .and_then(|child| text(child, src));
    field("descriptor") == Some("2")
        && matches!(
            (operator, field("destination")),
            (Some(">"), Some("/dev/null")) | (Some(">&"), Some("1"))
        )
}

/// `body` differs from `node`'s own body only in the list shape `Statement` describes. True as
/// `classify_displayed`'s is.
fn classify_redirected(
    node: Node,
    body: Option<Node>,
    src: &str,
    dirs: Dirs,
    producer: Option<&str>,
    findings: &mut Vec<Finding>,
) -> bool {
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
                    operand: destination,
                });
            }
        }
        return false;
    }

    // Heredoc-to-stdin is exempt outright, before any command name is looked at.
    if heredoc {
        return false;
    }
    if redirects
        .iter()
        .any(|redirect| hides_stdout(*redirect, src))
    {
        return false;
    }
    let Some(body) = body else {
        return false;
    };
    classify_displayed(body, src, dirs, producer, findings)
        && redirects
            .iter()
            .all(|redirect| only_moves_stderr(*redirect, src))
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
            // `show` has no byte, follow or NUL-record mode.
            if arguments.iter().any(|a| {
                a.starts_with("-c")
                    || a.starts_with("--bytes")
                    || a.starts_with("--zero")
                    || a.starts_with('-') && !a.starts_with("--") && a.contains('z')
            }) {
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
            // Without a count, the whole-file replacement is not the ten lines they print.
            let (suffix, extent) = match count {
                None => (String::new(), None),
                // `head -n -5` (all but the last 5) has no range form.
                Some(value) => {
                    let Some(lines) = number(value).filter(|lines| *lines > 0) else {
                        return;
                    };
                    (
                        format!(":1-{lines}"),
                        Some(Extent::Lines {
                            first: 1,
                            last: lines,
                        }),
                    )
                },
            };
            let Some(operands) = operands(&words, COUNT_FLAGS) else {
                return;
            };
            show(&operands, &suffix, extent, false, dirs, findings);
        },
        "nl" => {
            if !nl_numbers_every_line(&arguments, None) {
                return;
            }
            let Some(operands) = operands(&words, &["-b"]) else {
                return;
            };
            // nl numbers across every file it is given as one stream.
            if let [operand] = operands.as_slice() {
                show(&[operand], "", Some(Extent::Whole), true, dirs, findings);
            }
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
            findings.push(Finding::Show {
                target: format!("$({producer})"),
                extent: None,
                source: None,
                numbered: false,
            });
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
        mut flags,
        syntax,
        recursive,
        numbered,
        case_chosen,
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
    // grep -r walks hidden and ignored files that `lets find`'s default walk skips.
    if recursive
        && !candidates
            .iter()
            .any(|candidate| is_sensitive(Path::new(&candidate.text)))
    {
        flags.extend(
            ["--hidden", "--no-ignore", "--exclude '.git/**'"]
                .into_iter()
                .map(str::to_owned),
        );
    }
    findings.push(Finding::Find {
        flags,
        translated: pattern != *written,
        pattern,
        paths,
        operands: candidates.iter().map(|word| (*word).clone()).collect(),
        numbered,
        case_chosen,
    });
}

fn classify_cat(words: &[Word], arguments: &[&str], dirs: Dirs, findings: &mut Vec<Finding>) {
    let Some(numbered) = cat_numbering(arguments) else {
        return;
    };
    let Some(operands) = operands(words, &[]) else {
        return;
    };
    show(&operands, "", Some(Extent::Whole), numbered, dirs, findings);
}

/// Whether `cat` numbers its lines, or `None` for a flag that changes what it prints otherwise.
fn cat_numbering(arguments: &[&str]) -> Option<bool> {
    let mut numbered = false;
    for &argument in arguments {
        if !argument.starts_with('-') || argument == "-" || argument == "--" {
            continue;
        }
        if let Some(long) = argument.strip_prefix("--") {
            let name = long.split('=').next().unwrap_or(long);
            if !matches!(name, "number" | "number-nonblank") {
                return None;
            }
            numbered = true;
            continue;
        }
        if argument[1..].contains(|c| !matches!(c, 'n' | 'b' | 'u')) {
            return None;
        }
        numbered |= argument[1..].contains(['n', 'b']);
    }
    Some(numbered)
}

/// `nl -ba` numbers every line as `cat -n` does; any other `nl` flag changes the numbering or its
/// format. `start` is the number nl's first line must get, `-v` or its default of 1, or `None`
/// where no offset is taken.
fn nl_numbers_every_line(arguments: &[&str], start: Option<usize>) -> bool {
    let mut every_line = false;
    let mut from = 1;
    let mut arguments = arguments.iter();
    while let Some(&argument) = arguments.next() {
        let offset = match argument {
            "-ba" | "--body-numbering=a" => {
                every_line = true;
                continue;
            },
            "-b" if arguments.next() == Some(&"a") => {
                every_line = true;
                continue;
            },
            "-v" => arguments.next().copied(),
            _ => argument.strip_prefix("-v"),
        };
        let Some(offset) = offset else {
            if argument.starts_with('-') {
                return false;
            }
            continue;
        };
        let (Some(_), Some(offset)) = (start, number(offset)) else {
            return false;
        };
        from = offset;
    }
    every_line && start.is_none_or(|start| start == from)
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
    let numbered = cat_numbering(&texts(&cat_words))?;
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

    let target = path_argument(path, &format!(":1-{lines}"), Some(dirs.base))?;
    Some(Finding::Show {
        target,
        extent: rewritable(path, dirs).then_some(Extent::Lines {
            first: 1,
            last: lines,
        }),
        source: Some((*path).clone()),
        numbered,
    })
}

/// `nl -ba f | sed -n 'A,Bp'` and `sed -n 'A,Bp' f | nl -ba -vA` as `f:A-B`, numbered. nl numbers
/// its input from 1 without `-v`, so a range past line 1 is rewritten only when `-v` names A.
fn numbered_range_pipeline(stages: &[Node], src: &str, dirs: Dirs) -> Option<Finding> {
    let [first, second] = stages else {
        return None;
    };
    let words = |stage: &Node, name: &str| -> Option<Vec<Word>> {
        if stage.kind() != "command"
            || stage.child_by_field_name("name").and_then(|n| text(n, src)) != Some(name)
        {
            return None;
        }
        let mut cursor = stage.walk();
        stage
            .children_by_field_name("argument", &mut cursor)
            .map(|argument| literal(argument, src))
            .collect()
    };
    let (nl_words, sed_words, file_first) = match (words(first, "nl"), words(second, "sed")) {
        (Some(nl), Some(sed)) => (nl, sed, true),
        _ => (words(second, "nl")?, words(first, "sed")?, false),
    };
    let sed_arguments = texts(&sed_words);
    let (script, path) = match (sed_arguments.as_slice(), file_first) {
        (["-n", script], true) => (*script, None),
        (["-n", script, _], false) => (*script, sed_words.last()),
        _ => return None,
    };
    let (first_line, last_line) =
        print_range(script).or_else(|| print_line(script).map(|line| (line, line)))?;
    if sed_words.iter().chain(&nl_words).any(|word| word.glob) {
        return None;
    }
    let nl_operands = operands(&nl_words, &["-b", "-v"])?;
    let path = match (path, nl_operands.as_slice()) {
        (Some(path), []) => {
            if !nl_numbers_every_line(&texts(&nl_words), Some(first_line)) {
                return None;
            }
            path
        },
        (None, [path]) => {
            if !nl_numbers_every_line(&texts(&nl_words), None) {
                return None;
            }
            *path
        },
        _ => return None,
    };
    in_tree(dirs, &path.text)?;
    let suffix = if first_line == last_line {
        format!(":{first_line}")
    } else {
        format!(":{first_line}-{last_line}")
    };
    let target = path_argument(path, &suffix, Some(dirs.base))?;
    Some(Finding::Show {
        target,
        extent: rewritable(path, dirs).then_some(Extent::Lines {
            first: first_line,
            last: last_line,
        }),
        source: Some(path.clone()),
        numbered: true,
    })
}

fn head_sole_count(arguments: &[&str]) -> Option<usize> {
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
    let operands = paths.iter().map(|word| (*word).clone()).collect();
    let Some(paths) = paths
        .iter()
        .map(|path| path_argument(path, "", Some(dirs.base)))
        .collect::<Option<Vec<String>>>()
    else {
        return;
    };
    findings.push(Finding::Edit {
        paths,
        operands,
        old,
        new,
    });
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
    // A symbol's extent comes from the grammar, not sed's first `^}`, so only a range is exact.
    let (suffix, extent) = match symbol {
        Some(name) => (format!("#{name}"), None),
        None => match print_line(&script.text) {
            Some(line) => (
                format!(":{line}"),
                Some(Extent::Lines {
                    first: line,
                    last: line,
                }),
            ),
            None => match print_range(&script.text) {
                Some((first, last)) => (
                    format!(":{first}-{last}"),
                    Some(Extent::Lines { first, last }),
                ),
                None => return,
            },
        },
    };
    if let Some(target) = path_argument(path, &suffix, Some(dirs.base)) {
        findings.push(Finding::Show {
            target,
            extent: extent.filter(|_| rewritable(path, dirs)),
            source: Some((*path).clone()),
            numbered: false,
        });
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
fn show(
    operands: &[&Word],
    suffix: &str,
    extent: Option<Extent>,
    numbered: bool,
    dirs: Dirs,
    findings: &mut Vec<Finding>,
) {
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
    findings.extend(
        operands
            .iter()
            .zip(targets)
            .map(|(operand, target)| Finding::Show {
                target,
                extent: extent.filter(|_| rewritable(operand, dirs)),
                source: Some((*operand).clone()),
                numbered,
            }),
    );
}

/// A named file inside the tree that is not a dotfile, key or credential. `fit`
/// judges the path again after any kept `cd` and once symlinks resolve.
fn rewritable(operand: &Word, dirs: Dirs) -> bool {
    !operand.glob
        && !operand.text.starts_with('-')
        && in_tree(dirs, &operand.text).is_some()
        && !is_sensitive(Path::new(&operand.text))
}

/// A dotfile, or a key, secret or credential by name: a backstop for reads no user rule names,
/// kept a deny the agent sees.
fn is_sensitive(path: &Path) -> bool {
    let lowercase = |part: &std::ffi::OsStr| part.to_string_lossy().to_ascii_lowercase();
    let secret = path.components().any(|component| match component {
        Component::Normal(name) => {
            let name = lowercase(name);
            name.starts_with('.') || matches!(name.as_str(), "secret" | "secrets")
        },
        _ => false,
    });
    let name = path.file_name().map(lowercase).unwrap_or_default();
    let extension = path.extension().map(lowercase).unwrap_or_default();
    secret
        || name.starts_with("id_")
        || name.contains("credentials")
        || name.contains(".tfstate")
        || matches!(
            extension.as_str(),
            "env" | "pem" | "key" | "p12" | "pfx" | "jks" | "keystore"
        )
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

/// The last count given, as `head` itself takes it.
fn line_count<'a>(arguments: &[&'a str]) -> Option<&'a str> {
    let mut count = None;
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
        count = Some(value);
    }
    count
}

/// Digits only: `str::parse` alone would accept `+5`.
fn number(value: &str) -> Option<usize> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn print_line(script: &str) -> Option<usize> {
    number(script.strip_suffix('p')?).filter(|line| *line >= 1)
}

fn print_range(script: &str) -> Option<(usize, usize)> {
    let (start, end) = script.strip_suffix('p')?.split_once(',')?;
    let start = number(start)?;
    let end = number(end)?;
    (start >= 1 && end >= start).then_some((start, end))
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
    numbered: bool,
    case_chosen: bool,
}

/// `None` for any flag `lets find` lacks: dropping `-v` or `-o` would search for something else.
fn find_flags(arguments: &[&str], head: &str) -> Option<Search> {
    let mut dialect = None;
    let mut recursive = false;
    let mut case = None;
    let mut word = false;
    let mut files = false;
    let mut count = false;
    let mut numbered = false;
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
                "ignore-case" => case = Some('i'),
                "case-sensitive" if head != "grep" => case = Some('s'),
                "smart-case" if head != "grep" => case = Some('S'),
                "word-regexp" => word = true,
                "files-with-matches" => files = true,
                "count" => count = true,
                "context" | "after-context" | "before-context" => contexts.push(match attached {
                    Some(value) => value.to_owned(),
                    None => (*arguments.next()?).to_owned(),
                }),
                "recursive" if head == "grep" => recursive = true,
                "line-number" => numbered = true,
                "with-filename" | "no-messages" | "color" | "colour" => {},
                _ => return None,
            }
            continue;
        }
        let mut characters = flag.chars();
        while let Some(character) = characters.next() {
            match character {
                'F' => choose(&mut dialect, 'F')?,
                'i' => case = Some('i'),
                'w' => word = true,
                'l' => files = true,
                'c' => count = true,
                'n' => numbered = true,
                // grep's `-s` is `--no-messages`; rg's `-s` and `-S` set case, the last one
                // winning.
                'H' => {},
                's' if head == "grep" => {},
                's' | 'S' if head != "grep" => case = Some(character),
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

    let flags: Vec<String> = [
        (dialect == Some('F')).then(|| "-F".to_owned()),
        case_flag(case, files || count).map(str::to_owned),
        word.then(|| "-w".to_owned()),
        context.map(|context| format!("-C {context}")),
        files.then(|| "--files".to_owned()),
        count.then(|| "--count".to_owned()),
    ]
    .into_iter()
    .flatten()
    .collect();
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
        numbered,
        case_chosen: case.is_some(),
    })
}

/// The original's own case choice, else grep's exact case for a count or file list, whose number
/// the agent cannot check against hits. rg's `-S` is `lets find`'s default, so it needs no flag.
fn case_flag(case: Option<char>, listed: bool) -> Option<&'static str> {
    match case {
        Some('i') => Some("-i"),
        Some('s') => Some("-s"),
        Some(_) => None,
        None => listed.then_some("-s"),
    }
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

/// `spliced`, when set, is the whole command with each statement's `lets` command in its place,
/// and is the one `run:` line; it already holds any kept `cd`, so `prefix` is not added to it.
fn reason(
    findings: &[Finding],
    notes: &[String],
    prefix: &str,
    spliced: Option<&str>,
) -> Option<String> {
    if findings.is_empty() {
        return None;
    }
    let (lines, clauses) = run_lines(findings);
    let (lines, prefix) = match spliced {
        Some(command) => (vec![command.to_owned()], ""),
        None => (lines, prefix),
    };

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

/// One `lets show` holds every read; each search, edit and write gets a line of its own.
fn run_lines(findings: &[Finding]) -> (Vec<String>, Vec<&'static str>) {
    let mut lines: Vec<String> = Vec::new();
    let mut show: Vec<&str> = Vec::new();
    let mut show_line = None;
    let mut clauses: Vec<&str> = Vec::new();

    for finding in findings {
        match finding {
            Finding::Show { target, .. } => {
                if !show.contains(&target.as_str()) {
                    show.push(target);
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
            Finding::Edit {
                paths, old, new, ..
            } => {
                lines.push(format!(
                    "lets edit {} --old {} --new {} --all",
                    paths.join(" "),
                    quote(old),
                    quote(new)
                ));
                add_clause(&mut clauses, EDIT_CLAUSE);
            },
            Finding::Write { path, .. } => {
                lines.push(format!("lets write --force {path}"));
                add_clause(&mut clauses, WRITE_CLAUSE);
            },
        }
    }
    if let Some(at) = show_line {
        lines[at] = format!("lets show {}", show.join(" "));
    }
    (lines, clauses)
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
        Dirs, SHOW_MAX_BYTES, SHOW_MAX_FILE_BYTES, Sources, Verdict, Word, classify_command,
        in_tree, normalize, operands, shell_quote, wildcard_matches,
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

    /// `src/n.txt` holds `n1` to `n10`; `.git` stops the search for a repository's local settings
    /// at the tree.
    fn tree() -> TempDir {
        let tree = TempDir::new().expect("a temp tree");
        for file in FILES {
            write(&tree, file, "x\n");
        }
        let numbered = numbered(1..=10, "n");
        write(&tree, "src/n.txt", &numbered);
        std::fs::create_dir(tree.path().join(".git")).expect("a temp .git");
        tree
    }

    /// One line per number, each `prefix` then the number.
    fn numbered(lines: std::ops::RangeInclusive<usize>, prefix: &str) -> String {
        use std::fmt::Write as _;
        lines.fold(String::new(), |mut text, n| {
            writeln!(text, "{prefix}{n}").expect("a String accepts a write");
            text
        })
    }

    fn write(tree: &TempDir, file: &str, content: &str) {
        let path = tree.path().join(file);
        std::fs::create_dir_all(path.parent().expect("a file has a parent"))
            .expect("a temp directory");
        std::fs::write(path, content).expect("a temp file");
    }

    fn cwd(tree: &TempDir) -> &str {
        tree.path().to_str().expect("a utf-8 temp path")
    }

    /// Every settings tier inside the tree: user settings under `.home/.claude`, project and local
    /// under `.claude`, managed under `.managed`.
    fn sources(tree: &TempDir) -> Sources {
        Sources {
            home: Some(tree.path().join(".home")),
            config: None,
            project: Some(tree.path().to_path_buf()),
            managed: tree.path().join(".managed"),
        }
    }

    /// Codex's view: no Claude Code settings are read, and a deny splices only to keep a statement.
    fn verdict(command: &str) -> Verdict {
        let tree = tree();
        classify_command(command, cwd(&tree), None)
    }

    fn blocked(command: &str) -> String {
        match verdict(command) {
            Verdict::Block { reason } => reason,
            other => panic!("{command:?} was not blocked: {other:?}"),
        }
    }

    fn claude_code(command: &str) -> Verdict {
        let tree = tree();
        classify_command(command, cwd(&tree), Some(&sources(&tree)))
    }

    fn rewritten(command: &str) -> String {
        match claude_code(command) {
            Verdict::Rewrite { command, .. } => command,
            other => panic!("{command:?} was not rewritten: {other:?}"),
        }
    }

    fn assert_still_blocked(command: &str) {
        assert!(
            matches!(claude_code(command), Verdict::Block { .. }),
            "{command:?} must stay a deny on Claude Code: {:?}",
            claude_code(command)
        );
    }

    fn assert_allowed(command: &str) {
        assert_eq!(verdict(command), Verdict::Allow, "{command:?}");
    }

    #[test]
    fn a_whole_file_read_rewrites_to_a_show_with_no_window() {
        assert_eq!(
            rewritten("cat src/a.ts"),
            "lets show src/a.ts --all --no-header --no-numbers"
        );
        assert_eq!(
            rewritten("cat src/a.ts && cat src/b.ts"),
            "lets show src/a.ts src/b.ts --all --no-numbers"
        );
        assert_eq!(
            rewritten("cat src/a.ts; cat src/b.ts\ncat src/c.ts"),
            "lets show src/a.ts src/b.ts src/c.ts --all --no-numbers"
        );
    }

    #[test]
    fn a_read_that_numbered_its_lines_keeps_the_gutter_and_one_that_did_not_drops_it() {
        for command in [
            "cat -n src/a.ts",
            "cat -b src/a.ts",
            "cat --number src/a.ts",
            "nl -ba src/a.ts",
            "nl -b a src/a.ts",
        ] {
            assert_eq!(
                rewritten(command),
                "lets show src/a.ts --all --no-header",
                "{command}"
            );
        }
        for command in ["cat src/a.ts", "cat -u src/a.ts"] {
            assert_eq!(
                rewritten(command),
                "lets show src/a.ts --all --no-header --no-numbers",
                "{command}"
            );
        }
        assert_eq!(
            rewritten("cat -n src/n.txt | head -3"),
            "lets show src/n.txt:1-3 --no-header"
        );
    }

    #[test]
    fn a_numbered_and_an_unnumbered_read_in_one_chain_each_keep_their_own_gutter() {
        assert_eq!(
            rewritten("cat -n src/a.ts && cat src/b.ts"),
            "lets show src/a.ts --all --no-header && lets show src/b.ts --all --no-header \
             --no-numbers"
        );
        assert_eq!(
            rewritten("cat src/a.ts && cat src/b.ts"),
            "lets show src/a.ts src/b.ts --all --no-numbers"
        );
    }

    #[test]
    fn an_nl_range_pipeline_rewrites_to_a_numbered_range() {
        for command in [
            "nl -ba src/n.txt | sed -n '3,5p'",
            "sed -n '3,5p' src/n.txt | nl -ba -v3",
            "sed -n '3,5p' src/n.txt | nl -ba -v 3",
        ] {
            assert_eq!(
                rewritten(command),
                "lets show src/n.txt:3-5 --no-header",
                "{command}"
            );
        }
        assert_eq!(
            rewritten("nl -ba src/n.txt | sed -n '4p'"),
            "lets show src/n.txt:4 --no-header"
        );
        assert_eq!(
            rewritten("sed -n '1,2p' src/n.txt | nl -ba"),
            "lets show src/n.txt:1-2 --no-header"
        );
        assert_eq!(
            rewritten("nl -ba src/a.ts; nl -ba src/b.ts"),
            "lets show src/a.ts src/b.ts --all"
        );
    }

    #[test]
    fn an_nl_whose_numbering_lets_show_would_not_print_is_allowed() {
        for command in [
            "nl src/a.ts",
            "nl -bt src/a.ts",
            "nl -ba -w3 src/a.ts",
            "nl -ba -v5 src/a.ts",
            "nl -ba src/a.ts src/b.ts",
            "sed -n '3,5p' src/n.txt | nl -ba -v4",
            "sed -n '3,5p' src/n.txt | nl -ba",
            "sed -n '3,5p' src/n.txt | nl",
            "nl -ba src/n.txt | sed -n '/x/p'",
            "nl -ba src/n.txt | sed '3,5p'",
            "nl -ba src/n.txt | head -3",
            "cat src/n.txt | nl -ba",
        ] {
            assert_eq!(claude_code(command), Verdict::Allow, "{command}");
            assert_allowed(command);
        }
    }

    #[test]
    fn an_exact_line_range_rewrites_to_that_range_alone() {
        assert_eq!(
            rewritten("head -n 5 src/a.ts"),
            "lets show src/a.ts:1-5 --no-header --no-numbers"
        );
        assert_eq!(
            rewritten("head -5 src/a.ts"),
            "lets show src/a.ts:1-5 --no-header --no-numbers"
        );
        assert_eq!(
            rewritten("sed -n '3,8p' src/n.txt"),
            "lets show src/n.txt:3-8 --no-header --no-numbers"
        );
        assert_eq!(
            rewritten("sed -n '5p' src/n.txt"),
            "lets show src/n.txt:5 --no-header --no-numbers"
        );
        assert_eq!(
            rewritten("cat src/n.txt | head -3"),
            "lets show src/n.txt:1-3 --no-header --no-numbers"
        );
        assert_eq!(
            rewritten("head -n 2 src/a.ts && cat src/b.ts"),
            "lets show src/a.ts:1-2 src/b.ts --all --no-numbers"
        );
    }

    #[test]
    fn a_kept_leading_cd_and_a_dropped_stderr_redirect_still_rewrite() {
        assert_eq!(
            rewritten("cd src && cat a.ts"),
            "cd src && lets show a.ts --all --no-header --no-numbers"
        );
        assert_eq!(
            rewritten("cat src/a.ts 2>/dev/null"),
            "lets show src/a.ts --all --no-header --no-numbers"
        );
        assert_eq!(
            rewritten("cat src/a.ts 2>&1"),
            "lets show src/a.ts --all --no-header --no-numbers"
        );
    }

    #[test]
    fn each_exact_read_in_a_chain_is_rewritten_where_it_stands() {
        assert_eq!(
            rewritten("git diff && cat src/a.ts"),
            "git diff && lets show src/a.ts --all --no-header --no-numbers"
        );
        assert_eq!(
            rewritten("lets show src/b.ts && cat src/a.ts"),
            "lets show src/b.ts && lets show src/a.ts --all --no-header --no-numbers"
        );
        assert_eq!(
            rewritten("cat src/a.ts || cat src/b.ts"),
            "lets show src/a.ts --all --no-header --no-numbers || lets show src/b.ts --all \
             --no-header --no-numbers"
        );
        assert_eq!(
            rewritten("cd src && cat a.ts && git status"),
            "cd src && lets show a.ts --all --no-header --no-numbers && git status"
        );
        assert_eq!(
            rewritten("cat src/a.ts\ngit log -1;"),
            "lets show src/a.ts --all --no-header --no-numbers\ngit log -1;"
        );
        assert_eq!(
            rewritten("cat src/a.ts 2>/dev/null; head -n 3 src/n.txt && ls"),
            "lets show src/a.ts --all --no-header --no-numbers; lets show src/n.txt:1-3 \
             --no-header --no-numbers && ls"
        );
    }

    /// Each `lets show` has its own `--max-bytes`, so two reads that overflow one merged show
    /// still fit one show each.
    #[test]
    fn reads_too_big_for_one_show_together_are_spliced_one_show_each() {
        let tree = tree();
        let lines = format!("{}\n", "y".repeat(99)).repeat(SHOW_MAX_BYTES / 2 / 100 + 1);
        write(&tree, "one.txt", &lines);
        write(&tree, "two.txt", &lines);
        let command = "cat one.txt && cat two.txt";

        let verdict = classify_command(command, cwd(&tree), Some(&sources(&tree)));

        let Verdict::Rewrite { command, .. } = verdict else {
            panic!("each read fits its own show: {verdict:?}");
        };
        assert_eq!(
            command,
            "lets show one.txt --all --no-header --no-numbers && lets show two.txt --all \
             --no-header --no-numbers"
        );
    }

    /// A read `lets show` cannot reproduce runs as typed, and the rest of its chain is still
    /// rewritten around it.
    #[test]
    fn a_read_lets_show_cannot_reproduce_is_left_as_typed_inside_a_rewritten_chain() {
        let tree = tree();
        write(&tree, "big.txt", &"z\n".repeat(SHOW_MAX_BYTES));
        write(&tree, "wide.txt", &format!("{}\n", "w".repeat(1_001)));

        assert_eq!(
            command_of(rewrite_in(&tree, "cat src/a.ts && cat big.txt; ls")),
            "lets show src/a.ts --all --no-header --no-numbers && cat big.txt; ls"
        );
        for command in [
            "cat wide.txt; ls",
            "sed -n '9,99p' src/a.ts; ls",
            "cat big.txt",
        ] {
            assert_eq!(rewrite_in(&tree, command), Verdict::Allow, "{command}");
            assert_eq!(
                classify_command(command, cwd(&tree), None),
                Verdict::Allow,
                "{command}"
            );
        }
    }

    #[test]
    fn a_chain_holding_a_read_that_fails_a_check_is_a_deny_that_keeps_every_part() {
        let tree = tree();
        write(&tree, ".env", "K=v\n");
        for (command, run) in [
            (
                "cat .env && cat src/a.ts && ls",
                "lets show .env && lets show src/a.ts --all --no-header --no-numbers && ls",
            ),
            (
                "cat src/a.ts & git status",
                "lets show src/a.ts --all --no-header --no-numbers & git status",
            ),
            (
                "cat src/a.ts; ls &",
                "lets show src/a.ts --all --no-header --no-numbers; ls &",
            ),
            (
                "cat src/a.ts && sed -i 's/x/y/g' src/b.ts && ls",
                "lets show src/a.ts --all --no-header --no-numbers && lets edit src/b.ts --old \
                 'x' --new 'y' --all && ls",
            ),
        ] {
            let verdict = classify_command(command, cwd(&tree), Some(&sources(&tree)));
            let Verdict::Block { reason } = verdict else {
                panic!("{command:?} must stay a deny: {verdict:?}");
            };
            assert!(
                reason.ends_with(&format!("\nrun: {run}")),
                "{command:?}: {reason}"
            );
            assert_eq!(
                reason.matches("\nrun: ").count(),
                1,
                "{command:?}: {reason}"
            );
        }
    }

    #[test]
    fn a_multi_line_chain_that_is_denied_keeps_one_run_line_per_lets_command() {
        let reason = blocked("sed -i 's/x/y/g' src/a.ts\ngit status");

        assert!(
            reason.ends_with("\nrun: lets edit src/a.ts --old 'x' --new 'y' --all"),
            "{reason}"
        );
    }

    #[test]
    fn a_codex_deny_splices_only_when_its_lines_would_drop_a_statement() {
        assert!(
            blocked("sed -i 's/x/y/g' src/a.ts; git status")
                .ends_with("\nrun: lets edit src/a.ts --old 'x' --new 'y' --all; git status")
        );
        let reason = blocked("cat .env && sed -i 's/x/y/g' src/b.ts");
        assert!(
            reason.ends_with(
                "\nrun: lets show .env\nrun: lets edit src/b.ts --old 'x' --new 'y' --all"
            ),
            "{reason}"
        );
    }

    #[test]
    fn an_edit_or_a_write_stays_a_deny() {
        assert_still_blocked("sed -i 's/a/b/g' src/a.ts");
        assert_still_blocked("cat > out.txt <<'EOF'\nx\nEOF");
    }

    #[test]
    fn an_exact_search_whose_status_nothing_reads_is_rewritten_where_it_stands() {
        assert_eq!(
            rewritten("grep -rn cap src"),
            "lets find --hidden --no-ignore --exclude '.git/**' 'cap' src"
        );
        assert_eq!(
            rewritten("rg cap src/a.ts"),
            "lets find 'cap' src/a.ts --no-numbers"
        );
        assert_eq!(rewritten("rg -l cap"), "lets find -s --files 'cap'");
        assert_eq!(rewritten("rg -c cap src"), "lets find -s --count 'cap' src");
        assert_eq!(
            rewritten("grep -n x src/b.ts 2>/dev/null"),
            "lets find 'x' src/b.ts"
        );
        assert_eq!(
            rewritten("cat src/a.ts; rg cap src/b.ts"),
            "lets show src/a.ts --all --no-header --no-numbers; lets find 'cap' src/b.ts \
             --no-numbers"
        );
        assert_eq!(
            rewritten("git status; grep -n x src/b.ts\nls"),
            "git status; lets find 'x' src/b.ts\nls"
        );
        assert_eq!(
            rewritten("cd src && cat a.ts && grep -n x b.ts"),
            "cd src && lets show a.ts --all --no-header --no-numbers && lets find 'x' b.ts"
        );
    }

    #[test]
    fn a_search_keeps_the_gutter_only_when_the_original_numbered_its_hits() {
        for command in [
            "grep -n x src/b.ts",
            "grep --line-number x src/b.ts",
            "rg -n x src/b.ts",
            "rg --line-number x src/b.ts",
            "grep -Hn x src/b.ts",
        ] {
            assert_eq!(rewritten(command), "lets find 'x' src/b.ts", "{command}");
        }
        for command in ["grep x src/b.ts", "rg x src/b.ts", "grep -H x src/b.ts"] {
            assert_eq!(
                rewritten(command),
                "lets find 'x' src/b.ts --no-numbers",
                "{command}"
            );
        }
    }

    #[test]
    fn a_rewritten_search_keeps_its_case_to_lets_finds_smart_case() {
        assert_eq!(
            rewritten(r"grep 'a\|x' src/grep.txt"),
            "lets find 'a|x' src/grep.txt --no-numbers"
        );
        assert_eq!(
            rewritten("grep -F 'foo + ' src/grep.txt"),
            "lets find -F 'foo + ' src/grep.txt --no-numbers"
        );
        assert_eq!(
            rewritten("grep fooBar src/grep.txt"),
            "lets find 'fooBar' src/grep.txt --no-numbers"
        );
        assert_eq!(
            rewritten("grep -i cap src/grep.txt"),
            "lets find -i 'cap' src/grep.txt --no-numbers"
        );
        assert!(!rewritten("rg x src/a.ts; rg x src/b.ts").contains(" -s"));
    }

    /// A count or a file list is a number the agent cannot check against the hits, so it keeps
    /// grep's exact case; `-i` asked for the opposite and keeps it.
    #[test]
    fn a_count_or_a_file_list_keeps_greps_exact_case() {
        for (command, replacement) in [
            ("grep -c x src/b.ts", "lets find -s --count 'x' src/b.ts"),
            (
                "grep --count x src/b.ts",
                "lets find -s --count 'x' src/b.ts",
            ),
            ("grep -l x src/b.ts", "lets find -s --files 'x' src/b.ts"),
            (
                "rg --files-with-matches x src",
                "lets find -s --files 'x' src",
            ),
            ("grep -c -i x src/b.ts", "lets find -i --count 'x' src/b.ts"),
        ] {
            assert_eq!(rewritten(command), replacement, "{command:?}");
        }
        assert_allowed("grep -q x src/b.ts");
        assert_allowed("grep -L x src/b.ts");
    }

    /// rg's `-i`, `-s` and `-S` each set the case and the last one wins; grep's `-s` is
    /// `--no-messages` and grep has no `-S`.
    #[test]
    fn rgs_last_case_flag_decides_and_greps_s_is_not_a_case_flag() {
        for (command, replacement) in [
            (
                "rg -i -s x src/b.ts",
                "lets find -s 'x' src/b.ts --no-numbers",
            ),
            (
                "rg -s -i x src/b.ts",
                "lets find -i 'x' src/b.ts --no-numbers",
            ),
            (
                "rg -is x src/b.ts",
                "lets find -s 'x' src/b.ts --no-numbers",
            ),
            (
                "rg --case-sensitive x src/b.ts",
                "lets find -s 'x' src/b.ts --no-numbers",
            ),
            ("rg -s -S x src/b.ts", "lets find 'x' src/b.ts --no-numbers"),
            (
                "rg --smart-case -c x src/b.ts",
                "lets find --count 'x' src/b.ts",
            ),
            (
                "rg -i -s x src/b.ts && ls",
                "lets find -s 'x' src/b.ts --no-numbers --cap-exit-0 && ls",
            ),
            (
                "rg -S x src/b.ts && ls",
                "lets find 'x' src/b.ts --no-numbers --cap-exit-0 && ls",
            ),
            ("grep -s x src/b.ts", "lets find 'x' src/b.ts --no-numbers"),
            (
                "grep -i -s x src/b.ts && ls",
                "lets find -i 'x' src/b.ts --no-numbers --cap-exit-0 && ls",
            ),
        ] {
            assert_eq!(rewritten(command), replacement, "{command:?}");
        }
        assert_allowed("grep -S x src/b.ts");
        assert_allowed("grep --smart-case x src/b.ts");
    }

    #[test]
    fn a_recursive_grep_walks_hidden_and_ignored_files_as_grep_does() {
        assert_eq!(
            rewritten("grep -r x src"),
            "lets find --hidden --no-ignore --exclude '.git/**' 'x' src --no-numbers"
        );
        assert_eq!(
            rewritten("grep -R -i x src ."),
            "lets find -i --hidden --no-ignore --exclude '.git/**' 'x' src . --no-numbers"
        );
        assert_eq!(rewritten("rg x src"), "lets find 'x' src --no-numbers");
        assert_eq!(
            rewritten("grep x src/a.ts"),
            "lets find 'x' src/a.ts --no-numbers"
        );
    }

    /// `lets find` exits 1 over its cap where grep exits 0, and smart case can find what grep's
    /// exact case does not, so a search whose status an operator, `$?`, `set -e` or an `ERR` trap
    /// reads carries `--cap-exit-0` and `-s`, and one whose status nothing reads carries neither.
    #[test]
    fn a_search_whose_exit_status_decides_what_runs_exits_as_grep_would() {
        for (command, replacement) in [
            (
                "grep -n x src/a.ts && ls",
                "lets find 'x' src/a.ts -s --cap-exit-0 && ls",
            ),
            (
                "rg x src/a.ts || ls",
                "lets find 'x' src/a.ts --no-numbers -s --cap-exit-0 || ls",
            ),
            (
                "cat src/a.ts && rg cap src/b.ts || ls",
                "lets show src/a.ts --all --no-header --no-numbers && lets find 'cap' src/b.ts \
                 --no-numbers -s --cap-exit-0 || ls",
            ),
            (
                "set -e; grep -n x src/a.ts",
                "set -e; lets find 'x' src/a.ts -s --cap-exit-0",
            ),
            (
                "trap 'echo failed' ERR; grep -n x src/a.ts",
                "trap 'echo failed' ERR; lets find 'x' src/a.ts -s --cap-exit-0",
            ),
            (
                "{ set -e; }; grep -n x src/a.ts",
                "{ set -e; }; lets find 'x' src/a.ts -s --cap-exit-0",
            ),
            (
                "grep x src/a.ts; echo $?",
                "lets find 'x' src/a.ts --no-numbers -s --cap-exit-0; echo $?",
            ),
            (
                "grep -i x src/a.ts && ls",
                "lets find -i 'x' src/a.ts --no-numbers --cap-exit-0 && ls",
            ),
            (
                "grep -c x src/a.ts && ls",
                "lets find -s --count 'x' src/a.ts --cap-exit-0 && ls",
            ),
            (
                "grep x src/a.ts; echo ${?}",
                "lets find 'x' src/a.ts --no-numbers -s --cap-exit-0; echo ${?}",
            ),
            (
                "grep x src/a.ts; echo \"${?}\"",
                "lets find 'x' src/a.ts --no-numbers -s --cap-exit-0; echo \"${?}\"",
            ),
            (
                "grep x src/a.ts; echo ${PIPESTATUS[0]}",
                "lets find 'x' src/a.ts --no-numbers -s --cap-exit-0; echo ${PIPESTATUS[0]}",
            ),
            (
                "grep x src/a.ts; echo ${PIPESTATUS[@]}",
                "lets find 'x' src/a.ts --no-numbers -s --cap-exit-0; echo ${PIPESTATUS[@]}",
            ),
            (
                "grep x src/a.ts; echo $PIPESTATUS",
                "lets find 'x' src/a.ts --no-numbers -s --cap-exit-0; echo $PIPESTATUS",
            ),
            (
                "shopt -so errexit; grep x src/a.ts",
                "shopt -so errexit; lets find 'x' src/a.ts --no-numbers -s --cap-exit-0",
            ),
            (
                "shopt -s -o errexit; grep x src/a.ts",
                "shopt -s -o errexit; lets find 'x' src/a.ts --no-numbers -s --cap-exit-0",
            ),
        ] {
            assert_eq!(rewritten(command), replacement, "{command:?}");
            assert_eq!(
                verdict(command),
                Verdict::Rewrite {
                    command: replacement.to_owned()
                },
                "{command:?} from Codex"
            );
        }
        assert_eq!(
            rewritten("grep -n x src/a.ts; ls"),
            "lets find 'x' src/a.ts; ls"
        );
        assert_still_blocked("sed -i 's/x/y/g' src/a.ts && ls");
    }

    #[test]
    fn a_search_of_a_dotfile_a_glob_or_a_path_leaving_the_tree_stays_a_deny() {
        let tree = tree();
        write(&tree, ".env", "x\n");
        write(&tree, "config/.secrets/k.txt", "x\n");
        let outside = TempDir::new().expect("a temp directory outside the tree");
        write(&outside, "o.txt", "x\n");
        std::os::unix::fs::symlink(outside.path(), tree.path().join("out"))
            .expect("a symlink out of the tree");
        for command in [
            "grep -n x .env",
            "grep -rn x config/.secrets",
            "grep -rn x .git",
            "grep -n x src/*.ts",
            "grep -rn x out",
            "cd config/.secrets && rg x",
        ] {
            let verdict = rewrite_in(&tree, command);
            assert!(
                matches!(verdict, Verdict::Block { .. }),
                "{command:?} must stay a deny: {verdict:?}"
            );
        }
        assert_eq!(
            command_of(rewrite_in(&tree, "grep -rn x src")),
            "lets find --hidden --no-ignore --exclude '.git/**' 'x' src"
        );
        assert!(
            reason_of(rewrite_in(&tree, "grep -rn x config/.secrets"))
                .ends_with("\nrun: lets find 'x' config/.secrets")
        );
    }

    #[test]
    fn a_codex_search_is_the_rewrite_claude_code_gets() {
        for command in ["grep -rn cap src", "rg cap src/a.ts"] {
            assert_eq!(verdict(command), claude_code(command), "{command}");
        }
    }

    #[test]
    fn a_read_one_lets_show_cannot_stand_in_for_stays_a_deny() {
        assert_still_blocked("head src/a.ts");
        assert_still_blocked("tail src/a.ts");
        assert_still_blocked("sed -n '/^func Target(/,/^}/p' src/sym.go");
        assert_still_blocked("find . | xargs cat");
        assert_still_blocked("cat src/a.ts &");
        assert_still_blocked("LC_ALL=C cat src/a.ts");
        assert_still_blocked("cat src/a.ts 2>err.txt");
        assert_still_blocked("cat src/a.ts < in.txt");
        assert_still_blocked("cat src/*.ts");
        assert_still_blocked("cat src/a.ts /etc/hosts");
        assert_still_blocked("head -n 3 -- -x");
    }

    #[test]
    fn a_dotfile_key_or_credential_read_stays_a_deny() {
        for command in [
            "cat .env",
            "cat config/.env.local",
            "cat .ssh/config",
            "cat certs/server.pem",
            "cat keys/id_ed25519",
            "cat aws/Credentials",
            "cat tls.key",
            "head -n 3 .env",
            "cat src/a.ts .env",
        ] {
            assert_still_blocked(command);
        }
    }

    #[test]
    fn a_read_outside_the_tree_is_left_to_claude_codes_own_rules() {
        assert_eq!(claude_code("cat ~/.ssh/id_rsa"), Verdict::Allow);
        assert_eq!(claude_code("cd /etc && cat passwd"), Verdict::Allow);
        assert_eq!(claude_code("cat ../other/src/a.ts"), Verdict::Allow);
    }

    fn rewrite_in(tree: &TempDir, command: &str) -> Verdict {
        classify_command(command, cwd(tree), Some(&sources(tree)))
    }

    fn command_of(verdict: Verdict) -> String {
        match verdict {
            Verdict::Rewrite { command, .. } => command,
            other => panic!("not a rewrite: {other:?}"),
        }
    }

    fn reason_of(verdict: Verdict) -> String {
        match verdict {
            Verdict::Block { reason } => reason,
            other => panic!("not a deny: {other:?}"),
        }
    }

    fn deny(rules: &[&str]) -> String {
        serde_json::json!({ "permissions": { "deny": rules } }).to_string()
    }

    const PROJECT: &str = ".claude/settings.json";

    /// A tree holding `private/notes.txt` and settings at `at`.
    fn with_rules(at: &str, settings: &str) -> TempDir {
        let tree = tree();
        write(&tree, "private/notes.txt", "HELLO\n");
        write(&tree, at, settings);
        tree
    }

    #[test]
    fn a_read_a_project_deny_rule_covers_is_left_to_claude_code() {
        let tree = with_rules(PROJECT, &deny(&["Read(./private/**)"]));

        for command in [
            "head -n 1 private/notes.txt",
            "cat private/notes.txt",
            "cat src/a.ts && cat private/notes.txt",
            "cd private && cat notes.txt",
        ] {
            assert_eq!(rewrite_in(&tree, command), Verdict::Allow, "{command}");
        }
        assert_eq!(
            command_of(rewrite_in(&tree, "head -n 1 src/a.ts")),
            "lets show src/a.ts:1-1 --no-header --no-numbers"
        );
    }

    #[test]
    fn a_deny_that_would_suggest_a_covered_path_is_left_to_claude_code() {
        let tree = with_rules(PROJECT, &deny(&["Read(./private/**)"]));

        assert_eq!(
            rewrite_in(&tree, "rg HELLO private/notes.txt"),
            Verdict::Allow
        );
        assert_eq!(rewrite_in(&tree, "grep -rn HELLO private"), Verdict::Allow);
        assert_eq!(rewrite_in(&tree, "cat private/*.txt"), Verdict::Allow);
        assert_eq!(rewrite_in(&tree, "cat */notes.txt"), Verdict::Allow);
        assert!(reason_of(rewrite_in(&tree, "cat src/*.ts")).ends_with("run: lets show src/*.ts"));
        assert!(matches!(
            rewrite_in(&tree, "find private | xargs cat"),
            Verdict::Block { .. }
        ));
        assert_eq!(
            command_of(rewrite_in(&tree, "rg HELLO src/a.ts")),
            "lets find 'HELLO' src/a.ts --no-numbers"
        );
    }

    #[test]
    fn an_ask_rule_leaves_a_read_to_claude_code_as_a_deny_rule_does() {
        let ask = serde_json::json!({ "permissions": { "ask": ["Read(./private/**)"] } });
        let tree = with_rules(PROJECT, &ask.to_string());

        assert_eq!(rewrite_in(&tree, "cat private/notes.txt"), Verdict::Allow);
        assert_eq!(
            command_of(rewrite_in(&tree, "cat src/a.ts")),
            "lets show src/a.ts --all --no-header --no-numbers"
        );
    }

    #[test]
    fn an_edit_rule_binds_edits_and_writes_and_a_read_rule_binds_all_three() {
        let edit = with_rules(PROJECT, &deny(&["Edit(./private/**)"]));
        assert_eq!(
            rewrite_in(&edit, "sed -i 's/HELLO/BYE/g' private/notes.txt"),
            Verdict::Allow
        );
        assert_eq!(
            rewrite_in(&edit, "cat > private/new.txt <<'EOF'\nx\nEOF"),
            Verdict::Allow
        );
        assert_eq!(
            command_of(rewrite_in(&edit, "cat private/notes.txt")),
            "lets show private/notes.txt --all --no-header --no-numbers"
        );

        let read = with_rules(PROJECT, &deny(&["Read(./private/**)"]));
        assert_eq!(
            rewrite_in(&read, "sed -i 's/HELLO/BYE/g' private/notes.txt"),
            Verdict::Allow
        );
        assert!(
            reason_of(rewrite_in(&read, "sed -i 's/cap/limit/g' src/a.ts"))
                .contains("run: lets edit src/a.ts")
        );
    }

    #[test]
    fn a_settings_file_or_rule_that_cannot_be_read_leaves_every_read_to_claude_code() {
        for settings in [
            "{not json".to_owned(),
            r#"{"permissions":{"deny":"Read(./private/**)"}}"#.to_owned(),
            r#"{"permissions":{"deny":[7]}}"#.to_owned(),
            r#"{"permissions":[]}"#.to_owned(),
            deny(&["Read(./private/**"]),
            deny(&["Read(../elsewhere/**)"]),
            deny(&["Read(~bob/notes.txt)"]),
        ] {
            let tree = with_rules(PROJECT, &settings);
            assert_eq!(
                rewrite_in(&tree, "cat src/a.ts"),
                Verdict::Allow,
                "{settings}"
            );
            assert_eq!(
                rewrite_in(&tree, "rg cap src/a.ts"),
                Verdict::Allow,
                "{settings}"
            );
        }
        for settings in [
            "{}",
            r#"{"permissions":{"deny":["Bash(rm *)","WebFetch"]}}"#,
        ] {
            let tree = with_rules(PROJECT, settings);
            assert_eq!(
                command_of(rewrite_in(&tree, "cat src/a.ts")),
                "lets show src/a.ts --all --no-header --no-numbers",
                "{settings}"
            );
        }
    }

    #[test]
    fn a_settings_path_that_is_a_directory_cannot_be_read() {
        let tree = tree();
        std::fs::create_dir_all(tree.path().join(PROJECT)).expect("a directory in its place");

        assert_eq!(rewrite_in(&tree, "cat src/a.ts"), Verdict::Allow);
    }

    #[test]
    fn a_slash_rule_anchors_at_the_project_in_project_settings_and_at_the_config_dir_in_user_settings()
     {
        let project = with_rules(PROJECT, &deny(&["Read(/private/**)"]));
        assert_eq!(
            rewrite_in(&project, "cat private/notes.txt"),
            Verdict::Allow
        );

        let user = with_rules(".home/.claude/settings.json", &deny(&["Read(/private/**)"]));
        assert_eq!(
            command_of(rewrite_in(&user, "cat private/notes.txt")),
            "lets show private/notes.txt --all --no-header --no-numbers"
        );
        write(&user, ".home/.claude/private/notes.txt", "x\n");
        assert!(matches!(
            classify_command("cat .home/.claude/private/notes.txt", cwd(&user), None),
            Verdict::Block { .. }
        ));
        assert_eq!(
            rewrite_in(&user, "cat .home/.claude/private/notes.txt"),
            Verdict::Allow
        );
    }

    #[test]
    fn user_settings_reach_the_project_through_an_absolute_or_relative_rule() {
        let tree = tree();
        write(&tree, "private/notes.txt", "HELLO\n");
        let absolute = format!("Read(/{}/private/**)", cwd(&tree));
        for rule in [
            absolute.as_str(),
            "Read(private/**)",
            "Read(./private/notes.txt)",
        ] {
            write(&tree, ".home/.claude/settings.json", &deny(&[rule]));
            assert_eq!(
                rewrite_in(&tree, "cat private/notes.txt"),
                Verdict::Allow,
                "{rule}"
            );
        }
    }

    #[test]
    fn a_home_rule_resolves_against_home() {
        let tree = tree();
        write(&tree, ".home/project/private/notes.txt", "HELLO\n");
        write(
            &tree,
            ".home/.claude/settings.json",
            &deny(&["Read(~/project/private/**)"]),
        );
        let project = tree.path().join(".home/project");
        let sources = Sources {
            project: Some(project.clone()),
            ..sources(&tree)
        };
        let project = project.to_str().expect("a utf-8 path");

        assert_eq!(
            classify_command("cat private/notes.txt", project, Some(&sources)),
            Verdict::Allow
        );
    }

    #[test]
    fn local_managed_and_config_dir_settings_are_read() {
        for at in [
            ".claude/settings.local.json",
            ".managed/managed-settings.json",
            ".managed/managed-settings.d/10-private.json",
        ] {
            let tree = with_rules(at, &deny(&["Read(./private/**)"]));
            assert_eq!(
                rewrite_in(&tree, "cat private/notes.txt"),
                Verdict::Allow,
                "{at}"
            );
            assert_eq!(
                command_of(rewrite_in(&tree, "cat src/a.ts")),
                "lets show src/a.ts --all --no-header --no-numbers",
                "{at}"
            );
        }

        let tree = with_rules("config/settings.json", &deny(&["Read(./private/**)"]));
        let sources = Sources {
            config: Some(tree.path().join("config")),
            ..sources(&tree)
        };
        assert_eq!(
            classify_command("cat private/notes.txt", cwd(&tree), Some(&sources)),
            Verdict::Allow
        );
    }

    #[test]
    fn a_managed_slash_rule_has_no_documented_anchor_so_nothing_is_promised() {
        let tree = with_rules(
            ".managed/managed-settings.json",
            &deny(&["Read(/private/**)"]),
        );

        assert_eq!(rewrite_in(&tree, "cat src/a.ts"), Verdict::Allow);
    }

    #[test]
    fn a_subdirectory_session_reads_local_settings_at_the_repository_root_only() {
        let local = tree();
        write(&local, "app/private/notes.txt", "HELLO\n");
        write(
            &local,
            ".claude/settings.local.json",
            &deny(&["Read(./private/**)"]),
        );
        let app = local.path().join("app");
        let at_app = Sources {
            project: None,
            ..sources(&local)
        };
        let app = app.to_str().expect("a utf-8 path");
        assert_eq!(
            classify_command("cat private/notes.txt", app, Some(&at_app)),
            Verdict::Allow
        );

        let shared = tree();
        write(&shared, "app/private/notes.txt", "HELLO\n");
        write(&shared, PROJECT, &deny(&["Read(./private/**)"]));
        let app = shared.path().join("app");
        let at_app = Sources {
            project: None,
            ..sources(&shared)
        };
        let app = app.to_str().expect("a utf-8 path");
        assert!(matches!(
            classify_command("cat private/notes.txt", app, Some(&at_app)),
            Verdict::Rewrite { .. }
        ));
    }

    #[test]
    fn a_worktree_reads_the_main_checkouts_local_settings() {
        let main = tree();
        write(
            &main,
            ".claude/settings.local.json",
            &deny(&["Read(./private/**)"]),
        );
        let worktree = TempDir::new().expect("a temp worktree");
        std::fs::write(
            worktree.path().join(".git"),
            format!("gitdir: {}/.git/worktrees/wt\n", cwd(&main)),
        )
        .expect("a worktree .git file");
        std::fs::create_dir_all(worktree.path().join("private")).expect("a temp directory");
        std::fs::write(worktree.path().join("private/notes.txt"), "HELLO\n").expect("a file");
        let sources = Sources {
            project: Some(worktree.path().to_path_buf()),
            ..sources(&main)
        };

        assert_eq!(
            classify_command(
                "cat private/notes.txt",
                worktree.path().to_str().expect("a utf-8 path"),
                Some(&sources)
            ),
            Verdict::Allow
        );
    }

    #[test]
    fn a_symlink_is_covered_when_either_its_path_or_its_target_is() {
        let tree = with_rules(PROJECT, &deny(&["Read(./private/**)"]));
        std::os::unix::fs::symlink("private", tree.path().join("link")).expect("a dir symlink");
        std::os::unix::fs::symlink("../private/notes.txt", tree.path().join("src/alias.txt"))
            .expect("a file symlink");

        assert_eq!(rewrite_in(&tree, "cat link/notes.txt"), Verdict::Allow);
        assert_eq!(rewrite_in(&tree, "cat src/alias.txt"), Verdict::Allow);

        write(&tree, PROJECT, &deny(&["Read(./other/**)"]));
        assert_eq!(
            command_of(rewrite_in(&tree, "cat link/notes.txt")),
            "lets show link/notes.txt --all --no-header --no-numbers"
        );
    }

    #[test]
    fn rule_shapes_match_as_claude_codes_permissions_page_describes() {
        write_and_expect_allow(&["Read(notes.txt)"], "cat private/notes.txt");
        write_and_expect_allow(&["Read(vendor/**)"], "cat lib/vendor/x.txt");
        write_and_expect_allow(&["Read(*.txt)"], "cat src/n.txt");
        write_and_expect_allow(&["Read(**)"], "cat src/a.ts");
        write_and_expect_allow(&["Read"], "cat src/a.ts");
        write_and_expect_allow(&["Read()"], "cat src/a.ts");
        write_and_expect_allow(&["Edit"], "sed -i 's/x/y/g' src/a.ts");
        write_and_expect_allow(
            &["Read(*.txt)", "Read(!notes.txt)"],
            "cat private/notes.txt",
        );

        for (rules, command, expected) in [
            (
                &["Read(docs/**)"][..],
                "cat src/a.ts",
                "lets show src/a.ts --all --no-header --no-numbers",
            ),
            (
                &["Edit"][..],
                "cat src/a.ts",
                "lets show src/a.ts --all --no-header --no-numbers",
            ),
            (
                &["Read(!src/a.ts)"][..],
                "cat src/a.ts",
                "lets show src/a.ts --all --no-header --no-numbers",
            ),
        ] {
            let tree = with_rules(PROJECT, &deny(rules));
            assert_eq!(
                command_of(rewrite_in(&tree, command)),
                expected,
                "{rules:?}"
            );
        }
    }

    fn write_and_expect_allow(rules: &[&str], command: &str) {
        let tree = with_rules(PROJECT, &deny(rules));
        write(&tree, "lib/vendor/x.txt", "x\n");
        assert_eq!(
            rewrite_in(&tree, command),
            Verdict::Allow,
            "{rules:?} {command}"
        );
    }

    #[test]
    fn without_home_or_a_config_dir_the_user_tier_cannot_be_found() {
        let tree = tree();
        let sources = Sources {
            home: None,
            ..sources(&tree)
        };

        assert_eq!(
            classify_command("cat src/a.ts", cwd(&tree), Some(&sources)),
            Verdict::Allow
        );
    }

    #[test]
    fn codex_rewrites_whatever_claude_codes_settings_say() {
        let tree = with_rules(PROJECT, &deny(&["Read(./private/**)"]));

        assert_eq!(
            classify_command("cat private/notes.txt", cwd(&tree), None),
            Verdict::Rewrite {
                command: "lets show private/notes.txt --all --no-header --no-numbers".to_owned()
            }
        );
        assert_eq!(rewrite_in(&tree, "cat private/notes.txt"), Verdict::Allow);
    }

    /// `lines` lines of `width` bytes each, newline included.
    fn lines_of(lines: usize, width: usize) -> String {
        format!("{}\n", "x".repeat(width - 1)).repeat(lines)
    }

    #[test]
    fn a_rewrite_needs_its_output_under_lets_shows_byte_cap() {
        let tree = tree();
        write(
            &tree,
            "at-cap.txt",
            &(lines_of(655, 100) + &lines_of(1, 36)),
        );
        write(
            &tree,
            "over-cap.txt",
            &(lines_of(655, 100) + &lines_of(1, 37)),
        );
        write(&tree, "many.txt", &numbered(1..=20_000, ""));

        assert_eq!(
            command_of(rewrite_in(&tree, "cat at-cap.txt")),
            "lets show at-cap.txt --all --no-header --no-numbers"
        );
        for command in ["cat over-cap.txt", "cat many.txt", "head -n 20000 many.txt"] {
            assert_eq!(rewrite_in(&tree, command), Verdict::Allow, "{command}");
        }
        assert_eq!(
            command_of(rewrite_in(&tree, "head -n 5 many.txt")),
            "lets show many.txt:1-5 --no-header --no-numbers"
        );
    }

    #[test]
    fn the_byte_cap_counts_every_file_of_one_rewrite() {
        let tree = tree();
        write(&tree, "a.txt", &lines_of(400, 100));
        write(&tree, "b.txt", &lines_of(400, 100));

        assert!(matches!(
            rewrite_in(&tree, "cat a.txt"),
            Verdict::Rewrite { .. }
        ));
        assert!(matches!(
            rewrite_in(&tree, "cat b.txt"),
            Verdict::Rewrite { .. }
        ));
        assert_eq!(rewrite_in(&tree, "cat a.txt b.txt"), Verdict::Allow);
    }

    #[test]
    fn a_line_lets_show_would_cut_is_allowed() {
        let tree = tree();
        write(&tree, "at-cap.ts", &format!("{}\n", "x".repeat(1000)));
        write(&tree, "long.ts", &format!("{}\n", "x".repeat(1001)));

        assert!(matches!(
            rewrite_in(&tree, "cat at-cap.ts"),
            Verdict::Rewrite { .. }
        ));
        assert_eq!(rewrite_in(&tree, "cat long.ts"), Verdict::Allow);
        assert_eq!(rewrite_in(&tree, "head -n 1 long.ts"), Verdict::Allow);
    }

    #[test]
    fn a_range_that_starts_past_the_end_is_allowed_and_one_that_ends_past_it_rewrites() {
        let tree = tree();
        write(&tree, "empty.txt", "");

        for command in [
            "head -n 5 empty.txt",
            "sed -n '11p' src/n.txt",
            "sed -n '11,12p' src/n.txt",
            "nl -ba src/n.txt | sed -n '11,12p'",
        ] {
            assert_eq!(rewrite_in(&tree, command), Verdict::Allow, "{command}");
        }
        assert_eq!(
            command_of(rewrite_in(&tree, "cat empty.txt")),
            "lets show empty.txt --all --no-header --no-numbers"
        );
        assert_eq!(
            command_of(rewrite_in(&tree, "sed -n '10p' src/n.txt")),
            "lets show src/n.txt:10 --no-header --no-numbers"
        );
        assert_eq!(
            command_of(rewrite_in(&tree, "sed -n '9,20p' src/n.txt")),
            "lets show src/n.txt:9-20 --no-header --no-numbers"
        );
        assert_eq!(
            command_of(rewrite_in(&tree, "head -n 50 src/n.txt")),
            "lets show src/n.txt:1-50 --no-header --no-numbers"
        );
    }

    #[test]
    fn a_file_lets_show_reads_differently_is_allowed() {
        let tree = tree();
        write(&tree, "nul.txt", "a\0b\n");
        std::fs::write(tree.path().join("latin.txt"), b"caf\xe9\n").expect("a latin-1 file");

        for command in [
            "cat nul.txt",
            "cat latin.txt",
            "cat src/missing.ts",
            "cat src",
        ] {
            assert_eq!(rewrite_in(&tree, command), Verdict::Allow, "{command}");
        }
    }

    #[test]
    fn head_z_counts_nul_records_so_it_is_not_a_line_read() {
        for command in [
            "head -z -n 1 src/a.ts",
            "head -zn 1 src/a.ts",
            "head --zero-terminated -n 1 src/a.ts",
        ] {
            assert_eq!(claude_code(command), Verdict::Allow, "{command}");
            assert_allowed(command);
        }
    }

    #[test]
    fn a_repeated_head_count_takes_the_last_as_head_does() {
        assert_eq!(
            rewritten("head -n 3 -n 5 src/n.txt"),
            "lets show src/n.txt:1-5 --no-header --no-numbers"
        );
        assert_eq!(
            rewritten("head --lines=3 --lines=5 src/n.txt"),
            "lets show src/n.txt:1-5 --no-header --no-numbers"
        );
        assert_eq!(verdict("head -n 5 -n 3 src/n.txt"), Verdict::Rewrite {
            command: "lets show src/n.txt:1-3 --no-header --no-numbers".to_owned()
        });
    }

    #[test]
    fn a_dotfile_or_secret_named_through_a_kept_cd_stays_a_deny() {
        let tree = tree();
        write(&tree, ".ssh/config", "Host x\n");
        write(&tree, ".git/config", "[core]\n");
        write(&tree, "secrets/prod.txt", "x\n");
        write(&tree, "deploy/prod.env", "x\n");

        for command in [
            "cd .ssh && cat config",
            "cd .git && cat config",
            "cd secrets && cat prod.txt",
            "cat secrets/prod.txt",
            "cd deploy && cat prod.env",
            "cat deploy/prod.env",
            "cat certs/a.p12",
            "cat infra/terraform.tfstate",
            "cat keys/release.jks",
        ] {
            assert!(
                matches!(rewrite_in(&tree, command), Verdict::Block { .. }),
                "{command}"
            );
        }
        assert_eq!(
            command_of(rewrite_in(&tree, "cd src && cat a.ts")),
            "cd src && lets show a.ts --all --no-header --no-numbers"
        );
    }

    #[test]
    fn a_symlink_out_of_the_tree_or_onto_a_dotfile_is_never_rewritten() {
        let tree = tree();
        let outside = TempDir::new().expect("a directory outside the tree");
        std::fs::write(outside.path().join("hostname"), "host\n").expect("an outside file");
        std::os::unix::fs::symlink(outside.path(), tree.path().join("etclink"))
            .expect("a dir symlink");
        write(&tree, ".ssh/config", "Host x\n");
        std::os::unix::fs::symlink("../.ssh/config", tree.path().join("src/cfg"))
            .expect("a file symlink");
        std::os::unix::fs::symlink("a.ts", tree.path().join("src/alias.ts"))
            .expect("an in-tree symlink");

        assert_eq!(
            rewrite_in(&tree, "cd etclink && cat hostname"),
            Verdict::Allow
        );
        assert_eq!(
            classify_command("cd etclink && cat hostname", cwd(&tree), None),
            Verdict::Allow
        );
        assert!(matches!(
            rewrite_in(&tree, "cat etclink/hostname"),
            Verdict::Block { .. }
        ));
        assert!(matches!(
            rewrite_in(&tree, "cat src/cfg"),
            Verdict::Block { .. }
        ));
        assert_eq!(
            command_of(rewrite_in(&tree, "cat src/alias.ts")),
            "lets show src/alias.ts --all --no-header --no-numbers"
        );
    }

    #[test]
    fn the_caps_a_rewrite_is_checked_against_are_lets_shows_defaults() {
        use clap::Parser as _;
        let cli = crate::cli::Cli::try_parse_from(["lets", "show", "x"]).expect("a show parses");

        assert_eq!(cli.global.max_bytes, SHOW_MAX_BYTES);
        assert_eq!(cli.global.max_file_bytes, SHOW_MAX_FILE_BYTES);
    }

    #[test]
    fn two_displayed_reads_in_one_chain_share_one_show_line() {
        assert_eq!(verdict("cat src/a.ts && cat src/b.ts"), Verdict::Rewrite {
            command: "lets show src/a.ts src/b.ts --all --no-numbers".to_owned()
        });
        let reason = blocked("cat src/a.ts && cat .env");
        assert!(
            reason.ends_with("\nrun: lets show src/a.ts .env"),
            "{reason}"
        );
    }

    #[test]
    fn a_semicolon_chain_is_the_same_one_line() {
        assert_eq!(
            rewritten("cat src/a.ts; cat src/b.ts"),
            "lets show src/a.ts src/b.ts --all --no-numbers"
        );
    }

    /// `cat f f` prints the file twice, and one `lets show` would print it once.
    #[test]
    fn a_file_read_twice_is_left_as_typed() {
        for command in [
            "cat src/a.ts && cat src/a.ts",
            "cat src/a.ts; cat src/a.ts",
            "cat src/a.ts src/a.ts",
            "cat -n src/a.ts ./src/a.ts",
            "head -n 3 src/n.txt; sed -n '2,5p' src/n.txt",
        ] {
            assert_allowed(command);
        }
        assert_eq!(
            rewritten("cat src/a.ts && cat src/b.ts"),
            "lets show src/a.ts src/b.ts --all --no-numbers"
        );
        assert_eq!(
            rewritten("cat src/a.ts src/a.ts && cat src/b.ts"),
            "cat src/a.ts src/a.ts && lets show src/b.ts --all --no-header --no-numbers"
        );
        let reason = blocked("cat src/a.ts src/a.ts && sed -i 's/x/y/g' src/b.ts");
        assert!(
            reason.ends_with(
                "\nrun: cat src/a.ts src/a.ts && lets edit src/b.ts --old 'x' --new 'y' --all"
            ),
            "{reason}"
        );
        let reason = blocked("cat .env && cat .env");
        assert!(reason.ends_with("\nrun: lets show .env"), "{reason}");
    }

    #[test]
    fn a_lets_call_does_not_shield_a_later_read() {
        assert_eq!(
            rewritten("lets show a.ts && cat src/b.ts"),
            "lets show a.ts && lets show src/b.ts --all --no-header --no-numbers"
        );
        let reason = blocked("lets show a.ts && cat .env");
        assert!(
            reason.ends_with("\nrun: lets show a.ts && lets show .env"),
            "{reason}"
        );
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
    fn cat_piped_into_head_rewrites_to_the_head_range() {
        for command in ["cat src/a.ts | head -5", "cat src/a.ts | head -n 5"] {
            assert_eq!(
                verdict(command),
                Verdict::Rewrite {
                    command: "lets show src/a.ts:1-5 --no-header --no-numbers".to_owned()
                },
                "{command}"
            );
        }
        assert!(blocked("cat .env | head -5").contains("run: lets show .env:1-5"));
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
        assert_eq!(
            rewritten("head -n 5 src/a.ts"),
            "lets show src/a.ts:1-5 --no-header --no-numbers"
        );
        assert_eq!(
            rewritten("sed -n '1,3p' src/a.ts"),
            "lets show src/a.ts:1-3 --no-header --no-numbers"
        );
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
        let reason = blocked("cat src/a.ts 2> errors.log");
        assert!(reason.ends_with("\nrun: lets show src/a.ts"), "{reason}");
        assert!(!reason.contains("lets write"), "{reason}");
        assert_eq!(
            rewritten("cat src/a.ts 2>/dev/null"),
            "lets show src/a.ts --all --no-header --no-numbers"
        );
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
    fn cat_display_neutral_flags_still_rewrite() {
        for command in [
            "cat -n src/a.ts",
            "cat -b src/a.ts",
            "cat --number src/a.ts",
            "cat --number-nonblank src/a.ts",
            "cat -nb src/a.ts",
            "cat -un src/a.ts",
        ] {
            assert_eq!(
                verdict(command),
                Verdict::Rewrite {
                    command: "lets show src/a.ts --all --no-header".to_owned()
                },
                "{command}"
            );
        }
        assert_eq!(verdict("cat -u src/a.ts"), Verdict::Rewrite {
            command: "lets show src/a.ts --all --no-header --no-numbers".to_owned()
        });
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
        assert_eq!(
            rewritten("head -n 20 src/n.txt"),
            "lets show src/n.txt:1-20 --no-header --no-numbers"
        );
        assert_eq!(
            verdict("head -n 20 src/n.txt"),
            verdict("head -20 src/n.txt"),
            "both forms name the same one range"
        );
        assert_eq!(
            rewritten("sed -n '5,9p' src/n.txt"),
            "lets show src/n.txt:5-9 --no-header --no-numbers"
        );
        assert!(blocked("head -n 20 .env").contains("run: lets show .env:1-20"));
    }

    #[test]
    fn a_ranged_target_merges_into_the_one_show_line() {
        assert_eq!(
            rewritten("cat src/b.ts && head -n 20 src/a.ts"),
            "lets show src/b.ts src/a.ts:1-20 --all --no-numbers"
        );
        let reason = blocked("cat .env && head -n 20 src/a.ts");
        assert!(
            reason.ends_with("\nrun: lets show .env src/a.ts:1-20"),
            "{reason}"
        );
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
    fn sed_quiet_rewrites_to_lets_show() {
        assert_eq!(verdict("sed -n '1,10p' src/a.ts"), Verdict::Rewrite {
            command: "lets show src/a.ts:1-10 --no-header --no-numbers".to_owned()
        });
    }

    #[test]
    fn a_single_line_sed_rewrites_to_the_one_line_target() {
        assert_eq!(
            rewritten("sed -n '5p' src/n.txt"),
            "lets show src/n.txt:5 --no-header --no-numbers"
        );
        assert!(blocked("sed -n '5p' .env").contains("run: lets show .env:5"));
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
        for command in ["sed -nE '5p' src/n.txt", "sed --quiet -r '5p' src/n.txt"] {
            assert_eq!(
                rewritten(command),
                "lets show src/n.txt:5 --no-header --no-numbers",
                "{command}"
            );
        }
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
    fn a_displayed_search_rewrites_to_lets_find() {
        assert_eq!(verdict("grep -n 'cap' src/a.ts"), Verdict::Rewrite {
            command: "lets find 'cap' src/a.ts".to_owned()
        });
        assert_eq!(
            rewritten("rg 'cap' src/"),
            "lets find 'cap' src/ --no-numbers"
        );
        assert!(blocked("grep -rn 'cap' .claude").ends_with("\nrun: lets find 'cap' .claude"));
    }

    #[test]
    fn a_search_carries_its_match_changing_flags() {
        for (command, replacement) in [
            (
                "grep -F 'a.b' src/a.ts",
                "lets find -F 'a.b' src/a.ts --no-numbers",
            ),
            (
                "grep -i -w 'cap' src/a.ts",
                "lets find -i -w 'cap' src/a.ts --no-numbers",
            ),
            (
                "grep -C 3 'cap' src/a.ts",
                "lets find -C 3 'cap' src/a.ts --no-numbers",
            ),
            (
                "grep -A 2 -B 2 'cap' src/a.ts",
                "lets find -C 2 'cap' src/a.ts --no-numbers",
            ),
            (
                "grep -l 'cap' src/a.ts",
                "lets find -s --files 'cap' src/a.ts",
            ),
            (
                "grep -c 'cap' src/a.ts",
                "lets find -s --count 'cap' src/a.ts",
            ),
            (
                "rg -iF 'a.b' src/",
                "lets find -F -i 'a.b' src/ --no-numbers",
            ),
            (
                "grep --ignore-case --context=1 'cap' src/a.ts",
                "lets find -i -C 1 'cap' src/a.ts --no-numbers",
            ),
        ] {
            assert_eq!(rewritten(command), replacement, "{command}");
        }
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
    fn a_pathless_search_is_replaced_only_where_it_searches_the_tree() {
        assert_eq!(rewritten("rg 'cap'"), "lets find 'cap' --no-numbers");
        assert_allowed("grep 'cap'");
        assert_allowed("cat src/a.ts | rg 'cap'");
    }

    #[test]
    fn two_searches_keep_two_find_calls() {
        assert_eq!(
            rewritten("grep 'a' src/a.ts && grep 'b' src/b.ts"),
            "lets find 'a' src/a.ts --no-numbers -s --cap-exit-0 && lets find 'b' src/b.ts \
             --no-numbers"
        );
        let reason = blocked("grep -r 'a' .claude && grep 'b' src/b.ts");
        assert!(
            reason.ends_with("\nrun: lets find 'a' .claude\nrun: lets find 'b' src/b.ts"),
            "{reason}"
        );
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
    fn a_path_inside_the_working_tree_is_replaced() {
        let tree = tree();
        let absolute = format!("{}/src/a.ts", cwd(&tree));
        // The temp tree's own `.tmp…` name reads as a dotfile component, so this is a deny.
        let Verdict::Block { reason } =
            classify_command(&format!("cat {absolute}"), cwd(&tree), None)
        else {
            panic!("an absolute in-tree read was allowed");
        };
        assert!(
            reason.ends_with(&format!("\nrun: lets show {absolute}")),
            "{reason}"
        );
        assert_eq!(
            rewritten("cat ./src/a.ts"),
            "lets show ./src/a.ts --all --no-header --no-numbers"
        );
        assert_eq!(
            rewritten("cat src/../src/a.ts"),
            "lets show src/../src/a.ts --all --no-header --no-numbers"
        );
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
        assert_eq!(
            rewritten("ls | sort | head -4; cat src/a.ts 2>/dev/null"),
            "ls | sort | head -4; lets show src/a.ts --all --no-header --no-numbers"
        );
        assert_eq!(
            rewritten("cat \\\n  src/a.ts"),
            "lets show src/a.ts --all --no-header --no-numbers"
        );
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
    fn a_quoted_literal_path_is_replaced_unquoted() {
        for command in ["cat \"src/a.ts\"", "cat 'src/a.ts'"] {
            assert_eq!(
                rewritten(command),
                "lets show src/a.ts --all --no-header --no-numbers",
                "{command}"
            );
        }
    }

    #[test]
    fn a_cwd_that_is_not_absolute_is_allowed() {
        let tree = tree();
        let sources = sources(&tree);
        assert_eq!(
            classify_command("cat src/a.ts", "repo", Some(&sources)),
            Verdict::Allow,
            "relative cwd"
        );
        assert_eq!(
            classify_command("cat src/a.ts", "", Some(&sources)),
            Verdict::Allow,
            "no cwd"
        );
    }

    #[test]
    fn every_block_and_every_rewrite_carries_a_runnable_lets_command() {
        let replaced = [
            "cat src/a.ts",
            "cat .env",
            "cat src/a.ts && cat src/b.ts",
            "cat src/a.ts /etc/hosts",
            "head -n 20 src/a.ts",
            "head -n 20 .env",
            "tail src/a.ts",
            "sed -n '1,10p' src/a.ts",
            "sed -i 's/a/b/g' src/a.ts",
            "grep -n 'cap' src/a.ts",
            "grep -rn 'cap' .claude && ls",
            "grep -F 'a.b' -i src/a.ts",
            "rg 'cap'",
            "nl -ba src/n.txt | sed -n '2,3p'",
            "find . | xargs cat",
            "cat > scripts/x.sh <<'EOF'\nhi\nEOF",
            "cat <<'EOF' > out.txt\nhi\nEOF",
        ];
        for command in replaced {
            let run = match verdict(command) {
                Verdict::Block { reason } => reason
                    .lines()
                    .find_map(|line| line.strip_prefix("run: "))
                    .unwrap_or_else(|| panic!("{command:?} blocked without a run line: {reason}"))
                    .to_owned(),
                Verdict::Rewrite { command } => command,
                Verdict::Allow => panic!("{command:?} was allowed"),
            };
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
        let reason = blocked("cd src && sed -i 's/x/y/g' a.ts");

        assert!(reason.starts_with("`cd src` kept; "), "{reason}");
        assert!(
            reason.ends_with("\nrun: cd src && lets edit a.ts --old 'x' --new 'y' --all"),
            "{reason}"
        );
        assert_eq!(
            rewritten("cd src && cat a.ts"),
            "cd src && lets show a.ts --all --no-header --no-numbers"
        );
    }

    #[test]
    fn a_leading_cd_joined_by_a_semicolon_or_newline_is_the_same_rewrite() {
        for command in ["cd src; cat a.ts", "cd src\ncat a.ts"] {
            assert_eq!(
                rewritten(command),
                "cd src && lets show a.ts --all --no-header --no-numbers",
                "{command:?}"
            );
        }
    }

    #[test]
    fn a_leading_cd_prefixes_every_run_line() {
        let reason = blocked("cd src && cat a.ts && sed -i 's/x/y/g' b.ts");

        assert!(reason.contains("run: cd src && lets show a.ts"), "{reason}");
        assert!(
            reason.contains("run: cd src && lets edit b.ts --old 'x' --new 'y' --all"),
            "{reason}"
        );
    }

    #[test]
    fn a_cd_operand_the_shell_would_split_is_quoted_in_the_prefix() {
        let reason = blocked("cd 'my dir' && sed -i 's/x/y/g' a.ts");

        assert!(reason.starts_with("`cd 'my dir'` kept; "), "{reason}");
        assert!(
            reason.contains("run: cd 'my dir' && lets edit a.ts"),
            "{reason}"
        );
        assert_eq!(
            rewritten("cd 'my dir' && cat a.ts"),
            "cd 'my dir' && lets show a.ts --all --no-header --no-numbers"
        );
    }

    #[test]
    fn a_parent_path_after_a_cd_is_in_tree_when_it_stays_under_cwd() {
        assert_eq!(
            rewritten("cd src && cat ../notes.md"),
            "cd src && lets show ../notes.md --all --no-header --no-numbers"
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
        let tree = tree();
        write(&tree, "src/*.ts", "x\n");

        assert_eq!(
            classify_command("cat 'src/*.ts'", cwd(&tree), None),
            Verdict::Rewrite {
                command: "lets show 'src/*.ts' --all --no-header --no-numbers".to_owned()
            }
        );
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
    fn a_path_holding_a_target_metacharacter_is_replaced_only_when_it_names_a_file() {
        for (command, replacement) in [
            (
                "cat C#.md",
                "lets show 'C#.md' --all --no-header --no-numbers",
            ),
            ("cat v:2", "lets show v:2 --all --no-header --no-numbers"),
            (
                "cat src/a.ts C#.md",
                "lets show src/a.ts 'C#.md' --all --no-numbers",
            ),
            (
                "cat a@b.txt",
                "lets show a@b.txt --all --no-header --no-numbers",
            ),
        ] {
            assert_eq!(rewritten(command), replacement, "{command}");
        }
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
        assert_eq!(rewritten("grep -n foo C#.md"), "lets find 'foo' 'C#.md'");
    }

    #[test]
    fn a_grep_without_recursion_is_replaced_only_on_existing_files() {
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
        for command in [
            "grep -r foo src",
            "grep -R foo src",
            "grep --recursive foo src",
        ] {
            assert_eq!(
                rewritten(command),
                "lets find --hidden --no-ignore --exclude '.git/**' 'foo' src --no-numbers",
                "{command}"
            );
        }
        assert!(blocked("grep foo src/*.ts").contains("run: lets find 'foo' src/*.ts"));
        assert!(blocked("grep foo src/?.ts").contains("run: lets find 'foo' src/?.ts"));
        assert_eq!(rewritten("rg foo src"), "lets find 'foo' src --no-numbers");
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
        for (command, replacement) in [
            (
                r"grep alpha\|beta src/grep.txt",
                "lets find 'alpha[|]beta' src/grep.txt --no-numbers",
            ),
            (
                r#"grep "alpha\\|beta" src/grep.txt"#,
                "lets find 'alpha|beta' src/grep.txt --no-numbers",
            ),
            (
                r"cat my\ dir/a.ts",
                "lets show 'my dir/a.ts' --all --no-header --no-numbers",
            ),
            (
                r"cd my\ dir && cat a.ts",
                "cd 'my dir' && lets show a.ts --all --no-header --no-numbers",
            ),
            (
                r"cat src/\a.ts",
                "lets show src/a.ts --all --no-header --no-numbers",
            ),
        ] {
            assert_eq!(rewritten(command), replacement, "{command}");
        }
        assert_allowed(r#"cat "src/\a.ts""#);
        let tree = tree();
        write(&tree, r"src/\a.ts", "x\n");
        assert_eq!(
            classify_command(r#"cat "src/\a.ts""#, cwd(&tree), None),
            Verdict::Rewrite {
                command: r"lets show 'src/\a.ts' --all --no-header --no-numbers".to_owned()
            }
        );
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
        assert_eq!(
            rewritten("grep -E 'foo|bar' src/a.ts"),
            "lets find 'foo|bar' src/a.ts --no-numbers"
        );
    }

    #[test]
    fn a_translated_plain_grep_pattern_is_named_after_the_cd_note() {
        let reason = blocked(r"cd src && grep 'a\|b' *.txt");

        assert!(
            reason.starts_with("`cd src` kept; grep pattern translated to lets regex; "),
            "{reason}"
        );
        assert!(
            reason.contains("run: cd src && lets find 'a|b' *.txt"),
            "{reason}"
        );
    }

    #[test]
    fn a_plain_grep_pattern_the_translation_leaves_unchanged_carries_no_note() {
        let reason = blocked("grep 'cap' src/*.ts");

        assert!(!reason.contains("translated"), "{reason}");
    }

    #[test]
    fn grep_g_is_a_basic_regex_like_plain_grep() {
        for command in [
            r"grep -G 'a\|b' src/a.ts",
            r"grep --basic-regexp 'a\|b' src/a.ts",
        ] {
            assert_eq!(
                rewritten(command),
                "lets find 'a|b' src/a.ts --no-numbers",
                "{command}"
            );
        }
    }

    #[test]
    fn extended_fixed_and_rg_patterns_pass_unchanged() {
        for (command, replacement) in [
            ("grep -E 'a|b' src/*.ts", "run: lets find 'a|b' src/*.ts"),
            (
                "grep --extended-regexp 'f(x)' src/*.ts",
                "run: lets find 'f(x)' src/*.ts",
            ),
            ("grep -F 'a|b' src/*.ts", "run: lets find -F 'a|b' src/*.ts"),
            (r"rg 'a\|b' src/*.ts", r"run: lets find 'a\|b' src/*.ts"),
        ] {
            let reason = blocked(command);
            assert!(reason.contains(replacement), "{command:?}: {reason}");
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
        assert_eq!(
            rewritten(r"grep -rn '*a' src/"),
            r"lets find --hidden --no-ignore --exclude '.git/**' '\*a' src/"
        );
    }

    #[test]
    fn two_different_grep_dialects_allow() {
        assert_allowed("grep -E -F 'a' src/a.ts");
        assert_allowed("grep -G -E 'a' src/a.ts");
        assert_allowed("grep -FG 'a' src/a.ts");
        assert_eq!(
            rewritten("grep -E -E 'a' src/a.ts"),
            "lets find 'a' src/a.ts --no-numbers"
        );
    }
}
