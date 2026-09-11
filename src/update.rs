//! Updates (ADR-0015, ADR-0030): check the channel the running binary was
//! installed through, then either apply the update or hand it to cargo.
//!
//! - GitHub Releases installs self-update: check `releases/latest`, download the
//!   asset for the current platform, verify it against the release's
//!   `SHA256SUMS`, atomically replace the running binary, and restart.
//! - A cargo-tracked install (`cargo install colark` / `cargo binstall colark`,
//!   detected from cargo's install receipt) is never replaced: availability is
//!   checked on the crates.io sparse index and the user is given the cargo
//!   command instead.
//!
//! The binary's embedded version must equal the release tag (guarded in
//! `release.yml`), or the semver compare reports "update available" forever.

use std::path::{Path, PathBuf};

use anyhow::Context;
use semver::Version;
use sha2::{Digest, Sha256};

/// Exit code used to hand a systemd unit back to `Restart=on-failure` after a
/// restart (`/restart`) or an in-band update (`/update`). Under systemd,
/// spawning a child is wrong: the unit's default `KillMode=control-group` kills
/// the whole cgroup — including a spawned replacement — when the main process
/// exits (ADR-0015, ADR-0021).
pub const EXIT_SUPERVISOR_RESTART: i32 = 3;

/// The exit code to use when this process is supervised by a systemd unit:
/// exit with it and let `Restart=on-failure` bring cola back up from the same
/// ExecStart. `None` when no supervisor owns this process — callers fall back
/// to the spawn-and-exit re-exec.
///
/// Supervision is detected by systemd's `INVOCATION_ID` AND `SYSTEMD_EXEC_PID`
/// pointing at THIS process. The two must be checked together: `INVOCATION_ID`
/// alone leaks into every child of a unit — a terminal under a unit (e.g.
/// OpenChamber's managed shell) hands it to the cola it runs as a foreground
/// process, so a bare `INVOCATION_ID` check made `/restart` exit(3) expecting
/// `Restart=on-failure` from a supervisor that does not own cola, leaving
/// nothing running until a manual start.
pub fn supervisor_restart_code() -> Option<i32> {
    #[cfg(target_os = "linux")]
    if std::env::var_os("INVOCATION_ID").is_some()
        && std::env::var_os("SYSTEMD_EXEC_PID").is_some_and(|p| p == std::process::id().to_string().as_str())
    {
        return Some(EXIT_SUPERVISOR_RESTART);
    }
    None
}

/// Which install channel a binary was installed through (ADR-0030). Decides
/// how updates are applied: self-update for GitHub Releases installs, the
/// cargo command for cargo-tracked installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallChannel {
    /// GitHub Releases archive or a source build: self-update applies.
    GitHub,
    /// Tracked by cargo's install receipt (`cargo install` / `cargo binstall`):
    /// the cargo command updates it.
    Cargo,
}

/// Detect the install channel of `exe` from cargo's install receipt (ADR-0030):
/// a binary in `<install root>/bin` whose install root carries a
/// `.crates2.json` entry listing that binary name. Best effort — no receipt (a
/// GitHub archive, a source build, `--no-track`, or binstall's local
/// `--install-path`) means the GitHub channel.
pub fn detect_install_channel(exe: &Path) -> InstallChannel {
    let Ok(exe) = exe.canonicalize() else {
        return InstallChannel::GitHub;
    };
    let Some(bin_dir) = exe.parent() else {
        return InstallChannel::GitHub;
    };
    // cargo always installs into `<root>/bin`; requiring it rules out a stray
    // receipt file elsewhere from classifying an arbitrary binary.
    if bin_dir.file_name().and_then(|n| n.to_str()) != Some("bin") {
        return InstallChannel::GitHub;
    }
    let Some(root) = bin_dir.parent() else {
        return InstallChannel::GitHub;
    };
    // On Windows the receipt lists `cola` while the binary is `cola.exe`, so
    // both the full file name and the stem are checked.
    let names = [exe.file_name(), exe.file_stem()].map(|n| n.and_then(|s| s.to_str()));
    if names.iter().flatten().any(|bin| receipt_has_binary(root, bin)) {
        InstallChannel::Cargo
    } else {
        InstallChannel::GitHub
    }
}

