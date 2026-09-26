//! V2 wire shapes for the session read surface (spec #364, S4a).
//!
//! V2's session payloads differ from V1's in ways the neutral DTOs cannot
//! express: every read is wrapped in a `{data: ...}` envelope, the directory
//! rides in `location.directory` (not a top-level `directory`), and the list
//! cursor is an opaque body field (`cursor.next`) instead of a response
//! header. The run state has no `retry` entry at all, so the strategy derives
//! it from the newest assistant message's `retry` field.
//!
//! These private shapes decode that wire into the generation-blind DTOs
//! ([`SessionListInfo`], [`SessionInfo`]); nothing here escapes the strategy
//! (ADR-0055). The V1 coupling guard's denylist never applies to V2 literals,
//! but generation-distinctive names still stay inside this module.
//!
//! The transcript decode proper is slice S4b's work: the assistant read here
//! only answers "is the last assistant scheduled for retry?".

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::Value;

use crate::opencode::types::{SessionInfo, SessionListInfo, SessionModel, SessionTime};

/// `{data: T}` — the envelope most V2 reads share (R2's "unwrap per route —
/// there is no single rule").
#[derive(Debug, Deserialize)]
pub(super) struct DataEnvelope<T> {
    pub(super) data: T,
}

/// `GET /api/session` — `{data: Session.Info[], cursor: {previous?, next?}}`.
#[derive(Debug, Deserialize)]
pub(super) struct SessionListPage {
    pub(super) data: Vec<RawSessionInfo>,
    #[serde(default)]
    pub(super) cursor: PageCursor,
}

/// The page cursor. It is generated for every non-empty page, so a client
/// following `cursor.next` always makes one final request that yields an empty
/// `data` and no `next` (the store's anchor is exclusive).
#[derive(Debug, Default, Deserialize)]
pub(super) struct PageCursor {
    #[serde(default)]
    pub(super) next: Option<String>,
}

/// One `Session.Info` as the list/get envelopes serialize it.
#[derive(Debug, Deserialize)]
pub(super) struct RawSessionInfo {
    pub(super) id: String,
    #[serde(rename = "parentID", default)]
    pub(super) parent_id: Option<String>,
    #[serde(default)]
    pub(super) title: Option<String>,
    #[serde(default)]
    pub(super) agent: Option<String>,
    #[serde(default)]
    pub(super) model: Option<Value>,
    #[serde(default)]
    pub(super) location: Option<RawLocation>,
    #[serde(default)]
    pub(super) time: Option<SessionTime>,
}

/// `Location.PublicRef` — the only location field the public session surfaces.
#[derive(Debug, Deserialize)]
pub(super) struct RawLocation {
    pub(super) directory: String,
}

impl RawSessionInfo {
    /// The neutral `/switch`/`/dir` row: V1's top-level `directory` becomes
    /// `location.directory`, and the optional title reads as empty (the same
    /// tolerance [`SessionListInfo`] has for a missing one).
    pub(super) fn into_list_info(self) -> SessionListInfo {
        SessionListInfo {
            id: self.id,
            title: self.title.unwrap_or_default(),
            directory: self
                .location
                .map(|location| location.directory)
                .unwrap_or_default(),
            parent_id: self.parent_id,
            agent: self.agent,
            model: self.model,
            time: self.time,
        }
    }

    /// The neutral parent-chain/effective-model read. A malformed `model` is a
    /// decode error, exactly as it would be for V1's direct deserialize.
    pub(super) fn into_session_info(self) -> serde_json::Result<SessionInfo> {
        Ok(SessionInfo {
            id: self.id,
            parent_id: self.parent_id,
            title: self.title,
            model: self
                .model
                .map(serde_json::from_value::<SessionModel>)
                .transpose()?,
        })
    }
}

/// `GET /api/session/active` — `{data: Record<SessionID, {type:"running"}>}`.
/// Only `running` exists; absence means inactive.
#[derive(Debug, Deserialize)]
pub(super) struct ActiveSessions {
    pub(super) data: HashMap<String, Value>,
}

impl ActiveSessions {
    /// `Some(true)` = the session is running, `Some(false)` = the server
    /// reported an active entry whose type cola does not know (never guessed),
    /// `None` = the session is absent (not active).
    pub(super) fn state(&self, session_id: &str) -> Option<bool> {
        self.data
            .get(session_id)
            .map(|entry| entry.get("type").and_then(Value::as_str) == Some("running"))
    }
}

/// One projected message, reduced to the fields the run-state read needs. The
/// full `content[]` decode is S4b.
#[derive(Debug, Deserialize)]
pub(super) struct RawMessage {
    #[serde(rename = "type", default)]
    pub(super) kind: Option<String>,
    #[serde(default)]
    pub(super) retry: Option<Value>,
}

/// `GET /api/session/{id}/message` — `{data: Session.Message.Info[], cursor}`.
#[derive(Debug, Deserialize)]
pub(super) struct MessagesPage {
    #[serde(default)]
    pub(super) data: Vec<RawMessage>,
}

