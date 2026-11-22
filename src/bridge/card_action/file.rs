//! The `action: "file"` clicks (#739, #742, #743, #778; ticket #1120).
//!
//! Three cards share the action tag and are told apart by `op`:
//!
//! - `op: "send"` — the File Viewer's own 「发为文件」: transfer the file's whole
//!   bytes as ONE File Message and PATCH the clicked **viewer** card, which IS
//!   the card the click landed on. Registered through [`run_send`].
//! - `op: "preview"` — a Tool Panel's 「🌐 预览网页」: the same File Message, but
//!   **never** a PATCH — the clicked message is the agent's **Turn card**, and a
//!   transfer card built for the file would overwrite the panel and the reply
//!   (#778). Registered through [`run_preview`].
//! - no `op` — a browse card's file row (open the viewer **in place**) and a
//!   Tool Panel's 「📂 打开」 (reply a standalone viewer). The viewer's own
//!   `page`/`view`/`reread` controls are this entry's other branches, selected
//!   by the `op` the body parses itself, exactly as it did in `handler.rs`.
//!   Registered as the action's `op: None` default, which the lookup only
//!   reaches when no exact op matches.
//!
//! Both transfer paths keep the load-bearing ceilings and the File Message path
//! unchanged: the 30 MB upload cap, the bounded filesystem read, the
//! deliver-once memory, and the one File Message per clicked card. Neither
//! writes a card the dispatch did not already write — the send's PATCH is the
//! viewer's own outcome, and the preview may only reply (pitfall
//! `card-repaints`, ADR-0093).

use std::sync::Arc;

use tokio::time::Instant;

use crate::bridge::card_action::VerbFuture;
use crate::bridge::core::SharedCore;
use crate::bridge::handler::{
    App, CardActionResult, OwnedReturnTo, ViewerFileOp, ViewerFileTransferStatus, ViewerOpen, ViewerPage,
    ViewerReread, parse_show_hidden, return_hidden, return_page_num, thread_key_from_value, viewer_page,
};

/// The browse-row/tool-open default entry: a browse card's **file row** (open
/// the viewer in the clicked listing card) and a Tool Panel's **「📂 打开」**
/// (reply a standalone viewer), plus the viewer's own `page`/`view`/`reread`
/// controls, which the body parses from the payload.
pub(crate) fn run<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    _deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move { app.handle_file_card_action(&app.core, value).await })
}

/// The File Viewer's 「发为文件」 (ADR-0083 §5): the 30 MB transfer must not run
/// inside the callback's 3 s budget (ADR-0078), so the click claims the card and
/// spawns the bounded transfer; it PATCHes the clicked viewer card with the
/// outcome.
pub(crate) fn run_send<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    _deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move { app.handle_viewer_file_action(value, ViewerFileOp::Send).await })
}

/// A Tool Panel's 「🌐 预览网页」 (#778): the same File Message, but the clicked
/// message is the agent's Turn card, so the transfer **never** PATCHes it — a
/// landed File Message is its own evidence and a failure is spoken as a reply.
pub(crate) fn run_preview<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    _deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move { app.handle_viewer_file_action(value, ViewerFileOp::Preview).await })
}

