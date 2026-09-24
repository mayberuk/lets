---
title: Target grammar
description: The one grammar show, edit and transform speak for a path, a line, a range, a regex anchor or a symbol.
order: 20
group: Reference
commands: []
---

# Target grammar

One grammar, spoken by `show`, `edit` and `transform`. `find` takes a plain path or directory,
not a target, but every `find` hit prints as `path:line`, which is itself a valid target for a
following `show` or `edit` call.

| Form | Means |
|---|---|
| `path` | whole file, windowed to `--window` lines (`show`) |
| `path:40` | one line; add `-A`/`-B`/`-C` for context on `show` |
| `path:40-80` | a line range; `edit` accepts a range only with `--expect`, `--expect-all` or `--if` |
| `path@'regex'` | the first line matching the regex, with `-A`/`-B`/`-C` context; `path@'regex'+2` is the second match |
| `path#name` | the enclosing symbol: function, method, type, class, or a markdown heading |
| `path#Outer.inner` | a nested symbol |

## File names win over metacharacters

A target string that names an existing file, whole, is a path — checked before any
metacharacter is parsed. `lets show 'C#.md'` opens the file `C#.md`; only when no such file
exists is `#` read as the symbol separator. Verified:

```console
$ lets show 'C#.md'
── C#.md  (1-3 of 3) · sha:[..]
1 	one
2 	two
3 	three
── showed 1 target · 3 lines

$ lets show 'C#.md:2'
── C#.md:2  (2-2 of 2) · sha:[..]
2 	b
── showed 1 target · 1 line
```

When the whole string is not an existing file, `lets` looks for the longest prefix that ends
right before a `#`, `@` or `:`, names an existing file, and leaves a well-formed suffix
(`#name`, `@'regex'[+N]`, `:N` or `:N-M`). Only that remainder is read as the suffix. When no
prefix qualifies, the string parses by metacharacter as the table above shows, so `a.rs#main` is
the symbol `main` in `a.rs`.

## Symbol resolution (`#name`)

Tried in order:

1. **tree-sitter**, for Go, TypeScript, TSX, JavaScript, Python, Rust, Markdown, JSON, YAML,
   TOML, Shell, C, C++, C#, Java, PHP, Ruby and Swift. `.jsonc` files resolve `#symbol` with the
   JSON grammar. The footer names this resolver `via tree-sitter`.
2. **A plaintext heuristic**, for any other text file. `#name` finds lines where `name` is a
   whole word directly after one of the keywords `fn func function def class struct interface
   enum type fun trait impl object module sub proc record const let val var`, or directly before
   an opening `(`. The span is found by brace matching first, then by indentation, then falls
   back to the single matching line. The footer names this resolver `via heuristic (plaintext)`.
   Kotlin (`.kt`) always uses this path — there is no tree-sitter grammar crate for it that both
   passes the project's compatibility test and builds.
3. **`@'regex'`** is always available as a fallback for any file.

More than one candidate line for a symbol is exit 2 with every candidate listed as `path:line`;
no candidate is exit 1 with a hint to use `@'regex'` instead.

```console
$ lets show 'store.go#Open'
? 2
store.go#Open is ambiguous (2 candidates)
  store.go:44	func Open(path string) (*Store, error) {
  store.go:213	func (s *Store) Open(ctx context.Context) error {
ERROR_CODE=ambiguous

$ lets show 'greet.kt#greet'
── greet.kt#greet  (3-5 of 7 · via heuristic (plaintext)) · sha:[..]
3 	fun greet(name: String): String {
4 	    return "hi $name"
5 	}
── showed 1 target · 3 lines
```

Because the end of a plaintext span is a guess rather than a parsed boundary, `edit
--insert-after` on a plaintext symbol is refused (`ERROR_CODE=guessed_span`) unless the span came
from brace matching; `--insert-before` is unaffected, since it only needs the start.

## A target that doesn't parse gets a diagnosis, not a bare "not found"

`lets` recognizes four common habits typed against a file that does exist, and suggests the form
it would have accepted, exit 1 `ERROR_CODE=not_found`:

| Typed | Reads as | Suggestion |
|---|---|---|
| `path:40,60` | the `sed` comma habit | `path:40-60` |
| `path:40:60` | the colon habit / grep's `file:line:col` | `path:40-60` |
| `path:name` | — | `path#name` |
| `path@word` (shell stripped the quotes) | — | `path@'word'` |

```console
$ lets show a.ts:40,60
? 1
a.ts:40,60: no such file · did you mean a.ts:40-60
ERROR_CODE=not_found

$ lets show a.ts@cap
? 1
a.ts@cap: no such file · did you mean "a.ts@'cap'"
ERROR_CODE=not_found
```

When no prefix of the string names a file at all, the message is the plain one: `nope.ts: No
such file or directory (os error 2)`.

A directory given as a target is not a target: exit 7 `ERROR_CODE=unsupported_file`, naming
`lets find` as the way to search it.

## `@'regex'` with no match falls back to `find`'s reading

A `@'regex'` that matches no line is retried with the same grep-style second reading `find` gives
a pattern that matched nothing (see [lets find](/docs/find/)): `show f.go@'A\|B'` shows the first line
matching `A|B`, and the footer names the reading used.

## `find` output is made of targets

Every `find` hit prints as `path:line`, so the next call needs no read — pass that string
straight to `show` or `edit`.
