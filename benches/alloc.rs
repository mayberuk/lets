//! divan 0.1 only prints `AllocProfiler`'s tallies and keeps no baseline, so this binary re-runs
//! itself: the child runs divan, the parent parses its table. C `malloc` is invisible here.

#[allow(
    dead_code,
    unused_imports,
    reason = "bench/gates.rs is shared, each consumer reads a subset, and its test module's \
              import has no tests to serve in a harness-less bench"
)]
#[path = "../bench/gates.rs"]
mod gates;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use clap::Parser as _;
use divan::counter::BytesFormat;
use lets::cli::{Cli, OpKind, Verb};
use lets::hook::{self, Verdict};
use lets::output::{self, Format, RenderOptions};
use lets::{Outcome, verbs};
use tempfile::TempDir;

#[global_allocator]
static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

const CHILD_ENV: &str = "LETS_ALLOC_BENCH_CHILD";
const SAVE_FLAG: &str = "--save-baseline";
const CORPUSGEN: &str = "cargo run --manifest-path tests/corpusgen/Cargo.toml --release";

/// Warm iterations allocate identically, so a few samples settle the figure.
const SAMPLES: u32 = 5;

const HOOK_COMMAND_BYTES: usize = 200;
const EDIT_FILE_LINES: usize = 200;
const BATCH_FILES: usize = 10;

/// Big enough for per-sibling path work to dominate; half `--max-file-bytes`, so never refused.
const JSON_FIXTURE_BYTES: usize = 4 * 1024 * 1024;

/// Every corpus TypeScript file opens with `unit_0`, and `+ 0;` cannot match `+ 10;`.
const TS_OLD: &str = "return x + 0;";
const TS_NEW: &str = "return x + 0 + 1;";
/// Same length as the text it replaces, so the 8 MiB file stays at `--max-file-bytes`.
const MD_OLD: &str = "Paragraph body for unit 0.";
const MD_NEW: &str = "Paragraph body for unit 0!";

const BENCHES: [&str; 5] = [
    "edit_8mib_peak_bytes",
    "edit_from_stdin_10_files_proxy",
    "edit_structural_check_200_line_file",
    "hook_classify_200_char_command",
    "transform_set_multi_mib_json",
];

fn main() -> ExitCode {
    if std::env::var_os(CHILD_ENV).is_some() {
        prime();
        divan::Divan::default()
            .color(false)
            .bytes_format(BytesFormat::Decimal)
            .sample_count(SAMPLES)
            .sample_size(1)
            .run_benches();
        return ExitCode::SUCCESS;
    }
    match gate() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(message) => {
            eprintln!("alloc bench: {message}");
            ExitCode::FAILURE
        },
    }
}

#[divan::bench]
fn hook_classify_200_char_command(bencher: divan::Bencher) {
    let payload = hook_payload();
    bencher.bench_local(|| hook::classify(divan::black_box(&payload)));
}

#[divan::bench]
fn edit_structural_check_200_line_file(bencher: divan::Bencher) {
    let source = nearest_typescript(EDIT_FILE_LINES);
    bencher
        .with_inputs(|| Staged::edits(std::slice::from_ref(&source), TS_OLD, TS_NEW))
        .bench_local_refs(|staged| staged.run());
}

/// `--from -` gives `batch.rs` the same specs; `wall_clock` times the stdin parse skipped here.
#[divan::bench]
fn edit_from_stdin_10_files_proxy(bencher: divan::Bencher) {
    let sources: Vec<PathBuf> = typescript_files().into_iter().take(BATCH_FILES).collect();
    bencher
        .with_inputs(|| Staged::edits(&sources, TS_OLD, TS_NEW))
        .bench_local_refs(|staged| staged.run());
}

#[divan::bench]
fn transform_set_multi_mib_json(bencher: divan::Bencher) {
    let (fixture, set) = json_fixture();
    bencher
        .with_inputs(|| Staged::transform(&fixture, &set))
        .bench_local_refs(|staged| staged.run());
}

