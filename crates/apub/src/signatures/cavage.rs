//! draft-cavage-http-signatures-12, the scheme most Mastodon-era `ActivityPub` servers accept:
//!
//! `Signature: keyId="...",algorithm="rsa-sha256",headers="(request-target) host date digest",signature="..."`
//!
//! Only RSASSA-PKCS1-v1_5 with SHA-256 is supported. `hs2019` is accepted on verification and
//! treated as that, as Mastodon does. Pseudo-headers `(request-target)`, `(created)` and
//! `(expires)` are supported alongside ordinary headers.

use std::fmt::Write as _;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use super::keys::{RsaSigningKey, RsaVerifyingKey};
use super::{Headers, SignatureError, check_skew, parse_http_date, required_header, uri};

/// The headers to sign for a GET: enough to bind the request to a host and a moment.
pub const GET_HEADERS: &[&str] = &["(request-target)", "host", "date"];
/// The headers to sign for a POST: as for a GET, plus the body digest.
pub const POST_HEADERS: &[&str] = &["(request-target)", "host", "date", "digest"];

/// A parsed `Signature` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureHeader {
    pub key_id: String,
    pub algorithm: Option<String>,
    pub created: Option<i64>,
    pub expires: Option<i64>,
    /// The signed headers, lowercase, in order. `["date"]` when the header omits the list.
    pub headers: Vec<String>,
    pub signature: Vec<u8>,
}

/// A signature that passed verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedSignature {
    pub key_id: String,
    pub headers: Vec<String>,
    pub created: Option<i64>,
    /// The `Date` header, Unix seconds, when it was covered.
    pub date: Option<i64>,
}

/// Optional pseudo-header values for [`sign_request`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SignOptions {
    /// Adds `created=` to the header (sign `(created)` to cover it).
    pub created: Option<i64>,
    /// Adds `expires=` to the header (sign `(expires)` to cover it).
    pub expires: Option<i64>,
}

fn malformed(reason: impl Into<String>) -> SignatureError {
    SignatureError::MalformedSignature {
        reason: reason.into(),
    }
}

fn request_target(method: &str, target_uri: &str) -> Result<String, SignatureError> {
    let parts = uri::parse(target_uri)?;
    let query = parts.query.map(|q| format!("?{q}")).unwrap_or_default();
    Ok(format!(
        "{} {}{query}",
        method.to_ascii_lowercase(),
        parts.path
    ))
}

/// The string to sign (draft-cavage-12 section 2.3).
pub fn signing_string<H: Headers + ?Sized>(
    method: &str,
    target_uri: &str,
    headers: &H,
    signed_headers: &[&str],
    options: &SignOptions,
) -> Result<String, SignatureError> {
    let mut lines = Vec::with_capacity(signed_headers.len());
    for name in signed_headers {
        let value = match *name {
            "(request-target)" => request_target(method, target_uri)?,
            "(created)" => options
                .created
                .ok_or(SignatureError::MissingCreated)?
                .to_string(),
            "(expires)" => options
                .expires
                .ok_or_else(|| SignatureError::InvalidValue {
                    what: "(expires)",
                    reason: "no expires value given".to_owned(),
                })?
                .to_string(),
            other if other.starts_with('(') => {
                return Err(SignatureError::UnsupportedComponent {
                    component: other.to_owned(),
                    reason: "unknown pseudo-header".to_owned(),
                });
            }
            other
                if other
                    .bytes()
                    .any(|b| b.is_ascii_uppercase() || b == b' ' || b == b':') =>
            {
                return Err(SignatureError::UnsupportedComponent {
                    component: other.to_owned(),
                    reason: "header names must be lowercase".to_owned(),
                });
            }
            other => required_header(headers, other)?,
        };
        lines.push(format!("{name}: {value}"));
    }
    Ok(lines.join("\n"))
}

/// Signs a request and returns the `Signature` header value (`algorithm="rsa-sha256"`).
///
/// `signed_headers` is typically [`GET_HEADERS`] or [`POST_HEADERS`]; `headers` must contain
/// every ordinary header named there (`host`, `date`, `digest`...).
pub fn sign_request<H: Headers + ?Sized>(
    method: &str,
    target_uri: &str,
    headers: &H,
    signed_headers: &[&str],
    key_id: &str,
    options: &SignOptions,
    key: &RsaSigningKey,
) -> Result<String, SignatureError> {
    let string = signing_string(method, target_uri, headers, signed_headers, options)?;
    let signature = key.sign_pkcs1v15_sha256(string.as_bytes())?;
    let quote = |what: &'static str, v: &str| -> Result<String, SignatureError> {
        if v.chars().any(|c| c == '"' || c == '\\' || c.is_control()) {
            Err(SignatureError::InvalidValue {
                what,
                reason: "contains a quote, backslash or control character".to_owned(),
            })
        } else {
            Ok(format!("\"{v}\""))
        }
    };
    let mut out = format!("keyId={},algorithm=\"rsa-sha256\"", quote("keyId", key_id)?);
    if let Some(created) = options.created {
        let _ = write!(out, ",created={created}");
    }
    if let Some(expires) = options.expires {
        let _ = write!(out, ",expires={expires}");
    }
    let _ = write!(
        out,
        ",headers={},signature=\"{}\"",
        quote("headers", &signed_headers.join(" "))?,
        STANDARD.encode(signature)
    );
    Ok(out)
}

