//! Coordinator runtime configuration (BS-020.T1).
//!
//! The `serve` command reads one JSON file. Secrets are named by file path so
//! the document itself stays reviewable and can be mounted as a `ConfigMap`:
//!
//! - the PostgreSQL URL is `database.url` or `database.url_file`;
//! - the OIDC client secret is `identity.client_secret_file`;
//! - worker pool tokens are issued through the API and stored only as
//!   verifiers in PostgreSQL, never in this document.
//!
//! Environment variables override the two connection strings the container
//! entrypoint already uses (`BERGSMITH_DATABASE_URL`,
//! `BERGSMITH_HTTP_BIND`, `BERGSMITH_WORKER_BIND`), so a Compose file can keep
//! the rest of the document static.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bergsmith_common::ids::CatalogConnectionId;
use bergsmith_maintenance::{PlannerConfig, StatisticsConfig};
use bergsmith_persistence::scheduling::{AdmissionLimits, RetryPolicy};
use bergsmith_storage::{AzureProfile, GcsProfile, S3Profile, StorageProfile, StorageProfiles};
use serde::Deserialize;
use url::Url;

use crate::durable_publication::PublicationConfig;
use crate::identity::oidc::{OidcConfig, TokenEndpointAuth};
use crate::identity::session::SessionConfig;
use crate::runtime::statistics::SamplingSettings;
use crate::scheduling::SchedulerConfig;

/// The complete `serve` configuration document.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoordinatorConfig {
    /// PostgreSQL connection settings.
    pub database: DatabaseSettings,
    /// Browser HTTP listener and static assets.
    pub http: HttpSettings,
    /// Worker gRPC listener and TLS identity.
    pub worker: WorkerSettings,
    /// OIDC provider settings. Required to serve the browser API.
    pub identity: IdentitySettings,
    /// Browser session and cookie policy.
    #[serde(default)]
    pub sessions: SessionSettings,
    /// Storage profiles selected by table location.
    #[serde(default)]
    pub storage: StorageSettings,
    /// Base planner limits for manual jobs.
    #[serde(default)]
    pub planner: PlannerConfig,
    /// Snapshot-expiration safety settings.
    #[serde(default)]
    pub expiration: ExpirationSettings,
    /// Orphan-file cleanup limits and enablement (BS-031.T2).
    #[serde(default)]
    pub orphans: OrphanSettings,
    /// Scheduler admission and timing.
    #[serde(default)]
    pub scheduler: SchedulerSettings,
    /// Publication bounds.
    #[serde(default)]
    pub publication: PublicationSettings,
    /// The single task envelope and worker pool used for admitted work.
    pub task: TaskSettings,
    /// Coordinator leadership lease.
    #[serde(default)]
    pub leadership: LeadershipSettings,
    /// How often the planning and publication loops run.
    #[serde(default)]
    pub loops: LoopSettings,
    /// Metadata-only table statistics sampling.
    #[serde(default)]
    pub statistics: StatisticsSettings,
    /// Notification destinations and outbound delivery (BS-029).
    #[serde(default)]
    pub notifications: NotificationsSettings,
    /// Metrics listener and ledger snapshot bounds.
    #[serde(default)]
    pub observability: ObservabilitySettings,
}

/// PostgreSQL connection settings.
/// Snapshot-expiration policy that depends on the catalog.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpirationSettings {
    /// Whether the catalog atomically rejects a `remove-snapshots` commit that
    /// would remove a snapshot referenced by any branch or tag.
    ///
    /// False by default: a standard Iceberg catalog may drop a concurrently
    /// created ref with the snapshot, so expiration stays disabled until an
    /// operator certifies the behavior for this catalog.
    #[serde(default)]
    pub catalog_ref_safety: bool,
}

/// Orphan-file cleanup settings (BS-031.T2, ADR 0062).
///
/// Cleanup deletes storage objects, so it does nothing until an operator sets
/// `enabled`. Every bound fails closed: a listing or scan beyond its limit ends
/// the job instead of deleting from an incomplete view.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrphanSettings {
    /// Whether orphan-file cleanup may run at all.
    #[serde(default)]
    pub enabled: bool,
    /// Objects younger than this many seconds are never deleted.
    #[serde(default = "default_orphan_minimum_age_seconds")]
    pub minimum_age_seconds: u64,
    /// The most objects one listing may return.
    #[serde(default = "default_orphan_max_listed_objects")]
    pub max_listed_objects: usize,
    /// The most distinct manifests one reachability scan reads.
    #[serde(default = "default_orphan_max_scan_manifests")]
    pub max_scan_manifests: usize,
    /// The most manifest entries one reachability scan reads, counting each
    /// distinct manifest once.
    #[serde(default = "default_orphan_max_scan_entries")]
    pub max_scan_entries: usize,
    /// The largest manifest list or manifest one scan reads, in bytes.
    #[serde(default = "default_orphan_max_scan_bytes")]
    pub max_scan_bytes: u64,
    /// The most candidates one plan may carry.
    #[serde(default = "default_orphan_max_candidates")]
    pub max_candidates: usize,
    /// The most objects one deletion pass resolves.
    #[serde(default = "default_orphan_max_deletions_per_pass")]
    pub max_deletions_per_pass: usize,
    /// The deadline in seconds for one storage step: reading one manifest
    /// list or manifest, listing the table, or deleting one object.
    #[serde(default = "default_orphan_operation_timeout_seconds")]
    pub operation_timeout_seconds: u64,
}

impl Default for OrphanSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            minimum_age_seconds: default_orphan_minimum_age_seconds(),
            max_listed_objects: default_orphan_max_listed_objects(),
            max_scan_manifests: default_orphan_max_scan_manifests(),
            max_scan_entries: default_orphan_max_scan_entries(),
            max_scan_bytes: default_orphan_max_scan_bytes(),
            max_candidates: default_orphan_max_candidates(),
            max_deletions_per_pass: default_orphan_max_deletions_per_pass(),
            operation_timeout_seconds: default_orphan_operation_timeout_seconds(),
        }
    }
}

