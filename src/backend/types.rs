//! The shared DTOs of the backend contract (spec #860, #870).
//!
//! They carry the request and response shapes the `Backend` trait speaks —
//! sessions, permissions, questions, providers, agents, models — so they live
//! beside the trait instead of inside one adapter. A second adapter is written
//! against these names (ADR-0090); neutralising them is decision 7 of that ADR
//! and is deliberately *not* done here: this module is the relocation, not a
//! reshape.
//!
//! Today every type mirrors the OpenCode server's JSON contract; camelCase
//! fields carry `#[serde(rename)]` (AGENTS.md pitfall `wire-shapes`).
//! Request-body builders and payload parsing live with the generation strategy
//! (`opencode::v1`), never here.
//!
//! These DTOs are the documented out-of-scope exception to the generation seam
//! (spec #332): a strategy decodes its generation's payloads into them, so a
//! few generation-flavoured shapes (the session-list entry, the permission
//! request's V1 field names, the status fan-out) still surface here. The typed
//! form model ([`QuestionRequest`]/[`QuestionInfo`]) is genuinely neutral: the
//! V1 strategy maps its positional questions onto it and the V2 strategy
//! decodes its `Form.Field` union into it, so the card layer never learns a
//! generation (spec #364, S6). Generation-private wire structs live with the
//! strategy instead (the V2 read model's landed in `opencode::v2::wire`, spec
//! #364 S4a) and convert into these neutral shapes.

#![allow(dead_code)] // protocol types — field coverage matches server contract

use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct CreateSessionResponse {
    pub data: Session,
}

/// One entry from the canonical `GET /session` list — `Session.Info` with the
/// fields the discovery commands need. Server JSON uses camelCase; serde
/// renames are required (AGENTS.md pitfall `wire-shapes`).
#[derive(Debug, Clone, Deserialize)]
pub struct SessionListInfo {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub directory: String,
    #[serde(rename = "parentID", default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub model: Option<serde_json::Value>,
    #[serde(default)]
    pub time: Option<SessionTime>,
}

impl SessionListInfo {
    /// Sub-task child sessions (created by the `task` tool) keep a `parentID`
    /// and are excluded from `/switch` auto-adoption and the `/switch` card.
    pub fn is_child(&self) -> bool {
        self.parent_id.is_some()
    }

    /// Whether this session is a direct child of `parent_id` (`parentID`
    /// equals it) — the scoped lookup behind `/sub` and the `/sub attach`
    /// takeover.
    pub fn is_child_of(&self, parent_id: &str) -> bool {
        self.parent_id.as_deref() == Some(parent_id)
    }

    /// Whether the session is archived (`time.archived` set). A missing `time`
    /// reads as not archived, like every session surface that excludes them.
    pub fn is_archived(&self) -> bool {
        self.time.as_ref().is_some_and(|t| t.is_archived())
    }
}

/// An available agent (`GET /agent`, `Agent.Info`): `name`, `mode`
/// (`primary`/`subagent`/`all`), optional `description`. Used by the `/agent`
/// card to offer a picker.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub hidden: Option<bool>,
}

impl AgentInfo {
    /// The server's default agent: the first primary, visible agent in
    /// `GET /agent` order. The server's own agent service sorts the configured
    /// default (or `build`) first, so this is its `defaultInfo` fallback
    /// without a `/config` round trip — an ordering the live V2 selection
    /// chain validates against a real server whose `default_agent` is a custom
    /// agent. `None` when the server listed nothing usable.
    pub fn default_agent(agents: &[AgentInfo]) -> Option<String> {
        agents
            .iter()
            .find(|a| a.mode.as_deref() != Some("subagent") && a.hidden != Some(true))
            .map(|a| a.name.clone())
    }
}

/// A provider's available models (`GET /provider`), rendered as one `/model`
/// card row per provider. Each model carries its declared variants (the
/// thinking-level options `/think` surfaces).
#[derive(Debug, Clone)]
pub struct ProviderModels {
    pub provider: String,
    pub models: Vec<ModelOption>,
}

