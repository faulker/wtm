//! Self-update against the project's GitHub releases.
//!
//! The TUI kicks off [`check`] on a background thread at start so a slow or
//! offline network never delays the first frame, and prompts only once a newer
//! release actually comes back. [`install`] downloads the release asset built
//! for the running platform, verifies it against the release's checksum file,
//! and swaps it over the running binary.
//!
//! Everything here goes through the plain public release URLs, never
//! `api.github.com`: the API rate-limits anonymous callers per IP, which a
//! shared or office network exhausts easily, and a start-up check that fails
//! for everyone behind one NAT is worse than useless. `/releases/latest`
//! redirects to the newest tag, and the release workflow names its assets
//! predictably, so a single redirect lookup yields both the version and every
//! download URL.
//!
//! Network and hashing go through `curl` and `shasum`/`sha256sum` rather than
//! pulling in an HTTP or crypto crate, matching how the rest of wtm shells out
//! to `git`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

/// The version this binary was built as, used as the baseline for every check.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The repository releases are published to.
pub const REPO: &str = "faulker/wtm";

/// How long a check or download may take before it is abandoned. The check
/// runs unattended in the background, so it must never hang the app.
const CHECK_TIMEOUT_SECS: u32 = 10;
const DOWNLOAD_TIMEOUT_SECS: u32 = 300;

/// Name of the checksum manifest the release workflow publishes alongside the
/// per-platform tarballs.
const CHECKSUMS_ASSET: &str = "checksums-sha256.txt";

/// A published GitHub release, reduced to what an update needs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Release {
    /// Git tag, as published (e.g. `v0.2.0`).
    pub tag: String,
    /// The tag with any leading `v` removed, for comparing against
    /// [`CURRENT_VERSION`].
    pub version: String,
    /// Web page for the release, where the notes can be read.
    pub url: String,
}

impl Release {
    /// The release built from `tag`, with its asset URLs derived from the
    /// release workflow's naming convention.
    fn from_tag(tag: &str) -> Release {
        Release {
            tag: tag.to_string(),
            version: tag.trim_start_matches('v').to_string(),
            url: format!("https://github.com/{REPO}/releases/tag/{tag}"),
        }
    }

    /// File name of the tarball this release publishes for `triple`.
    pub fn asset_name(&self, triple: &str) -> String {
        format!("wtm-{}-{triple}.tar.gz", self.tag)
    }

    /// Download URL for one of this release's assets.
    pub fn asset_url(&self, name: &str) -> String {
        format!(
            "https://github.com/{REPO}/releases/download/{}/{name}",
            self.tag
        )
    }
}

/// The outcome of an update check, so callers can tell "nothing newer" apart
/// from "couldn't reach GitHub".
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CheckOutcome {
    /// A newer release than [`CURRENT_VERSION`] is published.
    Available(Release),
    /// The latest release is this version or older.
    UpToDate { latest: String },
}

/// Environment escape hatch that turns off the automatic check without editing
/// any config, for CI and offline use.
pub const DISABLE_ENV: &str = "WTM_NO_UPDATE_CHECK";

/// Whether this process looks like a local development binary rather than an
/// installed release. True for debug builds (`cargo run`, `./target/debug/…`)
/// and whenever Cargo itself launched us (`cargo run --release`, `cargo test`).
/// Those should not phone home for updates: the embedded version is whatever
/// is in Cargo.toml right now, not a published release.
pub fn is_dev_build() -> bool {
    cfg!(debug_assertions) || std::env::var_os("CARGO").is_some()
}

/// Whether the start-up check should run: the `auto_update_check` setting,
/// unless this is a dev build or [`DISABLE_ENV`] is set. An explicit
/// `wtm upgrade` or the Settings tab's check-now row ignores all three and
/// always checks.
pub fn auto_check_enabled(config: &crate::config::Config) -> bool {
    auto_check_enabled_for(
        config,
        is_dev_build(),
        std::env::var_os(DISABLE_ENV).is_some(),
    )
}

/// Pure form of [`auto_check_enabled`], so tests can pin the ambient inputs.
fn auto_check_enabled_for(
    config: &crate::config::Config,
    is_dev: bool,
    disable_env_set: bool,
) -> bool {
    !is_dev && !disable_env_set && config.auto_update_check()
}

