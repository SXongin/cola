use crate::backend::{MessageRole, TailEntry, TailMessage};
use crate::bridge::snapshot::{ElsewherePending, PendingOverflow, SnapshotData};
use crate::feishu::card::session::{BackToList, back_to_list_button};
use crate::opencode;
use serde_json::json;

/// The update promise both live-adoption chips carry (ADR-0028): the busy chip
/// and the 等待后台任务 chip (#586) each say the snapshot keeps updating — the
/// follow streams the in-flight turn into the card, or the wait is served in
/// place and resumed by its completion Wake.
pub(crate) const FOLLOW_HINT: &str = "有新进展会自动更新";

/// The busy run-state chip (ADR-0028). Tests assert this constant so a copy
/// tweak lives in one place.
pub(crate) const BUSY_CHIP: &str = "⚙️ 运行中";

/// The 等待后台任务 chip (#586, ADR-0059): the adopted Session's Execution has
/// ended while Background Tasks are still live, so the snapshot follows the
/// wait instead of reporting 空闲 — the card yields ⏳ with the ledger, and the
/// completion Wake resumes it in place.
pub(crate) const WAITING_BACKGROUND_CHIP: &str = "⏳ 等待后台任务";

/// The retry run-state chip.
pub(crate) const RETRY_CHIP: &str = "🔁 需要重试";

/// The idle run-state chip.
pub(crate) const IDLE_CHIP: &str = "✅ 空闲";

/// The 等待你的确认 chip, shown when the session has an adopt-time pending
/// request — the operator's action beats the server's run state.
pub(crate) const WAITING_CHIP: &str = "⏳ 等待你的确认";

/// The 等待你的确认 chip when the session's pendings are NOT embedded because
/// they are already hosted by another card, and Message Pin is on: the pointer
/// names the card and its pin (ADR-0028 update 2026-09-25).
pub(crate) const WAITING_ELSEWHERE_PINNED_CHIP: &str = "⏳ 等待你的确认（见置顶的原卡片）";

/// The pointer chip with Message Pin off: names the original card without
/// promising a pin, since the opt-in gates the pin itself.
pub(crate) const WAITING_ELSEWHERE_CHIP: &str = "⏳ 等待你的确认（见原卡片）";

/// The status chip content for a session's state (ADR-0028 precedence
/// 等待你的确认 > 等待后台任务 > 运行中 > 需要重试 > 空闲; the busy and waiting
/// chips each carry one hint phrase). A pending already surfaced elsewhere
/// keeps the operator action ahead of the state too, pointing at its hosting
/// card instead. `waiting` is the gather's ADR-0059 fact (server-idle with
/// live Background Tasks, #586) — it names the wait even though the server
/// reads idle. `None` when there is no status to report — the chip is omitted
/// rather than guessed.
fn status_chip(
    has_pending: bool,
    pending_elsewhere: Option<ElsewherePending>,
    waiting: bool,
    status: Option<opencode::types::SessionStatus>,
) -> Option<String> {
    if has_pending {
        return Some(WAITING_CHIP.to_string());
    }
    if let Some(elsewhere) = pending_elsewhere {
        return Some(
            match elsewhere {
                ElsewherePending::Pinned => WAITING_ELSEWHERE_PINNED_CHIP,
                ElsewherePending::Unpinned => WAITING_ELSEWHERE_CHIP,
            }
            .to_string(),
        );
    }
    if waiting {
        return Some(format!("{WAITING_BACKGROUND_CHIP}\n{FOLLOW_HINT}"));
    }
    match status {
        Some(opencode::types::SessionStatus::Busy) => Some(format!("{BUSY_CHIP}\n{FOLLOW_HINT}")),
        Some(opencode::types::SessionStatus::Retry) => Some(RETRY_CHIP.to_string()),
        Some(opencode::types::SessionStatus::Idle) => Some(IDLE_CHIP.to_string()),
        None => None,
    }
}

/// Build the status chip as a body element, if the precedence says one shows.
fn status_chip_element(
    has_pending: bool,
    pending_elsewhere: Option<ElsewherePending>,
    waiting: bool,
    status: Option<opencode::types::SessionStatus>,
) -> Option<serde_json::Value> {
    status_chip(has_pending, pending_elsewhere, waiting, status)
        .map(|content| json!({ "tag": "markdown", "content": content }))
}
/// The live answer state of one claimed question block on the snapshot
/// (ADR-0028): request_id → state. Rebuilds after a button interaction pass
/// the flow's current state so the block mirrors the standalone question
/// card's 已选/✅ markers; the initial adopt-time snapshot passes nothing (all
/// questions open).
#[derive(Default, Clone)]
pub struct QuestionBlockState {
    /// The displayed answers per question slot (locked answers + live
    /// multi-select toggles).
    pub display: Vec<Option<Vec<String>>>,
    /// Whether each question slot is finalized (answered).
    pub done: Vec<bool>,
}

pub type SnapshotQuestionState = std::collections::HashMap<String, QuestionBlockState>;

/// The 最近对话 panel: the last-four tail entries, each role-marked and shown
/// folded — its header previews the entry briefly and expanding reveals the
/// full verbatim text (chunked to cola's per-element budget) together with one
/// `📎 name · mime · size` record line per File Content the message carried
/// (ADR-0076). A **Compaction** marker (ADR-0082, ticket #723) is shown as the
/// same folded row the live card's timeline renders, from the same builder, so
/// an adopted session whose history looks oddly short has its explanation
/// sitting where it happened. No tail → no panel.
/// `md` is the card's markdown state: the entry texts share its one table budget
/// and fenced fallback. A tail entry's attached skills are NOT rendered here
/// (spec #652, ticket #655, acceptance reversal): the snapshot tail is text only.
/// `omitted` counts the OLDEST entries the card's one-shot budget could not
/// carry (issue #726) and renders as a visible line above them.
fn tail_panels(
    tail: &[TailEntry],
    omitted: usize,
    md: &mut crate::feishu::card::sanitize::CardMarkdown,
) -> Vec<serde_json::Value> {
    let mut panels = Vec::new();
    if tail.is_empty() && omitted == 0 {
        return panels;
    }
    panels.push(json!({ "tag": "markdown", "content": "**最近对话**" }));
    // The budget's degradation is deliberate and visible (#726): an entry the
    // card could not carry is COUNTED here rather than silently disappearing.
    // The line is unfurled text, outside every entry's fold, so a reader who
    // scrolls the snapshot knows the preview is a window, not the whole tail.
    if omitted > 0 {
        panels.push(json!({
            "tag": "markdown",
            "content": format!("…更早的 {omitted} 条记录已省略（超出单卡容量）")
        }));
    }
    for (i, entry) in tail.iter().enumerate() {
        // The snapshot is sent once, but SnapshotClaims rebuilds it in place on
        // a claim ack and on remote resolution — so its panels take stable ids
        // too (the tail's order is fixed, so the entry index is stable). The
        // index is the entry's position in the FULL tail, so an evicted prefix
        // cannot renumber the panels that remain.
        let element_id = format!("snap_{}", omitted + i);
        let message = match entry {
            // One Compaction marker: the live card's own row builder, so the two
            // surfaces can never disagree on the copy or the fold.
            TailEntry::Compaction(marker) => {
                panels.extend(crate::feishu::card::compaction::compaction_elements(
                    &marker.compaction,
                    Some(&element_id),
                    md,
                ));
                continue;
            }
            TailEntry::Message(message) => message,
        };
        let (role, preview) = tail_preview(message);
        let title = if preview.is_empty() {
            format!("{role} （空消息）")
        } else {
            format!("{role} {preview}…")
        };
        let body = message.body_text();
        let chunks = crate::feishu::card::chunk_text(&body, crate::feishu::card::MAX_ELEMENT_TEXT_CHARS);
        let chunks = if chunks.is_empty() {
            vec!["（空消息）".to_string()]
        } else {
            chunks
        };
        let chunks: Vec<String> = chunks.iter().map(|c| md.clean(c)).collect();
        panels.push(crate::feishu::card::shell::collapsible_panel_chunks(
            &title,
            &chunks,
            Some(&element_id),
        ));
    }
    panels
}

/// The emoji marker for a tail entry's role. Shared by the static snapshot's
/// collapsible panels and the busy-adopt follow's static text (ADR-0028), so
/// the two presentations cannot disagree.
pub(crate) fn role_marker(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::User => "👤",
        MessageRole::Assistant => "🤖",
        _ => "💬",
    }
}

