//! Body digests: `Content-Digest` (RFC 9530, what FASP and RFC 9421 use) and the older `Digest`
//! header (RFC 3230, what draft-cavage signatures cover).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};

use super::sf::{self, BareItem, Value};
use super::{Headers, SignatureError, required_header};

fn sha256(body: &[u8]) -> [u8; 32] {
    Sha256::digest(body).into()
}

/// The `Content-Digest` header value for `body`: `sha-256=:<base64>:`.
#[must_use]
pub fn content_digest(body: &[u8]) -> String {
    format!("sha-256=:{}:", STANDARD.encode(sha256(body)))
}

/// Checks a `Content-Digest` header value against `body`.
///
/// The value is an RFC 8941 dictionary. It is accepted if it has a `sha-256` member that matches;
/// members for other algorithms are ignored, and it is an error to have no `sha-256` member.
pub fn verify_content_digest(header_value: &str, body: &[u8]) -> Result<(), SignatureError> {
    let malformed = |reason: String| SignatureError::MalformedDigest { reason };
    let members = sf::parse_dictionary(header_value).map_err(malformed)?;
    let member = sf::find(&members, "sha-256").ok_or(SignatureError::NoSha256Digest)?;
    match &member.value {
        Value::Item(item) => match &item.value {
            BareItem::Bytes(expected) if expected.as_slice() == sha256(body) => Ok(()),
            BareItem::Bytes(_) => Err(SignatureError::DigestMismatch),
            _ => Err(malformed(
                "sha-256 member is not a byte sequence".to_owned(),
            )),
        },
        Value::InnerList(_) => Err(malformed("sha-256 member is a list".to_owned())),
    }
}

/// Reads the `content-digest` header from `headers` and checks it against `body`.
pub fn verify_content_digest_header<H: Headers + ?Sized>(
    headers: &H,
    body: &[u8],
) -> Result<(), SignatureError> {
    verify_content_digest(&required_header(headers, "content-digest")?, body)
}

/// The legacy `Digest` header value for `body`: `SHA-256=<base64>`.
#[must_use]
pub fn legacy_digest(body: &[u8]) -> String {
    format!("SHA-256={}", STANDARD.encode(sha256(body)))
}

/// Checks a legacy `Digest` header value (`SHA-256=<base64>[,<alg>=<base64>...]`) against `body`.
/// Accepted if a `SHA-256` member (any case) matches; other algorithms are ignored.
pub fn verify_legacy_digest(header_value: &str, body: &[u8]) -> Result<(), SignatureError> {
    let expected = sha256(body);
    let mut seen = false;
    for member in header_value.split(',') {
        let Some((alg, value)) = member.trim().split_once('=') else {
            return Err(SignatureError::MalformedDigest {
                reason: "member without `=`".to_owned(),
            });
        };
        if !alg.trim().eq_ignore_ascii_case("sha-256") {
            continue;
        }
        seen = true;
        let decoded =
            STANDARD
                .decode(value.trim())
                .map_err(|e| SignatureError::MalformedDigest {
                    reason: format!("sha-256 value is not base64: {e}"),
                })?;
        if decoded.as_slice() == expected {
            return Ok(());
        }
    }
    if seen {
        Err(SignatureError::DigestMismatch)
    } else {
        Err(SignatureError::NoSha256Digest)
    }
}
