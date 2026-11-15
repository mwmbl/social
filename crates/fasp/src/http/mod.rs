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

    /// Capabilities to advertise besides `data_sharing` 0.1, such as `account_search`.
    fn extra_capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }

    /// The current time, Unix seconds.
    fn now(&self) -> i64 {
        crate::system_now()
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
