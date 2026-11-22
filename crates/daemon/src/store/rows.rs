//! Row types the store returns, and the conversions between core values and columns.

use std::collections::BTreeMap;

use chaldea_core::command::CommandResult;
use chaldea_core::ids::{
    Actor, Agent, ApprovalId, InputId, LaunchId, StreamId, ThreadId, Timestamp, TokenId,
    WorkspaceId, WorktreeId,
};
use chaldea_core::input::{InputState, ReadinessMark};
use chaldea_core::payload::{
    ApprovalOutcome, ApprovalRequested, Content, ExitReason, InputEncoding, InputOrigin,
    SurfaceUpdate, ThreadRole, WorktreeChanged,
};
use chaldea_core::state::ThreadState;
use rusqlite::types::Value;
use serde::Serialize;
use serde::de::DeserializeOwned;
use uuid::Uuid;

use super::annex::{Continuity, WriteBasis};
use super::error::StoreError;

/// A thread's projection row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRow {
    pub thread_id: ThreadId,
    pub agent: Agent,
    pub cwd: String,
    pub workspace_id: WorkspaceId,
    pub parent_thread_id: Option<ThreadId>,
    pub role: ThreadRole,
    pub closed: bool,
    pub state: ThreadState,
    pub current_launch_id: Option<LaunchId>,
    pub session_id: Option<String>,
    pub transcript_path: Option<String>,
    /// The host-stream `seq` of `thread_registered`.
    pub registered_seq: u64,
}

