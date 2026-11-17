//! The admin API (PLAN.md, Deployment and adoption), on our full server (`serve_on`) against a
//! fake Mastodon. Nothing leaves 127.0.0.1.

mod common;

use std::time::Duration;

use apub::signatures::keys::Ed25519VerifyingKey;
use common::mastodon::FakeMastodon;
use mwmbl_social::cli::serve_on;
use mwmbl_social::config::{Config, Tokens};
use serde_json::{Value, json};
use tokio::sync::watch;

const TOKEN: &str = "admin-token-0123456789";

struct Server {
    base: String,
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
    _dir: tempfile::TempDir,
}

impl Server {
    async fn start(admin_tokens: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = Config {
            data_dir: dir.path().to_path_buf(),
            listen: addr.to_string(),
            base_url: Some(format!("http://{addr}")),
            posts_pages: 16,
            accounts_pages: 16,
            allow_http: true,
            admin_tokens: Tokens(admin_tokens.iter().map(|t| (*t).to_owned()).collect()),
            ..Config::default()
        };
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(serve_on(config, listener, shutdown_rx));
        let server = Self {
            base: format!("http://{addr}"),
            shutdown,
            task,
            _dir: dir,
        };
        for _ in 0..200 {
            if let Ok(r) = reqwest::get(format!("{}/actor", server.base)).await
                && r.status() == 200
            {
                return server;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the server did not start");
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> (u16, Value) {
        let mut request =
            reqwest::Client::new().request(method.parse().unwrap(), format!("{}{path}", self.base));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .body(body.to_string());
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn stop(self) {
        self.shutdown.send(true).unwrap();
        self.task.await.unwrap().unwrap();
    }
}

const REGISTRATIONS: &str = "/api/v1/admin/registrations";

#[tokio::test]
async fn the_admin_api_is_off_without_tokens() {
    let server = Server::start(&[]).await;
    let body = json!({"server_url": "https://m.example"});
    for (method, path, body) in [
        ("POST", REGISTRATIONS.to_owned(), Some(&body)),
        ("GET", REGISTRATIONS.to_owned(), None),
        ("GET", format!("{REGISTRATIONS}/1"), None),
        ("DELETE", format!("{REGISTRATIONS}/1"), None),
    ] {
        let (status, _) = server.call(method, &path, Some(TOKEN), body).await;
        assert_eq!(status, 404, "{method} {path}");
    }
    server.stop().await;
}

#[tokio::test]
async fn registers_lists_gets_and_revokes() {
    let fake = FakeMastodon::start().await;
    let server = Server::start(&[TOKEN]).await;
    let register = json!({"server_url": fake.origin});

    // Without a valid token, nothing happens.
    for token in [None, Some("wrong-token-0123456789")] {
        let (status, body) = server
            .call("POST", REGISTRATIONS, token, Some(&register))
            .await;
        assert_eq!(status, 401, "{body}");
        assert_eq!(server.call("GET", REGISTRATIONS, token, None).await.0, 401);
        let item = format!("{REGISTRATIONS}/1");
        assert_eq!(server.call("DELETE", &item, token, None).await.0, 401);
    }
    assert_eq!(fake.fasp_call_count(), 0);

    // Unusable URLs are refused before anything is sent.
    for bad in ["not a url", "ftp://m.example", "https://user:pw@m.example"] {
        let (status, body) = server
            .call(
                "POST",
                REGISTRATIONS,
                Some(TOKEN),
                Some(&json!({"server_url": bad})),
            )
            .await;
        assert_eq!(status, 400, "{bad}: {body}");
        assert!(body["error"].as_str().unwrap().contains(bad), "{body}");
    }
    let (status, _) = server
        .call(
            "POST",
            REGISTRATIONS,
            Some(TOKEN),
            Some(&json!({"url": "https://m.example"})),
        )
        .await;
    assert_eq!(status, 422);

    // A server that isn't there: 502, naming it.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let (status, body) = server
        .call(
            "POST",
            REGISTRATIONS,
            Some(TOKEN),
            Some(&json!({"server_url": closed})),
        )
        .await;
    assert_eq!(status, 502, "{body}");
    assert!(body["error"].as_str().unwrap().contains(&closed), "{body}");

    // Register: the fingerprint of the key we sent, and the server's approval link.
    let (status, created) = server
        .call("POST", REGISTRATIONS, Some(TOKEN), Some(&register))
        .await;
    assert_eq!(status, 201, "{created}");
    let our_key =
        Ed25519VerifyingKey::from_base64(fake.registered()["publicKey"].as_str().unwrap()).unwrap();
    let id = created["id"].as_i64().unwrap();
    assert_eq!(
        created,
        json!({
            "id": id,
            "base_url": fake.url("/api/fasp"),
            "status": "pending",
            "capabilities": [],
            "fingerprint": our_key.fingerprint(),
            "created_at": created["created_at"],
            "approval_url": fake.url("/admin/fasps"),
        })
    );
    assert!(created["created_at"].as_i64().unwrap() > 0);

    // Again: 409, with the existing registration's id.
    let (status, body) = server
        .call("POST", REGISTRATIONS, Some(TOKEN), Some(&register))
        .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["id"], id);

    // List and get show it without the approval link (the server gives that only once).
    let mut stored = created.clone();
    stored.as_object_mut().unwrap().remove("approval_url");
    let (status, list) = server.call("GET", REGISTRATIONS, Some(TOKEN), None).await;
    assert_eq!(status, 200);
    assert_eq!(list, json!([stored]));
    let item = format!("{REGISTRATIONS}/{id}");
    assert_eq!(
        server.call("GET", &item, Some(TOKEN), None).await,
        (200, stored)
    );
    for missing in [
        format!("{REGISTRATIONS}/{}", id + 1),
        format!("{REGISTRATIONS}/x"),
    ] {
        assert_eq!(server.call("GET", &missing, Some(TOKEN), None).await.0, 404);
        assert_eq!(
            server.call("DELETE", &missing, Some(TOKEN), None).await.0,
            404
        );
    }

    // Delete revokes it, as `unregister` does.
    let (status, revoked) = server.call("DELETE", &item, Some(TOKEN), None).await;
    assert_eq!(status, 200, "{revoked}");
    assert_eq!(revoked["id"], id);
    assert_eq!(revoked["status"], "revoked");
    let (_, after) = server.call("GET", &item, Some(TOKEN), None).await;
    assert_eq!(after["status"], "revoked");
    assert!(!list.to_string().contains("private"));

    server.stop().await;
}
