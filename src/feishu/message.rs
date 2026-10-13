//! What a Feishu message means to cola: its readable text, the images and
//! files it carries, and its @mentions. Pure content helpers plus the
//! quoted-context fetch that composes them; the WS transport lives in the
//! sibling `ws` module.

use crate::feishu::Platform;
use crate::feishu::client::{FileAttachment, ImageAttachment};
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
    // A message with no text of its own reads as its files — `[文件: <name>]`,
    // one per file (a `file` message, or a rich-text message that attached a
    // file with no caption). The model (and the user's own transcript record)
    // then sees WHICH file, never the opaque `{"file_key":…}` JSON; an unnamed
    // file falls back to the neutral default. A payload with no downloadable
    // ref keeps the per-type placeholder.
    let files = extract_file_refs(content, message_type);
    if !files.is_empty() {
        return files
            .iter()
            .map(|file| format!("[文件: {}]", file.name))
            .collect::<Vec<_>>()
            .join("\n");
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
    // The client's post payload carries the SAME elements in both `content` and
    // `content_v2` — when both are present, walk only one (v2 is canonical),
    // otherwise every embedded image is collected twice and the prompt gets
    // duplicate attachments. Same rule as `extract_card_text`.
    let source = if v.get("content").is_some() && v.get("content_v2").is_some() {
        &v["content_v2"]
    } else {
        &v
    };
    let mut out = Vec::new();
    collect_image_keys(source, &mut out);
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

/// One file reference embedded in a message: the `file_key` to download and the
/// display name. A rich-text message's top-level `files[]` carries entries the
/// Feishu client produces when a file is attached to a message with text; a
/// `file` message carries one directly. A folder entry (`is_folder: true`) has
/// no payload — `key` is empty and `folder` set, so the caller reports it as
/// unsupported instead of downloading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileRef {
    pub key: String,
    pub name: String,
    pub folder: bool,
}

/// The display name an inbound attachment reads when the message carried none
/// (`file_name` absent on an older payload). The same neutral default a File
/// Content uses (ADR-0076).
fn default_attachment_name() -> &'static str {
    crate::backend::file_content::DEFAULT_FILE_NAME
}

/// The files embedded in a message: a `file` message's own content
/// (`{"file_key":"…","file_name":"…"}`), and a rich-text message's top-level
/// `files[]` array (the shape the Feishu client produces when a file is
/// attached to a message with text — the `msg_type:"post"` form). Empty when
/// there is nothing downloadable.
pub(crate) fn extract_file_refs(content: &str, message_type: &str) -> Vec<FileRef> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(content) else {
        return Vec::new();
    };
    if message_type == "file" {
        let key = v.get("file_key").and_then(|k| k.as_str());
        return key
            .filter(|k| !k.is_empty())
            .map(|key| {
                vec![FileRef {
                    key: key.to_string(),
                    name: file_name(&v),
                    folder: false,
                }]
            })
            .unwrap_or_default();
    }
    let Some(files) = v.get("files").and_then(|f| f.as_array()) else {
        return Vec::new();
    };
    files
        .iter()
        .filter_map(|entry| {
            let folder = entry.get("is_folder").and_then(|f| f.as_bool()).unwrap_or(false);
            let key = entry
                .get("file_key")
                .and_then(|k| k.as_str())
                .unwrap_or_default()
                .to_string();
            // Nothing to download and not a folder: no reference at all (never
            // a guessed one).
            if key.is_empty() && !folder {
                return None;
            }
            Some(FileRef {
                key,
                name: file_name(entry),
                folder,
            })
        })
        .collect()
}

/// A file entry's display name, or the neutral default when it carried none.
fn file_name(entry: &serde_json::Value) -> String {
    entry
        .get("file_name")
        .and_then(|n| n.as_str())
        .filter(|n| !n.is_empty())
        .unwrap_or(default_attachment_name())
        .to_string()
}

/// A message's prompt-relevant source, before any download: the quoted text and
/// the attachment references. The split lets the caller inject the text on its
/// own budget and download the attachments best-effort afterwards (ADR-0076's
/// inbound amendment) — a slow download then delays the attachments alone, not
/// the quoted text.
pub(crate) struct QuotedSource {
    pub text: String,
    pub image_keys: Vec<String>,
    pub file_refs: Vec<FileRef>,
}

