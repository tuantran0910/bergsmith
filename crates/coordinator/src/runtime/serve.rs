//! The coordinator process runtime (BS-020.T1).
//!
//! One process owns the active coordinator epoch. It serves the worker RPC
//! endpoint, runs reconciliation, plans and publishes work, and serves the
//! browser API and UI on one origin. Shutdown stops new dispatch, drains the
//! HTTP listeners, and releases the leadership lease.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bergsmith_persistence::{Database, DatabaseConfig, DatabaseUrl, PersistenceError, leadership};
use tokio::sync::watch;
use tonic::transport::{Identity, Server};
use tracing::{info, warn};
use uuid::Uuid;

use crate::api::{self, EventStreamLimits, JobEventHub};
use crate::checks::CapabilityChecker;
use crate::discovery::{CatalogDiscovery, DiscoveryLimits};
use crate::identity::oidc::OidcProvider;
use crate::identity::routes::IdentityState;
use crate::observability::{Observability, RpcMetrics};
use crate::preview::{CatalogPreviewer, PreviewProfiles};
use crate::runtime::catalogs::{RuntimeCatalogs, RuntimeWorkerAccess};
use crate::runtime::config::CoordinatorConfig;
use crate::runtime::http;
use crate::runtime::planning::PlannerService;
use crate::runtime::publication::PublicationService;
use crate::scheduling::Scheduler;
use crate::workers::{WorkerConfig, WorkerService, worker_tls_config};

const LEADERSHIP_RETRY: Duration = Duration::from_secs(2);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
const OIDC_DISCOVERY_ATTEMPTS: u32 = 30;

