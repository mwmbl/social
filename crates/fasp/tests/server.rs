//! Our axum router, exercised the way Mastodon would: signed requests with keyid = serverId.

mod common;

use apub::signatures::keys::Ed25519SigningKey;
use common::{Env, Opts, privacy};
use fasp::announcement::{AnnouncementSource, EventType};
use fasp::{Capability, ProviderInfoConfig, StoreAccess};
use serde_json::json;
use store::{Category, RegistrationStatus, SubscriptionType};

const EMPTY_DIGEST: &str = "sha-256=:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=:";

#[tokio::test]
async fn pending_becomes_active_on_first_verified_request() {
    let env = Env::new().await;
    assert_eq!(env.status(), RegistrationStatus::Pending);
    let reply = env
        .mastodon_request("GET", "/provider_info", b"", &Opts::default())
        .await;
    assert_eq!(reply.status, 200);
    assert_eq!(env.status(), RegistrationStatus::Active);
    // Still active, and still served, on later requests.
    let reply = env
        .mastodon_request("GET", "/provider_info", b"", &Opts::default())
        .await;
    assert_eq!(reply.status, 200);
    assert_eq!(env.status(), RegistrationStatus::Active);
}

#[tokio::test]
async fn responses_are_signed_mastodon_style_including_empty_bodies() {
    let env = Env::new().await;
    // 200 with a body.
    let info = env
        .mastodon_request("GET", "/provider_info", b"", &Opts::default())
        .await;
    assert_eq!(info.status, 200);
    assert_eq!(info.header("content-type"), Some("application/json"));
    env.assert_signed(&info);
    // 204 with an empty body: digest of the empty string.
    let activated = env.activate("data_sharing", "0").await;
    assert_eq!(activated.status, 204);
    assert_eq!(activated.header("content-digest"), Some(EMPTY_DIGEST));
    env.assert_signed(&activated);
    // Errors that are still attributable are signed too: 404 and 422.
    let missing = env
        .mastodon_request("GET", "/nope", b"", &Opts::default())
        .await;
    assert_eq!(missing.status, 404);
    env.assert_signed(&missing);
    let wrong_method = env
        .mastodon_request("PUT", "/provider_info", b"", &Opts::default())
        .await;
    assert_eq!(wrong_method.status, 405);
    env.assert_signed(&wrong_method);
}

#[tokio::test]
async fn response_signature_does_not_verify_under_another_key() {
    let env = Env::new().await;
    let reply = env
        .mastodon_request("GET", "/provider_info", b"", &Opts::default())
        .await;
    let other = Ed25519SigningKey::generate().unwrap().verifying_key();
    let server_id = env.registration().server_id;
    let result = std::panic::catch_unwind(|| reply.verify_mastodon_style(&other, &server_id));
    assert!(result.is_err());
}

#[tokio::test]
async fn bad_signature_is_401_and_does_not_activate() {
    let env = Env::new().await;
    let opts = Opts {
        key: Some(Ed25519SigningKey::generate().unwrap()),
        ..Opts::default()
    };
    let reply = env
        .mastodon_request("GET", "/provider_info", b"", &opts)
        .await;
    assert_eq!(reply.status, 401);
    assert_eq!(env.status(), RegistrationStatus::Pending);
    // Unauthenticated responses are unsigned but carry a correct content-digest.
    assert!(reply.header("signature").is_none());
    assert_eq!(
        reply.header("content-digest"),
        Some(apub::signatures::content_digest(&reply.body).as_str())
    );
}

#[tokio::test]
async fn missing_or_unknown_signatures_are_401() {
    let env = Env::new().await;
    let missing = Opts {
        omit_signature: true,
        ..Opts::default()
    };
    let reply = env
        .mastodon_request("GET", "/provider_info", b"", &missing)
        .await;
    assert_eq!(reply.status, 401);
    let unknown = Opts {
        keyid: Some("not-a-server-id".to_owned()),
        ..Opts::default()
    };
    let reply = env
        .mastodon_request("GET", "/provider_info", b"", &unknown)
        .await;
    assert_eq!(reply.status, 401);
}

#[tokio::test]
async fn stale_and_future_created_are_401() {
    let env = Env::new().await;
    for offset in [-600, 600] {
        let opts = Opts {
            created_offset: offset,
            ..Opts::default()
        };
        let reply = env
            .mastodon_request("GET", "/provider_info", b"", &opts)
            .await;
        assert_eq!(reply.status, 401, "created offset {offset}");
    }
    assert_eq!(env.status(), RegistrationStatus::Pending);
    // Within the window is fine.
    let opts = Opts {
        created_offset: -200,
        ..Opts::default()
    };
    let reply = env
        .mastodon_request("GET", "/provider_info", b"", &opts)
        .await;
    assert_eq!(reply.status, 200);
}

