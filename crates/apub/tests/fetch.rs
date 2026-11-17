//! `apub::fetch` against an in-process fake `ActivityPub` host. No network beyond 127.0.0.1.
//!
//! The fake verifies every incoming signature with `apub::signatures` against our public key, and
//! can be told to accept only RFC 9421, only draft-cavage-12, or either.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use apub::fetch::{
    FetchError, FetchOutcome, Fetcher, FetcherConfig, HostSnapshot, SignatureScheme, TextOutcome,
    user_agent,
};
use apub::signatures::keys::{RsaSigningKey, RsaVerifyingKey, VerifyingKey};
use apub::signatures::{cavage, rfc9421};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, Response};
use serde_json::{Value, json};

const KEY_ID: &str = "https://social.example/actor#main-key";

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).unwrap()
}

/// Our key: the 1024-bit cavage fixture, so the tests stay fast.
fn our_key() -> RsaSigningKey {
    RsaSigningKey::from_pem(&fixture("cavage12_private.pem")).unwrap()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Accepts {
    Either,
    Rfc9421Only,
    CavageOnly,
}

struct Fake {
    accepts: Accepts,
    public: RsaVerifyingKey,
    total: AtomicUsize,
    rfc9421: AtomicUsize,
    cavage: AtomicUsize,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    paths: Mutex<HashMap<String, usize>>,
    headers_seen: Mutex<Vec<(String, String)>>,
}

impl Fake {
    fn requests(&self) -> usize {
        self.total.load(Ordering::SeqCst)
    }

    fn hits(&self, path: &str) -> usize {
        self.paths.lock().unwrap().get(path).copied().unwrap_or(0)
    }
}

fn now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

fn respond(status: u16, headers: &[(&str, &str)], body: impl Into<Body>) -> Response<Body> {
    let mut builder = Response::builder().status(status);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(body.into()).unwrap()
}

fn json_response(status: u16, value: &Value) -> Response<Body> {
    respond(
        status,
        &[("content-type", "application/activity+json")],
        value.to_string(),
    )
}

/// Which scheme verified, or `None` if the request is not acceptably signed.
fn verify(fake: &Fake, request: &Request<Body>, host: &str) -> Option<SignatureScheme> {
    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
        .collect();
    let target = format!("http://{host}{}", request.uri());
    let lookup_key = |keyid: &str| (keyid == KEY_ID).then(|| fake.public.clone());
    let has = |name: &str| headers.iter().any(|(n, _)| n == name);
    if has("signature-input") {
        let policy = rfc9421::VerifyPolicy::new(&["@method", "@target-uri"], now(), 300);
        rfc9421::verify_request("GET", &target, &headers, &policy, |k| {
            lookup_key(k).map(VerifyingKey::RsaV15Sha256)
        })
        .ok()?;
        (fake.accepts != Accepts::CavageOnly).then_some(SignatureScheme::Rfc9421)
    } else if has("signature") {
        cavage::verify_request(
            "GET",
            &target,
            &headers,
            cavage::GET_HEADERS,
            lookup_key,
            now(),
            300,
        )
        .ok()?;
        (fake.accepts != Accepts::Rfc9421Only).then_some(SignatureScheme::Cavage)
    } else {
        None
    }
}

async fn handle(State(fake): State<Arc<Fake>>, request: Request<Body>) -> Response<Body> {
    fake.total.fetch_add(1, Ordering::SeqCst);
    let now_in_flight = fake.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    fake.max_in_flight
        .fetch_max(now_in_flight, Ordering::SeqCst);
    // `Body` is not `Sync`, so everything taken from the request happens before the first await.
    let response = match check(&fake, &request) {
        Some((host, path)) => route(&host, &path).await,
        None => respond(401, &[], "bad signature"),
    };
    fake.in_flight.fetch_sub(1, Ordering::SeqCst);
    response
}

fn check(fake: &Fake, request: &Request<Body>) -> Option<(String, String)> {
    let host = request
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let path = request.uri().path().to_owned();
    *fake.paths.lock().unwrap().entry(path.clone()).or_default() += 1;
    *fake.headers_seen.lock().unwrap() = request
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
        .collect();

    let scheme = verify(fake, request, &host)?;
    match scheme {
        SignatureScheme::Rfc9421 => fake.rfc9421.fetch_add(1, Ordering::SeqCst),
        SignatureScheme::Cavage => fake.cavage.fetch_add(1, Ordering::SeqCst),
    };
    Some((host, path))
}

async fn route(host: &str, path: &str) -> Response<Body> {
    let id = format!("http://{host}{path}");
    match path {
        "/refuse" => respond(403, &[], "no"),
        "/gone404" => respond(404, &[], "missing"),
        "/gone410" => json_response(410, &json!({"id": id, "type": "Tombstone"})),
        "/tombstone" => json_response(
            200,
            &json!({"id": id, "type": "Tombstone", "deleted": "2026-11-01T00:00:00Z"}),
        ),
        "/limited" => respond(429, &[("retry-after", "120")], "slow down"),
        "/limited-date" => respond(429, &[("retry-after", &httpdate_in(90))], "slow down"),
        "/err500" => respond(500, &[], "oops"),
        "/redirect" => respond(302, &[("location", "/obj/target")], ""),
        "/redirect-loop" => respond(302, &[("location", "/redirect-loop")], ""),
        "/redirect-chain" => respond(301, &[("location", "/redirect")], ""),
        "/redirect-away" => {
            let other = host.replace("127.0.0.1", "localhost");
            respond(
                302,
                &[("location", &format!("http://{other}/obj/target"))],
                "",
            )
        }
        "/spoof" => json_response(
            200,
            &json!({"id": "http://evil.example/obj/1", "type": "Note"}),
        ),
        "/noid" => json_response(200, &json!({"type": "Note"})),
        "/doc" => json_response(
            200,
            &json!({"version": "2.1", "software": {"name": "lemmy"}}),
        ),
        "/doc-redirect" => respond(302, &[("location", "/doc")], ""),
        "/big" => respond(
            200,
            &[("content-type", "application/activity+json")],
            vec![b' '; 2048],
        ),
        "/text" => respond(200, &[("content-type", "text/html")], "<html>hello</html>"),
        "/robots.txt" => respond(
            200,
            &[("content-type", "text/plain")],
            "User-agent: *\nDisallow: /private\n",
        ),
        "/robots-redirect" => respond(302, &[("location", "/robots.txt")], ""),
        "/teapot" => respond(418, &[], "short and stout"),
        "/slow" => {
            tokio::time::sleep(Duration::from_millis(100)).await;
            json_response(200, &json!({"id": id, "type": "Note"}))
        }
        _ => json_response(200, &json!({"id": id, "type": "Note", "content": "hello"})),
    }
}

fn httpdate_in(secs: i64) -> String {
    apub::signatures::http_date(now() + secs)
}

struct Harness {
    fake: Arc<Fake>,
    base: String,
    fetcher: Fetcher,
}

impl Harness {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn fetch(&self, path: &str) -> FetchOutcome {
        self.fetcher.fetch(&self.url(path)).await
    }

    fn host(&self) -> String {
        self.base.trim_start_matches("http://").to_owned()
    }
}

fn config() -> FetcherConfig {
    let mut config = FetcherConfig::new(
        KEY_ID.to_owned(),
        our_key(),
        user_agent("https://social.example/"),
    );
    config.allow_http = true;
    config.allow_private_addresses = true;
    config.min_interval_per_host = Duration::ZERO;
    config.timeout = Duration::from_secs(10);
    config
}

async fn serve(accepts: Accepts) -> (Arc<Fake>, String) {
    let fake = Arc::new(Fake {
        accepts,
        public: RsaVerifyingKey::from_pem(&fixture("cavage12_public.pem")).unwrap(),
        total: AtomicUsize::new(0),
        rfc9421: AtomicUsize::new(0),
        cavage: AtomicUsize::new(0),
        in_flight: AtomicUsize::new(0),
        max_in_flight: AtomicUsize::new(0),
        paths: Mutex::new(HashMap::new()),
        headers_seen: Mutex::new(Vec::new()),
    });
    let app = Router::new().fallback(handle).with_state(Arc::clone(&fake));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (fake, base)
}

async fn harness_with(accepts: Accepts, tweak: impl FnOnce(&mut FetcherConfig)) -> Harness {
    let (fake, base) = serve(accepts).await;
    let mut config = config();
    tweak(&mut config);
    Harness {
        fake,
        base,
        fetcher: Fetcher::new(config).unwrap(),
    }
}

async fn harness(accepts: Accepts) -> Harness {
    harness_with(accepts, |_| {}).await
}

// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn found_with_rfc9421() {
    let h = harness(Accepts::Either).await;
    let FetchOutcome::Found { json, final_uri } = h.fetch("/obj/1").await else {
        panic!("not found");
    };
    assert_eq!(json["content"], "hello");
    assert_eq!(final_uri, h.url("/obj/1"));
    assert_eq!(h.fake.requests(), 1);
    assert_eq!(h.fake.rfc9421.load(Ordering::SeqCst), 1);

    // What we send besides the signature.
    let seen = h.fake.headers_seen.lock().unwrap().clone();
    let header = |name: &str| seen.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone());
    assert!(
        header("accept")
            .unwrap()
            .starts_with("application/activity+json, application/ld+json")
    );
    assert!(
        header("accept")
            .unwrap()
            .contains("https://www.w3.org/ns/activitystreams")
    );
    assert_eq!(
        header("user-agent").unwrap(),
        format!(
            "mwmbl-social/{} (+https://social.example)",
            env!("CARGO_PKG_VERSION")
        )
    );
    assert!(header("date").is_some());
    assert_eq!(header("host").unwrap(), h.host());
}

