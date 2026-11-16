//! Actors our server actor follows on an admin's say-so (PLAN.md Stage 3 item 2): naming them,
//! finding them by `WebFinger`, checking what they are, and the `Follow` and `Undo` activities
//! we send them. The `follow`/`unfollow` commands and the `follow` config option use this; the
//! inbox uses the same activities for its follow-backs.
//!
//! Every request goes through the [`Fetcher`] (signed, per-host limits, backoff). Only what an
//! actor document says about itself is kept: its type, its inbox and its outbox, each only if
//! it is on the actor's own origin.

use apub::fetch::{FetchOutcome, Fetcher};
use apub::inbox::same_origin;
use serde_json::{Value, json};
use url::Url;

use crate::activities::{ACTOR_TYPES, is_any};

const AS_CONTEXT: &str = "https://www.w3.org/ns/activitystreams";

/// Why a follow target could not be named, found or followed. Messages carry URIs and hosts.
#[derive(Debug, thiserror::Error)]
pub enum FollowError {
    #[error(
        "{0:?} is not an actor URI or a handle (`@name@host`, `!community@host` or `name@host`)"
    )]
    BadTarget(String),
    #[error("WebFinger for {handle} failed: {reason}")]
    WebFinger { handle: String, reason: String },
    #[error("WebFinger for {handle} has no ActivityPub `self` link")]
    NoSelfLink { handle: String },
    #[error("actor {uri} could not be fetched: {reason}")]
    Fetch { uri: String, reason: String },
    #[error("{uri} is not an actor (type {kind})")]
    NotAnActor { uri: String, kind: String },
    #[error("actor {uri} has no inbox on its own origin")]
    NoInbox { uri: String },
}

/// What `follow`/`unfollow` were given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowTarget {
    /// An actor URI.
    Actor(String),
    /// `@name@host`, `!name@host` or `name@host`, resolved by `WebFinger` as `acct:name@host`
    /// (Lemmy communities answer that too).
    Handle { name: String, host: String },
}

impl std::fmt::Display for FollowTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Actor(uri) => f.write_str(uri),
            Self::Handle { name, host } => write!(f, "{name}@{host}"),
        }
    }
}

/// Parses an actor URI (contains `://`) or a handle, with an optional leading `@` or `!`.
pub fn parse_follow_target(text: &str) -> Result<FollowTarget, FollowError> {
    let text = text.trim();
    let bad = || FollowError::BadTarget(text.to_owned());
    if text.contains("://") {
        let url = Url::parse(text).map_err(|_| bad())?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(bad());
        }
        return Ok(FollowTarget::Actor(text.to_owned()));
    }
    let handle = text
        .strip_prefix('@')
        .or_else(|| text.strip_prefix('!'))
        .unwrap_or(text);
    let (name, host) = handle.split_once('@').ok_or_else(bad)?;
    let clean = |s: &str| {
        !s.is_empty() && !s.contains(|c: char| c.is_whitespace() || matches!(c, '@' | '/' | '?'))
    };
    if !clean(name) || !clean(host) {
        return Err(bad());
    }
    Ok(FollowTarget::Handle {
        name: name.to_owned(),
        host: host.to_ascii_lowercase(),
    })
}

/// The `self` link of a `WebFinger` document that points at an `ActivityPub` actor.
#[must_use]
pub fn self_link(jrd: &Value) -> Option<String> {
    jrd.get("links")?.as_array()?.iter().find_map(|link| {
        let is_self = link.get("rel").and_then(Value::as_str) == Some("self");
        let kind = link.get("type").and_then(Value::as_str).unwrap_or_default();
        let activity = kind.contains("activity+json") || kind.contains("ld+json");
        (is_self && activity)
            .then(|| link.get("href").and_then(Value::as_str).map(str::to_string))
            .flatten()
    })
}

