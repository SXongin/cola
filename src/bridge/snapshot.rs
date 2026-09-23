use std::sync::Arc;

use tracing::Instrument;

use crate::bridge::core::SharedCore;
use crate::opencode;

/// Read-side state for the Session Snapshot card (ADR-0028). Built purely from
/// server reads at the moment a Chat/Topic activates a session — never from a
/// prompt, a session write, or a guess. Each source is best-effort: a read
/// failure degrades that field (status → `None`, lists → empty) and is logged,
/// so an adoption can never be blocked by the snapshot gather. Clone: the
/// claim registry keeps a copy of the adopt-time state to re-render the card
/// when a claimed request resolves.
#[derive(Clone)]
pub struct SnapshotData {
    pub session_id: String,
    pub directory: String,
    /// The server's run state for the session at activation time. `None` when
    /// the status read failed or returned an unrecognised type — the chip is
    /// omitted rather than guessed (ADR-0028).
    pub status: Option<opencode::types::SessionStatus>,
    /// Pending permission/question requests whose `sessionID` is the adopted
    /// session (never another session's in the same directory, and never a
    /// sub-task child's — ADR-0028 keeps those on today's standalone flow).
    pub pending: Vec<crate::bridge::request::PendingRequest>,
    /// The 最近对话 tail: the last text-bearing user/assistant messages,
    /// newest last, verbatim `text` parts only.
    pub tail: Vec<TailEntry>,
    /// The created time of the newest user message (ANY user message, text or
    /// not — the epoch the busy-adopt follow renders from, ticket 06). `None`
    /// when the session has no user message: nothing to follow.
    pub newest_user_epoch: Option<i64>,
    /// Whether the newest user message is a Cola-Authored Message (`msg_cola_`
    /// id, ADR-0026) — one input to the suppression predicate.
    pub newest_user_is_cola_authored: bool,
}

impl SnapshotData {
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// ADR-0028 suppression predicate, reading this snapshot's gathered state.
    /// `already_mapped_to_this_thread` marks a re-activation (the only case
    /// where suppression can fire); first-time adoption always emits.
    pub fn should_emit(&self, already_mapped_to_this_thread: bool) -> bool {
        should_emit_snapshot(
            already_mapped_to_this_thread,
            self.status,
            self.has_pending(),
            self.newest_user_is_cola_authored,
        )
    }
}

/// The emit decision for RE-activating a session already mapped to this
/// thread (ADR-0028 suppression). First-time adoptions always emit the full
/// snapshot and never consult this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotEmit {
    /// Content to report (busy/retry, a pending request, or an external
    /// newest message) → the full snapshot card.
    Full,
    /// Nothing to report (idle, no pending, newest user cola-authored — the
    /// session's recent life is already visible in this thread) → a compact
    /// state instead of the full snapshot.
    Suppressed,
}

/// ADR-0028: decide whether a re-switch to a session already mapped to this
/// thread reports the full snapshot. Shared by the text `/switch` mapped-hit
/// branch and the switch card's 切换 op so the predicate cannot drift between
/// the two surfaces.
pub(crate) fn re_switch_emit(data: &SnapshotData) -> SnapshotEmit {
    if data.should_emit(true) {
        SnapshotEmit::Full
    } else {
        SnapshotEmit::Suppressed
    }
}

/// One 最近对话 tail entry: a text-bearing user/assistant message's role, its
/// created time (for display/ordering) and its verbatim text-part content.
#[derive(Clone)]
pub struct TailEntry {
    pub role: String,
    pub created_ms: i64,
    pub text: String,
}

/// Whether an activation should emit a snapshot card (ADR-0028 suppression).
///
/// The card is omitted only when ALL hold: the session is already mapped to
/// this thread (a re-activation, not a first adopt), the server reports it
/// idle, there is no pending request for it, and the newest user message is
/// cola-authored (its recent life is already visible in this thread). A status
/// that is unknown (`None`) is NOT idle, so a failed status read never
/// suppresses — first-time adoption always emits.
pub fn should_emit_snapshot(
    already_mapped_to_this_thread: bool,
    status: Option<opencode::types::SessionStatus>,
    has_pending: bool,
    newest_user_is_cola_authored: bool,
) -> bool {
    let suppressed = already_mapped_to_this_thread
        && status == Some(opencode::types::SessionStatus::Idle)
        && !has_pending
        && newest_user_is_cola_authored;
    !suppressed
}

