use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::bridge::core::SharedCore;
use crate::bridge::request::PendingRequest;
use crate::bridge::snapshot::SnapshotData;
use crate::feishu::snapshot_card::SnapshotQuestionState;

/// Which request kind a snapshot claim hosts (ADR-0028). One registry holds
/// both kinds' claims, and each flow's poll sweep only drops claims of its own
/// kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimKind {
    Permission,
    Question,
}

/// The rebuild state of one snapshot card that claims adopt-time pendings
/// (ADR-0028): the verb + title its header was built with and the full
/// adopt-time read state. The claimed blocks are a subset of `data.pending`;
/// re-render filters them by what is still claimed.
struct ClaimedSnapshot {
    verb: String,
    title: String,
    data: SnapshotData,
}

/// The ADR-0028 claim registry: which snapshot card hosts which adopt-time
/// pending block, what each snapshot was built from, and the tombstones for
/// late second clicks. All three maps change together — claim, resolve and
/// prune are single method calls on this type, so the required order cannot
/// live in caller comments.
#[derive(Default)]
pub struct SnapshotClaims {
    /// request_id → (claiming snapshot message id, the request's kind). The
    /// poll loop treats a claimed id as already-surfaced (no standalone card,
    /// no re-inline); a claimed id leaving the pending list drops its block.
    claims: HashMap<String, (String, ClaimKind)>,
    /// snapshot message id → the adopt-time state the card was built from,
    /// kept so the card can be re-rendered in place when a claimed request
    /// resolves (via the snapshot's own buttons or by another client).
    hosts: HashMap<String, ClaimedSnapshot>,
    /// request ids whose block was resolved via a snapshot card. A late second
    /// click on the same block must keep patching the snapshot — never replace
    /// the whole card with a standalone result card. Pruned together with the
    /// host entry once no claim refers to the snapshot anymore.
    tombstones: HashSet<String>,
}

impl SnapshotClaims {
    /// Whether `request_id` is claimed by a snapshot card.
    pub fn contains(&self, request_id: &str) -> bool {
        self.claims.contains_key(request_id)
    }

    /// The claim's owning snapshot message id and kind, if any.
    pub fn claim_of(&self, request_id: &str) -> Option<(&str, ClaimKind)> {
        self.claims
            .get(request_id)
            .map(|(message_id, kind)| (message_id.as_str(), *kind))
    }

    /// Whether a resolved `request_id` must keep patching its snapshot (a late
    /// second click), instead of returning a standalone result card.
    pub fn is_tombstoned(&self, request_id: &str) -> bool {
        self.tombstones.contains(request_id)
    }

    /// The host's adopt-time pendings, the list a re-render filters by the
    /// claims that survive.
    pub fn host_pending(&self, message_id: &str) -> Option<Vec<PendingRequest>> {
        self.hosts.get(message_id).map(|h| h.data.pending.clone())
    }

    /// Number of live claims (test assertions on registry drain).
    #[cfg(test)]
    pub fn claim_count(&self) -> usize {
        self.claims.len()
    }

    /// Register a SENT snapshot card as the host of its embedded pendings: the
    /// poll loop then treats every claimed id as already surfaced. Called
    /// after the card was sent, with its message id.
    ///
    /// Precondition: every pending is still claimable — `claimable_pendings`
    /// filtered the ones surfaced elsewhere — so an id is never claimed twice
    /// by construction. A re-claim would overwrite the existing entry,
    /// matching the pre-C6 behaviour.
    pub fn claim(&mut self, message_id: &str, verb: &str, title: &str, data: &SnapshotData) {
        self.hosts
            .entry(message_id.to_string())
            .or_insert_with(|| ClaimedSnapshot {
                verb: verb.to_string(),
                title: title.to_string(),
                data: data.clone(),
            });
        for req in &data.pending {
            let kind = req.claim_kind();
            self.claims
                .insert(req.id().to_string(), (message_id.to_string(), kind));
            tracing::info!(
                "snapshot {} claims {} {} on session {}",
                message_id,
                req.id(),
                if kind == ClaimKind::Permission {
                    "permission"
                } else {
                    "question"
                },
                req.session_id()
            );
        }
    }

