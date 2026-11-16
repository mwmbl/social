//! The `following` table: actors our server actor follows (PLAN.md Stage 3), with our Follow
//! activity's id so it can be undone and its Accept matched. Bounded by follow opt-ins and
//! admin actions.

use rusqlite::{OptionalExtension, Row, params};

use crate::enums::FollowState;
use crate::error::ctx;
use crate::{Store, StoreError};

/// An actor we follow, or asked to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Following {
    pub actor_uri: String,
    /// Where our activities for the actor are delivered.
    pub inbox: String,
    /// The id of our `Follow` activity.
    pub follow_id: String,
    pub state: FollowState,
    /// When we last sent the follow (Unix seconds): a re-sent follow updates it.
    pub created_at: i64,
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<Following> {
    Ok(Following {
        actor_uri: row.get(0)?,
        inbox: row.get(1)?,
        follow_id: row.get(2)?,
        state: row.get(3)?,
        created_at: row.get(4)?,
    })
}

impl Store {
    /// Records a pending follow of `actor_uri`. Returns `false`, changing nothing, if we follow
    /// (or asked to follow) the actor already.
    pub fn add_following(
        &self,
        actor_uri: &str,
        inbox: &str,
        follow_id: &str,
        now: i64,
    ) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .execute(
                "INSERT INTO following (actor_uri, inbox, follow_id, state, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (actor_uri) DO NOTHING",
                params![actor_uri, inbox, follow_id, FollowState::Pending, now],
            )
            .map_err(ctx(format!("add following of {actor_uri}")))?;
        Ok(changed > 0)
    }

    /// Our follow of `actor_uri`, if any.
    pub fn following(&self, actor_uri: &str) -> Result<Option<Following>, StoreError> {
        self.conn
            .query_row(
                "SELECT actor_uri, inbox, follow_id, state, created_at FROM following
                 WHERE actor_uri = ?1",
                [actor_uri],
                from_row,
            )
            .optional()
            .map_err(ctx(format!("read following of {actor_uri}")))
    }

    /// Marks our pending follow of `actor_uri` accepted. Returns whether a pending follow
    /// became accepted (`false` if there is none, or it was accepted already), so a repeated
    /// `Accept` is told apart from the first.
    pub fn accept_following(&self, actor_uri: &str) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .execute(
                "UPDATE following SET state = ?2 WHERE actor_uri = ?1 AND state = ?3",
                params![actor_uri, FollowState::Accepted, FollowState::Pending],
            )
            .map_err(ctx(format!("accept following of {actor_uri}")))?;
        Ok(changed > 0)
    }

    /// Forgets our follow of `actor_uri` (we unfollowed, or it was rejected). Returns whether
    /// there was one.
    pub fn remove_following(&self, actor_uri: &str) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .execute("DELETE FROM following WHERE actor_uri = ?1", [actor_uri])
            .map_err(ctx(format!("remove following of {actor_uri}")))?;
        Ok(changed > 0)
    }

    /// Every actor we follow or asked to, oldest first.
    pub fn list_following(&self) -> Result<Vec<Following>, StoreError> {
        let op = "list following";
        let mut stmt = self
            .conn
            .prepare(
                "SELECT actor_uri, inbox, follow_id, state, created_at FROM following
                 ORDER BY created_at, actor_uri",
            )
            .map_err(ctx(op))?;
        stmt.query_map([], from_row)
            .map_err(ctx(op))?
            .collect::<Result<_, _>>()
            .map_err(ctx(op))
    }

    /// Records that our follow of `actor_uri` was sent again at `now`, as `follow_id`: a re-sent
    /// `Follow` needs a new id, since Lemmy 0.19 rejects an activity id it has seen (400).
    /// Returns whether there was one.
    pub fn following_resent(
        &self,
        actor_uri: &str,
        follow_id: &str,
        now: i64,
    ) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .execute(
                "UPDATE following SET created_at = ?2, follow_id = ?3 WHERE actor_uri = ?1",
                params![actor_uri, now, follow_id],
            )
            .map_err(ctx(format!("record re-sent follow of {actor_uri}")))?;
        Ok(changed > 0)
    }

    /// How many actors we follow, by state.
    pub fn count_following(&self) -> Result<Vec<(FollowState, i64)>, StoreError> {
        let op = "count following";
        let mut stmt = self
            .conn
            .prepare("SELECT state, COUNT(*) FROM following GROUP BY state ORDER BY state")
            .map_err(ctx(op))?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
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

    #[test]
    fn add_accept_remove() -> TestResult {
        let s = Store::open_in_memory()?;
        assert_eq!(s.following(BOB)?, None);
        assert!(s.add_following(BOB, "https://gts.example/users/bob/inbox", "f1", 10)?);
        // A second follow keeps the first one's id.
        assert!(!s.add_following(BOB, "https://gts.example/other", "f2", 20)?);
        let row = s.following(BOB)?.ok_or("no row")?;
        assert_eq!(row.follow_id, "f1");
        assert_eq!(row.inbox, "https://gts.example/users/bob/inbox");
        assert_eq!(row.state, FollowState::Pending);
        assert_eq!(row.created_at, 10);
        assert_eq!(s.count_following()?, vec![(FollowState::Pending, 1)]);
        assert!(s.accept_following(BOB)?);
        // A second Accept changes nothing.
        assert!(!s.accept_following(BOB)?);
        assert_eq!(
            s.following(BOB)?.ok_or("no row")?.state,
            FollowState::Accepted
        );
        assert!(s.following_resent(BOB, "f2", 30)?);
        let row = s.following(BOB)?.ok_or("no row")?;
        assert_eq!((row.created_at, row.follow_id.as_str()), (30, "f2"));
        assert!(s.remove_following(BOB)?);
        assert!(!s.remove_following(BOB)?);
        assert!(!s.accept_following(BOB)?);
        assert!(!s.following_resent(BOB, "f3", 40)?);
        assert_eq!(s.following(BOB)?, None);
        Ok(())
    }

    #[test]
    fn list_is_oldest_first() -> TestResult {
        let s = Store::open_in_memory()?;
        assert_eq!(s.list_following()?, Vec::new());
        s.add_following(
            "https://l.example/c/b",
            "https://l.example/c/b/inbox",
            "f2",
            20,
        )?;
        s.add_following(BOB, "https://gts.example/users/bob/inbox", "f1", 10)?;
        let uris: Vec<String> = s
            .list_following()?
            .into_iter()
            .map(|f| f.actor_uri)
            .collect();
        assert_eq!(uris, [BOB, "https://l.example/c/b"]);
        Ok(())
    }
}
