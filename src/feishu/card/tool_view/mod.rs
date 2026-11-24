//! One owner for a tool's identity → view (#1135, charter C5).
//!
//! A tool call's identity is its neutral `(kind, detail)`; a permission
//! request's identity is the action name its adapter reported. Both surfaces
//! classify here, so the Tool Panel ([`super::tool_render`]) and the permission
//! card (`bridge::request::kind`) cannot drift apart about what a tool *is*:
//! the panel's Input line, Output body, body charge, truncation note and folded
//! header details all come from one [`ToolView`], and the permission card's
//! icon, sentence and whether the request's diff is the request come from one
//! [`permission_view`] table whose entries name the same [`ToolFamily`].
//!
//! A new **vendor** tool that maps to an existing neutral identity (ADR-0105's
//! whole point) needs no card change at all: the adapter declares the shape and
//! both surfaces inherit it. A new **identity** is one [`ToolView`] variant
//! with its body builders — and, when a permission can name it, one
//! `PERMISSION_VIEWS` row beside it.
//!
//! This module deliberately holds no card JSON and no layout: the folded panel
//! element, the tracking lines and the `img`/button elements stay in
//! `tool_render`. What lives here is the per-tool knowledge the ticket names —
//! `is_command`, `has_file_edits`, `file_edits_*`, `read_detail`,
//! `search_detail`, `TaskLiveness`, `awaiting_action` — plus the dispatch that
//! used to branch per tool in `tool_render` and `kind`.

mod files;
mod liveness;

#[cfg(test)]
mod tests;

use std::borrow::Cow;

use crate::backend::{
    DelegationEffect, FileAction, FileEdit, NestedCall, SearchScope, ToolDetail, ToolKind, ToolStatus,
    read_output_is_directory,
};

use super::budget;
use super::clip;
use super::tool_render::{
    BodyStyle, PREVIEW_TRUNCATION_NOTE, SEARCH_TRUNCATION_NOTE, ToolPanel, assembled_body_estimate,
    call_output_charge, codex_collab_output, codex_web_search_output, collab_agent_input, command_input,
    delegation_input, delegation_output, diff_truncation_note, fetch_input, format_generic_input,
    header_file_name, json_escaped_len, multi_file_detail, plan_input, plan_output, program_input,
    program_output, question_input, read_input, read_output, search_input, skill_input, skill_output,
    web_output,
};

pub(crate) use files::{file_edits_body, file_edits_input, file_edits_output};
pub(crate) use liveness::{ChildActivity, TaskLiveness};

/// The Output body a panel renders: a short markdown header, the body's style
/// and the body itself. The panel's render and its size estimate both derive
/// from this one value, so they cannot charge a body the panel never writes.
pub(crate) type ToolBody = (Option<String>, BodyStyle, String);

/// The neutral tool family both surfaces classify. A permission-only action
/// (`mkdir`, `kill`) has no panel family; a call is always exactly one of the
/// panel families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolFamily {
    Command,
    Program,
    FileEdits,
    Read,
    ImageRead,
    Search,
    Fetch,
    Plan,
    Skill,
    Ask,
    Launch,
    CollabLaunch,
    Vendor,
}

/// A shell-like command's shape (the `Execute` family's `Command` half).
pub(crate) struct CommandShape<'a> {
    pub(crate) command: &'a str,
    pub(crate) cwd: Option<&'a str>,
    pub(crate) description: Option<&'a str>,
    pub(crate) exit: Option<i64>,
    pub(crate) timeout: bool,
}

/// A Code Mode program (the `Execute` family's `Program` half).
pub(crate) struct ProgramShape<'a> {
    pub(crate) source: &'a str,
    pub(crate) language: Option<&'a str>,
    pub(crate) calls: &'a [NestedCall],
    pub(crate) error: bool,
}

/// A file or directory read. Keyed on `(kind, detail)`, so `read` and a future
/// client's read reach the same view (ADR-0105).
pub(crate) struct ReadShape<'a> {
    pub(crate) path: Option<&'a str>,
    pub(crate) directory: bool,
}

