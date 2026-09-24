# Testing

A test that passes against wrong behaviour is not coverage. The real entry point is the binary,
and `CONTRIBUTING.md`'s Testing section is the harness inventory, the sandbox contract and the
commands.

| Tier | Where | Runs |
|---|---|---|
| Unit + property | `#[cfg(test)]` in `src/**`; `proptest` on `matcher`, `normalize`, `target`, `window` | every change |
| Literate cases | `tests/cmd/*.trycmd` and `.toml`, fixture tree copied per case | every change; generates `docs/examples.md` |
| Scenario replay | `tests/scenarios/<name>/script.sh` against `expected/`, byte-identical | every change |
| Hook corpus | `tests/hook/` command → verdict | every change |
| Bytes | `tests/bytes.rs`: CRLF, BOM, scalar boundaries, mode bits | every change |
| JSON snapshots | `tests/json_snapshots.rs` (`insta`) | every change |
| Bench gate | `just bench-gate` | before any performance claim |
| Agent smoke | `just smoke-agent` (real `claude -p`, two arms) | on demand, never CI |

## Always
- Derive the expected value from the spec or the requirement. Never run the new code and paste
  what it returned.
- Give every criterion a negative control: a case that must fail (exit 1, 2, 3, 5, 6, 7 or 8)
  beside the one that passes. A checker fixture that must revert is as required as the one that
  passes.
- Run inside the sandbox: a fresh copy of the fixture tree per case, `HOME` and the lock dir
  overridden, `LETS_NO_STATS=1` and `LETS_TOKEN_RATIO` pinned. Nothing touches the real HOME,
  `~/.claude`, or the repo's own files.
- List every scenario and trycmd case that may not vanish in `required.txt`. A case whose
  checker is absent is `skipped` by name, never silently green.
- Regenerate goldens only deliberately (`TRYCMD=overwrite`, `LETS_GOLDEN=overwrite`) and review
  the diff as a contract change.
- Assert identity, not count, when the requirement is about which thing survived.
- State a testing strategy per change: what is unit, what drives the binary, and the exact
  command that proves it.

## Never
- A test that reads or writes outside its sandbox.
- A mock of the unit under test. Fake the boundary (a scripted `--check` command, a fixture
  checker on `PATH`), not the matcher.
- Deleting a negative control because it looks redundant.
- A golden containing a timestamp, an absolute path, or a token estimate.

```rust
// ✅ DO — expected from spec § edit (exit 2, candidates shown), plus its control
#[test]
fn ambiguous_old_exits_2_and_lists_both_candidates() {
    let run = sandbox("two-caps").lets(["edit", "a.ts", "--old", "cap", "--new", "limit"]);
    assert_eq!(run.code, 2);
    assert!(run.out.contains("a.ts:40") && run.out.contains("a.ts:58"));
}
#[test]
fn unique_old_exits_0() { /* the control */ }

// ❌ DON'T — expected pasted from a run
assert_eq!(run.out, include_str!("what_it_printed_last_time.txt"));
```

Why: this tool's promise is that the output is the verification, and a suite that would pass
on wrong output cannot back that promise.
