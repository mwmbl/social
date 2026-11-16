//! Prometheus metrics in the text exposition format, written by hand.
//!
//! Served at `/metrics` on the main listener only when the `metrics` config is on. That path is
//! public there, so firewall it or proxy it to an internal-only address. Everything is bounded
//! operational state (queue, tombstones, registrations) or process counters; nothing here
//! reads the index beyond its size, and no content, URI or host appears in a label.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use social_index::{SocialAccount, SocialPost, SocialReader};
use store::{FetchReason, RegistrationStatus, Store, StoreError, TombstoneKind};

use crate::worker::FetchCounters;

const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Everything one scrape reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub queue_total: usize,
    pub queue_by_reason: BTreeMap<String, usize>,
    pub tombstones_object: usize,
    pub tombstones_author: usize,
    pub domain_blocks: usize,
    pub registrations_by_status: BTreeMap<String, usize>,
    /// Registrations with each capability enabled.
    pub capabilities_enabled: BTreeMap<String, usize>,
    pub fetch_outcomes: Vec<(&'static str, u64)>,
    /// Revalidations moved later to keep within a host's budget.
    pub revalidations_deferred: u64,
    /// `(index name, pages, page size in bytes)`.
    pub indexes: Vec<(&'static str, usize, usize)>,
}

/// Counts by name.
pub type Counts = BTreeMap<String, usize>;

/// Registrations by status (every status listed, zero included) and, per capability, how many
/// non-revoked registrations have it enabled.
pub fn registration_counts(store: &Store) -> Result<(Counts, Counts), StoreError> {
    let mut by_status: BTreeMap<String, usize> = [
        RegistrationStatus::Pending,
        RegistrationStatus::Active,
        RegistrationStatus::Revoked,
    ]
    .iter()
    .map(|status| (status.as_str().to_string(), 0))
    .collect();
    let mut capabilities: BTreeMap<String, usize> = BTreeMap::new();
    for registration in store.list_registrations()? {
        *by_status
            .entry(registration.status.as_str().to_string())
            .or_default() += 1;
        if registration.status == RegistrationStatus::Revoked {
            continue;
        }
        for (capability, _version) in store.enabled_capabilities(registration.id)? {
            *capabilities.entry(capability).or_default() += 1;
        }
    }
    Ok((by_status, capabilities))
}

/// Reads the SQLite part of a snapshot.
pub fn store_snapshot(store: &Store) -> Result<Snapshot, StoreError> {
    let tombstones = store.list_tombstones()?;
    let count = |kind| tombstones.iter().filter(|t| t.kind == kind).count();
    let mut queue_by_reason: BTreeMap<String, usize> = [
        FetchReason::Announce,
        FetchReason::Backfill,
        FetchReason::Revalidate,
    ]
    .iter()
    .map(|reason| (reason.as_str().to_string(), 0))
    .collect();
    queue_by_reason.extend(store.queue_len_by_reason()?);
    let (registrations_by_status, capabilities_enabled) = registration_counts(store)?;
    Ok(Snapshot {
        queue_total: store.queue_len()?,
        queue_by_reason,
        tombstones_object: count(TombstoneKind::Object),
        tombstones_author: count(TombstoneKind::Author),
        domain_blocks: store.list_domain_blocks()?.len(),
        registrations_by_status,
        capabilities_enabled,
        fetch_outcomes: Vec::new(),
        revalidations_deferred: 0,
        indexes: Vec::new(),
    })
}

/// One metric family: its header, then a sample per `(label value, number)` row. Without a
/// label name the single row's label value is ignored.
fn series<L: std::fmt::Display, V: std::fmt::Display>(
    out: &mut String,
    (name, kind, help): (&str, &str, &str),
    label: Option<&str>,
    rows: impl IntoIterator<Item = (L, V)>,
) {
    // Writing to a String cannot fail.
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
    for (text, value) in rows {
        let _ = match label {
            Some(key) => writeln!(out, "{name}{{{key}=\"{text}\"}} {value}"),
            None => writeln!(out, "{name} {value}"),
        };
    }
}

/// The Prometheus text for a snapshot. Label values are fixed names from this crate, never
/// anything received from a peer, so they need no escaping.
#[must_use]
pub fn render(snapshot: &Snapshot) -> String {
    let mut out = String::new();
    let out = &mut out;
    series(
        out,
        (
            "mwmbl_social_queue_depth_total",
            "gauge",
            "Rows in the fetch queue.",
        ),
        None,
        [("", snapshot.queue_total)],
    );
    series(
        out,
        (
            "mwmbl_social_queue_depth",
            "gauge",
            "Rows in the fetch queue, by reason.",
        ),
        Some("reason"),
        &snapshot.queue_by_reason,
    );
    series(
        out,
        (
            "mwmbl_social_tombstones",
            "gauge",
            "Tombstones not yet cleared by a sweep, by kind.",
        ),
        Some("kind"),
        [
            ("object", snapshot.tombstones_object),
            ("author", snapshot.tombstones_author),
        ],
    );
    series(
        out,
        ("mwmbl_social_domain_blocks", "gauge", "Blocked domains."),
        None,
        [("", snapshot.domain_blocks)],
    );
    series(
        out,
        (
            "mwmbl_social_registrations",
            "gauge",
            "Registrations with fediverse servers, by status.",
        ),
        Some("status"),
        &snapshot.registrations_by_status,
    );
    series(
        out,
        (
            "mwmbl_social_capabilities_enabled",
            "gauge",
            "Live registrations with a capability enabled.",
        ),
        Some("capability"),
        &snapshot.capabilities_enabled,
    );
    render_process(out, snapshot);
    std::mem::take(out)
}

/// The process counters and index sizes: what the scrape adds to the store snapshot.
fn render_process(out: &mut String, snapshot: &Snapshot) {
    series(
        out,
        (
            "mwmbl_social_fetch_outcomes_total",
            "counter",
            "Fetches by outcome since the process started.",
        ),
        Some("outcome"),
        snapshot.fetch_outcomes.iter().copied(),
    );
    series(
        out,
        (
            "mwmbl_social_revalidations_deferred_total",
            "counter",
            "Revalidations rescheduled to keep within a host's budget since the process started.",
        ),
        None,
        [("", snapshot.revalidations_deferred)],
    );
    series(
        out,
        (
            "mwmbl_social_index_pages",
            "gauge",
            "Pages in an index file.",
        ),
        Some("index"),
        snapshot
            .indexes
            .iter()
            .map(|&(name, pages, _)| (name, pages)),
    );
    series(
        out,
        (
            "mwmbl_social_index_size_bytes",
            "gauge",
            "Size of an index's page area in bytes.",
        ),
        Some("index"),
        snapshot
            .indexes
            .iter()
            .map(|&(name, pages, page_size)| (name, pages.saturating_mul(page_size))),
    );
}

/// What the `/metrics` handler reads. It has its own SQLite connection.
pub struct MetricsState {
    pub store: Mutex<Store>,
    pub counters: Arc<FetchCounters>,
    pub posts: SocialReader<SocialPost>,
    pub accounts: SocialReader<SocialAccount>,
}

impl MetricsState {
    fn snapshot(&self) -> Result<Snapshot, StoreError> {
        let mut snapshot = {
            let store = self.store.lock().unwrap_or_else(PoisonError::into_inner);
            store_snapshot(&store)?
        };
        snapshot.fetch_outcomes = self.counters.snapshot();
        snapshot.revalidations_deferred = self.counters.revalidations_deferred();
        snapshot.indexes = vec![
            (
                "posts",
                self.posts.index().num_pages(),
                self.posts.index().metadata().page_size,
            ),
            (
                "accounts",
                self.accounts.index().num_pages(),
                self.accounts.index().metadata().page_size,
            ),
        ];
        Ok(snapshot)
    }
}

/// The `/metrics` route.
pub fn router(state: Arc<MetricsState>) -> Router {
    Router::new()
        .route("/metrics", get(scrape))
        .with_state(state)
}

async fn scrape(State(state): State<Arc<MetricsState>>) -> Response {
    let rendered = tokio::task::spawn_blocking(move || state.snapshot().map(|s| render(&s))).await;
    let (status, body) = match rendered {
        Ok(Ok(text)) => (StatusCode::OK, text),
        Ok(Err(e)) => {
            tracing::error!(error = %e, "failed to collect metrics");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "metrics unavailable\n".to_string(),
            )
        }
        Err(e) => {
            tracing::error!(error = %e, "the metrics task failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "metrics unavailable\n".to_string(),
            )
        }
    };
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE));
    response
}

