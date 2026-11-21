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
    /// The host's nodeinfo `software.name`, lowercased, once looked up. `None` with a
    /// `software_checked_at` means the lookup failed.
    pub software: Option<String>,
    /// When `software` was last looked up (Unix seconds).
    pub software_checked_at: Option<i64>,
    /// The robots.txt rules that apply to us, reduced to one group; see [`HostRobots`].
    pub robots_txt: Option<String>,
    /// When robots.txt was last fetched (Unix seconds).
    pub robots_checked_at: Option<i64>,
}

/// The result of a host's last robots.txt fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRobots {
    /// The rules that apply to us as a reduced robots.txt; empty allows everything. `None`:
    /// robots.txt was unavailable, which disallows everything until it is fetched again.
    pub robots_txt: Option<String>,
    /// When it was fetched (Unix seconds).
    pub checked_at: i64,
}

/// The result of a host's last software lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSoftware {
    /// Nodeinfo `software.name`, lowercased; `None` if the lookup failed.
    pub software: Option<String>,
    /// When the lookup was made (Unix seconds).
    pub checked_at: i64,
}

const HOST_COLUMNS: &str = "host, signature_scheme, backoff_until, blocked, software, \
     software_checked_at, robots_txt, robots_checked_at";

fn host_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Host> {
    Ok(Host {
        host: r.get(0)?,
        signature_scheme: r.get(1)?,
        backoff_until: r.get(2)?,
        blocked: r.get(3)?,
        software: r.get(4)?,
        software_checked_at: r.get(5)?,
        robots_txt: r.get(6)?,
        robots_checked_at: r.get(7)?,
    })
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
                &format!("SELECT {HOST_COLUMNS} FROM hosts WHERE host = ?1"),
                [host],
                host_row,
            )
            .optional()
            .map_err(ctx(format!("read host {host}")))
    }

    /// Every host we hold state for.
    pub fn list_hosts(&self) -> Result<Vec<Host>, StoreError> {
        let op = "list hosts";
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {HOST_COLUMNS} FROM hosts ORDER BY host"))
            .map_err(ctx(op))?;
        stmt.query_map([], host_row)
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

    /// The host's last software lookup, if one was recorded.
    pub fn host_software(&self, host: &str) -> Result<Option<HostSoftware>, StoreError> {
        self.conn
            .query_row(
                "SELECT software, software_checked_at FROM hosts
                 WHERE host = ?1 AND software_checked_at IS NOT NULL",
                [host],
                |r| {
                    Ok(HostSoftware {
                        software: r.get(0)?,
                        checked_at: r.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(ctx(format!("read software of host {host}")))
    }

    /// Records a software lookup: the lowercased nodeinfo `software.name`, or `None` when the
    /// lookup failed, and when it was made.
    pub fn set_host_software(
        &self,
        host: &str,
        software: Option<&str>,
        checked_at: i64,
    ) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO hosts (host, software, software_checked_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (host) DO UPDATE SET software = excluded.software,
                     software_checked_at = excluded.software_checked_at",
                params![host, software, checked_at],
            )
            .map_err(ctx(format!("set software of host {host}")))?;
        Ok(())
    }

    /// The host's last robots.txt fetch, if one was recorded.
    pub fn host_robots(&self, host: &str) -> Result<Option<HostRobots>, StoreError> {
        self.conn
            .query_row(
                "SELECT robots_txt, robots_checked_at FROM hosts
                 WHERE host = ?1 AND robots_checked_at IS NOT NULL",
                [host],
                |r| {
                    Ok(HostRobots {
                        robots_txt: r.get(0)?,
                        checked_at: r.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(ctx(format!("read robots.txt of host {host}")))
    }

    /// Records a robots.txt fetch: the reduced rules (empty: allow everything), or `None` when
    /// robots.txt was unavailable, and when it was fetched.
    pub fn set_host_robots(
        &self,
        host: &str,
        robots_txt: Option<&str>,
        checked_at: i64,
    ) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO hosts (host, robots_txt, robots_checked_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (host) DO UPDATE SET robots_txt = excluded.robots_txt,
                     robots_checked_at = excluded.robots_checked_at",
                params![host, robots_txt, checked_at],
            )
            .map_err(ctx(format!("set robots.txt of host {host}")))?;
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

    /// Counts a revalidation of `host` that gave up during sweep pass `pass`, and returns how
    /// many passes in a row have had one. A pass counts once, however many of the host's
    /// revalidations fail in it (audit D5).
    pub fn record_failed_revalidation(&self, host: &str, pass: i64) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "INSERT INTO hosts (host, failed_revalidation_passes, failed_revalidation_pass)
                 VALUES (?1, 1, ?2)
                 ON CONFLICT (host) DO UPDATE SET
                     failed_revalidation_passes = CASE
                         WHEN hosts.failed_revalidation_pass IS NULL
                             OR hosts.failed_revalidation_pass < excluded.failed_revalidation_pass
                         THEN hosts.failed_revalidation_passes + 1
                         ELSE hosts.failed_revalidation_passes END,
                     failed_revalidation_pass = MAX(
                         COALESCE(hosts.failed_revalidation_pass, 0),
                         excluded.failed_revalidation_pass)
                 RETURNING failed_revalidation_passes",
                params![host, pass],
                |r| r.get(0),
            )
            .map_err(ctx(format!("count a failed revalidation of host {host}")))
    }

    /// Resets the host's failed revalidation passes: a revalidation succeeded. A no-op for
    /// unknown hosts and hosts with none.
    pub fn clear_failed_revalidations(&self, host: &str) -> Result<(), StoreError> {
        self.conn
            .execute(
                "UPDATE hosts SET failed_revalidation_passes = 0, failed_revalidation_pass = NULL
                 WHERE host = ?1 AND failed_revalidation_pass IS NOT NULL",
                [host],
            )
            .map_err(ctx(format!("clear failed revalidations of host {host}")))?;
        Ok(())
    }

    /// How many sweep passes in a row have had a failed revalidation of `host`.
    pub fn failed_revalidation_passes(&self, host: &str) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT failed_revalidation_passes FROM hosts WHERE host = ?1",
                [host],
                |r| r.get(0),
            )
            .optional()
            .map(Option::unwrap_or_default)
            .map_err(ctx(format!("read failed revalidations of host {host}")))
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
    fn failed_revalidations_count_once_per_pass_and_clear() -> TestResult {
        let s = Store::open_in_memory()?;
        assert_eq!(s.failed_revalidation_passes("a.example")?, 0);
        assert_eq!(s.record_failed_revalidation("a.example", 4)?, 1);
        // More of the host's records failing in the same pass don't count again.
        assert_eq!(s.record_failed_revalidation("a.example", 4)?, 1);
        assert_eq!(s.record_failed_revalidation("a.example", 5)?, 2);
        // A failure from an older pass, finishing late, doesn't move the pass back.
        assert_eq!(s.record_failed_revalidation("a.example", 3)?, 2);
        assert_eq!(s.record_failed_revalidation("a.example", 6)?, 3);
        assert_eq!(s.failed_revalidation_passes("b.example")?, 0);
        s.clear_failed_revalidations("a.example")?;
        assert_eq!(s.failed_revalidation_passes("a.example")?, 0);
        assert_eq!(s.record_failed_revalidation("a.example", 6)?, 1);
        // Clearing leaves the host's other state alone.
        s.set_backoff("a.example", 500)?;
        s.clear_failed_revalidations("a.example")?;
        assert_eq!(
            s.host("a.example")?.ok_or("missing")?.backoff_until,
            Some(500)
        );
        Ok(())
    }

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
    fn host_software() -> TestResult {
        let s = Store::open_in_memory()?;
        assert_eq!(s.host_software("l.example")?, None);
        // A host row from other state has no software lookup yet.
        s.set_backoff("l.example", 9)?;
        assert_eq!(s.host_software("l.example")?, None);
        s.set_host_software("l.example", Some("lemmy"), 100)?;
        assert_eq!(
            s.host_software("l.example")?,
            Some(HostSoftware {
                software: Some("lemmy".into()),
                checked_at: 100
            })
        );
        // Other host state is kept.
        let h = s.host("l.example")?.ok_or("missing")?;
        assert_eq!(h.backoff_until, Some(9));
        assert_eq!(h.software.as_deref(), Some("lemmy"));
        assert_eq!(h.software_checked_at, Some(100));
        // A failed lookup clears the name and records the time.
        s.set_host_software("l.example", None, 200)?;
        assert_eq!(
            s.host_software("l.example")?,
            Some(HostSoftware {
                software: None,
                checked_at: 200
            })
        );
        // A new host.
        s.set_host_software("g.example", Some("gotosocial"), 5)?;
        assert!(s.host("g.example")?.is_some_and(|h| !h.blocked));
        Ok(())
    }

    #[test]
    fn host_robots() -> TestResult {
        let s = Store::open_in_memory()?;
        assert_eq!(s.host_robots("l.example")?, None);
        s.set_host_software("l.example", Some("lemmy"), 5)?;
        assert_eq!(s.host_robots("l.example")?, None);
        s.set_host_robots("l.example", Some("User-agent: *\nDisallow: /a\n"), 100)?;
        assert_eq!(
            s.host_robots("l.example")?,
            Some(HostRobots {
                robots_txt: Some("User-agent: *\nDisallow: /a\n".into()),
                checked_at: 100
            })
        );
        let h = s.host("l.example")?.ok_or("missing")?;
        assert_eq!(h.software.as_deref(), Some("lemmy"));
        assert_eq!(h.robots_checked_at, Some(100));
        // Unavailable.
        s.set_host_robots("l.example", None, 200)?;
        assert_eq!(
            s.host_robots("l.example")?,
            Some(HostRobots {
                robots_txt: None,
                checked_at: 200
            })
        );
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
