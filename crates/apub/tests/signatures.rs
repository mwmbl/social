//! Known-answer and behaviour tests for `apub::signatures`.
//!
//! Vectors come from RFC 9421 Appendix B, RFC 9530, the FASP protocol basics example and
//! draft-cavage-http-signatures-12 Appendix C (whose own header calls them "old and possibly
//! wrong"; they still match, so they are kept). Keys are in `tests/fixtures/`.

use std::sync::OnceLock;

use apub::signatures::cavage;
use apub::signatures::integrity::{
    legacy_digest, verify_content_digest_header, verify_legacy_digest,
};
use apub::signatures::keys::{
    Ed25519SigningKey, Ed25519VerifyingKey, RsaSigningKey, RsaVerifyingKey, SigningKey,
    VerifyingKey,
};
use apub::signatures::rfc9421::{
    self, Message, SignatureParams, VerifyPolicy, parse_signature, parse_signature_input,
    signature_base, signature_params_value,
};
use apub::signatures::{
    FnHeaders, SignatureError, authority, content_digest, http_date, parse_http_date,
    verify_content_digest,
};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).unwrap()
}

fn hdrs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Digests

const HELLO: &[u8] = b"{\"hello\": \"world\"}\n";
const HELLO_SHA256: &str = "RK/0qy18MlBSVnWgjwz6lZEWjP/lF5HF9bvEF8FabDg=";

#[test]
fn content_digest_known_answer_rfc9530() {
    // RFC 9530 section 6: the JSON object `{"hello": "world"}` followed by an LF.
    assert_eq!(content_digest(HELLO), format!("sha-256=:{HELLO_SHA256}:"));
    verify_content_digest(&content_digest(HELLO), HELLO).unwrap();
}

#[test]
fn content_digest_verification_rules() {
    let good = format!("sha-256=:{HELLO_SHA256}:");
    // Other algorithms are ignored, in either position.
    verify_content_digest(&format!("sha-512=:AAAA:, {good}"), HELLO).unwrap();
    verify_content_digest(&format!("{good}, unixsum=:AAAA:"), HELLO).unwrap();
    // Without the trailing LF the digest differs.
    assert_eq!(
        verify_content_digest(&good, b"{\"hello\": \"world\"}"),
        Err(SignatureError::DigestMismatch)
    );
    // No sha-256 member at all.
    assert_eq!(
        verify_content_digest("sha-512=:AAAA:", HELLO),
        Err(SignatureError::NoSha256Digest)
    );
    assert_eq!(
        verify_content_digest("", HELLO),
        Err(SignatureError::NoSha256Digest)
    );
    // Malformed dictionaries and member types.
    for bad in [
        "sha-256",
        "sha-256=",
        "sha-256=:not base64!:",
        "sha-256=:AAAA",
        "sha-256=\"RK/0\"",
        "sha-256=(:AAAA:)",
        "sha-256=:AAAA:,",
        "SHA-256=:AAAA:",
    ] {
        assert!(
            verify_content_digest(bad, HELLO).is_err(),
            "accepted `{bad}`"
        );
    }
}

#[test]
fn content_digest_header_lookup_combines_instances() {
    let headers = hdrs(&[
        ("Content-Digest", "sha-512=:AAAA:"),
        ("content-digest", &format!("sha-256=:{HELLO_SHA256}:")),
    ]);
    verify_content_digest_header(&headers, HELLO).unwrap();
    assert_eq!(
        verify_content_digest_header(&hdrs(&[]), HELLO),
        Err(SignatureError::MissingHeader {
            name: "content-digest".to_owned()
        })
    );
}

#[test]
fn legacy_digest_known_answer_cavage12() {
    // draft-cavage-12 Appendix C: body `{"hello": "world"}` without a trailing newline.
    let body = b"{\"hello\": \"world\"}";
    let expected = "SHA-256=X48E9qOokqqrvdts8nOJRJN3OWDUoyWxBf7kbu9DBPE=";
    assert_eq!(legacy_digest(body), expected);
    verify_legacy_digest(expected, body).unwrap();
    verify_legacy_digest(&format!("sha-512=AAAA,{expected}"), body).unwrap();
    // The algorithm name is case-insensitive.
    verify_legacy_digest(&expected.replacen("SHA-256", "sha-256", 1), body).unwrap();
    assert_eq!(
        verify_legacy_digest(expected, b"other"),
        Err(SignatureError::DigestMismatch)
    );
    assert_eq!(
        verify_legacy_digest("SHA-512=AAAA", body),
        Err(SignatureError::NoSha256Digest)
    );
    assert!(verify_legacy_digest("SHA-256", body).is_err());
    assert!(verify_legacy_digest("SHA-256=***", body).is_err());
}

// ---------------------------------------------------------------------------------------------
// RFC 9421 Appendix B

const REQ_URI: &str = "https://example.com/foo?param=Value&Pet=dog";
const REQ_DATE: &str = "Tue, 20 Apr 2021 02:07:55 GMT";
const REQ_CONTENT_DIGEST: &str = "sha-512=:WZDPaVn/7XgHaAy8pmojAkGWoRx2UFChF41A2svX+TaPm+AbwAgBWnrIiYllu7BNNyealdVLvRwEmTHWXvJwew==:";
const CREATED: i64 = 1_618_884_473;

fn b2_request_headers() -> Vec<(String, String)> {
    hdrs(&[
        ("Host", "example.com"),
        ("Date", REQ_DATE),
        ("Content-Type", "application/json"),
        ("Content-Digest", REQ_CONTENT_DIGEST),
        ("Content-Length", "18"),
    ])
}

fn b2_response_headers() -> Vec<(String, String)> {
    hdrs(&[
        ("Date", "Tue, 20 Apr 2021 02:07:56 GMT"),
        ("Content-Type", "application/json"),
        (
            "Content-Digest",
            "sha-512=:mEWXIS7MaLRuGgxOBdODa3xqM1XdEvxoYhvlCFJ41QJgJc4GTsPp29l5oGX69wWdXymyU0rjJuahq4l5aGgfLQ==:",
        ),
        ("Content-Length", "23"),
    ])
}

fn params(keyid: &str) -> SignatureParams {
    SignatureParams {
        created: Some(CREATED),
        keyid: Some(keyid.to_owned()),
        ..SignatureParams::default()
    }
}

fn request() -> Message<'static> {
    Message::Request {
        method: "POST",
        target_uri: REQ_URI,
    }
}

fn ed25519_test_seed() -> [u8; 32] {
    // B.1.4, JWK "d".
    URL_SAFE_NO_PAD
        .decode("n4Ni-HpISpVObnQMW0wOhCKROaIKqKtW_2ZYb2p9KcU")
        .unwrap()
        .try_into()
        .unwrap()
}

#[test]
fn b26_ed25519_signature_base_and_exact_signature() {
    let components = [
        "date",
        "@method",
        "@path",
        "@authority",
        "content-type",
        "content-length",
    ];
    let headers = b2_request_headers();
    let p = params("test-key-ed25519");

    let base = signature_base(&request(), &headers, &components, &p).unwrap();
    assert_eq!(
        base,
        "\"date\": Tue, 20 Apr 2021 02:07:55 GMT\n\
         \"@method\": POST\n\
         \"@path\": /foo\n\
         \"@authority\": example.com\n\
         \"content-type\": application/json\n\
         \"content-length\": 18\n\
         \"@signature-params\": (\"date\" \"@method\" \"@path\" \"@authority\" \"content-type\" \"content-length\");created=1618884473;keyid=\"test-key-ed25519\""
    );

    let key = SigningKey::Ed25519(Ed25519SigningKey::from_seed(&ed25519_test_seed()));
    let (input, signature) =
        rfc9421::sign(&request(), &headers, "sig-b26", &components, &p, &key).unwrap();
    assert_eq!(
        input,
        "sig-b26=(\"date\" \"@method\" \"@path\" \"@authority\" \"content-type\" \"content-length\");created=1618884473;keyid=\"test-key-ed25519\""
    );
    assert_eq!(
        signature,
        "sig-b26=:wqcAqbmYJ2ji2glfAMaRy4gruYYnx2nEFN2HN6jrnDnQCK1u02Gb04v9EDgwUPiu4A0w6vuQv5lIp5WPpBKRCw==:"
    );
}