/// Fetch + parse a message for quote injection (Quoted Context): fetch it by
/// id and extract its text (mention placeholders → names) plus the refs of the
/// images and files it carries — no download. The returned text is prepended to
/// the reply's prompt so the model sees what the reply answers, including when
/// the parent is missing from session history (lobby-session switch,
/// compaction).
///
/// The bot's own @mention in the quoted text is not stripped (resolving the bot
/// id would add an API call per quote); a quoted bot mention keeps its display
/// name, which is fine for context.
pub(crate) async fn quoted_source(
    feishu: &Arc<dyn Platform>,
    message_id: &str,
) -> crate::error::Result<QuotedSource> {
    let msg = feishu.get_message(message_id).await?;
    let raw = message_text(&msg.msg_type, &msg.content);
    Ok(QuotedSource {
        text: strip_mentions(&raw, &msg.mentions, ""),
        image_keys: extract_image_keys(&msg.content, &msg.msg_type),
        file_refs: extract_file_refs(&msg.content, &msg.msg_type),
    })
}

/// A message's downloaded attachments: the payloads plus the refs that could
/// not be turned into one (a folder entry, a failed download) — reported, never
/// dropped silently (ADR-0076's inbound amendment).
pub(crate) struct DownloadedAttachments {
    pub images: Vec<ImageAttachment>,
    pub files: Vec<FileAttachment>,
    pub omitted: Vec<crate::bridge::OmittedAttachment>,
}

