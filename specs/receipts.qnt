// -*- mode: Bluespec; -*-
/**
 * Q-B, command side: command receipts keyed by `command_id` across retries, lost replies,
 * writer contention, and daemon restarts (docs/mvp-design.md §2.4 "Commands";
 * docs/design/http-surface.md §2.5 "Deadlines", §2.8, §8 D6, D14;
 * docs/design/formal-specs.md §7).
 *
 * Two layers, as in Q-A:
 *
 * - `checkReceipt` mirrors `crates/core/src/command.rs` `check_receipt`. Every step
 *   records its calls (`last.calls`), and the committed traces replay against the real
 *   function (crates/core/tests/quint_sync_traces.rs).
 * - The protocol layer is the contract, with four things kept apart: the request's
 *   admission deadline (`deadline`; the writer does not start its intent job after it),
 *   the end of the reply grace (`grace`), what the store made durable (`receipts`), and
 *   what the runner observed of the writer's reply (`writer` until `observe`, then
 *   `progress`). One writer runs jobs one at a time in order (store §2.2); a command's
 *   intent job looks the receipt up and, for `New`, inserts it `in_progress` with the
 *   intent in one transaction; the runner starts the effect once after it observes the
 *   intent committed with an effect pending, and completes the receipt in a later job;
 *   the request replies once the runner observed an outcome, or when the grace ends from
 *   what the runner observed by then (the runner goes on observing after the reply); a
 *   restart turns `in_progress` into `unknown_after_crash`.
 *
 * Abstracted: a request identity is a number (`hash`); hash 1 is a command completed in
 * its intent job (`send`), hash 2 a command with an effect (`stop`); a result is a number
 * with the stored HTTP status (200, or 409 for an effect that refused); a lost reply is a
 * client that retries anyway (`send` with the same `command_id`, token, and hash). The
 * intent's events are counted, not stored.
 * Not modelled: token checks and authorization refusals (the job writes nothing), `busy`
 * and queue-full replies, the completion budget's retries (a completion that cannot
 * commit is `CompletionFailed`), the shutdown drain, real time.
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

  // `CommandResult`: `n` stands for the body, `status` is its HTTP status.
  type Result = { n: int, status: int }
  type ReceiptStatus = InProgress | Completed(Result) | UnknownAfterCrash
  type Receipt = { token: int, hash: int, status: ReceiptStatus }
  type Existing = NoReceipt | Has(Receipt)
  type Lookup = New | LInProgress | Replay(Result) | Conflict | LUnknownAfterCrash

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

  type Job = Intent(int) | Complete({ cmd: int, attempt: int, result: Result })
  type WriterSlot = Idle | Busy(Job)
  // The writer's reply to the runner, before the runner observes it (store §2.2 outcomes).
  type WriterReply =
    | NoReply
    | WLookup(Lookup)
    | WCompleted(Result)
    | WPending
    | WExpired
    | WCommitUnknown
    | WFailed
    | WCompletionDone(Result)
    | WCompletionFailed
  // What the runner observed and published (`Progress`), with the store outcome.
  type Outcome =
    | OLookup(Lookup)
    | OCompleted(Result)
    | OExpired
    | OCommitUnknown
    | OFailed
    | OCompletionFailed
  type Progress = Submitted | IntentCommitted | Done(Outcome)
  // The HTTP reply (http §2.8 table).
  type Reply =
    | RResult(Result)
    | RInProgress
    | RConflict
    | RUnknownAfterCrash
    | RTimeout
    | ROutcomeUnknown
    | RStoreFailed
  // One request. `submitted`: its body was read and its intent job queued; `deadline`: its
  // admission deadline passed; `grace`: the reply grace after it ended; `live`: the daemon
  // that holds it has not restarted.
  type Attempt = {
    cmd: int, token: int, hash: int,
    submitted: bool, deadline: bool, grace: bool,
    writer: WriterReply, progress: Progress,
    live: bool, replied: bool,
  }
  // The writer's outcome for a job: committed, or `CommitUnknown` that did or did not commit.
  type WriteOutcome = Ok | UnknownCommitted | UnknownRolledBack

  // A reply with the facts at reply time: the durable receipt, what the runner observed,
  // whether the intent job had been submitted.
  type ReplyRecord = {
    attempt: int, cmd: int, token: int, hash: int, reply: Reply,
    receipt: Existing, observed: Progress, submitted: bool,
  }

  type State = {
    receipts: int -> Receipt,
    queue: List[Job],
    running: WriterSlot,
    // The writer's failed state (store §2.2): every later job fails until a restart.
    failed: bool,
    // Effects started and not yet finished, per command, with their runner's request.
    effects: int -> int,
    attempts: int -> Attempt,
    nextResult: int,
    // History the invariants read (ghosts).
    intents: int -> int,
    effectStarts: int -> int,
    creator: int -> { token: int, hash: int },
    stored: int -> Result,
    markedUnknown: Set[int],
    afterUnknown: Set[int],
    committed: Set[int],
    // Per attempt, what its runner had observed when the grace ended or it replied, whichever
    // came first (Submitted before any observation).
    byGrace: int -> Progress,
    replies: List[ReplyRecord],
  }

  type Call = CheckReceipt({ existing: Existing, token: int, hash: int, lookup: Lookup })

  type Action =
    | Init
    | Send({ cmd: int, token: int, hash: int })
    | Submit(int)
    | Deadline(int)
    | Grace(int)
    | WriterStart
    | WriterFinish(WriteOutcome)
    | WriterFail
    | Observe(int)
    | EffectDone({ cmd: int, ok: bool })
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
    byGrace: Map(),
    replies: [],
  }

  pure def existing(s: State, c: int): Existing = if (s.receipts.keys().contains(c)) Has(s.receipts.get(c)) else NoReceipt

  pure def setAttempt(s: State, a: int, f: Attempt => Attempt): State =
    { ...s, attempts: s.attempts.set(a, f(s.attempts.get(a))) }

  pure def setWriter(s: State, a: int, w: WriterReply): State = setAttempt(s, a, at => { ...at, writer: w })

  pure def seen(s: State, a: int, p: Progress): State = setAttempt(s, a, at => { ...at, progress: p })

  // The ghost: an observation made before the grace ended and before the reply.
  pure def noteByGrace(s: State, a: int): State =
    val at = s.attempts.get(a)
    if (at.grace or at.replied) s else { ...s, byGrace: s.byGrace.set(a, at.progress) }

  pure def without(m: a -> b, k: a): a -> b = m.keys().exclude(Set(k)).mapBy(x => m.get(x))

  type Out = { st: State, calls: List[Call] }

  /// The intent job's writes for `New`: the receipt `in_progress` (completed in the same
  /// job for `send`) with the intent, in one transaction.
  pure def commitIntent(s: State, a: int): State =
    val at = s.attempts.get(a)
    val r = { n: s.nextResult, status: 200 }
    {
      ...s,
      receipts: s.receipts.put(at.cmd, {
        token: at.token, hash: at.hash, status: if (withEffect(at.hash)) InProgress else Completed(r),
      }),
      intents: s.intents.set(at.cmd, s.intents.get(at.cmd) + 1),
      creator: s.creator.put(at.cmd, { token: at.token, hash: at.hash }),
      stored: if (withEffect(at.hash)) s.stored else s.stored.put(at.cmd, r),
      committed: s.committed.union(Set(a)),
      afterUnknown: if (s.markedUnknown.contains(at.cmd)) s.afterUnknown.union(Set(at.cmd)) else s.afterUnknown,
      nextResult: if (withEffect(at.hash)) s.nextResult else s.nextResult + 1,
    }

  /// The completion job's write.
  pure def commitCompletion(s: State, c: int, r: Result): State =
    { ...s, receipts: s.receipts.set(c, { ...s.receipts.get(c), status: Completed(r) }), stored: s.stored.put(c, r) }

  /// The runner starts the effect once, after it observed the intent committed.
  pure def startEffect(s: State, a: int): State =
    val c = s.attempts.get(a).cmd
    {
      ...s,
      effects: s.effects.put(c, a),
      effectStarts: s.effectStarts.set(c, s.effectStarts.get(c) + 1),
      afterUnknown: if (s.markedUnknown.contains(c)) s.afterUnknown.union(Set(c)) else s.afterUnknown,
    }

  /// The request's reply from what the runner observed (http §2.8; mvp §2.4).
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
  }

  type Finished = { ok: bool, o: Out }

  /// The running job ends with `w`: durable writes, and a reply the runner has not yet
  /// observed. `CommitUnknown` puts the writer in its failed state. A job that writes
  /// nothing (any lookup but `New`) ends `Ok`.
  pure def finish(s: State, w: WriteOutcome): Finished = match s.running {
    | Idle => { ok: false, o: { st: s, calls: [] } }
    | Busy(j) => match j {
        | Intent(a) => {
            val at = s.attempts.get(a)
            val ex = existing(s, at.cmd)
            val l = checkReceipt(ex, at.token, at.hash)
            val call = CheckReceipt({ existing: ex, token: at.token, hash: at.hash, lookup: l })
            val s1 = { ...s, running: Idle }
            if (l != New) { ok: w == Ok, o: { st: setWriter(s1, a, WLookup(l)), calls: [call] } }
            else {
              val s2 = if (w == UnknownRolledBack) s1 else commitIntent(s1, a)
              val s3 = match w {
                | Ok => setWriter(s2, a, if (withEffect(at.hash)) WPending else WCompleted({ n: s1.nextResult, status: 200 }))
                | _ => setWriter({ ...s2, failed: true }, a, WCommitUnknown)
              }
              { ok: true, o: { st: s3, calls: [call] } }
            }
          }
        | Complete(cj) => {
            val s1 = { ...s, running: Idle }
            val s2 = if (w == UnknownRolledBack) s1 else commitCompletion(s1, cj.cmd, cj.result)
            val s3 = match w {
              | Ok => setWriter(s2, cj.attempt, WCompletionDone(cj.result))
              | _ => setWriter({ ...s2, failed: true }, cj.attempt, WCompletionFailed)
            }
            { ok: true, o: { st: s3, calls: [] } }
          }
      }
  }

  // ---------------------------------------------------------------------------
  // Actions
  // ---------------------------------------------------------------------------

  action commit(a: Action, o: Out): bool = all {
    st' = o.st,
    last' = { act: a, calls: o.calls },
  }

  pure def start(s: State): Out = { st: s, calls: [] }

  action init = all { st' = INITIAL, last' = { act: Init, calls: [] } }

  /// A request (or a retry with the same `command_id`) arrives; its body is being read.
  action send(c: int, t: int, h: int): bool = all {
    st.attempts.keys().size() < MAX_ATTEMPTS,
    val a = st.attempts.keys().size() + 1
    commit(Send({ cmd: c, token: t, hash: h }), start({
      ...st,
      attempts: st.attempts.put(a, {
        cmd: c, token: t, hash: h, submitted: false, deadline: false, grace: false,
        writer: NoReply, progress: Submitted, live: true, replied: false,
      }),
      byGrace: st.byGrace.put(a, Submitted),
    })),
  }

  pure def has(s: State, a: int): bool = s.attempts.keys().contains(a)

  /// The body was read before the deadline: the runner is registered and the intent job
  /// submitted.
  action submit(a: int): bool = all {
    has(st, a),
    val at = st.attempts.get(a)
    all {
      at.live, not(at.submitted), not(at.deadline), not(at.replied),
      commit(Submit(a), start({ ...setAttempt(st, a, x => { ...x, submitted: true }), queue: st.queue.append(Intent(a)) })),
    },
  }

  /// The admission deadline passes: the writer will not start this request's intent job.
  action deadline(a: int): bool = all {
    has(st, a),
    val at = st.attempts.get(a)
    all {
      at.live, not(at.deadline), not(at.replied),
      commit(Deadline(a), start(setAttempt(st, a, x => { ...x, deadline: true }))),
    },
  }

  /// The request replies `r` from what its runner observed, and records the facts.
  pure def answer(s: State, a: int, r: Reply): State =
    val at = s.attempts.get(a)
    {
      ...setAttempt(s, a, x => { ...x, replied: true }),
      replies: s.replies.append({
        attempt: a, cmd: at.cmd, token: at.token, hash: at.hash, reply: r,
        receipt: existing(s, at.cmd), observed: at.progress, submitted: at.submitted,
      }),
    }

  /// The reply grace after the deadline ends (`write_reply_grace`): a submitted request
  /// that has not replied replies now, from what its runner observed by now; a writer
  /// reply observed later does not change it (PR7-CL-8: "taken at or after then" is 503).
  action grace(a: int): bool = all {
    has(st, a),
    val at = st.attempts.get(a)
    all {
      at.live, at.submitted, at.deadline, not(at.grace), not(at.replied),
      commit(Grace(a), start(answer(setAttempt(st, a, x => { ...x, grace: true }), a, replyOf(at.progress)))),
    },
  }

  /// The writer takes the next job; an intent job whose deadline passed replies `Expired`
  /// without running (store §2.2 step 3).
  action writerStart: bool = all {
    not(st.failed),
    st.running == Idle,
    st.queue.length() > 0,
    val j = head(st.queue)
    val s1 = { ...st, queue: tail(st.queue) }
    match j {
      | Intent(a) =>
        if (st.attempts.get(a).deadline) commit(WriterStart, start(setWriter(s1, a, WExpired)))
        else commit(WriterStart, start({ ...s1, running: Busy(j) }))
      | Complete(_) => commit(WriterStart, start({ ...s1, running: Busy(j) }))
    },
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
      | Intent(a) => commit(WriterFail, start(setWriter(s1, a, WFailed)))
      | Complete(cj) => commit(WriterFail, start(setWriter(s1, cj.attempt, WCompletionFailed)))
    },
  }

  /// The runner observes the writer's reply and publishes it; after an intent committed
  /// with its effect pending, it starts the effect, once.
  action observe(a: int): bool = all {
    has(st, a),
    val at = st.attempts.get(a)
    all {
      at.live,
      at.writer != NoReply,
      val s1 = setWriter(st, a, NoReply)
      commit(Observe(a), start(noteByGrace(match at.writer {
        | WPending => startEffect(seen(s1, a, IntentCommitted), a)
        | WLookup(l) => seen(s1, a, Done(OLookup(l)))
        | WCompleted(r) => seen(s1, a, Done(OCompleted(r)))
        | WExpired => seen(s1, a, Done(OExpired))
        | WCommitUnknown => seen(s1, a, Done(OCommitUnknown))
        | WFailed => seen(s1, a, Done(OFailed))
        | WCompletionDone(r) => seen(s1, a, Done(OCompleted(r)))
        | WCompletionFailed => seen(s1, a, Done(OCompletionFailed))
        | NoReply => st
      }, a))),
    },
  }

  /// The effect reports its terminal result (it ran, or it refused: a stored 409); the
  /// runner submits the completion.
  action effectDone(c: int, ok: bool): bool = all {
    st.effects.keys().contains(c),
    val a = st.effects.get(c)
    val r = { n: st.nextResult, status: if (ok) 200 else 409 }
    commit(EffectDone({ cmd: c, ok: ok }), start({
      ...st,
      effects: without(st.effects, c),
      queue: st.queue.append(Complete({ cmd: c, attempt: a, result: r })),
      nextResult: st.nextResult + 1,
    })),
  }

  /// The request replies once its runner is done, or once the grace ended; a body not
  /// read by the deadline is 408 with nothing submitted.
  action replyTo(a: int): bool = all {
    has(st, a),
    val at = st.attempts.get(a)
    val done = match at.progress { | Done(_) => at.submitted | _ => false }
    all {
      at.live, not(at.replied),
      done or (not(at.submitted) and at.deadline),
      val r = if (not(at.submitted)) RTimeout else replyOf(at.progress)
      commit(ReplyTo(a), start(answer(st, a, r))),
    },
  }

  /// A daemon crash and restart: the running job committed or not; every connection,
  /// runner, queued job, unobserved reply, and running effect is gone; every `in_progress`
  /// receipt becomes `unknown_after_crash` before commands are accepted again (mvp §2.4
  /// "Restart").
  action restart(runningCommitted: bool): bool = all {
    runningCommitted implies st.running != Idle,
    val o1: Out = match st.running {
      | Idle => start(st)
      | Busy(j) =>
        if (not(runningCommitted)) start(st)
        else match j {
          | Intent(a) => {
              val at = st.attempts.get(a)
              val ex = existing(st, at.cmd)
              val l = checkReceipt(ex, at.token, at.hash)
              {
                // The effect never started: the runner had not observed the commit.
                st: if (l != New) st else commitIntent(st, a),
                calls: [CheckReceipt({ existing: ex, token: at.token, hash: at.hash, lookup: l })],
              }
            }
          | Complete(cj) => start(commitCompletion(st, cj.cmd, cj.result))
        }
    }
    val s1 = o1.st
    val pending = s1.receipts.keys().filter(c => s1.receipts.get(c).status == InProgress)
    commit(Restart(runningCommitted), { st: {
      ...s1,
      receipts: s1.receipts.keys().mapBy(c => if (pending.contains(c)) { ...s1.receipts.get(c), status: UnknownAfterCrash } else s1.receipts.get(c)),
      markedUnknown: s1.markedUnknown.union(pending),
      queue: [],
      running: Idle,
      failed: false,
      effects: Map(),
      attempts: s1.attempts.keys().mapBy(a => { ...s1.attempts.get(a), live: false, writer: NoReply }),
    }, calls: o1.calls }),
  }

  // The attempts a step can act on: live and not replied (0 when there is none).
  pure def activeOf(s: State): Set[int] =
    val xs = s.attempts.keys().filter(a => s.attempts.get(a).live and not(s.attempts.get(a).replied))
    if (xs.size() == 0) Set(0) else xs
  // The runners with a writer reply to observe, replied or not (0 when there is none): a
  // runner outlives its request.
  pure def observableOf(s: State): Set[int] =
    val xs = s.attempts.keys().filter(a => s.attempts.get(a).live and s.attempts.get(a).writer != NoReply)
    if (xs.size() == 0) Set(0) else xs
  pure def anyOf(s: State): Set[int] = if (s.attempts.keys().size() == 0) Set(0) else s.attempts.keys()

  // Retries are frequent: a client keeps its `command_id`, token, and request. Time
  // (deadlines, graces) and restarts are rarer than work, so most attempts get a reply.
  action step = {
    nondet c = CMDS.oneOf()
    nondet t = TOKENS.oneOf()
    nondet h = HASHES.oneOf()
    nondet a = activeOf(st).oneOf()
    nondet r = anyOf(st).oneOf()
    nondet o = observableOf(st).oneOf()
    nondet w = Set(Ok, Ok, UnknownCommitted, UnknownRolledBack).oneOf()
    nondet ok = Bool.oneOf()
    nondet rc = Bool.oneOf()
    any {
      send(c, t, h),
      all { has(st, r), val at = st.attempts.get(r) send(at.cmd, at.token, at.hash) },
      all { has(st, r), val at = st.attempts.get(r) send(at.cmd, at.token, at.hash) },
      submit(a),
      submit(a),
      writerStart,
      writerStart,
      writerFinish(w),
      writerFinish(Ok),
      writerFail,
      observe(o),
      observe(o),
      effectDone(c, ok),
      replyTo(a),
      replyTo(a),
      any { deadline(a), grace(a), restart(rc) },
    }
  }

  // ---------------------------------------------------------------------------
  // Invariants (each cites its source)
  // ---------------------------------------------------------------------------

  pure def repliesOf(s: State): Set[ReplyRecord] = s.replies.indices().map(i => s.replies[i])

  pure def isCreator(s: State, r: ReplyRecord): bool =
    s.creator.keys().contains(r.cmd) and s.creator.get(r.cmd) == { token: r.token, hash: r.hash }

  /// mvp §2.4 "Insert", CL-8, PR7-CL-18: one `command_id` causes at most one intent.
  val oneIntentPerCommand = CMDS.forall(c => st.intents.get(c) <= 1)

  /// CL-8, PR7-CL-19: at most one effect per `command_id`, across retries and restarts.
  /// (That a healthy runner starts and completes the effect after it observed the intent
  /// committed is liveness, left to PR 7c and PR 17.)
  val oneEffectPerCommand = CMDS.forall(c => st.effectStarts.get(c) <= 1)

  /// mvp §2.4 "Insert": the receipt and the intent commit together or not at all.
  val receiptIffIntent = CMDS.forall(c => st.receipts.keys().contains(c) iff st.intents.get(c) >= 1)

  /// mvp §2.4 `ReceiptLookup`, CL-8: a stored result goes only to the token and request
  /// that created the receipt, and it is the stored one (status and body).
  val replayOnlyToCreator = repliesOf(st).forall(r => match r.reply {
    | RResult(res) => isCreator(st, r) and st.stored.keys().contains(r.cmd) and st.stored.get(r.cmd) == res
    | _ => true
  })

  /// mvp §2.4 `Conflict` "revealing nothing", http §2.8 "Receipt visibility": another
  /// token or request learns nothing about the receipt's state.
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
  /// from running": an attempt answered 408 never commits its intent, now or later.
  val timeoutOnlyWithoutIntent = repliesOf(st).forall(r => r.reply == RTimeout implies not(st.committed.contains(r.attempt)))

  /// http §2.8 "Why 408 is exact": 408 only for a body not read by the deadline (nothing
  /// submitted) or after the runner observed `Expired`.
  val timeoutOnlyWhenExpiredObserved = repliesOf(st).forall(r => r.reply == RTimeout implies
    (not(r.submitted) or r.observed == Done(OExpired)))

  /// http §8 D14 ("409 `in_progress` would claim a receipt that may never exist"): a 409
  /// `in_progress` only when the receipt exists at reply time and the runner observed it
  /// `in_progress` (its own intent committed, or the lookup found it).
  val inProgressOnlyWithReceipt = repliesOf(st).forall(r => r.reply == RInProgress implies and {
    r.receipt != NoReceipt,
    r.observed == IntentCommitted or r.observed == Done(OLookup(LInProgress)),
  })

  /// http §2.8, PR7-CL-8: a submitted request replies from what its runner observed by
  /// the end of the grace; an outcome observed later does not change the reply (it is 503
  /// `outcome_unknown` if nothing was observed by then).
  val replyFromObservedByGrace = repliesOf(st).forall(r => r.submitted implies r.reply == replyOf(st.byGrace.get(r.attempt)))

  val allInvariants = and {
    oneIntentPerCommand,
    oneEffectPerCommand,
    receiptIffIntent,
    replayOnlyToCreator,
    othersLearnNothing,
    unknownNeverReexecuted,
    noInProgressAfterRestart,
    timeoutOnlyWithoutIntent,
    timeoutOnlyWhenExpiredObserved,
    inProgressOnlyWithReceipt,
    replyFromObservedByGrace,
  }

  // Witnesses (`simulate` fails when one is never reached), from recorded replies.
  pure def replied(s: State, p: ReplyRecord => bool): bool = s.replies.indices().exists(i => p(s.replies[i]))
  def lookedUp(l: Lookup => bool): bool = last.calls.indices().exists(i => match last.calls[i] { | CheckReceipt(k) => l(k.lookup) })
  pure def sameRequest(x: ReplyRecord, y: ReplyRecord): bool = x.cmd == y.cmd and x.token == y.token and x.hash == y.hash
  pure def viaReplay(r: ReplyRecord): bool = match r.observed { | Done(o) => match o { | OLookup(l) => match l { | Replay(_) => true | _ => false } | _ => false } | _ => false }

  val witnessReplay = lookedUp(l => match l { | Replay(_) => true | _ => false })
  val witnessReplayNon200 = lookedUp(l => match l { | Replay(res) => res.status != 200 | _ => false })
  // A retry of the same request after 503 `outcome_unknown` gets the stored result.
  val witnessReplayAfterOutcomeUnknown = replied(st, r => viaReplay(r) and
    replied(st, u => sameRequest(u, r) and u.attempt < r.attempt and u.reply == ROutcomeUnknown))
  // http §8 D14: the intent is durable, the runner has not observed it, and the reply is 503.
  val witnessDurableUnobserved = replied(st, r => r.reply == ROutcomeUnknown and r.observed == Submitted and
    st.committed.contains(r.attempt) and r.receipt != NoReceipt)
  val witnessInProgress = replied(st, r => r.reply == RInProgress)
  // The runner observed its intent committed after its request replied, and started the
  // effect.
  val witnessEffectAfterReply = match last.act {
    | Observe(a) => st.attempts.get(a).replied and st.attempts.get(a).progress == IntentCommitted
    | _ => false
  }
  // The grace ended with nothing observed, and the runner observed `Expired` afterwards.
  val witnessExpiredAfterGrace = match last.act {
    | Observe(a) => st.attempts.get(a).grace and st.attempts.get(a).progress == Done(OExpired)
    | _ => false
  }
  val witnessDuplicateWhileInProgress = lookedUp(l => l == LInProgress)
  val witnessUnknownAfterCrash = replied(st, r => r.reply == RUnknownAfterCrash)
  val witnessConflict = replied(st, r => r.reply == RConflict)
  val witnessTimeoutBeforeSubmit = replied(st, r => r.reply == RTimeout and not(r.submitted))
  val witnessTimeoutThenIntent = replied(st, r => r.reply == RTimeout and r.submitted and
    st.committed.exists(a => a > r.attempt and st.attempts.get(a).cmd == r.cmd))
  val witnessCommitUnknownCommitted = match last.act { | WriterFinish(w) => w == UnknownCommitted | _ => false }
}
