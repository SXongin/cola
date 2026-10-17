# R1 — Feishu display-surface inventory for #669 (file browser + viewer)

Research date: **2026-10-16**. Scope: what surfaces can a **personal bot with no public
ingress** use to show *host-machine file content* (code, markdown, long logs) inside a Feishu
chat. Primary sources are `open.feishu.cn` official docs; claims are tagged
**[doc]** (documented upstream), **[ours]** (measured by cola — ADR / date), or
**[unverified]** (not covered by the docs, or not measured).

> **Naming note (2026-10-16)**: this note was written while the card this feature wanted was
> called `/ls`. [#669](https://github.com/SXongin/cola/issues/669) and
> [ADR-0083](../../adr/0083-host-file-browse-and-view.md) settled on **extending the
> Directory Browser** (it lists files now) with a `/cat` viewer instead — **there is no `/ls`
> command**. Read every `/ls` below as that card.

---

## Verdict

Given no public ingress, **the interactive card really is the ceiling for anything
interactive**, and its hard limits are documented and low: 30 KB of serialized card or rich-text
message body, 200 elements/components, 5 tables per card, 5 data rows per markdown table
[doc]. Nothing richer exists *inside* the card: the Card JSON 2.0 catalog is exhaustive —
containers, display, interactive — and contains **no file/document component, no tree, no
iframe/web-view element, and no server-loaded (dynamic or cascading) select options** [doc]. So
the proposed browse card (listing files) + `/cat` fenced-code-block viewer is the correct ceiling, not a
shortcoming of the design.

Two surfaces meaningfully beat the card for *large text*, and both are real:

1. **`post` (rich text) messages with the `md` tag** — CommonMark 0.31 + GFM, 30 KB body
   (same byte cap), but a *different render path*: no 200-element cap, no 5-table cap, no
   per-element truncation, no button/interactivity, and it is an ordinary chat message so it
   behaves like the user's own long paste [doc]. Cost: **zero new scopes** (same `im:message`
   cola already holds), one function. It is the right "this file is too big / too tabular for
   the card" fallback, and cc-connect already ships exactly this fallback
   (`platform/feishu/feishu.go:2947` — falls back to `MsgTypePost` when the card hits the
   5-table limit) [ours].
