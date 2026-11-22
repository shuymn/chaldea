//! Thread state: three axes and the attention set (`docs/mvp-design.md` §2.2).
//!
//! `reduce` is the only place an axis or the attention set changes. Its input type,
//! [`StateInput`], has no variant built from PTY output, so terminal text cannot drive
//! state (AGENTS.md invariant 1). Timers reach it as explicit inputs fed by the daemon.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::InvalidTransition;
use crate::ids::{Actor, InputId};
use crate::payload::ExitReason;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Starting,
    Running,
    Stopping,
    Exited,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    Unknown,
    Idle,
    Running,
    AwaitingApproval,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    None,
    Active,
    Blocked,
    Completed,
    Failed,
    Cancelled,
}

/// One reason a thread needs a human, each with its own clearing event (§2.2 "Attention").
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS, JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttentionCause {
    /// No `SessionStart` 20 s after `launch_started` of a non-manual launch.
    SessionStartTimeout,
    /// An expected Codex `Hooks need review` after a CLI upgrade.
    StartupStall,
    /// That input entered `unconfirmed`.
    InputUnconfirmed { input_id: InputId },
    /// That input stayed `queued` for 60 s.
    InputGateTimeout { input_id: InputId },
    /// A resume was refused because the old launch's death is not confirmed.
    ResumeBlocked,
}

impl AttentionCause {
    /// Whether only an explicit [`StateInput::AttentionCleared`] clears this cause (the
    /// input-bound causes, cleared when their input leaves the state that raised them).
    #[must_use]
    pub fn cleared_explicitly(self) -> bool {
        matches!(
            self,
            AttentionCause::InputUnconfirmed { .. } | AttentionCause::InputGateTimeout { .. }
        )
    }
}

/// The reducer input that caused a `status_changed` event.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum StatusCause {
    Reserved,
    Spawned,
    SessionBound,
    StopRequested,
    Exited,
    DeathConfirmed,
    ReadinessObserved,
    PromptSubmitted,
    Interrupted,
    TurnStopped,
    ApprovalOpened,
    ApprovalsAllResolved,
    AttentionRaised,
    AttentionCleared,
    TaskSet,
}

/// Payload of `status_changed`: one axis or the attention set changed.
/// The attention set is carried whole and sorted, `from` before and `to` after.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(tag = "field", rename_all = "snake_case")]
pub enum StatusChanged {
    ProcessState {
        from: ProcessState,
        to: ProcessState,
        cause: StatusCause,
    },
    TurnState {
        from: TurnState,
        to: TurnState,
        cause: StatusCause,
    },
    TaskState {
        from: TaskState,
        to: TaskState,
        cause: StatusCause,
    },
    Attention {
        from: Vec<AttentionCause>,
        to: Vec<AttentionCause>,
        cause: StatusCause,
    },
}

/// The projected state of one thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadState {
    pub process: ProcessState,
    pub turn: TurnState,
    pub task: TaskState,
    pub attention: BTreeSet<AttentionCause>,
}

impl ThreadState {
    /// The state `thread_registered` creates: no launch yet (`exited`, so the first
    /// reservation follows the same rule as a relaunch), turn `unknown`, and task
    /// `active` when the spawn gave a task, else `none`.
    #[must_use]
    pub fn registered(task: bool) -> Self {
        ThreadState {
            process: ProcessState::Exited,
            turn: TurnState::Unknown,
            task: if task {
                TaskState::Active
            } else {
                TaskState::None
            },
            attention: BTreeSet::new(),
        }
    }
}

