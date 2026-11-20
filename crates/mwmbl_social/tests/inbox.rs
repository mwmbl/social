//! The inbox and follow opt-in end to end (PLAN.md Stage 3 items 1 and 7): our full server
//! (`serve_on`) on a random local port, against fake fediverse hosts that sign their deliveries
//! and verify ours with our actor's key. Nothing leaves 127.0.0.1.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use apub::fetch::SignatureScheme;
use apub::inbox::{signer, verify_post};
use apub::signatures::integrity::legacy_digest;
use apub::signatures::keys::{RsaSigningKey, RsaVerifyingKey, SigningKey};
use apub::signatures::{authority, cavage, content_digest, http_date, rfc9421};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::Response;
use common::PUBLIC;
use mwmbl_social::cli::{follow, following, serve_on, unfollow};
use mwmbl_social::config::Config;
use mwmbl_social::now_secs;
use serde_json::{Value, json};
use store::{FollowState, Store, TombstoneKind};
use tokio::sync::watch;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/../apub/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// Bob's key, and the Lemmy community's: generated once per test run.
const KEY_A: &str = "generated";
/// Eve's key, and Carol's.
const KEY_B: &str = "cavage12_private.pem";

fn signing_key(name: &str) -> RsaSigningKey {
    static GENERATED: OnceLock<String> = OnceLock::new();
    let pem = if name == KEY_A {
        GENERATED
            .get_or_init(|| RsaSigningKey::generate().unwrap().to_pkcs8_pem().unwrap())
            .clone()
    } else {
        fixture(name)
    };
    RsaSigningKey::from_pem(&pem).unwrap()
}

fn public_pem(name: &str) -> String {
    signing_key(name).verifying_key().to_spki_pem().unwrap()
}

async fn wait_until(what: &str, mut check: impl AsyncFnMut() -> bool) {
    for _ in 0..300 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

// ---------------------------------------------------------------------------------------------
// A fake fediverse host

#[derive(Default)]
struct HostState {
    docs: Mutex<HashMap<String, Value>>,
    /// Our actor's public key, to check what we deliver.
    our_key: Mutex<Option<RsaVerifyingKey>>,
    /// Deliveries to any `.../inbox` that verified: (path, activity).
    delivered: Mutex<Vec<(String, Value)>>,
    /// Deliveries that did not verify.
    unverified: Mutex<usize>,
    /// GETs per path.
    hits: Mutex<HashMap<String, usize>>,
}

struct FakeHost {
    origin: String,
    state: Arc<HostState>,
}

async fn serve_host(State(state): State<Arc<HostState>>, request: Request) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path().to_owned();
    if parts.method == "POST" {
        let headers: Vec<(String, String)> = parts
            .headers
            .iter()
            .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap().to_owned()))
            .collect();
        let host = parts.headers["host"].to_str().unwrap().to_owned();
        let target = format!("http://{host}{}", parts.uri);
        let body = to_bytes(body, 1 << 20).await.unwrap();
        let key = state.our_key.lock().unwrap().clone().unwrap();
        let verified = signer(&headers)
            .is_ok_and(|by| verify_post(&by, &target, &headers, &body, &key, now_secs()).is_ok());
        if verified {
            let activity: Value = serde_json::from_slice(&body).unwrap();
            state.delivered.lock().unwrap().push((path, activity));
            return Response::builder().status(202).body(Body::empty()).unwrap();
        }
        *state.unverified.lock().unwrap() += 1;
        return Response::builder().status(401).body(Body::empty()).unwrap();
    }
    *state.hits.lock().unwrap().entry(path.clone()).or_default() += 1;
    match state.docs.lock().unwrap().get(&path) {
        Some(doc) => Response::builder()
            .status(200)
            .header("content-type", "application/activity+json")
            .body(Body::from(doc.to_string()))
            .unwrap(),
        None => Response::builder().status(404).body(Body::empty()).unwrap(),
    }
}

impl FakeHost {
    /// `host:port`, for handles.
    fn authority(&self) -> &str {
        self.origin.trim_start_matches("http://")
    }

    /// Answers `WebFinger` (for any resource) with `actor` as the `self` link.
    fn webfinger(&self, name: &str, actor: &str) {
        self.set(
            "/.well-known/webfinger",
            json!({"subject": format!("acct:{name}@{}", self.authority()), "links": [
                {"rel": "self", "type": "application/activity+json", "href": actor},
            ]}),
        );
    }

