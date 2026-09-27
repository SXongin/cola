//! The generation-parameterized `Backend` conformance suite (spec #364,
//! "Testing Decisions"): one neutral scenario set driven through the real
//! adapter — its HTTP transport, the concrete [`OpenCodeBackend`], the selected
//! generation strategy — against each generation's fake payloads. The scenarios
//! assert only what the Bridge consumes; generation-distinctive wire details
//! stay in that generation's own wire suite.
//!
//! Each generation contributes a [`SessionCase`] from its own module
//! (`v1::conformance` / `v2::conformance`), so a V1 route literal never leaves
//! the V1 strategy (the coupling guard, spec #364 §1) and V1 retirement deletes
//! its case with the rest of the strategy. The payload *values* are shared via
//! [`SessionReadFixture`], [`TranscriptFixture`] and [`PromptFixture`], so the
//! two generations' fixtures cannot drift — only the wire spellings around them
//! differ.

use crate::opencode::client::OpenCodeBackend;
use crate::opencode::strategy::Generation;
use crate::test_http::TestHttpServer;

/// One generation's conformance case: which strategy to speak, and how to
/// mount that generation's fake routes. The suite grows a scenario per neutral
/// capability (reads today, writes as they land); each mount translates the
/// shared fixture values into the generation's own wire shapes.
pub(crate) struct SessionCase {
    pub(crate) generation: Generation,
    /// Whether this generation keeps the model/agent selection server-side
    /// (`true` on V2's session switches, `false` on V1, whose picks ride each
    /// prompt). Drives the session-selection conformance scenario.
    pub(crate) keeps_session_selection: bool,
    /// Mount the generation's fake session-read routes (a two-page list, one
    /// session get, the run-state reads), publishing the shared
    /// [`SessionReadFixture`] values in that generation's envelope shapes.
    pub(crate) mount: fn(&TestHttpServer, &SessionReadFixture),
    /// Mount the generation's fake transcript read, publishing the shared
    /// [`TranscriptFixture`] values in that generation's message shape.
    pub(crate) mount_transcript: fn(&TestHttpServer, &TranscriptFixture),
    /// Serve one recorded transcript response body verbatim on the
    /// generation's transcript route. A paginated generation also answers the
    /// follow-up cursor page with the empty end-of-list page, so a recorded
    /// body's real `cursor.next` is exercised rather than stripped out.
    pub(crate) mount_recorded_transcript: fn(&TestHttpServer, &str, &str),
    /// Mount the generation's prompt dispatch with one scripted assistant
    /// answer, publishing the shared [`PromptFixture`] values in that
    /// generation's wire shape. V1 answers the blocking request inline; V2's
    /// mount includes its wait endpoint and the follow-up transcript read the
    /// synchronous polyfill performs.
    pub(crate) mount_prompt: fn(&TestHttpServer, &PromptFixture),
    /// Mount the generation's permission/question-form routes for one pending
    /// request each, publishing the shared [`RequestFixture`] values in that
    /// generation's shape (V1's positional questions, V2's typed forms).
    pub(crate) mount_requests: fn(&TestHttpServer, &RequestFixture),
    /// Mount the generation's model/agent switch routes, where it has any: V2
    /// mounts a stateful `GET /api/session/{id}` + `POST .../model|agent` so a
    /// switch is observable in the following read; V1 mounts nothing (its
    /// switches are no-ops).
    pub(crate) mount_selection: fn(&TestHttpServer, &SessionReadFixture),
}

/// The neutral values both generations' session-read payloads publish. One
/// shared default keeps the per-generation mounts from carrying duplicate
/// literals that could drift; only the wire spellings around these values
/// differ, and they stay in the generation modules.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SessionReadFixture {
    pub(crate) newest: &'static str,
    pub(crate) child: &'static str,
    pub(crate) oldest: &'static str,
    pub(crate) parent: &'static str,
    pub(crate) title: &'static str,
    pub(crate) child_title: &'static str,
    pub(crate) other_title: &'static str,
    pub(crate) directory: &'static str,
    pub(crate) other_directory: &'static str,
    pub(crate) agent: &'static str,
    pub(crate) provider: &'static str,
    pub(crate) model: &'static str,
    pub(crate) idle: &'static str,
    pub(crate) busy: &'static str,
    pub(crate) retrying: &'static str,
}

