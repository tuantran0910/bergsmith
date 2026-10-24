use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bergsmith_common::ids::{OrganizationId, PrincipalId};
use serde::{Deserialize, Serialize};

use super::ApiError;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PageQuery {
    limit: Option<usize>,
    cursor: Option<String>,
}

impl PageQuery {
    /// Builds a page query from a handler that parses the same two fields
    /// beside its own filters.
    pub(super) const fn new(limit: Option<usize>, cursor: Option<String>) -> Self {
        Self { limit, cursor }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u8,
    organization: OrganizationId,
    principal: PrincipalId,
    resource: String,
    after: String,
}

pub(super) struct Page {
    pub(super) limit: usize,
    pub(super) after: Option<String>,
    organization: OrganizationId,
    principal: PrincipalId,
    resource: &'static str,
}

impl Page {
    pub(super) fn parse(
        query: PageQuery,
        organization: OrganizationId,
        principal: PrincipalId,
        resource: &'static str,
    ) -> Result<Self, ApiError> {
        let limit = query.limit.unwrap_or(50);
        if !(1..=100).contains(&limit) {
            return Err(ApiError::invalid());
        }
        let after = query
            .cursor
            .map(|raw| {
                if raw.len() > 1024 {
                    return Err(ApiError::invalid());
                }
                let bytes = URL_SAFE_NO_PAD
                    .decode(raw)
                    .map_err(|_| ApiError::invalid())?;
                let cursor: Cursor =
                    serde_json::from_slice(&bytes).map_err(|_| ApiError::invalid())?;
                if cursor.version != 1
                    || cursor.organization != organization
                    || cursor.principal != principal
                    || cursor.resource != resource
                {
                    return Err(ApiError::invalid());
                }
                Ok(cursor.after)
            })
            .transpose()?;
        Ok(Self {
            limit,
            after,
            organization,
            principal,
            resource,
        })
    }

    pub(super) fn finish(
        &self,
        mut rows: Vec<(String, serde_json::Value)>,
    ) -> Result<serde_json::Value, ApiError> {
        let more = rows.len() > self.limit;
        rows.truncate(self.limit);
        let next_cursor = if more {
            rows.last()
                .map(|(after, _)| {
                    serde_json::to_vec(&Cursor {
                        version: 1,
                        organization: self.organization,
                        principal: self.principal,
                        resource: self.resource.into(),
                        after: after.clone(),
                    })
                    .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
                    .map_err(|_| ApiError::unavailable())
                })
                .transpose()?
        } else {
            None
        };
        Ok(
            serde_json::json!({ "items": rows.into_iter().map(|(_, value)| value).collect::<Vec<_>>(), "next_cursor": next_cursor }),
        )
    }
}
