//! Audit E (2026-11-19): key counts a stranger controls. These fail until fixed.

use social_index::{KeyScheme, Keyed, SocialAccount};

/// E2: an account's keys include every token of the username (`keys.rs` `tokenize(user)`),
/// and `acct` is `preferredUsername@host` untruncated (`records.rs` `account_record`). A
/// hostile server's discoverable actor with a long hyphenated `preferredUsername` is written
/// to thousands of pages, where its high claimed follower count evicts real accounts.
#[test]
#[ignore = "AUDIT: E2 - fails until fixed"]
fn account_keys_are_bounded() {
    let user: String = (0..2_000).map(|i| format!("a{i}-")).collect();
    let account = SocialAccount {
        uri: "https://evil.example/users/x".into(),
        acct: format!("{user}@evil.example"),
        display_name: "x".into(),
        summary: String::new(),
        score: 1.0,
        term: String::new(),
        checked: 0,
        languages: None,
    };
    let keys = account.keys(&KeyScheme::default());
    assert!(
        keys.len() <= 64,
        "one account is stored under {} keys",
        keys.len()
    );
}
