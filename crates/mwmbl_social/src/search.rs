//! Running a search and shaping the results. Shared by the HTTP API and the `search` command,
//! so both show exactly the same fields: the record's public ones, `uri` first, never the
//! internal `term`.

use std::sync::{PoisonError, RwLock};

use serde::Serialize;
use social_index::{
    SocialAccount, SocialPost, SocialReader, Tombstones, search_accounts, search_posts,
};
use tinyindex::IndexError;

/// Most results a search returns, whatever `limit` asks for.
pub const MAX_LIMIT: usize = 100;
/// Results returned when `limit` is not given.
pub const DEFAULT_LIMIT: usize = 20;
/// Longest `q` the search API takes, in UTF-8 bytes after trimming. Longer ones get a 422
/// rather than being cut, since a cut query would silently search for something else. 512
/// bytes is several sentences, far more than a search box needs, and bounds the tokenising
/// work one unauthenticated request can ask for (audit E1); `tokenize::MAX_QUERY_KEYS` then
/// bounds its page reads.
pub const MAX_QUERY_BYTES: usize = 512;

/// A post as the search API returns it. Field order is the JSON order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PostView {
    pub uri: String,
    pub author_uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub extract: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub published: i64,
    pub checked: i64,
    pub score: f64,
}

impl From<SocialPost> for PostView {
    fn from(p: SocialPost) -> Self {
        Self {
            uri: p.uri,
            author_uri: p.author_uri,
            title: p.title,
            extract: p.extract,
            language: p.language,
            published: p.published,
            checked: p.checked,
            score: p.score,
        }
    }
}

/// An account as the search API returns it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountView {
    pub uri: String,
    pub acct: String,
    pub display_name: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub languages: Option<Vec<String>>,
    pub checked: i64,
    pub score: f64,
}

impl From<SocialAccount> for AccountView {
    fn from(a: SocialAccount) -> Self {
        Self {
            uri: a.uri,
            acct: a.acct,
            display_name: a.display_name,
            summary: a.summary,
            languages: a.languages,
            checked: a.checked,
            score: a.score,
        }
    }
}

/// Clamp a requested limit to `1..=MAX_LIMIT`.
#[must_use]
pub fn clamp_limit(limit: usize) -> usize {
    limit.clamp(1, MAX_LIMIT)
}

/// Search posts, hiding anything the tombstones cover. Blocking: run it on a blocking thread.
pub fn posts(
    reader: &SocialReader<SocialPost>,
    tombstones: &RwLock<Tombstones>,
    query: &str,
    limit: usize,
    now: i64,
) -> Result<Vec<PostView>, IndexError> {
    let tombstones = tombstones.read().unwrap_or_else(PoisonError::into_inner);
    Ok(search_posts(reader, query, &tombstones, now)?
        .into_iter()
        .take(clamp_limit(limit))
        .map(PostView::from)
        .collect())
}

/// Search accounts, hiding anything the tombstones cover. Blocking.
pub fn accounts(
    reader: &SocialReader<SocialAccount>,
    tombstones: &RwLock<Tombstones>,
    query: &str,
    limit: usize,
    language: Option<&str>,
) -> Result<Vec<AccountView>, IndexError> {
    let tombstones = tombstones.read().unwrap_or_else(PoisonError::into_inner);
    Ok(search_accounts(reader, query, &tombstones, language)?
        .into_iter()
        .take(clamp_limit(limit))
        .map(AccountView::from)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_clamp() {
        assert_eq!(clamp_limit(0), 1);
        assert_eq!(clamp_limit(5), 5);
        assert_eq!(clamp_limit(1000), MAX_LIMIT);
    }

    #[test]
    fn json_starts_with_uri_and_hides_the_term() -> Result<(), serde_json::Error> {
        let view = PostView::from(SocialPost {
            uri: "https://h.example/s/1".into(),
            author_uri: "https://h.example/u/a".into(),
            extract: "x".into(),
            score: 1.0,
            term: "secret-term".into(),
            published: 1,
            checked: 2,
            title: None,
            language: None,
        });
        let text = serde_json::to_string(&view)?;
        assert!(text.starts_with("{\"uri\":\"https://h.example/s/1\""));
        assert!(!text.contains("term") && !text.contains("title"));
        let account = AccountView::from(SocialAccount {
            uri: "u".into(),
            acct: "a@h".into(),
            display_name: "A".into(),
            summary: "s".into(),
            score: 1.0,
            term: "t".into(),
            checked: 1,
            languages: Some(vec!["en".into()]),
        });
        let text = serde_json::to_string(&account)?;
        assert!(text.starts_with("{\"uri\":\"u\",\"acct\":\"a@h\""));
        assert!(text.contains("\"languages\":[\"en\"]"));
        Ok(())
    }
}
