use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use clap::error::ErrorKind;
use lets::cli::{Cli, Global, Verb};
use lets::error::Error;
use lets::output::{self, Body, Format, RenderOptions, Response};
use lets::{Outcome, verbs};

/// `EX_USAGE`: clap's own 2 would read as an ambiguous target, which exits 2 here.
const USAGE: u8 = 64;

fn main() -> ExitCode {
    match Cli::parse_args() {
        Ok(cli) => {
            let format = format_of(&cli.global);
            let mut outcome = dispatch(&cli.verb, &cli.global, format);
            outcome.response.stats.token_ratio = output::token_ratio(
                std::env::var_os("LETS_NO_STATS").as_deref(),
                std::env::var_os("LETS_TOKEN_RATIO").as_deref(),
            );
            report(&outcome, format, render_options(&cli))
        },
        Err(error) => parse_failure(&error, wants_json(std::env::args_os())),
    }
}

type Stderr = (String, &'static str, u8);

/// Pure, so a test can check `report`'s decisions without spawning the binary.
fn decide(
    outcome: &Outcome,
    format: Format,
    opts: RenderOptions,
) -> (Option<String>, Option<Stderr>) {
    // A JSON call that fails with nothing to print still gets one object, so a consumer never
    // has to parse stderr.
    let stdout = match (outcome.response.has_output(), &outcome.error) {
        (true, _) => Some(output::render(&outcome.response, format, &opts)),
        (false, Some(error)) if matches!(format, Format::Json | Format::Jsonl) => Some(
            output::render_error(&outcome.response, error.slug(), &error.to_string()),
        ),
        (false, _) => None,
    };
    let stderr = outcome
        .error
        .as_ref()
        .map(|error| (error.to_string(), error.slug(), code_for(error)));
    (stdout, stderr)
}

/// Stdout before the error: `show a.ts missing.ts` prints what resolved and still exits 1.
fn report(outcome: &Outcome, format: Format, opts: RenderOptions) -> ExitCode {
    let (stdout, stderr) = decide(outcome, format, opts);
    let written = match &stdout {
        Some(text) => output::write_stdout(text),
        None => Ok(()),
    };
    match stderr {
        Some((message, slug, code)) => {
            // The verb's error goes last, so the final `ERROR_CODE=` line is the one to branch on.
            if let Err(source) = written {
                let io_error = io_error(source);
                let _ = output::write_error(&io_error.to_string(), io_error.slug());
            }
            let _ = output::write_error(&message, slug);
            ExitCode::from(code)
        },
        None => exit_for_write(written),
    }
}

fn io_error(source: std::io::Error) -> Error {
    Error::Io {
        path: PathBuf::from("-"),
        source,
    }
}

/// A closed reader (`lets show big.ts | head`) is ordinary use, so `BrokenPipe` exits 0.
fn exit_for_write(written: std::io::Result<()>) -> ExitCode {
    match written {
        Ok(()) => ExitCode::SUCCESS,
        Err(source) if source.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(source) => exit_with(&io_error(source)),
    }
}

fn exit_with(error: &Error) -> ExitCode {
    let _ = output::write_error(&error.to_string(), error.slug());
    ExitCode::from(code_for(error))
}

/// The crate's only exit-code mapping; a code chosen inside a verb would be a second contract.
fn code_for(error: &Error) -> u8 {
    match error {
        Error::NotFound { .. }
        | Error::Refused { .. }
        | Error::InvalidPattern { .. }
        | Error::OverCap { .. }
        | Error::NoGrammar { .. }
        | Error::InstallRefused { .. }
        | Error::MistypedTarget { .. }
        | Error::NoHits { .. }
        | Error::EmptyFile { .. }
        | Error::MixedEndings { .. }
        | Error::GuessedSpan { .. }
        | Error::UpdateAvailable { .. } => 1,
        Error::Ambiguous { .. } | Error::ExpectRefused { .. } => 2,
        Error::CheckFailed { .. } => 3,
        Error::OverBudget { .. } => 4,
        Error::Changed { .. } => 5,
        Error::OutsideTree { .. } => 6,
        Error::Unsupported { .. }
        | Error::UpdateFailed { .. }
        | Error::Locked { .. }
        | Error::ReadOnly { .. } => 7,
        Error::PartialBatch { .. } => 8,
        Error::Usage { .. } => USAGE,
        Error::Several { errors } => errors.first().map_or(1, code_for),
        Error::Io { source, .. } => {
            if source.kind() == std::io::ErrorKind::NotFound {
                1
            } else {
                7
            }
        },
    }
}

fn dispatch(verb: &Verb, global: &Global, format: Format) -> Outcome {
    match verb {
        Verb::Guide => verbs::guide::run(),
        Verb::Version => version(format),
        Verb::Show(args) => verbs::show::run(args, global, format),
        Verb::Find(args) => verbs::find::run(args, global, format),
        Verb::Edit(args) => verbs::edit::run(args, global, format),
        Verb::Transform(args) => verbs::transform::run(args, global, format),
        Verb::Write(args) => verbs::write::run(args, global, format),
        Verb::Stats(args) => verbs::stats::run(args, format),
        Verb::Update { check, force } => verbs::update::run(*check, *force, format),
        Verb::Hooks { cmd } => verbs::hooks::run(cmd, format),
        Verb::Hook { .. } => verbs::hook::run(),
    }
}

/// Inline rather than under `verbs/`: its whole body is the text `lets <version>`.
fn version(format: Format) -> Outcome {
    let semver = env!("CARGO_PKG_VERSION");
    let text = match format {
        Format::Text => format!("lets {semver}\n"),
        Format::Json | Format::Jsonl => semver.to_owned(),
    };
    let mut response = Response::empty("version");
    response.body = Body::Raw {
        field: "version",
        text,
    };
    Outcome::ok(response)
}

/// `render()`, not clap's `print()`, which adds colour on a terminal.
fn parse_failure(error: &clap::Error, json: bool) -> ExitCode {
    match error.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
            exit_for_write(output::write_stdout(&error.render().to_string()))
        },
        _ => {
            let rendered = error.render().to_string();
            let message = rendered.trim_end();
            if json {
                let _ = output::write_stdout(&usage_error_object(message));
            }
            let _ = output::write_error(message, "usage");
            ExitCode::from(USAGE)
        },
    }
}

