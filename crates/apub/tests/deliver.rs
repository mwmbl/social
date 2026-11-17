//! `Fetcher::deliver` against an in-process fake inbox that checks every delivery with our own
//! inbox verifier (`apub::inbox`). No network beyond 127.0.0.1.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use apub::fetch::{DeliveryOutcome, FetchOutcome, Fetcher, FetcherConfig, SignatureScheme};
use apub::inbox::{signer, verify_post};
use apub::signatures::keys::{RsaSigningKey, RsaVerifyingKey};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::Response;

const KEY_ID: &str = "https://social.example/actor#main-key";

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).unwrap()
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Accepts {
    Either,
    CavageOnly,
    /// As Lemmy 0.19: an RFC 9421 signature can't be parsed and gets 400, not 401.
    CavageOnlyBadRequest,
}

struct Inbox {
    accepts: Accepts,
    public: RsaVerifyingKey,
    /// Bodies of verified deliveries, with the scheme that verified.
    received: Mutex<Vec<(SignatureScheme, Vec<u8>, String)>>,
    requests: AtomicUsize,
}

fn status(code: u16) -> Response<Body> {
    Response::builder()
        .status(code)
        .body(Body::empty())
        .unwrap()
}

async fn handle(State(inbox): State<Arc<Inbox>>, request: Request) -> Response<Body> {
    inbox.requests.fetch_add(1, Ordering::SeqCst);
    let (parts, body) = request.into_parts();
    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
        .collect();
    let host = parts.headers["host"].to_str().unwrap().to_owned();
    let target = format!("http://{host}{}", parts.uri);
    let body = to_bytes(body, 1 << 20).await.unwrap();
    match parts.uri.path() {
        "/busy" => return status(503),
        "/limited" => {
            return Response::builder()
                .status(429)
                .header("retry-after", "30")
                .body(Body::empty())
                .unwrap();
        }
        "/bad-request" => return status(400),
        _ => {}
    }
    if inbox.accepts == Accepts::CavageOnlyBadRequest
        && headers.iter().any(|(n, _)| n == "signature-input")
    {
        return status(400);
    }
    let Ok(signed_by) = signer(&headers) else {
        return status(401);
    };
    if signed_by.key_id != KEY_ID
        || verify_post(&signed_by, &target, &headers, &body, &inbox.public, now()).is_err()
    {
        return status(401);
    }
    if inbox.accepts != Accepts::Either && signed_by.scheme != SignatureScheme::Cavage {
        return status(401);
    }
    let content_type = headers
        .iter()
        .find(|(n, _)| n == "content-type")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    inbox
        .received
        .lock()
        .unwrap()
        .push((signed_by.scheme, body.to_vec(), content_type));
    status(202)
}

async fn serve(accepts: Accepts) -> (Arc<Inbox>, String) {
    let inbox = Arc::new(Inbox {
        accepts,
        public: RsaVerifyingKey::from_pem(&fixture("cavage12_public.pem")).unwrap(),
        received: Mutex::new(Vec::new()),
        requests: AtomicUsize::new(0),
    });
    let app = Router::new()
        .fallback(handle)
        .with_state(Arc::clone(&inbox));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (inbox, base)
}

fn fetcher() -> Fetcher {
    let mut config = FetcherConfig::new(
        KEY_ID.to_owned(),
        RsaSigningKey::from_pem(&fixture("cavage12_private.pem")).unwrap(),
        "mwmbl-social-test".to_owned(),
    );
    config.allow_http = true;
    config.allow_private_addresses = true;
    config.min_interval_per_host = Duration::ZERO;
    Fetcher::new(config).unwrap()
}

const BODY: &[u8] = br#"{"type":"Accept","actor":"https://social.example/actor"}"#;

#[tokio::test]
async fn delivers_with_rfc9421_and_content_digest() {
    let (inbox, base) = serve(Accepts::Either).await;
    let fetcher = fetcher();
    let outcome = fetcher.deliver(&format!("{base}/inbox"), BODY).await;
    assert_eq!(outcome, DeliveryOutcome::Delivered { status: 202 });
    let received = inbox.received.lock().unwrap().clone();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].0, SignatureScheme::Rfc9421);
    assert_eq!(received[0].1, BODY);
    assert_eq!(received[0].2, "application/activity+json");
}

#[tokio::test]
async fn falls_back_to_cavage_with_digest_and_remembers() {
    let (inbox, base) = serve(Accepts::CavageOnly).await;
    let fetcher = fetcher();
    let inbox_uri = format!("{base}/inbox");
    assert_eq!(
        fetcher.deliver(&inbox_uri, BODY).await,
        DeliveryOutcome::Delivered { status: 202 }
    );
    assert_eq!(inbox.requests.load(Ordering::SeqCst), 2);
    // The host is now known as cavage-only: one request per delivery.
    assert_eq!(
        fetcher.deliver(&inbox_uri, BODY).await,
        DeliveryOutcome::Delivered { status: 202 }
    );
    assert_eq!(inbox.requests.load(Ordering::SeqCst), 3);
    let received = inbox.received.lock().unwrap().clone();
    assert!(received.iter().all(|r| r.0 == SignatureScheme::Cavage));
    let host = base.trim_start_matches("http://");
    assert_eq!(
        fetcher.host_states()[0],
        (
            host.to_owned(),
            apub::fetch::HostSnapshot {
                scheme: Some(SignatureScheme::Cavage),
                backoff_until: None
            }
        )
    );
}