#[test]
fn b26_ed25519_verifies_the_rfc_headers() {
    let mut headers = b2_request_headers();
    headers.push((
        "Signature-Input".to_owned(),
        "sig-b26=(\"date\" \"@method\" \"@path\" \"@authority\" \"content-type\" \"content-length\");created=1618884473;keyid=\"test-key-ed25519\"".to_owned(),
    ));
    headers.push((
        "Signature".to_owned(),
        "sig-b26=:wqcAqbmYJ2ji2glfAMaRy4gruYYnx2nEFN2HN6jrnDnQCK1u02Gb04v9EDgwUPiu4A0w6vuQv5lIp5WPpBKRCw==:".to_owned(),
    ));
    let public = Ed25519SigningKey::from_seed(&ed25519_test_seed()).verifying_key();
    // JWK "x" of B.1.4 is the public key.
    assert_eq!(
        public.to_bytes().as_slice(),
        URL_SAFE_NO_PAD
            .decode("JrQLj5P_89iXES9-vFgrIy29clF9CC_oPPsw3c5D0bs")
            .unwrap()
            .as_slice()
    );
    let policy = VerifyPolicy {
        label: "sig-b26",
        required_components: &["@method", "@authority"],
        now: CREATED + 5,
        max_skew_secs: 60,
    };
    let verified = rfc9421::verify_request("POST", REQ_URI, &headers, &policy, |keyid| {
        (keyid == "test-key-ed25519").then(|| VerifyingKey::Ed25519(public.clone()))
    })
    .unwrap();
    assert_eq!(verified.keyid, "test-key-ed25519");
    assert_eq!(verified.created, CREATED);
    assert_eq!(
        verified.components,
        [
            "date",
            "@method",
            "@path",
            "@authority",
            "content-type",
            "content-length"
        ]
    );
}

fn pss_key() -> VerifyingKey {
    VerifyingKey::RsaPssSha512(
        RsaVerifyingKey::from_pem(&fixture("rfc9421_rsa_pss_public.pem")).unwrap(),
    )
}

#[test]
fn b21_minimal_rsa_pss_base_and_verify() {
    let p = SignatureParams {
        created: Some(CREATED),
        keyid: Some("test-key-rsa-pss".to_owned()),
        nonce: Some("b3k2pp5k7z-50gnwp.yemd".to_owned()),
        ..SignatureParams::default()
    };
    let headers = b2_request_headers();
    assert_eq!(
        signature_base(&request(), &headers, &[], &p).unwrap(),
        "\"@signature-params\": ();created=1618884473;keyid=\"test-key-rsa-pss\";nonce=\"b3k2pp5k7z-50gnwp.yemd\""
    );

    let mut with_sig = headers.clone();
    with_sig.push((
        "Signature-Input".to_owned(),
        "sig-b21=();created=1618884473;keyid=\"test-key-rsa-pss\";nonce=\"b3k2pp5k7z-50gnwp.yemd\""
            .to_owned(),
    ));
    with_sig.push((
        "Signature".to_owned(),
        "sig-b21=:d2pmTvmbncD3xQm8E9ZV2828BjQWGgiwAaw5bAkgibUopemLJcWDy/lkbbHAve4cRAtx31Iq786U7it++wgGxbtRxf8Udx7zFZsckzXaJMkA7ChG52eSkFxykJeNqsrWH5S+oxNFlD4dzVuwe8DhTSja8xxbR/Z2cOGdCbzR72rgFWhzx2VjBqJzsPLMIQKhO4DGezXehhWwE56YCE+O6c0mKZsfxVrogUvA4HELjVKWmAvtl6UnCh8jYzuVG5WSb/QEVPnP5TmcAnLH1g+s++v6d4s8m0gCw1fV5/SITLq9mhho8K3+7EPYTU8IU1bLhdxO5Nyt8C8ssinQ98Xw9Q==:".to_owned(),
    ));
    let policy = VerifyPolicy {
        label: "sig-b21",
        required_components: &[],
        now: CREATED,
        max_skew_secs: 5,
    };
    let verified =
        rfc9421::verify_request("POST", REQ_URI, &with_sig, &policy, |_| Some(pss_key())).unwrap();
    assert_eq!(verified.nonce.as_deref(), Some("b3k2pp5k7z-50gnwp.yemd"));
    assert!(verified.components.is_empty());

    // The same signature must not verify against a different key.
    let other = VerifyingKey::RsaPssSha512(
        RsaVerifyingKey::from_pem(&fixture("cavage12_public.pem")).unwrap(),
    );
    assert!(matches!(
        rfc9421::verify_request("POST", REQ_URI, &with_sig, &policy, |_| Some(other.clone())),
        Err(SignatureError::BadSignature { .. })
    ));
}

#[test]
fn b22_uses_a_component_parameter_we_refuse() {
    // `"@query-param";name="Pet"` is outside the fixed component set. Check that it is refused
    // rather than silently mis-verified.
    let mut headers = b2_request_headers();
    headers.push((
        "Signature-Input".to_owned(),
        "sig-b22=(\"@authority\" \"content-digest\" \"@query-param\";name=\"Pet\");created=1618884473;keyid=\"test-key-rsa-pss\";tag=\"header-example\"".to_owned(),
    ));
    headers.push(("Signature".to_owned(), "sig-b22=:AAAA:".to_owned()));
    let policy = VerifyPolicy {
        label: "sig-b22",
        required_components: &[],
        now: CREATED,
        max_skew_secs: 5,
    };
    assert!(matches!(
        rfc9421::verify_request("POST", REQ_URI, &headers, &policy, |_| Some(pss_key())),
        Err(SignatureError::UnsupportedComponent { .. })
    ));
    assert!(matches!(
        signature_base(&request(), &headers, &["@query-param"], &params("k")),
        Err(SignatureError::UnsupportedComponent { .. })
    ));
    // The tag is still parsed from the header.
    let parsed = parse_signature_input(
        "sig-b22=(\"@authority\" \"content-digest\");created=1618884473;keyid=\"test-key-rsa-pss\";tag=\"header-example\"",
    )
    .unwrap();
    assert_eq!(
        parsed.first().unwrap().2.tag.as_deref(),
        Some("header-example")
    );
}

