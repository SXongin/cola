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
        for dir in &directories {
            match app.opencode.list_permissions(Some(dir)).await {
                Ok(perms) => {
                    for p in &perms {
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
                        let body = describe_permission(p);
                        let card =
                            crate::feishu::card::build_permission_card(&sid, &p.request_id, &body, dir);
                        // Reply to the message that triggered the prompt for this
                        // session; fall back to sending into the chat when the
                        // accumulator is gone (e.g. after a cola restart). Sub-task
                        // sessions resolve up the parent chain.
                        let sent = match resolve_card_target(app, &sid, dir).await {
                            Some(CardTarget::ReplyTo(msg_id)) => {
                                app.feishu.reply_card(&msg_id, &card).await.is_ok()
                            }
                            Some(CardTarget::Chat(chat_id)) => {
                                app.feishu.send_card("chat_id", &chat_id, &card).await.is_ok()
                            }
                            None => {
                                tracing::warn!("No reply target or chat for permission on session {}", sid);
                                false
                            }
                        };
                        if !sent {
                            tracing::warn!("Permission card send failed on session {}", sid);
                        }
                    }
                }
                Err(e) => tracing::warn!("poll perm ({}): {}", dir, e),
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
        for dir in &directories {
            match app.opencode.list_questions(Some(dir)).await {
                Ok(questions) => {
                    for q in &questions {
                        if seen.contains(&q.id) {
                            continue;
                        }
                        seen.insert(q.id.clone());
                        tracing::info!("Question ({}): {} — {} questions", dir, q.id, q.questions.len());
                        {
                            let mut reqs = app.question_requests.lock().await;
                            reqs.insert(q.id.clone(), q.clone());
                        }
                        let card =
                            crate::feishu::card::build_question_card(&q.id, &q.session_id, &q.questions, dir);
                        let sent = match resolve_card_target(app, &q.session_id, dir).await {
                            Some(CardTarget::ReplyTo(msg_id)) => {
                                app.feishu.reply_card(&msg_id, &card).await.is_ok()
                            }
                            Some(CardTarget::Chat(chat_id)) => {
                                app.feishu.send_card("chat_id", &chat_id, &card).await.is_ok()
                            }
                            None => {
                                tracing::warn!(
                                    "No reply target or chat for question on session {}",
                                    q.session_id
                                );
                                false
                            }
                        };
                        if !sent {
                            tracing::warn!("Question card send failed on session {}", q.session_id);
                        }
                    }
                }
                Err(e) => tracing::warn!("poll question ({}): {}", dir, e),
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
