//! `reduce` properties (`docs/mvp-design.md` §5 "Tests", CL-2, CL-3).

use std::collections::BTreeSet;

use chaldea_core::ids::{Actor, InputId, ThreadId};
use chaldea_core::payload::ExitReason;
use chaldea_core::state::{
    AttentionCause, ProcessState, StateInput, StatusChanged, TaskState, ThreadState, TurnState,
    reduce,
};
use proptest::prelude::*;

fn process() -> impl Strategy<Value = ProcessState> {
    prop_oneof![
        Just(ProcessState::Starting),
        Just(ProcessState::Running),
        Just(ProcessState::Stopping),
        Just(ProcessState::Exited),
    ]
}

fn turn() -> impl Strategy<Value = TurnState> {
    prop_oneof![
        Just(TurnState::Unknown),
        Just(TurnState::Idle),
        Just(TurnState::Running),
        Just(TurnState::AwaitingApproval),
    ]
}

fn task() -> impl Strategy<Value = TaskState> {
    prop_oneof![
        Just(TaskState::None),
        Just(TaskState::Active),
        Just(TaskState::Blocked),
        Just(TaskState::Completed),
        Just(TaskState::Failed),
        Just(TaskState::Cancelled),
    ]
}

fn cause() -> impl Strategy<Value = AttentionCause> {
    // Two input ids, so that a cause of one input never clears the other's.
    let id = prop_oneof![Just(InputId::from_u128(1)), Just(InputId::from_u128(2))];
    prop_oneof![
        Just(AttentionCause::SessionStartTimeout),
        Just(AttentionCause::StartupStall),
        id.clone()
            .prop_map(|input_id| AttentionCause::InputUnconfirmed { input_id }),
        id.prop_map(|input_id| AttentionCause::InputGateTimeout { input_id }),
        Just(AttentionCause::ResumeBlocked),
    ]
}

fn exit_reason() -> impl Strategy<Value = ExitReason> {
    prop_oneof![
        Just(ExitReason::Exit),
        Just(ExitReason::Stopped),
        Just(ExitReason::SpawnFailed),
        Just(ExitReason::DaemonRestart),
        Just(ExitReason::UnknownAfterCrash),
    ]
}

fn state() -> impl Strategy<Value = ThreadState> {
    (
        process(),
        turn(),
        task(),
        proptest::collection::btree_set(cause(), 0..4),
    )
        .prop_map(|(process, turn, task, attention)| ThreadState {
            process,
            turn,
            task,
            attention,
        })
}

fn input() -> impl Strategy<Value = StateInput> {
    let actor = prop_oneof![
        Just(Actor::Human),
        Just(Actor::Thread(ThreadId::from_u128(7)))
    ];
    prop_oneof![
        any::<bool>().prop_map(|relaunch| StateInput::Reserved { relaunch }),
        Just(StateInput::Spawned),
        Just(StateInput::SessionBound),
        Just(StateInput::StopRequested),
        exit_reason().prop_map(|reason| StateInput::Exited { reason }),
        Just(StateInput::DeathConfirmed),
        Just(StateInput::ReadinessObserved),
        Just(StateInput::PromptSubmitted),
        Just(StateInput::Interrupted),
        Just(StateInput::TurnStopped),
        Just(StateInput::ApprovalOpened),
        Just(StateInput::ApprovalsAllResolved),
        cause().prop_map(StateInput::AttentionRaised),
        cause().prop_map(StateInput::AttentionCleared),
        (task(), actor).prop_map(|(to, by)| StateInput::TaskSet { to, by }),
    ]
}

/// The causes `input` may clear (§2.2 "Attention", "Cleared on").
fn may_clear(input: &StateInput, c: AttentionCause) -> bool {
    match input {
        StateInput::SessionBound | StateInput::Exited { .. } => matches!(
            c,
            AttentionCause::SessionStartTimeout | AttentionCause::StartupStall
        ),
        StateInput::DeathConfirmed => c == AttentionCause::ResumeBlocked,
        StateInput::AttentionCleared(target) => c == *target && c.cleared_explicitly(),
        _ => false,
    }
}

