pub mod languages;
pub mod manifest;
pub mod rng;
pub mod specials;

use std::ffi::OsStr;
use std::path::Path;
use std::{fs, io};

pub use manifest::Manifest;
use rng::Rng;

/// Bump on any change to the generation algorithm: a tree is reused only when its manifest
/// names this version.
pub const GENERATOR_VERSION: u32 = 1;

/// So `find` walks a tree rather than one flat directory of two hundred entries.
const FILES_PER_DIR: usize = 20;

/// A 6,144-byte mean: with the 8 MiB special, `full()` is ~19.8 MiB over 1,998 files, inside
/// the corpus's 18–22 MiB, 1,800–2,200 file target.
const FILLER_MIN_BYTES: u64 = 2048;
const FILLER_SPAN_BYTES: u64 = 8192;

/// Committed to keep the generated tree out of git; never counted, deleted, or taken for a
/// stranger's directory.
const IGNORE_FILE: &str = ".gitignore";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    pub seed: u64,
    pub filler_per_language: usize,
    pub large_file_bytes: u64,
}

pub fn full() -> Spec {
    Spec {
        seed: 1,
        filler_per_language: 181,
        large_file_bytes: 8 * 1024 * 1024,
    }
}

/// Every special file is still there.
pub fn small() -> Spec {
    Spec {
        seed: 1,
        filler_per_language: 2,
        large_file_bytes: 65_536,
    }
}

/// The manifest is written last, so a killed run leaves an unmanifested tree, which is refused
/// rather than deleted: nothing tells it apart from a directory somebody else owns.
pub fn generate(dir: &Path, spec: &Spec) -> io::Result<Manifest> {
    if let Some(existing) = reusable(dir, spec)? {
        return Ok(existing);
    }

    if dir.exists() {
        if manifest::read(dir).is_none() && holds_content(dir)? {
            return Err(io::Error::other(format!(
                "{} is not empty and holds no {}: refusing to delete a tree this generator did \
                 not write",
                dir.display(),
                manifest::FILE_NAME
            )));
        }
        clear(dir)?;
    }
    fs::create_dir_all(dir)?;

    let profile = profile_name(spec);

    let mut rng = Rng::new(spec.seed);
    let mut file_count = 0;
    let mut total_bytes = 0;

    for &lang in languages::ALL {
        let language_dir = dir.join(lang.name());
        for index in 0..spec.filler_per_language {
            let bucket = language_dir.join(format!("mod_{}", index / FILES_PER_DIR));
            if index % FILES_PER_DIR == 0 {
                fs::create_dir_all(&bucket)?;
            }
            let target = (FILLER_MIN_BYTES + rng.next_below(FILLER_SPAN_BYTES)) as usize;
            let text = languages::file(lang, target);
            let name = format!("{}_{index}.{}", lang.name(), lang.extension());
            fs::write(bucket.join(name), &text)?;
            file_count += 1;
            total_bytes += text.len() as u64;
        }
    }

    let (special_count, special_bytes) = specials::write_all(dir, spec.large_file_bytes, &mut rng)?;
    file_count += special_count;
    total_bytes += special_bytes;

    let manifest = Manifest {
        seed: spec.seed,
        profile: profile.unwrap_or(manifest::CUSTOM),
        generator_version: GENERATOR_VERSION,
        file_count,
        total_bytes,
    };
    manifest::write(dir, &manifest)?;
    Ok(manifest)
}

fn is_corpus(name: &OsStr) -> bool {
    name != manifest::FILE_NAME && name != IGNORE_FILE
}

fn holds_content(dir: &Path) -> io::Result<bool> {
    for entry in fs::read_dir(dir)? {
        if is_corpus(&entry?.file_name()) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn clear(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name() == IGNORE_FILE {
            continue;
        }
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// Ask before generating: a tree rebuilt because a file went missing ends up under the very
/// manifest it started with.
pub fn is_up_to_date(dir: &Path, spec: &Spec) -> io::Result<bool> {
    Ok(reusable(dir, spec)?.is_some())
}

/// The manifest must also still describe the disk, or a deleted, added or resized file would be
/// benched as if the corpus were whole.
fn reusable(dir: &Path, spec: &Spec) -> io::Result<Option<Manifest>> {
    let Some(existing) = manifest::read(dir) else {
        return Ok(None);
    };
    if profile_name(spec) != Some(existing.profile)
        || existing.seed != spec.seed
        || existing.generator_version != GENERATOR_VERSION
        || measure(dir)? != (existing.file_count, existing.total_bytes)
    {
        return Ok(None);
    }
    Ok(Some(existing))
}

fn measure(dir: &Path) -> io::Result<(usize, u64)> {
    let mut file_count = 0;
    let mut total_bytes = 0;
    let mut stack = vec![dir.to_path_buf()];

    while let Some(next) = stack.pop() {
        for entry in fs::read_dir(&next)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                stack.push(entry.path());
            } else if is_corpus(&entry.file_name()) {
                file_count += 1;
                total_bytes += entry.metadata()?.len();
            }
        }
    }

    Ok((file_count, total_bytes))
}

/// `None` means never reuse: the manifest cannot tell two custom specs apart. The seed is
/// compared separately, from the manifest's own record of it.
fn profile_name(spec: &Spec) -> Option<&'static str> {
    let shape = (spec.filler_per_language, spec.large_file_bytes);
    if shape == (full().filler_per_language, full().large_file_bytes) {
        Some(manifest::FULL)
    } else if shape == (small().filler_per_language, small().large_file_bytes) {
        Some(manifest::SMALL)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{Spec, full, profile_name, small};
    use crate::manifest;

    #[test]
    fn each_preset_shape_names_its_profile() {
        assert_eq!(profile_name(&full()), Some(manifest::FULL));
        assert_eq!(profile_name(&small()), Some(manifest::SMALL));
    }

    #[test]
    fn a_seed_change_alone_keeps_the_profile_name() {
        assert_eq!(
            profile_name(&Spec { seed: 99, ..full() }),
            Some(manifest::FULL)
        );
    }

    #[test]
    fn a_shape_that_is_neither_preset_has_no_reusable_profile() {
        assert_eq!(
            profile_name(&Spec {
                filler_per_language: 3,
                ..small()
            }),
            None
        );
        assert_eq!(
            profile_name(&Spec {
                large_file_bytes: 1234,
                ..full()
            }),
            None
        );
    }

    #[test]
    fn the_two_presets_are_different_shapes() {
        assert_ne!(
            (full().filler_per_language, full().large_file_bytes),
            (small().filler_per_language, small().large_file_bytes)
        );
    }
}
