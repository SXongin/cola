//! Self-update (ADR-0015): check GitHub Releases for a newer cola, download the
//! asset for the current platform, verify it against the release's `SHA256SUMS`,
//! atomically replace the running binary, and restart.
//!
//! The update channel is GitHub Releases only — the same binaries `release.yml`
//! builds. crates.io publishing is deferred (the `cola` name is taken); `cargo
//! install`/`cargo-binstall` remain future conveniences and do not change this
//! module. The binary's embedded version must equal the release tag
//! (guarded in `release.yml`), or the semver compare reports "update available"
//! forever.

use std::path::{Path, PathBuf};

use anyhow::Context;
use semver::Version;
use sha2::{Digest, Sha256};

/// Exit code used to hand a systemd unit back to `Restart=on-failure` after the
/// binary has been replaced. Under systemd, spawning a child is wrong: the
/// unit's default `KillMode=control-group` kills the whole cgroup — including a
/// spawned replacement — when the main process exits.
pub const EXIT_UPDATE_RESTART: i32 = 3;

/// The current version of the running binary (from Cargo.toml).
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo.toml version must be semver")
}

/// The release-asset platform triple cola ships for this platform, matching the
/// `release.yml` build matrix. `None` on platforms with no prebuilt asset.
pub fn platform_triple() -> Option<&'static str> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        Some("x86_64-unknown-linux-gnu")
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Some("aarch64-apple-darwin")
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        Some("x86_64-pc-windows-msvc")
    }
    #[cfg(not(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "windows", target_arch = "x86_64"),
    )))]
    {
        None
    }
}

/// The result of a release check.
#[derive(Debug)]
pub enum UpdateCheck {
    UpToDate,
    /// A newer release exists but this platform has no prebuilt asset.
    NoAssetForPlatform {
        latest: Version,
    },
    /// A newer release exists and this platform has a matching asset.
    Available(UpdateInfo),
}

/// What an update check found and where to fetch it.
#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub current: Version,
    pub latest: Version,
    pub asset_name: String,
    pub asset_url: String,
    pub sha256_url: String,
}

#[derive(serde::Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(serde::Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

const RELEASES_LATEST_URL: &str = "https://api.github.com/repos/SXongin/cola/releases/latest";

/// Query the latest GitHub release and decide whether an update is available.
pub async fn check() -> anyhow::Result<UpdateCheck> {
    let current = current_version();
    let client = reqwest::Client::new();
    let release = client
        .get(RELEASES_LATEST_URL)
        .header(reqwest::header::USER_AGENT, "cola-self-update")
        .send()
        .await
        .context("query GitHub releases/latest")?
        .error_for_status()
        .context("GitHub releases/latest returned an error")?
        .json::<Release>()
        .await
        .context("parse GitHub releases/latest response")?;

    // Tags are strict semver without a v prefix (release.yml), but tolerate one.
    let latest = Version::parse(release.tag_name.trim_start_matches('v'))
        .with_context(|| format!("release tag `{}` is not semver", release.tag_name))?;
    if latest <= current {
        return Ok(UpdateCheck::UpToDate);
    }

    let Some(triple) = platform_triple() else {
        return Ok(UpdateCheck::NoAssetForPlatform { latest });
    };
    let asset = select_asset(&release.assets, triple)
        .with_context(|| format!("release {latest} has no asset for platform {triple}"))?;
    let sha = release
        .assets
        .iter()
        .find(|a| a.name == "SHA256SUMS")
        .context("release has no SHA256SUMS")?;
    Ok(UpdateCheck::Available(UpdateInfo {
        current,
        latest,
        asset_name: asset.name.clone(),
        asset_url: asset.browser_download_url.clone(),
        sha256_url: sha.browser_download_url.clone(),
    }))
}

/// The release asset whose name contains `triple` (e.g. `x86_64-unknown-linux-gnu`).
fn select_asset<'a>(assets: &'a [ReleaseAsset], triple: &str) -> Option<&'a ReleaseAsset> {
    assets.iter().find(|a| a.name.contains(triple))
}

