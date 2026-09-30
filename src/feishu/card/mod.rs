pub(crate) mod command;
pub(crate) mod help;
pub(crate) mod ledger;
pub(crate) mod notify;
pub(crate) mod picker;
pub(crate) mod question;
pub(crate) mod sanitize;
pub(crate) mod session;
pub(crate) mod shell;
pub(crate) mod tool_render;

/// Card state for Feishu interactive message cards.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum CardState {
    #[default]
    Loading,
    Reasoning,
    Streaming,
    /// A card that was finalized mid-turn because the content filled the card;
    /// the rest continues in the next card. Shown as "部分完成，继续中".
    Continued,
    Done,
    Error,
    /// A failed card whose retry was submitted (spec #391): terminal, all
    /// failed content preserved, header 「↩️ 已重试」, no retry button. The
    /// retried attempt renders on a NEW card below it, so the record of the
    /// failure stays readable and cannot be retried again by mistake.
    Retried,
    /// A card whose run the operator deliberately stopped with `/stop`
    /// (#394): terminal, header 「⏹ 已停止」, grey, no retry button. The abort
    /// the server recorded on the transcript is NOT a failure and its text is
    /// never written to the card. The next Turn clears the session's stopped
    /// marker and replaces this card.
    Stopped,
    /// A card whose Execution ended but whose Turn still has live Background
    /// Tasks (ADR-0059): it yields with 「⏳ 等待后台任务」 — NOT a terminal and
    /// never ✅ — and stops receiving updates. The next Wake continues the
    /// chain on a new continuation card; a later Turn supersedes this one or a
    /// switch-away collects it (spec #405).
    Waiting,
    /// A card whose Turn's submitted message never reached the session
    /// transcript and whose Session is not live (ADR-0062): a steered admit
    /// no runner promoted, so nobody will answer it. Header
    /// 「⚠️ 这条消息未被接收」, terminal and never ✅ — the card offers the
    /// 重新发起 action (#437). Distinct from `Error`: nothing failed, the
    /// message was simply never received.
    Unreceived,
    /// A waiting card collected because a new Turn in the thread superseded it
    /// (ADR-0059, spec #405): header 「⏳ 部分完成 · 已由新消息接管」. Terminal —
    /// the card stops updating — and no Completion Notice follows (the notice
    /// belongs to a true end). The background work is unaffected: a later Wake
    /// continues the newest chain.
    Superseded,
    /// A waiting card collected because its Session stopped being the thread's
    /// Active Session — `/switch` away, `/switch forget` (ADR-0059, spec
    /// #405): header 「⏳ 已切换会话 · 后台任务仍在运行」. Terminal, no
    /// Completion Notice; switching back reports the Session through the
    /// ADR-0028 snapshot, and the background work runs on.
    SwitchedAway,
    /// A persisted live card collected because a new card took the chain over
    /// after a cola restart (ADR-0063): the Wake continuation the restart's
    /// Session Sync posted, or a fresh Turn's card. The orphaned card stops
    /// looking live — header 「⏳ 已由新卡片接管 · 已停止更新」 — so two cards
    /// never both claim the session. Terminal, grey, no Completion Notice and
    /// no recovery action: the successor owns the chain.
    TakenOver,
}

