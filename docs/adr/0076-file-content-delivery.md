# File Content delivery: embed images in the card, send the rest as a File Message

OpenCode's tool output can carry a **File Content** — a `file` block (a `data:`
URI plus mime and name) that `read` returns for an image or PDF, or that a user
message's `files` list carries — and `tool_output` drops it, so the panel shows
just 「Image read successfully」. cola renders a File Content back to the user by
a **hybrid** of the two Feishu surfaces: an image within Feishu's caps is
uploaded once and embedded in the card right after its **Tool Panel**; anything
else within Feishu's 30MB message cap is uploaded once and posted as a separate
**File Message**, while a File Content past that cap, or one whose send fails,
leaves only a tracking line. Feishu can show
a PDF in no other way (a card has no file component), and embedding is the only
way a turn's several images stay tied to the call that read each one.

## Context

- **The payload is already inline.** OpenCode's `read` returns
  `[{type:"text",text:"Image read successfully"},{type:"file",uri:"data:<mime>;base64,…",mime,name}]`
  (`packages/core/src/tool/plugin/read.ts`), and MCP resources can return file
  parts. V1's wire emits the same `Other` block, so the payload is
  generation-neutral once decoded. cola's `decode_tool_output` already keeps it
  as `ContentBlock::Other`; only the Platform's `tool_output` (which joins
  `Text` blocks) drops it.
- **Feishu's surfaces (confirmed 2026-10-10):**
  - The card is JSON 2.0 (`schema:"2.0"`) and has an `img` element
    (`img_key`, `alt`, `title`, `preview`) but **no `file` element** — a PDF can
    never render inside a card. `img` is allowed inside a `collapsible_panel`.
  - `POST /open-apis/im/v1/images` (scope `im:resource`) uploads an image
    ≤10MB, formats jpg/jpeg/png/webp/gif/bmp/ico/tiff/heic (tiff/heic become
    jpg), GIF ≤2000×2000 and others ≤12000×12000, returning a reusable
    `image_key`.
  - `POST /open-apis/im/v1/files` (scope `im:resource`) uploads a file ≤30MB,
    returning a `file_key`; `msg_type:"file"` sends it.
  - An uploaded key is stable and reusable: **one upload serves every later
    PATCH**, so a card rebuild never re-uploads.
  - An inline `data:` URI cannot be embedded directly: the base64 payload blows
    the card's ~30KB budget. Display always means upload-then-reference.
- **Card building is synchronous and pure.** The `img_key` must be resolved
  before the card JSON is built, so the upload needs a pre-resolve step in the
  render loop.

## Decision

- **One typed File Content over the neutral model.** The decoded file payload is
  exposed as a typed view — uri, mime, name, and a size derived from the inline
  bytes — uniform across Generations and across a tool's `file` block and a user
  message's `files` entry, with sane defaults for a missing name or mime. The
  renderer never re-parses the raw block, and the model's existing raw-block
  preservation is unchanged.
- **Embed an image within the caps.** An image whose mime is in Feishu's set,
  whose bytes are ≤10MB, and whose dimensions are within Feishu's limits (GIF
  ≤2000×2000, others ≤12000×12000) is uploaded once and embedded as a
  **standalone `img` element immediately after its Tool Panel** in the card body,
  title `📎 <name>`, `preview` enabled so a click enlarges it. It is not nested in
  the panel (the panel is folded by default), so the image is visible and its
  adjacency to the panel gives the correspondence. A format or dimension Feishu
  rejects is not embeddable and falls through to the File Message path. The
  card's whole-card content-rejection fallback (which otherwise fences every
  element as code) must preserve `img` elements.
- **Send everything else as a File Message.** A non-image file, or an image past
  the caps (still ≤30MB), is uploaded once and posted as a separate Feishu
  message (`msg_type:"file"`), **replied in-thread under the live card**, sent
  **once on the first poll that shows the block**, guarded by a process-local
  once-guard (a cola restart may resend it).
- **One shared Feishu capability, on the Platform boundary.** Image and file
  upload, and a generic message send (an arbitrary `msg_type` + content, with
  thread-reply support), are Platform methods, so the Bridge stays
  platform-agnostic and the fake Platform records uploads and sends. An image
  uploads as `image_type=message`; a file upload carries `file_type` (from the
  mime/name) and `file_name`. The delivery is one reusable step shared by the
  image-embed and File Message surfaces.
