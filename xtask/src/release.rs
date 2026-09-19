//! `cargo xtask release <version>` — the release cut (ADR-0033).
//!
//! The cut bumps `Cargo.toml`/`Cargo.lock` on a `release-<version>` branch,
//! opens a PR, watches every check, rebase-merges it with the admin
//! bypass, and only then tags the **merged** commit on `main` and pushes the
//! tag — `release.yml` takes over from there. Tagging a branch commit is the
//! `0.7.0` failure: a rebase merge can rewrite it, leaving the tag off `main`.
//!
//! A re-run resumes from whatever state it finds: a fresh cut, a release
//! branch waiting to merge, or a merged cut whose tag is still missing. The
//! state classification ([`plan`]) is pure and tested; the side effects are
//! plain git/gh invocations.

use semver::Version;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

pub type Result<T> = std::result::Result<T, String>;

const SMOKE_CHECKLIST: &str = "\
Release smoke test (run it in a real Feishu chat; CONTRIBUTING.md has the how-to):
  1. Send a message: the Done card renders and updates in place.
  2. With /autoaccept off, trigger a tool permission: the permission card's
     允许一次 / 始终允许 / 拒绝 buttons round-trip and the answer reaches the tool.
  3. Trigger the question tool: the question card's buttons round-trip.
  4. In a group, @cola /topic: the topic is created and shows up in the
     group's topic list.
  5. /model: the model picker card appears.
  6. /dir <non-git directory>: the session starts without error.";

/// Parse `cargo xtask release <version> [--yes]` and run the cut.
pub fn cli() -> ! {
    let mut yes = false;
    let mut target: Option<Version> = None;
    for arg in std::env::args().skip(2) {
        match arg.as_str() {
            "--yes" | "-y" => yes = true,
            "--help" | "-h" => {
                println!("usage: cargo xtask release <version> [--yes]");
                std::process::exit(0);
            }
            other if other.starts_with('-') => die(&format!("unknown flag `{other}`")),
            _ if target.is_some() => die("expected exactly one <version>"),
            other => {
                target = Some(
                    Version::parse(other)
                        .unwrap_or_else(|e| die(&format!("invalid semver `<version>` `{other}`: {e}"))),
                );
            }
        }
    }
    let Some(target) = target else {
        die("usage: cargo xtask release <version> [--yes]");
    };

    if let Err(e) = run(&target, yes) {
        die(&format!("release {target}: {e}"));
    }
    println!(
        "Tagged {target}. The Release workflow is building the artifacts now:\n  \
         gh run list --workflow=release.yml --limit 1"
    );
    std::process::exit(0);
}

fn die(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(1);
}

/// The repository state a cut starts from; gathered once, classified purely.
#[derive(Debug)]
struct Facts {
    branch: String,
    clean: bool,
    manifest: Version,
    tag_exists: bool,
}

/// What the state calls for; `Refuse` carries the reason to print.
#[derive(Debug, PartialEq, Eq)]
enum CutPlan {
    Refuse(String),
    FreshCut,
    ResumeBeforeMerge,
    ResumeAfterMerge,
}

/// Classify a cut. Pure, so the state table is testable without git/gh: which
/// action to take is exactly the decision the `0.7.0` drift got wrong.
fn plan(facts: &Facts, target: &Version, release_branch: &str) -> CutPlan {
    if facts.tag_exists {
        return CutPlan::Refuse(format!("tag {target} already exists"));
    }
    if facts.branch == "main" {
        if facts.manifest == *target {
            return CutPlan::ResumeAfterMerge;
        }
        if facts.manifest > *target {
            return CutPlan::Refuse(format!(
                "main is already at {}, which is not below {target}",
                facts.manifest
            ));
        }
        if !facts.clean {
            return CutPlan::Refuse("working tree is not clean; commit or stash first".into());
        }
        return CutPlan::FreshCut;
    }
    if facts.branch == release_branch {
        if facts.manifest == *target {
            return CutPlan::ResumeBeforeMerge;
        }
        return CutPlan::Refuse(format!(
            "{release_branch} is at version {}, expected {target}; delete the branch and re-run",
            facts.manifest
        ));
    }
    CutPlan::Refuse(format!(
        "on branch {}; run from main (or {release_branch} when resuming)",
        facts.branch
    ))
}

