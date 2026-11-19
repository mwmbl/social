//! tinyindex page decoding: the raw bytes as a page (a zstd frame inside), and the same bytes
//! zstd-compressed and zero-padded like a real page, so JSON and tuple decoding are reached
//! without the fuzzer having to find valid zstd. Decoded as generic tuples, `Document`,
//! `SocialPost` and `SocialAccount`. Also the index metadata header.
#![no_main]
#![deny(
    clippy::exit,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::panic,
    clippy::string_slice,
    clippy::unimplemented,
    clippy::unreachable,
    clippy::todo,
    clippy::pedantic
)]

use libfuzzer_sys::fuzz_target;
use social_index::{SocialAccount, SocialPost};
use tinyindex::page::{decode_items, decode_page};
use tinyindex::{Document, IndexMetadata};

fn decode_all(page: &[u8]) {
    let _ = decode_page(page, None, 4);
    let _ = decode_page(page, Some("term"), 4);
    let _ = decode_items::<Document>(page, None);
    let _ = decode_items::<SocialPost>(page, None);
    let _ = decode_items::<SocialAccount>(page, Some("term"));
}

fuzz_target!(|data: &[u8]| {
    let _ = IndexMetadata::from_bytes(data);
    decode_all(data);
    if let Ok(mut compressed) = zstd::bulk::compress(data, 1) {
        compressed.resize(compressed.len().max(4096), 0);
        decode_all(&compressed);
    }
});
