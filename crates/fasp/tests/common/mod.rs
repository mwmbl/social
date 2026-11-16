//! Test fixtures: a Mastodon-like fake FASP host, our own router served on a real socket, and a
//! client that talks to our router the way Mastodon does.
//!
//! Each test binary uses a different subset of these helpers.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use apub::signatures::keys::{Ed25519SigningKey, Ed25519VerifyingKey, SigningKey, VerifyingKey};
use apub::signatures::rfc9421::{self, VerifyPolicy};
use apub::signatures::{FnHeaders, content_digest, integrity};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fasp::announcement::Announcement;
use fasp::client::FaspClient;
use fasp::http::{AccountSearchPage, FaspState, router};
use fasp::registration::{RegistrationOutcome, register};
use fasp::{Capability, PrivacyPolicy, ProviderInfoConfig, StoreAccess};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use store::{Registration, Store};
use url::Url;

pub fn now() -> i64 {
    fasp::system_now()
}

// ---------------------------------------------------------------------------------------------
// The fake Mastodon

pub struct FakeInner {
    pub key: Ed25519SigningKey,
    pub other_key: Ed25519SigningKey,
    pub fasp_id: String,
    /// Full nodeinfo documents by version.
    pub nodeinfo: Mutex<Vec<(String, Value)>>,
    /// What `POST /registration` received.
    pub registered: Mutex<Option<Value>>,
    pub registration_status: AtomicU16,
    /// Every FASP API request (not nodeinfo/registration): (method, path).
    pub requests: Mutex<Vec<(String, String)>>,
    /// Bodies of subscription and backfill creations.
    pub subscription_bodies: Mutex<Vec<Value>>,
    pub backfill_bodies: Mutex<Vec<Value>>,
    pub verification_failures: Mutex<Vec<String>>,
    pub next_id: AtomicI64,
    pub continuations_left: AtomicUsize,
    /// Sign responses with the wrong key.
    pub tamper_signature: AtomicBool,
    /// Leave `content-digest` off responses.
    pub omit_digest: AtomicBool,
    pub rate_limit: AtomicBool,
    pub origin: Mutex<String>,
}

pub struct FakeMastodon {
    pub inner: Arc<FakeInner>,
    pub origin: String,
}

impl FakeMastodon {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let origin = format!("http://{addr}");
        let doc = mastodon_nodeinfo("mastodon");
        let inner = Arc::new(FakeInner {
            key: Ed25519SigningKey::generate().unwrap(),
            other_key: Ed25519SigningKey::generate().unwrap(),
            fasp_id: "dfkl3msw6ps3".to_owned(),
            nodeinfo: Mutex::new(vec![("2.0".into(), doc.clone()), ("2.1".into(), doc)]),
            registered: Mutex::new(None),
            // Mastodon 4.7.5 answers 200 where the spec says 201.
            registration_status: AtomicU16::new(200),
            requests: Mutex::new(Vec::new()),
            subscription_bodies: Mutex::new(Vec::new()),
            backfill_bodies: Mutex::new(Vec::new()),
            verification_failures: Mutex::new(Vec::new()),
            next_id: AtomicI64::new(11),
            continuations_left: AtomicUsize::new(0),
            tamper_signature: AtomicBool::new(false),
            omit_digest: AtomicBool::new(false),
            rate_limit: AtomicBool::new(false),
            origin: Mutex::new(origin.clone()),
        });
        let app = Router::new()
            .fallback(fake_dispatch)
            .with_state(inner.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { inner, origin }
    }

    pub fn url(&self) -> Url {
        Url::parse(&self.origin).unwrap()
    }

    pub fn fasp_base(&self) -> Url {
        Url::parse(&format!("{}/api/fasp", self.origin)).unwrap()
    }

    pub fn request_count(&self) -> usize {
        self.inner.requests.lock().unwrap().len()
    }

    pub fn requests(&self) -> Vec<(String, String)> {
        self.inner.requests.lock().unwrap().clone()
    }

    pub fn failures(&self) -> Vec<String> {
        self.inner.verification_failures.lock().unwrap().clone()
    }

    pub fn set_nodeinfo(&self, docs: Vec<(&str, Value)>) {
        *self.inner.nodeinfo.lock().unwrap() =
            docs.into_iter().map(|(v, d)| (v.to_owned(), d)).collect();
    }
}