/// Every explicit input of the reducer. No variant carries PTY output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateInput {
    /// `launch_reserved`; `relaunch` is false only for the thread's first launch.
    Reserved {
        relaunch: bool,
    },
    /// `launch_started`.
    Spawned,
    /// `session_bound`.
    SessionBound,
    /// A `stop` command was accepted.
    StopRequested,
    /// `launch_exited`, also for a spawn failure (`reason: spawn_failed`).
    Exited {
        reason: ExitReason,
    },
    /// `launch_death_confirmed`.
    DeathConfirmed,
    /// `readiness_observed` (from the adapter's `readiness_evidence` or `chaldea ready`).
    ReadinessObserved,
    /// A bound `UserPromptSubmit`.
    PromptSubmitted,
    /// Only from an explicit interrupt event (Codex `Interrupt` hook).
    Interrupted,
    /// `Stop`: the turn becomes `unknown`; only a later `ReadinessObserved` sets `idle`.
    TurnStopped,
    /// `approval_requested`.
    ApprovalOpened,
    /// The last open card resolved (`approval_resolved` with `ran`).
    ApprovalsAllResolved,
    AttentionRaised(AttentionCause),
    /// Clears an input-bound cause; other causes have their own clearing inputs.
    AttentionCleared(AttentionCause),
    TaskSet {
        to: TaskState,
        by: Actor,
    },
}

impl StateInput {
    #[must_use]
    pub fn cause(&self) -> StatusCause {
        match self {
            StateInput::Reserved { .. } => StatusCause::Reserved,
            StateInput::Spawned => StatusCause::Spawned,
            StateInput::SessionBound => StatusCause::SessionBound,
            StateInput::StopRequested => StatusCause::StopRequested,
            StateInput::Exited { .. } => StatusCause::Exited,
            StateInput::DeathConfirmed => StatusCause::DeathConfirmed,
            StateInput::ReadinessObserved => StatusCause::ReadinessObserved,
            StateInput::PromptSubmitted => StatusCause::PromptSubmitted,
            StateInput::Interrupted => StatusCause::Interrupted,
            StateInput::TurnStopped => StatusCause::TurnStopped,
            StateInput::ApprovalOpened => StatusCause::ApprovalOpened,
            StateInput::ApprovalsAllResolved => StatusCause::ApprovalsAllResolved,
            StateInput::AttentionRaised(_) => StatusCause::AttentionRaised,
            StateInput::AttentionCleared(_) => StatusCause::AttentionCleared,
            StateInput::TaskSet { .. } => StatusCause::TaskSet,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            StateInput::Reserved { .. } => "reserved",
            StateInput::Spawned => "spawned",
            StateInput::SessionBound => "session_bound",
            StateInput::StopRequested => "stop_requested",
            StateInput::Exited { .. } => "exited",
            StateInput::DeathConfirmed => "death_confirmed",
            StateInput::ReadinessObserved => "readiness_observed",
            StateInput::PromptSubmitted => "prompt_submitted",
            StateInput::Interrupted => "interrupted",
            StateInput::TurnStopped => "turn_stopped",
            StateInput::ApprovalOpened => "approval_opened",
            StateInput::ApprovalsAllResolved => "approvals_all_resolved",
            StateInput::AttentionRaised(_) => "attention_raised",
            StateInput::AttentionCleared(_) => "attention_cleared",
            StateInput::TaskSet { .. } => "task_set",
        }
    }
}

const STARTUP_CAUSES: [AttentionCause; 2] = [
    AttentionCause::SessionStartTimeout,
    AttentionCause::StartupStall,
];

