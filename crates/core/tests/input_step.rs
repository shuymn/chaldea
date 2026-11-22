//! `step` properties and the `gate` table (`docs/mvp-design.md` §5 "Tests", CL-4, CL-5,
//! CL-6).

use chaldea_core::ids::LaunchId;
use chaldea_core::input::{
    GateClosedReason, GateDecision, GateView, InputInput, InputState, ReadinessMark, RejectReason,
    blocks_gate, gate, step,
};
use chaldea_core::payload::ReadinessEvidence;
use chaldea_core::state::{ProcessState, TurnState};
use proptest::prelude::*;

const ALL_STATES: [InputState; 8] = [
    InputState::Queued,
    InputState::Sending,
    InputState::Sent,
    InputState::Acked,
    InputState::Unconfirmed,
    InputState::Rejected,
    InputState::Cancelled,
    InputState::Dismissed,
];

fn reject_reason() -> impl Strategy<Value = RejectReason> {
    prop_oneof![
        Just(RejectReason::ControlCharacter),
        Just(RejectReason::LossyText),
        Just(RejectReason::EmptyPrompt),
        Just(RejectReason::LeadingTrailingWhitespace),
        Just(RejectReason::Tab),
        Just(RejectReason::SpawnFailed),
    ]
}

fn input() -> impl Strategy<Value = InputInput> {
    prop_oneof![
        any::<u64>().prop_map(|sent_after_seq| InputInput::GateOpened { sent_after_seq }),
        Just(InputInput::WriteCompleted),
        Just(InputInput::WriteOutcomeUnknown),
        reject_reason().prop_map(InputInput::Rejected),
        Just(InputInput::Cancelled),
        Just(InputInput::RecheckFailed),
        Just(InputInput::LaunchExited),
        Just(InputInput::DaemonRestarted),
        Just(InputInput::AckAttributed),
        Just(InputInput::AckNotAttributable),
        Just(InputInput::AckWindowExpired),
        any::<bool>().prop_map(|delivered| InputInput::Resolved { delivered }),
    ]
}

fn state() -> impl Strategy<Value = InputState> {
    proptest::sample::select(ALL_STATES.to_vec())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    /// CL-4: from `queued`, any input sequence enters `sending` at most once and never
    /// returns to `queued` or `sending` from `sending`, `sent`, or `unconfirmed`.
    #[test]
    fn sending_entered_at_most_once(inputs in proptest::collection::vec(input(), 0..40)) {
        let mut s = InputState::Queued;
        let mut entered_sending = 0;
        for i in &inputs {
            if let Ok(n) = step(s, i) {
                if n == InputState::Sending {
                    entered_sending += 1;
                }
                s = n;
            }
        }
        prop_assert!(entered_sending <= 1);
    }

    #[test]
    fn nothing_leads_back_to_queued_or_sending(s in state(), i in input()) {
        if let Ok(n) = step(s, &i) {
            if matches!(s, InputState::Sending | InputState::Sent | InputState::Unconfirmed) {
                prop_assert!(!matches!(n, InputState::Queued | InputState::Sending));
            }
            prop_assert_ne!(n, InputState::Queued);
            if n == InputState::Sending {
                prop_assert_eq!(s, InputState::Queued);
            }
        }
    }

    /// Terminal states accept no input.
    #[test]
    fn terminal_states_accept_nothing(s in state(), i in input()) {
        if s.is_terminal() {
            prop_assert!(step(s, &i).is_err());
        }
    }

    /// `rejected` only from `queued` (validation) or from `sending` on a confirmed spawn
    /// failure of a positional prompt; `cancelled` only from `queued` or from `sending`
    /// on `RecheckFailed`.
    #[test]
    fn rejected_and_cancelled_sources(s in state(), i in input()) {
        match step(s, &i) {
            Ok(InputState::Rejected) => prop_assert!(
                (s == InputState::Queued
                    && matches!(i, InputInput::Rejected(r) if r != RejectReason::SpawnFailed))
                    || (s == InputState::Sending
                        && i == InputInput::Rejected(RejectReason::SpawnFailed))
            ),
            Ok(InputState::Cancelled) => prop_assert!(
                s == InputState::Queued || (s == InputState::Sending && i == InputInput::RecheckFailed)
            ),
            _ => {}
        }
    }

    /// CL-6: `DaemonRestarted` and `LaunchExited` map `sending`/`sent` to `unconfirmed`
    /// and `queued` to `cancelled`.
    #[test]
    fn restart_and_exit_recovery(
        s in proptest::sample::select(vec![InputState::Queued, InputState::Sending, InputState::Sent]),
        i in prop_oneof![Just(InputInput::DaemonRestarted), Just(InputInput::LaunchExited)],
    ) {
        let want = if s == InputState::Queued { InputState::Cancelled } else { InputState::Unconfirmed };
        prop_assert_eq!(step(s, &i), Ok(want));
    }

    /// `dismissed` and `Resolved { delivered: true }` only from `unconfirmed`.
    #[test]
    fn resolve_only_from_unconfirmed(s in state(), delivered in any::<bool>()) {
        let r = step(s, &InputInput::Resolved { delivered });
        if s == InputState::Unconfirmed {
            prop_assert_eq!(r, Ok(if delivered { InputState::Acked } else { InputState::Dismissed }));
        } else {
            prop_assert!(r.is_err());
        }
    }
}

