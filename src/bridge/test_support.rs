#![cfg(test)]

pub(crate) use std::sync::Arc;

pub(crate) use crate::bridge::handler::App;
pub(crate) use crate::bridge::turn::Turn;
pub(crate) use crate::feishu;

use crate::backend::{
    BackgroundTask, ChildEvidence, Compaction, CompactionReason, CompactionStatus, ContentBlock, Execution,
    ExecutionOutcome, FinishReason, MessageId, MessageRole, MessageTime, Part, ReasoningPart,
    SessionTranscript, StepFinish, StepStart, TextPart, ToolCall, ToolIdentity, ToolOutput, ToolStatus,
    TranscriptMessage, TurnAnchor, Wake, WakeSource,
};

/// One typed transcript message for view-shaped fixtures (spec #332): identity,
/// role, server time (completed at creation) and the given parts. `created:
/// None` models a message the payload carried no time for. Shared by the
/// snapshot, external and mock-backend test surfaces so their typed fixtures
/// cannot drift.
pub(crate) fn typed_message(
    id: &str,
    role: MessageRole,
    created: Option<i64>,
    parts: Vec<Part>,
) -> TranscriptMessage {
    TranscriptMessage {
        id: MessageId::new(id),
        role,
        time: created.map(|created| MessageTime {
            created,
            completed: Some(created),
        }),
        model: None,
        tokens: None,
        error: None,
        parts,
    }
}

/// One assistant message carrying TWO text parts at the SAME server time
/// (spec #561, review #569): the shape whose merge used to lose the second
/// part's identity, and whose card must still show each part exactly once.
pub(crate) fn two_parts_at_one_time(first: &str, second: &str) -> crate::backend::TranscriptMessage {
    typed_message(
        "msg_a_2000",
        crate::backend::MessageRole::Assistant,
        Some(2_000),
        vec![
            Part::Text(crate::backend::TextPart {
                text: first.to_string(),
                started_at: Some(2_000),
            }),
            Part::Text(crate::backend::TextPart {
                text: second.to_string(),
                started_at: Some(2_000),
            }),
            Part::StepFinish(crate::backend::StepFinish {
                reason: crate::backend::FinishReason::Stop,
            }),
        ],
    )
}

/// One completed auto **Compaction** on a message of its own — the neutral typed
/// shape V2 persists (ADR-0082), with its server time on the MESSAGE (a part
/// carries none). Shared by the bridge test surfaces, so the fixture a
/// compaction-only transcript is scripted from cannot drift; a test needing a
/// running, failed or usage-carrying fact builds its own.
pub(crate) fn compaction_message(id: &str, created: i64, summary: &str) -> TranscriptMessage {
    typed_message(
        id,
        MessageRole::Other("compaction".into()),
        Some(created),
        vec![Part::Compaction(Compaction {
            status: CompactionStatus::Completed,
            reason: CompactionReason::Auto,
            summary: summary.to_string(),
            usage: None,
            error: None,
            recent: "读者已经读过的最近对话".to_string(),
        })],
    )
}

/// One typed text part — the common content of the view-shaped fixtures.
pub(crate) fn text_part(text: &str) -> Part {
    Part::Text(TextPart {
        text: text.to_string(),
        started_at: None,
    })
}

/// One typed tool part — name, correlation id, status, raw input and a single
/// text output block: the shape most fixtures need. Fixtures that exercise
/// metadata, richer output blocks or server times build the call directly.
pub(crate) fn tool_part(
    name: &str,
    call_id: &str,
    status: ToolStatus,
    input: serde_json::Value,
    output: &str,
) -> Part {
    Part::Tool(ToolCall {
        identity: ToolIdentity {
            name: name.to_string(),
            call_id: call_id.to_string(),
        },
        status,
        started_at: None,
        input: Some(input),
        metadata: None,
        output: ToolOutput {
            raw: Some(serde_json::Value::String(output.to_string())),
            blocks: vec![ContentBlock::Text(output.to_string())],
            error: None,
        },
    })
}

/// A fixture Turn anchor: a message identity together with its server time,
/// one fact (spec #332).
pub(crate) fn turn_anchor(created_ms: i64) -> TurnAnchor {
    TurnAnchor {
        message_id: MessageId::new(format!("msg_anchor_{created_ms}")),
        created_ms,
    }
}

/// One fixture Execution boundary at `ended_ms` — the durable `idle` marker of
/// a busy period (ADR-0059). Shared by every test that scripts a V2 read, so
/// the background-task/wake fixtures below cannot drift from it.
pub(crate) fn execution(ended_ms: i64) -> Execution {
    Execution {
        id: MessageId::new(format!("msg_idle_{ended_ms}")),
        ended_ms: Some(ended_ms),
        outcome: ExecutionOutcome::Succeeded,
    }
}

/// The live Background Task a backgrounded shell started at `started_at` —
/// the fixture [`shell_wake`] retires.
pub(crate) fn background_shell(started_at: i64) -> BackgroundTask {
    BackgroundTask {
        tool: ToolIdentity {
            name: "shell".into(),
            call_id: "call_bg".into(),
        },
        shell_id: Some("sh_bg".into()),
        child_id: None,
        started_at: Some(started_at),
    }
}

/// A launch as a read still carries it: the launching tool call, its
/// shell/child ids and its start clock. Shared so the Task Run Id resolution
/// tests (`bridge::tasks::model`) and the Chain Records ending tests
/// (`bridge::chain::records`) pin one fixture instead of two drifting copies.
pub(crate) fn launch_task(call_id: &str, shell_id: Option<&str>, child_id: Option<&str>) -> BackgroundTask {
    BackgroundTask {
        tool: ToolIdentity {
            name: "shell".into(),
            call_id: call_id.into(),
        },
        shell_id: shell_id.map(str::to_string),
        child_id: child_id.map(str::to_string),
        started_at: Some(1_000),
    }
}

/// The shell Wake that retires [`background_shell`], carrying the command
/// label the Wake's own text names.
pub(crate) fn shell_wake(created_ms: i64) -> Wake {
    Wake {
        id: MessageId::new(format!("msg_wake_{created_ms}")),
        created_ms: Some(created_ms),
        source: WakeSource::Shell,
        shell_id: Some("sh_bg".into()),
        job_id: Some("sh_bg".into()),
        child_id: None,
        state: Some("completed".into()),
        label: Some("gh run watch".into()),
    }
}

/// A subagent completion Wake with the task description its text names, for
/// the completion entry's title (`🔔 subagent 完成：…`).
pub(crate) fn subagent_wake(created_ms: i64, description: &str) -> Wake {
    Wake {
        id: MessageId::new(format!("msg_wake_sub_{created_ms}")),
        created_ms: Some(created_ms),
        source: WakeSource::Subagent,
        shell_id: None,
        job_id: None,
        child_id: Some("ses_child".into()),
        state: Some("completed".into()),
        label: Some(description.into()),
    }
}

/// The one user-facing marker a Wake continuation opens with (the 承接 line) —
/// what the platform-call helpers key on to recognise a continuation card.
pub(crate) const WAKE_LEAD: &str = "已恢复执行";

/// The neutral receipt the Wake-less RESIDUAL continuation opens with (spec
/// #602, ticket #606): content the Backend wrote after the run reported idle
/// renders honestly — never the Wake's 「已恢复执行」 receipt, because nothing
/// resumed. Kept in lockstep with `turn::RESIDUAL_RECEIPT` by the residual
/// tests (the production copy is the source of truth).
pub(crate) const RESIDUAL_LEAD: &str = "还有更新";

/// Every card SEND (a reply or a top-level send) carrying the Wake
/// continuation's 承接 line, in call order. A re-post would be another SEND,
/// while the render updates the one continuation card in place many times —
/// so this is the counter for "was a new continuation card posted?", never
/// `updated_cards` nor a payload scan that also sees PATCHes.
pub(crate) async fn continuation_sends(platform: &RecordingPlatform) -> Vec<serde_json::Value> {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::ReplyCard { card, .. } | PlatformCall::SendCard { card, .. }
                if card_text(card).contains(WAKE_LEAD) =>
            {
                Some(card.clone())
            }
            _ => None,
        })
        .collect()
}

/// Every card POST (a reply or a top-level send) the platform recorded — the
/// counter for "was a new card posted?" (an in-place PATCH updates one, so it
/// is never counted). Shared by the collect and Wake tests so their new-card
/// assertions read the same call set.
pub(crate) async fn card_posts(platform: &RecordingPlatform) -> usize {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter(|call| {
            matches!(
                call,
                PlatformCall::ReplyCard { .. } | PlatformCall::SendCard { .. }
            )
        })
        .count()
}

/// A recorded `reply_question` call: (request_id, answers).
type QuestionReplyRecord = (String, Vec<Vec<String>>);

/// A recorded keyed `reply_question` call: (request_id, form answers).
type KeyedQuestionReply = (String, Vec<crate::backend::types::FormAnswer>);

/// One `session_diff` read's arguments, in call order: `(session_id, from,
/// to, context)` (#801).
pub(crate) type SessionDiffCall = (String, Option<String>, Option<String>, Option<u32>);

#[derive(Debug, Clone)]
#[allow(dead_code)] // the recording adapter captures full call details for assertions
pub enum PlatformCall {
    ReplyCard {
        reply_to: String,
        card: serde_json::Value,
    },
    SendCard {
        receive_id: String,
        card: serde_json::Value,
    },
    UpdateMessage {
        message_id: String,
        card: serde_json::Value,
    },
    EditMessage {
        message_id: String,
        msg_type: String,
        content: serde_json::Value,
        outcome: crate::feishu::MessageEditOutcome,
        business_code: Option<i32>,
    },
    EditMessageFailed {
        message_id: String,
        content: serde_json::Value,
    },
    ReplyText {
        message_id: String,
        text: String,
    },
    ReplyInThread {
        message_id: String,
        text: String,
        thread_id: Option<String>,
    },
    ReplyCardInThread {
        message_id: String,
        card: serde_json::Value,
        thread_id: Option<String>,
    },
    CompletionNotice {
        reply_to: String,
        open_id: String,
        name: Option<String>,
        text: String,
    },
    /// An Instant Reminder (`time_sensitive`, ADR-0043) call: the pinned
    /// Chat/Topic, its kind, the targeted users and whether it was pinned
    /// (`true`) or cleared (`false`).
    InstantReminder {
        chat_id: String,
        is_group: bool,
        user_ids: Vec<String>,
        on: bool,
    },
    /// A waiting-message pin call (ADR-0043 amendment): the message put into
    /// (or removed from) its Chat/Topic's pinned-message list.
    PinMessage {
        message_id: String,
        on: bool,
    },
    /// An image upload (ADR-0076): the File Content's mime and decoded length,
    /// recorded so a test can assert how many uploads a turn made.
    UploadImage {
        mime: String,
        size: u64,
    },
    /// A file upload (ADR-0076, #649): the File Content's name, mime, decoded
    /// length and bytes, recorded so a test can assert how many file uploads a
    /// turn made — and inspect the exact artifact it delivered (#801).
    UploadFile {
        name: String,
        mime: String,
        size: u64,
        content: Vec<u8>,
    },
    /// A generic reply message send (ADR-0076, #649): the live card it replied
    /// under and the `msg_type`/`content` it carried — the File Message path
    /// records `msg_type:"file"` with `{"file_key": …}`. The reply omits
    /// `reply_in_thread`, so Feishu keeps it in the card's own container
    /// (ADR-0076's amendment), which is why no mode is recorded.
    SendMessageReply {
        message_id: String,
        msg_type: String,
        content: serde_json::Value,
    },
}

/// A one-shot gate on one platform call, so a test can freeze a card write
/// mid-send and interleave another writer (the concurrency races ADR-0038's
/// "one writer per card" invariant depends on).
pub struct CallGate {
    /// `"update"` (= `update_message`), `"reply"` (= `reply_card`) or
    /// `"card_view"` (= `get_card_view`).
    pub method: &'static str,
    /// The message the gated call targets (`message_id` / `reply_to`).
    pub target: String,
    /// Signalled when the gated call has been entered (the caller is parked).
    pub entered: Arc<tokio::sync::Notify>,
    /// The parked call proceeds once this is signalled.
    pub release: Arc<tokio::sync::Notify>,
    /// Whether the test models Feishu accepting a message before the response
    /// is held, making a timeout explicitly ambiguous.
    pub accepted_before_reply: bool,
}

/// A scripted `reply_card` outcome (spec #561, review #569).
#[derive(Clone, Copy, Debug)]
pub enum ReplyOutcome {
    /// The send lands (the next scripted id, or the mock's default).
    Lands,
    /// An ambiguous transport failure: the create may have landed.
    Ambiguous,
    /// A definite card-content rejection (`230099`): no message was created.
    Rejected,
    /// A definite HTTP refusal with this status (a 4xx): no message was
    /// created.
    Refused(u16),
}

/// Records every card cola would send, instead of posting to Feishu.
pub struct RecordingPlatform {
    pub calls: Arc<tokio::sync::Mutex<Vec<PlatformCall>>>,
    /// open_id → display name served by `user_name` (empty = lookup fails).
    pub user_names: std::collections::HashMap<String, String>,
    /// chat_id → display name served by `chat_name` (absent = None).
    pub chat_names: std::collections::HashMap<String, String>,
    /// When true, `send_card` fails (tests the topic-cover fallback path).
    pub fail_send_card: bool,
    /// When true, `reply_card` fails (tests the restart-announce fallback).
    pub fail_reply_card: bool,
    /// The next N `reply_card` calls fail (a CONTINUATION send that must be
    /// retried without duplicating receipts, while the loading card's own
    /// reply — if any — succeeds).
    pub fail_reply_card_count: std::sync::atomic::AtomicUsize,
    /// The next N `update_message` calls fail with a card-content rejection
    /// (`230099`, what Feishu returns for a card whose markdown it refuses):
    /// the flush must degrade the card and retry instead of resending it.
    pub fail_update_card_content_count: std::sync::atomic::AtomicUsize,
    /// The next N `update_message` calls fail with a plain Feishu error: the
    /// caller must only warn (the retry's `Retried` marker PATCH must never
    /// block the retry itself, spec #391).
    pub fail_update_count: std::sync::atomic::AtomicUsize,
    /// The next N `update_message` calls report Feishu's message-gone error
    /// (`230002`, delivered inside a 2xx body as an API code) — the permanent
    /// shape a caller neutralizing a card must stop retrying.
    pub fail_update_message_gone_count: std::sync::atomic::AtomicUsize,
    /// The next N `update_message` calls fail with a bare HTTP 404. Unlike the
    /// `230002` shape this says nothing about the message (a proxy, an endpoint
    /// outage), so a caller must keep its retry state.
    pub fail_update_http_404_count: std::sync::atomic::AtomicUsize,
    /// The next N `update_message` calls fail with a transport-class error
    /// (`BridgeError::Io`, the shape a timeout or dropped connection surfaces
    /// as) instead of an API response. An ambiguous failure: the PATCH may
    /// have landed, so callers must not treat it as a definite card rejection.
    pub fail_update_transport_count: std::sync::atomic::AtomicUsize,
    /// `(needle, remaining)`: the next N `update_message` calls whose card JSON
    /// contains `needle` fail before landing. The live patch loop rewrites its
    /// card on its own cadence, so a global counter cannot aim at one write under
    /// test (a pull's freeze, say); a content match can.
    pub fail_update_containing: std::sync::Mutex<Vec<(String, usize)>>,
    /// The next N `reply_card` calls fail with the same typed rejection, for
    /// the continuation-send recovery path.
    pub fail_reply_card_content_count: std::sync::atomic::AtomicUsize,
    /// The next N `reply_card` calls record the send — as if Feishu accepted
    /// it — and then return a transport-class error: the "accepted remotely,
    /// response lost" shape a single-shot create must never repeat (spec #561,
    /// review #569).
    pub fail_reply_card_after_send_count: std::sync::atomic::AtomicUsize,
    /// Scripted `reply_card` outcomes, popped in call order; empty means every
    /// send lands. Lets a test drive a projection create's exact failure class
    /// (spec #561, review #569) or stop its chain at a known slice.
    pub reply_card_outcomes: std::sync::Mutex<std::collections::VecDeque<ReplyOutcome>>,
    /// When set, `set_instant_reminder` fails after recording the attempt
    /// (tests the best-effort pin path: failures log and never affect a turn).
    /// Atomic so a test can flip it mid-lifecycle and watch a recovery.
    pub fail_instant_reminder: std::sync::atomic::AtomicBool,
    /// When set, `pin_message`/`unpin_message` fail after recording the
    /// attempt (tests the best-effort waiting-card pin path: a failed pin is
    /// retried, a failed unpin stays tracked). Atomic so a test can flip it
    /// mid-lifecycle.
    pub fail_pin: std::sync::atomic::AtomicBool,
    /// The thread_id `reply_in_thread` returns; `None` simulates a chat
    /// without topic support (the create-topic surfaces degrade with a
    /// message instead of mapping).
    pub reply_in_thread_thread_id: Option<String>,
    /// message_id → quoted-parent content served by `get_message` (absent =
    /// the default text parent). Lets tests script quote-injection cases.
    pub quoted_messages:
        std::sync::Mutex<std::collections::HashMap<String, crate::feishu::client::FeishuMessage>>,
    /// message_id → the card view served by `get_card_view`: the schema-2.0
    /// JSON a real whole-card read returns. Absent = the read fails (like a
    /// missing permission), which is what every reap test that expects the
    /// bare ending relies on.
    pub card_views: std::sync::Mutex<std::collections::HashMap<String, serde_json::Value>>,
    /// Every `get_card_view` call's target, in call order, recorded on entry
    /// (a parked read still counts). The reap's stamp tests count the reads a
    /// settled key must never repeat (spec #571, ticket #575).
    pub card_view_reads: std::sync::Mutex<Vec<String>>,
    /// One-shot mid-send pause installed by a concurrency test (absent in
    /// every other test). Taken by the first matching call.
    pub pause_call: std::sync::Mutex<Option<CallGate>>,
    /// Scripted `reply_card` return ids, popped in call order; none left means
    /// the mock's default `msg_reply`. A concurrency test that must tell two
    /// racing creates apart (the Turn's card vs. a projection's late one) names
    /// each reply itself.
    pub reply_ids: std::sync::Mutex<std::collections::VecDeque<String>>,
    /// How long a writer awaits an issued keyed submission's ticket (spec #571
    /// review): production's delivery bound by default, shortened by a test
    /// that parks a keyed write and must watch a caller give up on it.
    pub keyed_ticket_await: std::sync::Mutex<std::time::Duration>,
    /// The `image_key` `upload_image` returns (ADR-0076).
    pub image_key: std::sync::Mutex<String>,
    /// The next N `upload_image` calls fail with a Feishu error after recording
    /// nothing (tests the best-effort path: a failed upload leaves the card
    /// intact and never fails the Turn).
    pub fail_upload_image_count: std::sync::atomic::AtomicUsize,
    /// Every `upload_image` entry — including a non-embeddable content the mock
    /// answers `Ok(None)` for — so a test can assert the bridge asks the
    /// platform at most once for a content it has already decided.
    pub upload_image_calls: std::sync::atomic::AtomicUsize,
    /// The `file_key` `upload_file` returns (ADR-0076, #649).
    pub file_key: std::sync::Mutex<String>,
    /// The next N `upload_file` calls fail with a Feishu error after recording
    /// nothing (tests the best-effort path: a failed file upload leaves the
    /// card intact and never fails the Turn).
    pub fail_upload_file_count: std::sync::atomic::AtomicUsize,
    /// The next N `reply_message` calls fail with a Feishu error (tests that a
    /// File Message failure degrades to `未发送` and never blocks the card).
    pub fail_send_message_count: std::sync::atomic::AtomicUsize,
    /// The next N `reply_message` calls fail ambiguously after the request
    /// went out (a 5xx/transport-shaped error): the send may have landed, so a
    /// caller must treat the outcome as unknown rather than retry.
    pub ambiguous_send_message_count: std::sync::atomic::AtomicUsize,
    /// The next N non-card message edits are definitively refused by Feishu.
    pub refuse_edit_message_count: std::sync::atomic::AtomicUsize,
    /// The next N non-card edits simulate Feishu's 230020 frequency-limit refusal.
    pub reject_edit_message_count: std::sync::atomic::AtomicUsize,
    /// The next N non-card message edits fail ambiguously at transport.
    pub fail_edit_message_count: std::sync::atomic::AtomicUsize,
    /// Scripted ids returned by `reply_message`; defaults to `msg_file`.
    pub message_reply_ids: std::sync::Mutex<std::collections::VecDeque<String>>,
    /// file_key → the payload `download_file` serves. A present `Some` is
    /// served; a present `None` fails the download (the 下载失败 omission); an
    /// absent key serves a small PDF with the caller's name.
    pub downloaded_files:
        std::sync::Mutex<std::collections::HashMap<String, Option<crate::feishu::client::FileAttachment>>>,
    /// Every `download_file` call `(message_id, file_key, name)`, in order.
    pub download_file_calls: std::sync::Mutex<Vec<(String, String, String)>>,
    /// When set, `download_file` sleeps this long before answering — a slow
    /// parent download for the Quoted Context budget tests.
    pub download_file_delay: std::sync::Mutex<Option<std::time::Duration>>,
}

