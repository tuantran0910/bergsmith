//! The organization's live job stream (BS-033.T7, ADR 0067).
//!
//! `GET /api/v1/events` tells a signed-in browser which of the jobs it may
//! list changed state, so open pages refresh only the affected rows. An event
//! carries a job's identifiers, state, and revision: never a name, reason, or
//! actor. Its SSE `id` is the job event's ledger ID, and a reconnecting
//! client resumes after the last one it saw.
//!
//! Ledger IDs are assigned when a row is inserted, so a later ID can commit
//! before an earlier one. A stream reads only up to a settled horizon: it
//! reads the ID sequence's last value and then the transactions in progress;
//! once all of those have ended, no event at or below that value can still
//! appear, so resuming after a delivered ID never skips one. Rows are scoped
//! by the jobs list's grant predicate inside the query.
//!
//! Streams share the job event hub's wake-ups and process capacity; each
//! session may hold [`EventStreamLimits::max_live_streams_per_session`]. A
//! stream ends when its session ends or its lifetime is reached, and the
//! client then reconnects.

use std::collections::{BTreeSet, VecDeque};
use std::convert::Infallible;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use bergsmith_common::permission::Permission;
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::auth::{self, Session};
use serde_json::json;
use tokio::sync::broadcast;
use tokio::time::Instant;
use uuid::Uuid;

use super::events::{EventStreamLimits, EventsQuery, StreamPermit, Wake, resume_point};
use super::lists::scoped;
use super::{ApiError, ApiState, Caller};

/// The most ledger IDs a resuming stream replays. Further behind, it sends
/// `reset` instead, and the client reloads what it shows.
const MAX_REPLAY: i64 = 10_000;

/// The most horizon marks a stream keeps while transactions are in
/// progress. Dropping the oldest only delays delivery; it never skips.
const MAX_MARKS: usize = 64;

/// Ledger IDs waiting to settle.
///
/// Each mark is a sequence value and the transactions that were in progress
/// just after it was read. An ID at or below the value was assigned by a
/// transaction that had already started, so once none of those transactions
/// is in progress, every such ID is either visible or never will be.
#[derive(Debug, Default)]
struct Horizon {
    marks: VecDeque<(i64, BTreeSet<u64>)>,
}

impl Horizon {
    /// Records `last`, read before `in_progress` was taken, and returns the
    /// highest value that has settled, if any.
    fn advance(&mut self, last: i64, in_progress: &BTreeSet<u64>) -> Option<i64> {
        let mut settled = in_progress.is_empty().then_some(last);
        for (value, held) in &self.marks {
            if held.is_disjoint(in_progress) {
                settled = settled.max(Some(*value));
            }
        }
        if let Some(settled) = settled {
            self.marks.retain(|(value, _)| *value > settled);
        }
        if !in_progress.is_empty() {
            if self.marks.len() == MAX_MARKS {
                self.marks.pop_front();
            }
            self.marks.push_back((last, in_progress.clone()));
        }
        settled
    }
}

