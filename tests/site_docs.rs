//! Every subcommand, flag and error slug the binary can produce is meant to have a home in
//! `site/src/content/docs/`; this asserts the docs and the binary have not drifted apart.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use clap::{Arg, Command, CommandFactory as _};
use lets::cli::Cli;

struct Page {
    file_name: String,
    commands: Vec<String>,
    front: String,
    body: String,
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn load_pages() -> Vec<Page> {
    let dir = manifest_dir().join("site/src/content/docs");
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("reading {}: {err}", dir.display()))
        .map(|entry| entry.expect("a readable dir entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let file_name = path
                .file_name()
                .expect("a file name")
                .to_string_lossy()
                .into_owned();
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
            let (front, commands, body) = split_frontmatter(&text);
            Page {
                file_name,
                commands,
                front,
                body,
            }
        })
        .collect()
}

/// Returns the page's frontmatter text, its `commands:` list, and everything after the fence.
fn split_frontmatter(text: &str) -> (String, Vec<String>, String) {
    let rest = text
        .strip_prefix("---\n")
        .expect("every docs page opens with a frontmatter fence");
    let end = rest
        .find("\n---\n")
        .expect("every docs page closes its frontmatter fence");
    let (front, body) = rest.split_at(end);
    let body = body["\n---\n".len()..].to_owned();
    let commands_line = front
        .lines()
        .find_map(|line| line.strip_prefix("commands:"))
        .expect("every docs page declares commands:");
    let inner = commands_line
        .trim()
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .expect("commands: is a bracketed flow list");
    let commands = inner
        .split(',')
        .map(|item| item.trim().trim_matches('"').to_owned())
        .filter(|item| !item.is_empty())
        .collect();
    (front.to_owned(), commands, body)
}

/// The page's frontmatter value for `key`, if the fence declares that key.
fn frontmatter_field<'a>(front: &'a str, key: &str) -> Option<&'a str> {
    front
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}:")))
        .map(str::trim)
}

/// Every non-hidden path in the subcommand tree, space-joined, with its own non-hidden args
/// (global args flattened onto a subcommand are not yet attached at this pre-parse stage).
fn subcommand_tree(cmd: &Command) -> Vec<(String, Vec<Arg>)> {
    let mut out = Vec::new();
    for sub in cmd.get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        walk(sub, sub.get_name(), &mut out);
    }
    out
}

fn walk(cmd: &Command, path: &str, out: &mut Vec<(String, Vec<Arg>)>) {
    let args = cmd
        .get_arguments()
        .filter(|arg| !arg.is_hide_set())
        .cloned()
        .collect();
    out.push((path.to_owned(), args));
    for sub in cmd.get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        walk(sub, &format!("{path} {}", sub.get_name()), out);
    }
}

fn arg_tokens(arg: &Arg) -> Vec<String> {
    let mut tokens = Vec::new();
    if let Some(long) = arg.get_long() {
        tokens.push(format!("--{long}"));
    }
    if let Some(short) = arg.get_short() {
        tokens.push(format!("-{short}"));
    }
    tokens
}

/// Every path documented by more than or fewer than one page, as a human-readable line.
fn coverage_violations(paths: &[String], page_commands: &[(String, String)]) -> Vec<String> {
    paths
        .iter()
        .filter_map(|path| {
            let owners: Vec<&str> = page_commands
                .iter()
                .filter(|(command, _)| command == path)
                .map(|(_, file)| file.as_str())
                .collect();
            match owners.len() {
                1 => None,
                0 => Some(format!("{path}: not documented by any page")),
                n => Some(format!(
                    "{path}: documented by {n} pages ({})",
                    owners.join(", ")
                )),
            }
        })
        .collect()
}

/// Every page `commands:` entry that names a path absent from `paths` — a page documenting a
/// subcommand the binary doesn't have, as a human-readable line.
fn phantom_command_violations(paths: &[String], page_commands: &[(String, String)]) -> Vec<String> {
    page_commands
        .iter()
        .filter(|(command, _)| !paths.contains(command))
        .map(|(command, file)| {
            format!("{file}: commands: names `{command}`, not a real subcommand")
        })
        .collect()
}

/// Whether `token`, a short flag such as `-A`, occurs in `body` at a real token boundary —
/// preceded by the start of the text, whitespace, or a backtick — and not merely as a substring
/// of a longer flag like `--all`.
fn short_flag_present(token: &str, body: &str) -> bool {
    body.match_indices(token)
        .any(|(i, _)| i == 0 || matches!(body.as_bytes()[i - 1], b' ' | b'\n' | b'\t' | b'`'))
}