    async fn start(software: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(HostState::default());
        let host = Self {
            origin: origin.clone(),
            state: Arc::clone(&state),
        };
        host.set(
            "/.well-known/nodeinfo",
            json!({"links": [{
                "rel": "http://nodeinfo.diaspora.software/ns/schema/2.1",
                "href": format!("{origin}/nodeinfo/2.1"),
            }]}),
        );
        host.set(
            "/nodeinfo/2.1",
            json!({"version": "2.1", "software": {"name": software, "version": "1.0"}}),
        );
        let app = Router::new().fallback(serve_host).with_state(state);
        tokio::spawn(async move { axum::serve(listener, app).await });
        host
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }

    fn set(&self, path: &str, doc: Value) {
        self.state.docs.lock().unwrap().insert(path.to_owned(), doc);
    }

    /// An actor at `path`, of `kind`, with `extra` fields, its key from fixture `key`.
    fn actor(&self, path: &str, kind: &str, key: &str, extra: &Value) -> String {
        let id = self.url(path);
        let mut doc = json!({
            "@context": ["https://www.w3.org/ns/activitystreams", "https://w3id.org/security/v1"],
            "id": id,
            "type": kind,
            "preferredUsername": path.rsplit('/').next().unwrap(),
            "name": "Someone",
            "inbox": format!("{id}/inbox"),
            "outbox": format!("{id}/outbox"),
            "publicKey": {"id": format!("{id}#main-key"), "owner": id, "publicKeyPem": public_pem(key)},
        });
        for (k, v) in extra.as_object().unwrap() {
            doc[k] = v.clone();
        }
        self.set(path, doc);
        id
    }

    fn hits(&self, path: &str) -> usize {
        self.state
            .hits
            .lock()
            .unwrap()
            .get(path)
            .copied()
            .unwrap_or(0)
    }

    /// The verified deliveries to `actor`'s inbox, by type.
    fn delivered_to(&self, actor_path: &str, kind: &str) -> Vec<Value> {
        let inbox = format!("{actor_path}/inbox");
        self.state
            .delivered
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, a)| *path == inbox && a["type"] == kind)
            .map(|(_, a)| a.clone())
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Our server, and signed deliveries to it

struct Ours {
    _dir: tempfile::TempDir,
    config: Config,
    base: String,
    http: reqwest::Client,
    _shutdown: watch::Sender<bool>,
}

impl Ours {
    async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    async fn start_with(configure: impl FnOnce(&mut Config)) -> Self {
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
        configure(&mut config);
        let (shutdown, shutdown_rx) = watch::channel(false);
        tokio::spawn(serve_on(config.clone(), listener, shutdown_rx));
        let ours = Self {
            _dir: dir,
            config,
            base: format!("http://{addr}"),
            http: reqwest::Client::new(),
            _shutdown: shutdown,
        };
        wait_until("our server to answer", async || {
            ours.http
                .get(format!("{}/actor", ours.base))
                .send()
                .await
                .is_ok_and(|r| r.status() == 200)
        })
        .await;
        ours
    }

    fn actor(&self) -> String {
        format!("{}/actor", self.base)
    }

    fn store(&self) -> Store {
        Store::open(&self.config.store_path()).unwrap()
    }

    /// Lets `host` check our deliveries with our actor's published key.
    async fn introduce(&self, host: &FakeHost) {
        let actor: Value = self
            .http
            .get(self.actor())
            .send()
            .await
            .unwrap()
            .text()
            .await
            .map(|t| serde_json::from_str(&t).unwrap())
            .unwrap();
        let pem = actor["publicKey"]["publicKeyPem"].as_str().unwrap();
        *host.state.our_key.lock().unwrap() = Some(RsaVerifyingKey::from_pem(pem).unwrap());
    }

    async fn search(&self, q: &str) -> Vec<String> {
        let results: Vec<Value> = self
            .http
            .get(format!("{}/api/v1/search/posts?q={q}", self.base))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .map(|t| serde_json::from_str(&t).unwrap())
            .unwrap();
        results
            .iter()
            .map(|r| r["uri"].as_str().unwrap().to_owned())
            .collect()
    }

