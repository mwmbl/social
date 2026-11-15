//! Mwmbl Social: the server, its background workers and the CLI.

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

pub mod decide;
pub mod records;
