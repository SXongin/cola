//! The harness's own contract: the one `wait_for` combinator returns what its
//! probe yields and names the condition it timed out on. A new wait reuses the
//! combinator instead of growing `test_support.rs` with another hand-rolled
//! poll loop.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::bridge::test_support::wait_for;

#[tokio::test]
async fn the_combinator_returns_what_its_probe_yields() {
    let polls = AtomicUsize::new(0);
    let value = wait_for("the probe yields on its third poll", || async {
        let poll = polls.fetch_add(1, Ordering::SeqCst) + 1;
        (poll == 3).then_some(poll)
    })
    .await;

    assert_eq!(value, 3, "the probe's value is the wait's value");
    assert_eq!(polls.load(Ordering::SeqCst), 3, "the probe ran until it yielded");
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "the probe never yields")]
async fn the_combinator_names_the_condition_it_timed_out_on() {
    // Paused time makes the shared deadline instant, so the bound costs no
    // wall-clock time.
    let _ = wait_for::<(), _, _>("the probe never yields", || async { None }).await;
}
