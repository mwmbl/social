//! What a page holds. A page is a JSON array of tuples, and each item type decides its own
//! tuple layout, as Python's `TinyIndex[T]` does with its `item_factory`.

use serde_json::Value;

use crate::page::PageIoError;

/// One entry on an index page.
///
/// Every item carries the term it was indexed under, because a page is shared by every term
/// that hashes to it and reads filter by term. An item indexed under ten keys is stored as ten
/// copies, one per key's page, each with its own `term`.
///
/// Layouts put required fields first and optional ones last, and `to_tuple` drops trailing
/// nulls, so a field added later at the end leaves old pages readable.
pub trait IndexItem: Sized {
    /// Written to the index metadata. Opening an index refuses one written for another type.
    const ITEM_FACTORY: &'static str;
    /// Where `term` sits in the tuple. Read filtering looks here without decoding the item.
    const TERM_POSITION: usize;

    /// The item's identity. Merging onto a page deduplicates by (`url`, `term`), and removal
    /// is by `url`.
    fn url(&self) -> &str;
    /// Pages are kept best-first by this, and the lowest scores are what fall off a full page.
    fn score(&self) -> f64;
    /// The term this copy is indexed under. `None` matches every term, as in Mwmbl.
    fn term(&self) -> Option<&str>;
    /// The tuple to store, with trailing nulls dropped.
    fn to_tuple(&self) -> Vec<Value>;
    /// The inverse of `to_tuple`. Missing trailing values are `None`.
    fn from_tuple(values: Vec<Value>) -> Result<Self, PageIoError>;
}

/// Drop trailing nulls, as Python's `as_tuple` does.
pub fn truncate_nulls(mut values: Vec<Value>) -> Vec<Value> {
    while values.last().is_some_and(Value::is_null) {
        values.pop();
    }
    values
}

/// Decoding helpers for `from_tuple` implementations. Each takes the tuple's values by
/// position; a missing position reads as null.
pub mod field {
    use serde::de::DeserializeOwned;
    use serde_json::Value;

    use crate::page::PageIoError;

    fn bad(item_factory: &str, position: usize, expected: &str, value: &Value) -> PageIoError {
        PageIoError::Unreadable(format!(
            "{item_factory} field {position}: expected {expected}, got {value}"
        ))
    }

    /// The value at `position`, as `T`, or `None` if it is null or missing.
    pub fn optional<T: DeserializeOwned>(
        item_factory: &str,
        values: &mut [Value],
        position: usize,
        expected: &str,
    ) -> Result<Option<T>, PageIoError> {
        match values.get_mut(position).map(Value::take) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => serde_json::from_value(value.clone())
                .map(Some)
                .map_err(|_| bad(item_factory, position, expected, &value)),
        }
    }

    /// The value at `position`, as `T`. Null or missing is an error.
    pub fn required<T: DeserializeOwned>(
        item_factory: &str,
        values: &mut [Value],
        position: usize,
        expected: &str,
    ) -> Result<T, PageIoError> {
        optional(item_factory, values, position, expected)?
            .ok_or_else(|| bad(item_factory, position, expected, &Value::Null))
    }
}

/// Mwmbl's own item, so this crate keeps reading and writing pages Mwmbl's Python wrote.
///
/// Layout: `[title, url, extract, score, term, state, user_ids, last_crawled, source]`.
/// Python turns a null title or extract into an empty string, and so does this.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub title: String,
    pub url: String,
    pub extract: String,
    pub score: Option<f64>,
    pub term: Option<String>,
    pub state: Option<i64>,
    pub user_ids: Option<Vec<i64>>,
    pub last_crawled: Option<i64>,
    pub source: Option<i64>,
}

impl IndexItem for Document {
    const ITEM_FACTORY: &'static str = "Document";
    const TERM_POSITION: usize = 4;

    fn url(&self) -> &str {
        &self.url
    }

    fn score(&self) -> f64 {
        self.score.unwrap_or(0.0)
    }

    fn term(&self) -> Option<&str> {
        self.term.as_deref()
    }

    fn to_tuple(&self) -> Vec<Value> {
        truncate_nulls(vec![
            Value::from(self.title.as_str()),
            Value::from(self.url.as_str()),
            Value::from(self.extract.as_str()),
            self.score.map_or(Value::Null, Value::from),
            self.term.as_deref().map_or(Value::Null, Value::from),
            self.state.map_or(Value::Null, Value::from),
            self.user_ids.clone().map_or(Value::Null, Value::from),
            self.last_crawled.map_or(Value::Null, Value::from),
            self.source.map_or(Value::Null, Value::from),
        ])
    }

    fn from_tuple(mut values: Vec<Value>) -> Result<Self, PageIoError> {
        let f = Self::ITEM_FACTORY;
        let v = values.as_mut_slice();
        Ok(Self {
            title: field::optional(f, v, 0, "a string")?.unwrap_or_default(),
            url: field::required(f, v, 1, "a string")?,
            extract: field::optional(f, v, 2, "a string")?.unwrap_or_default(),
            score: field::optional(f, v, 3, "a number")?,
            term: field::optional(f, v, 4, "a string")?,
            state: field::optional(f, v, 5, "an integer")?,
            user_ids: field::optional(f, v, 6, "a list of integers")?,
            last_crawled: field::optional(f, v, 7, "an integer")?,
            source: field::optional(f, v, 8, "an integer")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn document_round_trips_and_truncates_trailing_nulls() {
        let doc = Document {
            title: "t".into(),
            url: "https://e.com/".into(),
            extract: "x".into(),
            score: Some(1.5),
            term: Some("k".into()),
            state: None,
            user_ids: None,
            last_crawled: Some(5),
            source: None,
        };
        let tuple = doc.to_tuple();
        assert_eq!(
            Value::from(tuple.clone()),
            json!(["t", "https://e.com/", "x", 1.5, "k", null, null, 5])
        );
        assert_eq!(Document::from_tuple(tuple).unwrap(), doc);
    }

    #[test]
    fn document_null_title_reads_as_empty() {
        let doc = Document::from_tuple(vec![json!(null), json!("u"), json!(null)]).unwrap();
        assert_eq!(doc.title, "");
        assert_eq!(doc.extract, "");
        assert_eq!(doc.score, None);
    }

    #[test]
    fn document_missing_url_is_an_error() {
        assert!(Document::from_tuple(vec![json!("t")]).is_err());
        assert!(Document::from_tuple(vec![json!("t"), json!(3)]).is_err());
    }
}
