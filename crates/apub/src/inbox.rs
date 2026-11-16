//! Checking a `POST` to our inbox: who signed it, whether the signature and body digest hold,
//! and whether the key belongs to the activity's actor.
//!
//! Framework-agnostic, like [`crate::signatures`]. The caller fetches the key named by
//! [`signer`] (a signed `GET` of the key id), finds it in the fetched document with
//! [`find_public_key`], checks it with [`check_key_owner`] and then calls [`verify_post`].
//!
//! - RFC 9421: the signature must cover `@method`, the target (`@target-uri`, or `@authority`
//!   and `@path`) and `content-digest`, which must match the body.
//! - draft-cavage-12: it must cover `(request-target)`, `host`, `digest` and a time (`date` or
//!   `(created)`), and `Digest` must match the body.
//!
//! Either way the signature time must be within the allowed skew of now.

use serde_json::Value;
use thiserror::Error;
use url::Url;

use crate::fetch::SignatureScheme;
use crate::signatures::integrity::{verify_content_digest_header, verify_legacy_digest};
use crate::signatures::keys::{RsaVerifyingKey, VerifyingKey};
use crate::signatures::rfc9421::{self, VerifyPolicy};
use crate::signatures::{Headers, SignatureError, cavage};

/// How far a signature's time may be from ours, either way: an hour, as Mastodon allows.
pub const MAX_SKEW_SECS: i64 = 3600;

/// The scheme and key id of a request's signature, read without verifying it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signer {
    pub scheme: SignatureScheme,
    pub key_id: String,
    /// The RFC 9421 label checked (the first in `Signature-Input`); `None` for cavage.
    label: Option<String>,
    /// The RFC 9421 `alg` parameter, if any.
    alg: Option<String>,
}

fn header<H: Headers + ?Sized>(headers: &H, name: &str) -> Option<String> {
    headers.get_all(name).into_iter().next()
}

/// Who signed the request: RFC 9421 if it has `Signature-Input`, else cavage-12 `Signature`.
pub fn signer<H: Headers + ?Sized>(headers: &H) -> Result<Signer, SignatureError> {
    if let Some(input) = header(headers, "signature-input") {
        let inputs = rfc9421::parse_signature_input(&input)?;
        let Some((label, _, params)) = inputs.into_iter().next() else {
            return Err(SignatureError::MalformedSignatureInput {
                reason: "no signatures".to_owned(),
            });
        };
        let key_id = params.keyid.ok_or(SignatureError::MissingKeyId)?;
        return Ok(Signer {
            scheme: SignatureScheme::Rfc9421,
            key_id,
            label: Some(label),
            alg: params.alg,
        });
    }
    let value = header(headers, "signature").ok_or_else(|| SignatureError::MissingHeader {
        name: "signature".to_owned(),
    })?;
    let parsed = cavage::parse_signature_header(&value)?;
    Ok(Signer {
        scheme: SignatureScheme::Cavage,
        key_id: parsed.key_id,
        label: None,
        alg: None,
    })
}