#[tokio::test]
async fn scheme_fallback_is_cached() {
    let h = harness(Accepts::CavageOnly).await;
    assert!(matches!(
        h.fetch("/obj/1").await,
        FetchOutcome::Found { .. }
    ));
    // 9421 got a 401, then cavage worked.
    assert_eq!(h.fake.requests(), 2);
    assert_eq!(h.fake.cavage.load(Ordering::SeqCst), 1);

    // Remembered: one request now.
    assert!(matches!(
        h.fetch("/obj/2").await,
        FetchOutcome::Found { .. }
    ));
    assert_eq!(h.fake.requests(), 3);
    assert_eq!(h.fake.cavage.load(Ordering::SeqCst), 2);

    let states = h.fetcher.host_states();
    assert_eq!(
        states,
        vec![(
            h.host(),
            HostSnapshot {
                scheme: Some(SignatureScheme::Cavage),
                backoff_until: None
            }
        )]
    );
}

#[tokio::test]
async fn remembered_scheme_failing_tries_the_other_once() {
    let h = harness(Accepts::Rfc9421Only).await;
    // We believe this host wants cavage; it now only takes 9421.
    h.fetcher.seed_host(
        &h.host(),
        HostSnapshot {
            scheme: Some(SignatureScheme::Cavage),
            backoff_until: None,
        },
    );
    assert!(matches!(
        h.fetch("/obj/1").await,
        FetchOutcome::Found { .. }
    ));
    assert_eq!(h.fake.requests(), 2);
    assert_eq!(
        h.fetcher.host_states()[0].1.scheme,
        Some(SignatureScheme::Rfc9421)
    );
    assert!(matches!(
        h.fetch("/obj/2").await,
        FetchOutcome::Found { .. }
    ));
    assert_eq!(h.fake.requests(), 3);
}