impl RecordingPlatform {
    pub fn new() -> Self {
        Self {
            calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            user_names: std::collections::HashMap::new(),
            chat_names: std::collections::HashMap::new(),
            fail_send_card: false,
            fail_reply_card: false,
            fail_reply_card_count: std::sync::atomic::AtomicUsize::new(0),
            fail_update_card_content_count: std::sync::atomic::AtomicUsize::new(0),
            fail_update_count: std::sync::atomic::AtomicUsize::new(0),
            fail_update_message_gone_count: std::sync::atomic::AtomicUsize::new(0),
            fail_update_http_404_count: std::sync::atomic::AtomicUsize::new(0),
            fail_update_transport_count: std::sync::atomic::AtomicUsize::new(0),
            fail_update_containing: std::sync::Mutex::new(Vec::new()),
            fail_reply_card_content_count: std::sync::atomic::AtomicUsize::new(0),
            fail_reply_card_after_send_count: std::sync::atomic::AtomicUsize::new(0),
            reply_card_outcomes: std::sync::Mutex::new(std::collections::VecDeque::new()),
            fail_instant_reminder: std::sync::atomic::AtomicBool::new(false),
            fail_pin: std::sync::atomic::AtomicBool::new(false),
            reply_in_thread_thread_id: Some("omt_created_topic".into()),
            quoted_messages: std::sync::Mutex::new(std::collections::HashMap::new()),
            card_views: std::sync::Mutex::new(std::collections::HashMap::new()),
            card_view_reads: std::sync::Mutex::new(Vec::new()),
            pause_call: std::sync::Mutex::new(None),
            reply_ids: std::sync::Mutex::new(std::collections::VecDeque::new()),
            keyed_ticket_await: std::sync::Mutex::new(crate::feishu::delivery::KEYED_TICKET_AWAIT),
            image_key: std::sync::Mutex::new("img_test".into()),
            fail_upload_image_count: std::sync::atomic::AtomicUsize::new(0),
            upload_image_calls: std::sync::atomic::AtomicUsize::new(0),
            file_key: std::sync::Mutex::new("file_test".into()),
            fail_upload_file_count: std::sync::atomic::AtomicUsize::new(0),
            fail_send_message_count: std::sync::atomic::AtomicUsize::new(0),
            ambiguous_send_message_count: std::sync::atomic::AtomicUsize::new(0),
            refuse_edit_message_count: std::sync::atomic::AtomicUsize::new(0),
            reject_edit_message_count: std::sync::atomic::AtomicUsize::new(0),
            fail_edit_message_count: std::sync::atomic::AtomicUsize::new(0),
            message_reply_ids: std::sync::Mutex::new(std::collections::VecDeque::new()),
            downloaded_files: std::sync::Mutex::new(std::collections::HashMap::new()),
            download_file_calls: std::sync::Mutex::new(Vec::new()),
            download_file_delay: std::sync::Mutex::new(None),
        }
    }

    /// Shorten the bound a writer awaits an issued keyed submission's ticket
    /// (spec #571 review), so a test can park a keyed write and watch its
    /// caller proceed without a verdict.
    pub fn given_keyed_ticket_await(&self, bound: std::time::Duration) {
        *self.keyed_ticket_await.lock().unwrap() = bound;
    }

    /// Queue the id the next `reply_card` call returns (call order); a call
    /// with none queued keeps the mock's default `msg_reply`.
    pub fn given_reply_id(&self, id: &str) {
        self.reply_ids.lock().unwrap().push_back(id.to_string());
    }

    /// Queue the id the next `reply_message` call returns.
    pub fn given_message_reply_id(&self, id: &str) {
        self.message_reply_ids.lock().unwrap().push_back(id.to_string());
    }

    /// Set the `image_key` every `upload_image` returns (ADR-0076).
    pub fn given_image_key(&self, key: &str) {
        *self.image_key.lock().unwrap() = key.to_string();
    }

    /// Set the `file_key` every `upload_file` returns (ADR-0076, #649).
    pub fn given_file_key(&self, key: &str) {
        *self.file_key.lock().unwrap() = key.to_string();
    }

    /// Queue the next `reply_card` call's outcome; a failed send is not
    /// recorded (it never reached Feishu).
    pub fn given_reply_card_outcome(&self, outcome: ReplyOutcome) {
        self.reply_card_outcomes.lock().unwrap().push_back(outcome);
    }

    /// Fail the next `count` `update_message` calls whose card JSON contains
    /// `needle`, leaving every other card's writes alone.
    pub fn fail_updates_containing(&self, needle: &str, count: usize) {
        self.fail_update_containing
            .lock()
            .unwrap()
            .push((needle.to_string(), count));
    }

    /// Park the first `method` call targeting `target` until `release`, after
    /// signalling `entered`. Returns `(entered, release)` for the test to
    /// await and trigger.
    pub fn pause(
        &self,
        method: &'static str,
        target: &str,
    ) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        *self.pause_call.lock().unwrap() = Some(CallGate {
            method,
            target: target.to_string(),
            entered: entered.clone(),
            release: release.clone(),
            accepted_before_reply: false,
        });
        (entered, release)
    }

    /// Park the first matching reply after recording it as accepted, so a
    /// caller timing out observes an accepted-but-unacknowledged send.
    pub fn pause_after_accept(
        &self,
        method: &'static str,
        target: &str,
    ) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        *self.pause_call.lock().unwrap() = Some(CallGate {
            method,
            target: target.to_string(),
            entered: entered.clone(),
            release: release.clone(),
            accepted_before_reply: true,
        });
        (entered, release)
    }

    /// Script the card view `get_card_view` serves for `message_id`: the
    /// schema-2.0 JSON a real whole-card read returns. An unscripted message
    /// fails the read, exactly like a missing `im:message` grant.
    pub fn given_card_view(&self, message_id: &str, view: serde_json::Value) {
        self.card_views
            .lock()
            .unwrap()
            .insert(message_id.to_string(), view);
    }

    /// Take the installed gate when this call matches it.
    fn take_gate(&self, method: &str, target: &str) -> Option<CallGate> {
        let mut slot = self.pause_call.lock().unwrap();
        match slot.as_ref() {
            Some(gate) if gate.method == method && gate.target == target => slot.take(),
            _ => None,
        }
    }

    /// Every card the app replied to a message with, in call order (C4 query
    /// helper — no fluent DSL, just the structured payload).
    pub(crate) async fn replied_cards(&self) -> Vec<serde_json::Value> {
        self.cards_of(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card),
            _ => None,
        })
        .await
    }

    /// Every card replied into a thread — the shape a continuation uses when
    /// the conversation is a Topic. Kept separate from [`Self::replied_cards`]
    /// because its call carries a thread id too.
    pub(crate) async fn replied_in_thread_cards(&self) -> Vec<serde_json::Value> {
        self.cards_of(|c| match c {
            PlatformCall::ReplyCardInThread { card, .. } => Some(card),
            _ => None,
        })
        .await
    }

    /// Every replied card paired with the message id it replied under, in call
    /// order (asserts a reply lands under the right message).
    pub(crate) async fn replied_cards_with_targets(&self) -> Vec<(String, serde_json::Value)> {
        self.calls
            .lock()
            .await
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyCard { reply_to, card } => Some((reply_to.clone(), card.clone())),
                _ => None,
            })
            .collect()
    }

    /// Every card the app sent to a chat.
    pub(crate) async fn sent_cards(&self) -> Vec<serde_json::Value> {
        self.cards_of(|c| match c {
            PlatformCall::SendCard { card, .. } => Some(card),
            _ => None,
        })
        .await
    }

    /// Every in-place card update (the streaming card's flushes).
    pub(crate) async fn updated_cards(&self) -> Vec<serde_json::Value> {
        self.cards_of(|c| match c {
            PlatformCall::UpdateMessage { card, .. } => Some(card),
            _ => None,
        })
        .await
    }

    /// Every plain text reply.
    pub(crate) async fn texts(&self) -> Vec<String> {
        self.calls
            .lock()
            .await
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Every Instant Reminder call in order: `(chat_id, is_group, user_ids,
    /// on)` — `on = true` is the pin, `false` the clear (ADR-0043).
    pub(crate) async fn reminders(&self) -> Vec<(String, bool, Vec<String>, bool)> {
        self.calls
            .lock()
            .await
            .iter()
            .filter_map(|c| match c {
                PlatformCall::InstantReminder {
                    chat_id,
                    is_group,
                    user_ids,
                    on,
                } => Some((chat_id.clone(), *is_group, user_ids.clone(), *on)),
                _ => None,
            })
            .collect()
    }

    /// Every Completion Notice call in order: `(reply_to, open_id, name, text)`
    /// — the reply that notifies the requester a Turn ended (ADR-0043
    /// amendment 2026-09-21).
    pub(crate) async fn completion_notices(&self) -> Vec<(String, String, Option<String>, String)> {
        self.calls
            .lock()
            .await
            .iter()
            .filter_map(|c| match c {
                PlatformCall::CompletionNotice {
                    reply_to,
                    open_id,
                    name,
                    text,
                } => Some((reply_to.clone(), open_id.clone(), name.clone(), text.clone())),
                _ => None,
            })
            .collect()
    }

    /// Every waiting-message pin call in order: `(message_id, on)` — `on`
    /// `true` puts the message into its Chat/Topic's pinned-message list,
    /// `false` removes it (ADR-0043 amendment).
    pub(crate) async fn message_pins(&self) -> Vec<(String, bool)> {
        self.calls
            .lock()
            .await
            .iter()
            .filter_map(|c| match c {
                PlatformCall::PinMessage { message_id, on } => Some((message_id.clone(), *on)),
                _ => None,
            })
            .collect()
    }

    /// Every button `value` payload on every card the platform saw (replied,
    /// sent, in-thread or updated), in card walk order.
    pub(crate) async fn button_values(&self) -> Vec<serde_json::Value> {
        let calls = self.calls.lock().await;
        let mut out = Vec::new();
        for call in calls.iter() {
            match call {
                PlatformCall::ReplyCard { card, .. }
                | PlatformCall::SendCard { card, .. }
                | PlatformCall::UpdateMessage { card, .. }
                | PlatformCall::ReplyCardInThread { card, .. } => {
                    collect_button_values(card, &mut out);
                }
                _ => {}
            }
        }
        out
    }

    async fn cards_of(
        &self,
        pick: impl Fn(&PlatformCall) -> Option<&serde_json::Value>,
    ) -> Vec<serde_json::Value> {
        self.calls.lock().await.iter().filter_map(pick).cloned().collect()
    }
}

/// Park on `gate` until the test releases it.
async fn wait_gate(gate: CallGate) {
    gate.entered.notify_one();
    gate.release.notified().await;
}

/// Walk a card and collect every button's `value` payload (buttons nest in
/// column sets and action blocks, so the walk is recursive).
fn collect_button_values(value: &serde_json::Value, out: &mut Vec<serde_json::Value>) {
    match value {
        serde_json::Value::Object(map) => {
            if map.get("tag").and_then(|t| t.as_str()) == Some("button")
                && let Some(value) = map.get("value")
            {
                out.push(value.clone());
            }
            for v in map.values() {
                collect_button_values(v, out);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                collect_button_values(v, out);
            }
        }
        _ => {}
    }
}

#[async_trait::async_trait]
impl feishu::Platform for RecordingPlatform {
    async fn get_ws_endpoint(&self) -> crate::error::Result<String> {
        Ok("wss://example.test".into())
    }

    fn keyed_ticket_await(&self) -> std::time::Duration {
        *self.keyed_ticket_await.lock().unwrap()
    }

    async fn reply_card(&self, reply_to: &str, card: &serde_json::Value) -> crate::error::Result<String> {
        if self
            .fail_reply_card_content_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            self.fail_reply_card_content_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            self.calls.lock().await.push(PlatformCall::ReplyCard {
                reply_to: reply_to.into(),
                card: card.clone(),
            });
            return Err(crate::error::BridgeError::CardContentRejected {
                code: 230099,
                detail: "simulated card content rejection".into(),
            });
        }
        if self.fail_reply_card {
            return Err(crate::error::BridgeError::Feishu(
                "simulated reply_card failure".into(),
            ));
        }
        if self
            .fail_reply_card_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            self.fail_reply_card_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(crate::error::BridgeError::Feishu(
                "simulated reply_card failure".into(),
            ));
        }
        // The scripted id is consumed when the call is MADE, before any gate
        // parks it: a create parked in flight must not let the next reply take
        // its identity.
        let reply_id = self
            .reply_ids
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "msg_reply".into());
        // The "accepted remotely, response lost" shape: the send is recorded,
        // then a transport error is returned (spec #561, review #569).
        if self
            .fail_reply_card_after_send_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            self.fail_reply_card_after_send_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            self.calls.lock().await.push(PlatformCall::ReplyCard {
                reply_to: reply_to.into(),
                card: card.clone(),
            });
            return Err(crate::error::BridgeError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "simulated accepted-but-lost reply_card response",
            )));
        }
        // A scripted outcome drives the create's failure class (or lands it):
        // a failed send is not recorded — it never reached Feishu.
        match self.reply_card_outcomes.lock().unwrap().pop_front() {
            None | Some(ReplyOutcome::Lands) => {}
            Some(ReplyOutcome::Ambiguous) => {
                return Err(crate::error::BridgeError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "simulated reply_card failure",
                )));
            }
            Some(ReplyOutcome::Rejected) => {
                return Err(crate::error::BridgeError::CardContentRejected {
                    code: 230099,
                    detail: "simulated card content rejection".into(),
                });
            }
            Some(ReplyOutcome::Refused(status)) => {
                return Err(crate::error::BridgeError::FeishuHttp {
                    status,
                    detail: "simulated reply_card refusal".into(),
                });
            }
        }
        if let Some(gate) = self.take_gate("reply", reply_to) {
            wait_gate(gate).await;
        }
        self.calls.lock().await.push(PlatformCall::ReplyCard {
            reply_to: reply_to.into(),
            card: card.clone(),
        });
        Ok(reply_id)
    }

    async fn send_card(
        &self,
        _receive_id_type: &str,
        receive_id: &str,
        card: &serde_json::Value,
    ) -> crate::error::Result<String> {
        if self.fail_send_card {
            return Err(crate::error::BridgeError::Feishu(
                "simulated send_card failure".into(),
            ));
        }
        self.calls.lock().await.push(PlatformCall::SendCard {
            receive_id: receive_id.into(),
            card: card.clone(),
        });
        Ok("msg_sent".into())
    }

    async fn update_message(&self, message_id: &str, card: &serde_json::Value) -> crate::error::Result<()> {
        // A per-content failure the caller can aim: the live patch loop writes
        // the same card on its own cadence, so a global counter would be eaten
        // by it before the write under test.
        let targeted = {
            let mut failing = self.fail_update_containing.lock().unwrap();
            let card_json = card.to_string();
            match failing
                .iter_mut()
                .find(|(needle, remaining)| *remaining > 0 && card_json.contains(needle.as_str()))
            {
                Some((_, remaining)) => {
                    *remaining -= 1;
                    true
                }
                None => false,
            }
        };
        let rejected = self
            .fail_update_card_content_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0;
        if rejected {
            self.fail_update_card_content_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        let failed = self.fail_update_count.load(std::sync::atomic::Ordering::SeqCst) > 0;
        if failed {
            self.fail_update_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        let transport_failed = self
            .fail_update_transport_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0;
        if transport_failed {
            self.fail_update_transport_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        let message_gone = self
            .fail_update_message_gone_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0;
        if message_gone {
            self.fail_update_message_gone_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        let http_404 = self
            .fail_update_http_404_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0;
        if http_404 {
            self.fail_update_http_404_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        if let Some(gate) = self.take_gate("update", message_id) {
            wait_gate(gate).await;
        }
        self.calls.lock().await.push(PlatformCall::UpdateMessage {
            message_id: message_id.into(),
            card: card.clone(),
        });
        if targeted {
            return Err(crate::error::BridgeError::Feishu(
                "simulated card update failure".into(),
            ));
        }
        if rejected {
            return Err(crate::error::BridgeError::CardContentRejected {
                code: 230099,
                detail: "simulated card content rejection".into(),
            });
        }
        if failed {
            return Err(crate::error::BridgeError::Feishu(
                "simulated card update failure".into(),
            ));
        }
        if transport_failed {
            return Err(crate::error::BridgeError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "simulated update transport failure",
            )));
        }
        if message_gone {
            return Err(crate::error::BridgeError::Feishu(
                "update error 230002: message not found".into(),
            ));
        }
        if http_404 {
            return Err(crate::error::BridgeError::FeishuHttp {
                status: 404,
                detail: "simulated bare 404".into(),
            });
        }
        Ok(())
    }

    async fn edit_message(
        &self,
        message_id: &str,
        msg_type: &str,
        content: &serde_json::Value,
    ) -> crate::error::Result<crate::feishu::MessageEditOutcome> {
        let failed = self
            .fail_edit_message_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0;
        if failed {
            self.fail_edit_message_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            self.calls.lock().await.push(PlatformCall::EditMessageFailed {
                message_id: message_id.into(),
                content: content.clone(),
            });
            return Err(crate::error::BridgeError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "simulated ambiguous message edit failure",
            )));
        }
        let limit_reached = self
            .refuse_edit_message_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0;
        if limit_reached {
            self.refuse_edit_message_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        let rejected = self
            .reject_edit_message_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0;
        if rejected {
            self.reject_edit_message_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        let (outcome, business_code) = if limit_reached {
            (crate::feishu::MessageEditOutcome::EditLimitReached, Some(230072))
        } else if rejected {
            (crate::feishu::MessageEditOutcome::Refused, Some(230020))
        } else {
            (crate::feishu::MessageEditOutcome::Updated, None)
        };
        self.calls.lock().await.push(PlatformCall::EditMessage {
            message_id: message_id.into(),
            msg_type: msg_type.into(),
            content: content.clone(),
            outcome,
            business_code,
        });
        Ok(outcome)
    }

    async fn reply_text(&self, message_id: &str, text: &str) -> crate::error::Result<String> {
        self.calls.lock().await.push(PlatformCall::ReplyText {
            message_id: message_id.into(),
            text: text.into(),
        });
        Ok("msg_text".into())
    }

    async fn reply_in_thread(
        &self,
        message_id: &str,
        text: &str,
    ) -> crate::error::Result<(String, Option<String>)> {
        let thread_id = self.reply_in_thread_thread_id.clone();
        self.calls.lock().await.push(PlatformCall::ReplyInThread {
            message_id: message_id.into(),
            text: text.into(),
            thread_id: thread_id.clone(),
        });
        // The mock's created topic-reply message id becomes the anchor.
        Ok(("msg_topic_reply".into(), thread_id))
    }

    async fn reply_card_in_thread(
        &self,
        message_id: &str,
        card: &serde_json::Value,
    ) -> crate::error::Result<(String, Option<String>)> {
        let thread_id = self.reply_in_thread_thread_id.clone();
        self.calls.lock().await.push(PlatformCall::ReplyCardInThread {
            message_id: message_id.into(),
            card: card.clone(),
            thread_id: thread_id.clone(),
        });
        // The mock's created topic-reply message id becomes the anchor (same
        // id as the text reply_in_thread, so topic-adopt tests asserting the
        // anchor id pass for both seed kinds).
        Ok(("msg_topic_reply".into(), thread_id))
    }

    async fn reply_completion_notice(
        &self,
        message_id: &str,
        open_id: &str,
        name: Option<&str>,
        text: &str,
    ) -> crate::error::Result<String> {
        self.calls.lock().await.push(PlatformCall::CompletionNotice {
            reply_to: message_id.into(),
            open_id: open_id.into(),
            name: name.map(|s| s.to_string()),
            text: text.into(),
        });
        Ok("msg_notice".into())
    }

    async fn set_instant_reminder(
        &self,
        chat_id: &str,
        is_group: bool,
        user_ids: &[String],
        on: bool,
    ) -> crate::error::Result<()> {
        // Recorded even when failing, so a test can assert the attempt was
        // made and the turn still proceeded.
        self.calls.lock().await.push(PlatformCall::InstantReminder {
            chat_id: chat_id.into(),
            is_group,
            user_ids: user_ids.to_vec(),
            on,
        });
        if self
            .fail_instant_reminder
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(crate::error::BridgeError::Feishu(
                "simulated set_instant_reminder failure".into(),
            ));
        }
        Ok(())
    }

    async fn pin_message(&self, message_id: &str) -> crate::error::Result<()> {
        self.calls.lock().await.push(PlatformCall::PinMessage {
            message_id: message_id.into(),
            on: true,
        });
        if self.fail_pin.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(crate::error::BridgeError::Feishu(
                "simulated pin_message failure".into(),
            ));
        }
        Ok(())
    }

    async fn unpin_message(&self, message_id: &str) -> crate::error::Result<()> {
        self.calls.lock().await.push(PlatformCall::PinMessage {
            message_id: message_id.into(),
            on: false,
        });
        if self.fail_pin.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(crate::error::BridgeError::Feishu(
                "simulated unpin_message failure".into(),
            ));
        }
        Ok(())
    }

    async fn user_name(&self, open_id: &str) -> crate::error::Result<Option<String>> {
        Ok(self.user_names.get(open_id).cloned())
    }

    async fn chat_name(&self, chat_id: &str) -> crate::error::Result<Option<String>> {
        Ok(self.chat_names.get(chat_id).cloned())
    }

    async fn bot_open_id(&self) -> crate::error::Result<String> {
        Ok("ou_test_bot".into())
    }

    async fn list_messages(
        &self,
        _container_id_type: &str,
        _container_id: &str,
    ) -> crate::error::Result<Vec<crate::feishu::client::ChatMessage>> {
        Ok(vec![crate::feishu::client::ChatMessage {
            message_id: "msg_in_topic_anchor".into(),
            msg_type: "interactive".into(),
            create_time: "0".into(),
            chat_id: "chat_1".into(),
            sender: Some(crate::feishu::client::ChatMessageSender {
                id: Some("cli_bot".into()),
                sender_type: Some("app".into()),
            }),
            body: None,
        }])
    }

    async fn get_message(
        &self,
        message_id: &str,
    ) -> crate::error::Result<crate::feishu::client::FeishuMessage> {
        // The mock serves parents via `quoted_messages`; a missing entry is
        // a hard failure (like a real `im:message` permission error) so
        // degradation paths are testable.
        self.quoted_messages
            .lock()
            .unwrap()
            .get(message_id)
            .cloned()
            .ok_or_else(|| crate::error::BridgeError::Feishu(format!("mock: no quoted message {message_id}")))
    }

    async fn get_card_view(&self, message_id: &str) -> crate::error::Result<serde_json::Value> {
        self.card_view_reads.lock().unwrap().push(message_id.to_string());
        if let Some(gate) = self.take_gate("card_view", message_id) {
            wait_gate(gate).await;
        }
        // Scripted via `given_card_view`; a missing entry is a hard failure
        // (like a real read of a deleted or non-card message), so the reap's
        // bare-ending fallback is what every unscripted test exercises.
        self.card_views
            .lock()
            .unwrap()
            .get(message_id)
            .cloned()
            .ok_or_else(|| crate::error::BridgeError::Feishu(format!("mock: no card view {message_id}")))
    }

    async fn download_image(
        &self,
        _message_id: &str,
        _image_key: &str,
    ) -> crate::error::Result<crate::feishu::client::ImageAttachment> {
        Ok(crate::feishu::client::ImageAttachment {
            mime: "image/png".into(),
            data: vec![1, 2, 3, 4],
        })
    }

    async fn download_file(
        &self,
        message_id: &str,
        file_key: &str,
        name: &str,
    ) -> crate::error::Result<crate::feishu::client::FileAttachment> {
        self.download_file_calls.lock().unwrap().push((
            message_id.to_string(),
            file_key.to_string(),
            name.to_string(),
        ));
        let delay = *self.download_file_delay.lock().unwrap();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        // Scripted per file_key: a present `Some` is served, a present `None`
        // fails (the 下载失败 omission). Absent = a small PDF with the caller's
        // name, so an unscripted quoted-file parent still attaches.
        if let Some(scripted) = self.downloaded_files.lock().unwrap().get(file_key) {
            return match scripted {
                Some(file) => Ok(file.clone()),
                None => Err(crate::error::BridgeError::Feishu(format!(
                    "mock: download file {file_key} failed"
                ))),
            };
        }
        Ok(crate::feishu::client::FileAttachment {
            mime: "application/pdf".into(),
            name: name.to_string(),
            data: b"%PDF-1.7".to_vec(),
        })
    }

    async fn upload_image(
        &self,
        content: &crate::backend::FileContent,
    ) -> crate::error::Result<Option<String>> {
        self.upload_image_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Embeddability is the same production caps check the real platform
        // applies, so a bridge test's routing (an image embeds, a PDF does not)
        // is the real decision, not a mock's shortcut.
        if !crate::feishu::image::embeddable_image(content) {
            return Ok(None);
        }
        // A test may park the upload (single-flight tests): signal entry, then
        // hold until released, so a concurrent resolver is forced to await it.
        // Taken BEFORE the scripted failure so a test can park a call that then
        // fails — the failure/waiter race the settle semantics pin (B1).
        if let Some(gate) = self.take_gate("upload_image", "") {
            wait_gate(gate).await;
        }
        if self
            .fail_upload_image_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            self.fail_upload_image_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(crate::error::BridgeError::Feishu(
                "simulated upload_image failure".into(),
            ));
        }
        self.calls.lock().await.push(PlatformCall::UploadImage {
            mime: content.mime.clone(),
            size: content.size,
        });
        Ok(Some(self.image_key.lock().unwrap().clone()))
    }

    async fn upload_file(
        &self,
        content: &crate::backend::FileContent,
    ) -> crate::error::Result<Option<String>> {
        // The same production cap the real platform applies, so a bridge test's
        // routing (a PDF delivers, a >30MB file does not) is the real decision.
        if !crate::feishu::file::deliverable_file(content) {
            return Ok(None);
        }
        if self
            .fail_upload_file_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            self.fail_upload_file_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(crate::error::BridgeError::Feishu(
                "simulated upload_file failure".into(),
            ));
        }
        // A test may park the upload (the stale-panel test): signal entry, then
        // hold until released, so the panel can be replaced mid-upload.
        if let Some(gate) = self.take_gate("upload_file", "") {
            wait_gate(gate).await;
        }
        self.calls.lock().await.push(PlatformCall::UploadFile {
            name: content.name.clone(),
            mime: content.mime.clone(),
            size: content.size,
            content: content.data.clone().unwrap_or_default(),
        });
        Ok(Some(self.file_key.lock().unwrap().clone()))
    }

    async fn reply_message(
        &self,
        message_id: &str,
        msg_type: &str,
        content: &serde_json::Value,
    ) -> crate::error::Result<(String, Option<String>)> {
        if self
            .ambiguous_send_message_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            self.ambiguous_send_message_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(crate::error::BridgeError::FeishuHttp {
                status: 503,
                detail: "simulated post-accept send failure".into(),
            });
        }
        if self
            .fail_send_message_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            self.fail_send_message_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(crate::error::BridgeError::Feishu(
                "simulated send_message failure".into(),
            ));
        }
        let reply_id = self
            .message_reply_ids
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "msg_file".into());
        let gate = self.take_gate("send_message", message_id);
        let accepted_before_reply = gate.as_ref().is_some_and(|gate| gate.accepted_before_reply);
        if accepted_before_reply {
            self.calls.lock().await.push(PlatformCall::SendMessageReply {
                message_id: message_id.into(),
                msg_type: msg_type.into(),
                content: content.clone(),
            });
        }
        if let Some(gate) = gate {
            wait_gate(gate).await;
        }
        if !accepted_before_reply {
            self.calls.lock().await.push(PlatformCall::SendMessageReply {
                message_id: message_id.into(),
                msg_type: msg_type.into(),
                content: content.clone(),
            });
        }
        Ok((reply_id, Some("omt_file".into())))
    }
}

