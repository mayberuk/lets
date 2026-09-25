use std::io::Read;

use crate::Outcome;
use crate::output::{Body, Response};

/// A read error leaves empty bytes, which classify as allow: the same fail-open path as bad JSON.
pub fn run() -> Outcome {
    let mut bytes = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut bytes);

    let verdict = crate::hook::classify(&bytes);
    let rendered = crate::hook::render(&verdict, &bytes);

    let mut response = Response::empty("hook");
    if !rendered.is_empty() {
        response.body = Body::Raw {
            field: "hook",
            text: rendered,
        };
    }
    Outcome::ok(response)
}