impl CardState {
    /// Whether this state ends the card's lifecycle: no more content arrives,
    /// the header timer stops, and the only actions left are the ending's own
    /// recovery ones (the Error card's retry; the Unreceived card's 重新发起,
    /// spec #434). `Continued` is NOT terminal — the chain continues on a new
    /// card — and neither is `Waiting`: the Turn's Background Tasks are still
    /// live and a Wake will continue its chain on a new card (ADR-0059). The
    /// collected waiting states (`Superseded`, `SwitchedAway`) ARE terminal:
    /// the wait is over even though its background work is not, so the card
    /// stops updating (ADR-0059).
    /// One definition, so a new terminal state (#394's `Stopped`) cannot leave
    /// a probe reading the set differently.
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Done
                | Self::Error
                | Self::Retried
                | Self::Stopped
                | Self::Unreceived
                | Self::Superseded
                | Self::SwitchedAway
                | Self::TakenOver
        )
    }

    /// Whether a live renderer still owns the card's chain: the card is
    /// neither ended nor yielded to its Background Tasks. `Waiting` is NOT
    /// owned in this sense — the Turn that owned it yielded and the next Wake
    /// continues the chain on a new card (ADR-0059) — and neither is a
    /// terminal card. Session Sync's Wake step reads this to decide whether
    /// the chain can be handed over without double-rendering. Distinct from
    /// `CardSession::card_is_live` (the last send reached Feishu) and
    /// `CardSession::is_running` (non-terminal, `Waiting` included).
    pub(crate) fn is_render_owned(&self) -> bool {
        !self.is_terminal() && !matches!(self, Self::Waiting)
    }

    /// Whether this state's own header beats the awaiting-permission/question
    /// override (ADR-0014). A card paused on the operator keeps the override;
    /// every state that is no longer waiting for anyone must show itself
    /// instead — a terminal card (its wait is over: #386's fallback Error,
    /// #394's Stopped) and a Waiting card, which yields for its Background
    /// Tasks rather than for the operator (ADR-0059). One definition, so a new
    /// state cannot leave the header probe reading the set differently.
    pub(crate) fn overrides_awaiting(&self) -> bool {
        self.is_terminal() || matches!(self, Self::Waiting)
    }

    /// Whether this state owns a recovery action: `Error` (the retry that
    /// re-submits the failed prompt, spec #391) and `Unreceived` (重新发起,
    /// #437). The card render, the accumulator's button builder and
    /// [`crate::bridge::turn::Turn::claim_recovery`]'s contract all read this
    /// one predicate, so the set of action-owning states cannot drift.
    pub(crate) fn offers_recovery(&self) -> bool {
        matches!(self, Self::Error | Self::Unreceived)
    }

    /// The ending's word in Session Sync's reap INFO line (ADR-0063): one
    /// match here, so the reap cannot restate the state vocabulary. The five
    /// states the reap stamps — the settle endings and the takeover collect —
    /// have their own words; a state it never stamps falls back to the bare
    /// word rather than inventing a meaning.
    pub(crate) fn reap_word(&self) -> &'static str {
        match self {
            Self::Done => "settled done",
            Self::Error => "settled error",
            Self::Waiting => "settled waiting",
            Self::Unreceived => "ended unreceived",
            Self::TakenOver => "collected the orphaned card",
            _ => "settled",
        }
    }
}

/// The one line a card records a settled failure on (the accumulator's Error
/// card, the reap's reaped ending): the failure's own text, written verbatim
/// after `**错误**: ` and preceded by a blank line, so it reads as its own
/// paragraph under whatever content the card carries.
pub(crate) fn error_line(error: &str) -> String {
    format!("\n**错误**: {error}")
}

/// The one line a settling card records a Session's location move on (#428,
/// #439), [`error_line`]'s sibling: the move named, the new directory in
/// backticks. Built from the session's own directory alone — no chat content.
/// The reap renders it as its own markdown element, so it carries no leading
/// blank line.
pub(crate) fn move_line(directory: &str) -> String {
    format!("**会话已迁移**: `{directory}`")
}

/// How much text ONE card carries before it is finalized and the rest continues
/// on the next card. Kept below Feishu's card limits so a card full of text
/// never overflows; long answers flow across continuation cards instead of a
/// separate plain-text message.
pub const MAX_CARD_TEXT_CHARS: usize = 6000;

/// Estimated component ceiling for a card body. Feishu rejects cards over ~200
/// components (ErrCode 11310); when a streaming card would cross this it is
/// finalized with a "to be continued" marker and a fresh continuation card is
/// sent instead.
pub const MAX_CARD_COMPONENTS: usize = 150;

/// Feishu's documented ceiling for a card's serialized body (the message API
/// rejects cards above it; ~30KB, returned as `230099` / "create universal
/// card fail" 200800). `MAX_CARD_JSON_CHARS` keeps cards under this by a
/// comfortable margin, since the split estimate trails the real serialized
/// size by the un-accounted tail (footer, inline buttons) plus per-element
/// overhead.
pub const FEISHU_CARD_LIMIT_BYTES: usize = 30_000;

/// Estimated JSON size ceiling for a card body. Feishu's documented card limit
/// is [`FEISHU_CARD_LIMIT_BYTES`], and the message API rejects cards far below
/// the old 100KB assumption — a real 44KB card fails with 230099 / "create
/// universal card fail" (200800). A streaming card splits when the estimate
/// crosses this, keeping every card comfortably under the 30KB cap (the
/// estimate trails the serialized size by a few hundred bytes per element, so
/// the margin absorbs it). The 5KB gap to the hard limit covers the tail
/// sections (footer, inline permission/question buttons) the estimate doesn't
/// count.
pub const MAX_CARD_JSON_CHARS: usize = FEISHU_CARD_LIMIT_BYTES - 5_000;

