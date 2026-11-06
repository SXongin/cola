use std::sync::Arc;

use tracing::Instrument;

use crate::backend::{SessionTranscript, TailEntry, TurnAnchor};
use crate::bridge::handles::SnapshotHandles;

/// The **Session Snapshot** read (ADR-0028, ADR-0100): the state a takeover or
/// a `/card` read reports — status, the pending requests, the transcript tail.
/// Built purely from server reads, never from a prompt, a session write, or a
/// guess. Each source is best-effort: a read failure degrades that field
/// (status → `None`, lists → empty) and is logged, so a read can never be
/// blocked by the gather.
pub struct SnapshotData {
    pub session_id: String,
    pub directory: String,
    /// The server's run state for the session at activation time. `None` when
    /// the status read failed or returned an unrecognised type — the chip is
    /// omitted rather than guessed (ADR-0028).
    pub status: Option<crate::backend::types::SessionStatus>,
    /// Whether the Session is in a Waiting turn (ADR-0059) at activation time:
    /// the newest user message's Turn has finished while the read's transcript
    /// still carries live Background Tasks, so the Turn will wake by itself.
    /// A `/card` read of such a Session follows the wait — the card yields ⏳
    /// with the ledger and Session Sync serves the completion Wake in place
    /// (#586) — instead of freezing the one-shot read, and the status chip
    /// names the wait instead of 空闲. Scoped to the newest Turn on
    /// purpose: a NEWER user message whose own Turn has not finished (an
    /// external message just submitted, say) leaves the fact false — the
    /// older Turn's Wake belongs to the Session Sync continuation, never to a
    /// follow anchored at the newer message. Always `false` on a generation
    /// without Background Task facts (V1) and when the server reads anything
    /// but idle (never guessed).
    pub waiting: bool,
    /// The session's pending permission/question requests: the read's count
    /// pointer, and the set a busy follow may seed onto its card. The gather
    /// fills it with every request whose `sessionID` is the read session (never
    /// another session's in the same directory, and never a sub-task child's —
    /// those keep their standalone flow).
    pub pending: Vec<crate::bridge::request::kind::PendingRequest>,
    /// The 最近对话 tail: the last conversation user/assistant messages,
    /// newest last, with the **Compaction**s that happened inside that window
    /// interleaved at their server positions — a marker that never consumes one
    /// of the tail's four message slots (ADR-0082) — from the Session
    /// Transcript's shared `transcript_tail` projection (a files-only user
    /// message is included, for its record line).
    pub tail: Vec<TailEntry>,
    /// The newest user message's anchor — its identity together with its
    /// server time — from the Session Transcript's `newest_user` projection.
    /// The busy-adopt follow renders from it (ticket 06), so the message it
    /// answers travels with the turn. `None` when the session has no user
    /// message: nothing to follow.
    pub newest_user_anchor: Option<TurnAnchor>,
    /// Whether the newest user message is a Cola-Authored Message (`msg_cola_`
    /// id, ADR-0026) — the busy follow's guard: cola's own run hands off
    /// through the Chain Record, never a Session Card (ADR-0100).
    pub newest_user_is_cola_authored: bool,
}

