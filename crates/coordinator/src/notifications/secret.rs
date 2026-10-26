//! Signing-secret resolution (BS-029.T1, ADR 0060).
//!
//! A webhook destination stores a reference: a bare file name under
//! `notifications.secrets_dir`. The key is read when a request is sent, so
//! rotating it needs no database write, and the value never enters the ledger,
//! an API response, or a log.

use std::io;
use std::path::Path;

use bergsmith_common::notifications::SecretRef;
use bergsmith_common::secret::Secret;
use tokio::io::AsyncReadExt;

/// The largest accepted signing secret, in bytes.
pub const MAX_SECRET_BYTES: u64 = 4096;

/// Why a signing secret could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// `notifications.secrets_dir` is not configured, so no reference can
    /// resolve.
    #[error("notifications.secrets_dir is not configured")]
    NotConfigured,
    /// The stored reference is not a safe file name.
    #[error("signing secret reference is invalid")]
    InvalidReference,
    /// The file could not be read.
    #[error("signing secret is unavailable: {source}")]
    Unavailable {
        /// The underlying I/O error, which names no secret value.
        #[source]
        source: io::Error,
    },
    /// The file was empty after trimming a trailing newline.
    #[error("signing secret is empty")]
    Empty,
    /// The file was not UTF-8, which a shared key must be to round-trip.
    #[error("signing secret is not utf-8")]
    NotUtf8,
    /// The file exceeded [`MAX_SECRET_BYTES`].
    #[error("signing secret is larger than {max} bytes")]
    TooLarge {
        /// The accepted maximum.
        max: u64,
    },
}

/// Reads the signing secret named by `reference`.
///
/// One trailing newline (LF or CRLF) is stripped, because keys are commonly
/// created with an editor that adds one; every other byte is part of the key.
///
/// # Errors
///
/// [`SecretError`] when the directory is unconfigured, the reference is not a
/// bare file name, the file is missing, empty, too large, or not UTF-8.
pub async fn read_secret(
    secrets_dir: Option<&Path>,
    reference: &str,
) -> Result<Secret, SecretError> {
    let dir = secrets_dir.ok_or(SecretError::NotConfigured)?;
    let name = SecretRef::parse(reference).map_err(|_| SecretError::InvalidReference)?;
    let path = dir.join(name.as_str());
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|source| SecretError::Unavailable { source })?;
    let mut bytes = Vec::new();
    file.take(MAX_SECRET_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|source| SecretError::Unavailable { source })?;
    if bytes.len() as u64 > MAX_SECRET_BYTES {
        return Err(SecretError::TooLarge {
            max: MAX_SECRET_BYTES,
        });
    }
    let text = String::from_utf8(bytes).map_err(|_| SecretError::NotUtf8)?;
    let trimmed = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(&text);
    if trimmed.is_empty() {
        return Err(SecretError::Empty);
    }
    Ok(Secret::new(trimmed))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bergsmith-notifications-{name}-{}",
            std::process::id()
        ));
        tokio::fs::create_dir_all(&dir).await.expect("temp dir");
        dir
    }

    #[tokio::test]
    async fn a_secret_file_is_read_with_one_trailing_newline_removed() {
        let dir = temp_dir("read").await;
        tokio::fs::write(dir.join("key.pem"), "whsec_test\r\n")
            .await
            .expect("write");
        let secret = read_secret(Some(&dir), "key.pem").await.expect("secret");
        assert_eq!(secret.expose(), "whsec_test");
        tokio::fs::remove_dir_all(dir).await.expect("cleanup");
    }

    #[tokio::test]
    async fn an_unconfigured_directory_fails_closed() {
        assert!(matches!(
            read_secret(None, "key").await,
            Err(SecretError::NotConfigured)
        ));
    }

    #[tokio::test]
    async fn a_reference_can_never_leave_the_directory() {
        let dir = temp_dir("escape").await;
        tokio::fs::write(dir.join("key"), "value")
            .await
            .expect("write");
        for reference in ["../key", "a/b"] {
            assert!(matches!(
                read_secret(Some(&dir), reference).await,
                Err(SecretError::InvalidReference)
            ));
        }
        tokio::fs::remove_dir_all(dir).await.expect("cleanup");
    }

    #[tokio::test]
    async fn missing_and_empty_files_are_errors() {
        let dir = temp_dir("missing").await;
        assert!(matches!(
            read_secret(Some(&dir), "absent").await,
            Err(SecretError::Unavailable { .. })
        ));
        tokio::fs::write(dir.join("empty"), "")
            .await
            .expect("write");
        assert!(matches!(
            read_secret(Some(&dir), "empty").await,
            Err(SecretError::Empty)
        ));
        tokio::fs::remove_dir_all(dir).await.expect("cleanup");
    }
}
