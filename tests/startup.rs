//! This shares the machine with the rest of the suite, so it gates a ratio to a `/bin/true`
//! spawn; `just bench-gate` enforces the absolute `gates::GUIDE` figure.

// Each of the three crates that include this file reads only its own gates.
#[allow(dead_code)]
#[path = "../bench/gates.rs"]
mod gates;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use clap::CommandFactory as _;
use lets::cli::Cli;
use tempfile::TempDir;

/// At 50 spawns the p99 index is the single slowest one, and under `just test`'s parallel load one
/// scheduler hiccup failed the gate; at 200 it is the third slowest, for under half a second.
const RUNS: usize = 200;

/// A shell builtin never counts: it never pays fork+exec.
fn true_binary() -> Option<PathBuf> {
    ["/usr/bin/true", "/bin/true"]
        .into_iter()
        .map(Path::new)
        .find(|path| path.is_file())
        .map(Path::to_path_buf)
}

/// A machine-wide slowdown delays both p50s equally, so only `lets`'s own startup getting worse
/// trips it. Separate so the control below can feed it fabricated durations.
fn guide_ratio_ok(guide_p50: Duration, baseline_p50: Duration) -> bool {
    guide_p50.as_secs_f64() / baseline_p50.as_secs_f64() <= gates::GUIDE_VS_TRUE_MAX_RATIO
}

#[test]
fn guide_ratio_gate_fails_on_a_genuinely_slow_guide() {
    // 50x is far past the max ratio: a regression without spawning a slow process.
    let baseline = Duration::from_micros(500);
    assert!(!guide_ratio_ok(baseline * 50, baseline));
}

#[test]
fn guide_ratio_gate_passes_at_the_dev_box_actual_ratio() {
    // The measured dev-box ratio: the gate must not fail on the number it was set from.
    let baseline = Duration::from_micros(500);
    assert!(guide_ratio_ok(baseline * 4, baseline));
}

/// Integer math, so no `usize`/`f64` cast for clippy to flag.
fn percentile(sorted: &[Duration], numerator: usize, denominator: usize) -> Duration {
    let idx = ((sorted.len() - 1) * numerator + denominator / 2) / denominator;
    sorted[idx]
}

#[test]
fn guide_starts_under_the_loaded_runner_gate_and_is_deterministic() {
    let bin = env!("CARGO_BIN_EXE_lets");
    // Created once, outside the timed loop, so mkdtemp cost never lands inside a sample.
    let home = TempDir::new().expect("a temp HOME");
    let baseline = true_binary();

    let mut durations = Vec::with_capacity(RUNS);
    let mut baseline_durations = Vec::with_capacity(RUNS);
    let mut first_stdout: Option<Vec<u8>> = None;

    for _ in 0..RUNS {
        let start = Instant::now();
        let output = Command::new(bin)
            .arg("guide")
            .current_dir(home.path())
            .env("HOME", home.path())
            .env("XDG_RUNTIME_DIR", home.path())
            .env("LETS_NO_STATS", "1")
            .output()
            .expect("the built binary runs");
        durations.push(start.elapsed());

        assert!(
            output.status.success(),
            "a crashing spawn is not a timing sample: {:?}",
            output.status
        );
        match &first_stdout {
            None => first_stdout = Some(output.stdout),
            Some(expected) => assert_eq!(
                &output.stdout, expected,
                "identical input and file state must produce byte-identical stdout"
            ),
        }

        // Interleaved per spawn, so a neighbour process that comes and goes lands on both sides of
        // the same pair.
        if let Some(true_bin) = &baseline {
            let baseline_start = Instant::now();
            let status = Command::new(true_bin).status().expect("`true` runs");
            baseline_durations.push(baseline_start.elapsed());
            assert!(status.success(), "`true` itself failed: {status:?}");
        }
    }

    durations.sort_unstable();
    let p50 = percentile(&durations, 50, 100);
    let p99 = percentile(&durations, 99, 100);

    let Some(_) = &baseline else {
        // A missing reference tool is not this test's failure to report.
        println!(
            "lets guide startup over {RUNS} spawns: p50={:.3} ms p99={:.3} ms (no `true` on PATH \
             — relative gate skipped)",
            p50.as_secs_f64() * 1000.0,
            p99.as_secs_f64() * 1000.0,
        );
        return;
    };

    baseline_durations.sort_unstable();
    let baseline_p50 = percentile(&baseline_durations, 50, 100);
    let ratio = p50.as_secs_f64() / baseline_p50.as_secs_f64();

    println!(
        "lets guide startup over {RUNS} spawns: p50={:.3} ms p99={:.3} ms; `true` baseline \
         p50={:.3} ms; ratio={:.3} (max {:.1})",
        p50.as_secs_f64() * 1000.0,
        p99.as_secs_f64() * 1000.0,
        baseline_p50.as_secs_f64() * 1000.0,
        ratio,
        gates::GUIDE_VS_TRUE_MAX_RATIO,
    );

    assert!(
        guide_ratio_ok(p50, baseline_p50),
        "lets guide p50 {:.3} ms is {ratio:.2}x a `true` spawn's {:.3} ms, over the {:.1}x gate \
         (p99 {:.3} ms) — this is lets's own startup getting slower, not shared machine load, \
         since the baseline spawn absorbed the same load",
        p50.as_secs_f64() * 1000.0,
        baseline_p50.as_secs_f64() * 1000.0,
        gates::GUIDE_VS_TRUE_MAX_RATIO,
        p99.as_secs_f64() * 1000.0,
    );
}

