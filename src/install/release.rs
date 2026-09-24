use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{Error, InstallRefusedReason};
use crate::install::pathguard;

const INSTALLER_SCRIPT: &str = "lets-installer.sh";

/// `--fail` matters most: without it a 404 page is saved and handed to `sh` as the installer.
const CURL_HARDENING: [&str; 4] = ["--proto", "=https", "--tlsv1.2", "--fail"];

/// Makes dist's installer (0.33) install flat into this directory and leave shell rc files alone.
const INSTALL_DIR_VAR: &str = "LETS_UNMANAGED_INSTALL";

pub fn target_triple() -> Result<&'static str, Error> {
    triple_for(std::env::consts::OS, std::env::consts::ARCH)
}

fn triple_for(os: &str, arch: &str) -> Result<&'static str, Error> {
    match (os, arch) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        _ => Err(Error::UpdateFailed {
            detail: "no published release target for this OS/architecture".into(),
        }),
    }
}

/// Semver 2.0 precedence; build metadata is parsed and discarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Option<String>,
}

impl Version {
    pub fn parse(text: &str) -> Option<Version> {
        let (rest, build) = match text.split_once('+') {
            Some((rest, build)) => (rest, Some(build)),
            None => (text, None),
        };
        if build.is_some_and(|build| !is_valid_build_metadata(build)) {
            return None;
        }
        let (core, pre) = match rest.split_once('-') {
            Some((core, pre)) if is_valid_pre_release(pre) => (core, Some(pre.to_owned())),
            Some(_) => return None,
            None => (rest, None),
        };
        let mut fields = core.split('.').map(parse_numeric_core_field);
        match (fields.next(), fields.next(), fields.next(), fields.next()) {
            (Some(Some(major)), Some(Some(minor)), Some(Some(patch)), None) => Some(Version {
                major,
                minor,
                patch,
                pre,
            }),
            _ => None,
        }
    }

    pub fn current() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).expect("cargo requires a semver package version")
    }
}

fn parse_numeric_core_field(field: &str) -> Option<u64> {
    let valid = !field.is_empty()
        && field.bytes().all(|b| b.is_ascii_digit())
        && (field == "0" || !field.starts_with('0'));
    valid.then(|| field.parse::<u64>().ok()).flatten()
}

fn is_alphanumeric_or_hyphen(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn is_valid_pre_release(suffix: &str) -> bool {
    !suffix.is_empty()
        && suffix.split('.').all(|id| {
            is_alphanumeric_or_hyphen(id)
                && (!id.bytes().all(|b| b.is_ascii_digit()) || id == "0" || !id.starts_with('0'))
        })
}

/// Unlike a pre-release identifier, a numeric one may carry a leading zero (semver §10).
fn is_valid_build_metadata(build: &str) -> bool {
    !build.is_empty() && build.split('.').all(is_alphanumeric_or_hyphen)
}

/// With no leading zeros, length then bytes orders numeric identifiers without a u64 parse that a
/// long one could overflow.
fn identifier_cmp(ours: &str, theirs: &str) -> Ordering {
    let ours_numeric = ours.bytes().all(|b| b.is_ascii_digit());
    let theirs_numeric = theirs.bytes().all(|b| b.is_ascii_digit());
    match (ours_numeric, theirs_numeric) {
        (true, true) => ours.len().cmp(&theirs.len()).then_with(|| ours.cmp(theirs)),
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => ours.cmp(theirs),
    }
}

fn pre_release_cmp(ours: &str, theirs: &str) -> Ordering {
    let mut ours_ids = ours.split('.');
    let mut theirs_ids = theirs.split('.');
    loop {
        match (ours_ids.next(), theirs_ids.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ours), Some(theirs)) => {
                let ord = identifier_cmp(ours, theirs);
                if ord != Ordering::Equal {
                    return ord;
                }
            },
        }
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Version) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (Some(ours), Some(theirs)) => pre_release_cmp(ours, theirs),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Version) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        match &self.pre {
            Some(pre) => write!(f, "-{pre}"),
            None => Ok(()),
        }
    }
}

