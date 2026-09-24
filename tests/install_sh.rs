use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

struct Env {
    _root: TempDir,
    bin: PathBuf,
    home: PathBuf,
    log: PathBuf,
}

impl Env {
    fn new() -> Env {
        let root = TempDir::new().expect("a temp directory for the sandbox");
        let bin = root.path().join("bin");
        let home = root.path().join("home");
        fs::create_dir_all(&bin).expect("the sandbox bin directory");
        fs::create_dir_all(&home).expect("the sandbox home directory");
        Env {
            log: root.path().join("calls.log"),
            _root: root,
            bin,
            home,
        }
    }

    fn install_fake(&self, name: &str, fixture: &str) {
        let source = fixture_path(fixture);
        let target = self.bin.join(name);
        fs::copy(&source, &target)
            .unwrap_or_else(|error| panic!("copying {}: {error}", source.display()));
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|error| panic!("chmod {}: {error}", target.display()));
    }

    fn log_lines(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The developer's own PATH may hold a real `claude` or `codex`, which would leak into agent
    /// detection.
    fn configure<'a>(
        &self,
        command: &'a mut Command,
        extra_env: &[(&str, &str)],
    ) -> &'a mut Command {
        let path = format!("{}:/usr/bin:/bin", self.bin.display());
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", path)
            .env("LETS_TEST_LOG", &self.log)
            .stdin(Stdio::null());
        for (key, value) in extra_env {
            command.env(key, value);
        }
        command
    }

    /// A controlling terminal would still let `/dev/tty` open despite a null stdin; `setsid`
    /// detaches it, falling back to bare `sh` where there is no `setsid` (macOS).
    fn run(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        let mut with_setsid = Command::new("setsid");
        with_setsid.arg("sh").arg(install_sh_path()).args(args);
        match self.configure(&mut with_setsid, extra_env).output() {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut plain = Command::new("sh");
                plain.arg(install_sh_path()).args(args);
                self.configure(&mut plain, extra_env)
                    .output()
                    .expect("sh runs install.sh")
            },
            Err(error) => panic!("running install.sh under setsid: {error}"),
        }
    }
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/install_sh")
        .join(name)
}

fn install_sh_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("install.sh")
}

fn code(output: &Output) -> i32 {
    output
        .status
        .code()
        .expect("install.sh exits, never signals")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("install.sh writes utf-8 stdout")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("install.sh writes utf-8 stderr")
}

fn with_agents(env: &Env) {
    env.install_fake("claude", "agent-marker");
    env.install_fake("codex", "agent-marker");
}

