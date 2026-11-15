//! Cross-adapter tool-semantics conformance (spec #1057 P1, ADR-0105).
//!
//! One test per shared kind: the same semantic call, expressed in each
//! producer's own wire shape, must land the same `(kind, detail)`. The suite
//! drives the real decoders (V1's tool part, V2's tool item) and Codex's real
//! completion-event → `Item::parts` projection, so an id that loses its arm
//! falls into `Other`/`Opaque` and fails here instead of silently defaulting.
//! A third-party id stays `Other`/`Opaque` on every producer.
//!
//! `every_declared_shared_id_is_mapped` iterates the adapter's own inventory
//! (`opencode::tool_semantics::SHARED_TOOLS`), so a declared id cannot exist
//! without an arm, and an arm cannot exist without being declared.
//!
//! The surviving `*_baseline_card` tests pin byte-identical rendering for the
//! shapes P2 did not change: each producer's decoded wire payload is rendered
//! through the real card builder, and the expected JSON was captured from
//! `7c9859da`'s code. P3 moved the edit family onto `(kind, detail)`, so that
//! one snapshot is now a rendered-observable test instead.

use serde_json::{Value, json};

use super::tool_semantics::shared_tool_inventory;
use super::v1::decode_tool_for_conformance as v1_tool;
use super::v2::decode_tool_for_conformance as v2_tool;
use crate::backend::{
    DelegationEffect, FileAction, FileEdit, NestedCall, SearchScope, ToolCall, ToolDetail, ToolKind,
    ToolStatus,
};
use crate::codex::read_model::{
    ItemShape, RECOGNIZED_ITEM_TYPES, projected_item_for_conformance as codex_project,
    tool_for_conformance as codex_tool,
};

/// One V1 tool's `(kind, detail)`, decoded through the legacy wire decoder.
fn v1(name: &str, input: Value, metadata: Value) -> (ToolKind, ToolDetail) {
    let call = v1_tool(name, input, metadata, None, None);
    (call.kind, call.detail)
}

/// One V2 tool's `(kind, detail)`, decoded through the V2 wire decoder.
fn v2(name: &str, input: Value, metadata: Value) -> (ToolKind, ToolDetail) {
    let call = v2_tool(name, input, metadata, None);
    (call.kind, call.detail)
}

/// One Codex item's `(kind, detail)`, through the adapter's real projection
/// path (not a second copy of its mapping).
fn codex(item: Value) -> (ToolKind, ToolDetail) {
    let call = codex_tool(&item);
    (call.kind, call.detail)
}

/// The neutral concept a detail declares, with its payload values erased. The
/// cross-producer comparisons use it so two ids cannot pass by carrying the
/// same values under different shapes (or by both matching a tautology).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailShape {
    None,
    Command,
    Program,
    FileEdits,
    Search,
    Read,
    Delegation,
    Opaque,
}

fn detail_shape(detail: &ToolDetail) -> DetailShape {
    match detail {
        ToolDetail::None => DetailShape::None,
        ToolDetail::Command { .. } => DetailShape::Command,
        ToolDetail::Program { .. } => DetailShape::Program,
        ToolDetail::FileEdits(_) => DetailShape::FileEdits,
        ToolDetail::Search { .. } => DetailShape::Search,
        ToolDetail::Read { .. } => DetailShape::Read,
        ToolDetail::Delegation { .. } => DetailShape::Delegation,
        ToolDetail::Opaque => DetailShape::Opaque,
    }
}

/// Render one decoded call through the real Tool Panel card builder. The
/// baseline strings below were produced by this exact function on `7c9859da`.
fn render_card(call: ToolCall) -> String {
    use crate::feishu::card::CardState;
    use crate::feishu::card::shell::CardBuilder;
    use crate::feishu::card::tool_render::ToolPanel;
    let card = CardBuilder::new()
        .with_state(CardState::Done)
        .with_tool_at(ToolPanel::new(call), None, Some("tool_call"), Some("ses_1"))
        .build();
    serde_json::to_string(&card).expect("a card serializes")
}

/// Execute — `bash` (V1), `shell` (V2) and `codex/command_execution` are one
/// command.
#[test]
fn execute_is_one_command_on_every_producer() {
    let expected = (
        ToolKind::Execute,
        ToolDetail::Command {
            command: "ls -la".to_string(),
            cwd: Some("/work".to_string()),
            description: None,
            exit: Some(0),
            timeout: false,
        },
    );

    assert_eq!(
        v1(
            "bash",
            json!({"command": "ls -la", "workdir": "/work"}),
            json!({"exit": 0})
        ),
        expected
    );
    assert_eq!(
        v2(
            "shell",
            json!({"command": "ls -la", "cwd": "/work"}),
            json!({"exit": 0})
        ),
        expected
    );
    assert_eq!(
        codex(json!({
            "type": "commandExecution",
            "id": "item-command",
            "command": "ls -la",
            "cwd": "/work",
            "exitCode": 0,
            "status": "completed"
        })),
        expected
    );

    // The one-line description is V1 `bash`'s own input field; the shape keeps
    // it and the other two producers declare none (neither wire carries one),
    // so the field is the same fact wherever a client has it.
    assert_eq!(
        v1(
            "bash",
            json!({"command": "ls -la", "workdir": "/work", "description": "List files"}),
            json!({"exit": 0})
        ),
        (
            ToolKind::Execute,
            ToolDetail::Command {
                command: "ls -la".to_string(),
                cwd: Some("/work".to_string()),
                description: Some("List files".to_string()),
                exit: Some(0),
                timeout: false,
            }
        )
    );
    for (name, detail) in [
        (
            "shell",
            v2(
                "shell",
                json!({"command": "ls -la", "cwd": "/work"}),
                json!({"exit": 0}),
            ),
        ),
        (
            "codex/command_execution",
            codex(json!({
                "type": "commandExecution",
                "id": "item-command",
                "command": "ls -la",
                "cwd": "/work",
                "exitCode": 0,
                "status": "completed"
            })),
        ),
    ] {
        let ToolDetail::Command { description, .. } = detail.1 else {
            panic!("{name} is a command");
        };
        assert_eq!(description, None, "{name} declares no description");
    }
}