/// Every flag token from `args` missing from `body`: a long flag by literal substring, a short
/// flag only when it occurs at a token boundary (see `short_flag_present`).
fn missing_flag_tokens(args: &[Arg], body: &str) -> Vec<String> {
    args.iter()
        .flat_map(arg_tokens)
        .filter(|token| {
            if token.starts_with("--") {
                !body.contains(token.as_str())
            } else {
                !short_flag_present(token, body)
            }
        })
        .collect()
}

/// Every token in `tokens` that appears in none of `bodies`.
fn missing_from_every_page(tokens: &[String], bodies: &[&str]) -> Vec<String> {
    tokens
        .iter()
        .filter(|token| !bodies.iter().any(|body| body.contains(token.as_str())))
        .cloned()
        .collect()
}

/// The exit codes 0 through 8 that `docs/guide.md`'s two `exits` lines list, in order.
fn exit_codes_from_guide(guide: &str) -> Vec<u8> {
    let lines: Vec<&str> = guide.lines().collect();
    let start = lines
        .iter()
        .position(|line| line.trim_start().starts_with("exits"))
        .expect("docs/guide.md has an exits line");
    let joined = format!(
        "{}\n{}",
        lines[start],
        lines.get(start + 1).copied().unwrap_or_default()
    );
    let after_label = joined.split_once("exits").expect("the exits label").1;
    after_label
        .split('\u{b7}')
        .filter_map(|segment| {
            let digits: String = segment
                .trim()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            (!digits.is_empty()).then(|| digits.parse().expect("a one- or two-digit exit code"))
        })
        .collect()
}

fn missing_exit_code_rows(body: &str, codes: &[u8]) -> Vec<u8> {
    codes
        .iter()
        .filter(|code| !body.contains(&format!("| {code} |")))
        .copied()
        .collect()
}

/// Every slug string literal inside a `fn slug` body in `src/error.rs`, deduplicated.
fn slugs_from_error_rs(source: &str) -> Vec<String> {
    let mut slugs = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = source[cursor..].find("fn slug") {
        let fn_start = cursor + offset;
        let body_start = source[fn_start..]
            .find('{')
            .map(|i| fn_start + i)
            .expect("fn slug has a body");
        let mut depth = 0i32;
        let mut body_end = body_start;
        for (i, ch) in source[body_start..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        body_end = body_start + i;
                        break;
                    }
                },
                _ => {},
            }
        }
        let body = &source[body_start..=body_end];
        for (i, ch) in body.char_indices() {
            if ch != '"' {
                continue;
            }
            if let Some(end) = body[i + 1..].find('"') {
                let literal = &body[i + 1..i + 1 + end];
                if !literal.is_empty()
                    && literal.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                {
                    slugs.push(literal.to_owned());
                }
            }
        }
        cursor = body_end + 1;
    }
    slugs.sort_unstable();
    slugs.dedup();
    slugs
}

fn missing_slugs(body: &str, slugs: &[String]) -> Vec<String> {
    slugs
        .iter()
        .filter(|slug| !body.contains(&format!("`{slug}`")))
        .cloned()
        .collect()
}

/// `body` split into blank-line-delimited blocks, each block's lines joined with a space so a
/// wrapped list item or sentence reads as one unit — the unit a rendered page shows as one line.
fn paragraphs(body: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in body.lines() {
        if line.trim().is_empty() {
            if !current.is_empty() {
                blocks.push(current.join(" "));
                current.clear();
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        blocks.push(current.join(" "));
    }
    blocks
}

/// Every form in `forms` that never shares a block with the word "block" — named somewhere on
/// the page, but never actually stated as one of the forms the hook blocks.
fn blocked_form_violations(body: &str, forms: &[&str]) -> Vec<String> {
    let blocks = paragraphs(body);
    forms
        .iter()
        .copied()
        .filter(|form| {
            !blocks
                .iter()
                .any(|block| block.contains(*form) && block.contains("block"))
        })
        .map(str::to_owned)
        .collect()
}

/// Every page missing a non-empty `title`, a `description` of 160 characters or fewer, an
/// integer `order` (a value reused by another page is reported once, naming every page that
/// reuses it), or a `group` of Verbs, Setup or Reference — human-readable lines naming the page.
fn frontmatter_violations(pages: &[(String, String)]) -> Vec<String> {
    const GROUPS: [&str; 3] = ["Verbs", "Setup", "Reference"];
    let mut violations = Vec::new();
    let mut orders: HashMap<i64, Vec<&str>> = HashMap::new();

    for (file, front) in pages {
        match frontmatter_field(front, "title") {
            Some(title) if !title.is_empty() => {},
            _ => violations.push(format!("{file}: missing or empty title")),
        }
        match frontmatter_field(front, "description") {
            Some(description) if description.chars().count() <= 160 => {},
            Some(description) => violations.push(format!(
                "{file}: description is {} characters, over the 160 limit",
                description.chars().count()
            )),
            None => violations.push(format!("{file}: missing description")),
        }
        match frontmatter_field(front, "order").and_then(|value| value.parse::<i64>().ok()) {
            Some(order) => orders.entry(order).or_default().push(file.as_str()),
            None => violations.push(format!("{file}: missing or non-integer order")),
        }
        match frontmatter_field(front, "group") {
            Some(group) if GROUPS.contains(&group) => {},
            Some(group) => violations.push(format!(
                "{file}: group `{group}` is not Verbs, Setup or Reference"
            )),
            None => violations.push(format!("{file}: missing group")),
        }
    }

    for (order, files) in &orders {
        if files.len() > 1 {
            violations.push(format!(
                "order {order} used by {} pages ({})",
                files.len(),
                files.join(", ")
            ));
        }
    }
    violations.sort();
    violations
}

fn read(rel: &str) -> String {
    let path = manifest_dir().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()))
}

