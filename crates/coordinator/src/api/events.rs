//! Reconnectable job event streams (BS-018.T3, ADR 0036).
//!
//! `GET /api/v1/jobs/{id}/events` serves one job's history as server-sent
//! events. Each event's `id` is the job's per-job sequence, which is assigned
//! under the job row lock and commits in order, so a client that resumes from
//! the last ID it saw receives every later event exactly once.
//!
//! PostgreSQL notifications only wake streams early. Every stream also polls
//! the ledger, so a lost notification or a listener outage delays delivery but
//! never drops an event. No database connection is held while a stream waits.

use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::{JobId, PrincipalId};
use bergsmith_common::job::JobState;
use bergsmith_persistence::auth::{self, Session};
use bergsmith_persistence::jobs::{self, JobEvent, JobEventKind};
use bergsmith_persistence::{Database, PersistenceError};
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;
use sqlx::postgres::{PgListener, PgPoolOptions};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast};
use tokio::time::Instant;
use uuid::Uuid;

use super::{ApiError, ApiState, Caller, resources};

/// The PostgreSQL channel migration 0013 notifies after each event append.
/// The payload is the job ID only.
pub const JOB_EVENT_CHANNEL: &str = "bergsmith_job_events";

/// The header a reconnecting `EventSource` sends with the last ID it saw.
const LAST_EVENT_ID: &str = "last-event-id";

/// Wake-ups buffered per stream before a slow stream treats the backlog as
/// "anything may have changed" and re-reads the ledger.
const WAKE_BUFFER: usize = 1_024;

/// How long the listener waits before reconnecting after a failure.
const LISTENER_RETRY: Duration = Duration::from_secs(1);

/// How long the listener waits to open its dedicated connection.
const LISTENER_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Bounds and timings for job event streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventStreamLimits {
    /// The most streams this process serves at once.
    pub max_streams: usize,
    /// The most job streams one principal holds at once.
    pub max_streams_per_principal: usize,
    /// The most organization streams (`/api/v1/events`) one session holds
    /// at once.
    pub max_live_streams_per_session: usize,
    /// The most events read from the ledger per query.
    pub replay_page: i64,
    /// How often a stream re-reads the ledger without a notification.
    pub poll_interval: Duration,
    /// How often an idle stream sends a keep-alive comment.
    pub keep_alive: Duration,
    /// How often the session and job access are checked again.
    pub recheck_interval: Duration,
    /// The longest a single stream stays open; the client then reconnects.
    pub max_duration: Duration,
}

impl Default for EventStreamLimits {
    fn default() -> Self {
        Self {
            max_streams: 256,
            max_streams_per_principal: 8,
            max_live_streams_per_session: 4,
            replay_page: 500,
            poll_interval: Duration::from_secs(2),
            keep_alive: Duration::from_secs(15),
            recheck_interval: Duration::from_secs(30),
            max_duration: Duration::from_secs(60 * 60),
        }
    }
}

/// Invalid [`EventStreamLimits`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("event stream limit {field} must be positive")]
pub struct EventStreamLimitsError {
    /// The field that was zero.
    pub field: &'static str,
}

impl EventStreamLimits {
    fn validate(&self) -> Result<(), EventStreamLimitsError> {
        let zero = [
            ("max_streams", self.max_streams == 0),
            (
                "max_streams_per_principal",
                self.max_streams_per_principal == 0,
            ),
            (
                "max_live_streams_per_session",
                self.max_live_streams_per_session == 0,
            ),
            ("replay_page", self.replay_page < 1),
            ("poll_interval", self.poll_interval.is_zero()),
            ("keep_alive", self.keep_alive.is_zero()),
            ("recheck_interval", self.recheck_interval.is_zero()),
            ("max_duration", self.max_duration.is_zero()),
        ];
        match zero.into_iter().find(|(_, zero)| *zero) {
            Some((field, _)) => Err(EventStreamLimitsError { field }),
            None => Ok(()),
        }
    }
}

/// Something may have been appended for `Job`, or for any job.
#[derive(Clone, Copy, Debug)]
pub(super) enum Wake {
    Job(JobId),
    All,
}

