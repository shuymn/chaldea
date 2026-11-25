// -*- mode: Bluespec; -*-
/**
 * Named deterministic traces of `thread_input` (docs/design/formal-specs.md "Trace
 * bridge"). Each `trace_*` run is exported as ITF to `specs/traces/` and replayed against
 * the real reducers by crates/core/tests/quint_traces.rs.
 *
 * - `trace_refused_*` runs end where the contract refuses a race a reviewer found: the
 *   writer's recheck cancels the input, or no readiness evidence is admitted.
 * - `trace_coverage_*` runs reach reducer and gate branches the other traces shadow.
 * - `trace_hole_*` and `trace_finding_*` runs reach a state that a contract invariant
 *   forbids, or a gap in it, using inputs the reducers accept; each ends with an `expect`
 *   that states the gap (docs/design/formal-specs.md §3).
 */
module thread_input_traces {
  import thread_input.* from "./thread_input"

  pure val CODEX: LaunchConfig = { adapter: CodexPositional, mode: StopSettle, needsBracketedPaste: false, upgraded: true }
  pure val CODEX_MANUAL: LaunchConfig = { ...CODEX, adapter: CodexManual }
  pure val CLAUDE_POST_DECISION: LaunchConfig = { ...CLAUDE, mode: PostDecisionMode }

  /// The settle completion of the latest `Stop`.
  action settle: bool = ready(StopSettled, st.stopCount)

  /// A key sent from card `c` of the current launch.
  pure def cardKey(s: State, c: int): HumanOrigin = CardKey({ launch: s.launch, card: c })

  // A bound, running Claude launch with `SessionStart` evidence (bracketed paste, 2004 on).
  run readyLaunch =
    initWith(true)
      .then(reserve(CLAUDE))
      .then(spawn)
      .then(decset(true))
      .then(sessionStartHook)
      .then(bind)
      .then(ready(SessionStart, 0))
      .expect(last.gate == Open)

  // A bound Codex launch whose positional prompt was acked and whose first turn settled.
  run readyCodex =
    initWith(true)
      .then(reserve(CODEX))
      .then(spawn)
      .then(sessionStartHook)
      .then(bind)
      .then(prompt(Set(1)))
      .then(turnStop)
      .then(settle)
      .expect(stateOf(st, 1) == Acked and last.gate == Open)

  /// mvp §2.3: gate open -> sending -> dispatched -> sent -> attributed ack; the turn's
  /// `Stop` and its settle evidence reopen the gate.
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
      .then(settle)
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

  /// mvp §2.3 "Ack": a same-text hook before the input dispatched is a `user_message`; it
  /// leaves no candidate, so the hook after the dispatch acks the input.
  run trace_undispatched_match_ignored =
    readyLaunch
      .then(send(1))
      .then(open(1))
      .then(prompt(Set(1)))
      .expect(st.inputs.get(1).candidate == NoCandidate)
      .then(turnStop)
      .then(settle)
      .then(dispatch(1))
      .expect(st.inputs.get(1).dispatched)
      .then(prompt(Set(1)))
      .then(outcome(1, true))
      .expect(stateOf(st, 1) == Acked)

  /// mvp §2.3: a `key` write after the input's write makes the ack non-attributable; the
  /// input is resolved by the owner. Evidence of the same turn as an attach write does not
  /// reopen the gate; `chaldea ready` does.
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
      .then(turnStop)
      .then(humanWrite(Attach))
      .then(settle)
      .expect(last.gate == Closed(EvidenceOfSameTurn))
      .then(ownerReady)
      .expect(last.gate == Open)

  /// mvp §2.3, §2.10: a card key answers its own open card and closes nothing beyond the
  /// last write; the next evidence of the same turn reopens the gate.
  run trace_card_key =
    readyLaunch
      .then(prompt(Set()))
      .then(cardOpen(1))
      .then(humanWrite(cardKey(st, 1)))
      .then(cardResolve(1))
      .expect(st.thread.turn == TurnRunning)
      .then(turnStop)
      .then(settle)
      .expect(last.gate == Open)

  /// mvp §2.10: cards have identity. `Stop` supersedes card 1; card 2 opens in the next
  /// turn; card 1's late resolution feeds nothing, so the turn keeps awaiting card 2.
  run trace_cards_with_identity =
    readyLaunch
      .then(prompt(Set()))
      .then(cardOpen(1))
      .then(turnStop)
      .then(prompt(Set()))
      .then(cardOpen(2))
      .then(lateResolve(1))
      .expect(last.calls == [] and st.thread.turn == AwaitingApproval)
      .expect(not(openCards(st).contains(1)))
      .then(humanWrite(cardKey(st, 2)))
      .then(cardResolve(2))
      .expect(st.thread.turn == TurnRunning)

