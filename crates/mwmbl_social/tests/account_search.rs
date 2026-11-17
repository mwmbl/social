//! FASP `account_search` against our real server: indexed accounts are found by a request
//! signed the way Mastodon signs it, best first; consent and forgetting are honoured.

mod common;

use std::time::Duration;

use common::TestEnv;
use common::mastodon::{FakeMastodon, actor};
use fasp::http::FaspState;
use mwmbl_social::cli::{self, serve_on};
use mwmbl_social::config::Config;
use mwmbl_social::fasp_app::FaspApp;
use serde_json::{Value, json};
use store::{FetchKind, FetchReason, Store};
use tokio::sync::watch;

async fn wait_until(what: &str, mut check: impl AsyncFnMut() -> bool) {
    for _ in 0..300 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

fn with_summary(mut doc: Value, summary: &str) -> Value {
    doc["summary"] = json!(format!("<p>{summary}</p>"));
    doc
}

#[tokio::test]
async fn mastodon_searches_accounts_over_signed_fasp() {
    let fake = FakeMastodon::start().await;
    // erin only mentions alice in her bio; dave has not opted in to discovery.
    fake.set_doc(
        "/users/erin",
        with_summary(actor(&fake.origin, "erin"), "Friends with alice"),
    );
    let mut dave = actor(&fake.origin, "dave");
    dave["discoverable"] = json!(false);
    fake.set_doc("/users/dave", dave);

    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        listen: addr.to_string(),
        base_url: Some(format!("http://{addr}")),
        posts_pages: 64,
        accounts_pages: 64,
        allow_http: true,
        ..Config::default()
    };
    let store = || Store::open(&config.store_path()).unwrap();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server = tokio::spawn(serve_on(config.clone(), listener, shutdown_rx));
    let http = reqwest::Client::new();
    wait_until("the server to answer", async || {
        http.get(format!("http://{addr}/actor"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    })
    .await;

    cli::register(&config, &fake.origin, None).await.unwrap();
    const SEARCH: &str = "/account_search/v0/search?term=alice&limit=10";

    // Not yet activated: the capability is not ours to serve for this server.
    assert_eq!(
        fake.call_ours("GET", "/provider_info", b"").await.status,
        200
    );
    let reply = fake.call_ours("GET", SEARCH, b"").await;
    assert_eq!(reply.status, 404);
    fake.verify(&reply);

    for capability in ["account_search", "data_sharing"] {
        let path = format!("/capabilities/{capability}/0/activation");
        let reply = fake.call_ours("POST", &path, b"").await;
        assert_eq!(reply.status, 204);
        fake.verify(&reply);
    }
    wait_until("the account subscription", async || {
        fake.inner.subscriptions.lock().unwrap().len() == 3
    })
    .await;
    let account_sub = fake.subscription_id("account");
    let uris: Vec<String> = ["alice", "erin", "dave", "carol"]
        .iter()
        .map(|n| fake.url(&format!("/users/{n}")))
        .collect();
    let body = json!({
        "source": {"subscription": {"id": account_sub}},
        "category": "account",
        "eventType": "new",
        "objectUris": uris,
    });
    let reply = fake
        .call_ours_json("POST", "/data_sharing/v0/announcements", &body)
        .await;
    assert_eq!(reply.status, 204);

    let alice = fake.url("/users/alice");
    let erin = fake.url("/users/erin");
    wait_until("alice and erin to be searchable", async || {
        let reply = fake.call_ours("GET", SEARCH, b"").await;
        reply.status == 200 && reply.json() == json!([alice, erin])
    })
    .await;

    // Exactly what Mastodon sends: best first (the handle match before the bio mention), a
    // signed JSON array of actor URIs, and no Link header for a complete result.
    let reply = fake.call_ours("GET", SEARCH, b"").await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("content-type"), Some("application/json"));
    assert_eq!(reply.json(), json!([alice, erin]));
    assert!(reply.header("link").is_none());
    fake.verify(&reply);

    // Paging: one result per page, with a Link to the next.
    let page = fake
        .call_ours("GET", "/account_search/v0/search?term=alice&limit=1", b"")
        .await;
    assert_eq!(page.json(), json!([alice]));
    let link = page.header("link").unwrap().to_owned();
    assert!(
        link.ends_with("/account_search/v0/search?term=alice&limit=1&offset=1>; rel=\"next\""),
        "{link}"
    );
    fake.verify(&page);

    // Validation and language filtering reach the right code.
    let bad = fake
        .call_ours("GET", "/account_search/v0/search?limit=3", b"")
        .await;
    assert_eq!(bad.status, 422);
    fake.verify(&bad);
    let german = fake
        .call_ours(
            "GET",
            "/account_search/v0/search?term=alice&language=de",
            b"",
        )
        .await;
    assert_eq!(german.status, 200);
    // Accounts with no observed languages pass the filter (see `language_matches`).
    assert_eq!(german.json(), json!([alice, erin]));

    // Dave was fetched but is not discoverable: never indexed, so never found. Carol is.
    assert!(fake.ap_hits("/users/dave") >= 1);
    wait_until("the queue to drain", async || {
        store().queue_len().unwrap() == 0
    })
    .await;
    for term in ["dave", "gardener"] {
        let reply = fake
            .call_ours(
                "GET",
                &format!("/account_search/v0/search?term={term}"),
                b"",
            )
            .await;
        assert!(
            !reply
                .json()
                .as_array()
                .unwrap()
                .contains(&json!(fake.url("/users/dave"))),
            "{term}"
        );
    }
    let reply = fake
        .call_ours("GET", "/account_search/v0/search?term=gardener", b"")
        .await;
    assert_eq!(reply.json().as_array().unwrap().len(), 3);

    // Deactivated: 404 again.
    let reply = fake
        .call_ours("DELETE", "/capabilities/account_search/0/activation", b"")
        .await;
    assert_eq!(reply.status, 204);
    assert_eq!(fake.call_ours("GET", SEARCH, b"").await.status, 404);

    assert_eq!(fake.failures(), Vec::<String>::new());
    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(20), server)
        .await
        .expect("the server stops on shutdown")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn blocked_and_forgotten_accounts_are_not_returned_and_pages_are_consistent() {
    let env = TestEnv::new().await;
    let mut dave = env.fake.doc("/users/alice").unwrap();
    dave["id"] = json!(env.fake.url("/users/dave"));
    dave["preferredUsername"] = json!("dave");
    dave["discoverable"] = json!(false);
    env.fake.set_doc("/users/dave", dave);
    let mut erin = with_summary(env.fake.doc("/users/alice").unwrap(), "Friends with alice");
    erin["id"] = json!(env.fake.url("/users/erin"));
    erin["preferredUsername"] = json!("erin");
    env.fake.set_doc("/users/erin", erin);
    for path in ["/users/alice", "/users/erin", "/users/dave"] {
        env.ingest(path, FetchKind::Actor, FetchReason::Announce)
            .await;
    }

    let app = FaspApp::new(
        Store::open(&env.config.store_path()).unwrap(),
        reqwest::Client::new(),
        &env.config,
        "http://127.0.0.1:1",
        env.handle.readers(),
        env.handle.tombstones(),
    );
    let alice = env.fake.url("/users/alice");
    let erin = env.fake.url("/users/erin");
    let search = async |term: &str, limit: usize, offset: usize, language: Option<&str>| {
        app.account_search(1, term, limit, offset, language)
            .await
            .unwrap()
    };

    let all = search("alice", 10, 0, None).await;
    assert_eq!(all.uris, [alice.clone(), erin.clone()]);
    assert!(!all.has_more);
    // `limit` and `offset` page through the same ranking.
    let first = search("alice", 1, 0, None).await;
    assert_eq!((first.uris, first.has_more), (vec![alice.clone()], true));
    let second = search("alice", 1, 1, None).await;
    assert_eq!((second.uris, second.has_more), (vec![erin.clone()], false));
    let beyond = search("alice", 1, 2, None).await;
    assert!(beyond.uris.is_empty() && !beyond.has_more);
    // Nothing is stored for a non-discoverable account, so it is never returned.
    assert!(search("dave", 10, 0, None).await.uris.is_empty());

    // Forgetting an actor hides it at once; the others stay.
    env.handle
        .forget(mwmbl_social::writer::ForgetTarget::Uri(erin.clone()))
        .await
        .unwrap();
    assert_eq!(
        search("alice", 10, 0, None).await.uris,
        std::slice::from_ref(&alice)
    );

    // Blocking its domain hides every account on it, pooled across registrations.
    env.handle
        .forget(mwmbl_social::writer::ForgetTarget::Domain(
            "127.0.0.1".into(),
        ))
        .await
        .unwrap();
    assert!(search("alice", 10, 0, None).await.uris.is_empty());
}
