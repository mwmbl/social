//! End to end: our full server (real `serve_on`: router, writer, worker, FASP maintenance) on a
//! random local port, against a fake Mastodon that registers, signs, activates `data_sharing`,
//! announces and serves `ActivityPub` objects. Nothing leaves 127.0.0.1.

mod common;

use std::time::Duration;

use apub::signatures::keys::Ed25519VerifyingKey;
use common::mastodon::FakeMastodon;
use mwmbl_social::cli::{self, serve_on};
use mwmbl_social::config::Config;
use serde_json::{Value, json};
use store::{BackfillState, Category, RegistrationStatus, Store, TombstoneKind};
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

struct Ours {
    config: Config,
    base: String,
    http: reqwest::Client,
}

impl Ours {
    fn store(&self) -> Store {
        Store::open(&self.config.store_path()).unwrap()
    }

    async fn get(&self, path: &str) -> (u16, String) {
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    /// The URIs found by a search.
    async fn search(&self, kind: &str, q: &str) -> Vec<String> {
        let (status, body) = self.get(&format!("/api/v1/search/{kind}?q={q}")).await;
        assert_eq!(status, 200, "{body}");
        let results: Vec<Value> = serde_json::from_str(&body).unwrap();
        results
            .iter()
            .map(|r| r["uri"].as_str().unwrap().to_string())
            .collect()
    }
}

fn announcement(source: &Value, category: &str, extra: Value, uris: &[String]) -> Value {
    let mut body = json!({"source": source, "category": category, "objectUris": uris});
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    body
}

fn lifecycle(sub_id: &str, category: &str, event: &str, uris: &[String]) -> Value {
    announcement(
        &json!({"subscription": {"id": sub_id}}),
        category,
        json!({"eventType": event}),
        uris,
    )
}

fn backfill(id: &str, category: &str, more: bool, uris: &[String]) -> Value {
    announcement(
        &json!({"backfillRequest": {"id": id}}),
        category,
        json!({"moreObjectsAvailable": more}),
        uris,
    )
}

const ANNOUNCEMENTS: &str = "/data_sharing/v0/announcements";
const ACTIVATION: &str = "/capabilities/data_sharing/0/activation";

#[tokio::test]
#[allow(clippy::too_many_lines)] // One scenario, read top to bottom.
async fn registers_activates_ingests_deletes_and_stops() {
    let fake = FakeMastodon::start().await;
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
        allow_private_addresses: true,
        metrics: true,
        backfill_max_count: 5,
        privacy_policy_url: Some("https://social.example/privacy".to_string()),
        contact_email: Some("admin@social.example".to_string()),
        root_redirect: Some("https://mwmbl.example/social".to_string()),
        ..Config::default()
    };
    let ours = Ours {
        base: format!("http://{addr}"),
        config: config.clone(),
        // Never follow the root redirect off 127.0.0.1.
        http: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
    };
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server = tokio::spawn(serve_on(config.clone(), listener, shutdown_rx));

    // Existing routes keep working, and a 404 elsewhere is still the plain 404.
    wait_until("the server to answer", async || {
        ours.get("/actor").await.0 == 200
    })
    .await;
    assert_eq!(ours.get("/nonexistent").await.0, 404);
    assert_eq!(ours.get("/").await.0, 302);
    assert_eq!(ours.get("/api/v1/search/posts").await.0, 422);
    // The FASP paths require a signature: unsigned means 401, never a leak or a 404.
    for path in ["/provider_info", "/capabilities/data_sharing/0/activation"] {
        assert_eq!(ours.get(path).await.0, 401, "{path}");
    }

    // register: nodeinfo discovery finds /api/fasp for mastodon; the admin gets a fingerprint.
    let outcome = cli::register(&config, &fake.origin, None).await.unwrap();
    assert_eq!(outcome.fasp_base_url.as_str(), fake.url("/api/fasp"));
    assert_eq!(outcome.completion_uri, fake.url("/admin/fasps"));
    let registered = fake.registered();
    assert_eq!(registered["baseUrl"], ours.base);
    assert_eq!(registered["name"], "Mwmbl Social");
    let our_key =
        Ed25519VerifyingKey::from_base64(registered["publicKey"].as_str().unwrap()).unwrap();
    assert_eq!(outcome.fingerprint, our_key.fingerprint());
    let report = cli::registration_report(&outcome, &ours.base);
    assert!(report.contains(&outcome.fingerprint) && report.contains(&outcome.completion_uri));
    let id = outcome.registration_id;
    assert_eq!(
        ours.store().registration(id).unwrap().unwrap().status,
        RegistrationStatus::Pending
    );