impl OrphanSettings {
    /// The validated settings the services use.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Invalid`] when a bound is outside its range or the
    /// per-pass bound exceeds the candidate bound.
    pub fn cleanup(&self) -> Result<OrphanConfig, ConfigError> {
        if self.minimum_age_seconds > 31_536_000 {
            return Err(ConfigError::Invalid(
                "orphans.minimum_age_seconds must be at most 31536000".into(),
            ));
        }
        if !(1..=10_000_000).contains(&self.max_listed_objects) {
            return Err(ConfigError::Invalid(
                "orphans.max_listed_objects must be between 1 and 10000000".into(),
            ));
        }
        if !(1..=1_000_000).contains(&self.max_scan_manifests) {
            return Err(ConfigError::Invalid(
                "orphans.max_scan_manifests must be between 1 and 1000000".into(),
            ));
        }
        if !(1..=10_000_000).contains(&self.max_scan_entries) {
            return Err(ConfigError::Invalid(
                "orphans.max_scan_entries must be between 1 and 10000000".into(),
            ));
        }
        if !(1..=1_073_741_824).contains(&self.max_scan_bytes) {
            return Err(ConfigError::Invalid(
                "orphans.max_scan_bytes must be between 1 and 1073741824".into(),
            ));
        }
        if !(1..=1_000_000).contains(&self.max_candidates) {
            return Err(ConfigError::Invalid(
                "orphans.max_candidates must be between 1 and 1000000".into(),
            ));
        }
        if !(1..=1_000_000).contains(&self.max_deletions_per_pass) {
            return Err(ConfigError::Invalid(
                "orphans.max_deletions_per_pass must be between 1 and 1000000".into(),
            ));
        }
        if !(1..=3_600).contains(&self.operation_timeout_seconds) {
            return Err(ConfigError::Invalid(
                "orphans.operation_timeout_seconds must be between 1 and 3600".into(),
            ));
        }
        if self.max_deletions_per_pass > self.max_candidates {
            return Err(ConfigError::Invalid(
                "orphans.max_deletions_per_pass must not exceed orphans.max_candidates".into(),
            ));
        }
        Ok(OrphanConfig {
            enabled: self.enabled,
            minimum_age_seconds: self.minimum_age_seconds,
            max_listed_objects: self.max_listed_objects,
            max_scan_manifests: self.max_scan_manifests,
            max_scan_entries: self.max_scan_entries,
            max_scan_bytes: self.max_scan_bytes,
            max_candidates: self.max_candidates,
            max_deletions_per_pass: self.max_deletions_per_pass,
            operation_timeout_seconds: self.operation_timeout_seconds,
        })
    }
}

const fn default_orphan_minimum_age_seconds() -> u64 {
    3 * 24 * 60 * 60
}

const fn default_orphan_max_listed_objects() -> usize {
    1_000_000
}

const fn default_orphan_max_scan_manifests() -> usize {
    100_000
}

const fn default_orphan_max_scan_entries() -> usize {
    500_000
}

const fn default_orphan_max_scan_bytes() -> u64 {
    64 * 1024 * 1024
}

const fn default_orphan_max_candidates() -> usize {
    10_000
}

const fn default_orphan_max_deletions_per_pass() -> usize {
    500
}

const fn default_orphan_operation_timeout_seconds() -> u64 {
    120
}

/// Validated orphan-file cleanup settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrphanConfig {
    enabled: bool,
    minimum_age_seconds: u64,
    max_listed_objects: usize,
    max_scan_manifests: usize,
    max_scan_entries: usize,
    max_scan_bytes: u64,
    max_candidates: usize,
    max_deletions_per_pass: usize,
    operation_timeout_seconds: u64,
}

impl OrphanConfig {
    /// Whether cleanup may run.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// The minimum object age in seconds.
    #[must_use]
    pub const fn minimum_age_seconds(&self) -> u64 {
        self.minimum_age_seconds
    }

    /// The most objects one listing may return.
    #[must_use]
    pub const fn max_listed_objects(&self) -> usize {
        self.max_listed_objects
    }

    /// The most distinct manifests one scan reads.
    #[must_use]
    pub const fn max_scan_manifests(&self) -> usize {
        self.max_scan_manifests
    }

    /// The most manifest entries one scan reads.
    #[must_use]
    pub const fn max_scan_entries(&self) -> usize {
        self.max_scan_entries
    }

    /// The largest manifest list or manifest one scan reads, in bytes.
    #[must_use]
    pub const fn max_scan_bytes(&self) -> u64 {
        self.max_scan_bytes
    }

    /// The most candidates one plan may carry.
    #[must_use]
    pub const fn max_candidates(&self) -> usize {
        self.max_candidates
    }

    /// The most objects one deletion pass resolves.
    #[must_use]
    pub const fn max_deletions_per_pass(&self) -> usize {
        self.max_deletions_per_pass
    }

    /// The deadline for one storage step.
    #[must_use]
    pub const fn operation_timeout(&self) -> Duration {
        Duration::from_secs(self.operation_timeout_seconds)
    }

    /// The reachability bounds a planning or deletion pass scans with.
    ///
    /// # Errors
    ///
    /// [`OrphanError::InvalidPolicy`] when a bound is zero, which validated
    /// settings never produce.
    ///
    /// [`OrphanError::InvalidPolicy`]: bergsmith_maintenance::orphans::OrphanError::InvalidPolicy
    pub fn reachability_bounds(
        &self,
    ) -> Result<
        bergsmith_maintenance::orphans::ReachabilityBounds,
        bergsmith_maintenance::orphans::OrphanError,
    > {
        bergsmith_maintenance::orphans::ReachabilityBounds::new(
            self.max_scan_manifests,
            self.max_scan_entries,
            self.max_scan_bytes,
            self.operation_timeout(),
        )
    }
}

impl Default for OrphanConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            minimum_age_seconds: default_orphan_minimum_age_seconds(),
            max_listed_objects: default_orphan_max_listed_objects(),
            max_scan_manifests: default_orphan_max_scan_manifests(),
            max_scan_entries: default_orphan_max_scan_entries(),
            max_scan_bytes: default_orphan_max_scan_bytes(),
            max_candidates: default_orphan_max_candidates(),
            max_deletions_per_pass: default_orphan_max_deletions_per_pass(),
            operation_timeout_seconds: default_orphan_operation_timeout_seconds(),
        }
    }
}

/// PostgreSQL connection and pool settings.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseSettings {
    /// Connection URL. Mutually exclusive with [`Self::url_file`].
    pub url: Option<String>,
    /// File containing the connection URL.
    pub url_file: Option<PathBuf>,
    /// Pool size.
    #[serde(default = "default_database_connections")]
    pub max_connections: u32,
    /// How long to wait for a pooled connection.
    #[serde(default = "default_acquire_timeout_seconds")]
    pub acquire_timeout_seconds: u64,
}

impl DatabaseSettings {
    /// Resolves the URL from the inline value or file.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] when neither or both sources are set, or the file
    /// cannot be read.
    pub fn url(&self) -> Result<String, ConfigError> {
        match (&self.url, &self.url_file) {
            (Some(url), None) => Ok(url.clone()),
            (None, Some(path)) => read_trimmed(path, "database URL"),
            (Some(_), Some(_)) => Err(ConfigError::Invalid(
                "database.url and database.url_file are mutually exclusive".into(),
            )),
            (None, None) => Err(ConfigError::Invalid(
                "one of database.url or database.url_file is required".into(),
            )),
        }
    }
}

