pub mod card;
pub mod client;
pub(crate) mod delivery;
pub mod event;
pub(crate) mod message;
pub(crate) mod pbbp2;
pub mod snapshot_card;
pub mod ws;

use crate::error::Result;
use async_trait::async_trait;
use client::Client;
use serde_json::Value;
use std::time::Duration;

/// A one-shot callback the delivery layer runs when the Pending Card Update a
/// Completion Notice is gated on finally drains (spec #602, ticket #607). The
/// bridge builds the closure (it spawns the notice send, which needs the Turn's
/// own facts); the delivery layer only fires it, exactly once, when the write
/// it was armed against delivers.
pub(crate) type DeferredNotice = Box<dyn FnOnce() + Send + 'static>;

/// What a Completion Notice gate found (spec #602, ticket #607): whether the
/// card the terminal slice was written to still owes that write, so the notice
/// must wait for the retry's drain instead of racing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoticeGate {
    /// The card still owes a keyless Pending Card Update: the callback was
    /// stored and fires when that update delivers.
    Armed,
    /// Nothing is owed — the terminal write already landed (or the platform
    /// does not observe card writes): send the notice now.
    Delivered,
    /// The card's newest write settled without delivering (a permanent
    /// refusal): nothing will ever carry the terminal slice there, so the
    /// notice is suppressed.
    Never,
}

/// The Feishu platform, abstracted so the bridge core can be tested with a
/// recording adapter (captures every card cola would send) or, in the live
/// smoke test, the real [`Client`].
#[async_trait]
pub trait Platform: Send + Sync {
    async fn get_ws_endpoint(&self) -> Result<String>;

    async fn reply_card(&self, reply_to: &str, card: &Value) -> Result<String>;

    async fn send_card(&self, receive_id_type: &str, receive_id: &str, card: &Value) -> Result<String>;

    async fn update_message(&self, message_id: &str, card: &Value) -> Result<()>;

    /// Submit a **keyed submission** for ordered delivery (spec #571): an
    /// already-composed payload carrying the **chain generation** its writer
    /// decided under and the intent naming the logical write, with an optional
    /// **fallback** payload for a platform that refuses the primary as card
    /// content (spec #571 review). The delivery
    /// decorator serializes it against the card's other writers — dropping a
    /// stale generation, collapsing a settled `(generation, intent)`, keeping a
    /// recoverable failure owed, trying the fallback inside the same key — and
    /// returns the submission's completion ticket. The default implementation
    /// is the unordered fallback: a platform with no delivery decorator (a test
    /// fake, an unwrapped adapter) writes the payload straight through
    /// [`Self::update_message`], degrading to the fallback on a typed content
    /// rejection exactly as the queue's driver does.
    async fn submit_ordered(&self, submission: delivery::KeyedSubmission<'_>) -> delivery::CardWriteTicket {
        let outcome = match self.update_message(submission.message_id, submission.card).await {
            Err(e) if delivery::is_card_content_rejection(&e) => match submission.fallback {
                Some(fallback) => match self.update_message(submission.message_id, fallback).await {
                    Ok(()) => delivery::WriteOutcome::Delivered,
                    Err(e) => delivery::WriteOutcome::Failed(e),
                },
                None => delivery::WriteOutcome::Failed(e),
            },
            Ok(()) => delivery::WriteOutcome::Delivered,
            Err(e) => delivery::WriteOutcome::Failed(e),
        };
        delivery::CardWriteTicket::settled_now(outcome)
    }

    /// Whether the keyed queue already **covers** `(generation, intent)` for
    /// `message_id` (spec #571): the generation is closed (a settle ended it),
    /// a `Stamp` at or below an accepted ending's generation is shadowed (the
    /// waiting yield / the settle is the card's state), the key settled —
    /// delivered, so it is on the card, or permanently refused, so it is final
    /// (#522) — or a write of the exact key is already in flight or waiting in
    /// the queue. Either way a writer's re-decision owes no work, and the card
    /// must not be read again for it. A recoverable failure's payload is left
    /// **owed** with no driver: a later re-decision may still replace it (and
    /// the drain retries it), so that reports `false`. The query only
    /// remembers; it never orders or claims anything, so consulting it before
    /// a read cannot resurrect the retired in-flight claim. Default `false` —
    /// a platform with no delivery decorator remembers nothing.
    fn keyed_write_covered(
        &self,
        _message_id: &str,
        _generation: u64,
        _intent: delivery::CardWriteIntent,
    ) -> bool {
        false
    }

