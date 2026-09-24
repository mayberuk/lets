//! A plain `fn main`, not divan: every sample spawns the release binary, and a breach must exit
//! nonzero.

#[path = "../bench/gates.rs"]
// Each consumer reads a subset, and gates.rs's test module has no harness to run under here.
#[allow(dead_code, unused_imports)]
mod gates;

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs};

use gates::Gate;
use tempfile::TempDir;

/// At 50 samples the p99 is the single slowest spawn, so one scheduler hiccup fails the gate.
const RUNS: usize = 200;

const HOOK_COMMAND_BYTES: usize = 200;
const SMALL_FILE_LINES: usize = 200;
const YAML_FILE_LINES: usize = 500;
const BATCH_FILES: usize = 10;

/// corpusgen's `SEARCH_TARGET_SYMBOL`, placed exactly once in the corpus.
const SEARCH_TARGET: &str = "corpusSearchTarget";

/// Every generated TypeScript filler opens with `unit_0`, whose body is this line, once.
const EDIT_OLD: &str = "return x + 0;";
const EDIT_NEW: &str = "return x - 0;";

/// Once in corpusgen's 8 MiB markdown; a same-length swap keeps it at `--max-file-bytes`.
const LARGE_OLD: &str = "Paragraph body for unit 0.";
const LARGE_NEW: &str = "Paragraph body for unit 0!";

fn main() -> ExitCode {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let bin = env::var_os("LETS_BENCH_BIN")
        .map_or_else(|| root.join("target/release/lets"), PathBuf::from);
    if !bin.is_file() {
        eprintln!(
            "wall_clock: no binary at {}: run `cargo build --release --locked` first",
            bin.display()
        );
        return ExitCode::FAILURE;
    }
    let corpus = root.join("tests/fixtures/corpus");
    if !has_full_manifest(&corpus) {
        eprintln!(
            "wall_clock: {} is not the generated `full` corpus: run `cargo run --manifest-path \
             tests/corpusgen/Cargo.toml --release` first",
            corpus.display()
        );
        return ExitCode::FAILURE;
    }

    // Relative and memory ceilings compare within one runner or are absolute, so they never widen.
    let margin = if env::var_os("CI").is_some() {
        gates::CI_MARGIN
    } else {
        1
    };
    println!("lets under test: {}", bin.display());
    println!(
        "latency gates: {}",
        if margin == 1 {
            "tight (local)".to_owned()
        } else {
            format!("widened {margin}x (CI is set)")
        }
    );
    let rows = [
        guide(&bin),
        hook_classify(&bin, &corpus),
        show_small(&bin, &corpus),
        show_symbol(&bin, &corpus),
        find(&bin, &corpus),
        edit(&bin, &corpus),
        edit_batch(&bin, &corpus),
        transform_set(&bin, &corpus),
    ];

    let mut table = format!(
        "{:<44} {:>9} {:>9} {:>11}  {:<6} {}\n",
        "workload", "p50 ms", "p99 ms", "gate ms", "result", "relative"
    );
    let mut all_pass = true;
    for row in &rows {
        all_pass &= row.write(&mut table, margin);
    }
    all_pass &= binary_size(&bin).write(&mut table);
    all_pass &= edit_large_peak_rss(&bin, &corpus).write(&mut table);
    all_pass &= transform_json_peak_rss(&bin).write(&mut table);
    print!("{table}");
    if all_pass {
        ExitCode::SUCCESS
    } else {
        println!("wall_clock: at least one workload is over its gate");
        ExitCode::FAILURE
    }
}

fn has_full_manifest(corpus: &Path) -> bool {
    fs::read_to_string(corpus.join(".manifest"))
        .is_ok_and(|text| text.lines().any(|line| line == "profile=full"))
}

struct Row {
    workload: String,
    p50: Duration,
    p99: Duration,
    gate: Gate,
    relative: Option<Relative>,
}

enum Relative {
    Measured {
        tool: &'static str,
        ratio: f64,
        max: f64,
    },
    Skipped {
        tool: &'static str,
    },
}

