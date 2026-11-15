//! The signed `ActivityPub` `GET` client.
//!
//! [`Fetcher::fetch`] retrieves one object or actor by URI and says what happened as a
//! [`FetchOutcome`]. It is the only thing in the project that talks to other people's servers, so
//! it is deliberately cautious:
//!
//! - Only `https://` (and `http://` when [`FetcherConfig::allow_http`] is set, for tests).
//! - At most [`MAX_REDIRECTS`] redirects, and only to the same host.
//! - A found object's `id` must be on the host that served it (no spoofed objects).
//! - Per-host concurrency and a minimum interval between requests, and exponential backoff after
//!   429 and 5xx (or a network failure), during which nothing is sent at all.
//! - Bodies are size-limited, and never logged.
//!
//! Requests are signed: RFC 9421 first, draft-cavage-12 on 401 or 403, and the scheme that works
//! is remembered per host. [`Fetcher::host_states`] and [`Fetcher::seed_host`] let the binary
//! persist that (and the backoff) without this crate knowing about the store.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::time::Instant;
use url::Url;

use crate::signatures::keys::{RsaSigningKey, SigningKey};
use crate::signatures::{SignatureError, authority, cavage, http_date, parse_http_date, rfc9421};

/// Redirects followed per fetch.
pub const MAX_REDIRECTS: usize = 3;
/// The longest a host is left alone after a 429 or 5xx.
pub const MAX_BACKOFF: Duration = Duration::from_secs(3600);

const ACCEPT: &str = "application/activity+json, application/ld+json; \
                      profile=\"https://www.w3.org/ns/activitystreams\"";

/// Which HTTP signature scheme a host accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureScheme {
    /// RFC 9421 `Signature-Input` and `Signature`.
    Rfc9421,
    /// draft-cavage-http-signatures-12 `Signature`.
    Cavage,
}

impl SignatureScheme {
    fn other(self) -> Self {
        match self {
            Self::Rfc9421 => Self::Cavage,
            Self::Cavage => Self::Rfc9421,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Rfc9421 => "rfc9421",
            Self::Cavage => "cavage",
        }
    }
}

/// What we remember about a host, in a form the binary can store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostSnapshot {
    /// The scheme that last worked.
    pub scheme: Option<SignatureScheme>,
    /// Unix seconds until which nothing is sent to the host.
    pub backoff_until: Option<i64>,
}

#[derive(Debug)]
pub struct FetcherConfig {
    /// Our actor's key id, e.g. `https://social.example/actor#main-key`.
    pub key_id: String,
    /// The private half of the key published at `key_id`.
    pub key: RsaSigningKey,
    /// Generic, e.g. `mwmbl-social/0.1.0 (+https://social.example)`; see [`user_agent`].
    pub user_agent: String,
    /// Total time allowed per HTTP request, body included.
    pub timeout: Duration,
    /// Larger bodies are refused. Default 1 MiB.
    pub max_body_bytes: usize,
    /// Simultaneous fetches per host. Default 2.
    pub per_host_concurrency: usize,
    /// Minimum time between requests to one host. Default 250 ms.
    pub min_interval_per_host: Duration,
    /// First backoff step after a 429 or 5xx without `Retry-After`; doubles per failure.
    pub default_backoff: Duration,
    /// Allow `http://` URIs. For tests only.
    pub allow_http: bool,
}

impl FetcherConfig {
    /// A config with the default limits.
    #[must_use]
    pub fn new(key_id: String, key: RsaSigningKey, user_agent: String) -> Self {
        Self {
            key_id,
            key,
            user_agent,
            timeout: Duration::from_secs(20),
            max_body_bytes: 1024 * 1024,
            per_host_concurrency: 2,
            min_interval_per_host: Duration::from_millis(250),
            default_backoff: Duration::from_secs(60),
            allow_http: false,
        }
    }
}

/// The `User-Agent` we send: `mwmbl-social/<version> (+<our base url>)`.
#[must_use]
pub fn user_agent(base_url: &str) -> String {
    format!(
        "mwmbl-social/{} (+{})",
        env!("CARGO_PKG_VERSION"),
        base_url.trim_end_matches('/')
    )
}

