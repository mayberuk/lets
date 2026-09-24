use std::sync::OnceLock;

use tree_sitter_language::LanguageFn;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Bash,
    Go,
    JavaScript,
    Json,
    Markdown,
    Python,
    Rust,
    Toml,
    Tsx,
    TypeScript,
    Yaml,
    C,
    Cpp,
    CSharp,
    Java,
    Php,
    Ruby,
    Swift,
}

/// `ALL[i] as usize == i` must hold: `language` indexes its cells by `lang as usize`.
pub const ALL: &[Language] = &[
    Language::Bash,
    Language::Go,
    Language::JavaScript,
    Language::Json,
    Language::Markdown,
    Language::Python,
    Language::Rust,
    Language::Toml,
    Language::Tsx,
    Language::TypeScript,
    Language::Yaml,
    Language::C,
    Language::Cpp,
    Language::CSharp,
    Language::Java,
    Language::Php,
    Language::Ruby,
    Language::Swift,
];

pub fn name(lang: Language) -> &'static str {
    match lang {
        Language::Bash => "bash",
        Language::Go => "go",
        Language::JavaScript => "javascript",
        Language::Json => "json",
        Language::Markdown => "markdown",
        Language::Python => "python",
        Language::Rust => "rust",
        Language::Toml => "toml",
        Language::Tsx => "tsx",
        Language::TypeScript => "typescript",
        Language::Yaml => "yaml",
        Language::C => "c",
        Language::Cpp => "cpp",
        Language::CSharp => "csharp",
        Language::Java => "java",
        Language::Php => "php",
        Language::Ruby => "ruby",
        Language::Swift => "swift",
    }
}

pub fn from_extension(ext: &str) -> Option<Language> {
    match ext {
        "sh" | "bash" => Some(Language::Bash),
        "go" => Some(Language::Go),
        "js" | "mjs" | "cjs" => Some(Language::JavaScript),
        "json" | "jsonc" => Some(Language::Json),
        "md" | "markdown" => Some(Language::Markdown),
        "py" => Some(Language::Python),
        "rs" => Some(Language::Rust),
        "toml" => Some(Language::Toml),
        "tsx" => Some(Language::Tsx),
        "ts" => Some(Language::TypeScript),
        "yaml" | "yml" => Some(Language::Yaml),
        "c" | "h" => Some(Language::C),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Some(Language::Cpp),
        "cs" => Some(Language::CSharp),
        "java" => Some(Language::Java),
        "php" => Some(Language::Php),
        "rb" => Some(Language::Ruby),
        "swift" => Some(Language::Swift),
        _ => None,
    }
}

/// Initialises only this grammar: `lets guide` has a 5 ms startup budget.
pub fn language(lang: Language) -> &'static tree_sitter::Language {
    cell(lang).get_or_init(|| tree_sitter::Language::new(language_fn(lang)))
}

fn cell(lang: Language) -> &'static OnceLock<tree_sitter::Language> {
    static CELLS: [OnceLock<tree_sitter::Language>; ALL.len()] =
        [const { OnceLock::new() }; ALL.len()];
    &CELLS[lang as usize]
}

