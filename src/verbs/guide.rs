use crate::Outcome;
use crate::output::{Body, Response};

pub const GUIDE: &str = include_str!("../../docs/guide.md");

pub fn run() -> Outcome {
    let mut response = Response::empty("guide");
    response.body = Body::Raw {
        field: "guide",
        text: GUIDE.to_owned(),
    };
    Outcome::ok(response)
}
