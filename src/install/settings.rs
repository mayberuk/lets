//! Edits through `jsonc-parser`'s CST so every other key, comment and formatting byte survives.

use std::path::Path;

use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstArray, CstInputValue, CstNode, CstObject, CstObjectProp, CstRootNode};

use crate::error::Error;
use crate::{atomic, lock};

#[derive(Debug, PartialEq, Eq)]
pub enum InstallStatus {
    Installed,
    Updated,
    AlreadyInstalled,
}

pub struct HookEntry<'a> {
    pub event: &'a str,
    pub matcher: Option<&'a str>,
    pub command: &'a str,
    /// Recognises an earlier install's command, so a changed one replaces it instead of adding
    /// one.
    pub is_ours: fn(&str) -> bool,
}

/// The JSONC extensions Claude Code's settings.json accepts, and no more.
fn parse_options() -> ParseOptions {
    ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

fn invalid_data(path: &Path, detail: impl std::fmt::Display) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, detail.to_string()),
    }
}

fn read_text(path: &Path) -> Result<String, Error> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(source) => {
            return Err(Error::Io {
                path: path.to_path_buf(),
                source,
            });
        },
    };
    String::from_utf8(bytes).map_err(|err| invalid_data(path, err))
}

fn string_value(prop: &CstObjectProp) -> Option<String> {
    prop.value()?.as_string_lit()?.decoded_value().ok()
}

fn matcher_matches(entry_object: &CstObject, matcher: Option<&str>) -> bool {
    let Some(wanted) = matcher else { return true };
    entry_object
        .get("matcher")
        .and_then(|prop| string_value(&prop))
        .is_some_and(|existing| existing == wanted)
}

fn our_command_prop(array: &CstArray, entry: &HookEntry) -> Option<CstObjectProp> {
    array
        .elements()
        .iter()
        .filter_map(jsonc_parser::cst::CstNode::as_object)
        .filter(|entry_object| matcher_matches(entry_object, entry.matcher))
        .filter_map(|entry_object| entry_object.array_value("hooks"))
        .flat_map(|nested| nested.elements())
        .filter_map(|hook| hook.as_object()?.get("command"))
        .find(|prop| string_value(prop).is_some_and(|command| (entry.is_ours)(&command)))
}

fn entry_value(entry: &HookEntry) -> CstInputValue {
    let mut props = Vec::new();
    if let Some(matcher) = entry.matcher {
        props.push(("matcher".to_string(), CstInputValue::from(matcher)));
    }
    props.push((
        "hooks".to_string(),
        CstInputValue::Array(vec![CstInputValue::Object(vec![
            ("type".to_string(), CstInputValue::from("command")),
            ("command".to_string(), CstInputValue::from(entry.command)),
        ])]),
    ));
    CstInputValue::Object(props)
}

fn merge_one(
    hooks: &CstObject,
    entry: &HookEntry,
    settings_path: &Path,
) -> Result<InstallStatus, Error> {
    let event_array = hooks.array_value_or_create(entry.event).ok_or_else(|| {
        invalid_data(
            settings_path,
            format!("`hooks.{}` is not an array", entry.event),
        )
    })?;
    match our_command_prop(&event_array, entry) {
        Some(prop) if string_value(&prop).as_deref() == Some(entry.command) => {
            Ok(InstallStatus::AlreadyInstalled)
        },
        Some(prop) => {
            prop.set_value(CstInputValue::from(entry.command));
            Ok(InstallStatus::Updated)
        },
        None => {
            event_array.append(entry_value(entry));
            Ok(InstallStatus::Installed)
        },
    }
}

