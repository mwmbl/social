//! `FaspClient` against the Mastodon-like fake.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::Env;
use fasp::client::FaspClient;
use fasp::{ClientError, StoreAccess};
use serde_json::json;
use store::{Category, SubscriptionType};

#[tokio::test]
async fn refuses_without_sending_when_capability_is_not_enabled() {
    let env = Env::new().await;
    let client = env.client();
    // Never recorded.
    let err = client.data_sharing().unwrap_err();
    assert!(matches!(err, ClientError::NotEnabled(_)), "{err}");
    assert!(err.to_string().contains("data_sharing"));
    assert!(err.to_string().contains("127.0.0.1"));
    // Explicitly disabled.
    let id = env.outcome.registration_id;
    env.ours
        .state
        .with_store(|s| s.set_capability(id, "data_sharing", "0.1", false, 1))
        .unwrap();
    assert!(matches!(
        client.data_sharing().unwrap_err(),
        ClientError::NotEnabled(_)
    ));
    // Enabled only under another version string: not enabled.
    env.ours
        .state
        .with_store(|s| s.set_capability(id, "data_sharing", "0", true, 1))
        .unwrap();
    assert!(client.data_sharing().is_err());
    assert_eq!(env.mastodon.request_count(), 0);
}

#[tokio::test]
async fn every_call_rechecks_the_capability() {
    let env = Env::new().await;
    env.enable_data_sharing_in_store();
    let client = env.client();
    let ds = client.data_sharing().unwrap();
    // Disabled after the gate was passed: nothing is sent.
    let id = env.outcome.registration_id;
    env.ours
        .state
        .with_store(|s| s.set_capability(id, "data_sharing", "0.1", false, 2))
        .unwrap();
    assert!(matches!(
        ds.subscribe(Category::Content, SubscriptionType::Lifecycle, Some(20))
            .await,
        Err(ClientError::NotEnabled(_))
    ));
    assert!(matches!(
        ds.request_backfill(Category::Content, 10).await,
        Err(ClientError::NotEnabled(_))
    ));
    assert!(matches!(
        ds.continue_backfill("1").await,
        Err(ClientError::NotEnabled(_))
    ));
    assert!(matches!(
        ds.unsubscribe("1").await,
        Err(ClientError::NotEnabled(_))
    ));
    assert_eq!(env.mastodon.request_count(), 0);
}

#[tokio::test]
async fn data_sharing_calls_are_signed_and_ids_become_strings() {
    let env = Env::new().await;
    env.enable_data_sharing_in_store();
    let client = env.client();
    let ds = client.data_sharing().unwrap();

    let sub = ds
        .subscribe(Category::Account, SubscriptionType::Lifecycle, Some(25))
        .await
        .unwrap();
    assert_eq!(sub, "11", "numeric id from the server, as a string");
    let backfill = ds.request_backfill(Category::Content, 500).await.unwrap();
    assert_eq!(backfill, "12");
    ds.unsubscribe(&sub).await.unwrap();

    env.mastodon
        .inner
        .continuations_left
        .store(1, Ordering::SeqCst);
    assert!(ds.continue_backfill(&backfill).await.unwrap());
    assert!(
        !ds.continue_backfill(&backfill).await.unwrap(),
        "404 means no more"
    );

    // The fake verified every request (digest, signature, keyid = faspId, covered components).
    assert_eq!(env.mastodon.failures(), Vec::<String>::new());
    assert_eq!(
        env.mastodon.requests(),
        vec![
            (
                "POST".to_owned(),
                "/api/fasp/data_sharing/v0/event_subscriptions".to_owned()
            ),
            (
                "POST".to_owned(),
                "/api/fasp/data_sharing/v0/backfill_requests".to_owned()
            ),
            (
                "DELETE".to_owned(),
                "/api/fasp/data_sharing/v0/event_subscriptions/11".to_owned()
            ),
            (
                "POST".to_owned(),
                "/api/fasp/data_sharing/v0/backfill_requests/12/continuation".to_owned()
            ),
            (
                "POST".to_owned(),
                "/api/fasp/data_sharing/v0/backfill_requests/12/continuation".to_owned()
            ),
        ]
    );
    assert_eq!(
        env.mastodon
            .inner
            .subscription_bodies
            .lock()
            .unwrap()
            .clone(),
        vec![json!({"category": "account", "subscriptionType": "lifecycle", "maxBatchSize": 25})]
    );
    assert_eq!(
        env.mastodon.inner.backfill_bodies.lock().unwrap().clone(),
        vec![json!({"category": "content", "maxCount": 500})]
    );
}

#[tokio::test]
async fn response_verification_failures_are_errors() {
    let env = Env::new().await;
    env.enable_data_sharing_in_store();
    let client = env.client();
    let ds = client.data_sharing().unwrap();

    env.mastodon
        .inner
        .tamper_signature
        .store(true, Ordering::SeqCst);
    let err = ds
        .subscribe(Category::Content, SubscriptionType::Lifecycle, Some(20))
        .await
        .unwrap_err();
    assert!(
        matches!(err, ClientError::ResponseVerification { status: 201, .. }),
        "{err}"
    );
    assert!(err.to_string().contains("127.0.0.1"));

    env.mastodon
        .inner
        .tamper_signature
        .store(false, Ordering::SeqCst);
    env.mastodon.inner.omit_digest.store(true, Ordering::SeqCst);
    let err = ds.continue_backfill("1").await.unwrap_err();
    assert!(
        matches!(err, ClientError::ResponseVerification { .. }),
        "{err}"
    );

    env.mastodon
        .inner
        .omit_digest
        .store(false, Ordering::SeqCst);
    assert!(ds.request_backfill(Category::Content, 1).await.is_ok());
}

#[tokio::test]
async fn rate_limits_carry_the_retry_after_delay() {
    let env = Env::new().await;
    env.enable_data_sharing_in_store();
    let client = env.client();
    env.mastodon.inner.rate_limit.store(true, Ordering::SeqCst);
    let err = client
        .data_sharing()
        .unwrap()
        .request_backfill(Category::Content, 1)
        .await
        .unwrap_err();
    match err {
        ClientError::RateLimited {
            retry_after, host, ..
        } => {
            assert_eq!(retry_after, Duration::from_secs(7));
            assert!(host.contains("127.0.0.1"));
        }
        other => panic!("expected RateLimited, got {other}"),
    }
}

#[tokio::test]
async fn requests_older_than_five_minutes_are_rejected_by_the_server() {
    let env = Env::new().await;
    env.enable_data_sharing_in_store();
    let client = FaspClient::new(
        env.ours.http.clone(),
        env.ours.state.clone(),
        &env.registration(),
    )
    .unwrap()
    .with_clock(|| common::now() - 600);
    let err = client
        .data_sharing()
        .unwrap()
        .request_backfill(Category::Content, 1)
        .await
        .unwrap_err();
    // The fake answers 401; our (skewed) clock makes the response look fine or not, either
    // way the call fails and names the host.
    assert!(err.to_string().contains("127.0.0.1"), "{err}");
    assert_eq!(env.mastodon.failures().len(), 1);
}

#[tokio::test]
async fn unusable_registrations_cannot_make_a_client() {
    let env = Env::new().await;
    let mut registration = env.registration();
    registration.fasp_id = None;
    let err =
        FaspClient::new(env.ours.http.clone(), env.ours.state.clone(), &registration).unwrap_err();
    assert!(matches!(err, ClientError::Setup { .. }), "{err}");
    let mut registration = env.registration();
    registration.their_public_key = None;
    assert!(FaspClient::new(env.ours.http.clone(), env.ours.state.clone(), &registration).is_err());
}