/// Download a message's images and files, recording every ref the download did
/// not resolve. The one downloader both the receive path (the message's own
/// attachments) and Quoted Context (the parent's) run, so the two cannot drift.
/// Best-effort: a failure loses only that attachment, and each call keeps its
/// own platform timeout (10s image / 30s file). Runs on the caller's task, so
/// the WS read loop keeps reading while a download is in flight.
pub(crate) async fn download_attachments(
    feishu: &Arc<dyn Platform>,
    message_id: &str,
    image_keys: &[String],
    file_refs: &[FileRef],
) -> DownloadedAttachments {
    let mut images = Vec::new();
    let mut files = Vec::new();
    let mut omitted = Vec::new();
    for key in image_keys {
        match feishu.download_image(message_id, key).await {
            Ok(img) => images.push(img),
            Err(e) => {
                tracing::warn!("download image {} failed: {}", key, e);
                omitted.push(crate::bridge::OmittedAttachment {
                    name: None,
                    reason: crate::bridge::OmitReason::DownloadFailed,
                });
            }
        }
    }
    for file in file_refs {
        // A folder entry has no payload: report it as unsupported rather than
        // attempt a download.
        if file.folder || file.key.is_empty() {
            omitted.push(crate::bridge::OmittedAttachment {
                name: Some(file.name.clone()),
                reason: crate::bridge::OmitReason::Unsupported,
            });
            continue;
        }
        match feishu.download_file(message_id, &file.key, &file.name).await {
            Ok(downloaded) => files.push(downloaded),
            Err(e) => {
                tracing::warn!("download file {} failed: {}", file.name, e);
                omitted.push(crate::bridge::OmittedAttachment {
                    name: Some(file.name.clone()),
                    reason: crate::bridge::OmitReason::DownloadFailed,
                });
            }
        }
    }
    DownloadedAttachments {
        images,
        files,
        omitted,
    }
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
/// `@_user_1` into a card header or the `/switch` card).
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
    fn image_keys_post_not_duplicated_across_content_and_content_v2() {
        // Real post payload: the same elements appear in both `content` and
        // `content_v2`. Walking both downloads and attaches the image twice
        // (observed live: one screenshot became two identical file parts).
        let post = r#"{"title":"","content":[[{"tag":"img","image_key":"img_dup"}]],"content_v2":[[{"tag":"img","image_key":"img_dup"}]]}"#;
        assert_eq!(extract_image_keys(post, "post"), vec!["img_dup"]);

        // Text + two distinct images, duplicated across both fields, keeps one
        // key per actual image.
        let post = r#"{"title":"","content":[[{"tag":"text","text":"看图"}],[{"tag":"img","image_key":"img_a"},{"tag":"img","image_key":"img_b"}]],"content_v2":[[{"tag":"text","text":"看图"}],[{"tag":"img","image_key":"img_a"},{"tag":"img","image_key":"img_b"}]]}"#;
        assert_eq!(extract_image_keys(post, "post"), vec!["img_a", "img_b"]);
    }

    #[test]
    fn image_keys_ignores_non_img_image_key_fields() {
        // A generic `image_key` field NOT under a `tag:"img"` element must not
        // be treated as a downloadable image.
        let v = r#"{"extra":{"image_key":"not_an_element"}}"#;
        assert!(extract_image_keys(v, "post").is_empty());
    }

    /// A `file` message's content yields one ref; the body names it, never the
    /// opaque content JSON. A payload with no downloadable ref keeps `[文件]`.
    #[test]
    fn file_message_names_its_file() {
        let refs = extract_file_refs(r#"{"file_key":"fk_1","file_name":"P3173R0.pdf"}"#, "file");
        assert_eq!(
            refs,
            vec![FileRef {
                key: "fk_1".into(),
                name: "P3173R0.pdf".into(),
                folder: false,
            }]
        );
        assert_eq!(
            parse_message_content(&MessageData {
                message_id: "m".into(),
                root_id: None,
                parent_id: None,
                thread_id: None,
                chat_id: "c".into(),
                chat_type: "p2p".into(),
                message_type: "file".into(),
                content: r#"{"file_key":"fk_1","file_name":"P3173R0.pdf"}"#.into(),
                mentions: vec![],
            }),
            "[文件: P3173R0.pdf]"
        );
        // No name: the neutral default; no ref at all: the bare placeholder.
        assert_eq!(
            extract_file_refs(r#"{"file_key":"fk_2"}"#, "file")[0].name,
            "file"
        );
        assert!(extract_file_refs(r#"{"some_key":"xyz"}"#, "file").is_empty());
    }

    /// A rich-text message with NO text of its own but a file attached reads as
    /// its file (`[文件: <name>]`), not the opaque per-type placeholder; with
    /// text, the caption stands (the success marker is only for a text-less
    /// message — ADR-0076's inbound amendment).
    #[test]
    fn a_text_less_post_attachment_names_the_file() {
        let msg = |content: &str| MessageData {
            message_id: "m".into(),
            root_id: None,
            parent_id: None,
            thread_id: None,
            chat_id: "c".into(),
            chat_type: "p2p".into(),
            message_type: "post".into(),
            content: content.into(),
            mentions: vec![],
        };
        // An attached file with nothing else (here beside an image element).
        let bare = r#"{"content":[[{"tag":"img","image_key":"img_x"}]],"files":[{"file_key":"fk_1","file_name":"P3173R0.pdf","is_folder":false}]}"#;
        assert_eq!(parse_message_content(&msg(bare)), "[文件: P3173R0.pdf]");
        // With a caption, the caption is the body; no marker is added.
        let captioned = r#"{"content":[[{"tag":"text","text":"我发这个文件吧"}]],"files":[{"file_key":"fk_1","file_name":"P3173R0.pdf","is_folder":false}]}"#;
        assert_eq!(parse_message_content(&msg(captioned)), "我发这个文件吧");
    }

    /// A rich-text message's top-level `files[]` (the shape the Feishu client
    /// actually produces for a file attached to a message with text) yields
    /// every file, and a folder entry is surfaced (not dropped) so the caller
    /// can report it as unsupported.
    #[test]
    fn post_level_files_array_yields_every_file_and_marks_folders() {
        let post = r#"{"content":[[{"tag":"text","text":"我发这个文件吧"}]],"files":[{"file_key":"file_v3_1","file_name":"P3173R0.pdf","is_folder":false},{"file_key":"","file_name":"docs","is_folder":true}]}"#;
        assert_eq!(
            extract_file_refs(post, "post"),
            vec![
                FileRef {
                    key: "file_v3_1".into(),
                    name: "P3173R0.pdf".into(),
                    folder: false,
                },
                FileRef {
                    key: String::new(),
                    name: "docs".into(),
                    folder: true,
                },
            ]
        );
        // No files array: nothing.
        assert!(extract_file_refs(r#"{"text":"hi"}"#, "text").is_empty());
        assert!(extract_file_refs("not json", "post").is_empty());
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
