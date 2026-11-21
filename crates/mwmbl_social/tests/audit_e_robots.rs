//! Audit E (2026-11-19): robots.txt matching cost a remote host controls. Fails until fixed.

use std::time::{Duration, Instant};

use mwmbl_social::robots;

/// E10: `robots::matches` backtracks on `*` in O(pattern x path) per rule, and a host's
/// robots.txt may be up to `MAX_ROBOTS_BYTES` (512 KiB) of rules. A hostile host serves
/// rules that never match and object URIs with long paths; every `allows` check (run for each
/// fetch of its URIs) then burns CPU proportional to robots size x path length.
#[test]
#[ignore = "AUDIT: E10 - fails until fixed"]
fn robots_matching_is_cheap_on_hostile_rules() {
    let rule = format!("Disallow: /*{}b\n", "a".repeat(4_000));
    let text = format!("User-agent: *\n{}", rule.repeat(120));
    assert!(text.len() <= 512 * 1024);
    let rules = robots::parse(&text, "mwmblsocial");
    let path = format!("/{}", "a".repeat(4_000));
    let started = Instant::now();
    let allowed = rules.allows(&path);
    let took = started.elapsed();
    assert!(allowed);
    assert!(
        took < Duration::from_millis(100),
        "one robots check took {took:?}"
    );
}
