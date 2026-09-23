//! Session-scoped span constructors (ADR-0048).
//!
//! The Session is the unit of log retrieval: every flow that knows one enters
//! a `tracing` span whose `session` / `chat` / `topic` fields the default fmt
//! layer prefixes to each line inside it (`turn{session=ses_x chat=oc_x}: …`).
//! The span name must be a literal — the `tracing` macros build a static
//! callsite — so each flow gets its own constructor here, while the fields and
//! their recording live once in [`record_session_fields`].

use crate::config::ThreadKey;

/// The Turn's span: `session` and `chat` always, `topic` only when the
/// conversation lives inside a Topic (a lobby's thread id is its chat id —
/// that is not a topic).
///
/// `parent` is the span the trace hangs from: `Span::current().id()` for the
/// Turn's own span, `None` to root one. Rooting is for a span whose ambient
/// context is the WRONG one — the render poll (its own task, spawned under the
/// turn's span) and the fresh-session era after a recreate (spawned under the
/// stale turn's span): a default parent would print the whole chain twice on
/// every one of their lines (`turn{…}:turn{…}:`).
pub(crate) fn turn(session_id: &str, thread_key: &ThreadKey, parent: Option<tracing::Id>) -> tracing::Span {
    let span = tracing::info_span!(
        parent: parent,
        "turn",
        session = tracing::field::Empty,
        chat = tracing::field::Empty,
        topic = tracing::field::Empty,
    );
    record_session_fields(&span, Some(session_id), Some(thread_key));
    span
}

/// A pending request's span: everything one poll sweep does with a listed
/// permission/question — its `prepare` (auto-accept / remember), the re-host of
/// one that outlived its turn, and its card delivery — is retrievable by the
/// Session it belongs to. `chat`/`topic` ride along when the store maps that
/// session; a sub-task child is not mapped, so its span carries `session`
/// alone.
pub(crate) fn request(session_id: &str, thread_key: Option<&ThreadKey>) -> tracing::Span {
    let span = tracing::info_span!(
        "request",
        session = tracing::field::Empty,
        chat = tracing::field::Empty,
        topic = tracing::field::Empty,
    );
    record_session_fields(&span, Some(session_id), thread_key);
    span
}

/// A card action's span: entered once the click's Session is resolved, so
/// `action{session=ses_x chat=oc_x}: Permission reply sent…` lines line up with
/// the work the click triggered. Either field may be unknown (a Pending Session
/// has no id yet) and is then omitted from the prefix.
pub(crate) fn action(session_id: Option<&str>, thread_key: Option<&ThreadKey>) -> tracing::Span {
    let span = tracing::info_span!(
        "action",
        session = tracing::field::Empty,
        chat = tracing::field::Empty,
        topic = tracing::field::Empty,
    );
    record_session_fields(&span, session_id, thread_key);
    span
}

/// The external-message flow's span: the poll's per-Session body (the
/// observation, the notification and the renderer arming) and the reply-render
/// loop that streams the answer into the card. Rooted, because the render task
/// is spawned from inside another span — the poll's own, or the snapshot
/// adoption's — and a default parent would print the whole chain twice on every
/// one of its lines (`external{…}:external{…}:`).
pub(crate) fn external(session_id: &str, thread_key: Option<&ThreadKey>) -> tracing::Span {
    let span = tracing::info_span!(
        parent: None,
        "external",
        session = tracing::field::Empty,
        chat = tracing::field::Empty,
        topic = tracing::field::Empty,
    );
    record_session_fields(&span, Some(session_id), thread_key);
    span
}

/// A snapshot adoption's span: the gather's best-effort reads, the card build
/// and the post-send follow/claim decision are retrievable by the adopted
/// Session. A first adoption is not mapped to its Chat/Topic yet, so `chat`/
/// `topic` ride along only when the Session already has a mapping — a sub-task
/// child never does.
pub(crate) fn snapshot(session_id: &str, thread_key: Option<&ThreadKey>) -> tracing::Span {
    let span = tracing::info_span!(
        "snapshot",
        session = tracing::field::Empty,
        chat = tracing::field::Empty,
        topic = tracing::field::Empty,
    );
    record_session_fields(&span, Some(session_id), thread_key);
    span
}

/// A topic flow's span: the opening transaction, the post-turn cover sync and a
/// pending cover's retitle. A pending topic has no Session yet, so `session` is
/// optional and the span carries only what that moment knows — the Chat for a
/// fresh opening, Chat/Topic for a pending retitle.
pub(crate) fn topic(session_id: Option<&str>, thread_key: Option<&ThreadKey>) -> tracing::Span {
    let span = tracing::info_span!(
        "topic",
        session = tracing::field::Empty,
        chat = tracing::field::Empty,
        topic = tracing::field::Empty,
    );
    record_session_fields(&span, session_id, thread_key);
    span
}

/// The inbound message path's span: entered at receipt, where no Session exists
/// yet, so it carries `chat`/`topic` alone. The Turn's own span nests under it
/// once a Session is chosen, adding `session` to the trace.
pub(crate) fn message(thread_key: &ThreadKey) -> tracing::Span {
    let span = tracing::info_span!(
        "message",
        session = tracing::field::Empty,
        chat = tracing::field::Empty,
        topic = tracing::field::Empty,
    );
    record_session_fields(&span, None, Some(thread_key));
    span
}

/// The Chat/Topic a Session is mapped to — what a span built by a flow that
/// only knows the Session completes its `chat`/`topic` fields from. `None` when
/// the Session has no mapping (a first adoption, a sub-task child): the span
/// then carries `session` alone rather than guessing.
pub(crate) async fn thread_key_of(
    sessions: &crate::bridge::handles::SessionsHandle,
    session_id: &str,
) -> Option<ThreadKey> {
    sessions.thread_for_session(session_id).await
}

/// Record the three session fields on a span that declared them as
/// [`tracing::field::Empty`]. An unrecorded field is omitted from the fmt
/// prefix entirely — not printed empty — which is what the policy wants
/// (ADR-0048): `session`/`chat`/`topic` only when known. An empty id is
/// "unknown", not a value.
fn record_session_fields(span: &tracing::Span, session_id: Option<&str>, thread_key: Option<&ThreadKey>) {
    if let Some(id) = session_id.filter(|id| !id.is_empty()) {
        span.record("session", tracing::field::display(id));
    }
    if let Some(key) = thread_key.filter(|key| !key.chat_id.is_empty()) {
        span.record("chat", tracing::field::display(&key.chat_id));
        if !key.thread_id.is_empty() && key.thread_id != key.chat_id {
            span.record("topic", tracing::field::display(&key.thread_id));
        }
    }
}
