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
//! The `*_baseline_card` tests pin byte-identical rendering: each producer's
//! decoded wire payload is rendered through the real card builder, and the
//! expected JSON was captured from `7c9859da`'s code before this change (a
//! renderer that started reading `kind`/`detail` would fail here).

use serde_json::{Value, json};

use super::tool_semantics::shared_tool_inventory;
use super::v1::decode_tool_for_conformance as v1_tool;
use super::v2::decode_tool_for_conformance as v2_tool;
use crate::backend::{
    DelegationEffect, FileAction, FileEdit, NestedCall, SearchScope, ToolCall, ToolDetail, ToolKind,
    ToolStatus,
};
use crate::codex::read_model::tool_for_conformance as codex_tool;

/// One V1 tool call decoded through the legacy wire decoder.
fn v1_call(name: &str, input: Value, metadata: Value, output: Option<&str>) -> ToolCall {
    v1_tool(name, input, metadata, output)
}

/// One V2 tool call decoded through the V2 wire decoder.
fn v2_call(name: &str, input: Value, metadata: Value, output: Option<&str>) -> ToolCall {
    v2_tool(name, input, metadata, output)
}

fn v1(name: &str, input: Value, metadata: Value) -> (ToolKind, ToolDetail) {
    let call = v1_call(name, input, metadata, None);
    (call.kind, call.detail)
}

fn v2(name: &str, input: Value, metadata: Value) -> (ToolKind, ToolDetail) {
    let call = v2_call(name, input, metadata, None);
    (call.kind, call.detail)
}

/// One Codex item's Tool Call, through the adapter's real projection path.
fn codex(item: Value) -> (ToolKind, ToolDetail) {
    let call = codex_tool(&item);
    (call.kind, call.detail)
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

    let patch = (
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
        ]),
    );
    let patch_metadata = json!({"files": [
        {"file": "src/a.rs", "patch": "@@ -1 +1 @@\n-a\n+b", "status": "modified"},
        {"file": "src/b.rs", "patch": "", "status": "deleted"}
    ]});
    for name in ["patch", "apply_patch"] {
        assert_eq!(
            v1(
                name,
                json!({"patchText": "*** Begin Patch"}),
                patch_metadata.clone()
            ),
            patch,
            "{name} (V1) names its recorded files"
        );
        assert_eq!(
            v2(
                name,
                json!({"patchText": "*** Begin Patch"}),
                patch_metadata.clone()
            ),
            patch,
            "{name} (V2) names its recorded files"
        );
    }
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

    for (id, declared_kind) in inventory {
        for (generation, (kind, detail)) in [
            ("V1", v1(id, json!({}), Value::Null)),
            ("V2", v2(id, json!({}), Value::Null)),
        ] {
            assert_eq!(kind, declared_kind, "{id} ({generation}) keeps its kind");
            assert_ne!(
                detail,
                ToolDetail::Opaque,
                "{id} ({generation}) fell into the opaque default"
            );
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

/// Captured from `7c9859da` (see [`V1_BASELINE_CARD`]).
const V2_BASELINE_CARD: &str = r#"{"body":{"elements":[{"element_id":"tool_call","elements":[{"tag":"button","text":{"content":"📂 打开","tag":"plain_text"},"type":"default","value":{"action":"file","path":"src/a.rs","session_id":"ses_1"}},{"content":"**Input**\n📄 `src/a.rs`\n\n**Output**\n\n+1 −1\n\n```plain_text\n@@ -1 +1 @@\n-old\n+new\n\n```","tag":"markdown"}],"expanded":false,"header":{"icon":{"tag":"standard_icon","token":"down-small-ccm_outlined"},"icon_position":"right","title":{"content":"✅ edit · 📄 a.rs","tag":"plain_text"}},"tag":"collapsible_panel"}]},"config":{"wide_screen_mode":true},"header":{"template":"green","title":{"content":"✅ 完成","tag":"plain_text"}},"schema":"2.0"}"#;

/// Captured from `7c9859da` (see [`V1_BASELINE_CARD`]).
const CODEX_BASELINE_CARD: &str = r#"{"body":{"elements":[{"element_id":"tool_call","elements":[{"content":"**Input**\n`ls -la`\n📁 `/work`\n\n**Output**\n\ntotal 0\n","tag":"markdown"}],"expanded":false,"header":{"icon":{"tag":"standard_icon","token":"down-small-ccm_outlined"},"icon_position":"right","title":{"content":"✅ 命令","tag":"plain_text"}},"tag":"collapsible_panel"}]},"config":{"wide_screen_mode":true},"header":{"template":"green","title":{"content":"✅ 完成","tag":"plain_text"}},"schema":"2.0"}"#;

/// Byte-identity guard, one producer each: a decoded wire payload renders the
/// exact card the pre-change code rendered. A renderer that started reading a
/// semantic field changes one of these.
#[test]
fn a_decoded_v1_bash_renders_the_baseline_card() {
    let call = v1_call(
        "bash",
        json!({"command": "ls -la", "workdir": "/work"}),
        json!({"exit": 0}),
        Some("total 0\n"),
    );
    assert_eq!(render_card(call), V1_BASELINE_CARD);
}

#[test]
fn a_decoded_v2_edit_renders_the_baseline_card() {
    let call = v2_call(
        "edit",
        json!({"path": "src/a.rs", "oldString": "old", "newString": "new"}),
        json!({"files": [{"file": "src/a.rs", "patch": "@@ -1 +1 @@\n-old\n+new"}]}),
        Some("Edited src/a.rs"),
    );
    assert_eq!(render_card(call), V2_BASELINE_CARD);
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