/// Read — `read` and an `imageView` are one file read; V1 names the path
/// `filePath`, V2 `path`, and an empty path is still a read with `path: None`.
#[test]
fn read_is_one_file_read_on_every_producer() {
    let expected = (
        ToolKind::Read,
        ToolDetail::Read {
            path: Some("src/a.rs".to_string()),
            preview: None,
            directory: false,
        },
    );
    assert_eq!(v1("read", json!({"filePath": "src/a.rs"}), Value::Null), expected);
    assert_eq!(v2("read", json!({"path": "src/a.rs"}), Value::Null), expected);
    assert_eq!(
        codex(json!({"type": "imageView", "id": "item-image", "path": "src/a.rs"})),
        expected
    );

    let no_path = (
        ToolKind::Read,
        ToolDetail::Read {
            path: None,
            preview: None,
            directory: false,
        },
    );
    assert_eq!(v1("read", json!({}), Value::Null), no_path);
    assert_eq!(v2("read", json!({}), Value::Null), no_path);
    assert_eq!(
        codex(json!({"type": "imageView", "id": "item-image", "path": null})),
        no_path
    );
    assert_eq!(
        codex(json!({"type": "imageView", "id": "item-image", "path": ""})),
        no_path
    );
}

/// Edit — `write` is one add (V1 `filePath`, V2 `path`), `edit` one update
/// with its diff, and `patch`'s family names its recorded files.
#[test]
fn edit_is_one_file_edit_on_every_producer() {
    let add = (
        ToolKind::Edit,
        ToolDetail::FileEdits(vec![FileEdit {
            path: "src/a.rs".to_string(),
            action: FileAction::Add,
            rename_to: None,
            diff: None,
        }]),
    );
    assert_eq!(v1("write", json!({"filePath": "src/a.rs"}), Value::Null), add);
    assert_eq!(v2("write", json!({"path": "src/a.rs"}), Value::Null), add);

    let diff = "@@ -1 +1 @@\n-old\n+new";
    let update = (
        ToolKind::Edit,
        ToolDetail::FileEdits(vec![FileEdit {
            path: "src/a.rs".to_string(),
            action: FileAction::Update,
            rename_to: None,
            diff: Some(diff.to_string()),
        }]),
    );
    assert_eq!(
        v1("edit", json!({"filePath": "src/a.rs"}), json!({"diff": diff})),
        update
    );
    assert_eq!(
        v2(
            "edit",
            json!({"path": "src/a.rs"}),
            json!({"files": [{"file": "src/a.rs", "patch": diff}]})
        ),
        update
    );
    assert_eq!(
        codex(json!({
            "type": "fileChange",
            "id": "item-file",
            "changes": [{
                "path": "src/a.rs",
                "kind": {"type": "update", "movePath": null},
                "diff": diff
            }]
        })),
        update
    );

    // V2's `edit` also names its file in the input; a payload without the
    // metadata list still declares the update rather than an empty one.
    assert_eq!(
        v2(
            "edit",
            json!({"path": "src/a.rs", "oldString": "old", "newString": "new"}),
            Value::Null
        ),
        (
            ToolKind::Edit,
            ToolDetail::FileEdits(vec![FileEdit {
                path: "src/a.rs".to_string(),
                action: FileAction::Update,
                rename_to: None,
                diff: None,
            }])
        )
    );

    // V1's real `apply_patch` metadata is one `{filepath, diff}` pair (the
    // permission card reads the same shape); it names the file and carries the
    // panel's diff.
    assert_eq!(
        v1(
            "apply_patch",
            json!({"patchText": "*** Begin Patch"}),
            json!({"filepath": "src/a.rs", "diff": "@@ -1 +1 @@\n-a\n+b"})
        ),
        (
            ToolKind::Edit,
            ToolDetail::FileEdits(vec![FileEdit {
                path: "src/a.rs".to_string(),
                action: FileAction::Update,
                rename_to: None,
                diff: Some("@@ -1 +1 @@\n-a\n+b".to_string()),
            }])
        )
    );
    // V2's real `patch` metadata is its `FileDiff.Info` list.
    assert_eq!(
        v2(
            "patch",
            json!({"patchText": "*** Begin Patch"}),
            json!({"files": [
                {"file": "src/a.rs", "patch": "@@ -1 +1 @@\n-a\n+b", "status": "modified"},
                {"file": "src/b.rs", "patch": "", "status": "deleted"}
            ]})
        ),
        (
            ToolKind::Edit,
            ToolDetail::FileEdits(vec![
                FileEdit {
                    path: "src/a.rs".to_string(),
                    action: FileAction::Update,
                    rename_to: None,
                    diff: Some("@@ -1 +1 @@\n-a\n+b".to_string()),
                },
                FileEdit {
                    path: "src/b.rs".to_string(),
                    action: FileAction::Delete,
                    rename_to: None,
                    diff: None,
                },
            ])
        )
    );
    // A V1 patch with only a diff names no file, and a path-less entry is not
    // a FileEdit: that stays P3's patch-text parse.
    assert_eq!(
        v1(
            "apply_patch",
            json!({"patchText": "*** Begin Patch"}),
            json!({"diff": "@@ -1 +1 @@\n-a\n+b"})
        ),
        (ToolKind::Edit, ToolDetail::FileEdits(Vec::new()))
    );
}

