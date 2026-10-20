//! Random, URL-safe tokens and their domain-separated verifiers.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bergsmith_common::digest::Sha256Digest;
use ring::rand::{SecureRandom, SystemRandom};

/// 32 random bytes: 43 base64url characters without padding.
pub(crate) const TOKEN_BYTES: usize = 32;

/// Returns a fresh 32-byte base64url token.
pub(crate) fn token() -> Option<String> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    SystemRandom::new().fill(&mut bytes).ok()?;
    Some(URL_SAFE_NO_PAD.encode(bytes))
}

/// Parses a base64url token and returns exactly 32 bytes.
pub(crate) fn token_bytes(value: &str) -> Option<[u8; TOKEN_BYTES]> {
    if value.len() != 43 {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(value).ok()?;
    <[u8; TOKEN_BYTES]>::try_from(bytes.as_slice()).ok()
}

/// A domain-separated digest of the token's bytes.
pub(crate) fn verifier(domain: &[u8], value: &str) -> Sha256Digest {
    Sha256Digest::of(domain, value.as_bytes())
}

/// Constant-time equality for two digests.
pub(crate) fn digests_equal(a: &Sha256Digest, b: &Sha256Digest) -> bool {
    constant_time_eq(a.as_bytes(), b.as_bytes())
}

/// Constant-time equality for two byte slices of equal length.
///
/// The length comparison is not secret: the coordinator always compares a
/// presented 32-byte digest or a fixed-length nonce with its expected value.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0_u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_43_url_safe_characters_and_round_trip() {
        let value = token().unwrap();
        assert_eq!(value.len(), 43);
        assert!(token_bytes(&value).is_some());
        assert!(
            value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        );
    }

    #[test]
    fn malformed_token_text_is_rejected() {
        assert!(token_bytes("short").is_none());
        assert!(token_bytes(&"!".repeat(43)).is_none());
        assert!(token_bytes("").is_none());
    }

    #[test]
    fn verifiers_are_domain_separated() {
        let value = "a".repeat(43);
        assert_ne!(
            verifier(b"bergsmith.session-token.v1", &value),
            verifier(b"bergsmith.csrf-token.v1", &value)
        );
    }
}
