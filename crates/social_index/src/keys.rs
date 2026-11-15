//! Which keys an item is indexed under.
//!
//! Keys must be a pure function of the item's stored fields and the [`KeyScheme`]: deletion
//! re-derives them from a record read off a page. Never add a key from text the item does
//! not store, and change [`KeyScheme::id`] whenever the output of any of this changes.

use tinyindex::IndexItem;
use tokenize::{KeyConfig, TOKENIZER_VERSION, field_keys, special_keys, tokenize};

use crate::item::{SocialAccount, SocialPost};

/// The key every item is also stored under, so it can be found by URI alone.
#[must_use]
pub fn uri_key(uri: &str) -> String {
    format!("uri:{uri}")
}

/// Everything that decides an item's keys. Written to the index metadata, and an index
/// written with a different scheme is refused, because deletes would re-derive the wrong keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyScheme {
    pub config: KeyConfig,
}

impl KeyScheme {
    /// The identifier stored in the index metadata.
    #[must_use]
    pub fn id(&self) -> String {
        let KeyConfig {
            first_tokens,
            bigrams,
            special,
        } = self.config;
        format!(
            "social-v1;tokenize-v{TOKENIZER_VERSION};first={first_tokens};bigrams={bigrams};special={special}"
        )
    }
}

/// An index item that knows its own keys.
pub trait Keyed: IndexItem + Clone {
    fn uri(&self) -> &str;
    /// The author's actor URI, for author tombstones. An account is its own author.
    fn author_uri(&self) -> &str;
    fn checked(&self) -> i64;
    fn set_checked(&mut self, checked: i64);
    /// This item as the copy stored under `term`.
    #[must_use]
    fn with_term(&self, term: &str) -> Self;
    /// Every key, the `uri:` key included, without duplicates, in a fixed order.
    fn keys(&self, scheme: &KeyScheme) -> Vec<String>;
}

fn push_unique(keys: &mut Vec<String>, new: impl IntoIterator<Item = String>) {
    for key in new {
        if !key.is_empty() && !keys.contains(&key) {
            keys.push(key);
        }
    }
}

impl Keyed for SocialPost {
    fn uri(&self) -> &str {
        &self.uri
    }

    fn author_uri(&self) -> &str {
        &self.author_uri
    }

    fn checked(&self) -> i64 {
        self.checked
    }

    fn set_checked(&mut self, checked: i64) {
        self.checked = checked;
    }

    fn with_term(&self, term: &str) -> Self {
        Self {
            term: term.to_string(),
            ..self.clone()
        }
    }

    fn keys(&self, scheme: &KeyScheme) -> Vec<String> {
        let mut keys = Vec::new();
        if let Some(title) = &self.title {
            push_unique(&mut keys, field_keys(title, &scheme.config));
        }
        push_unique(&mut keys, field_keys(&self.extract, &scheme.config));
        push_unique(&mut keys, [uri_key(&self.uri)]);
        keys
    }
}

impl Keyed for SocialAccount {
    fn uri(&self) -> &str {
        &self.uri
    }

    fn author_uri(&self) -> &str {
        &self.uri
    }

    fn checked(&self) -> i64 {
        self.checked
    }

    fn set_checked(&mut self, checked: i64) {
        self.checked = checked;
    }

    fn with_term(&self, term: &str) -> Self {
        Self {
            term: term.to_string(),
            ..self.clone()
        }
    }

    /// The username, the host, the `@user@host` handle, then display-name and summary keys.
    /// Handle searches work because `query_keys("@user@host")` asks for the same handle key.
    fn keys(&self, scheme: &KeyScheme) -> Vec<String> {
        let mut keys = Vec::new();
        let (user, host) = self.acct.split_once('@').unwrap_or((&self.acct, ""));
        push_unique(&mut keys, tokenize(user));
        push_unique(&mut keys, tokenize(host));
        push_unique(&mut keys, special_keys(&format!("@{}", self.acct)));
        push_unique(&mut keys, field_keys(&self.display_name, &scheme.config));
        push_unique(&mut keys, field_keys(&self.summary, &scheme.config));
        push_unique(&mut keys, [uri_key(&self.uri)]);
        keys
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::tests::{account, post};

    #[test]
    fn post_keys_come_from_title_extract_and_uri() {
        let mut p = post("https://h.example/s/1", "Learning Rust today #rustlang");
        p.title = Some("My Article".into());
        let keys = p.keys(&KeyScheme::default());
        for key in [
            "article",
            "my article",
            "learning",
            "rust",
            "learning rust",
            "#rustlang",
        ] {
            assert!(keys.contains(&key.to_string()), "{key} in {keys:?}");
        }
        assert_eq!(keys.last().unwrap(), "uri:https://h.example/s/1");
        // Nothing from fields other than title, extract and uri.
        assert!(!keys.iter().any(|k| k.contains("users")));
    }

    #[test]
    fn account_keys_cover_handle_searches() {
        let a = account(
            "https://h.example/users/alice",
            "Alice_W@H.Example",
            "Alice Wonder",
        );
        let keys = a.keys(&KeyScheme::default());
        for key in [
            "alice_w",
            "h.example",
            "@alice_w@h.example",
            "alice",
            "wonder",
            "alice wonder",
            "bio",
        ] {
            assert!(keys.contains(&key.to_string()), "{key} in {keys:?}");
        }
        // A query for the handle looks up a key the account has.
        let query = tokenize::query_keys("@alice_w@h.example");
        assert!(query.iter().any(|q| keys.contains(q)));
    }

    #[test]
    fn keys_are_a_pure_function_of_stored_fields() {
        let p = post("https://h.example/s/1", "same text");
        let mut q = p.with_term("anything");
        q.score = 99.0;
        q.checked = 5;
        q.published = 6;
        assert_eq!(p.keys(&KeyScheme::default()), q.keys(&KeyScheme::default()));
    }

    #[test]
    fn scheme_id_changes_with_config() {
        let a = KeyScheme::default();
        let mut b = a;
        b.config.bigrams = 5;
        assert_ne!(a.id(), b.id());
        assert!(a.id().contains(&format!("tokenize-v{TOKENIZER_VERSION}")));
    }
}
