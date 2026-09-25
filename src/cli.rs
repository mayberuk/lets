use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use clap::error::ErrorKind;
use clap::{
    ArgAction, ArgMatches, Args, Command, CommandFactory as _, FromArgMatches as _, Parser,
    Subcommand, ValueEnum,
};

use crate::output::Sha12;

#[derive(Debug, Parser)]
#[command(name = "lets", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub verb: Verb,
    #[command(flatten)]
    pub global: Global,
}

impl Cli {
    /// Returns the parse error instead of exiting, so `main.rs` owns every exit code.
    pub fn parse_args() -> Result<Cli, clap::Error> {
        Cli::parse_argv(std::env::args_os())
    }

    /// Also recovers what the derive drops: the order `transform`'s op flags were typed in.
    fn parse_argv(argv: impl IntoIterator<Item = OsString>) -> Result<Cli, clap::Error> {
        let matches = command().try_get_matches_from(argv)?;
        let mut cli = Cli::from_arg_matches(&matches).map_err(|err| err.format(&mut command()))?;
        if let (Verb::Transform(args), Some(sub)) =
            (&mut cli.verb, matches.subcommand_matches("transform"))
        {
            args.order = op_order(sub);
        }
        if let Verb::Find(args) = &cli.verb
            && args.grep.invert_match
        {
            // Unformatted, so the replacement command is the last thing before `ERROR_CODE=`.
            return Err(clap::Error::raw(
                ErrorKind::ArgumentConflict,
                INVERT_MATCH_REFUSED,
            ));
        }
        Ok(cli)
    }
}

/// Every switch accepts a repeat, as grep's do. A repeated value option is still refused:
/// last-wins would silently drop one of two `--old`s.
fn command() -> Command {
    repeatable_switches(Cli::command())
}

fn repeatable_switches(command: Command) -> Command {
    command
        .mut_args(|arg| {
            if matches!(arg.get_action(), ArgAction::SetTrue) {
                let id = arg.get_id().clone();
                arg.overrides_with(id)
            } else {
                arg
            }
        })
        .mut_subcommands(repeatable_switches)
}

/// Unlike the other grep flags, `-v` as a no-op would print exactly the lines to exclude.
const INVERT_MATCH_REFUSED: &str = "lets find has no -v: it prints matching lines only \u{b7} use \
                                    grep -v to print the lines that do not match\n";

const IF_SHA_USAGE: &str =
    "--if takes sha: and 12 or more lowercase hex digits, as a show header prints it";

fn if_sha(value: &str) -> Result<String, String> {
    let hex = value.strip_prefix("sha:").unwrap_or(value);
    match Sha12::parse(hex) {
        Some(_) => Ok(value.to_owned()),
        None => Err(IF_SHA_USAGE.to_owned()),
    }
}

const CHECK_PRESETS: [&str; 5] = ["auto", "cargo", "go", "tsc", "py"];

fn check(value: &str) -> Result<String, String> {
    match value.strip_prefix('@') {
        Some(name) if !CHECK_PRESETS.contains(&name) => Err(format!(
            "unknown preset {value} \u{b7} the presets are @auto, @cargo, @go, @tsc, @py"
        )),
        _ => Ok(value.to_owned()),
    }
}

/// Days and hours only: `m` would read as minutes to one caller and months to another.
fn since(value: &str) -> Result<Duration, String> {
    let usage = || "--since takes a count and a unit, d or h, e.g. 7d".to_owned();
    let (count, seconds_per_unit) = if let Some(count) = value.strip_suffix('d') {
        (count, 86_400)
    } else if let Some(count) = value.strip_suffix('h') {
        (count, 3_600)
    } else {
        return Err(usage());
    };
    if count.is_empty() || !count.bytes().all(|b| b.is_ascii_digit()) {
        return Err(usage());
    }
    let count: u64 = count.parse().map_err(|_| usage())?;
    if count == 0 {
        return Err(usage());
    }
    count
        .checked_mul(seconds_per_unit)
        .map(Duration::from_secs)
        .ok_or_else(usage)
}

