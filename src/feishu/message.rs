//! What a Feishu message means to cola: its readable text, the images it
//! embeds, and its @mentions. Pure content helpers plus the quoted-context
//! fetch that composes them; the WS transport lives in the sibling `ws`
//! module.

use crate::feishu::Platform;
use crate::feishu::event::{Mention, MessageData};
use std::sync::Arc;

/// The readable text of a message payload — shared by the live receive path and
/// the quoted-context fetch so "what is this message's text" is defined once.
/// Text messages carry `{"text": "..."}`; cards/interactive/post messages may
/// embed user text (extracted once, never duplicated); anything else degrades
/// to a per-type placeholder instead of leaking raw content JSON.
fn message_text(message_type: &str, content: &str) -> String {
    if message_type == "text"
        && let Ok(content) = serde_json::from_str::<serde_json::Value>(content)
    {
        return content["text"].as_str().unwrap_or("").to_string();
    }
    // Non-text messages can still be cards with user text inside (the client
    // sometimes wraps input in a card: {"title":"","content":[[{"tag":"text",
    // "text":"..."}]]}). Without extracting it, the raw JSON leaks into the
    // prompt and shows up verbatim on the OpenChamber side.
    if let Some(text) = extract_card_text(content) {
        return text;
    }
    // No readable text — never leak raw content JSON (`{"image_key":...}`) into
    // the prompt. A per-type placeholder tells the model what kind of message
    // it was instead of wasting tokens on opaque JSON.
    message_placeholder(message_type)
}

pub(crate) fn parse_message_content(msg: &MessageData) -> String {
    message_text(&msg.message_type, &msg.content)
}

/// A human-readable placeholder for a message with no extractable text,
/// replacing what would otherwise leak raw content JSON into the prompt.
fn message_placeholder(message_type: &str) -> String {
    let label = match message_type {
        "image" => "图片",
        "video" | "media" => "视频",
        "audio" => "语音",
        "file" => "文件",
        "sticker" => "表情",
        "share_chat" | "share_user" => "分享",
        "merged_forward" => "合并转发",
        _ => "其他消息",
    };
    format!("[{label}]")
}

/// The image keys embedded in a message's content: for `image` messages the
/// content is `{"image_key":"..."}`; for rich-text `post` messages elements
/// carry `tag: "img"` with an `image_key`. Empty when nothing is downloadable.
pub(crate) fn extract_image_keys(content: &str, message_type: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(content) else {
        return Vec::new();
    };
    if message_type == "image" {
        return v
            .get("image_key")
            .and_then(|k| k.as_str())
            .map(|k| vec![k.to_string()])
            .unwrap_or_default();
    }
    let mut out = Vec::new();
    collect_image_keys(&v, &mut out);
    out
}