/// The lower-case hex sha256 digest of a byte slice.
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// The expected sha256 hex for `asset_name` in a `sha256sum`-format file
/// (`<hex>  <name>`, one per line — what `release.yml` writes).
fn expected_checksum(sums: &str, asset_name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hex = parts.next()?;
        let name = parts.next()?;
        (name == asset_name).then(|| hex.to_string())
    })
}

/// Download the release archive + `SHA256SUMS`, verify the archive's checksum,
/// extract the binary, and return its path (inside `dest_dir`).
pub async fn download_and_verify(info: &UpdateInfo, dest_dir: &Path) -> anyhow::Result<PathBuf> {
    let client = reqwest::Client::new();
    let archive = client
        .get(&info.asset_url)
        .header(reqwest::header::USER_AGENT, "cola-self-update")
        .send()
        .await
        .context("download release asset")?
        .error_for_status()?
        .bytes()
        .await
        .context("read release asset body")?;
    let sums = client
        .get(&info.sha256_url)
        .header(reqwest::header::USER_AGENT, "cola-self-update")
        .send()
        .await
        .context("download SHA256SUMS")?
        .error_for_status()?
        .text()
        .await
        .context("read SHA256SUMS body")?;

    let expected =
        expected_checksum(&sums, &info.asset_name).context("SHA256SUMS has no entry for the asset")?;
    let actual = sha256_hex(&archive);
    anyhow::ensure!(
        expected == actual,
        "sha256 mismatch for {}: expected {expected}, got {actual}",
        info.asset_name
    );

    extract_binary(&archive, dest_dir)
}

/// Extract the `cola` binary from the release archive into `dest_dir`.
#[cfg(not(target_os = "windows"))]
fn extract_binary(archive: &[u8], dest_dir: &Path) -> anyhow::Result<PathBuf> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    let entry = tar
        .entries()
        .context("list archive entries")?
        .filter_map(|e| e.ok())
        .find(|e| {
            e.path()
                .ok()
                .map(|p| p.to_string_lossy().contains("cola"))
                .unwrap_or(false)
        })
        .context("no `cola` entry in archive")?;
    let dest = dest_dir.join("cola");
    let mut out = std::fs::File::create(&dest).context("create new binary")?;
    let mut reader = entry;
    std::io::copy(&mut reader, &mut out).context("extract binary")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
            .context("make new binary executable")?;
    }
    Ok(dest)
}

/// Extract the `cola` binary from the release zip into `dest_dir`.
#[cfg(target_os = "windows")]
fn extract_binary(archive: &[u8], dest_dir: &Path) -> anyhow::Result<PathBuf> {
    use std::io::Read;

    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive)).context("open release zip")?;
    let mut dest: Option<PathBuf> = None;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).context("read zip entry")?;
        let name = file.name().to_string();
        if name.to_lowercase().contains("cola") && name.to_lowercase().ends_with(".exe") {
            let path = dest_dir.join("cola.exe");
            let mut out = std::fs::File::create(&path).context("create new binary")?;
            std::io::copy(&mut file, &mut out).context("extract binary")?;
            dest = Some(path);
            break;
        }
    }
    dest.context("no `cola` exe in release zip")
}

