//! The input state machine and the input gate (`docs/mvp-design.md` §2.3).
//!
//! No transition resends an input: nothing leads from `sending`, `sent`, or `unconfirmed`
//! back to `queued` or `sending` (AGENTS.md invariant 3, never auto-resend).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::InvalidTransition;
use crate::ids::LaunchId;
use crate::payload::ReadinessEvidence;
use crate::state::{ProcessState, TurnState};

pub mod encode;
pub mod key;

/// The largest prompt text, in UTF-8 bytes, that `send` and recognised attach pastes
/// admit (§2.3 "Prompt limit"). Longer text is refused before admission, with zero bytes
/// written and no input created. The PTY writer's internal limits derive from it.
pub const MAX_PROMPT_BYTES: usize = 256 * 1024;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum InputState {
    Queued,
    Sending,
    Sent,
    Acked,
    /// May have reached the CLI; never resent.
    Unconfirmed,
    Rejected,
    Cancelled,
    Dismissed,
}

impl InputState {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            InputState::Acked
                | InputState::Rejected
                | InputState::Cancelled
                | InputState::Dismissed
        )
    }

    /// `sending`, `sent`, and `unconfirmed`: the input may still reach the CLI.
    #[must_use]
    pub fn is_in_flight(self) -> bool {
        matches!(
            self,
            InputState::Sending | InputState::Sent | InputState::Unconfirmed
        )
    }
}

/// Whether one input keeps the gate closed for `current_launch`: only an in-flight input
/// of that launch does; inputs of earlier launches never block (§2.3). Feeds
/// [`GateView::in_flight`].
#[must_use]
pub fn blocks_gate(
    input_launch: Option<LaunchId>,
    state: InputState,
    current_launch: LaunchId,
) -> bool {
    input_launch == Some(current_launch) && state.is_in_flight()
}

/// Why an input was rejected with zero bytes written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// ESC or a C0 control other than TAB, CR, LF.
    ControlCharacter,
    /// Text the adapter's encoding cannot carry without loss, or that the CLI would
    /// reinterpret as a command instead of a prompt (Claude, C1 D16).
    LossyText,
    EmptyPrompt,
    /// Codex on Windows trims it (§8 D6).
    LeadingTrailingWhitespace,
    /// Codex on Windows takes TAB as its queue key (§8 D6).
    Tab,
    /// The spawn of a positional first prompt was confirmed not to have started.
    SpawnFailed,
}

/// One explicit input of [`step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputInput {
    /// The gate opened and the attempt, with the stream head, was persisted.
    GateOpened {
        sent_after_seq: u64,
    },
    /// Text and submit were written (or, for a positional prompt, `launch_started`).
    WriteCompleted,
    WriteOutcomeUnknown,
    Rejected(RejectReason),
    /// `chaldea input cancel`.
    Cancelled,
    /// The unit wrote zero bytes: the writer refused it before the first byte (its fence,
    /// basis, or recheck failed, its deadline passed, or the session closed), or its
    /// submission was refused.
    RecheckFailed,
    LaunchExited,
    DaemonRestarted,
    AckAttributed,
    AckNotAttributable,
    AckWindowExpired,
    /// `chaldea input resolve --delivered | --dismiss`.
    Resolved {
        delivered: bool,
    },
}

/// The `reason` of `input_state_changed`: the state-machine input that caused it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputReason {
    GateOpened,
    WriteCompleted,
    WriteOutcomeUnknown,
    Rejected { reject: RejectReason },
    Cancelled,
    RecheckFailed,
    LaunchExited,
    DaemonRestarted,
    AckAttributed,
    AckNotAttributable,
    AckWindowExpired,
    Resolved,
}

impl From<&InputInput> for InputReason {
    fn from(i: &InputInput) -> Self {
        match *i {
            InputInput::GateOpened { .. } => InputReason::GateOpened,
            InputInput::WriteCompleted => InputReason::WriteCompleted,
            InputInput::WriteOutcomeUnknown => InputReason::WriteOutcomeUnknown,
            InputInput::Rejected(reject) => InputReason::Rejected { reject },
            InputInput::Cancelled => InputReason::Cancelled,
            InputInput::RecheckFailed => InputReason::RecheckFailed,
            InputInput::LaunchExited => InputReason::LaunchExited,
            InputInput::DaemonRestarted => InputReason::DaemonRestarted,
            InputInput::AckAttributed => InputReason::AckAttributed,
            InputInput::AckNotAttributable => InputReason::AckNotAttributable,
            InputInput::AckWindowExpired => InputReason::AckWindowExpired,
            InputInput::Resolved { .. } => InputReason::Resolved,
        }
    }
}

fn input_name(i: &InputInput) -> &'static str {
    match i {
        InputInput::GateOpened { .. } => "gate_opened",
        InputInput::WriteCompleted => "write_completed",
        InputInput::WriteOutcomeUnknown => "write_outcome_unknown",
        InputInput::Rejected(_) => "rejected",
        InputInput::Cancelled => "cancelled",
        InputInput::RecheckFailed => "recheck_failed",
        InputInput::LaunchExited => "launch_exited",
        InputInput::DaemonRestarted => "daemon_restarted",
        InputInput::AckAttributed => "ack_attributed",
        InputInput::AckNotAttributable => "ack_not_attributable",
        InputInput::AckWindowExpired => "ack_window_expired",
        InputInput::Resolved { .. } => "resolved",
    }
}

