//! Inverted index pages in the format of Mwmbl's `TinyIndex`.
//!
//! This crate is fediverse-agnostic and is meant to be upstreamed to Mwmbl. It reads pages
//! written by Mwmbl's Python as [`Document`], and nothing here knows about `ActivityPub`: URI
//! lookup, re-tokenising and tombstones belong a layer above it.
//!
//! An index file is a 4096-byte header (see [`metadata`]) followed by fixed-size pages. Each
//! page is a zstd-compressed JSON array of item tuples; a key (a term) lives on page
//! [`hash::page_for_key`]. [`TinyIndex`] reads, and the one [`IndexWriter`] writes.

#![deny(
    clippy::exit,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::panic,
    clippy::string_slice,
    clippy::unimplemented,
    clippy::unreachable,
    clippy::todo,
    clippy::pedantic
)]
// As in mwmbl_core: every public fn here returns Result, and the error types say what failed.
#![allow(clippy::missing_errors_doc)]

pub mod hash;
pub mod index;
pub mod item;
pub mod metadata;
pub mod page;

pub use index::{AddStats, IndexError, IndexWriter, TinyIndex};
pub use item::{Document, IndexItem};
pub use metadata::IndexMetadata;
pub use page::PageIoError;
