pub mod atomic;
pub mod batch;
pub mod check;
pub mod cli;
pub mod error;
pub mod fs;
pub mod grammars;
pub mod hook;
pub mod install;
pub mod lock;
pub mod matcher;
pub mod normalize;
pub mod output;
pub mod symbols;
pub mod target;
pub mod transform;
pub mod verbs;
pub mod window;

pub use error::Error;

/// The working directory is per process and `fs::guard_scope` reads the tree root from it, so a
/// test needing its own cwd re-runs alone in a child rather than changing it under other tests.
#[cfg(test)]
pub(crate) mod own_process {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use tempfile::TempDir;

    const CHILD: &str = "LETS_TEST_OWN_PROCESS";

    pub(crate) fn is_child() -> bool {
        std::env::var_os(CHILD).is_some()
    }

    pub(crate) fn rerun_in(cwd: &Path) {
        let runtime = TempDir::new().expect("a runtime dir for the child's locks");
        let thread = std::thread::current();
        let name = thread
            .name()
            .filter(|name| *name != "main")
            .expect("libtest runs each test on a thread named after it");
        let run = Command::new(std::env::current_exe().expect("the test binary"))
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .current_dir(cwd)
            .env(CHILD, "1")
            .env("XDG_RUNTIME_DIR", runtime.path())
            .output()
            .expect("the test binary re-runs itself");
        let out = String::from_utf8_lossy(&run.stdout);
        let err = String::from_utf8_lossy(&run.stderr);
        // `--exact` with no matching test still exits 0; only the pass count proves it ran.
        assert!(
            run.status.success() && out.contains(" 1 passed"),
            "{name} in its own process:\n{out}\n{err}"
        );
    }

    pub(crate) struct Workdir(PathBuf);

    impl Workdir {
        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    /// `None` in the parent, which has already run the child.
    pub(crate) fn workdir(setup: impl FnOnce(&Path) -> PathBuf) -> Option<Workdir> {
        if is_child() {
            return Some(Workdir(
                std::env::current_dir().expect("the child starts in its working directory"),
            ));
        }
        let root = TempDir::new().expect("temp dir");
        rerun_in(&setup(root.path()));
        None
    }

    pub(crate) fn repo() -> Option<Workdir> {
        workdir(|root| {
            std::fs::create_dir(root.join(".git")).expect("fake .git dir");
            root.to_path_buf()
        })
    }
}

/// A non-zero exit still prints content (e.g. `show` exits 1 with the targets that resolved).
/// Lives in the crate root because neither `output` nor `error` may import the other.
#[derive(Debug)]
pub struct Outcome {
    pub response: output::Response,
    pub error: Option<Error>,
}

impl Outcome {
    pub fn ok(response: output::Response) -> Outcome {
        Outcome {
            response,
            error: None,
        }
    }

    pub fn partial(response: output::Response, error: Error) -> Outcome {
        Outcome {
            response,
            error: Some(error),
        }
    }

    pub fn failed(verb: &'static str, error: Error) -> Outcome {
        Outcome {
            response: output::Response::empty(verb),
            error: Some(error),
        }
    }
}
