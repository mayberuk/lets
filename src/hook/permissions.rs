//! Claude Code judges a rewrite, and a suggested `lets` call, by its rules for `lets`, never by the
//! `Read` and `Edit` rules that bind the read it replaced. A path those deny or ask rules cover is
//! left to Claude Code, and so is anything here that cannot be read the way Claude Code reads it.
//! Pattern syntax and settings tiers: code.claude.com/docs/en/permissions, "Read and Edit".

use std::path::{Component, Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Hand-written settings run to a few KiB; parsing a file this size would cost the hook more than
/// its whole `hook classify` gate.
const MAX_SETTINGS_BYTES: u64 = 1 << 20;

#[cfg(target_os = "macos")]
const MANAGED_DIR: &str = "/Library/Application Support/ClaudeCode";
#[cfg(not(target_os = "macos"))]
const MANAGED_DIR: &str = "/etc/claude-code";

pub struct Sources {
    pub home: Option<PathBuf>,
    /// `CLAUDE_CONFIG_DIR`, which stands in for `~/.claude`.
    pub config: Option<PathBuf>,
    /// `CLAUDE_PROJECT_DIR`, where the session started; the event's `cwd` follows `cd` and `/cd`.
    pub project: Option<PathBuf>,
    pub managed: PathBuf,
}

impl Sources {
    pub fn from_env() -> Sources {
        let absolute = |name: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        };
        Sources {
            home: absolute("HOME"),
            config: absolute("CLAUDE_CONFIG_DIR"),
            project: absolute("CLAUDE_PROJECT_DIR"),
            managed: PathBuf::from(MANAGED_DIR),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Edit,
}

/// A `Read` rule also binds edits and writes of its path; an `Edit` rule binds only those.
pub struct Rules {
    whole: Vec<Access>,
    matchers: Vec<(Access, Gitignore)>,
}

impl Rules {
    /// `None` when a settings file or a rule in one cannot be read as Claude Code reads it.
    pub fn load(sources: &Sources, cwd: &Path) -> Option<Rules> {
        let config = match &sources.config {
            Some(config) => config.clone(),
            None => sources.home.as_ref()?.join(".claude"),
        };
        let mut projects = vec![cwd.to_path_buf()];
        if let Some(project) = sources.project.as_ref().filter(|project| *project != cwd) {
            projects.push(project.clone());
        }
        let current = with_real_paths(&projects);
        let mut parsed = Parsed {
            home: sources.home.as_deref(),
            current: &current,
            whole: Vec::new(),
            lines: Vec::new(),
        };

        for file in managed_files(&sources.managed)? {
            parsed.file(&file, None)?;
        }
        parsed.file(
            &config.join("settings.json"),
            Some(std::slice::from_ref(&config)),
        )?;
        for project in &projects {
            let dir = project.join(".claude");
            parsed.file(&dir.join("settings.json"), Some(&projects))?;
            parsed.file(&dir.join("settings.local.json"), Some(&projects))?;
            for root in repository_roots(project)? {
                parsed.file(&root.join(".claude/settings.local.json"), Some(&projects))?;
            }
        }
        parsed.build()
    }

    pub fn is_empty(&self) -> bool {
        self.whole.is_empty() && self.matchers.is_empty()
    }

    /// Checks the path as named and as it resolves: a deny binds a symlink when either matches.
    /// A directory is covered when a rule matches it or a file directly inside it, as
    /// `private/**` matches what a search of `private` reads but not `private` itself.
    pub fn cover(&self, path: &Path, access: Access) -> bool {
        let binds = |rule: Access| rule == Access::Read || rule == access;
        if self.whole.iter().any(|rule| binds(*rule)) {
            return true;
        }
        let real = std::fs::canonicalize(path).ok();
        let is_dir = path.is_dir();
        let mut candidates = vec![(path.to_path_buf(), is_dir)];
        candidates.extend(real.map(|real| (real, is_dir)));
        if is_dir {
            let inside: Vec<_> = candidates
                .iter()
                .map(|(dir, _)| (dir.join("_"), false))
                .collect();
            candidates.extend(inside);
        }
        candidates.iter().any(|(candidate, is_dir)| {
            self.matchers.iter().any(|(rule, matcher)| {
                // The matcher panics on a path outside its root.
                binds(*rule)
                    && candidate.starts_with(matcher.path())
                    && matcher
                        .matched_path_or_any_parents(candidate, *is_dir)
                        .is_ignore()
            })
        })
    }
}

struct Parsed<'a> {
    home: Option<&'a Path>,
    /// Where `path` and `./path` rules resolve: every directory Claude Code may take as current.
    current: &'a [PathBuf],
    whole: Vec<Access>,
    /// Gitignore lines, each matched under its root.
    lines: Vec<(Access, PathBuf, String)>,
}

impl Parsed<'_> {
    /// `slash` is where this file's `/path` rules anchor; `None` where the docs do not say.
    fn file(&mut self, path: &Path, slash: Option<&[PathBuf]>) -> Option<()> {
        let settings = read_settings(path)?;
        let Some(permissions) = settings.as_object()?.get("permissions") else {
            return Some(());
        };
        let permissions = permissions.as_object()?;
        for list in ["deny", "ask"] {
            let Some(rules) = permissions.get(list) else {
                continue;
            };
            for rule in rules.as_array()? {
                self.rule(rule.as_str()?, slash)?;
            }
        }
        Some(())
    }

    fn rule(&mut self, rule: &str, slash: Option<&[PathBuf]>) -> Option<()> {
        let (tool, pattern) = match rule.split_once('(') {
            Some((tool, rest)) => (tool, Some(rest.strip_suffix(')')?)),
            None => (rule, None),
        };
        let access = match tool {
            "Read" => Access::Read,
            "Edit" => Access::Edit,
            _ => return Some(()),
        };
        let Some(pattern) = pattern.filter(|pattern| !pattern.is_empty()) else {
            self.whole.push(access);
            return Some(());
        };
        // A carve-out only narrows a deny, so skipping it leaves more to Claude Code, never less.
        if pattern.starts_with('!') {
            return Some(());
        }
        if Path::new(pattern)
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return None;
        }
        if let Some(rest) = pattern.strip_prefix("//") {
            self.anchored(access, Path::new("/"), rest);
        } else if let Some(rest) = pattern
            .strip_prefix("~/")
            .or((pattern == "~").then_some(""))
        {
            self.anchored(access, self.home?, rest);
        } else if pattern.starts_with('~') {
            return None;
        } else if let Some(rest) = pattern.strip_prefix('/') {
            for root in slash? {
                self.anchored(access, root, rest);
            }
        } else {
            let rest = pattern.strip_prefix("./").unwrap_or(pattern);
            let line = floating(rest);
            for root in self.current {
                self.lines.push((access, root.clone(), line.clone()));
                // A deny or ask rule such as `secrets/**` matches that directory at any depth.
                if rest.contains('/') && !rest.starts_with("**/") {
                    self.lines
                        .push((access, root.clone(), format!("**/{line}")));
                }
            }
        }
        Some(())
    }

    /// A rule written through a symlinked directory also binds that directory's real location.
    fn anchored(&mut self, access: Access, root: &Path, rest: &str) {
        self.lines.push((access, root.to_path_buf(), rooted(rest)));
        let segments: Vec<&str> = rest.split('/').collect();
        let literal = segments
            .iter()
            .position(|segment| segment.contains(['*', '?', '[', '{', '\\']))
            .unwrap_or(segments.len() - 1)
            .min(segments.len() - 1);
        let written = root.join(segments[..literal].join("/"));
        if let Ok(real) = std::fs::canonicalize(&written)
            && real != written
        {
            self.lines
                .push((access, real, rooted(&segments[literal..].join("/"))));
        }
    }

    fn build(self) -> Option<Rules> {
        let mut groups: Vec<(Access, PathBuf, GitignoreBuilder)> = Vec::new();
        for (access, root, line) in self.lines {
            let at = if let Some(at) = groups
                .iter()
                .position(|(rule, at, _)| *rule == access && *at == root)
            {
                at
            } else {
                let builder = GitignoreBuilder::new(&root);
                groups.push((access, root, builder));
                groups.len() - 1
            };
            groups[at].2.add_line(None, &line).ok()?;
        }
        let matchers = groups
            .into_iter()
            .map(|(access, _, builder)| Some((access, builder.build().ok()?)))
            .collect::<Option<Vec<_>>>()?;
        Some(Rules {
            whole: self.whole,
            matchers,
        })
    }
}