/// Whether `<install root>/.crates2.json` lists `bin` among the binaries cargo
/// installed. Every cargo new enough to build cola (edition 2024 ⇒ 1.85+)
/// writes this file — older cargo wrote only the legacy `.crates.toml`, which
/// cannot install cola anyway.
fn receipt_has_binary(install_root: &Path, bin: &str) -> bool {
    std::fs::read_to_string(install_root.join(".crates2.json"))
        .is_ok_and(|data| crates2_has_binary(&data, bin))
}

/// `.crates2.json` is `{"installs": {<pkg id>: {"bins": [<bin>...], ...}}}`.
fn crates2_has_binary(data: &str, bin: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
        return false;
    };
    v.get("installs")
        .and_then(|i| i.as_object())
        .is_some_and(|installs| {
            installs.values().any(|info| {
                info.get("bins")
                    .and_then(|b| b.as_array())
                    .is_some_and(|bins| bins.iter().any(|b| b.as_str() == Some(bin)))
            })
        })
}

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

const REPO: &str = "SXongin/cola";
const RELEASES_LATEST_URL: &str = "https://github.com/SXongin/cola/releases/latest";

/// Extract the release tag from the `Location` header of the `releases/latest`
/// redirect (e.g. `https://github.com/SXongin/cola/releases/tag/0.7.0` →
/// `0.7.0`). Handles absolute and relative targets; a `v` prefix is left for
/// the caller's semver parse (which tolerates it).
fn latest_tag_from_location(location: &str) -> Option<String> {
    if let Ok(url) = url::Url::parse(location) {
        let last = url
            .path_segments()
            .and_then(|mut segs| segs.rfind(|s| !s.is_empty()));
        if let Some(seg) = last {
            return Some(seg.to_string());
        }
    }
    // Relative `Location` (e.g. `/SXongin/cola/releases/tag/0.7.0`): the last
    // path segment of the raw string. Must look like a path — a bare word is
    // not a redirect target.
    location
        .strip_prefix('/')?
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The release asset name for a platform triple, following `release.yml`'s
/// packaging: `cola-<tag>-<triple>.tar.gz`, `.zip` on Windows.
fn asset_name(tag: &str, triple: &str) -> String {
    let ext = if triple.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    format!("cola-{tag}-{triple}.{ext}")
}

/// The `releases/download` URL for an asset — github.com (redirecting to the
/// asset CDN), not `api.github.com`, so downloads never touch the API quota.
fn download_url(tag: &str, asset_name: &str) -> String {
    format!("https://github.com/{REPO}/releases/download/{tag}/{asset_name}")
}

/// Query the latest GitHub release and decide whether an update is available.
///
/// Uses the HTML `releases/latest` redirect (github.com) instead of the
/// `api.github.com` endpoint, whose unauthenticated quota is 60 req/hr/IP:
/// GET with redirects disabled, read the tag from the `Location` header, and
/// build the asset URLs from the tag (`release.yml` names them
/// deterministically). Downloads are CDN-served and never count against the
/// quota, so the whole update flow stays API-free.
pub async fn check() -> anyhow::Result<UpdateCheck> {
    let current = current_version();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("build HTTP client")?;
    let resp = client
        .get(RELEASES_LATEST_URL)
        .header(reqwest::header::USER_AGENT, "cola-self-update")
        .send()
        .await
        .context("query GitHub releases/latest")?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("GitHub 上没有已发布的版本（releases 为空）。");
    }
    let resp = resp
        .error_for_status()
        .context("GitHub releases/latest returned an error")?;
    anyhow::ensure!(
        resp.status().is_redirection(),
        "GitHub releases/latest returned {} (expected a redirect)",
        resp.status()
    );
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .context("GitHub releases/latest did not redirect (no Location header)")?;
    let tag = latest_tag_from_location(location)
        .with_context(|| format!("cannot parse release tag from redirect target `{location}`"))?;
    // Tags are strict semver without a v prefix (release.yml), but tolerate one.
    let latest = Version::parse(tag.trim_start_matches('v'))
        .with_context(|| format!("release tag `{tag}` is not semver"))?;
    if latest <= current {
        return Ok(UpdateCheck::UpToDate);
    }

    let Some(triple) = platform_triple() else {
        return Ok(UpdateCheck::NoAssetForPlatform { latest });
    };
    let asset = asset_name(&tag, triple);
    Ok(UpdateCheck::Available(UpdateInfo {
        current,
        latest,
        asset_name: asset.clone(),
        asset_url: download_url(&tag, &asset),
        sha256_url: download_url(&tag, "SHA256SUMS"),
    }))
}