    /// POSTs `body` to `path`, signed with `key` under `key_id`.
    async fn post_signed(
        &self,
        path: &str,
        body: &[u8],
        key_id: &str,
        key: &str,
        scheme: SignatureScheme,
    ) -> u16 {
        let url = format!("{}{path}", self.base);
        let now = now_secs();
        let mut headers = vec![
            ("host".to_owned(), authority(&url).unwrap()),
            ("date".to_owned(), http_date(now)),
            (
                "content-type".to_owned(),
                "application/activity+json".to_owned(),
            ),
            ("content-digest".to_owned(), content_digest(body)),
            ("digest".to_owned(), legacy_digest(body)),
        ];
        match scheme {
            SignatureScheme::Rfc9421 => {
                let (input, signature) = rfc9421::sign_request(
                    "POST",
                    &url,
                    &headers,
                    &["@method", "@target-uri", "content-digest"],
                    key_id,
                    now,
                    &SigningKey::RsaV15Sha256(signing_key(key)),
                )
                .unwrap();
                headers.push(("signature-input".to_owned(), input));
                headers.push(("signature".to_owned(), signature));
            }
            SignatureScheme::Cavage => {
                let signature = cavage::sign_request(
                    "POST",
                    &url,
                    &headers,
                    cavage::POST_HEADERS,
                    key_id,
                    &cavage::SignOptions::default(),
                    &signing_key(key),
                )
                .unwrap();
                headers.push(("signature".to_owned(), signature));
            }
        }
        let mut request = self.http.post(&url).body(body.to_vec());
        for (name, value) in headers.iter().filter(|(n, _)| n != "host") {
            request = request.header(name, value);
        }
        request.send().await.unwrap().status().as_u16()
    }

    /// Delivers `activity` to our shared inbox, signed by `actor_id`'s key `key`.
    async fn deliver(
        &self,
        activity: &Value,
        actor_id: &str,
        key: &str,
        scheme: SignatureScheme,
    ) -> u16 {
        let body = serde_json::to_vec(activity).unwrap();
        self.post_signed(
            "/inbox",
            &body,
            &format!("{actor_id}#main-key"),
            key,
            scheme,
        )
        .await
    }

    async fn queue_is_empty(&self) -> bool {
        self.store().queue_len().unwrap() == 0
    }
}

fn note(id: &str, author: &str, content: &str) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": id,
        "type": "Note",
        "attributedTo": author,
        "to": [PUBLIC],
        "cc": [format!("{author}/followers")],
        "published": "2026-11-10T12:00:00Z",
        "content": format!("<p>{content}</p>"),
    })
}

// ---------------------------------------------------------------------------------------------

#[tokio::test]
#[allow(clippy::too_many_lines)] // One scenario, read top to bottom.
async fn a_flagless_follower_is_indexed_until_it_unfollows() {
    let ours = Ours::start().await;
    let gts = FakeHost::start("gotosocial").await;
    ours.introduce(&gts).await;
    // GoToSocial-like: no `discoverable` or `indexable` at all.
    let bob = gts.actor("/users/bob", "Person", KEY_A, &json!({}));
    let post = gts.url("/users/bob/statuses/1");
    gts.set(
        "/users/bob/statuses/1",
        note(&post, &bob, "Planting kohlrabi beside the greenhouse"),
    );

    // Bob follows us: an opt-in, an Accept, and a follow back, all checked by the fake.
    let follow_id = gts.url("/users/bob/follows/1");
    let follow = json!({"@context": "https://www.w3.org/ns/activitystreams",
        "id": follow_id, "type": "Follow", "actor": bob, "object": ours.actor()});
    assert_eq!(
        ours.deliver(&follow, &bob, KEY_A, SignatureScheme::Rfc9421)
            .await,
        202
    );
    wait_until("the Accept and our Follow", async || {
        !gts.delivered_to("/users/bob", "Accept").is_empty()
            && !gts.delivered_to("/users/bob", "Follow").is_empty()
    })
    .await;
    assert!(ours.store().is_follow_optin(&bob).unwrap());
    let accept = &gts.delivered_to("/users/bob", "Accept")[0];
    assert_eq!(accept["actor"], ours.actor());
    assert_eq!(accept["object"]["id"], follow_id);
    let our_follow = gts.delivered_to("/users/bob", "Follow")[0].clone();
    assert_eq!(our_follow["actor"], ours.actor());
    assert_eq!(our_follow["object"], bob);
    let row = ours.store().following(&bob).unwrap().unwrap();
    assert_eq!(row.state, FollowState::Pending);
    assert_eq!(row.follow_id, our_follow["id"].as_str().unwrap());
    assert_eq!(*gts.state.unverified.lock().unwrap(), 0);

    // Bob accepts our follow.
    let accept_ours = json!({"id": gts.url("/users/bob/accepts/1"), "type": "Accept",
        "actor": bob, "object": our_follow});
    assert_eq!(
        ours.deliver(&accept_ours, &bob, KEY_A, SignatureScheme::Cavage)
            .await,
        202
    );
    wait_until("our follow to be accepted", async || {
        ours.store().following(&bob).unwrap().unwrap().state == FollowState::Accepted
    })
    .await;

    // His public post arrives as a Create. What is indexed is what his server serves, not the
    // text in the delivery.
    let create = json!({"id": gts.url("/users/bob/statuses/1/activity"), "type": "Create",
        "actor": bob, "object": note(&post, &bob, "Forged quetzal words")});
    assert_eq!(
        ours.deliver(&create, &bob, KEY_A, SignatureScheme::Rfc9421)
            .await,
        202
    );
    wait_until("the post to be indexed", async || {
        ours.search("kohlrabi").await == [post.clone()]
    })
    .await;
    assert!(ours.search("quetzal").await.is_empty());

    // Bob unfollows: the opt-in goes, his post disappears, and we unfollow him back.
    let undo = json!({"id": gts.url("/users/bob/follows/1/undo"), "type": "Undo",
        "actor": bob, "object": follow});
    assert_eq!(
        ours.deliver(&undo, &bob, KEY_A, SignatureScheme::Cavage)
            .await,
        202
    );
    wait_until("the post to be removed", async || {
        ours.search("kohlrabi").await.is_empty()
    })
    .await;
    wait_until("our Undo", async || {
        !gts.delivered_to("/users/bob", "Undo").is_empty()
    })
    .await;
    let our_undo = &gts.delivered_to("/users/bob", "Undo")[0];
    assert_eq!(our_undo["object"]["id"], our_follow["id"]);
    assert!(!ours.store().is_follow_optin(&bob).unwrap());
    assert_eq!(ours.store().following(&bob).unwrap(), None);
    let tombstones = ours.store().list_tombstones().unwrap();
    assert!(
        tombstones
            .iter()
            .any(|t| t.uri == bob && t.kind == TombstoneKind::Author),
        "{tombstones:?}"
    );
    // The account fetched again after the unfollow is not consented either: it stays out.
    wait_until("the queue to drain", async || ours.queue_is_empty().await).await;
    assert!(ours.search("kohlrabi").await.is_empty());
}

