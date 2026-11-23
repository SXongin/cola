//! The identity → view contract (#1135): one tool renders through both the
//! Tool Panel and the permission card from the same [`ToolFamily`].

use crate::backend::{
    DelegationEffect, FileAction, FileEdit, SearchScope, ToolCall, ToolDetail, ToolIdentity, ToolKind,
    ToolOutput, ToolStatus,
};
use crate::feishu::card::CardState;
use crate::feishu::card::shell::CardBuilder;
use crate::feishu::card::tool_render::ToolPanel;

use super::{ToolFamily, ToolView, permission_view};

fn panel(kind: ToolKind, detail: ToolDetail) -> ToolPanel {
    ToolPanel::new(ToolCall {
        identity: ToolIdentity {
            name: "probe".to_string(),
            call_id: "call_probe".to_string(),
        },
        kind,
        detail,
        status: ToolStatus::Completed,
        output: ToolOutput::default(),
        ..ToolCall::default()
    })
}

fn panel_markdown(panel: ToolPanel) -> serde_json::Value {
    let card = CardBuilder::new()
        .with_state(CardState::Done)
        .with_tool(panel)
        .build();
    card["body"]["elements"][0]["elements"][0]["content"].clone()
}

/// One identity per pair: the permission action name the adapters report and a
/// call of the same tool, rendered through the panel. The two surfaces must
/// classify to the SAME family — the whole point of one owner — and each must
/// actually render (the panel a body, the permission its icon and sentence).
#[test]
fn one_tool_renders_through_both_surfaces() {
    let cases: Vec<(&str, ToolPanel)> = vec![
        (
            "shell",
            panel(
                ToolKind::Execute,
                ToolDetail::Command {
                    command: "make test".to_string(),
                    cwd: None,
                    description: None,
                    exit: Some(0),
                    timeout: false,
                },
            ),
        ),
        (
            "edit",
            panel(
                ToolKind::Edit,
                ToolDetail::FileEdits(vec![FileEdit {
                    path: "/x/a.rs".to_string(),
                    action: FileAction::Update,
                    rename_to: None,
                    diff: Some("@@ -1 +1 @@\n-old\n+new".to_string()),
                }]),
            ),
        ),
        (
            "read",
            panel(
                ToolKind::Read,
                ToolDetail::Read {
                    path: Some("/x/a.rs".to_string()),
                    preview: None,
                    directory: false,
                    image: false,
                },
            ),
        ),
        (
            "websearch",
            panel(
                ToolKind::Search,
                ToolDetail::Search {
                    query: Some("rust".to_string()),
                    matches: None,
                    scope: SearchScope::Web,
                },
            ),
        ),
        (
            "glob",
            panel(
                ToolKind::Search,
                ToolDetail::Search {
                    query: Some("**/*.rs".to_string()),
                    matches: None,
                    scope: SearchScope::FileName,
                },
            ),
        ),
        ("webfetch", panel(ToolKind::Fetch, ToolDetail::None)),
        (
            "task",
            panel(
                ToolKind::Other,
                ToolDetail::Delegation {
                    runs: Vec::new(),
                    effect: DelegationEffect::Launch,
                },
            ),
        ),
        ("skill", panel(ToolKind::Other, ToolDetail::Skill)),
        ("question", panel(ToolKind::Other, ToolDetail::Ask)),
    ];
    for (action, call) in cases {
        let family = ToolView::of(&call).family();
        assert_eq!(
            permission_view(action).family,
            Some(family),
            "{action}: both surfaces name one tool"
        );
        assert!(
            !panel_markdown(call).as_str().unwrap_or_default().is_empty(),
            "{action}: the panel renders a body"
        );
        let words = permission_view(action);
        assert!(
            !words.label.is_empty() && !words.icon.is_empty(),
            "{action}: the permission surface renders its words"
        );
    }
}

/// The same identity, characterized end to end: a shell call's panel text and a
/// `shell` permission's body, both from this module's one classification.
#[test]
fn a_shell_call_and_a_shell_permission_render_from_one_identity() {
    let call = panel(
        ToolKind::Execute,
        ToolDetail::Command {
            command: "make test".to_string(),
            cwd: Some("/work".to_string()),
            description: None,
            exit: Some(0),
            timeout: false,
        },
    );
    let body = panel_markdown(call).as_str().unwrap_or_default().to_string();
    assert!(
        body.contains("`make test`") && body.contains("📁 `/work`"),
        "the panel renders the command and its directory: {body}"
    );
    let words = permission_view("shell");
    assert_eq!(
        (words.icon, words.label),
        ("⚡", "执行 Shell 命令"),
        "the permission surface renders the same tool"
    );
}

/// Only an action whose request IS a diff (`edit`/`patch`/`apply_patch`) takes
/// the diff body; `write` shares the FileEdits panel family but keeps its
/// scaffolding. One table answers both the body and the file-led target.
#[test]
fn the_permission_diff_flag_is_per_action_not_per_family() {
    for action in ["edit", "patch", "apply_patch"] {
        let view = permission_view(action);
        assert_eq!(view.family, Some(ToolFamily::FileEdits), "{action}");
        assert!(view.renders_diff_body, "{action} renders its diff");
    }
    let write = permission_view("write");
    assert_eq!(write.family, Some(ToolFamily::FileEdits));
    assert!(!write.renders_diff_body, "write keeps its scaffolding");
}

/// An action the table does not know is the generic `🔐 执行操作` — never a
/// silently missing row nor another tool's words.
#[test]
fn an_unknown_action_renders_the_generic_words() {
    for action in ["mystery", "", "?"] {
        let view = permission_view(action);
        assert_eq!(
            (view.icon, view.label, view.family),
            ("🔐", "执行操作", None),
            "{action:?}"
        );
    }
}
