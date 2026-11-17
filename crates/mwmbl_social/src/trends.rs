//! Trend state: how often servers have announced each post as `trending`, by hour, over the
//! last week.
//!
//! Bounded by design (CLAUDE.md principle 1): it lives in memory only, keeps at most
//! [`MAX_WINDOW_HOURS`] hourly buckets per URI, forgets buckets older than that, and holds at
//! most `cap` URIs, evicting the least-announced (then the least recently announced) when a new
//! one arrives at the cap. It holds URIs and counts, never object content, and is lost on
//! restart, which only means trends start empty again.
//!
//! It records URIs as announced, before we know whether we may index them, so it is never
//! served as is: the binary only returns URIs it finds indexed and not tombstoned.

use std::collections::{HashMap, VecDeque};

/// The longest window kept and served: the week the spec requires.
pub const MAX_WINDOW_HOURS: i64 = 168;

const HOUR: i64 = 3600;

#[derive(Debug, Default)]
struct Entry {
    /// `(hour, events)`, oldest first; hours are Unix seconds divided by 3600.
    buckets: VecDeque<(i64, u32)>,
    last_seen: i64,
}

impl Entry {
    fn prune(&mut self, current_hour: i64) {
        while self
            .buckets
            .front()
            .is_some_and(|(hour, _)| *hour <= current_hour - MAX_WINDOW_HOURS)
        {
            self.buckets.pop_front();
        }
    }

    /// Events in the last `hours` hours, the current one included.
    fn count_within(&self, current_hour: i64, hours: i64) -> u64 {
        self.buckets
            .iter()
            .filter(|(hour, _)| *hour > current_hour - hours)
            .map(|(_, n)| u64::from(*n))
            .sum()
    }

    fn total(&self) -> u64 {
        self.buckets.iter().map(|(_, n)| u64::from(*n)).sum()
    }
}

/// `trending` event counts by URI.
#[derive(Debug)]
pub struct TrendState {
    cap: usize,
    entries: HashMap<String, Entry>,
}

/// A URI and its event count in the asked-for window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trending {
    pub uri: String,
    pub count: u64,
}

impl TrendState {
    /// `cap` is the most URIs held (at least 1).
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            entries: HashMap::new(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Counts one `trending` event for `uri` at `now` (Unix seconds).
    pub fn record(&mut self, uri: &str, now: i64) {
        let hour = now.div_euclid(HOUR);
        if !self.entries.contains_key(uri) && self.entries.len() >= self.cap {
            self.prune(hour);
            if self.entries.len() >= self.cap {
                self.evict_one();
            }
        }
        let entry = self.entries.entry(uri.to_owned()).or_default();
        entry.prune(hour);
        match entry.buckets.back_mut() {
            Some((h, n)) if *h == hour => *n = n.saturating_add(1),
            _ => entry.buckets.push_back((hour, 1)),
        }
        entry.last_seen = entry.last_seen.max(now);
    }

    /// Drops buckets older than the window, and URIs left with none.
    fn prune(&mut self, current_hour: i64) {
        self.entries.retain(|_, entry| {
            entry.prune(current_hour);
            !entry.buckets.is_empty()
        });
    }

    fn evict_one(&mut self) {
        let victim = self
            .entries
            .iter()
            .min_by(|a, b| {
                (a.1.total(), a.1.last_seen, b.0).cmp(&(b.1.total(), b.1.last_seen, a.0))
            })
            .map(|(uri, _)| uri.clone());
        if let Some(uri) = victim {
            self.entries.remove(&uri);
        }
    }

    /// URIs with events in the last `hours` hours (clamped to 1..=[`MAX_WINDOW_HOURS`]),
    /// most events first, then most recently announced, then by URI.
    #[must_use]
    pub fn ranked(&self, now: i64, hours: i64) -> Vec<Trending> {
        let hour = now.div_euclid(HOUR);
        let hours = hours.clamp(1, MAX_WINDOW_HOURS);
        let mut ranked: Vec<(u64, i64, &str)> = self
            .entries
            .iter()
            .map(|(uri, e)| (e.count_within(hour, hours), e.last_seen, uri.as_str()))
            .filter(|(count, _, _)| *count > 0)
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(b.2)));
        ranked
            .into_iter()
            .map(|(count, _, uri)| Trending {
                uri: uri.to_owned(),
                count,
            })
            .collect()
    }
}

