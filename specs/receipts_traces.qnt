// -*- mode: Bluespec; -*-
/**
 * Named deterministic traces of `receipts` (docs/design/formal-specs.md §7). Each
 * `trace_*` run is exported as ITF to `specs/traces/receipts/` and its `check_receipt`
 * calls are replayed against the real function by crates/core/tests/quint_sync_traces.rs.
 *
 * - `trace_refused_*` runs end where the contract refuses to answer as a reviewer might
 *   expect (a 408 only before submission or once the runner observed `Expired`; a 409
 *   `in_progress` only once the runner observed the receipt).
 *
 * Attempt ids count from 1 in send order; results count from 1 in commit order.
 */
module receipts_traces {
  import receipts.* from "./receipts"

  pure val STOP = 2

  pure def ok(n: int): Result = { n: n, status: 200 }
  pure def replyAt(s: State, i: int): Reply = s.replies[i].reply
  def lookupNow(l: Lookup): bool = last.calls.indices().exists(i => match last.calls[i] { | CheckReceipt(k) => k.lookup == l })

  /// One attempt from arrival to a reply observed from the writer's answer to its intent
  /// job (`a` is its attempt id).
  action roundTrip(c: int, t: int, h: int, a: int): bool =
    send(c, t, h).then(submit(a)).then(writerStart).then(writerFinish(Ok)).then(observe(a)).then(replyTo(a))

  /// mvp §2.4: `send` completes in its intent job; a retry after a lost reply replays the
  /// stored result, and the intent ran once.
  run trace_send_replayed =
    init
      .then(send(1, 1, SEND))
      .then(submit(1))
      .then(writerStart)
      .then(writerFinish(Ok))
      .expect(lookupNow(New))
      .then(observe(1))
      .then(replyTo(1))
      .expect(replyAt(st, 0) == RResult(ok(1)))
      .then(send(1, 1, SEND))
      .then(submit(2))
      .then(writerStart)
      .then(writerFinish(Ok))
      .expect(lookupNow(Replay(ok(1))))
      .then(observe(2))
      .then(replyTo(2))
      .expect(replyAt(st, 1) == RResult(ok(1)) and st.intents.get(1) == 1)

  /// mvp §2.4 "Concurrent duplicates": two requests with one `command_id` queue two jobs;
  /// the second reads the first's receipt.
  run trace_concurrent_duplicates =
    init
      .then(send(1, 1, SEND))
      .then(send(1, 1, SEND))
      .then(submit(1))
      .then(submit(2))
      .then(writerStart)
      .then(writerFinish(Ok))
      .then(writerStart)
      .then(writerFinish(Ok))
      .expect(lookupNow(Replay(ok(1))) and st.intents.get(1) == 1)

  /// mvp §2.4 "Completion": the runner starts the effect after it observed the intent
  /// committed; the request (its grace over) and a retry meanwhile get 409 `in_progress`;
  /// after completion a retry replays. The effect started once.
  run trace_effect_in_progress_then_replay =
    init
      .then(send(1, 1, STOP))
      .then(submit(1))
      .then(writerStart)
      .then(writerFinish(Ok))
      .then(observe(1))
      .expect(st.effects.keys() == Set(1))
      .then(deadline(1))
      .then(grace(1))
      .then(replyTo(1))
      .expect(replyAt(st, 0) == RInProgress)
      .then(send(1, 1, STOP))
      .then(submit(2))
      .then(writerStart)
      .then(writerFinish(Ok))
      .expect(lookupNow(LInProgress))
      .then(observe(2))
      .then(replyTo(2))
      .expect(replyAt(st, 1) == RInProgress)
      .then(effectDone(1, true))
      .then(writerStart)
      .then(writerFinish(Ok))
      .then(roundTrip(1, 1, STOP, 3))
      .expect(replyAt(st, 2) == RResult(ok(1)) and st.effectStarts.get(1) == 1)

  /// The stored result keeps its status: an effect that refused stores 409, and a retry
  /// replays that 409 with its body (mvp §2.4 `CommandResult`).
  run trace_stored_409_replayed =
    init
      .then(send(1, 1, STOP))
      .then(submit(1))
      .then(writerStart)
      .then(writerFinish(Ok))
      .then(observe(1))
      .then(effectDone(1, false))
      .then(writerStart)
      .then(writerFinish(Ok))
      .then(observe(1))
      .then(replyTo(1))
      .expect(replyAt(st, 0) == RResult({ n: 1, status: 409 }))
      .then(send(1, 1, STOP))
      .then(submit(2))
      .then(writerStart)
      .then(writerFinish(Ok))
      .expect(lookupNow(Replay({ n: 1, status: 409 })))
      .then(observe(2))
      .then(replyTo(2))
      .expect(replyAt(st, 1) == RResult({ n: 1, status: 409 }))

