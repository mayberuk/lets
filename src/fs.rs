use std::io::Read as _;
use std::path::{Path, PathBuf};

use crate::error::{Error, UnsupportedReason};
use crate::output::Sha12;

#[derive(Debug)]
pub struct ReadFile {
    pub content: String,
    pub bytes: usize,
}

impl ReadFile {
    /// Not hashed on read: `transform` hashes the bytes itself, so it would be computed twice.
    pub fn sha(&self) -> Sha12 {
        Sha12::parse(blake3::hash(self.content.as_bytes()).to_hex().as_str())
            .expect("blake3's 64-hex digest is well over the 12-hex floor")
    }
}

pub(crate) const BINARY_SNIFF_WINDOW: usize = 8192;

pub fn read(path: &Path, max_file_bytes: u64) -> Result<ReadFile, Error> {
    let metadata = std::fs::metadata(path).map_err(|source| io_error(path, source))?;
    // A FIFO, device or socket reports length 0 and then blocks under `std::fs::read`, so the type
    // is settled before the size cap.
    if metadata.is_dir() {
        return Err(Error::Unsupported {
            path: path.to_path_buf(),
            reason: UnsupportedReason::Directory,
        });
    }
    if !metadata.file_type().is_file() {
        return Err(Error::Unsupported {
            path: path.to_path_buf(),
            reason: UnsupportedReason::Binary,
        });
    }
    read_regular(path, max_file_bytes)
}

/// Re-checks the type on the open handle: the path may have become a FIFO since `read` typed it.
pub fn read_regular(path: &Path, max_file_bytes: u64) -> Result<ReadFile, Error> {
    let mut file = std::fs::File::open(path).map_err(|source| io_error(path, source))?;
    let metadata = file.metadata().map_err(|source| io_error(path, source))?;
    if !metadata.file_type().is_file() {
        return Err(Error::Unsupported {
            path: path.to_path_buf(),
            reason: UnsupportedReason::Binary,
        });
    }
    if metadata.len() > max_file_bytes {
        return Err(Error::Unsupported {
            path: path.to_path_buf(),
            reason: UnsupportedReason::TooLarge {
                bytes: metadata.len(),
                limit: max_file_bytes,
            },
        });
    }

    let mut raw = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.read_to_end(&mut raw)
        .map_err(|source| io_error(path, source))?;
    let sniff_end = raw.len().min(BINARY_SNIFF_WINDOW);
    if raw[..sniff_end].contains(&0) {
        return Err(Error::Unsupported {
            path: path.to_path_buf(),
            reason: UnsupportedReason::Binary,
        });
    }

    let bytes = raw.len();
    let content = String::from_utf8(raw).map_err(|_| Error::Unsupported {
        path: path.to_path_buf(),
        reason: UnsupportedReason::NonUtf8Region,
    })?;
    Ok(ReadFile { content, bytes })
}

/// A courtesy against a mis-pasted path, not a security boundary. The refusal names the path as
/// typed, since the canonical form differs between machines.
pub fn guard_scope(path: &Path, allow_outside: bool) -> Result<PathBuf, Error> {
    let canonical = canonical_or_nearest(path)?;
    if allow_outside || canonical.starts_with(tree_root()) {
        Ok(canonical)
    } else {
        Err(Error::OutsideTree {
            path: path.to_path_buf(),
        })
    }
}

/// A path `write` will create is resolved against its nearest existing ancestor.
fn canonical_or_nearest(path: &Path) -> Result<PathBuf, Error> {
    let source = match std::fs::canonicalize(path) {
        Ok(canonical) => return Ok(canonical),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => source,
        Err(source) => return Err(io_error(path, source)),
    };
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io_error(path, source));
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    Ok(canonical_or_nearest(parent)?.join(name))
}

pub fn tree_root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let mut dir = cwd.clone();
    loop {
        if dir.join(".git").exists() {
            return dir;
        }
        if !dir.pop() {
            return cwd;
        }
    }
}