fn language_fn(lang: Language) -> LanguageFn {
    match lang {
        Language::Bash => tree_sitter_bash::LANGUAGE,
        Language::Go => tree_sitter_go::LANGUAGE,
        Language::JavaScript => tree_sitter_javascript::LANGUAGE,
        Language::Json => tree_sitter_json::LANGUAGE,
        // Block grammar only: headings live there, and the `parser` feature links a second runtime.
        Language::Markdown => tree_sitter_md::LANGUAGE,
        Language::Python => tree_sitter_python::LANGUAGE,
        Language::Rust => tree_sitter_rust::LANGUAGE,
        Language::Toml => tree_sitter_toml_ng::LANGUAGE,
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX,
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
        Language::Yaml => tree_sitter_yaml::LANGUAGE,
        Language::C => tree_sitter_c::LANGUAGE,
        Language::Cpp => tree_sitter_cpp::LANGUAGE,
        Language::CSharp => tree_sitter_c_sharp::LANGUAGE,
        Language::Java => tree_sitter_java::LANGUAGE,
        // Not `LANGUAGE_PHP_ONLY`: a `.php` file opens with `<?php` and may carry inline HTML.
        Language::Php => tree_sitter_php::LANGUAGE_PHP,
        Language::Ruby => tree_sitter_ruby::LANGUAGE,
        Language::Swift => tree_sitter_swift::LANGUAGE,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tree_sitter::{Node, Parser};

    use super::{ALL, Language, cell, from_extension, language, name};
    use crate::own_process::workdir;

    const PARSE_CASES: &[(Language, &str, &str)] = &[
        (Language::Bash, "echo hello\nls -la\ncd /tmp\n", "command"),
        (
            Language::Go,
            "package main\n\nfunc main() {}\n",
            "function_declaration",
        ),
        (
            Language::JavaScript,
            "export function show(path) {\n  return path;\n}\n",
            "function_declaration",
        ),
        (Language::Json, "{\n  \"window\": 200\n}\n", "pair"),
        (
            Language::Markdown,
            "# Targets\n\nOne grammar every verb speaks.\n",
            "atx_heading",
        ),
        (
            Language::Python,
            "def show(path):\n    return path\n",
            "function_definition",
        ),
        (
            Language::Rust,
            "fn show(path: &str) -> &str {\n    path\n}\n",
            "function_item",
        ),
        (Language::Toml, "[package]\nname = \"lets\"\n", "table"),
        (
            Language::Tsx,
            "export const Badge = () => (\n  <span />\n);\n",
            "jsx_self_closing_element",
        ),
        (
            Language::TypeScript,
            "export interface Target {\n  path: string;\n}\n",
            "interface_declaration",
        ),
        (
            Language::Yaml,
            "name: lets\nitems:\n  - one\n",
            "block_mapping_pair",
        ),
        (
            Language::C,
            "int show(const char *path) {\n    return 0;\n}\n",
            "function_definition",
        ),
        (
            Language::Cpp,
            "namespace lets {\nclass Store {\n  void open();\n};\n}\n",
            "class_specifier",
        ),
        (
            Language::CSharp,
            "class Store {\n    void Open() {}\n}\n",
            "method_declaration",
        ),
        (
            Language::Java,
            "class Store {\n    void open() {}\n}\n",
            "method_declaration",
        ),
        (
            Language::Php,
            "<?php\nclass Store {\n    function open() {}\n}\n",
            "class_declaration",
        ),
        (
            Language::Ruby,
            "class Store\n  def open\n  end\nend\n",
            "method",
        ),
        (
            Language::Swift,
            "func show(path: String) -> String {\n    return path\n}\n",
            "function_declaration",
        ),
    ];

    fn has_named_kind(node: Node, kind: &str) -> bool {
        if node.is_named() && node.kind() == kind {
            return true;
        }
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .any(|child| has_named_kind(child, kind))
    }

    fn parse(lang: Language, source: &str) -> tree_sitter::Tree {
        let mut parser = Parser::new();
        parser
            .set_language(language(lang))
            .unwrap_or_else(|e| panic!("{} grammar rejected by the runtime: {e}", name(lang)));
        parser
            .parse(source, None)
            .unwrap_or_else(|| panic!("{} parser returned no tree", name(lang)))
    }

    #[test]
    fn all_lists_every_language_the_spec_bundles() {
        assert_eq!(ALL, &[
            Language::Bash,
            Language::Go,
            Language::JavaScript,
            Language::Json,
            Language::Markdown,
            Language::Python,
            Language::Rust,
            Language::Toml,
            Language::Tsx,
            Language::TypeScript,
            Language::Yaml,
            Language::C,
            Language::Cpp,
            Language::CSharp,
            Language::Java,
            Language::Php,
            Language::Ruby,
            Language::Swift,
        ]);
        for (i, &lang) in ALL.iter().enumerate() {
            assert_eq!(
                lang as usize,
                i,
                "{} is out of declaration order",
                name(lang)
            );
        }
    }

    #[test]
    fn every_bundled_grammar_parses_its_language_without_an_error_node() {
        let covered: Vec<Language> = PARSE_CASES.iter().map(|&(lang, _, _)| lang).collect();
        assert_eq!(covered, ALL, "a bundled language has no parse case");

        for &(lang, source, kind) in PARSE_CASES {
            let tree = parse(lang, source);
            assert!(
                !tree.root_node().has_error(),
                "{} parsed its own snippet with an error node",
                name(lang)
            );
            assert!(
                has_named_kind(tree.root_node(), kind),
                "{} parsed without a named `{kind}` node",
                name(lang)
            );
        }
    }

    #[test]
    fn a_broken_snippet_reports_an_error_node() {
        let tree = parse(Language::Rust, "fn show( {\n    path\n}\n");
        assert!(tree.root_node().has_error());
    }

    #[test]
    fn a_jsonc_comment_parses_under_the_json_grammar() {
        let lang = from_extension("jsonc").expect("jsonc maps to a grammar");
        let tree = parse(lang, "{\n  // the window size\n  \"window\": 200\n}\n");
        assert!(!tree.root_node().has_error());
        assert!(has_named_kind(tree.root_node(), "comment"));
        assert!(has_named_kind(tree.root_node(), "pair"));

        let broken = parse(lang, "{\n  // the window size\n  \"window\": \n}\n");
        assert!(broken.root_node().has_error());
    }

    #[test]
    fn a_kind_the_snippet_does_not_contain_is_not_found() {
        let tree = parse(Language::Json, "{\n  \"window\": 200\n}\n");
        assert!(!has_named_kind(tree.root_node(), "function_item"));
    }

    /// Runs in its own process: a sibling test may already have initialised another cell.
    #[test]
    fn language_initialises_only_the_language_asked_for() {
        if workdir(Path::to_path_buf).is_none() {
            return;
        }
        let asked = Language::Json;
        let _ = language(asked);
        for &other in ALL {
            if other == asked {
                continue;
            }
            assert!(
                cell(other).get().is_none(),
                "{} was initialised alongside {}",
                name(other),
                name(asked)
            );
        }
    }

    #[test]
    fn every_spec_extension_maps_to_its_language() {
        let cases = [
            ("sh", Language::Bash),
            ("bash", Language::Bash),
            ("go", Language::Go),
            ("js", Language::JavaScript),
            ("mjs", Language::JavaScript),
            ("cjs", Language::JavaScript),
            ("json", Language::Json),
            ("jsonc", Language::Json),
            ("md", Language::Markdown),
            ("markdown", Language::Markdown),
            ("py", Language::Python),
            ("rs", Language::Rust),
            ("toml", Language::Toml),
            ("tsx", Language::Tsx),
            ("ts", Language::TypeScript),
            ("yaml", Language::Yaml),
            ("yml", Language::Yaml),
            ("c", Language::C),
            ("h", Language::C),
            ("cpp", Language::Cpp),
            ("cc", Language::Cpp),
            ("cxx", Language::Cpp),
            ("hpp", Language::Cpp),
            ("hh", Language::Cpp),
            ("hxx", Language::Cpp),
            ("cs", Language::CSharp),
            ("java", Language::Java),
            ("php", Language::Php),
            ("rb", Language::Ruby),
            ("swift", Language::Swift),
        ];
        for (ext, expected) in cases {
            assert_eq!(from_extension(ext), Some(expected), ".{ext}");
        }
    }

    #[test]
    fn an_extension_with_no_bundled_grammar_is_none() {
        for ext in ["vue", "kt", "", "ts.snap", ".rs", "RS"] {
            assert_eq!(from_extension(ext), None, ".{ext}");
        }
    }
}