/// `GET /api/v1/events`.
///
/// Without a resume point the stream starts at the first settled horizon:
/// every ID at or below it has committed or never will, so the client's
/// reload sees it, and every later ID is delivered. (The newest committed ID
/// would not do: an earlier ID can still commit after it.) Every connection
/// first sends `ready`, whose ID is the resume point, so a client reloads what
/// it shows and then applies the events that follow.
pub(super) async fn events(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<EventsQuery>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let query = query.map_err(|_| ApiError::invalid())?.0;
    let resume = resume_point(&headers, query.after.as_deref())?;
    let resumed = headers.contains_key("last-event-id") || query.after.is_some();
    let Some(permit) = state.events.admit_live(caller.0.token_hash) else {
        return Err(ApiError::transient(
            "event_stream_capacity_exhausted",
            "retry_same_request",
        ));
    };
    let limits = state.events.limits();
    let now = Instant::now();
    let mut buffer = VecDeque::new();
    if resumed {
        buffer.push_back(marker("ready", resume));
    }
    let stream = LiveStream {
        wake: Some(state.events.subscribe()),
        state,
        caller: caller.0,
        cursor: resume,
        started: resumed,
        horizon: Horizon::default(),
        buffer,
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

/// A `ready` or `reset` event: no data, and the ID to resume after.
fn marker(name: &'static str, id: i64) -> Event {
    Event::default().id(id.to_string()).event(name).data("{}")
}

/// One ledger row a stream read.
#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    job_id: Uuid,
    table_id: Uuid,
    state: String,
    revision: i64,
    visible: bool,
}

/// The state of one open stream.
struct LiveStream {
    state: ApiState,
    caller: Session,
    /// The last ledger ID delivered, skipped as invisible, or resumed after.
    cursor: i64,
    /// Whether `ready` has been sent; a fresh stream sends it once a horizon
    /// settles.
    started: bool,
    horizon: Horizon,
    buffer: VecDeque<Event>,
    limits: EventStreamLimits,
    wake: Option<broadcast::Receiver<Wake>>,
    deadline: Instant,
    next_recheck: Instant,
    _permit: StreamPermit,
}

impl LiveStream {
    /// The next event to send, or `None` to end the stream. Like a job
    /// stream, the whole operation races the stream's deadline.
    async fn next(&mut self) -> Option<Event> {
        let deadline = self.deadline;
        tokio::time::timeout_at(deadline, self.next_before_deadline())
            .await
            .ok()
            .flatten()
    }

    async fn next_before_deadline(&mut self) -> Option<Event> {
        loop {
            if Instant::now() >= self.deadline {
                return None;
            }
            if Instant::now() >= self.next_recheck {
                self.recheck().await?;
                self.next_recheck = Instant::now() + self.limits.recheck_interval;
            }
            if let Some(event) = self.buffer.pop_front() {
                return Some(event);
            }
            let settled = self.settle().await?;
            if !self.started {
                if let Some(settled) = settled {
                    self.cursor = settled;
                    self.started = true;
                    self.buffer.push_back(marker("ready", settled));
                    continue;
                }
            } else if let Some(settled) = settled.filter(|settled| *settled > self.cursor) {
                if settled - self.cursor > MAX_REPLAY {
                    self.cursor = settled;
                    self.buffer.push_back(marker("reset", settled));
                    continue;
                }
                if self.read(settled).await? {
                    continue;
                }
            }
            self.wait().await;
        }
    }

    /// Ends the stream once the session is no longer valid. The session is
    /// checked without extending it; grants apply to every read.
    async fn recheck(&self) -> Option<()> {
        let checked = async {
            let mut conn = self.state.database.pool().acquire().await?;
            auth::verify_session(
                &mut conn,
                self.caller.token_hash,
                self.state.sessions.idle_timeout,
            )
            .await
        };
        match checked.await {
            Ok(()) => Some(()),
            Err(error) => {
                if !matches!(error, PersistenceError::SessionRejected { .. }) {
                    tracing::warn!(error = %error, "live event stream recheck failed");
                }
                None
            }
        }
    }

    /// The newest settled ledger ID. The sequence is read before the
    /// transactions in progress, in separate statements, so the snapshot
    /// covers every transaction that could hold an ID up to that value.
    async fn settle(&mut self) -> Option<Option<i64>> {
        let read = async {
            let mut conn = self.state.database.pool().acquire().await?;
            let last: i64 = sqlx::query_scalar(
                "SELECT COALESCE(pg_sequence_last_value(\
                 pg_get_serial_sequence('job_events', 'id')::regclass), 0)",
            )
            .fetch_one(&mut *conn)
            .await?;
            let in_progress: Vec<String> = sqlx::query_scalar(
                "SELECT x::text FROM pg_snapshot_xip(pg_current_snapshot()) AS x",
            )
            .fetch_all(&mut *conn)
            .await?;
            Ok::<_, sqlx::Error>((last, in_progress))
        };
        match read.await {
            Ok((last, in_progress)) => {
                let Some(in_progress) = in_progress
                    .iter()
                    .map(|xid| xid.parse().ok())
                    .collect::<Option<BTreeSet<u64>>>()
                else {
                    tracing::error!("a transaction ID could not be parsed");
                    return None;
                };
                Some(self.horizon.advance(last, &in_progress))
            }
            Err(error) => {
                // The client reconnects with its last event ID.
                tracing::warn!(error = %error, "live event stream could not settle");
                None
            }
        }
    }

    /// Buffers the visible events after the cursor up to `settled`, one page
    /// at a time. Returns whether the cursor moved.
    async fn read(&mut self, settled: i64) -> Option<bool> {
        let rows = match self.page(settled).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(reason = error.reason, "live event stream read failed");
                return None;
            }
        };
        let full = i64::try_from(rows.len()).is_ok_and(|len| len >= self.limits.replay_page);
        let before = self.cursor;
        for row in &rows {
            self.cursor = row.id;
            if row.visible {
                self.buffer.push_back(job_event(row)?);
            }
        }
        // A short page read everything up to the horizon, including the
        // IDs of rolled-back transactions that never appear.
        if !full {
            self.cursor = settled;
        }
        Some(self.cursor != before)
    }

    /// The ledger rows after the cursor up to `settled`, each marked with
    /// whether the caller may list its job. Other organizations' rows are
    /// never visible.
    async fn page(&self, settled: i64) -> Result<Vec<Row>, ApiError> {
        let mut sql = scoped(
            "WITH page AS (SELECT e.id, e.job_id, e.to_state, e.state_revision \
             FROM job_events e WHERE e.id > ",
            &self.caller,
            Permission::ViewJobs,
        )?;
        sql.push_bind(self.cursor)
            .push(" AND e.id <= ")
            .push_bind(settled)
            .push(" ORDER BY e.id LIMIT ")
            .push_bind(self.limits.replay_page)
            .push(concat!(
                ") SELECT p.id, p.job_id, r.table_identity_id AS table_id, \
                   p.to_state AS state, p.state_revision AS revision, \
                   (r.organization_id = $1 AND ",
                table_access!(),
                ") AS visible \
                 FROM page p JOIN jobs r ON r.id = p.job_id \
                 JOIN table_identities t \
                   ON t.id = r.table_identity_id AND t.organization_id = r.organization_id \
                 ORDER BY p.id"
            ));
        Ok(sql
            .build_query_as()
            .fetch_all(self.state.database.pool())
            .await?)
    }

    /// Waits for any job event, the poll interval, the next recheck, or the
    /// deadline, whichever comes first.
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
        tokio::select! {
            () = &mut sleep => {}
            received = wake.recv() => {
                if matches!(received, Err(broadcast::error::RecvError::Closed)) {
                    self.wake = None;
                    sleep.await;
                }
            }
        }
    }
}

