use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use corpusgen::languages::{ALL, Language};
use corpusgen::{
    GENERATOR_VERSION, Manifest, Spec, full, generate, is_up_to_date, manifest, small, specials,
};

const MIB: u64 = 1024 * 1024;

/// Left behind on failure for inspection. Every shape claim holds at either size, so only the
/// envelope and one determinism pair pay for `full()`.
fn corpus(name: &str, spec: &Spec) -> (PathBuf, Manifest) {
    let dir = scratch(name);
    let manifest = generate(&dir, spec).unwrap();
    (dir, manifest)
}

fn read_manifest(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// `tree-sitter-language` is the root's alone: no test here names it, and an unused
/// dev-dependency costs more than the drift it would catch.
fn tree_sitter_deps(manifest: &str) -> BTreeMap<String, String> {
    manifest
        .lines()
        .filter(|line| line.starts_with("tree-sitter") && !line.starts_with("tree-sitter-language"))
        .map(|line| {
            let (name, version) = line
                .split_once(" = ")
                .unwrap_or_else(|| panic!("not a dependency line: {line}"));
            (
                name.to_string(),
                version.trim().trim_matches('"').to_string(),
            )
        })
        .collect()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("corpusgen-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

/// The manifest is left out: it describes the tree rather than belonging to it.
fn walk(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut found = BTreeMap::new();
    collect(dir, dir, &mut found);
    found
}

fn collect(root: &Path, dir: &Path, found: &mut BTreeMap<String, Vec<u8>>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();

    for path in entries {
        if path.is_dir() {
            collect(root, &path, found);
        } else if path.file_name().unwrap() != manifest::FILE_NAME {
            let key = path
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            found.insert(key, fs::read(&path).unwrap());
        }
    }
}

fn grammar(lang: Language) -> tree_sitter::Language {
    let language_fn = match lang {
        Language::Bash => tree_sitter_bash::LANGUAGE,
        Language::Go => tree_sitter_go::LANGUAGE,
        Language::JavaScript => tree_sitter_javascript::LANGUAGE,
        Language::Json => tree_sitter_json::LANGUAGE,
        Language::Markdown => tree_sitter_md::LANGUAGE,
        Language::Python => tree_sitter_python::LANGUAGE,
        Language::Rust => tree_sitter_rust::LANGUAGE,
        Language::Toml => tree_sitter_toml_ng::LANGUAGE,
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX,
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
        Language::Yaml => tree_sitter_yaml::LANGUAGE,
    };
    tree_sitter::Language::new(language_fn)
}

fn parse(lang: Language, source: &[u8]) -> tree_sitter::Tree {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&grammar(lang)).unwrap();
    parser.parse(source, None).unwrap()
}

/// `has_error` is the assertion; this only names the offender when it fires.
fn first_error(tree: &tree_sitter::Tree) -> Option<String> {
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.is_error() || node.is_missing() {
            return Some(format!("{} at {:?}", node.kind(), node.start_position()));
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    None
}

fn filler_file(dir: &Path, lang: Language) -> PathBuf {
    dir.join(lang.name())
        .join("mod_0")
        .join(format!("{}_0.{}", lang.name(), lang.extension()))
}

fn special(dir: &Path, name: &str) -> PathBuf {
    dir.join(specials::DIR_NAME).join(name)
}

#[test]
fn the_full_profile_lands_inside_the_documented_file_and_byte_envelope() {
    let (dir, manifest) = corpus("envelope", &full());
    let files = walk(&dir);
    let bytes: u64 = files.values().map(|b| b.len() as u64).sum();

    assert!(
        (1800..=2200).contains(&files.len()),
        "{} files is outside 1800..=2200",
        files.len()
    );
    assert!(
        (18 * MIB..=22 * MIB).contains(&bytes),
        "{bytes} bytes is outside 18..=22 MiB"
    );
    assert_eq!(manifest.file_count, files.len());
    assert_eq!(manifest.total_bytes, bytes);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_small_profile_is_far_smaller_than_the_envelope() {
    let dir = scratch("small-envelope");
    let manifest = generate(&dir, &small()).unwrap();
    assert!(manifest.file_count < 100, "{}", manifest.file_count);
    assert!(manifest.total_bytes < 2 * MIB, "{}", manifest.total_bytes);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn two_runs_of_the_same_profile_and_seed_are_byte_identical() {
    let left_dir = scratch("identical-left");
    let right_dir = scratch("identical-right");
    let left_manifest = generate(&left_dir, &full()).unwrap();
    let right_manifest = generate(&right_dir, &full()).unwrap();

    let left = walk(&left_dir);
    let right = walk(&right_dir);

    assert_eq!(
        left.keys().collect::<Vec<_>>(),
        right.keys().collect::<Vec<_>>()
    );
    for (path, bytes) in &left {
        assert_eq!(bytes, &right[path], "{path}");
    }
    assert_eq!(left_manifest, right_manifest);

    fs::remove_dir_all(&left_dir).unwrap();
    fs::remove_dir_all(&right_dir).unwrap();
}

#[test]
fn a_different_seed_changes_at_least_one_files_bytes() {
    let default_dir = scratch("seed-default");
    let other_dir = scratch("seed-other");
    generate(&default_dir, &small()).unwrap();
    generate(&other_dir, &Spec { seed: 2, ..small() }).unwrap();

    let default = walk(&default_dir);
    let other = walk(&other_dir);

    assert_eq!(
        default.keys().collect::<Vec<_>>(),
        other.keys().collect::<Vec<_>>(),
        "the seed must not move files, only their bytes"
    );
    assert!(
        default.iter().any(|(path, bytes)| &other[path] != bytes),
        "a different seed produced an identical tree"
    );

    fs::remove_dir_all(&default_dir).unwrap();
    fs::remove_dir_all(&other_dir).unwrap();
}

#[test]
fn every_bundled_grammar_parses_its_corpus_file_without_an_error_node() {
    let (dir, _) = corpus("grammars", &small());
    for &lang in ALL {
        let path = filler_file(&dir, lang);
        let source = fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let tree = parse(lang, &source);
        assert!(
            !tree.root_node().has_error(),
            "{} file {} has {:?}",
            lang.name(),
            path.display(),
            first_error(&tree)
        );
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_search_target_file_parses_and_holds_its_symbol() {
    let (dir, _) = corpus("search-target", &small());
    let path = special(&dir, "search-target.ts");
    let source = fs::read(&path).unwrap();
    let tree = parse(Language::TypeScript, &source);
    assert!(!tree.root_node().has_error(), "{:?}", first_error(&tree));
    assert_eq!(
        String::from_utf8(source)
            .unwrap()
            .matches(specials::SEARCH_TARGET_SYMBOL)
            .count(),
        1
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_corrupted_corpus_file_does_report_an_error_node() {
    let (dir, _) = corpus("corrupted", &small());
    let mut source = fs::read(filler_file(&dir, Language::Rust)).unwrap();
    source.splice(0..0, b"pub fn (((".iter().copied());
    let tree = parse(Language::Rust, &source);
    assert!(tree.root_node().has_error());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_corpus_holds_a_vue_file_and_an_extension_no_grammar_claims() {
    let (dir, _) = corpus("unclaimed-extensions", &small());
    let claimed: Vec<&str> = ALL.iter().map(|l| l.extension()).collect();
    let files = walk(&dir);

    for extension in ["vue", "xyz"] {
        assert!(
            !claimed.contains(&extension),
            "a bundled grammar claims .{extension}, so it cannot exercise the skip path"
        );
        let matched: Vec<&String> = files
            .keys()
            .filter(|p| Path::new(p).extension().is_some_and(|e| e == extension))
            .collect();
        assert_eq!(matched.len(), 1, ".{extension}: {matched:?}");
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_corpus_holds_one_crlf_file_and_one_bom_file() {
    let (dir, _) = corpus("crlf-bom", &small());
    let files = walk(&dir);

    let crlf: Vec<&String> = files
        .iter()
        .filter(|(_, bytes)| bytes.windows(2).any(|w| w == b"\r\n"))
        .map(|(path, _)| path)
        .collect();
    assert_eq!(crlf.len(), 1, "{crlf:?}");

    let bom: Vec<&String> = files
        .iter()
        .filter(|(_, bytes)| bytes.starts_with(&[0xef, 0xbb, 0xbf]))
        .map(|(path, _)| path)
        .collect();
    assert_eq!(bom.len(), 1, "{bom:?}");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_corpus_holds_one_file_of_exactly_large_file_bytes_and_one_with_a_nul() {
    let (dir, _) = corpus("large-and-binary", &small());
    let files = walk(&dir);
    let large = usize::try_from(small().large_file_bytes).unwrap();

    let sized: Vec<&String> = files
        .iter()
        .filter(|(_, bytes)| bytes.len() == large)
        .map(|(path, _)| path)
        .collect();
    assert_eq!(sized.len(), 1, "{sized:?}");

    let binary: Vec<&String> = files
        .iter()
        .filter(|(_, bytes)| bytes[..bytes.len().min(8192)].contains(&0))
        .map(|(path, _)| path)
        .collect();
    assert_eq!(binary.len(), 1, "{binary:?}");
    fs::remove_dir_all(&dir).unwrap();
}

/// The witness is an existing file's mtime, not an added sentinel: an added file changes the
/// count the reuse branch now checks, so a sentinel would prove the opposite of a no-op.
#[test]
fn a_matching_manifest_makes_a_second_run_a_no_op() {
    let dir = scratch("idempotent");
    let first = generate(&dir, &small()).unwrap();

    let witness = filler_file(&dir, Language::Rust);
    let written_at = fs::metadata(&witness).unwrap().modified().unwrap();
    let before = walk(&dir);

    let second = generate(&dir, &small()).unwrap();

    assert_eq!(first, second);
    assert_eq!(walk(&dir), before);
    assert_eq!(
        fs::metadata(&witness).unwrap().modified().unwrap(),
        written_at,
        "the tree was rewritten"
    );

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_deleted_corpus_file_makes_the_next_run_regenerate() {
    let dir = scratch("deleted-file");
    let first = generate(&dir, &small()).unwrap();

    let victim = filler_file(&dir, Language::Rust);
    fs::remove_file(&victim).unwrap();

    let second = generate(&dir, &small()).unwrap();

    assert!(victim.exists(), "a tree missing a file was reused");
    assert_eq!(first, second, "the same spec rebuilds the same corpus");

    fs::remove_dir_all(&dir).unwrap();
}

/// The committed ignore file is there before the first run, so this checks it survives.
#[test]
fn the_output_directorys_own_ignore_file_is_neither_counted_nor_deleted() {
    let dir = scratch("ignore-file");
    fs::create_dir_all(&dir).unwrap();
    let ignore = dir.join(".gitignore");
    fs::write(&ignore, "*\n!.gitignore\n").unwrap();

    let first = generate(&dir, &small()).unwrap();
    assert!(ignore.exists(), "the first run deleted it");
    assert_eq!(
        first.file_count + 1,
        walk(&dir).len(),
        "the ignore file was counted as corpus"
    );
    assert!(
        is_up_to_date(&dir, &small()).unwrap(),
        "a tree with its ignore file present reads as incomplete"
    );

    fs::remove_file(filler_file(&dir, Language::Rust)).unwrap();
    assert_eq!(generate(&dir, &small()).unwrap(), first);
    assert!(ignore.exists(), "a regeneration took it with the tree");

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_directory_with_no_manifest_is_refused_not_deleted() {
    let dir = scratch("foreign");
    fs::create_dir_all(&dir).unwrap();
    let theirs = dir.join("someone-elses-work.txt");
    fs::write(&theirs, b"not the generator's").unwrap();

    let error = generate(&dir, &small()).unwrap_err();

    assert!(theirs.exists(), "a foreign tree was deleted");
    let message = error.to_string();
    assert!(message.contains(&dir.display().to_string()), "{message}");
    assert!(message.contains(manifest::FILE_NAME), "{message}");

    fs::remove_dir_all(&dir).unwrap();
}

/// A non-workspace crate cannot depend on `lets`, so its dev-dependencies copy the binary's
/// pins, only for the grammars the generator emits.
fn assert_generator_pins_match(
    binary: &BTreeMap<String, String>,
    generator: &BTreeMap<String, String>,
) {
    for (name, version) in generator {
        assert_eq!(
            binary.get(name),
            Some(version),
            "{name} is {version} in tests/corpusgen/Cargo.toml but {:?} in the root manifest",
            binary.get(name)
        );
    }
}

#[test]
fn every_generator_tree_sitter_dep_matches_the_binarys_version() {
    let binary = tree_sitter_deps(&read_manifest("../../Cargo.toml"));
    let generator = tree_sitter_deps(&read_manifest("Cargo.toml"));

    assert_eq!(generator.len(), 11, "{generator:?}");
    assert_generator_pins_match(&binary, &generator);
}

#[test]
fn a_mismatched_shared_version_fails_the_pin_check() {
    let binary = tree_sitter_deps(&read_manifest("../../Cargo.toml"));
    let mut generator = tree_sitter_deps(&read_manifest("Cargo.toml"));
    let (name, version) = generator
        .iter()
        .next()
        .map(|(n, v)| (n.clone(), v.clone()))
        .unwrap();
    generator.insert(name, format!("{version}-mismatched"));

    let result = std::panic::catch_unwind(|| assert_generator_pins_match(&binary, &generator));
    assert!(
        result.is_err(),
        "a mismatched pinned version must fail the check, not pass unnoticed"
    );
}

#[test]
fn a_stale_generator_version_regenerates_every_file() {
    let dir = scratch("stale-version");
    generate(&dir, &small()).unwrap();

    let sentinel = dir.join("sentinel.txt");
    fs::write(&sentinel, b"must not survive").unwrap();
    let path = dir.join(manifest::FILE_NAME);
    let stale = fs::read_to_string(&path).unwrap().replace(
        &format!("generator_version={GENERATOR_VERSION}"),
        &format!("generator_version={}", GENERATOR_VERSION + 1),
    );
    fs::write(&path, stale).unwrap();

    let rebuilt = generate(&dir, &small()).unwrap();

    assert!(!sentinel.exists(), "the tree was reused");
    assert_eq!(rebuilt.generator_version, GENERATOR_VERSION);
    assert_eq!(manifest::read(&dir), Some(rebuilt));

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_different_seed_regenerates_every_file() {
    let dir = scratch("stale-seed");
    generate(&dir, &small()).unwrap();

    let sentinel = dir.join("sentinel.txt");
    fs::write(&sentinel, b"must not survive").unwrap();

    let rebuilt = generate(&dir, &Spec { seed: 2, ..small() }).unwrap();

    assert!(!sentinel.exists(), "the tree was reused");
    assert_eq!(rebuilt.seed, 2);

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_custom_spec_is_never_reused() {
    let dir = scratch("custom-spec");
    let spec = Spec {
        filler_per_language: 1,
        ..small()
    };
    generate(&dir, &spec).unwrap();

    let sentinel = dir.join("sentinel.txt");
    fs::write(&sentinel, b"must not survive").unwrap();

    let rebuilt = generate(&dir, &spec).unwrap();

    assert!(!sentinel.exists(), "a custom spec reused its tree");
    assert_eq!(rebuilt.profile, manifest::CUSTOM);

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn generating_into_a_missing_directory_creates_it() {
    let dir = scratch("nested").join("deep").join("corpus");
    let manifest = generate(&dir, &small()).unwrap();
    assert!(dir.join(manifest::FILE_NAME).exists());
    assert!(manifest.file_count > 0);
    fs::remove_dir_all(dir.parent().unwrap().parent().unwrap()).unwrap();
}
