//! The admin API: what the `register`, `registrations` and `unregister` commands do, over HTTP,
//! for an operator's own sign-up page (PLAN.md, Deployment and adoption).
//!
//! - `POST /api/v1/admin/registrations` with `{"server_url": "https://...", "fasp_base_url"?:
//!   "https://..."}` registers with that server: 201 with the registration (see
//!   [`crate::registrations::registered_json`]), including `fingerprint` and `approval_url`.
//!   400 for an unusable URL, 409 if that server is already registered (the body carries its
//!   `id`), 502 when the server cannot be reached, is not a FASP host or refuses, 422 for a body
//!   of any other shape.
//! - `GET /api/v1/admin/registrations`: every registration. `GET .../{id}`: one, or 404.
//! - `DELETE /api/v1/admin/registrations/{id}`: revokes it, as `unregister` does, or 404.
//!
//! No `admin_tokens` configured: 404. A missing or wrong bearer token: 401
//! ([`crate::operator`]).
//!
//! It runs inside `serve`, on the FASP state's store connection and HTTP client, and like the
//! commands it writes only SQLite, never an index.

use std::sync::Arc;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use fasp::{RegistrationError, StoreAccess};
use serde::Deserialize;
use serde_json::{Value, json};
use store::StoreError;

use crate::config::Config;
use crate::fasp_app::FaspApp;
use crate::now_secs;
use crate::operator::{TokenCheck, json_error, json_response};
use crate::registrations::{
    RegisterError, get_registration, list_registrations, register_server, registered_json,
    revoke_registration,
};

/// The collection route.
pub const PATH: &str = "/api/v1/admin/registrations";
/// One registration.
pub const ITEM_PATH: &str = "/api/v1/admin/registrations/{id}";
/// Larger bodies get 413.
pub const MAX_BODY_BYTES: usize = 16 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewRegistration {
    server_url: String,
    fasp_base_url: Option<String>,
}

struct Inner {
    tokens: TokenCheck,
    fasp: FaspApp,
    config: Config,
}

/// The admin API.
#[derive(Clone)]
pub struct AdminApp {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for AdminApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminApp")
            .field("tokens", &self.inner.tokens.len())
            .finish_non_exhaustive()
    }
}

impl AdminApp {
    /// `fasp` is the server's FASP state, whose store and HTTP client registrations use.
    #[must_use]
    pub fn new(fasp: FaspApp, config: &Config) -> Self {
        Self {
            inner: Arc::new(Inner {
                tokens: TokenCheck::new(&config.admin_tokens.0),
                fasp,
                config: config.clone(),
            }),
        }
    }

    pub fn routes(&self) -> Router {
        Router::new()
            .route(PATH, get(list_route).post(register_route))
            .route(ITEM_PATH, get(get_route).delete(delete_route))
            .with_state(self.clone())
    }

    /// Runs `work` on the store off the async threads. A failure is logged and its `Err` is
    /// `what`, for [`internal_error`].
    async fn with_store(
        &self,
        what: &'static str,
        work: impl FnOnce(&store::Store) -> Result<Option<Value>, StoreError> + Send + 'static,
    ) -> Result<Option<Value>, &'static str> {
        let fasp = self.inner.fasp.clone();
        match tokio::task::spawn_blocking(move || fasp.with_store(work)).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => {
                tracing::error!(error = %e, "admin API: failed to {what}");
                Err(what)
            }
            Err(e) => {
                tracing::error!(error = %e, "admin API: the task to {what} failed");
                Err(what)
            }
        }
    }
}

/// The 500 for a store failure while trying to do `what`.
fn internal_error(what: &str) -> Response {
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        &format!("failed to {what}"),
    )
}

fn not_found(id: &str) -> Response {
    json_error(StatusCode::NOT_FOUND, &format!("no registration {id}"))
}

/// The answer for one registration: 200 with it, 404 without, or 500.
fn one(id: &str, result: Result<Option<Value>, &'static str>) -> Response {
    match result {
        Ok(Some(value)) => json_response(StatusCode::OK, &value),
        Ok(None) => not_found(id),
        Err(what) => internal_error(what),
    }
}