fn op_order(matches: &ArgMatches) -> Vec<OpKind> {
    let mut indexed: Vec<(usize, OpKind)> = Vec::new();
    for (id, kind) in [
        ("set", OpKind::Set),
        ("delete", OpKind::Delete),
        ("append", OpKind::Append),
    ] {
        indexed.extend(
            matches
                .indices_of(id)
                .into_iter()
                .flatten()
                .map(|at| (at, kind)),
        );
    }
    indexed.sort_unstable_by_key(|(at, _)| *at);
    indexed.into_iter().map(|(_, kind)| kind).collect()
}

#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct Global {
    #[arg(long, global = true)]
    pub json: bool,
    #[arg(long, global = true, conflicts_with = "json")]
    pub jsonl: bool,
    #[arg(long, global = true)]
    pub budget: Option<usize>,
    #[arg(long, global = true, default_value_t = 65536)]
    pub max_bytes: usize,
    #[arg(long, global = true, default_value_t = 8_388_608)]
    pub max_file_bytes: u64,
    #[arg(long, global = true)]
    pub no_ignore: bool,
    #[arg(long, global = true)]
    pub allow_outside: bool,
    #[arg(long, global = true)]
    pub no_check: bool,
    #[arg(short, long, global = true)]
    pub quiet: bool,
}

#[derive(Debug, Subcommand)]
pub enum Verb {
    Show(ShowArgs),
    #[command(visible_alias = "locate")]
    Find(FindArgs),
    Edit(EditArgs),
    Transform(TransformArgs),
    Write(WriteArgs),
    Stats(StatsArgs),
    Guide,
    Version,
    Update {
        /// Report whether a newer release exists, without installing it
        #[arg(long)]
        check: bool,
        /// Reinstall the latest release even when this one is already it
        #[arg(long, conflicts_with = "check")]
        force: bool,
    },
    Hooks {
        #[command(subcommand)]
        cmd: HooksCmd,
    },
    Hook {
        #[command(subcommand)]
        cmd: HookCmd,
    },
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    // Unlike edit/transform, `show` has no `--from -` batch form to stand in for targets.
    #[arg(required = true)]
    pub targets: Vec<String>,
    // 200: the corpus's measured p75 full-read is 211 lines.
    #[arg(long, default_value_t = 200)]
    pub window: usize,
    #[arg(long)]
    pub all: bool,
    #[arg(short = 'A')]
    pub after: Option<usize>,
    #[arg(short = 'B')]
    pub before: Option<usize>,
    #[arg(short = 'C')]
    pub context: Option<usize>,
    #[arg(long)]
    pub no_numbers: bool,
}

#[derive(Debug, Args, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct FindArgs {
    pub pattern: String,
    pub paths: Vec<String>,
    #[arg(short = 'F', long)]
    pub fixed_string: bool,
    #[arg(short = 'i', long)]
    pub ignore_case: bool,
    #[arg(short = 'w', long)]
    pub word: bool,
    // 50: the SWE-agent-tuned cap on over-cap search suppression.
    #[arg(long, default_value_t = 50)]
    pub cap: usize,
    #[arg(short = 'l', long)]
    pub files: bool,
    #[arg(short = 'c', long)]
    pub count: bool,
    #[arg(short = 'g', long = "glob", visible_alias = "include")]
    pub globs: Vec<String>,
    #[arg(long)]
    pub hidden: bool,
    #[arg(short = 'A')]
    pub after: Option<usize>,
    #[arg(short = 'B')]
    pub before: Option<usize>,
    #[arg(short = 'C')]
    pub context: Option<usize>,
    /// Print hit lines only, never the enclosing symbol or the lines around a hit
    #[arg(long)]
    pub no_expand: bool,
    #[command(flatten)]
    pub grep: GrepCompat,
}

#[derive(Debug, Args, Default, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct GrepCompat {
    /// Accepted for grep compatibility; find always numbers its hits
    #[arg(short = 'n', long)]
    pub line_number: bool,
    /// Accepted for grep compatibility; find always searches directories recursively
    #[arg(short = 'r')]
    pub recursive: bool,
    /// Same as -r, accepted for grep compatibility; find already searches recursively
    #[arg(short = 'R')]
    pub dereference_recursive: bool,
    /// Accepted for grep compatibility; find's patterns are already extended regex
    #[arg(short = 'E')]
    pub extended_regexp: bool,
    /// Accepted for grep compatibility; find always names the file
    #[arg(short = 'H')]
    pub with_filename: bool,
    /// Refused: find prints matching lines only; use grep -v for the complement
    #[arg(short = 'v', long)]
    pub invert_match: bool,
}

