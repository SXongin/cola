use std::sync::Arc;

use tracing::Instrument;

use crate::backend::{SessionTranscript, TailEntry, TurnAnchor};
use crate::bridge::handles::SnapshotHandles;
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
    pub pending: Vec<crate::bridge::request::kind::PendingRequest>,
    /// The session's pendings that `claimable_pendings` kept OFF this snapshot
    /// because they are already surfaced elsewhere (a live inline block, a
    /// standalone card, or an earlier snapshot — ADR-0028 update 2026-09-25).
    /// `None` when there were none. The status chip points at that card
    /// instead of falling back to the server run state; see [`ElsewherePending`].
    pub pending_elsewhere: Option<ElsewherePending>,
    /// The 最近对话 tail: the last text-bearing user/assistant messages,
    /// newest last — the Session Transcript's shared `transcript_tail`
    /// projection.
    pub tail: Vec<TailEntry>,
    /// The newest user message's anchor — its identity together with its
    /// server time — from the Session Transcript's `newest_user` projection.
    /// The busy-adopt follow renders from it (ticket 06), so the message it
    /// answers travels with the turn. `None` when the session has no user
    /// message: nothing to follow.
    pub newest_user_anchor: Option<TurnAnchor>,
    /// Whether the newest user message is a Cola-Authored Message (`msg_cola_`
    /// id, ADR-0026) — one input to the suppression predicate.
    pub newest_user_is_cola_authored: bool,
}