/// A filename, content or web search. The scope picks the word and the body
/// parser; the vendor name is never read.
pub(crate) struct SearchShape<'a> {
    pub(crate) query: Option<&'a str>,
    pub(crate) matches: Option<u64>,
    pub(crate) scope: SearchScope,
}

/// One call's identity, with the shape data its view needs.
pub(crate) enum ToolView<'a> {
    Command(CommandShape<'a>),
    Program(ProgramShape<'a>),
    FileEdits(Cow<'a, [FileEdit]>),
    Read(ReadShape<'a>),
    ImageRead,
    Search(SearchShape<'a>),
    Fetch,
    Plan,
    Skill,
    Ask,
    CollabLaunch,
    Launch,
    Vendor,
}

impl<'a> ToolView<'a> {
    /// The ONE classification of a call. The order is the precedence the
    /// surfaces had before #1135: `Execute` first, then the file-edit family,
    /// the plan carrier, an image read (a `Read` detail on any kind, which is
    /// why it precedes the text read), the text read, search, fetch, a
    /// delegation launch (a Codex collaboration launch is a launch with the
    /// adapter's richer input shape), then the envelope details and the opaque
    /// fallback.
    pub(super) fn of(tool: &'a ToolPanel) -> Self {
        let call = tool.call();
        match (call.kind, &call.detail) {
            (
                ToolKind::Execute,
                ToolDetail::Command {
                    command,
                    cwd,
                    description,
                    exit,
                    timeout,
                },
            ) => Self::Command(CommandShape {
                command,
                cwd: cwd.as_deref(),
                description: description.as_deref(),
                exit: *exit,
                timeout: *timeout,
            }),
            (
                ToolKind::Execute,
                ToolDetail::Program {
                    source,
                    language,
                    calls,
                    error,
                },
            ) => Self::Program(ProgramShape {
                source,
                language: language.as_deref(),
                calls,
                error: *error,
            }),
            (ToolKind::Edit | ToolKind::Delete | ToolKind::Move, ToolDetail::FileEdits(edits))
                if !edits.is_empty() =>
            {
                Self::FileEdits(files::file_edits(edits))
            }
            (_, ToolDetail::Plan) => Self::Plan,
            (_, ToolDetail::Read { image: true, .. }) => Self::ImageRead,
            (ToolKind::Read, ToolDetail::Read { path, directory, .. }) => Self::Read(ReadShape {
                path: path.as_deref(),
                directory: *directory,
            }),
            (
                ToolKind::Search,
                ToolDetail::Search {
                    query,
                    matches,
                    scope,
                },
            ) => Self::Search(SearchShape {
                query: query.as_deref(),
                matches: *matches,
                scope: *scope,
            }),
            (ToolKind::Fetch, _) => Self::Fetch,
            (
                _,
                ToolDetail::Delegation {
                    effect: DelegationEffect::Launch,
                    ..
                },
            ) => {
                if collab_agent_shape(tool.input()) {
                    Self::CollabLaunch
                } else {
                    Self::Launch
                }
            }
            (_, ToolDetail::Skill) => Self::Skill,
            (_, ToolDetail::Ask) => Self::Ask,
            _ => Self::Vendor,
        }
    }

    pub(crate) fn family(&self) -> ToolFamily {
        match self {
            Self::Command(_) => ToolFamily::Command,
            Self::Program(_) => ToolFamily::Program,
            Self::FileEdits(_) => ToolFamily::FileEdits,
            Self::Read(_) => ToolFamily::Read,
            Self::ImageRead => ToolFamily::ImageRead,
            Self::Search(_) => ToolFamily::Search,
            Self::Fetch => ToolFamily::Fetch,
            Self::Plan => ToolFamily::Plan,
            Self::Skill => ToolFamily::Skill,
            Self::Ask => ToolFamily::Ask,
            Self::CollabLaunch => ToolFamily::CollabLaunch,
            Self::Launch => ToolFamily::Launch,
            Self::Vendor => ToolFamily::Vendor,
        }
    }

    pub(crate) fn is_command(&self) -> bool {
        matches!(self.family(), ToolFamily::Command)
    }

    pub(crate) fn is_program(&self) -> bool {
        matches!(self.family(), ToolFamily::Program)
    }

    pub(crate) fn has_file_edits(&self) -> bool {
        matches!(self.family(), ToolFamily::FileEdits)
    }

    pub(crate) fn is_plan(&self) -> bool {
        matches!(self.family(), ToolFamily::Plan)
    }

    pub(crate) fn is_collab_launch(&self) -> bool {
        matches!(self.family(), ToolFamily::CollabLaunch)
    }

    /// Whether the call settles as a FAILURE by a fact the call's own shape
    /// records, even though the protocol's status settled `completed`: a
    /// `Command` whose exit is non-zero or which timed out, and a `Program`
    /// that reported an error — its own, or a nested call's. The typed status
    /// stays the protocol's and is never rewritten.
    pub(super) fn detail_failure(&self, tool: &ToolPanel) -> bool {
        if tool.status() != &ToolStatus::Completed {
            return false;
        }
        match self {
            Self::Command(shape) => shape.timeout || shape.exit.is_some_and(|exit| exit != 0),
            Self::Program(shape) => shape.error || shape.calls.iter().any(|call| call.status.is_failure()),
            _ => false,
        }
    }

    /// Whether the call settled as a failure by either rule its status icon
    /// draws: the protocol's [`ToolStatus::is_failure`], or a recorded fact in
    /// the shape ([`Self::detail_failure`]).
    pub(super) fn failed(&self, tool: &ToolPanel) -> bool {
        self.detail_failure(tool) || tool.status().is_failure()
    }

    /// The structured path a 「📂 打开」 button would open (ADR-0083 §8): a
    /// FileEdits call's one file — a rename's destination, a delete nothing at
    /// all, several no single target — or a FILE text `read`, whose panel is a
    /// preview. Two shapes are excluded because they already reach the reader
    /// whole: a read carrying a File Content (ADR-0076) and a DIRECTORY read.
    pub(super) fn open_path(&self, tool: &ToolPanel) -> Option<String> {
        match self {
            Self::FileEdits(edits) => {
                let [only] = edits.as_ref() else {
                    return None;
                };
                if only.action == FileAction::Delete {
                    return None;
                }
                let path = files::file_edit_display_path(only);
                (!path.trim().is_empty()).then(|| path.to_string())
            }
            Self::Read(read) => {
                if read.directory || !tool.files().is_empty() {
                    return None;
                }
                let path = read.path.filter(|path| !path.trim().is_empty())?;
                // A still-running read has no directory verdict yet: `?` leaves
                // it buttonless until it renders one.
                if read_output_is_directory(&tool.output()?) {
                    return None;
                }
                Some(path.to_string())
            }
            _ => None,
        }
    }

    /// The file a file-touching panel is about, as its FOLDED header names it
    /// (ADR-0083 §8 amendment, #766). A multi-file list names its count and
    /// totals instead (#793), and a read whose output carries a File Content
    /// names nothing (the `📎` marker already does, ADR-0076).
    pub(super) fn file_detail(&self, tool: &ToolPanel) -> Option<String> {
        match self {
            Self::FileEdits(edits) => {
                // A failed call's change is unknown, so `+0 −0` would claim
                // nothing changed; the count still stands.
                if edits.len() > 1 {
                    let totals = (!self.failed(tool)).then(|| files::file_edits_totals(edits));
                    return Some(multi_file_detail(edits.len(), totals));
                }
                Some(format!(" · {}", files::file_edit_title(edits.first()?)))
            }
            Self::Read(read) => {
                if !tool.files().is_empty() {
                    return None;
                }
                let path = read.path.filter(|path| !path.trim().is_empty())?;
                // A directory read may name its path with a trailing separator;
                // dropping it lets the short-name rule reach the directory's own
                // name. A path that is nothing BUT separators keeps its form.
                let trimmed = path.trim_end_matches(['/', '\\']);
                let name = if trimmed.is_empty() { path } else { trimmed };
                Some(format!(" · 📖 {}", header_file_name(name)))
            }
            _ => None,
        }
    }

    /// The target agents a collaboration panel's folded title names (issue
    /// #962): the first row's thread plus how many other targets the call
    /// covers.
    pub(super) fn target_detail(&self, tool: &ToolPanel) -> Option<String> {
        if !self.is_collab_launch() {
            return None;
        }
        let obj = tool.input()?.as_object()?;
        let agents = obj.get("agents")?.as_array()?;
        let first = agents.first()?;
        let label = collab_agent_label(first)?;
        let count = agents.len();
        let mut detail = format!(" · 🧵 {label}");
        if count > 1 {
            detail.push_str(&format!(" +{}", count - 1));
        }
        Some(detail)
    }

    /// The result-count header V2 records in a search call's metadata.
    pub(super) fn search_header(&self, tool: &ToolPanel) -> Option<String> {
        let Self::Search(search) = self else {
            return None;
        };
        let (count, unit) = match search.scope {
            SearchScope::FileName => (search.matches?, "个文件"),
            SearchScope::Content => (search.matches?, "处匹配"),
            SearchScope::Web => return None,
        };
        let truncated = tool
            .metadata()
            .and_then(|metadata| metadata.get("truncated"))
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        Some(if truncated {
            format!("{count} {unit} · 已截断")
        } else {
            format!("{count} {unit}")
        })
    }

    /// The note a panel appends under a body its budget clipped: a verified
    /// preview's way-in note, a FileEdits diff's bare clip signal, or a Codex
    /// search's raw-payload note. One function, because the render and the size
    /// estimate must agree on exactly one note (a panel never carries two: its
    /// body is one shape).
    pub(super) fn note(&self, tool: &ToolPanel, session_id: Option<&str>) -> Option<&'static str> {
        match self {
            Self::Read(_) => self.preview_truncation_note(tool, session_id),
            Self::FileEdits(_) => diff_truncation_note(tool),
            Self::Search(search) if search.scope == SearchScope::Web => codex_search_truncation_note(tool),
            _ => None,
        }
    }

    /// The note a file `read` panel appends under a clipped preview: it renders
    /// IFF the way-in button does, so it never promises a control an unopenable
    /// panel does not have.
    fn preview_truncation_note(&self, tool: &ToolPanel, session_id: Option<&str>) -> Option<&'static str> {
        // The button first: a panel without one (a directory read, an image
        // read, a routeless panel) leaves without assembling the body at all.
        self.open_path(tool)?;
        session_id.filter(|id| !id.is_empty())?;
        self.preview_body_clipped(tool).then_some(PREVIEW_TRUNCATION_NOTE)
    }

    /// Whether a `read` panel's rendered body is clipped by its own budget —
    /// measured from the same body the render shows, so the note renders
    /// exactly when the body clipped.
    fn preview_body_clipped(&self, tool: &ToolPanel) -> bool {
        let Self::Read(_) = self else {
            return false;
        };
        tool.output()
            .map(|raw| read_output(&raw).2)
            .is_some_and(|body| clip::clip(&body).exceeds(budget::TOOL_PANEL))
    }

    /// The panel's Input line. A declared shape owns its line, the empty one
    /// included: an empty line renders no section, and it never falls through
    /// to the raw payload's key table. Only the details whose ENVELOPE is
    /// vendor-specific and has no shared shape (`skill`, Codex's collaboration
    /// rows) keep their own renderer, and they are still selected by the
    /// neutral detail.
    pub(super) fn input(&self, tool: &ToolPanel) -> String {
        match self {
            Self::Command(shape) => command_input(shape),
            Self::Program(shape) => program_input(shape),
            Self::FileEdits(edits) => file_edits_input(edits, self.failed(tool)),
            Self::Plan => tool.input().map(plan_input).unwrap_or_default(),
            Self::ImageRead => tool.input().map(image_input).unwrap_or_default(),
            Self::Read(read) => read_input(tool, read),
            Self::Search(search) => search_input(tool, search),
            Self::Fetch => fetch_input(tool.input()),
            Self::CollabLaunch => tool.input().map(collab_agent_input).unwrap_or_default(),
            Self::Launch => tool.input().map(delegation_input).unwrap_or_default(),
            Self::Skill => tool.input().map(skill_input).unwrap_or_default(),
            Self::Ask => tool.input().map(question_input).unwrap_or_default(),
            Self::Vendor => tool.input().map(format_generic_input).unwrap_or_default(),
        }
    }

    /// The Output body the panel renders. An identity whose body is assembled
    /// from its shape falls back exactly as the pre-#1135 chain did, so the
    /// characterization pin sees the same bytes.
    pub(super) fn output(&self, tool: &ToolPanel) -> Option<ToolBody> {
        match self {
            Self::Program(shape) => program_output(tool, shape),
            Self::FileEdits(_) => {
                file_edits_output(tool).or_else(|| tool.output().map(|raw| self.format_output(&raw)))
            }
            Self::CollabLaunch => {
                codex_collab_output(tool).or_else(|| tool.output().map(|raw| delegation_output(&raw)))
            }
            Self::Search(search) if search.scope == SearchScope::Web => {
                codex_web_search_output(tool).or_else(|| tool.output().map(|raw| self.format_output(&raw)))
            }
            _ => tool.output().map(|raw| self.format_output(&raw)),
        }
    }

    /// The body an adapter-assembled shape renders from the call's output text:
    /// the shape-keyed formatter the panel applies where no shape assembles a
    /// body of its own.
    pub(super) fn format_output(&self, output: &str) -> ToolBody {
        match self {
            Self::Plan => plan_output(output),
            Self::Read(_) => read_output(output),
            Self::Search(search) if search.scope == SearchScope::Web => web_output(output),
            Self::Launch => delegation_output(output),
            Self::Skill => skill_output(output),
            // A `Command`'s body IS its output text (the shared command block
            // owns clipping and fencing); an image view shows its output raw;
            // every opaque shape passes through unchanged.
            Self::Command(_)
            | Self::Program(_)
            | Self::FileEdits(_)
            | Self::ImageRead
            | Self::Search(_)
            | Self::Fetch
            | Self::CollabLaunch
            | Self::Ask
            | Self::Vendor => (None, BodyStyle::Auto, output.to_string()),
        }
    }

    /// The card bytes the Output body contributes (#788), derived from the same
    /// body the panel writes: a Program's assembled rows, a FileEdits diff's
    /// composed body clipped at the diff byte budget, a Codex search's or
    /// collaboration call's assembled body, else the output text under the
    /// preview cap. Every body is measured JSON-escaped, and a diff is charged
    /// under the SAME cap it renders under (#792).
    pub(super) fn output_estimate(&self, tool: &ToolPanel) -> usize {
        match self {
            Self::Program(shape) => program_output(tool, shape)
                .map(|(_, style, body)| assembled_body_estimate(None, style, &body))
                .unwrap_or(0),
            Self::FileEdits(_) => match file_edits_body(tool) {
                Some(body) => {
                    let header = body
                        .header
                        .as_deref()
                        .map(|header| {
                            json_escaped_len(
                                &crate::feishu::card::sanitize::CardMarkdown::new().element(header),
                            )
                        })
                        .unwrap_or(0);
                    header + crate::feishu::card::sanitize::serialized_len(&body.render().0)
                }
                None => call_output_charge(tool),
            },
            Self::CollabLaunch => match codex_collab_output(tool) {
                Some((header, style, body)) => assembled_body_estimate(header.as_deref(), style, &body),
                None => call_output_charge(tool),
            },
            Self::Search(search) if search.scope == SearchScope::Web => match codex_web_search_output(tool) {
                Some((header, style, body)) => assembled_body_estimate(header.as_deref(), style, &body),
                None => call_output_charge(tool),
            },
            _ => call_output_charge(tool),
        }
    }
}

/// A permission request's icon, sentence and whether its metadata diff IS the
/// request. One row per action name the two generations report, aliases
/// included; `family` is the panel identity the same tool classifies as, so a
/// fixture can prove both surfaces name one tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PermissionView {
    pub(crate) family: Option<ToolFamily>,
    pub(crate) icon: &'static str,
    pub(crate) label: &'static str,
    /// An edit/patch/apply_patch request carries its unified diff, and that IS
    /// the request, so `describe_permission` shows the changed lines. `false`
    /// for `write`, whose panel is the FileEdits family but whose permission
    /// body has always rendered its scaffolding.
    pub(crate) renders_diff_body: bool,
}

const fn view(
    family: Option<ToolFamily>,
    icon: &'static str,
    label: &'static str,
    renders_diff_body: bool,
) -> PermissionView {
    PermissionView {
        family,
        icon,
        label,
        renders_diff_body,
    }
}

/// The permission surface's ONE registration table. Order is the pre-#1135
/// `describe_action` order (kept so nothing observable changes); an action
/// outside it renders the generic `🔐 执行操作`.
const PERMISSION_VIEWS: &[(&str, PermissionView)] = &[
    (
        "bash",
        view(Some(ToolFamily::Command), "⚡", "执行 Shell 命令", false),
    ),
    (
        "shell",
        view(Some(ToolFamily::Command), "⚡", "执行 Shell 命令", false),
    ),
    (
        "command",
        view(Some(ToolFamily::Command), "⚡", "执行 Shell 命令", false),
    ),
    ("permissions", view(None, "🔑", "申请额外权限", false)),
    (
        "subagent",
        view(Some(ToolFamily::Launch), "🤖", "调用子代理", false),
    ),
    ("task", view(Some(ToolFamily::Launch), "🤖", "调用子代理", false)),
    ("question", view(Some(ToolFamily::Ask), "❓", "向你提问", false)),
    ("read", view(Some(ToolFamily::Read), "📖", "读取文件", false)),
    (
        "write",
        view(Some(ToolFamily::FileEdits), "✏️", "修改文件", false),
    ),
    ("edit", view(Some(ToolFamily::FileEdits), "✏️", "编辑文件", true)),
    ("patch", view(Some(ToolFamily::FileEdits), "✏️", "应用补丁", true)),
    (
        "apply_patch",
        view(Some(ToolFamily::FileEdits), "🔐", "执行操作", true),
    ),
    ("webfetch", view(Some(ToolFamily::Fetch), "🌐", "访问网页", false)),
    (
        "fetch",
        view(Some(ToolFamily::Fetch), "🌐", "获取网络资源", false),
    ),
    (
        "websearch",
        view(Some(ToolFamily::Search), "🌐", "网络搜索", false),
    ),
    ("grep", view(Some(ToolFamily::Search), "🔎", "搜索内容", false)),
    ("glob", view(Some(ToolFamily::Search), "🔍", "查找文件", false)),
    ("skill", view(Some(ToolFamily::Skill), "🧩", "调用技能", false)),
    ("external_directory", view(None, "📁", "访问外部目录", false)),
    ("kill", view(None, "🛑", "终止进程", false)),
    ("ls", view(None, "📂", "列出目录", false)),
    ("list", view(None, "📂", "列出内容", false)),
    ("rename", view(None, "🔤", "重命名", false)),
    ("remove", view(None, "🗑️", "删除文件", false)),
    ("delete", view(None, "🗑️", "删除文件", false)),
    ("rm", view(None, "🗑️", "删除文件", false)),
    ("mkdir", view(None, "📁", "创建目录", false)),
    ("move", view(None, "📦", "移动文件", false)),
    ("mv", view(None, "📦", "移动文件", false)),
    ("copy", view(None, "📋", "复制文件", false)),
    ("cp", view(None, "📋", "复制文件", false)),
    ("link", view(None, "🔗", "创建链接", false)),
    ("ln", view(None, "🔗", "创建链接", false)),
    ("install", view(None, "📥", "安装依赖", false)),
    ("test", view(None, "🧪", "运行测试", false)),
    ("build", view(None, "🏗️", "构建项目", false)),
    ("git", view(None, "🌿", "执行 Git 操作", false)),
];

const GENERIC_PERMISSION: PermissionView = view(None, "🔐", "执行操作", false);

/// The permission surface's view of one action name: its icon, sentence and
/// whether the request's diff is the request.
pub(crate) fn permission_view(action: &str) -> PermissionView {
    PERMISSION_VIEWS
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(_, view)| *view)
        .unwrap_or(GENERIC_PERMISSION)
}

