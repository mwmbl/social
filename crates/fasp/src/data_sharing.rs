//! What the binary does around `data_sharing`: set it up when the capability is switched on,
//! tear down the local state when it is switched off, and keep backfills moving.
//!
//! The record of subscriptions and backfills lives in the store (bounded operational state).
//! Nothing here fetches objects or touches the index.

use store::{Backfill, BackfillState, Category, SubscriptionType};

use crate::StoreAccess;
use crate::client::FaspClient;
use crate::error::ClientError;

const CATEGORIES: [Category; 2] = [Category::Content, Category::Account];

/// The `maxBatchSize` we ask for. The spec makes it optional, but Mastodon 4.7.5 stores it
/// in a NOT NULL column and answers 500 without it. Its lifecycle announcements carry one
/// URI each anyway.
pub const MAX_BATCH_SIZE: u32 = 20;

/// What [`on_enable`] created in this run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EnableOutcome {
    pub subscriptions_created: usize,
    pub backfills_created: usize,
    /// Subscriptions from before a disable that were deleted on the server.
    pub orphans_deleted: usize,
}

/// What [`on_disable`] removed locally.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DisableOutcome {
    pub subscriptions_deleted: usize,
    pub backfills_deleted: usize,
}

/// What [`on_backfill_announcement`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackfillProgress {
    /// The announcement names a backfill we have no record of.
    Unknown,
    /// No more objects: the backfill is marked done.
    Done,
    /// More objects are available and the continuation was requested.
    Continued,
    /// More objects are available but the fetch queue had no room; `more_available` stays set
    /// and [`resume_backfills`] picks it up later.
    Deferred,
}

/// Switches `data_sharing` on for the client's registration: one `lifecycle` subscription and
/// one backfill request (for up to `max_count` objects) for each of `content` and `account`,
/// skipping any already recorded. Safe to call again after a partial failure: each step is
/// recorded as soon as the server accepts it.
pub async fn on_enable<S: StoreAccess>(
    client: &FaspClient<S>,
    store: &S,
    now: i64,
    max_count: u32,
) -> Result<EnableOutcome, ClientError> {
    let registration_id = client.registration_id();
    let data_sharing = client.data_sharing()?;
    let mut outcome = EnableOutcome::default();
    // Subscriptions left over from before a disable: delete them on the server now that we may
    // call it, or it keeps announcing to them alongside the new ones. A 404 means it already
    // has; any other failure leaves the orphan to retry next time without blocking setup.
    for remote_id in store.with_store(|s| s.list_orphaned_subscriptions(registration_id))? {
        match data_sharing.unsubscribe(&remote_id).await {
            Ok(()) | Err(ClientError::Status { status: 404, .. }) => {
                store
                    .with_store(|s| s.delete_orphaned_subscription(registration_id, &remote_id))?;
                outcome.orphans_deleted += 1;
            }
            Err(e) => tracing::warn!(
                registration = registration_id,
                remote_id,
                error = %e,
                "could not delete an orphaned subscription"
            ),
        }
    }
    for category in CATEGORIES {
        let has_subscription = store
            .with_store(|s| s.list_subscriptions(registration_id))?
            .iter()
            .any(|s| s.category == category && s.subscription_type == SubscriptionType::Lifecycle);
        if !has_subscription {
            let remote_id = data_sharing
                .subscribe(category, SubscriptionType::Lifecycle, Some(MAX_BATCH_SIZE))
                .await?;
            store.with_store(|s| {
                s.add_subscription(
                    registration_id,
                    &remote_id,
                    category,
                    SubscriptionType::Lifecycle,
                    now,
                )
            })?;
            outcome.subscriptions_created += 1;
        }
    }
    for category in CATEGORIES {
        let has_backfill = store
            .with_store(|s| s.list_backfills(registration_id))?
            .iter()
            .any(|b| b.category == category && b.state != BackfillState::Failed);
        if !has_backfill {
            let remote_id = data_sharing.request_backfill(category, max_count).await?;
            store.with_store(|s| s.add_backfill(registration_id, &remote_id, category))?;
            outcome.backfills_created += 1;
        }
    }
    tracing::info!(
        registration = registration_id,
        host = client.host(),
        subscriptions = outcome.subscriptions_created,
        backfills = outcome.backfills_created,
        "data_sharing set up"
    );
    Ok(outcome)
}

