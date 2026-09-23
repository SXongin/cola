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
