//! The endpoints, after authentication.

use axum::http::StatusCode;
use axum::http::request::Parts;
use serde::Serialize;
use store::Registration;

use super::respond::Reply;
use super::{FaspState, capabilities};
use crate::announcement::{Announcement, AnnouncementSource, parse_announcement};
use crate::{
    ACCOUNT_SEARCH_ID, ACCOUNT_SEARCH_VERSION, Capability, DATA_SHARING_ID, DATA_SHARING_VERSION,
    PrivacyPolicy,
};

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
        ("GET", ["account_search", "v0", "search"]) => {
            account_search(state, registration, parts.uri.query().unwrap_or("")).await
        }
        (
            _,
            ["provider_info"]
            | ["capabilities", _, _, "activation"]
            | ["data_sharing", "v0", "announcements"]
            | ["account_search", "v0", "search"],
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

/// The default and the largest `limit` of an account search.
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;

/// The validated query of an account search.
struct SearchParams {
    term: String,
    limit: usize,
    offset: usize,
    language: Option<String>,
}

/// A number made only of digits; one too large for `usize` saturates.
fn parse_count(value: &str) -> Option<usize> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(value.parse().unwrap_or(usize::MAX))
}

fn parse_search_params(query: &str) -> Result<SearchParams, &'static str> {
    let (mut term, mut limit, mut offset, mut language) = (None, None, None, None);
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let slot = match key.as_ref() {
            "term" => &mut term,
            "limit" => &mut limit,
            "offset" => &mut offset,
            "language" => &mut language,
            _ => continue,
        };
        // The first occurrence of a parameter wins.
        slot.get_or_insert(value.into_owned());
    }
    let term = term
        .filter(|t| !t.trim().is_empty())
        .ok_or("term is required")?;
    let limit = match limit {
        None => DEFAULT_LIMIT,
        Some(v) => match parse_count(&v) {
            Some(n) if n > 0 => n.min(MAX_LIMIT),
            _ => return Err("limit must be a positive integer"),
        },
    };
    let offset = match offset {
        None => 0,
        Some(v) => parse_count(&v).ok_or("offset must be a non-negative integer")?,
    };
    let language = language
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty());
    Ok(SearchParams {
        term,
        limit,
        offset,
        language,
    })
}

/// The URL of the page after this one, for the `Link: rel="next"` header.
fn next_page_url(base_url: &str, params: &SearchParams) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("term", &params.term);
    query.append_pair("limit", &params.limit.to_string());
    query.append_pair(
        "offset",
        &params.offset.saturating_add(params.limit).to_string(),
    );
    if let Some(language) = &params.language {
        query.append_pair("language", language);
    }
    format!(
        "{}/account_search/v0/search?{}",
        base_url.trim_end_matches('/'),
        query.finish()
    )
}

/// `GET /account_search/v0/search`. 404 unless the registration enabled the capability, 422
/// for a bad query, else a JSON array of actor URIs, with a `Link` header when more follow.
async fn account_search<S: FaspState>(
    state: &S,
    registration: &Registration,
    query: &str,
) -> Reply {
    let enabled = state.with_store(|s| {
        s.capability_enabled(registration.id, ACCOUNT_SEARCH_ID, ACCOUNT_SEARCH_VERSION)
    });
    match enabled {
        Ok(true) => {}
        Ok(false) => {
            tracing::info!(
                registration = registration.id,
                host = registration.base_url,
                "refused account_search: capability is not enabled"
            );
            return Reply::error(StatusCode::NOT_FOUND, "capability is not enabled");
        }
        Err(e) => return store_failure(registration, &e),
    }
    let params = match parse_search_params(query) {
        Ok(params) => params,
        Err(message) => return Reply::error(StatusCode::UNPROCESSABLE_ENTITY, message),
    };
    let page = state
        .account_search(
            registration.id,
            &params.term,
            params.limit,
            params.offset,
            params.language.as_deref(),
        )
        .await;
    match page {
        Ok(page) => {
            let reply = Reply::json(StatusCode::OK, &page.uris);
            if page.has_more {
                let next = next_page_url(state.base_url(), &params);
                reply.with_header("link", format!("<{next}>; rel=\"next\""))
            } else {
                reply
            }
        }
        Err(e) => {
            tracing::error!(registration = registration.id, host = registration.base_url,
                error = %e, "account_search failed");
            Reply::error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}