/// Admission and wake-up fan-out for job event streams.
///
/// Cloning shares the same limits and subscribers. Without
/// [`listen`](Self::listen), streams rely on polling alone.
#[derive(Clone, Debug)]
pub struct JobEventHub {
    inner: Arc<Hub>,
}

/// Who holds a stream: a principal's job streams and a session's
/// organization streams are counted separately.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Holder {
    Principal(PrincipalId),
    Session(Sha256Digest),
}

impl fmt::Debug for Holder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Principal(principal) => write!(f, "Principal({principal})"),
            // A session is named by its token's hash, which stays out of logs.
            Self::Session(_) => f.write_str("Session(..)"),
        }
    }
}

#[derive(Debug)]
struct Hub {
    limits: EventStreamLimits,
    wake: broadcast::Sender<Wake>,
    capacity: Arc<Semaphore>,
    /// Open streams per holder. An entry exists only while its count is
    /// positive, so memory is bounded by concurrent streams, not by every
    /// principal or session that ever connected.
    holders: Mutex<BTreeMap<Holder, usize>>,
}

impl Hub {
    /// The per-holder counts. The critical sections cannot panic, so a
    /// poisoned lock still holds consistent counts.
    fn holders(&self) -> MutexGuard<'_, BTreeMap<Holder, usize>> {
        self.holders.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Held for the life of one stream; dropping it frees the capacity.
#[derive(Debug)]
pub(super) struct StreamPermit {
    _process: OwnedSemaphorePermit,
    hub: Arc<Hub>,
    holder: Holder,
}

impl Drop for StreamPermit {
    fn drop(&mut self) {
        let mut holders = self.hub.holders();
        if let Some(open) = holders.get_mut(&self.holder) {
            *open = open.saturating_sub(1);
            if *open == 0 {
                holders.remove(&self.holder);
            }
        }
    }
}

impl JobEventHub {
    /// Creates a hub with `limits`.
    ///
    /// # Errors
    ///
    /// [`EventStreamLimitsError`] when a limit or interval is zero.
    pub fn new(limits: EventStreamLimits) -> Result<Self, EventStreamLimitsError> {
        limits.validate()?;
        let (wake, _) = broadcast::channel(WAKE_BUFFER);
        Ok(Self {
            inner: Arc::new(Hub {
                limits,
                wake,
                capacity: Arc::new(Semaphore::new(limits.max_streams)),
                holders: Mutex::new(BTreeMap::new()),
            }),
        })
    }

    /// The limits this hub enforces.
    #[must_use]
    pub fn limits(&self) -> EventStreamLimits {
        self.inner.limits
    }

    /// Starts listening on [`JOB_EVENT_CHANNEL`] so streams wake as soon as
    /// an event commits.
    ///
    /// The listener opens one dedicated PostgreSQL connection with
    /// `database`'s connect options, outside its pool: a `PgListener` holds
    /// its connection for as long as it listens, so sharing the request pool
    /// would permanently remove one connection from it. The listener
    /// reconnects after failures, and each (re)connection wakes every stream,
    /// because notifications sent while disconnected are lost. The task ends
    /// once every hub clone is dropped and the next notification or failure
    /// arrives; abort the handle to stop it sooner.
    #[must_use]
    pub fn listen(&self, database: &Database) -> tokio::task::JoinHandle<()> {
        let options = (*database.pool().connect_options()).clone();
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(LISTENER_CONNECT_TIMEOUT)
            .max_lifetime(None)
            .idle_timeout(None)
            .connect_lazy_with(options);
        tokio::spawn(run_listener(pool, Arc::downgrade(&self.inner)))
    }

    pub(super) fn subscribe(&self) -> broadcast::Receiver<Wake> {
        self.inner.wake.subscribe()
    }

    /// Reserves capacity for one job stream of `principal` without waiting.
    fn admit(&self, principal: PrincipalId) -> Option<StreamPermit> {
        self.admit_holder(
            Holder::Principal(principal),
            self.inner.limits.max_streams_per_principal,
        )
    }

