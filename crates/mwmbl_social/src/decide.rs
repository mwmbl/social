//! What a fetch means for the index. A pure function of what was queued and what came back,
//! table-tested like consent, because a mistake here keeps content someone withdrew.
//!
//! Rules:
//! - Found and consented: index it. A revalidation that carried the stored record replaces
//!   that record; anything else upserts (delete the old version by URI, then add).
//! - Gone, or found but refused by consent: remove it. Lifecycle announcements also tombstone
//!   (PLAN.md, Deletion), as does any delete without the stored record in hand, since the URI
//!   lookup may miss.
//! - An author who is no longer `indexable` gets an author tombstone; one who is indexable again
//!   has it cleared. An author who is no longer `discoverable` also loses their account record.
//! - Transient failures (backoff, rate limits, network errors) retry, and so does a refused
//!   signed fetch until it has failed `REFUSED_ATTEMPTS` times: then we can't confirm consent,
//!   and we remove.

use apub::consent::{Refusal, evaluate_account, evaluate_post};
use apub::objects::{parse_actor, parse_post};
use serde_json::Value;
use social_index::{SocialAccount, SocialPost};
use store::FetchReason;

use crate::records::{account_record, post_record};

/// After this many 401/403 answers in a row we stop trying and treat it as not public.
pub const REFUSED_ATTEMPTS: i64 = 3;

/// The result of one fetch, reduced to what the decision needs.
#[derive(Debug, Clone, PartialEq)]
pub enum Fetched {
    Found(Value),
    Gone,
    Refused,
    Transient(String),
}

/// A change to make, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    IndexPost {
        record: SocialPost,
        replaces: Option<SocialPost>,
    },
    RemovePost {
        uri: String,
        record: Option<SocialPost>,
        tombstone: bool,
    },
    IndexAccount {
        record: SocialAccount,
        replaces: Option<SocialAccount>,
    },
    RemoveAccount {
        uri: String,
        record: Option<SocialAccount>,
        tombstone: bool,
    },
    AddAuthorTombstone {
        actor_uri: String,
    },
    ClearAuthorTombstone {
        actor_uri: String,
    },
    Retry {
        reason: String,
    },
}

/// What the stored record that a revalidation carries is.
#[derive(Debug, Clone, PartialEq)]
pub enum Carried {
    Post(SocialPost),
    Account(SocialAccount),
}

/// Whether a JSON object looks like a post rather than an actor, for URIs queued without a
/// kind hint.
#[must_use]
pub fn looks_like_post(value: &Value) -> bool {
    let post_types = ["Note", "Article", "Page", "Question"];
    match value.get("type") {
        Some(Value::String(t)) => post_types.contains(&t.as_str()),
        Some(Value::Array(ts)) => ts
            .iter()
            .any(|t| t.as_str().is_some_and(|t| post_types.contains(&t))),
        _ => false,
    }
}

/// The actor a post names in `attributedTo`, so the caller knows what to fetch.
#[must_use]
pub fn author_of(object: &Value) -> Option<String> {
    parse_post(object).ok().map(|p| p.author_uri)
}

fn retry_or_remove_post(
    uri: &str,
    reason: FetchReason,
    attempts: i64,
    carried: Option<SocialPost>,
) -> Vec<Action> {
    if attempts + 1 < REFUSED_ATTEMPTS {
        return vec![Action::Retry {
            reason: format!("fetch of {uri} refused"),
        }];
    }
    remove_post(uri, reason, carried)
}

fn remove_post(uri: &str, reason: FetchReason, carried: Option<SocialPost>) -> Vec<Action> {
    let tombstone = reason == FetchReason::Announce || carried.is_none();
    vec![Action::RemovePost {
        uri: uri.to_string(),
        record: carried,
        tombstone,
    }]
}

fn remove_account(uri: &str, reason: FetchReason, carried: Option<SocialAccount>) -> Vec<Action> {
    let tombstone = reason == FetchReason::Announce || carried.is_none();
    vec![
        Action::RemoveAccount {
            uri: uri.to_string(),
            record: carried,
            tombstone,
        },
        Action::AddAuthorTombstone {
            actor_uri: uri.to_string(),
        },
    ]
}

