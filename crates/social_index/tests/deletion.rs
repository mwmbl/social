//! Deleted documents leave no trace on any page.
//!
//! A seeded random workload over a small vocabulary and few pages, so terms collide and pages
//! overflow, which is exactly when `uri:` lookups miss and the tombstone path has to work.

use std::collections::{BTreeMap, BTreeSet};

use social_index::{
    DeleteOutcome, KeyScheme, Keyed, SocialIndex, SocialPost, SweepSeen, Tombstones, sweep_page,
};

const PAGE_SIZE: usize = 4096;
const WORDS: &[&str] = &[
    "rust",
    "fediverse",
    "search",
    "index",
    "mastodon",
    "cat",
    "dog",
    "coffee",
    "tea",
    "music",
    "garden",
    "rain",
    "sun",
    "bread",
    "train",
    "bike",
    "book",
    "film",
    "code",
    "river",
];

/// A small deterministic generator, so a failure names its seed and reproduces.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next()).unwrap() % n
    }
}

fn random_post(rng: &mut Lcg, id: usize) -> SocialPost {
    let len = 3 + rng.below(25);
    let mut words: Vec<&str> = (0..len).map(|_| WORDS[rng.below(WORDS.len())]).collect();
    if rng.below(3) == 0 {
        words.push("#fedi");
    }
    SocialPost {
        uri: format!("https://h{}.example/statuses/{id}", rng.below(3)),
        author_uri: format!("https://h0.example/users/u{}", rng.below(5)),
        extract: words.join(" "),
        score: f64::from(u32::try_from(rng.below(1000)).unwrap()),
        term: String::new(),
        published: 1_000,
        checked: i64::try_from(rng.below(100)).unwrap(),
        title: (rng.below(4) == 0).then(|| {
            format!(
                "{} {}",
                WORDS[rng.below(WORDS.len())],
                WORDS[rng.below(WORDS.len())]
            )
        }),
        language: None,
    }
}

fn uris_on_pages(index: &SocialIndex<SocialPost>) -> BTreeMap<String, usize> {
    let mut found = BTreeMap::new();
    for (_, page) in index.reader().index().pages() {
        for item in page.unwrap() {
            *found.entry(item.uri.clone()).or_insert(0) += 1;
        }
    }
    found
}

fn run(seed: u64, num_pages: usize, num_posts: usize) {
    let dir = tempfile::tempdir().unwrap();
    let mut index = SocialIndex::<SocialPost>::create(
        &dir.path().join("posts.tinyindex"),
        num_pages,
        PAGE_SIZE,
        KeyScheme::default(),
    )
    .unwrap();
    let mut rng = Lcg(seed);
    let mut live: Vec<SocialPost> = Vec::new();
    let mut deleted: BTreeSet<String> = BTreeSet::new();
    let mut tombstones = Tombstones::new();

    for id in 0..num_posts {
        let action = rng.below(10);
        if action < 6 || live.is_empty() {
            let post = random_post(&mut rng, id);
            index.add(&post).unwrap();
            live.push(post);
        } else if action < 8 {
            // An edit: same URI, new text, so the keys change.
            let i = rng.below(live.len());
            let mut post = random_post(&mut rng, id);
            post.uri = live[i].uri.clone();
            post.author_uri = live[i].author_uri.clone();
            if index.upsert(&post).unwrap() == DeleteOutcome::NotFound {
                // The old version's uri: copy had fallen off. Its other copies may remain,
                // under keys the new version doesn't have. Revalidation is what finds those
                // (their `checked` is older); here we just track that they are stale.
            }
            live[i] = post;
        } else {
            let i = rng.below(live.len());
            let post = live.swap_remove(i);
            if index.delete(&post.uri).unwrap() == DeleteOutcome::NotFound {
                tombstones.add_object(&post.uri);
            }
            // Lifecycle deletes always tombstone too (see PLAN.md), because an earlier edit
            // whose lookup missed can leave copies of the old version.
            tombstones.add_object(&post.uri);
            deleted.insert(post.uri);
        }
    }

    // Search-time filtering: nothing deleted is visible through any key.
    let reader = index.reader();
    for word in WORDS.iter().copied().chain(["#fedi"]) {
        for item in reader.retrieve(word).unwrap() {
            if deleted.contains(&item.uri) {
                assert!(
                    tombstones.blocks(&item),
                    "seed {seed}: {} visible under {word}",
                    item.uri
                );
            }
        }
    }

    // A full sweep removes every remaining copy of everything tombstoned.
    let mut seen = SweepSeen::new(1_000_000);
    for page in 0..num_pages {
        sweep_page(&mut index, page, &tombstones, i64::MIN, &mut seen).unwrap();
    }
    let remaining = uris_on_pages(&index);
    for uri in &deleted {
        assert!(
            !remaining.contains_key(uri),
            "seed {seed}: {uri} left on a page after the sweep"
        );
    }

    // Deleting a live post by URI, when found, removes every copy of its current version.
    // Copies of an older version can remain if an edit's lookup missed; those carry other
    // text and an older `checked`, and revalidation removes them.
    let mut removed = Vec::new();
    for post in &live {
        if let DeleteOutcome::Removed { .. } = index.delete(&post.uri).unwrap() {
            removed.push(post);
        }
    }
    let mut leftovers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (_, page) in index.reader().index().pages() {
        for item in page.unwrap() {
            leftovers.entry(item.uri).or_default().push(item.extract);
        }
    }
    for post in removed {
        for extract in leftovers.get(&post.uri).into_iter().flatten() {
            assert_ne!(
                extract, &post.extract,
                "seed {seed}: current version of {} survived delete",
                post.uri
            );
        }
    }
}