fn run(target: &Version, yes: bool) -> Result<()> {
    let release_branch = format!("release-{target}");
    match classify(target, &release_branch)? {
        CutPlan::Refuse(why) => Err(why),
        CutPlan::ResumeAfterMerge => finish_tag(target),
        CutPlan::ResumeBeforeMerge => merge_flow(&release_branch, target),
        CutPlan::FreshCut => {
            if !yes && !confirm_smoke()? {
                return Err("smoke test not confirmed — run it, then retry (or pass --yes)".into());
            }
            fresh_cut(target, &release_branch)?;
            merge_flow(&release_branch, target)
        }
    }
}

fn classify(target: &Version, release_branch: &str) -> Result<CutPlan> {
    let facts = Facts {
        branch: git(&["rev-parse", "--abbrev-ref", "HEAD"])?,
        clean: git(&["status", "--porcelain"])?.is_empty(),
        manifest: package_version(&read_manifest()?)?,
        tag_exists: !git(&["tag", "--list", &target.to_string()])?.is_empty(),
    };
    Ok(plan(&facts, target, release_branch))
}

/// Bump the manifest and push `release-<version>` with a single commit.
fn fresh_cut(target: &Version, release_branch: &str) -> Result<()> {
    println!("Syncing main...");
    git_ok(&["pull", "--ff-only"])?;
    let current = package_version(&read_manifest()?)?;
    if current >= *target {
        return Err(format!(
            "main moved to {current} while preparing; expected a version below {target}"
        ));
    }

    git_ok(&["switch", "-c", release_branch])?;
    println!("Bumping version to {target} on {release_branch}...");
    let path = manifest_path();
    let text = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let bumped = bump_manifest(&text, target)?;
    std::fs::write(&path, bumped).map_err(|e| format!("write {}: {e}", path.display()))?;
    cargo_update_workspace()?;

    git_ok(&["add", "Cargo.toml", "Cargo.lock"])?;
    git_ok(&[
        "commit",
        "-m",
        &format!("chore(release): bump version to {target}"),
    ])?;
    println!("Pushing {release_branch}...");
    git_ok(&["push", "-u", "origin", release_branch])
}

/// Open (or reuse) the release PR, wait for CI, and merge it.
fn merge_flow(release_branch: &str, target: &Version) -> Result<()> {
    match pr_state(release_branch)? {
        None => {
            println!("Opening the release PR...");
            gh(&[
                "pr",
                "create",
                "--base",
                "main",
                "--head",
                release_branch,
                "--title",
                &format!("chore(release): bump version to {target}"),
                "--body",
                &pr_body(target),
            ])?;
        }
        Some(PrState::Closed) => {
            return Err(format!(
                "the PR for {release_branch} is closed; delete the branch and re-run"
            ));
        }
        Some(PrState::Open | PrState::Merged) => {}
    }

    match pr_state(release_branch)? {
        Some(PrState::Merged) => {}
        Some(PrState::Open) => {
            println!("Waiting for CI on {release_branch}...");
            watch_checks(release_branch)?;
            println!("CI is green; rebase-merging with the admin bypass...");
            gh(&["pr", "merge", release_branch, "--rebase", "--admin"])?;
        }
        Some(PrState::Closed) | None => {
            return Err(format!("PR for {release_branch} vanished; re-run"));
        }
    }
    finish_tag(target)
}

