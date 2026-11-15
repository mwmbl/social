//! `hosts` and `domain_blocks` tables.

use rusqlite::{OptionalExtension, params};

use crate::enums::SignatureScheme;
use crate::error::ctx;
use crate::{Store, StoreError};

/// What we know about a remote host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub host: String,
    /// The HTTP signature scheme the host accepted, once known.
    pub signature_scheme: Option<SignatureScheme>,
    /// Do not contact the host before this time (Unix seconds).
    pub backoff_until: Option<i64>,
    pub blocked: bool,
}

/// An admin-configured blocked domain (lowercase, no trailing dot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainBlock {
    pub domain: String,
    pub created_at: i64,
}

fn normalise_domain(domain: &str) -> String {
    domain.trim().trim_end_matches('.').to_lowercase()
}

impl Store {
    pub fn host(&self, host: &str) -> Result<Option<Host>, StoreError> {
        self.conn
            .query_row(
                "SELECT host, signature_scheme, backoff_until, blocked FROM hosts WHERE host = ?1",
                [host],
                |r| {
                    Ok(Host {
                        host: r.get(0)?,
                        signature_scheme: r.get(1)?,
                        backoff_until: r.get(2)?,
                        blocked: r.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(ctx(format!("read host {host}")))
    }

    /// Every host we hold state for.
    pub fn list_hosts(&self) -> Result<Vec<Host>, StoreError> {
        let op = "list hosts";
        let mut stmt = self
            .conn
            .prepare(
                "SELECT host, signature_scheme, backoff_until, blocked FROM hosts ORDER BY host",
            )
            .map_err(ctx(op))?;
        stmt.query_map([], |r| {
            Ok(Host {
                host: r.get(0)?,
                signature_scheme: r.get(1)?,
                backoff_until: r.get(2)?,
                blocked: r.get(3)?,
            })
        })
        .map_err(ctx(op))?
        .collect::<Result<_, _>>()
        .map_err(ctx(op))
    }

    pub fn set_signature_scheme(
        &self,
        host: &str,
        scheme: SignatureScheme,
    ) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO hosts (host, signature_scheme) VALUES (?1, ?2)
                 ON CONFLICT (host) DO UPDATE SET signature_scheme = excluded.signature_scheme",
                params![host, scheme],
            )
            .map_err(ctx(format!("set signature scheme of host {host}")))?;
        Ok(())
    }

    pub fn set_backoff(&self, host: &str, until: i64) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO hosts (host, backoff_until) VALUES (?1, ?2)
                 ON CONFLICT (host) DO UPDATE SET backoff_until = excluded.backoff_until",
                params![host, until],
            )
            .map_err(ctx(format!("set backoff of host {host}")))?;
        Ok(())
    }

    /// Clears a host's backoff. A no-op for unknown hosts.
    pub fn clear_backoff(&self, host: &str) -> Result<(), StoreError> {
        self.conn
            .execute(
                "UPDATE hosts SET backoff_until = NULL WHERE host = ?1",
                [host],
            )
            .map_err(ctx(format!("clear backoff of host {host}")))?;
        Ok(())
    }

    pub fn set_host_blocked(&self, host: &str, blocked: bool) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO hosts (host, blocked) VALUES (?1, ?2)
                 ON CONFLICT (host) DO UPDATE SET blocked = excluded.blocked",
                params![host, blocked],
            )
            .map_err(ctx(format!("set blocked flag of host {host}")))?;
        Ok(())
    }

    /// Adds a blocked domain, lowercased with any trailing dot removed. Re-adding keeps the
    /// original `created_at`. Returns the normalised domain.
    pub fn add_domain_block(&self, domain: &str, now: i64) -> Result<String, StoreError> {
        let domain = normalise_domain(domain);
        self.conn
            .execute(
                "INSERT OR IGNORE INTO domain_blocks (domain, created_at) VALUES (?1, ?2)",
                params![domain, now],
            )
            .map_err(ctx(format!("add domain block {domain}")))?;
        Ok(domain)
    }

    /// Removes a blocked domain. Returns whether it was blocked.
    pub fn remove_domain_block(&self, domain: &str) -> Result<bool, StoreError> {
        let domain = normalise_domain(domain);
        let changed = self
            .conn
            .execute("DELETE FROM domain_blocks WHERE domain = ?1", [&domain])
            .map_err(ctx(format!("remove domain block {domain}")))?;
        Ok(changed > 0)
    }

    pub fn list_domain_blocks(&self) -> Result<Vec<DomainBlock>, StoreError> {
        let op = "list domain blocks";
        let mut stmt = self
            .conn
            .prepare("SELECT domain, created_at FROM domain_blocks ORDER BY domain")
            .map_err(ctx(op))?;
        stmt.query_map([], |r| {
            Ok(DomainBlock {
                domain: r.get(0)?,
                created_at: r.get(1)?,
            })
        })
        .map_err(ctx(op))?
        .collect::<Result<_, _>>()
        .map_err(ctx(op))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn host_state() -> TestResult {
        let s = Store::open_in_memory()?;
        assert_eq!(s.host("a.example")?, None);
        s.clear_backoff("a.example")?;
        assert_eq!(s.host("a.example")?, None);
        s.set_signature_scheme("a.example", SignatureScheme::Cavage)?;
        s.set_backoff("a.example", 500)?;
        let h = s.host("a.example")?.ok_or("missing")?;
        assert_eq!(h.signature_scheme, Some(SignatureScheme::Cavage));
        assert_eq!(h.backoff_until, Some(500));
        assert!(!h.blocked);
        s.set_signature_scheme("a.example", SignatureScheme::Rfc9421)?;
        s.clear_backoff("a.example")?;
        let h = s.host("a.example")?.ok_or("missing")?;
        assert_eq!(h.signature_scheme, Some(SignatureScheme::Rfc9421));
        assert_eq!(h.backoff_until, None);
        s.set_host_blocked("a.example", true)?;
        assert!(s.host("a.example")?.is_some_and(|h| h.blocked));
        // Backoff on a host with nothing else recorded.
        s.set_backoff("b.example", 9)?;
        assert_eq!(s.host("b.example")?.and_then(|h| h.signature_scheme), None);
        let hosts: Vec<String> = s.list_hosts()?.into_iter().map(|h| h.host).collect();
        assert_eq!(hosts, ["a.example", "b.example"]);
        Ok(())
    }

    #[test]
    fn domain_blocks_normalise() -> TestResult {
        let s = Store::open_in_memory()?;
        assert_eq!(s.add_domain_block("Bad.Example.", 1)?, "bad.example");
        s.add_domain_block("bad.example", 2)?;
        s.add_domain_block("alpha.example", 3)?;
        assert_eq!(
            s.list_domain_blocks()?,
            vec![
                DomainBlock {
                    domain: "alpha.example".into(),
                    created_at: 3
                },
                DomainBlock {
                    domain: "bad.example".into(),
                    created_at: 1
                },
            ]
        );
        assert!(s.remove_domain_block("BAD.example.")?);
        assert!(!s.remove_domain_block("bad.example")?);
        assert_eq!(s.list_domain_blocks()?.len(), 1);
        Ok(())
    }
}
