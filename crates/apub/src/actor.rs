//! The documents we serve: our server actor, `WebFinger` and `NodeInfo`.
//!
//! Pure JSON builders. The binary wires them to routes. The actor carries the public key other
//! servers verify our signed fetches and deliveries with, and it is how authors on software
//! without FASP opt in: following it means "index my public posts" (PLAN.md Stage 3), which
//! [`SUMMARY`] says plainly. It accepts every follow, so it does not manually approve
//! followers, and it is neither discoverable nor indexable itself.

use serde_json::{Value, json};
use url::Url;

const NODEINFO_SCHEMA_2_1: &str = "http://nodeinfo.diaspora.software/ns/schema/2.1";

fn base(base_url: &str) -> &str {
    base_url.trim_end_matches('/')
}

/// What the actor's profile says about following it.
pub const SUMMARY: &str = "Mwmbl Social, a fediverse search service. Following this account \
means: index my public posts. It follows you back to receive them, and indexes only public \
posts, never when your account says not to (discoverable or indexable off). Unfollow it and \
your posts are removed from the index.";

/// The URL of our actor, `{base_url}/actor`.
#[must_use]
pub fn server_actor_url(base_url: &str) -> String {
    format!("{}/actor", base(base_url))
}

/// The id of the actor's key, `{base_url}/actor#main-key`: what `FetcherConfig::key_id` is.
#[must_use]
pub fn server_actor_key_id(base_url: &str) -> String {
    format!("{}#main-key", server_actor_url(base_url))
}

/// Our `Application` actor, with the public key (SPKI PEM) other servers check signatures with.
#[must_use]
pub fn server_actor(base_url: &str, username: &str, public_key_pem: &str) -> Value {
    let actor = server_actor_url(base_url);
    json!({
        "@context": [
            "https://www.w3.org/ns/activitystreams",
            "https://w3id.org/security/v1",
        ],
        "id": actor,
        "type": "Application",
        "preferredUsername": username,
        "name": "Mwmbl Social",
        "summary": SUMMARY,
        "url": actor,
        "inbox": format!("{}/inbox", actor),
        "outbox": format!("{}/outbox", actor),
        "endpoints": {"sharedInbox": format!("{}/inbox", base(base_url))},
        "manuallyApprovesFollowers": false,
        "discoverable": false,
        "indexable": false,
        "publicKey": {
            "id": server_actor_key_id(base_url),
            "owner": actor,
            "publicKeyPem": public_key_pem,
        },
    })
}

/// `host` or `host:port` of `base_url`.
fn host_of(base_url: &str) -> Option<String> {
    let url = Url::parse(base(base_url)).ok()?;
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })
}

/// The `WebFinger` answer for `resource`, if it names our actor: `acct:{username}@{host}` (host
/// compared case-insensitively) or the actor URL. Anything else is `None` (a 404).
#[must_use]
pub fn webfinger(resource: &str, base_url: &str, username: &str) -> Option<Value> {
    let host = host_of(base_url)?;
    let actor = server_actor_url(base_url);
    let subject = format!("acct:{username}@{host}");
    let resource = resource.trim();
    let matches = resource == actor
        || resource
            .get(..5)
            .is_some_and(|p| p.eq_ignore_ascii_case("acct:"))
            && resource
                .get(5..)
                .and_then(|acct| acct.split_once('@'))
                .is_some_and(|(user, h)| user == username && h.eq_ignore_ascii_case(&host));
    matches.then(|| {
        json!({
            "subject": subject,
            "aliases": [actor],
            "links": [{
                "rel": "self",
                "type": "application/activity+json",
                "href": actor,
            }],
        })
    })
}

/// `/.well-known/nodeinfo`.
#[must_use]
pub fn nodeinfo_well_known(base_url: &str) -> Value {
    json!({
        "links": [{
            "rel": NODEINFO_SCHEMA_2_1,
            "href": format!("{}/nodeinfo/2.1", base(base_url)),
        }],
    })
}

