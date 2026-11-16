//! Outbox backfill (PLAN.md Stage 3): when one of our follows is accepted, the actor's recent
//! posts are found in its `outbox` and queued (reason `backfill`) to be fetched from their
//! origin and judged like any other object.
//!
//! The outbox JSON is never indexed or stored (CLAUDE.md principle 4): only object ids are
//! taken from it, by [`object_ids`], a pure function. Lemmy community outboxes hold
//! `Announce{Create{Page|Note}}`; Mastodon's hold `Create{Note}` and `Announce` boosts, which
//! are skipped, as the inbox skips them. Ids must be on the actor's origin, or anywhere for a
//! `Group` (a community relays its members' posts from other instances), as in the inbox.
//!
//! [`backfill`] reads the collection and then at most `max_pages` pages (`first`, then
//! `next`), each on the actor's origin and fetched with a signed `GET`. It stops when the
//! queue defers a backfill item (it is full), or at the first page it can't fetch.

use std::collections::HashSet;

use apub::fetch::{FetchOutcome, Fetcher};
use apub::inbox::same_origin;
use serde_json::Value;
use store::{EnqueueOutcome, StoreError};

use crate::activities::{POST_TYPES, activity_actor, id_of, is_any};

/// The items of a collection or collection page: `orderedItems`, else `items`.
fn items(page: &Value) -> &[Value] {
    page.get("orderedItems")
        .or_else(|| page.get("items"))
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// The id of the post `item` (an activity or an object in `actor`'s outbox) brings, if any.
fn post_of<'v>(actor: &str, item: &'v Value, wrapped: bool) -> Option<&'v str> {
    if is_any(item, POST_TYPES) {
        return id_of(item);
    }
    let object = item.get("object")?;
    if is_any(item, &["Create"]) {
        // A Create of ours, or (inside a community's Announce) one of its members'.
        if !wrapped && activity_actor(item).is_some_and(|a| a != actor) {
            return None;
        }
        if object.is_object() && !is_any(object, POST_TYPES) {
            return None;
        }
        return id_of(object);
    }
    // An Announce wrapping a Create is a community relaying a post. Announcing an object or a
    // URI is a boost of someone else's post, which is not its author's consent.
    if !wrapped && is_any(item, &["Announce"]) && is_any(object, &["Create"]) {
        return post_of(actor, object, true);
    }
    None
}

/// The post ids on one outbox page (or a collection with inline items) of `actor`, in order,
/// without duplicates. `is_group`: the actor is a `Group`, whose posts may be on any origin.
#[must_use]
pub fn object_ids(actor: &str, is_group: bool, page: &Value) -> Vec<String> {
    let mut seen = HashSet::new();
    items(page)
        .iter()
        .filter_map(|item| post_of(actor, item, false))
        .filter(|id| is_group || same_origin(id, actor))
        .filter(|id| seen.insert(*id))
        .map(str::to_owned)
        .collect()
}

/// What a backfill did, for logs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillReport {
    /// Pages whose items were read (a collection with inline items counts as one).
    pub pages: usize,
    /// Ids handed to the queue (including ones queued already).
    pub queued: usize,
    /// Why it stopped early, if it did.
    pub stopped: Option<String>,
}

/// Where the next page comes from.
enum Next {
    Fetch(String),
    Embedded(Value),
}

fn next_of(value: Option<&Value>) -> Option<Next> {
    match value? {
        Value::String(uri) => Some(Next::Fetch(uri.clone())),
        page @ Value::Object(_)
            if page.get("orderedItems").is_some() || page.get("items").is_some() =>
        {
            Some(Next::Embedded(page.clone()))
        }
        page @ Value::Object(_) => id_of(page).map(|id| Next::Fetch(id.to_owned())),
        _ => None,
    }
}

async fn fetch_page(fetcher: &Fetcher, actor: &str, uri: &str) -> Result<Value, String> {
    if !same_origin(uri, actor) {
        return Err(format!("page {uri} is not on the actor's origin"));
    }
    match fetcher.fetch(uri).await {
        FetchOutcome::Found { json, .. } => Ok(json),
        FetchOutcome::Refused { status } => Err(format!("{uri} is not served to us ({status})")),
        FetchOutcome::Gone => Err(format!("{uri} is not served (gone)")),
        other => Err(format!("{uri} could not be fetched: {other:?}")),
    }
}

