// -*- mode: Bluespec; -*-
/**
 * Q-B, command side: command receipts keyed by `command_id` across retries, lost replies,
 * writer contention, and daemon restarts (docs/mvp-design.md §2.4 "Commands";
 * docs/design/http-surface.md §2.8, §8 D6, D14; docs/design/formal-specs.md §7).
 *
 * Two layers, as in Q-A:
 *
 * - `checkReceipt` mirrors `crates/core/src/command.rs` `check_receipt`. Every step
 *   records its calls (`last.calls`), and the committed traces replay against the real
 *   function (crates/core/tests/quint_sync_traces.rs).
 * - The protocol layer is the contract: one writer runs jobs one at a time in order
 *   (store §2.2); a command's intent job looks the receipt up and, for `New`, inserts it
 *   `in_progress` with the intent in one transaction; a registered runner starts the
 *   effect once and completes the receipt in a later job; the request replies from the
 *   progress it sees by its deadline plus the reply grace; a restart turns `in_progress`
 *   into `unknown_after_crash`.
 *
 * Abstracted: a request identity is a number (`hash`); hash 1 is a command completed in
 * its intent job (`send`), hash 2 a command with an effect (`stop`); a result is a number
 * standing for the stored status and body; the deadline is an explicit `tick`; a lost
 * reply is a client that retries anyway (`send` with the same `command_id`, token, and
 * hash). The intent's events are counted, not stored.
 * Not modelled: token checks and authorization refusals (the job writes nothing), `busy`
 * and queue-full replies, the completion budget's retries (a completion that cannot
 * commit is `CompletionFailed`), the shutdown drain.
 */
module receipts {
  // ---------------------------------------------------------------------------
  // Bounds
  // ---------------------------------------------------------------------------

  pure val CMDS = Set(1, 2)
  pure val TOKENS = Set(1, 2)
  pure val HASHES = Set(1, 2)
  pure val MAX_ATTEMPTS = 6

  // ---------------------------------------------------------------------------
  // Reducer layer: crates/core/src/command.rs `check_receipt`
  // ---------------------------------------------------------------------------

  type ReceiptStatus = InProgress | Completed(int) | UnknownAfterCrash
  type Receipt = { token: int, hash: int, status: ReceiptStatus }
  type Existing = NoReceipt | Has(Receipt)
  type Lookup = New | LInProgress | Replay(int) | Conflict | LUnknownAfterCrash

  /// `command::check_receipt`: ownership and the request first, then the status.
  pure def checkReceipt(ex: Existing, token: int, hash: int): Lookup = match ex {
    | NoReceipt => New
    | Has(r) =>
      if (r.token != token or r.hash != hash) Conflict
      else match r.status {
        | InProgress => LInProgress
        | Completed(res) => Replay(res)
        | UnknownAfterCrash => LUnknownAfterCrash
      }
  }

  // ---------------------------------------------------------------------------
  // Protocol layer
  // ---------------------------------------------------------------------------

  pure val SEND = 1
  pure def withEffect(hash: int): bool = hash != SEND

  type Job = Intent(int) | Complete({ cmd: int, attempt: int, result: int })
  type WriterSlot = Idle | Busy(Job)
  // What the runner publishes (`Progress`), with the store outcome of the intent job.
  type Outcome =
    | OLookup(Lookup)
    | OCompleted(int)
    | OExpired
    | OCommitUnknown
    | OFailed
    | OCompletionFailed
  type Progress = Submitted | JobRunning | IntentCommitted | Done(Outcome)
  // The HTTP reply (http-surface §2.8 table).
  type Reply =
    | RResult(int)
    | RInProgress
    | RConflict
    | RUnknownAfterCrash
    | RTimeout
    | ROutcomeUnknown
    | RStoreFailed
  type Attempt = { cmd: int, token: int, hash: int, progress: Progress, deadlinePassed: bool, live: bool, replied: bool }
  // The writer's outcome for a job: committed, or `CommitUnknown` that did or did not commit.
  type WriteOutcome = Ok | UnknownCommitted | UnknownRolledBack

  type ReplyRecord = { attempt: int, cmd: int, token: int, hash: int, reply: Reply, receipt: Existing }

