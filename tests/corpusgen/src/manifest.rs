use std::path::Path;
use std::{fs, io};

pub const FILE_NAME: &str = ".manifest";

pub const FULL: &str = "full";
pub const SMALL: &str = "small";
/// Round-trips through `read`, but `generate` never reuses a tree recorded under it.
pub const CUSTOM: &str = "custom";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Manifest {
    pub seed: u64,
    pub profile: &'static str,
    pub generator_version: u32,
    pub file_count: usize,
    pub total_bytes: u64,
}

/// The manifest itself is not counted, so the numbers do not depend on its own length.
pub fn write(dir: &Path, manifest: &Manifest) -> io::Result<()> {
    let text = format!(
        "seed={}\nprofile={}\ngenerator_version={}\nfile_count={}\ntotal_bytes={}\n",
        manifest.seed,
        manifest.profile,
        manifest.generator_version,
        manifest.file_count,
        manifest.total_bytes,
    );
    fs::write(dir.join(FILE_NAME), text)
}

/// `None` means generate: a missing or truncated manifest is the normal state, not an error.
pub fn read(dir: &Path) -> Option<Manifest> {
    let text = fs::read_to_string(dir.join(FILE_NAME)).ok()?;

    let mut seed = None;
    let mut profile = None;
    let mut generator_version = None;
    let mut file_count = None;
    let mut total_bytes = None;

    for line in text.lines() {
        let (key, value) = line.split_once('=')?;
        match key {
            "seed" => seed = Some(value.parse().ok()?),
            "profile" => profile = Some(profile_label(value)?),
            "generator_version" => generator_version = Some(value.parse().ok()?),
            "file_count" => file_count = Some(value.parse().ok()?),
            "total_bytes" => total_bytes = Some(value.parse().ok()?),
            _ => return None,
        }
    }

    Some(Manifest {
        seed: seed?,
        profile: profile?,
        generator_version: generator_version?,
        file_count: file_count?,
        total_bytes: total_bytes?,
    })
}

fn profile_label(value: &str) -> Option<&'static str> {
    match value {
        FULL => Some(FULL),
        SMALL => Some(SMALL),
        CUSTOM => Some(CUSTOM),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{FILE_NAME, FULL, Manifest, read, write};

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("corpusgen-manifest-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample() -> Manifest {
        Manifest {
            seed: 1,
            profile: FULL,
            generator_version: 1,
            file_count: 1998,
            total_bytes: 20_000_000,
        }
    }

    #[test]
    fn a_written_manifest_reads_back_unchanged() {
        let dir = scratch("round-trip");
        write(&dir, &sample()).unwrap();
        assert_eq!(read(&dir), Some(sample()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_manifest_is_none() {
        let dir = scratch("missing");
        assert_eq!(read(&dir), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unparsable_manifest_is_none() {
        for (name, body) in [
            ("garbage", "not a manifest at all"),
            ("truncated", "seed=1\nprofile=full\n"),
            (
                "bad-number",
                "seed=x\nprofile=full\ngenerator_version=1\nfile_count=1\ntotal_bytes=1\n",
            ),
            (
                "unknown-profile",
                "seed=1\nprofile=medium\ngenerator_version=1\nfile_count=1\ntotal_bytes=1\n",
            ),
            (
                "unknown-key",
                "seed=1\nprofile=full\ngenerator_version=1\nfile_count=1\ntotal_bytes=1\nextra=9\n",
            ),
        ] {
            let dir = scratch(name);
            fs::write(dir.join(FILE_NAME), body).unwrap();
            assert_eq!(read(&dir), None, "{name}");
            fs::remove_dir_all(&dir).unwrap();
        }
    }
}