#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct EditArgs {
    pub target: Option<String>,
    pub more_targets: Vec<String>,
    #[arg(long)]
    pub old: Option<String>,
    #[arg(long)]
    pub new: Option<String>,
    #[arg(long)]
    pub all: bool,
    #[arg(long)]
    pub expect: Option<String>,
    #[arg(long)]
    pub expect_all: bool,
    #[arg(long)]
    pub insert_after: Option<String>,
    #[arg(long)]
    pub insert_before: Option<String>,
    #[arg(long)]
    pub from: Option<String>,
    #[arg(long = "if", value_parser = if_sha)]
    pub if_sha: Option<String>,
    #[arg(long)]
    pub normalize: bool,
    #[arg(long)]
    pub literal_newlines: bool,
    #[arg(long, value_parser = check)]
    pub check: Option<String>,
    #[arg(long, default_value_t = 60)]
    pub check_timeout: u64,
}

#[derive(Debug, Args)]
pub struct TransformArgs {
    pub file: Option<PathBuf>,
    #[arg(long)]
    pub set: Vec<String>,
    #[arg(long)]
    pub delete: Vec<String>,
    #[arg(long)]
    pub append: Vec<String>,
    #[arg(long)]
    pub from: Option<String>,
    #[arg(long = "if", value_parser = if_sha)]
    pub if_sha: Option<String>,
    #[arg(long, value_parser = check)]
    pub check: Option<String>,
    #[arg(long, default_value_t = 60)]
    pub check_timeout: u64,
    /// Each `Vec` above keeps its own flag's order, not how the three interleave.
    #[arg(skip)]
    pub order: Vec<OpKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Set,
    Delete,
    Append,
}

#[derive(Debug, Args)]
pub struct StatsArgs {
    #[arg(long)]
    pub dir: Option<PathBuf>,
    #[arg(long, value_parser = since)]
    pub since: Option<Duration>,
}

#[derive(Debug, Args)]
pub struct WriteArgs {
    pub path: PathBuf,
    #[arg(long)]
    pub force: bool,
    #[arg(long)]
    pub empty: bool,
}

#[derive(Debug, Subcommand)]
pub enum HooksCmd {
    Install { target: HookTarget },
    Uninstall { target: HookTarget },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HookTarget {
    ClaudeCode,
    Codex,
}

#[derive(Debug, Subcommand)]
pub enum HookCmd {
    Classify,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_parses_two_targets() {
        let cli = Cli::try_parse_from(["lets", "show", "a.ts", "b.ts:40-80"]).unwrap();
        let Verb::Show(args) = cli.verb else {
            panic!("expected Verb::Show");
        };
        assert_eq!(args.targets, vec!["a.ts", "b.ts:40-80"]);
    }

    #[test]
    fn locate_alias_resolves_to_find() {
        let cli = Cli::try_parse_from(["lets", "locate", "foo"]).unwrap();
        let Verb::Find(args) = cli.verb else {
            panic!("expected Verb::Find");
        };
        assert_eq!(args.pattern, "foo");
    }

    #[test]
    fn global_flag_after_subcommand_sets_json() {
        let cli = Cli::try_parse_from(["lets", "find", "x", "--json"]).unwrap();
        assert!(cli.global.json);
    }

    #[test]
    fn window_with_no_value_is_a_parse_error() {
        let result = Cli::try_parse_from(["lets", "show", "a.ts", "--window"]);
        assert!(result.is_err());
    }

    #[test]
    fn edit_if_flag_fills_if_sha() {
        let cli = Cli::try_parse_from([
            "lets",
            "edit",
            "f",
            "--old",
            "a",
            "--new",
            "b",
            "--if",
            "sha:0123456789ab",
        ])
        .unwrap();
        let Verb::Edit(args) = cli.verb else {
            panic!("expected Verb::Edit");
        };
        assert_eq!(args.if_sha.as_deref(), Some("sha:0123456789ab"));
    }

