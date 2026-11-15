//! A tinyindex of posts or accounts, keyed by [`Keyed::keys`], with lookup and deletion by URI.

use std::path::Path;

use tinyindex::{AddStats, IndexError, IndexWriter, TinyIndex};

use crate::keys::{KeyScheme, Keyed, uri_key};

#[derive(Debug, thiserror::Error)]
pub enum SocialIndexError {
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error("index {path} was built with key scheme {found:?}, not {expected:?}; rebuild it")]
    KeyScheme {
        path: String,
        expected: String,
        found: Option<String>,
    },
}

/// What [`SocialIndex::delete`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// The record was found by URI and every copy on its keys' pages removed.
    Removed { copies: usize },
    /// No record under the `uri:` key: it fell off a full page or was never indexed. The
    /// caller must add a tombstone, so search filters it and the sweep removes any copies.
    NotFound,
}

/// Read access, safe to clone into every request handler.
#[derive(Debug, Clone)]
pub struct SocialReader<T: Keyed> {
    index: TinyIndex<T>,
}

impl<T: Keyed> SocialReader<T> {
    /// The copies stored under `key`.
    pub fn retrieve(&self, key: &str) -> Result<Vec<T>, IndexError> {
        self.index.retrieve(key)
    }

    /// The record with this URI, from its `uri:` page.
    pub fn find(&self, uri: &str) -> Result<Option<T>, IndexError> {
        find_in(&self.index, uri)
    }

    #[must_use]
    pub fn index(&self) -> &TinyIndex<T> {
        &self.index
    }
}

fn find_in<T: Keyed>(index: &TinyIndex<T>, uri: &str) -> Result<Option<T>, IndexError> {
    Ok(index
        .retrieve(&uri_key(uri))?
        .into_iter()
        .find(|copy| copy.uri() == uri))
}

/// The writer for one index. Only the single writer task owns one.
#[derive(Debug)]
pub struct SocialIndex<T: Keyed> {
    writer: IndexWriter<T>,
    scheme: KeyScheme,
}

impl<T: Keyed> SocialIndex<T> {
    pub fn create(
        path: &Path,
        num_pages: usize,
        page_size: usize,
        scheme: KeyScheme,
    ) -> Result<Self, SocialIndexError> {
        let writer = TinyIndex::create(path, num_pages, page_size, Some(scheme.id()))?;
        Ok(Self { writer, scheme })
    }

    /// Open an existing index, refusing one built with another key scheme: deletes would
    /// re-derive the wrong keys and leave copies behind.
    pub fn open(path: &Path, scheme: KeyScheme) -> Result<Self, SocialIndexError> {
        let writer = IndexWriter::open(path)?;
        let found = writer.reader().metadata().tokenizer.clone();
        let expected = scheme.id();
        if found.as_deref() != Some(expected.as_str()) {
            return Err(SocialIndexError::KeyScheme {
                path: path.display().to_string(),
                expected,
                found,
            });
        }
        Ok(Self { writer, scheme })
    }

    /// Open the index at `path`, creating it if there is no file there.
    pub fn open_or_create(
        path: &Path,
        num_pages: usize,
        page_size: usize,
        scheme: KeyScheme,
    ) -> Result<Self, SocialIndexError> {
        if path.exists() {
            Self::open(path, scheme)
        } else {
            Self::create(path, num_pages, page_size, scheme)
        }
    }

    #[must_use]
    pub fn reader(&self) -> SocialReader<T> {
        SocialReader {
            index: self.writer.reader(),
        }
    }

    #[must_use]
    pub fn scheme(&self) -> &KeyScheme {
        &self.scheme
    }

    pub fn find(&self, uri: &str) -> Result<Option<T>, IndexError> {
        find_in(&self.writer.reader(), uri)
    }

    /// Store a record under every one of its keys. Merging replaces an existing copy with the
    /// same URI and key, so re-adding unchanged content refreshes it in place. Content that
    /// changed must go through [`Self::upsert`] or [`Self::replace`], or copies under keys
    /// the new content no longer has are left behind.
    pub fn add(&mut self, record: &T) -> Result<AddStats, IndexError> {
        let copies = record
            .keys(&self.scheme)
            .iter()
            .map(|key| record.with_term(key))
            .collect();
        self.writer.add(copies)
    }

    /// Remove every copy of a record we hold, by re-deriving its keys. Pass a record read
    /// from a page or carried by the fetch queue, so its fields are the stored ones.
    pub fn delete_record(&mut self, record: &T) -> Result<usize, IndexError> {
        let keys = record.keys(&self.scheme);
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        self.writer.remove(record.uri(), &keys)
    }

    /// Delete by URI: look the record up under its `uri:` key and delete it by its keys.
    pub fn delete(&mut self, uri: &str) -> Result<DeleteOutcome, IndexError> {
        match self.find(uri)? {
            Some(record) => Ok(DeleteOutcome::Removed {
                copies: self.delete_record(&record)?,
            }),
            None => Ok(DeleteOutcome::NotFound),
        }
    }

    /// Store a new version of something that may already be indexed: delete the old version
    /// by URI lookup, then add. Returns what the delete found.
    pub fn upsert(&mut self, record: &T) -> Result<DeleteOutcome, IndexError> {
        let outcome = self.delete(record.uri())?;
        self.add(record)?;
        Ok(outcome)
    }

    /// Replace a record we have in hand (from the sweep, via revalidation) with a fresh
    /// version. If the keys are unchanged the add overwrites every copy in place; otherwise
    /// the old copies are removed first.
    pub fn replace(&mut self, old: &T, new: &T) -> Result<(), IndexError> {
        if old.uri() != new.uri() || old.keys(&self.scheme) != new.keys(&self.scheme) {
            self.delete_record(old)?;
        }
        self.add(new)?;
        Ok(())
    }

    /// Direct access for the sweep.
    pub(crate) fn reader_index(&self) -> TinyIndex<T> {
        self.writer.reader()
    }
}
