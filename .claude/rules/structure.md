---
paths:
  - src/**
  - tests/**
  - bench/**
  - docs/**
  - site/**
---
# Layout (`src/`, `tests/`, `bench/`, `docs/`)

The module layout below is decided. A new top-level module or directory is a proposal in the
pull request with its consumer named, not a file.

## Always
- Keep one file per verb under `src/verbs/`. A verb parses nothing itself: `target.rs` resolves
  targets, `window.rs` bounds, `output.rs` renders.
- Keep the exit-code mapping in `src/main.rs` only; `lib.rs` exposes the verbs and the `Error`
  enum.
- Put language-specific tree-sitter queries and heuristics under `src/symbols/`, one file per
  language; `check.rs` and `hook/` reuse them.
- Keep `docs/guide.md` as the one source of `lets guide` (`include_str!`). The Claude Code
  paragraph and the SubagentStart line are excerpts kept in `docs/agents.md`.
- Treat these as generated and regenerate them with the named recipe: `docs/examples/`
  (`just docs`), `bench/baselines/*` (`just bench-baseline`), `tests/scenarios/*/expected/*` and
  trycmd snapshots (the overwrite env vars), `site/dist/` (`just site`). CI diffs each of them;
  none is committed.
- Name fixtures, cases and scenarios with stable ids in the filename; `required.txt` lists the
  ones that may not vanish.
- Treat `site/` as the project website: an Astro static site built with Bun, deployed by
  `.github/workflows/site.yml` to GitHub Pages at lets.mayberuk.com.
- Keep `site/src/content/docs/*.md` as the one long-form CLI reference; `tests/site_docs.rs`
  fails when a subcommand, flag, exit code or error slug drifts from it.

## Never
- Hand-edit a generated file.
- A `src/util.rs`, `src/common.rs`, or a `mod.rs` that holds logic.
- A second, *unchecked* place where the guide text, the crate table, the gate table or the exit
  codes are written out — `site/src/content/docs/*.md` holds the CLI reference and stays in sync
  because `tests/site_docs.rs` enforces it.
- A second crate in the workspace before a second binary needs the library.

```text
✅ DO    src/verbs/find.rs   uses target::resolve, window::bound, output::Footer
❌ DON'T src/verbs/find.rs   re-implements gitignore walking and prints its own footer
```

Why: an agent working without this file invents structure unless the structure is already
written down.