/// PR 1 obligation: a late attributed ack moves `unconfirmed` to `acked`, for every way
/// an input can become `unconfirmed`.
#[test]
fn late_attributed_ack_moves_unconfirmed_to_acked() {
    let paths: [&[InputInput]; 5] = [
        &[InputInput::AckWindowExpired],
        &[InputInput::AckNotAttributable],
        &[InputInput::DaemonRestarted],
        &[InputInput::LaunchExited],
        &[],
    ];
    for path in paths {
        let mut s = step(
            InputState::Queued,
            &InputInput::GateOpened { sent_after_seq: 10 },
        )
        .unwrap();
        s = if path.is_empty() {
            step(s, &InputInput::WriteOutcomeUnknown).unwrap()
        } else {
            step(s, &InputInput::WriteCompleted).unwrap()
        };
        for i in path {
            s = step(s, i).unwrap();
        }
        assert_eq!(s, InputState::Unconfirmed, "path {path:?}");
        assert_eq!(
            step(s, &InputInput::AckAttributed),
            Ok(InputState::Acked),
            "path {path:?}"
        );
    }
}

/// CL-6: only in-flight inputs of the current launch block the gate.
#[test]
fn only_inputs_of_the_current_launch_block_the_gate() {
    let current = LaunchId::from_u128(2);
    let earlier = LaunchId::from_u128(1);
    for s in ALL_STATES {
        let in_flight = matches!(
            s,
            InputState::Sending | InputState::Sent | InputState::Unconfirmed
        );
        assert_eq!(blocks_gate(Some(current), s, current), in_flight, "{s:?}");
        assert!(!blocks_gate(Some(earlier), s, current), "{s:?}");
        assert!(!blocks_gate(None, s, current), "{s:?}");
    }
}

// ---- gate table (CL-5) -------------------------------------------------------------------

/// Every condition met: bound, running, idle, readiness at 20 after the last write at 10
/// (an input) and a human write at 8, with a turn start at 15 after the human write,
/// nothing in flight, bracketed paste on.
fn open_view() -> GateView {
    GateView {
        bound: true,
        process: ProcessState::Running,
        turn: TurnState::Idle,
        readiness: Some(ReadinessMark {
            seq: 20,
            evidence: ReadinessEvidence::StopSettled,
        }),
        last_write: Some(10),
        last_human_write: Some(8),
        last_turn_start: Some(15),
        in_flight: false,
        needs_bracketed_paste: true,
        bracketed_paste_on: true,
    }
}

#[test]
fn gate_opens_with_every_condition_met() {
    assert_eq!(gate(&open_view()), GateDecision::Open);
    // First turn: no write yet, `SessionStart` evidence.
    let first = GateView {
        readiness: Some(ReadinessMark {
            seq: 3,
            evidence: ReadinessEvidence::SessionStart,
        }),
        last_write: None,
        last_human_write: None,
        last_turn_start: Some(2),
        ..open_view()
    };
    assert_eq!(gate(&first), GateDecision::Open);
    // Bracketed paste does not matter for an encoding that does not use it.
    let keyed = GateView {
        needs_bracketed_paste: false,
        bracketed_paste_on: false,
        ..open_view()
    };
    assert_eq!(gate(&keyed), GateDecision::Open);
    // `chaldea ready` reopens after a human write without a later turn.
    let owner = GateView {
        readiness: Some(ReadinessMark {
            seq: 20,
            evidence: ReadinessEvidence::OwnerAsserted,
        }),
        last_turn_start: Some(5),
        ..open_view()
    };
    assert_eq!(gate(&owner), GateDecision::Open);
}

