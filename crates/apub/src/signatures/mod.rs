//! HTTP message signatures for `ActivityPub` and FASP traffic.
//!
//! Everything here works on plain data (a method, a target URI, a status, a header lookup), not on
//! axum or reqwest types, so the same code serves the client and the server side.
//!
//! - [`integrity`]: `Content-Digest` (RFC 9530) and the older `Digest` header.
//! - [`rfc9421`]: RFC 9421 signatures with a fixed set of components. Used for FASP traffic and
//!   tried first when fetching.
//! - [`cavage`]: draft-cavage-http-signatures-12, which most Mastodon-era servers accept.
//! - [`keys`]: RSA and Ed25519 keys and the formats they are exchanged in.

pub mod cavage;
mod error;
pub mod integrity;
pub mod keys;
pub mod rfc9421;
mod sf;
mod uri;

pub use error::SignatureError;
pub use integrity::{content_digest, verify_content_digest};
pub use uri::authority;

/// A lookup of HTTP header values by name.
///
/// Implemented for slices, arrays and vectors of `(name, value)` pairs (names compared
/// case-insensitively) and, through [`FnHeaders`], for a closure over any other header map.
pub trait Headers {
    /// Every value of the header `name`, in message order. `name` is lowercase.
    fn get_all(&self, name: &str) -> Vec<String>;
}

impl<N: AsRef<str>, V: AsRef<str>> Headers for [(N, V)] {
    fn get_all(&self, name: &str) -> Vec<String> {
        self.iter()
            .filter(|(n, _)| n.as_ref().eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_ref().to_owned())
            .collect()
    }
}

impl<N: AsRef<str>, V: AsRef<str>> Headers for Vec<(N, V)> {
    fn get_all(&self, name: &str) -> Vec<String> {
        self.as_slice().get_all(name)
    }
}

impl<N: AsRef<str>, V: AsRef<str>, const LEN: usize> Headers for [(N, V); LEN] {
    fn get_all(&self, name: &str) -> Vec<String> {
        self.as_slice().get_all(name)
    }
}

/// Adapts a closure returning every value of a header into [`Headers`].
pub struct FnHeaders<F>(pub F);

impl<F: Fn(&str) -> Vec<String>> Headers for FnHeaders<F> {
    fn get_all(&self, name: &str) -> Vec<String> {
        (self.0)(name)
    }
}

/// The value of header `name`, with several header lines combined with `", "` (RFC 9110 5.3).
///
/// `Ok(None)` when the header is absent. Values containing a line break are refused, since they
/// cannot be represented in a signature base.
pub(crate) fn combined_header<H: Headers + ?Sized>(
    headers: &H,
    name: &str,
) -> Result<Option<String>, SignatureError> {
    let values = headers.get_all(name);
    if values.is_empty() {
        return Ok(None);
    }
    let mut parts = Vec::with_capacity(values.len());
    for value in &values {
        if value.contains(['\r', '\n']) {
            return Err(SignatureError::InvalidHeaderValue {
                name: name.to_owned(),
            });
        }
        parts.push(value.trim());
    }
    Ok(Some(parts.join(", ")))
}

/// Like [`combined_header`], but a missing header is an error.
pub(crate) fn required_header<H: Headers + ?Sized>(
    headers: &H,
    name: &str,
) -> Result<String, SignatureError> {
    combined_header(headers, name)?.ok_or_else(|| SignatureError::MissingHeader {
        name: name.to_owned(),
    })
}

/// Formats a Unix timestamp as an HTTP date (`Date` header value, IMF-fixdate).
#[must_use]
pub fn http_date(unix_secs: i64) -> String {
    let secs = u64::try_from(unix_secs).unwrap_or(0);
    httpdate::fmt_http_date(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
}

/// Parses an HTTP date into Unix seconds.
pub fn parse_http_date(value: &str) -> Result<i64, SignatureError> {
    let time =
        httpdate::parse_http_date(value.trim()).map_err(|_| SignatureError::MalformedDate {
            value: value.to_owned(),
        })?;
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).map_err(|_| SignatureError::MalformedDate {
            value: value.to_owned(),
        }),
        Err(_) => Err(SignatureError::MalformedDate {
            value: value.to_owned(),
        }),
    }
}

/// Checks that `instant` is within `max_skew_secs` of `now`.
pub(crate) fn check_skew(
    what: &'static str,
    instant: i64,
    now: i64,
    max_skew_secs: i64,
) -> Result<(), SignatureError> {
    let age = now.saturating_sub(instant);
    if age > max_skew_secs {
        Err(SignatureError::TooOld {
            what,
            seconds: age,
            max: max_skew_secs,
        })
    } else if age < max_skew_secs.saturating_neg() {
        Err(SignatureError::InFuture {
            what,
            seconds: age.saturating_neg(),
            max: max_skew_secs,
        })
    } else {
        Ok(())
    }
}