/// What an actor we fetched means for the accounts index and for its posts.
fn account_actions(
    actor_json: &Value,
    carried: Option<SocialAccount>,
    reason: FetchReason,
    now: i64,
) -> Vec<Action> {
    let parsed = parse_actor(actor_json);
    let Some(uri) = actor_json
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return vec![Action::Retry {
            reason: "actor without an id".into(),
        }];
    };
    match (evaluate_account(actor_json), parsed) {
        (Ok(()), Ok(actor)) => {
            let record = account_record(&actor, carried.as_ref(), now);
            let tombstone_change = if actor.indexable {
                Action::ClearAuthorTombstone { actor_uri: uri }
            } else {
                Action::AddAuthorTombstone { actor_uri: uri }
            };
            vec![
                Action::IndexAccount {
                    record,
                    replaces: carried,
                },
                tombstone_change,
            ]
        }
        _ => remove_account(&uri, reason, carried),
    }
}

/// Decide what a fetched post means. `author` is the fetch of the post's `attributedTo`,
/// which the caller makes only when the post itself was found.
#[must_use]
pub fn decide_post(
    uri: &str,
    reason: FetchReason,
    attempts: i64,
    carried: Option<SocialPost>,
    object: &Fetched,
    author: Option<&Fetched>,
    now: i64,
) -> Vec<Action> {
    let json = match object {
        Fetched::Found(json) => json,
        Fetched::Gone => return remove_post(uri, reason, carried),
        Fetched::Refused => return retry_or_remove_post(uri, reason, attempts, carried),
        Fetched::Transient(e) => return vec![Action::Retry { reason: e.clone() }],
    };
    // A fetched object served under another id is not the object we were asked about.
    if json.get("id").and_then(Value::as_str) != Some(uri) {
        return remove_post(uri, reason, carried);
    }
    let author_json = match author {
        Some(Fetched::Found(a)) => a,
        Some(Fetched::Transient(e)) => return vec![Action::Retry { reason: e.clone() }],
        Some(Fetched::Refused) => return retry_or_remove_post(uri, reason, attempts, carried),
        // A post whose author is gone, or that names no fetchable author, can't be consented.
        Some(Fetched::Gone) | None => return remove_post(uri, reason, carried),
    };
    match (evaluate_post(json, author_json), parse_post(json)) {
        (Ok(()), Ok(post)) => {
            let mut actions = vec![Action::IndexPost {
                record: post_record(&post, now),
                replaces: carried,
            }];
            // The author we fetched anyway is worth indexing too, if they're discoverable.
            if evaluate_account(author_json).is_ok() {
                actions.extend(account_actions(
                    author_json,
                    None,
                    FetchReason::Backfill,
                    now,
                ));
            }
            actions
        }
        (Err(Refusal::NotIndexable | Refusal::NotDiscoverable), _) => {
            let mut actions = remove_post(uri, reason, carried);
            actions.extend(account_actions(
                author_json,
                None,
                FetchReason::Backfill,
                now,
            ));
            actions
        }
        _ => remove_post(uri, reason, carried),
    }
}

/// Decide what a fetched actor means.
#[must_use]
pub fn decide_account(
    uri: &str,
    reason: FetchReason,
    attempts: i64,
    carried: Option<SocialAccount>,
    actor: &Fetched,
    now: i64,
) -> Vec<Action> {
    match actor {
        Fetched::Found(json) if json.get("id").and_then(Value::as_str) == Some(uri) => {
            account_actions(json, carried, reason, now)
        }
        Fetched::Refused if attempts + 1 < REFUSED_ATTEMPTS => {
            vec![Action::Retry {
                reason: format!("fetch of {uri} refused"),
            }]
        }
        Fetched::Found(_) | Fetched::Gone | Fetched::Refused => {
            remove_account(uri, reason, carried)
        }
        Fetched::Transient(e) => vec![Action::Retry { reason: e.clone() }],
    }
}

#[cfg(test)]
mod tests {
    use FetchReason::{Announce, Backfill, Revalidate};
    use serde_json::json;

    use super::*;

    const NOW: i64 = 1_800_000_000;
    const POST: &str = "https://m.example/users/a/statuses/1";
    const AUTHOR: &str = "https://m.example/users/a";
    const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
    const FOLLOWERS: &str = "https://m.example/users/a/followers";

    fn author(discoverable: bool, indexable: bool) -> Fetched {
        Fetched::Found(
            json!({"id": AUTHOR, "type": "Person", "preferredUsername": "a",
            "name": "A", "inbox": format!("{AUTHOR}/inbox"),
            "discoverable": discoverable, "indexable": indexable}),
        )
    }

    fn note(to: &str) -> Fetched {
        Fetched::Found(
            json!({"id": POST, "type": "Note", "attributedTo": AUTHOR, "to": [to],
            "content": "<p>hello garden</p>", "published": "2026-01-01T00:00:00Z"}),
        )
    }

    fn transient() -> Fetched {
        Fetched::Transient("timeout".into())
    }

