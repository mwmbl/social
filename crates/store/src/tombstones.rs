//! The `tombstones` table: deletes and opt-outs not yet applied to every copy in the index.

use rusqlite::params;

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

    /// Deletes tombstones created strictly before `t`; returns how many.
    pub fn clear_tombstones_before(&self, t: i64) -> Result<usize, StoreError> {
        self.conn
            .execute("DELETE FROM tombstones WHERE created_at < ?1", [t])
            .map_err(ctx(format!("clear tombstones before {t}")))
    }
}

#[cfg(test)]
mod tests {
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
