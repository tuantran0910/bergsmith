//! Table previews (BS-018.T2).
//!
//! A preview is a read: it creates no job, plan, output, or idempotency key.
//! It reports the baseline it assessed, why the table cannot be maintained, or
//! the bins and estimate a job would be built from. The response carries no
//! metadata locations, credentials, or catalog connection settings; per-file
//! rejection paths are included because they explain a refusal.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use bergsmith_common::disposition::FailureDisposition;
use bergsmith_common::ids::TableId;
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_maintenance::{
    BinDisposition, BinPreview, DeferReason, FileTally, NoOpReason, PlanError, Preview, Rejection,
};
use bergsmith_persistence::tables;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiError, ApiState, Caller, json_body, resources};
use crate::preview::{AssessedPreview, PreviewError, PreviewTarget};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PreviewRequest {
    table_id: TableId,
}

/// Assesses a table without creating work.
///
/// The table's canonical identity and the caller's grants are resolved before
/// any catalog request, and the catalog name is chosen among connections the
/// caller's grant covers. A catalog name the caller may not use is never
/// contacted.
pub(super) async fn preview(
    State(state): State<ApiState>,
    caller: Caller,
    body: Result<Json<PreviewRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let request = json_body(body)?;
    let target = resolve(&state, &caller, request.table_id, Permission::SubmitJobs).await?;
    let assessed = state
        .previews
        .preview_target(caller.principal_id, &target)
        .await
        .map_err(|error| failed(request.table_id, &error))?;
    Ok(Json(report(request.table_id, &assessed)))
}

/// Resolves the catalog name and connection the caller may read `table` from
/// under `permission`.
///
/// Existence is checked first, so an unknown table is 404 while a known table
/// the caller lacks `permission` on is 403 and audited. Every step is a ledger
/// read; the connection is released before any catalog I/O.
pub(super) async fn resolve(
    state: &ApiState,
    caller: &Caller,
    table: TableId,
    permission: Permission,
) -> Result<PreviewTarget, ApiError> {
    let mut conn = state.database.pool().acquire().await?;
    let identity = tables::table_identity(&mut conn, caller.organization_id, table).await?;
    resources::require(
        &mut conn,
        &caller.0,
        permission,
        ResourceScope::Table(table),
    )
    .await?;
    let roles: Vec<&str> = permission
        .roles()
        .iter()
        .map(|role| role.as_str())
        .collect();
    let aliases = tables::accessible_aliases_with_connections(
        &mut conn,
        caller.organization_id,
        caller.principal_id,
        &roles,
        table,
    )
    .await?;
    // The permission check and this query apply the same grant predicate, so
    // an empty result here would mean the grants changed mid-request.
    if !aliases.is_empty() && !aliases.iter().any(|target| target.connection.enabled) {
        return Err(ApiError::conflict(
            "catalog_connection_disabled",
            "enable_catalog_connection",
        ));
    }
    let Some(target) = aliases.into_iter().find(|target| target.connection.enabled) else {
        return Err(ApiError::not_found());
    };
    Ok(PreviewTarget {
        table_id: table,
        table_uuid: identity.table_uuid,
        alias: target.alias,
        connection: target.connection,
    })
}

/// Logs a failed catalog read and returns the typed API error.
///
/// Client mistakes are not logged; server-side failures and configuration
/// gaps are. The fields are the table, the stable reason, the sanitized error
/// kind, and an upstream HTTP status when there was one. The upstream message,
/// URLs, and table locations are never interpolated: they are catalog- or
/// Iceberg-controlled text.
pub(super) fn failed(table: TableId, error: &PreviewError) -> ApiError {
    let api = ApiError::classify(error);
    if api.status.is_server_error() || api.disposition == FailureDisposition::RetrySameRequest {
        tracing::warn!(
            table_id = %table,
            reason = api.reason,
            kind = error.kind(),
            upstream_status = ?error.upstream_status(),
            "api table read failed"
        );
    }
    api
}