#[tokio::test]
async fn gone() {
    let h = harness(Accepts::Either).await;
    for path in ["/gone404", "/gone410", "/tombstone"] {
        assert_eq!(h.fetch(path).await, FetchOutcome::Gone, "{path}");
    }
    // A host that says "gone" is healthy: no backoff.
    assert!(matches!(
        h.fetch("/obj/1").await,
        FetchOutcome::Found { .. }
    ));
}

#[tokio::test]
async fn refused_after_both_schemes() {
    let h = harness(Accepts::Either).await;
    assert_eq!(
        h.fetch("/refuse").await,
        FetchOutcome::Refused { status: 403 }
    );
    assert_eq!(h.fake.hits("/refuse"), 2, "9421 then cavage, no more");
    // Refusal is about the object, not the host: no backoff, and nothing was remembered as working.
    assert!(matches!(
        h.fetch("/obj/1").await,
        FetchOutcome::Found { .. }
    ));
}

#[tokio::test]
async fn refused_when_signatures_are_never_accepted() {
    // A fake that rejects our key entirely: use a different key id by accepting nothing.
    let (fake, base) = serve(Accepts::Either).await;
    let mut cfg = config();
    cfg.key_id = "https://other.example/actor#main-key".to_owned();
    let fetcher = Fetcher::new(cfg).unwrap();
    let outcome = fetcher.fetch(&format!("{base}/obj/1")).await;
    assert_eq!(outcome, FetchOutcome::Refused { status: 401 });
    assert_eq!(fake.requests(), 2);
}

