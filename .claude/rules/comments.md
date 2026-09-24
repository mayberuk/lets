# Code comments

A comment says what the code cannot: an invariant, a non-obvious constraint, the measurement
behind a threshold, a rejected alternative, or a workaround naming its cause. Names and types
carry the what.

## Always
- Default to no comment. Delete any comment that restates the line under it.
- Put the measurement or the spec section that set a threshold, limit or default beside the
  value, once, at its definition.
- Keep a comment short. If it needs a paragraph, it belongs in CONTRIBUTING.md or the docs, and
  the comment states the fact.

## Never
- Banner comments, section dividers, commented-out code, `TODO`/`FIXME`, changelog or
  attribution lines. Git remembers.
- A doc comment on an item whose name and signature already say it.
- A comment a reader cannot resolve: a ticket, finding or thread id with no words beside it.

```rust
// ✅ DO — the constraint, not the code
/// 12 hex is the floor: 4 hex is 65k values, and a stale-guard that collides is not a guard.
const IF_HASH_MIN_HEX: usize = 12;

// ❌ DON'T
// Set the minimum hash length to 12
const IF_HASH_MIN_HEX: usize = 12; // TODO revisit
```

Why: much of this code is written by a fresh agent that defaults to explaining itself, and a
comment that restates code is a second place the truth can drift from.
