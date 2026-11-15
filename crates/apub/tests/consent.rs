//! Consent evaluation, table-driven over JSON shaped like what Mastodon, GoToSocial, Misskey
//! and Pleroma serve. Every rule in `apub::consent` has a case here; add one for any change.

use apub::consent::{Refusal, evaluate_account, evaluate_post};
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