/// Browser HTTP listener settings.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpSettings {
    /// Address to bind, such as `0.0.0.0:8080`.
    pub bind: String,
    /// Directory holding a Vite build (`index.html` and `assets/`). Omit when
    /// a reverse proxy serves the UI and this process only serves `/auth` and
    /// `/api/v1`.
    #[serde(default)]
    pub web_root: Option<PathBuf>,
    /// Whether session and CSRF cookies carry `Secure`. Disable only for
    /// plain-HTTP development.
    #[serde(default = "default_true")]
    pub secure_cookies: bool,
}

/// Worker gRPC listener settings.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerSettings {
    /// Address to bind, such as `0.0.0.0:7443`.
    pub bind: String,
    /// PEM certificate chain presented to workers.
    pub certificate: PathBuf,
    /// PEM private key for [`Self::certificate`].
    pub private_key: PathBuf,
}

/// OIDC provider settings.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentitySettings {
    /// Issuer URL, compared exactly with the `iss` claim.
    pub issuer: String,
    /// Registered confidential client identifier.
    pub client_id: String,
    /// File containing the client secret, if the client has one.
    pub client_secret_file: Option<PathBuf>,
    /// Registered redirect URI.
    pub redirect_uri: String,
    /// Requested scopes; `openid` is always added.
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    /// How the client authenticates at the token endpoint.
    #[serde(default = "default_token_endpoint_auth")]
    pub token_endpoint_auth: TokenEndpointAuthSetting,
    /// Allow a plain-HTTP issuer. Development only.
    #[serde(default)]
    pub allow_insecure_http: bool,
    /// Clock skew accepted for `exp` and `iat`.
    #[serde(default = "default_leeway_seconds")]
    pub leeway_seconds: u64,
    /// Timeout for each provider request.
    #[serde(default = "default_oidc_timeout_seconds")]
    pub request_timeout_seconds: u64,
}

/// How the OIDC client authenticates at the token endpoint.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TokenEndpointAuthSetting {
    /// HTTP Basic with the client secret.
    #[default]
    ClientSecretBasic,
    /// Client ID and secret in the form body.
    ClientSecretPost,
    /// A public client with no secret.
    None,
}

impl From<TokenEndpointAuthSetting> for TokenEndpointAuth {
    fn from(value: TokenEndpointAuthSetting) -> Self {
        match value {
            TokenEndpointAuthSetting::ClientSecretBasic => Self::ClientSecretBasic,
            TokenEndpointAuthSetting::ClientSecretPost => Self::ClientSecretPost,
            TokenEndpointAuthSetting::None => Self::None,
        }
    }
}

/// Browser session policy.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSettings {
    /// Session cookie name.
    #[serde(default = "default_cookie_name")]
    pub cookie_name: String,
    /// Absolute session lifetime.
    #[serde(default = "default_session_hours")]
    pub absolute_lifetime_hours: u64,
    /// Idle timeout.
    #[serde(default = "default_idle_hours")]
    pub idle_timeout_hours: u64,
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            cookie_name: default_cookie_name(),
            absolute_lifetime_hours: default_session_hours(),
            idle_timeout_hours: default_idle_hours(),
        }
    }
}

/// Storage profile configuration.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageSettings {
    /// Profiles tried in order for a connection without its own entry.
    #[serde(default)]
    pub default: Vec<StorageProfileSetting>,
    /// Per-connection profiles, keyed by catalog connection UUID.
    #[serde(default)]
    pub connections: BTreeMap<String, Vec<StorageProfileSetting>>,
}

/// One backend profile.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StorageProfileSetting {
    /// Amazon S3 or an S3-compatible store.
    S3 {
        /// Endpoint URL; omit for AWS.
        endpoint: Option<String>,
        /// Signing region.
        region: String,
        /// Address buckets as `endpoint/bucket/key`.
        #[serde(default)]
        path_style_access: bool,
    },
    /// Google Cloud Storage (parsed but not enabled in this release).
    Gcs {
        /// Project that owns the buckets.
        project: Option<String>,
        /// Emulator endpoint; omit for Google.
        endpoint: Option<String>,
    },
    /// Azure Blob Storage or ADLS Gen2 (parsed but not enabled).
    Azure {
        /// Storage account name.
        account: String,
        /// Emulator endpoint; omit for Azure.
        endpoint: Option<String>,
        /// Whether the account has a hierarchical namespace.
        #[serde(default)]
        hierarchical_namespace: bool,
    },
    /// The local filesystem.
    LocalFs,
    /// In-process memory.
    Memory,
}

impl From<&StorageProfileSetting> for StorageProfile {
    fn from(value: &StorageProfileSetting) -> Self {
        match value {
            StorageProfileSetting::S3 {
                endpoint,
                region,
                path_style_access,
            } => Self::S3(S3Profile {
                endpoint: endpoint.clone(),
                region: region.clone(),
                path_style_access: *path_style_access,
            }),
            StorageProfileSetting::Gcs { project, endpoint } => Self::Gcs(GcsProfile {
                project: project.clone(),
                endpoint: endpoint.clone(),
            }),
            StorageProfileSetting::Azure {
                account,
                endpoint,
                hierarchical_namespace,
            } => Self::Azure(AzureProfile {
                account: account.clone(),
                endpoint: endpoint.clone(),
                hierarchical_namespace: *hierarchical_namespace,
            }),
            StorageProfileSetting::LocalFs => Self::LocalFs,
            StorageProfileSetting::Memory => Self::Memory,
        }
    }
}

impl StorageSettings {
    /// Builds the runtime profile set and validates connection keys.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] for an empty default set, a malformed connection UUID,
    /// or an empty per-connection list.
    pub fn profiles(&self) -> Result<RuntimeProfiles, ConfigError> {
        if self.default.is_empty() {
            return Err(ConfigError::Invalid(
                "storage.default must name at least one profile".into(),
            ));
        }
        let default = profiles(&self.default);
        let mut connections = BTreeMap::new();
        for (key, settings) in &self.connections {
            let id = key.parse().map_err(|_| {
                ConfigError::Invalid(format!("storage.connections key {key} is not a UUID"))
            })?;
            if settings.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "storage.connections.{key} must name at least one profile"
                )));
            }
            connections.insert(id, profiles(settings));
        }
        Ok(RuntimeProfiles {
            default,
            connections,
        })
    }
}

fn profiles(settings: &[StorageProfileSetting]) -> StorageProfiles {
    StorageProfiles::new(settings.iter().map(StorageProfile::from))
}

/// Storage profiles resolved from the configuration document.
#[derive(Clone, Debug)]
pub struct RuntimeProfiles {
    /// Profiles for connections without an entry.
    pub default: StorageProfiles,
    /// Profiles for one catalog connection.
    pub connections: BTreeMap<CatalogConnectionId, StorageProfiles>,
}

