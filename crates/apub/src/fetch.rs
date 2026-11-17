//! The signed `ActivityPub` `GET` client.
//!
//! [`Fetcher::fetch`] retrieves one object or actor by URI and says what happened as a
//! [`FetchOutcome`]. [`Fetcher::fetch_document`] does the same for a plain JSON document such
//! as nodeinfo. It is the only thing in the project that talks to other people's servers, so
//! it is deliberately cautious:
//!
//! - Only `https://` (and `http://` when [`FetcherConfig::allow_http`] is set, for tests).
//! - Only public addresses: see [`crate::net`] (off with
//!   [`FetcherConfig::allow_private_addresses`], for tests).
//! - At most [`MAX_REDIRECTS`] redirects, and only to the same host.
//! - A found object's `id` must be on the host that served it (no spoofed objects).
//! - Per-host concurrency and a minimum interval between requests, and exponential backoff after
//!   429 and 5xx (or a network failure), during which nothing is sent at all.
//! - Bodies are size-limited, and never logged.
//!
//! [`Fetcher::fetch_text`] reads a plain-text document such as robots.txt, keeping only its
//! first bytes. [`Fetcher::fetch_guarded`] is [`Fetcher::fetch`] with a check on every redirect
//! target before it is requested (robots.txt for crawl submissions).
//!
//! [`Fetcher::deliver`] sends one activity to an inbox with a signed `POST`, under the same
//! per-host limits, backoff and scheme cache. It follows no redirects and never reads the body.
//!
//! Requests are signed: RFC 9421 first, draft-cavage-12 on 400, 401 or 403, and the scheme that works
//! is remembered per host. A `POST` carries `content-digest` (covered by RFC 9421) and `Digest`
//! (covered by cavage-12). [`Fetcher::host_states`] and [`Fetcher::seed_host`] let the binary
//! persist that (and the backoff) without this crate knowing about the store.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::time::Instant;
use url::Url;

use crate::signatures::integrity::legacy_digest;
use crate::signatures::keys::{RsaSigningKey, SigningKey};
use crate::signatures::{
    SignatureError, authority, cavage, content_digest, http_date, parse_http_date, rfc9421,
};

/// Redirects followed per fetch.
pub const MAX_REDIRECTS: usize = 3;
/// The longest a host is left alone after a 429 or 5xx.
pub const MAX_BACKOFF: Duration = Duration::from_secs(3600);

const ACCEPT: &str = "application/activity+json, application/ld+json; \
                      profile=\"https://www.w3.org/ns/activitystreams\"";
const ACCEPT_JSON: &str = "application/json";
const ACCEPT_TEXT: &str = "text/plain";

/// What is being fetched, which decides the `Accept` header and the checks on the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// An `ActivityPub` object or actor: its `id` must be on the serving origin, and a
    /// `Tombstone` counts as gone.
    ActivityPub,
    /// A plain JSON document such as nodeinfo, with no `id`.
    Document,
    /// A plain-text document such as robots.txt.
    Text,
}

impl Kind {
    fn accept(self) -> &'static str {
        match self {
            Self::ActivityPub => ACCEPT,
            Self::Document => ACCEPT_JSON,
            Self::Text => ACCEPT_TEXT,
        }
    }
}

/// The request one hop sends: a `GET` for a document of some kind, or a `POST` of a body.
#[derive(Debug, Clone, Copy)]
enum Method<'a> {
    Get(Kind),
    Post(&'a [u8]),
}

/// The content type of a delivered activity.
const ACTIVITY_JSON: &str = "application/activity+json";

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
    /// Allow hosts on loopback and private networks (see [`crate::net`]). For tests and dev
    /// setups only.
    pub allow_private_addresses: bool,
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
            allow_private_addresses: false,
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
    #[error("refusing to fetch `{uri}`: {reason}")]
    NotPublic { uri: String, reason: String },
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
    #[error("`{uri}` redirected to `{location}`, which the caller does not allow")]
    RedirectDisallowed { uri: String, location: String },
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