#[tokio::test]
async fn a_follower_with_indexable_false_stays_out() {
    let ours = Ours::start().await;
    let gts = FakeHost::start("gotosocial").await;
    ours.introduce(&gts).await;
    let eve = gts.actor(
        "/users/eve",
        "Person",
        KEY_B,
        &json!({"discoverable": true, "indexable": false}),
    );
    let post = gts.url("/users/eve/statuses/1");
    gts.set(
        "/users/eve/statuses/1",
        note(&post, &eve, "Eve grows saffron crocuses"),
    );
    let follow = json!({"id": gts.url("/users/eve/follows/1"), "type": "Follow",
        "actor": eve, "object": ours.actor()});
    assert_eq!(
        ours.deliver(&follow, &eve, KEY_B, SignatureScheme::Cavage)
            .await,
        202
    );
    wait_until("the follow to be recorded", async || {
        ours.store().following(&eve).unwrap().is_some()
    })
    .await;
    assert!(ours.store().is_follow_optin(&eve).unwrap());
    let create = json!({"type": "Create", "actor": eve, "object": post});
    assert_eq!(
        ours.deliver(&create, &eve, KEY_B, SignatureScheme::Cavage)
            .await,
        202
    );
    // The post is fetched from origin and judged there: not indexable.
    wait_until("the post to be fetched and judged", async || {
        gts.hits("/users/eve/statuses/1") > 0 && ours.queue_is_empty().await
    })
    .await;
    assert!(ours.search("saffron").await.is_empty());
}