impl Row {
    fn write(&self, table: &mut String, margin: u64) -> bool {
        let gate = self.gate.widened(margin);
        let absolute_ok = self.p50 <= Duration::from_millis(gate.p50_ms)
            && self.p99 <= Duration::from_millis(gate.p99_ms);
        let (relative_ok, relative) = match &self.relative {
            None => (true, String::new()),
            Some(Relative::Measured { tool, ratio, max }) => {
                let ok = ratio <= max;
                let verdict = if ok { "pass" } else { "FAIL" };
                // Three places, so a 2.004 over a 2.0 limit never prints as a failing "2.00x".
                (ok, format!("{ratio:.3}x {tool} (max {max:.1}x) {verdict}"))
            },
            Some(Relative::Skipped { tool }) => (true, format!("skipped ({tool} absent)")),
        };
        let result = if absolute_ok { "pass" } else { "FAIL" };
        let _ = writeln!(
            table,
            "{:<44} {:>9.3} {:>9.3} {:>11}  {:<6} {}",
            self.workload,
            millis(self.p50),
            millis(self.p99),
            format!("{}/{}", gate.p50_ms, gate.p99_ms),
            result,
            relative,
        );
        absolute_ok && relative_ok
    }
}

struct Ceiling {
    what: String,
    measured: Result<u64, &'static str>,
    max: u64,
}

impl Ceiling {
    fn write(&self, table: &mut String) -> bool {
        let (ok, line) = match self.measured {
            Ok(bytes) => {
                let ok = bytes < self.max;
                let verdict = if ok { "pass" } else { "FAIL" };
                (
                    ok,
                    format!("{bytes} bytes against the {} ceiling {verdict}", self.max),
                )
            },
            Err(why) => (true, format!("skipped ({why})")),
        };
        let _ = writeln!(table, "{:<44} {line}", self.what);
        ok
    }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Integer index math: no `usize` <-> `f64` cast for clippy to flag.
fn percentile(sorted: &[Duration], numerator: usize, denominator: usize) -> Duration {
    let idx = ((sorted.len() - 1) * numerator + denominator / 2) / denominator;
    sorted[idx]
}

fn p50_p99(mut durations: Vec<Duration>) -> (Duration, Duration) {
    durations.sort_unstable();
    (
        percentile(&durations, 50, 100),
        percentile(&durations, 99, 100),
    )
}

fn sample(one: impl Fn() -> Duration) -> (Duration, Duration) {
    p50_p99((0..RUNS).map(|_| one()).collect())
}

/// No inherited `RIPGREP_CONFIG_PATH`, git config or global gitignore may skew either side.
/// `CLAUDE_CONFIG_DIR`/`CLAUDE_PROJECT_DIR` point at the same fresh, settings-free `home` so
/// `hook classify`'s permission read never sees the real runner's `~/.claude/settings.json`.
fn scrubbed(program: &Path, cwd: &Path, home: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir(cwd)
        .env_clear()
        .env("HOME", home)
        .env("CLAUDE_CONFIG_DIR", home)
        .env("CLAUDE_PROJECT_DIR", home)
        .env("XDG_RUNTIME_DIR", home)
        .env("XDG_CONFIG_HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("LETS_NO_STATS", "1")
        .env("LETS_TOKEN_RATIO", "4");
    command
}

fn timed(mut command: Command, stdin: Option<&[u8]>) -> (Duration, Output) {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    command.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let start = Instant::now();
    let mut child = command.spawn().expect("the workload binary spawns");
    if let Some(bytes) = stdin {
        let mut pipe = child.stdin.take().expect("a piped stdin handle");
        pipe.write_all(bytes).expect("the child accepts its stdin");
    }
    let output = child.wait_with_output().expect("the child exits");
    let elapsed = start.elapsed();
    assert!(
        output.status.success(),
        "{command:?} exited {:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    (elapsed, output)
}

/// The untimed first run proves the output and warms the page cache, so p99 is never a cold read.
fn readonly<'a>(
    program: &'a Path,
    args: &'a [&'a str],
    cwd: &'a Path,
    stdin: Option<&'a [u8]>,
    proves: impl Fn(&Output) -> bool,
) -> impl Fn() -> Duration + 'a {
    let run = move || {
        let home = TempDir::new().expect("a temp HOME");
        let mut command = scrubbed(program, cwd, home.path());
        command.args(args);
        timed(command, stdin)
    };
    let (_, first) = run();
    assert!(
        proves(&first),
        "{} {args:?} did not produce the output this workload measures:\n{}",
        program.display(),
        String::from_utf8_lossy(&first.stdout)
    );
    move || run().0
}

