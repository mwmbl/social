//! The Stage 0 scenario: fetch, consent, index, search, delete upstream, sweep, gone. Against
//! an in-process fake `ActivityPub` host; no network beyond 127.0.0.1.

mod common;

use std::time::Duration;

use common::TestEnv;
use mwmbl_social::decide::Action;
use mwmbl_social::now_secs;
use mwmbl_social::sweep::{SweepConfig, step};
use mwmbl_social::writer::{ForgetTarget, IndexName};
use store::{FetchKind, FetchReason, TombstoneKind};

fn uris_of(env: &TestEnv) -> Vec<String> {
    env.post_pages().into_iter().map(|p| p.uri).collect()
}

/// A sweep config where every record counts as stale.
fn everything_stale() -> SweepConfig {
    SweepConfig {
        step_interval: Duration::from_millis(1),
        revalidate_after_secs: -3600,
        queue_cap: 1000,
    }
}

async fn sweep_everything(env: &mut TestEnv) {
    let config = everything_stale();
    for (index, pages) in [(IndexName::Posts, 64), (IndexName::Accounts, 64)] {
        for _ in 0..pages {
            step(
                index,
                pages,
                &mut env.store,
                &env.handle,
                &config,
                now_secs(),
            )
            .await
            .unwrap();
        }
    }
}

#[tokio::test]
async fn a_consented_post_and_its_author_are_found() {
    let env = TestEnv::new().await;
    let processed = env
        .ingest("/notes/public", FetchKind::Object, FetchReason::Announce)
        .await;
    assert!(processed.retry.is_none(), "{processed:?}");
    assert!(
        processed
            .actions
            .iter()
            .any(|a| matches!(a, Action::IndexPost { .. }))
    );

    let found = env.search_posts("garlic");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].uri, env.fake.url("/notes/public"));
    assert_eq!(found[0].author_uri, env.fake.url("/users/alice"));
    assert!(env.search_posts("zebrafinch").len() == 1);

    // The author was fetched for consent and is in account search.
    let accounts = env.search_accounts("alice");
    assert_eq!(accounts.len(), 1, "{accounts:?}");
    assert_eq!(accounts[0].uri, env.fake.url("/users/alice"));
    assert!(accounts[0].acct.starts_with("alice@127.0.0.1"));

    // Nothing was written to the tombstone table.
    assert!(env.store.list_tombstones().unwrap().is_empty());
}

#[tokio::test]
async fn unlisted_and_non_indexable_posts_are_never_indexed() {
    let env = TestEnv::new().await;
    for path in ["/notes/unlisted", "/notes/bobs"] {
        let processed = env
            .ingest(path, FetchKind::Unknown, FetchReason::Announce)
            .await;
        assert!(processed.retry.is_none(), "{path}: {processed:?}");
        assert!(
            !processed
                .actions
                .iter()
                .any(|a| matches!(a, Action::IndexPost { .. })),
            "{path}: {processed:?}"
        );
    }
    for word in ["quokka", "unlisted", "narwhal", "migration", "bob"] {
        assert!(env.search_posts(word).is_empty(), "{word}");
    }
    // Nothing of theirs is on any page.
    for record in env.post_pages() {
        assert!(
            !record.uri.contains("/notes/unlisted") && !record.uri.contains("/notes/bobs"),
            "{record:?}"
        );
        assert!(
            !record.extract.contains("quokka") && !record.extract.contains("narwhal"),
            "{record:?}"
        );
    }
    assert!(env.post_pages().is_empty());
    // Bob opted out of indexing, so his posts are hidden by an author tombstone as well.
    let tombstones = env.store.list_tombstones().unwrap();
    assert!(
        tombstones
            .iter()
            .any(|t| t.kind == TombstoneKind::Author && t.uri == env.fake.url("/users/bob"))
    );
}

#[tokio::test]
async fn deleted_upstream_is_removed_by_the_sweep_without_a_tombstone() {
    let mut env = TestEnv::new().await;
    env.ingest("/notes/public", FetchKind::Object, FetchReason::Announce)
        .await;
    assert_eq!(env.search_posts("garlic").len(), 1);
    let note_uri = env.fake.url("/notes/public");
    assert!(uris_of(&env).contains(&note_uri));

    env.fake.set_gone("/notes/public");
    // The index doesn't know yet.
    assert_eq!(env.search_posts("garlic").len(), 1);

    sweep_everything(&mut env).await;
    assert!(env.store.queue_len().unwrap() >= 1);
    env.worker.drain(&mut env.store).await.unwrap();
    assert_eq!(env.store.queue_len().unwrap(), 0);

    assert!(env.search_posts("garlic").is_empty());
    assert!(
        !uris_of(&env).contains(&note_uri),
        "a copy is left on a page"
    );
    assert!(
        env.store.list_tombstones().unwrap().is_empty(),
        "revalidation needs no tombstone"
    );
    assert_eq!(env.handle.stats().await.unwrap().tombstones, 0);

    // The author is still live and still searchable.
    assert_eq!(env.search_accounts("alice").len(), 1);
}

