//! V2's live tool progress: the event-stream overlay (issue #470).
//!
//! V2 persists a running tool part with EMPTY metadata. `context.progress`
//! publishes `session.tool.progress` on the server's bus, and the message
//! projection never folds it into the part — only the terminal events write
//! metadata back (the projection's own comment: terminal events are
//! self-contained and never reach into ephemeral progress history). A polled
//! `GET /api/session/{id}/message` read therefore cannot see a live `subagent`
//! call's child session id, and ADR-0054's liveness (plus Code Mode's live
//! nested rows) would have no data until the call settles.
//!
//! This module consumes `GET /api/event` (SSE) and keeps the latest progress
//! metadata per `(session, call)`; [`Progress::apply`] merges the cache into
//! the RUNNING tool parts of a transcript read, which is exactly the shape the
//! generations' decoders produce for a durable part. The stream is volatile by
//! its own contract (a slow consumer overflows; events during a disconnect are
//! missed), and the overlay is strictly additive: final metadata always arrives
//! through the polled message, so a lost stream degrades to no live line, never
//! to a wrong one.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;

use crate::backend::{Part, SessionTranscript};

use super::super::transport::Transport;

/// The server event stream (`GET /api/event`), SSE-framed.
const EVENT: &str = "/api/event";
/// Stop growing the cache this far past its live set — a safety valve for
/// missed terminal events, not a budget. Progress refills on the next event.
const MAX_ENTRIES: usize = 512;
/// Reconnect backoff after a stream end or failure.
const RECONNECT_MIN: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(10);

/// The live progress cache owned by one V2 strategy instance. Replacing the
/// strategy (reconnect) drops its cache; the reader task holds only a `Weak`,
/// so it exits at its next reconnect instead of outliving the attachment.
pub(crate) struct Progress {
    inner: Arc<Inner>,
}

struct Inner {
    /// `session -> call -> latest progress metadata`.
    entries: Mutex<HashMap<String, HashMap<String, Value>>>,
    /// One reader task per strategy instance.
    started: OnceLock<()>,
}

