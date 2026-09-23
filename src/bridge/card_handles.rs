use std::collections::{HashMap, HashSet};

use crate::bridge::snapshot_claims::ClaimKind;
use crate::bridge::turn::state::{BlockSpan, InteractionBlock};

/// One live interaction block as rendered onto a card: its element range plus
/// what the registry must remember about it so it stays resolvable after the
/// accumulator that rendered it is gone (ADR-0038, rule 2).
pub struct RenderedBlock {
    pub request_id: String,
    pub start: usize,
    pub end: usize,
    pub kind: ClaimKind,
    pub session_id: String,
    pub directory: String,
    /// Compact one-line receipt target, recorded from the block at render time.
    pub target: String,
}

impl RenderedBlock {
    /// The registry record of `block` for the span its elements occupy.
    pub fn of(span: &BlockSpan, block: &InteractionBlock) -> Option<Self> {
        let kind = match block {
            InteractionBlock::Permission(_) => ClaimKind::Permission,
            InteractionBlock::Question(_) => ClaimKind::Question,
            InteractionBlock::Receipt(_) => return None,
        };
        Some(Self {
            request_id: span.request_id.clone(),
            start: span.start,
            end: span.end,
            kind,
            session_id: block.session_id().to_string(),
            directory: block.directory().to_string(),
            target: block.receipt_target(),
        })
    }
}

/// A card handle: which card shows a live block, and what resolving it needs
/// when no accumulator carries it anymore.
struct BlockHandle {
    message_id: String,
    kind: ClaimKind,
    session_id: String,
    directory: String,
    target: String,
}

/// A card that shows at least one live interaction block: the JSON exactly as
/// last sent to Feishu (a surgical edit stays under the card size cap) plus the
/// element range of every live block it renders.
struct CachedCard {
    card: serde_json::Value,
    spans: Vec<BlockSpan>,
}

/// The card-handle registry (ADR-0038, rule 2): every card that shows a live
/// interaction block is repaintable through it. `blocks` maps a request to the
/// card that renders it; `cards` caches each such card's last-rendered JSON and
/// the blocks' element ranges. Both change together — `record` is the only
/// insert path, `resolve_on`/`refresh_on`/`drop_vanished` the only edit paths,
/// and a card's cache is released once no live block references it — so the
/// registry cannot drift from what the cards actually render.
#[derive(Default)]
pub struct CardHandles {
    blocks: HashMap<String, BlockHandle>,
    cards: HashMap<String, CachedCard>,
}

impl CardHandles {
    /// Record a just-sent card: cache its JSON and point every live block it
    /// renders at it. Called for every card the flush sends — the live card
    /// update and each continuation — so after a split the blocks follow the
    /// continuation that carries them. A card with no live blocks releases its
    /// cache: a finalized slice renders no controls anymore. The registry
    /// entries survive that release until the continuation records them, so a
    /// briefly unmatched entry is expected — resolving it just drops the entry,
    /// since there is nothing to repaint.
    pub fn record(&mut self, message_id: &str, card: &serde_json::Value, rendered: Vec<RenderedBlock>) {
        if rendered.is_empty() {
            self.cards.remove(message_id);
            return;
        }
        let mut spans = Vec::with_capacity(rendered.len());
        for block in rendered {
            spans.push(BlockSpan {
                request_id: block.request_id.clone(),
                start: block.start,
                end: block.end,
            });
            self.blocks.insert(
                block.request_id,
                BlockHandle {
                    message_id: message_id.to_string(),
                    kind: block.kind,
                    session_id: block.session_id,
                    directory: block.directory,
                    target: block.target,
                },
            );
        }
        self.cards.insert(
            message_id.to_string(),
            CachedCard {
                card: card.clone(),
                spans,
            },
        );
    }

    /// The card currently showing `request_id`'s live block.
    pub fn message_of(&self, request_id: &str) -> Option<&str> {
        self.blocks.get(request_id).map(|h| h.message_id.as_str())
    }

