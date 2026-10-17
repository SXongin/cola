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
