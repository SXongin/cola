# Terminal Commands: the Host runs a shell command from Feishu, in one process, outside the model loop

The model could already run shell commands through the Backend's `bash`/`shell`
**Built-in Tool**, gated by a **Permission** card. Nothing let the *Host* run one. Browsing
(ADR-0078) and reading (ADR-0083) had already made the Host machine visible from Feishu;
execution is the third and heaviest step of that ladder. `/run <cmd>`, or a `!<cmd>`
message, now runs exactly one command locally and renders it on its own **Terminal Card** —
with no model in the loop, no approval card, and no entry into the Session Transcript.

## Context

- **The model's side was never the gap.** A `bash` (V1) / `shell` (V2) call already reaches
  Feishu: its **Permission** renders 「⚡ 执行 Shell 命令」 (`describe_permission`,
  `src/bridge/request/kind.rs`), `/autoaccept` can pre-approve it, and its output lands in a
  **Tool Panel** under the card budgets. "Let the AI run commands" is therefore not a feature
  request — it is today's behavior. What did not exist was a way for the person to run one.
- **cola is already a Host-side actor.** It reads host files for the **File Viewer**
  (`src/bridge/viewer.rs`), runs `git` subprocesses locally for `/diff` (`src/git.rs`), and
  discovers, spawns and reaps the OpenCode server (`src/bridge/discovery.rs`). "cola starts a
  process on this machine" is not a new capability class; ADR-0083 §1 §6 established the
  same for local reads.
- **Cards are the only surface with buttons and a live tail.** A `post`/`text` message is
  editable at most 20 times and carries no controls; a card is patched at the production
  poll cadence and can carry the buttons a running command needs. The platform budget is
  unchanged: 30 KB, 200 elements (ADR-0083), and cola's own text budget is
  `MAX_ELEMENT_TEXT_CHARS` (3,000 chars per element).
- **A card is readable by the whole chat.** The **Access List** gates who may act, never who
  may read (ADR-0035, ADR-0083). Terminal output therefore joins the same exposure directory
  names and file contents already have.
- **Feishu delivers at least once, but the dedupe is best-effort.** `im.message.receive_v1`
  is deduped on `header.event_id` in the WebSocket layer, before dispatch
  (`src/feishu/ws.rs`) — a bounded in-memory set, and the key and the 5-minute age bound are
  both optional. **Card actions never enter that set at all** (`card.action.trigger` has no
  dedupe path), so a redelivered 「↻ 重跑」 click is unguarded. Any destructive path needs its
  own idempotence; this design gets it from a real per-chat claim, not from the WS layer.
- **A message is handled on its own task.** Every inbound frame is dispatched onto a spawned
  task (`src/feishu/ws.rs`), so "one command per chat" is not a property anything already
  provides: it must be an atomic claim, like `/diff`'s cooldown
  (`src/bridge/repo_diff.rs`).
- **Two reference implementations agree on the shape, and both inject output into context.**
  aider ships `/run` with **alias `!`** — "run a shell command and optionally add the output
  to the chat" — runs it in the project root, asks before adding the output, warns that it
  "executes unchecked in the real project directory", and later added a PTY lane for
  interactive commands. Claude Code ships `!` shell mode: runs the command directly, adds it
  and its output to the session, shows progress, supports Ctrl+B backgrounding, and
  "doesn't require Claude to interpret or approve the command". Claude Code also states the
  boundary this ADR reaches independently: commands typed in shell mode "run outside the
  sandbox even when you've enabled sandboxing, because the sandbox applies to the commands
  Claude runs". Its fixed-`/bin/zsh` bug is the price of pinning the shell.
- **ChatOps took the opposite path.** Hubot/Lita commands are pre-scripted actions (deploy,
  rollback), never arbitrary shell — a group chat is multi-party and a command there is
  unattributable. The Host-only model removes that objection: one admitted **Principal**, on
  their own machine (ADR-0035). Arbitrary commands are defensible *because* of that model,
  not in spite of it.

## The no-terminal constraint

