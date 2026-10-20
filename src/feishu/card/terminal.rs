//! The **Terminal Card** (ADR-0086 §4): one **Terminal Command** rendered as a
//! member of the *command card* family — a standalone reply that joins no
//! **Card Chain**, is never written to the **Chain Record** and is never
//! reaped, exactly like the **File Viewer**.
//!
//! It names the cwd the command actually ran in and the command itself, shows
//! the merged output the **Terminal Log** holds, and settles with the exit code
//! and elapsed time. Four states share one builder: [`TerminalState::Starting`]
//! is the card sent before the child exists (no controls), [`TerminalState::Live`]
//! is patched on the render cadence while the process group runs,
//! [`TerminalState::Settled`] is the one settling write the patch loop stops
//! after (#823), and [`TerminalState::Lost`] is what a restart's sweep settles a
//! run it can no longer track into (#827). The distinction is a state, not a
//! flag, so the loop can end on it and the buttons can follow it.
//!
//! Every fenced element — cwd, command and output alike — is clipped through the
//! shared [`clip_fenced_body`] walk, so a long backtick run can never make the
//! fence (which grows with the run) push the element past
//! [`MAX_ELEMENT_TEXT_CHARS`].
//!
//! The display tail is line-oriented and bounded twice (ADR-0086 §"The
//! no-terminal constraint" and decision 4): each line is capped at
//! [`TERMINAL_LINE_CHARS`] with a visible marker (one minified line can be
//! megabytes), and the newest whole lines are kept inside
//! [`TERMINAL_TAIL_CHARS`]. A `\r` inside a line is an overwrite, so a progress
//! bar collapses to its last frame rather than carrying every frame; ANSI and
//! other control sequences are stripped here, from the display only — the
//! **Terminal Log** keeps the bytes unchanged as evidence.

use std::time::Duration;

use serde_json::json;

use super::sanitize::CardMarkdown;
use super::shell::{card_shell, fmt_elapsed};
use super::tool_render::{clip_fenced_body, clip_fenced_tail};
use super::{MAX_ELEMENT_TEXT_CHARS, fenced_code};

/// The newest characters of a **Terminal Log** a card shows: the live tail and
/// the settled tail share this one budget (ADR-0086 §4).
pub(crate) const TERMINAL_TAIL_CHARS: usize = 2_500;

/// The per-line cap, well under the tail budget so a few lines always fit:
/// one minified line can be megabytes, and a fenced element cannot carry it.
pub(crate) const TERMINAL_LINE_CHARS: usize = 500;

/// The visible end of a line the per-line cap cut.
const LINE_CUT_MARKER: &str = " …（本行过长，已截断）";

/// The settled card's retrieval button (ADR-0086 decision 7). The label is a
/// constant so the handler and its tests name the same act.
pub(crate) const TERMINAL_FULL_OUTPUT_LABEL: &str = "📄 完整输出";

/// The command's own re-run (ADR-0086 decisions 4 and 5): a settled card and a
/// lost one keep it, and its payload names the run the record check compares.
pub(crate) const TERMINAL_RERUN_LABEL: &str = "↻ 重跑";

/// The settled card's explicit door to the model (ADR-0086 decision 9): the
/// only path by which a Terminal Command's output reaches the Session
/// Transcript. Constant for the same reason as the retrieval label.
pub(crate) const TERMINAL_ASK_AI_LABEL: &str = "问 AI";

/// What the settled card's buttons carry: the log path, the Chat/Topic the card
/// lives in, and the settled card's own render inputs, so a click's outcome can
/// be PATCHed back onto the card it was clicked on — and every button is
/// self-describing after a restart (ADR-0086 decision 4). A settled run gets
/// them whatever ended it — a spawn failure included, because 重跑 replays the
/// record and 完整输出 still names the log (whole-branch review, finding 3); each
/// action is gated on what it needs, not on the run having produced output. The
/// cwd and command are NOT carried here: the builder puts the text its elements
/// actually show into the payload, so a huge command cannot bloat the card's
/// JSON budget.
pub(crate) struct TerminalFullOutput<'a> {
    /// The **Terminal Log** the 「📄 完整输出」 button delivers. A run that never
    /// spawned still has one: it is created before the child.
    pub(crate) log_path: &'a str,
    /// The run every settled button names (ADR-0086 decision 4): a click for
    /// any other id is a stale click once the run record can check it.
    pub(crate) run_id: &'a str,
    /// The run 「↻ 重跑」 may replay, when the card itself named one. `None` on a
    /// legacy payload that carried no run id (#828) — the rebuild's fallback,
    /// the Chat/Topic's current record, may name a different command, and a
    /// rerun replays the record, so offering it there would replay the wrong
    /// one — and on a run whose record could not be written, where no click
    /// could prove itself current. 「问 AI」 still uses [`Self::run_id`], because
    /// it quotes the output the payload carries.
    pub(crate) rerun_run_id: Option<&'a str>,
    /// The Chat/Topic the card lives in, for the click's route and in-flight
    /// claim.
    pub(crate) chat_id: &'a str,
    pub(crate) thread_id: &'a str,
    /// Whether the sweep marked this run lost (ADR-0086 decision 10): the
    /// outcome PATCH must rebuild a lost card, never quietly relabel it as a
    /// settled one — a restart is not an exit code.
    pub(crate) lost: bool,
    /// Whether the card lives in a group chat. A click carries no chat type of
    /// its own, and the 「问 AI」 submission needs it to rebuild the
    /// Chat/Topic route (the group-lobby guidance and the chat type's notice
    /// opt-in).
    pub(crate) is_group: bool,
    pub(crate) exit_code: Option<i32>,
    pub(crate) elapsed: Duration,
    /// Whether the size probe stopped the run; the rebuilt card must still say
    /// 「输出超限」.
    pub(crate) output_over_limit: bool,
}