/// A Codex change's action — add, delete, update, rename — comes from the wire
/// `kind` union, not a tool name; a rename carries its destination.
#[test]
fn a_codex_file_change_action_is_its_wire_kind() {
    let action = |kind: Value| match codex(json!({
        "type": "fileChange",
        "id": "item-file",
        "changes": [{"path": "src/a.rs", "kind": kind, "diff": ""}]
    })) {
        (ToolKind::Edit, ToolDetail::FileEdits(edits)) => edits[0].clone(),
        other => panic!("a file change is an edit: {other:?}"),
    };
    assert_eq!(action(json!({"type": "add"})).action, FileAction::Add);
    assert_eq!(action(json!({"type": "delete"})).action, FileAction::Delete);
    assert_eq!(
        action(json!({"type": "update", "movePath": null})).action,
        FileAction::Update
    );
    let rename = action(json!({"type": "update", "movePath": "src/b.rs"}));
    assert_eq!(rename.action, FileAction::Rename);
    assert_eq!(rename.rename_to.as_deref(), Some("src/b.rs"));
}

/// OpenCode's patch TEXT names each file's action and its hunks; a rename is
/// the `*** Move to:` line. Both generations declare the same shape.
#[test]
fn opencode_patch_text_names_each_action() {
    let patch = "\
*** Begin Patch
*** Add File: a.rs
+added
*** Update File: b.rs
@@
-old
+new
*** Delete File: c.rs
*** Update File: old.rs
*** Move to: new.rs
@@
-old
+new
*** End Patch";
    let expected = (
        ToolKind::Edit,
        ToolDetail::FileEdits(vec![
            FileEdit {
                path: "a.rs".to_string(),
                action: FileAction::Add,
                rename_to: None,
                diff: Some("@@ -0,0 +1,1 @@\n+added".to_string()),
            },
            FileEdit {
                path: "b.rs".to_string(),
                action: FileAction::Update,
                rename_to: None,
                diff: Some("@@\n-old\n+new".to_string()),
            },
            FileEdit {
                path: "c.rs".to_string(),
                action: FileAction::Delete,
                rename_to: None,
                diff: None,
            },
            FileEdit {
                path: "old.rs".to_string(),
                action: FileAction::Rename,
                rename_to: Some("new.rs".to_string()),
                diff: Some("@@\n-old\n+new".to_string()),
            },
        ]),
    );
    assert_eq!(
        v2("patch", json!({"patchText": patch}), Value::Null),
        expected,
        "the V2 patch tool reads its patch text"
    );
    assert_eq!(
        v1("apply_patch", json!({"patchText": patch}), Value::Null),
        expected,
        "V1's apply_patch answers the same shape"
    );
}

/// A pure rename (no hunk) still carries its action and destination: the
/// action is a field, never inferred from a diff.
#[test]
fn a_patch_text_rename_with_no_hunk_keeps_its_destination() {
    let patch = "\
*** Begin Patch
*** Update File: old.rs
*** Move to: new.rs
*** End Patch";
    assert_eq!(
        v2("patch", json!({"patchText": patch}), Value::Null),
        (
            ToolKind::Edit,
            ToolDetail::FileEdits(vec![FileEdit {
                path: "old.rs".to_string(),
                action: FileAction::Rename,
                rename_to: Some("new.rs".to_string()),
                diff: None,
            }])
        )
    );
}

/// Search — each scope is one shape: `glob` filenames, `grep` contents,
/// `websearch`/`codex/web_search` the web (with the recorded result count).
#[test]
fn search_is_one_shape_per_scope_on_every_producer() {
    let files = (
        ToolKind::Search,
        ToolDetail::Search {
            query: Some("*.rs".to_string()),
            matches: None,
            scope: SearchScope::FileName,
        },
    );
    assert_eq!(v1("glob", json!({"pattern": "*.rs"}), Value::Null), files);
    assert_eq!(v2("glob", json!({"pattern": "*.rs"}), Value::Null), files);

    let content = (
        ToolKind::Search,
        ToolDetail::Search {
            query: Some("fn main".to_string()),
            matches: None,
            scope: SearchScope::Content,
        },
    );
    assert_eq!(v1("grep", json!({"pattern": "fn main"}), Value::Null), content);
    assert_eq!(v2("grep", json!({"pattern": "fn main"}), Value::Null), content);

    let web = (
        ToolKind::Search,
        ToolDetail::Search {
            query: Some("rust".to_string()),
            matches: None,
            scope: SearchScope::Web,
        },
    );
    assert_eq!(v1("websearch", json!({"query": "rust"}), Value::Null), web);
    assert_eq!(v2("websearch", json!({"query": "rust"}), Value::Null), web);
    assert_eq!(
        codex(json!({"type": "webSearch", "id": "item-web", "query": "rust"})),
        web
    );
    // A completed Codex search reports its result rows; the count is recorded,
    // not fabricated.
    assert_eq!(
        codex(json!({
            "type": "webSearch",
            "id": "item-web",
            "query": "rust",
            "results": [{"title": "a", "url": "https://a.example"}]
        })),
        (
            ToolKind::Search,
            ToolDetail::Search {
                query: Some("rust".to_string()),
                matches: Some(1),
                scope: SearchScope::Web,
            }
        )
    );
}

