use thiserror::Error;

/// Why a signature could not be made, parsed or verified.
///
/// Messages name the header, component or key id involved. They never contain key material.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SignatureError {
    #[error("missing header `{name}`")]
    MissingHeader { name: String },
    #[error("header `{name}` contains a line break")]
    InvalidHeaderValue { name: String },
    #[error("malformed Signature-Input: {reason}")]
    MalformedSignatureInput { reason: String },
    #[error("malformed Signature header: {reason}")]
    MalformedSignature { reason: String },
    #[error("malformed digest header: {reason}")]
    MalformedDigest { reason: String },
    #[error("digest header has no sha-256 member")]
    NoSha256Digest,
    #[error("sha-256 digest does not match the body")]
    DigestMismatch,
    #[error("no signature labelled `{label}` in Signature-Input")]
    LabelNotFound { label: String },
    #[error("no signature labelled `{label}` in the Signature header")]
    SignatureNotFound { label: String },
    #[error("invalid signature label `{label}`")]
    InvalidLabel { label: String },
    #[error("component `{component}` is not covered by the signature")]
    ComponentNotCovered { component: String },
    #[error("component `{component}` is covered more than once")]
    DuplicateComponent { component: String },
    #[error("unsupported component `{component}`: {reason}")]
    UnsupportedComponent { component: String, reason: String },
    #[error("invalid target URI `{uri}`: {reason}")]
    InvalidTargetUri { uri: String, reason: String },
    #[error("invalid {what}: {reason}")]
    InvalidValue { what: &'static str, reason: String },
    #[error("signature has no `created` parameter")]
    MissingCreated,
    #[error("signature has no `keyid` parameter")]
    MissingKeyId,
    #[error("{what} is {seconds} seconds old, more than the allowed {max}")]
    TooOld {
        what: &'static str,
        seconds: i64,
        max: i64,
    },
    #[error("{what} is {seconds} seconds in the future, more than the allowed {max}")]
    InFuture {
        what: &'static str,
        seconds: i64,
        max: i64,
    },
    #[error("signature expired {seconds} seconds ago")]
    Expired { seconds: i64 },
    #[error("malformed date `{value}`")]
    MalformedDate { value: String },
    #[error("unknown keyid `{keyid}`")]
    UnknownKey { keyid: String },
    #[error("unsupported algorithm `{algorithm}`")]
    UnsupportedAlgorithm { algorithm: String },
    #[error("keyid `{keyid}` is for `{expected}` but the signature declares alg `{declared}`")]
    AlgorithmMismatch {
        keyid: String,
        declared: String,
        expected: &'static str,
    },
    #[error("bad signature for keyid `{keyid}`")]
    BadSignature { keyid: String },
    #[error("failed to sign with {algorithm}: {reason}")]
    Signing {
        algorithm: &'static str,
        reason: String,
    },
}