/// Reads `actor`'s outbox at `outbox` and hands each post id to `enqueue`, reading at most
/// `max_pages` pages.
pub async fn backfill(
    fetcher: &Fetcher,
    actor: &str,
    is_group: bool,
    outbox: &str,
    max_pages: usize,
    mut enqueue: impl FnMut(&str) -> Result<EnqueueOutcome, StoreError>,
) -> BackfillReport {
    let mut report = BackfillReport::default();
    if max_pages == 0 {
        return report;
    }
    let collection = match fetch_page(fetcher, actor, outbox).await {
        Ok(collection) => collection,
        Err(reason) => {
            report.stopped = Some(reason);
            return report;
        }
    };
    let mut seen_pages = HashSet::from([outbox.to_owned()]);
    let mut next = if items(&collection).is_empty() {
        next_of(collection.get("first"))
    } else {
        Some(Next::Embedded(collection))
    };
    while report.pages < max_pages {
        let page = match next.take() {
            None => break,
            Some(Next::Embedded(page)) => page,
            Some(Next::Fetch(uri)) => {
                if !seen_pages.insert(uri.clone()) {
                    break;
                }
                match fetch_page(fetcher, actor, &uri).await {
                    Ok(page) => page,
                    Err(reason) => {
                        report.stopped = Some(reason);
                        break;
                    }
                }
            }
        };
        report.pages += 1;
        for id in object_ids(actor, is_group, &page) {
            match enqueue(&id) {
                Ok(EnqueueOutcome::Deferred) => {
                    report.stopped = Some("the fetch queue is full".to_owned());
                    return report;
                }
                Ok(_) => report.queued += 1,
                Err(e) => {
                    report.stopped = Some(e.to_string());
                    return report;
                }
            }
        }
        next = next_of(page.get("next"));
    }
    report
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

    #[test]
    fn object_ids_from_outbox_pages() {
        let community = "https://l.example/c/rust";
        let mastodon = "https://m.example/users/alice";
        // A Lemmy community outbox: inline items, Announce of Create of Page (and a comment).
        let lemmy = json!({
            "type": "OrderedCollection",
            "id": format!("{community}/outbox"),
            "totalItems": 4,
            "orderedItems": [
                {"type": "Announce", "actor": community, "to": [PUBLIC],
                 "object": {"type": "Create", "actor": "https://l.example/u/carol",
                     "object": {"type": "Page", "id": "https://l.example/post/1", "name": "x"}}},
                // A member on another instance: a community relays it.
                {"type": "Announce", "actor": community,
                 "object": {"type": "Create", "actor": "https://o.example/u/dan",
                     "object": {"type": "Page", "id": "https://o.example/post/9"}}},
                {"type": "Announce", "actor": community,
                 "object": {"type": "Create", "actor": "https://l.example/u/carol",
                     "object": "https://l.example/comment/3"}},
                // The same post again, and things that bring no post.
                {"type": "Announce", "actor": community,
                 "object": {"type": "Create", "object": {"type": "Page", "id": "https://l.example/post/1"}}},
                {"type": "Announce", "actor": community,
                 "object": {"type": "Delete", "object": "https://l.example/post/2"}},
                {"type": "Announce", "actor": community, "object": "https://l.example/post/4"},
                "https://l.example/activities/5",
            ],
        });
        // A Mastodon outbox page: Create of Note, and boosts.
        let mastodon_page = json!({
            "type": "OrderedCollectionPage",
            "id": format!("{mastodon}/outbox?page=true"),
            "next": format!("{mastodon}/outbox?max_id=1&page=true"),
            "orderedItems": [
                {"type": "Create", "actor": mastodon,
                 "object": {"type": "Note", "id": "https://m.example/users/alice/statuses/1"}},
                {"type": "Announce", "actor": mastodon,
                 "object": "https://x.example/users/bob/statuses/2"},
                {"type": "Announce", "actor": mastodon,
                 "object": {"type": "Note", "id": "https://x.example/users/bob/statuses/3"}},
                // Someone else's Create, a post elsewhere, and a non-post object.
                {"type": "Create", "actor": "https://x.example/users/bob",
                 "object": {"type": "Note", "id": "https://x.example/users/bob/statuses/4"}},
                {"type": "Create", "actor": mastodon,
                 "object": {"type": "Note", "id": "https://x.example/statuses/5"}},
                {"type": "Create", "actor": mastodon,
                 "object": {"type": "Person", "id": "https://m.example/users/alice/x"}},
                {"type": "Create", "actor": mastodon,
                 "object": "https://m.example/users/alice/statuses/6"},
                // A bare object.
                {"type": "Note", "id": "https://m.example/users/alice/statuses/7"},
                {"type": "Announce", "actor": mastodon,
                 "object": {"type": "Announce", "object": {"type": "Create",
                     "object": {"type": "Note", "id": "https://m.example/users/alice/statuses/8"}}}},
            ],
        });
        for (name, actor, is_group, page, want) in [
            (
                "lemmy community",
                community,
                true,
                &lemmy,
                vec![
                    "https://l.example/post/1",
                    "https://o.example/post/9",
                    "https://l.example/comment/3",
                ],
            ),
            (
                // Were the same page an account's, only its origin counts.
                "lemmy-shaped, not a group",
                community,
                false,
                &lemmy,
                vec!["https://l.example/post/1", "https://l.example/comment/3"],
            ),
            (
                "mastodon page",
                mastodon,
                false,
                &mastodon_page,
                vec![
                    "https://m.example/users/alice/statuses/1",
                    "https://m.example/users/alice/statuses/6",
                    "https://m.example/users/alice/statuses/7",
                ],
            ),
            (
                "unordered items",
                mastodon,
                false,
                &json!({"items": [{"type": "Create", "actor": mastodon,
                    "object": {"type": "Article", "id": "https://m.example/a/1"}}]}),
                vec!["https://m.example/a/1"],
            ),
            (
                "a collection without items",
                mastodon,
                false,
                &json!({"type": "OrderedCollection", "first": "https://m.example/o?page=1"}),
                vec![],
            ),
            ("not an object", mastodon, false, &json!("x"), vec![]),
        ] {
            assert_eq!(object_ids(actor, is_group, page), want, "{name}");
        }
    }

    #[test]
    fn next_pages() {
        assert!(
            matches!(next_of(Some(&json!("https://m.example/o?p=1"))), Some(Next::Fetch(u)) if u == "https://m.example/o?p=1")
        );
        assert!(matches!(
            next_of(Some(
                &json!({"id": "https://m.example/o?p=1", "type": "OrderedCollectionPage"})
            )),
            Some(Next::Fetch(_))
        ));
        assert!(matches!(
            next_of(Some(&json!({"orderedItems": []}))),
            Some(Next::Embedded(_))
        ));
        assert!(next_of(None).is_none());
        assert!(next_of(Some(&json!(3))).is_none());
    }
}
