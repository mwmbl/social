//! `apub::nodeinfo::lookup_software` against an in-process fake host. No network beyond
//! 127.0.0.1.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use apub::fetch::{Fetcher, FetcherConfig, user_agent};
use apub::nodeinfo::{NODEINFO_REL_PREFIX, NodeinfoError, lookup_software};
use apub::signatures::keys::RsaSigningKey;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, Response};
use serde_json::{Value, json};

fn our_key() -> RsaSigningKey {
    let path = format!(
        "{}/tests/fixtures/cavage12_private.pem",
        env!("CARGO_MANIFEST_DIR")
    );
    RsaSigningKey::from_pem(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// What the fake serves at `/.well-known/nodeinfo` and the paths it links to.
struct Fake {
    /// `(path, status, body)`.
    routes: Vec<(String, u16, Value)>,
    seen: Mutex<Vec<String>>,
}

async fn handle(State(fake): State<Arc<Fake>>, request: Request<Body>) -> Response<Body> {
    let path = request.uri().path().to_owned();
    fake.seen.lock().unwrap().push(path.clone());
    let (status, body) = fake
        .routes
        .iter()
        .find(|(p, _, _)| *p == path)
        .map_or((404, json!({})), |(_, status, body)| {
            (*status, body.clone())
        });
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Serves `routes` built from the fake's own base URL.
async fn serve(routes: impl FnOnce(&str) -> Vec<(String, u16, Value)>) -> (Arc<Fake>, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let fake = Arc::new(Fake {
        routes: routes(&base),
        seen: Mutex::new(Vec::new()),
    });
    let app = Router::new().fallback(handle).with_state(Arc::clone(&fake));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (fake, base)
}

fn fetcher() -> Fetcher {
    let mut config = FetcherConfig::new(
        "https://social.example/actor#main-key".to_owned(),
        our_key(),
        user_agent("https://social.example/"),
    );
    config.allow_http = true;
    config.allow_private_addresses = true;
    config.min_interval_per_host = Duration::ZERO;
    config.timeout = Duration::from_secs(10);
    Fetcher::new(config).unwrap()
}

fn link(version: &str, href: &str) -> Value {
    json!({"rel": format!("{NODEINFO_REL_PREFIX}{version}"), "href": href})
}

fn index(links: &[Value]) -> (String, u16, Value) {
    (
        "/.well-known/nodeinfo".to_owned(),
        200,
        json!({ "links": links }),
    )
}

fn software(path: &str, name: &str) -> (String, u16, Value) {
    (
        path.to_owned(),
        200,
        json!({"version": "2.1", "software": {"name": name, "version": "0.19.5"}}),
    )
}

#[tokio::test]
async fn finds_the_software_lowercased() {
    let (fake, base) = serve(|base| {
        vec![
            index(&[
                link("2.0", &format!("{base}/nodeinfo/2.0")),
                link("2.1", &format!("{base}/nodeinfo/2.1")),
            ]),
            software("/nodeinfo/2.0", "old"),
            software("/nodeinfo/2.1", "Lemmy"),
        ]
    })
    .await;
    let got = lookup_software(&fetcher(), &format!("{base}/u/carol")).await;
    assert_eq!(got, Ok("lemmy".to_owned()));
    assert_eq!(
        *fake.seen.lock().unwrap(),
        ["/.well-known/nodeinfo", "/nodeinfo/2.1"]
    );
}

#[tokio::test]
async fn relative_links_resolve_on_the_same_origin() {
    let (_, base) = serve(|_| {
        vec![
            index(&[link("2.0", "/nodeinfo/2.0")]),
            software("/nodeinfo/2.0", "gotosocial"),
        ]
    })
    .await;
    let got = lookup_software(&fetcher(), &base).await;
    assert_eq!(got, Ok("gotosocial".to_owned()));
}

#[tokio::test]
async fn a_link_to_another_host_is_not_followed() {
    let (fake, base) = serve(|base| {
        let other = base.replace("127.0.0.1", "localhost");
        vec![index(&[link("2.1", &format!("{other}/nodeinfo/2.1"))])]
    })
    .await;
    let got = lookup_software(&fetcher(), &base).await;
    assert!(
        matches!(got, Err(NodeinfoError::OffOrigin { .. })),
        "{got:?}"
    );
    assert_eq!(*fake.seen.lock().unwrap(), ["/.well-known/nodeinfo"]);
}

#[tokio::test]
async fn failures_are_errors() {
    // No nodeinfo at all.
    let (_, base) = serve(|_| Vec::new()).await;
    let got = lookup_software(&fetcher(), &base).await;
    assert!(
        matches!(got, Err(NodeinfoError::Unavailable { .. })),
        "{got:?}"
    );

    // Only a 1.x link.
    let (_, base) = serve(|_| vec![index(&[link("1.0", "/nodeinfo/1.0")])]).await;
    let got = lookup_software(&fetcher(), &base).await;
    assert!(matches!(got, Err(NodeinfoError::NoLink { .. })), "{got:?}");

    // A document without `software.name`.
    let (_, base) = serve(|_| {
        vec![
            index(&[link("2.0", "/nodeinfo/2.0")]),
            ("/nodeinfo/2.0".to_owned(), 200, json!({"version": "2.0"})),
        ]
    })
    .await;
    let got = lookup_software(&fetcher(), &base).await;
    assert!(
        matches!(got, Err(NodeinfoError::NoSoftware { .. })),
        "{got:?}"
    );

    // A server error backs the host off, and the next lookup sends nothing.
    let (fake, base) = serve(|_| vec![("/.well-known/nodeinfo".to_owned(), 503, json!({}))]).await;
    let f = fetcher();
    let got = lookup_software(&f, &base).await;
    assert!(matches!(got, Err(NodeinfoError::Fetch { .. })), "{got:?}");
    let got = lookup_software(&f, &base).await;
    assert!(
        matches!(got, Err(NodeinfoError::Unavailable { .. })),
        "{got:?}"
    );
    assert_eq!(fake.seen.lock().unwrap().len(), 1);

    // Not a URI with a host.
    let got = lookup_software(&f, "not a uri").await;
    assert!(matches!(got, Err(NodeinfoError::BadUri { .. })), "{got:?}");
}
