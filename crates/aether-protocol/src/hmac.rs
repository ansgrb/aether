use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SignatureError {
    #[error("invalid hex encoding in wire HMAC signature")]
    InvalidHex,
    #[error("HMAC-SHA256 signature verification failed")]
    VerificationFailed,
}

/// Signs and verifies Jupyter Wire Protocol message frames using HMAC-SHA256.
#[derive(Clone, Debug)]
pub struct HmacSigner {
    key: Vec<u8>,
}

impl HmacSigner {
    pub fn new(key: impl AsRef<[u8]>) -> Self {
        Self {
            key: key.as_ref().to_vec(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        !self.key.is_empty()
    }

    /// Computes the lowercase hex-encoded HMAC-SHA256 over the 4 serialized JSON frames:
    /// `[header, parent_header, metadata, content]`.
    pub fn sign(&self, header: &str, parent_header: &str, metadata: &str, content: &str) -> String {
        if self.key.is_empty() {
            return String::new();
        }
        let mut mac =
            HmacSha256::new_from_slice(&self.key).expect("HMAC-SHA256 accepts keys of any size");
        mac.update(header.as_bytes());
        mac.update(parent_header.as_bytes());
        mac.update(metadata.as_bytes());
        mac.update(content.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// Constant-time verification of an incoming wire signature over the 4 JSON frames.
    pub fn verify(
        &self,
        signature_hex: &str,
        header: &str,
        parent_header: &str,
        metadata: &str,
        content: &str,
    ) -> Result<(), SignatureError> {
        if self.key.is_empty() {
            return Ok(());
        }
        let expected_bytes =
            hex::decode(signature_hex.trim()).map_err(|_| SignatureError::InvalidHex)?;
        let mut mac =
            HmacSha256::new_from_slice(&self.key).expect("HMAC-SHA256 accepts keys of any size");
        mac.update(header.as_bytes());
        mac.update(parent_header.as_bytes());
        mac.update(metadata.as_bytes());
        mac.update(content.as_bytes());
        mac.verify_slice(&expected_bytes)
            .map_err(|_| SignatureError::VerificationFailed)
    }
}