/// The crates.io sparse index is a static CDN (no API quota) serving one JSON
/// line per published version of the crate (ADR-0030).
const CRATES_IO_INDEX_URL: &str = "https://index.crates.io/co/la/colark";

/// One sparse-index line: the fields cola needs from it.
#[derive(serde::Deserialize)]
struct IndexEntry {
    vers: String,
    #[serde(default)]
    yanked: bool,
}

/// The newest crates.io version that is a valid update for `current`: not
/// yanked, newer, and not a prerelease unless `current` itself is one.
fn latest_from_index(body: &str, current: &Version) -> Option<Version> {
    body.lines()
        .filter_map(|line| serde_json::from_str::<IndexEntry>(line).ok())
        .filter(|e| !e.yanked)
        .filter_map(|e| Version::parse(&e.vers).ok())
        // A stable install is never offered a prerelease; a prerelease channel
        // may advance to newer prereleases or the stable release.
        .filter(|v| !current.pre.is_empty() || v.pre.is_empty())
        .filter(|v| v > current)
        .max()
}

/// Ask crates.io for the latest update available to a cargo-tracked binary.
async fn crates_io_latest(current: &Version) -> anyhow::Result<Option<Version>> {
    let body = reqwest::Client::new()
        .get(CRATES_IO_INDEX_URL)
        .header(reqwest::header::USER_AGENT, "cola-self-update")
        .send()
        .await
        .context("query crates.io sparse index")?
        .error_for_status()
        .context("crates.io sparse index returned an error")?
        .text()
        .await
        .context("read crates.io sparse index body")?;
    Ok(latest_from_index(&body, current))
}

/// The lower-case hex sha256 digest of a byte slice.
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect()
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
/// When a systemd unit owns this process (see [`supervisor_restart_code`]),
/// cola exits with the supervisor-restart code and lets the unit's
/// `Restart=on-failure` bring up the new binary from the same ExecStart path.
/// Everywhere else it re-execs via the existing `restart_process()` (spawn with
/// the original args + `--replace`, then exit) — see ADR-0015. Only the in-band
/// `/update` path calls this; the `cola update` CLI replaces the binary and
/// restarts via its supervisor (or tells the operator — see [`restart_cli`]).
pub fn restart() -> ! {
    if let Some(code) = supervisor_restart_code() {
        std::process::exit(code);
    }
    match crate::bridge::command::restart_process() {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("cola update: restart spawn failed: {e}");
            std::process::exit(EXIT_SUPERVISOR_RESTART);
        }
    }
}

/// How long to wait for a supervisor restart to hand the singleton lock to a
/// new daemon running the freshly-installed binary.
const RESTART_VERIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Poll cadence while waiting for the restart to take effect.
const RESTART_VERIFY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(400);

/// The absolute path of the executable the given process is running (`None`
/// when the process is not visible or reports no exe).
fn process_exe_path(pid: i32) -> Option<std::path::PathBuf> {
    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid as u32)]),
        false,
        sysinfo::ProcessRefreshKind::nothing().with_exe(sysinfo::UpdateKind::Always),
    );
    system
        .process(sysinfo::Pid::from_u32(pid as u32))?
        .exe()
        .map(|p| p.to_path_buf())
}

