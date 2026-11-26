//! The sync API contract: replay decision, responses, and WS messages
//! (`docs/mvp-design.md` §2.4).

use schemars::JsonSchema;
use serde::de::{self, DeserializeOwned};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;
use ts_rs::TS;

use crate::adapter::HostOs;
use crate::event::EventEnvelope;
use crate::http::ErrorBody;
use crate::ids::{
    Actor, Agent, Epoch, InputId, LaunchId, Open, StreamId, ThreadId, Timestamp, WorkspaceId,
};
use crate::input::InputState;
use crate::payload::{
    ApprovalRequested, Content, ExitReason, InputEncoding, ReadinessEvidence, SurfaceUpdate,
    ThreadRole, VtCursor, VtModes, VtSnapshotTaken, WorktreeChanged,
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

// ---- adapters and thread summaries ----------------------------------------------------------------

/// The body of `GET /api/adapters`: this host's rows of the capability table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct AdaptersResponse {
    pub host_os: Open<HostOs>,
    /// This host's rows, in `CAPABILITIES` order.
    pub adapters: Vec<AdapterInfo>,
}

/// One row of the capability table, as clients see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct AdapterInfo {
    pub agent: Open<Agent>,
    /// `bracketed_paste` or `win_keys`.
    pub pty_encoding: Open<InputEncoding>,
    /// `bracketed_paste`, `win_keys`, or `positional`.
    pub first_prompt: Open<InputEncoding>,
}

/// The body of `GET /api/threads/{thread_id}`: fixed-size facts of one thread, whatever its
/// history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct ThreadSummary {
    pub thread_id: ThreadId,
    pub closed: bool,
    pub process_state: ProcessState,
    pub turn_state: TurnState,
    pub task_state: TaskState,
    /// Unresolved attention causes.
    pub attention_count: u32,
    /// The current launch.
    pub launch: Option<LaunchInfo>,
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

// ---- the rescue read surface -------------------------------------------------------------------

/// The largest serialized body of a [`ScreenResponse`]: half the CLI client's 1 MiB body limit.
pub const SCREEN_MAX_BODY: usize = 512 * 1024;

/// The body of `GET /api/threads/{thread_id}/screen`: the live screen of the thread's current
/// launch, one text line per row. The daemon builds it with [`ScreenResponse::new`], which
/// keeps the serialized body within [`SCREEN_MAX_BODY`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct ScreenResponse {
    pub thread_id: ThreadId,
    pub launch_id: LaunchId,
    pub cols: u16,
    pub rows: u16,
    /// In cell coordinates (a wide character takes two columns).
    pub cursor: VtCursor,
    pub modes: VtModes,
    /// Exactly `rows` lines, top to bottom: each row's cells without the second half of a
    /// wide character, combining characters kept, trailing spaces trimmed.
    pub lines: Vec<String>,
    /// Some line was cut to keep the body within its bound.
    pub truncated: bool,
}

/// The length of `c` inside a JSON string as `serde_json` writes it.
fn escaped_char_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
        c if u32::from(c) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// `line` cut at a character boundary so that its JSON-escaped form is at most `share`
/// bytes; `None` when it fits whole.
fn cut_to(line: &str, share: usize) -> Option<usize> {
    let mut used = 0;
    for (i, c) in line.char_indices() {
        used += escaped_char_len(c);
        if used > share {
            return Some(i);
        }
    }
    None
}

impl ScreenResponse {
    /// The response for a screen of `rows` rows whose lines are `lines` (padded with empty
    /// lines or cut to `rows`). Its serialized body is at most [`SCREEN_MAX_BODY`]: the
    /// response with `rows` empty lines is the fixed part, and the rest is shared evenly
    /// among the rows' escaped contents; a row above its share is cut at a character
    /// boundary (never inside an escape) and `truncated` is set.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "one argument per field of the response"
    )]
    pub fn new(
        thread_id: ThreadId,
        launch_id: LaunchId,
        cols: u16,
        rows: u16,
        cursor: VtCursor,
        modes: VtModes,
        mut lines: Vec<String>,
    ) -> ScreenResponse {
        lines.resize_with(usize::from(rows), String::new);
        let mut r = ScreenResponse {
            thread_id,
            launch_id,
            cols,
            rows,
            cursor,
            modes,
            lines: vec![String::new(); usize::from(rows)],
            // The longer literal, so the fixed part is not undercounted.
            truncated: false,
        };
        let fixed = serde_json::to_vec(&r).map_or(SCREEN_MAX_BODY, |b| b.len());
        let share = SCREEN_MAX_BODY
            .saturating_sub(fixed)
            .checked_div(usize::from(rows))
            .unwrap_or(0);
        for line in &mut lines {
            if let Some(at) = cut_to(line, share) {
                line.truncate(at);
                r.truncated = true;
            }
        }
        r.lines = lines;
        r
    }
}

