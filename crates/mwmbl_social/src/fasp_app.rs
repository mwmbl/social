//! Our side of FASP, wired to the fetch queue: the [`FaspState`] the `fasp` crate's router
//! drives, plus the background work that keeps `data_sharing` set up and backfills moving.
//!
//! Request-path rules: announcements are acknowledged only after their URIs are queued, and the
//! SQLite work runs in `spawn_blocking`. Anything that calls a server's FASP API (setting up
//! `data_sharing`, continuing a backfill) runs on a spawned task, never in the request path.
//! Server calls go through [`FaspClient`], which refuses a capability's API unless the store
//! says it is enabled (CLAUDE.md principle 4). Nothing here writes an index: URIs only go into
//! the queue, and the worker and the single writer do the rest (principle 5).

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::routing::any_service;
use fasp::announcement::{Announcement, AnnouncementSource, EventType};
use fasp::client::FaspClient;
use fasp::data_sharing::{on_backfill_announcement, on_disable, on_enable, resume_backfills};
use fasp::http::{FaspState, router};
use fasp::{
    ClientError, DATA_SHARING_ID, DATA_SHARING_VERSION, PrivacyPolicy, ProviderInfoConfig,
    StoreAccess,
};
use store::{
    BackfillState, Category, FetchKind, FetchReason, NewFetch, Registration, RegistrationStatus,
    Store, SubscriptionType,
};
use tokio::sync::watch;

use crate::config::Config;
use crate::now_secs;

/// How often the background task looks for backfills to resume and enables to retry.
pub const MAINTAIN_EVERY: Duration = Duration::from_secs(60);

/// A backfill continuation is requested only while the queue is below this share of its cap.
const CONTINUE_BELOW_PERCENT: usize = 80;
/// Deferred backfills are resumed by the background task only below this share.
const RESUME_BELOW_PERCENT: usize = 50;

/// The paths our FASP router serves, as route patterns at the root of our base URL. Only these
/// reach it: the FASP router authenticates every request it gets, so it must not see the rest.
pub const FASP_ROUTES: [&str; 3] = [
    "/provider_info",
    "/capabilities/{*rest}",
    "/data_sharing/{*rest}",
];

struct Inner {
    store: Arc<Mutex<Store>>,
    http: reqwest::Client,
    base_url: String,
    info: ProviderInfoConfig,
    queue_cap: usize,
    backfill_max_count: u32,
    /// Registrations with FASP client work in progress, so none runs twice at once.
    busy: Mutex<HashSet<i64>>,
}

/// Our FASP state: cheap to clone, shared by the router and the background tasks.
#[derive(Clone)]
pub struct FaspApp {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for FaspApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaspApp")
            .field("base_url", &self.inner.base_url)
            .finish_non_exhaustive()
    }
}

/// What `provider_info` says about us, from the config.
#[must_use]
pub fn provider_info(config: &Config) -> ProviderInfoConfig {
    ProviderInfoConfig {
        name: config.name.clone(),
        privacy_policy: config
            .privacy_policy_url
            .iter()
            .map(|url| PrivacyPolicy {
                url: url.clone(),
                language: config.privacy_policy_language.clone(),
            })
            .collect(),
        contact_email: config.contact_email.clone(),
        fediverse_account: config.fediverse_account.clone(),
    }
}

/// Releases a registration's busy mark when dropped.
struct BusyGuard {
    app: FaspApp,
    registration_id: i64,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.app
            .inner
            .busy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.registration_id);
    }
}

/// Whether `len` is below `percent` of `cap`.
fn below_percent(len: usize, cap: usize, percent: usize) -> bool {
    len.saturating_mul(100) < cap.saturating_mul(percent)
}

impl FaspApp {
    /// `store` is this component's own connection. `base_url` is our public base URL.
    #[must_use]
    pub fn new(store: Store, http: reqwest::Client, config: &Config, base_url: &str) -> Self {
        Self {
            inner: Arc::new(Inner {
                store: Arc::new(Mutex::new(store)),
                http,
                base_url: base_url.to_string(),
                info: provider_info(config),
                queue_cap: config.queue_cap,
                backfill_max_count: config.backfill_max_count,
                busy: Mutex::new(HashSet::new()),
            }),
        }
    }