/// One tail message's role marker + short preview, shared by the static
/// snapshot's collapsible panels and the busy-adopt follow's static text
/// (ADR-0028). A files-only entry previews its record line so the folded panel
/// names the file (ADR-0076); a text-bearing entry previews its text, unchanged.
pub(crate) fn tail_preview(message: &TailMessage) -> (&'static str, String) {
    let source = if message.text.trim().is_empty() && !message.files.is_empty() {
        message.body_text()
    } else {
        message.text.clone()
    };
    let preview: String = source.chars().take(40).collect::<String>().trim().to_string();
    (role_marker(&message.role), preview)
}

/// The snapshot's display title: the cleaned session label, falling back to
/// the id tail when the label is empty (shared by the full snapshot, the
/// compact suppressed-切换 state card, and the busy-adopt follow's static
/// prefix).
pub(crate) fn display_title(title: &str, session_id: &str) -> String {
    let label = crate::feishu::card::clean_session_label(title);
    if label.is_empty() {
        crate::bridge::display::id_tail(session_id)
    } else {
        label
    }
}

/// Build the one-shot Session Snapshot card (ADR-0028) for an adopted session.
/// `verb` is the takeover verb (接管/切换 — 已 is prefixed for the header) and
/// `title` the session's display title; the body comes entirely from the
/// read-side [`SnapshotData`]. Sections are assembled from individually
/// constructible builders so a later ticket can drop a resolved claim
/// (05) or stream a busy follow (06) into the same card. `back` is the
/// `/switch`-list state the adoption came from, when it came from the list
/// card: it renders the optional 「返回列表」 button (ADR-0052); every other
/// snapshot source passes `None`.
#[cfg(test)]
pub fn build_snapshot_card(
    verb: &str,
    title: &str,
    data: &SnapshotData,
    back: Option<&BackToList>,
) -> serde_json::Value {
    build_snapshot_card_with_state(verb, title, data, &SnapshotQuestionState::new(), &[], back)
}

/// [`build_snapshot_card`] with live question state: after an interaction on a
/// claimed question block, the rebuild passes the flow's current answers/done
/// flags so the embedded block mirrors the standalone question card.
/// `receipts` are Interaction Receipt lines for claimed blocks resolved by
/// another client (#175, ADR-0038 rule 4): one markdown line each, rendered
/// under the live blocks so a resolved block leaves a visible residue.
///
/// This is the RENDER path: `data.pending` is the split the send path decided
/// (already trimmed), so every pending it still carries is rendered and no
/// claimed block can be traded away for space (#842).
pub fn build_snapshot_card_with_state(
    verb: &str,
    title: &str,
    data: &SnapshotData,
    question_state: &SnapshotQuestionState,
    receipts: &[String],
    back: Option<&BackToList>,
) -> serde_json::Value {
    let verb = verb.strip_prefix("已").unwrap_or(verb);
    let title = display_title(title, &data.session_id);
    let pending: Vec<_> = data
        .pending
        .iter()
        .filter(|req| req.session_id() == data.session_id)
        .collect();
    let build = SnapshotBuild {
        verb,
        title: &title,
        data,
        question_state,
        back,
    };
    render_fitted(&build, &pending, data.pending_overflow, receipts)
}

/// A built snapshot together with the decision its caller must persist.
pub struct BuiltSnapshot {
    pub card: serde_json::Value,
    /// How many of `data.pending` the card EMBEDS, in order. The caller keeps
    /// exactly this prefix (and claims exactly it); a rebuild renders that
    /// stored set and never re-decides.
    pub embedded: usize,
    /// How many pendings the card left out (#841/#842). They stay UNCLAIMED, so
    /// the normal pending flow surfaces them (inline on the session's live card
    /// when one exists, otherwise its own card); the card names them instead of
    /// claiming what it cannot show.
    pub overflow: usize,
}

/// The most pending blocks one snapshot card embeds, independent of the byte
/// budget (#842). A card packed with dozens of permission blocks leaves no room
/// for the 最近对话 the adopting reader came for, and is unusable to scroll, so
/// the block share has its own ceiling — the budget can lower this further, it
/// never raises it.
pub const MAX_SNAPSHOT_PENDING_BLOCKS: usize = 5;

/// [`build_snapshot_card_with_state`] plus the embedded/overflow decision. The
/// adoption path uses this one: it persists the returned split and claims
/// exactly what the card rendered.
///
/// The decision is made HERE, ONCE: the longest prefix of the pendings whose
/// blocks fit one card with every evictable section gone, capped at
/// [`MAX_SNAPSHOT_PENDING_BLOCKS`]. A claim whose block the card does not render
/// is a request the user cannot answer there, so the card either renders a
/// pending or leaves it out entirely — and a rebuild only re-renders the stored
/// split, so claims and rendered blocks cannot drift (#841, #842).
pub fn build_snapshot_card_fitted(
    verb: &str,
    title: &str,
    data: &SnapshotData,
    question_state: &SnapshotQuestionState,
    receipts: &[String],
    back: Option<&BackToList>,
) -> BuiltSnapshot {
    let verb = verb.strip_prefix("已").unwrap_or(verb);
    let title = display_title(title, &data.session_id);

    // Only the adopted session's OWN pendings belong on the snapshot — never a
    // sibling session's block (defensive; the gather already filtered). The
    // same filtered set drives both the 等待你的确认 chip and the rendered
    // blocks, so a chip never claims a pending that isn't shown.
    let pending: Vec<_> = data
        .pending
        .iter()
        .filter(|req| req.session_id() == data.session_id)
        .collect();
    let build = SnapshotBuild {
        verb,
        title: &title,
        data,
        question_state,
        back,
    };

    // The floor of a candidate split: chip + the K blocks + the overflow notice
    // + the back button, with receipts and the tail gone. If this fits, the
    // ladder below can always fit the final card by trading those away.
    let floor_pass = SnapshotPass {
        receipts: &[],
        omitted_receipts: receipts.len(),
        tail: &[],
        omitted_tail: data.tail.len(),
    };
    let ceiling = pending.len().min(MAX_SNAPSHOT_PENDING_BLOCKS);
    let embedded = largest_fitting(ceiling, |k| {
        build.card(
            &pending[..k],
            PendingOverflow::from_send(pending.len() - k),
            &floor_pass,
        )
    });
    let overflow = pending.len() - embedded;

    let card = render_fitted(
        &build,
        &pending[..embedded],
        PendingOverflow::from_send(overflow),
        receipts,
    );
    BuiltSnapshot {
        card,
        embedded,
        overflow,
    }
}

/// The render half shared by the send path and every rebuild: the embedded
/// pendings are FIXED, and only the evictable content trades away. One-shot
/// cards have no splitter, so when the card outgrows the budget (issue #726):
///
/// 1. the receipts leave first — residue for blocks another client already
///    resolved, not conversation. ADR-0038's 2026-10-20 update reconciles this
///    with rule 4: a receipt the snapshot does not render is absent from this
///    surface, never mis-targeted, and keeps its transcript home;
/// 2. then the 最近对话 tail, oldest entry first, so the newest conversation
///    survives.
///
/// The claimed blocks are NOT in this ladder any more (#842): their inclusion
/// was decided at send. A rebuild that dropped a still-claimed block would
/// leave a request with no surface to answer it from — the invariant the whole
/// split exists to protect.
fn render_fitted(
    build: &SnapshotBuild<'_>,
    pendings: &[&crate::bridge::request::kind::PendingRequest],
    overflow: PendingOverflow,
    receipts: &[String],
) -> serde_json::Value {
    let full = SnapshotPass {
        receipts,
        omitted_receipts: 0,
        tail: &build.data.tail,
        omitted_tail: 0,
    };
    let card = build.card(pendings, overflow, &full);
    if fits_card(&card) {
        return card;
    }

    // Drop the receipts before the conversation. Binary-searched so a long
    // receipt list costs O(log n) builds rather than one per line — a
    // monotonically shrinking predicate, since each omitted line removes bytes
    // and only adds a digit to the overflow line.
    let without_receipts = SnapshotPass {
        receipts: &[],
        omitted_receipts: receipts.len(),
        ..full
    };
    let no_receipts = build.card(pendings, overflow, &without_receipts);
    if fits_card(&no_receipts) {
        return smallest_omission(
            receipts.len(),
            |omitted| {
                build.card(
                    pendings,
                    overflow,
                    &SnapshotPass {
                        receipts: &receipts[omitted..],
                        omitted_receipts: omitted,
                        ..without_receipts
                    },
                )
            },
            no_receipts,
        );
    }

    // Still over: the tail goes too, oldest entry first.
    let floor_pass = SnapshotPass {
        tail: &[],
        omitted_tail: build.data.tail.len(),
        ..without_receipts
    };
    let floor = build.card(pendings, overflow, &floor_pass);
    if fits_card(&floor) {
        return smallest_omission(
            build.data.tail.len(),
            |omitted| {
                build.card(
                    pendings,
                    overflow,
                    &SnapshotPass {
                        tail: &build.data.tail[omitted..],
                        omitted_tail: omitted,
                        ..floor_pass
                    },
                )
            },
            floor,
        );
    }

    // Even the floor (status chip, overflow notice, back button) is over
    // budget: nothing else is evictable without making a pending unanswerable.
    // The title is capped and the chip is fixed copy, so this is a pathological
    // input rather than a shape a real adoption produces.
    tracing::warn!(
        "snapshot card is {} chars with every evictable section dropped (budget {}): the non-evictable shell exceeds one card",
        floor.to_string().len(),
        crate::feishu::card::MAX_CARD_JSON_CHARS
    );
    floor
}