    #[test]
    fn version_flag_returns_display_version_error() {
        let err = Cli::try_parse_from(["lets", "--version"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    }

    #[test]
    fn every_spec_verb_parses_to_its_matching_variant() {
        let cli = Cli::try_parse_from(["lets", "show", "a.ts"]).unwrap();
        assert!(matches!(cli.verb, Verb::Show(_)));
        let cli = Cli::try_parse_from(["lets", "find", "x"]).unwrap();
        assert!(matches!(cli.verb, Verb::Find(_)));
        let cli = Cli::try_parse_from(["lets", "edit", "f", "--old", "a", "--new", "b"]).unwrap();
        assert!(matches!(cli.verb, Verb::Edit(_)));
        let cli = Cli::try_parse_from(["lets", "transform", "f.json", "--set", "a=1"]).unwrap();
        assert!(matches!(cli.verb, Verb::Transform(_)));
        let cli = Cli::try_parse_from(["lets", "write", "f.txt"]).unwrap();
        assert!(matches!(cli.verb, Verb::Write(_)));
        let cli = Cli::try_parse_from(["lets", "guide"]).unwrap();
        assert!(matches!(cli.verb, Verb::Guide));
        let cli = Cli::try_parse_from(["lets", "version"]).unwrap();
        assert!(matches!(cli.verb, Verb::Version));
        let cli = Cli::try_parse_from(["lets", "update"]).unwrap();
        assert!(matches!(cli.verb, Verb::Update {
            check: false,
            force: false
        }));
        let cli = Cli::try_parse_from(["lets", "hooks", "install", "claude-code"]).unwrap();
        assert!(matches!(cli.verb, Verb::Hooks { .. }));
        let cli = Cli::try_parse_from(["lets", "hook", "classify"]).unwrap();
        assert!(matches!(cli.verb, Verb::Hook { .. }));
    }

    #[test]
    fn unknown_verb_is_a_parse_error_not_a_process_exit() {
        assert!(Cli::try_parse_from(["lets", "bogus"]).is_err());
    }

    #[test]
    fn window_defaults_to_200() {
        let cli = Cli::try_parse_from(["lets", "show", "a.ts"]).unwrap();
        let Verb::Show(args) = cli.verb else {
            panic!("expected Verb::Show");
        };
        assert_eq!(args.window, 200);
    }

    #[test]
    fn find_cap_defaults_to_50() {
        let cli = Cli::try_parse_from(["lets", "find", "x"]).unwrap();
        let Verb::Find(args) = cli.verb else {
            panic!("expected Verb::Find");
        };
        assert_eq!(args.cap, 50);
    }

    #[test]
    fn global_byte_budgets_default_from_spec() {
        let cli = Cli::try_parse_from(["lets", "find", "x"]).unwrap();
        assert_eq!(cli.global.max_bytes, 65536);
        assert_eq!(cli.global.max_file_bytes, 8_388_608);
    }

    #[test]
    fn show_with_no_targets_is_a_parse_error() {
        assert!(Cli::try_parse_from(["lets", "show"]).is_err());
    }

    #[test]
    fn edit_and_transform_positionals_stay_optional_for_from_dash() {
        assert!(
            Cli::try_parse_from(["lets", "edit", "--from", "-", "--old", "a", "--new", "b"])
                .is_ok()
        );
        assert!(Cli::try_parse_from(["lets", "transform", "--from", "-", "--set", "a=1"]).is_ok());
    }

    #[test]
    fn json_and_jsonl_conflict_is_a_parse_error() {
        assert!(Cli::try_parse_from(["lets", "find", "x", "--json", "--jsonl"]).is_err());
    }

    #[test]
    fn global_flags_bind_before_the_subcommand() {
        // A field that loses `global = true` fails here yet passes the after-subcommand test.
        let cli = Cli::try_parse_from([
            "lets",
            "--json",
            "--budget",
            "111",
            "--max-bytes",
            "222",
            "--max-file-bytes",
            "333",
            "--no-ignore",
            "--allow-outside",
            "--no-check",
            "--quiet",
            "show",
            "a.ts",
        ])
        .unwrap();
        assert!(cli.global.json);
        assert_eq!(cli.global.budget, Some(111));
        assert_eq!(cli.global.max_bytes, 222);
        assert_eq!(cli.global.max_file_bytes, 333);
        assert!(cli.global.no_ignore);
        assert!(cli.global.allow_outside);
        assert!(cli.global.no_check);
        assert!(cli.global.quiet);
    }

    #[test]
    fn global_flags_bind_after_the_subcommand() {
        let cli = Cli::try_parse_from([
            "lets",
            "show",
            "a.ts",
            "--budget",
            "111",
            "--max-bytes",
            "222",
            "--max-file-bytes",
            "333",
            "--no-ignore",
            "--allow-outside",
            "--no-check",
            "--quiet",
        ])
        .unwrap();
        assert_eq!(cli.global.budget, Some(111));
        assert_eq!(cli.global.max_bytes, 222);
        assert_eq!(cli.global.max_file_bytes, 333);
        assert!(cli.global.no_ignore);
        assert!(cli.global.allow_outside);
        assert!(cli.global.no_check);
        assert!(cli.global.quiet);
    }

    #[test]
    fn jsonl_flag_binds_before_and_after_the_subcommand() {
        let before = Cli::try_parse_from(["lets", "--jsonl", "show", "a.ts"]).unwrap();
        assert!(before.global.jsonl);
        let after = Cli::try_parse_from(["lets", "show", "a.ts", "--jsonl"]).unwrap();
        assert!(after.global.jsonl);
    }

    #[test]
    fn quiet_short_flag_binds_before_and_after_the_subcommand() {
        let before = Cli::try_parse_from(["lets", "-q", "show", "a.ts"]).unwrap();
        assert!(before.global.quiet);
        let after = Cli::try_parse_from(["lets", "show", "a.ts", "-q"]).unwrap();
        assert!(after.global.quiet);
    }

    #[test]
    fn show_short_context_flags_bind_to_distinct_fields() {
        let cli =
            Cli::try_parse_from(["lets", "show", "a.ts", "-A", "3", "-B", "2", "-C", "5"]).unwrap();
        let Verb::Show(args) = cli.verb else {
            panic!("expected Verb::Show");
        };
        assert_eq!(args.after, Some(3));
        assert_eq!(args.before, Some(2));
        assert_eq!(args.context, Some(5));
    }

    #[test]
    fn find_short_context_flags_bind_to_distinct_fields() {
        let cli =
            Cli::try_parse_from(["lets", "find", "x", "-A", "3", "-B", "2", "-C", "5"]).unwrap();
        let Verb::Find(args) = cli.verb else {
            panic!("expected Verb::Find");
        };
        assert_eq!(args.after, Some(3));
        assert_eq!(args.before, Some(2));
        assert_eq!(args.context, Some(5));
    }

    #[test]
    fn find_short_match_mode_flags_bind_to_distinct_fields() {
        let cli = Cli::try_parse_from(["lets", "find", "x", "-F", "-i", "-w"]).unwrap();
        let Verb::Find(args) = cli.verb else {
            panic!("expected Verb::Find");
        };
        assert!(args.fixed_string);
        assert!(args.ignore_case);
        assert!(args.word);
    }

    #[test]
    fn transform_if_flag_fills_if_sha() {
        let cli = Cli::try_parse_from([
            "lets",
            "transform",
            "f.json",
            "--set",
            "a=1",
            "--if",
            "sha:0123456789ab",
        ])
        .unwrap();
        let Verb::Transform(args) = cli.verb else {
            panic!("expected Verb::Transform");
        };
        assert_eq!(args.if_sha.as_deref(), Some("sha:0123456789ab"));
    }

    fn transform_order(argv: &[&str]) -> Vec<OpKind> {
        let cli = Cli::parse_argv(argv.iter().map(OsString::from)).unwrap();
        let Verb::Transform(transform) = cli.verb else {
            panic!("expected Verb::Transform");
        };
        transform.order
    }

    #[test]
    fn transform_order_keeps_a_repeated_kind_split_by_another() {
        let order = transform_order(&[
            "lets",
            "transform",
            "f.yaml",
            "--set",
            "a=1",
            "--delete",
            "b",
            "--set",
            "c=2",
        ]);
        assert_eq!(order, [OpKind::Set, OpKind::Delete, OpKind::Set]);
    }

    #[test]
    fn transform_order_counts_three_of_one_kind() {
        let order = transform_order(&[
            "lets",
            "transform",
            "f.yaml",
            "--set",
            "a=1",
            "--set",
            "b=2",
            "--set",
            "c=3",
        ]);
        assert_eq!(order, [OpKind::Set; 3]);
    }

    #[test]
    fn transform_order_reads_the_equals_spelling_and_a_global_flag_between() {
        let order = transform_order(&[
            "lets",
            "transform",
            "f.yaml",
            "--delete=legacy.token",
            "--json",
            "--append",
            "allow[]=gh",
        ]);
        assert_eq!(order, [OpKind::Delete, OpKind::Append]);
    }

    fn find(words: &[&str]) -> FindArgs {
        let cli = Cli::parse_argv(words.iter().map(OsString::from)).unwrap();
        let Verb::Find(args) = cli.verb else {
            panic!("expected Verb::Find");
        };
        args
    }

    #[test]
    fn each_grep_compat_flag_binds_to_its_own_field() {
        let args = find(&["lets", "find", "x", "-n", "-r", "-R", "-E", "-H"]);
        assert!(args.grep.line_number);
        assert!(args.grep.recursive);
        assert!(args.grep.dereference_recursive);
        assert!(args.grep.extended_regexp);
        assert!(args.grep.with_filename);
        assert!(!args.grep.invert_match);
        assert!(
            find(&["lets", "find", "x", "--line-number"])
                .grep
                .line_number
        );
    }

    #[test]
    fn without_grep_flags_every_grep_compat_field_is_off() {
        let args = find(&["lets", "find", "x"]);
        assert!(
            !(args.grep.line_number
                || args.grep.recursive
                || args.grep.dereference_recursive
                || args.grep.extended_regexp
                || args.grep.with_filename)
        );
    }

    #[test]
    fn short_l_and_c_set_files_and_count() {
        let files = find(&["lets", "find", "x", "-l"]);
        assert!(files.files && !files.count);
        let count = find(&["lets", "find", "x", "-c"]);
        assert!(count.count && !count.files);
    }

    #[test]
    fn no_expand_is_off_unless_typed() {
        assert!(!find(&["lets", "find", "x"]).no_expand);
        assert!(find(&["lets", "find", "x", "--no-expand"]).no_expand);
    }

    #[test]
    fn g_glob_and_include_collect_into_globs_in_order() {
        let args = find(&["lets", "find", "x", "-g", "a", "-g", "b", "--include", "c"]);
        assert_eq!(args.globs, ["a", "b", "c"]);
        assert_eq!(find(&["lets", "find", "x", "--glob", "*.ts"]).globs, [
            "*.ts"
        ]);
        assert!(find(&["lets", "find", "x"]).globs.is_empty());
    }

    #[test]
    fn a_repeated_switch_parses_as_if_typed_once_on_every_subcommand() {
        for words in [
            &["lets", "find", "-i", "x", ".", "-i"][..],
            &["lets", "find", "x", "-rn", "-r", "--count", "--count"],
            &["lets", "show", "f", "--all", "--all"],
            &[
                "lets", "edit", "f", "--old", "a", "--new", "b", "--all", "--all",
            ],
            &["lets", "write", "f", "--force", "--force"],
            &["lets", "--json", "find", "x", "--json"],
            &["lets", "-q", "-q", "find", "x"],
        ] {
            let parsed = Cli::parse_argv(words.iter().map(OsString::from));
            assert!(parsed.is_ok(), "{words:?}: {:?}", parsed.err());
        }
        assert!(find(&["lets", "find", "-i", "x", "-i"]).ignore_case);
    }

    #[test]
    fn a_repeated_value_option_is_still_refused() {
        for words in [
            &[
                "lets", "edit", "f", "--old", "a", "--new", "b", "--old", "c",
            ][..],
            &["lets", "find", "x", "-C", "1", "-C", "2"],
        ] {
            let err = Cli::parse_argv(words.iter().map(OsString::from)).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::ArgumentConflict, "{words:?}");
        }
    }

    #[test]
    fn find_v_and_invert_match_are_refused_as_a_usage_error_naming_grep_v() {
        for flag in ["-v", "--invert-match"] {
            let err = Cli::parse_argv(["lets", "find", flag, "x"].map(OsString::from)).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
            let rendered = err.render().to_string();
            assert!(
                rendered.trim_end().ends_with(
                    "lets find has no -v: it prints matching lines only \u{b7} use grep -v to \
                     print the lines that do not match"
                ),
                "{rendered}"
            );
        }
    }

    #[test]
    fn edit_takes_further_file_targets_after_the_first() {
        let cli =
            Cli::try_parse_from(["lets", "edit", "f1", "f2", "--old", "a", "--new", "b"]).unwrap();
        let Verb::Edit(args) = cli.verb else {
            panic!("expected Verb::Edit");
        };
        assert_eq!(args.target.as_deref(), Some("f1"));
        assert_eq!(args.more_targets, ["f2"]);
    }

    #[test]
    fn edit_with_one_target_has_no_more_targets() {
        let cli = Cli::try_parse_from(["lets", "edit", "f1", "--old", "a", "--new", "b"]).unwrap();
        let Verb::Edit(args) = cli.verb else {
            panic!("expected Verb::Edit");
        };
        assert!(args.more_targets.is_empty());
    }

    fn stats_since(value: &str) -> Result<Option<Duration>, clap::Error> {
        let cli = Cli::try_parse_from(["lets", "stats", "--since", value])?;
        let Verb::Stats(args) = cli.verb else {
            panic!("expected Verb::Stats");
        };
        Ok(args.since)
    }

    #[test]
    fn since_reads_days_and_hours_as_seconds() {
        assert_eq!(
            stats_since("7d").unwrap(),
            Some(Duration::from_hours(7 * 24))
        );
        assert_eq!(stats_since("12h").unwrap(), Some(Duration::from_hours(12)));
    }

    #[test]
    fn since_refuses_an_unknown_unit_a_zero_count_and_a_bare_unit() {
        for value in ["7x", "0d", "d", "7", "+7d", "7m"] {
            let err = stats_since(value).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::ValueValidation, "{value}");
            assert!(
                err.to_string()
                    .contains("--since takes a count and a unit, d or h, e.g. 7d"),
                "{value}: {err}"
            );
        }
    }

