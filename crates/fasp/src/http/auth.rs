//! Request authentication.

use apub::signatures::keys::{Ed25519VerifyingKey, VerifyingKey};
use apub::signatures::rfc9421::{self, VerifyPolicy};
use apub::signatures::{FnHeaders, SignatureError, integrity};
use axum::http::request::Parts;
use store::{Registration, RegistrationStatus, StoreError};
use thiserror::Error;

use super::{FaspState, MAX_SKEW_SECS};

/// Why a request was not authenticated. Only ever logged, never sent to the peer.
#[derive(Debug, Error)]
pub(crate) enum AuthFailure {
    #[error("{0}")]
    Signature(#[from] SignatureError),
    #[error("no registration for the signature's keyid")]
    UnknownKeyId,
    #[error("registration {0} is revoked")]
    Revoked(i64),
    #[error("registration {0} has no stored public key for the server")]
    NoPublicKey(i64),
    #[error("registration {id} has an unusable public key: {reason}")]
    BadPublicKey { id: i64, reason: String },
    #[error("store failure: {0}")]
    Store(#[from] StoreError),
}

/// `@target-uri` as the peer built it: our base URL, then the path and query as received.
pub(crate) fn target_uri(base_url: &str, parts: &Parts) -> String {
    let mut uri = format!("{}{}", base_url.trim_end_matches('/'), parts.uri.path());
    if let Some(query) = parts.uri.query() {
        uri.push('?');
        uri.push_str(query);
    }
    uri
}

/// The keyid of the `sig1` signature, read before verification to find the registration.
fn keyid(lookup: &FnHeaders<impl Fn(&str) -> Vec<String>>) -> Result<String, SignatureError> {
    use apub::signatures::Headers;
    let value = lookup.get_all("signature-input").join(", ");
    if value.is_empty() {
        return Err(SignatureError::MissingHeader {
            name: "signature-input".to_owned(),
        });
    }
    let members = rfc9421::parse_signature_input(&value)?;
    let (_, _, params) = members
        .into_iter()
        .find(|(label, _, _)| label == rfc9421::DEFAULT_LABEL)
        .ok_or_else(|| SignatureError::LabelNotFound {
            label: rfc9421::DEFAULT_LABEL.to_owned(),
        })?;
    params.keyid.ok_or(SignatureError::MissingKeyId)
}

/// Authenticates a request and returns its registration, flipping `pending` to `active` on the
/// first request that verifies.
pub(crate) fn authenticate<S: FaspState>(
    state: &S,
    parts: &Parts,
    body: &[u8],
    now: i64,
) -> Result<Registration, AuthFailure> {
    let lookup = FnHeaders(|name: &str| {
        parts
            .headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    });
    let server_id = keyid(&lookup)?;
    let mut registration = state
        .with_store(|s| s.registration_by_server_id(&server_id))?
        .ok_or(AuthFailure::UnknownKeyId)?;
    if registration.status == RegistrationStatus::Revoked {
        return Err(AuthFailure::Revoked(registration.id));
    }
    let their_key = registration
        .their_public_key
        .as_deref()
        .ok_or(AuthFailure::NoPublicKey(registration.id))?;
    let their_key =
        Ed25519VerifyingKey::from_bytes(their_key).map_err(|e| AuthFailure::BadPublicKey {
            id: registration.id,
            reason: e.to_string(),
        })?;

    integrity::verify_content_digest_header(&lookup, body)?;
    let target = target_uri(state.base_url(), parts);
    let policy = VerifyPolicy::new(
        &["@method", "@target-uri", "content-digest"],
        now,
        MAX_SKEW_SECS,
    );
    rfc9421::verify_request(parts.method.as_str(), &target, &lookup, &policy, |id| {
        (id == server_id).then(|| VerifyingKey::Ed25519(their_key.clone()))
    })?;

    if registration.status == RegistrationStatus::Pending
        && state.with_store(|s| s.activate_registration(registration.id))?
    {
        registration.status = RegistrationStatus::Active;
        tracing::info!(
            registration = registration.id,
            host = registration.base_url,
            "registration is now active: first verified request received"
        );
    }
    Ok(registration)
}