#[test]
fn gate_closes_when_any_one_condition_is_missing() {
    use GateClosedReason as R;
    let base = open_view();
    let cases: Vec<(&str, GateView, R)> = vec![
        (
            "not bound",
            GateView {
                bound: false,
                ..base
            },
            R::NotBound,
        ),
        (
            "process starting",
            GateView {
                process: ProcessState::Starting,
                ..base
            },
            R::ProcessNotRunning,
        ),
        (
            "process stopping",
            GateView {
                process: ProcessState::Stopping,
                ..base
            },
            R::ProcessNotRunning,
        ),
        (
            "process exited",
            GateView {
                process: ProcessState::Exited,
                ..base
            },
            R::ProcessNotRunning,
        ),
        (
            "no evidence (none mode without ready, or interrupt)",
            GateView {
                readiness: None,
                ..base
            },
            R::NoEvidence,
        ),
        (
            "write after evidence",
            GateView {
                last_write: Some(25),
                ..base
            },
            R::WriteAfterEvidence,
        ),
        (
            "write at the evidence position",
            GateView {
                last_write: Some(20),
                ..base
            },
            R::WriteAfterEvidence,
        ),
        (
            "evidence of the same turn as the human write (no turn start between)",
            GateView {
                last_turn_start: Some(5),
                ..base
            },
            R::EvidenceOfSameTurn,
        ),
        (
            "evidence of the same turn, no turn start at all",
            GateView {
                last_turn_start: None,
                ..base
            },
            R::EvidenceOfSameTurn,
        ),
        (
            "turn running",
            GateView {
                turn: TurnState::Running,
                ..base
            },
            R::TurnNotIdle,
        ),
        (
            "turn unknown (after Stop, before readiness)",
            GateView {
                turn: TurnState::Unknown,
                ..base
            },
            R::TurnNotIdle,
        ),
        (
            "turn awaiting approval",
            GateView {
                turn: TurnState::AwaitingApproval,
                ..base
            },
            R::TurnNotIdle,
        ),
        (
            "another input in flight",
            GateView {
                in_flight: true,
                ..base
            },
            R::InputInFlight,
        ),
        (
            "DECSET 2004 off",
            GateView {
                bracketed_paste_on: false,
                ..base
            },
            R::BracketedPasteOff,
        ),
    ];
    for (name, view, want) in cases {
        assert_eq!(gate(&view), GateDecision::Closed(want), "{name}");
    }
}

proptest! {
    /// CL-5: the gate never opens without evidence strictly newer than the last write.
    #[test]
    fn gate_open_implies_evidence_newer_than_last_write(
        bound in any::<bool>(),
        running in any::<bool>(),
        idle in any::<bool>(),
        readiness in proptest::option::of((0u64..50, proptest::sample::select(vec![
            ReadinessEvidence::SessionStart,
            ReadinessEvidence::StopSettled,
            ReadinessEvidence::PostDecision,
            ReadinessEvidence::OwnerAsserted,
        ]))),
        human_write in proptest::option::of(0u64..50),
        input_write in proptest::option::of(0u64..50),
        turn_start in proptest::option::of(0u64..50),
        in_flight in any::<bool>(),
        needs in any::<bool>(),
        on in any::<bool>(),
    ) {
        // The last write of any origin is the later of the two.
        let write = human_write.max(input_write);
        let view = GateView {
            bound,
            process: if running { ProcessState::Running } else { ProcessState::Exited },
            turn: if idle { TurnState::Idle } else { TurnState::Running },
            readiness: readiness.map(|(seq, evidence)| ReadinessMark { seq, evidence }),
            last_write: write,
            last_human_write: human_write,
            last_turn_start: turn_start,
            in_flight,
            needs_bracketed_paste: needs,
            bracketed_paste_on: on,
        };
        if gate(&view) == GateDecision::Open {
            let ev = view.readiness.expect("open needs evidence");
            prop_assert!(bound && running && idle && !in_flight && (!needs || on));
            if let Some(w) = write {
                prop_assert!(ev.seq > w);
            }
            if let Some(h) = human_write {
                prop_assert!(
                    ev.evidence == ReadinessEvidence::OwnerAsserted
                        || turn_start.is_some_and(|t| h < t && t < ev.seq)
                );
            }
        }
    }
}

/// Only a human write needs a later turn or `chaldea ready` to reopen the gate. After an
/// input's own write, the next evidence newer than that write opens it, with or without a
/// turn start recorded in between; a `card_key` counts only as a write of any origin.
#[test]
fn reopen_rule_applies_to_human_writes_only() {
    let after_input_write = GateView {
        last_write: Some(10),
        last_human_write: None,
        last_turn_start: None,
        ..open_view()
    };
    assert_eq!(gate(&after_input_write), GateDecision::Open);

    let after_card_key = GateView {
        last_write: Some(18),
        last_human_write: Some(8),
        last_turn_start: Some(15),
        ..open_view()
    };
    assert_eq!(gate(&after_card_key), GateDecision::Open);

    let after_human_write = GateView {
        last_write: Some(10),
        last_human_write: Some(10),
        last_turn_start: None,
        ..open_view()
    };
    assert_eq!(
        gate(&after_human_write),
        GateDecision::Closed(GateClosedReason::EvidenceOfSameTurn)
    );
}
