//! A fake `claude` first on PATH records each call's arguments and environment; nothing here
//! reaches the real binary or spends API budget.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/smoke-agent.sh");
const LETS: &str = env!("CARGO_BIN_EXE_lets");

/// `SMOKE_SHIM_NO_RESULT` drops the result line and `SMOKE_SHIM_MUTATE` writes into the
/// fixture copy, so both failure paths have a driver.
const SHIM: &str = r#"#!/bin/sh
n=1
while [ -e "$SMOKE_SHIM_DIR/argv.$n" ]; do n=$((n + 1)); done
for arg in "$@"; do printf '%s\n' "$arg"; done >"$SMOKE_SHIM_DIR/argv.$n"
env | sed -n 's/^\([A-Za-z_][A-Za-z0-9_]*\)=.*/\1/p' | sort >"$SMOKE_SHIM_DIR/env.$n"
case "${1:-}" in
  --version) echo "0.0.0 (fake claude)"; exit 0 ;;
esac
[ -z "${SMOKE_SHIM_MUTATE:-}" ] || echo mutated >"./smoke-shim-wrote-this"
printf '%s\n' '{"type":"assistant","message":{"content":[]}}'
[ -n "${SMOKE_SHIM_NO_RESULT:-}" ] || printf '%s\n' '{"type":"result","subtype":"success","result":"stub"}'
"#;

/// Credential-shaped so it must be scrubbed; `ANTHROPIC_API_KEY` is the control that survives.
const PLANTED_SECRET: &str = "SMOKE_PLANTED_TOKEN";

struct Smoke {
    _temp: TempDir,
    logs: PathBuf,
    shim: PathBuf,
    home: PathBuf,
    bin: PathBuf,
}

struct Invocation {
    argv: Vec<String>,
    env: Vec<String>,
}

impl Invocation {
    fn has(&self, flag: &str) -> bool {
        self.argv.iter().any(|arg| arg == flag)
    }

    fn value(&self, flag: &str) -> &str {
        let at = self
            .argv
            .iter()
            .position(|arg| arg == flag)
            .unwrap_or_else(|| panic!("no {flag} in {:?}", self.argv));
        self.argv
            .get(at + 1)
            .unwrap_or_else(|| panic!("{flag} has no value in {:?}", self.argv))
    }
}

fn smoke() -> Smoke {
    let temp = TempDir::new().expect("a temp directory");
    let root = temp.path().to_path_buf();
    let (logs, shim, home, bin) = (
        root.join("logs"),
        root.join("shim"),
        root.join("home"),
        root.join("bin"),
    );
    for dir in [&logs, &shim, &home, &bin] {
        fs::create_dir_all(dir).expect("a harness directory");
    }

    write_executable(&shim.join("claude"), SHIM);
    // Stands in for `target/release/lets`: the script runs its `hooks install` for the with-hooks
    // arm's settings.json.
    std::os::unix::fs::symlink(LETS, bin.join("lets")).expect("a lets on the arms' PATH");

    Smoke {
        _temp: temp,
        logs,
        shim,
        home,
        bin,
    }
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).expect("a harness script");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("an executable bit");
}

impl Smoke {
    /// `GIT_*` is scrubbed: under a pre-commit hook it would redirect the script's `git init`.
    fn run(&self, overrides: &[(&str, &str)]) -> Output {
        let mut command = Command::new("sh");
        command.arg(SCRIPT);
        for (name, _) in std::env::vars_os() {
            if name.to_str().is_some_and(|name| name.starts_with("GIT_")) {
                command.env_remove(name);
            }
        }
        command
            .env("PATH", self.path())
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("LETS_SMOKE_LOGS_DIR", &self.logs)
            .env("LETS_SMOKE_BIN_DIR", &self.bin)
            .env("SMOKE_SHIM_DIR", &self.shim)
            .env("ANTHROPIC_API_KEY", "fake-key-the-shim-never-sends")
            .env(PLANTED_SECRET, "fake-secret-that-must-not-reach-claude");
        for (name, value) in overrides {
            command.env(name, value);
        }
        command.output().expect("the smoke-agent script runs")
    }

    fn path(&self) -> String {
        let inherited = std::env::var("PATH").unwrap_or_default();
        format!("{}:{inherited}", self.shim.display())
    }

