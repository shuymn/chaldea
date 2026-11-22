// -*- mode: Bluespec; -*-
/**
 * Q-A: the input state machine, the three-axis thread state with attention, and the
 * send gate of one thread, coupled across launches (docs/mvp-design.md §2.2, §2.3, §2.9;
 * docs/design/formal-specs.md).
 *
 * Two layers:
 *
 * - The reducer layer mirrors the pure functions of crates/core exactly: `reduce`
 *   (state.rs), `stepInput` (input.rs `step`) and `gate` (input.rs). Every model step
 *   records the calls it made into them (`last.calls`), so a committed trace can be
 *   replayed against the real functions (crates/core/tests/quint_traces.rs).
 * - The supervisor layer is the design contract: which events the daemon may feed, in
 *   which order, and with which guards. Its guards are the obligations PR 10 implements.
 *
 * Abstracted: stream positions are one monotonic counter `seq`; the PTY writer is a list
 * of counted writes per launch (its position is the list length) instead of the PR 2
 * writer's `last_started` counters and `Basis`; prompt text is abstract (a hook names
 * the in-flight inputs whose expected form it equals); time is explicit timer actions.
 * Deferred to PR 10 (not modelled): the writer lock and the concrete `Basis`, timer
 * durations, sid scans and descendant kills, worktree and close races.
 */
module thread_input {
  // ---------------------------------------------------------------------------
  // Bounds
  // ---------------------------------------------------------------------------

  pure val MAX_LAUNCHES = 2
  pure val MAX_INPUTS = 3
  pure val MAX_CARDS = 2
  pure val INPUT_IDS = 1.to(MAX_INPUTS)
  // Every VT reply is a counted write: the default policy (`ClassSet::ALL`,
  // docs/design/pty-runtime.md §2.3 "Basis", D3).
  pure val REPLIES_COUNTED = true

  // ---------------------------------------------------------------------------
  // Reducer layer: crates/core/src/state.rs
  // ---------------------------------------------------------------------------

  type Proc = Starting | Running | Stopping | Exited
  type Turn = TurnUnknown | Idle | TurnRunning | AwaitingApproval
  // A subset of `TaskState`: the task axis only has to show it changes on `TaskSet` alone.
  type Task = TaskNone | TaskActive | TaskCompleted
  type Cause =
    | SessionStartTimeout
    | StartupStall
    | InputUnconfirmed(int)
    | InputGateTimeout(int)
    | ResumeBlocked
  type Evidence = SessionStart | StopSettled | PostDecision | OwnerAsserted
  type ExitReason = ExitNormal | ExitStopped | ExitSpawnFailed | ExitDaemonRestart | ExitUnknownAfterCrash

  type StateInput =
    | Reserved(bool)
    | Spawned
    | SessionBound
    | StopRequested
    | ExitedBy(ExitReason)
    | DeathConfirmed
    | ReadinessObserved
    | PromptSubmitted
    | Interrupted
    | TurnStopped
    | ApprovalOpened
    | ApprovalsAllResolved
    | AttentionRaised(Cause)
    | AttentionCleared(Cause)
    | TaskSet(Task)

  type ThreadState = { process: Proc, turn: Turn, task: Task, attention: Set[Cause] }

  // `StatusChanged`; `cause` is the snake_case `StatusCause` of the input.
  type Change =
    | ProcessChanged({ from: Proc, to: Proc, cause: str })
    | TurnChanged({ from: Turn, to: Turn, cause: str })
    | TaskChanged({ from: Task, to: Task, cause: str })
    | AttentionChanged({ from: Set[Cause], to: Set[Cause], cause: str })

  type Reduced = { ok: bool, next: ThreadState, changes: List[Change] }

  pure def causeOf(i: StateInput): str = match i {
    | Reserved(_) => "reserved"
    | Spawned => "spawned"
    | SessionBound => "session_bound"
    | StopRequested => "stop_requested"
    | ExitedBy(_) => "exited"
    | DeathConfirmed => "death_confirmed"
    | ReadinessObserved => "readiness_observed"
    | PromptSubmitted => "prompt_submitted"
    | Interrupted => "interrupted"
    | TurnStopped => "turn_stopped"
    | ApprovalOpened => "approval_opened"
    | ApprovalsAllResolved => "approvals_all_resolved"
    | AttentionRaised(_) => "attention_raised"
    | AttentionCleared(_) => "attention_cleared"
    | TaskSet(_) => "task_set"
  }

  pure def isStartupCause(c: Cause): bool = match c {
    | SessionStartTimeout => true
    | StartupStall => true
    | _ => false
  }

  pure def withoutStartup(a: Set[Cause]): Set[Cause] = a.filter(c => not(isStartupCause(c)))

  pure def clearedExplicitly(c: Cause): bool = match c {
    | InputUnconfirmed(_) => true
    | InputGateTimeout(_) => true
    | _ => false
  }

  pure def diff(old: ThreadState, n: ThreadState, cause: str): List[Change] =
    val p = if (old.process != n.process) [ProcessChanged({ from: old.process, to: n.process, cause: cause })] else []
    val t = if (old.turn != n.turn) [TurnChanged({ from: old.turn, to: n.turn, cause: cause })] else []
    val k = if (old.task != n.task) [TaskChanged({ from: old.task, to: n.task, cause: cause })] else []
    val a = if (old.attention != n.attention) [AttentionChanged({ from: old.attention, to: n.attention, cause: cause })] else []
    p.concat(t).concat(k).concat(a)

