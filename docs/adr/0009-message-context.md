# Feishu message context: quote injection & image attachment

A reply to a Feishu message used to reach the model as bare text: `parent_id` was
parsed but dropped, so the model never knew which message it was answering, and
non-text messages leaked raw JSON (`{"image_key":"..."}`) into the prompt.

## Decision

- **Quoted Context**: when a message carries a Feishu `parent_id`, cola fetches
  that parent message (`GET /im/v1/messages/{id}?card_msg_content_type=raw_card_content`),
  extracts its text (mentions replaced with names), caps it at 2000 chars, and
  prepends `[引用消息]:\n{text}\n\n` to the prompt. Fetch is depth-1 and wrapped
  in a short timeout; any failure degrades silently to text-only (the pre-change
  behavior).
- **Image Attachment**: images are downloaded (`GET /im/v1/messages/{id}/resources/{key}?type=image`)
  and attached to the prompt as data-URL `file` parts
  (`{type:"file", mime, url:"data:<mime>;base64,..."}`) sent to
  `POST /session/{id}/message`. Covered: standalone `image` messages, images
  inside `post` (rich-text) messages, and the parent of a reply when it is an
  image. A model without vision support errors — accepted; the user switches
  model.
- **Placeholders**: non-text, non-card messages become `[图片]`/`[视频]`/`[语音]`/
  `[文件]`/`[表情]`/`[其他消息]` instead of leaking raw content JSON.
- **Feishu permissions**: no new scopes beyond the message ones cola already
  holds — reading a message by id and downloading its media both work with
  `im:message` alone (per Feishu's docs for 获取消息中的资源文件). Missing
  permission degrades the corresponding feature.

## Alternatives considered

- **Rely on session-history adjacency** (no fetch): the parent is usually already
  in the session's transcript, but the reply *relationship* is lost — the model
  cannot tell which earlier message the reply points to, and lobby-session
  switches / compactions drop the parent entirely.
- **Multi-level reply chains** (cc-connect traces up to 5): deferred. Deeper
  parents are almost always consecutive messages already present in session
  history; each extra level is another API call + latency for an edge case.
- **Temp-file file parts** (`file://` URIs): rejected. A data URL needs no temp
  file lifecycle or cleanup, survives server restarts, and matches how OpenCode
  itself encodes file parts.
- **Only standalone image messages**: rejected. Feishu delivers "screenshot +
  caption" from the composer as a single `post` message, so rich-text embedded
  images are the common case, not an edge case.