impl MessagesPage {
    /// Whether the newest assistant message is scheduled for retry. The
    /// strategy requests `type=assistant&order=desc&limit=1`, so this is the
    /// latest assistant; the tolerant scan keeps the answer correct if a server
    /// ever ignores the filter (the first assistant is still "the newest one
    /// present"). An absent `retry` — including an explicit JSON null, which is
    /// how V2 serializes the cleared field — means no retry.
    pub(super) fn newest_assistant_retrying(&self) -> bool {
        self.data
            .iter()
            .find(|message| message.kind.as_deref() == Some("assistant"))
            .and_then(|message| message.retry.as_ref())
            .is_some_and(|retry| !retry.is_null())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_session_info_maps_location_parent_and_model_onto_the_neutral_dtos() {
        // A real `session.list`/`session.get` entry: directory in
        // `location.directory` (not V1's top-level field), `parentID`
        // camelCase, `model` a `Model.Ref`.
        let raw: RawSessionInfo = serde_json::from_value(serde_json::json!({
            "id": "ses_child",
            "parentID": "ses_parent",
            "projectID": "proj_x",
            "title": "子会话",
            "agent": "build",
            "model": {"id": "deepseek-v4-flash", "providerID": "opencode-go"},
            "location": {"directory": "/work/cola"},
            "time": {"created": 1, "updated": 2},
            "cost": 0.0,
        }))
        .unwrap();

        let list = raw.into_list_info();
        assert_eq!(list.id, "ses_child");
        assert_eq!(list.title, "子会话");
        assert_eq!(list.directory, "/work/cola");
        assert_eq!(list.parent_id.as_deref(), Some("ses_parent"));
        assert!(list.is_child());
        assert_eq!(list.model.as_ref().unwrap()["providerID"], "opencode-go");

        let raw: RawSessionInfo = serde_json::from_value(serde_json::json!({
            "id": "ses_child",
            "parentID": "ses_parent",
            "title": "子会话",
            "model": {"id": "deepseek-v4-flash", "providerID": "opencode-go"},
            "location": {"directory": "/work/cola"},
        }))
        .unwrap();
        let info = raw.into_session_info().unwrap();
        assert_eq!(info.parent_id.as_deref(), Some("ses_parent"));
        let model = info.model.expect("the model must survive the envelope");
        assert_eq!(model.provider_id, "opencode-go");
        assert_eq!(model.id, "deepseek-v4-flash");
    }

    /// A missing title/location reads empty, like every tolerant session row;
    /// a malformed model is a loud decode error, never a silently dropped
    /// field.
    #[test]
    fn raw_session_info_tolerates_missing_optionals_but_not_a_malformed_model() {
        let raw: RawSessionInfo = serde_json::from_value(serde_json::json!({"id": "ses_root"})).unwrap();
        let list = raw.into_list_info();
        assert_eq!(list.title, "");
        assert_eq!(list.directory, "");
        assert!(list.parent_id.is_none());

        let raw: RawSessionInfo = serde_json::from_value(serde_json::json!({
            "id": "ses_root",
            "model": {"id": "only-an-id"},
        }))
        .unwrap();
        assert!(raw.into_session_info().is_err());
    }

    #[test]
    fn active_state_distinguishes_running_unknown_and_absent() {
        let active: ActiveSessions =
            serde_json::from_value(serde_json::json!({"data": {"ses_run": {"type": "running"}}})).unwrap();
        assert_eq!(active.state("ses_run"), Some(true));
        assert_eq!(active.state("ses_gone"), None);

        let odd: ActiveSessions =
            serde_json::from_value(serde_json::json!({"data": {"ses_odd": {"type": "zombie"}}})).unwrap();
        assert_eq!(
            odd.state("ses_odd"),
            Some(false),
            "unknown is never guessed as running"
        );
    }

    #[test]
    fn newest_assistant_retrying_reads_the_retry_field_not_a_null_or_a_absence() {
        let page: MessagesPage = serde_json::from_value(serde_json::json!({
            "data": [{"type": "assistant", "retry": {"attempt": 2, "at": 5000, "error": {"message": "boom"}}}]
        }))
        .unwrap();
        assert!(page.newest_assistant_retrying());

        let cleared: MessagesPage = serde_json::from_value(serde_json::json!({
            "data": [{"type": "assistant", "retry": null}]
        }))
        .unwrap();
        assert!(!cleared.newest_assistant_retrying());

        let none: MessagesPage = serde_json::from_value(serde_json::json!({
            "data": [{"type": "assistant", "content": []}]
        }))
        .unwrap();
        assert!(!none.newest_assistant_retrying());

        let empty: MessagesPage = serde_json::from_value(serde_json::json!({"data": []})).unwrap();
        assert!(!empty.newest_assistant_retrying());
    }

    /// The list cursor is a body field: a missing `cursor` object and a cursor
    /// without `next` both mean "the end".
    #[test]
    fn page_cursor_reads_next_or_the_end() {
        let page: SessionListPage = serde_json::from_value(serde_json::json!({
            "data": [],
            "cursor": {"previous": "p", "next": "n"}
        }))
        .unwrap();
        assert_eq!(page.cursor.next.as_deref(), Some("n"));

        let no_cursor: SessionListPage = serde_json::from_value(serde_json::json!({"data": []})).unwrap();
        assert!(no_cursor.cursor.next.is_none());
    }
}
