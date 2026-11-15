//! `registrations` and `capabilities` tables.

use std::fmt;

use rusqlite::{OptionalExtension, Row, params};

use crate::enums::RegistrationStatus;
use crate::error::{ctx, expect_changed};
use crate::{Store, StoreError};

/// A new registration to insert. Always starts as `pending`.
#[derive(Clone, PartialEq, Eq)]
pub struct NewRegistration {
    pub base_url: String,
    /// Our identifier for the server (the `serverId` we hand out).
    pub server_id: String,
    /// Our Ed25519 private key seed (32 bytes).
    pub our_private_key: Vec<u8>,
    pub created_at: i64,
}

impl fmt::Debug for NewRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewRegistration")
            .field("base_url", &self.base_url)
            .field("server_id", &self.server_id)
            .field("our_private_key", &"<redacted>")
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// A stored registration.
#[derive(Clone, PartialEq, Eq)]
pub struct Registration {
    pub id: i64,
    pub base_url: String,
    pub server_id: String,
    /// The provider's identifier for us; `None` until they reply.
    pub fasp_id: Option<String>,
    /// Our Ed25519 private key seed. Never printed by `Debug`.
    pub our_private_key: Vec<u8>,
    pub their_public_key: Option<Vec<u8>>,
    pub status: RegistrationStatus,
    pub created_at: i64,
}

impl fmt::Debug for Registration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registration")
            .field("id", &self.id)
            .field("base_url", &self.base_url)
            .field("server_id", &self.server_id)
            .field("fasp_id", &self.fasp_id)
            .field("our_private_key", &"<redacted>")
            .field("their_public_key", &self.their_public_key)
            .field("status", &self.status)
            .field("created_at", &self.created_at)
            .finish()
    }
}

const COLUMNS: &str =
    "id, base_url, server_id, fasp_id, our_private_key, their_public_key, status, created_at";

fn from_row(row: &Row<'_>) -> rusqlite::Result<Registration> {
    Ok(Registration {
        id: row.get(0)?,
        base_url: row.get(1)?,
        server_id: row.get(2)?,
        fasp_id: row.get(3)?,
        our_private_key: row.get(4)?,
        their_public_key: row.get(5)?,
        status: row.get(6)?,
        created_at: row.get(7)?,
    })
}

impl Store {
    pub fn create_registration(&self, new: NewRegistration) -> Result<Registration, StoreError> {
        let op = format!("create registration for {}", new.base_url);
        self.conn
            .execute(
                "INSERT INTO registrations
                    (base_url, server_id, our_private_key, status, created_at)
                 VALUES (?1, ?2, ?3, 'pending', ?4)",
                params![
                    new.base_url,
                    new.server_id,
                    new.our_private_key,
                    new.created_at
                ],
            )
            .map_err(ctx(op))?;
        Ok(Registration {
            id: self.conn.last_insert_rowid(),
            base_url: new.base_url,
            server_id: new.server_id,
            fasp_id: None,
            our_private_key: new.our_private_key,
            their_public_key: None,
            status: RegistrationStatus::Pending,
            created_at: new.created_at,
        })
    }

