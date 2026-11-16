//! The worker's checks on a crawl submission before it is fetched (PLAN.md Stage 3 item 3): the
//! host's robots.txt, cached in memory and in `hosts`. Whether the host runs Lemmy comes from
//! [`crate::context::ContextSource`], which caches nodeinfo `software`.
//!
//! robots.txt is fetched through the [`Fetcher`], so under its per-host limits and backoff. Per
//! RFC 9309 a 4xx answer other than 429 means no rules (everything allowed), and a 429, a 5xx
//! or a failed fetch means everything is disallowed until it is fetched again, after
//! [`ROBOTS_RETRY_SECS`]. Rules are trusted for [`ROBOTS_TTL_SECS`]. Only the rules that apply
//! to our product token are kept ([`crate::robots`]).
//!
//! Only crawl submissions are checked. Fetches for FASP announcements, follows and
//! revalidation are federation, not crawling.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use apub::fetch::{Fetcher, TextOutcome, uri_host_key};
use store::{HostRobots, Store};
use url::Url;

use crate::robots::{self, Robots};

/// How long a host's robots.txt is trusted before it is fetched again.
pub const ROBOTS_TTL_SECS: i64 = 24 * 3600;
/// How long after robots.txt was unavailable it is tried again; until then the host is
/// disallowed.
pub const ROBOTS_RETRY_SECS: i64 = 3600;
/// How much of a robots.txt is read. RFC 9309 asks crawlers to parse at least 500 KiB.
pub const MAX_ROBOTS_BYTES: usize = 512 * 1024;
/// Hosts whose robots.txt is held in memory in front of SQLite.
const MEMORY_CAPACITY: usize = 10_000;

/// What robots.txt says about a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RobotsVerdict {
    /// The rules that apply to us (none: everything allowed).
    Rules(Arc<Robots>),
    /// robots.txt was unavailable: everything is disallowed until `retry_at` (Unix seconds).
    Unavailable { retry_at: i64 },
}

#[derive(Debug, Clone)]
struct Cached {
    /// `None`: unavailable.
    robots: Option<Arc<Robots>>,
    checked_at: i64,
}

impl Cached {
    fn is_fresh(&self, now: i64) -> bool {
        now < self.checked_at.saturating_add(ttl(self.robots.is_some()))
    }

    fn verdict(&self) -> RobotsVerdict {
        match &self.robots {
            Some(robots) => RobotsVerdict::Rules(Arc::clone(robots)),
            None => RobotsVerdict::Unavailable {
                retry_at: self.checked_at.saturating_add(ROBOTS_RETRY_SECS),
            },
        }
    }
}

fn ttl(available: bool) -> i64 {
    if available {
        ROBOTS_TTL_SECS
    } else {
        ROBOTS_RETRY_SECS
    }
}

/// Whether a stored robots.txt fetch is recent enough to use.
#[must_use]
pub fn is_fresh(stored: &HostRobots, now: i64) -> bool {
    now < stored
        .checked_at
        .saturating_add(ttl(stored.robots_txt.is_some()))
}

/// The robots.txt URL for `url`'s origin.
#[must_use]
pub fn robots_url(url: &Url) -> Option<Url> {
    url.host_str()?;
    url.join("/robots.txt").ok()
}

/// What a robots.txt fetch means: the rules for `token` as stored text (empty: none), or
/// `None` when it was unavailable.
#[must_use]
pub fn stored_form(outcome: &TextOutcome, token: &str) -> Option<String> {
    match outcome {
        TextOutcome::Found(body) => {
            Some(robots::parse(&String::from_utf8_lossy(body), token).to_text())
        }
        TextOutcome::ClientError { .. } => Some(String::new()),
        TextOutcome::Failed(_) => None,
    }
}

