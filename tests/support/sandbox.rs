// Each test binary compiles this module on its own, so a helper only one of them calls is dead
// code in the others and would fail `clippy -D warnings` there.
#![allow(dead_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// Dot-prefixed: `find`'s walker skips hidden entries, so the sandbox's own scaffolding never
/// comes back as a search hit.
const HOME_DIR: &str = ".home";
const RUNTIME_DIR: &str = ".run";
const CONFIG_DIR: &str = ".config";

pub struct Sandbox {
    _temp: TempDir,
    root: PathBuf,
}

pub struct Run {
    pub code: i32,
    pub out: String,
    pub err: String,
}

pub fn sandbox(fixture: &str) -> Sandbox {
    let temp = TempDir::new().expect("a temp directory for the sandbox");
    // macOS hands out `/var/folders/…` while a process inside reports `/private/var/folders/…`;
    // normalising the path a subprocess actually prints needs the resolved one.
    let root = temp
        .path()
        .canonicalize()
        .expect("the temp directory resolves");
    copy_tree(&fixture_tree(fixture), &root);
    for dir in [HOME_DIR, RUNTIME_DIR, CONFIG_DIR] {
        std::fs::create_dir_all(root.join(dir)).expect("the sandbox's own directories");
    }
    git_init(&root, &root.join(HOME_DIR));

    Sandbox { _temp: temp, root }
}

/// For trycmd, whose cases name their `[fs] base`. The git working tree is the point: `ignore`
/// honours a `.gitignore` only inside one, so a `find` golden without it proves nothing.
pub fn prepare(fixture: &str, at: &Path) {
    if at.exists() {
        std::fs::remove_dir_all(at).expect("the previous run's tree is removable");
    }
    std::fs::create_dir_all(at).expect("the prepared tree's directory");
    let at = at.canonicalize().expect("the prepared tree resolves");
    copy_tree(&fixture_tree(fixture), &at);
    // `git init` only reads `$HOME/.gitconfig`, so the tree can stand in for its own HOME.
    git_init(&at, &at);
}

fn fixture_tree(fixture: &str) -> PathBuf {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    assert!(source.is_dir(), "no fixture tree at {}", source.display());
    source
}

/// The scope guard refuses a path outside a working tree. `HOME` alone still leaves
/// `/etc/gitconfig` reachable, hence `GIT_CONFIG_NOSYSTEM=1`.
fn git_init(directory: &Path, home: &Path) {
    let output = scrubbed("git", directory)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .args(["-c", "init.defaultBranch=main", "init", "--quiet"])
        .output()
        .expect("git is on PATH");
    assert!(
        output.status.success(),
        "git init in {}: {}",
        directory.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A pre-commit hook's inherited `GIT_DIR` and friends would redirect every git command onto
/// the real repo; removing every `GIT_*` means no fixed list of names can go stale.
fn scrubbed(program: &str, directory: &Path) -> Command {
    let mut command = Command::new(program);
    command.current_dir(directory);
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(|name| name.starts_with("GIT_")) {
            command.env_remove(name);
        }
    }
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command
}

impl Sandbox {
    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn lets<I, S>(&self, args: I) -> Run
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut command = self.command(env!("CARGO_BIN_EXE_lets"));
        for arg in args {
            command.arg(arg.as_ref());
        }
        self.finish(command)
    }

    /// Handed to `bash -c` whole: a heredoc is one command, not one per line.
    pub fn bash(&self, command: &str) -> Run {
        let mut process = self.command("bash");
        process.arg("-c").arg(command);
        self.finish(process)
    }

    pub fn read(&self, relative: &str) -> String {
        let path = self.root.join(relative);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
    }

    fn home(&self) -> PathBuf {
        self.root.join(HOME_DIR)
    }

    fn runtime(&self) -> PathBuf {
        self.root.join(RUNTIME_DIR)
    }

    fn config(&self) -> PathBuf {
        self.root.join(CONFIG_DIR)
    }

    /// Unset, `XDG_CONFIG_HOME` would let the runner's own global gitignore reach `find`'s walk.
    fn command(&self, program: &str) -> Command {
        let mut command = scrubbed(program, &self.root);
        command
            .env("HOME", self.home())
            .env("XDG_RUNTIME_DIR", self.runtime())
            .env("XDG_CONFIG_HOME", self.config())
            .env("LETS_NO_STATS", "1")
            .env("LETS_TOKEN_RATIO", "4")
            .env("PATH", path_with_binary_under_test());
        command
    }

    fn finish(&self, mut command: Command) -> Run {
        let output = command.output().expect("the sandbox command runs");
        Run {
            code: output
                .status
                .code()
                .expect("a sandbox command exits, never signals"),
            out: self.normalise(&utf8(output.stdout, "stdout")),
            err: self.normalise(&utf8(output.stderr, "stderr")),
        }
    }

    /// Longest prefix first: HOME is inside the sandbox root, so substituting the root first
    /// would stop `[HOME]` from ever matching. The version goes last so a bump is not a golden
    /// change.
    fn normalise(&self, text: &str) -> String {
        text.replace(&display(&self.home()), "[HOME]")
            .replace(&display(&self.runtime()), "[XDG_RUNTIME_DIR]")
            .replace(&display(&self.root), "[SANDBOX]")
            .replace(env!("CARGO_PKG_VERSION"), "[VERSION]")
    }
}