#[divan::bench]
fn edit_8mib_peak_bytes(bencher: divan::Bencher) {
    let source = corpus().join("specials/large.md");
    bencher
        .with_inputs(|| Staged::edits(std::slice::from_ref(&source), MD_OLD, MD_NEW))
        .bench_local_refs(|staged| staged.run());
}

/// Warms the lazily initialised grammars so no sample pays for them, and proves each run succeeds.
fn prime() {
    assert!(
        matches!(hook::classify(&hook_payload()), Verdict::Block { .. }),
        "hook_classify_200_char_command: the payload must block, or the bash parse is not measured"
    );
    let source = nearest_typescript(EDIT_FILE_LINES);
    let batch: Vec<PathBuf> = typescript_files().into_iter().take(BATCH_FILES).collect();
    let (fixture, set) = json_fixture();
    let large = corpus().join("specials/large.md");
    for (name, staged) in [
        (
            "edit_structural_check_200_line_file",
            Staged::edits(std::slice::from_ref(&source), TS_OLD, TS_NEW),
        ),
        (
            "edit_from_stdin_10_files_proxy",
            Staged::edits(&batch, TS_OLD, TS_NEW),
        ),
        (
            "transform_set_multi_mib_json",
            Staged::transform(&fixture, &set),
        ),
        (
            "edit_8mib_peak_bytes",
            Staged::edits(std::slice::from_ref(&large), MD_OLD, MD_NEW),
        ),
    ] {
        for (outcome, _) in staged.run() {
            assert!(outcome.error.is_none(), "{name}: {:?}", outcome.error);
        }
    }
}

/// Staged under the cwd, which the parent made the child's scope root.
struct Staged {
    _dir: TempDir,
    calls: Vec<Cli>,
}

impl Staged {
    fn edits(sources: &[PathBuf], old: &str, new: &str) -> Staged {
        let dir = stage_dir();
        let calls = sources
            .iter()
            .map(|source| {
                let copy = dir
                    .path()
                    .join(source.file_name().expect("corpus file name"));
                std::fs::copy(source, &copy).expect("copy corpus file");
                parse(&["edit", path_str(&copy), "--old", old, "--new", new])
            })
            .collect();
        Staged { _dir: dir, calls }
    }

    fn transform(fixture: &[u8], set: &str) -> Staged {
        let dir = stage_dir();
        let file = dir.path().join("units.json");
        std::fs::write(&file, fixture).expect("write JSON fixture");
        let mut cli = parse(&["transform", path_str(&file), "--set", set]);
        // The derive leaves `order` empty; only the binary's argv parse fills it.
        if let Verb::Transform(args) = &mut cli.verb {
            args.order = vec![OpKind::Set];
        }
        Staged {
            _dir: dir,
            calls: vec![cli],
        }
    }

    /// Includes the text render, so its allocations count as they do in `main.rs`.
    fn run(&self) -> Vec<(Outcome, Option<String>)> {
        self.calls
            .iter()
            .map(|cli| {
                let outcome = match &cli.verb {
                    Verb::Edit(args) => verbs::edit::run(args, &cli.global, Format::Text),
                    Verb::Transform(args) => verbs::transform::run(args, &cli.global, Format::Text),
                    other => unreachable!("only edit and transform are staged, got {other:?}"),
                };
                let opts = RenderOptions {
                    numbers: true,
                    quiet: cli.global.quiet,
                    cost_first: false,
                };
                let text = outcome
                    .response
                    .has_output()
                    .then(|| output::render(&outcome.response, Format::Text, &opts));
                (outcome, text)
            })
            .collect()
    }
}

fn stage_dir() -> TempDir {
    let root = std::env::current_dir().expect("working directory");
    tempfile::Builder::new()
        .tempdir_in(root)
        .expect("stage directory")
}

fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(std::iter::once("lets").chain(args.iter().copied()))
        .unwrap_or_else(|error| panic!("staged argv {args:?} does not parse: {error}"))
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("temp paths are UTF-8")
}

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/corpus")
}

fn typescript_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![corpus().join("typescript")];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("read corpus directory") {
            let path = entry.expect("corpus entry").path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// The same pick `benches/wall_clock.rs` times, so both targets measure one file.
fn nearest_typescript(lines: usize) -> PathBuf {
    typescript_files()
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path).expect("read corpus file");
            (text.lines().count(), path)
        })
        .filter(|(count, _)| *count <= lines)
        .min_by_key(|(count, path)| (count.abs_diff(lines), path.clone()))
        .map(|(_, path)| path)
        .expect("the corpus has a TypeScript file at or under the target line count")
}

/// The array sits under a key because a path cannot start with an index; the `--set` targets the
/// second-to-last sibling so resolution walks nearly all of them.
fn json_fixture() -> (Vec<u8>, String) {
    let mut out = String::with_capacity(JSON_FIXTURE_BYTES + 64);
    out.push_str("{\"units\": [\n");
    let mut count = 0usize;
    while out.len() < JSON_FIXTURE_BYTES {
        if count > 0 {
            out.push_str(",\n");
        }
        write!(out, "  {{\"index\": {count}, \"value\": \"unit-{count}\"}}").expect("String write");
        count += 1;
    }
    out.push_str("\n]}\n");
    (out.into_bytes(), format!("units[{}].value=late", count - 2))
}

fn hook_payload() -> Vec<u8> {
    let cwd = std::env::current_dir().expect("working directory");
    let command = hook_command();
    serde_json::to_vec(&serde_json::json!({
        "session_id": "alloc-bench",
        "cwd": path_str(&cwd),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": { "command": command },
    }))
    .expect("serialize hook payload")
}

/// `sed -i`, not `cat`: chained whole-file reads now rewrite instead of blocking, and `sed -i`
/// with a `g` flag still denies without needing the named files to exist (unlike a search, whose
/// deny only fires for a path this workload's synthetic corpus doesn't have). Built as
/// `benches/wall_clock.rs`'s `blocked_command` is, so both targets measure one command.
fn hook_command() -> String {
    const LINK: &str = " && sed -i 's/x/x/g' src/module_00.ts";
    const TAIL: &str = " && sed -i 's/x/x/g' src/.ts";
    let mut command = String::from("sed -i 's/x/x/g' src/a.ts");
    let mut index = 0;
    while command.len() + LINK.len() + TAIL.len() < HOOK_COMMAND_BYTES {
        write!(command, " && sed -i 's/x/x/g' src/module_{index:02}.ts").expect("String write");
        index += 1;
    }
    let pad = HOOK_COMMAND_BYTES - command.len() - TAIL.len();
    write!(command, " && sed -i 's/x/x/g' src/{}.ts", "x".repeat(pad)).expect("String write");
    assert_eq!(command.len(), HOOK_COMMAND_BYTES);
    command
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Metrics {
    /// Allocations plus in-place grows: a lost `with_capacity` shows up as grows, not allocs.
    count: u64,
    bytes: u64,
    peak_bytes: u64,
}

type Baseline = BTreeMap<String, Metrics>;

fn baseline_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/baselines/alloc.json")
}

/// `Ok(false)`: over the gate, already printed. `Err`: nothing could be measured.
fn gate() -> Result<bool, String> {
    let save = save_mode()?;
    check_corpus()?;
    let table = run_child()?;
    print!("{table}");
    let measured = parse_table(&table)?;
    for name in BENCHES {
        if !measured.contains_key(name) {
            return Err(format!("{name}: missing from divan's table"));
        }
    }

    let mut report = String::new();
    if save {
        let mut json = serde_json::to_string_pretty(&measured).map_err(|e| e.to_string())?;
        json.push('\n');
        let path = baseline_path();
        std::fs::create_dir_all(path.parent().expect("baseline directory"))
            .and_then(|()| std::fs::write(&path, json))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        println!("alloc gate: wrote {}", path.display());
        return Ok(true);
    }

    let path = baseline_path();
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "{}: {e}; run `cargo bench --bench alloc --locked -- {SAVE_FLAG}`",
            path.display()
        )
    })?;
    let baseline: Baseline =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let failed = check_regressions(&baseline, &measured, &mut report);
    print!("{report}");
    println!(
        "alloc gate: {}",
        if failed {
            "FAILED"
        } else {
            "all within baseline"
        }
    );
    Ok(!failed)
}