/// Applies the emitted changes to the old state, as a projection would.
fn apply(old: &ThreadState, changes: &[StatusChanged]) -> ThreadState {
    let mut s = old.clone();
    for c in changes {
        match c {
            StatusChanged::ProcessState { from, to, .. } => {
                assert_eq!(s.process, *from);
                s.process = *to;
            }
            StatusChanged::TurnState { from, to, .. } => {
                assert_eq!(s.turn, *from);
                s.turn = *to;
            }
            StatusChanged::TaskState { from, to, .. } => {
                assert_eq!(s.task, *from);
                s.task = *to;
            }
            StatusChanged::Attention { from, to, .. } => {
                assert_eq!(s.attention.iter().copied().collect::<Vec<_>>(), *from);
                s.attention = to.iter().copied().collect::<BTreeSet<_>>();
            }
        }
    }
    s
}

fn field(c: &StatusChanged) -> &'static str {
    match c {
        StatusChanged::ProcessState { .. } => "process",
        StatusChanged::TurnState { .. } => "turn",
        StatusChanged::TaskState { .. } => "task",
        StatusChanged::Attention { .. } => "attention",
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    /// Never panics; an error leaves nothing changed; every change appears in exactly one
    /// `StatusChanged` with the input's cause, and nothing else does.
    #[test]
    fn every_change_appears_in_exactly_one_status_changed(s in state(), i in input()) {
        if let Ok((n, changes)) = reduce(&s, &i) {
            prop_assert_eq!(apply(&s, &changes), n.clone());
            let fields: Vec<_> = changes.iter().map(field).collect();
            let unique: BTreeSet<_> = fields.iter().collect();
            prop_assert_eq!(unique.len(), fields.len());
            for c in &changes {
                let (changed, cause) = match c {
                    StatusChanged::ProcessState { from, to, cause } => (from != to, *cause),
                    StatusChanged::TurnState { from, to, cause } => (from != to, *cause),
                    StatusChanged::TaskState { from, to, cause } => (from != to, *cause),
                    StatusChanged::Attention { from, to, cause } => (from != to, *cause),
                };
                prop_assert!(changed);
                prop_assert_eq!(cause, i.cause());
            }
        }
    }

    /// CL-2: after registration, `task` changes only on `TaskSet`.
    #[test]
    fn task_changes_only_on_task_set(
        has_task in any::<bool>(),
        inputs in proptest::collection::vec(input(), 0..40),
    ) {
        let mut s = ThreadState::registered(has_task);
        prop_assert_eq!(
            s.task,
            if has_task { TaskState::Active } else { TaskState::None }
        );
        for i in &inputs {
            if let Ok((n, _)) = reduce(&s, i) {
                if !matches!(i, StateInput::TaskSet { .. }) {
                    prop_assert_eq!(n.task, s.task);
                }
                s = n;
            }
        }
    }

    /// After `Exited`, `turn` is `unknown`.
    #[test]
    fn exited_leaves_turn_unknown(s in state(), reason in exit_reason()) {
        if let Ok((n, _)) = reduce(&s, &StateInput::Exited { reason }) {
            prop_assert_eq!(n.process, ProcessState::Exited);
            prop_assert_eq!(n.turn, TurnState::Unknown);
        }
    }

    /// CL-3: each attention cause is cleared only by its own clearing input, and a
    /// clearing input that is not an exit leaves `turn` unchanged.
    #[test]
    fn attention_cleared_only_by_its_own_input(s in state(), i in input()) {
        if let Ok((n, _)) = reduce(&s, &i) {
            for c in s.attention.difference(&n.attention) {
                prop_assert!(may_clear(&i, *c), "{:?} cleared {:?}", i, c);
            }
            if matches!(
                i,
                StateInput::SessionBound
                    | StateInput::DeathConfirmed
                    | StateInput::AttentionCleared(_)
                    | StateInput::AttentionRaised(_)
            ) {
                prop_assert_eq!(n.turn, s.turn);
            }
        }
    }

    /// `ApprovalsAllResolved` sets `running` only from `awaiting_approval`.
    #[test]
    fn approvals_resolved_sets_running_only_from_awaiting(s in state()) {
        if let Ok((n, _)) = reduce(&s, &StateInput::ApprovalsAllResolved) {
            if s.turn == TurnState::AwaitingApproval {
                prop_assert_eq!(n.turn, TurnState::Running);
            } else {
                prop_assert_eq!(n.turn, s.turn);
            }
        }
    }

    /// PR 1 obligation: superseded cards take precedence over resolution. After an open
    /// card, `TurnStopped`, `Interrupted`, or `Exited` wins over a later resolution.
    #[test]
    fn supersession_wins_over_late_resolution(
        closer in prop_oneof![
            Just(StateInput::TurnStopped),
            Just(StateInput::Interrupted),
            exit_reason()
                .prop_filter("not a spawn failure", |r| *r != ExitReason::SpawnFailed)
                .prop_map(|reason| StateInput::Exited { reason }),
        ],
    ) {
        let s = running_with_open_card();
        let (s, _) = reduce(&s, &closer).expect("closer applies to a live launch");
        prop_assert_eq!(s.turn, TurnState::Unknown);
        let (s, changes) = reduce(&s, &StateInput::ApprovalsAllResolved).expect("late resolution is accepted");
        prop_assert_eq!(s.turn, TurnState::Unknown);
        prop_assert!(changes.is_empty());
    }

    /// `TurnStopped` alone never sets `idle`.
    #[test]
    fn turn_stopped_never_sets_idle(s in state()) {
        if let Ok((n, _)) = reduce(&s, &StateInput::TurnStopped) {
            prop_assert_eq!(n.turn, TurnState::Unknown);
        }
    }

    /// `DeathConfirmed` clears `resume_blocked`.
    #[test]
    fn death_confirmed_clears_resume_blocked(mut s in state()) {
        s.process = ProcessState::Exited;
        s.attention.insert(AttentionCause::ResumeBlocked);
        let (n, _) = reduce(&s, &StateInput::DeathConfirmed).expect("applies to an exited launch");
        prop_assert!(!n.attention.contains(&AttentionCause::ResumeBlocked));
    }
}

