//! The delivery observer's contract (charter C7, ticket #1139; ADR-0067,
//! ADR-0072): the keyed queue's three guarantees proved through the real
//! `CardDelivery` behind `Arc<dyn Platform>` — the production assembly shape —
//! and the documented no-op's visible failure on a platform that does not
//! implement the observer.

use std::sync::Arc;

use crate::bridge::test_support::{
    ALWAYS, RecordingPlatform, UpdateFailure, assert_line_level, capture_logs, patches_to, wait_for,
};
use crate::config::FeishuConfig;
use crate::error::BridgeError;
use crate::feishu::NoticeGate;
use crate::feishu::Platform;
use crate::feishu::client::Client;
use crate::feishu::delivery::{
    CardDelivery, CardWriteIntent, CardWriteTicket, KeyedSubmission, WriteOutcome,
};

/// Submit one keyed write through the platform's delivery observer — the
/// production accessor, never the decorator's concrete type.
async fn submit(
    platform: &Arc<dyn Platform>,
    message_id: &str,
    generation: u64,
    intent: CardWriteIntent,
    card: &serde_json::Value,
) -> CardWriteTicket {
    platform
        .delivery()
        .submit_ordered(KeyedSubmission {
            message_id,
            generation,
            intent,
            card,
            fallback: None,
        })
        .await
}

/// The decorated platform plus its recorder.
fn observed() -> (Arc<RecordingPlatform>, Arc<dyn Platform>) {
    let recorder = Arc::new(RecordingPlatform::new());
    let platform: Arc<dyn Platform> = Arc::new(CardDelivery::new(recorder.clone()));
    (recorder, platform)
}

/// A delivery below the card's newest generation is dropped before the wire: a
/// stamp whose card-view read outlived its ending can never paint over it.
#[tokio::test]
async fn a_stale_generation_is_dropped_without_a_write() {
    let (recorder, platform) = observed();

    let newest = serde_json::json!({ "body": "generation one" });
    let delivered = submit(&platform, "om_stale", 1, CardWriteIntent::Stamp, &newest).await;
    assert!(matches!(delivered.settled().await, WriteOutcome::Delivered));
    let writes = patches_to(&recorder, "om_stale").await.len();
    assert_eq!(writes, 1, "the newest generation reached the board once");

    let stale = serde_json::json!({ "body": "generation zero" });
    let ticket = submit(&platform, "om_stale", 0, CardWriteIntent::Stamp, &stale).await;
    assert!(
        matches!(ticket.settled().await, WriteOutcome::Superseded),
        "a generation below the card's floor is superseded, never delivered"
    );
    assert_eq!(
        patches_to(&recorder, "om_stale").await.len(),
        writes,
        "the stale generation never reaches the wire"
    );
}

/// A settled `(generation, intent)` key never writes again: a per-tick
/// re-decision collapses onto the delivery already on the card.
#[tokio::test]
async fn a_settled_key_is_never_written_again() {
    let (recorder, platform) = observed();
    let card = serde_json::json!({ "body": "collect" });

    let first = submit(&platform, "om_settled", 1, CardWriteIntent::Collect, &card).await;
    assert!(matches!(first.settled().await, WriteOutcome::Delivered));
    let writes = patches_to(&recorder, "om_settled").await.len();

    let duplicate = submit(&platform, "om_settled", 1, CardWriteIntent::Collect, &card).await;
    assert!(
        matches!(duplicate.settled().await, WriteOutcome::Delivered),
        "the settled key reports the delivery it already owns"
    );
    assert_eq!(
        patches_to(&recorder, "om_settled").await.len(),
        writes,
        "a settled key costs no second PATCH"
    );
}

/// A recoverable failure keeps the write owed: the caller sees its own
/// attempt's failure, the card still owes the ending, and the drain's retry
/// delivers it and clears the debt.
#[tokio::test]
async fn a_recoverable_failure_stays_owed_and_the_drain_delivers_it() {
    let (recorder, platform) = observed();
    let card = serde_json::json!({ "body": "ending" });
    recorder.fail_update(UpdateFailure::Transport, ALWAYS);

    let first = submit(&platform, "om_owed", 1, CardWriteIntent::Settle, &card).await;
    assert!(
        matches!(first.settled().await, WriteOutcome::Failed(_)),
        "the caller sees its own attempt's recoverable failure"
    );
    assert!(
        platform.delivery().has_pending_card_update("om_owed"),
        "a recoverable failure leaves the ending owed"
    );

    recorder.clear_fail_update();
    platform.delivery().drain_pending_card_updates(true).await;
    wait_for("the drain delivers the owed write", || async {
        (!platform.delivery().has_pending_card_update("om_owed")).then_some(())
    })
    .await;

    assert_eq!(
        patches_to(&recorder, "om_owed").await.len(),
        2,
        "the failed attempt and the drain's retry, and nothing else"
    );
}

