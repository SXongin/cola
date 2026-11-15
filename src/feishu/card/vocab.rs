//! ADR-0107's vocabulary: the card's **tokens** and the shared **copy**.
//!
//! A card-authored string is one of two kinds, decided by the question "would
//! a translator touch it?":
//!
//! - a **Token** is domain vocabulary — tool nouns, live phase words. It stays
//!   English when a locale arrives, never enters a translation catalog, and is
//!   derived here rather than written by a renderer;
//! - **Copy** is interface text — titles, sentences, buttons, units, section
//!   labels. It is Chinese today and becomes the `zh-CN` catalog when i18n
//!   lands.
//!
//! Data (ids, paths, command lines, model output, vendor titles) is
//! **Verbatim**: never classified here, never translated, never re-worded.
//!
//! This module owns every token constant and the functions that derive one
//! from the read model ([`tool_token`], [`phase_token`]), the run-outcome copy
//! ([`run_outcome_copy`]), and the copy constants a second module would
//! otherwise duplicate ([`INPUT_LABEL`], [`OUTPUT_LABEL`], the waits). A
//! renderer writes no token as a string literal; the guard test at the bottom
//! of this file enforces it.

use crate::backend::{Phase, SearchScope, ToolDetail, ToolIdentity, ToolKind};
use crate::bridge::tasks::model::TaskEnding;

// The closed tool-token set (ADR-0107's table). One constant per token, so a
// rename edits this module and the compiler finds the callers.
pub(crate) const TOOL_SHELL: &str = "shell";
pub(crate) const TOOL_READ: &str = "read";
pub(crate) const TOOL_WRITE: &str = "write";
pub(crate) const TOOL_EDIT: &str = "edit";
pub(crate) const TOOL_GLOB: &str = "glob";
pub(crate) const TOOL_GREP: &str = "grep";
pub(crate) const TOOL_FETCH: &str = "fetch";
pub(crate) const TOOL_WEBSEARCH: &str = "websearch";
pub(crate) const TOOL_SUBAGENT: &str = "subagent";
pub(crate) const TOOL_SCRIPT: &str = "script";
pub(crate) const TOOL_PLAN: &str = "plan";
pub(crate) const TOOL_SKILL: &str = "skill";
pub(crate) const TOOL_ASK: &str = "ask";

/// The live phase slot's three tokens (ADR-0107 §3). They serve the card
/// header and the child-agent activity line, so one run cannot read as two
/// different phases on two surfaces.
pub(crate) const PHASE_WAITING: &str = "waiting";
pub(crate) const PHASE_REASONING: &str = "reasoning";
pub(crate) const PHASE_RESPONDING: &str = "responding";

/// Section labels are copy, not tokens (ADR-0107 §5): the German-style
/// structural labels in Chinese prose were the defect.
pub(crate) const INPUT_LABEL: &str = "输入";
pub(crate) const OUTPUT_LABEL: &str = "输出";

/// The wait words (`等待你的授权` / `等待你的回答`) the header title and the
/// liveness line share (ADR-0054). Copy, because a translator would touch
/// them; one constant because two surfaces render them.
pub(crate) const AWAITING_PERMISSION: &str = "等待你的授权";
pub(crate) const AWAITING_QUESTION: &str = "等待你的回答";
pub(crate) const AWAITING_BOTH: &str = "等待你的授权/回答";

/// A run's ending word (ADR-0107 §4): copy, never a token, because a
/// translator would touch it. `TaskEnding` is wider than the client's
/// `RunStatus` — a runtime-confirmed end and a user cleanup have no client
/// status — so the mapping lives on the ending layer, not on `RunStatus`.
pub(crate) const RUN_COMPLETED: &str = "完成";
pub(crate) const RUN_FAILED: &str = "失败";
pub(crate) const RUN_CANCELLED: &str = "已取消";
pub(crate) const RUN_ENDED: &str = "结束";
pub(crate) const RUN_LOST: &str = "已失联";
pub(crate) const RUN_CLEANED: &str = "已清理";

/// The ledger section's title word (ADR-0060). Copy: the widened concept
/// ("live runs") keeps `后台任务` as its heading for now (spec #1057's P4).
pub(crate) const LEDGER_TITLE: &str = "后台任务";

