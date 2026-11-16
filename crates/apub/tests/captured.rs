//! Parsing objects captured from the end-to-end Mastodon 4.7.5 (see dev/mastodon/seed.rb) and
//! Lemmy 0.19.13 (dev/lemmy/seed.sh).

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

#[test]
fn lemmy_0_19_page() {
    let post = parse_post(&load("lemmy019_page")).unwrap();
    assert_eq!(post.uri, "https://lemmy.test/post/3");
    assert_eq!(post.name.as_deref(), Some("Pattern matching on slices"));
    assert_eq!(post.content_text, "Slice patterns make parsers tidy.");
    assert_eq!(post.author_uri, "https://lemmy.test/u/carol");
    // Lemmy 0.19 tags every post with its community's name.
    assert_eq!(post.hashtags, ["rust"]);
    assert!(post.published.is_some());
}

#[test]
fn lemmy_0_19_comment() {
    let post = parse_post(&load("lemmy019_comment")).unwrap();
    assert_eq!(post.uri, "https://lemmy.test/comment/2");
    assert_eq!(post.name, None);
    assert_eq!(
        post.in_reply_to.as_deref(),
        Some("https://lemmy.test/post/2")
    );
    assert_eq!(post.content_text, "Smol is underrated, honestly.");
    assert!(post.hashtags.is_empty());
}

#[test]
fn lemmy_0_19_person_and_group() {
    // No discoverable or indexable: the Lemmy consent rule decides.
    let person = parse_actor(&load("lemmy019_person")).unwrap();
    assert_eq!(person.acct, "carol@lemmy.test");
    assert!(person.public_key.is_some());
    let group = parse_actor(&load("lemmy019_group")).unwrap();
    assert_eq!(group.acct, "rust@lemmy.test");
    assert_eq!(group.name.as_deref(), Some("Rust"));
}
