// -*- mode: Bluespec; -*-
/**
 * Named deterministic traces of `thread_input` (docs/design/formal-specs.md "Trace
 * bridge"). Each `trace_*` run is exported as ITF to `specs/traces/` and replayed against
 * the real reducers by crates/core/tests/quint_traces.rs. `trace_hole_*` and
 * `trace_finding_*` runs reach a state that a contract invariant forbids, or a gap in it,
 * using inputs the reducers accept; each ends with an `expect` that states the gap.
 */
module thread_input_traces {
  import thread_input.* from "./thread_input"

  // A bound, running launch with readiness evidence (Claude: bracketed paste, 2004 on).
  run readyLaunch =
    initWith(true)
      .then(reserve(false, false, true))
      .then(spawn)
      .then(decset(true))
      .then(bind)
      .then(ready(SessionStart))
      .expect(last.gate == Open)

  /// mvp §2.3: gate open -> sending -> dispatched -> sent -> attributed ack.
  run trace_send_ack =
    readyLaunch
      .then(send(1))
      .then(open(1))
      // The projection counts the input's own attempt as the last write and in flight.
      .expect(last.gate == Closed(WriteAfterEvidence))
      .then(dispatch(1))
      .then(outcome(1, true))
      .then(prompt(Set(1)))
      .expect(stateOf(st, 1) == Acked)
      .then(turnStop)
      .then(ready(StopSettled))
      .expect(last.gate == Open)

  /// mvp §2.3 "Ack": an ack arriving while `sending` is kept and applied once `sent`.
  run trace_ack_while_sending =
    readyLaunch
      .then(send(1))
      .then(open(1))
      .then(dispatch(1))
      .then(prompt(Set(1)))
      .expect(stateOf(st, 1) == Sending)
      .then(outcome(1, true))
      .expect(stateOf(st, 1) == Acked)

  /// mvp §2.3: a `key` write after the input's write makes the ack non-attributable; the
  /// input is resolved by the owner. Evidence of the same turn does not reopen the gate;
  /// `chaldea ready` does.
  run trace_key_write_not_attributable =
    readyLaunch
      .then(send(1))
      .then(open(1))
      .then(dispatch(1))
      .then(outcome(1, true))
      .then(humanWrite(Key))
      .then(prompt(Set(1)))
      .expect(stateOf(st, 1) == Unconfirmed and st.thread.attention.contains(InputUnconfirmed(1)))
      .then(resolveInput(1, false))
      .expect(stateOf(st, 1) == Dismissed and st.thread.attention == Set())
      .then(humanWrite(Attach))
      .then(ready(StopSettled))
      .expect(last.gate == Closed(EvidenceOfSameTurn))
      .then(ownerReady)
      .expect(last.gate == Open)

  /// mvp §2.3: a card key closes nothing beyond the last write; the next evidence of the
  /// same turn reopens the gate, unlike after a raw key.
  run trace_card_key =
    readyLaunch
      .then(prompt(Set()))
      .then(cardOpen)
      .then(humanWrite(CardKey))
      .then(cardResolve)
      .expect(st.thread.turn == TurnRunning)
      .then(turnStop)
      .then(ready(StopSettled))
      .expect(last.gate == Open)

  /// mvp §2.3, §2.4 "Restart", §2.9: a restart turns `sending` into `unconfirmed` and
  /// `queued` into `cancelled`; the resume is refused until death is confirmed; the
  /// earlier launch's `unconfirmed` input does not block the new launch's gate.
  run trace_restart_and_resume =
    readyLaunch
      .then(send(1))
      .then(open(1))
      .then(dispatch(1))
      .then(send(2))
      .then(restart)
      .expect(stateOf(st, 1) == Unconfirmed and stateOf(st, 2) == Cancelled)
      .then(reserveRefused)
      .expect(st.thread.attention.contains(ResumeBlocked))
      .then(confirmDeath)
      .then(reserve(false, false, false))
      .then(spawn)
      .then(bind)
      .then(ready(SessionStart))
      .expect(last.gate == Open and stateOf(st, 1) == Unconfirmed)
      .then(send(3))
      .then(open(3))
      .then(dispatch(3))
      .then(outcome(3, false))
      .expect(stateOf(st, 3) == Unconfirmed)
      .then(prompt(Set(3)))
      .expect(stateOf(st, 3) == Acked and stateOf(st, 1) == Unconfirmed)

