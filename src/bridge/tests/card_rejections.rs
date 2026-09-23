//! Card-content rejection recovery (`230099`): Feishu refuses to compile a
//! card whose markdown it cannot parse (a model-written Feishu tag), whose
//! table budget is blown, or whose image key is invalid. The rejection is
//! deterministic — resending the same JSON fails on every future flush, which
//! silently freezes the turn's card. The flush must recognize the typed error,
//! degrade every model-markdown element to a code fence (the one form the
//! parser accepts unconditionally), and retry the same slice once.

use std::sync::Arc;

use crate::bridge::streaming::{CardFallback, CardSession, StreamAccumulator};
use crate::bridge::test_support::*;
use crate::feishu::card::CardState;

/// The markdown content of the first body element.
fn first_markdown(card: &serde_json::Value) -> String {
    card["body"]["elements"][0]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Every card the platform PATCHed onto `message_id`, in call order.
async fn updates_of(platform: &RecordingPlatform, message_id: &str) -> Vec<serde_json::Value> {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage {
                message_id: mid,
                card,
            } if mid == message_id => Some(card.clone()),
            _ => None,
        })
        .collect()
}

/// Every card the platform was asked to send, in call order.
async fn sent_cards(platform: &RecordingPlatform) -> Vec<serde_json::Value> {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
            _ => None,
        })
        .collect()
}

/// Seed a live card whose text holds a construct Feishu refuses (a bare
/// Feishu tag) and the in-flight guard.
async fn seed_live_card(app: &Arc<App>, text: &str) {
    let mut acc = StreamAccumulator::new("回合");
    acc.card_state = CardState::Streaming;
    acc.push_text(text);
    acc.reply_to_message_id = Some("msg_1".into());
    app.cards
        .lock()
        .await
        .insert("ses_test".into(), CardSession::new(acc, Some("om_live".into())));
    app.inflight.lock().await.insert("ses_test".to_string());
}

async fn app_with_live_card(text: &str) -> (Arc<App>, Arc<RecordingPlatform>) {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(
        App::new(
            cfg,
            Arc::new(MockBackend::new(realistic_parts())),
            platform.clone(),
        )
        .unwrap(),
    );
    seed_session(&app, "ses_test", "/work").await;
    seed_live_card(&app, text).await;
    (app, platform)
}

/// A `230099` rejection flips the turn to the fenced fallback and the SAME
/// slice is re-sent fenced — the card recovers instead of freezing.
#[tokio::test]
async fn a_rejected_card_update_is_retried_fenced() {
    let (app, platform) = app_with_live_card("回答 <number_tag> 里。").await;
    platform
        .fail_update_card_content_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    crate::bridge::turn::flush::flush_card(&app.cards_handle(), "ses_test").await;

    let updates = updates_of(&platform, "om_live").await;
    assert_eq!(
        updates.len(),
        2,
        "the rejected attempt and the fenced retry: {updates:?}"
    );
    assert!(
        first_markdown(&updates[0]).contains("&#60;number_tag>"),
        "the first attempt is the plain sanitized render: {}",
        updates[0]
    );
    let retry = first_markdown(&updates[1]);
    assert!(
        retry.starts_with("```") && retry.contains("<number_tag>"),
        "the retry fences the model markdown: {retry}"
    );
    let cards = app.cards.lock().await;
    assert_eq!(
        cards.get("ses_test").unwrap().acc.card_fallback,
        CardFallback::Fenced,
        "the fallback is sticky for the turn"
    );
}

