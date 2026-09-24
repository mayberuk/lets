---
paths:
  - src/output.rs
  - src/main.rs
  - tests/**
---
# Output contract (`src/output.rs`, `src/main.rs`, `tests/`)

The footer is a contract, not a status line: if it does not name a narrowing, that narrowing did
not happen. `docs/agents.md` § The footer contract holds the full text.

## Always
- Emit every header as `── <target>  (<a>-<b> of <N>[ · window W · :x-y not shown][ · via R]) · sha:<12 hex>`
  and every footer as `── <verb summary> · <cost>`; numbered lines right-aligned to the widest
  number, a tab, a `+`/`~` marker column that is blank otherwise. The tab, not two spaces, is the
  gutter separator, not two spaces — a model reads it the way it already reads `cat -n`, and
  cannot fold it into the line's own indentation.
- Name every omission in the footer: window, budget trim, hit cap, `.gitignore` and hidden
  skips, checker skipped or inconclusive, xattrs dropped, normalised match, partial-batch files.
- Write only the answer to stdout. Diagnostics and `ERROR_CODE=<slug>` (the last line) go to
  stderr. Every non-zero exit has a slug.
- Map `Error` to an exit code in `main.rs` and nowhere else; the codes are the spec's
  (`0 1 2 3 4 5 6 7 8`).
- Render text, `--json` and `--jsonl` from the same output struct. A field exists in all three
  or in none.
- Print token estimates as `~` with bytes ÷ 4 (or `LETS_TOKEN_RATIO`); omit the cost line under
  `LETS_NO_STATS=1`.
- Keep status metadata (headers, footers, `ERROR_CODE`) outside `--budget` and `--max-bytes`.
  Cap candidate lists at 20 with `(+N more)` and a checker excerpt at its first line, 1 KiB.
- Make stdout byte-identical for identical input and file state: no timestamps, no absolute
  temp paths, no unordered iteration.
- Treat a golden or snapshot change as a contract change: regenerate deliberately, review the
  diff, and update `docs/guide.md` if the shape moved.

## Never
- ANSI colour or terminal detection. Structure is typography: `──`, `·`, `«»`.
- A narrowing applied silently, or a footer trimmed by a budget.
- A second exit-code mapping, or an exit code chosen inside a verb module.
- Output that assumes a terminal. `>`, `<` and pipes work on every verb, and the scenario tier
  proves it.

```text
✅ DO
── find 'onBack' · 4 hits in 2 files · searched 31 files · ignored 12 (gitignore 9 · hidden 3) · ~0.9k tokens

❌ DON'T
── 4 hits in 2 files            ← 12 files were skipped and nothing says so
```

Why: an agent that trusts the footer skips the follow-up read, so a footer that lies costs it
the task, not a token.
