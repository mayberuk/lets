# Performance

Startup is a feature: `lets` is called hundreds of times a session, and `hook classify` runs on
every Bash call an agent makes. Thresholds live once, in `bench/gates.rs`, each with the
measurement that set it.

## Gates (p50 / p99 in ms; tight = local `just bench-gate`; CI enforces 3×)

| Workload | p50 | p99 |
|---|---|---|
| `guide` (startup, lazy grammars) | 5 | 10 |
| `hook classify`, 200-char command | 5 | 15 |
| `show`, one file ≤ 200 lines | 10 | 25 |
| `show path#symbol`, 2,000-line TypeScript | 30 | 60 |
| `find`, literal, < 50 hits, whole corpus | 50 | 100 |
| `edit`, one replacement + structural check, 200-line file | 15 | 40 |
| `edit --from -`, 10 files | 80 | 150 |
| `transform --set`, 500-line YAML | 20 | 50 |

Relative gates, same runner, same job: `find` ≤ 2× `rg`, `show` ≤ 3× `cat`, `hook classify`
≤ 2× `bash -n`. Hard CI gate: divan `AllocProfiler` baselines (count, bytes, peak) under
`bench/baselines/`; a 10% regression fails. Stripped binary < 40 MB; peak RSS < 128 MB editing an
8 MiB file. Gates are set from measured actuals, and any gate looser than 2× actual is tightened
in its own commit.

## Always
- Read the file whole; `--max-file-bytes` caps it at 8 MiB. No streaming line reader for a file
  you will render or splice.
- Render into one buffer and write locked stdout once.
- Compile a regex once per invocation. Initialise a grammar only when a target needs it.
- Let `grep-searcher` own line splitting and binary detection in `find`.
- Walk with `ignore::WalkParallel` in `find` when the tree is large enough to pay for the threads,
  and sort before rendering so stdout stays deterministic. A bench-gate before and after still
  goes in the merge request.
- Gate the binary we ship: the musl build on Linux, not the glibc one `cargo build` makes.
- Quote `just bench-gate` numbers in any merge request that claims a performance change or
  touches `src/hook/`, `src/output.rs`, `src/matcher.rs`, `src/symbols/` or `src/verbs/find.rs`.
- Measure on the generated corpus under `tests/fixtures/`, never on a real repo.

## Never
- `unsafe`, `rayon` or any thread, a cache or an index, without a `bench-gate` before and after
  in the merge request.
- A threshold written anywhere but `bench/gates.rs`.
- A hand-edited file under `bench/baselines/`; `just bench-baseline` regenerates it and the diff
  is reviewed.
- Per-line allocation in the render path.

```rust
// ✅ DO
use std::fmt::Write as _;
let mut out = String::with_capacity(bytes.len() + lines.len() * 8);
for (n, line) in &lines { writeln!(out, "{n:>width$}  {line}").unwrap(); }
stdout().lock().write_all(out.as_bytes())?;

// ❌ DON'T
for (n, line) in &lines { println!("{}", format!("{n:>width$}  {line}")); }
```

Why: under cache-read billing a slow tool is paid on every call, and a flaky gate is a gate
someone disables.
