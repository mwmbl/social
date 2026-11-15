//! Query-time retrieval and ranking.
//!
//! Retrieval looks up every key `tokenize::query_keys` gives, merges the copies by URI, and
//! drops anything tombstoned. Ranking is deliberately simple (PLAN.md: port only the simple
//! parts of Mwmbl's `rank.py`): how much of the query matched, with bigrams and handles worth
//! more than single words, then the index-time score and, for posts, freshness.

use std::collections::HashMap;

use tinyindex::IndexError;
use tokenize::{field_keys, query_keys, special_keys, tokenize};

use crate::index::SocialReader;
use crate::item::{SocialAccount, SocialPost};
use crate::keys::Keyed;
use crate::tombstones::{Tombstones, host_of};

/// A candidate and the query keys it was found under.
struct Candidate<T> {
    item: T,
    matched: Vec<String>,
}

fn is_unigram(key: &str) -> bool {
    !key.contains(' ') && !key.starts_with('#') && !key.starts_with('@')
}

/// Look up every key and merge by URI, keeping the most recently checked copy's fields.
fn gather<T: Keyed>(
    reader: &SocialReader<T>,
    keys: &[String],
    tombstones: &Tombstones,
) -> Result<Vec<Candidate<T>>, IndexError> {
    let mut by_uri: HashMap<String, Candidate<T>> = HashMap::new();
    for key in keys {
        for copy in reader.retrieve(key)? {
            if tombstones.blocks(&copy) {
                continue;
            }
            match by_uri.get_mut(copy.uri()) {
                Some(candidate) => {
                    if !candidate.matched.contains(key) {
                        candidate.matched.push(key.clone());
                    }
                    if copy.checked() > candidate.item.checked() {
                        candidate.item = copy;
                    }
                }
                None => {
                    by_uri.insert(
                        copy.uri().to_string(),
                        Candidate {
                            item: copy,
                            matched: vec![key.clone()],
                        },
                    );
                }
            }
        }
    }
    Ok(by_uri.into_values().collect())
}

/// How well the matched keys cover the query: unigrams count 1, bigrams and special keys 2,
/// and matching every unigram doubles it.
fn match_score(query: &[String], matched: &[String]) -> f64 {
    let weight = |k: &String| if is_unigram(k) { 1.0 } else { 2.0 };
    let total: f64 = matched.iter().map(weight).sum();
    let unigrams: Vec<&String> = query.iter().filter(|k| is_unigram(k)).collect();
    let all = !unigrams.is_empty() && unigrams.iter().all(|u| matched.contains(u));
    if all { total * 2.0 } else { total }
}

/// From 1 for brand new down towards 0, halving every `half_life_days`.
fn freshness(published: i64, now: i64, half_life_days: f64) -> f64 {
    #[allow(clippy::cast_precision_loss)] // Seconds of age; precision beyond 2^52 s is moot.
    let age_days = (now - published).max(0) as f64 / 86_400.0;
    0.5_f64.powf(age_days / half_life_days)
}

fn quality(score: f64) -> f64 {
    if score.is_finite() && score > 0.0 {
        score.ln_1p()
    } else {
        0.0
    }
}

fn sorted<T>(mut scored: Vec<(f64, T)>) -> Vec<T> {
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.into_iter().map(|(_, item)| item).collect()
}

/// Posts matching `query`, best first. `now` is Unix seconds.
pub fn search_posts(
    reader: &SocialReader<SocialPost>,
    query: &str,
    tombstones: &Tombstones,
    now: i64,
) -> Result<Vec<SocialPost>, IndexError> {
    let keys = query_keys(query);
    let scored = gather(reader, &keys, tombstones)?
        .into_iter()
        .map(|c| {
            let rank = match_score(&keys, &c.matched)
                * (1.0 + 0.1 * quality(c.item.score))
                * (0.5 + freshness(c.item.published, now, 14.0));
            (rank, c.item)
        })
        .collect();
    Ok(sorted(scored))
}