fn usage_error_object(message: &str) -> String {
    let mut response = Response::empty("usage");
    response.stats.token_ratio = output::token_ratio(
        std::env::var_os("LETS_NO_STATS").as_deref(),
        std::env::var_os("LETS_TOKEN_RATIO").as_deref(),
    );
    output::render_error(&response, "usage", message)
}

/// Clap failed, so there is no `Global`: the flag is read off the raw argv, up to `--`.
fn wants_json(argv: impl IntoIterator<Item = std::ffi::OsString>) -> bool {
    argv.into_iter()
        .skip(1)
        .take_while(|arg| arg != "--")
        .any(|arg| arg == "--json" || arg == "--jsonl")
}

fn format_of(global: &Global) -> Format {
    if global.json {
        Format::Json
    } else if global.jsonl {
        Format::Jsonl
    } else {
        Format::Text
    }
}

fn render_options(cli: &Cli) -> RenderOptions {
    RenderOptions {
        numbers: !matches!(&cli.verb, Verb::Show(args) if args.no_numbers),
        quiet: cli.global.quiet,
        cost_first: matches!(&cli.verb, Verb::Show(args) if args.all),
    }
}

#[cfg(test)]
mod tests {
    use lets::error::{CheckLayer, RefusedReason, UnsupportedReason};

    use super::*;

    fn io(kind: std::io::ErrorKind) -> Error {
        Error::Io {
            path: PathBuf::from("f"),
            source: std::io::Error::new(kind, "boom"),
        }
    }

    #[test]
    fn every_error_maps_to_the_exit_code_its_spec_section_names() {
        assert_eq!(
            code_for(&Error::NotFound {
                target: "a.ts".into(),
                what: "--old".into(),
                nearest: None,
            }),
            1
        );
        assert_eq!(
            code_for(&Error::Ambiguous {
                target: "a.ts#Open".into(),
                candidates: vec![],
            }),
            2
        );
        assert_eq!(
            code_for(&Error::CheckFailed {
                path: PathBuf::from("a.ts"),
                layer: CheckLayer::Structure,
                detail: "unexpected ')'".into(),
            }),
            3
        );
        assert_eq!(
            code_for(&Error::OverBudget {
                bytes: 90_000,
                limit: 65536,
            }),
            4
        );
        assert_eq!(
            code_for(&Error::Changed {
                path: PathBuf::from("a.ts"),
                expected: "e77be77be77b".into(),
                actual: "b410b410b410".into(),
            }),
            5
        );
        assert_eq!(
            code_for(&Error::OutsideTree {
                path: PathBuf::from("/home/user/.zshrc"),
            }),
            6
        );
        assert_eq!(
            code_for(&Error::Unsupported {
                path: PathBuf::from("a.bin"),
                reason: UnsupportedReason::Binary,
            }),
            7
        );
        assert_eq!(
            code_for(&Error::PartialBatch {
                written: vec![PathBuf::from("a.ts")],
                failed: PathBuf::from("b.ts"),
                detail: "rename failed".into(),
            }),
            8
        );
    }

