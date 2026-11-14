//! The neutral tool semantics both OpenCode generations declare (spec #1057
//! P1, ADR-0105).
//!
//! The two generations spell a few ids differently (`bash`/`shell`,
//! `task`/`subagent`, V2's `execute`/`patch`) but the meaning of a shared id is
//! the same on both. The mapping lives once, beside the two decoders that call
//! it, so a shared id cannot drift between the generations; the conformance
//! test (`tool_conformance`) pins the table and fails when an id loses its arm
//! and falls into the opaque default.
//!
//! A third-party id (an MCP/plugin tool) is `Other`/`Opaque`: the adapter never
//! sniffs a payload it does not understand (ADR-0042).

use serde_json::Value;

use crate::backend::{
    DelegationEffect, FileAction, FileEdit, NestedCall, SearchScope, ToolDetail, ToolKind, ToolStatus,
};

/// The kind and shared detail an OpenCode tool id declares. `input` and
/// `metadata` are the call's raw state payloads, read tolerantly: a missing
/// field yields the detail's empty value, never a decode failure.
pub(crate) fn tool_semantics(
    name: &str,
    input: Option<&Value>,
    metadata: Option<&Value>,
) -> (ToolKind, ToolDetail) {
    match name {
        // V1's `bash` and V2's `shell` are the same concept under two ids.
        "bash" | "shell" => (ToolKind::Execute, command_detail(input, metadata)),
        "read" => (
            ToolKind::Read,
            ToolDetail::Read {
                path: text(input, "filePath").or_else(|| text(input, "path")),
                preview: None,
                directory: false,
            },
        ),
        "write" => (
            ToolKind::Edit,
            ToolDetail::FileEdits(one_edit(input, FileAction::Add, None)),
        ),
        "edit" => (ToolKind::Edit, ToolDetail::FileEdits(edit_files(input, metadata))),
        "patch" | "apply_patch" => (ToolKind::Edit, ToolDetail::FileEdits(Vec::new())),
        "glob" => (
            ToolKind::Search,
            search_detail(input, metadata, SearchScope::FileName),
        ),
        "grep" => (
            ToolKind::Search,
            search_detail(input, metadata, SearchScope::Content),
        ),
        "websearch" => (ToolKind::Search, search_detail(input, metadata, SearchScope::Web)),
        "webfetch" => (ToolKind::Fetch, ToolDetail::None),
        // A launch of a child agent: the run itself is the AgentRun ticket's
        // fact; P1 only declares the shape and the verb.
        "task" | "subagent" => (
            ToolKind::Other,
            ToolDetail::Delegation {
                runs: Vec::new(),
                effect: DelegationEffect::Launch,
            },
        ),
        // V2's Code Mode entry: the program it ran, plus the nested calls it
        // made (recorded live in the call's metadata).
        "execute" => (ToolKind::Execute, program_detail(input, metadata)),
        // Known shared tools with no extra shape to declare.
        "todowrite" | "skill" | "question" => (ToolKind::Other, ToolDetail::None),
        // An id the adapter does not know — an MCP/plugin tool — stays opaque
        // and is never sniffed.
        _ => (ToolKind::Other, ToolDetail::Opaque),
    }
}

/// A command-line call: the command, its directory, the recorded exit code, and
/// the timeout flag the client records on a timed-out run.
fn command_detail(input: Option<&Value>, metadata: Option<&Value>) -> ToolDetail {
    ToolDetail::Command {
        command: text(input, "command").unwrap_or_default(),
        cwd: text(input, "workdir").or_else(|| text(input, "cwd")),
        exit: metadata
            .and_then(|metadata| metadata.get("exit"))
            .and_then(Value::as_i64),
        timeout: metadata
            .and_then(|metadata| metadata.get("timeout"))
            .and_then(Value::as_bool)
            == Some(true),
    }
}

/// One edited file for a single-file tool (`write`).
fn one_edit(input: Option<&Value>, action: FileAction, diff: Option<String>) -> Vec<FileEdit> {
    let Some(path) = text(input, "filePath") else {
        return Vec::new();
    };
    vec![FileEdit {
        path,
        action,
        rename_to: None,
        diff,
    }]
}

/// An `edit`'s files: V2 records `metadata.files[]` (one `{file, patch}` per
/// touched file) while V1 records one `metadata.diff` string against the input's
/// `filePath`. A no-hunk rename is V1's concern; the parsed patch actions land
/// with P3.
fn edit_files(input: Option<&Value>, metadata: Option<&Value>) -> Vec<FileEdit> {
    if let Some(files) = metadata
        .and_then(|metadata| metadata.get("files"))
        .and_then(Value::as_array)
        .filter(|files| !files.is_empty())
    {
        return files
            .iter()
            .filter_map(|file| {
                let path = file.get("file").and_then(Value::as_str)?.to_string();
                Some(FileEdit {
                    path,
                    action: FileAction::Update,
                    rename_to: None,
                    diff: non_empty(file.get("patch").and_then(Value::as_str)),
                })
            })
            .collect();
    }
    let diff = metadata
        .and_then(|metadata| metadata.get("diff"))
        .and_then(Value::as_str)
        .and_then(|diff| (!diff.is_empty()).then(|| diff.to_string()));
    one_edit(input, FileAction::Update, diff)
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

/// Code Mode's program: its source, its declared language, and the nested calls
/// the client recorded in the metadata.
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
        language: text(input, "language"),
        calls,
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
