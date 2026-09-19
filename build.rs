use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // Stamp the binary's build provenance first (ADR-0027): this must run in
    // CI and in non-git trees too, so it is NOT gated by the hook logic below.
    stamp_build_identity();

    // Re-sync git hooks whenever the lefthook config changes.
    println!("cargo:rerun-if-changed=lefthook.yml");

    // Hooks are a local-dev convenience: skip in CI and non-git checkouts.
    if env::var_os("CI").is_some() {
        return;
    }
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    if !Path::new(&manifest_dir).join(".git").exists() {
        return;
    }

    match Command::new("lefthook").arg("install").status() {
        Ok(status) if status.success() => {}
        Ok(_) => {
            println!(
                "cargo:warning=`lefthook install` failed; git hooks are stale. Run `lefthook install` manually."
            );
        }
        Err(_) => {
            println!(
                "cargo:warning=lefthook not found; git hooks not installed. Install it (`npm install -g lefthook` or `go install github.com/evilmartians/lefthook@latest`), then run `lefthook install`."
            );
        }
    }
}

/// Embed the Build Provenance (ADR-0027): a binary is a RELEASE build only when
/// HEAD sits exactly on a release tag equal to the Cargo.toml version AND the
/// tree is clean; every other build (a branch, a dirty tree, or no git at all)
/// is a dev build and gets its branch/short-sha/dirty state stamped for display.
///
/// A crates.io package build is the one non-git release identity (ADR-0030):
/// the package is extracted without `.git` but carries `.cargo_vcs_info.json`,
/// and its Cargo.toml version is the published release version.
///
/// Best effort: any git failure (no git, a tarball checkout) leaves the
/// release var unset, which the runtime reads as "dev build, no provenance".
fn stamp_build_identity() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let cargo_version = env::var("CARGO_PKG_VERSION").unwrap_or_default();

    // Re-run the stamp when git state moves (a commit, branch switch, or tag)
    // so a rebuilt binary carries fresh provenance.
    let dot_git = Path::new(&manifest_dir).join(".git");
    if dot_git.exists() {
        declare_git_inputs(&manifest_dir);
    }

    let tag = git_value(&manifest_dir, &["describe", "--tags", "--exact-match", "HEAD"]);
    // Clean means `git status --porcelain` succeeded with empty output; a git
    // failure (None — not a repo) is neither clean nor dirty.
    let porcelain = git(&manifest_dir, &["status", "--porcelain"]);
    let clean = porcelain.as_deref().is_some_and(str::is_empty);
    let release = clean && tag.as_deref() == Some(cargo_version.as_str());
    if release {
        println!("cargo:rustc-env=COLA_RELEASE=1");
        return;
    }

    // A crates.io package build (ADR-0030): no `.git`, but the package carries
    // `.cargo_vcs_info.json`. This is a release identity at the published
    // version, not a dev build.
    if !dot_git.exists() && Path::new(&manifest_dir).join(".cargo_vcs_info.json").exists() {
        println!("cargo:rustc-env=COLA_BUILD_CHANNEL=crates.io");
        return;
    }

    // Dev build: stamp what we can show (branch, short sha, dirty).
    if let Some(branch) = git_value(&manifest_dir, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        println!("cargo:rustc-env=COLA_GIT_BRANCH={branch}");
    }
    if let Some(sha) = git_value(&manifest_dir, &["rev-parse", "--short", "HEAD"]) {
        println!("cargo:rustc-env=COLA_GIT_SHA={sha}");
    }
    if porcelain.as_deref().is_some_and(|out| !out.is_empty()) {
        println!("cargo:rustc-env=COLA_GIT_DIRTY=1");
    }
}

/// Declare the git state that moves the provenance as build-script inputs: the
/// worktree's HEAD (a branch switch, commit, or reset writes it) and the local
/// ref store (a commit, tag, or branch update writes it). `refs/remotes` stays
/// out on purpose — a fetch only moves remote-tracking refs, and re-stamping
/// there would force a full rebuild the provenance cannot reflect.
///
/// The paths come from git, not from `<manifest>/.git`: a linked worktree's
/// `.git` is a FILE, its HEAD lives under `.git/worktrees/<name>`, and its refs
/// live in the shared common dir. Assuming `.git` was a directory declared no
/// git input at all, so the script never re-ran and the stamp froze at whatever
/// branch first built that worktree's target dir (#245).
fn declare_git_inputs(manifest_dir: &str) {
    if let Some(git_dir) = git_value(manifest_dir, &["rev-parse", "--absolute-git-dir"]) {
        declare_rerun_if_changed(&Path::new(&git_dir).join("HEAD"));
    }
    let Some(common_dir) = git_value(manifest_dir, &["rev-parse", "--git-common-dir"]) else {
        return;
    };
    let common_dir = absolutize(manifest_dir, &common_dir);
    for path in ["refs/heads", "refs/tags", "packed-refs"] {
        declare_rerun_if_changed(&common_dir.join(path));
    }
}

/// Declare `path` as a build-script input when it exists: cargo treats a
/// missing declared path as permanently dirty and re-runs the script on every
/// build, so an absent path must not be emitted. The existence check loses
/// nothing reachable: git creates `refs/heads` and `refs/tags` at init/clone
/// and leaves them in place through `pack-refs` and `gc`, so the ref dirs are
/// always there to be declared.
fn declare_rerun_if_changed(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

/// `git rev-parse` answers relative to the invocation dir (`-C manifest_dir`),
/// which is the package root, unless it already resolved an absolute path.
fn absolutize(manifest_dir: &str, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        Path::new(manifest_dir).join(path)
    }
}

/// Run a git command in `manifest_dir`; trimmed stdout on success, None when
/// git is unavailable or the command fails. Success with empty stdout is a
/// real result (e.g. a clean `git status --porcelain`), so it returns
/// `Some("")` — callers that need a value filter it themselves.
fn git(manifest_dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(["-C", manifest_dir])
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A [`git`] result that must carry a value: a successful-but-empty stdout is
/// dropped, for the provenance fields that are only meaningful when non-empty.
fn git_value(manifest_dir: &str, args: &[&str]) -> Option<String> {
    git(manifest_dir, args).filter(|out| !out.is_empty())
}