/// The largest `k` in `0..=ceiling` whose candidate build fits, or 0 when even
/// the empty split does not. One more pending block only adds an `hr` and its
/// elements, so the predicate is monotone and a binary search suffices: `lo` is
/// known to fit (or nothing does), `hi` is the untested ceiling.
fn largest_fitting(ceiling: usize, mut build: impl FnMut(usize) -> serde_json::Value) -> usize {
    if ceiling == 0 || !fits_card(&build(0)) {
        return 0;
    }
    let (mut lo, mut hi) = (0, ceiling);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if fits_card(&build(mid)) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

/// Whether an assembled card fits the one-shot budgets Feishu accepts: the
/// serialized size AND the component ceiling. Both bind independently — a tail
/// of many title-only compaction rows is small in bytes but crosses
/// [`crate::feishu::card::MAX_CARD_COMPONENTS`] (issue #726's review).
fn fits_card(card: &serde_json::Value) -> bool {
    card.to_string().len() <= crate::feishu::card::MAX_CARD_JSON_CHARS
        && card_components(card) <= crate::feishu::card::MAX_CARD_COMPONENTS
}

/// The card's component count as Feishu charges it: every body element, and
/// every element nested inside a panel (a folded panel is a component AND its
/// markdown chunks are components).
fn card_components(card: &serde_json::Value) -> usize {
    card["body"]["elements"]
        .as_array()
        .map(|elements| {
            elements
                .iter()
                .map(|element| 1 + nested_components(element))
                .sum()
        })
        .unwrap_or(0)
}

/// The components one element carries beyond itself.
fn nested_components(element: &serde_json::Value) -> usize {
    element["elements"]
        .as_array()
        .map(|children| children.iter().map(|child| 1 + nested_components(child)).sum())
        .unwrap_or(0)
}

/// The smallest prefix omission in `0..=len` whose card fits, given that the
/// fully-evicted build (`len`) is already known to fit.
///
/// The predicate is monotone: one more omitted entry removes its bytes and adds
/// at most a character to the overflow line, so more omission never costs more.
fn smallest_omission(
    len: usize,
    mut build: impl FnMut(usize) -> serde_json::Value,
    known_fit: serde_json::Value,
) -> serde_json::Value {
    let (mut lo, mut hi) = (0, len);
    let mut best = None;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let card = build(mid);
        if fits_card(&card) {
            best = Some(card);
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    best.unwrap_or(known_fit)
}

/// The fixed inputs of every build pass: the card's header plus the data
/// sections that never vary across the budget ladder.
struct SnapshotBuild<'a> {
    verb: &'a str,
    title: &'a str,
    data: &'a SnapshotData,
    question_state: &'a SnapshotQuestionState,
    back: Option<&'a BackToList>,
}

impl SnapshotBuild<'_> {
    /// One build pass: the embedded pending blocks (already chosen), the
    /// overflow fact they leave behind, and the evictable content in `pass`.
    fn card(
        &self,
        pendings: &[&crate::bridge::request::kind::PendingRequest],
        pending_overflow: PendingOverflow,
        pass: &SnapshotPass,
    ) -> serde_json::Value {
        let data = self.data;
        // The card's markdown state (one table budget, one fenced fallback) for
        // the section that builds model-authored text — the 最近对话 tail's
        // entry texts. Hoisted here, not built inside `tail_panels`, so every
        // entry's chunks draw from the ONE card-wide table budget rather than a
        // fresh one per entry.
        let mut md = crate::feishu::card::sanitize::CardMarkdown::new();

        let mut elements: Vec<serde_json::Value> = Vec::new();
        if let Some(chip) = status_chip_element(
            !pendings.is_empty() || pending_overflow.any(),
            data.pending_elsewhere,
            data.waiting,
            data.status,
        ) {
            elements.push(chip);
        }
        for (i, req) in pendings.iter().enumerate() {
            if i > 0 {
                elements.push(json!({ "tag": "hr" }));
            }
            elements.extend(req.snapshot_block_elements(&data.directory, self.question_state));
        }
        // The overflow line names what the card leaves to the normal pending
        // flow. It deliberately does not promise a standalone card: an
        // unclaimed request is surfaced INLINE on the session's live card when
        // one exists, and only falls back to its own card otherwise (#842).
        match pending_overflow {
            PendingOverflow::None => {}
            PendingOverflow::Counted(n) => elements.push(json!({
                "tag": "markdown",
                "content": format!(
                    "…另有 {n} 个待处理请求超出单卡容量，将稍后显示"
                )
            })),
            // A rebuild cannot know how many of the unclaimed ones still
            // exist (they are outside the claim registry), so it stops naming
            // a number rather than showing a stale one.
            PendingOverflow::Uncounted => elements.push(json!({
                "tag": "markdown",
                "content": "…另有待处理请求超出单卡容量，将稍后显示"
            })),
        }
        if pass.omitted_receipts > 0 {
            elements.push(json!({
                "tag": "markdown",
                "content": format!(
                    "…更早的 {} 条处理回执已省略（超出单卡容量）",
                    pass.omitted_receipts
                )
            }));
        }
        for line in pass.receipts {
            elements.push(json!({ "tag": "markdown", "content": line }));
        }
        let has_receipts = pass.omitted_receipts > 0 || !pass.receipts.is_empty();
        let has_tail = pass.omitted_tail > 0 || !pass.tail.is_empty();
        if (!pendings.is_empty() || pending_overflow.any() || has_receipts) && has_tail {
            elements.push(json!({ "tag": "hr" }));
        }
        elements.extend(tail_panels(pass.tail, pass.omitted_tail, &mut md));

        // The `/switch`-list adoption's way back to the page it came from
        // (ADR-0052), at the card's end where the list footer used to be.
        if let Some(back) = self.back {
            elements.push(back_to_list_button(back));
        }

        crate::feishu::card::shell::card_shell(&format!("已{} {}", self.verb, self.title), "blue", elements)
    }
}

/// The evictable content of one build pass, and how many older items were
/// omitted (each count rendered as a visible line).
///
/// The embedded pending blocks are outside this struct on purpose: the send
/// path decided them once and a rebuild must render every one it still has
/// (#842), so they never join this ladder.
#[derive(Clone, Copy)]
struct SnapshotPass<'a> {
    receipts: &'a [String],
    omitted_receipts: usize,
    tail: &'a [TailEntry],
    omitted_tail: usize,
}