  /// mvp §2.10, state.rs:362-367: `Stop` supersedes both cards; a late resolution changes
  /// nothing. The task axis changes only on `task_set`.
  run trace_late_card_resolution =
    readyLaunch
      .then(prompt(Set()))
      .then(cardOpen(1))
      .then(cardOpen(2))
      .then(turnStop)
      .then(lateResolve(1))
      .expect(st.thread == last.prevThread)
      .then(setTask(TaskCompleted))
      .then(stop)
      .then(exit(ExitStopped, true))
      .expect(st.thread.task == TaskCompleted and st.thread.turn == TurnUnknown)

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
      .then(reserve(CLAUDE))
      .then(spawn)
      .then(decset(true))
      .then(sessionStartHook)
      .then(bind)
      .then(ready(SessionStart, 0))
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
  /// `launch_started`, acked by the first `UserPromptSubmit` after binding; the upgraded
  /// CLI's `startup_stall` clears on binding.
  run trace_positional_prompt =
    initWith(true)
      .then(reserve(CODEX))
      .then(spawn)
      .expect(stateOf(st, 1) == Sent)
      .then(stall)
      .then(sessionStartHook)
      .then(bind)
      .expect(st.thread.attention == Set())
      .then(prompt(Set(1)))
      .expect(stateOf(st, 1) == Acked)

  /// mvp §2.9 "Binding": hooks of an unbound Codex launch are buffered with their intake
  /// facts and promoted in order at binding: the prompt acks the positional input, the
  /// `Stop` and its settle evidence open the gate.
  run trace_buffered_promotion =
    initWith(false)
      .then(reserve(CODEX))
      .then(spawn)
      .then(bufferPrompt(Set(1)))
      .then(bufferStop)
      .then(bufferReady(StopSettled, 1))
      .then(sessionStartHook)
      .then(bind)
      .expect(stateOf(st, 1) == Acked and last.gate == Open and st.gp.evidenceTurn == 1)

  /// A positional prompt whose spawn is confirmed not to have started is rejected; one
  /// whose launch crashed before `launch_started` is `unconfirmed`.
  run trace_positional_spawn_outcomes =
    initWith(false)
      .then(reserve(CODEX))
      .then(spawnFail)
      .expect(stateOf(st, 1) == Rejected)
      .then(reserve(CODEX))
      .then(restart)
      .expect(stateOf(st, 2) == Unconfirmed and st.thread.attention == Set(InputUnconfirmed(2)))

  /// A spawn whose process existed and then failed: the positional prompt `unconfirmed`,
  /// death confirmed exactly as the cleanup was, with either cleanup.
  run trace_spawn_started_then_failed =
    initWith(false)
      .then(reserve(CODEX))
      .then(spawnStartedThenFailed(true))
      .expect(stateOf(st, 1) == Unconfirmed and st.deathConfirmed
        and st.thread.process == Exited and st.thread.attention == Set(InputUnconfirmed(1)))
      .then(reserve(CODEX))
      .then(spawnStartedThenFailed(false))
      .expect(stateOf(st, 2) == Unconfirmed and not(st.deathConfirmed)
        and st.thread.attention == Set(InputUnconfirmed(1), InputUnconfirmed(2)))

  /// A `SessionStart` that does not bind leaves the launch unbound: the timer still raises
  /// `session_start_timeout`, and the exit clears it.
  run trace_session_timer_after_unbound_session_start =
    initWith(false)
      .then(reserve(CODEX))
      .then(spawn)
      .then(sessionStartHook)
      .expect(not(st.gp.bound))
      .then(sessionTimer)
      .expect(st.thread.attention.contains(SessionStartTimeout))
      .then(exit(ExitNormal, true))
      .expect(not(st.thread.attention.contains(SessionStartTimeout)))

  /// Refused: evidence of the same turn as a `key` write arrives after the gate opened;
  /// the writer's recheck sees `EvidenceOfSameTurn` and cancels with zero bytes.
  run trace_refused_recheck_same_turn_evidence =
    readyLaunch
      .then(humanWrite(Key))
      .then(ownerReady)
      .then(send(1))
      .then(open(1))
      .then(turnStop)
      .then(settle)
      .expect(gate(recheckView(st, 1)) == Closed(EvidenceOfSameTurn))
      .then(dispatch(1))
      .expect(stateOf(st, 1) == Cancelled and st.writer == [WHuman])

