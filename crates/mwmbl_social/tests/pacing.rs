//! The worker loop's per-host pacing: one host's revalidation backlog neither starves other
//! hosts nor reaches that host faster than its budget. Against two in-process fake hosts.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{FakeHost, TestEnv};
use mwmbl_social::now_secs;
use mwmbl_social::worker::WorkerConfig;
use store::{FetchKind, FetchReason, NewFetch, QueuedFetch, Store};
use tokio::sync::watch;

const BACKLOG: usize = 30;

fn enqueue(store: &Store, uri: String, reason: FetchReason, not_before: i64, now: i64) {
    let item = NewFetch {
        uri,
        kind: FetchKind::Object,
        reason,
        record: None,
        not_before,
    };
    store.enqueue(&item, now, 1000).unwrap();
}

fn rows_for(store: &Store, origin: &str) -> Vec<QueuedFetch> {
    store
        .next_due(i64::MAX, 1000)
        .unwrap()
        .into_iter()
        .filter(|q| q.uri.starts_with(&format!("{origin}/")))
        .collect()
}

#[tokio::test]
async fn a_big_hosts_revalidations_are_spread_and_do_not_starve_another_host() {
    let env = TestEnv::with_worker(WorkerConfig {
        fetch_concurrency: 4,
        max_transient_attempts: 10,
        tick: Duration::from_millis(10),
        revalidate_interval_per_host: Duration::from_secs(1),
    })
    .await;
    let a = &env.fake;
    let b = FakeHost::start().await;
    let start = now_secs();
    let due = start - 10;
    for i in 0..BACKLOG {
        let now = due + i64::try_from(i).unwrap();
        enqueue(
            &env.store,
            a.url(&format!("/notes/r{i}")),
            FetchReason::Revalidate,
            due,
            now,
        );
    }
    // Queued after all of A's: a revalidation for B, which plain queue order would put last,
    // and an announce, which goes first anyway.
    enqueue(
        &env.store,
        b.url("/notes/bobs"),
        FetchReason::Revalidate,
        due,
        start,
    );
    enqueue(
        &env.store,
        b.url("/notes/public"),
        FetchReason::Announce,
        due,
        start,
    );

    let began = Instant::now();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let worker = tokio::spawn(
        Arc::clone(&env.worker).run(Store::open(&env.config.store_path()).unwrap(), shutdown_rx),
    );
    let mut settled = false;
    for _ in 0..200 {
        let a_due = rows_for(&env.store, &a.origin)
            .iter()
            .filter(|q| q.not_before <= start)
            .count();
        if b.hits("/notes/public") >= 1 && b.hits("/notes/bobs") >= 1 && a_due <= 1 {
            settled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let a_hits = a.total_hits();
    let elapsed = began.elapsed().as_secs();
    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .unwrap()
        .unwrap();
    assert!(settled, "B was fetched and A's backlog was laid out");

    // B was served while nearly all of A's backlog is still queued: A got one fetch, plus at
    // most one per second since.
    assert!(
        a_hits <= usize::try_from(elapsed).unwrap() + 2,
        "{a_hits} fetches to A in {elapsed}s"
    );

    // The rest of A's backlog sits in the queue one second (the interval) apart, after now,
    // with no attempt counted against any of it.
    let rows = rows_for(&env.store, &a.origin);
    let mut deferred: Vec<&QueuedFetch> = rows.iter().filter(|q| q.not_before > start).collect();
    deferred.sort_by_key(|q| q.not_before);
    assert!(
        deferred.len() >= BACKLOG - 3,
        "{} of {BACKLOG} deferred",
        deferred.len()
    );
    for pair in deferred.windows(2) {
        if let [earlier, later] = pair {
            assert!(
                later.not_before > earlier.not_before,
                "{} and {} share a slot",
                earlier.uri,
                later.uri
            );
        }
    }
    assert!(
        deferred
            .iter()
            .all(|q| q.attempts == 0 && q.last_error.is_none())
    );
    let counted = env.worker.counters().revalidations_deferred();
    assert!(
        counted >= u64::try_from(deferred.len()).unwrap(),
        "{counted} deferrals counted"
    );
    assert!(
        rows_for(&env.store, &b.origin).is_empty(),
        "B's items are done"
    );
}
