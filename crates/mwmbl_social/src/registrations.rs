//! Registering with servers, listing registrations and revoking them: what the `register`,
//! `registrations` and `unregister` commands and the admin API ([`crate::admin_app`]) share.
//!
//! All of it writes only SQLite (and registering makes outbound HTTP), so it needs no writer
//! lock: the commands run beside a server, and the admin API runs inside one on the server's own
//! store connection. A registration's JSON never carries a key, only our key's fingerprint.

use apub::signatures::keys::Ed25519SigningKey;
use fasp::StoreAccess;
use fasp::registration::{RegistrationOutcome, register};
use serde_json::{Value, json};
use store::{Registration, RegistrationStatus, Store, StoreError};
use url::Url;

use crate::config::{Config, ConfigError};

/// Registering failed. Every variant names the URL or host.
#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
    /// The URL we were given is unusable; nothing was sent.
    #[error("{url:?} is not a usable server URL: {reason}")]
    BadUrl { url: String, reason: String },
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Registration(#[from] fasp::RegistrationError),
}

/// `text` as an `https` URL with a host (`http` too with `allow_http`) and no user info.
fn server_url(text: &str, allow_http: bool) -> Result<Url, RegisterError> {
    let bad = |reason: &str| RegisterError::BadUrl {
        url: text.to_owned(),
        reason: reason.to_owned(),
    };
    let url = Url::parse(text.trim()).map_err(|e| bad(&e.to_string()))?;
    let scheme_ok = url.scheme() == "https" || (allow_http && url.scheme() == "http");
    if !scheme_ok {
        return Err(bad(if allow_http {
            "must be an http(s) URL"
        } else {
            "must be an https URL"
        }));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(bad("has no host"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(bad("must not carry user info"));
    }
    Ok(url)
}

/// Registers with the server at `server_url` (its FASP base URL found through nodeinfo, unless
/// `fasp_base_url` is given) and returns what the admin needs to see.
pub async fn register_server<S: StoreAccess>(
    http: &reqwest::Client,
    store: &S,
    config: &Config,
    server: &str,
    fasp_base_url: Option<&str>,
    now: i64,
) -> Result<RegistrationOutcome, RegisterError> {
    let our_base_url = config.require_base_url()?;
    let our_base = Url::parse(our_base_url).map_err(|e| ConfigError::Invalid {
        field: "base_url",
        reason: format!("{our_base_url:?} is not a URL: {e}"),
    })?;
    let server = server_url(server, config.allow_http)?;
    let fasp_base = fasp_base_url
        .map(|url| server_url(url, config.allow_http))
        .transpose()?;
    let outcome = register(
        http,
        store,
        &config.name,
        &our_base,
        &server,
        fasp_base.as_ref(),
        now,
    )
    .await?;
    tracing::info!(
        registration = outcome.registration_id,
        host = server.host_str().unwrap_or_default(),
        fasp_base_url = %outcome.fasp_base_url,
        "registered, pending the admin's approval"
    );
    Ok(outcome)
}

/// Base64 of the SHA-256 of our public key for `registration`, as shown to the admin; `None`
/// if the stored seed is unusable.
fn fingerprint(registration: &Registration) -> Option<String> {
    Ed25519SigningKey::from_seed_slice(&registration.our_private_key)
        .ok()
        .map(|key| key.verifying_key().fingerprint())
}

/// One registration as JSON: `id`, `base_url` (the server's FASP base URL), `status`,
/// `capabilities` (enabled ones, as `id@version`), `fingerprint` and `created_at`.
pub fn registration_json(store: &Store, registration: &Registration) -> Result<Value, StoreError> {
    let capabilities: Vec<String> = store
        .enabled_capabilities(registration.id)?
        .into_iter()
        .map(|(id, version)| format!("{id}@{version}"))
        .collect();
    Ok(json!({
        "id": registration.id,
        "base_url": registration.base_url,
        "status": registration.status.as_str(),
        "capabilities": capabilities,
        "fingerprint": fingerprint(registration),
        "created_at": registration.created_at,
    }))
}

/// Every registration, as [`registration_json`].
pub fn list_registrations(store: &Store) -> Result<Value, StoreError> {
    store
        .list_registrations()?
        .iter()
        .map(|r| registration_json(store, r))
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}

/// The registration `id`, as [`registration_json`], if there is one.
pub fn get_registration(store: &Store, id: i64) -> Result<Option<Value>, StoreError> {
    store
        .registration(id)?
        .map(|r| registration_json(store, &r))
        .transpose()
}

/// What a successful registration reports: the registration as [`registration_json`] plus
/// `approval_url`, the server's `registrationCompletionUri`, where the admin compares the
/// fingerprint and approves. The server gives it only once, so it is not stored. When the new
/// registration replaced a revoked one for the same server, `replaces` is the old `id`.
pub fn registered_json(store: &Store, outcome: &RegistrationOutcome) -> Result<Value, StoreError> {
    let mut value = get_registration(store, outcome.registration_id)?.unwrap_or_else(|| {
        json!({
            "id": outcome.registration_id,
            "base_url": outcome.fasp_base_url.as_str().trim_end_matches('/'),
        })
    });
    if let Some(object) = value.as_object_mut() {
        object.insert("fingerprint".to_owned(), json!(outcome.fingerprint));
        object.insert("approval_url".to_owned(), json!(outcome.completion_uri));
        if let Some(old) = outcome.replaced_registration_id {
            object.insert("replaces".to_owned(), json!(old));
        }
    }
    Ok(value)
}

/// Marks registration `id` revoked, disables its capabilities and deletes its subscription and
/// backfill rows; `None` if there is no such registration. There is no remote call: the FASP
/// spec has no unregister endpoint, and a revoked registration's requests get 401 from then on.
pub fn revoke_registration(store: &Store, id: i64, now: i64) -> Result<Option<Value>, StoreError> {
    let Some(registration) = store.registration(id)? else {
        return Ok(None);
    };
    store.set_registration_status(id, RegistrationStatus::Revoked)?;
    for (capability, version) in store.enabled_capabilities(id)? {
        store.set_capability(id, &capability, &version, false, now)?;
    }
    let subscriptions = store.delete_subscriptions(id)?;
    let backfills = store.delete_backfills(id)?;
    tracing::info!(
        registration = id,
        host = registration.base_url,
        "registration revoked"
    );
    Ok(Some(json!({
        "id": id,
        "base_url": registration.base_url,
        "status": RegistrationStatus::Revoked.as_str(),
        "subscriptions_deleted": subscriptions,
        "backfills_deleted": backfills,
        "note": "the server was not contacted; remove the provider on its side too",
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_urls_are_checked_before_anything_is_sent() {
        let cases = [
            ("https://m.example", false, true),
            ("https://m.example:8443/", false, true),
            ("  https://m.example ", false, true),
            ("http://m.example", false, false),
            ("http://m.example", true, true),
            ("ftp://m.example", true, false),
            ("https://user:pw@m.example", false, false),
            ("m.example", false, false),
            ("not a url", false, false),
            ("", false, false),
        ];
        for (url, allow_http, ok) in cases {
            let result = server_url(url, allow_http);
            assert_eq!(result.is_ok(), ok, "{url:?}");
            if let Err(e) = result {
                assert!(e.to_string().contains(url), "{e} should name {url:?}");
            }
        }
    }

    #[test]
    fn json_carries_the_fingerprint_never_the_key() -> Result<(), Box<dyn std::error::Error>> {
        let store = Store::open_in_memory()?;
        let key = Ed25519SigningKey::from_seed(&[7; 32]);
        let registration = store.create_registration(store::NewRegistration {
            base_url: "https://m.example/api/fasp".into(),
            server_id: "sid".into(),
            our_private_key: key.to_seed().to_vec(),
            created_at: 5,
        })?;
        store.set_capability(registration.id, "account_search", "0.1", true, 6)?;
        let value = get_registration(&store, registration.id)?;
        assert_eq!(
            value,
            Some(json!({
                "id": registration.id,
                "base_url": "https://m.example/api/fasp",
                "status": "pending",
                "capabilities": ["account_search@0.1"],
                "fingerprint": key.verifying_key().fingerprint(),
                "created_at": 5,
            }))
        );
        assert_eq!(get_registration(&store, registration.id + 1)?, None);
        assert_eq!(
            list_registrations(&store)?,
            Value::Array(value.into_iter().collect())
        );
        Ok(())
    }
}