/// Applies one input. Returns the new state and one `StatusChanged` per changed field
/// (none when nothing changed), or `InvalidTransition` when the input cannot apply to
/// this state; the state is then unchanged.
///
/// Hook-driven inputs require a live process (`running` or `stopping`): hooks of a
/// launch are released only after `launch_started` (§2.9).
///
/// `StopRequested` applies only to a `running` launch. The daemon holds a stop that
/// arrives while the launch is `starting` until the spawn outcome (`Spawned` or
/// `Exited { spawn_failed }`) is recorded, so `stopping` always means the child started
/// and a spawn failure is accepted only from `starting`.
///
/// # Errors
///
/// `InvalidTransition` names the input and the rule it broke.
pub fn reduce(
    s: &ThreadState,
    input: &StateInput,
) -> Result<(ThreadState, Vec<StatusChanged>), InvalidTransition> {
    use ProcessState as P;
    use TurnState as T;

    let invalid = |rule: &'static str| InvalidTransition {
        input: input.name(),
        rule,
    };
    let alive = matches!(s.process, P::Running | P::Stopping);
    let mut n = s.clone();

    match input {
        StateInput::Reserved { .. } => {
            if s.process != P::Exited {
                return Err(invalid(
                    "a launch is reserved only after the previous one exited",
                ));
            }
            if s.attention.contains(&AttentionCause::ResumeBlocked) {
                return Err(invalid(
                    "a resume is refused until the old launch's death is confirmed",
                ));
            }
            n.process = P::Starting;
            n.turn = T::Unknown;
        }
        StateInput::Spawned => {
            if s.process != P::Starting {
                return Err(invalid("spawned only from starting"));
            }
            n.process = P::Running;
        }
        StateInput::SessionBound => {
            if !alive {
                return Err(invalid("a session binds only to a live launch"));
            }
            for c in STARTUP_CAUSES {
                n.attention.remove(&c);
            }
        }
        StateInput::StopRequested => {
            if s.process != P::Running {
                return Err(invalid(
                    "a stop applies only after the spawn outcome is recorded",
                ));
            }
            n.process = P::Stopping;
        }
        StateInput::Exited { reason } => {
            if s.process == P::Exited {
                return Err(invalid("the launch already exited"));
            }
            if *reason == ExitReason::SpawnFailed && s.process != P::Starting {
                return Err(invalid("a spawn fails only before the launch started"));
            }
            n.process = P::Exited;
            n.turn = T::Unknown;
            for c in STARTUP_CAUSES {
                n.attention.remove(&c);
            }
        }
        StateInput::DeathConfirmed => {
            if s.process != P::Exited {
                return Err(invalid("death is confirmed only for an exited launch"));
            }
            n.attention.remove(&AttentionCause::ResumeBlocked);
        }
        StateInput::ReadinessObserved => {
            if !alive {
                return Err(invalid("readiness needs a live launch"));
            }
            // While a card is open the turn stays `awaiting_approval`, which keeps the
            // gate closed.
            if s.turn != T::AwaitingApproval {
                n.turn = T::Idle;
            }
        }
        StateInput::PromptSubmitted => {
            if !alive {
                return Err(invalid("a prompt is submitted only to a live launch"));
            }
            // Never dropped, even with a card open: the daemon supersedes the cards.
            n.turn = T::Running;
        }
        StateInput::Interrupted | StateInput::TurnStopped => {
            if !alive {
                return Err(invalid("a turn ends only in a live launch"));
            }
            n.turn = T::Unknown;
        }
        StateInput::ApprovalOpened => {
            if !alive {
                return Err(invalid("a card opens only in a live launch"));
            }
            n.turn = T::AwaitingApproval;
        }
        StateInput::ApprovalsAllResolved => {
            // A `Stop`, interrupt, or exit already superseded the cards: a late
            // resolution leaves the turn as those set it.
            if s.turn == T::AwaitingApproval {
                n.turn = T::Running;
            }
        }
        StateInput::AttentionRaised(c) => {
            n.attention.insert(*c);
        }
        StateInput::AttentionCleared(c) => {
            if !c.cleared_explicitly() {
                return Err(invalid("this cause is cleared only by its own event"));
            }
            n.attention.remove(c);
        }
        StateInput::TaskSet { to, .. } => {
            n.task = *to;
        }
    }

    let changes = diff(s, &n, input.cause());
    Ok((n, changes))
}

fn diff(old: &ThreadState, new: &ThreadState, cause: StatusCause) -> Vec<StatusChanged> {
    let mut out = Vec::new();
    if old.process != new.process {
        out.push(StatusChanged::ProcessState {
            from: old.process,
            to: new.process,
            cause,
        });
    }
    if old.turn != new.turn {
        out.push(StatusChanged::TurnState {
            from: old.turn,
            to: new.turn,
            cause,
        });
    }
    if old.task != new.task {
        out.push(StatusChanged::TaskState {
            from: old.task,
            to: new.task,
            cause,
        });
    }
    if old.attention != new.attention {
        out.push(StatusChanged::Attention {
            from: old.attention.iter().copied().collect(),
            to: new.attention.iter().copied().collect(),
            cause,
        });
    }
    out
}