#[tokio::test]
async fn falls_back_to_cavage_on_400_as_lemmy_answers() {
    let (inbox, base) = serve(Accepts::CavageOnlyBadRequest).await;
    let fetcher = fetcher();
    let inbox_uri = format!("{base}/inbox");
    assert_eq!(
        fetcher.deliver(&inbox_uri, BODY).await,
        DeliveryOutcome::Delivered { status: 202 }
    );
    assert_eq!(inbox.requests.load(Ordering::SeqCst), 2);
    assert_eq!(
        fetcher.deliver(&inbox_uri, BODY).await,
        DeliveryOutcome::Delivered { status: 202 }
    );
    assert_eq!(inbox.requests.load(Ordering::SeqCst), 3);
    assert_eq!(
        fetcher.host_states()[0].1.scheme,
        Some(SignatureScheme::Cavage)
    );
}

#[tokio::test]
async fn a_400_under_both_schemes_is_a_rejection_and_caches_no_scheme() {
    let (inbox, base) = serve(Accepts::Either).await;
    let fetcher = fetcher();
    let outcome = fetcher.deliver(&format!("{base}/bad-request"), BODY).await;
    assert_eq!(outcome, DeliveryOutcome::Rejected { status: 400 });
    assert_eq!(inbox.requests.load(Ordering::SeqCst), 2);
    assert!(
        fetcher
            .host_states()
            .iter()
            .all(|(_, s)| s.scheme.is_none())
    );
}

#[tokio::test]
async fn failures_back_off_and_rejections_do_not() {
    let (_inbox, base) = serve(Accepts::Either).await;
    let fetcher = fetcher();
    let outcome = fetcher.deliver(&format!("{base}/bad-request"), BODY).await;
    assert_eq!(outcome, DeliveryOutcome::Rejected { status: 400 });
    assert!(!outcome.is_transient());

    let outcome = fetcher.deliver(&format!("{base}/busy"), BODY).await;
    assert!(matches!(outcome, DeliveryOutcome::Error(_)), "{outcome:?}");
    assert!(outcome.is_transient());
    // The host is backing off: nothing is sent, for deliveries and fetches alike.
    let outcome = fetcher.deliver(&format!("{base}/inbox"), BODY).await;
    assert!(
        matches!(outcome, DeliveryOutcome::Backoff { .. }),
        "{outcome:?}"
    );
    assert!(matches!(
        fetcher.fetch(&format!("{base}/obj")).await,
        FetchOutcome::Backoff { .. }
    ));
}

#[tokio::test]
async fn rate_limit_is_reported() {
    let (_inbox, base) = serve(Accepts::Either).await;
    let outcome = fetcher().deliver(&format!("{base}/limited"), BODY).await;
    assert_eq!(
        outcome,
        DeliveryOutcome::RateLimited {
            retry_after_secs: 30
        }
    );
}

/// The headers and target URI of a request.
type Captured = (Vec<(String, String)>, String);

#[tokio::test]
async fn tampered_body_fails_our_verifier() {
    // Deliver to a fake that only records the headers, then check them against other inputs.
    let captured: Arc<Mutex<Option<Captured>>> = Arc::default();
    let sink = Arc::clone(&captured);
    let app = Router::new().fallback(move |request: Request| {
        let sink = Arc::clone(&sink);
        async move {
            let headers: Vec<(String, String)> = request
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap().to_owned()))
                .collect();
            let host = request.headers()["host"].to_str().unwrap().to_owned();
            *sink.lock().unwrap() = Some((headers, format!("http://{host}{}", request.uri())));
            status(202)
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    fetcher().deliver(&format!("{base}/inbox"), BODY).await;
    let (headers, target) = captured.lock().unwrap().clone().unwrap();
    let public = RsaVerifyingKey::from_pem(&fixture("cavage12_public.pem")).unwrap();
    let by = signer(&headers).unwrap();
    assert!(verify_post(&by, &target, &headers, BODY, &public, now()).is_ok());
    assert!(verify_post(&by, &target, &headers, b"{}", &public, now()).is_err());
    // Another target, or a time outside the skew window, fails too.
    assert!(
        verify_post(
            &by,
            &format!("{base}/other"),
            &headers,
            BODY,
            &public,
            now()
        )
        .is_err()
    );
    assert!(verify_post(&by, &target, &headers, BODY, &public, now() + 7200).is_err());
    // Another key fails.
    let other = RsaVerifyingKey::from_pem(&fixture("rfc9421_rsa_public.pem")).unwrap();
    assert!(verify_post(&by, &target, &headers, BODY, &other, now()).is_err());
}