/// Pull the merged `main` and tag its HEAD.
fn finish_tag(target: &Version) -> Result<()> {
    println!("Syncing main and tagging the merged commit...");
    git_ok(&["switch", "main"])?;
    git_ok(&["pull", "--ff-only"])?;
    let manifest = package_version(&read_manifest()?)?;
    if manifest != *target {
        return Err(format!(
            "after merge, main's version is {manifest}, expected {target}"
        ));
    }
    if !git(&["tag", "--list", &target.to_string()])?.is_empty() {
        return Err(format!("tag {target} already exists"));
    }
    let sha = git(&["rev-parse", "--short", "HEAD"])?;
    println!("Tagging {sha} as {target}...");
    git_ok(&["tag", &target.to_string()])?;
    git_ok(&["push", "origin", &target.to_string()])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrState {
    Open,
    Merged,
    Closed,
}

fn pr_state(branch: &str) -> Result<Option<PrState>> {
    let out = Command::new("gh")
        .args(["pr", "view", branch, "--json", "state", "--jq", ".state"])
        .output()
        .map_err(|e| format!("failed to run gh: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("no pull requests found") {
            return Ok(None);
        }
        return Err(format!("gh pr view {branch} failed: {}", stderr.trim()));
    }
    match String::from_utf8_lossy(&out.stdout).trim() {
        "OPEN" => Ok(Some(PrState::Open)),
        "MERGED" => Ok(Some(PrState::Merged)),
        "CLOSED" => Ok(Some(PrState::Closed)),
        other => Err(format!("unexpected PR state `{other}`")),
    }
}

/// Watch the PR's checks. `gh pr checks` reports "no checks reported" until the
/// workflow starts, so retry that case; live output stays on the terminal.
fn watch_checks(branch: &str) -> Result<()> {
    const ATTEMPTS: u32 = 30;
    for attempt in 0..ATTEMPTS {
        let child = Command::new("gh")
            .args(["pr", "checks", branch, "--watch"])
            .stdout(Stdio::inherit())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to run gh pr checks: {e}"))?;
        let out = child
            .wait_with_output()
            .map_err(|e| format!("gh pr checks did not finish: {e}"))?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("no checks reported") && attempt + 1 < ATTEMPTS {
            println!("CI has not reported yet; retrying in 10s...");
            std::thread::sleep(Duration::from_secs(10));
            continue;
        }
        return Err(format!("checks on {branch} did not pass:\n{}", stderr.trim()));
    }
    unreachable!("the retry loop returns on its last attempt")
}

fn pr_body(target: &Version) -> String {
    format!(
        "## What\n\
         Release cut for `{target}`: bumps the Release Version in `Cargo.toml`.\n\n\
         ## Why\n\
         Cutting release {target}; the bump is created by `cargo xtask release` (ADR-0033).\n\n\
         ## How tested\n\
         CI gates this PR; the release smoke test was confirmed before the bump."
    )
}

fn confirm_smoke() -> Result<bool> {
    println!("{SMOKE_CHECKLIST}\n\nProceed with the release cut? [y/N]");
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("failed to read confirmation: {e}"))?;
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn manifest_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace root")
        .join("Cargo.toml")
}

fn read_manifest() -> Result<String> {
    let path = manifest_path();
    std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))
}

/// The `[package] version` of a manifest. Only the package section counts: the
/// workspace section and dependency tables also carry version keys.
pub(crate) fn package_version(text: &str) -> Result<Version> {
    let mut in_package = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("version")
            && let Some(value) = rest.trim_start().strip_prefix('=')
        {
            let raw = value.trim().trim_matches('"');
            return Version::parse(raw).map_err(|e| format!("Cargo.toml version `{raw}` is not semver: {e}"));
        }
    }
    Err("no `version` line in the [package] section of Cargo.toml".into())
}