#[test]
fn readme_quickstart_is_docs_guide_md_byte_for_byte() {
    let readme = include_str!("../README.md");
    let guide = include_str!("../docs/guide.md");

    let heading = readme.find("## Quickstart").expect("a Quickstart heading");
    let fence_start = readme[heading..].find("```\n").expect("the opening fence") + heading + 4;
    let fence_end = fence_start
        + readme[fence_start..]
            .find("```\n")
            .expect("the closing fence");

    assert_eq!(
        &readme[fence_start..fence_end],
        guide,
        "README's Quickstart block has drifted from docs/guide.md, the one source of `lets guide`"
    );
}

/// A flag lives on the subcommand that defines it, so each subcommand is rendered on its own.
fn full_help_text(cmd: &mut clap::Command) -> String {
    let mut text = cmd.render_long_help().to_string();
    for sub in cmd.get_subcommands_mut() {
        text.push('\n');
        text.push_str(&full_help_text(sub));
    }
    text
}

#[test]
fn readme_backtick_verbs_and_flags_are_real_cli_surface() {
    let readme = include_str!("../README.md");
    let mut cmd = Cli::command();
    let verbs: BTreeSet<String> = cmd
        .get_subcommands()
        .map(|sub| sub.get_name().to_string())
        .collect();
    let help = full_help_text(&mut cmd);

    // A fenced block's own ``` delimiters pair up with each other under a naive single-backtick
    // scan, so the Quickstart and install fences are stripped before looking for inline spans.
    let fence_re = regex::Regex::new(r"(?s)```.*?```").unwrap();
    let prose = fence_re.replace_all(readme, "");

    let span_re = regex::Regex::new(r"`([^`]+)`").unwrap();
    let flag_re = regex::Regex::new(r"^--\w[\w-]*$").unwrap();
    let mut checked = Vec::new();

    for span in span_re.captures_iter(&prose) {
        for token in span[1].split_whitespace() {
            if !flag_re.is_match(token) && !verbs.contains(token) {
                continue;
            }
            assert!(
                help.contains(token),
                "README backtick-quotes `{token}`, which is missing from `Cli::command()`'s help text"
            );
            checked.push(token.to_string());
        }
    }

    assert!(
        !checked.is_empty(),
        "no backtick-quoted verb or long flag found in README.md — the extraction broke"
    );
}
