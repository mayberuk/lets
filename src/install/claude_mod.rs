//! The Claude Code mod under `mod/`, embedded at build time and installed as plain files.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::atomic;
use crate::error::Error;

/// `mod/tests/` is never installed: Claude Code would load nothing from it.
const FILES: [(&str, &str); 5] = [
    (
        ".claude-plugin/plugin.json",
        include_str!("../../mod/.claude-plugin/plugin.json"),
    ),
    (
        "hooks/hooks.json",
        include_str!("../../mod/hooks/hooks.json"),
    ),
    (
        "hooks/register.ts",
        include_str!("../../mod/hooks/register.ts"),
    ),
    ("hooks/steer.ts", include_str!("../../mod/hooks/steer.ts")),
    ("tsconfig.json", include_str!("../../mod/tsconfig.json")),
];

/// Claude Code writes its type declarations here; nothing else does.
const TYPES_DIR: &str = ".claude-plugin/types";

pub fn install_dir() -> PathBuf {
    resolve(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

/// The XDG Base Directory spec says a relative `XDG_DATA_HOME` is invalid and must be ignored.
fn resolve(xdg_data_home: Option<OsString>, home: Option<OsString>) -> PathBuf {
    let data = match xdg_data_home.map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        _ => PathBuf::from(home.unwrap_or_default()).join(".local/share"),
    };
    data.join("lets/claude-code")
}

pub fn any_file_present(dir: &Path) -> bool {
    FILES
        .iter()
        .any(|(relative, _)| dir.join(relative).exists())
}

/// Returns whether any file changed; a file already holding its bytes is not rewritten.
pub fn write(dir: &Path) -> Result<bool, Error> {
    let mut changed = false;
    for (relative, contents) in FILES {
        let path = dir.join(relative);
        if std::fs::read(&path).is_ok_and(|bytes| bytes == contents.as_bytes()) {
            continue;
        }
        let parent = path.parent().unwrap_or(dir);
        std::fs::create_dir_all(parent).map_err(|source| Error::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        atomic::write_atomic(&path, contents.as_bytes(), None)?;
        changed = true;
    }
    Ok(changed)
}

pub fn is_ours(dir: &Path) -> bool {
    std::fs::read(dir.join(".claude-plugin/plugin.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_some_and(|manifest| manifest["name"] == "lets")
}

fn tolerating(
    result: std::io::Result<()>,
    path: &Path,
    expected: &[std::io::ErrorKind],
) -> Result<(), Error> {
    match result {
        Err(source) if !expected.contains(&source.kind()) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
        _ => Ok(()),
    }
}

/// Removes only what lets or Claude Code wrote, so a file someone else put there keeps its
/// directory. A `plugin.json` that is missing or not ours means the directory is not ours either.
pub fn remove(dir: &Path) -> Result<bool, Error> {
    use std::io::ErrorKind::{DirectoryNotEmpty, NotFound};
    if !is_ours(dir) {
        return Ok(false);
    }
    for (relative, _) in FILES {
        let path = dir.join(relative);
        tolerating(std::fs::remove_file(&path), &path, &[NotFound])?;
    }
    let types = dir.join(TYPES_DIR);
    tolerating(std::fs::remove_dir_all(&types), &types, &[NotFound])?;
    for path in [
        dir.join("hooks"),
        dir.join(".claude-plugin"),
        dir.to_path_buf(),
    ] {
        tolerating(std::fs::remove_dir(&path), &path, &[
            NotFound,
            DirectoryNotEmpty,
        ])?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt as _;

    use tempfile::TempDir;

    use super::*;
    use crate::verbs::hooks::CLAUDE_CODE_PARAGRAPH;

    fn embedded(relative: &str) -> &'static str {
        FILES
            .iter()
            .find(|(path, _)| *path == relative)
            .unwrap_or_else(|| panic!("{relative} is not embedded"))
            .1
    }

    fn installed() -> (TempDir, PathBuf) {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("lets/claude-code");
        assert!(write(&dir).unwrap());
        (root, dir)
    }

    #[test]
    fn every_file_lands_byte_identical_to_the_mod_directory() {
        let (_root, dir) = installed();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("mod");

        for (relative, _) in FILES {
            assert_eq!(
                std::fs::read(dir.join(relative)).unwrap(),
                std::fs::read(source.join(relative)).unwrap(),
                "{relative}"
            );
        }
    }

    #[test]
    fn a_second_write_changes_nothing_on_disk() {
        let (_root, dir) = installed();
        let before: Vec<(u64, i64, i64)> = FILES
            .iter()
            .map(|(relative, _)| {
                let meta = std::fs::metadata(dir.join(relative)).unwrap();
                (meta.ino(), meta.mtime(), meta.mtime_nsec())
            })
            .collect();

        assert!(!write(&dir).unwrap());

        let after: Vec<(u64, i64, i64)> = FILES
            .iter()
            .map(|(relative, _)| {
                let meta = std::fs::metadata(dir.join(relative)).unwrap();
                (meta.ino(), meta.mtime(), meta.mtime_nsec())
            })
            .collect();
        assert_eq!(before, after);
    }

    #[test]
    fn a_changed_file_is_rewritten_and_reported() {
        let (_root, dir) = installed();
        std::fs::write(dir.join("hooks/steer.ts"), "stale").unwrap();

        assert!(write(&dir).unwrap());

        assert_eq!(
            std::fs::read_to_string(dir.join("hooks/steer.ts")).unwrap(),
            embedded("hooks/steer.ts")
        );
    }

    #[test]
    fn remove_deletes_what_lets_and_claude_code_wrote_and_keeps_a_foreign_file() {
        let (root, dir) = installed();
        std::fs::create_dir_all(dir.join(".claude-plugin/types/claude-code")).unwrap();
        std::fs::write(dir.join(".claude-plugin/types/claude-code/index.d.ts"), "x").unwrap();
        std::fs::write(dir.join("hooks/mine.ts"), "theirs").unwrap();

        assert!(remove(&dir).unwrap());

        for (relative, _) in FILES {
            assert!(!dir.join(relative).exists(), "{relative}");
        }
        assert!(!dir.join(".claude-plugin").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("hooks/mine.ts")).unwrap(),
            "theirs"
        );
        assert!(root.path().join("lets").is_dir());
    }

    #[test]
    fn remove_takes_the_directory_itself_once_it_is_empty() {
        let (root, dir) = installed();

        assert!(remove(&dir).unwrap());

        assert!(!dir.exists());
        assert!(root.path().join("lets").is_dir());
    }

    #[test]
    fn remove_leaves_a_directory_whose_plugin_json_names_another_plugin() {
        let (_root, dir) = installed();
        let foreign = r#"{"name": "other", "version": "1.0.0"}"#;
        std::fs::write(dir.join(".claude-plugin/plugin.json"), foreign).unwrap();

        assert!(!remove(&dir).unwrap());

        assert_eq!(
            std::fs::read_to_string(dir.join(".claude-plugin/plugin.json")).unwrap(),
            foreign
        );
        for (relative, _) in &FILES[1..] {
            assert!(dir.join(relative).exists(), "{relative}");
        }
    }

    #[test]
    fn remove_with_a_malformed_plugin_json_touches_nothing() {
        let (_root, dir) = installed();
        for malformed in ["{ not json", "", "[]", r#"{"name": "#] {
            std::fs::write(dir.join(".claude-plugin/plugin.json"), malformed).unwrap();

            assert!(!remove(&dir).unwrap(), "{malformed:?}");

            assert_eq!(
                std::fs::read_to_string(dir.join(".claude-plugin/plugin.json")).unwrap(),
                malformed
            );
            for (relative, _) in &FILES[1..] {
                assert!(dir.join(relative).exists(), "{malformed:?} {relative}");
            }
        }
    }

    #[test]
    fn remove_without_a_plugin_json_touches_nothing() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("lets/claude-code");
        std::fs::create_dir_all(dir.join("hooks")).unwrap();
        std::fs::write(dir.join("hooks/steer.ts"), "x").unwrap();

        assert!(!remove(&dir).unwrap());

        assert!(dir.join("hooks/steer.ts").exists());
    }

    #[test]
    fn an_absolute_xdg_data_home_is_used() {
        assert_eq!(
            resolve(Some("/data".into()), Some("/home/u".into())),
            Path::new("/data/lets/claude-code")
        );
    }

    #[test]
    fn a_relative_or_empty_xdg_data_home_falls_back_to_home() {
        for xdg in [Some("data"), Some(""), None] {
            assert_eq!(
                resolve(xdg.map(OsString::from), Some("/home/u".into())),
                Path::new("/home/u/.local/share/lets/claude-code"),
                "{xdg:?}"
            );
        }
    }

    #[test]
    fn plugin_json_carries_the_crate_version() {
        let manifest: serde_json::Value =
            serde_json::from_str(embedded(".claude-plugin/plugin.json")).unwrap();
        assert_eq!(
            manifest["version"],
            env!("CARGO_PKG_VERSION"),
            "mod/.claude-plugin/plugin.json version drifted from Cargo.toml"
        );
        assert_eq!(manifest["name"], "lets");
    }

    #[test]
    fn steer_ts_carries_the_crate_version() {
        let line = format!("export const VERSION = \"{}\";", env!("CARGO_PKG_VERSION"));
        assert!(
            embedded("hooks/steer.ts").lines().any(|held| held == line),
            "mod/hooks/steer.ts lacks `{line}`"
        );
    }

    #[test]
    fn steer_ts_carries_the_claude_code_paragraph() {
        let line = format!(
            "export const LETS_TABLE = {};",
            serde_json::to_string(CLAUDE_CODE_PARAGRAPH).unwrap()
        );
        assert!(
            embedded("hooks/steer.ts").lines().any(|held| held == line),
            "mod/hooks/steer.ts LETS_TABLE drifted from CLAUDE_CODE_PARAGRAPH in src/verbs/hooks.rs"
        );
    }
}
