//! The permission companion lane (ADR-0083 §5, ADR-0102): one Rich Text
//! Message carrying the full content, falling to one File Message, delivered
//! before any interactive permission surface exists. The request-kind module
//! asks for the outcome; the transport policy lives here.

use crate::bridge::handles::FlowHandles;
use crate::bridge::pollers::{CardTarget, resolve_card_target};
use crate::bridge::surfaces::{CompanionDelivery, CompanionLane, Surfaces};

use super::kind::{PermissionOverflow, describe_action, describe_permission, permission_overflow};

/// The exact content a companion carries and the digest covers.
///
/// A **targets** overflow delivers the raw targets: `describe_permission`
/// clips each one to the display budget, which would drop the very text the
/// companion exists to show. A **rendered-body** overflow delivers the full
/// rendered description, so the chrome the proof charged (reason, metadata,
/// header) is in the companion too rather than being "proved" by text that
/// never contained the clipped part. The digest is over exactly this text.
pub(crate) fn companion_text(p: &crate::backend::types::PermissionRequest) -> String {
    match permission_overflow(p) {
        Some(PermissionOverflow::Targets) => p.patterns.join("\n\n"),
        _ => describe_permission(p),
    }
}

/// The stable digest of [`companion_text`]. It is a fingerprint, never the
/// content: the record is persisted beside the other surfaces.
pub(crate) fn companion_target_digest(p: &crate::backend::types::PermissionRequest) -> u64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(companion_text(p).as_bytes());
    u64::from_be_bytes(digest[..8].try_into().expect("sha256 is 32 bytes"))
}

/// The summary form of a permission for an over-budget request (ADR-0102): the
/// action, the total character count, the first 120 characters of the target,
/// and where the full content lives. Callers pass a **confirmed** delivery
/// (`delivered_companion`); an uncertain marker never reaches a card.
pub(crate) fn describe_permission_summary(
    p: &crate::backend::types::PermissionRequest,
    delivery: &CompanionDelivery,
) -> String {
    let action = p.permission.as_deref().unwrap_or("?");
    let (emoji, label) = describe_action(action);
    // The excerpt is the target text, not the rendered body: the body's first
    // line already repeats the action above, and the full content (including
    // its chrome) travels in the companion.
    let excerpt: String = p.patterns.join("\n\n").chars().take(120).collect();
    let mut s = format!(
        "{emoji} **{label}**\n**目标长度**: {} 字符\n**摘要**:\n{}\n**完整内容**: 已通过{}发送。",
        p.total_target_len(),
        crate::feishu::card::fenced_code(&excerpt, None),
        delivery.lane.label()
    );
    s.push_str("\n\nAI 想要执行这个操作，是否允许？");
    s
}

/// The body a permission surface renders: the summary when a companion carried
/// the full content, the ordinary full description otherwise. ONE chooser, so
/// the card, the inline block and the stale summary cannot disagree.
pub(crate) fn describe_permission_for(
    p: &crate::backend::types::PermissionRequest,
    companion: Option<&CompanionDelivery>,
) -> String {
    match companion {
        Some(delivery) => describe_permission_summary(p, delivery),
        None => describe_permission(p),
    }
}

/// What [`ensure_permission_companion`] did.
pub(crate) enum CompanionOutcome {
    /// The send landed and returned a message id.
    Delivered,
    /// The send may have landed (a timeout or a post-accept 5xx) but returned
    /// no id. It is terminal: a retry could duplicate the message, so the
    /// record carries an empty message id. It is not a delivery proof, so the
    /// caller refuses the request and names the unknown outcome.
    Uncertain,
    /// No lane delivered: the request must follow the refusal path.
    Failed,
}

/// The Rich Text body a companion can carry, or `None` when the lane must fall
/// to the File Message.
///
/// A **targets** overflow is fenced with its action header and sized by the
/// existing `rich_post_md` helper. A **body** overflow (or a defensive `None`)
/// is already the renderer's markdown and is sent as-is, but still guarded by
/// the same 30 KB post cap: the 3,060-character body ceiling is the proof's
/// *trigger*, not the post bound, so this check keeps ADR-0083 §5's size step
/// true if that render bound ever moves.
pub(crate) fn companion_rich_body(
    body: &str,
    overflow: Option<PermissionOverflow>,
    action_header: &str,
) -> Option<String> {
    match overflow {
        Some(PermissionOverflow::Targets) => crate::feishu::card::viewer::rich_post_md(
            crate::feishu::card::viewer::ViewerKind::Text,
            Some("txt"),
            body,
        )
        .map(|fenced| format!("{action_header}{fenced}"))
        .filter(|md| {
            crate::feishu::card::sanitize::serialized_len(md) + 128
                <= crate::feishu::card::FEISHU_CARD_LIMIT_BYTES
        }),
        // Defensive: the rendered body is normally far below the post cap, but
        // the ladder rule must still fall to the File Message if it is not.
        _ => (crate::feishu::card::sanitize::serialized_len(body) + 128
            <= crate::feishu::card::FEISHU_CARD_LIMIT_BYTES)
            .then_some(body.to_string()),
    }
}

