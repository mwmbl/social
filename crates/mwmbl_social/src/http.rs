//! The HTTP server: our actor, `WebFinger` and `NodeInfo` (so other servers can verify our
//! signed fetches), the search API, and `/` when `root_redirect` is set.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use apub::actor::{nodeinfo, nodeinfo_well_known, server_actor, webfinger};
use axum::Router;
use axum::body::Body;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use serde::Serialize;
use serde_json::{Value, json};
use social_index::{SocialAccount, SocialPost, SocialReader, Tombstones};

use crate::now_secs;
use crate::search::{self, DEFAULT_LIMIT, MAX_LIMIT};

const NODEINFO_CONTENT_TYPE: &str =
    "application/json; profile=\"http://nodeinfo.diaspora.software/ns/schema/2.1#\"";

/// Everything the handlers need.
#[derive(Debug, Clone)]
pub struct AppState {
    pub base_url: String,
    pub actor_username: String,
    pub name: String,
    /// The SPKI PEM of our actor key.
    pub public_key_pem: String,
    pub posts: SocialReader<SocialPost>,
    pub accounts: SocialReader<SocialAccount>,
    pub tombstones: Arc<RwLock<Tombstones>>,
    /// Where `GET /` redirects. `None`: `/` is a 404 like any other unknown path.
    pub root_redirect: Option<String>,
}

pub fn router(state: AppState) -> Router {
    let root = state.root_redirect.as_deref().and_then(|target| {
        HeaderValue::from_str(target)
            .inspect_err(|_| tracing::warn!(target, "root_redirect is not a valid header value"))
            .ok()
    });
    let routes = Router::new();
    let routes = match root {
        Some(location) => routes.route(
            "/",
            get(move || std::future::ready(redirect(location.clone()))),
        ),
        None => routes,
    };
    routes
        .route("/actor", get(actor))
        .route("/.well-known/webfinger", get(webfinger_route))
        .route("/.well-known/nodeinfo", get(nodeinfo_index))
        .route("/nodeinfo/2.1", get(nodeinfo_document))
        .route("/api/v1/search/posts", get(search_posts_route))
        .route("/api/v1/search/accounts", get(search_accounts_route))
        .with_state(Arc::new(state))
}

fn respond<T: Serialize>(status: StatusCode, content_type: &'static str, value: &T) -> Response {
    // Serialising our own response types cannot fail; `null` is the safe fallback.
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec());
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

fn json_ok<T: Serialize>(value: &T) -> Response {
    respond(StatusCode::OK, "application/json", value)
}

fn error(status: StatusCode, message: &str) -> Response {
    respond(status, "application/json", &json!({ "error": message }))
}

/// `GET /`. 302, not 301 or 308: a permanent redirect is cached by browsers indefinitely, and
/// the operator may change `root_redirect` (where their site lives is their choice, and ours
/// is only an API). 302 rather than 303 because `/` is never a POST target, so the two mean
/// the same here and 302 is the conventional "go there instead, for now".
fn redirect(location: HeaderValue) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::FOUND;
    response.headers_mut().insert(header::LOCATION, location);
    response
}

async fn actor(State(state): State<Arc<AppState>>) -> Response {
    let mut document = server_actor(
        &state.base_url,
        &state.actor_username,
        &state.public_key_pem,
    );
    if let Some(object) = document.as_object_mut() {
        object.insert("name".to_string(), Value::from(state.name.as_str()));
    }
    respond(StatusCode::OK, "application/activity+json", &document)
}

async fn webfinger_route(
    State(state): State<Arc<AppState>>,
    RawQuery(query): RawQuery,
) -> Response {
    let params = parse_query(query.as_deref());
    let found = params
        .get("resource")
        .and_then(|r| webfinger(r, &state.base_url, &state.actor_username));
    match found {
        Some(document) => respond(StatusCode::OK, "application/jrd+json", &document),
        None => error(StatusCode::NOT_FOUND, "unknown resource"),
    }
}

