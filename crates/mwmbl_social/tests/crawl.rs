//! Crawl submissions (PLAN.md Stage 3 item 3): `POST /api/v1/crawl/submissions` queues URIs,
//! and the worker fetches only those on a Lemmy host whose robots.txt allows them. Against
//! in-process fake hosts; no network beyond 127.0.0.1.

mod common;

use common::{FakeHost, PUBLIC, TestEnv};
use mwmbl_social::config::{Config, CrawlTokens};
use mwmbl_social::crawl_app::CrawlApp;
use serde_json::{Value, json};
use store::FetchReason;

const TOKEN: &str = "crawl-token-0123456789";

/// lemmy-ui's robots.txt, with `/post/2` disallowed as well.
const LEMMY_ROBOTS: &str = "User-Agent: *
Disallow: /login
Disallow: /settings
Disallow: /search
Disallow: /modlog
Disallow: /api
Disallow: /post/2$
Crawl-delay: 60
";

fn serve_nodeinfo(host: &FakeHost, software: &str) {
    host.set_doc(
        "/.well-known/nodeinfo",
        json!({"links": [{
            "rel": "http://nodeinfo.diaspora.software/ns/schema/2.1",
            "href": host.url("/nodeinfo/2.1"),
        }]}),
    );
    host.set_doc(
        "/nodeinfo/2.1",
        json!({"version": "2.1", "software": {"name": software, "version": "1.0"}}),
    );
}

/// A person with neither `discoverable` nor `indexable`, as Lemmy and GoToSocial serve them.
fn serve_flagless_person(host: &FakeHost, path: &str, name: &str) -> String {
    let id = host.url(path);
    host.set_doc(
        path,
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": id,
            "type": "Person",
            "preferredUsername": name,
            "name": name,
            "summary": "<p>Rust and gardening.</p>",
            "inbox": format!("{id}/inbox"),
            "outbox": format!("{id}/outbox"),
        }),
    );
    id
}

fn serve_page(host: &FakeHost, path: &str, author: &str, title: &str) {
    host.set_doc(
        path,
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": host.url(path),
            "type": "Page",
            "attributedTo": author,
            "to": [host.url("/c/rust"), PUBLIC],
            "name": title,
            "content": "<p>Some notes on the subject.</p>",
            "published": "2026-11-10T12:00:00Z",
        }),
    );
}

async fn start_endpoint(env: &TestEnv, tokens: &[&str]) -> String {
    let config = Config {
        crawl_tokens: CrawlTokens(tokens.iter().map(|t| (*t).to_owned()).collect()),
        ..env.config.clone()
    };
    let app = CrawlApp::new(
        store::Store::open(&config.store_path()).unwrap(),
        env.handle.tombstones(),
        &config,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/api/v1/crawl/submissions",
        listener.local_addr().unwrap()
    );
    let routes = app.routes();
    tokio::spawn(async move { axum::serve(listener, routes).await });
    url
}

async fn submit(url: &str, token: Option<&str>, body: &Value) -> (u16, Value) {
    let mut request = reqwest::Client::new()
        .post(url)
        .header("content-type", "application/json")
        .body(body.to_string());
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

#[tokio::test]
async fn only_lemmy_uris_that_robots_txt_allows_are_indexed() {
    let mut env = TestEnv::new().await;
    // env.fake plays the Lemmy host.
    let lemmy = &env.fake;
    serve_nodeinfo(lemmy, "lemmy");
    lemmy.set_text("/robots.txt", LEMMY_ROBOTS);
    let carol = serve_flagless_person(lemmy, "/u/carol", "carol");
    serve_page(
        lemmy,
        "/post/1",
        &carol,
        "Borrow checker tips for beginners",
    );
    serve_page(
        lemmy,
        "/post/2",
        &carol,
        "Lifetime elision explained gently",
    );

    // A reply in the Lemmy thread by a GoToSocial user, who has no consent flags.
    let gts = FakeHost::start().await;
    serve_nodeinfo(&gts, "gotosocial");
    let dave = serve_flagless_person(&gts, "/users/dave", "dave");
    lemmy.set_doc(
        "/comment/3",
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": lemmy.url("/comment/3"),
            "type": "Note",
            "attributedTo": dave,
            "inReplyTo": lemmy.url("/post/1"),
            "to": [PUBLIC],
            "content": "<p>Quokka-flavoured borrow checker anecdote.</p>",
            "published": "2026-11-10T13:00:00Z",
        }),
    );

    // A Mastodon host: alice is indexable, but Mastodon is not crawled.
    let mastodon = FakeHost::start().await;
    serve_nodeinfo(&mastodon, "mastodon");

    let url = start_endpoint(&env, &[TOKEN]).await;
    let uris = [
        lemmy.url("/post/1"),
        lemmy.url("/post/2"),
        lemmy.url("/post/1"),
        lemmy.url("/comment/3"),
        mastodon.url("/notes/public"),
        "ftp://l.example/post/1".to_owned(),
    ];
    let (status, counts) = submit(&url, Some(TOKEN), &json!({ "uris": uris })).await;
    assert_eq!(status, 202);
    assert_eq!(counts, json!({"accepted": 4, "deferred": 0, "rejected": 1}));
    let queued = env.store.next_due(i64::MAX, 100).unwrap();
    assert_eq!(queued.len(), 4);
    assert!(queued.iter().all(|q| q.reason == FetchReason::Crawl));

    env.worker.drain(&mut env.store).await.unwrap();
    assert_eq!(env.store.queue_len().unwrap(), 0);

    let found: Vec<String> = env
        .post_pages()
        .into_iter()
        .map(|p| p.uri)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(found, [env.fake.url("/post/1")]);
    let hits = env.search_posts("borrow");
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].title.as_deref(),
        Some("Borrow checker tips for beginners")
    );
    assert!(env.search_posts("lifetime").is_empty());
    assert!(env.search_posts("quokka").is_empty());

    // robots.txt was read once and the disallowed post never fetched; nothing at all was
    // fetched from the Mastodon host but its nodeinfo.
    assert_eq!(env.fake.hits("/robots.txt"), 1);
    assert_eq!(env.fake.hits("/post/2"), 0);
    assert_eq!(mastodon.hits("/notes/public"), 0);
    assert_eq!(mastodon.hits("/robots.txt"), 0);
    assert_eq!(
        gts.hits("/users/dave"),
        0,
        "an author off the object's origin"
    );

    let counters = env.worker.counters();
    assert_eq!(
        counters.crawl_skips(),
        [
            ("not_lemmy", 1),
            ("robots_disallowed", 1),
            ("robots_unavailable", 0)
        ]
    );
    // The verdict is cached in `hosts`, reduced to the rules for us.
    let host = env.fake.origin.trim_start_matches("http://").to_owned();
    let stored = env.store.host_robots(&host).unwrap().unwrap();
    assert!(
        stored
            .robots_txt
            .is_some_and(|t| t.contains("Disallow: /post/2$") && t.contains("Crawl-delay: 60"))
    );
}

