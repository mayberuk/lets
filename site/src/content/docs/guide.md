---
title: lets guide
description: The one-screen cheat sheet lets guide prints — verbs, targets and exit codes, meant to be read once per session.
order: 12
group: Setup
commands: [guide]
---

# `lets guide`

```
lets guide
```

Prints one screen: every verb, the target grammar, and the exit-code shorthand, so an agent (or a
person) reads it once and has the whole surface. This is the same text a `SessionStart` hook
installed by `lets hooks install claude-code` shows at the start of every session — see
`/docs/hooks/`.

```console
$ lets guide
lets — Locate · Edit · Transform · Show          one call, bounded output, post-state returned

  show   <target>...            read files, ranges, anchors, symbols — several per call
  find   <pattern> [path]...    search; hits print as path:line; capped at 50, says so
                                -F/-i/-w · -A/-B/-C context · --files/-l · --count/-c
                                grep's -n/--line-number -r -R -E -H are accepted as no-ops
  edit   <target> --old --new   exact-once replace; --all; --insert-after; --from - for batches
  transform <file> --set k=v    --append k=v; JSON/YAML/TOML/frontmatter keys, formatting preserved
  write  <path> < stdin         create a file; refuses to overwrite without --force

  targets   f.ts   f.ts:40-80   "f.ts@'regex'" -A 20   f.ts#funcName   f.md#'Heading'
  exits     0 done · 1 none/over cap · 2 ambiguous · 3 check failed (reverted) · 4 over budget
            5 changed since --if · 6 outside tree · 7 unsupported file · 8 batch partly written
  after an edit the changed region is in the output — do not cat or sed -n to check it
```

`lets guide --json` carries the same text under one key, with no `omitted`/`stats` — there is
nothing to omit and no file-derived cost to report: `{"guide":"lets — Locate · Edit · Transform ·
Show ..."}`. See `/docs/json/`.
