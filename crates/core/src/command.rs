//! Command receipts keyed by `command_id` (`docs/mvp-design.md` §2.4 "Commands").
//!
//! Internal: the store keeps receipts, HTTP maps a [`ReceiptLookup`] to its reply.

use crate::ids::{CommandId, TokenId};

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
