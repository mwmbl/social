//! `data_sharing` orchestration helpers against the Mastodon-like fake.

mod common;

use std::sync::atomic::Ordering;

use common::Env;
use fasp::ClientError;
use fasp::StoreAccess;
use fasp::data_sharing::{
    BackfillProgress, TrendsThreshold, on_backfill_announcement, on_disable, on_enable,
    resume_backfills,
};
use serde_json::json;
use store::{BackfillState, Category, SubscriptionType};

#[tokio::test]
async fn on_enable_creates_two_subscriptions_and_two_backfills_once() {
    let env = Env::new().await;
    env.enable_data_sharing_in_store();
    let client = env.client();
    let id = env.outcome.registration_id;

    let outcome = on_enable(&client, &*env.ours.state, 77, 250, None)
        .await
        .unwrap();
    assert_eq!(outcome.subscriptions_created, 2);
    assert_eq!(outcome.backfills_created, 2);

    let subs = env
        .ours
        .state
        .with_store(|s| s.list_subscriptions(id))
        .unwrap();
    assert_eq!(subs.len(), 2);
    assert!(
        subs.iter()
            .all(|s| s.subscription_type == SubscriptionType::Lifecycle)
    );
    assert!(subs.iter().all(|s| s.created_at == 77));
    let cats: Vec<_> = subs.iter().map(|s| s.category).collect();
    assert_eq!(cats, vec![Category::Content, Category::Account]);
    let backfills = env.ours.state.with_store(|s| s.list_backfills(id)).unwrap();
    assert_eq!(backfills.len(), 2);
    assert!(
        backfills
            .iter()
            .all(|b| b.state == BackfillState::Active && !b.more_available)
    );
    // Remote ids are the numbers the fake returned, as strings.
    let mut remote: Vec<_> = subs
        .iter()
        .map(|s| s.remote_id.clone())
        .chain(backfills.iter().map(|b| b.remote_id.clone()))
        .collect();
    remote.sort();
    assert_eq!(remote, vec!["11", "12", "13", "14"]);

    assert_eq!(
        env.mastodon
            .inner
            .subscription_bodies
            .lock()
            .unwrap()
            .clone(),
        vec![
            json!({"category": "content", "subscriptionType": "lifecycle", "maxBatchSize": 20}),
            json!({"category": "account", "subscriptionType": "lifecycle", "maxBatchSize": 20}),
        ]
    );
    assert_eq!(
        env.mastodon.inner.backfill_bodies.lock().unwrap().clone(),
        vec![
            json!({"category": "content", "maxCount": 250}),
            json!({"category": "account", "maxCount": 250}),
        ]
    );

    // Calling it again (e.g. a repeated activation) creates nothing.
    let before = env.mastodon.request_count();
    let again = on_enable(&client, &*env.ours.state, 78, 250, None)
        .await
        .unwrap();
    assert_eq!(
        (again.subscriptions_created, again.backfills_created),
        (0, 0)
    );
    assert_eq!(env.mastodon.request_count(), before);
}

#[tokio::test]
async fn on_enable_with_trends_adds_one_content_trends_subscription_with_the_threshold() {
    let env = Env::new().await;
    env.enable_data_sharing_in_store();
    let client = env.client();
    let id = env.outcome.registration_id;
    let threshold = TrendsThreshold {
        timeframe_minutes: 30,
        shares: 4,
        likes: 5,
        replies: 6,
    };
    let outcome = on_enable(&client, &*env.ours.state, 1, 10, Some(threshold))
        .await
        .unwrap();
    assert_eq!(outcome.subscriptions_created, 3);
    let bodies = env
        .mastodon
        .inner
        .subscription_bodies
        .lock()
        .unwrap()
        .clone();
    assert_eq!(
        bodies.last().unwrap(),
        &json!({"category": "content", "subscriptionType": "trends", "maxBatchSize": 20,
            "threshold": {"timeframe": 30, "shares": 4, "likes": 5, "replies": 6}})
    );
    let subs = env
        .ours
        .state
        .with_store(|s| s.list_subscriptions(id))
        .unwrap();
    let trends: Vec<_> = subs
        .iter()
        .filter(|s| s.subscription_type == SubscriptionType::Trends)
        .collect();
    assert_eq!(trends.len(), 1);
    assert_eq!(trends[0].category, Category::Content);

    // An existing deployment (lifecycle only) gets just the trends subscription added.
    let before = env.mastodon.request_count();
    let again = on_enable(&client, &*env.ours.state, 2, 10, Some(threshold))
        .await
        .unwrap();
    assert_eq!(again.subscriptions_created, 0);
    assert_eq!(env.mastodon.request_count(), before);
    assert_eq!(TrendsThreshold::default().timeframe_minutes, 15);
}

#[tokio::test]
async fn on_enable_requires_the_capability() {
    let env = Env::new().await;
    let client = env.client();
    let err = on_enable(&client, &*env.ours.state, 1, 10, None)
        .await
        .unwrap_err();
    assert!(matches!(err, ClientError::NotEnabled(_)), "{err}");
    assert_eq!(env.mastodon.request_count(), 0);
}