#[test]
fn b23_full_coverage_rsa_pss_base_and_verify() {
    let components = [
        "date",
        "@method",
        "@path",
        "@query",
        "@authority",
        "content-type",
        "content-digest",
        "content-length",
    ];
    let headers = b2_request_headers();
    let base = signature_base(
        &request(),
        &headers,
        &components,
        &params("test-key-rsa-pss"),
    )
    .unwrap();
    assert_eq!(
        base,
        format!(
            "\"date\": Tue, 20 Apr 2021 02:07:55 GMT\n\
             \"@method\": POST\n\
             \"@path\": /foo\n\
             \"@query\": ?param=Value&Pet=dog\n\
             \"@authority\": example.com\n\
             \"content-type\": application/json\n\
             \"content-digest\": {REQ_CONTENT_DIGEST}\n\
             \"content-length\": 18\n\
             \"@signature-params\": (\"date\" \"@method\" \"@path\" \"@query\" \"@authority\" \"content-type\" \"content-digest\" \"content-length\");created=1618884473;keyid=\"test-key-rsa-pss\""
        )
    );

    let mut with_sig = headers;
    with_sig.push((
        "Signature-Input".to_owned(),
        "sig-b23=(\"date\" \"@method\" \"@path\" \"@query\" \"@authority\" \"content-type\" \"content-digest\" \"content-length\");created=1618884473;keyid=\"test-key-rsa-pss\"".to_owned(),
    ));
    with_sig.push((
        "Signature".to_owned(),
        "sig-b23=:bbN8oArOxYoyylQQUU6QYwrTuaxLwjAC9fbY2F6SVWvh0yBiMIRGOnMYwZ/5MR6fb0Kh1rIRASVxFkeGt683+qRpRRU5p2voTp768ZrCUb38K0fUxN0O0iC59DzYx8DFll5GmydPxSmme9v6ULbMFkl+V5B1TP/yPViV7KsLNmvKiLJH1pFkh/aYA2HXXZzNBXmIkoQoLd7YfW91kE9o/CCoC1xMy7JA1ipwvKvfrs65ldmlu9bpG6A9BmzhuzF8Eim5f8ui9eH8LZH896+QIF61ka39VBrohr9iyMUJpvRX2Zbhl5ZJzSRxpJyoEZAFL2FUo5fTIztsDZKEgM4cUA==:".to_owned(),
    ));
    let policy = VerifyPolicy {
        label: "sig-b23",
        required_components: &["@method", "@query", "content-digest"],
        now: CREATED,
        max_skew_secs: 5,
    };
    rfc9421::verify_request("POST", REQ_URI, &with_sig, &policy, |_| Some(pss_key())).unwrap();
    // A different query string breaks the signature.
    assert!(matches!(
        rfc9421::verify_request(
            "POST",
            "https://example.com/foo?param=Value&Pet=cat",
            &with_sig,
            &policy,
            |_| Some(pss_key())
        ),
        Err(SignatureError::BadSignature { .. })
    ));
}

#[test]
fn b24_response_signature_base() {
    // The signature is ECDSA P-256, which is not supported; the base is still checked.
    let components = [
        "@status",
        "content-type",
        "content-digest",
        "content-length",
    ];
    let base = signature_base(
        &Message::Response { status: 200 },
        &b2_response_headers(),
        &components,
        &params("test-key-ecc-p256"),
    )
    .unwrap();
    assert_eq!(
        base,
        "\"@status\": 200\n\
         \"content-type\": application/json\n\
         \"content-digest\": sha-512=:mEWXIS7MaLRuGgxOBdODa3xqM1XdEvxoYhvlCFJ41QJgJc4GTsPp29l5oGX69wWdXymyU0rjJuahq4l5aGgfLQ==:\n\
         \"content-length\": 23\n\
         \"@signature-params\": (\"@status\" \"content-type\" \"content-digest\" \"content-length\");created=1618884473;keyid=\"test-key-ecc-p256\""
    );
}

#[test]
fn b25_hmac_signature_base() {
    // HMAC is not supported; the base is still checked.
    let base = signature_base(
        &request(),
        &b2_request_headers(),
        &["date", "@authority", "content-type"],
        &params("test-shared-secret"),
    )
    .unwrap();
    assert_eq!(
        base,
        "\"date\": Tue, 20 Apr 2021 02:07:55 GMT\n\
         \"@authority\": example.com\n\
         \"content-type\": application/json\n\
         \"@signature-params\": (\"date\" \"@authority\" \"content-type\");created=1618884473;keyid=\"test-shared-secret\""
    );
}

// ---------------------------------------------------------------------------------------------
// FASP

const FASP_URI: &str = "https://fedi.example.com/fasp/data_sharing/v0/event_subscriptions";

#[test]
fn fasp_example_component_order_and_params() {
    // protocol_basics.md: Signature-Input: sig1=("@method" "@target-uri" "content-digest");
    //   created=1728467285; keyid="b2ks6vm8p23w"   (the document wraps the line after `;`)
    // Content-Digest: sha-256=:RK/0qy18MlBSVnWgjwz6lZEWjP/lF5HF9bvEF8FabDg=:
    let digest = content_digest(HELLO);
    assert_eq!(
        digest,
        "sha-256=:RK/0qy18MlBSVnWgjwz6lZEWjP/lF5HF9bvEF8FabDg=:"
    );
    let headers = hdrs(&[("Content-Digest", &digest)]);
    let p = SignatureParams {
        created: Some(1_728_467_285),
        keyid: Some("b2ks6vm8p23w".to_owned()),
        ..SignatureParams::default()
    };
    let components = ["@method", "@target-uri", "content-digest"];
    assert_eq!(
        signature_params_value(&components, &p).unwrap(),
        "(\"@method\" \"@target-uri\" \"content-digest\");created=1728467285;keyid=\"b2ks6vm8p23w\""
    );
    let message = Message::Request {
        method: "POST",
        target_uri: FASP_URI,
    };
    assert_eq!(
        signature_base(&message, &headers, &components, &p).unwrap(),
        format!(
            "\"@method\": POST\n\
             \"@target-uri\": {FASP_URI}\n\
             \"content-digest\": {digest}\n\
             \"@signature-params\": (\"@method\" \"@target-uri\" \"content-digest\");created=1728467285;keyid=\"b2ks6vm8p23w\""
        )
    );
}

#[test]
fn fasp_request_and_response_round_trip() {
    let seed = Ed25519SigningKey::generate().unwrap();
    let public = seed.verifying_key();
    let key = SigningKey::Ed25519(seed);
    let now = 1_728_467_285;
    let lookup =
        |keyid: &str| (keyid == "b2ks6vm8p23w").then(|| VerifyingKey::Ed25519(public.clone()));

    let mut headers = hdrs(&[("Content-Digest", &content_digest(HELLO))]);
    let (input, signature) = rfc9421::sign_request(
        "POST",
        FASP_URI,
        &headers,
        &["@method", "@target-uri", "content-digest"],
        "b2ks6vm8p23w",
        now,
        &key,
    )
    .unwrap();
    assert!(input.starts_with("sig1=(\"@method\" \"@target-uri\" \"content-digest\");created="));
    headers.push(("Signature-Input".to_owned(), input));
    headers.push(("Signature".to_owned(), signature));
    let policy = VerifyPolicy::new(&["@method", "@target-uri", "content-digest"], now + 10, 300);
    let verified = rfc9421::verify_request("POST", FASP_URI, &headers, &policy, lookup).unwrap();
    assert_eq!(verified.keyid, "b2ks6vm8p23w");
    verify_content_digest_header(&headers, HELLO).unwrap();

    // Wrong method or URI: the signature no longer matches.
    assert!(matches!(
        rfc9421::verify_request("DELETE", FASP_URI, &headers, &policy, lookup),
        Err(SignatureError::BadSignature { .. })
    ));
    assert!(matches!(
        rfc9421::verify_request(
            "POST",
            "https://fedi.example.com/other",
            &headers,
            &policy,
            lookup
        ),
        Err(SignatureError::BadSignature { .. })
    ));

    // Responses cover @status and content-digest.
    let mut response = hdrs(&[("Content-Digest", &content_digest(b"{}"))]);
    let (input, signature) = rfc9421::sign_response(
        201,
        &response,
        &["@status", "content-digest"],
        "b2ks6vm8p23w",
        now,
        &key,
    )
    .unwrap();
    response.push(("Signature-Input".to_owned(), input));
    response.push(("Signature".to_owned(), signature));
    let policy = VerifyPolicy::new(&["@status", "content-digest"], now, 300);
    rfc9421::verify_response(201, &response, &policy, lookup).unwrap();
    assert!(matches!(
        rfc9421::verify_response(200, &response, &policy, lookup),
        Err(SignatureError::BadSignature { .. })
    ));
}