    /// Reserves capacity for one organization stream of the session whose
    /// token hashes to `session`, without waiting.
    pub(super) fn admit_live(&self, session: Sha256Digest) -> Option<StreamPermit> {
        self.admit_holder(
            Holder::Session(session),
            self.inner.limits.max_live_streams_per_session,
        )
    }

    /// The per-holder check and increment happen under one lock, so
    /// concurrent requests cannot exceed the limit; a refused request
    /// releases its process permit on return.
    fn admit_holder(&self, holder: Holder, limit: usize) -> Option<StreamPermit> {
        let process = Arc::clone(&self.inner.capacity).try_acquire_owned().ok()?;
        {
            let mut holders = self.inner.holders();
            let open = holders.get(&holder).copied().unwrap_or(0);
            if open >= limit {
                return None;
            }
            holders.insert(holder, open + 1);
        }
        Some(StreamPermit {
            _process: process,
            hub: Arc::clone(&self.inner),
            holder,
        })
    }
}

/// Forwards notifications to subscribed streams until the hub is dropped.
async fn run_listener(pool: PgPool, hub: Weak<Hub>) {
    loop {
        match PgListener::connect_with(&pool).await {
            Ok(mut listener) => match listener.listen(JOB_EVENT_CHANNEL).await {
                Ok(()) => {
                    // Anything appended before the subscription is found by
                    // the re-read every woken stream performs.
                    if !wake(&hub, Wake::All) {
                        return;
                    }
                    loop {
                        let next = match listener.try_recv().await {
                            // A payload that is not a job ID wakes everyone
                            // rather than being dropped.
                            Ok(Some(notification)) => Uuid::parse_str(notification.payload())
                                .map_or(Wake::All, |id| Wake::Job(JobId::from_uuid(id))),
                            // The connection dropped and reconnects on the
                            // next call; notifications in between are lost.
                            Ok(None) => Wake::All,
                            Err(error) => {
                                tracing::warn!(error = %error, "job event listener failed");
                                break;
                            }
                        };
                        if !wake(&hub, next) {
                            return;
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, "job event listener could not subscribe");
                }
            },
            Err(error) => {
                tracing::warn!(error = %error, "job event listener could not connect");
            }
        }
        if !wake(&hub, Wake::All) {
            return;
        }
        tokio::time::sleep(LISTENER_RETRY).await;
    }
}

/// Sends a wake-up. Returns false once the hub no longer exists.
fn wake(hub: &Weak<Hub>, wake: Wake) -> bool {
    let Some(hub) = hub.upgrade() else {
        return false;
    };
    // No subscribers is not an error: nothing is streaming right now.
    let _ = hub.wake.send(wake);
    true
}

/// Query parameters for the event stream.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EventsQuery {
    pub(super) after: Option<String>,
}

/// `GET /api/v1/jobs/{id}/events`.
///
/// Authorizes exactly like `GET /api/v1/jobs/{id}` before any stream bytes,
/// so unknown and inaccessible jobs are both 404.
pub(super) async fn events(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
    query: Result<Query<EventsQuery>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = id.map_err(|_| ApiError::invalid())?.0;
    let query = query.map_err(|_| ApiError::invalid())?.0;
    let after = resume_point(&headers, query.after.as_deref())?;
    let job_state = resources::visible_job_state(&state, &caller.0, id).await?;
    let Some(permit) = state.events.admit(caller.0.principal_id) else {
        return Err(ApiError::transient(
            "event_stream_capacity_exhausted",
            "retry_same_request",
        ));
    };
    let limits = state.events.limits();
    let now = Instant::now();
    let stream = EventStream {
        wake: Some(state.events.subscribe()),
        state,
        caller: caller.0,
        job: JobId::from_uuid(id),
        cursor: after,
        terminal: job_state.is_terminal(),
        drained: false,
        buffer: VecDeque::new(),
        limits,
        deadline: now + limits.max_duration,
        next_recheck: now + limits.recheck_interval,
        _permit: permit,
    };
    let events = futures::stream::unfold(stream, |mut stream| async move {
        stream
            .next()
            .await
            .map(|event| (Ok::<_, Infallible>(event), stream))
    });
    Ok(Sse::new(events)
        .keep_alive(KeepAlive::new().interval(limits.keep_alive))
        .into_response())
}

/// The sequence to resume after: `Last-Event-ID` wins over `after`, and
/// neither means the start of the history.
pub(super) fn resume_point(headers: &HeaderMap, after: Option<&str>) -> Result<i64, ApiError> {
    let header = headers
        .get(LAST_EVENT_ID)
        .map(|value| value.to_str().map_err(|_| ApiError::invalid()))
        .transpose()?;
    match header.or(after) {
        None => Ok(0),
        Some(value) => parse_sequence(value).ok_or_else(ApiError::invalid),
    }
}

/// A non-negative decimal sequence without sign or surrounding space.
fn parse_sequence(value: &str) -> Option<i64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

/// The state of one open stream.
struct EventStream {
    state: ApiState,
    caller: Session,
    job: JobId,
    /// The last sequence delivered or skipped by the resume point.
    cursor: i64,
    /// Whether the job has reached a terminal state.
    terminal: bool,
    /// Whether every event up to the terminal one has been read.
    drained: bool,
    buffer: VecDeque<Event>,
    limits: EventStreamLimits,
    wake: Option<broadcast::Receiver<Wake>>,
    deadline: Instant,
    next_recheck: Instant,
    _permit: StreamPermit,
}

impl EventStream {
    /// The next event to send, or `None` to end the stream.
    ///
    /// The whole operation, including connection acquisition, queries, and
    /// rechecks, races the stream's deadline: a stalled database call cannot
    /// keep the stream (and its admission permits) open past its lifetime,
    /// and an event is never yielded after it. An interrupted operation ends
    /// the stream, so its partial state is never observed.
    async fn next(&mut self) -> Option<Event> {
        let deadline = self.deadline;
        tokio::time::timeout_at(deadline, self.next_before_deadline())
            .await
            .ok()
            .flatten()
    }

