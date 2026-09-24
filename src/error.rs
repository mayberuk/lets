use std::fmt::Write as _;
use std::path::PathBuf;

use crate::output::{Omission, plural_suffix};

// Also caps an edit header's line list, the other list that grows with the match count.
pub const CANDIDATE_CAP: usize = 20;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}", render_not_found(target, what, nearest.as_ref()))]
    NotFound {
        target: String,
        what: String,
        nearest: Option<Candidate>,
    },
    #[error("{}", render_ambiguous(target, candidates))]
    Ambiguous {
        target: String,
        candidates: Vec<Candidate>,
    },
    #[error("invalid pattern: {message}")]
    InvalidPattern { pattern: String, message: String },
    #[error("{}", render_over_cap(*hits, *files, *cap))]
    OverCap {
        hits: usize,
        files: usize,
        cap: usize,
    },
    #[error("{} is unsupported: no grammar for .{ext} · use a :line, :a-b or @'regex' target", path.display())]
    NoGrammar { path: PathBuf, ext: String },
    #[error("{layer} check failed for {}: {detail}", path.display())]
    CheckFailed {
        path: PathBuf,
        layer: CheckLayer,
        detail: String,
    },
    #[error("content is {bytes} bytes, over the {limit}-byte budget")]
    OverBudget { bytes: u64, limit: u64 },
    #[error("{} changed since sha:{expected} (now sha:{actual})", path.display())]
    Changed {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("{} is outside the working tree", path.display())]
    OutsideTree { path: PathBuf },
    #[error("{} is unsupported: {reason}", path.display())]
    Unsupported {
        path: PathBuf,
        reason: UnsupportedReason,
    },
    #[error("batch partially written: {} landed, {} failed: {detail}", written.len(), failed.display())]
    PartialBatch {
        written: Vec<PathBuf>,
        failed: PathBuf,
        detail: String,
    },
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{}", render_refused(path, reason))]
    Refused {
        path: PathBuf,
        reason: RefusedReason,
    },
    #[error("{reason}")]
    InstallRefused { reason: InstallRefusedReason },
    #[error("update failed: {detail}")]
    UpdateFailed { detail: String },
    #[error("lets {current} \u{b7} latest {latest} \u{b7} run `lets update`")]
    UpdateAvailable { current: String, latest: String },
    #[error("{message}")]
    Usage { message: String },
    #[error("{} is locked by another lets process (waited 2 s) \u{b7} retry the call", path.display())]
    Locked { path: PathBuf },
    #[error(
        "{} is read-only \u{b7} run chmod u+w {} first if the edit is intended",
        path.display(),
        path.display()
    )]
    ReadOnly { path: PathBuf },
    #[error("{}", render_expect_refused(target, reason))]
    ExpectRefused {
        target: String,
        reason: ExpectReason,
    },
    #[error("{target}: no such file \u{b7} did you mean {meant}")]
    MistypedTarget { target: String, meant: String },
    #[error("{}", render_no_hits(pattern, grep_style.as_deref()))]
    NoHits {
        pattern: String,
        /// The grep-style reading, searched after the literal pattern found nothing.
        grep_style: Option<String>,
    },
    #[error(
        "{} is empty, so there is nothing to match \u{b7} write it with lets write --force {}",
        path.display(),
        path.display()
    )]
    EmptyFile { path: PathBuf },
    #[error(
        "--old spans lines, and {} mixes CRLF and LF endings \u{b7} match one line at a time, or \
         type \\r\\n where the file has CRLF",
        path.display()
    )]
    MixedEndings { path: PathBuf },
    #[error(
        "{target}: the end of this symbol is a guess (plaintext heuristic) \u{b7} use \
         --insert-before, or anchor on its last line: {command}"
    )]
    GuessedSpan { target: String, command: String },
    /// Two or more, in argument order: `Error::all` keeps a lone error as itself.
    #[error("{}", render_several(errors))]
    Several { errors: Vec<Error> },
}

