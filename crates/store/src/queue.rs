//! The `fetch_queue` table: bounded, deduplicated by URI, announcements first.

use std::collections::BTreeMap;

use rusqlite::{OptionalExtension, Row, params};

use crate::enums::{FetchKind, FetchReason};
use crate::error::{ctx, to_usize};
use crate::{Store, StoreError};

/// Crawl submissions are only admitted while the queue holds fewer rows than this percentage
/// of its cap, so they never take the room announcements and backfills need.
pub const CRAWL_QUEUE_PERCENT: usize = 50;

/// Something to add to the fetch queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewFetch {
    pub uri: String,
    pub kind: FetchKind,
    pub reason: FetchReason,
    /// Opaque JSON of the stored index record that a revalidation carries.
    ///
    /// This may only ever hold what the index already holds (a few hundred bytes of already
    /// public, already consented data). Never put non-public object content here.
    pub record: Option<String>,
    /// Earliest time (Unix seconds) the item may be fetched.
    pub not_before: i64,
}

/// What [`Store::enqueue`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// A new row was inserted.
    Added,
    /// The URI was queued already; nothing about its schedule changed.
    AlreadyQueued,
    /// The URI was queued already and was moved to a higher priority.
    Upgraded,
    /// The queue is full (for a crawl submission: at [`CRAWL_QUEUE_PERCENT`] of the cap) and
    /// this backfill, revalidation or crawl item was not inserted.
    Deferred,
}

/// A row of the fetch queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedFetch {
    pub uri: String,
    pub kind: FetchKind,
    pub reason: FetchReason,
    /// 0 is most urgent (announce), then backfill 1, revalidate 2, crawl 3.
    pub priority: i64,
    /// Opaque JSON of the stored index record. Only what the index already holds; never
    /// non-public object content.
    pub record: Option<String>,
    pub not_before: i64,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub enqueued_at: i64,
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<QueuedFetch> {
    Ok(QueuedFetch {
        uri: row.get(0)?,
        kind: row.get(1)?,
        reason: row.get(2)?,
        priority: row.get(3)?,
        record: row.get(4)?,
        not_before: row.get(5)?,
        attempts: row.get(6)?,
        last_error: row.get(7)?,
        enqueued_at: row.get(8)?,
    })
}

