# Host files are browsable and viewable from Feishu: a cola-local read, an advisory path scope, and a three-lane content ladder

Everything a **Host** machine holds was reachable from Feishu only through the model: a
**File Content** a tool happened to return (ADR-0076) or a text the agent chose to paste.
A text file the agent read showed its source in a **Tool Panel** clipped to
`TOOL_OUTPUT_MAX_CHARS`; a file the agent *wrote* reached Feishu not at all — it is no
transcript file block; and nothing let a user look at a path the model never touched. The
**Directory Browser** (ADR-0078) could walk the whole filesystem but listed **directories
only**: it could take you to the folder, never show you the file. Shaped by the grilling of
#669 (and its research note,
`docs/research/feishu-rich-surfaces/r1-display-surface-inventory.md`), cola gains a read
surface of its own: the browse card lists files and a row opens one, and `/cat <path>` opens
one directly — reading the bytes locally, on neither **Generation**, with no model in the
loop.

## Context

- **The platform ceiling is confirmed, not assumed.** Card JSON 2.0's component catalog is
  exhaustive: no file/document component, no tree, no iframe/web-view element, no
  server-loaded select options. The hard budget is **30 KB** per card or rich-text message
  body, **200** elements/components, **5** tables per card, and a markdown table shows only
  five data rows (49 body rows is our measured cap, undocumented upstream). Fenced code
  blocks carry a language hint from a fixed ~70-token list with client-side highlighting,
  and an unknown token degrades to `plain_text` rather than failing.
- **Two surfaces beat the card for *why* content overflows, and neither needs public
  ingress** (research note, §Verdict): a **`post`** message's `md` tag renders CommonMark
  0.31 + GFM, and is **reported** to do so without the card's 200-element cap, 5-table cap and
  per-element clipping — a claim inferred from cc-connect's use, never measured here, which
  is why the spec (#669's decision 11) keeps it unverified and decision 5 below bounds the
  lane —
  while a `text` message body allows 150 KB. `post` and `text` are also editable in place
  (`PUT /im/v1/messages/:id`, at most 20 edits). Everything genuinely richer is closed to a
  bot with no public address: 网页应用 (H5) documents that its production homepage "needs to
  be a public network address" (`127.0.0.1` appears only as a local-debugging stopgap),
  小程序 is a second published app, and 链接预览 requires a URL rule with a TLD + second-level
  domain — IP addresses are explicitly rejected — *and* a 3-second HTTP callback. The only
  documented "embed a web page" primitive is a docx `iframe` block, not a card.
- **The Directory Browser already reads the filesystem locally.** ADR-0078's render scans
  `read_dir` (capped at `BROWSE_SCAN_CAP`), `is_dir()`-probes symlinks and `stat`s each child
  for a `.git` entry. Reading a file's **bytes** locally is the same act on the same machine —
  not a new capability class, and not a new trust boundary.
- **cola is on the server's host.** Discovery resolves the server from local processes — a
  classic `serve --port P` through its argv and environment, a V2 `serve --service` through its
  registration file (`AGENTS.md` pitfall `attach`) — so the attached server is always a local
  process; a Session's directory is a local path. There is no remote-server deployment to design
  for.
- **Every Feishu card is readable by the whole chat.** The **Access List** (ADR-0035) gates
  who may *click*, not who may *read* a card — so a viewer card publishes a file's content to
  every member of a group chat. The same exposure already applies to directory names, and
  #673 established that a *sound* path boundary cannot be built with `std` (six escape
  cases; it needs `openat`/`O_NOFOLLOW` fd-relative traversal). #673 owns that work.

## Decision