    /// Re-render a host card in place: its blocks filtered to the claims that
    /// are still live, with the given live question state for them. `None`
    /// when the host entry is gone.
    pub fn rebuild(
        &self,
        message_id: &str,
        question_state: &SnapshotQuestionState,
    ) -> Option<serde_json::Value> {
        let host = self.hosts.get(message_id)?;
        let mut data = host.data.clone();
        data.pending
            .retain(|r| self.claim_of(r.id()).is_some_and(|(mid, _)| mid == message_id));
        Some(crate::feishu::snapshot_card::build_snapshot_card_with_state(
            &host.verb,
            &host.title,
            &data,
            question_state,
        ))
    }

    /// Resolve one claimed block (`request_id` was answered via this snapshot
    /// or vanished): drop its claim, tombstone it so a late second click keeps
    /// patching the snapshot, re-render the host without it, and prune the
    /// host once no claim refers to it. Returns the card JSON to patch in
    /// place (the ack path); `None` when the host entry is already gone.
    ///
    /// Precondition: `message_id` is the claim's own host. Callers take it
    /// from [`Self::claim_of`] (or from the snapshot they just sent); the
    /// registry does not re-validate, matching the pre-C6 sequence whose
    /// caller already held the message id.
    pub fn resolve(
        &mut self,
        request_id: &str,
        message_id: &str,
        question_state: &SnapshotQuestionState,
    ) -> Option<serde_json::Value> {
        self.claims.remove(request_id);
        self.tombstones.insert(request_id.to_string());
        let card = self.rebuild(message_id, question_state);
        self.prune(message_id);
        card
    }

    /// Drop the claims of `kind` whose request left the pending list (resolved
    /// by another client, or already dropped by a button click), tombstone
    /// them, and re-render each affected host ONCE without the resolved
    /// blocks. A claim hosted from a directory whose list call failed is kept:
    /// that directory said nothing, so its request may still be pending
    /// (#130, #144). Returns `(message_id, card)` for the caller to patch;
    /// never marks the snapshot stale — that patch targets standalone cards.
    pub fn drop_vanished(
        &mut self,
        kind: ClaimKind,
        pending: &HashSet<String>,
        failed_dirs: &HashSet<String>,
    ) -> Vec<(String, serde_json::Value)> {
        let resolved: Vec<String> = self
            .claims
            .iter()
            .filter(|(id, (message_id, k))| {
                *k == kind
                    && !pending.contains(*id)
                    && !self
                        .hosts
                        .get(message_id)
                        .is_some_and(|host| failed_dirs.contains(&host.data.directory))
            })
            .map(|(id, _)| id.clone())
            .collect();
        if resolved.is_empty() {
            return Vec::new();
        }
        let mut affected: Vec<String> = Vec::new();
        for id in &resolved {
            if let Some((message_id, _)) = self.claims.get(id)
                && !affected.contains(message_id)
            {
                affected.push(message_id.clone());
            }
        }
        for id in &resolved {
            self.claims.remove(id);
            self.tombstones.insert(id.clone());
        }
        let empty_state = SnapshotQuestionState::new();
        let mut dropped = Vec::new();
        for message_id in affected {
            if let Some(card) = self.rebuild(&message_id, &empty_state) {
                dropped.push((message_id.clone(), card));
            }
            self.prune(&message_id);
        }
        dropped
    }

    /// Drop the host (and its tombstones) once no claim refers to it — the
    /// card is fully resolved and nothing can re-render it anymore.
    fn prune(&mut self, message_id: &str) {
        let still_claimed = self.claims.values().any(|(mid, _)| mid == message_id);
        if still_claimed {
            return;
        }
        if let Some(host) = self.hosts.remove(message_id) {
            for req in &host.data.pending {
                self.tombstones.remove(req.id());
            }
        }
    }
}

