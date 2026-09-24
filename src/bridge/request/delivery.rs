//! The card-delivery helpers (spec #298, ticket C): how a resolved request
//! reaches every card that renders its block — the one resolution seam
//! ([`resolve_blocks`]), the callback-ack settling, and the neutral receipt
//! vocabulary shared by every request kind.

use crate::bridge::handler::CardActionResult;
use crate::bridge::handles::{CardsHandle, RequestsHandle};
use crate::bridge::pollers::result_card;
use crate::bridge::turn::Turn;
use crate::opencode;

use super::flow::RequestFlow;

/// Prefix of the receipt left when a click discovers the request was already
/// resolved by another client (a 404 reply): neutral — never claims cola
/// decided. The sweep adopts the same line for a block resolved remotely
/// (#175).
pub(super) const HANDLED_ELSEWHERE_PREFIX: &str = "⏱ 已由其他客户端处理";

/// Receipt prefix for a denied permission or a rejected question.
pub(super) const DENIED_PREFIX: &str = "🚫 已拒绝";

/// Cap for an Interaction Receipt line: a receipt is a one-line residue, not
/// a report.
pub(super) const RECEIPT_MAX_CHARS: usize = 120;

/// Compose one receipt line: `prefix：detail`, clipped to stay a residue. A
/// block with no derivable detail (should not happen for a live block) keeps
/// the bare prefix.
pub(super) fn receipt_line(prefix: &str, detail: &str) -> String {
    if detail.is_empty() {
        prefix.to_string()
    } else {
        truncate(&format!("{prefix}：{detail}"), RECEIPT_MAX_CHARS)
    }
}

/// Receipt for a block a click found already resolved elsewhere.
pub(crate) fn handled_elsewhere_receipt(target: &str) -> String {
    receipt_line(HANDLED_ELSEWHERE_PREFIX, target)
}

/// Receipt for a denied permission or a rejected question.
pub(super) fn denied_receipt(target: &str) -> String {
    receipt_line(DENIED_PREFIX, target)
}

/// The error-result block, shared by every kind: a red "处理失败" card (kept in
/// place when answered inline), with a toast that explains the failure.
pub(super) fn failed_result_card(inline: bool, body: &str, toast: &str) -> CardActionResult {
    let mut r = result_card("⚠️ 处理失败", "red", body);
    if inline {
        r.card = None;
    }
    r.toast = Some(toast.to_string());
    r
}

/// The neutral result for a request the backend no longer has (404): it was
/// already resolved — by another client, or by a click replayed after a cola
/// restart cleared the in-memory guard. NOT a failure, so it must never render
/// the red failure card. Reuses the stale-card body so both "resolved
/// elsewhere" paths look identical.
pub(super) fn already_handled_result(kind: &str, inline: bool, toast: &str) -> CardActionResult {
    let mut r = CardActionResult {
        card: Some(crate::feishu::card::notify::build_resolved_elsewhere_card(
            kind, "",
        )),
        toast: Some(toast.to_string()),
    };
    if inline {
        r.card = None;
    }
    r
}

/// Where a resolution came from — decides how its cards are settled.
pub(crate) enum Origin<'a> {
    /// A card callback: `clicked` is the message id the callback carried (the
    /// card the Host actually clicked, when the platform supplies it). That
    /// card's edited JSON is the atomic ack (ADR-0038, rule 3), and a
    /// DIFFERENT registered card is patched. Without an id the ack is rebuilt
    /// from the accumulator and NO card is patched — the ack IS the update, so
    /// no PATCH may race behind it.
    Click { clicked: Option<&'a str> },
    /// No callback at all (a command, e.g. `/autoaccept on`): every card that
    /// renders a block is patched eagerly.
    Command,
}