#[tokio::test]
async fn on_disable_makes_no_calls_and_re_enable_deletes_the_orphans() {
    let env = Env::new().await;
    env.enable_data_sharing_in_store();
    let client = env.client();
    let id = env.outcome.registration_id;
    on_enable(&client, &*env.ours.state, 1, 10, None)
        .await
        .unwrap();
    let calls = env.mastodon.request_count();
    assert_eq!(calls, 4);

    // The server has disabled the capability on its side (activation DELETE).
    assert_eq!(env.deactivate("data_sharing", "0").await.status, 204);
    let outcome = on_disable(&*env.ours.state, id).unwrap();
    assert_eq!(
        (outcome.subscriptions_deleted, outcome.backfills_deleted),
        (2, 2)
    );
    assert_eq!(
        env.mastodon.request_count(),
        calls,
        "no calls to a disabled capability"
    );
    assert!(
        env.ours
            .state
            .with_store(|s| s.list_subscriptions(id))
            .unwrap()
            .is_empty()
    );
    assert!(
        env.ours
            .state
            .with_store(|s| s.list_backfills(id))
            .unwrap()
            .is_empty()
    );

    // Re-enabling starts from scratch.
    assert_eq!(env.activate("data_sharing", "0").await.status, 204);
    // Mastodon keeps the old subscriptions when the capability is disabled, so re-enabling
    // first deletes them on the server (now that calls are allowed), then subscribes afresh.
    let before = env.mastodon.request_count();
    let outcome = on_enable(&client, &*env.ours.state, 2, 10, None)
        .await
        .unwrap();
    assert_eq!(
        (
            outcome.orphans_deleted,
            outcome.subscriptions_created,
            outcome.backfills_created
        ),
        (2, 2, 2)
    );
    let deletes = env.mastodon.requests()[before..]
        .iter()
        .filter(|(method, path)| {
            method == "DELETE" && path.starts_with("/api/fasp/data_sharing/v0/event_subscriptions/")
        })
        .count();
    assert_eq!(deletes, 2);
    assert!(
        env.ours
            .state
            .with_store(|s| s.list_orphaned_subscriptions(id))
            .unwrap()
            .is_empty()
    );
}

async fn enabled_env() -> (Env, fasp::client::FaspClient<common::TestState>) {
    let env = Env::new().await;
    env.enable_data_sharing_in_store();
    let client = env.client();
    on_enable(&client, &*env.ours.state, 1, 10, None)
        .await
        .unwrap();
    (env, client)
}

fn backfill(env: &Env, category: Category) -> store::Backfill {
    let id = env.outcome.registration_id;
    env.ours
        .state
        .with_store(|s| s.list_backfills(id))
        .unwrap()
        .into_iter()
        .find(|b| b.category == category)
        .unwrap()
}

#[tokio::test]
async fn more_available_continues_the_backfill() {
    let (env, client) = enabled_env().await;
    let store = &*env.ours.state;
    let bf = backfill(&env, Category::Content);
    env.mastodon
        .inner
        .continuations_left
        .store(1, Ordering::SeqCst);
    let before = env.mastodon.request_count();

    let progress = on_backfill_announcement(&client, store, &bf.remote_id, true, true)
        .await
        .unwrap();
    assert_eq!(progress, BackfillProgress::Continued);
    assert_eq!(env.mastodon.request_count(), before + 1);
    let after = backfill(&env, Category::Content);
    assert_eq!(after.state, BackfillState::Active);
    assert!(
        !after.more_available,
        "continuation requested, nothing outstanding"
    );
}

#[tokio::test]
async fn continuation_404_marks_the_backfill_done() {
    let (env, client) = enabled_env().await;
    let bf = backfill(&env, Category::Account);
    // The fake has no continuations left: it answers 404.
    let progress = on_backfill_announcement(&client, &*env.ours.state, &bf.remote_id, true, true)
        .await
        .unwrap();
    assert_eq!(progress, BackfillProgress::Done);
    assert_eq!(backfill(&env, Category::Account).state, BackfillState::Done);
}

#[tokio::test]
async fn no_more_available_marks_done_without_calling() {
    let (env, client) = enabled_env().await;
    let bf = backfill(&env, Category::Content);
    let before = env.mastodon.request_count();
    let progress = on_backfill_announcement(&client, &*env.ours.state, &bf.remote_id, false, true)
        .await
        .unwrap();
    assert_eq!(progress, BackfillProgress::Done);
    assert_eq!(env.mastodon.request_count(), before);
    let after = backfill(&env, Category::Content);
    assert_eq!(after.state, BackfillState::Done);
    assert!(!after.more_available);
}

#[tokio::test]
async fn full_queue_defers_and_resume_continues_later() {
    let (env, client) = enabled_env().await;
    let store = &*env.ours.state;
    let bf = backfill(&env, Category::Content);
    let before = env.mastodon.request_count();

    let progress = on_backfill_announcement(&client, store, &bf.remote_id, true, false)
        .await
        .unwrap();
    assert_eq!(progress, BackfillProgress::Deferred);
    assert_eq!(env.mastodon.request_count(), before);
    let deferred = backfill(&env, Category::Content);
    assert!(deferred.more_available && deferred.state == BackfillState::Active);

    // Still no room: nothing happens.
    assert_eq!(resume_backfills(&client, store, || false).await.unwrap(), 0);
    assert_eq!(env.mastodon.request_count(), before);

    env.mastodon
        .inner
        .continuations_left
        .store(1, Ordering::SeqCst);
    assert_eq!(resume_backfills(&client, store, || true).await.unwrap(), 1);
    assert!(!backfill(&env, Category::Content).more_available);
    // Nothing is left to resume.
    assert_eq!(resume_backfills(&client, store, || true).await.unwrap(), 0);
}

#[tokio::test]
async fn unknown_backfill_ids_are_reported() {
    let (env, client) = enabled_env().await;
    let before = env.mastodon.request_count();
    let progress = on_backfill_announcement(&client, &*env.ours.state, "nope", true, true)
        .await
        .unwrap();
    assert_eq!(progress, BackfillProgress::Unknown);
    assert_eq!(env.mastodon.request_count(), before);
}
