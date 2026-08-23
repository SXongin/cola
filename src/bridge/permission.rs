use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::bridge::handler::{App, CardActionResult};
use crate::bridge::pollers::{
    CardTarget, inline_host_session, mark_stale_cards, resolve_card_target, result_card,
};

/// The permission flow: polls pending permission requests and surfaces them as
/// cards (inline on a streaming card when possible, else a separate card),
/// auto-accepts for `/autoaccept` sessions, marks stale cards when another
/// client resolves a request, and handles the "perm" card action.
pub struct PermissionFlow {
    /// request_id → (card message_id, description) of the permission card cola
    /// sent. Used to mark a card stale — WITH the original request text — when
    /// the request is resolved by ANOTHER client.
    pub sent_permission_cards: Arc<Mutex<HashMap<String, (String, String)>>>,
}

impl PermissionFlow {
    pub fn new() -> Self {
        Self {
            sent_permission_cards: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Independent permission poller: runs forever, surfaces pending permission
    /// requests as cards as soon as they appear. Started once at App startup so
    /// a prompt blocked on an unanswered permission still gets its card shown.
    pub(crate) async fn poll_loop(&self, app: &Arc<App>) -> crate::error::Result<()> {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            // Pending permissions live in the server instance for the session's
            // directory; `GET /permission` must be scoped with `?directory=` or it
            // only sees the server cwd instance. Check every known session directory.
            let directories = { app.sessions.lock().await.directories() };
            let mut pending: std::collections::HashSet<String> = std::collections::HashSet::new();
            for dir in &directories {
                match app.opencode.list_permissions(Some(dir)).await {
                    Ok(perms) => {
                        for p in &perms {
                            pending.insert(p.request_id.clone());
                            let sid = p.session_id.clone().unwrap_or_default();
                            if seen.contains(&p.request_id) {
                                continue;
                            }
                            seen.insert(p.request_id.clone());
                            tracing::info!(
                                "Permission ({}): {} {:?}",
                                dir,
                                p.permission.as_deref().unwrap_or("?"),
                                p.patterns
                            );
                            // `/autoaccept` sessions: answer the request automatically
                            // instead of showing a card (mirrors OpenChamber's toggle).
                            // Resolves sub-task child sessions up their parent chain so
                            // they inherit the parent's flag (like approve_pending).
                            let auto = should_auto_accept(app, &sid, dir).await;
                            if auto {
                                match app
                                    .opencode
                                    .reply_permission(&p.request_id, "once", Some(dir))
                                    .await
                                {
                                    Ok(()) => tracing::info!(
                                        "Auto-accepted permission {} on session {} ({})",
                                        p.request_id,
                                        sid,
                                        p.permission.as_deref().unwrap_or("?")
                                    ),
                                    Err(e) => {
                                        tracing::warn!(
                                            "auto-accept {} on session {}: {}",
                                            p.request_id,
                                            sid,
                                            e
                                        )
                                    }
                                }
                                continue;
                            }
                            let body = describe_permission(p);
                            // One-card-per-turn: surface the permission INLINE on the
                            // streaming card of the session that owns this request —
                            // the session itself, or (sub-task children) its nearest
                            // ancestor with a live card. Only a separate card when
                            // there is no active card (e.g. external turns or restarts).
                            if let Some(host) = inline_host_session(app, &sid, Some(dir)).await {
                                let mut accs = app.accumulators.lock().await;
                                if let Some(acc) = accs.get_mut(&host)
                                    && !acc
                                        .pending_permissions
                                        .iter()
                                        .any(|pp| pp.request_id == p.request_id)
                                {
                                    acc.pending_permissions.push(
                                        crate::bridge::streaming::PendingPermission {
                                            session_id: sid.clone(),
                                            request_id: p.request_id.clone(),
                                            body,
                                            directory: dir.clone(),
                                        },
                                    );
                                    tracing::info!(
                                        "Permission {} inlined on session {} card",
                                        p.request_id,
                                        host
                                    );
                                    drop(accs);
                                    // Flush so the inline section appears NOW — the
                                    // render loop only flushes on new parts, and a
                                    // permission-blocked prompt produces none.
                                    crate::bridge::render::flush_card(app, &host).await;
                                }
                                continue;
                            }
                            let card =
                                crate::feishu::card::build_permission_card(&sid, &p.request_id, &body, dir);
                            // Reply to the message that triggered the prompt for this
                            // session; fall back to sending into the chat when the
                            // accumulator is gone (e.g. after a cola restart). Sub-task
                            // sessions resolve up the parent chain.
                            let sent_id = match resolve_card_target(app, &sid, dir).await {
                                Some(CardTarget::ReplyTo(msg_id)) => {
                                    app.feishu.reply_card(&msg_id, &card).await.ok()
                                }
                                Some(CardTarget::Chat(chat_id)) => {
                                    app.feishu.send_card("chat_id", &chat_id, &card).await.ok()
                                }
                                None => {
                                    tracing::warn!(
                                        "No reply target or chat for permission on session {}",
                                        sid
                                    );
                                    None
                                }
                            };
                            if let Some(mid) = sent_id {
                                self.sent_permission_cards
                                    .lock()
                                    .await
                                    .insert(p.request_id.clone(), (mid, body.clone()));
                            } else {
                                tracing::warn!("Permission card send failed on session {}", sid);
                            }
                        }
                    }
                    Err(e) => tracing::warn!("poll perm ({}): {}", dir, e),
                }
            }
            // Mark stale: a permission card cola sent whose request is no longer
            // pending (resolved by another client) and was NOT answered by cola.
            mark_stale_cards(app, &pending, &self.sent_permission_cards, "权限").await;
            // Drop inline permission sections whose request vanished (answered
            // elsewhere) — the streaming card re-renders without them.
            {
                let mut accs = app.accumulators.lock().await;
                for acc in accs.values_mut() {
                    acc.pending_permissions
                        .retain(|p| pending.contains(&p.request_id));
                }
            }
        }
    }

    /// Handle the "perm" card action: reply to the request and return a result
    /// card (or `card: None` + toast when answered inline on a streaming card).
    pub(crate) async fn handle_card_action(
        &self,
        app: &Arc<App>,
        value: &serde_json::Value,
    ) -> Option<CardActionResult> {
        let session_id = value.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
        let directory = value
            .get("directory")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let directory = match directory {
            Some(d) => Some(d),
            None => app.sessions.lock().await.directory_for_session(session_id),
        };
        let directory = directory.as_deref();
        // Carried from the permission card for the result display
        let perm_label = value.get("perm_label").and_then(|v| v.as_str()).unwrap_or("");
        let perm_color = value
            .get("perm_color")
            .and_then(|v| v.as_str())
            .unwrap_or("green");
        let perm_body = value.get("perm_body").and_then(|v| v.as_str()).unwrap_or("");

        let reply = value.get("reply").and_then(|v| v.as_str()).unwrap_or("reject");
        let request_id = value.get("request_id").and_then(|v| v.as_str());
        let req_id = request_id?;
        // Inline interaction: the session (or its sub-task parent chain) has a
        // live streaming card, so the result is NOT returned as a replacement
        // card — the streaming card re-renders itself on the next poll.
        let host = if app.accumulators.lock().await.contains_key(session_id) {
            Some(session_id.to_string())
        } else {
            inline_host_session(app, session_id, directory).await
        };
        let inline = host.is_some();
        // Double-click guard: once answered, a second click on the same request
        // only re-serves the result.
        let answered = {
            let mut seen = app.answered_requests.lock().await;
            if seen.contains(req_id) {
                true
            } else {
                seen.insert(req_id.to_string());
                false
            }
        };
        if !answered {
            // Route the reply to the instance owning the session.
            if let Err(e) = app.opencode.reply_permission(req_id, reply, directory).await {
                // The request is probably already resolved by another client (e.g.
                // OpenChamber) — show feedback instead of leaving the user with a
                // dead card and no response.
                tracing::error!("perm reply failed: {}", e);
                let mut r = result_card("⚠️ 处理失败", "red", "该权限请求可能已在其他端处理。");
                if inline {
                    r.card = None;
                }
                r.toast = Some("可能已在其他端处理".to_string());
                return Some(r);
            }
            tracing::info!("Permission reply sent: {} session={}", reply, session_id);
            self.sent_permission_cards.lock().await.remove(req_id);
            if let Some(host) = host
                && let Some(acc) = app.accumulators.lock().await.get_mut(&host)
            {
                acc.pending_permissions.retain(|p| p.request_id != req_id);
            }
        }
        // Result card: shows the decision, no buttons.
        let label = if !perm_label.is_empty() { perm_label } else { reply };
        let toast = match reply {
            "once" => "已允许本次执行".to_string(),
            "always" => "已允许，后续将自动放行".to_string(),
            _ => "已拒绝".to_string(),
        };
        let body = if perm_body.is_empty() {
            format!("Permission: {}", reply)
        } else {
            perm_body.to_string()
        };
        let mut r = result_card(label, perm_color, &body);
        if inline {
            r.card = None;
        }
        r.toast = Some(toast);
        Some(r)
    }
}

/// Whether a permission request for `session_id` should be auto-accepted.
///
/// Mirrors the `/autoaccept` flag, resolved like `resolve_card_target` does for
/// card delivery: a sub-task child session is NOT in cola's SessionStore, so a
/// direct lookup misses it and it would surface a card even though its parent
/// session has autoaccept on. Walking the parent chain makes the child inherit
/// the parent's flag, consistent with `App::approve_pending_for_session`.
async fn should_auto_accept(app: &Arc<App>, session_id: &str, directory: &str) -> bool {
    let mut current = session_id.to_string();
    for _ in 0..8 {
        {
            let sessions = app.sessions.lock().await;
            if let Some(e) = sessions.entry_for_session(&current) {
                return e.auto_accept;
            }
        }
        // Unknown session → walk up to its parent (sub-task child sessions).
        let Ok(info) = app.opencode.session_info(&current, Some(directory)).await else {
            return false;
        };
        let Some(parent) = info.parent_id.filter(|p| p != &current) else {
            return false;
        };
        current = parent;
    }
    false
}

fn describe_permission(p: &crate::opencode::client::PermissionRequest) -> String {
    let action = p.permission.as_deref().unwrap_or("?");
    let (emoji, label) = describe_action(action);
    let mut s = format!("{} **{}**\n", emoji, label);

    if !p.patterns.is_empty() {
        s.push_str("**对象**:\n");
        for r in &p.patterns {
            s.push_str(&format!("- `{}`\n", truncate(r, 150)));
        }
    }

    // Metadata often carries richer context (e.g. bash command, tool input)
    if let Some(meta) = &p.metadata {
        let mut shown = 0;
        for (key, label) in [
            ("command", "命令"),
            ("cwd", "目录"),
            ("description", "说明"),
            ("input", "输入"),
            ("path", "路径"),
        ] {
            if let Some(v) = meta.get(key) {
                let val = v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string());
                s.push_str(&format!("**{}**: `{}`\n", label, truncate(&val, 200)));
                shown += 1;
                if shown >= 3 {
                    break;
                }
            }
        }
        if shown == 0 {
            let compact = serde_json::to_string(meta).unwrap_or_default();
            if compact.len() > 300 {
                s.push_str(&format!("**详情**: `{}`", truncate(&compact, 300)));
            } else if !compact.is_empty() && compact != "{}" {
                s.push_str(&format!("**详情**: `{}`", compact));
            }
        }
    }