/// Whether an account's observed languages pass an RFC 4647 basic filter. An account whose
/// languages we haven't observed passes: we rarely know them, and excluding everyone
/// unknown would make the filter return almost nothing.
#[must_use]
pub fn language_matches(languages: Option<&[String]>, range: &str) -> bool {
    let range = range.trim().to_ascii_lowercase();
    if range.is_empty() || range == "*" {
        return true;
    }
    let Some(languages) = languages.filter(|l| !l.is_empty()) else {
        return true;
    };
    languages.iter().any(|tag| {
        let tag = tag.to_ascii_lowercase();
        tag == range
            || tag
                .strip_prefix(range.as_str())
                .is_some_and(|rest| rest.starts_with('-'))
    })
}

/// Accounts matching `query`, best first: an exact handle or host match, then an exact
/// username, then username and display-name word matches, then summary matches, each weighted by the index-time score
/// (followers and activity).
pub fn search_accounts(
    reader: &SocialReader<SocialAccount>,
    query: &str,
    tombstones: &Tombstones,
    language: Option<&str>,
) -> Result<Vec<SocialAccount>, IndexError> {
    let keys = query_keys(query);
    let trimmed = query.trim().trim_start_matches('@').to_lowercase();
    let scored = gather(reader, &keys, tombstones)?
        .into_iter()
        .filter(|c| language.is_none_or(|l| language_matches(c.item.languages.as_deref(), l)))
        .map(|c| {
            let a = &c.item;
            let acct = a.acct.to_lowercase();
            let (user, host) = acct.split_once('@').unwrap_or((&acct, ""));
            let handle_keys = special_keys(&format!("@{}", a.acct));
            let user_keys = tokenize(user);
            let display_keys = field_keys(&a.display_name, &tokenize::KeyConfig::default());
            let tier = if trimmed == acct
                || trimmed == host
                || c.matched.iter().any(|k| handle_keys.contains(k))
                || host_of(&a.uri).is_some_and(|h| h == trimmed)
            {
                1000.0
            } else if trimmed == user {
                100.0
            } else if c
                .matched
                .iter()
                .any(|k| user_keys.contains(k) || display_keys.contains(k))
            {
                10.0
            } else {
                1.0
            };
            let rank = tier * match_score(&keys, &c.matched) * (1.0 + 0.1 * quality(a.score));
            (rank, c.item)
        })
        .collect();
    Ok(sorted(scored))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::SocialIndex;
    use crate::item::tests::{account, post};
    use crate::keys::KeyScheme;

    const DAY: i64 = 86_400;

    fn posts(items: &[SocialPost]) -> (tempfile::TempDir, SocialIndex<SocialPost>) {
        let dir = tempfile::tempdir().unwrap();
        let mut index =
            SocialIndex::create(&dir.path().join("p"), 256, 4096, KeyScheme::default()).unwrap();
        for item in items {
            index.add(item).unwrap();
        }
        (dir, index)
    }

    fn uris<T: Keyed>(items: &[T]) -> Vec<&str> {
        items.iter().map(Keyed::uri).collect()
    }

    #[test]
    fn all_terms_beat_some_and_phrases_beat_scattered_words() {
        let now = 100 * DAY;
        let mut a = post("https://h.example/s/a", "river garden");
        let mut b = post("https://h.example/s/b", "garden party by the river");
        let mut c = post("https://h.example/s/c", "garden only");
        for p in [&mut a, &mut b, &mut c] {
            p.published = now;
        }
        let (_d, index) = posts(&[c, b, a]);
        let found = search_posts(&index.reader(), "river garden", &Tombstones::new(), now).unwrap();
        assert_eq!(
            uris(&found),
            [
                "https://h.example/s/a",
                "https://h.example/s/b",
                "https://h.example/s/c"
            ]
        );
    }

    #[test]
    fn fresher_wins_at_equal_match() {
        let now = 100 * DAY;
        let mut old = post("https://h.example/s/old", "coffee");
        old.published = now - 60 * DAY;
        let mut new = post("https://h.example/s/new", "coffee");
        new.published = now - DAY;
        let (_d, index) = posts(&[old, new]);
        let found = search_posts(&index.reader(), "coffee", &Tombstones::new(), now).unwrap();
        assert_eq!(
            uris(&found),
            ["https://h.example/s/new", "https://h.example/s/old"]
        );
    }

    #[test]
    fn tombstoned_and_blocked_results_are_filtered_and_results_deduped() {
        let a = post("https://h.example/s/a", "coffee tea");
        let mut b = post("https://h.example/s/b", "coffee tea");
        b.author_uri = "https://h.example/users/b".into();
        let mut c = post("https://bad.example/s/c", "coffee tea");
        c.author_uri = "https://bad.example/users/c".into();
        let (_d, index) = posts(&[a, b, c]);
        let mut t = Tombstones::new();
        let found = search_posts(&index.reader(), "coffee tea", &t, 0).unwrap();
        assert_eq!(
            found.len(),
            3,
            "one result per URI though each matched three keys"
        );
        t.add_author("https://h.example/users/b");
        t.add_domain("bad.example");
        let found = search_posts(&index.reader(), "coffee tea", &t, 0).unwrap();
        assert_eq!(uris(&found), ["https://h.example/s/a"]);
    }

    #[test]
    fn hashtag_queries() {
        let a = post("https://h.example/s/a", "planting garlic #gardening");
        let b = post("https://h.example/s/b", "gardening is hard work");
        let (_d, index) = posts(&[b, a]);
        let found = search_posts(&index.reader(), "#gardening", &Tombstones::new(), 0).unwrap();
        assert_eq!(uris(&found)[0], "https://h.example/s/a");
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn accounts_rank_handle_then_name_then_summary() {
        let dir = tempfile::tempdir().unwrap();
        let mut index: SocialIndex<SocialAccount> =
            SocialIndex::create(&dir.path().join("a"), 256, 4096, KeyScheme::default()).unwrap();
        let mut by_summary = account("https://x.example/users/z", "z@x.example", "Zed");
        by_summary.summary = "I like rust".into();
        by_summary.score = 1000.0;
        let by_name = account("https://y.example/users/q", "q@y.example", "Rust Fan");
        let by_handle = account("https://rust.example/users/rust", "rust@rust.example", "R");
        for a in [&by_summary, &by_name, &by_handle] {
            index.add(a).unwrap();
        }
        let r = index.reader();
        let found = search_accounts(&r, "rust", &Tombstones::new(), None).unwrap();
        assert_eq!(uris(&found)[1], "https://y.example/users/q");
        assert_eq!(uris(&found)[2], "https://x.example/users/z");
        assert_eq!(uris(&found)[0], "https://rust.example/users/rust");
        let found = search_accounts(&r, "@q@y.example", &Tombstones::new(), None).unwrap();
        assert_eq!(uris(&found)[0], "https://y.example/users/q");
        let found = search_accounts(&r, "Q@Y.EXAMPLE", &Tombstones::new(), None).unwrap();
        assert_eq!(uris(&found)[0], "https://y.example/users/q");
    }

    #[test]
    fn account_language_filter() {
        let langs = |l: &[&str]| l.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert!(language_matches(Some(&langs(&["en-GB"])), "en"));
        assert!(language_matches(Some(&langs(&["de", "EN"])), "en"));
        assert!(!language_matches(Some(&langs(&["eng"])), "en"));
        assert!(!language_matches(Some(&langs(&["de"])), "en"));
        assert!(language_matches(None, "en"), "unknown passes");
        assert!(language_matches(Some(&langs(&["de"])), "*"));
        let dir = tempfile::tempdir().unwrap();
        let mut index: SocialIndex<SocialAccount> =
            SocialIndex::create(&dir.path().join("a"), 64, 4096, KeyScheme::default()).unwrap();
        let mut de = account("https://h.example/users/de", "de@h.example", "Kaffee");
        de.languages = Some(langs(&["de"]));
        index.add(&de).unwrap();
        let r = index.reader();
        assert_eq!(
            search_accounts(&r, "kaffee", &Tombstones::new(), Some("en"))
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            search_accounts(&r, "kaffee", &Tombstones::new(), Some("de"))
                .unwrap()
                .len(),
            1
        );
    }
}