/// Encodes one visible ledger row: identifiers, state, and revision only.
fn job_event(row: &Row) -> Option<Event> {
    Event::default()
        .id(row.id.to_string())
        .event("job")
        .json_data(json!({
            "job_id": row.job_id,
            "table_id": row.table_id,
            "state": row.state,
            "revision": row.revision,
        }))
        .inspect_err(|error| tracing::error!(error = %error, "live event could not be encoded"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(xids: &[u64]) -> BTreeSet<u64> {
        xids.iter().copied().collect()
    }

    #[test]
    fn an_idle_database_settles_at_once() {
        let mut horizon = Horizon::default();
        assert_eq!(horizon.advance(7, &set(&[])), Some(7));
        assert!(horizon.marks.is_empty());
    }

    #[test]
    fn a_value_settles_only_after_its_transactions_end() {
        let mut horizon = Horizon::default();
        // Transactions 10 and 11 may hold IDs up to 5.
        assert_eq!(horizon.advance(5, &set(&[10, 11])), None);
        // 10 ended; 11 is still running, and 12 started.
        assert_eq!(horizon.advance(8, &set(&[11, 12])), None);
        // 11 ended: IDs up to 5 settled; 12 still covers 8.
        assert_eq!(horizon.advance(9, &set(&[12])), Some(5));
        assert_eq!(horizon.advance(9, &set(&[13])), Some(9));
        assert_eq!(horizon.marks.len(), 1, "only the open mark remains");
    }

    #[test]
    fn marks_are_bounded() {
        let mut horizon = Horizon::default();
        for value in 0..200 {
            assert_eq!(horizon.advance(value, &set(&[1])), None);
        }
        assert_eq!(horizon.marks.len(), MAX_MARKS);
        assert_eq!(horizon.advance(500, &set(&[2])), Some(199));
    }
}
