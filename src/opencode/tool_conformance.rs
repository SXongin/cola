//! Cross-adapter tool-semantics conformance (spec #1057 P1, ADR-0105).
//!
//! One test per shared kind: the same semantic call, expressed in each
//! producer's own wire shape, must land the same `(kind, detail)`. The suite
//! drives the real decoders (V1's tool part, V2's tool item) and Codex's real
//! item mapping, so an id that loses its arm falls into `Other`/`Opaque` and
//! fails here instead of silently defaulting. A third-party id stays
//! `Other`/`Opaque` on every producer.
//!
//! The table is the ticket's adapter table: an id added to one adapter without
//! an entry here is exactly what the "unmapped shared id" rows guard.

use serde_json::{Value, json};

use super::v1::decode_tool_for_conformance as v1_tool;
use super::v2::decode_tool_for_conformance as v2_tool;
use crate::backend::{
    DelegationEffect, FileAction, FileEdit, NestedCall, SearchScope, ToolDetail, ToolKind, ToolStatus,
};
use crate::codex::read_model::tool_semantics_for_conformance as codex_semantics;

/// One V1 tool call decoded through the legacy wire decoder.
fn v1(name: &str, input: Value, metadata: Value) -> (ToolKind, ToolDetail) {
    let call = v1_tool(name, input, metadata);
    (call.kind, call.detail)
}

/// One V2 tool call decoded through the V2 wire decoder.
fn v2(name: &str, input: Value, metadata: Value) -> (ToolKind, ToolDetail) {
    let call = v2_tool(name, input, metadata);
    (call.kind, call.detail)
}

/// One Codex item's declared semantics, through the adapter's real mapping.
fn codex(item: Value) -> (ToolKind, ToolDetail) {
    codex_semantics(&item)
}

/// Execute — `bash` (V1), `shell` (V2) and `codex/command_execution` are one
/// command command.
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

/// Read — `read` and an `imageView` are one file read; an empty path is still
/// a read with `path: None`.
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
    assert_eq!(v2("read", json!({"filePath": "src/a.rs"}), Value::Null), expected);
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

/// Edit — `write` is one add, `edit` one update with its diff, and `patch`'s
/// family the same shape.
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
    assert_eq!(v2("write", json!({"filePath": "src/a.rs"}), Value::Null), add);

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
            json!({"filePath": "src/a.rs"}),
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

    let patch = (ToolKind::Edit, ToolDetail::FileEdits(Vec::new()));
    for name in ["patch", "apply_patch"] {
        assert_eq!(
            v1(name, json!({"patchText": "*** Begin Patch"}), Value::Null),
            patch,
            "{name} (V1) is a file edit"
        );
        assert_eq!(
            v2(name, json!({"patchText": "*** Begin Patch"}), Value::Null),
            patch,
            "{name} (V2) is a file edit"
        );
    }
}

/// A Codex change's action — add, delete, update, rename — comes from the wire
/// `kind` union, not a tool name.
#[test]
fn a_codex_file_change_action_is_its_wire_kind() {
    let action = |kind: Value| match codex(json!({
        "type": "fileChange",
        "id": "item-file",
        "changes": [{"path": "src/a.rs", "kind": kind, "diff": ""}]
    })) {
        (ToolKind::Edit, ToolDetail::FileEdits(edits)) => edits[0].action,
        other => panic!("a file change is an edit: {other:?}"),
    };
    assert_eq!(action(json!({"type": "add"})), FileAction::Add);
    assert_eq!(action(json!({"type": "delete"})), FileAction::Delete);
    assert_eq!(
        action(json!({"type": "update", "movePath": null})),
        FileAction::Update
    );
    assert_eq!(
        action(json!({"type": "update", "movePath": "src/b.rs"})),
        FileAction::Rename
    );
}

/// Search — each scope is one shape: `glob` filenames, `grep` contents,
/// `websearch`/`codex/web_search` the web.
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

/// Code Mode — `execute` carries the program's source and its nested calls,
/// never a field called `code`.
#[test]
fn code_mode_carries_the_program_and_its_nested_calls() {
    let (kind, detail) = v2(
        "execute",
        json!({"code": "return 1", "language": "javascript"}),
        json!({"toolCalls": [{
            "tool": "fetch",
            "status": "completed",
            "input": {"url": "https://a.example"}
        }]}),
    );
    assert_eq!(kind, ToolKind::Execute);
    assert_eq!(
        detail,
        ToolDetail::Program {
            source: "return 1".to_string(),
            language: Some("javascript".to_string()),
            calls: vec![NestedCall {
                name: "fetch".to_string(),
                status: ToolStatus::Completed,
                input: Some(json!({"url": "https://a.example"})),
            }],
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