  type State = {
    receipts: int -> Receipt,
    queue: List[Job],
    running: WriterSlot,
    // The writer's failed state (store §2.2): every later job fails until a restart.
    failed: bool,
    // Effects started and not yet finished, per command.
    effects: int -> { attempt: int, result: int },
    attempts: int -> Attempt,
    nextResult: int,
    // History the invariants read (ghosts).
    intents: int -> int,
    effectStarts: int -> int,
    creator: int -> { token: int, hash: int },
    stored: int -> int,
    markedUnknown: Set[int],
    afterUnknown: Set[int],
    committed: Set[int],
    replies: List[ReplyRecord],
  }

  type Call = CheckReceipt({ existing: Existing, token: int, hash: int, lookup: Lookup })

  type Action =
    | Init
    | Send({ cmd: int, token: int, hash: int })
    | Tick(int)
    | WriterStart
    | WriterFinish(WriteOutcome)
    | WriterFail
    | EffectDone(int)
    | ReplyTo(int)
    | Restart(bool)

  type Last = { act: Action, calls: List[Call] }

  var st: State
  var last: Last

  pure val INITIAL: State = {
    receipts: Map(),
    queue: [],
    running: Idle,
    failed: false,
    effects: Map(),
    attempts: Map(),
    nextResult: 1,
    intents: CMDS.mapBy(_ => 0),
    effectStarts: CMDS.mapBy(_ => 0),
    creator: Map(),
    stored: Map(),
    markedUnknown: Set(),
    afterUnknown: Set(),
    committed: Set(),
    replies: [],
  }

  pure def existing(s: State, c: int): Existing = if (s.receipts.keys().contains(c)) Has(s.receipts.get(c)) else NoReceipt

  pure def setProgress(s: State, a: int, p: Progress): State =
    { ...s, attempts: s.attempts.set(a, { ...s.attempts.get(a), progress: p }) }

  pure def without(m: a -> b, k: a): a -> b = m.keys().exclude(Set(k)).mapBy(x => m.get(x))

  type Out = { st: State, calls: List[Call] }

  /// The intent job's writes for `New`: the receipt `in_progress` (completed in the same
  /// job for `send`) with the intent, in one transaction.
  pure def commitIntent(s: State, a: int, r: int): State =
    val at = s.attempts.get(a)
    val status = if (withEffect(at.hash)) InProgress else Completed(r)
    {
      ...s,
      receipts: s.receipts.put(at.cmd, { token: at.token, hash: at.hash, status: status }),
      intents: s.intents.set(at.cmd, s.intents.get(at.cmd) + 1),
      creator: s.creator.put(at.cmd, { token: at.token, hash: at.hash }),
      stored: if (withEffect(at.hash)) s.stored else s.stored.put(at.cmd, r),
      committed: s.committed.union(Set(a)),
      afterUnknown: if (s.markedUnknown.contains(at.cmd)) s.afterUnknown.union(Set(at.cmd)) else s.afterUnknown,
      nextResult: s.nextResult + 1,
    }

  /// The completion job's write.
  pure def commitCompletion(s: State, c: int, r: int): State =
    { ...s, receipts: s.receipts.set(c, { ...s.receipts.get(c), status: Completed(r) }), stored: s.stored.put(c, r) }

  /// The runner starts the effect once, after the intent committed.
  pure def startEffect(s: State, a: int, r: int): State =
    val c = s.attempts.get(a).cmd
    {
      ...s,
      effects: s.effects.put(c, { attempt: a, result: r }),
      effectStarts: s.effectStarts.set(c, s.effectStarts.get(c) + 1),
      afterUnknown: if (s.markedUnknown.contains(c)) s.afterUnknown.union(Set(c)) else s.afterUnknown,
    }

  /// The request's reply from the progress it sees (http-surface §2.8; mvp §2.4).
  pure def replyOf(p: Progress): Reply = match p {
    | Done(o) => match o {
        | OLookup(l) => match l {
            | New => ROutcomeUnknown
            | LInProgress => RInProgress
            | Replay(r) => RResult(r)
            | Conflict => RConflict
            | LUnknownAfterCrash => RUnknownAfterCrash
          }
        | OCompleted(r) => RResult(r)
        | OExpired => RTimeout
        | OCommitUnknown => ROutcomeUnknown
        | OFailed => RStoreFailed
        | OCompletionFailed => ROutcomeUnknown
      }
    | IntentCommitted => RInProgress
    | Submitted => ROutcomeUnknown
    | JobRunning => ROutcomeUnknown
  }

  // ---------------------------------------------------------------------------
  // Actions
  // ---------------------------------------------------------------------------

  action commit(a: Action, o: Out): bool = all {
    st' = o.st,
    last' = { act: a, calls: o.calls },
  }