impl Default for SessionReadFixture {
    fn default() -> Self {
        Self {
            newest: "ses_new",
            child: "ses_child",
            oldest: "ses_old",
            parent: "ses_parent",
            title: "新",
            child_title: "子会话",
            other_title: "旧",
            directory: "/work/cola",
            other_directory: "/work/other",
            agent: "build",
            provider: "opencode-go",
            model: "deepseek-v4-flash",
            idle: "ses_idle",
            busy: "ses_busy",
            retrying: "ses_retry",
        }
    }
}

/// The neutral values both generations' transcript payloads publish. One
/// shared default keeps the per-generation mounts from carrying duplicate
/// literals that could drift; only the wire spellings around these values
/// differ, and they stay in the generation modules.
///
/// The fixture describes one session with three assistant messages around one
/// user message: a previous turn's completed reply (before the anchor), an
/// in-flight step created before the anchor (the #310 case), and this turn's
/// completed reply. The turn projections and the tail are asserted on it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TranscriptFixture {
    pub(crate) session: &'static str,
    pub(crate) user_id: &'static str,
    pub(crate) user_text: &'static str,
    pub(crate) user_created_ms: i64,
    pub(crate) previous_id: &'static str,
    pub(crate) previous_text: &'static str,
    pub(crate) previous_created_ms: i64,
    pub(crate) previous_completed_ms: i64,
    pub(crate) inflight_id: &'static str,
    pub(crate) inflight_reasoning: &'static str,
    pub(crate) inflight_created_ms: i64,
    pub(crate) inflight_call_id: &'static str,
    pub(crate) assistant_id: &'static str,
    pub(crate) assistant_created_ms: i64,
    pub(crate) assistant_completed_ms: i64,
    pub(crate) assistant_reasoning: &'static str,
    pub(crate) assistant_text: &'static str,
    pub(crate) tool_call_id: &'static str,
    pub(crate) tool_command: &'static str,
    pub(crate) tool_output: &'static str,
    pub(crate) provider: &'static str,
    pub(crate) model: &'static str,
    pub(crate) input_tokens: i64,
    pub(crate) output_tokens: i64,
    pub(crate) reasoning_tokens: i64,
    pub(crate) cache_read_tokens: i64,
    pub(crate) cache_write_tokens: i64,
    pub(crate) total_tokens: i64,
}

impl Default for TranscriptFixture {
    fn default() -> Self {
        Self {
            session: "ses_transcript",
            user_id: "msg_cola_transcript",
            user_text: "第几个问题",
            user_created_ms: 1_700_000_000_000,
            previous_id: "msg_prev",
            previous_text: "上一个回合的回答",
            previous_created_ms: 1_699_999_999_000,
            previous_completed_ms: 1_699_999_999_500,
            inflight_id: "msg_inflight",
            inflight_reasoning: "还在思考",
            inflight_created_ms: 1_699_999_999_800,
            inflight_call_id: "call_inflight",
            assistant_id: "msg_answer",
            assistant_created_ms: 1_700_000_000_100,
            assistant_completed_ms: 1_700_000_000_300,
            assistant_reasoning: "先想想",
            assistant_text: "回答",
            tool_call_id: "call_transcript",
            tool_command: "echo transcript",
            tool_output: "transcript-output\n",
            provider: "opencode-go",
            model: "deepseek-v4-flash",
            input_tokens: 11,
            output_tokens: 7,
            reasoning_tokens: 3,
            cache_read_tokens: 5,
            cache_write_tokens: 2,
            total_tokens: 28,
        }
    }
}

impl SessionCase {
    /// The real adapter, pointed at the fake server and speaking this case's
    /// generation — production's construction with only the transport swapped
    /// for the no-proxy test one (ADR-0031).
    pub(crate) fn backend(&self, server: &TestHttpServer) -> OpenCodeBackend {
        let backend = OpenCodeBackend::with_generation(
            None,
            server.base_url(),
            Some("opencode"),
            Some("secret"),
            self.generation,
            None,
        );
        backend.disable_env_proxy(Some("opencode"), Some("secret"));
        backend
    }
}