/// A pending request that is already hosted by another card, so the snapshot
/// does not embed it (ADR-0028 update 2026-09-25). The status chip points at
/// that card — and only promises a pin when Message Pin is on, since the pin
/// sync is gated by the same `[bridge] instant_reminder` opt-in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ElsewherePending {
    /// Message Pin is on: the hosting card is pinned, so the pointer may say
    /// 置顶.
    Pinned,
    /// Message Pin is off: the pointer names the original card without
    /// promising a pin.
    Unpinned,
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
/// and the Session Transcript's newest user + tail — from server reads alone.
/// Best-effort per source.
pub(crate) async fn gather_snapshot(
    backend: &Arc<dyn crate::backend::Backend>,
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
    let pending: Vec<crate::bridge::request::kind::PendingRequest> = permissions
        .into_iter()
        .filter(|p| p.session_id.as_deref() == Some(session_id))
        .map(crate::bridge::request::kind::PendingRequest::Permission)
        .chain(
            questions
                .into_iter()
                .filter(|q| q.session_id == session_id)
                .map(crate::bridge::request::kind::PendingRequest::Question),
        )
        .collect();

    let transcript = match backend.transcript(session_id).await {
        Ok(transcript) => transcript,
        Err(e) => {
            tracing::warn!("snapshot: transcript for {} failed: {}", session_id, e);
            SessionTranscript::default()
        }
    };
    // Newest user and tail come from the shared transcript projections (ADR-0053),
    // so the snapshot cannot drift from the Turn and external-sync reads.
    let newest_user = transcript.newest_user();
    let newest_user_anchor = newest_user.and_then(|message| message.anchor());
    let newest_user_is_cola_authored = newest_user
        .map(|message| opencode::parsing::is_cola_message_id(message.id.as_str()))
        .unwrap_or(false);
    let tail = transcript.transcript_tail();

    SnapshotData {
        session_id: session_id.to_string(),
        directory: directory.to_string(),
        status,
        pending,
        pending_elsewhere: None,
        tail,
        newest_user_anchor,
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
/// the card and then claims the pendings with its message id. `back` is the
/// `/switch`-list state when the adoption came from the list card (ADR-0052);
/// every other adoption surface passes `None`.
pub(crate) async fn snapshot_card_for(
    handles: &SnapshotHandles,
    verb: &str,
    info: &crate::opencode::types::SessionListInfo,
    back: Option<&crate::feishu::card::session::BackToList>,
) -> (serde_json::Value, SnapshotData) {
    let thread_key = crate::bridge::span::thread_key_of(&handles.sessions, &info.id).await;
    let span = crate::bridge::span::snapshot(&info.id, thread_key.as_ref());
    async move {
        let data = gather_snapshot(&handles.backend, &info.id, &info.directory).await;
        snapshot_card_from_data(handles, verb, &info.title, data, back).await
    }
    .instrument(span)
    .await
}

/// ADR-0028: the filter+build half of [`snapshot_card_for`] — restrict the
/// gathered data to the claimable pendings and build the card. Shared by
/// [`re_switch_snapshot`], which gathers first (the suppression decision needs
/// the raw data) and then filters, so the two surfaces cannot drift.
pub(crate) async fn snapshot_card_from_data(
    handles: &SnapshotHandles,
    verb: &str,
    title: &str,
    data: SnapshotData,
    back: Option<&crate::feishu::card::session::BackToList>,
) -> (serde_json::Value, SnapshotData) {
    let data = crate::bridge::snapshot_claims::claimable_pendings(
        &handles.requests,
        &handles.cards,
        handles.pins_enabled,
        data,
    )
    .await;
    let card = crate::feishu::snapshot_card::build_snapshot_card(verb, title, &data, back);
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
/// card's 切换 op so the span and the sequence cannot drift. `back` is the
/// `/switch`-list state when the re-switch came from the list card
/// (ADR-0052); the text path passes `None`.
pub(crate) async fn re_switch_snapshot(
    handles: &SnapshotHandles,
    thread_key: &crate::config::ThreadKey,
    session_id: &str,
    directory: &str,
    title: &str,
    back: Option<&crate::feishu::card::session::BackToList>,
) -> ReSwitchSnapshot {
    let span = crate::bridge::span::snapshot(session_id, Some(thread_key));
    async move {
        let data = gather_snapshot(&handles.backend, session_id, directory).await;
        match re_switch_emit(&data) {
            SnapshotEmit::Full => {
                let (card, data) = snapshot_card_from_data(handles, "切换", title, data, back).await;
                ReSwitchSnapshot::Full { card, data }
            }
            SnapshotEmit::Suppressed => ReSwitchSnapshot::Suppressed,
        }
    }
    .instrument(span)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        FinishReason, MessageRole, OtherPart, Part, ReasoningPart, StepFinish, StepStart, ToolCall,
        ToolIdentity, ToolOutput, ToolStatus, TranscriptMessage,
    };
    use crate::bridge::test_support::{MockBackend, text_part, typed_message};

    fn user(id: &str, created: i64, texts: &[&str]) -> TranscriptMessage {
        typed_message(
            id,
            MessageRole::User,
            Some(created),
            texts.iter().map(|t| text_part(t)).collect(),
        )
    }

    fn assistant(id: &str, created: i64, texts: &[&str]) -> TranscriptMessage {
        typed_message(
            id,
            MessageRole::Assistant,
            Some(created),
            texts.iter().map(|t| text_part(t)).collect(),
        )
    }

    /// A mock whose session serves exactly this typed transcript: a scripted
    /// transcript wins over the wire-shape fallback, so the fixture is the
    /// only thing the snapshot can be reading.
    fn typed_backend(messages: Vec<TranscriptMessage>) -> MockBackend {
        let mut mock = MockBackend::new(serde_json::json!([]));
        mock.given_transcript("ses_adopted", vec![SessionTranscript::new(messages)]);
        mock
    }

    async fn gather_from_typed_backend(mock: MockBackend) -> SnapshotData {
        let backend: Arc<dyn crate::backend::Backend> = Arc::new(mock);
        gather_snapshot(&backend, "ses_adopted", "/work/proj").await
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

    #[tokio::test]
    async fn tail_is_newest_last_text_bearing_only() {
        // A turn with reasoning + tool (no conversation text) — must NOT appear
        // in the tail, and an image-only user message (file parts, no text)
        // likewise.
        let reasoning_only = typed_message(
            "b",
            MessageRole::Assistant,
            Some(2000),
            vec![
                Part::StepStart(StepStart),
                Part::Reasoning(ReasoningPart {
                    text: "我要先查目录。".into(),
                    started_at: None,
                }),
                Part::Tool(ToolCall {
                    identity: ToolIdentity {
                        name: "bash".into(),
                        call_id: "call_1".into(),
                    },
                    status: ToolStatus::Completed,
                    started_at: None,
                    input: Some(serde_json::json!({ "command": "ls" })),
                    metadata: None,
                    output: ToolOutput::default(),
                }),
                Part::StepFinish(StepFinish {
                    reason: FinishReason::ToolCalls,
                }),
            ],
        );
        let image_only = typed_message(
            "d",
            MessageRole::User,
            Some(4000),
            vec![Part::Other(OtherPart {
                kind: "file".into(),
                raw: serde_json::Value::Null,
            })],
        );
        let snap = gather_from_typed_backend(typed_backend(vec![
            user("a", 1000, &["问题一"]),
            reasoning_only,
            assistant("c", 3000, &["回答一"]),
            image_only,
            user("e", 5000, &["问题二"]),
            assistant("f", 6000, &["回答二"]),
            typed_message("g", MessageRole::System, Some(7000), vec![text_part("系统提示")]),
        ]))
        .await;
        let tail = snap.tail;
        // Reasoning-only, image-only and system messages are dropped; the four
        // surviving text-bearing messages stay newest-last.
        let roles: Vec<_> = tail.iter().map(|t| t.role.clone()).collect();
        assert_eq!(
            roles,
            vec![
                MessageRole::User,
                MessageRole::Assistant,
                MessageRole::User,
                MessageRole::Assistant
            ]
        );
        let texts: Vec<_> = tail.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["问题一", "回答一", "问题二", "回答二"]);
        assert_eq!(tail[0].created_ms, 1000);
        assert_eq!(tail[3].created_ms, 6000);
    }

    #[tokio::test]
    async fn tail_caps_at_four() {
        let messages = (0..10)
            .map(|i| user(&format!("u{i}"), i * 1000, &[&format!("m{i}")]))
            .collect();
        let snap = gather_from_typed_backend(typed_backend(messages)).await;
        let tail = snap.tail;
        assert_eq!(tail.len(), 4, "tail must cap at 4");
        assert_eq!(tail[0].text, "m6");
        assert_eq!(tail[3].text, "m9");
        assert_eq!(tail[3].created_ms, 9000);
    }

    #[tokio::test]
    async fn tail_joins_multiple_text_parts_and_keeps_roles() {
        let snap = gather_from_typed_backend(typed_backend(vec![typed_message(
            "u",
            MessageRole::User,
            Some(1000),
            vec![
                text_part("第一段"),
                Part::Other(OtherPart {
                    kind: "file".into(),
                    raw: serde_json::Value::Null,
                }),
                text_part("第二段"),
            ],
        )]))
        .await;
        let tail = snap.tail;
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].text, "第一段\n第二段");
        assert_eq!(tail[0].role, MessageRole::User);
    }

    #[tokio::test]
    async fn newest_user_cola_authored_detection() {
        // Cola's own prompt is the newest user message.
        let snap = gather_from_typed_backend(typed_backend(vec![
            user("msg_other", 1000, &["外部问题"]),
            user("msg_cola_x", 2000, &["我发的"]),
            assistant("assist", 3000, &["回答"]),
        ]))
        .await;
        assert_eq!(snap.newest_user_anchor.map(|a| a.created_ms), Some(2000));
        assert!(snap.newest_user_is_cola_authored);

        // An external user message newer than cola's is NOT cola-authored.
        let snap = gather_from_typed_backend(typed_backend(vec![
            user("msg_cola_x", 1000, &["我发的"]),
            user("msg_other", 2000, &["外部问题"]),
        ]))
        .await;
        assert_eq!(snap.newest_user_anchor.map(|a| a.created_ms), Some(2000));
        assert!(!snap.newest_user_is_cola_authored);

        // No user messages → None (no newest, no epoch).
        let snap = gather_from_typed_backend(typed_backend(vec![assistant("a", 1000, &["回答"])])).await;
        assert_eq!(snap.newest_user_anchor, None);

        // No time on any user message → None.
        let no_time = typed_message("msg_cola_x", MessageRole::User, None, vec![text_part("hi")]);
        let snap = gather_from_typed_backend(typed_backend(vec![no_time])).await;
        assert_eq!(snap.newest_user_anchor, None);
    }

    #[tokio::test]
    async fn gather_filters_pending_to_the_adopted_session_and_reads_status() {
        use crate::opencode::types::{PermissionRequest, QuestionInfo, QuestionRequest};

        let mut mock = typed_backend(vec![user("msg_u1", 1000, &["你好"])]);
        mock.with_session_status("ses_adopted", Some(opencode::types::SessionStatus::Busy));
        // A permission and a question for the ADOPTED session, plus one of each
        // for a sibling session in the SAME directory — only the former belong
        // on the snapshot.
        mock.ask_permissions(vec![
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
        ]);
        mock.ask_questions(vec![
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
        ]);
        let snap = gather_from_typed_backend(mock).await;

        assert_eq!(snap.status, Some(opencode::types::SessionStatus::Busy));
        assert!(snap.has_pending());
        let ids: Vec<_> = snap.pending.iter().map(|p| p.id().to_string()).collect();
        assert_eq!(
            ids,
            vec!["req_adopted", "q_adopted"],
            "sibling pendings leaked in"
        );
        // The tail and newest user come from the scripted transcript, not the
        // wire shape `messages` would have served.
        assert_eq!(snap.tail.len(), 1);
        assert_eq!(snap.tail[0].text, "你好");
        assert_eq!(snap.newest_user_anchor.map(|a| a.created_ms), Some(1000));
    }

    #[tokio::test]
    async fn gather_treats_a_failed_status_read_as_unknown() {
        let mut mock = typed_backend(vec![user("msg_u1", 1000, &["你好"])]);
        mock.status_read_fails("simulated failure");
        let snap = gather_from_typed_backend(mock).await;
        assert_eq!(snap.status, None, "a failed status read must not guess a status");
    }

    #[tokio::test]
    async fn gather_treats_an_unrecognised_status_type_as_unknown() {
        // The server reported an entry for the session whose status type cola
        // does not recognise (`Ok(None)` at the seam) — unknown, never guessed.
        let mut mock = typed_backend(vec![user("msg_u1", 1000, &["你好"])]);
        mock.with_session_status("ses_adopted", None);
        let snap = gather_from_typed_backend(mock).await;
        assert_eq!(snap.status, None, "an unknown status type must not be guessed");
    }

    #[tokio::test]
    async fn gather_maps_absent_session_to_idle() {
        let mock = typed_backend(vec![user("msg_u1", 1000, &["你好"])]);
        let snap = gather_from_typed_backend(mock).await;
        assert_eq!(snap.status, Some(opencode::types::SessionStatus::Idle));
    }
}