  /// `state::reduce`: the next state, or `ok: false` with the state unchanged.
  pure def reduce(s: ThreadState, i: StateInput): Reduced =
    val alive = s.process == Running or s.process == Stopping
    val r: { ok: bool, next: ThreadState } = match i {
      | Reserved(_) =>
        if (s.process != Exited or s.attention.contains(ResumeBlocked)) { ok: false, next: s }
        else { ok: true, next: { ...s, process: Starting, turn: TurnUnknown } }
      | Spawned =>
        if (s.process != Starting) { ok: false, next: s }
        else { ok: true, next: { ...s, process: Running } }
      | SessionBound =>
        if (not(alive)) { ok: false, next: s }
        else { ok: true, next: { ...s, attention: withoutStartup(s.attention) } }
      | StopRequested =>
        if (s.process != Running) { ok: false, next: s }
        else { ok: true, next: { ...s, process: Stopping } }
      | ExitedBy(reason) =>
        if (s.process == Exited or (reason == ExitSpawnFailed and s.process != Starting)) { ok: false, next: s }
        else { ok: true, next: { ...s, process: Exited, turn: TurnUnknown, attention: withoutStartup(s.attention) } }
      | DeathConfirmed =>
        if (s.process != Exited) { ok: false, next: s }
        else { ok: true, next: { ...s, attention: s.attention.exclude(Set(ResumeBlocked)) } }
      | ReadinessObserved =>
        if (not(alive)) { ok: false, next: s }
        else { ok: true, next: if (s.turn != AwaitingApproval) { ...s, turn: Idle } else s }
      | PromptSubmitted =>
        if (not(alive)) { ok: false, next: s }
        else { ok: true, next: { ...s, turn: TurnRunning } }
      | Interrupted =>
        if (not(alive)) { ok: false, next: s }
        else { ok: true, next: { ...s, turn: TurnUnknown } }
      | TurnStopped =>
        if (not(alive)) { ok: false, next: s }
        else { ok: true, next: { ...s, turn: TurnUnknown } }
      | ApprovalOpened =>
        if (not(alive)) { ok: false, next: s }
        else { ok: true, next: { ...s, turn: AwaitingApproval } }
      | ApprovalsAllResolved =>
        { ok: true, next: if (s.turn == AwaitingApproval) { ...s, turn: TurnRunning } else s }
      | AttentionRaised(c) => { ok: true, next: { ...s, attention: s.attention.union(Set(c)) } }
      | AttentionCleared(c) =>
        if (not(clearedExplicitly(c))) { ok: false, next: s }
        else { ok: true, next: { ...s, attention: s.attention.exclude(Set(c)) } }
      | TaskSet(t) => { ok: true, next: { ...s, task: t } }
    }
    { ok: r.ok, next: r.next, changes: if (r.ok) diff(s, r.next, causeOf(i)) else [] }

  // ---------------------------------------------------------------------------
  // Reducer layer: crates/core/src/input.rs
  // ---------------------------------------------------------------------------

  type InState = Queued | Sending | Sent | Acked | Unconfirmed | Rejected | Cancelled | Dismissed
  // A subset of `RejectReason`: one validation failure and the positional spawn failure.
  type RejectReason = ControlCharacter | SpawnFailed
  type InputInput =
    | GateOpened(int)
    | WriteCompleted
    | WriteOutcomeUnknown
    | Reject(RejectReason)
    | Cancel
    | RecheckFailed
    | LaunchExited
    | DaemonRestarted
    | AckAttributed
    | AckNotAttributable
    | AckWindowExpired
    | Resolved(bool)

  pure def isTerminal(s: InState): bool = Set(Acked, Rejected, Cancelled, Dismissed).contains(s)
  pure def isInFlight(s: InState): bool = Set(Sending, Sent, Unconfirmed).contains(s)

  /// `input::step`.
  pure def stepInput(s: InState, i: InputInput): { ok: bool, next: InState } =
    val no = { ok: false, next: s }
    match i {
      | GateOpened(_) => if (s == Queued) { ok: true, next: Sending } else no
      | Reject(r) =>
        if (s == Queued and r != SpawnFailed) { ok: true, next: Rejected }
        else if (s == Sending and r == SpawnFailed) { ok: true, next: Rejected }
        else no
      | Cancel => if (s == Queued) { ok: true, next: Cancelled } else no
      | LaunchExited =>
        if (s == Queued) { ok: true, next: Cancelled }
        else if (s == Sending or s == Sent) { ok: true, next: Unconfirmed }
        else no
      | DaemonRestarted =>
        if (s == Queued) { ok: true, next: Cancelled }
        else if (s == Sending or s == Sent) { ok: true, next: Unconfirmed }
        else no
      | WriteCompleted => if (s == Sending) { ok: true, next: Sent } else no
      | RecheckFailed => if (s == Sending) { ok: true, next: Cancelled } else no
      | WriteOutcomeUnknown => if (s == Sending) { ok: true, next: Unconfirmed } else no
      | AckAttributed => if (s == Sent or s == Unconfirmed) { ok: true, next: Acked } else no
      | AckNotAttributable => if (s == Sent) { ok: true, next: Unconfirmed } else no
      | AckWindowExpired => if (s == Sent) { ok: true, next: Unconfirmed } else no
      | Resolved(delivered) =>
        if (s == Unconfirmed) { ok: true, next: if (delivered) Acked else Dismissed } else no
    }

  type GateReason =
    | NotBound
    | ProcessNotRunning
    | NoEvidence
    | WriteAfterEvidence
    | EvidenceOfSameTurn
    | TurnNotIdle
    | InputInFlight
    | BracketedPasteOff
  type GateDecision = Open | Closed(GateReason)

  // `GateView`; a position of 0 stands for `None` (stream positions start at 1).
  type GateView = {
    bound: bool,
    process: Proc,
    turn: Turn,
    readinessSeq: int,
    evidence: Evidence,
    lastWrite: int,
    lastHumanWrite: int,
    lastTurnStart: int,
    inFlight: bool,
    needsBracketedPaste: bool,
    bracketedPasteOn: bool,
  }

  /// `input::gate`.
  pure def gate(g: GateView): GateDecision =
    val laterTurn = g.lastTurnStart != 0 and g.lastHumanWrite < g.lastTurnStart and g.lastTurnStart < g.readinessSeq
    if (not(g.bound)) Closed(NotBound)
    else if (g.process != Running) Closed(ProcessNotRunning)
    else if (g.readinessSeq == 0) Closed(NoEvidence)
    else if (g.lastWrite != 0 and g.readinessSeq <= g.lastWrite) Closed(WriteAfterEvidence)
    else if (g.lastHumanWrite != 0 and g.evidence != OwnerAsserted and not(laterTurn)) Closed(EvidenceOfSameTurn)
    else if (g.turn != Idle) Closed(TurnNotIdle)
    else if (g.inFlight) Closed(InputInFlight)
    else if (g.needsBracketedPaste and not(g.bracketedPasteOn)) Closed(BracketedPasteOff)
    else Open

  // ---------------------------------------------------------------------------
  // Supervisor layer: model state
  // ---------------------------------------------------------------------------

  // Who wrote to the launch's PTY, in writer order. Every origin here is counted.
  type Origin = WInput(int) | WHuman | WReply
  type HumanOrigin = Attach | Key | CardKey

  // The pending-candidate slot of a `sending` input (§2.3 "Ack"): eligibility is decided
  // at the hook's intake and applied once `sent` is recorded.
  type Candidate = NoCandidate | CandidateAttributable | CandidateNotAttributable

  type Input = {
    state: InState,
    launch: int,
    positional: bool,
    sentAfterSeq: int,
    // The writer dispatched its text and submit (for a positional prompt: `launch_started`).
    dispatched: bool,
    // Writer position of the dispatch: the 1-based index in `writer`; 0 for a positional
    // prompt, anchored by `launch_started` (§2.3 "Ack").
    writePos: int,
    candidate: Candidate,
  }