/// Verifies a `POST` to `target_uri` signed by `signer` with `key`, and its body digest.
pub fn verify_post<H: Headers + ?Sized>(
    signer: &Signer,
    target_uri: &str,
    headers: &H,
    body: &[u8],
    key: &RsaVerifyingKey,
    now: i64,
) -> Result<(), SignatureError> {
    match signer.scheme {
        SignatureScheme::Rfc9421 => {
            let label = signer.label.as_deref().unwrap_or(rfc9421::DEFAULT_LABEL);
            let key = match signer.alg.as_deref() {
                Some("rsa-pss-sha512") => VerifyingKey::RsaPssSha512(key.clone()),
                _ => VerifyingKey::RsaV15Sha256(key.clone()),
            };
            let policy = VerifyPolicy {
                label,
                required_components: &["@method", "content-digest"],
                now,
                max_skew_secs: MAX_SKEW_SECS,
            };
            let verified = rfc9421::verify_request("POST", target_uri, headers, &policy, |id| {
                (id == signer.key_id).then(|| key.clone())
            })?;
            let covers = |c: &str| verified.components.iter().any(|v| v == c);
            let target_covered = covers("@target-uri") || (covers("@authority") && covers("@path"));
            if !target_covered {
                return Err(SignatureError::ComponentNotCovered {
                    component: "@target-uri".to_owned(),
                });
            }
            verify_content_digest_header(headers, body)
        }
        SignatureScheme::Cavage => {
            cavage::verify_request(
                "POST",
                target_uri,
                headers,
                &["(request-target)", "host", "digest"],
                |id| (id == signer.key_id).then(|| key.clone()),
                now,
                MAX_SKEW_SECS,
            )?;
            let digest =
                header(headers, "digest").ok_or_else(|| SignatureError::MissingHeader {
                    name: "digest".to_owned(),
                })?;
            verify_legacy_digest(&digest, body)
        }
    }
}

/// A public key found in a fetched document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKeyInfo {
    pub id: String,
    /// The actor the key belongs to.
    pub owner: String,
    pub pem: String,
}

fn key_entry(entry: &Value, key_id: &str, default_owner: Option<&str>) -> Option<PublicKeyInfo> {
    if entry.get("id").and_then(Value::as_str) != Some(key_id) {
        return None;
    }
    let pem = entry.get("publicKeyPem").and_then(Value::as_str)?;
    let owner = entry
        .get("owner")
        .and_then(Value::as_str)
        .or(default_owner)?;
    Some(PublicKeyInfo {
        id: key_id.to_owned(),
        owner: owner.to_owned(),
        pem: pem.to_owned(),
    })
}

/// The key `key_id` in a document fetched from it: an actor whose `publicKey` (an object or an
/// array) has that id, the key's owner defaulting to the actor; or a key document itself, which
/// must name its `owner`.
#[must_use]
pub fn find_public_key(document: &Value, key_id: &str) -> Option<PublicKeyInfo> {
    let actor_id = document.get("id").and_then(Value::as_str);
    match document.get("publicKey") {
        Some(Value::Array(entries)) => entries
            .iter()
            .find_map(|entry| key_entry(entry, key_id, actor_id)),
        Some(entry @ Value::Object(_)) => key_entry(entry, key_id, actor_id),
        _ => key_entry(document, key_id, None),
    }
}

/// Why a key may not sign for an activity.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OwnerError {
    #[error("key `{key_id}` belongs to `{owner}`, not to the activity's actor `{actor}`")]
    NotTheActor {
        key_id: String,
        owner: String,
        actor: String,
    },
    #[error("key `{key_id}` claims owner `{owner}` on another origin")]
    OtherOrigin { key_id: String, owner: String },
}

fn origin(uri: &str) -> Option<(String, String, Option<u16>)> {
    let url = Url::parse(uri).ok()?;
    Some((
        url.scheme().to_owned(),
        url.host_str()?.to_ascii_lowercase(),
        url.port_or_known_default(),
    ))
}