fn display(path: &Path) -> String {
    path.to_str().expect("sandbox paths are utf-8").to_owned()
}

fn utf8(bytes: Vec<u8>, stream: &str) -> String {
    String::from_utf8(bytes).unwrap_or_else(|_| panic!("a case wrote non-utf-8 to {stream}"))
}

/// So a scripted `lets …` resolves to the binary this test run built, never to an installed one.
fn path_with_binary_under_test() -> OsString {
    let directory = Path::new(env!("CARGO_BIN_EXE_lets"))
        .parent()
        .expect("the built binary has a directory")
        .to_path_buf();
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(std::iter::once(directory).chain(std::env::split_paths(&inherited)))
        .expect("no PATH entry contains the separator")
}

fn copy_tree(source: &Path, target: &Path) {
    let entries = std::fs::read_dir(source)
        .unwrap_or_else(|error| panic!("reading {}: {error}", source.display()));
    for entry in entries {
        let entry = entry.expect("a fixture entry");
        let to = target.join(entry.file_name());
        if entry.file_type().expect("a fixture entry type").is_dir() {
            std::fs::create_dir_all(&to).expect("a fixture subdirectory");
            copy_tree(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to)
                .unwrap_or_else(|error| panic!("copying {}: {error}", entry.path().display()));
        }
    }
}

mod tests {
    use super::*;

    #[test]
    fn home_is_substituted_before_the_sandbox_root_that_contains_it() {
        let sandbox = sandbox("base");
        let root = display(sandbox.path());
        let home = display(&sandbox.home());

        let normalised =
            sandbox.normalise(&format!("cache at {home}/x · file at {root}/README.md"));

        assert_eq!(
            normalised, "cache at [HOME]/x · file at [SANDBOX]/README.md",
            "a HOME path must not survive as [SANDBOX]/.home…"
        );
    }

    #[test]
    fn the_crate_version_is_substituted_so_a_bump_is_not_a_golden_change() {
        let sandbox = sandbox("base");

        assert_eq!(
            sandbox.normalise(&format!("lets {}", env!("CARGO_PKG_VERSION"))),
            "lets [VERSION]"
        );
    }