  // The gate evidence of the current launch (§2.1 "Projections").
  type GateProjection = {
    bound: bool,
    boundSeq: int,
    readinessSeq: int,
    evidence: Evidence,
    // The writer position captured with the evidence at its intake.
    basis: int,
    lastWrite: int,
    lastHumanWrite: int,
    lastTurnStart: int,
    needsBracketedPaste: bool,
    bracketedPasteOn: bool,
  }

  // What the gate looked like when an input entered `sending` through the gate.
  type SendRecord = {
    bound: bool,
    running: bool,
    idle: bool,
    evidenceSeq: int,
    lastWrite: int,
    otherInFlight: bool,
    pasteOk: bool,
  }

  // The facts of an attributed ack at the hook's intake.
  type AckRecord = {
    sameLaunch: bool,
    afterAttempt: bool,
    uniqueMatch: bool,
    dispatched: bool,
    noInterveningWrite: bool,
  }

  type State = {
    thread: ThreadState,
    seq: int,
    launch: int,
    manual: bool,
    // The current (or last) launch's death is confirmed; true before the first launch.
    deathConfirmed: bool,
    gp: GateProjection,
    writer: List[Origin],
    inputs: int -> Input,
    cardsOpen: int,
    // A card was superseded or expired and none opened since.
    cardSuperseded: bool,
    // History for the invariants.
    sendCount: int -> int,
    writeCount: int -> int,
    sendRecords: int -> SendRecord,
    ackRecords: int -> AckRecord,
    reservedWithoutDeath: bool,
  }

  type CallKind =
    | Reduce(StateInput)
    | Step({ id: int, input: InputInput })
    | Create({ id: int, launch: int, initial: InState })
  type Call = { kind: CallKind, ok: bool, changes: List[Change] }

  type Action =
    | Init(bool)
    | Reserve({ positional: bool, manual: bool, needsBracketedPaste: bool })
    | ReserveRefused
    | Spawn
    | SpawnFail
    | Bind
    | Ready(Evidence)
    | OwnerReady
    | Prompt(Set[int])
    | HumanWrite(HumanOrigin)
    | VtReply
    | Send(int)
    | OpenGate(int)
    | Dispatch(int)
    | Outcome({ id: int, completed: bool })
    | WindowExpired(int)
    | GateTimeout(int)
    | CancelInput(int)
    | RejectInput(int)
    | ResolveInput({ id: int, delivered: bool })
    | Stop
    | Exit({ reason: ExitReason, deathConfirmed: bool })
    | Restart
    | ConfirmDeath
    | TurnStop
    | Interrupt
    | CardOpen
    | CardResolve
    | LateResolve
    | SessionTimer
    | Stall
    | SetTask(Task)
    | Decset(bool)
    // Inputs fed without the supervisor's guards (hole demonstrations only).
    | Raw(StateInput)
    | RawReadyUnbound(Evidence)

  type Last = {
    act: Action,
    calls: List[Call],
    gate: GateDecision,
    prevThread: ThreadState,
    prevInputs: int -> InState,
  }

  var st: State
  var last: Last

  // ---------------------------------------------------------------------------
  // Pure helpers
  // ---------------------------------------------------------------------------

  pure val NO_GATE_EVIDENCE: GateProjection = {
    bound: false,
    boundSeq: 0,
    readinessSeq: 0,
    evidence: SessionStart,
    basis: 0,
    lastWrite: 0,
    lastHumanWrite: 0,
    lastTurnStart: 0,
    needsBracketedPaste: false,
    bracketedPasteOn: false,
  }

  pure def initialState(task: bool): State = {
    thread: { process: Exited, turn: TurnUnknown, task: if (task) TaskActive else TaskNone, attention: Set() },
    seq: 1,
    launch: 0,
    manual: false,
    deathConfirmed: true,
    gp: NO_GATE_EVIDENCE,
    writer: [],
    inputs: Map(),
    cardsOpen: 0,
    cardSuperseded: false,
    sendCount: Map(),
    writeCount: Map(),
    sendRecords: Map(),
    ackRecords: Map(),
    reservedWithoutDeath: false,
  }

  pure def has(s: State, id: int): bool = s.inputs.keys().contains(id)
  pure def stateOf(s: State, id: int): InState = s.inputs.get(id).state
  pure def alive(s: State): bool = s.thread.process == Running or s.thread.process == Stopping

  /// `blocks_gate`: an in-flight input of the current launch; earlier launches never block.
  pure def inFlightCurrent(s: State): bool =
    s.inputs.keys().exists(id => s.inputs.get(id).launch == s.launch and isInFlight(stateOf(s, id)))

  pure def gateView(s: State): GateView = {
    bound: s.gp.bound,
    process: s.thread.process,
    turn: s.thread.turn,
    readinessSeq: s.gp.readinessSeq,
    evidence: s.gp.evidence,
    lastWrite: s.gp.lastWrite,
    lastHumanWrite: s.gp.lastHumanWrite,
    lastTurnStart: s.gp.lastTurnStart,
    inFlight: inFlightCurrent(s),
    needsBracketedPaste: s.gp.needsBracketedPaste,
    bracketedPasteOn: s.gp.bracketedPasteOn,
  }

  pure def counted(o: Origin): bool = match o {
    | WReply => REPLIES_COUNTED
    | _ => true
  }

  /// No counted write other than the input's own started after its write position.
  pure def noOtherWriteAfter(s: State, id: int): bool =
    val pos = s.inputs.get(id).writePos
    s.writer.indices().forall(j => j < pos or not(counted(s.writer[j])) or s.writer[j] == WInput(id))

  /// Ack eligibility at a hook's intake (§2.3 "Ack"; docs/design/pty-runtime.md §2.4
  /// "Ack attribution"): same launch, after the attempt, dispatched, and no other counted
  /// write after it. The unique text match is checked by the caller.
  pure def eligible(s: State, id: int, hookSeq: int): bool =
    val i = s.inputs.get(id)
    i.launch == s.launch and hookSeq > i.sentAfterSeq and i.dispatched and noOtherWriteAfter(s, id)

  pure def incr(m: int -> int, id: int): int -> int =
    m.put(id, if (m.keys().contains(id)) m.get(id) + 1 else 1)

  // An outcome under construction: the state and the reducer calls made so far.
  type Out = { st: State, calls: List[Call] }

  pure def red(o: Out, i: StateInput): Out =
    val r = reduce(o.st.thread, i)
    {
      st: { ...o.st, thread: r.next },
      calls: o.calls.append({ kind: Reduce(i), ok: r.ok, changes: r.changes }),
    }

