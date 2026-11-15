//! The index file: a header, then `num_pages` fixed-size pages.
//!
//! [`TinyIndex`] is a cheap-to-clone read handle. [`IndexWriter`] is the one writer, and
//! hands out readers that share its file. Callers keep the single-writer rule themselves:
//! nothing else may write to the file while a writer exists.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::marker::PhantomData;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crate::hash;
use crate::item::IndexItem;
use crate::metadata::{INDEX_VERSION, IndexMetadata, METADATA_SIZE};
use crate::page::{self, PageIoError};

/// How many times a read that finds an unreadable page is tried in all. The single writer
/// replaces a page with one write, so a reader can catch it half-written; a retry sees it
/// whole.
const READ_ATTEMPTS: usize = 3;

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("I/O error on index {}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("page {page} of index {}: {source}", path.display())]
    Page {
        path: PathBuf,
        page: usize,
        source: PageIoError,
    },
    #[error("bad metadata in index {}: {reason}", path.display())]
    Metadata { path: PathBuf, reason: String },
    #[error("index {} holds {found} items, not {expected}", path.display())]
    ItemFactory {
        path: PathBuf,
        expected: &'static str,
        found: String,
    },
    #[error("index {} already exists", path.display())]
    Exists { path: PathBuf },
    #[error("item {url} has no term to index it under")]
    NoTerm { url: String },
    #[error("page {page} is out of range for index {} with {num_pages} pages", path.display())]
    PageOutOfRange {
        path: PathBuf,
        page: usize,
        num_pages: usize,
    },
}

/// What an [`IndexWriter::add`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AddStats {
    pub pages_written: usize,
    /// Items now on the pages written, summed over them.
    pub stored: usize,
    /// Items that were in the merged lists but did not fit, summed over the pages written.
    pub dropped: usize,
}

struct Inner {
    file: File,
    path: PathBuf,
    metadata: IndexMetadata,
}

/// A read handle on an index. Cloning shares the open file.
pub struct TinyIndex<T: IndexItem> {
    inner: Arc<Inner>,
    // `fn() -> T` keeps the handle Send and Sync whatever `T` is: it holds no `T`.
    _item: PhantomData<fn() -> T>,
}

impl<T: IndexItem> Clone for TinyIndex<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            _item: PhantomData,
        }
    }
}

impl<T: IndexItem> std::fmt::Debug for TinyIndex<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TinyIndex")
            .field("path", &self.inner.path)
            .field("metadata", &self.inner.metadata)
            .finish()
    }
}

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> IndexError + '_ {
    move |source| IndexError::Io {
        path: path.to_path_buf(),
        source,
    }
}

impl<T: IndexItem> TinyIndex<T> {
    /// Create a new index file and return its writer. Fails with [`IndexError::Exists`] if
    /// the file is already there.
    pub fn create(
        path: &Path,
        num_pages: usize,
        page_size: usize,
        tokenizer: Option<String>,
    ) -> Result<IndexWriter<T>, IndexError> {
        let metadata = IndexMetadata {
            version: INDEX_VERSION,
            page_size,
            num_pages,
            item_factory: T::ITEM_FACTORY.to_string(),
            tokenizer,
        };
        let header = metadata.to_bytes().map_err(|e| IndexError::Metadata {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
        let (empty_page, _) =
            page::pack_page(page_size, &[]).map_err(|source| IndexError::Page {
                path: path.to_path_buf(),
                page: 0,
                source,
            })?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|source| {
                if source.kind() == std::io::ErrorKind::AlreadyExists {
                    IndexError::Exists {
                        path: path.to_path_buf(),
                    }
                } else {
                    IndexError::Io {
                        path: path.to_path_buf(),
                        source,
                    }
                }
            })?;
        let mut out = BufWriter::new(&file);
        out.write_all(&header).map_err(io_error(path))?;
        for _ in 0..num_pages {
            out.write_all(&empty_page).map_err(io_error(path))?;
        }
        out.flush().map_err(io_error(path))?;
        drop(out);
        Ok(IndexWriter {
            index: Self::from_parts(file, path, metadata),
        })
    }

    /// Open an existing index read-only.
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        let file = File::open(path).map_err(io_error(path))?;
        Self::load(file, path)
    }