/// Cola's own budget for one markdown element — NOT a Feishu per-element cap.
/// The platform documents no text-length limit per markdown element (a single
/// 8000-char ASCII / 3060-char CJK element renders fully via both create and
/// PATCH); the hard card limits are 30KB total and 200 elements/components
/// (11310). Keeping each element ≤ 3000 chars keeps a card of many elements
/// comfortably inside both. Reasoning, tool input/output and the question are
/// capped per-element on top of this.
pub const MAX_ELEMENT_TEXT_CHARS: usize = 3000;

/// Split `text` into chunks of at most `max` chars (character-aware), keeping
/// the full content. Used to bound single card elements and timeline items.
pub(crate) fn chunk_text(text: &str, max: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let take: String = rest.chars().take(max).collect();
        chunks.push(take.clone());
        rest = &rest[take.len()..];
    }
    chunks
}

/// Which kind of request keeps the turn paused (ADR-0014): the header names
/// the one that is actually pending (permission vs question) instead of
/// lumping both under one title.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AwaitingAction {
    /// Nothing pending: the header keeps its phase label.
    #[default]
    None,
    Permission,
    Question,
    /// A permission and a question are live at the same time.
    Both,
}

impl AwaitingAction {
    /// The header title for this awaiting state, if the turn is paused.
    pub(crate) fn title(self) -> Option<&'static str> {
        match self {
            AwaitingAction::None => None,
            AwaitingAction::Permission => Some(AWAITING_PERMISSION_TITLE),
            AwaitingAction::Question => Some(AWAITING_QUESTION_TITLE),
            AwaitingAction::Both => Some(AWAITING_BOTH_TITLE),
        }
    }

    /// The wait's words without the header's icon: the same vocabulary the
    /// task liveness line reuses (ADR-0054). Kept in lockstep with [`title`]
    /// by a test, so the card can never say two things for one state.
    ///
    /// [`title`]: Self::title
    pub(crate) fn label(self) -> Option<&'static str> {
        match self {
            AwaitingAction::None => None,
            AwaitingAction::Permission => Some("等待你的授权"),
            AwaitingAction::Question => Some("等待你的回答"),
            AwaitingAction::Both => Some("等待你的授权/回答"),
        }
    }
}

/// Progress/liveness signals for the card header (ADR-0014): which request
/// kinds pause the turn, how long the current phase has run, and the reasoning
/// text length. Bundled so they travel through the builder, the header
/// renderer, and the accumulator as one unit instead of three loose values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeaderProgress {
    /// The live request kind(s): the header shows the matching title instead
    /// of the phase label — the turn is paused, not stuck.
    pub awaiting: AwaitingAction,
    /// Seconds the current phase (thinking/reasoning/tool/streaming) has run.
    pub elapsed: Option<u64>,
    /// Reasoning text length, shown on the thinking header as real progress.
    pub reasoning_chars: usize,
}

/// A JSON 2.0 button (tag: `button`, sits directly in `body.elements`).
#[derive(Debug, Clone)]
pub struct CardActionButton {
    pub text: String,
    /// `primary` | `default` | `danger`.
    pub kind: &'static str,
    /// Callback payload delivered to cola on click.
    pub value: serde_json::Value,
}

/// The header titles while a request blocks the turn (ADR-0014). Tests assert
/// these constants so the copy lives in one place.
pub(crate) const AWAITING_PERMISSION_TITLE: &str = "⏳ 等待你的授权";
pub(crate) const AWAITING_QUESTION_TITLE: &str = "⏳ 等待你的回答";
pub(crate) const AWAITING_BOTH_TITLE: &str = "⏳ 等待你的授权/回答";

/// Wrap `text` in a fenced code block so long lines render without wrapping.
/// The fence is sized longer than any backtick run in the content, so an
/// embedded ``` can't break out of the block.
pub(crate) fn fenced_code(text: &str, lang: Option<&str>) -> String {
    let max_run = text
        .chars()
        .fold((0usize, 0usize), |(best, run), c| {
            if c == '`' {
                let run = run + 1;
                (best.max(run), run)
            } else {
                (best, 0)
            }
        })
        .0;
    let fence = "`".repeat((max_run + 1).max(3));
    let head = match lang {
        Some(l) if !l.is_empty() => format!("{fence}{l}"),
        _ => fence.clone(),
    };
    format!("{head}\n{text}\n{fence}")
}