    async fn next_before_deadline(&mut self) -> Option<Event> {
        loop {
            // The deadline and a due recheck apply to buffered events too: a
            // slow consumer must not keep receiving after its session or
            // access ended, or after the stream's lifetime.
            if Instant::now() >= self.deadline {
                return None;
            }
            if Instant::now() >= self.next_recheck {
                self.terminal |= self.recheck().await?.is_terminal();
                self.next_recheck = Instant::now() + self.limits.recheck_interval;
            }
            if let Some(event) = self.buffer.pop_front() {
                return Some(event);
            }
            if self.drained {
                return None;
            }
            let page = self.read().await?;
            let full = i64::try_from(page.len()).is_ok_and(|len| len >= self.limits.replay_page);
            for event in &page {
                self.cursor = event.sequence;
                self.terminal |= event.to_state.is_terminal();
                self.buffer.push_back(sse_event(event)?);
            }
            // Terminal jobs never change, so a short read after the terminal
            // state means the history is complete.
            if self.terminal && !full {
                self.drained = true;
                continue;
            }
            if page.is_empty() {
                self.wait().await;
            }
        }
    }

    /// Checks the session (without extending it) and job access again.
    async fn recheck(&self) -> Option<JobState> {
        let checked = async {
            let mut conn = self.state.database.pool().acquire().await?;
            match auth::verify_session(
                &mut conn,
                self.caller.token_hash,
                self.state.sessions.idle_timeout,
            )
            .await
            {
                Ok(()) => {}
                Err(PersistenceError::SessionRejected { .. }) => {
                    return Err(ApiError::unauthorized());
                }
                Err(error) => return Err(error.into()),
            }
            drop(conn);
            resources::visible_job_state(&self.state, &self.caller, *self.job.as_uuid()).await
        };
        match checked.await {
            Ok(state) => Some(state),
            Err(error) => {
                tracing::debug!(
                    job_id = %self.job,
                    reason = error.reason,
                    "job event stream ended by recheck"
                );
                None
            }
        }
    }

