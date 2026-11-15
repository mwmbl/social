//! The 4096-byte header of an index file: `mwmbl-tiny-search` followed by JSON, zero padded.

use serde::{Deserialize, Serialize};

pub const METADATA_SIZE: usize = 4096;
pub const INDEX_VERSION: i32 = 1;
pub const METADATA_CONSTANT: &[u8] = b"mwmbl-tiny-search";

/// Why a header could not be read or written. The caller adds the index path.
#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    #[error("this doesn't seem to be an index file")]
    NotAnIndex,
    #[error("the metadata is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("the metadata is too big for the {METADATA_SIZE}-byte header")]
    TooBig,
}

/// What an index file says about itself.
///
/// For a `Document` index the header is byte-compatible with Python's `TinyIndexMetadata`,
/// which is built with `TinyIndexMetadata(**json)`: `tokenizer_version` is only written when
/// set, so Python never sees a key it does not know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexMetadata {
    pub version: i32,
    pub page_size: usize,
    pub num_pages: usize,
    pub item_factory: String,
    /// The version of the tokeniser whose output the keys are. Set by callers that tokenise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokenizer_version: Option<u32>,
}

impl IndexMetadata {
    pub fn to_bytes(&self) -> Result<[u8; METADATA_SIZE], MetadataError> {
        let json = serde_json::to_vec(self)?;
        let mut bytes = [0u8; METADATA_SIZE];
        let end = METADATA_CONSTANT.len() + json.len();
        let Some(region) = bytes.get_mut(..end) else {
            return Err(MetadataError::TooBig);
        };
        let (constant, body) = region.split_at_mut(METADATA_CONSTANT.len());
        constant.copy_from_slice(METADATA_CONSTANT);
        body.copy_from_slice(&json);
        Ok(bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, MetadataError> {
        let Some(rest) = bytes.strip_prefix(METADATA_CONSTANT) else {
            return Err(MetadataError::NotAnIndex);
        };
        let json: Vec<u8> = rest.iter().copied().take_while(|b| *b != 0).collect();
        Ok(serde_json::from_slice(&json)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(tokenizer_version: Option<u32>) -> IndexMetadata {
        IndexMetadata {
            version: INDEX_VERSION,
            page_size: 4096,
            num_pages: 5,
            item_factory: "Document".into(),
            tokenizer_version,
        }
    }

    #[test]
    fn round_trips() {
        for version in [None, Some(2)] {
            let m = metadata(version);
            assert_eq!(
                IndexMetadata::from_bytes(&m.to_bytes().unwrap()).unwrap(),
                m
            );
        }
    }

    #[test]
    fn rejects_other_files() {
        assert!(matches!(
            IndexMetadata::from_bytes(&[1u8; 100]),
            Err(MetadataError::NotAnIndex)
        ));
    }

    #[test]
    fn rejects_oversized_metadata() {
        let mut m = metadata(None);
        m.item_factory = "x".repeat(METADATA_SIZE);
        assert!(matches!(m.to_bytes(), Err(MetadataError::TooBig)));
    }
}