    /// The recorded receipt target of `request_id` — what a resolution names
    /// when the accumulator no longer carries the block.
    pub fn target_of(&self, request_id: &str) -> Option<&str> {
        self.blocks.get(request_id).map(|h| h.target.as_str())
    }

    /// Every cached card that still renders `request_id`'s block: the
    /// registered one, plus any older copy the registry no longer points at
    /// (a re-host leaves one behind until its own edit).
    pub fn cards_rendering(&self, request_id: &str) -> Vec<String> {
        self.cards
            .iter()
            .filter(|(_, card)| card.spans.iter().any(|s| s.request_id == request_id))
            .map(|(message_id, _)| message_id.clone())
            .collect()
    }

    /// Drop a block's registry entry: it resolved, so nothing renders its
    /// controls anymore. Idempotent; the cached card stays until its last live
    /// block leaves (through `record` or an edit).
    pub fn forget(&mut self, request_id: &str) {
        self.blocks.remove(request_id);
    }

    /// Resolve a block on the cached card `message_id`: its elements are
    /// replaced in place by the one-line Interaction Receipt, and the cache is
    /// released when that was the card's last live block. Does NOT touch the
    /// registry — the resolver that owns the request drops the entry once every
    /// surface is settled. `None` when that card does not render the block.
    pub fn resolve_on(
        &mut self,
        message_id: &str,
        request_id: &str,
        line: &str,
    ) -> Option<serde_json::Value> {
        self.edit_on(message_id, request_id, vec![receipt_element(line)], false)
    }

    /// Replace a live block's elements on the cached card `message_id` with
    /// freshly built ones (the question block's updated 已选/✅ state). `None`
    /// when that card does not render the block.
    pub fn refresh_on(
        &mut self,
        message_id: &str,
        request_id: &str,
        elements: Vec<serde_json::Value>,
    ) -> Option<serde_json::Value> {
        self.edit_on(message_id, request_id, elements, true)
    }

    /// Remove a live block's elements from the cached card `message_id` — the
    /// cross-turn re-host (ADR-0038, rule 1): the block moved to a newer card,
    /// so the old card is repainted without it and no receipt is left behind.
    /// Does NOT touch the registry (the flush that re-hosted owns the entry
    /// now). `None` when that card does not render the block.
    pub fn remove_on(&mut self, message_id: &str, request_id: &str) -> Option<serde_json::Value> {
        self.edit_on(message_id, request_id, Vec::new(), false)
    }

    /// Apply `elements` in place of `request_id`'s span on the cached card. A
    /// failed edit means the cached JSON is not the shape the renderer wrote:
    /// drop the unusable handle (the caller falls back). `keep_span` re-anchors
    /// the same block over the replacement (a state refresh); otherwise the
    /// block leaves the card, releasing the cache once it held the last live
    /// block.
    fn edit_on(
        &mut self,
        message_id: &str,
        request_id: &str,
        elements: Vec<serde_json::Value>,
        keep_span: bool,
    ) -> Option<serde_json::Value> {
        let mut card = self.cards.remove(message_id)?;
        let Some(idx) = card.spans.iter().position(|s| s.request_id == request_id) else {
            self.cards.insert(message_id.to_string(), card);
            return None;
        };
        let span = card.spans.remove(idx);
        let new_end = span.start + elements.len();
        if !replace_elements(&mut card, &span, elements) {
            return None;
        }
        if keep_span {
            card.spans.push(BlockSpan {
                request_id: request_id.to_string(),
                start: span.start,
                end: new_end,
            });
        }
        let edited = card.card.clone();
        if !card.spans.is_empty() {
            self.cards.insert(message_id.to_string(), card);
        }
        Some(edited)
    }

