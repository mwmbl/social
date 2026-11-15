//! Page I/O.
//!
//! A page of the index is a zstd frame holding a JSON array of item tuples, padded with
//! zeros to the page size. Reading one is a positioned read, a decompress and a parse, and
//! writing one is a serialise, a compress and a positioned write. Positioned reads and writes
//! take `&File`, so any number of threads can share one open file.
//!
//! This is `mwmbl_core`'s `index.rs`, with raw file descriptors replaced by `&File` and the
//! term position passed in rather than fixed at `Document`'s.

use std::cell::RefCell;
use std::fs::File;
use std::os::unix::fs::FileExt;

use serde_json::Value;
use zstd::bulk::{Compressor, Decompressor};
use zstd::zstd_safe;

use crate::item::IndexItem;

/// python-zstandard's default level, which is what wrote every page in the index today.
const COMPRESSION_LEVEL: i32 = 3;

/// How much bigger than the page itself its contents are allowed to claim to be. See
/// `decompress_page`: it bounds what a corrupt frame header can make this allocate.
const MAX_PAGE_EXPANSION: usize = 1024;

/// Everything that can go wrong reading or writing a page, split by what callers should do:
/// a failed syscall is an I/O error, and a page that will not decode is unreadable, which
/// callers must never treat as an empty page.
#[derive(Debug, thiserror::Error)]
pub enum PageIoError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unreadable page: {0}")]
    Unreadable(String),
}

/// A read that came up short is a page that is not there rather than a failed syscall: a
/// positioned read stops at the end of the file, so it means the file is not the shape the
/// metadata says it is - truncated, or half-written by create. Decoding the fragment anyway
/// is how a writer comes to merge onto a page it never actually read.
fn read_error(error: std::io::Error) -> PageIoError {
    if error.kind() == std::io::ErrorKind::UnexpectedEof {
        return PageIoError::Unreadable(format!("could not read a whole page: {error}"));
    }
    PageIoError::Io(error)
}

pub fn read_page_bytes(file: &File, offset: u64, page_size: usize) -> Result<Vec<u8>, PageIoError> {
    let mut page = vec![0u8; page_size];
    // read_exact_at rather than read_at: it reads until it has the whole page, and it
    // retries a read the kernel interrupted with a signal, which is what os.pread does
    // since PEP 475 and what a bare read_at does not.
    file.read_exact_at(&mut page, offset).map_err(read_error)?;
    Ok(page)
}

thread_local! {
    /// One decompression context per thread, created on first use and reused for every page
    /// it reads. Building one allocates a working buffer and costs about as much as
    /// decompressing a page, and each call decompresses a whole frame on its own, so there is
    /// no state to carry between reads and a failed page cannot poison the next one.
    static DECOMPRESSOR: RefCell<Option<Decompressor<'static>>> = const { RefCell::new(None) };

    /// And one compression context, for the same reason. Packing a page compresses about
    /// a dozen prefixes of the items to find the ones that fit, so building a context
    /// per compression cost more than the compression did.
    static COMPRESSOR: RefCell<Option<Compressor<'static>>> = const { RefCell::new(None) };
}

/// Decompress the one zstd frame at the start of the page, ignoring whatever follows it -
/// which is the zero padding that brings the page up to its fixed size.
fn decompress_page(page: &[u8]) -> Result<Vec<u8>, PageIoError> {
    let frame_size = zstd_safe::find_frame_compressed_size(page).map_err(zstd_error)?;
    let Some(frame) = page.get(..frame_size) else {
        return Err(PageIoError::Unreadable("Invalid frame size".to_string()));
    };
    // Every frame the index writes carries its content size, so this is the exact size of
    // the buffer to decompress into rather than a guess.
    let contents_size: usize = zstd_safe::get_frame_content_size(frame)
        .map_err(|e| PageIoError::Unreadable(e.to_string()))?
        .ok_or_else(|| PageIoError::Unreadable("The frame has no content size".to_string()))?
        .try_into()
        .map_err(|e: std::num::TryFromIntError| PageIoError::Unreadable(e.to_string()))?;
    // That size is data like anything else on the page, and the buffer is allocated before
    // a byte is decompressed, so a corrupt header must not be able to ask for a gigabyte.
    // Real pages expand by well under ten times; this is far past that and still bounded.
    let largest_readable = page.len().saturating_mul(MAX_PAGE_EXPANSION);
    if contents_size > largest_readable {
        return Err(PageIoError::Unreadable(format!(
            "The frame claims {contents_size} bytes, more than the {largest_readable} a page can hold"
        )));
    }
    DECOMPRESSOR.with_borrow_mut(|slot| {
        if slot.is_none() {
            *slot = Some(Decompressor::new()?);
        }
        let Some(decompressor) = slot.as_mut() else {
            return Err(PageIoError::Unreadable(
                "no zstd decompression context".to_string(),
            ));
        };
        decompressor
            .decompress(frame, contents_size)
            .map_err(|e| PageIoError::Unreadable(e.to_string()))
    })
}

/// zstd reports a failure as an error code, and the text for it is all there is to say.
fn zstd_error(code: usize) -> PageIoError {
    PageIoError::Unreadable(zstd_safe::get_error_name(code).to_string())
}