fn find_page<'a>(pages: &'a [Page], command: &str) -> &'a Page {
    pages
        .iter()
        .find(|page| page.commands.iter().any(|c| c == command))
        .unwrap_or_else(|| panic!("no page in site/src/content/docs claims `{command}`"))
}

#[test]
fn every_subcommand_path_is_documented_by_exactly_one_page() {
    let pages = load_pages();
    let page_commands: Vec<(String, String)> = pages
        .iter()
        .flat_map(|page| {
            page.commands
                .iter()
                .map(|command| (command.clone(), page.file_name.clone()))
        })
        .collect();
    let paths: Vec<String> = subcommand_tree(&Cli::command())
        .into_iter()
        .map(|(path, _)| path)
        .collect();

    let violations = coverage_violations(&paths, &page_commands);
    assert!(violations.is_empty(), "{violations:#?}");
}

#[test]
fn coverage_violation_names_a_path_no_page_claims() {
    let paths = vec!["edit".to_owned(), "show".to_owned()];
    let page_commands = vec![("show".to_owned(), "show.md".to_owned())];

    let violations = coverage_violations(&paths, &page_commands);

    assert_eq!(violations, vec![
        "edit: not documented by any page".to_owned()
    ]);
}

#[test]
fn every_documented_command_is_a_real_subcommand_path() {
    let pages = load_pages();
    let page_commands: Vec<(String, String)> = pages
        .iter()
        .flat_map(|page| {
            page.commands
                .iter()
                .map(|command| (command.clone(), page.file_name.clone()))
        })
        .collect();
    let paths: Vec<String> = subcommand_tree(&Cli::command())
        .into_iter()
        .map(|(path, _)| path)
        .collect();

    let violations = phantom_command_violations(&paths, &page_commands);
    assert!(violations.is_empty(), "{violations:#?}");
}

#[test]
fn phantom_command_violation_names_a_command_no_path_has() {
    let paths = vec!["hooks".to_owned(), "hooks install".to_owned()];
    let page_commands = vec![("hooks status".to_owned(), "hooks.md".to_owned())];

    let violations = phantom_command_violations(&paths, &page_commands);

    assert_eq!(violations, vec![
        "hooks.md: commands: names `hooks status`, not a real subcommand".to_owned()
    ]);
}

#[test]
fn every_documented_subcommand_flag_appears_in_its_page() {
    let pages = load_pages();
    for (path, args) in subcommand_tree(&Cli::command()) {
        let owner = &page_commands_index(&pages)[&path];
        let page = pages.iter().find(|page| &page.file_name == owner).unwrap();
        let missing = missing_flag_tokens(&args, &page.body);
        assert!(
            missing.is_empty(),
            "{}: {:?} not found in {}",
            path,
            missing,
            page.file_name
        );
    }
}

/// One path per (documented) command, to look its single owning page up by name.
fn page_commands_index(pages: &[Page]) -> HashMap<String, String> {
    let mut index = HashMap::new();
    for page in pages {
        for command in &page.commands {
            index.insert(command.clone(), page.file_name.clone());
        }
    }
    index
}

#[test]
fn a_flag_removed_from_a_page_is_reported_missing() {
    let pages = load_pages();
    let page = find_page(&pages, "edit");
    let mutated = page.body.replace("--all", "");
    let (_, args) = subcommand_tree(&Cli::command())
        .into_iter()
        .find(|(path, _)| path == "edit")
        .expect("edit is a real subcommand");

    let missing = missing_flag_tokens(&args, &mutated);

    assert_eq!(missing, vec!["--all".to_owned()]);
}

#[test]
fn a_short_flag_matched_only_inside_a_longer_flag_is_reported_missing() {
    let args = vec![Arg::new("all").short('a').long("all")];
    let body = "flags: --all only, no short form spelled out anywhere";

    let missing = missing_flag_tokens(&args, body);

    assert_eq!(missing, vec!["-a".to_owned()]);
}