/// Record a non-proof marker for a send whose outcome is unknown. It prevents a
/// duplicate companion but never validates an approval (ADR-0102).
fn mark_uncertain(
    surfaces: &Surfaces,
    p: &crate::backend::types::PermissionRequest,
    lane: CompanionLane,
    target_digest: u64,
) -> CompanionOutcome {
    let delivery = CompanionDelivery {
        message_id: String::new(),
        lane,
        target_digest,
    };
    surfaces.set_companion(&p.request_id, delivery);
    CompanionOutcome::Uncertain
}

/// Deliver an over-budget permission's full content as one companion message,
/// before any interactive surface exists (ADR-0083 §5's ladder, ADR-0102).
///
/// A matching record already written for this request is trusted and never
/// re-sent; a record whose digest no longer matches is discarded and replaced.
/// The Rich Text Message is tried first via the existing
/// [`crate::feishu::card::viewer::rich_post_md`] sizing; a body past its 30 KB
/// cap, or a send that **definitely** did not land, falls to the File Message
/// lane through [`crate::bridge::viewer::send_file_message_with_outcome`]. An
/// ambiguous send is terminal — it must not be retried into a duplicate — and
/// leaves only a non-proof marker; the caller offers no interactive approval.
pub(crate) async fn ensure_permission_companion(
    handles: &FlowHandles,
    surfaces: &Surfaces,
    p: &crate::backend::types::PermissionRequest,
    directory: &str,
) -> CompanionOutcome {
    let digest = companion_target_digest(p);
    if let Some(existing) = surfaces.matching_companion(&p.request_id, digest) {
        return if existing.is_uncertain() {
            CompanionOutcome::Uncertain
        } else {
            CompanionOutcome::Delivered
        };
    }
    // A record for a different digest is stale: replace it, never trust it.
    surfaces.remove_companion(&p.request_id);
    let session_id = p.session_id.as_deref().unwrap_or("");
    let Some(target) = resolve_card_target(
        &handles.sessions,
        &handles.cards,
        &handles.backend,
        &handles.platform,
        session_id,
        directory,
    )
    .await
    else {
        tracing::warn!(
            "permission {} companion: no reply target on session {session_id}",
            p.request_id
        );
        return CompanionOutcome::Failed;
    };
    let CardTarget::ReplyTo(message_id) = target else {
        // A Chat target has no message to reply to, and only a card can be sent
        // to a chat; the companion lanes require a reply message. Treat it as a
        // failed delivery so the request is never offered without its content.
        tracing::warn!(
            "permission {} companion: only a chat target resolves; cannot deliver the full content",
            p.request_id
        );
        return CompanionOutcome::Failed;
    };

    let body = companion_text(p);
    let action = p.permission.as_deref().unwrap_or("?");
    let (emoji, label) = describe_action(action);
    // A TARGET overflow is raw text, so it gets the action header and a fenced
    // block. A BODY overflow is already the renderer's markdown (action,
    // metadata, footer); send it as-is — fencing it would show the markdown
    // literally and prepending the action would repeat it.
    let action_header = format!("{emoji} **{label}**\n\n");
    let rich = companion_rich_body(&body, permission_overflow(p), &action_header);
    if let Some(md) = rich {
        let content = crate::feishu::card::viewer::rich_post_content(&md);
        match crate::bridge::bounded_call(
            &format!("permission {} companion post", p.request_id),
            COMPANION_SEND_TIMEOUT_MS,
            handles.platform.reply_message(&message_id, "post", &content),
        )
        .await
        {
            Some(Ok((sent_id, _))) if !sent_id.is_empty() => {
                let delivery = CompanionDelivery {
                    message_id: sent_id,
                    lane: CompanionLane::RichText,
                    target_digest: digest,
                };
                surfaces.set_companion(&p.request_id, delivery.clone());
                tracing::info!(
                    "permission {} companion delivered as a Rich Text Message ({})",
                    p.request_id,
                    delivery.message_id
                );
                return CompanionOutcome::Delivered;
            }
            Some(Ok(_)) => {
                // A success without a message id lands in the marker half: the
                // two definitions of "delivered" must agree.
                tracing::warn!(
                    "permission {} companion Rich Text Message returned no message id; \
                     recording an uncertain marker",
                    p.request_id
                );
                return mark_uncertain(surfaces, p, CompanionLane::RichText, digest);
            }
            Some(Err(error)) if error.is_definite_non_delivery() => {
                tracing::warn!(
                    "permission {} companion Rich Text Message definitely failed: {error}; \
                     trying the File Message lane",
                    p.request_id
                );
            }
            Some(Err(error)) => {
                // The post may have landed after Feishu accepted it. Falling to
                // the File lane could duplicate the whole content. Record only a
                // no-duplicate marker; the caller refuses the request because
                // this is not a delivery proof.
                tracing::warn!(
                    "permission {} companion Rich Text Message outcome unknown: {error}; \
                     not retrying",
                    p.request_id
                );
                return mark_uncertain(surfaces, p, CompanionLane::RichText, digest);
            }
            None => {
                tracing::warn!(
                    "permission {} companion Rich Text Message timed out; not retrying",
                    p.request_id
                );
                return mark_uncertain(surfaces, p, CompanionLane::RichText, digest);
            }
        }
    }

    // File Message lane: the bytes are the full content, under a name that
    // names the request, with a following caption naming it too (Feishu's
    // `file` payload carries only `file_key`, so a caption cannot ride it).
    let safe_id: String = p
        .request_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = format!("permission-{safe_id}.txt");
    let content = crate::backend::FileContent::from_bytes("text/plain", &name, body.into_bytes());
    let hash = crate::bridge::handles::content_identity_hash(
        &content.mime,
        &content.name,
        content.data.as_deref().unwrap_or_default(),
    );
    let caption = format!(
        "📎 权限请求 {}：完整内容（{} 字符）已作为 .txt 文件发送。",
        p.request_id,
        p.total_target_len()
    );
    let (delivery, sent_id) = crate::bridge::viewer::send_file_message_with_outcome(
        &handles.platform,
        &handles.cards,
        &message_id,
        &content,
        hash,
        COMPANION_SEND_TIMEOUT_MS,
        Some(&caption),
    )
    .await;
    match delivery {
        crate::feishu::card::tool_render::FileDelivery::SentAsFile => {
            match sent_id.filter(|id| !id.is_empty()) {
                Some(message_id) => {
                    let delivery = CompanionDelivery {
                        message_id,
                        lane: CompanionLane::File,
                        target_digest: digest,
                    };
                    surfaces.set_companion(&p.request_id, delivery.clone());
                    tracing::info!(
                        "permission {} companion delivered as a File Message ({})",
                        p.request_id,
                        delivery.message_id
                    );
                    CompanionOutcome::Delivered
                }
                None => {
                    // A success without a message id is a marker, not a proof.
                    tracing::warn!(
                        "permission {} companion File Message returned no message id; \
                         recording an uncertain marker",
                        p.request_id
                    );
                    mark_uncertain(surfaces, p, CompanionLane::File, digest)
                }
            }
        }
        crate::feishu::card::tool_render::FileDelivery::FileSendOutcomeUnknown => {
            tracing::warn!(
                "permission {} companion File Message outcome unknown; not retrying",
                p.request_id
            );
            mark_uncertain(surfaces, p, CompanionLane::File, digest)
        }
        _ => {
            tracing::warn!("permission {} companion File Message did not land", p.request_id);
            CompanionOutcome::Failed
        }
    }
}

