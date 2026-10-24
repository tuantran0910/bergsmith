//! Worker service identity (ADR 0012).
//!
//! Workers authenticate every `bergsmith.cluster.v1` call with a pool-scoped
//! bearer token in the `authorization` metadata, over TLS. A token reads
//! `bsw1_<pool>_<secret>`, where `<pool>` is the worker pool name and
//! `<secret>` is 43 base64url characters (32 random bytes). The coordinator
//! stores only the token's [`verifier`](ServiceToken::verifier) and compares
//! verifiers in constant time.

use std::fmt;

use tonic::metadata::MetadataMap;

use crate::hashing::{Sha256Digest, domain_digest};

/// The gRPC metadata key that carries the token.
pub const AUTHORIZATION_METADATA_KEY: &str = "authorization";

const PREFIX: &str = "bsw1";
const SECRET_LEN: usize = 43;
const VERIFIER_DOMAIN: &[u8] = b"bergsmith.service-token.v1";

/// A worker pool's bearer token. `Debug` shows only the pool; the token has
/// no `Display`, so it cannot be logged by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct ServiceToken {
    token: String,
    pool_len: usize,
}

/// Why a token was rejected. Messages never include the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// The request carried no `authorization` metadata.
    #[error("missing authorization metadata")]
    Missing,
    /// The metadata is not `Bearer <token>`.
    #[error("authorization metadata is not a bearer token")]
    NotBearer,
    /// The token does not start with the `bsw1_` version prefix.
    #[error("service token has an unknown version prefix")]
    UnknownVersion,
    /// The pool name is empty or not lowercase letters, digits, and hyphens.
    #[error("service token has an invalid pool name")]
    InvalidPool,
    /// The secret is not 43 base64url characters.
    #[error("service token has a malformed secret")]
    InvalidSecret,
}

impl ServiceToken {
    /// Parses and validates a token.
    ///
    /// # Errors
    ///
    /// A [`TokenError`] naming the malformed part.
    pub fn parse(token: &str) -> Result<Self, TokenError> {
        let rest = token
            .strip_prefix(PREFIX)
            .and_then(|rest| rest.strip_prefix('_'))
            .ok_or(TokenError::UnknownVersion)?;
        // Pool names have no `_`, so the first one ends the pool; the secret
        // may itself contain `_`.
        let (pool, secret) = rest.split_once('_').ok_or(TokenError::InvalidPool)?;
        if !valid_pool(pool) {
            return Err(TokenError::InvalidPool);
        }
        if secret.len() != SECRET_LEN
            || !secret
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(TokenError::InvalidSecret);
        }
        Ok(Self {
            token: token.to_owned(),
            pool_len: pool.len(),
        })
    }

    /// Reads the token from `authorization: Bearer <token>` metadata.
    ///
    /// # Errors
    ///
    /// [`TokenError::Missing`] or [`TokenError::NotBearer`] for absent or
    /// malformed metadata, and the [`parse`](Self::parse) errors otherwise.
    pub fn from_metadata(metadata: &MetadataMap) -> Result<Self, TokenError> {
        let value = metadata
            .get(AUTHORIZATION_METADATA_KEY)
            .ok_or(TokenError::Missing)?
            .to_str()
            .map_err(|_| TokenError::NotBearer)?;
        let token = value.strip_prefix("Bearer ").ok_or(TokenError::NotBearer)?;
        Self::parse(token)
    }

    /// The worker pool the token belongs to.
    #[must_use]
    pub fn pool(&self) -> &str {
        &self.token[PREFIX.len() + 1..PREFIX.len() + 1 + self.pool_len]
    }

    /// The `authorization` metadata value, `Bearer <token>`. This exposes the
    /// secret: send it only over TLS and never log it.
    #[must_use]
    pub fn authorization_value(&self) -> String {
        format!("Bearer {}", self.token)
    }

    /// The bearer token without the `Bearer ` prefix.
    ///
    /// This exposes the secret. Use it only to return a newly issued token to
    /// its operator once; never log it, persist it, or put it in an artifact.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.token
    }

    /// The value the coordinator stores instead of the token.
    #[must_use]
    pub fn verifier(&self) -> Sha256Digest {
        domain_digest(VERIFIER_DOMAIN, self.token.as_bytes())
    }

    /// Checks the token against a stored verifier in constant time.
    #[must_use]
    pub fn matches(&self, verifier: &Sha256Digest) -> bool {
        let actual = self.verifier();
        actual
            .as_bytes()
            .iter()
            .zip(verifier.as_bytes())
            .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
            == 0
    }
}

