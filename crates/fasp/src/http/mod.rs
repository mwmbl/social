//! The axum side: our FASP endpoints.
//!
//! [`router`] builds a `Router` serving paths relative to our FASP base URL (mount it at the
//! root, or `nest` it under the base URL's path). Every request, whatever its path, must be
//! authenticated (RFC 9421 signature by the registration's key, over `@method`, `@target-uri`
//! and `content-digest`), and every authenticated response is signed (`@status` and
//! `content-digest`) because Mastodon verifies all of them, 204s included. A request that
//! fails authentication gets an unsigned 401, since we cannot know whose key to sign with.
//!
//! `@target-uri` is rebuilt from [`FaspState::base_url`] plus the request path and query. The
//! `Host` header is never trusted, as we normally run behind a reverse proxy.

mod auth;
mod handlers;
mod respond;

use std::future::Future;
use std::sync::Arc;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;

use crate::announcement::Announcement;
use crate::{Capability, DATA_SHARING_ID, DATA_SHARING_VERSION, ProviderInfoConfig, StoreAccess};

/// Largest request body we read; more is a 413.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// How far a signature's `created` may be from our clock, either way.
pub const MAX_SKEW_SECS: i64 = 300;

/// What the binary provides to the FASP router.
pub trait FaspState: StoreAccess + 'static {
    /// Our FASP base URL as the servers know it (the `baseUrl` sent at registration).
    fn base_url(&self) -> &str;

    /// The configurable `provider_info` fields.
    fn provider_info(&self) -> &ProviderInfoConfig;

    /// Capabilities to advertise besides `data_sharing` 0.1, such as `account_search` (the state
    /// must then implement [`FaspState::account_search`]).
    fn extra_capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }

    /// The current time, Unix seconds.
    fn now(&self) -> i64 {
        crate::system_now()
    }

    /// One page of `account_search` results for a registration that has the capability
    /// enabled: actor URIs, best first, skipping `offset` and taking at most `limit` (already
    /// validated and capped). `has_more` says whether another page follows. The default
    /// implementation is an error, so state that does not search never serves the route.
    fn account_search(
        &self,
        _registration_id: i64,
        _term: &str,
        _limit: usize,
        _offset: usize,
        _language: Option<&str>,
    ) -> impl Future<Output = Result<AccountSearchPage, String>> + Send {
        async { Err("account_search is not implemented".to_owned()) }
    }

    /// Trending content for a registration that has `trends` enabled, best first, at most
    /// `query.max_count`. Ranks must be in 1..=100 and comparable across instances of the
    /// same software (the spec's rule for merging FASP results).
    fn trending_content(
        &self,
        _registration_id: i64,
        _query: &TrendsQuery,
    ) -> impl Future<Output = Result<Vec<TrendingContent>, String>> + Send {
        async { Err("trends is not implemented".to_owned()) }
    }

    /// Trending hashtags, as [`FaspState::trending_content`].
    fn trending_hashtags(
        &self,
        _registration_id: i64,
        _query: &TrendsQuery,
    ) -> impl Future<Output = Result<Vec<TrendingTag>, String>> + Send {
        async { Err("trends is not implemented".to_owned()) }
    }

    /// Trending links, as [`FaspState::trending_content`]. The default is none, which is the
    /// honest answer for a state that does not see links.
    fn trending_links(
        &self,
        _registration_id: i64,
        _query: &TrendsQuery,
    ) -> impl Future<Output = Result<Vec<TrendingLink>, String>> + Send {
        async { Ok(Vec::new()) }
    }

    /// Accounts to recommend to the account `account_uri` (actor URIs, best first), for a
    /// registration that has `follow_recommendation` enabled. The account URI identifies a
    /// person, so implementations must not log it.
    fn follow_recommendations(
        &self,
        _registration_id: i64,
        _account_uri: &str,
        _language: Option<&str>,
    ) -> impl Future<Output = Result<Vec<String>, String>> + Send {
        async { Err("follow_recommendation is not implemented".to_owned()) }
    }

    /// A capability was activated or deactivated by the server (already recorded in the
    /// store, under the id and the version we advertise). Called on every activation request,
    /// including repeats, so the handler must be idempotent. The binary starts or stops
    /// `data_sharing` work here (see [`crate::data_sharing`]).
    fn on_capability_changed(
        &self,
        registration_id: i64,
        capability: &str,
        enabled: bool,
    ) -> impl Future<Output = ()> + Send;

    /// An announcement from one of this registration's subscriptions or backfills, received
    /// while `data_sharing` is enabled. The binary enqueues its URIs. The 204 is sent after
    /// this returns, so it must not block.
    fn on_announcement(
        &self,
        registration_id: i64,
        announcement: Announcement,
    ) -> impl Future<Output = ()> + Send;
}

/// One page of account search results.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountSearchPage {
    /// Actor URIs, best first.
    pub uris: Vec<String>,
    /// Whether more results follow this page.
    pub has_more: bool,
}

/// The validated common parameters of a `trends` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrendsQuery {
    /// 1..=[`MAX_WITHIN_LAST_HOURS`]; larger requests are clamped (the spec only requires 168).
    pub within_last_hours: u32,
    /// 1..=100.
    pub max_count: usize,
    /// A BCP 47 range for RFC 4647 basic filtering.
    pub language: Option<String>,
}

/// The longest `withinLastHours` we serve: the spec's required week.
pub const MAX_WITHIN_LAST_HOURS: u32 = 168;

/// One entry of `GET /trends/v0/content`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TrendingContent {
    pub uri: String,
    pub rank: u8,
}

/// One entry of `GET /trends/v0/hashtags`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TrendingTag {
    pub name: String,
    pub rank: u8,
    pub examples: Vec<String>,
}

/// One entry of `GET /trends/v0/links`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TrendingLink {
    pub url: String,
    pub rank: u8,
    pub examples: Vec<String>,
}

/// Every capability we advertise: `data_sharing` plus the state's extras.
pub(crate) fn capabilities<S: FaspState>(state: &S) -> Vec<Capability> {
    let mut all = vec![Capability::new(DATA_SHARING_ID, DATA_SHARING_VERSION)];
    all.extend(state.extra_capabilities());
    all
}

/// Builds the router for our FASP endpoints.
pub fn router<S: FaspState>(state: Arc<S>) -> Router {
    Router::new().fallback(dispatch::<S>).with_state(state)
}

async fn dispatch<S: FaspState>(State(state): State<Arc<S>>, request: Request) -> Response {
    let now = state.now();
    let (parts, body) = request.into_parts();
    let too_large = parts
        .headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|len| len > MAX_BODY_BYTES);
    let body = if too_large {
        None
    } else {
        to_bytes(body, MAX_BODY_BYTES).await.ok()
    };
    let Some(body) = body else {
        tracing::info!(
            path = parts.uri.path(),
            "rejected FASP request body over the size cap"
        );
        return respond::unsigned(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
    };

    let registration = match auth::authenticate(&*state, &parts, &body, now) {
        Ok(registration) => registration,
        Err(failure) => {
            tracing::info!(path = parts.uri.path(), reason = %failure, "FASP request rejected");
            return respond::unsigned(StatusCode::UNAUTHORIZED, "authentication failed");
        }
    };
    let reply = handlers::route(&*state, &registration, &parts, &body).await;
    respond::signed(&registration, reply, now)
}
