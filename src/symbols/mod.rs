//! Which exit code "not found" or "ambiguous" becomes is the verb's decision, never this module's.

mod bash;
mod c;
mod cpp;
mod csharp;
mod go;
mod java;
mod javascript;
mod json;
mod markdown;
mod php;
mod python;
mod ruby;
mod rust;
mod swift;
mod toml;
mod tsx;
mod typescript;
mod yaml;

pub mod plaintext;

use std::collections::HashMap;

use tree_sitter::{Node, Parser, Query, QueryCursor, QueryMatch, StreamingIterator as _};

use crate::grammars::{self, Language};
use crate::output::Resolver;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolMatch {
    /// Byte offsets, end exclusive.
    pub start: usize,
    pub end: usize,
    /// 1-based, inclusive.
    pub line: usize,
    pub end_line: usize,
    pub text: String,
    pub resolver: Resolver,
    /// Set when the end came from indentation or the hit line: an insert after it is a guess.
    pub end_guessed: bool,
}

pub fn resolve(lang: Language, content: &str, path: &[String]) -> Vec<SymbolMatch> {
    if path.is_empty() {
        return Vec::new();
    }
    let query = match lang {
        Language::Markdown => return markdown::resolve(content, path),
        Language::Bash => bash::QUERY,
        Language::Go => go::QUERY,
        Language::JavaScript => javascript::QUERY,
        Language::Json => json::QUERY,
        Language::Python => python::QUERY,
        Language::Rust => rust::QUERY,
        Language::Toml => toml::QUERY,
        Language::Tsx => tsx::QUERY,
        Language::TypeScript => typescript::QUERY,
        Language::Yaml => yaml::QUERY,
        Language::C => c::QUERY,
        Language::Cpp => cpp::QUERY,
        Language::CSharp => csharp::QUERY,
        Language::Java => java::QUERY,
        Language::Php => php::QUERY,
        Language::Ruby => ruby::QUERY,
        Language::Swift => swift::QUERY,
    };
    resolve_query(lang, query, content, path)
}

struct Definition<'a, 'tree> {
    node: Node<'tree>,
    name: &'a str,
    /// Named by the definition's own match rather than an ancestor node: a Go method's receiver.
    own_scope: Option<&'a str>,
}

fn resolve_query(
    lang: Language,
    query_source: &str,
    content: &str,
    path: &[String],
) -> Vec<SymbolMatch> {
    let language = grammars::language(lang);
    let mut parser = Parser::new();
    parser
        .set_language(language)
        .expect("a bundled grammar's ABI matches the linked runtime");
    let Some(tree) = parser.parse(content, None) else {
        return Vec::new();
    };
    let query = Query::new(language, query_source).unwrap_or_else(|e| {
        panic!(
            "the {} symbol query is malformed: {e}",
            grammars::name(lang)
        )
    });
    let (def, name) = (
        query.capture_index_for_name("def"),
        query.capture_index_for_name("name"),
    );
    let (scope, scope_name) = (
        query.capture_index_for_name("scope"),
        query.capture_index_for_name("scope.name"),
    );
    let own_scope = query.capture_index_for_name("self.scope");

    let source = content.as_bytes();
    let mut scopes: HashMap<usize, &str> = HashMap::new();
    let mut definitions: Vec<Definition> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source);
    // Scopes are collected first: nothing guarantees a container's match precedes its definitions.
    while let Some(matched) = matches.next() {
        if let (Some(node), Some(text)) = (
            captured(matched, scope),
            captured_text(matched, scope_name, source),
        ) {
            scopes.insert(node.id(), text);
        }
        if let (Some(node), Some(text)) =
            (captured(matched, def), captured_text(matched, name, source))
        {
            definitions.push(Definition {
                node,
                name: text,
                own_scope: captured_text(matched, own_scope, source),
            });
        }
    }

    let mut found: Vec<SymbolMatch> = definitions
        .iter()
        .filter(|definition| ends_with(&chain(definition, &scopes), path))
        .map(|definition| {
            let span = definition.node.byte_range();
            SymbolMatch {
                line: definition.node.start_position().row + 1,
                end_line: definition.node.end_position().row + 1,
                text: line_text(content, span.start),
                start: span.start,
                end: span.end,
                resolver: Resolver::TreeSitter,
                end_guessed: false,
            }
        })
        .collect();
    found.sort_by_key(|found| (found.start, found.end));
    found
}

