//! The `follow_optins` table: authors who opted in to indexing by following our server actor
//! (PLAN.md Stage 3). A record of a user action, bounded by those actions.

use rusqlite::{OptionalExtension, params};

use crate::error::ctx;
use crate::{Store, StoreError};

/// An author who follows our server actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowOptin {
    pub actor_uri: String,
    /// When the follow arrived (Unix seconds).
    pub followed_at: i64,
}

impl Store {
    /// Records that `actor_uri` follows us. Re-adding keeps the original `followed_at`.
    pub fn add_follow_optin(&self, actor_uri: &str, now: i64) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO follow_optins (actor_uri, followed_at) VALUES (?1, ?2)
                 ON CONFLICT (actor_uri) DO NOTHING",
                params![actor_uri, now],
            )
            .map_err(ctx(format!("add follow opt-in of {actor_uri}")))?;
        Ok(())
    }

    /// Forgets a follow (the author unfollowed). Returns whether there was one.
    pub fn remove_follow_optin(&self, actor_uri: &str) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .execute(
                "DELETE FROM follow_optins WHERE actor_uri = ?1",
                [actor_uri],
            )
            .map_err(ctx(format!("remove follow opt-in of {actor_uri}")))?;
        Ok(changed > 0)
    }

    /// Whether `actor_uri` follows us.
    pub fn is_follow_optin(&self, actor_uri: &str) -> Result<bool, StoreError> {
        self.conn
            .query_row(
                "SELECT 1 FROM follow_optins WHERE actor_uri = ?1",
                [actor_uri],
                |_| Ok(()),
            )
            .optional()
            .map(|found| found.is_some())
            .map_err(ctx(format!("read follow opt-in of {actor_uri}")))
    }

    /// Every author who follows us, by URI.
    pub fn follow_optins(&self) -> Result<Vec<FollowOptin>, StoreError> {
        let op = "list follow opt-ins";
        let mut stmt = self
            .conn
            .prepare("SELECT actor_uri, followed_at FROM follow_optins ORDER BY actor_uri")
            .map_err(ctx(op))?;
        stmt.query_map([], |r| {
            Ok(FollowOptin {
                actor_uri: r.get(0)?,
                followed_at: r.get(1)?,
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

    const BOB: &str = "https://gts.example/users/bob";
    const EVE: &str = "https://gts.example/users/eve";

    #[test]
    fn add_check_list_remove() -> TestResult {
        let s = Store::open_in_memory()?;
        assert!(!s.is_follow_optin(BOB)?);
        assert_eq!(s.follow_optins()?, []);
        s.add_follow_optin(BOB, 10)?;
        s.add_follow_optin(BOB, 20)?;
        s.add_follow_optin(EVE, 30)?;
        assert!(s.is_follow_optin(BOB)?);
        assert_eq!(
            s.follow_optins()?,
            vec![
                FollowOptin {
                    actor_uri: BOB.into(),
                    followed_at: 10
                },
                FollowOptin {
                    actor_uri: EVE.into(),
                    followed_at: 30
                },
            ]
        );
        assert!(s.remove_follow_optin(BOB)?);
        assert!(!s.remove_follow_optin(BOB)?);
        assert!(!s.is_follow_optin(BOB)?);
        assert!(s.is_follow_optin(EVE)?);
        Ok(())
    }
}