/// Build the compact suppressed-切换 state card (ADR-0028): when a re-switch
/// to a session already mapped to this thread has nothing to report (idle, no
/// pending, newest user message cola-authored), the switch card patches to
/// this small confirmation instead of a full snapshot — the card-form mirror
/// of the text form's one-line ack. No status chip, no tail, no pending
/// blocks: there is deliberately nothing to report. `back` is the
/// `/switch`-list state the adoption came from, when it came from the list
/// card: it renders the optional 「返回列表」 button (ADR-0052) with the same
/// behavior as the full snapshot's.
pub fn build_switched_state_card(
    title: &str,
    session_id: &str,
    directory: &str,
    back: Option<&BackToList>,
) -> serde_json::Value {
    let title = display_title(title, session_id);
    let mut elements = vec![json!({
        "tag": "markdown",
        "content": format!("已切换到该会话（目录 `{directory}`）。")
    })];
    if let Some(back) = back {
        elements.push(back_to_list_button(back));
    }
    crate::feishu::card::shell::card_shell(&format!("已切换 {title}"), "blue", elements)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::TailMarker;
    use crate::bridge::request::kind::PendingRequest;
    use crate::bridge::snapshot::SnapshotData;
    use crate::opencode::types::{PermissionRequest, QuestionInfo, QuestionOption, QuestionRequest};

    fn permission(req_id: &str, session_id: &str, action: &str) -> PermissionRequest {
        PermissionRequest {
            request_id: req_id.into(),
            session_id: Some(session_id.into()),
            permission: Some(action.into()),
            patterns: vec!["ls".into()],
            metadata: None,
            always: vec![],
        }
    }

    fn question(req_id: &str, session_id: &str) -> QuestionRequest {
        QuestionRequest {
            id: req_id.into(),
            session_id: session_id.into(),
            questions: vec![QuestionInfo {
                question: "继续?".into(),
                header: "确认".into(),
                options: vec![
                    QuestionOption {
                        label: "继续".into(),
                        description: String::new(),
                        ..Default::default()
                    },
                    QuestionOption {
                        label: "停下".into(),
                        description: String::new(),
                        ..Default::default()
                    },
                ],
                kind: crate::opencode::types::FormFieldKind::String,
                custom: None,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn data(
        status: Option<opencode::types::SessionStatus>,
        pending: Vec<PendingRequest>,
        tail: Vec<TailEntry>,
    ) -> SnapshotData {
        SnapshotData {
            session_id: "ses_adopted".into(),
            directory: "/work/proj".into(),
            status,
            waiting: false,
            pending,
            pending_elsewhere: None,
            pending_overflow: crate::bridge::snapshot::PendingOverflow::None,
            tail,
            newest_user_anchor: None,
            newest_user_is_cola_authored: false,
        }
    }

    fn tail(role: MessageRole, created_ms: i64, text: &str) -> TailEntry {
        TailEntry::Message(TailMessage {
            role,
            created_ms,
            text: text.into(),
            files: Vec::new(),
        })
    }

    /// A tail MESSAGE entry (the `tail` helper above is the same shape; this
    /// one keeps the entry type explicit where a vector mixes entries).
    fn tail_message(role: MessageRole, created_ms: i64, text: &str) -> TailEntry {
        tail(role, created_ms, text)
    }

    fn file(name: &str) -> crate::backend::FileContent {
        crate::backend::FileContent::decode("data:image/png;base64,QUJD", Some("image/png"), Some(name))
            .expect("an inline payload is a File Content")
    }

    /// The snapshot card a session's own transcript projects (ADR-0082, ticket
    /// #723): the card is built from the shared `transcript_tail`, exactly as an
    /// adoption builds it.
    fn snapshot_of(transcript: &crate::backend::SessionTranscript) -> serde_json::Value {
        let d = data(
            Some(opencode::types::SessionStatus::Idle),
            vec![],
            transcript.transcript_tail(),
        );
        build_snapshot_card("接管", "t", &d, None)
    }

    /// A **Compaction**'s neutral fact, as the transcript carries it.
    fn compaction(status: crate::backend::CompactionStatus, summary: &str) -> crate::backend::Compaction {
        crate::backend::Compaction {
            status,
            reason: crate::backend::CompactionReason::Manual,
            summary: summary.to_string(),
            usage: None,
            error: None,
            recent: "读者已经读过的最近对话".to_string(),
        }
    }

    /// One compaction message, the way V2 persists it — the carrier the bridge's
    /// own fixture builds a completed fact on (`test_support::compaction_message`),
    /// here with whatever fact a test needs.
    fn compaction_carrier(
        id: &str,
        created: i64,
        fact: crate::backend::Compaction,
    ) -> crate::backend::TranscriptMessage {
        crate::bridge::test_support::typed_message(
            id,
            MessageRole::Other("compaction".into()),
            Some(created),
            vec![crate::backend::Part::Compaction(fact)],
        )
    }

    fn elements(card: &serde_json::Value) -> &[serde_json::Value] {
        card["body"]["elements"].as_array().unwrap()
    }

    #[test]
    fn header_has_verb_and_title() {
        let d = data(Some(opencode::types::SessionStatus::Idle), vec![], vec![]);
        let card = build_snapshot_card("接管", "重写登录模块", &d, None);
        let h = card["header"]["title"]["content"].as_str().unwrap();
        assert_eq!(h, "已接管 重写登录模块", "header: {card}");
        assert_eq!(card["schema"].as_str().unwrap(), "2.0");
    }

    #[test]
    fn status_chip_precedence_matrix() {
        // The Waiting fact (idle + live Background Tasks, #586): the chip names
        // the wait instead of 空闲, with the follow hint.
        let waiting = status_chip(false, None, true, Some(opencode::types::SessionStatus::Idle))
            .expect("a Waiting turn renders its chip");
        assert!(waiting.contains(WAITING_BACKGROUND_CHIP), "{waiting}");
        assert!(waiting.contains(FOLLOW_HINT), "{waiting}");
        assert!(!waiting.contains(IDLE_CHIP), "{waiting}");

        // Pending beats every run state and the Waiting fact.
        assert_eq!(
            status_chip(true, None, false, Some(opencode::types::SessionStatus::Busy)).as_deref(),
            Some(WAITING_CHIP)
        );
        assert_eq!(
            status_chip(true, None, false, Some(opencode::types::SessionStatus::Retry)).as_deref(),
            Some(WAITING_CHIP)
        );
        assert_eq!(
            status_chip(true, None, false, Some(opencode::types::SessionStatus::Idle)).as_deref(),
            Some(WAITING_CHIP)
        );
        assert_eq!(
            status_chip(true, None, true, Some(opencode::types::SessionStatus::Idle)).as_deref(),
            Some(WAITING_CHIP)
        );
        assert_eq!(
            status_chip(true, None, false, None).as_deref(),
            Some(WAITING_CHIP)
        );

        // An embedded pending beats a pending surfaced elsewhere: the pointer
        // is only for pendings the snapshot does NOT show.
        assert_eq!(
            status_chip(
                true,
                Some(ElsewherePending::Unpinned),
                false,
                Some(opencode::types::SessionStatus::Busy)
            )
            .as_deref(),
            Some(WAITING_CHIP)
        );

        // A pending surfaced elsewhere beats every run state and the Waiting
        // fact too (ADR-0028 update 2026-09-25): the pointer copy depends on
        // Message Pin.
        assert_eq!(
            status_chip(
                false,
                Some(ElsewherePending::Pinned),
                true,
                Some(opencode::types::SessionStatus::Idle)
            )
            .as_deref(),
            Some(WAITING_ELSEWHERE_PINNED_CHIP)
        );
        assert_eq!(
            status_chip(
                false,
                Some(ElsewherePending::Pinned),
                false,
                Some(opencode::types::SessionStatus::Busy)
            )
            .as_deref(),
            Some(WAITING_ELSEWHERE_PINNED_CHIP)
        );
        assert_eq!(
            status_chip(
                false,
                Some(ElsewherePending::Unpinned),
                false,
                Some(opencode::types::SessionStatus::Idle)
            )
            .as_deref(),
            Some(WAITING_ELSEWHERE_CHIP)
        );

        // No pending and not waiting: the server state decides.
        assert!(
            status_chip(false, None, false, Some(opencode::types::SessionStatus::Busy))
                .unwrap()
                .contains(BUSY_CHIP)
        );
        assert_eq!(
            status_chip(false, None, false, Some(opencode::types::SessionStatus::Retry)).as_deref(),
            Some(RETRY_CHIP)
        );
        assert_eq!(
            status_chip(false, None, false, Some(opencode::types::SessionStatus::Idle)).as_deref(),
            Some(IDLE_CHIP)
        );
        // Unknown status and nothing pending → no chip, never guessed.
        assert_eq!(status_chip(false, None, false, None), None);
    }

    /// ADR-0028 update (2026-09-25): a pending already hosted by another card
    /// is not re-embedded, but the chip must say the session waits on the
    /// operator — never fall back to the server run state, never carry the
    /// busy hint.
    #[test]
    fn pending_elsewhere_points_at_the_host_card_without_the_busy_state() {
        let mut d = data(Some(opencode::types::SessionStatus::Busy), vec![], vec![]);
        d.pending_elsewhere = Some(ElsewherePending::Unpinned);
        let card = build_snapshot_card("接管", "t", &d, None);
        let body = elements(&card)[0]["content"].as_str().unwrap();
        assert_eq!(body, WAITING_ELSEWHERE_CHIP);
        let s = card.to_string();
        assert!(!s.contains(BUSY_CHIP), "no server run state: {s}");
        assert!(!s.contains(FOLLOW_HINT), "no follow hint: {s}");
    }

    /// The pointer promises 置顶 only when Message Pin is on (the same
    /// `[bridge] instant_reminder` opt-in gates the pin itself).
    #[test]
    fn pending_elsewhere_promises_the_pin_only_when_message_pin_is_on() {
        let mut d = data(None, vec![], vec![]);
        d.pending_elsewhere = Some(ElsewherePending::Pinned);
        let card = build_snapshot_card("接管", "t", &d, None);
        let body = elements(&card)[0]["content"].as_str().unwrap();
        assert_eq!(body, WAITING_ELSEWHERE_PINNED_CHIP);
        assert!(body.contains("置顶"), "{body}");
        assert!(!card.to_string().contains(BUSY_CHIP));
    }

    /// An embedded pending keeps the plain chip (no pointer), even when other
    /// pendings of the same session are surfaced elsewhere.
    #[test]
    fn embedded_pending_keeps_the_plain_waiting_chip() {
        let mut d = data(
            Some(opencode::types::SessionStatus::Busy),
            vec![PendingRequest::Permission(permission(
                "req_1",
                "ses_adopted",
                "bash",
            ))],
            vec![],
        );
        d.pending_elsewhere = Some(ElsewherePending::Pinned);
        let card = build_snapshot_card("接管", "t", &d, None);
        let body = elements(&card)[0]["content"].as_str().unwrap();
        assert_eq!(body, WAITING_CHIP);
        assert!(!card.to_string().contains("原卡片"), "{card}");
    }

    #[test]
    fn busy_chip_carries_one_hint() {
        let d = data(Some(opencode::types::SessionStatus::Busy), vec![], vec![]);
        let card = build_snapshot_card("接管", "t", &d, None);
        let body = elements(&card)[0]["content"].as_str().unwrap();
        assert!(body.contains(BUSY_CHIP));
        assert!(body.contains(FOLLOW_HINT));
    }

    /// #586: a Waiting gather (idle + live Background Tasks) shows the
    /// 等待后台任务 chip and the follow hint instead of 空闲 — the card is
    /// about to yield ⏳ with the ledger, and the chip says so up front.
    #[test]
    fn waiting_chip_names_the_background_wait() {
        let mut d = data(Some(opencode::types::SessionStatus::Idle), vec![], vec![]);
        d.waiting = true;
        let card = build_snapshot_card("接管", "t", &d, None);
        let body = elements(&card)[0]["content"].as_str().unwrap();
        assert!(body.contains(WAITING_BACKGROUND_CHIP), "{body}");
        assert!(body.contains(FOLLOW_HINT), "{body}");
        let s = card.to_string();
        assert!(!s.contains(IDLE_CHIP), "the plain idle chip must not render: {s}");
        assert!(!s.contains(BUSY_CHIP), "not a running turn: {s}");
    }

    #[test]
    fn unknown_status_renders_no_chip_line() {
        let d = data(None, vec![], vec![]);
        let card = build_snapshot_card("接管", "t", &d, None);
        let els = elements(&card);
        // Only the tail/none — no status markdown first.
        assert!(
            els.is_empty()
                || !els[0]["content"]
                    .as_str()
                    .is_some_and(|c| c.contains(IDLE_CHIP) || c.contains(BUSY_CHIP))
        );
    }

    #[test]
    fn pending_permission_block_reuses_buttons() {
        let d = data(
            Some(opencode::types::SessionStatus::Idle),
            vec![PendingRequest::Permission(permission(
                "req_1",
                "ses_adopted",
                "bash",
            ))],
            vec![],
        );
        let card = build_snapshot_card("接管", "t", &d, None);
        let els = elements(&card);
        let body: String = els
            .iter()
            .filter_map(|e| e["content"].as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(body.contains("🔐 **权限请求**"), "permission header: {card}");
        // The four interactive buttons ride along, not re-implemented.
        let buttons: Vec<&str> = els
            .iter()
            .filter(|e| e["tag"] == "button")
            .filter_map(|e| e["text"]["content"].as_str())
            .collect();
        assert_eq!(buttons.len(), 4);
        assert!(buttons.contains(&"✅ 允许一次"));
    }

    #[test]
    fn pending_question_block_reuses_question_elements() {
        let d = data(
            Some(opencode::types::SessionStatus::Idle),
            vec![PendingRequest::Question(question("q_1", "ses_adopted"))],
            vec![],
        );
        let card = build_snapshot_card("接管", "t", &d, None);
        let s = card.to_string();
        assert!(s.contains("确认"), "question text present: {s}");
        assert!(s.contains("继续"), "option present: {s}");
        assert!(s.contains("无法回答"), "reject control present: {s}");
    }

    #[test]
    fn no_pending_block_renders_for_a_different_session() {
        let d = data(
            Some(opencode::types::SessionStatus::Idle),
            vec![
                PendingRequest::Permission(permission("req_own", "ses_adopted", "bash")),
                PendingRequest::Permission(permission("req_other", "ses_sibling", "bash")),
                PendingRequest::Question(question("q_other", "ses_sibling")),
            ],
            vec![],
        );
        let card = build_snapshot_card("接管", "t", &d, None);
        let s = card.to_string();
        assert!(s.contains("req_own") || s.contains("允许一次"));
        assert!(!s.contains("q_other"), "sibling question leaked: {s}");
        // A sibling permission's body must not appear; only its request id would
        // be the tell — but permission_buttons embed the same body for both.
        assert!(!s.contains("ses_sibling"), "sibling session leaked: {s}");
    }

    #[test]
    fn empty_and_full_tail() {
        // Empty tail → no 最近对话 section at all.
        let empty = data(Some(opencode::types::SessionStatus::Idle), vec![], vec![]);
        let card = build_snapshot_card("接管", "t", &empty, None);
        assert!(!card.to_string().contains("最近对话"));

        // Full tail (4) → four role-marked folded panels + the header.
        let full = data(
            Some(opencode::types::SessionStatus::Idle),
            vec![],
            vec![
                tail(MessageRole::User, 1000, "问题一"),
                tail(MessageRole::Assistant, 2000, "回答一"),
                tail(MessageRole::User, 3000, "问题二"),
                tail(MessageRole::Assistant, 4000, "回答二"),
            ],
        );
        let card = build_snapshot_card("接管", "t", &full, None);
        let els = elements(&card);
        let panels: Vec<&serde_json::Value> =
            els.iter().filter(|e| e["tag"] == "collapsible_panel").collect();
        assert_eq!(panels.len(), 4, "one folded panel per entry: {card}");
        // Stable ids, so a claim rebuild keeps each panel's fold state.
        let ids: Vec<&str> = panels
            .iter()
            .map(|p| p["element_id"].as_str().expect("panel element_id"))
            .collect();
        assert_eq!(ids, ["snap_0", "snap_1", "snap_2", "snap_3"], "{card}");
        let s = card.to_string();
        assert!(s.contains("👤"), "user role marked: {s}");
        assert!(s.contains("🤖"), "assistant role marked: {s}");
        assert!(s.contains("回答二"), "full text present: {s}");
        assert!(panels.iter().all(|p| !p["expanded"].as_bool().unwrap()));
    }

    /// A tail entry that carries a File Content records it as a line
    /// (`📎 name · mime · size`, ADR-0076): the folded panel's body carries the
    /// full record line, and a files-only entry is not reported as an empty
    /// message. A text-bearing entry keeps its text preview (below).
    #[test]
    fn a_file_bearing_tail_entry_records_the_file_line() {
        let d = data(
            Some(opencode::types::SessionStatus::Idle),
            vec![],
            vec![
                TailEntry::Message(TailMessage {
                    role: MessageRole::User,
                    created_ms: 1_000,
                    text: String::new(),
                    files: vec![file("only.png")],
                }),
                TailEntry::Message(TailMessage {
                    role: MessageRole::User,
                    created_ms: 2_000,
                    text: "看看这个".into(),
                    files: vec![file("and.png")],
                }),
            ],
        );
        let card = build_snapshot_card("接管", "t", &d, None);
        let s = card.to_string();
        assert!(
            s.contains("📎 only.png · image/png · 3 B"),
            "the record line renders: {s}"
        );
        assert!(
            s.contains("📎 and.png · image/png · 3 B"),
            "each file records its own line: {s}"
        );
        assert!(
            !s.contains("（空消息）"),
            "a files-only entry is not an empty message: {s}"
        );
        // The text-bearing entry's preview stays its text (ADR-0076: semantics
        // of text-bearing entries unchanged).
        assert!(s.contains("👤 看看这个…"), "text preview unchanged: {s}");
    }

    /// A tail entry without files renders its text alone — the record-line
    /// change leaves a file-free snapshot byte-for-byte unchanged (ADR-0076).
    #[test]
    fn a_tail_entry_without_files_renders_its_text_alone() {
        assert_eq!(
            tail(MessageRole::User, 1_000, "问题一")
                .message()
                .expect("a message entry")
                .body_text(),
            "问题一"
        );
    }

    /// The tail's Compaction marker is the LIVE card's row (ADR-0082, ticket
    /// #723): one builder, so the copy, the fold and the chunking cannot
    /// disagree between the two surfaces — a failure is a plain line carrying
    /// the Backend's error, a completion is a folded panel whose body is the
    /// summary, chunked to cola's per-element budget.
    #[test]
    fn the_tail_marker_row_is_the_live_cards_row() {
        let failed = crate::backend::Compaction {
            error: Some("context length exceeded".into()),
            ..compaction(crate::backend::CompactionStatus::Failed, "")
        };
        let summary = "摘要正文。".repeat(1_200);
        let d = data(
            Some(opencode::types::SessionStatus::Idle),
            vec![],
            vec![
                tail(MessageRole::User, 1_000, "问题一"),
                TailEntry::Compaction(TailMarker {
                    created_ms: 2_000,
                    compaction: failed,
                }),
                TailEntry::Compaction(TailMarker {
                    created_ms: 3_000,
                    compaction: compaction(crate::backend::CompactionStatus::Completed, &summary),
                }),
            ],
        );
        let card = build_snapshot_card("接管", "t", &d, None);
        let s = card.to_string();
        assert!(
            s.contains("⚠️ 压缩失败：context length exceeded"),
            "the live row's failure copy: {s}"
        );
        let panel = elements(&card)
            .iter()
            .find(|element| {
                element
                    .pointer("/header/title/content")
                    .and_then(|value| value.as_str())
                    == Some("已压缩上下文（手动）")
            })
            .expect("the completion's folded panel");
        let chunks: Vec<&str> = panel["elements"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|chunk| chunk.get("content").and_then(|value| value.as_str()))
            .collect();
        assert!(chunks.len() > 1, "the summary is chunked: {panel}");
        assert_eq!(chunks.concat(), summary, "the chunks are the summary, whole");
        assert!(
            !s.contains("读者已经读过的最近对话"),
            "the protocol's recent tail is never rendered: {s}"
        );
    }

    /// A snapshot of a session with no compaction in the window is byte-for-byte
    /// unchanged (ADR-0082, ticket #723): a compaction that preceded the tail's
    /// oldest kept message contributes no entry, so the card is the very JSON the
    /// pre-#723 build produced — asserted here by building it from two
    /// transcripts that differ only by that older compaction.
    #[test]
    fn a_snapshot_without_a_compaction_in_the_window_is_unchanged() {
        let user_at = |i: i64| {
            crate::bridge::test_support::typed_message(
                &format!("u{i}"),
                MessageRole::User,
                Some(i * 1_000),
                vec![crate::bridge::test_support::text_part(&format!("m{i}"))],
            )
        };
        let messages: Vec<_> = (1..=5).map(user_at).collect();
        let plain = snapshot_of(&crate::backend::SessionTranscript::new(messages.clone()));

        let mut with_older_marker = messages;
        with_older_marker.insert(
            0,
            compaction_carrier(
                "c_old",
                500,
                compaction(
                    crate::backend::CompactionStatus::Completed,
                    "更早的对话已被压缩。",
                ),
            ),
        );
        let summarized = snapshot_of(&crate::backend::SessionTranscript::new(with_older_marker));

        assert_eq!(
            summarized, plain,
            "a compaction outside the window leaves the snapshot byte-for-byte unchanged"
        );
        assert!(
            !plain.to_string().contains("已压缩上下文"),
            "and the compaction-free snapshot carries no marker at all"
        );
    }

    /// A tail entry's text is rendered alone (spec #652, ticket #655, acceptance
    /// reversal): the Session Snapshot tail carries NO loaded-skill fold, even
    /// for a `/skill` message.
    #[test]
    fn a_tail_entry_renders_no_skill_fold() {
        let d = data(
            Some(opencode::types::SessionStatus::Idle),
            vec![],
            vec![tail(MessageRole::User, 1000, "/skill implement-spec 644")],
        );
        let card = build_snapshot_card("接管", "t", &d, None);
        let s = card.to_string();
        assert!(s.contains("/skill implement-spec 644"), "{s}");
        assert!(!s.contains("已加载技能"), "the tail renders no fold: {s}");
    }

    /// Worst-case input (4-message tail + 2 pending blocks) must stay within
    /// Feishu's card budgets — the streaming splitter is not available to a
    /// one-shot snapshot, so the builder itself must fit. The last tail message
    /// also exceeds `MAX_ELEMENT_TEXT_CHARS` so the per-entry collapsible has to
    /// split across several markdown chunks (the reviewer-noted untested branch).
    #[test]
    fn worst_case_fits_feishu_limits() {
        let mut long_tail: Vec<TailEntry> = (0..3)
            .map(|i| {
                let role = if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                };
                tail(
                    role,
                    i * 1000,
                    &format!("第{}条消息：{}", i, "很长的内容。".repeat(120)),
                )
            })
            .collect();
        // Push the last entry past cola's single-element budget so the panel
        // chunks its full text rather than truncating it — long enough to
        // cross the budget, small enough that the whole card still fits
        // Feishu's byte budget (the #726 budget only evicts entries when the
        // whole tail outgrows the card; it never clips a carried entry).
        long_tail.push(tail(
            MessageRole::Assistant,
            9000,
            // Just over the 3000-char element budget ("超长内容。" is 5 chars),
            // so the entry splits into two chunks while the whole card still
            // fits the one-shot byte budget.
            &format!(
                "第4条超长消息：{}",
                "超长内容。".repeat(crate::feishu::card::MAX_ELEMENT_TEXT_CHARS / 5 + 2)
            ),
        ));
        let d = data(
            Some(opencode::types::SessionStatus::Busy),
            vec![
                PendingRequest::Permission(permission("req_1", "ses_adopted", "bash")),
                PendingRequest::Question(question("q_1", "ses_adopted")),
            ],
            long_tail,
        );
        let card = build_snapshot_card("接管", "重写登录模块", &d, None);
        let s = card.to_string();
        // Chunked, not truncated: the whole long tail is present in the card.
        assert!(s.contains("超长内容。"), "full long text present: {}", s.len());
        assert!(
            s.len() <= crate::feishu::card::FEISHU_CARD_LIMIT_BYTES,
            "snapshot card over Feishu byte limit: {}",
            s.len()
        );
        assert!(
            s.len() <= crate::feishu::card::MAX_CARD_JSON_CHARS,
            "snapshot card over estimated JSON budget: {}",
            s.len()
        );
    }

    /// The tail's one-shot budget (issue #726): four long messages and a
    /// max-length compaction marker together are far past
    /// `MAX_CARD_JSON_CHARS`, and the card must still be one Feishu accepts.
    /// The degradation is deliberate: the OLDEST entries are dropped (the
    /// newest conversation is what the reader came for), the dropped count is
    /// named on the card, and every entry that IS carried is carried whole.
    #[test]
    fn an_oversized_tail_degrades_visibly_and_fits_the_card_budget() {
        // Each message is ~3,600 CJK chars (~10.8 KB of JSON) with sentinels
        // at both ends — a carried entry must show BOTH, so a partial entry can
        // never pass as whole. The marker's summary is the live row's own
        // 6,000-char cap (~18 KB).
        let long = |label: &str| format!("{label}开头：{}：{label}结尾", "很长的内容。".repeat(600));
        let marker_summary =
            "压缩摘要。".repeat(crate::feishu::card::compaction::MAX_COMPACTION_SUMMARY_CHARS / 5);
        let d = data(
            Some(opencode::types::SessionStatus::Idle),
            vec![],
            vec![
                tail(MessageRole::User, 1000, &long("第一条")),
                tail(MessageRole::Assistant, 2000, &long("第二条")),
                TailEntry::Compaction(TailMarker {
                    created_ms: 2500,
                    compaction: compaction(crate::backend::CompactionStatus::Completed, &marker_summary),
                }),
                tail(MessageRole::User, 3000, &long("第三条")),
                tail(MessageRole::Assistant, 4000, &long("第四条")),
            ],
        );
        let card = build_snapshot_card("接管", "t", &d, None);
        let s = card.to_string();

        assert!(
            s.len() <= crate::feishu::card::MAX_CARD_JSON_CHARS,
            "the one-shot snapshot must stay inside the card budget: {}",
            s.len()
        );
        // Three oldest entries (two messages and the marker) had to go.
        assert!(s.contains("…更早的 3 条记录已省略"), "the eviction is named: {s}");
        assert!(
            !s.contains("第一条") && !s.contains("第二条") && !s.contains("压缩摘要。"),
            "the OLDEST entries are the ones dropped: {s}"
        );
        // The two newest messages are carried WHOLE — the budget never clips a
        // surviving entry: both of each entry's sentinels are on the card.
        for label in ["第三条", "第四条"] {
            assert!(
                s.contains(&format!("{label}开头")),
                "{label} carries its start: {s}"
            );
            assert!(
                s.contains(&format!("{label}结尾")),
                "{label} carries its end: {s}"
            );
        }
    }

    /// The other end of the same budget: a single entry larger than the whole
    /// card cannot be carried at all, so it is dropped too — and named. The
    /// card still fits, which is the invariant that matters (#726: never a
    /// silently rejected card).
    #[test]
    fn a_tail_entry_larger_than_the_card_is_dropped_and_named() {
        let giant = "巨".repeat(crate::feishu::card::MAX_CARD_JSON_CHARS * 4);
        let d = data(
            Some(opencode::types::SessionStatus::Idle),
            vec![],
            vec![tail(MessageRole::User, 1000, &giant)],
        );
        let card = build_snapshot_card("接管", "t", &d, None);
        let s = card.to_string();
        assert!(
            s.len() <= crate::feishu::card::MAX_CARD_JSON_CHARS,
            "the card must fit: {}",
            s.len()
        );
        assert!(
            s.contains("…更早的 1 条记录已省略"),
            "the dropped entry is named: {s}"
        );
        assert!(!s.contains("巨巨巨"), "an entry that cannot fit is not carried");
    }

    /// Receipts are residue, not conversation: when the card is over budget the
    /// old ones go BEFORE any tail entry is touched, with their own visible
    /// line. The tail — the part the reader came for — then stays whole.
    #[test]
    fn oversized_receipts_are_evicted_before_the_tail() {
        let receipts: Vec<String> = (0..4)
            .map(|i| format!("回执{i}：{}", "已由其他客户端处理。".repeat(200)))
            .collect();
        let d = data(
            Some(opencode::types::SessionStatus::Idle),
            vec![],
            (0..2)
                .map(|i| tail(MessageRole::User, i * 1000, &format!("第{i}条短消息")))
                .collect(),
        );
        let card =
            build_snapshot_card_with_state("接管", "t", &d, &SnapshotQuestionState::new(), &receipts, None);
        let s = card.to_string();

        assert!(
            s.len() <= crate::feishu::card::MAX_CARD_JSON_CHARS,
            "the card must fit: {}",
            s.len()
        );
        assert!(
            s.contains("条处理回执已省略"),
            "the evicted receipts are named: {s}"
        );
        assert!(!s.contains("回执0"), "the OLDEST receipt is the one dropped: {s}");
        assert!(s.contains(&receipts[3]), "the newest receipt is kept whole");
        assert!(
            s.contains("第0条短消息") && s.contains("第1条短消息"),
            "the tail is untouched while receipts can pay for the space: {s}"
        );
    }

    /// A long pending set is embedded up to the block cap (#842): the card
    /// carries the first five blocks, names the rest, and keeps the
    /// conversation it has room for. The rest stay unclaimed for the normal
    /// pending flow.
    #[test]
    fn a_long_pending_set_embeds_the_cap_and_names_the_rest() {
        let pending: Vec<PendingRequest> = (0..60)
            .map(|i| PendingRequest::Permission(permission(&format!("req_{i}"), "ses_adopted", "bash")))
            .collect();
        let d = data(
            Some(opencode::types::SessionStatus::Busy),
            pending,
            vec![tail_message(MessageRole::User, 1000, "保留的对话")],
        );
        let built = build_snapshot_card_fitted("接管", "t", &d, &SnapshotQuestionState::new(), &[], None);
        let s = built.card.to_string();

        assert_eq!(built.embedded, MAX_SNAPSHOT_PENDING_BLOCKS, "the cap binds");
        assert_eq!(built.overflow, 60 - MAX_SNAPSHOT_PENDING_BLOCKS);
        assert!(
            s.len() <= crate::feishu::card::MAX_CARD_JSON_CHARS,
            "byte budget: {}",
            s.len()
        );
        assert!(
            card_components(&built.card) <= crate::feishu::card::MAX_CARD_COMPONENTS,
            "component budget: {}",
            card_components(&built.card)
        );
        assert_eq!(
            s.matches("✅ 允许一次").count(),
            MAX_SNAPSHOT_PENDING_BLOCKS,
            "exactly the capped prefix is embedded: {s}"
        );
        assert!(
            s.contains("55 个待处理请求超出单卡容量"),
            "the overflow is named: {s}"
        );
        assert!(
            s.contains("将稍后显示"),
            "the copy promises no particular card: {s}"
        );
        assert!(s.contains("保留的对话"), "the conversation is kept: {s}");
        assert!(s.contains("等待你的确认"), "the chip still reports the wait: {s}");
    }

    /// The card budget can bind below the cap: five oversized question blocks
    /// do not fit, so the snapshot embeds only the prefix that does — never
    /// zero just because the whole set is too big (#842).
    #[test]
    fn the_card_budget_can_embed_fewer_than_the_cap() {
        let pending: Vec<PendingRequest> = (0..5)
            .map(|i| {
                let mut q = question(&format!("req_{i}"), "ses_adopted");
                q.questions[0].question = "问".repeat(5000);
                PendingRequest::Question(q)
            })
            .collect();
        let d = data(
            Some(opencode::types::SessionStatus::Busy),
            pending,
            vec![tail_message(MessageRole::User, 1000, "对话")],
        );
        let built = build_snapshot_card_fitted("接管", "t", &d, &SnapshotQuestionState::new(), &[], None);
        let s = built.card.to_string();

        assert_eq!(built.embedded, 1, "one oversized block fits, the next does not");
        assert_eq!(built.overflow, 4);
        assert!(
            s.len() <= crate::feishu::card::MAX_CARD_JSON_CHARS,
            "byte budget: {}",
            s.len()
        );
        assert!(
            s.contains("4 个待处理请求超出单卡容量"),
            "the overflow is named: {s}"
        );
    }

    /// A claimed set that fits is embedded and claimed in full: the overflow
    /// stays zero on the common path.
    #[test]
    fn a_fitting_pending_set_is_embedded_whole() {
        let d = data(
            Some(opencode::types::SessionStatus::Busy),
            vec![PendingRequest::Permission(permission(
                "req_1",
                "ses_adopted",
                "bash",
            ))],
            vec![tail_message(MessageRole::User, 1000, "对话")],
        );
        let built = build_snapshot_card_fitted("接管", "t", &d, &SnapshotQuestionState::new(), &[], None);

        assert_eq!(built.embedded, 1);
        assert_eq!(built.overflow, 0);
        assert!(built.card.to_string().contains("✅ 允许一次"));
        assert!(!built.card.to_string().contains("超出单卡容量"));
    }

    /// A rebuild renders the stored split and names the leftover requests
    /// WITHOUT a number (#842): the unclaimed ones are outside the claim
    /// registry, so cola cannot know how many remain, and a stale count would
    /// be worse than none.
    #[test]
    fn a_rebuilt_card_names_the_overflow_without_a_count() {
        let mut d = data(
            Some(opencode::types::SessionStatus::Busy),
            vec![PendingRequest::Permission(permission(
                "req_1",
                "ses_adopted",
                "bash",
            ))],
            vec![tail_message(MessageRole::User, 1000, "对话")],
        );
        d.pending_overflow = PendingOverflow::Counted(3);
        let counted =
            build_snapshot_card_with_state("接管", "t", &d, &SnapshotQuestionState::new(), &[], None)
                .to_string();
        assert!(counted.contains("3 个待处理请求超出单卡容量"), "{counted}");

        d.pending_overflow = d.pending_overflow.uncounted();
        let rebuilt =
            build_snapshot_card_with_state("接管", "t", &d, &SnapshotQuestionState::new(), &[], None)
                .to_string();
        assert!(
            rebuilt.contains("另有待处理请求超出单卡容量"),
            "the rebuild still names the leftover set: {rebuilt}"
        );
        assert!(
            !rebuilt.contains("3 个待处理请求"),
            "no stale count on a rebuild: {rebuilt}"
        );
    }

    /// The component ceiling binds too (#726's review): 160 title-only
    /// compaction rows are small in bytes but would cross Feishu's component
    /// limit, so the ladder must keep evicting by COUNT. The newest entry — the
    /// message after all the markers — is the one that survives.
    #[test]
    fn a_tail_of_many_component_rows_is_evicted_by_component_count() {
        let mut tail: Vec<TailEntry> = (0..160)
            .map(|i| {
                TailEntry::Compaction(TailMarker {
                    created_ms: i,
                    compaction: compaction(crate::backend::CompactionStatus::Completed, ""),
                })
            })
            .collect();
        tail.push(tail_message(MessageRole::User, 9000, "最后一条消息"));
        let d = data(Some(opencode::types::SessionStatus::Idle), vec![], tail);
        let card = build_snapshot_card("接管", "t", &d, None);
        let s = card.to_string();

        assert!(
            s.len() <= crate::feishu::card::MAX_CARD_JSON_CHARS,
            "byte budget: {}",
            s.len()
        );
        assert!(
            card_components(&card) <= crate::feishu::card::MAX_CARD_COMPONENTS,
            "component budget: {} > {}",
            card_components(&card),
            crate::feishu::card::MAX_CARD_COMPONENTS
        );
        assert!(s.contains("条记录已省略"), "the eviction is named: {s}");
        assert!(s.contains("最后一条消息"), "the newest entry survives: {s}");
    }

    /// An evicted prefix must not renumber the panels that remain: the ids are
    /// positions in the FULL tail, so a claim-ack rebuild keeps the fold state
    /// of every entry still on the card.
    #[test]
    fn evicted_entries_do_not_renumber_the_kept_panels() {
        let huge = "巨".repeat(12_000);
        let d = data(
            Some(opencode::types::SessionStatus::Idle),
            vec![],
            vec![
                tail_message(MessageRole::User, 1000, &huge),
                tail_message(MessageRole::Assistant, 2000, &huge),
                tail_message(MessageRole::User, 3000, "最后一条"),
            ],
        );
        let card = build_snapshot_card("接管", "t", &d, None);
        let ids: Vec<&str> = elements(&card)
            .iter()
            .filter_map(|element| element["element_id"].as_str())
            .collect();

        assert_eq!(
            ids,
            vec!["snap_2"],
            "the surviving entry keeps its full-tail index"
        );
        assert!(card.to_string().contains("条记录已省略"), "{card}");
    }

    /// A card that fits is the FULL build verbatim: the budget only ever runs
    /// its eviction ladder when the unbudgeted card is over. This pins the
    /// no-degradation path, so a future change cannot quietly start dropping
    /// entries (or adding overflow lines) to cards that never needed it.
    #[test]
    fn a_fitting_card_is_the_full_build_verbatim() {
        let d = data(
            Some(opencode::types::SessionStatus::Idle),
            vec![],
            (0..4)
                .map(|i| {
                    let role = if i % 2 == 0 {
                        MessageRole::User
                    } else {
                        MessageRole::Assistant
                    };
                    tail(role, i * 1000, &format!("第{i}条短消息"))
                })
                .collect(),
        );
        let built = build_snapshot_card("接管", "t", &d, None);
        let unbudgeted = SnapshotBuild {
            verb: "接管",
            title: &display_title("t", &d.session_id),
            data: &d,
            question_state: &SnapshotQuestionState::new(),
            back: None,
        }
        .card(
            &[],
            PendingOverflow::None,
            &SnapshotPass {
                receipts: &[],
                omitted_receipts: 0,
                tail: &d.tail,
                omitted_tail: 0,
            },
        );

        assert_eq!(built, unbudgeted, "the budget must not touch a card that fits");
        let s = built.to_string();
        assert!(!s.contains("已省略"), "no overflow line on a fitting card: {s}");
        // chip + heading + four entries, and nothing else.
        assert_eq!(elements(&built).len(), 6, "elements: {s}");
    }

    /// A tail that fits is untouched by the budget: every entry whole, no
    /// overflow line. The full build is returned as-is, so the common snapshot
    /// keeps exactly the bytes it had before #726.
    #[test]
    fn a_tail_within_budget_is_not_degraded() {
        let d = data(
            Some(opencode::types::SessionStatus::Idle),
            vec![],
            (0..4)
                .map(|i| {
                    let role = if i % 2 == 0 {
                        MessageRole::User
                    } else {
                        MessageRole::Assistant
                    };
                    tail(role, i * 1000, &format!("第{i}条短消息"))
                })
                .collect(),
        );
        let card = build_snapshot_card("接管", "t", &d, None);
        let s = card.to_string();
        assert!(
            !s.contains("已省略"),
            "a fitting tail carries no overflow line: {s}"
        );
        for i in 0..4 {
            assert!(s.contains(&format!("第{i}条短消息")), "entry {i} is whole: {s}");
        }
    }

    /// ADR-0052: only an adoption from the `/switch` list carries the
    /// 「返回列表」 button on its full snapshot, and the button holds the
    /// list's keyword/scope/page.
    #[test]
    fn full_snapshot_back_button_renders_only_with_the_list_state() {
        let back = BackToList {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            keyword: "proj".into(),
            scope: crate::feishu::card::session::SwitchScope::All,
            page: 2,
        };
        let d = data(Some(opencode::types::SessionStatus::Idle), vec![], vec![]);

        let card = build_snapshot_card("接管", "t", &d, Some(&back));
        let button = elements(&card)
            .iter()
            .find(|e| e["tag"] == "button" && e["text"]["content"] == "返回列表")
            .expect("a list adoption renders the back button");
        assert_eq!(button["value"]["action"], "switch");
        assert_eq!(button["value"]["op"], "back");
        assert_eq!(button["value"]["keyword"], "proj");
        assert_eq!(button["value"]["scope"], "all");
        assert_eq!(button["value"]["page"], 2);
        assert_eq!(button["value"]["chat_id"], "chat_1");
        assert_eq!(button["value"]["thread_id"], "chat_1");

        // A snapshot from any other source (text /attach, /topic --adopt)
        // passes no list state and renders no button.
        let card = build_snapshot_card("接管", "t", &d, None);
        assert!(!card.to_string().contains("返回列表"), "{card}");
    }

    /// ADR-0052: the suppressed compact 已切换 state card carries the same
    /// back button when the adoption came from the list.
    #[test]
    fn switched_state_back_button_renders_only_with_the_list_state() {
        let back = BackToList {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            keyword: "proj".into(),
            scope: crate::feishu::card::session::SwitchScope::Directory,
            page: 3,
        };

        let card = build_switched_state_card("t", "ses_adopted", "/work/proj", Some(&back));
        let button = elements(&card)
            .iter()
            .find(|e| e["tag"] == "button" && e["text"]["content"] == "返回列表")
            .expect("the suppressed 已切换 card renders the back button");
        assert_eq!(button["value"]["op"], "back");
        assert_eq!(button["value"]["keyword"], "proj");
        assert_eq!(button["value"]["scope"], "dir");
        assert_eq!(button["value"]["page"], 3);

        let card = build_switched_state_card("t", "ses_adopted", "/work/proj", None);
        assert!(!card.to_string().contains("返回列表"), "{card}");
    }
}