/// One selectable option under a provider or a model scope.
#[derive(Debug, Clone, Default)]
pub struct ModelOption {
    /// The option's identity in its scope — a model id on a model page, a
    /// provider id on a provider page.
    pub id: String,
    /// The provider this option applies **together with** its model, when the
    /// client declares such a pair (Codex's `[profiles.*]` preset carries
    /// `model` + `model_provider`). `None` means "the model only" — the pick
    /// leaves the thread's provider as it is (issue #910).
    pub provider: Option<String>,
    /// The model's declared variants (e.g. `["low", "medium", "high"]`),
    /// empty when it declares none. `GET /provider` serializes each model's
    /// variants as a Record keyed by variant id (`model.variants` →
    /// `{"high": {...}}`); the names are its keys. There is no universal
    /// scale — each model declares its own set (ADR-0020).
    pub variants: Vec<String>,
    /// The button label, when it differs from [`Self::id`] (a display name
    /// or a clipped row). `None` shows `id`.
    pub label: Option<String>,
    /// The callback value a pick sends, when it differs from [`Self::id`].
    /// `None` sends `id` — OpenCode's provider page sends the provider id,
    /// its model page sends `provider/model`.
    pub value: Option<String>,
    /// The section an option belongs to inside one scope (#908's grouped
    /// provider scope: the built-in catalog must not render under a
    /// provider). `None` leaves the option ungrouped.
    pub group: Option<String>,
}

impl ModelOption {
    /// A plain option whose label and callback value are its `id`.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Self::default()
        }
    }

    /// The label the card shows.
    pub fn display_label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.id)
    }

    /// The callback value a pick sends.
    pub fn callback_value(&self) -> &str {
        self.value.as_deref().unwrap_or(&self.id)
    }
}

/// What a pick on a scope's options does when the bridge receives it.
///
/// It is stated by the surface rather than inferred from the scope id: Codex's
/// `provider` scope is a **selection** (a thread-level provider), while
/// OpenCode's is **navigation** (drill into that provider's models), and both
/// are legitimately named `provider` (issues #908, #910). The card carries the
/// distinction as the button's callback `level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PickKind {
    /// A concrete model ref (`provider/model`) for the next turn — the default,
    /// and every scope whose options are model refs.
    #[default]
    Model,
    /// Another surface page: the bridge asks the client for the drilled
    /// surface, naming this scope and the tapped option (OpenCode's provider →
    /// model flow).
    DrillDown,
    /// A thread-level provider id (Codex's `modelProvider`): applied when a
    /// thread is created, or by resuming it — never per turn.
    Provider,
}

/// What one axis of a client's model surface can offer, and where it may
/// change. A scope is one titled section of the `/model` card: a provider
/// list (OpenCode), a provider's models, or Codex's flat catalog.
#[derive(Debug, Clone, Default)]
pub struct ModelScope {
    /// The scope's identity: the drill-down key a card sends back and the
    /// callback `level` of its options where they are model picks
    /// (`"provider"` / `"model"` / `"catalog"` …). [`Self::pick`] decides what
    /// a click does, never this name.
    pub id: String,
    /// The section's title, also the card's header when it is the only scope.
    pub title: String,
    /// Where a pick on this scope takes effect.
    pub changeable: ChangeScope,
    /// What a pick on this scope does — navigation or a selection — so the
    /// bridge routes it without reading a scope's name as a command.
    pub pick: PickKind,
    /// The options, possibly empty.
    pub options: Vec<ModelOption>,
    /// A markdown note shown with the scope (why a list is empty, what a
    /// failed probe means). `None` renders nothing.
    pub note: Option<String>,
    /// A hand-write hint shown beside the scope (e.g. `codex/<id>`).
    pub free_text: Option<String>,
    /// The instruction line above the options (e.g. `**选择 provider**（共
    /// 3 个）：`). Kept as the client's own wording so the rendered card can
    /// stay byte-identical to the hand-built one it replaces.
    pub intro: Option<String>,
}

/// Where a model pick lands on a session (issue #910). A client whose provider
/// is a **per-prompt** fact (OpenCode sends `provider/model` with each message)
/// answers [`Self::PerTurn`] for every pick; a client whose provider is a
/// **thread-level** fact (Codex's `modelProvider`) answers
/// [`Self::ThreadModel`] when the pick would change the thread's provider —
/// the thread must then be re-opened before the model can be sent on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModelPickRoute {
    /// The pick rides the next turn: nothing thread-level happens.
    #[default]
    PerTurn,
    /// The pick changes a thread-level fact, so the thread must be switched
    /// (Codex: release the subscription, resume with the pair, read back).
    ThreadModel,
}

