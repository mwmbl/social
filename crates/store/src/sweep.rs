//! The `sweep_state` table: where each index's sweep has got to.

use rusqlite::params;

use crate::error::{ctx, expect_changed};
use crate::{Store, StoreError};

/// Sweep position for one index (for example `posts` or `accounts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepState {
    pub index_name: String,
    /// The next page to sweep.
    pub current_page: u32,
    pub current_sweep_started_at: i64,
    pub last_sweep_started_at: Option<i64>,
    pub last_sweep_finished_at: Option<i64>,
}

impl Store {
    fn ensure_sweep_row(&self, index: &str, now: i64) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO sweep_state (index_name, current_page, current_sweep_started_at)
                 VALUES (?1, 0, ?2)",
                params![index, now],
            )
            .map_err(ctx(format!("create sweep state for index {index}")))?;
        Ok(())
    }

    /// The sweep state of `index`; if there is none, a fresh one (page 0, started `now`) is
    /// created.
    pub fn sweep_state(&self, index: &str, now: i64) -> Result<SweepState, StoreError> {
        self.ensure_sweep_row(index, now)?;
        self.conn
            .query_row(
                "SELECT index_name, current_page, current_sweep_started_at,
                        last_sweep_started_at, last_sweep_finished_at
                 FROM sweep_state WHERE index_name = ?1",
                [index],
                |r| {
                    Ok(SweepState {
                        index_name: r.get(0)?,
                        current_page: r.get(1)?,
                        current_sweep_started_at: r.get(2)?,
                        last_sweep_started_at: r.get(3)?,
                        last_sweep_finished_at: r.get(4)?,
                    })
                },
            )
            .map_err(ctx(format!("read sweep state of index {index}")))
    }

    /// Records the next page to sweep. The state must exist (see [`Store::sweep_state`]).
    pub fn set_sweep_page(&self, index: &str, page: u32) -> Result<(), StoreError> {
        let changed = self
            .conn
            .execute(
                "UPDATE sweep_state SET current_page = ?2 WHERE index_name = ?1",
                params![index, page],
            )
            .map_err(ctx(format!("set sweep page of index {index}")))?;
        expect_changed(changed, "set sweep page", || {
            format!("sweep state of index {index}")
        })
    }

    /// Ends the current sweep and starts the next: the current start becomes the last start,
    /// the last finish becomes `now`, the page resets to 0 and the new sweep starts at `now`.
    pub fn finish_sweep(&self, index: &str, now: i64) -> Result<(), StoreError> {
        let op = format!("finish sweep of index {index}");
        let tx = self.write_transaction().map_err(ctx(op.clone()))?;
        tx.execute(
            "INSERT OR IGNORE INTO sweep_state (index_name, current_page, current_sweep_started_at)
             VALUES (?1, 0, ?2)",
            params![index, now],
        )
        .map_err(ctx(op.clone()))?;
        tx.execute(
            "UPDATE sweep_state
             SET last_sweep_started_at = current_sweep_started_at,
                 last_sweep_finished_at = ?2,
                 current_page = 0,
                 current_sweep_started_at = ?2
             WHERE index_name = ?1",
            params![index, now],
        )
        .map_err(ctx(op.clone()))?;
        tx.commit().map_err(ctx(op))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn default_state_starts_now_once() -> TestResult {
        let s = Store::open_in_memory()?;
        let st = s.sweep_state("posts", 100)?;
        assert_eq!(
            st,
            SweepState {
                index_name: "posts".into(),
                current_page: 0,
                current_sweep_started_at: 100,
                last_sweep_started_at: None,
                last_sweep_finished_at: None,
            }
        );
        // A later read does not restart it.
        assert_eq!(s.sweep_state("posts", 999)?.current_sweep_started_at, 100);
        Ok(())
    }

    #[test]
    fn page_and_finish_transitions() -> TestResult {
        let s = Store::open_in_memory()?;
        s.sweep_state("posts", 100)?;
        s.set_sweep_page("posts", 42)?;
        assert_eq!(s.sweep_state("posts", 101)?.current_page, 42);
        s.finish_sweep("posts", 200)?;
        let st = s.sweep_state("posts", 201)?;
        assert_eq!(st.current_page, 0);
        assert_eq!(st.current_sweep_started_at, 200);
        assert_eq!(st.last_sweep_started_at, Some(100));
        assert_eq!(st.last_sweep_finished_at, Some(200));
        s.set_sweep_page("posts", 7)?;
        s.finish_sweep("posts", 300)?;
        let st = s.sweep_state("posts", 301)?;
        assert_eq!(st.last_sweep_started_at, Some(200));
        assert_eq!(st.last_sweep_finished_at, Some(300));
        assert_eq!(st.current_sweep_started_at, 300);
        Ok(())
    }

    #[test]
    fn indexes_are_independent_and_finish_creates() -> TestResult {
        let s = Store::open_in_memory()?;
        s.sweep_state("posts", 10)?;
        s.set_sweep_page("posts", 5)?;
        s.finish_sweep("accounts", 50)?;
        let acc = s.sweep_state("accounts", 60)?;
        assert_eq!(acc.last_sweep_started_at, Some(50));
        assert_eq!(s.sweep_state("posts", 60)?.current_page, 5);
        assert!(matches!(
            s.set_sweep_page("nope", 1),
            Err(StoreError::NotFound { .. })
        ));
        Ok(())
    }
}