fn running_with_open_card() -> ThreadState {
    let mut s = ThreadState::registered(true);
    for i in [
        StateInput::Reserved { relaunch: false },
        StateInput::Spawned,
        StateInput::SessionBound,
        StateInput::PromptSubmitted,
        StateInput::ApprovalOpened,
    ] {
        s = reduce(&s, &i).expect("startup sequence").0;
    }
    assert_eq!(s.turn, TurnState::AwaitingApproval);
    s
}

/// Resolution while the card is still open returns the turn to `running` (the
/// positive control of `supersession_wins_over_late_resolution`).
#[test]
fn resolution_of_open_card_returns_to_running() {
    let (s, _) = reduce(&running_with_open_card(), &StateInput::ApprovalsAllResolved).unwrap();
    assert_eq!(s.turn, TurnState::Running);
}

/// PR 1 obligation: a spawn failure is `Exited { reason: spawn_failed }`, reduced like
/// any exit from `starting`, not an input of its own. The exhaustive match below stops
/// compiling if a separate spawn-failure input is ever added.
#[test]
fn spawn_failure_is_an_exit_with_reason() {
    let mut s = ThreadState::registered(false);
    s.attention.insert(AttentionCause::SessionStartTimeout);
    let (s, _) = reduce(&s, &StateInput::Reserved { relaunch: false }).unwrap();
    let (n, changes) = reduce(
        &s,
        &StateInput::Exited {
            reason: ExitReason::SpawnFailed,
        },
    )
    .expect("a reserved launch can fail to spawn");
    assert_eq!(n.process, ProcessState::Exited);
    assert_eq!(n.turn, TurnState::Unknown);
    assert!(n.attention.is_empty());
    assert!(changes.iter().all(|c| matches!(
        c,
        StatusChanged::ProcessState { cause, .. } | StatusChanged::Attention { cause, .. }
            if *cause == chaldea_core::state::StatusCause::Exited
    )));

    // A started launch cannot fail to spawn.
    let (running, _) = reduce(&s, &StateInput::Spawned).unwrap();
    assert!(
        reduce(
            &running,
            &StateInput::Exited {
                reason: ExitReason::SpawnFailed
            }
        )
        .is_err()
    );

    let every_input = |i: &StateInput| match i {
        StateInput::Reserved { .. }
        | StateInput::Spawned
        | StateInput::SessionBound
        | StateInput::StopRequested
        | StateInput::Exited { .. }
        | StateInput::DeathConfirmed
        | StateInput::ReadinessObserved
        | StateInput::PromptSubmitted
        | StateInput::Interrupted
        | StateInput::TurnStopped
        | StateInput::ApprovalOpened
        | StateInput::ApprovalsAllResolved
        | StateInput::AttentionRaised(_)
        | StateInput::AttentionCleared(_)
        | StateInput::TaskSet { .. } => (),
    };
    every_input(&StateInput::Spawned);
}

