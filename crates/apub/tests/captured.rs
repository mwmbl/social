//! Parsing objects captured from the end-to-end Mastodon 4.7.5 (see dev/mastodon/seed.rb).

use apub::objects::{parse_actor, parse_post};
use serde_json::Value;

fn load(name: &str) -> Value {
    let path = format!("{}/tests/consent/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

#[test]
fn mastodon_4_7_note() {
    let post = parse_post(&load("mastodon47_note_public")).unwrap();
    assert!(
        post.content_text
            .starts_with("Planting garlic in the allotment today #gardening"),
        "{}",
        post.content_text
    );
    assert_eq!(post.hashtags, ["gardening"]);
    assert_eq!(
        post.author_uri,
        "https://mastodon.test/ap/users/117622812625693465"
    );
    assert_eq!(post.language.as_deref(), Some("en"));
    assert!(post.published.is_some());
}

#[test]
fn mastodon_4_7_actor() {
    let actor = parse_actor(&load("mastodon47_actor_alice")).unwrap();
    assert_eq!(actor.acct, "alice@mastodon.test");
    assert_eq!(actor.name.as_deref(), Some("Alice"));
    assert_eq!(
        actor.summary_text.as_deref(),
        Some("Alice tests Mwmbl Social")
    );
    assert!(actor.discoverable && actor.indexable);
    assert!(actor.public_key.is_some());
}
