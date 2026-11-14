//! The neutral tool semantics both OpenCode generations declare (spec #1057
//! P1, ADR-0105).
//!
//! The two generations spell a few ids differently (`bash`/`shell`,
//! `task`/`subagent`, V2's `execute`/`patch`) but the meaning of a shared id is
//! the same on both. One table is the single source of truth: it names every
//! shared id the adapter claims and the function that derives its detail, so an
//! id cannot exist in the mapping without being in the inventory the conformance
//! test iterates (`tool_conformance::every_declared_shared_id_is_mapped`).
//!
//! A third-party id (an MCP/plugin tool) is `Other`/`Opaque`: the adapter never
//! sniffs a payload it does not understand (ADR-0042).

use serde_json::Value;

use crate::backend::{
    DelegationEffect, FileAction, FileEdit, NestedCall, SearchScope, ToolCall, ToolDetail, ToolIdentity,
    ToolKind, ToolOutput, ToolStatus,
};

/// One shared id the adapter claims: the kind it always carries and the
/// function that derives its detail from the call's raw state payloads.
struct SharedTool {
    id: &'static str,
    kind: ToolKind,
    detail: fn(Option<&Value>, Option<&Value>) -> ToolDetail,
}

/// Every shared tool id this adapter maps. The mapping and the inventory are
/// the same table by construction; the conformance test iterates it and fails
/// if any id falls through to the opaque third-party default.
const SHARED_TOOLS: &[SharedTool] = &[
    // V1's `bash` and V2's `shell` are the same concept under two ids.
    SharedTool {
        id: "bash",
        kind: ToolKind::Execute,
        detail: command_detail,
    },
    SharedTool {
        id: "shell",
        kind: ToolKind::Execute,
        detail: command_detail,
    },
    SharedTool {
        id: "read",
        kind: ToolKind::Read,
        detail: read_detail,
    },
    SharedTool {
        id: "write",
        kind: ToolKind::Edit,
        detail: write_detail,
    },
    SharedTool {
        id: "edit",
        kind: ToolKind::Edit,
        detail: edit_detail,
    },
    SharedTool {
        id: "patch",
        kind: ToolKind::Edit,
        detail: patch_detail,
    },
    SharedTool {
        id: "apply_patch",
        kind: ToolKind::Edit,
        detail: patch_detail,
    },
    SharedTool {
        id: "glob",
        kind: ToolKind::Search,
        detail: glob_detail,
    },
    SharedTool {
        id: "grep",
        kind: ToolKind::Search,
        detail: grep_detail,
    },
    SharedTool {
        id: "websearch",
        kind: ToolKind::Search,
        detail: websearch_detail,
    },
    SharedTool {
        id: "webfetch",
        kind: ToolKind::Fetch,
        detail: fetch_detail,
    },
    // A launch of a child agent: the run itself is the AgentRun ticket's fact;
    // P1 only declares the shape and the verb.
    SharedTool {
        id: "task",
        kind: ToolKind::Other,
        detail: delegation_detail,
    },
    SharedTool {
        id: "subagent",
        kind: ToolKind::Other,
        detail: delegation_detail,
    },
    // V2's Code Mode entry: the program it ran, plus the nested calls it made
    // (recorded live in the call's metadata).
    SharedTool {
        id: "execute",
        kind: ToolKind::Execute,
        detail: program_detail,
    },
    // Known shared tools with no extra shape to declare.
    SharedTool {
        id: "todowrite",
        kind: ToolKind::Other,
        detail: no_detail,
    },
    SharedTool {
        id: "skill",
        kind: ToolKind::Other,
        detail: no_detail,
    },
    SharedTool {
        id: "question",
        kind: ToolKind::Other,
        detail: no_detail,
    },
];

/// The kind and shared detail an OpenCode tool id declares. `input` and
/// `metadata` are the call's raw state payloads, read tolerantly: a missing
/// field yields the detail's empty value, never a decode failure.
pub(crate) fn tool_semantics(
    name: &str,
    input: Option<&Value>,
    metadata: Option<&Value>,
) -> (ToolKind, ToolDetail) {
    match SHARED_TOOLS.iter().find(|tool| tool.id == name) {
        Some(tool) => (tool.kind, (tool.detail)(input, metadata)),
        // An id the adapter does not know — an MCP/plugin tool — stays opaque
        // and is never sniffed.
        None => (ToolKind::Other, ToolDetail::Opaque),
    }
}

