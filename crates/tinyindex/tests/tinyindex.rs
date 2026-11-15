//! Acceptance tests for the generic index, written before the implementation.

use std::fs;

use serde_json::{Value, json};
use tinyindex::{Document, IndexError, IndexItem, IndexWriter, TinyIndex, hash};

const PAGE_SIZE: usize = 4096;

fn doc(url: &str, term: &str, score: f64) -> Document {
    Document {
        title: format!("title {url}"),
        url: url.to_string(),
        extract: "extract".to_string(),
        score: Some(score),
        term: Some(term.to_string()),
        state: None,
        user_ids: None,
        last_crawled: None,
        source: None,
    }
}

fn new_index(dir: &tempfile::TempDir, num_pages: usize) -> IndexWriter<Document> {
    TinyIndex::<Document>::create(
        &dir.path().join("test.tinyindex"),
        num_pages,
        PAGE_SIZE,
        None,
    )
    .unwrap()
}

#[test]
fn mmh3_matches_python() {
    let vectors: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/mmh3_vectors.json")).unwrap();
    assert!(vectors.len() > 10);
    for v in vectors {
        let key = v["key"].as_str().unwrap();
        assert_eq!(
            u64::from(hash::mmh3_32(key.as_bytes())),
            v["hash"].as_u64().unwrap(),
            "{key}"
        );
        assert_eq!(
            hash::page_for_key(key, 65536) as u64,
            v["page_65536"].as_u64().unwrap()
        );
        assert_eq!(
            hash::page_for_key(key, 1000) as u64,
            v["page_1000"].as_u64().unwrap()
        );
    }
}

#[test]
fn created_index_has_the_right_shape_and_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tinyindex");
    let writer = TinyIndex::<Document>::create(&path, 7, PAGE_SIZE, Some(3)).unwrap();
    drop(writer);
    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        (4096 + 7 * PAGE_SIZE) as u64
    );
    let index = TinyIndex::<Document>::open(&path).unwrap();
    let m = index.metadata();
    assert_eq!((m.version, m.page_size, m.num_pages), (1, PAGE_SIZE, 7));
    assert_eq!(m.item_factory, "Document");
    assert_eq!(m.tokenizer_version, Some(3));
    for page in 0..7 {
        assert!(index.read_page(page).unwrap().is_empty());
    }
    assert!(matches!(
        TinyIndex::<Document>::create(&path, 7, PAGE_SIZE, None),
        Err(IndexError::Exists { .. })
    ));
}

#[test]
fn metadata_without_tokenizer_version_is_what_python_writes() {
    // Mwmbl's Python builds TinyIndexMetadata(**json), so an extra key would break it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tinyindex");
    drop(TinyIndex::<Document>::create(&path, 3, PAGE_SIZE, None).unwrap());
    let bytes = fs::read(&path).unwrap();
    let header = &bytes[..4096];
    let end = header.iter().position(|b| *b == 0).unwrap();
    let text = std::str::from_utf8(&header[..end]).unwrap();
    let json: Value =
        serde_json::from_str(text.strip_prefix("mwmbl-tiny-search").unwrap()).unwrap();
    assert_eq!(
        json,
        json!({"version": 1, "page_size": 4096, "num_pages": 3, "item_factory": "Document"})
    );
}

#[test]
fn opens_an_index_whose_metadata_python_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("py.tinyindex");
    let mut bytes = include_bytes!("fixtures/python_metadata.bin").to_vec();
    let page = include_bytes!("fixtures/python_document_page.bin");
    for _ in 0..3 {
        bytes.extend_from_slice(page);
    }
    fs::write(&path, bytes).unwrap();
    let index = TinyIndex::<Document>::open(&path).unwrap();
    assert_eq!(index.metadata().tokenizer_version, None);
    assert_eq!(index.read_page(2).unwrap().len(), 5);
}