fn rooted(rest: &str) -> String {
    if rest.is_empty() {
        "**".to_owned()
    } else {
        format!("/{rest}")
    }
}

/// A leading `!` or `#` would read as a gitignore negation or comment, not as a name.
fn floating(rest: &str) -> String {
    match rest {
        "" | "." => "**".to_owned(),
        _ if rest.starts_with(['!', '#']) => format!("\\{rest}"),
        _ => rest.to_owned(),
    }
}

fn with_real_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut all = paths.to_vec();
    for path in paths {
        if let Ok(real) = std::fs::canonicalize(path)
            && !all.contains(&real)
        {
            all.push(real);
        }
    }
    all
}

fn absent(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// A file that is not there reads as empty settings; `None` for one that cannot be read or parsed.
fn read_settings(path: &Path) -> Option<serde_json::Value> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if absent(&error) => {
            return Some(serde_json::Value::Object(serde_json::Map::default()));
        },
        Err(_) => return None,
    };
    if !metadata.is_file() || metadata.len() > MAX_SETTINGS_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// `managed-settings.d/*.json` merges with `managed-settings.json` in the same directory.
fn managed_files(dir: &Path) -> Option<Vec<PathBuf>> {
    let mut files = vec![dir.join("managed-settings.json")];
    match std::fs::read_dir(dir.join("managed-settings.d")) {
        Ok(entries) => {
            for entry in entries {
                let path = entry.ok()?.path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == "json")
                {
                    files.push(path);
                }
            }
        },
        Err(error) if absent(&error) => {},
        Err(_) => return None,
    }
    Some(files)
}

/// Claude Code reads `settings.local.json` at the repository root too, and in a worktree at the
/// main checkout's root.
fn repository_roots(dir: &Path) -> Option<Vec<PathBuf>> {
    for ancestor in dir.ancestors() {
        let dot_git = ancestor.join(".git");
        let metadata = match std::fs::metadata(&dot_git) {
            Ok(metadata) => metadata,
            Err(error) if absent(&error) => continue,
            Err(_) => return None,
        };
        let mut roots = vec![ancestor.to_path_buf()];
        if metadata.is_file() {
            let text = std::fs::read_to_string(&dot_git).ok()?;
            let gitdir = ancestor.join(text.strip_prefix("gitdir: ")?.trim_end());
            roots.extend(main_checkout(&gitdir));
        }
        return Some(roots);
    }
    Some(Vec::new())
}

/// `<main>/.git/worktrees/<name>` names the main checkout.
fn main_checkout(gitdir: &Path) -> Option<PathBuf> {
    let worktrees = gitdir.parent()?;
    let dot_git = worktrees.parent()?;
    if worktrees.file_name()? != "worktrees" || dot_git.file_name()? != ".git" {
        return None;
    }
    dot_git.parent().map(Path::to_path_buf)
}