#[test]
fn fresh_install_downloads_and_runs_the_release_installer() {
    let env = Env::new();
    env.install_fake("curl", "curl");
    let install_dir = env.bin.display().to_string();

    let output = env.run(&[], &[
        (
            "LETS_TEST_INSTALLER",
            &fixture_path("installer.sh").display().to_string(),
        ),
        (
            "LETS_TEST_FAKE_LETS",
            &fixture_path("lets-ok").display().to_string(),
        ),
        ("LETS_BIN_DIR", &install_dir),
    ]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let calls = env.log_lines();
    assert!(
        calls.iter().any(|line| line.starts_with("curl ")
            && line.contains("releases/latest/download/lets-installer.sh")),
        "{calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|line| line == &format!("installer wrote lets to {install_dir}")),
        "{calls:?}"
    );
    let installed = env.bin.join("lets");
    assert!(installed.is_file());
    let mode = fs::metadata(&installed).unwrap().permissions().mode();
    assert_ne!(mode & 0o111, 0, "the installed lets must be executable");
}

/// The fake installer follows dist 0.32's precedence: `LETS_INSTALL_DIR`, then
/// `CARGO_DIST_FORCE_INSTALL_DIR`, each a prefix it appends `bin/` to, then the flat
/// `LETS_UNMANAGED_INSTALL`.
#[test]
fn a_dist_install_dir_in_the_environment_does_not_move_the_binary() {
    let env = Env::new();
    env.install_fake("curl", "curl");
    let prefix = env.home.join("prefix");
    let prefix = prefix.display().to_string();

    for dist_var in ["LETS_INSTALL_DIR", "CARGO_DIST_FORCE_INSTALL_DIR"] {
        let output = env.run(&["--no-hooks"], &[
            (
                "LETS_TEST_INSTALLER",
                &fixture_path("installer.sh").display().to_string(),
            ),
            (
                "LETS_TEST_FAKE_LETS",
                &fixture_path("lets-ok").display().to_string(),
            ),
            ("LETS_BIN_DIR", &env.bin.display().to_string()),
            (dist_var, &prefix),
        ]);

        assert_eq!(code(&output), 0, "{dist_var}: {}", stderr(&output));
        assert!(env.bin.join("lets").is_file(), "{dist_var}");
        assert!(
            !env.home.join("prefix/bin/lets").exists(),
            "{dist_var} must not redirect the install"
        );
        fs::remove_file(env.bin.join("lets")).unwrap();
    }
}

#[test]
fn without_lets_bin_dir_the_binary_goes_to_home_local_bin() {
    let env = Env::new();
    env.install_fake("curl", "curl");

    let output = env.run(&["--no-hooks"], &[
        (
            "LETS_TEST_INSTALLER",
            &fixture_path("installer.sh").display().to_string(),
        ),
        (
            "LETS_TEST_FAKE_LETS",
            &fixture_path("lets-ok").display().to_string(),
        ),
    ]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let local_bin = env.home.join(".local/bin");
    assert!(local_bin.join("lets").is_file());
    assert!(
        stdout(&output).contains(&format!("export PATH=\"{}:$PATH\"", local_bin.display())),
        "{}",
        stdout(&output)
    );
}

#[test]
fn an_installer_that_installs_nothing_exits_1() {
    let env = Env::new();
    env.install_fake("curl", "curl");
    let install_dir = env.home.join("empty-bin");

    let output = env.run(&["--no-hooks"], &[
        (
            "LETS_TEST_INSTALLER",
            &fixture_path("installer-noop.sh").display().to_string(),
        ),
        ("LETS_BIN_DIR", &install_dir.display().to_string()),
    ]);

    assert_eq!(code(&output), 1, "{}", stdout(&output));
    assert!(
        stderr(&output).contains("no lets binary was found"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn already_latest_reports_and_does_not_download() {
    let env = Env::new();
    env.install_fake("lets", "lets-ok");

    let output = env.run(&[], &[("LETS_FAKE_CHECK_EXIT", "0")]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(env.log_lines(), [
        "lets version --json",
        "lets update --check"
    ]);
}

#[test]
fn an_older_release_runs_lets_update() {
    let env = Env::new();
    env.install_fake("lets", "lets-ok");

    let output = env.run(&[], &[("LETS_FAKE_CHECK_EXIT", "1")]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(env.log_lines(), [
        "lets version --json",
        "lets update --check",
        "lets update"
    ]);
}

#[test]
fn a_foreign_lets_is_refused_and_named_without_touching_the_network() {
    let env = Env::new();
    env.install_fake("lets", "lets-foreign");

    let output = env.run(&[], &[]);

    assert_eq!(code(&output), 1);
    let foreign_path = env.bin.join("lets").display().to_string();
    let message = stderr(&output);
    assert!(message.contains(&foreign_path), "{message}");
    assert!(message.contains("not this project's build"), "{message}");
    assert_eq!(env.log_lines(), ["foreign version --json"]);
}

#[test]
fn no_tty_skips_hooks_and_prints_the_install_command_instead() {
    let env = Env::new();
    env.install_fake("lets", "lets-ok");
    with_agents(&env);

    let output = env.run(&[], &[("LETS_FAKE_CHECK_EXIT", "0")]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let printed = stdout(&output);
    assert!(
        printed.contains("lets hooks install claude-code"),
        "{printed}"
    );
    assert!(printed.contains("lets hooks install codex"), "{printed}");
    assert_eq!(env.log_lines(), [
        "lets version --json",
        "lets update --check"
    ]);
}

#[test]
fn hooks_flag_installs_only_the_named_agent_once() {
    let env = Env::new();
    env.install_fake("lets", "lets-ok");
    with_agents(&env);

    let output = env.run(&["--hooks=claude-code"], &[("LETS_FAKE_CHECK_EXIT", "0")]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(env.log_lines(), [
        "lets version --json",
        "lets update --check",
        "lets hooks install claude-code"
    ]);
}

#[test]
fn no_hooks_flag_installs_nothing_and_asks_nothing() {
    let env = Env::new();
    env.install_fake("lets", "lets-ok");
    with_agents(&env);

    let output = env.run(&["--no-hooks"], &[("LETS_FAKE_CHECK_EXIT", "0")]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(env.log_lines(), [
        "lets version --json",
        "lets update --check"
    ]);
    assert!(
        !stdout(&output).contains("hooks install"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn uninstall_removes_hooks_for_each_agent_then_the_binary() {
    let env = Env::new();
    env.install_fake("lets", "lets-ok");
    with_agents(&env);
    let binary = env.bin.join("lets");

    let output = env.run(&["--uninstall"], &[]);

    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(env.log_lines(), [
        "lets version --json",
        "lets hooks uninstall claude-code",
        "lets hooks uninstall codex"
    ]);
    assert!(
        !binary.exists(),
        "the binary must be removed after uninstall"
    );
    assert!(stdout(&output).contains(&binary.display().to_string()));
}

#[test]
fn uninstall_with_a_foreign_lets_is_refused_and_the_binary_is_kept() {
    let env = Env::new();
    env.install_fake("lets", "lets-foreign");
    let binary = env.bin.join("lets");

    let output = env.run(&["--uninstall"], &[]);

    assert_eq!(code(&output), 1);
    assert!(binary.exists(), "a foreign binary must never be removed");
    assert_eq!(env.log_lines(), ["foreign version --json"]);
}
