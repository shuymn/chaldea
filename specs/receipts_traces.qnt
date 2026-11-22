// -*- mode: Bluespec; -*-
/**
 * Named deterministic traces of `receipts` (docs/design/formal-specs.md §7). Each
 * `trace_*` run is exported as ITF to `specs/traces/receipts/` and its `check_receipt`
 * calls are replayed against the real function by crates/core/tests/quint_sync_traces.rs.
 *
 * - `trace_refused_*` runs end where the contract refuses to answer as a reviewer might
 *   expect (a 408 only once the writer reported `Expired`).
 *
 * Attempt ids count from 1 in send order; results count from 1 in commit order.
 */
module receipts_traces {
  import receipts.* from "./receipts"

  pure val STOP = 2

  pure def replyAt(s: State, i: int): Reply = s.replies[i].reply
  def lookupNow(l: Lookup): bool = last.calls.indices().exists(i => match last.calls[i] { | CheckReceipt(k) => k.lookup == l })

  /// mvp §2.4: `send` completes in its intent job; a retry after a lost reply replays the
  /// stored result, and the intent ran once.
  run trace_send_replayed =
    init
      .then(send(1, 1, SEND))
      .then(writerStart)
      .then(writerFinish(Ok))
      .expect(lookupNow(New))
      .then(replyTo(1))
      .expect(replyAt(st, 0) == RResult(1))
      .then(send(1, 1, SEND))
      .then(writerStart)
      .then(writerFinish(Ok))
      .expect(lookupNow(Replay(1)))
      .then(replyTo(2))
      .expect(replyAt(st, 1) == RResult(1) and st.intents.get(1) == 1)

  /// mvp §2.4 "Concurrent duplicates": two requests with one `command_id` queue two jobs;
  /// the second reads the first's receipt.
  run trace_concurrent_duplicates =
    init
      .then(send(1, 1, SEND))
      .then(send(1, 1, SEND))
      .then(writerStart)
      .then(writerFinish(Ok))
      .then(writerStart)
      .then(writerFinish(Ok))
      .expect(lookupNow(Replay(1)) and st.intents.get(1) == 1)

  /// mvp §2.4 "Completion": a command with an effect stays `in_progress` until the effect
  /// reports; the request and a retry meanwhile get 409 `in_progress`; after completion a
  /// retry replays.
  run trace_effect_in_progress_then_replay =
    init
      .then(send(1, 1, STOP))
      .then(writerStart)
      .then(writerFinish(Ok))
      .then(tick(1))
      .then(replyTo(1))
      .expect(replyAt(st, 0) == RInProgress)
      .then(send(1, 1, STOP))
      .then(writerStart)
      .then(writerFinish(Ok))
      .expect(lookupNow(LInProgress))
      .then(replyTo(2))
      .expect(replyAt(st, 1) == RInProgress)
      .then(effectDone(1))
      .then(writerStart)
      .then(writerFinish(Ok))
      .then(send(1, 1, STOP))
      .then(writerStart)
      .then(writerFinish(Ok))
      .then(replyTo(3))
      .expect(replyAt(st, 2) == RResult(1) and st.effectStarts.get(1) == 1)

  /// http-surface §8 D14: the writer has not replied by the deadline plus the grace, so the
  /// reply is 503 `outcome_unknown`; the intent then commits, and the retry replays it.
  run trace_outcome_unknown_then_replay =
    init
      .then(send(1, 1, SEND))
      .then(writerStart)
      .then(tick(1))
      .then(replyTo(1))
      .expect(replyAt(st, 0) == ROutcomeUnknown)
      .then(writerFinish(Ok))
      .then(send(1, 1, SEND))
      .then(writerStart)
      .then(writerFinish(Ok))
      .then(replyTo(2))
      .expect(replyAt(st, 1) == RResult(1))

  /// http-surface §2.8 "Why 408 is exact": a job still queued at reply time is 503, not
  /// 408; 408 only after the writer reported `Expired`, and a retry then runs the command.
  run trace_refused_timeout_while_queued =
    init
      .then(send(1, 1, SEND))
      .then(tick(1))
      .then(replyTo(1))
      .expect(replyAt(st, 0) == ROutcomeUnknown)
      .then(send(1, 1, SEND))
      .then(tick(2))
      .then(writerStart)
      .expect(st.attempts.get(1).progress == Done(OExpired))
      .then(writerStart)
      .then(replyTo(2))
      .expect(replyAt(st, 1) == RTimeout and st.committed == Set())
      .then(send(1, 1, SEND))
      .then(writerStart)
      .then(writerFinish(Ok))
      .then(replyTo(3))
      .expect(replyAt(st, 2) == RResult(1))

  /// mvp §2.4 "Restart": a crash while the effect runs leaves the receipt
  /// `unknown_after_crash`; a retry gets 409 `unknown_after_crash` and the effect never
  /// runs again.
  run trace_unknown_after_crash =
    init
      .then(send(1, 1, STOP))
      .then(writerStart)
      .then(writerFinish(Ok))
      .then(restart(false))
      .expect(st.receipts.get(1).status == UnknownAfterCrash)
      .then(send(1, 1, STOP))
      .then(writerStart)
      .then(writerFinish(Ok))
      .expect(lookupNow(LUnknownAfterCrash))
      .then(replyTo(2))
      .expect(replyAt(st, 0) == RUnknownAfterCrash and st.effectStarts.get(1) == 1)

  /// store §2.2: an intent job that ends `CommitUnknown` but committed leaves the receipt
  /// `in_progress` and the writer failed; the request gets 503 `outcome_unknown`, a retry
  /// 503 `store_failed`, and after the restart 409 `unknown_after_crash`. The effect never
  /// started.
  run trace_commit_unknown_then_restart =
    init
      .then(send(1, 1, STOP))
      .then(writerStart)
      .then(writerFinish(UnknownCommitted))
      .then(replyTo(1))
      .expect(replyAt(st, 0) == ROutcomeUnknown)
      .then(send(1, 1, STOP))
      .then(writerFail)
      .then(replyTo(2))
      .expect(replyAt(st, 1) == RStoreFailed)
      .then(restart(false))
      .then(send(1, 1, STOP))
      .then(writerStart)
      .then(writerFinish(Ok))
      .then(replyTo(3))
      .expect(replyAt(st, 2) == RUnknownAfterCrash and st.effectStarts.get(1) == 0)

  /// mvp §2.4 `Conflict`: another token, or the same token with another request, gets 409
  /// `conflict` whatever the receipt's state.
  run trace_conflict =
    init
      .then(send(1, 1, STOP))
      .then(writerStart)
      .then(writerFinish(Ok))
      .then(send(1, 2, STOP))
      .then(send(1, 1, SEND))
      .then(writerStart)
      .then(writerFinish(Ok))
      .expect(lookupNow(Conflict))
      .then(writerStart)
      .then(writerFinish(Ok))
      .expect(lookupNow(Conflict))
      .then(replyTo(2))
      .then(replyTo(3))
      .expect(replyAt(st, 0) == RConflict and replyAt(st, 1) == RConflict)
}
