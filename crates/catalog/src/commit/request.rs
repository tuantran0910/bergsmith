//! The exact `CommitTableRequest` bytes Bergsmith sends and persists.
//!
//! iceberg-rust's snapshot summary is a `HashMap`, so serializing the same
//! updates twice can order summary keys differently. The serialized body is
//! therefore the durable artifact: it is hashed and persisted once, and a
//! permitted replay sends the persisted bytes, never a re-serialization.

use bergsmith_common::digest::Sha256Digest;
use iceberg::{TableIdent, TableRequirement, TableUpdate};
use iceberg_catalog_rest::CommitTableRequest;

use crate::commit::error::ProposalError;
use crate::connection::TableName;

/// The hash domain of an exact commit request body.
const REQUEST_DOMAIN: &[u8] = b"bergsmith.catalog.v1.CommitTableRequest";

/// A serialized commit request for one table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitRequest {
    table: TableName,
    body: Vec<u8>,
    sha256: Sha256Digest,
}

impl CommitRequest {
    /// Serializes a request.
    pub(crate) fn encode(
        table: &TableName,
        identifier: &TableIdent,
        requirements: Vec<TableRequirement>,
        updates: Vec<TableUpdate>,
    ) -> Result<Self, ProposalError> {
        let request = CommitTableRequest {
            identifier: Some(identifier.clone()),
            requirements,
            updates,
        };
        let body = serde_json::to_vec(&request).map_err(ProposalError::Encode)?;
        Ok(Self::from_persisted(table.clone(), body))
    }

    /// Restores a request from its persisted body, for a permitted replay.
    #[must_use]
    pub fn from_persisted(table: TableName, body: Vec<u8>) -> Self {
        let sha256 = Sha256Digest::of(REQUEST_DOMAIN, &body);
        Self {
            table,
            body,
            sha256,
        }
    }

    /// The table the request is sent to.
    #[must_use]
    pub fn table(&self) -> &TableName {
        &self.table
    }

    /// The exact JSON body.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// The hash of the exact body, persisted with the intent.
    #[must_use]
    pub fn sha256(&self) -> Sha256Digest {
        self.sha256
    }

    /// Decodes the body, to inspect what the request asserts and changes.
    ///
    /// # Errors
    ///
    /// [`ProposalError::Encode`] when the body is not a commit request.
    pub fn decode(&self) -> Result<CommitTableRequest, ProposalError> {
        serde_json::from_slice(&self.body).map_err(ProposalError::Encode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> TableName {
        TableName {
            namespace: vec!["db".to_owned()],
            name: "t".to_owned(),
        }
    }

    #[test]
    fn the_request_hash_covers_the_exact_body() {
        let a = CommitRequest::from_persisted(table(), br#"{"a":1}"#.to_vec());
        let same = CommitRequest::from_persisted(table(), br#"{"a":1}"#.to_vec());
        let reordered = CommitRequest::from_persisted(table(), br#"{ "a":1}"#.to_vec());

        assert_eq!(a.sha256(), same.sha256());
        assert_ne!(a.sha256(), reordered.sha256());
        assert_eq!(
            a.sha256(),
            Sha256Digest::of(b"bergsmith.catalog.v1.CommitTableRequest", br#"{"a":1}"#)
        );
    }

    #[test]
    fn encoded_requests_decode_to_the_same_requirements() {
        let ident = TableIdent::from_strs(["db", "t"]).unwrap();
        let requirements = vec![TableRequirement::DefaultSpecIdMatch { default_spec_id: 0 }];

        let request =
            CommitRequest::encode(&table(), &ident, requirements.clone(), Vec::new()).unwrap();
        let decoded = request.decode().unwrap();

        assert_eq!(decoded.identifier, Some(ident));
        assert_eq!(decoded.requirements, requirements);
        assert!(
            CommitRequest::from_persisted(table(), b"not json".to_vec())
                .decode()
                .is_err()
        );
    }
}
