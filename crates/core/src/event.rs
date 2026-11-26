//! The open event envelope and forward-compatible decoding (`docs/mvp-design.md` §2.1).
//!
//! Readers validate the envelope first, with `kind` as a string and `payload` as raw JSON,
//! then decode the payload by kind. Anything they cannot decode becomes a
//! [`GenericEvent`]; it never fails a page or a stream.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use ts_rs::TS;

use crate::SCHEMA_VERSION;
use crate::ids::{
    Agent, CommandId, EventId, EventKindTag, IdempotencyKey, LaunchId, Open, StreamId, ThreadId,
    Timestamp,
};
use crate::payload::{
    ApprovalRequested, ApprovalResolved, AssistantMessage, HumanWrite, InputCreated,
    InputStateChanged, LaunchDeathConfirmed, LaunchEvidence, LaunchExited, LaunchReserved,
    LaunchStarted, MailboxMessage, RawUnknown, ReadinessObserved, SessionBound, SourceKind,
    SurfaceUpdate, TaskSet, ThreadClosed, ThreadRegistered, ToolActivity, TranscriptGeneration,
    UserMessage, VtSnapshotTaken, WorktreeChanged,
};
use crate::state::StatusChanged;

/// Where in a transcript an event's line ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct TranscriptPos {
    #[serde(
        serialize_with = "crate::wire::ser_safe_u64",
        deserialize_with = "crate::wire::safe_u64"
    )]
    pub generation: u64,
    pub path: String,
    pub file_id: String,
    /// Byte offset just after the line.
    #[serde(
        serialize_with = "crate::wire::ser_safe_u64",
        deserialize_with = "crate::wire::safe_u64"
    )]
    pub offset_end: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct Source {
    pub kind: Open<SourceKind>,
    pub agent: Option<Open<Agent>>,
    pub cli_version: Option<String>,
    /// Set only for `kind: transcript`.
    pub transcript: Option<TranscriptPos>,
}

/// One stored event. `kind` is an open set and `payload` stays raw until it is decoded
/// by kind.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
pub struct EventEnvelope {
    pub stream_id: StreamId,
    /// Per stream, gapless, from 1.
    #[serde(
        serialize_with = "crate::wire::ser_safe_u64",
        deserialize_with = "crate::wire::safe_u64"
    )]
    pub seq: u64,
    pub event_id: EventId,
    pub schema_version: u32,
    pub idempotency_key: IdempotencyKey,
    pub kind: EventKindTag,
    pub thread_id: Option<ThreadId>,
    pub launch_id: Option<LaunchId>,
    pub command_id: Option<CommandId>,
    pub source: Source,
    /// Source time, or `recorded_at` when the source states none.
    pub occurred_at: Timestamp,
    /// Append time.
    pub recorded_at: Timestamp,
    #[ts(type = "unknown")]
    pub payload: Box<RawValue>,
}

impl PartialEq for EventEnvelope {
    fn eq(&self, o: &Self) -> bool {
        self.stream_id == o.stream_id
            && self.seq == o.seq
            && self.event_id == o.event_id
            && self.schema_version == o.schema_version
            && self.idempotency_key == o.idempotency_key
            && self.kind == o.kind
            && self.thread_id == o.thread_id
            && self.launch_id == o.launch_id
            && self.command_id == o.command_id
            && self.source == o.source
            && self.occurred_at == o.occurred_at
            && self.recorded_at == o.recorded_at
            && same_json(&self.payload, &o.payload)
    }
}

/// Equal as JSON values, ignoring whitespace and key order.
fn same_json(a: &RawValue, b: &RawValue) -> bool {
    a.get() == b.get()
        || matches!(
            (
                serde_json::from_str::<serde_json::Value>(a.get()),
                serde_json::from_str::<serde_json::Value>(b.get()),
            ),
            (Ok(x), Ok(y)) if x == y
        )
}

/// Why an envelope did not decode into a known event kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GenericReason {
    UnknownKind,
    /// `schema_version` above the reader's.
    NewerSchema,
    /// A known kind whose payload fails its schema.
    PayloadInvalid,
}

/// An event this reader cannot decode, kept whole: shown as "unknown event" by the PWA
/// and printed as JSON by the CLI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, JsonSchema)]
pub struct GenericEvent {
    pub envelope: EventEnvelope,
    pub why: GenericReason,
}