/// Whether `a` and `b` are absolute URIs on the same scheme, host and port.
#[must_use]
pub fn same_origin(a: &str, b: &str) -> bool {
    match (origin(a), origin(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// The key must belong to `actor`, and its owner must be on the key's own origin: a host can
/// only vouch for keys of its own actors.
pub fn check_key_owner(key: &PublicKeyInfo, actor: &str) -> Result<(), OwnerError> {
    if !same_origin(&key.id, &key.owner) {
        return Err(OwnerError::OtherOrigin {
            key_id: key.id.clone(),
            owner: key.owner.clone(),
        });
    }
    if key.owner != actor {
        return Err(OwnerError::NotTheActor {
            key_id: key.id.clone(),
            owner: key.owner.clone(),
            actor: actor.to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const ALICE: &str = "https://a.example/users/alice";
    const KEY: &str = "https://a.example/users/alice#main-key";

    #[test]
    fn keys_in_actors_and_key_documents() {
        let actor =
            json!({"id": ALICE, "publicKey": {"id": KEY, "owner": ALICE, "publicKeyPem": "P"}});
        let found = find_public_key(&actor, KEY).unwrap();
        assert_eq!(
            found,
            PublicKeyInfo {
                id: KEY.into(),
                owner: ALICE.into(),
                pem: "P".into()
            }
        );
        // No owner: the actor carrying it.
        let actor = json!({"id": ALICE, "publicKey": {"id": KEY, "publicKeyPem": "P"}});
        assert_eq!(find_public_key(&actor, KEY).unwrap().owner, ALICE);
        // An array of keys: the one with the id.
        let other = "https://a.example/users/alice#other";
        let actor = json!({"id": ALICE, "publicKey": [
            {"id": other, "publicKeyPem": "O"},
            {"id": KEY, "publicKeyPem": "P"},
        ]});
        assert_eq!(find_public_key(&actor, KEY).unwrap().pem, "P");
        assert_eq!(find_public_key(&actor, other).unwrap().pem, "O");
        // A key document on its own needs an owner.
        let doc = json!({"id": KEY, "owner": ALICE, "publicKeyPem": "P"});
        assert_eq!(find_public_key(&doc, KEY).unwrap().owner, ALICE);
        let doc = json!({"id": KEY, "publicKeyPem": "P"});
        assert_eq!(find_public_key(&doc, KEY), None);
        // A different key id, or no PEM, finds nothing.
        let actor = json!({"id": ALICE, "publicKey": {"id": KEY, "publicKeyPem": "P"}});
        assert_eq!(find_public_key(&actor, other), None);
        let actor = json!({"id": ALICE, "publicKey": {"id": KEY}});
        assert_eq!(find_public_key(&actor, KEY), None);
    }

    #[test]
    fn owners() {
        let key = |owner: &str| PublicKeyInfo {
            id: KEY.into(),
            owner: owner.into(),
            pem: String::new(),
        };
        assert!(check_key_owner(&key(ALICE), ALICE).is_ok());
        assert!(matches!(
            check_key_owner(&key(ALICE), "https://a.example/users/bob"),
            Err(OwnerError::NotTheActor { .. })
        ));
        // b.example cannot serve a key that claims to be alice's on a.example.
        let foreign = PublicKeyInfo {
            id: "https://b.example/k".into(),
            owner: ALICE.into(),
            pem: String::new(),
        };
        assert!(matches!(
            check_key_owner(&foreign, ALICE),
            Err(OwnerError::OtherOrigin { .. })
        ));
        assert!(same_origin(
            "https://A.example/x",
            "https://a.example:443/y"
        ));
        assert!(!same_origin("https://a.example/x", "http://a.example/x"));
        assert!(!same_origin("nope", "nope"));
    }

    #[test]
    fn signer_scheme_and_key_id() {
        let rfc = [
            (
                "signature-input",
                "sig1=(\"@method\");created=1;keyid=\"k1\"",
            ),
            ("signature", "sig1=:AAAA:"),
        ];
        let s = signer(&rfc).unwrap();
        assert_eq!(s.scheme, SignatureScheme::Rfc9421);
        assert_eq!(s.key_id, "k1");
        let cav = [(
            "signature",
            "keyId=\"k2\",algorithm=\"rsa-sha256\",headers=\"date\",signature=\"AAAA\"",
        )];
        let s = signer(&cav).unwrap();
        assert_eq!(s.scheme, SignatureScheme::Cavage);
        assert_eq!(s.key_id, "k2");
        let none: [(&str, &str); 0] = [];
        assert!(signer(&none).is_err());
        let no_keyid = [("signature-input", "sig1=(\"@method\");created=1")];
        assert_eq!(signer(&no_keyid), Err(SignatureError::MissingKeyId));
    }
}
