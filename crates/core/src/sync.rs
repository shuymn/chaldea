//! The sync API contract: replay decision, responses, and WS messages
//! (`docs/mvp-design.md` §2.4).

use schemars::JsonSchema;
use serde::de::{self, DeserializeOwned};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;
use ts_rs::TS;

use crate::event::EventEnvelope;
use crate::ids::{
    Actor, Agent, Epoch, InputId, LaunchId, Open, StreamId, ThreadId, Timestamp, WorkspaceId,
};
use crate::input::InputState;
use crate::payload::{
    ApprovalRequested, Content, ExitReason, InputEncoding, ReadinessEvidence, SurfaceUpdate,
    ThreadRole, VtSnapshotTaken, WorktreeChanged,
};
use crate::state::{AttentionCause, ProcessState, TaskState, TurnState};

pub const LIMIT_MIN: u32 = 1;
pub const LIMIT_MAX: u32 = 1000;
pub const LIMIT_DEFAULT: u32 = 200;
/// A forward replay longer than this many events falls back to a snapshot.
pub const REPLAY_MAX_EVENTS: u64 = 10_000;
/// A forward replay larger than this many payload bytes falls back to a snapshot.
pub const REPLAY_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Events carried in a thread snapshot.
pub const SNAPSHOT_RECENT_EVENTS: usize = 50;

/// The query of `GET /api/events`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventsQuery {
    pub stream_id: StreamId,
    pub after_seq: Option<u64>,
    pub before_seq: Option<u64>,
    pub limit: Option<u32>,
    /// Omitted on a client's first fetch.
    pub epoch: Option<Epoch>,
}

/// What the store knows about the stream, read in the same transaction as the reply.
/// Byte counts are the per-stream running `cum_bytes` of the event payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamHead {
    /// 0 for an empty stream.
    pub head_seq: u64,
    pub cum_bytes_at_head: u64,
    /// `cum_bytes` at the query's `after_seq` (0 for `after_seq` 0); ignored when
    /// `after_seq` is above `head_seq`.
    pub cum_bytes_at_after: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotReason {
    EpochChanged,
    /// `after_seq` is above the stream head.
    CursorAhead,
    TooManyEvents,
    TooManyBytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayPlan {
    /// Events with `seq > after_seq`, ascending, at most `limit`.
    Forward {
        after_seq: u64,
        limit: u32,
    },
    /// The nearest `limit` events with `seq < before_seq`, ascending. An epoch mismatch
    /// does not change the plan: the reply carries the server's epoch and the client
    /// resyncs forward.
    Backward {
        before_seq: u64,
        limit: u32,
    },
    Snapshot {
        reason: SnapshotReason,
    },
}

/// A query the server answers with 400.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueryError {
    /// `limit` outside `LIMIT_MIN..=LIMIT_MAX`.
    LimitOutOfRange,
    AfterAndBefore,
}

/// Decides how to answer `GET /api/events`, as T3 Code's `decideThreadResume`.
/// A stream that does not exist or that the token may not read is a 404 decided before
/// this call.
///
/// # Errors
///
/// `QueryError` for a query the server answers with 400.
pub fn decide_replay(
    q: &EventsQuery,
    head: StreamHead,
    epoch: &Epoch,
) -> Result<ReplayPlan, QueryError> {
    let limit = q.limit.unwrap_or(LIMIT_DEFAULT);
    if !(LIMIT_MIN..=LIMIT_MAX).contains(&limit) {
        return Err(QueryError::LimitOutOfRange);
    }
    if let Some(before_seq) = q.before_seq {
        if q.after_seq.is_some() {
            return Err(QueryError::AfterAndBefore);
        }
        return Ok(ReplayPlan::Backward { before_seq, limit });
    }
    let after_seq = q.after_seq.unwrap_or(0);
    let snapshot = |reason| Ok(ReplayPlan::Snapshot { reason });
    if q.epoch.as_ref().is_some_and(|e| e != epoch) {
        return snapshot(SnapshotReason::EpochChanged);
    }
    if after_seq > head.head_seq {
        return snapshot(SnapshotReason::CursorAhead);
    }
    if head.head_seq - after_seq > REPLAY_MAX_EVENTS {
        return snapshot(SnapshotReason::TooManyEvents);
    }
    if head
        .cum_bytes_at_head
        .saturating_sub(head.cum_bytes_at_after)
        > REPLAY_MAX_BYTES
    {
        return snapshot(SnapshotReason::TooManyBytes);
    }
    Ok(ReplayPlan::Forward { after_seq, limit })
}