    async fn read(&self) -> Option<Vec<JobEvent>> {
        let read = async {
            let mut conn = self.state.database.pool().acquire().await?;
            jobs::job_events_after(&mut conn, self.job, self.cursor, self.limits.replay_page).await
        };
        match read.await {
            Ok(events) => Some(events),
            Err(error) => {
                // The client reconnects with its last event ID.
                tracing::warn!(job_id = %self.job, error = %error, "job event stream read failed");
                None
            }
        }
    }

    /// Waits for a relevant notification, the poll interval, the next
    /// recheck, or the deadline, whichever comes first.
    async fn wait(&mut self) {
        let until = (Instant::now() + self.limits.poll_interval)
            .min(self.next_recheck)
            .min(self.deadline);
        let sleep = tokio::time::sleep_until(until);
        tokio::pin!(sleep);
        let Some(wake) = self.wake.as_mut() else {
            sleep.await;
            return;
        };
        loop {
            tokio::select! {
                () = &mut sleep => return,
                received = wake.recv() => match received {
                    Ok(Wake::Job(job)) if job != self.job => {}
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => return,
                    Err(broadcast::error::RecvError::Closed) => {
                        self.wake = None;
                        sleep.await;
                        return;
                    }
                },
            }
        }
    }
}

/// The SSE `event:` name for a kind.
const fn event_name(kind: JobEventKind) -> &'static str {
    match kind {
        JobEventKind::Created => "created",
        JobEventKind::Transitioned => "transitioned",
        JobEventKind::CancellationRequested => "cancellation_requested",
        JobEventKind::PublicationAuthorized => "publication_authorized",
    }
}