/// One lock spans read and write so concurrent installs cannot both append; every entry is merged
/// before the single write, so a shape error on any leaves the file untouched.
pub fn merge_hook_entries(
    settings_path: &Path,
    entries: &[HookEntry],
    runtime: &Path,
) -> Result<Vec<InstallStatus>, Error> {
    let _lock = lock::Lock::acquire(settings_path, runtime)?;
    let text = read_text(settings_path)?;
    let root = CstRootNode::parse(&text, &parse_options())
        .map_err(|err| invalid_data(settings_path, err))?;

    let top = root
        .object_value_or_create()
        .ok_or_else(|| invalid_data(settings_path, "the top-level value is not an object"))?;
    let hooks = top
        .object_value_or_create("hooks")
        .ok_or_else(|| invalid_data(settings_path, "`hooks` is not an object"))?;
    let statuses = entries
        .iter()
        .map(|entry| merge_one(&hooks, entry, settings_path))
        .collect::<Result<Vec<_>, _>>()?;

    if statuses
        .iter()
        .any(|status| *status != InstallStatus::AlreadyInstalled)
    {
        atomic::write_atomic(settings_path, root.to_string().as_bytes(), None)?;
    }
    Ok(statuses)
}

fn is_ours(hook: &CstNode, entry: &HookEntry) -> bool {
    hook.as_object()
        .and_then(|hook| hook.get("command"))
        .and_then(|prop| string_value(&prop))
        .is_some_and(|command| (entry.is_ours)(&command))
}

/// Any other key means the object was written or extended by hand, so it outlives its last hook.
fn has_only_install_keys(entry_object: &CstObject) -> bool {
    entry_object.properties().iter().all(|prop| {
        prop.name()
            .and_then(|name| name.decoded_value().ok())
            .is_some_and(|name| name == "matcher" || name == "hooks")
    })
}

/// An empty container means nothing to Claude Code, so it goes even if the user made it.
fn remove_one(hooks: &CstObject, entry: &HookEntry, settings_path: &Path) -> Result<bool, Error> {
    let Some(event_prop) = hooks.get(entry.event) else {
        return Ok(false);
    };
    let event_array = event_prop.array_value().ok_or_else(|| {
        invalid_data(
            settings_path,
            format!("`hooks.{}` is not an array", entry.event),
        )
    })?;
    let mut removed = false;
    for element in event_array.elements() {
        let Some(entry_object) = element.as_object() else {
            continue;
        };
        if !matcher_matches(&entry_object, entry.matcher) {
            continue;
        }
        let Some(nested) = entry_object.array_value("hooks") else {
            continue;
        };
        let ours: Vec<CstNode> = nested
            .elements()
            .into_iter()
            .filter(|hook| is_ours(hook, entry))
            .collect();
        if ours.is_empty() {
            continue;
        }
        removed = true;
        ours.into_iter().for_each(CstNode::remove);
        if nested.elements().is_empty() && has_only_install_keys(&entry_object) {
            element.remove();
        }
    }
    if removed && event_array.elements().is_empty() {
        event_prop.remove();
    }
    Ok(removed)
}

