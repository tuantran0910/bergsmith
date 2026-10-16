//! A redacting wrapper for credentials and tokens.

use std::fmt;

use zeroize::Zeroize;

/// A secret string: a password, access key secret, session token, or OAuth
/// client secret.
///
/// `Debug` prints `Secret(<redacted>)`, there is no `Display` or
/// serialization, and the bytes are zeroed on drop. Reading the value needs
/// an explicit [`expose`](Self::expose), which makes every use reviewable.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret value. Never log it or copy it into errors, artifacts, or
    /// the database.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_is_redacted() {
        let secret = Secret::new("hunter2");
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        assert_eq!(format!("{:?}", Some(&secret)), "Some(Secret(<redacted>))");
    }

    #[test]
    fn the_value_is_available_only_through_expose() {
        let secret = Secret::new(String::from("hunter2"));
        assert_eq!(secret.expose(), "hunter2");
        assert_eq!(secret.clone(), secret);
    }
}