/// Gather everything the snapshot card needs for an adopted session
/// `(session_id, directory)` — status, adopt-time pendings for THIS session,
/// and the transcript tail — from server reads alone. Best-effort per source.
pub(crate) async fn gather_snapshot(
    backend: &Arc<dyn opencode::Backend>,
    session_id: &str,
    directory: &str,
) -> SnapshotData {
    let status = match backend.session_status(session_id, Some(directory)).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("snapshot: session status for {} failed: {}", session_id, e);
            None
        }
    };

    let permissions = match backend.list_permissions(Some(directory)).await {
        Ok(perms) => perms,
        Err(e) => {
            tracing::warn!("snapshot: list permissions for {} failed: {}", session_id, e);
            Vec::new()
        }
    };
    let questions = match backend.list_questions(Some(directory)).await {
        Ok(questions) => questions,
        Err(e) => {
            tracing::warn!("snapshot: list questions for {} failed: {}", session_id, e);
            Vec::new()
        }
    };
    // Only the adopted session's own requests belong on the snapshot — never a
    // sibling session's in the same directory, and never a sub-task child's
    // (ADR-0028 keeps child-session pendings on today's standalone flow).
    let pending: Vec<crate::bridge::request::PendingRequest> = permissions
        .into_iter()
        .filter(|p| p.session_id.as_deref() == Some(session_id))
        .map(crate::bridge::request::PendingRequest::Permission)
        .chain(
            questions
                .into_iter()
                .filter(|q| q.session_id == session_id)
                .map(crate::bridge::request::PendingRequest::Question),
        )
        .collect();

    let messages = match backend.messages(session_id).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("snapshot: messages for {} failed: {}", session_id, e);
            Vec::new()
        }
    };
    let newest_user_epoch = newest_user_message(&messages).map(|(created, _)| created);
    let newest_user_is_cola_authored = newest_user_message(&messages)
        .map(|(_, id)| opencode::parsing::is_cola_message_id(id))
        .unwrap_or(false);
    let tail = transcript_tail(&messages);

    SnapshotData {
        session_id: session_id.to_string(),
        directory: directory.to_string(),
        status,
        pending,
        tail,
        newest_user_epoch,
        newest_user_is_cola_authored,
    }
}

/// ADR-0028: gather an adopted session's snapshot data (status, adopt-time
/// pendings, transcript tail) from server reads, restrict the pendings to the
/// claimable ones (the session's own, not already surfaced elsewhere), and
/// build its card with the given takeover verb (接管/切换). Shared by every
/// adoption surface so the gather-before-mapping sequence cannot drift between
/// them. The whole gather+build runs inside the adopted Session's `snapshot`
/// span (ADR-0048), so its best-effort read warnings are retrievable by it.
/// Returns the card together with the filtered data — the caller sends
/// the card and then claims the pendings with its message id.
pub(crate) async fn snapshot_card_for(
    core: &Arc<SharedCore>,
    verb: &str,
    info: &crate::opencode::types::SessionListInfo,
) -> (serde_json::Value, SnapshotData) {
    let thread_key = crate::bridge::span::thread_key_of(core, &info.id).await;
    let span = crate::bridge::span::snapshot(&info.id, thread_key.as_ref());
    async move {
        let data = gather_snapshot(&core.opencode, &info.id, &info.directory).await;
        snapshot_card_from_data(core, verb, &info.title, data).await
    }
    .instrument(span)
    .await
}

/// ADR-0028: the filter+build half of [`snapshot_card_for`] — restrict the
/// gathered data to the claimable pendings and build the card. Shared by
/// [`re_switch_snapshot`], which gathers first (the suppression decision needs
/// the raw data) and then filters, so the two surfaces cannot drift.
pub(crate) async fn snapshot_card_from_data(
    core: &Arc<SharedCore>,
    verb: &str,
    title: &str,
    data: SnapshotData,
) -> (serde_json::Value, SnapshotData) {
    let data = crate::bridge::snapshot_claims::claimable_pendings(core, data).await;
    let card = crate::feishu::snapshot_card::build_snapshot_card(verb, title, &data);
    (card, data)
}

