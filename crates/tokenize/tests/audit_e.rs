//! Audit E (2026-11-19): tokeniser costs a stranger controls.

use std::time::{Duration, Instant};

/// E1: `GET /api/v1/search/posts?q=...` (unauthenticated) turns every distinct word, bigram
/// and hashtag of `q` into a key, and `social_index::search` reads and decompresses one page
/// per key. A URI can be ~64 KiB, so one request costs tens of thousands of page reads.
#[test]
fn query_keys_are_bounded() {
    let query: String = (0..10_000).map(|i| format!("w{i} ")).collect();
    assert!(query.len() < 64 * 1024, "fits in one URI");
    let keys = tokenize::query_keys(&query);
    assert!(
        keys.len() <= 64,
        "one query asks for {} page reads",
        keys.len()
    );
}

/// E3: `special_keys` deduped with `Vec::contains`, so it was quadratic in distinct hashtags
/// and mentions. It runs on search queries and on stored fields, and a fetched object can hold
/// up to 1 MiB. A fixed time limit passed even while it was quadratic, so this compares sizes:
/// eight times the tags must cost well under the 64 times a quadratic scan would.
#[test]
fn special_keys_is_not_quadratic() {
    fn best_of_three(tags: usize) -> Duration {
        let text: String = (0..tags).map(|i| format!("#t{i} ")).collect();
        (0..3)
            .map(|_| {
                let started = Instant::now();
                let keys = tokenize::special_keys(&text);
                let took = started.elapsed();
                assert_eq!(keys.len(), tags);
                took
            })
            .min()
            .unwrap_or_default()
    }
    let small = best_of_three(8_000);
    let large = best_of_three(64_000);
    assert!(
        large < small.saturating_mul(24),
        "8x the tags took {large:?} against {small:?}"
    );
}