    #[test]
    fn stats_parses_with_no_flags_and_with_a_dir() {
        let cli = Cli::try_parse_from(["lets", "stats"]).unwrap();
        let Verb::Stats(args) = cli.verb else {
            panic!("expected Verb::Stats");
        };
        assert_eq!((args.dir, args.since), (None, None));

        let cli = Cli::try_parse_from(["lets", "stats", "--dir", "/tmp/t"]).unwrap();
        let Verb::Stats(args) = cli.verb else {
            panic!("expected Verb::Stats");
        };
        assert_eq!(args.dir, Some(PathBuf::from("/tmp/t")));
    }

    fn edit_if(value: &str) -> Result<Cli, clap::Error> {
        Cli::try_parse_from([
            "lets", "edit", "f", "--old", "a", "--new", "b", "--if", value,
        ])
    }

    fn transform_if(value: &str) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(["lets", "transform", "f.json", "--set", "a=1", "--if", value])
    }

    #[test]
    fn if_takes_twelve_or_more_lowercase_hex_with_or_without_the_label() {
        for value in ["sha:0123456789ab", "0123456789ab", "sha:0123456789abcdef"] {
            assert!(edit_if(value).is_ok(), "{value}");
            assert!(transform_if(value).is_ok(), "{value}");
        }
    }

