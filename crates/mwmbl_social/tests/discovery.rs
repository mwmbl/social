//! FASP `trends` and `follow_recommendation` against our real server (`serve_on`) and a fake
//! Mastodon: trending announcements build trend state, the trends endpoints return only
//! indexed and consented posts, ranked, and tombstones and disabled capabilities are honoured.

mod common;

use std::time::Duration;

use common::mastodon::{FakeMastodon, public_note};
use mwmbl_social::cli::{self, serve_on};
use mwmbl_social::config::Config;
use serde_json::{Value, json};
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

fn event(sub_id: &str, category: &str, event: &str, uris: &[String]) -> Value {
    json!({
        "source": {"subscription": {"id": sub_id}},
        "category": category,
        "eventType": event,
        "objectUris": uris,
    })
}

async fn announce(fake: &FakeMastodon, body: &Value) {
    let reply = fake
        .call_ours_json("POST", "/data_sharing/v0/announcements", body)
        .await;
    assert_eq!(reply.status, 204);
    fake.verify(&reply);
}

async fn get(fake: &FakeMastodon, path: &str) -> (u16, Value) {
    let reply = fake.call_ours("GET", path, b"").await;
    fake.verify(&reply);
    let body = if reply.body.is_empty() {
        Value::Null
    } else {
        reply.json()
    };
    (reply.status, body)
}

