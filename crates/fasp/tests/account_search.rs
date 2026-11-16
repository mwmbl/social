//! `GET /account_search/v0/search`, exercised the way Mastodon calls it.

mod common;

use common::{Env, Opts, Reply};
use fasp::{ACCOUNT_SEARCH_ID, ACCOUNT_SEARCH_VERSION, Capability, StoreAccess};
use std::sync::atomic::Ordering;

async fn env() -> Env {
    let env = Env::with(
        common::default_info(),
        vec![Capability::new(ACCOUNT_SEARCH_ID, ACCOUNT_SEARCH_VERSION)],
    )
    .await;
    assert_eq!(env.activate("account_search", "0").await.status, 204);
    env
}

fn uris(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| format!("https://h.example/users/{i}"))
        .collect()
}

async fn search(env: &Env, query: &str) -> Reply {
    env.mastodon_request(
        "GET",
        &format!("/account_search/v0/search?{query}"),
        b"",
        &Opts::default(),
    )
    .await
}

fn calls(env: &Env) -> Vec<common::SearchCall> {
    env.ours.state.search_calls.lock().unwrap().clone()
}

#[tokio::test]
async fn missing_or_empty_term_is_422_and_signed() {
    let env = env().await;
    for query in ["", "limit=10", "term=", "term=%20%20", "term"] {
        let reply = search(&env, query).await;
        assert_eq!(reply.status, 422, "{query}");
        assert!(reply.json()["error"].is_string());
        env.assert_signed(&reply);
    }
    assert!(calls(&env).is_empty());
}

#[tokio::test]
async fn bad_limits_are_422() {
    let env = env().await;
    for limit in ["0", "-1", "abc", "", "1.5", "+3", "1e2"] {
        let reply = search(&env, &format!("term=a&limit={limit}")).await;
        assert_eq!(reply.status, 422, "limit={limit}");
    }
    for offset in ["-1", "x", ""] {
        let reply = search(&env, &format!("term=a&offset={offset}")).await;
        assert_eq!(reply.status, 422, "offset={offset}");
    }
    assert!(calls(&env).is_empty());
}

#[tokio::test]
async fn not_enabled_is_404_and_never_calls_the_state() {
    // Advertised but never activated.
    let env = Env::with(
        common::default_info(),
        vec![Capability::new(ACCOUNT_SEARCH_ID, ACCOUNT_SEARCH_VERSION)],
    )
    .await;
    let reply = search(&env, "term=a").await;
    assert_eq!(reply.status, 404);
    env.assert_signed(&reply);
    // Deactivated again.
    assert_eq!(env.activate("account_search", "0.1").await.status, 204);
    assert_eq!(search(&env, "term=a").await.status, 200);
    assert_eq!(env.deactivate("account_search", "0.1").await.status, 204);
    assert_eq!(search(&env, "term=a").await.status, 404);
    // Not advertised at all: activation is refused and so is the search.
    let plain = Env::new().await;
    assert_eq!(plain.activate("account_search", "0").await.status, 404);
    assert_eq!(search(&plain, "term=a").await.status, 404);
    assert_eq!(calls(&env).len(), 1);
    assert!(calls(&plain).is_empty());
}

#[tokio::test]
async fn returns_a_signed_json_array_of_uris() {
    let env = env().await;
    *env.ours.state.search_results.lock().unwrap() = uris(3);
    let reply = search(&env, "term=alice&limit=10").await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("content-type"), Some("application/json"));
    assert_eq!(reply.json(), serde_json::json!(uris(3)));
    assert!(reply.header("link").is_none());
    env.assert_signed(&reply);
    let id = env.outcome.registration_id;
    assert_eq!(calls(&env), vec![(id, "alice".to_owned(), 10, 0, None)]);
    // No results is an empty array, not an error.
    env.ours.state.search_results.lock().unwrap().clear();
    let reply = search(&env, "term=nobody").await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json(), serde_json::json!([]));
}

