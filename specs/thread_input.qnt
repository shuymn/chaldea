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
 * the in-flight inputs whose expected form it equals); time, including the settle
 * interval, is explicit nondeterministic actions.
 * Not modelled: the writer lock and the concrete `Basis`, timer durations, sid scans and
 * descendant kills, worktree and close races (PR 10); the prompt size limit, which
 * refuses before any input exists; `vt_snapshot_taken` at a gate timeout, an output with
 * no state.
 */
module thread_input {
  // ---------------------------------------------------------------------------
  // Bounds
  // ---------------------------------------------------------------------------

  pure val MAX_LAUNCHES = 2
  pure val MAX_INPUTS = 3
  pure val MAX_CARDS = 2
  pure val INPUT_IDS = 1.to(MAX_INPUTS)

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
  // A card key carries the launch and card it was sent from (mvp §2.10:336: refused once
  // that card is no longer open).
  type HumanOrigin = Attach | Key | CardKey({ launch: int, card: int })

  // The launch's adapter (mvp §2.7): Claude binds by `--session-id` and its `SessionStart`
  // is readiness evidence; a Codex launch has a positional first prompt, or none (a manual
  // resume), and no hook fires before its first turn.
  type Adapter = Claude | CodexPositional | CodexManual
  // The adapter's `readiness_evidence` mode after a turn (mvp §2.3, §8 D8).
  type ReadinessMode = StopSettle | PostDecisionMode | OwnerOnly
  type LaunchConfig = {
    adapter: Adapter,
    mode: ReadinessMode,
    needsBracketedPaste: bool,
    // A Codex launch after a CLI upgrade expects `Hooks need review` (mvp §2.7).
    upgraded: bool,
  }

  // The facts of a matching `UserPromptSubmit` for one input, taken at the hook's intake
  // (mvp §2.3 "Ack"; docs/design/pty-runtime.md §2.3 "Ack attribution").
  type AckRecord = {
    sameLaunch: bool,
    afterAttempt: bool,
    uniqueMatch: bool,
    dispatched: bool,
    noInterveningWrite: bool,
  }

  // The pending-candidate slot of a `sending` input: a dispatched match is kept with its
  // intake facts and applied once `sent` is recorded.
  type Candidate = NoCandidate | HasCandidate(AckRecord)

  // A `Stop` hook, identified by its launch, the turn it belongs to, and its intake order.
  type StopRef = { launch: int, turn: int, id: int }

  // A signal the adapter may turn into readiness evidence: Claude's `SessionStart`, or a
  // `Stop` whose settle interval (abstract) may complete. Any later hook of the launch
  // invalidates it, and a settle completion names the `Stop` it was started for.
  type EvidenceCandidate = NoEvidenceCandidate | SessionStartCandidate | StopCandidate(StopRef)

  type Input = {
    state: InState,
    launch: int,
    positional: bool,
    sentAfterSeq: int,
    // The seq of its `input_state_changed` to `sending`, and the last write before it.
    attemptSeq: int,
    lastWriteBefore: int,
    // The basis of the evidence the gate opened on (docs/design/pty-runtime.md §2.3).
    admittedBasis: int,
    // The writer dispatched its text and submit (for a positional prompt: `launch_started`).
    dispatched: bool,
    // Writer position of the dispatch: the 0-based index in `writer`, 0 for a positional
    // prompt, anchored by `launch_started` (mvp §2.3 "Ack").
    writePos: int,
    candidate: Candidate,
  }

  // The gate evidence of the current launch (mvp §2.1 "Projections").
  type GateProjection = {
    bound: bool,
    boundSeq: int,
    readinessSeq: int,
    evidence: Evidence,
    // The writer position captured with the evidence at its intake.
    basis: int,
    // The launch and turn the evidence belongs to (`readinessSeq == 0`: no evidence).
    evidenceLaunch: int,
    evidenceTurn: int,
    lastWrite: int,
    lastHumanWrite: int,
    lastTurnStart: int,
    needsBracketedPaste: bool,
    bracketedPasteOn: bool,
  }

  type CardState = CardIsOpen | CardRan | CardSuperseded | CardExpired
  type Card = { launch: int, turn: int, state: CardState }

  // A hook of an unbound launch, buffered with the facts of its intake (mvp §2.9 "Binding").
  type BufferedHook =
    | BufPrompt({ matches: Set[int], facts: int -> AckRecord })
    | BufStop(int)
    | BufReady({ kind: Evidence, basis: int, stop: int })

  // What the gate looked like when an input entered `sending` through the gate.
  type SendRecord = {
    bound: bool,
    running: bool,
    idle: bool,
    evidenceSeq: int,
    lastWrite: int,
    sameTurnOk: bool,
    otherInFlight: bool,
    pasteOk: bool,
  }

  // The raw facts at the dispatch boundary of an input, before the write: the invariant
  // re-derives the recheck from them instead of reading the guard's view.
  type DispatchRecord = {
    writerLength: int,
    admittedBasis: int,
    sameLaunch: bool,
    bound: bool,
    process: Proc,
    turn: Turn,
    readinessSeq: int,
    evidence: Evidence,
    lastWrite: int,
    attemptSeq: int,
    lastWriteBefore: int,
    lastHumanWrite: int,
    lastTurnStart: int,
    otherInFlight: int,
    needsBracketedPaste: bool,
    bracketedPasteOn: bool,
  }

  // A card key the writer accepted, with the card's state at that moment.
  type CardKeyRecord = { keyLaunch: int, launch: int, cardLaunch: int, cardOpen: bool }

  type State = {
    thread: ThreadState,
    seq: int,
    launch: int,
    cfg: LaunchConfig,
    // The current (or last) launch's death is confirmed; true before the first launch.
    deathConfirmed: bool,
    // The current launch's `SessionStart` arrived (binding may still be pending).
    sessionStartSeen: bool,
    // Turns started in the current launch (bound `UserPromptSubmit`s).
    turnNo: int,
    // The launch's hooks fire: from the start, except a manual Codex launch, whose hooks
    // start with the turn a human starts (mvp §2.7: no hook before the first turn).
    hooksLive: bool,
    // `Stop` hooks taken in so far, for their intake order.
    stopCount: int,
    evidenceCandidate: EvidenceCandidate,
    gp: GateProjection,
    writer: List[Origin],
    buffer: List[BufferedHook],
    // The buffered `Stop` a settle completion may still name, tracked at intake (0: none).
    bufferedStop: int,
    inputs: int -> Input,
    cards: int -> Card,
    // History for the invariants.
    sendCount: int -> int,
    writeCount: int -> int,
    sendRecords: int -> SendRecord,
    dispatchRecords: int -> DispatchRecord,
    ackRecords: int -> AckRecord,
    // Inputs the owner resolved as delivered (`chaldea input resolve --delivered`).
    ownerDelivered: Set[int],
    cardKeys: List[CardKeyRecord],
    // Per reservation the reducer accepted: was the previous launch's death confirmed.
    reservations: List[bool],
  }

