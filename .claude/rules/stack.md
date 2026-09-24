# Stack

One language, one toolchain, sixteen crates plus the grammar set. Each row was checked against
crates.io; a version here is a target to verify with ctx7, not a fact.

| Job | Use | Not |
|---|---|---|
| CLI parsing | `clap` (derive); `--json` etc. as global flags | `argh`, hand-rolled |
| File walking, `.gitignore` | `ignore` (ripgrep's walker) | `walkdir` plus own ignore logic |
| Line search | `grep-searcher` + `grep-regex` | an own line splitter |
| Regex | `regex` (linear-time) | `fancy-regex`, PCRE |
| Parsing, symbols, check, hook | `tree-sitter` 0.27 + `tree-sitter-language`; grammar crates used directly | vendored grammar sources (spike fallback only) |
| TOML edits | `toml_edit` | `toml` |
| JSON edits | `jsonc-parser` with the `cst` feature | `serde_json::Value` round-trip (loses comments and order) |
| YAML edits, frontmatter | `yamlpatch` + `yamlpath`; frontmatter is yamlpatch between the fences | `serde_yaml`, a frontmatter crate |
| Content hash | `blake3`, 12 hex behind the `sha:` label | `sha2` |
| Atomic write | `tempfile::NamedTempFile::persist` | write in place |
| `--json` | `serde` + `serde_json`, derived on the output model | a second output struct |
| Errors | `thiserror` | `anyhow` |
| Tests | `trycmd`, `proptest`, `insta`, `cargo-nextest` | — |
| Bench | `divan` with `AllocProfiler` | `criterion` |
| Release | `dist` on native runners per OS and arch; musl on Linux | cross-compilation |

Not taken, and why: `anyhow` (closed error set), any colour crate (the contract has no colour),
`similar` (edits render as marked lines, not diffs), `rayon` (the corpus median file is 87 lines;
parallel search is a v2 measurement), `unicode-normalization` (the normalise table is explicit).

## Always
- Ground library code with `npx ctx7@latest library` then `docs` before writing it.
- Commit `Cargo.lock`. Bump a version in its own commit.
- Keep `scripts/deps-gate.sh` green: 17 direct dependencies, each crate one entry, every
  `tree-sitter-*` grammar collapsed to one. The markdown grammar's `parser` feature stays off.
- Keep `cargo-deny` green: MIT/Apache/BSD licences only, advisories, `multiple-versions = "deny"`.

## Never
- Add a crate this table does not list. An unlisted dependency is a proposal in the pull
  request, not an install.
- Install a second crate for a job that already has a row.
- Enable a grammar crate feature that pulls a second `tree-sitter` runtime.

```toml
# ✅ DO — Cargo.toml [dependencies]
toml_edit = "0.25"

# ❌ DON'T — a second TOML crate because it looked simpler
toml = "0.9"
```

Why: sixteen crates is a number one person can hold in their head, and the gate makes drift
impossible rather than discouraged.