/// Parses a `Signature` header value (also accepts the `Signature ` prefix of an
/// `Authorization` header).
pub fn parse_signature_header(value: &str) -> Result<SignatureHeader, SignatureError> {
    let value = value.trim();
    let value = match value.split_once(' ') {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("signature") => rest,
        _ => value,
    };
    let params = parse_params(value)?;
    let get = |name: &str| {
        params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    let int = |name: &str| -> Result<Option<i64>, SignatureError> {
        get(name)
            .map(|v| {
                v.parse::<i64>()
                    .map_err(|_| malformed(format!("`{name}` is not an integer")))
            })
            .transpose()
    };
    let key_id = get("keyid")
        .ok_or_else(|| malformed("no keyId"))?
        .to_owned();
    let signature_b64: String = get("signature")
        .ok_or_else(|| malformed("no signature"))?
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let signature = STANDARD
        .decode(signature_b64)
        .map_err(|e| malformed(format!("signature is not base64: {e}")))?;
    let headers = match get("headers") {
        Some(list) => list
            .split_ascii_whitespace()
            .map(str::to_ascii_lowercase)
            .collect(),
        None => vec!["date".to_owned()],
    };
    if headers.is_empty() {
        return Err(malformed("empty headers list"));
    }
    Ok(SignatureHeader {
        key_id,
        algorithm: get("algorithm").map(str::to_owned),
        created: int("created")?,
        expires: int("expires")?,
        headers,
        signature,
    })
}

/// Splits `k="v",k2=v2` into pairs. Quoted values may contain commas; `\"` and `\\` unescape.
fn parse_params(input: &str) -> Result<Vec<(String, String)>, SignatureError> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut chars = input.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace() || *c == ',').is_some() {}
        if chars.peek().is_none() {
            return Ok(out);
        }
        let mut key = String::new();
        while let Some(c) = chars.next_if(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-') {
            key.push(c);
        }
        if key.is_empty() || chars.next_if_eq(&'=').is_none() {
            return Err(malformed(format!("expected `name=` after `{key}`")));
        }
        if out.iter().any(|(k, _)| k.eq_ignore_ascii_case(&key)) {
            return Err(malformed(format!("`{key}` appears twice")));
        }
        let mut value = String::new();
        if chars.next_if_eq(&'"').is_some() {
            loop {
                match chars.next() {
                    Some('"') => break,
                    Some('\\') => match chars.next() {
                        Some(c @ ('"' | '\\')) => value.push(c),
                        _ => return Err(malformed("invalid escape in quoted value")),
                    },
                    Some(c) => value.push(c),
                    None => return Err(malformed("unterminated quoted value")),
                }
            }
        } else {
            while let Some(c) = chars.next_if(|c| *c != ',' && !c.is_whitespace()) {
                value.push(c);
            }
        }
        out.push((key, value));
    }
}

/// Verifies the `Signature` header of a request.
///
/// Checks that every header in `required_headers` is covered; that a time is covered (`date` or
/// `(created)`) and within `max_skew_secs` of `now`; that `expires`, if present, has not passed;
/// that `keyId` resolves to a key; and the RSA-SHA256 signature. If `digest` is covered, also
/// check the body with [`super::integrity::verify_legacy_digest`].
pub fn verify_request<H: Headers + ?Sized>(
    method: &str,
    target_uri: &str,
    headers: &H,
    required_headers: &[&str],
    key_lookup: impl Fn(&str) -> Option<RsaVerifyingKey>,
    now: i64,
    max_skew_secs: i64,
) -> Result<VerifiedSignature, SignatureError> {
    let parsed = parse_signature_header(&required_header(headers, "signature")?)?;
    match parsed.algorithm.as_deref() {
        None | Some("rsa-sha256" | "hs2019") => {}
        Some(other) => {
            return Err(SignatureError::UnsupportedAlgorithm {
                algorithm: other.to_owned(),
            });
        }
    }
    for required in required_headers {
        if !parsed.headers.iter().any(|h| h == required) {
            return Err(SignatureError::ComponentNotCovered {
                component: (*required).to_owned(),
            });
        }
    }
    let covers = |name: &str| parsed.headers.iter().any(|h| h == name);
    let mut created = None;
    let mut date = None;
    if covers("(created)") {
        let value = parsed.created.ok_or(SignatureError::MissingCreated)?;
        check_skew("created", value, now, max_skew_secs)?;
        created = Some(value);
    }
    if covers("date") {
        let value = parse_http_date(&required_header(headers, "date")?)?;
        check_skew("Date header", value, now, max_skew_secs)?;
        date = Some(value);
    }
    if created.is_none() && date.is_none() {
        return Err(SignatureError::ComponentNotCovered {
            component: "date or (created)".to_owned(),
        });
    }
    if let Some(expires) = parsed.expires
        && now > expires
    {
        return Err(SignatureError::Expired {
            seconds: now.saturating_sub(expires),
        });
    }
    let key = key_lookup(&parsed.key_id).ok_or_else(|| SignatureError::UnknownKey {
        keyid: parsed.key_id.clone(),
    })?;
    let signed: Vec<&str> = parsed.headers.iter().map(String::as_str).collect();
    let options = SignOptions {
        created: parsed.created,
        expires: parsed.expires,
    };
    let string = signing_string(method, target_uri, headers, &signed, &options)?;
    if !key.verify_pkcs1v15_sha256(string.as_bytes(), &parsed.signature) {
        return Err(SignatureError::BadSignature {
            keyid: parsed.key_id,
        });
    }
    Ok(VerifiedSignature {
        key_id: parsed.key_id,
        headers: parsed.headers,
        created,
        date,
    })
}
