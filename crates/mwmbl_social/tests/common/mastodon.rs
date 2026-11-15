//! A fake Mastodon for the end-to-end FASP test. One origin serves everything a real one would
//! for us: nodeinfo, `POST /api/fasp/registration`, the signed `data_sharing` API (numeric ids,
//! as Mastodon sends them) and, unsigned, the `ActivityPub` objects its users own. It also plays
//! Mastodon's client side: [`FakeMastodon::call_ours`] sends a request to our FASP endpoints
//! signed with the server's key under `keyid = serverId`.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use apub::signatures::keys::{Ed25519SigningKey, Ed25519VerifyingKey, SigningKey, VerifyingKey};
use apub::signatures::rfc9421::{self, VerifyPolicy};
use apub::signatures::{FnHeaders, content_digest, integrity};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use serde_json::{Value, json};

pub const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

fn now() -> i64 {
    fasp::system_now()
}

pub struct Inner {
    key: Ed25519SigningKey,
    fasp_id: String,
    origin: String,
    /// What `POST /registration` received.
    pub registered: Mutex<Option<Value>>,
    /// Every request to the signed FASP API after registration: (method, path).
    pub fasp_calls: Mutex<Vec<(String, String)>>,
    /// `(id, category)` of each subscription created.
    pub subscriptions: Mutex<Vec<(i64, String)>>,
    pub subscription_bodies: Mutex<Vec<Value>>,
    /// `(id, category)` of each backfill request created.
    pub backfills: Mutex<Vec<(i64, String)>>,
    pub backfill_bodies: Mutex<Vec<Value>>,
    /// Paths of continuation requests.
    pub continuations: Mutex<Vec<String>>,
    pub continuations_left: AtomicUsize,
    /// Why our requests failed verification, if they did.
    pub failures: Mutex<Vec<String>>,
    next_id: AtomicI64,
    docs: Mutex<HashMap<String, Value>>,
    gone: Mutex<HashSet<String>>,
    ap_hits: Mutex<HashMap<String, usize>>,
}

pub struct FakeMastodon {
    pub inner: Arc<Inner>,
    pub origin: String,
}

pub fn actor(origin: &str, name: &str) -> Value {
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
        "discoverable": true,
        "indexable": true,
    })
}

pub fn public_note(origin: &str, name: &str, author: &str, content: &str) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{origin}/notes/{name}"),
        "type": "Note",
        "attributedTo": format!("{origin}/users/{author}"),
        "to": [PUBLIC],
        "cc": [format!("{origin}/users/{author}/followers")],
        "published": "2026-11-01T12:00:00Z",
        "content": format!("<p>{content}</p>"),
    })
}