/// Why a fetch failed. Messages carry the URI (never a body).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FetchError {
    #[error("failed to build the HTTP client: {reason}")]
    Client { reason: String },
    #[error("invalid URI `{uri}`: {reason}")]
    BadUri { uri: String, reason: String },
    #[error("refusing to fetch `{uri}`: only https is allowed")]
    BadScheme { uri: String },
    #[error("request to `{uri}` failed: {reason}")]
    Network { uri: String, reason: String },
    #[error("request to `{uri}` timed out")]
    Timeout { uri: String },
    #[error("response from `{uri}` is larger than {limit} bytes")]
    TooLarge { uri: String, limit: usize },
    #[error("response from `{uri}` is not JSON")]
    NotJson { uri: String },
    #[error("object fetched from `{uri}` has id `{id}` on a different origin")]
    OriginMismatch { uri: String, id: String },
    #[error("object fetched from `{uri}` has no string `id`")]
    MissingId { uri: String },
    #[error("`{uri}` answered with unexpected status {status}")]
    Status { uri: String, status: u16 },
    #[error("`{uri}` redirected to another host (`{location}`), which is not followed")]
    CrossHostRedirect { uri: String, location: String },
    #[error("`{uri}` redirected more than {MAX_REDIRECTS} times")]
    TooManyRedirects { uri: String },
    #[error("`{uri}` sent a bad redirect: {reason}")]
    BadRedirect { uri: String, reason: String },
    #[error("failed to sign the request for `{uri}`: {source}")]
    Signing {
        uri: String,
        #[source]
        source: SignatureError,
    },
    #[error("internal error fetching `{uri}`: {reason}")]
    Internal { uri: String, reason: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum FetchOutcome {
    /// The object, and the URI it was finally served from (after redirects).
    Found {
        json: Value,
        final_uri: String,
    },
    /// 404, 410, or a `Tombstone`.
    Gone,
    /// 401 or 403 under both signature schemes.
    Refused {
        status: u16,
    },
    /// 429. The host is now backing off for this long.
    RateLimited {
        retry_after_secs: u64,
    },
    /// The host is backing off until this Unix time; nothing was sent.
    Backoff {
        until: i64,
    },
    Error(FetchError),
}

/// How long a host stays cached as cavage-only before RFC 9421 is tried on it again.
const RETRY_RFC9421_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Default)]
struct HostState {
    scheme: Option<SignatureScheme>,
    /// When the host was last found to need cavage-12. The FASP `data_sharing` spec says RFC 9421
    /// MUST be retried periodically for such hosts, so the cache expires after
    /// [`RETRY_RFC9421_AFTER`].
    cavage_since: Option<Instant>,
    backoff_until: Option<i64>,
    failures: u32,
    next_slot: Option<Instant>,
}

struct HostEntry {
    permits: Arc<Semaphore>,
    state: Mutex<HostState>,
}

impl HostEntry {
    fn state(&self) -> MutexGuard<'_, HostState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn active_backoff(&self, now: i64) -> Option<i64> {
        self.state().backoff_until.filter(|until| *until > now)
    }

    /// Reserves the next send slot, at least `min_interval` after the previous one.
    fn reserve_slot(&self, min_interval: Duration) -> Instant {
        let mut state = self.state();
        let now = Instant::now();
        let start = state.next_slot.map_or(now, |next| next.max(now));
        state.next_slot = Some(start.checked_add(min_interval).unwrap_or(start));
        start
    }

    fn remember(&self, scheme: SignatureScheme) {
        let mut state = self.state();
        state.scheme = Some(scheme);
        state.cavage_since = (scheme == SignatureScheme::Cavage).then(Instant::now);
    }

    /// The scheme to try first: the cached one, except that a cavage-only host is tried with
    /// RFC 9421 again once its cache entry is old enough.
    fn first_scheme(&self) -> SignatureScheme {
        let state = self.state();
        match (state.scheme, state.cavage_since) {
            (Some(SignatureScheme::Cavage), Some(since))
                if since.elapsed() >= RETRY_RFC9421_AFTER =>
            {
                SignatureScheme::Rfc9421
            }
            (Some(scheme), _) => scheme,
            (None, _) => SignatureScheme::Rfc9421,
        }
    }

    fn succeeded(&self) {
        let mut state = self.state();
        state.failures = 0;
        state.backoff_until = None;
    }

    /// Records a failure and starts a backoff: `retry_after` if the host gave one, else
    /// exponential from `default_backoff`. Returns the seconds the host is left alone.
    fn failed(&self, now: i64, retry_after: Option<u64>, default_backoff: Duration) -> u64 {
        let mut state = self.state();
        state.failures = state.failures.saturating_add(1);
        let secs = retry_after.unwrap_or_else(|| {
            let doublings = state.failures.saturating_sub(1).min(20);
            let factor = 1_u32.checked_shl(doublings).unwrap_or(u32::MAX);
            default_backoff.saturating_mul(factor).as_secs()
        });
        let secs = secs.clamp(1, MAX_BACKOFF.as_secs());
        let until = now.saturating_add(i64::try_from(secs).unwrap_or(i64::MAX));
        state.backoff_until = Some(until);
        secs
    }
}