  /// mvp §2.3: window expiry, then a late attributable ack; a gate timeout, then cancel.
  run trace_late_ack_and_gate_timeout =
    readyLaunch
      .then(send(1))
      .then(open(1))
      .then(dispatch(1))
      .then(outcome(1, true))
      .then(windowExpired(1))
      .expect(stateOf(st, 1) == Unconfirmed)
      .then(send(2))
      .then(gateTimeout(2))
      .expect(st.thread.attention == Set(InputUnconfirmed(1), InputGateTimeout(2)))
      .then(prompt(Set(1)))
      .expect(stateOf(st, 1) == Acked)
      .then(cancelInput(2))
      .expect(st.thread.attention == Set())

  /// mvp §2.3 "Codex positional first prompt": `sending` with the reservation, `sent` at
  /// `launch_started`, acked by the first `UserPromptSubmit` after binding.
  run trace_positional_prompt =
    initWith(true)
      .then(reserve(true, false, false))
      .then(spawn)
      .expect(stateOf(st, 1) == Sent)
      .then(stall)
      .then(bind)
      .expect(st.thread.attention == Set())
      .then(prompt(Set(1)))
      .expect(stateOf(st, 1) == Acked)

  /// A positional prompt whose spawn is confirmed not to have started is rejected; one
  /// whose launch crashed before `launch_started` is `unconfirmed`.
  run trace_positional_spawn_outcomes =
    initWith(false)
      .then(reserve(true, false, false))
      .then(spawnFail)
      .expect(stateOf(st, 1) == Rejected)
      .then(reserve(true, false, false))
      .then(restart)
      .expect(stateOf(st, 2) == Unconfirmed and st.thread.attention == Set(InputUnconfirmed(2)))

  /// mvp §2.10, state.rs:362-367: `Stop` supersedes the cards; a late resolution changes
  /// nothing. The task axis changes only on `task_set`.
  run trace_late_card_resolution =
    readyLaunch
      .then(prompt(Set()))
      .then(cardOpen)
      .then(cardOpen)
      .then(turnStop)
      .then(lateResolve)
      .expect(last.calls[0].changes == [])
      .then(setTask(TaskCompleted))
      .then(stop)
      .then(exit(ExitStopped, true))
      .expect(st.thread.task == TaskCompleted and st.thread.turn == TurnUnknown)

  /// Hole 4a: after a restart the reducer accepts a relaunch reservation, because
  /// `Exited` carries no death bit and `Reserved` checks only `resume_blocked`
  /// (state.rs:174, :278-288); mvp §2.9:323 requires death confirmed.
  run trace_hole_a_reserve_without_death =
    readyLaunch
      .then(restart)
      .expect(not(st.deathConfirmed) and not(st.thread.attention.contains(ResumeBlocked)))
      .then(raw(Reserved(true)))
      .expect(st.thread.process == Starting and not(reservedOnlyAfterDeath))

  /// Hole 4b: `sending` -> `cancelled` on a failed recheck writes zero bytes, but the
  /// attempt already advanced the last write (mvp §2.1:115), so the gate stays closed
  /// (`WriteAfterEvidence`) with no write since the evidence.
  run trace_hole_b_recheck_keeps_gate_closed =
    readyLaunch
      .then(send(1))
      .then(open(1))
      .then(decset(false))
      .then(dispatch(1))
      .expect(stateOf(st, 1) == Cancelled)
      .then(decset(true))
      .then(send(2))
      .expect(last.gate == Closed(WriteAfterEvidence) and st.writer.length() == st.gp.basis)

  /// Hole 4c: the reducer raises a startup cause in any process state; one raised while
  /// `exited` survives `Reserved` into the next launch (state.rs:369-371, :278-291).
  run trace_hole_c_stale_startup_cause =
    readyLaunch
      .then(exit(ExitNormal, true))
      .then(raw(AttentionRaised(StartupStall)))
      .expect(not(noStartupCauseAfterBindOrExit))
      .then(reserve(false, false, false))
      .expect(st.thread.process == Starting and st.thread.attention == Set(StartupStall))

  /// Hole 4d: the reducer accepts readiness of an unbound launch (state.rs:333-336); the
  /// gate then opens after binding on evidence older than the binding.
  run trace_hole_d_unbound_readiness =
    initWith(true)
      .then(reserve(false, false, false))
      .then(spawn)
      .then(rawReadyUnbound(StopSettled))
      .expect(st.thread.turn == Idle)
      .then(bind)
      .expect(last.gate == Open and not(gateEvidenceIsBound))

  /// Finding: a VT reply after `launch_started` is a counted write after the positional
  /// prompt's anchor, so its ack is never attributable (mvp §2.3:182 with the default
  /// counted classes, docs/design/pty-runtime.md §2.3 "Basis").
  run trace_finding_positional_vt_reply =
    initWith(true)
      .then(reserve(true, false, false))
      .then(spawn)
      .then(vtReply)
      .then(bind)
      .then(prompt(Set(1)))
      .expect(stateOf(st, 1) == Unconfirmed)
}
