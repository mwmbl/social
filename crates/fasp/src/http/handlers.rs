//! The endpoints, after authentication.

use axum::http::StatusCode;
use axum::http::request::Parts;
use serde::Serialize;
use store::Registration;

use super::respond::Reply;
use super::{FaspState, capabilities};
use crate::announcement::{Announcement, AnnouncementSource, parse_announcement};
use crate::{Capability, DATA_SHARING_ID, DATA_SHARING_VERSION, PrivacyPolicy};

pub(crate) async fn route<S: FaspState>(
    state: &S,
    registration: &Registration,
    parts: &Parts,
    body: &[u8],
) -> Reply {
    let path = parts.uri.path().trim_end_matches('/');
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (parts.method.as_str(), segments.as_slice()) {
        ("GET", ["provider_info"]) => provider_info(state),
        ("POST" | "DELETE", ["capabilities", id, version, "activation"]) => {
            let enabled = parts.method == "POST";
            activation(state, registration, id, version, enabled).await
        }
        ("POST", ["data_sharing", "v0", "announcements"]) => {
            announcements(state, registration, body).await
        }
        (
            _,
            ["provider_info"]
            | ["capabilities", _, _, "activation"]
            | ["data_sharing", "v0", "announcements"],
        ) => Reply::error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
        _ => Reply::error(StatusCode::NOT_FOUND, "not found"),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderInfo<'a> {
    name: &'a str,
    privacy_policy: &'a [PrivacyPolicy],
    capabilities: Vec<Capability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    contact_email: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fediverse_account: Option<&'a str>,
}

fn provider_info<S: FaspState>(state: &S) -> Reply {
    let config = state.provider_info();
    Reply::json(
        StatusCode::OK,
        &ProviderInfo {
            name: &config.name,
            privacy_policy: &config.privacy_policy,
            capabilities: capabilities(state),
            contact_email: config.contact_email.as_deref(),
            fediverse_account: config.fediverse_account.as_deref(),
        },
    )
}

/// `POST|DELETE /capabilities/{id}/{version}/activation`. The version may be the full
/// advertised one (`0.1`) or just its major part (`0`, which is what Mastodon sends); either
/// way the store records the advertised version.
async fn activation<S: FaspState>(
    state: &S,
    registration: &Registration,
    id: &str,
    version: &str,
    enabled: bool,
) -> Reply {
    let Some(capability) = capabilities(state)
        .into_iter()
        .find(|c| c.id == id && c.matches_version(version))
    else {
        tracing::info!(
            registration = registration.id,
            host = registration.base_url,
            capability = id,
            version,
            "activation of an unknown capability refused"
        );
        return Reply::error(StatusCode::NOT_FOUND, "unknown capability");
    };
    let now = state.now();
    let saved = state.with_store(|s| {
        s.set_capability(
            registration.id,
            &capability.id,
            &capability.version,
            enabled,
            now,
        )
    });
    if let Err(e) = saved {
        tracing::error!(registration = registration.id, error = %e, "failed to record capability");
        return Reply::error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
    }
    tracing::info!(registration = registration.id, host = registration.base_url,
        capability = %capability.id, enabled, "capability changed");
    state
        .on_capability_changed(registration.id, &capability.id, enabled)
        .await;
    Reply::no_content()
}

/// Whether this announcement's source is one of the registration's own subscriptions or
/// backfills.
fn is_own_source<S: FaspState>(
    state: &S,
    registration_id: i64,
    announcement: &Announcement,
) -> Result<bool, store::StoreError> {
    state.with_store(|s| match &announcement.source {
        AnnouncementSource::Subscription { remote_id, .. } => Ok(s
            .list_subscriptions(registration_id)?
            .iter()
            .any(|sub| &sub.remote_id == remote_id)),
        AnnouncementSource::Backfill { remote_id, .. } => Ok(s
            .list_backfills(registration_id)?
            .iter()
            .any(|b| &b.remote_id == remote_id)),
    })
}

/// `POST /data_sharing/v0/announcements`. Malformed bodies get 422. Everything else gets 204,
/// including announcements we drop (`data_sharing` not enabled, an id that is not ours, trending
/// events), since the server cannot do anything useful with an error.
async fn announcements<S: FaspState>(state: &S, registration: &Registration, body: &[u8]) -> Reply {
    let announcement = match parse_announcement(body) {
        Ok(Some(announcement)) => announcement,
        Ok(None) => return Reply::no_content(),
        Err(e) => {
            tracing::info!(registration = registration.id, host = registration.base_url,
                error = %e, "malformed announcement");
            return Reply::error(StatusCode::UNPROCESSABLE_ENTITY, &e.to_string());
        }
    };
    let enabled = state.with_store(|s| {
        s.capability_enabled(registration.id, DATA_SHARING_ID, DATA_SHARING_VERSION)
    });
    match enabled {
        Ok(true) => {}
        Ok(false) => {
            tracing::info!(
                registration = registration.id,
                host = registration.base_url,
                "dropped announcement: data_sharing is not enabled"
            );
            return Reply::no_content();
        }
        Err(e) => return store_failure(registration, &e),
    }
    match is_own_source(state, registration.id, &announcement) {
        Ok(true) => {}
        Ok(false) => {
            tracing::info!(
                registration = registration.id,
                host = registration.base_url,
                "dropped announcement: source is not one of this registration's"
            );
            return Reply::no_content();
        }
        Err(e) => return store_failure(registration, &e),
    }
    state.on_announcement(registration.id, announcement).await;
    Reply::no_content()
}

fn store_failure(registration: &Registration, error: &store::StoreError) -> Reply {
    tracing::error!(registration = registration.id, host = registration.base_url,
        error = %error, "store failure while handling a FASP request");
    Reply::error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}
