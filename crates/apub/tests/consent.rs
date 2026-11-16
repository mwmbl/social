//! Consent evaluation, table-driven over JSON shaped like what Mastodon, GoToSocial, Misskey
//! Pleroma and Lemmy serve. Every rule in `apub::consent` has a case here; add one for any change.

use apub::consent::{
    ConsentContext, Refusal, evaluate_account, evaluate_account_in, evaluate_post,
    evaluate_post_in, posts_consented,
};
use serde_json::{Value, json};

fn load(name: &str) -> Value {
    let path = format!("{}/tests/consent/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

/// A copy of `base` with top-level fields set (or removed, for `null` in `remove`).
fn with(base: &Value, set: &Value, remove: &[&str]) -> Value {
    let mut v = base.clone();
    let obj = v.as_object_mut().unwrap();
    for (k, val) in set.as_object().unwrap() {
        obj.insert(k.clone(), val.clone());
    }
    for k in remove {
        obj.remove(*k);
    }
    v
}

const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

#[test]
fn posts() {
    let m_actor = load("mastodon_actor");
    let m_note = load("mastodon_note");
    let g_actor = load("gotosocial_actor");
    let g_note = load("gotosocial_note");
    let k_actor = load("misskey_actor");
    let k_note = load("misskey_note");
    let p_actor = load("pleroma_actor");
    let p_note = load("pleroma_note");
    let followers = "https://mastodon.example/users/alice/followers";

    let cases: Vec<(&str, Value, Value, Result<(), Refusal>)> = vec![
        // Each server's public post by an opted-in author.
        ("mastodon public", m_note.clone(), m_actor.clone(), Ok(())),
        (
            "gotosocial public, `to` as a bare string",
            g_note.clone(),
            g_actor.clone(),
            Ok(()),
        ),
        ("misskey public", k_note.clone(), k_actor.clone(), Ok(())),
        // Pleroma has no `indexable`: FEP-5feb consent is absent, so no.
        (
            "pleroma, author without indexable",
            p_note.clone(),
            p_actor.clone(),
            Err(Refusal::NotIndexable),
        ),
        (
            "pleroma, author opted in",
            p_note.clone(),
            with(&p_actor, &json!({"indexable": true}), &[]),
            Ok(()),
        ),
        // Visibility.
        (
            "unlisted: public only in cc",
            with(&m_note, &json!({"to": [followers], "cc": [PUBLIC]}), &[]),
            m_actor.clone(),
            Err(Refusal::NotPublic),
        ),
        (
            "followers only",
            with(&m_note, &json!({"to": [followers], "cc": []}), &[]),
            m_actor.clone(),
            Err(Refusal::NotPublic),
        ),
        (
            "direct",
            with(
                &m_note,
                &json!({"to": ["https://other.example/users/x"], "cc": []}),
                &[],
            ),
            m_actor.clone(),
            Err(Refusal::NotPublic),
        ),
        (
            "no `to` at all",
            with(&m_note, &json!({}), &["to"]),
            m_actor.clone(),
            Err(Refusal::NotPublic),
        ),
        (
            "public only in bto/audience doesn't count",
            with(
                &m_note,
                &json!({"to": [followers], "bto": [PUBLIC], "audience": PUBLIC}),
                &[],
            ),
            m_actor.clone(),
            Err(Refusal::NotPublic),
        ),
        (
            "compact as:Public",
            with(&m_note, &json!({"to": ["as:Public"]}), &[]),
            m_actor.clone(),
            Ok(()),
        ),
        (
            "compact Public",
            with(&m_note, &json!({"to": "Public"}), &[]),
            m_actor.clone(),
            Ok(()),
        ),
        (
            "public as a link object",
            with(
                &m_note,
                &json!({"to": [{"type": "Collection", "id": PUBLIC}]}),
                &[],
            ),
            m_actor.clone(),
            Ok(()),
        ),
        (
            "public among other recipients",
            with(&m_note, &json!({"to": [followers, PUBLIC]}), &[]),
            m_actor.clone(),
            Ok(()),
        ),
        (
            "near miss on the public IRI",
            with(
                &m_note,
                &json!({"to": ["http://www.w3.org/ns/activitystreams#Public2"]}),
                &[],
            ),
            m_actor.clone(),
            Err(Refusal::NotPublic),
        ),
        // Author opt-in.
        (
            "author not indexable",
            m_note.clone(),
            with(&m_actor, &json!({"indexable": false}), &[]),
            Err(Refusal::NotIndexable),
        ),
        (
            "author from before Mastodon had indexable",
            m_note.clone(),
            with(&m_actor, &json!({}), &["indexable"]),
            Err(Refusal::NotIndexable),
        ),
        (
            "author not discoverable",
            m_note.clone(),
            with(&m_actor, &json!({"discoverable": false}), &[]),
            Err(Refusal::NotDiscoverable),
        ),
        (
            "indexable as a string is not true",
            m_note.clone(),
            with(&m_actor, &json!({"indexable": "true"}), &[]),
            Err(Refusal::NotIndexable),
        ),
        (
            "discoverable null",
            m_note.clone(),
            with(&m_actor, &json!({"discoverable": null}), &[]),
            Err(Refusal::NotDiscoverable),
        ),
        // Authorship and origin.
        (
            "attributedTo is someone else",
            with(
                &m_note,
                &json!({"attributedTo": "https://mastodon.example/users/mallory"}),
                &[],
            ),
            m_actor.clone(),
            Err(Refusal::AuthorMismatch),
        ),
        (
            "attributedTo as a one-element list of objects",
            with(
                &m_note,
                &json!({"attributedTo": [{"type": "Person", "id": "https://mastodon.example/users/alice"}]}),
                &[],
            ),
            m_actor.clone(),
            Ok(()),
        ),
        (
            "two authors",
            with(
                &m_note,
                &json!({"attributedTo": ["https://mastodon.example/users/alice", "https://mastodon.example/users/bob"]}),
                &[],
            ),
            m_actor.clone(),
            Err(Refusal::MissingAuthor),
        ),
        (
            "no author",
            with(&m_note, &json!({}), &["attributedTo"]),
            m_actor.clone(),
            Err(Refusal::MissingAuthor),
        ),
        (
            "object on another host than its author",
            with(&m_note, &json!({"id": "https://evil.example/notes/1"}), &[]),
            m_actor.clone(),
            Err(Refusal::OriginMismatch),
        ),
        (
            "same host, different scheme",
            with(
                &m_note,
                &json!({"id": "http://mastodon.example/users/alice/statuses/1001"}),
                &[],
            ),
            m_actor.clone(),
            Err(Refusal::OriginMismatch),
        ),
        // Types.
        (
            "tombstone",
            json!({"id": "https://mastodon.example/users/alice/statuses/1001", "type": "Tombstone"}),
            m_actor.clone(),
            Err(Refusal::Gone),
        ),
        (
            "an Announce isn't a post",
            with(&m_note, &json!({"type": "Announce"}), &[]),
            m_actor.clone(),
            Err(Refusal::UnsupportedType),
        ),
        (
            "Video unsupported",
            with(&m_note, &json!({"type": "Video"}), &[]),
            m_actor.clone(),
            Err(Refusal::UnsupportedType),
        ),
        (
            "no type",
            with(&m_note, &json!({}), &["type"]),
            m_actor.clone(),
            Err(Refusal::UnsupportedType),
        ),
        (
            "Article",
            with(&m_note, &json!({"type": "Article", "name": "Title"}), &[]),
            m_actor.clone(),
            Ok(()),
        ),
        (
            "Page",
            with(&m_note, &json!({"type": "Page"}), &[]),
            m_actor.clone(),
            Ok(()),
        ),
        (
            "Question",
            with(&m_note, &json!({"type": "Question", "oneOf": []}), &[]),
            m_actor.clone(),
            Ok(()),
        ),
        (
            "type as a list",
            with(&m_note, &json!({"type": ["Note", "Object"]}), &[]),
            m_actor.clone(),
            Ok(()),
        ),
        (
            "no id",
            with(&m_note, &json!({}), &["id"]),
            m_actor.clone(),
            Err(Refusal::MissingId),
        ),
        (
            "empty id",
            with(&m_note, &json!({"id": ""}), &[]),
            m_actor.clone(),
            Err(Refusal::MissingId),
        ),
        // A post by an actor that is gone.
        (
            "author is a tombstone",
            m_note.clone(),
            json!({"id": "https://mastodon.example/users/alice", "type": "Tombstone"}),
            Err(Refusal::Gone),
        ),
        (
            "not even an object",
            json!("https://mastodon.example/users/alice/statuses/1001"),
            m_actor.clone(),
            Err(Refusal::UnsupportedType),
        ),
    ];

    let mut failures = Vec::new();
    for (name, object, author, expected) in &cases {
        let got = evaluate_post(object, author);
        if got != *expected {
            failures.push(format!("{name}: expected {expected:?}, got {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn accounts() {
    let m = load("mastodon_actor");
    let cases: Vec<(&str, Value, Result<(), Refusal>)> = vec![
        ("mastodon opted in", m.clone(), Ok(())),
        ("gotosocial opted in", load("gotosocial_actor"), Ok(())),
        ("misskey opted in", load("misskey_actor"), Ok(())),
        ("pleroma discoverable", load("pleroma_actor"), Ok(())),
        // Account search needs only discoverable; indexable is for post full text.
        (
            "discoverable but not indexable",
            with(&m, &json!({"indexable": false}), &[]),
            Ok(()),
        ),
        (
            "not discoverable",
            with(&m, &json!({"discoverable": false}), &[]),
            Err(Refusal::NotDiscoverable),
        ),
        (
            "discoverable missing",
            with(&m, &json!({}), &["discoverable"]),
            Err(Refusal::NotDiscoverable),
        ),
        (
            "discoverable as a string",
            with(&m, &json!({"discoverable": "true"}), &[]),
            Err(Refusal::NotDiscoverable),
        ),
        ("bot", with(&m, &json!({"type": "Service"}), &[]), Ok(())),
        ("group", with(&m, &json!({"type": "Group"}), &[]), Ok(())),
        (
            "a Note is not an account",
            with(&m, &json!({"type": "Note"}), &[]),
            Err(Refusal::UnsupportedType),
        ),
        (
            "deleted account",
            json!({"id": "https://mastodon.example/users/alice", "type": "Tombstone"}),
            Err(Refusal::Gone),
        ),
        (
            "no id",
            with(&m, &json!({}), &["id"]),
            Err(Refusal::MissingId),
        ),
    ];
    let mut failures = Vec::new();
    for (name, actor, expected) in &cases {
        let got = evaluate_account(actor);
        if got != *expected {
            failures.push(format!("{name}: expected {expected:?}, got {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

const LEMMY: ConsentContext = ConsentContext {
    author_on_lemmy: true,
    follows_us: false,
};
const FOLLOWS: ConsentContext = ConsentContext {
    author_on_lemmy: false,
    follows_us: true,
};
const NONE: ConsentContext = ConsentContext::NONE;

/// Name, object, author, context, expected.
type ContextCase = (
    &'static str,
    Value,
    Value,
    ConsentContext,
    Result<(), Refusal>,
);

/// Stand-ins for flags the author doesn't carry: a Lemmy host, or following our actor
/// (PLAN.md Stage 3). An explicit value that isn't `true` always wins.
#[test]
fn posts_with_context() {
    let l_person = load("lemmy_person");
    let l_page = load("lemmy_page");
    let l_comment = load("lemmy_comment");
    let m_actor = load("mastodon_actor");
    let m_note = load("mastodon_note");
    let g_actor = load("gotosocial_actor");
    let g_note = load("gotosocial_note");
    let p_actor = load("pleroma_actor");
    let p_note = load("pleroma_note");
    let g_bare = with(&g_actor, &json!({}), &["discoverable", "indexable"]);

    let cases: Vec<ContextCase> = vec![
        // Lemmy authors: missing flags count as yes.
        (
            "lemmy post",
            l_page.clone(),
            l_person.clone(),
            LEMMY,
            Ok(()),
        ),
        (
            "lemmy comment",
            l_comment.clone(),
            l_person.clone(),
            LEMMY,
            Ok(()),
        ),
        (
            "lemmy post judged without knowing the host is lemmy",
            l_page.clone(),
            l_person.clone(),
            NONE,
            Err(Refusal::NotDiscoverable),
        ),
        (
            "lemmy author with explicit indexable false",
            l_page.clone(),
            with(&l_person, &json!({"indexable": false}), &[]),
            LEMMY,
            Err(Refusal::NotIndexable),
        ),
        (
            "lemmy author with explicit discoverable false",
            l_comment.clone(),
            with(&l_person, &json!({"discoverable": false}), &[]),
            LEMMY,
            Err(Refusal::NotDiscoverable),
        ),
        (
            "lemmy author with discoverable null",
            l_page.clone(),
            with(&l_person, &json!({"discoverable": null}), &[]),
            LEMMY,
            Err(Refusal::NotDiscoverable),
        ),
        (
            "lemmy author with indexable as a string",
            l_page.clone(),
            with(&l_person, &json!({"indexable": "true"}), &[]),
            LEMMY,
            Err(Refusal::NotIndexable),
        ),
        // Lemmy doesn't relax visibility or authorship.
        (
            "lemmy post in a private community: not addressed to the public",
            with(
                &l_page,
                &json!({"to": ["https://lemmy.example/c/rust"]}),
                &[],
            ),
            l_person.clone(),
            LEMMY,
            Err(Refusal::NotPublic),
        ),
        (
            "lemmy comment with public only in cc",
            with(
                &l_comment,
                &json!({"to": ["https://lemmy.example/c/rust"], "cc": [PUBLIC]}),
                &[],
            ),
            l_person.clone(),
            LEMMY,
            Err(Refusal::NotPublic),
        ),
        (
            "lemmy post attributed to someone else",
            with(
                &l_page,
                &json!({"attributedTo": "https://lemmy.example/u/mallory"}),
                &[],
            ),
            l_person.clone(),
            LEMMY,
            Err(Refusal::AuthorMismatch),
        ),
        (
            "lemmy author's post hosted elsewhere",
            with(
                &l_page,
                &json!({"id": "https://other.example/post/42"}),
                &[],
            ),
            l_person.clone(),
            LEMMY,
            Err(Refusal::OriginMismatch),
        ),
        (
            "removed lemmy post",
            json!({"id": "https://lemmy.example/post/42", "type": "Tombstone"}),
            l_person.clone(),
            LEMMY,
            Err(Refusal::Gone),
        ),
        // The rule follows the author: a Mastodon user commenting in a Lemmy community is
        // judged with no stand-in (the caller passes the author's host, not the community's).
        (
            "mastodon author without indexable, in a lemmy thread",
            with(
                &m_note,
                &json!({"audience": "https://lemmy.example/c/rust", "cc": ["https://lemmy.example/c/rust"]}),
                &[],
            ),
            with(&m_actor, &json!({}), &["indexable"]),
            NONE,
            Err(Refusal::NotIndexable),
        ),
        // Following our actor stands in for missing flags only.
        (
            "follower whose software has no flags",
            g_note.clone(),
            g_bare.clone(),
            FOLLOWS,
            Ok(()),
        ),
        (
            "same author, not following",
            g_note.clone(),
            g_bare.clone(),
            NONE,
            Err(Refusal::NotDiscoverable),
        ),
        (
            "pleroma follower (discoverable, no indexable)",
            p_note.clone(),
            p_actor.clone(),
            FOLLOWS,
            Ok(()),
        ),
        (
            "follower with explicit indexable false",
            m_note.clone(),
            with(&m_actor, &json!({"indexable": false}), &[]),
            FOLLOWS,
            Err(Refusal::NotIndexable),
        ),
        (
            "follower with explicit discoverable false",
            m_note.clone(),
            with(&m_actor, &json!({"discoverable": false}), &[]),
            FOLLOWS,
            Err(Refusal::NotDiscoverable),
        ),
        (
            "follower's unlisted post",
            with(
                &g_note,
                &json!({"to": ["https://gts.example/users/bob/followers"], "cc": [PUBLIC]}),
                &[],
            ),
            g_bare.clone(),
            FOLLOWS,
            Err(Refusal::NotPublic),
        ),
        (
            "follower's followers-only post",
            with(
                &g_note,
                &json!({"to": ["https://gts.example/users/bob/followers"], "cc": []}),
                &[],
            ),
            g_bare.clone(),
            FOLLOWS,
            Err(Refusal::NotPublic),
        ),
        (
            "follower and on lemmy",
            l_page.clone(),
            l_person.clone(),
            ConsentContext {
                author_on_lemmy: true,
                follows_us: true,
            },
            Ok(()),
        ),
    ];

    let mut failures = Vec::new();
    for (name, object, author, context, expected) in &cases {
        let got = evaluate_post_in(object, author, *context);
        if got != *expected {
            failures.push(format!("{name}: expected {expected:?}, got {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn accounts_with_context() {
    let l_person = load("lemmy_person");
    let l_group = load("lemmy_group");
    let g_bare = with(&load("gotosocial_actor"), &json!({}), &["discoverable"]);
    let cases: Vec<(&str, Value, ConsentContext, Result<(), Refusal>)> = vec![
        ("lemmy person", l_person.clone(), LEMMY, Ok(())),
        ("lemmy community", l_group.clone(), LEMMY, Ok(())),
        (
            "lemmy person, host not known to be lemmy",
            l_person.clone(),
            NONE,
            Err(Refusal::NotDiscoverable),
        ),
        (
            "lemmy person with discoverable false",
            with(&l_person, &json!({"discoverable": false}), &[]),
            LEMMY,
            Err(Refusal::NotDiscoverable),
        ),
        ("follower without the flag", g_bare.clone(), FOLLOWS, Ok(())),
        (
            "not following, without the flag",
            g_bare.clone(),
            NONE,
            Err(Refusal::NotDiscoverable),
        ),
        (
            "follower with discoverable false",
            with(&g_bare, &json!({"discoverable": false}), &[]),
            FOLLOWS,
            Err(Refusal::NotDiscoverable),
        ),
    ];
    let mut failures = Vec::new();
    for (name, actor, context, expected) in &cases {
        let got = evaluate_account_in(actor, *context);
        if got != *expected {
            failures.push(format!("{name}: expected {expected:?}, got {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The effective `indexable` flag, which decides author tombstones. Same rule as for posts.
#[test]
fn posts_consented_flag() {
    let l_person = load("lemmy_person");
    let m_actor = load("mastodon_actor");
    let p_actor = load("pleroma_actor");
    let both = ConsentContext {
        author_on_lemmy: true,
        follows_us: true,
    };
    let cases: Vec<(&str, Value, ConsentContext, bool)> = vec![
        ("mastodon, indexable true", m_actor.clone(), NONE, true),
        (
            "mastodon, indexable false",
            with(&m_actor, &json!({"indexable": false}), &[]),
            NONE,
            false,
        ),
        (
            "mastodon, indexable false, follows us",
            with(&m_actor, &json!({"indexable": false}), &[]),
            FOLLOWS,
            false,
        ),
        (
            "lemmy without flags, on lemmy",
            l_person.clone(),
            LEMMY,
            true,
        ),
        (
            "lemmy without flags, host unknown",
            l_person.clone(),
            NONE,
            false,
        ),
        (
            "lemmy with indexable false",
            with(&l_person, &json!({"indexable": false}), &[]),
            both,
            false,
        ),
        (
            "lemmy with indexable null",
            with(&l_person, &json!({"indexable": null}), &[]),
            LEMMY,
            false,
        ),
        (
            "indexable as a string",
            with(&m_actor, &json!({"indexable": "true"}), &[]),
            FOLLOWS,
            false,
        ),
        (
            "pleroma (no indexable), follows us",
            p_actor.clone(),
            FOLLOWS,
            true,
        ),
        (
            "pleroma (no indexable), not following",
            p_actor.clone(),
            NONE,
            false,
        ),
    ];
    let mut failures = Vec::new();
    for (name, actor, context, expected) in &cases {
        let got = posts_consented(actor, *context);
        if got != *expected {
            failures.push(format!("{name}: expected {expected}, got {got}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The plain functions are the same as passing no context.
#[test]
fn plain_functions_use_no_context() {
    let l_person = load("lemmy_person");
    let l_page = load("lemmy_page");
    assert_eq!(
        evaluate_post(&l_page, &l_person),
        evaluate_post_in(&l_page, &l_person, NONE)
    );
    assert_eq!(
        evaluate_account(&l_person),
        evaluate_account_in(&l_person, NONE)
    );
}

/// Objects captured from the end-to-end Mastodon 4.7.5 in dev/mastodon (`seed.rb`), so the
/// rules are checked against what a current Mastodon really serves, not only our models of it.
#[test]
fn captured_from_mastodon_4_7() {
    let alice = load("mastodon47_actor_alice");
    let bob = load("mastodon47_actor_bob");
    assert_eq!(evaluate_account(&alice), Ok(()));
    assert_eq!(evaluate_account(&bob), Ok(()), "bob is discoverable");
    assert_eq!(
        evaluate_post(&load("mastodon47_note_public"), &alice),
        Ok(())
    );
    assert_eq!(
        evaluate_post(&load("mastodon47_note_unlisted"), &alice),
        Err(Refusal::NotPublic)
    );
    assert_eq!(
        evaluate_post(&load("mastodon47_note_bob"), &bob),
        Err(Refusal::NotIndexable)
    );
    // And a post paired with the wrong author is refused.
    assert_eq!(
        evaluate_post(&load("mastodon47_note_public"), &bob),
        Err(Refusal::AuthorMismatch)
    );
}
