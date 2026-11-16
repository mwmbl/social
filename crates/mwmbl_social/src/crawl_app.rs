//! `POST /api/v1/crawl/submissions`: object URIs found by Mwmbl's crawler (PLAN.md Stage 3
//! item 3).
//!
//! The body is `{"uris": [...]}`, at most [`MAX_URIS`] of them, and nothing else: object JSON
//! is never accepted, since a submission could be forged (CLAUDE.md principle 4). Each URI is
//! fetched from its origin by the worker, which first checks that the host runs Lemmy and that
//! its robots.txt allows the URI ([`crate::crawl`]).
//!
//! - No `crawl_tokens` configured: 404. A missing or wrong bearer token: 401. Tokens are
//!   compared by their SHA-256 digests, in constant time.
//! - A body over [`MAX_BODY_BYTES`]: 413. Not that shape, or too many URIs: 422.
//! - Otherwise 202 with `{accepted, deferred, rejected}`. Duplicates count once. A URI that is
//!   not an `https` URL (`http` too with `allow_http`), is longer than [`MAX_URI_LEN`], is on a
//!   blocked domain or was forgotten is rejected. One that finds the queue at
//!   [`store::CRAWL_QUEUE_PERCENT`] of its cap is deferred: counted, not queued.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use social_index::Tombstones;
use store::{EnqueueOutcome, FetchKind, FetchReason, NewFetch, Store};
use url::Url;

use crate::config::Config;
use crate::now_secs;
use crate::writer::host_blocked;

/// The route.
pub const PATH: &str = "/api/v1/crawl/submissions";
/// The most URIs one submission may carry.
pub const MAX_URIS: usize = 1000;
/// Larger bodies get 413.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Longer URIs are rejected.
pub const MAX_URI_LEN: usize = 2048;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Submission {
    uris: Vec<String>,
}

/// What became of a submission's URIs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    /// Queued, or already queued.
    pub accepted: usize,
    /// Not queued because the queue had no room for crawl submissions.
    pub deferred: usize,
    /// Not a URI we would fetch: bad scheme, too long, blocked domain or forgotten.
    pub rejected: usize,
}

/// Whether `uri` is an absolute URL we would fetch: `https` (or `http` with `allow_http`), with
/// a host and no user info, at most [`MAX_URI_LEN`] long.
#[must_use]
pub fn acceptable_uri(uri: &str, allow_http: bool) -> bool {
    if uri.len() > MAX_URI_LEN {
        return false;
    }
    let Ok(url) = Url::parse(uri) else {
        return false;
    };
    let scheme_ok = url.scheme() == "https" || (allow_http && url.scheme() == "http");
    scheme_ok
        && url.host_str().is_some_and(|h| !h.is_empty())
        && url.username().is_empty()
        && url.password().is_none()
}

fn digest(token: &[u8]) -> [u8; 32] {
    Sha256::digest(token).into()
}

/// Whether `given` is one of `tokens` (digests), looking at every one in full.
fn token_matches(tokens: &[[u8; 32]], given: &str) -> bool {
    let given = digest(given.as_bytes());
    // No early exit, neither within a digest nor across tokens.
    let matched = tokens.iter().fold(0_u8, |matched, token| {
        let diff = token
            .iter()
            .zip(given.iter())
            .fold(0_u8, |acc, (a, b)| acc | (a ^ b));
        matched | u8::from(diff == 0)
    });
    matched != 0
}

/// The bearer token of an `Authorization` header.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.trim().split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

struct Inner {
    tokens: Vec<[u8; 32]>,
    store: Mutex<Store>,
    tombstones: Arc<RwLock<Tombstones>>,
    queue_cap: usize,
    allow_http: bool,
}

/// The crawl submission endpoint. Holds its own SQLite connection.
#[derive(Clone)]
pub struct CrawlApp {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for CrawlApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrawlApp")
            .field("tokens", &self.inner.tokens.len())
            .finish_non_exhaustive()
    }
}