/// The wire form of a preview.
fn report(table: TableId, assessed: &AssessedPreview) -> Value {
    let preview = &assessed.preview;
    let (outcome, no_op_reason) = outcome(preview);
    json!({
        "table_id": table,
        "outcome": outcome,
        "no_op_reason": no_op_reason,
        // Snapshot IDs use all 64 bits; a decimal string keeps browser clients
        // from rounding them, as audit event IDs do.
        "baseline": preview.baseline.as_ref().map(|baseline| json!({
            "snapshot_id": baseline.snapshot_id().to_string(),
            "sequence_number": baseline.sequence_number(),
        })),
        "rejections": preview.rejections.iter().map(rejection).collect::<Vec<_>>(),
        "candidates": tally(preview.candidates),
        "skipped": preview.skipped.iter().map(|(reason, tally)| json!({
            "reason": reason,
            "files": tally.files,
            "bytes": tally.bytes,
        })).collect::<Vec<_>>(),
        "bins": preview.bins.iter().map(bin).collect::<Vec<_>>(),
        "bins_total": assessed.bins_total,
        "bins_truncated": assessed.bins_total > preview.bins.len(),
        "estimate": preview.estimate,
    })
}

/// What a job would do, mirroring the planner's own outcome order.
fn outcome(preview: &Preview) -> (&'static str, Option<&'static str>) {
    if preview.baseline.is_none() {
        return ("no_op", Some(NoOpReason::NoSnapshot.as_str()));
    }
    if !preview.rejections.is_empty() {
        return ("rejected", None);
    }
    if preview.estimate.tasks == 0 {
        return ("no_op", Some(NoOpReason::NoBeneficialWork.as_str()));
    }
    ("planned", None)
}

fn tally(tally: FileTally) -> Value {
    json!({"files": tally.files, "bytes": tally.bytes})
}

fn bin(bin: &BinPreview) -> Value {
    json!({
        "partition": bin.partition,
        "ordinal": bin.ordinal,
        "input": tally(bin.input),
        "has_deletes": bin.has_deletes,
        "files_removed": bin.files_removed,
        "disposition": disposition_name(bin.disposition),
    })
}

/// A bin decision's stable name. Deferral names the limit it exceeded.
const fn disposition_name(disposition: BinDisposition) -> &'static str {
    match disposition {
        BinDisposition::Admitted => "admitted",
        BinDisposition::NoBenefit => "no_benefit",
        BinDisposition::Deferred(DeferReason::JobInput) => "deferred_job_input",
        BinDisposition::Deferred(DeferReason::JobFiles) => "deferred_job_files",
        BinDisposition::Deferred(DeferReason::Duration) => "deferred_duration",
    }
}

/// A rejection as one flat object, so the contract stays a single schema
/// rather than a provider-specific union.
fn rejection(rejection: &Rejection) -> Value {
    let mut value = json!({
        "reason": rejection_reason(rejection),
        "detail": rejection.to_string(),
        "path": null,
        "version": null,
        "sort_order_id": null,
        "format": null,
        "field_id": null,
        "problem": null,
    });
    match rejection {
        Rejection::FormatVersion { version } => value["version"] = json!(version),
        Rejection::SortedTable { sort_order_id } => value["sort_order_id"] = json!(sort_order_id),
        Rejection::Encrypted => {}
        Rejection::WriteFormat { format } => value["format"] = json!(format),
        Rejection::NonParquetFile { path, format } => {
            value["path"] = json!(path);
            value["format"] = json!(format);
        }
        Rejection::EqualityKey {
            path,
            field_id,
            problem,
        } => {
            value["path"] = json!(path);
            value["field_id"] = json!(field_id);
            value["problem"] = json!(problem);
        }
        Rejection::VoidSpecEqualityDelete { path } => value["path"] = json!(path),
    }
    value
}

/// A stable rejection name, matching the `Rejection` variants.
const fn rejection_reason(rejection: &Rejection) -> &'static str {
    match rejection {
        Rejection::FormatVersion { .. } => "format_version",
        Rejection::SortedTable { .. } => "sorted_table",
        Rejection::Encrypted => "encrypted",
        Rejection::WriteFormat { .. } => "write_format",
        Rejection::NonParquetFile { .. } => "non_parquet_file",
        Rejection::EqualityKey { .. } => "equality_key",
        Rejection::VoidSpecEqualityDelete { .. } => "void_spec_equality_delete",
    }
}