/// Whether a pending request is already surfaced elsewhere — a standalone card
/// in a flow's `sent_cards`, an inline section on a live streaming card, or a
/// block on an EARLIER snapshot (re-switch dedupe). Such a request must NOT be
/// embedded/claimed by a snapshot: the existing card stays authoritative
/// (ADR-0028).
pub(crate) async fn is_already_surfaced(core: &Arc<SharedCore>, req: &PendingRequest) -> bool {
    match req {
        PendingRequest::Permission(p) => {
            if core
                .permission
                .sent_cards
                .lock()
                .await
                .contains_key(&p.request_id)
            {
                return true;
            }
        }
        PendingRequest::Question(q) => {
            if core.question.sent_cards.lock().await.contains_key(&q.id) {
                return true;
            }
        }
    }
    if core.snapshot_claims.lock().await.contains(req.id()) {
        return true;
    }
    let cards = core.cards.lock().await;
    cards.values().any(|c| {
        c.acc.pending_permissions.iter().any(|p| p.request_id == req.id())
            || c.acc.pending_questions.iter().any(|q| q.request_id == req.id())
    })
}

/// ADR-0028: restrict the gathered adopt-time state to the pendings the
/// snapshot may embed and claim — the adopted session's own requests minus any
/// already surfaced elsewhere. Called BEFORE the snapshot card is built, so an
/// already-surfaced request never shows a duplicate block on the snapshot.
pub(crate) async fn claimable_pendings(core: &Arc<SharedCore>, mut data: SnapshotData) -> SnapshotData {
    let mut claimable: Vec<PendingRequest> = Vec::new();
    for req in &data.pending {
        if is_already_surfaced(core, req).await {
            tracing::info!(
                "snapshot: {} {} already surfaced; not embedded",
                req.id(),
                req.session_id()
            );
            continue;
        }
        claimable.push(req.clone());
    }
    data.pending = claimable;
    data
}