    fn from_parts(file: File, path: &Path, metadata: IndexMetadata) -> Self {
        Self {
            inner: Arc::new(Inner {
                file,
                path: path.to_path_buf(),
                metadata,
            }),
            _item: PhantomData,
        }
    }

    /// Check the header and the file's length against it.
    fn load(file: File, path: &Path) -> Result<Self, IndexError> {
        let mut header = [0u8; METADATA_SIZE];
        file.read_exact_at(&mut header, 0)
            .map_err(|source| match source.kind() {
                std::io::ErrorKind::UnexpectedEof => IndexError::Metadata {
                    path: path.to_path_buf(),
                    reason: "the file is shorter than the metadata header".to_string(),
                },
                _ => IndexError::Io {
                    path: path.to_path_buf(),
                    source,
                },
            })?;
        let metadata = IndexMetadata::from_bytes(&header).map_err(|e| IndexError::Metadata {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
        if metadata.item_factory != T::ITEM_FACTORY {
            return Err(IndexError::ItemFactory {
                path: path.to_path_buf(),
                expected: T::ITEM_FACTORY,
                found: metadata.item_factory,
            });
        }
        let expected_len = metadata
            .num_pages
            .checked_mul(metadata.page_size)
            .and_then(|pages| pages.checked_add(METADATA_SIZE))
            .and_then(|len| u64::try_from(len).ok());
        let actual_len = file.metadata().map_err(io_error(path))?.len();
        if expected_len != Some(actual_len) {
            return Err(IndexError::Metadata {
                path: path.to_path_buf(),
                reason: format!(
                    "the file is {actual_len} bytes but {} pages of {} bytes need {}",
                    metadata.num_pages,
                    metadata.page_size,
                    expected_len.map_or_else(|| "too many".to_string(), |n| format!("{n}"))
                ),
            });
        }
        Ok(Self::from_parts(file, path, metadata))
    }

    #[must_use]
    pub fn metadata(&self) -> &IndexMetadata {
        &self.inner.metadata
    }

    #[must_use]
    pub fn num_pages(&self) -> usize {
        self.inner.metadata.num_pages
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// The page a key is stored on.
    #[must_use]
    pub fn page_for_key(&self, key: &str) -> usize {
        hash::page_for_key(key, self.num_pages())
    }

    fn offset(&self, page: usize) -> Result<u64, IndexError> {
        let metadata = self.metadata();
        let out_of_range = || IndexError::PageOutOfRange {
            path: self.inner.path.clone(),
            page,
            num_pages: metadata.num_pages,
        };
        if page >= metadata.num_pages {
            return Err(out_of_range());
        }
        page.checked_mul(metadata.page_size)
            .and_then(|bytes| bytes.checked_add(METADATA_SIZE))
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or_else(out_of_range)
    }

    fn page_error(&self, page: usize) -> impl FnOnce(PageIoError) -> IndexError + '_ {
        move |source| IndexError::Page {
            path: self.inner.path.clone(),
            page,
            source,
        }
    }

    fn read_filtered(&self, page: usize, term: Option<&str>) -> Result<Vec<T>, IndexError> {
        let offset = self.offset(page)?;
        let bytes = page::read_page_bytes(&self.inner.file, offset, self.metadata().page_size)
            .map_err(self.page_error(page))?;
        page::decode_items::<T>(&bytes, term).map_err(self.page_error(page))
    }

    /// The items stored under `term`, best first. The page may be shared with other terms,
    /// so this filters to the ones indexed under `term`.
    ///
    /// An unreadable page is retried, since the single writer may be mid-write.
    pub fn retrieve(&self, term: &str) -> Result<Vec<T>, IndexError> {
        let page = self.page_for_key(term);
        let mut attempt = 1;
        loop {
            match self.read_filtered(page, Some(term)) {
                Err(IndexError::Page {
                    source: PageIoError::Unreadable(_),
                    ..
                }) if attempt < READ_ATTEMPTS => attempt += 1,
                result => return result,
            }
        }
    }

    /// Every item on a page, whatever its term.
    pub fn read_page(&self, page: usize) -> Result<Vec<T>, IndexError> {
        self.read_filtered(page, None)
    }

    /// Every page in order, with its number.
    pub fn pages(&self) -> impl Iterator<Item = (usize, Result<Vec<T>, IndexError>)> + '_ {
        (0..self.num_pages()).map(|page| (page, self.read_page(page)))
    }
}

/// The one writer of an index file.
pub struct IndexWriter<T: IndexItem> {
    index: TinyIndex<T>,
}

impl<T: IndexItem> std::fmt::Debug for IndexWriter<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexWriter")
            .field("index", &self.index)
            .finish()
    }
}

