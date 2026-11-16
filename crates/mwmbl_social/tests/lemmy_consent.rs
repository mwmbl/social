//! The Lemmy and follow opt-in consent rules in the pipeline (PLAN.md Stage 3): an author
//! without consent flags is indexed when their own host's nodeinfo says Lemmy, or when they
//! follow our actor, and an explicit `indexable: false` still wins. Against the in-process
//! fake host; no network beyond 127.0.0.1.

mod common;

use std::time::Duration;

use common::{PUBLIC, TestEnv};
use mwmbl_social::now_secs;
use mwmbl_social::sweep::{SweepConfig, step};
use mwmbl_social::writer::IndexName;
use serde_json::{Value, json};
use store::{FetchKind, FetchReason, TombstoneKind};

const NODEINFO: &str = "/.well-known/nodeinfo";

/// Serves nodeinfo naming `software`, a Person `carol` with neither `discoverable` nor
/// `indexable` (as Lemmy and GoToSocial serve), and a public `Page` by her.
fn serve_flagless_author(env: &TestEnv, software: &str) {
    let origin = env.fake.origin.clone();
    env.fake.set_doc(
        NODEINFO,
        json!({"links": [{
            "rel": "http://nodeinfo.diaspora.software/ns/schema/2.1",
            "href": format!("{origin}/nodeinfo/2.1"),
        }]}),
    );
    env.fake.set_doc(
        "/nodeinfo/2.1",
        json!({"version": "2.1", "software": {"name": software, "version": "1.0"}}),
    );
    let carol = format!("{origin}/u/carol");
    env.fake.set_doc(
        "/u/carol",
        json!({
            "@context": ["https://join-lemmy.org/context.json", "https://www.w3.org/ns/activitystreams"],
            "id": carol,
            "type": "Person",
            "preferredUsername": "carol",
            "name": "Carol",
            "summary": "<p>Rust and gardening.</p>",
            "inbox": format!("{carol}/inbox"),
            "outbox": format!("{carol}/outbox"),
        }),
    );
    env.fake.set_doc(
        "/post/42",
        json!({
            "@context": ["https://join-lemmy.org/context.json", "https://www.w3.org/ns/activitystreams"],
            "id": format!("{origin}/post/42"),
            "type": "Page",
            "attributedTo": carol,
            "to": [format!("{origin}/c/rust"), PUBLIC],
            "cc": [],
            "name": "Borrow checker tips for beginners",
            "content": "<p>Some things that helped me with the ferrous wombat.</p>",
            "language": {"identifier": "en", "name": "English"},
            "published": "2026-11-10T12:00:00Z",
        }),
    );
}

fn set_carol(env: &TestEnv, field: &str, value: Value) {
    let mut carol = env.fake.doc("/u/carol").unwrap();
    carol[field] = value;
    env.fake.set_doc("/u/carol", carol);
}

async fn ingest_post(env: &TestEnv) {
    let processed = env
        .ingest("/post/42", FetchKind::Object, FetchReason::Announce)
        .await;
    assert!(processed.retry.is_none(), "{processed:?}");
}

/// Queue every stored record for revalidation and drain the queue.
async fn revalidate_everything(env: &mut TestEnv) {
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
    env.worker.drain(&mut env.store).await.unwrap();
}

#[tokio::test]
async fn a_flagless_author_on_a_lemmy_host_is_indexed() {
    let env = TestEnv::new().await;
    serve_flagless_author(&env, "Lemmy");
    ingest_post(&env).await;

    let found = env.search_posts("wombat");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].uri, env.fake.url("/post/42"));
    assert_eq!(
        found[0].title.as_deref(),
        Some("Borrow checker tips for beginners")
    );
    assert_eq!(env.search_accounts("carol").len(), 1);
    // No author tombstone hides her posts.
    assert!(env.store.list_tombstones().unwrap().is_empty());

    // The software is cached in `hosts`, and a second fetch doesn't look it up again.
    let host = env.fake.origin.trim_start_matches("http://");
    let software = env.store.host_software(host).unwrap().unwrap();
    assert_eq!(software.software.as_deref(), Some("lemmy"));
    ingest_post(&env).await;
    assert_eq!(env.fake.hits(NODEINFO), 1);
}