/// What the card is: the live state the patch loop refreshes, or the settled
/// result it stops on.
pub(crate) enum TerminalState<'a> {
    /// The card is sent before the child exists (ADR-0086 decision 5): it
    /// carries no controls, because there is no live run to stop yet.
    Starting,
    /// The process group still has members; the tail may still grow and no exit
    /// code exists. `past_cap` is the one state in which 「继续」 is available;
    /// `survivors` means `sh` itself exited and background members remain.
    Live { past_cap: bool, survivors: bool },
    /// The process group emptied (or never started): the one settling write.
    Settled {
        /// The child's real exit code; `None` when it was killed by a signal or
        /// never started at all.
        exit_code: Option<i32>,
        /// A spawn failure (missing cwd, no `sh`, …) — reported on the card so a
        /// broken launch is never an empty card.
        spawn_error: Option<&'a str>,
        elapsed: Duration,
        /// 终止 ended the run (it may also have outlived `sh` itself).
        stopped: bool,
        /// A group member outlived `sh`; the run was still live until it exited.
        survivors: bool,
    },
    /// A restart found the record still `running` and swept it (ADR-0086
    /// decisions 5 and 10): cola no longer tracks the run, and says so instead
    /// of guessing whether the child survived. 重跑, 完整输出 and 问 AI stay
    /// usable here — that is what keeps a restart from being collateral damage
    /// to the buttons.
    Lost,
}

/// Everything a Terminal Card renders: the resolved cwd, the command, the
/// merged output (already read from the log), and the state.
pub(crate) struct TerminalCard<'a> {
    pub(crate) cwd: &'a str,
    pub(crate) command: &'a str,
    pub(crate) output: &'a str,
    /// Whether the read itself omitted older bytes before the display tail was
    /// computed (the log is longer than the read window).
    pub(crate) output_truncated: bool,
    /// Whether the size probe stopped the run at the log's **observed**
    /// threshold (ADR-0086 decision 7). The settle line then reads 「输出超限」
    /// instead of reporting a signal.
    pub(crate) output_over_limit: bool,
    /// The settled card's button payload, when the run settled and produced a
    /// log worth retrieving.
    pub(crate) full_output: Option<TerminalFullOutput<'a>>,
    /// An outcome line the delivery PATCHes on (ADR-0086 decision 7): what was
    /// sent, and — when the log was past the platform's cap — that the file was
    /// not sent in full and where the bytes live.
    pub(crate) notice: Option<&'a str>,
    pub(crate) state: TerminalState<'a>,
    /// The id a click must name to be current. A live card's controls carry it,
    /// and so do a settled or lost card's buttons — either here (「↻ 重跑」) or
    /// through [`TerminalFullOutput`] — so every click can be checked against
    /// the chat's current **Terminal Run Record**.
    pub(crate) run_id: &'a str,
    pub(crate) chat_id: &'a str,
    pub(crate) thread_id: &'a str,
}