    #[test]
    fn every_new_error_maps_to_the_exit_code_its_spec_section_names() {
        assert_eq!(
            code_for(&Error::Usage {
                message: "bad flag".into()
            }),
            USAGE
        );
        assert_eq!(
            code_for(&Error::ExpectRefused {
                target: "a.ts:3".into(),
                reason: lets::error::ExpectReason::Range { first: 3, last: 5 },
            }),
            2
        );
        assert_eq!(
            code_for(&Error::Locked {
                path: PathBuf::from("a.ts"),
            }),
            7
        );
        assert_eq!(
            code_for(&Error::ReadOnly {
                path: PathBuf::from("a.ts"),
            }),
            7
        );
        assert_eq!(
            code_for(&Error::UpdateAvailable {
                current: "0.0.1".into(),
                latest: "0.2.0".into(),
            }),
            1
        );
        assert_eq!(
            code_for(&Error::UpdateFailed {
                detail: "curl exit status: 6".into(),
            }),
            7
        );
        assert_eq!(
            code_for(&Error::MistypedTarget {
                target: "a.ts".into(),
                meant: "a.tsx".into(),
            }),
            1
        );
        assert_eq!(
            code_for(&Error::NoHits {
                pattern: "onBack".into(),
                grep_style: None,
            }),
            1
        );
        assert_eq!(
            code_for(&Error::EmptyFile {
                path: PathBuf::from("a.ts"),
            }),
            1
        );
        assert_eq!(
            code_for(&Error::MixedEndings {
                path: PathBuf::from("a.ts"),
            }),
            1
        );
        assert_eq!(
            code_for(&Error::GuessedSpan {
                target: "a.ts#Open".into(),
                command: String::new(),
            }),
            1
        );
    }

    #[test]
    fn several_failures_exit_and_slug_as_the_first_and_name_every_one() {
        let ambiguous_first = Error::all(vec![
            Error::Ambiguous {
                target: "a.ts#open".into(),
                candidates: vec![],
            },
            io(std::io::ErrorKind::NotFound),
        ])
        .expect("two failures");
        let missing_first = Error::all(vec![io(std::io::ErrorKind::NotFound), Error::Ambiguous {
            target: "a.ts#open".into(),
            candidates: vec![],
        }])
        .expect("two failures");

        assert_eq!(
            (code_for(&ambiguous_first), ambiguous_first.slug()),
            (2, "ambiguous")
        );
        assert_eq!(
            (code_for(&missing_first), missing_first.slug()),
            (1, "not_found")
        );
        let message = missing_first.to_string();
        assert_eq!(message.lines().count(), 2, "{message}");
        assert!(message.contains("a.ts#open is ambiguous"), "{message}");
    }

    #[test]
    fn one_failure_stays_itself_and_none_is_no_error() {
        let one = Error::all(vec![io(std::io::ErrorKind::NotFound)]).expect("one failure");
        assert!(matches!(one, Error::Io { .. }), "{one:?}");
        assert!(Error::all(Vec::new()).is_none());
    }

    #[test]
    fn find_s_own_failures_and_a_missing_grammar_exit_1_with_their_own_slugs() {
        let invalid = Error::InvalidPattern {
            pattern: "(unclosed".into(),
            message: "unclosed group".into(),
        };
        let over_cap = Error::OverCap {
            hits: 312,
            files: 47,
            cap: 50,
        };
        let no_grammar = Error::NoGrammar {
            path: PathBuf::from("app.vue"),
            ext: "vue".into(),
        };

        assert_eq!(code_for(&invalid), 1);
        assert_eq!(code_for(&over_cap), 1);
        assert_eq!(code_for(&no_grammar), 1);

        let slugs = [invalid.slug(), over_cap.slug(), no_grammar.slug()];
        assert_eq!(slugs, ["invalid_pattern", "over_cap", "no_grammar"]);
    }

    #[test]
    fn both_refusals_exit_1_and_differ_only_in_the_slug() {
        let exists = Error::Refused {
            path: PathBuf::from("scripts/x.sh"),
            reason: RefusedReason::Exists {
                lines: 2,
                sha: "0e1f0e1f0e1f".into(),
            },
        };
        let empty = Error::Refused {
            path: PathBuf::from("scripts/x.sh"),
            reason: RefusedReason::EmptyInput,
        };

        assert_eq!(code_for(&exists), 1);
        assert_eq!(code_for(&empty), 1);
        assert_ne!(exists.slug(), empty.slug());
    }

    #[test]
    fn a_missing_file_exits_1_and_any_other_io_failure_exits_7() {
        assert_eq!(code_for(&io(std::io::ErrorKind::NotFound)), 1);
        assert_eq!(code_for(&io(std::io::ErrorKind::PermissionDenied)), 7);
    }

