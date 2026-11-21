//! The fetch worker: takes due URIs off the queue, fetches them, decides what each means
//! ([`crate::decide`]) and sends the result to the writer.
//!
//! [`Worker::process`] is the whole pipeline for one queue item and is also what
//! `ingest` runs in-process. [`Worker::run`] and [`Worker::drain`] drive it from the queue.
//! The worker never writes an index itself, and never logs post content: only URIs and hosts.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use apub::consent::ConsentContext;
use apub::fetch::{
    FetchError, FetchOutcome, Fetcher, HostSnapshot, SignatureScheme as FetchScheme, uri_host_key,
};
use serde_json::Value;
use social_index::{SocialAccount, SocialPost};
use store::{FetchKind, FetchReason, QueuedFetch, SignatureScheme, Store, StoreError};
use tinyindex::IndexItem;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use url::Url;

use crate::context::ContextSource;
use crate::crawl::{RobotsSource, RobotsVerdict};
use crate::decide::{
    Action, Author, Carried, Clock, Fetched, author_of, decide_account, decide_post,
    looks_like_post,
};
use crate::pacing::{Pacer, Plan, Running, ToDefer, candidate_limit, plan};
use crate::robots::Robots;
use crate::writer::{WriterHandle, host_blocked};
use crate::{now_millis, now_secs, stamp_millis};

/// The longest an item waits after a transient failure.
const MAX_RETRY_SECS: i64 = 6 * 3600;
/// The first retry delay; it doubles with each failed attempt.
const BASE_RETRY_SECS: i64 = 60;

/// How many author documents the cache holds, and for how long.
pub const AUTHOR_CACHE_CAPACITY: usize = 10_000;
pub const AUTHOR_CACHE_TTL_SECS: i64 = 3600;

// ---------------------------------------------------------------------------------------------
// Carried records

#[derive(Debug, thiserror::Error)]
pub enum CarriedError {
    #[error("carried record is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("carried record has neither a `post` nor an `account` tuple")]
    Shape,
    #[error("carried record does not decode: {0}")]
    Tuple(#[from] tinyindex::PageIoError),
}

/// The queue's `record` column: the stored record a revalidation carries, as JSON
/// `{"post": [tuple...]}` or `{"account": [tuple...]}`. Only what the index already holds.
#[must_use]
pub fn encode_carried(carried: &Carried) -> String {
    match carried {
        Carried::Post(post) => serde_json::json!({ "post": post.to_tuple() }),
        Carried::Account(account) => serde_json::json!({ "account": account.to_tuple() }),
    }
    .to_string()
}

pub fn decode_carried(text: &str) -> Result<Carried, CarriedError> {
    let value: Value = serde_json::from_str(text)?;
    let tuple = |key: &str| match value.get(key) {
        Some(Value::Array(items)) => Some(items.clone()),
        _ => None,
    };
    if let Some(items) = tuple("post") {
        Ok(Carried::Post(SocialPost::from_tuple(items)?))
    } else if let Some(items) = tuple("account") {
        Ok(Carried::Account(SocialAccount::from_tuple(items)?))
    } else {
        Err(CarriedError::Shape)
    }
}

// ---------------------------------------------------------------------------------------------
// Author cache

/// A small bounded cache of author documents, so a burst of posts by one author fetches the
/// author once. Only documents that were fetched successfully are cached.
#[derive(Debug)]
pub struct AuthorCache {
    entries: HashMap<String, CacheEntry>,
    capacity: usize,
    ttl_secs: i64,
}

#[derive(Debug)]
struct CacheEntry {
    inserted: i64,
    /// The [`crate::stamp_millis`] taken when the document's fetch started.
    fetched_at: i64,
    value: Value,
}

impl AuthorCache {
    #[must_use]
    pub fn new(capacity: usize, ttl_secs: i64) -> Self {
        Self {
            entries: HashMap::new(),
            capacity: capacity.max(1),
            ttl_secs,
        }
    }

    /// The cached document and when its fetch started ([`crate::stamp_millis`]).
    pub fn get(&mut self, uri: &str, now: i64) -> Option<(Value, i64)> {
        let expired = self
            .entries
            .get(uri)
            .is_some_and(|e| now >= e.inserted.saturating_add(self.ttl_secs));
        if expired {
            self.entries.remove(uri);
            return None;
        }
        self.entries
            .get(uri)
            .map(|e| (e.value.clone(), e.fetched_at))
    }

    pub fn remove(&mut self, uri: &str) {
        self.entries.remove(uri);
    }