/// Fetches the latest release and reports whether it is newer than the running
/// binary. A repository with no releases yet counts as up to date rather than
/// an error, since a 404 there is expected rather than a failure.
pub fn check() -> Result<CheckOutcome> {
    let Some(release) = latest_release()? else {
        return Ok(CheckOutcome::UpToDate {
            latest: CURRENT_VERSION.to_string(),
        });
    };
    if is_newer(CURRENT_VERSION, &release.version) {
        Ok(CheckOutcome::Available(release))
    } else {
        Ok(CheckOutcome::UpToDate {
            latest: release.version,
        })
    }
}

/// Fetches the repository's latest published release, or `None` when it has
/// none yet.
///
/// `https://github.com/<repo>/releases/latest` redirects to the newest tag's
/// page, so the tag falls out of the final URL without touching the rate-
/// limited API or parsing any HTML.
pub fn latest_release() -> Result<Option<Release>> {
    let url = format!("https://github.com/{REPO}/releases/latest");
    let resolved = resolve_redirect(&url, CHECK_TIMEOUT_SECS)?;
    Ok(tag_from_release_url(&resolved).map(|tag| Release::from_tag(&tag)))
}

/// Pulls the tag out of a resolved release URL. A repository with no releases
/// does not redirect (the URL stays on `/releases/latest` or lands on
/// `/releases`), which reads as `None` rather than an error.
fn tag_from_release_url(url: &str) -> Option<String> {
    let tag = url.rsplit_once("/releases/tag/")?.1;
    // Strip any query or fragment GitHub might append.
    let tag = tag.split(['?', '#']).next()?.trim_end_matches('/');
    (!tag.is_empty()).then(|| tag.to_string())
}

/// The release-asset target triple for the running platform, or `None` when
/// the release workflow publishes no build for it.
pub fn target_triple() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        _ => None,
    }
}

/// Compares two dotted version strings, reporting whether `candidate` is newer
/// than `current`. Missing components count as zero (`1.2` == `1.2.0`) and a
/// pre-release suffix (`1.2.0-rc.1`) sorts below the same version without one.
pub fn is_newer(current: &str, candidate: &str) -> bool {
    let (cur_core, cur_pre) = split_version(current);
    let (new_core, new_pre) = split_version(candidate);
    let len = cur_core.len().max(new_core.len());
    for i in 0..len {
        let a = cur_core.get(i).copied().unwrap_or(0);
        let b = new_core.get(i).copied().unwrap_or(0);
        if a != b {
            return b > a;
        }
    }
    // Same numeric version: only a release beats a pre-release of it.
    cur_pre && !new_pre
}

/// Splits `raw` into its numeric components and whether it carries a
/// pre-release suffix. Unparseable components become zero so a malformed tag
/// never looks newer than a real version.
fn split_version(raw: &str) -> (Vec<u64>, bool) {
    let raw = raw.trim().trim_start_matches('v');
    let core = raw.split(['-', '+']).next().unwrap_or("");
    let pre = raw.len() != core.len();
    let parts = core
        .split('.')
        .map(|p| p.trim().parse::<u64>().unwrap_or(0))
        .collect();
    (parts, pre)
}

/// What an applied update did, for the message shown afterwards.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Installed {
    pub version: String,
    /// The binary that was replaced, and which should be re-executed.
    pub path: PathBuf,
}

/// The wtm binary an update replaces: the running executable with symlinks
/// resolved, so an install writes through to the real file.
pub fn current_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot locate the running wtm binary")?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

