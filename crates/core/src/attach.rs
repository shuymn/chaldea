//! The frames of `GET /api/attach/{thread}` (`docs/design/t1-attach.md` §2): the web
//! terminal view's socket to one launch. Binary frames carry raw PTY output (server to
//! client) and deliberate raw input (client to server); these are the text frames.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::ids::{LaunchId, ThreadId};
use crate::input::RejectReason;
use crate::payload::VtCursor;

/// The resize frame's column range.
pub const MIN_COLS: u16 = 2;
pub const MAX_COLS: u16 = 512;
/// The resize frame's row range.
pub const MIN_ROWS: u16 = 1;
pub const MAX_ROWS: u16 = 256;

/// The terminal modes `attached` carries, for the client's emulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
pub struct AttachModes {
    /// DECSET 2004.
    pub bracketed_paste: bool,
    /// DECSET 1049.
    pub alt_screen: bool,
    /// DECTCEM.
    pub show_cursor: bool,
    /// DECCKM.
    pub app_cursor: bool,
    /// DECKPAM.
    pub app_keypad: bool,
    /// The kitty keyboard flags, 5 bits.
    pub kitty_flags: u8,
}

/// A paste's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PasteOutcome {
    /// Every byte was written.
    Written,
    /// Some bytes were written, then the write failed (`bytes_written` says how many).
    Incomplete,
    /// Whether it was written is not known yet; the socket submits nothing more until it is.
    Pending,
    /// Zero bytes written.
    NotWritten,
    /// Refused before it was queued, with zero bytes written.
    Rejected,
}

/// Why a paste or raw input was not written whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttachReason {
    /// Paste text above `core::input::MAX_PROMPT_BYTES`.
    PromptTooLarge,
    /// The adapter's paste needs DECSET 2004, off at encoding or changed by dispatch.
    BracketedPasteOff,
    /// Not dispatched by the unit's deadline.
    Expired,
    /// The session's queue is full.
    QueueFull,
    /// The launch's op lock was not taken in time.
    Busy,
    /// Written in part.
    Incomplete,
    /// Not written, for any other reason.
    NotWritten,
    /// Raw input that would carry a complete bracketed-paste marker.
    PasteMarker,
    /// A raw binary message above 64 KiB.
    TooLarge,
    /// Raw input whose write is not known yet.
    Pending,
}

/// A paste's reason: an encoder refusal or one of the attach reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(untagged)]
pub enum PasteReason {
    Rejected(RejectReason),
    Attach(AttachReason),
}

/// Why a launch's output ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// The PTY's output reached EOF.
    Eof,
    /// Reading the PTY's output failed (the child may still run).
    Error,
    /// The launch's session left the daemon.
    Gone,
}

/// Server to client on `GET /api/attach/{thread}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachServerMessage {
    /// The first frame: the screen at subscription; binary output follows gapless.
    Attached {
        thread_id: ThreadId,
        launch_id: LaunchId,
        cols: u16,
        rows: u16,
        cursor: VtCursor,
        modes: AttachModes,
        /// The rows of cell strings, trailing blanks omitted (`docs/design/pty-runtime.md`
        /// §2.6); `null` when the whole frame would exceed its bound.
        grid: Option<Vec<Vec<String>>>,
    },
    /// The PTY's size after a resize attempt; `ok: false` when it failed.
    Size { cols: u16, rows: u16, ok: bool },
    PasteResult {
        paste_id: String,
        outcome: PasteOutcome,
        /// `null` on `pending`.
        #[serde(
            serialize_with = "crate::wire::ser_opt_safe_u64",
            deserialize_with = "crate::wire::opt_safe_u64"
        )]
        bytes_written: Option<u64>,
        /// `null` on `written` and `pending`.
        reason: Option<PasteReason>,
    },
    /// Raw input not written: its bytes (all of the unit's on `pending`).
    InputDropped {
        #[serde(
            serialize_with = "crate::wire::ser_safe_u64",
            deserialize_with = "crate::wire::safe_u64"
        )]
        bytes: u64,
        reason: AttachReason,
    },
    /// The last frame before close 4001.
    Ended {
        launch_id: LaunchId,
        reason: EndReason,
    },
}