- **The tracking line has three states.** A tool-output File Content gets one
  line in its Tool Panel: `📎 name · mime · size · 已内嵌` when the image is
  embedded in the card; `📎 name · mime · size · 已发送为文件消息` when it was
  delivered as a File Message; `📎 name · mime · size · 未发送` when it could not
  be delivered — over Feishu's hard 30MB message cap, or the send failed. An
  embedded image is never marked `未发送`: it was shown on the card, not sent as a
  message, and its own state says so. A user-message File Content is recorded
  elsewhere (below) and never gets a File Message. The fallback ladder when the
  preferred surface fails: embed upload fails → degrade to a File Message → that
  fails → `未发送`.
- **Upload is pre-resolved in the render loop**, cached in memory **by content
  hash**, so an identical file is uploaded once and every PATCH reuses the key.
- **A user message's File Content is record-only.** It shows
  `📎 name · mime · size` wherever a user message's body is shown — the Session
  Snapshot「最近对话」tail and the External Message「有新消息」preview — and its
  bytes are never re-delivered (that would duplicate what the user already sent).
- **Generation-neutral.** The rendering reads the neutral decoded `file` block,
  so V1 and V2 behave identically.
- **Shell output (#588) is deliberately out.** A shell's captured output is
  noise as a file attachment; the Background Task Ledger's clipped tail stays its
  surface.

## Amendment (2026-10-11): the File Message's reply omits `reply_in_thread`

The Decision above says the File Message is "replied in-thread under the live
card". The first cut of the amendment decided the reply mode from the Session's
ThreadKey (`reply_in_thread: true` when `thread_id != chat_id`), but that reads
the **Session mapping** while the live card target is resolved separately and can
outlive it: a lobby Session adopted into a topic mid-turn still has a live card
targeting the lobby-root message, so the next poll passed `true` and Feishu
created a NEW topic around that card — exactly the fork the change was meant to
prevent.

Feishu's own rule for `POST /open-apis/im/v1/messages/:message_id/reply` settles
it, so cola carries no mode at all. `reply_in_thread` **defaults to `false`**,
and per the official docs: *"If the message to be replied is already in thread
form, the reply will be in thread form by default."* A reply that simply OMITS
the field therefore lands wherever the **target card** lives — inside the topic
when the card is already threaded, and as a plain quoted reply (creating no
topic) when the card is at chat level. The card's own container decides, so the
behaviour is correct across an adopt-mid-turn transition with no mapping lookup
and no race. Sending `reply_in_thread: true` against a lobby card would instead
create a new topic; because cola keys a conversation by `(chat_id, thread_id)`,
that topic resolves to a DIFFERENT Session — a fork (found live on spec #644).

The rest of the Decision is unchanged: exactly one File Message, sent once on the
first poll that shows the block, best-effort.

## Amendment (2026-10-11): the embedded `img` element's `alt` and `title` are the file's basename

The Decision's element carries `preview` enabled, an `alt` that is the file's
**basename as plain text**, and a `title` that is `📎 <basename>` — the same
short name the folded panel header shows when it shows one.

Feishu's Card JSON 2.0 image component marks `alt` **required** (its field table
lists `alt | Yes | Struct`), so the element always carries it. It is the image's
**accessible name** — the text a client shows if the image fails to load, and
what assistive technology announces — so the common practice for an `<img>` is to
fill it with the image's own name: the file's **basename** as PLAIN text
(`shot.png`, no `📎` prefix, no path). An empty `alt` would satisfy the schema but
say nothing.

`title` is the visible caption, `📎 <basename>`. Both fields share the same short
name — the basename, folded to one printable line and clipped to its tail within
the header's character budget **only when the basename itself is long** — so a
`read`'s `/root/workspace/dev/cola/target/acc/shot.png` renders `alt: shot.png`
and `title: 📎 shot.png`: for a name that has a basename segment, the full
absolute path and any path fragment (`📎 …ola/target/acc/shot.png`) never render.
A client renders the title as the image's caption, where path fragments are
noise. The size estimate follows the builder, so it charges the name's
JSON-escaped bytes once per field, exactly what renders.

The basename rule has one exception. `header_name_basename` returns `None` when
the folded name ends in a separator and so has no basename segment — `foo/`,
`dir/sub/`, `/` — and `header_file_name` then falls back to the whole folded
name, directories and all, clipped to its tail within the same budget. Such a
name renders its path (`alt: dir/sub/`), not a basename.

## Amendment (2026-10-11): the folded panel header carries the file marker

The Decision puts each File Content's tracking line in its Tool Panel **body**,
but the panel is folded by default, so the line is invisible until the user
expands it — worst of all for `未发送`, which is the ONLY signal that a file was
read but could not be delivered. The panel **header** therefore carries a compact
marker, visible while the panel stays collapsed.

The marker exists for the files the card does **not** show. An **embedded image
is visible in the card** — it is rendered right after the panel with its own
`📎 <name>` caption (the amendment above) — so naming it again in the folded
header only duplicated the caption; an `Unresolved` file is not yet a card's
concern. The marker therefore appears **iff** the panel has at least one File
Content delivered `SentAsFile` or `Undelivered`, and it names only those files:

- one such File Content — ` · 📎 <name>`, the file's **basename** (the segment
  after its final `/` or `\`), clipped to its **tail** within a short character
  budget **only when the basename itself is long** — a leading `…` plus the last
  characters, the `…` counted against the budget, so a long FILE name keeps its
  extension. An ordinary path is reduced to its basename first, so its
  directories never spend the budget and never render; a name with no basename
  segment (`dir/sub/`, `/`) falls back to the whole folded name, as the amendment
  above records;
- several — ` · 📎 <N> 个文件`, `N` the number of such files (not the panel's
  whole file count);
- and ` · 未发送` appended when ANY of them is undelivered, so the one state that
  must never be missed is on the header itself.

A panel whose files are ALL `Embedded` (or unresolved) carries **no marker at
all**: its header is byte-for-byte the plain one. The marker names every file the
card does not show — `SentAsFile` (a visible File Message of its own) as well as
`Undelivered` — and `Undelivered` additionally appends ` · 未发送`, the state
that must never be invisible. The body's detailed tracking line is unchanged for
every file — the header is the folded summary, the body remains the record. The
header title is card JSON, so the card-size estimate charges the marker's
JSON-escaped bytes exactly as it charges the body line.

Three bounds are accepted. An `Unresolved` file does not mark the header, so a
File Message that resolves before the card has a reply target shows a plain
header on the first write and gains its marker on a later poll — a transient
flicker, never a persistently hidden `未发送`. Two files whose basenames collide
are indistinguishable in the marker and the caption, while the panel body's
tracking line keeps the full path. And `alt` is the hover description in Feishu's
own model, so a desktop client can show the basename on hover in addition to the
`title` caption.

## Amendment (2026-10-13): the same File Content family is accepted inbound

The Decision above is the **outbound** half of File Content: what the agent read
reaches the user. Issue #473 asks the mirror — what the USER attaches in Feishu
currently reaches nothing. A `file` message renders the literal `[文件]`
placeholder and its bytes are never downloaded, and a rich-text message's
top-level `files[]` array (the shape the Feishu client actually produces when a
file is attached to a message with text) is not read at all, so "read this PDF"
does nothing. The two directions share every Feishu fact — the same
message-resource endpoint that sits beside the upload the Decision uses, and a
download that needs only the already-held `im:message` (ADR-0009), not the
upload's `im:resource` — but almost no code: inbound produces a prompt
attachment, outbound renders a transcript part.

- **A Prompt Attachment, not a File Content.** The inbound term generalizes
  **Image Attachment** (ADR-0009): an inbound platform file or image downloaded
  and attached to a prompt as a file part. Images keep their existing path; a
  `file` message and a rich-text message's top-level `files[]` entries (a folder
  entry excluded — it is reported as an unsupported type) join them.
- **Attach only what the server reads as model content.** The server forwards
  exactly `image/png`, `image/jpeg`, `image/gif`, `image/webp`,
  `application/pdf`, and `text/plain` to the model and drops every other mime
  silently on V2; on V1 any mime becomes a provider file part, where an
  unsupported one fails the run. The image set is those four mimes, **not
  `image/*`**: the server's resizer re-encodes only an image that EXCEEDS the
  size or dimension limits, so a small `image/bmp`/`tiff`/`heic` keeps its mime
  and is dropped. cola therefore attaches those three families only — a text
  file (extension table plus `text/*`) normalized to `text/plain` — and leaves
  any other type out of the prompt with a note. The image path gains this mime
  check too, which it lacks today.
- **The cap is the server's 20MB, checked per file.** The server measures the
  decoded payload (`MAX_ATTACHMENT_BYTES`) and fails the WHOLE prompt past it, so
  cola pre-checks the same line and never sends an oversize attachment. No
  per-prompt total is imposed — the desktop client's `assertAttachmentBudget` is
  stricter than the protocol requires. The check covers the image path as well,
  which had none: an oversize image used to fail the whole run the same way.
- **Nothing is dropped silently.** A file that is attached carries a success
  marker in the prompt body when the message has no text of its own —
  `[文件: <name>]`, replacing today's bare `[文件]`; an image carries none (it has
  no name). An unsupported type, an oversize file, or a download failure appends
  a note to the body — `[文件: <name> · 未附带：类型不支持 | 超过 20MB | 下载失败]`,
  and `[图片 · 未附带：类型不支持 | 超过 20MB | 下载失败]` for an image (it has no
  name to show) — and sends one short reply
  (`⚠️ 文件 <name> 未能附带（<reason>）`), so the user can tell their file was not
  read — the failure a file message has today.
- **Quoted Context carries attachments too, and its text no longer waits behind
  them.** The parent's files join its images; the fetch is split so the quoted
  text is injected as soon as it arrives (keeping its 1.5s budget) while the
  attachments download best-effort within their own budget (30s per file, where
  the image path's is 10s). Today's single 1500ms budget over fetch plus every
  download already discards the quoted TEXT when a parent's image download is
  slow.
- **No content dedup.** Upstream deduplicates only repeated `@mention`s of one
  payload inside a single prompt (the TUI's `deduplicatePromptImages`, V2's
  `userAttachmentContent`), which has no Feishu counterpart — a user
  re-attaching a file is expressing intent, and Feishu's at-least-once
  redelivery is already caught by the event-id dedupe.
- **One protocol shape, both Generations.** V1's `file` part
  (`{type, mime, url, filename}`) and V2's `files[]` entry (`{uri, name}`) both
  carry an arbitrary mime and a display name, and the attachment reaches the
  transcript as a user-message File Content — so the user's own attachment is
  recorded where their message is shown by the machinery above, with no inbound
  code of its own.

Two live-deployment facts are checked before implementation, because either can
change the design: whether a `file` message's content always carries
`file_name` (older payloads may carry only `file_key`, in which case the name
falls back to the neutral default `file`), and whether the resource response's
Content-Type is the generic `application/octet-stream` — which decides how the
mime is derived (the name's extension first, the header as the fallback). Tests
land at the three existing seams — the mock Platform, the wire client's fake
server, and the record line — with no live chain.

The outbound Decision is otherwise unchanged.

## Why

- **Hybrid, not one surface, because neither alone covers the issue.** A
  card-only answer drops every PDF (no card file element); a
  separate-messages-only answer loses the correspondence between a turn's
  several images and the calls that read them. Embedding images and messaging
  the rest covers both.
- **A separate message is the only path to a PDF**, and Feishu's own upload
  limits (10MB image / 30MB file) set the boundary between the two surfaces.
- **One shared step, not per-tool renderers.** File Content is content-shaped,
  not tool-shaped: the `file` block is a protocol-level variant of
  `Tool.Content`, defined for every tool, so `read`, an MCP resource, and a user
  message all ride one "File Content → card/message" path. This is not
  ADR-0042's forbidden shape-sniffing — that boundary keeps a tool's own
  `input`/`output` payload opaque, whereas a `file` content block is decoded
  neutrally before any tool id is consulted; ADR-0042 carries an amendment
  recording the distinction.

## Alternatives considered

- **All-in-card** (images embedded, non-images placeholder-only): rejected — it
  silently drops the PDF half of the feature.
- **All separate messages** (images too): rejected — a turn's several images
  cannot be tied back to their tool calls.
- **Uploading >30MB through Drive (云空间)**: rejected — chunked Drive upload
  needs another subsystem (a `parent_node` folder, Drive scopes, a
  `file_token`/link) for a rare case; a placeholder line tracks the file
  instead. Recorded here so it is not re-proposed lightly.

## Consequences

- Deployments must add the `im:resource` scope; an app without it degrades to
  `未发送` tracking lines (the path is best-effort and never blocks a card).
- Feishu's 30MB message cap is a permanent ceiling: a File Content over it is
  never deliverable through this path.
- cola gains an outbound surface beyond cards and text: it uploads to Feishu and
  posts image/file messages.
- The File Message is a side effect of rendering, not card state: it is
  deduplicated process-locally, and a crashed cola may resend one.

## Domain note

The concepts enter the glossary as **File Content** (the payload a part carries)
and **File Message** (the separate message cola posts for one it cannot embed).
The inbound amendment above adds **Prompt Attachment** (an inbound platform file
or image attached to a prompt), generalizing the earlier **Image Attachment**.

## Tests (implementation batch)

The implementation batch (spec #644, tickets #646–#649) adds the tests at the
three existing seams: the Feishu wire client (the upload multipart fields, the
bearer/token flow, the `msg_type:"file"` content with `reply_in_thread` omitted,
and business-error mapping), the pure card builder (the `img` element's position
immediately after its panel and its `alt`/`title`/`preview`, the three
tracking-line states, the user-record line, the content-rejection fenced
fallback), and the
bridge mock Platform (the pre-resolve, the content-hash dedup, the fallback
ladder, the once-guard, the File Message landing under the live card in
whatever container the card lives, best-effort on failure).