  /// Refused: a VT reply after the admitted basis, then newer owner evidence; the recheck
  /// keeps the basis the send relied on and cancels with zero bytes.
  run trace_refused_recheck_reply_after_basis =
    readyLaunch
      .then(send(1))
      .then(open(1))
      .then(vtReply)
      .then(ownerReady)
      .expect(gate(recheckView(st, 1)) == Open)
      .then(dispatch(1))
      .expect(stateOf(st, 1) == Cancelled and st.writer == [WReply])

  /// Refused: a Codex interrupt is not a `Stop`, so no settle evidence of that turn
  /// exists and the gate stays closed (mvp §2.3:173, :175).
  run trace_refused_interrupt_readiness =
    readyCodex
      .then(humanWrite(Key))
      .then(prompt(Set()))
      .then(interrupt)
      .expect(not(readyAllowed(st, StopSettled, st.stopCount)))
      .expect(last.gate == Closed(WriteAfterEvidence))

  /// Refused: a card resolution after `Stop` is a later hook of the launch, so the settle
  /// completion of that `Stop` no longer counts; the gate stays closed and the input waits.
  run trace_refused_settle_after_late_resolution =
    readyLaunch
      .then(prompt(Set()))
      .then(cardOpen(1))
      .then(turnStop)
      .then(lateResolve(1))
      .expect(not(readyAllowed(st, StopSettled, st.stopCount)))
      .then(send(1))
      .expect(last.gate == Closed(TurnNotIdle) and stateOf(st, 1) == Queued)

  /// Refused: a late `Stop` of the previous turn cannot back the current turn's evidence,
  /// nor can it once the current turn's own `Stop` replaced it; that `Stop` can.
  run trace_refused_late_stop_of_earlier_turn =
    readyLaunch
      .then(prompt(Set()))
      .then(turnStop)
      .then(prompt(Set()))
      .then(stopOfTurn(1))
      .expect(not(readyAllowed(st, StopSettled, st.stopCount)) and st.thread.turn == TurnRunning)
      .then(turnStop)
      .expect(not(readyAllowed(st, StopSettled, st.stopCount - 1)))
      .then(settle)
      .expect(last.gate == Open and st.gp.evidenceTurn == 2)

  /// Refused: `SessionStart` after a buffered `Stop` is a later hook, so that `Stop`
  /// settles nothing, not even after binding replays it, and no input is written.
  run trace_refused_buffered_settle_after_session_start =
    initWith(false)
      .then(reserve(CODEX))
      .then(spawn)
      .then(bufferPrompt(Set(1)))
      .then(bufferStop)
      .then(sessionStartHook)
      .expect(st.bufferedStop == 0)
      .then(bind)
      .expect(stateOf(st, 1) == Acked and last.gate == Closed(NoEvidence))
      .expect(not(readyAllowed(st, StopSettled, st.stopCount)))
      .then(send(2))
      .expect(last.gate == Closed(NoEvidence) and stateOf(st, 2) == Queued)

  /// mvp §2.9:325: a `Stop` buffered after `SessionStart` stays live through binding, so
  /// its settle completion opens the gate.
  run trace_buffered_stop_after_session_start =
    initWith(false)
      .then(reserve(CODEX))
      .then(spawn)
      .then(bufferPrompt(Set(1)))
      .then(sessionStartHook)
      .then(bufferStop)
      .then(bind)
      .then(settle)
      .expect(stateOf(st, 1) == Acked and last.gate == Open and st.gp.evidenceTurn == 1)

  /// Refused (mvp §2.10:336): a key sent from card 1 after `Stop` superseded it is refused
  /// at the write boundary while card 2 of the next turn is open: nothing written, no event.
  run trace_refused_superseded_card_key =
    readyLaunch
      .then(prompt(Set()))
      .then(cardOpen(1))
      .then(turnStop)
      .then(prompt(Set()))
      .then(cardOpen(2))
      .then(humanWrite(cardKey(st, 1)))
      .expect(last.calls == [] and st.writer == [] and st.gp.lastWrite == 0 and st.cardKeys == [])

  /// mvp §2.7: a manual Codex launch fires no hook until a human starts the first turn
  /// through attach; its hooks then bind and settle the launch.
  run trace_manual_first_turn =
    initWith(false)
      .then(reserve(CODEX_MANUAL))
      .then(spawn)
      .expect(not(st.hooksLive) and st.thread.attention == Set())
      .then(manualFirstTurn)
      .then(sessionStartHook)
      .then(bind)
      .then(prompt(Set()))
      .then(turnStop)
      .then(settle)
      .expect(last.gate == Open)

