//! A signed client for one registration's FASP API.
//!
//! Every request is signed with our key under the `faspId` the server gave us, carries a
//! `content-digest`, and its response must carry a valid `content-digest` and a signature by
//! the server's key. Calls into a capability's API go through a gate (CLAUDE.md principle 4):
//! [`FaspClient::data_sharing`] and every method of [`DataSharingClient`] check the store and
//! return [`ClientError::NotEnabled`] without sending anything if the capability is not
//! enabled for the registration.

use std::sync::Arc;
use std::time::Duration;

use apub::signatures::keys::{Ed25519SigningKey, Ed25519VerifyingKey, SigningKey, VerifyingKey};
use apub::signatures::rfc9421::{self, VerifyPolicy};
use apub::signatures::{FnHeaders, content_digest, parse_http_date};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use store::{Category, Registration, SubscriptionType};
use url::Url;

use crate::data_sharing::TrendsThreshold;
use crate::error::{ClientError, NotEnabled};
use crate::{DATA_SHARING_ID, DATA_SHARING_VERSION, StoreAccess, system_now};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SKEW_SECS: i64 = 300;
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(60);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The verified response to one request.
struct Verified {
    status: u16,
    body: Vec<u8>,
}

/// Client for the FASP API of one registered server.
pub struct FaspClient<S> {
    http: reqwest::Client,
    store: Arc<S>,
    registration_id: i64,
    base_url: Url,
    host: String,
    fasp_id: String,
    signing_key: SigningKey,
    their_key: VerifyingKey,
    clock: Clock,
}

impl<S> std::fmt::Debug for FaspClient<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaspClient")
            .field("registration_id", &self.registration_id)
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

impl<S: StoreAccess> FaspClient<S> {
    /// Builds a client from a stored registration, which must have its `faspId` and the
    /// server's public key (i.e. [`crate::registration::register`] completed).
    pub fn new(
        http: reqwest::Client,
        store: Arc<S>,
        registration: &Registration,
    ) -> Result<Self, ClientError> {
        let setup = |reason: String| ClientError::Setup {
            registration_id: registration.id,
            host: registration.base_url.clone(),
            reason,
        };
        let base_url = Url::parse(&registration.base_url)
            .map_err(|e| setup(format!("base URL is not a URL: {e}")))?;
        let host = base_url
            .host_str()
            .ok_or_else(|| setup("base URL has no host".to_owned()))?
            .to_owned();
        let fasp_id = registration
            .fasp_id
            .clone()
            .ok_or_else(|| setup("the server has not returned a faspId".to_owned()))?;
        let their_key = registration
            .their_public_key
            .as_deref()
            .ok_or_else(|| setup("the server's public key is not stored".to_owned()))?;
        let their_key = Ed25519VerifyingKey::from_bytes(their_key)
            .map_err(|e| setup(format!("stored server public key is invalid: {e}")))?;
        let signing_key = Ed25519SigningKey::from_seed_slice(&registration.our_private_key)
            .map_err(|e| setup(format!("stored private key is invalid: {e}")))?;
        Ok(Self {
            http,
            store,
            registration_id: registration.id,
            base_url,
            host,
            fasp_id,
            signing_key: SigningKey::Ed25519(signing_key),
            their_key: VerifyingKey::Ed25519(their_key),
            clock: Arc::new(system_now),
        })
    }

