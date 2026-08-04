#![allow(dead_code)] // protocol definitions — field coverage matches Feishu API

use serde::Deserialize;

/// Feishu WebSocket event types we care about.
#[derive(Debug, Deserialize)]
pub struct WsMessage {
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

/// The inner data of an `im.message.receive_v1` event.
#[derive(Debug, Deserialize)]
pub struct MessageReceiveEvent {
    pub schema: Option<String>,
    pub header: Option<EventHeader>,
    pub event: Option<EventPayload>,
}

#[derive(Debug, Deserialize)]
pub struct EventHeader {
    pub event_id: Option<String>,
    pub event_type: Option<String>,
    pub create_time: Option<String>,
    pub token: Option<String>,
    pub app_id: Option<String>,
    pub tenant_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct EventPayload {
    pub sender: Option<MessageSender>,
    pub message: Option<MessageData>,
}

#[derive(Debug, Deserialize)]
pub struct MessageSender {
    pub sender_id: Option<SenderId>,
}

#[derive(Debug, Deserialize)]
pub struct SenderId {
    pub open_id: Option<String>,
    pub union_id: Option<String>,
    pub user_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MessageData {
    pub message_id: String,
    pub root_id: Option<String>,
    pub parent_id: Option<String>,
    pub chat_id: String,
    pub chat_type: String,
    pub message_type: String,
    pub content: String,
}

/// Parsed message content — varies by message_type.
#[derive(Debug, Deserialize)]
pub struct TextContent {
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic `im.message.receive_v1` payload as delivered by the Feishu
    /// WebSocket long-connection gateway (p2p text message).
    fn p2p_text_event() -> &'static str {
        r#"{
            "schema": "2.0",
            "header": {
                "event_id": "5e220b3b2e0f4e6ab8c5f0d7f2a6b123",
                "event_type": "im.message.receive_v1",
                "create_time": "1609295409000",
                "token": "test-token",
                "app_id": "cli_test",
                "tenant_key": "test-tenant"
            },
            "event": {
                "sender": {
                    "sender_id": {
                        "open_id": "ou_user_1",
                        "union_id": "on_user_1",
                        "user_id": "ua_user_1"
                    },
                    "sender_type": "user",
                    "tenant_key": "test-tenant"
                },
                "message": {
                    "message_id": "om_abc123",
                    "create_time": "1609295409000",
                    "chat_id": "oc_chat_1",
                    "chat_type": "p2p",
                    "message_type": "text",
                    "content": "{\"text\":\"hello world\"}"
                }
            }
        }"#
    }

    #[test]
    fn parses_p2p_text_message_event() {
        let event: MessageReceiveEvent = serde_json::from_str(p2p_text_event()).unwrap();

        let header = event.header.expect("header present");
        assert_eq!(header.event_type.as_deref(), Some("im.message.receive_v1"));
        assert_eq!(header.event_id.as_deref(), Some("5e220b3b2e0f4e6ab8c5f0d7f2a6b123"));
        assert_eq!(header.create_time.as_deref(), Some("1609295409000"));
        assert_eq!(header.app_id.as_deref(), Some("cli_test"));
        assert_eq!(event.schema.as_deref(), Some("2.0"));

        let payload = event.event.expect("event payload present");
        let sender = payload.sender.expect("sender present");
        let sender_id = sender.sender_id.expect("sender_id present");
        assert_eq!(sender_id.open_id.as_deref(), Some("ou_user_1"));
        assert_eq!(sender_id.union_id.as_deref(), Some("on_user_1"));
        assert_eq!(sender_id.user_id.as_deref(), Some("ua_user_1"));

        let msg = payload.message.expect("message present");
        assert_eq!(msg.message_id, "om_abc123");
        assert_eq!(msg.chat_id, "oc_chat_1");
        assert_eq!(msg.chat_type, "p2p");
        assert_eq!(msg.message_type, "text");
        assert_eq!(msg.root_id, None);
        assert_eq!(msg.parent_id, None);
    }

    #[test]
    fn parses_group_thread_message_event() {
        let json = r#"{
            "schema": "2.0",
            "header": {
                "event_id": "f0b2c3d4e5f60718293a4b5c6d7e8f90",
                "event_type": "im.message.receive_v1",
                "create_time": "1609295409000",
                "token": "test-token",
                "app_id": "cli_test",
                "tenant_key": "test-tenant"
            },
            "event": {
                "sender": {
                    "sender_id": { "open_id": "ou_user_2" },
                    "sender_type": "user"
                },
                "message": {
                    "message_id": "om_reply_1",
                    "root_id": "om_root_1",
                    "parent_id": "om_reply_0",
                    "create_time": "1609295409000",
                    "chat_id": "oc_group_1",
                    "chat_type": "group",
                    "message_type": "text",
                    "content": "{\"text\":\"hi\"}"
                }
            }
        }"#;

        let event: MessageReceiveEvent = serde_json::from_str(json).unwrap();

        let payload = event.event.expect("event payload present");
        let msg = payload.message.expect("message present");
        assert_eq!(msg.chat_type, "group");
        assert_eq!(msg.root_id.as_deref(), Some("om_root_1"));
        assert_eq!(msg.parent_id.as_deref(), Some("om_reply_0"));
        // Thread replies must be routed to the root message
        assert_eq!(
            msg.root_id.clone().unwrap_or_else(|| msg.message_id.clone()),
            "om_root_1"
        );
    }

    #[test]
    fn tolerates_missing_optional_fields() {
        // Minimal payload: only what MessageData requires (non-Option fields).
        let json = r#"{
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "message": {
                    "message_id": "om_min",
                    "chat_id": "oc_min",
                    "chat_type": "p2p",
                    "message_type": "text",
                    "content": "{\"text\":\"x\"}"
                }
            }
        }"#;

        let event: MessageReceiveEvent = serde_json::from_str(json).unwrap();
        assert!(event.schema.is_none());
        assert!(event.header.is_some());
        assert!(event.event.is_some());
        // sender is optional too
        assert!(event.event.as_ref().unwrap().sender.is_none());
        let msg = event.event.unwrap().message.unwrap();
        assert_eq!(msg.message_id, "om_min");
        assert_eq!(msg.root_id, None);
    }

    #[test]
    fn rejects_non_text_message_content() {
        // MessageData fields are required; a missing message_id should fail.
        let json = r#"{
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "message": {
                    "chat_id": "oc_min",
                    "chat_type": "p2p",
                    "message_type": "image",
                    "content": "{\"image_key\":\"img_1\"}"
                }
            }
        }"#;
        assert!(serde_json::from_str::<MessageReceiveEvent>(json).is_err());
    }
}