/// Build a Terminal Card in any state: the live card carries the run controls
/// (`⏹ 终止`, `继续` past the cap), the settled card its `📄 完整输出` and
/// `问 AI` buttons (ADR-0086 §4).
pub(crate) fn build_terminal_card(view: TerminalCard<'_>) -> serde_json::Value {
    let mut markdown = CardMarkdown::new();
    let (cwd_element, cwd_clipped) = fenced_element("**目录**", view.cwd, false, &mut markdown);
    let (command_element, command_clipped) = fenced_element("**命令**", view.command, false, &mut markdown);
    let (tail, budget_cut) = display_tail(view.output);
    let truncated = budget_cut || view.output_truncated;
    // Every state-derived decision — header, colour, state line, the
    // placeholder for an empty tail, and whether run controls belong — is made
    // in one match, so the live/settled split has a single owner.
    let (header, template, line, empty_tail, controls) = match &view.state {
        TerminalState::Lost => (
            "🖥️ 终端命令 · 已失去跟踪",
            "orange",
            // The wording is load-bearing (ADR-0086 decision 10): a supervised
            // cola's restart takes the child with it, a hand-launched one's
            // does not, so 「可能」 is the only honest verb.
            "⚠️ 已失去跟踪（可能仍在运行）—— cola 重启后不再跟踪这条命令，日志保留在终端日志里。".to_string(),
            "（无输出）",
            false,
        ),
        TerminalState::Starting => (
            "🖥️ 终端命令 · 启动中",
            "blue",
            "⏳ 启动中…".to_string(),
            "（等待输出…）",
            false,
        ),
        TerminalState::Live { past_cap, survivors } => {
            let line = if *survivors && *past_cap {
                "⏳ 已达时限，命令进程已退出，后台进程仍在运行…"
            } else if *survivors {
                "⏳ 命令进程已退出，后台进程仍在运行…"
            } else if *past_cap {
                "⏳ 已达时限，仍在等待命令退出…"
            } else {
                "⏳ 运行中…"
            };
            let header = if *past_cap {
                "🖥️ 终端命令 · 已达时限"
            } else {
                "🖥️ 终端命令 · 运行中"
            };
            (header, "blue", line.to_string(), "（等待输出…）", true)
        }
        TerminalState::Settled {
            exit_code,
            spawn_error,
            elapsed,
            stopped,
            survivors,
        } => {
            let (line, template) = if *stopped && *survivors {
                // Termination could not be confirmed: never claim success.
                (
                    format!(
                        "⚠️ 已发送终止，但进程组可能仍在运行 · 耗时 {}",
                        elapsed_label(*elapsed)
                    ),
                    "red",
                )
            } else if *stopped {
                (format!("⏹ 已终止 · 耗时 {}", elapsed_label(*elapsed)), "grey")
            } else {
                match (*spawn_error, *exit_code) {
                    // The observed threshold is the reason the run stopped
                    // (ADR-0086 decision 7): naming it beats a bare "killed by
                    // a signal", which would hide why.
                    _ if view.output_over_limit => (
                        format!(
                            "⚠️ 输出超限：终端日志已超过 29 MB 观测阈值，命令已停止 · 耗时 {}",
                            elapsed_label(*elapsed)
                        ),
                        "red",
                    ),
                    (Some(error), _) => (format!("⚠️ 无法启动：{error}"), "red"),
                    (None, Some(0)) => (format!("✅ 退出码 0 · 耗时 {}", elapsed_label(*elapsed)), "green"),
                    (None, Some(code)) => (
                        format!("❌ 退出码 {code} · 耗时 {}", elapsed_label(*elapsed)),
                        "red",
                    ),
                    (None, None) => (
                        format!("⚠️ 进程被信号中止 · 耗时 {}", elapsed_label(*elapsed)),
                        "red",
                    ),
                }
            };
            let line = if *survivors && !*stopped {
                format!("{line}\n⚠️ 仍有后台进程未退出。")
            } else {
                line
            };
            ("🖥️ 终端命令", template, line, "（无输出）", false)
        }
    };
    let output = if tail.is_empty() {
        empty_tail.to_string()
    } else {
        tail
    };
    let (output_element, fence_clipped) = fenced_element("**输出**", &output, true, &mut markdown);

    let mut elements = vec![cwd_element, command_element];
    if cwd_clipped || command_clipped {
        // #822: a clip has to be visible. Without this the fenced body just
        // stops, and a reader takes a cut path or command for the real one —
        // exactly what the output's own cut has said for its element. The label
        // names only what was cut, so a clipped command never makes a whole cwd
        // look cut too.
        let what = match (cwd_clipped, command_clipped) {
            (true, true) => "目录和命令",
            (true, false) => "目录",
            _ => "命令",
        };
        elements.push(json!({
            "tag": "markdown",
            "content": markdown.clean(&format!("_（{what}过长，已截断显示）_")),
        }));
    }
    elements.push(output_element);
    if truncated || fence_clipped {
        elements.push(json!({
            "tag": "markdown",
            "content": markdown.clean("_（输出过长，已截断显示；完整输出已写入终端日志）_"),
        }));
    }
    elements.push(json!({
        "tag": "markdown",
        "content": markdown.clean(&line),
    }));
    if let Some(notice) = view.notice {
        elements.push(json!({
            "tag": "markdown",
            "content": markdown.clean(notice),
        }));
    }
    if let Some(full_output) = &view.full_output {
        // The payload carries the text the elements SHOW, not the raw input: a
        // multi-kilobyte command must not be duplicated into the card's JSON.
        // Clipping twice is idempotent, so the rebuilt card renders the same.
        let cwd = displayed_text("**目录**", view.cwd, false);
        let command = displayed_text("**命令**", view.command, false);
        // The 「问 AI」 window is exactly the fenced element's body, so the
        // submission quotes what the reader sees (ADR-0086 decision 9).
        let window = displayed_text("**输出**", &output, true);
        elements.push(settled_buttons_row(
            full_output,
            &cwd,
            &command,
            &window,
            truncated || fence_clipped,
        ));
    }
    if controls && !view.run_id.is_empty() {
        elements.push(run_controls_row(&view));
    }
    card_shell(header, template, elements)
}

/// The live card's controls: 「⏹ 终止」 always, 「继续」 exactly past the cap.
/// Both payloads carry the run id they were rendered for, so a click after a
/// successor run is refused as stale (ADR-0086 decision 4).
fn run_controls_row(view: &TerminalCard<'_>) -> serde_json::Value {
    let mut columns = vec![terminal_button(view, "⏹ 终止", "kill", "danger")];
    if matches!(view.state, TerminalState::Live { past_cap: true, .. }) {
        columns.push(terminal_button(view, "继续", "continue", "primary"));
    }
    json!({
        "tag": "column_set",
        "flex_mode": "flow",
        "horizontal_spacing": "default",
        "columns": columns,
    })
}

/// One run control, carrying the Chat/Topic route and the run id.
fn terminal_button(view: &TerminalCard<'_>, label: &str, op: &str, kind: &str) -> serde_json::Value {
    json!({
        "tag": "column",
        "width": "auto",
        "vertical_align": "center",
        "elements": [{
            "tag": "button",
            "type": kind,
            "text": { "tag": "plain_text", "content": label },
            "value": {
                "action": "terminal",
                "op": op,
                "run_id": view.run_id,
                "chat_id": view.chat_id,
                "thread_id": view.thread_id,
            },
        }],
    })
}