  /// mvp §2.3 `post_decision`: the CLI's post-decision signal is its own entry for the
  /// current turn, taken in before the daemon took in the turn's `Stop`.
  run trace_post_decision_own_entry =
    initWith(true)
      .then(reserve(CLAUDE_POST_DECISION))
      .then(spawn)
      .then(decset(true))
      .then(sessionStartHook)
      .then(bind)
      .then(prompt(Set()))
      .then(ready(PostDecision, 0))
      .expect(last.gate == Open and st.gp.evidenceTurn == 1)

  /// Coverage: `BracketedPasteOff`, `InputInFlight`, and a rejected input.
  run trace_coverage_gate_reasons =
    readyLaunch
      .then(decset(false))
      .expect(last.gate == Closed(BracketedPasteOff))
      .then(decset(true))
      .then(send(1))
      .then(open(1))
      .then(dispatch(1))
      .then(outcome(1, true))
      .then(ownerReady)
      .expect(last.gate == Closed(InputInFlight))
      .then(send(2))
      .then(rejectInput(2))
      .expect(stateOf(st, 2) == Rejected)

  /// Coverage: `Interrupted`, exit recovery of a `sent` and a `queued` input
  /// (`LaunchExited`), `resolve --delivered`, and a refused step.
  run trace_coverage_exit_recovery =
    readyCodex
      .then(send(2))
      .then(open(2))
      .then(dispatch(2))
      .then(outcome(2, true))
      .then(send(3))
      .then(interrupt)
      .expect(st.thread.turn == TurnUnknown)
      .then(exit(ExitNormal, true))
      .expect(stateOf(st, 2) == Unconfirmed and stateOf(st, 3) == Cancelled)
      .then(resolveInput(2, true))
      .expect(stateOf(st, 2) == Acked and st.thread.attention == Set())
      .then(rawStep(2, Resolved(true)))
      .expect(not(last.calls[0].ok))

  /// Hole a: after a restart the reducer accepts a relaunch reservation, because `Exited`
  /// carries no death bit and `Reserved` checks only `resume_blocked` (state.rs:174,
  /// :278-288); mvp §2.9:323 requires death confirmed.
  run trace_hole_a_reserve_without_death =
    readyLaunch
      .then(restart)
      .expect(not(st.deathConfirmed) and not(st.thread.attention.contains(ResumeBlocked)))
      .then(raw(Reserved(true)))
      .expect(st.thread.process == Starting and not(reservedOnlyAfterDeath))

  /// Hole b: `sending` -> `cancelled` on a failed recheck writes zero bytes, but the
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

  /// Hole c: the reducer raises a startup cause in any process state; one raised while
  /// `exited` survives `Reserved` into the next launch (state.rs:369-371, :278-291).
  run trace_hole_c_stale_startup_cause =
    readyLaunch
      .then(exit(ExitNormal, true))
      .then(raw(AttentionRaised(StartupStall)))
      .expect(not(noStartupCauseAfterBindOrExit))
      .then(reserve(CLAUDE))
      .expect(st.thread.process == Starting and st.thread.attention == Set(StartupStall))

  /// Hole d: the reducer accepts readiness of an unbound launch (state.rs:333-336); fed
  /// directly instead of buffered on a Codex launch, whose hooks before binding are
  /// buffered, it survives binding as evidence older than the binding.
  run trace_hole_d_unbound_readiness =
    initWith(false)
      .then(reserve(CODEX))
      .then(spawn)
      .then(rawReadyUnbound(StopSettled))
      .expect(st.thread.turn == Idle)
      .then(sessionStartHook)
      .then(bind)
      .expect(not(gateEvidenceIsBound) and last.gate == Closed(InputInFlight))

  /// Finding: `ApprovalsAllResolved` carries no card identity; fed for card 1 after card 2
  /// opened in the next turn, it ends the turn's wait for card 2 (state.rs:362-367).
  run trace_finding_resolution_without_card_identity =
    readyLaunch
      .then(prompt(Set()))
      .then(cardOpen(1))
      .then(turnStop)
      .then(prompt(Set()))
      .then(cardOpen(2))
      .then(raw(ApprovalsAllResolved))
      .expect(st.thread.turn == TurnRunning and not(openCardsAwaitApproval))

  /// Finding: a VT reply after `launch_started` is a counted write after the positional
  /// prompt's anchor, so its ack is never attributable (mvp §2.3:182 with the default
  /// counted classes, docs/design/pty-runtime.md §2.3 "Basis").
  run trace_finding_positional_vt_reply =
    initWith(true)
      .then(reserve(CODEX))
      .then(spawn)
      .then(vtReply)
      .then(sessionStartHook)
      .then(bind)
      .then(prompt(Set(1)))
      .expect(stateOf(st, 1) == Unconfirmed)
}