impl FakeMastodon {
    /// Starts on a random local port with the users and notes the test needs.
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let mut docs = HashMap::new();
        docs.insert("/users/alice".to_string(), actor(&origin, "alice"));
        docs.insert("/users/carol".to_string(), actor(&origin, "carol"));
        for (name, text) in [
            ("n1", "Planting garlic today with zebrafinch"),
            ("n2", "Backfilled quokka sourdough starter"),
            ("n3", "Dropped narwhal announcement"),
        ] {
            docs.insert(
                format!("/notes/{name}"),
                public_note(&origin, name, "alice", text),
            );
        }
        let inner = Arc::new(Inner {
            key: Ed25519SigningKey::generate().unwrap(),
            fasp_id: "dfkl3msw6ps3".to_string(),
            origin: origin.clone(),
            registered: Mutex::new(None),
            fasp_calls: Mutex::new(Vec::new()),
            subscriptions: Mutex::new(Vec::new()),
            subscription_bodies: Mutex::new(Vec::new()),
            backfills: Mutex::new(Vec::new()),
            backfill_bodies: Mutex::new(Vec::new()),
            continuations: Mutex::new(Vec::new()),
            continuations_left: AtomicUsize::new(0),
            failures: Mutex::new(Vec::new()),
            next_id: AtomicI64::new(11),
            docs: Mutex::new(docs),
            gone: Mutex::new(HashSet::new()),
            ap_hits: Mutex::new(HashMap::new()),
        });
        let app = Router::new()
            .fallback(dispatch)
            .with_state(Arc::clone(&inner));
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { inner, origin }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }

    pub fn set_gone(&self, path: &str) {
        self.inner.gone.lock().unwrap().insert(path.to_string());
    }

    pub fn ap_hits(&self, path: &str) -> usize {
        self.inner
            .ap_hits
            .lock()
            .unwrap()
            .get(path)
            .copied()
            .unwrap_or(0)
    }

    pub fn fasp_call_count(&self) -> usize {
        self.inner.fasp_calls.lock().unwrap().len()
    }

    pub fn failures(&self) -> Vec<String> {
        self.inner.failures.lock().unwrap().clone()
    }

    pub fn registered(&self) -> Value {
        self.inner.registered.lock().unwrap().clone().unwrap()
    }

    pub fn subscription_id(&self, category: &str) -> String {
        self.inner
            .subscriptions
            .lock()
            .unwrap()
            .iter()
            .find(|(_, c)| c == category)
            .map(|(id, _)| id.to_string())
            .unwrap()
    }

    pub fn backfill_id(&self, category: &str) -> String {
        self.inner
            .backfills
            .lock()
            .unwrap()
            .iter()
            .find(|(_, c)| c == category)
            .map(|(id, _)| id.to_string())
            .unwrap()
    }

    /// Sends a request to our FASP endpoints the way Mastodon does: signed with the server's
    /// key, `keyid` = the `serverId` we chose, target URI built from our registered base URL.
    pub async fn call_ours(&self, method: &str, path: &str, body: &[u8]) -> Reply {
        let registered = self.registered();
        let base = registered["baseUrl"].as_str().unwrap().to_string();
        let server_id = registered["serverId"].as_str().unwrap().to_string();
        let digest = content_digest(body);
        let (input, signature) = rfc9421::sign_request(
            method,
            &format!("{base}{path}"),
            &[("content-digest", digest.as_str())],
            &["@method", "@target-uri", "content-digest"],
            &server_id,
            now(),
            &SigningKey::Ed25519(Ed25519SigningKey::from_seed(&self.inner.key.to_seed())),
        )
        .unwrap();
        let response = reqwest::Client::new()
            .request(method.parse().unwrap(), format!("{base}{path}"))
            .header("content-digest", digest)
            .header("content-type", "application/json")
            .header("signature-input", input)
            .header("signature", signature)
            .body(body.to_vec())
            .send()
            .await
            .unwrap();
        Reply::from(response).await
    }

    /// `call_ours` for a JSON body.
    pub async fn call_ours_json(&self, method: &str, path: &str, body: &Value) -> Reply {
        self.call_ours(method, path, body.to_string().as_bytes())
            .await
    }

    /// Verifies one of our responses the way Mastodon does, against the key and `serverId` we
    /// registered with.
    pub fn verify(&self, reply: &Reply) {
        let registered = self.registered();
        let key =
            Ed25519VerifyingKey::from_base64(registered["publicKey"].as_str().unwrap()).unwrap();
        reply.verify_mastodon_style(&key, registered["serverId"].as_str().unwrap());
    }
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    async fn from(response: reqwest::Response) -> Self {
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(n, v)| (n.as_str().to_string(), v.to_str().unwrap().to_string()))
            .collect();
        let body = response.bytes().await.unwrap().to_vec();
        Self {
            status,
            headers,
            body,
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    /// `content-digest` must be exactly the digest of the body, and an RFC 9421 signature over
    /// `("@status" "content-digest")` with `keyid` = the `serverId` must verify under our
    /// registered public key.
    pub fn verify_mastodon_style(&self, key: &Ed25519VerifyingKey, server_id: &str) {
        assert_eq!(
            self.header("content-digest"),
            Some(content_digest(&self.body).as_str()),
            "content-digest must match the body (status {})",
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

fn json_response(status: u16, value: &Value) -> Response {
    let mut response = Response::new(Body::from(value.to_string()));
    *response.status_mut() = StatusCode::from_u16(status).unwrap();
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));
    response
}

/// Signs a response the way Mastodon does: `@status` and `content-digest`, no `keyid`.
fn signed(inner: &Inner, status: u16, body: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(body.clone()));
    *response.status_mut() = StatusCode::from_u16(status).unwrap();
    if !body.is_empty() {
        response
            .headers_mut()
            .insert("content-type", HeaderValue::from_static("application/json"));
    }
    let digest = content_digest(&body);
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
        &SigningKey::Ed25519(Ed25519SigningKey::from_seed(&inner.key.to_seed())),
    )
    .unwrap();
    let headers = response.headers_mut();
    headers.insert("content-digest", digest.parse().unwrap());
    headers.insert("signature-input", input.parse().unwrap());
    headers.insert("signature", signature.parse().unwrap());
    response
}

fn nodeinfo() -> Value {
    json!({
        "version": "2.0",
        "software": {"name": "mastodon", "version": "4.7.5"},
        "protocols": ["activitypub"],
        "metadata": {"nodeName": "fake"}
    })
}