/// The spec's 1..=100 rank for a score (an event count, or a sum of them). It is an absolute
/// scale, not relative to the best result, so ranks from two instances of this software merge
/// correctly as the spec requires: `1 + 20·log2(1 + score)`, capped at 100, computed in
/// integers (linear between powers of two). One event ranks 21, two 31, three 41, seven 61,
/// fifteen 81, thirty-one or more 100.
#[must_use]
pub fn rank(score: u64) -> u8 {
    if score == 0 {
        return 1;
    }
    // ilog2 gives whole steps; interpolate between them in integers to stay monotonic.
    let x = score.saturating_add(1);
    let whole = u64::from(x.ilog2());
    let low = 1_u64 << whole;
    let fraction = (x - low).saturating_mul(20) / low;
    let value = 1 + 20 * whole + fraction;
    u8::try_from(value.min(100)).unwrap_or(100)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000 * HOUR + 1800;

    #[test]
    fn counts_by_window_and_ranks_by_count_then_recency() {
        let mut t = TrendState::new(100);
        for _ in 0..3 {
            t.record("https://h.example/a", NOW);
        }
        t.record("https://h.example/b", NOW - 2 * HOUR);
        t.record("https://h.example/b", NOW - 30 * HOUR);
        t.record("https://h.example/c", NOW - 5);
        let day = t.ranked(NOW, 24);
        let uris: Vec<_> = day.iter().map(|t| (t.uri.as_str(), t.count)).collect();
        assert_eq!(
            uris,
            [
                ("https://h.example/a", 3),
                ("https://h.example/c", 1),
                ("https://h.example/b", 1)
            ]
        );
        assert_eq!(t.ranked(NOW, 48)[1].uri, "https://h.example/b");
        assert_eq!(
            t.ranked(NOW, 1).len(),
            2,
            "b's events are older than an hour"
        );
    }

    #[test]
    fn buckets_older_than_a_week_are_forgotten() {
        let mut t = TrendState::new(100);
        t.record("https://h.example/old", NOW - (MAX_WINDOW_HOURS + 1) * HOUR);
        t.record("https://h.example/new", NOW);
        assert!(
            t.ranked(NOW, 1000)
                .iter()
                .all(|e| e.uri != "https://h.example/old")
        );
        // Pruned once the cap forces a clean-up.
        let mut small = TrendState::new(1);
        small.record("https://h.example/old", NOW - (MAX_WINDOW_HOURS + 1) * HOUR);
        small.record("https://h.example/new", NOW);
        assert_eq!(small.len(), 1);
        assert_eq!(small.ranked(NOW, 168)[0].uri, "https://h.example/new");
    }

    #[test]
    fn the_cap_evicts_the_least_announced() {
        let mut t = TrendState::new(2);
        t.record("https://h.example/hot", NOW);
        t.record("https://h.example/hot", NOW);
        t.record("https://h.example/cold", NOW);
        t.record("https://h.example/new", NOW + 1);
        assert_eq!(t.len(), 2);
        let uris: Vec<_> = t.ranked(NOW + 1, 24).into_iter().map(|e| e.uri).collect();
        assert_eq!(uris, ["https://h.example/hot", "https://h.example/new"]);
        // Buckets per URI stay within the window however long it keeps trending.
        let mut long = TrendState::new(1);
        for h in 0..1000 {
            long.record("https://h.example/x", NOW + h * HOUR);
        }
        let entry = long.entries.get("https://h.example/x").unwrap();
        assert!(entry.buckets.len() <= usize::try_from(MAX_WINDOW_HOURS).unwrap());
    }

    #[test]
    fn ranks_are_absolute_and_monotonic() {
        assert_eq!(rank(0), 1);
        assert_eq!(rank(1), 21);
        assert_eq!(rank(2), 31);
        assert_eq!(rank(3), 41);
        assert_eq!(rank(7), 61);
        assert_eq!(rank(15), 81);
        assert_eq!(rank(31), 100);
        assert_eq!(rank(u64::MAX), 100);
        let mut last = 0;
        for score in 0..200 {
            let r = rank(score);
            assert!(r >= last && (1..=100).contains(&r), "{score}");
            last = r;
        }
    }
}