/// The tool token for one call: ADR-0107's closed table, derived from
/// `(kind, detail)` first and the vendor name last.
///
/// The name is read in exactly one place — the `Other` + `None` arm, where a
/// closed three-entry table maps `todowrite`/`skill`/`question` to
/// `plan`/`skill`/`ask` (OpenCode is their only producer, so they have no
/// shared shape). Everywhere else the name is only the opaque fallback: a
/// vendor tool keeps its **full** vendor name, namespace included (ADR-0042).
/// It never selects a renderer.
///
/// The return borrows the identity for the opaque case, so the lifetime is
/// the identity's rather than `'static`: the derived tokens are `'static`
/// constants and coerce to it.
pub(crate) fn tool_token<'a>(identity: &'a ToolIdentity, kind: ToolKind, detail: &ToolDetail) -> &'a str {
    match (kind, detail) {
        (ToolKind::Execute, ToolDetail::Command { .. }) => TOOL_SHELL,
        (ToolKind::Execute, ToolDetail::Program { .. }) => TOOL_SCRIPT,
        (ToolKind::Edit, ToolDetail::FileEdits(edits)) => {
            // `write` needs a non-empty list whose every entry is an add: an
            // empty list is not "nothing changed", it is an edit with no
            // recorded files.
            if !edits.is_empty()
                && edits
                    .iter()
                    .all(|edit| edit.action == crate::backend::FileAction::Add)
            {
                TOOL_WRITE
            } else {
                TOOL_EDIT
            }
        }
        (ToolKind::Search, ToolDetail::Search { scope, .. }) => match scope {
            SearchScope::FileName => TOOL_GLOB,
            SearchScope::Content => TOOL_GREP,
            SearchScope::Web => TOOL_WEBSEARCH,
        },
        (ToolKind::Read, ToolDetail::Read { .. }) => TOOL_READ,
        (ToolKind::Fetch, ToolDetail::None) => TOOL_FETCH,
        (
            ToolKind::Other,
            ToolDetail::Delegation {
                effect: crate::backend::DelegationEffect::Launch,
                ..
            },
        ) => TOOL_SUBAGENT,
        // The closed name-only token set (ADR-0107's one exception).
        (ToolKind::Other, ToolDetail::None) => match identity.name.as_str() {
            "todowrite" => TOOL_PLAN,
            "skill" => TOOL_SKILL,
            "question" => TOOL_ASK,
            _ => identity.name.as_str(),
        },
        // A shape on the wrong kind, or a vendor tool: opaque.
        _ => identity.name.as_str(),
    }
}

/// The live phase slot's token (ADR-0107 §3): `waiting` before anything
/// streams, `reasoning` while a reasoning part does, `responding` while reply
/// text does.
pub(crate) fn phase_token(phase: Phase) -> &'static str {
    match phase {
        Phase::Waiting => PHASE_WAITING,
        Phase::Reasoning => PHASE_REASONING,
        Phase::Responding => PHASE_RESPONDING,
    }
}