#[tokio::test]
async fn wrong_digest_is_401() {
    let env = Env::new().await;
    // The header does not match the body (and the signature covers the header).
    let opts = Opts {
        digest_header: Some(apub::signatures::content_digest(b"something else")),
        ..Opts::default()
    };
    let reply = env
        .mastodon_request(
            "POST",
            "/capabilities/data_sharing/0/activation",
            b"",
            &opts,
        )
        .await;
    assert_eq!(reply.status, 401);
    // The header matches the body but not what was signed.
    let opts = Opts {
        signed_body: Some(b"{}".to_vec()),
        ..Opts::default()
    };
    let reply = env
        .mastodon_request(
            "POST",
            "/capabilities/data_sharing/0/activation",
            b"",
            &opts,
        )
        .await;
    assert_eq!(reply.status, 401);
    assert_eq!(env.status(), RegistrationStatus::Pending);
}

#[tokio::test]
async fn signature_for_another_target_uri_is_401() {
    let env = Env::new().await;
    // We never trust the Host header or the client's idea of our URL: the signed
    // @target-uri must be our configured base URL plus the path.
    let opts = Opts {
        target_base: Some("https://evil.example".to_owned()),
        ..Opts::default()
    };
    let reply = env
        .mastodon_request("GET", "/provider_info", b"", &opts)
        .await;
    assert_eq!(reply.status, 401);
    // The query is part of the target: signed together with the request it is accepted.
    let reply = env
        .mastodon_request("GET", "/provider_info?x=1", b"", &Opts::default())
        .await;
    assert_eq!(reply.status, 200);
}

#[tokio::test]
async fn revoked_registration_is_401() {
    let env = Env::new().await;
    let id = env.outcome.registration_id;
    env.ours
        .state
        .with_store(|s| s.set_registration_status(id, RegistrationStatus::Revoked))
        .unwrap();
    let reply = env
        .mastodon_request("GET", "/provider_info", b"", &Opts::default())
        .await;
    assert_eq!(reply.status, 401);
    assert_eq!(env.status(), RegistrationStatus::Revoked);
}

#[tokio::test]
async fn oversized_body_is_413() {
    let env = Env::new().await;
    let body = vec![b'x'; 1024 * 1024 + 1];
    let reply = env
        .mastodon_request(
            "POST",
            "/data_sharing/v0/announcements",
            &body,
            &Opts::default(),
        )
        .await;
    assert_eq!(reply.status, 413);
}

#[tokio::test]
async fn provider_info_defaults() {
    let env = Env::new().await;
    let reply = env
        .mastodon_request("GET", "/provider_info", b"", &Opts::default())
        .await;
    assert_eq!(
        reply.json(),
        json!({
            "name": "Mwmbl Social",
            "privacyPolicy": [],
            "capabilities": [{"id": "data_sharing", "version": "0.1"}],
        })
    );
}

#[tokio::test]
async fn provider_info_with_configuration_and_extra_capabilities() {
    let info = ProviderInfoConfig {
        name: "Hosted".to_owned(),
        privacy_policy: vec![
            privacy("https://example.com/privacy", "en"),
            privacy("https://example.com/datenschutz", "de"),
        ],
        contact_email: Some("admin@example.com".to_owned()),
        fediverse_account: Some("@admin@example.com".to_owned()),
    };
    let env = Env::with(info, vec![Capability::new("account_search", "0.1")]).await;
    let reply = env
        .mastodon_request("GET", "/provider_info", b"", &Opts::default())
        .await;
    assert_eq!(
        reply.json(),
        json!({
            "name": "Hosted",
            "privacyPolicy": [
                {"url": "https://example.com/privacy", "language": "en"},
                {"url": "https://example.com/datenschutz", "language": "de"},
            ],
            "capabilities": [
                {"id": "data_sharing", "version": "0.1"},
                {"id": "account_search", "version": "0.1"},
            ],
            "contactEmail": "admin@example.com",
            "fediverseAccount": "@admin@example.com",
        })
    );
    // The extra capability can be activated too, under its major version.
    assert_eq!(env.activate("account_search", "0").await.status, 204);
    let id = env.outcome.registration_id;
    assert!(
        env.ours
            .state
            .with_store(|s| s.capability_enabled(id, "account_search", "0.1"))
            .unwrap()
    );
}