/// The shared-id inventory the conformance suite iterates: every id the table
/// claims, with the kind it declares. A row with no arm is impossible (the
/// table *is* the mapping); a table row that falls into the opaque default is
/// what the suite catches.
#[cfg(test)]
pub(crate) fn shared_tool_inventory() -> Vec<(&'static str, ToolKind)> {
    SHARED_TOOLS.iter().map(|tool| (tool.id, tool.kind)).collect()
}

/// The two raw payloads a decode site read: the call's structured input and its
/// recorded metadata. Bundled so a call's construction does not grow one
/// positional argument per payload (and so a future payload is one field, not a
/// new parameter at every call site).
pub(crate) struct ToolPayloads {
    pub(crate) input: Option<Value>,
    pub(crate) metadata: Option<Value>,
}

impl ToolPayloads {
    pub(crate) fn new(input: Option<Value>, metadata: Option<Value>) -> Self {
        Self { input, metadata }
    }
}

impl ToolCall {
    /// One decoded call with its semantics attached. The two OpenCode decoders
    /// call this so the kind, the derived locations and the detail are written
    /// in one place rather than repeated per decode site. Codex derives the
    /// same fields in `Item::tool_part`, where its typed item replaces the raw
    /// payloads.
    pub(crate) fn declared(
        identity: ToolIdentity,
        title: Option<String>,
        status: ToolStatus,
        started_at: Option<i64>,
        payloads: ToolPayloads,
        output: ToolOutput,
    ) -> Self {
        let ToolPayloads { input, metadata } = payloads;
        let (kind, detail) = tool_semantics(&identity.name, input.as_ref(), metadata.as_ref());
        let locations = detail.locations();
        Self {
            identity,
            kind,
            title,
            locations,
            status,
            started_at,
            input,
            metadata,
            output,
            detail,
        }
    }
}

/// A command-line call: the command, its directory, the recorded exit code, and
/// the timeout flag the client records on a timed-out run.
fn command_detail(input: Option<&Value>, metadata: Option<&Value>) -> ToolDetail {
    ToolDetail::Command {
        command: text(input, "command").unwrap_or_default(),
        cwd: text(input, "workdir").or_else(|| text(input, "cwd")),
        description: text(input, "description"),
        exit: metadata
            .and_then(|metadata| metadata.get("exit"))
            .and_then(Value::as_i64),
        timeout: metadata
            .and_then(|metadata| metadata.get("timeout"))
            .and_then(Value::as_bool)
            == Some(true),
    }
}

/// A file read. V1 names the path `filePath`, V2 names it `path`.
fn read_detail(input: Option<&Value>, _metadata: Option<&Value>) -> ToolDetail {
    ToolDetail::Read {
        path: input_path(input),
        preview: None,
        directory: false,
    }
}

/// A `write`'s file: the metadata's `FileDiff` list when V2 recorded one
/// (its diff rides there), otherwise the input's structured path.
///
/// The action stays `Add` even over a recorded status: ticket #1058's table
/// says `write` is `FileEdits` with one `add`, and the status field is the
/// diff entry's own label rather than the tool's semantic action.
fn write_detail(input: Option<&Value>, metadata: Option<&Value>) -> ToolDetail {
    let recorded = metadata_files(metadata, Some(FileAction::Add));
    if !recorded.is_empty() {
        return ToolDetail::FileEdits(recorded);
    }
    ToolDetail::FileEdits(one_edit(input, FileAction::Add, metadata_diff(metadata)))
}

/// An `edit`'s files. The card's `diff_from_metadata` prefers a top-level
/// `metadata.diff` over `metadata.files[]`; the detail mirrors that precedence,
/// so a payload carrying both describes the change the panel actually renders.
fn edit_detail(input: Option<&Value>, metadata: Option<&Value>) -> ToolDetail {
    if let Some(diff) = metadata_diff(metadata) {
        let edits = one_edit(input, FileAction::Update, Some(diff));
        if !edits.is_empty() {
            return ToolDetail::FileEdits(edits);
        }
    }
    let recorded = metadata_files(metadata, None);
    if !recorded.is_empty() {
        return ToolDetail::FileEdits(recorded);
    }
    ToolDetail::FileEdits(one_edit(input, FileAction::Update, None))
}

