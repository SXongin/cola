//! The identity → view contract (#1135): one tool renders through both the
//! Tool Panel and the permission card from the same [`ToolFamily`].

use crate::backend::types::PermissionRequest;
use crate::backend::{
    DelegationEffect, FileAction, FileEdit, SearchScope, ToolCall, ToolDetail, ToolIdentity, ToolKind,
    ToolOutput, ToolStatus,
};
use crate::bridge::request::kind::describe_permission;
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

/// One permission request as an adapter reports it: the action name, one
/// pattern, and the metadata the renderer reads.
fn permission(action: &str, metadata: serde_json::Value) -> PermissionRequest {
    PermissionRequest {
        request_id: "per_1".to_string(),
        session_id: Some("ses_1".to_string()),
        permission: Some(action.to_string()),
        patterns: vec!["/proj/src/main.rs".to_string()],
        metadata: Some(metadata),
        always: vec![],
        always_supported: true,
    }
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
        // The permission surface's own renderer, not just its table: its body
        // names the same tool.
        let words = permission_view(action);
        let body = describe_permission(&permission(action, serde_json::json!({})));
        assert!(
            body.starts_with(&format!("{} **{}**", words.icon, words.label)),
            "{action}: the permission body renders its words: {body}"
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
        "the permission surface names the same tool"
    );
    let body = describe_permission(&permission("shell", serde_json::json!({})));
    assert_eq!(
        body.lines().next(),
        Some("⚡ **执行 Shell 命令**"),
        "the permission card renders the same tool's sentence: {body}"
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

/// The FileEdits identity end to end: the panel renders the hunks and the
/// permission body renders the same change, both selected by this module — the
/// panel by `(kind, detail)`, the permission by its action name and the one
/// `renders_diff_body` flag.
#[test]
fn a_file_edit_and_an_edit_permission_render_the_same_change() {
    let diff = "@@ -1 +1 @@\n-old\n+new";
    let call = panel(
        ToolKind::Edit,
        ToolDetail::FileEdits(vec![FileEdit {
            path: "/x/a.rs".to_string(),
            action: FileAction::Update,
            rename_to: None,
            diff: Some(diff.to_string()),
        }]),
    );
    assert_eq!(
        ToolView::of(&call).family(),
        permission_view("edit").family.expect("edit names the family"),
        "one tool"
    );
    let panel_body = panel_markdown(call).as_str().unwrap_or_default().to_string();
    assert!(
        panel_body.contains("+new"),
        "the panel renders the hunk: {panel_body}"
    );
    let permission_body = describe_permission(&permission(
        "edit",
        serde_json::json!({"filepath": "/x/a.rs", "diff": diff}),
    ));
    assert!(
        permission_body.contains("**改动**: 📄 `/x/a.rs` (+1 −1)") && permission_body.contains("+new"),
        "the permission card renders the same change: {permission_body}"
    );
}