#[tokio::test]
async fn activation_accepts_major_and_full_versions() {
    let env = Env::new().await;
    let id = env.outcome.registration_id;
    let enabled = |env: &Env| {
        env.ours
            .state
            .with_store(|s| s.capability_enabled(id, "data_sharing", "0.1"))
            .unwrap()
    };
    for version in ["0", "0.1"] {
        assert_eq!(env.activate("data_sharing", version).await.status, 204);
        assert!(enabled(&env), "enabled via {version}");
        assert_eq!(env.deactivate("data_sharing", version).await.status, 204);
        assert!(!enabled(&env), "disabled via {version}");
    }
    let events = env.ours.state.capability_events.lock().unwrap().clone();
    let expected: Vec<_> = [true, false, true, false]
        .into_iter()
        .map(|on| (id, "data_sharing".to_owned(), on))
        .collect();
    assert_eq!(events, expected);
    // Nothing is stored under the raw path version.
    let all = env
        .ours
        .state
        .with_store(|s| s.enabled_capabilities(id))
        .unwrap();
    assert!(all.is_empty());
}

#[tokio::test]
async fn unknown_capabilities_and_versions_are_404_and_signed() {
    let env = Env::new().await;
    for (id, version) in [
        ("trends", "0"),
        ("data_sharing", "1"),
        ("data_sharing", "0.2"),
    ] {
        let reply = env.activate(id, version).await;
        assert_eq!(reply.status, 404, "{id} {version}");
        env.assert_signed(&reply);
        let reply = env.deactivate(id, version).await;
        assert_eq!(reply.status, 404, "{id} {version}");
    }
    let rid = env.outcome.registration_id;
    assert!(
        env.ours
            .state
            .with_store(|s| s.enabled_capabilities(rid))
            .unwrap()
            .is_empty()
    );
    assert!(env.ours.state.capability_events.lock().unwrap().is_empty());
}

fn announcement_body(source: &serde_json::Value, extra: serde_json::Value) -> Vec<u8> {
    let mut body = json!({"source": source, "category": "content"});
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    serde_json::to_vec(&body).unwrap()
}

async fn announce(env: &Env, body: &[u8]) -> common::Reply {
    let reply = env
        .mastodon_request(
            "POST",
            "/data_sharing/v0/announcements",
            body,
            &Opts::default(),
        )
        .await;
    env.assert_signed(&reply);
    reply
}

fn set_up_sources(env: &Env) {
    let id = env.outcome.registration_id;
    env.ours
        .state
        .with_store(|s| {
            s.add_subscription(id, "12", Category::Content, SubscriptionType::Lifecycle, 1)?;
            s.add_backfill(id, "5", Category::Content)
        })
        .unwrap();
}

fn received(env: &Env) -> Vec<(i64, fasp::announcement::Announcement)> {
    env.ours.state.announcements.lock().unwrap().clone()
}

#[tokio::test]
async fn announcements_are_handed_over_when_enabled_and_known() {
    let env = Env::new().await;
    set_up_sources(&env);
    env.enable_data_sharing_in_store();
    let sub = json!({"subscription": {"id": "12"}});
    let reply = announce(
        &env,
        &announcement_body(
            &sub,
            json!({"eventType": "new", "objectUris": ["https://a.example/1", "ftp://x/2"]}),
        ),
    )
    .await;
    assert_eq!(reply.status, 204);
    let backfill = json!({"backfillRequest": {"id": "5"}});
    let reply = announce(
        &env,
        &announcement_body(
            &backfill,
            json!({"moreObjectsAvailable": true, "objectUris": ["https://a.example/2"]}),
        ),
    )
    .await;
    assert_eq!(reply.status, 204);

    let got = received(&env);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].0, env.outcome.registration_id);
    assert_eq!(
        got[0].1.source,
        AnnouncementSource::Subscription {
            remote_id: "12".into(),
            event: EventType::New
        }
    );
    assert_eq!(got[0].1.object_uris, vec!["https://a.example/1".to_owned()]);
    assert_eq!(
        got[1].1.source,
        AnnouncementSource::Backfill {
            remote_id: "5".into(),
            more_available: true
        }
    );
}

