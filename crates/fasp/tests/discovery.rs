//! `trends` and `follow_recommendation`: capability gating, parameters, response shapes and
//! signatures, exercised the way a server calls them.

mod common;

use common::{Env, Opts, Reply};
use fasp::http::{TrendingContent, TrendingTag, TrendsQuery};
use fasp::{
    Capability, FOLLOW_RECOMMENDATION_ID, FOLLOW_RECOMMENDATION_VERSION, TRENDS_ID, TRENDS_VERSION,
};
use serde_json::json;

fn capabilities() -> Vec<Capability> {
    vec![
        Capability::new(TRENDS_ID, TRENDS_VERSION),
        Capability::new(FOLLOW_RECOMMENDATION_ID, FOLLOW_RECOMMENDATION_VERSION),
    ]
}

async fn env() -> Env {
    let env = Env::with(common::default_info(), capabilities()).await;
    for capability in ["trends", "follow_recommendation"] {
        assert_eq!(env.activate(capability, "0").await.status, 204);
    }
    env
}

async fn get(env: &Env, path: &str) -> Reply {
    env.mastodon_request("GET", path, b"", &Opts::default())
        .await
}

fn content(n: usize) -> Vec<TrendingContent> {
    (0..n)
        .map(|i| TrendingContent {
            uri: format!("https://h.example/s/{i}"),
            rank: u8::try_from(100 - i).unwrap(),
        })
        .collect()
}