async fn dispatch(State(inner): State<Arc<Inner>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, 1 << 20).await.unwrap().to_vec();
    let path = parts.uri.path().to_string();
    let method = parts.method.as_str().to_string();
    let origin = inner.origin.clone();

    if path == "/.well-known/nodeinfo" {
        return json_response(
            200,
            &json!({"links": [{
                "rel": "http://nodeinfo.diaspora.software/ns/schema/2.0",
                "href": format!("{origin}/nodeinfo/2.0"),
            }]}),
        );
    }
    if path == "/nodeinfo/2.0" {
        return json_response(200, &nodeinfo());
    }
    if path == "/api/fasp/registration" && method == "POST" {
        *inner.registered.lock().unwrap() = serde_json::from_slice(&body).ok();
        return json_response(
            201,
            &json!({
                "faspId": inner.fasp_id,
                "publicKey": inner.key.verifying_key().to_base64(),
                "registrationCompletionUri": format!("{origin}/admin/fasps"),
            }),
        );
    }
    if let Some(api_path) = path.strip_prefix("/api/fasp") {
        return fasp_api(&inner, &method, api_path, &parts, &body);
    }
    activity_pub(&inner, &path)
}

/// Serves an `ActivityPub` document. It checks no signatures.
fn activity_pub(inner: &Inner, path: &str) -> Response {
    *inner
        .ap_hits
        .lock()
        .unwrap()
        .entry(path.to_string())
        .or_default() += 1;
    let doc = inner.docs.lock().unwrap().get(path).cloned();
    let gone = inner.gone.lock().unwrap().contains(path);
    let mut response = match doc {
        Some(doc) if !gone => Response::new(Body::from(doc.to_string())),
        _ => {
            let mut r = Response::new(Body::empty());
            *r.status_mut() = StatusCode::NOT_FOUND;
            r
        }
    };
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("application/activity+json"),
    );
    response
}

fn fasp_api(
    inner: &Inner,
    method: &str,
    api_path: &str,
    parts: &axum::http::request::Parts,
    body: &[u8],
) -> Response {
    inner
        .fasp_calls
        .lock()
        .unwrap()
        .push((method.to_string(), api_path.to_string()));
    if let Err(reason) = verify_request(inner, method, parts, body) {
        inner.failures.lock().unwrap().push(reason);
        return signed(inner, 401, Vec::new());
    }
    let id = || inner.next_id.fetch_add(1, Ordering::SeqCst);
    let json_body = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
    match (method, api_path) {
        ("POST", "/data_sharing/v0/event_subscriptions") => {
            // Mastodon 4.7.5 answers 500 without maxBatchSize (a NOT NULL column).
            if json_body.get("maxBatchSize").is_none() {
                return signed(inner, 500, Vec::new());
            }
            let category = json_body["category"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            inner.subscription_bodies.lock().unwrap().push(json_body);
            let id = id();
            inner.subscriptions.lock().unwrap().push((id, category));
            // Mastodon returns the id as a JSON number.
            signed(
                inner,
                201,
                json!({"subscription": {"id": id}}).to_string().into_bytes(),
            )
        }
        ("POST", "/data_sharing/v0/backfill_requests") => {
            let category = json_body["category"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            inner.backfill_bodies.lock().unwrap().push(json_body);
            let id = id();
            inner.backfills.lock().unwrap().push((id, category));
            signed(
                inner,
                201,
                json!({"backfillRequest": {"id": id}})
                    .to_string()
                    .into_bytes(),
            )
        }
        ("POST", p)
            if p.starts_with("/data_sharing/v0/backfill_requests/")
                && p.ends_with("/continuation") =>
        {
            inner.continuations.lock().unwrap().push(p.to_string());
            let left = inner.continuations_left.load(Ordering::SeqCst);
            if left > 0 {
                inner.continuations_left.store(left - 1, Ordering::SeqCst);
                signed(inner, 204, Vec::new())
            } else {
                signed(inner, 404, Vec::new())
            }
        }
        _ => signed(inner, 404, Vec::new()),
    }
}

/// Verifies one of our requests Mastodon-style: content-digest, then an RFC 9421 signature by
/// our registered key over `@method`, `@target-uri` and `content-digest`, keyid = faspId.
fn verify_request(
    inner: &Inner,
    method: &str,
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
            .map(|v| v.to_str().unwrap().to_string())
            .collect::<Vec<_>>()
    });
    integrity::verify_content_digest_header(&lookup, body).map_err(|e| e.to_string())?;
    let target = format!(
        "{}{}",
        inner.origin,
        parts
            .uri
            .path_and_query()
            .map_or(parts.uri.path(), axum::http::uri::PathAndQuery::as_str)
    );
    let policy = VerifyPolicy::new(&["@method", "@target-uri", "content-digest"], now(), 300);
    let verified = rfc9421::verify_request(method, &target, &lookup, &policy, |keyid| {
        (keyid == inner.fasp_id).then(|| VerifyingKey::Ed25519(public_key.clone()))
    })
    .map_err(|e| e.to_string())?;
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