/// The neutral values both generations' prompt mounts publish. The prompt
/// dispatch differs structurally — V1 answers the blocking request with the
/// assistant message inline, V2 admits, waits and reads the transcript — but
/// the neutral reply the Bridge consumes is the same.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PromptFixture {
    pub(crate) session: &'static str,
    pub(crate) message_id: &'static str,
    pub(crate) text: &'static str,
    pub(crate) answer_id: &'static str,
    pub(crate) answer_text: &'static str,
}

impl Default for PromptFixture {
    fn default() -> Self {
        Self {
            session: "ses_prompt",
            message_id: "msg_cola_prompt",
            text: "开始干活",
            answer_id: "msg_prompt_answer",
            answer_text: "干完了",
        }
    }
}

/// The neutral values both generations' permission/form payloads publish. The
/// permission request renames (`action`/`resources`/`save`) and the form model
/// (typed fields, keyed answers) differ per generation; the neutral outcome the
/// Bridge consumes is the same.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RequestFixture {
    pub(crate) session: &'static str,
    pub(crate) directory: &'static str,
    pub(crate) permission_id: &'static str,
    pub(crate) action: &'static str,
    pub(crate) resource: &'static str,
    pub(crate) form_id: &'static str,
    pub(crate) form_title: &'static str,
    pub(crate) field_key: &'static str,
    pub(crate) field_title: &'static str,
    pub(crate) field_question: &'static str,
    pub(crate) option_value: &'static str,
    pub(crate) option_label: &'static str,
}

impl Default for RequestFixture {
    fn default() -> Self {
        Self {
            session: "ses_requests",
            directory: "/work/cola",
            permission_id: "per_req",
            action: "shell",
            resource: "rm -rf *",
            form_id: "frm_req",
            form_title: "Questions",
            field_key: "q0",
            field_title: "目录",
            field_question: "选哪个目录？",
            option_value: "/a",
            option_label: "目录 A",
        }
    }
}

/// Both generations under test. The suite grows a case per generation, never a
/// scenario per generation.
fn cases() -> [SessionCase; 2] {
    [
        crate::opencode::v1::conformance::case(),
        crate::opencode::v2::conformance::case(),
    ]
}

/// The session list has the same neutral outcome on both generations: every
/// page merged in server order, the directory exposed, and the parent chain the
/// `/switch`/`/sub` surfaces filter on.
#[tokio::test]
async fn list_sessions_yields_the_same_neutral_view_on_every_generation() {
    for case in cases() {
        let generation = case.generation.as_str();
        let fixture = SessionReadFixture::default();
        let server = TestHttpServer::start().await;
        (case.mount)(&server, &fixture);
        let backend = case.backend(&server);

        let sessions = backend
            .list_sessions()
            .await
            .unwrap_or_else(|e| panic!("{generation}: list_sessions failed: {e}"));

        let listed: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            listed,
            [fixture.newest, fixture.child, fixture.oldest],
            "{generation}: pages merged in order"
        );
        assert_eq!(
            sessions[0].directory, fixture.directory,
            "{generation}: directory"
        );
        assert_eq!(sessions[0].title, fixture.title, "{generation}: title");
        assert_eq!(
            sessions[0].agent.as_deref(),
            Some(fixture.agent),
            "{generation}: agent"
        );
        assert!(
            sessions[0].parent_id.is_none(),
            "{generation}: a root has no parent"
        );
        assert!(
            sessions[1].is_child(),
            "{generation}: the child keeps its parentID"
        );
        assert!(
            sessions[1].is_child_of(fixture.parent),
            "{generation}: parentID value"
        );
        assert_eq!(
            sessions[1].title, fixture.child_title,
            "{generation}: child title"
        );
        assert_eq!(
            sessions[2].directory, fixture.other_directory,
            "{generation}: a session in another directory keeps its own"
        );
        assert_eq!(
            sessions[2].title, fixture.other_title,
            "{generation}: the oldest row's title"
        );
        assert_eq!(
            sessions[0]
                .model
                .as_ref()
                .and_then(|m| m.get("providerID"))
                .and_then(|p| p.as_str()),
            Some(fixture.provider),
            "{generation}: the row's model survives for display"
        );
    }
}

