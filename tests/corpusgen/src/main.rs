use std::io;
use std::path::{Path, PathBuf};

use corpusgen::{Spec, full, generate, is_up_to_date, small};

const USAGE: &str = "usage: corpusgen [--profile full|small] [--out <dir>]";

fn main() {
    let (spec, out) = match parse_args(std::env::args().skip(1)) {
        Ok(Some(parsed)) => parsed,
        Ok(None) => {
            println!("{USAGE}");
            return;
        },
        Err(message) => {
            eprintln!("corpusgen: {message}\n{USAGE}");
            std::process::exit(1);
        },
    };

    match summary(&spec, &out) {
        Ok(line) => println!("{line}"),
        Err(err) => {
            eprintln!("corpusgen: {}: {err}", out.display());
            std::process::exit(1);
        },
    }
}

fn summary(spec: &Spec, out: &Path) -> io::Result<String> {
    let reused = is_up_to_date(out, spec)?;
    let manifest = generate(out, spec)?;

    Ok(if reused {
        format!("up to date: {}", out.display())
    } else {
        format!(
            "{} files, {} bytes: {}",
            manifest.file_count,
            manifest.total_bytes,
            out.display()
        )
    })
}

/// `Ok(None)` is `--help`.
fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<(Spec, PathBuf)>, String> {
    let mut spec = full();
    let mut out = default_out();
    let mut args = args;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--profile" => {
                let value = args.next().ok_or("--profile needs a value")?;
                spec = match value.as_str() {
                    "full" => full(),
                    "small" => small(),
                    other => return Err(format!("unknown profile `{other}`")),
                };
            },
            "--out" => out = PathBuf::from(args.next().ok_or("--out needs a value")?),
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }

    Ok(Some((spec, out)))
}

/// Compile-time, so the default output is the same whatever directory the binary runs from.
fn default_out() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the corpusgen package sits inside tests/")
        .join("fixtures")
        .join("corpus")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use corpusgen::{full, generate, small};

    use super::{default_out, parse_args, summary};

    fn parse(args: &[&str]) -> Result<Option<(corpusgen::Spec, std::path::PathBuf)>, String> {
        parse_args(args.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn no_arguments_is_the_full_profile_in_the_fixture_tree() {
        let (spec, out) = parse(&[]).unwrap().unwrap();
        assert_eq!(spec, full());
        assert_eq!(out, default_out());
        assert!(out.ends_with("tests/fixtures/corpus"), "{}", out.display());
    }

    #[test]
    fn the_small_profile_and_an_explicit_out_are_both_read() {
        let (spec, out) = parse(&["--profile", "small", "--out", "/tmp/x"])
            .unwrap()
            .unwrap();
        assert_eq!(spec, small());
        assert_eq!(out, std::path::PathBuf::from("/tmp/x"));
    }

    #[test]
    fn help_asks_for_no_work() {
        assert!(parse(&["--help"]).unwrap().is_none());
    }

    #[test]
    fn a_bad_argument_is_an_error_not_a_default() {
        assert!(parse(&["--profile", "medium"]).is_err());
        assert!(parse(&["--profile"]).is_err());
        assert!(parse(&["--out"]).is_err());
        assert!(parse(&["--wat"]).is_err());
        assert!(parse(&["tests/fixtures/corpus"]).is_err());
    }

    #[test]
    fn a_rebuild_that_lands_on_the_same_manifest_is_not_reported_as_a_no_op() {
        let dir = std::env::temp_dir().join(format!("corpusgen-summary-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let manifest = generate(&dir, &small()).unwrap();

        assert_eq!(
            summary(&small(), &dir).unwrap(),
            format!("up to date: {}", dir.display())
        );

        fs::remove_file(dir.join("specials").join("component.vue")).unwrap();
        assert_eq!(
            summary(&small(), &dir).unwrap(),
            format!(
                "{} files, {} bytes: {}",
                manifest.file_count,
                manifest.total_bytes,
                dir.display()
            )
        );

        fs::remove_dir_all(&dir).unwrap();
    }
}
