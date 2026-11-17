//! Parsing of `POST /data_sharing/v0/announcements` bodies.

use serde::Deserialize;
use store::Category;
use thiserror::Error;
use url::Url;

/// Most object URIs taken from one announcement; the rest are dropped.
pub const MAX_OBJECT_URIS: usize = 1000;
/// Longest URI we accept; longer ones are ignored.
pub const MAX_URI_LEN: usize = 2048;

/// An event type: the three `lifecycle` ones, or `trending` from a `trends` subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    New,
    Update,
    Delete,
    /// The object had recent interactions above the subscription's threshold.
    Trending,
}

impl EventType {
    /// The subscription type whose announcements carry this event.
    #[must_use]
    pub const fn subscription_type(self) -> store::SubscriptionType {
        match self {
            Self::New | Self::Update | Self::Delete => store::SubscriptionType::Lifecycle,
            Self::Trending => store::SubscriptionType::Trends,
        }
    }
}

/// What the announcement answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnouncementSource {
    /// A lifecycle event on one of our subscriptions.
    Subscription { remote_id: String, event: EventType },
    /// A batch for one of our backfill requests.
    Backfill {
        remote_id: String,
        more_available: bool,
    },
}

/// A validated announcement. `object_uris` holds only `http(s)` URIs, at most
/// [`MAX_OBJECT_URIS`] of them; it can be empty (a final, empty backfill batch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announcement {
    pub source: AnnouncementSource,
    pub category: Category,
    pub object_uris: Vec<String>,
}

/// Why an announcement body was refused (HTTP 422).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AnnouncementError {
    #[error("body is not a valid announcement: {0}")]
    Malformed(String),
}

fn malformed(reason: impl Into<String>) -> AnnouncementError {
    AnnouncementError::Malformed(reason.into())
}

#[derive(Deserialize)]
#[serde(untagged)]
enum IdValue {
    Text(String),
    Int(i64),
}

impl IdValue {
    fn into_string(self) -> String {
        match self {
            Self::Text(s) => s,
            Self::Int(n) => n.to_string(),
        }
    }
}

#[derive(Deserialize)]
struct IdRef {
    id: IdValue,
}

#[derive(Deserialize)]
struct RawSource {
    subscription: Option<IdRef>,
    #[serde(rename = "backfillRequest")]
    backfill_request: Option<IdRef>,
}

#[derive(Deserialize)]
struct RawAnnouncement {
    source: RawSource,
    category: String,
    #[serde(rename = "eventType")]
    event_type: Option<String>,
    #[serde(rename = "objectUris")]
    object_uris: Vec<serde_json::Value>,
    #[serde(rename = "moreObjectsAvailable")]
    more_objects_available: Option<bool>,
}

fn clean_uris(raw: Vec<serde_json::Value>) -> Vec<String> {
    raw.into_iter()
        .take(MAX_OBJECT_URIS)
        .filter_map(|v| match v {
            serde_json::Value::String(s) => Some(s),
            _ => None,
        })
        .filter(|s| s.len() <= MAX_URI_LEN)
        .filter(|s| Url::parse(s).is_ok_and(|u| matches!(u.scheme(), "http" | "https")))
        .collect()
}

