//! Audit C (2026-11-19): FASP, registration and admin. Tests marked `#[ignore = "AUDIT: ..."]`
//! demonstrate a finding and fail until it is fixed (`cargo test -p mwmbl_social --test audit_c
//! -- --ignored`); the others pin claims that were checked and hold.

mod common;

use std::time::Duration;

use common::FakeHost;
use common::mastodon::FakeMastodon;
use mwmbl_social::cli::{self, serve_on};
use mwmbl_social::config::Config;
use serde_json::{Value, json};
use store::{FetchKind, FetchReason, NewFetch, Store};
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

/// Polls `check` for about `millis` and returns whether it ever held.
async fn becomes_true(millis: u64, mut check: impl FnMut() -> bool) -> bool {
    for _ in 0..(millis / 50) {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    check()
}

struct Running {
    addr: std::net::SocketAddr,
    shutdown: watch::Sender<bool>,
    _dir: tempfile::TempDir,
    config: Config,
}

impl Running {
    async fn stop(self) {
        self.shutdown.send(true).unwrap();
    }
}

/// Our real server, registered with `fake` and with `capabilities` activated by it.
async fn serve_registered(
    fake: &FakeMastodon,
    capabilities: &[&str],
    tweak: impl FnOnce(&mut Config),
) -> Running {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = Config {
        data_dir: dir.path().to_path_buf(),
        listen: addr.to_string(),
        base_url: Some(format!("http://{addr}")),
        posts_pages: 64,
        accounts_pages: 64,
        allow_http: true,
        allow_private_addresses: true,
        ..Config::default()
    };
    tweak(&mut config);
    let (shutdown, shutdown_rx) = watch::channel(false);
    tokio::spawn(serve_on(config.clone(), listener, shutdown_rx));
    let http = reqwest::Client::new();
    wait_until("the server to answer", async || {
        http.get(format!("http://{addr}/actor"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    })
    .await;
    cli::register(&config, &fake.origin, None).await.unwrap();
    for capability in capabilities {
        let path = format!("/capabilities/{capability}/0/activation");
        assert_eq!(fake.call_ours("POST", &path, b"").await.status, 204);
    }
    if capabilities.contains(&"data_sharing") {
        wait_until("the lifecycle subscriptions", async || {
            fake.inner.subscriptions.lock().unwrap().len() >= 2
        })
        .await;
    }
    Running {
        addr,
        shutdown,
        _dir: dir,
        config,
    }
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
}

/// C1 (fixed): a registration's announcements are bound to its own host. The server registered
/// as `127.0.0.1:<port>` announces a URI on another host (`localhost:<port>`, a different fake)
/// next to one of its own: its own is fetched, the other is dropped, so a registered server
/// cannot point our fetcher at third-party hosts.
#[tokio::test]
async fn announced_uris_on_another_host_are_not_fetched() {
    let fake = FakeMastodon::start().await;
    let victim = FakeHost::start().await;
    let running = serve_registered(&fake, &["data_sharing"], |_| {}).await;
    let victim_uri = victim
        .url("/notes/public")
        .replace("127.0.0.1", "localhost");
    let own_uri = fake.url("/notes/n1");
    let lifecycle = fake.subscription_id("content");
    announce(
        &fake,
        &event(&lifecycle, "content", "new", &[own_uri, victim_uri]),
    )
    .await;
    wait_until("the server's own note to be fetched", async || {
        fake.ap_hits("/notes/n1") > 0
    })
    .await;
    let fetched_elsewhere = becomes_true(2000, || victim.hits("/notes/public") > 0).await;
    running.stop().await;
    assert!(
        !fetched_elsewhere,
        "a URI on a host other than the registration's was fetched"
    );
}

/// C2 (fixed): one `trending` announcement that lists the same URI many times counts once, so
/// a single signed request cannot take a post to rank 100 in the trends pooled across every
/// registration (and its author into everyone's follow recommendations).
#[tokio::test]
async fn a_uri_repeated_in_one_trending_announcement_counts_once() {
    let fake = FakeMastodon::start().await;
    let running = serve_registered(&fake, &["data_sharing", "trends"], |_| {}).await;
    wait_until("the trends subscription", async || {
        fake.inner.subscriptions.lock().unwrap().len() == 3
    })
    .await;
    let n1 = fake.url("/notes/n1");
    let lifecycle = fake.subscription_id("content");
    announce(
        &fake,
        &event(&lifecycle, "content", "new", std::slice::from_ref(&n1)),
    )
    .await;
    let http = reqwest::Client::new();
    let addr = running.addr;
    wait_until("n1 to be indexed", async || {
        let body: Value = http
            .get(format!("http://{addr}/api/v1/search/posts?q=zebrafinch"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .map(|t| serde_json::from_str(&t).unwrap_or(Value::Null))
            .unwrap();
        body.as_array().is_some_and(|a| !a.is_empty())
    })
    .await;

    let trends = fake.trends_subscription_id();
    let repeated = vec![n1.clone(); 40];
    announce(&fake, &event(&trends, "content", "trending", &repeated)).await;
    let reply = fake.call_ours("GET", "/trends/v0/content", b"").await;
    assert_eq!(reply.status, 200);
    let body = reply.json();
    running.stop().await;
    let rank = body["content"][0]["rank"].as_u64();
    // One event ranks 21 (`trends::rank(1)`).
    assert_eq!(
        rank,
        Some(21),
        "one announcement listing a URI 40 times gave {body}"
    );
}

/// C3: announcements are admitted past the queue cap. When everything queued is itself an
/// announcement there is nothing lower to evict, so the queue grows without bound: a
/// registered server posting announcements of fresh URIs fills the disk.
#[test]
#[ignore = "AUDIT: C3 - fails until fixed"]
fn announcements_do_not_grow_the_queue_past_its_cap() {
    let store = Store::open_in_memory().unwrap();
    let cap = 10;
    for i in 0..(cap * 10) {
        let item = NewFetch {
            uri: format!("https://m.example/notes/{i}"),
            kind: FetchKind::Object,
            reason: FetchReason::Announce,
            record: None,
            not_before: 0,
        };
        store.enqueue(&item, 0, cap).unwrap();
    }
    let len = store.queue_len().unwrap();
    assert!(len <= cap, "queue cap {cap}, but {len} rows are queued");
}

/// Holds: `/metrics` carries no host, URI, registration id, serverId or token. Checked with a
/// registered server whose host appears nowhere else in the output.
#[tokio::test]
async fn metrics_name_no_host_registration_or_token() {
    let fake = FakeMastodon::start().await;
    let admin = "admin-token-0123456789".to_string();
    let crawl = "crawl-token-0123456789".to_string();
    let running = serve_registered(&fake, &["data_sharing"], |c| {
        c.metrics = true;
        c.admin_tokens = mwmbl_social::config::Tokens(vec![admin.clone()]);
        c.crawl_tokens = mwmbl_social::config::Tokens(vec![crawl.clone()]);
    })
    .await;
    let lifecycle = fake.subscription_id("content");
    announce(
        &fake,
        &event(&lifecycle, "content", "new", &[fake.url("/notes/n1")]),
    )
    .await;
    let addr = running.addr;
    let text = reqwest::get(format!("http://{addr}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let server_id = fake.registered()["serverId"].as_str().unwrap().to_owned();
    let fake_port = fake.origin.rsplit(':').next().unwrap().to_owned();
    running.stop().await;
    assert!(
        text.contains("mwmbl_social_registrations{status=\"pending\"}")
            || text.contains("mwmbl_social_registrations{status=\"active\"}")
    );
    for secret in [
        server_id.as_str(),
        admin.as_str(),
        crawl.as_str(),
        "127.0.0.1",
        fake_port.as_str(),
        "notes",
        "dfkl3msw6ps3",
    ] {
        assert!(!text.contains(secret), "/metrics contains {secret:?}");
    }
}

/// Holds: without `metrics`, `/metrics` is not served; and the admin and crawl APIs refuse a
/// token given anywhere but the `Authorization` header, and one token's value for the other.
#[tokio::test]
async fn operator_apis_take_only_their_own_bearer_token() {
    let fake = FakeMastodon::start().await;
    let admin = "admin-token-0123456789".to_string();
    let crawl = "crawl-token-0123456789".to_string();
    let running = serve_registered(&fake, &[], |c| {
        c.admin_tokens = mwmbl_social::config::Tokens(vec![admin.clone()]);
        c.crawl_tokens = mwmbl_social::config::Tokens(vec![crawl.clone()]);
    })
    .await;
    let base = format!("http://{}", running.addr);
    let http = reqwest::Client::new();
    let status = |r: reqwest::Response| r.status().as_u16();
    assert_eq!(
        status(http.get(format!("{base}/metrics")).send().await.unwrap()),
        404
    );
    let admin_path = format!("{base}/api/v1/admin/registrations");
    assert_eq!(
        status(
            http.get(&admin_path)
                .bearer_auth(&admin)
                .send()
                .await
                .unwrap()
        ),
        200
    );
    for request in [
        http.get(format!("{admin_path}?access_token={admin}")),
        http.get(&admin_path).bearer_auth(&crawl),
        http.get(&admin_path).basic_auth(&admin, Some(&admin)),
        http.get(&admin_path)
            .header("authorization", admin.as_str()),
        http.get(&admin_path).bearer_auth(format!("{admin}x")),
        http.get(&admin_path)
            .bearer_auth(admin.get(..admin.len() - 1).unwrap()),
    ] {
        let reply = request.send().await.unwrap();
        assert_eq!(reply.status().as_u16(), 401);
        let body = reply.text().await.unwrap();
        assert!(!body.contains(&admin) && !body.contains(&crawl), "{body}");
    }
    let crawl_path = format!("{base}/api/v1/crawl/submissions");
    let reply = http
        .post(&crawl_path)
        .bearer_auth(&admin)
        .header("content-type", "application/json")
        .body(json!({"uris": []}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status().as_u16(), 401);
    // Object JSON is never accepted in a submission, only URIs.
    let reply = http
        .post(&crawl_path)
        .bearer_auth(&crawl)
        .header("content-type", "application/json")
        .body(json!({"uris": [], "objects": [{"id": "https://l.example/post/1"}]}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status().as_u16(), 422);
    let reply = http
        .post(&crawl_path)
        .bearer_auth(&crawl)
        .header("content-type", "application/json")
        .body(json!({"uris": [{"id": "https://l.example/post/1", "type": "Note"}]}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status().as_u16(), 422);
    let _ = running.config;
    running.stop().await;
}
