/// The eleven grammars the binary bundles, in the order `src/grammars.rs` declares them.
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
}

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
];

impl Language {
    pub fn name(self) -> &'static str {
        match self {
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
        }
    }

    /// One extension per language, each of them one `src/grammars.rs` `from_extension` maps.
    pub fn extension(self) -> &'static str {
        match self {
            Language::Bash => "sh",
            Language::Go => "go",
            Language::JavaScript => "js",
            Language::Json => "json",
            Language::Markdown => "md",
            Language::Python => "py",
            Language::Rust => "rs",
            Language::Toml => "toml",
            Language::Tsx => "tsx",
            Language::TypeScript => "ts",
            Language::Yaml => "yaml",
        }
    }

    /// Go is the only bundled language whose file is not a bare sequence of declarations.
    fn preamble(self) -> &'static str {
        match self {
            Language::Go => "package unit\n\n",
            _ => "",
        }
    }
}

/// Grows past `target_bytes` by at most one unit; the caller's envelope is a mean, not a cap.
pub fn file(lang: Language, target_bytes: usize) -> String {
    if lang == Language::Json {
        return json_file(json_object_count(target_bytes));
    }

    let unit: fn(usize) -> String = match lang {
        Language::Bash => bash,
        Language::Go => go,
        Language::JavaScript => javascript,
        Language::Markdown => markdown,
        Language::Python => python,
        Language::Rust => rust,
        Language::Toml => toml,
        Language::Tsx => tsx,
        Language::TypeScript => typescript,
        Language::Yaml => yaml,
        Language::Json => unreachable!("json builds one root value, handled above"),
    };

    let mut out = String::with_capacity(target_bytes + 128);
    out.push_str(lang.preamble());
    let mut index = 0;
    while out.len() < target_bytes {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&unit(index));
        index += 1;
    }
    out
}

pub fn bash(index: usize) -> String {
    format!("unit_{index}() {{\n  echo \"unit {index}\"\n}}\n")
}

pub fn go(index: usize) -> String {
    format!("func unit_{index}(x int) int {{\n\treturn x + {index}\n}}\n")
}

pub fn javascript(index: usize) -> String {
    format!("export function unit_{index}(x) {{\n  return x + {index};\n}}\n")
}

pub fn markdown(index: usize) -> String {
    format!("## unit {index}\n\nParagraph body for unit {index}.\n")
}

pub fn python(index: usize) -> String {
    format!("def unit_{index}(x):\n    return x + {index}\n")
}

pub fn rust(index: usize) -> String {
    format!("pub fn unit_{index}(x: i64) -> i64 {{\n    x + {index}\n}}\n")
}

pub fn toml(index: usize) -> String {
    format!("[unit_{index}]\nname = \"unit-{index}\"\nvalue = {index}\n")
}

pub fn tsx(index: usize) -> String {
    format!(
        "export function Unit{index}() {{\n  return <span className=\"unit-{index}\">unit {index}</span>;\n}}\n"
    )
}

pub fn typescript(index: usize) -> String {
    format!("export function unit_{index}(x: number): number {{\n  return x + {index};\n}}\n")
}

pub fn yaml(index: usize) -> String {
    format!("unit_{index}:\n  name: unit-{index}\n  value: {index}\n")
}

/// A JSON document holds one root value, so the N declarations go in one root array.
pub fn json_file(count: usize) -> String {
    let mut out = String::from("[\n");
    for index in 0..count {
        let comma = if index + 1 == count { "" } else { "," };
        out.push_str(&format!(
            "  {{\"index\": {index}, \"value\": \"unit-{index}\"}}{comma}\n"
        ));
    }
    out.push_str("]\n");
    out
}

/// Fixed punctuation bytes per object line, plus the index twice.
const JSON_OBJECT_FIXED_BYTES: usize = 33;

/// `[\n` and `]\n` less the last object's comma, so the sum below is exact.
fn json_object_count(target_bytes: usize) -> usize {
    let mut len = 3;
    let mut count = 0;
    while len < target_bytes {
        len += JSON_OBJECT_FIXED_BYTES + 2 * decimal_digits(count);
        count += 1;
    }
    count.max(1)
}

fn decimal_digits(n: usize) -> usize {
    if n == 0 { 1 } else { n.ilog10() as usize + 1 }
}

#[cfg(test)]
mod tests {
    use super::{ALL, Language, file, json_file, json_object_count};

    #[test]
    fn every_language_has_its_own_name_and_extension() {
        let mut names: Vec<&str> = ALL.iter().map(|l| l.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ALL.len(), "two languages share a name");

        let mut extensions: Vec<&str> = ALL.iter().map(|l| l.extension()).collect();
        extensions.sort_unstable();
        extensions.dedup();
        assert_eq!(
            extensions.len(),
            ALL.len(),
            "two languages share an extension"
        );
    }

    #[test]
    fn a_file_reaches_its_target_without_overshooting_by_more_than_one_unit() {
        for &lang in ALL {
            for target in [2048, 6144, 10_239] {
                let text = file(lang, target);
                assert!(
                    text.len() >= target,
                    "{} stopped short of {target}",
                    lang.name()
                );
                assert!(
                    text.len() < target + 512,
                    "{} overshot {target} by more than one unit: {}",
                    lang.name(),
                    text.len()
                );
            }
        }
    }

    #[test]
    fn units_within_a_file_carry_distinct_identifiers() {
        for &lang in ALL {
            let text = file(lang, 2048);
            assert!(
                text.contains("unit-0") || text.contains("unit_0") || text.contains("unit 0"),
                "{} has no first unit",
                lang.name()
            );
            assert!(
                text.contains("unit-7") || text.contains("unit_7") || text.contains("unit 7"),
                "{} has no eighth unit",
                lang.name()
            );
        }
    }

    #[test]
    fn a_go_file_opens_with_its_package_clause() {
        assert!(file(Language::Go, 2048).starts_with("package unit\n"));
    }

    #[test]
    fn no_other_language_carries_a_go_package_clause() {
        for &lang in ALL {
            if lang == Language::Go {
                continue;
            }
            assert!(
                !file(lang, 2048).contains("package unit"),
                "{} grew a Go preamble",
                lang.name()
            );
        }
    }

    #[test]
    fn a_json_file_is_one_root_array_with_no_trailing_comma() {
        let text = json_file(3);
        assert!(text.starts_with("[\n") && text.ends_with("]\n"));
        assert!(!text.contains(",\n]"));
        assert_eq!(text.matches("\"index\"").count(), 3);
    }

    #[test]
    fn a_json_file_of_one_object_separates_nothing() {
        assert_eq!(json_object_count(0), 1);
        assert!(!json_file(1).contains("},"));
    }

    #[test]
    fn the_object_line_estimate_matches_the_renderer() {
        for count in [1, 2, 9, 10, 11, 99, 100, 101, 1000] {
            let predicted: usize = 3
                + (0..count)
                    .map(|i| super::JSON_OBJECT_FIXED_BYTES + 2 * super::decimal_digits(i))
                    .sum::<usize>();
            assert_eq!(json_file(count).len(), predicted, "count {count}");
        }
    }

    #[test]
    fn decimal_digits_counts_digits() {
        for (n, expected) in [(0, 1), (9, 1), (10, 2), (99, 2), (100, 3), (1234, 4)] {
            assert_eq!(super::decimal_digits(n), expected, "{n}");
        }
    }
}