#[tokio::test]
async fn forged_deliveries_get_401_and_change_nothing() {
    let ours = Ours::start().await;
    let gts = FakeHost::start("gotosocial").await;
    ours.introduce(&gts).await;
    let bob = gts.actor("/users/bob", "Person", KEY_A, &json!({}));
    let eve = gts.actor("/users/eve", "Person", KEY_B, &json!({}));
    let follow = json!({"id": gts.url("/users/bob/follows/1"), "type": "Follow",
        "actor": bob, "object": ours.actor()});
    let body = serde_json::to_vec(&follow).unwrap();

    for scheme in [SignatureScheme::Rfc9421, SignatureScheme::Cavage] {
        // Bob's key id, but signed with Eve's key.
        let bad = ours
            .post_signed("/inbox", &body, &format!("{bob}#main-key"), KEY_B, scheme)
            .await;
        assert_eq!(bad, 401, "{scheme:?}: bad signature");
        // Signed properly by Eve, claiming to be Bob.
        let impostor = ours
            .post_signed(
                "/actor/inbox",
                &body,
                &format!("{eve}#main-key"),
                KEY_B,
                scheme,
            )
            .await;
        assert_eq!(impostor, 401, "{scheme:?}: signed by another actor");
    }
    // A key on another origin is not even fetched.
    let other = FakeHost::start("gotosocial").await;
    let mallory = other.actor("/users/mallory", "Person", KEY_A, &json!({}));
    let foreign = ours
        .post_signed(
            "/inbox",
            &body,
            &format!("{mallory}#main-key"),
            KEY_A,
            SignatureScheme::Cavage,
        )
        .await;
    assert_eq!(foreign, 401);
    // Unsigned, the wrong media type, and not JSON.
    let unsigned = ours
        .http
        .post(format!("{}/inbox", ours.base))
        .header("content-type", "application/activity+json")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(unsigned.status(), 401);
    let html = ours
        .http
        .post(format!("{}/inbox", ours.base))
        .header("content-type", "text/html")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(html.status(), 415);
    let garbage = ours
        .http
        .post(format!("{}/inbox", ours.base))
        .header("content-type", "application/activity+json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(garbage.status(), 400);
    let huge = ours
        .http
        .post(format!("{}/inbox", ours.base))
        .header("content-type", "application/activity+json")
        .body(vec![b' '; 300 * 1024])
        .send()
        .await
        .unwrap();
    assert_eq!(huge.status(), 413);

    tokio::time::sleep(Duration::from_millis(200)).await;
    let store = ours.store();
    assert!(store.follow_optins().unwrap().is_empty());
    assert_eq!(store.following(&bob).unwrap(), None);
    assert_eq!(store.queue_len().unwrap(), 0);
    assert!(gts.state.delivered.lock().unwrap().is_empty());
    // A properly signed delivery still works afterwards.
    assert_eq!(
        ours.deliver(&follow, &bob, KEY_A, SignatureScheme::Cavage)
            .await,
        202
    );
    wait_until("the follow", async || {
        ours.store().is_follow_optin(&bob).unwrap()
    })
    .await;
}

#[tokio::test]
async fn a_followed_lemmy_community_relays_posts_and_removals() {
    let ours = Ours::start().await;
    let lemmy = FakeHost::start("lemmy").await;
    ours.introduce(&lemmy).await;
    let community = lemmy.actor("/c/garden", "Group", KEY_A, &json!({}));
    let carol = lemmy.actor("/u/carol", "Person", KEY_B, &json!({}));
    let page = lemmy.url("/post/7");
    lemmy.set(
        "/post/7",
        json!({
            "@context": ["https://join-lemmy.org/context.json", "https://www.w3.org/ns/activitystreams"],
            "id": page,
            "type": "Page",
            "attributedTo": carol,
            "to": [community, PUBLIC],
            "cc": [],
            "audience": community,
            "name": "Companion planting with marigolds",
            "content": "<p>Marigolds keep the aphids away.</p>",
            "published": "2026-11-10T12:00:00Z",
        }),
    );
    // An admin follows the community, and it accepts.
    let followed = follow(&ours.config, &community).await.unwrap();
    assert_eq!(followed["delivered"], true);
    let our_follow = lemmy.delivered_to("/c/garden", "Follow")[0].clone();
    let accept = json!({"type": "Accept", "actor": community, "object": our_follow});
    assert_eq!(
        ours.deliver(&accept, &community, KEY_A, SignatureScheme::Cavage)
            .await,
        202
    );
    wait_until("our follow to be accepted", async || {
        ours.store().following(&community).unwrap().unwrap().state == FollowState::Accepted
    })
    .await;

    let announce = |inner: Value| json!({"type": "Announce", "actor": community, "object": inner});
    let create = json!({"type": "Create", "actor": carol, "to": [PUBLIC],
        "object": {"id": page, "type": "Page", "name": "Not what is indexed"}});
    assert_eq!(
        ours.deliver(
            &announce(create),
            &community,
            KEY_A,
            SignatureScheme::Cavage
        )
        .await,
        202
    );
    wait_until("the page to be indexed", async || {
        ours.search("marigolds").await == [page.clone()]
    })
    .await;

    // A moderator removes it.
    let remove = json!({"type": "Remove", "actor": lemmy.url("/u/mod"), "object": page,
        "target": community});
    assert_eq!(
        ours.deliver(
            &announce(remove),
            &community,
            KEY_A,
            SignatureScheme::Rfc9421
        )
        .await,
        202
    );
    wait_until("the page to be removed", async || {
        ours.search("marigolds").await.is_empty()
    })
    .await;
    let tombstones = ours.store().list_tombstones().unwrap();
    assert!(
        tombstones
            .iter()
            .any(|t| t.uri == page && t.kind == TombstoneKind::Object),
        "{tombstones:?}"
    );
}

fn lemmy_page(id: &str, author: &str, community: &str, title: &str) -> Value {
    json!({
        "@context": ["https://join-lemmy.org/context.json", "https://www.w3.org/ns/activitystreams"],
        "id": id,
        "type": "Page",
        "attributedTo": author,
        "to": [community, PUBLIC],
        "cc": [],
        "audience": community,
        "name": title,
        "content": "<p>Ownership and borrowing.</p>",
        "published": "2026-11-10T12:00:00Z",
    })
}

#[tokio::test]
async fn an_admin_follow_of_a_lemmy_community_backfills_its_outbox() {
    let ours = Ours::start().await;
    let lemmy = FakeHost::start("lemmy").await;
    let other = FakeHost::start("lemmy").await;
    ours.introduce(&lemmy).await;
    let community = lemmy.actor("/c/rust", "Group", KEY_A, &json!({}));
    lemmy.webfinger("rust", &community);
    // Flagless Lemmy authors, one on another instance.
    let carol = lemmy.actor("/u/carol", "Person", KEY_B, &json!({}));
    let dan = other.actor("/u/dan", "Person", KEY_B, &json!({}));
    let local = lemmy.url("/post/1");
    let remote = other.url("/post/9");
    lemmy.set(
        "/post/1",
        lemmy_page(&local, &carol, &community, "Lifetimes explained"),
    );
    other.set(
        "/post/9",
        lemmy_page(&remote, &dan, &community, "Lifetimes in async code"),
    );
    let boosted = other.url("/post/boosted");
    let announce = |object: &Value| json!({"type": "Announce", "actor": community, "to": [PUBLIC], "object": object});
    lemmy.set(
        "/c/rust/outbox",
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": lemmy.url("/c/rust/outbox"),
            "type": "OrderedCollection",
            "orderedItems": [
                announce(&json!({"type": "Create", "actor": carol,
                    "object": {"id": local, "type": "Page", "name": "Not what is indexed"}})),
                announce(&json!({"type": "Create", "actor": dan,
                    "object": {"id": remote, "type": "Page"}})),
                announce(&json!(boosted)),
            ],
        }),
    );

    // `follow !rust@host`: WebFinger, the actor, and our Follow, which the fake verifies.
    let handle = format!("!rust@{}", lemmy.authority());
    let followed = follow(&ours.config, &handle).await.unwrap();
    assert_eq!(followed["actor"], community);
    assert_eq!(followed["type"], "community");
    assert_eq!(followed["delivered"], true);
    assert!(lemmy.hits("/.well-known/webfinger") > 0);
    let our_follow = lemmy.delivered_to("/c/rust", "Follow")[0].clone();
    assert_eq!(our_follow["actor"], ours.actor());
    assert_eq!(our_follow["object"], community);
    assert_eq!(*lemmy.state.unverified.lock().unwrap(), 0);
    let listed = following(&ours.config).unwrap();
    assert_eq!(listed[0]["actor"], community);
    assert_eq!(listed[0]["state"], "pending");
    // Following again while pending sends the same Follow again (Lemmy 0.19 drops the Accept
    // for the first follow from a new instance).
    let again = follow(&ours.config, &community).await.unwrap();
    assert_eq!(again["state"], "pending");
    assert_eq!(again["delivered"], true);
    let follows = lemmy.delivered_to("/c/rust", "Follow");
    assert_eq!(follows.len(), 2);
    assert_eq!(follows[1]["object"], community);
    assert_ne!(follows[1]["id"], our_follow["id"]);
    let our_follow = follows[1].clone();

    // The community accepts: the server reads its outbox and queues the posts.
    let accept = json!({"id": lemmy.url("/activities/accept/1"), "type": "Accept",
        "actor": community, "object": our_follow});
    assert_eq!(
        ours.deliver(&accept, &community, KEY_A, SignatureScheme::Rfc9421)
            .await,
        202
    );
    wait_until("the outbox's posts to be indexed", async || {
        let mut found = ours.search("lifetimes").await;
        let mut want = vec![local.clone(), remote.clone()];
        found.sort();
        want.sort();
        found == want
    })
    .await;
    assert_eq!(
        ours.store().following(&community).unwrap().unwrap().state,
        FollowState::Accepted
    );
    assert_eq!(lemmy.hits("/c/rust/outbox"), 1);
    // Once accepted, following again changes nothing.
    let again = follow(&ours.config, &community).await.unwrap();
    assert_eq!(again["note"], "already followed");
    assert_eq!(lemmy.delivered_to("/c/rust", "Follow").len(), 2);
    // The boost of a post was not fetched.
    assert_eq!(other.hits("/post/boosted"), 0);

    // `unfollow`: our Undo, and the row goes. The posts stay: their authors' consent stands.
    let unfollowed = unfollow(&ours.config, &handle).await.unwrap();
    assert_eq!(unfollowed["delivered"], true);
    let undo = &lemmy.delivered_to("/c/rust", "Undo")[0];
    assert_eq!(undo["object"]["id"], our_follow["id"]);
    assert_eq!(ours.store().following(&community).unwrap(), None);
    assert_eq!(following(&ours.config).unwrap(), json!([]));
    assert_eq!(ours.search("lifetimes").await.len(), 2);
    assert!(unfollow(&ours.config, &community).await.is_err());
}

