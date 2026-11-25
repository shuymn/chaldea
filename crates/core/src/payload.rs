//! `Content` and the payload of every event kind (`docs/mvp-design.md` §2.1 "Event kinds").
//!
//! A field marked `| null` in the design is an `Option` here and is always present on the
//! wire (as `null`), so a payload has one spelling.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::ids::{
    Actor, Agent, ApprovalId, InputId, LaunchId, MessageId, SurfaceId, ThreadId, WorkspaceId,
    WorktreeId,
};
use crate::input::{InputReason, InputState};
use crate::state::TaskState;

/// Above this many bytes, content moves to a blob (§2.1 `Content`).
pub const INLINE_LIMIT_BYTES: usize = 64 * 1024;
/// A blob's `preview` is its first this many bytes, decoded as UTF-8 (see `Content::Blob`).
pub const PREVIEW_LIMIT_BYTES: usize = 1024;

/// Every payload field that can be large: inline text or bytes, or a blob reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(untagged)]
pub enum Content {
    Inline {
        inline: String,
        encoding: Encoding,
    },
    Blob {
        blob: BlobRef,
        /// The first `PREVIEW_LIMIT_BYTES` bytes decoded as UTF-8, each invalid
        /// sequence replaced by U+FFFD. A valid character that crosses the limit is left
        /// out, not replaced.
        preview: String,
        /// True when any invalid sequence was replaced; a character left out at the
        /// limit does not set it.
        preview_lossy: bool,
    },
}

impl Content {
    /// Inline UTF-8 text.
    #[must_use]
    pub fn text(s: impl Into<String>) -> Self {
        Content::Inline {
            inline: s.into(),
            encoding: Encoding::Utf8,
        }
    }
}

/// `utf8` carries text; `base64` carries bytes that are not valid UTF-8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    Utf8,
    Base64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct BlobRef {
    /// Lowercase hex SHA-256 of the full content.
    pub sha256: String,
    #[serde(
        serialize_with = "crate::wire::ser_safe_u64",
        deserialize_with = "crate::wire::safe_u64"
    )]
    pub size: u64,
    pub media_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadRole {
    Standard,
    /// Spawned by the owner with `--role rescue`; holds a Rescue token (§2.5).
    Rescue,
}

// ---- host stream: threads -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct ThreadRegistered {
    pub thread_id: ThreadId,
    pub agent: Agent,
    pub cwd: String,
    pub workspace_id: WorkspaceId,
    pub parent_thread_id: Option<ThreadId>,
    pub role: ThreadRole,
    /// The task given at spawn; `task_state` starts `active` when present, else `none`.
    pub task: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct ThreadClosed {
    pub thread_id: ThreadId,
    pub by: Actor,
}

// ---- thread stream: launches ----------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct LaunchReserved {
    pub launch_id: LaunchId,
    /// The launch the command expected to be current (§2.9).
    pub expected_previous: Option<LaunchId>,
    pub agent: Agent,
    /// Lowercase hex SHA-256 of the argv, so the prompt in argv is not stored here.
    pub argv_digest: String,
    /// A launch with no first prompt that Chaldea controls (§8 D5).
    pub manual: bool,
    pub worktree_id: Option<WorktreeId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct LaunchStarted {
    pub launch_id: LaunchId,
    pub pid: u32,
    /// The OS's process start time, opaque; with `pid` it identifies the process.
    pub start_time: String,
    /// The process group (Unix) or job object name (Windows), opaque.
    pub pgid_or_job: String,
    pub cli_version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    Exit,
    Stopped,
    /// The child never started; reduced as an ordinary exit (`StateInput::Exited`).
    SpawnFailed,
    DaemonRestart,
    UnknownAfterCrash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct LaunchExited {
    pub launch_id: LaunchId,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub reason: ExitReason,
    /// False until the whole process tree is proven dead (§2.9 "Daemon restart").
    pub death_confirmed: bool,
    /// The identity a failed start returned when its cleanup is unresolved (a launch with no
    /// `launch_started`). Absent otherwise, never `null`: the one exception to this module's
    /// rule that an `Option` is present as `null`, so every earlier payload keeps its
    /// spelling.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::wire::present_object"
    )]
    #[ts(optional)]
    #[schemars(with = "FailedStart")]
    pub failed_start: Option<FailedStart>,
}