  type CallKind =
    | Reduce(StateInput)
    | Step({ id: int, input: InputInput })
    | Create({ id: int, launch: int, initial: InState })
  type Call = { kind: CallKind, ok: bool, changes: List[Change] }

  type Action =
    | Init(bool)
    | Reserve(LaunchConfig)
    | ReserveRefused
    | Spawn
    | SpawnFail
    | SpawnStartedThenFailed(bool)
    | SessionStartHook
    | ManualFirstTurn
    | Bind
    | Ready({ kind: Evidence, stop: int })
    | OwnerReady
    | Prompt(Set[int])
    | BufferPrompt(Set[int])
    | BufferStop
    | BufferReady({ kind: Evidence, stop: int })
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
    | TurnStop(int)
    | Interrupt
    | CardOpen(int)
    | CardResolve(int)
    | LateResolve(int)
    | SessionTimer
    | Stall
    | SetTask(Task)
    | Decset(bool)
    // Inputs fed without the supervisor's guards (hole demonstrations and coverage only).
    | Raw(StateInput)
    | RawStep({ id: int, input: InputInput })
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
    evidenceLaunch: 0,
    evidenceTurn: 0,
    lastWrite: 0,
    lastHumanWrite: 0,
    lastTurnStart: 0,
    needsBracketedPaste: false,
    bracketedPasteOn: false,
  }

  pure val CLAUDE: LaunchConfig = { adapter: Claude, mode: StopSettle, needsBracketedPaste: true, upgraded: false }

  /// A launch configuration: Claude always uses bracketed paste and has no hook review;
  /// Codex uses it on macOS and not on Windows (`win_keys`, mvp §2.7).
  pure def config(a: Adapter, m: ReadinessMode, bp: bool, upgraded: bool): LaunchConfig =
    if (a == Claude) { ...CLAUDE, mode: m }
    else { adapter: a, mode: m, needsBracketedPaste: bp, upgraded: upgraded }

  pure def initialState(task: bool): State = {
    thread: { process: Exited, turn: TurnUnknown, task: if (task) TaskActive else TaskNone, attention: Set() },
    seq: 1,
    launch: 0,
    cfg: CLAUDE,
    deathConfirmed: true,
    sessionStartSeen: false,
    turnNo: 0,
    hooksLive: false,
    stopCount: 0,
    evidenceCandidate: NoEvidenceCandidate,
    gp: NO_GATE_EVIDENCE,
    writer: [],
    buffer: [],
    bufferedStop: 0,
    inputs: Map(),
    cards: Map(),
    sendCount: Map(),
    writeCount: Map(),
    sendRecords: Map(),
    dispatchRecords: Map(),
    ackRecords: Map(),
    ownerDelivered: Set(),
    cardKeys: [],
    reservations: [],
  }

  pure def has(s: State, id: int): bool = s.inputs.keys().contains(id)
  pure def stateOf(s: State, id: int): InState = s.inputs.get(id).state
  pure def alive(s: State): bool = s.thread.process == Running or s.thread.process == Stopping
  pure def openCards(s: State): Set[int] =
    s.cards.keys().filter(c => s.cards.get(c).launch == s.launch and s.cards.get(c).state == CardIsOpen)

  /// `blocks_gate`: the in-flight inputs of the current launch; earlier launches never block.
  pure def inFlightIds(s: State): Set[int] =
    s.inputs.keys().filter(id => s.inputs.get(id).launch == s.launch and isInFlight(stateOf(s, id)))

  pure def gateView(s: State): GateView = {
    bound: s.gp.bound,
    process: s.thread.process,
    turn: s.thread.turn,
    readinessSeq: s.gp.readinessSeq,
    evidence: s.gp.evidence,
    lastWrite: s.gp.lastWrite,
    lastHumanWrite: s.gp.lastHumanWrite,
    lastTurnStart: s.gp.lastTurnStart,
    inFlight: inFlightIds(s).size() > 0,
    needsBracketedPaste: s.gp.needsBracketedPaste,
    bracketedPasteOn: s.gp.bracketedPasteOn,
  }

  /// The writer's recheck view of an input (mvp §2.3 "Writer revalidation"): the whole gate,
  /// leaving out only the input's own attempt, which the projection counts as the last
  /// write and as in flight.
  pure def recheckView(s: State, id: int): GateView =
    val i = s.inputs.get(id)
    {
      ...gateView(s),
      lastWrite: if (s.gp.lastWrite == i.attemptSeq) i.lastWriteBefore else s.gp.lastWrite,
      inFlight: inFlightIds(s).exclude(Set(id)).size() > 0,
    }

  /// No counted write other than the input's own started after its write position.
  pure def noOtherWriteAfter(s: State, id: int): bool =
    val pos = s.inputs.get(id).writePos
    s.writer.indices().forall(j => j < pos or s.writer[j] == WInput(id))

  /// The facts of a hook matching `id` at its intake, the hook's seq being `hookSeq`.
  pure def intakeFacts(s: State, id: int, matches: Set[int], hookSeq: int): AckRecord =
    val i = s.inputs.get(id)
    {
      sameLaunch: i.launch == s.launch,
      afterAttempt: hookSeq > i.sentAfterSeq,
      uniqueMatch: matches.size() == 1,
      dispatched: i.dispatched,
      noInterveningWrite: noOtherWriteAfter(s, id),
    }

  pure def attributable(r: AckRecord): bool =
    r.sameLaunch and r.afterAttempt and r.uniqueMatch and r.dispatched and r.noInterveningWrite

  /// Whether the launch's adapter and readiness mode can produce this evidence kind
  /// (mvp §2.3 `readiness_evidence` modes, §2.7). Model assumption: under `none` Claude's
  /// `SessionStart` is no evidence either (§2.3:173 "only `chaldea ready`"), although
  /// §2.7:303 lists it before the after-turn mode; PR 10 decides.
  pure def kindAllowed(c: LaunchConfig, ev: Evidence): bool = match ev {
    | SessionStart => c.adapter == Claude and c.mode != OwnerOnly
    | StopSettled => c.mode == StopSettle
    | PostDecision => c.mode == PostDecisionMode
    | OwnerAsserted => true
  }

  /// Whether the current candidate is the `Stop` `id` of the current launch and turn.
  pure def stopBacks(s: State, c: EvidenceCandidate, id: int): bool = match c {
    | StopCandidate(r) => r.id == id and r.launch == s.launch and r.turn == s.turnNo
    | _ => false
  }

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

  /// One input step, with the attention it couples to (mvp §2.2 "Attention"): leaving
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

  pure def withState(o: Out, f: State => State): Out = { ...o, st: f(o.st) }

  pure def newInput(state: InState, launch: int, positional: bool): Input = {
    state: state, launch: launch, positional: positional, sentAfterSeq: 0,
    attemptSeq: 0, lastWriteBefore: 0, admittedBasis: 0,
    dispatched: false, writePos: 0, candidate: NoCandidate,
  }

  /// An attributed ack, recording the intake facts it was decided on.
  pure def ackWith(o: Out, id: int, r: AckRecord): Out =
    withState(bump(stp(o, id, AckAttributed)), s => { ...s, ackRecords: s.ackRecords.put(id, r) })

  /// Exit and restart recovery (mvp §2.3 "States", §2.4 "Restart"): `queued` ->
  /// `cancelled`, `sending`/`sent` -> `unconfirmed`, for the given launch (0: every launch).
  pure def recoverInputs(o: Out, launch: int, ii: InputInput): Out =
    range(1, MAX_INPUTS + 1).foldl(o, (acc, id) =>
      if (has(acc.st, id)
          and (launch == 0 or acc.st.inputs.get(id).launch == launch)
          and Set(Queued, Sending, Sent).contains(stateOf(acc.st, id)))
        stp(setInput(acc, id, i => { ...i, candidate: NoCandidate }), id, ii)
      else acc)

  /// Open cards of the current launch close (mvp §2.10): superseded by `Stop`, an
  /// interrupt or a prompt, expired by exit.
  pure def closeCards(o: Out, closed: CardState): Out =
    withState(o, s => { ...s, cards: s.cards.keys().mapBy(c =>
      if (openCards(s).contains(c)) { ...s.cards.get(c), state: closed } else s.cards.get(c)) })

  /// A later hook of the launch invalidates a pending evidence candidate.
  pure def invalidate(o: Out): Out = withState(o, s => { ...s, evidenceCandidate: NoEvidenceCandidate })

  pure def exitLaunch(o: Out, reason: ExitReason, deathConfirmed: bool, ii: InputInput, launch: int): Out =
    val o1 = red(bump(o), ExitedBy(reason))
    val o2 = recoverInputs(closeCards(invalidate(o1), CardExpired), launch, ii)
    // Buffered hooks of a launch that can no longer bind become `raw_unknown` (mvp §2.9).
    withState(o2, s => { ...s, deathConfirmed: deathConfirmed, buffer: [] })

  pure def firstFree(s: State): int =
    range(1, MAX_INPUTS + 1).foldl(0, (acc, id) => if (acc == 0 and not(has(s, id))) id else acc)

  /// A bound `UserPromptSubmit` (`user_message`) with its ack attribution (mvp §2.3 "Ack"),
  /// from the facts of its intake. A unique match that has not dispatched is a
  /// `user_message` without `input_id`; a dispatched one acks a `sent` or `unconfirmed`
  /// input, or is kept as the candidate of a `sending` one.
  pure def applyPrompt(o: Out, matches: Set[int], facts: int -> AckRecord): Out =
    val o1 = red(bump(o), PromptSubmitted)
    val o2 = closeCards(invalidate(withState(o1, s => {
      ...s, turnNo: s.turnNo + 1, gp: { ...s.gp, lastTurnStart: s.seq },
    })), CardSuperseded)
    if (matches.size() != 1) o2 else {
      val id = matches.fold(0, (a, b) => b)
      val r = facts.get(id)
      val cur = stateOf(o2.st, id)
      // A buffered hook is applied at promotion; an input that left flight since its
      // intake is no longer a candidate.
      if (not(r.dispatched) or not(isInFlight(cur))) o2
      else if (cur == Sending)
        setInput(o2, id, i => { ...i, candidate: match i.candidate {
          | NoCandidate => HasCandidate(r)
          // A second dispatched match for one input: ambiguous.
          | HasCandidate(prev) => HasCandidate({ ...prev, uniqueMatch: false })
        } })
      else if (attributable(r)) ackWith(o2, id, r)
      else if (cur == Sent) bump(stp(o2, id, AckNotAttributable))
      else o2
    }

  /// A `Stop` of the current `turn`: the turn becomes `unknown`, the cards are superseded,
  /// and the stop replaces any pending candidate.
  pure def applyStop(o: Out, turn: int, id: int): Out =
    val o1 = closeCards(red(bump(o), TurnStopped), CardSuperseded)
    withState(o1, s => { ...s, evidenceCandidate: StopCandidate({ launch: s.launch, turn: turn, id: id }) })

  /// `readiness_observed` with the basis captured at the evidence's intake, consuming the
  /// candidate that backs it.
  pure def applyReady(o: Out, ev: Evidence, basis: int, turn: int): Out =
    val o1 = red(bump(o), ReadinessObserved)
    withState(o1, s => {
      ...s,
      evidenceCandidate: NoEvidenceCandidate,
      gp: { ...s.gp, readinessSeq: s.seq, evidence: ev, basis: basis, evidenceLaunch: s.launch, evidenceTurn: turn },
    })

  /// Whether evidence `ev` may be taken in for the current launch and turn, given the
  /// candidate the launch holds: `SessionStart` backed by Claude's own, a settle completion
  /// only for the `Stop` it names when that is still the current turn's candidate, and a
  /// post-decision signal on its own entry for a turn that started (the CLI emits it after
  /// every `Stop` hook decided, whatever the daemon has taken in yet).
  pure def evidenceBacked(s: State, c: EvidenceCandidate, ev: Evidence, stop: int): bool = and {
    kindAllowed(s.cfg, ev),
    match ev {
      | SessionStart => c == SessionStartCandidate and stop == 0
      | StopSettled => stopBacks(s, c, stop)
      | PostDecision => s.turnNo >= 1 and stop == 0
      | OwnerAsserted => false
    },
  }

  /// Whether `ready(ev, stop)` may be observed now: the evidence is of the current bound
  /// live launch, whose hooks fire, and is backed (`evidenceBacked`).
  pure def readyAllowed(s: State, ev: Evidence, stop: int): bool = and {
    alive(s),
    s.gp.bound,
    s.hooksLive,
    evidenceBacked(s, s.evidenceCandidate, ev, stop),
  }

  /// The turn an evidence of kind `ev` belongs to: the bind turn for `SessionStart`, the
  /// current turn otherwise.
  pure def evidenceTurnOf(s: State, ev: Evidence): int = if (ev == SessionStart) 0 else s.turnNo

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

  /// `launch_reserved` (mvp §2.9 "Reservation"): only after the previous launch exited with
  /// death confirmed. A Codex positional first prompt is created `sending` with it (§2.3).
  action reserve(c: LaunchConfig): bool = all {
    st.thread.process == Exited,
    st.deathConfirmed,
    st.launch < MAX_LAUNCHES,
    c.adapter != CodexPositional or firstFree(st) != 0,
    val l = st.launch + 1
    val o1 = red(bump(start(st)), Reserved(st.launch > 0))
    val fresh = {
      ...o1.st,
      launch: l,
      cfg: c,
      deathConfirmed: false,
      sessionStartSeen: false,
      turnNo: 0,
      hooksLive: c.adapter != CodexManual,
      evidenceCandidate: NoEvidenceCandidate,
      gp: { ...NO_GATE_EVIDENCE, needsBracketedPaste: c.needsBracketedPaste },
      writer: [],
      buffer: [],
      bufferedStop: 0,
      reservations: st.reservations.append(st.deathConfirmed),
    }
    val id = firstFree(st)
    val o2 = if (c.adapter == CodexPositional) {
      st: {
        ...fresh,
        inputs: fresh.inputs.put(id, newInput(Sending, l, true)),
        sendCount: incr(fresh.sendCount, id),
      },
      calls: o1.calls.append({ kind: Create({ id: id, launch: l, initial: Sending }), ok: true, changes: [] }),
    } else { st: fresh, calls: o1.calls }
    commit(Reserve(c), o2),
  }

  /// A resume refused because the old launch's death is not confirmed raises
  /// `resume_blocked` (mvp §2.9 "Daemon restart"); the reducer refuses `Reserved` from then on.
  action reserveRefused: bool = all {
    st.thread.process == Exited,
    not(st.deathConfirmed),
    val o1 = if (st.thread.attention.contains(ResumeBlocked)) start(st)
      else red(bump(start(st)), AttentionRaised(ResumeBlocked))
    commit(ReserveRefused, red(o1, Reserved(true))),
  }

  /// `launch_started`; it moves a positional prompt to `sent` with `sent_after_seq` =
  /// its seq, in the same transaction (mvp §2.3 "Codex positional first prompt").
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
    commit(SpawnFail, withState(o3, s => { ...s, deathConfirmed: true })),
  }

  /// A process existed and its setup then failed (`SpawnError::StartedThenFailed`): the
  /// positional prompt may have run, so it is `unconfirmed` through `LaunchExited`; death is
  /// confirmed exactly when the backend confirmed the cleanup (`dc`).
  action spawnStartedThenFailed(dc: bool): bool = all {
    st.thread.process == Starting,
    val o1 = red(bump(start(st)), ExitedBy(ExitSpawnFailed))
    val o2 = recoverInputs(o1, st.launch, LaunchExited)
    commit(SpawnStartedThenFailed(dc), withState(o2, s => { ...s, deathConfirmed: dc })),
  }

  /// The launch's `SessionStart` arrived; binding (its ancestry check) may still be pending.
  /// As a later hook it invalidates pending candidates.
  action sessionStartHook: bool = all {
    alive(st),
    st.hooksLive,
    not(st.sessionStartSeen),
    commit(SessionStartHook, start({ ...st,
      sessionStartSeen: true, evidenceCandidate: NoEvidenceCandidate, bufferedStop: 0 })),
  }

  /// A human starts the first turn of a manual Codex launch through attach (an attach
  /// write); its hooks fire from then on (mvp §2.7, §2.9 "Resume").
  action manualFirstTurn: bool = all {
    st.thread.process == Running,
    st.cfg.adapter == CodexManual,
    not(st.hooksLive),
    val o1 = bump(start(st))
    commit(ManualFirstTurn, withState(o1, s => { ...s,
      hooksLive: true,
      writer: s.writer.append(WHuman),
      gp: { ...s.gp, lastWrite: s.seq, lastHumanWrite: s.seq },
    })),
  }

  /// `session_bound`, also a turn start (mvp §2.1:115), then the buffered hooks of the
  /// launch in intake order, each with its intake facts (mvp §2.9 "Binding"). Claude's
  /// `SessionStart` is readiness evidence.
  action bind: bool = all {
    alive(st),
    st.sessionStartSeen,
    not(st.gp.bound),
    val o1 = red(bump(start(st)), SessionBound)
    val o2 = withState(o1, s => { ...s,
      gp: { ...s.gp, bound: true, boundSeq: s.seq, lastTurnStart: s.seq },
      evidenceCandidate: if (s.cfg.adapter == Claude) SessionStartCandidate else NoEvidenceCandidate,
    })
    val o3 = st.buffer.foldl(o2, (acc, h) => match h {
      | BufPrompt(p) => applyPrompt(acc, p.matches, p.facts)
      | BufStop(id) => applyStop(acc, acc.st.turnNo, id)
      | BufReady(r) =>
        if (evidenceBacked(acc.st, acc.st.evidenceCandidate, r.kind, r.stop))
          applyReady(acc, r.kind, r.basis, evidenceTurnOf(acc.st, r.kind))
        else acc
    })
    // Replaying the buffer cannot revive a `Stop` that a later hook invalidated at intake:
    // an unsettled `Stop` candidate survives only if it is the one still live there.
    val live = st.bufferedStop
    commit(Bind, withState(o3, s => { ...s,
      buffer: [],
      bufferedStop: 0,
      evidenceCandidate: match s.evidenceCandidate {
        | StopCandidate(r) => if (r.id == live) s.evidenceCandidate else NoEvidenceCandidate
        | _ => s.evidenceCandidate
      },
    })),
  }

  /// Readiness evidence from the adapter (mvp §2.3 "Gate", `readiness_evidence` modes);
  /// `stop` names the `Stop` a settle completion was started for.
  action ready(ev: Evidence, stop: int): bool = all {
    readyAllowed(st, ev, stop),
    commit(Ready({ kind: ev, stop: stop }), applyReady(start(st), ev, st.writer.length(), evidenceTurnOf(st, ev))),
  }

  /// `chaldea ready`: accepted only for the current bound launch; otherwise refused,
  /// writing nothing (mvp §2.3 "Gate").
  action ownerReady: bool = all {
    st.launch > 0,
    commit(OwnerReady,
      if (alive(st) and st.gp.bound) applyReady(start(st), OwnerAsserted, st.writer.length(), st.turnNo)
      else start(st)),
  }

  pure def matchable(s: State, matches: Set[int]): bool =
    matches.forall(id => has(s, id) and s.inputs.get(id).launch == s.launch and isInFlight(stateOf(s, id)))

  pure def factsNow(s: State, matches: Set[int]): int -> AckRecord =
    matches.mapBy(id => intakeFacts(s, id, matches, s.seq + 1))

  /// A bound `UserPromptSubmit`. `matches` is the set of in-flight inputs of the current
  /// launch whose expected form equals the prompt: a nondeterministic choice standing for
  /// the text match.
  action prompt(matches: Set[int]): bool = all {
    alive(st),
    st.hooksLive,
    st.gp.bound,
    matchable(st, matches),
    commit(Prompt(matches), applyPrompt(start(st), matches, factsNow(st, matches))),
  }

  /// Hooks of an unbound live launch are buffered with their intake facts (mvp §2.9).
  action bufferPrompt(matches: Set[int]): bool = all {
    alive(st),
    st.hooksLive,
    not(st.gp.bound),
    st.buffer.length() < 3,
    matchable(st, matches),
    commit(BufferPrompt(matches), start({ ...st,
      buffer: st.buffer.append(BufPrompt({ matches: matches, facts: factsNow(st, matches) })),
      bufferedStop: 0,
    })),
  }

  action bufferStop: bool = all {
    alive(st),
    st.hooksLive,
    not(st.gp.bound),
    st.buffer.length() < 3,
    val id = st.stopCount + 1
    commit(BufferStop, start({ ...st,
      stopCount: id,
      buffer: st.buffer.append(BufStop(id)),
      bufferedStop: id,
    })),
  }

  /// A settle completion or post-decision signal of an unbound launch, buffered. A settle
  /// completion names its `Stop`, which must still be the buffered candidate.
  action bufferReady(ev: Evidence, stop: int): bool = all {
    alive(st),
    st.hooksLive,
    not(st.gp.bound),
    st.buffer.length() < 3,
    kindAllowed(st.cfg, ev),
    match ev {
      | StopSettled => st.bufferedStop == stop and stop != 0
      | PostDecision => stop == 0
      | _ => false
    },
    commit(BufferReady({ kind: ev, stop: stop }), start({ ...st,
      buffer: st.buffer.append(BufReady({ kind: ev, basis: st.writer.length(), stop: stop })),
      bufferedStop: 0,
    })),
  }

  /// A human write (`human_write`): attach and `key` set the last write and the last
  /// gate-closing human write. A card key sets the last write only, and the write boundary
  /// accepts it only while its card is open in its launch, which must be current; otherwise
  /// it is refused with nothing written (mvp §2.1:115, §2.3:174, §2.10:336).
  action humanWrite(o: HumanOrigin): bool = all {
    alive(st),
    match o {
      | CardKey(k) =>
        if (k.launch == st.launch and openCards(st).contains(k.card)) {
          val o1 = bump(start(st))
          val rec = {
            keyLaunch: k.launch,
            launch: st.launch,
            cardLaunch: if (st.cards.keys().contains(k.card)) st.cards.get(k.card).launch else 0,
            cardOpen: st.cards.keys().contains(k.card) and st.cards.get(k.card).state == CardIsOpen,
          }
          commit(HumanWrite(o), withState(o1, s => { ...s,
            gp: { ...s.gp, lastWrite: s.seq },
            writer: st.writer.append(WHuman),
            cardKeys: st.cardKeys.append(rec),
          }))
        } else commit(HumanWrite(o), start(st))
      | _ => {
          val o1 = bump(start(st))
          commit(HumanWrite(o), withState(o1, s => { ...s,
            gp: { ...s.gp, lastWrite: s.seq, lastHumanWrite: s.seq },
            writer: st.writer.append(WHuman),
          }))
        }
    },
  }

  /// A VT reply: a counted write with no event.
  action vtReply: bool = all {
    alive(st),
    commit(VtReply, start({ ...st, writer: st.writer.append(WReply) })),
  }

  /// `send` appends `input_created` (queued) for the current launch. Model assumption:
  /// only while the launch is `starting` or `running`.
  action send(id: int): bool = all {
    not(has(st, id)),
    st.launch > 0,
    st.thread.process == Starting or st.thread.process == Running,
    val o1 = bump(start(st))
    commit(Send(id), {
      st: { ...o1.st, inputs: o1.st.inputs.put(id, newInput(Queued, st.launch, false)) },
      calls: [{ kind: Create({ id: id, launch: st.launch, initial: Queued }), ok: true, changes: [] }],
    }),
  }

  /// The gate opened and the attempt, with the stream head, was persisted (mvp §2.3
  /// "States"); `input_state_changed` to `sending` advances the last write (§2.1:115). The
  /// attempt keeps the basis of the evidence it relied on.
  action open(id: int): bool = all {
    has(st, id),
    stateOf(st, id) == Queued,
    st.inputs.get(id).launch == st.launch,
    gate(gateView(st)) == Open,
    // Recorded from the state itself, not from the guard's view.
    val g = st.gp
    val laterTurn = g.lastTurnStart != 0 and g.lastHumanWrite < g.lastTurnStart and g.lastTurnStart < g.readinessSeq
    val rec = {
      bound: g.bound,
      running: st.thread.process == Running,
      idle: st.thread.turn == Idle,
      evidenceSeq: g.readinessSeq,
      lastWrite: g.lastWrite,
      sameTurnOk: g.lastHumanWrite == 0 or g.evidence == OwnerAsserted or laterTurn,
      otherInFlight: INPUT_IDS.exists(o => has(st, o) and st.inputs.get(o).launch == st.launch
        and Set(Sending, Sent, Unconfirmed).contains(stateOf(st, o))),
      pasteOk: not(g.needsBracketedPaste) or g.bracketedPasteOn,
    }
    val o1 = setInput(start(st), id, i => { ...i,
      sentAfterSeq: st.seq, attemptSeq: st.seq + 1, lastWriteBefore: st.gp.lastWrite, admittedBasis: st.gp.basis,
    })
    val o2 = bump(stp(o1, id, GateOpened(st.seq)))
    commit(OpenGate(id), withState(o2, s => { ...s,
      gp: { ...s.gp, lastWrite: s.seq },
      sendRecords: s.sendRecords.put(id, rec),
    })),
  }

  /// Whether the writer's revalidation before the first byte passes (mvp §2.3 "Writer
  /// revalidation"; docs/design/pty-runtime.md §2.3 "Basis"): no counted write since the
  /// admitted basis, the input's launch is current, and the whole gate, without the
  /// input's own attempt, is open.
  pure def recheckPasses(s: State, id: int): bool = and {
    s.inputs.get(id).launch == s.launch,
    s.writer.length() == s.inputs.get(id).admittedBasis,
    gate(recheckView(s, id)) == Open,
  }

  /// Passing the recheck dispatches text and submit as one unit; failing it cancels the
  /// input with zero bytes.
  action dispatch(id: int): bool = all {
    has(st, id),
    stateOf(st, id) == Sending,
    not(st.inputs.get(id).positional),
    not(st.inputs.get(id).dispatched),
    if (recheckPasses(st, id)) {
      val i = st.inputs.get(id)
      val rec = {
        writerLength: st.writer.length(),
        admittedBasis: i.admittedBasis,
        sameLaunch: i.launch == st.launch,
        bound: st.gp.bound,
        process: st.thread.process,
        turn: st.thread.turn,
        readinessSeq: st.gp.readinessSeq,
        evidence: st.gp.evidence,
        lastWrite: st.gp.lastWrite,
        attemptSeq: i.attemptSeq,
        lastWriteBefore: i.lastWriteBefore,
        lastHumanWrite: st.gp.lastHumanWrite,
        lastTurnStart: st.gp.lastTurnStart,
        otherInFlight: INPUT_IDS.filter(o => o != id and has(st, o) and st.inputs.get(o).launch == st.launch
          and Set(Sending, Sent, Unconfirmed).contains(stateOf(st, o))).size(),
        needsBracketedPaste: st.gp.needsBracketedPaste,
        bracketedPasteOn: st.gp.bracketedPasteOn,
      }
      val w = st.writer.append(WInput(id))
      val o1 = setInput(start(st), id, i => { ...i, dispatched: true, writePos: w.length() - 1 })
      commit(Dispatch(id), withState(o1, s => { ...s,
        writer: w, writeCount: incr(st.writeCount, id), dispatchRecords: s.dispatchRecords.put(id, rec),
      }))
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
        match c {
          | NoCandidate => o3
          | HasCandidate(r) => if (attributable(r)) ackWith(o3, id, r) else bump(stp(o3, id, AckNotAttributable))
        }
      }
    commit(Outcome({ id: id, completed: completed }), o2),
  }

  /// The ack window expired; suspended for a positional prompt while a startup cause is
  /// active (mvp §2.3 "Codex positional first prompt").
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
    commit(ResolveInput({ id: id, delivered: delivered }), withState(bump(stp(start(st), id, Resolved(delivered))),
      s => { ...s, ownerDelivered: if (delivered) s.ownerDelivered.union(Set(id)) else s.ownerDelivered })),
  }

  action stop: bool = all {
    st.thread.process == Running,
    commit(Stop, red(bump(start(st)), StopRequested)),
  }

  /// `launch_exited` of a started launch, with or without death confirmed. Model
  /// assumption: reason `stopped` exactly when a stop was accepted.
  action exit(reason: ExitReason, deathConfirmed: bool): bool = all {
    alive(st),
    (reason == ExitStopped) == (st.thread.process == Stopping),
    reason == ExitNormal or reason == ExitStopped,
    commit(Exit({ reason: reason, deathConfirmed: deathConfirmed }),
      exitLaunch(start(st), reason, deathConfirmed, LaunchExited, st.launch)),
  }

  /// A daemon crash and restart (mvp §2.4 "Restart", §2.9 "Daemon restart"): every open
  /// launch exits with death unconfirmed, then every input is recovered.
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
    commit(ConfirmDeath, withState(red(bump(start(st)), DeathConfirmed), s => { ...s, deathConfirmed: true })),
  }

  /// A `Stop` hook of turn `t`: the current turn, or a late one of the previous turn. A
  /// late `Stop` ends nothing (its turn is over), but as a later hook it replaces the
  /// pending candidate with one for its own turn, which backs no evidence of the current.
  action stopOfTurn(t: int): bool = all {
    alive(st),
    st.hooksLive,
    st.gp.bound,
    t >= 0,
    t == st.turnNo or t == st.turnNo - 1,
    val id = st.stopCount + 1
    val o1 = withState(start(st), s => { ...s, stopCount: id })
    commit(TurnStop(t), if (t == st.turnNo) applyStop(o1, t, id)
      else withState(o1, s => { ...s, evidenceCandidate: StopCandidate({ launch: s.launch, turn: t, id: id }) })),
  }

  action turnStop: bool = stopOfTurn(st.turnNo)

  /// Codex's `Interrupt` hook (Claude has none, mvp §2.2:135): supersedes the cards and
  /// invalidates the evidence candidate without making one.
  action interrupt: bool = all {
    alive(st),
    st.hooksLive,
    st.cfg.adapter != Claude,
    st.gp.bound,
    commit(Interrupt, closeCards(invalidate(red(bump(start(st)), Interrupted)), CardSuperseded)),
  }

  /// `approval_requested` for card `c` of the current turn.
  action cardOpen(c: int): bool = all {
    alive(st),
    st.hooksLive,
    st.gp.bound,
    not(st.cards.keys().contains(c)),
    val o1 = invalidate(red(bump(start(st)), ApprovalOpened))
    commit(CardOpen(c), withState(o1, s => { ...s,
      cards: s.cards.put(c, { launch: s.launch, turn: s.turnNo, state: CardIsOpen }) })),
  }

  /// Evidence correlated to card `c` (`approval_resolved` with `ran`); only when it was the
  /// last open card of the current turn does it feed `ApprovalsAllResolved` (mvp §2.10).
  action cardResolve(c: int): bool = all {
    openCards(st).contains(c),
    val o1 = invalidate(withState(bump(start(st)), s => { ...s,
      cards: s.cards.set(c, { ...s.cards.get(c), state: CardRan }) }))
    val lastOfTurn = openCards(o1.st).filter(o => o1.st.cards.get(o).turn == st.cards.get(c).turn).size() == 0
    commit(CardResolve(c), if (lastOfTurn) red(o1, ApprovalsAllResolved) else o1),
  }

  /// Evidence correlated to a card that `Stop`, an interrupt, a prompt or exit already
  /// closed: it resolves nothing and feeds no reducer input, but as a later hook of the
  /// launch (`PostToolUse`) it invalidates the evidence candidate.
  action lateResolve(c: int): bool = all {
    st.cards.keys().contains(c),
    Set(CardSuperseded, CardExpired).contains(st.cards.get(c).state),
    commit(LateResolve(c), if (alive(st) and st.cards.get(c).launch == st.launch) invalidate(start(st)) else start(st)),
  }

  /// The launch unbound 20 s after `launch_started` of a non-manual launch (mvp §2.2:142):
  /// a `SessionStart` that did not bind leaves it unbound (hook-ingest:268). The timer
  /// belongs to the current launch and also runs while `stopping`.
  action sessionTimer: bool = all {
    alive(st),
    not(st.gp.bound),
    st.cfg.adapter != CodexManual,
    not(st.thread.attention.contains(SessionStartTimeout)),
    commit(SessionTimer, red(bump(start(st)), AttentionRaised(SessionStartTimeout))),
  }

  /// An expected Codex `Hooks need review` after a CLI upgrade (mvp §2.2:143, §2.7).
  action stall: bool = all {
    st.thread.process == Running,
    not(st.gp.bound),
    st.cfg.adapter != Claude,
    st.cfg.upgraded,
    not(st.thread.attention.contains(StartupStall)),
    commit(Stall, red(bump(start(st)), AttentionRaised(StartupStall))),
  }

  action setTask(t: Task): bool = commit(SetTask(t), red(bump(start(st)), TaskSet(t)))

  /// DECSET 2004 set or reset in the VT.
  action decset(on: bool): bool = all {
    alive(st),
    commit(Decset(on), start({ ...st, gp: { ...st.gp, bracketedPasteOn: on } })),
  }

  // The simulator picks uniformly among the enabled branches of `any`, so the actions are
  // grouped: progress along a launch and an input, hooks, and disturbances.
  action lifecycleStep = any {
    nondet a = Set(Claude, CodexPositional, CodexManual).oneOf()
    nondet m = Set(StopSettle, PostDecisionMode, OwnerOnly).oneOf()
    nondet bp = Bool.oneOf()
    nondet u = Bool.oneOf()
    reserve(config(a, m, bp, u)),
    reserveRefused,
    confirmDeath,
    spawn,
    sessionStartHook,
    manualFirstTurn,
    bind,
    nondet ev = Set(SessionStart, StopSettled, PostDecision).oneOf()
    nondet sid = 0.to(st.stopCount).oneOf()
    ready(ev, sid),
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
    }
  }

  // Hooks: a prompt that matches an in-flight input, a buffered hook of an unbound launch,
  // and approval cards with their keys.
  action hookStep = any {
    all {
      inFlightIds(st).size() > 0,
      nondet ms = inFlightIds(st).powerset().exclude(Set(Set())).oneOf()
      any { prompt(ms), bufferPrompt(ms) },
    },
    bufferStop,
    nondet ev = Set(StopSettled, PostDecision).oneOf()
    nondet sid = 0.to(st.stopCount).oneOf()
    bufferReady(ev, sid),
    nondet c = 1.to(MAX_CARDS).oneOf()
    nondet kl = 1.to(MAX_LAUNCHES).oneOf()
    any { cardOpen(c), cardResolve(c), lateResolve(c), humanWrite(CardKey({ launch: kl, card: c })) },
    // A late `Stop` of the previous turn.
    stopOfTurn(st.turnNo - 1),
  }

  action disturbanceStep = any {
    nondet id = INPUT_IDS.oneOf()
    nondet d = Bool.oneOf()
    any { gateTimeout(id), cancelInput(id), rejectInput(id), resolveInput(id, d) },
    nondet ho = Set(Attach, Key).oneOf()
    humanWrite(ho),
    vtReply,
    prompt(Set()),
    stop,
    nondet r = Set(ExitNormal, ExitStopped).oneOf()
    nondet dc = Bool.oneOf()
    exit(r, dc),
    spawnFail,
    nondet sdc = Bool.oneOf()
    spawnStartedThenFailed(sdc),
    restart,
    interrupt,
    sessionTimer,
    stall,
    nondet t = Set(TaskNone, TaskActive, TaskCompleted).oneOf()
    setTask(t),
    nondet on = Bool.oneOf()
    decset(on),
  }

  // Progress is listed twice: it has twice the weight of hooks and disturbances.
  action step = any { lifecycleStep, lifecycleStep, inputStep, inputStep, hookStep, disturbanceStep }

  // ---------------------------------------------------------------------------
  // Inputs without the supervisor's guards: what the reducers alone admit
  // ---------------------------------------------------------------------------

  /// One reducer input fed with no supervisor guard; only the reducer decides.
  action raw(i: StateInput): bool = all {
    val o1 = red(start(st), i)
    val reserved = match i { | Reserved(_) => o1.calls[0].ok | _ => false }
    commit(Raw(i), withState(o1, s => { ...s,
      reservations: if (reserved) s.reservations.append(st.deathConfirmed) else s.reservations })),
  }

  /// One input step fed with no supervisor guard (replay coverage of refused steps).
  action rawStep(id: int, ii: InputInput): bool = all {
    has(st, id),
    commit(RawStep({ id: id, input: ii }), stp(start(st), id, ii)),
  }

  /// Readiness of an unbound launch reaching the reducer and the gate evidence directly,
  /// instead of being buffered.
  action rawReadyUnbound(ev: Evidence): bool = all {
    alive(st),
    not(st.gp.bound),
    commit(RawReadyUnbound(ev), applyReady(start(st), ev, st.writer.length(), st.turnNo)),
  }

  pure val RAW_INPUTS: Set[StateInput] = Set(
    Reserved(true), SessionBound, ReadinessObserved, PromptSubmitted,
    AttentionRaised(StartupStall), AttentionRaised(SessionStartTimeout), AttentionRaised(InputUnconfirmed(1)),
    ApprovalsAllResolved,
  )

  /// The contract's steps plus unguarded reducer inputs: `quint run --step stepUnguarded`
  /// finds the gaps of docs/design/formal-specs.md §3.
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

  /// mvp §2.3:171, :174, CL-5: `sending` through the gate only when bound, running, idle,
  /// with evidence newer than every write and not of the same turn as a gate-closing human
  /// write, no other in-flight input of the launch, and bracketed paste on where needed.
  /// The positional prompt (§2.3:186) has no record.
  val sendingOnlyThroughOpenGate = st.sendRecords.keys().forall(id => {
    val r = st.sendRecords.get(id)
    r.bound and r.running and r.idle and r.evidenceSeq > r.lastWrite and r.sameTurnOk
      and not(r.otherInFlight) and r.pasteOk
  })

  /// mvp §2.3:171-177, docs/design/pty-runtime.md §2.3 "Basis": at the first byte, the
  /// recheck holds, re-derived here from the facts the dispatch recorded rather than from
  /// `recheckView`/`gate`: the same launch, no counted write since the admitted basis, and
  /// every gate condition, where the input's own attempt does not count as a write.
  val dispatchOnlyThroughRecheck = st.dispatchRecords.keys().forall(id => {
    val r = st.dispatchRecords.get(id)
    val write = if (r.lastWrite == r.attemptSeq) r.lastWriteBefore else r.lastWrite
    val laterTurn = r.lastTurnStart > r.lastHumanWrite and r.readinessSeq > r.lastTurnStart
    and {
      r.sameLaunch,
      r.writerLength == r.admittedBasis,
      r.bound,
      r.process == Running,
      r.readinessSeq > 0,
      r.readinessSeq > write,
      r.lastHumanWrite == 0 or r.evidence == OwnerAsserted or laterTurn,
      r.turn == Idle,
      r.otherInFlight == 0,
      r.needsBracketedPaste implies r.bracketedPasteOn,
    }
  }) and st.writeCount.keys().forall(id => st.inputs.get(id).positional or st.dispatchRecords.keys().contains(id))

  /// mvp §2.3:171: no other input of the current launch is in flight, so at most one is.
  val oneInFlightPerLaunch = 1.to(MAX_LAUNCHES).forall(l =>
    st.inputs.keys().filter(id => st.inputs.get(id).launch == l and isInFlight(stateOf(st, id))).size() <= 1)

  /// mvp §2.3:171, CL-6: the gate's decision equals the decision with `in_flight` counted
  /// over the current launch's inputs only, so inputs of earlier launches never block.
  val earlierLaunchesNeverBlock = {
    val current = INPUT_IDS.filter(id => has(st, id) and st.inputs.get(id).launch == st.launch
      and Set(Sending, Sent, Unconfirmed).contains(stateOf(st, id)))
    last.gate == gate({ ...gateView(st), inFlight: current.size() > 0 })
  }

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

  /// mvp §2.2:143, §2.7: `startup_stall` only for a Codex launch after a CLI upgrade.
  val stallOnlyForUpgradedCodex =
    st.thread.attention.contains(StartupStall) implies (st.cfg.adapter != Claude and st.cfg.upgraded)

  /// mvp §2.10:333-336, state.rs:362-367: while a card of the current launch is open the
  /// turn awaits approval, so a resolution of another card cannot end it.
  val openCardsAwaitApproval = openCards(st).size() > 0 implies st.thread.turn == AwaitingApproval

  /// mvp §2.10:335: a resolution after `Stop`, an interrupt, a prompt or exit changes nothing.
  val lateResolutionChangesNothing = match last.act {
    | LateResolve(_) => st.thread == last.prevThread
    | _ => true
  }

  /// mvp §2.3:183, §2.4:214, CL-6: a restart turns `sending`/`sent` into `unconfirmed` and
  /// `queued` into `cancelled`.
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

  /// mvp §2.3:183, :185: once the launch exited, no input is `queued`, `sending` or `sent`.
  val exitedLaunchHasNoPendingInput =
    st.thread.process == Exited implies
      st.inputs.keys().forall(id => not(Set(Queued, Sending, Sent).contains(stateOf(st, id))))

  /// mvp §2.3:182, CL-7: every `AckAttributed` the step fed has a record of its intake
  /// facts, and every record is of the same launch, after the attempt, a unique match, a
  /// dispatched write, with no other counted write after it.
  val ackOnlyWhenAttributable = and {
    st.ackRecords.keys().forall(id => {
      val r = st.ackRecords.get(id)
      r.sameLaunch and r.afterAttempt and r.uniqueMatch and r.dispatched and r.noInterveningWrite
    }),
    last.calls.indices().forall(j => match last.calls[j].kind {
      | Step(c) => (c.input == AckAttributed and last.calls[j].ok) implies st.ackRecords.keys().contains(c.id)
      | _ => true
    }),
  }

  /// mvp §2.9:323, §8 "Also decided", CL-18: every reservation the reducer accepted came
  /// after the previous launch's death was confirmed.
  val reservedOnlyAfterDeath = st.reservations.indices().forall(j => st.reservations[j])

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

  /// The evidence each adapter produces per `readiness_evidence` mode (mvp §2.3, §2.7),
  /// written out independently of `kindAllowed`. Owner assertion is every mode's.
  pure val EVIDENCE_TABLE: Set[(Adapter, ReadinessMode, Evidence)] = Set(
    (Claude, StopSettle, SessionStart), (Claude, PostDecisionMode, SessionStart),
    (Claude, StopSettle, StopSettled), (CodexPositional, StopSettle, StopSettled), (CodexManual, StopSettle, StopSettled),
    (Claude, PostDecisionMode, PostDecision), (CodexPositional, PostDecisionMode, PostDecision),
    (CodexManual, PostDecisionMode, PostDecision),
  )

  /// mvp §2.3:171-173, §2.7: an open gate rests on evidence of the current launch and
  /// turn, of a kind the launch's adapter and mode produce (`EVIDENCE_TABLE`).
  val openGateEvidenceOfCurrentTurn = last.gate == Open implies and {
    st.gp.evidenceLaunch == st.launch,
    st.gp.evidenceTurn == st.turnNo,
    st.gp.evidence == OwnerAsserted or EVIDENCE_TABLE.contains((st.cfg.adapter, st.cfg.mode, st.gp.evidence)),
  }

  /// mvp §2.3:182-184: an input becomes `acked` only through an attributable intake
  /// record or the owner resolving it as delivered.
  val ackedHasCause = st.inputs.keys().forall(id =>
    stateOf(st, id) == Acked implies (st.ackRecords.keys().contains(id) or st.ownerDelivered.contains(id)))

  /// mvp §2.10:336: every card key the writer accepted was sent from a card open in the
  /// current launch.
  val cardKeysOnlyToOpenCards = st.cardKeys.indices().forall(j => {
    val r = st.cardKeys[j]
    r.keyLaunch == r.launch and r.cardLaunch == r.launch and r.cardOpen
  })

  /// mvp §2.2:135, §2.7:301: `Interrupted` is fed only for a Codex launch; Claude has no
  /// interrupt hook.
  val interruptedOnlyForCodex = last.calls.indices().forall(j =>
    last.calls[j].kind == Reduce(Interrupted) implies st.cfg.adapter != Claude)

  /// The supervisor feeds the reducers only inputs they accept, except the reservation of
  /// `ReserveRefused`, which must be refused.
  val supervisorInputsAccepted = match last.act {
    | ReserveRefused => {
        val n = last.calls.length()
        n > 0 and not(last.calls[n - 1].ok) and last.calls[n - 1].kind == Reduce(Reserved(true))
          and last.calls.indices().forall(j => j == n - 1 or last.calls[j].ok)
      }
    | Raw(_) => true
    | RawStep(_) => true
    | _ => last.calls.indices().forall(j => last.calls[j].ok)
  }

  val allInvariants = and {
    sendingAtMostOnce,
    writtenAtMostOnce,
    sendingOnlyThroughOpenGate,
    dispatchOnlyThroughRecheck,
    oneInFlightPerLaunch,
    earlierLaunchesNeverBlock,
    terminalStatesImmutable,
    neverResent,
    unconfirmedAttentionIff,
    gateTimeoutOnlyWhileQueued,
    noStartupCauseAfterBindOrExit,
    stallOnlyForUpgradedCodex,
    openCardsAwaitApproval,
    lateResolutionChangesNothing,
    restartRecovers,
    exitedLaunchHasNoPendingInput,
    ackOnlyWhenAttributable,
    reservedOnlyAfterDeath,
    resumeBlockedOnlyWhileDeathUnconfirmed,
    exitedTurnUnknown,
    taskOnlyByTaskSet,
    gateEvidenceIsBound,
    openGateEvidenceOfCurrentTurn,
    ackedHasCause,
    cardKeysOnlyToOpenCards,
    interruptedOnlyForCodex,
    supervisorInputsAccepted,
  }

  // Witnesses: reachability of the interesting states (`--witnesses`; `simulate` fails
  // when one is never reached).
  val witnessAcked = st.inputs.keys().exists(id => stateOf(st, id) == Acked)
  val witnessAckedThroughGate = st.ackRecords.keys().exists(id => not(st.inputs.get(id).positional))
  val witnessLateAck = match last.act {
    | Prompt(_) => last.prevInputs.keys().exists(id => last.prevInputs.get(id) == Unconfirmed and stateOf(st, id) == Acked)
    | _ => false
  }
  val witnessRecheckFailed = match last.act {
    | Dispatch(id) => stateOf(st, id) == Cancelled
    | _ => false
  }
  val witnessDismissed = st.inputs.keys().exists(id => stateOf(st, id) == Dismissed)
  val witnessSecondLaunch = st.launch == 2
  val witnessPositionalAcked = st.ackRecords.keys().exists(id => st.inputs.get(id).positional)
  val witnessResumeBlocked = st.thread.attention.contains(ResumeBlocked)
  val witnessOpenAfterEarlierUnconfirmed = last.gate == Open and
    st.inputs.keys().exists(id => st.inputs.get(id).launch < st.launch and stateOf(st, id) == Unconfirmed)
  val witnessBufferedPromotion = match last.act {
    | Bind => last.calls.length() > 1
    | _ => false
  }
  val witnessLateResolutionBesideOpenCard = match last.act {
    | LateResolve(_) => openCards(st).size() > 0
    | _ => false
  }
  val witnessTwoCardsOpen = openCards(st).size() == 2
}