#[tokio::test]
async fn configured_follows_are_followed_at_start_up() {
    let lemmy = FakeHost::start("lemmy").await;
    let community = lemmy.actor("/c/tea", "Group", KEY_A, &json!({}));
    lemmy.webfinger("tea", &community);
    // A post, not an actor: not followed.
    lemmy.set("/post/1", note(&lemmy.url("/post/1"), &community, "x"));
    // A follow pending for two days, sent before this server started.
    let stale = lemmy.actor("/c/coffee", "Group", KEY_A, &json!({}));
    let targets = vec![format!("!tea@{}", lemmy.authority()), lemmy.url("/post/1")];
    let ours = Ours::start_with(|c| {
        c.follow = targets;
        // Our key exists before the server starts, so the fake can check its first deliveries.
        let key = mwmbl_social::keys::load_or_generate(&c.keys_dir()).unwrap();
        *lemmy.state.our_key.lock().unwrap() = Some(key.verifying_key());
        let store = Store::open(&c.store_path()).unwrap();
        let inbox = format!("{stale}/inbox");
        store
            .add_following(&stale, &inbox, "old-follow", now_secs() - 2 * 86_400)
            .unwrap();
    })
    .await;
    wait_until("the configured follow", async || {
        ours.store().following(&community).unwrap().is_some()
    })
    .await;
    wait_until("our Follow", async || {
        !lemmy.delivered_to("/c/tea", "Follow").is_empty()
    })
    .await;
    // The stale follow is sent again under a new id (Lemmy rejects an id it has seen), which
    // the row records, and is not re-sent at once.
    wait_until("the re-sent Follow", async || {
        !lemmy.delivered_to("/c/coffee", "Follow").is_empty()
    })
    .await;
    let resent = lemmy.delivered_to("/c/coffee", "Follow")[0].clone();
    assert_ne!(resent["id"], "old-follow");
    assert_eq!(resent["object"], stale);
    let row = ours.store().following(&stale).unwrap().unwrap();
    assert!(row.created_at > now_secs() - 60);
    assert_eq!(resent["id"], row.follow_id.as_str());
    let listed = following(&ours.config).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 2, "{listed}");
}