  action init = all { st' = INITIAL, last' = { act: Init, calls: [] } }

  /// A request (or a retry with the same `command_id`): the runner is registered and the
  /// intent job submitted.
  action send(c: int, t: int, h: int): bool = all {
    st.attempts.keys().size() < MAX_ATTEMPTS,
    val a = st.attempts.keys().size() + 1
    commit(Send({ cmd: c, token: t, hash: h }), { st: {
      ...st,
      attempts: st.attempts.put(a, { cmd: c, token: t, hash: h, progress: Submitted, deadlinePassed: false, live: true, replied: false }),
      queue: st.queue.append(Intent(a)),
    }, calls: [] }),
  }

  /// The request's deadline passes.
  action tick(a: int): bool = all {
    st.attempts.keys().contains(a),
    val at = st.attempts.get(a)
    all {
      at.live, not(at.replied), not(at.deadlinePassed),
      commit(Tick(a), { st: { ...st, attempts: st.attempts.set(a, { ...at, deadlinePassed: true }) }, calls: [] }),
    },
  }

  /// The writer takes the next job; an intent job whose deadline passed is `Expired`
  /// without running (store §2.2 step 3).
  action writerStart: bool = all {
    not(st.failed),
    st.running == Idle,
    st.queue.length() > 0,
    val j = head(st.queue)
    val s1 = { ...st, queue: tail(st.queue) }
    match j {
      | Intent(a) =>
        if (st.attempts.get(a).deadlinePassed) commit(WriterStart, { st: setProgress(s1, a, Done(OExpired)), calls: [] })
        else commit(WriterStart, { st: setProgress({ ...s1, running: Busy(j) }, a, JobRunning), calls: [] })
      | Complete(_) => commit(WriterStart, { st: { ...s1, running: Busy(j) }, calls: [] })
    },
  }

  type Finished = { ok: bool, o: Out }

  /// The running job ends with `w`. `CommitUnknown` puts the writer in its failed state.
  /// A job that writes nothing (any lookup but `New`) ends `Ok`.
  pure def finish(s: State, w: WriteOutcome): Finished = match s.running {
    | Idle => { ok: false, o: { st: s, calls: [] } }
    | Busy(j) => match j {
        | Intent(a) => {
            val at = s.attempts.get(a)
            val ex = existing(s, at.cmd)
            val l = checkReceipt(ex, at.token, at.hash)
            val call = CheckReceipt({ existing: ex, token: at.token, hash: at.hash, lookup: l })
            val s1 = { ...s, running: Idle }
            val r = s.nextResult
            if (l != New) { ok: w == Ok, o: { st: setProgress(s1, a, Done(OLookup(l))), calls: [call] } }
            else {
              val s2 = if (w == UnknownRolledBack) s1 else commitIntent(s1, a, r)
              val s3 = match w {
                | Ok => if (withEffect(at.hash)) setProgress(startEffect(s2, a, r), a, IntentCommitted) else setProgress(s2, a, Done(OCompleted(r)))
                | _ => setProgress({ ...s2, failed: true }, a, Done(OCommitUnknown))
              }
              { ok: true, o: { st: s3, calls: [call] } }
            }
          }
        | Complete(cj) => {
            val s1 = { ...s, running: Idle }
            val s2 = if (w == UnknownRolledBack) s1 else commitCompletion(s1, cj.cmd, cj.result)
            val waiting = s.attempts.get(cj.attempt).progress == IntentCommitted
            val s3 = match w {
              | Ok => if (waiting) setProgress(s2, cj.attempt, Done(OCompleted(cj.result))) else s2
              | _ => {
                  val f = { ...s2, failed: true }
                  if (waiting) setProgress(f, cj.attempt, Done(OCompletionFailed)) else f
                }
            }
            { ok: true, o: { st: s3, calls: [] } }
          }
      }
  }

  action writerFinish(w: WriteOutcome): bool = all {
    val f = finish(st, w)
    all { f.ok, commit(WriterFinish(w), f.o) },
  }

  /// A failed writer answers every queued job `Failed`.
  action writerFail: bool = all {
    st.failed,
    st.queue.length() > 0,
    val s1 = { ...st, queue: tail(st.queue) }
    match head(st.queue) {
      | Intent(a) => commit(WriterFail, { st: setProgress(s1, a, Done(OFailed)), calls: [] })
      | Complete(cj) => commit(WriterFail, {
          st: if (st.attempts.get(cj.attempt).progress == IntentCommitted) setProgress(s1, cj.attempt, Done(OCompletionFailed)) else s1,
          calls: [],
        })
    },
  }