/// Whether `pool` is a valid token pool name: 1 to 63 lowercase letters,
/// digits, and hyphens, not starting with a hyphen.
#[must_use]
pub fn valid_pool(pool: &str) -> bool {
    let bytes = pool.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && bytes[0] != b'-'
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

impl fmt::Debug for ServiceToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceToken")
            .field("pool", &self.pool())
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use tonic::metadata::MetadataValue;

    use super::*;

    const SECRET: &str = "abcdefghijklmnopqrstuvwxyz_ABCDEFGHIJKLMN-0";

    fn token(pool: &str) -> String {
        format!("bsw1_{pool}_{SECRET}")
    }

    #[test]
    fn a_well_formed_token_parses_and_names_its_pool() {
        let parsed = ServiceToken::parse(&token("compaction-a")).unwrap();
        assert_eq!(parsed.pool(), "compaction-a");
        assert_eq!(
            parsed.authorization_value(),
            format!("Bearer {}", token("compaction-a"))
        );
    }

    #[test]
    fn malformed_tokens_are_rejected_with_the_reason() {
        let cases = [
            ("bsw2_pool_x", TokenError::UnknownVersion),
            ("pool_x", TokenError::UnknownVersion),
            ("bsw1_", TokenError::InvalidPool),
            (&format!("bsw1__{SECRET}"), TokenError::InvalidPool),
            (&format!("bsw1_Pool_{SECRET}"), TokenError::InvalidPool),
            (&format!("bsw1_-pool_{SECRET}"), TokenError::InvalidPool),
            ("bsw1_pool_short", TokenError::InvalidSecret),
            (
                &format!("bsw1_pool_{}", "!".repeat(43)),
                TokenError::InvalidSecret,
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(ServiceToken::parse(input), Err(expected), "{input}");
        }
    }

    #[test]
    fn tokens_are_read_from_bearer_metadata() {
        let mut metadata = MetadataMap::new();
        assert_eq!(
            ServiceToken::from_metadata(&metadata),
            Err(TokenError::Missing)
        );

        metadata.insert(
            AUTHORIZATION_METADATA_KEY,
            MetadataValue::from_static("Basic abc"),
        );
        assert_eq!(
            ServiceToken::from_metadata(&metadata),
            Err(TokenError::NotBearer)
        );

        let value = ServiceToken::parse(&token("pool"))
            .unwrap()
            .authorization_value();
        metadata.insert(AUTHORIZATION_METADATA_KEY, value.parse().unwrap());
        assert_eq!(
            ServiceToken::from_metadata(&metadata).unwrap().pool(),
            "pool"
        );
    }

    #[test]
    fn verifiers_match_only_the_same_token() {
        let a = ServiceToken::parse(&token("pool")).unwrap();
        let b = ServiceToken::parse(&token("other")).unwrap();

        assert!(a.matches(&a.verifier()));
        assert!(!a.matches(&b.verifier()));
        assert_ne!(a.verifier().to_hex(), SECRET);
    }

    #[test]
    fn debug_output_redacts_the_secret() {
        let debug = format!("{:?}", ServiceToken::parse(&token("pool")).unwrap());
        assert!(debug.contains("pool"));
        assert!(!debug.contains(SECRET));
    }
}
