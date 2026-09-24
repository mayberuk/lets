//! The `PreToolUse` Bash classifier. Anything short of a confident classification is allow,
//! and a block travels in the JSON, never the exit code, so a crash is allow by construction.

mod bash;
pub(crate) mod bre;
mod event;

pub use event::{Verdict, classify, render};