/// What a re-activation's snapshot produced (ADR-0028).
pub(crate) enum ReSwitchSnapshot {
    /// Content to report: the full 切换 card plus the filtered data the caller
    /// claims against the message it sends.
    Full {
        card: serde_json::Value,
        data: SnapshotData,
    },
    /// Nothing to report (the suppression predicate fired): the caller renders
    /// its own compact state instead.
    Suppressed,
}

/// ADR-0028: a re-activation's snapshot — a session already mapped to this
/// thread, so the `thread_key` is known at the call site rather than resolved
/// from the store. Gathers the session's data, decides the emit, and on `Full`
/// restricts the pendings and builds the 切换 card. The whole gather+decide+
/// build sequence runs inside the Session's `snapshot` span (ADR-0048), so the
/// gather's best-effort read warnings are retrievable by the Session being
/// re-activated. Shared by the text `/switch` mapped-hit path and the switch
/// card's 切换 op so the span and the sequence cannot drift.
pub(crate) async fn re_switch_snapshot(
    core: &Arc<SharedCore>,
    thread_key: &crate::config::ThreadKey,
    session_id: &str,
    directory: &str,
    title: &str,
) -> ReSwitchSnapshot {
    let span = crate::bridge::span::snapshot(session_id, Some(thread_key));
    async move {
        let data = gather_snapshot(&core.opencode, session_id, directory).await;
        match re_switch_emit(&data) {
            SnapshotEmit::Full => {
                let (card, data) = snapshot_card_from_data(core, "切换", title, data).await;
                ReSwitchSnapshot::Full { card, data }
            }
            SnapshotEmit::Suppressed => ReSwitchSnapshot::Suppressed,
        }
    }
    .instrument(span)
    .await
}

/// The newest user message (by created time), if any — `(created, id)`. A
/// session with no user message has no newest; the caller decides the meaning
/// (cola authorship for suppression, an epoch for the busy-adopt follow).
fn newest_user_message(messages: &[opencode::types::SessionMessage]) -> Option<(i64, &str)> {
    messages
        .iter()
        .filter(|m| m.info.role.as_deref() == Some("user"))
        .filter_map(|m| m.info.time.as_ref().map(|t| (t.created, m.info.id.as_str())))
        .max_by_key(|(created, _)| *created)
}

/// The 最近对话 tail: the last (at most `limit`) text-bearing user/assistant
/// messages, newest last. A message is text-bearing when it carries at least
/// one `text` part with non-empty content; reasoning/tool/step parts are inner
/// monologue, not conversation, and are excluded. Messages without a created
/// time cannot be ordered and are dropped.
pub(crate) fn transcript_tail(messages: &[opencode::types::SessionMessage]) -> Vec<TailEntry> {
    const TAIL_LIMIT: usize = 4;
    let mut out: Vec<TailEntry> = messages
        .iter()
        .filter(|m| matches!(m.info.role.as_deref(), Some("user" | "assistant")))
        .filter_map(|m| {
            let text = text_parts_only(&m.parts);
            if text.trim().is_empty() {
                return None;
            }
            Some(TailEntry {
                role: m.info.role.clone().unwrap_or_default(),
                created_ms: m.info.time.as_ref()?.created,
                text,
            })
        })
        .collect();
    // Sort oldest → newest, then keep only the newest `TAIL_LIMIT` so the tail
    // is the last four messages, newest last.
    out.sort_by_key(|t| t.created_ms);
    let start = out.len().saturating_sub(TAIL_LIMIT);
    out.drain(..start);
    out
}