impl RuntimeProfiles {
    /// The profiles that apply to `connection`.
    #[must_use]
    pub fn for_connection(&self, connection: CatalogConnectionId) -> &StorageProfiles {
        self.connections.get(&connection).unwrap_or(&self.default)
    }
}

/// Scheduler settings.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerSettings {
    /// Maximum input bytes one admitted job may rewrite.
    #[serde(default = "default_max_job_input_bytes")]
    pub max_job_input_bytes: u64,
    /// Maximum estimated job duration.
    #[serde(default = "default_max_job_duration_millis")]
    pub max_job_duration_millis: u64,
    /// Jobs read per database page.
    #[serde(default = "default_page_size")]
    pub page_size: i64,
    /// Reconciliation interval.
    #[serde(default = "default_scheduler_interval_millis")]
    pub interval_millis: u64,
    /// Per-job external validation deadline.
    #[serde(default = "default_validation_timeout_millis")]
    pub validation_timeout_millis: u64,
    /// Grace before periodic recovery of an authorized send.
    #[serde(default = "default_publication_grace_millis")]
    pub publication_send_grace_millis: u64,
    /// Admission budgets.
    #[serde(default)]
    pub admission: AdmissionSettings,
    /// Retry backoff bounds.
    #[serde(default)]
    pub retry: RetrySettings,
}

impl Default for SchedulerSettings {
    fn default() -> Self {
        Self {
            max_job_input_bytes: default_max_job_input_bytes(),
            max_job_duration_millis: default_max_job_duration_millis(),
            page_size: default_page_size(),
            interval_millis: default_scheduler_interval_millis(),
            validation_timeout_millis: default_validation_timeout_millis(),
            publication_send_grace_millis: default_publication_grace_millis(),
            admission: AdmissionSettings::default(),
            retry: RetrySettings::default(),
        }
    }
}

impl SchedulerSettings {
    /// Converts to the scheduler's runtime configuration.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] when a duration or limit is invalid.
    pub fn scheduler(&self) -> Result<SchedulerConfig, ConfigError> {
        let config = SchedulerConfig {
            admission: self.admission.limits(),
            retry: self.retry.policy(),
            max_job_input_bytes: self.max_job_input_bytes,
            max_job_duration_millis: self.max_job_duration_millis,
            page_size: self.page_size,
            interval: duration_millis(self.interval_millis, "scheduler.interval_millis")?,
            validation_timeout: duration_millis(
                self.validation_timeout_millis,
                "scheduler.validation_timeout_millis",
            )?,
            publication_send_grace: duration_millis(
                self.publication_send_grace_millis,
                "scheduler.publication_send_grace_millis",
            )?,
        };
        if !config.admission.is_valid()
            || !config.retry.is_valid()
            || config.max_job_input_bytes == 0
            || config.max_job_duration_millis == 0
            || !(1..=10_000).contains(&config.page_size)
            || config.interval.is_zero()
            || config.interval > Duration::from_secs(86_400)
            || config.validation_timeout.is_zero()
            || config.validation_timeout > Duration::from_secs(3_600)
            || config.publication_send_grace > Duration::from_secs(86_400)
        {
            return Err(ConfigError::Invalid(
                "scheduler settings are outside their supported bounds".into(),
            ));
        }
        Ok(config)
    }
}

/// Admission budget settings.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionSettings {
    /// Admitted jobs globally.
    #[serde(default = "default_admission_jobs")]
    pub jobs: i64,
    /// Admitted jobs per organization.
    #[serde(default = "default_admission_organization_jobs")]
    pub organization_jobs: i64,
    /// Admitted jobs per pool.
    #[serde(default = "default_admission_pool_jobs")]
    pub pool_jobs: i64,
    /// Active managed attempts.
    #[serde(default = "default_admission_attempts")]
    pub attempts: i64,
    /// Total reserved memory.
    #[serde(default = "default_admission_memory")]
    pub memory_bytes: i64,
    /// Total reserved CPU milliseconds.
    #[serde(default = "default_admission_cpu")]
    pub cpu_millis: i64,
}

impl Default for AdmissionSettings {
    fn default() -> Self {
        let limits = AdmissionLimits::default();
        Self {
            jobs: limits.jobs,
            organization_jobs: limits.organization_jobs,
            pool_jobs: limits.pool_jobs,
            attempts: limits.attempts,
            memory_bytes: limits.memory_bytes,
            cpu_millis: limits.cpu_millis,
        }
    }
}

impl AdmissionSettings {
    const fn limits(self) -> AdmissionLimits {
        AdmissionLimits {
            jobs: self.jobs,
            organization_jobs: self.organization_jobs,
            pool_jobs: self.pool_jobs,
            attempts: self.attempts,
            memory_bytes: self.memory_bytes,
            cpu_millis: self.cpu_millis,
        }
    }
}

/// Retry backoff settings.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrySettings {
    /// Initial ceiling in milliseconds.
    #[serde(default = "default_retry_base_millis")]
    pub base_millis: i64,
    /// Maximum ceiling in milliseconds.
    #[serde(default = "default_retry_max_millis")]
    pub max_millis: i64,
}

impl Default for RetrySettings {
    fn default() -> Self {
        let policy = RetryPolicy::default();
        Self {
            base_millis: policy.base_millis,
            max_millis: policy.max_millis,
        }
    }
}

impl RetrySettings {
    const fn policy(self) -> RetryPolicy {
        RetryPolicy {
            base_millis: self.base_millis,
            max_millis: self.max_millis,
        }
    }
}

/// Publication bounds.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationSettings {
    /// Deadline for one bounded preparation step.
    #[serde(default = "default_publication_step_seconds")]
    pub operation_timeout_seconds: u64,
}

impl Default for PublicationSettings {
    fn default() -> Self {
        Self {
            operation_timeout_seconds: default_publication_step_seconds(),
        }
    }
}

impl PublicationSettings {
    /// Converts to the publication configuration.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] when the timeout is zero.
    pub fn config(&self) -> Result<PublicationConfig, ConfigError> {
        Ok(PublicationConfig {
            operation_timeout: duration_seconds(
                self.operation_timeout_seconds,
                "publication.operation_timeout_seconds",
            )?,
        })
    }
}

/// The one task envelope and worker pool used by this release.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSettings {
    /// Worker pool that receives admitted tasks.
    pub pool: String,
    /// Required worker capability.
    pub capability: String,
    /// Memory reserved per active attempt.
    pub memory_bytes: u64,
    /// CPU reserved per active attempt.
    pub cpu_millis: u32,
    /// Maximum task duration.
    pub max_duration_seconds: u64,
    /// Maximum bytes one task may write.
    pub max_output_bytes: u64,
}

