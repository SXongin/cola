use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::bridge::handler::{App, CardActionResult};
use crate::bridge::pollers::{
    CardTarget, inline_host_session, mark_stale_cards, resolve_card_target, result_card,
};

/// Partial answers recorded for a pending question request: `answers[i]` is
/// `None` until the user answers question `i`. A request is only submitted once
/// every slot is filled (or the user clicks "submit/skip").
type QuestionPartial = HashMap<String, Vec<Option<Vec<String>>>>;

/// The question flow: polls pending `question` tool requests and surfaces them
/// as cards (inline on a streaming card when possible, else a separate card),
/// accumulates partial answers, submits once every question is answered (or the
/// user clicks submit/skip), rejects, and marks stale cards.
pub struct QuestionFlow {
    /// request_id → pending question request (the AI asks the user; cola posts
    /// the answer back from the question card).
    pub question_requests: Arc<Mutex<HashMap<String, crate::opencode::client::QuestionRequest>>>,
    /// request_id → answers recorded so far (None = not answered yet). A request
    /// is only submitted once EVERY question has an answer (or the user clicks
    /// "submit/skip"), because `reply_question` expects answers for all of them.
    pub question_partial: Arc<Mutex<QuestionPartial>>,
    /// request_id → (card message_id, description) of the question card cola
    /// sent (used to mark a card stale — same use as permission cards).
    pub sent_question_cards: Arc<Mutex<HashMap<String, (String, String)>>>,
}

