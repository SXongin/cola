//! The card-element schema: one module owns how a card element is built and
//! read.
//!
//! Construction goes through [`CardElement`], a newtype over the wire
//! [`serde_json::Value`] whose only constructors are typed — a caller that
//! builds an element through the layer cannot spell a tag by hand. The wire type
//! stays `Value` at every boundary (tests, PATCH paths, the shared test
//! walkers), and [`CardElement::into_value`] / `From` is the one way back to it.
//!
//! Reading goes through [`ElementTag`] and the accessors below, so a probe on a
//! card's shape names the element it means instead of a string literal. A
//! card-wide assertion walks through `bridge::test_support::card_tags`, the
//! shared typed walker built on [`ElementTag`].
//!
//! The *card* envelope — schema 2.0, the header's title/template/subtitle
//! layout — is not an element and stays in [`super::shell`].

use serde_json::{Value, json};

use super::vocab;

/// The `tag` a card element carries on the JSON 2.0 wire. `PlainText` is also
/// the tag of a plain-text *field* (a header, button, panel or image title):
/// the platform uses one shape for both, so one variant names both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElementTag {
    Markdown,
    PlainText,
    Button,
    Panel,
    ColumnSet,
    Input,
    Form,
    Hr,
    Action,
    Image,
}

impl ElementTag {
    /// The wire tag's string.
    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::PlainText => "plain_text",
            Self::Button => "button",
            Self::Panel => "collapsible_panel",
            Self::ColumnSet => "column_set",
            Self::Input => "input",
            Self::Form => "form",
            Self::Hr => "hr",
            Self::Action => "action",
            Self::Image => "img",
        }
    }

    /// The tag a wire value names, or `None` for a value that is not an
    /// element (or carries no tag).
    pub(crate) fn from_wire(tag: &str) -> Option<Self> {
        Some(match tag {
            "markdown" => Self::Markdown,
            "plain_text" => Self::PlainText,
            "button" => Self::Button,
            "collapsible_panel" => Self::Panel,
            "column_set" => Self::ColumnSet,
            "input" => Self::Input,
            "form" => Self::Form,
            "hr" => Self::Hr,
            "action" => Self::Action,
            "img" => Self::Image,
            _ => return None,
        })
    }
}

/// A card element under construction: one of the shapes the card writes.
/// Construct it with a typed constructor and hand it to a builder as `Value`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CardElement(Value);

impl CardElement {
    /// A markdown body element — the card's text carrier.
    pub(crate) fn markdown(content: impl Into<String>) -> Self {
        Self(json!({ "tag": ElementTag::Markdown.wire(), "content": content.into() }))
    }

    /// A plain-text node: an element in its own right, or the title field of a
    /// header, button, panel or image.
    pub(crate) fn plain_text(content: impl Into<String>) -> Self {
        Self(json!({ "tag": ElementTag::PlainText.wire(), "content": content.into() }))
    }

    /// A horizontal rule separating sections.
    pub(crate) fn hr() -> Self {
        Self(json!({ "tag": ElementTag::Hr.wire() }))
    }

    /// A recovery/action button; `kind` is `primary`, `default` or `danger`,
    /// and `value` is the callback payload the click replays.
    pub(crate) fn button(text: impl Into<String>, kind: &str, value: Value) -> Self {
        Self(json!({
            "tag": ElementTag::Button.wire(),
            "text": { "tag": ElementTag::PlainText.wire(), "content": text.into() },
            "type": kind,
            "value": value,
        }))
    }

    /// A collapsible panel, folded by default. `element_id` is the panel's
    /// stable identity for the duration of the card (see
    /// [`super::shell::collapsible_panel_chunks`] for why that matters).
    pub(crate) fn panel(title: impl Into<String>, elements: Vec<Value>, element_id: Option<&str>) -> Self {
        let mut panel = json!({
            "tag": ElementTag::Panel.wire(),
            "expanded": false,
            "header": {
                "title": { "tag": ElementTag::PlainText.wire(), "content": title.into() },
                "icon": { "tag": "standard_icon", "token": "down-small-ccm_outlined" },
                "icon_position": "right"
            },
            "elements": elements,
        });
        if let Some(id) = element_id {
            panel["element_id"] = json!(id);
        }
        Self(panel)
    }

    /// The wire value, for a caller handing the element to a JSON builder.
    pub(crate) fn into_value(self) -> Value {
        self.0
    }
}

impl From<CardElement> for Value {
    fn from(element: CardElement) -> Self {
        element.0
    }
}

/// The tag `value` names, if any.
pub(crate) fn tag(value: &Value) -> Option<ElementTag> {
    value
        .get("tag")
        .and_then(Value::as_str)
        .and_then(ElementTag::from_wire)
}

/// Whether `value` is an element of `tag`.
pub(crate) fn is(value: &Value, tag: ElementTag) -> bool {
    value
        .get("tag")
        .and_then(Value::as_str)
        .and_then(ElementTag::from_wire)
        == Some(tag)
}

/// Whether `element` is a running tool panel as a whole-card read returns it
/// (ADR-0068): a [`ElementTag::Panel`] whose plain-text title begins with the
/// running glyph. The Background Task Ledger shares the glyph and is matched by
/// its element id before this is asked.
pub(crate) fn is_running_panel(element: &Value) -> bool {
    is(element, ElementTag::Panel)
        && tag(&element["header"]["title"]) == Some(ElementTag::PlainText)
        && element["header"]["title"]["content"]
            .as_str()
            .is_some_and(|title| title.starts_with(vocab::RUNNING_ICON))
}
