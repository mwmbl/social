//! Our inboxes, `POST /actor/inbox` and the shared `POST /inbox`, and what the activities they
//! receive do (PLAN.md Stage 3).
//!
//! A delivery is checked before it is answered: the body must be at most [`MAX_BODY_BYTES`] of
//! `application/activity+json` or `ld+json` (else 413 or 415), and a JSON object with an `actor`
//! and a `type` (else 400). A sender on a blocked domain, or one an admin forgot, gets 202 and
//! is ignored, with nothing fetched. Then the HTTP signature (RFC 9421 or cavage-12) must verify
//! with the key its `keyId` names, which must belong to the activity's `actor` on the same
//! origin (else 401). Keys are fetched with a signed `GET` and cached for [`KEY_TTL`]; a cached
//! key that fails is fetched once more, for key rotation. Verified deliveries get 202 at once
//! and are handled on a spawned task.
//!
//! The JSON is never stored or indexed (CLAUDE.md principle 4), and never logged: activities
//! only yield the [`Effect`]s of [`crate::activities`], and objects are fetched from their origin
//! through the fetch queue like everything else.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use apub::actor::server_actor_url;
use apub::fetch::{FetchOutcome, Fetcher};
use apub::inbox::{
    PublicKeyInfo, Signer, check_key_owner, find_public_key, same_origin, signer, verify_post,
};
use apub::signatures::keys::RsaVerifyingKey;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use serde_json::{Value, json};
use social_index::Tombstones;
use social_index::tombstones::host_of;
use store::{FetchKind, FetchReason, FollowState, NewFetch, Store};
use tokio::sync::watch;

use crate::activities::{
    Effect, Sender, account_deleted, activity_actor, handle, self_delete_with_gone_key,
};
use crate::decide::Action;
use crate::delivery::Deliveries;
use crate::follows::{
    ActorInfo, FollowTarget, activity_id, actor_info, fetch_followable, follow_activity, resolve,
    undo_follow_activity,
};
use crate::outbox;
use crate::writer::WriterHandle;
use crate::{now_millis, now_secs};

/// Larger deliveries get 413.
pub const MAX_BODY_BYTES: usize = 256 * 1024;
/// How long a fetched key, or what we learnt about an actor, is trusted.
pub const KEY_TTL: Duration = Duration::from_secs(3600);
/// Keys and actors held in memory; past this the oldest half is dropped.
const CACHE_CAPACITY: usize = 10_000;

const AS_CONTEXT: &str = "https://www.w3.org/ns/activitystreams";
/// How often configured follows are checked, and stale pending follows re-sent.
pub const FOLLOW_MAINTENANCE_EVERY: Duration = Duration::from_secs(6 * 3600);
/// A follow still pending after this long is sent again (once per this long).
pub const RESEND_PENDING_AFTER_SECS: i64 = 86_400;

/// The follows an admin configured, and how outbox backfill runs.
#[derive(Debug, Clone, Default)]
pub struct FollowSettings {
    /// The `follow` config option, parsed.
    pub targets: Vec<FollowTarget>,
    /// `WebFinger` over `http://` (tests only).
    pub allow_http: bool,
    /// Outbox pages read when a follow is accepted (0: none).
    pub outbox_backfill_pages: usize,
}

/// A fetched key and what its document said about it. The owner is checked against the
/// activity's actor on every request, cached or not (audit A1).
#[derive(Clone)]
struct CachedKey {
    info: PublicKeyInfo,
    key: RsaVerifyingKey,
    listed: Listed,
}

/// Whether the key's owner lists it with the same PEM (audit A2). A key found in its owner's
/// own document is listed by definition; one in a document of its own needs the owner fetched.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Listed {
    Yes,
    No,
    /// Not looked at, because the key failed the owner check when it was fetched.
    Unchecked,
}

/// A small in-memory map whose entries expire.
struct Cache<T> {
    entries: HashMap<String, (Instant, T)>,
}

impl<T: Clone> Cache<T> {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    fn get(&self, key: &str) -> Option<T> {
        self.entries
            .get(key)
            .filter(|(at, _)| at.elapsed() < KEY_TTL)
            .map(|(_, value)| value.clone())
    }

