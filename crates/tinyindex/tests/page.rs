//! Page format tests, ported from `mwmbl_core`'s `tests/index.rs`.

use std::fs::File;
use std::os::unix::fs::FileExt;

use serde_json::{Value, json};
use tinyindex::PageIoError;
use tinyindex::page::{pack_page, read_index_page, write_index_page};

const PAGE_SIZE: usize = 4096;
const TERM_POSITION: usize = 4;

/// A temporary file of zeros, removed with its directory when the test ends.
struct ScratchFile {
    _dir: tempfile::TempDir,
    file: File,
}

impl ScratchFile {
    fn with_pages(num_pages: usize) -> ScratchFile {
        let dir = tempfile::tempdir().unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.path().join("pages"))
            .unwrap();
        file.set_len((num_pages * PAGE_SIZE) as u64).unwrap();
        ScratchFile { _dir: dir, file }
    }
}

fn document(url: &str, term: Option<&str>) -> Value {
    json!(["A title", url, "An extract", 1.5, term, "page"])
}

fn read(scratch: &ScratchFile, page: u64, term: Option<&str>) -> Result<Vec<Value>, PageIoError> {
    read_index_page(
        &scratch.file,
        page * PAGE_SIZE as u64,
        PAGE_SIZE,
        term,
        TERM_POSITION,
    )
}

#[test]
fn a_written_page_reads_back_unchanged() {
    let scratch = ScratchFile::with_pages(2);
    let documents = vec![
        document("https://example.com/", Some("example")),
        json!(["Unicode ✓", "https://example.org/", null, 0.25]),
        json!({"not": ["a", "tuple"], "number": 12345678901234567890u64}),
    ];

    let num_stored =
        write_index_page(&scratch.file, PAGE_SIZE as u64, PAGE_SIZE, &documents).unwrap();

    assert_eq!(num_stored, documents.len());
    assert_eq!(read(&scratch, 1, None).unwrap(), documents);
}

#[test]
fn writing_a_page_leaves_its_neighbours_alone() {
    let scratch = ScratchFile::with_pages(3);
    let first = vec![document("https://first.example/", None)];
    let last = vec![document("https://last.example/", None)];
    write_index_page(&scratch.file, 0, PAGE_SIZE, &first).unwrap();
    write_index_page(&scratch.file, 2 * PAGE_SIZE as u64, PAGE_SIZE, &last).unwrap();

    write_index_page(
        &scratch.file,
        PAGE_SIZE as u64,
        PAGE_SIZE,
        &[document("https://middle.example/", None)],
    )
    .unwrap();

    assert_eq!(read(&scratch, 0, None).unwrap(), first);
    assert_eq!(read(&scratch, 2, None).unwrap(), last);
}

#[test]
fn a_term_keeps_its_own_documents_and_those_with_no_term() {
    let scratch = ScratchFile::with_pages(1);
    let own = document("https://own.example/", Some("rust"));
    let other = document("https://other.example/", Some("python"));
    let null_term = document("https://null.example/", None);
    // as_tuple truncates trailing Nones, so a document with no term can be this short.
    let truncated = json!(["A title", "https://truncated.example/", "An extract", 1.0]);
    let not_a_string = json!(["A title", "https://number.example/", "An extract", 1.0, 7]);
    let documents = vec![
        own.clone(),
        other,
        null_term.clone(),
        truncated.clone(),
        not_a_string,
    ];
    write_index_page(&scratch.file, 0, PAGE_SIZE, &documents).unwrap();

    let matching = read(&scratch, 0, Some("rust")).unwrap();

    assert_eq!(matching, vec![own, null_term, truncated]);
}