/// How a resolution leaves its residue on the card and in the timeline
/// (ADR-0038, rule 4).
pub(crate) enum Residue<'a> {
    /// One receipt per resolved block, naming that block's own target — a
    /// click on the block's own controls.
    PerBlock(&'a (dyn Fn(&str) -> String + Send + Sync)),
    /// ONE receipt for the whole resolution: a mode change resolves every
    /// pending block at once and reports the mode, not the requests it
    /// swallowed. The rest are dismissed without lines of their own, so a card
    /// never carries the same line twice.
    Single(&'a str),
}

/// Apply one block's residue to the cached card `card_id`: a per-block receipt
/// line, or — for a mode change — the single mode line on the first block that
/// card renders and a plain removal for the rest. `stamped_cards` remembers which
/// cards already carry the mode line. `None` when that card does not render the
/// block.
fn residue_edit(
    handles: &mut crate::bridge::card_handles::CardHandles,
    card_id: &str,
    request_id: &str,
    residue: &Residue<'_>,
    stamped_cards: &mut std::collections::HashSet<String>,
) -> Option<serde_json::Value> {
    match residue {
        Residue::PerBlock(line) => {
            let text = handles.target_of(request_id).map(line)?;
            handles.resolve_on(card_id, request_id, &text)
        }
        Residue::Single(text) => {
            if stamped_cards.contains(card_id) {
                return handles.remove_on(card_id, request_id);
            }
            let card = handles.resolve_on(card_id, request_id, text)?;
            stamped_cards.insert(card_id.to_string());
            Some(card)
        }
    }
}

/// Apply one block's residue to the cached card `card_id` and stack the
/// repainted card into `patches` (one PATCH per card), restamping its header
/// from the accumulator's post-resolution state.
fn repaint_card(
    handles: &mut crate::bridge::card_handles::CardHandles,
    card_id: &str,
    request_id: &str,
    residue: &Residue<'_>,
    stamped_cards: &mut std::collections::HashSet<String>,
    header: Option<&(String, &'static str)>,
    patches: &mut Vec<(String, serde_json::Value)>,
) {
    if let Some(card) = residue_edit(handles, card_id, request_id, residue, stamped_cards) {
        crate::bridge::card_handles::merge_patch(
            patches,
            card_id.to_string(),
            restamped_header(card, header),
        );
    }
}

/// Resolve a set of answered request ids on EVERY surface that renders their
/// blocks (ADR-0038, rule 2) — the one mutation seam, so the accumulator and
/// the card handles cannot drift:
///
/// 1. `sent_cards` forgets them (the poller stops delivering standalone cards).
/// 2. The host accumulator that still carries a block resolves it into its
///    timeline receipt; the next flush (new part, header tick, split)
///    re-renders it in its transcript position.
/// 3. Every card handle that renders one of them gets its cached JSON edited
///    in place — the clicked card's edit becomes the callback ack, and another
///    card's edit is patched eagerly. The registry entry is then dropped, so a
///    resolved block never lingers.
///
/// `origin` decides how the cards are settled and `residue` what each block
/// leaves behind (ADR-0038, rules 3+4). Returns the clicked card's edited
/// JSON, when the click landed on a card carrying one of the blocks.
#[allow(clippy::too_many_arguments)] // the resolution seam: flow + the two handles it settles on + origin/residue
pub(crate) async fn resolve_blocks(
    flow: &RequestFlow,
    cards: &CardsHandle,
    requests: &RequestsHandle,
    host: &Option<String>,
    session_id: &str,
    origin: Origin<'_>,
    ids: &[String],
    residue: Residue<'_>,
) -> Option<serde_json::Value> {
    // The session's card-write lock, shared with `flush_card`: a resolution
    // settles the accumulator and the cached cards, and a flush that
    // snapshotted before it must not record its stale copy after — the block
    // would come back to life and the sweep would report cola's own decision
    // as another client's.
    let write_lock = cards.write_lock(host.as_deref().unwrap_or(session_id)).await;
    let _guard = write_lock.lock().await;
    // A click settles the standalone surface too, so its `sent_cards` entry
    // goes. A COMMAND does not: an id with no inline block and no card handle
    // is a standalone card, and clearing its entry here would strand its live
    // buttons — `mark_stale_cards` repaints standalone cards and owns that
    // lifecycle (ADR-0038, rule 6). Mixed surfaces (standalone copy + inline
    // block) keep their entry; the sweep marks the standalone copy while this
    // seam settles the inline one.
    if matches!(origin, Origin::Click { .. }) {
        {
            let mut sent = flow.sent_cards.lock().await;
            for id in ids {
                sent.remove(id);
            }
        }
        for id in ids {
            flow.surfaces.remove_standalone(id);
        }
    }
    // 1. The accumulator that still carries a block is the render source: a
    //    receipt must live in its timeline, or the next flush re-adds the
    //    block to the card the ack just cleaned. Resolving the last live block
    //    also changes the HEADER (the awaiting-action title lifts), so
    //    the post-resolution header is captured here and restamped onto every
    //    card this resolution edits — otherwise a click leaves the clicked
    //    card titled "waiting" until the next render-poll flush (~2 s).
    //    Only an accumulator that ACTUALLY resolved one of the blocks may
    //    restamp: a card whose own turn is gone (its block lives only in the
    //    handle cache) has no header source, and wearing another turn's live
    //    header would be a lie. The restamp also cannot overwrite a newer
    //    card's header: a flush that ran after this resolution rebuilt the
    //    card without this block's span, so `resolve_on` finds nothing and
    //    the ack falls back to a fresh rebuild.
    let residue_mode = match &residue {
        Residue::PerBlock(line) => crate::bridge::turn::InlineResidue::PerBlock(*line),
        Residue::Single(text) => crate::bridge::turn::InlineResidue::Single(text),
    };
    let post_resolution_header =
        Turn::resolve_interactions(cards, host.as_deref().unwrap_or(session_id), ids, &residue_mode).await;
    // 2. The card handles: every card that rendered one of the blocks is
    //    edited from its cache. The clicked card's edit is the ack; any other
    //    card's edit is patched so its controls do not linger.
    let mut ack = None;
    let mut patches: Vec<(String, serde_json::Value)> = Vec::new();
    // Cards that already carry a mode line; their remaining blocks are removed
    // without one (see `residue_edit`).
    let mut stamped_cards: std::collections::HashSet<String> = std::collections::HashSet::new();
    {
        let mut handles = cards.card_handles.lock().await;
        for id in ids {
            match origin {
                Origin::Click {
                    clicked: Some(clicked_id),
                } => {
                    // The clicked card carried the block: its edited JSON is
                    // the atomic callback ack (ADR-0038, rule 3).
                    if let Some(card) =
                        residue_edit(&mut handles, clicked_id, id, &residue, &mut stamped_cards)
                    {
                        ack = Some(restamped_header(card, post_resolution_header.as_ref()));
                    }
                    // The registered card is a different one (the block moved,
                    // or the click landed on a stale copy): repaint it so its
                    // controls do not linger.
                    if let Some(registered) = handles.message_of(id).map(str::to_string)
                        && registered != clicked_id
                    {
                        repaint_card(
                            &mut handles,
                            &registered,
                            id,
                            &residue,
                            &mut stamped_cards,
                            post_resolution_header.as_ref(),
                            &mut patches,
                        );
                    }
                }
                // A callback with no card id: the ack is rebuilt from the
                // accumulator, so no card may be patched behind it.
                Origin::Click { clicked: None } => {}
                // No callback (a command resolved the block): every card that
                // still renders it is patched — the registered one and any
                // older copy a re-host left behind. Only the registered card
                // (the accumulator's current card) gets the header restamp:
                // an older copy may belong to a previous turn, and wearing
                // another turn's live header would be a lie.
                Origin::Command => {
                    for card_id in handles.cards_rendering(id) {
                        let registered = handles.message_of(id) == Some(card_id.as_str());
                        let header = if registered {
                            post_resolution_header.as_ref()
                        } else {
                            None
                        };
                        repaint_card(
                            &mut handles,
                            &card_id,
                            id,
                            &residue,
                            &mut stamped_cards,
                            header,
                            &mut patches,
                        );
                    }
                }
            }
            handles.forget(id);
        }
    }
    for (message_id, card) in patches {
        if let Err(e) = cards.feishu.update_message(&message_id, &card).await {
            tracing::warn!("resolved block repaint failed on {}: {}", message_id, e);
        }
    }
    // The settlement rendered (or found nothing to render): release the
    // in-flight claim, so a standalone copy of the same request is left to
    // `mark_stale_cards` again.
    {
        let mut settling = requests.settling_requests.lock().await;
        for id in ids {
            settling.remove(id);
        }
    }
    ack
}

/// Restamp an edited card's header title and template from the accumulator's
/// post-resolution state, keeping the header's subtitle (the session/date
/// line) untouched. The cached card's own header was captured while the block
/// was still live, so without this a click would leave the "等待你的授权 /
/// 回答" title showing until the next render-poll flush.
fn restamped_header(
    mut card: serde_json::Value,
    header: Option<&(String, &'static str)>,
) -> serde_json::Value {
    if let Some((title, template)) = header
        && let Some(h) = card.get_mut("header").and_then(|h| h.as_object_mut())
    {
        h.insert(
            "title".to_string(),
            serde_json::json!({ "tag": "plain_text", "content": title }),
        );
        h.insert("template".to_string(), serde_json::json!(template));
    }
    card
}

/// Settle the callback ack for a click (ADR-0038, rule 3): a card-handle edit
/// (the clicked card's cached JSON, updated through the seam) wins; otherwise
/// an inline click re-renders the accumulator's current card and a standalone
/// click keeps its result card.
pub(super) async fn settle_ack(
    cards: &CardsHandle,
    host: &Option<String>,
    session_id: &str,
    inline: bool,
    cached: Option<serde_json::Value>,
    result: &mut CardActionResult,
) {
    if let Some(card) = cached {
        result.card = Some(card);
    } else if inline {
        result.card = ack_inline_card(cards, host, session_id).await;
    }
}

/// Keep a live question block's display state in step everywhere it renders
/// (ADR-0038, rule 2): the accumulator's section — the render source every
/// later flush re-renders the markers from — and, through its handle, the
/// clicked card's cached JSON, whose refreshed copy is the callback ack. One
/// seam, so the two cannot drift. Returns the refreshed clicked card when a
/// handle carries the block.
#[allow(clippy::too_many_arguments)] // the display state is what the seam keeps in step
pub(super) async fn refresh_question_block(
    cards: &CardsHandle,
    host: &Option<String>,
    session_id: &str,
    clicked: Option<&str>,
    req_id: &str,
    request: &opencode::types::QuestionRequest,
    directory: &str,
    display: &[Option<Vec<String>>],
    done: &[bool],
) -> Option<serde_json::Value> {
    Turn::update_question_state(
        cards,
        host.as_deref().unwrap_or(session_id),
        req_id,
        display,
        done,
    )
    .await;
    let message_id = clicked?;
    let elements = crate::feishu::card::question::question_elements(
        req_id,
        &request.session_id,
        &request.questions,
        directory,
        display,
        done,
    );
    cards
        .card_handles
        .lock()
        .await
        .refresh_on(message_id, req_id, elements)
}

/// The clicked card's updated JSON, built from a CLONE of the host accumulator
/// — a split probe that cannot advance the live `render_from` from inside a
/// click handler (`flush_card` owns that flow). `None` when the card needs a
/// split or no accumulator exists; the caller falls back to the PATCH flush.
async fn inline_ack_card(
    cards: &CardsHandle,
    host: &Option<String>,
    session_id: &str,
) -> Option<serde_json::Value> {
    Turn::ack_card(cards, host.as_deref().unwrap_or(session_id)).await
}

/// Deliver the clicked card's updated card in the callback ack (ADR-0038,
/// rule 3): the host accumulator was already mutated through the seam, so the
/// ack carries the receipt / partial state atomically — no PATCH race and no
/// dependence on which card is current. Falls back to the PATCH flush when the
/// card needs a split or the accumulator is gone (the turn ended between the
/// click and now).
pub(super) async fn ack_inline_card(
    cards: &CardsHandle,
    host: &Option<String>,
    session_id: &str,
) -> Option<serde_json::Value> {
    let card = inline_ack_card(cards, host, session_id).await;
    if card.is_none() {
        flush_inline_card(cards, host, session_id).await;
    }
    card
}

/// Re-render the live streaming card so inline interaction state (✅/已选,
/// receipts, removed buttons) shows immediately. An interaction-blocked prompt
/// produces no new parts, so without an explicit flush the render poll never
/// fires until the AI resumes — the card would stay frozen on the pre-answer
/// state. Same reason the question paths flush explicitly.
async fn flush_inline_card(cards: &CardsHandle, host: &Option<String>, session_id: &str) {
    Turn::flush_card(cards, host.as_deref().unwrap_or(session_id)).await;
}

/// Clip `s` to at most `max` characters, appending a "…" marker when it was
/// cut. Character-counted (not bytes) so CJK paths/values truncate at the same
/// visual length as ASCII — mirrors `card::truncate_md`.
pub(super) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}