pub struct Fetcher {
    client: reqwest::Client,
    key: SigningKey,
    key_id: String,
    max_body_bytes: usize,
    per_host_concurrency: usize,
    min_interval: Duration,
    default_backoff: Duration,
    allow_http: bool,
    hosts: Mutex<HashMap<String, Arc<HostEntry>>>,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// `host` or `host:port`, lowercase.
fn host_key(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    }
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str().map(str::to_ascii_lowercase) == b.host_str().map(str::to_ascii_lowercase)
        && a.port_or_known_default() == b.port_or_known_default()
}

fn auth_failed(status: reqwest::StatusCode) -> bool {
    status.as_u16() == 401 || status.as_u16() == 403
}

/// `Retry-After` as seconds from now: delta-seconds or an HTTP date.
fn parse_retry_after(value: &str, now: i64) -> Option<u64> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(secs);
    }
    let at = parse_http_date(value).ok()?;
    Some(u64::try_from(at.saturating_sub(now)).unwrap_or(0))
}

fn is_tombstone(json: &Value) -> bool {
    match json.get("type") {
        Some(Value::String(t)) => t == "Tombstone",
        Some(Value::Array(types)) => types.iter().any(|t| t.as_str() == Some("Tombstone")),
        _ => false,
    }
}

impl Fetcher {
    pub fn new(config: FetcherConfig) -> Result<Self, FetchError> {
        let client = reqwest::Client::builder()
            .user_agent(config.user_agent)
            .timeout(config.timeout)
            // Redirects are followed by hand, same host only.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| FetchError::Client {
                reason: e.to_string(),
            })?;
        Ok(Self {
            client,
            key: SigningKey::RsaV15Sha256(config.key),
            key_id: config.key_id,
            max_body_bytes: config.max_body_bytes,
            per_host_concurrency: config.per_host_concurrency.max(1),
            min_interval: config.min_interval_per_host,
            default_backoff: config.default_backoff,
            allow_http: config.allow_http,
            hosts: Mutex::new(HashMap::new()),
        })
    }
}

impl Fetcher {
    fn entry(&self, host: &str) -> Arc<HostEntry> {
        let mut hosts = self.hosts.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(hosts.entry(host.to_owned()).or_insert_with(|| {
            Arc::new(HostEntry {
                permits: Arc::new(Semaphore::new(self.per_host_concurrency)),
                state: Mutex::new(HostState::default()),
            })
        }))
    }