/// Switches `data_sharing` off locally: orphans the registration's subscriptions and deletes
/// its backfill rows. It does not call the server. The capability is already disabled on its
/// side, and calling a disabled capability's API is forbidden (CLAUDE.md principle 4).
/// Mastodon 4.7.5 keeps the subscriptions, so [`on_enable`] deletes the orphans on the server
/// once the capability is enabled again.
pub fn on_disable<S: StoreAccess>(
    store: &S,
    registration_id: i64,
) -> Result<DisableOutcome, store::StoreError> {
    store.with_store(|s| {
        Ok(DisableOutcome {
            subscriptions_deleted: s.orphan_subscriptions(registration_id)?,
            backfills_deleted: s.delete_backfills(registration_id)?,
        })
    })
}

/// Records a backfill announcement and, if the server has more, continues the backfill.
///
/// `more_available == false` marks the backfill done. Otherwise the continuation is requested
/// when `queue_has_room` (the binary's fetch queue can take more work); if the server answers
/// 404 the backfill is done after all. When there is no room, `more_available` is left set in
/// the store for [`resume_backfills`].
pub async fn on_backfill_announcement<S: StoreAccess>(
    client: &FaspClient<S>,
    store: &S,
    remote_id: &str,
    more_available: bool,
    queue_has_room: bool,
) -> Result<BackfillProgress, ClientError> {
    let registration_id = client.registration_id();
    let backfill = store
        .with_store(|s| s.list_backfills(registration_id))?
        .into_iter()
        .find(|b| b.remote_id == remote_id);
    let Some(backfill) = backfill else {
        return Ok(BackfillProgress::Unknown);
    };
    if !more_available {
        store.with_store(|s| s.update_backfill(backfill.id, BackfillState::Done, false))?;
        return Ok(BackfillProgress::Done);
    }
    store.with_store(|s| s.update_backfill(backfill.id, BackfillState::Active, true))?;
    if !queue_has_room {
        return Ok(BackfillProgress::Deferred);
    }
    continue_one(client, store, &backfill).await
}

/// Continues every active backfill of the client's registration that the server said has more
/// and that was deferred for lack of queue room. Returns how many continuations were requested.
pub async fn resume_backfills<S: StoreAccess>(
    client: &FaspClient<S>,
    store: &S,
    mut queue_has_room: impl FnMut() -> bool,
) -> Result<usize, ClientError> {
    let registration_id = client.registration_id();
    let pending: Vec<Backfill> = store
        .with_store(|s| s.list_backfills(registration_id))?
        .into_iter()
        .filter(|b| b.state == BackfillState::Active && b.more_available)
        .collect();
    let mut continued = 0;
    for backfill in pending {
        if !queue_has_room() {
            break;
        }
        if continue_one(client, store, &backfill).await? == BackfillProgress::Continued {
            continued += 1;
        }
    }
    Ok(continued)
}

async fn continue_one<S: StoreAccess>(
    client: &FaspClient<S>,
    store: &S,
    backfill: &Backfill,
) -> Result<BackfillProgress, ClientError> {
    if client
        .data_sharing()?
        .continue_backfill(&backfill.remote_id)
        .await?
    {
        // Requested: nothing is outstanding until the next announcement says so.
        store.with_store(|s| s.update_backfill(backfill.id, BackfillState::Active, false))?;
        Ok(BackfillProgress::Continued)
    } else {
        store.with_store(|s| s.update_backfill(backfill.id, BackfillState::Done, false))?;
        Ok(BackfillProgress::Done)
    }
}
