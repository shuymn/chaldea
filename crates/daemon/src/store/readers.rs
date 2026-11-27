//! The read-only connection pool and `ReadTx`, the store's read operations
//! (`docs/design/event-store.md` §2.2 "Readers", §2.7).

use std::cell::RefCell;
use std::str::FromStr;
use std::sync::{Condvar, Mutex};
use std::time::Instant;

use chaldea_core::command::{CommandReceipt, ReceiptStatus};
use chaldea_core::event::{EventEnvelope, TypedEvent};
use chaldea_core::ids::{
    Actor, ApprovalId, CommandId, IdempotencyKey, InputId, LaunchId, Open, StreamId, SurfaceId,
    ThreadId, Timestamp, TokenId, WorkspaceId, WorktreeId,
};
use chaldea_core::input::{InputState, ReadinessMark};
use chaldea_core::payload::{ApprovalRequested, SurfaceUpdate, VtSnapshotTaken, WorktreeChanged};
use chaldea_core::state::ThreadState;
use chaldea_core::sync::{
    GateEvidence, HostSnapshot, InputInfo, LaunchInfo, SNAPSHOT_RECENT_EVENTS, SessionInfo,
    StreamHead, StreamHeadEntry, ThreadInfo, ThreadSnapshot, ThreadSummary,
};
use rusqlite::{Connection, OptionalExtension, Params, Row};
use uuid::Uuid;

use super::annex::Continuity;
use super::append::{EVENT_COLUMNS, RawRow};
use super::error::{ReadError, StoreError};
use super::lock;
use super::rows::{
    ApprovalRow, BlobBody, BlobDownload, BufferedDelivery, BufferedHook, BufferedSummary,
    HookDelivery, HookReply, InputRow, LargeBlob, LaunchRow, Routing, SchemaInfo, SurfaceRow,
    ThreadRow, TokenKind, TokenRow, TranscriptCursor, WorktreeRow, command_result, enum_text,
    opt_u64, opt_uuid, parse_enum, parse_json, parse_uuid, to_i64, to_u64,
};

/// [`ReadTx::binding_receipt_ordinal`]'s query: the binding is searched within the thread's
/// stream through `events_kind` (newest first), then its delivery by the launch's key.
pub(super) const BINDING_ORDINAL_SQL: &str = "SELECT d.receipt_ordinal FROM events e
     LEFT JOIN hook_deliveries d ON d.launch_id = e.launch_id AND d.stream_id = e.stream_id
       AND d.seq = e.seq AND d.routing = 'appended'
     WHERE e.stream_id = ?1 AND e.kind = 'session_bound' AND e.launch_id = ?2
     ORDER BY e.seq DESC LIMIT 1";

/// One page of a stream, as `core::sync::EventsResponse::Replay` carries it.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub events: Vec<EventEnvelope>,
    /// An event beyond the page exists (after it for `events_after`, at or after
    /// `before_seq` for `events_before`).
    pub has_more: bool,
    /// The page does not start at the stream's first event.
    pub has_more_before: bool,
}

// ---- pool -------------------------------------------------------------------------------

struct PoolState {
    idle: Vec<Connection>,
    /// Connections not yet closed: idle, lent, or being closed by a drain.
    open: usize,
    closed: bool,
    drained: bool,
}

pub(crate) struct Pool {
    faults: std::sync::Arc<super::writer::Faults>,
    state: Mutex<PoolState>,
    cv: Condvar,
    /// Waits begun, counted under the lock just before it is released (tests).
    #[cfg(test)]
    waits: std::sync::atomic::AtomicU32,
}

impl Pool {
    pub(crate) fn new(
        conns: Vec<Connection>,
        faults: std::sync::Arc<super::writer::Faults>,
    ) -> Self {
        Pool {
            faults,
            state: Mutex::new(PoolState {
                open: conns.len(),
                idle: conns,
                closed: false,
                drained: false,
            }),
            cv: Condvar::new(),
            #[cfg(test)]
            waits: std::sync::atomic::AtomicU32::new(0),
        }
    }

    fn borrow(&self, wait_until: Instant) -> Result<Connection, ReadError> {
        let mut s = lock(&self.state);
        let mut waited = false;
        loop {
            if s.closed {
                return Err(ReadError::Closed);
            }
            // After a wait, a reader freed once `wait_until` passed is not lent: the deadline
            // is checked under the lock, just before the lend.
            if waited && Instant::now() >= wait_until {
                return Err(ReadError::Busy);
            }
            if let Some(c) = s.idle.pop() {
                return Ok(c);
            }
            let now = Instant::now();
            if now >= wait_until {
                return Err(ReadError::Busy);
            }
            #[cfg(test)]
            self.waits
                .fetch_add(1, std::sync::atomic::Ordering::Release);
            s = self
                .cv
                .wait_timeout(s, wait_until - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
            waited = true;
        }
    }

    fn give_back(&self, conn: Connection) {
        if !conn.is_autocommit() {
            let _ = conn.execute_batch("ROLLBACK");
        }
        let mut s = lock(&self.state);
        if s.drained {
            drop(s);
            drop(conn);
            lock(&self.state).open -= 1;
        } else {
            s.idle.push(conn);
            drop(s);
        }
        self.cv.notify_all();
    }

    pub(crate) fn read<R>(
        &self,
        wait_until: Instant,
        f: impl FnOnce(&ReadTx<'_>) -> Result<R, StoreError>,
    ) -> Result<R, ReadError> {
        struct Lent<'p> {
            pool: &'p Pool,
            conn: Option<Connection>,
        }
        impl Drop for Lent<'_> {
            fn drop(&mut self) {
                if let Some(c) = self.conn.take() {
                    self.pool.give_back(c);
                }
            }
        }
        let lent = Lent {
            pool: self,
            conn: Some(self.borrow(wait_until)?),
        };
        let conn = lent.conn.as_ref().ok_or(ReadError::Closed)?;
        conn.execute_batch("BEGIN DEFERRED")
            .map_err(StoreError::from)?;
        let tx = ReadTx::new(conn, None);
        let result = f(&tx);
        let end = conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" });
        let value = result?;
        end.map_err(StoreError::from)?;
        Ok(value)
    }

    /// Stops lending; waiting borrowers get `Closed`.
    pub(crate) fn close(&self) {
        lock(&self.state).closed = true;
        self.cv.notify_all();
    }

