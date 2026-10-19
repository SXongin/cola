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
  poll cadence (1.5 s, ADR-0011/ADR-0045) and can carry the buttons a running command needs.
  The platform budget is unchanged: 30 KB, 200 elements (ADR-0083).
- **A card is readable by the whole chat.** The **Access List** gates who may act, never who
  may read (ADR-0035, ADR-0083). Terminal output therefore joins the same exposure directory
  names and file contents already have.
- **Feishu delivers at least once.** `im.message.receive_v1` is deduped on `header.event_id`
  in the WebSocket layer, before dispatch (`src/feishu/ws.rs`). A Terminal Command is
  destructive by nature, so that dedupe is a correctness requirement of this feature, not an
  incidental upstream nicety: a redelivered message must never run a command twice.
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

## Decision

1. **Two entry points, one act.** `/run <cmd>`; or a message whose **first character** is `!`
   and whose remainder is non-empty. `!` is a message prefix, not a **Command** — a message
   starting with `!` used to be prose that reached the model, and this is a deliberate parse
   change, bounded to that one shape so `!`, `! ` and any `!` mid-message stay prose. `/run`
   alone answers usage; `/help run` documents it. `--timeout <dur>` is the only flag; there is
   deliberately no `--dir`, because the **Path Scope** is the only cwd and `cd sub && …`
   already covers the rest.
2. **One command, one child process, no carried state.** `sh -c`, cwd on the **Path Scope**'s
   base (the **Session**'s directory, the current **Project** while the Session is still
   **Pending**), stdin closed (`Stdio::null`) so a password prompt, a pager or an editor
   fails fast instead of hanging forever, stdout and stderr merged. There is no persistent
   shell and no `cd`/`export` carry between commands — a second truth about "where am I"
   is worse than typing `cd x && …`. The card names the cwd it actually used.
3. **The output goes to a file, not through cola.** The child's stdout and stderr are
   redirected straight into its **Terminal Log**, and cola tails that file's delta on the
   render cadence. This is not only bookkeeping: on a pipe, cola's own death closes the read
   end and the child dies of `SIGPIPE` on its next write — which would make decision 8's
   orphan impossible. Direct-to-file means a command outlives cola, the file we need for
   「📄 完整输出」 is already there, and the live tail is "read the bytes appended since the
   last offset" rather than a ring buffer fed by a reader.
4. **The result is a Terminal Card, and it is a command card.** A standalone member of the
   `/dir`-`/switch`-`/help`-`File Viewer` family (ADR-0083 §3): it joins no **Card Chain**,
   is never written to the **Chain Record**, and is never reaped. It shows the resolved cwd,
   the command, a live tail (the newest ~40 lines, ANSI stripped, non-UTF-8 read lossily) and,
   once settled, the exit code and elapsed time; the full output beyond the card rides
   ADR-0076/ADR-0083's **File Message** ladder under a bounded cap. It carries 「⏹ 终止」,
   「↻ 重跑」, 「📄 完整输出」 and 「问 AI」. 「重跑」 carries the command string inside its own
   payload, so the button still works after a cola restart — no durable history is needed for
   it, and none is kept: the in-memory cache only feeds the button.
