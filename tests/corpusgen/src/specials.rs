use std::path::Path;
use std::{fs, io};

use crate::languages;
use crate::rng::Rng;

pub const DIR_NAME: &str = "specials";

/// Fixed so a bench workload can name it; a seed-dependent symbol would need a lookup first.
pub const SEARCH_TARGET_SYMBOL: &str = "corpusSearchTarget";

/// A thousand lines deep, where a windowed `show` has to seek rather than print from the top.
const SEARCH_TARGET_UNITS: usize = 500;

/// Whole-file, so it sits inside the 8 KiB window the binary sniff reads.
const BINARY_BLOB_BYTES: usize = 4096;

/// Returns the number of files written and their total bytes.
pub fn write_all(dir: &Path, large_file_bytes: u64, rng: &mut Rng) -> io::Result<(usize, u64)> {
    let root = dir.join(DIR_NAME);
    fs::create_dir_all(&root)?;

    let target = usize::try_from(large_file_bytes)
        .map_err(|_| io::Error::other(format!("{large_file_bytes} bytes exceeds this platform")))?;

    let files: [(&str, Vec<u8>); 7] = [
        ("search-target.ts", search_target_file().into_bytes()),
        ("component.vue", vue_file().into_bytes()),
        ("notes.xyz", unknown_extension_file().into_bytes()),
        ("crlf.ts", crlf_file().into_bytes()),
        ("bom.md", bom_file().into_bytes()),
        ("large.md", large_file(target).into_bytes()),
        ("blob.bin", binary_blob(rng)),
    ];

    let mut bytes = 0;
    for (name, content) in &files {
        fs::write(root.join(name), content)?;
        bytes += content.len() as u64;
    }
    Ok((files.len(), bytes))
}

fn search_target_file() -> String {
    let mut out = String::new();
    for index in 0..SEARCH_TARGET_UNITS {
        if index > 0 {
            out.push('\n');
        }
        if index == SEARCH_TARGET_UNITS / 2 {
            out.push_str(&format!(
                "export function {SEARCH_TARGET_SYMBOL}(x: number): number {{\n  return x;\n}}\n"
            ));
        } else {
            out.push_str(&languages::typescript(index));
        }
    }
    out
}

/// No bundled grammar claims `.vue`, so this exercises `skipped (no grammar)` whatever it holds.
fn vue_file() -> String {
    "<template>\n  <span class=\"unit\">unit</span>\n</template>\n".to_string()
}

fn unknown_extension_file() -> String {
    "unit one\nunit two\nunit three\n".to_string()
}

fn crlf_file() -> String {
    let mut out = String::new();
    for index in 0..8 {
        out.push_str(&languages::typescript(index));
    }
    out.replace('\n', "\r\n")
}

fn bom_file() -> String {
    let mut out = String::from("\u{feff}");
    for index in 0..8 {
        out.push_str(&languages::markdown(index));
    }
    out
}

/// Exactly `target` bytes: whole markdown units first, then ASCII padding, so the file is still
/// UTF-8 text and does not double as the binary case.
fn large_file(target: usize) -> String {
    let mut out = String::with_capacity(target);
    let mut index = 0;
    loop {
        let unit = languages::markdown(index);
        let separator = usize::from(index > 0);
        if out.len() + separator + unit.len() > target {
            break;
        }
        if separator == 1 {
            out.push('\n');
        }
        out.push_str(&unit);
        index += 1;
    }

    let pad = target - out.len();
    if pad > 0 {
        out.push_str(&"x".repeat(pad - 1));
        out.push('\n');
    }
    out
}

fn binary_blob(rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::with_capacity(BINARY_BLOB_BYTES);
    while out.len() < BINARY_BLOB_BYTES {
        out.extend_from_slice(&rng.next_u64().to_le_bytes());
    }
    out.truncate(BINARY_BLOB_BYTES);
    out[0] = 0;
    out
}

#[cfg(test)]
mod tests {
    use super::{
        SEARCH_TARGET_SYMBOL, binary_blob, bom_file, crlf_file, large_file, search_target_file,
    };
    use crate::rng::Rng;

    #[test]
    fn the_large_file_is_the_requested_length_to_the_byte() {
        for target in [1, 41, 4096, 65_536, 8 * 1024 * 1024] {
            assert_eq!(large_file(target).len(), target, "target {target}");
        }
    }

    #[test]
    fn the_large_file_is_text_with_no_nul() {
        let text = large_file(65_536);
        assert!(!text.as_bytes().contains(&0));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn the_binary_blob_opens_with_a_nul() {
        let mut rng = Rng::new(1);
        let blob = binary_blob(&mut rng);
        assert_eq!(blob.len(), 4096);
        assert_eq!(blob[0], 0);
    }

    #[test]
    fn the_binary_blob_follows_its_seed() {
        let a = binary_blob(&mut Rng::new(1));
        let b = binary_blob(&mut Rng::new(2));
        assert_ne!(a, b);
    }

    #[test]
    fn the_search_target_appears_once_near_the_middle() {
        let text = search_target_file();
        assert_eq!(text.matches(SEARCH_TARGET_SYMBOL).count(), 1);

        let lines: Vec<&str> = text.lines().collect();
        let at = lines
            .iter()
            .position(|l| l.contains(SEARCH_TARGET_SYMBOL))
            .unwrap();
        assert!(
            at > lines.len() / 4 && at < lines.len() * 3 / 4,
            "symbol at line {at} of {}",
            lines.len()
        );
        assert!(lines.len() > 1500, "only {} lines", lines.len());
    }

    #[test]
    fn the_crlf_file_has_no_bare_newline() {
        let text = crlf_file();
        assert!(text.contains("\r\n"));
        assert_eq!(text.matches('\n').count(), text.matches("\r\n").count());
    }

    #[test]
    fn the_bom_file_opens_with_the_three_byte_bom() {
        let bytes = bom_file().into_bytes();
        assert_eq!(&bytes[..3], &[0xef, 0xbb, 0xbf]);
    }

    #[test]
    fn an_ordinary_file_carries_neither_bom_nor_crlf() {
        let text = large_file(4096);
        assert!(!text.starts_with('\u{feff}'));
        assert!(!text.contains("\r\n"));
    }
}