    /// How long a writer awaits an issued keyed submission's completion ticket
    /// before proceeding without a verdict (spec #571 review): an issued keyed
    /// write is **never cancelled** — a cancelled PATCH could still commit at
    /// Feishu and land over a newer generation — so a slow or hung one can
    /// outlive its caller. The writer bounds its own wait instead and then
    /// proceeds, leaving the write owned by the queue, which may still land it.
    /// The delivery decorator forwards this to the platform it wraps, so a
    /// test fake may shorten it; production uses the delivery layer's bound.
    fn keyed_ticket_await(&self) -> Duration {
        delivery::KEYED_TICKET_AWAIT
    }

    /// Whether `(generation, intent)` already **settled** on `message_id`'s
    /// keyed queue (spec #571 review): `Some(true)` delivered — the card holds
    /// the logical write — `Some(false)` permanently refused, so it will never
    /// write again (#522), and `None` unsettled (or no remembered queue state;
    /// an unwrapped platform remembers nothing by default). A writer that
    /// re-decides every tick consults this before its card-view read, so a
    /// settled key costs no read and no submission. The query only reports.
    fn keyed_write_settled(
        &self,
        _message_id: &str,
        _generation: u64,
        _intent: delivery::CardWriteIntent,
    ) -> Option<bool> {
        None
    }

    async fn reply_text(&self, message_id: &str, text: &str) -> Result<String>;

    /// Reply to a message in thread form (`reply_in_thread: true`) with an
    /// interactive card, creating a topic around a non-topic seed message.
    /// Returns the created reply's `message_id` (an anchor inside the topic)
    /// and the topic's `thread_id` (or `None` if the chat does not support
    /// topic replies). Used by `/topic --adopt` to place the Session Snapshot
    /// card as the topic's first message and anchor (ADR-0028).
    async fn reply_card_in_thread(&self, message_id: &str, card: &Value) -> Result<(String, Option<String>)>;

    /// Reply to a message in thread form (`reply_in_thread: true`), creating a
    /// topic. Returns the created reply's `message_id` (an anchor inside the
    /// topic) and the topic's `thread_id` (or `None` if the chat does not
    /// support topic replies).
    async fn reply_in_thread(&self, message_id: &str, text: &str) -> Result<(String, Option<String>)>;

    /// Reply to a message with a completion notice. When `name` is known the
    /// requester is @-mentioned (`<at user_id="...">name</at>`); otherwise a
    /// plain reply — Feishu still notifies the message's author either way.
    async fn reply_completion_notice(
        &self,
        message_id: &str,
        open_id: &str,
        name: Option<&str>,
        text: &str,
    ) -> Result<String>;

    /// The display name of a user (contact API), best-effort `Ok(None)` on failure.
    async fn user_name(&self, open_id: &str) -> Result<Option<String>>;

    /// The display name of a chat (im API), best-effort `Ok(None)` on failure.
    async fn chat_name(&self, chat_id: &str) -> Result<Option<String>>;

    /// The bot's own open_id, used to recognise @mentions of cola itself.
    async fn bot_open_id(&self) -> Result<String>;

    /// List messages in a container (chat or topic). `container_id_type` is
    /// `"chat"` or `"thread"`; used to find a message INSIDE a topic to reply
    /// into it (the create API cannot target a `thread_id`).
    async fn list_messages(
        &self,
        container_id_type: &str,
        container_id: &str,
    ) -> Result<Vec<client::ChatMessage>>;

    /// Fetch a message by id (`GET /im/v1/messages/{id}`), used to build the
    /// Quoted Context for a reply. Best-effort for the bridge: callers degrade
    /// to text-only on any error (e.g. the `im:message` permission missing).
    async fn get_message(&self, message_id: &str) -> Result<client::FeishuMessage>;

    /// Fetch a card message's currently rendered view
    /// (`GET /im/v1/messages/{id}?card_msg_content_type=user_card_content`):
    /// the schema-2.0 JSON a PATCH of the same message accepts back. The
    /// durable reap reads it to keep an orphaned card's existing body while it
    /// restamps the header (ADR-0063, #434 acceptance feedback). Best-effort:
    /// callers degrade to the bare ending on any error.
    async fn get_card_view(&self, message_id: &str) -> Result<Value>;