#[test]
fn verification_failures_say_what_failed() {
    let seed = Ed25519SigningKey::from_seed(&[7; 32]);
    let public = seed.verifying_key();
    let key = SigningKey::Ed25519(seed);
    let lookup = |keyid: &str| (keyid == "k1").then(|| VerifyingKey::Ed25519(public.clone()));
    let now = 1_700_000_000;

    let signed = |components: &[&str], created: i64| {
        let mut headers = hdrs(&[("Content-Digest", &content_digest(b"x"))]);
        let (i, s) =
            rfc9421::sign_request("POST", FASP_URI, &headers, components, "k1", created, &key)
                .unwrap();
        headers.push(("Signature-Input".to_owned(), i));
        headers.push(("Signature".to_owned(), s));
        headers
    };
    let all = ["@method", "@target-uri", "content-digest"];
    let policy = VerifyPolicy::new(&all, now, 300);
    let check = |headers: &Vec<(String, String)>, policy: &VerifyPolicy<'_>| {
        rfc9421::verify_request("POST", FASP_URI, headers, policy, lookup)
    };

    check(&signed(&all, now), &policy).unwrap();
    check(&signed(&all, now - 300), &policy).unwrap();
    check(&signed(&all, now + 300), &policy).unwrap();

    let err = check(&signed(&all, now - 301), &policy).unwrap_err();
    assert_eq!(
        err,
        SignatureError::TooOld {
            what: "created",
            seconds: 301,
            max: 300
        }
    );
    assert_eq!(
        err.to_string(),
        "created is 301 seconds old, more than the allowed 300"
    );
    let err = check(&signed(&all, now + 400), &policy).unwrap_err();
    assert_eq!(
        err,
        SignatureError::InFuture {
            what: "created",
            seconds: 400,
            max: 300
        }
    );

    // A signature that leaves out a required component.
    let err = check(&signed(&["@method", "content-digest"], now), &policy).unwrap_err();
    assert_eq!(
        err.to_string(),
        "component `@target-uri` is not covered by the signature"
    );

    // Unknown key id, missing headers, wrong label.
    let err = rfc9421::verify_request("POST", FASP_URI, &signed(&all, now), &policy, |_| None)
        .unwrap_err();
    assert_eq!(err.to_string(), "unknown keyid `k1`");
    let err = check(&hdrs(&[]), &policy).unwrap_err();
    assert_eq!(err.to_string(), "missing header `signature-input`");
    let other_label = VerifyPolicy {
        label: "sig9",
        ..policy
    };
    assert!(matches!(
        check(&signed(&all, now), &other_label),
        Err(SignatureError::LabelNotFound { .. })
    ));

    // Tampered signature bytes.
    let mut headers = signed(&all, now);
    let sig = headers.pop().unwrap().1;
    let mut bytes = parse_signature(&sig, "sig1").unwrap();
    bytes[0] ^= 1;
    headers.push((
        "Signature".to_owned(),
        format!("sig1=:{}:", STANDARD.encode(bytes)),
    ));
    assert_eq!(
        check(&headers, &policy).unwrap_err().to_string(),
        "bad signature for keyid `k1`"
    );

    // expires in the past, no created, no keyid, alg mismatch.
    let with_params = |p: SignatureParams| {
        let mut headers = hdrs(&[("Content-Digest", &content_digest(b"x"))]);
        let (i, s) = rfc9421::sign(
            &Message::Request {
                method: "POST",
                target_uri: FASP_URI,
            },
            &headers,
            "sig1",
            &all,
            &p,
            &key,
        )
        .unwrap();
        headers.push(("Signature-Input".to_owned(), i));
        headers.push(("Signature".to_owned(), s));
        headers
    };
    let base = SignatureParams {
        created: Some(now),
        keyid: Some("k1".to_owned()),
        ..SignatureParams::default()
    };
    assert_eq!(
        check(
            &with_params(SignatureParams {
                expires: Some(now - 5),
                ..base.clone()
            }),
            &policy
        ),
        Err(SignatureError::Expired { seconds: 5 })
    );
    check(
        &with_params(SignatureParams {
            expires: Some(now + 5),
            ..base.clone()
        }),
        &policy,
    )
    .unwrap();
    assert_eq!(
        check(
            &with_params(SignatureParams {
                created: None,
                ..base.clone()
            }),
            &policy
        ),
        Err(SignatureError::MissingCreated)
    );
    assert_eq!(
        check(
            &with_params(SignatureParams {
                keyid: None,
                ..base.clone()
            }),
            &policy
        ),
        Err(SignatureError::MissingKeyId)
    );
    assert!(matches!(
        check(
            &with_params(SignatureParams {
                alg: Some("rsa-pss-sha512".to_owned()),
                ..base.clone()
            }),
            &policy
        ),
        Err(SignatureError::AlgorithmMismatch { .. })
    ));
    let ok = check(
        &with_params(SignatureParams {
            alg: Some("ed25519".to_owned()),
            tag: Some("fasp".to_owned()),
            ..base
        }),
        &policy,
    )
    .unwrap();
    assert_eq!(ok.tag.as_deref(), Some("fasp"));
}

#[test]
fn signing_errors() {
    let key = SigningKey::Ed25519(Ed25519SigningKey::from_seed(&[1; 32]));
    let headers = hdrs(&[("date", "x")]);
    let sign = |label: &str, components: &[&str], keyid: &str| {
        rfc9421::sign(
            &request(),
            &headers,
            label,
            components,
            &params(keyid),
            &key,
        )
    };
    assert!(matches!(
        sign("Sig1", &["date"], "k"),
        Err(SignatureError::InvalidLabel { .. })
    ));
    assert!(matches!(
        sign("sig1", &["Date"], "k"),
        Err(SignatureError::UnsupportedComponent { .. })
    ));
    assert!(matches!(
        sign("sig1", &["@scheme"], "k"),
        Err(SignatureError::UnsupportedComponent { .. })
    ));
    assert!(matches!(
        sign("sig1", &["@status"], "k"),
        Err(SignatureError::UnsupportedComponent { .. })
    ));
    assert!(matches!(
        sign("sig1", &["date", "date"], "k"),
        Err(SignatureError::DuplicateComponent { .. })
    ));
    assert!(matches!(
        sign("sig1", &["content-digest"], "k"),
        Err(SignatureError::MissingHeader { .. })
    ));
    assert!(matches!(
        sign("sig1", &["date"], "k\u{e9}"),
        Err(SignatureError::InvalidValue { .. })
    ));
    // Quotes in a keyid are escaped.
    let (input, _) = sign("sig1", &["date"], "a\"b\\c").unwrap();
    assert!(input.ends_with("keyid=\"a\\\"b\\\\c\""), "{input}");
}

#[test]
fn derived_components() {
    let p = SignatureParams::default();
    let value = |uri: &str, component: &str| {
        let base = signature_base(
            &Message::Request {
                method: "GET",
                target_uri: uri,
            },
            &Vec::<(String, String)>::new(),
            &[component],
            &p,
        )
        .unwrap();
        base.lines()
            .next()
            .unwrap()
            .split_once(": ")
            .unwrap()
            .1
            .to_owned()
    };
    assert_eq!(
        value("https://Example.COM:443/a/b?x=1&y=", "@authority"),
        "example.com"
    );
    assert_eq!(value("http://example.com:80/", "@authority"), "example.com");
    assert_eq!(
        value("https://example.com:8443/", "@authority"),
        "example.com:8443"
    );
    assert_eq!(value("https://example.com", "@path"), "/");
    assert_eq!(value("https://example.com/a%2Fb", "@path"), "/a%2Fb");
    assert_eq!(value("https://example.com/a", "@query"), "?");
    assert_eq!(value("https://example.com/a?", "@query"), "?");
    assert_eq!(value("https://example.com/a?x=1&y=", "@query"), "?x=1&y=");
    assert_eq!(
        value("https://u:p@example.com/a#frag", "@authority"),
        "example.com"
    );
    assert_eq!(
        value("https://example.com/a?b#frag", "@target-uri"),
        "https://example.com/a?b#frag"
    );
    assert_eq!(
        authority("https://Mastodon.Example:443/users/a").unwrap(),
        "mastodon.example"
    );
    for bad in [
        "",
        "example.com/a",
        "ftp://example.com/",
        "https:///a",
        "https://exa mple.com/",
        "https://ex\u{e9}.com/",
    ] {
        assert!(authority(bad).is_err(), "accepted `{bad}`");
    }
}