/// The note a Codex search's raw-JSON fallback appends under a body its budget
/// CLIPPED ([`SEARCH_TRUNCATION_NOTE`]): the body clips bare, so without this
/// line a cut payload would look complete.
fn codex_search_truncation_note(tool: &ToolPanel) -> Option<&'static str> {
    let results = super::tool_render::codex_search_results(tool)?;
    if super::tool_render::codex_search_entries(results).is_some() {
        return None;
    }
    let body = super::tool_render::codex_search_raw_body(results);
    (!super::tool_render::fenced_body_fits(&body, Some("json"), budget::TOOL_PANEL_FENCED.max))
        .then_some(SEARCH_TRUNCATION_NOTE)
}

/// An image read's Input line: the file it looked at. Unlike a `read` there is
/// no preview body to fold, so the panel only names it — with the same backtick
/// flattening as a file change's path. Selected only by the neutral `Read`
/// detail's `image` flag, never by a name.
pub(super) fn image_input(input: &serde_json::Value) -> String {
    let Some(obj) = input.as_object() else {
        return input.to_string();
    };
    match obj
        .get("path")
        .and_then(serde_json::Value::as_str)
        .filter(|path| !path.trim().is_empty())
    {
        Some(path) => format!("🖼 {}", super::tool_render::inline_code(path)),
        None => format_generic_input(input),
    }
}