#[tokio::test]
async fn rate_limit_then_backoff_without_sending() {
    let h = harness(Accepts::Either).await;
    assert_eq!(
        h.fetch("/limited").await,
        FetchOutcome::RateLimited {
            retry_after_secs: 120
        }
    );
    let before = h.fake.requests();
    let FetchOutcome::Backoff { until } = h.fetch("/obj/1").await else {
        panic!("expected backoff");
    };
    let remaining = until - now();
    assert!((100..=121).contains(&remaining), "{remaining}");
    assert_eq!(h.fake.requests(), before, "a backing-off host gets nothing");

    let states = h.fetcher.host_states();
    assert_eq!(states[0].1.backoff_until, Some(until));
}

#[tokio::test]
async fn retry_after_http_date() {
    let h = harness(Accepts::Either).await;
    let FetchOutcome::RateLimited { retry_after_secs } = h.fetch("/limited-date").await else {
        panic!("expected 429");
    };
    assert!((85..=91).contains(&retry_after_secs), "{retry_after_secs}");
}

#[tokio::test]
async fn server_errors_back_off_exponentially() {
    let h = harness_with(Accepts::Either, |c| {
        c.default_backoff = Duration::from_secs(10)
    })
    .await;
    assert!(matches!(
        h.fetch("/err500").await,
        FetchOutcome::Error(FetchError::Status { status: 500, .. })
    ));
    let FetchOutcome::Backoff { until } = h.fetch("/obj/1").await else {
        panic!("expected backoff");
    };
    assert!((8..=11).contains(&(until - now())));
    assert_eq!(h.fake.requests(), 1);
}

#[tokio::test]
async fn seeded_backoff_is_honoured_and_expired_one_is_not() {
    let h = harness(Accepts::Either).await;
    let until = now() + 600;
    h.fetcher.seed_host(
        &h.host(),
        HostSnapshot {
            scheme: None,
            backoff_until: Some(until),
        },
    );
    assert_eq!(h.fetch("/obj/1").await, FetchOutcome::Backoff { until });
    assert_eq!(h.fake.requests(), 0);

    h.fetcher.seed_host(
        &h.host(),
        HostSnapshot {
            scheme: Some(SignatureScheme::Rfc9421),
            backoff_until: Some(now() - 5),
        },
    );
    assert!(matches!(
        h.fetch("/obj/1").await,
        FetchOutcome::Found { .. }
    ));
    // Success clears it.
    assert_eq!(h.fetcher.host_states()[0].1.backoff_until, None);
}

#[tokio::test]
async fn same_host_redirect_is_followed() {
    let h = harness(Accepts::Either).await;
    let FetchOutcome::Found { json, final_uri } = h.fetch("/redirect").await else {
        panic!("not found");
    };
    assert_eq!(final_uri, h.url("/obj/target"));
    // The id is that of the object's own location, on the same host.
    assert_eq!(json["id"], h.url("/obj/target"));
    assert_eq!(h.fake.hits("/redirect"), 1);
    assert_eq!(h.fake.hits("/obj/target"), 1);
}

#[tokio::test]
async fn cross_host_redirect_is_refused() {
    let h = harness(Accepts::Either).await;
    let outcome = h.fetch("/redirect-away").await;
    assert!(
        matches!(
            outcome,
            FetchOutcome::Error(FetchError::CrossHostRedirect { .. })
        ),
        "{outcome:?}"
    );
    assert_eq!(
        h.fake.hits("/obj/target"),
        0,
        "never requested the other host"
    );
}