/// The finished card's buttons (ADR-0086 decisions 4, 5, 7 and 9) —
/// 「↻ 重跑」, 「📄 完整输出」 and 「问 AI」 — in one row, on a settled card and
/// on a run the sweep marked lost alike. Every payload is self-describing: the
/// run id, the log path, the Chat/Topic route, and the card's render inputs, so
/// a click can PATCH the same card with its outcome after a restart without a
/// card-view read. The 「问 AI」 payload additionally carries the output window
/// it was rendered with, so the submission needs neither a live process nor the
/// log on disk: a run the sweep marked lost still hands its result over.
fn settled_buttons_row(
    view: &TerminalFullOutput<'_>,
    cwd: &str,
    command: &str,
    output: &str,
    output_truncated: bool,
) -> serde_json::Value {
    let chat_type = if view.is_group { "group" } else { "p2p" };
    let facts = |op: &str, run_id: &str| {
        let mut value = json!({
            "action": "terminal",
            "op": op,
            "chat_id": view.chat_id,
            "thread_id": view.thread_id,
            "chat_type": chat_type,
            "path": view.log_path,
            "terminal_card": {
                "cwd": cwd,
                "command": command,
                "lost": view.lost,
                "exit_code": view.exit_code,
                "elapsed_ms": view.elapsed.as_millis() as u64,
                "output_over_limit": view.output_over_limit,
            },
        });
        // An empty id is never written (#828): a click that reads one can only
        // be refused, so the field is omitted and the payload says nothing it
        // cannot back.
        if !run_id.is_empty() {
            value["run_id"] = json!(run_id);
            value["terminal_card"]["run_id"] = json!(run_id);
        }
        value
    };
    let mut columns = Vec::new();
    if let Some(rerun_run_id) = view.rerun_run_id {
        // 「↻ 重跑」 names the run and its route, and nothing else: the record
        // holds the command, cwd and timeout it replays (ADR-0086 decision 4 as
        // amended for #827). Carrying them here would duplicate a
        // multi-kilobyte command and the resolved cwd into the card's JSON
        // budget for no reader.
        columns.push(button_column(
            TERMINAL_RERUN_LABEL,
            "primary",
            json!({
                "action": "terminal",
                "op": "rerun",
                "run_id": rerun_run_id,
                "chat_id": view.chat_id,
                "thread_id": view.thread_id,
            }),
        ));
    }
    columns.push(button_column(
        TERMINAL_FULL_OUTPUT_LABEL,
        "default",
        facts("full_output", view.run_id),
    ));
    // A click has to name its run (ADR-0086 decision 4), and a rebuild with no
    // id to name could only write a button that toasts 「无效操作」 (#828): the
    // button is omitted instead, never written with an empty id.
    if !view.run_id.is_empty() {
        let mut ask_ai = facts("ask_ai", view.run_id);
        ask_ai["output"] = json!(output);
        ask_ai["output_truncated"] = json!(output_truncated);
        columns.push(button_column(TERMINAL_ASK_AI_LABEL, "primary", ask_ai));
    }
    json!({
        "tag": "column_set",
        "flex_mode": "flow",
        "horizontal_spacing": "default",
        "columns": columns,
    })
}

/// One button cell of a settled card's row.
fn button_column(label: &str, kind: &str, value: serde_json::Value) -> serde_json::Value {
    json!({
        "tag": "column",
        "width": "auto",
        "vertical_align": "center",
        "elements": [{
            "tag": "button",
            "type": kind,
            "text": { "tag": "plain_text", "content": label },
            "value": value,
        }],
    })
}

/// One fenced element under the per-element budget. The clip charges the body's
/// JSON-escaped bytes PLUS the dynamic fence `fenced_code` will add, so the
/// whole element (prefix, fence and body) stays inside
/// [`MAX_ELEMENT_TEXT_CHARS`]; the returned flag says whether the body had to be
/// cut. The prefix itself is charged at its byte length, which is never smaller
/// than its character count (the conservative side). `keep_tail` clips from the
/// end, so the output's newest line — its diagnostic — survives.
fn fenced_element(
    prefix: &str,
    body: &str,
    keep_tail: bool,
    markdown: &mut CardMarkdown,
) -> (serde_json::Value, bool) {
    let clipped = displayed_text(prefix, body, keep_tail);
    let was_clipped = clipped != body;
    let block = fenced_code(&clipped, None);
    let element = json!({
        "tag": "markdown",
        "content": markdown.clean(&format!("{prefix}\n{block}")),
    });
    (element, was_clipped)
}

/// The body a fenced element under `prefix` actually shows: clipped to the
/// per-element budget, from the head or (the output's newest line) the tail.
/// The button payload uses this too, so what it carries is exactly what the
/// card renders — never the raw input.
fn displayed_text(prefix: &str, body: &str, keep_tail: bool) -> String {
    let budget = MAX_ELEMENT_TEXT_CHARS.saturating_sub(prefix.len() + 1);
    if keep_tail {
        clip_fenced_tail(body, None, budget)
    } else {
        clip_fenced_body(body, None, budget)
    }
}

/// Render the newest output for display: split on `\n`, treat a `\r` inside a
/// line as an overwrite (keep the last frame), strip ANSI/control sequences,
/// cap each line, then keep the newest whole lines inside
/// [`TERMINAL_TAIL_CHARS`]. Returns the body and whether the budget omitted
/// older content.
fn display_tail(raw: &str) -> (String, bool) {
    let mut lines: Vec<String> = raw.split('\n').map(display_line).collect();
    // A final newline is a line terminator, not an empty last line.
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }

    let mut kept: Vec<String> = Vec::new();
    let mut used = 0;
    let mut cut = false;
    for line in lines.iter().rev() {
        // The separator only exists between two kept lines.
        let cost = line.chars().count() + usize::from(!kept.is_empty());
        if used + cost > TERMINAL_TAIL_CHARS {
            cut = true;
            break;
        }
        used += cost;
        kept.push(line.clone());
    }
    kept.reverse();
    (kept.join("\n"), cut)
}