/// Decode a page into its raw tuples, keeping only those that belong to `term` if given.
///
/// `term_position` is where the term sits in each tuple; see [`has_term`].
pub fn decode_page(
    page: &[u8],
    term: Option<&str>,
    term_position: usize,
) -> Result<Vec<Value>, PageIoError> {
    let contents = decompress_page(page)?;
    // Damage that gets past zstd lands on the parse instead, and it is the same kind of
    // unreadable. Invalid UTF-8 fails here too, as the decode in Python did.
    let items: Vec<Value> =
        serde_json::from_slice(&contents).map_err(|e| PageIoError::Unreadable(e.to_string()))?;
    match term {
        None => Ok(items),
        Some(term) => Ok(items
            .into_iter()
            .filter(|item| has_term(item, term, term_position))
            .collect()),
    }
}

/// Decode a page into items of type `T`, filtered by `term` at `T::TERM_POSITION`.
///
/// A tuple that is not an array, or that `T::from_tuple` refuses, is skipped rather than
/// failing the page: one bad entry must not hide the rest.
pub fn decode_items<T: IndexItem>(page: &[u8], term: Option<&str>) -> Result<Vec<T>, PageIoError> {
    Ok(decode_page(page, term, T::TERM_POSITION)?
        .into_iter()
        .filter_map(|tuple| match tuple {
            Value::Array(values) => T::from_tuple(values).ok(),
            _ => None,
        })
        .collect())
}

/// Whether a tuple belongs to this term: its own, or no term at all. Filtering on the raw
/// tuple means a page's items that another term put there are never built into objects.
/// `position` is where the term sits in the tuple; a truncated tuple may have nothing there.
#[must_use]
pub fn has_term(item: &Value, term: &str, position: usize) -> bool {
    match item.get(position) {
        None | Some(Value::Null) => true,
        Some(Value::String(item_term)) => item_term == term,
        Some(_) => false,
    }
}

fn compress_prefix(serialised: &[String], count: usize) -> Result<Vec<u8>, PageIoError> {
    let mut json = String::from("[");
    let Some(bounded) = serialised.get(..count) else {
        return Err(PageIoError::Unreadable("Out of bound read".to_string()));
    };
    for (position, item) in bounded.iter().enumerate() {
        if position > 0 {
            json.push(',');
        }
        json.push_str(item);
    }
    json.push(']');
    COMPRESSOR.with_borrow_mut(|slot| {
        if slot.is_none() {
            *slot = Some(Compressor::new(COMPRESSION_LEVEL)?);
        }
        let Some(compressor) = slot.as_mut() else {
            return Err(PageIoError::Unreadable(
                "no zstd compression context".to_string(),
            ));
        };
        compressor
            .compress(json.as_bytes())
            .map_err(PageIoError::Io)
    })
}

/// The padded page bytes, and how many of `items` actually fit on them.
///
/// A page holds a fixed number of bytes, and how many items that is depends on how
/// well they compress, so the largest prefix that fits is found by binary search over the
/// compressed size. The tail that does not fit is dropped, which is why callers pass their
/// items best-first.
pub fn pack_page(page_size: usize, items: &[Value]) -> Result<(Vec<u8>, usize), PageIoError> {
    let serialised: Vec<String> = items.iter().map(std::string::ToString::to_string).collect();
    // One compression context for the whole search: the binary search compresses a dozen
    // prefixes, and building a context each time would cost more than the compression.
    let mut low = 0;
    let mut high = items.len();
    let mut best = (0, compress_prefix(&serialised, 0)?);
    while low <= high {
        let count = usize::midpoint(low, high);
        let compressed = compress_prefix(&serialised, count)?;
        if compressed.len() > page_size {
            if count == 0 {
                break;
            }
            high = count - 1;
        } else {
            best = (count, compressed);
            low = count + 1;
        }
    }

    let (num_stored, mut page) = best;
    if page.len() > page_size {
        return Err(PageIoError::Unreadable(format!(
            "Data is too big ({}) for page size ({page_size})",
            page.len()
        )));
    }
    page.resize(page_size, 0);
    Ok((page, num_stored))
}

/// Read the page at `offset`, returning its raw tuples.
///
/// Pass `term` (with the tuple position it sits at) to keep only the items that belong to
/// it - the ones whose term is that term, or which have none. Fails with
/// [`PageIoError::Unreadable`] if the page will not decode, which means it is corrupt or was
/// caught half-written; it deliberately does not fall back to an empty page, because a
/// writer that merged onto one would store its result over everything that was there.
pub fn read_index_page(
    file: &File,
    offset: u64,
    page_size: usize,
    term: Option<&str>,
    term_position: usize,
) -> Result<Vec<Value>, PageIoError> {
    let page = read_page_bytes(file, offset, page_size)?;
    decode_page(&page, term, term_position)
}

/// Write `values` over the page at `offset`, returning how many of them fitted.
pub fn write_index_page(
    file: &File,
    offset: u64,
    page_size: usize,
    values: &[Value],
) -> Result<usize, PageIoError> {
    let (page, num_stored) = pack_page(page_size, values)?;
    // write_all_at rather than write_at: a short write would leave the page half old
    // and half new, which is the torn page every reader and writer here is guarding
    // against, so it writes the rest rather than reporting it - and, like the read, it
    // retries a write a signal interrupted. What is left is a real failure of the
    // device.
    file.write_all_at(&page, offset).map_err(PageIoError::Io)?;
    Ok(num_stored)
}