/// A runtime startup or listener failure.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// The configuration document is invalid.
    #[error(transparent)]
    Config(#[from] crate::runtime::config::ConfigError),
    /// Database settings are invalid or the database is unreachable.
    #[error("database connection failed: {0}")]
    Database(#[from] PersistenceError),
    /// The leadership lease could not be acquired.
    #[error("coordinator leadership could not be acquired")]
    Leadership,
    /// OIDC discovery failed.
    #[error("OIDC provider discovery failed: {0}")]
    Oidc(#[from] crate::identity::oidc::OidcError),
    /// The browser surface could not be assembled.
    #[error(transparent)]
    Http(#[from] http::HttpError),
    /// The worker RPC certificate or key could not be read.
    #[error("worker TLS identity {path} could not be read: {source}")]
    Tls {
        /// File path.
        path: std::path::PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A configured listener address is invalid.
    #[error("{listener} listener address {address} is invalid")]
    Bind {
        /// Listener name.
        listener: &'static str,
        /// Configured address.
        address: String,
    },
    /// A listener failed while running.
    #[error("{listener} listener failed: {message}")]
    Listener {
        /// Listener name.
        listener: &'static str,
        /// Sanitized error text.
        message: String,
    },
    /// A background task ended unexpectedly.
    #[error("{task} task ended unexpectedly")]
    Task {
        /// Task name.
        task: &'static str,
    },
}

/// Serves until a shutdown signal arrives.
///
/// # Errors
///
/// [`ServeError`] for an invalid configuration, unreachable dependency,
/// failed leadership acquisition, or listener failure.
#[expect(
    clippy::too_many_lines,
    reason = "one startup order composes every runtime service; splitting it would hide the ordering"
)]
pub async fn serve(config: CoordinatorConfig) -> Result<(), ServeError> {
    config.validate()?;
    let database = connect(&config).await?;
    let shutdown = Shutdown::new();
    spawn_signals(shutdown.clone());
    let leadership = acquire(&database, &config, &shutdown).await?;
    let epoch = leadership.epoch;
    info!(epoch, "coordinator leadership acquired");

    let mut tasks: tokio::task::JoinSet<RuntimeTaskResult> = tokio::task::JoinSet::new();
    // Keep the lease alive while OIDC discovery and service construction run.
    // Discovery can take longer than the configured lease; starting renewal
    // only after it would leave the process with an expired epoch.
    {
        let shutdown = shutdown.clone();
        let database = database.clone();
        let leadership = leadership.clone();
        let settings = config.leadership;
        tasks.spawn(async move {
            renew(database, leadership, settings, shutdown).await;
            ("leadership", Ok(()))
        });
    }
    let started = match start(&config, &database, &shutdown, epoch).await {
        Ok(started) => started,
        Err(error) => {
            shutdown.trigger();
            tasks.shutdown().await;
            if let Err(release_error) = release(&database, &leadership).await {
                warn!(
                    error = %release_error,
                    "coordinator leadership release failed after startup error"
                );
            }
            return Err(error);
        }
    };
    if shutdown.is_triggered() {
        tasks.shutdown().await;
        let _ = release(&database, &leadership).await;
        return Err(ServeError::Leadership);
    }
    let Started {
        app,
        http_bind,
        worker_bind,
        metrics_bind,
        observability,
        tls,
        events,
        scheduler,
        planner,
        publication,
    } = started;
    let listener = events.listen(&database);
    let http_shutdown = shutdown.clone();
    tasks.spawn(async move {
        (
            "http",
            serve_router(app, http_bind, "http", http_shutdown).await,
        )
    });
    let worker_service = scheduler.worker_service();
    let worker_shutdown = shutdown.clone();
    let rpc_metrics = observability.rpc().clone();
    tasks.spawn(async move {
        (
            "worker",
            serve_worker(
                worker_service,
                worker_bind,
                tls,
                rpc_metrics,
                worker_shutdown,
            )
            .await,
        )
    });
    if let Some(bind) = metrics_bind {
        let metrics_shutdown = shutdown.clone();
        let router = crate::observability::router(observability);
        tasks.spawn(async move {
            (
                "metrics",
                serve_router(router, bind, "metrics", metrics_shutdown).await,
            )
        });
    }
    {
        let shutdown = shutdown.clone();
        let scheduler = scheduler.clone();
        tasks.spawn(async move {
            scheduler.run(shutdown.wait()).await;
            ("scheduler", Ok(()))
        });
    }
    {
        let shutdown = shutdown.clone();
        let interval = Duration::from_millis(config.loops.planning_millis);
        tasks.spawn(async move {
            planner.run(interval, shutdown.wait()).await;
            ("planner", Ok(()))
        });
    }
    {
        let shutdown = shutdown.clone();
        let interval = Duration::from_millis(config.loops.publication_millis);
        tasks.spawn(async move {
            publication.run(interval, shutdown.wait()).await;
            ("publication", Ok(()))
        });
    }
    let first = tokio::select! {
        () = shutdown.wait() => None,
        joined = tasks.join_next() => Some(joined),
    };
    let mut failure = None;
    if let Some(joined) = first
        && !shutdown.is_triggered()
    {
        failure = Some(unexpected_task_exit(joined));
        shutdown.trigger();
    }

    // Stop admission first and release the lease before draining listeners, so
    // a replacement can acquire the next epoch while this process finishes
    // closing connections. Already authorized sends are resolved by evidence
    // recovery, exactly as after a crash.
    info!("coordinator shutting down");
    shutdown.trigger();
    listener.abort();
    if let Err(error) = release(&database, &leadership).await {
        warn!(error = %error, "coordinator leadership release failed; the lease will expire");
    }
    if tokio::time::timeout(SHUTDOWN_GRACE, async {
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok((task, Err(error))) => {
                    warn!(task, reason = %error, "runtime task ended with an error during shutdown");
                }
                Ok((_, Ok(()))) => {}
                Err(error) => {
                    warn!(error = %error, "runtime task did not end cleanly during shutdown");
                }
            }
        }
    })
    .await
    .is_err()
    {
        tasks.abort_all();
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

type RuntimeTaskResult = (&'static str, Result<(), ServeError>);

fn unexpected_task_exit(
    joined: Option<Result<RuntimeTaskResult, tokio::task::JoinError>>,
) -> ServeError {
    match joined {
        Some(Ok((task, result))) => {
            warn!(task, "runtime task ended; stopping the coordinator");
            result.err().unwrap_or(ServeError::Task { task })
        }
        Some(Err(error)) => {
            warn!(error = %error, "runtime task failed; stopping the coordinator");
            ServeError::Task { task: "background" }
        }
        None => ServeError::Task { task: "runtime" },
    }
}

struct Started {
    app: axum::Router,
    http_bind: SocketAddr,
    worker_bind: SocketAddr,
    metrics_bind: Option<SocketAddr>,
    observability: Observability,
    tls: Identity,
    events: JobEventHub,
    scheduler: Arc<Scheduler>,
    planner: PlannerService,
    publication: PublicationService,
}

#[expect(
    clippy::too_many_lines,
    reason = "one startup order composes every runtime service; splitting it would hide the ordering"
)]
async fn start(
    config: &CoordinatorConfig,
    database: &Database,
    shutdown: &Shutdown,
    epoch: i64,
) -> Result<Started, ServeError> {
    let sessions = config.session().map_err(|error| {
        ServeError::Database(PersistenceError::InvalidValue {
            entity: "session configuration",
            constraint: error.to_string(),
        })
    })?;
    let provider = discover_provider(config, shutdown).await?;
    let preview_profiles = config.storage.profiles().map_err(|error| {
        ServeError::Database(PersistenceError::InvalidValue {
            entity: "storage configuration",
            constraint: error.to_string(),
        })
    })?;
    let mut preview_profiles_by_connection = PreviewProfiles::new(preview_profiles.default.clone());
    for (connection, profiles) in &preview_profiles.connections {
        preview_profiles_by_connection =
            preview_profiles_by_connection.with_connection(*connection, profiles.clone());
    }
    let previews = CatalogPreviewer::new(preview_profiles_by_connection, config.planner.clone())
        .map_err(|error| {
            ServeError::Database(PersistenceError::InvalidValue {
                entity: "preview planner",
                constraint: error.to_string(),
            })
        })?;
    let events = JobEventHub::new(EventStreamLimits::default()).map_err(|error| {
        ServeError::Database(PersistenceError::InvalidValue {
            entity: "event stream limits",
            constraint: error.to_string(),
        })
    })?;
    let catalogs = Arc::new(RuntimeCatalogs::new(database.clone(), preview_profiles));
    let api = api::ApiState {
        database: database.clone(),
        sessions: sessions.clone(),
        previews,
        events: events.clone(),
        checks: CapabilityChecker::default(),
        discovery: CatalogDiscovery::new(catalogs.clone(), DiscoveryLimits::default()),
    };
    let identity = IdentityState::new(database.clone(), Arc::new(provider), sessions);

    let access = Arc::new(RuntimeWorkerAccess::new(catalogs.clone()));
    let verifiers = config.verifiers().map_err(|error| {
        ServeError::Database(PersistenceError::InvalidValue {
            entity: "worker verifiers",
            constraint: error.to_string(),
        })
    })?;
    let worker = WorkerService::new(
        database.pool().clone(),
        epoch,
        verifiers,
        WorkerConfig::default(),
        access.clone(),
    )
    .map_err(|error| ServeError::Listener {
        listener: "worker",
        message: error.message().to_owned(),
    })?;
    let scheduler_config = config.scheduler.scheduler().map_err(|error| {
        ServeError::Database(PersistenceError::InvalidValue {
            entity: "scheduler",
            constraint: error.to_string(),
        })
    })?;
    let scheduler = Arc::new(
        Scheduler::new(worker, access, scheduler_config.clone()).map_err(|error| {
            ServeError::Database(PersistenceError::InvalidValue {
                entity: "scheduler",
                constraint: error.to_string(),
            })
        })?,
    );
    let planner = PlannerService::new(
        database.pool().clone(),
        catalogs.clone(),
        scheduler.clone(),
        config.planner.clone(),
        config.task.clone(),
        scheduler_config.max_job_input_bytes,
        scheduler_config.retry,
    )
    .map_err(|error| {
        ServeError::Database(PersistenceError::InvalidValue {
            entity: "planner",
            constraint: error.to_string(),
        })
    })?;
    let publication = PublicationService::new(
        database.pool().clone(),
        catalogs,
        epoch,
        config.publication.config().map_err(|error| {
            ServeError::Database(PersistenceError::InvalidValue {
                entity: "publication",
                constraint: error.to_string(),
            })
        })?,
        scheduler_config.retry,
    );

    let observability = Observability::new(
        database.clone(),
        config.observability.metrics().map_err(|error| {
            ServeError::Database(PersistenceError::InvalidValue {
                entity: "observability",
                constraint: error.to_string(),
            })
        })?,
    );
    let metrics_bind = config
        .observability
        .metrics_bind
        .as_deref()
        .map(|bind| parse_bind(bind, "metrics"))
        .transpose()?;

    Ok(Started {
        app: http::app(api, identity, config.http.web_root.clone())?,
        http_bind: parse_bind(&config.http.bind, "http")?,
        worker_bind: parse_bind(&config.worker.bind, "worker")?,
        metrics_bind,
        observability,
        tls: tls_identity(config)?,
        events,
        scheduler,
        planner,
        publication,
    })
}

async fn connect(config: &CoordinatorConfig) -> Result<Database, ServeError> {
    let url = config.database.url().map_err(|error| {
        ServeError::Database(PersistenceError::InvalidValue {
            entity: "database URL",
            constraint: error.to_string(),
        })
    })?;
    let url = DatabaseUrl::new(url);
    crate::database::warn_unless_verified(&url);
    let mut settings = DatabaseConfig::new(url);
    settings.max_connections = config.database.max_connections;
    settings.acquire_timeout = Duration::from_secs(config.database.acquire_timeout_seconds);
    Ok(Database::connect(&settings).await?)
}

async fn acquire(
    database: &Database,
    config: &CoordinatorConfig,
    shutdown: &Shutdown,
) -> Result<leadership::Leadership, ServeError> {
    let holder = Uuid::now_v7().to_string();
    let lease = Duration::from_secs(config.leadership.lease_seconds);
    loop {
        let mut conn = database
            .pool()
            .acquire()
            .await
            .map_err(PersistenceError::from)?;
        if let Some(leadership) = leadership::acquire_leadership(&mut conn, &holder, lease).await? {
            return Ok(leadership);
        }
        drop(conn);
        info!("another coordinator holds the leadership lease; waiting");
        tokio::select! {
            () = shutdown.wait() => return Err(ServeError::Leadership),
            () = tokio::time::sleep(LEADERSHIP_RETRY) => {}
        }
    }
}

async fn discover_provider(
    config: &CoordinatorConfig,
    shutdown: &Shutdown,
) -> Result<OidcProvider, ServeError> {
    let oidc = config.oidc().map_err(|error| {
        ServeError::Database(PersistenceError::InvalidValue {
            entity: "OIDC configuration",
            constraint: error.to_string(),
        })
    })?;
    let mut attempt = 0;
    loop {
        match OidcProvider::discover(oidc.clone()).await {
            Ok(provider) => return Ok(provider),
            Err(error) if attempt < OIDC_DISCOVERY_ATTEMPTS => {
                attempt += 1;
                warn!(attempt, "OIDC discovery failed; retrying: {error}");
                tokio::select! {
                    () = shutdown.wait() => return Err(ServeError::Oidc(error)),
                    () = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
            }
            Err(error) => return Err(ServeError::Oidc(error)),
        }
    }
}

fn parse_bind(address: &str, listener: &'static str) -> Result<SocketAddr, ServeError> {
    address.parse().map_err(|_| ServeError::Bind {
        listener,
        address: address.to_owned(),
    })
}

fn tls_identity(config: &CoordinatorConfig) -> Result<Identity, ServeError> {
    let certificate =
        std::fs::read(&config.worker.certificate).map_err(|source| ServeError::Tls {
            path: config.worker.certificate.clone(),
            source,
        })?;
    let key = std::fs::read(&config.worker.private_key).map_err(|source| ServeError::Tls {
        path: config.worker.private_key.clone(),
        source,
    })?;
    Ok(Identity::from_pem(certificate, key))
}

async fn serve_router(
    app: axum::Router,
    bind: SocketAddr,
    listener_name: &'static str,
    shutdown: Shutdown,
) -> Result<(), ServeError> {
    let listener =
        tokio::net::TcpListener::bind(bind)
            .await
            .map_err(|error| ServeError::Listener {
                listener: listener_name,
                message: error.to_string(),
            })?;
    info!(%bind, listener = listener_name, "http listener started");
    let wait = {
        let shutdown = shutdown.clone();
        async move { shutdown.wait().await }
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(wait)
        .await
        .map_err(|error| ServeError::Listener {
            listener: listener_name,
            message: error.to_string(),
        })
}

async fn serve_worker(
    service: WorkerService,
    bind: SocketAddr,
    identity: Identity,
    rpc_metrics: RpcMetrics,
    shutdown: Shutdown,
) -> Result<(), ServeError> {
    info!(%bind, "worker RPC listening");
    Server::builder()
        .layer(rpc_metrics.layer())
        .tls_config(worker_tls_config(identity))
        .map_err(|error| ServeError::Listener {
            listener: "worker",
            message: error.to_string(),
        })?
        .add_service(
            bergsmith_protocol::cluster::cluster_service_server::ClusterServiceServer::new(service),
        )
        .serve_with_shutdown(bind, async move { shutdown.wait().await })
        .await
        .map_err(|error| ServeError::Listener {
            listener: "worker",
            message: error.to_string(),
        })
}

async fn renew(
    database: Database,
    leadership: leadership::Leadership,
    settings: crate::runtime::config::LeadershipSettings,
    shutdown: Shutdown,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(settings.renew_seconds));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = shutdown.wait() => return,
            _ = tick.tick() => {
                let mut conn = match database.pool().acquire().await {
                    Ok(conn) => conn,
                    Err(error) => {
                        warn!(error = %error, "leadership renewal connection unavailable");
                        continue;
                    }
                };
                match leadership::renew_leadership(
                    &mut conn,
                    &leadership.holder,
                    leadership.epoch,
                    Duration::from_secs(settings.lease_seconds),
                )
                .await
                {
                    Ok(_) => {}
                    Err(PersistenceError::LeadershipLost { .. }) => {
                        warn!("coordinator leadership lost; stopping new dispatch and publication");
                        shutdown.trigger();
                        return;
                    }
                    Err(error) => {
                        warn!(error = %error, "leadership renewal failed; retrying");
                    }
                }
            }
        }
    }
}

