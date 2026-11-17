//! What the operator APIs share: the crawl submissions endpoint ([`crate::crawl_app`]) and the
//! admin API ([`crate::admin_app`]) both take bearer tokens from the config and answer JSON.
//!
//! - No tokens configured: 404, as if the endpoint did not exist.
//! - A missing or wrong bearer token: 401 with `WWW-Authenticate: Bearer`.
//!
//! Tokens are compared by their SHA-256 digests, in constant time.

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use serde_json::json;
use sha2::{Digest, Sha256};

/// A JSON response.
pub(crate) fn json_response(status: StatusCode, body: &serde_json::Value) -> Response {
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// `{"error": message}` with `status`.
pub(crate) fn json_error(status: StatusCode, message: &str) -> Response {
    json_response(status, &json!({ "error": message }))
}

fn digest(token: &[u8]) -> [u8; 32] {
    Sha256::digest(token).into()
}

/// The bearer token of an `Authorization` header.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.trim().split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

/// The accepted tokens of one API, kept only as digests.
#[derive(Clone)]
pub struct TokenCheck {
    digests: Vec<[u8; 32]>,
}

impl std::fmt::Debug for TokenCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TokenCheck({} tokens)", self.digests.len())
    }
}

impl TokenCheck {
    #[must_use]
    pub fn new(tokens: &[String]) -> Self {
        Self {
            digests: tokens.iter().map(|t| digest(t.as_bytes())).collect(),
        }
    }

    /// No tokens: the API is off.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.digests.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.digests.len()
    }

    /// Whether `given` is one of the tokens, looking at every one in full.
    #[must_use]
    pub fn matches(&self, given: &str) -> bool {
        let given = digest(given.as_bytes());
        // No early exit, neither within a digest nor across tokens.
        let matched = self.digests.iter().fold(0_u8, |matched, token| {
            let diff = token
                .iter()
                .zip(given.iter())
                .fold(0_u8, |acc, (a, b)| acc | (a ^ b));
            matched | u8::from(diff == 0)
        });
        matched != 0
    }

    /// The response to send instead of handling a request with `headers`, if any: 404 when
    /// the API is off, 401 without a valid bearer token.
    #[must_use]
    pub fn reject(&self, headers: &HeaderMap) -> Option<Response> {
        if self.is_empty() {
            return Some(json_error(StatusCode::NOT_FOUND, "not found"));
        }
        if bearer(headers).is_some_and(|t| self.matches(t)) {
            return None;
        }
        let mut response = json_error(StatusCode::UNAUTHORIZED, "a valid bearer token is required");
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        Some(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(value: &'static str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static(value));
        h
    }

    #[test]
    fn tokens_and_bearer_headers() {
        let check = TokenCheck::new(&["0123456789abcdef".into(), "fedcba9876543210".into()]);
        assert!(check.matches("0123456789abcdef"));
        assert!(check.matches("fedcba9876543210"));
        assert!(!check.matches("0123456789abcde"));
        assert!(!check.matches(""));
        assert!(!TokenCheck::new(&[]).matches("0123456789abcdef"));
        assert_eq!(bearer(&headers("Bearer abc")), Some("abc"));
        assert_eq!(bearer(&headers("bearer  abc ")), Some("abc"));
        assert_eq!(bearer(&headers("Basic abc")), None);
        assert_eq!(bearer(&headers("Bearer ")), None);
        assert_eq!(bearer(&headers("Bearer")), None);
        assert_eq!(bearer(&HeaderMap::new()), None);
        assert!(!format!("{check:?}").contains("0123"));
    }

    #[test]
    fn rejections() {
        let status = |check: &TokenCheck, h: &HeaderMap| check.reject(h).map(|r| r.status());
        let off = TokenCheck::new(&[]);
        assert_eq!(
            status(&off, &headers("Bearer 0123456789abcdef")),
            Some(StatusCode::NOT_FOUND)
        );
        let on = TokenCheck::new(&["0123456789abcdef".into()]);
        assert_eq!(status(&on, &headers("Bearer 0123456789abcdef")), None);
        assert_eq!(
            status(&on, &headers("Bearer wrong-token-000000")),
            Some(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(
            status(&on, &HeaderMap::new()),
            Some(StatusCode::UNAUTHORIZED)
        );
    }
}
