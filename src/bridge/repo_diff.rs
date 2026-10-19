//! The `/diff` command (spec #803, ticket #804): render the **repository
//! review** — committed changes since the base's fork point, uncommitted
//! tracked changes and untracked files — as one self-contained HTML **File
//! Message**.
//!
//! The data producer is [`crate::review::review_diff`] (#805); the artifact
//! generator is [`crate::backend::render_diff_html`] (#798); the delivery lane
//! is [`crate::bridge::viewer::send_file_message`] (#801). This module owns
//! exactly the command-shaped glue: the per-chat cooldown, the file name, the
//! empty/error wording, and speaking every delivery outcome. It never parses
//! the document's subtitle — the producer's [`crate::review::ReviewDiff`]
//! fields carry the resolved base, branch and short SHA.
//!
//! Everything here is bounded and local: `/diff` runs git (bounded inside
//! #805), renders in memory, and replies to the command's own message id. It
//! adds no card and no network call of its own.

use std::collections::HashMap;
use std::sync::PoisonError;
use std::time::{Duration, Instant};

use crate::bridge::handles::CommandHandles;
use crate::config::ThreadKey;
use crate::feishu::card::tool_render::FileDelivery;

/// The one usage line for an unsupported `/diff` argument. The grammar is
/// exactly `/diff` and `/diff --base <ref>`; parsing answers this before any
/// git call.
pub(crate) const DIFF_USAGE: &str = "⚠️ 不支持的参数。用法：`/diff [--base <ref>]`";

/// How long one chat must wait between `/diff` calls (spec #803 decision 5):
/// long enough to stop a double-click from uploading two files, short enough
/// not to get in a reader's way.
pub(crate) const DIFF_COOLDOWN: Duration = Duration::from_secs(10);

/// The reply when the chat is still inside its cooldown window.
const COOLDOWN_REPLY: &str = "⏳ 操作太频繁，请稍后再试。";

/// The reply when a File Message send timed out after the request may have
/// reached Feishu. Retrying could duplicate it, so the command says so and
/// stops (the same words the viewer and turn diff use).
const SEND_UNKNOWN_REPLY: &str = "发送结果未知，为避免重复未重试。";

/// The reply for a definite non-send (over Feishu's upload cap, or a failed
/// upload/send).
const SEND_FAILED_REPLY: &str = "⚠️ 文件发送失败，请稍后重试。";

/// Execute `/diff`: claim the chat's cooldown, read the review in the
/// conversation's current project directory, then answer with one text line or
/// one HTML File Message. `base` is the parsed `--base`; when absent the
/// configured `[review] base` applies, and when that is absent #805 resolves
/// `origin/main` → `main`.
pub(crate) async fn run(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    message_id: &str,
    base: Option<String>,
) -> crate::error::Result<()> {
    // The claim runs before any git/render/upload work, so a repeat click can
    // never pay for the expensive path (spec #803 decision 5).
    if !claim(handles, &thread_key.chat_id) {
        handles
            .flow
            .platform
            .reply_text(message_id, COOLDOWN_REPLY)
            .await?;
        return Ok(());
    }

    let directory = handles.current_project_directory(thread_key).await;
    let base = base.or_else(|| handles.config.review_base().map(str::to_owned));
    let review = match crate::review::review_diff(&directory, base.as_deref()).await {
        Ok(review) => review,
        Err(error) => {
            // The producer's Display is the readable, distinct failure (not a
            // git repository, base not found, merge-base failed, git missing,
            // permission/IO, timeout): name it instead of swallowing it.
            handles
                .flow
                .platform
                .reply_text(message_id, &format!("⚠️ /diff 失败：{error}"))
                .await?;
            return Ok(());
        }
    };

    if review.document.files.is_empty() && review.document.truncated.is_none() {
        handles
            .flow
            .platform
            .reply_text(
                message_id,
                &format!("分支 {} 相对 {} 没有改动", review.branch, review.base),
            )
            .await?;
        return Ok(());
    }

    tracing::debug!(
        "repo review: branch {} base {} merge-base {} files {}",
        review.branch,
        review.base,
        review.merge_base,
        review.document.files.len()
    );
    let html = crate::backend::render_diff_html(&review.document);
    let name = diff_file_name(&review.branch, &review.short_sha);
    let content = crate::backend::FileContent::from_bytes("text/html", &name, html.into_bytes());
    let hash = crate::bridge::handles::content_identity_hash(
        &content.mime,
        &content.name,
        content.data.as_deref().unwrap_or_default(),
    );
    // The reply target is the command message only (no `reply_in_thread`,
    // ADR-0076), so the file lands in the command's own chat/topic.
    let delivery = crate::bridge::viewer::send_file_message(
        &handles.flow.platform,
        &handles.flow.cards,
        message_id,
        &content,
        hash,
        handles.flow.cards.viewer_transfer_timeout_ms(),
    )
    .await;
    match delivery {
        FileDelivery::SentAsFile => {}
        FileDelivery::FileSendOutcomeUnknown => {
            handles
                .flow
                .platform
                .reply_text(message_id, SEND_UNKNOWN_REPLY)
                .await?;
        }
        _ => {
            handles
                .flow
                .platform
                .reply_text(message_id, SEND_FAILED_REPLY)
                .await?;
        }
    }
    Ok(())
}