/// dist's installer detects the platform at run time, so the URL names no target triple.
pub fn installer_url(repository: &str) -> Result<String, Error> {
    Ok(format!(
        "{}/latest/download/{INSTALLER_SCRIPT}",
        releases_url(repository)?
    ))
}

fn releases_url(repository: &str) -> Result<String, Error> {
    if repository.is_empty() {
        return Err(Error::InstallRefused {
            reason: InstallRefusedReason::NoRepository,
        });
    }
    let slug = repository
        .trim_start_matches("https://github.com/")
        .trim_end_matches('/')
        .trim_end_matches(".git");
    let valid_part = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    match slug.split_once('/') {
        Some((owner, name)) if valid_part(owner) && valid_part(name) => {
            Ok(format!("https://github.com/{owner}/{name}/releases"))
        },
        _ => Err(Error::UpdateFailed {
            detail: format!("repository `{repository}` is not a GitHub owner/name"),
        }),
    }
}

/// The `releases/latest` redirect names the newest tag without the API's rate limit; no `-L`,
/// since the redirect itself is the answer.
pub fn latest(repository: &str, curl: &Path) -> Result<Version, Error> {
    let url = format!("{}/latest", releases_url(repository)?);
    let output = Command::new(curl)
        .args(CURL_HARDENING)
        .arg("-sSI")
        .arg(&url)
        .stderr(Stdio::inherit())
        .output()
        .map_err(|source| Error::Io {
            path: curl.to_path_buf(),
            source,
        })?;
    if !output.status.success() {
        return Err(Error::UpdateFailed {
            detail: format!(
                "resolving the latest release from {url} failed: curl {}",
                output.status
            ),
        });
    }
    tag_version(&String::from_utf8_lossy(&output.stdout), &url)
}

/// The last `location` header wins: a proxy's `CONNECT` response can come first in `-I` output.
fn tag_version(headers: &str, url: &str) -> Result<Version, Error> {
    let location = headers
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("location")
                .then(|| value.trim())
        })
        .next_back();
    let Some((_, tag)) = location.and_then(|location| location.rsplit_once("/tag/")) else {
        return Err(Error::UpdateFailed {
            detail: format!("{url} did not redirect to a release tag"),
        });
    };
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok_or_else(|| Error::UpdateFailed {
        detail: format!("the latest release tag `{tag}` is not vMAJOR.MINOR.PATCH"),
    })
}

pub fn install_dir(path_var: &str) -> Result<PathBuf, Error> {
    let current = pathguard::refuse_if_shadowed(path_var)?;
    current
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| Error::UpdateFailed {
            detail: format!("{} has no parent directory", current.display()),
        })
}