impl Store {
    /// Adds `item` to the queue, deduplicating by URI and keeping the queue near `cap` rows.
    ///
    /// - Already queued: one row stays. If the new item has a higher priority (lower number)
    ///   the row takes that priority, reason and kind and the earlier `not_before`
    ///   ([`EnqueueOutcome::Upgraded`]); otherwise [`EnqueueOutcome::AlreadyQueued`]. An existing
    ///   `record` is never dropped, and a missing one is filled from the new item.
    /// - Queue at or over `cap`: an announce is still admitted, evicting the lowest-priority,
    ///   newest row with priority above 0 (or none, if there is no such row). Backfill and
    ///   revalidate items are [`EnqueueOutcome::Deferred`].
    /// - A crawl item is [`EnqueueOutcome::Deferred`] once the queue holds
    ///   [`CRAWL_QUEUE_PERCENT`] of `cap`.
    pub fn enqueue(
        &self,
        item: &NewFetch,
        now: i64,
        cap: usize,
    ) -> Result<EnqueueOutcome, StoreError> {
        let op = format!("enqueue {}", item.uri);
        let tx = self.write_transaction().map_err(ctx(op.clone()))?;
        let priority = item.reason.priority();

        let existing: Option<(i64, i64)> = tx
            .query_row(
                "SELECT priority, not_before FROM fetch_queue WHERE uri = ?1",
                [&item.uri],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(ctx(op.clone()))?;

        let outcome = if let Some((old_priority, old_not_before)) = existing {
            if priority < old_priority {
                tx.execute(
                    "UPDATE fetch_queue SET priority = ?2, reason = ?3, kind = ?4,
                         not_before = ?5, record = COALESCE(record, ?6)
                     WHERE uri = ?1",
                    params![
                        item.uri,
                        priority,
                        item.reason,
                        item.kind,
                        item.not_before.min(old_not_before),
                        item.record
                    ],
                )
                .map_err(ctx(op.clone()))?;
                EnqueueOutcome::Upgraded
            } else {
                // A request of the same priority can bring the fetch forward but never push it
                // back: a delete announced after a delayed update must not wait for the
                // update's delay. A lower-priority request changes nothing.
                let not_before = if priority == old_priority {
                    item.not_before.min(old_not_before)
                } else {
                    old_not_before
                };
                tx.execute(
                    "UPDATE fetch_queue SET record = COALESCE(record, ?2), not_before = ?3
                     WHERE uri = ?1",
                    params![item.uri, item.record, not_before],
                )
                .map_err(ctx(op.clone()))?;
                EnqueueOutcome::AlreadyQueued
            }
        } else {
            let len: i64 = tx
                .query_row("SELECT COUNT(*) FROM fetch_queue", [], |r| r.get(0))
                .map_err(ctx(op.clone()))?;
            let len = to_usize(len, "enqueue")?;
            let full = len >= cap;
            let crawl_full = item.reason == FetchReason::Crawl
                && len.saturating_mul(100) >= cap.saturating_mul(CRAWL_QUEUE_PERCENT);
            if (full && item.reason != FetchReason::Announce) || crawl_full {
                EnqueueOutcome::Deferred
            } else {
                if full {
                    tx.execute(
                        "DELETE FROM fetch_queue WHERE uri = (
                             SELECT uri FROM fetch_queue WHERE priority > 0
                             ORDER BY priority DESC, enqueued_at DESC, rowid DESC LIMIT 1)",
                        [],
                    )
                    .map_err(ctx(op.clone()))?;
                }
                tx.execute(
                    "INSERT INTO fetch_queue
                        (uri, kind, reason, priority, record, not_before, attempts, enqueued_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)",
                    params![
                        item.uri,
                        item.kind,
                        item.reason,
                        priority,
                        item.record,
                        item.not_before,
                        now
                    ],
                )
                .map_err(ctx(op.clone()))?;
                EnqueueOutcome::Added
            }
        };
        tx.commit().map_err(ctx(op))?;
        Ok(outcome)
    }

    /// Items whose `not_before` has passed, most urgent first.
    pub fn next_due(&self, now: i64, limit: usize) -> Result<Vec<QueuedFetch>, StoreError> {
        let op = "list due fetches";
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = self
            .conn
            .prepare(
                "SELECT uri, kind, reason, priority, record, not_before, attempts, last_error,
                        enqueued_at
                 FROM fetch_queue WHERE not_before <= ?1
                 ORDER BY priority, not_before, enqueued_at, uri LIMIT ?2",
            )
            .map_err(ctx(op))?;
        stmt.query_map(params![now, limit], from_row)
            .map_err(ctx(op))?
            .collect::<Result<_, _>>()
            .map_err(ctx(op))
    }