impl Error {
    pub fn slug(&self) -> &'static str {
        match self {
            Error::Ambiguous { .. } => "ambiguous",
            Error::InvalidPattern { .. } => "invalid_pattern",
            Error::OverCap { .. } => "over_cap",
            Error::NoGrammar { .. } => "no_grammar",
            Error::CheckFailed { .. } => "check_failed",
            Error::OverBudget { .. } => "over_budget",
            Error::Changed { .. } => "changed",
            Error::OutsideTree { .. } => "outside_tree",
            Error::Unsupported { .. } => "unsupported_file",
            Error::PartialBatch { .. } => "partial_batch",
            Error::Io { source, .. } => {
                if source.kind() == std::io::ErrorKind::NotFound {
                    "not_found"
                } else {
                    "io_error"
                }
            },
            Error::Refused { reason, .. } => reason.slug(),
            Error::InstallRefused { reason } => reason.slug(),
            Error::UpdateFailed { .. } => "update_failed",
            Error::UpdateAvailable { .. } => "update_available",
            Error::Usage { .. } => "usage",
            Error::Locked { .. } => "locked",
            Error::ReadOnly { .. } => "read_only",
            Error::ExpectRefused { .. } => "expect_refused",
            Error::NotFound { .. } | Error::MistypedTarget { .. } | Error::NoHits { .. } => {
                "not_found"
            },
            Error::EmptyFile { .. } => "empty_file",
            Error::MixedEndings { .. } => "mixed_endings",
            Error::GuessedSpan { .. } => "guessed_span",
            // The first failure sets the slug so the slug and the exit code name the same one.
            Error::Several { errors } => errors.first().map_or("not_found", Error::slug),
        }
    }

    pub fn all(mut errors: Vec<Error>) -> Option<Error> {
        match errors.len() {
            0 | 1 => errors.pop(),
            _ => Some(Error::Several { errors }),
        }
    }
}

fn render_several(errors: &[Error]) -> String {
    let mut out = String::new();
    for (index, error) in errors.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let _ = write!(out, "{error}");
    }
    out
}

fn render_ambiguous(target: &str, candidates: &[Candidate]) -> String {
    let mut out = format!("{target} is ambiguous ({} candidates)", candidates.len());
    for candidate in candidates.iter().take(CANDIDATE_CAP) {
        out.push_str("\n  ");
        out.push_str(&candidate.to_string());
    }
    if candidates.len() > CANDIDATE_CAP {
        let _ = write!(out, "\n  (+{} more)", candidates.len() - CANDIDATE_CAP);
    }
    out
}

// Reuses `Omission::HitCap`, so the error and the footer sentence cannot drift apart.
fn render_over_cap(hits: usize, files: usize, cap: usize) -> String {
    format!(
        "{hits} hit{} in {files} file{} \u{b7} {}",
        plural_suffix(hits),
        plural_suffix(files),
        Omission::HitCap { hits, cap }
    )
}

fn render_not_found(target: &str, what: &str, nearest: Option<&Candidate>) -> String {
    let mut out = format!("{what} not found in {target}");
    if let Some(candidate) = nearest {
        let _ = write!(
            out,
            "\n  nearest: line {}\t{}",
            candidate.line, candidate.text
        );
    }
    out
}

fn render_no_hits(pattern: &str, grep_style: Option<&str>) -> String {
    match grep_style {
        None => format!("no hits for \u{ab}{pattern}\u{bb}"),
        Some(reading) => format!(
            "no hits for \u{ab}{pattern}\u{bb}, nor for its grep-style reading \u{ab}{reading}\u{bb}"
        ),
    }
}

fn render_refused(path: &std::path::Path, reason: &RefusedReason) -> String {
    match reason {
        RefusedReason::Exists { lines, sha } => format!(
            "{} exists ({lines} lines, sha:{sha}) \u{b7} pass --force to overwrite",
            path.display()
        ),
        RefusedReason::EmptyInput => {
            format!("{}: refuses empty stdin without --empty", path.display())
        },
    }
}

#[derive(Debug)]
pub enum ExpectReason {
    Range { first: usize, last: usize },
    Mismatch { line: usize, actual: String },
}

fn render_expect_refused(target: &str, reason: &ExpectReason) -> String {
    match reason {
        ExpectReason::Range { first, last } => format!(
            "{target}: --expect checks line {first} only, and the range runs to line {last} \u{b7} \
             pass --expect-all (the whole range on stdin) or --if sha:<12 hex>"
        ),
        ExpectReason::Mismatch { line, actual } => format!(
            "{target}: line {line} does not match --expect \u{b7} it reads: {actual} \u{b7} \
             re-read it, or confirm with --expect-all or --if sha:<12 hex>"
        ),
    }
}

#[derive(Debug)]
pub struct Candidate {
    pub path: PathBuf,
    pub line: usize,
    pub text: String,
}

// A tab: `cat -n`-trained models read a run of spaces as the line's own indentation.
impl std::fmt::Display for Candidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}\t{}", self.path.display(), self.line, self.text)
    }
}