/// A resume is refused while `resume_blocked` is raised (§2.9).
#[test]
fn reservation_refused_while_resume_blocked() {
    let mut s = ThreadState::registered(false);
    s.attention.insert(AttentionCause::ResumeBlocked);
    assert!(reduce(&s, &StateInput::Reserved { relaunch: true }).is_err());
    let (s, _) = reduce(&s, &StateInput::DeathConfirmed).unwrap();
    assert!(reduce(&s, &StateInput::Reserved { relaunch: true }).is_ok());
}

/// A stop is applied only after the spawn outcome is recorded: while `starting`, it is
/// refused, so `stopping` always means the child started.
#[test]
fn stop_is_refused_while_starting() {
    let s = ThreadState::registered(false);
    let (starting, _) = reduce(&s, &StateInput::Reserved { relaunch: false }).unwrap();
    assert!(reduce(&starting, &StateInput::StopRequested).is_err());
}

/// After `Spawned → StopRequested`, the child started, so it cannot fail to spawn.
#[test]
fn spawn_failure_is_refused_after_a_stop_of_a_started_launch() {
    let s = ThreadState::registered(false);
    let (s, _) = reduce(&s, &StateInput::Reserved { relaunch: false }).unwrap();
    let (s, _) = reduce(&s, &StateInput::Spawned).unwrap();
    let (stopping, _) = reduce(&s, &StateInput::StopRequested).unwrap();
    assert!(
        reduce(
            &stopping,
            &StateInput::Exited {
                reason: ExitReason::SpawnFailed
            }
        )
        .is_err()
    );
    assert!(reduce(&stopping, &StateInput::Spawned).is_err());
}

/// The normal stop path after the spawn still works.
#[test]
fn stop_after_spawn_exits() {
    let s = ThreadState::registered(false);
    let (s, _) = reduce(&s, &StateInput::Reserved { relaunch: false }).unwrap();
    let (s, _) = reduce(&s, &StateInput::Spawned).unwrap();
    let (s, _) = reduce(&s, &StateInput::StopRequested).unwrap();
    assert_eq!(s.process, ProcessState::Stopping);
    let (s, _) = reduce(
        &s,
        &StateInput::Exited {
            reason: ExitReason::Stopped,
        },
    )
    .unwrap();
    assert_eq!(s.process, ProcessState::Exited);
    assert_eq!(s.turn, TurnState::Unknown);
}

/// A bound `UserPromptSubmit` is never lost, even while a card is open: the turn becomes
/// `running` and the daemon supersedes the cards. Readiness while a card is open changes
/// nothing, so the gate stays closed by the turn.
#[test]
fn prompt_and_readiness_while_a_card_is_open() {
    let open = running_with_open_card();
    let (n, changes) = reduce(&open, &StateInput::PromptSubmitted).expect("prompt is accepted");
    assert_eq!(n.turn, TurnState::Running);
    assert_eq!(changes.len(), 1);

    let (n, changes) =
        reduce(&open, &StateInput::ReadinessObserved).expect("readiness is accepted");
    assert_eq!(n.turn, TurnState::AwaitingApproval);
    assert!(changes.is_empty());
}
