//! Every performance threshold, included via `#[path]` by `tests/startup.rs` and both benches.
//!
//! The `Gate` p50/p99 values below are each the median of three 200-run `bench-gate` passes
//! taken 2026-09-24 on a shared 8-core/16-thread Ryzen 7 5700X3D (1-minute load 2–4, not idle),
//! set at 2× that median, rounded up.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gate {
    pub p50_ms: u64,
    pub p99_ms: u64,
}

impl Gate {
    #[must_use]
    pub const fn widened(self, factor: u64) -> Gate {
        Gate {
            p50_ms: self.p50_ms * factor,
            p99_ms: self.p99_ms * factor,
        }
    }
}

/// Median p50 0.77 ms, p99 1.41 ms.
pub const GUIDE: Gate = Gate {
    p50_ms: 2,
    p99_ms: 3,
};

/// Median p50 1.05 ms, p99 1.66 ms.
pub const HOOK_CLASSIFY: Gate = Gate {
    p50_ms: 3,
    p99_ms: 4,
};

/// Median p50 0.87 ms, p99 1.31 ms.
pub const SHOW_SMALL: Gate = Gate {
    p50_ms: 2,
    p99_ms: 3,
};

/// Median p50 11.39 ms, p99 12.26 ms.
pub const SHOW_SYMBOL: Gate = Gate {
    p50_ms: 23,
    p99_ms: 25,
};

/// Median p50 7.83 ms, p99 9.69 ms.
pub const FIND: Gate = Gate {
    p50_ms: 16,
    p99_ms: 20,
};

/// The same search printing its hit's enclosing symbol, which parses the 2,000-line file holding
/// it: median p50 16.11 ms, p99 18.39 ms.
pub const FIND_EXPANDED: Gate = Gate {
    p50_ms: 33,
    p99_ms: 37,
};

/// Median p50 2.30 ms, p99 2.99 ms.
pub const EDIT: Gate = Gate {
    p50_ms: 5,
    p99_ms: 6,
};

/// Median p50 22.99 ms, p99 24.37 ms.
pub const EDIT_BATCH_10: Gate = Gate {
    p50_ms: 46,
    p99_ms: 49,
};

/// Median p50 3.18 ms, p99 3.96 ms.
pub const TRANSFORM_SET: Gate = Gate {
    p50_ms: 7,
    p99_ms: 8,
};

/// 1.13–1.15× `rg` at default threads across three passes (2026-09-24); tighter than
/// performance.md's 2× ceiling.
pub const FIND_VS_RG_MAX_RATIO: f64 = 1.3;

/// 1.62–1.67× `cat` across three passes (2026-09-24); tighter than performance.md's
/// "`show` ≤ 3× `cat`" ceiling.
pub const SHOW_VS_CAT_MAX_RATIO: f64 = 2.0;

/// 1.17–1.19× `bash -n` across three passes (2026-09-24); tighter than performance.md's
/// "`hook classify` ≤ 2× `bash -n`" ceiling.
pub const HOOK_VS_BASH_N_MAX_RATIO: f64 = 1.4;

/// 2.2–3.3× quiet and under 6–40 busy loops on 16 cores; the gate is 2× that.
pub const GUIDE_VS_TRUE_MAX_RATIO: f64 = 6.0;

/// 39.6 MB max RSS; the gate is 2× that, inside performance.md's 128 MB ceiling.
pub const EDIT_8MIB_PEAK_RSS_BYTES: u64 = 80 * 1024 * 1024;

/// 28.0 MB max RSS; the gate is 2× that, inside performance.md's 128 MB ceiling.
pub const TRANSFORM_8MIB_JSON_PEAK_RSS_BYTES: u64 = 56 * 1024 * 1024;

/// `just release-check` reads this line with `sed`: keep it one integer expression bash can eval.
pub const BINARY_MAX_BYTES: u64 = 40 * 1024 * 1024;

/// performance.md: a 10% regression in allocation count, bytes or peak fails.
pub const ALLOC_REGRESSION_MAX_RATIO: f64 = 1.10;

/// performance.md: "CI enforces 3×" over the tight local gate.
pub const CI_MARGIN: u64 = 3;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widened_scales_both_percentiles() {
        assert_eq!(FIND.widened(3), Gate {
            p50_ms: 48,
            p99_ms: 60
        });
    }

    #[test]
    fn widened_by_one_is_the_identity() {
        assert_eq!(GUIDE.widened(1), GUIDE);
    }
}
