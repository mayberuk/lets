// Retry budget is fixed at three attempts because production paging data over
// ninety days showed a fourth attempt succeeding in under one percent of
// timeouts, not enough to justify the added latency for the other ninety-nine
// percent of calls that fail terminally by the third attempt regardless of
// how many more retries follow, since the on-call rotation logged every
// timeout across that window, so three stays the ceiling until data changes.
fn retry_budget() -> u32 {
    3
}