/// Gather everything a Session Card read needs for an adopted session
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
    // (ADR-0028 keeps child-session pendings on today's standalone flow). A
    // permission whose target the card cannot show in full is left out too:
    // embedding it would bypass the permission flow's decline guard (a claimed
    // request is never surfaced again), and the poll sweep declines it with the
    // reason card.
    let pending: Vec<crate::bridge::request::kind::PendingRequest> = permissions
        .into_iter()
        .filter(|p| p.session_id.as_deref() == Some(session_id) && p.targets_are_fully_shown())
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
    let newest_user_is_cola_authored = match newest_user {
        Some(message) => backend.is_cola_authored(message.id.as_str()).await,
        None => false,
    };
    let tail = transcript.transcript_tail();
    // The ADR-0059 Waiting fact (#586): the newest user message's Turn is
    // complete (an assistant step finished it) while live Background Tasks
    // remain, and the server reads idle — exactly the read the follow's own
    // settle decision turns into the ⏳ yield. A running Execution takes the
    // busy arm; a retry, an unread status, or a newer user message whose Turn
    // has not finished is never guessed into a wait.
    let waiting = status == Some(crate::backend::types::SessionStatus::Idle)
        && newest_user_anchor.as_ref().is_some_and(|anchor| {
            transcript.turn_for_user(anchor).complete
                && transcript.settle(Some(anchor)) == crate::backend::TurnSettle::Waiting
        });

    SnapshotData {
        session_id: session_id.to_string(),
        directory: directory.to_string(),
        status,
        waiting,
        pending,
        tail,
        newest_user_anchor,
        newest_user_is_cola_authored,
    }
}

/// ADR-0100: the ONE line a text adoption confirms in, replacing the Session
/// Snapshot card. Carries the shared body's status summary
/// ([`crate::feishu::card::session::status_line`]), so a takeover
/// still answers "did the last Turn end, is something blocking me". The whole
/// gather runs inside the adopted Session's `snapshot` span (ADR-0048), so its
/// best-effort read warnings are retrievable by that Session. The thread key is
/// resolved from the store: a re-switch already has one, a first adoption does
/// not (the mapping is written after this, so the line reflects the session's
/// pre-adoption state).
pub(crate) async fn adoption_line(
    handles: &SnapshotHandles,
    verb: &str,
    info: &crate::backend::types::SessionListInfo,
) -> String {
    let thread_key = crate::bridge::span::thread_key_of(&handles.sessions, &info.id).await;
    let span = crate::bridge::span::snapshot(&info.id, thread_key.as_ref());
    async move {
        // Unlike the old adoption card, this line is the adoption's only
        // answer: a server that accepts TCP but never replies must not hang
        // `/switch`, `/attach` or `/topic --adopt`. Bound the whole gather and
        // degrade to the title-only line on timeout (no status is omitted, not
        // guessed).
        let read = crate::bridge::bounded_call("adoption read", handles.read_timeout_ms, async {
            Ok::<_, crate::error::BridgeError>(
                gather_snapshot(&handles.backend, &info.id, &info.directory).await,
            )
        })
        .await;
        let data = read
            .and_then(|result| result.ok())
            .unwrap_or_else(|| SnapshotData {
                session_id: info.id.clone(),
                directory: info.directory.clone(),
                status: None,
                waiting: false,
                pending: Vec::new(),
                tail: Vec::new(),
                newest_user_anchor: None,
                newest_user_is_cola_authored: false,
            });
        adoption_line_from_data(verb, &info.title, &data)
    }
    .instrument(span)
    .await
}

