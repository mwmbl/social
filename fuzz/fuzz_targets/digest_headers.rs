//! `Digest` (legacy) and `Content-Digest` (RFC 9530) header parsing. Input: the header value,
//! then an optional NUL byte and the body.
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

use apub::signatures::integrity::{verify_content_digest, verify_legacy_digest};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let (header, body) = match data.iter().position(|&b| b == 0) {
        Some(at) => (
            data.get(..at).unwrap_or_default(),
            data.get(at + 1..).unwrap_or_default(),
        ),
        None => (data, &[][..]),
    };
    let Ok(header) = std::str::from_utf8(header) else {
        return;
    };
    let _ = verify_content_digest(header, body);
    let _ = verify_legacy_digest(header, body);
});