    /// Resolve every registered block of `kind` whose request left the pending
    /// list into its `⏱ 已由其他客户端处理` receipt, editing each hosting card
    /// from its cache. Two sources of truth own a still-live block and are left
    /// to them: an accumulator's own flush repaints the card its
    /// `card_message_id` names (passed in `flush_owned`, `request_id → card`),
    /// and a directory whose list call failed said nothing, so its request may
    /// still be pending (#130, #144). A block cola itself is answering
    /// (`answered`) is also left to its settlement — cola's disappearance from
    /// the pending list is not "another client handled it". A block the
    /// accumulator owns but whose handle points at a DIFFERENT, older card is
    /// not skipped: that stale card shows the block too and must be repainted
    /// (ADR-0038, rule 2). Returns `(message_id, card)` per affected card for
    /// the caller to patch.
    pub fn drop_vanished(
        &mut self,
        kind: ClaimKind,
        pending: &HashSet<String>,
        failed_dirs: &HashSet<String>,
        cola_claimed: &HashSet<String>,
        flush_owned: &HashMap<String, String>,
        line: impl Fn(&str) -> String,
    ) -> Vec<(String, serde_json::Value)> {
        let vanished: Vec<(String, String)> = self
            .blocks
            .iter()
            .filter(|(id, h)| {
                h.kind == kind
                    && !pending.contains(*id)
                    && !cola_claimed.contains(*id)
                    && !failed_dirs.contains(&h.directory)
                    && flush_owned.get(*id) != Some(&h.message_id)
            })
            .map(|(id, h)| (id.clone(), h.target.clone()))
            .collect();
        let mut patches: Vec<(String, serde_json::Value)> = Vec::new();
        for (id, target) in vanished {
            let Some(handle) = self.blocks.remove(&id) else {
                continue;
            };
            let message_id = handle.message_id;
            tracing::info!(
                "block {} resolved on its card handle (session {}, card {})",
                id,
                handle.session_id,
                message_id
            );
            let text = line(&target);
            if let Some(card) = self.resolve_on(&message_id, &id, &text) {
                merge_patch(&mut patches, message_id, card);
            }
        }
        patches
    }

    /// Number of registered live blocks (test assertions on registry drain).
    #[cfg(test)]
    pub fn live_count(&self) -> usize {
        self.blocks.len()
    }

    /// Number of cached cards (test assertions on cache release).
    #[cfg(test)]
    pub fn cached_count(&self) -> usize {
        self.cards.len()
    }
}

/// Coalesce `(message_id, card)` patches: one entry per card, the latest edit
/// wins (several blocks can resolve on one card in a single sweep or click).
pub fn merge_patch(
    patches: &mut Vec<(String, serde_json::Value)>,
    message_id: String,
    card: serde_json::Value,
) {
    match patches.iter_mut().find(|(mid, _)| mid == &message_id) {
        Some(slot) => slot.1 = card,
        None => patches.push((message_id, card)),
    }
}

/// The markdown element one Interaction Receipt renders as.
fn receipt_element(line: &str) -> serde_json::Value {
    serde_json::json!({ "tag": "markdown", "content": line })
}