    #[test]
    fn if_refuses_short_uppercase_and_non_hex_values() {
        for value in [
            "sha:abc",
            "sha:0123456789AB",
            "sha:0123456789ag",
            "sha:",
            "md5:0123456789ab",
        ] {
            for err in [
                edit_if(value).unwrap_err(),
                transform_if(value).unwrap_err(),
            ] {
                assert_eq!(err.kind(), ErrorKind::ValueValidation, "{value}");
                assert!(
                    err.to_string().contains(
                        "--if takes sha: and 12 or more lowercase hex digits, as a show header \
                         prints it"
                    ),
                    "{value}: {err}"
                );
            }
        }
    }

    fn edit_check(value: &str) -> Result<Cli, clap::Error> {
        Cli::try_parse_from([
            "lets", "edit", "f", "--old", "a", "--new", "b", "--check", value,
        ])
    }

    #[test]
    fn check_accepts_every_preset_and_any_literal_command() {
        for value in [
            "@auto",
            "@cargo",
            "@go",
            "@tsc",
            "@py",
            "tsc",
            "tsc --noEmit {}",
        ] {
            let Verb::Edit(args) = edit_check(value).unwrap().verb else {
                panic!("expected Verb::Edit");
            };
            assert_eq!(args.check.as_deref(), Some(value));
        }
        let cli = Cli::try_parse_from([
            "lets",
            "transform",
            "f.json",
            "--set",
            "a=1",
            "--check",
            "@go",
        ])
        .unwrap();
        let Verb::Transform(args) = cli.verb else {
            panic!("expected Verb::Transform");
        };
        assert_eq!(args.check.as_deref(), Some("@go"));
    }

