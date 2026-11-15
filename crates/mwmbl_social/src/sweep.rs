//! The sweep: walks every page of each index in a cycle, which is how tombstones are applied,
//! stored records are revalidated at least every few days, and tombstones are retired.
//!
//! One page per step, one step every `sweep_period / pages`. The position lives in
//! `sweep_state`, so a restart resumes where it was. The sweep reads and deletes through the
//! writer ([`WriterHandle::sweep_step`]); it only touches SQLite for the position and the
//! fetch queue.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use store::{EnqueueOutcome, FetchKind, FetchReason, NewFetch, Store, StoreError};
use tokio::sync::watch;

use crate::decide::Carried;
use crate::now_secs;
use crate::worker::encode_carried;
use crate::writer::{IndexName, SweepStepOutcome, WriterError, WriterHandle};

#[derive(Debug, thiserror::Error)]
pub enum SweepError {
    #[error(transparent)]
    Writer(#[from] WriterError),
    #[error("sweep of the {index} index: {source}")]
    Store {
        index: &'static str,
        source: StoreError,
    },
}

fn store_err(index: IndexName) -> impl FnOnce(StoreError) -> SweepError {
    move |source| SweepError::Store {
        index: index.as_str(),
        source,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepConfig {
    /// The time between pages.
    pub step_interval: Duration,
    /// A record last checked longer ago than this is revalidated.
    pub revalidate_after_secs: i64,
    pub queue_cap: usize,
}

/// `period / pages`, but never less than a millisecond.
#[must_use]
pub fn step_interval(period_secs: u64, num_pages: usize) -> Duration {
    let pages = u32::try_from(num_pages.max(1)).unwrap_or(u32::MAX);
    (Duration::from_secs(period_secs) / pages).max(Duration::from_millis(1))
}

/// What one step did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StepReport {
    pub page: usize,
    /// Records deleted because a tombstone matched.
    pub deleted: usize,
    /// Revalidations queued.
    pub queued: usize,
    /// Revalidations the full queue turned away.
    pub deferred: usize,
    /// The page could not be read; it was skipped.
    pub skipped: bool,
    /// This step finished a full pass.
    pub wrapped: bool,
    /// Tombstones retired at the end of a pass.
    pub tombstones_cleared: usize,
}

/// A revalidation is spread over up to ten steps' worth of time, by a hash of its URI, so a
/// page full of one host's posts does not become a burst of requests to it.
fn spread_secs(step_interval: Duration) -> i64 {
    i64::try_from(step_interval.as_millis().saturating_mul(10) / 1000).unwrap_or(i64::MAX)
}

/// A deterministic delay in `0..=spread` seconds for `uri`.
#[must_use]
pub fn jitter_secs(uri: &str, spread: i64) -> i64 {
    if spread <= 0 {
        return 0;
    }
    let mut hasher = DefaultHasher::new();
    uri.hash(&mut hasher);
    let span = u64::try_from(spread).unwrap_or(u64::MAX).saturating_add(1);
    i64::try_from(hasher.finish() % span).unwrap_or(0)
}

fn enqueue(
    store: &Store,
    config: &SweepConfig,
    uri: &str,
    kind: FetchKind,
    carried: &Carried,
    now: i64,
    index: IndexName,
) -> Result<EnqueueOutcome, SweepError> {
    let item = NewFetch {
        uri: uri.to_string(),
        kind,
        reason: FetchReason::Revalidate,
        record: Some(encode_carried(carried)),
        not_before: now.saturating_add(jitter_secs(uri, spread_secs(config.step_interval))),
    };
    store
        .enqueue(&item, now, config.queue_cap)
        .map_err(store_err(index))
}

/// Sweep the next page of `index` and advance the position. `num_pages` is the index's size.
pub async fn step(
    index: IndexName,
    num_pages: usize,
    store: &mut Store,
    writer: &WriterHandle,
    config: &SweepConfig,
    now: i64,
) -> Result<StepReport, SweepError> {
    let name = index.as_str();
    let state = store.sweep_state(name, now).map_err(store_err(index))?;
    let page = match usize::try_from(state.current_page) {
        Ok(page) if page < num_pages => page,
        _ => 0,
    };
    let stale_before = now.saturating_sub(config.revalidate_after_secs);
    let mut report = StepReport {
        page,
        ..StepReport::default()
    };

    let outcome = match writer.sweep_step(index, page, stale_before).await {
        Ok(outcome) => Some(outcome),
        // The writer already logged it with the page number. The page is not reset (that
        // would delete what is on it); skipping keeps the rest of the pass moving.
        Err(WriterError::Index { .. }) => {
            report.skipped = true;
            None
        }
        Err(e) => return Err(e.into()),
    };
    let count = |report: &mut StepReport, outcome: EnqueueOutcome| match outcome {
        EnqueueOutcome::Deferred => report.deferred += 1,
        _ => report.queued += 1,
    };
    match outcome {
        Some(SweepStepOutcome::Posts(done)) => {
            report.deleted = done.deleted.len();
            for post in done.revalidate {
                let uri = post.uri.clone();
                let carried = Carried::Post(post);
                let outcome =
                    enqueue(store, config, &uri, FetchKind::Object, &carried, now, index)?;
                count(&mut report, outcome);
            }
        }
        Some(SweepStepOutcome::Accounts(done)) => {
            report.deleted = done.deleted.len();
            for account in done.revalidate {
                let uri = account.uri.clone();
                let carried = Carried::Account(account);
                let outcome = enqueue(store, config, &uri, FetchKind::Actor, &carried, now, index)?;
                count(&mut report, outcome);
            }
        }
        None => {}
    }

    let next = page + 1;
    if next >= num_pages {
        report.wrapped = true;
        store.finish_sweep(name, now).map_err(store_err(index))?;
        report.tombstones_cleared = retire_tombstones(store, writer, now).await?;
        tracing::info!(
            index = name,
            tombstones_cleared = report.tombstones_cleared,
            "sweep pass finished"
        );
    } else {
        let next = u32::try_from(next).unwrap_or(0);
        store.set_sweep_page(name, next).map_err(store_err(index))?;
    }
    Ok(report)
}

/// Clear the tombstones that every page has been swept since. A tombstone made before the
/// start of the last completed pass of *both* indexes was applied by those passes, so the
/// cutoff is the older of the two start times.
async fn retire_tombstones(
    store: &mut Store,
    writer: &WriterHandle,
    now: i64,
) -> Result<usize, SweepError> {
    let posts = store
        .sweep_state(IndexName::Posts.as_str(), now)
        .map_err(store_err(IndexName::Posts))?;
    let accounts = store
        .sweep_state(IndexName::Accounts.as_str(), now)
        .map_err(store_err(IndexName::Accounts))?;
    match (posts.last_sweep_started_at, accounts.last_sweep_started_at) {
        (Some(posts), Some(accounts)) => {
            Ok(writer.clear_tombstones_before(posts.min(accounts)).await?)
        }
        _ => Ok(0),
    }
}

/// Sweep one index forever, a page per `config.step_interval`, until shutdown.
pub async fn run_index(
    index: IndexName,
    num_pages: usize,
    mut store: Store,
    writer: WriterHandle,
    config: SweepConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(config.step_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = shutdown.changed() => {}
        }
        if *shutdown.borrow() {
            break;
        }
        if let Err(e) = step(index, num_pages, &mut store, &writer, &config, now_secs()).await {
            tracing::error!(index = index.as_str(), error = %e, "sweep step failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_intervals() {
        assert_eq!(step_interval(6 * 86_400, 65_536).as_secs(), 7);
        assert_eq!(step_interval(100, 10), Duration::from_secs(10));
        assert_eq!(step_interval(1, 1_000_000), Duration::from_millis(1));
        assert_eq!(step_interval(0, 10), Duration::from_millis(1));
        assert_eq!(step_interval(10, 0), Duration::from_secs(10));
    }

    #[test]
    fn jitter_is_deterministic_and_bounded() {
        assert_eq!(jitter_secs("u", 0), 0);
        assert_eq!(jitter_secs("u", -3), 0);
        for uri in ["a", "b", "https://h.example/1", "https://h.example/2"] {
            let j = jitter_secs(uri, 79);
            assert!((0..=79).contains(&j));
            assert_eq!(j, jitter_secs(uri, 79));
        }
        let distinct: std::collections::HashSet<i64> = (0..50)
            .map(|i| jitter_secs(&format!("https://h.example/{i}"), 79))
            .collect();
        assert!(distinct.len() > 10, "spreads: {distinct:?}");
    }

    #[test]
    fn spread_is_ten_steps() {
        assert_eq!(spread_secs(Duration::from_secs(7)), 70);
        assert_eq!(spread_secs(Duration::from_millis(1)), 0);
        assert_eq!(spread_secs(Duration::from_millis(250)), 2);
    }
}