/// What a plain-text fetch ([`Fetcher::fetch_text`]) got.
#[derive(Debug, Clone, PartialEq)]
pub enum TextOutcome {
    /// A 2xx answer, and the first bytes of its body (up to the requested limit).
    Found(Vec<u8>),
    /// A 4xx answer other than 429, including 401 and 403 under both signature schemes.
    ClientError { status: u16 },
    /// Nothing usable: a 429 or 5xx (the host now backs off), an active backoff, or an error.
    Failed(FetchOutcome),
}

/// Checks a redirect target before it is requested; see [`Fetcher::fetch_guarded`].
pub type RedirectGuard<'a> = &'a (dyn Fn(&Url) -> bool + Sync);

/// What happened to one delivery ([`Fetcher::deliver`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// A 2xx answer.
    Delivered { status: u16 },
    /// 401 or 403 under both signature schemes.
    Refused { status: u16 },
    /// Another 3xx or 4xx answer: the inbox will not take it, and retrying will not help.
    Rejected { status: u16 },
    /// 429. The host is now backing off for this long.
    RateLimited { retry_after_secs: u64 },
    /// The host is backing off until this Unix time; nothing was sent.
    Backoff { until: i64 },
    /// A 5xx answer (the host is now backing off), or the request could not be made.
    Error(FetchError),
}

impl DeliveryOutcome {
    /// Whether trying again later could succeed.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. } | Self::Backoff { .. } | Self::Error(_)
        )
    }
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
    user_agent: String,
    key: SigningKey,
    key_id: String,
    max_body_bytes: usize,
    per_host_concurrency: usize,
    min_interval: Duration,
    default_backoff: Duration,
    allow_http: bool,
    allow_private_addresses: bool,
    hosts: Mutex<HashMap<String, Arc<HostEntry>>>,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// The key per-host limits are kept under for `uri`: lowercase `host` or `host:port`. `None`