/// Applies one input to an input's state.
///
/// An attributable ack of a `sending` input whose unit was written is fed as
/// `WriteCompleted` then `AckAttributed` in the hook's own job (send §2.5).
///
/// # Errors
///
/// `InvalidTransition` when the input does not apply to `state`; terminal states accept
/// nothing.
pub fn step(state: InputState, input: &InputInput) -> Result<InputState, InvalidTransition> {
    use InputInput as I;
    use InputState as S;

    let next = match (state, *input) {
        (S::Queued, I::GateOpened { .. }) => S::Sending,
        (S::Queued, I::Rejected(r)) if r != RejectReason::SpawnFailed => S::Rejected,
        (S::Queued, I::Cancelled | I::LaunchExited | I::DaemonRestarted) => S::Cancelled,

        (S::Sending, I::WriteCompleted) => S::Sent,
        (S::Sending, I::RecheckFailed) => S::Cancelled,
        (S::Sending, I::Rejected(RejectReason::SpawnFailed)) => S::Rejected,
        (S::Sending, I::WriteOutcomeUnknown | I::LaunchExited | I::DaemonRestarted) => {
            S::Unconfirmed
        }

        (S::Sent | S::Unconfirmed, I::AckAttributed) => S::Acked,
        (
            S::Sent,
            I::AckNotAttributable | I::AckWindowExpired | I::LaunchExited | I::DaemonRestarted,
        ) => S::Unconfirmed,

        (S::Unconfirmed, I::Resolved { delivered: true }) => S::Acked,
        (S::Unconfirmed, I::Resolved { delivered: false }) => S::Dismissed,

        _ => {
            return Err(InvalidTransition {
                input: input_name(input),
                rule: "no such transition from this input state",
            });
        }
    };
    Ok(next)
}

/// The latest readiness evidence of the current launch, with its stream position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadinessMark {
    pub seq: u64,
    pub evidence: ReadinessEvidence,
}

/// Everything the gate reads, projected from the current launch's events (§2.1
/// "Projections", gate evidence). Positions are thread-stream `seq` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GateView {
    /// The current launch is bound to a session.
    pub bound: bool,
    pub process: ProcessState,
    pub turn: TurnState,
    pub readiness: Option<ReadinessMark>,
    /// The last write of any origin: an input's write or any human write, `card_key`
    /// included. Evidence must be newer than it.
    pub last_write: Option<u64>,
    /// The last human write that closes the gate (`attach` or `key`; never `card_key`).
    /// Evidence after it reopens the gate only from a later turn or `chaldea ready`.
    pub last_human_write: Option<u64>,
    /// The last `UserPromptSubmit` or `SessionStart` of the launch.
    pub last_turn_start: Option<u64>,
    /// Another input of the current launch is `sending`, `sent`, or `unconfirmed`.
    pub in_flight: bool,
    /// The adapter's encoding for this input is bracketed paste.
    pub needs_bracketed_paste: bool,
    /// DECSET 2004 is set in the VT.
    pub bracketed_paste_on: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GateClosedReason {
    NotBound,
    ProcessNotRunning,
    NoEvidence,
    /// A write happened after the latest evidence.
    WriteAfterEvidence,
    /// The evidence is of the same turn as the last gate-closing human write, so it does
    /// not reopen the gate.
    EvidenceOfSameTurn,
    TurnNotIdle,
    InputInFlight,
    BracketedPasteOff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GateDecision {
    Open,
    Closed(GateClosedReason),
}

/// The one conservative gate rule: open only with positive readiness evidence newer than
/// the last write of any origin and every other condition met. Unknown means closed.
/// The writer does not evaluate it again: before the first byte it checks, under its lock,
/// the open's fence word and write basis (send §2.3).
#[must_use]
pub fn gate(g: &GateView) -> GateDecision {
    use GateClosedReason as R;
    let closed = GateDecision::Closed;

    if !g.bound {
        return closed(R::NotBound);
    }
    if g.process != ProcessState::Running {
        return closed(R::ProcessNotRunning);
    }
    let Some(ev) = g.readiness else {
        return closed(R::NoEvidence);
    };
    if g.last_write.is_some_and(|w| ev.seq <= w) {
        return closed(R::WriteAfterEvidence);
    }
    if let Some(h) = g.last_human_write {
        // Evidence after a human write reopens the gate only when it comes from a later
        // turn (a turn start between the write and the evidence) or from the owner.
        let later_turn = g.last_turn_start.is_some_and(|t| h < t && t < ev.seq);
        if ev.evidence != ReadinessEvidence::OwnerAsserted && !later_turn {
            return closed(R::EvidenceOfSameTurn);
        }
    }
    if g.turn != TurnState::Idle {
        return closed(R::TurnNotIdle);
    }
    if g.in_flight {
        return closed(R::InputInFlight);
    }
    if g.needs_bracketed_paste && !g.bracketed_paste_on {
        return closed(R::BracketedPasteOff);
    }
    GateDecision::Open
}