fn collect_image_keys(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::Object(map) => {
            if map.get("tag").and_then(|t| t.as_str()) == Some("img")
                && let Some(k) = map.get("image_key").and_then(|k| k.as_str())
            {
                out.push(k.to_string());
            }
            for val in map.values() {
                collect_image_keys(val, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for val in arr {
                collect_image_keys(val, out);
            }
        }
        _ => {}
    }
}

/// Fetch + parse a message for quote injection (Quoted Context): fetch it by
/// id, extract its text (mention placeholders → names), and download any
/// images it embeds. The returned context is prepended to the reply's prompt so
/// the model sees what the reply answers — including when the parent is missing
/// from session history (lobby-session switch, compaction).
///
/// The bot's own @mention in the quoted text is not stripped (resolving the bot
/// id would add an API call per quote); a quoted bot mention keeps its display
/// name, which is fine for context.
pub(crate) async fn quoted_context(
    feishu: &Arc<dyn Platform>,
    message_id: &str,
) -> crate::error::Result<crate::feishu::client::MessageContext> {
    let msg = feishu.get_message(message_id).await?;
    let raw = message_text(&msg.msg_type, &msg.content);
    let mut ctx = crate::feishu::client::MessageContext {
        text: strip_mentions(&raw, &msg.mentions, ""),
        images: Vec::new(),
    };
    for key in extract_image_keys(&msg.content, &msg.msg_type) {
        match feishu.download_image(message_id, &key).await {
            Ok(img) => ctx.images.push(img),
            Err(e) => tracing::warn!("quoted context: download image {} failed: {}", key, e),
        }
    }
    Ok(ctx)
}

/// Extract the user-visible text from a card/interactive message JSON. The
/// client's card payload carries the SAME text in both `content` and `content_v2`
/// — walk only one of them (v2 is canonical), otherwise the text is collected
/// twice and the prompt gets a duplicated message. Returns None when there is
/// no readable text (e.g. an image message), so the caller falls back to raw.
fn extract_card_text(content: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(content).ok()?;
    let source = if v.get("content_v2").is_some() {
        v.get("content_v2")
    } else {
        v.get("content")
    }?;
    let mut out = String::new();
    collect_text_fields(source, &mut out);
    let text = out.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

fn collect_text_fields(v: &serde_json::Value, out: &mut String) {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(s)) = map.get("text") {
                out.push_str(s);
            }
            for val in map.values() {
                collect_text_fields(val, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for val in arr {
                collect_text_fields(val, out);
            }
        }
        _ => {}
    }
}

/// True when any mention targets the bot itself (`bot_open_id`). Used to know
/// whether a group message addressed cola directly.
pub(crate) fn is_mentioned(mentions: &[Mention], bot_open_id: &str) -> bool {
    mentions
        .iter()
        .any(|m| m.id.as_ref().and_then(|i| i.open_id.as_deref()) == Some(bot_open_id))
}

/// Replace @mention placeholders (`@_user_1`) in text with readable names:
/// the bot's own mention is dropped, other mentions become `@名字`, so the
/// AI sees who was referenced instead of opaque `@_user_N` tokens.
pub(crate) fn strip_mentions(text: &str, mentions: &[Mention], bot_open_id: &str) -> String {
    if mentions.is_empty() {
        return text.to_string();
    }
    let mut out = text.to_string();
    for m in mentions {
        let Some(key) = m.key.as_deref() else { continue };
        if !out.contains(key) {
            continue;
        }
        let is_bot = m.id.as_ref().and_then(|i| i.open_id.as_deref()) == Some(bot_open_id);
        let replacement = if is_bot {
            ""
        } else if let Some(name) = m.name.as_deref() {
            &format!("@{name}")
        } else {
            ""
        };
        out = out.replace(key, replacement);
    }
    out.trim().to_string()
}

/// Remove Feishu @mention placeholder tokens (`@_user_N`) from arbitrary text,
/// regardless of any mentions mapping. Used to clean stale session names that
/// were persisted before mention stripping existed (they would otherwise leak
/// `@_user_1` into the card header and `/list`).
pub(crate) fn strip_mention_tokens(text: &str) -> String {
    text.split_whitespace()
        .filter(|w| !w.starts_with("@_user_"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_text_message() {
        let msg = MessageData {
            message_id: "msg_1".into(),
            root_id: None,
            parent_id: None,
            thread_id: None,
            chat_id: "chat_1".into(),
            chat_type: "p2p".into(),
            message_type: "text".into(),
            content: r#"{"text": "hello world"}"#.into(),
            mentions: vec![],
        };
        assert_eq!(parse_message_content(&msg), "hello world");
    }

    #[test]
    fn parse_non_text_message() {
        let msg = MessageData {
            message_id: "msg_2".into(),
            root_id: Some("root_1".into()),
            parent_id: None,
            thread_id: None,
            chat_id: "chat_1".into(),
            chat_type: "group".into(),
            message_type: "image".into(),
            content: r#"{"image_key": "abc123"}"#.into(),
            mentions: vec![],
        };
        // Raw content JSON must never leak into the prompt — a placeholder.
        assert_eq!(parse_message_content(&msg), "[图片]");
    }

    #[test]
    fn placeholders_cover_each_media_type() {
        let cases = [
            ("image", "[图片]"),
            ("video", "[视频]"),
            ("media", "[视频]"),
            ("audio", "[语音]"),
            ("file", "[文件]"),
            ("sticker", "[表情]"),
            ("share_chat", "[分享]"),
            ("merged_forward", "[合并转发]"),
            ("unknown_type", "[其他消息]"),
        ];
        for (msg_type, expected) in cases {
            let msg = MessageData {
                message_id: "msg_m".into(),
                root_id: None,
                parent_id: None,
                thread_id: None,
                chat_id: "chat_1".into(),
                chat_type: "p2p".into(),
                message_type: msg_type.into(),
                content: r#"{"some_key":"xyz"}"#.into(),
                mentions: vec![],
            };
            assert_eq!(parse_message_content(&msg), expected, "for type {msg_type}");
        }
    }

    #[test]
    fn image_keys_extracted_for_image_and_post_messages() {
        // Standalone image message.
        let keys = extract_image_keys(r#"{"image_key":"img_a"}"#, "image");
        assert_eq!(keys, vec!["img_a"]);

        // Post (rich text) with text + image elements — the "screenshot +
        // caption" composer shape.
        let post = r#"{"title":"","content":[[{"tag":"text","text":"看看这个报错"}],[{"tag":"img","image_key":"img_b"}]]}"#;
        assert_eq!(extract_image_keys(post, "post"), vec!["img_b"]);

        // Text messages carry no image keys.
        assert!(extract_image_keys(r#"{"text":"hi"}"#, "text").is_empty());

        // Unparseable content degrades to empty.
        assert!(extract_image_keys("not json", "image").is_empty());
    }

    #[test]
    fn image_keys_ignores_non_img_image_key_fields() {
        // A generic `image_key` field NOT under a `tag:"img"` element must not
        // be treated as a downloadable image.
        let v = r#"{"extra":{"image_key":"not_an_element"}}"#;
        assert!(extract_image_keys(v, "post").is_empty());
    }

    /// Feishu sometimes delivers the user's input as a CARD (msg_type
    /// "interactive"/"post") whose content is the message JSON. The visible
    /// text must be extracted — from `content_v2`/`content` only ONCE — not
    /// forwarded as raw JSON (or duplicated) to the AI.
    #[test]
    fn parse_card_message_extracts_user_text() {
        let msg = MessageData {
            message_id: "msg_3".into(),
            root_id: None,
            parent_id: None,
            thread_id: None,
            chat_id: "chat_1".into(),
            chat_type: "p2p".into(),
            message_type: "interactive".into(),
            // `content` and `content_v2` carry the same text.
            content: r#"{"title":"","content":[[{"tag":"text","text":"你说的AI回复镜像是什么意思？"}]],"content_v2":[[{"tag":"text","text":"你说的AI回复镜像是什么意思？"}]]}"#.into(),
            mentions: vec![],
        };
        let text = parse_message_content(&msg);
        assert_eq!(
            text, "你说的AI回复镜像是什么意思？",
            "text must be extracted once: {}",
            text
        );
        assert!(!text.contains("content"), "raw JSON must not leak: {}", text);
    }

    fn mention(key: &str, open_id: &str, name: &str) -> crate::feishu::event::Mention {
        crate::feishu::event::Mention {
            key: Some(key.into()),
            id: Some(crate::feishu::event::MentionId {
                open_id: Some(open_id.into()),
            }),
            name: Some(name.into()),
        }
    }

    #[test]
    fn strip_mentions_drops_bot_and_names_others() {
        let text = "@_user_1 你好 @_user_2 请 review";
        let mentions = vec![
            mention("@_user_1", "ou_bot", "cola"),
            mention("@_user_2", "ou_li", "李明"),
        ];
        assert_eq!(strip_mentions(text, &mentions, "ou_bot"), "你好 @李明 请 review");
    }

    #[test]
    fn strip_mention_tokens_cleans_stale_session_names() {
        // Stale names persisted before mention stripping existed.
        assert_eq!(strip_mention_tokens("@_user_1 你好"), "你好");
        assert_eq!(strip_mention_tokens("@_user_1 你能做什么？"), "你能做什么？");
        assert_eq!(strip_mention_tokens("frontend-refactor"), "frontend-refactor");
        assert_eq!(strip_mention_tokens(""), "");
    }

    #[test]
    fn strip_mentions_handles_no_mentions() {
        assert_eq!(strip_mentions("hello", &[], "ou_bot"), "hello");
    }

    #[test]
    fn strip_mentions_handles_bot_only_message() {
        let text = "@_user_1 你能做什么？";
        let mentions = vec![mention("@_user_1", "ou_bot", "cola")];
        assert_eq!(strip_mentions(text, &mentions, "ou_bot"), "你能做什么？");
    }

    #[test]
    fn strip_mentions_handles_mention_without_name() {
        let text = "hi @_user_2";
        let mentions = vec![crate::feishu::event::Mention {
            key: Some("@_user_2".into()),
            id: None,
            name: None,
        }];
        assert_eq!(strip_mentions(text, &mentions, "ou_bot"), "hi");
    }

    #[test]
    fn is_mentioned_detects_bot() {
        let mentions = vec![
            mention("@_user_1", "ou_bot", "cola"),
            mention("@_user_2", "ou_li", "李明"),
        ];
        assert!(is_mentioned(&mentions, "ou_bot"));
        assert!(!is_mentioned(&mentions, "ou_other"));
        assert!(!is_mentioned(&[], "ou_bot"));
    }
}
