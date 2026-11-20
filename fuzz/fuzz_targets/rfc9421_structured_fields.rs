//! RFC 9421 `Signature-Input` / `Signature` structured-field parsing (the RFC 8941 parser in
//! `apub::signatures::sf` is private, so it is reached through these), plus full verification,
//! which builds the signature base, on arbitrary header sets. The raw input is fed to both
//! parsers; then its first line is the target URI and the rest are `name: value` headers.
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

use apub::signatures::keys::{Ed25519SigningKey, VerifyingKey};
use apub::signatures::rfc9421::{self, VerifyPolicy};
use libfuzzer_sys::fuzz_target;

#[path = "common.rs"]
mod common;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(inputs) = rfc9421::parse_signature_input(text) {
        for (label, _, _) in &inputs {
            let _ = rfc9421::parse_signature(text, label);
        }
    }
    let _ = rfc9421::parse_signature(text, "sig1");

    let (target, rest) = text.split_once('\n').unwrap_or((text, ""));
    let headers = common::headers(rest);
    let _ = apub::inbox::signer(&headers);
    let key = VerifyingKey::Ed25519(Ed25519SigningKey::from_seed(&[7; 32]).verifying_key());
    let policy = VerifyPolicy::new(&["@method"], 1_700_000_000, 3600);
    let _ = rfc9421::verify_request("POST", target, &headers, &policy, |_| Some(key.clone()));
    let _ = rfc9421::verify_response(200, &headers, &policy, |_| Some(key.clone()));
    let _ = rfc9421::verify_response_with_key(200, &headers, &policy, &key);
});