/// A platform that does not implement the observer gets the one documented
/// no-op: the ordered write fails visibly (`BridgeError::Feishu` + a WARN), the
/// delivery queries report no state, and nothing ever claims `Delivered`.
#[tokio::test]
async fn a_platform_without_an_observer_fails_the_ordered_write_visibly() {
    // A real transport with no `DeliveryObserver` impl: exactly a platform that
    // skipped the behaviour. The no-op never reaches the network.
    let raw: Arc<dyn Platform> = Arc::new(Client::new(FeishuConfig {
        app_id: "cli_test".into(),
        app_secret: "secret".into(),
    }));
    let card = serde_json::json!({ "body": "never written" });

    let (outcome, logs) = capture_logs(async {
        submit(&raw, "om_unobserved", 1, CardWriteIntent::Stamp, &card)
            .await
            .settled()
            .await
    })
    .await;

    match outcome {
        WriteOutcome::Failed(BridgeError::Feishu(detail)) => assert!(
            detail.contains("ordered delivery is unsupported"),
            "the visible failure names the missing behaviour: {detail}"
        ),
        other => {
            panic!("a platform without an observer must fail the ordered write, never claim it: {other:?}")
        }
    }
    assert_line_level(
        &logs,
        "ordered card write for om_unobserved not delivered",
        "WARN",
    );

    assert!(
        !raw.delivery().has_pending_card_update("om_unobserved"),
        "the no-op owes nothing"
    );
    assert_eq!(
        raw.delivery().pending_card_write("om_unobserved", &card),
        None,
        "the no-op holds no payload"
    );
    assert!(
        !raw.delivery().card_write_delivered("om_unobserved", 1),
        "the no-op never claims a delivery"
    );
}

/// The same no-op, read side: without an observer every delivery query says
/// "nothing owed / never delivered", the notice gate keeps its documented
/// ungated answer, and a drain has no queue to write from.
#[tokio::test]
async fn a_platform_without_an_observer_reports_no_delivery_state() {
    let raw: Arc<dyn Platform> = Arc::new(Client::new(FeishuConfig {
        app_id: "cli_test".into(),
        app_secret: "secret".into(),
    }));
    let card = serde_json::json!({ "body": "never written" });
    let delivery = raw.delivery();

    assert!(
        !delivery.keyed_write_covered("om_unobserved", 1, CardWriteIntent::Stamp),
        "the no-op covers no key"
    );
    assert_eq!(
        delivery.keyed_write_settled("om_unobserved", 1, CardWriteIntent::Stamp),
        None,
        "the no-op remembers no settled key"
    );
    assert_eq!(
        delivery.failed_card_write("om_unobserved", &card),
        None,
        "the no-op remembers no failure"
    );
    assert!(
        !delivery.card_write_delivered("om_unobserved", 1),
        "no sequence is ever delivered"
    );
    assert_eq!(
        delivery.settled_card_write_delivered("om_unobserved"),
        None,
        "no write ever settled"
    );
    assert_eq!(
        delivery.defer_notice_until_delivered("om_unobserved", 1, Box::new(|| {})),
        NoticeGate::Delivered,
        "the no-queue answer is the ungated notice"
    );

    // The drain has no queue: the card owes nothing afterwards and no write
    // leaves. The recorder's observer is the same documented no-op by
    // delegation, so a drain that wrote would show up as a PATCH.
    delivery.drain_pending_card_updates(true).await;
    assert!(
        !delivery.has_pending_card_update("om_unobserved"),
        "the no-op drain leaves nothing owed"
    );
    assert_eq!(
        delivery.pending_card_write("om_unobserved", &card),
        None,
        "the no-op drain retains no payload"
    );
    let recorder = Arc::new(RecordingPlatform::new());
    recorder.delivery().drain_pending_card_updates(true).await;
    assert!(
        patches_to(&recorder, "om_unobserved").await.is_empty(),
        "the no-op drain writes nothing"
    );
}
