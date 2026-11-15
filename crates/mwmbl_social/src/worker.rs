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

use apub::fetch::{FetchOutcome, Fetcher, HostSnapshot, SignatureScheme as FetchScheme};
use serde_json::Value;
use social_index::{SocialAccount, SocialPost};
use store::{FetchKind, FetchReason, QueuedFetch, SignatureScheme, Store, StoreError};
use tinyindex::IndexItem;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use url::Url;

use crate::decide::{
    Action, Carried, Fetched, author_of, decide_account, decide_post, looks_like_post,
};
use crate::now_secs;
use crate::writer::{WriterHandle, host_blocked};

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

    pub fn get(&mut self, uri: &str, now: i64) -> Option<Value> {
        let expired = self
            .entries
            .get(uri)
            .is_some_and(|e| now >= e.inserted.saturating_add(self.ttl_secs));
        if expired {
            self.entries.remove(uri);
            return None;
        }
        self.entries.get(uri).map(|e| e.value.clone())
    }

    pub fn remove(&mut self, uri: &str) {
        self.entries.remove(uri);
    }

    pub fn insert(&mut self, uri: &str, value: Value, now: i64) {
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

/// The URIs being processed, so none is started twice.
#[derive(Debug, Default, Clone)]
struct InFlight(Arc<Mutex<HashSet<String>>>);

struct Claim {
    set: Arc<Mutex<HashSet<String>>>,
    uri: String,
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.set
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.uri);
    }
}

impl InFlight {
    fn claim(&self, uri: &str) -> Option<Claim> {
        let mut set = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        set.insert(uri.to_string()).then(|| Claim {
            set: Arc::clone(&self.0),
            uri: uri.to_string(),
        })
    }

    fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).len()
    }
}

fn same_origin(a: &str, b: &str) -> bool {
    match (Url::parse(a), Url::parse(b)) {
        (Ok(a), Ok(b)) => a.origin() == b.origin(),
        _ => false,
    }
}

impl Worker {
    #[must_use]
    pub fn new(fetcher: Arc<Fetcher>, writer: WriterHandle, config: WorkerConfig) -> Self {
        Self {
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

    /// Replaces the cached copy of an actor document with a fresh fetch, or drops it when the
    /// fetch did not return the document.
    fn refresh_author(&self, uri: &str, fetched: &Fetched, now: i64) {
        let mut cache = self.authors.lock().unwrap_or_else(PoisonError::into_inner);
        match fetched {
            Fetched::Found(json) => cache.insert(uri, json.clone(), now),
            _ => cache.remove(uri),
        }
    }

    /// The author's actor document, from the cache or a fetch.
    async fn fetch_author(&self, author_uri: &str, hint: &mut Option<i64>) -> Fetched {
        let now = now_secs();
        let cached = self
            .authors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(author_uri, now);
        if let Some(json) = cached {
            return Fetched::Found(json);
        }
        let fetched = self.fetch(author_uri, hint).await;
        if let Fetched::Found(json) = &fetched {
            self.authors
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(author_uri, json.clone(), now);
        }
        fetched
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
        } else {
            self.decide(item, carried, now, &mut hint).await
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
        Processed {
            uri: uri.to_string(),
            actions: to_apply,
            retry,
        }
    }

    async fn decide(
        &self,
        item: &QueuedFetch,
        carried: Option<Carried>,
        now: i64,
        hint: &mut Option<i64>,
    ) -> Vec<Action> {
        let uri = item.uri.as_str();
        let (carried_post, carried_account) = match carried {
            Some(Carried::Post(p)) => (Some(p), None),
            Some(Carried::Account(a)) => (None, Some(a)),
            None => (None, None),
        };
        let fetched = self.fetch(uri, hint).await;
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
                self.decide_post_path(item, carried_post, &fetched, now, hint)
                    .await
            }
            Some(false) => {
                // A freshly fetched actor document is newer than any cached copy: an opt-out
                // announced for this account must reach posts processed after it.
                self.refresh_author(uri, &fetched, now);
                decide_account(
                    uri,
                    item.reason,
                    item.attempts,
                    carried_account,
                    &fetched,
                    now,
                )
            }
            None => {
                // We don't know whether it was a post or an actor, and it is not there to say:
                // remove it as both.
                let post = decide_post(uri, item.reason, item.attempts, None, &fetched, None, now);
                let account = decide_account(uri, item.reason, item.attempts, None, &fetched, now);
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
        now: i64,
        hint: &mut Option<i64>,
    ) -> Vec<Action> {
        let uri = item.uri.as_str();
        let author = match fetched {
            Fetched::Found(json) => match author_of(json) {
                // Consent needs the author on the object's own origin, so an author anywhere
                // else is never fetched: that is also what stops an object from pointing us at
                // arbitrary URLs.
                Some(author_uri) if same_origin(&author_uri, uri) => {
                    Some(self.fetch_author(&author_uri, hint).await)
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
            author.as_ref(),
            now,
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
            store.complete(&item.uri)?;
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
            store.complete(&item.uri)?;
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
        store.fail(&item.uri, &retry.reason, retry_at)?;
        Ok(Finished::Rescheduled { retry_at })
    }

    /// Process everything due, repeatedly, until nothing is. Items rescheduled into the future
    /// stay queued. Returns how many items were processed.
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
    /// `fetch_concurrency` at once and never one URI twice.
    pub async fn run(self: Arc<Self>, store: Store, mut shutdown: watch::Receiver<bool>) {
        let permits = Arc::new(Semaphore::new(self.config.fetch_concurrency));
        let in_flight = InFlight::default();
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
                match store.next_due(now_secs(), free + in_flight.len()) {
                    Ok(items) => {
                        for item in items {
                            let Some(claim) = in_flight.claim(&item.uri) else {
                                continue;
                            };
                            let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                                break;
                            };
                            let worker = Arc::clone(&self);
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
        cache.insert("a", json!(1), 0);
        cache.insert("b", json!(2), 10);
        assert_eq!(cache.get("a", 50), Some(json!(1)));
        // Full: the oldest ("a") goes.
        cache.insert("c", json!(3), 20);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get("a", 20), None);
        assert_eq!(cache.get("b", 20), Some(json!(2)));
        // Expiry: "b" was inserted at 10 with a 100 s ttl.
        assert_eq!(cache.get("b", 110), None);
        assert_eq!(cache.get("c", 110), Some(json!(3)));
        // Making room prefers dropping the expired entries.
        cache.insert("d", json!(4), 125);
        cache.insert("e", json!(5), 126);
        assert_eq!(cache.get("d", 126), Some(json!(4)));
        assert_eq!(cache.get("e", 126), Some(json!(5)));
        assert_eq!(cache.get("c", 126), None);
        // Re-inserting an existing key does not evict.
        cache.insert("d", json!(6), 127);
        assert_eq!(cache.get("e", 127), Some(json!(5)));
        assert_eq!(cache.get("d", 127), Some(json!(6)));
    }

    #[test]
    fn in_flight_claims_are_exclusive_and_released_on_drop() {
        let in_flight = InFlight::default();
        let claim = in_flight.claim("u");
        assert!(claim.is_some());
        assert!(in_flight.claim("u").is_none());
        assert!(in_flight.claim("v").is_some());
        assert_eq!(in_flight.len(), 1);
        drop(claim);
        assert!(in_flight.claim("u").is_some());
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