/// One line as the card shows it: the last `\r` frame, control-free, and capped
/// with a visible marker.
fn display_line(line: &str) -> String {
    // A trailing `\r` returns the cursor; it is not a frame's content.
    let line = line.trim_end_matches('\r');
    let frame = line.rsplit('\r').next().unwrap_or("");
    let cleaned = strip_ansi_and_control(frame);
    if cleaned.chars().count() > TERMINAL_LINE_CHARS {
        let head: String = cleaned.chars().take(TERMINAL_LINE_CHARS).collect();
        format!("{head}{LINE_CUT_MARKER}")
    } else {
        cleaned
    }
}

/// Remove ANSI escape sequences and other control characters from display
/// text, keeping tabs (a code block's indentation). The Terminal Log keeps the
/// original bytes. This is the char-level pass: the raw 8-bit C1 controls are
/// not valid UTF-8 and are stripped on the bytes by [`decode_log_bytes`], which
/// is where a log window is turned into text.
fn strip_ansi_and_control(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => skip_escape(&mut chars),
            // The 8-bit C1 introducers, in their UTF-8 two-byte form (a raw
            // `0x9b` never reaches this pass — see `strip_raw_c1`): CSI, OSC,
            // DCS, SOS, PM and APC respectively.
            '\u{9b}' => skip_csi(&mut chars),
            '\u{9d}' => skip_string(&mut chars, true),
            '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => skip_string(&mut chars, false),
            '\t' => out.push('\t'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Skip one ESC-introduced sequence: CSI (`ESC [`), the string controls —
/// OSC (`ESC ]`), DCS (`ESC P`), SOS (`ESC X`), PM (`ESC ^`) and APC (`ESC _`) —
/// the two-character character-set designators, or a bare ESC plus one char.
fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.next() {
        Some('[') => skip_csi(chars),
        // OSC is the one string control some programs end with BEL, and it is
        // the only one that accepts it (#823b).
        Some(']') => skip_string(chars, true),
        // DCS, SOS, PM and APC carry an opaque payload terminated by ST
        // (`ESC \`) only. Treating them as OSC — or as nothing — left the
        // payload such as `ESC P1;2|payload ESC \` visible on the card.
        Some('P' | 'X' | '^' | '_') => skip_string(chars, false),
        Some('(' | ')' | '*' | '+' | '-' | '.' | '/' | '#' | '%') => {
            chars.next();
        }
        _ => {}
    }
}

/// Skip a CSI sequence up to and including its final byte (`@`–`~`).
fn skip_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for c in chars.by_ref() {
        if ('\u{40}'..='\u{7e}').contains(&c) {
            break;
        }
    }
}

/// Skip a string-control payload up to ST — `ESC \` or the 8-bit `0x9c`.
/// `bel_ends` is the OSC exception (#823b): some programs terminate an OSC with
/// BEL, and only OSC accepts it — a DCS/SOS/PM/APC payload may legitimately
/// contain one, so BEL there must not end the skip.
fn skip_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, bel_ends: bool) {
    while let Some(c) = chars.next() {
        if c == '\u{9c}' || (bel_ends && c == '\u{7}') {
            break;
        }
        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
            chars.next();
            break;
        }
    }
}

/// Decode one window of **Terminal Log** bytes into display text (#823a).
///
/// The raw 8-bit C1 introducers (`0x90`, `0x9b`, `0x9d`, …) are not valid
/// UTF-8, so `String::from_utf8_lossy` folds each into U+FFFD before
/// [`strip_ansi_and_control`] can see it — and the sequence's payload then
/// renders as ordinary text. Their introducer bytes are recognised here, on the
/// bytes, and their payload is skipped; the rest is decoded lossily exactly as
/// before.
pub(crate) fn decode_log_bytes(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&strip_raw_c1(bytes)).into_owned()
}

/// Strip the raw 8-bit C1 controls from one window of log bytes (#823a).
///
/// C1 introducers are continuation-range bytes, so they can never start a
/// character: the decision is made per byte only where no valid UTF-8 character
/// starts. A log that carries the controls in their UTF-8 two-byte form
/// (`0xC2 0x9B`) stays whole here and is handled by the char-level pass, which
/// is also where every other control is removed.
fn strip_raw_c1(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if let Some(len) = utf8_char_len(bytes, i) {
            out.extend_from_slice(&bytes[i..i + len]);
            i += len;
            continue;
        }
        match byte {
            // CSI: its parameters, up to and including the final byte.
            0x9b => i = skip_csi_bytes(bytes, i + 1),
            // OSC, DCS, SOS, PM and APC: the opaque payload up to ST. BEL ends
            // OSC only, the same rule as the char-level pass (#823b).
            0x90 | 0x98 | 0x9d | 0x9e | 0x9f => i = skip_string_bytes(bytes, i + 1, byte == 0x9d),
            // A stray C1 control — a terminator whose introducer was already
            // consumed, PAD, NEL, …: a control character, never content, so it
            // is dropped exactly as the char-level pass drops a control.
            0x80..=0x9f => i += 1,
            // Any other byte that starts no character: keep the replacement
            // character the lossy decode would have produced, so nothing
            // textual silently changes size or meaning.
            _ => {
                out.extend_from_slice("\u{fffd}".as_bytes());
                i += 1;
            }
        }
    }
    out
}