pub fn mastodon_nodeinfo(software: &str) -> Value {
    json!({
        "version": "2.0",
        "software": {"name": software, "version": "4.7.5"},
        "protocols": ["activitypub"],
        "metadata": {"nodeName": "fake"}
    })
}

fn json_response(status: u16, value: &Value) -> Response {
    let mut response = Response::new(Body::from(value.to_string()));
    *response.status_mut() = StatusCode::from_u16(status).unwrap();
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));
    response
}

/// Signs a response the way Mastodon does: `@status` and `content-digest`, no `keyid`.
fn fake_signed(inner: &FakeInner, status: u16, body: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(body.clone()));
    *response.status_mut() = StatusCode::from_u16(status).unwrap();
    if !body.is_empty() {
        response
            .headers_mut()
            .insert("content-type", HeaderValue::from_static("application/json"));
    }
    if inner.omit_digest.load(Ordering::SeqCst) {
        return response;
    }
    let digest = content_digest(&body);
    let key = if inner.tamper_signature.load(Ordering::SeqCst) {
        &inner.other_key
    } else {
        &inner.key
    };
    // Mastodon 4.7.5 (Linzer) signs responses with `created` and no `keyid`.
    let params = rfc9421::SignatureParams {
        created: Some(now()),
        ..rfc9421::SignatureParams::default()
    };
    let (input, signature) = rfc9421::sign(
        &rfc9421::Message::Response { status },
        &[("content-digest", digest.as_str())],
        rfc9421::DEFAULT_LABEL,
        &["@status", "content-digest"],
        &params,
        &SigningKey::Ed25519(Ed25519SigningKey::from_seed(&key.to_seed())),
    )
    .unwrap();
    let headers = response.headers_mut();
    headers.insert("content-digest", digest.parse().unwrap());
    headers.insert("signature-input", input.parse().unwrap());
    headers.insert("signature", signature.parse().unwrap());
    response
}

async fn fake_dispatch(State(inner): State<Arc<FakeInner>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, 1 << 20).await.unwrap().to_vec();
    let path = parts.uri.path().to_owned();
    let method = parts.method.as_str().to_owned();
    let origin = inner.origin.lock().unwrap().clone();

    if path == "/.well-known/nodeinfo" {
        let links: Vec<Value> = inner
            .nodeinfo
            .lock()
            .unwrap()
            .iter()
            .map(|(v, _)| {
                json!({
                    "rel": format!("http://nodeinfo.diaspora.software/ns/schema/{v}"),
                    "href": format!("{origin}/nodeinfo/{v}"),
                })
            })
            .collect();
        return json_response(200, &json!({"links": links}));
    }
    if let Some(version) = path.strip_prefix("/nodeinfo/") {
        let doc = inner
            .nodeinfo
            .lock()
            .unwrap()
            .iter()
            .find(|(v, _)| v == version)
            .map(|(_, d)| d.clone());
        return doc.map_or_else(
            || json_response(404, &json!({})),
            |d| json_response(200, &d),
        );
    }
    if path == "/api/fasp/registration" && method == "POST" {
        let status = inner.registration_status.load(Ordering::SeqCst);
        if status != 200 && status != 201 {
            return json_response(status, &json!({"error": "no"}));
        }
        *inner.registered.lock().unwrap() = serde_json::from_slice(&body).ok();
        return json_response(
            status,
            &json!({
                "faspId": inner.fasp_id,
                "publicKey": inner.key.verifying_key().to_base64(),
                "registrationCompletionUri": format!("{origin}/admin/fasps"),
            }),
        );
    }

    // Everything else is a signed FASP API call.
    inner
        .requests
        .lock()
        .unwrap()
        .push((method.clone(), path.clone()));
    if inner.rate_limit.load(Ordering::SeqCst) {
        let mut response = json_response(429, &json!({}));
        response
            .headers_mut()
            .insert("retry-after", HeaderValue::from_static("7"));
        return response;
    }
    if let Err(reason) = fake_verify(&inner, &method, &path, &parts, &body) {
        inner.verification_failures.lock().unwrap().push(reason);
        return fake_signed(&inner, 401, Vec::new());
    }
    let id = || inner.next_id.fetch_add(1, Ordering::SeqCst);
    let json_body = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    match (method.as_str(), path.trim_start_matches("/api/fasp")) {
        ("POST", "/data_sharing/v0/event_subscriptions") => {
            // Mastodon 4.7.5 answers 500 without maxBatchSize (a NOT NULL column).
            if json_body.get("maxBatchSize").is_none() {
                return fake_signed(&inner, 500, Vec::new());
            }
            inner.subscription_bodies.lock().unwrap().push(json_body);
            // Mastodon returns the id as a JSON number.
            let reply = json!({"subscription": {"id": id()}});
            fake_signed(&inner, 201, reply.to_string().into_bytes())
        }
        ("POST", "/data_sharing/v0/backfill_requests") => {
            inner.backfill_bodies.lock().unwrap().push(json_body);
            let reply = json!({"backfillRequest": {"id": id()}});
            fake_signed(&inner, 201, reply.to_string().into_bytes())
        }
        ("POST", p) if p.ends_with("/continuation") => {
            let left = inner.continuations_left.load(Ordering::SeqCst);
            if left > 0 {
                inner.continuations_left.store(left - 1, Ordering::SeqCst);
                fake_signed(&inner, 204, Vec::new())
            } else {
                fake_signed(&inner, 404, Vec::new())
            }
        }
        ("DELETE", p) if p.starts_with("/data_sharing/v0/event_subscriptions/") => {
            fake_signed(&inner, 204, Vec::new())
        }
        _ => fake_signed(&inner, 404, Vec::new()),
    }
}