    /// Waits until every connection is closed (lent ones come back and are closed), or the
    /// deadline. Only meaningful after `drain`.
    pub(crate) fn wait_closed(&self, deadline: Instant) -> bool {
        let mut s = lock(&self.state);
        loop {
            if s.open == 0 {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            s = self
                .cv
                .wait_timeout(s, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// Connections not yet closed.
    pub(crate) fn open(&self) -> usize {
        lock(&self.state).open
    }

    /// Closes every idle connection; one still lent is closed when it comes back. `open`
    /// counts a connection until it is closed, so a concurrent caller waits for this one.
    pub(crate) fn drain(&self) {
        let mut s = lock(&self.state);
        s.drained = true;
        let idle = std::mem::take(&mut s.idle);
        drop(s);
        let n = idle.len();
        self.faults.mark("drain_taken");
        drop(idle);
        lock(&self.state).open -= n;
        self.cv.notify_all();
    }
}

// ---- ReadTx -----------------------------------------------------------------------------

/// The read operations, all inside one read transaction (or the job's write transaction
/// through `WriteTx::read`).
pub struct ReadTx<'a> {
    conn: &'a Connection,
    /// The enclosing `WriteTx`'s poison slot: a read error inside a job poisons it too.
    poison: Option<&'a RefCell<Option<StoreError>>>,
}

/// Every input state; the exhaustive match below stops compiling when a state is added.
const ALL_INPUT_STATES: [InputState; 8] = [
    InputState::Queued,
    InputState::Sending,
    InputState::Sent,
    InputState::Acked,
    InputState::Unconfirmed,
    InputState::Rejected,
    InputState::Cancelled,
    InputState::Dismissed,
];

const _: fn(InputState) = |s| match s {
    InputState::Queued
    | InputState::Sending
    | InputState::Sent
    | InputState::Acked
    | InputState::Unconfirmed
    | InputState::Rejected
    | InputState::Cancelled
    | InputState::Dismissed => {}
};

/// Clamps a cursor to SQLite's integer range.
fn clamp(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn sql<T>(r: rusqlite::Result<T>) -> Result<T, StoreError> {
    r.map_err(StoreError::from)
}

impl<'a> ReadTx<'a> {
    pub(crate) fn new(
        conn: &'a Connection,
        poison: Option<&'a RefCell<Option<StoreError>>>,
    ) -> Self {
        ReadTx { conn, poison }
    }

    /// Runs one read, honouring and setting the enclosing job's poison.
    fn guard<T>(&self, f: impl FnOnce() -> Result<T, StoreError>) -> Result<T, StoreError> {
        if let Some(p) = self.poison
            && let Some(e) = p.borrow().as_ref()
        {
            return Err(e.clone());
        }
        let r = f();
        if let (Err(e), Some(p)) = (&r, self.poison) {
            let mut slot = p.borrow_mut();
            if slot.is_none() {
                *slot = Some(e.clone());
            }
        }
        r
    }

    fn rows<T>(
        &self,
        sql_text: &str,
        params: impl Params,
        map: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>, StoreError> {
        let mut stmt = sql(self.conn.prepare(sql_text))?;
        let it = sql(stmt.query_map(params, map))?;
        sql(it.collect())
    }

    fn events(
        &self,
        sql_text: &str,
        params: impl Params,
    ) -> Result<Vec<EventEnvelope>, StoreError> {
        self.rows(sql_text, params, RawRow::read)?
            .into_iter()
            .map(|r| r.decode().map(|s| s.envelope))
            .collect()
    }

    /// `host` always exists; a thread stream exists once the thread is registered.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn stream_exists(&self, s: StreamId) -> Result<bool, StoreError> {
        self.guard(|| match s {
            StreamId::Host => Ok(true),
            StreamId::Thread(t) => sql(self.conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM events WHERE stream_id = 'host' AND kind = 'thread_registered'
                   AND thread_id = ?1)",
                [t.to_string()],
                |r| r.get(0),
            )),
        })
    }

    /// The stream's head and the running byte counts `decide_replay` takes.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn head(&self, s: StreamId, after_seq: u64) -> Result<StreamHead, StoreError> {
        self.guard(|| {
            let (head, cum): (i64, i64) = sql(self.conn.query_row(
                "SELECT coalesce(max(seq), 0), coalesce((SELECT cum_bytes FROM events WHERE stream_id = ?1
                   ORDER BY seq DESC LIMIT 1), 0) FROM events WHERE stream_id = ?1",
                [s.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            ))?;
            let at_after: i64 = sql(self.conn.query_row(
                "SELECT coalesce((SELECT cum_bytes FROM events WHERE stream_id = ?1 AND seq = ?2), 0)",
                rusqlite::params![s.to_string(), clamp(after_seq)],
                |r| r.get(0),
            ))?;
            Ok(StreamHead {
                head_seq: to_u64(head)?,
                cum_bytes_at_head: to_u64(cum)?,
                cum_bytes_at_after: to_u64(at_after)?,
            })
        })
    }

    /// The head of each stream (0 for an empty one).
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn heads(&self, streams: &[StreamId]) -> Result<Vec<StreamHeadEntry>, StoreError> {
        self.guard(|| {
            streams
                .iter()
                .map(|s| {
                    Ok(StreamHeadEntry {
                        stream_id: *s,
                        head_seq: self.stream_head(*s)?,
                    })
                })
                .collect()
        })
    }