/// A display label for a session title: strips raw Feishu mention tokens
/// (`@_user_N`) and drops meaningless default titles — the `/new`-generated
/// `sess-<uuid>` and the server's `New session - <iso>` / `Child session - <iso>`
/// placeholders (the caller then shows the session ID instead). Used for card
/// subtitles and notification cards.
pub fn clean_session_label(name: &str) -> String {
    let cleaned = crate::feishu::message::strip_mention_tokens(name);
    if (cleaned.starts_with("sess-") && cleaned.len() == 41)
        || cleaned.starts_with("New session - ")
        || cleaned.starts_with("Child session - ")
    {
        String::new()
    } else {
        cleaned
    }
}

/// `HH:MM` in the machine's local time for an epoch-millisecond instant — the
/// panel header suffix (#183) and the permission receipt's clock (ADR-0038
/// rule 4) read the same format. `None` for a value outside chrono's
/// representable range (never a real part key), so callers can skip the suffix.
pub(crate) fn fmt_local_time(epoch_ms: i64) -> Option<String> {
    format_local(epoch_ms, "%H:%M")
}

/// `MM-DD` in the machine's local time — the card header's date anchor
/// (#183), taken from the turn's SERVER time so it is stable across flushes
/// and can't disagree with the panels.
pub(crate) fn fmt_local_date(epoch_ms: i64) -> Option<String> {
    format_local(epoch_ms, "%m-%d")
}

fn format_local(epoch_ms: i64, fmt: &str) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(epoch_ms)
        .map(|at| at.with_timezone(&chrono::Local).format(fmt).to_string())
}

/// Build an epoch-millisecond instant from a local wall time, so tests can
/// assert `HH:MM` / `MM-DD` strings that hold in any machine timezone.
#[cfg(test)]
pub(crate) fn test_local_ms(y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
    use chrono::TimeZone;

    chrono::Local
        .with_ymd_and_hms(y, m, d, h, min, 0)
        .single()
        .expect("unambiguous local time")
        .timestamp_millis()
}

/// Clip `text` to at most `max_len` characters, appending a "…" marker when it
/// was cut. Character-counted so CJK content (3 bytes/char) is truncated at the
/// same visual length as ASCII instead of at a byte budget.
pub(crate) fn truncate_md(text: &str, max_len: usize) -> String {
    if text.chars().count() <= max_len {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max_len).collect::<String>())
    }
}