/// ADR-0028: register the snapshot's embedded pendings as claimed (the poll
/// loop then treats them as surfaced) and remember question requests so the
/// block buttons resolve (the poll loop never sees claimed requests, so
/// `prepare()` never ran for them). Called AFTER the snapshot card is sent,
/// with its message id.
pub(crate) async fn claim_snapshot_pendings(
    core: &Arc<SharedCore>,
    snapshot_message_id: &str,
    verb: &str,
    title: &str,
    data: &SnapshotData,
) {
    core.snapshot_claims
        .lock()
        .await
        .claim(snapshot_message_id, verb, title, data);
    for req in &data.pending {
        if let PendingRequest::Question(q) = req {
            core.question.remember_question(q, &data.directory).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode;

    fn perm(request_id: &str) -> PendingRequest {
        PendingRequest::Permission(opencode::types::PermissionRequest {
            request_id: request_id.into(),
            session_id: Some("ses_1".into()),
            permission: Some("bash".into()),
            patterns: vec!["ls".into()],
            metadata: None,
            always: vec![],
        })
    }

    fn data_with(ids: &[&str]) -> SnapshotData {
        SnapshotData {
            session_id: "ses_1".into(),
            directory: "/work".into(),
            status: None,
            pending: ids.iter().map(|id| perm(id)).collect(),
            tail: vec![],
            newest_user_epoch: None,
            newest_user_is_cola_authored: false,
        }
    }

    fn empty_state() -> SnapshotQuestionState {
        SnapshotQuestionState::new()
    }

    #[test]
    fn claim_registers_host_and_claims() {
        let mut claims = SnapshotClaims::default();
        claims.claim("mid_1", "接管", "title", &data_with(&["p1", "p2"]));
        assert_eq!(claims.claim_count(), 2);
        assert_eq!(claims.claim_of("p1"), Some(("mid_1", ClaimKind::Permission)));
        assert!(claims.host_pending("mid_1").is_some());
        assert_eq!(claims.host_pending("mid_1").unwrap().len(), 2);
    }

    #[test]
    fn resolve_drops_claim_and_tombstones_but_keeps_the_host() {
        let mut claims = SnapshotClaims::default();
        claims.claim("mid_1", "接管", "title", &data_with(&["p1", "p2"]));
        let card = claims.resolve("p1", "mid_1", &empty_state());
        assert!(card.is_some(), "host card rebuilt without the block");
        assert!(!claims.contains("p1"));
        assert!(claims.is_tombstoned("p1"), "late click keeps patching");
        // p2's claim keeps the host entry (and the rebuild path) alive.
        assert!(claims.host_pending("mid_1").is_some());
        assert_eq!(claims.claim_of("p2"), Some(("mid_1", ClaimKind::Permission)));
    }

    #[test]
    fn resolve_prunes_the_host_and_its_tombstones_on_the_last_claim() {
        let mut claims = SnapshotClaims::default();
        claims.claim("mid_1", "接管", "title", &data_with(&["p1", "p2"]));
        claims.resolve("p1", "mid_1", &empty_state());
        let card = claims.resolve("p2", "mid_1", &empty_state());
        assert!(card.is_some(), "last resolve still returns the drop card");
        assert_eq!(claims.claim_count(), 0);
        assert!(
            claims.host_pending("mid_1").is_none(),
            "host pruned with its last claim"
        );
        assert!(
            !claims.is_tombstoned("p1"),
            "tombstones die with the host they patch"
        );
    }

    #[test]
    fn drop_vanished_rebuilds_each_affected_host_once() {
        let mut claims = SnapshotClaims::default();
        claims.claim("mid_1", "接管", "title", &data_with(&["p1", "p2"]));
        claims.claim("mid_2", "接管", "title", &data_with(&["p3"]));
        // p1 vanished (kind permission); p2 stays pending.
        let pending: HashSet<String> = ["p2".to_string(), "p3".to_string()].into_iter().collect();
        let dropped = claims.drop_vanished(ClaimKind::Permission, &pending, &HashSet::new());
        assert_eq!(dropped.len(), 1, "only the affected host is rebuilt");
        assert_eq!(dropped[0].0, "mid_1");
        assert!(!claims.contains("p1"));
        assert!(claims.is_tombstoned("p1"));
        assert!(claims.contains("p2"), "still-pending claim kept");
        assert!(claims.contains("p3"));

        // The last claim of mid_2 vanishing drops the whole host.
        let pending: HashSet<String> = ["p2".to_string()].into_iter().collect();
        let dropped = claims.drop_vanished(ClaimKind::Permission, &pending, &HashSet::new());
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].0, "mid_2");
        assert!(claims.host_pending("mid_2").is_none());
        assert!(
            !claims.is_tombstoned("p3"),
            "host gone → nothing can re-render, tombstone pruned"
        );
    }

    /// #144: a claim hosted from a directory whose list call failed must
    /// survive — that directory said nothing, so the request may still be
    /// pending.
    #[test]
    fn drop_vanished_keeps_claims_from_a_failed_directory() {
        let mut claims = SnapshotClaims::default();
        claims.claim("mid_1", "接管", "title", &data_with(&["p1"]));
        let failed: HashSet<String> = ["/work".to_string()].into_iter().collect();
        let dropped = claims.drop_vanished(ClaimKind::Permission, &HashSet::new(), &failed);
        assert!(dropped.is_empty(), "a failed list must not drop the claim");
        assert!(claims.contains("p1"));
        assert!(!claims.is_tombstoned("p1"));
    }

    #[test]
    fn drop_vanished_only_touches_its_own_kind() {
        let mut claims = SnapshotClaims::default();
        claims.claim("mid_1", "接管", "title", &data_with(&["p1"]));
        let pending: HashSet<String> = HashSet::new();
        let dropped = claims.drop_vanished(ClaimKind::Question, &pending, &HashSet::new());
        assert!(dropped.is_empty(), "a permission claim survives the other sweep");
        assert!(claims.contains("p1"));
    }
}