    /// Events with `seq > after_seq`, ascending, at most `limit`.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn events_after(
        &self,
        s: StreamId,
        after_seq: u64,
        limit: u32,
    ) -> Result<Page, StoreError> {
        self.guard(|| {
            let after = clamp(after_seq);
            let mut events = self.events(
                &format!(
                    "SELECT {EVENT_COLUMNS} FROM events WHERE stream_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3"
                ),
                rusqlite::params![s.to_string(), after, i64::from(limit) + 1],
            )?;
            let has_more = events.len() > limit as usize;
            events.truncate(limit as usize);
            let has_more_before = after >= 1
                && sql(self.conn.query_row(
                    "SELECT EXISTS (SELECT 1 FROM events WHERE stream_id = ?1 AND seq <= ?2)",
                    rusqlite::params![s.to_string(), after],
                    |r| r.get(0),
                ))?;
            Ok(Page {
                events,
                has_more,
                has_more_before,
            })
        })
    }

    /// The nearest `limit` events with `seq < before_seq`, ascending.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn events_before(
        &self,
        s: StreamId,
        before_seq: u64,
        limit: u32,
    ) -> Result<Page, StoreError> {
        self.guard(|| {
            let before = clamp(before_seq);
            let mut events = self.events(
                &format!(
                    "SELECT {EVENT_COLUMNS} FROM events WHERE stream_id = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3"
                ),
                rusqlite::params![s.to_string(), before, i64::from(limit) + 1],
            )?;
            let has_more_before = events.len() > limit as usize;
            events.truncate(limit as usize);
            events.reverse();
            let has_more = sql(self.conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM events WHERE stream_id = ?1 AND seq >= ?2)",
                rusqlite::params![s.to_string(), before],
                |r| r.get(0),
            ))?;
            Ok(Page {
                events,
                has_more,
                has_more_before,
            })
        })
    }

    /// The event stored under `key` in the stream.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn event_by_key(
        &self,
        s: StreamId,
        key: &IdempotencyKey,
    ) -> Result<Option<EventEnvelope>, StoreError> {
        self.guard(|| {
            Ok(self
                .events(
                    &format!("SELECT {EVENT_COLUMNS} FROM events WHERE stream_id = ?1 AND idempotency_key = ?2"),
                    [s.to_string(), key.0.clone()],
                )?
                .pop())
        })
    }

    /// Events of one kind with `seq > after_seq`, ascending, at most `limit`.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn events_of_kind(
        &self,
        s: StreamId,
        kind: &str,
        after_seq: u64,
        limit: u32,
    ) -> Result<Page, StoreError> {
        self.guard(|| {
            let after = clamp(after_seq);
            let mut events = self.events(
                &format!(
                    "SELECT {EVENT_COLUMNS} FROM events WHERE stream_id = ?1 AND kind = ?2 AND seq > ?3
                     ORDER BY seq LIMIT ?4"
                ),
                rusqlite::params![s.to_string(), kind, after, i64::from(limit) + 1],
            )?;
            let has_more = events.len() > limit as usize;
            events.truncate(limit as usize);
            let has_more_before = sql(self.conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM events WHERE stream_id = ?1 AND kind = ?2 AND seq <= ?3)",
                rusqlite::params![s.to_string(), kind, after],
                |r| r.get(0),
            ))?;
            Ok(Page {
                events,
                has_more,
                has_more_before,
            })
        })
    }

    /// The latest event of one kind.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn latest_of_kind(
        &self,
        s: StreamId,
        kind: &str,
    ) -> Result<Option<EventEnvelope>, StoreError> {
        self.guard(|| self.latest_of_kind_raw(s, kind))
    }

    fn stream_head(&self, s: StreamId) -> Result<u64, StoreError> {
        let h: i64 = sql(self.conn.query_row(
            "SELECT coalesce(max(seq), 0) FROM events WHERE stream_id = ?1",
            [s.to_string()],
            |r| r.get(0),
        ))?;
        to_u64(h)
    }

    /// The thread's snapshot and its `as_of_seq`, from one snapshot of the database.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn thread_snapshot(
        &self,
        t: ThreadId,
    ) -> Result<Option<(ThreadSnapshot, u64)>, StoreError> {
        self.guard(|| {
            let Some(thread) = self.thread_row(t)? else {
                return Ok(None);
            };
            let stream = StreamId::Thread(t);
            let launch = match thread.current_launch_id {
                Some(l) => self.launch_row(l)?,
                None => None,
            };
            let session = thread.session_id.clone().map(|session_id| SessionInfo {
                session_id,
                transcript_path: thread.transcript_path.clone(),
            });
            let gate = launch.as_ref().map_or(
                GateEvidence {
                    readiness_seq: None,
                    readiness_evidence: None,
                    last_write_seq: None,
                    last_human_write_seq: None,
                    last_turn_start_seq: None,
                },
                |l| GateEvidence {
                    readiness_seq: l.readiness.map(|r| r.seq),
                    readiness_evidence: l.readiness.map(|r| r.evidence),
                    last_write_seq: l.last_write_seq,
                    last_human_write_seq: l.last_human_write_seq,
                    last_turn_start_seq: l.last_turn_start_seq,
                },
            );
            let active: Vec<InputState> = ALL_INPUT_STATES
                .into_iter()
                .filter(|s| !s.is_terminal())
                .collect();
            let inputs = self
                .inputs_in(t, &active)?
                .into_iter()
                .map(|i| InputInfo {
                    input_id: i.input_id,
                    launch_id: i.launch_id,
                    state: i.state,
                    text: i.text,
                    encoding: i.encoding,
                    created_by: i.created_by,
                    sent_after_seq: i.sent_after_seq,
                })
                .collect::<Vec<_>>();
            let open_approvals: Vec<ApprovalRequested> = self
                .approval_rows("WHERE thread_id = ?1 AND outcome IS NULL ORDER BY opened_seq", [t.to_string()])?
                .into_iter()
                .map(|a| a.requested)
                .collect();
            let worktree = match launch.as_ref().and_then(|l| l.worktree_id.clone()) {
                Some(w) => self.worktree_row(&w)?.map(|w| w.record),
                None => None,
            };
            let surfaces = self
                .surface_rows("WHERE stream_id = ?1 ORDER BY surface_id", [stream.to_string()])?
                .into_iter()
                .map(|s| s.update)
                .collect();
            let vt_snapshot = if thread.state.attention.is_empty() && open_approvals.is_empty() {
                None
            } else {
                match self.latest_of_kind_raw(stream, "vt_snapshot_taken")? {
                    Some(e) => match TypedEvent::decode(&e) {
                        Ok(TypedEvent::VtSnapshotTaken(v)) => Some(v),
                        _ => None::<VtSnapshotTaken>,
                    },
                    None => None,
                }
            };
            let mut recent_events = self.events(
                &format!("SELECT {EVENT_COLUMNS} FROM events WHERE stream_id = ?1 ORDER BY seq DESC LIMIT ?2"),
                rusqlite::params![stream.to_string(), super::rows::sql_len(SNAPSHOT_RECENT_EVENTS)],
            )?;
            recent_events.reverse();
            let snapshot = ThreadSnapshot {
                thread: thread_info(&thread),
                launch: launch.as_ref().map(launch_info),
                session,
                gate,
                inputs,
                open_approvals,
                worktree,
                surfaces,
                vt_snapshot,
                recent_events,
            };
            Ok(Some((snapshot, self.stream_head(stream)?)))
        })
    }

    /// The fixed-size facts of one thread (`GET /api/threads/{thread_id}`): its states, the
    /// number of its unresolved attention causes, counted in SQL without decoding them, and
    /// its current launch. `None` for a missing thread.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn thread_summary(&self, t: ThreadId) -> Result<Option<ThreadSummary>, StoreError> {
        self.guard(|| {
            type Raw = (bool, String, String, String, i64, Option<String>);
            let Some(r) = self
                .rows(
                    "SELECT closed, process_state, turn_state, task_state, json_array_length(attention),
                       current_launch_id FROM proj_threads WHERE thread_id = ?1",
                    [t.to_string()],
                    |r| -> rusqlite::Result<Raw> {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
                    },
                )?
                .pop()
            else {
                return Ok(None);
            };
            let launch = match opt_uuid(r.5.as_deref())?.map(LaunchId) {
                Some(l) => self.launch_row(l)?,
                None => None,
            };
            Ok(Some(ThreadSummary {
                thread_id: t,
                closed: r.0,
                process_state: parse_enum(&r.1)?,
                turn_state: parse_enum(&r.2)?,
                task_state: parse_enum(&r.3)?,
                attention_count: u32::try_from(r.4).map_err(StoreError::decode)?,
                launch: launch.as_ref().map(launch_info),
            }))
        })
    }

    fn latest_of_kind_raw(
        &self,
        s: StreamId,
        kind: &str,
    ) -> Result<Option<EventEnvelope>, StoreError> {
        Ok(self
            .events(
                &format!("SELECT {EVENT_COLUMNS} FROM events WHERE stream_id = ?1 AND kind = ?2 ORDER BY seq DESC LIMIT 1"),
                rusqlite::params![s.to_string(), kind],
            )?
            .pop())
    }

    /// The host snapshot and the host stream's head.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn host_snapshot(&self) -> Result<(HostSnapshot, u64), StoreError> {
        self.guard(|| {
            let threads = self.thread_infos()?;
            let worktrees = self
                .worktree_rows("ORDER BY worktree_id", [])?
                .into_iter()
                .map(|w| w.record)
                .collect();
            let surfaces = self
                .surface_rows("WHERE stream_id = 'host' ORDER BY surface_id", [])?
                .into_iter()
                .map(|s| s.update)
                .collect();
            Ok((
                HostSnapshot {
                    threads,
                    worktrees,
                    surfaces,
                },
                self.stream_head(StreamId::Host)?,
            ))
        })
    }

    fn thread_rows(&self, tail: &str, params: impl Params) -> Result<Vec<ThreadRow>, StoreError> {
        type Raw = (
            String,
            String,
            String,
            String,
            Option<String>,
            String,
            bool,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            i64,
        );
        let raws: Vec<Raw> = self.rows(
            &format!(
                "SELECT thread_id, agent, cwd, workspace_id, parent_thread_id, role, closed, process_state,
                   turn_state, task_state, attention, current_launch_id, session_id, transcript_path,
                   registered_seq FROM proj_threads {tail}"
            ),
            params,
            |r| {
                Ok((
                    r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?,
                    r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?, r.get(12)?, r.get(13)?, r.get(14)?,
                ))
            },
        )?;
        raws.into_iter()
            .map(|r| {
                Ok(ThreadRow {
                    thread_id: ThreadId(parse_uuid(&r.0)?),
                    agent: parse_enum(&r.1)?,
                    cwd: r.2,
                    workspace_id: WorkspaceId(r.3),
                    parent_thread_id: opt_uuid(r.4.as_deref())?.map(ThreadId),
                    role: parse_enum(&r.5)?,
                    closed: r.6,
                    state: ThreadState {
                        process: parse_enum(&r.7)?,
                        turn: parse_enum(&r.8)?,
                        task: parse_enum(&r.9)?,
                        attention: parse_json::<Vec<_>>(&r.10)?.into_iter().collect(),
                    },
                    current_launch_id: opt_uuid(r.11.as_deref())?.map(LaunchId),
                    session_id: r.12,
                    transcript_path: r.13,
                    registered_seq: to_u64(r.14)?,
                })
            })
            .collect()
    }

    fn thread_row(&self, t: ThreadId) -> Result<Option<ThreadRow>, StoreError> {
        Ok(self
            .thread_rows("WHERE thread_id = ?1", [t.to_string()])?
            .pop())
    }

    /// # Errors
    ///
    /// A store error.
    pub fn thread(&self, t: ThreadId) -> Result<Option<ThreadRow>, StoreError> {
        self.guard(|| self.thread_row(t))
    }

    /// Every thread, in registration order.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn threads(&self) -> Result<Vec<ThreadRow>, StoreError> {
        self.guard(|| self.thread_rows("ORDER BY registered_seq", []))
    }

    /// Every thread as the thread list and the host snapshot carry it, in registration order.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn thread_infos(&self) -> Result<Vec<ThreadInfo>, StoreError> {
        self.guard(|| {
            Ok(self
                .thread_rows("ORDER BY registered_seq", [])?
                .iter()
                .map(thread_info)
                .collect())
        })
    }

    fn launch_rows(&self, tail: &str, params: impl Params) -> Result<Vec<LaunchRow>, StoreError> {
        let raws = self.rows(
            &format!(
                "SELECT launch_id, thread_id, reserved_seq, expected_previous, agent, argv_digest, manual,
                   worktree_id, pid, start_time, pgid_or_job, cli_version, exit_code, exit_signal, exit_reason,
                   death_confirmed, death_method, session_id, readiness_seq, readiness_evidence, write_basis,
                   last_write_seq, last_human_write_seq, last_turn_start_seq
                 FROM proj_launches {tail}"
            ),
            params,
            |r| {
                let v: Vec<rusqlite::types::Value> = (0..24).map(|i| r.get(i)).collect::<Result<_, _>>()?;
                Ok(v)
            },
        )?;
        raws.iter().map(|v| decode_launch(v)).collect()
    }

    fn launch_row(&self, l: LaunchId) -> Result<Option<LaunchRow>, StoreError> {
        #[cfg(test)]
        launch_reads::seen(l);
        Ok(self
            .launch_rows("WHERE launch_id = ?1", [l.to_string()])?
            .pop())
    }

    /// # Errors
    ///
    /// A store error.
    pub fn launch(&self, l: LaunchId) -> Result<Option<LaunchRow>, StoreError> {
        self.guard(|| self.launch_row(l))
    }

    /// Launches that have not exited, or whose death is not confirmed.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn open_launches(&self) -> Result<Vec<LaunchRow>, StoreError> {
        self.guard(|| {
            self.launch_rows(
                "WHERE exit_reason IS NULL OR death_confirmed = 0 ORDER BY thread_id, reserved_seq",
                [],
            )
        })
    }

    /// Launches reserved with this worktree.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn launches_using_worktree(&self, w: &WorktreeId) -> Result<Vec<LaunchRow>, StoreError> {
        self.guard(|| {
            self.launch_rows(
                "WHERE worktree_id = ?1 ORDER BY thread_id, reserved_seq",
                [w.0.clone()],
            )
        })
    }

    /// The thread's inputs in `states`, selected in SQL so other rows are never read.
    /// Every input of any thread in one of `states`, by thread, then creation.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn inputs_in_states(&self, states: &[InputState]) -> Result<Vec<InputRow>, StoreError> {
        self.guard(|| {
            let wanted: Vec<String> = states.iter().map(enum_text).collect();
            let wanted = serde_json::to_string(&wanted).map_err(StoreError::decode)?;
            self.input_rows(
                "WHERE state IN (SELECT value FROM json_each(?1)) ORDER BY thread_id, created_seq",
                [wanted],
            )
        })
    }

    fn inputs_in(&self, t: ThreadId, states: &[InputState]) -> Result<Vec<InputRow>, StoreError> {
        let wanted: Vec<String> = states.iter().map(enum_text).collect();
        let wanted = serde_json::to_string(&wanted).map_err(StoreError::decode)?;
        self.input_rows(
            "WHERE thread_id = ?1 AND state IN (SELECT value FROM json_each(?2)) ORDER BY created_seq",
            [t.to_string(), wanted],
        )
    }

    fn input_rows(&self, tail: &str, params: impl Params) -> Result<Vec<InputRow>, StoreError> {
        type Raw = (
            String,
            String,
            Option<String>,
            String,
            String,
            String,
            String,
            String,
            Option<i64>,
            Option<i64>,
            i64,
        );
        let raws: Vec<Raw> = self.rows(
            &format!(
                "SELECT input_id, thread_id, launch_id, state, text, encoding, origin, created_by,
                   sent_after_seq, ack_seq, created_seq FROM proj_inputs {tail}"
            ),
            params,
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                    r.get(10)?,
                ))
            },
        )?;
        raws.into_iter()
            .map(|r| {
                Ok(InputRow {
                    input_id: InputId(parse_uuid(&r.0)?),
                    thread_id: ThreadId(parse_uuid(&r.1)?),
                    launch_id: opt_uuid(r.2.as_deref())?.map(LaunchId),
                    state: parse_enum(&r.3)?,
                    text: parse_json(&r.4)?,
                    encoding: parse_enum(&r.5)?,
                    origin: parse_enum(&r.6)?,
                    created_by: Actor::from_str(&r.7).map_err(StoreError::decode)?,
                    sent_after_seq: opt_u64(r.8)?,
                    ack_seq: opt_u64(r.9)?,
                    created_seq: to_u64(r.10)?,
                })
            })
            .collect()
    }

    /// # Errors
    ///
    /// A store error.
    pub fn input(&self, i: InputId) -> Result<Option<InputRow>, StoreError> {
        self.guard(|| {
            Ok(self
                .input_rows("WHERE input_id = ?1", [i.to_string()])?
                .pop())
        })
    }

    /// The thread's inputs in any of `states`, in creation order.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn inputs(&self, t: ThreadId, states: &[InputState]) -> Result<Vec<InputRow>, StoreError> {
        self.guard(|| self.inputs_in(t, states))
    }

    fn approval_rows(
        &self,
        tail: &str,
        params: impl Params,
    ) -> Result<Vec<ApprovalRow>, StoreError> {
        type Raw = (
            String,
            String,
            String,
            String,
            i64,
            Option<String>,
            Option<i64>,
        );
        let raws: Vec<Raw> = self.rows(
            &format!(
                "SELECT approval_id, thread_id, launch_id, requested, opened_seq, outcome, resolved_seq
                 FROM proj_approvals {tail}"
            ),
            params,
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
        )?;
        raws.into_iter()
            .map(|r| {
                Ok(ApprovalRow {
                    approval_id: ApprovalId(parse_uuid(&r.0)?),
                    thread_id: ThreadId(parse_uuid(&r.1)?),
                    launch_id: LaunchId(parse_uuid(&r.2)?),
                    requested: parse_json(&r.3)?,
                    opened_seq: to_u64(r.4)?,
                    outcome: r.5.as_deref().map(parse_enum).transpose()?,
                    resolved_seq: opt_u64(r.6)?,
                })
            })
            .collect()
    }

    /// # Errors
    ///
    /// A store error.
    pub fn approval(&self, a: ApprovalId) -> Result<Option<ApprovalRow>, StoreError> {
        self.guard(|| {
            Ok(self
                .approval_rows("WHERE approval_id = ?1", [a.to_string()])?
                .pop())
        })
    }

    /// The thread's cards without an outcome, in opening order.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn open_approvals(&self, t: ThreadId) -> Result<Vec<ApprovalRow>, StoreError> {
        self.guard(|| {
            self.approval_rows(
                "WHERE thread_id = ?1 AND outcome IS NULL ORDER BY opened_seq",
                [t.to_string()],
            )
        })
    }

    fn surface_rows(&self, tail: &str, params: impl Params) -> Result<Vec<SurfaceRow>, StoreError> {
        let raws: Vec<(String, String, i64)> = self.rows(
            &format!("SELECT stream_id, payload, updated_seq FROM proj_surfaces {tail}"),
            params,
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        raws.into_iter()
            .map(|r| {
                Ok(SurfaceRow {
                    stream_id: StreamId::from_str(&r.0).map_err(StoreError::decode)?,
                    update: parse_json::<SurfaceUpdate>(&r.1)?,
                    updated_seq: to_u64(r.2)?,
                })
            })
            .collect()
    }

    /// # Errors
    ///
    /// A store error.
    pub fn surface(
        &self,
        stream: StreamId,
        s: &SurfaceId,
    ) -> Result<Option<SurfaceRow>, StoreError> {
        self.guard(|| {
            Ok(self
                .surface_rows(
                    "WHERE stream_id = ?1 AND surface_id = ?2",
                    [stream.to_string(), s.0.clone()],
                )?
                .pop())
        })
    }

    fn worktree_rows(
        &self,
        tail: &str,
        params: impl Params,
    ) -> Result<Vec<WorktreeRow>, StoreError> {
        let raws: Vec<(String, i64)> = self.rows(
            &format!("SELECT record, updated_seq FROM proj_worktrees {tail}"),
            params,
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        raws.into_iter()
            .map(|r| {
                Ok(WorktreeRow {
                    record: parse_json::<WorktreeChanged>(&r.0)?,
                    updated_seq: to_u64(r.1)?,
                })
            })
            .collect()
    }

    fn worktree_row(&self, w: &WorktreeId) -> Result<Option<WorktreeRow>, StoreError> {
        Ok(self
            .worktree_rows("WHERE worktree_id = ?1", [w.0.clone()])?
            .pop())
    }

    /// # Errors
    ///
    /// A store error.
    pub fn worktree(&self, w: &WorktreeId) -> Result<Option<WorktreeRow>, StoreError> {
        self.guard(|| self.worktree_row(w))
    }

    /// # Errors
    ///
    /// A store error.
    pub fn worktrees(&self) -> Result<Vec<WorktreeRow>, StoreError> {
        self.guard(|| self.worktree_rows("ORDER BY worktree_id", []))
    }

    pub(crate) fn cursor_row(&self, t: ThreadId) -> Result<Option<TranscriptCursor>, StoreError> {
        let raw: Option<(i64, String, String, i64, String, i64, String)> = sql(self
            .conn
            .query_row(
                "SELECT generation, path, file_id, event_offset, event_continuity, read_offset, read_continuity
                 FROM proj_transcript_cursors WHERE thread_id = ?1",
                [t.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
            )
            .optional())?;
        raw.map(|r| {
            Ok(TranscriptCursor {
                generation: to_u64(r.0)?,
                path: r.1,
                file_id: r.2,
                event_offset: to_u64(r.3)?,
                event_continuity: Continuity(r.4),
                read_offset: to_u64(r.5)?,
                read_continuity: Continuity(r.6),
            })
        })
        .transpose()
    }

    /// # Errors
    ///
    /// A store error.
    pub fn transcript_cursor(&self, t: ThreadId) -> Result<Option<TranscriptCursor>, StoreError> {
        self.guard(|| self.cursor_row(t))
    }

    /// Whether the stream has a `blob_refs` or `large_blob_refs` row for the blob (the
    /// download check of `GET /api/blobs`, decided before any blob body is loaded).
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn blob_ref_exists(&self, s: StreamId, sha256: &str) -> Result<bool, StoreError> {
        self.guard(|| {
            sql(self.conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM blob_refs WHERE stream_id = ?1 AND sha256 = ?2)
                     OR EXISTS (SELECT 1 FROM large_blob_refs WHERE stream_id = ?1 AND sha256 = ?2)",
                [s.to_string(), sha256.to_owned()],
                |r| r.get(0),
            ))
        })
    }

    /// The stream's download type from the refs in `table`: their type when they all
    /// agree, else `application/octet-stream`; `None` without a ref.
    fn download_type(
        &self,
        table: &str,
        s: StreamId,
        sha256: &str,
    ) -> Result<Option<String>, StoreError> {
        let types: Vec<String> = self.rows(
            &format!(
                "SELECT DISTINCT media_type FROM {table} WHERE stream_id = ?1 AND sha256 = ?2"
            ),
            [s.to_string(), sha256.to_owned()],
            |r| r.get(0),
        )?;
        Ok(match types.as_slice() {
            [] => None,
            [one] => Some(one.clone()),
            _ => Some("application/octet-stream".to_owned()),
        })
    }

    /// What a download of the blob serves, only when the stream has a ref to it: the bytes
    /// of a blob in `blobs`, or a large blob's size and part count. Both with the stream's
    /// agreed media type or `application/octet-stream`.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn blob_download(
        &self,
        s: StreamId,
        sha256: &str,
    ) -> Result<Option<BlobDownload>, StoreError> {
        self.guard(|| {
            if let Some(media_type) = self.download_type("blob_refs", s, sha256)? {
                let data: Option<Vec<u8>> = sql(self
                    .conn
                    .query_row("SELECT data FROM blobs WHERE sha256 = ?1", [sha256], |r| {
                        r.get(0)
                    })
                    .optional())?;
                return Ok(data.map(|data| BlobDownload::Whole(BlobBody { data, media_type })));
            }
            let Some(media_type) = self.download_type("large_blob_refs", s, sha256)? else {
                return Ok(None);
            };
            let row: Option<(i64, i64)> = sql(self
                .conn
                .query_row(
                    "SELECT size, part_count FROM large_blobs WHERE sha256 = ?1",
                    [sha256],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional())?;
            row.map(|(size, part_count)| {
                Ok(BlobDownload::Parts(LargeBlob {
                    media_type,
                    size: to_u64(size)?,
                    part_count: to_u64(part_count)?,
                }))
            })
            .transpose()
        })
    }

    /// One part of a large blob, by index (refs are permanent and parts immutable, so a
    /// download reads each part in its own short read).
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn large_blob_part(&self, sha256: &str, index: u64) -> Result<Option<Vec<u8>>, StoreError> {
        self.guard(|| self.part_row(sha256, index))
    }

    fn part_row(&self, sha256: &str, index: u64) -> Result<Option<Vec<u8>>, StoreError> {
        sql(self
            .conn
            .query_row(
                "SELECT data FROM large_blob_parts WHERE sha256 = ?1 AND part_index = ?2",
                rusqlite::params![sha256, to_i64(index)?],
                |r| r.get(0),
            )
            .optional())
    }

    /// The blob's whole bytes, only when the stream has a ref to it, with the stream's
    /// agreed media type or `application/octet-stream`. A large blob is read whole into
    /// memory; a download reads it part by part instead ([`ReadTx::blob_download`]).
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn blob(&self, s: StreamId, sha256: &str) -> Result<Option<BlobBody>, StoreError> {
        match self.blob_download(s, sha256)? {
            None => Ok(None),
            Some(BlobDownload::Whole(body)) => Ok(Some(body)),
            Some(BlobDownload::Parts(large)) => self.guard(|| {
                let mut data = Vec::new();
                for index in 0..large.part_count {
                    let part = self
                        .part_row(sha256, index)?
                        .ok_or_else(|| StoreError::decode("a large blob's part is missing"))?;
                    data.extend_from_slice(&part);
                }
                if data.len() as u64 != large.size {
                    return Err(StoreError::decode("a large blob's parts are incomplete"));
                }
                Ok(Some(BlobBody {
                    data,
                    media_type: large.media_type,
                }))
            }),
        }
    }

    /// # Errors
    ///
    /// A store error.
    pub fn receipt(&self, c: CommandId) -> Result<Option<CommandReceipt>, StoreError> {
        self.guard(|| self.receipt_row(c))
    }

    pub(crate) fn receipt_row(&self, c: CommandId) -> Result<Option<CommandReceipt>, StoreError> {
        type Raw = (String, Vec<u8>, String, Option<i64>, Option<String>);
        let raw: Option<Raw> = sql(self
            .conn
            .query_row(
                "SELECT token_id, request_sha256, status, http_status, body FROM command_receipts WHERE command_id = ?1",
                [c.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional())?;
        raw.map(|r| {
            let status = match r.2.as_str() {
                "in_progress" => ReceiptStatus::InProgress,
                "unknown_after_crash" => ReceiptStatus::UnknownAfterCrash,
                "completed" => ReceiptStatus::Completed(command_result(
                    r.3.ok_or_else(|| StoreError::decode("completed receipt without a status"))?,
                    r.4.ok_or_else(|| StoreError::decode("completed receipt without a body"))?,
                )?),
                other => return Err(StoreError::decode(format!("receipt status {other}"))),
            };
            Ok(CommandReceipt {
                command_id: c,
                token: TokenId(parse_uuid(&r.0)?),
                request_sha256: r
                    .1
                    .try_into()
                    .map_err(|_| StoreError::decode("request hash length"))?,
                status,
            })
        })
        .transpose()
    }

    /// # Errors
    ///
    /// A store error.
    pub fn token_by_hash(&self, sha256: &[u8; 32]) -> Result<Option<TokenRow>, StoreError> {
        self.guard(|| {
            type Raw = (String, String, Vec<u8>, Option<String>, Option<String>, String, Option<String>);
            let raw: Option<Raw> = sql(self
                .conn
                .query_row(
                    "SELECT token_id, kind, sha256, thread_id, launch_id, created_at, revoked_at FROM tokens WHERE sha256 = ?1",
                    [sha256.as_slice()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
                )
                .optional())?;
            raw.map(|r| {
                Ok(TokenRow {
                    token_id: TokenId(parse_uuid(&r.0)?),
                    kind: parse_enum::<TokenKind>(&r.1)?,
                    sha256: r.2.try_into().map_err(|_| StoreError::decode("token hash length"))?,
                    thread_id: opt_uuid(r.3.as_deref())?.map(ThreadId),
                    launch_id: opt_uuid(r.4.as_deref())?.map(LaunchId),
                    created_at: Timestamp(r.5),
                    revoked_at: r.6.map(Timestamp),
                })
            })
            .transpose()
        })
    }

    pub(crate) fn delivery_row(
        &self,
        l: LaunchId,
        emit: Uuid,
    ) -> Result<Option<HookDelivery>, StoreError> {
        type Raw = (i64, i64, i64, String, String, Option<String>, Option<i64>);
        let raw: Option<Raw> = sql(self
            .conn
            .query_row(
                "SELECT receipt_ordinal, commit_ordinal, reply_status, reply_body, routing, stream_id, seq
                 FROM hook_deliveries WHERE launch_id = ?1 AND emit_id = ?2",
                [l.to_string(), emit.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
            )
            .optional())?;
        raw.map(|r| {
            let position = || -> Result<(StreamId, u64), StoreError> {
                let s =
                    r.5.as_deref()
                        .ok_or_else(|| StoreError::decode("routing without a stream"))?;
                Ok((
                    StreamId::from_str(s).map_err(StoreError::decode)?,
                    to_u64(r.6.ok_or_else(|| StoreError::decode("routing without a seq"))?)?,
                ))
            };
            let routing = match r.4.as_str() {
                "buffered" => Routing::Buffered,
                "appended" => {
                    let (stream_id, seq) = position()?;
                    Routing::Appended { stream_id, seq }
                }
                "raw_unknown" => {
                    let (stream_id, seq) = position()?;
                    Routing::RawUnknown { stream_id, seq }
                }
                other => return Err(StoreError::decode(format!("routing {other}"))),
            };
            Ok(HookDelivery {
                receipt_ordinal: to_u64(r.0)?,
                commit_ordinal: to_u64(r.1)?,
                first_reply: HookReply {
                    http_status: u16::try_from(r.2).map_err(StoreError::decode)?,
                    body: r.3,
                },
                routing,
            })
        })
        .transpose()
    }

    /// # Errors
    ///
    /// A store error.
    pub fn hook_delivery(
        &self,
        l: LaunchId,
        emit: Uuid,
    ) -> Result<Option<HookDelivery>, StoreError> {
        self.guard(|| self.delivery_row(l, emit))
    }

    /// The launch's buffered deliveries in receipt order.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn buffered_deliveries(&self, l: LaunchId) -> Result<Vec<BufferedDelivery>, StoreError> {
        self.guard(|| {
            let raws: Vec<(String, i64, String, Vec<u8>, String)> = self.rows(
                "SELECT emit_id, receipt_ordinal, received_at, pending_body, pending_meta FROM hook_deliveries
                 WHERE launch_id = ?1 AND routing = 'buffered' ORDER BY receipt_ordinal",
                [l.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?;
            raws.into_iter()
                .map(|r| {
                    Ok(BufferedDelivery {
                        emit_id: parse_uuid(&r.0)?,
                        receipt_ordinal: to_u64(r.1)?,
                        received_at: Timestamp(r.2),
                        hook: BufferedHook { body: r.3, meta: r.4 },
                    })
                })
                .collect()
        })
    }

    /// The launch's buffered deliveries: their count, the summed length of their bodies, and
    /// the earliest `received_at` (no body is read).
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn buffered_summary(&self, l: LaunchId) -> Result<BufferedSummary, StoreError> {
        self.guard(|| {
            let (count, bytes, earliest): (i64, i64, Option<String>) = sql(self.conn.query_row(
                "SELECT count(*), coalesce(sum(length(pending_body)), 0), min(received_at)
                 FROM hook_deliveries WHERE launch_id = ?1 AND routing = 'buffered'",
                [l.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            ))?;
            Ok(BufferedSummary {
                count: to_u64(count)?,
                body_bytes: to_u64(bytes)?,
                earliest_received_at: earliest.map(Timestamp),
            })
        })
    }

    /// The seq of launch `l`'s latest input attempt (`input_state_changed` to `sending`),
    /// a zero-byte one included.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn last_attempt_seq(&self, l: LaunchId) -> Result<Option<u64>, StoreError> {
        self.guard(|| {
            let max: Option<i64> = sql(self.conn.query_row(
                "SELECT max(attempt_seq) FROM proj_inputs WHERE launch_id = ?1",
                [l.to_string()],
                |r| r.get(0),
            ))?;
            opt_u64(max)
        })
    }

    /// The largest `receipt_ordinal` of the launch's deliveries; 0 when it has none.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn max_receipt_ordinal(&self, l: LaunchId) -> Result<u64, StoreError> {
        self.guard(|| {
            let max: i64 = sql(self.conn.query_row(
                "SELECT coalesce(max(receipt_ordinal), 0) FROM hook_deliveries WHERE launch_id = ?1",
                [l.to_string()],
                |r| r.get(0),
            ))?;
            to_u64(max)
        })
    }

    /// The stream seq of the primary event of launch `l`'s delivery with receipt ordinal
    /// `ordinal`; `None` when there is none or it is still buffered.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn delivery_seq(&self, l: LaunchId, ordinal: u64) -> Result<Option<u64>, StoreError> {
        self.guard(|| {
            let seq: Option<Option<i64>> = sql(self
                .conn
                .query_row(
                    "SELECT seq FROM hook_deliveries WHERE launch_id = ?1 AND receipt_ordinal = ?2",
                    rusqlite::params![l.to_string(), to_i64(ordinal)?],
                    |r| r.get(0),
                )
                .optional())?;
            opt_u64(seq.flatten())
        })
    }

    /// The `receipt_ordinal` of the delivery whose primary event is launch `l`'s latest
    /// `session_bound` in thread `t`'s stream (by stream sequence); `None` when the launch is
    /// unbound or that event was not recorded by a hook delivery.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn binding_receipt_ordinal(
        &self,
        t: ThreadId,
        l: LaunchId,
    ) -> Result<Option<u64>, StoreError> {
        self.guard(|| {
            let ordinal: Option<Option<i64>> = sql(self
                .conn
                .query_row(
                    BINDING_ORDINAL_SQL,
                    [StreamId::Thread(t).to_string(), l.to_string()],
                    |r| r.get(0),
                )
                .optional())?;
            ordinal.flatten().map(to_u64).transpose()
        })
    }

    /// Whether a delivery of any launch reserved in thread `t`'s stream already took
    /// `emit` (buffered deliveries included).
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn hook_emit_in_thread(&self, t: ThreadId, emit: Uuid) -> Result<bool, StoreError> {
        self.guard(|| {
            sql(self.conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM events e JOIN hook_deliveries d
                   ON d.launch_id = e.launch_id AND d.emit_id = ?2
                   WHERE e.stream_id = ?1 AND e.kind = 'launch_reserved')",
                [StreamId::Thread(t).to_string(), emit.to_string()],
                |r| r.get(0),
            ))
        })
    }

    /// Launches with at least one buffered delivery.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn buffered_launches(&self) -> Result<Vec<LaunchId>, StoreError> {
        self.guard(|| {
            let ids: Vec<String> = self.rows(
                "SELECT DISTINCT launch_id FROM hook_deliveries WHERE routing = 'buffered' ORDER BY launch_id",
                [],
                |r| r.get(0),
            )?;
            ids.iter().map(|s| parse_uuid(s).map(LaunchId)).collect()
        })
    }

    /// # Errors
    ///
    /// A store error.
    pub fn schema_info(&self) -> Result<SchemaInfo, StoreError> {
        self.guard(|| {
            let application_id: i32 =
                sql(self
                    .conn
                    .pragma_query_value(None, "application_id", |r| r.get(0)))?;
            let user_version: i64 = sql(self
                .conn
                .pragma_query_value(None, "user_version", |r| r.get(0)))?;
            let projection_version: String = sql(self.conn.query_row(
                "SELECT value FROM meta WHERE key = 'projection_version'",
                [],
                |r| r.get(0),
            ))?;
            let sqlite_version: String =
                sql(self
                    .conn
                    .query_row("SELECT sqlite_version()", [], |r| r.get(0)))?;
            Ok(SchemaInfo {
                application_id,
                user_version: u32::try_from(user_version).map_err(StoreError::decode)?,
                projection_version,
                sqlite_version,
            })
        })
    }

    /// `PRAGMA integrity_check(max_errors)`; empty when the database is intact.
    ///
    /// # Errors
    ///
    /// A store error.
    pub fn integrity_check(&self, max_errors: u32) -> Result<Vec<String>, StoreError> {
        self.guard(|| {
            let lines: Vec<String> =
                self.rows(&format!("PRAGMA integrity_check({max_errors})"), [], |r| {
                    r.get(0)
                })?;
            Ok(if lines == ["ok"] { Vec::new() } else { lines })
        })
    }
}