    /// Replaces the clock (Unix seconds), for tests.
    #[must_use]
    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    #[must_use]
    pub fn registration_id(&self) -> i64 {
        self.registration_id
    }

    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The `data_sharing` API, if the store says that capability is enabled right now.
    pub fn data_sharing(&self) -> Result<DataSharingClient<'_, S>, ClientError> {
        self.ensure_enabled(DATA_SHARING_ID, DATA_SHARING_VERSION)?;
        Ok(DataSharingClient { client: self })
    }

    fn ensure_enabled(&self, capability: &str, version: &str) -> Result<(), ClientError> {
        let enabled = self
            .store
            .with_store(|s| s.capability_enabled(self.registration_id, capability, version))?;
        if enabled {
            Ok(())
        } else {
            Err(NotEnabled {
                registration_id: self.registration_id,
                host: self.host.clone(),
                capability: capability.to_owned(),
            }
            .into())
        }
    }

    fn url_for(&self, segments: &[&str]) -> Result<Url, ClientError> {
        let mut url = self.base_url.clone();
        let ok = url
            .path_segments_mut()
            .map(|mut s| {
                s.pop_if_empty().extend(segments);
            })
            .is_ok();
        if ok {
            Ok(url)
        } else {
            Err(ClientError::Setup {
                registration_id: self.registration_id,
                host: self.host.clone(),
                reason: "base URL cannot have a path".to_owned(),
            })
        }
    }

    /// Signs and sends one request, then verifies the response. Private: capability APIs
    /// reach it only through their gated clients.
    async fn send(
        &self,
        method: Method,
        url: &Url,
        body: Option<&Value>,
    ) -> Result<Verified, ClientError> {
        let path = url.path().to_owned();
        let body_bytes = match body {
            Some(value) => serde_json::to_vec(value).unwrap_or_default(),
            None => Vec::new(),
        };
        let digest = content_digest(&body_bytes);
        let now = (self.clock)();
        let (signature_input, signature) = rfc9421::sign_request(
            method.as_str(),
            url.as_str(),
            &[("content-digest", digest.as_str())],
            &["@method", "@target-uri", "content-digest"],
            &self.fasp_id,
            now,
            &self.signing_key,
        )
        .map_err(|source| ClientError::Signing {
            host: self.host.clone(),
            path: path.clone(),
            source,
        })?;
        let response = self
            .http
            .request(method, url.clone())
            .header("content-digest", digest)
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .header("signature-input", signature_input)
            .header("signature", signature)
            .body(body_bytes)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|source| ClientError::Http {
                host: self.host.clone(),
                path: path.clone(),
                source,
            })?;
        self.verify_response(&path, response).await
    }

    async fn verify_response(
        &self,
        path: &str,
        mut response: reqwest::Response,
    ) -> Result<Verified, ClientError> {
        let status = response.status().as_u16();
        let host_path = |reason: String| ClientError::BadResponse {
            host: self.host.clone(),
            path: path.to_owned(),
            reason,
        };
        if status == 429 {
            // Taken before verification: a limiting proxy may not sign. The delay is capped.
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| parse_retry_after(v, (self.clock)()))
                .unwrap_or(DEFAULT_RETRY_AFTER)
                .min(MAX_RETRY_AFTER);
            return Err(ClientError::RateLimited {
                host: self.host.clone(),
                path: path.to_owned(),
                retry_after,
            });
        }
        let headers = response.headers().clone();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|source| ClientError::Http {
            host: self.host.clone(),
            path: path.to_owned(),
            source,
        })? {
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(host_path(format!(
                    "response is larger than {MAX_RESPONSE_BYTES} bytes"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        let lookup = FnHeaders(|name: &str| {
            headers
                .get_all(name)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        });
        let failed = |source| ClientError::ResponseVerification {
            host: self.host.clone(),
            path: path.to_owned(),
            status,
            source,
        };
        let digest = apub::signatures::integrity::verify_content_digest_header(&lookup, &body);
        digest.map_err(failed)?;
        // The server's key is the only key we hold for this registration, so the signature's
        // `keyid` is not used to choose a key, and may be absent: Mastodon 4.7.5 sends none.
        let policy = VerifyPolicy::new(
            &["@status", "content-digest"],
            (self.clock)(),
            MAX_SKEW_SECS,
        );
        rfc9421::verify_response_with_key(status, &lookup, &policy, &self.their_key)
            .map_err(failed)?;
        Ok(Verified { status, body })
    }

    fn status_error(&self, url: &Url, status: u16) -> ClientError {
        ClientError::Status {
            host: self.host.clone(),
            path: url.path().to_owned(),
            status,
        }
    }

    fn bad_response(&self, url: &Url, reason: String) -> ClientError {
        ClientError::BadResponse {
            host: self.host.clone(),
            path: url.path().to_owned(),
            reason,
        }
    }
}

/// `Retry-After` as delay seconds or an HTTP date.
fn parse_retry_after(value: &str, now: i64) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = parse_http_date(value).ok()?;
    Some(Duration::from_secs(
        u64::try_from(at.saturating_sub(now)).unwrap_or(0),
    ))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum IdValue {
    Text(String),
    Int(i64),
}

#[derive(Deserialize)]
struct Created {
    id: IdValue,
}

#[derive(Deserialize)]
struct SubscriptionReply {
    subscription: Created,
}

#[derive(Deserialize)]
struct BackfillReply {
    #[serde(rename = "backfillRequest")]
    backfill_request: Created,
}

fn id_string(id: IdValue) -> String {
    match id {
        IdValue::Text(s) => s,
        IdValue::Int(n) => n.to_string(),
    }
}

/// The `data_sharing` API of a server. Every method re-checks that the capability is enabled.
pub struct DataSharingClient<'a, S> {
    client: &'a FaspClient<S>,
}

impl<S> std::fmt::Debug for DataSharingClient<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataSharingClient")
            .field("registration_id", &self.client.registration_id)
            .finish()
    }
}