There is no pseudo-terminal, and that is a shape constraint, not an implementation detail:
`/run` is **line-oriented, not screen-oriented**. Measured on a Host with stdin `/dev/null`,
stdout to a file, and `setsid` — the closest stand-in for "no terminal", taken before the
implementation's own limit below was known:

| Command | Result |
|---|---|
| `top` | exit 1, **0 bytes** of stdout, stderr `top: failed tty get` |
| `more file` | degrades to `cat` (adds a `::::::` banner) |
| `git log` | does **not** page — git disables its pager for a non-tty stdout |
| `git commit` | exit 1: `unable to start editor 'vi'` / `supply the message using -m or -F` |
| `ssh -t` | exit 255: `Pseudo-terminal will not be allocated because stdin is not a terminal` |
| reading `/dev/tty` | `No such device or address` |

**The last row is conditional, because the implementation cannot call `setsid`.** The crate is
`#![cfg_attr(not(test), forbid(unsafe_code))]`, and `pre_exec` is the only `std` route to it —
the wall ADR-0085 already hit, answered there with `CommandExt::process_group(0)`
(`src/bridge/discovery.rs`). A process group is not a session: the child keeps cola's session
and, when cola has a controlling terminal, its tty. So:

- **cola under a Supervisor** (no controlling terminal): `/dev/tty` is `ENXIO`, a password
  prompt fails fast, and the table stands.
- **cola launched by hand from a terminal**: the child is a background process group in
  cola's session, so a read from `/dev/tty` is stopped by `SIGTTIN` — `sudo`/`ssh-add` hang
  until the cap (or 终止) instead of failing.

That is accepted and documented rather than bought with a lint exception: a terminal is not
where passwords should be typed, the hang is bounded by the same cap as any other run, and
the property that matters — no terminal hangup reaches the child, and its output does not die
with cola — holds in both modes precisely because stdin is null and stdout/stderr are files
(the same argument ADR-0085 records for the Shared Service).

Two practical consequences the implementation must carry:

- **A progress bar writes one unbounded line.** 50 frames of `\r`-overwritten progress measured
  **691 bytes and zero newlines**, so a newline-only tail would render a single enormous line.
  The tail splits on `\n` and treats a `\r` inside a line as an overwrite, keeping the last
  frame.
- **Some programs ignore `isatty`** and write `ESC[2J` / cursor-positioning into the file
  anyway. ANSI and control sequences are stripped for display and kept in the **Terminal Log**
  as evidence; a screen that has been flattened into a stream is not content, which is the
  honest reason a TUI has no home here.

A command that genuinely needs a screen has a correct answer that is not cola's: let another
program own the PTY — `tmux new -d -s x 'htop'`, then `tmux capture-pane -pt x` turns the
screen back into lines. `script -q -c …` also fakes a TTY, and its output is exactly the
escape soup this section refuses.

## Decision

1. **Two entry points, one act.** `/run <cmd>`; or a message whose **first character** is `!`
   and whose remainder is non-empty — the text the bridge sees is already mention-stripped and
   trimmed, so `@cola !ls` and `  !ls` qualify, and in a group the bot must still be mentioned
   for the message to arrive at all. `!` is a message prefix, not a **Command**. A
   `!`-prefixed message currently reaches the model as prose (and is still scanned for
   `#<id>` **Skill** tokens), so this is a deliberate parse change, bounded to that one shape:
   `!`, `! ` and any `!` mid-message stay prose, and `!ls #caveman` is now a shell command
   containing the literal `#caveman` rather than a skill load. `/run` alone answers usage;
   `/help run` documents both entry points, the cap and `--timeout`, the absent terminal and the
   Windows refusal; `/run` must be added to the **Command** parser's help tables or
   it silently becomes a `Forward` prompt.
2. **One command, one child process, no carried state.** `sh -c`, cwd on the **Path Scope**'s
   base (the **Session**'s directory, the current **Project** while the Session is still
   **Pending**, and the default session directory when the conversation has neither), stdin
   closed (`Stdio::null`), stdout and stderr merged. The child leaves cola's process group via
   `CommandExt::process_group(0)` — the safe route ADR-0085 established — which is what
   decision 6's kill targets and what keeps a terminal hangup from reaching the child. There
   is no persistent shell and no `cd`/`export` carry between commands. A Terminal Command never
   touches the Backend: it does not ensure a server, and it works with `start_server = "never"`
   and while the server is down. The card names the cwd it actually used.
