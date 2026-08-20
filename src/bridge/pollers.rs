use std::sync::Arc;

use crate::bridge::handler::App;

/// Where to deliver a permission/question card.
enum CardTarget {
    /// Reply to a specific message (an in-flight prompt's streaming card).
    ReplyTo(String),
    /// Send into a chat.
    Chat(String),
}

/// Resolve where a card for `session_id` should be delivered.
///
/// Sessions cola created map to a Feishu chat directly. Sub-task sessions
/// (created by the `task` tool) are NOT in cola's SessionStore, so when the
/// direct lookup fails we walk up the parent chain (via `session_info`) until
/// a session cola knows is found. The directory is passed through so the
/// parent session is looked up in the same server instance.
async fn resolve_card_target(app: &Arc<App>, session_id: &str, directory: &str) -> Option<CardTarget> {
    let mut current = session_id.to_string();
    for _ in 0..8 {
        // In-flight prompt for this session → reply to its streaming card.
        let reply_to = {
            let accs = app.accumulators.lock().await;
            accs.get(&current).and_then(|a| a.reply_to_message_id.clone())
        };
        if let Some(msg_id) = reply_to {
            return Some(CardTarget::ReplyTo(msg_id));
        }
        // Session mapped to a chat → send into the chat.
        let chat = { app.sessions.lock().await.chat_for_session(&current) };
        if let Some(chat_id) = chat {
            return Some(CardTarget::Chat(chat_id));
        }
        // Unknown session → walk up to its parent (sub-task child sessions).
        let info = app.opencode.session_info(&current, Some(directory)).await.ok()?;
        let parent = info.parent_id.filter(|p| p != &current)?;
        current = parent;
    }
    None
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

/// Independent permission poller: runs forever, surfaces pending permission
/// requests as cards as soon as they appear. Started once at App startup so a
/// prompt blocked on an unanswered permission still gets its card shown.
pub(crate) async fn permission_poll_loop(app: &Arc<App>) -> crate::error::Result<()> {
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
                                    tracing::warn!("auto-accept {} on session {}: {}", p.request_id, sid, e)
                                }
                            }
                            continue;
                        }
                        let body = describe_permission(p);
                        // One-card-per-turn: when the session has an active
                        // streaming accumulator, surface the permission INLINE on
                        // that card instead of a separate card (only separate when
                        // there is no active card, e.g. external turns or restarts).
                        let inline = {
                            let accs = app.accumulators.lock().await;
                            accs.get(&sid)
                                .map(|a| {
                                    !a.pending_permissions
                                        .iter()
                                        .any(|pp| pp.request_id == p.request_id)
                                })
                                .unwrap_or(false)
                        };
                        if inline {
                            let mut accs = app.accumulators.lock().await;
                            if let Some(acc) = accs.get_mut(&sid) {
                                acc.pending_permissions
                                    .push(crate::bridge::streaming::PendingPermission {
                                        session_id: sid.clone(),
                                        request_id: p.request_id.clone(),
                                        body,
                                        directory: dir.clone(),
                                    });
                                tracing::info!("Permission {} inlined on session {} card", p.request_id, sid);
                                drop(accs);
                                // Flush so the inline section appears NOW — the
                                // render loop only flushes on new parts, and a
                                // permission-blocked prompt produces none.
                                crate::bridge::render::flush_card(app, &sid).await;
                                continue;
                            }
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
                                tracing::warn!("No reply target or chat for permission on session {}", sid);
                                None
                            }
                        };
                        if let Some(mid) = sent_id {
                            app.sent_permission_cards
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
        mark_stale_cards(app, &pending, &app.sent_permission_cards, "权限").await;
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

/// Mark permission/question cards as "already handled" when the underlying
/// request disappeared without cola answering it (another client resolved it).
/// The stale card keeps the original request description so the user can see
/// what was handled.
async fn mark_stale_cards(
    app: &Arc<App>,
    pending: &std::collections::HashSet<String>,
    sent: &Arc<tokio::sync::Mutex<std::collections::HashMap<String, (String, String)>>>,
    kind: &str,
) {
    let stale: Vec<(String, String, String)> = {
        let sent_map = sent.lock().await.clone();
        let answered = app.answered_requests.lock().await;
        sent_map
            .into_iter()
            .filter(|(rid, _)| !pending.contains(rid) && !answered.contains(rid))
            .map(|(rid, (mid, desc))| (rid, mid, desc))
            .collect()
    };
    for (rid, mid, desc) in stale {
        sent.lock().await.remove(&rid);
        let card = crate::feishu::card::build_resolved_elsewhere_card(kind, &desc);
        if let Err(e) = app.feishu.update_message(&mid, &card).await {
            tracing::warn!("mark stale {} card {}: {}", kind, rid, e);
        } else {
            tracing::info!("Marked stale {} card {} as handled", kind, rid);
        }
    }
}

/// Preview of a user message, for the external-message notification card.
fn user_message_preview(msgs: &[crate::opencode::client::SessionMessage], created: i64) -> String {
    let mut out = String::new();
    for m in msgs {
        if m.info.role.as_deref() != Some("user") {
            continue;
        }
        if m.info.time.as_ref().map(|t| t.created) != Some(created) {
            continue;
        }
        if let Some(parts) = m.parts.as_array() {
            for p in parts {
                if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                    out.push_str(t);
                }
            }
        }
    }
    out.chars().take(80).collect()
}