impl ApiError {
    /// Classifies a preview failure without logging; [`failed`] logs.
    fn classify(error: &PreviewError) -> Self {
        use bergsmith_storage::{ProfileError, StorageError};
        match error {
            PreviewError::IdentityChanged { .. } => Self::permanent(
                StatusCode::CONFLICT,
                "table_identity_changed",
                "refresh_inventory",
            ),
            PreviewError::CredentialUnavailable(_) => Self::permanent(
                StatusCode::INTERNAL_SERVER_ERROR,
                "catalog_credential_unavailable",
                "configure_catalog_credential",
            ),
            PreviewError::ConnectionConfig(..) => Self::permanent(
                StatusCode::INTERNAL_SERVER_ERROR,
                "catalog_connection_invalid",
                "fix_catalog_connection",
            ),
            PreviewError::Profile(
                ProfileError::NoProfile(_)
                | ProfileError::BackendNotEnabled(_)
                | ProfileError::MissingCredentials(_),
            )
            | PreviewError::Storage(StorageError::Profile(
                ProfileError::NoProfile(_)
                | ProfileError::BackendNotEnabled(_)
                | ProfileError::MissingCredentials(_),
            )) => Self::permanent(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_profile_unavailable",
                "configure_storage_profile",
            ),
            PreviewError::Storage(_) | PreviewError::Plan(PlanError::Iceberg(_)) => {
                Self::transient("storage_unavailable", "retry_same_request")
            }
            PreviewError::Catalog(error) => Self::classify_catalog(error),
            PreviewError::Overloaded => {
                Self::transient("preview_capacity_exhausted", "retry_same_request")
            }
            PreviewError::Plan(PlanError::InvalidSelection(_)) => Self::invalid(),
            PreviewError::Plan(PlanError::StagedTable { .. }) => {
                Self::conflict("table_staged", "write_first_snapshot")
            }
            PreviewError::Plan(PlanError::InventoryTooLarge { .. }) => Self::permanent(
                StatusCode::CONFLICT,
                "inventory_too_large",
                "reduce_table_or_raise_limit",
            ),
            PreviewError::Plan(PlanError::InvalidConfig(_) | PlanError::CorruptPlan(_)) => {
                Self::internal()
            }
            PreviewError::Plan(
                PlanError::MissingSnapshot { .. }
                | PlanError::UnboundSpec { .. }
                | PlanError::UnknownSpec { .. }
                | PlanError::PartitionMismatch { .. }
                | PlanError::MissingSequenceNumber { .. }
                | PlanError::DuplicateFile { .. }
                | PlanError::Location(_),
            ) => Self::permanent(
                StatusCode::CONFLICT,
                "table_metadata_inconsistent",
                "refresh_inventory",
            ),
        }
    }
}

impl From<PreviewError> for ApiError {
    fn from(error: PreviewError) -> Self {
        Self::classify(&error)
    }
}