/// Fetch — `webfetch` declares the kind and nothing extra.
#[test]
fn fetch_is_the_bare_kind_on_both_generations() {
    let expected = (ToolKind::Fetch, ToolDetail::None);
    assert_eq!(
        v1("webfetch", json!({"url": "https://a.example"}), Value::Null),
        expected
    );
    assert_eq!(
        v2("webfetch", json!({"url": "https://a.example"}), Value::Null),
        expected
    );
}

/// Delegation — `task`/`subagent`/`spawnAgent` are one launch; a child
/// observation is a report and an addressing verb a receipt.
#[test]
fn delegation_is_one_shape_per_verb() {
    let launch = (
        ToolKind::Other,
        ToolDetail::Delegation {
            runs: Vec::new(),
            effect: DelegationEffect::Launch,
        },
    );
    assert_eq!(v1("task", json!({"description": "review"}), Value::Null), launch);
    assert_eq!(
        v2("subagent", json!({"description": "review"}), Value::Null),
        launch
    );
    assert_eq!(
        codex(json!({
            "type": "collabAgentToolCall",
            "id": "item-collab",
            "tool": "spawnAgent",
            "status": "completed",
            "senderThreadId": "thread-1",
            "receiverThreadIds": [],
            "agentsStates": {}
        })),
        launch
    );

    let addressed = (
        ToolKind::Other,
        ToolDetail::Delegation {
            runs: Vec::new(),
            effect: DelegationEffect::Address,
        },
    );
    assert_eq!(
        codex(json!({
            "type": "collabAgentToolCall",
            "id": "item-collab",
            "tool": "wait",
            "status": "completed",
            "senderThreadId": "thread-1",
            "receiverThreadIds": [],
            "agentsStates": {}
        })),
        addressed
    );
    assert_eq!(
        codex(json!({
            "type": "subAgentActivity",
            "id": "item-activity",
            "kind": "started",
            "agentThreadId": "thread-2",
            "agentPath": "/root/worker"
        })),
        (
            ToolKind::Other,
            ToolDetail::Delegation {
                runs: Vec::new(),
                effect: DelegationEffect::Report,
            }
        )
    );
}

/// Code Mode — `execute` carries the program's source and its nested calls;
/// the real wire shape has no language, so that field stays `None`.
#[test]
fn code_mode_carries_the_program_and_its_nested_calls() {
    let (kind, detail) = v2(
        "execute",
        json!({"code": "return 1"}),
        json!({"toolCalls": [
            {"tool": "fetch", "status": "completed", "input": {"url": "https://a.example"}},
            {"tool": "grep", "status": "error"},
            {"tool": "read", "status": "running"}
        ]}),
    );
    assert_eq!(kind, ToolKind::Execute);
    assert_eq!(
        detail,
        ToolDetail::Program {
            source: "return 1".to_string(),
            language: None,
            error: false,
            calls: vec![
                NestedCall {
                    name: "fetch".to_string(),
                    status: ToolStatus::Completed,
                    input: Some(json!({"url": "https://a.example"})),
                },
                NestedCall {
                    name: "grep".to_string(),
                    status: ToolStatus::Error,
                    input: None,
                },
                NestedCall {
                    name: "read".to_string(),
                    status: ToolStatus::Running,
                    input: None,
                },
            ],
        }
    );
}

/// Other — the known shared ids with nothing extra, and a third-party id that
/// is never sniffed.
#[test]
fn other_is_none_for_known_ids_and_opaque_for_third_party_ids() {
    for name in ["todowrite", "skill", "question"] {
        assert_eq!(
            v1(name, json!({}), Value::Null),
            (ToolKind::Other, ToolDetail::None),
            "{name} (V1)"
        );
        assert_eq!(
            v2(name, json!({}), Value::Null),
            (ToolKind::Other, ToolDetail::None),
            "{name} (V2)"
        );
    }

    for name in ["my_server/my_tool", "mystery"] {
        assert_eq!(
            v1(name, json!({"x": 1}), Value::Null),
            (ToolKind::Other, ToolDetail::Opaque),
            "{name} (V1) is third-party"
        );
        assert_eq!(
            v2(name, json!({"x": 1}), Value::Null),
            (ToolKind::Other, ToolDetail::Opaque),
            "{name} (V2) is third-party"
        );
    }
    assert_eq!(
        codex(json!({
            "type": "mcpToolCall",
            "id": "item-mcp",
            "server": "my_server",
            "tool": "my_tool",
            "arguments": {},
            "result": null,
            "error": null,
            "status": "completed"
        })),
        (ToolKind::Other, ToolDetail::Opaque)
    );
}

