//! `subscriptions` and `backfills` tables.

use rusqlite::{Row, params};

use crate::enums::{BackfillState, Category, SubscriptionType};
use crate::error::{ctx, expect_changed};
use crate::{Store, StoreError};

/// A data-sharing subscription we hold on a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    pub id: i64,
    pub registration_id: i64,
    /// The provider's id for the subscription.
    pub remote_id: String,
    pub category: Category,
    pub subscription_type: SubscriptionType,
    pub created_at: i64,
}

/// A backfill request made to a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backfill {
    pub id: i64,
    pub registration_id: i64,
    pub remote_id: String,
    pub category: Category,
    pub state: BackfillState,
    pub more_available: bool,
}

fn sub_row(row: &Row<'_>) -> rusqlite::Result<Subscription> {
    Ok(Subscription {
        id: row.get(0)?,
        registration_id: row.get(1)?,
        remote_id: row.get(2)?,
        category: row.get(3)?,
        subscription_type: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn backfill_row(row: &Row<'_>) -> rusqlite::Result<Backfill> {
    Ok(Backfill {
        id: row.get(0)?,
        registration_id: row.get(1)?,
        remote_id: row.get(2)?,
        category: row.get(3)?,
        state: row.get(4)?,
        more_available: row.get(5)?,
    })
}

impl Store {
    pub fn add_subscription(
        &self,
        registration_id: i64,
        remote_id: &str,
        category: Category,
        subscription_type: SubscriptionType,
        now: i64,
    ) -> Result<Subscription, StoreError> {
        self.conn
            .execute(
                "INSERT INTO subscriptions
                    (registration_id, remote_id, category, subscription_type, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![registration_id, remote_id, category, subscription_type, now],
            )
            .map_err(ctx(format!(
                "add subscription {remote_id} on registration {registration_id}"
            )))?;
        Ok(Subscription {
            id: self.conn.last_insert_rowid(),
            registration_id,
            remote_id: remote_id.to_owned(),
            category,
            subscription_type,
            created_at: now,
        })
    }

    pub fn list_subscriptions(
        &self,
        registration_id: i64,
    ) -> Result<Vec<Subscription>, StoreError> {
        let op = format!("list subscriptions of registration {registration_id}");
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, registration_id, remote_id, category, subscription_type, created_at
                 FROM subscriptions WHERE registration_id = ?1 ORDER BY id",
            )
            .map_err(ctx(op.clone()))?;
        stmt.query_map([registration_id], sub_row)
            .map_err(ctx(op.clone()))?
            .collect::<Result<_, _>>()
            .map_err(ctx(op))
    }

    /// Stop using a registration's subscriptions while its capability is disabled: move them to
    /// `orphaned_subscriptions`, to be deleted on the server once calls are allowed again.
    /// Returns how many were moved.
    pub fn orphan_subscriptions(&self, registration_id: i64) -> Result<usize, StoreError> {
        let op = format!("orphan subscriptions of registration {registration_id}");
        let tx = self.conn.unchecked_transaction().map_err(ctx(op.clone()))?;
        tx.execute(
            "INSERT OR IGNORE INTO orphaned_subscriptions (registration_id, remote_id)
             SELECT registration_id, remote_id FROM subscriptions WHERE registration_id = ?1",
            [registration_id],
        )
        .map_err(ctx(op.clone()))?;
        let moved = tx
            .execute(
                "DELETE FROM subscriptions WHERE registration_id = ?1",
                [registration_id],
            )
            .map_err(ctx(op.clone()))?;
        tx.commit().map_err(ctx(op))?;
        Ok(moved)
    }

    /// The remote ids of a registration's orphaned subscriptions.
    pub fn list_orphaned_subscriptions(
        &self,
        registration_id: i64,
    ) -> Result<Vec<String>, StoreError> {
        let op = format!("list orphaned subscriptions of registration {registration_id}");
        let mut stmt = self
            .conn
            .prepare(
                "SELECT remote_id FROM orphaned_subscriptions WHERE registration_id = ?1
                 ORDER BY remote_id",
            )
            .map_err(ctx(op.clone()))?;
        stmt.query_map([registration_id], |r| r.get(0))
            .map_err(ctx(op.clone()))?
            .collect::<Result<_, _>>()
            .map_err(ctx(op))
    }

    /// Forget an orphaned subscription once the server has deleted it.
    pub fn delete_orphaned_subscription(
        &self,
        registration_id: i64,
        remote_id: &str,
    ) -> Result<(), StoreError> {
        self.conn
            .execute(
                "DELETE FROM orphaned_subscriptions WHERE registration_id = ?1 AND remote_id = ?2",
                params![registration_id, remote_id],
            )
            .map_err(ctx(format!(
                "delete orphaned subscription {remote_id} on registration {registration_id}"
            )))?;
        Ok(())
    }

    /// Returns whether a subscription was deleted.
    pub fn delete_subscription(
        &self,
        registration_id: i64,
        remote_id: &str,
    ) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .execute(
                "DELETE FROM subscriptions WHERE registration_id = ?1 AND remote_id = ?2",
                params![registration_id, remote_id],
            )
            .map_err(ctx(format!(
                "delete subscription {remote_id} on registration {registration_id}"
            )))?;
        Ok(changed > 0)
    }

    /// Starts tracking a backfill, in state `active` with `more_available` false.
    pub fn add_backfill(
        &self,
        registration_id: i64,
        remote_id: &str,
        category: Category,
    ) -> Result<Backfill, StoreError> {
        self.conn
            .execute(
                "INSERT INTO backfills (registration_id, remote_id, category, state, more_available)
                 VALUES (?1, ?2, ?3, 'active', 0)",
                params![registration_id, remote_id, category],
            )
            .map_err(ctx(format!("add backfill {remote_id} on registration {registration_id}")))?;
        Ok(Backfill {
            id: self.conn.last_insert_rowid(),
            registration_id,
            remote_id: remote_id.to_owned(),
            category,
            state: BackfillState::Active,
            more_available: false,
        })
    }

    pub fn update_backfill(
        &self,
        id: i64,
        state: BackfillState,
        more_available: bool,
    ) -> Result<(), StoreError> {
        let op = "update backfill";
        let changed = self
            .conn
            .execute(
                "UPDATE backfills SET state = ?2, more_available = ?3 WHERE id = ?1",
                params![id, state, more_available],
            )
            .map_err(ctx(format!("{op} {id}")))?;
        expect_changed(changed, op, || format!("backfill {id}"))
    }

    /// Every backfill of a registration, in any state, oldest first.
    pub fn list_backfills(&self, registration_id: i64) -> Result<Vec<Backfill>, StoreError> {
        let op = format!("list backfills of registration {registration_id}");
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, registration_id, remote_id, category, state, more_available
                 FROM backfills WHERE registration_id = ?1 ORDER BY id",
            )
            .map_err(ctx(op.clone()))?;
        stmt.query_map([registration_id], backfill_row)
            .map_err(ctx(op.clone()))?
            .collect::<Result<_, _>>()
            .map_err(ctx(op))
    }

    /// Deletes every subscription row of a registration, returning how many. Local only.
    pub fn delete_subscriptions(&self, registration_id: i64) -> Result<usize, StoreError> {
        self.conn
            .execute(
                "DELETE FROM subscriptions WHERE registration_id = ?1",
                [registration_id],
            )
            .map_err(ctx(format!(
                "delete subscriptions of registration {registration_id}"
            )))
    }

    /// Deletes every backfill row of a registration, returning how many. Local only.
    pub fn delete_backfills(&self, registration_id: i64) -> Result<usize, StoreError> {
        self.conn
            .execute(
                "DELETE FROM backfills WHERE registration_id = ?1",
                [registration_id],
            )
            .map_err(ctx(format!(
                "delete backfills of registration {registration_id}"
            )))
    }

    pub fn list_active_backfills(&self) -> Result<Vec<Backfill>, StoreError> {
        let op = "list active backfills";
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, registration_id, remote_id, category, state, more_available
                 FROM backfills WHERE state = 'active' ORDER BY id",
            )
            .map_err(ctx(op))?;
        stmt.query_map([], backfill_row)
            .map_err(ctx(op))?
            .collect::<Result<_, _>>()
            .map_err(ctx(op))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewRegistration;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn reg(s: &Store, name: &str) -> Result<i64, StoreError> {
        Ok(s.create_registration(NewRegistration {
            base_url: format!("https://{name}"),
            server_id: name.to_owned(),
            our_private_key: vec![0; 32],
            created_at: 0,
        })?
        .id)
    }

    #[test]
    fn subscription_add_list_delete() -> TestResult {
        let s = Store::open_in_memory()?;
        let (a, b) = (reg(&s, "a")?, reg(&s, "b")?);
        let sub = s.add_subscription(a, "r1", Category::Content, SubscriptionType::Lifecycle, 5)?;
        s.add_subscription(a, "r2", Category::Account, SubscriptionType::Trends, 6)?;
        s.add_subscription(b, "r1", Category::Content, SubscriptionType::Lifecycle, 7)?;
        assert_eq!(s.list_subscriptions(a)?.len(), 2);
        assert_eq!(s.list_subscriptions(a)?.first(), Some(&sub));
        assert_eq!(s.list_subscriptions(b)?.len(), 1);
        // UNIQUE(registration_id, remote_id)
        assert!(
            s.add_subscription(a, "r1", Category::Content, SubscriptionType::Lifecycle, 8)
                .is_err()
        );
        assert!(s.delete_subscription(a, "r1")?);
        assert!(!s.delete_subscription(a, "r1")?);
        assert_eq!(s.list_subscriptions(a)?.len(), 1);
        assert_eq!(s.list_subscriptions(b)?.len(), 1);
        Ok(())
    }

    #[test]
    fn list_and_delete_per_registration() -> TestResult {
        let s = Store::open_in_memory()?;
        let (a, b) = (reg(&s, "a")?, reg(&s, "b")?);
        s.add_subscription(a, "r1", Category::Content, SubscriptionType::Lifecycle, 5)?;
        s.add_subscription(a, "r2", Category::Account, SubscriptionType::Lifecycle, 5)?;
        s.add_subscription(b, "r1", Category::Content, SubscriptionType::Lifecycle, 5)?;
        let done = s.add_backfill(a, "bf1", Category::Content)?;
        s.update_backfill(done.id, BackfillState::Done, false)?;
        s.add_backfill(a, "bf2", Category::Account)?;
        s.add_backfill(b, "bf1", Category::Content)?;
        // list_backfills includes finished ones, unlike list_active_backfills.
        assert_eq!(s.list_backfills(a)?.len(), 2);
        assert_eq!(s.list_active_backfills()?.len(), 2);
        assert_eq!(s.delete_subscriptions(a)?, 2);
        assert_eq!(s.delete_backfills(a)?, 2);
        assert_eq!(s.delete_subscriptions(a)?, 0);
        assert_eq!(s.list_subscriptions(b)?.len(), 1);
        assert_eq!(s.list_backfills(b)?.len(), 1);
        Ok(())
    }

    #[test]
    fn subscriptions_cascade_with_registration() -> TestResult {
        let s = Store::open_in_memory()?;
        let a = reg(&s, "a")?;
        s.add_subscription(a, "r1", Category::Content, SubscriptionType::Lifecycle, 5)?;
        s.conn
            .execute("DELETE FROM registrations WHERE id = ?1", [a])?;
        assert_eq!(s.list_subscriptions(a)?.len(), 0);
        Ok(())
    }

    #[test]
    fn backfill_lifecycle() -> TestResult {
        let s = Store::open_in_memory()?;
        let a = reg(&s, "a")?;
        let b1 = s.add_backfill(a, "bf1", Category::Content)?;
        let b2 = s.add_backfill(a, "bf2", Category::Account)?;
        assert_eq!(b1.state, BackfillState::Active);
        assert_eq!(s.list_active_backfills()?, vec![b1.clone(), b2]);
        s.update_backfill(b1.id, BackfillState::Active, true)?;
        assert!(
            s.list_active_backfills()?
                .first()
                .is_some_and(|b| b.more_available)
        );
        s.update_backfill(b1.id, BackfillState::Done, false)?;
        s.update_backfill(b1.id + 1, BackfillState::Failed, false)?;
        assert_eq!(s.list_active_backfills()?.len(), 0);
        assert!(matches!(
            s.update_backfill(999, BackfillState::Done, false),
            Err(StoreError::NotFound { .. })
        ));
        assert!(s.add_backfill(999, "x", Category::Content).is_err());
        Ok(())
    }
}
