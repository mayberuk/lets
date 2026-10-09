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
            .env_remove("XDG_DATA_HOME")
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
        "data".to_owned(),
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

fn installed_settings(smoke: &Smoke, data_home: &Path) -> Vec<u8> {
    let home = TempDir::new().expect("an install HOME");
    let inherited = std::env::var("PATH").unwrap_or_default();
    let output = Command::new(LETS)
        .args(["hooks", "install", "claude-code"])
        .env("HOME", home.path())
        .env("XDG_RUNTIME_DIR", home.path())
        .env("XDG_DATA_HOME", data_home)
        .env("PATH", format!("{}:{inherited}", smoke.bin.display()))
        .output()
        .expect("lets runs");
    assert!(output.status.success(), "{}", stderr(&output));
    fs::read(home.path().join(".claude/settings.json")).expect("the installed settings")
}

// Discovery tier 1 is the lets mod that `CLAUDE_CODE_PLUGIN_DIRS` names, so the arm loads what
// `hooks install` writes, not `docs/agents.md`, which is a reference page.
#[test]
fn the_with_hooks_arm_loads_the_settings_hooks_install_writes() {
    let smoke = smoke();
    let output = smoke.run(&[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let loaded = fs::read(smoke.run_dir().join("hooks-settings.json")).expect("the kept settings");
    let data_home = smoke.run_dir().join("data");
    let mod_dir = data_home.join("lets/claude-code");
    assert!(mod_dir.join("hooks/steer.ts").is_file());

    assert_eq!(loaded, installed_settings(&smoke, &data_home));
    assert_ne!(loaded, include_bytes!("../docs/agents.md").to_vec());
    let text = String::from_utf8(loaded).expect("utf-8");
    assert!(text.contains("PreToolUse"), "{text}");
    assert!(
        text.contains(&format!(
            "\"CLAUDE_CODE_PLUGIN_DIRS\": \"{}\"",
            mod_dir.display()
        )),
        "{text}"
    );
    assert!(text.contains("\"SessionStart\": ["), "{text}");
    assert!(text.contains("\"SubagentStart\": ["), "{text}");
}

fn assert_kept_settings_name_a_surviving_mod(smoke: &Smoke) {
    let run_dir = smoke.run_dir();
    let settings = fs::read_to_string(run_dir.join("hooks-settings.json")).expect("kept settings");
    let settings: serde_json::Value = serde_json::from_str(&settings).expect("settings are JSON");
    let named = settings["env"]["CLAUDE_CODE_PLUGIN_DIRS"]
        .as_str()
        .expect("settings name the mod directory");

    let named = Path::new(named);
    assert!(named.is_dir(), "{} is gone after the run", named.display());
    assert!(named.join("hooks/steer.ts").is_file());
    assert!(
        named.starts_with(&run_dir),
        "{} is outside {}",
        named.display(),
        run_dir.display()
    );
    let meta = fs::read_to_string(run_dir.join("meta.txt")).expect("meta.txt");
    assert!(
        meta.lines()
            .any(|line| line == format!("lets-mod-dir={}", named.display())),
        "{meta}"
    );
}

#[test]
fn the_mod_the_settings_name_outlives_the_run_when_xdg_data_home_is_unset() {
    let smoke = smoke();
    let output = smoke.run(&[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    assert_kept_settings_name_a_surviving_mod(&smoke);
    assert!(
        !smoke.home.join(".local").exists(),
        "the mod landed in the caller's default data directory"
    );
}

#[test]
fn the_mod_the_settings_name_outlives_the_run_and_the_callers_xdg_data_home_stays_empty() {
    let smoke = smoke();
    let other = TempDir::new().expect("a caller data directory");
    let output = smoke.run(&[("XDG_DATA_HOME", other.path().to_str().expect("utf-8 path"))]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    assert_kept_settings_name_a_surviving_mod(&smoke);
    assert_eq!(
        fs::read_dir(other.path())
            .expect("the caller data directory")
            .count(),
        0,
        "the caller's XDG_DATA_HOME was written to"
    );
}

#[test]
fn a_hooks_install_that_leaves_no_mod_stops_before_either_arm() {
    let smoke = smoke();
    fs::remove_file(smoke.bin.join("lets")).expect("the symlink goes");
    write_executable(&smoke.bin.join("lets"), "#!/bin/sh\nexit 0\n");

    let output = smoke.run(&[]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("hooks install left no mod at"),
        "{}",
        stderr(&output)
    );
    assert!(smoke.arms().is_empty());
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

const LIVE_CAT_PROMPT: &str =
    "Use the Bash tool to run exactly `cat README.md`, then reply with only its first line.";
const LIVE_SED_COMMAND: &str = "sed -i 's/alpha/beta/g' notes.txt";
const LIVE_REWRITTEN_SHOW: &str = "show README.md --all --no-header --no-numbers";
const LIVE_BUDGET_USD: f64 = 5.0;

struct LiveRun {
    stream: String,
    verdicts: String,
    notes: String,
    lets_argv: String,
}

impl LiveRun {
    fn events(&self) -> Vec<serde_json::Value> {
        self.stream
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn bash_commands(&self) -> Vec<String> {
        let mut commands = Vec::new();
        for event in self.events() {
            let blocks = event["message"]["content"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for block in blocks {
                if block["type"] == "tool_use" && block["name"] == "Bash" {
                    commands.push(
                        block["input"]["command"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned(),
                    );
                }
            }
        }
        commands
    }

    fn tool_results(&self) -> Vec<(String, bool)> {
        let mut results = Vec::new();
        for event in self.events() {
            let blocks = event["message"]["content"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for block in blocks.iter().filter(|block| block["type"] == "tool_result") {
                let text = match &block["content"] {
                    serde_json::Value::String(text) => text.clone(),
                    serde_json::Value::Array(parts) => parts
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                results.push((text, block["is_error"] == true));
            }
        }
        results
    }

    fn reply(&self) -> String {
        self.events()
            .iter()
            .rev()
            .find(|event| event["type"] == "result")
            .and_then(|event| event["result"].as_str())
            .unwrap_or_default()
            .to_owned()
    }

    fn cost(&self) -> f64 {
        self.events()
            .iter()
            .filter(|event| event["type"] == "result")
            .filter_map(|event| event["total_cost_usd"].as_f64())
            .sum()
    }

    fn ran_lets(&self, argv: &str) -> bool {
        self.lets_argv.lines().any(|line| line == argv)
    }

    fn has_verdict(&self, kind: &str) -> bool {
        self.verdicts
            .lines()
            .any(|line| line == format!("{{\"verdict\":\"{kind}\"}}"))
    }
}

fn credential_variables() -> Vec<std::ffi::OsString> {
    let bedrock =
        std::env::var_os("CLAUDE_CODE_USE_BEDROCK").is_some_and(|value| !value.is_empty());
    std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| {
            let Some(name) = name.to_str() else {
                return false;
            };
            if name.starts_with("GIT_") {
                return true;
            }
            if matches!(
                name,
                "ANTHROPIC_API_KEY" | "ANTHROPIC_AUTH_TOKEN" | "CLAUDE_CODE_OAUTH_TOKEN"
            ) {
                return false;
            }
            if name.starts_with("AWS_") {
                return !bedrock;
            }
            ["_TOKEN", "_KEY", "_SECRET"]
                .iter()
                .any(|suffix| name.ends_with(suffix))
        })
        .collect()
}

fn live_fixture(root: &Path, label: &str) -> PathBuf {
    let fixture = root.join(label);
    fs::create_dir_all(fixture.join("run")).expect("a fixture directory");
    fs::write(fixture.join("README.md"), "alpha line\nsecond line\n").expect("README.md");
    fs::write(fixture.join("notes.txt"), "alpha\n").expect("notes.txt");
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&fixture)
        .envs([("GIT_CONFIG_NOSYSTEM", "1")])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .expect("git runs");
    assert!(init.status.success(), "{}", stderr(&init));
    fixture
}

fn live_arm(
    root: &Path,
    label: &str,
    prompt: &str,
    plugin_dir: Option<&Path>,
) -> (LiveRun, PathBuf) {
    let fixture = live_fixture(root, label);
    let verdict_log = root.join(format!("{label}.verdicts.jsonl"));
    let argv_log = root.join(format!("{label}.lets-argv"));
    let shim_dir = root.join(format!("{label}.bin"));
    fs::create_dir_all(&shim_dir).expect("a shim directory");
    let shim = shim_dir.join("lets");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >>'{}'\nexec '{LETS}' \"$@\"\n",
            argv_log.display()
        ),
    )
    .expect("the lets shim");
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).expect("an executable shim");
    let inherited = std::env::var("PATH").unwrap_or_default();

    let mut command = Command::new("claude");
    command
        .args(["-p", prompt])
        .args(["--output-format", "stream-json", "--verbose"])
        .args(["--permission-mode", "acceptEdits"])
        .args(["--allowedTools", "Bash"])
        .args(["--settings", r#"{"outputStyle":"default"}"#])
        .args(["--setting-sources", "project"])
        .arg("--strict-mcp-config")
        .args(["--tools", "Bash"])
        .arg("--disable-slash-commands")
        .args(["--max-turns", "6"])
        .args(["--model", "haiku"])
        .current_dir(&fixture);
    for name in credential_variables() {
        command.env_remove(name);
    }
    command
        .env("PATH", format!("{}:{inherited}", shim_dir.display()))
        .env("XDG_RUNTIME_DIR", fixture.join("run"))
        .env("LETS_NO_STATS", "1")
        .env("LETS_HOOK_LOG", &verdict_log);
    match plugin_dir {
        Some(dir) => command.env("CLAUDE_CODE_PLUGIN_DIRS", dir),
        None => command.env_remove("CLAUDE_CODE_PLUGIN_DIRS"),
    };

    let output = command.output().expect("claude runs");
    let stream = String::from_utf8_lossy(&output.stdout).into_owned();
    fs::write(root.join(format!("{label}.stream.jsonl")), &stream).expect("the raw stream");
    fs::write(root.join(format!("{label}.stderr.log")), &output.stderr).expect("claude's stderr");
    let run = LiveRun {
        stream,
        verdicts: fs::read_to_string(&verdict_log).unwrap_or_default(),
        notes: fs::read_to_string(fixture.join("notes.txt")).unwrap_or_default(),
        lets_argv: fs::read_to_string(&argv_log).unwrap_or_default(),
    };
    (run, fixture)
}

fn live_failures(cat_with: &LiveRun, sed_with: &LiveRun) -> Vec<String> {
    let mut failures = Vec::new();
    let mut check = |ok: bool, what: &str| {
        if !ok {
            failures.push(what.to_owned());
        }
    };

    check(
        cat_with.bash_commands() == ["cat README.md"],
        "with mod: the model ran exactly `cat README.md`",
    );
    check(
        cat_with.has_verdict("rewrite"),
        "with mod: the verdict log has a rewrite line",
    );
    check(
        cat_with.ran_lets(LIVE_REWRITTEN_SHOW),
        "with mod: lets was run as `show README.md --all --no-header --no-numbers`",
    );
    check(
        cat_with
            .tool_results()
            .iter()
            .any(|(text, is_error)| !is_error && text.contains("alpha line")),
        "with mod: the Bash tool_result holds the README text and is not an error",
    );
    check(
        cat_with.reply().contains("alpha line"),
        "with mod: the reply holds `alpha line`",
    );

    check(
        sed_with.bash_commands() == [LIVE_SED_COMMAND],
        "with mod: the model ran exactly the sed command",
    );
    check(sed_with.notes == "beta\n", "with mod: notes.txt reads beta");
    check(
        sed_with
            .lets_argv
            .lines()
            .any(|line| line.starts_with("edit notes.txt ")),
        "with mod: lets edit ran on notes.txt in place of the sed command",
    );
    check(
        sed_with
            .tool_results()
            .iter()
            .any(|(text, _)| text.contains("1 replacement")),
        "with mod: the sed tool_result holds lets edit's footer",
    );
    check(
        sed_with.has_verdict("block"),
        "with mod: the verdict log has a block line",
    );
    check(
        sed_with
            .tool_results()
            .iter()
            .all(|(_, is_error)| !is_error)
            && !sed_with.tool_results().is_empty(),
        "with mod: the sed tool_result is present and is not an error",
    );
    failures
}

fn live_control_failures(cat_control: &LiveRun, sed_control: &LiveRun) -> Vec<String> {
    let mut failures = Vec::new();
    let mut check = |ok: bool, what: &str| {
        if !ok {
            failures.push(what.to_owned());
        }
    };

    check(
        cat_control.bash_commands() == ["cat README.md"],
        "control: the model ran exactly `cat README.md`",
    );
    check(
        !cat_control
            .lets_argv
            .lines()
            .any(|line| line.starts_with("show")),
        "control: lets show never ran",
    );
    check(
        cat_control.verdicts.is_empty(),
        "control: the cat arm logged no verdict",
    );
    check(
        cat_control
            .tool_results()
            .iter()
            .any(|(text, _)| text.contains("alpha line")),
        "control: the cat tool_result holds the raw README text",
    );
    check(
        sed_control.bash_commands() == [LIVE_SED_COMMAND],
        "control: the model ran exactly the sed command",
    );
    check(
        !sed_control
            .lets_argv
            .lines()
            .any(|line| line.starts_with("edit")),
        "control: lets edit never ran",
    );
    check(
        !sed_control
            .tool_results()
            .iter()
            .any(|(text, _)| text.contains("1 replacement")),
        "control: the sed tool_result holds no lets output",
    );
    check(
        sed_control.verdicts.is_empty(),
        "control: the sed arm logged no verdict",
    );
    check(
        sed_control.notes == "beta\n",
        "control: sed ran as written and notes.txt reads beta",
    );
    failures
}

/// Real `claude -p` under dp's child flags, so a pass also shows a dp child loads the mod. Costs
/// API budget (four Haiku runs, a few cents), hence the opt-in variable on top of `#[ignore]`.
/// Run with `--success-output immediate` so the cost line shows on a pass.
#[test]
#[ignore = "spends API budget; set LETS_LIVE_SMOKE=1 and pass --run-ignored"]
fn live_haiku_loads_the_mod_and_answers_instead_of_denying() {
    if std::env::var_os("LETS_LIVE_SMOKE").is_none_or(|value| value != "1") {
        return;
    }
    let temp = TempDir::new().expect("a temp directory");
    let root = temp.path();

    let install_home = root.join("install-home");
    fs::create_dir_all(&install_home).expect("an install home");
    let lets_dir = Path::new(LETS)
        .parent()
        .expect("the lets binary has a directory");
    let inherited = std::env::var("PATH").unwrap_or_default();
    let install = Command::new(LETS)
        .args(["hooks", "install", "claude-code"])
        .env("HOME", &install_home)
        .env("XDG_RUNTIME_DIR", &install_home)
        .env("XDG_DATA_HOME", install_home.join(".local/share"))
        .env("PATH", format!("{}:{inherited}", lets_dir.display()))
        .output()
        .expect("lets runs");
    assert!(install.status.success(), "{}", stderr(&install));
    let mod_dir = install_home.join(".local/share/lets/claude-code");
    assert!(
        mod_dir.join("hooks/register.ts").is_file(),
        "{}",
        mod_dir.display()
    );

    let sed_prompt =
        format!("Use the Bash tool to run exactly `{LIVE_SED_COMMAND}`, then reply done.");
    let (cat_with, _) = live_arm(root, "with-mod-cat", LIVE_CAT_PROMPT, Some(&mod_dir));
    let (sed_with, _) = live_arm(root, "with-mod-sed", &sed_prompt, Some(&mod_dir));
    let (cat_control, _) = live_arm(root, "control-cat", LIVE_CAT_PROMPT, None);
    let (sed_control, _) = live_arm(root, "control-sed", &sed_prompt, None);

    let total: f64 = [&cat_with, &sed_with, &cat_control, &sed_control]
        .iter()
        .map(|run| run.cost())
        .sum();
    let cost_line = format!(
        "live smoke cost: with-mod cat ${:.4}, sed ${:.4}; control cat ${:.4}, sed ${:.4}; total ${total:.4}",
        cat_with.cost(),
        sed_with.cost(),
        cat_control.cost(),
        sed_control.cost()
    );
    eprintln!("{cost_line}");

    let mut failures = live_failures(&cat_with, &sed_with);
    failures.extend(live_control_failures(&cat_control, &sed_control));
    if total > LIVE_BUDGET_USD {
        failures.push(format!(
            "over the ${LIVE_BUDGET_USD:.2} budget: {cost_line}"
        ));
    }
    if !failures.is_empty() {
        let kept = temp.keep();
        panic!(
            "live smoke failed:\n  {}\nraw streams: {}",
            failures.join("\n  "),
            kept.display()
        );
    }
}