/// The actor URI `target` names: itself, or a signed `WebFinger` lookup on the handle's host.
pub async fn resolve(
    fetcher: &Fetcher,
    allow_http: bool,
    target: &FollowTarget,
) -> Result<String, FollowError> {
    let (name, host) = match target {
        FollowTarget::Actor(uri) => return Ok(uri.clone()),
        FollowTarget::Handle { name, host } => (name, host),
    };
    let handle = format!("{name}@{host}");
    let failed = |reason: String| FollowError::WebFinger {
        handle: handle.clone(),
        reason,
    };
    let scheme = if allow_http { "http" } else { "https" };
    let mut url = Url::parse(&format!("{scheme}://{host}/.well-known/webfinger"))
        .map_err(|e| failed(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("resource", &format!("acct:{handle}"));
    let jrd = match fetcher.fetch_document(url.as_str()).await {
        FetchOutcome::Found { json, .. } => json,
        other => return Err(failed(format!("{other:?}"))),
    };
    let href = self_link(&jrd).ok_or_else(|| FollowError::NoSelfLink {
        handle: handle.clone(),
    })?;
    if parse_follow_target(&href).is_ok_and(|t| matches!(t, FollowTarget::Actor(_))) {
        Ok(href)
    } else {
        Err(failed(format!(
            "its `self` link {href:?} is not an http(s) URI"
        )))
    }
}

/// What we know about an actor beyond its URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorInfo {
    /// Whether it has an actor type (`Person`, `Group`, `Service`, …).
    pub is_actor: bool,
    pub is_group: bool,
    /// The actor's own inbox, if it is on the actor's origin.
    pub inbox: Option<String>,
    /// The actor's outbox, if it is on the actor's origin.
    pub outbox: Option<String>,
}

/// What an actor document says about itself.
#[must_use]
pub fn actor_info(actor: &Value) -> ActorInfo {
    let id = actor.get("id").and_then(Value::as_str).unwrap_or_default();
    let own = |field: &str| {
        actor
            .get(field)
            .and_then(Value::as_str)
            .filter(|uri| same_origin(uri, id))
            .map(str::to_owned)
    };
    ActorInfo {
        is_actor: is_any(actor, ACTOR_TYPES),
        is_group: is_any(actor, &["Group"]),
        inbox: own("inbox"),
        outbox: own("outbox"),
    }
}

/// An actor we may follow: its canonical id, and what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowableActor {
    pub uri: String,
    pub inbox: String,
    pub info: ActorInfo,
}

/// Fetches `uri` and checks it is an actor with an inbox on its own origin. The id the document
/// gives (on the origin it was served from) is the actor's URI from then on.
pub async fn fetch_followable(
    fetcher: &Fetcher,
    uri: &str,
) -> Result<FollowableActor, FollowError> {
    let json = match fetcher.fetch(uri).await {
        FetchOutcome::Found { json, .. } => json,
        other => {
            return Err(FollowError::Fetch {
                uri: uri.to_owned(),
                reason: format!("{other:?}"),
            });
        }
    };
    let id = json
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(uri)
        .to_owned();
    let info = actor_info(&json);
    if !info.is_actor {
        let kind = json.get("type").map_or_else(String::new, Value::to_string);
        return Err(FollowError::NotAnActor { uri: id, kind });
    }
    let inbox = info
        .inbox
        .clone()
        .ok_or_else(|| FollowError::NoInbox { uri: id.clone() })?;
    Ok(FollowableActor {
        uri: id,
        inbox,
        info,
    })
}

/// A fresh id under our actor for an activity we send: `{actor}/{kind}/<ms>-<n>`. `n` tells
/// apart ids made in the same millisecond (a counter in the server, the process id in the CLI).
#[must_use]
pub fn activity_id(our_actor: &str, kind: &str, millis: i64, n: u64) -> String {
    format!("{our_actor}/{kind}/{millis}-{n}")
}

/// Our `Follow` of `object`.
#[must_use]
pub fn follow_activity(our_actor: &str, id: &str, object: &str) -> Value {
    json!({
        "@context": AS_CONTEXT,
        "id": id,
        "type": "Follow",
        "actor": our_actor,
        "object": object,
    })
}

