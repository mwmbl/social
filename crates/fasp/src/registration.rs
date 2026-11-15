//! Registering with a fediverse server: base URL discovery and the `/registration` call.

use std::time::Duration;

use apub::signatures::keys::{Ed25519SigningKey, Ed25519VerifyingKey};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde_json::{Value, json};
use store::NewRegistration;
use url::Url;

use crate::StoreAccess;
use crate::error::RegistrationError;

const NODEINFO_REL_PREFIX: &str = "http://nodeinfo.diaspora.software/ns/schema/";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Random bytes in a generated `serverId`: 18 bytes are 24 url-safe characters.
const SERVER_ID_BYTES: usize = 18;

/// What a successful [`register`] gives the admin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationOutcome {
    pub registration_id: i64,
    /// Base64 of the SHA-256 of our public key, shown to the admin to compare on the server.
    pub fingerprint: String,
    /// Where the admin approves the registration on the server.
    pub completion_uri: String,
    /// The FASP base URL that was used (and is stored as the registration's `base_url`).
    pub fasp_base_url: Url,
}

fn invalid_url(url: &Url, reason: &str) -> RegistrationError {
    RegistrationError::InvalidUrl {
        url: url.to_string(),
        reason: reason.to_owned(),
    }
}

async fn read_limited(
    url: &Url,
    mut response: reqwest::Response,
) -> Result<Vec<u8>, RegistrationError> {
    let http_err = |source| RegistrationError::Http {
        url: url.to_string(),
        source,
    };
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(http_err)? {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(RegistrationError::BadResponse {
                url: url.to_string(),
                reason: format!("response is larger than {MAX_RESPONSE_BYTES} bytes"),
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn get_json(http: &reqwest::Client, url: &Url) -> Result<Value, RegistrationError> {
    let response = http
        .get(url.clone())
        .header("accept", "application/json")
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|source| RegistrationError::Http {
            url: url.to_string(),
            source,
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(RegistrationError::Status {
            url: url.to_string(),
            status: status.as_u16(),
        });
    }
    let body = read_limited(url, response).await?;
    serde_json::from_slice(&body).map_err(|e| RegistrationError::BadResponse {
        url: url.to_string(),
        reason: format!("not JSON: {e}"),
    })
}

/// `2.1` becomes `(2, 1)`; anything that is not `major.minor` is `None`.
fn nodeinfo_version(rel: &str) -> Option<(u32, u32)> {
    let version = rel.strip_prefix(NODEINFO_REL_PREFIX)?;
    let (major, minor) = version.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

fn origin_of(url: &Url) -> Result<String, RegistrationError> {
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(invalid_url(url, "not an http(s) URL with a host"));
    }
    Ok(url.origin().ascii_serialization())
}

/// Finds the FASP base URL of `server_url`.
///
/// With `override_url` that is returned as is. Otherwise: read `/.well-known/nodeinfo`, follow
/// the highest-version 2.x link (it must be on the server's own origin), and use
/// `metadata.faspBaseUrl`. Mastodon 4.7.5 does not publish that, so for `software.name ==
/// "mastodon"` the base URL is `<server origin>/api/fasp`. Anything else is an error.
pub async fn discover_fasp_base_url(
    http: &reqwest::Client,
    server_url: &Url,
    override_url: Option<&Url>,
) -> Result<Url, RegistrationError> {
    if let Some(url) = override_url {
        origin_of(url)?;
        return Ok(url.clone());
    }
    let origin = origin_of(server_url)?;
    let well_known = Url::parse(&format!("{origin}/.well-known/nodeinfo"))
        .map_err(|e| invalid_url(server_url, &e.to_string()))?;
    let index = get_json(http, &well_known).await?;
    let bad = |reason: &str| RegistrationError::BadResponse {
        url: well_known.to_string(),
        reason: reason.to_owned(),
    };
    let links = index
        .get("links")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("no `links` array"))?;
    let href = links
        .iter()
        .filter_map(|link| {
            let version = nodeinfo_version(link.get("rel")?.as_str()?)?;
            (version.0 == 2).then_some((version, link.get("href")?.as_str()?))
        })
        .max_by_key(|(version, _)| *version)
        .map(|(_, href)| href)
        .ok_or_else(|| bad("no nodeinfo 2.x link"))?;
    let nodeinfo_url = well_known
        .join(href)
        .map_err(|e| bad(&format!("bad nodeinfo link `{href}`: {e}")))?;
    if nodeinfo_url.origin() != server_url.origin() {
        return Err(bad(&format!(
            "nodeinfo link {nodeinfo_url} is not on the server's own origin"
        )));
    }
    let nodeinfo = get_json(http, &nodeinfo_url).await?;
    if let Some(configured) = nodeinfo
        .pointer("/metadata/faspBaseUrl")
        .and_then(Value::as_str)
    {
        let url = Url::parse(configured).map_err(|e| RegistrationError::BadResponse {
            url: nodeinfo_url.to_string(),
            reason: format!("faspBaseUrl `{configured}` is not a URL: {e}"),
        })?;
        origin_of(&url)?;
        return Ok(url);
    }
    let software = nodeinfo
        .pointer("/software/name")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if software.eq_ignore_ascii_case("mastodon") {
        return Url::parse(&format!("{origin}/api/fasp"))
            .map_err(|e| invalid_url(server_url, &e.to_string()));
    }
    Err(RegistrationError::NoFaspBaseUrl {
        server: origin,
        software: software.to_owned(),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegistrationReply {
    fasp_id: String,
    public_key: String,
    registration_completion_uri: String,
}

fn random_server_id() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; SERVER_ID_BYTES];
    getrandom::fill(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Registers us with the server at `server_url`.
///
/// Generates an Ed25519 key and a random `serverId`, records a `pending` registration, sends
/// the unsigned `POST <faspBaseUrl>/registration` and stores the returned `faspId` and the
/// server's public key. If the POST fails the pending row is removed again. The registration
/// becomes `active` when the server first makes a request to us that verifies (the spec gives
/// us no explicit approval callback).
///
/// The stored `base_url` is the FASP base URL (not the server's own URL): it is what
/// [`crate::client::FaspClient`] needs.
pub async fn register<S: StoreAccess>(
    http: &reqwest::Client,
    store: &S,
    our_name: &str,
    our_base_url: &Url,
    server_url: &Url,
    override_url: Option<&Url>,
    now: i64,
) -> Result<RegistrationOutcome, RegistrationError> {
    let fasp_base_url = discover_fasp_base_url(http, server_url, override_url).await?;
    let base_url = fasp_base_url.as_str().trim_end_matches('/').to_owned();
    let key_error = |source| RegistrationError::Key {
        base_url: base_url.clone(),
        source,
    };
    let existing = store
        .with_store(store::Store::list_registrations)?
        .into_iter()
        .find(|r| r.base_url == base_url);
    if let Some(existing) = existing {
        return Err(RegistrationError::AlreadyRegistered {
            base_url,
            registration_id: existing.id,
        });
    }

    let key = Ed25519SigningKey::generate().map_err(key_error)?;
    let public_key = key.verifying_key();
    let server_id = random_server_id().map_err(|e| {
        key_error(apub::signatures::keys::KeyError::Generate {
            algorithm: "serverId",
            reason: e.to_string(),
        })
    })?;
    let registration = store.with_store(|s| {
        s.create_registration(NewRegistration {
            base_url: base_url.clone(),
            server_id: server_id.clone(),
            our_private_key: key.to_seed().to_vec(),
            created_at: now,
        })
    })?;

    let exchange = Exchange {
        registration_id: registration.id,
        base_url: &base_url,
        our_name,
        our_base_url,
        server_id: &server_id,
        public_key: &public_key,
    };
    let result = exchange.run(http, store).await;
    match result {
        Ok(completion_uri) => Ok(RegistrationOutcome {
            registration_id: registration.id,
            fingerprint: public_key.fingerprint(),
            completion_uri,
            fasp_base_url,
        }),
        Err(e) => {
            if let Err(cleanup) = store.with_store(|s| s.delete_registration(registration.id)) {
                tracing::warn!(registration = registration.id, error = %cleanup,
                    "failed to remove the pending registration after a failed attempt");
            }
            Err(e)
        }
    }
}

/// The fixed inputs of the registration POST.
struct Exchange<'a> {
    registration_id: i64,
    base_url: &'a str,
    our_name: &'a str,
    our_base_url: &'a Url,
    server_id: &'a str,
    public_key: &'a Ed25519VerifyingKey,
}

impl Exchange<'_> {
    /// Sends the registration and stores the reply; returns the completion URI.
    async fn run<S: StoreAccess>(
        &self,
        http: &reqwest::Client,
        store: &S,
    ) -> Result<String, RegistrationError> {
        let url = Url::parse(&format!("{}/registration", self.base_url))
            .map_err(|e| error_url(self.base_url, &e.to_string()))?;
        let payload = json!({
            "name": self.our_name,
            "baseUrl": self.our_base_url.as_str().trim_end_matches('/'),
            "serverId": self.server_id,
            "publicKey": self.public_key.to_base64(),
        });
        let response = http
            .post(url.clone())
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .body(payload.to_string())
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|source| RegistrationError::Http {
                url: url.to_string(),
                source,
            })?;
        let status = response.status();
        if status.as_u16() != 201 {
            return Err(RegistrationError::Status {
                url: url.to_string(),
                status: status.as_u16(),
            });
        }
        let body = read_limited(&url, response).await?;
        let bad = |reason: String| RegistrationError::BadResponse {
            url: url.to_string(),
            reason,
        };
        let reply: RegistrationReply = serde_json::from_slice(&body)
            .map_err(|e| bad(format!("not a registration reply: {e}")))?;
        let their_key = Ed25519VerifyingKey::from_base64(&reply.public_key)
            .map_err(|e| bad(format!("bad public key: {e}")))?;
        if reply.fasp_id.is_empty() {
            return Err(bad("empty faspId".to_owned()));
        }
        store.with_store(|s| {
            s.set_registration_remote(self.registration_id, &reply.fasp_id, &their_key.to_bytes())
        })?;
        tracing::info!(registration = self.registration_id, url = %url, "registered with FASP host");
        Ok(reply.registration_completion_uri)
    }
}

fn error_url(base_url: &str, reason: &str) -> RegistrationError {
    RegistrationError::InvalidUrl {
        url: base_url.to_owned(),
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodeinfo_versions() {
        let rel = |v: &str| format!("{NODEINFO_REL_PREFIX}{v}");
        assert_eq!(nodeinfo_version(&rel("2.1")), Some((2, 1)));
        assert_eq!(nodeinfo_version(&rel("2.0")), Some((2, 0)));
        assert_eq!(nodeinfo_version(&rel("1.0")), Some((1, 0)));
        assert_eq!(nodeinfo_version(&rel("x")), None);
        assert_eq!(nodeinfo_version("http://other/2.1"), None);
    }

    #[test]
    fn server_ids_are_long_url_safe_and_distinct() {
        let a = random_server_id().unwrap();
        let b = random_server_id().unwrap();
        assert!(a.len() >= 16);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_ne!(a, b);
    }
}