#[tokio::test]
async fn too_many_redirects() {
    let h = harness(Accepts::Either).await;
    assert!(matches!(
        h.fetch("/redirect-loop").await,
        FetchOutcome::Error(FetchError::TooManyRedirects { .. })
    ));
    // 4 requests: the original and three redirects.
    assert_eq!(h.fake.hits("/redirect-loop"), 4);
    // Two hops is fine.
    assert!(matches!(
        h.fetch("/redirect-chain").await,
        FetchOutcome::Found { .. }
    ));
}

#[tokio::test]
async fn origin_mismatch_is_an_error() {
    let h = harness(Accepts::Either).await;
    match h.fetch("/spoof").await {
        FetchOutcome::Error(FetchError::OriginMismatch { uri, id }) => {
            assert_eq!(uri, h.url("/spoof"));
            assert_eq!(id, "http://evil.example/obj/1");
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        h.fetch("/noid").await,
        FetchOutcome::Error(FetchError::MissingId { .. })
    ));
}

#[tokio::test]
async fn too_large_body() {
    let h = harness_with(Accepts::Either, |c| c.max_body_bytes = 1024).await;
    assert!(matches!(
        h.fetch("/big").await,
        FetchOutcome::Error(FetchError::TooLarge { limit: 1024, .. })
    ));
    // Under the limit is fine.
    assert!(matches!(
        h.fetch("/obj/1").await,
        FetchOutcome::Found { .. }
    ));
}

#[tokio::test]
async fn not_json() {
    let h = harness(Accepts::Either).await;
    assert!(matches!(
        h.fetch("/text").await,
        FetchOutcome::Error(FetchError::NotJson { .. })
    ));
}

#[tokio::test]
async fn bad_schemes_and_uris() {
    let strict = Fetcher::new(FetcherConfig::new(
        KEY_ID.to_owned(),
        our_key(),
        "t".to_owned(),
    ))
    .unwrap();
    for uri in [
        "http://a.example/x",
        "ftp://a.example/x",
        "file:///etc/passwd",
    ] {
        assert!(
            matches!(
                strict.fetch(uri).await,
                FetchOutcome::Error(FetchError::BadScheme { .. })
            ),
            "{uri}"
        );
    }
    assert!(matches!(
        strict.fetch("not a uri").await,
        FetchOutcome::Error(FetchError::BadUri { .. })
    ));
}

