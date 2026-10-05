#!/usr/bin/env bash
# Shared verdict parsing for the Codex review gate (#530).
#
# One definition of "a complete review" and of the verdict it carries, sourced
# by both the post step (may this review be published?) and the approve step
# (which verdict does the published comment end with?), and exercised by
# `cargo test -p xtask codex_gate`.
#
# A review is COMPLETE when its last non-blank line ends with the verdict
# marker. Blank means empty or whitespace-only — grep's `[[:space:]]` includes
# `\r`, so a CRLF tail cannot smuggle a line after the verdict. Markdown
# emphasis (`**…**`, a code span) around the value is tolerated because the
# model emits it that way in practice; prose may precede the marker on the
# same line, but anything after the value except emphasis or whitespace (a
# period, more prose) makes the review incomplete, so a marker merely quoted
# in a sentence never counts. The value itself stays exact.

# The last non-blank line of stdin (empty when there is none).
codex_last_line_stdin() {
  grep -v '^[[:space:]]*$' 2>/dev/null | tail -n 1 || true
}

# The last non-blank line of file $1 (empty when there is none).
codex_last_line() {
  codex_last_line_stdin < "$1"
}

# The PASS/FAIL verdict that line $1 ends with (empty when it carries none).
codex_verdict_of_line() {
  printf '%s\n' "$1" | sed -nE 's/.*CODEX_REVIEW_VERDICT: (PASS|FAIL)[*`[:space:]]*$/\1/p'
}

# The verdict file $1 carries on its last non-blank line (empty when the
# output is incomplete).
codex_verdict_of_file() {
  codex_verdict_of_line "$(codex_last_line "$1")"
}

# File $1 through its last non-blank line — trailing blank lines dropped
# (a CR-only tail included), so the verdict stays the final line of anything
# built from this output.
codex_trim_trailing_blanks() {
  awk '{lines[NR]=$0} $0 !~ /^[[:space:]]*$/ {last=NR} END{for (i=1; i<=last; i++) print lines[i]}' "$1"
}