/// An `imageGeneration` creates an image, so it stays opaque; only an
/// `imageView` reads a file. Codex projects both into one `codex/image` item
/// kind, so this distinction is the adapter's, not the card's.
#[test]
fn an_image_generation_is_opaque_where_an_image_view_is_a_read() {
    assert_eq!(
        codex(json!({"type": "imageGeneration", "id": "item-gen", "savedPath": "out.png"})),
        (ToolKind::Other, ToolDetail::Opaque)
    );
    assert_eq!(
        codex(json!({"type": "imageView", "id": "item-view", "path": "in.png"})),
        (
            ToolKind::Read,
            ToolDetail::Read {
                path: Some("in.png".to_string()),
                preview: None,
                directory: false,
            }
        )
    );
}

/// The client's title is the adapter's fact: V1's `state.title` reaches the
/// call (an empty one is no title) and V2, which reports none, stays `None`.
#[test]
fn a_v1_title_reaches_the_call_and_a_v2_call_reports_none() {
    let titled = v1_tool(
        "bash",
        json!({"command": "ls"}),
        Value::Null,
        Some("List files"),
        None,
    );
    assert_eq!(titled.title.as_deref(), Some("List files"));

    let untitled = v1_tool("bash", json!({"command": "ls"}), Value::Null, Some(""), None);
    assert_eq!(untitled.title, None, "an empty title is no title");

    let v2_call = v2_tool("shell", json!({"command": "ls"}), Value::Null, None);
    assert_eq!(v2_call.title, None, "V2's ToolState reports no title");
}

/// Every file-bearing detail names the same paths in `ToolCall::locations`,
/// on all three producers.
#[test]
fn file_bearing_calls_carry_their_detail_locations() {
    let calls = [
        v1_tool("read", json!({"filePath": "src/a.rs"}), Value::Null, None, None),
        v2_tool("read", json!({"path": "src/a.rs"}), Value::Null, None),
        v1_tool("write", json!({"filePath": "src/a.rs"}), Value::Null, None, None),
        v2_tool("write", json!({"path": "src/a.rs"}), Value::Null, None),
        v1_tool(
            "apply_patch",
            json!({"patchText": "*** Begin Patch"}),
            json!({"filepath": "src/a.rs", "diff": "@@ -1 +1 @@"}),
            None,
            None,
        ),
        v2_tool(
            "patch",
            json!({"patchText": "*** Begin Patch"}),
            json!({"files": [{"file": "src/a.rs", "patch": "@@ -1 +1 @@"}]}),
            None,
        ),
        codex_tool(&json!({"type": "imageView", "id": "item-view", "path": "src/a.rs"})),
        codex_tool(&json!({
            "type": "fileChange",
            "id": "item-file",
            "changes": [{"path": "src/a.rs", "kind": {"type": "update"}, "diff": ""}]
        })),
    ];
    for call in calls {
        assert_eq!(
            call.locations,
            call.detail.locations(),
            "locations must mirror the detail"
        );
        assert_eq!(call.locations.len(), 1, "one named file: {call:#?}");
        assert_eq!(call.locations[0].path, "src/a.rs");
        assert_eq!(call.locations[0].line, None);
    }
}

/// Codex's own inventory: every wire item type its decoder recognizes has a
/// declared Tool kind and shared shape (`NoTool` for the message/session
/// facts), so a recognized type with no expectation fails here.
#[test]
fn every_recognized_codex_item_type_has_its_declared_shape() {
    assert_eq!(
        RECOGNIZED_ITEM_TYPES
            .iter()
            .map(|(wire, _, _)| *wire)
            .collect::<Vec<_>>(),
        [
            "userMessage",
            "agentMessage",
            "reasoning",
            "commandExecution",
            "fileChange",
            "mcpToolCall",
            "webSearch",
            "imageView",
            "imageGeneration",
            "collabAgentToolCall",
            "subAgentActivity",
            "plan",
            "contextCompaction",
        ],
        "the declared inventory is the decoder's complete recognized-item set"
    );

    for &(wire, expected_kind, shape) in RECOGNIZED_ITEM_TYPES {
        let item = json!({"type": wire, "id": format!("item-{wire}")});
        let call = codex_project(&item);
        match (call, expected_kind, shape) {
            (None, None, ItemShape::NoTool) => {}
            (Some(call), Some(expected_kind), shape) => {
                assert_eq!(call.kind, expected_kind, "{wire} keeps its declared kind");
                let expected = match shape {
                    ItemShape::NoTool => panic!("{wire} declares no tool but a shape"),
                    ItemShape::Command => DetailShape::Command,
                    ItemShape::FileEdits => DetailShape::FileEdits,
                    ItemShape::Search => DetailShape::Search,
                    ItemShape::Read => DetailShape::Read,
                    ItemShape::Delegation => DetailShape::Delegation,
                    ItemShape::Opaque => DetailShape::Opaque,
                };
                assert_eq!(
                    detail_shape(&call.detail),
                    expected,
                    "{wire} keeps its declared detail shape"
                );
            }
            (call, kind, _) => panic!(
                "{wire}: a tool call and its kind must agree (tool={:?}, kind={kind:?})",
                call.is_some()
            ),
        }
    }
}