/// Claim `chat_id` in the shared cooldown. The tiny lock is held only for the
/// map operation — never across an await.
fn claim(handles: &CommandHandles, chat_id: &str) -> bool {
    let mut cooldown = handles
        .diff_cooldown
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    cooldown.claim(chat_id, Instant::now())
}

/// The artifact's file name: `review-<sanitized-branch>-<short-sha>.html`.
/// Every character outside `[A-Za-z0-9._-]` becomes `_` (a branch may carry
/// `/` and HTML-special characters), and the `.html` suffix is always kept.
/// `detached`/`unborn` need no special case: #805 already reports them as the
/// branch and short SHA, yielding `review-detached-<short>.html` and
/// `review-unborn-unborn.html`.
pub(crate) fn diff_file_name(branch: &str, short_sha: &str) -> String {
    format!("review-{}-{}.html", sanitize(branch), sanitize(short_sha))
}

/// Keep only `[A-Za-z0-9._-]`; everything else (including non-ASCII) is `_`.
fn sanitize(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The process-internal `/diff` cooldown: one last-claim instant per chat, so a
/// double click inside the window answers text instead of rendering and
/// uploading a second file. A value type with an injected `now`, so its window
/// logic is tested without sleeping.
#[derive(Debug)]
pub(crate) struct DiffCooldown {
    /// How long a claim stays live.
    window: Duration,
    /// chat_id → the instant of its last successful claim. Stale entries are
    /// evicted on every claim, so the map is bounded by the chats active in one
    /// window, never by every chat a process ever saw.
    last_claim: HashMap<String, Instant>,
}

impl Default for DiffCooldown {
    fn default() -> Self {
        Self::new(DIFF_COOLDOWN)
    }
}

impl DiffCooldown {
    pub(crate) fn new(window: Duration) -> Self {
        Self {
            window,
            last_claim: HashMap::new(),
        }
    }

    /// Whether a `/diff` from `chat_id` may run at `now`. A claim inside the
    /// window is refused; an expired one is replaced.
    pub(crate) fn claim(&mut self, chat_id: &str, now: Instant) -> bool {
        self.last_claim
            .retain(|_, at| now.saturating_duration_since(*at) < self.window);
        if self.last_claim.contains_key(chat_id) {
            return false;
        }
        self.last_claim.insert(chat_id.to_string(), now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_branch_is_sanitized_and_always_keeps_the_html_suffix() {
        assert_eq!(
            diff_file_name("feature/diff", "abc1234"),
            "review-feature_diff-abc1234.html"
        );
        let name = diff_file_name("feat/<b>&\"x\"", "abc1234");
        assert_eq!(name, "review-feat__b___x_-abc1234.html");
        assert!(name.ends_with(".html"), "{name}");
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
            "{name}"
        );
    }

    #[test]
    fn detached_and_unborn_branches_still_produce_the_pinned_names() {
        assert_eq!(
            diff_file_name("detached", "abc1234"),
            "review-detached-abc1234.html"
        );
        assert_eq!(diff_file_name("unborn", "unborn"), "review-unborn-unborn.html");
    }

    #[test]
    fn a_non_ascii_branch_is_replaced_not_dropped() {
        let name = diff_file_name("功能/登录", "abc1234");
        assert_eq!(name, "review-_____-abc1234.html");
        assert!(name.ends_with(".html"));
    }

    #[test]
    fn the_first_claim_wins_a_repeat_inside_the_window_loses() {
        let start = t0();
        let mut cooldown = DiffCooldown::new(Duration::from_secs(10));
        assert!(cooldown.claim("chat_1", start));
        assert!(!cooldown.claim("chat_1", start + Duration::from_secs(9)));
        // The refused claim must not extend the window.
        assert!(!cooldown.claim("chat_1", start + Duration::from_secs(9)));
    }

    #[test]
    fn the_window_expiry_releases_the_chat() {
        let start = t0();
        let mut cooldown = DiffCooldown::new(Duration::from_secs(10));
        assert!(cooldown.claim("chat_1", start));
        assert!(cooldown.claim("chat_1", start + Duration::from_secs(10)));
    }

    #[test]
    fn chats_are_independent() {
        let start = t0();
        let mut cooldown = DiffCooldown::new(Duration::from_secs(10));
        assert!(cooldown.claim("chat_1", start));
        assert!(cooldown.claim("chat_2", start));
        assert!(!cooldown.claim("chat_1", start + Duration::from_secs(1)));
        assert!(!cooldown.claim("chat_2", start + Duration::from_secs(1)));
    }

    #[test]
    fn stale_entries_are_evicted_so_the_map_stays_bounded() {
        let start = t0();
        let mut cooldown = DiffCooldown::new(Duration::from_secs(10));
        for chat in ["a", "b", "c"] {
            assert!(cooldown.claim(chat, start));
        }
        assert_eq!(cooldown.last_claim.len(), 3);
        // A later claim evicts every expired chat before inserting the new one.
        assert!(cooldown.claim("d", start + Duration::from_secs(11)));
        assert_eq!(cooldown.last_claim.len(), 1);
        assert!(cooldown.last_claim.contains_key("d"));
    }
}
