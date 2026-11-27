# Design: Intake Facts Under Lock Contention (fix/stop-facts)

Narrows supervisor-send L-12: hooks taken in while a needed lock is held lack intake facts.

## 1. Scope

**Changes.** `SupervisorProbe::facts`, the observation, `IntakeOrder::intake`, the writer's permission and positions, the settle basis.

**Does not change.** The input state machine, attribution, the fence word, `TurnLog`, the turn watermark, or the Quint model (facts missing still settle nothing). PTY data stays write-position metadata, never screen text.

## 2. Decisions

| # | Decision | Rejected alternative |
|---|---|---|
| D1 | The writer publishes `last_started` in a sequence-counted cell: under its queue lock it stores the counter odd, decides the permission (deadline and fence for a unit, dequeue for a reply), stores the changed class's position, then the counter even, every counter and position access `SeqCst`. A reader accepts a triple read between two equal even counter loads, retrying until its deadline. `ProbePoint::Dispatching` stays before the odd store; `ProbePoint::Publishing` sits inside it. | A bounded queue-lock wait (hooks depend on the writer's scheduling); `Acquire`/`Release` fences (the fence word and counter form a store-buffer pair only `SeqCst` orders). |
| D2 | `SendLaunch` holds its `Arc<LiveLaunch>`. `LiveLaunch.removed`, set in `Supervisor::remove`, gives `Unobservable`; otherwise `Published` → `Started`, `creation_started` → `Unobservable`, else `Reserved`; no live-map lock. | `detached`: a removal without `take_session` leaves it clear, and a stop sets it while the launch is live. |
| D3 | `SendState` is waited for up to the intake deadline; under it the probe copies the `Copy` facts, stores `latest_intake`, advances the fence, and releases it, then observes. | Holding it across the observation (the single store writer's open and exit jobs wait on it); a lock-free `SendState` (one observation point splits in two). |
| D4 | Intake deadline (real time) = `min(Instant::now() + INTAKE_WAIT, real(request deadline))`, `INTAKE_WAIT` = 500 ms: a full wait plus the 1 s pid wait stays inside attempt 1's 1.8 s, and a retry (`intake_reliable: false`) keeps none of the facts the wait is for; the waited sections are microseconds, and the observed early `SessionStart` (C1b e2e) came ~100 ms after spawn. | 1 s (wait plus pid wait passes attempt 1); a supervisor-clock deadline (a stopped test clock maps it a day ahead). |
| D5 | Each launch's intake entry has an async `intake` lock, taken until the request deadline and moved onto the blocking pool, where ordinal, facts, and `now()` run, and a `state` mutex (`next`, `pending`, `released`) with no wait inside; `IntakeTicket::drop` and `earlier_released` take only `state`. | One per-launch std mutex (a ticket drop on a Tokio worker waits behind an intake); inline waiting. |
| D6 | An intake waits for publication only while `creation_started`, unpublished, and not `removed`, before `SendState`; `set_published` and `remove` wake it. | `Unobservable` (the first `SessionStart` loses readiness). |
| D7 | The settle basis comes from `pass`'s entry in one read attempt; a read overlapping an update skips the pass (the next tick retries). | The map lock. |

Also rejected: an unbounded wait (a lock cycle stops intake), test retries (the defect is real-host), worker-side repair of a facts-missing `Stop` (revives stale ones, SB15).

## 3. Intake Contract

Steps:
1. Request deadline passed → 408, no ordinal.
2. Async `intake` lock.
3. On the blocking pool: deadline re-checked (passed → 408, no ordinal); ordinal reserved under `state`.
4. Publication wait (D6).
5. `SendState` (D3).
6. Observation (D1, D2).
7. `now()` (pid wait origin); release.

`SendState` not taken by the deadline: store `latest_intake`, advance the fence, `None`. An observation not taken by it: `None`, fence already advanced.

Ordering:
- **The intake point is step 5's section**; the ordinal orders the launch's hooks among themselves. A permission, completion, or record commit during steps 3–5 is before the intake point, like delays before intake (body, pre-read); a settle or open meanwhile shows this hook its unit running or recorded.
- `set_running`, `written`, human registration, and the open job's recheck run under `SendState`, so each is before or after the intake point; a record committing after the read leaves its unit running.
- **A write permitted before an intake's first counter read is in its facts**: permission and publication share the odd section, and the reader retries while it is odd. A later permission is refused (the intake moved the fence) or is a reply whose bytes follow its publication (`write_parts` runs after), so it cannot have caused the hook; `attribution::written_alone`, SB3's basis, and `human_at` miss no earlier write.
- `inflight.position` is visible whenever `written` is (publication, completion, `SendState`, read).
- No `SendState` holder takes an intake lock or drops an `IntakeTicket` (verified).

## 4. Hook Kinds M1 Needs

`UserPromptSubmit` (ack), `SessionStart` (readiness, `spawn --prompt`), `Stop` (settle), `CardOpened` (`human_at`); not `PostDecision` (every row is `StopSettle`). All keep facts unless §5 applies.

## 5. Remaining Limits and User-Visible Effect

- **Facts stay missing** (fail closed: the owner resolves `unconfirmed`, runs `ready`, or waits for the next card) when `SendState` is held, publication is late, or the writer stalls mid-publication past the intake deadline, or the launch is `removed`.
- **A request past its deadline gets 408.** The emitter's retry is `intake_reliable: false`: recorded, with no readiness, ack, or `human_at`.
- **While an intake waits** (≤ 500 ms), the launch's fence has not moved.

## 6. Invariants

New tests fail on `fb5f13a3`; kept ones pass on both.

| ID | Invariant | Test |
|---|---|---|
| I-1 | A hook taken in while the writer is parked at `Dispatching` has facts. | `probe::session_start_beside_parked_writer`, `human::same_turn_evidence_beside_parked_writer` |
| I-2 | A write permitted before an intake's counter read is in its facts, or the facts are missing. | `ack::reply_permitted_before_intake` |
| I-3 | No torn triple is accepted. | pty `writer::tests::published_positions` (fails without the re-check), `…_stress` (macOS arm64) |
| I-4 | `SendState` released within the deadline gives facts, held past it `None`, the fence advancing once; a later completion is no fact. | `probe::state_wait`, `ack::completion_during_intake` (no ack) |
| I-5 | A record committed during the wait counts before the intake point. | `turn_order::record_during_state_wait` |
| I-6 | A waiting intake delays no ticket drop, `earlier_released`, other launch, or timer. | `hook::tests::intake::waiting_intake_blocks_nothing` (current-thread runtime) |
| I-7 | A publication wait ends at publication, `remove`, or the deadline; removed is `Unobservable` at once; detached but unremoved is `Started`. | `claude::session_start_before_publication`, `…_after_deadline`; `live::observe_removed` |
| I-8 | An intake starting past the request deadline takes no ordinal and moves no fence. | `hook::tests::intake::deadline_before_ordinal`, `…_during_wait` |
| I-9 | A parked writer or held live map skips no settle. | `evidence::settle_beside_parked_writer`, `…_beside_held_map` |
| I-10 (kept) | A record after the read stays running; a unit asking its first permission with an older fence word gets `FenceMoved`; facts-missing hooks fail closed (PR10b-CL-4, CL-11; `observe` seam). | `record_during_intake_keeps_running`, `gate::fence`, `evidence`/`ack` facts-missing cases |

## 7. Acceptance Criteria

| ID | Criterion (EARS) | Test |
|---|---|---|
| SF-1 | When a Claude `SessionStart` is taken in while the writer holds its queue lock, the binding job shall append `readiness_observed { session_start }`. | `session_start_beside_parked_writer` |
| SF-2 | When a turn's `Stop` is taken in under that hold, the queued input shall reach `sent` after `SETTLE`. | `same_turn_evidence_beside_parked_writer` |
| SF-3 | If a reply was permitted before a `UserPromptSubmit`'s intake, the hook shall not ack an input whose unit started before it. | `reply_permitted_before_intake` |
| SF-4 | While `SendState` is held within the intake deadline, the intake shall wait and return facts; if held past it, `None` with the fence advanced. | `state_wait` |
| SF-5 | While one intake waits, other intake-order work and timers shall proceed. | `waiting_intake_blocks_nothing` |
| SF-6 | When a hook arrives after creation started and before publication, the intake shall wait until publication, removal, or the intake deadline. | `session_start_before_publication`, `observe_removed` |
| SF-7 | If the request deadline has passed when the intake starts, the hook shall get 408, taking no ordinal. | `deadline_before_ordinal` |
| SF-8 | While the writer or the live map is held, a settle candidate shall settle. | `settle_beside_parked_writer`, `settle_beside_held_map` |

## 8. Proposed Canonical-Doc Edits

1. supervisor-send §2.5 "Intake facts": §3.
2. supervisor-send §2.4 table, `stop_settled`: D7.
3. supervisor-send §2.8: D4's row.
4. supervisor-send PR10b-CL-7: §3's waits; I-1, I-2, I-4, I-7 tests.
5. supervisor-send L-12: "A hook whose intake does not get `L`'s `SendState`, publication, or a consistent position triple by the intake deadline (`INTAKE_WAIT`, 500 ms), or whose launch was removed, has its facts missing." Consequences stay; revisit when real-host intakes reach the bound.
6. supervisor-send PR10b-P-1: its L-12 exception per item 5.
7. supervisor.md §2.11 "Observation", "Where 10b plugs in", code block, identifiers: D1, D2 (`published_last_started` replaces `try_last_started_each`; no `Supervisor::observe`).
8. supervisor.md SD12, PR10a1-CL-30: D2; test `live::observe_removed`.
9. hook-ingest §2.3 step 5 and §2.4: D5; `facts` takes the deadline.
10. hook-ingest §2.11 H6 (step 5's 408), §5 resource paths and "Waits" (D4, D5; the pid wait starts after D6).
11. pty-runtime §2.3, §2.7, §6: D1's permission, `Publishing`, and reader retry.
