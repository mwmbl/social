//! The writer's apply semantics: the consent guarantees, one by one.

mod common;

use common::{TestEnv, test_config};
use mwmbl_social::decide::Action;
use mwmbl_social::writer::{ForgetTarget, OpenError, Writer, WriterLock};
use social_index::{SocialAccount, SocialPost};
use store::{Store, TombstoneKind};

fn post(uri: &str, extract: &str) -> SocialPost {
    SocialPost {
        uri: uri.into(),
        author_uri: "http://h.example/users/a".into(),
        extract: extract.into(),
        score: 1.0,
        term: String::new(),
        published: 100,
        checked: 200,
        title: None,
        language: None,
    }
}

fn account(uri: &str, languages: Option<Vec<String>>) -> SocialAccount {
    SocialAccount {
        uri: uri.into(),
        acct: "a@h.example".into(),
        display_name: "Ada".into(),
        summary: "bio".into(),
        score: 1.0,
        term: String::new(),
        checked: 200,
        languages,
    }
}

fn uris(env: &TestEnv) -> Vec<String> {
    env.post_pages().into_iter().map(|p| p.uri).collect()
}

#[tokio::test]
async fn index_then_replace_then_upsert() {
    let env = TestEnv::new().await;
    let old = post("http://h.example/s/1", "alpha beta");
    env.handle
        .apply(vec![Action::IndexPost {
            record: old.clone(),
            replaces: None,
        }])
        .await
        .unwrap();
    assert_eq!(env.search_posts("alpha").len(), 1);
    // The record we hold is replaced, and the old words are gone.
    let new = post("http://h.example/s/1", "gamma delta");
    env.handle
        .apply(vec![Action::IndexPost {
            record: new.clone(),
            replaces: Some(old),
        }])
        .await
        .unwrap();
    assert!(env.search_posts("alpha").is_empty());
    assert_eq!(env.search_posts("gamma").len(), 1);
    // Without the old record, upsert finds it by URI.
    env.handle
        .apply(vec![Action::IndexPost {
            record: post("http://h.example/s/1", "epsilon"),
            replaces: None,
        }])
        .await
        .unwrap();
    assert!(env.search_posts("gamma").is_empty());
    assert_eq!(env.search_posts("epsilon").len(), 1);
    assert!(env.store.list_tombstones().unwrap().is_empty());
}

#[tokio::test]
async fn remove_post_tombstone_rules() {
    let env = TestEnv::new().await;
    let tombstones = |env: &TestEnv| env.store.list_tombstones().unwrap().len();
    let index = |text: &str, n: u32| Action::IndexPost {
        record: post(&format!("http://h.example/s/{n}"), text),
        replaces: None,
    };
    env.handle
        .apply(vec![
            index("alpine", 1),
            index("birch", 2),
            index("cedar", 3),
        ])
        .await
        .unwrap();

    // Found by URI lookup, no record in hand, no tombstone asked for: none made.
    env.handle
        .apply(vec![Action::RemovePost {
            uri: "http://h.example/s/1".into(),
            record: None,
            tombstone: false,
        }])
        .await
        .unwrap();
    assert!(env.search_posts("alpine").is_empty());
    assert_eq!(tombstones(&env), 0);

    // Not in the index and no record: a tombstone, so search and the sweep cover any copies.
    env.handle
        .apply(vec![Action::RemovePost {
            uri: "http://h.example/s/404".into(),
            record: None,
            tombstone: false,
        }])
        .await
        .unwrap();
    assert_eq!(tombstones(&env), 1);

    // A record in hand that the URI lookup also finds: every copy is seen, no tombstone.
    let two = env.search_posts("birch").remove(0);
    let stored = env
        .post_pages()
        .into_iter()
        .find(|p| p.uri == two.uri)
        .unwrap();
    env.handle
        .apply(vec![Action::RemovePost {
            uri: two.uri.clone(),
            record: Some(stored),
            tombstone: false,
        }])
        .await
        .unwrap();
    assert!(env.search_posts("birch").is_empty());
    assert_eq!(tombstones(&env), 1);

    // A record in hand and a lookup that misses: a tombstone (D7), since a newer version
    // whose `uri:` copy fell off its page could only be found by URI.
    env.handle
        .apply(vec![Action::RemovePost {
            uri: "http://h.example/s/405".into(),
            record: Some(post("http://h.example/s/405", "dogwood")),
            tombstone: false,
        }])
        .await
        .unwrap();
    assert_eq!(tombstones(&env), 2);

    // Lifecycle deletes always tombstone, even when found.
    env.handle
        .apply(vec![Action::RemovePost {
            uri: "http://h.example/s/3".into(),
            record: None,
            tombstone: true,
        }])
        .await
        .unwrap();
    assert!(env.search_posts("cedar").is_empty());
    assert_eq!(tombstones(&env), 3);
    assert!(uris(&env).is_empty());
    // The tombstone is also in memory.
    assert_eq!(env.handle.stats().await.unwrap().tombstones, 3);
}