/// Verifies one of our requests Mastodon-style.
fn fake_verify(
    inner: &FakeInner,
    method: &str,
    path: &str,
    parts: &axum::http::request::Parts,
    body: &[u8],
) -> Result<(), String> {
    let registered = inner.registered.lock().unwrap().clone();
    let registered = registered.ok_or("no registration")?;
    let public_key = Ed25519VerifyingKey::from_base64(registered["publicKey"].as_str().unwrap())
        .map_err(|e| e.to_string())?;
    let lookup = FnHeaders(|name: &str| {
        parts
            .headers
            .get_all(name)
            .iter()
            .map(|v| v.to_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    });
    integrity::verify_content_digest_header(&lookup, body).map_err(|e| e.to_string())?;
    let origin = inner.origin.lock().unwrap().clone();
    let target = format!("{origin}{path}");
    let policy = VerifyPolicy::new(&["@method", "@target-uri", "content-digest"], now(), 300);
    let verified = rfc9421::verify_request(method, &target, &lookup, &policy, |keyid| {
        (keyid == inner.fasp_id).then(|| VerifyingKey::Ed25519(public_key.clone()))
    })
    .map_err(|e| e.to_string())?;
    // Order and parameters exactly as the spec's example.
    if verified.components != ["@method", "@target-uri", "content-digest"] {
        return Err("unexpected covered components".into());
    }
    for header in ["content-type", "accept"] {
        if parts.headers.get(header).and_then(|v| v.to_str().ok()) != Some("application/json") {
            return Err(format!("missing or wrong {header}"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Our side

pub struct TestState {
    pub store: Mutex<Store>,
    pub base_url: String,
    pub info: ProviderInfoConfig,
    pub extra: Vec<Capability>,
    pub capability_events: Mutex<Vec<(i64, String, bool)>>,
    pub announcements: Mutex<Vec<(i64, Announcement)>>,
    /// What `account_search` returns, best first; it pages through this like the binary does.
    pub search_results: Mutex<Vec<String>>,
    /// Every `account_search` call: (registration, term, limit, offset, language).
    pub search_calls: Mutex<Vec<SearchCall>>,
    pub search_fails: AtomicBool,
}

pub type SearchCall = (i64, String, usize, usize, Option<String>);

impl StoreAccess for TestState {
    fn with_store<R>(&self, f: impl FnOnce(&Store) -> R) -> R {
        self.store.with_store(f)
    }
}

impl FaspState for TestState {
    fn base_url(&self) -> &str {
        &self.base_url
    }
    fn provider_info(&self) -> &ProviderInfoConfig {
        &self.info
    }
    fn extra_capabilities(&self) -> Vec<Capability> {
        self.extra.clone()
    }
    async fn on_capability_changed(&self, registration_id: i64, capability: &str, enabled: bool) {
        self.capability_events.lock().unwrap().push((
            registration_id,
            capability.to_owned(),
            enabled,
        ));
    }
    async fn on_announcement(&self, registration_id: i64, announcement: Announcement) {
        self.announcements
            .lock()
            .unwrap()
            .push((registration_id, announcement));
    }
    async fn account_search(
        &self,
        registration_id: i64,
        term: &str,
        limit: usize,
        offset: usize,
        language: Option<&str>,
    ) -> Result<AccountSearchPage, String> {
        self.search_calls.lock().unwrap().push((
            registration_id,
            term.to_owned(),
            limit,
            offset,
            language.map(str::to_owned),
        ));
        if self.search_fails.load(Ordering::SeqCst) {
            return Err("boom".to_owned());
        }
        let all = self.search_results.lock().unwrap();
        let uris: Vec<String> = all.iter().skip(offset).take(limit).cloned().collect();
        let has_more = all.len() > offset + uris.len();
        Ok(AccountSearchPage { uris, has_more })
    }
}

pub struct Ours {
    pub state: Arc<TestState>,
    pub base_url: String,
    pub http: reqwest::Client,
}

pub async fn start_ours(info: ProviderInfoConfig, extra: Vec<Capability>) -> Ours {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let state = Arc::new(TestState {
        store: Mutex::new(Store::open_in_memory().unwrap()),
        base_url: base_url.clone(),
        info,
        extra,
        capability_events: Mutex::new(Vec::new()),
        announcements: Mutex::new(Vec::new()),
        search_results: Mutex::new(Vec::new()),
        search_calls: Mutex::new(Vec::new()),
        search_fails: AtomicBool::new(false),
    });
    let app = router(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Ours {
        state,
        base_url,
        http: reqwest::Client::new(),
    }
}

pub fn default_info() -> ProviderInfoConfig {
    ProviderInfoConfig {
        name: "Mwmbl Social".to_owned(),
        ..ProviderInfoConfig::default()
    }
}

/// Everything wired together: the fake Mastodon, our served router, and a completed
/// registration of us with the fake.
pub struct Env {
    pub mastodon: FakeMastodon,
    pub ours: Ours,
    pub outcome: RegistrationOutcome,
}

impl Env {
    pub async fn new() -> Self {
        Self::with(default_info(), Vec::new()).await
    }

    pub async fn with(info: ProviderInfoConfig, extra: Vec<Capability>) -> Self {
        let mastodon = FakeMastodon::start().await;
        let ours = start_ours(info, extra).await;
        let outcome = register(
            &ours.http,
            &*ours.state,
            "Mwmbl Social",
            &Url::parse(&ours.base_url).unwrap(),
            &mastodon.url(),
            None,
            now(),
        )
        .await
        .unwrap();
        Self {
            mastodon,
            ours,
            outcome,
        }
    }

    pub fn registration(&self) -> Registration {
        self.ours
            .state
            .with_store(|s| s.registration(self.outcome.registration_id))
            .unwrap()
            .unwrap()
    }

    pub fn status(&self) -> store::RegistrationStatus {
        self.registration().status
    }

    pub fn our_public_key(&self) -> Ed25519VerifyingKey {
        Ed25519SigningKey::from_seed_slice(&self.registration().our_private_key)
            .unwrap()
            .verifying_key()
    }

    /// Mastodon enabling a capability: `POST /capabilities/{id}/{version}/activation`.
    pub async fn activate(&self, id: &str, version: &str) -> Reply {
        self.mastodon_request(
            "POST",
            &format!("/capabilities/{id}/{version}/activation"),
            b"",
            &Opts::default(),
        )
        .await
    }

    pub async fn deactivate(&self, id: &str, version: &str) -> Reply {
        self.mastodon_request(
            "DELETE",
            &format!("/capabilities/{id}/{version}/activation"),
            b"",
            &Opts::default(),
        )
        .await
    }

    pub fn enable_data_sharing_in_store(&self) {
        let id = self.outcome.registration_id;
        self.ours
            .state
            .with_store(|s| s.set_capability(id, "data_sharing", "0.1", true, 1))
            .unwrap();
    }

    /// A client for the fake, built from the stored registration.
    pub fn client(&self) -> FaspClient<TestState> {
        FaspClient::new(
            self.ours.http.clone(),
            self.ours.state.clone(),
            &self.registration(),
        )
        .unwrap()
    }

    /// Sends a request to our router the way Mastodon does.
    pub async fn mastodon_request(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        opts: &Opts,
    ) -> Reply {
        let registration = self.registration();
        let keyid = opts
            .keyid
            .clone()
            .unwrap_or_else(|| registration.server_id.clone());
        let key = opts.key.as_ref().unwrap_or(&self.mastodon.inner.key);
        let signed_digest = content_digest(opts.signed_body.as_deref().unwrap_or(body));
        let target = format!(
            "{}{}",
            opts.target_base.as_deref().unwrap_or(&self.ours.base_url),
            path
        );
        let (input, signature) = rfc9421::sign_request(
            method,
            &target,
            &[("content-digest", signed_digest.as_str())],
            &["@method", "@target-uri", "content-digest"],
            &keyid,
            now() + opts.created_offset,
            &SigningKey::Ed25519(Ed25519SigningKey::from_seed(&key.to_seed())),
        )
        .unwrap();
        let sent_digest = opts
            .digest_header
            .clone()
            .unwrap_or_else(|| content_digest(body));
        let mut request = self
            .ours
            .http
            .request(
                method.parse().unwrap(),
                format!("{}{}", self.ours.base_url, path),
            )
            .body(body.to_vec());
        if !opts.omit_signature {
            request = request
                .header("signature-input", input)
                .header("signature", signature);
        }
        request = request.header("content-digest", sent_digest);
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(n, v)| (n.as_str().to_owned(), v.to_str().unwrap().to_owned()))
            .collect();
        let body = response.bytes().await.unwrap().to_vec();
        Reply {
            status,
            headers,
            body,
        }
    }
}

#[derive(Default)]
pub struct Opts {
    pub created_offset: i64,
    pub keyid: Option<String>,
    pub key: Option<Ed25519SigningKey>,
    /// Sign the digest of this body but send the real one.
    pub signed_body: Option<Vec<u8>>,
    /// Send this `content-digest` header instead of the right one.
    pub digest_header: Option<String>,
    pub omit_signature: bool,
    /// The base URL used in the signed `@target-uri` (default: ours).
    pub target_base: Option<String>,
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    /// Verifies this response the way Mastodon does: the `content-digest` header must be
    /// exactly `sha-256=:<base64 sha256(body)>:`, and an RFC 9421 signature over
    /// `("@status" "content-digest")` must verify under our registration's public key.
    pub fn verify_mastodon_style(&self, key: &Ed25519VerifyingKey, server_id: &str) {
        let expected = format!("sha-256=:{}:", STANDARD.encode(Sha256::digest(&self.body)));
        assert_eq!(
            self.header("content-digest"),
            Some(expected.as_str()),
            "content-digest must be exactly sha-256=:<b64>: (status {})",
            self.status
        );
        let input = self.header("signature-input").expect("signature-input");
        assert!(
            input.starts_with("sig1=(\"@status\" \"content-digest\");created="),
            "unexpected signature-input: {input}"
        );
        assert!(
            input.ends_with(&format!(";keyid=\"{server_id}\"")),
            "{input}"
        );
        let lookup = FnHeaders(|name: &str| {
            self.headers
                .iter()
                .filter(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
                .collect::<Vec<_>>()
        });
        let policy = VerifyPolicy::new(&["@status", "content-digest"], now(), 300);
        rfc9421::verify_response(self.status, &lookup, &policy, |id| {
            (id == server_id).then(|| VerifyingKey::Ed25519(key.clone()))
        })
        .unwrap();
    }
}

impl Env {
    /// Asserts that a reply from our router is signed correctly for this registration.
    pub fn assert_signed(&self, reply: &Reply) {
        let registration = self.registration();
        reply.verify_mastodon_style(&self.our_public_key(), &registration.server_id);
    }
}

pub fn privacy(url: &str, language: &str) -> PrivacyPolicy {
    PrivacyPolicy {
        url: url.to_owned(),
        language: language.to_owned(),
    }
}