    #[test]
    fn check_refuses_an_unknown_preset_naming_the_five() {
        for err in [
            edit_check("@nope").unwrap_err(),
            Cli::try_parse_from([
                "lets",
                "transform",
                "f.json",
                "--set",
                "a=1",
                "--check",
                "@nope",
            ])
            .unwrap_err(),
        ] {
            assert_eq!(err.kind(), ErrorKind::ValueValidation);
            assert!(
                err.to_string().contains(
                    "unknown preset @nope \u{b7} the presets are @auto, @cargo, @go, @tsc, @py"
                ),
                "{err}"
            );
        }
    }

    #[test]
    fn hooks_install_parses_both_targets() {
        for (value, target) in [
            ("claude-code", HookTarget::ClaudeCode),
            ("codex", HookTarget::Codex),
        ] {
            let cli = Cli::try_parse_from(["lets", "hooks", "install", value]).unwrap();
            let Verb::Hooks {
                cmd: HooksCmd::Install { target: parsed },
            } = cli.verb
            else {
                panic!("expected Verb::Hooks");
            };
            assert_eq!(parsed, target);
        }
        assert!(Cli::try_parse_from(["lets", "hooks", "install", "cursor"]).is_err());
    }

    #[test]
    fn hooks_uninstall_parses_both_targets() {
        for (value, target) in [
            ("claude-code", HookTarget::ClaudeCode),
            ("codex", HookTarget::Codex),
        ] {
            let cli = Cli::try_parse_from(["lets", "hooks", "uninstall", value]).unwrap();
            let Verb::Hooks {
                cmd: HooksCmd::Uninstall { target: parsed },
            } = cli.verb
            else {
                panic!("expected Verb::Hooks uninstall");
            };
            assert_eq!(parsed, target);
        }
        assert!(Cli::try_parse_from(["lets", "hooks", "uninstall"]).is_err());
    }

    #[test]
    fn update_takes_check_or_force_but_not_both() {
        let cli = Cli::try_parse_from(["lets", "update", "--check"]).unwrap();
        assert!(matches!(cli.verb, Verb::Update {
            check: true,
            force: false
        }));
        let cli = Cli::try_parse_from(["lets", "update", "--force"]).unwrap();
        assert!(matches!(cli.verb, Verb::Update {
            check: false,
            force: true
        }));
        let err = Cli::try_parse_from(["lets", "update", "--check", "--force"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn a_verb_other_than_transform_parses_through_the_same_path() {
        let cli = Cli::parse_argv(["lets", "show", "a.ts"].map(OsString::from)).unwrap();
        assert!(matches!(cli.verb, Verb::Show(_)));
    }
}