fn save_mode() -> Result<bool, String> {
    let mut save = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            // `cargo bench` passes `--bench` to every bench target.
            "--bench" => {},
            SAVE_FLAG => save = true,
            other => {
                return Err(format!(
                    "unknown argument `{other}`; the only flag is {SAVE_FLAG}"
                ));
            },
        }
    }
    Ok(save)
}

fn check_corpus() -> Result<(), String> {
    let manifest = std::fs::read_to_string(corpus().join(".manifest")).unwrap_or_default();
    if manifest.lines().any(|line| line == "profile=full") {
        Ok(())
    } else {
        Err(format!(
            "tests/fixtures/corpus is not the full profile; run `{CORPUSGEN}`"
        ))
    }
}

/// `work` has no `.git` above it, so the scope guard treats it as the tree root.
fn run_child() -> Result<String, String> {
    let sandbox = TempDir::new().map_err(|e| format!("sandbox: {e}"))?;
    let [home, runtime, config, work] =
        ["home", "runtime", "config", "work"].map(|name| sandbox.path().join(name));
    for dir in [&home, &runtime, &config, &work] {
        std::fs::create_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let output = Command::new(exe)
        .env_clear()
        .env(CHILD_ENV, "1")
        .env("HOME", &home)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_CONFIG_HOME", &config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("LETS_NO_STATS", "1")
        .env("LETS_TOKEN_RATIO", "4")
        .current_dir(&work)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("spawn divan child: {e}"))?;
    let stdout = String::from_utf8(output.stdout).map_err(|e| format!("divan table: {e}"))?;
    if !output.status.success() {
        print!("{stdout}");
        return Err(format!("divan child exited with {}", output.status));
    }
    Ok(stdout)
}

/// Reads the fastest column, which carries no one-time cost; a row divan omits counts as zero.
fn parse_table(table: &str) -> Result<Baseline, String> {
    let mut benches = BTreeMap::new();
    let mut bench: Option<(String, BTreeMap<String, Vec<u64>>)> = None;
    let mut label: Option<String> = None;
    for line in table.lines() {
        let body = line.strip_prefix('│').unwrap_or(line);
        let cell = body.split('│').next().unwrap_or_default().trim();
        if let Some(rest) = line.strip_prefix("├─").or_else(|| line.strip_prefix("╰─")) {
            if let Some((name, rows)) = bench.take() {
                benches.insert(
                    name.clone(),
                    metrics(&rows).map_err(|e| format!("{name}: {e}"))?,
                );
            }
            let name = rest
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_owned();
            bench = Some((name, BTreeMap::new()));
            label = None;
        } else if let (Some((_, rows)), true) = (&mut bench, cell.ends_with(':')) {
            rows.insert(cell.to_owned(), Vec::new());
            label = Some(cell.to_owned());
        } else if let (Some((name, rows)), Some(row)) = (&mut bench, &label) {
            let values = rows.get_mut(row).expect("label row inserted");
            let value = if values.is_empty() {
                cell.parse().ok()
            } else {
                parse_bytes(cell)
            };
            values.push(value.ok_or_else(|| format!("{name} {row} unreadable value `{cell}`"))?);
            if values.len() == 2 {
                label = None;
            }
        }
    }
    if let Some((name, rows)) = bench.take() {
        benches.insert(
            name.clone(),
            metrics(&rows).map_err(|e| format!("{name}: {e}"))?,
        );
    }
    Ok(benches)
}