    #[test]
    fn a_case_sees_the_sandbox_home_and_the_pinned_stats_environment() {
        let sandbox = sandbox("base");

        let environment = sandbox.bash(
            r#"printf '%s %s %s %s' "$HOME" "$XDG_RUNTIME_DIR" "$LETS_NO_STATS" "$LETS_TOKEN_RATIO""#,
        );

        assert_eq!(environment.out, "[HOME] [XDG_RUNTIME_DIR] 1 4");
        let real = std::env::var("HOME").expect("the test runner has a HOME");
        assert!(
            !environment.out.contains(&real),
            "the real HOME reached a case: {}",
            environment.out
        );

        let config_home = sandbox.bash(r#"printf '%s' "$XDG_CONFIG_HOME""#).out;
        assert_eq!(
            config_home, "[SANDBOX]/.config",
            "XDG_CONFIG_HOME must stay inside the sandbox root, or a real ~/.config/git/ignore \
             could reach the walk `ignore` 0.4.33 does"
        );
    }

    #[test]
    fn the_fixture_tree_is_copied_into_a_working_tree() {
        let sandbox = sandbox("base");

        assert!(sandbox.read("README.md").contains("# base fixture"));
        assert!(sandbox.read("config/app.json").contains("\"threads\""));
        assert!(
            sandbox.path().join(".git").is_dir(),
            "the scope guard needs a working tree"
        );
    }

    #[test]
    fn a_scripted_lets_resolves_to_the_binary_under_test() {
        let sandbox = sandbox("base");

        assert_eq!(sandbox.bash("lets version").out, "lets [VERSION]\n");
        assert_eq!(sandbox.lets(["version"]).out, "lets [VERSION]\n");
    }

    /// Git exports `GIT_DIR` and friends to hook subprocesses; the test re-runs itself alone with
    /// them set rather than set them under every other test in this binary.
    #[test]
    fn git_env_from_the_process_does_not_redirect_the_sandbox_git_init() {
        const CHILD: &str = "LETS_TEST_OWN_PROCESS";
        if std::env::var_os(CHILD).is_none() {
            let thread = std::thread::current();
            let name = thread
                .name()
                .expect("libtest runs each test on a thread named after it");
            let run = Command::new(std::env::current_exe().expect("the test binary"))
                .args(["--exact", name, "--nocapture", "--test-threads=1"])
                .env(CHILD, "1")
                .env("GIT_DIR", "/nonexistent/git-dir-from-hook")
                .env("GIT_WORK_TREE", "/nonexistent/work-tree-from-hook")
                .output()
                .expect("the test binary re-runs itself");
            let out = String::from_utf8_lossy(&run.stdout);
            // Exit 0 alone would also cover a filter that matched no test.
            assert!(
                run.status.success() && out.contains(" 1 passed"),
                "{name} in its own process:\n{out}\n{}",
                String::from_utf8_lossy(&run.stderr)
            );
            return;
        }

        let sandbox = sandbox("base");
        let scrubbed = sandbox
            .command("git")
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .expect("git runs in the sandbox");
        let scrubbed_toplevel = String::from_utf8_lossy(&scrubbed.stdout).trim().to_owned();

        assert_eq!(
            scrubbed_toplevel,
            display(sandbox.path()),
            "an inherited GIT_DIR/GIT_WORK_TREE must not redirect the sandbox's git commands"
        );

        // Without the scrub the same variables must break git, or the assertion above proves
        // nothing.
        let unscrubbed = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(sandbox.path())
            .output()
            .expect("git runs unscrubbed");
        let unscrubbed_toplevel = String::from_utf8_lossy(&unscrubbed.stdout)
            .trim()
            .to_owned();
        assert!(
            !unscrubbed.status.success() || unscrubbed_toplevel != scrubbed_toplevel,
            "the control must fail or disagree with the sandbox toplevel: {unscrubbed_toplevel}"
        );
    }

    #[test]
    fn two_sandboxes_of_one_fixture_do_not_share_a_tree() {
        let first = sandbox("base");
        let second = sandbox("base");

        first.bash("printf 'edited' > README.md");

        assert_eq!(first.read("README.md"), "edited");
        assert!(second.read("README.md").contains("# base fixture"));
    }
}