/// What making a thread run a model/provider pair concluded (issue #910).
///
/// Every variant that claims success is decided by a read-back of **both**
/// halves — the provider alone is not the pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThreadModelChange {
    /// The thread already ran the exact pair: no thread-level request was
    /// needed.
    #[default]
    Unchanged,
    /// The thread now runs the pair, verified by a read-back.
    Changed,
    /// The thread was already on this provider and only the model differs: no
    /// thread-level request is made (a same-provider model change is the
    /// per-turn path), and the model rides the next turn.
    ModelNextTurn,
}

/// Where changing a model selection takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChangeScope {
    /// The next turn picks it up (Codex's `turn/start.model`).
    #[default]
    ThisTurn,
    /// Only a newly started thread can use it.
    NewThread,
    /// Only resuming a thread can change it.
    ResumeOnly,
}

/// The running model selection for a session: the display label and the wire
/// reference a pick would record.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelSelection {
    /// `provider/model@variant`, what the card's "current model" line shows.
    pub label: String,
    /// `provider/model`, the wire reference a pick records.
    pub reference: String,
}

/// What a client can offer on `/model`, and where each axis may change.
#[derive(Debug, Clone, Default)]
pub struct ModelSurface {
    /// The running selection, shown as the "current model" line. `None` when
    /// no rung of the caller's ladder resolves (or this page omits it).
    pub current: Option<ModelSelection>,
    /// The page's scopes, each rendered as one titled section.
    pub scopes: Vec<ModelScope>,
}

/// The context a [`ModelSurface`] read is made in. The session identity is
/// part of the request because `current` is a session fact and a client may
/// need the thread's provider (#908); nothing is read from global state.
#[derive(Debug, Clone, Default)]
pub struct ModelSurfaceQuery<'a> {
    /// The thread's target session (`None` for a Pending Session).
    pub session_id: Option<&'a str>,
    /// The directory the session is rooted at (a location-scoped read).
    pub directory: Option<&'a str>,
    /// The running selection the caller resolved (its settings mirror →
    /// configured default → server-recorded ladder). A backend that knows a
    /// stronger session fact may replace it; the bridge passes it so the
    /// card's current line keeps its one source of truth.
    pub current: Option<ModelSelection>,
    /// The scope the card has drilled into (`None` = the root page).
    pub scope: Option<&'a str>,
    /// The option tapped on the parent page that selected [`Self::scope`]
    /// (OpenCode: the provider id).
    pub selected: Option<&'a str>,
    /// Ask for the bare catalog only, with no config read and no provider
    /// probe. Set **only** by `/think`'s context-free variant lookup, which runs
    /// on a card callback's short ack budget; every card page — a Pending
    /// Session's included — leaves it false.
    pub catalog_only: bool,
    /// When this read answers a card callback, that callback's total budget
    /// (Feishu's 2 s ack budget). An adapter must fit callback-originated
    /// network work inside it; `None` means the read is not bounded by an ack —
    /// a command's page, or a callback's **validation** read whose completeness
    /// matters more than its toast (the handler keeps running and still records
    /// the outcome). OpenCode ignores it; Codex derives its config-read and
    /// catalog slices from it and serves probe answers from cache only.
    pub ack_budget: Option<Duration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    #[serde(rename = "projectID")]
    pub project_id: Option<String>,
    pub agent: Option<String>,
    pub title: Option<String>,
    pub location: Option<serde_json::Value>,
    pub cost: Option<f64>,
    pub time: Option<SessionTime>,
}

/// Minimal `Session.Info` (canonical `GET /session/{id}`) — enough to resolve
/// a sub-task session's parent, and the session's server-recorded model (the
/// last rung of the `/think` effective-model resolution).
#[derive(Debug, Clone, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(rename = "parentID")]
    pub parent_id: Option<String>,
    /// The server-managed session title (what OpenChamber shows).
    #[serde(default)]
    pub title: Option<String>,
    /// The model the server has recorded for the session
    /// (`Session.Info.model`: `{id, providerID, variant?}`).
    #[serde(default)]
    pub model: Option<SessionModel>,
}

/// The model reference on a session (`Session.Info.model`).
#[derive(Debug, Clone, Deserialize)]
pub struct SessionModel {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    pub id: String,
}