    fn run_dir(&self) -> PathBuf {
        let mut runs: Vec<PathBuf> = fs::read_dir(self.logs.join("agent-smoke"))
            .expect("the evidence directory")
            .map(|entry| entry.expect("an evidence entry").path())
            .collect();
        assert_eq!(runs.len(), 1, "{runs:?}");
        runs.pop().expect("one run directory")
    }

    fn invocations(&self) -> Vec<Invocation> {
        let mut calls = Vec::new();
        for index in 1.. {
            let argv = self.shim.join(format!("argv.{index}"));
            if !argv.exists() {
                break;
            }
            calls.push(Invocation {
                argv: lines(&argv),
                env: lines(&self.shim.join(format!("env.{index}"))),
            });
        }
        calls
    }

    fn arms(&self) -> Vec<Invocation> {
        self.invocations()
            .into_iter()
            .filter(|call| call.has("-p"))
            .collect()
    }
}

fn lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .lines()
        .map(str::to_owned)
        .collect()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("utf-8 stdout")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("utf-8 stderr")
}

#[test]
fn two_arms_run_and_only_the_second_loads_the_installed_hooks() {
    let smoke = smoke();
    let output = smoke.run(&[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let arms = smoke.arms();
    assert_eq!(arms.len(), 2, "claude -p ran {} times", arms.len());
    assert_eq!(
        arms.iter().filter(|arm| arm.has("-p")).count(),
        2,
        "both arms are print-mode runs"
    );

    let with_settings: Vec<&Invocation> = arms.iter().filter(|arm| arm.has("--settings")).collect();
    assert_eq!(with_settings.len(), 1, "exactly one arm loads hooks");
    assert_eq!(
        Path::new(with_settings[0].value("--settings")),
        smoke.run_dir().join("hooks-settings.json"),
        "{:?}",
        with_settings[0].argv
    );

    let prompts: BTreeSet<&str> = arms.iter().map(|arm| arm.value("-p")).collect();
    assert_eq!(
        prompts.len(),
        1,
        "the two arms must vary only whether hooks are loaded"
    );
}

#[test]
fn every_arm_isolates_the_settings_and_forbids_the_writing_tools() {
    let smoke = smoke();
    let output = smoke.run(&[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    for arm in smoke.arms() {
        assert_eq!(arm.value("--setting-sources"), "", "{:?}", arm.argv);
        assert_eq!(
            arm.value("--disallowed-tools"),
            "Write,Edit,NotebookEdit",
            "{:?}",
            arm.argv
        );
    }
}

#[test]
fn a_credential_shaped_variable_is_unset_for_the_arms_and_the_auth_one_is_not() {
    let smoke = smoke();
    let output = smoke.run(&[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    for arm in smoke.arms() {
        assert!(
            !arm.env.iter().any(|name| name == PLANTED_SECRET),
            "{PLANTED_SECRET} reached an arm"
        );
        assert!(
            arm.env.iter().any(|name| name == "ANTHROPIC_API_KEY"),
            "the auth variable was stripped with the secrets"
        );
    }

    // Set in the script's own environment, so the assertion above tests `env -u`, not a variable
    // that was never set.
    let version_call = smoke
        .invocations()
        .into_iter()
        .find(|call| call.has("--version"))
        .expect("meta.txt records claude --version");
    assert!(version_call.env.iter().any(|name| name == PLANTED_SECRET));
}

#[test]
fn each_arm_leaves_its_raw_evidence_and_no_verdict() {
    let smoke = smoke();
    let output = smoke.run(&[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let run_dir = smoke.run_dir();
    let found: BTreeSet<String> = fs::read_dir(&run_dir)
        .expect("the run directory")
        .map(|entry| {
            entry
                .expect("an evidence file")
                .file_name()
                .to_str()
                .expect("utf-8 file names")
                .to_owned()
        })
        .collect();

    let mut expected: BTreeSet<String> = BTreeSet::from([
        "meta.txt".to_owned(),
        "hooks-install.out".to_owned(),
        "hooks-settings.json".to_owned(),
    ]);
    for arm in ["baseline", "with-hooks"] {
        for suffix in [
            "jsonl",
            "stderr.log",
            "exit",
            "tree-before.sha",
            "tree-after.sha",
        ] {
            expected.insert(format!("{arm}.{suffix}"));
        }
    }
    assert_eq!(found, expected, "grading belongs to scripts/smoke-judge.md");

    for arm in ["baseline", "with-hooks"] {
        assert_eq!(
            fs::read_to_string(run_dir.join(format!("{arm}.exit"))).unwrap(),
            "0\n"
        );
        let before = fs::read_to_string(run_dir.join(format!("{arm}.tree-before.sha"))).unwrap();
        let after = fs::read_to_string(run_dir.join(format!("{arm}.tree-after.sha"))).unwrap();
        assert!(before.contains("README.md"), "{before}");
        assert_eq!(before, after);
    }

    let printed = stdout(&output);
    assert!(
        printed.contains(run_dir.to_str().expect("utf-8 path")),
        "{printed}"
    );
    assert!(
        printed.contains("grade with scripts/smoke-judge.md"),
        "{printed}"
    );
    for arm in ["baseline", "with-hooks"] {
        assert!(printed.contains(&format!("{arm} exit=0 ")), "{printed}");
        assert!(printed.contains("result-line=yes"), "{printed}");
    }
}

/// The stderr assertion is what makes this a test of the guard, not of an earlier failure.
#[test]
fn claude_missing_from_path_stops_before_either_arm() {
    let smoke = smoke();
    let output = smoke.run(&[("PATH", "/usr/bin:/bin")]);

    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("smoke-agent: claude not on PATH"),
        "{}",
        stderr(&output)
    );
    assert!(
        !smoke.logs.join("agent-smoke").exists(),
        "an evidence directory was created for a run that never happened"
    );
    assert!(smoke.invocations().is_empty());
}

#[test]
fn an_arm_whose_transcript_has_no_result_line_fails_the_run() {
    let smoke = smoke();
    let output = smoke.run(&[("SMOKE_SHIM_NO_RESULT", "1")]);

    assert_ne!(output.status.code(), Some(0), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("result-line=no"),
        "{}",
        stdout(&output)
    );
    assert_eq!(
        smoke.arms().len(),
        2,
        "both arms still ran and were recorded"
    );
}

#[test]
fn a_run_that_writes_into_the_fixture_copy_fails_the_run() {
    let smoke = smoke();
    let output = smoke.run(&[("SMOKE_SHIM_MUTATE", "1")]);

    assert_ne!(output.status.code(), Some(0), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("the fixture copy changed"),
        "{}",
        stdout(&output)
    );

    let run_dir = smoke.run_dir();
    let before = fs::read_to_string(run_dir.join("baseline.tree-before.sha")).unwrap();
    let after = fs::read_to_string(run_dir.join("baseline.tree-after.sha")).unwrap();
    assert_ne!(before, after);
}

fn installed_settings(smoke: &Smoke) -> Vec<u8> {
    let home = TempDir::new().expect("an install HOME");
    let inherited = std::env::var("PATH").unwrap_or_default();
    let output = Command::new(LETS)
        .args(["hooks", "install", "claude-code"])
        .env("HOME", home.path())
        .env("XDG_RUNTIME_DIR", home.path())
        .env("PATH", format!("{}:{inherited}", smoke.bin.display()))
        .output()
        .expect("lets runs");
    assert!(output.status.success(), "{}", stderr(&output));
    fs::read(home.path().join(".claude/settings.json")).expect("the installed settings")
}

// Discovery tier 1 is a SessionStart hook, so the arm loads what `hooks install` writes, not
// `docs/agents.md`, which is a reference page.
#[test]
fn the_with_hooks_arm_loads_the_settings_hooks_install_writes() {
    let smoke = smoke();
    let output = smoke.run(&[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let loaded = fs::read(smoke.run_dir().join("hooks-settings.json")).expect("the kept settings");

    assert_eq!(loaded, installed_settings(&smoke));
    assert_ne!(loaded, include_bytes!("../docs/agents.md").to_vec());
    let text = String::from_utf8(loaded).expect("utf-8");
    assert!(text.contains("SessionStart"), "{text}");
    assert!(text.contains("SubagentStart"), "{text}");
    assert!(
        text.contains("# File work: use `lets` through Bash"),
        "{text}"
    );
}

#[test]
fn a_failed_hooks_install_stops_before_either_arm() {
    let smoke = smoke();
    fs::remove_file(smoke.bin.join("lets")).expect("the symlink goes");
    write_executable(&smoke.bin.join("lets"), "#!/bin/sh\nexit 1\n");

    let output = smoke.run(&[]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("lets hooks install claude-code failed"),
        "{}",
        stderr(&output)
    );
    assert!(
        smoke.arms().is_empty(),
        "no arm may run without the paragraph"
    );
}