#[test]
fn rsa_v15_round_trip_and_alg_check() {
    let private = owned_generated_rsa();
    let public = private.verifying_key();
    let key = SigningKey::RsaV15Sha256(private);
    assert_eq!(key.algorithm(), "rsa-v1_5-sha256");

    let mut headers = b2_request_headers();
    let p = SignatureParams {
        alg: Some("rsa-v1_5-sha256".to_owned()),
        ..params("test-key-rsa")
    };
    let components = ["@method", "@authority", "date", "content-digest"];
    let (i, s) = rfc9421::sign(&request(), &headers, "sig1", &components, &p, &key).unwrap();
    // RSASSA-PKCS1-v1_5 is deterministic.
    assert_eq!(
        s,
        rfc9421::sign(&request(), &headers, "sig1", &components, &p, &key)
            .unwrap()
            .1
    );
    headers.push(("Signature-Input".to_owned(), i));
    headers.push(("Signature".to_owned(), s));
    let policy = VerifyPolicy::new(&["@method", "content-digest"], CREATED, 5);
    let v = rfc9421::verify_request("POST", REQ_URI, &headers, &policy, |_| {
        Some(VerifyingKey::RsaV15Sha256(public.clone()))
    })
    .unwrap();
    assert_eq!(v.alg.as_deref(), Some("rsa-v1_5-sha256"));
    // The same RSA key registered as PSS fails on the declared alg before the signature.
    assert!(matches!(
        rfc9421::verify_request("POST", REQ_URI, &headers, &policy, |_| Some(
            VerifyingKey::RsaPssSha512(public.clone())
        )),
        Err(SignatureError::AlgorithmMismatch { .. })
    ));
}

#[test]
fn headers_can_come_from_a_closure() {
    let map = std::collections::HashMap::from([(
        "content-digest".to_owned(),
        vec![content_digest(HELLO)],
    )]);
    let headers = FnHeaders(|name: &str| map.get(name).cloned().unwrap_or_default());
    verify_content_digest_header(&headers, HELLO).unwrap();
}

// ---------------------------------------------------------------------------------------------
// Parser strictness

#[test]
fn signature_input_parsing() {
    let parsed = parse_signature_input(
        "sig1=(\"@method\" \"@target-uri\");created=1;keyid=\"a\", sig2=(\"date\");created=2;expires=3;alg=\"ed25519\";nonce=\"n\";tag=\"t\"",
    )
    .unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].0, "sig1");
    assert_eq!(parsed[0].1, ["@method", "@target-uri"]);
    assert_eq!(parsed[1].2.expires, Some(3));
    assert_eq!(parsed[1].2.alg.as_deref(), Some("ed25519"));
    assert!(parse_signature_input("").unwrap().is_empty());
    assert!(parse_signature_input("  sig1=()  ").is_ok());
    assert_eq!(parse_signature("sig1=:AAAA:", "sig1").unwrap(), [0, 0, 0]);
    // Missing base64 padding is accepted, as RFC 8941 recommends.
    assert_eq!(parse_signature("sig1=:AAA:", "sig1").unwrap(), [0, 0]);
}

const MALFORMED: &[&str] = &[
    "sig1",
    "sig1=",
    "sig1=(",
    "sig1=(\"a\"",
    "sig1=(\"a\"\"b\")",
    "sig1=(\"a\"b)",
    "sig1=(a)",
    "sig1=(\"a\") x",
    "sig1=(\"a\"),",
    "sig1=(\"a\"),,sig2=()",
    "sig1=\"unterminated",
    "sig1=\"bad\\escape\"",
    "sig1=:@@@:",
    "sig1=:AAAA",
    "sig1=();created=",
    "sig1=();created=1.5",
    "sig1=();created=--1",
    "sig1=();created=1234567890123456",
    "sig1=();created=abc;keyid",
    "sig1=();;created=1",
    "SIG1=()",
    "1sig=()",
    "sig1=?2",
    "sig1=(\"a\u{e9}\")",
    "sig1=\"tab\there\"",
    "sig1=()\u{0}",
    "=()",
];

#[test]
fn malformed_input_is_an_error_not_a_panic() {
    for bad in MALFORMED {
        assert!(
            parse_signature_input(bad).is_err(),
            "accepted Signature-Input `{bad}`"
        );
    }
    for bad in [
        "sig1",
        "sig1=",
        "sig1=:AAAA",
        "sig1=\"x\"",
        "sig1=(:AAAA:)",
        "sig1=:@@:",
    ] {
        assert!(
            parse_signature(bad, "sig1").is_err(),
            "accepted Signature `{bad}`"
        );
    }
    // Non-string, non-integer parameter types are refused when a signature is verified.
    let headers = hdrs(&[
        ("Signature-Input", "sig1=();created=\"1\";keyid=\"k\""),
        ("Signature", "sig1=:AAAA:"),
    ]);
    let policy = VerifyPolicy::new(&[], 1, 1);
    assert!(matches!(
        rfc9421::verify_request("GET", "https://a.example/", &headers, &policy, |_| None),
        Err(SignatureError::MalformedSignatureInput { .. })
    ));
}

#[test]
fn mutated_headers_never_panic() {
    let valid = "sig1=(\"@method\" \"@target-uri\" \"content-digest\");created=1728467285;keyid=\"b2ks6vm8p23w\";alg=\"ed25519\"";
    let sig = "sig1=:+CcncFjyE+JAuwJO8MOEhRdyfShQz59e9bWDYGN3hoBorVp69k4V2PvS7zJiAoX3QchMlc47sUF4DsptUN+rDQ==:";
    let mut inputs = vec![];
    for end in 0..=valid.len() {
        inputs.push(valid.get(..end).unwrap().to_owned());
        inputs.push(valid.get(end..).unwrap().to_owned());
    }
    for pos in 0..valid.len() {
        for replacement in ['"', ')', '(', ':', ';', ',', '=', ' ', '\\', '\u{e9}', '\0'] {
            let mut s = valid.to_owned();
            s.replace_range(pos..=pos, &replacement.to_string());
            inputs.push(s);
        }
    }
    for input in &inputs {
        let _ = parse_signature_input(input);
        let _ = parse_signature(input, "sig1");
        let _ = verify_content_digest(input, b"");
        let _ = verify_legacy_digest(input, b"");
        let _ = cavage::parse_signature_header(input);
        let headers = hdrs(&[
            ("Signature-Input", input),
            ("Signature", sig),
            ("Content-Digest", ""),
        ]);
        let _ = rfc9421::verify_request(
            "POST",
            FASP_URI,
            &headers,
            &VerifyPolicy::new(&["@method"], 1_728_467_285, 10),
            |_| None,
        );
    }
}

// ---------------------------------------------------------------------------------------------
// draft-cavage-12

