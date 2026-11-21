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
pub mod admin_app;
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
pub mod operator;
pub mod outbox;
pub mod pacing;
pub mod records;
pub mod registrations;
pub mod robots;
pub mod search;
pub mod sweep;
pub mod trends;
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

static LAST_STAMP: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// A time in Unix milliseconds that is strictly later than every earlier stamp in this process,
/// even within one millisecond. Fetch starts and tombstones both take one, so the writer can
/// tell whether a tombstone was made before or after the fetch an index decision came from.
#[must_use]
pub fn stamp_millis() -> i64 {
    use std::sync::atomic::Ordering;
    let now = now_millis();
    let mut last = LAST_STAMP.load(Ordering::SeqCst);
    loop {
        let next = now.max(last.saturating_add(1));
        match LAST_STAMP.compare_exchange_weak(last, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return next,
            Err(actual) => last = actual,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn stamps_are_strictly_increasing() {
        let mut last = super::stamp_millis();
        for _ in 0..1000 {
            let next = super::stamp_millis();
            assert!(next > last);
            last = next;
        }
    }
}
