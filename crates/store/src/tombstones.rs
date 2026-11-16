//! The `tombstones` table: deletes and opt-outs not yet applied to every copy in the index.

use rusqlite::{OptionalExtension, params};

use crate::enums::TombstoneKind;
use crate::error::ctx;
use crate::{Store, StoreError};

/// A pending delete or opt-out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tombstone {
    /// Object URI or author (actor) URI, depending on `kind`.
    pub uri: String,
    pub kind: TombstoneKind,
    pub created_at: i64,
}

impl Store {
    /// Permanently forget a URI (object or actor), as `mwmbl-social forget` does. Unlike a
    /// tombstone, this is never cleared by the sweep.
    pub fn add_forgotten(&self, uri: &str, now: i64) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO forgotten (uri, created_at) VALUES (?1, ?2) ON CONFLICT (uri) DO NOTHING",
                params![uri, now],
            )
            .map_err(ctx(format!("forget {uri}")))?;
        Ok(())
    }

    /// Whether `uri` was forgotten.
    pub fn is_forgotten(&self, uri: &str) -> Result<bool, StoreError> {
        self.conn
            .query_row("SELECT 1 FROM forgotten WHERE uri = ?1", [uri], |_| Ok(()))
            .optional()
            .map(|found| found.is_some())
            .map_err(ctx(format!("read forgotten {uri}")))
    }

    /// Every forgotten URI.
    pub fn list_forgotten(&self) -> Result<Vec<String>, StoreError> {
        let op = "list forgotten";
        let mut stmt = self
            .conn
            .prepare("SELECT uri FROM forgotten ORDER BY uri")
            .map_err(ctx(op))?;
        let rows = stmt
            .query_map([], |r| r.get(0))
            .map_err(ctx(op))?
            .collect::<Result<Vec<String>, _>>()
            .map_err(ctx(op))?;
        Ok(rows)
    }

    /// Adds a tombstone. Re-adding the same `(uri, kind)` refreshes `created_at`.
    pub fn add_tombstone(
        &self,
        uri: &str,
        kind: TombstoneKind,
        now: i64,
    ) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO tombstones (uri, kind, created_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (uri, kind) DO UPDATE SET created_at = excluded.created_at",
                params![uri, kind, now],
            )
            .map_err(ctx(format!("add {kind} tombstone {uri}")))?;
        Ok(())
    }

    pub fn list_tombstones(&self) -> Result<Vec<Tombstone>, StoreError> {
        let op = "list tombstones";
        let mut stmt = self
            .conn
            .prepare("SELECT uri, kind, created_at FROM tombstones ORDER BY created_at, uri, kind")
            .map_err(ctx(op))?;
        stmt.query_map([], |r| {
            Ok(Tombstone {
                uri: r.get(0)?,
                kind: r.get(1)?,
                created_at: r.get(2)?,
            })
        })
        .map_err(ctx(op))?
        .collect::<Result<_, _>>()
        .map_err(ctx(op))
    }

    /// Removes one tombstone. Returns whether it existed.
    pub fn remove_tombstone(&self, uri: &str, kind: TombstoneKind) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .execute(
                "DELETE FROM tombstones WHERE uri = ?1 AND kind = ?2",
                params![uri, kind],
            )
            .map_err(ctx(format!("remove {kind} tombstone {uri}")))?;
        Ok(changed > 0)
    }

    /// Deletes tombstones created strictly before `t`; returns how many.
    pub fn clear_tombstones_before(&self, t: i64) -> Result<usize, StoreError> {
        self.conn
            .execute("DELETE FROM tombstones WHERE created_at < ?1", [t])
            .map_err(ctx(format!("clear tombstones before {t}")))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn forgotten_is_permanent_and_idempotent() {
        let store = Store::open_in_memory().unwrap();
        store.add_forgotten("https://h.example/u/a", 1).unwrap();
        store.add_forgotten("https://h.example/u/a", 2).unwrap();
        store
            .add_tombstone("https://h.example/s/1", TombstoneKind::Object, 1)
            .unwrap();
        assert_eq!(store.clear_tombstones_before(100).unwrap(), 1);
        assert_eq!(store.list_forgotten().unwrap(), ["https://h.example/u/a"]);
        assert!(store.is_forgotten("https://h.example/u/a").unwrap());
        assert!(!store.is_forgotten("https://h.example/u/b").unwrap());
    }

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn add_refresh_list() -> TestResult {
        let s = Store::open_in_memory()?;
        s.add_tombstone("https://x/1", TombstoneKind::Object, 10)?;
        s.add_tombstone("https://x/1", TombstoneKind::Author, 20)?;
        s.add_tombstone("https://x/1", TombstoneKind::Object, 30)?;
        let list = s.list_tombstones()?;
        assert_eq!(
            list,
            vec![
                Tombstone {
                    uri: "https://x/1".into(),
                    kind: TombstoneKind::Author,
                    created_at: 20
                },
                Tombstone {
                    uri: "https://x/1".into(),
                    kind: TombstoneKind::Object,
                    created_at: 30
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn remove_one_kind() -> TestResult {
        let s = Store::open_in_memory()?;
        s.add_tombstone("a", TombstoneKind::Object, 10)?;
        s.add_tombstone("a", TombstoneKind::Author, 10)?;
        assert!(s.remove_tombstone("a", TombstoneKind::Author)?);
        assert!(!s.remove_tombstone("a", TombstoneKind::Author)?);
        let left = s.list_tombstones()?;
        assert_eq!(left.len(), 1);
        assert_eq!(left.first().map(|t| t.kind), Some(TombstoneKind::Object));
        Ok(())
    }

    #[test]
    fn clearing_is_strictly_before() -> TestResult {
        let s = Store::open_in_memory()?;
        s.add_tombstone("a", TombstoneKind::Object, 10)?;
        s.add_tombstone("b", TombstoneKind::Object, 20)?;
        s.add_tombstone("c", TombstoneKind::Author, 30)?;
        assert_eq!(s.clear_tombstones_before(10)?, 0);
        assert_eq!(s.clear_tombstones_before(20)?, 1);
        assert_eq!(s.list_tombstones()?.len(), 2);
        assert_eq!(s.clear_tombstones_before(1000)?, 2);
        assert_eq!(s.list_tombstones()?.len(), 0);
        // A refreshed tombstone survives a clear that would have removed the old one.
        s.add_tombstone("a", TombstoneKind::Object, 10)?;
        s.add_tombstone("a", TombstoneKind::Object, 50)?;
        assert_eq!(s.clear_tombstones_before(40)?, 0);
        Ok(())
    }
}
