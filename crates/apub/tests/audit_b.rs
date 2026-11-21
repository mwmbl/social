//! AUDIT B: per-host limits in `apub::fetch` (amplification and unbounded state).
//! Offline: only 127.0.0.1 and closed or held ports on it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use apub::fetch::{Fetcher, FetcherConfig, uri_host_key, user_agent};
use apub::signatures::keys::RsaSigningKey;
use tokio::net::TcpListener;

fn our_key() -> RsaSigningKey {
    let path = format!(
        "{}/tests/fixtures/cavage12_private.pem",
        env!("CARGO_MANIFEST_DIR")
    );
    RsaSigningKey::from_pem(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn config() -> FetcherConfig {
    let mut config = FetcherConfig::new(
        "https://social.example/actor#main-key".to_owned(),
        our_key(),
        user_agent("https://social.example/"),
    );
    config.allow_http = true;
    config.allow_private_addresses = true;
    config.min_interval_per_host = Duration::ZERO;
    config.timeout = Duration::from_secs(3);
    config
}

/// B2: `victim.example.` and `victim.example` are the same host (DNS root), but get separate
/// per-host concurrency, pacing and backoff, so a stranger can double the rate we send to a
/// host, and keep hitting one that is backing off.
#[test]
#[ignore = "AUDIT: B2 - fails until fixed"]
fn a_trailing_dot_does_not_make_a_new_host() {
    assert_eq!(
        uri_host_key("https://victim.example./users/a"),
        uri_host_key("https://victim.example/users/a"),
    );
}

/// B3: the fetcher keeps per-host state (and the binary persists it to the SQLite `hosts`
/// table) for every host it was ever asked about, with no bound. Each inbox POST from a
/// stranger can name a new `keyId` host (random subdomains of a wildcard domain), so memory and
/// the `hosts` table grow without limit. Here 300 closed ports stand in for 300 hosts.
#[tokio::test]
#[ignore = "AUDIT: B3 - fails until fixed"]
async fn per_host_state_is_bounded() {
    let fetcher = Fetcher::new(config()).unwrap();
    let mut ports = Vec::new();
    for _ in 0..300 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        ports.push(listener.local_addr().unwrap().port());
        // Dropped: the port is closed again, so the fetch fails fast.
    }
    for port in ports {
        let _ = fetcher.fetch(&format!("http://127.0.0.1:{port}/key")).await;
    }
    let kept = fetcher.host_states().len();
    assert!(
        kept <= 100,
        "{kept} hosts kept: per-host state grows with every host a stranger names"
    );
}

/// B4: there is no global limit on simultaneous outbound requests, only a per-host one. Inbox
/// key fetches run inline in the request handler, so N concurrent inbox POSTs naming N slow
/// hosts hold N outbound connections (each up to `timeout` per hop, and up to 8 hops with
/// redirects and the signature-scheme retry).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "AUDIT: B4 - fails until fixed"]
async fn outbound_requests_have_a_global_limit() {
    let open = Arc::new(AtomicUsize::new(0));
    let mut urls = Vec::new();
    for _ in 0..64 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        urls.push(format!("http://{}/key", listener.local_addr().unwrap()));
        let open = Arc::clone(&open);
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                open.fetch_add(1, Ordering::SeqCst);
                // Slow loris: accept and never answer.
                held.push(socket);
            }
        });
    }
    let fetcher = Arc::new(Fetcher::new(config()).unwrap());
    for url in urls {
        let fetcher = Arc::clone(&fetcher);
        tokio::spawn(async move { fetcher.fetch(&url).await });
    }
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let open = open.load(Ordering::SeqCst);
    assert!(
        open <= 32,
        "{open} outbound connections held open at once: no global cap"
    );
}