/// robots.txt per host for crawl submissions. Shares the worker's SQLite connection.
pub struct RobotsSource {
    fetcher: Arc<Fetcher>,
    store: Arc<Mutex<Store>>,
    token: String,
    memory: Mutex<HashMap<String, Cached>>,
    /// One robots.txt fetch per host at a time: the others wait for it and use its result.
    fetching: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl std::fmt::Debug for RobotsSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RobotsSource")
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

impl RobotsSource {
    /// The product token matched against `User-agent` lines is the fetcher's own User-Agent's.
    #[must_use]
    pub fn new(fetcher: Arc<Fetcher>, store: Arc<Mutex<Store>>) -> Self {
        let token = robots::product_token(fetcher.user_agent());
        Self {
            fetcher,
            store,
            token,
            memory: Mutex::new(HashMap::new()),
            fetching: Mutex::new(HashMap::new()),
        }
    }

    fn memory(&self) -> std::sync::MutexGuard<'_, HashMap<String, Cached>> {
        self.memory.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The `Crawl-delay` a host asked of us, if its robots.txt is in memory. For pacing, which
    /// cannot wait for a fetch; the first crawl fetch to a host loads it.
    #[must_use]
    pub fn crawl_delay_secs(&self, host: &str) -> Option<u64> {
        self.memory()
            .get(host)
            .and_then(|c| c.robots.as_ref())
            .and_then(|r| r.crawl_delay_secs())
    }

    /// What robots.txt says about `url`'s host: from memory, SQLite, or a fetch (one per host
    /// at a time).
    pub async fn verdict(&self, url: &Url, now: i64) -> RobotsVerdict {
        let unavailable = RobotsVerdict::Unavailable {
            retry_at: now.saturating_add(ROBOTS_RETRY_SECS),
        };
        let Some(host) = uri_host_key(url.as_str()) else {
            return unavailable;
        };
        if let Some(cached) = self.cached(&host, now) {
            return cached.verdict();
        }
        let lock = Arc::clone(
            self.fetching
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(host.clone())
                .or_default(),
        );
        let verdict = {
            let _fetching = lock.lock().await;
            // Whoever held the lock may have fetched it meanwhile.
            match self.cached(&host, now) {
                Some(cached) => cached.verdict(),
                None => self.fetch(url, &host, now).await,
            }
        };
        let mut fetching = self.fetching.lock().unwrap_or_else(PoisonError::into_inner);
        // Two references: the map's and ours. Nobody else is waiting.
        if Arc::strong_count(&lock) <= 2 {
            fetching.remove(&host);
        }
        verdict
    }

    async fn fetch(&self, url: &Url, host: &str, now: i64) -> RobotsVerdict {
        let Some(robots_url) = robots_url(url) else {
            return RobotsVerdict::Unavailable {
                retry_at: now.saturating_add(ROBOTS_RETRY_SECS),
            };
        };
        let outcome = self
            .fetcher
            .fetch_text(robots_url.as_str(), MAX_ROBOTS_BYTES)
            .await;
        let stored = stored_form(&outcome, &self.token);
        match (&stored, &outcome) {
            (Some(_), _) => tracing::debug!(host, "fetched robots.txt"),
            (None, TextOutcome::Failed(failure)) => {
                tracing::info!(host, outcome = ?failure, "robots.txt unavailable; not crawling the host for now");
            }
            (None, _) => {}
        }
        if let Err(e) = self
            .store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .set_host_robots(host, stored.as_deref(), now)
        {
            tracing::error!(host, error = %e, "failed to record robots.txt");
        }
        let cached = Cached {
            robots: stored.map(|text| Arc::new(robots::parse(&text, &self.token))),
            checked_at: now,
        };
        let verdict = cached.verdict();
        self.remember(host, cached, now);
        verdict
    }