/// A session's durable model/agent selection — V2's session-scoped switches
/// (`Session.Info.model` + `Session.Info.agent`). On a generation that keeps
/// the selection server-side the next turn uses it with nothing re-sent, and
/// every client sharing the store sees the same pick; a generation whose
/// selection rides each prompt (V1) reports `None` instead.
#[derive(Debug, Clone, Default)]
pub struct SessionSelection {
    /// The selected model ref (`Model.Ref`, variant inside it).
    pub model: Option<ModelInfo>,
    /// The selected agent id, when the session has one recorded.
    pub agent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTime {
    pub created: i64,
    pub updated: i64,
    /// When set, the session is archived (`time.archived`, a timestamp).
    #[serde(default)]
    pub archived: Option<i64>,
}

impl SessionTime {
    pub fn is_archived(&self) -> bool {
        self.archived.is_some()
    }
}

#[derive(Debug, Serialize)]
pub struct CreateSessionInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelInfo>,
    /// The thread-level provider the new session must start on, when a client
    /// has that axis (Codex's `thread/start.modelProvider`). It is paired with
    /// [`Self::model`] — a Pending Session's provider pick rides the thread's
    /// creation together with the model chosen beside it (issue #910). `None`
    /// means the client's configured default; a client without the axis
    /// (OpenCode) never sets it, so its create body is byte-identical.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Location {
    pub directory: String,
}

/// One inbound platform attachment sent with a prompt — a Feishu image or file
/// (GLOSSARY: Prompt Attachment; ADR-0076's inbound amendment). `mime` is the
/// media type the server will read it as (the four image mimes, `application/pdf`,
/// or a text file normalized to `text/plain`), `data_base64` its bytes, and
/// `name` its display name — `None` for an image, which has none. Each
/// generation serializes it into its own payload: V1 a `{type:"file", mime,
/// filename?, url:"data:<mime>;base64,…"}` part, V2 a `{uri, name?}` `files[]`
/// entry. An image requires a vision-capable model; an unsupported one surfaces
/// an error.
#[derive(Debug, Clone)]
pub struct PromptFile {
    pub mime: String,
    pub data_base64: String,
    pub name: Option<String>,
}

/// The typed kind of a form field (V2 `Form.Field`). V1 questions map onto
/// [`Self::String`] and [`Self::Multiselect`]; the other kinds exist for V2
/// forms and render as their own controls (spec #364, S6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FormFieldKind {
    #[default]
    String,
    Number,
    Integer,
    Boolean,
    Multiselect,
    External,
}

/// One selectable option of a form field (or a V1 question).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QuestionOption {
    /// The value submitted when this option is chosen. V1 options carry no
    /// separate value, so an empty `value` reads as the label (the V2 question
    /// tool builds both from the label); the display always uses `label`.
    pub value: String,
    pub label: String,
    pub description: String,
}

impl QuestionOption {
    /// The answer value this option submits (the label when no value is set).
    pub fn answer_value(&self) -> &str {
        if self.value.is_empty() {
            &self.label
        } else {
            &self.value
        }
    }
}

/// One field of a pending form (V2) / question (V1): a typed input with an
/// answer key. V1 has no keys or types — its questions become `String` or
/// `Multiselect` fields keyed positionally (`q0`, `q1`, …) at reply time.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QuestionInfo {
    /// The answer key (V2 `Form.Field.key`). Empty synthesizes `q{index}` for
    /// the V1 questions, which carry no key.
    pub key: String,
    /// The full question/description text.
    pub question: String,
    /// The short header/title shown first.
    pub header: String,
    pub kind: FormFieldKind,
    pub options: Vec<QuestionOption>,
    /// Whether a free-text answer is allowed (default true, V1's `custom`).
    pub custom: Option<bool>,
    pub required: bool,
    /// The link of an `external` field (not answerable; acknowledged).
    pub url: Option<String>,
}

impl QuestionInfo {
    /// Whether this field's answer is a list of values (V2 `multiselect`).
    pub fn is_multi(&self) -> bool {
        self.kind == FormFieldKind::Multiselect
    }

    /// Whether a custom (typed) answer may be given.
    pub fn custom_allowed(&self) -> bool {
        self.custom.unwrap_or(true)
    }

    /// The display title: the header when set, else the full question.
    pub fn title(&self) -> &str {
        if self.header.is_empty() {
            &self.question
        } else {
            &self.header
        }
    }

    /// The display labels for submitted answer values: an option's label when
    /// the value names one of its values, else the raw value collapsed to one
    /// line (a custom answer, kept verbatim on the wire but never allowed to
    /// break the card's markdown).
    pub fn display_values(&self, values: &[String]) -> Vec<String> {
        values
            .iter()
            .map(|value| {
                self.options
                    .iter()
                    .find(|option| option.answer_value() == value)
                    .map(|option| option.label.clone())
                    .unwrap_or_else(|| value.split_whitespace().collect::<Vec<_>>().join(" "))
            })
            .collect()
    }
}