#[derive(Debug)]
pub enum RefusedReason {
    Exists { lines: usize, sha: String },
    EmptyInput,
}

impl RefusedReason {
    fn slug(&self) -> &'static str {
        match self {
            RefusedReason::Exists { .. } => "exists",
            RefusedReason::EmptyInput => "empty_input",
        }
    }
}

#[derive(Debug)]
pub enum InstallRefusedReason {
    DifferentLetsOnPath { found: PathBuf, current: PathBuf },
    NotOnPath { current: PathBuf },
    NoRepository,
}

impl InstallRefusedReason {
    fn slug(&self) -> &'static str {
        match self {
            InstallRefusedReason::DifferentLetsOnPath { .. } => "path_conflict",
            InstallRefusedReason::NotOnPath { .. } => "not_on_path",
            InstallRefusedReason::NoRepository => "no_repository",
        }
    }
}

impl std::fmt::Display for InstallRefusedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallRefusedReason::DifferentLetsOnPath { found, current } => write!(
                f,
                "a different `lets` comes first on PATH at {} \u{2014} this one is {} \u{b7} \
                 remove the other, or put this one's directory ahead of it on PATH",
                found.display(),
                current.display()
            ),
            InstallRefusedReason::NotOnPath { current } => write!(
                f,
                "this `lets` ({}) is not on PATH, and the hooks run a bare `lets` \u{b7} put its \
                 directory on PATH, or install from the `lets` that is",
                current.display()
            ),
            InstallRefusedReason::NoRepository => f.write_str(
                "this build has no configured release repository \u{b7} set `repository` in \
                 Cargo.toml and rebuild before running `lets update`",
            ),
        }
    }
}

#[derive(Debug)]
pub enum CheckLayer {
    Structured,
    Structure,
    Command,
}

impl std::fmt::Display for CheckLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CheckLayer::Structured => "structured",
            CheckLayer::Structure => "structure",
            CheckLayer::Command => "command",
        })
    }
}

#[derive(Debug)]
pub enum UnsupportedReason {
    Binary,
    Hardlink,
    NonUtf8Region,
    TooLarge {
        bytes: u64,
        limit: u64,
    },
    LockUnavailable {
        detail: String,
    },
    NotStructured,
    /// Changing the key would touch bytes outside its entry, or YAML cannot classify the node.
    NotInPlace {
        key: String,
        why: String,
    },
    Directory,
}