#[test]
fn packing_keeps_the_longest_prefix_that_fits() {
    // Distinct pseudo-random extracts, so the documents do not compress away to nothing.
    let documents: Vec<Value> = (0..500u64)
        .map(|i| {
            let extract: String = (0..20)
                .map(|j| format!("{:x}", (i * 7919 + j * 104_729).wrapping_mul(2_654_435_761)))
                .collect();
            json!(["Title", format!("https://example.com/{i}"), extract, 1.0])
        })
        .collect();

    let (page, num_stored) = pack_page(PAGE_SIZE, &documents).unwrap();

    assert_eq!(page.len(), PAGE_SIZE);
    assert!(
        num_stored > 0 && num_stored < documents.len(),
        "stored {num_stored}"
    );
    let scratch = ScratchFile::with_pages(1);
    scratch.file.write_all_at(&page, 0).unwrap();
    assert_eq!(read(&scratch, 0, None).unwrap(), documents[..num_stored]);
    let (_, one_more) = pack_page(PAGE_SIZE, &documents[..=num_stored]).unwrap();
    assert_eq!(
        one_more, num_stored,
        "the next document should not have fitted"
    );
}

#[test]
fn an_empty_list_packs_to_an_empty_page() {
    let (page, num_stored) = pack_page(PAGE_SIZE, &[]).unwrap();

    assert_eq!(num_stored, 0);
    assert_eq!(page.len(), PAGE_SIZE);
}

#[test]
fn a_page_too_small_for_even_an_empty_list_is_an_error() {
    let result = pack_page(4, &[document("https://example.com/", None)]);

    assert!(
        matches!(result, Err(PageIoError::Unreadable(_))),
        "{result:?}"
    );
}

#[test]
fn a_page_of_zeros_is_unreadable_rather_than_empty() {
    let scratch = ScratchFile::with_pages(1);

    let result = read(&scratch, 0, None);

    assert!(
        matches!(result, Err(PageIoError::Unreadable(_))),
        "{result:?}"
    );
}

#[test]
fn a_page_past_the_end_of_the_file_is_unreadable() {
    let scratch = ScratchFile::with_pages(1);
    write_index_page(
        &scratch.file,
        0,
        PAGE_SIZE,
        &[document("https://example.com/", None)],
    )
    .unwrap();
    scratch.file.set_len((PAGE_SIZE / 2) as u64).unwrap();

    let result = read(&scratch, 0, None);

    assert!(
        matches!(result, Err(PageIoError::Unreadable(_))),
        "{result:?}"
    );
}

#[test]
fn a_frame_claiming_too_much_content_is_refused_before_decompressing() {
    // A megabyte of one byte compresses to a few dozen, so it fits on a small page while
    // its header claims far more than MAX_PAGE_EXPANSION times the page size.
    let page_size = 256;
    let mut page = zstd::bulk::compress(&vec![b' '; 1 << 20], 3).unwrap();
    assert!(page.len() < page_size);
    page.resize(page_size, 0);
    let scratch = ScratchFile::with_pages(1);
    scratch.file.write_all_at(&page, 0).unwrap();

    let result = read_index_page(&scratch.file, 0, page_size, None, TERM_POSITION);

    let Err(PageIoError::Unreadable(message)) = result else {
        panic!("expected Unreadable, got {result:?}");
    };
    assert!(message.contains("more than"), "{message}");
}

#[test]
fn a_page_that_is_not_json_is_unreadable() {
    let mut page = zstd::bulk::compress(b"[not json", 3).unwrap();
    page.resize(PAGE_SIZE, 0);
    let scratch = ScratchFile::with_pages(1);
    scratch.file.write_all_at(&page, 0).unwrap();

    let result = read(&scratch, 0, None);

    assert!(
        matches!(result, Err(PageIoError::Unreadable(_))),
        "{result:?}"
    );
}

#[test]
fn a_failed_read_is_an_io_error() {
    // Reading a directory fails with EISDIR: a real syscall failure, not a bad page.
    let directory = File::open(std::env::temp_dir()).unwrap();

    let result = read_index_page(&directory, 0, PAGE_SIZE, None, TERM_POSITION);

    assert!(matches!(result, Err(PageIoError::Io(_))), "{result:?}");
}
