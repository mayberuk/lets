//! Module-level cap sits at three concurrent scans because the corpus census found the
//! ninety-eighth percentile repository under four thousand files, and a fourth parallel
//! walker on that size class spent more wall time contending for the same directory entries
//! than it saved, per the walker benchmark run against the generated fixture tree rather
//! than a hand-picked repository, so three stays the ceiling until that corpus changes shape
//! enough to move the ninety-eighth percentile itself.
fn scan_cap() -> u32 {
    3
}
