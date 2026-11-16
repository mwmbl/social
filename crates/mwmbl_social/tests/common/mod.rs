//! Shared test scaffolding: an in-process fake `ActivityPub` host and a ready-wired pipeline
//! (writer, worker, store). Nothing here touches the network beyond 127.0.0.1.
#![allow(dead_code)]

pub mod mastodon;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use apub::fetch::{Fetcher, FetcherConfig};
use apub::signatures::keys::RsaSigningKey;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Response, Uri};
use mwmbl_social::config::Config;
use mwmbl_social::now_secs;
use mwmbl_social::search::{self, AccountView, PostView};
use mwmbl_social::worker::{Processed, Worker, WorkerConfig, adhoc_item};
use mwmbl_social::writer::{Writer, WriterHandle};
use serde_json::{Value, json};
use social_index::{SocialAccount, SocialPost};
use store::{FetchKind, FetchReason, Store};
use tempfile::TempDir;

pub const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

pub fn test_key() -> RsaSigningKey {
    let pem = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../apub/tests/fixtures/cavage12_private.pem"
    ))
    .unwrap();
    RsaSigningKey::from_pem(&pem).unwrap()
}

#[derive(Default)]
struct FakeState {
    docs: Mutex<HashMap<String, Value>>,
    gone: Mutex<HashSet<String>>,
    failing: Mutex<HashSet<String>>,
    hits: Mutex<HashMap<String, usize>>,
}

/// A fake host serving a few actors and notes. It does not check signatures.
pub struct FakeHost {
    pub origin: String,
    state: Arc<FakeState>,
}

async fn serve_doc(State(state): State<Arc<FakeState>>, uri: Uri) -> Response<Body> {
    let path = uri.path().to_string();
    *state.hits.lock().unwrap().entry(path.clone()).or_default() += 1;
    let status = if state.failing.lock().unwrap().contains(&path) {
        500
    } else if state.gone.lock().unwrap().contains(&path) {
        404
    } else if state.docs.lock().unwrap().contains_key(&path) {
        200
    } else {
        404
    };
    let body = state
        .docs
        .lock()
        .unwrap()
        .get(&path)
        .filter(|_| status == 200)
        .map_or_else(String::new, Value::to_string);
    Response::builder()
        .status(status)
        .header("content-type", "application/activity+json")
        .body(Body::from(body))
        .unwrap()
}

fn actor(origin: &str, name: &str, indexable: bool, discoverable: bool) -> Value {
    let id = format!("{origin}/users/{name}");
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": id,
        "type": "Person",
        "preferredUsername": name,
        "name": format!("{name} Gardener"),
        "summary": "<p>Gardens and Rust</p>",
        "inbox": format!("{id}/inbox"),
        "outbox": format!("{id}/outbox"),
        "followers": format!("{id}/followers"),
        "discoverable": discoverable,
        "indexable": indexable,
    })
}

fn note(origin: &str, name: &str, author: &str, content: &str, to: &[&str], cc: &[&str]) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{origin}/notes/{name}"),
        "type": "Note",
        "attributedTo": format!("{origin}/users/{author}"),
        "to": to,
        "cc": cc,
        "published": "2026-11-01T12:00:00Z",
        "content": format!("<p>{content}</p>"),
    })
}

