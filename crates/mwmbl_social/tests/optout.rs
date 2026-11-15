//! Account opt-outs: an `update` announcement for an author who turned off `indexable` makes
//! their posts disappear from search once the queue drains. Against the in-process fake host.

mod common;

use common::TestEnv;
use mwmbl_social::now_secs;
use store::{FetchKind, FetchReason, NewFetch, TombstoneKind};

fn announce_account_update(env: &TestEnv, path: &str) {
    let now = now_secs();
    env.store
        .enqueue(
            &NewFetch {
                uri: env.fake.url(path),
                kind: FetchKind::Actor,
                reason: FetchReason::Announce,
                record: None,
                not_before: now,
            },
            now,
            1000,
        )
        .unwrap();
}

#[tokio::test]
async fn turning_off_indexable_removes_the_authors_posts_from_search() {
    let mut env = TestEnv::new().await;
    env.ingest("/notes/public", FetchKind::Object, FetchReason::Announce)
        .await;
    assert_eq!(env.search_posts("garlic").len(), 1);
    assert_eq!(env.search_accounts("alice").len(), 1);

    // Alice opts out of indexing (still discoverable), and the server announces the update.
    let mut actor = env.fake.doc("/users/alice").unwrap();
    actor["indexable"] = false.into();
    env.fake.set_doc("/users/alice", actor);
    announce_account_update(&env, "/users/alice");
    let processed = env.worker.drain(&mut env.store).await.unwrap();
    assert!(processed >= 1);
    assert_eq!(env.store.queue_len().unwrap(), 0, "the queue drained");

    // Her posts are hidden at once, by an author tombstone ...
    assert!(env.search_posts("garlic").is_empty());
    assert!(env.search_posts("zebrafinch").is_empty());
    assert!(
        env.store
            .list_tombstones()
            .unwrap()
            .iter()
            .any(|t| t.kind == TombstoneKind::Author && t.uri == env.fake.url("/users/alice"))
    );
    // ... while the still-discoverable account itself stays searchable.
    assert_eq!(env.search_accounts("alice").len(), 1);
}

#[tokio::test]
async fn turning_indexable_back_on_lets_new_posts_in_again() {
    let mut env = TestEnv::new().await;
    env.ingest("/notes/public", FetchKind::Object, FetchReason::Announce)
        .await;
    let mut actor = env.fake.doc("/users/alice").unwrap();
    actor["indexable"] = false.into();
    env.fake.set_doc("/users/alice", actor.clone());
    announce_account_update(&env, "/users/alice");
    env.worker.drain(&mut env.store).await.unwrap();
    assert!(env.search_posts("garlic").is_empty());

    actor["indexable"] = true.into();
    env.fake.set_doc("/users/alice", actor);
    announce_account_update(&env, "/users/alice");
    env.worker.drain(&mut env.store).await.unwrap();
    // The opt-in is back: re-fetching the post indexes it again.
    env.ingest("/notes/public", FetchKind::Object, FetchReason::Announce)
        .await;
    assert_eq!(env.search_posts("garlic").len(), 1);
}

#[tokio::test]
async fn a_post_processed_after_the_opt_out_is_not_indexed_from_a_stale_author_copy() {
    let mut env = TestEnv::new().await;
    // The author document is now cached by the worker.
    env.ingest("/notes/public", FetchKind::Object, FetchReason::Announce)
        .await;
    let mut actor = env.fake.doc("/users/alice").unwrap();
    actor["indexable"] = false.into();
    env.fake.set_doc("/users/alice", actor);
    announce_account_update(&env, "/users/alice");
    env.worker.drain(&mut env.store).await.unwrap();

    env.ingest("/notes/public", FetchKind::Object, FetchReason::Announce)
        .await;
    assert!(env.search_posts("garlic").is_empty());
    assert!(env.post_pages().is_empty());
}