/// Byte length of the first `n` characters of `s` — what a render that clips
/// at `n` characters contributes to the card's serialized size. Card budgets
/// count UTF-8 bytes (Feishu rejects on bytes, not characters) while every
/// clip ([`truncate_md`]) counts characters, so this is the one conversion
/// between the two units a size estimate may use.
pub(crate) fn first_n_chars_bytes(s: &str, n: usize) -> usize {
    s.chars().take(n).map(|c| c.len_utf8()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one set of states whose header must beat the awaiting override
    /// (ADR-0014/#386, ADR-0059): a terminal card's wait is over and a Waiting
    /// card yields for its Background Tasks. Pinned here so a new state cannot
    /// quietly change the header probe.
    #[test]
    fn only_ended_states_override_the_awaiting_title() {
        for state in [
            CardState::Loading,
            CardState::Reasoning,
            CardState::Streaming,
            CardState::Continued,
        ] {
            assert!(
                !state.overrides_awaiting(),
                "a live card still waits for the operator: {state:?}"
            );
        }
        for state in [
            CardState::Done,
            CardState::Error,
            CardState::Retried,
            CardState::Stopped,
            CardState::Waiting,
            CardState::Unreceived,
            CardState::Superseded,
            CardState::SwitchedAway,
            CardState::TakenOver,
        ] {
            assert!(state.overrides_awaiting(), "{state:?} has its own ending to show");
        }
    }

    /// The reap's INFO line reads its ending word from the state itself
    /// (ADR-0063): each ending the reap stamps has its own word, so the log
    /// vocabulary cannot drift from the card vocabulary.
    #[test]
    fn reap_words_name_each_ending_the_reap_stamps() {
        assert_eq!(CardState::Done.reap_word(), "settled done");
        assert_eq!(CardState::Error.reap_word(), "settled error");
        assert_eq!(CardState::Waiting.reap_word(), "settled waiting");
        assert_eq!(CardState::Unreceived.reap_word(), "ended unreceived");
        assert_eq!(
            CardState::TakenOver.reap_word(),
            "collected the orphaned card",
            "the takeover collect has its own word too"
        );
        assert_eq!(
            CardState::Streaming.reap_word(),
            "settled",
            "a state the reap never stamps keeps the bare word"
        );
    }

    /// The settle card's move line (#439) names the new directory alone — no
    /// chat content can ride a directory formatter.
    #[test]
    fn the_move_line_names_the_directory_alone() {
        assert_eq!(
            move_line("/work/.worktrees/zh-user-guide"),
            "**会话已迁移**: `/work/.worktrees/zh-user-guide`"
        );
    }

    /// The TakenOver collect (ADR-0063) is terminal — the successor owns the
    /// chain, so the orphan stops updating — and never render-owned: Session
    /// Sync must not treat it as a live renderer's card.
    #[test]
    fn the_taken_over_collect_is_terminal_and_not_render_owned() {
        assert!(CardState::TakenOver.is_terminal());
        assert!(!CardState::TakenOver.is_render_owned());
        assert!(!CardState::TakenOver.offers_recovery());
    }

    /// The Unreceived ending (ADR-0062) is terminal — the card stops updating
    /// and its 重新发起 action is the only way forward (#437) — and never
    /// render-owned, so the prompt router and the Wake step see no live chain
    /// over a message nobody will answer.
    #[test]
    fn the_unreceived_ending_is_terminal_and_not_render_owned() {
        assert!(CardState::Unreceived.is_terminal());
        assert!(!CardState::Unreceived.is_render_owned());
        assert!(CardState::Unreceived.overrides_awaiting());
    }

    /// The collected waiting states (ADR-0059) are terminal — the card stops
    /// updating — and never render-owned: a later Wake continues a collected
    /// chain, so Session Sync reading `is_render_owned` must not treat the
    /// collected card as a live renderer's card.
    #[test]
    fn collected_waiting_states_are_terminal_and_not_render_owned() {
        for state in [CardState::Superseded, CardState::SwitchedAway] {
            assert!(state.is_terminal(), "{state:?} collected a finished wait");
            assert!(
                !state.is_render_owned(),
                "{state:?} must not block a Wake continuation"
            );
        }
    }

    /// The liveness line's wait label (ADR-0054) must always be the header
    /// title minus its icon — one vocabulary, two render sites.
    #[test]
    fn awaiting_labels_are_the_header_titles_without_the_icon() {
        for action in [
            AwaitingAction::Permission,
            AwaitingAction::Question,
            AwaitingAction::Both,
        ] {
            assert_eq!(
                action.title().unwrap(),
                format!("⏳ {}", action.label().unwrap()),
                "the wait vocabulary drifted for {action:?}"
            );
        }
        assert_eq!(AwaitingAction::None.label(), None);
    }

    /// #183: both panel headers and the card header date read the same local
    /// clock through these helpers. The expected strings are built from a local
    /// wall time, so they hold on any test-machine timezone.
    #[test]
    fn local_time_helpers_format_the_machine_zone() {
        let at = test_local_ms(2026, 9, 16, 14, 3);
        assert_eq!(fmt_local_time(at).as_deref(), Some("14:03"));
        assert_eq!(fmt_local_date(at).as_deref(), Some("09-16"));
        // Unrepresentable epochs (never a real server key) format to nothing
        // instead of panicking.
        assert_eq!(fmt_local_time(i64::MAX), None);
        assert_eq!(fmt_local_date(i64::MIN), None);
    }

    #[test]
    fn clean_session_label_handles_uuid_and_mentions() {
        // `/new`-generated `sess-<uuid>` names are meaningless → empty label, so
        // the caller shows the session ID instead (never "新会话").
        assert_eq!(
            clean_session_label("sess-7a025fa5-74a1-44e0-b5c5-80b9a21f71bc"),
            ""
        );
        assert_eq!(clean_session_label("@_user_1 你好"), "你好");
        assert_eq!(clean_session_label("frontend-refactor"), "frontend-refactor");
        // A notification card shows the cleaned label, not the raw name.
        let card = notify::build_external_message_card("sess-7a025fa5-74a1-44e0-b5c5-80b9a21f71bc", "hi");
        let text = card.to_string();
        assert!(!text.contains("sess-"), "raw sess-uuid must not leak: {}", text);
    }
}