/// One keyed answer of a form reply (V2 `Form.Answer`'s value union).
#[derive(Debug, Clone, PartialEq)]
pub enum FormValue {
    Text(String),
    Number(f64),
    /// An `integer`-kind field's answer, kept integral so the wire value is a
    /// JSON integer (never `3.0`) — V2 validates `Number.isInteger`.
    Integer(i64),
    Bool(bool),
    List(Vec<String>),
}

impl FormValue {
    /// The positional string form V1's `answers: string[][]` carries (and the
    /// test recordings use). The reverse of a V2 keyed answer is not needed:
    /// V2 decodes its own keys.
    pub fn to_strings(&self) -> Vec<String> {
        match self {
            FormValue::Text(text) => vec![text.clone()],
            FormValue::Number(number) => vec![number.to_string()],
            FormValue::Integer(integer) => vec![integer.to_string()],
            FormValue::Bool(flag) => vec![flag.to_string()],
            FormValue::List(values) => values.clone(),
        }
    }
}

/// One keyed form answer, in field order. `value` is `None` for a field the
/// user never finalized: V1 keeps its positional slot (`answers: [[]]`) and V2
/// omits the key (the server applies its own `required` rule).
#[derive(Debug, Clone, PartialEq)]
pub struct FormAnswer {
    pub key: String,
    pub value: Option<FormValue>,
}

/// Build the keyed answers a form reply carries from the positional string
/// answers the card accumulates, one entry per field in order. `answers[i]` is
/// `None` until field `i` is finalized (a confirmed multi-select may be
/// `Some(vec![])` — "不选"); a numeric field whose typed text cannot parse (or a
/// non-integral value in an `integer` field) is left unanswered — never sent as
/// a value the server's schema would reject.
pub fn build_form_answers(questions: &[QuestionInfo], answers: &[Option<Vec<String>>]) -> Vec<FormAnswer> {
    questions
        .iter()
        .enumerate()
        .map(|(index, field)| FormAnswer {
            key: if field.key.is_empty() {
                format!("q{index}")
            } else {
                field.key.clone()
            },
            value: answers
                .get(index)
                .and_then(|slot| slot.as_ref())
                .and_then(|values| field_answer(field, values)),
        })
        .collect()
}

/// `i64::MIN` as an f64 (`-2^63`): the inclusive lower bound for narrowing a
/// whole f64 into [`FormValue::Integer`].
const INTEGER_MIN_F64: f64 = -9_223_372_036_854_775_808.0;
/// The exclusive upper bound for that narrowing: `2^63` (one past `i64::MAX`),
/// the first f64 beyond the i64 range. A plain `<= i64::MAX as f64` would admit
/// `2^63` itself (both round to the same f64), which `as i64` saturates rather
/// than represents.
const INTEGER_LIMIT_F64: f64 = 9_223_372_036_854_775_808.0;

/// Decode one finalized field's typed answer, or `None` to leave the field
/// unanswered (V2 omits the key; V1 keeps an empty positional slot).
fn field_answer(field: &QuestionInfo, values: &[String]) -> Option<FormValue> {
    match field.kind {
        // External fields are acknowledged (not answered): a `true` answer
        // records the acknowledgement.
        FormFieldKind::External => values
            .iter()
            .any(|value| value == "true")
            .then_some(FormValue::Bool(true)),
        FormFieldKind::Multiselect => Some(FormValue::List(values.to_vec())),
        FormFieldKind::Boolean => Some(FormValue::Bool(values.first().is_some_and(|v| v == "true"))),
        // A numeric answer must be a finite JSON number; `NaN`/`inf` parse as
        // f64 but cannot be represented on the wire.
        FormFieldKind::Number => values
            .first()?
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite())
            .map(FormValue::Number),
        // An integer answer must also be whole: `3.0` is accepted and narrowed
        // to `3`, a fractional value is invalid and leaves the field open.
        FormFieldKind::Integer => values
            .first()?
            .parse::<f64>()
            .ok()
            .filter(|number| {
                number.is_finite()
                    && number.fract() == 0.0
                    // Reject anything outside the i64 range: an `as i64` cast
                    // SATURATES (`1e30` would become `i64::MAX`), which would
                    // submit a wrong answer instead of reporting the field as
                    // unanswered.
                    && (INTEGER_MIN_F64..INTEGER_LIMIT_F64).contains(number)
            })
            .map(|number| FormValue::Integer(number as i64)),
        FormFieldKind::String => {
            let text = values.first().cloned().unwrap_or_default();
            if text.is_empty() && field.required {
                // A required string must not be submitted empty; leave it
                // unanswered so the server reports the missing field.
                None
            } else {
                Some(FormValue::Text(text))
            }
        }
    }
}