/// A `patch`/`apply_patch`. V2 records the touched files in
/// `metadata.files[]` (`FileDiff.Info = {file, patch, status}`); V1 records one
/// `{filepath, diff}` pair. Both name the change the panel renders. A diff with
/// no name at all is not a `FileEdit` (a path-less entry would be wrong), so the
/// patch TEXT's parse that names it stays P3's.
fn patch_detail(_input: Option<&Value>, metadata: Option<&Value>) -> ToolDetail {
    let recorded = metadata_files(metadata, None);
    if !recorded.is_empty() {
        return ToolDetail::FileEdits(recorded);
    }
    let path = metadata
        .and_then(|metadata| metadata.get("filepath"))
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty());
    match (path, metadata_diff(metadata)) {
        (Some(path), diff) => ToolDetail::FileEdits(vec![FileEdit {
            path: path.to_string(),
            action: FileAction::Update,
            rename_to: None,
            diff,
        }]),
        _ => ToolDetail::FileEdits(Vec::new()),
    }
}

fn glob_detail(input: Option<&Value>, metadata: Option<&Value>) -> ToolDetail {
    search_detail(input, metadata, SearchScope::FileName)
}

fn grep_detail(input: Option<&Value>, metadata: Option<&Value>) -> ToolDetail {
    search_detail(input, metadata, SearchScope::Content)
}

fn websearch_detail(input: Option<&Value>, metadata: Option<&Value>) -> ToolDetail {
    search_detail(input, metadata, SearchScope::Web)
}

fn fetch_detail(_input: Option<&Value>, _metadata: Option<&Value>) -> ToolDetail {
    ToolDetail::None
}

fn delegation_detail(_input: Option<&Value>, _metadata: Option<&Value>) -> ToolDetail {
    ToolDetail::Delegation {
        runs: Vec::new(),
        effect: DelegationEffect::Launch,
    }
}

fn no_detail(_input: Option<&Value>, _metadata: Option<&Value>) -> ToolDetail {
    ToolDetail::None
}

/// The structured input path: V1 names it `filePath`, V2 `path`.
fn input_path(input: Option<&Value>) -> Option<String> {
    text(input, "filePath").or_else(|| text(input, "path"))
}

/// One edited file for a single-file tool (`write`, `edit`).
fn one_edit(input: Option<&Value>, action: FileAction, diff: Option<String>) -> Vec<FileEdit> {
    let Some(path) = input_path(input) else {
        return Vec::new();
    };
    vec![FileEdit {
        path,
        action,
        rename_to: None,
        diff,
    }]
}

/// The top-level unified diff a payload records (`metadata.diff`; V1's shape).
fn metadata_diff(metadata: Option<&Value>) -> Option<String> {
    metadata
        .and_then(|metadata| metadata.get("diff"))
        .and_then(Value::as_str)
        .filter(|diff| !diff.is_empty())
        .map(str::to_string)
}

/// V2's recorded `FileDiff.Info` list (`metadata.files[]`, one `{file, patch,
/// status}` per touched file). `action` fixes the action when the caller knows
/// it; a `None` action takes it from the file's recorded `status`.
fn metadata_files(metadata: Option<&Value>, action: Option<FileAction>) -> Vec<FileEdit> {
    let Some(files) = metadata
        .and_then(|metadata| metadata.get("files"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    files
        .iter()
        .filter_map(|file| {
            let path = file
                .get("file")
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty())?
                .to_string();
            Some(FileEdit {
                path,
                action: action.unwrap_or_else(|| file_action(file.get("status").and_then(Value::as_str))),
                rename_to: None,
                diff: non_empty(file.get("patch").and_then(Value::as_str)),
            })
        })
        .collect()
}

/// A `FileDiff.Info.status` as a file action; an unknown or absent status is an
/// update (the only action a diff entry without a marker supports).
fn file_action(status: Option<&str>) -> FileAction {
    match status {
        Some("added") => FileAction::Add,
        Some("deleted") => FileAction::Delete,
        _ => FileAction::Update,
    }
}

/// A search's query, recorded match count and scope. The count is whichever
/// key the generation wrote (`count` for `glob`, `matches` for `grep`).
fn search_detail(input: Option<&Value>, metadata: Option<&Value>, scope: SearchScope) -> ToolDetail {
    let matches = metadata.and_then(|metadata| {
        metadata
            .get("matches")
            .or_else(|| metadata.get("count"))
            .and_then(Value::as_u64)
    });
    ToolDetail::Search {
        query: match scope {
            SearchScope::Web => text(input, "query"),
            _ => text(input, "pattern"),
        },
        matches,
        scope,
    }
}

/// Code Mode's program: its source and the nested calls the client recorded in
/// the metadata. The real `CodeMode.Input` carries no language, so the field
/// stays `None` until an adapter's wire shape declares one.
///
/// `error` is the wire's `metadata.error`, which upstream writes only on a failed
/// program and always as the boolean `true` — the official client's own test is
/// `metadata.error === true`. It is read as exactly that flag: a `true` is a
/// failure, and a message-shaped or otherwise non-boolean value is not one this
/// build can name (the program's diagnostic reaches the card through its output
/// text, so nothing is silently dropped from what the reader can see).
fn program_detail(input: Option<&Value>, metadata: Option<&Value>) -> ToolDetail {
    let calls = metadata
        .and_then(|metadata| metadata.get("toolCalls"))
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .map(|call| NestedCall {
                    name: call
                        .get("tool")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_string(),
                    status: nested_status(call.get("status").and_then(Value::as_str)),
                    input: call.get("input").cloned(),
                })
                .collect()
        })
        .unwrap_or_default();
    ToolDetail::Program {
        source: text(input, "code").unwrap_or_default(),
        language: None,
        calls,
        error: metadata
            .and_then(|metadata| metadata.get("error"))
            .and_then(Value::as_bool)
            == Some(true),
    }
}