2. **云文档 import** (`POST /open-apis/drive/v1/medias/upload_all` with
   `parent_type=ccm_import_open` → `POST /open-apis/drive/v1/import_tasks` → poll
   `.../import_tasks/:ticket`) — imports a local `.md`/`.markdown`/`.mark`/`.txt`/`.html`
   file (≤20 MB) into a docx and yields a `https://<tenant>.feishu.cn/docx/<id>` link the
   client renders natively [doc]. Cost: **a new subsystem** — new scopes
   (`docs:document:import` or `drive:drive`, plus `docs:document.media:upload`), 3+ round
   trips with async polling, a mounted folder, and no import format for arbitrary source code
   (`.rs`, `.py`, … are not importable — you'd have to rename/convert).

Plain **`text` messages are the third and cheapest escape hatch for raw dumps**: 150 KB body,
5× the card [doc]. No formatting beyond bold/italic/underline/strikethrough and links, and
editable at most 20 times [doc].

Everything genuinely "richer" (H5/网页应用, 小程序, 链接预览, a docx `iframe` block) requires
a **publicly reachable HTTPS URL with a real domain**, which cola by construction does not
have. The one documented mention of `127.0.0.1` is the developer-console *Web application
homepage* field, explicitly framed as a local debugging stopgap, not a production
configuration [doc].

---

## Decision table

| Surface | What it can show | Max size | Public ingress? | Extra scopes / config | Desktop + mobile | Verdict for #669 |
|---|---|---|---|---|---|---|
| **Card `markdown`** | Code (fenced, highlighted), headings, lists, tables (5/card, 5 rows shown), blockquotes, inline tags, `<audio>`, images by `image_key` | 30 KB card body; ≤200 elements/components [doc]. No documented per-element text cap [doc]; an 8000-char ASCII / 3060-char CJK element renders fully [ours, 2026-09-18] | No | none beyond `im:message` | Yes | **Primary viewer.** Fenced code block + language hint + pager for a text file. |
| **Card `img`** | Images (≤10 MB upload; GIF ≤2000×2000, others ≤12000×12000) [doc/ours] | 10 MB, dimension-capped [ours, ADR-0076] | No | `im:resource` (already held) | Yes | Already used by ADR-0076. Keeps "thumbnail in the card". |
| **Card `table`** | Plain text / `lark_md` / `markdown` / options / number / persons / date cells. Cells **clip at the end**; the user sees the rest by hover/click [doc]. **Cells are not buttons** | 5 table components per card, ≤50 columns, `page_size` 1–10 rows per page, row height ≤999px [doc] | No | none | Yes | Good for *file listings* (name/size/mtime columns), bad for long text. ADR-0078 already established cells can't be clickable. |
| **Card `chart`** | VChart spec (`line/area/bar/pie/funnel/scatter/radar/wordCloud/linearProgress/circularProgress/common`), interactive on click, enlargeable on PC / full-screen on mobile | ≤5 charts per card (recommended), `height` ≤999px [doc]; `chart_spec` is validated server-side (error `200550`) and **no JavaScript syntax** is allowed [doc] | No | none | Yes (with documented mobile gaps: conical gradient, grid word-cloud, texture, SVG bar background [doc]) | **Not a file viewer.** Could be a bonus for "plot this CSV" later. Not a display escape hatch. |
| **`file` message** | The bytes. Client-side preview depends on type (see §2) — `.html` renders as a page (inline `<script>` runs) and `.md` renders as rich text, both **measured 2026-10-18**; the full format list is still unverified | 30 MB upload hard cap; no 0-byte files [doc]. Client preview: **12 MB** for `.html` [doc, measured] | No | `im:resource` (already held) | Yes | **The binary/PDF download path** (ADR-0076) — and the only surface that renders an HTML page (#774). |
| **`audio` / `media`** | Audio player / mp4 video with cover [doc] | file upload ≤30 MB; audio must be `opus`, video `mp4` (or `stream`) [doc] | No | `im:resource` | Yes | Only useful for a future "read this aloud". Also `<audio file_key=…>` inline in a card's markdown [doc] — a card *can* embed an audio player, which is the precedent that a card can reference an uploaded `file_key`, but only for audio. |
| **云文档 import** | A real online doc: rich text, headings, tables, images, `code` blocks with `CodeLanguage`, `file`/`iframe` blocks. Link renders natively in the client | Import source ≤20 MB for txt/md/html (600 MB docx, 800 MB xlsx) [doc]; `file_token` expires after **5 minutes** [doc]; folder tree ≤15 levels / 1500 nodes per level [doc] | **No** — everything is outbound REST to `open.feishu.cn` | `docs:document:import` **or** `drive:drive`; plus `docs:document.media:upload` for the upload step; a `folder_token` (empty = My Space root) | Yes | **Viable but heavy.** The one surface that genuinely beats the card for a 5000-line log. Only `.md/.mark/.markdown/.txt/.html` — **no arbitrary source code**. |
| **H5 via `open_url`** | Whatever you serve — full text viewer, syntax highlighting, search, a real file tree | Unlimited (it's your server) | **YES** — and the app's homepage URL must be a public network address in production [doc] | App needs the **Web application** capability + a published version; `applink.feishu.cn/client/web_app/open?appId=…` [doc] | Yes | **Out.** Requires exactly what cola does not have. |
| **H5 mini-app `open_url`** | Same, richer container (window/sidebar/tab modes) | — | **YES** | A separate 小程序 app, built in Feishu's mini-program toolchain and published [doc] | Yes | **Out.** A whole second app identity. |
| **链接预览 (link preview)** | Bare URLs in messages/chat are parsed into an icon+title ("inline") and optionally a **card** | The preview body is itself a card → 30 KB / 200 elements again [doc] | **YES** — URL rules need a **TLD + second-level domain** (IP addresses explicitly rejected) [doc], *and* an HTTP callback answered within 3 s [doc] | App **Link preview** capability, ≤10 URL rules, a **published (not test) version**, `url.preview.get` event subscription [doc] | Yes | **Out**, doubly: needs a domain *and* an HTTP callback URL, and it doesn't raise the ceiling even if it worked. |
| **`docx` `iframe` block** | A web page embedded **inside a docx** — the only documented "embed a web page" primitive in the whole platform | — | **YES** for the framed page | `docx:document` | Yes (client behaviour unverified) | Curiosity, not a viewer. Also the closest thing to a card-embedded web view — and it is not in a card. |
| **多维表格 (Base)** | Structured rows/fields: text, attachment, URL, select, date, user | 500 records per `batch_create`; 20 000 records per table; 300 tables per Base [doc] | No | `bitable:app` or `base:record:create` | Yes | **Reject for a viewer.** It's a spreadsheet, not a document; text files don't map onto it. Fine for a structured listing if ever needed. |

---

## 1. In-card ceiling (Card JSON 2.0)

### 1.1 The catalog is exhaustive, and has no file/tree/iframe

[Card JSON 2.0 component overview](https://open.feishu.cn/document/feishu-cards/card-json-v2-components/component-json-v2-overview) [doc] enumerates, and this is the *complete* set:

- **Containers** — `column_set`, `loop`, `form`, `interactive_container`, `collapsible_panel`
- **Display** — `header`, `div`, `markdown`, `img`, `img_combination`, `person`, `person_list`, `chart`, `table`, `hr`
- **Interactive** — `input`, `button`, `overflow`, `select_static`, `multi_select_static`, `select_person`, `multi_select_person`, `date_picker`, `picker_time`, `picker_datetime`, `select_img`, `checker`

Verified negatives, all from the same page:

- **No file / document / attachment component.** The only way a card references an uploaded
  binary is `img`/`img_combination` (`image_key`) and the markdown-inline
  `<audio file_key='…'>` tag, whose `file_key` comes from
  [`POST /open-apis/im/v1/files`](https://open.feishu.cn/document/uAjLw4CM/ukTMukTMukTM/reference/im-v1/file/create)
  [doc]. A PDF therefore has no in-card representation [ours — ADR-0076, 2026-10-10].
- **No tree / no directory component.**
- **No iframe / web-view element.** Nothing in the catalog embeds a remote page.
- **No server-loaded dynamic select options.** The tag is literally `select_static` and its
  `options` field is a static inline array of `{text, icon, value}` objects, with no URL,
  no fetch, and no cascading/dependency field
  ([single-select dropdown doc](https://open.feishu.cn/document/feishu-cards/card-json-v2-components/interactive-components/single-select-dropdown-menu))
  [doc]. ADR-0078's "click → callback → server sends a new card version" is the documented
  pattern [ours — 2026-10-11].
- Client floor: JSON 2.0 needs Feishu client **≥7.20**; below that the title renders and the
  body shows an upgrade prompt [doc].

### 1.2 Markdown inline tags and extensions (authoritative list)

From [Rich Text (Markdown) component](https://open.feishu.cn/document/feishu-cards/card-json-v2-components/content-components/rich-text) [doc]:

- Base is **CommonMark** (spec 0.31.2), plus Feishu-specific HTML tags. All **standard
  Markdown** except `HTMLBlock`; the 2.0 structure adds **headings**, **blockquotes**,
  **inline code**, and **markdown tables** that 1.0 lacked.
- Supported HTML tags: `<br>`, `<br/>`, `<hr>`, `<hr/>`, `<person></person>`,
  `<local_datetime></local_datetime>`, `<at></at>`, `<a></a>`, `<text_tag></text_tag>`,
  `<raw></raw>`, `<link></link>`, `<font></font>` (nestable). The `[Differentiated Jump]($urlVal)`
  syntax is **removed** in 2.0 — replaced by `<link icon=… url=… pc_url=… >text</link>`.
- Feature tags: `<audio file_key='…' audio_id='…' show_time=… style='normal'|'speak' …>`,
  `<number_tag>0–99</number_tag>`, `![hover](image_key)`,
  `<at id=open_id|all|… ids=… email=…>`.
- Rendering divergences from CommonMark, stated on the page: one Enter = soft break (may be
  ignored), two = hard break; bold needs surrounding spaces; ordered/unordered lists need
  4-space indent levels and are client ≥7.6.
- **Markdown tables**: "Except for the header row, up to five rows of data is shown. Any data
  exceeding five rows will be paginated"; "A maximum of four tables can be placed in a single
  rich text component" [doc]. cola's `MAX_CARD_TABLES = 5` is the **whole-card** budget
  (measured 2026-09-22: a 6th table fails with `230099/11310`) [ours].
- Escaping: special characters must be HTML-entity escaped (`&#60;`, `&#42;`, `&#95;`,
  `&#45;`, …) — cola's `sanitize.rs` policy [ours].

### 1.3 Fenced code-block languages and where highlighting happens

The same page lists the supported languages verbatim [doc], case-insensitive: `plain_text`,
`abap`, `ada`, `apache`, `apex`, `assembly`, `bash`, `c_sharp`, `cpp`, `c`, `cmake`, `cobol`,
`css`, `coffee_script`, `d`, `dart`, `delphi`, `diff`, `django`, `docker_file`, `erlang`,
`fortran`, `gherkin`, `go`, `graphql`, `groovy`, `html`, `htmlbars`, `http`, `haskell`, `json`,
`java`, `javascript`, `julia`, `kotlin`, `latex`, `lisp`, `lua`, `matlab`, `makefile`,
`markdown`, `nginx`, `objective_c`, `opengl_shading_language`, `php`, `perl`, `powershell`,
`prolog`, `properties`, `protobuf`, `python`, `r`, `ruby`, `rust`, `sas`, `scss`, `sql`,
`scala`, `scheme`, `shell`, `solidity`, `swift`, `toml`, `thrift`, `typescript`, `vbscript`,
`visual_basic`, `xml`, `yaml`.

Notes for #669:

- There is **no `jsonc`, `ts`, `tsx`, `jsx`, `shellscript`, `ini`, `toml`-adjacent `dotenv`,
  `text`, `log`, `diff`-adjacent `patch`** — an unknown token degrades to `plain_text`
  ("If not specified, defaults to Plain Text"), it does not error [doc]. The extension→token
  map for the viewer must fall back to `plain_text` for `.ts`, `.tsx`, `.log`, `.ini`, `.env`, etc.
- **Highlighting is client-side only.** The API accepts a plain string; nothing in the request
  carries a token stream. By contrast cc-connect *does* tokenize code blocks itself when it
  builds cards (`platform/feishu/feishu.go:2306` comment: "code_span, code_block (with
  tokenized contents)") [ours] — that is cc-connect's choice, not a Feishu feature.
- Four or more leading spaces also produce a code block (CommonMark indented code block) [doc].

### 1.4 Size / element limits — documented vs measured

| Limit | Value | Provenance |
|---|---|---|
| Card message request body | **30 KB** — "The maximum size of a card message or rich text message request body cannot exceed 30 KB" | [doc] [Send a message](https://open.feishu.cn/document/uAjLw4CM/ukTMukTMukTM/reference/im-v1/message/create) |
| Rich-text (`post`) message body | **30 KB** (same sentence) | [doc] |
| Text message body | **150 KB** | [doc] |
| Elements/components per card | **200** — "supports up to 200 elements … or components for a card"; failure surfaces as `230099` / `11310` "element exceeds the limit" | [doc] [Card JSON 2.0 structure](https://open.feishu.cn/document/feishu-cards/card-json-v2-structure) + [send-message error table](https://open.feishu.cn/document/uAjLw4CM/ukTMukTMukTM/reference/im-v1/message/create#-f367d31) |
| Markdown tables per **card** | 5 | [ours] measured 2026-09-22 (`src/feishu/card/sanitize.rs:45`) — the doc only states "four per rich text component" |
| Markdown table **body rows** | 49 | [ours] measured 2026-09-22 (`sanitize.rs:50`): 50 fails, 49 renders. **Not documented at all.** |
| `table` component: per card / columns / page size | ≤5 components, ≤50 columns, `page_size` 1–10 | [doc] [Table component](https://open.feishu.cn/document/feishu-cards/card-json-v2-components/content-components/table) |
| Per-element text | **No platform cap is documented.** A single 8000-char ASCII / 3060-char CJK element renders fully via create and PATCH; limits are 30 KB total + 200 elements | [ours] measured 2026-09-18 — AGENTS.md pitfall `card-budgets`, and the comment on `src/feishu/card/mod.rs:249` explicitly warns not to "fix" the 3000-char `MAX_ELEMENT_TEXT_CHARS` as a platform cap |
| Card nesting depth | **Not documented** | [unverified] — issue #279 lists it as a known gap |
| `loop` container | Builder-tool only, **not constructible in card JSON** | [doc] catalog + ADR-0078 |
| Image upload | ≤10 MB; jpg/jpeg/png/webp/gif/bmp/ico/tiff/heic (tiff/heic → jpg); GIF ≤2000×2000, others ≤12000×12000 | [ours] ADR-0076, 2026-10-10 |
| File upload | ≤30 MB, non-empty; `file_type` ∈ `opus`/`mp4`/`pdf`/`doc`/`xls`/`ppt`/`stream` | [doc] [Upload file](https://open.feishu.cn/document/uAjLw4CM/ukTMukTMukTM/reference/im-v1/file/create) |

Known documented-vs-actual divergences to carry into #279:
1. The per-card markdown-table cap is 5, not the 4 the rich-text page states per component.
2. Markdown table body rows cap at 49 — documented nowhere.
3. cola's "30 KB" operational ceiling is really ~25 KB (`MAX_CARD_JSON_CHARS = 30_000 − 5_000`)
   because the size estimate must trail the serialized body [ours].

### 1.5 Other card facts worth knowing for a viewer

- **`collapsible_panel`** folds long text. A `form` **cannot nest inside it** [ours — ADR-0078,
  2026-10-11]; `img` **can** [ours — ADR-0076].
- **`img_combination`** lays out 2 / 3 / 6 / 9 images (`double`/`triple`/`bisect`/`trisect`),
  recommended ≤10 MB and within 1500×3000 px [doc].
- **Card callback** is 3 s, no retry [ours — ADR-0078].
- **Streaming / typewriter (the "AI card")**: `config.streaming_mode: true` plus
  `streaming_config {print_frequency_ms (default 70), print_step (default 1), print_strategy
  ('fast'|'delay')}`; the platform computes the incremental part and animates it. Requires the
  **card entity** flow — `POST /open-apis/cardkit/v1/cards` to create an entity, then send
  `{"type":"card","data":{"card_id":…}}`; a card entity can only be sent once and must be sent
  by the creating app. Scopes: `im:message`, `im:message:send_as_bot`, **`cardkit:card:write`**.
  Rate: 10 ops/s per card entity. Caveats: while streaming, the server **cannot** update the card
  from a user callback; a streaming card **cannot be forwarded**; auto-closes 10 min after last
  activation. Component-level partial updates work with *or* without streaming mode.
  ([Streaming card updates](https://open.feishu.cn/document/feishu-cards/streaming-updates-openapi-overview))
  [doc]. This is a *presentation* upgrade for cola's existing streaming turn card, not a way to
  show more content.

---

## 2. Message types a bot can send

Full set from [Send a message](https://open.feishu.cn/document/uAjLw4CM/ukTMukTMukTM/reference/im-v1/message/create)
and [Message management overview](https://open.feishu.cn/document/server-docs/im-v1/message/intro)
[doc]: `text`, `post`, `image`, `file`, `audio`, `media`, `sticker`, `interactive`,
`share_chat`, `share_user`, `system`.

There is **no** "custom" or "card-with-embedded-mini-program" `msg_type`. What a card *can* do
is open one via `behaviors: [{"type":"open_url", …}]`, including an `lark://` applink.

Per type, and what it can show that a card cannot:

- **`text`** — **150 KB** body [doc], the largest payload of any message type. Supports
  `\n`, `<at user_id=…>`, `**bold**`, `<i>`, `<u>`, `<s>`, `[text](link)` [doc]. No markdown
  tables/headings/code. **Editable in place** via `PUT /open-apis/im/v1/messages/:message_id`,
  `msg_type` ∈ `text|post`, **max 20 edits**, within an admin-configurable edit window
  ([Edit message](https://open.feishu.cn/document/uAjLw4CM/ukTMukTMukTM/reference/im-v1/message/update))
  [doc]. → This is the honest "big raw dump" lane.
- **`post` (rich text)** — **30 KB** body [doc]. The key discovery for this question: the `md`
  tag supports **CommonMark 0.31 + GFM** — tables, task lists, strikethrough, autolinks,
  footnotes, fenced code blocks with a language hint — plus Feishu extensions for `@`, `@all`,
  and `![alt](img_v3_xxx)` images. Explicitly **unsupported**: Setext headings, indented code
  blocks, arbitrary raw HTML, automatic email links; email autolinks and footnote
  click-to-jump are unsupported; `md` must not share a line with other tags; tables must be
  separated by blank lines. Also `code_block` and `hr` tags.
  ([Send message content structure](https://open.feishu.cn/document/uAjLw4CM/ukTMukTMukTM/im-v1/message/create_json))
  [doc]. → **A card-free, in-chat markdown renderer.** cc-connect uses it as the card's overflow
  valve [ours].
- **`file`** — content is just `{"file_key": …}` [doc]. There is **no API parameter for a
  preview, thumbnail, title, or excerpt** [doc]. Upload is ≤30 MB via
  [`POST /open-apis/im/v1/files`](https://open.feishu.cn/document/uAjLw4CM/ukTMukTMukTM/reference/im-v1/file/create);
  the upload's `file_type` must match the message type or the send fails with `230055` [doc].
  The **client-side** 在线预览 behaviour is governed by
  [文件上传、在线预览的大小及格式要求](https://www.feishu.cn/hc/zh-CN/articles/360049067549)
  — **that help-centre page is client-side rendered and the full preview-format list still
  has not been extracted here.** A search-result snippet from that same
  article states "支持预览压缩包内文件列表和目录结构,不支持预览压缩包内文件" (archives: directory
  listing is previewable, contents are not) — treat as weak evidence, not a citation.
  **Measured 2026-10-18 (#774)** — in a real chat, desktop *and* mobile, sent through cola's own
  upload path (`file_type=stream`, the real file name) — for the two formats cola cares about:
  - a **`.html`** file renders as a page in the file message's preview; its inline `<script>`
    **executes**; the client's own "code view" toggles back to the source. The help centre's
    [在飞书中查看 HTML 文件](https://www.feishu.cn/hc/zh-CN/articles/052565607602) caps that
    preview at **12 MB** (a display cap — a larger file still uploads and downloads). Desktop
    and mobile both render; on mobile the reference-quote bar was flattened to an indent.
    Feishu's own guidance is that a page with heavy front-end logic or many external assets
    belongs on 妙搭 instead, which matches the 12 MB single-page framing.
  - a **`.md`** file renders as rich text, and **without opening it** the message already shows
    a rendered preview excerpt.
  - the same markdown through a card `markdown` element and through a `post` `md` tag renders
    **equivalent** to that `.md` file preview — the file preview is not a better renderer, only
    a surface that needs no card.

  These are `[ours]` measurements from one client version (the V7.71 era), not a documented
  format list; they are evidence for `.html` and `.md` only.
- **`image`** — `{"image_key": …}`, same 10 MB upload. Nothing a card can't already do.
- **`audio` / `media`** — `{"file_key": …}` (+ optional `image_key` cover for `media`). Audio
  must be uploaded as `opus`, video as `mp4` [doc]. A card can *also* embed an audio player via
  the markdown `<audio>` tag with the same `file_key` [doc] — so `audio` beats the card only in
  that it's a standalone message.
- **`sticker`** — only stickers *received by the bot* can be re-sent [doc]. Irrelevant.
- **`share_chat` / `share_user`** — business cards. Irrelevant.
- **`system`** — only `type: "divider"`, p2p only, extra scope
  `im:message:send_sys_msg`, divider text ≤20 chars / 10 CJK chars [doc]. Irrelevant.

**Is there any bot-sendable message that renders a file's content inline?** No. Every
`msg_type` carries either a key to already-uploaded bytes (`image`/`file`/`audio`/`media`/
`sticker`) or literal text (`text`/`post`/`interactive`/`system`). The only ways to get bytes
*rendered* as text are (a) a card `markdown` code block, (b) a `post` `md`/`code_block` block,
or (c) a `text` message — i.e. cola re-encodes the file's content itself, which is exactly what
`/cat` proposes [doc, exhaustive over the enum].

Rate limits to keep in mind for any of these lanes: send is **5 QPS per user / 5 QPS per
group**, API-level 1000/min & 50/s [doc].

---

## 3. 云文档 (docx / Drive import) — the real escape hatch

### 3.1 The documented pipeline

1. **Upload** —
   `POST /open-apis/drive/v1/medias/upload_all` with `parent_type=ccm_import_open`,
   `file_name`, `size`, `extra={"obj_type":"docx","file_extension":"md"}`, `file`.
   Returns a `file_token`.
   ([Upload media](https://open.feishu.cn/document/uAjLw4CM/ukTMukTMukTM/reference/drive-v1/media/upload_all),
   [Import file overview](https://open.feishu.cn/document/uAjLw4CM/ukTMukTMukTM/reference/drive-v1/import_task/import-user-guide))
   [doc]. Upload media is capped at **20 MB**, 5 QPS, 10 000 calls/day [doc].
2. **Create import task** — `POST /open-apis/drive/v1/import_tasks`
   (`file_extension`, `file_token`, `type` ∈ `docx|sheet|bitable`, optional `file_name`,
   `point:{mount_type:1, mount_key:<folder token, empty = root>}`), returns a `ticket`
   ([Create an import task](https://open.feishu.cn/document/uAjLw4CM/ukTMukTMukTM/reference/drive-v1/import_task/create))
   [doc].
3. **Poll** `GET /open-apis/drive/v1/import_tasks/:ticket` [doc].
4. **Share** — `POST /open-apis/drive/v1/permissions/:token/members`
   (`member_type=openid`, `perm=view|edit|full_access`, `type=docx`) if the reader is not the
   owner ([Add permissions](https://open.feishu.cn/document/server-docs/docs/permission/permission-member/create))
   [doc]. The document URL is `https://<tenant-domain>/feishu.cn|feishu.cn/docx/<document_id>`
   [doc, from the create response].

Alternative to steps 1–3: `POST /open-apis/docx/v1/documents` creates an empty docx, then
`POST /open-apis/docx/v1/documents/:document_id/blocks/:block_id/children` writes blocks
one at a time ([Create a document](https://open.feishu.cn/document/server-docs/docs/docs/docx-v1/document/create),
[Create blocks](https://open.feishu.cn/document/ukTMukTMukTM/uUDN04SN0QjL1QDN/document-docx/docx-v1/document-block-children/create))
[doc]. The block API supports `code` blocks with a `CodeLanguage` enum (same ~70-language set
as the card, numeric 1–N), `table`, `image`, `file`, and **`iframe`**
([Block data structure](https://open.feishu.cn/document/ukTMukTMukTM/uUDN04SN0QjL1QDN/document-docx/docx-v1/data-structure/block))
[doc]. This path is **much** worse for a 5000-line log: one block per line, 3 calls/s per app
(`99991400`), and hard structural caps (`1770004 too many blocks`, `1770005 too deep`,
`1770007 too many children`, `1770033 raw content size exceed limited`, `1770034 operation count
exceed limited`) [doc]. **Prefer import over block-writing.**

### 3.2 Accepted formats and caps [doc]

| File | Extension | Max import size | Target types |
|---|---|---|---|
| Word 2007+ | `docx` | 600 MB | docx |
| Word 97-2003 | `doc` | (same row) | docx |
| Text | `txt` | **20 MB** | docx |
| Markdown | `md`, `mark`, `markdown` | **20 MB** (shared cell) | docx |
| HTML | `html` | **20 MB** (shared cell) | docx |
| Excel 2007+ | `xlsx` | 800 MB | sheet, bitable |
| CSV | `csv` | sheet 20 MB / bitable 100 MB | sheet, bitable |
| Excel 97-2003 | `xls` | 20 MB | sheet |

Failure modes: `1069909` file >20 MB, `1069910` extension mismatch (the `md`/`mark`/`markdown`
distinction bites), `1069911` type mismatch, `1069913` **uploaded file token expired after 5
minutes**, `1069908` mount point not found / no permission, `1069912` folder not exist, `1069923`
rate-limited [doc].

### 3.3 Scopes needed [doc]

- `POST /drive/v1/import_tasks` → **either** `docs:document:import` **or** `drive:drive`
- `POST /drive/v1/medias/upload_all` → **one of** `bitable:app`, `docs:doc`,
  **`docs:document.media:upload`**, `drive:drive`, `sheets:spreadsheet`
- `POST /drive/v1/permissions/:token/members` → `docs:permission.member:create` or `drive:drive` …
- `POST /docx/v1/documents` → `docx:document` or `docx:document:create`

For a `tenant_access_token`, **only folders the app itself created** can be used as
`folder_token` for docx create [doc], and an app must be *added as a document application* on the
doc before collaborator APIs succeed (`1063002`) [doc].

### 3.4 Is it viable for "show a 5000-line log nicely"?

**Yes for `.md`/`.txt`/`.html`; no for arbitrary code.** A 5000-line log is well under the
20 MB import cap, imports asynchronously in seconds, and yields a link the client renders as a
first-class document with the user's own Feishu navigation, search, comment, and share. The
costs are real and should be stated in the spec rather than discovered later:

- a whole new subsystem (upload → ticket → poll → permission → URL) plus 2–3 new scopes;
- **only 8 extensions are importable**, and none of them is a source-code extension — so a
  `.rs`/`.py`/`.log` viewer cannot use this path without renaming or wrapping the file;
- the created doc **accumulates in the user's My Space** (no auto-cleanup API in this path);
- the doc is a *copy*, not a live view of the file — a `/cat` viewer wants a re-read;
- the ticket poll is async, so the first send of a large file has latency cola must budget for
  against the card-callback window.

**Recommendation:** keep it out of #669. It is the right answer for an explicit
「在云文档中打开」 escape hatch for a *text/markdown/log* file when the card and the `post`
fallback both overflow — a natural follow-up spec, not this one.

---

## 4. H5 / 网页应用 / 小程序

### 4a. Can a card button or message open an arbitrary URL?

Yes. `behaviors: [{"type":"open_url","default_url":…,"pc_url":…,"ios_url":…,"android_url":…}]`
([Button component](https://open.feishu.cn/document/feishu-cards/card-json-v2-components/interactive-components/button))
[doc]; `card_link` does the same for the whole card, and can be disabled per-platform with
`lark://msgcard/unsupported_action` [doc]. Markdown `[text](url)` and `<link url=…>` likewise;
the docs say **"Hyperlinks must include a schema to be effective; currently, only HTTP and
HTTPS are supported"** [doc]. The applink
`https://applink.feishu.cn/client/web_app/open?appId=…&mode=window|sidebar|appCenter|…`
opens a registered H5 app in a chosen container [doc].

### 4b. Does `http://localhost:PORT` work?

- **Documented, in exactly one place**: the Web-application homepage field of the developer
  console — *"If the company's network security policy does not allow the use of reverse proxy
  tools, or if you want to quickly experience the access process, you can temporarily use the
  local environment to debug and fill in `http://127.0.0.1:3000`."*
  ([Create and configure a web app, step 1](https://open.feishu.cn/document/uYjL24iN/uMTMuMTMuMTM/development-guide/step1))
  [doc]. The same page is explicit that *"In the actual production environment, the address
  configured here needs to be a public network address"* and suggests ngrok otherwise [doc].
- **Nothing in the docs says a card `open_url` or a plain markdown link to `http://localhost`
  is blocked or allowed** [unverified]. Empirically plausible on a same-machine desktop client
  (the in-app webview is a local process on the same host) and certain to fail on mobile or on
  any other user's client — but **this is an inference, not a documented fact, and this
  research did not measure it.** If it matters for a future shell feature it must be *probed*,
  not assumed.
- For **link preview** it is definitively out: URL rules require a TLD + second-level domain and
  **"IP addresses cannot be used in the `Host` section"** [doc] — `localhost` has no TLD.

### 4c. 网页应用 (H5) and 小程序 (mini-app): registration/hosting

- **网页应用**: add the *Web application* capability to an app, configure Desktop/Mobile
  homepage URLs, optionally JSAPI auth and SSO, then **publish a new app version approved by
  the enterprise admin** ([Web app overview](https://open.feishu.cn/document/client-docs/h5/introduction),
  [step 1](https://open.feishu.cn/document/uYjL24iN/uMTMuMTMuMTM/development-guide/step1),
  [embed into workbench](https://open.feishu.cn/document/embed-web-app-into-feishu-workbench/introduction))
  [doc]. The overview's own "Functional characteristics" says the app "is integrated with Feishu
  through their **public access URLs**" and its "Usage limitations" warn about network quality
  [doc]. → **Requires public ingress.**
- **小程序 (gadget)**: a separate app identity with its own `appId`, developed against Feishu's
  mini-program runtime and opened via
  `https://applink.feishu.cn/client/mini_program/open?appId=…&mode=…` [doc]. Chat surfaces that
  can open one are the "gadget message card (cardlink)" in p2p and group chats (start scenes
  `1007`–`1010`) [doc]. → **Requires its own published mini-app plus public hosting.**

### 4d. Is there a documented way to embed a web page *inside a card*?

**No.** No card component takes a URL. The two nearest primitives are:

- the whole-card `card_link` / button `open_url`, which *navigates away* from the card; and
- the **docx `iframe` block** — `component: {type: IframeComponentType, url}`, creatable and
  readable but **not editable** via the block API
  ([Block data structure](https://open.feishu.cn/document/ukTMukTMukTM/uUDN04SN0QjL1QDN/document-docx/docx-v1/data-structure/block),
  [Document overview](https://open.feishu.cn/document/server-docs/docs/docs/docx-v1/docx-overview))
  [doc] — which embeds a web page **in a 云文档**, not in a card, and needs the framed URL to be
  publicly reachable.

---

## 5. 链接预览 (link preview)

**Documented, real, and — for cola — closed.** From
[Link preview development guide](https://open.feishu.cn/document/development-link-preview/link-preview-development-guide)
[doc]:

- An app enables the **Link preview** capability, registers up to **10 URL rules**, subscribes to
  the **`url.preview.get`** event, and answers within **3 seconds** with `{inline: {i18n_title,
  image_key}, card?: {type: "template"|"raw", data: {...}}}`.
- Triggers: a link inside a **chat message** or a **group pin** (`host: "im_message"` /
  group top).
- A bare URL **does** get parsed — into an icon+title "inline" text, or, optionally, a **card**.
  So yes, a URL in a card's markdown does not itself render a preview card; the preview is
  produced by the app's callback for URLs the app registered.
- **The blocking constraints for cola:**
  1. URL rules **cannot use IP addresses** and must contain a TLD + second-level domain, no
     wildcards in those levels [doc] — no `localhost`, no `192.168.x.x`.
  2. The callback is an **HTTP endpoint** the platform POSTs to [doc]. Whether `url.preview.get`
     is also offered on the long-connection (WebSocket) event subscription — which is how cola
     receives events — is **[unverified]**; the guide only ever describes a callback address, and
     an HTTP endpoint on a localhost-only box is not reachable by Feishu's servers.
  3. Only the **published** version supports it; test versions cannot configure link preview [doc].
  4. Rules are **exclusive per tenant** across self-built apps [doc].
- **And it would not raise the ceiling anyway**: the preview body is a card (`type: "raw"` returns
  complete card JSON), so it inherits the same 30 KB / 200-element budget [doc].

**Conclusion: link preview is not an escape hatch for cola.** It is a surface for making
*your own already-public web service* look nice in chat, which presupposes the thing cola does
not have.

---

## 6. Anything else worth knowing

- **`chart`** — VChart-based, `chart_spec` is validated server-side (`230099` / `200550` with an
  `ErrPath`), **no JavaScript syntax allowed**, media queries auto-appended unless you set
  `"media":[]`, ≤5 charts recommended, `aspect_ratio` ∈ 1:1/2:1/4:3/16:9, `height` ≤999px,
  `preview` toggles the enlarge/full-screen affordance. VChart version is pinned per client
  range (1.2.2 on 7.1–7.6 up to 1.12.3 on ≥7.27) [doc]. It is a **data-visualisation** component,
  not a document renderer — a `chart_spec` cannot render arbitrary JSON or source.
- **`table`** — root-level only (cannot nest in other components, cannot contain components);
  ≤5 per card; ≤50 columns; `page_size` 1–10; `row_height` ∈ `low|middle|high|auto|[32,124]px`
  with `row_max_height` [32,999]px; `freeze_first_column`; cell `data_type` ∈
  `text` (≥7.4) / `lark_md` (≥7.10) / `options` / `number` / `persons` / `date` (≥7.6) /
  `markdown` (≥7.14). Cell content **clips at the end** and the reader recovers it by hover or
  click on desktop [doc]. **Cells are not buttons** — so a listing card cannot make rows
  tappable here [ours — ADR-0078, 2026-10-11].
- **`img_combination`** — `double` (2), `triple` (3), `bisect` (6, two per row), `trisect` (9,
  three per row); excess images are dropped, not paginated [doc].
- **Card streaming / 打字机** — see §1.5. It changes how text *appears*, never how much fits.
- **多维表格 (Base)** — `POST /open-apis/bitable/v1/apps` + tables + `batch_create` records
  (≤500/call, ≤20 000 records/table, ≤300 tables/Base; scopes `bitable:app` or
  `base:record:create`) [doc]. A spreadsheet is the wrong shape for reading a file; keep it out
  of #669.
- **在线预览 for common types** — see §2: the authoritative format list is in
  [文件上传、在线预览的大小及格式要求](https://www.feishu.cn/hc/zh-CN/articles/360049067549),
  which is **client-side rendered and was not extractable** during this research
  ([unverified]). Practical consequence for #669: the File Message lane must keep treating the
  preview as **opaque** — cola should not promise "PDF renders inline" or "text files preview"
  in user-facing copy without measuring it per type.

---

## 7. What we could not verify

1. **The client's 在线预览 format list for a `file` message.** The help-centre article
   (`/hc/zh-CN/articles/360049067549`) renders client-side; four fetch strategies returned an
   empty body. No primary-source list obtained. Any claim about "PDF previews / office previews /
   text previews" is **[unverified]**.
2. **Whether a card `open_url` to `http://localhost:PORT` works** from a same-machine desktop
   client. Documented only as a developer-console *web-app homepage* debugging value; nothing
   documents it for links/buttons, and nothing documents mobile or a second user's client.
   **Must be probed**, not assumed.
3. **Whether `url.preview.get` is available on the long-connection (WebSocket) event
   subscription**, which would be the only way cola could serve it without public ingress. The
   link-preview guide only describes an HTTP callback address.
4. **Card container nesting depth** — undocumented; already flagged as a gap in issue #279.
5. **Whether the markdown-table "4 per rich-text component" cap stacks with a per-card cap**
   beyond our measured 5/card, and the exact body-row cap semantics (49 measured; 50 fails —
   but the failure text, `element exceeds the limit`, is shared with other limits).
6. **Whether a `post` message's `md` block has any per-element or per-table cap.** The docs give
   only the 30 KB body limit. cc-connect's use as a card-overflow valve implies it does not, but
   that is inference from their code, not a measurement.
7. **`msg_type:"file"` in-thread rendering and topic behaviour** beyond what ADR-0076 already
   recorded (reply container placement) — not re-measured here.
8. **Whether the client renders a docx URL shared in a chat as a rich link-preview card.**
   Feishu reserves its own domains from user-registered link-preview rules [doc], implying native
   handling, but the exact bubble rendering is client behaviour and is **[unverified]**.

---

## Provenance of the "ours" claims

| Claim | Source |
|---|---|
| Card JSON 2.0 has no file component; `img` fits inside `collapsible_panel` | `docs/adr/0076-file-content-delivery.md` (2026-10-10), "Feishu's surfaces (confirmed 2026-10-10)" |
| Image ≤10 MB / dimension caps; file ≤30 MB; uploaded key stable across PATCHs | same ADR, Context |
| 30 KB card, 200 elements, `11310`; an 8000-char ASCII / 3060-char CJK element renders fully; 3000 chars is **cola's** budget, not a platform cap | `AGENTS.md` pitfall `card-budgets` (measured 2026-09-18) + the comment on `src/feishu/card/mod.rs:249` |
| Card callback 3 s, no retry; no file-picker/tree/dynamic options; `form` cannot nest in `collapsible_panel`; `loop` is builder-tool only; table cells are not buttons | `docs/adr/0078-directory-browser-card.md` (2026-10-11), Context |
| `MAX_CARD_TABLES = 5` (whole card), `MAX_TABLE_ROWS = 49` | `src/feishu/card/sanitize.rs:41-50`, both "measured 2026-09-22" |
| `FEISHU_CARD_LIMIT_BYTES = 30_000`, `MAX_CARD_JSON_CHARS = 30_000 − 5_000`, `MAX_CARD_COMPONENTS = 150` | `src/feishu/card/mod.rs:229-246` |
| `post` as the card-overflow fallback (5-table limit → `11310`) | `platform/feishu/feishu.go:2941-2960` in `/root/workspace/dev/cc-connect`; cc-connect also tokenizes code blocks itself (`feishu.go:2306`) |
| Surfaces cc-connect actually uses: `text`, `post` (with `md`), `interactive`, `image`, `audio`, `media`, `file`; it downloads inbound `file` messages | `platform/feishu/feishu.go` (`MsgTypeInteractive`, `MsgTypePost`, `MsgTypeFile`, `MsgTypeAudio`, `MsgTypeMedia`, `MsgTypeImage`) |
| Lark SDK message-type constants | `/root/go/pkg/mod/github.com/larksuite/oapi-sdk-go/v3@v3.5.3/larkim/` |
