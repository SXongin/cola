//! Waiting-message pins (ADR-0043 amendment): the in-chat locator that pairs
//! with Instant Reminder's list-level nudge.
//!
//! Feishu's Instant Reminder (`time_sensitive`) pulls a Chat/Topic to the top
//! of the message list, but the pinned entry previews only the chat's newest
//! message — in a chat with several topics a wait in an older topic is
//! invisible from the list. Message pins (`POST /im/v1/pins`) put the exact
//! waiting card into the chat's pinned-message list instead: tapping the
//! reminder and opening the chat shows what needs answering.
//!
//! Lifecycle: a live Permission/Question's host card (the streaming card that
//! renders it inline, or the standalone card it was sent as) is pinned while
//! the request waits, and unpinned once it leaves the pending list. One card
//! can host both kinds, so pins are reference-counted by message: the last
//! waiting request off a card unpins it.
//!
//! Pins are best-effort like the reminder: failures log and are retried while
//! the request still waits (a failed pin) or stays tracked (a failed unpin).
//! Cola pins each card once, on the transition from "no waiting request here"
//! to "one": a user who unpins a waiting card by hand is not fought — no
//! state change means no re-pin. State is in-memory and not reconciled at
//! startup: a pin orphaned by a crash stays until the user removes it (its
//! message id is unknowable after a restart, and the next wait pins a new
//! card).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::bridge::snapshot_claims::ClaimKind;
use crate::feishu::Platform;

/// One waiting request's host card, as a sweep found it: which request waits,
/// the directory that listed it (so a failed directory's wait is never read
/// as resolved, #130), and the message rendering its live controls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WaitingCard {
    pub(crate) request_id: String,
    pub(crate) directory: String,
    pub(crate) message_id: String,
}

/// What cola believes it has pinned for one request.
#[derive(Clone, Debug)]
struct Pinned {
    kind: ClaimKind,
    directory: String,
    message_id: String,
}

#[derive(Default)]
struct Inner {
    /// request_id → the pinned card currently rendering its controls.
    by_request: HashMap<String, Pinned>,
    /// message_id → how many live requests' controls that message renders (a
    /// card can host a Permission and a Question at once). Absent means cola
    /// believes the message is not pinned.
    refs: HashMap<String, usize>,
}

/// The waiting-card pin registry: one pin call per (request → host card)
/// transition, one unpin per card's last leaving request. `[bridge]
/// instant_reminder` gates the whole feature, like the reminder itself.
pub(crate) struct MessagePins {
    enabled: bool,
    inner: Mutex<Inner>,
}

impl MessagePins {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            inner: Mutex::new(Inner::default()),
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Reconcile one flow's waiting requests with the message pins: `waiting`
    /// is exactly the requests this flow currently surfaces and that still
    /// need the user (auto-resolved and cola-claimed requests are already
    /// excluded by the caller). A tracked request of this kind that left
    /// `waiting` releases its pin; one whose host card changed releases the
    /// old card and pins the new; a tracked request from a failed directory
    /// stays (#130: unknown is never resolved).
    pub(crate) async fn sync(
        &self,
        feishu: &Arc<dyn Platform>,
        kind: ClaimKind,
        waiting: &[WaitingCard],
        failed_dirs: &HashSet<String>,
    ) {
        if !self.enabled {
            return;
        }
        let mut inner = self.inner.lock().await;
        let wanted: HashMap<&str, &WaitingCard> =
            waiting.iter().map(|w| (w.request_id.as_str(), w)).collect();
        // Releases first: a request that resolved (or moved to another card)
        // must free its old card before the pin pass below can pin anything.
        let releases: Vec<(String, String)> = inner
            .by_request
            .iter()
            .filter(|(_, pinned)| pinned.kind == kind)
            .filter(|(id, pinned)| match wanted.get(id.as_str()) {
                // Still waiting on the same card: nothing to do.
                Some(want) if want.message_id == pinned.message_id => false,
                // Moved to another card, or gone. A gone request from a
                // directory that did not list is unknown, not resolved.
                Some(_) => true,
                None => !failed_dirs.contains(&pinned.directory),
            })
            .map(|(id, pinned)| (id.clone(), pinned.message_id.clone()))
            .collect();
        for (request_id, message_id) in releases {
            Self::release_locked(&mut inner, feishu, &request_id, &message_id).await;
        }
        // Pin pass: every waiting request whose card is not tracked yet. A
        // card shared by two requests is pinned by the first and only
        // reference-counted by the second.
        for want in waiting {
            if inner.by_request.contains_key(&want.request_id) {
                // Tracked already (a failed release kept it): a later sweep
                // retries instead of leaking the old pin.
                continue;
            }
            if let Some(refs) = inner.refs.get_mut(&want.message_id) {
                *refs += 1;
                inner.by_request.insert(
                    want.request_id.clone(),
                    Pinned {
                        kind,
                        directory: want.directory.clone(),
                        message_id: want.message_id.clone(),
                    },
                );
                continue;
            }
            match feishu.pin_message(&want.message_id).await {
                Ok(()) => {
                    inner.refs.insert(want.message_id.clone(), 1);
                    inner.by_request.insert(
                        want.request_id.clone(),
                        Pinned {
                            kind,
                            directory: want.directory.clone(),
                            message_id: want.message_id.clone(),
                        },
                    );
                    tracing::info!(
                        "waiting card {} pinned (request {})",
                        want.message_id,
                        want.request_id
                    );
                }
                Err(e) => tracing::warn!(
                    "waiting card pin {} failed (best-effort; retried while it waits): {}",
                    want.message_id,
                    e
                ),
            }
        }
    }

