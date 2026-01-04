//! Content hashing for clipboard deduplication

use sha2::{Digest, Sha256};

/// Compute SHA256 hash of clipboard content
pub fn compute_hash(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("SHA256:{}", hex::encode(hasher.finalize()))
}

/// Quick hash from first N bytes + length for large content
/// Useful for quick deduplication check before transferring
#[allow(dead_code)]
pub fn compute_quick_hash(data: &[u8], size: u64) -> String {
    const SAMPLE_SIZE: usize = 4096;
    let mut hasher = Sha256::new();
    hasher.update(&data[..data.len().min(SAMPLE_SIZE)]);
    hasher.update(size.to_le_bytes());
    format!("SHA256-QUICK:{}", hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_hash() {
        let data = b"Hello, World!";
        let hash = compute_hash(data);
        assert!(hash.starts_with("SHA256:"));
        assert_eq!(hash.len(), 7 + 64); // "SHA256:" + 64 hex chars
    }

    #[test]
    fn test_same_content_same_hash() {
        let data1 = b"test content";
        let data2 = b"test content";
        assert_eq!(compute_hash(data1), compute_hash(data2));
    }

    #[test]
    fn test_different_content_different_hash() {
        let data1 = b"test content 1";
        let data2 = b"test content 2";
        assert_ne!(compute_hash(data1), compute_hash(data2));
    }
}