#[test]
fn a_page_python_wrote_reads_back_identically() {
    let expected: Vec<Vec<Value>> =
        serde_json::from_str(include_str!("fixtures/python_document_page.json")).unwrap();
    let docs = tinyindex::page::decode_items::<Document>(
        include_bytes!("fixtures/python_document_page.bin"),
        None,
    )
    .unwrap();
    assert_eq!(docs.len(), expected.len());
    for (doc, tuple) in docs.iter().zip(&expected) {
        assert_eq!(&doc.to_tuple(), tuple, "{doc:?}");
    }
    assert_eq!(docs[2].title, "");
    assert_eq!(docs[2].score, None);
    assert_eq!(docs[1].user_ids, Some(vec![1, 2]));
    // Filtering by term keeps that term's documents and the one with no term.
    let rust = tinyindex::page::decode_items::<Document>(
        include_bytes!("fixtures/python_document_page.bin"),
        Some("rust"),
    )
    .unwrap();
    let urls: Vec<_> = rust.iter().map(|d| d.url.as_str()).collect();
    assert_eq!(
        urls,
        [
            "https://www.rust-lang.org/",
            "https://example.com/a",
            "https://example.com/b",
            "https://example.com/d"
        ]
    );
}

#[test]
fn opening_with_the_wrong_item_type_is_refused() {
    struct Other;
    impl IndexItem for Other {
        const ITEM_FACTORY: &'static str = "Other";
        const TERM_POSITION: usize = 0;
        fn url(&self) -> &str {
            ""
        }
        fn score(&self) -> f64 {
            0.0
        }
        fn term(&self) -> Option<&str> {
            None
        }
        fn to_tuple(&self) -> Vec<Value> {
            vec![]
        }
        fn from_tuple(_: Vec<Value>) -> Result<Self, tinyindex::PageIoError> {
            Ok(Other)
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tinyindex");
    drop(TinyIndex::<Document>::create(&path, 2, PAGE_SIZE, None).unwrap());
    assert!(matches!(
        TinyIndex::<Other>::open(&path),
        Err(IndexError::ItemFactory { .. })
    ));
}

#[test]
fn a_truncated_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tinyindex");
    drop(TinyIndex::<Document>::create(&path, 4, PAGE_SIZE, None).unwrap());
    let f = fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.set_len((4096 + 3 * PAGE_SIZE) as u64).unwrap();
    assert!(TinyIndex::<Document>::open(&path).is_err());
}

#[test]
fn add_then_retrieve_by_term() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = new_index(&dir, 16);
    w.add(vec![
        doc("https://a/", "rust", 1.0),
        doc("https://b/", "rust", 3.0),
        doc("https://c/", "go", 2.0),
    ])
    .unwrap();
    let r = w.reader();
    let urls: Vec<_> = r
        .retrieve("rust")
        .unwrap()
        .into_iter()
        .map(|d| d.url)
        .collect();
    assert_eq!(urls, ["https://b/", "https://a/"], "best first");
    assert_eq!(r.retrieve("go").unwrap().len(), 1);
    assert!(r.retrieve("python").unwrap().is_empty());
}

#[test]
fn add_merges_with_what_is_on_the_page_and_dedupes_by_url_and_term() {
    let dir = tempfile::tempdir().unwrap();
    // One page, so every term shares it.
    let mut w = new_index(&dir, 1);
    w.add(vec![
        doc("https://a/", "rust", 1.0),
        doc("https://a/", "go", 1.0),
    ])
    .unwrap();
    w.add(vec![
        doc("https://a/", "rust", 5.0),
        doc("https://b/", "rust", 2.0),
    ])
    .unwrap();
    let r = w.reader();
    let rust = r.retrieve("rust").unwrap();
    assert_eq!(rust.len(), 2);
    assert_eq!(
        (rust[0].url.as_str(), rust[0].score),
        ("https://a/", Some(5.0)),
        "newer copy wins"
    );
    assert_eq!(
        r.retrieve("go").unwrap().len(),
        1,
        "same url under another term is kept"
    );
    let page = r.read_page(0).unwrap();
    let scores: Vec<_> = page.iter().map(|d| d.score.unwrap()).collect();
    assert_eq!(scores, [5.0, 2.0, 1.0], "whole page kept best first");
}

