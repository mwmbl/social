//! One step of the sweep: walk a page, delete what tombstones match, and hand back stale
//! records for revalidation.

use std::collections::HashSet;

use tinyindex::IndexError;

use crate::index::SocialIndex;
use crate::keys::Keyed;
use crate::tombstones::Tombstones;

/// URIs the sweep has already handled in the current page range. A record shows up once per
/// key it was indexed under, so without this each would be revalidated many times. It is
/// bounded: when full it is cleared, and the fetch queue's own URI dedupe catches repeats.
#[derive(Debug)]
pub struct SweepSeen {
    uris: HashSet<String>,
    capacity: usize,
}

impl SweepSeen {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            uris: HashSet::new(),
            capacity: capacity.max(1),
        }
    }

    /// Record a URI, returning whether it was new.
    pub fn insert(&mut self, uri: &str) -> bool {
        if self.uris.contains(uri) {
            return false;
        }
        if self.uris.len() >= self.capacity {
            self.uris.clear();
        }
        self.uris.insert(uri.to_string());
        true
    }

    pub fn clear(&mut self) {
        self.uris.clear();
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.uris.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.uris.is_empty()
    }
}

/// What one page of the sweep did.
#[derive(Debug, Clone, PartialEq)]
pub struct SweepOutcome<T> {
    /// URIs deleted everywhere because a tombstone matched.
    pub deleted: Vec<String>,
    /// Records last checked before the cutoff, with `term` cleared, for the fetch queue.
    pub revalidate: Vec<T>,
}

/// Sweep one page.
///
/// Each record not seen yet in this range is checked against the tombstones first; a match is
/// deleted from every page by re-tokenising it. Otherwise, if its `checked` is before
/// `stale_before`, it is returned for revalidation. Records are handled once per URI.
pub fn sweep_page<T: Keyed>(
    index: &mut SocialIndex<T>,
    page: usize,
    tombstones: &Tombstones,
    stale_before: i64,
    seen: &mut SweepSeen,
) -> Result<SweepOutcome<T>, IndexError> {
    let records = index.reader_index().read_page(page)?;
    let mut outcome = SweepOutcome {
        deleted: Vec::new(),
        revalidate: Vec::new(),
    };
    for record in records {
        if !seen.insert(record.uri()) {
            continue;
        }
        if tombstones.blocks(&record) {
            index.delete_record(&record)?;
            outcome.deleted.push(record.uri().to_string());
        } else if record.checked() < stale_before {
            outcome.revalidate.push(record.with_term(""));
        }
    }
    Ok(outcome)
}
