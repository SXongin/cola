//! The **Terminal Card** (ADR-0086 §4): one **Terminal Command** rendered as a
//! member of the *command card* family — a standalone reply that joins no
//! **Card Chain**, is never written to the **Chain Record** and is never
//! reaped, exactly like the **File Viewer**.
//!
//! It names the cwd the command actually ran in and the command itself, shows
//! the merged output the **Terminal Log** holds, and settles with the exit code
//! and elapsed time. Two states share one builder: [`TerminalState::Live`] is
//! patched on the render cadence while the child runs, [`TerminalState::Settled`]
//! is the one settling write the patch loop stops after (#823). The distinction
//! is a state, not a flag, so the loop can end on it.
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

/// What 「📄 完整输出」 carries: the log path and the settled card's own render
/// inputs, so the delivery's outcome can be PATCHed back onto the card it was
/// clicked on — and the button is self-describing after a restart (ADR-0086
/// decision 4). Only a settled run without a spawn failure gets one: a launch
/// that never started has no output to retrieve. The cwd and command are NOT
/// carried here: the builder puts the text its elements actually show into the
/// payload, so a huge command cannot bloat the card's JSON budget.
pub(crate) struct TerminalFullOutput<'a> {
    /// The **Terminal Log** the button delivers.
    pub(crate) log_path: &'a str,
    /// The Chat/Topic the card lives in, for the click's route and in-flight
    /// claim.
    pub(crate) chat_id: &'a str,
    pub(crate) thread_id: &'a str,
    pub(crate) exit_code: Option<i32>,
    pub(crate) elapsed: Duration,
    /// Whether the size probe stopped the run; the rebuilt card must still say
    /// 「输出超限」.
    pub(crate) output_over_limit: bool,
}

/// What the card is: the live state the patch loop refreshes, or the settled
/// result it stops on.
pub(crate) enum TerminalState<'a> {
    /// The child is running; the tail may still grow and no exit code exists.
    Live,
    /// The child ended (or never started): the one settling write.
    Settled {
        /// The child's real exit code; `None` when it was killed by a signal or
        /// never started at all.
        exit_code: Option<i32>,
        /// A spawn failure (missing cwd, no `sh`, …) — reported on the card so a
        /// broken launch is never an empty card.
        spawn_error: Option<&'a str>,
        elapsed: Duration,
    },
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
    /// The 「📄 完整输出」 button's payload, when the run settled and produced a
    /// log worth retrieving.
    pub(crate) full_output: Option<TerminalFullOutput<'a>>,
    /// An outcome line the delivery PATCHes on (ADR-0086 decision 7): what was
    /// sent, and — when the log was past the platform's cap — that the file was
    /// not sent in full and where the bytes live.
    pub(crate) notice: Option<&'a str>,
    pub(crate) state: TerminalState<'a>,
}