/// D7: revalidation carries version 1, but version 2 is indexed under new keys before it runs.
/// Removing version 1 by its keys also clears the shared `uri:` copy, so the URI lookup must
/// run first, or it misses and version 2 stays searchable with no tombstone.
#[tokio::test]
async fn removing_a_carried_record_also_removes_a_newer_version() {
    let env = TestEnv::new().await;
    let uri = "http://h.example/s/7";
    let v1 = post(uri, "walnut");
    let v2 = post(uri, "hazelnut");
    for record in [v1.clone(), v2] {
        env.handle
            .apply(vec![Action::IndexPost {
                record,
                replaces: None,
            }])
            .await
            .unwrap();
    }
    assert_eq!(env.search_posts("hazelnut").len(), 1);
    env.handle
        .apply(vec![Action::RemovePost {
            uri: uri.into(),
            record: Some(v1),
            tombstone: false,
        }])
        .await
        .unwrap();
    assert!(env.search_posts("hazelnut").is_empty());
    assert!(uris(&env).is_empty(), "a copy is left on a page");
    assert!(env.store.list_tombstones().unwrap().is_empty());
}

#[tokio::test]
async fn accounts_keep_observed_languages_and_removal_works() {
    let env = TestEnv::new().await;
    let uri = "http://h.example/users/a";
    env.handle
        .apply(vec![Action::IndexAccount {
            record: account(uri, Some(vec!["en".into()])),
            replaces: None,
        }])
        .await
        .unwrap();
    // A fresh record without languages, and no stored record carried: languages carry over.
    env.handle
        .apply(vec![Action::IndexAccount {
            record: account(uri, None),
            replaces: None,
        }])
        .await
        .unwrap();
    let found = env.search_accounts("ada");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].languages, Some(vec!["en".to_string()]));

    env.handle
        .apply(vec![Action::RemoveAccount {
            uri: uri.into(),
            record: None,
            tombstone: false,
        }])
        .await
        .unwrap();
    assert!(env.search_accounts("ada").is_empty());
    assert!(env.account_pages().is_empty());
    assert!(env.store.list_tombstones().unwrap().is_empty());

    // Removing what isn't there tombstones it.
    env.handle
        .apply(vec![Action::RemoveAccount {
            uri: uri.into(),
            record: None,
            tombstone: false,
        }])
        .await
        .unwrap();
    assert_eq!(env.store.list_tombstones().unwrap().len(), 1);
}

#[tokio::test]
async fn author_tombstones_hide_posts_and_clear() {
    let env = TestEnv::new().await;
    env.handle
        .apply(vec![Action::IndexPost {
            record: post("http://h.example/s/1", "hello"),
            replaces: None,
        }])
        .await
        .unwrap();
    env.handle
        .apply(vec![Action::AddAuthorTombstone {
            actor_uri: "http://h.example/users/a".into(),
        }])
        .await
        .unwrap();
    assert!(env.search_posts("hello").is_empty());
    let rows = env.store.list_tombstones().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, TombstoneKind::Author);
    // Hidden, not yet physically removed: that is the sweep's job.
    assert!(!uris(&env).is_empty());

    env.handle
        .apply(vec![Action::ClearAuthorTombstone {
            actor_uri: "http://h.example/users/a".into(),
        }])
        .await
        .unwrap();
    assert_eq!(env.search_posts("hello").len(), 1);
    assert!(env.store.list_tombstones().unwrap().is_empty());
}

#[tokio::test]
async fn forget_a_uri_removes_it_from_both_indexes_and_tombstones_both_ways() {
    let env = TestEnv::new().await;
    let uri = "http://h.example/users/a";
    env.handle
        .apply(vec![
            Action::IndexAccount {
                record: account(uri, None),
                replaces: None,
            },
            Action::IndexPost {
                record: post(uri, "odd object with the actor's uri"),
                replaces: None,
            },
        ])
        .await
        .unwrap();
    env.handle
        .forget(ForgetTarget::Uri(uri.into()))
        .await
        .unwrap();
    assert!(env.account_pages().is_empty());
    assert!(env.post_pages().is_empty());
    let mut kinds: Vec<_> = env
        .store
        .list_tombstones()
        .unwrap()
        .into_iter()
        .map(|t| t.kind)
        .collect();
    kinds.sort();
    assert_eq!(kinds, vec![TombstoneKind::Object, TombstoneKind::Author]);
}

