//! Discovery and registration against a Mastodon-like fake.

mod common;

use std::sync::atomic::Ordering;

use apub::signatures::keys::Ed25519VerifyingKey;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::{FakeMastodon, default_info, mastodon_nodeinfo, now, start_ours};
use fasp::RegistrationError;
use fasp::registration::{discover_fasp_base_url, register};
use serde_json::json;
use sha2::{Digest, Sha256};
use store::RegistrationStatus;
use url::Url;

#[tokio::test]
async fn discovery_uses_fasp_base_url_from_nodeinfo() {
    let m = FakeMastodon::start().await;
    let mut doc = mastodon_nodeinfo("other");
    doc["metadata"]["faspBaseUrl"] = json!("https://fedi.example.com/fasp");
    m.set_nodeinfo(vec![("2.0", doc)]);
    let url = discover_fasp_base_url(&reqwest::Client::new(), &m.url(), None)
        .await
        .unwrap();
    assert_eq!(url.as_str(), "https://fedi.example.com/fasp");
}

#[tokio::test]
async fn discovery_follows_the_highest_2x_link() {
    let m = FakeMastodon::start().await;
    let with = |u: &str| {
        let mut doc = mastodon_nodeinfo("other");
        doc["metadata"]["faspBaseUrl"] = json!(u);
        doc
    };
    m.set_nodeinfo(vec![
        ("1.0", with("https://v1.example/fasp")),
        ("2.0", with("https://v20.example/fasp")),
        ("2.1", with("https://v21.example/fasp")),
        ("3.0", with("https://v3.example/fasp")),
    ]);
    let url = discover_fasp_base_url(&reqwest::Client::new(), &m.url(), None)
        .await
        .unwrap();
    assert_eq!(url.as_str(), "https://v21.example/fasp");
}

#[tokio::test]
async fn discovery_falls_back_to_api_fasp_for_mastodon() {
    let m = FakeMastodon::start().await;
    let url = discover_fasp_base_url(&reqwest::Client::new(), &m.url(), None)
        .await
        .unwrap();
    assert_eq!(url, m.fasp_base());
}

#[tokio::test]
async fn discovery_fails_clearly_for_unknown_software() {
    let m = FakeMastodon::start().await;
    m.set_nodeinfo(vec![("2.0", mastodon_nodeinfo("fediexample"))]);
    let err = discover_fasp_base_url(&reqwest::Client::new(), &m.url(), None)
        .await
        .unwrap_err();
    assert!(matches!(err, RegistrationError::NoFaspBaseUrl { .. }));
    let text = err.to_string();
    assert!(text.contains("fediexample"), "{text}");
    assert!(text.contains(&m.origin), "{text}");
    assert!(text.contains("explicitly"), "{text}");
}

#[tokio::test]
async fn discovery_override_skips_the_network() {
    // Nothing listens on this port: an override must not touch the server.
    let server = Url::parse("http://127.0.0.1:1").unwrap();
    let wanted = Url::parse("https://elsewhere.example/fasp").unwrap();
    let url = discover_fasp_base_url(&reqwest::Client::new(), &server, Some(&wanted))
        .await
        .unwrap();
    assert_eq!(url, wanted);
}

#[tokio::test]
async fn discovery_errors_name_the_url() {
    let server = Url::parse("http://127.0.0.1:1").unwrap();
    let err = discover_fasp_base_url(&reqwest::Client::new(), &server, None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("127.0.0.1:1"), "{err}");

    let m = FakeMastodon::start().await;
    m.set_nodeinfo(vec![]);
    let err = discover_fasp_base_url(&reqwest::Client::new(), &m.url(), None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, RegistrationError::BadResponse { .. }),
        "{err}"
    );
}

#[tokio::test]
async fn nodeinfo_links_to_other_origins_are_refused() {
    let m = FakeMastodon::start().await;
    // The fake builds hrefs from its `origin`; point it somewhere else.
    *m.inner.origin.lock().unwrap() = "http://evil.example".to_owned();
    let err = discover_fasp_base_url(&reqwest::Client::new(), &m.url(), None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("own origin"), "{err}");
}

#[tokio::test]
async fn full_registration_flow() {
    let m = FakeMastodon::start().await;
    let ours = start_ours(default_info(), Vec::new()).await;
    let outcome = register(
        &ours.http,
        &*ours.state,
        "Mwmbl Social",
        &Url::parse(&format!("{}/", ours.base_url)).unwrap(),
        &m.url(),
        None,
        1234,
    )
    .await
    .unwrap();

    // What the server received.
    let received = m.inner.registered.lock().unwrap().clone().unwrap();
    assert_eq!(received["name"], "Mwmbl Social");
    assert_eq!(
        received["baseUrl"],
        ours.base_url.as_str(),
        "no trailing slash"
    );
    let server_id = received["serverId"].as_str().unwrap();
    assert!(server_id.len() >= 16);
    assert!(
        server_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    );
    let public_key = STANDARD
        .decode(received["publicKey"].as_str().unwrap())
        .unwrap();
    assert_eq!(public_key.len(), 32);

    // The fingerprint is base64(sha256(raw public key)).
    assert_eq!(
        outcome.fingerprint,
        STANDARD.encode(Sha256::digest(&public_key))
    );
    assert_eq!(outcome.completion_uri, format!("{}/admin/fasps", m.origin));
    assert_eq!(outcome.fasp_base_url, m.fasp_base());

    // What we stored.
    let reg = ours
        .state
        .store
        .lock()
        .unwrap()
        .registration(outcome.registration_id)
        .unwrap()
        .unwrap();
    assert_eq!(reg.status, RegistrationStatus::Pending);
    assert_eq!(reg.base_url, format!("{}/api/fasp", m.origin));
    assert_eq!(reg.server_id, server_id);
    assert_eq!(reg.fasp_id.as_deref(), Some("dfkl3msw6ps3"));
    assert_eq!(reg.created_at, 1234);
    assert_eq!(
        reg.their_public_key.unwrap(),
        m.inner.key.verifying_key().to_bytes().to_vec()
    );
    // The stored private key matches the public key we sent.
    let ours_key =
        apub::signatures::keys::Ed25519SigningKey::from_seed_slice(&reg.our_private_key).unwrap();
    assert_eq!(ours_key.verifying_key().to_bytes().to_vec(), public_key);
    assert_eq!(
        Ed25519VerifyingKey::from_bytes(&public_key)
            .unwrap()
            .fingerprint(),
        outcome.fingerprint
    );
}