/// Replace the `[package] version` line, leaving every other version key (a
/// dependency's, the workspace's) untouched.
pub(crate) fn bump_manifest(text: &str, version: &Version) -> Result<String> {
    let mut out = String::with_capacity(text.len() + 8);
    let mut in_package = false;
    let mut replaced = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
        } else if in_package
            && !replaced
            && let Some(rest) = trimmed.strip_prefix("version")
            && rest.trim_start().starts_with('=')
        {
            out.push_str(&format!("version = \"{version}\""));
            out.push('\n');
            replaced = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !replaced {
        return Err("no `version` line in the [package] section of Cargo.toml".into());
    }
    Ok(out)
}

fn cargo_update_workspace() -> Result<()> {
    let mut cmd = Command::new("cargo");
    cmd.args(["update", "--workspace"]);
    run_status(&mut cmd)
}

fn git(args: &[&str]) -> Result<String> {
    capture(Command::new("git").args(args))
}

fn git_ok(args: &[&str]) -> Result<()> {
    run_status(Command::new("git").args(args))
}

fn gh(args: &[&str]) -> Result<String> {
    capture(Command::new("gh").args(args))
}

fn capture(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().map_err(|e| format!("failed to run {cmd:?}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{cmd:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run_status(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().map_err(|e| format!("failed to run {cmd:?}: {e}"))?;
    if !status.success() {
        return Err(format!("{cmd:?} exited with {status}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    const MANIFEST: &str = "\
[workspace]
members = [\".\", \"xtask\"]
resolver = \"3\"

[package]
name = \"colark\"
version = \"0.8.2\"
edition = \"2024\"

[dependencies]
serde = { version = \"1\", features = [\"derive\"] }
tokio = { version = \"1\", features = [\"full\"] }
";

    #[test]
    fn package_version_reads_the_package_section() {
        assert_eq!(package_version(MANIFEST).unwrap(), v("0.8.2"));
    }

    #[test]
    fn bump_manifest_touches_only_the_package_version() {
        let bumped = bump_manifest(MANIFEST, &v("0.9.0")).unwrap();
        assert_eq!(package_version(&bumped).unwrap(), v("0.9.0"));
        assert!(bumped.contains("serde = { version = \"1\""));
        assert!(bumped.contains("tokio = { version = \"1\""));
        assert!(!bumped.contains("0.8.2"));
    }

    #[test]
    fn manifests_without_a_package_version_are_refused() {
        let text = "[workspace]\nmembers = [\".\"]\n";
        assert!(package_version(text).is_err());
        assert!(bump_manifest(text, &v("1.0.0")).is_err());
    }

    fn facts(branch: &str, clean: bool, manifest: &str, tag_exists: bool) -> Facts {
        Facts {
            branch: branch.into(),
            clean,
            manifest: v(manifest),
            tag_exists,
        }
    }

    #[test]
    fn plan_finds_the_three_cut_states() {
        let target = v("0.9.0");
        let rb = "release-0.9.0";
        assert_eq!(
            plan(&facts("main", true, "0.8.2", false), &target, rb),
            CutPlan::FreshCut
        );
        assert_eq!(
            plan(&facts(rb, false, "0.9.0", false), &target, rb),
            CutPlan::ResumeBeforeMerge
        );
        assert_eq!(
            plan(&facts("main", true, "0.9.0", false), &target, rb),
            CutPlan::ResumeAfterMerge
        );
    }

    #[test]
    fn plan_refuses_unsafe_states() {
        let target = v("0.9.0");
        let rb = "release-0.9.0";
        let cases = [
            (facts("main", true, "0.8.2", true), "tag"),
            (facts("main", false, "0.8.2", false), "clean"),
            (facts("main", true, "1.0.0", false), "already"),
            (facts("feat/x", true, "0.8.2", false), "branch"),
            (facts(rb, true, "0.8.2", false), "version"),
        ];
        for (state, expected) in cases {
            match plan(&state, &target, rb) {
                CutPlan::Refuse(message) => {
                    assert!(message.contains(expected), "expected {expected:?} in {message:?}")
                }
                other => panic!("expected Refuse for {state:?}, got {other:?}"),
            }
        }
    }
}