#[test]
fn every_global_flag_appears_on_some_docs_page() {
    let pages = load_pages();
    let bodies: Vec<&str> = pages.iter().map(|page| page.body.as_str()).collect();
    let tokens: Vec<String> = Cli::command()
        .get_arguments()
        .filter(|arg| !arg.is_hide_set())
        .flat_map(arg_tokens)
        .collect();

    let missing = missing_from_every_page(&tokens, &bodies);

    assert!(missing.is_empty(), "{missing:#?}");
}

#[test]
fn a_global_flag_absent_from_every_page_is_reported_missing() {
    let tokens = vec!["--json".to_owned(), "--budget".to_owned()];
    let bodies = ["flags: --budget only, nothing else"];

    let missing = missing_from_every_page(&tokens, &bodies);

    assert_eq!(missing, vec!["--json".to_owned()]);
}

#[test]
fn exit_codes_md_has_a_row_for_every_exit_code() {
    let guide = read("docs/guide.md");
    let mut codes = exit_codes_from_guide(&guide);
    codes.push(64);
    let exit_codes = read("site/src/content/docs/exit-codes.md");

    let missing = missing_exit_code_rows(&exit_codes, &codes);

    assert!(missing.is_empty(), "{missing:?}");
}

#[test]
fn removing_exit_code_eight_is_reported_missing() {
    let guide = read("docs/guide.md");
    let mut codes = exit_codes_from_guide(&guide);
    codes.push(64);
    let exit_codes = read("site/src/content/docs/exit-codes.md");
    let mutated = exit_codes
        .lines()
        .filter(|line| !line.starts_with("| 8 |"))
        .collect::<Vec<_>>()
        .join("\n");

    let missing = missing_exit_code_rows(&mutated, &codes);

    assert_eq!(missing, vec![8]);
}

#[test]
fn exit_codes_md_names_every_error_slug() {
    let source = read("src/error.rs");
    let slugs = slugs_from_error_rs(&source);
    assert!(
        slugs.len() >= 20,
        "the fn slug scan found suspiciously few slugs: {slugs:?}"
    );
    let exit_codes = read("site/src/content/docs/exit-codes.md");

    let missing = missing_slugs(&exit_codes, &slugs);

    assert!(missing.is_empty(), "{missing:?}");
}

#[test]
fn removing_a_slug_row_is_reported_missing() {
    let source = read("src/error.rs");
    let slugs = slugs_from_error_rs(&source);
    let exit_codes = read("site/src/content/docs/exit-codes.md");
    let mutated = exit_codes.replace("`no_grammar`", "");

    let missing = missing_slugs(&mutated, &slugs);

    assert_eq!(missing, vec!["no_grammar".to_owned()]);
}

#[test]
fn hooks_md_states_every_blocked_command_form() {
    let pages = load_pages();
    let page = find_page(&pages, "hook classify");

    let violations = blocked_form_violations(&page.body, &["cat", "sed -n", "grep", "sed -i"]);

    assert!(
        violations.is_empty(),
        "{}: {violations:?} not stated as blocked",
        page.file_name
    );
}

#[test]
fn a_form_named_outside_any_blocked_paragraph_is_reported() {
    let body = "\
`sed -i` is a common way to edit a file in place; nothing here relates to the hook.

This hook blocks a bare `cat`, `sed -n` and `grep` read of a repo file.
";

    let violations = blocked_form_violations(body, &["cat", "sed -n", "grep", "sed -i"]);

    assert_eq!(violations, vec!["sed -i".to_owned()]);
}

#[test]
fn every_docs_page_frontmatter_is_valid() {
    let pages = load_pages();
    let fronts: Vec<(String, String)> = pages
        .into_iter()
        .map(|page| (page.file_name, page.front))
        .collect();

    let violations = frontmatter_violations(&fronts);

    assert!(violations.is_empty(), "{violations:#?}");
}

#[test]
fn a_duplicated_order_and_an_overlong_description_are_reported() {
    let pages = vec![
        (
            "a.md".to_owned(),
            "title: A\ndescription: short\norder: 1\ngroup: Verbs".to_owned(),
        ),
        (
            "b.md".to_owned(),
            format!(
                "title: B\ndescription: {}\norder: 1\ngroup: Setup",
                "x".repeat(200)
            ),
        ),
    ];

    let violations = frontmatter_violations(&pages);

    assert!(
        violations
            .iter()
            .any(|v| v.contains("a.md") && v.contains("b.md") && v.contains("order 1"))
    );
    assert!(
        violations
            .iter()
            .any(|v| v.contains("b.md") && v.contains("description"))
    );
}