    /// Download an image embedded in a message (`GET /im/v1/messages/{id}/resources/{key}?type=image`),
    /// used to attach Image Attachments to a prompt. Requires the `im:message`
    /// permission (already held); callers degrade to a `[图片]` placeholder on
    /// error.
    async fn download_image(&self, message_id: &str, image_key: &str) -> Result<client::ImageAttachment>;

    /// Set or clear a Chat/Topic's **Instant Reminder** (`time_sensitive`,
    /// ADR-0043): a group targets its own `chat_id`, a bot p2p chat targets
    /// the fixed bot feed card. `user_ids` are the open_ids whose message
    /// lists are pinned (the turn's requester). Best-effort for the bridge: a
    /// failure (e.g. the `im:datasync.feed_card.time_sensitive:write` scope
    /// is missing) is logged by the caller and never affects the turn.
    async fn set_instant_reminder(
        &self,
        chat_id: &str,
        is_group: bool,
        user_ids: &[String],
        on: bool,
    ) -> Result<()>;

    /// Pin a message into its Chat/Topic's pinned-message list (ADR-0043
    /// amendment): the in-chat locator for a waiting Permission/Question card.
    /// NOT idempotent at Feishu (pinning twice can fail) — the caller owns
    /// single-shot semantics. Best-effort for the bridge: failures log and the
    /// card lives on.
    async fn pin_message(&self, message_id: &str) -> Result<()>;

    /// Remove a message from its Chat/Topic's pinned-message list. Feishu
    /// reports success for a message that was not pinned (e.g. the user
    /// unpinned it by hand), so this is safe to call unconditionally.
    async fn unpin_message(&self, message_id: &str) -> Result<()>;

    /// Retry the Pending Card Updates that are due now (ADR-0067). A no-op
    /// default — only the delivery decorator owns such a set; the Session Sync
    /// pass calls this every tick, and the WS loop calls it with `force` after
    /// a reconnect so a restored REST path converges immediately.
    async fn drain_pending_card_updates(&self, _force: bool) {}

    /// Whether `message_id` still owes an undelivered write — the Live Card
    /// record's removal consults this (ADR-0063 amendment): a terminal card's
    /// record stays until its ending is confirmed. Both write classes count
    /// (spec #571's amendment): the keyless Pending Card Update and a keyed
    /// ending write (`Settle`) the queue still holds. Default `false` — a
    /// platform with no delivery decorator owes nothing.
    fn has_pending_card_update(&self, _message_id: &str) -> bool {
        false
    }

    /// The sequence of `message_id`'s newest card write when it is still owed
    /// as a Pending Card Update carrying exactly `card` — the Rendered
    /// Cursor's tie to the payload a drain will retry (spec #561). `None` when
    /// nothing is owed, the newest owed payload is a different one, or the
    /// platform does not observe card writes.
    fn pending_card_write(&self, _message_id: &str, _card: &Value) -> Option<u64> {
        None
    }

    /// The sequence of `message_id`'s card write `seq` settled **delivered** —
    /// the confirmation the drain reconcile advances the Rendered Cursor on
    /// (spec #561, review #569): ONLY this exact sequence's own verdict, never
    /// a newer write's. `false` while the write is still owed, was permanently
    /// refused, was superseded or evicted, or is not observed.
    fn card_write_delivered(&self, _message_id: &str, _seq: u64) -> bool {
        false
    }

    /// The sequence of `message_id`'s most recent RECOVERABLE failure when the
    /// payload it carried is exactly `card` (spec #561, review #569): the write
    /// a failure note asks about even after a newer write — a cached repaint
    /// that may omit this payload's delta — replaced the card's entry. `None`
    /// when the newest failure carried another payload, or the platform does
    /// not observe card writes.
    fn failed_card_write(&self, _message_id: &str, _card: &Value) -> Option<u64> {
        None
    }

    /// The settled delivery verdict of `message_id`'s newest card write once
    /// it is no longer owed (spec #561, review #569): `Some(true)` when the
    /// settled write delivered, `Some(false)` when it settled otherwise (a
    /// permanent refusal). `None` while a payload is still owed, the card has
    /// no entry, or the platform does not observe card writes. A failure note
    /// whose payload a drain delivered in the meantime reads `Some(true)` and
    /// may confirm its staged cursor immediately instead of discarding it.
    fn settled_card_write_delivered(&self, _message_id: &str) -> Option<bool> {
        None
    }