/// Our `Undo` of our `Follow` `follow_id` of `object`.
#[must_use]
pub fn undo_follow_activity(our_actor: &str, follow_id: &str, object: &str) -> Value {
    json!({
        "@context": AS_CONTEXT,
        "id": format!("{follow_id}/undo"),
        "type": "Undo",
        "actor": our_actor,
        "object": {
            "id": follow_id,
            "type": "Follow",
            "actor": our_actor,
            "object": object,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        let handle = |name: &str, host: &str| {
            Some(FollowTarget::Handle {
                name: name.into(),
                host: host.into(),
            })
        };
        for (text, want) in [
            (
                "https://l.example/c/rust",
                Some(FollowTarget::Actor("https://l.example/c/rust".into())),
            ),
            ("!rust@Lemmy.Example", handle("rust", "lemmy.example")),
            ("@alice@m.example", handle("alice", "m.example")),
            ("alice@m.example", handle("alice", "m.example")),
            (" bob@127.0.0.1:8080 ", handle("bob", "127.0.0.1:8080")),
            ("", None),
            ("rust", None),
            ("!@l.example", None),
            ("!rust@", None),
            ("a@b@c", None),
            ("a@b/c", None),
            ("two words@x", None),
            ("ftp://l.example/c/rust", None),
            ("https://", None),
        ] {
            assert_eq!(parse_follow_target(text).ok(), want, "{text:?}");
        }
        assert_eq!(
            handle("rust", "l.example").map(|t| t.to_string()),
            Some("rust@l.example".into())
        );
    }

    #[test]
    fn webfinger_self_link() {
        let jrd = json!({"subject": "acct:a@h.example", "links": [
            {"rel": "http://webfinger.net/rel/profile-page", "type": "text/html", "href": "https://h.example/@a"},
            {"rel": "self", "type": "application/activity+json", "href": "https://h.example/users/a"},
        ]});
        assert_eq!(
            self_link(&jrd).as_deref(),
            Some("https://h.example/users/a")
        );
        let ld = json!({"links": [{"rel": "self", "type": "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\"", "href": "https://h.example/u"}]});
        assert_eq!(self_link(&ld).as_deref(), Some("https://h.example/u"));
        assert_eq!(
            self_link(&json!({"links": [{"rel": "self", "type": "text/html", "href": "x"}]})),
            None
        );
        assert_eq!(self_link(&json!({})), None);
        assert_eq!(self_link(&json!({"links": "no"})), None);
    }

    #[test]
    fn actor_infos() {
        let actor = json!({"id": "https://l.example/c/g", "type": "Group",
            "inbox": "https://l.example/c/g/inbox", "outbox": "https://l.example/c/g/outbox"});
        assert_eq!(
            actor_info(&actor),
            ActorInfo {
                is_actor: true,
                is_group: true,
                inbox: Some("https://l.example/c/g/inbox".into()),
                outbox: Some("https://l.example/c/g/outbox".into()),
            }
        );
        // An inbox or outbox elsewhere is not used.
        let actor = json!({"id": "https://a.example/u/x", "type": ["Person"],
            "inbox": "https://victim.example/inbox", "outbox": "https://victim.example/o"});
        assert_eq!(
            actor_info(&actor),
            ActorInfo {
                is_actor: true,
                is_group: false,
                inbox: None,
                outbox: None,
            }
        );
        let note = json!({"id": "https://a.example/n/1", "type": "Note"});
        assert!(!actor_info(&note).is_actor);
    }

    #[test]
    fn activities() {
        let us = "https://s.example/actor";
        let id = activity_id(us, "follows", 1700, 3);
        assert_eq!(id, "https://s.example/actor/follows/1700-3");
        let follow = follow_activity(us, &id, "https://l.example/c/g");
        assert_eq!(follow["type"], "Follow");
        assert_eq!(follow["actor"], us);
        assert_eq!(follow["object"], "https://l.example/c/g");
        let undo = undo_follow_activity(us, &id, "https://l.example/c/g");
        assert_eq!(undo["id"], format!("{id}/undo"));
        assert_eq!(undo["object"]["id"], id);
        assert_eq!(undo["object"]["object"], "https://l.example/c/g");
    }
}
