//! What an activity delivered to our inbox means. A pure function of the verified sender, the
//! activity JSON and what we know about the sender, table-tested like [`crate::decide`],
//! because a mistake here indexes someone who never opted in or keeps someone who left.
//!
//! The inbox never indexes or stores what it is sent (CLAUDE.md principle 4): an activity only
//! yields URIs to fetch from their origin, removals, and follow bookkeeping.
//!
//! Rules:
//! - `Follow` of our actor: record the opt-in, accept it, follow back, and fetch the actor.
//! - `Undo{Follow}` of our actor: drop the opt-in, tombstone the author, unfollow back, and
//!   fetch the actor again, so an author whose own flags consent is shown again.
//! - `Accept`/`Reject` of our `Follow`: mark it accepted, or drop it.
//! - `Create`/`Update` from an actor we follow: fetch the object if it is on the actor's
//!   origin, or the actor is a `Group` (a Lemmy community relays its members' posts).
//!   Updates wait [`UPDATE_FETCH_DELAY_SECS`], as FASP updates do.
//! - `Announce` from an actor we follow, wrapping an activity (Lemmy communities wrap
//!   `Create`/`Update`/`Delete`/`Remove`/`Undo`/`Block`): the inner activity is handled as if
//!   the community sent it. A boost (a plain object or URI) is ignored: someone else sharing a
//!   post is not the author's consent.
//! - `Delete` from any actor, of an object on its origin (or by a followed `Group`, a moderator
//!   delete): remove it and tombstone it. Of the actor itself: remove the account.
//! - `Remove` from a followed `Group` whose `target` is the group (a moderator removal):
//!   remove the object.
//! - `Block` with `removeData` from a followed `Group` or the blocked user's own instance:
//!   tombstone the author.
//! - `Undo{Delete}`/`Undo{Remove}` from an actor we follow (a restore): fetch the object again;
//!   the fetch decides.
//! - Anything else is ignored, and so is anything our own actor sends.

use apub::inbox::same_origin;
use serde_json::Value;
use store::FetchKind;

use crate::fasp_app::UPDATE_FETCH_DELAY_SECS;

/// The verified sender of an activity and what we know about them.
#[derive(Debug, Clone, Copy)]
pub struct Sender<'a> {
    /// The actor whose key signed the delivery, which is the activity's `actor`.
    pub actor: &'a str,
    /// Whether the actor is a `Group` (a community).
    pub is_group: bool,
    /// The id of our `Follow` of the actor, if we follow it or asked to.
    pub our_follow: Option<&'a str>,
    /// Whether the actor follows our actor (a `follow_optins` row).
    pub follows_us: bool,
}

impl Sender<'_> {
    fn followed(&self) -> bool {
        self.our_follow.is_some()
    }

    /// Whether what the sender says about `uri` (create, update, delete) is worth acting on:
    /// it is on the sender's origin, or the sender is a community we follow.
    fn speaks_for(&self, uri: &str) -> bool {
        same_origin(uri, self.actor) || (self.is_group && self.followed())
    }
}

/// A change to make, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Record a `follow_optins` row.
    RecordFollowOptin { actor_uri: String },
    /// Delete the `follow_optins` row.
    RemoveFollowOptin { actor_uri: String },
    /// Deliver an `Accept` of the actor's `Follow` (by `follow_id`, if it had one).
    AcceptFollow {
        actor_uri: String,
        follow_id: Option<String>,
    },
    /// Follow the actor, unless we do already.
    FollowBack { actor_uri: String },
    /// Deliver `Undo{Follow}` for our follow of the actor, and drop the `following` row.
    Unfollow { actor_uri: String },
    /// Our follow of the actor was accepted.
    FollowAccepted { actor_uri: String },
    /// Drop our `following` row without telling the actor (rejected, or the actor is gone).
    DropFollowing { actor_uri: String },
    /// Queue `uri` for a fetch from its origin (reason announce).
    Fetch {
        uri: String,
        kind: FetchKind,
        delay_secs: i64,
    },
    /// Hide every post by the actor.
    AddAuthorTombstone { actor_uri: String },
    /// Remove an object by URI, with an object tombstone.
    RemoveObject { uri: String },
    /// Remove an account by URI, with an object tombstone.
    RemoveAccount { uri: String },
}