/// Whether the given process is running the binary at `expected` (same resolved
/// executable path). Confirms a supervisor restart brought up the freshly
/// installed binary rather than a stale one from another install location (the
/// ExecStart path-mismatch case).
fn process_runs_binary(pid: i32, expected: &std::path::Path) -> bool {
    let Some(exe) = process_exe_path(pid) else {
        return false;
    };
    paths_equal(&exe, expected)
}

/// Whether two binary paths resolve to the same file (symlinks followed).
fn paths_equal(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// Poll until the singleton lock is held by a DIFFERENT daemon process running
/// `current_exe` — proof the supervisor restart brought up the new binary.
/// Returns false when the deadline passes without that: the old daemon still
/// holds the lock (manual instance outside the supervisor), or the unit's
/// ExecStart points at a different binary than the one just updated.
fn supervisor_restart_took_effect(old_pid: Option<i32>, current_exe: &std::path::Path) -> bool {
    let deadline = std::time::Instant::now() + RESTART_VERIFY_TIMEOUT;
    loop {
        let new_pid = crate::running_daemon_pid();
        if let Some(pid) = new_pid {
            let different = old_pid.is_none_or(|old| pid != old);
            if different && process_runs_binary(pid, current_exe) {
                return true;
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(RESTART_VERIFY_INTERVAL);
    }
}

/// After the CLI has replaced the binary, restart a RUNNING daemon through its
/// OS supervisor (systemd user unit / launchd agent) — a supervisor-mediated
/// restart stays supervised and does not die with the CLI's terminal.
///
/// Returns the message to print: `None` when the supervisor restarted the
/// daemon **and verification confirmed the new process took the singleton lock
/// running the freshly-installed binary**; otherwise a hint tailored to the
/// situation. `/restart` in Feishu is only offered when `daemon_running` — a
/// dead bot cannot answer.
///
/// The verification is what makes the CLI honest: `systemctl --user restart`
/// reports success even when the new instance refuses to start (a manual daemon
/// still holding the singleton lock — systemd starts without `--replace`) or
/// when the unit's ExecStart points at a different binary path than the one the
/// CLI just replaced. Both leave the old version running while the CLI would
/// otherwise claim "restarted".
pub fn restart_cli(daemon_running: bool) -> Option<String> {
    if !daemon_running {
        return Some("cola 尚未运行，更新已就绪。启动 cola 即生效。".to_string());
    }
    let Some(cmd) = crate::autostart::supervisor_restart_command() else {
        return Some("运行中的 cola 仍是旧版本，重启生效：在飞书里发 /restart。".to_string());
    };
    let old_pid = crate::running_daemon_pid();
    let restarted = matches!(
        std::process::Command::new("sh").args(["-c", &cmd]).status(),
        Ok(s) if s.success()
    ) && std::env::current_exe()
        .ok()
        .map(|exe| supervisor_restart_took_effect(old_pid, &exe))
        .unwrap_or(false);
    if restarted {
        return None;
    }
    Some(format!(
        "⚠️ 监督者重启未确认生效：运行的 cola 仍是旧版本或未在运行（`{cmd}` 执行失败，或新进程未接管单例锁，或 ExecStart 与本次更新的路径不一致）。请检查，或在飞书里发 /restart。"
    ))
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
    /// A cargo-tracked install has a newer version on crates.io; nothing was
    /// downloaded or replaced — the user updates with the cargo command
    /// (ADR-0030).
    CargoUpdateAvailable,
    /// Applied; carries the new version.
    Updated(Version),
    Failed,
}

/// The message a cargo-tracked install gets when crates.io has a newer
/// version: the exact commands, including the escape hatch for stale install
/// bookkeeping (`--force`).
fn cargo_update_message(latest: &Version, current: &Version) -> String {
    format!(
        "发现新版本 {latest}（当前 {current}）。\n\
         当前 cola 由 cargo 安装，请用 cargo 更新：\n\
         - `cargo install colark`\n\
         - 若当初用 cargo-binstall 安装：`cargo binstall colark`\n\
         - 若 cargo 提示 already installed，加 `--force`"
    )
}

/// The cargo-channel flow (ADR-0030): report crates.io availability and the
/// command that applies it. Never downloads, installs, or restarts — a
/// cargo-tracked binary belongs to cargo.
async fn run_cargo_update(reporter: &dyn UpdateReporter) -> UpdateOutcome {
    let current = current_version();
    match crates_io_latest(&current).await {
        Err(e) => {
            tracing::warn!("crates.io update check failed: {e}");
            reporter.report(format!("❌ 检查 crates.io 更新失败：{e}")).await;
            UpdateOutcome::Failed
        }
        Ok(None) => {
            reporter
                .report(format!("✅ 已是最新版本（{current}，crates.io）。"))
                .await;
            UpdateOutcome::UpToDate
        }
        Ok(Some(latest)) => {
            reporter.report(cargo_update_message(&latest, &current)).await;
            UpdateOutcome::CargoUpdateAvailable
        }
    }
}

/// Run the whole self-update flow against `reporter`. Returns whether an
/// update was applied; the caller decides whether to call [`restart`].
pub async fn run_update(reporter: &dyn UpdateReporter, mode: UpdateMode) -> UpdateOutcome {
    // A cargo-tracked binary is updated through cargo, never by replacing it
    // with a GitHub asset (ADR-0030).
    let cargo_tracked =
        std::env::current_exe().is_ok_and(|exe| detect_install_channel(&exe) == InstallChannel::Cargo);

    // A dev build has no release-tag guarantee (ADR-0027): warn before the
    // check so `/update` on a local build never silently downgrades the
    // developer to the last release. Only the GitHub channel replaces a
    // binary, so a cargo-tracked dev install gets no replacement warning.
    if !cargo_tracked && crate::version::is_dev_build() {
        reporter
            .report(
                "⚠️ 当前是本地 dev 构建 —— 自更新会把二进制替换为最新发布版（可能比本地代码旧）。仅在发布版上建议执行更新。"
                    .into(),
            )
            .await;
    }
    reporter.report("🔍 正在检查更新…".into()).await;

    if cargo_tracked {
        return run_cargo_update(reporter).await;
    }

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
                    "发现新版本 {latest}，但当前平台没有预编译二进制，请手动更新（例如 `cargo install colark`）。"
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
            // A supervisor registered earlier (systemd unit / launchd agent) has
            // its ExecStart baked to the path it ran at registration time. If it
            // points at a DIFFERENT binary than the one this update replaces,
            // the supervisor restart would bring up the old version — warn now
            // so the operator can fix the path instead of a silent stale daemon.
            if let Some(supervisor_exe) = crate::autostart::supervisor_binary_path()
                && !paths_equal(&supervisor_exe, &exe)
            {
                reporter
                    .report(format!(
                        "⚠️ 监督者（ExecStart）指向 {}，而本次更新的是 {}——监督者重启后仍会运行旧版本。请统一安装路径。",
                        supervisor_exe.display(),
                        exe.display()
                    ))
                    .await;
            }
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

    /// Clean up the systemd env vars `supervisor_restart_code` reads so each
    /// test starts from a neutral state (they are only set by systemd, so the
    /// test runner never has them).
    fn clear_supervisor_env() {
        unsafe {
            std::env::remove_var("INVOCATION_ID");
            std::env::remove_var("SYSTEMD_EXEC_PID");
        }
    }

    /// Serializes tests that mutate process-global env vars (`cargo test` runs
    /// them on parallel threads; racing writes to the same vars would flake).
    static SUPERVISOR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    #[cfg(target_os = "linux")]
    fn supervisor_restart_code_requires_own_exec_pid() {
        let _guard = SUPERVISOR_ENV_LOCK.lock().unwrap();
        clear_supervisor_env();
        // Unit main process: INVOCATION_ID + SYSTEMD_EXEC_PID == our own PID.
        unsafe {
            std::env::set_var("INVOCATION_ID", "test");
            std::env::set_var("SYSTEMD_EXEC_PID", std::process::id().to_string());
        }
        assert_eq!(supervisor_restart_code(), Some(EXIT_SUPERVISOR_RESTART));
        clear_supervisor_env();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn supervisor_restart_code_ignores_inherited_invocation_id() {
        let _guard = SUPERVISOR_ENV_LOCK.lock().unwrap();
        clear_supervisor_env();
        // A terminal spawned under a unit inherits INVOCATION_ID but runs cola
        // as a plain foreground process: SYSTEMD_EXEC_PID points at the unit's
        // main process, not us, so there is NO supervisor to hand the restart
        // to — the fix regression (restart exited 3 with nothing to restart it).
        unsafe {
            std::env::set_var("INVOCATION_ID", "inherited");
            std::env::set_var("SYSTEMD_EXEC_PID", "99999");
        }
        assert_eq!(supervisor_restart_code(), None);
        clear_supervisor_env();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn supervisor_restart_code_none_without_invocation_id() {
        let _guard = SUPERVISOR_ENV_LOCK.lock().unwrap();
        clear_supervisor_env();
        unsafe {
            std::env::set_var("SYSTEMD_EXEC_PID", std::process::id().to_string());
        }
        assert_eq!(supervisor_restart_code(), None);
        clear_supervisor_env();
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
    fn latest_tag_from_location_absolute_url() {
        assert_eq!(
            latest_tag_from_location("https://github.com/SXongin/cola/releases/tag/0.7.0").as_deref(),
            Some("0.7.0")
        );
    }

    #[test]
    fn latest_tag_from_location_relative_and_v_prefix() {
        assert_eq!(
            latest_tag_from_location("/SXongin/cola/releases/tag/v0.7.0").as_deref(),
            Some("v0.7.0")
        );
    }

    #[test]
    fn latest_tag_from_location_trailing_slash() {
        assert_eq!(
            latest_tag_from_location("https://github.com/SXongin/cola/releases/tag/0.7.0/").as_deref(),
            Some("0.7.0")
        );
    }

    #[test]
    fn latest_tag_from_location_rejects_garbage() {
        assert_eq!(latest_tag_from_location("not-a-url"), None);
        assert_eq!(latest_tag_from_location("https://github.com/"), None);
    }

    #[test]
    fn asset_name_follows_release_yml_naming() {
        assert_eq!(
            asset_name("0.7.0", "x86_64-unknown-linux-gnu"),
            "cola-0.7.0-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            asset_name("0.7.0", "aarch64-apple-darwin"),
            "cola-0.7.0-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            asset_name("0.7.0", "x86_64-pc-windows-msvc"),
            "cola-0.7.0-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn download_url_points_at_releases_download() {
        assert_eq!(
            download_url("0.7.0", "cola-0.7.0-x86_64-unknown-linux-gnu.tar.gz"),
            "https://github.com/SXongin/cola/releases/download/0.7.0/cola-0.7.0-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            download_url("0.7.0", "SHA256SUMS"),
            "https://github.com/SXongin/cola/releases/download/0.7.0/SHA256SUMS"
        );
    }
    #[test]
    fn detect_channel_reads_crates2_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let bin_dir = root.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join("cola"), b"x").unwrap();
        std::fs::write(
            root.join(".crates2.json"),
            r#"{"installs":{"colark 0.8.1 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["cola"]}}}"#,
        )
        .unwrap();
        assert_eq!(
            detect_install_channel(&bin_dir.join("cola")),
            InstallChannel::Cargo
        );
    }

    #[test]
    fn detect_channel_without_receipt_is_github() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join("cola"), b"x").unwrap();
        assert_eq!(
            detect_install_channel(&bin_dir.join("cola")),
            InstallChannel::GitHub
        );
    }

    #[test]
    fn detect_channel_ignores_receipt_for_another_binary() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let bin_dir = root.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join("cola"), b"x").unwrap();
        std::fs::write(
            root.join(".crates2.json"),
            r#"{"installs":{"other 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["other"]}}}"#,
        )
        .unwrap();
        assert_eq!(
            detect_install_channel(&bin_dir.join("cola")),
            InstallChannel::GitHub
        );
    }

    #[test]
    fn detect_channel_accepts_windows_stem_in_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let bin_dir = root.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join("cola.exe"), b"x").unwrap();
        std::fs::write(
            root.join(".crates2.json"),
            r#"{"installs":{"colark 0.8.1 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["cola"]}}}"#,
        )
        .unwrap();
        assert_eq!(
            detect_install_channel(&bin_dir.join("cola.exe")),
            InstallChannel::Cargo
        );
    }

    #[test]
    fn detect_channel_requires_bin_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("cola"), b"x").unwrap();
        std::fs::write(
            root.join(".crates2.json"),
            r#"{"installs":{"colark 0.8.1 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["cola"]}}}"#,
        )
        .unwrap();
        assert_eq!(detect_install_channel(&root.join("cola")), InstallChannel::GitHub);
    }

    #[test]
    fn index_picks_newest_stable_and_skips_yanked() {
        let body = r#"{"name":"colark","vers":"0.7.0","yanked":false}
{"name":"colark","vers":"0.8.2","yanked":false}
{"name":"colark","vers":"0.9.0-rc.1","yanked":false}
{"name":"colark","vers":"1.0.0","yanked":true}"#;
        let current = Version::parse("0.8.1").unwrap();
        assert_eq!(
            latest_from_index(body, &current),
            Some(Version::parse("0.8.2").unwrap())
        );
    }

    #[test]
    fn index_allows_prerelease_when_current_is_prerelease() {
        let body = r#"{"name":"colark","vers":"0.9.0-rc.1","yanked":false}
{"name":"colark","vers":"0.9.0-rc.2","yanked":false}"#;
        let current = Version::parse("0.9.0-rc.1").unwrap();
        assert_eq!(
            latest_from_index(body, &current),
            Some(Version::parse("0.9.0-rc.2").unwrap())
        );
    }

    #[test]
    fn index_none_when_nothing_newer() {
        let body = r#"{"name":"colark","vers":"0.8.1","yanked":false}
not json at all
{"name":"colark","vers":"0.8.0","yanked":false}"#;
        let current = Version::parse("0.8.1").unwrap();
        assert_eq!(latest_from_index(body, &current), None);
    }

    #[test]
    fn cargo_update_message_names_commands_and_force() {
        let msg = cargo_update_message(
            &Version::parse("0.9.0").unwrap(),
            &Version::parse("0.8.1").unwrap(),
        );
        assert!(msg.contains("0.9.0"), "{msg}");
        assert!(msg.contains("0.8.1"), "{msg}");
        assert!(msg.contains("cargo install colark"), "{msg}");
        assert!(msg.contains("cargo binstall colark"), "{msg}");
        assert!(msg.contains("--force"), "{msg}");
    }

    #[test]
    fn paths_equal_self_is_equal() {
        let exe = std::env::current_exe().unwrap();
        assert!(paths_equal(&exe, &exe));
    }

    #[test]
    fn paths_equal_distinct_files_are_not_equal() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"y").unwrap();
        assert!(!paths_equal(&a, &b));
    }

    #[test]
    fn process_runs_binary_sees_own_binary() {
        let exe = std::env::current_exe().unwrap();
        assert!(process_runs_binary(std::process::id() as i32, &exe));
    }

    #[test]
    fn process_runs_binary_rejects_other_binary() {
        // A far-out PID is certainly not the test process.
        assert!(!process_runs_binary(
            i32::MAX - 1,
            &std::env::current_exe().unwrap()
        ));
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