impl QuestionFlow {
    pub fn new() -> Self {
        Self {
            question_requests: Arc::new(Mutex::new(HashMap::new())),
            question_partial: Arc::new(Mutex::new(HashMap::new())),
            sent_question_cards: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Independent question poller: surfaces pending `question` tool requests as
    /// Feishu cards (the AI blocks until answered; the event never reaches the
    /// global SSE). Started once at App startup, like the permission poller.
    pub(crate) async fn poll_loop(&self, app: &Arc<App>) -> crate::error::Result<()> {
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
                                let mut reqs = self.question_requests.lock().await;
                                reqs.insert(q.id.clone(), q.clone());
                            }
                            // One-card-per-turn: surface the question inline on the
                            // streaming card of the session that owns it — the session
                            // itself, or (sub-task children) its nearest ancestor with
                            // a live card.
                            if let Some(host) = inline_host_session(app, &q.session_id, Some(dir)).await {
                                let mut accs = app.accumulators.lock().await;
                                if let Some(acc) = accs.get_mut(&host) {
                                    if !acc.pending_questions.iter().any(|pq| pq.request_id == q.id) {
                                        acc.pending_questions.push(
                                            crate::bridge::streaming::PendingQuestion {
                                                request_id: q.id.clone(),
                                                session_id: q.session_id.clone(),
                                                questions: q.questions.clone(),
                                                directory: dir.clone(),
                                                answers: vec![None; q.questions.len()],
                                            },
                                        );
                                    }
                                    tracing::info!("Question {} inlined on session {} card", q.id, host);
                                    drop(accs);
                                    // Flush so the inline answer buttons appear NOW —
                                    // the render loop only flushes on new parts, and a
                                    // question-blocked prompt produces none.
                                    crate::bridge::render::flush_card(app, &host).await;
                                }
                                continue;
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
                                self.sent_question_cards
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
            mark_stale_cards(app, &pending, &self.sent_question_cards, "问答").await;
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

    /// Handle the "question" card action: answer / submit / reject. Returns the
    /// updated card (or `card: None` + toast when answered inline).
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

        let reply = value.get("reply").and_then(|v| v.as_str()).unwrap_or("reject");
        let request_id = value.get("request_id").and_then(|v| v.as_str());
        let Some(req_id) = request_id else {
            return None;
        };
        // Inline interaction: the session (or its sub-task parent chain) has a
        // live streaming card — answered inline there, so no replacement card is
        // returned.
        let host = if app.accumulators.lock().await.contains_key(session_id) {
            Some(session_id.to_string())
        } else {
            inline_host_session(app, session_id, directory).await
        };
        let inline = host.is_some();

        match reply {
            "answer" => {
                let index = value.get("question_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let answer = value
                    .get("answer")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if answer.is_empty() {
                    return None;
                }
                // A request is submitted ONLY when every question has an answer
                // (`reply_question` expects all of them).
                let already = app.answered_requests.lock().await.contains(req_id);
                if already {
                    // finalized before (double-click): replay the result.
                    let mut r = result_card(
                        &format!("✅ 已回答：{}", answer),
                        "green",
                        &format!("AI 的问题是：\n{}", answer),
                    );
                    if inline {
                        r.card = None;
                    }
                    return Some(r);
                }
                let n = {
                    let reqs = self.question_requests.lock().await;
                    reqs.get(req_id).map(|r| r.questions.len())
                };
                let Some(n) = n else {
                    return None; // stale card for an already-submitted request
                };
                // Record this question's answer.
                let (answered_count, slot) = {
                    let mut partial = self.question_partial.lock().await;
                    let (count, slot) = record_answer(&mut partial, req_id, n, index, &answer);
                    (count, slot)
                };
                // Keep the inline card's partial answers in sync.
                if inline
                    && let Some(pq) = app
                        .accumulators
                        .lock()
                        .await
                        .get_mut(host.as_deref().unwrap_or(session_id))
                        .and_then(|acc| {
                            acc.pending_questions
                                .iter_mut()
                                .find(|pq| pq.request_id == req_id)
                        })
                {
                    pq.answers = slot.clone();
                }
                if answered_count == n {
                    // All questions answered → submit the whole request.
                    app.answered_requests.lock().await.insert(req_id.to_string());
                    let answers: Vec<Vec<String>> = self
                        .question_partial
                        .lock()
                        .await
                        .remove(req_id)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|a| a.unwrap_or_default())
                        .collect();
                    if let Err(e) = app.opencode.reply_question(req_id, &answers, directory).await {
                        tracing::error!("question reply failed: {}", e);
                        let mut r = result_card("⚠️ 处理失败", "red", "该问题可能已在其他端回答。");
                        if inline {
                            r.card = None;
                        }
                        r.toast = Some("可能已在其他端回答".to_string());
                        return Some(r);
                    }
                    tracing::info!(
                        "Question answered: {} session={} -> {:?}",
                        req_id,
                        session_id,
                        answers
                    );
                    self.question_requests.lock().await.remove(req_id);
                    self.sent_question_cards.lock().await.remove(req_id);
                    if inline
                        && let Some(acc) = app
                            .accumulators
                            .lock()
                            .await
                            .get_mut(host.as_deref().unwrap_or(session_id))
                    {
                        acc.pending_questions.retain(|pq| pq.request_id != req_id);
                    }
                    let mut r = result_card(
                        &format!("✅ 已回答：{}", answer),
                        "green",
                        &format!("AI 的问题是：\n{}", answer),
                    );
                    if inline {
                        r.card = None;
                    }
                    r.toast = Some("已回答".to_string());
                    return Some(r);
                } else if inline {
                    // Inline: the streaming card re-renders with the updated
                    // partial answers — toast only.
                    let mut r = CardActionResult {
                        card: None,
                        toast: None,
                    };
                    r.toast = Some(format!("已记录答案，还有 {} 题未答", n - answered_count));
                    return Some(r);
                } else {
                    // Still questions left: return an updated card that shows the
                    // answered ones as done and the rest open.
                    let remaining = n - answered_count;
                    let req = {
                        let reqs = self.question_requests.lock().await;
                        reqs.get(req_id).cloned()
                    };
                    let req = req?;
                    let partial = self
                        .question_partial
                        .lock()
                        .await
                        .get(req_id)
                        .cloned()
                        .unwrap_or_else(|| vec![None; n]);
                    let card = crate::feishu::card::build_question_card(
                        req_id,
                        &req.session_id,
                        &req.questions,
                        directory.unwrap_or(""),
                        &partial,
                    );
                    let mut r = CardActionResult {
                        card: Some(card),
                        toast: None,
                    };
                    r.toast = Some(format!("已记录答案，还有 {} 题未答", remaining));
                    return Some(r);
                }
            }
            "submit" => {
                // Finalize with whatever was answered (empty for the rest).
                let already = app.answered_requests.lock().await.contains(req_id);
                if already {
                    let mut r = result_card("✅ 已回答", "green", "已提交 AI 的问题答案。");
                    if inline {
                        r.card = None;
                    }
                    return Some(r);
                }
                app.answered_requests.lock().await.insert(req_id.to_string());
                let answers: Vec<Vec<String>> = self
                    .question_partial
                    .lock()
                    .await
                    .remove(req_id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|a| a.unwrap_or_default())
                    .collect();
                if let Err(e) = app.opencode.reply_question(req_id, &answers, directory).await {
                    tracing::error!("question reply failed: {}", e);
                    let mut r = result_card("⚠️ 处理失败", "red", "该问题可能已在其他端回答。");
                    if inline {
                        r.card = None;
                    }
                    r.toast = Some("可能已在其他端回答".to_string());
                    return Some(r);
                }
                tracing::info!(
                    "Question submitted: {} session={} -> {:?}",
                    req_id,
                    session_id,
                    answers
                );
                self.question_requests.lock().await.remove(req_id);
                if inline
                    && let Some(acc) = app
                        .accumulators
                        .lock()
                        .await
                        .get_mut(host.as_deref().unwrap_or(session_id))
                {
                    acc.pending_questions.retain(|pq| pq.request_id != req_id);
                }
                let labels: Vec<&str> = answers.iter().flatten().map(|s| s.as_str()).collect();
                let summary = if labels.is_empty() {
                    "已提交".to_string()
                } else {
                    labels.join("、")
                };
                let mut r = result_card(
                    &format!("✅ 已回答：{}", summary),
                    "green",
                    &format!("AI 的问题是：\n{}", summary),
                );
                if inline {
                    r.card = None;
                }
                r.toast = Some("已提交".to_string());
                Some(r)
            }
            "reject" => {
                let already = app.answered_requests.lock().await.contains(req_id);
                if already {
                    let mut r = result_card("🚫 已拒绝回答", "red", "已拒绝回答 AI 的问题。");
                    if inline {
                        r.card = None;
                    }
                    return Some(r);
                }
                app.answered_requests.lock().await.insert(req_id.to_string());
                if let Err(e) = app.opencode.reject_question(req_id, directory).await {
                    tracing::error!("question reject failed: {}", e);
                    let mut r = result_card("⚠️ 处理失败", "red", "该问题可能已在其他端回答。");
                    if inline {
                        r.card = None;
                    }
                    r.toast = Some("可能已在其他端回答".to_string());
                    return Some(r);
                }
                tracing::info!("Question rejected: {}", req_id);
                self.question_requests.lock().await.remove(req_id);
                self.question_partial.lock().await.remove(req_id);
                self.sent_question_cards.lock().await.remove(req_id);
                if inline
                    && let Some(acc) = app
                        .accumulators
                        .lock()
                        .await
                        .get_mut(host.as_deref().unwrap_or(session_id))
                {
                    acc.pending_questions.retain(|pq| pq.request_id != req_id);
                }
                let mut r = result_card("🚫 已拒绝回答", "red", "已拒绝回答 AI 的问题。");
                if inline {
                    r.card = None;
                }
                r.toast = Some("已拒绝回答".to_string());
                Some(r)
            }
            _ => None,
        }
    }
}

/// Record one answer into the partial-answer slots for a question request.
/// Returns `(answered_count, slot)` where `slot` is the full (cloned) answer
/// vector — the caller syncs it to the inline card and submits once
/// `answered_count == n`.
fn record_answer(
    partial: &mut QuestionPartial,
    req_id: &str,
    n: usize,
    index: usize,
    answer: &str,
) -> (usize, Vec<Option<Vec<String>>>) {
    let slot = partial.entry(req_id.to_string()).or_insert_with(|| vec![None; n]);
    if index < n {
        slot[index] = Some(vec![answer.to_string()]);
    }
    let count = slot.iter().filter(|a| a.is_some()).count();
    (count, slot.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_answer_starts_empty_and_fills_slots() {
        let mut partial = HashMap::new();
        // First answer, index 0 of 2.
        let (count, slot) = record_answer(&mut partial, "q1", 2, 0, "main");
        assert_eq!(count, 1);
        assert_eq!(slot, vec![Some(vec!["main".to_string()]), None]);
        // Second answer, index 1 → all filled.
        let (count, slot) = record_answer(&mut partial, "q1", 2, 1, "dev");
        assert_eq!(count, 2);
        assert_eq!(
            slot,
            vec![Some(vec!["main".to_string()]), Some(vec!["dev".to_string()])]
        );
    }

    #[test]
    fn record_answer_replacing_a_slot_keeps_count_stable() {
        let mut partial = HashMap::new();
        record_answer(&mut partial, "q1", 2, 0, "a");
        record_answer(&mut partial, "q1", 2, 1, "b");
        // Re-answering slot 0 replaces it without changing the count.
        let (count, slot) = record_answer(&mut partial, "q1", 2, 0, "c");
        assert_eq!(count, 2);
        assert_eq!(
            slot,
            vec![Some(vec!["c".to_string()]), Some(vec!["b".to_string()])]
        );
    }

    #[test]
    fn record_answer_out_of_range_index_is_ignored() {
        let mut partial = HashMap::new();
        let (count, slot) = record_answer(&mut partial, "q1", 2, 99, "x");
        assert_eq!(count, 0);
        assert_eq!(slot, vec![None, None]);
    }

    #[test]
    fn record_answer_isolates_requests_by_id() {
        let mut partial = HashMap::new();
        record_answer(&mut partial, "q1", 1, 0, "a");
        let (count, _) = record_answer(&mut partial, "q2", 1, 0, "b");
        assert_eq!(count, 1); // q2 has its own slots
    }
}
