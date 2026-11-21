//! Audit D (2026-11-19, consent correctness): failing tests for the gaps found. Each is
//! `#[ignore]`d so the suite stays green; run them with `--ignored` to see them fail.

mod common;

use std::time::Duration;

use apub::consent::ConsentContext;
use common::{PUBLIC, TestEnv};
use mwmbl_social::decide::{Action, Author, Fetched, decide_post};
use mwmbl_social::now_secs;
use mwmbl_social::sweep::{SweepConfig, step};
use mwmbl_social::writer::IndexName;
use serde_json::json;
use store::{FetchKind, FetchReason, NewFetch};

fn announce(uri: &str, kind: FetchKind, not_before: i64) -> NewFetch {
    NewFetch {
        uri: uri.to_owned(),
        kind,
        reason: FetchReason::Announce,
        record: None,
        not_before,
    }
}

async fn sweep_everything(env: &mut TestEnv) {
    let config = SweepConfig {
        step_interval: Duration::from_millis(1),
        revalidate_after_secs: -3600,
        queue_cap: 1000,
    };
    for index in [IndexName::Posts, IndexName::Accounts] {
        for _ in 0..64 {
            step(index, 64, &mut env.store, &env.handle, &config, now_secs())
                .await
                .unwrap();
        }
    }
}

/// D1: a lifecycle `delete` announced while the same URI's `create` is being fetched is
/// deduplicated into the in-flight queue row, and `finish` then deletes that row. The delete is
/// never fetched, and the post stays searchable until its revalidation, days later.
#[tokio::test]
#[ignore = "AUDIT: D1 - fails until fixed"]
async fn a_delete_announced_while_the_create_is_in_flight_is_not_lost() {
    let mut env = TestEnv::new().await;
    let uri = env.fake.url("/notes/public");
    env.store
        .enqueue(&announce(&uri, FetchKind::Object, 0), 0, 100)
        .unwrap();
    let item = env.store.next_due(now_secs(), 10).unwrap().remove(0);

    // The worker fetches the post while it is still public, and indexes it.
    let processed = env.worker.process(&item, now_secs()).await;
    assert!(
        processed
            .actions
            .iter()
            .any(|a| matches!(a, Action::IndexPost { .. }))
    );

    // Before the worker updates the queue row, the author deletes the post and their server
    // announces the delete.
    env.fake.set_gone("/notes/public");
    env.store
        .enqueue(
            &announce(&uri, FetchKind::Object, now_secs()),
            now_secs(),
            100,
        )
        .unwrap();

    env.worker
        .finish(&env.store, &item, &processed, now_secs())
        .unwrap();
    env.worker.drain(&mut env.store).await.unwrap();

    assert!(
        env.search_posts("garlic").is_empty(),
        "the announced delete was dropped: the deleted post is still served"
    );
}

/// D2: the inbox applies a `Delete` straight to the writer. An index decision the worker made
/// from a fetch before the delete, applied after it, clears the object tombstone and indexes
/// the deleted post again.
#[tokio::test]
#[ignore = "AUDIT: D2 - fails until fixed"]
async fn an_index_decision_from_before_an_inbox_delete_does_not_resurrect_the_post() {
    let env = TestEnv::new().await;
    let uri = env.fake.url("/notes/public");
    // The worker's fetch of the post, decided while it was public.
    let processed = env
        .ingest("/notes/public", FetchKind::Object, FetchReason::Announce)
        .await;
    let stale: Vec<Action> = processed
        .actions
        .into_iter()
        .filter(|a| matches!(a, Action::IndexPost { .. }))
        .collect();
    assert_eq!(stale.len(), 1);

    // The author's `Delete` reaches the inbox, which removes and tombstones the post.
    env.handle
        .apply(vec![Action::RemovePost {
            uri: uri.clone(),
            record: None,
            tombstone: true,
        }])
        .await
        .unwrap();
    assert!(env.search_posts("garlic").is_empty());

    // The in-flight decision lands after it.
    env.handle.apply(stale).await.unwrap();
    assert!(
        env.search_posts("garlic").is_empty(),
        "a decision older than the delete re-indexed the deleted post"
    );
}