const CAVAGE_URI: &str = "https://example.com/foo?param=value&pet=dog";
const CAVAGE_DATE: &str = "Sun, 05 Jan 2014 21:31:40 GMT";
const CAVAGE_DIGEST: &str = "SHA-256=X48E9qOokqqrvdts8nOJRJN3OWDUoyWxBf7kbu9DBPE=";

fn cavage_headers() -> Vec<(String, String)> {
    hdrs(&[
        ("Host", "example.com"),
        ("Date", CAVAGE_DATE),
        ("Content-Type", "application/json"),
        ("Digest", CAVAGE_DIGEST),
        ("Content-Length", "18"),
    ])
}

fn cavage_keys() -> (RsaSigningKey, RsaVerifyingKey) {
    let private = RsaSigningKey::from_pem(&fixture("cavage12_private.pem")).unwrap();
    let public = RsaVerifyingKey::from_pem(&fixture("cavage12_public.pem")).unwrap();
    assert_eq!(private.verifying_key(), public);
    (private, public)
}

#[test]
fn cavage_c1_default_test() {
    // No `headers` list: only `date` is signed.
    let value = "keyId=\"Test\",algorithm=\"rsa-sha256\",signature=\"SjWJWbWN7i0wzBvtPl8rbASWz5xQW6mcJmn+ibttBqtifLN7Sazz6m79cNfwwb8DMJ5cou1s7uEGKKCs+FLEEaDV5lp7q25WqS+lavg7T8hc0GppauB6hbgEKTwblDHYGEtbGmtdHgVCk9SuS13F0hZ8FD0k/5OxEPXe5WozsbM=\"";
    let parsed = cavage::parse_signature_header(value).unwrap();
    assert_eq!(parsed.headers, ["date"]);
    assert_eq!(
        cavage::signing_string(
            "POST",
            CAVAGE_URI,
            &cavage_headers(),
            &["date"],
            &cavage::SignOptions::default()
        )
        .unwrap(),
        "date: Sun, 05 Jan 2014 21:31:40 GMT"
    );
    let (_, public) = cavage_keys();
    let mut headers = cavage_headers();
    headers.push(("Signature".to_owned(), value.to_owned()));
    let now = parse_http_date(CAVAGE_DATE).unwrap();
    let v = cavage::verify_request(
        "POST",
        CAVAGE_URI,
        &headers,
        &["date"],
        |k| (k == "Test").then(|| public.clone()),
        now,
        30,
    )
    .unwrap();
    assert_eq!(v.date, Some(now));
}

#[test]
fn cavage_c2_basic_test() {
    let (private, public) = cavage_keys();
    let headers = cavage_headers();
    let signed = ["(request-target)", "host", "date"];
    let options = cavage::SignOptions::default();
    assert_eq!(
        cavage::signing_string("POST", CAVAGE_URI, &headers, &signed, &options).unwrap(),
        "(request-target): post /foo?param=value&pet=dog\nhost: example.com\ndate: Sun, 05 Jan 2014 21:31:40 GMT"
    );
    // RSASSA-PKCS1-v1_5 is deterministic, so signing reproduces the document's value exactly.
    let header = cavage::sign_request(
        "POST", CAVAGE_URI, &headers, &signed, "Test", &options, &private,
    )
    .unwrap();
    assert_eq!(
        header,
        "keyId=\"Test\",algorithm=\"rsa-sha256\",headers=\"(request-target) host date\",signature=\"qdx+H7PHHDZgy4y/Ahn9Tny9V3GP6YgBPyUXMmoxWtLbHpUnXS2mg2+SbrQDMCJypxBLSPQR2aAjn7ndmw2iicw3HMbe8VfEdKFYRqzic+efkb3nndiv/x1xSHDJWeSWkx3ButlYSuBskLu6kd9Fswtemr3lgdDEmn04swr2Os0=\""
    );
    // And the Authorization form of the same header verifies.
    let authorization = format!("Signature {header}");
    assert_eq!(
        cavage::parse_signature_header(&authorization)
            .unwrap()
            .key_id,
        "Test"
    );
    let mut with_sig = headers;
    with_sig.push(("Signature".to_owned(), header));
    let now = parse_http_date(CAVAGE_DATE).unwrap();
    let lookup = |k: &str| (k == "Test").then(|| public.clone());
    cavage::verify_request("POST", CAVAGE_URI, &with_sig, &signed, lookup, now + 10, 30).unwrap();

    // Date skew, both ways.
    assert!(matches!(
        cavage::verify_request("POST", CAVAGE_URI, &with_sig, &signed, lookup, now + 31, 30),
        Err(SignatureError::TooOld { .. })
    ));
    assert!(matches!(
        cavage::verify_request("POST", CAVAGE_URI, &with_sig, &signed, lookup, now - 31, 30),
        Err(SignatureError::InFuture { .. })
    ));
    // Required header not covered.
    assert_eq!(
        cavage::verify_request(
            "POST",
            CAVAGE_URI,
            &with_sig,
            &["(request-target)", "host", "date", "digest"],
            lookup,
            now,
            30
        ),
        Err(SignatureError::ComponentNotCovered {
            component: "digest".to_owned()
        })
    );
    // Tampering: method, path, a signed header, the key id.
    assert!(matches!(
        cavage::verify_request("GET", CAVAGE_URI, &with_sig, &signed, lookup, now, 30),
        Err(SignatureError::BadSignature { .. })
    ));
    assert!(matches!(
        cavage::verify_request(
            "POST",
            "https://example.com/foo?param=value&pet=cat",
            &with_sig,
            &signed,
            lookup,
            now,
            30
        ),
        Err(SignatureError::BadSignature { .. })
    ));
    let mut other_host: Vec<_> = with_sig
        .iter()
        .filter(|(n, _)| n != "Host")
        .cloned()
        .collect();
    other_host.push(("Host".to_owned(), "evil.example".to_owned()));
    assert!(matches!(
        cavage::verify_request("POST", CAVAGE_URI, &other_host, &signed, lookup, now, 30),
        Err(SignatureError::BadSignature { .. })
    ));
    assert_eq!(
        cavage::verify_request("POST", CAVAGE_URI, &with_sig, &signed, |_| None, now, 30),
        Err(SignatureError::UnknownKey {
            keyid: "Test".to_owned()
        })
    );
}