1. **A cola-local read, not a server route.** The bytes come from the local filesystem
   (`std::fs`, in a bounded blocking task), not from V2's `GET /api/fs/read` nor V1's
   `/file/content`. Two
   consequences are accepted outright: the read is **generation-neutral** (no V1/V2 branch,
   and V1's richer `text|binary` answer costs us nothing because V1 is not consulted), and it
   is **backend-neutral** — it depends on no OpenCode concept, which is what keeps the door
   open for a non-OpenCode **Backend** later. The price is that **type detection is cola's**:
   see §6.
2. **Two entry points, one browser.** The **Directory Browser** card lists directories *and*
   files; a file row opens it. A new **Command** `/cat <path>` opens a known path in one step
   — the form that gets its value from the agent, from a log, or from the user's memory —
   and a path inside `/cat` resolves relative to the Session's directory with `~` expanded.
   `/cat <directory>` opens the browse card at that directory instead of erroring, and a bare
   `/cat` answers usage. A path that does not exist, a path cola cannot read, and an empty
   file each answer **distinctly**, naming the path that was resolved — an unopenable file is
   a stated reason, never an empty card, and `/help cat` documents the command. `/ls` is
   deliberately **not** added: the browse card already owns listing, and a second path
   language would cost more than it saves.
3. **The viewer is a command card.** The **File Viewer** replies to the command (or replaces
   the browse card in place, exactly as the browse card replaces the Recent Directories card)
   and is a member of the *command card* family — `/dir`, `/switch`, `/help` — a standalone
   message: it joins no **Card Chain**, is never written to the **Chain Record**, and is
   never reaped. Opened from the browse card it carries 「← 返回目录」 and round-trips that
   card's state (browsed directory, page, filter, hidden toggle) through its own button
   payloads, so returning restores the listing exactly (ADR-0052's statelessness). A `/cat`
   card, which has no listing to return to, carries 「📂 所在目录」 instead. Every viewer
   **names the file it shows** — path, size and type — so two similarly named files are never
   ambiguous, and the path is the one that was resolved, not the one that was typed. The
   consequence is accepted: the viewer is not collected when the conversation moves on, so a
   chat can accumulate them.
4. **The path scope is advisory, and says so.** Every path resolves relative to the Session's
   directory (the conversation's current **Project** when the Session is still **Pending**),
   with `~` expanded and absolute paths passed through. This is a **privacy hint, not a
   security control**: it is the same reach the Directory Browser already has, and #673's
   finding stands — a sound boundary needs fd-relative traversal and belongs there. The
   **Access List** remains the only gate on who may act, and the ADR states plainly that a
   group chat's members can read whatever a Host opens.
5. **A three-lane content ladder, by what the content *is*, not how big it is.** A preview
   that fits is a card; a preview the card **structurally** cannot hold — past the element
   budget, past five tables, past the 49-row table cap — goes out as one **Rich Text
   Message** rather than a half-rendered or paginated-around card; and the file's whole bytes
   are always available as a **File Message** through ADR-0076's existing ladder (uploaded
   once, 30 MB cap, a `未发送` tracking line beyond it). **The middle lane is bounded**: a
   `post` body carries the same **30 KB** cap as a card, so content whose rich-text body would
   exceed that does not take it — it goes to the File Message lane, with the card saying so.
   **The whole-bytes lane is a user action, not a promise**: a viewer carries 「发为文件」 when
   its content has not already landed outside the card. The action uploads the file and posts it
   as a File Message; it is the way to send a fitting preview's bytes and the rescue for an
   overflow lane that did not land. **Deliver once, by bytes:** when the bytes have already
   landed as a **File Message**, the viewer omits 「发为文件」 rather than offer a duplicate (as
   does an ambiguous send whose retry might duplicate); its tracking line names the message
   lane. A **Rich Text Message** is a rendered preview, not the bytes, so that viewer still
   carries the action — the original file still needs a way out. An embedded image remains
   visible on the viewer
   card and keeps the explicit File Message action. **That transfer never runs inside the card
   callback.** The click records the
   intent and acknowledges; a bounded, timed-out step performs the upload and the send off
   the callback path, and the card reports the outcome — including a failure or a timeout —
   so a 30 MB upload can neither die with the callback's 3 s budget nor leave the clicker
   with a dropped result (CODING_STANDARDS.md's callback-network rule). Viewer opens also
   acknowledge first and run their `stat` plus classification in a bounded background task:
   even a capped local read can stall on a slow or network-mounted filesystem, and the task
   patches the clicked card after its lane settles. Directory navigation keeps its existing
   bounded scan discipline. A text file's card preview is a
   fenced code block with a language hint, paged by **lines** with the shared pager control —
   **except a delimited-text file (`.csv`, `.tsv`), whose preview is a markdown table**, since
   a table is what those bytes *are* and the platform renders one natively; that lane is
   where the card's table caps bite, so a table too big for the card takes the Rich Text
   Message lane above rather than being silently cut to five rows. An image within Feishu's
   caps is embedded by ADR-0076's image path and reuses its content-hash upload cache, so
   opening the same image twice uploads once.
6. **Type detection is local, explicit, and bounded.** Magic bytes (`infer`) decide image and
   known-binary types first, then UTF-8 validity decides text; an image takes ADR-0076's
   embed path, any other binary takes the File Message path. A text file larger than the
   **preview read cap** (8 MB) is not previewed at all — it goes straight to a File Message
   under its **text** label (`文本（过大，未预览）`), because the CONTENT decides the type and a
   big UTF-8 file is a text file, never 「二进制」 — and the 30 MB upload cap is unchanged. Both
   are constants, not policy. **Classification reads up to the 30 MB upload cap plus
   one detection byte**: a file within that cap is read once — and those same bytes both
   decide text vs binary and reach the delivery path, so the card can never describe
   bytes the upload does not carry — and a file past it is named from a bounded header
   alone, because nothing more is read — so telling a large text
   file from a binary is bounded by the constant, never by the file. Emptiness and the
   reported size come from the **bytes actually read**, never from the `stat` size: a
   pseudo-file (`/proc`, `/sys`) reports size 0 while having content, and a file that shrinks
   between the `stat` and the read is named by what was read — the same bytes the upload
   carries; a file that GROWS past the cap is read one byte over it and reported past-cap
   (`未发送`), never delivered as a truncated prefix.
7. **A locally-read file reaches the delivery path as bytes, not as a re-encoded URI.** The
   typed **File Content** gains a byte-backed construction beside its decode-from-URI one, so
   a viewer's bytes reach the Platform's upload paths with **no base64 round trip** — a 30 MB
   file would otherwise be re-encoded into a ~40 MB string and decoded again. `size` keeps its
   meaning on both constructions: the byte count actually handed to the Platform, so the two
   sources cannot drift, and the transcript path's decode-from-URI is unchanged.
8. **The tool panel gains the way in, and only where it pays.** A `write` or `edit` **Tool
   Panel** carries a 「📂 打开」 button built from that call's **structured** `filePath` — the
   files the agent *produced*, which are the ones a reader wants in full. A `read` panel gets
   none: its content is already on the card. Natural-language paths in the agent's reply are
   **not** parsed into controls — a card's `<link>`/`<a>` can only open a URL and cannot
   invoke a callback, so a tappable path would have to be a button (a false-positive
   minefield, and an element-budget cost per mention), and structure beats prose.
9. **Re-reading is a button.** 「🔄 重新读取」 re-reads the file and updates the viewer in
   place — the card, and the Rich Text Message that carries its overflow, together. The
   message half is **best-effort against the platform's edit budget** (20 edits per message):
   while an edit is still available the message is edited in place, and once the budget is
   spent the refreshed copy is **re-sent as a fresh message** — the card's tracking line says
   which — so a long-lived viewer never quietly stops refreshing half of itself. No watcher
   and no auto-refresh: a build artifact that changes should not rewrite a card the reader is
   looking at, and a filesystem watcher would contend with the card machinery.
10. **What this is not.** No HTML or rich-text rendering (the platform has no such component);
   no 云文档 import (it is the one surface with a bigger byte budget, but it is a new
   subsystem — 2–3 scopes, async import tickets, copies accumulating in the user's My Space —
   and only `.md`/`.txt`/`.html` are importable, never source code; deferred to its own
   issue); no H5 or 小程序 (they require exactly the public ingress cola does not have, and a
   localhost address serves nobody else in the chat); no write operations (rename, delete,
   edit) — a read surface stays read-only; no recursive search (it brings its own policy
   surface: scan cap, result cap, `.gitignore`); and no inline preview of non-image binaries,
   because the client's 在线预览 format list could not be obtained from the docs and cola must
   not promise a preview it has not measured.

These are the **decisions**; the behavioural contract is the spec #669's — every user story
and acceptance criterion, down to the copy for a missing, unreadable or empty path and the
commands `/help` documents. Where this record's prose is silent, the spec governs, and a
divergence between the two is a defect in this record rather than a design change.

## Considered options

- **Reading through the server's routes** (V2 `GET /api/fs/list` + `GET /api/fs/read/*`, V1
  `/file` + `/file/content`): rejected. It forks the implementation by Generation, V2's
  answer is *poorer* than V1's (bytes + mime, no `text|binary` verdict, and `fs.list` returns
  only `{path, type}` — no size, no `.git` marker, so the browse card's 📦 would cost a
  request per child), and it makes the feature depend on a server that a non-OpenCode
  backend may not offer. Local read is one world on every Generation.
- **A browse-root allow-list** (`[bridge] browse_roots`): rejected/withdrawn in #670 and
  again here. Six review rounds found six escapes, all of the same class — a path-based check
  judged against a live, mutable filesystem — and the residue is unfixable without
  fd-relative traversal. #673 holds the real work; this ADR must not claim a boundary it does
  not have, which is why §4 is worded as a hint.
- **An H5 file browser served from cola** (a real tree + syntax highlighting + search in the
  client's webview): rejected. It needs a public network address in production, and a
  `http://localhost` link is a same-machine desktop-only trick that serves neither mobile nor
  the other members of a group chat. It is also a different product — a client, where cola is
  a **Bridge** (ADR-0081 rejected mirroring session commands in the CLI for the same reason).
- **云文档 import** (`.md`/`.txt`/`.html` → docx): deferred, not rejected. It is the only
  surface whose byte budget exceeds the card's, and it is genuinely right for a 5000-line log
  — but it is a subsystem, it cannot import source code without renaming, and it writes a
  stray copy into the user's drive. It belongs in a follow-up as an explicit 「在云文档中打开」
  escape hatch, not as this feature's default.
- **Delivering the overflow as a plain 150 KB `text` message**: rejected as the middle lane.
  It is bigger but formatting-blind: no code block, no tables, no headings.
- **Paginating the card around a structural limit**: rejected. Five tables and 200 elements
  are not size problems — a 60-row table cannot be fixed by showing it a page at a time.
- **Parsing the agent's reply prose for paths and rendering buttons**: rejected (§8).
- **A `/ls` command alongside the browse card**: rejected (§2).
- **Asking the model to read the file**: rejected — that is the token-and-latency cost this
  feature exists to remove.
- **Auto-delivering a file the agent writes**: rejected. "Deliver what the agent touched" is
  not the goal; the goal is that any path is openable on request. A button on the producing
  panel is the honest version of the same convenience.

## Consequences

- **cola reads file content for the first time.** Until now its filesystem access was
  metadata (the browse card's `read_dir`/`stat`, git state for the Turn Footer). This is a
  read-only content read, and it is the first surface whose payload is user-chosen bytes
  rather than transcript-derived text.
- **A group chat can leak file content, not just directory names.** *#669's advisory scope
  narrows nothing here; the mitigation is `docs/user-guide.md`'s warning plus the Access List.
  The real mitigation is #673.
- **The card's element budget becomes a rendering input.** Whether content fits decides the
  lane (§5), so the browse card's file rows and the viewer's preview compete with the same
  30 KB/200-element budget the rest of the card layer lives under.
- **`post` becomes cola's first rich-text message.** `Platform` gains one send path beside
  ADR-0076's File Message; the researched claim that `md` escapes the card's structural caps
  is **inference from cc-connect's use**, not a measurement — if it is wrong, the lane
  degrades to the File Message, which is why §5 keeps that lane unconditional.
- **New GLOSSARY terms**: **File Viewer**, **Path Scope**, **Rich Text Message**; the
  **Directory Browser** and **Command** entries are amended.
- **The user guide grows `/cat` and the browse card's file rows**, in the `docs/user-guide.md`
  + `docs/user-guide.zh-CN.md` pair (the `Docs drift` job covers the pair, not the prose).
- **Documented gaps this ADR does not close**: the client's 在线预览 format list (unverified
  upstream, so no copy promises a preview), the `post`/`md` structural caps (unverified), card
  nesting depth (#279), and the path boundary (#673).

## Domain note

`GLOSSARY.md` gains **File Viewer**（文件查看）, **Path Scope**（路径作用域） and **Rich
Text Message**（富文本消息）; **Directory Browser** loses "directories only" and gains the file
rows and the way into the File Viewer; **Command** gains `/cat`. **ADR-0078 is marked
amended**: its browser listed directories only, and ADR-0083 extends that same card — the
later record governs, and the earlier one says so where it states the old scope. "Viewer"
alone is not a domain term — the platform has viewers (在线预览) that cola does not control.

## Amendment (2026-10-17, #766): the open button rides inside the fold, and the folded header names the file

§8 put a `write`/`edit` panel's 「📂 打开」 button **beside** the panel, deliberately outside the
fold "so the file opens without unfolding". Read on a phone, that gives every written or edited
file a full-width button row of its own — in a turn heavy on edits the button, not the work, is
what the eye lands on. The placement is reversed, and the folded header takes over the naming:

- **The button is the panel body's first element.** A collapsed panel shows no button; opening
  the file is expand → 打开 (two taps). The two taps are accepted: opening the produced file is a
  secondary action — the reader usually wants the diff or the success line — and the folded
  header already names the file. Feishu's JSON 2.0 button docs list the collapsible panel among
  the containers a button may nest in, so no new surface is involved; putting it first keeps a
  3000-character diff from pushing it out of view.
- **The folded header names the file the call is about.** A `write`/`edit` header gains
  ` · 📄 <name>`; a `read` whose output carries **no** File Content gains ` · 📖 <name>` (its
  path otherwise lives only in the folded Input line, and its book icon still reads correctly for
  a directory read). The name is the same short form ADR-0076's marker and image caption use: one
  printable line, basename first, tail-clipped to 24 characters — in a phone-width header a full
  absolute path would dominate the line, so the directories belong to the unfolded body and the
  viewer.
- **A `read` with a File Content gets no path fragment.** That file is already named by the `📎`
  marker (`SentAsFile`/`Undelivered`) or by the embedded image's caption; naming it twice on one
  line is noise. ADR-0076's amendment stands unchanged for those files.
- **The size estimate charges the header bytes.** `panel_file_detail_estimate` measures the
  fragment's JSON-escaped length, exactly like the marker's, and `panel_estimate` adds it, so
  card splitting cannot under-count a header the card renders.

The way into the viewer itself is unchanged: the payload is the call's structured
`filePath`/`path`, `action: "file"`, no `op`. The **viewer's own** action row is unaffected — a
viewer is not folded, so its buttons stay in the body.

## Amendment (2026-10-17, #770): a file `read` panel gets the way in, and its clipped body says so

§8 gave the button to `write`/`edit` alone, on the grounds that "A `read` panel gets none: its content
is already on the card". That reason does not hold where a reader would feel it most: a `read` body is
clamped to `TOOL_OUTPUT_MAX_CHARS` (3000) characters, so the panel shows a **preview** of the file with
a `…` where the rest was — and §8 left the reader no way at all to see the rest. The `read` panel now
carries the same control, in the same place:

- **Only a file `read` gets the button.** It appears when the call's structured input
  names a path *and* the read was of a file: a **directory** read (V1's `<type>directory</type>`,
  V2's `Read directory …` header line) names no file to open, and a read carrying a **File Content**
  (an image or a PDF) already reaches the reader through its own delivery (ADR-0076's embed or File
  Message lane), so a second way in would only duplicate it. A still-running read has no output yet and
  therefore no directory verdict: it carries no button until one arrives — the same "charge what
  renders" rule the estimates follow.
- **The placement and the payload are #766's, unchanged.** The button is the panel body's first
  element, `action: "file"` with no `op`, routed by the Session — so a click opens the same File
  Viewer, carrying 「📂 所在目录」 for its parent directory (a tool panel has no listing to return to).
  The path is the call's own structured one, V1's `filePath` or V2's `path`, read through the same
  `input_path` rule the panel's Input line uses.
- **A clipped `read` body says it was clipped.** When the body that renders exceeds
  `TOOL_OUTPUT_MAX_CHARS`, one more line closes the Output section —
  `_（预览已截断，打开查看完整文件）_` — naming what the `…` means and where the rest is. It renders
  exactly when the button does, so it never promises a control a directory read, an image read or a
  panel with no Session to route the click by does not have; a body that fits carries nothing. The byte
  charge follows the render: `panel_estimate` adds `read_truncation_note_estimate`, beside the button's
  own `tool_open_button_estimate`.
- **The folded header is unchanged.** A text `read` keeps its ` · 📖 <name>` fragment (#766), a
  directory read included; the button adds no second name.

Everything else in §8 stands: prose paths are still never parsed into controls, and a `read` stays a
cola-local, generation-neutral read.

## Amendment (2026-10-17, #771): the viewer card stops repeating the path-scope statement

§4 remains the home of the privacy statement — the scope is advisory, a hint rather than a
control, and the **Access List** is the only gate on who may act. What changes is where that
statement is *told*: the viewer card had been repeating it as a `⚠️` row appended to the
naming block of every path outside the Session's directory:

> ⚠️ 此路径位于会话目录之外；仍可打开。此提示不是访问边界。

The row is removed, together with `is_outside_session_dir` — the lexical `.`/`..`-normalizing
comparison whose only caller built it.

- **The naming block is 文件 / 大小 / 类型, and nothing more.** The viewer still names the
  **resolved** path, so an outside-session file is as unambiguous as any other; the card simply
  no longer editorialises about where the file lives. A statement read once belongs to the ADR
  and the user guide's **Path Scope** entry, not to a line repeated on every card — repeated
  copy is read as boilerplate and stops being read at all.
- **「此提示不是访问边界」 was the worst part of it.** On a card that is showing file content,
  that sentence reads as "a boundary exists here, and this loading is a bypass of it" — a claim
  §4 and the rejected browse-root allow-list took pains never to make (#673 holds the real
  work; #670's six escapes are why the ADR words §4 as a hint).
- **No behaviour changes.** A path outside the Session's directory still opens, `/cat` still
  resolves and displays the **resolved** path, and the scope is still advisory. The guide's
  Path Scope entry already carries the plain statement the card was trying to make — a group
  chat can read whatever a Host opens.
- **`ViewerCard.session_dir` stays.** Only the naming block stopped reading it; the pager
  payload still carries `session_dir` (`extra.push(("session_dir", …))`) and a viewer action
  still consumes it as the directory a relative path resolves against. No payload, handler or
  pager change rides along.

The unit test that asserted the row is replaced by its inverse: an outside-session path renders
its resolved path with no `⚠️` row, and the two bridge tests that asserted the row now assert
its absence.


## Amendment (2026-10-18, #774): an HTML page's viewer leads with the File Message

Decision 10 rejected HTML rendering on the ground that "the platform has no such component".
That ground was true of the **card** and false of the **platform**: the rendered page exists, in
the client's own file preview. Feishu's help centre **documents** that preview
([在飞书中查看 HTML 文件](https://www.feishu.cn/hc/zh-CN/articles/052565607602): "点击文件，即可预览
文件内容 … 点击右上角代码，可切换至代码视图"), and the owner verified it on the release build in a
real chat, desktop and mobile. The probe seeded for #774 supplied the one detail the help centre
does not state — an inline `<script>` executes — and its date is host-local, **2026-10-18 (UTC+8;
2026-10-17 UTC)**, which is why CI's UTC runner and this record can disagree about "today". The full
evidence lives in `docs/research/feishu-rich-surfaces/r1-display-surface-inventory.md` §2:

- a `.html` File Message renders as a **page** when opened — desktop and mobile — and its inline
  `<script>` executes; the client toggles to a code view. Feishu's help centre caps that preview
  at **12 MB**.
- a `.md` File Message renders as rich text, with a rendered preview excerpt **before** the file
  is even opened; a card `markdown` element and a `post` `md` tag render the same markdown
  equivalently.

So an HTML page has a viewer cola cannot build and does not need to: the File Message. What
changes is only the **card's action priority and copy** — no new lane, no new payload:

- **Classification is untouched.** CONTENT still decides the kind (§6): an `.html` file is UTF-8
  text and previews as a fenced code block with the `html` language token. The extension decides
  only which action the card leads with — an `html`/`htm` file is a *page*.
- **The page's transfer leads the row and is named for what it buys.** For an `.html`/`.htm` text
  file the card's first button is 「🌐 预览网页」, the same action and payload as 「发为文件」
  (`file`/`send`, the resolved path, the browse listing's whole state); 「🔄 重新读取」 and the
  directory control follow. Every other file keeps 「发为文件」 exactly as it was, so one action
  keeps one name per intent and the callback grammar is unchanged.
- **The card does not explain itself; the label does.** The transfer button is named for the
  page it produces — 「🌐 预览网页」 — and that is the whole story: an owner who presses it once
  sees the File Message and the page its client renders. A standing line of prose above every
  HTML viewer's source block would occupy the card's most valuable row to teach a one-time fact,
  so the note this amendment first carried was **dropped during the owner's manual verification**
  (#774). What remains is the button, obeying the predicate it always had: a card that already
  sent the file offers no second send, an ambiguous send is not retried, and a file past the
  upload cap keeps its button — the click re-reads the file and reports 未发送 on the card,
  exactly as every other kind does. The client's 12 MB preview limit stays documented evidence
  (`docs/research/feishu-rich-surfaces/r1-display-surface-inventory.md`), not card copy.

- **The source preview stays.** A page's markup is still worth reading, and the card is the only
  place cola shows it; the amendment demotes the action, it does not delete the preview.

Not in this amendment: **rendering markdown inside the card**. The measurement makes it viable —
the card `markdown` element already renders that family — but it is a separate change (a
preview/source toggle is a new card state), and decision 10's blanket "no rich-text rendering"
sentence is narrowed here only to the extent measured above.

## Amendment (2026-10-18, #778): a Tool Panel's HTML page button sends, and does not touch the card

#774 gave the *viewer* the page lane, but a Tool Panel still led with 「📂 打开」: the reader went
panel → viewer → 「🌐 预览网页」, and the middle hop showed the same markup the panel's body had
already shown. For an HTML page that hop buys nothing, so the panel leads with the page directly —
two hops collapse to one. The change is a second semantics for the panel's button, and nothing
else:

- **The button is page-aware, on one shared predicate.** A Tool Panel whose structured path spells
  `.html`/`.htm` (the same extension rule the viewer's page lane uses — now factored into one
  `is_html_page_path`, so the two cannot disagree) reads 「🌐 预览网页」; every other path keeps
  「📂 打开」. The extension still never decides the KIND (§6): it decides which action the button
  names.
- **`op: "preview"` is the new, PATCH-free transfer.** The viewer's 「发为文件」/「🌐 预览网页」
  carries `op: "send"`, which PATCHes the clicked message with the outcome — correct on the viewer,
  whose clicked message IS the card, and wrong on a Tool Panel, whose clicked message is the
  agent's **Turn card**: that PATCH would overwrite the panel and the reply. `op: "preview"` runs
  the same transfer (one File Message) and rewrites no card.
- **The panel payload is routed as the open payload already was.** It carries the Session, never
  the Chat, so the Chat/Topic and the session directory are read from the SessionStore exactly as
  `handle_file_card_action` does; the structured path resolves on the Path Scope (§4), so a panel
  whose `filePath` is relative to the session directory sends the right file. A Session the store
  no longer knows answers an honest Toast, no transfer.
- **There is no card of cola's own to write `未发送` on.** A landed send is its own evidence
  (the File Message) and adds no card copy; a definite failure is spoken as a reply under the
  clicked card — `⚠️ 无法发送 …：原因` — and so is an ambiguous timeout
  (`⚠️ 发送结果未知，为避免重复未重试。`), because a silent outcome would read as a silent failure.
  A definite failure is remembered on nothing, so the button stays live for a retry.
- **Deliver-once still holds, keyed by the path.** The same bounded window remembers a landed or
  ambiguous send so a stale second click cannot post a duplicate; the key gains the path, because
  one Turn card can carry several file panels and the clicked message alone does not name one file.
  A definite failure is remembered on nothing, so the button stays live for a retry.

The callback grammar gains one `op` value; the open lane, the viewer's own lane, and every other
file kind are unchanged.

## Amendment (2026-10-18, #781): a markdown file's viewer offers its rendered text

The #774 amendment ended by deferring exactly this: *"rendering markdown inside the card … is
viable — the card `markdown` element already renders that family — but it is a separate change (a
preview/source toggle is a new card state), and decision 10's blanket 'no rich-text rendering'
sentence is narrowed here only to the extent measured above."* The measurement it rested on
settled that a `.md` file's client preview, a card `markdown` element and a `post` `md` tag render
**equivalently** — so for a markdown document the card IS a renderer, and no File Message hop is
needed to read one. That capability is now one tap away, with the source still the default.

- **The source stays the default; the rendered text is opt-in.** A `.md`/`.markdown`/`.mark`
  text file renders exactly as it always has — the fenced source block, paged by lines past the
  card budget — and its first button is 「📖 渲染预览」: the file's own text cleaned by the card's
  markdown sanitizer, as one `markdown` element and no fence. The first cut of this change led
  with the render; the owner **reversed it on a real device** (#781, host-local): a long document
  rendered whole reads as a flooded phone screen, where the fenced source reads as a file
  preview. Unlike HTML, whose only renderer is the client's File Message preview (#774), a
  markdown document renders *in the card*: no extra message, no upload.
- **The toggle is the pager's rebuild, not new machinery.** `op: "view"` (payload word
  `viewer_view`, `"rendered"`/`"source"`) rides the `op: "page"` path end to end: a bounded
  re-read off the callback, an in-place PATCH of the clicked card, and **no** per-card state
  (ADR-0052). Every rebuild control shares one carried-state helper (`viewer_carry_fields`) —
  path, path scope, the browse listing, the snapshot identity, the whole-bytes lane and the
  chosen view — so a page flip, a re-read or a transfer's PATCH cannot drop the view, the way
  back, or a completed transfer.
- **A render the card cannot hold is not offered.** The gate is the SANITIZED body fitting the
  preview budget — `serialized_len(clean(text))` plus the element wrapper against
  [`PREVIEW_JSON_BUDGET`], because `clean` can wrap an over-limit table in a fence sized to that
  table's longest backtick run, and measuring the raw text would under-count exactly the file that
  must not be rendered — and a card over that budget has no toggle at all, its source view
  unchanged. The complete, uncapped render stays the 「发为文件」 File Message's own client
  preview, which is also why this amendment needs no new overflow lane: a markdown table past the
  card's table budget degrades to a fenced block *inside* the render, through the sanitizer's
  existing rule.
- **The extension still never decides the KIND (§6).** `is_markdown_path` (the three extensions
  the Drive import accepts, and the `markdown` code token's partners) selects the RENDER only: a
  `.md`-named binary keeps the ordinary row, exactly as a `.html`-named one does (#774), and
  `.rs`, `.txt` and every other text kind are untouched.
- **The transfer is untouched.** A markdown file keeps 「发为文件」, and the transfer's card PATCH
  keeps the chosen view the same way it keeps the page (`transfer_value` carries both).

Not in this amendment: rendering any non-markdown text kind as markdown (the extension rule is the
markdown trio alone), and any per-table authoring of source vs render — the sanitizer's existing
budget behaviour is the whole of it.

## Amendment (2026-10-18, #788): a `write` Tool Panel shows the file it produced

§8 gave a `write` panel the way into the file it produced but left the panel itself showing only the
path and the tool's bare success sentence — "Wrote file successfully." — so the file the agent had
just written was the one thing the card did not show, even though §8's own reason for the button
("the files the agent *produced*, which are the ones a reader wants in full") is that the produced
file is what a reader wants. A `write` panel now renders that content as its body, the way a `read`
panel already renders the file it read:

- **The body is the call's own `content`.** The `content` input — V1's and V2's alike — is the
  Output section's body, in place of the success sentence, which is dropped as noise the way `edit`
  drops its summary. A trailing note the tool reported after that summary (V1's "LSP errors
  detected …") is kept as its own paragraph AFTER the fenced content, outside the preview budget so
  a long note is never clipped by the content's clamp, and is itself clamped to
  `TOOL_OUTPUT_MAX_CHARS` so one panel stays bounded. The Input line and the folded header are
  unchanged, and the body's `📄 <path>` header is the `read` panel's shape.
- **The fence takes the touched file's language.** `language_for_path` (the one measured table the
  File Viewer and a `read` panel share, #786) reads the call's own `filePath`/`path` through the
  same `input_path` rule every file formatter uses, so a written `.md` previews its **source**,
  exactly as the viewer's default does.
- **The body is a preview like a `read`'s.** The `content` alone is clamped to
  `TOOL_OUTPUT_MAX_CHARS` (3000) characters, and a clipped one closes with the same
  `_（预览已截断，打开查看完整文件）_` note, rendering exactly when the 「📂 打开」 button does
  (ADR-0083 §8 amendment, #770). The byte charge follows: `panel_estimate` sums
  `panel_output_estimate` — the rendered content body, JSON-escaped like the card serializes it —
  and `write_tail_estimate` in place of the output text it would otherwise count.
- **A body only where one exists.** A FAILED `write` keeps its error text, and a call whose input
  carries no usable `content` keeps today's output panel; nothing is invented. A `write` carries no
  File Content block (ADR-0076), so this adds none.