/// A launch's projection row, with its gate evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRow {
    pub launch_id: LaunchId,
    pub thread_id: ThreadId,
    pub reserved_seq: u64,
    pub expected_previous: Option<LaunchId>,
    pub agent: Agent,
    pub argv_digest: String,
    pub manual: bool,
    pub worktree_id: Option<WorktreeId>,
    pub pid: Option<u32>,
    pub start_time: Option<String>,
    pub pgid_or_job: Option<String>,
    pub cli_version: Option<String>,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
    pub exit_reason: Option<ExitReason>,
    pub death_confirmed: bool,
    pub death_method: Option<String>,
    pub session_id: Option<String>,
    /// The latest `readiness_observed` and its kind.
    pub readiness: Option<ReadinessMark>,
    /// The write basis stored with that evidence.
    pub write_basis: Option<WriteBasis>,
    pub last_write_seq: Option<u64>,
    pub last_human_write_seq: Option<u64>,
    pub last_turn_start_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputRow {
    pub input_id: InputId,
    pub thread_id: ThreadId,
    pub launch_id: Option<LaunchId>,
    pub state: InputState,
    pub text: Content,
    pub encoding: InputEncoding,
    pub origin: InputOrigin,
    pub created_by: Actor,
    pub sent_after_seq: Option<u64>,
    /// The `user_message` that acknowledged it.
    pub ack_seq: Option<u64>,
    pub created_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRow {
    pub approval_id: ApprovalId,
    pub thread_id: ThreadId,
    pub launch_id: LaunchId,
    pub requested: ApprovalRequested,
    pub opened_seq: u64,
    pub outcome: Option<ApprovalOutcome>,
    pub resolved_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceRow {
    pub stream_id: StreamId,
    pub update: SurfaceUpdate,
    pub updated_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRow {
    pub record: WorktreeChanged,
    pub updated_seq: u64,
}

/// A thread's transcript cursor: the event position (from the last transcript event) and
/// the read position (which may be past it, over lines that produced no event).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptCursor {
    pub generation: u64,
    pub path: String,
    pub file_id: String,
    pub event_offset: u64,
    pub event_continuity: Continuity,
    pub read_offset: u64,
    pub read_continuity: Continuity,
}

/// The thread's binding as a transcript batch sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub launch_id: Option<LaunchId>,
    pub session_id: Option<String>,
    pub transcript_path: Option<String>,
}

/// A blob's bytes and its download type for the stream that read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobBody {
    pub data: Vec<u8>,
    /// The stream's references' type when they all agree, else `application/octet-stream`.
    pub media_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    Owner,
    Agent,
    Rescue,
    Hook,
    Extension,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRow {
    pub token_id: TokenId,
    pub kind: TokenKind,
    pub sha256: [u8; 32],
    pub thread_id: Option<ThreadId>,
    pub launch_id: Option<LaunchId>,
    pub created_at: Timestamp,
    pub revoked_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaInfo {
    pub application_id: i32,
    pub user_version: u32,
    pub projection_version: String,
    pub sqlite_version: String,
}

/// The first reply of a hook delivery, immutable once stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookReply {
    pub http_status: u16,
    pub body: String,
}

/// PR 9's buffered hook: its raw payload and metadata (JSON), opaque to the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferedHook {
    pub body: Vec<u8>,
    pub meta: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Routing {
    Appended { stream_id: StreamId, seq: u64 },
    Buffered,
    RawUnknown { stream_id: StreamId, seq: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookDelivery {
    pub receipt_ordinal: u64,
    pub commit_ordinal: u64,
    pub first_reply: HookReply,
    pub routing: Routing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDeliveryLookup {
    New(HookDelivery),
    /// The delivery existed: its first reply and current routing; nothing changed.
    Replay(HookDelivery),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferedDelivery {
    pub emit_id: Uuid,
    pub receipt_ordinal: u64,
    pub received_at: Timestamp,
    pub hook: BufferedHook,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebuildReport {
    pub events: u64,
}

/// Differences between maintained and rebuilt projections, one line each; empty when they
/// agree under the permitted transcript rewind.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectionDiff {
    pub differences: Vec<String>,
}

impl ProjectionDiff {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.differences.is_empty()
    }
}

/// A receipt's stored result.
pub(crate) fn command_result(http_status: i64, body: String) -> Result<CommandResult, StoreError> {
    Ok(CommandResult {
        http_status: u16::try_from(http_status).map_err(StoreError::decode)?,
        body,
    })
}

// ---- column conversions ----------------------------------------------------------------

/// The snake_case string of a unit enum, as serde writes it.
pub(crate) fn enum_text<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        other => panic!("not a unit enum: {other:?}"),
    }
}

pub(crate) fn parse_enum<T: DeserializeOwned>(s: &str) -> Result<T, StoreError> {
    serde_json::from_value(serde_json::Value::String(s.to_owned())).map_err(StoreError::decode)
}

pub(crate) fn parse_json<T: DeserializeOwned>(s: &str) -> Result<T, StoreError> {
    serde_json::from_str(s).map_err(StoreError::decode)
}

pub(crate) fn to_json<T: Serialize>(v: &T) -> Result<String, StoreError> {
    serde_json::to_string(v).map_err(StoreError::decode)
}

pub(crate) fn parse_uuid(s: &str) -> Result<Uuid, StoreError> {
    Uuid::try_parse(s).map_err(StoreError::decode)
}

pub(crate) fn opt_uuid(s: Option<&str>) -> Result<Option<Uuid>, StoreError> {
    s.map(parse_uuid).transpose()
}

/// A length as an SQLite integer; lengths in memory never reach `i64::MAX`.
pub(crate) fn sql_len(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// SQLite integers are `i64`; stored positions are never negative.
pub(crate) fn to_u64(v: i64) -> Result<u64, StoreError> {
    u64::try_from(v).map_err(StoreError::decode)
}

pub(crate) fn opt_u64(v: Option<i64>) -> Result<Option<u64>, StoreError> {
    v.map(to_u64).transpose()
}

/// Binds a `u64` position; above `i64::MAX` it is out of SQLite's range.
pub(crate) fn to_i64(v: u64) -> Result<i64, StoreError> {
    i64::try_from(v).map_err(|_| StoreError::Decode(format!("{v} is above SQLite's integer range")))
}

/// One dumped projection table: column names and rows keyed by the primary key's text.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TableDump {
    pub(crate) columns: Vec<String>,
    pub(crate) rows: BTreeMap<String, Vec<Value>>,
}