/// Not `curl … | sh`: the pipeline's status is `sh`'s, which exits 0 on a failed download's empty
/// input.
pub fn download_and_install(url: &str, install_dir: &Path, curl: &Path) -> Result<(), Error> {
    let scratch = tempfile::tempdir().map_err(|source| Error::Io {
        path: std::env::temp_dir(),
        source,
    })?;
    let script = scratch.path().join(INSTALLER_SCRIPT);

    let download = Command::new(curl)
        .args(CURL_HARDENING)
        .args(["-sSL", "-o"])
        .arg(&script)
        .arg(url)
        .stdout(Stdio::from(std::io::stderr()))
        .status()
        .map_err(|source| Error::Io {
            path: curl.to_path_buf(),
            source,
        })?;
    if !download.success() {
        return Err(Error::UpdateFailed {
            detail: format!("downloading {url} failed: curl {download}"),
        });
    }

    let install = Command::new("/bin/sh")
        .arg(&script)
        .env(INSTALL_DIR_VAR, install_dir)
        .stdout(Stdio::from(std::io::stderr()))
        .status()
        .map_err(|source| Error::Io {
            path: script.clone(),
            source,
        })?;
    if install.success() {
        Ok(())
    } else {
        Err(Error::UpdateFailed {
            detail: format!("the installer from {url} failed: {install}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use tempfile::TempDir;

    use super::*;

    const URL: &str = "https://github.com/acme/lets/releases/latest/download/lets-installer.sh";

    fn fake_curl(body: &str) -> TempDir {
        let bin = TempDir::new().unwrap();
        let curl = bin.path().join("curl");
        std::fs::write(
            &curl,
            format!(
                "#!/bin/sh\nargs=\"$*\"\nwhile [ $# -gt 0 ]; do case \"$1\" in -o) out=\"$2\"; shift;; esac; \
                 shift; done\n{body}\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    fn update_failed_detail(result: Result<(), Error>) -> String {
        match result {
            Err(Error::UpdateFailed { detail }) => detail,
            other => panic!("expected UpdateFailed, got {other:?}"),
        }
    }

    #[test]
    fn linux_x86_64_maps_to_the_musl_triple() {
        assert_eq!(
            triple_for("linux", "x86_64").unwrap(),
            "x86_64-unknown-linux-musl"
        );
    }

    #[test]
    fn macos_targets_map_to_the_darwin_triples() {
        assert_eq!(
            triple_for("macos", "aarch64").unwrap(),
            "aarch64-apple-darwin"
        );
        assert_eq!(
            triple_for("macos", "x86_64").unwrap(),
            "x86_64-apple-darwin"
        );
    }

    #[test]
    fn linux_aarch64_maps_to_the_musl_triple() {
        assert_eq!(
            triple_for("linux", "aarch64").unwrap(),
            "aarch64-unknown-linux-musl"
        );
    }

    #[test]
    fn an_unpublished_platform_fails_without_panicking() {
        let detail = update_failed_detail(triple_for("windows", "x86_64").map(|_| ()));
        assert_eq!(
            detail,
            "no published release target for this OS/architecture"
        );
    }

    #[test]
    fn a_full_url_a_trailing_slash_a_git_suffix_and_a_bare_slug_all_name_the_same_installer() {
        for repository in [
            "https://github.com/acme/lets",
            "https://github.com/acme/lets/",
            "https://github.com/acme/lets.git",
            "acme/lets",
        ] {
            assert_eq!(installer_url(repository).unwrap(), URL, "{repository}");
        }
    }

    #[test]
    fn a_repository_that_is_not_owner_slash_name_is_refused() {
        for repository in [
            "https://gitlab.com/acme/lets",
            "acme",
            "acme/lets/extra",
            "acme/le ts",
        ] {
            assert!(
                matches!(installer_url(repository), Err(Error::UpdateFailed { .. })),
                "{repository}"
            );
        }
    }

    #[test]
    fn an_empty_repository_is_refused_as_no_repository() {
        assert!(matches!(
            installer_url(""),
            Err(Error::InstallRefused {
                reason: InstallRefusedReason::NoRepository
            })
        ));
    }

    fn latest_from(body: &str) -> Result<Version, Error> {
        let bin = fake_curl(body);
        latest("acme/lets", &bin.path().join("curl"))
    }

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap_or_else(|| panic!("{text} should parse"))
    }

    #[test]
    fn the_releases_latest_redirect_names_the_latest_version() {
        let found = latest_from(
            "printf 'HTTP/2 302\\r\\nlocation:              https://github.com/acme/lets/releases/tag/v0.2.0\\r\\n\\r\\n'",
        )
        .unwrap();

        assert_eq!(found, version("0.2.0"));
    }

    #[test]
    fn an_http_1_location_header_is_matched_case_insensitively_and_the_last_one_wins() {
        let found = latest_from(
            "printf 'HTTP/1.1 200 Connection established\\r\\nLocation:              https://proxy.example/tag/v9.9.9\\r\\n\\r\\nHTTP/1.1 302 Found\\r\\nLocation:              https://github.com/acme/lets/releases/tag/v1.4.0-rc.1\\r\\n\\r\\n'",
        )
        .unwrap();

        assert_eq!(found, version("1.4.0-rc.1"));
    }

    #[test]
    fn the_latest_check_asks_for_headers_only_with_the_download_s_hardening_and_no_redirects() {
        let log = TempDir::new().unwrap();
        let args = log.path().join("args");
        let found = latest_from(&format!(
            "printf '%s' \"$args\" > '{}'; printf 'location: /acme/lets/releases/tag/v0.2.0\\n'",
            args.display()
        ))
        .unwrap();

        assert_eq!(found, version("0.2.0"));
        assert_eq!(
            std::fs::read_to_string(args).unwrap(),
            "--proto =https --tlsv1.2 --fail -sSI https://github.com/acme/lets/releases/latest"
        );
    }

    #[test]
    fn a_failed_latest_check_is_update_failed_naming_the_url_and_curl_s_status() {
        let detail = update_failed_detail(latest_from("exit 6").map(|_| ()));

        assert_eq!(
            detail,
            "resolving the latest release from https://github.com/acme/lets/releases/latest \
             failed: curl exit status: 6"
        );
    }

    #[test]
    fn a_redirect_to_no_release_tag_is_update_failed() {
        let detail = update_failed_detail(
            latest_from("printf 'location: https://github.com/acme/lets/releases\\n'").map(|_| ()),
        );

        assert_eq!(
            detail,
            "https://github.com/acme/lets/releases/latest did not redirect to a release tag"
        );
    }

    #[test]
    fn a_response_with_no_location_header_is_update_failed() {
        let detail = update_failed_detail(latest_from("printf 'HTTP/2 200\\n'").map(|_| ()));

        assert!(
            detail.ends_with("did not redirect to a release tag"),
            "{detail}"
        );
    }

    #[test]
    fn a_tag_with_build_metadata_resolves_to_the_version_without_it() {
        let found = latest_from(
            "printf 'location: https://github.com/acme/lets/releases/tag/v1.2.3+build.1\\n'",
        )
        .unwrap();

        assert_eq!(found, version("1.2.3"));
    }

    #[test]
    fn a_tag_that_is_not_semver_is_update_failed_naming_the_tag() {
        for tag in [
            "v1.2",
            "v1.2.3.4",
            "nightly",
            "v1.x.0",
            "v1.2.3-",
            "v+1.2.3",
            "v01.2.3",
            "v1.0.0-.",
            "v1.0.0-rc..1",
            "v1.0.0-01",
            "v1.2.3+",
        ] {
            let detail = update_failed_detail(
                latest_from(&format!(
                    "printf 'location: https://github.com/acme/lets/releases/tag/{tag}\\n'"
                ))
                .map(|_| ()),
            );

            assert_eq!(
                detail,
                format!("the latest release tag `{tag}` is not vMAJOR.MINOR.PATCH")
            );
        }
    }

    #[test]
    fn versions_order_numerically_field_by_field_with_a_pre_release_before_its_release() {
        let ascending = [
            "0.0.1",
            "0.0.2",
            "0.1.0",
            "0.9.0",
            "0.10.0",
            "1.0.0-alpha",
            "1.0.0-rc.1",
            "1.0.0",
        ];
        for pair in ascending.windows(2) {
            assert!(
                version(pair[0]) < version(pair[1]),
                "{} < {}",
                pair[0],
                pair[1]
            );
        }
        assert_eq!(
            version("0.0.1").cmp(&version("0.0.1")),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn the_semver_spec_s_own_pre_release_chain_orders_correctly() {
        let ascending = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
        ];
        for pair in ascending.windows(2) {
            assert!(
                version(pair[0]) < version(pair[1]),
                "{} < {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn a_two_digit_pre_release_identifier_outranks_a_one_digit_one_numerically() {
        assert!(version("1.0.0-rc.9") < version("1.0.0-rc.10"));
    }

    #[test]
    fn text_ordering_of_the_same_pair_would_have_been_wrong() {
        assert!("rc.9" > "rc.10");
    }

    #[test]
    fn a_running_build_ahead_by_pre_release_precedence_never_looks_older() {
        let current = version("1.4.0-rc.10");
        let latest = version("1.4.0-rc.9");
        assert!(latest <= current, "update_available must stay false");
    }

    #[test]
    fn a_leading_zero_in_a_numeric_core_field_is_rejected() {
        assert!(Version::parse("01.2.3").is_none());
    }

    #[test]
    fn a_bare_zero_core_field_is_accepted() {
        assert!(Version::parse("0.2.3").is_some());
    }

    #[test]
    fn an_empty_pre_release_identifier_is_rejected() {
        for text in ["1.0.0-.", "1.0.0-rc..1"] {
            assert!(Version::parse(text).is_none(), "{text}");
        }
    }

    #[test]
    fn a_non_empty_pre_release_identifier_is_accepted() {
        assert!(Version::parse("1.0.0-rc.1").is_some());
    }

    #[test]
    fn a_leading_zero_in_a_numeric_pre_release_identifier_is_rejected() {
        assert!(Version::parse("1.0.0-01").is_none());
    }

    #[test]
    fn a_bare_zero_pre_release_identifier_is_accepted() {
        assert!(Version::parse("1.0.0-0").is_some());
    }

    #[test]
    fn build_metadata_is_accepted_and_ignored_for_comparison() {
        assert_eq!(version("1.2.3+build.1"), version("1.2.3"));
    }

    #[test]
    fn empty_build_metadata_is_rejected() {
        assert!(Version::parse("1.2.3+").is_none());
    }

    #[test]
    fn a_version_displays_without_a_v_and_with_its_pre_release() {
        assert_eq!(version("1.4.0-rc.1").to_string(), "1.4.0-rc.1");
        assert_eq!(version("0.10.0").to_string(), "0.10.0");
    }

    #[test]
    fn the_running_build_s_version_parses() {
        assert_eq!(Version::current().to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn a_failed_download_is_update_failed_naming_the_url_and_nothing_is_installed() {
        let bin = fake_curl("echo 'curl: (6) Could not resolve host' >&2; exit 6");
        let install_dir = TempDir::new().unwrap();

        let detail = update_failed_detail(download_and_install(
            URL,
            install_dir.path(),
            &bin.path().join("curl"),
        ));

        assert!(
            detail.starts_with(&format!("downloading {URL} failed")),
            "{detail}"
        );
        assert!(detail.ends_with("exit status: 6"), "{detail}");
        assert_eq!(std::fs::read_dir(install_dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_downloaded_installer_runs_with_the_install_dir_pointed_at_the_running_binary() {
        let bin = fake_curl(
            "printf '%s\\n' 'printf installed > \"$LETS_UNMANAGED_INSTALL/lets\"' > \"$out\"",
        );
        let install_dir = TempDir::new().unwrap();

        download_and_install(URL, install_dir.path(), &bin.path().join("curl")).unwrap();

        assert_eq!(
            std::fs::read_to_string(install_dir.path().join("lets")).unwrap(),
            "installed"
        );
    }

    #[test]
    fn an_installer_that_exits_non_zero_is_update_failed() {
        let bin = fake_curl("printf 'exit 3\\n' > \"$out\"");
        let install_dir = TempDir::new().unwrap();

        let detail = update_failed_detail(download_and_install(
            URL,
            install_dir.path(),
            &bin.path().join("curl"),
        ));

        assert!(
            detail.starts_with(&format!("the installer from {URL} failed")),
            "{detail}"
        );
        assert!(detail.ends_with("exit status: 3"), "{detail}");
    }
}