/// Verbatim concatenation of a message's `text`-type parts (newline-joined),
/// excluding reasoning/tool/step parts. Empty when the message carries no text.
fn text_parts_only(parts: &serde_json::Value) -> String {
    parts
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode::types::{MessageInfo, MessageTime, SessionMessage};

    fn msg(id: &str, role: &str, created: i64, parts: serde_json::Value) -> SessionMessage {
        SessionMessage {
            info: MessageInfo {
                id: id.into(),
                role: Some(role.into()),
                parent_id: None,
                time: Some(MessageTime { created }),
                model_id: None,
                provider_id: None,
                tokens: None,
            },
            parts,
        }
    }

    fn text(t: &str) -> serde_json::Value {
        serde_json::json!([{ "type": "text", "text": t }])
    }

    /// A turn with reasoning + tool (no conversation text) — must NOT appear in
    /// the tail, and an image-only user message (file parts, no text) likewise.
    fn reasoning_only() -> serde_json::Value {
        serde_json::json!([
            { "type": "step-start", "snapshot": "x" },
            { "type": "reasoning", "text": "我要先查目录。" },
            { "type": "tool", "tool": "bash", "state": { "status": "completed", "input": { "command": "ls" } } },
            { "type": "step-finish", "reason": "tool-calls" }
        ])
    }

    fn image_only() -> serde_json::Value {
        serde_json::json!([{ "type": "file", "mime": "image/png", "url": "data:image/png;base64,x" }])
    }

    fn cola_user(created: i64, t: &str) -> SessionMessage {
        msg(&format!("msg_cola_{created}"), "user", created, text(t))
    }

    #[test]
    fn suppression_matrix() {
        // Full matrix: adopt/re-switch × idle/busy/retry/unknown × pending/none ×
        // external/cola newest user. The ONLY suppressed cell is re-activation +
        // idle + no pending + cola-authored newest (ADR-0028): a first adoption
        // always snapshots, and an unknown status is never treated as idle.
        let statuses = [
            ("idle", Some(opencode::types::SessionStatus::Idle)),
            ("busy", Some(opencode::types::SessionStatus::Busy)),
            ("retry", Some(opencode::types::SessionStatus::Retry)),
            ("unknown", None),
        ];
        for (sname, status) in statuses {
            for has_pending in [false, true] {
                for cola in [false, true] {
                    for already_mapped in [false, true] {
                        let emits = should_emit_snapshot(already_mapped, status, has_pending, cola);
                        let expect_suppressed = already_mapped && sname == "idle" && !has_pending && cola;
                        assert_eq!(
                            emits, !expect_suppressed,
                            "matrix cell: mapped={already_mapped} status={sname} \
                             pending={has_pending} cola={cola}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn tail_is_newest_last_text_bearing_only() {
        let messages = vec![
            msg("a", "user", 1000, text("问题一")),
            msg("b", "assistant", 2000, reasoning_only()), // reasoning/tool only: excluded
            msg("c", "assistant", 3000, text("回答一")),
            msg("d", "user", 4000, image_only()), // image only: excluded
            msg("e", "user", 5000, text("问题二")),
            msg("f", "assistant", 6000, text("回答二")),
            msg("g", "system", 7000, text("系统提示")), // system role: excluded
        ];
        let tail = transcript_tail(&messages);
        // Reasoning-only, image-only and system messages are dropped; the four
        // surviving text-bearing messages stay newest-last.
        let roles: Vec<_> = tail.iter().map(|t| t.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant", "user", "assistant"]);
        let texts: Vec<_> = tail.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["问题一", "回答一", "问题二", "回答二"]);
        assert_eq!(tail[0].created_ms, 1000);
        assert_eq!(tail[3].created_ms, 6000);
    }

    #[test]
    fn tail_caps_at_four() {
        let mut messages = Vec::new();
        for i in 0..10 {
            messages.push(msg(&format!("u{i}"), "user", i * 1000, text(&format!("m{i}"))));
        }
        let tail = transcript_tail(&messages);
        assert_eq!(tail.len(), 4, "tail must cap at 4");
        assert_eq!(tail[0].text, "m6");
        assert_eq!(tail[3].text, "m9");
        assert_eq!(tail[3].created_ms, 9000);
    }

    #[test]
    fn tail_joins_multiple_text_parts_and_keeps_roles() {
        let messages = vec![msg(
            "u",
            "user",
            1000,
            serde_json::json!([
                { "type": "text", "text": "第一段" },
                { "type": "file", "mime": "image/png", "url": "data:..." },
                { "type": "text", "text": "第二段" }
            ]),
        )];
        let tail = transcript_tail(&messages);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].text, "第一段\n第二段");
        assert_eq!(tail[0].role, "user");
    }

    #[test]
    fn newest_user_cola_authored_detection() {
        // Cola's own prompt is the newest user message.
        let messages = vec![
            msg("msg_other", "user", 1000, text("外部问题")),
            msg("msg_cola_x", "user", 2000, text("我发的")),
            msg("assist", "assistant", 3000, text("回答")),
        ];
        let (created, id) = newest_user_message(&messages).unwrap();
        assert_eq!(created, 2000);
        assert!(opencode::parsing::is_cola_message_id(id));

        // An external user message newer than cola's is NOT cola-authored.
        let messages = vec![
            msg("msg_cola_x", "user", 1000, text("我发的")),
            msg("msg_other", "user", 2000, text("外部问题")),
        ];
        let (created, id) = newest_user_message(&messages).unwrap();
        assert_eq!(created, 2000);
        assert!(!opencode::parsing::is_cola_message_id(id));

        // No user messages → None (no newest, no epoch).
        let messages = vec![msg("a", "assistant", 1000, text("回答"))];
        assert!(newest_user_message(&messages).is_none());

        // No time on any user message → None.
        let mut no_time = cola_user(1000, "hi");
        no_time.info.time = None;
        assert!(newest_user_message(&[no_time]).is_none());
    }

    #[tokio::test]
    async fn gather_filters_pending_to_the_adopted_session_and_reads_status() {
        use crate::bridge::test_support::MockBackend;
        use crate::opencode::types::{PermissionRequest, QuestionInfo, QuestionRequest};

        let mut mock = MockBackend::new(text("你好"));
        mock.session_statuses
            .insert("ses_adopted".into(), Some(opencode::types::SessionStatus::Busy));
        // A permission and a question for the ADOPTED session, plus one of each
        // for a sibling session in the SAME directory — only the former belong
        // on the snapshot.
        mock.permissions = vec![
            PermissionRequest {
                request_id: "req_adopted".into(),
                session_id: Some("ses_adopted".into()),
                permission: Some("bash".into()),
                patterns: vec!["ls".into()],
                metadata: None,
                always: vec![],
            },
            PermissionRequest {
                request_id: "req_other".into(),
                session_id: Some("ses_sibling".into()),
                permission: Some("bash".into()),
                patterns: vec!["rm".into()],
                metadata: None,
                always: vec![],
            },
        ];
        mock.questions = vec![
            QuestionRequest {
                id: "q_adopted".into(),
                session_id: "ses_adopted".into(),
                questions: vec![QuestionInfo {
                    question: "继续?".into(),
                    header: "确认".into(),
                    options: vec![],
                    multiple: None,
                    custom: None,
                }],
            },
            QuestionRequest {
                id: "q_other".into(),
                session_id: "ses_sibling".into(),
                questions: vec![],
            },
        ];
        let backend: Arc<dyn opencode::Backend> = Arc::new(mock);
        let snap = gather_snapshot(&backend, "ses_adopted", "/work/proj").await;

        assert_eq!(snap.status, Some(opencode::types::SessionStatus::Busy));
        assert!(snap.has_pending());
        let ids: Vec<_> = snap.pending.iter().map(|p| p.id().to_string()).collect();
        assert_eq!(
            ids,
            vec!["req_adopted", "q_adopted"],
            "sibling pendings leaked in"
        );
    }

    #[tokio::test]
    async fn gather_treats_a_failed_status_read_as_unknown() {
        use crate::bridge::test_support::MockBackend;

        let mut mock = MockBackend::new(text("你好"));
        mock.session_status_error = Some("simulated failure".into());
        let backend: Arc<dyn opencode::Backend> = Arc::new(mock);
        let snap = gather_snapshot(&backend, "ses_adopted", "/work/proj").await;
        assert_eq!(snap.status, None, "a failed status read must not guess a status");
    }

    #[tokio::test]
    async fn gather_treats_an_unrecognised_status_type_as_unknown() {
        use crate::bridge::test_support::MockBackend;

        // The server reported an entry for the session whose status type cola
        // does not recognise (`Ok(None)` at the seam) — unknown, never guessed.
        let mut mock = MockBackend::new(text("你好"));
        mock.session_statuses.insert("ses_adopted".into(), None);
        let backend: Arc<dyn opencode::Backend> = Arc::new(mock);
        let snap = gather_snapshot(&backend, "ses_adopted", "/work/proj").await;
        assert_eq!(snap.status, None, "an unknown status type must not be guessed");
    }

    #[tokio::test]
    async fn gather_maps_absent_session_to_idle() {
        use crate::bridge::test_support::MockBackend;

        let mock = MockBackend::new(text("你好"));
        let backend: Arc<dyn opencode::Backend> = Arc::new(mock);
        let snap = gather_snapshot(&backend, "ses_adopted", "/work/proj").await;
        assert_eq!(snap.status, Some(opencode::types::SessionStatus::Idle));
    }
}