fn stdout_has(needle: &'static str) -> impl Fn(&Output) -> bool {
    move |output| String::from_utf8_lossy(&output.stdout).contains(needle)
}

fn on_path(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Alternates spawn by spawn: sampled as two blocks, drift between them landed in the ratio.
fn paired(
    lets: impl Fn() -> Duration,
    tool: &'static str,
    max: f64,
    reference: &Reference,
) -> (Duration, Duration, Relative) {
    let Some(program) = on_path(tool) else {
        let (p50, p99) = sample(lets);
        return (p50, p99, Relative::Skipped { tool });
    };
    let theirs = readonly(
        &program,
        reference.args,
        reference.cwd,
        reference.stdin,
        reference.proves,
    );
    let (mut ours, mut refs) = (Vec::with_capacity(RUNS), Vec::with_capacity(RUNS));
    for _ in 0..RUNS {
        ours.push(lets());
        refs.push(theirs());
    }
    let (p50, p99) = p50_p99(ours);
    let (ref_p50, _) = p50_p99(refs);
    let ratio = p50.as_secs_f64() / ref_p50.as_secs_f64();
    (p50, p99, Relative::Measured { tool, ratio, max })
}

struct Reference<'a> {
    args: &'a [&'a str],
    cwd: &'a Path,
    stdin: Option<&'a [u8]>,
    proves: fn(&Output) -> bool,
}

fn guide(bin: &Path) -> Row {
    let home = TempDir::new().expect("a temp cwd");
    let (p50, p99) = sample(readonly(bin, &["guide"], home.path(), None, |output| {
        !output.stdout.is_empty()
    }));
    Row {
        workload: "guide".into(),
        p50,
        p99,
        gate: gates::GUIDE,
        relative: None,
    }
}

/// A command the classifier blocks, so the timed path is the full bash parse, not an early allow.
/// `sed -i` with a `g` flag, not `cat`: chained whole-file reads now rewrite instead of blocking,
/// and unlike a search, `sed -i` denies without needing the named files to exist.
fn blocked_command() -> String {
    const LINK: &str = " && sed -i 's/x/x/g' src/module_00.ts";
    const TAIL: &str = " && sed -i 's/x/x/g' src/.ts";
    let mut command = String::from("sed -i 's/x/x/g' src/a.ts");
    let mut index = 0;
    while command.len() + LINK.len() + TAIL.len() < HOOK_COMMAND_BYTES {
        let _ = write!(command, " && sed -i 's/x/x/g' src/module_{index:02}.ts");
        index += 1;
    }
    let pad = HOOK_COMMAND_BYTES - command.len() - TAIL.len();
    let _ = write!(command, " && sed -i 's/x/x/g' src/{}.ts", "x".repeat(pad));
    assert_eq!(command.len(), HOOK_COMMAND_BYTES);
    command
}

/// A real absolute `cwd`: a missing or relative one allows before the grammar loads.
fn hook_classify(bin: &Path, corpus: &Path) -> Row {
    let command = blocked_command();
    let event = serde_json::json!({
        "session_id": "wall-clock-bench",
        "cwd": corpus.to_str().expect("a UTF-8 corpus path"),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": { "command": command },
    })
    .to_string();
    let lets = readonly(
        bin,
        &["hook", "classify"],
        corpus,
        Some(event.as_bytes()),
        stdout_has("\"permissionDecision\":\"deny\""),
    );
    let stdin = format!("{command}\n");
    let (p50, p99, relative) = paired(lets, "bash", gates::HOOK_VS_BASH_N_MAX_RATIO, &Reference {
        args: &["-n", "-s"],
        cwd: corpus,
        stdin: Some(stdin.as_bytes()),
        proves: |_| true,
    });
    Row {
        workload: format!("hook classify, {HOOK_COMMAND_BYTES}-byte command"),
        p50,
        p99,
        gate: gates::HOOK_CLASSIFY,
        relative: Some(relative),
    }
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(&next).expect("a readable corpus directory") {
            let path = entry.expect("a readable corpus entry").path();
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

fn line_count(path: &Path) -> usize {
    let text = fs::read_to_string(path).expect("a UTF-8 corpus file");
    text.lines().count()
}

/// Searched, not named: corpusgen draws filler sizes from a byte range.
fn nearest_lines(dir: &Path, target: usize, ceiling: usize) -> PathBuf {
    files_under(dir)
        .into_iter()
        .map(|path| (line_count(&path), path))
        .filter(|(lines, _)| *lines <= ceiling)
        .min_by_key(|(lines, path)| (lines.abs_diff(target), path.clone()))
        .map_or_else(
            || panic!("no file under {}", dir.display()),
            |(_, path)| path,
        )
}

fn relative_to<'a>(path: &'a Path, root: &Path) -> &'a str {
    path.strip_prefix(root)
        .expect("a corpus file sits under the corpus root")
        .to_str()
        .expect("a UTF-8 corpus path")
}