3. **The output goes to a file, not through cola.** The child's stdout and stderr are
   redirected straight into its **Terminal Log**, and cola tails that file's delta on its own
   render cadence. This is not only bookkeeping: on a pipe, cola's own death closes the read
   end and the child dies of `SIGPIPE` on its next write — which would make decision 10's
   orphan impossible. Direct-to-file is what lets a command outlive cola **where the launch mode
   permits it** — a supervised cola's cgroup takes the child on restart regardless (decision
   10) — and it means the bytes 「📄 完整输出」
   needs are already there, and the live tail is "read what was appended since the last
   offset" rather than a ring buffer fed by a reader.
4. **The result is a Terminal Card, and it is a command card.** A standalone member of the
   `/dir`-`/switch`-`/help`-`File Viewer` family (ADR-0083 §3): it joins no **Card Chain**, is
   never written to the **Chain Record**, and is never *deleted* by the reaper — the startup
   sweep may patch it (decision 5), which is a different act. It is posted as a reply to the
   command message (ADR-0076's no-`reply_in_thread` rule) and captures the requester, reply
   target and its own message id at send. It owns a per-command patch loop at the render
   cadence, cancelled when the run settles; the Turn poller is not involved because a
   Terminal Command is not a Turn. It shows the resolved cwd, the command, a live tail and,
   once settled, the exit code and elapsed time. The tail keeps the newest lines **within a
   2,500-character budget** — a per-line cap as well, since one minified line can be megabytes
   — splits on `\n` with `\r` treated as an overwrite, strips ANSI, and reads non-UTF-8
   lossily; when the budget cuts content it says so. A failure whose stderr matches a known
   no-tty signature — `failed tty get`, `not a terminal`, `unable to start editor`, `the input
   device is not a TTY`, `No such device or address` — gains one line pointing at `tmux` or at
   the command's non-interactive flag (`top -b`, `git commit -m`); that is a fixed mapping,
   never a heuristic over the command text. The card carries 「⏹ 终止」 for as long as the run
   is **live**, and 「继续」 joins it once the cap is reached; 「↻ 重跑」, 「📄 完整输出」 and
   「问 AI」 enable once the run **settles**, and stay usable on a run the sweep marked lost —
   that is what keeps a restart from being collateral damage to the buttons. 终止, 继续 and the
   rest are dead once the run settles; they are refused by state, not hidden, so a stale click
   is a no-op rather than an error. **Every button carries the run id it was rendered for** —
   「↻ 重跑」 also the
   command, cwd and timeout, 「📄 完整输出」 the log path and 「问 AI」 the output window — so a
   card is self-describing after a restart. A button whose run id is not the chat's current
   record id is refused as stale, and that is what makes a redelivered rerun click single-shot:
   the claim in decision 6 prevents overlap, but card actions carry no dedupe of their own, so
   a duplicate arriving after a fast command has settled would otherwise claim a released slot.
5. **The card's identity is durable; the run itself is not.** A **Terminal Run Record** — one
   small file per **Chat**/**Topic** under cola's state directory, carrying the run id, the
   card's message id and chat, the **Terminal Log** path, the command, the cwd, the timeout, the
   process ids and the run's state (`running` / `settled` / `lost`) — is written **before the
   child is spawned**, completed with the card's message id and the process ids once the card is
   sent and the child is up, marked settled when the run settles, and kept until the next run
   in that chat replaces it. The order is the guarantee: **card → record → spawn → record
   completed**. The card is sent first, in 「启动中」; the record then carries its message id
   and the run id; only then does the child spawn. So no child ever runs without a record
   naming its card, and a run that outlives cola is always findable and markable — and where
   the launch mode takes the child instead, the record still reports the run rather than
   leaving it frozen. A record written but never spawned has a card id and no pid; the sweep
   patches that card to 「已失去跟踪」 and marks the record lost, **keeping** it — the card's
   rerun must still find its run id current, and deleting the record would turn the buttons
   into stale clicks. One window cannot be closed: a
   crash between the card send and the record write leaves a card in 「启动中」 with no record
   — and because the spawn comes *after* the record, **nothing is running**; that card is the
   only lie this design cannot fix, and it is settled by hand. Making the send and the record
   one act is not available. This is
   the one durable thing, and it exists because neither of the existing options can serve: the
   **Chain Record** is excluded by decision 4, and the card-handle cache drops any card with no
   interaction block, which is exactly this card. Two further properties fall out of the
   retention: a click whose run id is not the current record's is refused as stale even after
   the run settled, and 「↻ 重跑」/「📄 完整输出」 still know their data after a restart. On
   startup the sweep reads each record whose state is `running`, PATCHes its card to
   「已失去跟踪」 when it has a message id, and marks it `lost`; a persisted process id is
   **never signalled**, because a pid may have been reused since — a lost run is reported, not
   adopted. This is not the deferred background-task ledger: it has one record per chat, no
   liveness model and no takeover.
6. **Serial, bounded, and killable.** One Terminal Command per **Chat**/**Topic** at a time,
   enforced by an atomic claim (the `/diff` cooldown's precedent) — a second command is
   refused with the running card's 「⏹ 终止」 named. The claim prevents *overlap*, not
   *duplication*: idempotence is decision 4's run id, and the two together are what make a
   double-clicked 「↻ 重跑」 single-shot even when the first rerun finished instantly. The cap
   is `--timeout <n>[smh]`, default ten minutes, hard maximum
   twenty-four hours; flags are parsed only before the command, and `--` ends flag parsing, so
   a command may begin with `-`. The `!` form accepts no flags at all — everything after `!`
   is the command. Reaching the cap stops **tailing**, never the `wait()`: a command that exits
   at 10:01 still settles with its exit code, and the card offers 「继续」 (extend by the same
   amount) and 「⏹ 终止」. The chat's slot stays claimed until the process group empties or is
   killed, so an extended run blocks later commands — stated, not hidden. 终止 signals the
   **process group** (`SIGTERM`, then `SIGKILL` after a grace), because killing only the
   parent leaves the compilers it spawned running. **The run is live while its process group
   has members, not while `sh` is alive**: `sh -c 'cargo build &'` exits at once while cargo
   keeps writing the tree, so a run that leaves background children stays claimed, keeps its
   「⏹ 终止」, and names the survivors on the card until the group empties. Liveness is a
   `kill(-pgid, 0)` probe through a safe wrapper — the crate's `unsafe` ban is about its own
   code — and never a pid read back from a record. The consequence for the documented
   long-running escape hatch is stated: a `nohup … &` inside `/run` keeps the slot until it
   ends, which is what "still using this tree" means. One boundary is stated rather than
   guarded: a command that deliberately starts a new session (`setsid …`) leaves the group, so
   it escapes the claim, the tail and 终止 — the same class as a command that kills cola
   (decision 11). No portable safe mechanism stops a process from calling `setsid`, and
   pretending otherwise would be the fake safety this ADR rejects elsewhere.
7. **Output volume has an observed stop threshold, and delivery never depends on it.** Because
   the child writes to a file, a runaway command can fill the disk — and no portable safe
   primitive bounds one descriptor's size: `RLIMIT_FSIZE` is per process and would cap the
   command's own build artifacts too, while truncating the live file does not help because the
   child's offset does not move back. So the cap is a **stop threshold, not a write limit**: a
   dedicated size probe — a metadata read on a short interval of its own, not the slower render
   tick — kills the group and settles 「输出超限」 when the log passes 29 MB. A fast producer
   can overshoot the threshold before the probe sees it; that is stated, not hidden. What must
   not fail is retrieval: when a log exceeds the File Message lane's 30 MB cap, 「📄 完整输出」
   sends the first 30 MB from a bounded copy with a truncation line, so output is always
   retrievable to the platform's own ceiling. The 29 MB threshold exists so that ordinary runs
   — the overwhelming majority — get their whole output without truncation. That transfer never
   runs inside the card callback: the click acknowledges, a bounded background step performs
   the upload and the send with its own timeout, and the card reports the outcome — ADR-0083
   §5's rule, unchanged, because a 30 MB upload cannot fit a callback's ack budget.
8. **The environment is inherited, minus cola's own credentials — and this is not a
   sandbox.** `OPENCODE_SERVER_PASSWORD`, `OPENCODE_PASSWORD` and `OPENCODE_SERVER_USERNAME`
   are stripped (the same names `discovery.rs` already strips in the other direction, with
   its removals-last rule); everything else is the Host's shell. **PATH is the Supervisor's,
   not the login shell's**: `cola autostart enable` writes `Environment=PATH=<the PATH at
   enable time>` into the unit, so a toolchain installed afterwards is invisible until the
   enable is re-run — documented, and surfaced by `cola status`/`cola doctor` rather than
   papered over with a login shell or a new config knob. The limits are stated, not implied:
   `cat ~/.config/opencode/auth.json` still works, `env` still shows whatever the Host
   exported, and every card is readable by every member of the chat. The Access List keeps the
   *act* private; it cannot keep the *result* private.
9. **The model is not in the loop, and the transcript does not learn.** A Terminal Command
   enters no **Turn**, no **Session Transcript**, and no context. Both reference
   implementations inject output into the conversation; the `!`/`/run` surface here is a
   terminal in a chat, and silently turning every `ls` into a prompt is a context cost and a
   surprise. 「问 AI」 is the explicit door, and it is framed as data: the submission quotes
   the command and its output the way a **Supplement** quotes context, bounded to the tail
   rather than an unbounded log, and it goes through the normal prompt pipeline — so with a
   live Turn it becomes that Turn's **Supplement** (ADR-0062), not a new Turn. The content is
   attacker-controlled whenever the command read untrusted text, and it is presented as
   output to reason about, never as instructions.
10. **A restart loses the run, and the card says so.** The child's fate depends on how cola was
    launched, and cola cannot know which: hand-launched, the child survives as an orphan
    (direct-to-file, decision 3); supervised by systemd, the restart's cgroup termination
    (`KillMode=control-group`) takes it too, `process_group` notwithstanding; launchd has no
    cgroup. The card therefore reads 「已失去跟踪（**可能**仍在运行）」, never 「仍在运行」, and
    the startup sweep settles it through decision 5's record. Its **Terminal Log** survives,
    which is why the sweep is by **age** and never a wholesale boot-time delete: a blanket
    delete would unlink a file a live orphan is still writing.
11. **No guard against a command that operates on cola itself.** `!kill <cola pid>`,
    `!cola restart`, `!rm -rf ~/.cola` and `!pkill -f opencode` all run. The terminal is the
    emergency exit when cola itself is wedged, and pattern-matching a "self-affecting" list is
    the same fake safety as a command whitelist — `kill $(cat ~/.cola/cola.lock)` defeats any
    list. The consequences are accepted and documented: a command that kills cola leaves a
    frozen card (settled as lost on the next start), re-running cola is a lock-protected no-op,
    deleting `access.json` un-claims the bot, and the recovery path is `/restart` (or the
    Supervisor), never the button on a card whose process is gone.
12. **The completion notice is the existing one, captured by the command itself.** A Terminal
    Command notifies only when it ran past the five-minute threshold **and** the chat type's own
    notice opt-in is on: `[bridge] long_task_notice` in p2p (off by default) and
    `[bridge] group_completion_notice` in a group (on by default), exactly the switches a
    **Turn** obeys (ADR-0043, ADR-0060). A quick `/run ls` owes nobody a push even where the
    opt-in is on; a long command in a group notifies by default, and in p2p only when the Host
    opted in. No new switch, and no policy the existing ones contradict. The capture cannot be
    a **Turn**'s: the notice needs a requester, a reply target and a card id, and a Terminal
    Command has no Turn, so it captures the three itself.
13. **Unix first; Windows refuses and says why.** cola ships a Windows build, but the hard half
    is not the shell (`sh -c` vs `cmd /C`), it is terminating the whole tree: a process group
    and a negative pid on Unix versus a Job Object or `taskkill /T` on Windows. V1 answers
    Windows with a stated refusal rather than a half-implemented lane; the loop is its own
    ticket.

## Why

- **It is the third step of one capability ladder**, not a new subsystem: browse (ADR-0078) →
  read (ADR-0083) → execute. The first two were Host-local and backend-neutral; this one is
  too, and putting it anywhere else would fork the one boundary that must stay single.
- **The marginal risk is small and honest.** The model already runs arbitrary commands on this
  machine, and can be told to do so with auto-accept on. The change is that the *person* no
  longer needs the model to mediate — which is the whole point of an IDE terminal, and which
  the Host-only **Access List** already scopes.
- **The alternative references validate the shape.** aider's `/run` + `!` alias is the same
  entry language; Claude Code's shell mode is the same "no approval for the human's own
  command" rule and the same explicit "this runs outside the sandbox" statement. The one place
  this ADR departs — output not entering context — is covered by 「问 AI」.

## Considered options

- **A persistent PTY.** Real shell semantics (`cd`, `export`, prompts, `^C`) at the cost of
  TTY ownership, prompt answering and signal translation — all to carry state that
  `cd x && …` carries in one line. aider only added a PTY lane after `/run` existed, and this
  is the same sequencing.
- **A command whitelist.** Fake safety: `cargo test && rm -rf …` passes any prefix rule, and
  the model's `bash` tool has no whitelist either.
- **A confirmation card per command.** The message *is* the consent; a second confirmation
  turns a terminal into a permission manager and trains people to click through.
- **Injecting the output into the model's context.** Both references do it; it makes every
  command a hidden prompt and taxes the context with output nobody asked the model to read.
  「问 AI」 keeps the benefit and the choice.
- **A separate daemon (sidecar).** It buys isolation only if it runs with different
  privileges — it does not, since the child runs as the Host either way — while forking the
  **Principal** check that ADR-0035 requires to be total and single. If a real sandbox
  (bubblewrap/container) or restart-surviving commands ever arrive, the module boundary keeps
  the split available at no caller cost.
- **`--background` now.** That is the body of a different ADR: a cola-local durable task
  ledger, restart takeover, and a pid-plus-log re-attachment model. The existing **Background
  Task Ledger** is backend-owned and carries only the V2 shell tool and subagents (ADR-0079),
  so borrowing it would mean teaching it a third, cola-owned kind.
- **Wrapping tmux.** It solves persistence and interactivity by delegating to a program the
  Host may not run, and it moves the truth about "what is running" outside cola entirely.
- **Relaxing `forbid(unsafe_code)` for `setsid`.** One corner case (a password prompt under a
  hand-launched cola) is not worth weakening a crate-wide guard the repo already decided
  against once, in ADR-0085.

## Consequences

- **A new card family member, a new module, and one small durable record.** The Terminal Card
  joins the command card family; execution, tail and kill live in one Host-capability module
  shaped like `viewer` (backend-neutral, generation-neutral); the Terminal Run Record is one
  overwritten file per Chat/Topic, not a ledger.
- **`!` is a breaking parse change.** Messages that begin with `!` stop reaching the model.
  Bounded deliberately to the first character, and documented in `/help run`.
- **The blast radius is the Host's own shell.** A Terminal Command can kill cola, kill the
  OpenCode server, or destroy the working tree. That is the feature, and it is why the three
  boundary sentences above are part of the decision rather than an appendix.
- **A frozen card is a normal outcome, not a bug.** Any command that kills its parent leaves
  one; the sweep settles it on the next start, the log keeps the evidence, and `/restart` is
  the way back.
- **The Terminal Log is a new at-rest secret surface.** Temporary by design, 0700/0600,
  stopped at a 29 MB observed threshold, swept by age, and named so it can never be confused
  with the application log.
- **Two launch modes behave differently, visibly.** A supervised cola has no controlling
  terminal (prompts fail fast) and takes its children with it on restart; a hand-launched one
  can have a stopped `sudo` on the desktop and leaves orphans behind. Both are reported, never
  guessed.