/// The session get has the same neutral outcome on both generations: the
/// parent chain and the server-recorded model, which the effective-model
/// ladder and sub-task re-homing read.
#[tokio::test]
async fn session_info_exposes_parent_and_model_on_every_generation() {
    for case in cases() {
        let generation = case.generation.as_str();
        let fixture = SessionReadFixture::default();
        let server = TestHttpServer::start().await;
        (case.mount)(&server, &fixture);
        let backend = case.backend(&server);

        let info = backend
            .session_info(fixture.child, Some(fixture.directory))
            .await
            .unwrap_or_else(|e| panic!("{generation}: session_info failed: {e}"));

        assert_eq!(info.id, fixture.child, "{generation}: id");
        assert_eq!(
            info.parent_id.as_deref(),
            Some(fixture.parent),
            "{generation}: parent chain"
        );
        assert_eq!(
            info.title.as_deref(),
            Some(fixture.child_title),
            "{generation}: title"
        );
        let model = info.model.expect("the server-recorded model must parse");
        assert_eq!(
            model.provider_id, fixture.provider,
            "{generation}: model provider"
        );
        assert_eq!(model.id, fixture.model, "{generation}: model id");
    }
}

/// The run state has the same neutral outcome on both generations: Idle, Busy
/// and Retry. V2 derives Retry from the newest assistant message's `retry`
/// field where V1 reads it from the status map, but the Session Snapshot's
/// status line cannot tell them apart.
#[tokio::test]
async fn session_status_maps_idle_busy_retry_on_every_generation() {
    use crate::opencode::types::SessionStatus;

    for case in cases() {
        let generation = case.generation.as_str();
        let fixture = SessionReadFixture::default();
        let server = TestHttpServer::start().await;
        (case.mount)(&server, &fixture);
        let backend = case.backend(&server);

        for (session_id, expected, label) in [
            (fixture.idle, Some(SessionStatus::Idle), "idle"),
            (fixture.busy, Some(SessionStatus::Busy), "busy"),
            (fixture.retrying, Some(SessionStatus::Retry), "retry"),
        ] {
            let status = backend
                .session_status(session_id, Some(fixture.directory))
                .await
                .unwrap_or_else(|e| panic!("{generation}/{label}: session_status failed: {e}"));
            assert_eq!(status, expected, "{generation}/{label}: run state");
        }
    }
}