#[tokio::test]
async fn failed_registration_leaves_no_row_and_can_be_retried() {
    let m = FakeMastodon::start().await;
    let ours = start_ours(default_info(), Vec::new()).await;
    let our_url = Url::parse(&ours.base_url).unwrap();
    m.inner.registration_status.store(500, Ordering::SeqCst);
    let err = register(
        &ours.http,
        &*ours.state,
        "n",
        &our_url,
        &m.url(),
        None,
        now(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, RegistrationError::Status { status: 500, .. }),
        "{err}"
    );
    assert!(err.to_string().contains("/api/fasp/registration"));
    assert!(
        ours.state
            .store
            .lock()
            .unwrap()
            .list_registrations()
            .unwrap()
            .is_empty()
    );

    // The spec's 201 works as well as Mastodon's 200.
    m.inner.registration_status.store(201, Ordering::SeqCst);
    register(
        &ours.http,
        &*ours.state,
        "n",
        &our_url,
        &m.url(),
        None,
        now(),
    )
    .await
    .unwrap();
    // A second registration with the same server is refused, naming the existing one.
    let err = register(
        &ours.http,
        &*ours.state,
        "n",
        &our_url,
        &m.url(),
        None,
        now(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, RegistrationError::AlreadyRegistered { .. }),
        "{err}"
    );
}

#[tokio::test]
async fn a_revoked_or_abandoned_registration_is_replaced() {
    let m = FakeMastodon::start().await;
    let ours = start_ours(default_info(), Vec::new()).await;
    let our_url = Url::parse(&ours.base_url).unwrap();
    let register_now = async |now: i64| {
        register(&ours.http, &*ours.state, "n", &our_url, &m.url(), None, now).await
    };
    let rows = || {
        ours.state
            .store
            .lock()
            .unwrap()
            .list_registrations()
            .unwrap()
    };

    let first = register_now(now()).await.unwrap();
    assert_eq!(first.replaced_registration_id, None);
    let first_server_id = m.inner.registered.lock().unwrap().clone().unwrap()["serverId"].clone();
    let first_row = rows().into_iter().next().unwrap();

    // Revoked: registering again replaces it with a new key and serverId.
    ours.state
        .store
        .lock()
        .unwrap()
        .set_registration_status(first.registration_id, RegistrationStatus::Revoked)
        .unwrap();
    let second = register_now(now()).await.unwrap();
    assert_ne!(second.registration_id, first.registration_id);
    assert_eq!(second.replaced_registration_id, Some(first.registration_id));
    assert_ne!(second.fingerprint, first.fingerprint);
    let after = rows();
    assert_eq!(after.len(), 1);
    let second_row = after.into_iter().next().unwrap();
    assert_eq!(second_row.id, second.registration_id);
    assert_eq!(second_row.status, RegistrationStatus::Pending);
    assert_ne!(second_row.server_id, first_row.server_id);
    assert_ne!(second_row.our_private_key, first_row.our_private_key);
    assert_ne!(
        m.inner.registered.lock().unwrap().clone().unwrap()["serverId"],
        first_server_id
    );

    // Live, pending the admin's approval: refused, naming it.
    let err = register_now(now()).await.unwrap_err();
    assert!(
        matches!(err, RegistrationError::AlreadyRegistered { registration_id, .. }
            if registration_id == second.registration_id),
        "{err}"
    );

    // A pending row with no faspId, from an attempt a crash interrupted: replaced once it is
    // old enough not to be an attempt still in flight.
    let stale = {
        let store = ours.state.store.lock().unwrap();
        store.delete_registration(second.registration_id).unwrap();
        store
            .create_registration(store::NewRegistration {
                base_url: second_row.base_url.clone(),
                server_id: "interrupted".into(),
                our_private_key: vec![9; 32],
                created_at: 1000,
            })
            .unwrap()
    };
    assert!(matches!(
        register_now(1010).await,
        Err(RegistrationError::AlreadyRegistered { .. })
    ));
    let third = register_now(1000 + 3600).await.unwrap();
    assert_eq!(third.replaced_registration_id, Some(stale.id));
    assert_eq!(rows().len(), 1);
}

#[tokio::test]
async fn override_is_used_for_the_registration_call() {
    let m = FakeMastodon::start().await;
    let ours = start_ours(default_info(), Vec::new()).await;
    // The server URL is unreachable; only the override is used.
    let outcome = register(
        &ours.http,
        &*ours.state,
        "n",
        &Url::parse(&ours.base_url).unwrap(),
        &Url::parse("http://127.0.0.1:1").unwrap(),
        Some(&m.fasp_base()),
        now(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.fasp_base_url, m.fasp_base());
}
