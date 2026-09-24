---
paths:
  - src/verbs/edit.rs
  - src/verbs/write.rs
  - src/fs.rs
  - src/lock.rs
  - src/matcher.rs
  - src/check.rs
  - src/normalize.rs
  - src/transform/**
---
# Edit safety (`edit`, `transform`, `write`, `fs`, `lock`, `matcher`, `check`, `normalize`)

A revert or partial batch that leaves a file in a state the output does not name is stop-ship.
The full rules, with their reasons, follow.

## Always
- Splice bytes: read as bytes, match on a UTF-8 view with a byte-offset map, replace only the
  matched span. CRLF, BOM, trailing newline and every untouched byte survive by construction.
- Match exact bytes by default. When exact fails and the normalise table (smart quotes, en and
  em dash, non-breaking space to ASCII; no NFKC) would match, exit 1 with
  `a normalized match exists at line N — pass --normalize`. The footer says `normalized` when
  the flag mattered.
- Refuse before mutation, exit 7 `unsupported_file`: hardlinks, non-UTF-8 in the matched
  region, a NUL in the first 8 KiB, over `--max-file-bytes`, a lock that cannot be created.
- Lock every batch target in sorted canonical-path order under
  `${XDG_RUNTIME_DIR:-$TMPDIR}/lets/locks/<blake3(canonical path)>` before validation, and hold
  through write, check and rollback. Re-verify `--if` after the lock is taken.
- Validate every edit (match, `--if`, `--expect`, pre-check) before writing any file. If a later
  rename fails, exit 8 `partial_batch` and name every file that landed.
- Write atomically: temp file beside the target, mode bits copied, persist over. Follow symlinks
  and name the resolved path in the footer.
- Join `--new` lines with the file's dominant line ending unless `--literal-newlines`. Insert
  after an existing BOM, never before it.
- Label layer-1 results in the spec's words: `json ok`, `yaml ok`, `toml ok`, `frontmatter ok`
  for validators; `structure ok`, `structure ok in edited region (N pre-existing errors
  elsewhere)`, `structure inconclusive (…)`, `skipped (<why>)`, `failed → reverted` for
  tree-sitter. A retained error node needs matching kind, range shifted by the edit delta, and
  an unchanged nearest named ancestor.
- Run `--check <cmd>` once before any write and once after the whole batch, and compare exit
  codes only. `non-zero → non-zero` and a timeout are `inconclusive`, kept.
- Require 12 or more hex for `--if sha:`. Display 12.
- Refuse a multi-line range with `--expect` alone; require `--expect-all` or `--if`.

## Never
- Claim syntax validity from a tree-sitter parse. `structure ok` means no error nodes, not
  "the language's compiler accepts this".
- Normalise by default, or let `--all` rewrite text under normalisation without the flag.
- Decode the whole file and re-encode it. Rename over a hardlink. Write outside the working tree
  without `--allow-outside` (exit 6).
- Run a project checker between the files of a batch.
- Promise write-atomicity across files. Rename is atomic per path only.

```rust
// ✅ DO — locks first, sorted; validate all; write in order; check and roll back under the same locks
let mut targets = batch.targets();
targets.sort_by(|a, b| a.canonical.cmp(&b.canonical));
let _guards = targets.iter().map(Lock::acquire).collect::<Result<Vec<_>>>()?;
let plans = batch.validate_all()?;      // nothing written yet
let landed = plans.write_in_order()?;   // exit 8 names `landed` on a later failure

// ❌ DON'T — write as you go
for edit in batch { edit.apply()?; }
```

Why: the edit's output is the agent's only verification, so a file state the output does not
describe is a lie it will act on.
