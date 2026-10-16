//! Domain-separated SHA-256 digests.
//!
//! Every Bergsmith digest is `SHA-256(domain || 0x00 || bytes)`. The domain
//! names what was hashed, so digests of different kinds never collide even
//! when their byte encodings do.

use std::fmt;

use sha2::{Digest as _, Sha256};

/// A SHA-256 digest.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    /// `SHA-256(domain || 0x00 || bytes)`.
    #[must_use]
    pub fn of(domain: &[u8], bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(domain);
        hasher.update([0]);
        hasher.update(bytes);
        Self(hasher.finalize().into())
    }

    /// Wraps 32 digest bytes, such as an `ArtifactRef.sha256` field.
    ///
    /// # Errors
    ///
    /// Returns [`DigestLengthError`] unless `bytes` is exactly 32 bytes long.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, DigestLengthError> {
        <[u8; 32]>::try_from(bytes)
            .map(Self)
            .map_err(|_| DigestLengthError { len: bytes.len() })
    }

    /// Parses 64 hex characters, the form persisted in PostgreSQL.
    ///
    /// # Errors
    ///
    /// Returns [`DigestHexError`] for any other input.
    pub fn from_hex(hex: &str) -> Result<Self, DigestHexError> {
        let hex = hex.as_bytes();
        if hex.len() != 64 {
            return Err(DigestHexError);
        }
        let mut bytes = [0; 32];
        for (byte, pair) in bytes.iter_mut().zip(hex.chunks_exact(2)) {
            *byte = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Ok(Self(bytes))
    }

    /// The digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The digest as 64 lowercase hex characters, the form persisted in
    /// PostgreSQL.
    #[must_use]
    pub fn to_hex(&self) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut hex = String::with_capacity(64);
        for byte in self.0 {
            hex.push(char::from(DIGITS[usize::from(byte >> 4)]));
            hex.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
        }
        hex
    }
}

fn nibble(c: u8) -> Result<u8, DigestHexError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(DigestHexError),
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256Digest({})", self.to_hex())
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Bytes that are not a 32-byte SHA-256 digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a SHA-256 digest has 32 bytes, got {len}")]
pub struct DigestLengthError {
    len: usize,
}

/// Text that is not 64 hex characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a SHA-256 digest is 64 hex characters")]
pub struct DigestHexError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_are_domain_separated() {
        assert_ne!(Sha256Digest::of(b"a", b"b"), Sha256Digest::of(b"ab", b""));
        // SHA-256 of "d\0x".
        assert_eq!(
            Sha256Digest::of(b"d", b"x").to_hex(),
            "ba91ca008f20fde6bcabba8b62f7ec58c203a74cd23f489919f2176b9d4eda92"
        );
    }

    #[test]
    fn digests_round_trip_through_bytes_and_hex() {
        let digest = Sha256Digest::of(b"domain", b"bytes");

        assert_eq!(Sha256Digest::from_slice(digest.as_bytes()), Ok(digest));
        assert_eq!(Sha256Digest::from_hex(&digest.to_hex()), Ok(digest));
        assert_eq!(
            Sha256Digest::from_hex(&digest.to_hex().to_uppercase()),
            Ok(digest)
        );
        assert_eq!(digest.to_string(), digest.to_hex());
        assert!(format!("{digest:?}").contains(&digest.to_hex()));
    }

    #[test]
    fn malformed_digests_are_rejected() {
        assert_eq!(
            Sha256Digest::from_slice(&[0; 31]).unwrap_err().to_string(),
            "a SHA-256 digest has 32 bytes, got 31"
        );
        for invalid in ["", "00", &"g".repeat(64), &"0".repeat(65)] {
            assert_eq!(Sha256Digest::from_hex(invalid), Err(DigestHexError));
        }
    }
}
