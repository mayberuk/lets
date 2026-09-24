use std::path::{Path, PathBuf};
use std::process::Command;

/// Fixed by the stack rule, not read back from the manifest: sixteen crates plus the grammar set.
const DIRECT_DEPENDENCY_LIMIT: usize = 17;

/// Seventeen crates cover eighteen languages — tsx ships inside `tree-sitter-typescript`.
const GRAMMAR_CRATES: usize = 17;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run_gate(manifest: &Path) -> (i32, String) {
    let output = Command::new("sh")
        .arg(repo_root().join("scripts/deps-gate.sh"))
        .arg(manifest)
        .output()
        .expect("scripts/deps-gate.sh runs under sh");
    let code = output.status.code().expect("the gate exits, never signals");
    let stdout = String::from_utf8(output.stdout).expect("the gate prints utf-8");
    (code, stdout)
}

fn reported_count(stdout: &str) -> usize {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("deps-gate: "))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|count| count.parse().ok())
        .unwrap_or_else(|| panic!("no `deps-gate: <n> direct dependencies` line in:\n{stdout}"))
}

#[test]
fn manifest_names_exactly_seventeen_direct_dependencies() {
    let (code, stdout) = run_gate(&repo_root().join("Cargo.toml"));

    assert_eq!(
        code, 0,
        "the gate rejected the committed manifest:\n{stdout}"
    );
    assert_eq!(reported_count(&stdout), DIRECT_DEPENDENCY_LIMIT, "{stdout}");
}

#[test]
fn the_grammar_set_collapses_to_one_entry_and_the_runtime_does_not() {
    let (_, stdout) = run_gate(&repo_root().join("Cargo.toml"));
    let entries: Vec<&str> = stdout.lines().map(str::trim).collect();

    assert!(
        entries.contains(&format!("tree-sitter-* ({GRAMMAR_CRATES} grammar crates)").as_str()),
        "{stdout}"
    );
    assert!(
        !stdout.contains("tree-sitter-rust"),
        "a grammar escaped the collapse:\n{stdout}"
    );
    assert!(
        entries.contains(&"tree-sitter"),
        "the runtime lost its entry:\n{stdout}"
    );
    assert!(
        entries.contains(&"tree-sitter-language"),
        "the ABI shim lost its entry:\n{stdout}"
    );
}

/// `links = "tree-sitter"` makes a second runtime a link failure; the lock shows it first.
#[test]
fn the_lock_resolves_one_tree_sitter_runtime_for_every_grammar() {
    let lock = std::fs::read_to_string(repo_root().join("Cargo.lock")).expect("Cargo.lock is read");
    let lock: toml_edit::DocumentMut = lock.parse().expect("Cargo.lock is toml");
    let packages = lock["package"].as_array_of_tables().expect("[[package]]");

    let mut runtimes: Vec<&str> = Vec::new();
    let mut grammars = 0;
    for package in packages {
        let name = package["name"].as_str().expect("a package name");
        if name == "tree-sitter" {
            runtimes.push(package["version"].as_str().expect("a package version"));
        } else if name.starts_with("tree-sitter-") && name != "tree-sitter-language" {
            grammars += 1;
        }
    }

    assert_eq!(
        runtimes.len(),
        1,
        "runtime versions in the lock: {runtimes:?}"
    );
    assert!(
        grammars >= GRAMMAR_CRATES,
        "only {grammars} grammar crates resolved, so the single runtime proves nothing"
    );
}

#[test]
fn an_eighteenth_dependency_in_table_form_fails_the_gate() {
    let committed =
        std::fs::read_to_string(repo_root().join("Cargo.toml")).expect("Cargo.toml is readable");
    let over_limit = committed.replace(
        "[dev-dependencies]\n",
        "[dependencies.walkdir]\nversion = \"2\"\n\n[dev-dependencies]\n",
    );
    assert_ne!(over_limit, committed, "the [dev-dependencies] header moved");

    let dir = tempfile::tempdir().expect("a temp dir");
    let manifest = dir.path().join("Cargo.toml");
    std::fs::write(&manifest, over_limit).expect("the temp manifest is writable");

    let (code, stdout) = run_gate(&manifest);

    assert_eq!(
        code, 1,
        "a dependency declared as its own table slipped past the gate:\n{stdout}"
    );
    assert_eq!(
        reported_count(&stdout),
        DIRECT_DEPENDENCY_LIMIT + 1,
        "{stdout}"
    );
    assert!(
        stdout.lines().any(|line| line.trim() == "walkdir"),
        "the table-form dependency was counted but never named:\n{stdout}"
    );
}

