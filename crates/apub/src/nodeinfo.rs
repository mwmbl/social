//! Reading other servers' nodeinfo: which software a host runs.
//!
//! [`nodeinfo_href`] and [`software_name`] are pure, and shared with FASP registration's base URL
//! discovery. [`lookup_software`] fetches through the [`Fetcher`], so it is held to the same
//! per-host limits, backoff, same-host redirects and body cap as every other fetch.

use serde_json::Value;
use thiserror::Error;
use url::Url;

use crate::fetch::{FetchError, FetchOutcome, Fetcher};

/// The `rel` of a nodeinfo link, before the schema version.
pub const NODEINFO_REL_PREFIX: &str = "http://nodeinfo.diaspora.software/ns/schema/";

/// Why a software lookup failed. Messages carry the URI, never a body.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NodeinfoError {
    #[error("`{uri}` has no origin to look nodeinfo up on")]
    BadUri { uri: String },
    #[error("fetching nodeinfo `{uri}` failed: {source}")]
    Fetch {
        uri: String,
        #[source]
        source: FetchError,
    },
    #[error("nodeinfo `{uri}` was not available: {reason}")]
    Unavailable { uri: String, reason: String },
    #[error("`{uri}` has no nodeinfo 2.x link")]
    NoLink { uri: String },
    #[error("nodeinfo link `{href}` in `{uri}` is not on the same origin")]
    OffOrigin { uri: String, href: String },
    #[error("nodeinfo `{uri}` has no `software.name`")]
    NoSoftware { uri: String },
}

/// `2.1` becomes `(2, 1)`; anything that is not `major.minor` after the prefix is `None`.
#[must_use]
pub fn nodeinfo_version(rel: &str) -> Option<(u32, u32)> {
    let version = rel.strip_prefix(NODEINFO_REL_PREFIX)?;
    let (major, minor) = version.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// The `href` of the highest-version nodeinfo 2.x link in a `/.well-known/nodeinfo` document.
#[must_use]
pub fn nodeinfo_href(index: &Value) -> Option<&str> {
    index
        .get("links")?
        .as_array()?
        .iter()
        .filter_map(|link| {
            let version = nodeinfo_version(link.get("rel")?.as_str()?)?;
            (version.0 == 2).then_some((version, link.get("href")?.as_str()?))
        })
        .max_by_key(|(version, _)| *version)
        .map(|(_, href)| href)
}

/// `software.name` of a nodeinfo document, trimmed and lowercased. Empty is `None`.
#[must_use]
pub fn software_name(nodeinfo: &Value) -> Option<String> {
    let name = nodeinfo.pointer("/software/name")?.as_str()?.trim();
    (!name.is_empty()).then(|| name.to_ascii_lowercase())
}

fn document(outcome: FetchOutcome, uri: &str) -> Result<Value, NodeinfoError> {
    let unavailable = |reason: String| NodeinfoError::Unavailable {
        uri: uri.to_owned(),
        reason,
    };
    match outcome {
        FetchOutcome::Found { json, .. } => Ok(json),
        FetchOutcome::Gone => Err(unavailable("not found".to_owned())),
        FetchOutcome::Refused { status } => Err(unavailable(format!("refused with {status}"))),
        FetchOutcome::RateLimited { retry_after_secs } => {
            Err(unavailable(format!("rate limited for {retry_after_secs}s")))
        }
        FetchOutcome::Backoff { until } => {
            Err(unavailable(format!("host is backing off until {until}")))
        }
        FetchOutcome::Error(source) => Err(NodeinfoError::Fetch {
            uri: uri.to_owned(),
            source,
        }),
    }
}

/// The software the host of `uri` runs: `/.well-known/nodeinfo` on the origin of `uri`, then the
/// highest 2.x link it lists, which must be on that same origin, then that document's
/// `software.name`, lowercased.
pub async fn lookup_software(fetcher: &Fetcher, uri: &str) -> Result<String, NodeinfoError> {
    let bad_uri = || NodeinfoError::BadUri {
        uri: uri.to_owned(),
    };
    let base = Url::parse(uri).map_err(|_| bad_uri())?;
    if base.host_str().is_none_or(str::is_empty) {
        return Err(bad_uri());
    }
    let well_known = base.join("/.well-known/nodeinfo").map_err(|_| bad_uri())?;
    let well_known_uri = well_known.as_str();
    let index = document(fetcher.fetch_document(well_known_uri).await, well_known_uri)?;
    let href = nodeinfo_href(&index).ok_or_else(|| NodeinfoError::NoLink {
        uri: well_known_uri.to_owned(),
    })?;
    let off_origin = || NodeinfoError::OffOrigin {
        uri: well_known_uri.to_owned(),
        href: href.to_owned(),
    };
    let target = well_known.join(href).map_err(|_| off_origin())?;
    if target.origin() != well_known.origin() {
        return Err(off_origin());
    }
    let target_uri = target.as_str();
    let nodeinfo = document(fetcher.fetch_document(target_uri).await, target_uri)?;
    software_name(&nodeinfo).ok_or_else(|| NodeinfoError::NoSoftware {
        uri: target_uri.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn versions() {
        assert_eq!(
            nodeinfo_version(&format!("{NODEINFO_REL_PREFIX}2.1")),
            Some((2, 1))
        );
        assert_eq!(
            nodeinfo_version(&format!("{NODEINFO_REL_PREFIX}1.0")),
            Some((1, 0))
        );
        assert_eq!(nodeinfo_version(&format!("{NODEINFO_REL_PREFIX}2")), None);
        assert_eq!(nodeinfo_version("https://example/ns/schema/2.0"), None);
    }

    #[test]
    fn picks_the_highest_2x_link() {
        let index = json!({"links": [
            {"rel": format!("{NODEINFO_REL_PREFIX}1.0"), "href": "/n/1.0"},
            {"rel": format!("{NODEINFO_REL_PREFIX}2.0"), "href": "/n/2.0"},
            {"rel": format!("{NODEINFO_REL_PREFIX}2.1"), "href": "/n/2.1"},
            {"rel": "https://www.w3.org/ns/activitystreams#Application", "href": "/actor"},
        ]});
        assert_eq!(nodeinfo_href(&index), Some("/n/2.1"));
        let old = json!({"links": [{"rel": format!("{NODEINFO_REL_PREFIX}1.0"), "href": "/n"}]});
        assert_eq!(nodeinfo_href(&old), None);
        assert_eq!(nodeinfo_href(&json!({})), None);
        assert_eq!(nodeinfo_href(&json!({"links": "nope"})), None);
    }

    #[test]
    fn software_names() {
        assert_eq!(
            software_name(&json!({"software": {"name": " Lemmy "}})).as_deref(),
            Some("lemmy")
        );
        assert_eq!(software_name(&json!({"software": {"name": ""}})), None);
        assert_eq!(software_name(&json!({"software": {"name": 3}})), None);
        assert_eq!(software_name(&json!({})), None);
    }
}