    fn stored_post() -> SocialPost {
        SocialPost {
            uri: POST.into(),
            author_uri: AUTHOR.into(),
            extract: "old text".into(),
            score: 1.0,
            term: String::new(),
            published: 1,
            checked: 2,
            title: None,
            language: None,
        }
    }

    fn stored_account() -> SocialAccount {
        SocialAccount {
            uri: AUTHOR.into(),
            acct: "a@m.example".into(),
            display_name: "A".into(),
            summary: String::new(),
            score: 1.0,
            term: String::new(),
            checked: 1,
            languages: None,
        }
    }

    fn kind(a: &Action) -> &'static str {
        match a {
            Action::IndexPost { replaces: None, .. } => "index post",
            Action::IndexPost {
                replaces: Some(_), ..
            } => "replace post",
            Action::RemovePost {
                tombstone: true, ..
            } => "remove post + tombstone",
            Action::RemovePost {
                tombstone: false, ..
            } => "remove post",
            Action::IndexAccount { .. } => "index account",
            Action::RemoveAccount {
                tombstone: true, ..
            } => "remove account + tombstone",
            Action::RemoveAccount {
                tombstone: false, ..
            } => "remove account",
            Action::AddAuthorTombstone { .. } => "author tombstone",
            Action::ClearAuthorTombstone { .. } => "clear author tombstone",
            Action::Retry { .. } => "retry",
        }
    }

    struct PostCase {
        name: &'static str,
        reason: FetchReason,
        attempts: i64,
        carried: Option<SocialPost>,
        object: Fetched,
        author: Option<Fetched>,
        expected: &'static [&'static str],
    }

    fn post_case(
        name: &'static str,
        reason: FetchReason,
        carried: Option<SocialPost>,
        object: Fetched,
        author: Option<Fetched>,
        expected: &'static [&'static str],
    ) -> PostCase {
        PostCase {
            name,
            reason,
            attempts: 0,
            carried,
            object,
            author,
            expected,
        }
    }

    const INDEXED: &[&str] = &["index post", "index account", "clear author tombstone"];
    const REPLACED: &[&str] = &["replace post", "index account", "clear author tombstone"];
    const GONE_TOMBSTONED: &[&str] = &["remove post + tombstone"];
    const REMOVED: &[&str] = &["remove post"];
    const RETRY: &[&str] = &["retry"];

    fn post_cases() -> Vec<PostCase> {
        vec![
            post_case(
                "new public post, opted-in author",
                Announce,
                None,
                note(PUBLIC),
                Some(author(true, true)),
                INDEXED,
            ),
            post_case(
                "revalidation, still fine",
                Revalidate,
                Some(stored_post()),
                note(PUBLIC),
                Some(author(true, true)),
                REPLACED,
            ),
            post_case(
                "lifecycle delete",
                Announce,
                None,
                Fetched::Gone,
                None,
                GONE_TOMBSTONED,
            ),
            post_case(
                "revalidation finds it gone",
                Revalidate,
                Some(stored_post()),
                Fetched::Gone,
                None,
                REMOVED,
            ),
            post_case(
                "backfill of something gone",
                Backfill,
                None,
                Fetched::Gone,
                None,
                GONE_TOMBSTONED,
            ),
            post_case(
                "edit made it unlisted",
                Announce,
                None,
                note(FOLLOWERS),
                Some(author(true, true)),
                GONE_TOMBSTONED,
            ),
            post_case(
                "revalidation: now unlisted",
                Revalidate,
                Some(stored_post()),
                note(FOLLOWERS),
                Some(author(true, true)),
                REMOVED,
            ),
            post_case(
                "author turned off indexable",
                Revalidate,
                Some(stored_post()),
                note(PUBLIC),
                Some(author(true, false)),
                &["remove post", "index account", "author tombstone"],
            ),
            post_case(
                "author turned off discoverable",
                Revalidate,
                Some(stored_post()),
                note(PUBLIC),
                Some(author(false, true)),
                &[
                    "remove post",
                    "remove account + tombstone",
                    "author tombstone",
                ],
            ),
        ]
    }

    fn more_post_cases() -> Vec<PostCase> {
        let other_id = Fetched::Found(json!({"id": "https://m.example/users/a/statuses/2",
            "type": "Note", "attributedTo": AUTHOR, "to": [PUBLIC], "content": "x"}));
        let boost = Fetched::Found(json!({"id": POST, "type": "Announce", "to": [PUBLIC]}));
        let mut refused_last = post_case(
            "refused, last try: can't confirm consent",
            Revalidate,
            Some(stored_post()),
            Fetched::Refused,
            None,
            REMOVED,
        );
        refused_last.attempts = REFUSED_ATTEMPTS - 1;
        vec![
            post_case(
                "author gone",
                Revalidate,
                Some(stored_post()),
                note(PUBLIC),
                Some(Fetched::Gone),
                REMOVED,
            ),
            post_case(
                "author fetch failed",
                Revalidate,
                Some(stored_post()),
                note(PUBLIC),
                Some(transient()),
                RETRY,
            ),
            post_case("network error", Announce, None, transient(), None, RETRY),
            post_case(
                "refused, first try",
                Revalidate,
                Some(stored_post()),
                Fetched::Refused,
                None,
                RETRY,
            ),
            refused_last,
            post_case(
                "served under another id",
                Announce,
                None,
                other_id,
                Some(author(true, true)),
                GONE_TOMBSTONED,
            ),
            post_case(
                "not a post",
                Announce,
                None,
                boost,
                Some(author(true, true)),
                GONE_TOMBSTONED,
            ),
            post_case(
                "no author fetched",
                Announce,
                None,
                note(PUBLIC),
                None,
                GONE_TOMBSTONED,
            ),
        ]
    }

    #[test]
    fn posts() {
        let mut failures = Vec::new();
        for c in post_cases().into_iter().chain(more_post_cases()) {
            let got = decide_post(
                POST,
                c.reason,
                c.attempts,
                c.carried,
                &c.object,
                c.author.as_ref(),
                NOW,
            );
            let got: Vec<_> = got.iter().map(kind).collect();
            if got != c.expected {
                failures.push(format!(
                    "{}: expected {:?}, got {got:?}",
                    c.name, c.expected
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn the_indexed_record_is_built_from_the_fetched_post() {
        let got = decide_post(
            POST,
            Revalidate,
            0,
            Some(stored_post()),
            &note(PUBLIC),
            Some(&author(true, true)),
            NOW,
        );
        let Some(Action::IndexPost { record, replaces }) = got.first() else {
            panic!("{got:?}")
        };
        assert_eq!(record.extract, "hello garden");
        assert_eq!(record.checked, NOW);
        assert_eq!(
            replaces.as_ref().map(|r| r.extract.as_str()),
            Some("old text")
        );
    }

    type AccountCase = (
        &'static str,
        FetchReason,
        i64,
        Option<SocialAccount>,
        Fetched,
        &'static [&'static str],
    );

    #[test]
    fn accounts() {
        let other = Fetched::Found(json!({"id": "https://m.example/users/b", "type": "Person",
            "discoverable": true}));
        let removed_new: &[&str] = &["remove account + tombstone", "author tombstone"];
        let cases: Vec<AccountCase> = vec![
            (
                "opted in",
                Announce,
                0,
                None,
                author(true, true),
                &["index account", "clear author tombstone"],
            ),
            (
                "discoverable, not indexable",
                Announce,
                0,
                None,
                author(true, false),
                &["index account", "author tombstone"],
            ),
            (
                "not discoverable",
                Announce,
                0,
                None,
                author(false, true),
                removed_new,
            ),
            (
                "revalidation: no longer discoverable",
                Revalidate,
                0,
                Some(stored_account()),
                author(false, false),
                &["remove account", "author tombstone"],
            ),
            ("deleted", Announce, 0, None, Fetched::Gone, removed_new),
            (
                "served under another id",
                Announce,
                0,
                None,
                other,
                removed_new,
            ),
            (
                "refused once",
                Revalidate,
                0,
                Some(stored_account()),
                Fetched::Refused,
                RETRY,
            ),
            (
                "refused for good",
                Revalidate,
                REFUSED_ATTEMPTS,
                Some(stored_account()),
                Fetched::Refused,
                &["remove account", "author tombstone"],
            ),
            ("transient", Announce, 0, None, transient(), RETRY),
        ];
        let mut failures = Vec::new();
        for (name, reason, attempts, carried, actor, expected) in cases {
            let got = decide_account(AUTHOR, reason, attempts, carried, &actor, NOW);
            let got: Vec<_> = got.iter().map(kind).collect();
            if got != expected {
                failures.push(format!("{name}: expected {expected:?}, got {got:?}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn kind_sniffing() {
        let Fetched::Found(n) = note(PUBLIC) else {
            panic!()
        };
        let Fetched::Found(a) = author(true, true) else {
            panic!()
        };
        assert!(looks_like_post(&n));
        assert!(!looks_like_post(&a));
        assert_eq!(author_of(&n).as_deref(), Some(AUTHOR));
    }
}