impl<S: StoreAccess> DataSharingClient<'_, S> {
    fn gate(&self) -> Result<(), ClientError> {
        self.client
            .ensure_enabled(DATA_SHARING_ID, DATA_SHARING_VERSION)
    }

    async fn create(&self, segments: &[&str], body: &Value) -> Result<(Url, Vec<u8>), ClientError> {
        self.gate()?;
        let url = self.client.url_for(segments)?;
        let reply = self.client.send(Method::POST, &url, Some(body)).await?;
        if reply.status != 201 {
            return Err(self.client.status_error(&url, reply.status));
        }
        Ok((url, reply.body))
    }

    /// `POST /data_sharing/v0/event_subscriptions`; returns the server's id for it.
    /// `threshold` is sent only for a `trends` subscription.
    pub async fn subscribe(
        &self,
        category: Category,
        subscription_type: SubscriptionType,
        max_batch_size: Option<u32>,
        threshold: Option<&TrendsThreshold>,
    ) -> Result<String, ClientError> {
        let mut body = Map::new();
        body.insert("category".into(), json!(category.as_str()));
        body.insert("subscriptionType".into(), json!(subscription_type.as_str()));
        if let Some(size) = max_batch_size {
            body.insert("maxBatchSize".into(), json!(size));
        }
        if let Some(t) = threshold.filter(|_| subscription_type == SubscriptionType::Trends) {
            body.insert(
                "threshold".into(),
                json!({
                    "timeframe": t.timeframe_minutes,
                    "shares": t.shares,
                    "likes": t.likes,
                    "replies": t.replies,
                }),
            );
        }
        let (url, bytes) = self
            .create(
                &["data_sharing", "v0", "event_subscriptions"],
                &Value::Object(body),
            )
            .await?;
        let reply: SubscriptionReply = serde_json::from_slice(&bytes).map_err(|e| {
            self.client
                .bad_response(&url, format!("no subscription id: {e}"))
        })?;
        Ok(id_string(reply.subscription.id))
    }

    /// `DELETE /data_sharing/v0/event_subscriptions/{id}`.
    pub async fn unsubscribe(&self, remote_id: &str) -> Result<(), ClientError> {
        self.gate()?;
        let url = self
            .client
            .url_for(&["data_sharing", "v0", "event_subscriptions", remote_id])?;
        let reply = self.client.send(Method::DELETE, &url, None).await?;
        if reply.status == 204 {
            Ok(())
        } else {
            Err(self.client.status_error(&url, reply.status))
        }
    }

    /// `POST /data_sharing/v0/backfill_requests`; returns the server's id for it.
    pub async fn request_backfill(
        &self,
        category: Category,
        max_count: u32,
    ) -> Result<String, ClientError> {
        let body = json!({"category": category.as_str(), "maxCount": max_count});
        let (url, bytes) = self
            .create(&["data_sharing", "v0", "backfill_requests"], &body)
            .await?;
        let reply: BackfillReply = serde_json::from_slice(&bytes).map_err(|e| {
            self.client
                .bad_response(&url, format!("no backfill request id: {e}"))
        })?;
        Ok(id_string(reply.backfill_request.id))
    }

    /// `POST /data_sharing/v0/backfill_requests/{id}/continuation`: `true` for 204 (more is
    /// coming), `false` for 404 (nothing more, or an id the server does not know).
    pub async fn continue_backfill(&self, remote_id: &str) -> Result<bool, ClientError> {
        self.gate()?;
        let url = self.client.url_for(&[
            "data_sharing",
            "v0",
            "backfill_requests",
            remote_id,
            "continuation",
        ])?;
        let reply = self.client.send(Method::POST, &url, None).await?;
        match reply.status {
            204 => Ok(true),
            404 => Ok(false),
            other => Err(self.client.status_error(&url, other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_forms() {
        assert_eq!(parse_retry_after("120", 0), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after(" 7 ", 0), Some(Duration::from_secs(7)));
        assert_eq!(
            parse_retry_after("Thu, 01 Jan 1970 00:10:00 GMT", 0),
            Some(Duration::from_secs(600))
        );
        assert_eq!(
            parse_retry_after("Thu, 01 Jan 1970 00:00:00 GMT", 100),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("soon", 0), None);
    }
}