/// One `task_runtime` call as the mock records it: the session id and the
/// shell / child ids the reconcile asked about, in order (issue #454).
pub(crate) type TaskRuntimeCall = (String, Vec<String>, Vec<String>);

/// A per-read test hook (spec #602 review, round 5): run on every `transcript`
/// read with the number of gate-entered reads so far, so a test can flip state
/// at a specific read without a timing race.
pub(crate) type TranscriptReadHook = Box<dyn Fn(usize) + Send + Sync>;

/// One `model_surface` request's callback-relevant context, recorded so a test
/// can pin what a caller sent — the `/think` validation read's `catalog_only`,
/// a callback's remaining budget, or a drilled page's scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelSurfaceQueryRecord {
    pub scope: Option<String>,
    pub selected: Option<String>,
    pub catalog_only: bool,
    pub ack_budget: Option<std::time::Duration>,
}

/// Serves scripted parts/permissions instead of a live OpenCode server.
pub struct MockBackend {
    /// The parts the default assistant turn carries: reasoning → tool → text.
    pub parts: Vec<Part>,
    pub permissions: Vec<crate::backend::types::PermissionRequest>,
    /// Number of initial `list_permissions` calls to hang forever — simulates
    /// an in-flight request stuck on a half-open connection while the server
    /// restarts. Each hung call decrements the counter; once it reaches zero
    /// the call serves normally, so a bounded poller recovers on a later poll.
    pub hang_list_permissions: Arc<std::sync::atomic::AtomicUsize>,
    /// Same as `hang_list_permissions`, for `list_questions`.
    pub hang_list_questions: Arc<std::sync::atomic::AtomicUsize>,
    /// Same as `hang_list_permissions`, for `transcript` (the external
    /// poller's per-session read).
    pub hang_transcript: Arc<std::sync::atomic::AtomicUsize>,
    /// Same as `hang_list_permissions`, for `session_info` (the session
    /// subtitle fetch — a wedged first request on a freshly spawned server).
    pub hang_session_info: Arc<std::sync::atomic::AtomicUsize>,
    /// Same as `hang_list_permissions`, for `list_sessions` (the Lazy Start
    /// readiness probe).
    pub hang_list_sessions: Arc<std::sync::atomic::AtomicUsize>,
    /// While set, `list_permissions` fails with the structural 500 a deleted
    /// session directory produces (`FileSystem.realPath` ENOENT, #421) instead
    /// of serving the scripted requests. Flippable mid-test: a sweep must stop
    /// calling while the directory is gone and resume when it returns.
    pub fail_list_permissions: Arc<std::sync::atomic::AtomicBool>,
    /// Counts `list_permissions` calls. The sweep lists once per known
    /// directory — never once per session — so a store with many sessions in
    /// one directory still makes exactly one call per sweep (the V2 poller's
    /// no-iteration-storm contract).
    pub list_permission_calls: Arc<std::sync::atomic::AtomicUsize>,
    /// Counts `list_questions` calls, one per known directory per sweep.
    pub list_question_calls: Arc<std::sync::atomic::AtomicUsize>,
    /// Records every `session_info` call's session id, in call order — the
    /// parent-chain walk's read. A pending request whose session id cannot
    /// name a session must never trigger one (#489).
    pub session_info_calls: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// When set, `messages` returns this as a fresh user message (simulates
    /// a message posted from OpenChamber).
    pub external_user_message: Option<String>,
    /// Per-session fresh user messages (simulates an external message posted on
    /// a SPECIFIC session). Takes precedence over `external_user_message` when
    /// a session has an entry, so tests can script an external message on a
    /// historical (non-active) session while the active one has none.
    pub external_user_messages: std::collections::HashMap<String, String>,
    /// Per-session cola-authored user messages: simulates cola's OWN prompt
    /// persisting on the store with a `msg_cola_` id (ADR-0026) — e.g. after a
    /// server died mid-turn and healed, so the poller sees it as newer than the
    /// stale watermark. Returned with the newest user-message slot taken, so a
    /// test can assert cola's own round is never notified as external.
    pub cola_user_messages: std::collections::HashMap<String, String>,
    /// Created time of each cola-authored message, captured on first read so it
    /// stays stable across polls.
    pub cola_user_created: Arc<std::sync::Mutex<std::collections::HashMap<String, i64>>>,
    /// When set, `transcript` returns this as the assistant reply to the
    /// external user message (simulates OpenCode answering it), replacing the
    /// default assistant turn. Returned only once `external_reply_ready`
    /// flips, so tests can script the reply arriving on a LATER poll.
    pub external_reply_parts: Option<Vec<Part>>,
    /// Gates whether the `external_reply_parts` assistant turn is returned.
    /// The test holds a clone of this `Arc` and flips it after the
    /// notification card is sent, to simulate the model answering later.
    pub external_reply_ready: Arc<std::sync::atomic::AtomicBool>,
    /// Created time of the external user message, captured on first read so
    /// it stays stable across polls (the poller's watermark logic must not
    /// see the same message as "new" every call). A test may bump it via
    /// the `Arc` handle to simulate a SECOND external message arriving.
    pub external_user_created: Arc<std::sync::Mutex<Option<i64>>>,
    /// Records every `reply_permission` call: (request_id, reply).
    pub reply_permission_calls: Arc<tokio::sync::Mutex<Vec<(String, String)>>>,
    /// The session id each `reply_permission` was routed with, in call order —
    /// the child-session routing assertion (V2's reply is session-scoped).
    pub reply_permission_sessions: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// Records every `interrupt` call's session id (the `/stop` path).
    pub interrupt_calls: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// Records every `compact` call's session id (the `/compact` path).
    pub compact_calls: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// Records every `release_session` call's session id, in call order — the
    /// bridge's mirror teardown (a chat unmapping or switching away from a
    /// session).
    pub release_calls: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// Number of initial `release_session` calls to fail; a consumer treats a
    /// failure as best-effort and must not fail the command that dropped the
    /// mapping.
    pub fail_release_count: Arc<std::sync::atomic::AtomicUsize>,
    /// Request ids cola answered via `reply_permission` — filtered out of
    /// `list_permissions` like the real server drops a replied request. A test
    /// simulating resolution by ANOTHER client inserts the id here directly.
    pub replied_permissions: Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>,
    /// When true, `reply_permission` reports the request as already gone (404):
    /// simulates a click replayed after the request was resolved elsewhere (or
    /// after a cola restart cleared the in-memory guard).
    pub reply_permission_not_found: bool,
    /// Number of initial `reply_permission` calls to fail with a genuine
    /// (non-404) error — tests the guard rollback that lets the user retry.
    pub fail_reply_permission_count: Arc<std::sync::atomic::AtomicUsize>,
    /// When true, `reply_question`/`reject_question` report the request as
    /// already gone (404), like a question resolved elsewhere.
    pub reply_question_not_found: bool,
    /// Requests a test adds AFTER the app is built (simulating a request that
    /// arrives after the snapshot was sent, ADR-0028).
    pub extra_permissions: Arc<tokio::sync::Mutex<Vec<crate::backend::types::PermissionRequest>>>,
    /// session_id → server title (simulates OpenChamber's session title).
    /// `std::sync::Mutex` for interior mutability: `update_session_title`
    /// writes it through `&self` (the trait requires `&self`).
    pub session_titles: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
    /// Session id → scripted Session Transcripts served by `transcript`,
    /// consumed one snapshot per call (the last repeating). Lets a test script
    /// the Backend's history call by call — e.g. the post-prompt drain's exit
    /// check seeing no supplement and the racing finish re-check seeing one —
    /// and mutate it mid-turn (a supplement that missed the run, then its
    /// assistant reply). Sessions absent from the map keep the default shape.
    pub transcript_scripts:
        Arc<tokio::sync::Mutex<std::collections::HashMap<String, Vec<SessionTranscript>>>>,
    /// Records every `transcript` call's session id — the neutral read the
    /// render poll, the drain and the follow poll on.
    pub transcript_calls: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// Whether `session_diff` serves a V2-shaped answer (`true`) or V1's
    /// `Ok(None)`, which means no Turn Diff at all (ADR-0087). Default
    /// `false`.
    pub session_diff_available: std::sync::atomic::AtomicBool,
    /// The Turn Diff `session_diff` serves when
    /// [`Self::session_diff_available`] is set; `None` reads as an empty diff
    /// (a Turn that changed no file).
    pub session_diff: Arc<std::sync::Mutex<Option<crate::backend::TurnDiff>>>,
    /// Every `session_diff` read's arguments: `(session_id, from, to,
    /// context)`, in call order (#801). Lets a test pin the exact Turn range
    /// the callback asks for instead of only its effect.
    pub session_diff_calls: Arc<tokio::sync::Mutex<Vec<SessionDiffCall>>>,
    /// When set, `session_diff` fails with a transport-level error, standing in
    /// for the transient/restart/unanswerable read (ADR-0087). The call is still
    /// recorded, so a test can prove the read happened.
    pub fail_session_diff: std::sync::atomic::AtomicBool,
    /// When set, `session_diff` never resolves, standing in for a hung read
    /// (ADR-0087's timeout arm). The call is recorded first.
    pub hang_session_diff: std::sync::atomic::AtomicBool,
    /// Session ids whose `transcript` read fails with a 500 (spec #501): the
    /// child-read failure the ledger's activity carry-over pins — a failed
    /// gather leaves the row's last successful fragment in place, its age
    /// still growing. Armable mid-life via [`MockBackend::fail_transcript_for`].
    pub fail_transcripts: Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>,
    /// Session ids whose `transcript` read NEVER resolves (a half-open child
    /// read in a render's liveness gather, #457). The caller's own bound must
    /// abandon it. Reversible via [`MockBackend::release_transcript_for`].
    pub hang_transcripts: Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>,
    /// Number of initial `transcript` calls to fail with a 500 (then the read
    /// serves normally) — a wedged or failed read on one arm of a caller that
    /// must degrade without failing the whole turn. Armable through `&self`.
    pub fail_transcript_reads: Arc<std::sync::atomic::AtomicUsize>,
    /// Pending questions served by `list_questions`.
    pub questions: Vec<crate::backend::types::QuestionRequest>,
    /// Records `reply_question` calls: (request_id, answers). The answers are
    /// the positional string form the keyed neutral answers flatten to, so the
    /// existing click assertions read the submitted selection.
    pub reply_question_calls: Arc<tokio::sync::Mutex<Vec<QuestionReplyRecord>>>,
    /// The keyed answers each `reply_question` carried, in call order — the
    /// form-model assertion (V2 submits `Form.Answer` objects).
    pub reply_question_keyed_calls: Arc<tokio::sync::Mutex<Vec<KeyedQuestionReply>>>,
    /// Question ids cola answered/submitted/rejected via `reply_question` —
    /// filtered out of `list_questions` like the real server drops a replied
    /// request. A test simulating resolution by ANOTHER client inserts the id
    /// here directly.
    pub replied_questions: Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>,
    /// When set, `prompt` fails with this message (simulates a provider 503).
    pub prompt_error: Option<String>,
    /// Number of initial `prompt` calls to fail (for testing retry-after-error).
    pub fail_prompt_count: Arc<std::sync::atomic::AtomicUsize>,
    /// Number of initial `interrupt` calls to fail with a genuine error (the
    /// `/stop` abort's failure path); 0 succeeds like the real server.
    pub fail_interrupt_count: Arc<std::sync::atomic::AtomicUsize>,
    /// The message counted `interrupt` failures report; `None` keeps the
    /// generic one.
    pub fail_interrupt_message: Option<String>,
    /// Records every `prompt` call's text (the main dispatch and a Supplement
    /// go through the same submit, ADR-0056).
    pub prompt_calls: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// Records the number of images attached to each `prompt` call.
    pub prompt_files: Arc<tokio::sync::Mutex<Vec<usize>>>,
    /// Every prompt's attached file parts as `(mime, name, data_base64)`, in
    /// call order — what `prompt_files` counts, but with the payload facts a
    /// classification regression would show.
    #[allow(clippy::type_complexity)]
    pub prompt_file_parts: Arc<tokio::sync::Mutex<Vec<Vec<(String, Option<String>, String)>>>>,
    /// Records every `prompt` call's attached skills (spec #652, ticket #654):
    /// the neutral `{ id, name }` list, so a `/skill` submit is asserted
    /// through the mock.
    pub prompt_skills: Arc<tokio::sync::Mutex<Vec<Vec<crate::backend::PromptSkill>>>>,
    /// Records the model passed to each `prompt` call ("provider/model").
    pub prompt_models: Arc<tokio::sync::Mutex<Vec<Option<String>>>>,
    /// Records the variant passed to each `prompt` call (the `/think` override).
    pub prompt_variants: Arc<tokio::sync::Mutex<Vec<Option<String>>>>,
    /// Records the agent passed to each `prompt` call.
    pub prompt_agents: Arc<tokio::sync::Mutex<Vec<Option<String>>>>,
    /// Records the message_id passed to each `prompt` call (asserts every cola
    /// prompt carries a `msg_cola_` id and retries reuse it, ADR-0026).
    pub prompt_message_ids: Arc<tokio::sync::Mutex<Vec<Option<String>>>>,
    /// A callback run inside `prompt` just before it succeeds — lets a test
    /// mutate the session's working tree mid-turn (e.g. switch branches or
    /// leave an uncommitted file) to exercise the Turn Footer's turn-end
    /// refresh (ADR-0019).
    pub on_prompt: Option<Box<dyn Fn() + Send + Sync>>,
    /// When set, `prompt` waits for a permit here before returning, so a test
    /// can hold a turn in flight (inline a pending request, then abort) and
    /// only afterwards let it finish — the mid-turn ordering the turn-end
    /// leftover rejection (#187) acts on.
    pub prompt_gate: Option<Arc<tokio::sync::Semaphore>>,
    /// When set, every `transcript` read waits on this gate before serving —
    /// armable mid-life (`hold_transcripts`, same `&self` as the trait) so a
    /// test can park the ONE reader it owns (the out-of-turn follow, once the
    /// drain handed off) after it passed a tick's own marker check, inject a
    /// `/stop`, then release it — the mid-tick stop re-check (#394).
    /// [`Self::transcript_gate_entered`] counts reads that reached the gate.
    pub transcript_gate: std::sync::Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// Counts `transcript` reads that reached [`Self::transcript_gate`] — the
    /// test's "the reader is now parked" signal.
    pub transcript_gate_entered: Arc<std::sync::atomic::AtomicUsize>,
    /// The session id `create_session` returns.
    pub session_id: String,
    /// Every `create_session` call's requested directory (ADR-0041: asserts
    /// lazy creation — nothing on `/new`, one creation on the first message,
    /// in the pending's directory).
    pub created_session_dirs: Arc<tokio::sync::Mutex<Vec<Option<String>>>>,
    /// Number of initial `create_session` calls to fail — materialisation must
    /// keep the pending and retry on the next message.
    pub fail_create_session_count: Arc<std::sync::atomic::AtomicUsize>,
    /// When true, `prompt` 404s for any session id other than `session_id`
    /// (simulates a stale mapping to a session that no longer exists).
    pub stale_session_404: bool,
    /// child session_id → parent session_id, served by `session_info`
    /// (simulates sub-task sessions created by the `task` tool).
    pub session_parents: std::collections::HashMap<String, String>,
    /// The shared store served by `list_sessions` (for `/attach` and
    /// `/switch` tests).
    pub session_list: Vec<crate::backend::types::SessionListInfo>,
    /// Records `update_session_title` calls: (session_id, title).
    pub update_title_calls: Arc<tokio::sync::Mutex<Vec<(String, String)>>>,
    /// When true, `update_session_title` fails — materialisation must keep the
    /// created session (never orphan it) and warn.
    pub fail_title_patch: bool,
    /// Counts `list_sessions` invocations (asserts the 30 s cache).
    pub list_sessions_calls: Arc<std::sync::atomic::AtomicUsize>,
    /// Available agents served by `list_agents` (for the `/agent` card).
    pub agents: Vec<crate::backend::types::AgentInfo>,
    /// Registered skills served by `list_skills` (for the `/skill` picker and
    /// the `/skill <id>` resolution, spec #652, ticket #656).
    pub skills: Vec<crate::backend::SkillInfo>,
    /// Counts `list_skills` calls (a `/skill` dispatch must read the list
    /// exactly once).
    pub list_skills_calls: Arc<std::sync::atomic::AtomicUsize>,
    /// Records the directory each `list_skills` call was scoped to, in call
    /// order — the location a session's skill read must carry.
    pub list_skills_directories: Arc<tokio::sync::Mutex<Vec<Option<String>>>>,
    /// Simulate a real adapter BEFORE Lazy Start: serverless (an empty
    /// `base_url`, so a read against it yields nothing) and reporting
    /// [`crate::backend::AttachOutcome::Serverless`]. Lets a test drive the
    /// attach-only / NoServer path that the default mock (attached, and
    /// therefore "nothing to do") never reaches.
    pub serverless: bool,
    /// The attachment `attachment_status` reports (`cola status`/`doctor`'s
    /// read). `None` means "nothing to report", the default.
    pub attachment: Option<crate::backend::lifecycle::AttachmentStatus>,
    /// The client-scoped diagnosis `client_report` reports (`cola status`/
    /// `doctor`, ticket #883). `None` means "nothing to report", the default.
    pub client_report: Option<crate::backend::lifecycle::ClientReport>,
    /// The answer `probe_turn` gives (`cola doctor`'s turn probe).
    pub turn_probe: Option<crate::backend::lifecycle::TurnProbe>,
    /// Counts `ensure_attached` passes, so a poll-loop test can prove the
    /// client-owned pass actually ran.
    pub ensure_attached_calls: Arc<std::sync::atomic::AtomicUsize>,
    /// Available models grouped by provider, served by `list_models` and
    /// mapped into an OpenCode-shaped `model_surface` when no explicit
    /// `model_surface` is set.
    pub provider_models: Vec<crate::backend::types::ProviderModels>,
    /// Every `model_surface` request's context, in call order.
    pub model_surface_queries: Arc<tokio::sync::Mutex<Vec<ModelSurfaceQueryRecord>>>,
    /// When set, served verbatim by `model_surface`, overriding the
    /// OpenCode-shaped mapping of `provider_models`. Lets a test declare any
    /// client's surface (Codex's flat catalog, a two-scope fake, …).
    pub model_surface: Option<crate::backend::types::ModelSurface>,
    /// The configured default model served by `configured_default_model` (the
    /// second rung of the `/think` effective-model resolution).
    pub default_model: Option<crate::backend::types::ModelInfo>,
    /// What `provider_change_scope` answers (issue #910): the client's
    /// `provider_switch_on_resume` policy. `NewThread` (the default) means a
    /// live thread's provider pick is refused with a toast.
    pub provider_change_scope: crate::backend::types::ChangeScope,
    /// Every `switch_session_provider` call as `(session_id, provider)`, in
    /// call order.
    pub provider_switch_calls: Arc<tokio::sync::Mutex<Vec<(String, String)>>>,
    /// When set, `switch_session_provider` fails with this message and records
    /// nothing — the refused resume of issue #910.
    pub provider_switch_error: Option<String>,
    /// Every `create_session` call's thread-level provider and model id (issue
    /// #910): a Pending Session's creation picks.
    #[allow(clippy::type_complexity)]
    pub created_session_picks: Arc<tokio::sync::Mutex<Vec<(Option<String>, Option<String>)>>>,
    /// The context-window size `model_context_window` reports; `None`
    /// simulates a server that reports none (ADR-0044's used-only fallback).
    pub context_window: Option<i64>,
    /// Counts `model_context_window` calls, so a test can prove the per-turn
    /// memo fetches at most once per (provider, model).
    pub context_window_calls: Arc<std::sync::atomic::AtomicUsize>,
    /// The server-recorded session model served by `session_info` (the third
    /// rung of the `/think` effective-model resolution).
    pub session_model: Option<crate::backend::types::SessionModel>,
    /// Per-session last-run models served by `session_last_run_model` (the
    /// model of the newest assistant message — the durable ladder's last rung
    /// when the session has no selection and no configured default).
    pub session_last_run_models: std::collections::HashMap<String, crate::backend::types::ModelInfo>,
    /// When set, `session_last_run_model` fails with this message (the ladder
    /// must degrade to "no current model", never hang or guess).
    pub session_last_run_model_error: Option<String>,
    /// Per-session durable selections served by `session_selection` (V2's
    /// session-scoped switches). Empty by default: the mock speaks V1's
    /// per-prompt selection until a scenario seeds one or a switch lands.
    pub session_selections:
        Arc<tokio::sync::Mutex<std::collections::HashMap<String, crate::backend::types::SessionSelection>>>,
    /// Whether the mock speaks a generation with durable session selection
    /// (V2): `session_selection` serves the map and `keeps_session_selection`
    /// is true. Default false (V1: picks ride each prompt); a scenario that
    /// seeds a selection switches it on.
    pub durable_selection: bool,
    /// When set, `session_selection` fails with this message (a transient V2
    /// read failure) — the caller must not treat it as "no durable selection".
    pub session_selection_error: Option<String>,
    /// Whether the mock speaks a generation where a same-id re-post of an
    /// ADMITTED user message can continue the turn (V1's upsert-continue) —
    /// what [`crate::backend::Backend::reuse_continues_an_admitted_turn`]
    /// answers. Default true (V1); a V2 scenario sets it false, where admission
    /// makes the re-post a no-op.
    pub reuse_continues_an_admitted_turn: bool,
    /// Whether the mock speaks a generation with the resume endpoint (V2) —
    /// what [`crate::backend::Backend::resume_supported`] answers. Default
    /// false (V1: no resume endpoint, so the 重新发起 action degrades to a new
    /// Turn); a V2 scenario sets it true.
    pub resume_supported: bool,
    /// Records every `resume` call's session id (the 重新发起 action, #437).
    pub resume_calls: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// The ordered recovery-write sequence the 重新发起 action issues
    /// (`"interrupt:<sid>"` / `"resume:<sid>"`), so a test pins the order:
    /// interrupt must precede resume (#437). `/stop`'s interrupt writes the
    /// same entry; a recovery scenario issues nothing else.
    pub recovery_ops: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// Number of initial `resume` calls to fail with a genuine error (the
    /// action's failure path); consumed one per call.
    pub fail_resume_count: Arc<std::sync::atomic::AtomicUsize>,
    /// The message counted `resume` failures report; `None` keeps the generic.
    pub fail_resume_message: Option<String>,
    /// Records `switch_session_model` calls: `(session_id, model)`.
    pub switch_model_calls: Arc<tokio::sync::Mutex<Vec<(String, crate::backend::types::ModelInfo)>>>,
    /// Records `switch_session_agent` calls: `(session_id, agent)`.
    pub switch_agent_calls: Arc<tokio::sync::Mutex<Vec<(String, String)>>>,
    /// When set, both `switch_session_*` calls fail with this message.
    pub switch_error: Option<String>,
    /// Per-session server status served by `session_status` (session_id →
    /// status). A missing key means idle (matches the server: a finished run is
    /// removed from the status map). `Some(None)` inside simulates a session
    /// present but with an unrecognised status type (unknown). Behind a lock so
    /// a test can flip a session Busy → Idle while a loop (e.g. the #284 drain
    /// follow) is watching it.
    pub session_statuses: Arc<
        tokio::sync::Mutex<std::collections::HashMap<String, Option<crate::backend::types::SessionStatus>>>,
    >,
    /// Records the session ids each `session_status` read targeted — the `/sub`
    /// card asserts one read per rendered row (spec #344).
    pub session_status_reads: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// The runtime verdict `task_runtime` serves (issue #454): set per test to
    /// script a shell the runtime reports ended/missing, or a child session it
    /// reports inactive. Empty by default — no evidence, exactly like V1.
    pub task_runtime: Arc<std::sync::Mutex<crate::backend::TaskRuntime>>,
    /// Whether this client publishes a Background Task surface (spec #860,
    /// #880). True by default — the V2-shaped mock every ledger test drives; a
    /// test that scripts a no-surface client flips it through
    /// [`Self::without_task_surface`].
    pub task_surface: bool,
    /// Message ids the client answers as cola-authored through the authorship
    /// seam (ADR-0090): a client that assigns its own ids answers from its own
    /// record, never from the `msg_cola_` prefix. Empty by default, so the mock
    /// keeps the neutral prefix rule.
    pub authored_message_ids: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Records every `task_runtime` call: `(session_id, shells, children)`.
    pub task_runtime_calls: Arc<tokio::sync::Mutex<Vec<TaskRuntimeCall>>>,
    /// Number of initial `task_runtime` calls to fail with a 500 (then later
    /// reads serve normally) — the shared reconcile must leave the transcript
    /// exactly as read, never guess a verdict (#589).
    pub fail_task_runtime_reads: Arc<std::sync::atomic::AtomicUsize>,
    /// Number of initial `task_runtime` calls to hang forever — a wedged
    /// runtime read the caller's own request bound must abandon; the
    /// transcript must stay exactly as read (#589).
    pub hang_task_runtime: Arc<std::sync::atomic::AtomicUsize>,
    /// The child-evidence read's scripted answers, keyed by child session id
    /// (#591, issue #464): the newest assistant message's evidence for a
    /// suspect. A child with no entry answers `Unfinished` — no evidence,
    /// exactly like V1.
    pub child_evidence: Arc<tokio::sync::Mutex<std::collections::HashMap<String, ChildEvidence>>>,
    /// Records every `child_evidence` call's child session id, in order.
    pub child_evidence_calls: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// Number of initial `child_evidence` calls to fail with a 500 (then later
    /// reads serve normally) — an unreadable child leaves the row exactly as
    /// the runtime marked it (#591).
    pub fail_child_evidence_reads: Arc<std::sync::atomic::AtomicUsize>,
    /// Number of initial `child_evidence` calls to hang forever — a wedged
    /// child read the cycle's own read budget must abandon, skipping the
    /// cycle's later reads instead of stacking another timeout (spec #588,
    /// review PR #595).
    pub hang_child_evidence: Arc<std::sync::atomic::AtomicUsize>,
    /// The output windows `shell_output` serves, keyed by shell id (spec #588,
    /// #592). A missing key (or a stored `None`) is the no-output case — a
    /// vanished record answers `Ok(None)`.
    pub shell_outputs:
        Arc<std::sync::Mutex<std::collections::HashMap<String, Option<crate::backend::ShellOutputWindow>>>>,
    /// Records every `shell_output` call's shell id, in order — the shared
    /// reconcile's one-tail-read-per-shell-per-cycle bound.
    pub shell_output_calls: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// Number of initial `shell_output` calls to fail with a 500 (then later
    /// reads serve normally) — a window must be omitted, never guessed.
    pub fail_shell_output_reads: Arc<std::sync::atomic::AtomicUsize>,
    /// Number of initial `shell_output` calls to hang forever — a wedged
    /// capture the cycle's own read budget must abandon, skipping the cycle's
    /// later window reads instead of stacking another timeout (spec #588,
    /// review PR #595).
    pub hang_shell_output: Arc<std::sync::atomic::AtomicUsize>,
    /// The Background Task State handle (issue #454, review #588, ticket #682):
    /// the module's one handle — the same type the adapter holds and the Bridge
    /// records through. Its liveness tier is recorded through `record` /
    /// `set_unconfirmed` and applied inside `transcript`, exactly like the real
    /// adapter's. Kept named `overlay` for the tests that read it by name.
    pub overlay: crate::bridge::tasks::TasksHandle,
    /// When set, `session_status` fails with this message (simulates a read
    /// failure — the caller must not guess a status).
    pub session_status_error: Option<String>,
    /// When true, `session_status` fails from that moment on — the post-build
    /// toggle for a decision/observation read that must degrade to "unknown"
    /// (the retry's status leg, spec #391). Separate from
    /// [`Self::session_status_error`], which is fixed at construction.
    pub session_status_fails: Arc<std::sync::atomic::AtomicBool>,
    /// When set, every `transcript` read rewrites its last assistant text part
    /// with a fresh, numbered body (spec #602 review): a deterministic
    /// ever-changing stream, so each read carries content the card has not
    /// seen — without the text accumulating. Used to prove finalization cannot
    /// chase a continuing stream forever.
    pub growing_transcript: Arc<std::sync::atomic::AtomicBool>,
    /// The number of `transcript` reads the growing stream has rewritten.
    pub growing_transcript_reads: Arc<std::sync::atomic::AtomicUsize>,
    /// A targeted, one-shot `transcript` read delay (spec #602 review, round 7):
    /// when set, the FIRST read whose newest assistant text equals the marker
    /// sleeps the duration before it is served — a delayed-but-SUCCESSFUL read
    /// carrying a specific late part, distinct from a hang. Used to prove the
    /// finalization read gets the full per-read timeout, never a grace-capped
    /// one.
    pub delay_reads_with_tail: std::sync::Mutex<Option<(String, std::time::Duration)>>,
    /// A test hook run on every `transcript` read with the number of
    /// gate-entered reads so far (spec #602 review, round 5): it lets a test
    /// flip state at a specific read — the finalization confirmation read is
    /// the third gate-entered read — without a timing race.
    pub on_transcript_read: std::sync::Mutex<Option<TranscriptReadHook>>,
    /// Scripts the ADR-0028 busy→idle race: the first `session_status` read
    /// returns Busy (and clears the flag), later reads serve the map.
    pub status_busy_once: std::sync::atomic::AtomicBool,
    /// Scenario state for [`MockBackend::given_prompt`]: `(needle, parts)`
    /// pairs matched against the prompt text, first match wins.
    prompt_scripts: Vec<(String, Vec<Part>)>,
    /// The parts the last matched prompt script streamed. `transcript` serves
    /// them instead of `parts` once set, so the scripted prompt's own turn
    /// renders consistently through both read paths.
    last_prompt_parts: std::sync::Mutex<Option<Vec<Part>>>,
    /// The message counted `prompt` failures report (see
    /// [`MockBackend::fail_prompts`]); `None` keeps the generic one.
    fail_prompt_message: Option<String>,
    /// Every `prompt` submit's `(message_id, text)`, recorded for the default
    /// transcript: the user messages a real server would have persisted by the
    /// time the Turn reads the turn (ADR-0056). An empty id falls back to the
    /// historical `msg_user`.
    prompts: std::sync::Mutex<Vec<(String, String)>>,
}