/// The word a `TaskEnding` contributes to a completion entry (ADR-0080's
/// ending layer, ADR-0107 §4). Never derived from the client's `RunStatus`.
pub(crate) fn run_outcome_copy(ending: &TaskEnding) -> &'static str {
    match ending {
        TaskEnding::Wake { state } if state.as_deref() == Some("cancelled") => RUN_CANCELLED,
        TaskEnding::Wake { state } if state.as_deref() == Some("error") => RUN_FAILED,
        TaskEnding::Wake { .. } => RUN_COMPLETED,
        TaskEnding::RuntimeEnded => RUN_ENDED,
        TaskEnding::Lost => RUN_LOST,
        TaskEnding::Cleaned => RUN_CLEANED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        AgentRunRef, DelegationEffect, FileAction, FileEdit, NestedCall, SearchScope, ToolDetail, ToolKind,
    };

    fn identity(name: &str) -> ToolIdentity {
        ToolIdentity {
            name: name.to_string(),
            call_id: "call_test".to_string(),
        }
    }

    fn token(name: &str, kind: ToolKind, detail: ToolDetail) -> String {
        tool_token(&identity(name), kind, &detail).to_string()
    }

    fn edits(actions: &[FileAction]) -> ToolDetail {
        ToolDetail::FileEdits(
            actions
                .iter()
                .enumerate()
                .map(|(i, action)| FileEdit {
                    path: format!("f{i}.rs"),
                    action: *action,
                    rename_to: None,
                    diff: None,
                })
                .collect(),
        )
    }

    fn command() -> ToolDetail {
        ToolDetail::Command {
            command: "ls -la".to_string(),
            cwd: None,
            description: None,
            exit: None,
            timeout: false,
        }
    }

    fn program() -> ToolDetail {
        ToolDetail::Program {
            source: "await tools.read({})".to_string(),
            language: None,
            calls: Vec::<NestedCall>::new(),
            error: false,
        }
    }

    fn read() -> ToolDetail {
        ToolDetail::Read {
            path: Some("/x".to_string()),
            preview: None,
            directory: false,
        }
    }

    fn search(scope: SearchScope) -> ToolDetail {
        ToolDetail::Search {
            query: None,
            matches: None,
            scope,
        }
    }

    /// ADR-0107's derivation table, one case per shared shape. One concept can
    /// come from several ids: the token follows `(kind, detail)`, never the
    /// name.
    #[test]
    fn the_token_table_is_per_shared_shape() {
        // Execute: a command is `shell`, Code Mode's program is `script`.
        for name in ["bash", "shell", "codex/command_execution"] {
            assert_eq!(
                token(name, ToolKind::Execute, command()),
                TOOL_SHELL,
                "{name} is a shell-like command"
            );
        }
        for name in ["execute", "codemode", "exec", "execute_tools", "code_execution"] {
            assert_eq!(
                token(name, ToolKind::Execute, program()),
                TOOL_SCRIPT,
                "{name} is a Code Mode program"
            );
        }

        // Read absorbs OpenCode's `read` and Codex's `imageView`; a generated
        // image has no read shape and stays opaque.
        assert_eq!(token("read", ToolKind::Read, read()), TOOL_READ);
        assert_eq!(token("codex/image_view", ToolKind::Read, read()), TOOL_READ);
        assert_eq!(
            token("codex/imageGeneration", ToolKind::Other, ToolDetail::Opaque),
            "codex/imageGeneration",
            "a generated image is not a read"
        );

        // FileEdits: an all-Add list is `write`; an empty or mixed list is
        // `edit`.
        assert_eq!(
            token("write", ToolKind::Edit, edits(&[FileAction::Add])),
            TOOL_WRITE
        );
        assert_eq!(
            token(
                "patch",
                ToolKind::Edit,
                edits(&[FileAction::Add, FileAction::Add])
            ),
            TOOL_WRITE
        );
        assert_eq!(
            token("edit", ToolKind::Edit, edits(&[])),
            TOOL_EDIT,
            "an empty FileEdits list is an edit, not a write"
        );
        assert_eq!(
            token(
                "apply_patch",
                ToolKind::Edit,
                edits(&[FileAction::Add, FileAction::Update])
            ),
            TOOL_EDIT,
            "a mixed list is an edit"
        );
        assert_eq!(
            token("edit", ToolKind::Edit, edits(&[FileAction::Delete])),
            TOOL_EDIT
        );

        // Search: the scope decides the token.
        assert_eq!(
            token("glob", ToolKind::Search, search(SearchScope::FileName)),
            TOOL_GLOB
        );
        assert_eq!(
            token("grep", ToolKind::Search, search(SearchScope::Content)),
            TOOL_GREP
        );
        assert_eq!(
            token("codex/web_search", ToolKind::Search, search(SearchScope::Web)),
            TOOL_WEBSEARCH
        );

        // Fetch and a launching delegation.
        assert_eq!(token("webfetch", ToolKind::Fetch, ToolDetail::None), TOOL_FETCH);
        assert_eq!(
            token(
                "task",
                ToolKind::Other,
                ToolDetail::Delegation {
                    runs: vec![AgentRunRef {
                        run_id: "run_1".to_string()
                    }],
                    effect: DelegationEffect::Launch,
                }
            ),
            TOOL_SUBAGENT
        );
        assert_eq!(
            token(
                "spawnAgent",
                ToolKind::Other,
                ToolDetail::Delegation {
                    runs: vec![],
                    effect: DelegationEffect::Launch,
                }
            ),
            TOOL_SUBAGENT
        );
        // An addressed/reported run is not a subagent token: its call keeps
        // its vendor name.
        for name in ["wait", "sendInput", "subAgentActivity"] {
            assert_eq!(
                token(
                    name,
                    ToolKind::Other,
                    ToolDetail::Delegation {
                        runs: vec![],
                        effect: DelegationEffect::Address,
                    }
                ),
                name
            );
        }
    }

    /// The three name-only tokens are the closed exception; the name is read
    /// nowhere else.
    #[test]
    fn the_three_name_only_tokens_are_closed() {
        assert_eq!(token("todowrite", ToolKind::Other, ToolDetail::None), TOOL_PLAN);
        assert_eq!(token("skill", ToolKind::Other, ToolDetail::None), TOOL_SKILL);
        assert_eq!(token("question", ToolKind::Other, ToolDetail::None), TOOL_ASK);
    }

    /// A vendor id is never tokenized: the opaque fallback keeps the **full**
    /// name, namespace included (ADR-0042), and a Detail on the wrong kind
    /// gets the same treatment.
    #[test]
    fn an_opaque_id_is_not_tokenized() {
        for name in ["my_server/my_tool", "codegraph_explore", "mystery"] {
            assert_eq!(token(name, ToolKind::Other, ToolDetail::Opaque), name);
            assert_eq!(token(name, ToolKind::Other, ToolDetail::None), name);
        }
        // A `Command` detail on a non-Execute kind is a lying adapter, not an
        // execute call.
        assert_eq!(
            token("bash", ToolKind::Read, command()),
            "bash",
            "the shape and the kind must agree"
        );
    }

    /// The live phase slot: three distinct words, shared by every surface that
    /// renders a phase.
    #[test]
    fn the_phase_tokens_are_three_distinct_words() {
        assert_eq!(phase_token(Phase::Waiting), PHASE_WAITING);
        assert_eq!(phase_token(Phase::Reasoning), PHASE_REASONING);
        assert_eq!(phase_token(Phase::Responding), PHASE_RESPONDING);
        assert_ne!(PHASE_WAITING, PHASE_REASONING);
        assert_ne!(PHASE_REASONING, PHASE_RESPONDING);
    }

    /// Every `TaskEnding` renders a word, and the client's `RunStatus` never
    /// supplies one (ADR-0080/0107 §4).
    #[test]
    fn every_ending_has_its_own_word() {
        let cases = [
            (TaskEnding::Wake { state: None }, RUN_COMPLETED),
            (
                TaskEnding::Wake {
                    state: Some("completed".to_string()),
                },
                RUN_COMPLETED,
            ),
            (
                TaskEnding::Wake {
                    state: Some("cancelled".to_string()),
                },
                RUN_CANCELLED,
            ),
            (
                TaskEnding::Wake {
                    state: Some("error".to_string()),
                },
                RUN_FAILED,
            ),
            (TaskEnding::RuntimeEnded, RUN_ENDED),
            (TaskEnding::Lost, RUN_LOST),
            (TaskEnding::Cleaned, RUN_CLEANED),
        ];
        let mut words = std::collections::BTreeSet::new();
        for (ending, expected) in &cases {
            assert_eq!(run_outcome_copy(ending), *expected, "{ending:?}");
            words.insert(*expected);
        }
        assert_eq!(words.len(), 6, "the six endings are six distinct words");
    }

    /// ADR-0107 §6: a renderer writes no token as a string literal. The scan
    /// strips the test module and comments, then looks for a token in a
    /// display position — a match arm's result, a `Some`, a binding, a
    /// `format!`/`push_str`, a `to_string`, a `return`. A vendor id compared
    /// against a name (`name == "subagent"`) is not a display position and is
    /// the one legitimate spelling.
    ///
    /// Two files hold non-card ids that happen to spell a token: `lang.rs` maps
    /// file extensions to **fence languages** (`zsh` → `shell`) and
    /// `picker.rs` compares the agent **wire mode** `subagent`. Neither renders
    /// a card token, so both are exempt — a new exemption needs the same
    /// argument.
    #[test]
    fn no_renderer_writes_a_token_literal() {
        const TOKENS: &[&str] = &[
            TOOL_SHELL,
            TOOL_READ,
            TOOL_WRITE,
            TOOL_EDIT,
            TOOL_GLOB,
            TOOL_GREP,
            TOOL_FETCH,
            TOOL_WEBSEARCH,
            TOOL_SUBAGENT,
            TOOL_SCRIPT,
            TOOL_PLAN,
            TOOL_SKILL,
            TOOL_ASK,
            PHASE_WAITING,
            PHASE_REASONING,
            PHASE_RESPONDING,
        ];
        const EXEMPT: &[&str] = &["vocab.rs", "lang.rs", "picker.rs"];
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/feishu/card");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).expect("the card module is readable") {
            let path = entry.expect("a directory entry").path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string();
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") || EXEMPT.contains(&name.as_str())
            {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("a card source file is readable");
            // The test module is last: everything from `mod tests {` on is
            // fixtures and assertions, not renderer code.
            let code = source
                .split("\nmod tests {")
                .next()
                .expect("the split always yields a first part");
            for (index, line) in code.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") {
                    continue;
                }
                for token in TOKENS {
                    let quoted = format!("\"{token}\"");
                    let displays = [
                        format!("=> {quoted}"),
                        format!("Some({quoted})"),
                        format!(" = {quoted}"),
                        format!("{quoted}.to_string()"),
                        format!("format!(\"{token}"),
                        format!("push_str({quoted}"),
                        format!("return {quoted}"),
                        format!("{quoted};"),
                    ];
                    assert!(
                        !displays.iter().any(|pattern| line.contains(pattern)),
                        "{}:{} writes the token {token:?} as a literal; move it into vocab.rs\n  {line}",
                        name,
                        index + 1
                    );
                }
            }
            checked += 1;
        }
        assert!(checked > 10, "the guard actually scanned the card module");
    }
}