    pub fn registration(&self, id: i64) -> Result<Option<Registration>, StoreError> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM registrations WHERE id = ?1"),
                [id],
                from_row,
            )
            .optional()
            .map_err(ctx(format!("read registration {id}")))
    }

    pub fn registration_by_server_id(
        &self,
        server_id: &str,
    ) -> Result<Option<Registration>, StoreError> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM registrations WHERE server_id = ?1"),
                [server_id],
                from_row,
            )
            .optional()
            .map_err(ctx(format!("read registration by server id {server_id}")))
    }

    pub fn registration_by_fasp_id(
        &self,
        fasp_id: &str,
    ) -> Result<Option<Registration>, StoreError> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM registrations WHERE fasp_id = ?1"),
                [fasp_id],
                from_row,
            )
            .optional()
            .map_err(ctx(format!("read registration by fasp id {fasp_id}")))
    }

    pub fn list_registrations(&self) -> Result<Vec<Registration>, StoreError> {
        let op = "list registrations";
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {COLUMNS} FROM registrations ORDER BY id"))
            .map_err(ctx(op))?;
        stmt.query_map([], from_row)
            .map_err(ctx(op))?
            .collect::<Result<_, _>>()
            .map_err(ctx(op))
    }

    /// Records the provider's id for us and their public key, once they have replied.
    pub fn set_registration_remote(
        &self,
        id: i64,
        fasp_id: &str,
        their_public_key: &[u8],
    ) -> Result<(), StoreError> {
        let op = "set registration remote";
        let changed = self
            .conn
            .execute(
                "UPDATE registrations SET fasp_id = ?2, their_public_key = ?3 WHERE id = ?1",
                params![id, fasp_id, their_public_key],
            )
            .map_err(ctx(format!("{op} {id}")))?;
        expect_changed(changed, op, || format!("registration {id}"))
    }

    pub fn set_registration_status(
        &self,
        id: i64,
        status: RegistrationStatus,
    ) -> Result<(), StoreError> {
        let op = "set registration status";
        let changed = self
            .conn
            .execute(
                "UPDATE registrations SET status = ?2 WHERE id = ?1",
                params![id, status],
            )
            .map_err(ctx(format!("{op} {id}")))?;
        expect_changed(changed, op, || format!("registration {id}"))
    }

    /// Records whether a capability version is enabled for a registration.
    pub fn set_capability(
        &self,
        registration_id: i64,
        capability: &str,
        version: &str,
        enabled: bool,
        now: i64,
    ) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO capabilities (registration_id, capability, version, enabled, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (registration_id, capability, version)
                 DO UPDATE SET enabled = excluded.enabled, updated_at = excluded.updated_at",
                params![registration_id, capability, version, enabled, now],
            )
            .map_err(ctx(format!(
                "set capability {capability} v{version} on registration {registration_id}"
            )))?;
        Ok(())
    }

    /// Whether a capability version is enabled; `false` when it was never recorded.
    pub fn capability_enabled(
        &self,
        registration_id: i64,
        capability: &str,
        version: &str,
    ) -> Result<bool, StoreError> {
        let enabled: Option<bool> = self
            .conn
            .query_row(
                "SELECT enabled FROM capabilities
                 WHERE registration_id = ?1 AND capability = ?2 AND version = ?3",
                params![registration_id, capability, version],
                |r| r.get(0),
            )
            .optional()
            .map_err(ctx(format!(
                "read capability {capability} v{version} on registration {registration_id}"
            )))?;
        Ok(enabled.unwrap_or(false))
    }

    /// Enabled `(capability, version)` pairs for a registration, sorted.
    pub fn enabled_capabilities(
        &self,
        registration_id: i64,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let op = format!("list enabled capabilities of registration {registration_id}");
        let mut stmt = self
            .conn
            .prepare(
                "SELECT capability, version FROM capabilities
                 WHERE registration_id = ?1 AND enabled = 1 ORDER BY capability, version",
            )
            .map_err(ctx(op.clone()))?;
        stmt.query_map([registration_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(ctx(op.clone()))?
            .collect::<Result<_, _>>()
            .map_err(ctx(op))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn new(url: &str, sid: &str) -> NewRegistration {
        NewRegistration {
            base_url: url.to_owned(),
            server_id: sid.to_owned(),
            our_private_key: vec![0xAB; 32],
            created_at: 100,
        }
    }

    #[test]
    fn create_and_lookup() -> TestResult {
        let s = Store::open_in_memory()?;
        let r = s.create_registration(new("https://a.example", "sid-a"))?;
        assert_eq!(r.status, RegistrationStatus::Pending);
        assert_eq!(r.fasp_id, None);
        assert_eq!(s.registration(r.id)?, Some(r.clone()));
        assert_eq!(s.registration_by_server_id("sid-a")?, Some(r.clone()));
        assert_eq!(s.registration_by_server_id("nope")?, None);
        assert_eq!(s.registration(999)?, None);
        let r2 = s.create_registration(new("https://b.example", "sid-b"))?;
        assert_eq!(s.list_registrations()?, vec![r, r2]);
        Ok(())
    }

    #[test]
    fn unique_constraints() -> TestResult {
        let s = Store::open_in_memory()?;
        s.create_registration(new("https://a.example", "sid-a"))?;
        let dup_url = s.create_registration(new("https://a.example", "sid-x"));
        assert!(dup_url.is_err_and(|e| e.to_string().contains("https://a.example")));
        assert!(
            s.create_registration(new("https://c.example", "sid-a"))
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn remote_and_status_updates() -> TestResult {
        let s = Store::open_in_memory()?;
        let r = s.create_registration(new("https://a.example", "sid-a"))?;
        assert_eq!(s.registration_by_fasp_id("fasp-1")?, None);
        s.set_registration_remote(r.id, "fasp-1", &[1, 2, 3])?;
        s.set_registration_status(r.id, RegistrationStatus::Active)?;
        let got = s.registration_by_fasp_id("fasp-1")?.ok_or("missing")?;
        assert_eq!(got.their_public_key, Some(vec![1, 2, 3]));
        assert_eq!(got.status, RegistrationStatus::Active);
        assert!(matches!(
            s.set_registration_status(999, RegistrationStatus::Revoked),
            Err(StoreError::NotFound { .. })
        ));
        assert!(matches!(
            s.set_registration_remote(999, "x", &[]),
            Err(StoreError::NotFound { .. })
        ));
        Ok(())
    }

    #[test]
    fn debug_redacts_private_key() -> TestResult {
        let s = Store::open_in_memory()?;
        let mut n = new("https://a.example", "sid-a");
        n.our_private_key = vec![171, 205, 239, 77, 78, 79, 80, 81];
        let shown_new = format!("{n:?}");
        let r = s.create_registration(n)?;
        let shown = format!("{r:?}");
        for text in [shown, shown_new] {
            assert!(text.contains("<redacted>"));
            assert!(!text.contains("171"));
            assert!(!text.contains("205"));
            assert!(!text.contains("239"));
            assert!(!text.contains("0xAB") && !text.contains("ab, cd"));
        }
        Ok(())
    }

    #[test]
    fn capabilities() -> TestResult {
        let s = Store::open_in_memory()?;
        let r = s.create_registration(new("https://a.example", "sid-a"))?;
        assert!(!s.capability_enabled(r.id, "data_sharing", "v0")?);
        s.set_capability(r.id, "data_sharing", "v0", true, 1)?;
        s.set_capability(r.id, "trends", "v0", false, 1)?;
        s.set_capability(r.id, "account_search", "v0", true, 1)?;
        assert!(s.capability_enabled(r.id, "data_sharing", "v0")?);
        assert!(!s.capability_enabled(r.id, "data_sharing", "v1")?);
        assert!(!s.capability_enabled(r.id, "trends", "v0")?);
        assert_eq!(
            s.enabled_capabilities(r.id)?,
            vec![
                ("account_search".to_owned(), "v0".to_owned()),
                ("data_sharing".to_owned(), "v0".to_owned())
            ]
        );
        s.set_capability(r.id, "data_sharing", "v0", false, 2)?;
        assert!(!s.capability_enabled(r.id, "data_sharing", "v0")?);
        assert_eq!(s.enabled_capabilities(r.id)?.len(), 1);
        // Foreign key enforced.
        assert!(s.set_capability(999, "x", "v0", true, 1).is_err());
        Ok(())
    }
}