/// How long one companion send (or upload) may take. It runs in the sweep,
/// before the request becomes interactive, so a stalled call must not hold the
/// poller. A timeout is an UNCERTAIN send — it may have landed, so it is
/// recorded and never retried rather than treated as a failed lane.
const COMPANION_SEND_TIMEOUT_MS: u64 = 10_000;

#[cfg(test)]
mod tests {
    use super::*;

    /// The ladder's 30 KB step stays true for a rendered-body companion if the
    /// render bound ever moves: an oversized body takes the File lane.
    #[test]
    fn a_body_past_the_post_cap_takes_the_file_lane() {
        let body = "x".repeat(crate::feishu::card::FEISHU_CARD_LIMIT_BYTES + 1);
        assert!(
            companion_rich_body(
                &body,
                Some(PermissionOverflow::Body),
                "⚡ **执行 Shell 命令**\n\n"
            )
            .is_none(),
            "a body past the post cap must fall to the File Message"
        );
    }

    /// A body overflow is sent as the renderer's markdown once — no fence and no
    /// repeated action header.
    #[test]
    fn a_body_overflow_is_sent_as_markdown_once() {
        let body = "⚡ **执行 Shell 命令**\n**对象**:\n- `ls`\n\nAI 想要执行这个操作，是否允许？";
        let md = companion_rich_body(body, Some(PermissionOverflow::Body), "⚡ **执行 Shell 命令**\n\n")
            .expect("a normal rendered body fits");
        assert_eq!(md, body);
        assert_eq!(md.matches("执行 Shell 命令").count(), 1);
        assert!(!md.starts_with("```"));
    }

    /// A targets overflow still takes the fenced lane and its own post cap.
    #[test]
    fn a_target_past_the_post_cap_takes_the_file_lane() {
        let target = "x".repeat(crate::feishu::card::FEISHU_CARD_LIMIT_BYTES + 1);
        assert!(
            companion_rich_body(
                &target,
                Some(PermissionOverflow::Targets),
                "⚡ **执行 Shell 命令**\n\n"
            )
            .is_none()
        );
    }
}