#[cfg(test)]
mod tests {
    use store::{NewFetch, NewRegistration};

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn render_has_the_expected_series() {
        let snapshot = Snapshot {
            queue_total: 3,
            queue_by_reason: [("announce".to_string(), 2), ("backfill".to_string(), 1)].into(),
            tombstones_object: 4,
            tombstones_author: 5,
            domain_blocks: 6,
            registrations_by_status: [("active".to_string(), 1)].into(),
            capabilities_enabled: [("data_sharing".to_string(), 1)].into(),
            fetch_outcomes: vec![("found", 9), ("gone", 0)],
            revalidations_deferred: 7,
            indexes: vec![("posts", 64, 4096)],
        };
        let text = render(&snapshot);
        for line in [
            "# TYPE mwmbl_social_queue_depth_total gauge",
            "mwmbl_social_queue_depth_total 3",
            "mwmbl_social_queue_depth{reason=\"announce\"} 2",
            "mwmbl_social_queue_depth{reason=\"backfill\"} 1",
            "mwmbl_social_tombstones{kind=\"object\"} 4",
            "mwmbl_social_tombstones{kind=\"author\"} 5",
            "mwmbl_social_domain_blocks 6",
            "mwmbl_social_registrations{status=\"active\"} 1",
            "mwmbl_social_capabilities_enabled{capability=\"data_sharing\"} 1",
            "# TYPE mwmbl_social_fetch_outcomes_total counter",
            "mwmbl_social_fetch_outcomes_total{outcome=\"found\"} 9",
            "mwmbl_social_fetch_outcomes_total{outcome=\"gone\"} 0",
            "# TYPE mwmbl_social_revalidations_deferred_total counter",
            "mwmbl_social_revalidations_deferred_total 7",
            "mwmbl_social_index_pages{index=\"posts\"} 64",
            "mwmbl_social_index_size_bytes{index=\"posts\"} 262144",
        ] {
            assert!(
                text.lines().any(|l| l == line),
                "missing {line:?} in\n{text}"
            );
        }
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn a_store_snapshot_counts_what_is_there() -> TestResult {
        let store = Store::open_in_memory()?;
        let empty = store_snapshot(&store)?;
        assert_eq!(empty.queue_total, 0);
        assert_eq!(empty.queue_by_reason.get("revalidate"), Some(&0));
        assert_eq!(empty.registrations_by_status.get("pending"), Some(&0));

        store.enqueue(
            &NewFetch {
                uri: "https://h.example/n/1".into(),
                kind: store::FetchKind::Object,
                reason: FetchReason::Backfill,
                record: None,
                not_before: 0,
            },
            0,
            10,
        )?;
        store.add_tombstone("https://h.example/n/2", TombstoneKind::Object, 0)?;
        store.add_domain_block("bad.example", 0)?;
        let registration = store.create_registration(NewRegistration {
            base_url: "https://m.example/api/fasp".into(),
            server_id: "sid".into(),
            our_private_key: vec![1; 32],
            created_at: 0,
        })?;
        store.set_capability(registration.id, "data_sharing", "0.1", true, 0)?;
        let snapshot = store_snapshot(&store)?;
        assert_eq!(snapshot.queue_total, 1);
        assert_eq!(snapshot.queue_by_reason.get("backfill"), Some(&1));
        assert_eq!(snapshot.tombstones_object, 1);
        assert_eq!(snapshot.domain_blocks, 1);
        assert_eq!(snapshot.registrations_by_status.get("pending"), Some(&1));
        assert_eq!(snapshot.capabilities_enabled.get("data_sharing"), Some(&1));

        store.set_registration_status(registration.id, RegistrationStatus::Revoked)?;
        let revoked = store_snapshot(&store)?;
        assert_eq!(revoked.registrations_by_status.get("revoked"), Some(&1));
        assert!(revoked.capabilities_enabled.is_empty());
        Ok(())
    }
}