// ---------------------------------------------------------------------------------------------
// Audit 2026-11-19 (reviewer A): failing tests that demonstrate findings. Each is ignored until
// the finding is fixed; run with `--ignored` to see it fail.

/// Waits a moment for a spawned inbox task to apply its effects.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(700)).await;
}

#[tokio::test]
#[ignore = "AUDIT: A1 - fails until fixed"]
async fn audit_a1_a_cached_key_signs_for_another_actor_on_its_origin() {
    let ours = Ours::start().await;
    let gts = FakeHost::start("gotosocial").await;
    ours.introduce(&gts).await;
    // Bob carries no flags: a follow opt-in is all his posts need to be indexed.
    let bob = gts.actor("/users/bob", "Person", KEY_A, &json!({}));
    let eve = gts.actor("/users/eve", "Person", KEY_B, &json!({}));
    // Eve sends anything properly signed as herself: her key is now cached.
    let hello = json!({"id": gts.url("/users/eve/likes/1"), "type": "Like", "actor": eve,
        "object": gts.url("/users/bob/statuses/1")});
    assert_eq!(
        ours.deliver(&hello, &eve, KEY_B, SignatureScheme::Cavage)
            .await,
        202
    );
    // Eve, with her own key, claims to be Bob following us.
    let forged = json!({"id": gts.url("/users/bob/follows/1"), "type": "Follow",
        "actor": bob, "object": ours.actor()});
    let body = serde_json::to_vec(&forged).unwrap();
    let status = ours
        .post_signed(
            "/inbox",
            &body,
            &format!("{eve}#main-key"),
            KEY_B,
            SignatureScheme::Cavage,
        )
        .await;
    settle().await;
    assert!(
        !ours.store().is_follow_optin(&bob).unwrap(),
        "Eve's key recorded a follow opt-in for Bob (status {status})"
    );
    assert_eq!(status, 401, "Eve's cached key was accepted for Bob");
}