async fn release(
    database: &Database,
    leadership: &leadership::Leadership,
) -> Result<(), PersistenceError> {
    let mut conn = database
        .pool()
        .acquire()
        .await
        .map_err(PersistenceError::from)?;
    leadership::release_leadership(&mut conn, &leadership.holder, leadership.epoch).await
}

/// A one-way shutdown signal shared by every task.
#[derive(Clone)]
struct Shutdown {
    sender: watch::Sender<bool>,
}

impl Shutdown {
    fn new() -> Self {
        let (sender, _) = watch::channel(false);
        Self { sender }
    }

    fn trigger(&self) {
        // `send` fails when no receiver exists; startup can receive SIGTERM
        // before any waiter subscribes. `send_replace` stores the value for the
        // next subscriber instead of dropping the signal.
        self.sender.send_replace(true);
    }

    fn is_triggered(&self) -> bool {
        *self.sender.borrow()
    }

    async fn wait(&self) {
        let mut receiver = self.sender.subscribe();
        if *receiver.borrow() {
            return;
        }
        while receiver.changed().await.is_ok() {
            if *receiver.borrow() {
                return;
            }
        }
    }
}

fn spawn_signals(shutdown: Shutdown) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let Ok(mut terminate) = signal(SignalKind::terminate()) else {
                let _ = tokio::signal::ctrl_c().await;
                shutdown.trigger();
                return;
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        shutdown.trigger();
    });
}

