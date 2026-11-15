//! The HTTP routes, against a real listener on 127.0.0.1.

mod common;

use common::{TestEnv, test_key};
use mwmbl_social::decide::Action;
use mwmbl_social::http::{AppState, router};
use serde_json::Value;
use social_index::{SocialAccount, SocialPost};

async fn start(env: &TestEnv) -> String {
    let (posts, accounts) = env.handle.readers();
    let app = router(AppState {
        base_url: "https://social.example".into(),
        actor_username: "mwmbl-social".into(),
        name: "Test Search".into(),
        public_key_pem: test_key().verifying_key().to_spki_pem().unwrap(),
        posts,
        accounts,
        tombstones: env.handle.tombstones(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    base
}

async fn get(url: &str) -> (u16, String, Value) {
    let response = reqwest::get(url).await.unwrap();
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let body = response.text().await.unwrap();
    (status, content_type, serde_json::from_str(&body).unwrap())
}

fn post(n: usize) -> SocialPost {
    SocialPost {
        uri: format!("https://h.example/s/{n}"),
        author_uri: "https://h.example/users/a".into(),
        extract: format!("Planting garlic number {n}"),
        score: 1.0,
        term: String::new(),
        published: 1_700_000_000 + i64::try_from(n).unwrap(),
        checked: 1_700_000_100,
        title: None,
        language: Some("en".into()),
    }
}

#[tokio::test]
async fn actor_webfinger_and_nodeinfo() {
    let env = TestEnv::new().await;
    let base = start(&env).await;

    let (status, content_type, actor) = get(&format!("{base}/actor")).await;
    assert_eq!(status, 200);
    assert_eq!(content_type, "application/activity+json");
    assert_eq!(actor["id"], "https://social.example/actor");
    assert_eq!(actor["name"], "Test Search");
    assert!(
        actor["publicKey"]["publicKeyPem"]
            .as_str()
            .unwrap()
            .contains("BEGIN PUBLIC KEY")
    );

    let q = "resource=acct%3Amwmbl-social%40social.example";
    let (status, content_type, jrd) = get(&format!("{base}/.well-known/webfinger?{q}")).await;
    assert_eq!(status, 200);
    assert_eq!(content_type, "application/jrd+json");
    assert_eq!(jrd["links"][0]["href"], "https://social.example/actor");
    let (status, _, body) = get(&format!(
        "{base}/.well-known/webfinger?resource=acct:other@social.example"
    ))
    .await;
    assert_eq!(status, 404);
    assert!(body["error"].is_string());
    assert_eq!(get(&format!("{base}/.well-known/webfinger")).await.0, 404);

    let (status, _, wk) = get(&format!("{base}/.well-known/nodeinfo")).await;
    assert_eq!(status, 200);
    assert_eq!(
        wk["links"][0]["href"],
        "https://social.example/nodeinfo/2.1"
    );
    let (status, content_type, info) = get(&format!("{base}/nodeinfo/2.1")).await;
    assert_eq!(status, 200);
    assert!(content_type.starts_with("application/json; profile="));
    assert_eq!(info["software"]["name"], "mwmbl-social");
}

#[tokio::test]
async fn search_posts_and_accounts() {
    let env = TestEnv::new().await;
    let mut actions: Vec<Action> = (0..30)
        .map(|n| Action::IndexPost {
            record: post(n),
            replaces: None,
        })
        .collect();
    actions.push(Action::IndexAccount {
        record: SocialAccount {
            uri: "https://h.example/users/a".into(),
            acct: "ada@h.example".into(),
            display_name: "Ada Lovelace".into(),
            summary: "Maths".into(),
            score: 1.0,
            term: String::new(),
            checked: 1_700_000_100,
            languages: Some(vec!["en-GB".into()]),
        },
        replaces: None,
    });
    env.handle.apply(actions).await.unwrap();
    let base = start(&env).await;

    let (status, content_type, body) = get(&format!("{base}/api/v1/search/posts?q=garlic")).await;
    assert_eq!(status, 200);
    assert_eq!(content_type, "application/json");
    let results = body.as_array().unwrap();
    assert_eq!(results.len(), 20, "default limit");
    let first = results[0].as_object().unwrap();
    // `uri` leads in the wire format (a parsed `Value` sorts its keys, so look at the text).
    let raw = reqwest::get(format!("{base}/api/v1/search/posts?q=garlic"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(raw.starts_with("[{\"uri\":\"https://h.example/s/"), "{raw}");
    assert!(first.contains_key("extract") && first.contains_key("author_uri"));
    assert!(!first.contains_key("term"));

    let (_, _, body) = get(&format!("{base}/api/v1/search/posts?q=garlic&limit=5")).await;
    assert_eq!(body.as_array().unwrap().len(), 5);
    let (_, _, body) = get(&format!("{base}/api/v1/search/posts?q=garlic&limit=1000")).await;
    assert_eq!(body.as_array().unwrap().len(), 30);
    let (_, _, body) = get(&format!("{base}/api/v1/search/posts?q=nonexistentword")).await;
    assert_eq!(body, Value::Array(vec![]));

    for bad in [
        "/api/v1/search/posts",
        "/api/v1/search/posts?q=",
        "/api/v1/search/posts?q=%20",
        "/api/v1/search/posts?q=a&limit=0",
        "/api/v1/search/posts?q=a&limit=x",
        "/api/v1/search/accounts",
    ] {
        let (status, content_type, body) = get(&format!("{base}{bad}")).await;
        assert_eq!(status, 422, "{bad}");
        assert_eq!(content_type, "application/json");
        assert!(body["error"].is_string(), "{bad}");
    }

    let (_, _, body) = get(&format!("{base}/api/v1/search/accounts?q=lovelace")).await;
    let accounts = body.as_array().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["uri"], "https://h.example/users/a");
    assert_eq!(accounts[0]["acct"], "ada@h.example");
    let (_, _, body) = get(&format!(
        "{base}/api/v1/search/accounts?q=lovelace&language=en"
    ))
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    let (_, _, body) = get(&format!(
        "{base}/api/v1/search/accounts?q=lovelace&language=de"
    ))
    .await;
    assert_eq!(body.as_array().unwrap().len(), 0);

    // A tombstone hides results at once.
    env.handle
        .forget(mwmbl_social::writer::ForgetTarget::Domain(
            "h.example".into(),
        ))
        .await
        .unwrap();
    let (_, _, body) = get(&format!("{base}/api/v1/search/posts?q=garlic")).await;
    assert_eq!(body, Value::Array(vec![]));
}