#[test]
fn an_eighteenth_dependency_as_a_build_dependency_fails_the_gate() {
    let committed =
        std::fs::read_to_string(repo_root().join("Cargo.toml")).expect("Cargo.toml is readable");
    let over_limit = committed.replace(
        "[dev-dependencies]\n",
        "[build-dependencies]\nwalkdir = \"2\"\n\n[dev-dependencies]\n",
    );
    assert_ne!(over_limit, committed, "the [dev-dependencies] header moved");

    let dir = tempfile::tempdir().expect("a temp dir");
    let manifest = dir.path().join("Cargo.toml");
    std::fs::write(&manifest, over_limit).expect("the temp manifest is writable");

    let (code, stdout) = run_gate(&manifest);

    assert_eq!(
        code, 1,
        "a build-dependency ships in the binary same as [dependencies] and must count:\n{stdout}"
    );
    assert_eq!(
        reported_count(&stdout),
        DIRECT_DEPENDENCY_LIMIT + 1,
        "{stdout}"
    );
    assert!(
        stdout.lines().any(|line| line.trim() == "walkdir"),
        "the build-dependency was counted but never named:\n{stdout}"
    );
}

#[test]
fn an_eighteenth_dependency_fails_the_gate() {
    let committed =
        std::fs::read_to_string(repo_root().join("Cargo.toml")).expect("Cargo.toml is readable");
    let over_limit = committed.replace("[dependencies]\n", "[dependencies]\nwalkdir = \"2\"\n");
    assert_ne!(over_limit, committed, "the [dependencies] header moved");

    let dir = tempfile::tempdir().expect("a temp dir");
    let manifest = dir.path().join("Cargo.toml");
    std::fs::write(&manifest, over_limit).expect("the temp manifest is writable");

    let (code, stdout) = run_gate(&manifest);

    assert_eq!(
        code, 1,
        "an eighteenth dependency passed the gate:\n{stdout}"
    );
    assert_eq!(
        reported_count(&stdout),
        DIRECT_DEPENDENCY_LIMIT + 1,
        "{stdout}"
    );
}

/// Reads the checked-in lock, so a drift is caught without running `cargo`.
#[test]
fn cargo_lock_resolves_exactly_one_tree_sitter_package() {
    let lock = std::fs::read_to_string(repo_root().join("Cargo.lock")).expect("Cargo.lock is read");
    let lock: toml_edit::DocumentMut = lock.parse().expect("Cargo.lock is toml");
    let packages = lock["package"].as_array_of_tables().expect("[[package]]");

    let tree_sitter_entries = packages
        .iter()
        .filter(|package| package["name"].as_str() == Some("tree-sitter"))
        .count();

    assert_eq!(
        tree_sitter_entries, 1,
        "Cargo.lock must resolve exactly one `tree-sitter` package"
    );
}

#[cfg(unix)]
#[test]
fn lint_without_cargo_deny_exits_1_and_names_the_install_command() {
    let scratch = tempfile::tempdir().expect("a scratch PATH dir");
    let real_path = std::env::var("PATH").expect("PATH is set for the test process");

    let mut just_bin = None;
    for dir in std::env::split_paths(&real_path) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name == "cargo-deny" || scratch.path().join(&name).exists() {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() && !metadata.is_symlink() {
                continue;
            }
            let link = scratch.path().join(&name);
            if std::os::unix::fs::symlink(entry.path(), &link).is_ok() && name == "just" {
                just_bin = Some(link);
            }
        }
    }
    let just_bin = just_bin.expect("`just` is on the test runner's PATH");

    let output = Command::new(just_bin)
        .arg("lint")
        .current_dir(repo_root())
        .envs(std::env::vars())
        .env("PATH", scratch.path())
        .output()
        .expect("just runs");

    let code = output.status.code().expect("just exits, never signals");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        code, 1,
        "lint must fail closed when cargo-deny is missing:\n{combined}"
    );
    assert!(
        combined.contains("cargo install cargo-deny --locked"),
        "the failure names no runnable replacement:\n{combined}"
    );
}