  /// One input step, with the attention it couples to (§2.2 "Attention"): leaving
  /// `queued` clears `input_gate_timeout`, entering `unconfirmed` raises
  /// `input_unconfirmed`, leaving it clears it.
  pure def stp(o: Out, id: int, ii: InputInput): Out =
    val cur = o.st.inputs.get(id)
    val r = stepInput(cur.state, ii)
    val o1 = {
      st: {
        ...o.st,
        inputs: o.st.inputs.set(id, { ...cur, state: r.next }),
        sendCount: if (r.ok and r.next == Sending) incr(o.st.sendCount, id) else o.st.sendCount,
      },
      calls: o.calls.append({ kind: Step({ id: id, input: ii }), ok: r.ok, changes: [] }),
    }
    val leftQueued = r.ok and cur.state == Queued and r.next != Queued
    val o2 = if (leftQueued and o1.st.thread.attention.contains(InputGateTimeout(id)))
      red(o1, AttentionCleared(InputGateTimeout(id))) else o1
    val o3 = if (r.ok and r.next == Unconfirmed and cur.state != Unconfirmed)
      red(o2, AttentionRaised(InputUnconfirmed(id))) else o2
    if (r.ok and cur.state == Unconfirmed and r.next != Unconfirmed)
      red(o3, AttentionCleared(InputUnconfirmed(id))) else o3

  pure def setInput(o: Out, id: int, f: Input => Input): Out =
    { ...o, st: { ...o.st, inputs: o.st.inputs.set(id, f(o.st.inputs.get(id))) } }

  pure def bump(o: Out): Out = { ...o, st: { ...o.st, seq: o.st.seq + 1 } }

  pure def start(s: State): Out = { st: s, calls: [] }

  /// Exit and restart recovery (§2.3 "States", §2.4 "Restart"): `queued` -> `cancelled`,
  /// `sending`/`sent` -> `unconfirmed`, for the given launch (0: every launch).
  pure def recoverInputs(o: Out, launch: int, ii: InputInput): Out =
    range(1, MAX_INPUTS + 1).foldl(o, (acc, id) =>
      if (has(acc.st, id)
          and (launch == 0 or acc.st.inputs.get(id).launch == launch)
          and Set(Queued, Sending, Sent).contains(stateOf(acc.st, id)))
        stp(setInput(acc, id, i => { ...i, candidate: NoCandidate }), id, ii)
      else acc)

  /// Cards close on `Stop`, an interrupt, a prompt (superseded) or exit (expired) (§2.10).
  pure def closeCards(o: Out): Out =
    { ...o, st: { ...o.st, cardsOpen: 0, cardSuperseded: o.st.cardSuperseded or o.st.cardsOpen > 0 } }

  pure def exitLaunch(o: Out, reason: ExitReason, deathConfirmed: bool, ii: InputInput, launch: int): Out =
    val o1 = red(bump(o), ExitedBy(reason))
    val o2 = recoverInputs(closeCards(o1), launch, ii)
    { ...o2, st: { ...o2.st, deathConfirmed: deathConfirmed } }

  pure def firstFree(s: State): int =
    range(1, MAX_INPUTS + 1).foldl(0, (acc, id) => if (acc == 0 and not(has(s, id))) id else acc)

  // ---------------------------------------------------------------------------
  // Actions
  // ---------------------------------------------------------------------------

