//! Signing and verifying keys, and the formats they travel in.
//!
//! RSA keys are what `ActivityPub` actors publish (`publicKeyPem`, SPKI PEM). Ed25519 keys are what
//! FASP peers exchange (raw 32-byte public key, base64). Secret keys never appear in `Debug`
//! output.

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::Signature as Ed25519Signature;
use getrandom::SysRng;
use rsa::pkcs1::{DecodeRsaPrivateKey, DecodeRsaPublicKey};
use rsa::pkcs8::{
    DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey, LineEnding,
};
use rsa::sha2::Sha256 as RsaSha256;
use rsa::sha2::Sha512 as RsaSha512;
use rsa::signature::{SignatureEncoding, Signer, Verifier};
use rsa::{PrivateKey, PublicKey, pkcs1v15, pss};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::SignatureError;

/// RSA modulus size used for generated keys.
pub const RSA_BITS: usize = 2048;

/// A key could not be generated, parsed or encoded.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum KeyError {
    #[error("failed to generate an {algorithm} key: {reason}")]
    Generate {
        algorithm: &'static str,
        reason: String,
    },
    #[error("invalid {what} PEM: {reason}")]
    Pem { what: &'static str, reason: String },
    #[error("failed to encode {what}: {reason}")]
    Encode { what: &'static str, reason: String },
    #[error("ed25519 {what} must be 32 bytes, got {actual}")]
    Length { what: &'static str, actual: usize },
    #[error("invalid ed25519 public key: {reason}")]
    InvalidPublicKey { reason: String },
    #[error("ed25519 public key is not valid base64: {reason}")]
    Base64 { reason: String },
}

/// An RSA private key (2048-bit when generated here).
pub struct RsaSigningKey(PrivateKey);

impl fmt::Debug for RsaSigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RsaSigningKey(<redacted>)")
    }
}

impl RsaSigningKey {
    /// Generates a new 2048-bit key. This takes a noticeable fraction of a second.
    pub fn generate() -> Result<Self, KeyError> {
        PrivateKey::new(&mut SysRng, RSA_BITS)
            .map(Self)
            .map_err(|e| KeyError::Generate {
                algorithm: "RSA",
                reason: e.to_string(),
            })
    }

    /// Parses a PKCS#8 (`BEGIN PRIVATE KEY`) or PKCS#1 (`BEGIN RSA PRIVATE KEY`) PEM.
    pub fn from_pem(pem: &str) -> Result<Self, KeyError> {
        PrivateKey::from_pkcs8_pem(pem)
            .or_else(|_| PrivateKey::from_pkcs1_pem(pem))
            .map(Self)
            .map_err(|_| KeyError::Pem {
                what: "RSA private key",
                reason: "not a PKCS#8 or PKCS#1 private key".to_owned(),
            })
    }

    /// The key as PKCS#8 PEM. Treat the result as a secret.
    pub fn to_pkcs8_pem(&self) -> Result<String, KeyError> {
        self.0
            .to_pkcs8_pem(LineEnding::LF)
            .map(|pem| pem.to_string())
            .map_err(|e| KeyError::Encode {
                what: "RSA private key",
                reason: e.to_string(),
            })
    }

    #[must_use]
    pub fn verifying_key(&self) -> RsaVerifyingKey {
        RsaVerifyingKey(self.0.to_public_key())
    }

    /// RSASSA-PKCS1-v1_5 with SHA-256 (`rsa-v1_5-sha256`, draft-cavage `rsa-sha256`).
    pub(crate) fn sign_pkcs1v15_sha256(&self, message: &[u8]) -> Result<Vec<u8>, SignatureError> {
        let key = pkcs1v15::SigningKey::<RsaSha256>::new(self.0.clone());
        key.try_sign(message)
            .map(|sig| sig.to_vec())
            .map_err(|e| SignatureError::Signing {
                algorithm: "rsa-v1_5-sha256",
                reason: e.to_string(),
            })
    }
}

/// An RSA public key.
#[derive(Clone, PartialEq, Eq)]
pub struct RsaVerifyingKey(PublicKey);

impl fmt::Debug for RsaVerifyingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RsaVerifyingKey(..)")
    }
}

impl RsaVerifyingKey {
    /// Parses an SPKI PEM (`BEGIN PUBLIC KEY`, what `publicKeyPem` carries) or a PKCS#1 PEM
    /// (`BEGIN RSA PUBLIC KEY`, which some older servers publish).
    pub fn from_pem(pem: &str) -> Result<Self, KeyError> {
        PublicKey::from_public_key_pem(pem)
            .or_else(|_| PublicKey::from_pkcs1_pem(pem))
            .map(Self)
            .map_err(|_| KeyError::Pem {
                what: "RSA public key",
                reason: "not an SPKI or PKCS#1 public key".to_owned(),
            })
    }

    /// The key as SPKI PEM, for an actor's `publicKeyPem`.
    pub fn to_spki_pem(&self) -> Result<String, KeyError> {
        self.0
            .to_public_key_pem(LineEnding::LF)
            .map_err(|e| KeyError::Encode {
                what: "RSA public key",
                reason: e.to_string(),
            })
    }

    pub(crate) fn verify_pkcs1v15_sha256(&self, message: &[u8], signature: &[u8]) -> bool {
        let Ok(sig) = pkcs1v15::Signature::try_from(signature) else {
            return false;
        };
        pkcs1v15::VerifyingKey::<RsaSha256>::new(self.0.clone())
            .verify(message, &sig)
            .is_ok()
    }

