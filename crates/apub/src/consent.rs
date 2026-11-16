//! Whether we may index something. A pure function of the fetched JSON, so it is tested
//! table-first: every situation it covers has a case in `tests/consent.rs`.
//!
//! The rules (CLAUDE.md principle 3, PLAN.md "Consent"):
//! - Posts: a supported object type, addressed to the public collection in `to` (unlisted and
//!   quiet-public posts put it in `cc` only, and don't qualify), attributed to the author we
//!   fetched, on the author's host, by an author with `discoverable: true` and
//!   `indexable: true` (FEP-5feb).
//! - Accounts: a supported actor type with `discoverable: true`.
//! - Anything missing, malformed or unexpected is a refusal. When in doubt, don't index.
//!
//! Two inputs can stand in for a flag the actor doesn't carry at all ([`ConsentContext`]):
//! the actor's host runs Lemmy, or the actor follows our server actor (PLAN.md Stage 3). An
//! explicit value that isn't `true` (`false`, `null`, `"true"`) still means no. Both follow the
//! *author*: a Mastodon user replying in a Lemmy community needs the usual flags.

use serde_json::Value;

/// The `ActivityStreams` public collection, and the compact forms JSON-LD allows for it.
const PUBLIC: [&str; 3] = [
    "https://www.w3.org/ns/activitystreams#Public",
    "as:Public",
    "Public",
];

const POST_TYPES: [&str; 4] = ["Note", "Article", "Page", "Question"];
const ACTOR_TYPES: [&str; 5] = ["Person", "Service", "Group", "Application", "Organization"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The object is a `Tombstone`, or otherwise says it is gone.
    Gone,
    /// Not a type we index.
    UnsupportedType,
    /// No usable `id`.
    MissingId,
    /// Not addressed to the public collection in `to`: unlisted, followers-only or direct.
    NotPublic,
    /// No `attributedTo`, or more than one author.
    MissingAuthor,
    /// `attributedTo` is not the actor we were given.
    AuthorMismatch,
    /// The object and its author are on different hosts.
    OriginMismatch,
    /// The author has not set `discoverable: true`.
    NotDiscoverable,
    /// The author has not set `indexable: true`.
    NotIndexable,
}

/// The type of an object or actor, which may be a string or a list of strings.
fn has_type(value: &Value, allowed: &[&str]) -> bool {
    match value.get("type") {
        Some(Value::String(t)) => allowed.contains(&t.as_str()),
        Some(Value::Array(types)) => types
            .iter()
            .any(|t| t.as_str().is_some_and(|t| allowed.contains(&t))),
        _ => false,
    }
}

/// A link that may be a bare string or an object with an `id`.
fn link_id(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) => Some(s),
        Value::Object(o) => o.get("id").and_then(Value::as_str),
        _ => None,
    }
}

/// Every id in a property that may be a single link or a list of links.
fn link_ids(value: Option<&Value>) -> Vec<&str> {
    match value {
        None => Vec::new(),
        Some(Value::Array(items)) => items.iter().filter_map(link_id).collect(),
        Some(single) => link_id(single).into_iter().collect(),
    }
}

fn id_of(value: &Value) -> Option<&str> {
    value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

/// The scheme and host of a URI, lowercased, for same-origin checks.
fn origin(uri: &str) -> Option<String> {
    let (scheme, rest) = uri.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    (!authority.is_empty()).then(|| {
        format!(
            "{}://{}",
            scheme.to_ascii_lowercase(),
            authority.to_ascii_lowercase()
        )
    })
}

/// What we know about an actor beyond its own JSON, which can stand in for a consent flag the
/// actor doesn't carry. The caller works these out; the evaluation stays pure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConsentContext {
    /// The actor's host runs Lemmy, by its nodeinfo `software.name`.
    pub author_on_lemmy: bool,
    /// The actor follows our server actor, which opts them in to indexing.
    pub follows_us: bool,
}

impl ConsentContext {
    /// Neither stand-in applies: only the actor's own flags count.
    pub const NONE: Self = Self {
        author_on_lemmy: false,
        follows_us: false,
    };
}

/// A literal JSON `true` counts. A missing field counts only when `context` supplies a
/// stand-in. Anything else that is present (`false`, `null`, a string "true") is a no.
fn flag(value: &Value, name: &str, context: ConsentContext) -> bool {
    match value.get(name) {
        Some(Value::Bool(true)) => true,
        None => context.author_on_lemmy || context.follows_us,
        Some(_) => false,
    }
}

fn is_public(object: &Value) -> bool {
    link_ids(object.get("to"))
        .iter()
        .any(|id| PUBLIC.contains(id))
}

/// May this actor be indexed as an account? Only its own flags count.
pub fn evaluate_account(actor: &Value) -> Result<(), Refusal> {
    evaluate_account_in(actor, ConsentContext::NONE)
}

/// May this actor be indexed as an account, given what `context` says about it?
pub fn evaluate_account_in(actor: &Value, context: ConsentContext) -> Result<(), Refusal> {
    if has_type(actor, &["Tombstone"]) {
        return Err(Refusal::Gone);
    }
    if !has_type(actor, &ACTOR_TYPES) {
        return Err(Refusal::UnsupportedType);
    }
    if id_of(actor).is_none() {
        return Err(Refusal::MissingId);
    }
    if !flag(actor, "discoverable", context) {
        return Err(Refusal::NotDiscoverable);
    }
    Ok(())
}

/// May this object be indexed as a post by `author`, the actor its `attributedTo` names? Only
/// the author's own flags count.
pub fn evaluate_post(object: &Value, author: &Value) -> Result<(), Refusal> {
    evaluate_post_in(object, author, ConsentContext::NONE)
}

/// May this object be indexed as a post by `author`, given what `context` says about the
/// author?
pub fn evaluate_post_in(
    object: &Value,
    author: &Value,
    context: ConsentContext,
) -> Result<(), Refusal> {
    if has_type(object, &["Tombstone"]) {
        return Err(Refusal::Gone);
    }
    if !has_type(object, &POST_TYPES) {
        return Err(Refusal::UnsupportedType);
    }
    let Some(object_id) = id_of(object) else {
        return Err(Refusal::MissingId);
    };
    if !is_public(object) {
        return Err(Refusal::NotPublic);
    }
    let authors = link_ids(object.get("attributedTo"));
    let [attributed_to] = authors.as_slice() else {
        return Err(Refusal::MissingAuthor);
    };
    evaluate_account_in(author, context)?;
    let Some(author_id) = id_of(author) else {
        return Err(Refusal::MissingId);
    };
    if *attributed_to != author_id {
        return Err(Refusal::AuthorMismatch);
    }
    if origin(object_id).is_none() || origin(object_id) != origin(author_id) {
        return Err(Refusal::OriginMismatch);
    }
    if !flag(author, "indexable", context) {
        return Err(Refusal::NotIndexable);
    }
    Ok(())
}