pub fn remove_hook_entries(
    settings_path: &Path,
    entries: &[HookEntry],
    runtime: &Path,
) -> Result<Vec<bool>, Error> {
    let nothing = vec![false; entries.len()];
    if !settings_path.exists() {
        return Ok(nothing);
    }
    let _lock = lock::Lock::acquire(settings_path, runtime)?;
    let text = read_text(settings_path)?;
    if text.trim().is_empty() {
        return Ok(nothing);
    }
    let root = CstRootNode::parse(&text, &parse_options())
        .map_err(|err| invalid_data(settings_path, err))?;
    let top = root
        .object_value()
        .ok_or_else(|| invalid_data(settings_path, "the top-level value is not an object"))?;
    let Some(hooks_prop) = top.get("hooks") else {
        return Ok(nothing);
    };
    let hooks = hooks_prop
        .object_value()
        .ok_or_else(|| invalid_data(settings_path, "`hooks` is not an object"))?;
    let removed = entries
        .iter()
        .map(|entry| remove_one(&hooks, entry, settings_path))
        .collect::<Result<Vec<_>, _>>()?;

    if removed.contains(&true) {
        if hooks.properties().is_empty() {
            hooks_prop.remove();
        }
        atomic::write_atomic(settings_path, root.to_string().as_bytes(), None)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    struct Sandbox {
        dir: TempDir,
        runtime: TempDir,
    }

    impl Sandbox {
        fn new(existing: Option<&str>) -> Sandbox {
            let runtime = TempDir::new().unwrap();
            let dir = TempDir::new().unwrap();
            if let Some(text) = existing {
                std::fs::write(dir.path().join("settings.json"), text).unwrap();
            }
            Sandbox { dir, runtime }
        }

        fn path(&self) -> std::path::PathBuf {
            self.dir.path().join("settings.json")
        }

        fn text(&self) -> String {
            std::fs::read_to_string(self.path()).unwrap()
        }

        fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.text()).unwrap()
        }
    }

    fn is_classify(command: &str) -> bool {
        command.ends_with("hook classify")
    }

    fn bash_classify(matcher: &'static str) -> HookEntry<'static> {
        HookEntry {
            event: "PreToolUse",
            matcher: Some(matcher),
            command: "lets hook classify",
            is_ours: is_classify,
        }
    }

    #[test]
    fn unrelated_keys_and_a_comment_survive_byte_identical_outside_the_touched_array() {
        let sandbox = Sandbox::new(Some(
            "{\n  // keep me\n  \"env\": {\"FOO\": \"bar\"},\n  \"hooks\": {}\n}\n",
        ));

        merge_hook_entries(
            &sandbox.path(),
            &[bash_classify("Bash")],
            sandbox.runtime.path(),
        )
        .unwrap();

        let result = sandbox.text();
        assert!(result.starts_with("{\n  // keep me\n  \"env\": {\"FOO\": \"bar\"},\n"));
    }

    #[test]
    fn a_missing_hooks_key_is_created_beside_the_existing_keys() {
        let sandbox = Sandbox::new(Some("{\"env\": {}}\n"));

        merge_hook_entries(
            &sandbox.path(),
            &[bash_classify("Bash")],
            sandbox.runtime.path(),
        )
        .unwrap();

        assert_eq!(
            sandbox.json(),
            serde_json::json!({
                "env": {},
                "hooks": {"PreToolUse": [
                    {"matcher": "Bash", "hooks": [{"type": "command", "command": "lets hook classify"}]}
                ]}
            })
        );
    }

    #[test]
    fn a_missing_file_is_created_holding_only_the_hooks_key() {
        let sandbox = Sandbox::new(None);

        let statuses = merge_hook_entries(
            &sandbox.path(),
            &[bash_classify("Bash")],
            sandbox.runtime.path(),
        )
        .unwrap();

        assert_eq!(statuses, [InstallStatus::Installed]);
        assert_eq!(
            sandbox.json(),
            serde_json::json!({"hooks": {"PreToolUse": [
                {"matcher": "Bash", "hooks": [{"type": "command", "command": "lets hook classify"}]}
            ]}})
        );
    }

    #[test]
    fn an_empty_file_is_treated_as_an_empty_object() {
        let sandbox = Sandbox::new(Some(""));

        let statuses = merge_hook_entries(
            &sandbox.path(),
            &[bash_classify("Bash")],
            sandbox.runtime.path(),
        )
        .unwrap();

        assert_eq!(statuses, [InstallStatus::Installed]);
        assert_eq!(
            sandbox.json()["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "lets hook classify"
        );
    }

    #[test]
    fn the_same_entry_installed_twice_is_installed_then_already_installed_with_identical_bytes() {
        let sandbox = Sandbox::new(Some("{}"));

        let first = merge_hook_entries(
            &sandbox.path(),
            &[bash_classify("Bash")],
            sandbox.runtime.path(),
        )
        .unwrap();
        let bytes_after_first = std::fs::read(sandbox.path()).unwrap();
        let second = merge_hook_entries(
            &sandbox.path(),
            &[bash_classify("Bash")],
            sandbox.runtime.path(),
        )
        .unwrap();

        assert_eq!(first, [InstallStatus::Installed]);
        assert_eq!(second, [InstallStatus::AlreadyInstalled]);
        assert_eq!(bytes_after_first, std::fs::read(sandbox.path()).unwrap());
    }

    #[test]
    fn two_matchers_for_the_same_event_are_two_elements() {
        let sandbox = Sandbox::new(Some("{}"));

        merge_hook_entries(
            &sandbox.path(),
            &[bash_classify("Bash"), bash_classify("Write")],
            sandbox.runtime.path(),
        )
        .unwrap();

        let matchers: Vec<_> = sandbox.json()["hooks"]["PreToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .map(|element| element["matcher"].clone())
            .collect();
        assert_eq!(matchers, ["Bash", "Write"]);
    }

    #[test]
    fn a_matcherless_event_omits_the_matcher_key() {
        let sandbox = Sandbox::new(Some("{}"));
        let entry = HookEntry {
            event: "SubagentStart",
            matcher: None,
            command: "printf '%s' '{}'",
            is_ours: |command| command.starts_with("printf"),
        };

        merge_hook_entries(&sandbox.path(), &[entry], sandbox.runtime.path()).unwrap();

        assert_eq!(
            sandbox.json(),
            serde_json::json!({"hooks": {"SubagentStart": [
                {"hooks": [{"type": "command", "command": "printf '%s' '{}'"}]}
            ]}})
        );
    }

    #[test]
    fn a_changed_command_of_ours_is_replaced_in_place_not_appended() {
        let sandbox = Sandbox::new(Some(
            r#"{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "/opt/lets hook classify"}]}]}}"#,
        ));

        let statuses = merge_hook_entries(
            &sandbox.path(),
            &[bash_classify("Bash")],
            sandbox.runtime.path(),
        )
        .unwrap();

        assert_eq!(statuses, [InstallStatus::Updated]);
        assert_eq!(
            sandbox.json()["hooks"]["PreToolUse"],
            serde_json::json!([
                {"matcher": "Bash", "hooks": [{"type": "command", "command": "lets hook classify"}]}
            ])
        );
    }

    #[test]
    fn a_foreign_command_that_mentions_ours_is_left_and_ours_is_added_beside_it() {
        let foreign = "echo lets hook classify disabled";
        let sandbox = Sandbox::new(Some(&format!(
            r#"{{"hooks": {{"PreToolUse": [{{"matcher": "Bash", "hooks": [{{"type": "command", "command": "{foreign}"}}]}}]}}}}"#
        )));

        let statuses = merge_hook_entries(
            &sandbox.path(),
            &[bash_classify("Bash")],
            sandbox.runtime.path(),
        )
        .unwrap();

        assert_eq!(statuses, [InstallStatus::Installed]);
        let commands: Vec<_> = sandbox.json()["hooks"]["PreToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .map(|element| element["hooks"][0]["command"].clone())
            .collect();
        assert_eq!(commands, [foreign, "lets hook classify"]);
    }

    #[test]
    fn an_unparsable_existing_file_is_a_hard_error_and_nothing_is_written() {
        let sandbox = Sandbox::new(Some("{ not json at all }}}"));

        let err = merge_hook_entries(
            &sandbox.path(),
            &[bash_classify("Bash")],
            sandbox.runtime.path(),
        )
        .unwrap_err();

        assert!(matches!(err, Error::Io { .. }));
        assert_eq!(sandbox.text(), "{ not json at all }}}");
    }

    #[test]
    fn a_top_level_value_that_is_not_an_object_is_a_hard_error_and_nothing_is_written() {
        for existing in ["[\"keep\", 1]", "[]", "\"x\"", "42"] {
            let sandbox = Sandbox::new(Some(existing));

            let err = merge_hook_entries(
                &sandbox.path(),
                &[bash_classify("Bash")],
                sandbox.runtime.path(),
            )
            .unwrap_err();

            let Error::Io { source, .. } = &err else {
                panic!("{existing}: expected Io, got {err:?}");
            };
            assert_eq!(source.kind(), std::io::ErrorKind::InvalidData, "{existing}");
            assert_eq!(sandbox.text(), existing);
        }
    }

    #[test]
    fn a_shape_error_on_the_second_entry_leaves_the_first_unwritten() {
        let existing = r#"{"hooks": {"SubagentStart": null}}"#;
        let sandbox = Sandbox::new(Some(existing));
        let subagent = HookEntry {
            event: "SubagentStart",
            matcher: None,
            command: "true",
            is_ours: |command| command == "true",
        };

        let err = merge_hook_entries(
            &sandbox.path(),
            &[bash_classify("Bash"), subagent],
            sandbox.runtime.path(),
        )
        .unwrap_err();

        assert!(matches!(err, Error::Io { .. }));
        assert_eq!(sandbox.text(), existing);
    }

    #[test]
    fn the_lock_lands_in_the_sandboxed_runtime_dir() {
        let sandbox = Sandbox::new(Some("{}"));

        merge_hook_entries(
            &sandbox.path(),
            &[bash_classify("Bash")],
            sandbox.runtime.path(),
        )
        .unwrap();

        assert!(sandbox.runtime.path().join("lets/locks").is_dir());
    }

    fn inode(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(path).unwrap().ino()
    }

    fn install_then_remove(existing: &str, entries: &[HookEntry]) -> (Sandbox, Vec<bool>) {
        let sandbox = Sandbox::new(Some(existing));
        merge_hook_entries(&sandbox.path(), entries, sandbox.runtime.path()).unwrap();
        assert_ne!(sandbox.text(), existing);
        let removed =
            remove_hook_entries(&sandbox.path(), entries, sandbox.runtime.path()).unwrap();
        (sandbox, removed)
    }

    #[test]
    fn install_then_remove_restores_the_file_byte_for_byte() {
        let shapes = [
            "{}\n",
            "{\n  \"env\": {\"FOO\": \"bar\"}\n}\n",
            "{\n  \"model\": \"opus\",\n  \"env\": {\"FOO\": \"bar\"}\n}",
            "{\n  \"hooks\": {\n    \"PreToolUse\": [\n      {\"matcher\": \"Bash\", \"hooks\": \
             [{\"type\": \"command\", \"command\": \"my-guard\"}]}\n    ]\n  }\n}\n",
            "{\n  \"hooks\": {\n    \"Stop\": [{\"hooks\": [{\"type\": \"command\", \"command\": \
             \"say done\"}]}]\n  }\n}\n",
        ];
        for existing in shapes {
            let (sandbox, removed) =
                install_then_remove(existing, &[bash_classify("Bash"), HookEntry {
                    event: "SubagentStart",
                    matcher: None,
                    command: "lets hook classify",
                    is_ours: is_classify,
                }]);

            assert_eq!(removed, [true, true], "{existing}");
            assert_eq!(sandbox.text(), existing);
        }
    }

    #[test]
    fn a_pre_existing_empty_hooks_object_is_removed_not_restored() {
        let existing = "{\n  \"hooks\": {}\n}\n";

        let (sandbox, removed) = install_then_remove(existing, &[bash_classify("Bash")]);

        assert_eq!(removed, [true]);
        assert_eq!(sandbox.json(), serde_json::json!({}));
    }

    #[test]
    fn a_user_hook_under_our_matcher_survives_and_keeps_its_matcher_object() {
        let existing = "{\"hooks\": {\"PreToolUse\": [{\"matcher\": \"Bash\", \"hooks\": [\
                        {\"type\": \"command\", \"command\": \"lets hook classify\"}, \
                        {\"type\": \"command\", \"command\": \"my-guard\"}]}]}}\n";
        let sandbox = Sandbox::new(Some(existing));

        let removed = remove_hook_entries(
            &sandbox.path(),
            &[bash_classify("Bash")],
            sandbox.runtime.path(),
        )
        .unwrap();

        assert_eq!(removed, [true]);
        assert_eq!(
            sandbox.json(),
            serde_json::json!({"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [
                {"type": "command", "command": "my-guard"}
            ]}]}})
        );
    }

    #[test]
    fn a_matcher_object_holding_a_key_install_never_writes_outlives_its_last_hook() {
        let existing = "{\"hooks\": {\"PreToolUse\": [{\"matcher\": \"Bash\", \"timeout\": 5, \
                        \"hooks\": [{\"type\": \"command\", \"command\": \"lets hook classify\"}]}]}}\n";
        let sandbox = Sandbox::new(Some(existing));

        remove_hook_entries(
            &sandbox.path(),
            &[bash_classify("Bash")],
            sandbox.runtime.path(),
        )
        .unwrap();

        assert_eq!(
            sandbox.json(),
            serde_json::json!({"hooks": {"PreToolUse": [{"matcher": "Bash", "timeout": 5, "hooks": []}]}})
        );
    }

    #[test]
    fn our_command_under_another_matcher_is_not_ours_to_remove() {
        let existing = "{\"hooks\": {\"PreToolUse\": [{\"matcher\": \"Edit\", \"hooks\": [\
                        {\"type\": \"command\", \"command\": \"lets hook classify\"}]}]}}\n";
        let sandbox = Sandbox::new(Some(existing));

        let removed = remove_hook_entries(
            &sandbox.path(),
            &[bash_classify("Bash")],
            sandbox.runtime.path(),
        )
        .unwrap();

        assert_eq!(removed, [false]);
        assert_eq!(sandbox.text(), existing);
    }

    #[test]
    fn a_second_remove_finds_nothing_and_does_not_rewrite_the_file() {
        let existing = "{\n  \"env\": {}\n}\n";
        let (sandbox, _) = install_then_remove(existing, &[bash_classify("Bash")]);
        let before = inode(&sandbox.path());

        let removed = remove_hook_entries(
            &sandbox.path(),
            &[bash_classify("Bash")],
            sandbox.runtime.path(),
        )
        .unwrap();

        assert_eq!(removed, [false]);
        assert_eq!(inode(&sandbox.path()), before);
        assert_eq!(sandbox.text(), existing);
    }

    #[test]
    fn a_one_line_object_comes_back_expanded_but_equal_as_json() {
        let existing = "{\"env\": {\"FOO\": \"bar\"}}\n";
        let (sandbox, removed) = install_then_remove(existing, &[bash_classify("Bash")]);

        assert_eq!(removed, [true]);
        assert_eq!(
            sandbox.json(),
            serde_json::from_str::<serde_json::Value>(existing).unwrap()
        );
    }

    #[test]
    fn an_empty_event_array_that_was_already_there_is_left_as_found() {
        let existing = "{\"hooks\": {\"PreToolUse\": []}}\n";
        let sandbox = Sandbox::new(Some(existing));

        let removed = remove_hook_entries(
            &sandbox.path(),
            &[bash_classify("Bash")],
            sandbox.runtime.path(),
        )
        .unwrap();

        assert_eq!(removed, [false]);
        assert_eq!(sandbox.text(), existing);
    }

    #[test]
    fn a_missing_file_stays_missing_and_takes_no_lock() {
        let sandbox = Sandbox::new(None);

        let removed = remove_hook_entries(
            &sandbox.path(),
            &[bash_classify("Bash")],
            sandbox.runtime.path(),
        )
        .unwrap();

        assert_eq!(removed, [false]);
        assert!(!sandbox.path().exists());
        assert!(!sandbox.runtime.path().join("lets").exists());
    }

    #[test]
    fn a_hooks_value_that_is_not_an_object_is_a_hard_error_and_nothing_is_written() {
        let existing = "{\"hooks\": []}\n";
        let sandbox = Sandbox::new(Some(existing));

        let err = remove_hook_entries(
            &sandbox.path(),
            &[bash_classify("Bash")],
            sandbox.runtime.path(),
        )
        .unwrap_err();

        assert!(matches!(err, Error::Io { .. }));
        assert_eq!(sandbox.text(), existing);
    }
}