#[tokio::test]
#[ignore = "AUDIT: A2 - fails until fixed"]
async fn audit_a2_a_key_document_the_actor_does_not_list_signs_for_it() {
    let ours = Ours::start().await;
    let gts = FakeHost::start("gotosocial").await;
    ours.introduce(&gts).await;
    let bob = gts.actor("/users/bob", "Person", KEY_A, &json!({}));
    // Any JSON a user can get served from Bob's origin (an upload, say), naming Bob as the owner
    // of a key Eve holds. Bob's actor never lists it.
    let doc = gts.url("/files/upload.json");
    gts.set(
        "/files/upload.json",
        json!({"id": doc, "publicKey": {"id": format!("{doc}#k"), "owner": bob,
            "publicKeyPem": public_pem(KEY_B)}}),
    );
    let forged = json!({"id": gts.url("/users/bob/follows/1"), "type": "Follow",
        "actor": bob, "object": ours.actor()});
    let body = serde_json::to_vec(&forged).unwrap();
    let status = ours
        .post_signed(
            "/inbox",
            &body,
            &format!("{doc}#k"),
            KEY_B,
            SignatureScheme::Rfc9421,
        )
        .await;
    settle().await;
    assert!(
        !ours.store().is_follow_optin(&bob).unwrap(),
        "a key Bob's actor does not list recorded his opt-in (status {status})"
    );
    assert_eq!(status, 401);
}

#[tokio::test]
#[ignore = "AUDIT: A3 - fails until fixed"]
async fn audit_a3_anyone_on_an_origin_can_delete_anothers_post() {
    let ours = Ours::start().await;
    let gts = FakeHost::start("gotosocial").await;
    ours.introduce(&gts).await;
    let bob = gts.actor(
        "/users/bob",
        "Person",
        KEY_A,
        &json!({"discoverable": true, "indexable": true}),
    );
    let eve = gts.actor("/users/eve", "Person", KEY_B, &json!({}));
    let post = gts.url("/users/bob/statuses/1");
    gts.set(
        "/users/bob/statuses/1",
        note(&post, &bob, "Pruning the espaliered pear"),
    );
    // We follow Bob, he accepts, and his post is indexed.
    follow(&ours.config, &bob).await.unwrap();
    let our_follow = gts.delivered_to("/users/bob", "Follow")[0].clone();
    let accept = json!({"type": "Accept", "actor": bob, "object": our_follow});
    assert_eq!(
        ours.deliver(&accept, &bob, KEY_A, SignatureScheme::Cavage)
            .await,
        202
    );
    wait_until("our follow to be accepted", async || {
        ours.store().following(&bob).unwrap().unwrap().state == FollowState::Accepted
    })
    .await;
    let create = json!({"type": "Create", "actor": bob, "object": post});
    assert_eq!(
        ours.deliver(&create, &bob, KEY_A, SignatureScheme::Cavage)
            .await,
        202
    );
    wait_until("the post to be indexed", async || {
        ours.search("espaliered").await == [post.clone()]
    })
    .await;
    // Eve, another user on Bob's server, "deletes" Bob's post, which is still live.
    let delete = json!({"type": "Delete", "actor": eve,
        "object": {"id": post, "type": "Tombstone"}});
    assert_eq!(
        ours.deliver(&delete, &eve, KEY_B, SignatureScheme::Cavage)
            .await,
        202
    );
    settle().await;
    assert_eq!(
        ours.search("espaliered").await,
        [post.clone()],
        "Eve removed Bob's live post"
    );
}

#[tokio::test]
#[ignore = "AUDIT: A4 - fails until fixed"]
async fn audit_a4_anyone_on_an_origin_can_tombstone_another_author() {
    let ours = Ours::start().await;
    let gts = FakeHost::start("gotosocial").await;
    ours.introduce(&gts).await;
    let bob = gts.actor(
        "/users/bob",
        "Person",
        KEY_A,
        &json!({"discoverable": true, "indexable": true}),
    );
    let eve = gts.actor("/users/eve", "Person", KEY_B, &json!({}));
    // An ordinary user "bans" a neighbour with removeData.
    let block = json!({"type": "Block", "actor": eve, "object": bob, "removeData": true});
    assert_eq!(
        ours.deliver(&block, &eve, KEY_B, SignatureScheme::Cavage)
            .await,
        202
    );
    settle().await;
    let tombstones = ours.store().list_tombstones().unwrap();
    assert!(
        !tombstones
            .iter()
            .any(|t| t.uri == bob && t.kind == TombstoneKind::Author),
        "Eve tombstoned every post by Bob: {tombstones:?}"
    );
}