/// The Codex inventory is the decoder's real match set: a new `from_wire` arm
/// that is not declared here fails, not just a declaration that diverges.
#[test]
fn the_codex_item_inventory_covers_every_recognized_wire_type() {
    let source = include_str!("../codex/read_model.rs");
    let start = source
        .find("fn from_wire(item: &Value) -> Self {")
        .expect("the decoder's item match exists");
    let tail = &source[start..];
    let end = tail
        .find("fn from_completed")
        .expect("the decoder's match has an end");
    let mut decoded: Vec<&str> = tail[..end]
        .match_indices("Some(\"")
        .map(|(index, _)| {
            let rest = &tail[..end][index + "Some(\"".len()..];
            &rest[..rest.find('"').expect("a closed wire-type literal")]
        })
        .collect();
    decoded.sort_unstable();
    decoded.dedup();

    let mut declared: Vec<&str> = RECOGNIZED_ITEM_TYPES.iter().map(|(wire, _, _)| *wire).collect();
    declared.sort_unstable();
    assert_eq!(
        decoded, declared,
        "the declared inventory must be the decoder's recognized wire types"
    );
}

/// Every id the adapter declares is mapped on both generations, and the
/// inventory itself has no duplicate — an unmapped shared id fails here rather
/// than becoming opaque.
#[test]
fn every_declared_shared_id_is_mapped() {
    let inventory = shared_tool_inventory();
    assert!(!inventory.is_empty(), "the adapter claims at least one id");
    let mut ids: Vec<_> = inventory.iter().map(|(id, _)| *id).collect();
    let count = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), count, "a shared id is declared twice");
    assert_eq!(
        ids,
        [
            "apply_patch",
            "bash",
            "edit",
            "execute",
            "glob",
            "grep",
            "patch",
            "question",
            "read",
            "shell",
            "skill",
            "subagent",
            "task",
            "todowrite",
            "webfetch",
            "websearch",
            "write",
        ],
        "the declared inventory is the adapter's complete shared-id set"
    );

    for id in inventory.iter().map(|(id, _)| *id) {
        for (generation, (_, detail)) in [
            ("V1", v1(id, json!({}), Value::Null)),
            ("V2", v2(id, json!({}), Value::Null)),
        ] {
            assert_ne!(
                detail_shape(&detail),
                DetailShape::Opaque,
                "{id} ({generation}) fell into the opaque default"
            );
        }
    }

    // One neutral concept, several ids: a divergent arm fails here (the table
    // lookup alone cannot compare two ids to each other).
    for group in [
        &["bash", "shell"][..],
        &["patch", "apply_patch"],
        &["task", "subagent"],
        &["todowrite", "skill", "question"],
    ] {
        let first = group[0];
        let (expected_kind, expected_detail) = v1(first, json!({}), Value::Null);
        let expected_shape = detail_shape(&expected_detail);
        for id in &group[1..] {
            for (generation, (kind, detail)) in [
                ("V1", v1(id, json!({}), Value::Null)),
                ("V2", v2(id, json!({}), Value::Null)),
            ] {
                assert_eq!(
                    (kind, detail_shape(&detail)),
                    (expected_kind, expected_shape),
                    "{id} ({generation}) must declare the same concept as {first}"
                );
            }
        }
    }
}

/// A tool the adapter does not know is never guessed at: both OpenCode
/// generations agree on the opaque fallback for every third-party shape.
#[test]
fn an_unknown_id_is_opaque_on_both_generations() {
    let expected = (ToolKind::Other, ToolDetail::Opaque);
    for name in ["mcp__server__tool", "some/plugin", ""] {
        assert_eq!(v1(name, json!({"path": "x"}), Value::Null), expected);
        assert_eq!(v2(name, json!({"path": "x"}), Value::Null), expected);
    }
}

/// Captured from `7c9859da` by rendering the same decoded payload through this
/// same builder.
const V1_BASELINE_CARD: &str = r#"{"body":{"elements":[{"element_id":"tool_call","elements":[{"content":"**Input**\n`ls -la`\n📁 `/work`\n\n**Output**\n\ntotal 0\n","tag":"markdown"}],"expanded":false,"header":{"icon":{"tag":"standard_icon","token":"down-small-ccm_outlined"},"icon_position":"right","title":{"content":"✅ bash","tag":"plain_text"}},"tag":"collapsible_panel"}]},"config":{"wide_screen_mode":true},"header":{"template":"green","title":{"content":"✅ 完成","tag":"plain_text"}},"schema":"2.0"}"#;

/// Captured from `7c9859da` by the same builder, from Codex's decoded command item.
const CODEX_BASELINE_CARD: &str = r#"{"body":{"elements":[{"element_id":"tool_call","elements":[{"content":"**Input**\n`ls -la`\n📁 `/work`\n\n**Output**\n\ntotal 0\n","tag":"markdown"}],"expanded":false,"header":{"icon":{"tag":"standard_icon","token":"down-small-ccm_outlined"},"icon_position":"right","title":{"content":"✅ 命令","tag":"plain_text"}},"tag":"collapsible_panel"}]},"config":{"wide_screen_mode":true},"header":{"template":"green","title":{"content":"✅ 完成","tag":"plain_text"}},"schema":"2.0"}"#;

/// Byte-identity guard, one producer each: a decoded wire payload renders the
/// exact card the pre-change code rendered. A renderer that started reading a
/// semantic field changes one of these.
///
/// These still hold after P2 moved the Execute renderer onto `(kind, detail)`
/// (#1059): the command and its cwd are the same facts the name arms read, so
/// the card did not change — only the way the renderer found them.
#[test]
fn a_decoded_v1_bash_renders_the_baseline_card() {
    let call = v1_tool(
        "bash",
        json!({"command": "ls -la", "workdir": "/work"}),
        json!({"exit": 0}),
        None,
        Some("total 0\n"),
    );
    assert_eq!(render_card(call), V1_BASELINE_CARD);
}