fn small_typescript(corpus: &Path) -> PathBuf {
    nearest_lines(
        &corpus.join("typescript"),
        SMALL_FILE_LINES,
        SMALL_FILE_LINES,
    )
}

fn show_small(bin: &Path, corpus: &Path) -> Row {
    let file = small_typescript(corpus);
    let target = relative_to(&file, corpus);
    let args = ["show", target];
    let lets = readonly(bin, &args, corpus, None, stdout_has(EDIT_OLD));
    let (p50, p99, relative) = paired(lets, "cat", gates::SHOW_VS_CAT_MAX_RATIO, &Reference {
        args: &args[1..],
        cwd: corpus,
        stdin: None,
        proves: |output| String::from_utf8_lossy(&output.stdout).contains(EDIT_OLD),
    });
    Row {
        workload: format!("show, {} lines", line_count(&file)),
        p50,
        p99,
        gate: gates::SHOW_SMALL,
        relative: Some(relative),
    }
}

fn show_symbol(bin: &Path, corpus: &Path) -> Row {
    let target = format!("specials/search-target.ts#{SEARCH_TARGET}");
    let (p50, p99) = sample(readonly(
        bin,
        &["show", &target],
        corpus,
        None,
        stdout_has("export function corpusSearchTarget"),
    ));
    Row {
        workload: format!(
            "show path#symbol, {} lines",
            line_count(&corpus.join("specials/search-target.ts"))
        ),
        p50,
        p99,
        gate: gates::SHOW_SYMBOL,
        relative: None,
    }
}

/// `--no-ignore`: the corpus root's `*` .gitignore would hide every file from both tools.
fn find(bin: &Path, corpus: &Path) -> Row {
    let lets = readonly(
        bin,
        &["find", "-F", "--no-ignore", SEARCH_TARGET, "."],
        corpus,
        None,
        stdout_has("1 hit in 1 file"),
    );
    let (p50, p99, relative) = paired(lets, "rg", gates::FIND_VS_RG_MAX_RATIO, &Reference {
        // Default threads on both: `find` walks with the same parallel walker `rg` uses.
        args: &["-F", "--no-ignore", SEARCH_TARGET, "."],
        cwd: corpus,
        stdin: None,
        proves: |output| String::from_utf8_lossy(&output.stdout).contains("search-target.ts"),
    });
    Row {
        workload: "find, literal, 1 hit, whole corpus".into(),
        p50,
        p99,
        gate: gates::FIND,
        relative: Some(relative),
    }
}

/// Outside any git repo, so the scope guard falls back to the cwd, `tree/`.
struct Scratch {
    _dir: TempDir,
    home: PathBuf,
    tree: PathBuf,
}

impl Scratch {
    fn empty() -> Scratch {
        let dir = TempDir::new().expect("a scratch dir");
        let home = dir.path().join("home");
        let tree = dir.path().join("tree");
        fs::create_dir(&home).expect("a scratch HOME");
        fs::create_dir(&tree).expect("a scratch tree");
        Scratch {
            _dir: dir,
            home,
            tree,
        }
    }

    fn with_copies(sources: &[(&Path, String)]) -> Scratch {
        let scratch = Scratch::empty();
        for (source, name) in sources {
            fs::copy(source, scratch.tree.join(name)).expect("a copied corpus file");
        }
        scratch
    }

