# Design: Intake Facts Under Lock Contention (fix/stop-facts)

Narrows supervisor-send L-12: a hook taken in while a lock its facts need is held has no intake facts, so it records no readiness, acks nothing, or keeps no card basis.

## 1. Scope

**Changes.** `SupervisorProbe::facts` and the launch observation (send §2.5 "Intake facts", sup §2.11 "Observation"), `IntakeOrder::intake` (hook-ingest §2.4), `WriterObserver` (pty-runtime §2.2), and the settle job's basis (send §2.4).

**Does not change.** The input state machine, attribution, no resend of `unconfirmed`, the fence and its writer check, `TurnLog` candidates, the turn watermark, and the Quint model (facts missing still settle nothing).

## 2. Today

`facts` runs inline on a Tokio worker under the daemon-wide intake mutex and tries three locks once: `L`'s `SendState`, the live map, and the PTY writer's queue lock (the writer thread, the drain, `with_session` callers). Any held one, or an unpublished launch, means facts missing.

## 3. Decision

| # | Decision | Rejected alternative |
|---|---|---|
| D1 | The observation takes no lock. The writer publishes `last_started` per class under a single-writer sequence counter (odd while it stores the three positions under its queue lock, even after), and `WriterObserver::published_last_started(deadline)` returns a triple read between two equal even counter values, retrying until `deadline`. `SendLaunch` keeps its `Arc<LiveLaunch>`, so the probe reads `Published` (a `OnceLock`) and the entry's `detached` flag instead of the live map. | A bounded wait on the queue lock: every hook would still depend on the PTY writer thread being scheduled, and a writer parked at dispatch would cost the full bound. |
| D2 | `SendState` stays the one observation point and is waited for up to the intake deadline: one `try_lock`, a few yields, then 1 ms sleeps (Windows' 15.6 ms tick accepted). | A lock-free `SendState` snapshot (swap or sequence lock): `set_running` must be read after the fence advance and `recorded` before it, so the one point splits into two reads. |
| D3 | The intake deadline is `min(intake start + INTAKE_WAIT, request deadline)`, with `INTAKE_WAIT` = 1 s, the pid wait's bound. It is passed in: `IntakeProbe::facts(launch, ordinal, deadline)`. | One bound per lock: up to three bounds added together. |
| D4 | `IntakeOrder.launches` holds one `Arc<Mutex<LaunchIntake>>` per launch; the outer map lock covers only lookup and insertion. `intake` runs on the blocking pool, awaited until the request deadline (past it: 408). | Waiting inline: a stalled intake pins a Tokio worker and, under the global lock, every launch's intake. |
| D5 | An intake that finds `creation_started` and no `Published` waits for publication until the intake deadline before taking `SendState`, then observes as usual. | Keeping `Unobservable`: Claude's first `SessionStart` often arrives within ~100 ms of spawn (`spawn --prompt`). |
| D6 | The settle job reads its basis from `pass`'s own entry through `published_last_started(now)`, not `Supervisor::observe`; a torn read skips the pass (the next tick retries). | Keeping the map lookup: a contended map skips the settle with nothing in tests to retry it. |

Also rejected: an unbounded wait (a future lock cycle stops intake); retries in tests (the defect is real-host); worker-side repair of a facts-missing `Stop` (revives a stale `Stop`, SB15; needs new marks and a Quint change).

## 4. Ordering Against the Record and the Fence

The probe's steps, under the per-launch intake lock:

1. If unpublished, wait for publication (D5) before taking `SendState`.
2. Take `SendState` (D2). On it: read `written`, `running` (`Some` and `recorded` false), `human_unsettled`, and `Dispatch`; store `latest_intake`; advance the fence; read `Published.reference` and `published_last_started` (input class first); release.
3. If `SendState` is not taken by the deadline: store `latest_intake`, advance the fence, return `None` (today's contended path).

Why the order holds:

- `set_running`, a `Written` completion's `written`, human registration, and the open job's recheck all run under `SendState`, so each is before the intake or after it.
- A record that commits after step 2's read leaves the unit running for this intake (`turn_order::record_during_intake_keeps_running`).
- A unit whose open job read the fence before the advance is refused `FenceMoved` at its permission, unchanged.
- Positions read after the advance may include writes permitted after it, as today. A write permitted just before the advance but published after the read is missing; its permission followed the hook's receipt, and each consumer stays safe: a lower evidence basis makes SB3 refuse the next open, a lower `human_at` refuses the card key, and `inflight.position` is visible whenever `written` is true (the publication happens before the completion, through the queue lock and `SendState`).
- No `SendState` holder takes an intake lock or drops an `IntakeTicket`, so the wait cannot cycle.

## 5. Hook Kinds M1 Needs

M1 needs `UserPromptSubmit` (else a false `unconfirmed`), `SessionStart` (Claude's initial readiness, `spawn --prompt`), `Stop` (the settle), and `CardOpened` (`human_at` for card keys); `PostDecision` evidence is unused (every row is `StopSettle`). All get facts unless §6 applies, so a `Stop`-only facts part is not needed.

## 6. Remaining Limits

Facts stay missing, failing closed as today (the owner resolves the `unconfirmed` input, runs `ready`, or waits for the next `approval_requested`), when `SendState` is held past the intake deadline, publication comes later or creation fails, the writer is preempted mid-publication past the deadline, or the session has left (`detached`); `intake_reliable: false` is unchanged. A request deadline passing during the wait is 408, and the emitter retries with the same `emit_id` (hook-ingest §2.13).

## 7. Invariants

| ID | Invariant | Test |
|---|---|---|
| I-1 | A hook taken in while the PTY writer holds its queue lock has facts. | `send::tests::probe::session_start_beside_parked_writer` (`rig.claude()`'s steps with the writer parked at `ProbePoint::Dispatching` on a raw unit); `send::tests::human::same_turn_evidence_beside_parked_writer` (the second `Stop` so parked: `End { settle: true }`, input `Sent`) |
| I-2 | A `SendState` held and then released before the deadline gives facts; held past it, `None`, with the fence advanced once either way. | `send::tests::probe::state_wait` (holder on another thread); `probe` (deadline `now`: `None` at once) |
| I-3 | A record committed after the read keeps `running`; a `Written` completion is wholly before or after. | `turn_order::record_during_intake_keeps_running`, `ack::completion_during_intake` |
| I-4 | One launch's waiting intake never delays another launch's intake or the runtime. | `hook::tests::intake::other_launch_not_held`, `hook::tests::intake::wait_off_runtime` (a current-thread runtime stays live) |
| I-5 | A `SessionStart` taken in before publication has facts once publication comes within the deadline, and none after it. | `send::tests::claude::session_start_before_publication`, `…_after_deadline` |
| I-6 | A published position triple is never torn. | pty `writer::tests::published_positions` (a parked publication: `None` at deadline `now`, the new triple after release) |
| I-7 | A settle is not skipped for a parked writer or a held live map. | `send::tests::evidence::settle_beside_parked_writer`, `…_beside_held_map` |
| I-8 | The fence still refuses units permitted after an intake. | existing `send::tests::gate::fence`, `probe::fence_word` |

## 8. Acceptance Criteria

| ID | Criterion (EARS) | Test |
|---|---|---|
| SF-1 | When a Claude `SessionStart` is taken in while the PTY writer holds its queue lock, the binding job shall record `readiness_observed { session_start }`. | `session_start_beside_parked_writer` |
| SF-2 | When a turn's `Stop` is taken in while the PTY writer holds its queue lock, the queued input shall be dispatched after `SETTLE`. | `same_turn_evidence_beside_parked_writer` |
| SF-3 | While another holder keeps `L`'s `SendState` for less than `INTAKE_WAIT`, the intake shall wait and return facts; if it is held past the intake deadline, the intake shall return `None` and still advance the fence. | `state_wait` |
| SF-4 | While an intake of launch `A` waits, an intake of launch `B` shall complete without waiting for it. | `other_launch_not_held` |
| SF-5 | When a `SessionStart` arrives after creation started and before publication, the intake shall wait for publication until the intake deadline. | `session_start_before_publication` |
| SF-6 | If the request deadline passes during the intake, the hook shall get 408 and record nothing. | `hook::tests::intake::deadline_during_wait` |
| SF-7 | When the settle job builds its basis while the writer or the live map is held, it shall settle. | `settle_beside_parked_writer`, `settle_beside_held_map` |

## 9. Proposed Canonical-Doc Edits

1. supervisor-send §2.5 "Intake facts" and D1: the §4 steps.
2. supervisor-send §2.4 table, `stop_settled`: D6's basis.
3. supervisor-send §2.8: an intake row (`INTAKE_WAIT`, capped by the request deadline).
4. supervisor-send PR10b-CL-7: waits on `SendState` and publication until the intake deadline; `None` for an unobservable launch or a passed deadline; I-1, I-2, I-5 tests.
5. supervisor-send L-12 becomes: "A hook whose intake cannot take `L`'s `SendState` mutex, or whose launch is not published, by the intake deadline (`INTAKE_WAIT`, 1 s), or whose launch's session has left, has its facts missing." Consequences and the fail-closed paths stay; revisit when real-host captures show intakes reaching the bound.
6. supervisor.md §2.11 "Observation", code block, identifiers: D1 through the entry; `published_last_started` replaces `try_last_started_each`.
7. hook-ingest §2.4: D4; `facts` takes the deadline.
8. hook-ingest §6 "Waits" and §4's PR 10 row: "a per-launch lock and a probe bounded by the intake deadline".
9. pty-runtime §2.2 `WriterObserver`: D1's published positions.
