# File Content delivery: embed images in the card, send the rest as a File Message

OpenCode's tool output can carry a **File Content** — a `file` block (a `data:`
URI plus mime and name) that `read` returns for an image or PDF, or that a user
message's `files` list carries — and `tool_output` drops it, so the panel shows
just 「Image read successfully」. cola renders a File Content back to the user by
a **hybrid** of the two Feishu surfaces: an image within Feishu's caps is
uploaded once and embedded in the card right after its **Tool Panel**; anything
else is uploaded once and posted as a separate **File Message**. Feishu can show
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
    returning a `file_key`; `msg_type:"file"` (or `"image"`) sends it.
  - An uploaded key is stable and reusable: **one upload serves every later
    PATCH**, so a card rebuild never re-uploads.
  - An inline `data:` URI cannot be embedded directly: the base64 payload blows
    the card's ~30KB budget. Display always means upload-then-reference.
- **Card building is synchronous and pure.** The `img_key` must be resolved
  before the card JSON is built, so the upload needs a pre-resolve step in the
  render loop.

## Decision

- **Embed an image within the caps.** An image whose mime is in Feishu's set and
  whose bytes are ≤10MB is uploaded once and embedded as a **standalone `img`
  element immediately after its Tool Panel** in the card body, title
  `📎 <name>`. It is not nested in the panel (the panel is folded by default), so
  the image is visible and its adjacency to the panel gives the correspondence.
- **Send everything else as a File Message.** A non-image file, or an image past
  the caps (still ≤30MB), is uploaded once and posted as a separate Feishu
  message (`msg_type:"file"`), **replied in-thread under the live card**, sent
  **once on the first poll that shows the block**, guarded by a process-local
  once-guard (a cola restart may resend it).
- **The panel's tracking line has three states.** Every File Content gets one
  line in its Tool Panel: `📎 name · mime · size · 已内嵌` when the image is
  embedded in the card; `📎 name · mime · size · 已发送为文件消息` when it was
  delivered as a File Message; `📎 name · mime · size · 未发送` when it could not
  be delivered — over Feishu's hard 30MB message cap, or the send failed. An
  embedded image is never marked `未发送`: it was shown on the card, not sent as
  a message, and its own state says so. The fallback ladder when the preferred
  surface fails: embed upload fails → degrade to a File Message → that fails →
  `未发送`.
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

## Why

- **Hybrid, not one surface, because neither alone covers the issue.** A
  card-only answer drops every PDF (no card file element); a
  separate-messages-only answer loses the correspondence between a turn's
  several images and the calls that read them. Embedding images and messaging
  the rest covers both.
- **A separate message is the only path to a PDF**, and Feishu's own upload
  limits (10MB image / 30MB file) set the boundary between the two surfaces.
- **One shared step, not per-tool renderers.** File Content is content-shaped,
  not tool-shaped, so `read`, an MCP resource, and a user message all ride one
  "File Content → card/message" path — consistent with ADR-0042's boundary.

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

## Tests (implementation batch)

The implementation batch (spec #644, tickets #646–#649) adds the tests at the
three existing seams: the Feishu wire client (the new upload/send request
bodies), the pure card builder (the `img` element, the three tracking-line
states, the content-rejection fenced fallback), and the bridge mock Platform
(the pre-resolve, the content-hash dedup, the fallback ladder, the once-guard).
