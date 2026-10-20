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
use super::tool_render::clip_fenced_body;
use super::{MAX_ELEMENT_TEXT_CHARS, fenced_code};

/// The newest characters of a **Terminal Log** a card shows: the live tail and
/// the settled tail share this one budget (ADR-0086 §4).
pub(crate) const TERMINAL_TAIL_CHARS: usize = 2_500;

/// The per-line cap, well under the tail budget so a few lines always fit:
/// one minified line can be megabytes, and a fenced element cannot carry it.
pub(crate) const TERMINAL_LINE_CHARS: usize = 500;

/// The visible end of a line the per-line cap cut.
const LINE_CUT_MARKER: &str = " …（本行过长，已截断）";

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
    pub(crate) state: TerminalState<'a>,
}

/// Build a Terminal Card in either state. No buttons in this slice: the card is
/// the run's surface (ADR-0086 §4 defers 终止/重跑/完整输出).
pub(crate) fn build_terminal_card(view: TerminalCard<'_>) -> serde_json::Value {
    let mut markdown = CardMarkdown::new();
    let (cwd_element, _) = fenced_element("**目录**", view.cwd, &mut markdown);
    let (command_element, _) = fenced_element("**命令**", view.command, &mut markdown);
    let (tail, budget_cut) = display_tail(view.output);
    let truncated = budget_cut || view.output_truncated;
    let live = matches!(view.state, TerminalState::Live);
    let output = if !tail.is_empty() {
        tail
    } else if live {
        "（等待输出…）".to_string()
    } else {
        "（无输出）".to_string()
    };
    let (output_element, fence_clipped) = fenced_element("**输出**", &output, &mut markdown);

    let mut elements = vec![cwd_element, command_element, output_element];
    if truncated || fence_clipped {
        elements.push(json!({
            "tag": "markdown",
            "content": markdown.clean("_（输出过长，已截断显示；完整输出已写入终端日志）_"),
        }));
    }
    let (header, template, line) = match view.state {
        TerminalState::Live => ("🖥️ 终端命令 · 运行中", "blue", "⏳ 运行中…".to_string()),
        TerminalState::Settled {
            exit_code,
            spawn_error,
            elapsed,
        } => {
            let (line, template) = match (spawn_error, exit_code) {
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
            ("🖥️ 终端命令", template, line)
        }
    };
    elements.push(json!({
        "tag": "markdown",
        "content": markdown.clean(&line),
    }));
    card_shell(header, template, elements)
}

/// One fenced element under the per-element budget. `clip_fenced_body` charges
/// the body's JSON-escaped bytes PLUS the dynamic fence `fenced_code` will add,
/// so the whole element (prefix, fence and body) stays inside
/// [`MAX_ELEMENT_TEXT_CHARS`]; the returned flag says whether the body had to
/// be cut. The prefix itself is charged at its byte length, which is never
/// smaller than its character count (the conservative side).
fn fenced_element(prefix: &str, body: &str, markdown: &mut CardMarkdown) -> (serde_json::Value, bool) {
    let budget = MAX_ELEMENT_TEXT_CHARS.saturating_sub(prefix.len() + 1);
    let clipped = clip_fenced_body(body, None, budget);
    let was_clipped = clipped != body;
    let block = fenced_code(&clipped, None);
    let element = json!({
        "tag": "markdown",
        "content": markdown.clean(&format!("{prefix}\n{block}")),
    });
    (element, was_clipped)
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
            // The 8-bit CSI/OSC introducers some programs emit instead of ESC.
            '\u{9b}' => skip_csi(&mut chars),
            '\u{9d}' => skip_osc(&mut chars),
            '\t' => out.push('\t'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Skip one ESC-introduced sequence: CSI (`ESC [`), OSC (`ESC ] … BEL|ESC \`),
/// the two-character character-set designators, or a bare ESC plus one char.
fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.next() {
        Some('[') => skip_csi(chars),
        Some(']') => skip_osc(chars),
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

/// Skip an OSC sequence up to BEL or the `ESC \` string terminator.
fn skip_osc(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(c) = chars.next() {
        if c == '\u{7}' {
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