    /// The FASP routes as a router to merge into the main one. Each pattern in [`FASP_ROUTES`]
    /// is routed to the FASP router with the path intact; everything else stays with the main
    /// router (including its 404).
    pub fn routes(&self) -> Router {
        let fasp = router(Arc::new(self.clone()));
        FASP_ROUTES.iter().fold(Router::new(), |routes, pattern| {
            routes.route(pattern, any_service(fasp.clone()))
        })
    }

    fn try_claim(&self, registration_id: i64) -> Option<BusyGuard> {
        self.inner
            .busy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(registration_id)
            .then(|| BusyGuard {
                app: self.clone(),
                registration_id,
            })
    }

    fn queue_below(&self, percent: usize) -> bool {
        self.with_store(Store::queue_len)
            .is_ok_and(|len| below_percent(len, self.inner.queue_cap, percent))
    }

    fn client_for(&self, registration_id: i64) -> Result<Option<FaspClient<Mutex<Store>>>, String> {
        let registration = self
            .with_store(|s| s.registration(registration_id))
            .map_err(|e| e.to_string())?;
        let Some(registration) = registration else {
            return Ok(None);
        };
        if registration.status == RegistrationStatus::Revoked {
            return Ok(None);
        }
        FaspClient::new(
            self.inner.http.clone(),
            Arc::clone(&self.inner.store),
            &registration,
        )
        .map(Some)
        .map_err(|e| e.to_string())
    }

    /// Queues every URI. Announcements are admitted even when the queue is at its cap; the
    /// store defers backfill work instead. Returns how many URIs were taken.
    fn enqueue_all(&self, uris: &[String], kind: FetchKind, reason: FetchReason) -> usize {
        let now = now_secs();
        let mut taken = 0;
        for uri in uris {
            let item = NewFetch {
                uri: uri.clone(),
                kind,
                reason,
                record: None,
                not_before: now,
            };
            match self.with_store(|s| s.enqueue(&item, now, self.inner.queue_cap)) {
                Ok(store::EnqueueOutcome::Deferred) => {}
                Ok(_) => taken += 1,
                Err(e) => tracing::error!(uri, error = %e, "failed to queue an announced URI"),
            }
        }
        taken
    }

    /// Sets `data_sharing` up for a registration: subscriptions and backfills, skipping what
    /// exists. Safe to repeat. Failures are logged with the host and retried by
    /// [`FaspApp::maintain`].
    pub async fn enable(&self, registration_id: i64) {
        let Some(_guard) = self.try_claim(registration_id) else {
            tracing::debug!(
                registration = registration_id,
                "FASP work already in progress; the background task will retry"
            );
            return;
        };
        self.enable_claimed(registration_id).await;
    }

    async fn enable_claimed(&self, registration_id: i64) {
        let client = match self.client_for(registration_id) {
            Ok(Some(client)) => client,
            Ok(None) => return,
            Err(reason) => {
                tracing::warn!(registration = registration_id, %reason,
                    "cannot set up data_sharing: the registration is unusable");
                return;
            }
        };
        let result = on_enable(
            &client,
            &*self.inner.store,
            now_secs(),
            self.inner.backfill_max_count,
        )
        .await;
        if let Err(e) = result {
            log_client_error(&client, "set up data_sharing", &e);
        }
        // Deactivated while we were setting up: do not leave rows behind.
        let still_enabled = self.with_store(|s| {
            s.capability_enabled(registration_id, DATA_SHARING_ID, DATA_SHARING_VERSION)
        });
        if matches!(still_enabled, Ok(false))
            && let Err(e) = on_disable(self, registration_id)
        {
            tracing::error!(registration = registration_id, error = %e,
                "failed to remove data_sharing rows after deactivation");
        }
    }

