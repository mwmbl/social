//! The sweep: tombstones are applied to every page, then retired once every page has been
//! swept since they were made.

mod common;

use std::time::Duration;

use common::TestEnv;
use mwmbl_social::decide::Action;
use mwmbl_social::sweep::{SweepConfig, step};
use mwmbl_social::writer::IndexName;
use social_index::SocialPost;
use store::TombstoneKind;

const PAGES: usize = 64;

fn post(uri: &str, extract: &str) -> SocialPost {
    SocialPost {
        uri: uri.into(),
        author_uri: "http://h.example/users/a".into(),
        extract: extract.into(),
        score: 1.0,
        term: String::new(),
        published: 100,
        checked: 10_000_000_000,
        title: None,
        language: None,
    }
}

fn config() -> SweepConfig {
    SweepConfig {
        step_interval: Duration::from_millis(1),
        revalidate_after_secs: 3600,
        queue_cap: 100,
    }
}

async fn pass(env: &mut TestEnv, index: IndexName, now: i64) {
    for _ in 0..PAGES {
        step(index, PAGES, &mut env.store, &env.handle, &config(), now)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn tombstones_are_applied_then_retired_after_a_full_pass_of_both_indexes() {
    let mut env = TestEnv::new().await;
    env.handle
        .apply(vec![
            Action::IndexPost {
                record: post("http://h.example/s/1", "hello world"),
                replaces: None,
            },
            Action::IndexPost {
                record: post("http://h.example/s/2", "hello again"),
                replaces: None,
            },
        ])
        .await
        .unwrap();
    // Tombstones made before the sweep starts (as a CLI would, in another process).
    env.store
        .add_tombstone("http://h.example/s/1", TombstoneKind::Object, 100)
        .unwrap();
    env.handle.reload_tombstones().await.unwrap();
    assert_eq!(env.search_posts("hello").len(), 1, "hidden at once");
    assert!(env.post_pages().iter().any(|p| p.uri.ends_with("/s/1")));

    // A full pass of posts, starting at t=1000, removes every copy.
    pass(&mut env, IndexName::Posts, 1000).await;
    assert!(!env.post_pages().iter().any(|p| p.uri.ends_with("/s/1")));
    assert!(env.post_pages().iter().any(|p| p.uri.ends_with("/s/2")));
    // Accounts haven't completed a pass, so the tombstone stays.
    assert_eq!(env.store.list_tombstones().unwrap().len(), 1);

    // One created during the pass in progress is newer than its start and must survive it.
    env.store
        .add_tombstone("http://h.example/s/2", TombstoneKind::Object, 1500)
        .unwrap();
    env.handle.reload_tombstones().await.unwrap();
    pass(&mut env, IndexName::Accounts, 2000).await;
    let left = env.store.list_tombstones().unwrap();
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0].uri, "http://h.example/s/2");
    assert_eq!(env.handle.stats().await.unwrap().tombstones, 1);
    assert!(env.search_posts("hello").is_empty(), "still hidden");

    // The next full passes cover it: removed from the pages, then retired.
    pass(&mut env, IndexName::Posts, 3000).await;
    assert!(env.post_pages().is_empty());
    pass(&mut env, IndexName::Accounts, 3000).await;
    // The posts pass that just ended began at 1000, before the tombstone: it may not have
    // covered every page when it was made, so the tombstone is kept.
    assert_eq!(env.store.list_tombstones().unwrap().len(), 1);
    // A posts pass that began at 3000 did.
    pass(&mut env, IndexName::Posts, 4000).await;
    assert!(env.store.list_tombstones().unwrap().is_empty());
    assert_eq!(env.handle.stats().await.unwrap().tombstones, 0);
}

#[tokio::test]
async fn the_sweep_resumes_where_it_stopped_and_wraps() {
    let mut env = TestEnv::new().await;
    let cfg = config();
    for expected in 0..3 {
        let report = step(
            IndexName::Posts,
            PAGES,
            &mut env.store,
            &env.handle,
            &cfg,
            500,
        )
        .await
        .unwrap();
        assert_eq!(report.page, expected);
        assert!(!report.wrapped);
    }
    assert_eq!(env.store.sweep_state("posts", 0).unwrap().current_page, 3);
    for _ in 3..PAGES - 1 {
        step(
            IndexName::Posts,
            PAGES,
            &mut env.store,
            &env.handle,
            &cfg,
            500,
        )
        .await
        .unwrap();
    }
    let last = step(
        IndexName::Posts,
        PAGES,
        &mut env.store,
        &env.handle,
        &cfg,
        600,
    )
    .await
    .unwrap();
    assert!(last.wrapped);
    assert_eq!(last.page, PAGES - 1);
    let state = env.store.sweep_state("posts", 0).unwrap();
    assert_eq!(state.current_page, 0);
    assert_eq!(state.last_sweep_started_at, Some(500));
    assert_eq!(state.last_sweep_finished_at, Some(600));
}

#[tokio::test]
async fn stale_records_are_queued_for_revalidation_with_their_record_once() {
    let mut env = TestEnv::new().await;
    let mut old = post("http://h.example/s/1", "alpha beta gamma delta");
    old.checked = 100;
    env.handle
        .apply(vec![Action::IndexPost {
            record: old,
            replaces: None,
        }])
        .await
        .unwrap();
    pass(&mut env, IndexName::Posts, 1_000_000).await;
    // Indexed under many keys, queued once.
    assert_eq!(env.store.queue_len().unwrap(), 1);
    let item = env.store.next_due(i64::MAX, 10).unwrap().remove(0);
    assert_eq!(item.uri, "http://h.example/s/1");
    assert_eq!(item.kind, store::FetchKind::Object);
    assert_eq!(item.reason, store::FetchReason::Revalidate);
    let carried = mwmbl_social::worker::decode_carried(&item.record.unwrap()).unwrap();
    assert!(
        matches!(carried, mwmbl_social::decide::Carried::Post(p) if p.checked == 100 && p.term.is_empty())
    );
}