#[tokio::test]
async fn network_error_is_reported_and_backs_off() {
    // Nothing listens on this port any more.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let fetcher = Fetcher::new(config()).unwrap();
    let uri = format!("http://{addr}/obj/1");
    assert!(matches!(
        fetcher.fetch(&uri).await,
        FetchOutcome::Error(FetchError::Network { .. })
    ));
    assert!(matches!(
        fetcher.fetch(&uri).await,
        FetchOutcome::Backoff { .. }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_host_concurrency_is_limited() {
    let h = harness_with(Accepts::Either, |c| c.per_host_concurrency = 2).await;
    let h = Arc::new(h);
    let started = Instant::now();
    let tasks: Vec<_> = (0..6)
        .map(|_| {
            let h = Arc::clone(&h);
            tokio::spawn(async move { h.fetch("/slow").await })
        })
        .collect();
    for task in tasks {
        assert!(matches!(task.await.unwrap(), FetchOutcome::Found { .. }));
    }
    assert_eq!(h.fake.max_in_flight.load(Ordering::SeqCst), 2);
    // Six 100 ms requests, two at a time.
    assert!(started.elapsed() >= Duration::from_millis(290));
}

#[tokio::test]
async fn minimum_interval_between_requests() {
    let h = harness_with(Accepts::Either, |c| {
        c.min_interval_per_host = Duration::from_millis(150);
    })
    .await;
    let started = Instant::now();
    let (a, b, c) = tokio::join!(h.fetch("/obj/1"), h.fetch("/obj/2"), h.fetch("/obj/3"));
    for outcome in [a, b, c] {
        assert!(matches!(outcome, FetchOutcome::Found { .. }));
    }
    assert!(
        started.elapsed() >= Duration::from_millis(290),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn seed_host_round_trips_through_host_states() {
    let a = harness(Accepts::CavageOnly).await;
    assert!(matches!(
        a.fetch("/obj/1").await,
        FetchOutcome::Found { .. }
    ));
    let states = a.fetcher.host_states();

    // A new fetcher (as after a restart) seeded from the snapshot sends one request, not two.
    let b = Fetcher::new(config()).unwrap();
    for (host, snapshot) in &states {
        b.seed_host(host, *snapshot);
    }
    assert!(matches!(
        b.fetch(&a.url("/obj/2")).await,
        FetchOutcome::Found { .. }
    ));
    assert_eq!(a.fake.requests(), 3);
}

#[tokio::test]
async fn documents_skip_the_object_checks_only() {
    let h = harness(Accepts::Either).await;
    // A document has no `id`, which `fetch` refuses and `fetch_document` accepts.
    assert!(matches!(
        h.fetch("/doc").await,
        FetchOutcome::Error(FetchError::MissingId { .. })
    ));
    let FetchOutcome::Found { json, final_uri } = h.fetcher.fetch_document(&h.url("/doc")).await
    else {
        panic!("not found");
    };
    assert_eq!(json["software"]["name"], "lemmy");
    assert_eq!(final_uri, h.url("/doc"));
    let seen = h.fake.headers_seen.lock().unwrap().clone();
    assert!(
        seen.iter()
            .any(|(k, v)| k == "accept" && v == "application/json"),
        "{seen:?}"
    );
    // Nor is a `Tombstone` or a foreign id anything special in a document.
    assert!(matches!(
        h.fetcher.fetch_document(&h.url("/tombstone")).await,
        FetchOutcome::Found { .. }
    ));
    assert!(matches!(
        h.fetcher.fetch_document(&h.url("/spoof")).await,
        FetchOutcome::Found { .. }
    ));
    // Same-host redirects are followed, others are not.
    assert!(matches!(
        h.fetcher.fetch_document(&h.url("/doc-redirect")).await,
        FetchOutcome::Found { .. }
    ));
    assert!(matches!(
        h.fetcher.fetch_document(&h.url("/redirect-away")).await,
        FetchOutcome::Error(FetchError::CrossHostRedirect { .. })
    ));
    assert!(matches!(
        h.fetcher.fetch_document(&h.url("/gone404")).await,
        FetchOutcome::Gone
    ));
}

#[tokio::test]
async fn documents_keep_the_size_cap_and_backoff() {
    let h = harness_with(Accepts::Either, |c| c.max_body_bytes = 1024).await;
    assert!(matches!(
        h.fetcher.fetch_document(&h.url("/big")).await,
        FetchOutcome::Error(FetchError::TooLarge { limit: 1024, .. })
    ));
    assert!(matches!(
        h.fetcher.fetch_document(&h.url("/limited")).await,
        FetchOutcome::RateLimited { .. }
    ));
    let before = h.fake.requests();
    assert!(matches!(
        h.fetcher.fetch_document(&h.url("/doc")).await,
        FetchOutcome::Backoff { .. }
    ));
    assert_eq!(h.fake.requests(), before, "nothing sent while backing off");
}

#[tokio::test]
async fn text_is_read_raw_and_cut_at_the_limit() {
    let h = harness(Accepts::Either).await;
    assert_eq!(
        h.fetcher.fetch_text(&h.url("/robots.txt"), 1024).await,
        TextOutcome::Found(b"User-agent: *\nDisallow: /private\n".to_vec())
    );
    // Same-host redirects are followed; the body is cut, not refused.
    assert_eq!(
        h.fetcher.fetch_text(&h.url("/robots-redirect"), 10).await,
        TextOutcome::Found(b"User-agent".to_vec())
    );
    assert_eq!(
        h.fetcher.user_agent(),
        user_agent("https://social.example/")
    );
}

#[tokio::test]
async fn text_status_answers() {
    let h = harness(Accepts::Either).await;
    for (path, status) in [("/gone404", 404), ("/refuse", 403), ("/teapot", 418)] {
        assert_eq!(
            h.fetcher.fetch_text(&h.url(path), 1024).await,
            TextOutcome::ClientError { status },
            "{path}"
        );
    }
    assert!(matches!(
        h.fetcher.fetch_text(&h.url("/redirect-away"), 1024).await,
        TextOutcome::Failed(FetchOutcome::Error(FetchError::CrossHostRedirect { .. }))
    ));
    assert!(matches!(
        h.fetcher.fetch_text(&h.url("/err500"), 1024).await,
        TextOutcome::Failed(FetchOutcome::Error(FetchError::Status { status: 500, .. }))
    ));
    // The 500 started a backoff, which text fetches honour too.
    assert!(matches!(
        h.fetcher.fetch_text(&h.url("/robots.txt"), 1024).await,
        TextOutcome::Failed(FetchOutcome::Backoff { .. })
    ));
}

#[tokio::test]
async fn a_guard_sees_each_redirect_target_before_it_is_requested() {
    let h = harness(Accepts::Either).await;
    let seen = Mutex::new(Vec::new());
    let allow_all = |url: &url::Url| {
        seen.lock().unwrap().push(url.path().to_owned());
        true
    };
    assert!(matches!(
        h.fetcher
            .fetch_guarded(&h.url("/redirect-chain"), &allow_all)
            .await,
        FetchOutcome::Found { .. }
    ));
    assert_eq!(*seen.lock().unwrap(), ["/redirect", "/obj/target"]);

    let refuse_target = |url: &url::Url| url.path() != "/obj/target";
    let outcome = h
        .fetcher
        .fetch_guarded(&h.url("/redirect"), &refuse_target)
        .await;
    assert!(
        matches!(
            &outcome,
            FetchOutcome::Error(FetchError::RedirectDisallowed { location, .. })
                if location.ends_with("/obj/target")
        ),
        "{outcome:?}"
    );
    assert_eq!(
        h.fake.hits("/obj/target"),
        1,
        "only the first, allowed fetch"
    );
    // A cross-host target is refused before the guard is asked.
    let never = |_: &url::Url| -> bool { panic!("asked about another host") };
    assert!(matches!(
        h.fetcher
            .fetch_guarded(&h.url("/redirect-away"), &never)
            .await,
        FetchOutcome::Error(FetchError::CrossHostRedirect { .. })
    ));
}

#[tokio::test]
async fn private_addresses_are_refused_unless_allowed() {
    let h = harness(Accepts::Either).await;
    let port = h.host().rsplit(':').next().unwrap().to_owned();
    let mut strict = config();
    strict.allow_private_addresses = false;
    let strict = Fetcher::new(strict).unwrap();

    // A name that resolves to loopback is refused at connect time, and the error says which
    // host. Nothing reaches the fake.
    let by_name = format!("http://localhost:{port}/obj/1");
    match strict.fetch(&by_name).await {
        FetchOutcome::Error(e @ FetchError::NotPublic { .. }) => {
            let message = e.to_string();
            assert!(message.contains("`localhost`"), "{message}");
            assert!(message.contains("non-public"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    // Not the host's fault: no backoff.
    assert!(matches!(
        strict.fetch(&by_name).await,
        FetchOutcome::Error(FetchError::NotPublic { .. })
    ));
    // An IP literal is refused before anything is sent, and so is a delivery.
    let literal = h.url("/obj/1");
    match strict.fetch(&literal).await {
        FetchOutcome::Error(e @ FetchError::NotPublic { .. }) => {
            assert!(e.to_string().contains("`127.0.0.1`"), "{e}");
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        strict.deliver(&literal, b"{}").await,
        apub::fetch::DeliveryOutcome::Error(FetchError::NotPublic { .. })
    ));
    assert!(matches!(
        strict
            .fetch("https://[::ffff:169.254.169.254]/latest")
            .await,
        FetchOutcome::Error(FetchError::NotPublic { .. })
    ));
    assert_eq!(h.fake.total.load(Ordering::SeqCst), 0);

    // With the option set (as in every other test here), the same name is fetched.
    let allowed = Fetcher::new(config()).unwrap();
    assert!(matches!(
        allowed.fetch(&by_name).await,
        FetchOutcome::Found { .. }
    ));
}