    fn insert(&mut self, key: &str, value: T) {
        if self.entries.len() >= CACHE_CAPACITY {
            self.entries.retain(|_, (at, _)| at.elapsed() < KEY_TTL);
        }
        if self.entries.len() >= CACHE_CAPACITY {
            let mut ages: Vec<Instant> = self.entries.values().map(|(at, _)| *at).collect();
            ages.sort_unstable();
            if let Some(cutoff) = ages.get(ages.len() / 2).copied() {
                self.entries.retain(|_, (at, _)| *at > cutoff);
            }
        }
        self.entries.insert(key.to_owned(), (Instant::now(), value));
    }

    fn remove(&mut self, key: &str) {
        self.entries.remove(key);
    }
}

/// Why a delivery's signature was not accepted. Messages carry URIs, never content.
#[derive(Debug, thiserror::Error)]
enum VerifyError {
    #[error("no usable signature: {0}")]
    Signature(#[from] apub::signatures::SignatureError),
    #[error("key {key_id} is not on the origin of actor {actor}")]
    ForeignKey { key_id: String, actor: String },
    #[error("key {key_id} is gone")]
    KeyGone { key_id: String },
    #[error("key {key_id} could not be fetched: {reason}")]
    KeyUnavailable { key_id: String, reason: String },
    #[error("no key {key_id} in its document")]
    NoKey { key_id: String },
    #[error(transparent)]
    Owner(#[from] apub::inbox::OwnerError),
    #[error("key {key_id} is not a usable RSA key")]
    BadKey { key_id: String },
    #[error("owner {owner} of key {key_id} could not be fetched: {reason}")]
    OwnerUnavailable {
        key_id: String,
        owner: String,
        reason: String,
    },
    #[error("owner {owner} does not list key {key_id} with the same PEM")]
    NotListed { key_id: String, owner: String },
}

struct Inner {
    our_actor: String,
    base_url: String,
    fetcher: Arc<Fetcher>,
    store: Mutex<Store>,
    writer: WriterHandle,
    tombstones: Arc<RwLock<Tombstones>>,
    deliveries: Deliveries,
    queue_cap: usize,
    follows: FollowSettings,
    keys: Mutex<Cache<CachedKey>>,
    actors: Mutex<Cache<ActorInfo>>,
    sequence: AtomicU64,
}

/// The inbox: cheap to clone, shared by the routes and the tasks they spawn.
#[derive(Clone)]
pub struct InboxApp {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for InboxApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboxApp").finish_non_exhaustive()
    }
}

/// Whether two PEM encodings are the same, ignoring line breaks and other whitespace.
fn same_pem(a: &str, b: &str) -> bool {
    a.split_whitespace().eq(b.split_whitespace())
}

fn not_listed(info: &PublicKeyInfo) -> VerifyError {
    VerifyError::NotListed {
        key_id: info.id.clone(),
        owner: info.owner.clone(),
    }
}

fn status(code: StatusCode) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = code;
    response
}

fn is_activity_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(|media| media.trim().to_ascii_lowercase())
        .is_some_and(|media| media == "application/activity+json" || media == "application/ld+json")
}

impl InboxApp {
    #[must_use]
    pub fn new(
        base_url: &str,
        fetcher: Arc<Fetcher>,
        store: Store,
        writer: WriterHandle,
        deliveries: Deliveries,
        queue_cap: usize,
        follows: FollowSettings,
    ) -> Self {
        let tombstones = writer.tombstones();
        Self {
            inner: Arc::new(Inner {
                our_actor: server_actor_url(base_url),
                base_url: base_url.trim_end_matches('/').to_owned(),
                fetcher,
                store: Mutex::new(store),
                writer,
                tombstones,
                deliveries,
                queue_cap,
                follows,
                keys: Mutex::new(Cache::new()),
                actors: Mutex::new(Cache::new()),
                sequence: AtomicU64::new(0),
            }),
        }
    }

    /// `POST /actor/inbox` and `POST /inbox`. Takes the app: it holds a writer handle, and the
    /// writer stops only once every handle is gone.
    pub fn routes(self) -> Router {
        Router::new()
            .route("/actor/inbox", post(receive))
            .route("/inbox", post(receive))
            .with_state(self)
    }