5. **Serial, bounded, and killable.** One Terminal Command per **Chat**/**Topic** at a time; a
   second is refused with the running card's 「⏹ 终止」 named in the reply — a hidden queue
   would land a forgotten command in the working tree minutes later. The default hard cap is
   10 minutes, `--timeout` extends it per command; at the cap cola stops tailing and offers
   「继续」/「⏹ 终止」 rather than killing silently, because the default guards against
   forgetting, not against a deliberate long run. 终止 signals the **process group**
   (`SIGTERM`, then `SIGKILL` after a grace), because killing only the parent leaves the
   compilers it spawned running.
6. **The environment is inherited, minus cola's own credentials — and this is not a
   sandbox.** `OPENCODE_SERVER_PASSWORD`, `OPENCODE_PASSWORD` and `OPENCODE_SERVER_USERNAME`
   are stripped (the same names `discovery.rs` already strips in the other direction, with
   its removals-last rule); everything else is the Host's shell. The limits are stated, not
   implied: `cat ~/.config/opencode/auth.json` still works, `env` still shows whatever the
   Host exported, and every card is readable by every member of the chat. The Access List
   keeps the *act* private; it cannot keep the *result* private.
7. **The model is not in the loop, and the transcript does not learn.** A Terminal Command
   enters no **Turn**, no **Session Transcript**, and no context. Both reference
   implementations inject output into the conversation; the `!`/`/run` surface here is a
   terminal in a chat, and silently turning every `ls` into a prompt is a context-cost and a
   surprise. 「问 AI」 is the explicit door: it submits the command and its output as a prompt
   through the normal pipeline, so it opens a real **Turn** and costs what a prompt costs.
8. **A restart loses the run, and the card says so.** The child's fate depends on how cola was
   launched, and cola cannot know which: hand-launched, the child survives as an orphan
   (direct-to-file, decision 3); supervised by systemd, the restart's cgroup termination
   (`KillMode=control-group`) takes it too, `setsid` notwithstanding; launchd has no cgroup.
   The card therefore reads 「已失去跟踪（**可能**仍在运行）」, never 「仍在运行」, and the startup
   sweep settles it as lost — never as done. Its Terminal Log survives, which is why the sweep
   is by **age** (the `[bridge] log_days` precedent) and never a wholesale boot-time delete: a
   blanket delete would unlink a file a live orphan is still writing.
9. **No guard against a command that operates on cola itself.** `!kill <cola pid>`,
   `!cola restart`, `!rm -rf ~/.cola` and `!pkill -f opencode` all run. The terminal is the
   emergency exit when cola itself is wedged, and pattern-matching a "self-affecting" list is
   the same fake safety as a command whitelist — `kill $(cat ~/.cola/cola.lock)` defeats any
   list. The consequences are accepted and documented: a command that kills cola leaves a
   frozen card with no exit code, re-running cola is a lock-protected no-op, deleting
   `access.json` un-claims the bot, and the recovery path is `/restart` (or the Supervisor),
   never the button on a card whose process is gone.
10. **The completion notice is the existing one.** A Terminal Command that ran past five
    minutes owes the same notice a long **Turn** does — `[bridge] long_task_notice` in p2p,
    `[bridge] group_completion_notice` in groups — because a card patch pushes no
    notification (ADR-0074). No new switch.
11. **Unix first; Windows refuses and says why.** cola ships a Windows build, but the hard half
    is not the shell (`sh -c` vs `cmd /C`), it is terminating the whole tree: `setsid` and a
    negative pid on Unix versus a Job Object or `taskkill /T` on Windows. V1 answers Windows
    with a stated refusal rather than a half-implemented lane; the loop is its own ticket.

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

## Consequences

- **A new card family member and a new module.** The Terminal Card joins the command card
  family; the execution, tail and kill logic lives in one Host-capability module shaped like
  `viewer` (backend-neutral, generation-neutral), so no generation strategy and no fixture is
  involved.
- **`!` is a breaking parse change.** Messages that begin with `!` stop reaching the model.
  Bounded deliberately to the first character, and documented in `/help run`.
- **The blast radius is the Host's own shell.** A Terminal Command can kill cola, kill the
  OpenCode server, or destroy the working tree. That is the feature, and it is why the three
  boundary sentences above are part of the decision rather than an appendix.
- **A frozen card is a normal outcome, not a bug.** Any command that kills its parent leaves
  one; the startup sweep settles it, the log keeps the evidence, and `/restart` is the way
  back.
- **The Terminal Log is a new at-rest secret surface.** Temporary by design, capped, swept by
  age, and named so it can never be confused with the application log.