fn io_error(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::own_process::{repo, workdir};

    fn write_file(dir: &TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).expect("test fixture writes");
        path
    }

    #[test]
    fn whole_utf8_file_under_the_limit_returns_content_bytes_and_sha() {
        let dir = TempDir::new().expect("temp dir");
        let path = write_file(&dir, "a.txt", b"hello\nworld\n");

        let file = read(&path, 64).expect("a small utf-8 file reads");

        assert_eq!(file.content, "hello\nworld\n");
        assert_eq!(file.bytes, 12);
        assert_eq!(file.sha().as_str().len(), 12);
        assert!(file.sha().as_str().bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn a_file_exactly_at_max_file_bytes_succeeds() {
        let dir = TempDir::new().expect("temp dir");
        let path = write_file(&dir, "exact.txt", b"0123456789");

        let file = read(&path, 10).expect("a file at the limit still reads");

        assert_eq!(file.bytes, 10);
    }

    #[test]
    fn a_file_over_max_file_bytes_is_too_large() {
        let dir = TempDir::new().expect("temp dir");
        let path = write_file(&dir, "big.txt", b"0123456789");

        let err = read(&path, 9).expect_err("over the limit is refused");

        assert!(matches!(err, Error::Unsupported {
            reason: UnsupportedReason::TooLarge {
                bytes: 10,
                limit: 9
            },
            ..
        }));
    }

    #[test]
    fn a_nul_byte_in_the_first_8_kib_is_binary() {
        let dir = TempDir::new().expect("temp dir");
        let path = write_file(&dir, "bin.dat", b"abc\0def");

        let err = read(&path, 64).expect_err("a NUL byte refuses as binary");

        assert!(matches!(err, Error::Unsupported {
            reason: UnsupportedReason::Binary,
            ..
        }));
    }

    #[test]
    fn an_empty_file_reads_as_zero_bytes_with_the_published_blake3_empty_digest() {
        let dir = TempDir::new().expect("temp dir");
        let path = write_file(&dir, "empty.txt", b"");

        let file = read(&path, 64).expect("an empty file reads");

        assert_eq!(file.bytes, 0);
        assert_eq!(file.content, "");
        // BLAKE3's published empty-input vector: the one digest not anchored on this code.
        assert_eq!(file.sha().as_str(), "af1349b9f5f9");
    }

    #[test]
    fn a_nul_just_past_the_8_kib_sniff_window_is_not_binary() {
        let dir = TempDir::new().expect("temp dir");
        let mut bytes = vec![b'a'; BINARY_SNIFF_WINDOW];
        bytes.push(0);
        let path = write_file(&dir, "late-nul.txt", &bytes);

        let file = read(&path, 16_384).expect("a NUL outside the sniff window is not binary");

        assert_eq!(file.bytes, BINARY_SNIFF_WINDOW + 1);
    }

    #[test]
    fn a_nul_on_the_last_byte_of_the_sniff_window_is_binary() {
        let dir = TempDir::new().expect("temp dir");
        let mut bytes = vec![b'a'; BINARY_SNIFF_WINDOW - 1];
        bytes.push(0);
        let path = write_file(&dir, "edge-nul.dat", &bytes);

        let err = read(&path, 16_384).expect_err("the last byte of the window still counts");

        assert!(matches!(err, Error::Unsupported {
            reason: UnsupportedReason::Binary,
            ..
        }));
    }

    #[test]
    fn binary_beats_non_utf8_when_a_file_is_both() {
        let dir = TempDir::new().expect("temp dir");
        let path = write_file(&dir, "both.dat", &[0x00, 0xff]);

        let err = read(&path, 64).expect_err("a NUL and invalid utf-8 together are refused");

        assert!(matches!(err, Error::Unsupported {
            reason: UnsupportedReason::Binary,
            ..
        }));
    }

    #[test]
    fn a_directory_is_not_a_regular_file() {
        let dir = TempDir::new().expect("temp dir");
        let inner = dir.path().join("sub");
        std::fs::create_dir(&inner).expect("a directory to point read at");

        let err = read(&inner, 64).expect_err("a directory has no content to show");

        assert!(matches!(err, Error::Unsupported {
            reason: UnsupportedReason::Directory,
            ..
        }));
        assert!(err.to_string().contains("lets find"), "{err}");
    }

    #[test]
    fn a_fifo_is_still_binary_not_a_directory() {
        let dir = TempDir::new().expect("temp dir");
        let fifo = dir.path().join("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo runs");
        assert!(made.success(), "mkfifo creates the pipe");

        let err = read(&fifo, 64).expect_err("a FIFO has no content to show");

        assert!(matches!(err, Error::Unsupported {
            reason: UnsupportedReason::Binary,
            ..
        }));
    }

    #[test]
    fn a_symlink_to_a_directory_is_not_a_regular_file() {
        let dir = TempDir::new().expect("temp dir");
        let inner = dir.path().join("sub");
        std::fs::create_dir(&inner).expect("a directory to point the link at");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&inner, &link).expect("a symlink to that directory");

        let err = read(&link, 64).expect_err("the link resolves to a directory");

        assert!(matches!(err, Error::Unsupported {
            reason: UnsupportedReason::Directory,
            ..
        }));
    }

    #[test]
    fn read_regular_checks_the_type_and_size_on_the_open_handle() {
        let dir = TempDir::new().expect("temp dir");
        let inner = dir.path().join("sub");
        std::fs::create_dir(&inner).expect("a directory to open");
        let path = write_file(&dir, "big.txt", b"0123456789");

        let err = read_regular(&inner, 64).expect_err("a directory has no content to show");
        assert!(matches!(err, Error::Unsupported {
            reason: UnsupportedReason::Binary,
            ..
        }));
        let err = read_regular(&path, 9).expect_err("over the limit is refused");
        assert!(matches!(err, Error::Unsupported {
            reason: UnsupportedReason::TooLarge {
                bytes: 10,
                limit: 9
            },
            ..
        }));
        let file = read_regular(&path, 10).expect("a file at the limit reads");
        assert_eq!(file.content, "0123456789");
    }

    #[test]
    fn a_regular_file_the_size_gate_would_refuse_is_still_refused_for_its_size() {
        let dir = TempDir::new().expect("temp dir");
        let path = write_file(&dir, "big.txt", b"0123456789");

        let err = read(&path, 4).expect_err("the file-type gate does not swallow the size gate");

        assert!(matches!(err, Error::Unsupported {
            reason: UnsupportedReason::TooLarge { .. },
            ..
        }));
    }

    #[test]
    fn invalid_utf8_bytes_are_a_non_utf8_region() {
        let dir = TempDir::new().expect("temp dir");
        let path = write_file(&dir, "invalid.dat", &[0xff, 0xfe, b'a', b'b']);

        let err = read(&path, 64).expect_err("invalid utf-8 is refused");

        assert!(matches!(err, Error::Unsupported {
            reason: UnsupportedReason::NonUtf8Region,
            ..
        }));
    }

    #[test]
    fn a_missing_path_is_io_not_found() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("missing.txt");

        let err = read(&path, 64).expect_err("a missing file is refused");

        match err {
            Error::Io { source, .. } => assert_eq!(source.kind(), std::io::ErrorKind::NotFound),
            other => panic!("expected Error::Io, got {other:?}"),
        }
    }

    /// The `show` goldens pin these `sha:` literals, so they are anchored on blake3 itself.
    #[test]
    fn the_show_goldens_pin_blake3_of_the_read_fixture_s_bytes() {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/read");
        for (name, expected) in [
            ("small.md", "bc4ff3a9b973"),
            ("big.ts", "e37232c09d01"),
            ("store.go", "7aacc2f489c8"),
        ] {
            let path = fixtures.join(name);
            let bytes = std::fs::read(&path).expect("a committed fixture");
            assert_eq!(
                &blake3::hash(&bytes).to_hex().as_str()[..12],
                expected,
                "{name}"
            );
            assert_eq!(
                read(&path, 8_388_608)
                    .expect("the fixture reads")
                    .sha()
                    .as_str(),
                expected,
                "{name}: the reader and the hash of the bytes must agree"
            );
        }
    }

    #[test]
    fn a_path_inside_the_current_git_work_tree_succeeds() {
        let Some(repo) = repo() else { return };
        let path = repo.path().join("src.rs");
        std::fs::write(&path, b"fn main() {}").expect("test fixture writes");

        let canonical = guard_scope(&path, false).expect("a path inside the tree is allowed");

        assert_eq!(
            canonical,
            std::fs::canonicalize(&path).expect("the fixture path exists")
        );
    }

    #[test]
    fn tree_root_is_the_work_tree_guard_scope_confines_to() {
        let Some(repo) = repo() else { return };
        let root = std::fs::canonicalize(repo.path()).expect("the repo exists");

        assert_eq!(tree_root(), root);
    }

    #[test]
    fn a_path_outside_the_current_git_work_tree_is_refused() {
        let Some(_repo) = repo() else { return };
        let outside = TempDir::new().expect("a second, unrelated temp dir");
        let path = write_file(&outside, "elsewhere.rs", b"fn main() {}");

        let err = guard_scope(&path, false).expect_err("a path outside the tree is refused");

        assert!(matches!(err, Error::OutsideTree { .. }));
    }

    #[test]
    fn allow_outside_bypasses_the_refusal() {
        let Some(_repo) = repo() else { return };
        let outside = TempDir::new().expect("a second, unrelated temp dir");
        let path = write_file(&outside, "elsewhere.rs", b"fn main() {}");

        let canonical = guard_scope(&path, true).expect("--allow-outside steps around the guard");

        assert_eq!(
            canonical,
            std::fs::canonicalize(&path).expect("the fixture path exists")
        );
    }

    #[test]
    fn no_git_repo_falls_back_to_the_current_directory() {
        let Some(dir) = workdir(Path::to_path_buf) else {
            return;
        };
        let path = dir.path().join("a.rs");
        std::fs::write(&path, b"fn main() {}").expect("test fixture writes");

        let canonical = guard_scope(&path, false).expect("cwd itself is the fallback root");

        assert_eq!(
            canonical,
            std::fs::canonicalize(&path).expect("the fixture path exists")
        );
    }

    fn nested_repo() -> Option<(PathBuf, PathBuf)> {
        let repo = workdir(|parent| {
            let repo = parent.join("repo");
            std::fs::create_dir(&repo).expect("the repo dir");
            std::fs::create_dir(repo.join(".git")).expect("fake .git dir");
            std::fs::write(parent.join("outside.txt"), b"out").expect("the outside file");
            repo
        })?;
        let parent = repo
            .path()
            .parent()
            .expect("the repo has a parent")
            .to_path_buf();
        Some((parent, repo.path().to_path_buf()))
    }

    #[test]
    fn the_refusal_names_the_path_as_typed_not_its_canonical_form() {
        let Some((parent, _repo)) = nested_repo() else {
            return;
        };

        let err =
            guard_scope(Path::new("../outside.txt"), false).expect_err("`..` leaves the tree");

        match &err {
            Error::OutsideTree { path } => assert_eq!(path, Path::new("../outside.txt")),
            other => panic!("expected Error::OutsideTree, got {other:?}"),
        }
        let rendered = err.to_string();
        assert_eq!(rendered, "../outside.txt is outside the working tree");
        assert!(
            !rendered.contains(parent.to_str().expect("temp dirs are UTF-8")),
            "an absolute temp path in the message would differ per machine: {rendered:?}"
        );
    }

    #[test]
    fn a_path_that_does_not_exist_yet_is_judged_by_its_nearest_existing_ancestor() {
        let Some((_parent, repo)) = nested_repo() else {
            return;
        };
        let root = std::fs::canonicalize(&repo).expect("the repo dir exists");

        let inside = guard_scope(Path::new("new/sub/f.txt"), false)
            .expect("a path under the tree is allowed before it exists");

        assert_eq!(inside, root.join("new/sub/f.txt"));

        let err = guard_scope(Path::new("../not-there-yet.txt"), false)
            .expect_err("a path that does not exist yet still leaves the tree");

        assert!(matches!(err, Error::OutsideTree { .. }), "{err:?}");
    }

    #[test]
    fn a_symlink_pointing_out_of_the_tree_is_refused() {
        let Some((parent, repo)) = nested_repo() else {
            return;
        };
        std::os::unix::fs::symlink(parent.join("outside.txt"), repo.join("link.txt"))
            .expect("a symlink out of the tree");

        let err = guard_scope(Path::new("link.txt"), false)
            .expect_err("canonicalizing resolves the link out of the tree");

        match err {
            Error::OutsideTree { path } => assert_eq!(path, Path::new("link.txt")),
            other => panic!("expected Error::OutsideTree, got {other:?}"),
        }
    }

    #[test]
    fn a_worktree_style_dot_git_file_names_the_root() {
        let Some(nested) = workdir(|parent| {
            let worktree = parent.join("wt");
            std::fs::create_dir(&worktree).expect("the worktree dir");
            // A linked worktree's `.git` is a file, not a directory.
            std::fs::write(
                worktree.join(".git"),
                b"gitdir: /elsewhere/.git/worktrees/wt\n",
            )
            .expect("the worktree .git file");
            std::fs::write(worktree.join("a.rs"), b"fn main() {}").expect("a file at the root");
            let nested = worktree.join("src");
            std::fs::create_dir(&nested).expect("a dir below the root");
            nested
        }) else {
            return;
        };
        let worktree = nested.path().parent().expect("src sits in the worktree");

        let canonical = guard_scope(Path::new("../a.rs"), false)
            .expect("the .git file makes the worktree the root, so `../a.rs` is inside it");

        assert_eq!(
            canonical,
            std::fs::canonicalize(worktree.join("a.rs")).expect("the fixture path exists")
        );
    }
}