/// Why a question request cannot be collected on a card at all. The neutral
/// shape carries only the reason; each surface renders its own copy (the
/// Feishu card owns the user-facing wording).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UncollectableReason {
    /// The answer is secret; a shared card would expose it, and cola has no
    /// private input channel.
    SecretInput,
}

/// A pending question (V1) / form (V2) request: the AI asks the user one or
/// more typed fields and blocks until answered or cancelled.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QuestionRequest {
    pub id: String,
    pub session_id: String,
    /// The form's title (V2 `Form.Info.title`); empty for V1 questions.
    pub title: String,
    pub questions: Vec<QuestionInfo>,
    /// Set when the request has no answerable fields: the surface must show
    /// the reason and offer only a decline. The neutral shape carries the
    /// reason, never the user-facing copy.
    pub uncollectable: Option<UncollectableReason>,
}

impl QuestionRequest {
    /// A request cola cannot safely collect at all — a secret input, which
    /// would be visible on a shared card. The shape is a request with no
    /// fields and a named reason, so every surface can show its own copy and
    /// only a reject control; the reject path declines the request.
    pub fn uncollectable(session_id: &str) -> Self {
        Self {
            id: String::new(),
            session_id: session_id.to_string(),
            title: String::new(),
            questions: Vec::new(),
            uncollectable: Some(UncollectableReason::SecretInput),
        }
    }
}

/// A session's run state as reported by the server (`GET /session/status`).
/// cola consumes the server's own status service (ADR-0028): idle/busy/retry
/// per session. A read this client cannot interpret is never guessed — it
/// becomes `None` and the caller omits whatever depends on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    /// The server has no running turn for the session (`{type:"idle"}`, or the
    /// session is absent from the status map — a finished run is removed).
    Idle,
    /// A turn is actively running (`{type:"busy"}`).
    Busy,
    /// The last turn failed and is scheduled for retry (`{type:"retry", …}`).
    Retry,
}

impl SessionStatus {
    /// Whether the server reports a LIVE run for the session: `Busy` (a turn
    /// is running) or `Retry` (the last turn failed and is scheduled to run
    /// again) — `Idle` is the only non-live status. This is ADR-0059's "live
    /// Execution" fact, and every busy read in the Bridge and the cards goes
    /// through it, so the predicate is spelled once.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Busy | Self::Retry)
    }
}

/// The character budget for one rendered permission pattern. A request whose
/// rendered target would exceed it is refused before a card is offered, so the
/// user approves exactly what the card shows. Neutral presentation contract:
/// the card renderer and every adapter that can raise an approval share it.
pub const PERMISSION_PATTERN_MAX_CHARS: usize = 150;

/// The character budget for one rendered **command** target.
///
/// A command approval's target is the command itself, not a short path, so the
/// pattern budget above would truncate ordinary invocations (`cargo test
/// --workspace --all-targets` is already over half of it) — and a target the
/// card clips is a target the user approves without seeing. The budget is
/// therefore larger, and still small enough that the card's own limits hold:
/// the permission body is rendered once and copied into every button's
/// callback value, so it appears six times in the card's JSON (one markdown
/// element plus five buttons); 1,000 characters of the worst case (CJK, three
/// bytes each) is 18 KB of a 25 KB body budget. See
/// `permission_target_budget` for the one place that maps an action to it, and
/// the card tests that pin the rendered card inside
/// [`crate::feishu::card::MAX_CARD_JSON_CHARS`].
///
/// A request must never be offered for approval unless every target fits:
/// [`PermissionRequest::targets_are_fully_shown`] is the proof, and the
/// permission flow declines what fails it with a named reason card rather than
/// showing part of what a button would approve.
pub const PERMISSION_COMMAND_MAX_CHARS: usize = 1000;

/// Whether `action` names a shell command, i.e. a target whose text IS the
/// decision rather than a short path or grant name. The spellings are the
/// neutral surface's own: both OpenCode generations' shell tool and Codex's
/// `command` approval.
pub fn is_command_action(action: &str) -> bool {
    matches!(action, "bash" | "shell" | "command")
}

