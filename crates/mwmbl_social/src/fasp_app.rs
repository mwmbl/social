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
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use axum::Router;
use axum::routing::any_service;
use fasp::announcement::{Announcement, AnnouncementSource, EventType};
use fasp::client::FaspClient;
use fasp::data_sharing::TrendsThreshold;
use fasp::data_sharing::{on_backfill_announcement, on_disable, on_enable, resume_backfills};
use fasp::http::{AccountSearchPage, FaspState, TrendingContent, TrendingTag, TrendsQuery, router};
use fasp::{
    ACCOUNT_SEARCH_ID, ACCOUNT_SEARCH_VERSION, Capability, ClientError, DATA_SHARING_ID,
    DATA_SHARING_VERSION, FOLLOW_RECOMMENDATION_ID, FOLLOW_RECOMMENDATION_VERSION, PrivacyPolicy,
    ProviderInfoConfig, StoreAccess, TRENDS_ID, TRENDS_VERSION,
};
use social_index::{
    SocialAccount, SocialPost, SocialReader, Tombstones, language_matches, related_accounts,
    search_accounts,
};
use store::{
    BackfillState, Category, FetchKind, FetchReason, NewFetch, Registration, RegistrationStatus,
    Store, SubscriptionType,
};
use tokio::sync::watch;

use crate::config::Config;
use crate::now_secs;
use crate::trends::{MAX_WINDOW_HOURS, TrendState, rank};

/// How often the background task looks for backfills to resume and enables to retry.
pub const MAINTAIN_EVERY: Duration = Duration::from_secs(60);

/// A backfill continuation is requested only while the queue is below this share of its cap.
const CONTINUE_BELOW_PERCENT: usize = 80;
/// Deferred backfills are resumed by the background task only below this share.
const RESUME_BELOW_PERCENT: usize = 50;

/// The paths our FASP router serves, as route patterns at the root of our base URL. Only these
/// reach it: the FASP router authenticates every request it gets, so it must not see the rest.
pub const FASP_ROUTES: [&str; 6] = [
    "/provider_info",
    "/capabilities/{*rest}",
    "/data_sharing/{*rest}",
    "/account_search/{*rest}",
    "/trends/{*rest}",
    "/follow_recommendation/{*rest}",
];

/// The most trend-state URIs looked up in the posts index for one request. Each lookup is a
/// page read, so this bounds a request's cost whatever the trend state holds.
const MAX_TREND_LOOKUPS: usize = 500;
/// Example post URIs given with each trending hashtag.
const HASHTAG_EXAMPLES: usize = 3;
/// The most accounts one follow recommendation returns. Mastodon fetches each one it does not
/// know, so this is kept small.
pub const MAX_RECOMMENDATIONS: usize = 20;

struct Inner {
    store: Arc<Mutex<Store>>,
    http: reqwest::Client,
    base_url: String,
    info: ProviderInfoConfig,
    queue_cap: usize,
    backfill_max_count: u32,
    /// Registrations with FASP client work in progress, so none runs twice at once.
    busy: Mutex<HashSet<i64>>,
    /// The accounts index, read-only, for `account_search`. Results are pooled across every
    /// registration (PLAN.md, Multi-tenancy).
    accounts: SocialReader<SocialAccount>,
    /// The posts index, read-only, so trends only return posts we have indexed.
    posts: SocialReader<SocialPost>,
    /// The writer's tombstones: blocked hosts and forgotten accounts never appear.
    tombstones: Arc<RwLock<Tombstones>>,
    /// `trending` announcements, pooled across registrations (bounded; see `crate::trends`).
    trends: Mutex<TrendState>,
    trends_threshold: TrendsThreshold,
}

