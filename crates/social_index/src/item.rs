//! The two item types and their tuple layouts. Required fields first, optional ones last, so
//! trailing nulls truncate and a field added later at the end leaves old pages readable.

use serde_json::Value;
use tinyindex::item::{field, truncate_nulls};
use tinyindex::{IndexItem, PageIoError};

/// A post, as stored on every page it is indexed under.
///
/// Layout: `[uri, author_uri, extract, score, term, published, checked, title?, language?]`.
#[derive(Debug, Clone, PartialEq)]
pub struct SocialPost {
    /// The `ActivityPub` object id. The identity, and what FASP returns.
    pub uri: String,
    /// The actor id. Author tombstones match on it.
    pub author_uri: String,
    /// Plain text of the content, content warning and hashtags included, truncated.
    pub extract: String,
    pub score: f64,
    /// The key this copy is stored under. Empty on a record that is not yet a copy.
    pub term: String,
    /// Unix seconds.
    pub published: i64,
    /// Unix seconds: when we last fetched or confirmed the object.
    pub checked: i64,
    /// The `name` of an Article or Page, or a Lemmy post title.
    pub title: Option<String>,
    /// BCP 47.
    pub language: Option<String>,
}

impl IndexItem for SocialPost {
    const ITEM_FACTORY: &'static str = "SocialPost";
    const TERM_POSITION: usize = 4;

    fn url(&self) -> &str {
        &self.uri
    }

    fn score(&self) -> f64 {
        self.score
    }

    fn term(&self) -> Option<&str> {
        Some(&self.term)
    }

    fn to_tuple(&self) -> Vec<Value> {
        truncate_nulls(vec![
            Value::from(self.uri.as_str()),
            Value::from(self.author_uri.as_str()),
            Value::from(self.extract.as_str()),
            Value::from(self.score),
            Value::from(self.term.as_str()),
            Value::from(self.published),
            Value::from(self.checked),
            self.title.as_deref().map_or(Value::Null, Value::from),
            self.language.as_deref().map_or(Value::Null, Value::from),
        ])
    }

    fn from_tuple(mut values: Vec<Value>) -> Result<Self, PageIoError> {
        let f = Self::ITEM_FACTORY;
        let v = values.as_mut_slice();
        Ok(Self {
            uri: field::required(f, v, 0, "a string")?,
            author_uri: field::required(f, v, 1, "a string")?,
            extract: field::required(f, v, 2, "a string")?,
            score: field::required(f, v, 3, "a number")?,
            term: field::required(f, v, 4, "a string")?,
            published: field::required(f, v, 5, "an integer")?,
            checked: field::required(f, v, 6, "an integer")?,
            title: field::optional(f, v, 7, "a string")?,
            language: field::optional(f, v, 8, "a string")?,
        })
    }
}

/// An account, as stored on every page it is indexed under.
///
/// Layout: `[uri, acct, display_name, summary, score, term, checked, languages?]`.
#[derive(Debug, Clone, PartialEq)]
pub struct SocialAccount {
    /// The actor id.
    pub uri: String,
    /// `user@host`.
    pub acct: String,
    pub display_name: String,
    /// The bio as plain text, truncated.
    pub summary: String,
    pub score: f64,
    /// The key this copy is stored under. Empty on a record that is not yet a copy.
    pub term: String,
    /// Unix seconds: when we last fetched or confirmed the actor.
    pub checked: i64,
    /// Posting languages we have observed, for `account_search`'s `language` filter.
    pub languages: Option<Vec<String>>,
}

impl IndexItem for SocialAccount {
    const ITEM_FACTORY: &'static str = "SocialAccount";
    const TERM_POSITION: usize = 5;

    fn url(&self) -> &str {
        &self.uri
    }

    fn score(&self) -> f64 {
        self.score
    }

    fn term(&self) -> Option<&str> {
        Some(&self.term)
    }

    fn to_tuple(&self) -> Vec<Value> {
        truncate_nulls(vec![
            Value::from(self.uri.as_str()),
            Value::from(self.acct.as_str()),
            Value::from(self.display_name.as_str()),
            Value::from(self.summary.as_str()),
            Value::from(self.score),
            Value::from(self.term.as_str()),
            Value::from(self.checked),
            self.languages.clone().map_or(Value::Null, Value::from),
        ])
    }

    fn from_tuple(mut values: Vec<Value>) -> Result<Self, PageIoError> {
        let f = Self::ITEM_FACTORY;
        let v = values.as_mut_slice();
        Ok(Self {
            uri: field::required(f, v, 0, "a string")?,
            acct: field::required(f, v, 1, "a string")?,
            display_name: field::required(f, v, 2, "a string")?,
            summary: field::required(f, v, 3, "a string")?,
            score: field::required(f, v, 4, "a number")?,
            term: field::required(f, v, 5, "a string")?,
            checked: field::required(f, v, 6, "an integer")?,
            languages: field::optional(f, v, 7, "a list of strings")?,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::json;

    use super::*;

    pub fn post(uri: &str, extract: &str) -> SocialPost {
        SocialPost {
            uri: uri.into(),
            author_uri: "https://h.example/users/a".into(),
            extract: extract.into(),
            score: 1.0,
            term: String::new(),
            published: 100,
            checked: 200,
            title: None,
            language: None,
        }
    }

    pub fn account(uri: &str, acct: &str, display_name: &str) -> SocialAccount {
        SocialAccount {
            uri: uri.into(),
            acct: acct.into(),
            display_name: display_name.into(),
            summary: "Bio".into(),
            score: 1.0,
            term: String::new(),
            checked: 200,
            languages: None,
        }
    }

    #[test]
    fn post_layout() {
        let mut p = post("https://h.example/s/1", "hello");
        p.term = "hello".into();
        assert_eq!(
            Value::from(p.to_tuple()),
            json!([
                "https://h.example/s/1",
                "https://h.example/users/a",
                "hello",
                1.0,
                "hello",
                100,
                200
            ])
        );
        p.language = Some("en".into());
        assert_eq!(
            Value::from(p.to_tuple()),
            json!([
                "https://h.example/s/1",
                "https://h.example/users/a",
                "hello",
                1.0,
                "hello",
                100,
                200,
                null,
                "en"
            ])
        );
        assert_eq!(SocialPost::from_tuple(p.to_tuple()).unwrap(), p);
    }

    #[test]
    fn account_layout() {
        let mut a = account("https://h.example/users/a", "a@h.example", "A");
        a.term = "a".into();
        a.languages = Some(vec!["en".into(), "de".into()]);
        assert_eq!(
            Value::from(a.to_tuple()),
            json!([
                "https://h.example/users/a",
                "a@h.example",
                "A",
                "Bio",
                1.0,
                "a",
                200,
                ["en", "de"]
            ])
        );
        assert_eq!(SocialAccount::from_tuple(a.to_tuple()).unwrap(), a);
    }

    #[test]
    fn a_tuple_missing_a_required_field_is_refused() {
        assert!(SocialPost::from_tuple(vec![json!("u"), json!("a"), json!("x")]).is_err());
        assert!(SocialAccount::from_tuple(vec![json!("u")]).is_err());
        assert!(
            SocialPost::from_tuple(vec![
                json!("u"),
                json!("a"),
                json!("x"),
                json!("high"),
                json!("t"),
                json!(1),
                json!(2)
            ])
            .is_err()
        );
    }
}