const ACTOR_TYPES: &[&str] = &["Person", "Service", "Group", "Application", "Organization"];
const POST_TYPES: &[&str] = &["Note", "Article", "Page", "Question"];
const WRAPPED_ACTIVITIES: &[&str] = &["Create", "Update", "Delete", "Remove", "Undo", "Block"];

/// The `type` values of a JSON object (a string or an array of them).
fn types(value: &Value) -> Vec<&str> {
    match value.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn is_any(value: &Value, wanted: &[&str]) -> bool {
    types(value).iter().any(|t| wanted.contains(t))
}

/// A reference's id: the string itself, or an embedded object's `id`.
fn id_of(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) => Some(s.as_str()),
        Value::Object(_) => value.get("id").and_then(Value::as_str),
        _ => None,
    }
}

fn field_id<'v>(value: &'v Value, field: &str) -> Option<&'v str> {
    value.get(field).and_then(id_of)
}

/// The `actor` of an activity, by id.
#[must_use]
pub fn activity_actor(activity: &Value) -> Option<&str> {
    field_id(activity, "actor")
}

/// What an activity from a verified sender means. `our_actor` is our server actor's id.
#[must_use]
pub fn handle(our_actor: &str, sender: &Sender<'_>, activity: &Value) -> Vec<Effect> {
    if sender.actor == our_actor || activity_actor(activity) != Some(sender.actor) {
        return Vec::new();
    }
    dispatch(our_actor, sender, activity, false)
}

/// What the removal of an actor's own account means.
#[must_use]
pub fn account_deleted(actor: &str) -> Vec<Effect> {
    vec![
        Effect::RemoveFollowOptin {
            actor_uri: actor.to_owned(),
        },
        Effect::DropFollowing {
            actor_uri: actor.to_owned(),
        },
        Effect::RemoveAccount {
            uri: actor.to_owned(),
        },
        Effect::AddAuthorTombstone {
            actor_uri: actor.to_owned(),
        },
    ]
}

/// The actor of a `Delete` of itself whose key `key_id` names it (`<actor>#fragment`), when
/// that key's document is gone: a deleted account can't sign with a key we can still fetch, but
/// its own origin saying the actor is gone is as good. `None` for anything else.
#[must_use]
pub fn self_delete_with_gone_key<'a>(activity: &'a Value, key_id: &str) -> Option<&'a str> {
    let actor = activity_actor(activity)?;
    let key_document = key_id.split_once('#').map_or(key_id, |(doc, _)| doc);
    (is_any(activity, &["Delete"])
        && field_id(activity, "object") == Some(actor)
        && key_document == actor)
        .then_some(actor)
}