/// The length of the valid UTF-8 character starting at `i`, or `None` when the
/// byte there starts no complete character (an invalid or truncated sequence).
fn utf8_char_len(bytes: &[u8], i: usize) -> Option<usize> {
    let len = match bytes[i] {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    let slice = bytes.get(i..i.checked_add(len)?)?;
    std::str::from_utf8(slice).is_ok().then_some(len)
}

/// Skip a raw C1 CSI sequence (`0x9b`): its parameters up to and including the
/// final byte (`0x40`–`0x7e`), the same rule as [`skip_csi`].
fn skip_csi_bytes(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        let byte = bytes[i];
        i += 1;
        if (0x40..=0x7e).contains(&byte) {
            break;
        }
    }
    i
}

/// Skip a raw C1 string-control payload up to ST — `0x9c` or `ESC \` — with the
/// OSC-only BEL terminator, the same rule as [`skip_string`].
fn skip_string_bytes(bytes: &[u8], mut i: usize, bel_ends: bool) -> usize {
    while i < bytes.len() {
        match bytes[i] {
            0x9c => return i + 1,
            0x07 if bel_ends => return i + 1,
            0x1b if bytes.get(i + 1) == Some(&b'\\') => return i + 2,
            _ => i += 1,
        }
    }
    i
}

/// The settle line's elapsed label: milliseconds under a second (a quick
/// `/run echo` must not read `0s`), hundredths up to a minute, then the shared
/// minute/hour/day grammar.
pub(crate) fn elapsed_label(elapsed: Duration) -> String {
    if elapsed < Duration::from_secs(1) {
        format!("{}ms", elapsed.as_millis())
    } else if elapsed < Duration::from_secs(60) {
        format!("{:.2}s", elapsed.as_secs_f64())
    } else {
        fmt_elapsed(elapsed.as_secs())
    }
}

/// What 「📄 完整输出」's delivery reports on the clicked card (ADR-0086
/// decision 7): what landed, and — past the upload cap — that the file was NOT
/// the whole log, with the path its remaining bytes live at.
pub(crate) enum FullOutputOutcome<'a> {
    /// One File Message went out; `truncated` means it carried only the bounded
    /// first 30 MB of a longer log.
    Sent {
        truncated: bool,
        total_size: u64,
        path: &'a str,
    },
    /// Nothing was sent (a failed upload, a refused send, an unreadable log).
    NotSent { reason: &'a str, path: &'a str },
    /// The send may have landed; it is not retried (ADR-0076's ambiguity rule).
    Unknown { path: &'a str },
}