/// A nested Code Mode call's status, in the same spellings the top-level tool
/// statuses use.
fn nested_status(status: Option<&str>) -> ToolStatus {
    match status {
        Some("pending") | Some("streaming") => ToolStatus::Pending,
        Some("running") | Some("inProgress") => ToolStatus::Running,
        Some("completed") => ToolStatus::Completed,
        Some("error") | Some("failed") => ToolStatus::Error,
        Some(other) => ToolStatus::Other(other.to_string()),
        None => ToolStatus::Unknown,
    }
}

/// A non-empty string field.
fn text(value: Option<&Value>, key: &str) -> Option<String> {
    value
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value.filter(|value| !value.is_empty()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The two-key path rule: V1's `filePath` and V2's `path` both name the file.
    #[test]
    fn a_file_path_reads_from_either_generation_key() {
        for input in [json!({"filePath": "src/a.rs"}), json!({"path": "src/a.rs"})] {
            assert_eq!(
                read_detail(Some(&input), None),
                ToolDetail::Read {
                    path: Some("src/a.rs".to_string()),
                    preview: None,
                    directory: false,
                }
            );
        }
        assert_eq!(
            one_edit(Some(&json!({"path": "src/a.rs"})), FileAction::Add, None),
            vec![FileEdit {
                path: "src/a.rs".to_string(),
                action: FileAction::Add,
                rename_to: None,
                diff: None,
            }]
        );
        // A path-less write declares the variant but names no file.
        assert!(one_edit(Some(&json!({"content": "x"})), FileAction::Add, None).is_empty());
        assert!(one_edit(None, FileAction::Add, None).is_empty());
    }

    /// A command's timeout, exit and cwd come from the recorded state, and
    /// `cwd` falls back to the input's `cwd`. V1's `bash` also names the call
    /// with a one-line `description`, which the shape keeps.
    #[test]
    fn a_command_reads_its_timeout_exit_and_directory() {
        assert_eq!(
            command_detail(
                Some(&json!({"command": "sleep 1", "cwd": "/work"})),
                Some(&json!({"exit": 2, "timeout": true}))
            ),
            ToolDetail::Command {
                command: "sleep 1".to_string(),
                cwd: Some("/work".to_string()),
                description: None,
                exit: Some(2),
                timeout: true,
            }
        );
        assert_eq!(
            command_detail(Some(&json!({"command": "ls", "workdir": "/w"})), None),
            ToolDetail::Command {
                command: "ls".to_string(),
                cwd: Some("/w".to_string()),
                description: None,
                exit: None,
                timeout: false,
            }
        );
        assert_eq!(
            command_detail(
                Some(&json!({"command": "ls", "description": "List the tree"})),
                None
            ),
            ToolDetail::Command {
                command: "ls".to_string(),
                cwd: None,
                description: Some("List the tree".to_string()),
                exit: None,
                timeout: false,
            }
        );
    }

    /// Code Mode's own failure is a recorded fact, not only a nested call's: a
    /// program that errored with clean nested calls still declares `error`. The
    /// wire writes it as the boolean `true` and the official client tests
    /// `=== true`, so a message-shaped value is NOT a failure this build names
    /// (its diagnostic still reaches the card as the program's output text).
    #[test]
    fn a_program_reads_its_own_error_separately_from_its_nested_calls() {
        let with = |metadata: Value| {
            let detail = program_detail(Some(&json!({"code": "return 1"})), Some(&metadata));
            let ToolDetail::Program { calls, error, .. } = detail else {
                panic!("a Code Mode call is a Program");
            };
            (calls, error)
        };

        let (calls, error) = with(json!({
            "toolCalls": [{"tool": "fetch", "status": "completed"}],
            "error": true
        }));
        assert!(error, "the program's own error is a fact");
        assert_eq!(calls.len(), 1);
        assert!(!calls[0].status.is_failure(), "no nested call failed");

        let (_, error) = with(json!({"toolCalls": [], "error": false}));
        assert!(!error, "an explicit false is not a failure");
        let (_, error) = with(json!({"toolCalls": [], "error": "boom"}));
        assert!(!error, "a non-boolean value is not the flag upstream writes");
        let (_, error) = with(json!({"toolCalls": [], "error": {"message": "boom"}}));
        assert!(
            !error,
            "a message-shaped error is not the boolean the official client checks"
        );
    }

    /// A search's count is `matches` (grep) or `count` (glob); either way it is
    /// the recorded number, not a fabricated one.
    #[test]
    fn a_search_reads_its_recorded_match_count() {
        assert_eq!(
            grep_detail(Some(&json!({"pattern": "fn"})), Some(&json!({"matches": 7}))),
            ToolDetail::Search {
                query: Some("fn".to_string()),
                matches: Some(7),
                scope: SearchScope::Content,
            }
        );
        assert_eq!(
            glob_detail(Some(&json!({"pattern": "*.rs"})), Some(&json!({"count": 3}))),
            ToolDetail::Search {
                query: Some("*.rs".to_string()),
                matches: Some(3),
                scope: SearchScope::FileName,
            }
        );
        assert_eq!(
            websearch_detail(Some(&json!({"query": "rust"})), None),
            ToolDetail::Search {
                query: Some("rust".to_string()),
                matches: None,
                scope: SearchScope::Web,
            }
        );
    }

    /// An `edit` prefers the top-level diff the panel renders, then the recorded
    /// file list, then the bare input path; an empty `files` list falls through.
    #[test]
    fn an_edit_falls_back_from_diff_to_files_to_input_path() {
        let input = json!({"path": "src/a.rs"});
        let both = json!({
            "diff": "@@ -1 +1 @@\n-a\n+b",
            "files": [{"file": "src/b.rs", "patch": "@@ -2 +2 @@\n-c\n+d", "status": "modified"}]
        });
        assert_eq!(
            edit_detail(Some(&input), Some(&both)),
            ToolDetail::FileEdits(vec![FileEdit {
                path: "src/a.rs".to_string(),
                action: FileAction::Update,
                rename_to: None,
                diff: Some("@@ -1 +1 @@\n-a\n+b".to_string()),
            }])
        );

        let files =
            json!({"files": [{"file": "src/b.rs", "patch": "@@ -2 +2 @@\n-c\n+d", "status": "modified"}]});
        assert_eq!(
            edit_detail(Some(&input), Some(&files)),
            ToolDetail::FileEdits(vec![FileEdit {
                path: "src/b.rs".to_string(),
                action: FileAction::Update,
                rename_to: None,
                diff: Some("@@ -2 +2 @@\n-c\n+d".to_string()),
            }])
        );

        assert_eq!(
            edit_detail(Some(&input), Some(&json!({"files": []}))),
            ToolDetail::FileEdits(vec![FileEdit {
                path: "src/a.rs".to_string(),
                action: FileAction::Update,
                rename_to: None,
                diff: None,
            }])
        );
    }

    /// A patch's recorded file list names each file and its diff, with the
    /// action from the recorded status.
    #[test]
    fn a_patch_reads_its_recorded_files() {
        assert_eq!(
            patch_detail(
                None,
                Some(&json!({"files": [
                    {"file": "src/a.rs", "patch": "@@ -1 +1 @@", "status": "modified"},
                    {"file": "src/b.rs", "patch": "", "status": "deleted"}
                ]}))
            ),
            ToolDetail::FileEdits(vec![
                FileEdit {
                    path: "src/a.rs".to_string(),
                    action: FileAction::Update,
                    rename_to: None,
                    diff: Some("@@ -1 +1 @@".to_string()),
                },
                FileEdit {
                    path: "src/b.rs".to_string(),
                    action: FileAction::Delete,
                    rename_to: None,
                    diff: None,
                },
            ])
        );
        // No recorded list: the patch TEXT's parse is P3's, not P1's.
        assert_eq!(
            patch_detail(Some(&json!({"patchText": "*** Begin Patch"})), None),
            ToolDetail::FileEdits(Vec::new())
        );
    }

    /// V1 records one `{filepath, diff}` pair; either half alone is not a file
    /// edit (the patch text's parse that names a path-less diff is P3's).
    #[test]
    fn a_v1_patch_reads_its_filepath_and_diff() {
        assert_eq!(
            patch_detail(
                Some(&json!({"patchText": "*** Begin Patch"})),
                Some(&json!({"filepath": "src/a.rs", "diff": "@@ -1 +1 @@\n-a\n+b"}))
            ),
            ToolDetail::FileEdits(vec![FileEdit {
                path: "src/a.rs".to_string(),
                action: FileAction::Update,
                rename_to: None,
                diff: Some("@@ -1 +1 @@\n-a\n+b".to_string()),
            }])
        );
        assert_eq!(
            patch_detail(None, Some(&json!({"filepath": "src/a.rs"}))),
            ToolDetail::FileEdits(vec![FileEdit {
                path: "src/a.rs".to_string(),
                action: FileAction::Update,
                rename_to: None,
                diff: None,
            }])
        );
        assert_eq!(
            patch_detail(None, Some(&json!({"diff": "@@ -1 +1 @@\n-a\n+b"}))),
            ToolDetail::FileEdits(Vec::new()),
            "a path-less diff waits for P3's patch-text parse"
        );
    }

    /// A recorded `FileDiff.Info.status` names the action, and an empty path is
    /// skipped rather than emitted as a nameless edit.
    #[test]
    fn a_recorded_status_names_the_action_and_empty_paths_are_skipped() {
        assert_eq!(file_action(Some("added")), FileAction::Add);
        assert_eq!(file_action(Some("deleted")), FileAction::Delete);
        assert_eq!(file_action(Some("modified")), FileAction::Update);
        assert_eq!(file_action(Some("renamed")), FileAction::Update);
        assert_eq!(file_action(None), FileAction::Update);

        let edits = metadata_files(
            Some(&json!({"files": [
                {"file": "", "patch": "ignored"},
                {"file": "src/a.rs", "patch": "@@ -1 +1 @@", "status": "added"}
            ]})),
            None,
        );
        assert_eq!(
            edits,
            vec![FileEdit {
                path: "src/a.rs".to_string(),
                action: FileAction::Add,
                rename_to: None,
                diff: Some("@@ -1 +1 @@".to_string()),
            }]
        );
    }

    /// A write's recorded file list is its add, diff included.
    #[test]
    fn a_write_prefers_a_recorded_file_diff() {
        assert_eq!(
            write_detail(
                Some(&json!({"path": "src/a.rs", "content": "hi"})),
                Some(&json!({"files": [{"file": "src/a.rs", "patch": "@@ -0 +1 @@\n+hi"}]}))
            ),
            ToolDetail::FileEdits(vec![FileEdit {
                path: "src/a.rs".to_string(),
                action: FileAction::Add,
                rename_to: None,
                diff: Some("@@ -0 +1 @@\n+hi".to_string()),
            }])
        );
    }

    /// Every nested Code Mode status maps onto the neutral arm; an unknown
    /// spelling stays verbatim and an absent one is unknown.
    #[test]
    fn nested_call_statuses_map_onto_the_neutral_arms() {
        for (wire, neutral) in [
            (Some("pending"), ToolStatus::Pending),
            (Some("streaming"), ToolStatus::Pending),
            (Some("running"), ToolStatus::Running),
            (Some("inProgress"), ToolStatus::Running),
            (Some("completed"), ToolStatus::Completed),
            (Some("error"), ToolStatus::Error),
            (Some("failed"), ToolStatus::Error),
            (Some("weird"), ToolStatus::Other("weird".to_string())),
            (None, ToolStatus::Unknown),
        ] {
            assert_eq!(nested_status(wire), neutral, "{wire:?}");
        }
    }

    /// The table's inventory is duplicate-free, and a third-party id is opaque.
    #[test]
    fn the_inventory_is_unique_and_unknown_ids_stay_opaque() {
        let mut ids: Vec<_> = shared_tool_inventory().into_iter().map(|(id, _)| id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "the shared-id table has a duplicate");
        assert_eq!(
            tool_semantics("my_server/my_tool", None, None),
            (ToolKind::Other, ToolDetail::Opaque)
        );
    }
}