    fn with_content(name: &str, content: &[u8]) -> Scratch {
        let scratch = Scratch::empty();
        fs::write(scratch.tree.join(name), content).expect("a written fixture file");
        scratch
    }

    fn lets(&self, bin: &Path, args: &[&str]) -> Command {
        let mut command = scrubbed(bin, &self.tree, &self.home);
        command.args(args);
        command
    }
}

fn assert_once(path: &Path, needle: &str) {
    let text = fs::read_to_string(path).expect("a UTF-8 corpus file");
    assert_eq!(
        text.matches(needle).count(),
        1,
        "{} must hold `{needle}` exactly once for the edit to be unambiguous",
        path.display()
    );
}

fn edit(bin: &Path, corpus: &Path) -> Row {
    let file = small_typescript(corpus);
    assert_once(&file, EDIT_OLD);
    let copies = [(file.as_path(), "target.ts".to_string())];
    let args = ["edit", "target.ts", "--old", EDIT_OLD, "--new", EDIT_NEW];
    let run = || {
        let scratch = Scratch::with_copies(&copies);
        timed(scratch.lets(bin, &args), None)
    };
    let (_, first) = run();
    assert!(
        stdout_has("check: structure ok")(&first),
        "edit did not run its structural check:\n{}",
        String::from_utf8_lossy(&first.stdout)
    );
    let (p50, p99) = sample(|| run().0);
    Row {
        workload: format!("edit + structural check, {} lines", line_count(&file)),
        p50,
        p99,
        gate: gates::EDIT,
        relative: None,
    }
}

fn edit_batch(bin: &Path, corpus: &Path) -> Row {
    let sources: Vec<PathBuf> = files_under(&corpus.join("typescript"))
        .into_iter()
        .take(BATCH_FILES)
        .collect();
    assert_eq!(
        sources.len(),
        BATCH_FILES,
        "the corpus has too few TypeScript files"
    );
    let copies: Vec<(&Path, String)> = sources
        .iter()
        .enumerate()
        .map(|(index, source)| {
            assert_once(source, EDIT_OLD);
            (source.as_path(), format!("batch_{index}.ts"))
        })
        .collect();
    let mut spec = String::new();
    for (_, name) in &copies {
        let line = serde_json::json!({ "file": name, "old": EDIT_OLD, "new": EDIT_NEW });
        let _ = writeln!(spec, "{line}");
    }
    let run = || {
        let scratch = Scratch::with_copies(&copies);
        timed(
            scratch.lets(bin, &["edit", "--from", "-"]),
            Some(spec.as_bytes()),
        )
    };
    let (_, first) = run();
    let footer = format!(
        "{BATCH_FILES} files · {BATCH_FILES} edits · all applied · checks: structure ok \
         ×{BATCH_FILES}"
    );
    assert!(
        String::from_utf8_lossy(&first.stdout).contains(&footer),
        "the batch did not edit and check every file:\n{}",
        String::from_utf8_lossy(&first.stdout)
    );
    let (p50, p99) = sample(|| run().0);
    Row {
        workload: format!("edit --from -, {BATCH_FILES} files"),
        p50,
        p99,
        gate: gates::EDIT_BATCH_10,
        relative: None,
    }
}

fn transform_set(bin: &Path, corpus: &Path) -> Row {
    let file = nearest_lines(&corpus.join("yaml"), YAML_FILE_LINES, usize::MAX);
    assert_once(&file, "\nunit_1:\n");
    let copies = [(file.as_path(), "target.yaml".to_string())];
    let args = ["transform", "target.yaml", "--set", "unit_1.value=2"];
    let run = || {
        let scratch = Scratch::with_copies(&copies);
        timed(scratch.lets(bin, &args), None)
    };
    let (_, first) = run();
    assert!(
        stdout_has("check: yaml ok")(&first),
        "transform did not validate its output:\n{}",
        String::from_utf8_lossy(&first.stdout)
    );
    let (p50, p99) = sample(|| run().0);
    Row {
        workload: format!("transform --set, {} lines of YAML", line_count(&file)),
        p50,
        p99,
        gate: gates::TRANSFORM_SET,
        relative: None,
    }
}

