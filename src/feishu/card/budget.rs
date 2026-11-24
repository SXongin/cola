//! The card's text budgets as data.
//!
//! Every clip asks for its surface's [`SurfaceBudget`] — one value naming the
//! cap AND the rule the clip follows — instead of passing a bare number, so the
//! call site reads `clip(text).cut(budget::CARD_ELEMENT)` and the number, the
//! unit and the rule live here.
//!
//! ## These are cola's budgets, not Feishu's
//!
//! [`CARD_ELEMENT`] is cola's own budget for one markdown element — NOT a
//! Feishu per-element cap. The platform documents no text-length limit per
//! markdown element (a single 8000-char ASCII / 3060-char CJK element renders
//! fully via both create and PATCH); the hard card limits are 30KB total and
//! 200 elements/components (11310). Keeping each element ≤ 3000 chars keeps a
//! card of many elements comfortably inside both. Reasoning, tool input/output
//! and the question are capped per-element on top of this.
//!
//! A markdown surface's split must stay construct-aware (#1096): the platform
//! parses each element on its own, so a fence or table crossing the budget goes
//! through [`super::clip::Clip::split`]'s markdown rule, which closes the
//! construct on one side and re-opens it on the other.
//!
//! Card-wide **ceilings** are not surface budgets and stay plain constants in
//! [`super`]: their unit is the whole card (bytes or components), not one
//! surface's text.

use super::MAX_CARD_TEXT_CHARS;

/// How [`super::clip::Clip::cut`] trims a surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CutRule {
    /// A head clip counted in characters, ending on the `…` marker.
    Marked,
    /// A head clip counted in characters with NO marker — for a fenced body
    /// whose note below the fence already says it was clipped (`…` immediately
    /// before a closing fence stops Feishu rendering the block as code at all,
    /// #788).
    Bare,
    /// A head clip budgeted in the JSON-escaped BYTES the element actually
    /// carries, charging the fence and the opening fence's language token.
    FencedBytes,
    /// [`Self::FencedBytes`] from the END: the newest bytes survive, because a
    /// command's last line is its diagnostic.
    FencedBytesTail,
}

/// How [`super::clip::Clip::split`] divides a surface that must span elements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SplitRule {
    /// Each element is self-contained markdown: a fence or table crossing the
    /// cut is closed on one element and re-opened on the next (#1096).
    Markdown,
    /// A plain character split, fence-blind — only for text that is fenced or
    /// escaped per chunk by its caller.
    Plain,
}

/// One card surface's text budget: the cap, the unit its `cut` counts, and the
/// rule its split follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SurfaceBudget {
    /// Characters, or JSON-escaped bytes for a fenced surface.
    pub(crate) max: usize,
    pub(crate) cut: CutRule,
    pub(crate) split: SplitRule,
}

impl SurfaceBudget {
    /// A markdown/narrative surface: character-counted, `…` on a cut,
    /// construct-aware when split.
    pub(crate) const fn chars(max: usize) -> Self {
        Self {
            max,
            cut: CutRule::Marked,
            split: SplitRule::Markdown,
        }
    }

    /// A fenced body budgeted in JSON-escaped bytes, kept from the head.
    pub(crate) const fn fenced(max: usize) -> Self {
        Self {
            max,
            cut: CutRule::FencedBytes,
            split: SplitRule::Markdown,
        }
    }

    /// [`Self::fenced`] keeping the tail.
    pub(crate) const fn fenced_tail(max: usize) -> Self {
        Self {
            max,
            cut: CutRule::FencedBytesTail,
            split: SplitRule::Markdown,
        }
    }

    /// A plain character split, for a caller that fences each chunk itself.
    pub(crate) const fn plain(max: usize) -> Self {
        Self {
            max,
            cut: CutRule::Marked,
            split: SplitRule::Plain,
        }
    }

    /// The same budget, clipped bare (no `…`), for a fenced body whose own note
    /// carries the truncation signal.
    pub(crate) const fn bare(self) -> Self {
        Self {
            cut: CutRule::Bare,
            ..self
        }
    }

    /// The same budget with `n` fewer units: a body that must share its element
    /// with a prefix, a note or the bytes already written.
    pub(crate) const fn less(self, n: usize) -> Self {
        Self {
            max: self.max.saturating_sub(n),
            ..self
        }
    }

    /// The budget left for a body that renders under `prefix` and its newline.
    pub(crate) fn under_prefix(self, prefix: &str) -> Self {
        self.less(prefix.len() + 1)
    }
}

/// One markdown element (cola's own 3,000-character cap; see the module docs).
pub(crate) const CARD_ELEMENT: SurfaceBudget = SurfaceBudget::chars(3_000);

/// One timeline entry inside a card. Equal to the card-wide text ceiling today:
/// the entry is what the whole-card split measures against, so the two numbers
/// are one decision with two readers.
pub(crate) const TIMELINE_ENTRY: SurfaceBudget = SurfaceBudget::chars(MAX_CARD_TEXT_CHARS);

/// A Tool Panel's output/input body, character-counted with the `…` marker.
pub(crate) const TOOL_PANEL: SurfaceBudget = SurfaceBudget::chars(3_000);

/// A Tool Panel's fenced element, budgeted in JSON-escaped bytes under a
/// prefix (see [`SurfaceBudget::under_prefix`]).
pub(crate) const TOOL_PANEL_FENCED: SurfaceBudget = SurfaceBudget::fenced(3_000);

/// [`TOOL_PANEL_FENCED`] keeping the tail.
pub(crate) const TOOL_PANEL_FENCED_TAIL: SurfaceBudget = SurfaceBudget::fenced_tail(3_000);

/// A file-editing panel's diff body: escaped bytes, not characters, because a
/// real diff routinely exceeds 3,000 characters and a panel cannot be split
/// (the whole card is bounded by [`super::MAX_CARD_JSON_CHARS`]). Roughly half
/// the card budget leaves room for the panel's input, header, button and note.
pub(crate) const DIFF_FENCED: SurfaceBudget = SurfaceBudget::fenced(12_000);

/// A Tool Panel's Input line: a multi-line input is summarized rather than
/// flooding the card (a `Program`'s source is the one exception, because it IS
/// the panel).
pub(crate) const TOOL_INPUT: SurfaceBudget = SurfaceBudget::chars(400);

/// One reasoning entry's element: the model text plus the `…` marker (#561).
/// The Rendered Cursor's delivered extent for a reasoning part is clamped to it
/// — a frontier must never claim characters the card never showed.
pub(crate) const REASONING_ELEMENT: SurfaceBudget = SurfaceBudget::chars(800);

/// A session title's element. The server's `title` agent caps its own output at
/// 100 characters; matching that keeps every legal title verbatim while still
/// guarding the card against a title that did not pass through that cap.
pub(crate) const CARD_TITLE: SurfaceBudget = SurfaceBudget::chars(100);

/// The whole-card fenced fallback's plain chunk: the caller fences each chunk
/// itself, so the split is fence-blind.
pub(crate) const PLAIN_ELEMENT: SurfaceBudget = SurfaceBudget::plain(3_000);