    fn store<R>(&self, f: impl FnOnce(&Store) -> R) -> R {
        let store = self
            .inner
            .store
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        f(&store)
    }

    /// Whether everything from `actor` is ignored: its domain is blocked or an admin forgot it.
    fn ignored(&self, actor: &str) -> bool {
        let blocked = host_of(actor).is_none_or(|host| {
            self.inner
                .tombstones
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .domain_blocked(&host)
        });
        blocked
            || self.store(|s| s.is_forgotten(actor)).unwrap_or_else(|e| {
                tracing::error!(actor, error = %e, "failed to read forgotten URIs");
                true
            })
    }

    /// Fetches the key `key_id`, caches it with its owner and checks it belongs to `actor`.
    /// Remembers the actor's type and inbox when the key's document is the actor.
    async fn fetch_key(&self, key_id: &str, actor: &str) -> Result<RsaVerifyingKey, VerifyError> {
        let json = match self.inner.fetcher.fetch(key_id).await {
            FetchOutcome::Found { json, .. } => json,
            FetchOutcome::Gone => {
                return Err(VerifyError::KeyGone {
                    key_id: key_id.to_owned(),
                });
            }
            other => {
                return Err(VerifyError::KeyUnavailable {
                    key_id: key_id.to_owned(),
                    reason: format!("{other:?}"),
                });
            }
        };
        let info: PublicKeyInfo =
            find_public_key(&json, key_id).ok_or_else(|| VerifyError::NoKey {
                key_id: key_id.to_owned(),
            })?;
        let key = RsaVerifyingKey::from_pem(&info.pem).map_err(|_| VerifyError::BadKey {
            key_id: key_id.to_owned(),
        })?;
        let owned = check_key_owner(&info, actor);
        let in_owner = json.get("id").and_then(Value::as_str) == Some(info.owner.as_str());
        // A key document of its own only names its owner: the owner must list it too (A2).
        let listed = if in_owner {
            Listed::Yes
        } else if owned.is_ok() {
            self.owner_lists(&info).await?
        } else {
            Listed::Unchecked
        };
        // Cache the key whoever owns it, so a forger naming someone else's key can't make us
        // fetch it again on every request.
        self.inner
            .keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                key_id,
                CachedKey {
                    info: info.clone(),
                    key: key.clone(),
                    listed,
                },
            );
        owned?;
        if listed == Listed::No {
            return Err(not_listed(&info));
        }
        if in_owner {
            self.inner
                .actors
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(actor, actor_info(&json));
        }
        Ok(key)
    }

    /// Fetches the owner of a key served from a document of its own, and finds whether its
    /// `publicKey` (an object or an array) lists that key id with the same PEM.
    async fn owner_lists(&self, info: &PublicKeyInfo) -> Result<Listed, VerifyError> {
        let owner = info.owner.as_str();
        let json = match self.inner.fetcher.fetch(owner).await {
            FetchOutcome::Found { json, .. } => json,
            other => {
                return Err(VerifyError::OwnerUnavailable {
                    key_id: info.id.clone(),
                    owner: owner.to_owned(),
                    reason: format!("{other:?}"),
                });
            }
        };
        if json.get("id").and_then(Value::as_str) != Some(owner) {
            tracing::debug!(
                key_id = info.id,
                owner,
                "a key's owner document has another id"
            );
            return Ok(Listed::No);
        }
        let listed = find_public_key(&json, &info.id)
            .is_some_and(|k| k.owner == owner && same_pem(&k.pem, &info.pem));
        if !listed {
            return Ok(Listed::No);
        }
        self.inner
            .actors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(owner, actor_info(&json));
        Ok(Listed::Yes)
    }

    /// Verifies a delivery from `actor`. Returns the signer.
    async fn verify(
        &self,
        headers: &[(String, String)],
        target_uri: &str,
        body: &[u8],
        actor: &str,
    ) -> Result<Signer, (Option<String>, VerifyError)> {
        let by = signer(headers).map_err(|e| (None, e.into()))?;
        let key_id = by.key_id.clone();
        let fail = |e: VerifyError| (Some(key_id.clone()), e);
        // A key elsewhere could not belong to the actor: don't fetch it.
        if !same_origin(&key_id, actor) {
            return Err(fail(VerifyError::ForeignKey {
                key_id: key_id.clone(),
                actor: actor.to_owned(),
            }));
        }
        let now = now_secs();
        let cached = self
            .inner
            .keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key_id);
        if let Some(CachedKey { info, key, listed }) = cached {
            // A key's owner doesn't change with rotation, nor whether the owner listed it when
            // last fetched: refuse at once, without a refetch.
            check_key_owner(&info, actor).map_err(|e| fail(e.into()))?;
            if listed == Listed::No {
                return Err(fail(not_listed(&info)));
            }
            if listed == Listed::Yes
                && verify_post(&by, target_uri, headers, body, &key, now).is_ok()
            {
                return Ok(by);
            }
            // Perhaps the key was rotated: fetch it once more.
            self.inner
                .keys
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&key_id);
        }
        let key = self.fetch_key(&key_id, actor).await.map_err(fail)?;
        verify_post(&by, target_uri, headers, body, &key, now).map_err(|e| fail(e.into()))?;
        Ok(by)
    }

    /// What we know about `actor`, from the caches or a fetch.
    async fn actor_info(&self, actor: &str) -> Option<ActorInfo> {
        let cached = self
            .inner
            .actors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(actor);
        if cached.is_some() {
            return cached;
        }
        match self.inner.fetcher.fetch(actor).await {
            FetchOutcome::Found { json, .. }
                if json.get("id").and_then(Value::as_str) == Some(actor) =>
            {
                let info = actor_info(&json);
                self.inner
                    .actors
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(actor, info.clone());
                Some(info)
            }
            other => {
                tracing::debug!(actor, outcome = ?other, "could not fetch an inbox sender");
                None
            }
        }
    }

    /// A fresh id under our actor for an activity we send.
    fn new_id(&self, kind: &str) -> String {
        let n = self.inner.sequence.fetch_add(1, Ordering::Relaxed);
        activity_id(&self.inner.our_actor, kind, now_millis(), n)
    }

    /// Handles a verified activity: works out its effects and applies them in order.
    pub async fn process(&self, actor: &str, activity: &Value) {
        let (our_follow, follows_us) = self.store(|s| {
            let follow = s.following(actor).map(|f| f.map(|f| f.follow_id));
            let optin = s.is_follow_optin(actor);
            (follow, optin)
        });
        let (our_follow, follows_us) = match (our_follow, follows_us) {
            (Ok(follow), Ok(optin)) => (follow, optin),
            (Err(e), _) | (_, Err(e)) => {
                tracing::error!(actor, error = %e, "failed to read follow state; ignoring");
                return;
            }
        };
        // Only the rules for followed actors care whether the sender is a community.
        let is_group = if our_follow.is_some() {
            self.actor_info(actor).await.is_some_and(|i| i.is_group)
        } else {
            false
        };
        let sender = Sender {
            actor,
            is_group,
            our_follow: our_follow.as_deref(),
            follows_us,
        };
        let effects = handle(&self.inner.our_actor, &sender, activity);
        let id = activity.get("id").and_then(Value::as_str);
        tracing::debug!(actor, id, effects = effects.len(), "inbox activity");
        self.apply(effects).await;
    }

    async fn apply(&self, effects: Vec<Effect>) {
        for effect in effects {
            self.apply_one(effect).await;
        }
    }

    async fn write(&self, actions: Vec<Action>) {
        if let Err(e) = self.inner.writer.apply(actions).await {
            tracing::error!(error = %e, "failed to apply an inbox removal");
        }
    }

    fn log_store<T>(result: Result<T, store::StoreError>) {
        if let Err(e) = result {
            tracing::error!(error = %e, "failed to record an inbox activity");
        }
    }

    async fn apply_one(&self, effect: Effect) {
        match effect {
            Effect::RecordFollowOptin { actor_uri } => {
                tracing::info!(actor = actor_uri, "follow opt-in");
                Self::log_store(self.store(|s| s.add_follow_optin(&actor_uri, now_secs())));
            }
            Effect::RemoveFollowOptin { actor_uri } => {
                if self
                    .store(|s| s.remove_follow_optin(&actor_uri))
                    .unwrap_or_else(|e| {
                        tracing::error!(actor = actor_uri, error = %e, "failed to remove opt-in");
                        false
                    })
                {
                    tracing::info!(actor = actor_uri, "follow opt-in withdrawn");
                }
            }
            Effect::AcceptFollow {
                actor_uri,
                follow_id,
            } => self.accept_follow(&actor_uri, follow_id).await,
            Effect::FollowBack { actor_uri } => self.follow_back(&actor_uri).await,
            Effect::Unfollow { actor_uri } => self.unfollow(&actor_uri),
            Effect::FollowAccepted { actor_uri } => {
                match self.store(|s| s.accept_following(&actor_uri)) {
                    Ok(true) => {
                        tracing::info!(actor = actor_uri, "our follow was accepted");
                        // Off the inbox's path: an outbox can take a while to read.
                        let app = self.clone();
                        tokio::spawn(async move { app.backfill_outbox(&actor_uri).await });
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tracing::error!(actor = actor_uri, error = %e, "failed to accept a follow");
                    }
                }
            }
            Effect::DropFollowing { actor_uri } => {
                Self::log_store(self.store(|s| s.remove_following(&actor_uri)));
            }
            Effect::Fetch {
                uri,
                kind,
                delay_secs,
            } => {
                let now = now_secs();
                let item = NewFetch {
                    uri,
                    kind,
                    reason: FetchReason::Announce,
                    record: None,
                    not_before: now.saturating_add(delay_secs),
                };
                Self::log_store(self.store(|s| s.enqueue(&item, now, self.inner.queue_cap)));
            }
            Effect::AddAuthorTombstone { actor_uri } => {
                self.write(vec![Action::AddAuthorTombstone { actor_uri }])
                    .await;
            }
            Effect::RemoveObject { uri } => {
                self.write(vec![Action::RemovePost {
                    uri,
                    record: None,
                    tombstone: true,
                }])
                .await;
            }
            Effect::RemoveAccount { uri } => {
                self.write(vec![Action::RemoveAccount {
                    uri,
                    record: None,
                    tombstone: true,
                }])
                .await;
            }
        }
    }

    /// Delivers an `Accept` of `actor`'s follow of us.
    async fn accept_follow(&self, actor: &str, follow_id: Option<String>) {
        let Some(inbox) = self.inbox_of(actor).await else {
            return;
        };
        let our_actor = &self.inner.our_actor;
        let follow = match follow_id {
            Some(id) => json!({"id": id, "type": "Follow", "actor": actor, "object": our_actor}),
            None => json!({"type": "Follow", "actor": actor, "object": our_actor}),
        };
        let accept = json!({
            "@context": AS_CONTEXT,
            "id": self.new_id("accepts"),
            "type": "Accept",
            "actor": our_actor,
            "object": follow,
        });
        self.inner.deliveries.send(&inbox, &accept);
    }

    /// Follows `actor`, unless we do (or asked to) already.
    async fn follow_back(&self, actor: &str) {
        match self.store(|s| s.following(actor)) {
            Ok(None) => {}
            Ok(Some(_)) => return,
            Err(e) => {
                tracing::error!(actor, error = %e, "failed to read following");
                return;
            }
        }
        let Some(inbox) = self.inbox_of(actor).await else {
            return;
        };
        if self.record_and_send_follow(actor, &inbox) {
            tracing::info!(actor, "following back");
        }
    }

    /// Records a pending follow of `actor` and delivers our `Follow`. Returns whether it was
    /// new.
    fn record_and_send_follow(&self, actor: &str, inbox: &str) -> bool {
        let id = self.new_id("follows");
        match self.store(|s| s.add_following(actor, inbox, &id, now_secs())) {
            Ok(true) => {
                let follow = follow_activity(&self.inner.our_actor, &id, actor);
                self.inner.deliveries.send(inbox, &follow);
                true
            }
            Ok(false) => false,
            Err(e) => {
                tracing::error!(actor, error = %e, "failed to record following");
                false
            }
        }
    }

    /// Undoes our follow of `actor`, if any, and forgets it.
    fn unfollow(&self, actor: &str) {
        let row = match self.store(|s| s.following(actor)) {
            Ok(Some(row)) => row,
            Ok(None) => return,
            Err(e) => {
                tracing::error!(actor, error = %e, "failed to read following");
                return;
            }
        };
        let undo = undo_follow_activity(&self.inner.our_actor, &row.follow_id, actor);
        tracing::info!(actor, "unfollowing");
        self.inner.deliveries.send(&row.inbox, &undo);
        Self::log_store(self.store(|s| s.remove_following(actor)));
    }

    /// Reads `actor`'s outbox and queues its posts for backfill (see [`crate::outbox`]).
    pub async fn backfill_outbox(&self, actor: &str) {
        let pages = self.inner.follows.outbox_backfill_pages;
        if pages == 0 {
            return;
        }
        let Some(info) = self.actor_info(actor).await else {
            return;
        };
        let Some(outbox_uri) = info.outbox else {
            tracing::info!(actor, "no outbox on the actor's origin; no backfill");
            return;
        };
        let cap = self.inner.queue_cap;
        let report = outbox::backfill(
            &self.inner.fetcher,
            actor,
            info.is_group,
            &outbox_uri,
            pages,
            |uri| {
                let item = NewFetch {
                    uri: uri.to_owned(),
                    kind: FetchKind::Object,
                    reason: FetchReason::Backfill,
                    record: None,
                    not_before: now_secs(),
                };
                self.store(|s| s.enqueue(&item, now_secs(), cap))
            },
        )
        .await;
        match &report.stopped {
            None => tracing::info!(
                actor,
                pages = report.pages,
                queued = report.queued,
                "outbox backfill done"
            ),
            Some(reason) => tracing::info!(
                actor,
                pages = report.pages,
                queued = report.queued,
                reason,
                "outbox backfill stopped"
            ),
        }
    }

    /// Follows each configured target we don't follow yet, and re-sends follows pending for
    /// more than [`RESEND_PENDING_AFTER_SECS`]. `resolved` remembers what handles resolved to,
    /// so `WebFinger` is asked once per process. On the first run (`hint`), followed actors
    /// that are neither configured nor follow opt-ins are logged, since dropping a target from
    /// the config doesn't unfollow it.
    pub async fn maintain_follows(&self, resolved: &mut HashMap<String, String>, hint: bool) {
        let settings = &self.inner.follows;
        let mut configured = HashSet::new();
        for target in &settings.targets {
            let key = target.to_string();
            let uri = if let Some(uri) = resolved.get(&key) {
                uri.clone()
            } else {
                match resolve(&self.inner.fetcher, settings.allow_http, target).await {
                    Ok(uri) => {
                        resolved.insert(key, uri.clone());
                        uri
                    }
                    Err(e) => {
                        tracing::warn!(target = %target, error = %e, "cannot follow a configured target");
                        continue;
                    }
                }
            };
            configured.insert(uri.clone());
            self.follow_configured(&uri).await;
        }
        let rows = match self.store(Store::list_following) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::error!(error = %e, "failed to list following");
                return;
            }
        };
        let now = now_secs();
        for row in &rows {
            if row.state == FollowState::Pending
                && row.created_at <= now.saturating_sub(RESEND_PENDING_AFTER_SECS)
            {
                tracing::info!(actor = row.actor_uri, "re-sending a pending follow");
                // A new id: Lemmy rejects one it has seen.
                let id = self.new_id("follows");
                let follow = follow_activity(&self.inner.our_actor, &id, &row.actor_uri);
                self.inner.deliveries.send(&row.inbox, &follow);
                Self::log_store(self.store(|s| s.following_resent(&row.actor_uri, &id, now)));
            }
            if hint && !settings.targets.is_empty() && !configured.contains(&row.actor_uri) {
                let optin = self.store(|s| s.is_follow_optin(&row.actor_uri));
                if matches!(optin, Ok(false)) {
                    tracing::info!(
                        actor = row.actor_uri,
                        "followed but not in the `follow` config; `mwmbl-social unfollow` stops it"
                    );
                }
            }
        }
    }

    /// Follows the configured actor `uri`, unless we do already.
    async fn follow_configured(&self, uri: &str) {
        match self.store(|s| s.following(uri)) {
            Ok(None) => {}
            Ok(Some(_)) => return,
            Err(e) => {
                tracing::error!(actor = uri, error = %e, "failed to read following");
                return;
            }
        }
        if self.ignored(uri) {
            tracing::warn!(
                actor = uri,
                "a configured follow is blocked or forgotten; skipping"
            );
            return;
        }
        let actor = match fetch_followable(&self.inner.fetcher, uri).await {
            Ok(actor) => actor,
            Err(e) => {
                tracing::warn!(actor = uri, error = %e, "cannot follow a configured target");
                return;
            }
        };
        self.inner
            .actors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(&actor.uri, actor.info.clone());
        if self.record_and_send_follow(&actor.uri, &actor.inbox) {
            tracing::info!(
                actor = actor.uri,
                group = actor.info.is_group,
                "following (configured)"
            );
        }
    }

    /// Runs [`InboxApp::maintain_follows`] now and every `every` until `shutdown`.
    pub async fn run_follow_maintenance(
        self,
        every: Duration,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut resolved = HashMap::new();
        let mut interval = tokio::time::interval(every);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut first = true;
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = shutdown.changed() => {}
            }
            if *shutdown.borrow() {
                break;
            }
            self.maintain_follows(&mut resolved, first).await;
            first = false;
        }
    }

    /// The inbox to deliver to `actor` at, if it is on the actor's origin.
    async fn inbox_of(&self, actor: &str) -> Option<String> {
        let inbox = self.actor_info(actor).await.and_then(|i| i.inbox);
        if inbox.is_none() {
            tracing::warn!(actor, "no inbox on the actor's origin; not delivering");
        }
        inbox
    }
}

