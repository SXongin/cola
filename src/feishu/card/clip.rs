//! The one clipping entry point.
//!
//! [`clip`] returns a [`Clip`] handle over the text; the handle's method names
//! the operation and the [`SurfaceBudget`] names the surface, so every clip in
//! the card reads the same way:
//!
//! ```
//! clip(text).cut(budget::CARD_ELEMENT);            // one element's head clip
//! clip(text).split(budget::CARD_ELEMENT);          // element-sized chunks
//! clip(text).exceeds(budget::CARD_ELEMENT);        // the clip's own predicate
//! clip(text).cut_fenced(Some("rust"), budget::DIFF_FENCED); // escaped bytes
//! ```
//!
//! There is no other public clip: the algorithms live behind it (the
//! construct-aware markdown walker is [`super::sanitize::split_construct_aware`];
//! the escaped-byte walkers are [`super::tool_render::fenced_body_head`] and
//! [`super::tool_render::fenced_body_tail`]).

use super::budget::{CutRule, SplitRule, SurfaceBudget};

/// Start a clip over `text`.
pub(crate) fn clip(text: &str) -> Clip<'_> {
    Clip { text }
}

/// A pending clip: pick the operation, then the surface's budget.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Clip<'a> {
    text: &'a str,
}

impl<'a> Clip<'a> {
    /// Trim `text` to `budget`'s cap under its cut rule. A fenced budget with
    /// no language token clips through [`Self::cut_fenced`] with `None`.
    pub(crate) fn cut(self, budget: SurfaceBudget) -> String {
        match budget.cut {
            CutRule::Marked => {
                if self.exceeds(budget) {
                    format!("{}…", self.take(budget.max))
                } else {
                    self.text.to_string()
                }
            }
            // A bare clip and a marked one differ only in the marker, and a
            // body inside the budget is kept whole either way.
            CutRule::Bare => self.take(budget.max),
            CutRule::FencedBytes | CutRule::FencedBytesTail => self.cut_fenced(None, budget),
        }
    }

    /// [`Self::cut`] for a fenced body, charging the opening fence's `lang`
    /// token against the budget. The one form a caller with a language uses.
    pub(crate) fn cut_fenced(self, lang: Option<&str>, budget: SurfaceBudget) -> String {
        match budget.cut {
            CutRule::FencedBytes => super::tool_render::fenced_body_head(self.text, lang, budget.max),
            CutRule::FencedBytesTail => super::tool_render::fenced_body_tail(self.text, lang, budget.max),
            // A char-counted budget has no fence to charge; its rule is the
            // same either way.
            CutRule::Marked | CutRule::Bare => self.cut(budget),
        }
    }

    /// Split `text` into chunks of at most `budget`'s cap under its split rule.
    pub(crate) fn split(self, budget: SurfaceBudget) -> Vec<String> {
        match budget.split {
            SplitRule::Markdown => super::sanitize::split_construct_aware(self.text, budget.max),
            SplitRule::Plain => self.split_plain(budget.max),
        }
    }

    /// Whether [`Self::cut`] would cut `text`: the exact character comparison
    /// the clip makes, so a caller that must know whether a rendered body was
    /// ACTUALLY clipped — a `read` panel's truncation note (ADR-0083 §8
    /// amendment, #770) — decides on exactly the case the `…` marks.
    pub(crate) fn exceeds(self, budget: SurfaceBudget) -> bool {
        self.text.chars().count() > budget.max
    }

    fn take(self, max: usize) -> String {
        self.text.chars().take(max).collect()
    }

    /// The fence-blind plain split: full content, chunk boundaries on the
    /// character count. The caller fences each chunk itself.
    fn split_plain(self, max: usize) -> Vec<String> {
        if self.text.is_empty() {
            return Vec::new();
        }
        // Clamp like the markdown walker: a zero budget (a `…less(n)` surface)
        // must still advance one character per chunk, never spin.
        let max = max.max(1);
        let mut chunks = Vec::new();
        let mut rest = self.text;
        while !rest.is_empty() {
            let take: String = rest.chars().take(max).collect();
            chunks.push(take.clone());
            rest = &rest[take.len()..];
        }
        chunks
    }
}