/// Encodes one ledger event.
///
/// Only typed transition reasons are exposed. A cancellation request's reason
/// is operator text, which the job resource does not expose either.
fn sse_event(event: &JobEvent) -> Option<Event> {
    let reason = match event.kind {
        JobEventKind::Transitioned => event.reason.as_deref(),
        _ => None,
    };
    let data = json!({
        "sequence": event.sequence,
        "kind": event_name(event.kind),
        "from_state": event.from_state.map(JobState::as_str),
        "to_state": event.to_state.as_str(),
        "state_revision": event.state_revision,
        "reason": reason,
        "actor_id": event.actor,
        "occurred_at": event.occurred_at,
    });
    Event::default()
        .id(event.sequence.to_string())
        .event(event_name(event.kind))
        .json_data(data)
        .inspect_err(|error| tracing::error!(error = %error, "job event could not be encoded"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(last: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(last) = last {
            headers.insert(LAST_EVENT_ID, HeaderValue::from_str(last).unwrap());
        }
        headers
    }

    #[test]
    fn the_resume_point_prefers_the_header_and_defaults_to_the_start() {
        assert_eq!(resume_point(&headers(None), None).unwrap(), 0);
        assert_eq!(resume_point(&headers(None), Some("4")).unwrap(), 4);
        assert_eq!(resume_point(&headers(Some("7")), Some("4")).unwrap(), 7);
        assert_eq!(resume_point(&headers(Some("0")), None).unwrap(), 0);
    }

    #[test]
    fn malformed_resume_points_are_rejected() {
        for value in ["", "-1", "+1", " 1", "1.5", "abc", "99999999999999999999"] {
            assert!(
                resume_point(&headers(None), Some(value)).is_err(),
                "query {value:?}"
            );
            if !value.starts_with(' ') && !value.is_empty() {
                assert!(
                    resume_point(&headers(Some(value)), None).is_err(),
                    "header {value:?}"
                );
            }
        }
        // A malformed header is rejected even when the query is valid.
        assert!(resume_point(&headers(Some("x")), Some("1")).is_err());
    }

    #[test]
    fn zero_limits_are_rejected() {
        assert!(JobEventHub::new(EventStreamLimits::default()).is_ok());
        let error = JobEventHub::new(EventStreamLimits {
            poll_interval: Duration::ZERO,
            ..EventStreamLimits::default()
        })
        .unwrap_err();
        assert_eq!(error.field, "poll_interval");
        let error = JobEventHub::new(EventStreamLimits {
            replay_page: 0,
            ..EventStreamLimits::default()
        })
        .unwrap_err();
        assert_eq!(error.field, "replay_page");
    }

    #[test]
    fn event_names_are_snake_case_kinds() {
        for (kind, name) in [
            (JobEventKind::Created, "created"),
            (JobEventKind::Transitioned, "transitioned"),
            (
                JobEventKind::CancellationRequested,
                "cancellation_requested",
            ),
            (
                JobEventKind::PublicationAuthorized,
                "publication_authorized",
            ),
        ] {
            assert_eq!(event_name(kind), name);
        }
    }

    #[test]
    fn documented_event_enums_match_the_rust_names() {
        let document: serde_json::Value =
            serde_json::from_str(include_str!("openapi.json")).unwrap();
        let event = &document["components"]["schemas"]["JobEvent"]["properties"];
        let kinds: Vec<_> = [
            JobEventKind::Created,
            JobEventKind::Transitioned,
            JobEventKind::CancellationRequested,
            JobEventKind::PublicationAuthorized,
        ]
        .into_iter()
        .map(|kind| json!(event_name(kind)))
        .collect();
        assert_eq!(event["kind"]["enum"].as_array().unwrap(), &kinds);
        let states: Vec<_> = JobState::ALL
            .into_iter()
            .map(|state| json!(state.as_str()))
            .collect();
        assert_eq!(event["to_state"]["enum"].as_array().unwrap(), &states);
        assert_eq!(
            event["from_state"]["anyOf"][0]["enum"].as_array().unwrap(),
            &states
        );
    }

    #[test]
    fn admission_is_bounded_per_process_and_per_principal() {
        let hub = JobEventHub::new(EventStreamLimits {
            max_streams: 3,
            max_streams_per_principal: 2,
            ..EventStreamLimits::default()
        })
        .unwrap();
        let (alice, bob) = (PrincipalId::generate(), PrincipalId::generate());
        let first = hub.admit(alice).unwrap();
        let _second = hub.admit(alice).unwrap();
        assert!(hub.admit(alice).is_none(), "per-principal limit");
        let _third = hub.admit(bob).unwrap();
        assert!(hub.admit(bob).is_none(), "process limit");
        drop(first);
        assert!(hub.admit(alice).is_some(), "a closed stream frees capacity");
    }

    #[test]
    fn session_streams_are_bounded_apart_from_job_streams() {
        let hub = JobEventHub::new(EventStreamLimits {
            max_streams_per_principal: 1,
            max_live_streams_per_session: 2,
            ..EventStreamLimits::default()
        })
        .unwrap();
        let session = Sha256Digest::of(b"test", b"session");
        let _job = hub.admit(PrincipalId::generate()).unwrap();
        let first = hub.admit_live(session).unwrap();
        let _second = hub.admit_live(session).unwrap();
        assert!(hub.admit_live(session).is_none(), "per-session limit");
        assert!(
            hub.admit_live(Sha256Digest::of(b"test", b"other"))
                .is_some()
        );
        drop(first);
        assert!(
            hub.admit_live(session).is_some(),
            "a closed stream frees capacity"
        );
        assert_eq!(
            format!("{:?}", Holder::Session(session)),
            "Session(..)",
            "a session's token hash stays out of logs"
        );
    }

    #[test]
    fn admission_entries_are_reclaimed_when_streams_close() {
        let hub = JobEventHub::new(EventStreamLimits {
            max_streams_per_principal: 1,
            ..EventStreamLimits::default()
        })
        .unwrap();
        for _ in 0..1_000 {
            drop(hub.admit(PrincipalId::generate()).unwrap());
        }
        assert!(hub.inner.holders().is_empty());

        let alice = PrincipalId::generate();
        let open = hub.admit(alice).unwrap();
        // A refused request neither leaks an entry nor changes the count.
        assert!(hub.admit(alice).is_none());
        assert_eq!(hub.inner.holders().get(&Holder::Principal(alice)), Some(&1));
        drop(open);
        assert!(hub.inner.holders().is_empty());
        // The process permit of the refused request was returned.
        assert_eq!(
            hub.inner.capacity.available_permits(),
            EventStreamLimits::default().max_streams
        );
    }
}