/// Parses an announcement body.
///
/// `Ok(None)` means a well-formed announcement we drop: a `trending` event for the `account`
/// category, which the spec does not define (`trends` applies to `content` only). Ids may
/// be strings (the spec) or numbers. A subscription announcement needs an `eventType`; a
/// backfill announcement without `moreObjectsAvailable` counts as having no more.
pub fn parse_announcement(body: &[u8]) -> Result<Option<Announcement>, AnnouncementError> {
    let raw: RawAnnouncement =
        serde_json::from_slice(body).map_err(|e| malformed(e.to_string()))?;
    let category: Category = raw
        .category
        .parse()
        .map_err(|_| malformed(format!("unknown category `{}`", raw.category)))?;
    let source = match (raw.source.subscription, raw.source.backfill_request) {
        (Some(sub), None) => {
            let event = match raw.event_type.as_deref() {
                Some("new") => EventType::New,
                Some("update") => EventType::Update,
                Some("delete") => EventType::Delete,
                Some("trending") if category == Category::Account => return Ok(None),
                Some("trending") => EventType::Trending,
                Some(other) => return Err(malformed(format!("unknown eventType `{other}`"))),
                None => return Err(malformed("subscription announcement has no eventType")),
            };
            let remote_id = sub.id.into_string();
            AnnouncementSource::Subscription { remote_id, event }
        }
        (None, Some(backfill)) => {
            if raw.event_type.is_some() {
                return Err(malformed("backfill announcement has an eventType"));
            }
            AnnouncementSource::Backfill {
                remote_id: backfill.id.into_string(),
                more_available: raw.more_objects_available.unwrap_or(false),
            }
        }
        _ => {
            return Err(malformed(
                "source must have exactly one of subscription or backfillRequest",
            ));
        }
    };
    let (AnnouncementSource::Subscription { remote_id, .. }
    | AnnouncementSource::Backfill { remote_id, .. }) = &source;
    if remote_id.is_empty() {
        return Err(malformed("source id is empty"));
    }
    Ok(Some(Announcement {
        source,
        category,
        object_uris: clean_uris(raw.object_uris),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<Option<Announcement>, AnnouncementError> {
        parse_announcement(json.as_bytes())
    }

    #[test]
    fn subscription_events() {
        let a = parse(
            r#"{"source":{"subscription":{"id":"58"}},"category":"content","eventType":"new",
                "objectUris":["https://a.example/1","http://b.example/2"]}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            a.source,
            AnnouncementSource::Subscription {
                remote_id: "58".into(),
                event: EventType::New
            }
        );
        assert_eq!(a.category, Category::Content);
        assert_eq!(a.object_uris.len(), 2);
    }

    #[test]
    fn numeric_ids_become_strings() {
        let a = parse(
            r#"{"source":{"backfillRequest":{"id":5}},"category":"account",
                "moreObjectsAvailable":true,"objectUris":["https://a.example/u"]}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            a.source,
            AnnouncementSource::Backfill {
                remote_id: "5".into(),
                more_available: true
            }
        );
    }

    #[test]
    fn trending_content_is_an_event_and_trending_accounts_are_dropped() {
        let a = parse(
            r#"{"source":{"subscription":{"id":"1"}},"category":"content",
                "eventType":"trending","objectUris":["https://a.example/1"]}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            a.source,
            AnnouncementSource::Subscription {
                remote_id: "1".into(),
                event: EventType::Trending
            }
        );
        assert_eq!(a.object_uris, ["https://a.example/1"]);
        let b = parse(
            r#"{"source":{"subscription":{"id":"1"}},"category":"account",
                "eventType":"trending","objectUris":["https://a.example/u"]}"#,
        );
        assert_eq!(b, Ok(None));
    }

    #[test]
    fn non_http_and_oversized_uris_are_ignored() {
        let long = format!("https://a.example/{}", "x".repeat(MAX_URI_LEN));
        let body = format!(
            r#"{{"source":{{"subscription":{{"id":"1"}}}},"category":"content","eventType":"new",
                "objectUris":["ftp://a.example/1","javascript:alert(1)","not a uri",5,null,
                "{long}","https://ok.example/1"]}}"#
        );
        let a = parse(&body).unwrap().unwrap();
        assert_eq!(a.object_uris, vec!["https://ok.example/1".to_owned()]);
    }

    #[test]
    fn uri_count_is_capped() {
        let uris: Vec<String> = (0..MAX_OBJECT_URIS + 50)
            .map(|i| format!("\"https://a.example/{i}\""))
            .collect();
        let body = format!(
            r#"{{"source":{{"subscription":{{"id":"1"}}}},"category":"content","eventType":"new",
                "objectUris":[{}]}}"#,
            uris.join(",")
        );
        let a = parse(&body).unwrap().unwrap();
        assert_eq!(a.object_uris.len(), MAX_OBJECT_URIS);
    }

    #[test]
    fn empty_backfill_batch_is_accepted_and_missing_flag_means_done() {
        let a = parse(
            r#"{"source":{"backfillRequest":{"id":"9"}},"category":"content","objectUris":[]}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(a.object_uris.len(), 0);
        assert_eq!(
            a.source,
            AnnouncementSource::Backfill {
                remote_id: "9".into(),
                more_available: false
            }
        );
    }

    #[test]
    fn malformed_bodies() {
        for bad in [
            "",
            "[]",
            "{}",
            r#"{"source":{},"category":"content","eventType":"new","objectUris":[]}"#,
            r#"{"source":{"subscription":{"id":"1"},"backfillRequest":{"id":"2"}},"category":"content","eventType":"new","objectUris":[]}"#,
            r#"{"source":{"subscription":{"id":"1"}},"category":"status","eventType":"new","objectUris":[]}"#,
            r#"{"source":{"subscription":{"id":"1"}},"category":"content","objectUris":[]}"#,
            r#"{"source":{"subscription":{"id":"1"}},"category":"content","eventType":"bogus","objectUris":[]}"#,
            r#"{"source":{"subscription":{"id":"1"}},"category":"content","eventType":"new"}"#,
            r#"{"source":{"subscription":{"id":"1"}},"category":"content","eventType":"new","objectUris":"x"}"#,
            r#"{"source":{"subscription":{"id":""}},"category":"content","eventType":"new","objectUris":[]}"#,
            r#"{"source":{"backfillRequest":{"id":"1"}},"category":"content","eventType":"new","objectUris":[]}"#,
            r#"{"source":{"subscription":{"id":[1]}},"category":"content","eventType":"new","objectUris":[]}"#,
        ] {
            assert!(parse(bad).is_err(), "should reject: {bad}");
        }
    }
}
