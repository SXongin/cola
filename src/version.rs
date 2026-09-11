//! cola's printable build identity (ADR-0027): the Release Version and the
//! Build Provenance stamped by `build.rs`.
//!
//! A binary is a RELEASE build only when `build.rs` found HEAD exactly on a
//! clean release tag equal to the Cargo.toml version (`COLA_RELEASE` set);
//! every other build is a dev build. Self-update compares the Release Version
//! alone ([`crate::update::current_version`] / `CARGO_PKG_VERSION`) — the dev
//! marker here is display-only and must never enter a version comparison, or a
//! dev build ahead of the last release would report "update available" forever.

/// The env `build.rs` sets when HEAD is exactly a clean release tag equal to
/// the Cargo.toml version.
const RELEASE: Option<&str> = option_env!("COLA_RELEASE");
/// Set when `build.rs` found a crates.io package (no `.git`, but
/// `.cargo_vcs_info.json`): a release identity that is not a git checkout
/// (ADR-0030). Display-only, like the branch/sha below.
const BUILD_CHANNEL: Option<&str> = option_env!("COLA_BUILD_CHANNEL");
/// The branch name (or "HEAD" when detached) of a dev build's source tree.
const BRANCH: Option<&str> = option_env!("COLA_GIT_BRANCH");
/// The short commit hash of a dev build's source tree.
const SHA: Option<&str> = option_env!("COLA_GIT_SHA");
/// Set when the dev build's source tree was Dirty at build time.
const DIRTY: Option<&str> = option_env!("COLA_GIT_DIRTY");

/// The Release Version — the semver this binary was built from
/// (`CARGO_PKG_VERSION`). The ONLY value self-update compares against the
/// release tag (ADR-0015, ADR-0027); see [`is_dev_build`].
pub fn release_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Whether this binary is a dev build (ADR-0027): `build.rs` found neither a
/// clean release tag on HEAD nor a crates.io package identity (ADR-0030).
pub fn is_dev_build() -> bool {
    RELEASE.is_none() && BUILD_CHANNEL.is_none()
}

/// The canonical identity string shared by `cola --version`, the startup log
/// line, and the Feishu `/version` reply:
///
/// - release:   `cola 0.7.0`
/// - crates.io: `cola 0.8.1 (crates.io)`
/// - dev:       `cola 0.7.0-dev <branch>@<shortsha> ⚠` (⚠ when the tree was
///   Dirty; detached HEAD omits the branch; no git provenance shows no tail)
pub fn display_version() -> String {
    render(
        release_version(),
        is_dev_build(),
        BUILD_CHANNEL,
        BRANCH,
        SHA,
        DIRTY.is_some(),
    )
}

/// The Feishu `/version` reply: the canonical identity string plus, for dev
/// builds, a short note that self-update targets release builds only.
pub fn feishu_reply() -> String {
    if is_dev_build() {
        format!(
            "当前 {}\n（本地 dev 构建，非发布版。自更新仅面向发布版本。）",
            display_version()
        )
    } else {
        format!("当前 {}", display_version())
    }
}

/// Assemble the identity string from its parts (pure, so the format is
/// testable without a specific build environment). `branch` of `HEAD` (git's
/// name for a detached HEAD) renders as no branch — the short sha stands alone.
fn render(
    release: &str,
    dev: bool,
    channel: Option<&str>,
    branch: Option<&str>,
    sha: Option<&str>,
    dirty: bool,
) -> String {
    if !dev {
        return match channel.filter(|c| !c.is_empty()) {
            Some(channel) => format!("cola {release} ({channel})"),
            None => format!("cola {release}"),
        };
    }
    let branch = branch.filter(|b| *b != "HEAD" && !b.is_empty());
    let sha = sha.filter(|s| !s.is_empty());
    let mut words: Vec<String> = vec![format!("cola {release}-dev")];
    match (branch, sha) {
        (Some(b), Some(s)) => words.push(format!("{b}@{s}")),
        (None, Some(s)) => words.push(s.to_string()),
        (Some(b), None) => words.push(b.to_string()),
        (None, None) => {}
    }
    if dirty {
        words.push("⚠".to_string());
    }
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_renders_bare_version() {
        assert_eq!(
            render("0.7.0", false, None, Some("main"), Some("ab12cd3"), true),
            "cola 0.7.0"
        );
    }

    #[test]
    fn cratesio_build_renders_channel() {
        assert_eq!(
            render("0.8.1", false, Some("crates.io"), None, None, false),
            "cola 0.8.1 (crates.io)"
        );
    }

    #[test]
    fn dev_renders_branch_at_sha_and_dirty() {
        assert_eq!(
            render("0.7.0", true, None, Some("feat"), Some("ab12cd3"), true),
            "cola 0.7.0-dev feat@ab12cd3 ⚠"
        );
    }

    #[test]
    fn dev_clean_omits_warning() {
        assert_eq!(
            render("0.7.0", true, None, Some("main"), Some("ab12cd3"), false),
            "cola 0.7.0-dev main@ab12cd3"
        );
    }

    #[test]
    fn dev_detached_head_omits_branch() {
        assert_eq!(
            render("0.7.0", true, None, Some("HEAD"), Some("ab12cd3"), false),
            "cola 0.7.0-dev ab12cd3"
        );
    }

    #[test]
    fn dev_without_git_shows_no_provenance() {
        assert_eq!(render("0.7.0", true, None, None, None, false), "cola 0.7.0-dev");
    }

    #[test]
    fn dev_dirty_without_locator_still_flags() {
        assert_eq!(render("0.7.0", true, None, None, None, true), "cola 0.7.0-dev ⚠");
    }
}