// ---- snapshots -------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct ThreadInfo {
    pub thread_id: ThreadId,
    pub agent: Open<Agent>,
    pub cwd: String,
    pub workspace_id: WorkspaceId,
    pub parent_thread_id: Option<ThreadId>,
    pub role: ThreadRole,
    pub closed: bool,
    pub process_state: ProcessState,
    pub turn_state: TurnState,
    pub task_state: TaskState,
    /// Sorted.
    pub attention: Vec<AttentionCause>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct LaunchInfo {
    pub launch_id: LaunchId,
    pub pid: Option<u32>,
    pub manual: bool,
    pub exit_reason: Option<ExitReason>,
    pub death_confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct SessionInfo {
    pub session_id: String,
    pub transcript_path: Option<String>,
}

/// The gate evidence of the current launch, as thread-stream positions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct GateEvidence {
    #[serde(
        default,
        serialize_with = "crate::wire::ser_opt_safe_u64",
        deserialize_with = "crate::wire::opt_safe_u64"
    )]
    pub readiness_seq: Option<u64>,
    pub readiness_evidence: Option<ReadinessEvidence>,
    /// The last write of any origin.
    #[serde(
        default,
        serialize_with = "crate::wire::ser_opt_safe_u64",
        deserialize_with = "crate::wire::opt_safe_u64"
    )]
    pub last_write_seq: Option<u64>,
    /// The last human write that closes the gate (`attach` or `key`).
    #[serde(
        default,
        serialize_with = "crate::wire::ser_opt_safe_u64",
        deserialize_with = "crate::wire::opt_safe_u64"
    )]
    pub last_human_write_seq: Option<u64>,
    #[serde(
        default,
        serialize_with = "crate::wire::ser_opt_safe_u64",
        deserialize_with = "crate::wire::opt_safe_u64"
    )]
    pub last_turn_start_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct InputInfo {
    pub input_id: InputId,
    pub launch_id: Option<LaunchId>,
    pub state: InputState,
    pub text: Content,
    pub encoding: InputEncoding,
    pub created_by: Actor,
    #[serde(
        default,
        serialize_with = "crate::wire::ser_opt_safe_u64",
        deserialize_with = "crate::wire::opt_safe_u64"
    )]
    pub sent_after_seq: Option<u64>,
}

/// A thread's projection plus its latest events (§2.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, JsonSchema)]
pub struct ThreadSnapshot {
    pub thread: ThreadInfo,
    pub launch: Option<LaunchInfo>,
    pub session: Option<SessionInfo>,
    pub gate: GateEvidence,
    /// Inputs not yet terminal.
    pub inputs: Vec<InputInfo>,
    pub open_approvals: Vec<ApprovalRequested>,
    pub worktree: Option<WorktreeChanged>,
    pub surfaces: Vec<SurfaceUpdate>,
    /// The latest `vt_snapshot_taken`, while an attention cause is active or a card is open.
    pub vt_snapshot: Option<VtSnapshotTaken>,
    /// The last `SNAPSHOT_RECENT_EVENTS` events of the stream.
    pub recent_events: Vec<EventEnvelope>,
}

/// The thread list, worktrees, and host- and workspace-scoped surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct HostSnapshot {
    pub threads: Vec<ThreadInfo>,
    pub worktrees: Vec<WorktreeChanged>,
    pub surfaces: Vec<SurfaceUpdate>,
}

#[derive(Debug, Clone, PartialEq, Serialize, TS, JsonSchema)]
#[serde(tag = "stream", rename_all = "snake_case")]
pub enum StreamSnapshot {
    Host(HostSnapshot),
    Thread(Box<ThreadSnapshot>),
}

