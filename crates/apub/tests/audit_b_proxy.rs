//! AUDIT B1: a proxy taken from the environment bypasses the SSRF filter.
//!
//! `apub::net::client_builder` never calls `.no_proxy()`, so reqwest reads `HTTP_PROXY`,
//! `HTTPS_PROXY` and `ALL_PROXY`. With one set, the target name is resolved by the proxy, not by
//! `PublicResolver`, and a name that points at a metadata service or a private network is
//! fetched. This test is its own binary because it sets process environment variables.
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
#[ignore = "AUDIT: B1 - fails until fixed"]
async fn an_environment_proxy_does_not_bypass_the_address_filter() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
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
    let mut config = FetcherConfig::new(
        "https://social.example/actor#main-key".to_owned(),
        our_key(),
        user_agent("https://social.example/"),
    );
    config.allow_http = true;
    config.allow_private_addresses = false;
    config.timeout = Duration::from_secs(5);
    let fetcher = Fetcher::new(config).unwrap();

    // A name for the cloud metadata service. Without a proxy PublicResolver refuses it (or it
    // does not resolve); with one, the proxy resolves and fetches it.
    let _ = fetcher
        .fetch("http://metadata.google.internal/computeMetadata/v1/instance/")
        .await;

    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.is_empty(),
        "the request went out through the environment proxy, bypassing PublicResolver: {seen:?}"
    );
}