/// Replace the elements at `span` on `card` with `replacement`, shifting every
/// block that sat after them so its span keeps naming the same elements.
/// Returns false when the span is out of bounds — the cached JSON was somehow
/// not the shape the renderer wrote.
fn replace_elements(card: &mut CachedCard, span: &BlockSpan, replacement: Vec<serde_json::Value>) -> bool {
    let Some(elements) = card.card["body"]["elements"].as_array_mut() else {
        return false;
    };
    if span.start > span.end || span.end > elements.len() {
        return false;
    }
    let removed = span.end - span.start;
    let inserted = replacement.len();
    elements.splice(span.start..span.end, replacement);
    // Every surviving span starts at or after this one's end (spans never
    // overlap), so the shift can't go below zero.
    for s in card.spans.iter_mut() {
        if s.start >= span.end {
            s.start = s.start + inserted - removed;
            s.end = s.end + inserted - removed;
        }
    }
    card.spans.sort_by_key(|s| s.start);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::snapshot_claims::ClaimKind;

    fn block(request_id: &str, start: usize, end: usize, kind: ClaimKind) -> RenderedBlock {
        RenderedBlock {
            request_id: request_id.into(),
            start,
            end,
            kind,
            session_id: "ses_1".into(),
            directory: "/work".into(),
            target: format!("target-{request_id}"),
        }
    }

    fn text(content: &str) -> serde_json::Value {
        serde_json::json!({ "tag": "markdown", "content": content })
    }

    fn card(elements: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({ "schema": "2.0", "body": { "elements": elements } })
    }

    fn contents(card: &serde_json::Value) -> Vec<String> {
        card["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["content"].as_str().unwrap_or("").to_string())
            .collect()
    }

    /// Every cached copy of a block is discoverable, not just the one the
    /// registry currently names — a re-host can leave an older copy behind.
    #[test]
    fn cards_rendering_names_every_cached_copy() {
        let mut handles = CardHandles::default();
        let c = card(vec![text("body"), text("button")]);
        handles.record("om_old", &c, vec![block("p1", 0, 2, ClaimKind::Permission)]);
        handles.record("om_new", &c, vec![block("p1", 0, 2, ClaimKind::Permission)]);

        let mut found = handles.cards_rendering("p1");
        found.sort();
        assert_eq!(found, vec!["om_new", "om_old"]);
        assert!(handles.cards_rendering("p_other").is_empty());
    }

    /// Resolving one block replaces exactly its elements and shifts the blocks
    /// after it, so their spans keep naming the same elements.
    #[test]
    fn resolve_replaces_the_block_and_shifts_the_next_span() {
        let mut handles = CardHandles::default();
        let c = card(vec![
            text("head"),
            text("p1 body"),
            text("p1 button"),
            text("p2 body"),
            text("p2 button"),
            text("footer"),
        ]);
        handles.record(
            "om_1",
            &c,
            vec![
                block("p1", 1, 3, ClaimKind::Permission),
                block("p2", 3, 5, ClaimKind::Permission),
            ],
        );

        let edited = handles.resolve_on("om_1", "p1", "receipt-p1").unwrap();
        assert_eq!(
            contents(&edited),
            vec!["head", "receipt-p1", "p2 body", "p2 button", "footer"]
        );
        assert_eq!(handles.cached_count(), 1, "p2 still references the card");

        // p2's span shifted left by one; resolving it edits the right place.
        let edited = handles.resolve_on("om_1", "p2", "receipt-p2").unwrap();
        assert_eq!(
            contents(&edited),
            vec!["head", "receipt-p1", "receipt-p2", "footer"]
        );
        assert_eq!(handles.cached_count(), 0, "released with its last live block");

        // `resolve_on` edits the card only; the resolver owning the requests
        // drops the registry entries (the one mutation seam).
        handles.forget("p1");
        handles.forget("p2");
        assert_eq!(handles.live_count(), 0);
    }

    /// A state refresh replaces the block's elements with a longer set; the
    /// tracked span follows it, so a later resolve still hits the new elements.
    #[test]
    fn refresh_swaps_the_span_and_shifts_the_rest() {
        let mut handles = CardHandles::default();
        let c = card(vec![text("q1"), text("q2"), text("p1")]);
        handles.record(
            "om_1",
            &c,
            vec![
                block("q", 0, 2, ClaimKind::Question),
                block("p", 2, 3, ClaimKind::Permission),
            ],
        );

        let edited = handles
            .refresh_on("om_1", "q", vec![text("new-q1"), text("new-q2"), text("new-q3")])
            .unwrap();
        assert_eq!(contents(&edited), vec!["new-q1", "new-q2", "new-q3", "p1"]);

        let edited = handles.resolve_on("om_1", "q", "receipt-q").unwrap();
        assert_eq!(contents(&edited), vec!["receipt-q", "p1"]);
        assert_eq!(handles.message_of("p"), Some("om_1"));
    }

    /// A card that no longer renders any live block releases its cache; the
    /// registry entry survives until the block is resolved (or re-registered
    /// by the continuation that took it over).
    #[test]
    fn record_without_live_blocks_releases_the_cache() {
        let mut handles = CardHandles::default();
        handles.record(
            "om_1",
            &card(vec![text("x")]),
            vec![block("p", 0, 1, ClaimKind::Permission)],
        );
        assert_eq!(handles.cached_count(), 1);

        handles.record("om_1", &card(vec![text("y")]), vec![]);
        assert_eq!(handles.cached_count(), 0);
        assert_eq!(handles.message_of("p"), Some("om_1"));
    }

    /// The sweep's registry pass resolves only blocks that are gone AND
    /// outside a failed directory AND not already owned by the flush that will
    /// repaint them AND not claimed by cola's own settlement, and only its own
    /// kind.
    #[test]
    fn drop_vanished_skips_owned_pending_failed_and_claimed_blocks() {
        let mut handles = CardHandles::default();
        let c = card(vec![
            text("a"),
            text("b"),
            text("c"),
            text("d"),
            text("e"),
            text("f"),
        ]);
        let mut failed = block("p_failed", 1, 2, ClaimKind::Permission);
        failed.directory = "/failed".into();
        let mut stale = block("p_stale", 2, 3, ClaimKind::Permission);
        stale.directory = "/stale".into();
        handles.record(
            "om_1",
            &c,
            vec![
                block("p_gone", 0, 1, ClaimKind::Permission),
                failed,
                stale,
                block("q_other", 3, 4, ClaimKind::Question),
                block("p_pending", 4, 5, ClaimKind::Permission),
                block("p_claimed", 5, 6, ClaimKind::Permission),
            ],
        );
        // The accumulator owns p_stale, but its flush repaints `om_other`, not
        // the `om_1` this handle names — the stale card must be repainted too.
        handles.record(
            "om_other",
            &card(vec![text("new")]),
            vec![block("p_owned", 0, 1, ClaimKind::Permission)],
        );

        let pending: HashSet<String> = ["p_pending".to_string()].into();
        let failed_dirs: HashSet<String> = ["/failed".to_string()].into();
        let cola_claimed: HashSet<String> = ["p_claimed".to_string()].into();
        let flush_owned: HashMap<String, String> = [
            ("p_owned".to_string(), "om_other".to_string()),
            ("p_stale".to_string(), "om_other".to_string()),
        ]
        .into();
        let dropped = handles.drop_vanished(
            ClaimKind::Permission,
            &pending,
            &failed_dirs,
            &cola_claimed,
            &flush_owned,
            |t| format!("⏱ {t}"),
        );

        assert_eq!(dropped.len(), 1, "only the stale card is repainted once");
        assert_eq!(dropped[0].0, "om_1");
        assert_eq!(
            contents(&dropped[0].1),
            vec!["⏱ target-p_gone", "b", "⏱ target-p_stale", "d", "e", "f"],
            "the gone block AND the owned-but-stale block resolved on this card"
        );
        assert_eq!(handles.message_of("p_gone"), None);
        assert_eq!(handles.message_of("p_stale"), None);
        assert_eq!(
            handles.message_of("p_owned"),
            Some("om_other"),
            "the flush that owns it repaints it, so the sweep must skip it"
        );
        assert!(handles.message_of("p_failed").is_some());
        assert!(handles.message_of("q_other").is_some());
        assert!(handles.message_of("p_pending").is_some());
        assert!(
            handles.message_of("p_claimed").is_some(),
            "cola's own in-flight settlement owns this block, not the sweep"
        );
    }

    #[test]
    fn resolve_on_a_card_that_does_not_render_the_block_is_none() {
        let mut handles = CardHandles::default();
        handles.record(
            "om_1",
            &card(vec![text("a")]),
            vec![block("p", 0, 1, ClaimKind::Permission)],
        );
        assert!(handles.resolve_on("om_1", "other", "receipt").is_none());
        assert!(handles.resolve_on("om_missing", "p", "receipt").is_none());
        assert_eq!(handles.cached_count(), 1, "a miss keeps the cache intact");
        assert_eq!(handles.live_count(), 1);
    }
}