/// Atomically replace the running binary at `current_exe` with `new_binary`.
///
/// Unix: `rename` replaces atomically (the running process keeps the old inode).
/// Windows: a running exe cannot be overwritten, so the old file is renamed
/// aside first, then the new one moved into place, rolling back on failure.
pub fn install(new_binary: &Path, current_exe: &Path) -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    {
        let old = current_exe.with_extension("old.exe");
        std::fs::rename(current_exe, &old)
            .with_context(|| format!("rename running binary to {}", old.display()))?;
        match std::fs::rename(new_binary, current_exe) {
            Ok(()) => {
                let _ = std::fs::remove_file(&old);
                Ok(())
            }
            Err(e) => {
                let _ = std::fs::rename(&old, current_exe); // roll back
                Err(e).with_context(|| format!("move new binary into {}", current_exe.display()))
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::fs::rename(new_binary, current_exe)
            .with_context(|| format!("replace {}", current_exe.display()))?;
        Ok(())
    }
}

/// Restart cola after a successful in-band install (never returns).
///
/// Under a systemd unit (`INVOCATION_ID` is set) cola exits with
/// `EXIT_UPDATE_RESTART` and lets the unit's `Restart=on-failure` bring up the
/// new binary from the same ExecStart path. Everywhere else it re-execs via the
/// existing `restart_process()` (spawn with the original args + `--replace`,
/// then exit) — see ADR-0015. Only the in-band `/update` path calls this; the
/// `cola update` CLI replaces the binary and restarts via its supervisor (or
/// tells the operator — see [`restart_cli`]).
pub fn restart() -> ! {
    #[cfg(target_os = "linux")]
    if std::env::var_os("INVOCATION_ID").is_some() {
        std::process::exit(EXIT_UPDATE_RESTART);
    }
    match crate::bridge::command::restart_process() {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("cola update: restart spawn failed: {e}");
            std::process::exit(EXIT_UPDATE_RESTART);
        }
    }
}

/// After the CLI has replaced the binary, restart a RUNNING daemon through its
/// OS supervisor (systemd user unit / launchd agent) — a supervisor-mediated
/// restart stays supervised and does not die with the CLI's terminal.
///
/// Returns the message to print: `None` when the supervisor restarted the
/// daemon, otherwise a hint tailored to whether a daemon is running. `/restart`
/// in Feishu is only offered when `daemon_running` — a dead bot cannot answer.
pub fn restart_cli(daemon_running: bool) -> Option<String> {
    let supervisor = crate::autostart::supervisor_restart_command();
    if daemon_running {
        if let Some(cmd) = supervisor.as_ref()
            && matches!(
                std::process::Command::new("sh").args(["-c", cmd]).status(),
                Ok(s) if s.success()
            )
        {
            return None;
        }
        let via = supervisor.map(|c| format!("，或 {c}")).unwrap_or_default();
        Some(format!(
            "运行中的 cola 仍是旧版本，重启生效：在飞书里发 /restart{via}。"
        ))
    } else {
        Some("cola 尚未运行，更新已就绪。启动 cola 即生效。".to_string())
    }
}

/// How much of the update flow to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateMode {
    /// Report the situation; download/apply nothing.
    Check,
    /// Download, verify, install, and restart.
    Apply,
}

/// Where the update flow reports its progress: Feishu replies as text, the CLI
/// prints to stdout.
#[async_trait::async_trait]
pub trait UpdateReporter: Sync {
    async fn report(&self, msg: String);
}

#[derive(Debug, PartialEq, Eq)]
pub enum UpdateOutcome {
    UpToDate,
    NoAssetForPlatform,
    Available,
    /// Applied; carries the new version.
    Updated(Version),
    Failed,
}