#[test]
fn cavage_c3_all_headers() {
    let (private, public) = cavage_keys();
    let headers = cavage_headers();
    let options = cavage::SignOptions {
        created: Some(1_402_170_695),
        expires: Some(1_402_170_699),
    };
    let documented = "vSdrb+dS3EceC9bcwHSo4MlyKS59iFIrhgYkz8+oVLEEzmYZZvRs8rgOp+63LEM3v+MFHB32NfpB2bEKBIvB1q52LaEUHFv120V01IL+TAD48XaERZFukWgHoBTLMhYS2Gb51gWxpeIq8knRmPnYePbF5MOkR0Zkly4zKH7s1dE=";

    // The document's signing string for the header-only part of C.3.
    let plain = [
        "(request-target)",
        "host",
        "date",
        "content-type",
        "digest",
        "content-length",
    ];
    assert_eq!(
        cavage::signing_string("POST", CAVAGE_URI, &headers, &plain, &options).unwrap(),
        "(request-target): post /foo?param=value&pet=dog\nhost: example.com\ndate: Sun, 05 Jan 2014 21:31:40 GMT\ncontent-type: application/json\ndigest: SHA-256=X48E9qOokqqrvdts8nOJRJN3OWDUoyWxBf7kbu9DBPE=\ncontent-length: 18"
    );
    // C.3's printed signature was made over exactly that string, without the `(created)` and
    // `(expires)` lines its own `headers` list names (the draft warns its vectors are "old and
    // possibly wrong"). We follow the draft's text and include the pseudo-header lines, so only
    // the header-only signature reproduces the document's bytes.
    let header = cavage::sign_request(
        "POST",
        CAVAGE_URI,
        &headers,
        &plain,
        "Test",
        &cavage::SignOptions::default(),
        &private,
    )
    .unwrap();
    assert!(
        header.contains(&format!("signature=\"{documented}\"")),
        "{header}"
    );

    // With the pseudo-headers covered, the string gains their lines.
    let signed = [
        "(request-target)",
        "(created)",
        "(expires)",
        "host",
        "date",
        "content-type",
        "digest",
        "content-length",
    ];
    assert_eq!(
        cavage::signing_string("POST", CAVAGE_URI, &headers, &signed, &options).unwrap(),
        "(request-target): post /foo?param=value&pet=dog\n(created): 1402170695\n(expires): 1402170699\nhost: example.com\ndate: Sun, 05 Jan 2014 21:31:40 GMT\ncontent-type: application/json\ndigest: SHA-256=X48E9qOokqqrvdts8nOJRJN3OWDUoyWxBf7kbu9DBPE=\ncontent-length: 18"
    );
    let header = cavage::sign_request(
        "POST", CAVAGE_URI, &headers, &signed, "Test", &options, &private,
    )
    .unwrap();
    assert!(header.contains(",created=1402170695,expires=1402170699,"));
    let mut with_sig = headers;
    with_sig.push(("Signature".to_owned(), header));
    let lookup = |k: &str| (k == "Test").then(|| public.clone());
    let verify = |now: i64, skew: i64| {
        cavage::verify_request("POST", CAVAGE_URI, &with_sig, &signed, lookup, now, skew)
    };
    // The Date header (January) is checked against the clock as well as `created` (June).
    assert!(matches!(
        verify(1_402_170_697, 10),
        Err(SignatureError::TooOld {
            what: "Date header",
            ..
        })
    ));
    let ok = verify(1_402_170_697, i64::MAX / 4).unwrap();
    assert_eq!(ok.created, Some(1_402_170_695));
    assert_eq!(
        verify(1_402_170_700, i64::MAX / 4),
        Err(SignatureError::Expired { seconds: 1 })
    );
    // `created` is skew-checked too.
    assert!(matches!(
        verify(1_402_170_695 + 100, 50),
        Err(SignatureError::TooOld {
            what: "created",
            ..
        })
    ));
}

fn owned_generated_rsa() -> RsaSigningKey {
    RsaSigningKey::from_pem(&generated_rsa().to_pkcs8_pem().unwrap()).unwrap()
}

fn generated_rsa() -> &'static RsaSigningKey {
    static KEY: OnceLock<RsaSigningKey> = OnceLock::new();
    KEY.get_or_init(|| RsaSigningKey::generate().unwrap())
}

#[test]
fn cavage_activitypub_get_and_post_round_trip() {
    let private = generated_rsa();
    let public = private.verifying_key();
    let key_id = "https://social.example/actor#main-key";
    let now = 1_760_000_000;
    let lookup = |k: &str| (k == key_id).then(|| public.clone());

    // GET: (request-target) host date.
    let url = "https://mastodon.example/users/alice/outbox?page=true";
    let headers = hdrs(&[
        ("host", &authority(url).unwrap()),
        ("date", &http_date(now)),
        ("accept", "application/activity+json"),
    ]);
    let header = cavage::sign_request(
        "GET",
        url,
        &headers,
        cavage::GET_HEADERS,
        key_id,
        &cavage::SignOptions::default(),
        private,
    )
    .unwrap();
    assert!(header.contains("headers=\"(request-target) host date\""));
    let mut received = headers.clone();
    received.push(("signature".to_owned(), header));
    let v = cavage::verify_request(
        "GET",
        url,
        &received,
        cavage::GET_HEADERS,
        lookup,
        now + 20,
        300,
    )
    .unwrap();
    assert_eq!(v.key_id, key_id);
    assert_eq!(v.date, Some(now));

    // POST: adds digest, which the receiver then checks against the body.
    let inbox = "https://mastodon.example/inbox";
    let body = b"{\"type\":\"Delete\"}";
    let headers = hdrs(&[
        ("host", "mastodon.example"),
        ("date", &http_date(now)),
        ("digest", &legacy_digest(body)),
    ]);
    let header = cavage::sign_request(
        "POST",
        inbox,
        &headers,
        cavage::POST_HEADERS,
        key_id,
        &cavage::SignOptions::default(),
        private,
    )
    .unwrap();
    let mut received = headers;
    received.push(("signature".to_owned(), header));
    cavage::verify_request(
        "POST",
        inbox,
        &received,
        cavage::POST_HEADERS,
        lookup,
        now,
        300,
    )
    .unwrap();
    verify_legacy_digest(
        &received.iter().find(|(n, _)| n == "digest").unwrap().1,
        body,
    )
    .unwrap();
    // A signature that does not cover the digest is refused when the caller requires it.
    let headers = hdrs(&[
        ("host", "mastodon.example"),
        ("date", &http_date(now)),
        ("digest", &legacy_digest(body)),
    ]);
    let header = cavage::sign_request(
        "POST",
        inbox,
        &headers,
        cavage::GET_HEADERS,
        key_id,
        &cavage::SignOptions::default(),
        private,
    )
    .unwrap();
    let mut received = headers;
    received.push(("signature".to_owned(), header));
    assert!(matches!(
        cavage::verify_request(
            "POST",
            inbox,
            &received,
            cavage::POST_HEADERS,
            lookup,
            now,
            300
        ),
        Err(SignatureError::ComponentNotCovered { .. })
    ));
}

#[test]
fn cavage_header_parsing() {
    let ok = cavage::parse_signature_header(
        "keyId=\"https://a.example/actor#k\",algorithm=\"hs2019\", headers=\"(request-target) Host DATE\" ,signature=\"AAAA\",extra=\"x,y\"",
    )
    .unwrap();
    assert_eq!(ok.key_id, "https://a.example/actor#k");
    assert_eq!(ok.headers, ["(request-target)", "host", "date"]);
    assert_eq!(ok.algorithm.as_deref(), Some("hs2019"));
    let numeric =
        cavage::parse_signature_header("keyId=\"k\",created=12,expires=34,signature=\"AAAA\"")
            .unwrap();
    assert_eq!((numeric.created, numeric.expires), (Some(12), Some(34)));

    for bad in [
        "",
        "keyId",
        "keyId=\"k\"",
        "signature=\"AAAA\"",
        "keyId=\"k\",signature=\"AAAA",
        "keyId=\"k\",signature=\"AAAA\",keyId=\"j\"",
        "keyId=\"k\",signature=\"***\"",
        "keyId=\"k\",signature=\"AAAA\",created=soon",
        "keyId=\"k\",signature=\"AAAA\",headers=\"\"",
        "keyId=\"k\"\\",
        "keyId=\"k\",signature=\"AA\\xAA\"",
        "=\"k\"",
    ] {
        assert!(
            cavage::parse_signature_header(bad).is_err(),
            "accepted `{bad}`"
        );
    }
}