/// `NodeInfo` 2.1.
#[must_use]
pub fn nodeinfo(software_version: &str) -> Value {
    json!({
        "version": "2.1",
        "software": {
            "name": "mwmbl-social",
            "version": software_version,
            "repository": env!("CARGO_PKG_REPOSITORY"),
        },
        "protocols": ["activitypub"],
        "services": {"inbound": [], "outbound": []},
        "openRegistrations": false,
        "usage": {"users": {"total": 0}},
        "metadata": {},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://social.example";

    #[test]
    fn actor_document() {
        let a = server_actor("https://social.example/", "mwmbl-social", "PEM");
        assert_eq!(a["id"], "https://social.example/actor");
        assert_eq!(a["type"], "Application");
        assert_eq!(a["preferredUsername"], "mwmbl-social");
        assert_eq!(a["inbox"], "https://social.example/actor/inbox");
        assert_eq!(a["outbox"], "https://social.example/actor/outbox");
        assert_eq!(a["publicKey"]["id"], server_actor_key_id(BASE));
        assert_eq!(a["publicKey"]["owner"], "https://social.example/actor");
        assert_eq!(a["publicKey"]["publicKeyPem"], "PEM");
        assert_eq!(
            a["endpoints"]["sharedInbox"],
            "https://social.example/inbox"
        );
        assert_eq!(a["manuallyApprovesFollowers"], false);
        let summary = a["summary"].as_str().unwrap();
        assert!(summary.contains("index my public posts"));
        assert!(summary.contains("Unfollow it and your posts are removed"));
        assert_eq!(a["discoverable"], false);
        assert_eq!(a["indexable"], false);
        assert_eq!(
            server_actor_key_id(BASE),
            "https://social.example/actor#main-key"
        );
    }

    #[test]
    fn webfinger_resources() {
        let found = webfinger("acct:mwmbl-social@social.example", BASE, "mwmbl-social").unwrap();
        assert_eq!(found["subject"], "acct:mwmbl-social@social.example");
        assert_eq!(found["aliases"][0], "https://social.example/actor");
        assert_eq!(found["links"][0]["rel"], "self");
        assert_eq!(found["links"][0]["type"], "application/activity+json");
        assert_eq!(found["links"][0]["href"], "https://social.example/actor");
        assert!(webfinger("ACCT:mwmbl-social@SOCIAL.example", BASE, "mwmbl-social").is_some());
        assert!(webfinger("https://social.example/actor", BASE, "mwmbl-social").is_some());
        for other in [
            "acct:other@social.example",
            "acct:mwmbl-social@other.example",
            "https://social.example/actor/inbox",
            "mwmbl-social@social.example",
            "acct:",
            "",
        ] {
            assert!(webfinger(other, BASE, "mwmbl-social").is_none(), "{other}");
        }
        assert!(webfinger("acct:a@b", "not a url", "a").is_none());
        let with_port = webfinger("acct:u@localhost:8080", "http://localhost:8080", "u").unwrap();
        assert_eq!(with_port["subject"], "acct:u@localhost:8080");
    }

    #[test]
    fn nodeinfo_documents() {
        let wk = nodeinfo_well_known("https://social.example/");
        assert_eq!(
            wk["links"][0]["href"],
            "https://social.example/nodeinfo/2.1"
        );
        assert_eq!(wk["links"][0]["rel"], NODEINFO_SCHEMA_2_1);
        let n = nodeinfo("1.2.3");
        assert_eq!(n["version"], "2.1");
        assert_eq!(n["software"]["name"], "mwmbl-social");
        assert_eq!(n["software"]["version"], "1.2.3");
        assert_eq!(n["protocols"], json!(["activitypub"]));
        assert_eq!(n["openRegistrations"], false);
        assert_eq!(n["usage"]["users"]["total"], 0);
        assert_eq!(n["metadata"], json!({}));
    }
}
