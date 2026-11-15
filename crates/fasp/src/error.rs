use std::time::Duration;

use apub::signatures::SignatureError;
use apub::signatures::keys::KeyError;
use store::StoreError;
use thiserror::Error;

/// Registering with a server failed.
#[derive(Debug, Error)]
pub enum RegistrationError {
    #[error("invalid URL `{url}`: {reason}")]
    InvalidUrl { url: String, reason: String },
    #[error("request to {url} failed: {source}")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("{url} answered HTTP {status}")]
    Status { url: String, status: u16 },
    #[error("{url} sent an unusable response: {reason}")]
    BadResponse { url: String, reason: String },
    #[error(
        "cannot find the FASP base URL of {server}: its nodeinfo has no metadata.faspBaseUrl and \
         its software is `{software}`, not mastodon; pass the FASP base URL explicitly"
    )]
    NoFaspBaseUrl { server: String, software: String },
    #[error("{base_url} is already registered (registration {registration_id})")]
    AlreadyRegistered {
        base_url: String,
        registration_id: i64,
    },
    #[error("failed to set up keys for {base_url}: {source}")]
    Key {
        base_url: String,
        #[source]
        source: KeyError,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// A capability's API was about to be called but the store does not say it is enabled.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("capability {capability} is not enabled for registration {registration_id} ({host})")]
pub struct NotEnabled {
    pub registration_id: i64,
    pub host: String,
    pub capability: String,
}

/// A call to a server's FASP API failed. Every variant names the host.
#[derive(Debug, Error)]
pub enum ClientError {
    #[error(transparent)]
    NotEnabled(#[from] NotEnabled),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("registration {registration_id} ({host}) cannot be used as a client: {reason}")]
    Setup {
        registration_id: i64,
        host: String,
        reason: String,
    },
    #[error("request to {host}{path} failed: {source}")]
    Http {
        host: String,
        path: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("failed to sign request to {host}{path}: {source}")]
    Signing {
        host: String,
        path: String,
        #[source]
        source: SignatureError,
    },
    #[error("response from {host}{path} (HTTP {status}) failed verification: {source}")]
    ResponseVerification {
        host: String,
        path: String,
        status: u16,
        #[source]
        source: SignatureError,
    },
    #[error("{host}{path} is rate limiting us; retry after {retry_after:?}")]
    RateLimited {
        host: String,
        path: String,
        retry_after: Duration,
    },
    #[error("{host}{path} answered HTTP {status}")]
    Status {
        host: String,
        path: String,
        status: u16,
    },
    #[error("{host}{path} sent an unusable response: {reason}")]
    BadResponse {
        host: String,
        path: String,
        reason: String,
    },
}