  /// The effect reports its terminal result; the runner submits the completion.
  action effectDone(c: int): bool = all {
    st.effects.keys().contains(c),
    val e = st.effects.get(c)
    commit(EffectDone(c), { st: {
      ...st,
      effects: without(st.effects, c),
      queue: st.queue.append(Complete({ cmd: c, attempt: e.attempt, result: e.result })),
    }, calls: [] }),
  }

  /// The request replies once its runner is done or its deadline plus the grace passed.
  action replyTo(a: int): bool = all {
    st.attempts.keys().contains(a),
    val at = st.attempts.get(a)
    val ready = match at.progress { | Done(_) => true | _ => at.deadlinePassed }
    all {
      at.live, not(at.replied), ready,
      commit(ReplyTo(a), { st: {
        ...st,
        attempts: st.attempts.set(a, { ...at, replied: true }),
        replies: st.replies.append({ attempt: a, cmd: at.cmd, token: at.token, hash: at.hash, reply: replyOf(at.progress), receipt: existing(st, at.cmd) }),
      }, calls: [] }),
    },
  }

  /// A daemon crash and restart: the running job committed or not; every connection,
  /// queued job, and running effect is gone; every `in_progress` receipt becomes
  /// `unknown_after_crash` before commands are accepted again (mvp §2.4 "Restart").
  action restart(runningCommitted: bool): bool = all {
    runningCommitted implies st.running != Idle,
    val s1 = match st.running {
      | Idle => st
      | Busy(j) =>
        if (not(runningCommitted)) st
        else match j {
          | Intent(a) => {
              val at = st.attempts.get(a)
              if (checkReceipt(existing(st, at.cmd), at.token, at.hash) != New) st
              else {
                // The effect never started: the runner had not seen the commit.
                commitIntent(st, a, st.nextResult)
              }
            }
          | Complete(cj) => commitCompletion(st, cj.cmd, cj.result)
        }
    }
    val pending = s1.receipts.keys().filter(c => s1.receipts.get(c).status == InProgress)
    commit(Restart(runningCommitted), { st: {
      ...s1,
      receipts: s1.receipts.keys().mapBy(c => if (pending.contains(c)) { ...s1.receipts.get(c), status: UnknownAfterCrash } else s1.receipts.get(c)),
      markedUnknown: s1.markedUnknown.union(pending),
      queue: [],
      running: Idle,
      failed: false,
      effects: Map(),
      attempts: s1.attempts.keys().mapBy(a => { ...s1.attempts.get(a), live: false }),
    }, calls: [] }),
  }

  pure def someAttempt(s: State): Set[int] = if (s.attempts.keys().size() == 0) Set(0) else s.attempts.keys()

  // Retries are frequent: a client keeps its `command_id`, token, and request.
  action clientStep = {
    nondet c = CMDS.oneOf()
    nondet t = TOKENS.oneOf()
    nondet h = HASHES.oneOf()
    nondet a = someAttempt(st).oneOf()
    any {
      send(c, t, h),
      all { st.attempts.keys().contains(a), val at = st.attempts.get(a) send(at.cmd, at.token, at.hash) },
    }
  }

  action writerStep = {
    nondet w = Set(Ok, Ok, UnknownCommitted, UnknownRolledBack).oneOf()
    any { writerStart, writerFinish(Ok), writerFinish(w), writerFail }
  }

  action requestStep = {
    nondet a = someAttempt(st).oneOf()
    nondet c = CMDS.oneOf()
    any { replyTo(a), tick(a), effectDone(c) }
  }

  action step = any {
    clientStep,
    writerStep,
    writerStep,
    requestStep,
    requestStep,
    nondet rc = Bool.oneOf()
    restart(rc),
  }

  // ---------------------------------------------------------------------------
  // Invariants (each cites its source)
  // ---------------------------------------------------------------------------

  pure def repliesOf(s: State): Set[ReplyRecord] = s.replies.indices().map(i => s.replies[i])

  pure def isCreator(s: State, r: ReplyRecord): bool =
    s.creator.keys().contains(r.cmd) and s.creator.get(r.cmd) == { token: r.token, hash: r.hash }

