use crate::bridge::handler::CardActionResult;
use crate::bridge::pollers::result_card;
use crate::opencode;

/// One pending question request's whole in-flight state: what the AI asked,
/// where the request lives, the finalized answers and the live multi-select
/// toggles. One entry per request replaces the four parallel maps that had to
/// be mutated in lockstep — a missed map used to be a silent bug.
pub struct QuestionState {
    /// The full request, kept so its card can be rebuilt with partial answers.
    request: opencode::client::QuestionRequest,
    /// Owning directory, recorded where the poll loop knows it. The question
    /// card's `name` no longer carries the directory (Feishu caps `name` at
    /// 100 chars), so a value-less form-submit callback re-resolves it here
    /// (sub-task child sessions aren't in the Session Mapping — pitfall 11).
    dir: String,
    /// FINAL answers: `answers[i]` is `None` until question `i` is finalized.
    /// A request is only submitted once every slot is filled (or the user
    /// clicks "submit/skip").
    answers: Vec<Option<Vec<String>>>,
    /// LIVE multi-select toggles, kept SEPARATE from `answers` so an
    /// in-progress multi-select (labels toggled but not yet confirmed) never
    /// counts as answered — the request can only auto-submit once every
    /// question is finalized (single-select clicked, multi-select confirmed).
    toggles: Vec<Option<Vec<String>>>,
}

impl QuestionState {
    pub(crate) fn new(request: opencode::client::QuestionRequest, dir: String) -> Self {
        let slots = vec![None; request.questions.len()];
        Self {
            request,
            dir,
            answers: slots.clone(),
            toggles: slots,
        }
    }

    /// Update the remembered request/dir in place, keeping recorded answers and
    /// toggles: a re-armed follow or a re-claim must not wipe what the user
    /// already selected.
    pub(crate) fn refresh(&mut self, request: &opencode::client::QuestionRequest, dir: &str) {
        self.request = request.clone();
        self.dir = dir.to_string();
        self.answers.resize(request.questions.len(), None);
        self.toggles.resize(request.questions.len(), None);
    }

    /// The full request, for rebuilding its card with partial answers.
    pub(crate) fn request(&self) -> &opencode::client::QuestionRequest {
        &self.request
    }

    /// The owning directory, used as the delivery fallback when a card
    /// callback carries none.
    pub(crate) fn dir(&self) -> &str {
        &self.dir
    }

    /// The FINAL answers, one slot per question (`None` = still open).
    pub(crate) fn answers(&self) -> &[Option<Vec<String>>] {
        &self.answers
    }

    pub(crate) fn len(&self) -> usize {
        self.request.questions.len()
    }

    pub(crate) fn is_multi(&self, index: usize) -> bool {
        self.request
            .questions
            .get(index)
            .is_some_and(|q| q.multiple == Some(true))
    }

    /// Record a single-select answer (replaces any previous one).
    pub(crate) fn record_answer(&mut self, index: usize, answer: &str) {
        if index < self.answers.len() {
            self.answers[index] = Some(vec![answer.to_string()]);
        }
    }

    /// Toggle a label in an open multi-select: add if absent, remove if
    /// present. Removing the last label reverts the slot to `None` (the
    /// question is open again).
    pub(crate) fn record_toggle(&mut self, index: usize, answer: &str) {
        if index >= self.toggles.len() {
            return;
        }
        let empty = {
            let set = self.toggles[index].get_or_insert_with(Vec::new);
            if let Some(pos) = set.iter().position(|l| l == answer) {
                set.remove(pos);
            } else {
                set.push(answer.to_string());
            }
            set.is_empty()
        };
        if empty {
            self.toggles[index] = None;
        }
    }

    /// Append a typed custom label to an open multi-select (never removes; a
    /// duplicate is a no-op). Removal has its own affordance: the rendered
    /// Custom Answer button re-enters as a normal toggle (`reply: "answer"`).
    pub(crate) fn record_append(&mut self, index: usize, answer: &str) {
        if index >= self.toggles.len() {
            return;
        }
        let set = self.toggles[index].get_or_insert_with(Vec::new);
        if !set.iter().any(|l| l == answer) {
            set.push(answer.to_string());
        }
    }