#[test]
fn deleted_documents_leave_no_trace_roomy_index() {
    for seed in 0..8 {
        run(seed, 256, 150);
    }
}

#[test]
fn deleted_documents_leave_no_trace_crowded_index() {
    // Few pages and many posts: pages overflow, uri: lookups miss, tombstones do the work.
    for seed in 100..108 {
        run(seed, 4, 300);
    }
}

#[test]
fn delete_by_uri_removes_every_copy_and_find_misses_after() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = SocialIndex::<SocialPost>::create(
        &dir.path().join("p"),
        64,
        PAGE_SIZE,
        KeyScheme::default(),
    )
    .unwrap();
    let mut rng = Lcg(7);
    let post = random_post(&mut rng, 1);
    let other = random_post(&mut rng, 2);
    index.add(&post).unwrap();
    index.add(&other).unwrap();
    assert_eq!(
        index.find(&post.uri).unwrap().unwrap().extract,
        post.extract
    );
    let copies = post.keys(index.scheme()).len();
    assert_eq!(
        index.delete(&post.uri).unwrap(),
        DeleteOutcome::Removed { copies }
    );
    assert!(!uris_on_pages(&index).contains_key(&post.uri));
    assert!(index.find(&post.uri).unwrap().is_none());
    assert_eq!(index.delete(&post.uri).unwrap(), DeleteOutcome::NotFound);
    assert!(uris_on_pages(&index).contains_key(&other.uri));
}

#[test]
fn an_edit_moves_the_post_to_its_new_keys() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = SocialIndex::<SocialPost>::create(
        &dir.path().join("p"),
        256,
        PAGE_SIZE,
        KeyScheme::default(),
    )
    .unwrap();
    let mut rng = Lcg(9);
    let mut post = random_post(&mut rng, 1);
    post.extract = "coffee garden".into();
    post.title = None;
    index.add(&post).unwrap();
    post.extract = "tea river".into();
    index.upsert(&post).unwrap();
    let r = index.reader();
    assert!(
        r.retrieve("coffee")
            .unwrap()
            .iter()
            .all(|p| p.uri != post.uri)
    );
    assert!(r.retrieve("tea").unwrap().iter().any(|p| p.uri == post.uri));
}

#[test]
fn replace_after_revalidation_refreshes_checked_everywhere() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = SocialIndex::<SocialPost>::create(
        &dir.path().join("p"),
        256,
        PAGE_SIZE,
        KeyScheme::default(),
    )
    .unwrap();
    let mut rng = Lcg(11);
    let old = random_post(&mut rng, 1);
    index.add(&old).unwrap();
    let mut seen = SweepSeen::new(100);
    let mut stale = Vec::new();
    for page in 0..256 {
        stale.extend(
            sweep_page(&mut index, page, &Tombstones::new(), 1_000, &mut seen)
                .unwrap()
                .revalidate,
        );
    }
    assert_eq!(stale.len(), 1, "one revalidation per URI, not one per copy");
    let mut fresh = stale[0].clone();
    fresh.set_checked(5_000);
    index.replace(&stale[0], &fresh).unwrap();
    for (_, page) in index.reader().index().pages() {
        for item in page.unwrap() {
            assert_eq!(item.checked, 5_000);
        }
    }
}

#[test]
fn sweep_applies_author_tombstones() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = SocialIndex::<SocialPost>::create(
        &dir.path().join("p"),
        32,
        PAGE_SIZE,
        KeyScheme::default(),
    )
    .unwrap();
    let mut rng = Lcg(13);
    let posts: Vec<_> = (0..30).map(|i| random_post(&mut rng, i)).collect();
    for p in &posts {
        index.add(p).unwrap();
    }
    let mut t = Tombstones::new();
    t.add_author("https://h0.example/users/u1");
    let mut seen = SweepSeen::new(10);
    let mut deleted = BTreeSet::new();
    for page in 0..32 {
        deleted.extend(
            sweep_page(&mut index, page, &t, i64::MIN, &mut seen)
                .unwrap()
                .deleted,
        );
    }
    let remaining = uris_on_pages(&index);
    for p in &posts {
        let by_u1 = p.author_uri.ends_with("/u1");
        assert_eq!(
            remaining.contains_key(&p.uri),
            !by_u1 && remaining.contains_key(&p.uri)
        );
        if by_u1 {
            assert!(!remaining.contains_key(&p.uri));
        }
    }
    assert!(!deleted.is_empty());
}

#[test]
fn opening_with_another_key_scheme_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p");
    drop(SocialIndex::<SocialPost>::create(&path, 4, PAGE_SIZE, KeyScheme::default()).unwrap());
    let mut other = KeyScheme::default();
    other.config.first_tokens = 3;
    assert!(SocialIndex::<SocialPost>::open(&path, other).is_err());
    assert!(SocialIndex::<SocialPost>::open(&path, KeyScheme::default()).is_ok());
    assert!(
        SocialIndex::<SocialPost>::open_or_create(&path, 4, PAGE_SIZE, KeyScheme::default())
            .is_ok()
    );
}