fn launch_info(l: &LaunchRow) -> LaunchInfo {
    LaunchInfo {
        launch_id: l.launch_id,
        pid: l.pid,
        manual: l.manual,
        exit_reason: l.exit_reason,
        death_confirmed: l.death_confirmed,
    }
}

fn thread_info(t: &ThreadRow) -> ThreadInfo {
    ThreadInfo {
        thread_id: t.thread_id,
        agent: Open::Known(t.agent),
        cwd: t.cwd.clone(),
        workspace_id: t.workspace_id.clone(),
        parent_thread_id: t.parent_thread_id,
        role: t.role,
        closed: t.closed,
        process_state: t.state.process,
        turn_state: t.state.turn,
        task_state: t.state.task,
        attention: t.state.attention.iter().copied().collect(),
    }
}

fn decode_launch(v: &[rusqlite::types::Value]) -> Result<LaunchRow, StoreError> {
    use rusqlite::types::Value;
    let text = |i: usize| -> Result<Option<String>, StoreError> {
        match &v[i] {
            Value::Null => Ok(None),
            Value::Text(s) => Ok(Some(s.clone())),
            other => Err(StoreError::decode(format!("launch column {i}: {other:?}"))),
        }
    };
    let int = |i: usize| -> Result<Option<i64>, StoreError> {
        match &v[i] {
            Value::Null => Ok(None),
            Value::Integer(n) => Ok(Some(*n)),
            other => Err(StoreError::decode(format!("launch column {i}: {other:?}"))),
        }
    };
    let need = |o: Option<String>| o.ok_or_else(|| StoreError::decode("missing launch column"));
    let needi = |o: Option<i64>| o.ok_or_else(|| StoreError::decode("missing launch column"));
    let readiness = match (int(18)?, text(19)?) {
        (Some(seq), Some(ev)) => Some(ReadinessMark {
            seq: to_u64(seq)?,
            evidence: parse_enum(&ev)?,
        }),
        _ => None,
    };
    Ok(LaunchRow {
        launch_id: LaunchId(parse_uuid(&need(text(0)?)?)?),
        thread_id: ThreadId(parse_uuid(&need(text(1)?)?)?),
        reserved_seq: to_u64(needi(int(2)?)?)?,
        expected_previous: opt_uuid(text(3)?.as_deref())?.map(LaunchId),
        agent: parse_enum(&need(text(4)?)?)?,
        argv_digest: need(text(5)?)?,
        manual: needi(int(6)?)? != 0,
        worktree_id: text(7)?.map(WorktreeId),
        pid: int(8)?
            .map(u32::try_from)
            .transpose()
            .map_err(StoreError::decode)?,
        start_time: text(9)?,
        pgid_or_job: text(10)?,
        cli_version: text(11)?,
        exit_code: int(12)?
            .map(i32::try_from)
            .transpose()
            .map_err(StoreError::decode)?,
        exit_signal: int(13)?
            .map(i32::try_from)
            .transpose()
            .map_err(StoreError::decode)?,
        exit_reason: text(14)?.as_deref().map(parse_enum).transpose()?,
        death_confirmed: needi(int(15)?)? != 0,
        death_method: text(16)?,
        session_id: text(17)?,
        readiness,
        write_basis: text(20)?.as_deref().map(parse_json).transpose()?,
        last_write_seq: opt_u64(int(21)?)?,
        last_human_write_seq: opt_u64(int(22)?)?,
        last_turn_start_seq: opt_u64(int(23)?)?,
    })
}