    /// Lock the live toggles of question `index` into its final answer (empty
    /// allowed — "不选"). A stale confirm of an already-finalized question is a
    /// no-op, never overwriting the locked answer.
    pub(crate) fn confirm(&mut self, index: usize) {
        if index >= self.answers.len() || self.answers[index].is_some() {
            return;
        }
        self.answers[index] = Some(self.toggles[index].take().unwrap_or_default());
    }

    /// Whether `answer` is currently in the live toggles for one question.
    /// Read BEFORE recording so the caller can name the outcome in the toast.
    pub(crate) fn toggle_selected(&self, index: usize, answer: &str) -> bool {
        self.toggles
            .get(index)
            .and_then(|slot| slot.as_ref())
            .is_some_and(|labels| labels.iter().any(|l| l == answer))
    }

    /// What the card should display: `display[i]` is the locked answer for
    /// finalized questions or the live toggles of an open multi-select;
    /// `done[i]` marks finalized questions. Returns `(count, display, done)`
    /// where `count` counts FINAL answers only — in-progress toggles never
    /// drive auto-submit.
    pub(crate) fn merge(&self) -> (usize, Vec<Option<Vec<String>>>, Vec<bool>) {
        let n = self.answers.len();
        let mut display = vec![None; n];
        let mut done = vec![false; n];
        let mut count = 0;
        for i in 0..n {
            if let Some(labels) = &self.answers[i] {
                display[i] = Some(labels.clone());
                done[i] = true;
                count += 1;
            } else if let Some(labels) = &self.toggles[i] {
                display[i] = Some(labels.clone());
            }
        }
        (count, display, done)
    }
}

/// What one multi-select interaction did to the selection: an option or Custom
/// Answer added, one removed, or a Custom Answer that was already selected
/// (append dedupes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MultiOutcome {
    Added,
    Removed,
    Duplicate,
}

/// Toast after a question-card interaction. A multi-select add, removal and
/// deduped custom each name what happened, so the user can tell "added" from
/// "was already there" without re-reading the card; 确定该题 reports how many
/// questions are still left, and a single-select answer reports how many remain.
pub(crate) fn action_toast(reply: &str, remaining: usize, outcome: Option<MultiOutcome>) -> String {
    if reply == "confirm" {
        return format!("已确定该题，还有 {} 题未答", remaining);
    }
    match outcome {
        Some(MultiOutcome::Duplicate) => "该选项已在已选中".to_string(),
        Some(MultiOutcome::Removed) => "已移除选项".to_string(),
        // A typed custom answer is its own kind of add; an option click is just
        // an option.
        Some(MultiOutcome::Added) if reply == "custom" => "已添加自定义答案".to_string(),
        Some(MultiOutcome::Added) => "已添加选项".to_string(),
        None => answer_recorded_toast(remaining),
    }
}

/// Toast after recording a single-select answer, with how many questions are
/// still open.
fn answer_recorded_toast(remaining: usize) -> String {
    format!("已记录答案，还有 {} 题未答", remaining)
}

/// The generic "already answered" replay a losing question click gets when the
/// winning click's result is not recorded yet (it is still in flight).
pub(crate) fn question_replay_card(inline: bool) -> CardActionResult {
    let mut r = result_card("✅ 已回答", "green", "已提交 AI 的问题答案。");
    if inline {
        r.card = None;
    }
    r
}

/// The truthful result for a click cola cannot classify: the state is gone and
/// the directory was never listed successfully (fresh process before the first
/// sweep, failing lists, unknown directory). The request may or may not be
/// resolved — the card must not claim either way.
pub(crate) fn stale_question_card(inline: bool) -> CardActionResult {
    let mut r = result_card("❓ 卡片已失效", "grey", "此问答卡片已失效，请使用最新卡片作答。");
    if inline {
        r.card = None;
    }
    r.toast = Some("此卡片已失效".to_string());
    r
}