    /// Gate a Completion Notice on the terminal write being **accepted** (spec
    /// #602, ticket #607): delivered now, or owed by the delivery layer's
    /// retry. `message_id` is the card carrying the terminal slice and `seq` is
    /// that terminal write's **own** keyless sequence (from
    /// [`Self::pending_card_write`] / [`Self::failed_card_write`]) — the gate
    /// binds to it, never to the entry's current newest, so a newer unrelated
    /// repaint can never answer for it. [`NoticeGate::Armed`] stores `notice` to
    /// fire once that exact write drains; [`NoticeGate::Delivered`] means it
    /// already delivered and the caller sends at once; [`NoticeGate::Never`]
    /// means it was superseded, evicted or permanently refused, so the notice is
    /// suppressed — a write that will never land must not announce the end. The
    /// default — a platform with no delivery decorator — answers
    /// [`NoticeGate::Delivered`], the ungated notice.
    fn defer_notice_until_delivered(
        &self,
        _message_id: &str,
        _seq: u64,
        notice: DeferredNotice,
    ) -> NoticeGate {
        let _ = notice;
        NoticeGate::Delivered
    }
}

#[async_trait]
impl Platform for Client {
    async fn get_ws_endpoint(&self) -> Result<String> {
        Client::get_ws_endpoint(self).await
    }

    async fn reply_card(&self, reply_to: &str, card: &Value) -> Result<String> {
        Client::reply_card(self, reply_to, card).await
    }

    async fn send_card(&self, receive_id_type: &str, receive_id: &str, card: &Value) -> Result<String> {
        Client::send_card(self, receive_id_type, receive_id, card).await
    }

    async fn update_message(&self, message_id: &str, card: &Value) -> Result<()> {
        Client::update_message(self, message_id, card).await
    }

    async fn reply_text(&self, message_id: &str, text: &str) -> Result<String> {
        Client::reply_text(self, message_id, text).await
    }

    async fn reply_card_in_thread(&self, message_id: &str, card: &Value) -> Result<(String, Option<String>)> {
        Client::reply_card_in_thread(self, message_id, card).await
    }

    async fn reply_in_thread(&self, message_id: &str, text: &str) -> Result<(String, Option<String>)> {
        Client::reply_in_thread(self, message_id, text).await
    }

    async fn reply_completion_notice(
        &self,
        message_id: &str,
        open_id: &str,
        name: Option<&str>,
        text: &str,
    ) -> Result<String> {
        Client::reply_completion_notice(self, message_id, open_id, name, text).await
    }

    async fn user_name(&self, open_id: &str) -> Result<Option<String>> {
        Client::user_name(self, open_id).await
    }

    async fn chat_name(&self, chat_id: &str) -> Result<Option<String>> {
        Client::chat_name(self, chat_id).await
    }

    async fn bot_open_id(&self) -> Result<String> {
        Client::bot_open_id(self).await
    }

    async fn list_messages(
        &self,
        container_id_type: &str,
        container_id: &str,
    ) -> Result<Vec<client::ChatMessage>> {
        Client::list_messages(self, container_id_type, container_id).await
    }

    async fn get_message(&self, message_id: &str) -> Result<client::FeishuMessage> {
        Client::get_message(self, message_id).await
    }

    async fn get_card_view(&self, message_id: &str) -> Result<Value> {
        Client::get_card_view(self, message_id).await
    }

    async fn download_image(&self, message_id: &str, image_key: &str) -> Result<client::ImageAttachment> {
        Client::download_image(self, message_id, image_key).await
    }

    async fn set_instant_reminder(
        &self,
        chat_id: &str,
        is_group: bool,
        user_ids: &[String],
        on: bool,
    ) -> Result<()> {
        Client::set_instant_reminder(self, chat_id, is_group, user_ids, on).await
    }

    async fn pin_message(&self, message_id: &str) -> Result<()> {
        Client::pin_message(self, message_id).await
    }

    async fn unpin_message(&self, message_id: &str) -> Result<()> {
        Client::unpin_message(self, message_id).await
    }
}
