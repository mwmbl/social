//! Small shared types.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Identifier of the `data_sharing` capability.
pub const DATA_SHARING_ID: &str = "data_sharing";
/// The version of `data_sharing` we advertise and call. Mastodon activates it using only the
/// major part (`0`); both spellings are accepted and stored under this one.
pub const DATA_SHARING_VERSION: &str = "0.1";

/// Identifier of the `account_search` capability.
pub const ACCOUNT_SEARCH_ID: &str = "account_search";
/// The version of `account_search` we advertise. Mastodon activates it using only the major
/// part (`0`); both spellings are accepted and stored under this one.
pub const ACCOUNT_SEARCH_VERSION: &str = "0.1";

/// A capability we advertise in `provider_info`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    pub id: String,
    pub version: String,
}

impl Capability {
    #[must_use]
    pub fn new(id: &str, version: &str) -> Self {
        Self {
            id: id.to_owned(),
            version: version.to_owned(),
        }
    }

    /// Whether `version` names this capability's version: the full `0.1` or just the major `0`.
    #[must_use]
    pub fn matches_version(&self, version: &str) -> bool {
        self.version == version || self.version.split('.').next() == Some(version)
    }
}

/// One privacy policy entry of `provider_info`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivacyPolicy {
    pub url: String,
    pub language: String,
}

/// The configurable parts of `GET /provider_info`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderInfoConfig {
    pub name: String,
    pub privacy_policy: Vec<PrivacyPolicy>,
    pub contact_email: Option<String>,
    pub fediverse_account: Option<String>,
}

/// The current time as Unix seconds. The default clock of the client and the router.
#[must_use]
pub fn system_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}