#[tokio::test]
async fn rows_written_by_another_process_are_picked_up_on_reload() {
    let env = TestEnv::new().await;
    env.handle
        .apply(vec![Action::IndexPost {
            record: post("http://h.example/s/1", "hello"),
            replaces: None,
        }])
        .await
        .unwrap();
    assert_eq!(env.search_posts("hello").len(), 1);

    // What `forget` does while a server holds the lock: write SQLite only.
    let other = Store::open(&env.config.store_path()).unwrap();
    other.add_domain_block("H.example", 1).unwrap();
    assert_eq!(env.search_posts("hello").len(), 1, "not yet loaded");
    env.handle.reload_tombstones().await.unwrap();
    assert!(env.search_posts("hello").is_empty());
    other.remove_domain_block("h.example").unwrap();
    other
        .add_tombstone("http://h.example/s/1", TombstoneKind::Object, 5)
        .unwrap();
    env.handle.reload_tombstones().await.unwrap();
    assert!(env.search_posts("hello").is_empty());
    // Clearing retires old rows and reloads.
    assert_eq!(env.handle.clear_tombstones_before(10).await.unwrap(), 1);
    assert_eq!(env.search_posts("hello").len(), 1);
}

#[tokio::test]
async fn tombstones_and_blocks_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(&dir);
    {
        let writer = Writer::start(&config).unwrap();
        let handle = writer.handle();
        handle
            .forget(ForgetTarget::Domain("bad.example".into()))
            .await
            .unwrap();
        handle
            .apply(vec![Action::AddAuthorTombstone {
                actor_uri: "http://h.example/users/a".into(),
            }])
            .await
            .unwrap();
        drop(handle);
        writer.finish();
    }
    let writer = Writer::start(&config).unwrap();
    let handle = writer.handle();
    assert_eq!(handle.stats().await.unwrap().tombstones, 2);
    let tombstones = handle.tombstones();
    assert!(tombstones.read().unwrap().domain_blocked("sub.bad.example"));
}

#[tokio::test]
async fn only_one_writer_per_data_dir() {
    let env = TestEnv::new().await;
    assert!(WriterLock::try_acquire(&env.config).unwrap().is_none());
    assert!(matches!(
        Writer::start(&env.config),
        Err(OpenError::Locked { .. })
    ));
    let TestEnv {
        writer,
        handle,
        worker,
        config,
        ..
    } = env;
    drop(worker);
    drop(handle);
    writer.finish();
    // Released when the writer stops.
    assert!(WriterLock::try_acquire(&config).unwrap().is_some());
}

#[tokio::test]
async fn reindexing_after_a_fresh_consented_fetch_clears_the_object_tombstone() {
    // Made unlisted by an edit (removed, tombstoned), then public again (indexed). The old
    // tombstone must not make the sweep delete the post that now has consent.
    let env = TestEnv::new().await;
    let p = post("http://h.example/s/9", "garlic allotment");
    env.handle
        .apply(vec![Action::RemovePost {
            uri: p.uri.clone(),
            record: None,
            tombstone: true,
        }])
        .await
        .unwrap();
    assert_eq!(env.store.list_tombstones().unwrap().len(), 1);
    env.handle
        .apply(vec![Action::IndexPost {
            record: p.clone(),
            replaces: None,
        }])
        .await
        .unwrap();
    assert!(env.store.list_tombstones().unwrap().is_empty());
    assert_eq!(env.search_posts("garlic").len(), 1);
}

#[tokio::test]
async fn a_forgotten_uri_is_never_indexed_again_even_after_tombstones_clear() {
    let env = TestEnv::new().await;
    let p = post("http://h.example/s/10", "turnip");
    env.handle
        .forget(ForgetTarget::Uri(p.uri.clone()))
        .await
        .unwrap();
    // The sweep clears every tombstone row eventually.
    env.handle.clear_tombstones_before(i64::MAX).await.unwrap();
    assert!(env.store.list_tombstones().unwrap().is_empty());
    env.handle
        .apply(vec![Action::IndexPost {
            record: p.clone(),
            replaces: None,
        }])
        .await
        .unwrap();
    assert!(env.search_posts("turnip").is_empty());
    assert!(!uris(&env).contains(&p.uri));
    // A forgotten author's posts are not indexed either, and their author tombstone stays.
    env.handle
        .forget(ForgetTarget::Uri("http://h.example/users/a".into()))
        .await
        .unwrap();
    env.handle
        .apply(vec![
            Action::IndexPost {
                record: post("http://h.example/s/11", "parsnip"),
                replaces: None,
            },
            Action::ClearAuthorTombstone {
                actor_uri: "http://h.example/users/a".into(),
            },
        ])
        .await
        .unwrap();
    assert!(env.search_posts("parsnip").is_empty());
    assert!(
        env.store
            .list_tombstones()
            .unwrap()
            .iter()
            .any(|t| t.kind == TombstoneKind::Author && t.uri == "http://h.example/users/a")
    );
}
