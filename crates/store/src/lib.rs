#![deny(
    clippy::exit,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::panic,
    clippy::string_slice,
    clippy::unimplemented,
    clippy::unreachable,
    clippy::todo,
    clippy::pedantic
)]
// As in mwmbl_core: every public fn here returns Result, and the error types say what failed.
#![allow(clippy::missing_errors_doc)]
//! SQLite operational state for Mwmbl Social.
//!
//! Holds bounded state only: registrations, subscriptions, the fetch queue, hosts, tombstones
//! and the sweep position, plus records of explicit user and admin actions (follow opt-ins,
//! forgotten URIs). It never holds a row per document or per account, and never term
//! lists: the index is the only record of what is indexed.
//!
//! The API is synchronous. Time is always passed in as `now` (Unix seconds); nothing here reads
//! the clock.

mod enums;
mod error;
mod follow_optins;
mod following;
mod hosts;
mod queue;
mod registrations;
mod schema;
mod subscriptions;
mod sweep;
mod tombstones;

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, Transaction, TransactionBehavior};

pub use enums::{
    BackfillState, Category, FetchKind, FetchReason, FollowState, RegistrationStatus,
    SignatureScheme, SubscriptionType, TombstoneKind,
};
pub use error::StoreError;
pub use follow_optins::FollowOptin;
pub use following::Following;
pub use hosts::{DomainBlock, Host, HostRobots, HostSoftware};
pub use queue::{CRAWL_QUEUE_PERCENT, EnqueueOutcome, NewFetch, QueuedFetch};
pub use registrations::{NewRegistration, Registration};
pub use subscriptions::{Backfill, Subscription};
pub use sweep::SweepState;
pub use tombstones::Tombstone;

use error::ctx;

/// Number of migrations this build knows; the `user_version` of an up-to-date database.
pub const SCHEMA_VERSION: usize = schema::MIGRATIONS.len();