/// Downloads `release`'s build for the running platform, verifies it against
/// the release checksums, and replaces the running binary with it.
///
/// The new binary is staged next to the old one and moved into place with a
/// rename, so an interrupted download can never leave a half-written `wtm` on
/// disk. On Unix, renaming over the running executable is safe: the running
/// process keeps its own open image.
pub fn install(release: &Release) -> Result<Installed> {
    let triple = target_triple().ok_or_else(|| {
        anyhow!(
            "no wtm release is built for {}-{}; update manually from {}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            release.url
        )
    })?;
    let asset = release.asset_name(triple);

    let exe = current_binary()?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("cannot locate the directory holding {}", exe.display()))?
        .to_path_buf();
    // Fail before downloading anything if the install location is read-only
    // (a Homebrew or /usr/local install owned by root), so the user gets the
    // real problem rather than a rename error at the very end.
    ensure_writable(&dir, &exe)?;

    let work = TempDir::new("wtm-update")?;
    let tarball = work.path.join(&asset);
    download(&release.asset_url(&asset), &tarball).with_context(|| {
        format!(
            "release {} has no {triple} build, or it could not be downloaded; \
             update manually from {}",
            release.tag, release.url
        )
    })?;
    verify_checksum(release, &asset, &tarball)?;

    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&work.path)
        .status()
        .context("failed to run tar; it must be on PATH to unpack an update")?;
    if !status.success() {
        bail!("could not unpack {asset}");
    }
    let unpacked = work.path.join("wtm");
    if !unpacked.is_file() {
        bail!("{asset} did not contain a wtm binary");
    }
    make_executable(&unpacked)?;
    verify_runs(&unpacked, &release.version)?;

    // Stage inside the destination directory so the final move is a rename on
    // the same filesystem, which is atomic; a cross-device rename would fail.
    let staged = dir.join(format!(".wtm-update-{}", std::process::id()));
    std::fs::copy(&unpacked, &staged)
        .with_context(|| format!("failed to stage the new binary at {}", staged.display()))?;
    make_executable(&staged)?;
    if let Err(e) = std::fs::rename(&staged, &exe) {
        let _ = std::fs::remove_file(&staged);
        return Err(e).with_context(|| format!("failed to replace {}", exe.display()));
    }
    Ok(Installed {
        version: release.version.clone(),
        path: exe,
    })
}

/// Errors unless a new binary can actually be written into `dir` over `exe`.
fn ensure_writable(dir: &Path, exe: &Path) -> Result<()> {
    let probe = dir.join(format!(".wtm-update-probe-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => bail!(
            "cannot write to {} ({e}); wtm was installed somewhere this user cannot modify. \
             Re-run with the right permissions, or reinstall {} by hand.",
            dir.display(),
            exe.display()
        ),
    }
}

/// Checks the downloaded tarball against the release's checksum manifest.
///
/// Every release built by the project workflow publishes one, so a missing or
/// silent manifest is treated as a broken release rather than a reason to
/// install an unverified binary.
fn verify_checksum(release: &Release, asset_name: &str, tarball: &Path) -> Result<()> {
    let text = http_get(&release.asset_url(CHECKSUMS_ASSET), CHECK_TIMEOUT_SECS)
        .with_context(|| format!("failed to download {CHECKSUMS_ASSET} for {}", release.tag))?;
    let Some(expected) = checksum_for(&text, asset_name) else {
        bail!("{CHECKSUMS_ASSET} does not list {asset_name}");
    };
    let actual = sha256(tarball)?;
    if !actual.eq_ignore_ascii_case(&expected) {
        bail!("checksum mismatch for {asset_name}: expected {expected}, got {actual}");
    }
    Ok(())
}

/// Pulls one file's hash out of a `shasum`-style manifest (`<hash>  <name>`).
fn checksum_for(manifest: &str, name: &str) -> Option<String> {
    manifest.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        // The name column may carry a `*` binary marker or a path prefix.
        let file = parts.next()?.trim_start_matches('*');
        (Path::new(file).file_name()?.to_str()? == name).then(|| hash.to_string())
    })
}

/// Hex SHA-256 of `path`, via whichever of the standard command-line hashers
/// this system has.
fn sha256(path: &Path) -> Result<String> {
    let attempts: [(&str, &[&str]); 2] = [("shasum", &["-a", "256"]), ("sha256sum", &[])];
    for (program, args) in attempts {
        let Ok(out) = Command::new(program).args(args).arg(path).output() else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        if let Some(hash) = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .map(str::to_string)
        {
            return Ok(hash);
        }
    }
    bail!("no checksum tool found; install `shasum` or `sha256sum` so updates can be verified")
}