macro_rules! event_kinds {
    ($($variant:ident($payload:ty) = $kind:literal,)*) => {
        /// The known event kinds with their payloads. On the wire this is the
        /// envelope's `kind` and `payload` pair.
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, JsonSchema)]
        #[serde(tag = "kind", content = "payload")]
        pub enum TypedEvent {
            $(
                #[serde(rename = $kind)]
                $variant($payload),
            )*
        }

        impl TypedEvent {
            /// Every known kind, in declaration order.
            pub const KINDS: &'static [&'static str] = &[$($kind),*];

            #[must_use]
            pub fn kind(&self) -> &'static str {
                match self {
                    $(TypedEvent::$variant(_) => $kind,)*
                }
            }

            /// Serializes the payload alone, for an envelope's `payload`.
            ///
            /// # Errors
            ///
            /// If a wire integer in the payload is above 2^53 - 1 (`MAX_SAFE_INTEGER`).
            pub fn to_payload(&self) -> Result<Box<RawValue>, serde_json::Error> {
                match self {
                    $(TypedEvent::$variant(p) => serde_json::value::to_raw_value(p),)*
                }
            }

            /// `None` for an unknown kind. A known kind's payload must be a JSON object:
            /// serde also reads a struct from a JSON array, which the schema refuses, so
            /// that form is rejected before typed decoding.
            fn decode_payload(kind: &str, raw: &RawValue) -> Option<Result<Self, serde_json::Error>> {
                match kind {
                    $($kind => Some(crate::wire::object::<_, $payload>(raw).map(TypedEvent::$variant)),)*
                    _ => None,
                }
            }
        }
    };
}

event_kinds! {
    ThreadRegistered(ThreadRegistered) = "thread_registered",
    ThreadClosed(ThreadClosed) = "thread_closed",
    LaunchReserved(LaunchReserved) = "launch_reserved",
    LaunchStarted(LaunchStarted) = "launch_started",
    LaunchExited(LaunchExited) = "launch_exited",
    LaunchDeathConfirmed(LaunchDeathConfirmed) = "launch_death_confirmed",
    SessionBound(SessionBound) = "session_bound",
    TranscriptGeneration(TranscriptGeneration) = "transcript_generation",
    ReadinessObserved(ReadinessObserved) = "readiness_observed",
    HumanWrite(HumanWrite) = "human_write",
    UserMessage(UserMessage) = "user_message",
    AssistantMessage(AssistantMessage) = "assistant_message",
    ToolActivity(ToolActivity) = "tool_activity",
    StatusChanged(StatusChanged) = "status_changed",
    ApprovalRequested(ApprovalRequested) = "approval_requested",
    ApprovalResolved(ApprovalResolved) = "approval_resolved",
    InputCreated(InputCreated) = "input_created",
    InputStateChanged(InputStateChanged) = "input_state_changed",
    MailboxMessage(MailboxMessage) = "mailbox_message",
    TaskSet(TaskSet) = "task_set",
    SurfaceUpdate(SurfaceUpdate) = "surface_update",
    VtSnapshotTaken(VtSnapshotTaken) = "vt_snapshot_taken",
    WorktreeChanged(WorktreeChanged) = "worktree_changed",
    RawUnknown(RawUnknown) = "raw_unknown",
    LaunchEvidence(LaunchEvidence) = "launch_evidence",
}

impl TypedEvent {
    /// Decodes an envelope's payload by its kind.
    ///
    /// # Errors
    ///
    /// A [`GenericEvent`] holding the whole envelope when the schema version is newer
    /// than this reader's, the kind is unknown, or the payload fails its schema.
    #[allow(
        clippy::result_large_err,
        reason = "the error is the whole envelope by design; it is returned, not propagated"
    )]
    pub fn decode(e: &EventEnvelope) -> Result<TypedEvent, GenericEvent> {
        let generic = |why| GenericEvent {
            envelope: e.clone(),
            why,
        };
        if e.schema_version > SCHEMA_VERSION {
            return Err(generic(GenericReason::NewerSchema));
        }
        match TypedEvent::decode_payload(e.kind.as_str(), &e.payload) {
            None => Err(generic(GenericReason::UnknownKind)),
            Some(Err(_)) => Err(generic(GenericReason::PayloadInvalid)),
            Some(Ok(t)) => Ok(t),
        }
    }
}