    #[test]
    fn content_and_a_non_zero_exit_are_not_exclusive() {
        let mut response = Response::empty("edit");
        response.body = Body::Raw {
            field: "edit",
            text: "── src/a.ts · 1 replacement · line 42\n".to_owned(),
        };
        let outcome = Outcome::partial(response, Error::PartialBatch {
            written: vec![PathBuf::from("src/a.ts")],
            failed: PathBuf::from("src/b.ts"),
            detail: "rename failed".into(),
        });
        let error = outcome.error.as_ref().expect("a partial batch is terminal");

        assert!(outcome.response.has_output());
        assert!(
            output::render(&outcome.response, Format::Text, &RenderOptions {
                numbers: true,
                quiet: false,
                cost_first: false,
            })
            .contains("src/a.ts"),
            "the file that landed is named on stdout"
        );
        assert_eq!(code_for(error), 8);
    }

    fn opts() -> RenderOptions {
        RenderOptions {
            numbers: true,
            quiet: false,
            cost_first: false,
        }
    }

    #[test]
    fn decide_pairs_the_file_that_landed_with_the_batch_s_terminal_error() {
        let mut response = Response::empty("edit");
        response.body = Body::Raw {
            field: "edit",
            text: "── src/a.ts · 1 replacement · line 42\n".to_owned(),
        };
        let outcome = Outcome::partial(response, Error::PartialBatch {
            written: vec![PathBuf::from("src/a.ts")],
            failed: PathBuf::from("src/b.ts"),
            detail: "rename failed".into(),
        });

        let (stdout, stderr) = decide(&outcome, Format::Text, opts());

        assert!(
            stdout
                .expect("the file that landed still has output")
                .contains("src/a.ts"),
            "stdout must not be dropped just because the outcome also carries an error"
        );
        let (_, slug, code) = stderr.expect("a partial batch is terminal");
        assert_eq!((slug, code), ("partial_batch", 8));
    }

    #[test]
    fn decide_has_no_stdout_for_a_verb_with_nothing_to_say() {
        let outcome = Outcome::failed("show", Error::OverBudget {
            bytes: 10,
            limit: 5,
        });

        let (stdout, stderr) = decide(&outcome, Format::Text, opts());

        assert_eq!(stdout, None);
        let (_, slug, code) = stderr.expect("the terminal error");
        assert_eq!((slug, code), ("over_budget", 4));
    }

    #[test]
    fn decide_writes_an_error_object_to_stdout_when_json_has_nothing_else_to_say() {
        let outcome = Outcome::failed("show", Error::NotFound {
            target: "nope.txt".into(),
            what: "target".into(),
            nearest: None,
        });

        let (stdout, stderr) = decide(&outcome, Format::Json, opts());

        let stdout = stdout.expect("a json call still owes its caller an object");
        assert!(stdout.contains("\"slug\":\"not_found\""), "{stdout}");
        assert!(stdout.contains("\"omitted\":[]"), "{stdout}");
        let (_, slug, code) = stderr.expect("the terminal error");
        assert_eq!((slug, code), ("not_found", 1));
    }

    #[test]
    fn decide_has_no_stdout_for_the_same_failure_in_text_format() {
        let outcome = Outcome::failed("show", Error::NotFound {
            target: "nope.txt".into(),
            what: "target".into(),
            nearest: None,
        });

        let (stdout, _) = decide(&outcome, Format::Text, opts());

        assert_eq!(
            stdout, None,
            "text keeps stderr as the only carrier of the error"
        );
    }

    #[test]
    fn wants_json_reads_either_flag_only_before_the_operand_separator() {
        let argv = |args: &[&str]| -> Vec<std::ffi::OsString> {
            args.iter().map(std::ffi::OsString::from).collect()
        };
        assert!(wants_json(argv(&["lets", "--json", "edit", "a.rs"])));
        assert!(wants_json(argv(&["lets", "show", "a.rs", "--jsonl"])));
        assert!(!wants_json(argv(&["lets", "show", "a.rs"])));
        assert!(!wants_json(argv(&["lets", "write", "--", "--json"])));
        assert!(!wants_json(argv(&["--json"])));
    }

    #[test]
    fn decide_keeps_ordinary_json_when_the_response_already_has_output() {
        let mut response = Response::empty("edit");
        response.body = Body::Raw {
            field: "edit",
            text: "── src/a.ts · 1 replacement · line 42\n".to_owned(),
        };
        let outcome = Outcome::partial(response, Error::PartialBatch {
            written: vec![PathBuf::from("src/a.ts")],
            failed: PathBuf::from("src/b.ts"),
            detail: "rename failed".into(),
        });

        let (stdout, _) = decide(&outcome, Format::Json, opts());

        let stdout = stdout.expect("a partial batch still has content to report");
        assert!(
            !stdout.contains("\"error\""),
            "a response with output renders its own shape, not the error object: {stdout}"
        );
    }
}
