//! The **Terminal Card** (ADR-0086 §4): one **Terminal Command** rendered as a
//! member of the *command card* family — a standalone reply that joins no
//! **Card Chain**, is never written to the **Chain Record** and is never
//! reaped, exactly like the **File Viewer**.
//!
//! It names the cwd the command actually ran in and the command itself, shows
//! the merged output the **Terminal Log** holds, and settles with the exit code
//! and elapsed time. Output is rendered from a bounded tail
//! ([`TERMINAL_TAIL_CHARS`]) because a card is a fixed budget and a command can
//! write indefinitely; the log keeps every byte. Every fenced element — cwd,
//! command and output alike — is clipped through the shared
//! [`clip_fenced_body`] walk, so a long backtick run can never make the fence
//! (which grows with the run) push the element past
//! [`MAX_ELEMENT_TEXT_CHARS`]. The live tail, the tail's `\r`/ANSI handling and
//! the controls are later slices (#823/#825).

use std::time::Duration;

use serde_json::json;

use super::sanitize::CardMarkdown;
use super::shell::{card_shell, fmt_elapsed};
use super::tool_render::clip_fenced_body;
use super::{MAX_ELEMENT_TEXT_CHARS, fenced_code};

/// The newest characters of a **Terminal Log** a settled card shows: the same
/// budget the live tail will use (#823, ADR-0086 §4).
pub(crate) const TERMINAL_TAIL_CHARS: usize = 2_500;

/// Everything a Terminal Card renders: the resolved cwd, the command, the
/// merged output (already bounded by [`TERMINAL_TAIL_CHARS`]), how the child
/// ended, and how long it took.
pub(crate) struct TerminalCard<'a> {
    pub(crate) cwd: &'a str,
    pub(crate) command: &'a str,
    pub(crate) output: &'a str,
    pub(crate) output_truncated: bool,
    /// The child's real exit code; `None` when it was killed by a signal or
    /// never started at all.
    pub(crate) exit_code: Option<i32>,
    /// A spawn failure (missing cwd, no `sh`, …) — reported on the card so a
    /// broken launch is never an empty card.
    pub(crate) spawn_error: Option<&'a str>,
    pub(crate) elapsed: Duration,
}

/// Build the settled Terminal Card. No buttons in this slice: the card is the
/// tracer bullet's whole surface (ADR-0086 §4 defers 终止/重跑/完整输出).
pub(crate) fn build_terminal_card(view: TerminalCard<'_>) -> serde_json::Value {
    let mut markdown = CardMarkdown::new();
    let (cwd_element, _) = fenced_element("**目录**", view.cwd, &mut markdown);
    let (command_element, _) = fenced_element("**命令**", view.command, &mut markdown);
    let output = if view.output.is_empty() {
        "（无输出）"
    } else {
        view.output
    };
    let (output_element, fence_clipped) = fenced_element("**输出**", output, &mut markdown);

    let mut elements = vec![cwd_element, command_element, output_element];
    if view.output_truncated || fence_clipped {
        elements.push(json!({
            "tag": "markdown",
            "content": markdown.clean("_（输出过长，已截断显示；完整输出已写入终端日志）_"),
        }));
    }
    let (line, template) = match (view.spawn_error, view.exit_code) {
        (Some(error), _) => (format!("⚠️ 无法启动：{error}"), "red"),
        (None, Some(0)) => (
            format!("✅ 退出码 0 · 耗时 {}", elapsed_label(view.elapsed)),
            "green",
        ),
        (None, Some(code)) => (
            format!("❌ 退出码 {code} · 耗时 {}", elapsed_label(view.elapsed)),
            "red",
        ),
        (None, None) => (
            format!("⚠️ 进程被信号中止 · 耗时 {}", elapsed_label(view.elapsed)),
            "red",
        ),
    };
    elements.push(json!({
        "tag": "markdown",
        "content": markdown.clean(&line),
    }));
    card_shell("🖥️ 终端命令", template, elements)
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