/// How long to wait before fetching an object an `update` announcement names. Mastodon 4.7.5
/// caches a status's `ActivityPub` JSON for 3 minutes under a key that ignores `updated_at`, and
/// announces the update at once, so fetching straight away gets the old version.
pub const UPDATE_FETCH_DELAY_SECS: i64 = 4 * 60;

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
        sign_in_url: config.sign_in_url.clone(),
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
    /// `readers` and `tombstones` are the writer's (see `WriterHandle::readers` and
    /// `WriterHandle::tombstones`), which `account_search`, `trends` and
    /// `follow_recommendation` read.
    #[must_use]
    pub fn new(
        store: Store,
        http: reqwest::Client,
        config: &Config,
        base_url: &str,
        readers: (SocialReader<SocialPost>, SocialReader<SocialAccount>),
        tombstones: Arc<RwLock<Tombstones>>,
    ) -> Self {
        let (posts, accounts) = readers;
        Self {
            inner: Arc::new(Inner {
                store: Arc::new(Mutex::new(store)),
                http,
                base_url: base_url.to_string(),
                info: provider_info(config),
                queue_cap: config.queue_cap,
                backfill_max_count: config.backfill_max_count,
                busy: Mutex::new(HashSet::new()),
                accounts,
                posts,
                tombstones,
                trends: Mutex::new(TrendState::new(config.trends_max_uris)),
                trends_threshold: config.trends_threshold(),
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
    fn enqueue_all(
        &self,
        uris: &[String],
        kind: FetchKind,
        reason: FetchReason,
        delay_secs: i64,
    ) -> usize {
        let now = now_secs();
        let mut taken = 0;
        for uri in uris {
            let item = NewFetch {
                uri: uri.clone(),
                kind,
                reason,
                record: None,
                not_before: now + delay_secs,
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
            Some(self.inner.trends_threshold),
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
    /// request for either category, or the content `trends` subscription (an earlier set-up
    /// failed part-way, or it predates trends).
    fn needs_enable(&self, registration_id: i64) -> bool {
        self.with_store(|s| {
            let subscriptions = s.list_subscriptions(registration_id)?;
            let backfills = s.list_backfills(registration_id)?;
            let no_trends = !subscriptions.iter().any(|sub| {
                sub.category == Category::Content
                    && sub.subscription_type == SubscriptionType::Trends
            });
            Ok::<_, store::StoreError>(
                no_trends
                    || [Category::Content, Category::Account]
                        .iter()
                        .any(|&category| {
                            !subscriptions.iter().any(|sub| {
                                sub.category == category
                                    && sub.subscription_type == SubscriptionType::Lifecycle
                            }) || !backfills
                                .iter()
                                .any(|b| b.category == category && b.state != BackfillState::Failed)
                        }),
            )
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

impl FaspApp {
    /// Records `trending` announcements and queues the URIs we have not indexed, so a post
    /// that trends before we have seen it can be fetched (from its origin, and judged for
    /// consent like any announced object) and then served. They are queued as backfill, so a
    /// full queue defers them rather than displacing lifecycle announcements.
    fn record_trending(&self, uris: &[String]) -> usize {
        let now = now_secs();
        {
            let mut trends = self
                .inner
                .trends
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for uri in uris {
                trends.record(uri, now);
            }
        }
        let missing: Vec<String> = uris
            .iter()
            .filter(|uri| matches!(self.inner.posts.find(uri), Ok(None)))
            .cloned()
            .collect();
        self.enqueue_all(&missing, FetchKind::Object, FetchReason::Backfill, 0)
    }

    /// Trending posts in the last `hours` hours that we have indexed and may show, with their
    /// event counts, best first, at most `want`. A URI missing from the posts index (not yet
    /// fetched, refused for consent, or fallen off its page) or tombstoned is skipped, so
    /// trends never surface anything we could not index. With a `language`, a post must
    /// carry a matching one.
    fn trending_posts(
        &self,
        now: i64,
        hours: i64,
        language: Option<&str>,
        want: usize,
    ) -> Result<Vec<(SocialPost, u64)>, String> {
        let ranked = self
            .inner
            .trends
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .ranked(now, hours);
        let tombstones = self
            .inner
            .tombstones
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let mut found = Vec::new();
        for trending in ranked.into_iter().take(MAX_TREND_LOOKUPS) {
            if found.len() >= want {
                break;
            }
            let post = self
                .inner
                .posts
                .find(&trending.uri)
                .map_err(|e| format!("looking up {} failed: {e}", trending.uri))?;
            let Some(post) = post else { continue };
            if tombstones.blocks(&post) {
                continue;
            }
            if let Some(range) = language
                && !post
                    .language
                    .as_ref()
                    .is_some_and(|l| language_matches(Some(std::slice::from_ref(l)), range))
            {
                continue;
            }
            found.push((post, trending.count));
        }
        Ok(found)
    }

    /// Hashtags of the trending posts, each scored by the sum of its posts' event counts, with
    /// the best of those posts as examples.
    fn trending_tags(posts: &[(SocialPost, u64)]) -> Vec<TrendingTag> {
        let mut tags: Vec<(String, u64, Vec<String>)> = Vec::new();
        for (post, count) in posts {
            let text = match &post.title {
                Some(title) => format!("{title} {}", post.extract),
                None => post.extract.clone(),
            };
            for tag in tokenize::special_keys(&text)
                .into_iter()
                .filter(|k| k.starts_with('#') && k.len() > 1)
            {
                match tags.iter_mut().find(|(name, _, _)| *name == tag) {
                    Some((_, score, examples)) => {
                        *score = score.saturating_add(*count);
                        if examples.len() < HASHTAG_EXAMPLES {
                            examples.push(post.uri.clone());
                        }
                    }
                    None => tags.push((tag, *count, vec![post.uri.clone()])),
                }
            }
        }
        tags.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        tags.into_iter()
            .map(|(name, score, examples)| TrendingTag {
                name,
                rank: rank(score),
                examples,
            })
            .collect()
    }

    /// Follow recommendations for `account_uri`: accounts related by the words of its own
    /// indexed name and bio (only if we have indexed it, so only if it is discoverable), then
    /// authors of this week's trending posts. Every result is an indexed (so discoverable),
    /// non-tombstoned account other than the one asking.
    fn recommend(&self, account_uri: &str, language: Option<&str>) -> Result<Vec<String>, String> {
        let accounts = &self.inner.accounts;
        let mut out: Vec<String> = Vec::new();
        let push = |uri: &str, out: &mut Vec<String>| {
            if uri != account_uri
                && out.len() < MAX_RECOMMENDATIONS
                && !out.iter().any(|u| u == uri)
            {
                out.push(uri.to_owned());
            }
        };
        let trending =
            self.trending_posts(now_secs(), MAX_WINDOW_HOURS, None, MAX_TREND_LOOKUPS)?;
        let tombstones = self
            .inner
            .tombstones
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let lookup_failed = |e: tinyindex::IndexError| format!("account lookup failed: {e}");
        if let Some(me) = accounts
            .find(account_uri)
            .map_err(lookup_failed)?
            .filter(|a| !tombstones.blocks(a))
        {
            for related in
                related_accounts(accounts, &me, &tombstones, language).map_err(lookup_failed)?
            {
                push(&related.uri, &mut out);
            }
        }
        for (post, _) in &trending {
            if out.len() >= MAX_RECOMMENDATIONS {
                break;
            }
            let Some(author) = accounts.find(&post.author_uri).map_err(lookup_failed)? else {
                continue;
            };
            if tombstones.blocks(&author)
                || language.is_some_and(|l| !language_matches(author.languages.as_deref(), l))
            {
                continue;
            }
            push(&author.uri, &mut out);
        }
        Ok(out)
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

    fn extra_capabilities(&self) -> Vec<Capability> {
        vec![
            Capability::new(ACCOUNT_SEARCH_ID, ACCOUNT_SEARCH_VERSION),
            Capability::new(TRENDS_ID, TRENDS_VERSION),
            Capability::new(FOLLOW_RECOMMENDATION_ID, FOLLOW_RECOMMENDATION_VERSION),
        ]
    }

    async fn trending_content(
        &self,
        _registration_id: i64,
        query: &TrendsQuery,
    ) -> Result<Vec<TrendingContent>, String> {
        let app = self.clone();
        let query = query.clone();
        tokio::task::spawn_blocking(move || {
            let posts = app.trending_posts(
                now_secs(),
                i64::from(query.within_last_hours),
                query.language.as_deref(),
                query.max_count,
            )?;
            Ok(posts
                .into_iter()
                .map(|(post, count)| TrendingContent {
                    uri: post.uri,
                    rank: rank(count),
                })
                .collect())
        })
        .await
        .map_err(|e| format!("the trends task failed: {e}"))?
    }

    async fn trending_hashtags(
        &self,
        _registration_id: i64,
        query: &TrendsQuery,
    ) -> Result<Vec<TrendingTag>, String> {
        let app = self.clone();
        let query = query.clone();
        tokio::task::spawn_blocking(move || {
            let posts = app.trending_posts(
                now_secs(),
                i64::from(query.within_last_hours),
                query.language.as_deref(),
                MAX_TREND_LOOKUPS,
            )?;
            let mut tags = Self::trending_tags(&posts);
            tags.truncate(query.max_count);
            Ok(tags)
        })
        .await
        .map_err(|e| format!("the trends task failed: {e}"))?
    }

    async fn follow_recommendations(
        &self,
        registration_id: i64,
        account_uri: &str,
        language: Option<&str>,
    ) -> Result<Vec<String>, String> {
        let app = self.clone();
        let account_uri = account_uri.to_owned();
        let language = language.map(str::to_owned);
        tokio::task::spawn_blocking(move || app.recommend(&account_uri, language.as_deref()))
            .await
            .map_err(|e| format!("the follow recommendation task failed: {e}"))?
            .map_err(|e| {
                format!("follow recommendation failed for registration {registration_id}: {e}")
            })
    }

    async fn account_search(
        &self,
        registration_id: i64,
        term: &str,
        limit: usize,
        offset: usize,
        language: Option<&str>,
    ) -> Result<AccountSearchPage, String> {
        let app = self.clone();
        let term = term.to_owned();
        let language = language.map(str::to_owned);
        let searched = tokio::task::spawn_blocking(move || {
            let tombstones = app
                .inner
                .tombstones
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            search_accounts(&app.inner.accounts, &term, &tombstones, language.as_deref())
        })
        .await
        .map_err(|e| format!("the account search task failed: {e}"))?
        .map_err(|e| format!("account search failed for registration {registration_id}: {e}"))?;
        let mut uris: Vec<String> = searched
            .into_iter()
            .skip(offset)
            .take(limit.saturating_add(1))
            .map(|account| account.uri)
            .collect();
        let has_more = uris.len() > limit;
        uris.truncate(limit);
        Ok(AccountSearchPage { uris, has_more })
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
        if event == Some(EventType::Trending) {
            let app = self.clone();
            let queued =
                tokio::task::spawn_blocking(move || app.record_trending(&object_uris)).await;
            match queued {
                Ok(taken) => tracing::debug!(
                    registration = registration_id,
                    count,
                    taken,
                    "trending announcement recorded"
                ),
                Err(e) => tracing::error!(registration = registration_id, error = %e,
                    "the trending recording task failed"),
            }
            return;
        }
        let delay = if event == Some(EventType::Update) {
            UPDATE_FETCH_DELAY_SECS
        } else {
            0
        };
        let app = self.clone();
        let queued =
            tokio::task::spawn_blocking(move || app.enqueue_all(&object_uris, kind, reason, delay))
                .await;
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
        assert_eq!(bare.sign_in_url, None);

        config.privacy_policy_url = Some("https://social.example/privacy".into());
        config.contact_email = Some("admin@social.example".into());
        config.fediverse_account = Some("@admin@social.example".into());
        config.sign_in_url = Some("https://mwmbl.example/social/sign-in".into());
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
            full.sign_in_url.as_deref(),
            Some("https://mwmbl.example/social/sign-in")
        );
        assert_eq!(
            full.fediverse_account.as_deref(),
            Some("@admin@social.example")
        );
    }
}