fn respond(status: StatusCode, body: &serde_json::Value) -> Response {
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

fn error(status: StatusCode, message: &str) -> Response {
    respond(status, &json!({ "error": message }))
}

impl CrawlApp {
    #[must_use]
    pub fn new(store: Store, tombstones: Arc<RwLock<Tombstones>>, config: &Config) -> Self {
        Self {
            inner: Arc::new(Inner {
                tokens: config
                    .crawl_tokens
                    .0
                    .iter()
                    .map(|t| digest(t.as_bytes()))
                    .collect(),
                store: Mutex::new(store),
                tombstones,
                queue_cap: config.queue_cap,
                allow_http: config.allow_http,
            }),
        }
    }

    pub fn routes(&self) -> Router {
        Router::new()
            .route(PATH, post(submit_route))
            .with_state(self.clone())
    }

    /// Queues `uris` (deduplicated) as crawl submissions.
    pub fn submit(&self, uris: &[String], now: i64) -> Counts {
        let mut counts = Counts::default();
        let mut seen = HashSet::new();
        let store = self
            .inner
            .store
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for uri in uris {
            if !seen.insert(uri.as_str()) {
                continue;
            }
            let forgotten = || {
                store.is_forgotten(uri).unwrap_or_else(|e| {
                    tracing::error!(uri, error = %e, "failed to read forgotten URIs");
                    true
                })
            };
            if !acceptable_uri(uri, self.inner.allow_http)
                || host_blocked(&self.inner.tombstones, uri)
                || forgotten()
            {
                counts.rejected += 1;
                continue;
            }
            let item = NewFetch {
                uri: uri.clone(),
                kind: FetchKind::Unknown,
                reason: FetchReason::Crawl,
                record: None,
                not_before: now,
            };
            match store.enqueue(&item, now, self.inner.queue_cap) {
                Ok(EnqueueOutcome::Deferred) => counts.deferred += 1,
                Ok(_) => counts.accepted += 1,
                Err(e) => {
                    tracing::error!(uri, error = %e, "failed to queue a crawl submission");
                    counts.deferred += 1;
                }
            }
        }
        counts
    }
}

async fn submit_route(State(app): State<CrawlApp>, request: Request) -> Response {
    if app.inner.tokens.is_empty() {
        return error(StatusCode::NOT_FOUND, "not found");
    }
    if !bearer(request.headers()).is_some_and(|t| token_matches(&app.inner.tokens, t)) {
        let mut response = error(StatusCode::UNAUTHORIZED, "a valid bearer token is required");
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return response;
    }
    let Ok(body) = to_bytes(request.into_body(), MAX_BODY_BYTES).await else {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "the body is too large");
    };
    let Ok(submission) = serde_json::from_slice::<Submission>(&body) else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "the body must be {\"uris\": [...]}, a list of object URIs",
        );
    };
    if submission.uris.len() > MAX_URIS {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "at most 1000 URIs per submission",
        );
    }
    let worker = app.clone();
    let submitted = submission.uris.len();
    match tokio::task::spawn_blocking(move || worker.submit(&submission.uris, now_secs())).await {
        Ok(counts) => {
            tracing::info!(
                submitted,
                accepted = counts.accepted,
                deferred = counts.deferred,
                rejected = counts.rejected,
                "crawl submission"
            );
            respond(StatusCode::ACCEPTED, &json!(counts))
        }
        Err(e) => {
            tracing::error!(error = %e, "the crawl submission task failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "the submission failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_we_would_fetch() {
        let cases = [
            ("https://l.example/post/1", false, true),
            ("https://l.example:8443/comment/2", false, true),
            ("http://l.example/post/1", false, false),
            ("http://l.example/post/1", true, true),
            ("ftp://l.example/post/1", true, false),
            ("https://user:pw@l.example/post/1", false, false),
            ("https://user@l.example/post/1", false, false),
            ("/post/1", false, false),
            ("not a uri", false, false),
            ("mailto:a@l.example", false, false),
            ("", false, false),
        ];
        for (uri, allow_http, expected) in cases {
            assert_eq!(acceptable_uri(uri, allow_http), expected, "{uri}");
        }
        let long = format!("https://l.example/{}", "a".repeat(MAX_URI_LEN));
        assert!(!acceptable_uri(&long, false));
    }

    #[test]
    fn tokens_and_bearer_headers() {
        let tokens = [digest(b"0123456789abcdef"), digest(b"fedcba9876543210")];
        assert!(token_matches(&tokens, "0123456789abcdef"));
        assert!(token_matches(&tokens, "fedcba9876543210"));
        assert!(!token_matches(&tokens, "0123456789abcde"));
        assert!(!token_matches(&tokens, ""));
        assert!(!token_matches(&[], "0123456789abcdef"));
        let headers = |value: &'static str| {
            let mut h = HeaderMap::new();
            h.insert(header::AUTHORIZATION, HeaderValue::from_static(value));
            h
        };
        assert_eq!(bearer(&headers("Bearer abc")), Some("abc"));
        assert_eq!(bearer(&headers("bearer  abc ")), Some("abc"));
        assert_eq!(bearer(&headers("Basic abc")), None);
        assert_eq!(bearer(&headers("Bearer ")), None);
        assert_eq!(bearer(&headers("Bearer")), None);
        assert_eq!(bearer(&HeaderMap::new()), None);
    }

    #[test]
    fn submissions_are_counted_and_deduplicated() -> Result<(), Box<dyn std::error::Error>> {
        let store = Store::open_in_memory()?;
        store.add_domain_block("blocked.example", 0)?;
        store.add_forgotten("https://l.example/post/9", 0)?;
        let mut tombstones = Tombstones::new();
        tombstones.add_domain("blocked.example");
        let config = Config {
            queue_cap: 4,
            ..Config::default()
        };
        let app = CrawlApp::new(store, Arc::new(RwLock::new(tombstones)), &config);
        let uris: Vec<String> = [
            "https://l.example/post/1",
            "https://l.example/post/1",
            "https://l.example/post/2",
            "http://l.example/post/3",
            "https://blocked.example/post/4",
            "https://sub.blocked.example/post/5",
            "https://l.example/post/9",
            // The queue (cap 4) takes crawl items only while it is under half full.
            "https://l.example/post/6",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(
            app.submit(&uris, 100),
            Counts {
                accepted: 2,
                deferred: 1,
                rejected: 4
            }
        );
        // Already queued counts as accepted.
        assert_eq!(app.submit(&uris[..1], 101).accepted, 1);
        let store = app
            .inner
            .store
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let queued = store.next_due(100, 10)?;
        assert_eq!(queued.len(), 2);
        assert!(
            queued
                .iter()
                .all(|q| q.reason == FetchReason::Crawl && q.kind == FetchKind::Unknown)
        );
        Ok(())
    }
}