    /// What we know about each host, for the binary to persist.
    #[must_use]
    pub fn host_states(&self) -> Vec<(String, HostSnapshot)> {
        let hosts = self.hosts.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out: Vec<(String, HostSnapshot)> = hosts
            .iter()
            .filter_map(|(host, entry)| {
                let state = entry.state();
                (state.scheme.is_some() || state.backoff_until.is_some()).then(|| {
                    (
                        host.clone(),
                        HostSnapshot {
                            scheme: state.scheme,
                            backoff_until: state.backoff_until,
                        },
                    )
                })
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Restores what [`Fetcher::host_states`] gave earlier. `host` is `host` or `host:port`.
    pub fn seed_host(&self, host: &str, snapshot: HostSnapshot) {
        let entry = self.entry(&host.to_ascii_lowercase());
        let mut state = entry.state();
        state.scheme = snapshot.scheme;
        // A persisted cavage entry counts from now: we don't know how old it is.
        state.cavage_since = (snapshot.scheme == Some(SignatureScheme::Cavage)).then(Instant::now);
        state.backoff_until = snapshot.backoff_until;
        state.failures = u32::from(snapshot.backoff_until.is_some());
    }

    fn parse_url(&self, uri: &str) -> Result<Url, FetchError> {
        let mut url = Url::parse(uri).map_err(|e| FetchError::BadUri {
            uri: uri.to_owned(),
            reason: e.to_string(),
        })?;
        self.check_scheme(&url, uri)?;
        if url.host_str().is_none_or(str::is_empty) {
            return Err(FetchError::BadUri {
                uri: uri.to_owned(),
                reason: "no host".to_owned(),
            });
        }
        url.set_fragment(None);
        Ok(url)
    }

    fn check_scheme(&self, url: &Url, uri: &str) -> Result<(), FetchError> {
        match url.scheme() {
            "https" => Ok(()),
            "http" if self.allow_http => Ok(()),
            _ => Err(FetchError::BadScheme {
                uri: uri.to_owned(),
            }),
        }
    }

    /// Fetches `uri` with a signed `GET`. See [`FetchOutcome`].
    pub async fn fetch(&self, uri: &str) -> FetchOutcome {
        let url = match self.parse_url(uri) {
            Ok(url) => url,
            Err(e) => return FetchOutcome::Error(e),
        };
        let host = host_key(&url);
        let entry = self.entry(&host);
        if let Some(until) = entry.active_backoff(now_secs()) {
            tracing::debug!(host, uri, until, "host is backing off, not sending");
            return FetchOutcome::Backoff { until };
        }
        let Ok(_permit) = Arc::clone(&entry.permits).acquire_owned().await else {
            return FetchOutcome::Error(FetchError::Internal {
                uri: uri.to_owned(),
                reason: "host semaphore closed".to_owned(),
            });
        };
        // The host may have started backing off while we waited for a permit.
        if let Some(until) = entry.active_backoff(now_secs()) {
            return FetchOutcome::Backoff { until };
        }
        self.follow(url, &host, &entry).await
    }

    async fn follow(&self, start: Url, host: &str, entry: &HostEntry) -> FetchOutcome {
        let original = start.to_string();
        let mut current = start;
        for _ in 0..=MAX_REDIRECTS {
            let response = match self.send_signed(&current, host, entry).await {
                Ok(response) => response,
                Err(outcome) => return outcome,
            };
            if !response.status().is_redirection() {
                return self.finish(response, &current, host, entry).await;
            }
            let uri = current.to_string();
            let Some(location) = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
            else {
                return FetchOutcome::Error(FetchError::BadRedirect {
                    uri,
                    reason: "no usable Location header".to_owned(),
                });
            };
            let mut next = match current.join(location) {
                Ok(next) => next,
                Err(e) => {
                    return FetchOutcome::Error(FetchError::BadRedirect {
                        uri,
                        reason: format!("bad Location: {e}"),
                    });
                }
            };
            next.set_fragment(None);
            if let Err(e) = self.check_scheme(&next, &uri) {
                return FetchOutcome::Error(e);
            }
            // Same host only. A scheme change on the same host is allowed only to https.
            if host_key(&next) != host
                || (next.scheme() != current.scheme() && next.scheme() != "https")
            {
                tracing::debug!(host, uri, location = next.as_str(), "refusing redirect");
                return FetchOutcome::Error(FetchError::CrossHostRedirect {
                    uri,
                    location: next.to_string(),
                });
            }
            tracing::debug!(host, uri, location = next.as_str(), "following redirect");
            current = next;
        }
        FetchOutcome::Error(FetchError::TooManyRedirects { uri: original })
    }

    /// One hop: the signed request, with the scheme fallback. Returns the response unless it is
    /// a refusal or a transport failure.
    async fn send_signed(
        &self,
        url: &Url,
        host: &str,
        entry: &HostEntry,
    ) -> Result<reqwest::Response, FetchOutcome> {
        let first = entry.first_scheme();
        let response = self.send_once(url, first, host, entry).await?;
        if !auth_failed(response.status()) {
            Self::note_scheme(&response, first, entry);
            return Ok(response);
        }
        let second = first.other();
        let response = self.send_once(url, second, host, entry).await?;
        if auth_failed(response.status()) {
            return Err(FetchOutcome::Refused {
                status: response.status().as_u16(),
            });
        }
        Self::note_scheme(&response, second, entry);
        Ok(response)
    }

    /// A signature that got an answer other than 401, 403, 429 or 5xx was accepted.
    fn note_scheme(response: &reqwest::Response, scheme: SignatureScheme, entry: &HostEntry) {
        let status = response.status().as_u16();
        if status < 500 && status != 429 {
            entry.remember(scheme);
        }
    }

    async fn send_once(
        &self,
        url: &Url,
        scheme: SignatureScheme,
        host: &str,
        entry: &HostEntry,
    ) -> Result<reqwest::Response, FetchOutcome> {
        let uri = url.as_str();
        let signing = |source: SignatureError| {
            FetchOutcome::Error(FetchError::Signing {
                uri: uri.to_owned(),
                source,
            })
        };
        let now = now_secs();
        let mut headers = vec![
            ("host".to_owned(), authority(uri).map_err(signing)?),
            ("date".to_owned(), http_date(now)),
        ];
        match scheme {
            SignatureScheme::Rfc9421 => {
                // Checked end to end: Mastodon 4.7.5 with AUTHORIZED_FETCH accepts a GET
                // signed over `@method` and `@target-uri` with `created` and `keyid` and no
                // `alg`, fetching our key from `keyid` (dev/mastodon, 2026-11-15).
                let (input, signature) = rfc9421::sign_request(
                    "GET",
                    uri,
                    &headers,
                    &["@method", "@target-uri"],
                    &self.key_id,
                    now,
                    &self.key,
                )
                .map_err(signing)?;
                headers.push(("signature-input".to_owned(), input));
                headers.push(("signature".to_owned(), signature));
            }
            SignatureScheme::Cavage => {
                let SigningKey::RsaV15Sha256(rsa) = &self.key else {
                    return Err(signing(SignatureError::Signing {
                        algorithm: "rsa-sha256",
                        reason: "the fetch key is not an RSA key".to_owned(),
                    }));
                };
                let signature = cavage::sign_request(
                    "GET",
                    uri,
                    &headers,
                    cavage::GET_HEADERS,
                    &self.key_id,
                    &cavage::SignOptions::default(),
                    rsa,
                )
                .map_err(signing)?;
                headers.push(("signature".to_owned(), signature));
            }
        }

        let mut request = self.client.get(uri).header(reqwest::header::ACCEPT, ACCEPT);
        for (name, value) in &headers {
            request = request.header(name.as_str(), value.as_str());
        }

        tokio::time::sleep_until(entry.reserve_slot(self.min_interval)).await;
        match request.send().await {
            Ok(response) => {
                tracing::debug!(
                    host,
                    uri,
                    status = response.status().as_u16(),
                    scheme = scheme.name(),
                    "fetched"
                );
                Ok(response)
            }
            Err(e) => {
                let secs = entry.failed(now_secs(), None, self.default_backoff);
                tracing::debug!(host, uri, scheme = scheme.name(), secs, "request failed");
                Err(FetchOutcome::Error(if e.is_timeout() {
                    FetchError::Timeout {
                        uri: uri.to_owned(),
                    }
                } else {
                    FetchError::Network {
                        uri: uri.to_owned(),
                        reason: e.without_url().to_string(),
                    }
                }))
            }
        }
    }

    /// Turns the final (non-redirect) response into an outcome and updates the host's backoff.
    async fn finish(
        &self,
        response: reqwest::Response,
        url: &Url,
        host: &str,
        entry: &HostEntry,
    ) -> FetchOutcome {
        let uri = url.as_str();
        let status = response.status();
        let code = status.as_u16();
        let now = now_secs();
        if code == 429 || status.is_server_error() {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| parse_retry_after(v, now));
            let secs = entry.failed(now, retry_after, self.default_backoff);
            tracing::debug!(host, uri, status = code, secs, "host is backing off");
            return if code == 429 {
                FetchOutcome::RateLimited {
                    retry_after_secs: secs,
                }
            } else {
                FetchOutcome::Error(FetchError::Status {
                    uri: uri.to_owned(),
                    status: code,
                })
            };
        }
        // The host answered properly: whatever the status, it is not struggling.
        entry.succeeded();
        if code == 404 || code == 410 {
            return FetchOutcome::Gone;
        }
        if !status.is_success() {
            return FetchOutcome::Error(FetchError::Status {
                uri: uri.to_owned(),
                status: code,
            });
        }
        let body = match self.read_body(response, uri).await {
            Ok(body) => body,
            Err(e) => return FetchOutcome::Error(e),
        };
        let Ok(json) = serde_json::from_slice::<Value>(&body) else {
            return FetchOutcome::Error(FetchError::NotJson {
                uri: uri.to_owned(),
            });
        };
        if is_tombstone(&json) {
            return FetchOutcome::Gone;
        }
        if let Err(e) = check_origin(&json, url) {
            return FetchOutcome::Error(e);
        }
        FetchOutcome::Found {
            json,
            final_uri: uri.to_owned(),
        }
    }

    async fn read_body(
        &self,
        mut response: reqwest::Response,
        uri: &str,
    ) -> Result<Vec<u8>, FetchError> {
        let too_large = || FetchError::TooLarge {
            uri: uri.to_owned(),
            limit: self.max_body_bytes,
        };
        if response
            .content_length()
            .is_some_and(|len| len > self.max_body_bytes as u64)
        {
            return Err(too_large());
        }
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len().saturating_add(chunk.len()) > self.max_body_bytes {
                        return Err(too_large());
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => return Ok(body),
                Err(e) if e.is_timeout() => {
                    return Err(FetchError::Timeout {
                        uri: uri.to_owned(),
                    });
                }
                Err(e) => {
                    return Err(FetchError::Network {
                        uri: uri.to_owned(),
                        reason: e.without_url().to_string(),
                    });
                }
            }
        }
    }
}

/// The object's `id` must be on the scheme, host and port it was served from.
fn check_origin(json: &Value, served_from: &Url) -> Result<(), FetchError> {
    let uri = served_from.as_str();
    let Some(id) = json.get("id").and_then(Value::as_str) else {
        return Err(FetchError::MissingId {
            uri: uri.to_owned(),
        });
    };
    match Url::parse(id) {
        Ok(id_url) if same_origin(&id_url, served_from) => Ok(()),
        _ => Err(FetchError::OriginMismatch {
            uri: uri.to_owned(),
            id: id.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_forms() {
        assert_eq!(parse_retry_after("120", 0), Some(120));
        assert_eq!(parse_retry_after(" 7 ", 0), Some(7));
        let now = 1_700_000_000;
        let date = http_date(now + 90);
        assert_eq!(parse_retry_after(&date, now), Some(90));
        assert_eq!(parse_retry_after(&http_date(now - 90), now), Some(0));
        assert_eq!(parse_retry_after("soon", now), None);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let entry = HostEntry {
            permits: Arc::new(Semaphore::new(1)),
            state: Mutex::new(HostState::default()),
        };
        let base = Duration::from_secs(60);
        assert_eq!(entry.failed(0, None, base), 60);
        assert_eq!(entry.failed(0, None, base), 120);
        assert_eq!(entry.failed(0, None, base), 240);
        assert_eq!(entry.failed(0, Some(5), base), 5);
        for _ in 0..40 {
            entry.failed(0, None, base);
        }
        assert_eq!(entry.failed(0, None, base), 3600);
        assert_eq!(entry.failed(0, Some(999_999), base), 3600);
        entry.succeeded();
        assert_eq!(entry.failed(0, None, base), 60);
    }

    #[test]
    fn origin_rules() {
        let url = Url::parse("https://a.example/users/x").unwrap();
        let ok = serde_json::json!({"id": "https://a.example/other"});
        assert!(check_origin(&ok, &url).is_ok());
        for id in [
            "https://b.example/users/x",
            "http://a.example/users/x",
            "nope",
        ] {
            let bad = serde_json::json!({ "id": id });
            assert!(matches!(
                check_origin(&bad, &url),
                Err(FetchError::OriginMismatch { .. })
            ));
        }
        assert!(matches!(
            check_origin(&serde_json::json!({}), &url),
            Err(FetchError::MissingId { .. })
        ));
    }
}