impl TaskSettings {
    /// The protobuf envelope offered with every task.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] for a zero or unrepresentable limit.
    pub fn limits(&self) -> Result<bergsmith_protocol::cluster::ResourceLimits, ConfigError> {
        let seconds = i64::try_from(self.max_duration_seconds)
            .map_err(|_| ConfigError::Invalid("task.max_duration_seconds is too large".into()))?;
        if self.pool.is_empty()
            || self.capability.is_empty()
            || self.memory_bytes == 0
            || self.cpu_millis == 0
            || self.max_duration_seconds == 0
            || self.max_output_bytes == 0
        {
            return Err(ConfigError::Invalid(
                "task limits must all be positive".into(),
            ));
        }
        Ok(bergsmith_protocol::cluster::ResourceLimits {
            memory_bytes: self.memory_bytes,
            cpu_millis: self.cpu_millis,
            max_duration: Some(prost_types::Duration { seconds, nanos: 0 }),
            max_output_bytes: self.max_output_bytes,
        })
    }
}

/// Leadership lease settings.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeadershipSettings {
    /// Lease duration.
    #[serde(default = "default_lease_seconds")]
    pub lease_seconds: u64,
    /// How often to renew.
    #[serde(default = "default_renew_seconds")]
    pub renew_seconds: u64,
}

impl Default for LeadershipSettings {
    fn default() -> Self {
        Self {
            lease_seconds: default_lease_seconds(),
            renew_seconds: default_renew_seconds(),
        }
    }
}

/// How often the runtime loops run.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopSettings {
    /// Planning scan interval.
    #[serde(default = "default_loop_millis")]
    pub planning_millis: u64,
    /// Publication scan interval.
    #[serde(default = "default_loop_millis")]
    pub publication_millis: u64,
}

impl Default for LoopSettings {
    fn default() -> Self {
        Self {
            planning_millis: default_loop_millis(),
            publication_millis: default_loop_millis(),
        }
    }
}

/// Metadata-only statistics sampling (BS-025.T1).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatisticsSettings {
    /// How often a sampling pass runs.
    #[serde(default = "default_statistics_interval_millis")]
    pub interval_millis: u64,
    /// How long a sample is kept.
    #[serde(default = "default_statistics_retention_days")]
    pub retention_days: u64,
    /// How many due tables one pass examines.
    #[serde(default = "default_statistics_page")]
    pub page_size: i64,
    /// How many catalog connections one pass samples concurrently.
    #[serde(default = "default_statistics_concurrency")]
    pub max_concurrent_connections: usize,
    /// The bounds of one sample.
    #[serde(default)]
    pub sample: StatisticsConfig,
}

impl Default for StatisticsSettings {
    fn default() -> Self {
        Self {
            interval_millis: default_statistics_interval_millis(),
            retention_days: default_statistics_retention_days(),
            page_size: default_statistics_page(),
            max_concurrent_connections: default_statistics_concurrency(),
            sample: StatisticsConfig::default(),
        }
    }
}

impl StatisticsSettings {
    /// The validated sampling settings.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] when an interval, retention, page, or concurrency bound
    /// is outside its range, or the sample bounds are unusable.
    pub fn sampling(&self) -> Result<SamplingSettings, ConfigError> {
        if !(1..=86_400_000).contains(&self.interval_millis) {
            return Err(ConfigError::Invalid(
                "statistics.interval_millis must be between 1 and 86400000".into(),
            ));
        }
        if !(1..=3_650).contains(&self.retention_days) {
            return Err(ConfigError::Invalid(
                "statistics.retention_days must be between 1 and 3650".into(),
            ));
        }
        if !(1..=1_000).contains(&self.page_size) {
            return Err(ConfigError::Invalid(
                "statistics.page_size must be between 1 and 1000".into(),
            ));
        }
        if !(1..=32).contains(&self.max_concurrent_connections) {
            return Err(ConfigError::Invalid(
                "statistics.max_concurrent_connections must be between 1 and 32".into(),
            ));
        }
        self.sample
            .validate()
            .map_err(|error| ConfigError::Invalid(format!("statistics.sample: {error}")))?;
        Ok(SamplingSettings {
            interval_millis: self.interval_millis,
            retention_days: self.retention_days,
            page_size: self.page_size,
            max_concurrent_connections: self.max_concurrent_connections,
            sample: self.sample.clone(),
        })
    }
}

fn default_statistics_interval_millis() -> u64 {
    300_000
}

fn default_statistics_retention_days() -> u64 {
    90
}

fn default_statistics_page() -> i64 {
    64
}

fn default_statistics_concurrency() -> usize {
    4
}

/// Notification destinations and outbound delivery (BS-029, ADR 0060).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationsSettings {
    /// Hosts a destination URL may target, such as `hooks.slack.com`,
    /// `*.example.com:8443`, or `127.0.0.1:9000`. Empty allows none.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Directory holding webhook signing secrets. A destination stores only a
    /// name inside it.
    #[serde(default)]
    pub secrets_dir: Option<PathBuf>,
    /// Timeout for one delivery request.
    #[serde(default = "default_notification_timeout_seconds")]
    pub request_timeout_seconds: u64,
    /// How often the leader claims due deliveries.
    #[serde(default = "default_notification_delivery_millis")]
    pub delivery_interval_millis: u64,
}

impl Default for NotificationsSettings {
    fn default() -> Self {
        Self {
            allowed_hosts: Vec::new(),
            secrets_dir: None,
            request_timeout_seconds: default_notification_timeout_seconds(),
            delivery_interval_millis: default_notification_delivery_millis(),
        }
    }
}

impl NotificationsSettings {
    /// The validated outbound policy.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] when an allow-list entry is malformed or names a
    /// link-local address, or the timeout is outside 1 to 60 seconds.
    pub fn policy(&self) -> Result<crate::notifications::Settings, ConfigError> {
        let allow_list = bergsmith_common::notifications::AllowList::new(&self.allowed_hosts)
            .map_err(|error| {
                ConfigError::Invalid(format!("notifications.allowed_hosts: {error}"))
            })?;
        if !(1..=60).contains(&self.request_timeout_seconds) {
            return Err(ConfigError::Invalid(
                "notifications.request_timeout_seconds must be 1 to 60".into(),
            ));
        }
        if !(1_000..=300_000).contains(&self.delivery_interval_millis) {
            return Err(ConfigError::Invalid(
                "notifications.delivery_interval_millis must be 1000 to 300000".into(),
            ));
        }
        Ok(crate::notifications::Settings::new(
            allow_list,
            self.secrets_dir.clone(),
            Duration::from_secs(self.request_timeout_seconds),
        ))
    }
}

fn default_notification_timeout_seconds() -> u64 {
    10
}

fn default_notification_delivery_millis() -> u64 {
    15_000
}