/// A rejection on the FINALIZED patch (the live slice overflowed and was
/// finalized in place) must restore the slice and re-send it fenced on the
/// same card — not lose it to a continuation that starts after it.
#[tokio::test]
async fn a_rejected_finalized_update_is_re_sent_fenced_on_the_same_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(
        App::new(
            cfg,
            Arc::new(MockBackend::new(realistic_parts())),
            platform.clone(),
        )
        .unwrap(),
    );
    seed_session(&app, "ses_test", "/work").await;

    // Two exactly-full text slices: the first build fills the card and
    // finalizes it (advancing `render_from` to the second slice).
    let max = crate::feishu::card::MAX_CARD_TEXT_CHARS;
    let slice = |i: usize| format!("【S{i:02}】{}", "长".repeat(max - 5));
    let mut acc = StreamAccumulator::new("回合");
    acc.card_state = CardState::Streaming;
    acc.push_text(&slice(0));
    acc.push_text(&slice(1));
    acc.reply_to_message_id = Some("msg_1".into());
    app.cards
        .lock()
        .await
        .insert("ses_test".into(), CardSession::new(acc, Some("om_live".into())));
    app.inflight.lock().await.insert("ses_test".to_string());
    platform
        .fail_update_card_content_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    crate::bridge::turn::flush::flush_card(&app.cards_handle(), "ses_test").await;

    let updates = updates_of(&platform, "om_live").await;
    assert_eq!(
        updates.len(),
        2,
        "the rejected finalize and its fenced re-send: {updates:?}"
    );
    assert!(
        !first_markdown(&updates[0]).starts_with("```"),
        "the first finalize is the plain render"
    );
    assert!(
        first_markdown(&updates[1]).starts_with("```"),
        "the re-send fences the slice: {}",
        updates[1]
    );
    // The second slice continues on a new card, as a normal full-slice flush.
    let continuation = platform
        .calls
        .lock()
        .await
        .iter()
        .find_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
            _ => None,
        })
        .expect("the remainder continues on a new card");
    assert!(
        continuation.to_string().contains("【S01】"),
        "the continuation carries the second slice: {continuation}"
    );
}

/// The continuation `reply_card` can be rejected too: its retry re-renders the
/// same slice fenced and lands.
#[tokio::test]
async fn a_rejected_continuation_send_is_retried_fenced() {
    let (app, platform) = app_with_live_card("回答 <number_tag> 里。").await;
    // The tracked card is already finalized, so this flush owes a continuation.
    {
        let mut cards = app.cards.lock().await;
        let session = cards.get_mut("ses_test").unwrap();
        session.card_is_live = false;
        session.acc.reply_to_message_id = Some("msg_1".into());
    }
    platform
        .fail_reply_card_content_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    crate::bridge::turn::flush::flush_card(&app.cards_handle(), "ses_test").await;

    let cards = sent_cards(&platform).await;
    assert_eq!(
        cards.len(),
        2,
        "the rejected send and the fenced retry: {cards:?}"
    );
    assert!(
        cards[1].to_string().contains("```") && cards[1].to_string().contains("<number_tag>"),
        "the retry fences the model markdown: {}",
        cards[1]
    );
    assert_eq!(
        app.cards.lock().await.get("ses_test").unwrap().acc.card_fallback,
        CardFallback::Fenced
    );
}

/// A second rejection means fencing cannot help: the card is suspended, and a
/// later flush makes no further PATCH attempts (no API hammering).
#[tokio::test]
async fn a_second_rejection_suspends_the_card() {
    let (app, platform) = app_with_live_card("回答 <number_tag> 里。").await;
    platform
        .fail_update_card_content_count
        .store(2, std::sync::atomic::Ordering::SeqCst);

    crate::bridge::turn::flush::flush_card(&app.cards_handle(), "ses_test").await;
    assert_eq!(
        updates_of(&platform, "om_live").await.len(),
        2,
        "the plain attempt and the fenced one only"
    );
    assert_eq!(
        app.cards.lock().await.get("ses_test").unwrap().acc.card_fallback,
        CardFallback::Suspended
    );

    // A later poll must not PATCH the suspended card again.
    crate::bridge::turn::flush::flush_card(&app.cards_handle(), "ses_test").await;
    assert_eq!(
        updates_of(&platform, "om_live").await.len(),
        2,
        "a suspended card is not retried on later flushes"
    );
}