/// Best score first, stable, with NaN last.
fn by_score_descending<T: IndexItem>(a: &T, b: &T) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a.score().is_nan(), b.score().is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => b.score().partial_cmp(&a.score()).unwrap_or(Ordering::Equal),
    }
}

fn tuples<T: IndexItem>(items: &[T]) -> Vec<Value> {
    items
        .iter()
        .map(|item| Value::Array(item.to_tuple()))
        .collect()
}

impl<T: IndexItem> IndexWriter<T> {
    /// Open an existing index for reading and writing.
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(io_error(path))?;
        Ok(Self {
            index: TinyIndex::load(file, path)?,
        })
    }

    /// A read handle sharing this writer's file.
    #[must_use]
    pub fn reader(&self) -> TinyIndex<T> {
        self.index.clone()
    }

    fn write(&self, page: usize, items: &[T]) -> Result<usize, IndexError> {
        let offset = self.index.offset(page)?;
        page::write_index_page(
            &self.index.inner.file,
            offset,
            self.index.metadata().page_size,
            &tuples(items),
        )
        .map_err(self.index.page_error(page))
    }

    /// Merge items onto their pages. Each item is stored on the page of its term; a new item
    /// replaces any on the page with the same (url, term). Pages are kept best-first and what
    /// does not fit is dropped from the bottom.
    ///
    /// Every item must have a term; if any does not, nothing is written.
    pub fn add(&mut self, items: Vec<T>) -> Result<AddStats, IndexError> {
        let mut by_page: BTreeMap<usize, Vec<T>> = BTreeMap::new();
        for item in items {
            let Some(term) = item.term() else {
                return Err(IndexError::NoTerm {
                    url: item.url().to_string(),
                });
            };
            by_page
                .entry(self.index.page_for_key(term))
                .or_default()
                .push(item);
        }

        let mut stats = AddStats::default();
        for (page, new_items) in by_page {
            // An unreadable page is an error, never an empty page: merging onto one would
            // overwrite everything that was there.
            let existing = self.index.read_page(page)?;
            // Later copies win over earlier ones in the same batch, too.
            let mut seen: HashSet<(String, Option<String>)> = HashSet::new();
            let mut fresh: Vec<T> = Vec::with_capacity(new_items.len());
            for item in new_items.into_iter().rev() {
                if seen.insert((item.url().to_string(), item.term().map(str::to_string))) {
                    fresh.push(item);
                }
            }
            fresh.reverse();
            let mut merged: Vec<T> = existing
                .into_iter()
                .filter(|old| {
                    !seen.contains(&(old.url().to_string(), old.term().map(str::to_string)))
                })
                .collect();
            merged.extend(fresh);
            merged.sort_by(by_score_descending);
            let stored = self.write(page, &merged)?;
            stats.pages_written += 1;
            stats.stored += stored;
            stats.dropped += merged.len().saturating_sub(stored);
        }
        Ok(stats)
    }

    /// Remove every item with this `url` from the pages of `terms`, whatever term each was
    /// indexed under, and return how many were removed. Pages are only rewritten if
    /// something was removed.
    pub fn remove(&mut self, url: &str, terms: &[&str]) -> Result<usize, IndexError> {
        let pages: BTreeSet<usize> = terms.iter().map(|t| self.index.page_for_key(t)).collect();
        let mut removed = 0;
        for page in pages {
            let mut items = self.index.read_page(page)?;
            let before = items.len();
            items.retain(|item| item.url() != url);
            let gone = before - items.len();
            if gone > 0 {
                self.write(page, &items)?;
                removed += gone;
            }
        }
        Ok(removed)
    }

    /// Overwrite a page with `items` as given (best first) and return how many fitted.
    pub fn write_page(&mut self, page: usize, items: &[T]) -> Result<usize, IndexError> {
        self.write(page, items)
    }

    /// Overwrite a page with an empty one, e.g. to recover from corruption.
    pub fn reset_page(&mut self, page: usize) -> Result<(), IndexError> {
        self.write(page, &[]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use crate::item::Document;

    use super::*;

    fn doc(url: &str, term: &str, score: Option<f64>) -> Document {
        Document {
            title: String::new(),
            url: url.into(),
            extract: String::new(),
            score,
            term: Some(term.into()),
            state: None,
            user_ids: None,
            last_crawled: None,
            source: None,
        }
    }

    fn writer(dir: &tempfile::TempDir, pages: usize) -> IndexWriter<Document> {
        TinyIndex::create(&dir.path().join("i"), pages, 4096, None).unwrap()
    }

    #[test]
    fn nan_and_missing_scores_sort_last_and_ties_are_stable() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer(&dir, 1);
        w.add(vec![
            doc("nan", "t", Some(f64::NAN)),
            doc("tie1", "t", Some(1.0)),
            doc("none", "t", None),
            doc("tie2", "t", Some(1.0)),
            doc("best", "t", Some(9.0)),
        ])
        .unwrap();
        let urls: Vec<_> = w
            .reader()
            .read_page(0)
            .unwrap()
            .into_iter()
            .map(|d| d.url)
            .collect();
        assert_eq!(urls, ["best", "tie1", "tie2", "none", "nan"]);
    }

    #[test]
    fn a_later_copy_in_the_same_batch_wins() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer(&dir, 1);
        let stats = w
            .add(vec![doc("u", "t", Some(1.0)), doc("u", "t", Some(2.0))])
            .unwrap();
        assert_eq!(
            (stats.pages_written, stats.stored, stats.dropped),
            (1, 1, 0)
        );
        assert_eq!(w.reader().read_page(0).unwrap()[0].score, Some(2.0));
    }

    #[test]
    fn a_failed_add_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer(&dir, 4);
        let mut bad = doc("bad", "y", Some(1.0));
        bad.term = None;
        assert!(w.add(vec![doc("ok", "x", Some(1.0)), bad]).is_err());
        assert!(w.reader().pages().all(|(_, p)| p.unwrap().is_empty()));
    }

    #[test]
    fn writer_reopens_and_errors_name_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer(&dir, 2);
        w.add(vec![doc("u", "t", Some(1.0))]).unwrap();
        drop(w);
        let path = dir.path().join("i");
        let mut w = IndexWriter::<Document>::open(&path).unwrap();
        assert_eq!(w.reader().retrieve("t").unwrap().len(), 1);
        let error = w.write_page(2, &[]).unwrap_err();
        assert!(matches!(
            error,
            IndexError::PageOutOfRange {
                page: 2,
                num_pages: 2,
                ..
            }
        ));
        assert!(error.to_string().contains(path.to_str().unwrap()));
    }

    #[test]
    fn a_file_that_is_not_an_index_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("junk");
        std::fs::write(&path, vec![7u8; 5000]).unwrap();
        assert!(matches!(
            TinyIndex::<Document>::open(&path),
            Err(IndexError::Metadata { .. })
        ));
        std::fs::write(&path, b"short").unwrap();
        assert!(matches!(
            TinyIndex::<Document>::open(&path),
            Err(IndexError::Metadata { .. })
        ));
    }

    #[test]
    fn readers_are_send_and_sync_for_any_item() {
        fn check<T: Send + Sync>() {}
        check::<TinyIndex<Document>>();
    }
}
