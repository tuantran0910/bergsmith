//! Redacted `Debug` for messages that carry secrets.

use std::fmt;

use crate::cluster::StorageCredentials;

/// Shows the scope, property names, and expiry; never property values.
impl fmt::Debug for StorageCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StorageCredentials")
            .field("prefix", &self.prefix)
            .field("properties", &self.properties.keys().collect::<Vec<_>>())
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_omits_credential_values() {
        let credentials = StorageCredentials {
            prefix: "s3://bucket/table".to_owned(),
            properties: [("s3.secret-access-key".to_owned(), "hunter2".to_owned())].into(),
            expires_at: None,
        };

        let debug = format!("{credentials:?}");

        assert!(debug.contains("s3.secret-access-key"));
        assert!(!debug.contains("hunter2"));
    }
}