  action commit(a: Action, o: Out): bool = all {
    st' = o.st,
    last' = {
      act: a,
      calls: o.calls,
      gate: gate(gateView(o.st)),
      prevThread: st.thread,
      prevInputs: st.inputs.keys().mapBy(id => stateOf(st, id)),
    },
  }

  action initWith(task: bool): bool = all {
    st' = initialState(task),
    last' = {
      act: Init(task),
      calls: [],
      gate: gate(gateView(initialState(task))),
      prevThread: initialState(task).thread,
      prevInputs: Map(),
    },
  }

  action init = { nondet task = Bool.oneOf() initWith(task) }

  /// `launch_reserved` (§2.9 "Reservation"): only after the previous launch exited with
  /// death confirmed. A Codex positional first prompt is created `sending` with it (§2.3).
  action reserve(positional: bool, manual: bool, needsBracketedPaste: bool): bool = all {
    st.thread.process == Exited,
    st.deathConfirmed,
    st.launch < MAX_LAUNCHES,
    not(positional) or firstFree(st) != 0,
    val l = st.launch + 1
    val o1 = red(bump(start(st)), Reserved(st.launch > 0))
    val fresh = {
      ...o1.st,
      launch: l,
      manual: manual and not(positional),
      deathConfirmed: false,
      gp: { ...NO_GATE_EVIDENCE, needsBracketedPaste: needsBracketedPaste },
      writer: [],
      cardsOpen: 0,
      cardSuperseded: false,
    }
    val id = firstFree(st)
    val o2 = if (positional) {
      st: {
        ...fresh,
        inputs: fresh.inputs.put(id, {
          state: Sending, launch: l, positional: true, sentAfterSeq: 0,
          dispatched: false, writePos: 0, candidate: NoCandidate,
        }),
        sendCount: incr(fresh.sendCount, id),
      },
      calls: o1.calls.append({ kind: Create({ id: id, launch: l, initial: Sending }), ok: true, changes: [] }),
    } else { st: fresh, calls: o1.calls }
    commit(Reserve({ positional: positional, manual: manual, needsBracketedPaste: needsBracketedPaste }), o2),
  }

  /// A resume refused because the old launch's death is not confirmed raises
  /// `resume_blocked` (§2.9 "Daemon restart"); the reducer refuses `Reserved` from then on.
  action reserveRefused: bool = all {
    st.thread.process == Exited,
    not(st.deathConfirmed),
    val o1 = if (st.thread.attention.contains(ResumeBlocked)) start(st)
      else red(bump(start(st)), AttentionRaised(ResumeBlocked))
    commit(ReserveRefused, red(o1, Reserved(true))),
  }

  /// `launch_started`; it moves a positional prompt to `sent` with `sent_after_seq` =
  /// its seq, in the same transaction (§2.3 "Codex positional first prompt").
  action spawn: bool = all {
    st.thread.process == Starting,
    val o1 = red(bump(start(st)), Spawned)
    val o2 = range(1, MAX_INPUTS + 1).foldl(o1, (acc, id) =>
      if (has(acc.st, id) and acc.st.inputs.get(id).positional and acc.st.inputs.get(id).launch == st.launch
          and stateOf(acc.st, id) == Sending)
        stp(setInput(acc, id, i => { ...i, dispatched: true, writePos: 0, sentAfterSeq: acc.st.seq }), id, WriteCompleted)
      else acc)
    commit(Spawn, o2),
  }

  /// The spawn is confirmed not to have started: the positional prompt is rejected.
  action spawnFail: bool = all {
    st.thread.process == Starting,
    val o1 = red(bump(start(st)), ExitedBy(ExitSpawnFailed))
    val o2 = range(1, MAX_INPUTS + 1).foldl(o1, (acc, id) =>
      if (has(acc.st, id) and acc.st.inputs.get(id).launch == st.launch and acc.st.inputs.get(id).positional
          and stateOf(acc.st, id) == Sending)
        stp(acc, id, Reject(SpawnFailed))
      else acc)
    val o3 = recoverInputs(o2, st.launch, LaunchExited)
    commit(SpawnFail, { ...o3, st: { ...o3.st, deathConfirmed: true } }),
  }

  /// `session_bound`; also the turn start of the launch (§2.1 "Projections").
  action bind: bool = all {
    alive(st),
    not(st.gp.bound),
    val o1 = red(bump(start(st)), SessionBound)
    commit(Bind, { ...o1, st: { ...o1.st, gp: { ...o1.st.gp, bound: true, boundSeq: o1.st.seq, lastTurnStart: o1.st.seq } } }),
  }

  pure def observe(s: State, ev: Evidence): Out =
    val o1 = red(bump(start(s)), ReadinessObserved)
    { ...o1, st: { ...o1.st, gp: { ...o1.st.gp, readinessSeq: o1.st.seq, evidence: ev, basis: s.writer.length() } } }

  /// `readiness_observed` from the adapter's `readiness_evidence`: hooks of a bound launch
  /// only (§2.9 "Binding": an unbound launch's hooks are buffered).
  action ready(ev: Evidence): bool = all {
    alive(st),
    st.gp.bound,
    ev != OwnerAsserted,
    commit(Ready(ev), observe(st, ev)),
  }

  /// `chaldea ready`: accepted only for the current bound launch; otherwise refused,
  /// writing nothing (§2.3 "Gate").
  action ownerReady: bool = all {
    st.launch > 0,
    commit(OwnerReady, if (alive(st) and st.gp.bound) observe(st, OwnerAsserted) else start(st)),
  }

  /// A bound `UserPromptSubmit` (`user_message`), with its ack attribution (§2.3 "Ack").
  /// `matches` is the set of in-flight inputs of the current launch whose expected form
  /// equals the prompt: a nondeterministic choice standing for the text match.
  action prompt(matches: Set[int]): bool = all {
    alive(st),
    st.gp.bound,
    matches.forall(id => has(st, id) and st.inputs.get(id).launch == st.launch and isInFlight(stateOf(st, id))),
    val o1 = red(bump(start(st)), PromptSubmitted)
    val hookSeq = o1.st.seq
    val o2 = closeCards({ ...o1, st: { ...o1.st, gp: { ...o1.st.gp, lastTurnStart: hookSeq } } })
    val o3 = if (matches.size() != 1) o2 else {
      val id = matches.fold(0, (a, b) => b)
      val ok = eligible(st, id, hookSeq)
      val rec = {
        sameLaunch: st.inputs.get(id).launch == st.launch,
        afterAttempt: hookSeq > st.inputs.get(id).sentAfterSeq,
        uniqueMatch: true,
        dispatched: st.inputs.get(id).dispatched,
        noInterveningWrite: noOtherWriteAfter(st, id),
      }
      val s = stateOf(st, id)
      if (s == Sending)
        setInput(o2, id, i => { ...i, candidate:
          if (i.candidate == NoCandidate and ok) CandidateAttributable else CandidateNotAttributable })
      else if (ok) {
        val o4 = bump(stp(o2, id, AckAttributed))
        { ...o4, st: { ...o4.st, ackRecords: o4.st.ackRecords.put(id, rec) } }
      }
      else if (s == Sent) bump(stp(o2, id, AckNotAttributable))
      else o2
    }
    commit(Prompt(matches), o3),
  }

  /// A human write (`human_write`): attach and `key` close the gate; a card key only
  /// advances the last write of any origin (§2.1 "Projections", §2.3 "Gate").
  action humanWrite(o: HumanOrigin): bool = all {
    alive(st),
    o != CardKey or st.cardsOpen > 0,
    val o1 = bump(start(st))
    val gp = { ...o1.st.gp, lastWrite: o1.st.seq, lastHumanWrite: if (o == CardKey) st.gp.lastHumanWrite else o1.st.seq }
    commit(HumanWrite(o), { ...o1, st: { ...o1.st, gp: gp, writer: st.writer.append(WHuman) } }),
  }

  /// A VT reply: a counted write with no event.
  action vtReply: bool = all {
    alive(st),
    commit(VtReply, { st: { ...st, writer: st.writer.append(WReply) }, calls: [] }),
  }

  /// `send` appends `input_created` (queued) for the current launch.
  action send(id: int): bool = all {
    not(has(st, id)),
    st.launch > 0,
    st.thread.process == Starting or st.thread.process == Running,
    val o1 = bump(start(st))
    val s = {
      ...o1.st,
      inputs: o1.st.inputs.put(id, {
        state: Queued, launch: st.launch, positional: false, sentAfterSeq: 0,
        dispatched: false, writePos: 0, candidate: NoCandidate,
      }),
    }
    commit(Send(id), { st: s, calls: [{ kind: Create({ id: id, launch: st.launch, initial: Queued }), ok: true, changes: [] }] }),
  }

  /// The gate opened and the attempt, with the stream head, was persisted (§2.3 "States");
  /// `input_state_changed` to `sending` advances the last write of any origin (§2.1).
  action open(id: int): bool = all {
    has(st, id),
    stateOf(st, id) == Queued,
    st.inputs.get(id).launch == st.launch,
    gate(gateView(st)) == Open,
    val v = gateView(st)
    val rec = {
      bound: v.bound,
      running: v.process == Running,
      idle: v.turn == Idle,
      evidenceSeq: v.readinessSeq,
      lastWrite: v.lastWrite,
      otherInFlight: v.inFlight,
      pasteOk: not(v.needsBracketedPaste) or v.bracketedPasteOn,
    }
    val o1 = setInput(start(st), id, i => { ...i, sentAfterSeq: st.seq })
    val o2 = bump(stp(o1, id, GateOpened(st.seq)))
    commit(OpenGate(id), { ...o2, st: { ...o2.st, gp: { ...o2.st.gp, lastWrite: o2.st.seq }, sendRecords: o2.st.sendRecords.put(id, rec) } }),
  }

  /// The writer's revalidation under its lock before the first byte (§2.3 "Writer
  /// revalidation"). It excludes the input's own attempt, which the projection already
  /// counts as a write and as in flight. Passing it dispatches text and submit as one unit;
  /// failing it cancels the input with zero bytes.
  action dispatch(id: int): bool = all {
    has(st, id),
    stateOf(st, id) == Sending,
    not(st.inputs.get(id).positional),
    not(st.inputs.get(id).dispatched),
    val recheck = and {
      st.thread.process == Running,
      st.inputs.get(id).launch == st.launch,
      st.gp.bound,
      st.thread.turn == Idle,
      not(st.gp.needsBracketedPaste) or st.gp.bracketedPasteOn,
      st.writer.length() == st.gp.basis,
    }
    if (recheck) {
      val w = st.writer.append(WInput(id))
      val o1 = setInput(start(st), id, i => { ...i, dispatched: true, writePos: w.length() - 1 })
      commit(Dispatch(id), { ...o1, st: { ...o1.st, writer: w, writeCount: incr(st.writeCount, id) } })
    } else commit(Dispatch(id), bump(stp(start(st), id, RecheckFailed))),
  }

  /// The write's outcome: completed (`sent`, then the pending candidate is evaluated) or
  /// unknown (`unconfirmed`; the candidate is dropped).
  action outcome(id: int, completed: bool): bool = all {
    has(st, id),
    stateOf(st, id) == Sending,
    st.inputs.get(id).dispatched,
    not(st.inputs.get(id).positional),
    val c = st.inputs.get(id).candidate
    val o1 = setInput(bump(start(st)), id, i => { ...i, candidate: NoCandidate })
    val o2 = if (not(completed)) stp(o1, id, WriteOutcomeUnknown)
      else {
        val o3 = stp(o1, id, WriteCompleted)
        if (c == CandidateAttributable) {
          val o4 = bump(stp(o3, id, AckAttributed))
          val rec = { sameLaunch: true, afterAttempt: true, uniqueMatch: true, dispatched: true, noInterveningWrite: true }
          { ...o4, st: { ...o4.st, ackRecords: o4.st.ackRecords.put(id, rec) } }
        }
        else if (c == CandidateNotAttributable) bump(stp(o3, id, AckNotAttributable))
        else o3
      }
    commit(Outcome({ id: id, completed: completed }), o2),
  }

  /// The ack window expired; suspended for a positional prompt while a startup cause is
  /// active (§2.3 "Codex positional first prompt").
  action windowExpired(id: int): bool = all {
    has(st, id),
    stateOf(st, id) == Sent,
    not(st.inputs.get(id).positional and st.thread.attention.exists(c => isStartupCause(c))),
    commit(WindowExpired(id), bump(stp(start(st), id, AckWindowExpired))),
  }

  /// 60 s in `queued`: `input_gate_timeout`, the input stays `queued`.
  action gateTimeout(id: int): bool = all {
    has(st, id),
    stateOf(st, id) == Queued,
    not(st.thread.attention.contains(InputGateTimeout(id))),
    commit(GateTimeout(id), red(bump(start(st)), AttentionRaised(InputGateTimeout(id)))),
  }

  action cancelInput(id: int): bool = all {
    has(st, id),
    stateOf(st, id) == Queued,
    commit(CancelInput(id), bump(stp(start(st), id, Cancel))),
  }

  action rejectInput(id: int): bool = all {
    has(st, id),
    stateOf(st, id) == Queued,
    commit(RejectInput(id), bump(stp(start(st), id, Reject(ControlCharacter)))),
  }

  /// `chaldea input resolve --delivered | --dismiss`.
  action resolveInput(id: int, delivered: bool): bool = all {
    has(st, id),
    stateOf(st, id) == Unconfirmed,
    commit(ResolveInput({ id: id, delivered: delivered }), bump(stp(start(st), id, Resolved(delivered)))),
  }

  action stop: bool = all {
    st.thread.process == Running,
    commit(Stop, red(bump(start(st)), StopRequested)),
  }

  /// `launch_exited` of a started launch, with or without death confirmed.
  action exit(reason: ExitReason, deathConfirmed: bool): bool = all {
    alive(st),
    (reason == ExitStopped) == (st.thread.process == Stopping),
    reason == ExitNormal or reason == ExitStopped,
    commit(Exit({ reason: reason, deathConfirmed: deathConfirmed }),
      exitLaunch(start(st), reason, deathConfirmed, LaunchExited, st.launch)),
  }

  /// A daemon crash and restart (§2.4 "Restart", §2.9 "Daemon restart"): every open launch
  /// exits with death unconfirmed, then every input is recovered.
  action restart: bool = all {
    val o1 = if (st.thread.process == Exited) start(st)
      else exitLaunch(start(st),
        if (st.thread.process == Starting) ExitUnknownAfterCrash else ExitDaemonRestart,
        false, DaemonRestarted, st.launch)
    commit(Restart, recoverInputs(o1, 0, DaemonRestarted)),
  }

  action confirmDeath: bool = all {
    st.thread.process == Exited,
    not(st.deathConfirmed),
    val o1 = red(bump(start(st)), DeathConfirmed)
    commit(ConfirmDeath, { ...o1, st: { ...o1.st, deathConfirmed: true } }),
  }

  action turnStop: bool = all {
    alive(st),
    st.gp.bound,
    commit(TurnStop, closeCards(red(bump(start(st)), TurnStopped))),
  }

  action interrupt: bool = all {
    alive(st),
    st.gp.bound,
    commit(Interrupt, closeCards(red(bump(start(st)), Interrupted))),
  }

  action cardOpen: bool = all {
    alive(st),
    st.gp.bound,
    st.cardsOpen < MAX_CARDS,
    val o1 = red(bump(start(st)), ApprovalOpened)
    commit(CardOpen, { ...o1, st: { ...o1.st, cardsOpen: st.cardsOpen + 1, cardSuperseded: false } }),
  }

  /// One open card resolves (`ran`); the last one feeds `ApprovalsAllResolved`.
  action cardResolve: bool = all {
    st.cardsOpen > 0,
    val o1 = { ...bump(start(st)), st: { ...bump(start(st)).st, cardsOpen: st.cardsOpen - 1 } }
    commit(CardResolve, if (st.cardsOpen == 1) red(o1, ApprovalsAllResolved) else o1),
  }

  /// A resolution that arrives after `Stop`, an interrupt or exit closed the cards.
  action lateResolve: bool = all {
    st.cardSuperseded,
    st.cardsOpen == 0,
    commit(LateResolve, red(start(st), ApprovalsAllResolved)),
  }

  /// No `SessionStart` 20 s after `launch_started` of a non-manual launch.
  action sessionTimer: bool = all {
    st.thread.process == Running,
    not(st.gp.bound),
    not(st.manual),
    not(st.thread.attention.contains(SessionStartTimeout)),
    commit(SessionTimer, red(bump(start(st)), AttentionRaised(SessionStartTimeout))),
  }

  /// An expected Codex `Hooks need review` during startup.
  action stall: bool = all {
    st.thread.process == Running,
    not(st.gp.bound),
    not(st.thread.attention.contains(StartupStall)),
    commit(Stall, red(bump(start(st)), AttentionRaised(StartupStall))),
  }

  action setTask(t: Task): bool = commit(SetTask(t), red(bump(start(st)), TaskSet(t)))

  /// DECSET 2004 set or reset in the VT.
  action decset(on: bool): bool = all {
    alive(st),
    commit(Decset(on), { st: { ...st, gp: { ...st.gp, bracketedPasteOn: on } }, calls: [] }),
  }

  pure def inFlightIds(s: State): Set[int] =
    s.inputs.keys().filter(id => s.inputs.get(id).launch == s.launch and isInFlight(stateOf(s, id)))

  // The simulator picks uniformly among the enabled branches of `any`, so the actions are
  // grouped: progress along a launch and an input, a prompt hook, and disturbances.
  action lifecycleStep = any {
    nondet p = Bool.oneOf()
    nondet m = Bool.oneOf()
    nondet b = Bool.oneOf()
    reserve(p, m, b),
    reserveRefused,
    confirmDeath,
    spawn,
    bind,
    nondet ev = Set(SessionStart, StopSettled, PostDecision).oneOf()
    ready(ev),
    ownerReady,
    turnStop,
  }

  action inputStep = {
    nondet id = INPUT_IDS.oneOf()
    any {
      send(id),
      open(id),
      dispatch(id),
      nondet c = Bool.oneOf()
      outcome(id, c),
      windowExpired(id),
      nondet d = Bool.oneOf()
      resolveInput(id, d),
    }
  }

  // A prompt that matches an in-flight input; one matching none is a disturbance.
  action promptStep = all {
    inFlightIds(st).size() > 0,
    nondet ms = inFlightIds(st).powerset().exclude(Set(Set())).oneOf()
    prompt(ms),
  }

  action disturbanceStep = any {
    nondet id = INPUT_IDS.oneOf()
    any { gateTimeout(id), cancelInput(id), rejectInput(id) },
    nondet ho = Set(Attach, Key, CardKey).oneOf()
    humanWrite(ho),
    vtReply,
    prompt(Set()),
    stop,
    nondet r = Set(ExitNormal, ExitStopped).oneOf()
    nondet dc = Bool.oneOf()
    exit(r, dc),
    spawnFail,
    restart,
    interrupt,
    cardOpen,
    cardResolve,
    lateResolve,
    sessionTimer,
    stall,
    nondet t = Set(TaskNone, TaskActive, TaskCompleted).oneOf()
    setTask(t),
    nondet on = Bool.oneOf()
    decset(on),
  }

  action step = any { lifecycleStep, inputStep, promptStep, disturbanceStep }

  // ---------------------------------------------------------------------------
  // Inputs without the supervisor's guards: what the reducers alone admit (holes 4a-4d)
  // ---------------------------------------------------------------------------

  /// One reducer input fed with no supervisor guard; only the reducer decides.
  action raw(i: StateInput): bool = all {
    val o1 = red(start(st), i)
    val accepted = o1.calls[0].ok
    val unsafe = match i {
      | Reserved(_) => accepted and not(st.deathConfirmed)
      | _ => false
    }
    commit(Raw(i), { ...o1, st: { ...o1.st, reservedWithoutDeath: st.reservedWithoutDeath or unsafe } }),
  }

  /// Readiness of an unbound launch reaching the reducer and the gate evidence.
  action rawReadyUnbound(ev: Evidence): bool = all {
    alive(st),
    not(st.gp.bound),
    commit(RawReadyUnbound(ev), observe(st, ev)),
  }

  pure val RAW_INPUTS: Set[StateInput] = Set(
    Reserved(true), SessionBound, ReadinessObserved, PromptSubmitted,
    AttentionRaised(StartupStall), AttentionRaised(SessionStartTimeout), AttentionRaised(InputUnconfirmed(1)),
    ApprovalsAllResolved,
  )

  /// The contract's steps plus unguarded reducer inputs: `quint run --step stepUnguarded`
  /// finds the holes.
  action stepUnguarded = any {
    step,
    nondet i = RAW_INPUTS.oneOf()
    raw(i),
    nondet ev = Set(SessionStart, StopSettled).oneOf()
    rawReadyUnbound(ev),
  }

  // ---------------------------------------------------------------------------
  // Invariants (each cites its source)
  // ---------------------------------------------------------------------------

  pure def countOf(m: int -> int, id: int): int = if (m.keys().contains(id)) m.get(id) else 0

  /// CL-4 (mvp §6.2), input.rs:3-4: `sending` is entered at most once per input.
  val sendingAtMostOnce = INPUT_IDS.forall(id => countOf(st.sendCount, id) <= 1)

  /// arch §4.3, mvp §2.3:183 "Never resent": an input's text is written at most once.
  val writtenAtMostOnce = INPUT_IDS.forall(id => countOf(st.writeCount, id) <= 1)

  /// mvp §2.3:171 "Gate", CL-5: `sending` through the gate only when bound, running, idle,
  /// with evidence newer than every write, no other in-flight input of the launch, and
  /// bracketed paste on where needed. The positional prompt (§2.3:186) has no record.
  val sendingOnlyThroughOpenGate = st.sendRecords.keys().forall(id => {
    val r = st.sendRecords.get(id)
    r.bound and r.running and r.idle and r.evidenceSeq > r.lastWrite and not(r.otherInFlight) and r.pasteOk
  })

  /// mvp §2.3:171: no other input of the current launch is in flight, so at most one is.
  val oneInFlightPerLaunch = 1.to(MAX_LAUNCHES).forall(l =>
    st.inputs.keys().filter(id => st.inputs.get(id).launch == l and isInFlight(stateOf(st, id))).size() <= 1)

  /// mvp §2.3:171, CL-6: inputs of earlier launches never block the gate.
  val earlierLaunchesNeverBlock =
    last.gate == Closed(InputInFlight) implies
      st.inputs.keys().exists(id => st.inputs.get(id).launch == st.launch and isInFlight(stateOf(st, id)))

  /// mvp §2.3:185 and input.rs:36-46: a terminal state never changes.
  val terminalStatesImmutable = last.prevInputs.keys().forall(id =>
    isTerminal(last.prevInputs.get(id)) implies stateOf(st, id) == last.prevInputs.get(id))

  /// arch §4.3, mvp §2.3:183, CL-4: nothing leads from `sending`, `sent` or `unconfirmed`
  /// back to `queued` or `sending`, and `unconfirmed` leaves only to `acked` or `dismissed`.
  val neverResent = last.prevInputs.keys().forall(id => {
    val p = last.prevInputs.get(id)
    val c = stateOf(st, id)
    and {
      isInFlight(p) implies not(Set(Queued, Sending).contains(c) and c != p),
      p == Unconfirmed implies Set(Unconfirmed, Acked, Dismissed).contains(c),
    }
  })

  /// mvp §2.2:144: `input_unconfirmed(id)` is active iff that input is `unconfirmed`.
  val unconfirmedAttentionIff = INPUT_IDS.forall(id =>
    st.thread.attention.contains(InputUnconfirmed(id)) iff (has(st, id) and stateOf(st, id) == Unconfirmed))

  /// mvp §2.2:145: `input_gate_timeout(id)` only while that input is `queued`.
  val gateTimeoutOnlyWhileQueued = INPUT_IDS.forall(id =>
    st.thread.attention.contains(InputGateTimeout(id)) implies (has(st, id) and stateOf(st, id) == Queued))

  /// mvp §2.2:142-143: startup causes are absent once the launch is bound or exited.
  val noStartupCauseAfterBindOrExit =
    (st.gp.bound or st.thread.process == Exited) implies not(st.thread.attention.exists(c => isStartupCause(c)))

  /// mvp §2.10:335, state.rs:362-367: a resolution after `Stop`, an interrupt or exit
  /// changes nothing.
  val lateResolutionChangesNothing = match last.act {
    | LateResolve => st.thread == last.prevThread
    | _ => true
  }

  /// mvp §2.3:183, §2.4:214, CL-6: after a restart no input is `queued`, `sending` or `sent`;
  /// `sending`/`sent` became `unconfirmed` and `queued` became `cancelled`.
  val restartRecovers = match last.act {
    | Restart => last.prevInputs.keys().forall(id => {
        val p = last.prevInputs.get(id)
        val c = stateOf(st, id)
        and {
          p == Queued implies c == Cancelled,
          (p == Sending or p == Sent) implies c == Unconfirmed,
        }
      })
    | _ => true
  }

  /// mvp §2.3:183, §2.3:185: once the launch exited, none of its inputs is still `queued`,
  /// `sending` or `sent` (the exit's own recovery).
  val exitedLaunchHasNoPendingInput =
    st.thread.process == Exited implies
      st.inputs.keys().forall(id => not(Set(Queued, Sending, Sent).contains(stateOf(st, id))))

  /// mvp §2.3:182, CL-7: an attributed ack is of the same launch, after the attempt, with a
  /// unique match, for a dispatched write, with no other counted write after it.
  val ackOnlyWhenAttributable = st.ackRecords.keys().forall(id => {
    val r = st.ackRecords.get(id)
    r.sameLaunch and r.afterAttempt and r.uniqueMatch and r.dispatched and r.noInterveningWrite
  })

  /// mvp §2.9:323, §8 "Also decided", CL-18: a launch is reserved only after the previous
  /// launch's death is confirmed.
  val reservedOnlyAfterDeath = not(st.reservedWithoutDeath)

  /// mvp §2.2:146, §2.9:326: `resume_blocked` only while the exited launch's death is
  /// unconfirmed.
  val resumeBlockedOnlyWhileDeathUnconfirmed =
    st.thread.attention.contains(ResumeBlocked) implies (st.thread.process == Exited and not(st.deathConfirmed))

  /// mvp §2.2:135, state.rs:321-322: an exited launch's turn is `unknown`.
  val exitedTurnUnknown = st.thread.process == Exited implies st.thread.turn == TurnUnknown

  /// CL-2: after registration `task_state` changes only on `task_set`.
  val taskOnlyByTaskSet = match last.act {
    | SetTask(_) => true
    | _ => st.thread.task == last.prevThread.task
  }

  /// mvp §2.3:171, §2.9:325: gate evidence is of the bound launch, after its binding.
  val gateEvidenceIsBound = st.gp.readinessSeq == 0 or (st.gp.bound and st.gp.readinessSeq > st.gp.boundSeq)

  /// The supervisor feeds the reducers only inputs they accept, except the reservation it
  /// expects to be refused while `resume_blocked` is active.
  val supervisorInputsAccepted = match last.act {
    | ReserveRefused => last.calls.indices().forall(j => last.calls[j].ok or j == last.calls.length() - 1)
    | Raw(_) => true
    | _ => last.calls.indices().forall(j => last.calls[j].ok)
  }

  val allInvariants = and {
    sendingAtMostOnce,
    writtenAtMostOnce,
    sendingOnlyThroughOpenGate,
    oneInFlightPerLaunch,
    earlierLaunchesNeverBlock,
    terminalStatesImmutable,
    neverResent,
    unconfirmedAttentionIff,
    gateTimeoutOnlyWhileQueued,
    noStartupCauseAfterBindOrExit,
    lateResolutionChangesNothing,
    restartRecovers,
    exitedLaunchHasNoPendingInput,
    ackOnlyWhenAttributable,
    reservedOnlyAfterDeath,
    resumeBlockedOnlyWhileDeathUnconfirmed,
    exitedTurnUnknown,
    taskOnlyByTaskSet,
    gateEvidenceIsBound,
    supervisorInputsAccepted,
  }

  // Witnesses: reachability of the interesting states (`--witnesses`; each should be
  // reported for some traces, else the simulation does not cover it).
  val witnessAcked = st.inputs.keys().exists(id => stateOf(st, id) == Acked)
  val witnessLateAck = match last.act { | Prompt(_) => last.prevInputs.keys().exists(id => last.prevInputs.get(id) == Unconfirmed and stateOf(st, id) == Acked) | _ => false }
  val witnessRecheckFailed = st.inputs.keys().exists(id => stateOf(st, id) == Cancelled and countOf(st.sendCount, id) == 1)
  val witnessAckedThroughGate = st.ackRecords.keys().exists(id => not(st.inputs.get(id).positional))
  val witnessDismissed = st.inputs.keys().exists(id => stateOf(st, id) == Dismissed)
  val witnessSecondLaunch = st.launch == 2
  val witnessPositionalAcked = st.inputs.keys().exists(id => st.inputs.get(id).positional and stateOf(st, id) == Acked)
  val witnessResumeBlocked = st.thread.attention.contains(ResumeBlocked)
  val witnessOpenAfterEarlierUnconfirmed = last.gate == Open and
    st.inputs.keys().exists(id => st.inputs.get(id).launch < st.launch and stateOf(st, id) == Unconfirmed)
}
