//! Per-host pacing for the fetch worker: which due queue items to start now, which to leave
//! queued untouched, and which revalidations and crawl submissions to move later so that one
//! host's backlog is laid out over time instead of filling the worker or hitting the host at
//! the fetcher's full rate. Crawl submissions are paced at the host's `Crawl-delay` when it asks
//! for more than the revalidation interval.
//!
//! Pure: the worker hands in the due candidates and what is in flight, and applies the
//! [`Plan`]. Times here are Unix milliseconds; the queue's `not_before` is Unix seconds.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use apub::fetch::uri_host_key;
use store::{FetchReason, QueuedFetch};

/// A revalidation whose host's next slot is at most this far off is left queued untouched
/// (one per host per pass) rather than deferred: `not_before` is whole seconds, so a nearer
/// slot cannot be written to the queue.
pub const HOLD_WINDOW_MS: i64 = 1000;
/// When the pacer tracks more hosts than this, hosts whose slots have all passed are dropped.
pub const PACER_HOST_CAP: usize = 10_000;
/// The worker reads up to this many due candidates per free slot, so items for busy hosts can
/// be passed over without leaving slots idle.
pub const CANDIDATES_PER_SLOT: usize = 4;
/// At most this many candidates (plus those in flight) are read per pass.
pub const MAX_CANDIDATES: usize = 256;

/// How many due items to read for a pass: in-flight items are still queued, so they are read
/// too and skipped.
#[must_use]
pub fn candidate_limit(free: usize, in_flight: usize) -> usize {
    free.saturating_mul(CANDIDATES_PER_SLOT)
        .min(MAX_CANDIDATES)
        .saturating_add(in_flight)
}

/// What the pacer says about one due revalidation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pace {
    /// Start it now.
    Start,
    /// Leave it queued as it is; its host's slot is less than [`HOLD_WINDOW_MS`] away.
    Hold,
    /// Move it in the queue to this slot.
    Defer { until_ms: i64 },
}

#[derive(Debug, Clone, Copy, Default)]
struct HostPace {
    /// The earliest time the next revalidation to this host may start.
    next_start: i64,
    /// The first slot not yet given to a deferred item.
    next_free: i64,
    /// The pass in which an item for this host was last held.
    held_in: u64,
}

/// Per-host budgets: revalidations to one host start at most once per interval, and crawl
/// submissions at most once per the larger of that interval and the host's `Crawl-delay`. The
/// two are budgeted apart, so a host's crawl backlog does not hold up its revalidations. In
/// memory only; losing it on restart costs at most one burst of a few fetches per host.
#[derive(Debug)]
pub struct Pacer {
    interval_ms: i64,
    hosts: HashMap<String, HostPace>,
    crawl_hosts: HashMap<String, HostPace>,
    pass: u64,
}

impl Pacer {
    /// A zero interval disables the revalidation budget, and leaves crawl submissions paced by
    /// `Crawl-delay` alone.
    #[must_use]
    pub fn new(interval: Duration) -> Self {
        Self {
            interval_ms: i64::try_from(interval.as_millis()).unwrap_or(i64::MAX),
            hosts: HashMap::new(),
            crawl_hosts: HashMap::new(),
            pass: 0,
        }
    }

    /// Call once per loop pass, before asking about that pass's candidates.
    pub fn begin_pass(&mut self, now_ms: i64) {
        self.pass = self.pass.wrapping_add(1);
        for hosts in [&mut self.hosts, &mut self.crawl_hosts] {
            if hosts.len() > PACER_HOST_CAP {
                // A host whose slots have all passed is the same as one never seen.
                hosts.retain(|_, h| h.next_start > now_ms || h.next_free > now_ms);
            }
        }
    }

    /// How many hosts are tracked, for revalidation and crawl submissions together.
    #[must_use]
    pub fn len(&self) -> usize {
        self.hosts.len().saturating_add(self.crawl_hosts.len())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty() && self.crawl_hosts.is_empty()
    }

    /// Decide one due revalidation for `host`. `Start` and `Defer` take a slot. `can_start` is
    /// false when the host has no fetch capacity free right now: the item is then held or
    /// deferred like one whose slot has not come, so a busy host's backlog still leaves the due
    /// set instead of crowding other hosts out of the candidates.
    pub fn revalidation(&mut self, host: &str, now_ms: i64, can_start: bool) -> Pace {
        let pass = self.pass;
        slot(
            &mut self.hosts,
            host,
            self.interval_ms,
            pass,
            now_ms,
            can_start,
        )
    }