#[tokio::test]
async fn an_announced_delete_leaves_a_tombstone() {
    let env = TestEnv::new().await;
    env.ingest("/notes/public", FetchKind::Object, FetchReason::Announce)
        .await;
    assert_eq!(env.search_posts("garlic").len(), 1);

    env.fake.set_gone("/notes/public");
    let processed = env
        .ingest("/notes/public", FetchKind::Object, FetchReason::Announce)
        .await;
    assert!(processed.retry.is_none());
    assert!(processed.actions.iter().any(|a| matches!(
        a,
        Action::RemovePost {
            tombstone: true,
            ..
        }
    )));

    assert!(env.search_posts("garlic").is_empty());
    assert!(!uris_of(&env).contains(&env.fake.url("/notes/public")));
    let tombstones = env.store.list_tombstones().unwrap();
    assert!(
        tombstones
            .iter()
            .any(|t| t.kind == TombstoneKind::Object && t.uri == env.fake.url("/notes/public"))
    );
    assert_eq!(env.handle.stats().await.unwrap().tombstones, 1);
}

#[tokio::test]
async fn forgetting_a_domain_hides_everything_from_it_at_once_and_stops_fetching() {
    let env = TestEnv::new().await;
    env.ingest("/notes/public", FetchKind::Object, FetchReason::Announce)
        .await;
    assert_eq!(env.search_posts("garlic").len(), 1);
    assert_eq!(env.search_accounts("alice").len(), 1);

    env.handle
        .forget(ForgetTarget::Domain("127.0.0.1".into()))
        .await
        .unwrap();
    assert!(env.search_posts("garlic").is_empty());
    assert!(env.search_accounts("alice").is_empty());
    assert_eq!(env.store.list_domain_blocks().unwrap().len(), 1);

    // The worker no longer fetches from it.
    let before = env.fake.total_hits();
    let processed = env
        .ingest("/notes/public", FetchKind::Object, FetchReason::Announce)
        .await;
    assert!(processed.actions.is_empty() && processed.retry.is_none());
    assert_eq!(env.fake.total_hits(), before);
}

#[tokio::test]
async fn a_domain_blocked_revalidation_removes_the_carried_record() {
    let mut env = TestEnv::new().await;
    env.ingest("/notes/public", FetchKind::Object, FetchReason::Announce)
        .await;
    sweep_everything(&mut env).await;
    env.handle
        .forget(ForgetTarget::Domain("127.0.0.1".into()))
        .await
        .unwrap();
    let before = env.fake.total_hits();
    env.worker.drain(&mut env.store).await.unwrap();
    assert_eq!(env.fake.total_hits(), before, "nothing fetched");
    assert_eq!(env.store.queue_len().unwrap(), 0);
    assert!(env.post_pages().is_empty(), "removed from the pages too");
    assert!(env.account_pages().is_empty());
}

#[tokio::test]
async fn transient_failures_retry_with_backoff_then_give_up() {
    let mut env = TestEnv::with_attempts(2).await;
    env.fake.set_failing("/notes/public");
    let uri = env.fake.url("/notes/public");
    env.store
        .enqueue(
            &store::NewFetch {
                uri: uri.clone(),
                kind: FetchKind::Object,
                reason: FetchReason::Announce,
                record: None,
                not_before: 0,
            },
            0,
            100,
        )
        .unwrap();
    env.worker.drain(&mut env.store).await.unwrap();
    let queued = env.store.next_due(i64::MAX, 10).unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].attempts, 1);
    assert!(queued[0].not_before > now_secs(), "backed off");
    assert!(queued[0].last_error.is_some());

    // The second failure reaches max_transient_attempts: complete and give up.
    let item = queued[0].clone();
    let processed = env.worker.process(&item, now_secs()).await;
    let finished = env
        .worker
        .finish(&env.store, &item, &processed, now_secs())
        .unwrap();
    assert_eq!(finished, mwmbl_social::worker::Finished::GaveUp);
    assert_eq!(env.store.queue_len().unwrap(), 0);
    assert!(env.post_pages().is_empty());
}