#[cfg(test)]
mod tests {
    use super::{ServeError, Shutdown, unexpected_task_exit};
    use std::time::Duration;

    #[tokio::test]
    async fn a_shutdown_signal_before_a_waiter_is_remembered() {
        let shutdown = Shutdown::new();
        shutdown.trigger();
        tokio::time::timeout(Duration::from_millis(50), shutdown.wait())
            .await
            .expect("a waiter must observe an earlier signal");
    }
    #[tokio::test]
    async fn a_background_panic_is_a_runtime_failure() {
        let joined = tokio::spawn(async { panic!("background task failed") }).await;
        let error = unexpected_task_exit(Some(joined));
        assert!(matches!(error, ServeError::Task { .. }));
    }

    #[test]
    fn an_unexpected_clean_exit_is_a_runtime_failure() {
        assert!(matches!(
            unexpected_task_exit(Some(Ok(("planner", Ok(()))))),
            ServeError::Task { task: "planner" }
        ));
        assert!(matches!(
            unexpected_task_exit(None),
            ServeError::Task { .. }
        ));
    }

    #[test]
    fn listener_failures_keep_their_original_error() {
        let error = ServeError::Listener {
            listener: "http",
            message: "bind failed".into(),
        };
        assert!(matches!(
            unexpected_task_exit(Some(Ok(("http", Err(error))))),
            ServeError::Listener {
                listener: "http",
                ..
            }
        ));
    }
}