/// Records whether a watched launch row was read (unit tests only), for tests that show a
/// read never ran: process-wide, so each test watches only its own launch ids.
#[cfg(test)]
pub(crate) mod launch_reads {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use chaldea_core::ids::LaunchId;

    use crate::store::lock;

    static WATCHED: Mutex<BTreeMap<LaunchId, bool>> = Mutex::new(BTreeMap::new());

    /// Starts watching `l`, as not read.
    pub(crate) fn watch(l: LaunchId) {
        lock(&WATCHED).insert(l, false);
    }

    /// Whether `l` was read since [`watch`].
    pub(crate) fn was_read(l: LaunchId) -> bool {
        lock(&WATCHED).get(&l).copied().unwrap_or(false)
    }

    pub(super) fn seen(l: LaunchId) {
        if let Some(read) = lock(&WATCHED).get_mut(&l) {
            *read = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use rusqlite::Connection;

    use super::{Pool, ReadError, lock};

    /// A reader freed while a borrower waits, but taken in only after the borrower's deadline
    /// passed, is not lent.
    #[test]
    fn a_reader_freed_past_the_deadline_is_not_lent() {
        let pool = Pool::new(Vec::new(), std::sync::Arc::default());
        let wait_until = Instant::now() + Duration::from_millis(200);
        std::thread::scope(|sc| {
            let waiter = sc.spawn(|| pool.borrow(wait_until).map(drop));
            // Once the lock is taken after the wait was counted, the borrower waits; the
            // reader comes back under the lock, which is held until the deadline is reached.
            while pool.waits.load(std::sync::atomic::Ordering::Acquire) == 0
                && !waiter.is_finished()
            {
                std::thread::yield_now();
            }
            let mut s = lock(&pool.state);
            // The reader comes back while the borrower waits.
            assert!(pool.waits.load(std::sync::atomic::Ordering::Acquire) > 0);
            assert!(
                !waiter.is_finished(),
                "the borrower ended before the reader came back"
            );
            s.idle.push(Connection::open_in_memory().unwrap());
            s.open += 1;
            while Instant::now() < wait_until {
                std::thread::sleep(wait_until.saturating_duration_since(Instant::now()));
            }
            drop(s);
            pool.cv.notify_all();
            assert!(matches!(waiter.join().unwrap(), Err(ReadError::Busy)));
        });
    }
}