impl std::fmt::Display for UnsupportedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnsupportedReason::Binary => f.write_str("binary file"),
            UnsupportedReason::Hardlink => f.write_str("hardlinked file"),
            UnsupportedReason::NonUtf8Region => f.write_str("non-UTF-8 region"),
            UnsupportedReason::TooLarge { bytes, limit } => {
                write!(f, "{bytes} bytes, over the {limit}-byte limit")
            },
            UnsupportedReason::LockUnavailable { detail } => {
                write!(f, "lock unavailable: {detail}")
            },
            UnsupportedReason::NotStructured => {
                f.write_str("not JSON, YAML, TOML or Markdown with frontmatter")
            },
            UnsupportedReason::NotInPlace { key, why } => {
                write!(f, "{key} cannot be changed in place ({why})")
            },
            UnsupportedReason::Directory => {
                f.write_str("a directory \u{b7} search it with lets find <pattern> <dir>")
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io_error(kind: std::io::ErrorKind) -> Error {
        Error::Io {
            path: PathBuf::from("f"),
            source: std::io::Error::new(kind, "boom"),
        }
    }

    #[test]
    fn every_exit_condition_has_its_own_slug() {
        assert_eq!(
            Error::NotFound {
                target: "f".into(),
                what: "--old".into(),
                nearest: None,
            }
            .slug(),
            "not_found"
        );
        assert_eq!(
            Error::Ambiguous {
                target: "f#sym".into(),
                candidates: vec![],
            }
            .slug(),
            "ambiguous"
        );
        assert_eq!(
            Error::CheckFailed {
                path: PathBuf::from("f"),
                layer: CheckLayer::Structure,
                detail: "unbalanced brace".into(),
            }
            .slug(),
            "check_failed"
        );
        assert_eq!(
            Error::OverBudget {
                bytes: 100,
                limit: 64
            }
            .slug(),
            "over_budget"
        );
        assert_eq!(
            Error::Changed {
                path: PathBuf::from("f"),
                expected: "aaaaaaaaaaaa".into(),
                actual: "bbbbbbbbbbbb".into(),
            }
            .slug(),
            "changed"
        );
        assert_eq!(
            Error::OutsideTree {
                path: PathBuf::from("/etc/f")
            }
            .slug(),
            "outside_tree"
        );
        assert_eq!(
            Error::Unsupported {
                path: PathBuf::from("f"),
                reason: UnsupportedReason::Binary,
            }
            .slug(),
            "unsupported_file"
        );
        assert_eq!(
            Error::PartialBatch {
                written: vec![],
                failed: PathBuf::from("f"),
                detail: "rename failed".into(),
            }
            .slug(),
            "partial_batch"
        );
        assert_eq!(
            Error::InvalidPattern {
                pattern: "(unclosed".into(),
                message: "unclosed group".into(),
            }
            .slug(),
            "invalid_pattern"
        );
        assert_eq!(
            Error::OverCap {
                hits: 312,
                files: 47,
                cap: 50,
            }
            .slug(),
            "over_cap"
        );
        assert_eq!(
            Error::NoGrammar {
                path: PathBuf::from("app.vue"),
                ext: "vue".into(),
            }
            .slug(),
            "no_grammar"
        );
    }

    #[test]
    fn every_new_exit_condition_has_its_own_slug() {
        assert_eq!(
            Error::Usage {
                message: "line 3: unknown key \"olf\"".into(),
            }
            .slug(),
            "usage"
        );
        assert_eq!(
            Error::Locked {
                path: PathBuf::from("f"),
            }
            .slug(),
            "locked"
        );
        assert_eq!(
            Error::ReadOnly {
                path: PathBuf::from("f"),
            }
            .slug(),
            "read_only"
        );
        assert_eq!(
            Error::ExpectRefused {
                target: "a.ts:3-5".into(),
                reason: ExpectReason::Range { first: 3, last: 5 },
            }
            .slug(),
            "expect_refused"
        );
        assert_eq!(
            Error::MistypedTarget {
                target: "f:40,60".into(),
                meant: "f:40-60".into(),
            }
            .slug(),
            "not_found"
        );
        assert_eq!(
            Error::NoHits {
                pattern: "onBack".into(),
                grep_style: None,
            }
            .slug(),
            "not_found"
        );
        assert_eq!(
            Error::EmptyFile {
                path: PathBuf::from("f"),
            }
            .slug(),
            "empty_file"
        );
        assert_eq!(
            Error::MixedEndings {
                path: PathBuf::from("f"),
            }
            .slug(),
            "mixed_endings"
        );
        assert_eq!(
            Error::GuessedSpan {
                target: "f#Outer.inner".into(),
                command: "lets edit f --insert-after @x --new y".into(),
            }
            .slug(),
            "guessed_span"
        );
        assert_eq!(
            Error::Unsupported {
                path: PathBuf::from("dir"),
                reason: UnsupportedReason::Directory,
            }
            .slug(),
            "unsupported_file"
        );
    }

    #[test]
    fn new_variants_display_the_contract_text_verbatim() {
        assert_eq!(
            Error::Usage {
                message: "line 3: unknown key \"olf\"".into(),
            }
            .to_string(),
            "line 3: unknown key \"olf\""
        );
        assert_eq!(
            Error::Locked {
                path: PathBuf::from("a.ts"),
            }
            .to_string(),
            "a.ts is locked by another lets process (waited 2 s) \u{b7} retry the call"
        );
        assert_eq!(
            Error::ReadOnly {
                path: PathBuf::from("a.ts"),
            }
            .to_string(),
            "a.ts is read-only \u{b7} run chmod u+w a.ts first if the edit is intended"
        );
        assert_eq!(
            Error::ExpectRefused {
                target: "a.ts:3-5".into(),
                reason: ExpectReason::Range { first: 3, last: 5 },
            }
            .to_string(),
            "a.ts:3-5: --expect checks line 3 only, and the range runs to line 5 \u{b7} pass \
             --expect-all (the whole range on stdin) or --if sha:<12 hex>"
        );
        assert_eq!(
            Error::ExpectRefused {
                target: "a.ts:40".into(),
                reason: ExpectReason::Mismatch {
                    line: 40,
                    actual: "if (x) {".into(),
                },
            }
            .to_string(),
            "a.ts:40: line 40 does not match --expect \u{b7} it reads: if (x) { \u{b7} re-read it, \
             or confirm with --expect-all or --if sha:<12 hex>"
        );
        assert_eq!(
            Error::MistypedTarget {
                target: "f:40,60".into(),
                meant: "f:40-60".into(),
            }
            .to_string(),
            "f:40,60: no such file \u{b7} did you mean f:40-60"
        );
        assert_eq!(
            Error::NoHits {
                pattern: "onBack".into(),
                grep_style: None,
            }
            .to_string(),
            "no hits for \u{ab}onBack\u{bb}"
        );
        assert_eq!(
            Error::NoHits {
                pattern: r"a\|b".into(),
                grep_style: Some("a|b".into()),
            }
            .to_string(),
            "no hits for \u{ab}a\\|b\u{bb}, nor for its grep-style reading \u{ab}a|b\u{bb}"
        );
        assert_eq!(
            Error::EmptyFile {
                path: PathBuf::from("a.ts"),
            }
            .to_string(),
            "a.ts is empty, so there is nothing to match \u{b7} write it with lets write --force \
             a.ts"
        );
        assert_eq!(
            Error::MixedEndings {
                path: PathBuf::from("a.ts"),
            }
            .to_string(),
            "--old spans lines, and a.ts mixes CRLF and LF endings \u{b7} match one line at a \
             time, or type \\r\\n where the file has CRLF"
        );
        assert_eq!(
            Error::GuessedSpan {
                target: "f#Outer.inner".into(),
                command: "lets edit f --insert-after @x --new y".into(),
            }
            .to_string(),
            "f#Outer.inner: the end of this symbol is a guess (plaintext heuristic) \u{b7} use \
             --insert-before, or anchor on its last line: lets edit f --insert-after @x --new y"
        );
        assert_eq!(
            Error::Unsupported {
                path: PathBuf::from("src"),
                reason: UnsupportedReason::Directory,
            }
            .to_string(),
            "src is unsupported: a directory \u{b7} search it with lets find <pattern> <dir>"
        );
    }

    #[test]
    fn an_invalid_pattern_says_what_is_wrong_with_it_on_one_line() {
        // A regex error's Display is a four-line diagram; stderr has one line before `ERROR_CODE=`.
        let err = Error::InvalidPattern {
            pattern: "(unclosed".into(),
            message: "unclosed group".into(),
        };

        assert_eq!(err.to_string(), "invalid pattern: unclosed group");
        assert_eq!(err.to_string().lines().count(), 1);
    }

    #[test]
    fn over_cap_carries_the_footer_sentence_word_for_word() {
        let err = Error::OverCap {
            hits: 312,
            files: 47,
            cap: 50,
        };

        assert_eq!(
            err.to_string(),
            "312 hits in 47 files \u{b7} over the 50-hit cap \u{b7} narrow the pattern or the \
             paths, or --files"
        );
        assert!(
            err.to_string()
                .ends_with(&Omission::HitCap { hits: 312, cap: 50 }.to_string())
        );
    }

    #[test]
    fn over_cap_with_one_hit_in_one_file_says_hit_and_file() {
        // `--cap 0` is the only way one hit is over the cap.
        let err = Error::OverCap {
            hits: 1,
            files: 1,
            cap: 0,
        };

        assert!(
            err.to_string().starts_with("1 hit in 1 file \u{b7} "),
            "{err}"
        );
    }

    #[test]
    fn no_grammar_names_the_extension_and_a_target_form_that_works() {
        let err = Error::NoGrammar {
            path: PathBuf::from("unsupported.vue"),
            ext: "vue".into(),
        };

        assert_eq!(
            err.to_string(),
            "unsupported.vue is unsupported: no grammar for .vue \u{b7} use a :line, :a-b or \
             @'regex' target"
        );
        assert!(!err.to_string().contains("not found in"));
    }

    #[test]
    fn io_not_found_slugs_not_found() {
        assert_eq!(io_error(std::io::ErrorKind::NotFound).slug(), "not_found");
    }

    #[test]
    fn io_other_kinds_slug_io_error() {
        assert_eq!(
            io_error(std::io::ErrorKind::PermissionDenied).slug(),
            "io_error"
        );
    }

    #[test]
    fn same_condition_from_different_verbs_shares_a_slug() {
        let show_not_found = Error::NotFound {
            target: "a.md:40".into(),
            what: "line".into(),
            nearest: None,
        };
        let edit_not_found = Error::NotFound {
            target: "a.ts".into(),
            what: "--old".into(),
            nearest: None,
        };
        assert_eq!(show_not_found.slug(), edit_not_found.slug());
    }

    fn candidate(line: usize) -> Candidate {
        Candidate {
            path: PathBuf::from("f"),
            line,
            text: format!("text{line}"),
        }
    }

    #[test]
    fn ambiguous_display_lists_all_candidates_under_the_cap() {
        let err = Error::Ambiguous {
            target: "f#sym".into(),
            candidates: (1..=CANDIDATE_CAP).map(candidate).collect(),
        };
        let rendered = err.to_string();
        for line in 1..=CANDIDATE_CAP {
            assert!(
                rendered.contains(&format!("f:{line}\ttext{line}")),
                "missing candidate {line} in {rendered:?}"
            );
        }
        assert!(!rendered.contains("more)"));
    }

    #[test]
    fn ambiguous_display_caps_at_20_with_a_plus_n_more_tail() {
        let err = Error::Ambiguous {
            target: "f#sym".into(),
            candidates: (1..=CANDIDATE_CAP + 1).map(candidate).collect(),
        };
        let rendered = err.to_string();
        assert!(rendered.contains("(+1 more)"));
        assert!(!rendered.contains(&format!(
            "f:{}\ttext{}",
            CANDIDATE_CAP + 1,
            CANDIDATE_CAP + 1
        )));
    }

    #[test]
    fn not_found_display_shows_nearest_when_present() {
        let err = Error::NotFound {
            target: "src/store/usage.ts".into(),
            what: "--old".into(),
            nearest: Some(Candidate {
                path: PathBuf::from("src/store/usage.ts"),
                line: 42,
                text: "const cap = 10".into(),
            }),
        };
        assert!(err.to_string().contains("nearest: line 42\tconst cap = 10"));
    }

    #[test]
    fn not_found_display_omits_nearest_when_absent() {
        let err = Error::NotFound {
            target: "f".into(),
            what: "--old".into(),
            nearest: None,
        };
        assert!(!err.to_string().contains("nearest"));
    }

    #[test]
    fn refused_exists_slug_and_display() {
        let err = Error::Refused {
            path: PathBuf::from("scripts/new-check.sh"),
            reason: RefusedReason::Exists {
                lines: 2,
                sha: "0e1f0e1f0e1f".into(),
            },
        };
        assert_eq!(err.slug(), "exists");
        assert_eq!(
            err.to_string(),
            "scripts/new-check.sh exists (2 lines, sha:0e1f0e1f0e1f) \u{b7} pass --force to overwrite"
        );
    }

    #[test]
    fn refused_empty_input_slug_and_display() {
        let err = Error::Refused {
            path: PathBuf::from("f"),
            reason: RefusedReason::EmptyInput,
        };
        assert_eq!(err.slug(), "empty_input");
        assert_eq!(err.to_string(), "f: refuses empty stdin without --empty");
    }

    #[test]
    fn install_refused_no_repository_names_cargo_toml_field() {
        let err = Error::InstallRefused {
            reason: InstallRefusedReason::NoRepository,
        };
        assert_eq!(err.slug(), "no_repository");
        assert!(err.to_string().contains("Cargo.toml"));
        assert!(err.to_string().contains("repository"));
    }

    #[test]
    fn install_refused_path_conflict_slug_and_display() {
        let err = Error::InstallRefused {
            reason: InstallRefusedReason::DifferentLetsOnPath {
                found: PathBuf::from("/opt/old/lets"),
                current: PathBuf::from("/usr/local/bin/lets"),
            },
        };
        assert_eq!(err.slug(), "path_conflict");
        assert!(err.to_string().contains("/opt/old/lets"));
        assert!(err.to_string().contains("/usr/local/bin/lets"));
    }

    #[test]
    fn install_refused_not_on_path_names_the_running_binary() {
        let err = Error::InstallRefused {
            reason: InstallRefusedReason::NotOnPath {
                current: PathBuf::from("/tmp/build/lets"),
            },
        };
        assert_eq!(err.slug(), "not_on_path");
        assert!(err.to_string().contains("/tmp/build/lets"));
    }

    #[test]
    fn update_failed_names_the_detail() {
        let err = Error::UpdateFailed {
            detail: "installer exited 1".into(),
        };
        assert_eq!(err.to_string(), "update failed: installer exited 1");
    }

    #[test]
    fn update_available_names_both_versions_and_the_command_that_updates() {
        let err = Error::UpdateAvailable {
            current: "0.0.1".into(),
            latest: "0.2.0".into(),
        };
        assert_eq!(err.slug(), "update_available");
        assert_eq!(
            err.to_string(),
            "lets 0.0.1 \u{b7} latest 0.2.0 \u{b7} run `lets update`"
        );
    }
}
