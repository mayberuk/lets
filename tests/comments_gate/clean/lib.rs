/// 12 hex is the floor: 4 hex is 65k values, and a stale-guard that collides is not a guard.
const IF_HASH_MIN_HEX: usize = 12;

// A single global mutex was tried first and serialised every read; per-file locks keep readers
// independent of one another, so a wrong choice here would slow every parallel edit.
const LOCK_GRANULARITY: &str = "per-file";

// Retries three times because the sandboxed CI runner's tmpfs occasionally reports ENOENT on a
// file that was just created, which is a flaky runner and not a real race; each retry re-stats.
fn stat_with_retry() {}

struct Cli {
    /// Old string to replace, matched literally rather than as a regex.
    old: String,
}