#[tokio::test]
async fn a_flagless_author_elsewhere_needs_a_follow_and_an_explicit_false_still_wins() {
    let mut env = TestEnv::new().await;
    serve_flagless_author(&env, "gotosocial");
    ingest_post(&env).await;
    assert!(env.search_posts("wombat").is_empty());
    assert!(env.post_pages().is_empty());
    assert!(env.search_accounts("carol").is_empty());

    // Carol follows our actor: that stands in for the flags she doesn't carry.
    let carol = env.fake.url("/u/carol");
    env.store.add_follow_optin(&carol, now_secs()).unwrap();
    ingest_post(&env).await;
    assert_eq!(env.search_posts("wombat").len(), 1);
    assert_eq!(env.search_accounts("carol").len(), 1);
    assert!(
        !env.store
            .list_tombstones()
            .unwrap()
            .iter()
            .any(|t| t.kind == TombstoneKind::Author),
        "no author tombstone for a follower"
    );
    // The non-Lemmy answer is cached too.
    assert_eq!(env.fake.hits(NODEINFO), 1);

    // She sets `indexable: false`. Following us doesn't override an explicit no.
    set_carol(&env, "indexable", false.into());
    revalidate_everything(&mut env).await;
    assert!(env.search_posts("wombat").is_empty());
    assert!(
        env.store
            .list_tombstones()
            .unwrap()
            .iter()
            .any(|t| t.kind == TombstoneKind::Author && t.uri == carol)
    );
    // The post's own revalidation ran beside the account's, against the worker's cached copy
    // of her actor document, so its copy may still be on a page, hidden by the tombstone. The
    // next pass removes it.
    revalidate_everything(&mut env).await;
    assert!(env.post_pages().is_empty(), "{:?}", env.post_pages());
}

#[tokio::test]
async fn an_unfollow_takes_effect_on_revalidation() {
    let mut env = TestEnv::new().await;
    serve_flagless_author(&env, "gotosocial");
    let carol = env.fake.url("/u/carol");
    env.store.add_follow_optin(&carol, now_secs()).unwrap();
    ingest_post(&env).await;
    assert_eq!(env.search_posts("wombat").len(), 1);

    env.store.remove_follow_optin(&carol).unwrap();
    revalidate_everything(&mut env).await;
    assert!(env.search_posts("wombat").is_empty());
    assert!(env.post_pages().is_empty());
    assert!(env.search_accounts("carol").is_empty());
}

#[tokio::test]
async fn a_failed_software_lookup_counts_as_not_lemmy() {
    let env = TestEnv::new().await;
    serve_flagless_author(&env, "lemmy");
    env.fake.set_gone(NODEINFO);
    ingest_post(&env).await;
    assert!(env.search_posts("wombat").is_empty());
    let host = env.fake.origin.trim_start_matches("http://");
    let software = env.store.host_software(host).unwrap().unwrap();
    assert_eq!(software.software, None, "the failure is recorded");
    // And not retried within the hour.
    ingest_post(&env).await;
    assert_eq!(env.fake.hits(NODEINFO), 1);
}

#[tokio::test]
async fn authors_with_both_flags_cost_no_software_lookup() {
    let env = TestEnv::new().await;
    env.ingest("/notes/public", FetchKind::Object, FetchReason::Announce)
        .await;
    env.ingest("/notes/bobs", FetchKind::Object, FetchReason::Announce)
        .await;
    assert_eq!(env.search_posts("garlic").len(), 1);
    assert_eq!(env.fake.hits(NODEINFO), 0);
}
