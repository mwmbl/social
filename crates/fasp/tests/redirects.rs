//! Audit B5: the FASP and registration client follows no redirects. A server that redirects
//! our signed calls elsewhere gets an error naming the URL and the target, and the target never
//! sees the request (nor its signature headers).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use common::Env;
use fasp::client::{FaspClient, http_client_builder};
use fasp::registration::discover_fasp_base_url;
use fasp::{ClientError, RegistrationError};
use store::{Category, SubscriptionType};
use url::Url;

#[derive(Clone)]
struct Fake {
    hits: Arc<AtomicUsize>,
    target: Option<String>,
    status: StatusCode,
}

async fn answer(State(fake): State<Fake>, request: Request) -> Response {
    fake.hits.fetch_add(1, Ordering::SeqCst);
    let mut response = fake.status.into_response();
    if let Some(target) = fake.target {
        let location = format!("{target}{}", request.uri().path());
        response
            .headers_mut()
            .insert("location", HeaderValue::from_str(&location).unwrap());
    }
    response
}

/// A server that counts the requests it gets and answers each with `status`, pointing
/// `location` at `target` plus the request path when given.
async fn serve(target: Option<String>, status: StatusCode) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let fake = Fake {
        hits: hits.clone(),
        target,
        status,
    };
    let app = Router::new().fallback(answer).with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), hits)
}

/// A redirecting server, and the other origin it points at.
async fn redirector(status: StatusCode) -> (String, Arc<AtomicUsize>, String, Arc<AtomicUsize>) {
    let (target, target_hits) = serve(None, StatusCode::OK).await;
    let (origin, hits) = serve(Some(target.clone()), status).await;
    (origin, hits, target, target_hits)
}

fn http() -> reqwest::Client {
    // Private addresses allowed only to reach the fakes on 127.0.0.1.
    http_client_builder(true).build().unwrap()
}

#[tokio::test]
async fn registration_does_not_follow_a_redirect_to_another_origin() {
    let (origin, hits, target, target_hits) = redirector(StatusCode::FOUND).await;
    let server = Url::parse(&origin).unwrap();
    let err = discover_fasp_base_url(&http(), &server, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, RegistrationError::BadResponse { .. }),
        "{err}"
    );
    let text = err.to_string();
    assert!(
        text.contains(&format!("{origin}/.well-known/nodeinfo")),
        "{text}"
    );
    assert!(text.contains(&target), "{text}");
    assert!(text.contains("not followed"), "{text}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(target_hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn signed_fasp_calls_do_not_follow_a_redirect_to_another_origin() {
    let env = Env::new().await;
    env.enable_data_sharing_in_store();
    let (origin, hits, target, target_hits) = redirector(StatusCode::TEMPORARY_REDIRECT).await;
    // The stored registration, pointed at the redirecting server.
    let mut registration = env.registration();
    registration.base_url = format!("{origin}/api/fasp");
    let client = FaspClient::new(http(), env.ours.state.clone(), &registration).unwrap();
    let err = client
        .data_sharing()
        .unwrap()
        .subscribe(Category::Content, SubscriptionType::Lifecycle, None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, ClientError::BadResponse { .. }), "{err}");
    let text = err.to_string();
    assert!(text.contains("127.0.0.1"), "names the host: {text}");
    assert!(
        text.contains("/api/fasp/data_sharing/v0/event_subscriptions"),
        "{text}"
    );
    assert!(text.contains(&target), "names the target: {text}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(target_hits.load(Ordering::SeqCst), 0);
}