fn captured<'tree>(matched: &QueryMatch<'_, 'tree>, index: Option<u32>) -> Option<Node<'tree>> {
    let index = index?;
    matched
        .captures
        .iter()
        .find(|capture| capture.index == index)
        .map(|capture| capture.node)
}

fn captured_text<'a>(
    matched: &QueryMatch<'_, '_>,
    index: Option<u32>,
    source: &'a [u8],
) -> Option<&'a str> {
    captured(matched, index)?.utf8_text(source).ok()
}

fn chain<'a>(definition: &Definition<'a, '_>, scopes: &HashMap<usize, &'a str>) -> Vec<&'a str> {
    let mut chain = Vec::new();
    let mut ancestor = definition.node.parent();
    while let Some(node) = ancestor {
        if let Some(name) = scopes.get(&node.id()) {
            chain.push(*name);
        }
        ancestor = node.parent();
    }
    chain.reverse();
    chain.extend(definition.own_scope);
    chain.push(definition.name);
    chain
}

fn ends_with(chain: &[&str], path: &[String]) -> bool {
    chain.len() >= path.len()
        && chain[chain.len() - path.len()..]
            .iter()
            .zip(path)
            .all(|(segment, wanted)| *segment == wanted.as_str())
}

fn line_text(content: &str, start: usize) -> String {
    let line_start = content[..start].rfind('\n').map_or(0, |at| at + 1);
    let rest = &content[line_start..];
    let end = rest.find('\n').unwrap_or(rest.len());
    let line = &rest[..end];
    line.strip_suffix('\r').unwrap_or(line).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segments(path: &[&str]) -> Vec<String> {
        path.iter().map(|s| (*s).to_owned()).collect()
    }

    fn find(lang: Language, content: &str, path: &[&str]) -> Vec<SymbolMatch> {
        resolve(lang, content, &segments(path))
    }

    const RUST_SHOW: &str = "fn show(path: &str) -> &str {\n    path\n}\n";

    #[test]
    fn a_rust_function_resolves_to_one_match_spanning_the_whole_function() {
        let found = find(Language::Rust, RUST_SHOW, &["show"]);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].start, 0);
        assert_eq!(found[0].end, RUST_SHOW.len() - 1);
        assert_eq!(
            &RUST_SHOW[found[0].start..found[0].end],
            RUST_SHOW.trim_end()
        );
        assert_eq!(found[0].line, 1);
        assert_eq!(found[0].end_line, 3);
        assert_eq!(found[0].text, "fn show(path: &str) -> &str {");
        assert_eq!(found[0].resolver, Resolver::TreeSitter);
    }

    #[test]
    fn a_name_no_definition_carries_is_not_found() {
        assert!(find(Language::Rust, RUST_SHOW, &["hide"]).is_empty());
    }

    const GO_FREE_FUNCTION: &str = "func Open(path string) (*Store, error) {";
    const GO_METHOD: &str = "func (s *Store) Open(ctx context.Context) error {";

    fn go_source() -> String {
        fn pad_to(lines: &mut Vec<String>, line: usize) {
            while lines.len() < line - 1 {
                lines.push("// filler".to_owned());
            }
        }
        let mut lines = vec!["package main".to_owned()];
        pad_to(&mut lines, 44);
        lines.push(GO_FREE_FUNCTION.to_owned());
        lines.push("\treturn nil, nil".to_owned());
        lines.push("}".to_owned());
        pad_to(&mut lines, 213);
        lines.push(GO_METHOD.to_owned());
        lines.push("\treturn nil".to_owned());
        lines.push("}".to_owned());
        lines.join("\n") + "\n"
    }

    #[test]
    fn a_go_name_on_two_definitions_returns_both_with_their_own_lines() {
        let source = go_source();
        let found = find(Language::Go, &source, &["Open"]);

        assert_eq!(found.len(), 2);
        assert_eq!((found[0].line, found[0].end_line), (44, 46));
        assert_eq!(found[0].text, GO_FREE_FUNCTION);
        assert_eq!((found[1].line, found[1].end_line), (213, 215));
        assert_eq!(found[1].text, GO_METHOD);
    }

    #[test]
    fn naming_the_receiver_keeps_only_the_method_on_it() {
        let source = go_source();
        let found = find(Language::Go, &source, &["Store", "Open"]);

        assert_eq!(found.len(), 1);
        assert_eq!((found[0].line, found[0].end_line), (213, 215));
        assert_eq!(found[0].text, GO_METHOD);
    }

    #[test]
    fn a_receiver_no_method_has_is_not_found() {
        let source = go_source();
        assert!(find(Language::Go, &source, &["Cache", "Open"]).is_empty());
    }

    const DOC: &str = "\
# Targets

