//! Every performance threshold, included via `#[path]` by `tests/startup.rs` and both benches.

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

/// p50 1.06 ms, p99 1.25 ms over 200 runs; the gate is 2× that, rounded up.
pub const GUIDE: Gate = Gate {
    p50_ms: 3,
    p99_ms: 3,
};

/// p50 1.22 ms, p99 1.37 ms over 200 runs; the gate is 2× that, rounded up.
pub const HOOK_CLASSIFY: Gate = Gate {
    p50_ms: 3,
    p99_ms: 3,
};

/// p50 1.20 ms, p99 1.32 ms over 200 runs; the gate is 2× that, rounded up.
pub const SHOW_SMALL: Gate = Gate {
    p50_ms: 3,
    p99_ms: 3,
};

/// p50 13.78 ms, p99 14.40 ms over 200 runs; the gate is 2× that, rounded up.
pub const SHOW_SYMBOL: Gate = Gate {
    p50_ms: 28,
    p99_ms: 29,
};

/// p50 8.75 ms, p99 10.60 ms over 200 runs; the gate is 2× that, rounded up.
pub const FIND: Gate = Gate {
    p50_ms: 18,
    p99_ms: 22,
};

/// p50 2.66 ms, p99 3.05 ms over 200 runs; the gate is 2× that, rounded up.
pub const EDIT: Gate = Gate {
    p50_ms: 6,
    p99_ms: 7,
};

/// p50 24.78 ms, p99 25.96 ms over 200 runs; the gate is 2× that, rounded up.
pub const EDIT_BATCH_10: Gate = Gate {
    p50_ms: 50,
    p99_ms: 52,
};

/// p50 3.73 ms, p99 4.18 ms over 200 runs; the gate is 2× that, rounded up.
pub const TRANSFORM_SET: Gate = Gate {
    p50_ms: 8,
    p99_ms: 9,
};

/// 1.28–1.33× `rg` at default threads; tighter than performance.md's 2× ceiling.
pub const FIND_VS_RG_MAX_RATIO: f64 = 1.5;

/// performance.md "Relative gates" line: "`show` ≤ 3× `cat`".
pub const SHOW_VS_CAT_MAX_RATIO: f64 = 3.0;

/// performance.md "Relative gates" line: "`hook classify` ≤ 2× `bash -n`".
pub const HOOK_VS_BASH_N_MAX_RATIO: f64 = 2.0;

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
            p50_ms: 54,
            p99_ms: 66
        });
    }

    #[test]
    fn widened_by_one_is_the_identity() {
        assert_eq!(GUIDE.widened(1), GUIDE);
    }
}