/// Runs the freshly unpacked binary to confirm it executes on this machine and
/// really is the version the release claims, before it replaces a working one.
fn verify_runs(binary: &Path, version: &str) -> Result<()> {
    let out = Command::new(binary)
        .arg("--version")
        .output()
        .with_context(|| format!("the downloaded binary would not run ({})", binary.display()))?;
    if !out.status.success() {
        bail!("the downloaded binary would not run");
    }
    let reported = String::from_utf8_lossy(&out.stdout);
    if !reported.contains(version) {
        bail!("the downloaded binary reports {reported:?}, not version {version}");
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("failed to make {} executable", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// Replaces this process with `exe`, passing the same arguments through, so an
/// update takes effect without the user relaunching by hand.
pub fn restart(exe: &Path) -> Result<std::convert::Infallible> {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = Command::new(exe).args(&args).exec();
        Err(err).with_context(|| format!("failed to restart {}", exe.display()))
    }
    #[cfg(not(unix))]
    {
        let status = Command::new(exe)
            .args(&args)
            .status()
            .with_context(|| format!("failed to restart {}", exe.display()))?;
        std::process::exit(status.code().unwrap_or(0));
    }
}

/// Follows `url`'s redirects without downloading the body, and returns the URL
/// it ends on. Used to turn `/releases/latest` into the newest tag's page.
fn resolve_redirect(url: &str, timeout: u32) -> Result<String> {
    let out = Command::new("curl")
        .args(["--silent", "--show-error", "--location", "--fail"])
        // HEAD only: the release page's HTML is never needed, just its address.
        .arg("--head")
        .args(["--max-time", &timeout.to_string()])
        .args(["--header", &format!("User-Agent: wtm/{CURRENT_VERSION}")])
        .args(["--output", "/dev/null"])
        .args(["--write-out", "%{url_effective}"])
        .arg(url)
        .output()
        .context("failed to run curl; it must be on PATH to check for updates")?;
    if !out.status.success() {
        bail!(
            "could not reach {url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// GETs `url` and returns the body, following redirects and failing on any
/// non-success status.
fn http_get(url: &str, timeout: u32) -> Result<String> {
    let out = Command::new("curl")
        .args(["--silent", "--show-error", "--location", "--fail"])
        .args(["--max-time", &timeout.to_string()])
        .args(["--header", &format!("User-Agent: wtm/{CURRENT_VERSION}")])
        .arg(url)
        .output()
        .context("failed to run curl; it must be on PATH to check for updates")?;
    if !out.status.success() {
        bail!(
            "could not reach {url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Downloads `url` to `dest`.
fn download(url: &str, dest: &Path) -> Result<()> {
    let out = Command::new("curl")
        .args(["--silent", "--show-error", "--location", "--fail"])
        .args(["--max-time", &DOWNLOAD_TIMEOUT_SECS.to_string()])
        .args(["--header", &format!("User-Agent: wtm/{CURRENT_VERSION}")])
        .arg("--output")
        .arg(dest)
        .arg(url)
        .output()
        .context("failed to run curl; it must be on PATH to download an update")?;
    if !out.status.success() {
        bail!(
            "failed to download {url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// A scratch directory removed when it goes out of scope, so a failed update
/// leaves nothing behind.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Result<TempDir> {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", std::process::id()));
        // A leftover directory from a previous run of the same pid would mix
        // old files into this update; start clean.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        Ok(TempDir { path })
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_versions_are_detected() {
        assert!(is_newer("0.1.0", "0.1.1"));
        assert!(is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.9.9", "1.0.0"));
        assert!(is_newer("1.2.3", "1.10.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("1.0.0", "0.9.9"));
    }

    #[test]
    fn version_comparison_ignores_v_prefix_and_pads() {
        assert!(is_newer("v0.1.0", "v0.1.1"));
        assert!(!is_newer("0.1", "0.1.0"));
        assert!(is_newer("0.1", "0.1.1"));
    }

    #[test]
    fn prereleases_sort_below_the_release() {
        assert!(is_newer("1.0.0-rc.1", "1.0.0"));
        assert!(!is_newer("1.0.0", "1.0.0-rc.1"));
        assert!(is_newer("0.9.0", "1.0.0-rc.1"));
    }

    #[test]
    fn unparseable_versions_never_look_newer() {
        assert!(!is_newer("0.1.0", "not-a-version"));
        assert!(!is_newer("0.1.0", ""));
    }

    #[test]
    fn a_release_derives_its_urls_from_its_tag() {
        let release = Release::from_tag("v1.2.0");
        assert_eq!(release.tag, "v1.2.0");
        assert_eq!(release.version, "1.2.0");
        assert_eq!(
            release.url,
            "https://github.com/faulker/wtm/releases/tag/v1.2.0"
        );
        // The names and URLs must match what the release workflow publishes.
        assert_eq!(
            release.asset_name("aarch64-apple-darwin"),
            "wtm-v1.2.0-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            release.asset_url("wtm-v1.2.0-aarch64-apple-darwin.tar.gz"),
            "https://github.com/faulker/wtm/releases/download/v1.2.0/\
             wtm-v1.2.0-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            release.asset_url(CHECKSUMS_ASSET),
            "https://github.com/faulker/wtm/releases/download/v1.2.0/checksums-sha256.txt"
        );
    }

    #[test]
    fn the_tag_is_read_out_of_the_redirect_target() {
        assert_eq!(
            tag_from_release_url("https://github.com/faulker/wtm/releases/tag/v1.2.0").as_deref(),
            Some("v1.2.0")
        );
        // Trailing slashes, queries, and fragments must not end up in the tag.
        assert_eq!(
            tag_from_release_url("https://github.com/faulker/wtm/releases/tag/v1.2.0/").as_deref(),
            Some("v1.2.0")
        );
        assert_eq!(
            tag_from_release_url("https://github.com/faulker/wtm/releases/tag/v2.0.0?x=1")
                .as_deref(),
            Some("v2.0.0")
        );
        // Tags need not start with `v`.
        assert_eq!(
            tag_from_release_url("https://github.com/faulker/wtm/releases/tag/2026.07.1")
                .as_deref(),
            Some("2026.07.1")
        );
    }

    #[test]
    fn a_repo_with_no_releases_reads_as_no_release() {
        // Without a release to redirect to, GitHub leaves the URL on the
        // listing page, which must read as "nothing published" not as a tag.
        for url in [
            "https://github.com/faulker/wtm/releases",
            "https://github.com/faulker/wtm/releases/latest",
            "https://github.com/faulker/wtm/releases/tag/",
        ] {
            assert!(tag_from_release_url(url).is_none(), "{url}");
        }
    }

    #[test]
    fn checksum_manifest_is_matched_by_file_name() {
        let manifest = "\
aaa111  wtm-v1.0.0-aarch64-apple-darwin.tar.gz
bbb222 *wtm-v1.0.0-x86_64-unknown-linux-gnu.tar.gz
ccc333  ./nested/wtm-v1.0.0-aarch64-unknown-linux-gnu.tar.gz
";
        assert_eq!(
            checksum_for(manifest, "wtm-v1.0.0-aarch64-apple-darwin.tar.gz").as_deref(),
            Some("aaa111")
        );
        assert_eq!(
            checksum_for(manifest, "wtm-v1.0.0-x86_64-unknown-linux-gnu.tar.gz").as_deref(),
            Some("bbb222")
        );
        assert_eq!(
            checksum_for(manifest, "wtm-v1.0.0-aarch64-unknown-linux-gnu.tar.gz").as_deref(),
            Some("ccc333")
        );
        assert!(checksum_for(manifest, "wtm-v1.0.0-mystery.tar.gz").is_none());
    }

    #[test]
    fn target_triple_is_known_on_supported_platforms() {
        // The release workflow builds macOS and Linux on both architectures;
        // anywhere else must report no triple rather than guess one.
        let expected = matches!(std::env::consts::OS, "macos" | "linux")
            && matches!(std::env::consts::ARCH, "x86_64" | "aarch64");
        assert_eq!(target_triple().is_some(), expected);
    }

    #[test]
    fn temp_dir_is_removed_on_drop() {
        let path = {
            let dir = TempDir::new("wtm-update-test").unwrap();
            std::fs::write(dir.path.join("f"), "x").unwrap();
            dir.path.clone()
        };
        assert!(!path.exists());
    }

    #[test]
    fn auto_check_skips_dev_builds_even_when_config_allows() {
        let cfg = crate::config::Config::default();
        assert!(cfg.auto_update_check());
        assert!(!auto_check_enabled_for(&cfg, true, false));
        assert!(!auto_check_enabled_for(&cfg, false, true));
        assert!(!auto_check_enabled_for(&cfg, true, true));
    }

    #[test]
    fn auto_check_runs_for_installed_release_builds() {
        let cfg = crate::config::Config::default();
        assert!(auto_check_enabled_for(&cfg, false, false));

        let off = crate::config::Config {
            auto_update_check: Some(false),
            ..Default::default()
        };
        assert!(!auto_check_enabled_for(&off, false, false));
    }

    #[test]
    fn cargo_run_and_debug_builds_count_as_dev() {
        // This suite itself runs under cargo (CARGO set) as a debug binary, so
        // a local `cargo run` looks the same from here.
        assert!(is_dev_build());
        const {
            assert!(cfg!(debug_assertions));
        }
        assert!(std::env::var_os("CARGO").is_some());
    }
}