/// Client to server on `GET /api/attach/{thread}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AttachClientMessage {
    /// `cols` 2–512, `rows` 1–256.
    Resize { cols: u16, rows: u16 },
    /// Clipboard text; `paste_id` is the socket's own and never replayed.
    Paste { paste_id: String, text: String },
}

impl AttachClientMessage {
    /// The frame in `text`, or `None` when it is not a client message or its size is out
    /// of range.
    #[must_use]
    pub fn parse(text: &str) -> Option<AttachClientMessage> {
        let m: AttachClientMessage = serde_json::from_str(text).ok()?;
        match m {
            AttachClientMessage::Resize { cols, rows }
                if !(MIN_COLS..=MAX_COLS).contains(&cols)
                    || !(MIN_ROWS..=MAX_ROWS).contains(&rows) =>
            {
                None
            }
            m => Some(m),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let msgs = [
            AttachServerMessage::Attached {
                thread_id: ThreadId::from_u128(1),
                launch_id: LaunchId::from_u128(2),
                cols: 80,
                rows: 2,
                cursor: VtCursor {
                    row: 0,
                    col: 1,
                    visible: true,
                },
                modes: AttachModes {
                    bracketed_paste: true,
                    alt_screen: false,
                    show_cursor: true,
                    app_cursor: false,
                    app_keypad: false,
                    kitty_flags: 0,
                },
                grid: Some(vec![vec!["a".into()], vec![]]),
            },
            AttachServerMessage::PasteResult {
                paste_id: "p1".into(),
                outcome: PasteOutcome::Rejected,
                bytes_written: Some(0),
                reason: Some(PasteReason::Rejected(RejectReason::EmptyPrompt)),
            },
            AttachServerMessage::PasteResult {
                paste_id: "p2".into(),
                outcome: PasteOutcome::Pending,
                bytes_written: None,
                reason: None,
            },
            AttachServerMessage::InputDropped {
                bytes: 3,
                reason: AttachReason::PasteMarker,
            },
            AttachServerMessage::Ended {
                launch_id: LaunchId::from_u128(2),
                reason: EndReason::Gone,
            },
        ];
        for m in msgs {
            let t = serde_json::to_string(&m).unwrap();
            assert_eq!(serde_json::from_str::<AttachServerMessage>(&t).unwrap(), m);
        }
        let t = serde_json::to_string(&AttachServerMessage::PasteResult {
            paste_id: "p".into(),
            outcome: PasteOutcome::NotWritten,
            bytes_written: Some(0),
            reason: Some(PasteReason::Attach(AttachReason::BracketedPasteOff)),
        })
        .unwrap();
        assert!(t.contains(r#""reason":"bracketed_paste_off""#), "{t}");
    }

    #[test]
    fn client_frames_are_checked() {
        assert_eq!(
            AttachClientMessage::parse(r#"{"type":"resize","cols":2,"rows":1}"#),
            Some(AttachClientMessage::Resize { cols: 2, rows: 1 })
        );
        assert!(AttachClientMessage::parse(r#"{"type":"resize","cols":512,"rows":256}"#).is_some());
        for bad in [
            r#"{"type":"resize","cols":1,"rows":10}"#,
            r#"{"type":"resize","cols":10,"rows":0}"#,
            r#"{"type":"resize","cols":513,"rows":10}"#,
            r#"{"type":"resize","cols":10,"rows":257}"#,
            r#"{"type":"resize","cols":10,"rows":10,"x":1}"#,
            r#"{"type":"paste","text":"x"}"#,
            r#"{"type":"subscribe","stream_ids":[]}"#,
            "[]",
        ] {
            assert_eq!(AttachClientMessage::parse(bad), None, "{bad}");
        }
    }
}