/// Metrics settings (BS-020.T2).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservabilitySettings {
    /// Address for the Prometheus `/metrics` listener, such as
    /// `0.0.0.0:9464`. Omit it to serve no metrics. It is a separate listener
    /// from the browser origin and has no authentication, so bind it to a
    /// network only the metrics collector reaches.
    #[serde(default)]
    pub metrics_bind: Option<String>,
    /// How long a ledger snapshot is reused before PostgreSQL is read again.
    #[serde(default = "default_ledger_refresh_seconds")]
    pub ledger_refresh_seconds: u64,
    /// Statement timeout for the ledger snapshot.
    #[serde(default = "default_ledger_query_timeout_millis")]
    pub ledger_query_timeout_millis: u64,
}

impl Default for ObservabilitySettings {
    fn default() -> Self {
        Self {
            metrics_bind: None,
            ledger_refresh_seconds: default_ledger_refresh_seconds(),
            ledger_query_timeout_millis: default_ledger_query_timeout_millis(),
        }
    }
}

impl ObservabilitySettings {
    /// The metrics snapshot bounds.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] when the bind is empty or a bound is zero.
    pub fn metrics(&self) -> Result<crate::observability::MetricsConfig, ConfigError> {
        if self.metrics_bind.as_ref().is_some_and(String::is_empty) {
            return Err(ConfigError::Invalid(
                "observability.metrics_bind must not be empty".into(),
            ));
        }
        if self.ledger_refresh_seconds == 0 || self.ledger_query_timeout_millis == 0 {
            return Err(ConfigError::Invalid(
                "observability ledger refresh and query timeout must be positive".into(),
            ));
        }
        Ok(crate::observability::MetricsConfig {
            refresh: Duration::from_secs(self.ledger_refresh_seconds),
            query_timeout: Duration::from_millis(self.ledger_query_timeout_millis),
        })
    }
}

