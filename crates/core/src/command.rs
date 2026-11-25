//! Command receipts keyed by `command_id` (`docs/mvp-design.md` §2.4 "Commands").
//!
//! Internal: the store keeps receipts, HTTP maps a [`ReceiptLookup`] to its reply.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ts_rs::TS;

use crate::canonical::CanonicalJson;
use crate::ids::{Agent, ApprovalId, CommandId, InputId, LaunchId, ThreadId, TokenId};
use crate::input::key::KeyToken;
use crate::input::{InputState, RejectReason};
use crate::payload::InputOrigin;

/// The domain of [`request_identity`]: a new identity scheme gets a new one.
pub const IDENTITY_DOMAIN: &str = "chaldea-command-v1";

/// The identity of a command request (`docs/design/http-surface.md` §2.8 "Request
/// identity"): `SHA-256(IDENTITY_DOMAIN ‖ 0x00 ‖ name ‖ 0x00 ‖ normalized)`, where
/// `normalized` is the canonical form of the handler's parsed arguments. Two requests that
/// mean the same arguments share it whatever their key order, whitespace, or number
/// spelling; `command_id` is the receipt key and not part of it.
#[must_use]
pub fn request_identity(name: &str, normalized: &CanonicalJson) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(IDENTITY_DOMAIN.as_bytes());
    h.update([0]);
    h.update(name.as_bytes());
    h.update([0]);
    h.update(normalized.as_str().as_bytes());
    h.finalize().into()
}

/// The stored reply of a finished command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub http_status: u16,
    /// The JSON body as sent.
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptStatus {
    /// Inserted with the command's intent; a concurrent duplicate fails its insert.
    InProgress,
    Completed(CommandResult),
    /// Was `in_progress` at a daemon restart; never re-executed.
    UnknownAfterCrash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandReceipt {
    pub command_id: CommandId,
    /// The token that created it; a receipt is visible only to that token.
    pub token: TokenId,
    /// SHA-256 of the canonical request.
    pub request_sha256: [u8; 32],
    pub status: ReceiptStatus,
}

/// What to do with a request whose `command_id` may already have a receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptLookup {
    /// No receipt: the command runs.
    New,
    /// 409 `{ code: "in_progress", retry_after }`.
    InProgress,
    /// Same token and request: the stored status and body.
    Replay(CommandResult),
    /// Another token or another request: 409 `{ code: "conflict" }`, revealing nothing
    /// about the receipt.
    Conflict,
    /// 409 `{ code: "unknown_after_crash" }`.
    UnknownAfterCrash,
}

/// Classifies a request against the existing receipt for its `command_id`. Ownership and
/// the request hash are checked before the receipt's status, so another token learns
/// nothing, not even whether the command is still running.
#[must_use]
pub fn check_receipt(
    existing: Option<&CommandReceipt>,
    token: TokenId,
    request_sha256: &[u8; 32],
) -> ReceiptLookup {
    let Some(r) = existing else {
        return ReceiptLookup::New;
    };
    if r.token != token || &r.request_sha256 != request_sha256 {
        return ReceiptLookup::Conflict;
    }
    match &r.status {
        ReceiptStatus::InProgress => ReceiptLookup::InProgress,
        ReceiptStatus::Completed(res) => ReceiptLookup::Replay(res.clone()),
        ReceiptStatus::UnknownAfterCrash => ReceiptLookup::UnknownAfterCrash,
    }
}

// ---- launch commands (`docs/design/supervisor.md` §2.2) ----------------------------------------

/// The arguments of `spawn`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpawnArgs {
    pub agent: Agent,
    pub cwd: String,
    /// A Codex positional first prompt; a Claude one is refused (`bad_args`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub prompt: Option<String>,
    /// The task given at spawn (`thread_registered.task`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub task: Option<String>,
}

/// The arguments of `resume`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResumeArgs {
    pub thread_id: ThreadId,
    /// The launch the client read as current; another current launch is `launch_changed`.
    pub expected_launch_id: LaunchId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub prompt: Option<String>,
}

/// The arguments of `stop`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StopArgs {
    pub thread_id: ThreadId,
    /// When given, the current launch must be this one (`launch_changed` otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub expected_launch_id: Option<LaunchId>,
}

