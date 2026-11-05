//! `nyq upgrade`: find the newest GitHub release on the selected channel,
//! download the archive for this platform, verify it against the release's
//! SHA256SUMS and atomically replace the running executable. The same steps
//! back the background auto-upgrade of interactive sessions (`auto_upgrade`).

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const REPO: &str = "Znskr/nyq";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const STATE_FILE: &str = "update.json";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// Published, non-prerelease versions.
    #[default]
    Stable,
    /// The newest release, prereleases included.
    Latest,
}

impl Channel {
    pub fn label(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Latest => "latest",
        }
    }
}

// ---------------------------------------------------------------- versions

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Ident {
    // Declared first: numeric identifiers sort below alphanumeric ones.
    Num(u64),
    Alpha(String),
}

/// A semantic version (`v` prefix and `+build` metadata ignored).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    core: [u64; 3],
    pre: Vec<Ident>,
}

impl Version {
    pub fn parse(raw: &str) -> Option<Version> {
        let raw = raw.trim();
        let raw = raw.strip_prefix('v').unwrap_or(raw);
        let raw = raw.split_once('+').map_or(raw, |(v, _)| v);
        let (core, pre) = raw
            .split_once('-')
            .map_or((raw, None), |(c, p)| (c, Some(p)));
        let mut parts = core.split('.');
        let mut nums = [0u64; 3];
        for n in &mut nums {
            let part = parts.next()?;
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            *n = part.parse().ok()?;
        }
        if parts.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(p) => p
                .split('.')
                .map(|id| match id.parse() {
                    _ if id.is_empty() => None,
                    Ok(n) if id.bytes().all(|b| b.is_ascii_digit()) => Some(Ident::Num(n)),
                    _ => Some(Ident::Alpha(id.to_string())),
                })
                .collect::<Option<_>>()?,
        };
        Some(Version { core: nums, pre })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Whether `candidate` is strictly newer than `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((Version::parse(candidate), Version::parse(current)), (Some(a), Some(b)) if a > b)
}

// ---------------------------------------------------------------- releases

#[derive(Clone, Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// The newest non-draft release on `channel`.
pub fn pick_release(releases: Vec<Release>, channel: Channel) -> Option<Release> {
    releases
        .into_iter()
        .filter(|r| !r.draft && (channel == Channel::Latest || !r.prerelease))
        .filter_map(|r| Version::parse(&r.tag_name).map(|v| (v, r)))
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, r)| r)
}

/// The release target triple for an OS/arch pair (as in `std::env::consts`).
pub fn target_for(os: &str, arch: &str) -> Option<&'static str> {
    Some(match (os, arch) {
        // Static musl builds run on any Linux distribution.
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-gnu",
        _ => return None,
    })
}

/// The release archive name for a target triple.
pub fn archive_name(target: &str) -> String {
    let ext = if target.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    format!("nyq-{target}.{ext}")
}

/// The expected SHA-256 for `name` in a `sha256sum`-style listing.
pub fn checksum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hex, file) = line.trim().split_once(char::is_whitespace)?;
        let file = file.trim_start();
        let file = file.strip_prefix('*').unwrap_or(file);
        (file == name && hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hex.to_ascii_lowercase())
    })
}

pub fn verify_sha256(bytes: &[u8], expected: &str) -> Result<()> {
    let actual: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("checksum mismatch: expected {expected}, got {actual}; the download may be incomplete or tampered with");
    }
    Ok(())
}

// ---------------------------------------------------------------- network

/// Reads public GitHub releases. A `GITHUB_TOKEN`/`GH_TOKEN` in the
/// environment is sent with release-list requests to lift the anonymous rate
/// limit; downloads go to the public asset links and never carry it.
pub(crate) struct Github {
    client: reqwest::Client,
    token: Option<String>,
}

fn env_token() -> Option<String> {
    ["GITHUB_TOKEN", "GH_TOKEN"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|t| !t.trim().is_empty()))
}

impl Github {
    pub(crate) fn new(timeout: Duration) -> Result<Github> {
        let client = crate::http::builder()
            .user_agent(crate::config::USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(timeout)
            .build()?;
        Ok(Github {
            client,
            token: env_token(),
        })
    }

    /// The newest releases of nyq, newest first.
    pub(crate) async fn releases(&mut self) -> Result<Vec<Release>> {
        let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=30");
        let mut req = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.context("contacting GitHub")?;
        match resp.status().as_u16() {
            200 => Ok(resp.json().await.context("reading the release list")?),
            403 | 429 => bail!(
                "GitHub rate-limited the release check (HTTP {}): retry later or set GITHUB_TOKEN",
                resp.status().as_u16()
            ),
            s => bail!("GitHub answered HTTP {s} for the releases of {REPO}"),
        }
    }

    async fn download(&self, asset: &Asset) -> Result<Vec<u8>> {
        let resp = self
            .client
            .get(&asset.browser_download_url)
            .header("Accept", "application/octet-stream")
            .send()
            .await
            .with_context(|| format!("downloading {}", asset.name))?;
        anyhow::ensure!(
            resp.status().is_success(),
            "downloading {} failed: HTTP {}",
            asset.name,
            resp.status()
        );
        Ok(resp
            .bytes()
            .await
            .with_context(|| format!("downloading {}", asset.name))?
            .to_vec())
    }
}

// ---------------------------------------------------------------- state

#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    channel: Option<Channel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checked_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    latest: Option<String>,
}

fn load(home: &Path) -> State {
    std::fs::read(home.join(STATE_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save(home: &Path, state: &State) -> Result<()> {
    crate::config::write_private(&home.join(STATE_FILE), &serde_json::to_vec_pretty(state)?)
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The release channel saved by `nyq upgrade --channel` (default: stable).
pub fn saved_channel(home: &Path) -> Channel {
    load(home).channel.unwrap_or_default()
}

/// Remember the result of a release check.
pub(crate) fn record_check(home: &Path, latest: Option<&str>) {
    let mut state = load(home);
    state.checked_at = Some(now());
    if let Some(latest) = latest {
        state.latest = Some(latest.to_string());
    }
    let _ = save(home, &state);
}

// ---------------------------------------------------------------- install

/// The path of the running executable, with symlinks resolved.
pub(crate) fn current_exe() -> Result<PathBuf> {
    let exe =
        std::env::current_exe().context("could not determine the path of the running binary")?;
    let exe = match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(stripped) => PathBuf::from(stripped),
        None => exe,
    };
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

/// Where Windows parks the running executable while the new one takes its place.
fn aside_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    exe.with_file_name(name)
}

/// Rename the current file aside, then move the replacement in; restore on failure.
/// Needed on Windows, where a running executable cannot be overwritten but can be renamed.
fn swap_aside(staged: &Path, exe: &Path) -> std::io::Result<()> {
    let old = aside_path(exe);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old)?;
    if let Err(e) = std::fs::rename(staged, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(e);
    }
    Ok(())
}

/// Replace `exe` with `new_bin`: stage a copy next to it (same filesystem),
/// let `check` vet the staged file, then rename it over the original.
pub fn replace_exe(
    new_bin: &Path,
    exe: &Path,
    check: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let dir = exe
        .parent()
        .context("the executable has no parent directory")?;
    let staged = dir.join(format!(
        ".nyq-upgrade-{}{}",
        uuid::Uuid::new_v4(),
        std::env::consts::EXE_SUFFIX
    ));
    let result = (|| -> Result<()> {
        std::fs::copy(new_bin, &staged)
            .with_context(|| format!("cannot write to {} (permission denied?)", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        }
        check(&staged)?;
        let swapped = if cfg!(windows) {
            swap_aside(&staged, exe)
        } else {
            std::fs::rename(&staged, exe)
        };
        swapped.with_context(|| format!("failed to replace {} (permission denied?)", exe.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

/// Make sure the staged binary runs and reports the expected version.
fn check_binary(path: &Path, version: &str) -> Result<()> {
    let out = Command::new(path)
        .arg("--version")
        .output()
        .context("the downloaded binary does not run")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let reported = text
        .trim()
        .strip_prefix("nyq ")
        .context("the downloaded binary is not nyq")?;
    anyhow::ensure!(
        Version::parse(reported) == Version::parse(version),
        "the downloaded binary reports version {reported}, expected {version}"
    );
    Ok(())
}

/// Find the `nyq` executable in an extracted archive (top level or one directory down).
fn find_binary(dir: &Path) -> Option<PathBuf> {
    let name = format!("nyq{}", std::env::consts::EXE_SUFFIX);
    let top = dir.join(&name);
    if top.is_file() {
        return Some(top);
    }
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path().join(&name))
        .find(|p| p.is_file())
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn extract(archive: &Path, dest: &Path) -> Result<()> {
    // bsdtar (bundled with Windows 10+) also reads zip archives.
    let flags = if archive.extension().is_some_and(|e| e == "zip") {
        "-xf"
    } else {
        "-xzf"
    };
    let status = Command::new("tar")
        .arg(flags)
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .status()
        .context("running tar")?;
    anyhow::ensure!(status.success(), "failed to extract {}", archive.display());
    Ok(())
}

// ---------------------------------------------------------------- commands

/// The release archive name for this platform, if nyq ships one.
pub(crate) fn platform_archive() -> Option<String> {
    target_for(std::env::consts::OS, std::env::consts::ARCH).map(archive_name)
}

/// The newest release on `channel` and its version (without the `v`).
pub(crate) async fn newest(gh: &mut Github, channel: Channel) -> Result<(Release, String)> {
    let release = pick_release(gh.releases().await?, channel)
        .with_context(|| format!("{REPO} has no {} release yet", channel.label()))?;
    let latest = release.tag_name.trim_start_matches('v').to_string();
    Ok((release, latest))
}

/// Download the archive `name` of `release` and verify it against the
/// release's SHA256SUMS.
pub(crate) async fn download_verified(
    gh: &Github,
    release: &Release,
    name: &str,
) -> Result<Vec<u8>> {
    let asset = release
        .asset(name)
        .with_context(|| format!("release {} has no asset named {name}", release.tag_name))?;
    let sums = release.asset("SHA256SUMS").with_context(|| {
        format!(
            "release {} has no SHA256SUMS: refusing to install without checksum verification",
            release.tag_name
        )
    })?;
    let sums = String::from_utf8_lossy(&gh.download(sums).await?).into_owned();
    let expected =
        checksum_for(&sums, name).with_context(|| format!("SHA256SUMS has no entry for {name}"))?;
    let bytes = gh.download(asset).await?;
    verify_sha256(&bytes, &expected)?;
    Ok(bytes)
}

/// Unpack a verified archive and install its `nyq` over `exe`, after checking
/// that it runs and reports `version`. Blocking (runs `tar` and the binary).
pub(crate) fn install_archive(bytes: &[u8], name: &str, version: &str, exe: &Path) -> Result<()> {
    let _ = std::fs::remove_file(aside_path(exe)); // left over from a previous Windows upgrade
    let tmp = TempDir(std::env::temp_dir().join(format!("nyq-upgrade-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir_all(&tmp.0)?;
    let archive = tmp.0.join(name);
    std::fs::write(&archive, bytes)?;
    let unpacked = tmp.0.join("unpacked");
    std::fs::create_dir_all(&unpacked)?;
    extract(&archive, &unpacked)?;
    let new_bin =
        find_binary(&unpacked).with_context(|| format!("{name} does not contain a nyq binary"))?;
    replace_exe(&new_bin, exe, |staged| check_binary(staged, version))
}

/// `nyq upgrade [--channel stable|latest] [--check] [--json]`.
pub async fn run(home: &Path, channel: Option<Channel>, check: bool, json: bool) -> Result<()> {
    let mut state = load(home);
    if let Some(c) = channel.filter(|c| state.channel != Some(*c)) {
        state.channel = Some(c);
        save(home, &state)?;
    }
    let channel = state.channel.unwrap_or_default();
    let name = platform_archive()
        .context("no prebuilt nyq release exists for this platform; build from source instead")?;
    let report = |status: &str, latest: &str, text: String| {
        if json {
            let value = serde_json::json!({
                "status": status,
                "current": CURRENT,
                "latest": latest,
                "channel": channel.label(),
            });
            println!("{value}");
        } else {
            println!("{text}");
        }
    };

    eprintln!("Checking for updates ({} channel)...", channel.label());
    let mut gh = Github::new(Duration::from_secs(300))?;
    let (release, latest) = newest(&mut gh, channel).await?;
    record_check(home, Some(&latest));
    if !is_newer(&latest, CURRENT) {
        report(
            "up_to_date",
            &latest,
            format!("nyq {CURRENT} is up to date ({} channel).", channel.label()),
        );
        return Ok(());
    }
    if check {
        let text = format!(
            "Update available: nyq {CURRENT} -> {latest} ({} channel). Run `nyq upgrade` to install it.",
            channel.label()
        );
        report("available", &latest, text);
        return Ok(());
    }

    let exe = current_exe()?;
    eprintln!("Downloading {name} ({})...", release.tag_name);
    let bytes = download_verified(&gh, &release, &name).await?;
    install_archive(&bytes, &name, &latest, &exe)?;
    report(
        "upgraded",
        &latest,
        format!("Upgraded nyq {CURRENT} -> {latest} ({}).", exe.display()),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, prerelease: bool, assets: &[&str]) -> Release {
        Release {
            tag_name: tag.into(),
            prerelease,
            draft: false,
            assets: assets
                .iter()
                .map(|n| Asset {
                    name: n.to_string(),
                    browser_download_url: format!("dl/{n}"),
                })
                .collect(),
        }
    }

    #[test]
    fn versions_compare_by_semver() {
        assert!(is_newer("v0.4.0", "0.3.9"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(is_newer("1.0.0", "1.0.0-rc.1"));
        assert!(is_newer("1.0.0-rc.2", "1.0.0-rc.1"));
        assert!(is_newer("1.0.0-rc.10", "1.0.0-rc.9"));
        assert!(is_newer("1.0.0-beta", "1.0.0-alpha.5"));
        assert!(is_newer("1.0.0-alpha.beta", "1.0.0-alpha.1"));
        assert!(!is_newer("0.3.0", "0.3.0+build.7"));
        assert!(!is_newer("0.2.9", "0.3.0"));
        assert!(!is_newer("garbage", "0.1.0"));
        assert!(
            Version::parse("1.2").is_none()
                && Version::parse("1.2.3.4").is_none()
                && Version::parse("1.2.3-").is_none()
        );
    }

    #[test]
    fn releases_are_picked_per_channel() {
        let mut draft = release("v9.0.0", false, &[]);
        draft.draft = true;
        let all = vec![
            release("v0.3.0", false, &[]),
            release("v0.5.0-rc.1", true, &[]),
            release("v0.4.0", false, &[]),
            draft,
            release("nightly", false, &[]),
        ];
        assert_eq!(
            pick_release(all.clone(), Channel::Stable).unwrap().tag_name,
            "v0.4.0"
        );
        assert_eq!(
            pick_release(all, Channel::Latest).unwrap().tag_name,
            "v0.5.0-rc.1"
        );
        assert!(pick_release(vec![release("v1.0.0-rc.1", true, &[])], Channel::Stable).is_none());
    }

    #[test]
    fn assets_are_selected_per_target() {
        assert_eq!(
            target_for("linux", "x86_64"),
            Some("x86_64-unknown-linux-musl")
        );
        assert_eq!(target_for("macos", "aarch64"), Some("aarch64-apple-darwin"));
        assert_eq!(
            target_for("windows", "x86_64"),
            Some("x86_64-pc-windows-gnu")
        );
        assert_eq!(target_for("freebsd", "x86_64"), None);
        assert_eq!(
            archive_name("aarch64-unknown-linux-musl"),
            "nyq-aarch64-unknown-linux-musl.tar.gz"
        );
        assert_eq!(
            archive_name("x86_64-pc-windows-gnu"),
            "nyq-x86_64-pc-windows-gnu.zip"
        );
        let r = release(
            "v1.0.0",
            false,
            &[
                "nyq-x86_64-apple-darwin.tar.gz",
                "nyq-aarch64-apple-darwin.tar.gz",
                "SHA256SUMS",
            ],
        );
        let a = r
            .asset(&archive_name(target_for("macos", "aarch64").unwrap()))
            .unwrap();
        assert_eq!(a.browser_download_url, "dl/nyq-aarch64-apple-darwin.tar.gz");
        assert!(r.asset("nyq-x86_64-pc-windows-gnu.zip").is_none());
        // The running platform maps to a target wherever nyq ships binaries.
        if cfg!(all(
            any(target_os = "linux", target_os = "macos"),
            any(target_arch = "x86_64", target_arch = "aarch64")
        )) {
            assert!(target_for(std::env::consts::OS, std::env::consts::ARCH).is_some());
        }
    }

    #[test]
    fn checksums_are_parsed_and_verified() {
        let data = b"hello nyq";
        let hex: String = Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let sums = format!(
            "{}  nyq-a.tar.gz\n{hex}  nyq-b.tar.gz\n{} *nyq-c.zip\nshort  nyq-d.tar.gz\n",
            "0".repeat(64),
            "A".repeat(64)
        );
        assert_eq!(
            checksum_for(&sums, "nyq-b.tar.gz").as_deref(),
            Some(hex.as_str())
        );
        assert_eq!(checksum_for(&sums, "nyq-c.zip"), Some("a".repeat(64)));
        assert_eq!(checksum_for(&sums, "nyq-d.tar.gz"), None);
        assert_eq!(checksum_for(&sums, "nyq-b.tar"), None);
        verify_sha256(data, &hex).unwrap();
        verify_sha256(data, &hex.to_uppercase()).unwrap();
        assert!(verify_sha256(b"tampered", &hex)
            .unwrap_err()
            .to_string()
            .contains("checksum mismatch"));
    }

    #[test]
    fn executable_is_replaced_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("nyq");
        let new = tmp.path().join("download");
        std::fs::write(&exe, "old").unwrap();
        std::fs::write(&new, "new").unwrap();
        replace_exe(&new, &exe, |staged| {
            assert_eq!(staged.parent(), Some(tmp.path()));
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        // A failed check leaves the original alone and cleans up the staged copy.
        std::fs::write(&new, "broken").unwrap();
        assert!(replace_exe(&new, &exe, |_| bail!("does not run")).is_err());
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        let staged = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".nyq-upgrade-")
            })
            .count();
        assert_eq!(staged, 0);
    }

    #[test]
    fn windows_style_swap_moves_the_running_file_aside() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("nyq.exe");
        let staged = tmp.path().join("staged");
        std::fs::write(&exe, "old").unwrap();
        std::fs::write(&staged, "new").unwrap();
        std::fs::write(aside_path(&exe), "stale").unwrap();
        swap_aside(&staged, &exe).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("nyq.exe.old")).unwrap(),
            "old"
        );
        // A missing replacement restores the original.
        assert!(swap_aside(&tmp.path().join("missing"), &exe).is_err());
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
    }

    #[test]
    fn binaries_are_found_in_extracted_archives() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find_binary(tmp.path()).is_none());
        let name = format!("nyq{}", std::env::consts::EXE_SUFFIX);
        std::fs::create_dir(tmp.path().join("pkg")).unwrap();
        std::fs::write(tmp.path().join("pkg").join(&name), "x").unwrap();
        assert_eq!(
            find_binary(tmp.path()).unwrap(),
            tmp.path().join("pkg").join(&name)
        );
        std::fs::write(tmp.path().join(&name), "x").unwrap();
        assert_eq!(find_binary(tmp.path()).unwrap(), tmp.path().join(&name));
    }

    #[test]
    fn state_round_trips_privately() {
        let tmp = tempfile::tempdir().unwrap();
        record_check(tmp.path(), Some("9.9.9"));
        let state = load(tmp.path());
        assert_eq!(state.latest.as_deref(), Some("9.9.9"));
        assert!(state.checked_at.is_some() && state.channel.is_none());
        // A failed check keeps the last known version.
        record_check(tmp.path(), None);
        assert_eq!(load(tmp.path()).latest.as_deref(), Some("9.9.9"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(tmp.path().join(STATE_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