/// The transcript read has the same neutral outcome on both generations: the
/// server's order, the user anchor, the typed assistant content (reasoning,
/// tool, text), the model/token facts, and the two projections the Bridge's
/// read paths consume — the Turn (membership + completion) and the
/// recent-conversation tail. The generation-specific spellings (`parts` vs
/// `content[]`, `step-finish` vs `finish`) stay in the mounts.
#[tokio::test]
async fn transcript_reads_the_same_neutral_view_on_every_generation() {
    use crate::backend::{ContentBlock, MessageRole, Part, ToolStatus};

    for case in cases() {
        let generation = case.generation.as_str();
        let fixture = TranscriptFixture::default();
        let server = TestHttpServer::start().await;
        (case.mount_transcript)(&server, &fixture);
        let backend = case.backend(&server);

        let transcript = backend
            .transcript(fixture.session)
            .await
            .unwrap_or_else(|e| panic!("{generation}: transcript failed: {e}"));

        let ids: Vec<&str> = transcript.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                fixture.previous_id,
                fixture.inflight_id,
                fixture.user_id,
                fixture.assistant_id
            ],
            "{generation}: server order"
        );
        assert_eq!(
            transcript
                .messages
                .iter()
                .filter(|m| m.role == MessageRole::User)
                .count(),
            1,
            "{generation}: one user message"
        );
        assert_eq!(
            transcript
                .messages
                .iter()
                .filter(|m| m.role == MessageRole::Assistant)
                .count(),
            3,
            "{generation}: three assistant messages"
        );

        // The user message anchors the Turn: identity together with its server
        // time, and the text verbatim.
        let user = transcript.newest_user().expect("a user message exists");
        assert_eq!(user.id.as_str(), fixture.user_id, "{generation}: anchor id");
        assert_eq!(user.text(), fixture.user_text, "{generation}: anchor text");
        let anchor = user.anchor().expect("the user message carries a server time");
        assert_eq!(anchor.created_ms, fixture.user_created_ms);

        // Turn membership: the previous completed reply is out, the in-flight
        // step created before the anchor and this Turn's reply are in, and the
        // terminal finish completes the Turn.
        let turn = transcript.turn_for_user(&anchor);
        let turn_ids: Vec<&str> = turn.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            turn_ids,
            [fixture.inflight_id, fixture.assistant_id],
            "{generation}: turn membership"
        );
        assert!(
            turn.complete,
            "{generation}: the terminal finish completes the turn"
        );

        // The in-flight step has no completion stamp and a live tool.
        let inflight = transcript
            .messages
            .iter()
            .find(|m| m.id.as_str() == fixture.inflight_id)
            .expect("the in-flight message is present");
        assert!(
            inflight.time.unwrap().completed.is_none(),
            "{generation}: an in-flight step has no completion stamp"
        );
        let live = inflight
            .parts
            .iter()
            .find_map(|part| match part {
                Part::Tool(call) => Some(call),
                _ => None,
            })
            .expect("the in-flight tool call decodes");
        assert_eq!(live.identity.call_id, fixture.inflight_call_id);
        assert!(live.status.is_live(), "{generation}: the call is still live");

        // The completed answer's typed facts, in content order.
        let answer = transcript
            .messages
            .iter()
            .find(|m| m.id.as_str() == fixture.assistant_id)
            .expect("the answer is present");
        assert_eq!(
            answer
                .model
                .as_ref()
                .expect("the answer names its model")
                .provider_id,
            fixture.provider
        );
        assert_eq!(answer.model.as_ref().unwrap().model_id, fixture.model);
        let tokens = answer.tokens.expect("the answer reports usage");
        assert_eq!(tokens.total, fixture.total_tokens, "{generation}: neutral total");
        assert_eq!(
            tokens.context_used(),
            fixture.total_tokens,
            "{generation}: the footer's context figure"
        );

        let reasoning: Vec<&str> = answer
            .parts
            .iter()
            .filter_map(|part| match part {
                Part::Reasoning(reasoning) => Some(reasoning.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasoning,
            [fixture.assistant_reasoning],
            "{generation}: reasoning"
        );
        let texts: Vec<&str> = answer
            .parts
            .iter()
            .filter_map(|part| match part {
                Part::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, [fixture.assistant_text], "{generation}: answer text");
        let tool = answer
            .parts
            .iter()
            .find_map(|part| match part {
                Part::Tool(call) => Some(call),
                _ => None,
            })
            .expect("the settled tool call decodes");
        assert_eq!(tool.identity.call_id, fixture.tool_call_id);
        assert_eq!(tool.status, ToolStatus::Completed);
        assert_eq!(
            tool.input
                .as_ref()
                .and_then(|input| input.get("command"))
                .and_then(|v| v.as_str()),
            Some(fixture.tool_command),
            "{generation}: the tool input survives"
        );
        assert_eq!(
            tool.output.blocks,
            vec![ContentBlock::Text(fixture.tool_output.to_string())],
            "{generation}: the tool content decodes into one text block"
        );
        let position = |predicate: &dyn Fn(&Part) -> bool| answer.parts.iter().position(predicate);
        let reasoning_at = position(&|p| matches!(p, Part::Reasoning(_))).expect("reasoning position");
        let tool_at = position(&|p| matches!(p, Part::Tool(_))).expect("tool position");
        let text_at = position(&|p| matches!(p, Part::Text(_))).expect("text position");
        assert!(
            reasoning_at < tool_at && tool_at < text_at,
            "{generation}: content order is reasoning, tool, text"
        );

        // No text renders twice across the transcript (the content/ordinal
        // dedup the Turn's renderer applies would see each text exactly once).
        let rendered: Vec<&str> = transcript
            .messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(|part| match part {
                Part::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect();
        for expected in [fixture.previous_text, fixture.user_text, fixture.assistant_text] {
            assert_eq!(
                rendered.iter().filter(|text| **text == expected).count(),
                1,
                "{generation}: `{expected}` must appear exactly once: {rendered:?}"
            );
        }

        // The Session Snapshot tail: the text-bearing conversation, newest
        // last, with the tool-only in-flight step excluded.
        let tail = transcript.transcript_tail();
        let tail_texts: Vec<&str> = tail.iter().map(|entry| entry.text.as_str()).collect();
        assert_eq!(
            tail_texts,
            [fixture.previous_text, fixture.user_text, fixture.assistant_text],
            "{generation}: tail"
        );
        assert_eq!(tail[2].role, MessageRole::Assistant);
        assert_eq!(tail[2].created_ms, fixture.assistant_created_ms);
    }
}

/// The prompt dispatch has the same neutral outcome on both generations: the
/// blocking call returns the answer the turn produced, answers the admitted
/// user message, and surfaces no error. How the block is achieved (V1's native
/// blocking prompt vs V2's admit + wait + transcript read) stays in the mounts.
#[tokio::test]
async fn prompt_returns_the_turn_reply_on_every_generation() {
    use crate::backend::Part;

    for case in cases() {
        let generation = case.generation.as_str();
        let fixture = PromptFixture::default();
        let server = TestHttpServer::start().await;
        (case.mount_prompt)(&server, &fixture);
        let backend = case.backend(&server);

        let response = backend
            .prompt(
                fixture.session,
                fixture.text,
                &[],
                None,
                None,
                None,
                Some(fixture.message_id),
            )
            .await
            .unwrap_or_else(|e| panic!("{generation}: prompt failed: {e}"));

        assert_eq!(response.id, fixture.answer_id, "{generation}: answer id");
        assert_eq!(
            response.session_id.as_deref(),
            Some(fixture.session),
            "{generation}: session"
        );
        assert_eq!(
            response.parent_id.as_deref(),
            Some(fixture.message_id),
            "{generation}: the reply answers the admitted user message"
        );
        assert!(
            response.error.is_none(),
            "{generation}: a clean turn reports no error: {:?}",
            response.error
        );
        let texts: Vec<&str> = response
            .parts
            .iter()
            .filter_map(|part| match part {
                Part::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, [fixture.answer_text], "{generation}: reply text");
    }
}

/// The pending-permission read and reply have the same neutral outcome on both
/// generations: the request id, the owning session, the action name and its
/// resources reach the Bridge identically, and a decision round-trips.
#[tokio::test]
async fn permission_list_and_reply_round_trip_on_every_generation() {
    for case in cases() {
        let generation = case.generation.as_str();
        let fixture = RequestFixture::default();
        let server = TestHttpServer::start().await;
        (case.mount_requests)(&server, &fixture);
        let backend = case.backend(&server);

        let permissions = backend
            .list_permissions(Some(fixture.directory))
            .await
            .unwrap_or_else(|e| panic!("{generation}: list_permissions failed: {e}"));
        assert_eq!(permissions.len(), 1, "{generation}: one pending permission");
        assert_eq!(
            permissions[0].request_id, fixture.permission_id,
            "{generation}: id"
        );
        assert_eq!(
            permissions[0].session_id.as_deref(),
            Some(fixture.session),
            "{generation}: owning session"
        );
        assert_eq!(
            permissions[0].permission.as_deref(),
            Some(fixture.action),
            "{generation}: action"
        );
        assert_eq!(
            permissions[0].patterns,
            vec![fixture.resource],
            "{generation}: resources"
        );

        backend
            .reply_permission(
                fixture.session,
                fixture.permission_id,
                "once",
                Some(fixture.directory),
            )
            .await
            .unwrap_or_else(|e| panic!("{generation}: reply_permission failed: {e}"));
    }
}

/// The pending form/question read and its keyed reply have the same neutral
/// outcome on both generations: a field with an answer key, a typed kind and an
/// option whose submitted value is what a click sends back.
#[tokio::test]
async fn form_list_and_keyed_reply_round_trip_on_every_generation() {
    use crate::opencode::types::{FormAnswer, FormFieldKind, FormValue};

    for case in cases() {
        let generation = case.generation.as_str();
        let fixture = RequestFixture::default();
        let server = TestHttpServer::start().await;
        (case.mount_requests)(&server, &fixture);
        let backend = case.backend(&server);

        let forms = backend
            .list_questions(Some(fixture.directory))
            .await
            .unwrap_or_else(|e| panic!("{generation}: list_questions failed: {e}"));
        assert_eq!(forms.len(), 1, "{generation}: one pending form");
        let form = &forms[0];
        assert_eq!(form.id, fixture.form_id, "{generation}: form id");
        assert_eq!(form.session_id, fixture.session, "{generation}: owning session");
        assert_eq!(form.questions.len(), 1, "{generation}: one field");
        let field = &form.questions[0];
        assert_eq!(field.key, fixture.field_key, "{generation}: answer key");
        assert_eq!(
            field.kind,
            FormFieldKind::String,
            "{generation}: a question/string field"
        );
        assert!(
            !field.options[0].label.is_empty(),
            "{generation}: the option carries a display label"
        );
        assert_eq!(
            field.options[0].answer_value(),
            fixture.option_value,
            "{generation}: the option's submitted value"
        );

        let answers = vec![FormAnswer {
            key: fixture.field_key.to_string(),
            value: Some(FormValue::Text(fixture.option_value.to_string())),
        }];
        backend
            .reply_question(
                fixture.session,
                fixture.form_id,
                &answers,
                Some(fixture.directory),
            )
            .await
            .unwrap_or_else(|e| panic!("{generation}: reply_question failed: {e}"));
    }
}

/// Cancelling a pending question/form round-trips on both generations: V1
/// rejects, V2 deletes — the neutral caller sees one outcome.
#[tokio::test]
async fn form_cancel_round_trips_on_every_generation() {
    for case in cases() {
        let generation = case.generation.as_str();
        let fixture = RequestFixture::default();
        let server = TestHttpServer::start().await;
        (case.mount_requests)(&server, &fixture);
        let backend = case.backend(&server);

        backend
            .reject_question(fixture.session, fixture.form_id, Some(fixture.directory))
            .await
            .unwrap_or_else(|e| panic!("{generation}: reject_question failed: {e}"));
    }
}

/// The session-scoped selection is generation-dependent by design: a
/// generation with durable switches (V2) round-trips the model ref — variant
/// inside it — and the agent, while a generation whose picks ride each prompt
/// (V1) reports no durable selection and its switches are no-ops. The neutral
/// caller sees one shape either way: [`SessionSelection`].
#[tokio::test]
async fn session_switches_round_trip_only_where_the_generation_keeps_them() {
    use crate::opencode::types::ModelInfo;

    for case in cases() {
        let generation = case.generation.as_str();
        let fixture = SessionReadFixture::default();
        let server = TestHttpServer::start().await;
        (case.mount_selection)(&server, &fixture);
        let backend = case.backend(&server);

        backend
            .switch_session_model(
                fixture.newest,
                &ModelInfo {
                    id: fixture.model.into(),
                    provider_id: fixture.provider.into(),
                    variant: Some("high".into()),
                },
            )
            .await
            .unwrap_or_else(|e| panic!("{generation}: model switch failed: {e}"));
        backend
            .switch_session_agent(fixture.newest, fixture.agent)
            .await
            .unwrap_or_else(|e| panic!("{generation}: agent switch failed: {e}"));

        assert_eq!(
            backend.keeps_session_selection(),
            case.keeps_session_selection,
            "{generation}: the capability must match whether the scenario keeps a selection"
        );
        let selection = backend
            .session_selection(fixture.newest, Some(fixture.directory))
            .await
            .unwrap_or_else(|e| panic!("{generation}: selection read failed: {e}"));

        if case.keeps_session_selection {
            let selection = selection.unwrap_or_else(|| panic!("{generation}: a durable selection"));
            let model = selection.model.expect("the switched model survives");
            assert_eq!(model.provider_id, fixture.provider, "{generation}: provider");
            assert_eq!(model.id, fixture.model, "{generation}: model id");
            assert_eq!(
                model.variant.as_deref(),
                Some("high"),
                "{generation}: the variant rides the model ref"
            );
            assert_eq!(
                selection.agent.as_deref(),
                Some(fixture.agent),
                "{generation}: agent"
            );
        } else {
            assert!(
                selection.is_none(),
                "{generation}: a per-prompt generation keeps no durable selection"
            );
            assert!(
                server.requests().is_empty(),
                "{generation}: its switches must not reach the wire: {:?}",
                server.requests()
            );
        }
    }
}