/// Run the whole self-update flow against `reporter`. Returns whether an
/// update was applied; the caller decides whether to call [`restart`].
pub async fn run_update(reporter: &dyn UpdateReporter, mode: UpdateMode) -> UpdateOutcome {
    reporter.report("🔍 正在检查更新…".into()).await;
    match check().await {
        Err(e) => {
            tracing::warn!("update check failed: {e}");
            reporter.report(format!("❌ 检查更新失败：{e}")).await;
            UpdateOutcome::Failed
        }
        Ok(UpdateCheck::UpToDate) => {
            reporter
                .report(format!("✅ 已是最新版本（{}）。", current_version()))
                .await;
            UpdateOutcome::UpToDate
        }
        Ok(UpdateCheck::NoAssetForPlatform { latest }) => {
            reporter
                .report(format!(
                    "发现新版本 {latest}，但当前平台没有预编译二进制，请手动更新。"
                ))
                .await;
            UpdateOutcome::NoAssetForPlatform
        }
        Ok(UpdateCheck::Available(info)) => {
            if mode == UpdateMode::Check {
                reporter
                    .report(format!(
                        "发现新版本 {}（当前 {}）—— 仅检查，未应用。",
                        info.latest, info.current
                    ))
                    .await;
                return UpdateOutcome::Available;
            }
            reporter
                .report(format!(
                    "发现新版本 {}（当前 {}）→ 正在下载…",
                    info.latest, info.current
                ))
                .await;
            let exe = match std::env::current_exe() {
                Ok(exe) => exe,
                Err(err) => {
                    reporter.report(format!("❌ 无法定位当前可执行文件：{err}")).await;
                    return UpdateOutcome::Failed;
                }
            };
            let exe_dir = exe.parent().unwrap_or_else(|| Path::new("."));
            // Extract in the same directory as the binary so the final rename
            // stays on one filesystem (an EXDEV rename would fail).
            let tmp = match tempfile::tempdir_in(exe_dir) {
                Ok(tmp) => tmp,
                Err(err) => {
                    reporter.report(format!("❌ 无法创建临时目录：{err}")).await;
                    return UpdateOutcome::Failed;
                }
            };
            match download_and_verify(&info, tmp.path()).await {
                Err(e) => {
                    reporter.report(format!("❌ 下载或校验失败：{e}")).await;
                    UpdateOutcome::Failed
                }
                Ok(new_binary) => match install(&new_binary, &exe) {
                    Err(e) => {
                        reporter.report(format!("❌ 替换二进制失败：{e}")).await;
                        UpdateOutcome::Failed
                    }
                    Ok(()) => {
                        reporter.report(format!("✅ 已更新到 {}。", info.latest)).await;
                        UpdateOutcome::Updated(info.latest)
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_version_is_semver() {
        assert!(Version::parse(env!("CARGO_PKG_VERSION")).is_ok());
    }

    #[test]
    fn sha256_hex_matches_known_digest() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn expected_checksum_parses_sha256sum_format() {
        let sums = "abc123  cola-0.4.0-x86_64-unknown-linux-gnu.tar.gz\n\
                    def456  cola-0.4.0-aarch64-apple-darwin.tar.gz\n";
        assert_eq!(
            expected_checksum(sums, "cola-0.4.0-x86_64-unknown-linux-gnu.tar.gz"),
            Some("abc123".into())
        );
        assert_eq!(expected_checksum(sums, "nope"), None);
    }

    #[test]
    fn select_asset_matches_by_triple() {
        let assets = vec![
            ReleaseAsset {
                name: "cola-0.4.0-x86_64-unknown-linux-gnu.tar.gz".into(),
                browser_download_url: "u1".into(),
            },
            ReleaseAsset {
                name: "cola-0.4.0-aarch64-apple-darwin.tar.gz".into(),
                browser_download_url: "u2".into(),
            },
            ReleaseAsset {
                name: "SHA256SUMS".into(),
                browser_download_url: "u3".into(),
            },
        ];
        assert_eq!(
            select_asset(&assets, "x86_64-unknown-linux-gnu")
                .unwrap()
                .browser_download_url,
            "u1"
        );
        assert!(select_asset(&assets, "aarch64-unknown-linux-gnu").is_none());
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn extract_binary_reads_single_file_archive() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        {
            let mut tar = tar::Builder::new(&mut gz);
            let mut header = tar::Header::new_gnu();
            header.set_size(6);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, "cola", b"\x7fELF!\x01".as_slice())
                .unwrap();
            tar.finish().unwrap();
        }
        let archive = gz.finish().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let exe = extract_binary(&archive, dir.path()).unwrap();
        assert_eq!(exe.file_name().unwrap(), "cola");
        assert_eq!(std::fs::read(&exe).unwrap(), b"\x7fELF!\x01");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&exe).unwrap().permissions().mode() & 0o111,
                0o111
            );
        }
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn install_replaces_binary_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("cola");
        let new = dir.path().join("cola.new");
        std::fs::write(&current, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();
        install(&new, &current).unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), b"new");
        assert!(!new.exists(), "the new binary must be moved into place");
    }
}