    /// Decide one due crawl submission for `host`, as [`Pacer::revalidation`] does, with slots
    /// `max(revalidation interval, crawl_delay_secs)` apart.
    pub fn crawl(
        &mut self,
        host: &str,
        crawl_delay_secs: Option<u64>,
        now_ms: i64,
        can_start: bool,
    ) -> Pace {
        let delay_ms = crawl_delay_secs.map_or(0, |secs| {
            i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX)
        });
        let interval = self.interval_ms.max(delay_ms);
        let pass = self.pass;
        slot(
            &mut self.crawl_hosts,
            host,
            interval,
            pass,
            now_ms,
            can_start,
        )
    }
}

/// The pacing decision for one item to `host`, whose fetches start at most once per
/// `interval` ms (zero or less: unbudgeted).
fn slot(
    hosts: &mut HashMap<String, HostPace>,
    host: &str,
    interval: i64,
    pass: u64,
    now_ms: i64,
    can_start: bool,
) -> Pace {
    if interval <= 0 {
        return if can_start { Pace::Start } else { Pace::Hold };
    }
    let h = hosts.entry(host.to_owned()).or_default();
    if can_start && now_ms >= h.next_start {
        h.next_start = now_ms.saturating_add(interval);
        return Pace::Start;
    }
    let ready = h.next_start.max(now_ms);
    let held_now = h.held_in == pass;
    if !held_now && ready.saturating_sub(now_ms) <= HOLD_WINDOW_MS {
        h.held_in = pass;
        return Pace::Hold;
    }
    // `ready` is taken by the item held this pass, if there is one.
    let first = if held_now {
        ready.saturating_add(interval)
    } else {
        ready
    };
    let next = h.next_free.max(first);
    h.next_free = next.saturating_add(interval);
    Pace::Defer { until_ms: next }
}

/// Unix milliseconds rounded up to Unix seconds, so a deferred item never comes due before its
/// slot.
#[must_use]
pub fn ceil_secs(ms: i64) -> i64 {
    ms.saturating_add(999).div_euclid(1000)
}

/// A due item to start now. `host` is [`uri_host_key`] of its URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToStart {
    pub item: QueuedFetch,
    pub host: Option<String>,
}

/// A revalidation or crawl submission to move to `not_before` (Unix seconds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToDefer {
    pub item: QueuedFetch,
    pub host: String,
    pub not_before: i64,
}

/// What one pass of the worker loop does with the candidates it read. Anything in neither list
/// stays queued as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub start: Vec<ToStart>,
    pub defer: Vec<ToDefer>,
}

/// A host's `Crawl-delay` in seconds, when its robots.txt is known and asks for one.
pub type CrawlDelays<'a> = &'a dyn Fn(&str) -> Option<u64>;

/// What is running now, as the worker sees it.
#[derive(Clone, Copy)]
pub struct Running<'a> {
    pub uris: &'a HashSet<String>,
    /// Fetches in flight per host key.
    pub hosts: &'a HashMap<String, usize>,
    /// Free worker slots.
    pub free: usize,
    /// The fetcher's per-host concurrency.
    pub per_host: usize,
    /// Each host's `Crawl-delay`, for pacing crawl submissions.
    pub crawl_delays: CrawlDelays<'a>,
}