One grammar every verb speaks.

## Bottom line

Lines locate, content confirms.

```sh
# not a heading
```

## Next

Something else.
";

    #[test]
    fn a_heading_runs_to_the_line_before_the_next_heading_of_its_level_or_higher() {
        let found = find(Language::Markdown, DOC, &["Bottom line"]);

        assert_eq!(found.len(), 1);
        assert_eq!((found[0].line, found[0].end_line), (5, 12));
        assert_eq!(found[0].text, "## Bottom line");
        assert_eq!(found[0].resolver, Resolver::Heuristic("heading"));

        let section = &DOC[found[0].start..found[0].end];
        assert!(section.starts_with("## Bottom line\n"));
        assert!(
            section.contains("# not a heading"),
            "a fenced comment is not a heading and may not end the section: {section:?}"
        );
        assert!(
            !section.contains("## Next"),
            "the section stops before the next heading: {section:?}"
        );
        assert_eq!(DOC[found[0].end..].lines().next(), Some(""));
    }

    #[test]
    fn the_last_heading_runs_to_the_end_of_the_file() {
        let found = find(Language::Markdown, DOC, &["Next"]);

        assert_eq!(found.len(), 1);
        assert_eq!((found[0].line, found[0].end_line), (13, 15));
        assert_eq!(
            &DOC[found[0].start..found[0].end],
            "## Next\n\nSomething else."
        );
    }

    #[test]
    fn a_deeper_heading_does_not_end_a_shallower_section() {
        let found = find(Language::Markdown, DOC, &["Targets"]);

        assert_eq!(found.len(), 1);
        let section = &DOC[found[0].start..found[0].end];
        assert!(
            section.contains("## Bottom line"),
            "a level-2 heading is inside its level-1 section: {section:?}"
        );
    }

    #[test]
    fn a_heading_the_document_does_not_carry_is_not_found() {
        assert!(find(Language::Markdown, DOC, &["Bottom"]).is_empty());
        assert!(find(Language::Markdown, DOC, &["not a heading"]).is_empty());
        assert!(find(Language::Markdown, DOC, &["Targets", "Bottom line"]).is_empty());
    }

    struct Case {
        lang: Language,
        source: &'static str,
        path: &'static [&'static str],
        line: usize,
        end_line: usize,
        span: &'static str,
    }

    const PER_LANGUAGE: &[Case] = &[
        Case {
            lang: Language::Bash,
            source: "#!/usr/bin/env bash\nrun() {\n  echo hi\n}\n",
            path: &["run"],
            line: 2,
            end_line: 4,
            span: "run() {\n  echo hi\n}",
        },
        Case {
            lang: Language::Go,
            source: "package main\n\ntype Store struct{}\n",
            path: &["Store"],
            line: 3,
            end_line: 3,
            span: "type Store struct{}",
        },
        Case {
            lang: Language::JavaScript,
            source: "export class Store {\n  open() {\n    return 1;\n  }\n}\n",
            path: &["Store", "open"],
            line: 2,
            end_line: 4,
            span: "open() {\n    return 1;\n  }",
        },
        Case {
            lang: Language::Json,
            source: "{\n  \"window\": {\n    \"size\": 200\n  }\n}\n",
            path: &["window", "size"],
            line: 3,
            end_line: 3,
            span: "\"size\": 200",
        },
        Case {
            lang: Language::Python,
            source: "class Store:\n    def open(self):\n        pass\n",
            path: &["Store", "open"],
            line: 2,
            end_line: 3,
            span: "def open(self):\n        pass",
        },
        Case {
            lang: Language::Rust,
            source: "struct Store;\n\nimpl Store {\n    fn open(&self) {}\n}\n",
            path: &["Store", "open"],
            line: 4,
            end_line: 4,
            span: "fn open(&self) {}",
        },
        Case {
            lang: Language::Toml,
            source: "[package]\nname = \"lets\"\n",
            path: &["package", "name"],
            line: 2,
            end_line: 2,
            span: "name = \"lets\"",
        },
        Case {
            lang: Language::Tsx,
            source: "export class Badge {\n  render() {\n    return null;\n  }\n}\n",
            path: &["Badge", "render"],
            line: 2,
            end_line: 4,
            span: "render() {\n    return null;\n  }",
        },
        Case {
            lang: Language::TypeScript,
            source: "export interface Target {\n  path: string;\n}\n",
            path: &["Target"],
            line: 1,
            end_line: 3,
            span: "interface Target {\n  path: string;\n}",
        },
        Case {
            lang: Language::Yaml,
            source: "name: lets\nwindow:\n  size: 200\n",
            path: &["window", "size"],
            line: 3,
            end_line: 3,
            span: "size: 200",
        },
        Case {
            lang: Language::Markdown,
            source: "# Targets\n\nOne grammar.\n",
            path: &["Targets"],
            line: 1,
            end_line: 3,
            span: "# Targets\n\nOne grammar.",
        },
        Case {
            lang: Language::C,
            source: "struct Store {\n    int fd;\n};\n\nint open_store(struct Store *s) {\n    return \
                     s->fd;\n}\n",
            path: &["open_store"],
            line: 5,
            end_line: 7,
            span: "int open_store(struct Store *s) {\n    return s->fd;\n}",
        },
        Case {
            lang: Language::Cpp,
            source: "namespace lets {\nclass Store {\n  void open() {}\n};\n}\n",
            path: &["lets", "Store", "open"],
            line: 3,
            end_line: 3,
            span: "void open() {}",
        },
        Case {
            lang: Language::CSharp,
            source: "namespace Lets {\n    class Store {\n        void Open() {}\n    }\n}\n",
            path: &["Lets", "Store", "Open"],
            line: 3,
            end_line: 3,
            span: "void Open() {}",
        },
        Case {
            lang: Language::Java,
            source: "class Store {\n    void open() {\n        return;\n    }\n}\n",
            path: &["Store", "open"],
            line: 2,
            end_line: 4,
            span: "void open() {\n        return;\n    }",
        },
        Case {
            lang: Language::Php,
            source: "<?php\nclass Store {\n    public function open() {}\n}\n",
            path: &["Store", "open"],
            line: 3,
            end_line: 3,
            span: "public function open() {}",
        },
        Case {
            lang: Language::Ruby,
            source: "module Lets\n  class Store\n    def open\n    end\n  end\nend\n",
            path: &["Lets", "Store", "open"],
            line: 3,
            end_line: 4,
            span: "def open\n    end",
        },
        Case {
            lang: Language::Swift,
            source: "struct Store {\n    func open() {}\n}\n",
            path: &["Store", "open"],
            line: 2,
            end_line: 2,
            span: "func open() {}",
        },
    ];

    #[test]
    fn every_bundled_language_resolves_a_symbol_of_its_own() {
        let covered: Vec<Language> = PER_LANGUAGE.iter().map(|case| case.lang).collect();
        let mut sorted = covered.clone();
        sorted.sort_by_key(|lang| *lang as usize);
        sorted.dedup();
        assert_eq!(sorted, grammars::ALL, "a bundled language has no case");

        for case in PER_LANGUAGE {
            let Case {
                lang,
                source,
                path,
                line,
                end_line,
                span,
            } = *case;
            let found = find(lang, source, path);
            let name = grammars::name(lang);
            assert_eq!(found.len(), 1, "{name} resolved {path:?} to {found:?}");
            assert_eq!(found[0].line, line, "{name} start line");
            assert_eq!(found[0].end_line, end_line, "{name} end line");
            assert_eq!(
                found[0].start,
                source.find(span).expect("the fixture carries the span"),
                "{name} span start"
            );
            assert_eq!(&source[found[0].start..found[0].end], span, "{name} span");
        }
    }

    #[test]
    fn only_markdown_answers_with_a_heuristic() {
        for &Case {
            lang, source, path, ..
        } in PER_LANGUAGE
        {
            let expected = if lang == Language::Markdown {
                Resolver::Heuristic("heading")
            } else {
                Resolver::TreeSitter
            };
            assert_eq!(
                find(lang, source, path)[0].resolver,
                expected,
                "{}",
                grammars::name(lang)
            );
        }
    }

    #[test]
    fn a_container_the_definition_is_not_under_is_not_found() {
        for &Case {
            lang, source, path, ..
        } in PER_LANGUAGE
        {
            let mut wrong = vec!["elsewhere"];
            wrong.extend(path);
            assert!(
                find(lang, source, &wrong).is_empty(),
                "{} matched {wrong:?}",
                grammars::name(lang)
            );
        }
    }

    #[test]
    fn an_impl_block_names_its_methods_without_becoming_a_candidate_itself() {
        let source = "struct Store;\n\nimpl Store {\n    fn open(&self) {}\n}\n";
        let found = find(Language::Rust, source, &["Store"]);

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].line, 1);
        assert_eq!(found[0].text, "struct Store;");
    }

    #[test]
    fn a_c_struct_named_without_its_body_is_not_a_candidate() {
        let source = "struct Store {\n    int fd;\n};\n\nint open_store(struct Store *s);\n";
        let found = find(Language::C, source, &["Store"]);

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].line, 1);
        assert!(
            find(Language::C, "void close_store(struct Store *s);\n", &[
                "Store"
            ])
            .is_empty()
        );
    }

    #[test]
    fn a_c_function_returning_a_pointer_resolves_by_its_name() {
        let source = "char *name(void) {\n    return 0;\n}\n";
        let found = find(Language::C, source, &["name"]);

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(&source[found[0].start..found[0].end], source.trim_end());
    }

    #[test]
    fn a_c_typedef_answers_its_alias_once() {
        let same = "typedef struct Store {\n    int fd;\n} Store;\n";
        let found = find(Language::C, same, &["Store"]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            &same[found[0].start..found[0].end],
            "struct Store {\n    int fd;\n}"
        );

        let renamed = "typedef struct store_s {\n    int fd;\n} Store;\n";
        let found = find(Language::C, renamed, &["Store"]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(&renamed[found[0].start..found[0].end], renamed.trim_end());
        assert_eq!(find(Language::C, renamed, &["store_s"]).len(), 1);

        let anonymous = "typedef struct {\n    int fd;\n} Store;\n";
        let found = find(Language::C, anonymous, &["Store"]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            &anonymous[found[0].start..found[0].end],
            anonymous.trim_end()
        );
    }

    #[test]
    fn a_cpp_method_defined_outside_its_class_is_scoped_by_its_qualifier() {
        let source = "class Store {\n  void open();\n};\n\nvoid Store::open() {}\n";
        let found = find(Language::Cpp, source, &["Store", "open"]);

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].line, 5);
        assert_eq!(found[0].text, "void Store::open() {}");
        assert!(find(Language::Cpp, source, &["Cache", "open"]).is_empty());
    }

    #[test]
    fn a_constructor_does_not_make_its_class_ambiguous() {
        let java = "class Store {\n    Store() {}\n}\n";
        let csharp = "class Store {\n    public Store() {}\n}\n";
        for (lang, source) in [(Language::Java, java), (Language::CSharp, csharp)] {
            let found = find(lang, source, &["Store"]);
            assert_eq!(found.len(), 1, "{}: {found:?}", grammars::name(lang));
            assert_eq!(found[0].line, 1, "{}", grammars::name(lang));
        }
    }

    #[test]
    fn a_swift_extension_scopes_its_methods_without_becoming_a_candidate_itself() {
        let source = "struct Store {}\n\nextension Store {\n    func open() {}\n}\n";

        let found = find(Language::Swift, source, &["Store"]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].text, "struct Store {}");

        let found = find(Language::Swift, source, &["Store", "open"]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].line, 4);
        assert!(find(Language::Swift, source, &["Cache", "open"]).is_empty());
    }

    #[test]
    fn a_ruby_class_method_resolves_under_its_class() {
        let source = "class Store\n  def self.open\n  end\nend\n";
        let found = find(Language::Ruby, source, &["Store", "open"]);

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].line, found[0].end_line), (2, 3));
    }

    #[test]
    fn an_empty_path_names_nothing() {
        assert!(resolve(Language::Rust, RUST_SHOW, &[]).is_empty());
    }
}