/// P3 moved the edit family onto `(kind, detail)`: the card's words are the
/// action, its fence is the file's language, and the body is the same diff.
/// Pinned as the rendered observables (the P1 byte snapshot cannot survive a
/// renderer that now reads the detail, by design).
#[test]
fn a_decoded_v2_edit_renders_one_file_edits_panel() {
    let call = v2_tool(
        "edit",
        json!({"path": "src/a.rs", "oldString": "old", "newString": "new"}),
        json!({"files": [{"file": "src/a.rs", "patch": "@@ -1 +1 @@\n-old\n+new"}]}),
        Some("Edited src/a.rs"),
    );
    let card = render_card(call);
    assert!(card.contains("✏️ 编辑 `src/a.rs`"), "action + path: {card}");
    assert!(card.contains("+1 −1"), "the totals: {card}");
    assert!(card.contains("```rust"), "the file's language: {card}");
    assert!(card.contains("+new"), "the hunk: {card}");
    assert!(card.contains("📂 打开"), "the way in: {card}");
    assert!(!card.contains("Edited src/a.rs"), "the summary is noise: {card}");
}

#[test]
fn a_decoded_codex_command_renders_the_baseline_card() {
    let call = codex_tool(&json!({
        "type": "commandExecution",
        "id": "item-command",
        "command": "ls -la",
        "cwd": "/work",
        "status": "completed",
        "exitCode": 0,
        "aggregatedOutput": "total 0\n"
    }));
    assert_eq!(render_card(call), CODEX_BASELINE_CARD);
}

/// The outcome a settled call records under `completed` is a failure, on every
/// producer, straight from the wire through the real decoder and card builder
/// (#1059).
///
/// This is the ticket's one user-visible verdict change: V1's `bash` records its
/// exit in `metadata` beside a `completed` status exactly as V2 does, and before
/// this ticket the card only read that record for the id `shell` — so a failed
/// V1 command rendered ✅. V2 and Codex already rendered ❌ from the same facts,
/// which is why each producer is pinned here: the rows differ in what makes the
/// case non-trivial (V1's verdict is new; V2's and Codex's must not regress).
#[test]
fn a_settled_non_zero_exit_renders_the_failure_icon_from_the_wire() {
    for (name, card) in [
        (
            "v1 bash",
            render_card(v1_tool(
                "bash",
                json!({"command": "make", "workdir": "/work"}),
                json!({"exit": 1}),
                None,
                Some("error: 3 assertions failed\n"),
            )),
        ),
        (
            "v2 shell",
            render_card(v2_tool(
                "shell",
                json!({"command": "make", "cwd": "/work"}),
                json!({"status": "completed", "exit": 1}),
                Some("error: 3 assertions failed\n"),
            )),
        ),
        (
            "codex command",
            render_card(codex_tool(&json!({
                "type": "commandExecution",
                "id": "item-command-fail",
                "command": "make",
                "cwd": "/work",
                "status": "completed",
                "exitCode": 1,
                "aggregatedOutput": "error: 3 assertions failed\n"
            }))),
        ),
    ] {
        assert!(
            card.contains("❌"),
            "{name}: a settled non-zero exit is a failure: {card}"
        );
        assert!(
            card.contains("error: 3 assertions failed"),
            "{name}: the error body survives: {card}"
        );
    }

    // A timeout is the other state `completed` hides.
    for (name, card) in [
        (
            "v1 bash",
            render_card(v1_tool(
                "bash",
                json!({"command": "make", "workdir": "/work"}),
                json!({"timeout": true}),
                None,
                Some("timed out\n"),
            )),
        ),
        (
            "v2 shell",
            render_card(v2_tool(
                "shell",
                json!({"command": "make", "cwd": "/work"}),
                json!({"status": "completed", "timeout": true}),
                Some("timed out\n"),
            )),
        ),
    ] {
        assert!(card.contains("❌"), "{name}: a timeout is a failure: {card}");
        assert!(
            card.contains("timed out"),
            "{name}: the timeout body survives: {card}"
        );
    }

    // Control: a settled success stays ✅ on V1 too (the new verdict reads the
    // recorded outcome, not the producer).
    let ok = render_card(v1_tool(
        "bash",
        json!({"command": "make", "workdir": "/work"}),
        json!({"exit": 0}),
        None,
        Some("ok\n"),
    ));
    assert!(!ok.contains("❌"), "an exit 0 is not a failure: {ok}");
}

/// P3 regression: a decoded V2 `websearch` carries its result envelope as text
/// blocks (its `output.raw` is the content array). It must render through the
/// neutral Search renderer — count, Markdown list, excerpts dropped — not as
/// opaque pretty-printed JSON.
#[test]
fn a_decoded_v2_websearch_renders_its_markdown_results() {
    let results = json!({
        "search_id": "search_1",
        "results": [{
            "url": "https://a.example/one",
            "title": "First result",
            "publish_date": "2026-01-02",
            "excerpts": ["an excerpt that must not appear"],
        }],
    })
    .to_string();
    let call = v2_tool(
        "websearch",
        json!({"query": "rust lifetimes"}),
        Value::Null,
        Some(&results),
    );
    let card = render_card(call);
    assert!(card.contains("🔎 1 条结果"), "the result count: {card}");
    assert!(
        card.contains("[First result](https://a.example/one)"),
        "the Markdown list: {card}"
    );
    assert!(!card.contains("```json"), "not opaque JSON: {card}");
    assert!(!card.contains("excerpt"), "excerpts dropped: {card}");
}