/// [`adoption_line`]'s pure half: `已{verb}「{title}」` plus the shared
/// composition's ONE-line status summary between ` · `, so the confirmation
/// stays ONE line. The title goes through the same
/// [`crate::feishu::card::session::display_title`] the card uses (so an empty
/// label falls back to the id tail either way), then every whitespace run —
/// including a title's embedded newline — collapses to one space: a multi-line
/// server label must not break the single-line contract. No status to report
/// (an unreadable status read) omits the summary rather than guessing one.
pub(crate) fn adoption_line_from_data(verb: &str, title: &str, data: &SnapshotData) -> String {
    let mut title = crate::feishu::card::session::display_title(title, &data.session_id)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        // A whitespace-only label collapses to nothing; keep the id tail the
        // card would have named instead of an empty pair of brackets.
        title = crate::bridge::display::id_tail(&data.session_id);
    }
    match crate::feishu::card::session::status_line(data) {
        Some(status) => format!("已{verb}「{title}」 · {status}"),
        None => format!("已{verb}「{title}」"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        BackgroundTask, FinishReason, MessageRole, OtherPart, Part, ReasoningPart, StepFinish, StepStart,
        TailMessage, ToolCall, ToolIdentity, ToolOutput, ToolStatus, TranscriptMessage,
    };
    use crate::bridge::test_support::{
        MockBackend, background_shell, execution, list_session, realistic_parts, test_config, test_work_dir,
        text_part, typed_message,
    };

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

    /// An assistant step that finishes its Turn — the fact
    /// [`SessionTranscript::turn_for_user`]'s `complete` reads.
    fn finished_turn(id: &str, created: i64) -> TranscriptMessage {
        typed_message(
            id,
            MessageRole::Assistant,
            Some(created),
            vec![Part::StepFinish(StepFinish {
                reason: FinishReason::Stop,
            })],
        )
    }

    /// A mock whose session serves exactly this typed transcript: a scripted
    /// transcript wins over the mock's default shape, so the fixture is the
    /// only thing the snapshot can be reading.
    fn typed_backend(messages: Vec<TranscriptMessage>) -> MockBackend {
        let mut mock = MockBackend::new(Vec::new());
        mock.given_transcript("ses_adopted", vec![SessionTranscript::new(messages)]);
        mock
    }

    /// Like [`typed_backend`], with live Background Tasks on the served read —
    /// the fact the Waiting detection derives from (#586). An Execution
    /// boundary rides along so a finished turn reads as idle.
    fn typed_backend_with_tasks(messages: Vec<TranscriptMessage>, tasks: Vec<BackgroundTask>) -> MockBackend {
        let mut mock = MockBackend::new(Vec::new());
        mock.given_transcript(
            "ses_adopted",
            vec![
                SessionTranscript::new(messages)
                    .with_executions(vec![execution(2_500)])
                    .with_background_tasks(tasks),
            ],
        );
        mock
    }

    async fn gather_from_typed_backend(mock: MockBackend) -> SnapshotData {
        let backend: Arc<dyn crate::backend::Backend> = Arc::new(mock);
        gather_snapshot(&backend, "ses_adopted", "/work/proj").await
    }

    /// Whole-branch re-review #883: a permission whose target a card cannot
    /// render in full never enters the read, so neither the idle pointer nor a
    /// busy follow can surface it — the permission flow's decline guard keeps
    /// it off a card that would offer a clipped command. The in-budget request
    /// still rides.
    #[tokio::test]
    async fn an_unshowable_permission_never_enters_the_read() {
        let mut backend = typed_backend(vec![
            user("msg_u1", 1_000, &["帮我改一下"]),
            assistant("msg_a1", 2_000, &["好的"]),
        ]);
        let too_long = format!(
            "echo {}",
            "x".repeat(crate::backend::types::PERMISSION_COMMAND_MAX_CHARS)
        );
        backend.ask_permissions(vec![
            crate::backend::types::PermissionRequest {
                request_id: "per_ok".into(),
                session_id: Some("ses_adopted".into()),
                permission: Some("bash".into()),
                patterns: vec!["ls".into()],
                metadata: None,
                always: Vec::new(),
                always_supported: true,
            },
            crate::backend::types::PermissionRequest {
                request_id: "per_long".into(),
                session_id: Some("ses_adopted".into()),
                permission: Some("bash".into()),
                patterns: vec![too_long],
                metadata: None,
                always: Vec::new(),
                always_supported: true,
            },
        ]);

        let data = gather_from_typed_backend(backend).await;
        let ids: Vec<&str> = data.pending.iter().map(|req| req.id()).collect();
        assert_eq!(
            ids,
            vec!["per_ok"],
            "only the fully showable pending may reach the read"
        );
    }

    /// A file part that inlines no payload is not a File Content: it stays a raw
    /// `Other`, carries no conversation text, and is dropped from the tail
    /// (ADR-0076: a real `Part::File` message is kept — the typed tail test
    /// covers that).
    #[tokio::test]
    async fn tail_is_newest_last_text_bearing_only() {
        // A turn with reasoning + tool (no conversation text) — must NOT appear
        // in the tail, and a file part that inlines no payload (a raw `Other`,
        // not a File Content) likewise.
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
        let messages: Vec<&TailMessage> = tail.iter().filter_map(TailEntry::message).collect();
        // Reasoning-only, image-only and system messages are dropped; the four
        // surviving text-bearing messages stay newest-last.
        let roles: Vec<_> = messages.iter().map(|t| t.role.clone()).collect();
        assert_eq!(
            roles,
            vec![
                MessageRole::User,
                MessageRole::Assistant,
                MessageRole::User,
                MessageRole::Assistant
            ]
        );
        let texts: Vec<_> = messages.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["问题一", "回答一", "问题二", "回答二"]);
        assert_eq!(messages[0].created_ms, 1000);
        assert_eq!(messages[3].created_ms, 6000);
    }

    #[tokio::test]
    async fn tail_caps_at_four() {
        let messages = (0..10)
            .map(|i| user(&format!("u{i}"), i * 1000, &[&format!("m{i}")]))
            .collect();
        let snap = gather_from_typed_backend(typed_backend(messages)).await;
        let tail = snap.tail;
        let messages: Vec<&TailMessage> = tail.iter().filter_map(TailEntry::message).collect();
        assert_eq!(messages.len(), 4, "tail must cap at 4");
        assert_eq!(messages[0].text, "m6");
        assert_eq!(messages[3].text, "m9");
        assert_eq!(messages[3].created_ms, 9000);
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
        let message = tail[0].message().expect("a message entry");
        assert_eq!(tail.len(), 1);
        assert_eq!(message.text, "第一段\n第二段");
        assert_eq!(message.role, MessageRole::User);
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
        use crate::backend::types::{PermissionRequest, QuestionInfo, QuestionRequest};

        let mut mock = typed_backend(vec![user("msg_u1", 1000, &["你好"])]);
        mock.with_session_status("ses_adopted", Some(crate::backend::types::SessionStatus::Busy));
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
                always_supported: true,
            },
            PermissionRequest {
                request_id: "req_other".into(),
                session_id: Some("ses_sibling".into()),
                permission: Some("bash".into()),
                patterns: vec!["rm".into()],
                metadata: None,
                always: vec![],
                always_supported: true,
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
                    kind: crate::backend::types::FormFieldKind::String,
                    custom: None,
                    ..Default::default()
                }],
                ..Default::default()
            },
            QuestionRequest {
                id: "q_other".into(),
                session_id: "ses_sibling".into(),
                questions: vec![],
                ..Default::default()
            },
        ]);
        let snap = gather_from_typed_backend(mock).await;

        assert_eq!(snap.status, Some(crate::backend::types::SessionStatus::Busy));
        assert!(!snap.pending.is_empty());
        let ids: Vec<_> = snap.pending.iter().map(|p| p.id().to_string()).collect();
        assert_eq!(
            ids,
            vec!["req_adopted", "q_adopted"],
            "sibling pendings leaked in"
        );
        // The tail and newest user come from the scripted transcript, not the
        // mock's default shape.
        assert_eq!(snap.tail.len(), 1);
        assert_eq!(snap.tail[0].message().expect("a message entry").text, "你好");
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
        assert_eq!(snap.status, Some(crate::backend::types::SessionStatus::Idle));
    }

    /// #586: the Waiting fact holds only when the server reads idle AND the
    /// newest user message's Turn is complete with live Background Tasks. A
    /// running Execution, a retry, an unread status, and a newer user message
    /// whose own Turn has not finished are all "not waiting" — never guessed.
    #[tokio::test]
    async fn gather_detects_a_waiting_turn_only_on_idle_with_live_tasks() {
        let finished = || vec![user("msg_u1", 1000, &["你好"]), finished_turn("msg_a1", 2000)];
        let waiting =
            gather_from_typed_backend(typed_backend_with_tasks(finished(), vec![background_shell(1100)]))
                .await;
        assert!(waiting.waiting, "a finished turn + a live task is a Waiting turn");

        let idle = gather_from_typed_backend(typed_backend(vec![user("msg_u1", 1000, &["你好"])])).await;
        assert!(!idle.waiting, "idle with no live task is not waiting");

        let mut busy = typed_backend_with_tasks(finished(), vec![background_shell(1100)]);
        busy.with_session_status("ses_adopted", Some(crate::backend::types::SessionStatus::Busy));
        let snap = gather_from_typed_backend(busy).await;
        assert!(
            !snap.waiting,
            "a running Execution takes the busy arm, not waiting"
        );

        let mut retry = typed_backend_with_tasks(finished(), vec![background_shell(1100)]);
        retry.with_session_status("ses_adopted", Some(crate::backend::types::SessionStatus::Retry));
        let snap = gather_from_typed_backend(retry).await;
        assert!(!snap.waiting, "retry is never guessed into a wait");

        let mut unread = typed_backend_with_tasks(finished(), vec![background_shell(1100)]);
        unread.status_read_fails("simulated failure");
        let snap = gather_from_typed_backend(unread).await;
        assert_eq!(snap.status, None);
        assert!(!snap.waiting, "an unknown status is not idle");

        // A NEWER user message whose own Turn has not finished: the live task
        // belongs to the older Turn, whose missed Wake must stay with the
        // Session Sync continuation — never a follow anchored at the newer
        // message (the collect fixture this guards).
        let newer = gather_from_typed_backend(typed_backend_with_tasks(
            vec![
                user("msg_u1", 1000, &["你好"]),
                finished_turn("msg_a1", 2000),
                user("msg_u2", 3000, &["外部新消息"]),
            ],
            vec![background_shell(1100)],
        ))
        .await;
        assert!(
            !newer.waiting,
            "a newer unfinished turn is not the Waiting turn of the older one"
        );
    }

    /// ADR-0100: the confirmation is ONE line, so a server title carrying a
    /// newline (or runs of whitespace) collapses to single spaces, and the
    /// shared composition's chip rides after ` · `. The id-tail fallback is not
    /// affected.
    #[test]
    fn the_adoption_line_sanitizes_a_multi_line_title() {
        let data = SnapshotData {
            session_id: "ses_adopted".into(),
            directory: "/work".into(),
            status: Some(crate::backend::types::SessionStatus::Idle),
            waiting: false,
            pending: Vec::new(),
            tail: Vec::new(),
            newest_user_anchor: None,
            newest_user_is_cola_authored: false,
        };
        let line = adoption_line_from_data("接管", " 标题\n第二行\t尾巴 ", &data);
        assert_eq!(line, "已接管「标题 第二行 尾巴」 · ✅ 空闲");
        assert_eq!(line.lines().count(), 1, "the confirmation is ONE line: {line}");

        // A whitespace-only label trims to nothing: fall back to the id tail
        // rather than an empty pair of brackets.
        let blank = adoption_line_from_data("接管", "   ", &data);
        assert_eq!(blank, "已接管「adopted」 · ✅ 空闲");
    }

    /// The adoption line is bounded: a transcript read that never answers
    /// degrades to the title-only line instead of hanging `/switch`, `/attach`
    /// or `/topic --adopt`.
    #[tokio::test]
    async fn the_adoption_line_bounds_a_hanging_read() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        backend.hang_transcript_for("ses_test").await;
        let (app, _platform) = crate::bridge::test_support::build_app(cfg, backend).await;
        let mut handles = SnapshotHandles::from_flow(&app.flow_handles());
        handles.read_timeout_ms = 50;

        let line = adoption_line(&handles, "接管", &list_session("ses_test", "标题", "/work", 1)).await;
        assert_eq!(line, "已接管「标题」", "a timed-out gather omits the status");
    }
}