fn dispatch(our_actor: &str, sender: &Sender<'_>, activity: &Value, wrapped: bool) -> Vec<Effect> {
    let actor = sender.actor.to_owned();
    let object = activity.get("object").unwrap_or(&Value::Null);
    let kind = types(activity);
    let Some(kind) = kind.first() else {
        return Vec::new();
    };
    // Only answers, undos and announces may embed an object without an id (a `Follow`, or a
    // wrapped activity); everything else acts on the object's id.
    let object_id = match id_of(object) {
        Some(id) => id,
        None if matches!(*kind, "Accept" | "Reject" | "Undo" | "Announce")
            && object.is_object() =>
        {
            ""
        }
        None => return Vec::new(),
    };
    match *kind {
        "Follow" if !wrapped && object_id == our_actor => vec![
            Effect::RecordFollowOptin {
                actor_uri: actor.clone(),
            },
            Effect::AcceptFollow {
                actor_uri: actor.clone(),
                follow_id: activity
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
            Effect::FollowBack {
                actor_uri: actor.clone(),
            },
            Effect::Fetch {
                uri: actor,
                kind: FetchKind::Actor,
                delay_secs: 0,
            },
        ],
        "Undo" => undo(our_actor, sender, object, wrapped),
        "Accept" | "Reject" if !wrapped && is_our_follow(our_actor, sender, object) => {
            if *kind == "Accept" {
                vec![Effect::FollowAccepted { actor_uri: actor }]
            } else {
                vec![Effect::DropFollowing { actor_uri: actor }]
            }
        }
        "Create" | "Update" if sender.followed() && sender.speaks_for(object_id) => {
            let fetch_kind = if object.is_string() {
                FetchKind::Unknown
            } else if is_any(object, ACTOR_TYPES) {
                FetchKind::Actor
            } else if is_any(object, POST_TYPES) {
                FetchKind::Object
            } else {
                return Vec::new();
            };
            let delay_secs = if *kind == "Update" {
                UPDATE_FETCH_DELAY_SECS
            } else {
                0
            };
            vec![Effect::Fetch {
                uri: object_id.to_owned(),
                kind: fetch_kind,
                delay_secs,
            }]
        }
        "Announce" if !wrapped && sender.followed() && is_any(object, WRAPPED_ACTIVITIES) => {
            dispatch(our_actor, sender, object, true)
        }
        "Delete" if !wrapped && object_id == sender.actor => account_deleted(sender.actor),
        "Delete" if object_id != sender.actor && sender.speaks_for(object_id) => {
            vec![Effect::RemoveObject {
                uri: object_id.to_owned(),
            }]
        }
        "Remove"
            if sender.is_group
                && sender.followed()
                && field_id(activity, "target") == Some(sender.actor) =>
        {
            vec![Effect::RemoveObject {
                uri: object_id.to_owned(),
            }]
        }
        "Block"
            if activity.get("removeData") == Some(&Value::Bool(true))
                && object_id != our_actor
                && ((sender.is_group && sender.followed())
                    || (!wrapped && same_origin(object_id, sender.actor))) =>
        {
            vec![Effect::AddAuthorTombstone {
                actor_uri: object_id.to_owned(),
            }]
        }
        _ => Vec::new(),
    }
}

/// Whether `object` (an `Accept`'s or `Reject`'s) is our `Follow` of the sender: its id, or an
/// embedded `Follow` with that id or from us to the sender.
fn is_our_follow(our_actor: &str, sender: &Sender<'_>, object: &Value) -> bool {
    let Some(ours) = sender.our_follow else {
        return false;
    };
    match object {
        Value::String(id) => id == ours,
        Value::Object(_) if is_any(object, &["Follow"]) => {
            field_id(object, "id") == Some(ours)
                || (field_id(object, "actor") == Some(our_actor)
                    && field_id(object, "object") == Some(sender.actor))
        }
        _ => false,
    }
}

fn undo(our_actor: &str, sender: &Sender<'_>, inner: &Value, wrapped: bool) -> Vec<Effect> {
    let actor = sender.actor.to_owned();
    let unfollow = || {
        vec![
            Effect::RemoveFollowOptin {
                actor_uri: actor.clone(),
            },
            Effect::AddAuthorTombstone {
                actor_uri: actor.clone(),
            },
            Effect::Unfollow {
                actor_uri: actor.clone(),
            },
            Effect::Fetch {
                uri: actor.clone(),
                kind: FetchKind::Actor,
                delay_secs: 0,
            },
        ]
    };
    match inner {
        // Only the id of the follow: the sender follows us, and withdrawing consent on a
        // doubt is the safe way round.
        Value::String(_) if !wrapped && sender.follows_us => unfollow(),
        Value::Object(_) if !wrapped && is_any(inner, &["Follow"]) => {
            let by_sender = field_id(inner, "actor").is_none_or(|a| a == sender.actor);
            if by_sender && field_id(inner, "object") == Some(our_actor) {
                unfollow()
            } else {
                Vec::new()
            }
        }
        // A restore: fetch the object again and let the fetch decide.
        Value::Object(_) if is_any(inner, &["Delete", "Remove"]) && sender.followed() => {
            match field_id(inner, "object") {
                Some(uri) if uri != sender.actor && sender.speaks_for(uri) => {
                    vec![Effect::Fetch {
                        uri: uri.to_owned(),
                        kind: FetchKind::Unknown,
                        delay_secs: 0,
                    }]
                }
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const US: &str = "https://social.example/actor";
    const GTS: &str = "https://gts.example/users/bob";
    const GTS_POST: &str = "https://gts.example/users/bob/statuses/1";
    const OTHER_POST: &str = "https://m.example/users/a/statuses/9";
    const COMMUNITY: &str = "https://lemmy.example/c/garden";
    const LEMMY_USER: &str = "https://lemmy.example/u/carol";
    const LEMMY_POST: &str = "https://lemmy.example/post/7";
    const REMOTE_LEMMY_POST: &str = "https://other-lemmy.example/post/3";
    const OUR_FOLLOW: &str = "https://social.example/actor/follows/1";

    fn stranger(actor: &str) -> Sender<'_> {
        Sender {
            actor,
            is_group: false,
            our_follow: None,
            follows_us: false,
        }
    }

    fn followed(actor: &str) -> Sender<'_> {
        Sender {
            our_follow: Some(OUR_FOLLOW),
            follows_us: true,
            ..stranger(actor)
        }
    }

    fn community() -> Sender<'static> {
        Sender {
            actor: COMMUNITY,
            is_group: true,
            our_follow: Some(OUR_FOLLOW),
            follows_us: false,
        }
    }

    fn fetch(uri: &str, kind: FetchKind, delay_secs: i64) -> Effect {
        Effect::Fetch {
            uri: uri.into(),
            kind,
            delay_secs,
        }
    }

    fn remove(uri: &str) -> Effect {
        Effect::RemoveObject { uri: uri.into() }
    }

    fn note(id: &str) -> Value {
        json!({"id": id, "type": "Note", "content": "never stored"})
    }

    fn follow_effects(actor: &str, id: Option<&str>) -> Vec<Effect> {
        vec![
            Effect::RecordFollowOptin {
                actor_uri: actor.into(),
            },
            Effect::AcceptFollow {
                actor_uri: actor.into(),
                follow_id: id.map(str::to_owned),
            },
            Effect::FollowBack {
                actor_uri: actor.into(),
            },
            fetch(actor, FetchKind::Actor, 0),
        ]
    }

    fn unfollow_effects(actor: &str) -> Vec<Effect> {
        vec![
            Effect::RemoveFollowOptin {
                actor_uri: actor.into(),
            },
            Effect::AddAuthorTombstone {
                actor_uri: actor.into(),
            },
            Effect::Unfollow {
                actor_uri: actor.into(),
            },
            fetch(actor, FetchKind::Actor, 0),
        ]
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One table, read top to bottom.
    fn activities() {
        let follow_us =
            json!({"id": "https://gts.example/f/1", "type": "Follow", "actor": GTS, "object": US});
        let cases: Vec<(&str, Sender<'_>, Value, Vec<Effect>)> = vec![
            // --- follows of our actor
            (
                "follow of us: opt in, accept, follow back, fetch the actor",
                stranger(GTS),
                follow_us.clone(),
                follow_effects(GTS, Some("https://gts.example/f/1")),
            ),
            (
                "follow of us with an embedded object and no id",
                stranger(GTS),
                json!({"type": "Follow", "actor": {"id": GTS}, "object": {"id": US, "type": "Application"}}),
                follow_effects(GTS, None),
            ),
            (
                "follow of someone else",
                stranger(GTS),
                json!({"type": "Follow", "actor": GTS, "object": "https://m.example/users/a"}),
                vec![],
            ),
            (
                "actor field is not the signer",
                stranger(GTS),
                json!({"type": "Follow", "actor": "https://gts.example/users/eve", "object": US}),
                vec![],
            ),
            (
                "our own actor sends nothing we act on",
                stranger(US),
                json!({"type": "Follow", "actor": US, "object": US}),
                vec![],
            ),
            // --- unfollows
            (
                "undo of the follow of us",
                followed(GTS),
                json!({"type": "Undo", "actor": GTS, "object": follow_us}),
                unfollow_effects(GTS),
            ),
            (
                "undo by follow id from an opted-in actor",
                followed(GTS),
                json!({"type": "Undo", "actor": GTS, "object": "https://gts.example/f/1"}),
                unfollow_effects(GTS),
            ),
            (
                "undo by follow id from an actor that doesn't follow us",
                stranger(GTS),
                json!({"type": "Undo", "actor": GTS, "object": "https://gts.example/f/1"}),
                vec![],
            ),
            (
                "undo of a follow of someone else",
                followed(GTS),
                json!({"type": "Undo", "actor": GTS, "object": {"type": "Follow", "actor": GTS, "object": "https://m.example/users/a"}}),
                vec![],
            ),
            (
                "undo of someone else's follow of us",
                followed(GTS),
                json!({"type": "Undo", "actor": GTS, "object": {"type": "Follow", "actor": "https://gts.example/users/eve", "object": US}}),
                vec![],
            ),
            // --- answers to our follows
            (
                "accept of our follow by id",
                followed(GTS),
                json!({"type": "Accept", "actor": GTS, "object": OUR_FOLLOW}),
                vec![Effect::FollowAccepted {
                    actor_uri: GTS.into(),
                }],
            ),
            (
                "accept of our follow embedded without its id",
                followed(GTS),
                json!({"type": "Accept", "actor": GTS, "object": {"type": "Follow", "actor": US, "object": GTS}}),
                vec![Effect::FollowAccepted {
                    actor_uri: GTS.into(),
                }],
            ),
            (
                "reject of our follow",
                community(),
                json!({"type": "Reject", "actor": COMMUNITY, "object": {"type": "Follow", "id": OUR_FOLLOW}}),
                vec![Effect::DropFollowing {
                    actor_uri: COMMUNITY.into(),
                }],
            ),
            (
                "accept of a follow we never sent",
                stranger(GTS),
                json!({"type": "Accept", "actor": GTS, "object": OUR_FOLLOW}),
                vec![],
            ),
            (
                "accept of some other follow",
                followed(GTS),
                json!({"type": "Accept", "actor": GTS, "object": "https://social.example/actor/follows/2"}),
                vec![],
            ),
            // --- posts from followed actors
            (
                "create of an embedded note on the actor's origin",
                followed(GTS),
                json!({"type": "Create", "actor": GTS, "object": note(GTS_POST)}),
                vec![fetch(GTS_POST, FetchKind::Object, 0)],
            ),
            (
                "create of a note by URI",
                followed(GTS),
                json!({"type": "Create", "actor": GTS, "object": GTS_POST}),
                vec![fetch(GTS_POST, FetchKind::Unknown, 0)],
            ),
            (
                "update waits for the edit cache",
                followed(GTS),
                json!({"type": "Update", "actor": GTS, "object": note(GTS_POST)}),
                vec![fetch(GTS_POST, FetchKind::Object, UPDATE_FETCH_DELAY_SECS)],
            ),
            (
                "update of the actor's profile",
                followed(GTS),
                json!({"type": "Update", "actor": GTS, "object": {"id": GTS, "type": "Person"}}),
                vec![fetch(GTS, FetchKind::Actor, UPDATE_FETCH_DELAY_SECS)],
            ),
            (
                "create of something that is neither post nor actor",
                followed(GTS),
                json!({"type": "Create", "actor": GTS, "object": {"id": GTS_POST, "type": "ChatMessage"}}),
                vec![],
            ),
            (
                "create from an actor we don't follow",
                stranger(GTS),
                json!({"type": "Create", "actor": GTS, "object": note(GTS_POST)}),
                vec![],
            ),
            (
                "create of an object on another origin",
                followed(GTS),
                json!({"type": "Create", "actor": GTS, "object": note(OTHER_POST)}),
                vec![],
            ),
            // --- boosts and Lemmy's wrapped activities
            (
                "boost of a post by URI",
                followed(GTS),
                json!({"type": "Announce", "actor": GTS, "object": OTHER_POST}),
                vec![],
            ),
            (
                "boost of an embedded post",
                followed(GTS),
                json!({"type": "Announce", "actor": GTS, "object": note(GTS_POST)}),
                vec![],
            ),
            (
                "community announces a member's post from another instance",
                community(),
                json!({"type": "Announce", "actor": COMMUNITY, "object":
                    {"type": "Create", "actor": "https://other-lemmy.example/u/dan",
                     "object": {"id": REMOTE_LEMMY_POST, "type": "Page"}}}),
                vec![fetch(REMOTE_LEMMY_POST, FetchKind::Object, 0)],
            ),
            (
                "community announces an update",
                community(),
                json!({"type": "Announce", "actor": COMMUNITY, "object":
                    {"type": "Update", "actor": LEMMY_USER, "object": {"id": LEMMY_POST, "type": "Page"}}}),
                vec![fetch(
                    LEMMY_POST,
                    FetchKind::Object,
                    UPDATE_FETCH_DELAY_SECS,
                )],
            ),
            (
                "announce from a community we don't follow",
                Sender {
                    our_follow: None,
                    ..community()
                },
                json!({"type": "Announce", "actor": COMMUNITY, "object":
                    {"type": "Create", "actor": LEMMY_USER, "object": {"id": LEMMY_POST, "type": "Page"}}}),
                vec![],
            ),
            (
                "community announces a moderator delete",
                community(),
                json!({"type": "Announce", "actor": COMMUNITY, "object":
                    {"type": "Delete", "actor": "https://lemmy.example/u/mod", "object": REMOTE_LEMMY_POST}}),
                vec![remove(REMOTE_LEMMY_POST)],
            ),
            (
                "community announces a removal",
                community(),
                json!({"type": "Announce", "actor": COMMUNITY, "object":
                    {"type": "Remove", "actor": "https://lemmy.example/u/mod", "object": LEMMY_POST, "target": COMMUNITY}}),
                vec![remove(LEMMY_POST)],
            ),
            (
                "removal of a moderator (target the moderators collection)",
                community(),
                json!({"type": "Announce", "actor": COMMUNITY, "object":
                    {"type": "Remove", "actor": "https://lemmy.example/u/mod", "object": LEMMY_USER,
                     "target": "https://lemmy.example/c/garden/moderators"}}),
                vec![],
            ),
            (
                "community announces a restore",
                community(),
                json!({"type": "Announce", "actor": COMMUNITY, "object":
                    {"type": "Undo", "actor": "https://lemmy.example/u/mod", "object":
                        {"type": "Remove", "object": LEMMY_POST, "target": COMMUNITY}}}),
                vec![fetch(LEMMY_POST, FetchKind::Unknown, 0)],
            ),
            (
                "community announces a ban that removes data",
                community(),
                json!({"type": "Announce", "actor": COMMUNITY, "object":
                    {"type": "Block", "actor": "https://lemmy.example/u/mod", "object": LEMMY_USER,
                     "target": COMMUNITY, "removeData": true}}),
                vec![Effect::AddAuthorTombstone {
                    actor_uri: LEMMY_USER.into(),
                }],
            ),
            (
                "a ban that keeps data",
                community(),
                json!({"type": "Announce", "actor": COMMUNITY, "object":
                    {"type": "Block", "actor": "https://lemmy.example/u/mod", "object": LEMMY_USER,
                     "target": COMMUNITY, "removeData": false}}),
                vec![],
            ),
            (
                "an announced follow or announce is not unwrapped further",
                community(),
                json!({"type": "Announce", "actor": COMMUNITY, "object":
                    {"type": "Announce", "actor": COMMUNITY, "object":
                        {"type": "Delete", "actor": LEMMY_USER, "object": OTHER_POST}}}),
                vec![],
            ),
            (
                "an announced undo of a follow is not an unfollow",
                community(),
                json!({"type": "Announce", "actor": COMMUNITY, "object":
                    {"type": "Undo", "actor": LEMMY_USER, "object": {"type": "Follow", "actor": LEMMY_USER, "object": US}}}),
                vec![],
            ),
            // --- deletes
            (
                "delete of an object on the sender's origin, from anyone",
                stranger(GTS),
                json!({"type": "Delete", "actor": GTS, "object": GTS_POST}),
                vec![remove(GTS_POST)],
            ),
            (
                "delete with a tombstone object",
                stranger(GTS),
                json!({"type": "Delete", "actor": GTS, "object": {"id": GTS_POST, "type": "Tombstone"}}),
                vec![remove(GTS_POST)],
            ),
            (
                "delete of an object on another origin",
                followed(GTS),
                json!({"type": "Delete", "actor": GTS, "object": OTHER_POST}),
                vec![],
            ),
            (
                "delete by a community we don't follow, of a post elsewhere",
                Sender {
                    our_follow: None,
                    ..community()
                },
                json!({"type": "Delete", "actor": COMMUNITY, "object": REMOTE_LEMMY_POST}),
                vec![],
            ),
            (
                "delete of the actor itself",
                stranger(GTS),
                json!({"type": "Delete", "actor": GTS, "object": GTS}),
                account_deleted(GTS),
            ),
            (
                "remove from a group we don't follow",
                Sender {
                    our_follow: None,
                    ..community()
                },
                json!({"type": "Remove", "actor": COMMUNITY, "object": LEMMY_POST, "target": COMMUNITY}),
                vec![],
            ),
            (
                "remove from a person",
                followed(GTS),
                json!({"type": "Remove", "actor": GTS, "object": GTS_POST, "target": GTS}),
                vec![],
            ),
            (
                "instance ban that removes data",
                stranger("https://lemmy.example/u/admin"),
                json!({"type": "Block", "actor": "https://lemmy.example/u/admin", "object": LEMMY_USER,
                       "target": "https://lemmy.example/", "removeData": true}),
                vec![Effect::AddAuthorTombstone {
                    actor_uri: LEMMY_USER.into(),
                }],
            ),
            (
                "a user blocking someone on another instance",
                stranger(GTS),
                json!({"type": "Block", "actor": GTS, "object": LEMMY_USER, "removeData": true}),
                vec![],
            ),
            (
                "a block of our actor",
                stranger("https://social.example/users/x"),
                json!({"type": "Block", "actor": "https://social.example/users/x", "object": US, "removeData": true}),
                vec![],
            ),
            // --- the rest
            (
                "like from a followed actor",
                followed(GTS),
                json!({"type": "Like", "actor": GTS, "object": GTS_POST}),
                vec![],
            ),
            (
                "no object",
                followed(GTS),
                json!({"type": "Create", "actor": GTS}),
                vec![],
            ),
            (
                "no type",
                followed(GTS),
                json!({"actor": GTS, "object": GTS_POST}),
                vec![],
            ),
        ];
        for (name, sender, activity, expected) in cases {
            assert_eq!(handle(US, &sender, &activity), expected, "{name}");
        }
    }

    #[test]
    fn self_deletes_with_a_gone_key() {
        let delete = json!({"type": "Delete", "actor": GTS, "object": GTS});
        assert_eq!(
            self_delete_with_gone_key(&delete, &format!("{GTS}#main-key")),
            Some(GTS)
        );
        assert_eq!(self_delete_with_gone_key(&delete, GTS), Some(GTS));
        // A key elsewhere, or a delete of something else, proves nothing.
        assert_eq!(
            self_delete_with_gone_key(&delete, "https://gts.example/keys/1"),
            None
        );
        let other = json!({"type": "Delete", "actor": GTS, "object": GTS_POST});
        assert_eq!(
            self_delete_with_gone_key(&other, &format!("{GTS}#main-key")),
            None
        );
        let create = json!({"type": "Create", "actor": GTS, "object": GTS});
        assert_eq!(
            self_delete_with_gone_key(&create, &format!("{GTS}#main-key")),
            None
        );
    }
}