/// V1's `write` records no patch: the adapter synthesizes the Add hunk from the
/// input's content, so both generations render one `➕ 新增` panel.
#[test]
fn a_v1_write_synthesizes_its_add_hunk() {
    let call = v1_tool(
        "write",
        json!({"filePath": "src/a.rs", "content": "one\ntwo"}),
        Value::Null,
        None,
        Some("Wrote file successfully."),
    );
    assert_eq!(
        call.detail,
        ToolDetail::FileEdits(vec![FileEdit {
            path: "src/a.rs".to_string(),
            action: FileAction::Add,
            rename_to: None,
            diff: Some("@@ -0,0 +1,2 @@\n+one\n+two".to_string()),
        }])
    );
    let card = render_card(call);
    assert!(card.contains("➕ 新增 `src/a.rs`"), "the action: {card}");
    assert!(card.contains("+2 −0"), "the totals: {card}");
    assert!(card.contains("+one"), "the synthesized hunk: {card}");
}

/// A path-less V1 patch still names its file, action and hunks from the raw
/// diff's headers: `/dev/null` is an add/delete side, two names a rename.
#[test]
fn a_v1_patch_metadata_recovers_its_action_from_the_diff_headers() {
    let cases = [
        (
            "--- /dev/null\n+++ b/a.rs\n@@ -0,0 +1 @@\n+new",
            FileAction::Add,
            "a.rs",
            None,
        ),
        (
            "--- a/old.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-gone",
            FileAction::Delete,
            "old.rs",
            None,
        ),
        (
            "--- a/old.rs\n+++ b/new.rs\n@@ -1 +1 @@\n-old\n+new",
            FileAction::Rename,
            "old.rs",
            Some("new.rs"),
        ),
    ];
    for (diff, action, path, rename_to) in cases {
        assert_eq!(
            v1(
                "apply_patch",
                json!({"patchText": "*** Begin Patch"}),
                json!({"diff": diff})
            ),
            (
                ToolKind::Edit,
                ToolDetail::FileEdits(vec![FileEdit {
                    path: path.to_string(),
                    action,
                    rename_to: rename_to.map(str::to_string),
                    diff: crate::backend::file_edit_hunks(diff),
                }])
            ),
            "{diff}"
        );
    }
}

/// A patch text with headers but no hunks does not suppress the metadata's
/// applied diff; its action/rename still wins.
#[test]
fn patch_text_actions_merge_with_metadata_hunks() {
    let metadata = json!({"files": [{
        "file": "/a/one.rs",
        "patch": "@@ -1 +1 @@\n-old\n+new",
        "status": "modified"
    }]});
    assert_eq!(
        v2(
            "patch",
            json!({"patchText": "*** Begin Patch\n*** Update File: /a/one.rs\n*** Move to: /a/two.rs\n*** End Patch"}),
            metadata.clone()
        ),
        (
            ToolKind::Edit,
            ToolDetail::FileEdits(vec![FileEdit {
                path: "/a/one.rs".to_string(),
                action: FileAction::Rename,
                rename_to: Some("/a/two.rs".to_string()),
                diff: Some("@@ -1 +1 @@\n-old\n+new".to_string()),
            }])
        )
    );
}

/// A patch that updates one file twice is one edit: the hunks merge in order,
/// so the card's count, totals and open button see one file (#793).
#[test]
fn a_repeated_patch_header_merges_into_one_file() {
    let patch = "\
*** Begin Patch
*** Update File: a.rs
@@
-a
+b
*** Update File: a.rs
@@
-c
+d
*** End Patch";
    assert_eq!(
        v2("patch", json!({"patchText": patch}), Value::Null),
        (
            ToolKind::Edit,
            ToolDetail::FileEdits(vec![FileEdit {
                path: "a.rs".to_string(),
                action: FileAction::Update,
                rename_to: None,
                diff: Some("@@\n-a\n+b\n@@\n-c\n+d".to_string()),
            }])
        )
    );
}

/// A header-only diff is not a body: a pure rename or delete keeps its action
/// and destination but carries no hunks (and so no `+1 −1`).
#[test]
fn a_header_only_metadata_diff_yields_no_hunks() {
    assert_eq!(
        v1(
            "apply_patch",
            json!({"patchText": "*** Begin Patch"}),
            json!({"diff": "diff --git a/old.rs b/new.rs\nsimilarity index 100%\nrename from old.rs\nrename to new.rs"})
        ),
        (
            ToolKind::Edit,
            ToolDetail::FileEdits(vec![FileEdit {
                path: "old.rs".to_string(),
                action: FileAction::Rename,
                rename_to: Some("new.rs".to_string()),
                diff: None,
            }])
        )
    );
    assert_eq!(
        v1(
            "apply_patch",
            json!({"patchText": "*** Begin Patch"}),
            json!({"diff": "--- a/gone.rs\n+++ /dev/null"})
        ),
        (
            ToolKind::Edit,
            ToolDetail::FileEdits(vec![FileEdit {
                path: "gone.rs".to_string(),
                action: FileAction::Delete,
                rename_to: None,
                diff: None,
            }])
        )
    );
}
