# lets hook classify — the PreToolUse backstop

Hand-written: `scripts/gen-examples.sh` renders one page per `tests/cmd/<verb>/` directory and
`hook classify` has none. The command and output below are copied verbatim from
`tests/scenarios/hook/`; a change to that golden is a change to this page.

Claude Code's `PreToolUse` hook pipes one JSON event per Bash call to `lets hook classify` on
stdin. `classify` replies on stdout: nothing for allow, one JSON `deny` line for a block, always
naming a runnable `lets` replacement.

## Blocked, then replaced

```console
$ printf '{"session_id":"s","cwd":"%s","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cat src/usage.ts"}}' "$(pwd)" | lets hook classify
{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"lets replaces this command in one call.\nrun: lets show src/usage.ts"}}

$ lets show src/usage.ts
── src/usage.ts  (1-9 of 9) · sha:9925e474e391
1   const cap = 10
2   
3   export function usage(entries: number[]) {
4     let total = 0
5     for (const entry of entries) {
6       total += entry
7     }
8     return total > cap ? cap : total
9   }
── showed 1 target · 9 lines
```

## Allowed — piped consumer

`cat`'s stdout goes to `wc`, not to the tool result, so the walk that only looks at a pipeline's
last displayed stage never finds a read to replace:

```console
$ printf '{"session_id":"s","cwd":"%s","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cat config/app.json | wc -l"}}' "$(pwd)" | lets hook classify
```

Stdout is empty and the exit code is 0 — the command runs unmodified.