/// Independent poller: detects user messages that were NOT sent by cola (i.e.
/// someone posted from OpenChamber or another client on the shared store) and
/// notifies the Feishu side with a small card. cola's own prompts are excluded
/// via the per-session baseline set at the end of each prompt.
pub(crate) async fn external_message_poll_loop(app: &Arc<App>) -> crate::error::Result<()> {
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(8)).await;
        let sessions: Vec<(String, String, String)> = app
            .sessions
            .lock()
            .await
            .all_entries()
            .into_iter()
            .map(|e| (e.session_id.clone(), e.thread_key.chat_id.clone(), e.name.clone()))
            .collect();
        for (sid, chat_id, name) in sessions {
            // While cola is answering this session, any new message is cola's own.
            if app.inflight.lock().await.contains(&sid) {
                continue;
            }
            let Ok(msgs) = app.opencode.messages(&sid).await else {
                continue;
            };
            let latest_user = msgs
                .iter()
                .filter(|m| m.info.role.as_deref() == Some("user"))
                .filter_map(|m| m.info.time.as_ref().map(|t| t.created))
                .max();
            let Some(latest) = latest_user else { continue };
            let mut map = app.last_user_msg_epoch.lock().await;
            match map.get(&sid).copied() {
                // First observation: just establish the baseline, don't notify.
                None => {
                    map.insert(sid.clone(), latest);
                }
                Some(prev) if latest > prev => {
                    map.insert(sid.clone(), latest);
                    let preview = user_message_preview(&msgs, latest);
                    drop(map);
                    tracing::info!("External message on session {}: {}", sid, preview);
                    let card = crate::feishu::card::build_external_message_card(&name, &preview);
                    if let Err(e) = app.feishu.send_card("chat_id", &chat_id, &card).await {
                        tracing::warn!("external message notify: {}", e);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Independent question poller: surfaces pending `question` tool requests as
/// Feishu cards (the AI blocks until answered; the event never reaches the
/// global SSE). Started once at App startup, like the permission poller.
pub(crate) async fn question_poll_loop(app: &Arc<App>) -> crate::error::Result<()> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
        let directories = { app.sessions.lock().await.directories() };
        let mut pending: std::collections::HashSet<String> = std::collections::HashSet::new();
        for dir in &directories {
            match app.opencode.list_questions(Some(dir)).await {
                Ok(questions) => {
                    for q in &questions {
                        pending.insert(q.id.clone());
                        if seen.contains(&q.id) {
                            continue;
                        }
                        seen.insert(q.id.clone());
                        tracing::info!("Question ({}): {} — {} questions", dir, q.id, q.questions.len());
                        {
                            let mut reqs = app.question_requests.lock().await;
                            reqs.insert(q.id.clone(), q.clone());
                        }
                        // One-card-per-turn: surface the question inline on the
                        // session's streaming card when it has an active accumulator.
                        let inline = { app.accumulators.lock().await.contains_key(&q.session_id) };
                        if inline {
                            let mut accs = app.accumulators.lock().await;
                            if let Some(acc) = accs.get_mut(&q.session_id) {
                                if !acc.pending_questions.iter().any(|pq| pq.request_id == q.id) {
                                    acc.pending_questions
                                        .push(crate::bridge::streaming::PendingQuestion {
                                            request_id: q.id.clone(),
                                            session_id: q.session_id.clone(),
                                            questions: q.questions.clone(),
                                            directory: dir.clone(),
                                            answers: vec![None; q.questions.len()],
                                        });
                                }
                                tracing::info!("Question {} inlined on session {} card", q.id, q.session_id);
                                drop(accs);
                                // Flush so the inline answer buttons appear NOW —
                                // the render loop only flushes on new parts, and a
                                // question-blocked prompt produces none.
                                crate::bridge::render::flush_card(app, &q.session_id).await;
                                continue;
                            }
                        }
                        let card = crate::feishu::card::build_question_card(
                            &q.id,
                            &q.session_id,
                            &q.questions,
                            dir,
                            &vec![None; q.questions.len()],
                        );
                        let sent_id = match resolve_card_target(app, &q.session_id, dir).await {
                            Some(CardTarget::ReplyTo(msg_id)) => {
                                app.feishu.reply_card(&msg_id, &card).await.ok()
                            }
                            Some(CardTarget::Chat(chat_id)) => {
                                app.feishu.send_card("chat_id", &chat_id, &card).await.ok()
                            }
                            None => {
                                tracing::warn!(
                                    "No reply target or chat for question on session {}",
                                    q.session_id
                                );
                                None
                            }
                        };
                        if let Some(mid) = sent_id {
                            let desc = crate::feishu::card::question_summary(&q.questions);
                            app.sent_question_cards
                                .lock()
                                .await
                                .insert(q.id.clone(), (mid, desc));
                        } else {
                            tracing::warn!("Question card send failed on session {}", q.session_id);
                        }
                    }
                }
                Err(e) => tracing::warn!("poll question ({}): {}", dir, e),
            }
        }
        // Mark stale: a question card cola sent whose request was resolved by
        // another client (and not answered by cola).
        mark_stale_cards(app, &pending, &app.sent_question_cards, "问答").await;
        // Drop inline question sections whose request vanished (answered
        // elsewhere) — the streaming card re-renders without them.
        {
            let mut accs = app.accumulators.lock().await;
            for acc in accs.values_mut() {
                acc.pending_questions.retain(|q| pending.contains(&q.request_id));
            }
        }
    }
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