    // Mastodon calls provider_info, signed with keyid = serverId; the registration goes active.
    let reply = fake.call_ours("GET", "/provider_info", b"").await;
    assert_eq!(reply.status, 200);
    fake.verify(&reply);
    let info = reply.json();
    assert_eq!(info["name"], "Mwmbl Social");
    assert_eq!(
        info["capabilities"],
        json!([
            {"id": "data_sharing", "version": "0.1"},
            {"id": "account_search", "version": "0.1"},
            {"id": "trends", "version": "0.1"},
            {"id": "follow_recommendation", "version": "0.1"},
        ])
    );
    assert_eq!(
        info["privacyPolicy"],
        json!([{"url": "https://social.example/privacy", "language": "en"}])
    );
    assert_eq!(info["contactEmail"], "admin@social.example");
    assert_eq!(
        ours.store().registration(id).unwrap().unwrap().status,
        RegistrationStatus::Active
    );

    // Activation: 204, then we create a lifecycle subscription and a backfill per category,
    // and a content trends subscription with the spec's default threshold.
    let reply = fake.call_ours("POST", ACTIVATION, b"").await;
    assert_eq!(reply.status, 204);
    fake.verify(&reply);
    wait_until(
        "three subscriptions and two backfills on the fake",
        async || {
            fake.inner.subscriptions.lock().unwrap().len() == 3
                && fake.inner.backfills.lock().unwrap().len() == 2
                && ours.store().list_backfills(id).unwrap().len() == 2
        },
    )
    .await;
    let subscription_bodies = fake.inner.subscription_bodies.lock().unwrap().clone();
    for category in ["content", "account"] {
        assert!(
            subscription_bodies
                .iter()
                .any(|b| b == &json!({"category": category, "subscriptionType": "lifecycle", "maxBatchSize": 20})),
            "{category}: {subscription_bodies:?}"
        );
    }
    assert!(subscription_bodies.contains(&json!({
        "category": "content", "subscriptionType": "trends", "maxBatchSize": 20,
        "threshold": {"timeframe": 15, "shares": 3, "likes": 3, "replies": 3}
    })));
    let backfill_bodies = fake.inner.backfill_bodies.lock().unwrap().clone();
    assert!(
        backfill_bodies.iter().all(|b| b["maxCount"] == 5),
        "{backfill_bodies:?}"
    );
    let stored = ours.store().list_subscriptions(id).unwrap();
    assert_eq!(stored.len(), 3);
    assert!(
        stored
            .iter()
            .any(|s| s.category == Category::Content
                && s.remote_id == fake.subscription_id("content"))
    );
    // Activating again is idempotent: nothing more is created.
    assert_eq!(fake.call_ours("POST", ACTIVATION, b"").await.status, 204);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(fake.inner.subscriptions.lock().unwrap().len(), 3);
    assert_eq!(fake.inner.backfills.lock().unwrap().len(), 2);

    // A `new` lifecycle event (subscription id as a string): fetched, consented, indexed.
    let content_sub = fake.subscription_id("content");
    let n1 = fake.url("/notes/n1");
    let reply = fake
        .call_ours_json(
            "POST",
            ANNOUNCEMENTS,
            &lifecycle(&content_sub, "content", "new", std::slice::from_ref(&n1)),
        )
        .await;
    assert_eq!(reply.status, 204);
    fake.verify(&reply);
    wait_until("the note to be searchable", async || {
        ours.search("posts", "garlic").await == [n1.clone()]
    })
    .await;
    assert_eq!(
        ours.search("accounts", "alice").await,
        [fake.url("/users/alice")]
    );

    // An account lifecycle event maps to an actor fetch.
    let account_sub = fake.subscription_id("account");
    let carol = fake.url("/users/carol");
    let reply = fake
        .call_ours_json(
            "POST",
            ANNOUNCEMENTS,
            &lifecycle(&account_sub, "account", "new", std::slice::from_ref(&carol)),
        )
        .await;
    assert_eq!(reply.status, 204);
    wait_until("carol to be searchable", async || {
        ours.search("accounts", "carol").await == [carol.clone()]
    })
    .await;

    // Backfill with more available: queued as backfill work, and a continuation is requested.
    fake.inner
        .continuations_left
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let content_backfill = fake.backfill_id("content");
    let n2 = fake.url("/notes/n2");
    let reply = fake
        .call_ours_json(
            "POST",
            ANNOUNCEMENTS,
            &backfill(
                &content_backfill,
                "content",
                true,
                std::slice::from_ref(&n2),
            ),
        )
        .await;
    assert_eq!(reply.status, 204);
    wait_until("the continuation call", async || {
        fake.inner.continuations.lock().unwrap().len() == 1
    })
    .await;
    assert_eq!(
        fake.inner.continuations.lock().unwrap()[0],
        format!("/data_sharing/v0/backfill_requests/{content_backfill}/continuation")
    );
    wait_until("the backfilled note", async || {
        ours.search("posts", "sourdough").await == [n2.clone()]
    })
    .await;
    // The final, empty batch marks the backfill done and asks for nothing more.
    let reply = fake
        .call_ours_json(
            "POST",
            ANNOUNCEMENTS,
            &backfill(&content_backfill, "content", false, &[]),
        )
        .await;
    assert_eq!(reply.status, 204);
    wait_until("the backfill to be done", async || {
        ours.store()
            .list_backfills(id)
            .unwrap()
            .iter()
            .any(|b| b.remote_id == content_backfill && b.state == BackfillState::Done)
    })
    .await;
    assert_eq!(fake.inner.continuations.lock().unwrap().len(), 1);