/// The identity a `StartedThenFailed` spawn returned. A part the backend returned empty is
/// absent (never `null`), so such a record never identifies a process by its pid alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct FailedStart {
    pub pid: u32,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::wire::present"
    )]
    #[ts(optional)]
    #[schemars(with = "String")]
    pub start_time: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::wire::present"
    )]
    #[ts(optional)]
    #[schemars(with = "String")]
    pub pgid_or_job: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct LaunchDeathConfirmed {
    pub launch_id: LaunchId,
    /// How death was proven, e.g. a session scan or a job query.
    pub method: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct SessionBound {
    pub launch_id: LaunchId,
    pub session_id: String,
    pub transcript_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct TranscriptGeneration {
    #[serde(
        serialize_with = "crate::wire::ser_safe_u64",
        deserialize_with = "crate::wire::safe_u64"
    )]
    pub generation: u64,
    pub path: String,
    /// Device + inode on Unix, volume serial + file index on Windows, opaque.
    pub file_id: String,
}

// ---- thread stream: turns and input gate ----------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessEvidence {
    SessionStart,
    StopSettled,
    PostDecision,
    /// `chaldea ready` by the owner, for the current bound launch only.
    OwnerAsserted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct ReadinessObserved {
    pub launch_id: LaunchId,
    pub evidence: ReadinessEvidence,
    pub by: Actor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HumanWriteOrigin {
    Attach,
    Key,
    /// A key sent from an open approval card; it does not close the input gate.
    CardKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct HumanWrite {
    pub launch_id: LaunchId,
    pub origin: HumanWriteOrigin,
    /// Set for `card_key`.
    pub approval_id: Option<ApprovalId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct UserMessage {
    pub prompt: Content,
    /// The input this prompt acknowledged, when attributed (§2.3 "Ack").
    pub input_id: Option<InputId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct AssistantMessage {
    pub text: Content,
    /// The CLI's own turn id when its payload has one.
    pub turn_ref: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolPhase {
    Started,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct ToolActivity {
    pub phase: ToolPhase,
    pub tool_name: String,
    pub tool_use_id: Option<String>,
    pub turn_ref: Option<String>,
    pub input: Content,
    pub output: Option<Content>,
}

// ---- thread stream: approvals ----------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct ApprovalCorrelation {
    pub tool_use_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct ApprovalRequested {
    pub approval_id: ApprovalId,
    pub launch_id: LaunchId,
    pub turn_ref: Option<String>,
    pub tool_name: String,
    pub tool_input: Content,
    /// Present only when exactly one `PreToolUse` of the same turn matches (§2.10).
    pub correlation: Option<ApprovalCorrelation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOutcome {
    /// A `PostToolUse` with the card's `tool_use_id`.
    Ran,
    /// The launch exited with the card open.
    Expired,
    /// A `Stop` or interrupt closed the card.
    Superseded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct ApprovalResolved {
    pub approval_id: ApprovalId,
    pub outcome: ApprovalOutcome,
}

// ---- thread stream: inputs ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputInitialState {
    Queued,
    /// A Codex positional first prompt, whose bytes travel in argv (§2.3).
    Sending,
}

/// How the input's text reaches the CLI (§2.7 "Text encoding", "First prompt").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputEncoding {
    BracketedPaste,
    WinKeys,
    /// The launch's positional argument.
    Positional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputOrigin {
    Cli,
    Pwa,
    SurfaceAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct InputCreated {
    pub input_id: InputId,
    pub launch_id: Option<LaunchId>,
    pub initial_state: InputInitialState,
    pub text: Content,
    pub encoding: InputEncoding,
    pub origin: InputOrigin,
    pub created_by: Actor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct InputStateChanged {
    pub input_id: InputId,
    pub from: InputState,
    pub to: InputState,
    pub reason: InputReason,
    /// The thread stream's head when `sending` was recorded.
    #[serde(
        default,
        serialize_with = "crate::wire::ser_opt_safe_u64",
        deserialize_with = "crate::wire::opt_safe_u64"
    )]
    pub sent_after_seq: Option<u64>,
    /// Set when a person or thread caused the change (cancel, resolve).
    pub by: Option<Actor>,
}

// ---- thread stream: orchestration ------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct MailboxMessage {
    pub message_id: MessageId,
    /// `null` when a human sent it.
    pub from_thread_id: Option<ThreadId>,
    pub to_thread_id: ThreadId,
    pub body: Content,
    pub in_reply_to: Option<MessageId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct TaskSet {
    pub to: TaskState,
    pub by: Actor,
    pub summary: Option<String>,
}

// ---- surfaces (§2.8) ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SurfaceScope {
    Host,
    Workspace { workspace_id: WorkspaceId },
    Thread { thread_id: ThreadId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceActionKind {
    SendInput,
    SendKey,
    Stop,
    Ready,
    OpenAttach,
}

/// Run only when a human with an owner token clicks its button.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct SurfaceAction {
    pub action_id: String,
    pub kind: SurfaceActionKind,
    /// Action arguments, validated by the command the action runs.
    #[ts(type = "unknown")]
    pub args: serde_json::Value,
}

/// The minimal A2UI catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Component {
    Text {
        text: String,
    },
    Markdown {
        markdown: String,
    },
    VtSnapshot {
        /// The grid in the `vt_snapshot_taken` encoding.
        grid: Content,
        cols: u16,
        rows: u16,
    },
    Button {
        label: String,
        action: SurfaceAction,
    },
    TextInput {
        name: String,
        label: String,
    },
    List {
        items: Vec<Component>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct SurfaceUpdate {
    pub surface_id: SurfaceId,
    pub scope: SurfaceScope,
    /// An empty list removes the surface.
    pub components: Vec<Component>,
}

// ---- VT snapshot -------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct VtCursor {
    /// Zero-based cell coordinates in the grid.
    pub row: u16,
    pub col: u16,
    pub visible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct VtModes {
    /// DECSET 2004.
    pub bracketed_paste: bool,
    pub alternate_screen: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct VtSnapshotTaken {
    /// A JSON array of exactly `rows` rows, top to bottom, each an array of at most
    /// `cols` cell strings, left to right. `""` is the second cell of a wide character and
    /// directly follows it; every other cell is non-empty (a blank cell is `" "`). Trailing
    /// blank cells are omitted; a reader fills them back to `cols`. A grid of any other
    /// shape is an unreadable snapshot. Above 64 KiB the grid is a blob, fetched before
    /// decoding (`docs/design/pty-runtime.md` §2.6).
    pub grid: Content,
    pub cols: u16,
    pub rows: u16,
    pub cursor: VtCursor,
    pub modes: VtModes,
}

// ---- worktrees (§2.6) --------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeState {
    Creating,
    Active,
    Released,
    Removing,
    Uncertain,
    Removed,
    Failed,
}

/// The full worktree record, carried on every change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct WorktreeChanged {
    pub worktree_id: WorktreeId,
    pub repo: String,
    pub workspace_id: WorkspaceId,
    pub path: String,
    pub branch: String,
    pub base_ref: String,
    /// The resolved SHA of `base_ref`.
    pub base_commit: String,
    /// `null` when the human owner holds it.
    pub owner_thread_id: Option<ThreadId>,
    pub created_by: Actor,
    pub state: WorktreeState,
}

// ---- raw_unknown ---------------------------------------------------------------------------

/// Where an event came from (`source.kind` of the envelope).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Hook,
    Transcript,
    Process,
    Command,
    Daemon,
    Extension,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RawUnknownReason {
    Unparseable,
    UnknownEvent,
    ForeignSession,
    Unbound,
    SchemaMismatch,
    PartialLine,
}

/// Input kept verbatim because it could not be parsed or bound; nothing is dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct RawUnknown {
    pub source: SourceKind,
    pub reason: RawUnknownReason,
    pub agent: Option<Agent>,
    pub cli_version: Option<String>,
    pub content_type: String,
    pub raw: Content,
}