    /// Whether an enabled `data_sharing` registration lacks a subscription or a backfill
    /// request for either category (an earlier set-up failed part-way).
    fn needs_enable(&self, registration_id: i64) -> bool {
        self.with_store(|s| {
            let subscriptions = s.list_subscriptions(registration_id)?;
            let backfills = s.list_backfills(registration_id)?;
            Ok::<_, store::StoreError>([Category::Content, Category::Account].iter().any(
                |&category| {
                    !subscriptions.iter().any(|sub| {
                        sub.category == category
                            && sub.subscription_type == SubscriptionType::Lifecycle
                    }) || !backfills
                        .iter()
                        .any(|b| b.category == category && b.state != BackfillState::Failed)
                },
            ))
        })
        .unwrap_or(false)
    }

    /// One pass of the background work: for each live registration with `data_sharing` enabled,
    /// retry a failed set-up, or resume deferred backfills when the queue has room. Each
    /// registration runs on its own task, at most one at a time.
    pub fn maintain(&self) {
        let registrations = match self.with_store(Store::list_registrations) {
            Ok(registrations) => registrations,
            Err(e) => {
                tracing::error!(error = %e, "failed to list registrations");
                return;
            }
        };
        for registration in registrations {
            if registration.status == RegistrationStatus::Revoked {
                continue;
            }
            let enabled = self.with_store(|s| {
                s.capability_enabled(registration.id, DATA_SHARING_ID, DATA_SHARING_VERSION)
            });
            if !matches!(enabled, Ok(true)) {
                continue;
            }
            let Some(guard) = self.try_claim(registration.id) else {
                continue;
            };
            let app = self.clone();
            tokio::spawn(async move {
                app.maintain_one(&registration).await;
                drop(guard);
            });
        }
    }

    async fn maintain_one(&self, registration: &Registration) {
        if self.needs_enable(registration.id) {
            self.enable_claimed(registration.id).await;
            return;
        }
        if !self.queue_below(RESUME_BELOW_PERCENT) {
            return;
        }
        let client = match self.client_for(registration.id) {
            Ok(Some(client)) => client,
            Ok(None) => return,
            Err(reason) => {
                tracing::warn!(registration = registration.id, %reason,
                    "cannot resume backfills: the registration is unusable");
                return;
            }
        };
        let result = resume_backfills(&client, &*self.inner.store, || {
            self.queue_below(RESUME_BELOW_PERCENT)
        })
        .await;
        match result {
            Ok(0) => {}
            Ok(continued) => tracing::info!(
                registration = registration.id,
                host = client.host(),
                continued,
                "resumed deferred backfills"
            ),
            Err(e) => log_client_error(&client, "resume backfills", &e),
        }
    }

    /// Runs [`FaspApp::maintain`] every `every` until shutdown.
    pub async fn run_maintenance(self, every: Duration, mut shutdown: watch::Receiver<bool>) {
        let mut interval = tokio::time::interval(every);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = shutdown.changed() => {}
            }
            if *shutdown.borrow() {
                break;
            }
            self.maintain();
        }
    }

    async fn continue_backfill(&self, registration_id: i64, remote_id: String, more: bool) {
        let room = self.queue_below(CONTINUE_BELOW_PERCENT);
        let client = match self.client_for(registration_id) {
            Ok(Some(client)) => client,
            Ok(None) => return,
            Err(reason) => {
                tracing::warn!(registration = registration_id, %reason,
                    "cannot continue a backfill: the registration is unusable");
                return;
            }
        };
        match on_backfill_announcement(&client, &*self.inner.store, &remote_id, more, room).await {
            Ok(progress) => tracing::debug!(
                registration = registration_id,
                host = client.host(),
                ?progress,
                "backfill announcement handled"
            ),
            Err(e) => log_client_error(&client, "continue a backfill", &e),
        }
    }
}

fn log_client_error(client: &FaspClient<Mutex<Store>>, what: &str, error: &ClientError) {
    match error {
        ClientError::NotEnabled(_) => tracing::info!(
            registration = client.registration_id(),
            host = client.host(),
            "did not {what}: data_sharing is not enabled"
        ),
        other => tracing::warn!(
            registration = client.registration_id(),
            host = client.host(),
            error = %other,
            "failed to {what}"
        ),
    }
}