/// `collabAgentToolCall`: the whole shape cola's adapter writes (issue #962).
/// The identity name is never the selector — the launch's neutral detail plus
/// this input shape is.
pub(super) fn collab_agent_shape(input: Option<&serde_json::Value>) -> bool {
    let Some(obj) = input.and_then(serde_json::Value::as_object) else {
        return false;
    };
    let optional_scalar = |key: &str| {
        obj.get(key)
            .is_some_and(|value| value.is_null() || value.is_string())
    };
    obj.get("tool").and_then(serde_json::Value::as_str).is_some()
        && optional_scalar("status")
        && obj
            .get("senderThreadId")
            .and_then(serde_json::Value::as_str)
            .is_some()
        && obj
            .get("receiverThreadIds")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|ids| ids.iter().all(serde_json::Value::is_string))
        && optional_scalar("prompt")
        && optional_scalar("model")
        && optional_scalar("reasoningEffort")
        && obj
            .get("agents")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|agents| {
                agents.iter().all(|agent| {
                    agent
                        .get("threadId")
                        .and_then(serde_json::Value::as_str)
                        .is_some()
                        && agent.get("status").and_then(serde_json::Value::as_str).is_some()
                        && agent
                            .get("message")
                            .is_some_and(|message| message.is_null() || message.is_string())
                        && agent
                            .get("label")
                            .is_none_or(|label| label.is_null() || label.is_string())
                })
            })
}

/// A target's readable label: the child's own `agentPath` when the read carried
/// one, otherwise the thread id's first eight characters, folded to one line
/// because the folded title and the body row are single lines.
pub(super) fn collab_agent_label(agent: &serde_json::Value) -> Option<String> {
    let thread_id = agent.get("threadId").and_then(serde_json::Value::as_str)?;
    let label = agent
        .get("label")
        .and_then(serde_json::Value::as_str)
        .filter(|label| !label.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| thread_id.chars().take(8).collect());
    Some(super::tool_render::folded_header_fragment(&label))
}
