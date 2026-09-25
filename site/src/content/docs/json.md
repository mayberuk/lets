---
title: --json and --jsonl
description: The structured output shape every reporting verb carries — one object for --json, one object per item plus a trailing stats record for --jsonl.
order: 22
group: Reference
commands: []
---

# `--json` and `--jsonl`

Every verb that reports on files takes `--json` (one object on stdout) or `--jsonl` (one object
per target, hit, file, count row or edit, followed by one trailing object carrying `stats` and
`omitted`). The same information renders as text, `--json` or `--jsonl` — a field exists in all
three or in none. Shapes below are copied from real `lets` 0.0.1 output (`LETS_NO_STATS=1`, which
is why `tokens_est` reads `null`).

## `show --json`

```json
{"targets":[{"target":"usage.ts","path":"usage.ts","start":1,"end":9,"total":9,
  "sha":"14dc685cb937","lines":[{"number":1,"marker":"none","text":"import { usageCap } from './config'"}]}],
 "omitted":[],"stats":{"lines":9,"bytes":189,"tokens_est":null}}
```

A `#symbol` target adds a `"resolver"` key (`"tree-sitter"` or `"via heuristic (plaintext)"`
text, depending on the verb). `--jsonl` prints one such object per target, no `targets` wrapper,
then `{"stats":{...},"omitted":[]}`.

## `find --json`

```json
{"targets":[{"target":"usage.ts","path":"usage.ts",
  "lines":[{"number":1,"marker":"hit","text":"import { «usageCap» } from './config'"}]}],
 "omitted":[],"stats":{"lines":1,"bytes":40,"tokens_est":null}}
```

`--files` is `{"files":["usage.ts"],"omitted":[],"stats":{...}}`. `--count` is
`{"counts":[{"count":1,"path":"usage.ts"}],"omitted":[],"stats":{...}}`. Over the cap, `targets`
is empty and the top-files map appears under its own key, named in `omitted` too:

```json
{"targets":[],
 "omitted":[{"hit_cap":{"hits":60,"cap":50}},{"top_files":{"shown":1}}],
 "stats":{"lines":0,"bytes":0,"tokens_est":null},
 "top_files":[{"count":60,"path":"many.txt"}]}
```

## `edit --json`, one file

```json
{"path":"usage.ts","replacements":1,"lines":[5],"match":"exact",
 "region":{"start":3,"end":7,"lines":[
   {"number":3,"marker":"none","text":"export function usage(id: string) {"},
   {"number":5,"marker":"replaced","text":"  const cap = 20"}]},
 "check":{"layer":"structure","status":"ok","errors_before":0,"errors_after":0},
 "sha":{"before":"14dc685cb937","after":"9d77b7d8a9f6"},
 "omitted":[],"stats":{"lines":5,"bytes":178,"tokens_est":null}}
```

An insert carries `"inserted"` and `"anchor"` in place of `"replacements"`. A line's `"marker"`
is `"none"`, `"replaced"`, or (for an insert) `"inserted"`. `--jsonl` on a multi-file batch prints
one such object per file, then the trailing `{"stats":...,"omitted":[]}` record.

## `transform --json`

```json
{"path":"config.json","format":"json","operations":[{"op":"set","key":"review.threads"}],
 "lines":[6],
 "region":{"start":4,"end":8,"lines":[{"number":6,"marker":"replaced","text":"    \"threads\": 3"}]},
 "check":{"layer":"json","status":"ok","errors_before":0,"errors_after":0},
 "sha":{"before":"5e9ec954d099","after":"13adcd49eab3"},
 "omitted":[],"stats":{"lines":5,"bytes":42,"tokens_est":null}}
```

## `write --json`

```json
{"path":"new.txt","outcome":"created","lines":1,"bytes":6,"sha":"8e4c7c1b99db",
 "omitted":[{"check_skipped":{"reason":"no grammar for .txt"}}],
 "stats":{"lines":1,"bytes":6,"tokens_est":null}}
```

`"outcome"` is `"created"`, `"overwritten"`, or (on a failed call that still had something to
report, such as a refused overwrite) `"exists"`.

## `guide --json` and `version --json`

These two carry their text alone, with no `omitted`/`stats` — there is nothing to omit and no
file-derived cost to report:

```json
{"guide":"lets — Locate · Edit · Transform · Show ..."}
{"version":"0.0.1"}
```

## The `omitted` array

The machine-readable half of the footer: a unit omission is a bare string (e.g. `"normalized"`),
one with detail is a single-key object. `[]` means nothing was left out. Observed shapes:

| Shape | Means |
|---|---|
| `{"hit_cap":{"hits":60,"cap":50}}` | `find` was over its cap |
| `{"top_files":{"shown":10}}` | the over-cap map showed this many files |
| `{"check_skipped":{"reason":"no grammar for .txt"}}` | no checker ran for this file type |
| `"normalized"` | a `--normalize` match was used |
| `{"partial_batch":{"written":["src/a.ts"]}}` | some files of a batch landed before a failure |

## The error shape

A call that fails with no other stdout to report — every target missing, a batch that never got
to write anything — still produces one JSON object, so a `--json` caller never has to fall back to
parsing stderr:

```json
{"error":{"slug":"not_found","message":"--old not found in usage.ts\n  nearest: line 5\t  const cap = 20"},
 "omitted":[],"stats":{"lines":0,"bytes":0,"tokens_est":null}}
```

`error.slug` is the same slug `ERROR_CODE=` prints on stderr — see [exit codes](/docs/exit-codes/)
for the full table. A call that produces some real output alongside a partial failure keeps its
normal shape instead of this one.

## `stats.tokens_est`

Bytes ÷ 4, always an estimate. `LETS_TOKEN_RATIO` overrides the divisor; `LETS_NO_STATS=1` makes
it `null` instead, for a caller diffing two runs that doesn't want the estimate to be part of the
diff.