impl Progress {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                entries: Mutex::new(HashMap::new()),
                started: OnceLock::new(),
            }),
        }
    }

    /// Start the reader task on first use — it needs the live transport (whose
    /// client carries the auth headers). Without a tokio runtime there is no
    /// task; the cache stays empty and reads behave exactly as before.
    pub(crate) fn ensure_started(&self, transport: &Transport) {
        if self.inner.started.set(()).is_err() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let weak = Arc::downgrade(&self.inner);
        let transport = transport.clone();
        handle.spawn(async move { read_stream(weak, transport).await });
    }

    /// Merge the cached progress into every RUNNING tool part of `transcript`.
    /// Settled parts keep their durable metadata; a part with no cached entry
    /// is left untouched.
    pub(crate) fn apply(&self, session_id: &str, transcript: &mut SessionTranscript) {
        let entries = self.inner.entries.lock().expect("progress cache lock");
        let Some(calls) = entries.get(session_id) else {
            return;
        };
        if calls.is_empty() {
            return;
        }
        for message in &mut transcript.messages {
            for part in &mut message.parts {
                let Part::Tool(call) = part else {
                    continue;
                };
                if !call.status.is_live() {
                    continue;
                }
                let Some(progress) = calls.get(&call.identity.call_id) else {
                    continue;
                };
                call.metadata = Some(merge_metadata(call.metadata.take(), progress));
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn cached(&self, session_id: &str, call_id: &str) -> Option<Value> {
        self.inner
            .entries
            .lock()
            .expect("progress cache lock")
            .get(session_id)?
            .get(call_id)
            .cloned()
    }
}

/// The reader loop: reconnect with capped backoff while the cache is alive.
/// A clean stream end (server restart, proxy) resets the backoff; the next
/// connect re-reads nothing (the stream is live-only), which is safe because
/// the polled message remains the source of truth.
async fn read_stream(inner: std::sync::Weak<Inner>, transport: Transport) {
    let mut backoff = RECONNECT_MIN;
    while inner.upgrade().is_some() {
        match consume(&inner, &transport).await {
            Ok(()) => backoff = RECONNECT_MIN,
            Err(error) => {
                tracing::debug!("tool progress stream: {error}; reconnecting");
                backoff = (backoff * 2).min(RECONNECT_MAX);
            }
        }
        tokio::time::sleep(backoff).await;
    }
}

/// One connection: read SSE lines and absorb every `data:` payload.
async fn consume(inner: &std::sync::Weak<Inner>, transport: &Transport) -> Result<(), String> {
    let response = transport
        .client()
        .get(transport.url(EVENT))
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let mut stream = response.bytes_stream();
    let mut pending = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        pending.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(newline) = pending.find('\n') {
            let line: String = pending.drain(..=newline).collect();
            let line = line.trim_end_matches(['\n', '\r']);
            if let Some(data) = line.strip_prefix("data:")
                && let Some(inner) = inner.upgrade()
            {
                absorb(&inner, data.trim_start());
            }
        }
    }
    Ok(())
}

/// One event's payload into the cache: the latest `session.tool.progress`
/// metadata wins; terminal events drop their call; a session's idle drops the
/// session. Unknown events and unparseable payloads are ignored — the stream
/// carries far more than cola needs.
fn absorb(inner: &Inner, data: &str) {
    let Ok(event) = serde_json::from_str::<Value>(data) else {
        return;
    };
    let Some(kind) = event.get("type").and_then(Value::as_str) else {
        return;
    };
    let data = event.get("data");
    let session = data
        .and_then(|data| data.get("sessionID"))
        .and_then(Value::as_str);
    let call = data.and_then(|data| data.get("id")).and_then(Value::as_str);
    let mut entries = inner.entries.lock().expect("progress cache lock");
    match kind {
        "session.tool.progress" => {
            let (Some(session), Some(call), Some(metadata)) =
                (session, call, data.and_then(|data| data.get("metadata")))
            else {
                return;
            };
            if entries.values().map(HashMap::len).sum::<usize>() >= MAX_ENTRIES {
                entries.clear();
            }
            entries
                .entry(session.to_string())
                .or_default()
                .insert(call.to_string(), metadata.clone());
        }
        "session.tool.success" | "session.tool.failed" => {
            let (Some(session), Some(call)) = (session, call) else {
                return;
            };
            if let Some(calls) = entries.get_mut(session) {
                calls.remove(call);
            }
        }
        "session.idle" => {
            if let Some(session) = session {
                entries.remove(session);
            }
        }
        _ => {}
    }
}

/// Merge one progress snapshot over a running part's persisted metadata. Both
/// sides are objects in practice (the running part persists `{}`); a non-object
/// persisted value is never clobbered.
fn merge_metadata(base: Option<Value>, progress: &Value) -> Value {
    match (base, progress) {
        (Some(Value::Object(mut base)), Value::Object(extra)) => {
            for (key, value) in extra {
                base.insert(key.clone(), value.clone());
            }
            Value::Object(base)
        }
        (Some(base), _) => base,
        (None, progress) => progress.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(kind: &str, data: Value) -> String {
        json!({ "type": kind, "data": data }).to_string()
    }

    /// The exact live payload (captured from 2.0.18): a `session.tool.progress`
    /// for a foreground subagent names its child session.
    fn subagent_progress() -> String {
        event(
            "session.tool.progress",
            json!({
                "sessionID": "ses_parent",
                "assistantMessageID": "msg_1",
                "id": "call_1",
                "metadata": {"sessionID": "ses_child", "status": "running"},
            }),
        )
    }

    fn transcript_with_tool(
        name: &str,
        status: crate::backend::ToolStatus,
        metadata: Value,
    ) -> SessionTranscript {
        super::super::wire::decode_messages(&[json!({
            "id": "msg_1",
            "type": "assistant",
            "content": [{
                "type": "tool",
                "id": "call_1",
                "name": name,
                "state": {"status": status_spelling(&status), "input": {}, "metadata": metadata},
            }],
        })])
    }

    fn status_spelling(status: &crate::backend::ToolStatus) -> &'static str {
        use crate::backend::ToolStatus;
        match status {
            ToolStatus::Pending => "pending",
            ToolStatus::Running => "running",
            ToolStatus::Completed => "completed",
            ToolStatus::Error => "error",
            _ => "weird",
        }
    }

    fn tool_of(transcript: &SessionTranscript) -> &crate::backend::ToolCall {
        match &transcript.messages[0].parts[0] {
            Part::Tool(call) => call,
            other => panic!("expected a tool part: {other:?}"),
        }
    }

    /// The overlay's whole point: a running subagent part gets its child
    /// session id from the event stream, which is what ADR-0054's liveness and
    /// `execute`'s live rows read.
    #[test]
    fn a_progress_event_fills_a_running_tool_call() {
        let progress = Progress::new();
        absorb(&progress.inner, &subagent_progress());
        let mut transcript = transcript_with_tool("subagent", crate::backend::ToolStatus::Running, json!({}));
        progress.apply("ses_parent", &mut transcript);
        assert_eq!(
            tool_of(&transcript).metadata.as_ref().unwrap()["sessionID"],
            "ses_child"
        );
    }

    /// A settled call keeps its durable metadata: cached progress is live-only,
    /// and an entry that outlives its terminal event (a disconnect dropped it)
    /// must never overwrite a finished call.
    #[test]
    fn a_settled_tool_call_is_never_overlaid() {
        let progress = Progress::new();
        absorb(&progress.inner, &subagent_progress());
        let mut transcript = transcript_with_tool(
            "subagent",
            crate::backend::ToolStatus::Completed,
            json!({"sessionID": "ses_child", "status": "completed"}),
        );
        progress.apply("ses_parent", &mut transcript);
        let metadata = tool_of(&transcript).metadata.clone().unwrap();
        assert_eq!(metadata["status"], "completed");
    }

    /// The terminal events drop their call, so a later poll of a still-running
    /// part (an id the server reused — never, but the cache must not leak)
    /// stops contributing, and `session.idle` clears the session whole.
    #[test]
    fn terminal_and_idle_events_drop_entries() {
        let progress = Progress::new();
        absorb(&progress.inner, &subagent_progress());
        assert!(progress.cached("ses_parent", "call_1").is_some());

        let idle = event("session.idle", json!({"sessionID": "ses_parent"}));
        absorb(&progress.inner, &idle);
        assert!(progress.cached("ses_parent", "call_1").is_none());

        absorb(&progress.inner, &subagent_progress());
        let success = event(
            "session.tool.success",
            json!({"sessionID": "ses_parent", "assistantMessageID": "msg_1", "id": "call_1"}),
        );
        absorb(&progress.inner, &success);
        assert!(progress.cached("ses_parent", "call_1").is_none());
    }

    /// Other sessions' progress never leaks into this transcript, and an
    /// unrelated event type is ignored.
    #[test]
    fn unrelated_events_and_sessions_are_ignored() {
        let progress = Progress::new();
        absorb(&progress.inner, &subagent_progress());
        absorb(
            &progress.inner,
            &event(
                "session.tool.called",
                json!({"sessionID": "ses_parent", "id": "call_1", "metadata": {"sessionID": "ses_other"}}),
            ),
        );
        let mut transcript = transcript_with_tool("subagent", crate::backend::ToolStatus::Running, json!({}));
        progress.apply("ses_other", &mut transcript);
        assert!(
            tool_of(&transcript)
                .metadata
                .as_ref()
                .unwrap()
                .get("sessionID")
                .is_none(),
            "another session's cache must not overlay"
        );
    }

    /// End to end over a socket: the reader consumes a real SSE body (auth
    /// header included) and fills the cache.
    #[tokio::test]
    async fn the_event_stream_fills_the_cache_over_http() {
        let server = crate::test_http::TestHttpServer::start().await;
        server.route_raw(
            "GET",
            "/api/event",
            200,
            "text/event-stream",
            format!("data: {}\n\n: heartbeat\n\n", subagent_progress()),
        );
        let transport = Transport::new(Some("opencode"), Some("secret"), server.base_url());
        transport.disable_env_proxy(Some("opencode"), Some("secret"));
        let progress = Progress::new();
        progress.ensure_started(&transport);

        let mut cached = None;
        for _ in 0..200 {
            if let Some(value) = progress.cached("ses_parent", "call_1") {
                cached = Some(value);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let cached = cached.expect("the stream's progress event reaches the cache");
        assert_eq!(cached["sessionID"], "ses_child");
        assert!(
            server
                .requests()
                .iter()
                .any(|request| request.path == "/api/event" && request.header("authorization").is_some()),
            "the event stream must carry Basic auth"
        );
    }
}
