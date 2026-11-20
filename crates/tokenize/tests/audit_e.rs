//! Audit E (2026-11-19): tokeniser costs a stranger controls. These fail until fixed.

use std::time::{Duration, Instant};

/// E1: `GET /api/v1/search/posts?q=...` (unauthenticated) turns every distinct word, bigram
/// and hashtag of `q` into a key, and `social_index::search` reads and decompresses one page
/// per key. A URI can be ~64 KiB, so one request costs tens of thousands of page reads.
#[test]
#[ignore = "AUDIT: E1 - fails until fixed"]
fn query_keys_are_bounded() {
    let query: String = (0..10_000).map(|i| format!("w{i} ")).collect();
    assert!(query.len() < 64 * 1024, "fits in one URI");
    let keys = tokenize::query_keys(&query);
    assert!(keys.len() <= 64, "one query asks for {} page reads", keys.len());
}

/// E3: `special_keys` dedupes with `Vec::contains`, so it is quadratic in distinct hashtags
/// and mentions. It runs on whole search queries (`query_keys`) and on stored fields. 64 KiB
/// of distinct hashtags (one search URI) must not take more than a moment.
#[test]
#[ignore = "AUDIT: E3 - fails until fixed"]
fn special_keys_is_not_quadratic() {
    let text: String = (0..8_000).map(|i| format!("#t{i} ")).collect();
    let started = Instant::now();
    let keys = tokenize::special_keys(&text);
    let took = started.elapsed();
    assert_eq!(keys.len(), 8_000);
    assert!(text.len() < 64 * 1024, "fits in one search URI");
    assert!(
        took < Duration::from_millis(500),
        "special_keys took {took:?} on {} bytes",
        text.len()
    );
}