async fn nodeinfo_index(State(state): State<Arc<AppState>>) -> Response {
    json_ok(&nodeinfo_well_known(&state.base_url))
}

async fn nodeinfo_document() -> Response {
    respond(
        StatusCode::OK,
        NODEINFO_CONTENT_TYPE,
        &nodeinfo(env!("CARGO_PKG_VERSION")),
    )
}

fn parse_query(query: Option<&str>) -> HashMap<String, String> {
    url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .into_owned()
        .collect()
}

/// The `q` and `limit` every search takes, or the message for a 422.
fn common_params(params: &HashMap<String, String>) -> Result<(String, usize), &'static str> {
    let q = params.get("q").map(|q| q.trim()).unwrap_or_default();
    if q.is_empty() {
        return Err("the `q` parameter is required");
    }
    let limit = match params.get("limit").map(|l| l.trim()) {
        None | Some("") => DEFAULT_LIMIT,
        Some(text) => match text.parse::<usize>() {
            Ok(n) if n >= 1 => n.min(MAX_LIMIT),
            _ => return Err("`limit` must be a positive whole number"),
        },
    };
    Ok((q.to_string(), limit))
}

async fn blocking<T: Serialize + Send + 'static>(
    what: &'static str,
    work: impl FnOnce() -> Result<T, tinyindex::IndexError> + Send + 'static,
) -> Response {
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(results)) => json_ok(&results),
        Ok(Err(e)) => {
            tracing::error!(error = %e, "{what} search failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "the search failed")
        }
        Err(e) => {
            tracing::error!(error = %e, "{what} search task failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "the search failed")
        }
    }
}

async fn search_posts_route(
    State(state): State<Arc<AppState>>,
    RawQuery(query): RawQuery,
) -> Response {
    let (q, limit) = match common_params(&parse_query(query.as_deref())) {
        Ok(params) => params,
        Err(message) => return error(StatusCode::UNPROCESSABLE_ENTITY, message),
    };
    let state = Arc::clone(&state);
    blocking("post", move || {
        search::posts(&state.posts, &state.tombstones, &q, limit, now_secs())
    })
    .await
}

async fn search_accounts_route(
    State(state): State<Arc<AppState>>,
    RawQuery(query): RawQuery,
) -> Response {
    let params = parse_query(query.as_deref());
    let (q, limit) = match common_params(&params) {
        Ok(params) => params,
        Err(message) => return error(StatusCode::UNPROCESSABLE_ENTITY, message),
    };
    let language = params
        .get("language")
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty());
    let state = Arc::clone(&state);
    blocking("account", move || {
        search::accounts(
            &state.accounts,
            &state.tombstones,
            &q,
            limit,
            language.as_deref(),
        )
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_parsing() {
        let p = parse_query(Some("q=hello%20world&limit=5&x="));
        assert_eq!(p.get("q").map(String::as_str), Some("hello world"));
        assert_eq!(p.get("limit").map(String::as_str), Some("5"));
        assert!(parse_query(None).is_empty());
    }

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn q_and_limit_validation() {
        assert!(matches!(common_params(&params(&[("q", "rust")])), Ok((q, 20)) if q == "rust"));
        assert!(matches!(
            common_params(&params(&[("q", "a"), ("limit", "500")])),
            Ok((_, 100))
        ));
        assert!(matches!(
            common_params(&params(&[("q", "a"), ("limit", "7")])),
            Ok((_, 7))
        ));
        for bad in [
            params(&[]),
            params(&[("q", "")]),
            params(&[("q", "   ")]),
            params(&[("q", "a"), ("limit", "0")]),
            params(&[("q", "a"), ("limit", "-1")]),
            params(&[("q", "a"), ("limit", "lots")]),
        ] {
            assert!(common_params(&bad).is_err(), "{bad:?}");
        }
    }
}