/// The output of `chaldea inspect <thread> --json`. On success `snapshot` is set and
/// `summary` is not; when the snapshot could not be read for its size or in time, `summary`
/// is the thread's [`ThreadSummary`] instead (no attention causes, session, inputs, cards,
/// or events). `screen` is the live screen of the launch the read showed; each error is the
/// daemon's, or the client's own (`payload_too_large`, `request_timeout`, `internal`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, JsonSchema)]
pub struct InspectOutput {
    pub thread_id: ThreadId,
    pub snapshot: Option<ThreadSnapshot>,
    pub summary: Option<ThreadSummary>,
    pub screen: Option<ScreenResponse>,
    pub snapshot_error: Option<ErrorBody>,
    /// `launch_changed` when the screen is of another launch than the one read.
    pub screen_error: Option<ErrorBody>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: ThreadId = ThreadId::from_u128(1);
    const L: LaunchId = LaunchId::from_u128(2);

    fn screen(rows: u16, lines: Vec<String>) -> ScreenResponse {
        ScreenResponse::new(
            T,
            L,
            80,
            rows,
            VtCursor {
                row: 0,
                col: 0,
                visible: true,
            },
            VtModes {
                bracketed_paste: false,
                alternate_screen: false,
            },
            lines,
        )
    }

    #[test]
    fn escaped_len_matches_serde() {
        let mut cs: Vec<char> = (0u32..0x80).filter_map(char::from_u32).collect();
        cs.extend(['é', '日', '\u{301}', '\u{2028}', '😀', '\u{7f}']);
        for c in cs {
            let s = serde_json::to_string(&c.to_string()).unwrap();
            assert_eq!(escaped_char_len(c), s.len() - 2, "{c:?}");
        }
    }

    /// C1-11: at `rows` = 4095 with escape-heavy rows, the body stays within the bound,
    /// every row is kept, rows are cut on character boundaries, and `truncated` is set.
    #[test]
    fn screen_bound_at_max_rows() {
        let rows = 4095;
        let row = "\"\\\u{1}日\u{7}".repeat(200);
        let r = screen(rows, vec![row.clone(); usize::from(rows)]);
        let body = serde_json::to_vec(&r).unwrap();
        assert!(body.len() <= SCREEN_MAX_BODY, "{}", body.len());
        assert_eq!(r.lines.len(), usize::from(rows));
        assert!(r.truncated);
        for l in &r.lines {
            assert!(row.starts_with(l.as_str()));
            assert!(!l.is_empty());
        }
        let back: ScreenResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(back, r);
    }

    /// One row far above its share next to empty rows: cut, and only that row.
    #[test]
    fn screen_bound_one_long_row() {
        let long = "x".repeat(SCREEN_MAX_BODY);
        let r = screen(3, vec![String::new(), long, "ok".into()]);
        assert!(serde_json::to_vec(&r).unwrap().len() <= SCREEN_MAX_BODY);
        assert!(r.truncated);
        assert_eq!((r.lines[0].as_str(), r.lines[2].as_str()), ("", "ok"));
        assert!(r.lines[1].len() > SCREEN_MAX_BODY / 4);
    }

    /// A normal screen is whole, and the line count is `rows` whatever was given.
    #[test]
    fn screen_normal_untruncated() {
        let r = screen(4, vec!["日本語".into(), "e\u{301}".into()]);
        assert!(!r.truncated);
        assert_eq!(r.lines, ["日本語", "e\u{301}", "", ""]);
        let r = screen(1, vec!["a".into(), "b".into()]);
        assert_eq!(r.lines, ["a"]);
    }
}