fn binary_size(bin: &Path) -> Ceiling {
    Ceiling {
        what: "binary size".into(),
        measured: Ok(fs::metadata(bin).expect("the binary under test").len()),
        max: gates::BINARY_MAX_BYTES,
    }
}

/// GNU time's `%M` is the child's max RSS in KiB, C `malloc` included; std has no `getrusage`.
fn edit_large_peak_rss(bin: &Path, corpus: &Path) -> Ceiling {
    let what = "edit, 8 MiB markdown, peak RSS".to_owned();
    let max = gates::EDIT_8MIB_PEAK_RSS_BYTES;
    let Some(time) = on_path("time").filter(|time| is_gnu_time(time)) else {
        return Ceiling {
            what,
            measured: Err("GNU time absent"),
            max,
        };
    };
    let source = corpus.join("specials/large.md");
    assert_once(&source, LARGE_OLD);
    let scratch = Scratch::with_copies(&[(source.as_path(), "large.md".to_owned())]);
    let report = scratch.home.join("maxrss");
    let output = scrubbed(&time, &scratch.tree, &scratch.home)
        .args(["-f", "%M", "-o"])
        .arg(&report)
        .arg(bin)
        .args(["edit", "large.md", "--old", LARGE_OLD, "--new", LARGE_NEW])
        .output()
        .expect("GNU time spawns");
    assert!(
        output.status.success(),
        "the 8 MiB edit exited {:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_once(&scratch.tree.join("large.md"), LARGE_NEW);
    let kib: u64 = fs::read_to_string(&report)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .expect("GNU time wrote the child's max RSS");
    Ceiling {
        what,
        measured: Ok(kib * 1024),
        max,
    }
}

/// Under the 8 MiB `--max-file-bytes` default by enough that the last unit never crosses it.
const TRANSFORM_JSON_TARGET_BYTES: usize = 8 * 1024 * 1024 - 4096;

fn json_fixture() -> (Vec<u8>, String) {
    let mut out = String::with_capacity(TRANSFORM_JSON_TARGET_BYTES + 64);
    out.push_str("{\"units\": [\n");
    let mut count = 0usize;
    while out.len() < TRANSFORM_JSON_TARGET_BYTES {
        if count > 0 {
            out.push_str(",\n");
        }
        let _ = write!(out, "  {{\"index\": {count}, \"value\": \"unit-{count}\"}}");
        count += 1;
    }
    out.push_str("\n]}\n");
    (out.into_bytes(), format!("units[{}].value=late", count - 2))
}

fn transform_json_peak_rss(bin: &Path) -> Ceiling {
    let what = "transform, 8 MiB JSON, peak RSS".to_owned();
    let max = gates::TRANSFORM_8MIB_JSON_PEAK_RSS_BYTES;
    let Some(time) = on_path("time").filter(|time| is_gnu_time(time)) else {
        return Ceiling {
            what,
            measured: Err("GNU time absent"),
            max,
        };
    };
    let (fixture, set) = json_fixture();
    let scratch = Scratch::with_content("units.json", &fixture);
    let report = scratch.home.join("maxrss");
    let output = scrubbed(&time, &scratch.tree, &scratch.home)
        .args(["-f", "%M", "-o"])
        .arg(&report)
        .arg(bin)
        .args(["transform", "units.json", "--set", &set])
        .output()
        .expect("GNU time spawns");
    assert!(
        output.status.success(),
        "the 8 MiB transform exited {:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_to_string(scratch.tree.join("units.json"))
            .expect("read the written JSON fixture")
            .contains("\"value\": \"late\""),
        "transform did not set the target value"
    );
    let kib: u64 = fs::read_to_string(&report)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .expect("GNU time wrote the child's max RSS");
    Ceiling {
        what,
        measured: Ok(kib * 1024),
        max,
    }
}

/// BSD time rejects `-f`, and a time that writes no number cannot be read either.
fn is_gnu_time(time: &Path) -> bool {
    let dir = TempDir::new().expect("a temp dir");
    let report = dir.path().join("maxrss");
    Command::new(time)
        .args(["-f", "%M", "-o"])
        .arg(&report)
        .arg("true")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
        && fs::read_to_string(&report).is_ok_and(|text| text.trim().parse::<u64>().is_ok())
}