    /// Removes a finished item. Returns whether it was queued.
    pub fn complete(&self, uri: &str) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .execute("DELETE FROM fetch_queue WHERE uri = ?1", [uri])
            .map_err(ctx(format!("complete fetch {uri}")))?;
        Ok(changed > 0)
    }

    /// Records a failed attempt: bumps `attempts`, stores `error` and reschedules to
    /// `retry_at`. Returns whether the URI was queued.
    pub fn fail(&self, uri: &str, error: &str, retry_at: i64) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .execute(
                "UPDATE fetch_queue
                 SET attempts = attempts + 1, last_error = ?2, not_before = ?3
                 WHERE uri = ?1",
                params![uri, error, retry_at],
            )
            .map_err(ctx(format!("fail fetch {uri}")))?;
        Ok(changed > 0)
    }

    /// Moves an item to `not_before` without counting a failed attempt: `attempts` and
    /// `last_error` are untouched. Only applies while the row still has `reason`, so an item
    /// upgraded since it was read (an announce for a URI queued for revalidation) keeps its new
    /// schedule. Returns whether a row was moved.
    pub fn defer(
        &self,
        uri: &str,
        reason: FetchReason,
        not_before: i64,
    ) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .execute(
                "UPDATE fetch_queue SET not_before = ?3 WHERE uri = ?1 AND reason = ?2",
                params![uri, reason, not_before],
            )
            .map_err(ctx(format!("defer fetch {uri}")))?;
        Ok(changed > 0)
    }

    pub fn queue_len(&self) -> Result<usize, StoreError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM fetch_queue", [], |r| r.get(0))
            .map_err(ctx("count fetch queue"))?;
        to_usize(n, "count fetch queue")
    }

    /// Queue length per reason (`announce`, `backfill`, `revalidate`, `crawl`); absent reasons are
    /// omitted.
    pub fn queue_len_by_reason(&self) -> Result<BTreeMap<String, usize>, StoreError> {
        let op = "count fetch queue by reason";
        let mut stmt = self
            .conn
            .prepare("SELECT reason, COUNT(*) FROM fetch_queue GROUP BY reason")
            .map_err(ctx(op))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(ctx(op))?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (reason, n) = row.map_err(ctx(op))?;
            out.insert(reason, to_usize(n, op)?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_duplicate_brings_the_fetch_forward_but_never_back() {
        let store = Store::open_in_memory().unwrap();
        let item = |not_before| NewFetch {
            uri: "https://h.example/s/1".into(),
            kind: FetchKind::Object,
            reason: FetchReason::Announce,
            record: None,
            not_before,
        };
        store.enqueue(&item(500), 0, 10).unwrap();
        assert_eq!(
            store.enqueue(&item(100), 0, 10).unwrap(),
            EnqueueOutcome::AlreadyQueued
        );
        assert_eq!(store.next_due(100, 10).unwrap().len(), 1);
        store.enqueue(&item(900), 0, 10).unwrap();
        assert_eq!(store.next_due(100, 10).unwrap().len(), 1, "not pushed back");
    }

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn item(uri: &str, reason: FetchReason, not_before: i64) -> NewFetch {
        NewFetch {
            uri: uri.to_owned(),
            kind: FetchKind::Object,
            reason,
            record: None,
            not_before,
        }
    }

    fn uris(v: &[QueuedFetch]) -> Vec<&str> {
        v.iter().map(|q| q.uri.as_str()).collect()
    }

    /// Another connection holds the write lock while `enqueue` reads, then commits. A deferred
    /// transaction can't upgrade past that commit and fails with "database is locked" at once
    /// (an announced delete was lost this way); an immediate one waits for the lock instead.
    #[test]
    fn enqueue_waits_for_a_writer_that_commits_meanwhile() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("s.db");
        Store::open(&path)?.enqueue(&item("seed", FetchReason::Announce, 0), 0, 10)?;
        let other = rusqlite::Connection::open(&path)?;
        other.execute_batch("BEGIN IMMEDIATE; DELETE FROM fetch_queue WHERE uri = 'seed';")?;
        let enqueuer = {
            let path = path.clone();
            std::thread::spawn(move || -> Result<EnqueueOutcome, StoreError> {
                Store::open(&path)?.enqueue(&item("u", FetchReason::Announce, 0), 0, 10)
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(300));
        other.execute_batch("COMMIT")?;
        let outcome = enqueuer.join().map_err(|_| "enqueue thread panicked")?;
        assert_eq!(outcome?, EnqueueOutcome::Added);
        assert_eq!(uris(&Store::open(&path)?.next_due(0, 10)?), ["u"]);
        Ok(())
    }

    #[test]
    fn add_and_dedupe() -> TestResult {
        let s = Store::open_in_memory()?;
        let a = item("https://x/1", FetchReason::Backfill, 10);
        assert_eq!(s.enqueue(&a, 1, 100)?, EnqueueOutcome::Added);
        assert_eq!(s.enqueue(&a, 2, 100)?, EnqueueOutcome::AlreadyQueued);
        // Same or lower priority is not an upgrade.
        let lower = item("https://x/1", FetchReason::Revalidate, 0);
        assert_eq!(s.enqueue(&lower, 3, 100)?, EnqueueOutcome::AlreadyQueued);
        assert_eq!(s.queue_len()?, 1);
        let q = s.next_due(10, 10)?;
        let row = q.first().ok_or("empty")?;
        assert_eq!((row.priority, row.not_before, row.enqueued_at), (1, 10, 1));
        assert_eq!(row.reason, FetchReason::Backfill);
        Ok(())
    }

    #[test]
    fn upgrade_takes_priority_and_earlier_not_before() -> TestResult {
        let s = Store::open_in_memory()?;
        s.enqueue(&item("u", FetchReason::Revalidate, 500), 1, 100)?;
        let up = NewFetch {
            kind: FetchKind::Actor,
            ..item("u", FetchReason::Announce, 20)
        };
        assert_eq!(s.enqueue(&up, 2, 100)?, EnqueueOutcome::Upgraded);
        let row = s.next_due(1000, 10)?.into_iter().next().ok_or("empty")?;
        assert_eq!(row.priority, 0);
        assert_eq!(row.reason, FetchReason::Announce);
        assert_eq!(row.kind, FetchKind::Actor);
        assert_eq!(row.not_before, 20);
        assert_eq!(row.enqueued_at, 1);
        // Upgrade never pushes not_before later.
        s.enqueue(&item("v", FetchReason::Revalidate, 5), 1, 100)?;
        s.enqueue(&item("v", FetchReason::Announce, 50), 1, 100)?;
        let v = s.next_due(1000, 10)?;
        assert_eq!(
            v.iter().find(|q| q.uri == "v").map(|q| q.not_before),
            Some(5)
        );
        assert_eq!(s.queue_len()?, 2);
        Ok(())
    }

    #[test]
    fn record_is_kept_and_filled() -> TestResult {
        let s = Store::open_in_memory()?;
        let with = NewFetch {
            record: Some("{\"a\":1}".into()),
            ..item("u", FetchReason::Revalidate, 0)
        };
        s.enqueue(&with, 1, 100)?;
        // Upgrade without a record keeps the existing one.
        s.enqueue(&item("u", FetchReason::Announce, 0), 2, 100)?;
        let row = s.next_due(0, 1)?.into_iter().next().ok_or("empty")?;
        assert_eq!(row.record.as_deref(), Some("{\"a\":1}"));
        // A different record does not replace it.
        let other = NewFetch {
            record: Some("{\"b\":2}".into()),
            ..item("u", FetchReason::Announce, 0)
        };
        assert_eq!(s.enqueue(&other, 3, 100)?, EnqueueOutcome::AlreadyQueued);
        let row = s.next_due(0, 1)?.into_iter().next().ok_or("empty")?;
        assert_eq!(row.record.as_deref(), Some("{\"a\":1}"));
        // A missing record is filled from a later duplicate.
        s.enqueue(&item("w", FetchReason::Backfill, 0), 1, 100)?;
        let fill = NewFetch {
            record: Some("{}".into()),
            ..item("w", FetchReason::Backfill, 0)
        };
        s.enqueue(&fill, 2, 100)?;
        let w = s
            .next_due(0, 10)?
            .into_iter()
            .find(|q| q.uri == "w")
            .ok_or("w")?;
        assert_eq!(w.record.as_deref(), Some("{}"));
        Ok(())
    }

    #[test]
    fn cap_defers_backfill_and_revalidate() -> TestResult {
        let s = Store::open_in_memory()?;
        assert_eq!(
            s.enqueue(&item("a", FetchReason::Backfill, 0), 1, 2)?,
            EnqueueOutcome::Added
        );
        assert_eq!(
            s.enqueue(&item("b", FetchReason::Announce, 0), 2, 2)?,
            EnqueueOutcome::Added
        );
        assert_eq!(
            s.enqueue(&item("c", FetchReason::Backfill, 0), 3, 2)?,
            EnqueueOutcome::Deferred
        );
        assert_eq!(
            s.enqueue(&item("d", FetchReason::Revalidate, 0), 3, 2)?,
            EnqueueOutcome::Deferred
        );
        assert_eq!(s.queue_len()?, 2);
        // Duplicates at cap are still recognised, not deferred.
        assert_eq!(
            s.enqueue(&item("a", FetchReason::Backfill, 0), 4, 2)?,
            EnqueueOutcome::AlreadyQueued
        );
        assert_eq!(
            s.enqueue(&item("a", FetchReason::Announce, 0), 4, 2)?,
            EnqueueOutcome::Upgraded
        );
        Ok(())
    }

    #[test]
    fn crawl_items_only_take_the_first_half_of_the_queue() -> TestResult {
        let s = Store::open_in_memory()?;
        let cap = 4;
        assert_eq!(
            s.enqueue(&item("c1", FetchReason::Crawl, 0), 1, cap)?,
            EnqueueOutcome::Added
        );
        assert_eq!(
            s.enqueue(&item("c2", FetchReason::Crawl, 0), 2, cap)?,
            EnqueueOutcome::Added
        );
        // Half full: crawl items wait, backfill still fits.
        assert_eq!(
            s.enqueue(&item("c3", FetchReason::Crawl, 0), 3, cap)?,
            EnqueueOutcome::Deferred
        );
        assert_eq!(
            s.enqueue(&item("b", FetchReason::Backfill, 0), 3, cap)?,
            EnqueueOutcome::Added
        );
        // Already queued is still recognised.
        assert_eq!(
            s.enqueue(&item("c1", FetchReason::Crawl, 0), 4, cap)?,
            EnqueueOutcome::AlreadyQueued
        );
        // Crawl items come after revalidation, and an announce evicts them first.
        s.enqueue(&item("r", FetchReason::Revalidate, 0), 4, cap)?;
        assert_eq!(uris(&s.next_due(0, 10)?), ["b", "r", "c1", "c2"]);
        s.enqueue(&item("a", FetchReason::Announce, 0), 5, cap)?;
        assert_eq!(uris(&s.next_due(0, 10)?), ["a", "b", "r", "c1"]);
        // An announce of a queued crawl URI upgrades it.
        assert_eq!(
            s.enqueue(&item("c1", FetchReason::Announce, 0), 6, cap)?,
            EnqueueOutcome::Upgraded
        );
        // A cap of one still admits one crawl item.
        let t = Store::open_in_memory()?;
        assert_eq!(
            t.enqueue(&item("c", FetchReason::Crawl, 0), 1, 1)?,
            EnqueueOutcome::Added
        );
        Ok(())
    }

    #[test]
    fn announce_evicts_lowest_priority_newest() -> TestResult {
        let s = Store::open_in_memory()?;
        let cap = 4;
        s.enqueue(&item("ann", FetchReason::Announce, 0), 1, cap)?;
        s.enqueue(&item("bf", FetchReason::Backfill, 0), 2, cap)?;
        s.enqueue(&item("rv-old", FetchReason::Revalidate, 0), 3, cap)?;
        s.enqueue(&item("rv-new", FetchReason::Revalidate, 0), 4, cap)?;
        assert_eq!(
            s.enqueue(&item("ann2", FetchReason::Announce, 0), 5, cap)?,
            EnqueueOutcome::Added
        );
        assert_eq!(s.queue_len()?, 4);
        let all = s.next_due(0, 10)?;
        assert_eq!(uris(&all), ["ann", "ann2", "bf", "rv-old"]);
        s.enqueue(&item("ann3", FetchReason::Announce, 0), 6, cap)?;
        s.enqueue(&item("ann4", FetchReason::Announce, 0), 7, cap)?;
        let all = s.next_due(0, 10)?;
        assert_eq!(uris(&all), ["ann", "ann2", "ann3", "ann4"]);
        // Nothing evictable: announce is admitted anyway and the queue goes over cap.
        assert_eq!(
            s.enqueue(&item("ann5", FetchReason::Announce, 0), 8, cap)?,
            EnqueueOutcome::Added
        );
        assert_eq!(s.queue_len()?, 5);
        Ok(())
    }

    #[test]
    fn ordering() -> TestResult {
        let s = Store::open_in_memory()?;
        s.enqueue(&item("rv", FetchReason::Revalidate, 1), 1, 100)?;
        s.enqueue(&item("bf-late", FetchReason::Backfill, 9), 2, 100)?;
        s.enqueue(&item("bf-early", FetchReason::Backfill, 5), 3, 100)?;
        s.enqueue(&item("an", FetchReason::Announce, 9), 4, 100)?;
        s.enqueue(&item("future", FetchReason::Announce, 99), 5, 100)?;
        let due = s.next_due(10, 10)?;
        assert_eq!(uris(&due), ["an", "bf-early", "bf-late", "rv"]);
        assert_eq!(uris(&s.next_due(10, 2)?), ["an", "bf-early"]);
        assert_eq!(uris(&s.next_due(99, 10)?).first(), Some(&"an"));
        // Tie on priority and not_before falls back to enqueue time.
        s.enqueue(&item("t2", FetchReason::Revalidate, 1), 20, 100)?;
        s.enqueue(&item("t1", FetchReason::Revalidate, 1), 10, 100)?;
        let due = s.next_due(10, 10)?;
        assert_eq!(uris(&due).get(3..), Some(["rv", "t1", "t2"].as_slice()));
        Ok(())
    }

    #[test]
    fn fail_retry_and_complete() -> TestResult {
        let s = Store::open_in_memory()?;
        s.enqueue(&item("u", FetchReason::Announce, 0), 1, 10)?;
        assert!(s.fail("u", "HTTP 503 from x", 100)?);
        assert!(s.fail("u", "timeout", 200)?);
        assert_eq!(s.next_due(150, 10)?.len(), 0);
        let row = s.next_due(200, 10)?.into_iter().next().ok_or("empty")?;
        assert_eq!(row.attempts, 2);
        assert_eq!(row.last_error.as_deref(), Some("timeout"));
        assert_eq!(row.not_before, 200);
        assert!(!s.fail("missing", "e", 1)?);
        assert!(s.complete("u")?);
        assert!(!s.complete("u")?);
        assert_eq!(s.queue_len()?, 0);
        Ok(())
    }

    #[test]
    fn defer_reschedules_without_counting_an_attempt() -> TestResult {
        let s = Store::open_in_memory()?;
        s.enqueue(&item("u", FetchReason::Revalidate, 0), 1, 10)?;
        s.fail("u", "timeout", 5)?;
        assert!(s.defer("u", FetchReason::Revalidate, 300)?);
        assert_eq!(s.next_due(299, 10)?.len(), 0);
        let row = s.next_due(300, 10)?.into_iter().next().ok_or("empty")?;
        assert_eq!(row.not_before, 300);
        assert_eq!(row.attempts, 1, "not bumped");
        assert_eq!(row.last_error.as_deref(), Some("timeout"), "not replaced");
        // Upgraded to an announce since it was read: left alone.
        s.enqueue(&item("u", FetchReason::Announce, 10), 2, 10)?;
        assert!(!s.defer("u", FetchReason::Revalidate, 900)?);
        assert_eq!(s.next_due(10, 10)?.len(), 1);
        assert!(!s.defer("missing", FetchReason::Revalidate, 1)?);
        Ok(())
    }

    #[test]
    fn counts_by_reason() -> TestResult {
        let s = Store::open_in_memory()?;
        assert_eq!(s.queue_len_by_reason()?.len(), 0);
        s.enqueue(&item("a", FetchReason::Announce, 0), 1, 10)?;
        s.enqueue(&item("b", FetchReason::Backfill, 0), 1, 10)?;
        s.enqueue(&item("c", FetchReason::Backfill, 0), 1, 10)?;
        let m = s.queue_len_by_reason()?;
        assert_eq!(m.get("announce"), Some(&1));
        assert_eq!(m.get("backfill"), Some(&2));
        assert_eq!(m.get("revalidate"), None);
        Ok(())
    }
}