impl MockBackend {
    pub fn new(parts: Vec<Part>) -> Self {
        Self {
            parts,
            permissions: Vec::new(),
            hang_list_permissions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            hang_list_questions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            hang_transcript: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            hang_session_info: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            hang_list_sessions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            fail_list_permissions: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            list_permission_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            list_question_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            session_info_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            external_user_message: None,
            external_user_messages: std::collections::HashMap::new(),
            cola_user_messages: std::collections::HashMap::new(),
            cola_user_created: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            external_reply_parts: None,
            external_reply_ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            external_user_created: Arc::new(std::sync::Mutex::new(None)),
            reply_permission_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            reply_permission_sessions: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            interrupt_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            compact_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            release_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            fail_release_count: std::sync::atomic::AtomicUsize::new(0).into(),
            replied_permissions: Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new())),
            reply_permission_not_found: false,
            fail_reply_permission_count: std::sync::atomic::AtomicUsize::new(0).into(),
            reply_question_not_found: false,
            extra_permissions: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            session_titles: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            transcript_scripts: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            transcript_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            session_diff_available: std::sync::atomic::AtomicBool::new(false),
            session_diff: Arc::new(std::sync::Mutex::new(None)),
            session_diff_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            fail_session_diff: std::sync::atomic::AtomicBool::new(false),
            hang_session_diff: std::sync::atomic::AtomicBool::new(false),
            fail_transcripts: Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new())),
            hang_transcripts: Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new())),
            fail_transcript_reads: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            questions: Vec::new(),
            reply_question_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            reply_question_keyed_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            replied_questions: Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new())),
            prompt_error: None,
            fail_prompt_count: std::sync::atomic::AtomicUsize::new(0).into(),
            fail_interrupt_count: std::sync::atomic::AtomicUsize::new(0).into(),
            fail_interrupt_message: None,
            prompt_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_files: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_file_parts: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_skills: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_models: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_variants: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_agents: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_message_ids: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            on_prompt: None,
            prompt_gate: None,
            transcript_gate: std::sync::Mutex::new(None),
            transcript_gate_entered: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            session_id: "ses_test".into(),
            created_session_dirs: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            fail_create_session_count: std::sync::atomic::AtomicUsize::new(0).into(),
            stale_session_404: false,
            session_parents: std::collections::HashMap::new(),
            session_list: Vec::new(),
            update_title_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            fail_title_patch: false,
            list_sessions_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            agents: Vec::new(),
            skills: Vec::new(),
            list_skills_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            list_skills_directories: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            serverless: false,
            attachment: None,
            client_report: None,
            turn_probe: None,
            ensure_attached_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            provider_models: Vec::new(),
            model_surface_queries: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            model_surface: None,
            default_model: None,
            provider_change_scope: crate::backend::types::ChangeScope::NewThread,
            provider_switch_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            provider_switch_error: None,
            created_session_picks: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            context_window: Some(100_000),
            context_window_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            session_model: None,
            session_last_run_models: std::collections::HashMap::new(),
            session_last_run_model_error: None,
            session_selections: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            durable_selection: false,
            session_selection_error: None,
            reuse_continues_an_admitted_turn: true,
            resume_supported: false,
            resume_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            recovery_ops: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            fail_resume_count: std::sync::atomic::AtomicUsize::new(0).into(),
            fail_resume_message: None,
            switch_model_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            switch_agent_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            switch_error: None,
            session_statuses: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            session_status_reads: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            task_runtime: Arc::new(std::sync::Mutex::new(crate::backend::TaskRuntime::default())),
            task_surface: true,
            authored_message_ids: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            task_runtime_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            fail_task_runtime_reads: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            hang_task_runtime: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            child_evidence: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            child_evidence_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            fail_child_evidence_reads: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            hang_child_evidence: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            shell_outputs: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            shell_output_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            fail_shell_output_reads: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            hang_shell_output: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            overlay: crate::bridge::tasks::TasksHandle::new(),
            session_status_error: None,
            session_status_fails: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            growing_transcript: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            growing_transcript_reads: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            delay_reads_with_tail: std::sync::Mutex::new(None),
            on_transcript_read: std::sync::Mutex::new(None),
            status_busy_once: std::sync::atomic::AtomicBool::new(false),
            prompt_scripts: Vec::new(),
            last_prompt_parts: std::sync::Mutex::new(None),
            fail_prompt_message: None,
            prompts: std::sync::Mutex::new(Vec::new()),
        }
    }

    // ===== Scenario vocabulary =====
    //
    // The methods below script the common flows without the test reaching into
    // the fields above: "given a prompt, stream these parts / ask these
    // permissions / fail this call". Fields stay public for the exotic axes;
    // these are the front door for the flows tests actually build.

    /// Scenario: the shared store holds exactly `sessions` (drives `/switch`,
    /// `/dir` and the external poller's discovery).
    pub(crate) fn given_sessions(
        &mut self,
        sessions: Vec<crate::backend::types::SessionListInfo>,
    ) -> &mut Self {
        self.session_list = sessions;
        self
    }

    /// Scenario: `request` is pending on the server (appends to the pending
    /// list `list_permissions` serves).
    pub(crate) fn ask_permission(&mut self, request: crate::backend::types::PermissionRequest) -> &mut Self {
        self.permissions.push(request);
        self
    }

    /// Scenario: exactly these permissions are pending.
    pub(crate) fn ask_permissions(
        &mut self,
        requests: Vec<crate::backend::types::PermissionRequest>,
    ) -> &mut Self {
        self.permissions = requests;
        self
    }

    /// Scenario: `request` is pending on the server (appends to the pending
    /// list `list_questions` serves).
    pub(crate) fn ask_question(&mut self, request: crate::backend::types::QuestionRequest) -> &mut Self {
        self.questions.push(request);
        self
    }

    /// Scenario: exactly these questions are pending.
    pub(crate) fn ask_questions(
        &mut self,
        requests: Vec<crate::backend::types::QuestionRequest>,
    ) -> &mut Self {
        self.questions = requests;
        self
    }

    /// Scenario: every `prompt` fails with `message`.
    pub(crate) fn fail_prompt(&mut self, message: &str) -> &mut Self {
        self.prompt_error = Some(message.to_string());
        self
    }

    /// Scenario: the next `count` `prompt` calls fail with `message`; the
    /// scripted failure is consumed, so later prompts succeed.
    pub(crate) fn fail_prompts(&mut self, count: usize, message: &str) -> &mut Self {
        self.fail_prompt_count
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self.fail_prompt_message = Some(message.to_string());
        self
    }

    /// Scenario: the next `count` `interrupt` calls fail with `message`; the
    /// scripted failure is consumed, so a later `/stop` succeeds (the `/stop`
    /// rollback path, #394).
    pub(crate) fn fail_interrupts(&mut self, count: usize, message: &str) -> &mut Self {
        self.fail_interrupt_count
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self.fail_interrupt_message = Some(message.to_string());
        self
    }

    /// Scenario: the mock speaks a generation with the resume endpoint (V2) —
    /// the 重新发起 action then interrupts and resumes instead of degrading to
    /// a new Turn.
    pub(crate) fn with_resume_supported(&mut self, supported: bool) -> &mut Self {
        self.resume_supported = supported;
        self
    }

    /// Scenario: the next `count` `resume` calls fail with `message`; the
    /// scripted failure is consumed, so a later press succeeds.
    pub(crate) fn fail_resumes(&mut self, count: usize, message: &str) -> &mut Self {
        self.fail_resume_count
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self.fail_resume_message = Some(message.to_string());
        self
    }

    /// Scenario: `prompt` streams `parts` when the prompt text contains
    /// `needle` (first match wins; `""` matches any prompt).
    pub(crate) fn given_prompt(&mut self, needle: &str, parts: Vec<Part>) -> &mut Self {
        self.prompt_scripts.push((needle.to_string(), parts));
        self
    }

    /// Scenario: `list_models` serves `models`.
    pub(crate) fn with_models(&mut self, models: Vec<crate::backend::types::ProviderModels>) -> &mut Self {
        self.provider_models = models;
        self
    }

    /// Scenario: `model_surface` serves `surface` verbatim, whatever its
    /// scopes — a client shape other than OpenCode's provider tree.
    pub(crate) fn with_model_surface(&mut self, surface: crate::backend::types::ModelSurface) -> &mut Self {
        self.model_surface = Some(surface);
        self
    }

    /// Scenario: the client allows (or refuses) a confirmed provider switch on
    /// an existing thread (issue #910).
    pub(crate) fn with_provider_change_scope(
        &mut self,
        scope: crate::backend::types::ChangeScope,
    ) -> &mut Self {
        self.provider_change_scope = scope;
        self
    }

    /// Scenario: `switch_session_provider` fails with `message` and records no
    /// switch — a server that refused the resume.
    pub(crate) fn fail_provider_switch(&mut self, message: &str) -> &mut Self {
        self.provider_switch_error = Some(message.to_string());
        self
    }

    /// Scenario: the client answers `id` as cola-authored through the
    /// authorship seam (a client that assigns its own ids, ADR-0090).
    pub(crate) fn with_authored_id(&mut self, id: &str) -> &mut Self {
        self.authored_message_ids
            .lock()
            .expect("authored ids")
            .insert(id.to_string());
        self
    }

    /// Scenario: `list_agents` serves `agents`.
    pub(crate) fn with_agents(&mut self, agents: Vec<crate::backend::types::AgentInfo>) -> &mut Self {
        self.agents = agents;
        self
    }

    /// Scenario: `list_skills` serves `skills` (the `/skill` picker and the
    /// `/skill <id>` resolution, spec #652, ticket #656).
    pub(crate) fn with_skills(&mut self, skills: Vec<crate::backend::SkillInfo>) -> &mut Self {
        self.skills = skills;
        self
    }

    /// Scenario: `configured_default_model` serves `model`.
    pub(crate) fn with_default_model(&mut self, model: crate::backend::types::ModelInfo) -> &mut Self {
        self.default_model = Some(model);
        self
    }

    /// Scenario: the server records `model` for the session (`session_info`).
    pub(crate) fn with_session_model(&mut self, model: crate::backend::types::SessionModel) -> &mut Self {
        self.session_model = Some(model);
        self
    }

    /// Scenario: the newest assistant message of `session_id` ran `model`
    /// (`session_last_run_model`). Turns the mock into the durable-selection
    /// generation (V2 semantics): only a durable ladder reads this rung.
    pub(crate) fn with_session_last_run_model(
        &mut self,
        session_id: &str,
        model: crate::backend::types::ModelInfo,
    ) -> &mut Self {
        self.durable_selection = true;
        self.session_last_run_models.insert(session_id.to_string(), model);
        self
    }

    /// Scenario: the last-run model read fails with `message` (a transient V2
    /// failure) — the ladder must degrade to "no current model", never guess.
    pub(crate) fn fail_session_last_run_model(&mut self, message: &str) -> &mut Self {
        self.session_last_run_model_error = Some(message.to_string());
        self
    }

    /// Scenario: the mock speaks a generation where a same-id re-post of an
    /// admitted turn can continue it (V1) or is a no-op (V2, `false`). Drives
    /// [`crate::backend::Backend::reuse_continues_an_admitted_turn`].
    pub(crate) fn with_reuse_continues_an_admitted_turn(&mut self, reuse_continues: bool) -> &mut Self {
        self.reuse_continues_an_admitted_turn = reuse_continues;
        self
    }

    /// Scenario: the session has `selection` durably (V2's session-scoped
    /// switches) — what `session_selection` serves before any pick. Turns the
    /// mock into the durable-selection generation (V2 semantics).
    pub(crate) fn with_session_selection(
        &mut self,
        session_id: &str,
        selection: crate::backend::types::SessionSelection,
    ) -> &mut Self {
        self.durable_selection = true;
        self.session_selections
            .try_lock()
            .expect("with_session_selection before the app is built")
            .insert(session_id.to_string(), selection);
        self
    }

    /// Scenario: both session switches fail with `message` (a V2 server that
    /// rejects the switch) — the pick must not be mirrored.
    pub(crate) fn fail_session_switch(&mut self, message: &str) -> &mut Self {
        self.switch_error = Some(message.to_string());
        self
    }

    /// Scenario: the durable selection read fails with `message` (a transient
    /// V2 failure) — a pick that needs the session's own selection must not
    /// pretend the generation has none.
    pub(crate) fn fail_session_selection(&mut self, message: &str) -> &mut Self {
        self.session_selection_error = Some(message.to_string());
        self
    }

    /// Scenario: `create_session` returns `id`.
    pub(crate) fn with_session_id(&mut self, id: &str) -> &mut Self {
        self.session_id = id.to_string();
        self
    }

    /// Scenario: `transcript` serves `snapshots` for `session_id`, one per
    /// call (the last repeating) — the neutral view of that session's history.
    /// Sessions without a script keep the default shape.
    pub(crate) fn given_transcript(
        &mut self,
        session_id: &str,
        snapshots: Vec<SessionTranscript>,
    ) -> &mut Self {
        self.transcript_scripts
            .try_lock()
            .expect("given_transcript before the app is built")
            .insert(session_id.to_string(), snapshots);
        self
    }

    /// Scenario: the backend serves the V2 session diff endpoint, answering
    /// `diff` for every read (#799, #801). Without this the mock keeps V1's
    /// `Ok(None)` — no Turn Diff on either surface (ADR-0087).
    pub(crate) fn given_session_diff(&mut self, diff: crate::backend::TurnDiff) -> &mut Self {
        *self
            .session_diff
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(diff);
        self.session_diff_available
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// [`given_transcript`] after the app is built (through the `Arc` the app
    /// holds): script a session the pass must be able to read — a second
    /// mapped session whose reads are a test's proof-of-pass, or a session
    /// whose read changes mid-life.
    pub(crate) async fn given_transcript_after_build(
        &self,
        session_id: &str,
        snapshots: Vec<SessionTranscript>,
    ) {
        self.transcript_scripts
            .lock()
            .await
            .insert(session_id.to_string(), snapshots);
    }

    /// Scenario: `transcript` for `session_id` fails with a 500 — the child
    /// read a ledger row's activity gather must survive (spec #501): a failed
    /// gather leaves the row's last successful fragment in place, its age
    /// still growing. Armable mid-life (`&self`), and reversible:
    /// [`Self::heal_transcript`] clears it.
    pub(crate) async fn fail_transcript_for(&self, session_id: &str) {
        self.fail_transcripts.lock().await.insert(session_id.to_string());
    }

    /// [`Self::fail_transcript_for`]'s reversal: the session's reads serve its
    /// script again.
    pub(crate) async fn heal_transcript(&self, session_id: &str) {
        self.fail_transcripts.lock().await.remove(session_id);
    }

    /// Scenario: `transcript` for `session_id` never resolves — a half-open
    /// read (a wedged child session in the liveness gather, #457). Every
    /// caller must bound the read; the parked future is dropped when it does.
    /// Reversible via [`Self::release_transcript_for`].
    pub(crate) async fn hang_transcript_for(&self, session_id: &str) {
        self.hang_transcripts.lock().await.insert(session_id.to_string());
    }

    /// [`Self::hang_transcript_for`]'s reversal: the session's reads serve
    /// their script again (a previously parked future stays dropped).
    pub(crate) async fn release_transcript_for(&self, session_id: &str) {
        self.hang_transcripts.lock().await.remove(session_id);
    }

    /// Scenario: the next `count` `transcript` reads fail with a 500 (later
    /// reads serve normally) — one arm of a caller that must degrade without
    /// failing the whole turn.
    pub(crate) fn fail_transcript_reads(&self, count: usize) -> &Self {
        self.fail_transcript_reads
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Scenario: hold every `prompt` until the returned semaphore is released
    /// — the test can keep a turn in flight and interleave state through the
    /// normal seams.
    pub(crate) fn hold_prompts(&mut self) -> Arc<tokio::sync::Semaphore> {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        self.prompt_gate = Some(Arc::clone(&gate));
        gate
    }

    /// Scenario: hold every `transcript` read until the returned semaphore is
    /// released, counting reads that reached the gate in
    /// [`Self::transcript_gate_entered`]. Armable through `&self` after the
    /// app is built, so a test can park the follow's next read (already past
    /// its tick's marker check) and inject a `/stop` before releasing it
    /// (#394).
    pub(crate) fn hold_transcripts(&self) -> Arc<tokio::sync::Semaphore> {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *self.transcript_gate.lock().unwrap() = Some(Arc::clone(&gate));
        gate
    }

    /// Scenario: the addressed permission was resolved elsewhere — a reply
    /// reports it gone (404) instead of succeeding.
    pub(crate) fn permission_resolved_elsewhere(&mut self) -> &mut Self {
        self.reply_permission_not_found = true;
        self
    }

    /// Scenario: the addressed question was resolved elsewhere — a reply
    /// reports it gone (404) instead of succeeding.
    pub(crate) fn question_resolved_elsewhere(&mut self) -> &mut Self {
        self.reply_question_not_found = true;
        self
    }

    /// Scenario: another client already resolved `request_id` — the server
    /// filters it out of the pending list, like a replied request. Usable
    /// mid-lifecycle (through the `Arc` the app holds).
    pub(crate) async fn permission_resolved_by_another(&self, request_id: &str) {
        self.replied_permissions
            .lock()
            .await
            .insert(request_id.to_string());
    }

    /// Scenario: another client already resolved `question_id` — the server
    /// filters it out of the pending list.
    pub(crate) async fn question_resolved_by_another(&self, question_id: &str) {
        self.replied_questions
            .lock()
            .await
            .insert(question_id.to_string());
    }

    /// Scenario: the external message's server timestamp (captured on first
    /// read otherwise) — lets a test place it in the past.
    pub(crate) fn external_message_created_at(&self, ms: i64) -> &Self {
        *self.external_user_created.lock().unwrap() = Some(ms);
        self
    }

    /// Scenario: another client (e.g. OpenChamber) posts `text` into the
    /// session; the external poller sees it as a new user message.
    pub(crate) fn external_message(&mut self, text: &str) -> &mut Self {
        self.external_user_message = Some(text.to_string());
        self
    }

    /// Scenario: another client posts `text` into a SPECIFIC session (takes
    /// precedence over [`external_message`] for that session).
    pub(crate) fn external_message_for(&mut self, session_id: &str, text: &str) -> &mut Self {
        self.external_user_messages
            .insert(session_id.to_string(), text.to_string());
        self
    }

    /// Scenario: cola's OWN prompt persisted on the store with a `msg_cola_`
    /// id (ADR-0026) — e.g. after a server died mid-turn and healed. The
    /// poller must recognise it as cola-authored, never as external traffic.
    pub(crate) fn cola_message(&mut self, session_id: &str, text: &str) -> &mut Self {
        self.cola_user_messages
            .insert(session_id.to_string(), text.to_string());
        self
    }

    /// Scenario: the next `count` `reply_permission` calls fail with a genuine
    /// (non-404) error — the in-flight guard must roll back so the user can
    /// retry.
    pub(crate) fn fail_permission_replies(&self, count: usize) -> &Self {
        self.fail_reply_permission_count
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Scenario: run `hook` inside `prompt` just before it succeeds — lets a
    /// test mutate the session's working tree mid-turn (e.g. switch branches
    /// or leave an uncommitted file) to exercise the Turn Footer's turn-end
    /// refresh (ADR-0019).
    pub(crate) fn on_prompt(&mut self, hook: impl Fn() + Send + Sync + 'static) -> &mut Self {
        self.on_prompt = Some(Box::new(hook));
        self
    }

    /// Scenario: the server's live status for `session_id` (`None` = an
    /// unrecognised status type; an absent entry means idle).
    pub(crate) fn with_session_status(
        &mut self,
        session_id: &str,
        status: Option<crate::backend::types::SessionStatus>,
    ) -> &mut Self {
        self.session_statuses
            .try_lock()
            .expect("with_session_status before the app is built")
            .insert(session_id.to_string(), status);
        self
    }

    /// Scenario: update the server's live status for `session_id` AFTER the
    /// app is built — e.g. a session that was Busy during the drain goes Idle
    /// while the out-of-turn follow watches it (#284).
    pub(crate) async fn set_session_status(
        &self,
        session_id: &str,
        status: Option<crate::backend::types::SessionStatus>,
    ) {
        self.session_statuses
            .lock()
            .await
            .insert(session_id.to_string(), status);
    }

    /// Scenario: a client with no Background Task surface (spec #860, #880):
    /// the card renders no ledger and the Bridge spends no runtime read, even
    /// when a scripted transcript carries task facts.
    pub(crate) fn without_task_surface(&mut self) -> &mut Self {
        self.task_surface = false;
        self
    }

    /// Scenario: the server's title for `session_id` (the render poll's
    /// subtitle source, `session_info`).
    pub(crate) fn with_session_title(&mut self, session_id: &str, title: &str) -> &mut Self {
        self.session_titles
            .lock()
            .unwrap()
            .insert(session_id.to_string(), title.to_string());
        self
    }

    /// Scenario: the next `count` `list_permissions` calls hang forever (a
    /// request stuck on a half-open connection); later polls serve normally.
    pub(crate) fn hang_permission_lists(&self, count: usize) -> &Self {
        self.hang_list_permissions
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Scenario: the next `count` `list_questions` calls hang forever.
    pub(crate) fn hang_question_lists(&self, count: usize) -> &Self {
        self.hang_list_questions
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Scenario: the next `count` per-session transcript reads hang forever
    /// (a wedged per-session read).
    pub(crate) fn hang_transcript_reads(&self, count: usize) -> &Self {
        self.hang_transcript
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Scenario: the next `count` runtime verdict reads hang forever (a wedged
    /// runtime registry, #589) — the caller's own request bound must abandon
    /// the read and change nothing.
    pub(crate) fn hang_task_runtime_reads(&self, count: usize) -> &Self {
        self.hang_task_runtime
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Scenario: the next `count` child-evidence reads hang forever (a wedged
    /// child read). The cycle's one read budget must abandon the read and skip
    /// its later reads — not stack another full timeout per suspect (spec
    /// #588, review PR #595).
    pub(crate) fn hang_child_evidence_reads(&self, count: usize) -> &Self {
        self.hang_child_evidence
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Scenario: the next `count` shell-output reads hang forever (a wedged
    /// capture). The cycle's one read budget must abandon the read and skip
    /// its later window reads — not stack another full timeout per shell
    /// (spec #588, review PR #595).
    pub(crate) fn hang_shell_output_reads(&self, count: usize) -> &Self {
        self.hang_shell_output
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Scenario: the next `count` runtime verdict reads fail with a 500 (a
    /// flaky runtime, #589) — the shared reconcile must leave the transcript
    /// exactly as read and retry after its interval.
    pub(crate) fn fail_task_runtime_reads(&self, count: usize) -> &Self {
        self.fail_task_runtime_reads
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Scenario: script one child session's newest-assistant evidence (#591,
    /// issue #464) — the read the shared reconcile spends on a suspect it
    /// cannot confirm as running.
    pub(crate) async fn with_child_evidence(&self, child_id: &str, evidence: ChildEvidence) -> &Self {
        self.child_evidence
            .lock()
            .await
            .insert(child_id.to_string(), evidence);
        self
    }

    /// Scenario: the next `count` `session_info` calls hang forever (a wedged
    /// subtitle fetch on a freshly spawned server).
    pub(crate) fn hang_session_info_reads(&self, count: usize) -> &Self {
        self.hang_session_info
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Scenario: the next `count` `list_sessions` calls hang forever (the Lazy
    /// Start readiness probe's wedged startup window).
    pub(crate) fn hang_session_lists(&self, count: usize) -> &Self {
        self.hang_list_sessions
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Scenario: the FIRST `session_status` read reports Busy, later reads
    /// serve the map (the ADR-0028 busy→idle race).
    pub(crate) fn busy_then_idle_once(&self) -> &Self {
        self.status_busy_once
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Scenario: `request` arrives AFTER the app was built (the snapshot was
    /// already sent, ADR-0028).
    pub(crate) fn ask_permission_later(&self, request: crate::backend::types::PermissionRequest) -> &Self {
        self.extra_permissions
            .try_lock()
            .expect("ask_permission_later before the app is built")
            .push(request);
        self
    }

    /// Scenario: the model answers the external message with `parts`. Returns
    /// the gate the test flips once the notification card has been sent.
    pub(crate) fn external_reply(&mut self, parts: Vec<Part>) -> Arc<std::sync::atomic::AtomicBool> {
        self.external_reply_parts = Some(parts);
        Arc::clone(&self.external_reply_ready)
    }

    /// Scenario: `child` is a sub-task session whose parent is `parent`
    /// (`session_info` serves the parent chain).
    pub(crate) fn with_session_parent(&mut self, child: &str, parent: &str) -> &mut Self {
        self.session_parents.insert(child.to_string(), parent.to_string());
        self
    }

    /// Scenario: the mapped session no longer exists on the server — `prompt`
    /// 404s for any session other than the one `create_session` returns.
    pub(crate) fn stale_session_mapping(&mut self) -> &mut Self {
        self.stale_session_404 = true;
        self
    }

    /// Scenario: the next `count` `create_session` calls fail (materialisation
    /// must keep the pending and retry on the next message).
    pub(crate) fn fail_create_sessions(&mut self, count: usize) -> &mut Self {
        self.fail_create_session_count
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Scenario: `update_session_title` fails (materialisation must keep the
    /// created session and warn).
    pub(crate) fn title_patch_fails(&mut self) -> &mut Self {
        self.fail_title_patch = true;
        self
    }

    /// Scenario: `session_status` fails with `message` (the caller must not
    /// guess a status).
    pub(crate) fn status_read_fails(&mut self, message: &str) -> &mut Self {
        self.session_status_error = Some(message.to_string());
        self
    }

    /// Shared recording/error policy for the question reply endpoints: record
    /// the call (positional for the click assertions, keyed for the form-model
    /// assertions), honor a scripted 404, then mark the request replied (like
    /// the real server drops a replied request from the pending list).
    async fn record_question_reply(
        &self,
        request_id: &str,
        answers: Vec<Vec<String>>,
        keyed: Vec<crate::backend::types::FormAnswer>,
    ) -> crate::error::Result<()> {
        self.reply_question_calls
            .lock()
            .await
            .push((request_id.to_string(), answers));
        self.reply_question_keyed_calls
            .lock()
            .await
            .push((request_id.to_string(), keyed));
        if self.reply_question_not_found {
            return Err(crate::error::BridgeError::NotFound(format!(
                "question {request_id}"
            )));
        }
        self.replied_questions.lock().await.insert(request_id.to_string());
        Ok(())
    }
    /// The default transcript shape `transcript` serves when the test scripts
    /// no transcript for the session: cola's own or an external user message
    /// when modeled, otherwise the assistant side of cola's own turn.
    fn default_transcript(&self, session_id: &str) -> SessionTranscript {
        let now = chrono::Utc::now().timestamp_millis();
        let mut messages: Vec<TranscriptMessage> = Vec::new();
        // cola's OWN user message persisting on the store (ADR-0026): id starts
        // with `msg_cola_`, created time stable across polls. Simulates a prompt
        // cola sent that the poller must recognise as cola-authored even when it
        // surfaces AFTER a stale watermark (server died mid-turn then healed).
        if let Some(cola_text) = self.cola_user_messages.get(session_id) {
            let created = {
                let mut map = self.cola_user_created.lock().unwrap();
                *map.entry(session_id.to_string())
                    .or_insert_with(|| chrono::Utc::now().timestamp_millis())
            };
            messages.push(typed_message(
                "msg_cola_mock_user",
                MessageRole::User,
                Some(created),
                vec![text_part(cola_text)],
            ));
        }
        // When set, simulate a user message posted by ANOTHER client (e.g.
        // OpenChamber), for the external-message poller tests. If an AI
        // reply is also set (and `external_reply_ready` has flipped), return
        // it as the assistant turn — simulates OpenCode answering the
        // shared-store message.
        let text = self
            .external_user_messages
            .get(session_id)
            .cloned()
            .or_else(|| self.external_user_message.clone());
        if let Some(text) = text {
            // Stable created time: captured once, so the same message is not
            // seen as "new" on every poll.
            let created = {
                let mut slot = self.external_user_created.lock().unwrap();
                *slot.get_or_insert_with(|| chrono::Utc::now().timestamp_millis())
            };
            // If cola's own message is also present, guarantee the external one
            // is NEWEST — it was posted after cola's (the heal scenario).
            let created = {
                let map = self.cola_user_created.lock().unwrap();
                map.get(session_id)
                    .map(|c| created.max(c + 1000))
                    .unwrap_or(created)
            };
            messages.push(typed_message(
                "msg_ext_user",
                MessageRole::User,
                Some(created),
                vec![text_part(&text)],
            ));
            if self
                .external_reply_ready
                .load(std::sync::atomic::Ordering::SeqCst)
                && let Some(parts) = &self.external_reply_parts
            {
                messages.push(typed_message(
                    "msg_ext_assist",
                    MessageRole::Assistant,
                    Some(created + 1000),
                    parts.clone(),
                ));
            }
            return SessionTranscript::new(messages);
        }
        // No cola-authored or external user message modeled: return the
        // assistant side of cola's own turn plus the user messages its prompts
        // admitted. The async-native Turn anchors on the `msg_cola_` user
        // message and renders the turn from the transcript (ADR-0056), so the
        // default read must carry what a real server persists. A modeled
        // cola-authored message keeps its historical shape (no assistant side).
        if !messages.is_empty() {
            return SessionTranscript::new(messages);
        }
        let parts = self
            .last_prompt_parts
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| self.parts.clone());
        let mut messages: Vec<TranscriptMessage> = self
            .prompts
            .lock()
            .unwrap()
            .iter()
            .map(|(id, text)| {
                let id = if id.is_empty() { "msg_user" } else { id.as_str() };
                typed_message(id, MessageRole::User, Some(now), vec![text_part(text)])
            })
            .collect();
        messages.push(typed_message(
            "msg_assist",
            MessageRole::Assistant,
            Some(now + 1000),
            parts,
        ));
        SessionTranscript::new(messages)
    }
}

/// Test scaffolding: while `counter` is positive, hang this call forever.
/// Decrements once per hung call, so a later poll (after a caller-side timeout
/// cancelled the first) serves normally. A future awaiting forever is exactly
/// the half-open-connection failure mode: only a caller-side timeout can end it.
async fn hang_if_scripted(counter: &std::sync::atomic::AtomicUsize) {
    if counter.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        counter.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        std::future::pending::<()>().await;
    }
}

#[async_trait::async_trait]
impl crate::backend::Backend for MockBackend {
    fn new_session_input(&self, directory: Option<&str>) -> crate::backend::types::CreateSessionInput {
        crate::backend::types::CreateSessionInput {
            id: None,
            agent: None,
            model: Some(crate::backend::types::ModelInfo {
                id: "m".into(),
                provider_id: "p".into(),
                variant: None,
            }),
            provider: None,
            location: directory.map(|d| crate::backend::types::Location {
                directory: d.to_string(),
            }),
        }
    }

    async fn create_session(
        &self,
        input: &crate::backend::types::CreateSessionInput,
    ) -> crate::error::Result<crate::backend::types::Session> {
        self.created_session_dirs
            .lock()
            .await
            .push(input.location.as_ref().map(|l| l.directory.clone()));
        self.created_session_picks.lock().await.push((
            input.provider.clone(),
            input.model.as_ref().map(|model| model.id.clone()),
        ));
        if self
            .fail_create_session_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            self.fail_create_session_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(crate::error::BridgeError::OpenCode(
                "Simulated create failure".into(),
            ));
        }
        Ok(crate::backend::types::Session {
            id: self.session_id.clone(),
            project_id: None,
            agent: None,
            title: None,
            location: None,
            cost: None,
            time: None,
        })
    }

    async fn list_sessions(&self) -> crate::error::Result<Vec<crate::backend::types::SessionListInfo>> {
        hang_if_scripted(&self.hang_list_sessions).await;
        self.list_sessions_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.session_list.clone())
    }

    async fn update_session_title(&self, session_id: &str, title: &str) -> crate::error::Result<()> {
        self.update_title_calls
            .lock()
            .await
            .push((session_id.to_string(), title.to_string()));
        if self.fail_title_patch {
            return Err(crate::error::BridgeError::OpenCode(
                "Simulated title failure".into(),
            ));
        }
        self.session_titles
            .lock()
            .unwrap()
            .insert(session_id.to_string(), title.to_string());
        Ok(())
    }

    /// The Bridge deliberately never deletes a server session (`/switch forget`
    /// keeps it, ADR-0008); the capability exists on the seam for the generation
    /// wire tests, so the mock is a no-op.
    async fn delete_session(&self, _session_id: &str) -> crate::error::Result<()> {
        Ok(())
    }

    /// Record the release and serve a scripted failure when one is armed: the
    /// bridge's release is best-effort, so a test can prove the command still
    /// lands when the client cannot free its mirror.
    async fn release_session(&self, session_id: &str) -> crate::error::Result<()> {
        self.release_calls.lock().await.push(session_id.to_string());
        if self.fail_release_count.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            self.fail_release_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(crate::error::BridgeError::OpenCode(
                "Simulated release failure".into(),
            ));
        }
        Ok(())
    }

    fn provider_change_scope(&self) -> crate::backend::types::ChangeScope {
        self.provider_change_scope
    }

    async fn switch_session_provider(&self, session_id: &str, provider: &str) -> crate::error::Result<()> {
        if let Some(message) = &self.provider_switch_error {
            return Err(crate::error::BridgeError::OpenCode(message.clone()));
        }
        self.provider_switch_calls
            .lock()
            .await
            .push((session_id.to_string(), provider.to_string()));
        Ok(())
    }

    async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        files: &[crate::backend::types::PromptFile],
        skills: &[crate::backend::PromptSkill],
        _model: Option<&crate::backend::types::ModelInfo>,
        variant: Option<&str>,
        agent: Option<&str>,
        message_id: Option<&str>,
    ) -> crate::error::Result<()> {
        self.prompt_calls.lock().await.push(text.to_string());
        self.prompt_files.lock().await.push(files.len());
        self.prompt_file_parts.lock().await.push(
            files
                .iter()
                .map(|file| (file.mime.clone(), file.name.clone(), file.data_base64.clone()))
                .collect(),
        );
        self.prompt_skills.lock().await.push(skills.to_vec());
        self.prompt_message_ids
            .lock()
            .await
            .push(message_id.map(|s| s.to_string()));
        self.prompt_models
            .lock()
            .await
            .push(_model.map(|m| format!("{}/{}", m.provider_id, m.id)));
        self.prompt_variants
            .lock()
            .await
            .push(variant.map(|v| v.to_string()));
        self.prompt_agents.lock().await.push(agent.map(|s| s.to_string()));
        // Hold the turn in flight while a test injects state (a pending
        // request) through the normal seams, then release it into the scripted
        // outcome — abort or success.
        if let Some(gate) = &self.prompt_gate {
            let _permit = gate.acquire().await;
        }
        if self.stale_session_404 && session_id != self.session_id {
            return Err(crate::error::BridgeError::SessionNotFound(session_id.to_string()));
        }
        if self.fail_prompt_count.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            self.fail_prompt_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(crate::error::BridgeError::OpenCode(
                self.fail_prompt_message
                    .clone()
                    .unwrap_or_else(|| "Simulated provider failure".into()),
            ));
        }
        if let Some(err) = &self.prompt_error {
            return Err(crate::error::BridgeError::OpenCode(err.clone()));
        }
        // The admitted user message: a rejected submit leaves none behind, and
        // the default transcript serves the ones a real server persisted so the
        // async-native Turn can anchor and render the submitted turn.
        self.prompts
            .lock()
            .unwrap()
            .push((message_id.unwrap_or_default().to_string(), text.to_string()));
        if let Some(hook) = &self.on_prompt {
            hook();
        }
        // A prompt script wins over the construction-time `parts`: the prompt
        // that matches makes `transcript` serve its own parts, so the submitted
        // turn renders consistently through the observed read.
        if let Some((_, parts)) = self
            .prompt_scripts
            .iter()
            .find(|(needle, _)| needle.is_empty() || text.contains(needle.as_str()))
        {
            *self.last_prompt_parts.lock().unwrap() = Some(parts.clone());
        }
        Ok(())
    }

    /// Read one Session's neutral transcript (ADR-0053). A scripted transcript
    /// wins; otherwise the scenario's default typed shape is served (spec
    /// #332, #334–#339).
    async fn transcript(&self, session_id: &str) -> crate::error::Result<SessionTranscript> {
        // A test may hold the read in flight — after the reader's own marker
        // check — and inject state before releasing it (#394).
        let gate = self.transcript_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            self.transcript_gate_entered
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _permit = gate.acquire().await;
        }
        // A test hook keyed on the gate-entered read index: state that must
        // change at a specific read (spec #602 review, round 5 — the
        // finalization confirmation read is the third gate-entered read) is
        // flipped here, before the read is served.
        if let Some(hook) = self.on_transcript_read.lock().unwrap().as_ref() {
            hook(
                self.transcript_gate_entered
                    .load(std::sync::atomic::Ordering::SeqCst),
            );
        }
        hang_if_scripted(&self.hang_transcript).await;
        // A one-shot failure: exactly `count` initial reads serve a 500, the
        // later ones normally — one caller's read arm degrades without
        // failing the whole turn.
        if self
            .fail_transcript_reads
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            self.fail_transcript_reads
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            self.transcript_calls.lock().await.push(session_id.to_string());
            return Err(crate::error::BridgeError::OpenCode(format!(
                "transcript {session_id} failed: 500 Internal Server Error"
            )));
        }
        // A per-session hang (#457): the read never resolves — the caller's own
        // bound must abandon it. The guard is dropped before parking, so a
        // release can still clear the set.
        let hanging = self.hang_transcripts.lock().await.contains(session_id);
        if hanging {
            std::future::pending::<()>().await;
        }
        // A session a test marked failing serves a 500, recorded like any other
        // read (the read was spent): the ledger's activity carry-over pins the
        // failed gather (spec #501).
        if self.fail_transcripts.lock().await.contains(session_id) {
            self.transcript_calls.lock().await.push(session_id.to_string());
            return Err(crate::error::BridgeError::OpenCode(format!(
                "transcript {session_id} failed: 500 Internal Server Error"
            )));
        }
        let scripted = {
            let mut scripts = self.transcript_scripts.lock().await;
            match scripts.get_mut(session_id) {
                Some(script) if !script.is_empty() => Some(if script.len() == 1 {
                    script[0].clone()
                } else {
                    script.remove(0)
                }),
                _ => None,
            }
        };
        let mut transcript = match scripted {
            Some(transcript) => transcript,
            None => self.default_transcript(session_id),
        };
        // The deterministic ever-changing stream (spec #602 review): rewrite the
        // last assistant text in place, so each read carries a fresh body the
        // card has not seen, while the served text stays bounded (the rewrite
        // replaces the source's run rather than appending).
        if self.growing_transcript.load(std::sync::atomic::Ordering::SeqCst) {
            let n = self
                .growing_transcript_reads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            if let Some(message) = transcript
                .messages
                .iter_mut()
                .rev()
                .find(|message| message.role == MessageRole::Assistant)
            {
                for part in message.parts.iter_mut() {
                    if let Part::Text(text) = part {
                        text.text = format!("第{n}段。");
                        break;
                    }
                }
            }
        }
        // The Background Task overlay, applied exactly like the real adapter's
        // (issue #454, review #588): retirements leave the live list and
        // unconfirmed markers ride every read.
        self.overlay.apply(session_id, &mut transcript);
        // A targeted read delay (spec #602 review, round 7): a read whose newest
        // assistant text matches the configured marker is a delayed-but-
        // SUCCESSFUL read — one-shot, so later reads flow. Used to prove the
        // finalization read gets the full per-read timeout, not a grace-capped
        // one.
        let delay = {
            let guard = self.delay_reads_with_tail.lock().unwrap();
            guard.as_ref().and_then(|(tail, delay)| {
                let newest_text = transcript
                    .messages
                    .iter()
                    .rev()
                    .find(|message| message.role == MessageRole::Assistant)
                    .and_then(|message| {
                        message.parts.iter().find_map(|part| match part {
                            Part::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                    });
                (newest_text == Some(tail.as_str())).then_some(*delay)
            })
        };
        if let Some(delay) = delay {
            *self.delay_reads_with_tail.lock().unwrap() = None;
            tokio::time::sleep(delay).await;
        }
        self.transcript_calls.lock().await.push(session_id.to_string());
        Ok(transcript)
    }

    /// No scripted session diff yet: the mock answers V1's `Ok(None)` arm,
    /// which is a no-entry Turn Diff (ADR-0087).
    ///
    /// [`Self::given_session_diff`] switches the mock to the V2 arm and names
    /// the diff the read serves.
    async fn session_diff(
        &self,
        session_id: &str,
        from: Option<&str>,
        to: Option<&str>,
        context: Option<u32>,
    ) -> crate::error::Result<Option<crate::backend::TurnDiff>> {
        self.session_diff_calls.lock().await.push((
            session_id.to_string(),
            from.map(str::to_string),
            to.map(str::to_string),
            context,
        ));
        if self.hang_session_diff.load(std::sync::atomic::Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        if self.fail_session_diff.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(crate::error::BridgeError::OpenCode(
                "session diff failed: 503".to_string(),
            ));
        }
        if !self
            .session_diff_available
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(None);
        }
        Ok(Some(
            self.session_diff
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .unwrap_or_default(),
        ))
    }

    async fn list_permissions(
        &self,
        _d: Option<&str>,
    ) -> crate::error::Result<Vec<crate::backend::types::PermissionRequest>> {
        self.list_permission_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        hang_if_scripted(&self.hang_list_permissions).await;
        if self
            .fail_list_permissions
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(crate::error::BridgeError::OpenCode(
                "permission list failed: 500 Internal Server Error".into(),
            ));
        }
        let replied = self.replied_permissions.lock().await;
        let mut out: Vec<_> = self
            .permissions
            .iter()
            .filter(|p| !replied.contains(&p.request_id))
            .cloned()
            .collect();
        // Post-build scripting: a test may add requests AFTER the app is built
        // (simulating a request arriving after the snapshot was sent).
        out.extend(self.extra_permissions.lock().await.iter().cloned());
        Ok(out)
    }

    async fn list_questions(
        &self,
        _d: Option<&str>,
    ) -> crate::error::Result<Vec<crate::backend::types::QuestionRequest>> {
        self.list_question_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        hang_if_scripted(&self.hang_list_questions).await;
        let replied = self.replied_questions.lock().await;
        Ok(self
            .questions
            .iter()
            .filter(|q| !replied.contains(&q.id))
            .cloned()
            .collect())
    }

    async fn model_context_window(&self, _provider: &str, _model: &str) -> crate::error::Result<Option<i64>> {
        self.context_window_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.context_window)
    }

    fn configured_default_model(&self) -> Option<crate::backend::types::ModelInfo> {
        self.default_model.clone()
    }

    async fn list_agents(&self) -> Vec<crate::backend::types::AgentInfo> {
        self.agents.clone()
    }

    async fn list_skills(&self, directory: Option<&str>) -> Vec<crate::backend::SkillInfo> {
        self.list_skills_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.list_skills_directories
            .lock()
            .await
            .push(directory.map(str::to_string));
        self.skills.clone()
    }

    async fn list_models(&self) -> Vec<crate::backend::types::ProviderModels> {
        self.provider_models.clone()
    }

    async fn model_surface(
        &self,
        query: crate::backend::types::ModelSurfaceQuery<'_>,
    ) -> crate::backend::types::ModelSurface {
        self.model_surface_queries
            .lock()
            .await
            .push(ModelSurfaceQueryRecord {
                scope: query.scope.map(str::to_string),
                selected: query.selected.map(str::to_string),
                catalog_only: query.catalog_only,
                ack_budget: query.ack_budget,
            });
        if let Some(surface) = &self.model_surface {
            // A client's declared shape, with the caller's current selection
            // filled in the way both real adapters do.
            let mut surface = surface.clone();
            if surface.current.is_none() {
                surface.current = query.current.clone();
            }
            return surface;
        }
        // The default mock speaks OpenCode's two-page tree so the `/model`
        // regression tests can keep seeding `provider_models`.
        let providers = &self.provider_models;
        if let (Some("provider"), Some(provider)) = (query.scope, query.selected) {
            let models = providers
                .iter()
                .find(|p| p.provider == provider)
                .map(|p| p.models.as_slice())
                .unwrap_or_default();
            let options: Vec<crate::backend::types::ModelOption> = models
                .iter()
                .map(|model| crate::backend::types::ModelOption {
                    id: model.id.clone(),
                    variants: model.variants.clone(),
                    value: Some(format!("{provider}/{}", model.id)),
                    ..Default::default()
                })
                .collect();
            return crate::backend::types::ModelSurface {
                current: query.current.clone(),
                scopes: vec![crate::backend::types::ModelScope {
                    id: "model".into(),
                    title: provider.to_string(),
                    changeable: crate::backend::types::ChangeScope::ThisTurn,
                    pick: crate::backend::types::PickKind::Model,
                    options: options.clone(),
                    note: None,
                    free_text: None,
                    intro: Some(if options.is_empty() {
                        "_(该 provider 没有可用模型)_".to_string()
                    } else {
                        "**选择 model**（下一条消息开始生效）：".to_string()
                    }),
                }],
            };
        }
        let options: Vec<crate::backend::types::ModelOption> = providers
            .iter()
            .map(|p| crate::backend::types::ModelOption::new(p.provider.clone()))
            .collect();
        let intro = if options.is_empty() {
            "_(没有可用模型)_".to_string()
        } else {
            format!("**选择 provider**（共 {} 个）：", options.len())
        };
        crate::backend::types::ModelSurface {
            current: query.current.clone(),
            scopes: vec![crate::backend::types::ModelScope {
                id: "provider".into(),
                title: "选择模型".into(),
                changeable: crate::backend::types::ChangeScope::ThisTurn,
                pick: crate::backend::types::PickKind::DrillDown,
                options,
                note: None,
                free_text: None,
                intro: Some(intro),
            }],
        }
    }

    async fn is_cola_authored(&self, message_id: &str) -> bool {
        self.authored_message_ids
            .lock()
            .expect("authored ids")
            .contains(message_id)
            || crate::backend::parsing::is_cola_message_id(message_id)
    }

    async fn reply_question(
        &self,
        _session_id: &str,
        request_id: &str,
        answers: &[crate::backend::types::FormAnswer],
        _d: Option<&str>,
    ) -> crate::error::Result<()> {
        let positional = answers
            .iter()
            .map(|answer| {
                answer
                    .value
                    .as_ref()
                    .map_or_else(Vec::new, crate::backend::types::FormValue::to_strings)
            })
            .collect();
        self.record_question_reply(request_id, positional, answers.to_vec())
            .await
    }

    async fn reject_question(
        &self,
        _session_id: &str,
        request_id: &str,
        _d: Option<&str>,
    ) -> crate::error::Result<()> {
        self.record_question_reply(request_id, vec![vec!["__reject__".to_string()]], Vec::new())
            .await
    }

    async fn reply_permission(
        &self,
        session_id: &str,
        r: &str,
        reply: &str,
        _d: Option<&str>,
    ) -> crate::error::Result<()> {
        self.reply_permission_sessions
            .lock()
            .await
            .push(session_id.to_string());
        self.reply_permission_calls
            .lock()
            .await
            .push((r.to_string(), reply.to_string()));
        if self.reply_permission_not_found {
            return Err(crate::error::BridgeError::NotFound(format!("permission {r}")));
        }
        if self
            .fail_reply_permission_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            self.fail_reply_permission_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(crate::error::BridgeError::OpenCode(
                "simulated permission reply failure".into(),
            ));
        }
        // Like the real server, a replied request leaves the pending list.
        self.replied_permissions.lock().await.insert(r.to_string());
        Ok(())
    }

    async fn session_info(
        &self,
        session_id: &str,
        _d: Option<&str>,
    ) -> crate::error::Result<crate::backend::types::SessionInfo> {
        self.session_info_calls.lock().await.push(session_id.to_string());
        hang_if_scripted(&self.hang_session_info).await;
        Ok(crate::backend::types::SessionInfo {
            id: session_id.to_string(),
            parent_id: self.session_parents.get(session_id).cloned(),
            title: self.session_titles.lock().unwrap().get(session_id).cloned(),
            model: self.session_model.clone(),
        })
    }

    async fn session_selection(
        &self,
        session_id: &str,
        _d: Option<&str>,
    ) -> crate::error::Result<Option<crate::backend::types::SessionSelection>> {
        if let Some(error) = &self.session_selection_error {
            return Err(crate::error::BridgeError::OpenCode(error.clone()));
        }
        if !self.durable_selection {
            return Ok(None);
        }
        Ok(self.session_selections.lock().await.get(session_id).cloned())
    }

    async fn session_last_run_model(
        &self,
        session_id: &str,
        _d: Option<&str>,
    ) -> crate::error::Result<Option<crate::backend::types::ModelInfo>> {
        if let Some(error) = &self.session_last_run_model_error {
            return Err(crate::error::BridgeError::OpenCode(error.clone()));
        }
        Ok(self.session_last_run_models.get(session_id).cloned())
    }

    fn keeps_session_selection(&self) -> bool {
        self.durable_selection
    }

    fn reuse_continues_an_admitted_turn(&self) -> bool {
        self.reuse_continues_an_admitted_turn
    }

    fn resume_supported(&self) -> bool {
        self.resume_supported
    }

    async fn switch_session_model(
        &self,
        session_id: &str,
        model: &crate::backend::types::ModelInfo,
    ) -> crate::error::Result<()> {
        if let Some(error) = &self.switch_error {
            return Err(crate::error::BridgeError::OpenCode(error.clone()));
        }
        self.switch_model_calls
            .lock()
            .await
            .push((session_id.to_string(), model.clone()));
        self.session_selections
            .lock()
            .await
            .entry(session_id.to_string())
            .or_default()
            .model = Some(model.clone());
        Ok(())
    }

    async fn switch_session_agent(&self, session_id: &str, agent: &str) -> crate::error::Result<()> {
        if let Some(error) = &self.switch_error {
            return Err(crate::error::BridgeError::OpenCode(error.clone()));
        }
        self.switch_agent_calls
            .lock()
            .await
            .push((session_id.to_string(), agent.to_string()));
        self.session_selections
            .lock()
            .await
            .entry(session_id.to_string())
            .or_default()
            .agent = Some(agent.to_string());
        Ok(())
    }

    async fn session_status(
        &self,
        session_id: &str,
        _d: Option<&str>,
    ) -> crate::error::Result<Option<crate::backend::types::SessionStatus>> {
        self.session_status_reads
            .lock()
            .await
            .push(session_id.to_string());
        if let Some(err) = &self.session_status_error {
            return Err(crate::error::BridgeError::OpenCode(err.clone()));
        }
        if self
            .session_status_fails
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(crate::error::BridgeError::OpenCode(
                "simulated session_status failure".into(),
            ));
        }
        // Scripts the ADR-0028 busy→idle race: the FIRST read reports Busy
        // (the snapshot gather sees a mid-flight turn), later reads serve the
        // map (the follow's arm-time check sees the turn already finished).
        if self
            .status_busy_once
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(Some(crate::backend::types::SessionStatus::Busy));
        }
        // A scripted entry is served verbatim (`Some(None)` → unknown); a
        // missing key means idle (the server removes finished runs).
        Ok(match self.session_statuses.lock().await.get(session_id) {
            Some(status) => *status,
            None => Some(crate::backend::types::SessionStatus::Idle),
        })
    }

    /// Whether this mock publishes a Background Task surface (spec #860,
    /// #880): the scripted `task_surface` flag.
    fn background_tasks_supported(&self) -> bool {
        self.task_surface
    }

    /// The scripted runtime verdict (issue #454). Empty by default, so a test
    /// that does not care gets the same no-op reconciliation V1 would produce;
    /// a test scripts verdicts through [`Self::task_runtime`]'s `Arc`.
    async fn task_runtime(
        &self,
        session_id: &str,
        _directory: Option<&str>,
        shells: &[String],
        children: &[String],
    ) -> crate::error::Result<crate::backend::TaskRuntime> {
        self.task_runtime_calls.lock().await.push((
            session_id.to_string(),
            shells.to_vec(),
            children.to_vec(),
        ));
        hang_if_scripted(&self.hang_task_runtime).await;
        if self
            .fail_task_runtime_reads
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            self.fail_task_runtime_reads
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(crate::error::BridgeError::OpenCode(format!(
                "task runtime {session_id} failed: 500 Internal Server Error"
            )));
        }
        Ok(self
            .task_runtime
            .lock()
            .expect("the task-runtime lock is never poisoned")
            .clone())
    }

    /// The scripted child-evidence read (#591, issue #464). A child with no
    /// scripted evidence answers `Unfinished` — no evidence, exactly like V1's
    /// arm — so a test only scripts the suspects whose evidence it means to
    /// pin.
    async fn child_evidence(&self, session_id: &str) -> crate::error::Result<crate::backend::ChildEvidence> {
        self.child_evidence_calls
            .lock()
            .await
            .push(session_id.to_string());
        hang_if_scripted(&self.hang_child_evidence).await;
        if self
            .fail_child_evidence_reads
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            self.fail_child_evidence_reads
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(crate::error::BridgeError::OpenCode(format!(
                "child evidence {session_id} failed: 500 Internal Server Error"
            )));
        }
        Ok(self
            .child_evidence
            .lock()
            .await
            .get(session_id)
            .copied()
            .unwrap_or(crate::backend::ChildEvidence::Unfinished))
    }

    fn tasks(&self) -> &crate::bridge::tasks::TasksHandle {
        &self.overlay
    }

    /// The scripted output window (spec #588, #592), keyed by shell id; a
    /// missing key answers `Ok(None)` like a vanished record.
    async fn shell_output(
        &self,
        shell_id: &str,
        _directory: Option<&str>,
    ) -> crate::error::Result<Option<crate::backend::ShellOutputWindow>> {
        self.shell_output_calls.lock().await.push(shell_id.to_string());
        hang_if_scripted(&self.hang_shell_output).await;
        if self
            .fail_shell_output_reads
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            self.fail_shell_output_reads
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(crate::error::BridgeError::OpenCode(format!(
                "shell output {shell_id} failed: 500 Internal Server Error"
            )));
        }
        Ok(self
            .shell_outputs
            .lock()
            .expect("the shell-output lock is never poisoned")
            .get(shell_id)
            .cloned()
            .flatten())
    }

    async fn interrupt(&self, session_id: &str) -> crate::error::Result<()> {
        self.interrupt_calls.lock().await.push(session_id.to_string());
        self.recovery_ops
            .lock()
            .await
            .push(format!("interrupt:{session_id}"));
        if self
            .fail_interrupt_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            self.fail_interrupt_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(crate::error::BridgeError::OpenCode(
                self.fail_interrupt_message
                    .clone()
                    .unwrap_or_else(|| "Simulated interrupt failure".into()),
            ));
        }
        Ok(())
    }

    async fn resume(&self, session_id: &str) -> crate::error::Result<()> {
        self.resume_calls.lock().await.push(session_id.to_string());
        self.recovery_ops
            .lock()
            .await
            .push(format!("resume:{session_id}"));
        if self.fail_resume_count.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            self.fail_resume_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(crate::error::BridgeError::OpenCode(
                self.fail_resume_message
                    .clone()
                    .unwrap_or_else(|| "Simulated resume failure".into()),
            ));
        }
        Ok(())
    }
    async fn compact(&self, _s: &str) -> crate::error::Result<()> {
        self.compact_calls.lock().await.push(_s.to_string());
        Ok(())
    }
    fn base_url(&self) -> String {
        if self.serverless {
            String::new()
        } else {
            "http://mock".into()
        }
    }

    fn for_directory(self: Arc<Self>, directory: &str) -> Arc<dyn crate::backend::DirectoryBackend> {
        Arc::new(crate::backend::BackendDirectory::new(self, directory.to_string()))
    }
}

#[async_trait::async_trait]
impl crate::backend::ServerLifecycle for MockBackend {
    /// The mock has no runtime to discover: it reports the state a test set.
    /// An attached mock (the default) is "attached, nothing to do"; a
    /// serverless one reports serverless, so the Lazy Start / NoServer paths
    /// are reachable (the real adapter would start a server instead).
    async fn ensure_attached(
        &self,
        _request: crate::backend::EnsureAttached<'_>,
    ) -> crate::error::Result<crate::backend::AttachOutcome> {
        self.ensure_attached_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(if self.serverless {
            crate::backend::AttachOutcome::Serverless
        } else {
            crate::backend::AttachOutcome::Attached
        })
    }

    async fn attachment_status(&self) -> Option<crate::backend::lifecycle::AttachmentStatus> {
        self.attachment.clone()
    }

    async fn client_report(&self) -> Option<crate::backend::lifecycle::ClientReport> {
        self.client_report.clone()
    }

    async fn probe_turn(&self) -> crate::backend::lifecycle::TurnProbe {
        self.turn_probe
            .clone()
            .unwrap_or(crate::backend::lifecycle::TurnProbe::NotAttempted {
                detail: "the mock has no turn probe".to_string(),
            })
    }
}

/// The test Host every `test_config` app is claimed for (ADR-0035). Messages
/// built with [`incoming`] and no explicit requester act as this Principal.
pub(crate) const TEST_HOST: &str = "ou_test_host";

pub fn test_config(session_file: &std::path::Path) -> crate::config::Config {
    let cfg = crate::config::Config {
        opencode: crate::config::OpenCodeConfig {
            url: Some("http://localhost:1".into()),
            model: Some("test/model".into()),
            start_server: crate::config::ServerStartPolicy::Auto,
            generation: crate::config::GenerationOverride::Auto,
        },
        feishu: crate::config::FeishuConfig {
            app_id: "app".into(),
            app_secret: "secret".into(),
        },
        bridge: crate::config::BridgeConfig {
            session_file: session_file.to_path_buf(),
            access_file: crate::state::StateLayout::beside(session_file, "access.json"),
            work_dir: None,
            default_auto_accept: false,
            group_completion_notice: true,
            long_task_notice: false,
            instant_reminder: false,
            log_days: 14,
            default_client: None,
        },
        review: crate::config::ReviewConfig::default(),
        worktree: crate::config::WorktreeConfig::default(),
        clients: Default::default(),
    };
    // A test config comes claimed for TEST_HOST: the suite's apps are ready to
    // use. The gate tests build an unclaimed config explicitly.
    let mut access = crate::bridge::access::AccessList::load(&cfg.bridge.access_file);
    access.claim(TEST_HOST).unwrap();
    cfg
}

/// A config whose Access List does not exist — the bot is unclaimed.
pub fn test_config_unclaimed(session_file: &std::path::Path) -> crate::config::Config {
    let mut cfg = test_config(session_file);
    cfg.bridge.access_file = crate::state::StateLayout::beside(session_file, "access-unclaimed.json");
    cfg
}

/// The parts a real assistant turn produces: reasoning → tool → text.
pub fn realistic_parts() -> Vec<Part> {
    vec![
        Part::StepStart(StepStart),
        Part::Reasoning(ReasoningPart {
            text: "用户想让我分析目录。".into(),
            started_at: None,
        }),
        tool_part(
            "bash",
            "call_1",
            ToolStatus::Completed,
            serde_json::json!({ "command": "ls -la" }),
            "src/\nCargo.toml\n",
        ),
        Part::StepFinish(StepFinish {
            reason: FinishReason::ToolCalls,
        }),
        Part::StepStart(StepStart),
        Part::Text(TextPart {
            text: "当前目录有 src/ 和 Cargo.toml。".into(),
            started_at: None,
        }),
        Part::StepFinish(StepFinish {
            reason: FinishReason::Stop,
        }),
    ]
}

/// A prompt whose answer is far longer than one card's text budget, so it
/// must flow across continuation cards (no plain-text fallback anymore).
pub fn long_answer_parts() -> Vec<Part> {
    // 1200 × 6 chars = 7200 chars, above MAX_CARD_TEXT_CHARS (6000).
    let long_text = "很长的回答。".repeat(1200);
    vec![
        Part::StepStart(StepStart),
        Part::Text(TextPart {
            text: long_text,
            started_at: None,
        }),
        Part::StepFinish(StepFinish {
            reason: FinishReason::Stop,
        }),
    ]
}

/// Build a `ModelOption` with the given id and declared variants.
pub(crate) fn model_option(id: &str, variants: &[&str]) -> crate::backend::types::ModelOption {
    crate::backend::types::ModelOption {
        id: id.to_string(),
        variants: variants.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

pub(crate) async fn build_app(
    cfg: crate::config::Config,
    backend: MockBackend,
) -> (Arc<App>, Arc<RecordingPlatform>) {
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());
    // The Completion Notice's repaint delay (#650) is a production timing
    // constant, like the cadences `scripted_app_with` injects: the tests that pin
    // it store their own value, and every other notice test runs without the
    // wall-clock wait. The send is still spawned either way, so a test asserting
    // a notice must await it (`wait_for_notice`).
    app.completion_notice_delay_ms
        .store(0, std::sync::atomic::Ordering::Relaxed);
    (app, platform)
}

/// Build a plain text `IncomingMessage` for tests (no parent, no images).
/// `None` means "no explicit requester": the message acts as [`TEST_HOST`],
/// the Principal every `test_config` app admits. Use [`incoming_anonymous`]
/// for an identity-less payload.
pub(crate) fn incoming(
    message_id: String,
    chat_id: String,
    chat_type: String,
    thread_id: Option<String>,
    text: String,
    requester: Option<String>,
) -> crate::bridge::IncomingMessage {
    crate::bridge::IncomingMessage {
        message_id,
        chat_id,
        chat_type,
        thread_id,
        parent_id: None,
        text,
        images: vec![],
        files: vec![],
        omitted: vec![],
        requester_open_id: Some(requester.unwrap_or_else(|| TEST_HOST.to_string())),
    }
}

/// A plain text message with NO sender identity — the gate must fail closed.
pub(crate) fn incoming_anonymous(
    message_id: String,
    chat_id: String,
    chat_type: String,
    thread_id: Option<String>,
    text: String,
) -> crate::bridge::IncomingMessage {
    crate::bridge::IncomingMessage {
        message_id,
        chat_id,
        chat_type,
        thread_id,
        parent_id: None,
        text,
        images: vec![],
        files: vec![],
        omitted: vec![],
        requester_open_id: None,
    }
}

/// Drive a slash command through the coordinator's message entry point — the
/// production parse → gate → route path — instead of calling the dispatcher
/// directly. The routing is what a test at this level is about; the
/// dispatcher's own unit concerns live in `command.rs`. `text` must round-trip
/// through [`crate::bridge::command::parse_command`] to the same command the
/// test means.
pub(crate) async fn send_command(app: &Arc<App>, text: &str, message_id: &str) {
    send_command_in(
        app,
        text,
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        message_id,
        crate::config::ConversationKind::P2p,
    )
    .await;
}

/// [`send_command`] for an explicit conversation: the helper rebuilds the
/// `IncomingMessage` from `thread_key` and `kind`, so the coordinator derives
/// exactly the routing the direct call used to pass by hand. A
/// [`Topic`](crate::config::ConversationKind::Topic) carries `thread_key`'s
/// thread id; the other kinds are the chat's top level.
pub(crate) async fn send_command_in(
    app: &Arc<App>,
    text: &str,
    thread_key: crate::config::ThreadKey,
    message_id: &str,
    kind: crate::config::ConversationKind,
) {
    let chat_type = match kind {
        crate::config::ConversationKind::GroupLobby => "group",
        _ => "p2p",
    };
    let thread_id = match kind {
        crate::config::ConversationKind::Topic => Some(thread_key.thread_id.clone()),
        _ => None,
    };
    app.handle_message(incoming(
        message_id.into(),
        thread_key.chat_id,
        chat_type.into(),
        thread_id,
        text.into(),
        None,
    ))
    .await;
}

/// Dispatch a card action as the Host (ADR-0035). A real click carries the
/// clicking user's `open_id` on the value (the platform threads it in) and the
/// bridge's gate refuses a click without one, so tests that mean "the Host
/// clicked this" go through this helper instead of `App::handle_card_action`.
#[async_trait::async_trait]
pub(crate) trait HostCardAction {
    async fn host_action(&self, value: serde_json::Value)
    -> Option<crate::bridge::handler::CardActionResult>;
    /// Dispatch a rendered button value as if Feishu supplied the containing
    /// card's message id in callback context (it is not part of the card JSON).
    async fn host_action_on_message(
        &self,
        value: serde_json::Value,
        message_id: &str,
    ) -> Option<crate::bridge::handler::CardActionResult>;
}

#[async_trait::async_trait]
impl HostCardAction for Arc<App> {
    async fn host_action(
        &self,
        value: serde_json::Value,
    ) -> Option<crate::bridge::handler::CardActionResult> {
        let mut value = value;
        value["operator_open_id"] = serde_json::Value::String(TEST_HOST.to_string());
        self.handle_card_action(value).await
    }

    async fn host_action_on_message(
        &self,
        mut value: serde_json::Value,
        message_id: &str,
    ) -> Option<crate::bridge::handler::CardActionResult> {
        value["operator_open_id"] = serde_json::Value::String(TEST_HOST.to_string());
        value["open_message_id"] = serde_json::Value::String(message_id.to_string());
        self.handle_card_action(value).await
    }
}

/// Create a temp work dir, set it as the process cwd (sessions are created
/// in cwd, and tests must never operate in the cola repo) and return it.
/// The returned TempDir must stay alive for the test's duration.
pub(crate) fn test_work_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    dir
}

/// Run a git command in `dir`, panicking on failure (test setup helper).
pub(crate) fn git_in(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(["-C"])
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A temp git repo on branch `main` with one commit — a real work context
/// for the Turn Footer tests (ADR-0019). The returned TempDir must stay
/// alive for the test's duration.
pub(crate) fn git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git_in(dir.path(), &["init", "-b", "main"]);
    git_in(dir.path(), &["config", "user.email", "test@example.com"]);
    git_in(dir.path(), &["config", "user.name", "test"]);
    std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
    git_in(dir.path(), &["add", "a.txt"]);
    git_in(dir.path(), &["commit", "-m", "init"]);
    dir
}

/// Every visible text string on a card, in walk order: `content` fields
/// (markdown / plain_text, including nested panels) and `text` fields. Button
/// payloads (`value`) are not visible text and stay out, so an assertion on
/// rendered copy reads the card's structure instead of its JSON dump.
pub(crate) fn card_texts(card: &serde_json::Value) -> Vec<String> {
    fn walk(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, v) in map {
                    match (key.as_str(), v.as_str()) {
                        ("content" | "text", Some(s)) => out.push(s.to_string()),
                        _ => walk(v, out),
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for v in items {
                    walk(v, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(card, &mut out);
    out
}

/// A card's visible text joined by newlines — the structural counterpart of
/// `card.to_string()` for copy assertions.
pub(crate) fn card_text(card: &serde_json::Value) -> String {
    card_texts(card).join("\n")
}

/// Assert `needle` renders exactly `count` times across a split's two cards —
/// the outgoing card and its continuation — so a ledger fact (a completion
/// entry, a live row) can never be lost or duplicated across a handover
/// (ADR-0060, ticket #488). `count` is 0 for a fact that must not migrate.
pub(crate) fn assert_across_cards(cards: [&serde_json::Value; 2], needle: &str, count: usize) {
    let counts: Vec<usize> = cards
        .iter()
        .map(|card| card_text(card).matches(needle).count())
        .collect();
    assert_eq!(
        counts.iter().sum::<usize>(),
        count,
        "expected {needle:?} ×{count} across the outgoing/continuation cards, got {counts:?}"
    );
}

/// Every in-place PATCH the platform recorded for `message_id`, in call order —
/// so one card's own updates can be told apart from every other card's (the
/// harness serves one message id per send, so a scenario that must count
/// PATCHes per chain names each card itself).
pub(crate) async fn patches_to(platform: &RecordingPlatform, message_id: &str) -> Vec<serde_json::Value> {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::UpdateMessage {
                message_id: mid,
                card,
            } if mid == message_id => Some(card.clone()),
            _ => None,
        })
        .collect()
}

/// Which recorded card updates [`wait_for_card_update`] checks.
#[derive(Clone, Copy)]
pub(crate) enum CardUpdates {
    /// Any update ever recorded — a side effect that happens (or happened).
    Any,
    /// Only the newest update — the card's current settled state.
    Latest,
}

/// The overall deadline a test gives an observable outcome — a spawned
/// handler, a recorded card, a continuation. Generous on purpose: every wait
/// polls the fact it needs, so this bounds a genuine regression instead of
/// turning a loaded parallel suite into a flake.
pub(crate) const SETTLE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

/// Await a spawned handler by polling `is_finished`, not by racing a fixed
/// timeout: the very next poll after the work lands sees it, so the wait cannot
/// lose a race a longer timeout would merely postpone.
pub(crate) async fn wait_for_handler<T>(
    handle: tokio::task::JoinHandle<T>,
    label: &str,
    within: std::time::Duration,
) -> T {
    let deadline = tokio::time::Instant::now() + within;
    while !handle.is_finished() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{label} did not finish within {within:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    handle.await.expect("the handler joins")
}

/// Await `check` over every card the platform recorded — in-place updates and
/// new/reply sends — within `within`. [`wait_for_card_update`] sees updates
/// only; a continuation is a reply, so a wait for one must look at both.
///
/// A timeout names the headers that *were* recorded: if the expected outcome
/// never arrived, the failure says whether the card was missing, still live, or
/// carried different content, instead of only "timed out".
pub(crate) async fn wait_for_recorded_card(
    platform: &RecordingPlatform,
    label: &str,
    within: std::time::Duration,
    check: impl Fn(&serde_json::Value) -> bool,
) {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let mut cards = platform.updated_cards().await;
        cards.extend(platform.replied_cards().await);
        cards.extend(platform.replied_in_thread_cards().await);
        if cards.iter().any(&check) {
            return;
        }
        let headers: Vec<&str> = cards.iter().map(card_header).collect();
        assert!(
            tokio::time::Instant::now() < deadline,
            "{label} never appeared within {within:?}; recorded card headers: {headers:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// Await a recorded in-place card update satisfying `check`, or panic after
/// [`SETTLE_DEADLINE`]. Keeps a test on an observed side effect instead of a
/// fixed sleep; `updates` selects whether any update or only the newest one
/// counts.
pub(crate) async fn wait_for_card_update(
    platform: &RecordingPlatform,
    label: &str,
    updates: CardUpdates,
    check: impl Fn(&serde_json::Value) -> bool,
) {
    wait_for_card_update_within(platform, label, SETTLE_DEADLINE, updates, check).await;
}

/// [`wait_for_card_update`] with an explicit bound: a test that must wait out
/// real wall-clock time (a ledger minute turning over) stretches the default
/// 5 s without loosening every other caller's bound.
pub(crate) async fn wait_for_card_update_within(
    platform: &RecordingPlatform,
    label: &str,
    timeout: std::time::Duration,
    updates: CardUpdates,
    check: impl Fn(&serde_json::Value) -> bool,
) {
    let wait = async {
        loop {
            let cards = platform.updated_cards().await;
            let matched = match updates {
                CardUpdates::Any => cards.iter().any(&check),
                CardUpdates::Latest => cards.last().is_some_and(&check),
            };
            if matched {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(timeout, wait)
        .await
        .unwrap_or_else(|_| panic!("card never reached {label}"));
}

/// Wait until `backend` has served `session_id`'s transcript at least `n`
/// times — the proof-of-pass for a test whose flow (a restarted process's
/// Session Sync, the reap) reads a session the test itself never drives.
pub(crate) async fn wait_for_transcript_reads(backend: &Arc<MockBackend>, session_id: &str, n: usize) {
    let probe = async {
        loop {
            let reads = backend
                .transcript_calls
                .lock()
                .await
                .iter()
                .filter(|sid| sid.as_str() == session_id)
                .count();
            if reads >= n {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), probe)
        .await
        .unwrap_or_else(|_| panic!("the flow never read {session_id}'s transcript {n} times"));
}

/// Every button element on a card, in walk order (buttons nest in column sets
/// and action blocks) — for assertions on a button's `value` payload or label
/// without string-matching the card's JSON dump.
pub(crate) fn card_buttons(card: &serde_json::Value) -> Vec<&serde_json::Value> {
    fn walk<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Value>) {
        match value {
            serde_json::Value::Object(map) => {
                if map.get("tag").and_then(|t| t.as_str()) == Some("button") {
                    out.push(value);
                }
                for v in map.values() {
                    walk(v, out);
                }
            }
            serde_json::Value::Array(items) => {
                for v in items {
                    walk(v, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(card, &mut out);
    out
}

/// A card's header title, for Done/Streaming/topic assertions.
pub(crate) fn card_header(card: &serde_json::Value) -> &str {
    card["header"]["title"]["content"].as_str().unwrap_or("")
}

/// Whether any element anywhere on the card carries `tag` — elements nest in
/// panels, column sets and action blocks, so a top-level scan is not enough.
pub(crate) fn card_has_tag(card: &serde_json::Value, tag: &str) -> bool {
    fn walk(value: &serde_json::Value, tag: &str) -> bool {
        match value {
            serde_json::Value::Object(map) => {
                map.get("tag").and_then(|t| t.as_str()) == Some(tag) || map.values().any(|v| walk(v, tag))
            }
            serde_json::Value::Array(items) => items.iter().any(|v| walk(v, tag)),
            _ => false,
        }
    }
    walk(card, tag)
}

/// The last card the app flushed in place — the finalized card.
pub(crate) async fn final_card(platform: &RecordingPlatform) -> serde_json::Value {
    let calls = platform.calls.lock().await;
    calls
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { card, .. } => Some(card.clone()),
            _ => None,
        })
        .expect("the final card must be updated in place")
}

/// The most recent card the platform saw, whether updated in place or sent as
/// a split's continuation card.
pub(crate) async fn latest_card(platform: &RecordingPlatform) -> serde_json::Value {
    let calls = platform.calls.lock().await;
    calls
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { card, .. } | PlatformCall::ReplyCard { card, .. } => {
                Some(card.clone())
            }
            _ => None,
        })
        .expect("a card must have been sent")
}

/// The Supplement split's continuation: the ONE card the platform replied to
/// the supplement message, carrying the receipt line (ADR-0043). Panics when
/// the reply is missing or duplicated — a split sends exactly one
/// continuation, and a competing Turn would reply its own Loading card to the
/// same message. Analyzed under one lock on the recorded calls.
pub(crate) async fn supplement_continuation(
    platform: &RecordingPlatform,
    reply_to: &str,
) -> serde_json::Value {
    let calls = platform.calls.lock().await;
    let mut replies = calls.iter().filter_map(|c| match c {
        PlatformCall::ReplyCard { reply_to: mid, card } if mid == reply_to => Some(card.clone()),
        _ => None,
    });
    let card = replies
        .next()
        .unwrap_or_else(|| panic!("no Supplement continuation replied to {reply_to}: {calls:?}"));
    assert!(
        replies.next().is_none(),
        "exactly one card may reply to {reply_to} (a competing Turn would): {calls:?}"
    );
    assert!(
        card_text(&card).contains("📨 已收到补充"),
        "the continuation must carry the Supplement receipt: {card}"
    );
    card
}

/// Map `entry` as its thread's active session and persist the store — the
/// setup tests need instead of reaching into `app.sessions` directly.
pub(crate) async fn seed_entry(app: &Arc<App>, entry: crate::config::SessionEntry) {
    let mut store = app.sessions.lock().await;
    store.set_active(entry);
    store
        .persist()
        .expect("seed_entry: persisting the seeded store failed");
}

/// Seed a mapped session carrying the per-session overrides the re-adoption
/// tests assert on: `provider/model-a`, variant `high`, Auto-Accept on.
pub(crate) async fn seed_overridden_entry(
    app: &Arc<App>,
    thread_key: crate::config::ThreadKey,
    session_id: &str,
    directory: &str,
) {
    let mut entry = crate::config::SessionEntry::new(thread_key, session_id, directory);
    entry.model = Some("provider/model-a".into());
    entry.variant = Some("high".into());
    entry.auto_accept = true;
    seed_entry(app, entry).await;
}

/// Seed a Pending Session for a thread (ADR-0041): the `set_pending` write,
/// with the same persist-failure panic policy as [`seed_entry`].
pub(crate) async fn seed_pending(app: &Arc<App>, pending: crate::bridge::session::PendingEntry) {
    app.sessions
        .lock()
        .await
        .set_pending(pending)
        .expect("seed_pending: persisting the seeded store failed");
}

/// The directories `sessions.json` records for its Pending Sessions, read back
/// through JSON: a Windows path appears escaped in the file, so a raw
/// `contains(path)` would never match.
pub(crate) fn saved_pending_dirs(path: &std::path::Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(store) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    store["pending"]
        .as_array()
        .map(|pending| {
            pending
                .iter()
                .filter_map(|entry| entry["directory"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Seed the cover-title cache for `session_id` (the topic cover card's title
/// memory) — the `app.core` setup tests would otherwise do by hand.
pub(crate) async fn seed_cover_title(app: &Arc<App>, session_id: &str, title: &str) {
    app.core.cover_titles.lock().await.insert(
        session_id.into(),
        crate::bridge::core::CoverTitle {
            title: title.into(),
            model: None,
            pending: false,
        },
    );
}

/// Seed the chat_1 lobby thread's session with the default entry shape.
pub(crate) async fn seed_session(app: &Arc<App>, session_id: &str, directory: &str) {
    seed_entry(
        app,
        crate::config::SessionEntry {
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: session_id.into(),
            directory: directory.into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
}

/// #144 rig: the three live surfaces a pending request may own. The caller
/// seeds the one session (`ses_1`, `/work`) before passing these to
/// [`assert_failed_dir_keeps_surfaces`].
pub(crate) struct FailedDirSurfaces {
    /// The standalone card's request id and Message id.
    pub card_id: &'static str,
    pub card_message_id: &'static str,
    /// The inline request id, already seeded onto `inline_acc`.
    pub inline_id: &'static str,
    /// The snapshot host's Message id; the claim rides `claim`.
    pub snapshot_message_id: &'static str,
    /// The request inlined on the streaming card, seeded onto it by the rig.
    pub inline_request: crate::bridge::request::kind::PendingRequest,
    /// The request embedded (and claimed) by the snapshot card.
    pub claim: crate::bridge::request::kind::PendingRequest,
}

/// Seed `request` as an inline block on `session_id`'s card, kind-agnostically —
/// the #144 rig's fixture builder.
async fn seed_inline_request(
    cards: &crate::bridge::handles::CardsHandle,
    session_id: &str,
    request: &crate::bridge::request::kind::PendingRequest,
    directory: &str,
) {
    use crate::bridge::request::kind::PendingRequest;
    match request {
        PendingRequest::Permission(p) => {
            Turn::add_permission(cards, session_id, p, directory).await;
        }
        PendingRequest::Question(q) => {
            Turn::add_question(
                cards,
                session_id,
                q,
                directory,
                &vec![None; q.questions.len()],
                &vec![false; q.questions.len()],
            )
            .await;
        }
    }
}

/// #144: drive one hanging-list sweep and one successful-list sweep over the
/// three surfaces: the first must keep them all ("unknown must never be read
/// as resolved"), the second must still fire every cleanup. `flow` and `hang`
/// pick the kind; `surfaces` carries its kind-specific fixtures.
pub(crate) async fn assert_failed_dir_keeps_surfaces(
    app: &Arc<App>,
    flow: &crate::bridge::request::flow::RequestFlow,
    hang: &std::sync::atomic::AtomicUsize,
    platform: &Arc<RecordingPlatform>,
    surfaces: FailedDirSurfaces,
) {
    flow.list_timeout_ms
        .store(30, std::sync::atomic::Ordering::Relaxed);
    let claim_id = surfaces.claim.id().to_string();

    // One live surface per request, all owned by the failing directory /work.
    flow.sent_cards.lock().await.insert(
        surfaces.card_id.into(),
        crate::bridge::request::flow::SentCard {
            message_id: surfaces.card_message_id.into(),
            summary: "待处理的请求".into(),
            directory: "/work".into(),
            session_id: "ses_1".into(),
        },
    );
    let cards = app.core.cards_handle();
    Turn::seed_card(&cards, "ses_1", None).await;
    seed_inline_request(&cards, "ses_1", &surfaces.inline_request, "/work").await;
    app.core.snapshot_claims.lock().await.claim(
        surfaces.snapshot_message_id,
        "已接管",
        "标题",
        &crate::bridge::snapshot::SnapshotData {
            session_id: "ses_1".into(),
            directory: "/work".into(),
            status: None,
            waiting: false,
            pending: vec![surfaces.claim],
            pending_elsewhere: None,
            pending_overflow: crate::bridge::snapshot::PendingOverflow::None,
            tail: vec![],
            newest_user_anchor: None,
            newest_user_is_cola_authored: false,
        },
        None,
    );

    // The list call hangs: /work said nothing this sweep.
    hang.store(1, std::sync::atomic::Ordering::SeqCst);
    let mut seen = std::collections::HashSet::new();
    flow.sweep(&app.flow_handles(), &mut seen).await;

    assert!(
        flow.sent_cards.lock().await.contains_key(surfaces.card_id),
        "a failed list must not mark the standalone card stale"
    );
    assert!(
        inline_surface_live(app, surfaces.inline_id).await,
        "a failed list must not drop the inline section"
    );
    assert!(
        app.core.snapshot_claims.lock().await.contains(&claim_id),
        "a failed list must not drop the snapshot claim"
    );
    let first_calls = platform.calls.lock().await.clone();
    assert!(
        first_calls.iter().all(|c| !matches!(
            c,
            PlatformCall::UpdateMessage { message_id, .. }
                if message_id == surfaces.card_message_id
                    || message_id == surfaces.snapshot_message_id
        )),
        "a failed list must not re-render any surface: {first_calls:?}"
    );

    // The next sweep lists /work successfully with the requests gone: every
    // cleanup fires.
    flow.sweep(&app.flow_handles(), &mut seen).await;

    assert!(
        !flow.sent_cards.lock().await.contains_key(surfaces.card_id),
        "a successful list must still stale the gone card"
    );
    assert!(
        !inline_surface_live(app, surfaces.inline_id).await,
        "a successful list must still drop the gone inline section"
    );
    assert!(
        inline_handled_elsewhere_receipt_present(app).await,
        "a successful list must leave the gone inline block's receipt (#175)"
    );
    assert!(
        !app.core.snapshot_claims.lock().await.contains(&claim_id),
        "a successful list must still drop the gone claim"
    );
    let calls = platform.calls.lock().await.clone();
    assert!(
        calls.iter().any(|c| matches!(
            c,
            PlatformCall::UpdateMessage { message_id, card }
                if message_id == surfaces.card_message_id && card.to_string().contains("已处理")
        )),
        "the standalone card is marked stale: {calls:?}"
    );
    assert!(
        calls.iter().any(|c| matches!(
            c,
            PlatformCall::UpdateMessage { message_id, .. }
                if message_id == surfaces.snapshot_message_id
        )),
        "the snapshot is re-rendered without the block: {calls:?}"
    );
}

/// Whether any card still carries a LIVE inline block for `id` (either kind) —
/// the #144 rig's surface probe. A resolved block left its receipt tombstone in
/// the section, which is not live (ADR-0038).
async fn inline_surface_live(app: &Arc<App>, id: &str) -> bool {
    Turn::has_live_interaction(&app.cards_handle(), id).await
}

/// #175: whether any card carries an Interaction Receipt left by a sweep over a
/// vanished inline block — the #144 rig proves it lands once a SUCCESSFUL list
/// resolves the block.
async fn inline_handled_elsewhere_receipt_present(app: &Arc<App>) -> bool {
    Turn::has_receipt_prefix(&app.cards_handle(), "⏱ 已由其他客户端处理").await
}

// ===== Session discovery & adoption (ADR-0008) =====

/// A helper: a session in the shared store with the given title/dir/id.
pub(crate) fn list_session(
    id: &str,
    title: &str,
    directory: &str,
    updated: i64,
) -> crate::backend::types::SessionListInfo {
    crate::backend::types::SessionListInfo {
        id: id.into(),
        title: title.into(),
        directory: directory.into(),
        parent_id: None,
        agent: None,
        model: None,
        time: Some(crate::backend::types::SessionTime {
            created: updated,
            updated,
            archived: None,
        }),
    }
}

/// ADR-0028 re-switch scenario: `ses_own1`（本项目会话, /work/cola）is both
/// the thread's mapped active session and the shared store's only session,
/// so `/switch 本项目` takes the mapped-hit branch of `handle_switch`. The
/// backend arrives pre-scripted with the cell under test (cola/external
/// newest message, status, pendings).
pub(crate) async fn build_reeswitch_app(
    backend: MockBackend,
) -> (Arc<App>, Arc<RecordingPlatform>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = backend;
    backend.given_sessions(vec![list_session("ses_own1", "本项目会话", "/work/cola", 500)]);
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_own1".into(),
            directory: "/work/cola".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    (app, platform, dir)
}

// ===== ADR-0028 adopt-time pending claim (ticket 05) =====

/// A permission request helper for the claim tests.
pub(crate) fn perm_request(
    request_id: &str,
    session_id: &str,
    pattern: &str,
) -> crate::backend::types::PermissionRequest {
    crate::backend::types::PermissionRequest {
        request_id: request_id.into(),
        session_id: Some(session_id.into()),
        permission: Some("bash".into()),
        patterns: vec![pattern.into()],
        metadata: None,
        always: vec![],
        always_supported: true,
    }
}

// ===== Session-scoped log capture (ADR-0048) =====

/// An in-memory [`tracing_subscriber::fmt::MakeWriter`]: every formatted event
/// appends to one shared byte buffer. Cloneable because `make_writer` hands a
/// fresh writer to each event.
#[derive(Clone, Default)]
struct CaptureBuffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CaptureBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureBuffer {
    type Writer = CaptureBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The level the fmt layer rendered on a captured line — the token after the
/// timestamp (`INFO`, `DEBUG`, `WARN`, …). Lets a test pin a line's level,
/// which the capture's TRACE-wide filter would otherwise hide.
pub(crate) fn line_level(line: &str) -> &str {
    line.split_whitespace().nth(1).unwrap_or("")
}

/// The first captured line containing `needle` — the line an assertion is
/// about — or a panic dumping every captured line.
pub(crate) fn line_with<'a>(logs: &'a str, needle: &str) -> &'a str {
    logs.lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no captured line contains {needle:?}:\n{logs}"))
}

/// Assert that the captured line containing `needle` was rendered at `level`
/// and — unless `level` is INFO itself — that no same-`needle` line leaks at
/// INFO: the ADR-0048 policy keeps whole payload dumps and ack timings off
/// INFO, so a demoted line that is still emitted there is a regression. The
/// line is returned so the caller can additionally assert on its body.
pub(crate) fn assert_line_level<'a>(logs: &'a str, needle: &str, level: &str) -> &'a str {
    let line = line_with(logs, needle);
    assert_eq!(
        line_level(line),
        level,
        "the {needle:?} line must be rendered at {level}: {line}"
    );
    if level != "INFO" {
        assert!(
            !logs
                .lines()
                .any(|other| line_level(other) == "INFO" && other.contains(needle)),
            "the {needle:?} line must never be emitted at INFO:\n{logs}"
        );
    }
    line
}

/// How many captured lines contain `needle` and were rendered at `level` — the
/// count [`assert_line_level`] cannot express (a WARN-once policy is about how
/// many lines there are, and which level each carries).
pub(crate) fn level_count(logs: &str, needle: &str, level: &str) -> usize {
    logs.lines()
        .filter(|line| line_level(line) == level && line.contains(needle))
        .count()
}

/// Run `body` under a captured subscriber and return its output plus every log
/// line it emitted.
///
/// The subscriber writes production's plain-text, ANSI-free shape into an
/// in-memory buffer ([`CaptureBuffer`]) and is installed with
/// `tracing::subscriber::set_default`, i.e. THREAD-LOCAL: the global subscriber
/// is never touched, so tests stay safe under the harness's parallel threads.
/// No level filter is applied (production's `EnvFilter` default is `cola=info`),
/// so a test can also pin a line's level. Run the body on a current-thread
/// runtime (`#[tokio::test]`) — spawned tasks then share the capture thread and
/// their instrumented spans still land in the buffer.
pub(crate) async fn capture_logs<F, T>(body: F) -> (T, String)
where
    F: std::future::Future<Output = T>,
{
    let buffer = CaptureBuffer::default();
    // A second, sink-writing dispatcher, kept alive for the capture's duration.
    // What defeats the callsite interest cache is REGISTRATION, not TLS
    // installation: `Dispatch::new` registers the dispatch in tracing's
    // dispatcher registry and rebuilds every cached `Interest` — `set_default`
    // below registers this capture the same way. With two registered
    // dispatchers a callsite first hit by a parallel test's thread no longer
    // takes the `JustOne` shortcut (which consults that thread's own, empty,
    // default) and caches `always` instead of `never`.
    let _interest_keepalive = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::sink)
            .finish(),
    );
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(buffer.clone())
        .finish();

    let guard = tracing::subscriber::set_default(subscriber);
    let output = body.await;
    drop(guard);
    let text = String::from_utf8(buffer.0.lock().unwrap().clone()).expect("the fmt layer writes valid UTF-8");
    (output, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::backend::{MessageRole, Part, SessionTranscript};

    /// A scripted transcript is served as-is, so a test can describe cola's
    /// domain instead of the backend's wire format (spec #332).
    #[tokio::test]
    async fn mock_serves_a_scripted_transcript_as_is() {
        let mut mock = MockBackend::new(Vec::new());
        mock.given_transcript(
            "ses_x",
            vec![SessionTranscript::new(vec![typed_message(
                "msg_u1",
                MessageRole::User,
                Some(1000),
                vec![text_part("脚本化的问题")],
            )])],
        );
        let transcript_calls = std::sync::Arc::clone(&mock.transcript_calls);
        let backend: std::sync::Arc<dyn Backend> = std::sync::Arc::new(mock);

        let transcript = backend.transcript("ses_x").await.unwrap();

        assert_eq!(transcript.messages.len(), 1);
        assert_eq!(transcript.messages[0].text(), "脚本化的问题");
        assert_eq!(
            transcript_calls.lock().await.as_slice(),
            &["ses_x".to_string()],
            "the transcript read must be recorded for polls that wait on it"
        );
    }

    /// Without a script, the mock serves the default typed shape built from the
    /// scenario's parts.
    #[tokio::test]
    async fn mock_serves_its_default_typed_shape_when_no_transcript_is_scripted() {
        let mock = MockBackend::new(realistic_parts());
        let transcript_calls = std::sync::Arc::clone(&mock.transcript_calls);
        let backend: std::sync::Arc<dyn Backend> = std::sync::Arc::new(mock);

        let transcript = backend.transcript("ses_test").await.unwrap();
        assert_eq!(transcript_calls.lock().await.len(), 1, "the read is recorded");

        assert_eq!(transcript.messages.len(), 1);
        let message = &transcript.messages[0];
        assert_eq!(message.role, MessageRole::Assistant);
        assert_eq!(message.text(), "当前目录有 src/ 和 Cargo.toml。");
        assert!(
            message
                .parts
                .iter()
                .any(|part| matches!(part, Part::Tool(tool) if tool.identity.name == "bash")),
            "the typed tool part must round-trip: {:?}",
            message.parts
        );
    }
}