impl FakeHost {
    /// Serves, at `{origin}/...`: users `alice` (discoverable and indexable) and `bob`
    /// (discoverable, not indexable); notes `public` and `unlisted` by alice, and `bobs` (public)
    /// by bob.
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(FakeState::default());
        {
            let mut docs = state.docs.lock().unwrap();
            let followers = format!("{origin}/users/alice/followers");
            docs.insert("/users/alice".into(), actor(&origin, "alice", true, true));
            docs.insert("/users/bob".into(), actor(&origin, "bob", false, true));
            docs.insert(
                "/notes/public".into(),
                note(
                    &origin,
                    "public",
                    "alice",
                    "Planting garlic today with zebrafinch",
                    &[PUBLIC],
                    &[&followers],
                ),
            );
            docs.insert(
                "/notes/unlisted".into(),
                note(
                    &origin,
                    "unlisted",
                    "alice",
                    "Secretly unlisted quokka thoughts",
                    &[&followers],
                    &[PUBLIC],
                ),
            );
            docs.insert(
                "/notes/bobs".into(),
                note(
                    &origin,
                    "bobs",
                    "bob",
                    "Bob writes about narwhal migration",
                    &[PUBLIC],
                    &[],
                ),
            );
        }
        let app = Router::new()
            .fallback(serve_doc)
            .with_state(Arc::clone(&state));
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { origin, state }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }

    /// Replace (or add) the document served at `path`.
    pub fn set_doc(&self, path: &str, doc: Value) {
        self.state
            .docs
            .lock()
            .unwrap()
            .insert(path.to_string(), doc);
    }

    /// The document currently served at `path`.
    pub fn doc(&self, path: &str) -> Option<Value> {
        self.state.docs.lock().unwrap().get(path).cloned()
    }

    /// Answer 404 for `path` from now on.
    pub fn set_gone(&self, path: &str) {
        self.state.gone.lock().unwrap().insert(path.to_string());
    }

    /// Answer 500 for `path` from now on.
    pub fn set_failing(&self, path: &str) {
        self.state.failing.lock().unwrap().insert(path.to_string());
    }

    pub fn hits(&self, path: &str) -> usize {
        self.state
            .hits
            .lock()
            .unwrap()
            .get(path)
            .copied()
            .unwrap_or(0)
    }

    pub fn total_hits(&self) -> usize {
        self.state.hits.lock().unwrap().values().sum()
    }
}

/// A small test instance config: 64-page indexes, plain http allowed.
pub fn test_config(dir: &TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        posts_pages: 64,
        accounts_pages: 64,
        page_size: 4096,
        allow_http: true,
        base_url: Some("http://127.0.0.1:1".to_string()),
        ..Config::default()
    }
}

pub struct TestEnv {
    pub dir: TempDir,
    pub config: Config,
    pub writer: Writer,
    pub handle: WriterHandle,
    pub worker: Arc<Worker>,
    pub store: Store,
    pub fake: FakeHost,
}

impl TestEnv {
    pub async fn new() -> Self {
        Self::with_attempts(10).await
    }

    pub async fn with_attempts(max_transient_attempts: i64) -> Self {
        Self::with_worker(WorkerConfig {
            fetch_concurrency: 4,
            max_transient_attempts,
            tick: Duration::from_millis(10),
            revalidate_interval_per_host: Duration::ZERO,
        })
        .await
    }

    pub async fn with_worker(worker_config: WorkerConfig) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(&dir);
        let writer = Writer::start(&config).unwrap();
        let handle = writer.handle();
        let mut fetcher = FetcherConfig::new(
            "http://127.0.0.1:1/actor#main-key".to_string(),
            test_key(),
            "mwmbl-social-test".to_string(),
        );
        fetcher.allow_http = true;
        fetcher.min_interval_per_host = Duration::ZERO;
        let worker = Arc::new(Worker::new(
            Arc::new(Fetcher::new(fetcher).unwrap()),
            handle.clone(),
            Store::open(&config.store_path()).unwrap(),
            worker_config,
        ));
        let store = Store::open(&config.store_path()).unwrap();
        let fake = FakeHost::start().await;
        Self {
            dir,
            config,
            writer,
            handle,
            worker,
            store,
            fake,
        }
    }

    /// Run one URI through the worker's pipeline, as `ingest` does.
    pub async fn ingest(&self, path: &str, kind: FetchKind, reason: FetchReason) -> Processed {
        let mut item = adhoc_item(&self.fake.url(path), kind, now_secs());
        item.reason = reason;
        self.worker.process(&item, now_secs()).await
    }

    pub fn search_posts(&self, query: &str) -> Vec<PostView> {
        let (posts, _) = self.handle.readers();
        search::posts(&posts, &self.handle.tombstones(), query, 50, now_secs()).unwrap()
    }

    pub fn search_accounts(&self, query: &str) -> Vec<AccountView> {
        let (_, accounts) = self.handle.readers();
        search::accounts(&accounts, &self.handle.tombstones(), query, 50, None).unwrap()
    }

    /// Every record on every post page.
    pub fn post_pages(&self) -> Vec<SocialPost> {
        let (posts, _) = self.handle.readers();
        posts
            .index()
            .pages()
            .flat_map(|(_, records)| records.unwrap())
            .collect()
    }

    pub fn account_pages(&self) -> Vec<SocialAccount> {
        let (_, accounts) = self.handle.readers();
        accounts
            .index()
            .pages()
            .flat_map(|(_, records)| records.unwrap())
            .collect()
    }
}