  /// mvp §2.4 "Insert", CL-8, PR7-CL-18: one `command_id` causes at most one intent.
  val oneIntentPerCommand = CMDS.forall(c => st.intents.get(c) <= 1)

  /// CL-8, PR7-CL-19 "its effect shall start exactly once": at most one effect per
  /// `command_id`, across retries and restarts.
  val oneEffectPerCommand = CMDS.forall(c => st.effectStarts.get(c) <= 1)

  /// mvp §2.4 "Insert": the receipt and the intent commit together or not at all.
  val receiptIffIntent = CMDS.forall(c => st.receipts.keys().contains(c) iff st.intents.get(c) >= 1)

  /// mvp §2.4 `ReceiptLookup`, CL-8: a stored result goes only to the token and request
  /// that created the receipt, and it is the stored one.
  val replayOnlyToCreator = repliesOf(st).forall(r => match r.reply {
    | RResult(res) => isCreator(st, r) and st.stored.keys().contains(r.cmd) and st.stored.get(r.cmd) == res
    | _ => true
  })

  /// mvp §2.4 `Conflict` "revealing nothing", http-surface §2.8 "Receipt visibility":
  /// another token or request learns nothing about the receipt's state.
  val othersLearnNothing = repliesOf(st).forall(r => match r.reply {
    | RInProgress => isCreator(st, r)
    | RUnknownAfterCrash => isCreator(st, r)
    | _ => true
  })

  /// mvp §2.4 "Restart": an `unknown_after_crash` command is never re-executed: no intent
  /// or effect for it afterwards, and its receipt stays `unknown_after_crash`.
  val unknownNeverReexecuted = and {
    st.afterUnknown == Set(),
    st.markedUnknown.forall(c => st.receipts.get(c).status == UnknownAfterCrash),
  }

  /// mvp §2.4 "Restart": no receipt is `in_progress` right after a restart.
  val noInProgressAfterRestart = match last.act {
    | Restart(_) => st.receipts.keys().forall(c => st.receipts.get(c).status != InProgress)
    | _ => true
  }

  /// mvp §2.4 "A 408 describes the current attempt only: its deadline prevented its intent
  /// from running"; http-surface §2.8 "Why 408 is exact": an attempt answered 408 never
  /// commits its intent, now or later.
  val timeoutOnlyWithoutIntent = repliesOf(st).forall(r => r.reply == RTimeout implies not(st.committed.contains(r.attempt)))

  /// http-surface §8 D14 ("409 `in_progress` would claim a receipt that may never exist"):
  /// a 409 `in_progress` is answered only when the receipt exists at reply time (it may
  /// have completed since the job read it; the retry then replays).
  val inProgressOnlyWithReceipt = repliesOf(st).forall(r => r.reply == RInProgress implies r.receipt != NoReceipt)

  val allInvariants = and {
    oneIntentPerCommand,
    oneEffectPerCommand,
    receiptIffIntent,
    replayOnlyToCreator,
    othersLearnNothing,
    unknownNeverReexecuted,
    noInProgressAfterRestart,
    timeoutOnlyWithoutIntent,
    inProgressOnlyWithReceipt,
  }

  // Witnesses (`simulate` fails when one is never reached).
  pure def replied(s: State, p: ReplyRecord => bool): bool = s.replies.indices().exists(i => p(s.replies[i]))
  pure def lookedUp(cs: List[Call], l: Lookup => bool): bool = cs.indices().exists(i => match cs[i] { | CheckReceipt(k) => l(k.lookup) })

  val witnessReplay = lookedUp(last.calls, l => match l { | Replay(_) => true | _ => false })
  val witnessReplayAfterOutcomeUnknown = replied(st, r => match r.reply { | RResult(_) => true | _ => false } and
    replied(st, u => u.cmd == r.cmd and u.attempt < r.attempt and u.reply == ROutcomeUnknown))
  val witnessInProgress = replied(st, r => r.reply == RInProgress)
  val witnessDuplicateWhileInProgress = lookedUp(last.calls, l => l == LInProgress)
  val witnessUnknownAfterCrash = replied(st, r => r.reply == RUnknownAfterCrash)
  val witnessConflict = replied(st, r => r.reply == RConflict)
  val witnessTimeoutThenIntent = replied(st, r => r.reply == RTimeout and
    st.committed.exists(a => a > r.attempt and st.attempts.get(a).cmd == r.cmd))
  val witnessCommitUnknownCommitted = match last.act { | WriterFinish(w) => w == UnknownCommitted | _ => false }
}