async fn list_route(State(app): State<AdminApp>, request: Request) -> Response {
    if let Some(rejected) = app.inner.tokens.reject(request.headers()) {
        return rejected;
    }
    match app
        .with_store("list registrations", |s| list_registrations(s).map(Some))
        .await
    {
        Ok(value) => json_response(StatusCode::OK, &value.unwrap_or(Value::Null)),
        Err(what) => internal_error(what),
    }
}

async fn get_route(
    State(app): State<AdminApp>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    if let Some(rejected) = app.inner.tokens.reject(request.headers()) {
        return rejected;
    }
    let Ok(number) = id.parse::<i64>() else {
        return not_found(&id);
    };
    let result = app
        .with_store("read a registration", move |s| get_registration(s, number))
        .await;
    one(&id, result)
}

async fn delete_route(
    State(app): State<AdminApp>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    if let Some(rejected) = app.inner.tokens.reject(request.headers()) {
        return rejected;
    }
    let Ok(number) = id.parse::<i64>() else {
        return not_found(&id);
    };
    let result = app
        .with_store("revoke a registration", move |s| {
            revoke_registration(s, number, now_secs())
        })
        .await;
    one(&id, result)
}

/// The status and message for a failed registration.
fn register_failure(server_url: &str, error: &RegisterError) -> Response {
    let status = match error {
        RegisterError::BadUrl { .. }
        | RegisterError::Registration(RegistrationError::InvalidUrl { .. }) => {
            StatusCode::BAD_REQUEST
        }
        RegisterError::Registration(RegistrationError::AlreadyRegistered {
            registration_id,
            ..
        }) => {
            return json_response(
                StatusCode::CONFLICT,
                &json!({ "error": error.to_string(), "id": registration_id }),
            );
        }
        RegisterError::Registration(
            RegistrationError::Http { .. }
            | RegistrationError::Status { .. }
            | RegistrationError::BadResponse { .. }
            | RegistrationError::NoFaspBaseUrl { .. },
        ) => StatusCode::BAD_GATEWAY,
        RegisterError::Config(_)
        | RegisterError::Registration(
            RegistrationError::Key { .. } | RegistrationError::Store(_),
        ) => {
            tracing::error!(server_url, error = %error, "admin API: registration failed");
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("registering with {server_url} failed on our side"),
            );
        }
    };
    tracing::info!(server_url, error = %error, "admin API: registration refused");
    json_error(
        status,
        &format!("failed to register with {server_url}: {error}"),
    )
}

async fn register_route(State(app): State<AdminApp>, request: Request) -> Response {
    if let Some(rejected) = app.inner.tokens.reject(request.headers()) {
        return rejected;
    }
    let Ok(body) = to_bytes(request.into_body(), MAX_BODY_BYTES).await else {
        return json_error(StatusCode::PAYLOAD_TOO_LARGE, "the body is too large");
    };
    let Ok(new) = serde_json::from_slice::<NewRegistration>(&body) else {
        return json_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "the body must be {\"server_url\": \"https://...\"}, optionally with \"fasp_base_url\"",
        );
    };
    let fasp = &app.inner.fasp;
    let outcome = match register_server(
        fasp.http(),
        fasp,
        &app.inner.config,
        &new.server_url,
        new.fasp_base_url.as_deref(),
        now_secs(),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(e) => return register_failure(&new.server_url, &e),
    };
    let id = outcome.registration_id;
    match app
        .with_store("read the new registration", move |s| {
            registered_json(s, &outcome).map(Some)
        })
        .await
    {
        Ok(value) => {
            let mut response = json_response(StatusCode::CREATED, &value.unwrap_or(Value::Null));
            if let Ok(location) = HeaderValue::from_str(&format!("{PATH}/{id}")) {
                response.headers_mut().insert(header::LOCATION, location);
            }
            response
        }
        Err(what) => internal_error(what),
    }
}