/// Go through `candidates` in queue order and decide each:
///
/// - in flight already: skip;
/// - a revalidation or a crawl submission: ask the [`Pacer`] (start, hold or defer);
/// - anything else whose host already has `per_host` fetches in flight (counting those started
///   this pass): skip, so it does not hold a worker slot waiting on the fetcher's per-host
///   limit;
/// - anything else: start.
///
/// Stops once `free` items are to start. A URI with no host key is started as before; the
/// fetch reports it.
pub fn plan(
    candidates: Vec<QueuedFetch>,
    running: Running<'_>,
    pacer: &mut Pacer,
    now_ms: i64,
) -> Plan {
    let mut out = Plan::default();
    let mut started: HashMap<String, usize> = HashMap::new();
    for item in candidates {
        if out.start.len() >= running.free {
            break;
        }
        if running.uris.contains(&item.uri) {
            continue;
        }
        let Some(host) = uri_host_key(&item.uri) else {
            out.start.push(ToStart { item, host: None });
            continue;
        };
        let busy = running
            .hosts
            .get(&host)
            .copied()
            .unwrap_or(0)
            .saturating_add(started.get(&host).copied().unwrap_or(0));
        let can_start = busy < running.per_host;
        let pace = match item.reason {
            FetchReason::Revalidate => Some(pacer.revalidation(&host, now_ms, can_start)),
            FetchReason::Crawl => {
                Some(pacer.crawl(&host, (running.crawl_delays)(&host), now_ms, can_start))
            }
            FetchReason::Announce | FetchReason::Backfill => None,
        };
        if let Some(pace) = pace {
            match pace {
                Pace::Start => {}
                Pace::Hold => continue,
                Pace::Defer { until_ms } => {
                    out.defer.push(ToDefer {
                        item,
                        host,
                        not_before: ceil_secs(until_ms),
                    });
                    continue;
                }
            }
        } else if !can_start {
            continue;
        }
        *started.entry(host.clone()).or_default() += 1;
        out.start.push(ToStart {
            item,
            host: Some(host),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use store::FetchKind;

    use super::*;

    fn due(uri: &str, reason: FetchReason) -> QueuedFetch {
        QueuedFetch {
            uri: uri.to_owned(),
            kind: FetchKind::Object,
            reason,
            priority: reason.priority(),
            record: None,
            not_before: 0,
            attempts: 0,
            last_error: None,
            enqueued_at: 0,
        }
    }

    fn uris_started(plan: &Plan) -> Vec<&str> {
        plan.start.iter().map(|s| s.item.uri.as_str()).collect()
    }

    fn running<'a>(
        uris: &'a HashSet<String>,
        hosts: &'a HashMap<String, usize>,
        free: usize,
    ) -> Running<'a> {
        Running {
            uris,
            hosts,
            free,
            per_host: 2,
            crawl_delays: &|_| None,
        }
    }

    #[test]
    fn busy_hosts_are_passed_over_not_waited_on() {
        let uris = HashSet::from(["https://a.example/0".to_owned()]);
        let hosts = HashMap::from([("a.example".to_owned(), 1)]);
        let candidates = vec![
            due("https://a.example/0", FetchReason::Announce),
            due("https://a.example/1", FetchReason::Announce),
            due("https://A.example/2", FetchReason::Announce),
            due("https://a.example/3", FetchReason::Backfill),
            due("https://b.example:8443/1", FetchReason::Announce),
            due("nonsense", FetchReason::Announce),
            due("https://c.example/1", FetchReason::Backfill),
        ];
        let mut pacer = Pacer::new(Duration::ZERO);
        let plan = plan(candidates, running(&uris, &hosts, 3), &mut pacer, 0);
        // a/0 is in flight; a/1 makes a.example's second; a/2 and a/3 wait in the queue.
        assert_eq!(
            uris_started(&plan),
            [
                "https://a.example/1",
                "https://b.example:8443/1",
                "nonsense"
            ]
        );
        assert_eq!(
            plan.start
                .iter()
                .map(|s| s.host.as_deref())
                .collect::<Vec<_>>(),
            [Some("a.example"), Some("b.example:8443"), None]
        );
        assert_eq!(plan.defer, Vec::new());
    }

    #[test]
    fn revalidations_take_slots_and_others_do_not() {
        let none = HashSet::new();
        let no_hosts = HashMap::new();
        let mut pacer = Pacer::new(Duration::from_millis(1000));
        pacer.begin_pass(10_000);
        let mut candidates: Vec<QueuedFetch> = (0..5)
            .map(|i| due(&format!("https://a.example/r{i}"), FetchReason::Revalidate))
            .collect();
        candidates.push(due("https://a.example/ann", FetchReason::Announce));
        candidates.push(due("https://b.example/r", FetchReason::Revalidate));
        let plan = plan(candidates, running(&none, &no_hosts, 8), &mut pacer, 10_000);
        // r0 starts; r1 is held for the slot at 11 000; r2.. go to 12 000, 13 000, 14 000.
        // The announce is not budgeted, and b.example has its own budget.
        assert_eq!(
            uris_started(&plan),
            [
                "https://a.example/r0",
                "https://a.example/ann",
                "https://b.example/r"
            ]
        );
        let deferred: Vec<(&str, i64)> = plan
            .defer
            .iter()
            .map(|d| (d.item.uri.as_str(), d.not_before))
            .collect();
        assert_eq!(
            deferred,
            [
                ("https://a.example/r2", 12),
                ("https://a.example/r3", 13),
                ("https://a.example/r4", 14)
            ]
        );
        assert!(plan.defer.iter().all(|d| d.host == "a.example"));
    }

    #[test]
    fn a_busy_hosts_revalidations_are_laid_out_not_left_due() {
        let uris = HashSet::from([
            "https://a.example/0".to_owned(),
            "https://a.example/1".to_owned(),
        ]);
        let hosts = HashMap::from([("a.example".to_owned(), 2)]);
        let mut pacer = Pacer::new(Duration::from_millis(1000));
        pacer.begin_pass(5000);
        let candidates = (2..5)
            .map(|i| due(&format!("https://a.example/{i}"), FetchReason::Revalidate))
            .collect();
        let plan = plan(candidates, running(&uris, &hosts, 4), &mut pacer, 5000);
        assert_eq!(plan.start, Vec::new());
        // a/2 is held for when a fetch finishes; the rest are spread from a second on.
        let deferred: Vec<i64> = plan.defer.iter().map(|d| d.not_before).collect();
        assert_eq!(deferred, [6, 7]);
        // With no budget, a busy host's items are only passed over.
        let mut pacer = Pacer::new(Duration::ZERO);
        let candidates = (2..5)
            .map(|i| due(&format!("https://a.example/{i}"), FetchReason::Revalidate))
            .collect();
        assert_eq!(
            plan_of(candidates, &uris, &hosts, &mut pacer),
            Plan::default()
        );
    }

    fn plan_of(
        candidates: Vec<QueuedFetch>,
        uris: &HashSet<String>,
        hosts: &HashMap<String, usize>,
        pacer: &mut Pacer,
    ) -> Plan {
        plan(candidates, running(uris, hosts, 4), pacer, 0)
    }

    #[test]
    fn no_more_than_free_are_started() {
        let none = HashSet::new();
        let no_hosts = HashMap::new();
        let candidates = (0..5)
            .map(|i| due(&format!("https://h{i}.example/x"), FetchReason::Announce))
            .collect();
        let mut pacer = Pacer::new(Duration::ZERO);
        let plan = plan(candidates, running(&none, &no_hosts, 2), &mut pacer, 0);
        assert_eq!(
            uris_started(&plan),
            ["https://h0.example/x", "https://h1.example/x"]
        );
    }

    #[test]
    fn slots_are_one_interval_apart_and_held_items_start_on_time() {
        let mut p = Pacer::new(Duration::from_millis(200));
        p.begin_pass(0);
        assert_eq!(p.revalidation("a", 0, true), Pace::Start);
        assert_eq!(p.revalidation("a", 0, true), Pace::Hold);
        assert_eq!(p.revalidation("a", 0, true), Pace::Defer { until_ms: 400 });
        assert_eq!(p.revalidation("a", 0, true), Pace::Defer { until_ms: 600 });
        assert_eq!(p.revalidation("b", 0, true), Pace::Start);
        // Next pass, before the slot: the held item is held again, not deferred.
        p.begin_pass(100);
        assert_eq!(p.revalidation("a", 100, true), Pace::Hold);
        // At its slot it starts.
        p.begin_pass(200);
        assert_eq!(p.revalidation("a", 200, true), Pace::Start);
        // The deferred ones come due at their slots and start there.
        p.begin_pass(400);
        assert_eq!(p.revalidation("a", 400, true), Pace::Start);
        p.begin_pass(600);
        assert_eq!(p.revalidation("a", 600, true), Pace::Start);
        // A new item after the backlog is laid out goes after it.
        p.begin_pass(610);
        assert_eq!(p.revalidation("a", 610, true), Pace::Hold);
        assert_eq!(
            p.revalidation("a", 610, true),
            Pace::Defer { until_ms: 1000 }
        );
    }

    #[test]
    fn a_far_slot_is_deferred_to_rather_than_held() {
        let mut p = Pacer::new(Duration::from_secs(5));
        p.begin_pass(0);
        assert_eq!(p.revalidation("a", 0, true), Pace::Start);
        // 5 s away is beyond the hold window: the next item gets that slot in the queue.
        assert_eq!(p.revalidation("a", 0, true), Pace::Defer { until_ms: 5000 });
        assert_eq!(
            p.revalidation("a", 0, true),
            Pace::Defer { until_ms: 10_000 }
        );
        // Once the backlog's slots have passed, the host starts afresh.
        p.begin_pass(20_000);
        assert_eq!(p.revalidation("a", 20_000, true), Pace::Start);
        assert_eq!(
            p.revalidation("a", 20_000, true),
            Pace::Defer { until_ms: 25_000 }
        );
    }

    #[test]
    fn a_zero_interval_disables_the_budget() {
        let mut p = Pacer::new(Duration::ZERO);
        p.begin_pass(0);
        for _ in 0..3 {
            assert_eq!(p.revalidation("a", 0, true), Pace::Start);
        }
        assert!(p.is_empty());
    }

    #[test]
    fn the_host_map_is_pruned_of_spent_hosts() {
        let mut p = Pacer::new(Duration::from_millis(100));
        p.begin_pass(0);
        for i in 0..=PACER_HOST_CAP {
            p.revalidation(&format!("h{i}"), 0, true);
        }
        assert_eq!(p.len(), PACER_HOST_CAP + 1);
        // At 50 ms every slot is still ahead: nothing goes.
        p.begin_pass(50);
        assert_eq!(p.len(), PACER_HOST_CAP + 1);
        p.revalidation("late", 150, true);
        p.begin_pass(200);
        assert_eq!(p.len(), 1, "only the host with a slot ahead is kept");
    }

    #[test]
    fn crawl_submissions_are_paced_at_the_larger_of_crawl_delay_and_the_interval() {
        let none = HashSet::new();
        let no_hosts = HashMap::new();
        let mut pacer = Pacer::new(Duration::from_millis(1000));
        pacer.begin_pass(0);
        let mut candidates: Vec<QueuedFetch> = (0..3)
            .map(|i| due(&format!("https://l.example/post/{i}"), FetchReason::Crawl))
            .collect();
        candidates
            .extend((0..3).map(|i| due(&format!("https://n.example/{i}"), FetchReason::Crawl)));
        candidates.push(due("https://l.example/r", FetchReason::Revalidate));
        let delays = |host: &str| (host == "l.example").then_some(60);
        let running = Running {
            crawl_delays: &delays,
            ..running(&none, &no_hosts, 8)
        };
        let plan = plan(candidates, running, &mut pacer, 0);
        // l.example asks for 60 s; n.example has no Crawl-delay, so the 1 s interval applies.
        // The revalidation has its own budget.
        assert_eq!(
            uris_started(&plan),
            [
                "https://l.example/post/0",
                "https://n.example/0",
                "https://l.example/r"
            ]
        );
        let deferred: Vec<(&str, i64)> = plan
            .defer
            .iter()
            .map(|d| (d.item.uri.as_str(), d.not_before))
            .collect();
        assert_eq!(
            deferred,
            [
                ("https://l.example/post/1", 60),
                ("https://l.example/post/2", 120),
                ("https://n.example/2", 2)
            ]
        );
        // With no interval and no Crawl-delay, crawl submissions are not budgeted.
        let mut p = Pacer::new(Duration::ZERO);
        p.begin_pass(0);
        assert_eq!(p.crawl("a", None, 0, true), Pace::Start);
        assert_eq!(p.crawl("a", None, 0, true), Pace::Start);
        assert_eq!(p.crawl("a", Some(5), 0, true), Pace::Start);
        assert_eq!(
            p.crawl("a", Some(5), 0, true),
            Pace::Defer { until_ms: 5000 }
        );
        assert_eq!(
            p.crawl("a", Some(u64::MAX), 0, false),
            Pace::Defer { until_ms: 10_000 }
        );
    }

    #[test]
    fn seconds_round_up() {
        assert_eq!(ceil_secs(0), 0);
        assert_eq!(ceil_secs(1), 1);
        assert_eq!(ceil_secs(1000), 1);
        assert_eq!(ceil_secs(1001), 2);
        assert_eq!(candidate_limit(2, 3), 11);
        assert_eq!(candidate_limit(1000, 3), MAX_CANDIDATES + 3);
    }
}