/// A configuration problem, reported before any listener starts.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("configuration file {path} could not be read: {source}")]
    Read {
        /// Path named on the command line.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The document is not valid JSON for this schema.
    #[error("configuration file {path} is invalid: {source}")]
    Parse {
        /// Path named on the command line.
        path: PathBuf,
        /// Underlying JSON error.
        #[source]
        source: serde_json::Error,
    },
    /// A setting is missing or inconsistent.
    #[error("invalid coordinator configuration: {0}")]
    Invalid(String),
    /// A secret file could not be read.
    #[error("{name} file {path} could not be read: {source}")]
    Secret {
        /// Human-readable setting name.
        name: &'static str,
        /// File path.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

impl CoordinatorConfig {
    /// Reads and validates a configuration document.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] for a missing, malformed, or inconsistent document.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let config = Self::parse(path)?;
        config.validate()?;
        Ok(config)
    }

    /// Reads and parses a document without validating it.
    ///
    /// Command-line and environment overrides must be applied before
    /// validation, so a deployment can supply a database URL exclusively
    /// through `BERGSMITH_DATABASE_URL` even when the file names an
    /// unavailable secret file.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] when the file cannot be read or is not valid JSON.
    pub fn parse(path: &Path) -> Result<Self, ConfigError> {
        let bytes = std::fs::read(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        serde_json::from_slice(&bytes).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Validates every cross-field invariant that does not need I/O.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] naming the first problem.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.database.url()?;
        self.orphans.cleanup()?;
        if self.http.bind.is_empty() {
            return Err(ConfigError::Invalid("http.bind must not be empty".into()));
        }
        if self
            .http
            .web_root
            .as_ref()
            .is_some_and(|root| root.as_os_str().is_empty())
        {
            return Err(ConfigError::Invalid(
                "http.web_root must not be empty".into(),
            ));
        }
        if self.worker.bind.is_empty() {
            return Err(ConfigError::Invalid("worker.bind must not be empty".into()));
        }
        if self.worker.certificate.as_os_str().is_empty()
            || self.worker.private_key.as_os_str().is_empty()
        {
            return Err(ConfigError::Invalid(
                "worker.certificate and worker.private_key are required".into(),
            ));
        }
        let issuer = Url::parse(&self.identity.issuer)
            .map_err(|_| ConfigError::Invalid("identity.issuer is not a URL".into()))?;
        if issuer.scheme() != "https" && !self.identity.allow_insecure_http {
            return Err(ConfigError::Invalid(
                "identity.issuer must use https unless allow_insecure_http is set".into(),
            ));
        }
        if self.identity.client_id.is_empty() {
            return Err(ConfigError::Invalid(
                "identity.client_id must not be empty".into(),
            ));
        }
        if self.identity.client_secret_file.is_none()
            && self.identity.token_endpoint_auth != TokenEndpointAuthSetting::None
        {
            return Err(ConfigError::Invalid(
                "identity.client_secret_file is required unless token_endpoint_auth is none".into(),
            ));
        }
        Url::parse(&self.identity.redirect_uri)
            .map_err(|_| ConfigError::Invalid("identity.redirect_uri is not a URL".into()))?;
        if self.identity.scopes.is_empty() {
            return Err(ConfigError::Invalid(
                "identity.scopes must not be empty".into(),
            ));
        }
        self.session()?;
        self.storage.profiles()?;
        self.planner
            .validate()
            .map_err(|error| ConfigError::Invalid(error.to_string()))?;
        self.scheduler.scheduler()?;
        self.publication.config()?;
        self.task.limits()?;
        if self.task.max_output_bytes < self.planner.max_group_input_bytes {
            return Err(ConfigError::Invalid(
                "task.max_output_bytes must cover planner.max_group_input_bytes".into(),
            ));
        }
        if self.leadership.lease_seconds == 0
            || self.leadership.renew_seconds == 0
            || self.leadership.renew_seconds >= self.leadership.lease_seconds
        {
            return Err(ConfigError::Invalid(
                "leadership.renew_seconds must be positive and below lease_seconds".into(),
            ));
        }
        if self.loops.planning_millis == 0 || self.loops.publication_millis == 0 {
            return Err(ConfigError::Invalid(
                "loop intervals must be positive".into(),
            ));
        }
        self.observability.metrics()?;
        self.statistics.sampling()?;
        self.notifications.policy()?;
        Ok(())
    }

    /// The session policy, with cookie security taken from `http`.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] when the session bounds are invalid.
    pub fn session(&self) -> Result<SessionConfig, ConfigError> {
        let config = SessionConfig {
            cookie_name: self.sessions.cookie_name.clone(),
            secure: self.http.secure_cookies,
            absolute_lifetime: duration_hours(
                self.sessions.absolute_lifetime_hours,
                "sessions.absolute_lifetime_hours",
            )?,
            idle_timeout: duration_hours(
                self.sessions.idle_timeout_hours,
                "sessions.idle_timeout_hours",
            )?,
        };
        config
            .validate()
            .map_err(|error| ConfigError::Invalid(error.to_string()))?;
        Ok(config)
    }

    /// The OIDC configuration, reading the secret file if one is named.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] for an unreadable secret or invalid URL/duration.
    pub fn oidc(&self) -> Result<OidcConfig, ConfigError> {
        let client_secret = match &self.identity.client_secret_file {
            Some(path) => Some(bergsmith_common::secret::Secret::new(read_trimmed(
                path,
                "OIDC client secret",
            )?)),
            None => None,
        };
        let redirect_uri = Url::parse(&self.identity.redirect_uri)
            .map_err(|_| ConfigError::Invalid("identity.redirect_uri is not a URL".into()))?;
        Ok(OidcConfig {
            issuer: self.identity.issuer.clone(),
            client_id: self.identity.client_id.clone(),
            client_secret,
            redirect_uri,
            scopes: self.identity.scopes.clone(),
            token_endpoint_auth: self.identity.token_endpoint_auth.into(),
            allow_insecure_http: self.identity.allow_insecure_http,
            leeway: duration_seconds(self.identity.leeway_seconds, "identity.leeway_seconds")?,
            request_timeout: duration_seconds(
                self.identity.request_timeout_seconds,
                "identity.request_timeout_seconds",
            )?,
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(duration_seconds(
                    self.identity.request_timeout_seconds,
                    "identity.request_timeout_seconds",
                )?)
                .build()
                .map_err(|error| {
                    ConfigError::Invalid(format!("OIDC HTTP client could not be built: {error}"))
                })?,
        })
    }
}

fn read_trimmed(path: &Path, name: &'static str) -> Result<String, ConfigError> {
    let value = std::fs::read_to_string(path).map_err(|source| ConfigError::Secret {
        name,
        path: path.to_path_buf(),
        source,
    })?;
    let value = value.trim();
    if value.is_empty() {
        return Err(ConfigError::Invalid(format!("{name} file is empty")));
    }
    Ok(value.to_owned())
}

fn duration_millis(value: u64, field: &str) -> Result<Duration, ConfigError> {
    if value == 0 {
        return Err(ConfigError::Invalid(format!("{field} must be positive")));
    }
    Ok(Duration::from_millis(value))
}

fn duration_seconds(value: u64, field: &str) -> Result<Duration, ConfigError> {
    if value == 0 {
        return Err(ConfigError::Invalid(format!("{field} must be positive")));
    }
    Ok(Duration::from_secs(value))
}

fn duration_hours(value: u64, field: &str) -> Result<Duration, ConfigError> {
    if value == 0 {
        return Err(ConfigError::Invalid(format!("{field} must be positive")));
    }
    value
        .checked_mul(3_600)
        .map(Duration::from_secs)
        .ok_or_else(|| ConfigError::Invalid(format!("{field} is too large")))
}

const fn default_true() -> bool {
    true
}
const fn default_database_connections() -> u32 {
    10
}
const fn default_acquire_timeout_seconds() -> u64 {
    5
}
fn default_scopes() -> Vec<String> {
    vec!["openid".into(), "profile".into(), "email".into()]
}
const fn default_token_endpoint_auth() -> TokenEndpointAuthSetting {
    TokenEndpointAuthSetting::ClientSecretBasic
}
const fn default_leeway_seconds() -> u64 {
    30
}
const fn default_oidc_timeout_seconds() -> u64 {
    10
}
fn default_cookie_name() -> String {
    crate::identity::SESSION_COOKIE.to_owned()
}
const fn default_session_hours() -> u64 {
    12
}
const fn default_idle_hours() -> u64 {
    2
}
const fn default_max_job_input_bytes() -> u64 {
    64 * 1024 * 1024 * 1024
}
const fn default_max_job_duration_millis() -> u64 {
    180_000
}
const fn default_page_size() -> i64 {
    128
}
const fn default_scheduler_interval_millis() -> u64 {
    1_000
}
const fn default_validation_timeout_millis() -> u64 {
    30_000
}
const fn default_publication_grace_millis() -> u64 {
    120_000
}
const fn default_admission_jobs() -> i64 {
    8
}
const fn default_admission_organization_jobs() -> i64 {
    4
}
const fn default_admission_pool_jobs() -> i64 {
    8
}
const fn default_admission_attempts() -> i64 {
    16
}
const fn default_admission_memory() -> i64 {
    64 * 1024 * 1024 * 1024
}
const fn default_admission_cpu() -> i64 {
    32_000
}
const fn default_retry_base_millis() -> i64 {
    1_000
}
const fn default_retry_max_millis() -> i64 {
    60_000
}
const fn default_publication_step_seconds() -> u64 {
    60
}
const fn default_lease_seconds() -> u64 {
    300
}
const fn default_renew_seconds() -> u64 {
    60
}
const fn default_ledger_refresh_seconds() -> u64 {
    15
}
const fn default_ledger_query_timeout_millis() -> u64 {
    5000
}
const fn default_loop_millis() -> u64 {
    1_000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> serde_json::Value {
        serde_json::json!({
            "database": { "url": "postgres://bergsmith:bergsmith@localhost/bergsmith" },
            "http": {
                "bind": "127.0.0.1:8080",
                "web_root": "web/dist",
                "secure_cookies": false
            },
            "worker": {
                "bind": "127.0.0.1:7443",
                "certificate": "/tmp/cert.pem",
                "private_key": "/tmp/key.pem"
            },
            "identity": {
                "issuer": "http://127.0.0.1:58080/realms/bergsmith",
                "client_id": "bergsmith",
                "redirect_uri": "http://127.0.0.1:8080/auth/callback",
                "allow_insecure_http": true,
                "token_endpoint_auth": "none"
            },
            "storage": {
                "default": [{ "kind": "local_fs" }]
            },
            "task": {
                "pool": "default",
                "capability": "native-v1",
                "memory_bytes": 1_073_741_824,
                "cpu_millis": 1000,
                "max_duration_seconds": 180,
                "max_output_bytes": 4_294_967_296_u64
            }
        })
    }

    fn parse(value: serde_json::Value) -> Result<CoordinatorConfig, ConfigError> {
        let config: CoordinatorConfig =
            serde_json::from_value(value).map_err(|source| ConfigError::Parse {
                path: PathBuf::from("<test>"),
                source,
            })?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn the_minimal_document_is_valid_and_uses_documented_defaults() {
        let config = parse(minimal()).unwrap();
        assert_eq!(config.sessions.absolute_lifetime_hours, 12);
        assert_eq!(config.scheduler.page_size, 128);
        assert_eq!(config.planner, PlannerConfig::default());
        assert!(!config.session().unwrap().secure);
    }

    #[test]
    fn metrics_are_off_by_default_and_bounds_must_be_positive() {
        let config = parse(minimal()).unwrap();
        assert_eq!(config.observability.metrics_bind, None);
        assert_eq!(
            config.observability.metrics().unwrap(),
            crate::observability::MetricsConfig::default()
        );

        let mut value = minimal();
        value["observability"] = serde_json::json!({ "metrics_bind": "127.0.0.1:9464" });
        assert!(parse(value).is_ok());
        for invalid in [
            serde_json::json!({ "metrics_bind": "" }),
            serde_json::json!({ "ledger_refresh_seconds": 0 }),
            serde_json::json!({ "ledger_query_timeout_millis": 0 }),
        ] {
            let mut value = minimal();
            value["observability"] = invalid;
            assert!(parse(value).is_err());
        }
    }

    #[test]
    fn a_web_root_is_optional_for_a_proxy_served_ui() {
        let mut value = minimal();
        value["http"].as_object_mut().unwrap().remove("web_root");
        assert!(parse(value).is_ok());
    }

    #[test]
    fn parsing_does_not_read_secret_files_before_overrides() {
        let mut value = minimal();
        value["database"] = serde_json::json!({ "url_file": "/definitely/missing" });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("coordinator.json");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let mut config = CoordinatorConfig::parse(&path).unwrap();
        assert!(config.validate().is_err());
        config.database.url = Some("postgres://bergsmith@localhost/bergsmith".into());
        config.database.url_file = None;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn an_https_issuer_is_required_outside_development() {
        let mut value = minimal();
        value["identity"]["allow_insecure_http"] = serde_json::json!(false);
        assert!(parse(value).is_err());
    }

    #[test]
    fn a_client_secret_is_required_for_a_confidential_client() {
        let mut value = minimal();
        value["identity"]["issuer"] = serde_json::json!("https://issuer.example");
        value["identity"]["token_endpoint_auth"] = serde_json::json!("client_secret_basic");
        assert!(parse(value).is_err());
    }

    #[test]
    fn storage_connection_keys_must_be_uuids() {
        let mut value = minimal();
        value["storage"]["connections"] =
            serde_json::json!({ "not-a-uuid": [{ "kind": "local_fs" }] });
        assert!(parse(value).is_err());
    }

    #[test]
    fn orphan_cleanup_is_off_by_default_with_documented_bounds() {
        let config = parse(minimal()).unwrap();
        let cleanup = config.orphans.cleanup().unwrap();
        assert_eq!(cleanup, OrphanConfig::default());
        assert!(!cleanup.enabled());
        assert_eq!(cleanup.minimum_age_seconds(), 259_200);
        assert_eq!(cleanup.max_listed_objects(), 1_000_000);
        assert_eq!(cleanup.max_scan_manifests(), 100_000);
        assert_eq!(cleanup.max_scan_entries(), 500_000);
        assert_eq!(cleanup.max_scan_bytes(), 64 * 1024 * 1024);
        assert_eq!(cleanup.max_candidates(), 10_000);
        assert_eq!(cleanup.max_deletions_per_pass(), 500);
        assert_eq!(cleanup.operation_timeout(), Duration::from_secs(120));
        assert!(cleanup.reachability_bounds().is_ok());
    }

    #[test]
    fn orphan_bounds_are_checked_at_each_edge() {
        let accepts = |field: &str, value: serde_json::Value| {
            let mut config = minimal();
            config["orphans"] = serde_json::json!({ field: value });
            parse(config).is_ok()
        };
        for (field, low, high) in [
            ("max_listed_objects", 1_u64, 10_000_000_u64),
            ("max_scan_manifests", 1, 1_000_000),
            ("max_scan_entries", 1, 10_000_000),
            ("max_scan_bytes", 1, 1_073_741_824),
            ("operation_timeout_seconds", 1, 3_600),
        ] {
            assert!(!accepts(field, serde_json::json!(0)), "{field} = 0");
            assert!(accepts(field, serde_json::json!(low)), "{field} = {low}");
            assert!(accepts(field, serde_json::json!(high)), "{field} = {high}");
            assert!(
                !accepts(field, serde_json::json!(high + 1)),
                "{field} = {}",
                high + 1
            );
        }
        assert!(accepts("minimum_age_seconds", serde_json::json!(0)));
        assert!(accepts(
            "minimum_age_seconds",
            serde_json::json!(31_536_000)
        ));
        assert!(!accepts(
            "minimum_age_seconds",
            serde_json::json!(31_536_001)
        ));
        assert!(!accepts("unknown", serde_json::json!(true)));

        // The candidate and per-pass bounds are checked together.
        let bounded = |candidates: u64, per_pass: u64| {
            let mut config = minimal();
            config["orphans"] = serde_json::json!({
                "max_candidates": candidates,
                "max_deletions_per_pass": per_pass,
            });
            parse(config).is_ok()
        };
        assert!(bounded(1, 1));
        assert!(bounded(1_000_000, 1_000_000));
        assert!(!bounded(0, 1));
        assert!(!bounded(1_000_001, 1));
        assert!(!bounded(10, 0));
        assert!(!bounded(10, 11), "a pass may not exceed the plan");
    }

    #[test]
    fn scheduler_bounds_are_checked() {
        let mut value = minimal();
        value["scheduler"]["interval_millis"] = serde_json::json!(0);
        assert!(parse(value).is_err());
    }

    #[test]
    fn session_idle_timeout_cannot_exceed_the_absolute_lifetime() {
        let mut value = minimal();
        value["sessions"] = serde_json::json!({
            "absolute_lifetime_hours": 2,
            "idle_timeout_hours": 3
        });
        assert!(parse(value).is_err());
    }

    #[test]
    fn database_url_and_file_are_mutually_exclusive() {
        let mut value = minimal();
        value["database"]["url_file"] = serde_json::json!("/tmp/url");
        assert!(parse(value).is_err());
    }

    #[test]
    fn profile_conversion_preserves_every_backend() {
        let value = serde_json::json!([
            { "kind": "s3", "endpoint": "http://rustfs:9000", "region": "us-east-1", "path_style_access": true },
            { "kind": "gcs", "project": "p", "endpoint": null },
            { "kind": "azure", "account": "a", "hierarchical_namespace": true },
            { "kind": "local_fs" },
            { "kind": "memory" }
        ]);
        let settings: Vec<StorageProfileSetting> = serde_json::from_value(value).unwrap();
        let profiles: Vec<StorageProfile> = settings.iter().map(StorageProfile::from).collect();
        assert!(matches!(profiles[0], StorageProfile::S3(_)));
        assert!(matches!(profiles[1], StorageProfile::Gcs(_)));
        assert!(matches!(profiles[2], StorageProfile::Azure(_)));
        assert!(matches!(profiles[3], StorageProfile::LocalFs));
        assert!(matches!(profiles[4], StorageProfile::Memory));
    }
}