    // /metrics (enabled in this config): registrations, capabilities, fetches, index sizes.
    let (status, metrics) = ours.get("/metrics").await;
    assert_eq!(status, 200);
    for line in [
        "mwmbl_social_registrations{status=\"active\"} 1",
        "mwmbl_social_capabilities_enabled{capability=\"data_sharing\"} 1",
        "mwmbl_social_index_pages{index=\"posts\"} 64",
        "mwmbl_social_queue_depth_total 0",
    ] {
        assert!(
            metrics.lines().any(|l| l == line),
            "{line:?} missing from\n{metrics}"
        );
    }
    let found: u64 = metrics
        .lines()
        .find_map(|l| l.strip_prefix("mwmbl_social_fetch_outcomes_total{outcome=\"found\"} "))
        .unwrap()
        .parse()
        .unwrap();
    assert!(found >= 4, "{metrics}");

    // An update event is fetched only after Mastodon's 3-minute cache of the old version has
    // expired.
    let edited = fake.url("/notes/edited");
    let reply = fake
        .call_ours_json(
            "POST",
            ANNOUNCEMENTS,
            &lifecycle(
                &content_sub,
                "content",
                "update",
                std::slice::from_ref(&edited),
            ),
        )
        .await;
    assert_eq!(reply.status, 204);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .cast_signed();
    let due_now = ours.store().next_due(now + 60, 10).unwrap();
    assert!(due_now.iter().all(|q| q.uri != edited), "fetched too early");
    let due_later = ours.store().next_due(now + 300, 10).unwrap();
    assert!(due_later.iter().any(|q| q.uri == edited), "{due_later:?}");
    let queued = due_later.iter().find(|q| q.uri == edited).unwrap();
    ours.store().complete(&edited, queued.generation).unwrap();

    // A delete event, with the object gone: removed from search, and a tombstone is left.
    fake.set_gone("/notes/n1");
    let reply = fake
        .call_ours_json(
            "POST",
            ANNOUNCEMENTS,
            &lifecycle(&content_sub, "content", "delete", std::slice::from_ref(&n1)),
        )
        .await;
    assert_eq!(reply.status, 204);
    wait_until("the note to disappear", async || {
        ours.search("posts", "garlic").await.is_empty()
    })
    .await;
    assert!(
        ours.store()
            .list_tombstones()
            .unwrap()
            .iter()
            .any(|t| t.kind == TombstoneKind::Object && t.uri == n1)
    );
    assert_eq!(ours.store().queue_len().unwrap(), 0);

    // Deactivation: local rows go, announcements are dropped, and the fake is never called.
    let calls_before = fake.fasp_call_count();
    let n3 = fake.url("/notes/n3");
    let reply = fake.call_ours("DELETE", ACTIVATION, b"").await;
    assert_eq!(reply.status, 204);
    fake.verify(&reply);
    assert_eq!(ours.store().list_subscriptions(id).unwrap().len(), 0);
    assert_eq!(ours.store().list_backfills(id).unwrap().len(), 0);
    for body in [
        lifecycle(&content_sub, "content", "new", std::slice::from_ref(&n3)),
        backfill(
            &content_backfill,
            "content",
            true,
            std::slice::from_ref(&n3),
        ),
    ] {
        let reply = fake.call_ours_json("POST", ANNOUNCEMENTS, &body).await;
        assert_eq!(reply.status, 204);
        fake.verify(&reply);
    }
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(ours.store().queue_len().unwrap(), 0);
    assert_eq!(
        fake.ap_hits("/notes/n3"),
        0,
        "a dropped announcement was fetched"
    );
    assert!(ours.search("posts", "narwhal").await.is_empty());
    assert_eq!(
        fake.fasp_call_count(),
        calls_before,
        "we called a disabled capability"
    );

    // Unregistering revokes: further requests get 401.
    cli::unregister(&config, id).unwrap();
    let reply = fake.call_ours("GET", "/provider_info", b"").await;
    assert_eq!(reply.status, 401);

    assert_eq!(
        fake.failures(),
        Vec::<String>::new(),
        "our requests failed Mastodon's checks"
    );
    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(20), server)
        .await
        .expect("the server stops on shutdown")
        .unwrap()
        .unwrap();
}
