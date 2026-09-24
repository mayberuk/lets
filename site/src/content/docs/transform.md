---
title: lets transform
description: Structured edits for JSON, YAML, TOML and markdown frontmatter — a dotted path in, the changed value back, comments and order preserved.
order: 4
group: Verbs
commands: [transform]
---

# `lets transform`

Structured edits for structured files: JSON, YAML, TOML and markdown frontmatter. Parses,
mutates and re-emits, preserving comments, key order and indentation. Replaces `jq`, `yq` and
`sed` run against config files.

```
lets transform [OPTIONS] [FILE]
```

## What it does

`--set`, `--delete` and `--append` take a dotted path (`a.b[0].c`) and change one value in place
without rewriting the rest of the file. The same structural guardrail that `edit` runs — a real
parser for these four formats — runs before and after every change and reverts on failure.

## Flags

| Flag | Meaning | Default |
|---|---|---|
| `--set <path=value>` | set a key's value (repeatable) | — |
| `--delete <path>` | remove a key (repeatable) | — |
| `--append <path=value>` | append to an array, or append a table to a TOML array of tables | — |
| `--from -` | a JSONL batch on stdin (`-` is the only accepted value), one file's operations per line, validation-atomic like `edit --from -` | — |
| `--if sha:<12+ hex>` | refuse (exit 5) if the file changed since the hash was taken | — |
| `--check <cmd\|@preset>` | same layer-2 checker as `edit` — see `/docs/edit/#checks-the-guardrail` | — |
| `--check-timeout <secs>` | timeout before a layer-2 result is `inconclusive` | 60 |
| `--no-check` | skip the structured-format check | off |
| `--json`, `--jsonl`, `--budget`, `--max-bytes`, `--max-file-bytes`, `--no-ignore`, `--allow-outside`, `-q/--quiet` | shared flags | see `lets transform --help` |

## Path syntax

- Dotted keys with numeric indexes: `a.b[0].c`.
- **`--set` keeps the existing type.** When `path` already holds a string, a numeric-looking
  value is still stored as a string; when it already holds a number or bool, the value is parsed
  as JSON. A brand-new key's number is written byte-for-byte as typed (`--set v=3.10` writes
  `3.10`, not `3.1`), since a version string and a decimal look identical on the command line.
- **A quoted key is one literal key, not a path.** `--set '"editor.formatOnSave"=true'` sets the
  single key literally named `editor.formatOnSave`, distinct from the nested path
  `editor.formatOnSave`.
- **Attribute selectors** — `plugins[name=gitty].version` — select the array element whose named
  field equals the given value. `lets` resolves the selector to a plain numeric index before any
  format-specific parser sees the path, and the footer names the resolution:
  `plugins[name=gitty] → plugins[2]`. No match is exit 1, listing the values the field actually
  held; more than one match is exit 2.
- **`--append 'bin[]={"name":"b","path":"b.rs"}'`** on a TOML array of tables appends a new
  `[[bin]]` table after the last one, keeping existing formatting.

## Output shape

```
── <path> · <format> · <op summary> · line(s) <n>
<n>	<context line>
<n>~	<changed line>
<n>+	<inserted line>
<n>-	<deleted line>          (deleted)
── check: <format> ok · sha:<before>→<after>
```

## Exit codes

Same table as `edit` ([exit codes](/docs/exit-codes/)), plus this verb's own reason for `unsupported_file`:
a file that is not JSON, YAML, TOML or markdown-with-frontmatter, or a key that exists but can't
be changed in place (a tagged YAML node, or one reached through an alias).

| Exit | Slug | Means |
|---|---|---|
| 0 | — | applied |
| 1 | `not_found` | the path, or an attribute selector's value, matched nothing |
| 2 | `ambiguous` | an attribute selector matched more than one element |
| 3 | `check_failed` | the guardrail reverted the change |
| 5 | `changed` | the file changed since the `--if sha:…` given |
| 6 | `outside_tree` | the target is outside the working tree |
| 7 | `unsupported_file` | not a structured format, or an in-place-uneditable key |
| 64 | `usage` | malformed command line, e.g. an out-of-range TOML integer |

## Examples

Set two JSON values; comments and the rest of the file survive:

```console
$ lets transform config.json --set features.e2e=false --set review.threads=3
── config.json · json · set features.e2e, review.threads · lines 3, 4
1   {
2     // feature flags
3~    "features": { "e2e": false },
4~    "review": { "threads": 3 }
5   }
── check: json ok · sha:bfe3e156b1ce→01c03fe96fd3 · ~21 tokens
```

Frontmatter is a YAML edit between the fences of a markdown file:

```console
$ lets transform docs/note.md --set last_updated=2026.09.16
── docs/note.md · frontmatter · set last_updated · line 4
2 	title: Notes
3 	tags: [moc]
4~	last_updated: 2026.09.16
5 	---
6 	# Notes
── check: frontmatter ok · sha:5abff77ae5c2→60afc02190e6
```

An attribute selector that matches two elements is refused, not guessed:

```console
$ lets transform compose.yaml --set 'services[name=web].image=c'
? 2
services[name=web].image is ambiguous (2 candidates)
  compose.yaml:2	services[0].name="web"
  compose.yaml:4	services[1].name="web"
ERROR_CODE=ambiguous
```

Append a TOML array-of-tables entry, formatting preserved:

```console
$ lets transform Cargo.toml --append 'bin[]={"name":"b","path":"src/b.rs"}'
── Cargo.toml · toml · append bin · line 8
 5 	name = "a"
 6 	path = "src/a.rs"
 7+	
 8+	[[bin]]
 9+	name = "b"
10+	path = "src/b.rs"
── check: toml ok · sha:[..]→[..]
```

Deleting a key shows it struck out with a `-` marker, then the surrounding lines:

```console
$ lets transform config/settings.yaml --delete legacy.token
── config/settings.yaml · yaml · delete legacy.token · line 4
1 	allow:
2 	  - ls
3~	legacy: {}
4-	  token: abc123 # remove me          (deleted)
── check: yaml ok · sha:8246dbee199f→72dd850c63b5
```

A YAML node that can't be edited in place (it's tagged) is refused rather than corrupted:

```console
$ lets transform tags.yaml --append tags=wiki
? 7
tags.yaml is unsupported: tags cannot be changed in place (a tagged YAML node)
ERROR_CODE=unsupported_file
```