  /// http-surface §8 D14: the intent committed and is durable, but the runner has not
  /// observed the writer's reply when the grace ends, so the reply is 503
  /// `outcome_unknown`; the retry of the same request replays the stored result.
  run trace_outcome_unknown_then_replay =
    init
      .then(send(1, 1, SEND))
      .then(submit(1))
      .then(writerStart)
      .then(deadline(1))
      .then(grace(1))
      .then(writerFinish(Ok))
      .then(replyTo(1))
      .expect(and {
        replyAt(st, 0) == ROutcomeUnknown,
        witnessDurableUnobserved,
      })
      .then(roundTrip(1, 1, SEND, 2))
      .expect(replyAt(st, 1) == RResult(ok(1)) and witnessReplayAfterOutcomeUnknown)

  /// http-surface §8 D14: a receipt `in_progress` that the runner has not observed is not
  /// claimed: the reply is 503 `outcome_unknown`, not 409 `in_progress`.
  run trace_refused_in_progress_unobserved =
    init
      .then(send(1, 1, STOP))
      .then(submit(1))
      .then(writerStart)
      .then(writerFinish(Ok))
      .then(deadline(1))
      .then(grace(1))
      .then(replyTo(1))
      .expect(and {
        st.receipts.get(1).status == InProgress,
        replyAt(st, 0) == ROutcomeUnknown,
      })

  /// http-surface §2.5: a body not read by the admission deadline is 408 with nothing
  /// submitted.
  run trace_timeout_before_submit =
    init
      .then(send(1, 1, SEND))
      .then(deadline(1))
      .then(replyTo(1))
      .expect(replyAt(st, 0) == RTimeout and st.queue == [] and st.committed == Set())

  /// http-surface §2.8 "Why 408 is exact": a job still queued when the grace ends is 503,
  /// not 408; 408 only after the runner observed `Expired`, and a retry then runs the
  /// command.
  run trace_refused_timeout_while_queued =
    init
      .then(send(1, 1, SEND))
      .then(submit(1))
      .then(deadline(1))
      .then(grace(1))
      .then(replyTo(1))
      .expect(replyAt(st, 0) == ROutcomeUnknown)
      .then(send(1, 1, SEND))
      .then(submit(2))
      .then(deadline(2))
      .then(writerStart)
      .then(writerStart)
      .then(observe(2))
      .expect(st.attempts.get(2).progress == Done(OExpired))
      .then(replyTo(2))
      .expect(replyAt(st, 1) == RTimeout and st.committed == Set())
      .then(roundTrip(1, 1, SEND, 3))
      .expect(replyAt(st, 2) == RResult(ok(1)))

  /// mvp §2.4 "Restart": a crash while the effect runs leaves the receipt
  /// `unknown_after_crash`; a retry gets 409 `unknown_after_crash` and the effect never
  /// runs again.
  run trace_unknown_after_crash =
    init
      .then(send(1, 1, STOP))
      .then(submit(1))
      .then(writerStart)
      .then(writerFinish(Ok))
      .then(observe(1))
      .then(restart(false))
      .expect(st.receipts.get(1).status == UnknownAfterCrash)
      .then(roundTrip(1, 1, STOP, 2))
      .expect(replyAt(st, 0) == RUnknownAfterCrash and st.effectStarts.get(1) == 1)

  /// mvp §2.4 "Restart": the running intent job committed before the crash; the restart
  /// applies it (its receipt lookup is recorded) and marks the receipt
  /// `unknown_after_crash`. The effect never started.
  run trace_restart_after_committed_intent =
    init
      .then(send(1, 1, STOP))
      .then(submit(1))
      .then(writerStart)
      .then(restart(true))
      .expect(and {
        lookupNow(New),
        st.receipts.get(1).status == UnknownAfterCrash,
        st.effectStarts.get(1) == 0,
      })

  /// store §2.2: an intent job that ends `CommitUnknown` but committed leaves the receipt
  /// `in_progress` and the writer failed; the request gets 503 `outcome_unknown`, a retry
  /// 503 `store_failed`, and after the restart 409 `unknown_after_crash`. The effect never
  /// started.
  run trace_commit_unknown_then_restart =
    init
      .then(send(1, 1, STOP))
      .then(submit(1))
      .then(writerStart)
      .then(writerFinish(UnknownCommitted))
      .then(observe(1))
      .then(replyTo(1))
      .expect(replyAt(st, 0) == ROutcomeUnknown)
      .then(send(1, 1, STOP))
      .then(submit(2))
      .then(writerFail)
      .then(observe(2))
      .then(replyTo(2))
      .expect(replyAt(st, 1) == RStoreFailed)
      .then(restart(false))
      .then(roundTrip(1, 1, STOP, 3))
      .expect(replyAt(st, 2) == RUnknownAfterCrash and st.effectStarts.get(1) == 0)

  /// mvp §2.4 `Conflict`: another token, or the same token with another request, gets 409
  /// `conflict` whatever the receipt's state.
  run trace_conflict =
    init
      .then(send(1, 1, STOP))
      .then(submit(1))
      .then(writerStart)
      .then(writerFinish(Ok))
      .then(roundTrip(1, 2, STOP, 2))
      .expect(replyAt(st, 0) == RConflict)
      .then(roundTrip(1, 1, SEND, 3))
      .expect(replyAt(st, 0) == RConflict and replyAt(st, 1) == RConflict)
}
