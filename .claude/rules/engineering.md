# Engineering canon

Agent-built codebases rot through invented abstraction, not through style. Every module stays
the size of its real job, and the decided seams below are the only seams.

## Always
- Treat the decided seams as fixed: the target grammar (`target.rs`), the output model
  (`output.rs`), the one `Error` enum, one module per verb. A change that wants a new seam
  proposes it in the pull request with the second concrete consumer named; it does not add one.
- Write it inline the first time, duplicate it the second, extract it the third.
- Call a crate directly until the second call site exists.
- Keep one `Error` enum (`thiserror`) in the library and one exit-code `match` in `main.rs`.
  A new failure is a new variant, never a new type.
- Put a type next to the function that creates the data; move it only when a second module
  imports it.
- Enforce mechanically what can be enforced mechanically: `clippy pedantic` with the allow-list
  in `Cargo.toml` `[lints]`, `cargo-deny`, `cargo-machete`, `scripts/deps-gate.sh`. A prose rule
  exists only for a judgment call.
- Make every code path deterministic for identical input and file state: no `HashMap` iteration
  in output order, no timestamps in stdout, no randomness.

## Never
- A trait with one implementation, a "provider" or "backend" with one backend, a builder for a
  struct constructed in one place.
- A wrapper module around a crate used in one place.
- `anyhow`, `Box<dyn Error>`, a per-module error type, or a `Result` alias that hides the enum.
- A `utils.rs`, `helpers.rs`, `common.rs` or `types.rs` dump.
- Module-level mutable state (`static mut`, or an `OnceLock` holding anything but a lazily
  initialised grammar), or a global that carries request state.
- `unsafe` without a measurement in the merge request and a comment stating the invariant.
- A feature flag, config file, cache, index or daemon. The spec's "nothing to maintain" is a
  principle, not a v1 shortcut.

```rust
// ✅ DO — one enum, one mapping
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("`--old` not found in {path}")]
    OldNotFound { path: PathBuf, nearest: Option<Candidate> },
}
// main.rs: match err { Error::OldNotFound { .. } => 1, Error::Ambiguous { .. } => 2, /* … */ }

// ❌ DON'T — a second error type and a trait for one matcher
pub struct MatchError(String);
pub trait Matcher { fn find(&self, hay: &[u8]) -> Option<Span>; }
pub struct ExactMatcher;
impl Matcher for ExactMatcher { /* the only implementation */ }
```

Why: every "never" item is something an agent adds to feel safe, and each is a second place the
truth can drift from.