impl StoreAccess for FaspApp {
    fn with_store<R>(&self, f: impl FnOnce(&Store) -> R) -> R {
        self.inner.store.with_store(f)
    }
}

impl FaspState for FaspApp {
    fn base_url(&self) -> &str {
        &self.inner.base_url
    }

    fn provider_info(&self) -> &ProviderInfoConfig {
        &self.inner.info
    }

    async fn on_capability_changed(&self, registration_id: i64, capability: &str, enabled: bool) {
        if capability != DATA_SHARING_ID {
            return;
        }
        if enabled {
            let app = self.clone();
            tokio::spawn(async move { app.enable(registration_id).await });
            return;
        }
        // Local rows only: the server is not called for a disabled capability.
        let app = self.clone();
        let removed = tokio::task::spawn_blocking(move || on_disable(&app, registration_id)).await;
        match removed {
            Ok(Ok(outcome)) => tracing::info!(
                registration = registration_id,
                subscriptions = outcome.subscriptions_deleted,
                backfills = outcome.backfills_deleted,
                "data_sharing switched off"
            ),
            Ok(Err(e)) => tracing::error!(registration = registration_id, error = %e,
                "failed to remove data_sharing rows"),
            Err(e) => tracing::error!(registration = registration_id, error = %e,
                "the data_sharing removal task failed"),
        }
    }

    async fn on_announcement(&self, registration_id: i64, announcement: Announcement) {
        let Announcement {
            source,
            category,
            object_uris,
        } = announcement;
        let kind = match category {
            Category::Content => FetchKind::Object,
            Category::Account => FetchKind::Actor,
        };
        let (reason, event, backfill) = match &source {
            AnnouncementSource::Subscription { event, .. } => {
                (FetchReason::Announce, Some(*event), None)
            }
            AnnouncementSource::Backfill {
                remote_id,
                more_available,
            } => (
                FetchReason::Backfill,
                None,
                Some((remote_id.clone(), *more_available)),
            ),
        };
        let count = object_uris.len();
        let app = self.clone();
        let queued =
            tokio::task::spawn_blocking(move || app.enqueue_all(&object_uris, kind, reason)).await;
        match queued {
            Ok(taken) => tracing::debug!(
                registration = registration_id,
                count,
                taken,
                ?category,
                delete = event == Some(EventType::Delete),
                "announcement queued"
            ),
            Err(e) => tracing::error!(registration = registration_id, error = %e,
                "the announcement queueing task failed"),
        }
        if let Some((remote_id, more)) = backfill {
            let app = self.clone();
            tokio::spawn(async move {
                app.continue_backfill(registration_id, remote_id, more)
                    .await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_thresholds() {
        assert!(below_percent(79, 100, 80));
        assert!(!below_percent(80, 100, 80));
        assert!(below_percent(0, 100, 50));
        assert!(!below_percent(50, 100, 50));
        assert!(!below_percent(0, 0, 80));
        assert!(below_percent(usize::MAX / 200, usize::MAX, 50));
    }

    #[test]
    fn provider_info_comes_from_the_config() {
        let mut config = Config::default();
        let bare = provider_info(&config);
        assert_eq!(bare.name, "Mwmbl Social");
        assert_eq!(bare.privacy_policy.len(), 0);
        assert_eq!(bare.contact_email, None);
        assert_eq!(bare.fediverse_account, None);

        config.privacy_policy_url = Some("https://social.example/privacy".into());
        config.contact_email = Some("admin@social.example".into());
        config.fediverse_account = Some("@admin@social.example".into());
        let full = provider_info(&config);
        assert_eq!(
            full.privacy_policy,
            vec![PrivacyPolicy {
                url: "https://social.example/privacy".into(),
                language: "en".into()
            }]
        );
        config.privacy_policy_language = "de".into();
        assert_eq!(
            provider_info(&config)
                .privacy_policy
                .first()
                .map(|p| p.language.as_str()),
            Some("de")
        );
        assert_eq!(full.contact_email.as_deref(), Some("admin@social.example"));
        assert_eq!(
            full.fediverse_account.as_deref(),
            Some("@admin@social.example")
        );
    }
}