/// The one line the delivery PATCHes onto the card. It always names where the
/// log's bytes are, so a truncated or failed send still leaves the output
/// reachable on the Host.
pub(crate) fn full_output_notice(outcome: FullOutputOutcome<'_>) -> String {
    match outcome {
        FullOutputOutcome::Sent {
            truncated: false,
            total_size,
            ..
        } => format!(
            "📄 完整输出已作为文件消息发送（{}）。",
            crate::backend::file_content::human_size(total_size)
        ),
        FullOutputOutcome::Sent {
            truncated: true,
            total_size,
            path,
        } => {
            // Derived from the platform cap itself, so the copy cannot drift
            // from `feishu::file::MAX_FILE_BYTES`.
            let cap = crate::backend::file_content::human_size(crate::feishu::file::MAX_FILE_BYTES);
            format!(
                "📄 完整输出共 {}，超过 {cap} 上传上限：文件未完整发送，仅发送了前 {cap}（超出部分未发送，末行为截断说明）。完整输出在 `{path}`。",
                crate::backend::file_content::human_size(total_size)
            )
        }
        FullOutputOutcome::NotSent { reason, path } => {
            format!("⚠️ 完整输出未发送（{reason}）。文件在 `{path}`。")
        }
        FullOutputOutcome::Unknown { path } => {
            format!("⚠️ 完整输出发送结果未知，为避免重复未重试。文件在 `{path}`。")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settled(stopped: bool, survivors: bool) -> serde_json::Value {
        build_terminal_card(TerminalCard {
            cwd: "/work",
            command: "sleep 5",
            output: "",
            output_truncated: false,
            output_over_limit: false,
            full_output: None,
            notice: None,
            state: TerminalState::Settled {
                exit_code: None,
                spawn_error: None,
                elapsed: Duration::from_secs(1),
                stopped,
                survivors,
            },
            run_id: "",
            chat_id: "",
            thread_id: "",
        })
    }

    /// A termination the run could not confirm is not reported as one
    /// (#825/#826 review): the card says the group may still be alive.
    #[test]
    fn an_unconfirmed_termination_does_not_claim_success() {
        let text = crate::bridge::test_support::card_text(&settled(true, true));
        assert!(text.contains("可能仍在运行"), "{text}");
        assert!(!text.contains("⏹ 已终止"), "{text}");

        let text = crate::bridge::test_support::card_text(&settled(true, false));
        assert!(text.contains("⏹ 已终止"), "{text}");
        assert!(!text.contains("可能仍在运行"), "{text}");
    }

    /// #827: the losing half of a restart is the card, and 「已失去跟踪」 must
    /// never read as 「仍在运行」 — or as an exit code the sweep never saw. The
    /// card it rebuilds keeps the buttons a restart must not break: 重跑, 完整输出
    /// and 问 AI.
    #[test]
    fn a_lost_card_reports_uncertainty_and_keeps_its_buttons() {
        let card = build_terminal_card(TerminalCard {
            cwd: "/work",
            command: "make",
            output: "building\n",
            output_truncated: false,
            output_over_limit: false,
            full_output: Some(TerminalFullOutput {
                log_path: "/logs/run.log",
                chat_id: "chat_1",
                thread_id: "chat_1",
                run_id: "run-827",
                rerun_run_id: Some("run-827"),
                lost: true,
                is_group: false,
                exit_code: None,
                elapsed: Duration::ZERO,
                output_over_limit: false,
            }),
            notice: None,
            state: TerminalState::Lost,
            run_id: "run-827",
            chat_id: "chat_1",
            thread_id: "chat_1",
        });
        let text = crate::bridge::test_support::card_text(&card);
        assert!(text.contains("已失去跟踪（可能仍在运行）"), "{text}");
        assert!(!text.contains("退出码"), "a lost run has no exit code: {text}");
        assert!(text.contains("building"), "the log's tail survives: {text}");
        assert!(
            text.contains(TERMINAL_FULL_OUTPUT_LABEL),
            "完整输出 stays: {text}"
        );
        assert!(text.contains(TERMINAL_ASK_AI_LABEL), "问 AI stays: {text}");
        assert!(text.contains(TERMINAL_RERUN_LABEL), "重跑 stays: {text}");
        assert!(
            !text.contains("⏹ 终止"),
            "there is nothing left to signal: {text}"
        );
        let value = card.to_string();
        assert!(value.contains("\"op\":\"rerun\""), "the rerun payload: {value}");
        assert!(
            value.contains("\"run_id\":\"run-827\""),
            "it names the run: {value}"
        );
        assert!(
            value.contains("\"lost\":true"),
            "the outcome keeps it lost: {value}"
        );
    }

    /// #827 review (finding 3): 「↻ 重跑」 names only the run and its route. The
    /// command, cwd and timeout live in the **Terminal Run Record**, so a
    /// multi-kilobyte command is never duplicated into the card's JSON budget.
    #[test]
    fn the_rerun_button_carries_only_the_run_and_its_route() {
        fn find_button(card: &serde_json::Value, op: &str) -> Option<serde_json::Value> {
            match card {
                serde_json::Value::Object(map) => {
                    if map.get("tag").and_then(|tag| tag.as_str()) == Some("button")
                        && map
                            .get("value")
                            .and_then(|value| value.get("op"))
                            .and_then(|op| op.as_str())
                            == Some(op)
                    {
                        return map.get("value").cloned();
                    }
                    map.values().find_map(|value| find_button(value, op))
                }
                serde_json::Value::Array(items) => items.iter().find_map(|value| find_button(value, op)),
                _ => None,
            }
        }

        let card = build_terminal_card(TerminalCard {
            cwd: "/work",
            command: &"x".repeat(5_000),
            output: "",
            output_truncated: false,
            output_over_limit: false,
            full_output: Some(TerminalFullOutput {
                log_path: "/logs/run.log",
                chat_id: "chat_1",
                thread_id: "chat_1",
                run_id: "run-827",
                rerun_run_id: Some("run-827"),
                lost: false,
                is_group: false,
                exit_code: Some(0),
                elapsed: Duration::from_secs(1),
                output_over_limit: false,
            }),
            notice: None,
            state: TerminalState::Settled {
                exit_code: Some(0),
                spawn_error: None,
                elapsed: Duration::from_secs(1),
                stopped: false,
                survivors: false,
            },
            run_id: "run-827",
            chat_id: "chat_1",
            thread_id: "chat_1",
        });
        let rerun = find_button(&card, "rerun").expect("the settled card carries 重跑");
        let mut keys: Vec<&str> = rerun
            .as_object()
            .expect("a button value is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["action", "chat_id", "op", "run_id", "thread_id"],
            "the rerun payload is the run and its route: {rerun}"
        );
        assert_eq!(rerun["run_id"], "run-827");
        // The same card's 完整输出 keeps its own payload (path + render inputs).
        let full = find_button(&card, "full_output").expect("完整输出");
        assert!(full.get("terminal_card").is_some(), "{full}");
    }

    /// #827: the card the send happens on says 「启动中」 and offers no control —
    /// the record and the child do not exist yet (ADR-0086 decision 5).
    #[test]
    fn a_starting_card_offers_nothing_to_click() {
        let card = build_terminal_card(TerminalCard {
            cwd: "/work",
            command: "make",
            output: "",
            output_truncated: false,
            output_over_limit: false,
            full_output: None,
            notice: None,
            state: TerminalState::Starting,
            run_id: "run-827",
            chat_id: "chat_1",
            thread_id: "chat_1",
        });
        let text = crate::bridge::test_support::card_text(&card);
        assert!(text.contains("启动中"), "{text}");
        assert!(
            !text.contains("⏹ 终止") && !text.contains(TERMINAL_RERUN_LABEL),
            "{text}"
        );
        assert_eq!(card.to_string().matches("\"tag\":\"button\"").count(), 0);
    }

    /// A card whose cwd and command are exactly `cwd`/`command`, settled.
    fn card_with<'a>(cwd: &'a str, command: &'a str) -> serde_json::Value {
        build_terminal_card(TerminalCard {
            cwd,
            command,
            output: "done\n",
            output_truncated: false,
            output_over_limit: false,
            full_output: None,
            notice: None,
            state: TerminalState::Settled {
                exit_code: Some(0),
                spawn_error: None,
                elapsed: Duration::from_secs(1),
                stopped: false,
                survivors: false,
            },
            run_id: "",
            chat_id: "",
            thread_id: "",
        })
    }

    /// #823a: a raw C1 introducer is not valid UTF-8, so the lossy decode used
    /// to fold it into U+FFFD before the char pass — and everything the
    /// introducer carried rendered as text. The byte pass strips each raw
    /// string control with its payload; the char pass keeps handling the UTF-8
    /// form, which is valid bytes and must survive the byte pass untouched.
    #[test]
    fn a_raw_c1_string_control_is_stripped_with_its_payload() {
        let raw: &[u8] = b"before\x90dcs-payload\x9cafter\x9b31mcolour\x9dosc-title\x9cextra";
        assert_eq!(decode_log_bytes(raw), "beforeaftercolourextra");

        assert_eq!(decode_log_bytes("héllo 世界".as_bytes()), "héllo 世界");
        assert_eq!(
            decode_log_bytes("a\u{90}b\u{9c}c".as_bytes()),
            "a\u{90}b\u{9c}c",
            "the UTF-8 two-byte form is the char pass's job"
        );
        assert_eq!(strip_ansi_and_control("\u{90}hidden\u{9c}shown"), "shown");
        assert_eq!(strip_ansi_and_control("\u{9d}title\u{9c}shown"), "shown");
        // A stray C1 byte is a control, not content: dropped, never U+FFFD.
        assert_eq!(decode_log_bytes(b"a\x9cb\x85c"), "abc");
    }

    /// #823b: BEL ends an OSC only. A DCS/SOS/PM/APC payload may contain one,
    /// and treating it as a terminator left the rest of that payload visible.
    #[test]
    fn bel_ends_only_an_osc_string_control() {
        assert_eq!(strip_ansi_and_control("\u{1b}]0;title\u{7}visible"), "visible");
        assert_eq!(
            strip_ansi_and_control("\u{1b}P1;2|payload\u{7}more\u{1b}\\visible"),
            "visible"
        );
        for introducer in ['X', '^', '_'] {
            let input = format!("\u{1b}{introducer}payload\u{7}more\u{1b}\\visible");
            assert_eq!(strip_ansi_and_control(&input), "visible", "{introducer}");
        }
        // The raw byte form follows the same rule.
        assert_eq!(decode_log_bytes(b"a\x90payload\x07still\x9cb"), "ab");
    }

    /// #828: a rebuild with no run id to name omits 「问 AI」 — a button whose
    /// click could only toast 「无效操作」 — and never writes an empty id.
    /// 「📄 完整输出」 needs no run id and stays.
    #[test]
    fn a_card_with_no_run_id_writes_no_ask_ai_button() {
        let card = build_terminal_card(TerminalCard {
            cwd: "/work",
            command: "make",
            output: "done\n",
            output_truncated: false,
            output_over_limit: false,
            full_output: Some(TerminalFullOutput {
                log_path: "/logs/run.log",
                run_id: "",
                rerun_run_id: None,
                chat_id: "chat_1",
                thread_id: "chat_1",
                lost: false,
                is_group: false,
                exit_code: Some(0),
                elapsed: Duration::from_secs(1),
                output_over_limit: false,
            }),
            notice: None,
            state: TerminalState::Settled {
                exit_code: Some(0),
                spawn_error: None,
                elapsed: Duration::from_secs(1),
                stopped: false,
                survivors: false,
            },
            run_id: "",
            chat_id: "chat_1",
            thread_id: "chat_1",
        });
        let value = card.to_string();
        assert!(
            !value.contains("\"op\":\"ask_ai\""),
            "no dead 「问 AI」 button: {value}"
        );
        assert!(
            !value.contains("\"op\":\"rerun\""),
            "重跑 needs a run this card named: {value}"
        );
        assert!(
            value.contains("\"op\":\"full_output\""),
            "完整输出 needs no run id: {value}"
        );
        assert!(
            !value.contains("\"run_id\":\"\""),
            "an empty run id is never written: {value}"
        );
    }

    /// #822: the cwd and the command are clipped to the per-element budget, and
    /// the clip used to be silent — the fenced body just stopped, so a reader
    /// took an incomplete path or command for the real one. Each cut now says
    /// so, and names only what was cut.
    #[test]
    fn a_clipped_cwd_or_command_says_so() {
        let long = "x".repeat(MAX_ELEMENT_TEXT_CHARS + 1);

        let text = crate::bridge::test_support::card_text(&card_with(&long, "make"));
        assert!(text.contains("（目录过长，已截断显示）"), "{text}");
        assert!(
            !text.contains("命令过长"),
            "only the cut element is named: {text}"
        );

        let text = crate::bridge::test_support::card_text(&card_with("/work", &long));
        assert!(text.contains("（命令过长，已截断显示）"), "{text}");
        assert!(
            !text.contains("目录过长"),
            "only the cut element is named: {text}"
        );

        let card = card_with(&long, &long);
        let text = crate::bridge::test_support::card_text(&card);
        assert!(text.contains("（目录和命令过长，已截断显示）"), "{text}");
        for element in crate::bridge::test_support::card_texts(&card) {
            assert!(
                element.chars().count() <= MAX_ELEMENT_TEXT_CHARS,
                "the note stays inside the per-element budget ({} chars): {element}",
                element.chars().count()
            );
        }

        let text = crate::bridge::test_support::card_text(&card_with("/work", "make"));
        assert!(!text.contains("过长"), "no cut, no notice: {text}");
    }
}