    s.push_str("\nAI 想要执行这个操作，是否允许？");
    s
}

/// Map an OpenCode permission action to a friendly emoji + Chinese description.
fn describe_action(action: &str) -> (&'static str, &'static str) {
    match action {
        "bash" => ("⚡", "执行 Shell 命令"),
        "read" => ("📖", "读取文件"),
        "write" => ("✏️", "修改文件"),
        "edit" => ("✏️", "编辑文件"),
        "patch" => ("✏️", "应用补丁"),
        "webfetch" => ("🌐", "访问网页"),
        "fetch" => ("🌐", "获取网络资源"),
        "external_directory" => ("📁", "访问外部目录"),
        "kill" => ("🛑", "终止进程"),
        "ls" => ("📂", "列出目录"),
        "list" => ("📂", "列出内容"),
        "rename" => ("🔤", "重命名"),
        "remove" | "delete" | "rm" => ("🗑️", "删除文件"),
        "mkdir" => ("📁", "创建目录"),
        "move" | "mv" => ("📦", "移动文件"),
        "copy" | "cp" => ("📋", "复制文件"),
        "link" | "ln" => ("🔗", "创建链接"),
        "install" => ("📥", "安装依赖"),
        "test" => ("🧪", "运行测试"),
        "build" => ("🏗️", "构建项目"),
        "git" => ("🌿", "执行 Git 操作"),
        _ => ("🔐", "执行操作"),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max).collect::<String>())
    }
}