#[tokio::test]
async fn announcements_are_dropped_with_204_when_not_ours_or_not_enabled() {
    let env = Env::new().await;
    set_up_sources(&env);
    let sub = |id: &str| json!({"subscription": {"id": id}});
    let uris = json!({"eventType": "new", "objectUris": ["https://a.example/1"]});

    // data_sharing not enabled.
    let reply = announce(&env, &announcement_body(&sub("12"), uris.clone())).await;
    assert_eq!(reply.status, 204);
    assert!(received(&env).is_empty());

    env.enable_data_sharing_in_store();
    // Unknown source id.
    let reply = announce(&env, &announcement_body(&sub("999"), uris.clone())).await;
    assert_eq!(reply.status, 204);
    // A backfill id used as a subscription id, and the other way round.
    let reply = announce(&env, &announcement_body(&sub("5"), uris.clone())).await;
    assert_eq!(reply.status, 204);
    let reply = announce(
        &env,
        &announcement_body(
            &json!({"backfillRequest": {"id": "12"}}),
            json!({"moreObjectsAvailable": false, "objectUris": ["https://a.example/1"]}),
        ),
    )
    .await;
    assert_eq!(reply.status, 204);
    // Trending events.
    let reply = announce(
        &env,
        &announcement_body(
            &sub("12"),
            json!({"eventType": "trending", "objectUris": ["https://a.example/1"]}),
        ),
    )
    .await;
    assert_eq!(reply.status, 204);
    assert!(received(&env).is_empty());
}

#[tokio::test]
async fn trending_events_are_handed_over_only_from_a_trends_subscription() {
    let env = Env::new().await;
    set_up_sources(&env);
    let id = env.outcome.registration_id;
    env.ours
        .state
        .with_store(|s| {
            s.add_subscription(id, "13", Category::Content, SubscriptionType::Trends, 1)
        })
        .unwrap();
    env.enable_data_sharing_in_store();
    let sub = |id: &str| json!({"subscription": {"id": id}});
    let trending = json!({"eventType": "trending", "objectUris": ["https://a.example/1"]});
    let new = json!({"eventType": "new", "objectUris": ["https://a.example/2"]});
    // Wrong pairings are dropped: trending on the lifecycle subscription, new on the trends one.
    assert_eq!(
        announce(&env, &announcement_body(&sub("12"), trending.clone()))
            .await
            .status,
        204
    );
    assert_eq!(
        announce(&env, &announcement_body(&sub("13"), new))
            .await
            .status,
        204
    );
    assert!(received(&env).is_empty());
    assert_eq!(
        announce(&env, &announcement_body(&sub("13"), trending))
            .await
            .status,
        204
    );
    let got = received(&env);
    assert_eq!(got.len(), 1);
    assert_eq!(
        got[0].1.source,
        AnnouncementSource::Subscription {
            remote_id: "13".into(),
            event: EventType::Trending
        }
    );
    assert_eq!(got[0].1.object_uris, ["https://a.example/1"]);
}

#[tokio::test]
async fn another_registrations_source_ids_do_not_count() {
    let env = Env::new().await;
    env.enable_data_sharing_in_store();
    // The other registration owns subscription "12"; ours has none.
    env.ours
        .state
        .with_store(|s| {
            let other = s.create_registration(store::NewRegistration {
                base_url: "https://other.example/api/fasp".into(),
                server_id: "other-server-id-123456".into(),
                our_private_key: vec![1; 32],
                created_at: 0,
            })?;
            s.add_subscription(
                other.id,
                "12",
                Category::Content,
                SubscriptionType::Lifecycle,
                1,
            )
        })
        .unwrap();
    let reply = announce(
        &env,
        &announcement_body(
            &json!({"subscription": {"id": "12"}}),
            json!({"eventType": "new", "objectUris": ["https://a.example/1"]}),
        ),
    )
    .await;
    assert_eq!(reply.status, 204);
    assert!(received(&env).is_empty());
}

#[tokio::test]
async fn malformed_announcements_are_422() {
    let env = Env::new().await;
    set_up_sources(&env);
    env.enable_data_sharing_in_store();
    for bad in [
        &b"not json"[..],
        b"{}",
        br#"{"source":{"subscription":{"id":"12"}},"category":"content","objectUris":[]}"#,
        br#"{"source":{"subscription":{"id":"12"}},"category":"nope","eventType":"new","objectUris":[]}"#,
        br#"{"source":{},"category":"content","eventType":"new","objectUris":[]}"#,
    ] {
        let reply = announce(&env, bad).await;
        assert_eq!(reply.status, 422, "{}", String::from_utf8_lossy(bad));
    }
    assert!(received(&env).is_empty());
}
