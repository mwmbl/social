//! AUDIT B1: a proxy taken from the environment bypasses the SSRF filter.
//!
//! Before the fix `apub::net::client_builder` didn't call `.no_proxy()`, so reqwest read
//! `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY`. With one set, the target name was resolved by the
//! proxy, not by `PublicResolver`, and a name that points at a metadata service or a private
//! network was fetched. This test is its own binary because it sets process environment variables.
//! Offline: the "proxy" is a listener on 127.0.0.1 that never forwards anything.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use apub::fetch::{Fetcher, FetcherConfig, user_agent};
use apub::signatures::keys::RsaSigningKey;
use std::io::{Read, Write};
use std::net::TcpListener;

fn our_key() -> RsaSigningKey {
    let path = format!(
        "{}/tests/fixtures/cavage12_private.pem",
        env!("CARGO_MANIFEST_DIR")
    );
    RsaSigningKey::from_pem(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[tokio::test]
async fn an_environment_proxy_does_not_bypass_the_address_filter() {
    let (proxy, seen) = recording_listener();
    let proxy = format!("http://{proxy}");

    // SAFETY: this binary has one test and the runtime is single-threaded, so nothing else
    // reads the environment concurrently.
    unsafe {
        std::env::remove_var("NO_PROXY");
        std::env::remove_var("no_proxy");
        std::env::set_var("HTTP_PROXY", &proxy);
        std::env::set_var("http_proxy", &proxy);
    }

    // The production setting: non-public addresses refused. (`allow_http` only so the fake
    // proxy needs no TLS; `HTTPS_PROXY` with CONNECT is the same bypass.)
    let fetcher = Fetcher::new(fetcher_config(false)).unwrap();

    // A name for the cloud metadata service. Without a proxy PublicResolver refuses it (or it
    // does not resolve); with one, the proxy resolves and fetches it.
    let _ = fetcher
        .fetch("http://metadata.google.internal/computeMetadata/v1/instance/")
        .await;

    let through_proxy = seen.lock().unwrap().clone();
    assert!(
        through_proxy.is_empty(),
        "the request went out through the environment proxy, bypassing PublicResolver: \
         {through_proxy:?}"
    );

    // With `allow_private_addresses` (dev setups) the environment proxy is ignored too: the
    // request goes straight to the server, not to the proxy.
    let (direct, direct_seen) = recording_listener();
    let fetcher = Fetcher::new(fetcher_config(true)).unwrap();
    let _ = fetcher.fetch(&format!("http://{direct}/users/a")).await;
    assert_eq!(
        direct_seen.lock().unwrap().clone(),
        ["GET /users/a HTTP/1.1"],
        "the request did not reach the server directly"
    );
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.is_empty(),
        "the request went through the proxy: {seen:?}"
    );
}

/// A fetcher config with `allow_http` (so the fakes need no TLS) and the given
/// `allow_private_addresses`.
fn fetcher_config(allow_private_addresses: bool) -> FetcherConfig {
    let mut config = FetcherConfig::new(
        "https://social.example/actor#main-key".to_owned(),
        our_key(),
        user_agent("https://social.example/"),
    );
    config.allow_http = true;
    config.allow_private_addresses = allow_private_addresses;
    config.timeout = Duration::from_secs(5);
    config
}

/// A listener on 127.0.0.1 that answers every request with 404 and records each request line.
fn recording_listener() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let record = Arc::clone(&seen);
    std::thread::spawn(move || {
        for socket in listener.incoming() {
            let Ok(mut socket) = socket else { break };
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(buf.get(..n).unwrap_or_default()).into_owned();
            record
                .lock()
                .unwrap()
                .push(head.lines().next().unwrap_or_default().to_owned());
            let _ = socket.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n");
        }
    });
    (addr, seen)
}