/// Body of the completion card for a standalone question request: EVERY
/// question with the answer(s) the user gave, so the finished card keeps the
/// full Q&A instead of echoing only the last clicked answer (which used to be
/// mislabeled "AI 的问题是：<answer>"). Empty answer slots (skipped via
/// submit/skip) render as 未作答.
pub(crate) fn qa_completion_body(
    questions: &[crate::opencode::client::QuestionInfo],
    answers: &[Vec<String>],
) -> String {
    let mut lines: Vec<String> = Vec::new();
    for (i, (q, a)) in questions.iter().zip(answers).enumerate() {
        lines.push(format!("**{}. {}**", i + 1, q.question));
        lines.push(if a.is_empty() {
            "（未作答）".to_string()
        } else {
            format!("👉 {}", a.join("、"))
        });
    }
    if lines.is_empty() {
        "已提交 AI 的问题答案。".to_string()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A state with one question per `multi` flag (other fields irrelevant to
    /// the transitions under test).
    fn state_with(multi: &[bool]) -> QuestionState {
        let questions = multi
            .iter()
            .enumerate()
            .map(|(i, m)| crate::opencode::client::QuestionInfo {
                question: format!("q{i}"),
                header: String::new(),
                options: vec![],
                multiple: Some(*m),
                custom: None,
            })
            .collect();
        QuestionState::new(
            crate::opencode::client::QuestionRequest {
                id: "q1".into(),
                session_id: "ses_1".into(),
                questions,
            },
            "/work".into(),
        )
    }

    #[test]
    fn record_answer_starts_empty_and_fills_slots() {
        let mut state = state_with(&[false, false]);
        // First answer, index 0 of 2.
        state.record_answer(0, "main");
        assert_eq!(state.answers, vec![Some(vec!["main".to_string()]), None]);
        assert_eq!(state.merge().0, 1);
        // Second answer, index 1 → all filled.
        state.record_answer(1, "dev");
        assert_eq!(
            state.answers,
            vec![Some(vec!["main".to_string()]), Some(vec!["dev".to_string()])]
        );
        assert_eq!(state.merge().0, 2);
    }

    #[test]
    fn record_answer_replacing_a_slot_keeps_count_stable() {
        let mut state = state_with(&[false, false]);
        state.record_answer(0, "a");
        state.record_answer(1, "b");
        // Re-answering slot 0 replaces it without changing the count.
        state.record_answer(0, "c");
        assert_eq!(
            state.answers,
            vec![Some(vec!["c".to_string()]), Some(vec!["b".to_string()])]
        );
        assert_eq!(state.merge().0, 2);
    }

    #[test]
    fn record_answer_out_of_range_index_is_ignored() {
        let mut state = state_with(&[false, false]);
        state.record_answer(99, "x");
        assert_eq!(state.answers, vec![None, None]);
        assert_eq!(state.merge().0, 0);
    }

    #[test]
    fn remember_refresh_keeps_recorded_answers() {
        // A re-armed follow or re-claim refreshes request/dir in place; the
        // user's recorded selection survives.
        let mut state = state_with(&[false]);
        state.record_answer(0, "a");
        state.refresh(&state.request.clone(), "/new");
        assert_eq!(state.dir, "/new");
        assert_eq!(state.answers, vec![Some(vec!["a".to_string()])]);
    }

    #[test]
    fn is_multi_reads_the_question_flag() {
        let state = state_with(&[false, true]);
        assert!(!state.is_multi(0));
        assert!(state.is_multi(1));
        assert!(!state.is_multi(99));
    }

    #[test]
    fn record_toggle_adds_then_removes_labels() {
        let mut state = state_with(&[true]);
        // First toggle adds the label.
        state.record_toggle(0, "a");
        assert_eq!(state.toggles, vec![Some(vec!["a".to_string()])]);
        // Second toggle accumulates a second label.
        state.record_toggle(0, "b");
        assert_eq!(state.toggles, vec![Some(vec!["a".to_string(), "b".to_string()])]);
        // Toggling one off keeps the other.
        state.record_toggle(0, "a");
        assert_eq!(state.toggles, vec![Some(vec!["b".to_string()])]);
        // Toggling the last off reverts the slot to None (question open again).
        state.record_toggle(0, "b");
        assert_eq!(state.toggles, vec![None]);
        // An unconfirmed toggle never counts as a final answer.
        assert_eq!(state.merge().0, 0);
    }

    #[test]
    fn record_toggle_out_of_range_index_is_ignored() {
        let mut state = state_with(&[true, true]);
        state.record_toggle(99, "x");
        assert_eq!(state.toggles, vec![None, None]);
    }

    #[test]
    fn record_append_adds_but_never_toggles_away() {
        let mut state = state_with(&[true]);
        // First custom answer adds the label.
        state.record_append(0, "自定");
        assert_eq!(state.toggles, vec![Some(vec!["自定".to_string()])]);
        // Re-adding the same custom label is a no-op (deduped).
        state.record_append(0, "自定");
        assert_eq!(state.toggles, vec![Some(vec!["自定".to_string()])]);
        // A second custom label accumulates.
        state.record_append(0, "另一个");
        assert_eq!(
            state.toggles,
            vec![Some(vec!["自定".to_string(), "另一个".to_string()])]
        );
    }

    #[test]
    fn record_append_out_of_range_index_is_ignored() {
        let mut state = state_with(&[true, true]);
        state.record_append(99, "x");
        assert_eq!(state.toggles, vec![None, None]);
    }

    #[test]
    fn merge_separates_done_from_in_progress_toggles() {
        // Q0 answered (single-select), Q1 toggled (multi, in progress).
        let mut state = state_with(&[false, true]);
        state.record_answer(0, "/a");
        state.record_toggle(1, "苹果");
        let (count, display, done) = state.merge();
        // Only the finalized question counts toward submit.
        assert_eq!(count, 1);
        assert_eq!(done, vec![true, false]);
        // Display shows the locked answer AND the live toggles.
        assert_eq!(
            display,
            vec![Some(vec!["/a".to_string()]), Some(vec!["苹果".to_string()])]
        );
    }

    #[test]
    fn merge_confirmed_multi_select_counts_as_done() {
        // A confirmed multi-select moves its toggles into the final answers.
        let mut state = state_with(&[true]);
        state.record_toggle(0, "苹果");
        state.record_toggle(0, "香蕉");
        state.confirm(0);
        assert_eq!(
            state.answers,
            vec![Some(vec!["苹果".to_string(), "香蕉".to_string()])]
        );
        assert_eq!(state.toggles, vec![None]);
        let (count, display, done) = state.merge();
        assert_eq!(count, 1);
        assert_eq!(done, vec![true]);
        assert_eq!(display, vec![Some(vec!["苹果".to_string(), "香蕉".to_string()])]);
    }

    #[test]
    fn confirm_locks_toggles_and_a_stale_confirm_is_a_no_op() {
        let mut state = state_with(&[true]);
        // Confirming with nothing toggled is the "不选" path: an empty answer.
        state.confirm(0);
        assert_eq!(state.answers, vec![Some(Vec::new())]);
        // A stale second confirm must not overwrite the locked answer.
        state.record_toggle(0, "苹果");
        state.confirm(0);
        assert_eq!(state.answers, vec![Some(Vec::new())]);
        assert_eq!(state.toggles, vec![Some(vec!["苹果".to_string()])]);
    }

    #[test]
    fn toggle_selected_reads_the_live_toggles() {
        let mut state = state_with(&[true, true]);
        assert!(!state.toggle_selected(0, "a"));
        state.record_toggle(0, "a");
        assert!(state.toggle_selected(0, "a"));
        assert!(!state.toggle_selected(0, "b"));
        // A different question index is a different selection.
        assert!(!state.toggle_selected(1, "a"));
        // Exact match only — a custom answer with the same prefix is not it.
        state.record_append(1, "a b");
        assert!(state.toggle_selected(1, "a b"));
        assert!(!state.toggle_selected(1, "a"));
    }

    #[test]
    fn action_toast_names_each_outcome() {
        assert_eq!(
            action_toast("custom", 1, Some(MultiOutcome::Added)),
            "已添加自定义答案"
        );
        assert_eq!(action_toast("answer", 1, Some(MultiOutcome::Added)), "已添加选项");
        assert_eq!(
            action_toast("answer", 1, Some(MultiOutcome::Removed)),
            "已移除选项"
        );
        assert_eq!(
            action_toast("custom", 1, Some(MultiOutcome::Duplicate)),
            "该选项已在已选中"
        );
        assert_eq!(action_toast("confirm", 2, None), "已确定该题，还有 2 题未答");
        // A single-select click keeps its remaining-questions count.
        assert_eq!(action_toast("answer", 1, None), "已记录答案，还有 1 题未答");
    }
}