    pub(crate) fn verify_pss_sha512(&self, message: &[u8], signature: &[u8]) -> bool {
        let Ok(sig) = pss::Signature::try_from(signature) else {
            return false;
        };
        pss::VerifyingKey::<RsaSha512>::new(self.0.clone())
            .verify(message, &sig)
            .is_ok()
    }
}

/// An Ed25519 private key.
pub struct Ed25519SigningKey(ed25519_dalek::SigningKey);

impl fmt::Debug for Ed25519SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Ed25519SigningKey(<redacted>)")
    }
}

impl Ed25519SigningKey {
    /// Generates a new key from the operating system's random number generator.
    pub fn generate() -> Result<Self, KeyError> {
        let mut seed = [0_u8; 32];
        getrandom::fill(&mut seed).map_err(|e| KeyError::Generate {
            algorithm: "Ed25519",
            reason: e.to_string(),
        })?;
        Ok(Self::from_seed(&seed))
    }

    /// The key with this 32-byte seed (RFC 8032 private key).
    #[must_use]
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self(ed25519_dalek::SigningKey::from_bytes(seed))
    }

    /// Like [`Self::from_seed`] for a slice of unknown length.
    pub fn from_seed_slice(seed: &[u8]) -> Result<Self, KeyError> {
        let seed: [u8; 32] = seed.try_into().map_err(|_| KeyError::Length {
            what: "seed",
            actual: seed.len(),
        })?;
        Ok(Self::from_seed(&seed))
    }

    /// The 32-byte seed. Treat it as a secret.
    #[must_use]
    pub fn to_seed(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    #[must_use]
    pub fn verifying_key(&self) -> Ed25519VerifyingKey {
        Ed25519VerifyingKey(self.0.verifying_key())
    }

    pub(crate) fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.0.sign(message).to_bytes().to_vec()
    }
}

/// An Ed25519 public key.
#[derive(Clone, PartialEq, Eq)]
pub struct Ed25519VerifyingKey(ed25519_dalek::VerifyingKey);

impl fmt::Debug for Ed25519VerifyingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Ed25519VerifyingKey({})", self.to_base64())
    }
}

impl Ed25519VerifyingKey {
    /// From the raw 32-byte public key.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, KeyError> {
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| KeyError::Length {
            what: "public key",
            actual: bytes.len(),
        })?;
        ed25519_dalek::VerifyingKey::from_bytes(&bytes)
            .map(Self)
            .map_err(|e| KeyError::InvalidPublicKey {
                reason: e.to_string(),
            })
    }

    /// From the base64 of the raw public key, as FASP registration exchanges it.
    pub fn from_base64(value: &str) -> Result<Self, KeyError> {
        let bytes = STANDARD
            .decode(value.trim())
            .map_err(|e| KeyError::Base64 {
                reason: e.to_string(),
            })?;
        Self::from_bytes(&bytes)
    }

    #[must_use]
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    /// Standard base64 (with padding) of the raw public key.
    #[must_use]
    pub fn to_base64(&self) -> String {
        STANDARD.encode(self.to_bytes())
    }

    /// The fingerprint an admin compares: base64 of the SHA-256 of the raw public key.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        STANDARD.encode(Sha256::digest(self.to_bytes()))
    }

    pub(crate) fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        let Ok(sig) = Ed25519Signature::from_slice(signature) else {
            return false;
        };
        self.0.verify_strict(message, &sig).is_ok()
    }
}

/// A key that can make RFC 9421 signatures.
#[derive(Debug)]
pub enum SigningKey {
    /// `ed25519`
    Ed25519(Ed25519SigningKey),
    /// `rsa-v1_5-sha256`
    RsaV15Sha256(RsaSigningKey),
}

impl SigningKey {
    /// The RFC 9421 algorithm name.
    #[must_use]
    pub fn algorithm(&self) -> &'static str {
        match self {
            Self::Ed25519(_) => "ed25519",
            Self::RsaV15Sha256(_) => "rsa-v1_5-sha256",
        }
    }

    pub(crate) fn sign(&self, message: &[u8]) -> Result<Vec<u8>, SignatureError> {
        match self {
            Self::Ed25519(key) => Ok(key.sign(message)),
            Self::RsaV15Sha256(key) => key.sign_pkcs1v15_sha256(message),
        }
    }
}

/// A key that can check RFC 9421 signatures. The variant fixes the algorithm, so a signature's
/// `alg` parameter, if any, can only confirm it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyingKey {
    /// `ed25519`
    Ed25519(Ed25519VerifyingKey),
    /// `rsa-v1_5-sha256`
    RsaV15Sha256(RsaVerifyingKey),
    /// `rsa-pss-sha512` (verify only)
    RsaPssSha512(RsaVerifyingKey),
}

impl VerifyingKey {
    /// The RFC 9421 algorithm name.
    #[must_use]
    pub fn algorithm(&self) -> &'static str {
        match self {
            Self::Ed25519(_) => "ed25519",
            Self::RsaV15Sha256(_) => "rsa-v1_5-sha256",
            Self::RsaPssSha512(_) => "rsa-pss-sha512",
        }
    }

    pub(crate) fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        match self {
            Self::Ed25519(key) => key.verify(message, signature),
            Self::RsaV15Sha256(key) => key.verify_pkcs1v15_sha256(message, signature),
            Self::RsaPssSha512(key) => key.verify_pss_sha512(message, signature),
        }
    }
}