impl App {
    /// Handle a browse card's **file row** — and a Tool Panel's **「📂 打开」**
    /// (ADR-0083 §3, §8): read the file and answer its **File Viewer** card,
    /// which replaces the clicked card *in place* — the ack patches the
    /// clicked message, the same move as the browse card replacing the Recent
    /// Directories card, so the listing is never left as a second message.
    ///
    /// The row's payload carries the listing's own state (`dir`/`keyword`/
    /// `page`/`show_hidden`), so the viewer's 「← 返回目录」 restores it exactly.
    /// The action tag is `file`, never the browse card's `dir`: a file row
    /// offers no 选择/建话题, because declaring a Session requires a directory
    /// (the pick op refuses a file path on its own too).
    ///
    /// A tool panel's 打开 carries the Session instead of the Chat/Topic (a
    /// live Turn card knows no Chat — see
    /// [`crate::feishu::card::session::tool_file_value`]): the click resolves
    /// the Session's Chat/Topic from the SessionStore — the same read the
    /// card-action span context makes, and the rule a live card's own
    /// section-level buttons follow (`ledger::cleanup_button`) — so the viewer
    /// opens with working controls. Its structured path resolves on the Path
    /// Scope (ADR-0083 §4): `~` expands, a relative path joins the Session's
    /// directory, an absolute path passes through. A Session the store no
    /// longer knows answers an honest Toast, never a viewer whose controls
    /// route nowhere.
    async fn handle_file_card_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
    ) -> Option<CardActionResult> {
        let session_id = value
            .get("session_id")
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty());
        let op = value.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let mut thread_key = thread_key_from_value(value);
        let session_dir = {
            let sessions = self.sessions.lock().await;
            let mut session_dir = None;
            if let Some(sid) = session_id {
                if thread_key.chat_id.is_empty()
                    && let Some(resolved) = sessions.thread_for_session(sid)
                {
                    thread_key = resolved;
                }
                session_dir = sessions.directory_for_session(sid);
            }
            value
                .get("session_dir")
                .and_then(|v| v.as_str())
                .filter(|dir| !dir.is_empty())
                .map(str::to_string)
                .or(session_dir)
                .or_else(|| sessions.current_directory(&thread_key))
        };
        // The row's `path` comes straight from the browse scan, so it names a
        // filesystem entry, not free text: it is read **verbatim**. Trimming it
        // would open a different file (or none) for a name that ends in — or is
        // surrounded by — whitespace, which Unix allows (the `/cat <path>` lane
        // may still trim, because there the path IS typed by hand).
        let path = value
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if path.is_empty() {
            return Some(CardActionResult {
                card: None,
                toast: Some("无效操作".to_string()),
            });
        }
        // A tool panel's 打开 without a Session behind it — and no Chat routing
        // on the payload to fall back to — cannot route the viewer's own
        // controls: say so, and leave the card standing.
        if session_id.is_some() && thread_key.chat_id.is_empty() {
            return Some(CardActionResult {
                card: None,
                toast: Some(format!("⚠️ 打不开 `{path}`：会话已不在。")),
            });
        }
        // A tool panel's structured path resolves on the Path Scope (an
        // absolute scan path resolves to itself, so browse rows never take
        // this lane — only a payload carrying a Session does).
        let resolved = match (session_id, session_dir.as_deref()) {
            (Some(_), Some(dir)) => crate::bridge::command::lexical_normalize(
                &crate::bridge::command::expand_against(&path, std::path::Path::new(dir)),
            ),
            _ => std::path::PathBuf::from(&path),
        };
        // The listing to return to, when the row came from one
        // (`browse_file_value`). Absent — a viewer opened from a tool panel
        // (#743) — leaves the viewer without a 返回目录 control. The viewer's
        // own pager (`op: "page"`, #740) carries the same listing as `return_*`
        // — strings only, because the shared pager holds string callback
        // fields — and the TARGET viewer page as `page` (a row's `page` is the
        // LISTING's, so the op is what tells the two shapes apart).
        let (home_dir, home_keyword, home_hidden, home_page, page) =
            if matches!(op, "page" | "reread" | "view") {
                (
                    value
                        .get("return_dir")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    value
                        .get("return_keyword")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    return_hidden(value),
                    return_page_num(value),
                    viewer_page(value),
                )
            } else {
                (
                    value
                        .get("dir")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    value
                        .get("keyword")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    parse_show_hidden(value),
                    value.get("page").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize,
                    1,
                )
            };
        // The pager flip and the markdown view toggle share ONE bounded rebuild
        // (#740, #781): both re-read the file and rebuild the card at the
        // payload's `(page, view)`, so the toggle needs no second machinery. The
        // view rides the payload like the page does (ADR-0052), and an absent
        // one reads as the default.
        if matches!(op, "page" | "view") {
            let toggling = op == "view";
            let busy = if toggling {
                "正在切换视图…"
            } else {
                "正在翻页…"
            };
            let Some(open_message_id) = value
                .get("open_message_id")
                .and_then(|v| v.as_str())
                .filter(|id| !id.is_empty())
            else {
                return Some(CardActionResult {
                    card: None,
                    toast: Some("无法翻页：找不到文件查看卡。".to_string()),
                });
            };
            let Some(in_flight_guard) = self.claim_viewer_action(&thread_key, open_message_id) else {
                return Some(CardActionResult {
                    card: None,
                    toast: Some(busy.to_string()),
                });
            };
            let app = Arc::clone(self);
            let page = ViewerPage {
                thread_key,
                path: resolved.to_string_lossy().to_string(),
                session_dir: session_dir.clone(),
                card_message_id: open_message_id.to_string(),
                return_to: OwnedReturnTo {
                    dir: (!home_dir.is_empty()).then(|| home_dir.clone()),
                    keyword: home_keyword.clone(),
                    show_hidden: home_hidden,
                    page: home_page,
                },
                page,
                view: crate::feishu::card::viewer::ViewerView::from_payload(value),
                lane: crate::feishu::card::viewer::lane_from_page_payload(value),
                payload_identity: value
                    .get("viewer_file_identity")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|identity| identity.parse().ok()),
                in_flight_guard,
            };
            tokio::spawn(async move { app.run_viewer_page(page).await });
            return Some(CardActionResult {
                card: None,
                toast: Some(busy.to_string()),
            });
        }
        if op == "reread" {
            // The viewer's own Feishu message id cannot be serialized when its
            // card is built (the send returns that id afterward). ws injects
            // it from callback context; it is the target for the in-place PATCH.
            let Some(open_message_id) = value
                .get("open_message_id")
                .and_then(|v| v.as_str())
                .filter(|id| !id.is_empty())
            else {
                return Some(CardActionResult {
                    card: None,
                    toast: Some("无法重新读取：找不到文件查看卡。".to_string()),
                });
            };
            let Some(in_flight_guard) = self.claim_viewer_action(&thread_key, open_message_id) else {
                return Some(CardActionResult {
                    card: None,
                    toast: Some("正在重新读取…".to_string()),
                });
            };
            let app = Arc::clone(self);
            let card_message_id = open_message_id.to_string();
            let path = resolved.to_string_lossy().to_string();
            let rich_message_id = value
                .get("rich_message_id")
                .and_then(|v| v.as_str())
                .filter(|id| !id.is_empty())
                .map(str::to_string);
            let sent_file_identity = value
                .get("viewer_sent_file_identity")
                .and_then(serde_json::Value::as_str)
                .and_then(|identity| identity.parse().ok());
            let ambiguous_file_identity = value
                .get("viewer_ambiguous_file_identity")
                .and_then(serde_json::Value::as_str)
                .and_then(|identity| identity.parse().ok());
            let embedded_image_key = value
                .get("viewer_embedded_image_key")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let return_to = OwnedReturnTo {
                dir: (!home_dir.is_empty()).then(|| home_dir.clone()),
                keyword: home_keyword.clone(),
                show_hidden: home_hidden,
                page: home_page,
            };
            // A re-read keeps the view the reader chose (#781).
            let view = crate::feishu::card::viewer::ViewerView::from_payload(value);
            tokio::spawn(async move {
                app.run_viewer_reread(ViewerReread {
                    thread_key,
                    path,
                    session_dir: session_dir.clone(),
                    card_message_id,
                    rich_message_id,
                    sent_file_identity,
                    ambiguous_file_identity,
                    embedded_image_key,
                    return_to,
                    page,
                    view,
                    in_flight_guard,
                })
                .await;
            });
            return Some(CardActionResult {
                card: None,
                toast: Some("正在重新读取…".to_string()),
            });
        }
        // Both click paths take the same bounded background read, but their
        // payloads distinguish delivery: browse rows carry the listing state
        // (`dir`/`keyword`/`page`, or `return_*` on a viewer page), while a Tool
        // Panel carries `session_id` and no listing. The latter must reply with
        // a standalone viewer message, never PATCH the live Turn card.
        // Even a fitting text file must classify off this callback path because
        // a host read can block (e.g. a network mount).
        // Pager flips are handled above with their own bounded background path.
        if !op.is_empty() {
            return Some(CardActionResult {
                card: None,
                toast: Some("无效操作".to_string()),
            });
        }
        let Some(open_message_id) = value
            .get("open_message_id")
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty())
        else {
            return Some(CardActionResult {
                card: None,
                toast: Some("无法打开文件：找不到文件浏览卡。".to_string()),
            });
        };
        let Some(in_flight_guard) = self.claim_viewer_action(&thread_key, open_message_id) else {
            return Some(CardActionResult {
                card: None,
                toast: Some("正在打开文件…".to_string()),
            });
        };
        let app = Arc::clone(self);
        let core = Arc::clone(core);
        let standalone = session_id.is_some();
        let message_id = open_message_id.to_string();
        let path = resolved.to_string_lossy().to_string();
        let return_to = OwnedReturnTo {
            dir: (!home_dir.is_empty()).then(|| home_dir.clone()),
            keyword: home_keyword.clone(),
            show_hidden: home_hidden,
            page: home_page,
        };
        tokio::spawn(async move {
            app.run_viewer_open(ViewerOpen {
                thread_key,
                path,
                session_dir,
                standalone,
                card_message_id: message_id,
                return_to,
                page,
                core,
                in_flight_guard,
            })
            .await;
        });
        Some(CardActionResult {
            card: None,
            toast: Some("正在打开文件…".to_string()),
        })
    }

    /// Handle the **File Viewer** card's 「发为文件」 click (ADR-0083 §5) — and,
    /// since #778, a Tool Panel's 「🌐 预览网页」 click (`.html`/`.htm`, `op:
    /// "preview"`). The card callback must ack within Feishu's 3 s, no-retry
    /// budget (ADR-0078), and a 30 MB upload cannot fit it, so the click records
    /// the intent, spawns the transfer and returns a toast immediately; the
    /// transfer then re-reads the file locally and posts exactly ONE File Message
    /// under the clicked card.
    ///
    /// The two ops differ in what happens to that clicked card. `Send` PATCHes it
    /// with the outcome — `已发送为文件消息`, `未发送` for a file past the 30 MB cap
    /// or definite failure, or an honest unknown result after an ambiguous
    /// timeout — because the clicked card IS the viewer. `Preview` **never**
    /// PATCHes: the clicked message is a Tool Panel on the agent's Turn card, and
    /// a transfer card built for the file would overwrite the panel and the
    /// reply. So a preview reports nothing on the card when the File Message
    /// lands (it is its own evidence) and speaks a failure as a reply instead.
    ///
    /// A viewer payload carries its Chat/Topic; a Tool Panel payload carries only
    /// the Session (`tool_file_value`), so this resolves the Chat/Topic and the
    /// path scope from the SessionStore the way the open lane does
    /// (`handle_file_card_action`) — the panel's structured path may be relative
    /// to the Session's directory. A Session the store no longer knows cannot
    /// route the reply and answers an honest Toast.
    async fn handle_viewer_file_action(
        self: &Arc<Self>,
        value: &serde_json::Value,
        op: ViewerFileOp,
    ) -> Option<CardActionResult> {
        let path = value.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let message_id = value
            .get("open_message_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let session_id = value
            .get("session_id")
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty());
        if path.is_empty() || message_id.is_empty() {
            return Some(CardActionResult {
                card: None,
                toast: Some("无效操作".to_string()),
            });
        }
        // Route from the payload's Chat/Topic, or — a Tool Panel — from the
        // Session, exactly as the open lane does; a panel whose Session is gone
        // has nothing to reply under and nothing to resolve its path against.
        let (thread_key, session_dir) = {
            let sessions = self.sessions.lock().await;
            let mut thread_key = thread_key_from_value(value);
            let mut session_dir = None;
            if let Some(sid) = session_id {
                if thread_key.chat_id.is_empty()
                    && let Some(resolved) = sessions.thread_for_session(sid)
                {
                    thread_key = resolved;
                }
                session_dir = sessions.directory_for_session(sid);
            }
            (thread_key, session_dir)
        };
        if thread_key.chat_id.is_empty() {
            return Some(CardActionResult {
                card: None,
                toast: Some(format!("⚠️ 无法发送 `{path}`：会话已不在。")),
            });
        }
        // The structured path resolves on the Path Scope (ADR-0083 §4), the same
        // rule the open lane applies; a viewer's already-resolved absolute path
        // passes through unchanged.
        let resolved = match (session_id, session_dir.as_deref()) {
            (Some(_), Some(dir)) => crate::bridge::command::lexical_normalize(
                &crate::bridge::command::expand_against(path, std::path::Path::new(dir)),
            ),
            _ => std::path::PathBuf::from(path),
        };
        let resolved = resolved.to_string_lossy().to_string();
        let content_identity = value
            .get("viewer_file_identity")
            .and_then(serde_json::Value::as_str)
            .and_then(|identity| identity.parse().ok())
            .unwrap_or(0);
        let transfer_key = (
            thread_key.chat_id.clone(),
            message_id.to_string(),
            resolved.clone(),
            content_identity,
        );
        let prior_transfer = self
            .viewer_file_transfers_sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&transfer_key)
            .copied();
        if let Some(status) = prior_transfer {
            return Some(CardActionResult {
                card: None,
                toast: Some(match status {
                    ViewerFileTransferStatus::Sent => "文件已发送".to_string(),
                    ViewerFileTransferStatus::OutcomeUnknown => "发送结果未知，为避免重复未重试".to_string(),
                }),
            });
        }
        let Some(in_flight_guard) = self.claim_viewer_action(&thread_key, message_id) else {
            return Some(CardActionResult {
                card: None,
                toast: Some("正在发送文件…".to_string()),
            });
        };
        let message_id = message_id.to_string();
        let value = value.clone();
        let app = Arc::clone(self);
        tokio::spawn(async move {
            let _in_flight_guard = in_flight_guard;
            app.run_viewer_file_transfer(thread_key, resolved, message_id, content_identity, value, op)
                .await;
        });
        Some(CardActionResult {
            card: None,
            toast: Some("正在发送文件…".to_string()),
        })
    }

    // The off-ack helpers (`run_viewer_page`, `run_viewer_reread`,
    // `run_viewer_open`, `run_viewer_file_transfer`) and the viewer payload
    // structs stay in `handler.rs`; only the two entry bodies live here.
}
