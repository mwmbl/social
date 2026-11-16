//! Working out an actor's [`ConsentContext`]: whether they follow our actor (`follow_optins`)
//! and whether their own host runs Lemmy (nodeinfo `software.name`, cached in `hosts`).
//!
//! The context only stands in for consent flags an actor doesn't carry, so an actor with both
//! `discoverable` and `indexable` gets [`ConsentContext::NONE`] at no cost: no software lookup
//! and no query. The rule follows the author, so the host looked up is always the actor's own,
//! never a community's or an object's.
//!
//! Software is cached in memory and in `hosts`: a found name for [`SOFTWARE_TTL_SECS`], a
//! failed lookup (`software = NULL`) for [`SOFTWARE_RETRY_SECS`]. Until a failed lookup is
//! retried the host counts as not Lemmy: when in doubt, don't index. A failed lookup for a host
//! whose software we already knew keeps that name, retried after [`SOFTWARE_RETRY_SECS`], so a
//! brief nodeinfo outage during revalidation doesn't drop a Lemmy host's content (a host does
//! not plausibly change software under the same name). The context is worked out
//! afresh on every fetch of the actor, so revalidation picks up an unfollow or a software change.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use apub::consent::ConsentContext;
use apub::fetch::{Fetcher, uri_host_key};
use apub::nodeinfo::lookup_software;
use serde_json::Value;
use store::{HostSoftware, Store};

/// How long a host's software name is trusted before it is looked up again.
pub const SOFTWARE_TTL_SECS: i64 = 24 * 3600;
/// How long after a failed lookup it is tried again.
pub const SOFTWARE_RETRY_SECS: i64 = 3600;
/// Hosts whose software is held in memory in front of SQLite.
const MEMORY_CAPACITY: usize = 10_000;

/// The software name Lemmy reports in nodeinfo.
const LEMMY: &str = "lemmy";

/// Whether the actor lacks `discoverable` or `indexable` altogether, so that a context could
/// stand in for it. A flag that is present, whatever its value, needs no context.
#[must_use]
pub fn lacks_a_flag(actor: &Value) -> bool {
    actor.get("discoverable").is_none() || actor.get("indexable").is_none()
}

/// Whether a recorded lookup is recent enough to use.
#[must_use]
pub fn is_fresh(entry: &HostSoftware, now: i64) -> bool {
    let ttl = if entry.software.is_some() {
        SOFTWARE_TTL_SECS
    } else {
        SOFTWARE_RETRY_SECS
    };
    now < entry.checked_at.saturating_add(ttl)
}

/// The entry to record after a failed lookup, given what was known before: a known name is kept
/// but counts as fresh for only [`SOFTWARE_RETRY_SECS`]; otherwise the host is unknown.
#[must_use]
pub fn after_failure(previous: Option<HostSoftware>, now: i64) -> HostSoftware {
    match previous.and_then(|p| p.software) {
        Some(name) => HostSoftware {
            software: Some(name),
            checked_at: now
                .saturating_sub(SOFTWARE_TTL_SECS)
                .saturating_add(SOFTWARE_RETRY_SECS),
        },
        None => HostSoftware {
            software: None,
            checked_at: now,
        },
    }
}

/// Works out consent contexts for the worker. Holds its own SQLite connection.
pub struct ContextSource {
    fetcher: Arc<Fetcher>,
    store: Arc<Mutex<Store>>,
    software: Mutex<HashMap<String, HostSoftware>>,
}

impl std::fmt::Debug for ContextSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextSource").finish_non_exhaustive()
    }
}

impl ContextSource {
    #[must_use]
    pub fn new(fetcher: Arc<Fetcher>, store: Store) -> Self {
        Self {
            fetcher,
            store: Arc::new(Mutex::new(store)),
            software: Mutex::new(HashMap::new()),
        }
    }

    /// The SQLite connection, to share with [`crate::crawl::RobotsSource`].
    #[must_use]
    pub fn store_handle(&self) -> Arc<Mutex<Store>> {
        Arc::clone(&self.store)
    }

    /// Whether the host of `uri` runs Lemmy, by its nodeinfo `software.name` (cached as for
    /// consent contexts). A failed lookup counts as no.
    pub async fn host_runs_lemmy(&self, uri: &str, now: i64) -> bool {
        self.software_of(uri, now).await.as_deref() == Some(LEMMY)
    }

    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The context for the actor at `actor_uri`, whose fetched document is `actor`.
    pub async fn context_for(&self, actor_uri: &str, actor: &Value, now: i64) -> ConsentContext {
        if !lacks_a_flag(actor) {
            return ConsentContext::NONE;
        }
        let follows_us = self.store().is_follow_optin(actor_uri).unwrap_or_else(|e| {
            tracing::error!(uri = actor_uri, error = %e, "failed to read follow opt-ins");
            false
        });
        let author_on_lemmy = self.software_of(actor_uri, now).await.as_deref() == Some(LEMMY);
        ConsentContext {
            author_on_lemmy,
            follows_us,
        }
    }