/// when `uri` does not parse or has no host. Callers that pace fetches per host use this so
/// they group URIs exactly as the fetcher does.
#[must_use]
pub fn uri_host_key(uri: &str) -> Option<String> {
    let url = Url::parse(uri).ok()?;
    url.host_str().filter(|h| !h.is_empty())?;
    Some(host_key(&url))
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

/// A 400 may be a signature the server could not parse: Lemmy 0.19 understands only cavage-12
/// and answers an RFC 9421 signature with 400, not 401 (seen end to end in dev/lemmy). A host
/// that doesn't check signatures on `GET`s can be cached as RFC 9421 by a fetch, so a 400 is
/// retried once with the other scheme too.
fn maybe_bad_signature(status: reqwest::StatusCode) -> bool {
    status.as_u16() == 400
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
        let client = crate::net::client_builder(config.allow_private_addresses)
            .user_agent(config.user_agent.clone())
            .timeout(config.timeout)
            // Redirects are followed by hand, same host only.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| FetchError::Client {
                reason: e.to_string(),
            })?;
        Ok(Self {
            client,
            user_agent: config.user_agent,
            key: SigningKey::RsaV15Sha256(config.key),
            key_id: config.key_id,
            max_body_bytes: config.max_body_bytes,
            per_host_concurrency: config.per_host_concurrency.max(1),
            min_interval: config.min_interval_per_host,
            default_backoff: config.default_backoff,
            allow_http: config.allow_http,
            allow_private_addresses: config.allow_private_addresses,
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

    /// The `User-Agent` sent with every request.
    #[must_use]
    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    /// Simultaneous fetches allowed per host (`FetcherConfig::per_host_concurrency`).
    #[must_use]
    pub fn per_host_concurrency(&self) -> usize {
        self.per_host_concurrency
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
        crate::net::check_url(&url, self.allow_private_addresses).map_err(|e| {
            FetchError::NotPublic {
                uri: uri.to_owned(),
                reason: e.to_string(),
            }
        })?;
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
        self.get(uri, Kind::ActivityPub, None).await
    }

    /// [`Fetcher::fetch`], except that every redirect target (same host, already validated) is
    /// passed to `guard` before it is requested, and one it refuses ends the fetch with
    /// [`FetchError::RedirectDisallowed`]. The caller checks `uri` itself.
    pub async fn fetch_guarded(&self, uri: &str, guard: RedirectGuard<'_>) -> FetchOutcome {
        self.get(uri, Kind::ActivityPub, Some(guard)).await
    }

    /// Fetches a plain-text document (such as robots.txt) with a signed `GET`, under the same
    /// per-host limits, backoff and same-host redirect rule as [`Fetcher::fetch`]. Only the
    /// first `max_bytes` of the body are kept; the rest is not read. Never use it for anything
    /// that is indexed.
    pub async fn fetch_text(&self, uri: &str, max_bytes: usize) -> TextOutcome {
        let (url, host, entry, _permit) = match self.admit(uri).await {
            Ok(admitted) => admitted,
            Err(outcome) => return TextOutcome::Failed(outcome),
        };
        let (response, url) = match self.follow(url, Kind::Text, &host, &entry, None).await {
            Ok(done) => done,
            Err(FetchOutcome::Refused { status }) => return TextOutcome::ClientError { status },
            Err(outcome) => return TextOutcome::Failed(outcome),
        };
        let uri = url.as_str();
        let status = response.status();
        let code = status.as_u16();
        let now = now_secs();
        if code == 429 || status.is_server_error() {
            return TextOutcome::Failed(self.backing_off(&response, &entry, &host, uri, now));
        }
        entry.succeeded();
        if status.is_client_error() {
            return TextOutcome::ClientError { status: code };
        }
        if !status.is_success() {
            return TextOutcome::Failed(FetchOutcome::Error(FetchError::Status {
                uri: uri.to_owned(),
                status: code,
            }));
        }
        match self.read_body(response, uri, max_bytes, true).await {
            Ok(body) => TextOutcome::Found(body),
            Err(e) => TextOutcome::Failed(FetchOutcome::Error(e)),
        }
    }

    /// Fetches a plain JSON document (such as nodeinfo) with a signed `GET`, under the same
    /// per-host limits, backoff, same-host redirect rule and body size cap as [`Fetcher::fetch`],
    /// but without the `id`/origin check or `Tombstone` handling, which only apply to
    /// `ActivityPub` objects. Never use it for anything that is indexed.
    pub async fn fetch_document(&self, uri: &str) -> FetchOutcome {
        self.get(uri, Kind::Document, None).await
    }

    /// The URL and host entry for a request to `uri`, once the host is not backing off and a
    /// permit is held. `Err` is the outcome to report instead.
    async fn admit(
        &self,
        uri: &str,
    ) -> Result<
        (
            Url,
            String,
            Arc<HostEntry>,
            tokio::sync::OwnedSemaphorePermit,
        ),
        FetchOutcome,
    > {
        let url = self.parse_url(uri).map_err(FetchOutcome::Error)?;
        let host = host_key(&url);
        let entry = self.entry(&host);
        if let Some(until) = entry.active_backoff(now_secs()) {
            tracing::debug!(host, uri, until, "host is backing off, not sending");
            return Err(FetchOutcome::Backoff { until });
        }
        let Ok(permit) = Arc::clone(&entry.permits).acquire_owned().await else {
            return Err(FetchOutcome::Error(FetchError::Internal {
                uri: uri.to_owned(),
                reason: "host semaphore closed".to_owned(),
            }));
        };
        // The host may have started backing off while we waited for a permit.
        if let Some(until) = entry.active_backoff(now_secs()) {
            return Err(FetchOutcome::Backoff { until });
        }
        Ok((url, host, entry, permit))
    }

    async fn get(&self, uri: &str, kind: Kind, guard: Option<RedirectGuard<'_>>) -> FetchOutcome {
        let (url, host, entry, _permit) = match self.admit(uri).await {
            Ok(admitted) => admitted,
            Err(outcome) => return outcome,
        };
        match self.follow(url, kind, &host, &entry, guard).await {
            Ok((response, url)) => self.finish(response, &url, kind, &host, &entry).await,
            Err(outcome) => outcome,
        }
    }

    /// Delivers `body` (an activity, serialised) to `inbox` with a signed `POST`. See
    /// [`DeliveryOutcome`]. Redirects are not followed, and the response body is not read.
    pub async fn deliver(&self, inbox: &str, body: &[u8]) -> DeliveryOutcome {
        let (url, host, entry, _permit) = match self.admit(inbox).await {
            Ok(admitted) => admitted,
            Err(outcome) => return delivery_failure(outcome),
        };
        let response = match self
            .send_signed(&url, Method::Post(body), &host, &entry)
            .await
        {
            Ok(response) => response,
            Err(outcome) => return delivery_failure(outcome),
        };
        let status = response.status();
        let code = status.as_u16();
        let now = now_secs();
        if code == 429 || status.is_server_error() {
            let retry_after = retry_after(&response, now);
            let secs = entry.failed(now, retry_after, self.default_backoff);
            tracing::debug!(host, inbox, status = code, secs, "host is backing off");
            return if code == 429 {
                DeliveryOutcome::RateLimited {
                    retry_after_secs: secs,
                }
            } else {
                DeliveryOutcome::Error(FetchError::Status {
                    uri: inbox.to_owned(),
                    status: code,
                })
            };
        }
        entry.succeeded();
        if status.is_success() {
            DeliveryOutcome::Delivered { status: code }
        } else {
            DeliveryOutcome::Rejected { status: code }
        }
    }

    /// Sends the request and follows same-host redirects. Returns the final, non-redirect
    /// response and the URL it came from.
    async fn follow(
        &self,
        start: Url,
        kind: Kind,
        host: &str,
        entry: &HostEntry,
        guard: Option<RedirectGuard<'_>>,
    ) -> Result<(reqwest::Response, Url), FetchOutcome> {
        let original = start.to_string();
        let mut current = start;
        for _ in 0..=MAX_REDIRECTS {
            let response = match self
                .send_signed(&current, Method::Get(kind), host, entry)
                .await
            {
                Ok(response) => response,
                Err(outcome) => return Err(outcome),
            };
            if !response.status().is_redirection() {
                return Ok((response, current));
            }
            let uri = current.to_string();
            let Some(location) = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
            else {
                return Err(FetchOutcome::Error(FetchError::BadRedirect {
                    uri,
                    reason: "no usable Location header".to_owned(),
                }));
            };
            let mut next = match current.join(location) {
                Ok(next) => next,
                Err(e) => {
                    return Err(FetchOutcome::Error(FetchError::BadRedirect {
                        uri,
                        reason: format!("bad Location: {e}"),
                    }));
                }
            };
            next.set_fragment(None);
            if let Err(e) = self.check_scheme(&next, &uri) {
                return Err(FetchOutcome::Error(e));
            }
            // Same host only. A scheme change on the same host is allowed only to https.
            if host_key(&next) != host
                || (next.scheme() != current.scheme() && next.scheme() != "https")
            {
                tracing::debug!(host, uri, location = next.as_str(), "refusing redirect");
                return Err(FetchOutcome::Error(FetchError::CrossHostRedirect {
                    uri,
                    location: next.to_string(),
                }));
            }
            // The guard sees only a target that passed the checks above, so nothing it looks up
            // (a robots.txt) is ever for a host we would not fetch from.
            if guard.is_some_and(|allowed| !allowed(&next)) {
                tracing::debug!(host, uri, location = next.as_str(), "redirect not allowed");
                return Err(FetchOutcome::Error(FetchError::RedirectDisallowed {
                    uri,
                    location: next.to_string(),
                }));
            }
            tracing::debug!(host, uri, location = next.as_str(), "following redirect");
            current = next;
        }
        Err(FetchOutcome::Error(FetchError::TooManyRedirects {
            uri: original,
        }))
    }

    /// One hop: the signed request, with the scheme fallback. Returns the response unless it is
    /// a refusal or a transport failure.
    async fn send_signed(
        &self,
        url: &Url,
        method: Method<'_>,
        host: &str,
        entry: &HostEntry,
    ) -> Result<reqwest::Response, FetchOutcome> {
        let first = entry.first_scheme();
        let response = self.send_once(url, method, first, host, entry).await?;
        if !auth_failed(response.status()) && !maybe_bad_signature(response.status()) {
            Self::note_scheme(&response, first, entry);
            return Ok(response);
        }
        let second = first.other();
        let response = self.send_once(url, method, second, host, entry).await?;
        if auth_failed(response.status()) {
            return Err(FetchOutcome::Refused {
                status: response.status().as_u16(),
            });
        }
        Self::note_scheme(&response, second, entry);
        Ok(response)
    }

    /// A signature that got an answer other than 400, 401, 403, 429 or 5xx was accepted. A 400
    /// proves nothing: the request may have been rejected for its signature or for itself.
    fn note_scheme(response: &reqwest::Response, scheme: SignatureScheme, entry: &HostEntry) {
        let status = response.status().as_u16();
        if status < 500 && status != 429 && status != 400 {
            entry.remember(scheme);
        }
    }

    async fn send_once(
        &self,
        url: &Url,
        method: Method<'_>,
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
        let (name, rfc9421_components, cavage_headers) = match method {
            Method::Get(_) => ("GET", &["@method", "@target-uri"][..], cavage::GET_HEADERS),
            Method::Post(body) => {
                headers.push(("content-type".to_owned(), ACTIVITY_JSON.to_owned()));
                headers.push(("content-digest".to_owned(), content_digest(body)));
                headers.push(("digest".to_owned(), legacy_digest(body)));
                (
                    "POST",
                    &["@method", "@target-uri", "content-digest"][..],
                    cavage::POST_HEADERS,
                )
            }
        };
        match scheme {
            SignatureScheme::Rfc9421 => {
                // Checked end to end: Mastodon 4.7.5 with AUTHORIZED_FETCH accepts a GET
                // signed over `@method` and `@target-uri` with `created` and `keyid` and no
                // `alg`, fetching our key from `keyid` (dev/mastodon, 2026-11-15).
                let (input, signature) = rfc9421::sign_request(
                    name,
                    uri,
                    &headers,
                    rfc9421_components,
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
                    name,
                    uri,
                    &headers,
                    cavage_headers,
                    &self.key_id,
                    &cavage::SignOptions::default(),
                    rsa,
                )
                .map_err(signing)?;
                headers.push(("signature".to_owned(), signature));
            }
        }

        let mut request = match method {
            Method::Get(kind) => self
                .client
                .get(uri)
                .header(reqwest::header::ACCEPT, kind.accept()),
            Method::Post(body) => self.client.post(uri).body(body.to_vec()),
        };
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
            Err(e) => Err(FetchOutcome::Error(
                self.request_failed(e, uri, host, entry, scheme),
            )),
        }
    }

    /// The error for a request that got no response, starting a backoff unless the address was
    /// refused (not the host's fault, and nothing was sent).
    fn request_failed(
        &self,
        e: reqwest::Error,
        uri: &str,
        host: &str,
        entry: &HostEntry,
        scheme: SignatureScheme,
    ) -> FetchError {
        if let Some(refused) = crate::net::address_error(&e) {
            tracing::debug!(host, uri, "refusing a non-public address");
            return FetchError::NotPublic {
                uri: uri.to_owned(),
                reason: refused.to_string(),
            };
        }
        let secs = entry.failed(now_secs(), None, self.default_backoff);
        tracing::debug!(host, uri, scheme = scheme.name(), secs, "request failed");
        if e.is_timeout() {
            FetchError::Timeout {
                uri: uri.to_owned(),
            }
        } else {
            FetchError::Network {
                uri: uri.to_owned(),
                reason: e.without_url().to_string(),
            }
        }
    }

    /// Turns the final (non-redirect) response into an outcome and updates the host's backoff.
    async fn finish(
        &self,
        response: reqwest::Response,
        url: &Url,
        kind: Kind,
        host: &str,
        entry: &HostEntry,
    ) -> FetchOutcome {
        let uri = url.as_str();
        let status = response.status();
        let code = status.as_u16();
        let now = now_secs();
        if code == 429 || status.is_server_error() {
            return self.backing_off(&response, entry, host, uri, now);
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
        let body = match self
            .read_body(response, uri, self.max_body_bytes, false)
            .await
        {
            Ok(body) => body,
            Err(e) => return FetchOutcome::Error(e),
        };
        let Ok(json) = serde_json::from_slice::<Value>(&body) else {
            return FetchOutcome::Error(FetchError::NotJson {
                uri: uri.to_owned(),
            });
        };
        if kind == Kind::ActivityPub {
            if is_tombstone(&json) {
                return FetchOutcome::Gone;
            }
            if let Err(e) = check_origin(&json, url) {
                return FetchOutcome::Error(e);
            }
        }
        FetchOutcome::Found {
            json,
            final_uri: uri.to_owned(),
        }
    }

    /// A 429 or 5xx answer: starts the host's backoff and says so.
    fn backing_off(
        &self,
        response: &reqwest::Response,
        entry: &HostEntry,
        host: &str,
        uri: &str,
        now: i64,
    ) -> FetchOutcome {
        let code = response.status().as_u16();
        let retry_after = retry_after(response, now);
        let secs = entry.failed(now, retry_after, self.default_backoff);
        tracing::debug!(host, uri, status = code, secs, "host is backing off");
        if code == 429 {
            FetchOutcome::RateLimited {
                retry_after_secs: secs,
            }
        } else {
            FetchOutcome::Error(FetchError::Status {
                uri: uri.to_owned(),
                status: code,
            })
        }
    }

    /// Reads at most `limit` bytes of the body. A longer body is an error, or with `truncate`
    /// is cut at `limit` (the rest is not read).
    async fn read_body(
        &self,
        mut response: reqwest::Response,
        uri: &str,
        limit: usize,
        truncate: bool,
    ) -> Result<Vec<u8>, FetchError> {
        let too_large = || FetchError::TooLarge {
            uri: uri.to_owned(),
            limit,
        };
        if !truncate
            && response
                .content_length()
                .is_some_and(|len| len > limit as u64)
        {
            return Err(too_large());
        }
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len().saturating_add(chunk.len()) > limit {
                        if !truncate {
                            return Err(too_large());
                        }
                        let room = limit.saturating_sub(body.len());
                        body.extend(chunk.iter().take(room));
                        return Ok(body);
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

/// A response's `Retry-After`, as seconds from `now`.
fn retry_after(response: &reqwest::Response, now: i64) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| parse_retry_after(v, now))
}

/// A fetch outcome that ended a delivery before it got a usable answer.
fn delivery_failure(outcome: FetchOutcome) -> DeliveryOutcome {
    match outcome {
        FetchOutcome::Refused { status } => DeliveryOutcome::Refused { status },
        FetchOutcome::RateLimited { retry_after_secs } => {
            DeliveryOutcome::RateLimited { retry_after_secs }
        }
        FetchOutcome::Backoff { until } => DeliveryOutcome::Backoff { until },
        FetchOutcome::Error(e) => DeliveryOutcome::Error(e),
        // Only a finished GET is found or gone; a delivery never gets that far.
        FetchOutcome::Found { final_uri, .. } => DeliveryOutcome::Error(FetchError::Internal {
            uri: final_uri,
            reason: "unexpected fetch outcome for a delivery".to_owned(),
        }),
        FetchOutcome::Gone => DeliveryOutcome::Rejected { status: 404 },
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
    fn uri_host_keys() {
        let key = |uri| uri_host_key(uri);
        assert_eq!(key("https://A.Example/x").as_deref(), Some("a.example"));
        assert_eq!(
            key("http://a.example:8080/x").as_deref(),
            Some("a.example:8080")
        );
        // The scheme's default port is not part of the key, as in `Url::port`.
        assert_eq!(key("https://a.example:443/x").as_deref(), Some("a.example"));
        assert_eq!(key("not a uri"), None);
        assert_eq!(key("mailto:a@b.example"), None);
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
