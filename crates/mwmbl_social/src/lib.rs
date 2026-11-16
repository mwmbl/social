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

pub mod activities;
pub mod cli;
pub mod config;
pub mod context;
pub mod crawl;
pub mod crawl_app;
pub mod decide;
pub mod delivery;
pub mod fasp_app;
pub mod follows;
pub mod http;
pub mod inbox_app;
pub mod keys;
pub mod metrics;
pub mod outbox;
pub mod pacing;
pub mod records;
pub mod robots;
pub mod search;
pub mod sweep;
pub mod worker;
pub mod writer;

/// The current time in Unix seconds.
#[must_use]
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// The current time in Unix milliseconds.
#[must_use]
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}