/// Handle to the SQLite database. Not `Sync`: use one per thread, or keep it on the dedicated
/// blocking thread.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (creating if needed) the database at `path`, enables WAL, foreign keys and a 5 s
    /// busy timeout, and runs pending migrations.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn =
            Connection::open(path).map_err(ctx(format!("open database {}", path.display())))?;
        Self::init(conn, &path.display().to_string())
    }

    /// Opens a private in-memory database. For tests.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(ctx("open in-memory database"))?;
        Self::init(conn, ":memory:")
    }

    fn init(conn: Connection, name: &str) -> Result<Self, StoreError> {
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(ctx(format!("set busy timeout on {name}")))?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))
            .map_err(ctx(format!("enable WAL on {name}")))?;
        let store = Self { conn };
        // Migrations run with foreign keys off, as SQLite's procedure for rebuilding a table
        // requires (dropping a parent table would otherwise cascade to its children); each is
        // checked with `foreign_key_check` before it commits. The bundled SQLite turns them on
        // by default, and the pragma is ignored inside a transaction, so it is set here.
        store
            .conn
            .pragma_update(None, "foreign_keys", false)
            .map_err(ctx(format!("disable foreign keys for migrating {name}")))?;
        store.migrate(name)?;
        store
            .conn
            .pragma_update(None, "foreign_keys", true)
            .map_err(ctx(format!("enable foreign keys on {name}")))?;
        Ok(store)
    }

    /// Begins a transaction that takes the write lock at once (`BEGIN IMMEDIATE`). Every
    /// transaction here reads and then writes. A deferred one starts as a reader, and if another
    /// connection commits before it writes, SQLite can't upgrade it and returns "database is
    /// locked" at once, without waiting out the busy timeout. Taking the lock first makes it
    /// wait like any other write.
    fn write_transaction(&self) -> rusqlite::Result<Transaction<'_>> {
        Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
    }

    fn migrate(&self, name: &str) -> Result<(), StoreError> {
        let found = self.schema_version()?;
        if found > SCHEMA_VERSION {
            return Err(StoreError::SchemaTooNew {
                found,
                supported: SCHEMA_VERSION,
            });
        }
        for (i, sql) in schema::MIGRATIONS.iter().enumerate().skip(found) {
            let op = format!("migration {} on {name}", i + 1);
            let tx = self.write_transaction().map_err(ctx(op.clone()))?;
            tx.execute_batch(sql).map_err(ctx(op.clone()))?;
            let violations: i64 = tx
                .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                    r.get(0)
                })
                .map_err(ctx(op.clone()))?;
            if violations > 0 {
                return Err(StoreError::ForeignKeys {
                    operation: op,
                    violations,
                });
            }
            // PRAGMA values cannot be bound; `i + 1` is a usize we produced.
            tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))
                .map_err(ctx(op.clone()))?;
            tx.commit().map_err(ctx(op))?;
        }
        Ok(())
    }

    /// The schema version recorded in the database.
    pub fn schema_version(&self) -> Result<usize, StoreError> {
        let v: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(ctx("read schema version"))?;
        error::to_usize(v, "read schema version")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn open_sets_pragmas_and_version() -> TestResult {
        let dir = tempfile::tempdir()?;
        let store = Store::open(&dir.path().join("s.db"))?;
        assert_eq!(store.schema_version()?, SCHEMA_VERSION);
        let mode: String = store
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        assert_eq!(mode, "wal");
        let fk: i64 = store
            .conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
        assert_eq!(fk, 1);
        let timeout: i64 = store
            .conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))?;
        assert_eq!(timeout, 5000);
        Ok(())
    }

    #[test]
    fn reopen_is_idempotent_and_keeps_data() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("s.db");
        {
            let store = Store::open(&path)?;
            store.add_domain_block("Example.com.", 1)?;
        }
        let store = Store::open(&path)?;
        assert_eq!(store.schema_version()?, SCHEMA_VERSION);
        assert_eq!(store.list_domain_blocks()?.len(), 1);
        Ok(())
    }

    #[test]
    fn host_software_migration_keeps_existing_hosts() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("s.db");
        {
            // A database from before the software columns (migration 4).
            let conn = Connection::open(&path)?;
            for sql in schema::MIGRATIONS.iter().take(3) {
                conn.execute_batch(sql)?;
            }
            conn.execute_batch("PRAGMA user_version = 3")?;
            conn.execute(
                "INSERT INTO hosts (host, backoff_until) VALUES ('a.example', 7)",
                [],
            )?;
        }
        let store = Store::open(&path)?;
        let host = store.host("a.example")?.ok_or("missing")?;
        assert_eq!(host.backoff_until, Some(7));
        assert_eq!(host.software, None);
        assert_eq!(store.host_software("a.example")?, None);
        assert_eq!(store.follow_optins()?, []);
        Ok(())
    }

    #[test]
    fn crawl_migration_rebuilds_the_queue_with_its_rows() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("s.db");
        {
            // A database from before the crawl reason (migration 6).
            let conn = Connection::open(&path)?;
            for sql in schema::MIGRATIONS.iter().take(5) {
                conn.execute_batch(sql)?;
            }
            conn.execute_batch("PRAGMA user_version = 5")?;
            conn.execute(
                "INSERT INTO fetch_queue (uri, kind, reason, priority, not_before, attempts,
                     last_error, enqueued_at)
                 VALUES ('https://a.example/1', 'object', 'backfill', 1, 9, 2, 'timeout', 3)",
                [],
            )?;
            conn.execute(
                "INSERT INTO hosts (host, software, software_checked_at)
                 VALUES ('a.example', 'lemmy', 4)",
                [],
            )?;
        }
        let store = Store::open(&path)?;
        let rows = store.next_due(9, 10)?;
        let row = rows.first().ok_or("missing row")?;
        assert_eq!(
            (row.uri.as_str(), row.reason, row.attempts, row.not_before),
            ("https://a.example/1", FetchReason::Backfill, 2, 9)
        );
        assert_eq!(row.last_error.as_deref(), Some("timeout"));
        let crawl = NewFetch {
            uri: "https://a.example/2".into(),
            kind: FetchKind::Unknown,
            reason: FetchReason::Crawl,
            record: None,
            not_before: 0,
        };
        assert_eq!(store.enqueue(&crawl, 5, 10)?, EnqueueOutcome::Added);
        let indexes: i64 = store.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'fetch_queue_due'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(indexes, 1);
        let host = store.host("a.example")?.ok_or("missing host")?;
        assert_eq!(host.software.as_deref(), Some("lemmy"));
        assert_eq!(store.host_robots("a.example")?, None);
        Ok(())
    }

    #[test]
    fn registration_id_migration_keeps_rows_and_children() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("s.db");
        {
            // A database from before AUTOINCREMENT registration ids (migration 7).
            let conn = Connection::open(&path)?;
            for sql in schema::MIGRATIONS.iter().take(6) {
                conn.execute_batch(sql)?;
            }
            conn.execute_batch("PRAGMA user_version = 6")?;
            for (id, url) in [(1, "https://a.example"), (2, "https://b.example")] {
                conn.execute(
                    "INSERT INTO registrations
                         (id, base_url, server_id, fasp_id, our_private_key, status, created_at)
                     VALUES (?1, ?2, ?2, 'f', x'00', 'active', 1)",
                    rusqlite::params![id, url],
                )?;
            }
            conn.execute(
                "INSERT INTO capabilities VALUES (2, 'data_sharing', 'v0', 1, 1)",
                [],
            )?;
            conn.execute(
                "INSERT INTO subscriptions
                     (registration_id, remote_id, category, subscription_type, created_at)
                 VALUES (2, 's1', 'content', 'lifecycle', 1)",
                [],
            )?;
        }
        let store = Store::open(&path)?;
        assert_eq!(store.list_registrations()?.len(), 2);
        assert!(store.capability_enabled(2, "data_sharing", "v0")?);
        assert_eq!(store.list_subscriptions(2)?.len(), 1);
        let fk_problems: i64 =
            store
                .conn
                .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                    r.get(0)
                })?;
        assert_eq!(fk_problems, 0);
        // Children still cascade from the new table, and a deleted top id is not reused.
        assert!(store.delete_registration(2)?);
        assert!(!store.capability_enabled(2, "data_sharing", "v0")?);
        assert_eq!(store.list_subscriptions(2)?.len(), 0);
        let next = store.create_registration(registrations::NewRegistration {
            base_url: "https://c.example".into(),
            server_id: "sid-c".into(),
            our_private_key: vec![0; 32],
            created_at: 2,
        })?;
        assert_eq!(next.id, 3);
        // Foreign keys are enforced again after migrating.
        assert!(store.set_capability(99, "x", "v0", true, 1).is_err());
        assert!(store.set_capability(next.id, "x", "v0", true, 1).is_ok());
        Ok(())
    }

    #[test]
    fn tombstone_milliseconds_migration_keeps_creation_times() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("s.db");
        {
            // A database from before tombstone milliseconds (migration 9).
            let conn = Connection::open(&path)?;
            for sql in schema::MIGRATIONS.iter().take(8) {
                conn.execute_batch(sql)?;
            }
            conn.execute_batch("PRAGMA user_version = 8")?;
            conn.execute(
                "INSERT INTO tombstones (uri, kind, created_at) VALUES ('https://a/1', 'object', 5)",
                [],
            )?;
        }
        let store = Store::open(&path)?;
        assert_eq!(
            store.tombstone_created_ms("https://a/1", TombstoneKind::Object)?,
            Some(5000)
        );
        Ok(())
    }

    #[test]
    fn in_memory_is_current() -> TestResult {
        let store = Store::open_in_memory()?;
        assert_eq!(store.schema_version()?, SCHEMA_VERSION);
        Ok(())
    }

    #[test]
    fn newer_database_is_refused() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("s.db");
        {
            let store = Store::open(&path)?;
            store
                .conn
                .execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))?;
        }
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::SchemaTooNew { .. })
        ));
        Ok(())
    }

    #[test]
    fn open_error_names_the_path() {
        let err = Store::open(Path::new("/nonexistent-dir-xyz/s.db"));
        assert!(err.is_err_and(|e| e.to_string().contains("/nonexistent-dir-xyz/s.db")));
    }

    #[test]
    fn unknown_enum_in_db_is_an_error_not_a_panic() -> TestResult {
        let store = Store::open_in_memory()?;
        store
            .conn
            .execute_batch("PRAGMA ignore_check_constraints = ON")?;
        store.conn.execute(
            "INSERT INTO registrations (base_url, server_id, our_private_key, status, created_at)
             VALUES ('https://a', 's', x'00', 'weird', 0)",
            [],
        )?;
        let err = store.list_registrations();
        assert!(matches!(err, Err(StoreError::UnknownValue { value, .. }) if value == "weird"));
        Ok(())
    }
}