async fn receive(State(app): State<InboxApp>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    if !is_activity_json(&parts.headers) {
        return status(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    let Ok(body) = to_bytes(body, MAX_BODY_BYTES).await else {
        return status(StatusCode::PAYLOAD_TOO_LARGE);
    };
    let Ok(activity) = serde_json::from_slice::<Value>(&body) else {
        return status(StatusCode::BAD_REQUEST);
    };
    let Some(actor) = activity_actor(&activity).map(str::to_owned) else {
        return status(StatusCode::BAD_REQUEST);
    };
    if activity.get("type").is_none() {
        return status(StatusCode::BAD_REQUEST);
    }
    if app.ignored(&actor) {
        tracing::debug!(
            actor,
            "ignoring a delivery from a blocked or forgotten sender"
        );
        return status(StatusCode::ACCEPTED);
    }
    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
        .collect();
    let target = format!(
        "{}{}",
        app.inner.base_url,
        parts
            .uri
            .path_and_query()
            .map_or(parts.uri.path(), |pq| pq.as_str())
    );
    match app.verify(&headers, &target, &body, &actor).await {
        Ok(_) => {
            let app = app.clone();
            tokio::spawn(async move { app.process(&actor, &activity).await });
            status(StatusCode::ACCEPTED)
        }
        Err((Some(key_id), VerifyError::KeyGone { .. }))
            if self_delete_with_gone_key(&activity, &key_id) == Some(actor.as_str()) =>
        {
            // A deleted account can't sign with a key we can fetch, but its origin says it is
            // gone, which is all the removal needs.
            tracing::info!(actor, "account deleted (its key is gone)");
            let app = app.clone();
            tokio::spawn(async move { app.apply(account_deleted(&actor)).await });
            status(StatusCode::ACCEPTED)
        }
        Err((_, e)) => {
            tracing::debug!(actor, error = %e, "refusing an inbox delivery");
            status(StatusCode::UNAUTHORIZED)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_types() {
        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, value.parse().unwrap());
            is_activity_json(&headers)
        };
        assert!(with("application/activity+json"));
        assert!(with(
            "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\""
        ));
        assert!(with("Application/Activity+JSON; charset=utf-8"));
        assert!(!with("application/json"));
        assert!(!with("text/html"));
        assert!(!is_activity_json(&HeaderMap::new()));
    }

    #[test]
    fn cache_expires_and_stays_bounded() {
        let mut cache: Cache<u32> = Cache::new();
        cache.insert("a", 1);
        assert_eq!(cache.get("a"), Some(1));
        cache.remove("a");
        assert_eq!(cache.get("a"), None);
        for n in 0..(CACHE_CAPACITY + 10) {
            cache.insert(&n.to_string(), 0);
        }
        assert!(cache.entries.len() <= CACHE_CAPACITY);
    }
}