#[tokio::test]
async fn limit_defaults_to_20_and_is_capped_at_100() {
    let env = env().await;
    *env.ours.state.search_results.lock().unwrap() = uris(150);
    let default = search(&env, "term=a").await;
    assert_eq!(default.json().as_array().unwrap().len(), 20);
    let capped = search(&env, "term=a&limit=5000").await;
    assert_eq!(capped.json().as_array().unwrap().len(), 100);
    let huge = search(&env, "term=a&limit=99999999999999999999999999").await;
    assert_eq!(huge.status, 200);
    let limits: Vec<usize> = calls(&env).iter().map(|c| c.2).collect();
    assert_eq!(limits, [20, 100, 100]);
}

#[tokio::test]
async fn language_is_passed_through() {
    let env = env().await;
    search(&env, "term=a&language=pt-BR").await;
    search(&env, "term=a&language=").await;
    let languages: Vec<Option<String>> = calls(&env).into_iter().map(|c| c.4).collect();
    assert_eq!(languages, [Some("pt-BR".to_owned()), None]);
}

#[tokio::test]
async fn link_header_is_present_only_when_more_results_follow_and_is_encoded() {
    let env = env().await;
    let base = env.ours.base_url.clone();
    *env.ours.state.search_results.lock().unwrap() = uris(5);

    let first = search(&env, "term=a%20%26%20b%3Dc%2Bd&limit=2&language=en-GB").await;
    assert_eq!(first.status, 200);
    assert_eq!(first.json().as_array().unwrap().len(), 2);
    let link = first.header("link").unwrap().to_owned();
    assert_eq!(
        link,
        format!(
            "<{base}/account_search/v0/search?term=a+%26+b%3Dc%2Bd&limit=2&offset=2&language=en-GB>; rel=\"next\""
        )
    );
    env.assert_signed(&first);

    // Following the link (as a client would) gives the next page and the term round-trips.
    let target = link
        .strip_prefix('<')
        .and_then(|l| l.split_once(">;"))
        .map(|(url, _)| url.strip_prefix(base.as_str()).unwrap().to_owned())
        .unwrap();
    let second = env
        .mastodon_request("GET", &target, b"", &Opts::default())
        .await;
    assert_eq!(second.json().as_array().unwrap().len(), 2);
    assert!(second.header("link").is_some());
    let last_call = calls(&env).last().cloned().unwrap();
    assert_eq!(last_call.1, "a & b=c+d");
    assert_eq!((last_call.2, last_call.3), (2, 2));

    // The final page has no Link, and neither does an exactly-full single page.
    let last = search(&env, "term=a&limit=2&offset=4").await;
    assert_eq!(last.json().as_array().unwrap().len(), 1);
    assert!(last.header("link").is_none());
    let exact = search(&env, "term=a&limit=5").await;
    assert!(exact.header("link").is_none());
}

#[tokio::test]
async fn state_errors_are_signed_500s() {
    let env = env().await;
    env.ours.state.search_fails.store(true, Ordering::SeqCst);
    let reply = search(&env, "term=a").await;
    assert_eq!(reply.status, 500);
    env.assert_signed(&reply);
    assert!(!String::from_utf8_lossy(&reply.body).contains("boom"));
}

#[tokio::test]
async fn activation_accepts_major_and_full_versions() {
    let env = Env::with(
        common::default_info(),
        vec![Capability::new(ACCOUNT_SEARCH_ID, ACCOUNT_SEARCH_VERSION)],
    )
    .await;
    let id = env.outcome.registration_id;
    for version in ["0", "0.1"] {
        assert_eq!(env.deactivate("account_search", version).await.status, 204);
        assert_eq!(env.activate("account_search", version).await.status, 204);
        assert!(
            env.ours
                .state
                .with_store(|s| s.capability_enabled(id, "account_search", "0.1"))
                .unwrap()
        );
    }
    assert_eq!(env.activate("account_search", "1").await.status, 404);
    assert_eq!(env.activate("account_search", "0.2").await.status, 404);
}