#[test]
fn a_full_page_drops_the_lowest_scores() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = new_index(&dir, 1);
    // Incompressible extracts so that only a few fit on a page.
    let docs: Vec<Document> = (0..200_i32)
        .map(|i| {
            let mut d = doc(&format!("https://x/{i}"), "t", f64::from(i));
            // Pseudo-random hex from an LCG, so zstd cannot squash it.
            let mut state = u64::from(i.unsigned_abs()) * 7919 + 1;
            d.extract = (0..100)
                .map(|_| {
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    format!("{:x}", state >> 40)
                })
                .collect();
            d
        })
        .collect();
    let stats = w.add(docs).unwrap();
    assert!(stats.dropped > 0, "{stats:?}");
    let page = w.reader().read_page(0).unwrap();
    assert_eq!(page.len(), stats.stored);
    assert_eq!(page[0].score, Some(199.0));
    let lowest = page.last().unwrap().score.unwrap();
    assert!(lowest >= f64::from(200 - u32::try_from(page.len()).unwrap()));
}

#[test]
fn add_refuses_an_item_with_no_term() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = new_index(&dir, 4);
    let mut d = doc("https://a/", "x", 1.0);
    d.term = None;
    assert!(matches!(w.add(vec![d]), Err(IndexError::NoTerm { .. })));
}

#[test]
fn remove_takes_the_url_off_the_given_terms_pages_only() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = new_index(&dir, 64);
    w.add(vec![
        doc("https://a/", "one", 1.0),
        doc("https://a/", "two", 1.0),
        doc("https://a/", "three", 1.0),
        doc("https://b/", "one", 1.0),
    ])
    .unwrap();
    let removed = w.remove("https://a/", &["one", "two"]).unwrap();
    assert_eq!(removed, 2);
    let r = w.reader();
    assert_eq!(r.retrieve("one").unwrap().len(), 1);
    assert!(r.retrieve("two").unwrap().is_empty());
    assert_eq!(
        r.retrieve("three").unwrap().len(),
        1,
        "not asked to remove from this term's page"
    );
    assert_eq!(w.remove("https://zzz/", &["one"]).unwrap(), 0);
}

#[test]
fn remove_shares_a_page_between_terms() {
    // Two terms on one page: removing under one term clears every copy of the url there.
    let dir = tempfile::tempdir().unwrap();
    let mut w = new_index(&dir, 1);
    w.add(vec![
        doc("https://a/", "one", 1.0),
        doc("https://a/", "two", 1.0),
    ])
    .unwrap();
    assert_eq!(w.remove("https://a/", &["one", "two"]).unwrap(), 2);
    assert!(w.reader().read_page(0).unwrap().is_empty());
}

#[test]
fn write_page_and_page_iteration() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = new_index(&dir, 3);
    let stored = w.write_page(1, &[doc("https://a/", "x", 1.0)]).unwrap();
    assert_eq!(stored, 1);
    let r = w.reader();
    let counts: Vec<_> = r.pages().map(|(i, p)| (i, p.unwrap().len())).collect();
    assert_eq!(counts, [(0, 0), (1, 1), (2, 0)]);
    assert!(r.read_page(3).is_err());
}

#[test]
fn a_corrupt_page_is_an_error_not_an_empty_page_and_can_be_reset() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tinyindex");
    let mut w = TinyIndex::<Document>::create(&path, 2, PAGE_SIZE, None).unwrap();
    w.add(vec![doc("https://a/", "x", 1.0)]).unwrap();
    let page = hash::page_for_key("x", 2);
    {
        use std::os::unix::fs::FileExt;
        let f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.write_all_at(&[0xff; 64], (4096 + page * PAGE_SIZE) as u64)
            .unwrap();
    }
    assert!(matches!(
        w.reader().read_page(page),
        Err(IndexError::Page { .. })
    ));
    assert!(
        w.add(vec![doc("https://b/", "x", 1.0)]).is_err(),
        "never merge onto a page it can't read"
    );
    w.reset_page(page).unwrap();
    assert!(w.reader().read_page(page).unwrap().is_empty());
}

#[test]
fn readers_see_writes_and_are_shareable_across_threads() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = new_index(&dir, 8);
    let r = w.reader();
    w.add(vec![doc("https://a/", "x", 1.0)]).unwrap();
    let handle = std::thread::spawn(move || r.retrieve("x").unwrap().len());
    assert_eq!(handle.join().unwrap(), 1);
    // A reader opened separately from the path sees the same data.
    let path = dir.path().join("test.tinyindex");
    assert_eq!(
        TinyIndex::<Document>::open(&path)
            .unwrap()
            .retrieve("x")
            .unwrap()
            .len(),
        1
    );
}