#[test]
fn cavage_refuses_other_algorithms_and_bad_dates() {
    let (_, public) = cavage_keys();
    let mut headers = cavage_headers();
    headers.push((
        "Signature".to_owned(),
        "keyId=\"Test\",algorithm=\"ed25519\",headers=\"date\",signature=\"AAAA\"".to_owned(),
    ));
    assert!(matches!(
        cavage::verify_request(
            "GET",
            CAVAGE_URI,
            &headers,
            &["date"],
            |_| Some(public.clone()),
            0,
            1
        ),
        Err(SignatureError::UnsupportedAlgorithm { .. })
    ));
    let mut headers = hdrs(&[("Host", "example.com"), ("Date", "yesterday-ish")]);
    headers.push((
        "Signature".to_owned(),
        "keyId=\"Test\",headers=\"host date\",signature=\"AAAA\"".to_owned(),
    ));
    assert!(matches!(
        cavage::verify_request(
            "GET",
            CAVAGE_URI,
            &headers,
            &["date"],
            |_| Some(public.clone()),
            0,
            1
        ),
        Err(SignatureError::MalformedDate { .. })
    ));
    // Neither `date` nor `(created)` covered: no time binding at all.
    let mut headers = hdrs(&[("Host", "example.com")]);
    headers.push((
        "Signature".to_owned(),
        "keyId=\"Test\",headers=\"host\",signature=\"AAAA\"".to_owned(),
    ));
    assert!(matches!(
        cavage::verify_request(
            "GET",
            CAVAGE_URI,
            &headers,
            &["host"],
            |_| Some(public.clone()),
            0,
            1
        ),
        Err(SignatureError::ComponentNotCovered { .. })
    ));
}

#[test]
fn http_dates() {
    assert_eq!(http_date(1_388_957_500), CAVAGE_DATE);
    assert_eq!(parse_http_date(CAVAGE_DATE).unwrap(), 1_388_957_500);
    assert_eq!(
        parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT").unwrap(),
        784_111_777
    );
    assert!(parse_http_date("").is_err());
    assert!(parse_http_date("2014-01-05T21:31:40Z").is_err());
}

// ---------------------------------------------------------------------------------------------
// Keys

#[test]
fn ed25519_key_formats() {
    let key = Ed25519SigningKey::from_seed(&ed25519_test_seed());
    let public = key.verifying_key();
    assert_eq!(
        public.to_base64(),
        "JrQLj5P/89iXES9+vFgrIy29clF9CC/oPPsw3c5D0bs="
    );
    // base64(SHA-256(raw public key)), computed independently with Python's hashlib.
    assert_eq!(
        public.fingerprint(),
        "sWwtG+rRJiY5dk/bDuTTd0WZM2vUk0BM2ksRNsWfIGI="
    );
    assert_eq!(
        Ed25519VerifyingKey::from_base64(&public.to_base64()).unwrap(),
        public
    );
    assert_eq!(
        Ed25519VerifyingKey::from_bytes(&public.to_bytes()).unwrap(),
        public
    );
    assert_eq!(key.to_seed(), ed25519_test_seed());
    assert_eq!(
        Ed25519SigningKey::from_seed_slice(&ed25519_test_seed())
            .unwrap()
            .verifying_key(),
        public
    );

    assert!(Ed25519SigningKey::from_seed_slice(&[0; 31]).is_err());
    assert!(Ed25519VerifyingKey::from_bytes(&[0; 33]).is_err());
    assert!(Ed25519VerifyingKey::from_base64("not base64!").is_err());
    assert_ne!(
        Ed25519SigningKey::generate().unwrap().to_seed(),
        Ed25519SigningKey::generate().unwrap().to_seed()
    );
}

#[test]
fn secrets_stay_out_of_debug_output() {
    let seed = ed25519_test_seed();
    let ed = Ed25519SigningKey::from_seed(&seed);
    let shown = format!(
        "{ed:?} {:?}",
        SigningKey::Ed25519(Ed25519SigningKey::from_seed(&seed))
    );
    assert!(!shown.contains(&STANDARD.encode(seed)));
    assert!(!shown.contains(&format!("{seed:?}")));
    assert!(shown.contains("redacted"));

    let shown = format!(
        "{:?} {:?}",
        owned_generated_rsa(),
        SigningKey::RsaV15Sha256(owned_generated_rsa())
    );
    assert!(shown.contains("redacted"));
    assert!(!shown.contains("MII"));
}

#[test]
fn rsa_key_formats() {
    // PKCS#1 private key (draft-cavage's format) in, PKCS#8 out, and back.
    let key = RsaSigningKey::from_pem(&fixture("cavage12_private.pem")).unwrap();
    assert!(fixture("cavage12_private.pem").starts_with("-----BEGIN RSA PRIVATE KEY-----"));
    let pkcs8 = key.to_pkcs8_pem().unwrap();
    assert!(pkcs8.starts_with("-----BEGIN PRIVATE KEY-----"));
    let again = RsaSigningKey::from_pem(&pkcs8).unwrap();
    assert_eq!(again.verifying_key(), key.verifying_key());

    // SPKI public key out (what `publicKeyPem` carries) and in; PKCS#1 public key in.
    let spki = key.verifying_key().to_spki_pem().unwrap();
    assert!(spki.starts_with("-----BEGIN PUBLIC KEY-----"));
    assert_eq!(
        RsaVerifyingKey::from_pem(&spki).unwrap(),
        key.verifying_key()
    );
    let pkcs1_public = fixture("rfc9421_rsa_public.pem");
    assert!(pkcs1_public.starts_with("-----BEGIN RSA PUBLIC KEY-----"));
    // RFC 9421 B.1.1's public key is PKCS#1; it survives a round trip through SPKI.
    let b11 = RsaVerifyingKey::from_pem(&pkcs1_public).unwrap();
    assert_eq!(
        RsaVerifyingKey::from_pem(&b11.to_spki_pem().unwrap()).unwrap(),
        b11
    );

    assert!(RsaSigningKey::from_pem("nope").is_err());
    assert!(RsaVerifyingKey::from_pem(&fixture("rfc9421_rsa_private.pem")).is_err());
}

#[test]
fn generated_rsa_key_signs_and_round_trips_pem() {
    let key = generated_rsa();
    let reloaded = RsaSigningKey::from_pem(&key.to_pkcs8_pem().unwrap()).unwrap();
    assert_eq!(reloaded.verifying_key(), key.verifying_key());
    let spki = key.verifying_key().to_spki_pem().unwrap();
    assert_eq!(
        RsaVerifyingKey::from_pem(&spki).unwrap(),
        key.verifying_key()
    );
}

/// Mastodon 4.7.5 signs FASP responses with `created` and no `keyid`. A caller that knows the
/// signer's key can verify them; key lookup by `keyid` still requires one.
#[test]
fn responses_without_keyid_verify_against_a_known_key() {
    let signer = Ed25519SigningKey::from_seed(&[9; 32]);
    let key = SigningKey::Ed25519(Ed25519SigningKey::from_seed(&[9; 32]));
    let digest = content_digest(b"{}");
    let params = SignatureParams {
        created: Some(1000),
        ..SignatureParams::default()
    };
    let (input, signature) = rfc9421::sign(
        &Message::Response { status: 201 },
        &[("content-digest", digest.as_str())],
        "sig1",
        &["@status", "content-digest"],
        &params,
        &key,
    )
    .unwrap();
    assert!(!input.contains("keyid"), "{input}");
    let headers = [
        ("content-digest", digest.as_str()),
        ("signature-input", input.as_str()),
        ("signature", signature.as_str()),
    ];
    let policy = VerifyPolicy::new(&["@status", "content-digest"], 1000, 300);
    let public = VerifyingKey::Ed25519(signer.verifying_key());
    let verified = rfc9421::verify_response_with_key(201, &headers[..], &policy, &public).unwrap();
    assert_eq!(verified.keyid, "");
    assert!(matches!(
        rfc9421::verify_response(201, &headers[..], &policy, |_| Some(public.clone())),
        Err(SignatureError::MissingKeyId)
    ));
    // The wrong key, or a tampered status, still fails.
    let other = VerifyingKey::Ed25519(Ed25519SigningKey::from_seed(&[8; 32]).verifying_key());
    assert!(rfc9421::verify_response_with_key(201, &headers[..], &policy, &other).is_err());
    assert!(rfc9421::verify_response_with_key(200, &headers[..], &policy, &public).is_err());
}
