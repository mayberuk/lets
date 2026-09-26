//! The `PreToolUse` Bash classifier. Anything short of a confident classification is allow,
//! and a block travels in the JSON, never the exit code, so a crash is allow by construction.

mod bash;
pub(crate) mod bre;
pub(crate) mod check;
mod event;
mod permissions;

pub use event::{Answer, Verdict, classify, render};
