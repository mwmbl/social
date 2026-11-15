//! Building and signing responses.

use apub::signatures::keys::{Ed25519SigningKey, SigningKey};
use apub::signatures::{content_digest, rfc9421};
use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use serde::Serialize;
use store::Registration;

/// A handler's answer before digest and signature are added.
pub(crate) struct Reply {
    pub status: StatusCode,
    /// The JSON body; empty for 204 and bodiless errors.
    pub body: Vec<u8>,
}

impl Reply {
    pub fn no_content() -> Self {
        Self::status(StatusCode::NO_CONTENT)
    }

    pub fn status(status: StatusCode) -> Self {
        Self {
            status,
            body: Vec::new(),
        }
    }

    pub fn json<T: Serialize>(status: StatusCode, value: &T) -> Self {
        match serde_json::to_vec(value) {
            Ok(body) => Self { status, body },
            Err(_) => Self::status(StatusCode::INTERNAL_SERVER_ERROR),
        }
    }

    pub fn error(status: StatusCode, message: &str) -> Self {
        Self::json(status, &serde_json::json!({ "error": message }))
    }
}

fn build(status: StatusCode, body: Vec<u8>, extra: &[(&'static str, String)]) -> Response {
    let json = !body.is_empty();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    if json {
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
    }
    for (name, value) in extra {
        if let Ok(value) = HeaderValue::from_str(value) {
            headers.insert(*name, value);
        }
    }
    response
}

/// An error answer for a request we could not attribute to a registration: no signature, but
/// still a correct `content-digest`.
pub(crate) fn unsigned(status: StatusCode, message: &str) -> Response {
    let reply = Reply::error(status, message);
    let digest = content_digest(&reply.body);
    build(reply.status, reply.body, &[("content-digest", digest)])
}

/// Adds `content-digest` and signs `("@status" "content-digest")` with the registration's key,
/// keyid = our `serverId`.
pub(crate) fn signed(registration: &Registration, reply: Reply, now: i64) -> Response {
    let digest = content_digest(&reply.body);
    let key = match Ed25519SigningKey::from_seed_slice(&registration.our_private_key) {
        Ok(key) => SigningKey::Ed25519(key),
        Err(e) => {
            tracing::error!(registration = registration.id, error = %e,
                "stored registration key is unusable, cannot sign response");
            return unsigned(StatusCode::INTERNAL_SERVER_ERROR, "cannot sign response");
        }
    };
    let signature = rfc9421::sign_response(
        reply.status.as_u16(),
        &[("content-digest", digest.as_str())],
        &["@status", "content-digest"],
        &registration.server_id,
        now,
        &key,
    );
    match signature {
        Ok((input, signature)) => build(
            reply.status,
            reply.body,
            &[
                ("content-digest", digest),
                ("signature-input", input),
                ("signature", signature),
            ],
        ),
        Err(e) => {
            tracing::error!(registration = registration.id, error = %e, "failed to sign response");
            unsigned(StatusCode::INTERNAL_SERVER_ERROR, "cannot sign response")
        }
    }
}

#[cfg(test)]
mod tests {
    use apub::signatures::keys::VerifyingKey;
    use apub::signatures::rfc9421::VerifyPolicy;
    use apub::signatures::{FnHeaders, integrity};
    use store::RegistrationStatus;

    use super::*;

    fn registration(key: &Ed25519SigningKey) -> Registration {
        Registration {
            id: 1,
            base_url: "https://m.example/api/fasp".to_owned(),
            server_id: "server-id-abcdefgh".to_owned(),
            fasp_id: Some("f".to_owned()),
            our_private_key: key.to_seed().to_vec(),
            their_public_key: None,
            status: RegistrationStatus::Active,
            created_at: 0,
        }
    }

    fn header(response: &Response, name: &str) -> String {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn signed_responses_carry_the_exact_digest_and_a_verifiable_signature() {
        let key = Ed25519SigningKey::generate().unwrap();
        let reg = registration(&key);
        for (status, body) in [
            (StatusCode::NO_CONTENT, Vec::new()),
            (StatusCode::OK, br#"{"a":1}"#.to_vec()),
        ] {
            let response = signed(
                &reg,
                Reply {
                    status,
                    body: body.clone(),
                },
                1_000,
            );
            // Exactly `sha-256=:<b64>:`, nothing else in the header.
            let digest = header(&response, "content-digest");
            assert_eq!(digest, content_digest(&body));
            assert!(digest.starts_with("sha-256=:") && digest.ends_with(':'));
            assert_eq!(digest.matches(',').count(), 0);
            assert_eq!(
                header(&response, "signature-input"),
                format!(
                    "sig1=(\"@status\" \"content-digest\");created=1000;keyid=\"{}\"",
                    reg.server_id
                )
            );
            let lookup = FnHeaders(|name: &str| vec![header(&response, name)]);
            integrity::verify_content_digest_header(&lookup, &body).unwrap();
            let policy = VerifyPolicy::new(&["@status", "content-digest"], 1_000, 300);
            let verifying = VerifyingKey::Ed25519(key.verifying_key());
            rfc9421::verify_response(status.as_u16(), &lookup, &policy, |_| {
                Some(verifying.clone())
            })
            .unwrap();
            // The status is covered: the same headers do not verify for another status.
            assert!(
                rfc9421::verify_response(500, &lookup, &policy, |_| Some(verifying.clone()))
                    .is_err()
            );
        }
    }

    #[test]
    fn unsigned_errors_still_carry_a_digest() {
        let response = unsigned(StatusCode::UNAUTHORIZED, "authentication failed");
        assert!(response.headers().get("signature").is_none());
        assert!(header(&response, "content-digest").starts_with("sha-256=:"));
    }
}