/// The body of `GET /api/events`.
#[derive(Debug, Clone, PartialEq, Serialize, TS, JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum EventsResponse {
    Replay {
        stream_id: StreamId,
        epoch: Epoch,
        events: Vec<EventEnvelope>,
        #[serde(serialize_with = "crate::wire::ser_safe_u64")]
        head_seq: u64,
        has_more: bool,
        has_more_before: bool,
    },
    Snapshot {
        stream_id: StreamId,
        epoch: Epoch,
        snapshot: StreamSnapshot,
        #[serde(serialize_with = "crate::wire::ser_safe_u64")]
        as_of_seq: u64,
        reason: SnapshotReason,
    },
}

// serde's derived internally tagged enums buffer their input in serde's private content
// type, which cannot produce the envelope's `RawValue` payload. These two enums therefore
// capture the raw JSON first, read their tag from it, and then decode the variant's fields
// from the same text. They decode from JSON text and from a `serde_json::Value` alike.

#[derive(Deserialize)]
struct StreamTag {
    stream: String,
}

#[derive(Deserialize)]
struct ModeTag {
    mode: String,
}

fn fields<T: DeserializeOwned, E: de::Error>(raw: &RawValue) -> Result<T, E> {
    serde_json::from_str(raw.get()).map_err(E::custom)
}

impl<'de> Deserialize<'de> for StreamSnapshot {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = Box::<RawValue>::deserialize(d)?;
        let tag: StreamTag = fields(&raw)?;
        match tag.stream.as_str() {
            "host" => fields(&raw).map(StreamSnapshot::Host),
            "thread" => fields(&raw).map(StreamSnapshot::Thread),
            other => Err(de::Error::unknown_variant(other, &["host", "thread"])),
        }
    }
}

impl<'de> Deserialize<'de> for EventsResponse {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Replay {
            stream_id: StreamId,
            epoch: Epoch,
            events: Vec<EventEnvelope>,
            #[serde(deserialize_with = "crate::wire::safe_u64")]
            head_seq: u64,
            has_more: bool,
            has_more_before: bool,
        }
        #[derive(Deserialize)]
        struct Snapshot {
            stream_id: StreamId,
            epoch: Epoch,
            snapshot: StreamSnapshot,
            #[serde(deserialize_with = "crate::wire::safe_u64")]
            as_of_seq: u64,
            reason: SnapshotReason,
        }

        let raw = Box::<RawValue>::deserialize(d)?;
        let tag: ModeTag = fields(&raw)?;
        match tag.mode.as_str() {
            "replay" => fields(&raw).map(|r: Replay| EventsResponse::Replay {
                stream_id: r.stream_id,
                epoch: r.epoch,
                events: r.events,
                head_seq: r.head_seq,
                has_more: r.has_more,
                has_more_before: r.has_more_before,
            }),
            "snapshot" => fields(&raw).map(|s: Snapshot| EventsResponse::Snapshot {
                stream_id: s.stream_id,
                epoch: s.epoch,
                snapshot: s.snapshot,
                as_of_seq: s.as_of_seq,
                reason: s.reason,
            }),
            other => Err(de::Error::unknown_variant(other, &["replay", "snapshot"])),
        }
    }
}

// ---- live notifications ------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct StreamHeadEntry {
    pub stream_id: StreamId,
    #[serde(
        serialize_with = "crate::wire::ser_safe_u64",
        deserialize_with = "crate::wire::safe_u64"
    )]
    pub head_seq: u64,
}

/// Server to client on `GET /api/ws`. Notify-then-pull: the client fetches events with
/// `GET /api/events`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsServerMessage {
    Hello {
        epoch: Epoch,
        server_time: Timestamp,
    },
    StreamAdvanced {
        stream_id: StreamId,
        #[serde(
            serialize_with = "crate::wire::ser_safe_u64",
            deserialize_with = "crate::wire::safe_u64"
        )]
        head_seq: u64,
    },
    /// Sent on every `subscribe` and every 30 s.
    Heads { heads: Vec<StreamHeadEntry> },
}

/// Client to server on `GET /api/ws`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsClientMessage {
    Subscribe { stream_ids: Vec<StreamId> },
}