#[tokio::test]
async fn an_unavailable_robots_txt_puts_crawling_off_and_a_missing_one_allows_it() {
    let mut env = TestEnv::new().await;
    serve_nodeinfo(&env.fake, "lemmy");
    let carol = serve_flagless_person(&env.fake, "/u/carol", "carol");
    serve_page(&env.fake, "/post/1", &carol, "Borrow checker tips");
    env.fake
        .set_text("/robots.txt", "User-agent: *\nAllow: /\n");
    env.fake.set_failing("/robots.txt");
    let url = start_endpoint(&env, &[TOKEN]).await;
    let (status, _) = submit(
        &url,
        Some(TOKEN),
        &json!({"uris": [env.fake.url("/post/1")]}),
    )
    .await;
    assert_eq!(status, 202);
    env.worker.drain(&mut env.store).await.unwrap();
    // Rescheduled for an hour on, not dropped, and nothing fetched meanwhile.
    let queued = env.store.next_due(i64::MAX, 10).unwrap();
    assert_eq!(queued.len(), 1);
    assert!(queued[0].not_before >= mwmbl_social::now_secs() + 3500);
    assert_eq!(
        queued[0].last_error.as_deref(),
        Some("robots.txt unavailable")
    );
    assert_eq!(env.fake.hits("/post/1"), 0);

    // No robots.txt at all (404) allows everything.
    let mut env = TestEnv::new().await;
    serve_nodeinfo(&env.fake, "lemmy");
    let carol = serve_flagless_person(&env.fake, "/u/carol", "carol");
    serve_page(&env.fake, "/post/1", &carol, "Borrow checker tips");
    let url = start_endpoint(&env, &[TOKEN]).await;
    submit(
        &url,
        Some(TOKEN),
        &json!({"uris": [env.fake.url("/post/1")]}),
    )
    .await;
    env.worker.drain(&mut env.store).await.unwrap();
    assert_eq!(env.search_posts("borrow").len(), 1);
}

#[tokio::test]
async fn the_endpoint_needs_a_token_and_takes_only_uris() {
    let env = TestEnv::new().await;
    let off = start_endpoint(&env, &[]).await;
    let uris = json!({"uris": [env.fake.url("/post/1")]});
    assert_eq!(submit(&off, Some(TOKEN), &uris).await.0, 404);

    let url = start_endpoint(&env, &[TOKEN, "another-token-abcdefgh"]).await;
    assert_eq!(submit(&url, None, &uris).await.0, 401);
    assert_eq!(
        submit(&url, Some("wrong-token-0123456789"), &uris).await.0,
        401
    );
    assert_eq!(
        submit(&url, Some("another-token-abcdefgh"), &uris).await.0,
        202
    );

    // An object, not URIs.
    let note = json!({
        "id": env.fake.url("/notes/forged"),
        "type": "Note",
        "attributedTo": env.fake.url("/users/alice"),
        "to": [PUBLIC],
        "content": "<p>forged</p>",
    });
    assert_eq!(submit(&url, Some(TOKEN), &note).await.0, 422);
    assert_eq!(
        submit(&url, Some(TOKEN), &json!({"uris": [note]})).await.0,
        422
    );
    assert_eq!(
        submit(&url, Some(TOKEN), &json!({"uris": [], "objects": []}))
            .await
            .0,
        422
    );
    assert_eq!(submit(&url, Some(TOKEN), &json!([])).await.0, 422);
    let too_many: Vec<String> = (0..1001)
        .map(|i| env.fake.url(&format!("/post/{i}")))
        .collect();
    assert_eq!(
        submit(&url, Some(TOKEN), &json!({ "uris": too_many }))
            .await
            .0,
        422
    );
    let (status, counts) = submit(&url, Some(TOKEN), &json!({"uris": []})).await;
    assert_eq!(status, 202);
    assert_eq!(counts, json!({"accepted": 0, "deferred": 0, "rejected": 0}));
}
