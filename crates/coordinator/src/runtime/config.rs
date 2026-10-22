//! Coordinator runtime configuration (BS-020.T1).
//!
//! The `serve` command reads one JSON file. Secrets are named by file path so
//! the document itself stays reviewable and can be mounted as a `ConfigMap`:
//!
//! - the PostgreSQL URL is `database.url` or `database.url_file`;
//! - the OIDC client secret is `identity.client_secret_file`;
//! - worker pool tokens are never stored here, only their verifiers.
//!
//! Environment variables override the two connection strings the container
//! entrypoint already uses (`BERGSMITH_DATABASE_URL`,
//! `BERGSMITH_HTTP_BIND`, `BERGSMITH_WORKER_BIND`), so a Compose file can keep
//! the rest of the document static.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::{CatalogConnectionId, OrganizationId, PrincipalId};
use bergsmith_maintenance::PlannerConfig;
use bergsmith_persistence::scheduling::{AdmissionLimits, RetryPolicy};
use bergsmith_storage::{AzureProfile, GcsProfile, S3Profile, StorageProfile, StorageProfiles};
use serde::Deserialize;
use url::Url;

use crate::durable_publication::PublicationConfig;
use crate::identity::oidc::{OidcConfig, TokenEndpointAuth};
use crate::identity::session::SessionConfig;
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
    /// Scheduler admission and timing.
    #[serde(default)]
    pub scheduler: SchedulerSettings,
    /// Publication bounds.
    #[serde(default)]
    pub publication: PublicationSettings,
    /// The single task envelope and worker pool used for admitted work.
    pub task: TaskSettings,
    /// Worker pool token verifiers, one per organization/principal/pool.
    pub worker_verifiers: Vec<WorkerVerifierSettings>,
    /// Coordinator leadership lease.
    #[serde(default)]
    pub leadership: LeadershipSettings,
    /// How often the planning and publication loops run.
    #[serde(default)]
    pub loops: LoopSettings,
    /// Metrics listener and ledger snapshot bounds.
    #[serde(default)]
    pub observability: ObservabilitySettings,
}

/// PostgreSQL connection settings.
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

/// One worker token verifier.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerVerifierSettings {
    /// Organization the worker acts in.
    pub organization: OrganizationId,
    /// Service principal the worker session is bound to.
    pub principal: PrincipalId,
    /// Worker pool.
    pub pool: String,
    /// Hex SHA-256 verifier of the pool token.
    pub verifier: String,
}

impl WorkerVerifierSettings {
    /// Parses the verifier and builds the service's verifier list.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] for a malformed digest or empty pool.
    pub fn verifier(&self) -> Result<crate::workers::WorkerVerifier, ConfigError> {
        if self.pool.is_empty() {
            return Err(ConfigError::Invalid(
                "worker_verifiers[].pool must not be empty".into(),
            ));
        }
        let verifier = Sha256Digest::from_hex(&self.verifier).map_err(|_| {
            ConfigError::Invalid(format!(
                "worker_verifiers[] for {} has a malformed verifier",
                self.pool
            ))
        })?;
        Ok(crate::workers::WorkerVerifier {
            identity: bergsmith_persistence::workers::Identity {
                organization: self.organization,
                principal: self.principal,
                pool: self.pool.clone(),
            },
            verifier,
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
    #[expect(
        clippy::too_many_lines,
        reason = "one validation pass reports every cross-field invariant in order"
    )]
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.database.url()?;
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
        if self.worker_verifiers.is_empty() {
            return Err(ConfigError::Invalid(
                "worker_verifiers must name at least one pool".into(),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for verifier in &self.worker_verifiers {
            verifier.verifier()?;
            let key = (
                verifier.organization,
                verifier.principal,
                verifier.pool.clone(),
            );
            if !seen.insert(key) {
                return Err(ConfigError::Invalid(
                    "worker_verifiers contains a duplicate organization/principal/pool".into(),
                ));
            }
        }
        if !self
            .worker_verifiers
            .iter()
            .any(|verifier| verifier.pool == self.task.pool)
        {
            return Err(ConfigError::Invalid(
                "task.pool must have a matching worker verifier".into(),
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

    /// The worker token verifiers.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] for a malformed verifier.
    pub fn verifiers(&self) -> Result<Vec<crate::workers::WorkerVerifier>, ConfigError> {
        self.worker_verifiers
            .iter()
            .map(WorkerVerifierSettings::verifier)
            .collect()
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
            },
            "worker_verifiers": [{
                "organization": "018f0000-0000-7000-8000-000000000001",
                "principal": "018f0000-0000-7000-8000-000000000002",
                "pool": "default",
                "verifier": "0000000000000000000000000000000000000000000000000000000000000000"
            }]
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
    fn a_missing_worker_verifier_is_rejected() {
        let mut value = minimal();
        value["worker_verifiers"] = serde_json::json!([]);
        assert!(parse(value).is_err());
    }

    #[test]
    fn a_task_pool_without_a_verifier_is_rejected() {
        let mut value = minimal();
        value["task"]["pool"] = serde_json::json!("other");
        assert!(parse(value).is_err());
    }

    #[test]
    fn a_duplicate_verifier_is_rejected() {
        let mut value = minimal();
        let first = value["worker_verifiers"][0].clone();
        value["worker_verifiers"] = serde_json::json!([first.clone(), first]);
        assert!(parse(value).is_err());
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