/// Every workload allocates, so a zero is a broken parse that would pass every regression check.
fn metrics(rows: &BTreeMap<String, Vec<u64>>) -> Result<Metrics, String> {
    const LABELS: [&str; 5] = ["alloc:", "dealloc:", "grow:", "shrink:", "max alloc:"];
    if let Some(unknown) = rows.keys().find(|label| !LABELS.contains(&label.as_str())) {
        return Err(format!("divan printed an unknown row `{unknown}`"));
    }
    let row = |label: &str| -> Result<(u64, u64), String> {
        match rows.get(label).map(Vec::as_slice) {
            None => Ok((0, 0)),
            Some(&[count, bytes]) => Ok((count, bytes)),
            Some(other) => Err(format!("`{label}` row has {} values, not 2", other.len())),
        }
    };
    let (alloc_count, alloc_bytes) = row("alloc:")?;
    let (grow_count, grow_bytes) = row("grow:")?;
    let (_, peak_bytes) = row("max alloc:")?;
    if alloc_count == 0 || peak_bytes == 0 {
        return Err(
            "divan reported no allocations; is `AllocProfiler` still the global allocator?"
                .to_owned(),
        );
    }
    Ok(Metrics {
        count: alloc_count + grow_count,
        bytes: alloc_bytes + grow_bytes,
        peak_bytes,
    })
}

/// Scaled on the digits, not through `f64`, so divan's four significant figures stay exact.
fn parse_bytes(cell: &str) -> Option<u64> {
    let (number, unit) = cell.split_once(' ')?;
    let exponent: usize = match unit {
        "B" => 0,
        "KB" => 3,
        "MB" => 6,
        "GB" => 9,
        "TB" => 12,
        _ => return None,
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    let scale = exponent.checked_sub(fraction.len())?;
    let digits: u64 = format!("{whole}{fraction}").parse().ok()?;
    digits.checked_mul(10u64.checked_pow(u32::try_from(scale).ok()?)?)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "allocation figures stay far below 2^52, where f64 is exact"
)]
fn check_regressions(baseline: &Baseline, measured: &Baseline, report: &mut String) -> bool {
    let (mut failed, mut any_stale) = (false, false);
    for name in baseline.keys().filter(|name| !measured.contains_key(*name)) {
        writeln!(report, "FAIL  {name}: in the baseline but not measured").expect("String write");
        failed = true;
    }
    for (name, now) in measured {
        let Some(then) = baseline.get(name) else {
            writeln!(report, "FAIL  {name}: no baseline entry").expect("String write");
            failed = true;
            continue;
        };
        for (metric, now, then) in [
            ("count", now.count, then.count),
            ("bytes", now.bytes, then.bytes),
            ("peak_bytes", now.peak_bytes, then.peak_bytes),
        ] {
            let over = now as f64 > then as f64 * gates::ALLOC_REGRESSION_MAX_RATIO;
            // Two-sided: an unrecorded improvement would let a later regression back to it pass.
            let stale = (now as f64) < then as f64 / gates::ALLOC_REGRESSION_MAX_RATIO;
            failed |= over || stale;
            any_stale |= stale;
            let ratio = if then == 0 {
                if now == 0 { 1.0 } else { f64::INFINITY }
            } else {
                now as f64 / then as f64
            };
            writeln!(
                report,
                "{} {name} {metric} {now} against baseline {then} ({ratio:.3}x, max {:.2}x)",
                if over {
                    "FAIL "
                } else if stale {
                    "STALE"
                } else {
                    "ok   "
                },
                gates::ALLOC_REGRESSION_MAX_RATIO
            )
            .expect("String write");
        }
    }
    if any_stale {
        writeln!(
            report,
            "a STALE figure improved past the ratio: rerun `just bench-baseline` and review the diff"
        )
        .expect("String write");
    }
    failed
}