#[tokio::test]
async fn both_are_advertised_and_activate_without_side_effects() {
    let env = Env::with(common::default_info(), capabilities()).await;
    let info = get(&env, "/provider_info").await.json();
    let ids: Vec<_> = info["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["id"].as_str().unwrap().to_owned(),
                c["version"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(ids.contains(&("trends".into(), "0.1".into())));
    assert!(ids.contains(&("follow_recommendation".into(), "0.1".into())));
    for version in ["0", "0.1"] {
        assert_eq!(env.activate("trends", version).await.status, 204);
        assert_eq!(
            env.activate("follow_recommendation", version).await.status,
            204
        );
    }
    // No call to the server: activation is local.
    assert_eq!(env.mastodon.request_count(), 0);
}

#[tokio::test]
async fn not_enabled_is_a_signed_404_and_never_calls_the_state() {
    let env = Env::with(common::default_info(), capabilities()).await;
    for path in [
        "/trends/v0/content",
        "/trends/v0/hashtags",
        "/trends/v0/links",
        "/follow_recommendation/v0/accounts?accountUri=https%3A%2F%2Fh.example%2Fu%2F1",
    ] {
        let reply = get(&env, path).await;
        assert_eq!(reply.status, 404, "{path}");
        env.assert_signed(&reply);
    }
    // Enabling one does not enable the other; disabling turns it off again.
    assert_eq!(env.activate("trends", "0").await.status, 204);
    assert_eq!(get(&env, "/trends/v0/content").await.status, 200);
    let rec = "/follow_recommendation/v0/accounts?accountUri=x";
    assert_eq!(get(&env, rec).await.status, 404);
    assert_eq!(env.deactivate("trends", "0").await.status, 204);
    assert_eq!(get(&env, "/trends/v0/content").await.status, 404);
    assert_eq!(env.ours.state.trend_calls.lock().unwrap().len(), 1);
    assert!(
        env.ours
            .state
            .recommendation_calls
            .lock()
            .unwrap()
            .is_empty()
    );
    // Not advertised at all: refused.
    let plain = Env::new().await;
    assert_eq!(plain.activate("trends", "0").await.status, 404);
    assert_eq!(
        plain.activate("follow_recommendation", "0").await.status,
        404
    );
    assert_eq!(get(&plain, "/trends/v0/content").await.status, 404);
}

#[tokio::test]
async fn trending_content_is_a_signed_object_truncated_to_max_count() {
    let env = env().await;
    *env.ours.state.trending_content.lock().unwrap() = content(30);
    let reply = get(&env, "/trends/v0/content").await;
    assert_eq!(reply.status, 200);
    env.assert_signed(&reply);
    let body = reply.json();
    let list = body["content"].as_array().unwrap();
    assert_eq!(list.len(), 20, "maxCount defaults to 20");
    assert_eq!(
        list[0],
        json!({"uri": "https://h.example/s/0", "rank": 100})
    );
    let reply = get(
        &env,
        "/trends/v0/content?withinLastHours=2&maxCount=3&language=fr",
    )
    .await;
    assert_eq!(reply.json()["content"].as_array().unwrap().len(), 3);
    let calls = env.ours.state.trend_calls.lock().unwrap().clone();
    let id = env.outcome.registration_id;
    assert_eq!(
        calls,
        vec![
            (
                id,
                "content",
                TrendsQuery {
                    within_last_hours: 24,
                    max_count: 20,
                    language: None
                }
            ),
            (
                id,
                "content",
                TrendsQuery {
                    within_last_hours: 2,
                    max_count: 3,
                    language: Some("fr".into())
                }
            ),
        ]
    );
}

#[tokio::test]
async fn trends_options_are_validated_and_clamped() {
    let env = env().await;
    for query in [
        "withinLastHours=0",
        "withinLastHours=-1",
        "withinLastHours=abc",
        "withinLastHours=",
        "maxCount=0",
        "maxCount=1.5",
    ] {
        let reply = get(&env, &format!("/trends/v0/content?{query}")).await;
        assert_eq!(reply.status, 422, "{query}");
        env.assert_signed(&reply);
    }
    assert!(env.ours.state.trend_calls.lock().unwrap().is_empty());
    get(
        &env,
        "/trends/v0/hashtags?withinLastHours=100000&maxCount=999999",
    )
    .await;
    let calls = env.ours.state.trend_calls.lock().unwrap().clone();
    assert_eq!(calls[0].1, "hashtags");
    assert_eq!(calls[0].2.within_last_hours, 168);
    assert_eq!(calls[0].2.max_count, 100);
}

#[tokio::test]
async fn hashtags_and_links_have_the_spec_shapes() {
    let env = env().await;
    *env.ours.state.trending_tags.lock().unwrap() = vec![TrendingTag {
        name: "#cats".into(),
        rank: 72,
        examples: vec!["https://h.example/s/1".into()],
    }];
    let reply = get(&env, "/trends/v0/hashtags").await;
    assert_eq!(
        reply.json(),
        json!({"hashtags": [{"name": "#cats", "rank": 72, "examples": ["https://h.example/s/1"]}]})
    );
    env.assert_signed(&reply);
    // The default links answer is an honest empty list, not an error.
    let reply = get(&env, "/trends/v0/links").await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json(), json!({"links": []}));
    env.assert_signed(&reply);
}

#[tokio::test]
async fn follow_recommendation_requires_account_uri_and_returns_an_array() {
    let env = env().await;
    for query in ["", "?language=en", "?accountUri=", "?accountUri=%20"] {
        let reply = get(&env, &format!("/follow_recommendation/v0/accounts{query}")).await;
        assert_eq!(reply.status, 422, "{query}");
        env.assert_signed(&reply);
    }
    *env.ours.state.recommendations.lock().unwrap() = vec!["https://o.example/u/2".into()];
    let reply = get(
        &env,
        "/follow_recommendation/v0/accounts?accountUri=https%3A%2F%2Ffedi.example.com%2Fusers%2F1&language=de",
    )
    .await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("content-type"), Some("application/json"));
    assert_eq!(reply.json(), json!(["https://o.example/u/2"]));
    env.assert_signed(&reply);
    assert_eq!(
        env.ours.state.recommendation_calls.lock().unwrap().clone(),
        vec![(
            env.outcome.registration_id,
            "https://fedi.example.com/users/1".to_owned(),
            Some("de".to_owned())
        )]
    );
}

#[tokio::test]
async fn other_methods_are_405() {
    let env = env().await;
    for path in ["/trends/v0/content", "/follow_recommendation/v0/accounts"] {
        let reply = env
            .mastodon_request("POST", path, b"", &Opts::default())
            .await;
        assert_eq!(reply.status, 405, "{path}");
    }
}