    pub fn insert(&mut self, uri: &str, value: Value, now: i64, fetched_at: i64) {
        if !self.entries.contains_key(uri) && self.entries.len() >= self.capacity {
            let ttl = self.ttl_secs;
            self.entries
                .retain(|_, e| now < e.inserted.saturating_add(ttl));
            if self.entries.len() >= self.capacity {
                let oldest = self
                    .entries
                    .iter()
                    .min_by_key(|(_, e)| e.inserted)
                    .map(|(k, _)| k.clone());
                if let Some(oldest) = oldest {
                    self.entries.remove(&oldest);
                }
            }
        }
        self.entries.insert(
            uri.to_string(),
            CacheEntry {
                inserted: now,
                fetched_at,
                value,
            },
        );
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------------------------
// Fetch outcomes and retry timing

/// What a fetch means to the decision, plus the earliest time worth retrying, when the host
/// said so. Never carries a response body.
#[must_use]
pub fn to_fetched(outcome: FetchOutcome, now: i64) -> (Fetched, Option<i64>) {
    match outcome {
        FetchOutcome::Found { json, .. } => (Fetched::Found(json), None),
        FetchOutcome::Gone => (Fetched::Gone, None),
        FetchOutcome::Refused { .. } => (Fetched::Refused, None),
        FetchOutcome::RateLimited { retry_after_secs } => (
            Fetched::Transient(format!("rate limited, retry after {retry_after_secs}s")),
            Some(now.saturating_add(i64::try_from(retry_after_secs).unwrap_or(i64::MAX))),
        ),
        FetchOutcome::Backoff { until } => (
            Fetched::Transient(format!("host is backing off until {until}")),
            Some(until),
        ),
        FetchOutcome::Error(e) => (Fetched::Transient(e.to_string()), None),
    }
}

/// 60 s times 2 to the number of attempts so far, at most 6 h.
#[must_use]
pub fn backoff_secs(attempts: i64) -> i64 {
    let exponent = u32::try_from(attempts.clamp(0, 20)).unwrap_or(20);
    BASE_RETRY_SECS
        .saturating_mul(1_i64 << exponent)
        .min(MAX_RETRY_SECS)
}

// ---------------------------------------------------------------------------------------------
// The worker

/// How many fetches ended in each outcome since the process started. For `/metrics`.
#[derive(Debug, Default)]
pub struct FetchCounters {
    found: AtomicU64,
    gone: AtomicU64,
    refused: AtomicU64,
    rate_limited: AtomicU64,
    backoff: AtomicU64,
    error: AtomicU64,
    /// Revalidations moved later to keep within a host's budget. Not fetches.
    revalidations_deferred: AtomicU64,
    /// Crawl submissions moved later to keep within a host's budget or `Crawl-delay`.
    crawl_deferred: AtomicU64,
    /// Crawl submissions dropped because their host does not run Lemmy.
    crawl_not_lemmy: AtomicU64,
    /// Crawl submissions (or a redirect or author fetch of one) robots.txt disallowed.
    crawl_robots_disallowed: AtomicU64,
    /// Crawl submissions put off because the host's robots.txt was unavailable.
    crawl_robots_unavailable: AtomicU64,
}

/// Why a crawl submission was not fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrawlSkip {
    NotLemmy,
    RobotsDisallowed,
    RobotsUnavailable,
}

impl FetchCounters {
    pub fn record(&self, outcome: &FetchOutcome) {
        let counter = match outcome {
            FetchOutcome::Found { .. } => &self.found,
            FetchOutcome::Gone => &self.gone,
            FetchOutcome::Refused { .. } => &self.refused,
            FetchOutcome::RateLimited { .. } => &self.rate_limited,
            FetchOutcome::Backoff { .. } => &self.backoff,
            FetchOutcome::Error(_) => &self.error,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_deferred(&self, reason: FetchReason) {
        let counter = if reason == FetchReason::Crawl {
            &self.crawl_deferred
        } else {
            &self.revalidations_deferred
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_crawl_skip(&self, skip: CrawlSkip) {
        let counter = match skip {
            CrawlSkip::NotLemmy => &self.crawl_not_lemmy,
            CrawlSkip::RobotsDisallowed => &self.crawl_robots_disallowed,
            CrawlSkip::RobotsUnavailable => &self.crawl_robots_unavailable,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// How many crawl submissions were moved later to keep within a host's budget.
    #[must_use]
    pub fn crawl_deferred(&self) -> u64 {
        self.crawl_deferred.load(Ordering::Relaxed)
    }

    /// `(reason, count)` for crawl submissions not fetched, in a fixed order.
    #[must_use]
    pub fn crawl_skips(&self) -> Vec<(&'static str, u64)> {
        [
            ("not_lemmy", &self.crawl_not_lemmy),
            ("robots_disallowed", &self.crawl_robots_disallowed),
            ("robots_unavailable", &self.crawl_robots_unavailable),
        ]
        .into_iter()
        .map(|(name, counter)| (name, counter.load(Ordering::Relaxed)))
        .collect()
    }

    /// How many revalidations were moved later to keep within a host's budget.
    #[must_use]
    pub fn revalidations_deferred(&self) -> u64 {
        self.revalidations_deferred.load(Ordering::Relaxed)
    }

    /// `(outcome name, count)` for every outcome, in a fixed order.
    #[must_use]
    pub fn snapshot(&self) -> Vec<(&'static str, u64)> {
        [
            ("found", &self.found),
            ("gone", &self.gone),
            ("refused", &self.refused),
            ("rate_limited", &self.rate_limited),
            ("backoff", &self.backoff),
            ("error", &self.error),
        ]
        .into_iter()
        .map(|(name, counter)| (name, counter.load(Ordering::Relaxed)))
        .collect()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WorkerConfig {
    pub fetch_concurrency: usize,
    pub max_transient_attempts: i64,
    /// How long the loop waits when there is nothing to start.
    pub tick: Duration,
    /// Revalidations to one host start at most once per this interval (zero: no budget). See
    /// [`crate::pacing`].
    pub revalidate_interval_per_host: Duration,
}

/// A transient failure to retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryRequest {
    pub reason: String,
    /// The earliest time to retry, when a host asked for one.
    pub not_before_hint: Option<i64>,
}

/// What processing one queue item did.
#[derive(Debug, Clone, PartialEq)]
pub struct Processed {
    pub uri: String,
    /// What was decided and sent to the writer, `Retry` excluded.
    pub actions: Vec<Action>,
    /// Set if the item should be tried again later.
    pub retry: Option<RetryRequest>,
}

/// What the worker did with a queue item once it had been processed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finished {
    Completed,
    Rescheduled { retry_at: i64 },
    GaveUp,
}

pub struct Worker {
    fetcher: Arc<Fetcher>,
    writer: WriterHandle,
    contexts: ContextSource,
    robots: RobotsSource,
    authors: Mutex<AuthorCache>,
    counters: Arc<FetchCounters>,
    config: WorkerConfig,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// The URIs being processed, so none is started twice, and how many per host.
#[derive(Debug, Default, Clone)]
struct InFlight(Arc<Mutex<InFlightState>>);

#[derive(Debug, Default, Clone)]
struct InFlightState {
    uris: HashSet<String>,
    hosts: HashMap<String, usize>,
}

struct Claim {
    state: Arc<Mutex<InFlightState>>,
    uri: String,
    host: Option<String>,
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.uris.remove(&self.uri);
        if let Some(host) = &self.host
            && let Some(count) = state.hosts.get_mut(host)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.hosts.remove(host);
            }
        }
    }
}

impl InFlight {
    fn claim(&self, uri: &str, host: Option<&str>) -> Option<Claim> {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.uris.insert(uri.to_string()) {
            return None;
        }
        if let Some(host) = host {
            *state.hosts.entry(host.to_string()).or_default() += 1;
        }
        Some(Claim {
            state: Arc::clone(&self.0),
            uri: uri.to_string(),
            host: host.map(str::to_string),
        })
    }

    fn snapshot(&self) -> InFlightState {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

fn same_origin(a: &str, b: &str) -> bool {
    match (Url::parse(a), Url::parse(b)) {
        (Ok(a), Ok(b)) => a.origin() == b.origin(),
        _ => false,
    }
}

impl Worker {
    /// `store` is the worker's own connection, for follow opt-ins and host software.
    #[must_use]
    pub fn new(
        fetcher: Arc<Fetcher>,
        writer: WriterHandle,
        store: Store,
        config: WorkerConfig,
    ) -> Self {
        let contexts = ContextSource::new(Arc::clone(&fetcher), store);
        Self {
            robots: RobotsSource::new(Arc::clone(&fetcher), contexts.store_handle()),
            contexts,
            fetcher,
            writer,
            authors: Mutex::new(AuthorCache::new(
                AUTHOR_CACHE_CAPACITY,
                AUTHOR_CACHE_TTL_SECS,
            )),
            counters: Arc::new(FetchCounters::default()),
            config,
        }
    }

    /// The fetch outcome counters, shared with `/metrics`.
    #[must_use]
    pub fn counters(&self) -> Arc<FetchCounters> {
        Arc::clone(&self.counters)
    }

    async fn fetch(&self, uri: &str, hint: &mut Option<i64>) -> Fetched {
        let now = now_secs();
        let outcome = self.fetcher.fetch(uri).await;
        self.counters.record(&outcome);
        let (fetched, retry_hint) = to_fetched(outcome, now);
        *hint = (*hint).max(retry_hint);
        fetched
    }

    /// [`Worker::fetch`] for a crawl submission: every redirect target must be allowed by
    /// `robots` too. `None` when one was not, and nothing more is to be done with the item.
    async fn fetch_crawled(
        &self,
        uri: &str,
        robots: &Robots,
        hint: &mut Option<i64>,
    ) -> Option<Fetched> {
        let now = now_secs();
        let guard = |url: &Url| robots.allows_url(url);
        let outcome = self.fetcher.fetch_guarded(uri, &guard).await;
        if let FetchOutcome::Error(FetchError::RedirectDisallowed { location, .. }) = &outcome {
            tracing::info!(
                uri,
                host = uri_host_key(uri),
                location,
                "crawl submission dropped: robots.txt disallows its redirect target"
            );
            self.counters.record_crawl_skip(CrawlSkip::RobotsDisallowed);
            return None;
        }
        self.counters.record(&outcome);
        let (fetched, retry_hint) = to_fetched(outcome, now);
        *hint = (*hint).max(retry_hint);
        Some(fetched)
    }

    /// The checks before a crawl submission is fetched, cheapest first: the host runs Lemmy,
    /// and its robots.txt allows the URI. The rules to fetch it under, or why not.
    async fn crawl_check(&self, uri: &str, now: i64) -> Result<Arc<Robots>, (CrawlSkip, i64)> {
        let host = uri_host_key(uri);
        let skip = |skip: CrawlSkip, retry_at: i64| {
            self.counters.record_crawl_skip(skip);
            Err((skip, retry_at))
        };
        if !self.contexts.host_runs_lemmy(uri, now).await {
            tracing::info!(
                uri,
                host,
                "crawl submission dropped: the host does not run Lemmy"
            );
            return skip(CrawlSkip::NotLemmy, now);
        }
        let Ok(url) = Url::parse(uri) else {
            return skip(CrawlSkip::RobotsDisallowed, now);
        };
        match self.robots.verdict(&url, now).await {
            RobotsVerdict::Rules(robots) if robots.allows_url(&url) => Ok(robots),
            RobotsVerdict::Rules(_) => {
                tracing::info!(
                    uri,
                    host,
                    "crawl submission dropped: robots.txt disallows it"
                );
                skip(CrawlSkip::RobotsDisallowed, now)
            }
            RobotsVerdict::Unavailable { retry_at } => {
                tracing::debug!(uri, host, retry_at, "crawl submission waits for robots.txt");
                skip(CrawlSkip::RobotsUnavailable, retry_at)
            }
        }
    }

    /// Replaces the cached copy of an actor document with a fresh fetch, or drops it when the
    /// fetch did not return the document.
    fn refresh_author(&self, uri: &str, fetched: &Fetched, now: i64, fetched_at: i64) {
        let mut cache = self.authors.lock().unwrap_or_else(PoisonError::into_inner);
        match fetched {
            Fetched::Found(json) => cache.insert(uri, json.clone(), now, fetched_at),
            _ => cache.remove(uri),
        }
    }

    /// The author's actor document, from the cache or a fetch, and when its fetch started.
    async fn fetch_author(&self, author_uri: &str, hint: &mut Option<i64>) -> (Fetched, i64) {
        let now = now_secs();
        let cached = self
            .authors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(author_uri, now);
        if let Some((json, fetched_at)) = cached {
            return (Fetched::Found(json), fetched_at);
        }
        let fetched_at = stamp_millis();
        let fetched = self.fetch(author_uri, hint).await;
        if let Fetched::Found(json) = &fetched {
            self.authors
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(author_uri, json.clone(), now, fetched_at);
        }
        (fetched, fetched_at)
    }

    /// The consent context of the actor at `uri`, when its document was fetched. It is worked
    /// out on every fetch, never cached with the document, so revalidation sees an unfollow
    /// or a host's software change.
    async fn context_of(&self, uri: &str, fetched: &Fetched, now: i64) -> ConsentContext {
        match fetched {
            Fetched::Found(json) => self.contexts.context_for(uri, json, now).await,
            _ => ConsentContext::NONE,
        }
    }

    /// Fetch, decide and apply one queue item. This is the pipeline: the queue worker and
    /// `ingest` both come through here.
    pub async fn process(&self, item: &QueuedFetch, now: i64) -> Processed {
        let uri = item.uri.as_str();
        let carried = item.record.as_deref().and_then(|text| {
            decode_carried(text)
                .map_err(
                    |e| tracing::warn!(uri, error = %e, "ignoring an undecodable carried record"),
                )
                .ok()
        });
        let mut hint = None;

        let actions = if host_blocked(&self.writer.tombstones(), uri) {
            tracing::debug!(uri, "host is domain-blocked, not fetching");
            match carried {
                Some(Carried::Post(record)) => vec![Action::RemovePost {
                    uri: uri.to_string(),
                    record: Some(record),
                    tombstone: false,
                }],
                Some(Carried::Account(record)) => vec![Action::RemoveAccount {
                    uri: uri.to_string(),
                    record: Some(record),
                    tombstone: false,
                }],
                None => Vec::new(),
            }
        } else if item.reason == FetchReason::Crawl {
            match self.crawl_check(uri, now).await {
                Ok(robots) => {
                    self.decide(item, carried, Some(&robots), now, &mut hint)
                        .await
                }
                Err((CrawlSkip::RobotsUnavailable, retry_at)) => {
                    hint = Some(retry_at);
                    vec![Action::Retry {
                        reason: "robots.txt unavailable".to_owned(),
                    }]
                }
                // Dropped: completing the item is all there is to do.
                Err(_) => Vec::new(),
            }
        } else {
            self.decide(item, carried, None, now, &mut hint).await
        };

        let (retry_reasons, to_apply): (Vec<Action>, Vec<Action>) = actions
            .into_iter()
            .partition(|a| matches!(a, Action::Retry { .. }));
        let mut retry = retry_reasons.into_iter().find_map(|a| match a {
            Action::Retry { reason } => Some(RetryRequest {
                reason,
                not_before_hint: hint,
            }),
            _ => None,
        });
        if !to_apply.is_empty()
            && let Err(e) = self.writer.apply(to_apply.clone()).await
        {
            tracing::error!(uri, error = %e, "the writer failed to apply a decision");
            retry = Some(RetryRequest {
                reason: format!("index write failed: {e}"),
                not_before_hint: None,
            });
        }
        // A cached author document is now older than the author tombstone it led to, so the
        // writer would refuse every decision made from it. The next post fetches it afresh.
        for action in &to_apply {
            if let Action::AddAuthorTombstone { actor_uri } = action {
                self.authors
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(actor_uri);
            }
        }
        Processed {
            uri: uri.to_string(),
            actions: to_apply,
            retry,
        }
    }

    /// `robots`: the rules a crawl submission is fetched under; `None` for everything else.
    async fn decide(
        &self,
        item: &QueuedFetch,
        carried: Option<Carried>,
        robots: Option<&Robots>,
        now: i64,
        hint: &mut Option<i64>,
    ) -> Vec<Action> {
        let uri = item.uri.as_str();
        let (carried_post, carried_account) = match carried {
            Some(Carried::Post(p)) => (Some(p), None),
            Some(Carried::Account(a)) => (None, Some(a)),
            None => (None, None),
        };
        let clock = Clock {
            now,
            fetched_at: stamp_millis(),
        };
        let fetched = match robots {
            Some(robots) => match self.fetch_crawled(uri, robots, hint).await {
                Some(fetched) => fetched,
                None => return Vec::new(),
            },
            None => self.fetch(uri, hint).await,
        };
        let as_post = match item.kind {
            FetchKind::Object => Some(true),
            FetchKind::Actor => Some(false),
            FetchKind::Unknown => match &fetched {
                Fetched::Found(json) => Some(looks_like_post(json)),
                // Not found: what it was is only known from what we stored for it.
                _ if carried_post.is_some() => Some(true),
                _ if carried_account.is_some() => Some(false),
                _ => None,
            },
        };
        match as_post {
            Some(true) => {
                self.decide_post_path(item, carried_post, &fetched, robots, clock, hint)
                    .await
            }
            Some(false) => {
                // A freshly fetched actor document is newer than any cached copy: an opt-out
                // announced for this account must reach posts processed after it.
                self.refresh_author(uri, &fetched, now, clock.fetched_at);
                let context = self.context_of(uri, &fetched, now).await;
                decide_account(
                    uri,
                    item.reason,
                    item.attempts,
                    carried_account,
                    &fetched,
                    context,
                    clock,
                )
            }
            None => {
                // We don't know whether it was a post or an actor, and it is not there to say:
                // remove it as both.
                let post =
                    decide_post(uri, item.reason, item.attempts, None, &fetched, None, clock);
                let account = decide_account(
                    uri,
                    item.reason,
                    item.attempts,
                    None,
                    &fetched,
                    ConsentContext::NONE,
                    clock,
                );
                if post
                    .iter()
                    .chain(&account)
                    .any(|a| matches!(a, Action::Retry { .. }))
                {
                    post.into_iter()
                        .filter(|a| matches!(a, Action::Retry { .. }))
                        .collect()
                } else {
                    post.into_iter().chain(account).collect()
                }
            }
        }
    }

    async fn decide_post_path(
        &self,
        item: &QueuedFetch,
        carried: Option<SocialPost>,
        fetched: &Fetched,
        robots: Option<&Robots>,
        mut clock: Clock,
        hint: &mut Option<i64>,
    ) -> Vec<Action> {
        let uri = item.uri.as_str();
        let now = clock.now;
        let author = match fetched {
            Fetched::Found(json) => match author_of(json) {
                // A crawl submission's author is on the same host, under the same robots.txt.
                Some(author_uri)
                    if robots.is_some_and(|r| {
                        Url::parse(&author_uri).is_ok_and(|u| !r.allows_url(&u))
                    }) =>
                {
                    tracing::info!(
                        uri,
                        author = author_uri,
                        "crawl submission dropped: robots.txt disallows its author"
                    );
                    self.counters.record_crawl_skip(CrawlSkip::RobotsDisallowed);
                    return Vec::new();
                }
                // Consent needs the author on the object's own origin, so an author anywhere
                // else is never fetched: that is also what stops an object from pointing us at
                // arbitrary URLs.
                Some(author_uri) if same_origin(&author_uri, uri) => {
                    let (author, author_fetched_at) = self.fetch_author(&author_uri, hint).await;
                    // A cached author document can be older than the post's fetch (audit D6).
                    clock.fetched_at = clock.fetched_at.min(author_fetched_at);
                    let context = self.context_of(&author_uri, &author, now).await;
                    Some((author, context))
                }
                _ => None,
            },
            _ => None,
        };
        decide_post(
            uri,
            item.reason,
            item.attempts,
            carried,
            fetched,
            author.as_ref().map(|(fetched, context)| Author {
                fetched,
                context: *context,
            }),
            clock,
        )
    }

    /// Record the result of processing `item` in the queue: complete it, or schedule a retry
    /// with exponential backoff, or give up after `max_transient_attempts`.
    pub fn finish(
        &self,
        store: &Store,
        item: &QueuedFetch,
        processed: &Processed,
        now: i64,
    ) -> Result<Finished, StoreError> {
        let Some(retry) = &processed.retry else {
            complete(store, item)?;
            return Ok(Finished::Completed);
        };
        if item.attempts + 1 >= self.config.max_transient_attempts {
            // Revalidation will queue it again on a later pass if it is still stored.
            tracing::warn!(
                uri = item.uri,
                attempts = item.attempts + 1,
                reason = retry.reason,
                "giving up on a fetch"
            );
            complete(store, item)?;
            return Ok(Finished::GaveUp);
        }
        let retry_at = retry
            .not_before_hint
            .unwrap_or_else(|| now.saturating_add(backoff_secs(item.attempts)))
            .max(now.saturating_add(1));
        tracing::debug!(
            uri = item.uri,
            retry_at,
            reason = retry.reason,
            "will retry"
        );
        if !store.fail(&item.uri, item.generation, &retry.reason, retry_at)? {
            tracing::debug!(
                uri = item.uri,
                "enqueued again while in flight; not rescheduled"
            );
        }
        Ok(Finished::Rescheduled { retry_at })
    }

    /// Process everything due, repeatedly, until nothing is. Items rescheduled into the future
    /// stay queued. Returns how many items were processed. Unpaced: for tests and one-off
    /// runs, not the server loop ([`Worker::run`]).
    pub async fn drain(self: &Arc<Self>, store: &mut Store) -> Result<usize, StoreError> {
        let mut total = 0;
        loop {
            let items = store.next_due(now_secs(), self.config.fetch_concurrency)?;
            if items.is_empty() {
                return Ok(total);
            }
            let mut tasks = JoinSet::new();
            for item in items {
                let worker = Arc::clone(self);
                tasks.spawn(async move {
                    let processed = worker.process(&item, now_secs()).await;
                    (item, processed)
                });
            }
            while let Some(done) = tasks.join_next().await {
                match done {
                    Ok((item, processed)) => {
                        self.finish(&*store, &item, &processed, now_secs())?;
                        total += 1;
                    }
                    Err(e) => tracing::error!(error = %e, "a fetch task failed"),
                }
            }
        }
    }

    /// Run until `shutdown` is signalled: start due items as capacity frees up, at most
    /// `fetch_concurrency` at once and never one URI twice. Items for a host that already has
    /// the fetcher's per-host concurrency in flight are passed over, and revalidations are
    /// held to each host's budget ([`crate::pacing`]).
    pub async fn run(self: Arc<Self>, store: Store, mut shutdown: watch::Receiver<bool>) {
        let permits = Arc::new(Semaphore::new(self.config.fetch_concurrency));
        let in_flight = InFlight::default();
        let mut pacer = Pacer::new(self.config.revalidate_interval_per_host);
        let per_host = self.fetcher.per_host_concurrency();
        let mut tasks: JoinSet<(QueuedFetch, Processed, Claim)> = JoinSet::new();
        loop {
            if *shutdown.borrow() {
                break;
            }
            while let Some(done) = tasks.try_join_next() {
                self.reap(&store, done);
            }
            let free = permits.available_permits();
            if free > 0 {
                let running = in_flight.snapshot();
                match store.next_due(now_secs(), candidate_limit(free, running.uris.len())) {
                    Ok(items) => {
                        let now_ms = now_millis();
                        pacer.begin_pass(now_ms);
                        let crawl_delays = |host: &str| self.robots.crawl_delay_secs(host);
                        let Plan { start, defer } = plan(
                            items,
                            Running {
                                uris: &running.uris,
                                hosts: &running.hosts,
                                free,
                                per_host,
                                crawl_delays: &crawl_delays,
                            },
                            &mut pacer,
                            now_ms,
                        );
                        for deferral in defer {
                            self.defer(&store, &deferral);
                        }
                        for next in start {
                            let Some(claim) = in_flight.claim(&next.item.uri, next.host.as_deref())
                            else {
                                continue;
                            };
                            let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                                break;
                            };
                            let worker = Arc::clone(&self);
                            let item = next.item;
                            tasks.spawn(async move {
                                let processed = worker.process(&item, now_secs()).await;
                                drop(permit);
                                // The claim lives until the queue row is updated, so the
                                // loop cannot pick the item up again before then.
                                (item, processed, claim)
                            });
                        }
                    }
                    Err(e) => tracing::error!(error = %e, "failed to read the fetch queue"),
                }
            }
            tokio::select! {
                done = tasks.join_next(), if !tasks.is_empty() => {
                    if let Some(done) = done {
                        self.reap(&store, done);
                    }
                }
                () = tokio::time::sleep(self.config.tick) => {}
                _ = shutdown.changed() => {}
            }
        }
        while let Some(done) = tasks.join_next().await {
            self.reap(&store, done);
        }
        tracing::debug!("fetch worker stopped");
    }

    /// Move a revalidation or crawl submission to its host's next free slot. Not a failure:
    /// attempts and the last error are left as they are.
    fn defer(&self, store: &Store, deferral: &ToDefer) {
        let uri = deferral.item.uri.as_str();
        let reason = deferral.item.reason;
        match store.defer(uri, reason, deferral.not_before) {
            Ok(true) => {
                self.counters.record_deferred(reason);
                tracing::debug!(
                    uri,
                    host = deferral.host,
                    reason = reason.as_str(),
                    not_before = deferral.not_before,
                    "fetch deferred to keep within the host's budget"
                );
            }
            // Upgraded or finished since it was read: nothing to move.
            Ok(false) => {}
            Err(e) => tracing::error!(uri, error = %e, "failed to defer a fetch"),
        }
    }

    fn reap(
        &self,
        store: &Store,
        done: Result<(QueuedFetch, Processed, Claim), tokio::task::JoinError>,
    ) {
        match done {
            Ok((item, processed, _claim)) => {
                if let Err(e) = self.finish(store, &item, &processed, now_secs()) {
                    tracing::error!(uri = item.uri, error = %e, "failed to update the fetch queue");
                }
            }
            Err(e) => tracing::error!(error = %e, "a fetch task failed"),
        }
    }
}

/// Removes `item`'s row unless the URI was enqueued again while it was in flight, in which case
/// the row stays to be fetched again (audit D1).
fn complete(store: &Store, item: &QueuedFetch) -> Result<(), StoreError> {
    if !store.complete(&item.uri, item.generation)? {
        tracing::debug!(
            uri = item.uri,
            "enqueued again while in flight; kept queued"
        );
    }
    Ok(())
}

/// A queue-shaped item for something fetched without going through the queue (`ingest`).
#[must_use]
pub fn adhoc_item(uri: &str, kind: FetchKind, now: i64) -> QueuedFetch {
    QueuedFetch {
        uri: uri.to_string(),
        kind,
        reason: FetchReason::Announce,
        priority: FetchReason::Announce.priority(),
        record: None,
        not_before: now,
        attempts: 0,
        last_error: None,
        enqueued_at: now,
        generation: 0,
    }
}

// ---------------------------------------------------------------------------------------------
// Host state persistence

fn to_store_scheme(s: FetchScheme) -> SignatureScheme {
    match s {
        FetchScheme::Rfc9421 => SignatureScheme::Rfc9421,
        FetchScheme::Cavage => SignatureScheme::Cavage,
    }
}

fn from_store_scheme(s: SignatureScheme) -> FetchScheme {
    match s {
        SignatureScheme::Rfc9421 => FetchScheme::Rfc9421,
        SignatureScheme::Cavage => FetchScheme::Cavage,
    }
}

/// Load what we remember about hosts into the fetcher. Returns how many hosts.
pub fn seed_hosts(fetcher: &Fetcher, store: &Store, now: i64) -> Result<usize, StoreError> {
    let hosts = store.list_hosts()?;
    let count = hosts.len();
    for host in hosts {
        fetcher.seed_host(
            &host.host,
            HostSnapshot {
                scheme: host.signature_scheme.map(from_store_scheme),
                backoff_until: host.backoff_until.filter(|&until| until > now),
            },
        );
    }
    Ok(count)
}

/// Save the fetcher's per-host state.
pub fn persist_hosts(fetcher: &Fetcher, store: &Store, now: i64) -> Result<(), StoreError> {
    for (host, snapshot) in fetcher.host_states() {
        if let Some(scheme) = snapshot.scheme {
            store.set_signature_scheme(&host, to_store_scheme(scheme))?;
        }
        match snapshot.backoff_until {
            Some(until) if until > now => store.set_backoff(&host, until)?,
            _ => store.clear_backoff(&host)?,
        }
    }
    Ok(())
}

/// Persist the fetcher's host state every `every` until shutdown, and once more on the way out.
pub async fn run_host_persistence(
    fetcher: Arc<Fetcher>,
    store: Store,
    every: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(every);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await;
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = shutdown.changed() => {}
        }
        if let Err(e) = persist_hosts(&fetcher, &store, now_secs()) {
            tracing::error!(error = %e, "failed to persist host state");
        }
        if *shutdown.borrow() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use apub::fetch::FetchError;
    use serde_json::json;

    use super::*;

    fn post() -> SocialPost {
        SocialPost {
            uri: "https://h.example/s/1".into(),
            author_uri: "https://h.example/u/a".into(),
            extract: "hello world".into(),
            score: 1.5,
            term: String::new(),
            published: 100,
            checked: 200,
            title: Some("T".into()),
            language: Some("en".into()),
        }
    }

    fn account() -> SocialAccount {
        SocialAccount {
            uri: "https://h.example/u/a".into(),
            acct: "a@h.example".into(),
            display_name: "A".into(),
            summary: "bio".into(),
            score: 2.0,
            term: String::new(),
            checked: 300,
            languages: Some(vec!["en".into()]),
        }
    }

    #[test]
    fn carried_round_trips() {
        let p = Carried::Post(post());
        let a = Carried::Account(account());
        let encoded = encode_carried(&p);
        assert!(encoded.starts_with("{\"post\":["));
        assert_eq!(decode_carried(&encoded).ok(), Some(p));
        let encoded = encode_carried(&a);
        assert!(encoded.starts_with("{\"account\":["));
        assert_eq!(decode_carried(&encoded).ok(), Some(a));
        // Optional fields survive being absent.
        let mut bare = post();
        bare.title = None;
        bare.language = None;
        let bare = Carried::Post(bare);
        assert_eq!(decode_carried(&encode_carried(&bare)).ok(), Some(bare));
    }

    #[test]
    fn bad_carried_records_are_errors() {
        assert!(matches!(decode_carried("nope"), Err(CarriedError::Json(_))));
        assert!(matches!(decode_carried("{}"), Err(CarriedError::Shape)));
        assert!(matches!(
            decode_carried("{\"post\": \"x\"}"),
            Err(CarriedError::Shape)
        ));
        assert!(matches!(
            decode_carried("{\"post\": [\"u\"]}"),
            Err(CarriedError::Tuple(_))
        ));
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff_secs(0), 60);
        assert_eq!(backoff_secs(1), 120);
        assert_eq!(backoff_secs(3), 480);
        assert_eq!(backoff_secs(20), 6 * 3600);
        assert_eq!(backoff_secs(1_000_000), 6 * 3600);
        assert_eq!(backoff_secs(-5), 60);
    }

    #[test]
    fn outcomes_map_to_decisions() {
        let found = FetchOutcome::Found {
            json: json!({"id": "x"}),
            final_uri: "x".into(),
        };
        assert_eq!(
            to_fetched(found, 0),
            (Fetched::Found(json!({"id": "x"})), None)
        );
        assert_eq!(to_fetched(FetchOutcome::Gone, 0), (Fetched::Gone, None));
        assert_eq!(
            to_fetched(FetchOutcome::Refused { status: 401 }, 0),
            (Fetched::Refused, None)
        );
        let (fetched, hint) = to_fetched(FetchOutcome::Backoff { until: 500 }, 0);
        assert!(matches!(fetched, Fetched::Transient(_)));
        assert_eq!(hint, Some(500));
        let (fetched, hint) = to_fetched(
            FetchOutcome::RateLimited {
                retry_after_secs: 30,
            },
            100,
        );
        assert!(matches!(fetched, Fetched::Transient(_)));
        assert_eq!(hint, Some(130));
        let (fetched, hint) = to_fetched(
            FetchOutcome::Error(FetchError::Timeout {
                uri: "https://h.example/x".into(),
            }),
            0,
        );
        assert!(
            matches!(fetched, Fetched::Transient(reason) if reason.contains("https://h.example/x"))
        );
        assert_eq!(hint, None);
    }

    #[test]
    fn author_cache_expires_and_evicts_the_oldest() {
        let mut cache = AuthorCache::new(2, 100);
        assert!(cache.is_empty());
        cache.insert("a", json!(1), 0, 0);
        cache.insert("b", json!(2), 10, 10);
        assert_eq!(cache.get("a", 50).map(|(v, _)| v), Some(json!(1)));
        // Full: the oldest ("a") goes.
        cache.insert("c", json!(3), 20, 20);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get("a", 20).map(|(v, _)| v), None);
        assert_eq!(cache.get("b", 20).map(|(v, _)| v), Some(json!(2)));
        // Expiry: "b" was inserted at 10 with a 100 s ttl.
        assert_eq!(cache.get("b", 110).map(|(v, _)| v), None);
        assert_eq!(cache.get("c", 110).map(|(v, _)| v), Some(json!(3)));
        // Making room prefers dropping the expired entries.
        cache.insert("d", json!(4), 125, 125);
        cache.insert("e", json!(5), 126, 126);
        assert_eq!(cache.get("d", 126).map(|(v, _)| v), Some(json!(4)));
        assert_eq!(cache.get("e", 126).map(|(v, _)| v), Some(json!(5)));
        assert_eq!(cache.get("c", 126).map(|(v, _)| v), None);
        // Re-inserting an existing key does not evict.
        cache.insert("d", json!(6), 127, 9000);
        assert_eq!(cache.get("e", 127).map(|(v, _)| v), Some(json!(5)));
        // The fetch time comes back with the document, for the writer's tombstone check.
        assert_eq!(cache.get("d", 127), Some((json!(6), 9000)));
    }

    #[test]
    fn in_flight_claims_are_exclusive_and_released_on_drop() {
        let in_flight = InFlight::default();
        let claim = in_flight.claim("u", Some("h.example"));
        assert!(claim.is_some());
        assert!(in_flight.claim("u", Some("h.example")).is_none());
        assert!(in_flight.claim("v", None).is_some());
        assert_eq!(in_flight.snapshot().uris.len(), 1);
        let other = in_flight.claim("w", Some("h.example"));
        assert_eq!(in_flight.snapshot().hosts.get("h.example"), Some(&2));
        drop(claim);
        assert_eq!(in_flight.snapshot().hosts.get("h.example"), Some(&1));
        drop(other);
        assert!(
            in_flight.snapshot().hosts.is_empty(),
            "no empty host entries"
        );
        assert!(in_flight.claim("u", None).is_some());
    }

    #[test]
    fn origins() {
        assert!(same_origin("https://a.example/x", "https://a.example/y/z"));
        assert!(!same_origin("https://a.example/x", "https://b.example/x"));
        assert!(!same_origin("http://a.example/x", "https://a.example/x"));
        assert!(!same_origin(
            "https://a.example:81/x",
            "https://a.example/x"
        ));
        assert!(!same_origin("nonsense", "nonsense"));
    }
}