/// The arguments of `close`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloseArgs {
    pub thread_id: ThreadId,
    pub expected_launch_id: LaunchId,
    /// Stop a live current launch first; absent means false.
    #[serde(default)]
    pub stop: bool,
}

/// Whether a spawn or resume started its process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LaunchOutcome {
    Started,
    SpawnFailed,
}

/// The success body of `spawn`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct SpawnResult {
    pub thread_id: ThreadId,
    pub launch_id: LaunchId,
    /// The positional first prompt's input; `null` without one.
    pub input_id: Option<InputId>,
    pub outcome: LaunchOutcome,
}

/// The success body of `resume`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct ResumeResult {
    pub launch_id: LaunchId,
    pub input_id: Option<InputId>,
    pub outcome: LaunchOutcome,
}

/// How a stop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StopOutcome {
    /// This stop sealed and killed the launch.
    Stopped,
    /// The launch had already exited with its death confirmed.
    AlreadyExited,
}

/// The success body of `stop` (`docs/design/supervisor.md` §2.2). Every field is always
/// present, `exit_code` and `signal` as `null` when unknown, and each is in the JSON
/// Schema's `required` list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[schemars(extend("required" = [
    "launch_id",
    "outcome",
    "death_confirmed",
    "confirmed_within_deadline",
    "exit_code",
    "signal"
]))]
pub struct StopResult {
    pub launch_id: LaunchId,
    pub outcome: StopOutcome,
    /// The launch's process tree was proven dead.
    pub death_confirmed: bool,
    /// Death was confirmed and proven at or before the request's start + 5 s.
    pub confirmed_within_deadline: bool,
    /// The leader's exit code; `null` when the waiter had not reported an exit, and for
    /// `already_exited` (the launch's own `launch_exited` carries it).
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

/// The success body of `close`: `stop` is the result of the stop a `close --stop` ran, and
/// `null` when none ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[schemars(extend("required" = ["thread_id", "stop"]))]
pub struct CloseResult {
    pub thread_id: ThreadId,
    pub stop: Option<StopResult>,
}

// ---- send-side commands (`docs/design/supervisor-send.md` §2.2) -------------------------------

/// The arguments of `send`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendArgs {
    pub thread_id: ThreadId,
    pub text: String,
    /// The client's claim of where the text came from; `cli` when absent. Informational.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub origin: Option<InputOrigin>,
}

/// The success body of `send`: the input, `queued` or `rejected` with its reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[schemars(extend("required" = ["input_id", "state", "reject_reason"]))]
pub struct SendResult {
    pub input_id: InputId,
    pub state: InputState,
    /// `null` unless `state` is `rejected`.
    pub reject_reason: Option<RejectReason>,
}

/// The arguments of `ready`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadyArgs {
    pub thread_id: ThreadId,
    /// When given, the current launch must be this one (`launch_changed` otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub expected_launch_id: Option<LaunchId>,
}

/// The success body of `ready`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct ReadyResult {
    pub launch_id: LaunchId,
}

/// The arguments of `key`: 1 to `MAX_KEYS` tokens; with `approval_id`, every token before
/// the last is a navigation key of the row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeyArgs {
    pub thread_id: ThreadId,
    pub launch_id: LaunchId,
    pub keys: Vec<KeyToken>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub approval_id: Option<ApprovalId>,
}

/// What a `key` request knew of its unit when it replied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KeyOutcome {
    /// Every byte was written and the `human_write` committed.
    Written,
    /// Some bytes may have reached the CLI; the `human_write` committed.
    Incomplete,
    /// No recorded outcome was known at the reply; the daemon records it later.
    Pending,
}

/// The success body of `key`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[schemars(extend("required" = ["outcome", "bytes_written"]))]
pub struct KeyResult {
    pub outcome: KeyOutcome,
    /// `null` for `pending`.
    #[serde(
        serialize_with = "crate::wire::ser_opt_safe_u64",
        deserialize_with = "crate::wire::opt_safe_u64"
    )]
    pub bytes_written: Option<u64>,
}

/// The arguments of `input_resolve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputResolveArgs {
    pub input_id: InputId,
    /// `true`: the owner saw it delivered (`acked`); `false`: `dismissed`.
    pub delivered: bool,
}

/// The arguments of `input_cancel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputCancelArgs {
    pub input_id: InputId,
}

/// The success body of `input_resolve` and `input_cancel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct InputCommandResult {
    pub input_id: InputId,
    pub state: InputState,
}
