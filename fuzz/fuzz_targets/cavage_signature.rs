//! The draft-cavage `Signature` header parser, plus `inbox::signer` and full cavage verification
//! on arbitrary header sets (first line is the target URI, the rest are `name: value` headers).
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

use std::sync::OnceLock;

use apub::signatures::cavage;
use apub::signatures::keys::{RsaSigningKey, RsaVerifyingKey};
use libfuzzer_sys::fuzz_target;

#[path = "common.rs"]
mod common;

fn key() -> Option<&'static RsaVerifyingKey> {
    static KEY: OnceLock<Option<RsaVerifyingKey>> = OnceLock::new();
    KEY.get_or_init(|| RsaSigningKey::generate().ok().map(|k| k.verifying_key()))
        .as_ref()
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = cavage::parse_signature_header(text);

    let (target, rest) = text.split_once('\n').unwrap_or((text, ""));
    let headers = common::headers(rest);
    let _ = apub::inbox::signer(&headers);
    if let Some(key) = key() {
        let _ = cavage::verify_request(
            "POST",
            target,
            &headers,
            &["(request-target)", "host", "digest"],
            |_| Some(key.clone()),
            1_700_000_000,
            3600,
        );
    }
});
