//! Fediverse items on top of `tinyindex`: the post and account layouts, the keys each is
//! indexed under, lookup and deletion by URI, tombstone filtering and the sweep.
//!
//! The invariant everything here rests on: an item's keys are a pure function of its stored
//! fields and the key scheme, so re-tokenising a record read from a page gives exactly the
//! pages it was written to. That is what makes deletion possible.

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

pub mod index;
pub mod item;
pub mod keys;
pub mod sweep;
pub mod tombstones;

pub use index::{DeleteOutcome, SocialIndex, SocialIndexError};
pub use item::{SocialAccount, SocialPost};
pub use keys::{KeyScheme, Keyed};
pub use sweep::{SweepOutcome, SweepSeen, sweep_page};
pub use tombstones::Tombstones;