    /// A fresh entry from memory, else from SQLite (which then fills memory).
    fn cached(&self, host: &str, now: i64) -> Option<Cached> {
        if let Some(entry) = self.memory().get(host).filter(|c| c.is_fresh(now)) {
            return Some(entry.clone());
        }
        let stored = self
            .store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .host_robots(host);
        let stored = match stored {
            Ok(stored) => stored,
            Err(e) => {
                tracing::error!(host, error = %e, "failed to read robots.txt");
                None
            }
        };
        let stored = stored.filter(|s| is_fresh(s, now))?;
        let cached = Cached {
            // The stored form is already reduced to the rules for us; parsing it again for our
            // token reads it back the same.
            robots: stored
                .robots_txt
                .map(|text| Arc::new(robots::parse(&text, &self.token))),
            checked_at: stored.checked_at,
        };
        self.remember(host, cached.clone(), now);
        Some(cached)
    }

    fn remember(&self, host: &str, entry: Cached, now: i64) {
        let mut memory = self.memory();
        if memory.len() >= MEMORY_CAPACITY && !memory.contains_key(host) {
            memory.retain(|_, e| e.is_fresh(now));
            if memory.len() >= MEMORY_CAPACITY {
                // Every entry is fresh: start over. SQLite still has them all.
                memory.clear();
            }
        }
        memory.insert(host.to_owned(), entry);
    }
}

#[cfg(test)]
mod tests {
    use apub::fetch::{FetchError, FetchOutcome};

    use super::*;

    #[test]
    fn fetch_outcomes_follow_rfc_9309() {
        let found = TextOutcome::Found(b"User-agent: *\nDisallow: /a\nCrawl-delay: 9\n".to_vec());
        assert_eq!(
            stored_form(&found, "mwmbl-social").as_deref(),
            Some("User-agent: *\nDisallow: /a\nCrawl-delay: 9\n")
        );
        // Not for us: no rules.
        let other = TextOutcome::Found(b"User-agent: other\nDisallow: /\n".to_vec());
        assert_eq!(stored_form(&other, "mwmbl-social").as_deref(), Some(""));
        // A 4xx means there are no rules; anything else unavailable means disallow.
        for status in [401, 403, 404, 410] {
            assert_eq!(
                stored_form(&TextOutcome::ClientError { status }, "x").as_deref(),
                Some("")
            );
        }
        for failed in [
            FetchOutcome::RateLimited {
                retry_after_secs: 5,
            },
            FetchOutcome::Backoff { until: 5 },
            FetchOutcome::Error(FetchError::Timeout {
                uri: "https://l.example/robots.txt".into(),
            }),
        ] {
            assert_eq!(stored_form(&TextOutcome::Failed(failed), "x"), None);
        }
        // Not UTF-8: read lossily rather than given up on.
        let latin1 = TextOutcome::Found(b"User-agent: *\nDisallow: /caf\xe9\n".to_vec());
        assert!(stored_form(&latin1, "x").is_some_and(|t| t.contains("Disallow: /caf")));
    }

    #[test]
    fn freshness() {
        let rules = HostRobots {
            robots_txt: Some(String::new()),
            checked_at: 1000,
        };
        let unavailable = HostRobots {
            robots_txt: None,
            checked_at: 1000,
        };
        assert!(is_fresh(&rules, 1000 + ROBOTS_TTL_SECS - 1));
        assert!(!is_fresh(&rules, 1000 + ROBOTS_TTL_SECS));
        assert!(is_fresh(&unavailable, 1000 + ROBOTS_RETRY_SECS - 1));
        assert!(!is_fresh(&unavailable, 1000 + ROBOTS_RETRY_SECS));
    }

    #[test]
    fn the_robots_url_is_on_the_origin() -> Result<(), url::ParseError> {
        let url = |s: &str| Url::parse(s);
        assert_eq!(
            robots_url(&url("https://l.example/post/1?x=1#y")?).map(String::from),
            Some("https://l.example/robots.txt".into())
        );
        assert_eq!(
            robots_url(&url("http://127.0.0.1:8080/a/b")?).map(String::from),
            Some("http://127.0.0.1:8080/robots.txt".into())
        );
        assert_eq!(robots_url(&url("mailto:a@b.example")?), None);
        Ok(())
    }
}
