//! The guarded card map's concurrency contract (ticket #1136).
//!
//! [`CardMap`](crate::bridge::handles::card_map::CardMap) promises two things,
//! and these tests name them so a regression that re-exposes the raw map or
//! drops the guard fails instead of quietly re-opening the seam:
//!
//! 1. a session's card-write lock serializes that session alone: two sessions
//!    write under their OWN write locks, and A's parked write lock does not
//!    block B's full per-session write;
//! 2. a [`card`](crate::bridge::handles::card_map::CardMap::card) guard holds
//!    the map mutex for EVERY session while it is alive, so it keeps every
//!    other per-session acquisition out until it drops.

use crate::bridge::test_support::{MockBackend, core_with, test_config, test_work_dir};
use crate::bridge::turn::Turn;

/// A bare core's card handle over a fresh empty map, with `sessions` seeded as
/// empty card sessions (the per-session write lock and map door are what these
/// tests exercise; no Turn runs).
async fn cards_with(sessions: &[&str]) -> crate::bridge::handles::CardsHandle {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let cards = core_with(&cfg, MockBackend::new(Vec::new())).cards_handle();
    for session in sessions {
        Turn::seed_card(&cards, session, Some(&format!("om_{session}"))).await;
    }
    cards
}

/// Two sessions write under their own write locks: A holds its lock across a
/// park while B's whole read-guard-write sequence completes. Both writes land
/// once A is released; A's parked lock is the only thing held across the park.
#[tokio::test]
async fn two_sessions_write_under_their_own_write_locks() {
    let _wd = test_work_dir();
    let cards = cards_with(&["ses_a", "ses_b"]).await;

    let (holding_tx, holding_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let writer_a = {
        let cards = cards.clone();
        tokio::spawn(async move {
            // A's write lock, held across the park: the write that follows is
            // A's alone, and it takes the map guard only for its own step.
            let write_lock = cards.write_lock("ses_a").await;
            let _held = write_lock.lock().await;
            holding_tx.send(()).expect("the test is waiting");
            release_rx.await.expect("the test releases the writer");
            Turn::set_reply_target(&cards, "ses_a", "msg_a").await;
        })
    };
    holding_rx.await.expect("A holds its write lock");

    // B's write lock is a different lock, so B's full write completes while A
    // holds A's.
    {
        let write_lock = cards.write_lock("ses_b").await;
        let _held = write_lock.lock().await;
        Turn::set_reply_target(&cards, "ses_b", "msg_b").await;
    }
    assert_eq!(
        Turn::reply_target(&cards, "ses_b").await.as_deref(),
        Some("msg_b"),
        "B's write landed while A held A's write lock"
    );
    assert_eq!(
        Turn::reply_target(&cards, "ses_a").await,
        None,
        "A has written nothing while its writer is parked"
    );

    release_tx.send(()).expect("A's writer is waiting");
    writer_a.await.expect("A's writer task");

    assert_eq!(
        Turn::reply_target(&cards, "ses_a").await.as_deref(),
        Some("msg_a"),
        "A's write lands once its write lock is released"
    );
    assert_eq!(
        Turn::reply_target(&cards, "ses_b").await.as_deref(),
        Some("msg_b"),
        "B's write stayed landed"
    );
}

/// A held `card()` guard keeps another session's per-session acquisition out
/// until it drops — the hold-then-await case. The waiter reports through
/// oneshots; no sleeps and no `try_lock` seam.
#[tokio::test]
async fn a_held_card_guard_keeps_another_session_out_until_it_drops() {
    let _wd = test_work_dir();
    let cards = cards_with(&["ses_a", "ses_b"]).await;

    let guard = cards.card("ses_a").await.expect("the seeded card");

    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (acquired_tx, acquired_rx) = tokio::sync::oneshot::channel::<()>();
    let waiter = {
        let cards = cards.clone();
        tokio::spawn(async move {
            started_tx.send(()).expect("the test is waiting");
            let card = cards.card("ses_b").await;
            assert!(card.is_some(), "the seeded card stays in the map");
            acquired_tx.send(()).expect("the test is waiting");
        })
    };
    // The waiter has begun and is parked on the map mutex the guard holds.
    started_rx.await.expect("the map waiter starts");
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(
        !waiter.is_finished(),
        "a held card guard keeps another session's acquisition out"
    );

    drop(guard);
    acquired_rx
        .await
        .expect("the second acquisition resolves once the guard drops");
    waiter.await.expect("the map waiter task");
}