/// The character budget for one rendered target of `action`: the command budget
/// where the target is a command ([`is_command_action`]), the pattern budget
/// everywhere else.
///
/// One definition, shared by the renderer (which clips) and by the permission
/// flow's completeness proof (`PermissionRequest::targets_are_fully_shown`,
/// which refuses what the renderer would clip), so the two can never disagree
/// about what "shown in full" means.
pub fn permission_target_budget(action: &str) -> usize {
    if is_command_action(action) {
        PERMISSION_COMMAND_MAX_CHARS
    } else {
        PERMISSION_PATTERN_MAX_CHARS
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PermissionRequest {
    #[serde(rename = "id")]
    pub request_id: String,
    #[serde(rename = "sessionID")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub permission: Option<String>,
    #[serde(default)]
    pub patterns: Vec<String>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub always: Vec<String>,
    /// Whether the backend advertised a **session-scoped** "always allow" for
    /// this request. The card offers the 始终允许 button (and says what the
    /// scope means) only then. Defaults to `true`: every OpenCode generation
    /// offers its always rule, and a backend whose protocol advertises the
    /// decision only for some request kinds sets it per request. The neutral
    /// model names the decision by its scope, never by a client's wire value.
    #[serde(default = "always_supported_default")]
    pub always_supported: bool,
}

fn always_supported_default() -> bool {
    true
}

impl PermissionRequest {
    /// The character budget every target of this request is rendered at: the
    /// command budget for a command target, the short pattern budget otherwise
    /// (see [`permission_target_budget`]).
    pub fn target_budget(&self) -> usize {
        permission_target_budget(self.permission.as_deref().unwrap_or_default())
    }

    /// Whether the card renders **every** target in full at
    /// [`Self::target_budget`].
    ///
    /// `false` means the card would have to clip one, so the request must never
    /// be offered for approval: the buttons approve what the card shows, and a
    /// clipped command would approve a suffix the user never saw. Every client
    /// is covered here rather than in its adapter — the card is the shared
    /// decision surface, so the completeness proof belongs to the same layer as
    /// the renderer that clips.
    pub fn targets_are_fully_shown(&self) -> bool {
        let budget = self.target_budget();
        self.patterns
            .iter()
            .all(|pattern| pattern.chars().count() <= budget)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one liveness predicate (ADR-0059's "live Execution"): Idle is the
    /// only status that is not a live run; Busy and Retry are.
    #[test]
    fn session_status_is_live_is_busy_or_retry() {
        assert!(!SessionStatus::Idle.is_live());
        assert!(SessionStatus::Busy.is_live());
        assert!(SessionStatus::Retry.is_live());
    }

    #[test]
    fn parses_session_list_info_camelcase_fixture() {
        // A real `GET /session` entry (Session.Info, camelCase). ParentID/time
        // must map onto the serde-renamed fields (AGENTS.md pitfall `wire-shapes`).
        let json = r#"{
            "id": "ses_alpha01",
            "slug": "alpha",
            "projectID": "proj_x",
            "directory": "/work/cola",
            "parentID": "ses_parent",
            "title": "重写登录模块",
            "agent": "build",
            "model": {"id": "deepseek-v4-flash", "providerID": "opencode-go"},
            "time": {"created": 1700000000000, "updated": 1700000100000},
            "version": "1"
        }"#;
        let info: SessionListInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.id, "ses_alpha01");
        assert_eq!(info.title, "重写登录模块");
        assert_eq!(info.directory, "/work/cola");
        assert_eq!(info.parent_id.as_deref(), Some("ses_parent"));
        assert_eq!(info.agent.as_deref(), Some("build"));
        assert_eq!(info.time.as_ref().unwrap().created, 1700000000000);
        assert_eq!(info.time.as_ref().unwrap().updated, 1700000100000);
        assert!(info.is_child());
        assert!(info.is_child_of("ses_parent"));
        assert!(!info.is_archived());
    }

    #[test]
    fn child_session_list_info_is_child() {
        let info: SessionListInfo = serde_json::from_str(
            r#"{"id":"ses_child","title":"Child session - x","directory":"/w",
                "parentID":"ses_parent","time":{"created":1,"updated":2}}"#,
        )
        .unwrap();
        assert!(info.is_child());
        assert!(info.is_child_of("ses_parent"));
        assert!(!info.is_child_of("ses_other"));
        assert_eq!(info.parent_id.as_deref(), Some("ses_parent"));
    }

    #[test]
    fn archived_session_time_reports_archived() {
        let info: SessionListInfo = serde_json::from_str(
            r#"{"id":"ses_a","title":"t","directory":"/w",
                "time":{"created":1,"updated":2,"archived":3}}"#,
        )
        .unwrap();
        assert!(info.time.as_ref().unwrap().is_archived());
        assert!(info.is_archived());
    }

    /// A missing `time` (or one without `archived`) reads as not archived,
    /// which the session surfaces rely on when they exclude archived entries.
    #[test]
    fn missing_time_is_not_archived() {
        let info: SessionListInfo =
            serde_json::from_str(r#"{"id":"ses_a","title":"t","directory":"/w"}"#).unwrap();
        assert!(!info.is_archived());
        assert!(!info.is_child());
    }

    fn field(key: &str, kind: FormFieldKind) -> QuestionInfo {
        QuestionInfo {
            key: key.into(),
            kind,
            ..Default::default()
        }
    }

    /// The typed answer decode: a whole value in an `integer` field narrows to
    /// an `Integer` (never a float), a fractional one is invalid and leaves the
    /// field unanswered, and a required empty string is omitted.
    #[test]
    fn form_answers_decode_integers_and_reject_invalid_numbers() {
        let questions = vec![
            field("count", FormFieldKind::Integer),
            field("ratio", FormFieldKind::Number),
            QuestionInfo {
                key: "required".into(),
                kind: FormFieldKind::String,
                required: true,
                ..Default::default()
            },
        ];
        let slots = vec![
            Some(vec!["3".to_string()]),
            Some(vec!["2.5".to_string()]),
            Some(Vec::new()),
        ];
        let decoded = build_form_answers(&questions, &slots);
        assert_eq!(decoded[0].value, Some(FormValue::Integer(3)));
        assert_eq!(decoded[1].value, Some(FormValue::Number(2.5)));
        assert_eq!(decoded[2].value, None, "a required empty string is omitted");
        // The positional wire form keeps the integer integral.
        assert_eq!(decoded[0].value.as_ref().unwrap().to_strings(), vec!["3"]);

        // `3.0` is whole and narrows; `2.5` (or a garbled/NaN value) is not.
        assert_eq!(
            field_answer(&field("i", FormFieldKind::Integer), &["3.0".into()]),
            Some(FormValue::Integer(3))
        );
        assert_eq!(
            field_answer(&field("i", FormFieldKind::Integer), &["2.5".into()]),
            None
        );
        assert_eq!(
            field_answer(&field("i", FormFieldKind::Integer), &["many".into()]),
            None
        );
        assert_eq!(
            field_answer(&field("n", FormFieldKind::Number), &["NaN".into()]),
            None
        );
        // Whole but out-of-range values must not saturate into a wrong answer:
        // `1e30 as i64` would clamp to `i64::MAX`.
        assert_eq!(
            field_answer(&field("i", FormFieldKind::Integer), &["1e30".into()]),
            None
        );
        assert_eq!(
            field_answer(&field("i", FormFieldKind::Integer), &["-1e30".into()]),
            None
        );
        // `2^63` is one past the i64 range (the exclusive upper bound).
        assert_eq!(
            field_answer(
                &field("i", FormFieldKind::Integer),
                &["9223372036854775808".into()]
            ),
            None
        );
        // A large in-range value still narrows.
        assert_eq!(
            field_answer(&field("i", FormFieldKind::Integer), &["9007199254740992".into()]),
            Some(FormValue::Integer(9_007_199_254_740_992))
        );
        // An unanswered slot stays unanswered in field order.
        let unanswered = build_form_answers(&questions, &[None, None, None]);
        assert_eq!(unanswered.len(), 3);
        assert!(unanswered.iter().all(|answer| answer.value.is_none()));
    }

    /// V1 questions carry no key: the neutral decode synthesizes the positional
    /// `q{i}` key, and an external acknowledgement is the `true` answer.
    #[test]
    fn form_answers_synthesize_positional_keys_and_acknowledge_external_fields() {
        let questions = vec![
            field("", FormFieldKind::String),
            field("link", FormFieldKind::External),
        ];
        let decoded = build_form_answers(&questions, &[Some(vec!["/a".into()]), Some(vec!["true".into()])]);
        assert_eq!(decoded[0].key, "q0");
        assert_eq!(decoded[0].value, Some(FormValue::Text("/a".into())));
        assert_eq!(decoded[1].key, "link");
        assert_eq!(decoded[1].value, Some(FormValue::Bool(true)));
    }
}