    /// Drop one request's hold on its card; the card is unpinned once no
    /// request relies on it any more. A failed unpin restores the hold so a
    /// later sweep retries instead of leaking the pin.
    async fn release_locked(
        inner: &mut Inner,
        feishu: &Arc<dyn Platform>,
        request_id: &str,
        message_id: &str,
    ) {
        let Some(pinned) = inner.by_request.get(request_id) else {
            return;
        };
        if pinned.message_id != message_id {
            return; // superseded by a later pin
        }
        let last = match inner.refs.get_mut(message_id) {
            Some(refs) => {
                *refs = refs.saturating_sub(1);
                *refs == 0
            }
            None => true,
        };
        if !last {
            inner.by_request.remove(request_id);
            return;
        }
        match feishu.unpin_message(message_id).await {
            Ok(()) => {
                inner.refs.remove(message_id);
                inner.by_request.remove(request_id);
                tracing::info!("waiting card {} unpinned (request {})", message_id, request_id);
            }
            Err(e) => {
                // Keep the hold (and this request) tracked: the next sweep
                // retries the unpin.
                *inner.refs.entry(message_id.to_string()).or_insert(0) += 1;
                tracing::warn!(
                    "waiting card unpin {} failed (best-effort; kept for a later sweep): {}",
                    message_id,
                    e
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::test_support::RecordingPlatform;

    fn waiting(request: &str, dir: &str, message: &str) -> WaitingCard {
        WaitingCard {
            request_id: request.into(),
            directory: dir.into(),
            message_id: message.into(),
        }
    }

    fn no_dirs() -> HashSet<String> {
        HashSet::new()
    }

    /// The recorder (for assertions) plus its trait-object handle (for
    /// `MessagePins::sync`, which takes `&Arc<dyn Platform>`).
    fn test_platform() -> (Arc<RecordingPlatform>, Arc<dyn Platform>) {
        let recorder = Arc::new(RecordingPlatform::new());
        let feishu: Arc<dyn Platform> = recorder.clone();
        (recorder, feishu)
    }

    /// One waiting card is pinned exactly once; resolution unpins it once.
    #[tokio::test]
    async fn a_waiting_message_pins_once_and_resolution_unpins_once() {
        let pins = MessagePins::new(true);
        let (platform, feishu) = test_platform();
        let card = waiting("per_1", "/work", "msg_card");

        pins.sync(
            &feishu,
            ClaimKind::Permission,
            std::slice::from_ref(&card),
            &no_dirs(),
        )
        .await;
        assert_eq!(platform.message_pins().await, vec![("msg_card".into(), true)]);

        // Idempotent: the same waiting request makes no second call.
        pins.sync(&feishu, ClaimKind::Permission, &[card], &no_dirs())
            .await;
        assert_eq!(platform.message_pins().await.len(), 1);

        // Resolved elsewhere: the next complete sweep unpins.
        pins.sync(&feishu, ClaimKind::Permission, &[], &no_dirs()).await;
        assert_eq!(
            platform.message_pins().await,
            vec![("msg_card".into(), true), ("msg_card".into(), false)],
            "one pin, one unpin"
        );

        pins.sync(&feishu, ClaimKind::Permission, &[], &no_dirs()).await;
        assert_eq!(platform.message_pins().await.len(), 2, "nothing tracked any more");
    }

    /// A card hosting two waits (both kinds) stays pinned until the LAST one
    /// leaves — the refcount never unpins a card another wait still uses.
    #[tokio::test]
    async fn a_shared_card_keeps_the_pin_until_the_last_request_leaves() {
        let pins = MessagePins::new(true);
        let (platform, feishu) = test_platform();
        let perm = waiting("per_1", "/work", "msg_card");
        let ques = waiting("que_1", "/work", "msg_card");

        pins.sync(
            &feishu,
            ClaimKind::Permission,
            std::slice::from_ref(&perm),
            &no_dirs(),
        )
        .await;
        pins.sync(
            &feishu,
            ClaimKind::Question,
            std::slice::from_ref(&ques),
            &no_dirs(),
        )
        .await;
        assert_eq!(
            platform.message_pins().await,
            vec![("msg_card".into(), true)],
            "the shared card is pinned once"
        );

        // One request leaves: the other still holds the card.
        pins.sync(&feishu, ClaimKind::Permission, &[], &no_dirs()).await;
        assert_eq!(platform.message_pins().await.len(), 1, "still pinned");

        // The last one leaves: unpin.
        pins.sync(&feishu, ClaimKind::Question, &[], &no_dirs()).await;
        assert_eq!(
            platform.message_pins().await,
            vec![("msg_card".into(), true), ("msg_card".into(), false)]
        );
    }

    /// A directory that failed to list said nothing: its wait stays pinned
    /// until a complete sweep reports it gone (#130).
    #[tokio::test]
    async fn a_failed_directory_keeps_its_wait_pinned() {
        let pins = MessagePins::new(true);
        let (platform, feishu) = test_platform();
        let card = waiting("per_1", "/work", "msg_card");

        pins.sync(
            &feishu,
            ClaimKind::Permission,
            std::slice::from_ref(&card),
            &no_dirs(),
        )
        .await;
        let failed: HashSet<String> = ["/work".to_string()].into_iter().collect();
        pins.sync(&feishu, ClaimKind::Permission, &[], &failed).await;
        assert_eq!(platform.message_pins().await.len(), 1, "unknown is not resolved");

        pins.sync(&feishu, ClaimKind::Permission, &[], &no_dirs()).await;
        assert_eq!(
            platform.message_pins().await.len(),
            2,
            "the complete sweep unpins"
        );
    }

    /// A request whose host card changed (a card re-host/split) unpins the old
    /// message and pins the new one, in that order.
    #[tokio::test]
    async fn a_moved_card_releases_the_old_pin_and_pins_the_new() {
        let pins = MessagePins::new(true);
        let (platform, feishu) = test_platform();
        let old = waiting("per_1", "/work", "msg_old");
        let new = waiting("per_1", "/work", "msg_new");

        pins.sync(&feishu, ClaimKind::Permission, &[old], &no_dirs())
            .await;
        pins.sync(&feishu, ClaimKind::Permission, &[new], &no_dirs())
            .await;
        assert_eq!(
            platform.message_pins().await,
            vec![
                ("msg_old".into(), true),
                ("msg_old".into(), false),
                ("msg_new".into(), true),
            ]
        );
    }

    /// A failed pin is retried while the request waits; a failed unpin stays
    /// tracked and is retried — never a leaked pin, never a fabricated clear.
    #[tokio::test]
    async fn failures_are_retried_without_leaks() {
        let pins = MessagePins::new(true);
        let (platform, feishu) = test_platform();
        platform.fail_pin.store(true, std::sync::atomic::Ordering::SeqCst);
        let card = waiting("per_1", "/work", "msg_card");

        pins.sync(
            &feishu,
            ClaimKind::Permission,
            std::slice::from_ref(&card),
            &no_dirs(),
        )
        .await;
        pins.sync(
            &feishu,
            ClaimKind::Permission,
            std::slice::from_ref(&card),
            &no_dirs(),
        )
        .await;
        assert_eq!(
            platform.message_pins().await,
            vec![("msg_card".into(), true), ("msg_card".into(), true)],
            "a failed pin is retried while the wait lasts"
        );

        // The pin lands on the next sweep; the resolved request then unpins.
        platform
            .fail_pin
            .store(false, std::sync::atomic::Ordering::SeqCst);
        pins.sync(&feishu, ClaimKind::Permission, &[card], &no_dirs())
            .await;
        platform.fail_pin.store(true, std::sync::atomic::Ordering::SeqCst);
        pins.sync(&feishu, ClaimKind::Permission, &[], &no_dirs()).await;
        pins.sync(&feishu, ClaimKind::Permission, &[], &no_dirs()).await;
        let calls = platform.message_pins().await;
        assert_eq!(
            calls.len(),
            5,
            "two failed pins, one good pin, two failed unpins: {calls:?}"
        );
        assert_eq!(calls.last(), Some(&("msg_card".into(), false)));

        platform
            .fail_pin
            .store(false, std::sync::atomic::Ordering::SeqCst);
        pins.sync(&feishu, ClaimKind::Permission, &[], &no_dirs()).await;
        pins.sync(&feishu, ClaimKind::Permission, &[], &no_dirs()).await;
        assert_eq!(
            platform.message_pins().await.len(),
            6,
            "the unpin lands exactly once"
        );
    }

    /// The opt-in default: off means no pin call is ever made.
    #[tokio::test]
    async fn disabled_records_no_pin_calls() {
        let pins = MessagePins::new(false);
        let (platform, feishu) = test_platform();

        pins.sync(
            &feishu,
            ClaimKind::Permission,
            &[waiting("per_1", "/work", "msg_card")],
            &no_dirs(),
        )
        .await;
        assert!(platform.message_pins().await.is_empty());
    }
}
