use std::env;
use std::path::Path;
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
/// Best effort: any git failure (no git, a tarball checkout) leaves the
/// release var unset, which the runtime reads as "dev build, no provenance".
fn stamp_build_identity() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let cargo_version = env::var("CARGO_PKG_VERSION").unwrap_or_default();

    // Re-run the stamp when git state moves (a commit, branch switch, or tag
    // change touches HEAD / packed-refs / a ref under .git/refs) so a rebuilt
    // binary carries fresh provenance.
    let git_dir = Path::new(&manifest_dir).join(".git");
    if git_dir.join("HEAD").exists() {
        println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
    }
    let packed_refs = git_dir.join("packed-refs");
    if packed_refs.exists() {
        println!("cargo:rerun-if-changed={}", packed_refs.display());
    }
    declare_refs_changed(&git_dir.join("refs"));

    let tag = git(&manifest_dir, &["describe", "--tags", "--exact-match", "HEAD"]);
    // Clean means `git status --porcelain` succeeded with empty output; a git
    // failure (None — not a repo) is neither clean nor dirty.
    let porcelain = git(&manifest_dir, &["status", "--porcelain"]);
    let clean = porcelain.as_deref().is_some_and(str::is_empty);
    let release = clean && tag.as_deref() == Some(cargo_version.as_str());
    if release {
        println!("cargo:rustc-env=COLA_RELEASE=1");
        return;
    }

    // Dev build: stamp what we can show (branch, short sha, dirty).
    if let Some(branch) = git(&manifest_dir, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        println!("cargo:rustc-env=COLA_GIT_BRANCH={branch}");
    }
    if let Some(sha) = git(&manifest_dir, &["rev-parse", "--short", "HEAD"]) {
        println!("cargo:rustc-env=COLA_GIT_SHA={sha}");
    }
    if porcelain.as_deref().is_some_and(|out| !out.is_empty()) {
        println!("cargo:rustc-env=COLA_GIT_DIRTY=1");
    }
}

/// Declare every file under `refs` (recursively) as a build-script input, so a
/// new commit or branch on an existing ref re-runs the provenance stamp.
fn declare_refs_changed(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            declare_refs_changed(&path);
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

/// Run a git command in `manifest_dir`; trimmed stdout on success, None when
/// git is unavailable, the command fails, or the output is empty.
fn git(manifest_dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(["-C", manifest_dir])
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}
