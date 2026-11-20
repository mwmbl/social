//! Audit E (2026-11-19): bounds on state a stranger can grow. These fail until fixed.

use store::{FetchKind, FetchReason, NewFetch, Store};

fn announce(uri: &str) -> NewFetch {
    NewFetch {
        uri: uri.to_owned(),
        kind: FetchKind::Actor,
        reason: FetchReason::Announce,
        record: None,
        not_before: 0,
    }
}

/// E5: every signed `Follow` of our actor from a new (stranger) actor enqueues that actor with
/// `FetchReason::Announce` (activities.rs `Effect::Fetch`, inbox_app.rs `enqueue`). When the
/// queue is full of announces there is no `priority > 0` row to evict, so the insert happens
/// anyway and `queue_cap` is not a cap.
#[test]
#[ignore = "AUDIT: E5 - fails until fixed"]
fn announces_cannot_grow_the_queue_past_its_cap() {
    let store = Store::open_in_memory().unwrap();
    let cap = 10;
    for n in 0..100 {
        store
            .enqueue(
                &announce(&format!("https://s{n}.evil.example/actor")),
                1,
                cap,
            )
            .unwrap();
    }
    let len = store.queue_len().unwrap();
    assert!(len <= cap, "queue holds {len} rows with queue_cap {cap}");
}
