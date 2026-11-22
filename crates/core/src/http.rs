//! Public HTTP bodies (`docs/mvp-design.md` §2.5 "Bodies", `docs/design/http-surface.md`
//! §8 D13): the command envelope, the error body every daemon-decided rejection carries,
//! and the ticket, pairing, and list bodies.
//!
//! Types that carry a secret or command arguments print them redacted in `Debug`.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use ts_rs::TS;

use crate::ids::{CommandId, Epoch};
use crate::payload::WorktreeChanged;
use crate::sync::ThreadInfo;

/// The body of `POST /api/commands/{name}`. `args` is kept as raw JSON so the daemon can
/// refuse duplicate members before any conversion; it must be an object.
#[derive(Serialize, Deserialize, TS, JsonSchema)]
pub struct CommandRequest {
    pub command_id: CommandId,
    #[ts(type = "Record<string, unknown>")]
    #[schemars(with = "serde_json::Map<String, serde_json::Value>")]
    pub args: Box<RawValue>,
}

impl fmt::Debug for CommandRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandRequest")
            .field("command_id", &self.command_id)
            .field("args", &"<redacted>")
            .finish()
    }
}

/// The code of an [`ErrorBody`] (snake_case on the wire). Command PRs add their own refusal
/// codes here.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// 421: the `Host` header or the absolute-form authority is not the daemon's.
    MisdirectedRequest,
    /// 403: the `Origin` header breaks the route's rule.
    OriginRejected,
    /// 401.
    Unauthorized,
    /// 403: the token's kind or grant does not allow the action or object.
    Forbidden,
    /// 404: no such route, command, or object, or one the token cannot read.
    NotFound,
    /// 405, with `Allow`.
    MethodNotAllowed,
    /// 408.
    RequestTimeout,
    /// 411.
    LengthRequired,
    /// 413.
    PayloadTooLarge,
    /// 400: framing, a handshake, or a body that is not the route's.
    BadRequest,
    /// 400: the query string.
    BadQuery,
    /// 400: the command arguments.
    BadArgs,
    /// 409: the command's receipt is still `in_progress`.
    InProgress,
    /// 409: the `command_id` belongs to another token or another request.
    Conflict,
    /// 409: the command's receipt was `in_progress` when the daemon restarted.
    UnknownAfterCrash,
    /// 501: the route's or command's handler has not been implemented yet.
    NotImplemented,
    /// 503: a bounded resource is taken; retry after `retry_after`.
    Busy,
    /// 503.
    DiskFull,
    /// 503: the daemon is stopping.
    ShuttingDown,
    /// 503: the store refused writes after a fatal error.
    StoreFailed,
    /// 503: the write's outcome is not known; retry with the same `command_id`.
    OutcomeUnknown,
    /// 500.
    Internal,
}

/// The body of every rejection the daemon decides. `retry_after` is whole seconds and
/// absent (never `null`) when not given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct ErrorBody {
    pub code: ErrorCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub retry_after: Option<u32>,
}

/// The body of `POST /api/ws-ticket`: `events` or `attach:<thread_id>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WsTicketRequest {
    pub purpose: String,
}

/// A single-use WebSocket ticket, valid `expires_in` seconds.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct WsTicketResponse {
    pub ticket: String,
    pub expires_in: u32,
}

impl fmt::Debug for WsTicketResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsTicketResponse")
            .field("ticket", &"<redacted>")
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// A single-use pair code from `POST /api/pair-codes`, valid `expires_in` seconds.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct PairCodeResponse {
    pub code: String,
    pub expires_in: u32,
}

impl fmt::Debug for PairCodeResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairCodeResponse")
            .field("code", &"<redacted>")
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// The body of `POST /api/pair`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PairRequest {
    pub code: String,
}

impl fmt::Debug for PairRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairRequest")
            .field("code", &"<redacted>")
            .finish()
    }
}

/// The `owner` token `POST /api/pair` issues.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct PairResponse {
    pub token: String,
}

impl fmt::Debug for PairResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairResponse")
            .field("token", &"<redacted>")
            .finish()
    }
}

/// The body of `GET /api/threads`: the threads the token can read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct ThreadsResponse {
    pub epoch: Epoch,
    pub threads: Vec<ThreadInfo>,
}

/// The body of `GET /api/worktrees`: the worktrees the token can read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct WorktreesResponse {
    pub epoch: Epoch,
    pub worktrees: Vec<WorktreeChanged>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_body_omits_absent_retry_after() {
        let b = ErrorBody {
            code: ErrorCode::NotFound,
            retry_after: None,
        };
        assert_eq!(
            serde_json::to_string(&b).unwrap(),
            r#"{"code":"not_found"}"#
        );
        let b = ErrorBody {
            code: ErrorCode::Busy,
            retry_after: Some(1),
        };
        assert_eq!(
            serde_json::to_string(&b).unwrap(),
            r#"{"code":"busy","retry_after":1}"#
        );
        let back: ErrorBody = serde_json::from_str(r#"{"code":"outcome_unknown"}"#).unwrap();
        assert_eq!(back.retry_after, None);
    }

    #[test]
    fn secrets_are_redacted_in_debug() {
        let marker = "chd_secret_marker";
        let dbg = [
            format!("{:?}", WsTicketResponse { ticket: marker.into(), expires_in: 30 }),
            format!("{:?}", PairCodeResponse { code: marker.into(), expires_in: 60 }),
            format!("{:?}", PairRequest { code: marker.into() }),
            format!("{:?}", PairResponse { token: marker.into() }),
            format!(
                "{:?}",
                serde_json::from_str::<CommandRequest>(&format!(
                    r#"{{"command_id":"00000000-0000-0000-0000-000000000001","args":{{"text":"{marker}"}}}}"#
                ))
                .unwrap()
            ),
        ];
        for d in dbg {
            assert!(!d.contains(marker), "{d}");
        }
    }
}