    /// The software the host of `uri` runs, from the caches or a lookup. `None` if unknown.
    async fn software_of(&self, uri: &str, now: i64) -> Option<String> {
        let host = uri_host_key(uri)?;
        if let Some(entry) = self.cached(&host, now) {
            return entry.software;
        }
        let entry = match lookup_software(&self.fetcher, uri).await {
            Ok(name) => {
                tracing::debug!(host, software = name, "looked up host software");
                HostSoftware {
                    software: Some(name),
                    checked_at: now,
                }
            }
            Err(e) => {
                tracing::debug!(host, error = %e, "host software lookup failed");
                after_failure(self.any_age(&host), now)
            }
        };
        if let Err(e) =
            self.store()
                .set_host_software(&host, entry.software.as_deref(), entry.checked_at)
        {
            tracing::error!(host, error = %e, "failed to record host software");
        }
        let software = entry.software.clone();
        self.remember(&host, entry, now);
        software
    }

    /// Whatever is known about `host`, fresh or not.
    fn any_age(&self, host: &str) -> Option<HostSoftware> {
        let in_memory = self
            .software
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(host)
            .cloned();
        in_memory.or_else(|| self.store().host_software(host).ok().flatten())
    }

    /// A fresh entry from memory, else from SQLite (which then fills memory).
    fn cached(&self, host: &str, now: i64) -> Option<HostSoftware> {
        let in_memory = self
            .software
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(host)
            .cloned();
        if let Some(entry) = in_memory.filter(|e| is_fresh(e, now)) {
            return Some(entry);
        }
        let stored = match self.store().host_software(host) {
            Ok(stored) => stored,
            Err(e) => {
                tracing::error!(host, error = %e, "failed to read host software");
                None
            }
        };
        let entry = stored.filter(|e| is_fresh(e, now))?;
        self.remember(host, entry.clone(), now);
        Some(entry)
    }

    fn remember(&self, host: &str, entry: HostSoftware, now: i64) {
        let mut memory = self.software.lock().unwrap_or_else(PoisonError::into_inner);
        if memory.len() >= MEMORY_CAPACITY && !memory.contains_key(host) {
            memory.retain(|_, e| is_fresh(e, now));
            if memory.len() >= MEMORY_CAPACITY {
                // Every entry is fresh: start over rather than track recency. SQLite still has
                // them all.
                memory.clear();
            }
        }
        memory.insert(host.to_string(), entry);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn only_a_missing_flag_needs_a_context() {
        let cases = [
            (json!({"discoverable": true, "indexable": true}), false),
            (json!({"discoverable": false, "indexable": false}), false),
            (json!({"discoverable": null, "indexable": true}), false),
            (json!({"discoverable": true}), true),
            (json!({"indexable": true}), true),
            (json!({}), true),
        ];
        for (actor, expected) in cases {
            assert_eq!(lacks_a_flag(&actor), expected, "{actor}");
        }
    }

    #[test]
    fn a_failed_lookup_keeps_a_known_name_for_a_retry_period() {
        let now = 1_000_000;
        let known = HostSoftware {
            software: Some("lemmy".into()),
            checked_at: now - 2 * SOFTWARE_TTL_SECS,
        };
        let kept = after_failure(Some(known), now);
        assert_eq!(kept.software.as_deref(), Some("lemmy"));
        assert!(is_fresh(&kept, now));
        assert!(is_fresh(&kept, now + SOFTWARE_RETRY_SECS - 1));
        assert!(!is_fresh(&kept, now + SOFTWARE_RETRY_SECS));
        let unknown = HostSoftware {
            software: None,
            checked_at: now - 10,
        };
        assert_eq!(
            after_failure(Some(unknown), now),
            HostSoftware {
                software: None,
                checked_at: now
            }
        );
        assert_eq!(after_failure(None, now).software, None);
    }

    #[test]
    fn freshness() {
        let found = HostSoftware {
            software: Some("lemmy".into()),
            checked_at: 1000,
        };
        let failed = HostSoftware {
            software: None,
            checked_at: 1000,
        };
        assert!(is_fresh(&found, 1000));
        assert!(is_fresh(&found, 1000 + SOFTWARE_TTL_SECS - 1));
        assert!(!is_fresh(&found, 1000 + SOFTWARE_TTL_SECS));
        assert!(is_fresh(&failed, 1000 + SOFTWARE_RETRY_SECS - 1));
        assert!(!is_fresh(&failed, 1000 + SOFTWARE_RETRY_SECS));
    }
}