impl ApiError {
    /// Classifies a catalog failure.
    ///
    /// A failure the operator must fix — bad credentials or configuration, a
    /// catalog without the required endpoints, or missing credential vending —
    /// is permanent and names the action. Only transport failures, timeouts,
    /// throttling, undecodable bodies, and upstream server errors are retryable.
    fn classify_catalog(error: &bergsmith_catalog::CatalogError) -> Self {
        use bergsmith_catalog::CatalogError;
        match error {
            CatalogError::Status { status: 404, .. } => Self::new(
                StatusCode::NOT_FOUND,
                "catalog_table_missing",
                "refresh_inventory",
            ),
            CatalogError::Status {
                status: 401 | 403 | 419,
                ..
            } => Self::permanent(
                StatusCode::INTERNAL_SERVER_ERROR,
                "catalog_access_denied",
                "check_catalog_credentials",
            ),
            CatalogError::Status {
                status: 408 | 425 | 429,
                ..
            }
            | CatalogError::Authentication {
                status: 408 | 425 | 429,
            } => Self::transient("catalog_unavailable", "retry_same_request"),
            CatalogError::Status { status, .. } if *status >= 500 => {
                Self::transient("catalog_unavailable", "retry_same_request")
            }
            CatalogError::Status { .. } => Self::permanent(
                StatusCode::INTERNAL_SERVER_ERROR,
                "catalog_request_rejected",
                "check_catalog_compatibility",
            ),
            CatalogError::Authentication { status } if *status >= 500 => {
                Self::transient("catalog_unavailable", "retry_same_request")
            }
            CatalogError::Authentication { .. } => Self::permanent(
                StatusCode::INTERNAL_SERVER_ERROR,
                "catalog_access_denied",
                "check_catalog_credentials",
            ),
            CatalogError::MissingEndpoints(_) | CatalogError::IdempotencyUnsupported => {
                Self::permanent(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "catalog_unsupported",
                    "check_catalog_compatibility",
                )
            }
            CatalogError::Credentials(_) | CatalogError::NoVendedCredentials => Self::permanent(
                StatusCode::INTERNAL_SERVER_ERROR,
                "catalog_credentials_unavailable",
                "configure_catalog_credential_vending",
            ),
            CatalogError::InvalidUrl(_) => Self::permanent(
                StatusCode::INTERNAL_SERVER_ERROR,
                "catalog_connection_invalid",
                "fix_catalog_connection",
            ),
            CatalogError::InvalidLocation(_) | CatalogError::DataLocationOutsideTable(_) => {
                Self::permanent(
                    StatusCode::CONFLICT,
                    "table_metadata_inconsistent",
                    "fix_table_location",
                )
            }
            CatalogError::Transport { .. } | CatalogError::Decode { .. } => {
                Self::transient("catalog_unavailable", "retry_same_request")
            }
            CatalogError::ResponseTooLarge { .. } => Self::permanent(
                StatusCode::INTERNAL_SERVER_ERROR,
                "catalog_response_too_large",
                "check_catalog_configuration",
            ),
            CatalogError::ListingOverflow { .. } => Self::permanent(
                StatusCode::INTERNAL_SERVER_ERROR,
                "catalog_page_too_large",
                "check_catalog_limits",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bergsmith_maintenance::KeyProblem;

    #[test]
    fn bin_dispositions_have_stable_names() {
        for (disposition, name) in [
            (BinDisposition::Admitted, "admitted"),
            (BinDisposition::NoBenefit, "no_benefit"),
            (
                BinDisposition::Deferred(DeferReason::JobInput),
                "deferred_job_input",
            ),
            (
                BinDisposition::Deferred(DeferReason::JobFiles),
                "deferred_job_files",
            ),
            (
                BinDisposition::Deferred(DeferReason::Duration),
                "deferred_duration",
            ),
        ] {
            assert_eq!(disposition_name(disposition), name);
        }
    }

    #[test]
    fn rejections_flatten_to_their_documented_fields() {
        let format = rejection(&Rejection::FormatVersion { version: 3 });
        assert_eq!(format["reason"], "format_version");
        assert_eq!(format["version"], 3);
        assert!(format["path"].is_null());

        let equality = rejection(&Rejection::EqualityKey {
            path: "s3://warehouse/t/data/f.parquet".to_owned(),
            field_id: 7,
            problem: KeyProblem::Nested,
        });
        assert_eq!(equality["reason"], "equality_key");
        assert_eq!(equality["field_id"], 7);
        assert_eq!(equality["problem"], "nested");
        assert_eq!(equality["path"], "s3://warehouse/t/data/f.parquet");

        let sorted = rejection(&Rejection::SortedTable { sort_order_id: 4 });
        assert_eq!(sorted["reason"], "sorted_table");
        assert_eq!(sorted["sort_order_id"], 4);

        let write = rejection(&Rejection::WriteFormat {
            format: "orc".to_owned(),
        });
        assert_eq!(write["reason"], "write_format");
        assert_eq!(write["format"], "orc");

        let file = rejection(&Rejection::NonParquetFile {
            path: "s3://warehouse/t/data/f.orc".to_owned(),
            format: "orc".to_owned(),
        });
        assert_eq!(file["reason"], "non_parquet_file");
        assert_eq!(file["format"], "orc");

        let void = rejection(&Rejection::VoidSpecEqualityDelete {
            path: "s3://warehouse/t/data/d.parquet".to_owned(),
        });
        assert_eq!(void["reason"], "void_spec_equality_delete");
        assert_eq!(void["path"], "s3://warehouse/t/data/d.parquet");

        assert_eq!(rejection(&Rejection::Encrypted)["reason"], "encrypted");
        assert!(rejection(&Rejection::Encrypted)["detail"].is_string());
    }

    #[test]
    fn documented_enums_match_the_rust_names() {
        use bergsmith_maintenance::{KeyProblem, SkipReason};

        let document: Value = serde_json::from_str(include_str!("openapi.json")).unwrap();
        let documented = |pointer: &str| -> Vec<Value> {
            document
                .pointer(pointer)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_else(|| panic!("missing enum at {pointer}"))
        };

        let reasons = documented("/components/schemas/Rejection/properties/reason/enum");
        for rejection in [
            Rejection::FormatVersion { version: 3 },
            Rejection::SortedTable { sort_order_id: 1 },
            Rejection::Encrypted,
            Rejection::WriteFormat {
                format: "orc".to_owned(),
            },
            Rejection::NonParquetFile {
                path: "p".to_owned(),
                format: "orc".to_owned(),
            },
            Rejection::EqualityKey {
                path: "p".to_owned(),
                field_id: 1,
                problem: KeyProblem::Nested,
            },
            Rejection::VoidSpecEqualityDelete {
                path: "p".to_owned(),
            },
        ] {
            let name = json!(rejection_reason(&rejection));
            assert!(reasons.contains(&name), "undocumented rejection {name}");
        }

        let skipped = documented("/components/schemas/Skipped/properties/reason/enum");
        for reason in [
            SkipReason::OutsideSelection,
            SkipReason::TooLarge,
            SkipReason::AtTargetSize,
        ] {
            let name = serde_json::to_value(reason).unwrap();
            assert!(skipped.contains(&name), "undocumented skip reason {name}");
        }

        let problems = documented("/components/schemas/Rejection/properties/problem/anyOf/0/enum");
        for problem in [
            KeyProblem::FloatingPoint,
            KeyProblem::Nested,
            KeyProblem::NotInSchema,
        ] {
            let name = serde_json::to_value(problem).unwrap();
            assert!(problems.contains(&name), "undocumented key problem {name}");
        }

        let dispositions = documented("/components/schemas/Bin/properties/disposition/enum");
        for disposition in [
            BinDisposition::Admitted,
            BinDisposition::NoBenefit,
            BinDisposition::Deferred(DeferReason::JobInput),
            BinDisposition::Deferred(DeferReason::JobFiles),
            BinDisposition::Deferred(DeferReason::Duration),
        ] {
            let name = json!(disposition_name(disposition));
            assert!(
                dispositions.contains(&name),
                "undocumented disposition {name}"
            );
        }
    }

    #[test]
    fn failure_classification_is_stable() {
        let table = TableId::generate();
        let error = PreviewError::IdentityChanged {
            table,
            expected: uuid::Uuid::now_v7(),
            observed: uuid::Uuid::now_v7(),
        };
        let api = ApiError::from(error);
        assert_eq!(api.status, StatusCode::CONFLICT);
        assert_eq!(api.reason, "table_identity_changed");
        assert_eq!(api.disposition, FailureDisposition::PermanentFailure);

        let api = ApiError::from(PreviewError::Overloaded);
        assert_eq!(api.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(api.reason, "preview_capacity_exhausted");
        assert_eq!(api.disposition, FailureDisposition::RetrySameRequest);

        let api = ApiError::from(PreviewError::Plan(PlanError::InventoryTooLarge {
            limit: 10,
        }));
        assert_eq!(api.status, StatusCode::CONFLICT);
        assert_eq!(api.reason, "inventory_too_large");
        assert_eq!(api.disposition, FailureDisposition::PermanentFailure);
    }

    #[test]
    fn catalog_configuration_failures_are_permanent_and_transient_ones_retry() {
        use bergsmith_catalog::CatalogError;
        use bergsmith_storage::CredentialError;

        let failure = |error: CatalogError| ApiError::from(PreviewError::Catalog(error));
        let status = |operation: &'static str, status: u16| CatalogError::Status {
            operation,
            status,
            model: None,
        };

        // The operator must change something; repeating the request cannot work.
        for (error, reason) in [
            (
                CatalogError::NoVendedCredentials,
                "catalog_credentials_unavailable",
            ),
            (
                CatalogError::Credentials(CredentialError::MissingProperty("s3.access-key-id")),
                "catalog_credentials_unavailable",
            ),
            (
                CatalogError::MissingEndpoints(vec!["GET /v1/config".to_owned()]),
                "catalog_unsupported",
            ),
            (
                CatalogError::InvalidUrl("mailto:not-a-catalog".to_owned()),
                "catalog_connection_invalid",
            ),
            (status("GET load table", 401), "catalog_access_denied"),
            (
                CatalogError::Authentication { status: 403 },
                "catalog_access_denied",
            ),
            (status("GET load table", 400), "catalog_request_rejected"),
            (
                CatalogError::InvalidLocation(bergsmith_storage::LocationError::UnsupportedScheme(
                    "mem".to_owned(),
                )),
                "table_metadata_inconsistent",
            ),
            (
                CatalogError::DataLocationOutsideTable("s3://elsewhere/t".to_owned()),
                "table_metadata_inconsistent",
            ),
        ] {
            let api = failure(error);
            assert_eq!(
                api.disposition,
                FailureDisposition::PermanentFailure,
                "{reason}"
            );
            assert_eq!(api.reason, reason);
            assert!(api.status.is_server_error() || api.status == StatusCode::CONFLICT);
        }

        // Upstream again later: retry the same request.
        for error in [
            status("GET load table", 503),
            status("GET load table", 429),
            status("GET load table", 408),
            CatalogError::Authentication { status: 503 },
        ] {
            let api = failure(error);
            assert_eq!(api.disposition, FailureDisposition::RetrySameRequest);
            assert_eq!(api.reason, "catalog_unavailable");
            assert_eq!(api.status, StatusCode::SERVICE_UNAVAILABLE);
        }
    }

    #[test]
    fn preview_logs_never_include_upstream_error_text() {
        use std::fmt;
        use std::sync::{Arc, Mutex};

        use bergsmith_catalog::{CatalogError, CatalogErrorModel};
        use tracing::field::{Field, Visit};
        use tracing_subscriber::layer::{Context, SubscriberExt};

        const SENTINEL: &str = "SENSITIVE-UPSTREAM-SENTINEL";

        #[derive(Clone, Default)]
        struct Capture(Arc<Mutex<Vec<String>>>);

        struct Fields<'a>(&'a mut Vec<String>);

        impl Visit for Fields<'_> {
            fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
                self.0.push(format!("{field}={value:?}"));
            }
        }

        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
            fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
                let mut fields = Vec::new();
                event.record(&mut Fields(&mut fields));
                self.0.lock().expect("capture lock").extend(fields);
            }
        }

        let error = PreviewError::Catalog(CatalogError::Status {
            operation: "GET load table",
            status: 500,
            model: Some(CatalogErrorModel {
                kind: "Exception".to_owned(),
                message: format!("{SENTINEL} at s3://internal/secret-path"),
            }),
        });
        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        tracing::subscriber::with_default(subscriber, || {
            let api = failed(TableId::generate(), &error);
            assert_eq!(api.reason, "catalog_unavailable");
        });
        let logged = capture.0.lock().expect("capture lock").join(" ");
        assert!(!logged.contains(SENTINEL), "upstream text reached the log");
        assert!(
            !logged.contains("s3://internal"),
            "upstream path reached the log"
        );
        assert!(logged.contains("catalog_status"));
        assert!(logged.contains("upstream_status"));
    }
}