/// D3: when a post's author document says `discoverable: false`, `decide_post` removes and
/// tombstones whatever account that document's `id` names, without checking that the `id` is
/// the post's `attributedTo` or on the post's origin. A host can make us remove any account
/// and hide all of its posts.
#[test]
fn a_post_cannot_make_us_remove_an_account_on_another_host() {
    let post = "https://evil.example/notes/1";
    let author = "https://evil.example/users/x";
    let victim = "https://victim.example/users/v";
    let note = Fetched::Found(json!({"id": post, "type": "Note", "attributedTo": author,
        "to": [PUBLIC], "content": "<p>hi</p>"}));
    // Served at the author's URI on evil.example, but claiming to be the victim.
    let forged = Fetched::Found(json!({"id": victim, "type": "Person",
        "preferredUsername": "v", "inbox": format!("{victim}/inbox"),
        "discoverable": false, "indexable": false}));
    let actions = decide_post(
        post,
        FetchReason::Announce,
        0,
        None,
        &note,
        Some(Author {
            fetched: &forged,
            context: ConsentContext::NONE,
        }),
        now_secs(),
    );
    let touches_victim = actions.iter().any(|a| match a {
        Action::RemoveAccount { uri, .. } => uri == victim,
        Action::AddAuthorTombstone { actor_uri } | Action::ClearAuthorTombstone { actor_uri } => {
            actor_uri == victim
        }
        _ => false,
    });
    assert!(!touches_victim, "{actions:?}");
}

/// D4: an account indexed only because it follows our actor (no `discoverable` flag of its
/// own) stays in account search after it unfollows, if the re-fetch the unfollow queues fails:
/// the worker gives up and nothing removes the account until its revalidation, days later.
#[tokio::test]
#[ignore = "AUDIT: D4 - fails until fixed"]
async fn an_unfollow_withdraws_a_flagless_account_even_if_its_refetch_fails() {
    let mut env = TestEnv::with_attempts(1).await;
    let carol = env.fake.url("/users/carol");
    env.fake.set_doc(
        "/users/carol",
        json!({"id": carol, "type": "Person", "preferredUsername": "carol",
            "name": "Carol Quinceberry", "inbox": format!("{carol}/inbox")}),
    );
    env.store.add_follow_optin(&carol, now_secs()).unwrap();
    env.ingest("/users/carol", FetchKind::Actor, FetchReason::Announce)
        .await;
    assert_eq!(env.search_accounts("quinceberry").len(), 1);

    // Carol unfollows: what the inbox does for `Undo{Follow}` (activities::undo).
    env.store.remove_follow_optin(&carol).unwrap();
    env.handle
        .apply(vec![Action::AddAuthorTombstone {
            actor_uri: carol.clone(),
        }])
        .await
        .unwrap();
    env.store
        .enqueue(&announce(&carol, FetchKind::Actor, 0), now_secs(), 100)
        .unwrap();
    // Her server is having a bad day.
    env.fake.set_failing("/users/carol");
    env.worker.drain(&mut env.store).await.unwrap();
    assert_eq!(env.store.queue_len().unwrap(), 0, "the worker gave up");

    assert!(
        env.search_accounts("quinceberry").is_empty(),
        "consent was withdrawn but the account is still served"
    );
}

/// D5: transient failures never escalate to removal. A post whose origin keeps failing (a
/// server shut down, or one that now errors for us) is re-queued every sweep pass, given up on,
/// and served forever, though its consent can no longer be confirmed.
#[tokio::test]
#[ignore = "AUDIT: D5 - fails until fixed"]
async fn content_whose_origin_never_answers_again_is_eventually_removed() {
    let mut env = TestEnv::with_attempts(1).await;
    env.ingest("/notes/public", FetchKind::Object, FetchReason::Announce)
        .await;
    assert_eq!(env.search_posts("garlic").len(), 1);
    env.fake.set_failing("/notes/public");
    env.fake.set_failing("/users/alice");
    for _ in 0..5 {
        sweep_everything(&mut env).await;
        env.worker.drain(&mut env.store).await.unwrap();
    }
    assert!(
        env.search_posts("garlic").is_empty(),
        "five failed revalidation passes in a row, and the post is still served"
    );
}
