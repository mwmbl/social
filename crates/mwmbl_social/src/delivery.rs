//! Sending our activities (`Accept`, `Follow`, `Undo`) to other servers' inboxes.
//!
//! A small, bounded, in-memory task: at most [`DeliveryConfig::max_pending`] deliveries are
//! held (more are dropped with a warning), at most [`DeliveryConfig::concurrency`] are sent at
//! once, and a delivery that fails transiently is retried after each of
//! [`DeliveryConfig::retry_delays`] (or the host's backoff, if longer), then given up. It is
//! **not persistent**: deliveries still pending at shutdown are lost. That is acceptable for what
//! we send. A lost `Accept` leaves a follow pending on the follower's side, and a lost `Follow`
//! or `Undo` leaves ours pending or in place; the next follow or unfollow sends them again.
//!
//! Sending goes through [`Fetcher::deliver`], so it shares the per-host limits, backoff and
//! signature scheme cache with fetching. Logs name the inbox and the activity id, never more.

use std::sync::Arc;
use std::time::Duration;

use apub::fetch::{DeliveryOutcome, Fetcher};
use serde_json::Value;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinSet;

use crate::now_secs;

/// Limits and retry schedule.
#[derive(Debug, Clone)]
pub struct DeliveryConfig {
    /// Deliveries held, waiting or in flight. More are dropped.
    pub max_pending: usize,
    /// Deliveries sent at once.
    pub concurrency: usize,
    /// The wait before each retry; its length is the number of retries.
    pub retry_delays: Vec<Duration>,
}

impl Default for DeliveryConfig {
    fn default() -> Self {
        Self {
            max_pending: 1024,
            concurrency: 8,
            retry_delays: vec![
                Duration::from_secs(30),
                Duration::from_secs(5 * 60),
                Duration::from_secs(30 * 60),
            ],
        }
    }
}

struct Delivery {
    inbox: String,
    /// The activity's id, for logs.
    id: String,
    body: Vec<u8>,
}

/// Cloneable access to the delivery task.
#[derive(Debug, Clone)]
pub struct Deliveries {
    tx: mpsc::Sender<Delivery>,
}

impl Deliveries {
    /// Queues `activity` for `inbox`. Returns `false` (and logs) if the task is full or stopped.
    pub fn send(&self, inbox: &str, activity: &Value) -> bool {
        let id = activity
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let body = match serde_json::to_vec(activity) {
            Ok(body) => body,
            Err(e) => {
                tracing::error!(inbox, id, error = %e, "failed to serialise an activity");
                return false;
            }
        };
        let delivery = Delivery {
            inbox: inbox.to_owned(),
            id,
            body,
        };
        match self.tx.try_send(delivery) {
            Ok(()) => true,
            Err(e) => {
                let delivery = match e {
                    mpsc::error::TrySendError::Full(d) | mpsc::error::TrySendError::Closed(d) => d,
                };
                tracing::warn!(
                    inbox = delivery.inbox,
                    id = delivery.id,
                    "delivery queue full or stopped; dropping the delivery"
                );
                false
            }
        }
    }
}

/// Starts the delivery task's channel. Run the returned future (it ends on `shutdown`).
pub fn start(
    fetcher: Arc<Fetcher>,
    config: DeliveryConfig,
    shutdown: watch::Receiver<bool>,
) -> (Deliveries, impl Future<Output = ()> + Send + 'static) {
    let (tx, rx) = mpsc::channel(config.max_pending.max(1));
    (Deliveries { tx }, run(rx, fetcher, config, shutdown))
}

async fn run(
    mut rx: mpsc::Receiver<Delivery>,
    fetcher: Arc<Fetcher>,
    config: DeliveryConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    let pending = Arc::new(Semaphore::new(config.max_pending.max(1)));
    let sending = Arc::new(Semaphore::new(config.concurrency.max(1)));
    let delays = Arc::new(config.retry_delays);
    let mut tasks = JoinSet::new();
    loop {
        if *shutdown.borrow() {
            break;
        }
        // Hold a pending slot before taking the next delivery, so a backlog waits in the
        // bounded channel (where `send` drops the excess) rather than in spawned tasks.
        let slot = tokio::select! {
            slot = Arc::clone(&pending).acquire_owned() => slot,
            changed = shutdown.changed() => if changed.is_err() { break } else { continue },
        };
        let Ok(slot) = slot else { break };
        let delivery = tokio::select! {
            delivery = rx.recv() => delivery,
            Some(_) = tasks.join_next() => continue,
            changed = shutdown.changed() => if changed.is_err() { break } else { continue },
        };
        let Some(delivery) = delivery else { break };
        let fetcher = Arc::clone(&fetcher);
        let sending = Arc::clone(&sending);
        let delays = Arc::clone(&delays);
        tasks.spawn(async move {
            deliver_with_retries(&fetcher, &sending, &delays, &delivery).await;
            drop(slot);
        });
        while tasks.try_join_next().is_some() {}
    }
    let lost = tasks.len();
    if lost > 0 {
        tracing::info!(
            lost,
            "shutting down with deliveries pending; they are not kept"
        );
    }
    tasks.abort_all();
}

async fn deliver_with_retries(
    fetcher: &Fetcher,
    sending: &Semaphore,
    delays: &[Duration],
    delivery: &Delivery,
) {
    let (inbox, id) = (delivery.inbox.as_str(), delivery.id.as_str());
    let mut retries = delays.iter();
    loop {
        let outcome = {
            let Ok(_permit) = sending.acquire().await else {
                return;
            };
            fetcher.deliver(inbox, &delivery.body).await
        };
        match &outcome {
            DeliveryOutcome::Delivered { status } => {
                tracing::debug!(inbox, id, status, "delivered");
                return;
            }
            outcome if outcome.is_transient() => {
                let Some(delay) = retries.next() else {
                    tracing::warn!(inbox, id, ?outcome, "delivery failed; giving up");
                    return;
                };
                let backoff = match outcome {
                    DeliveryOutcome::Backoff { until } => {
                        u64::try_from(until.saturating_sub(now_secs())).unwrap_or(0)
                    }
                    DeliveryOutcome::RateLimited { retry_after_secs } => *retry_after_secs,
                    _ => 0,
                };
                let wait = (*delay).max(Duration::from_secs(backoff));
                tracing::debug!(inbox, id, ?outcome, ?wait, "delivery failed; retrying");
                tokio::time::sleep(wait).await;
            }
            outcome => {
                tracing::warn!(inbox, id, ?outcome, "delivery refused");
                return;
            }
        }
    }
}