async fn trending(fake: &FakeMastodon, query: &str) -> Vec<(String, u64)> {
    let (status, body) = get(fake, &format!("/trends/v0/content{query}")).await;
    assert_eq!(status, 200, "{body}");
    body["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["uri"].as_str().unwrap().to_owned(),
                e["rank"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn trending_announcements_feed_trends_and_follow_recommendations() {
    let fake = FakeMastodon::start().await;
    fake.set_doc(
        "/notes/n1",
        public_note(
            &fake.origin,
            "n1",
            "alice",
            "Planting garlic today #gardening",
        ),
    );
    let mut n3 = public_note(
        &fake.origin,
        "n3",
        "alice",
        "Narwhal Garten #gardening #sea",
    );
    n3["contentMap"] = json!({"de": "<p>Narwhal Garten #gardening #sea</p>"});
    fake.set_doc("/notes/n3", n3);
    // Unlisted: Mastodon announces trending statuses whatever their visibility.
    let mut quiet = public_note(&fake.origin, "quiet", "alice", "Quiet quokka");
    quiet["to"] = json!([fake.url("/users/alice/followers")]);
    quiet["cc"] = json!([common::PUBLIC]);
    fake.set_doc("/notes/quiet", quiet);

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
        ..Config::default()
    };
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

    // Neither capability is enabled yet: signed 404s.
    assert_eq!(get(&fake, "/trends/v0/content").await.0, 404);
    let rec = format!(
        "/follow_recommendation/v0/accounts?accountUri={}",
        fake.url("/users/carol")
    );
    assert_eq!(get(&fake, &rec).await.0, 404);

    for capability in ["data_sharing", "trends"] {
        let path = format!("/capabilities/{capability}/0/activation");
        assert_eq!(fake.call_ours("POST", &path, b"").await.status, 204);
    }
    wait_until("the lifecycle and trends subscriptions", async || {
        fake.inner.subscriptions.lock().unwrap().len() == 3
    })
    .await;
    // follow_recommendation still is not enabled.
    assert_eq!(get(&fake, &rec).await.0, 404);
    let lifecycle = fake.subscription_id("content");
    let trends = fake.trends_subscription_id();
    let [n1, n2, n3, quiet] = ["n1", "n2", "n3", "quiet"].map(|n| fake.url(&format!("/notes/{n}")));

    // n1 and n2 are indexed through lifecycle events; n3 only ever trends.
    announce(
        &fake,
        &event(&lifecycle, "content", "new", &[n1.clone(), n2.clone()]),
    )
    .await;
    let carol = fake.url("/users/carol");
    let account_sub = fake.subscription_id("account");
    announce(
        &fake,
        &event(&account_sub, "account", "new", std::slice::from_ref(&carol)),
    )
    .await;

    // Trending events, one per interaction as Mastodon sends them.
    for (uri, times) in [(&n2, 3), (&n3, 2), (&n1, 1), (&quiet, 5)] {
        for _ in 0..times {
            announce(
                &fake,
                &event(&trends, "content", "trending", std::slice::from_ref(uri)),
            )
            .await;
        }
    }
    // Trending through the lifecycle subscription is not a trends source: ignored.
    for _ in 0..10 {
        announce(
            &fake,
            &event(&lifecycle, "content", "trending", std::slice::from_ref(&n1)),
        )
        .await;
    }

    // n3 was queued because it trended unindexed, fetched and indexed; the unlisted post was
    // fetched but never indexed, so it never trends.
    wait_until("the trending posts to be indexed", async || {
        trending(&fake, "").await == [(n2.clone(), 41), (n3.clone(), 31), (n1.clone(), 21)]
    })
    .await;
    wait_until("the unlisted post to be fetched", async || {
        fake.ap_hits("/notes/quiet") >= 1
    })
    .await;
    assert_eq!(trending(&fake, "").await.len(), 3);
    assert_eq!(trending(&fake, "?maxCount=1").await, [(n2.clone(), 41)]);
    assert_eq!(trending(&fake, "?withinLastHours=1").await.len(), 3);
    // Only n3 carries a language.
    assert_eq!(trending(&fake, "?language=de").await, [(n3.clone(), 31)]);
    assert!(trending(&fake, "?language=en").await.is_empty());
    assert_eq!(get(&fake, "/trends/v0/content?maxCount=0").await.0, 422);

    // Hashtags from the trending posts, scored by their events; links are never reported.
    let (status, tags) = get(&fake, "/trends/v0/hashtags").await;
    assert_eq!(status, 200);
    assert_eq!(
        tags,
        json!({"hashtags": [
            {"name": "#gardening", "rank": 41, "examples": [n3.clone(), n1.clone()]},
            {"name": "#sea", "rank": 31, "examples": [n3.clone()]},
        ]})
    );
    assert_eq!(
        get(&fake, "/trends/v0/links").await,
        (200, json!({"links": []}))
    );

    // A deleted post leaves trends (its tombstone), though its events remain.
    fake.set_gone("/notes/n2");
    announce(
        &fake,
        &event(&lifecycle, "content", "delete", std::slice::from_ref(&n2)),
    )
    .await;
    wait_until("the deleted post to leave trends", async || {
        trending(&fake, "").await == [(n3.clone(), 31), (n1.clone(), 21)]
    })
    .await;

    // follow_recommendation: related accounts by bio words, then trending authors.
    assert_eq!(
        fake.call_ours(
            "POST",
            "/capabilities/follow_recommendation/0/activation",
            b""
        )
        .await
        .status,
        204
    );
    let alice = fake.url("/users/alice");
    wait_until("carol to be indexed", async || {
        let (status, body) = get(&fake, &rec).await;
        status == 200 && body == json!([alice.clone()])
    })
    .await;
    let for_alice = format!("/follow_recommendation/v0/accounts?accountUri={alice}");
    assert_eq!(get(&fake, &for_alice).await, (200, json!([carol.clone()])));
    // An account we have not indexed gets the trending authors only.
    let stranger = format!(
        "/follow_recommendation/v0/accounts?accountUri={}",
        fake.url("/users/nobody")
    );
    assert_eq!(get(&fake, &stranger).await, (200, json!([alice.clone()])));
    assert_eq!(
        get(&fake, "/follow_recommendation/v0/accounts").await.0,
        422
    );

    // Deactivated: 404 again.
    assert_eq!(
        fake.call_ours("DELETE", "/capabilities/trends/0/activation", b"")
            .await
            .status,
        204
    );
    assert_eq!(get(&fake, "/trends/v0/content").await.0, 404);

    assert_eq!(fake.failures(), Vec::<String>::new());
    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(20), server)
        .await
        .expect("the server stops on shutdown")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn tombstoned_posts_and_authors_are_never_returned() {
    use common::TestEnv;
    use fasp::announcement::{Announcement, AnnouncementSource, EventType};
    use fasp::http::{FaspState, TrendsQuery};
    use mwmbl_social::fasp_app::FaspApp;
    use store::{Category, FetchKind, FetchReason, Store};

    let env = TestEnv::new().await;
    env.ingest("/notes/public", FetchKind::Object, FetchReason::Announce)
        .await;
    let public = env.fake.url("/notes/public");
    let alice = env.fake.url("/users/alice");
    let app = FaspApp::new(
        Store::open(&env.config.store_path()).unwrap(),
        reqwest::Client::new(),
        &env.config,
        "http://127.0.0.1:1",
        env.handle.readers(),
        env.handle.tombstones(),
    );
    let trending = Announcement {
        source: AnnouncementSource::Subscription {
            remote_id: "1".into(),
            event: EventType::Trending,
        },
        category: Category::Content,
        object_uris: vec![public.clone()],
    };
    app.on_announcement(1, trending).await;
    let query = TrendsQuery {
        within_last_hours: 24,
        max_count: 20,
        language: None,
    };
    let uris = async || -> Vec<String> {
        app.trending_content(1, &query)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.uri)
            .collect()
    };
    assert_eq!(uris().await, std::slice::from_ref(&public));
    let stranger = "https://elsewhere.example/users/x";
    assert_eq!(
        app.follow_recommendations(1, stranger, None).await.unwrap(),
        std::slice::from_ref(&alice)
    );

    // A domain block keeps the records on their pages until the sweep, but tombstones hide
    // them from trends and recommendations at once.
    env.handle
        .forget(mwmbl_social::writer::ForgetTarget::Domain(
            "127.0.0.1".into(),
        ))
        .await
        .unwrap();
    assert!(env.post_pages().iter().any(|p| p.uri == public));
    assert!(uris().await.is_empty());
    assert!(
        app.follow_recommendations(1, stranger, None)
            .await
            .unwrap()
            .is_empty()
    );
}