/// Build a Terminal Card in either state. No buttons in this slice: the card is
/// the run's surface (ADR-0086 §4 defers 终止/重跑/完整输出).
pub(crate) fn build_terminal_card(view: TerminalCard<'_>) -> serde_json::Value {
    let mut markdown = CardMarkdown::new();
    let (cwd_element, _) = fenced_element("**目录**", view.cwd, false, &mut markdown);
    let (command_element, _) = fenced_element("**命令**", view.command, false, &mut markdown);
    let (tail, budget_cut) = display_tail(view.output);
    let truncated = budget_cut || view.output_truncated;
    // Every state-derived decision — header, colour, state line and the
    // placeholder for an empty tail — is made in one match, so the live/settled
    // split has a single owner.
    let (header, template, line, empty_tail) = match view.state {
        TerminalState::Live => (
            "🖥️ 终端命令 · 运行中",
            "blue",
            "⏳ 运行中…".to_string(),
            "（等待输出…）",
        ),
        TerminalState::Settled {
            exit_code,
            spawn_error,
            elapsed,
        } => {
            let (line, template) = match (spawn_error, exit_code) {
                // The observed threshold is the reason the run stopped
                // (ADR-0086 decision 7): naming it beats a bare "killed by a
                // signal", which would hide why.
                _ if view.output_over_limit => (
                    format!(
                        "⚠️ 输出超限：终端日志已超过 29 MB 观测阈值，命令已停止 · 耗时 {}",
                        elapsed_label(elapsed)
                    ),
                    "red",
                ),
                (Some(error), _) => (format!("⚠️ 无法启动：{error}"), "red"),
                (None, Some(0)) => (format!("✅ 退出码 0 · 耗时 {}", elapsed_label(elapsed)), "green"),
                (None, Some(code)) => (
                    format!("❌ 退出码 {code} · 耗时 {}", elapsed_label(elapsed)),
                    "red",
                ),
                (None, None) => (
                    format!("⚠️ 进程被信号中止 · 耗时 {}", elapsed_label(elapsed)),
                    "red",
                ),
            };
            ("🖥️ 终端命令", template, line, "（无输出）")
        }
    };
    let output = if tail.is_empty() {
        empty_tail.to_string()
    } else {
        tail
    };
    let (output_element, fence_clipped) = fenced_element("**输出**", &output, true, &mut markdown);

    let mut elements = vec![cwd_element, command_element, output_element];
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
    if let Some(full_output) = view.full_output {
        // The payload carries the text the elements SHOW, not the raw input: a
        // multi-kilobyte command must not be duplicated into the card's JSON.
        // Clipping twice is idempotent, so the rebuilt card renders the same.
        let cwd = displayed_text("**目录**", view.cwd, false);
        let command = displayed_text("**命令**", view.command, false);
        elements.push(full_output_row(&full_output, &cwd, &command));
    }
    card_shell(header, template, elements)
}

/// The 「📄 完整输出」 button row (ADR-0086 decisions 4 and 7). The payload is
/// self-describing: the log path plus the settled card's render inputs, so the
/// click can PATCH the same card with its outcome after a restart.
fn full_output_row(view: &TerminalFullOutput<'_>, cwd: &str, command: &str) -> serde_json::Value {
    json!({
        "tag": "column_set",
        "flex_mode": "flow",
        "horizontal_spacing": "default",
        "columns": [{
            "tag": "column",
            "width": "auto",
            "vertical_align": "center",
            "elements": [{
                "tag": "button",
                "type": "default",
                "text": { "tag": "plain_text", "content": TERMINAL_FULL_OUTPUT_LABEL },
                "value": {
                    "action": "terminal",
                    "op": "full_output",
                    "chat_id": view.chat_id,
                    "thread_id": view.thread_id,
                    "path": view.log_path,
                    "terminal_card": {
                        "cwd": cwd,
                        "command": command,
                        "exit_code": view.exit_code,
                        "elapsed_ms": view.elapsed.as_millis() as u64,
                        "output_over_limit": view.output_over_limit,
                    },
                },
            }],
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
/// original bytes.
fn strip_ansi_and_control(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => skip_escape(&mut chars),
            // The 8-bit C1 introducers some programs emit instead of ESC:
            // CSI, OSC, DCS, SOS, PM and APC respectively.
            '\u{9b}' => skip_csi(&mut chars),
            '\u{9d}' | '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => skip_string(&mut chars),
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
        // Every string control carries an opaque payload terminated by ST
        // (`ESC \`); treating only OSC as a string left a DCS payload such as
        // `ESC P1;2|payload ESC \` visible on the card.
        Some(']' | 'P' | 'X' | '^' | '_') => skip_string(chars),
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

/// Skip a string-control payload (OSC/DCS/SOS/PM/APC) up to ST — `ESC \` or
/// the 8-bit `0x9C` — or the BEL some terminals accept as a terminator.
fn skip_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(c) = chars.next() {
        if c == '\u{7}' || c == '\u{9c}' {
            break;
        }
        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
            chars.next();
            break;
        }
    }
}

/// The settle line's elapsed label: milliseconds under a second (a quick
/// `/run echo` must not read `0s`), hundredths up to a minute, then the shared
/// minute/hour/day grammar.
fn elapsed_label(elapsed: Duration) -> String {
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
