# Design: Intake Facts Under Lock Contention (fix/stop-facts)

Narrows supervisor-send L-12 (hooks taken in under a held lock lack intake facts).

## 1. Scope

**Changes.** The probe, observation, `IntakeOrder::intake`, writer permission and positions, fence-word ordering, settle basis.

**Does not change.** The input state machine, attribution, the fence word's layout and meaning, `TurnLog`, the turn watermark, the Quint model. PTY data stays write-position metadata, never screen text.

## 2. Decisions

| # | Decision | Rejected alternative |
|---|---|---|
| D1 | The writer publishes `last_started` in a sequence-counted cell: under its queue lock, counter odd (`wrapping_add`), the permission (deadline and fence for a unit, dequeue for a reply), the changed class's position, counter even; a refusal or empty reply lane returns to even with no position. Counter and position accesses, `fence_advance`'s successful CAS, and `Fence::moved`'s final load are `SeqCst`. A reader accepts a triple between two equal even counter loads. `Dispatching` precedes the odd store; `Publishing` follows a granted permission, before the position store. | A queue-lock wait (hooks depend on the writer's scheduling); `Acquire`/`Release` (fence word and counter form a store-buffer pair). |
| D2 | `SendLaunch` holds its `Arc<LiveLaunch>`. One observation: `removed` (set by `Supervisor::remove`) → `Unobservable`; `Published` → a triple, re-checking `removed` before returning `Started`; `creation_started` → `Unobservable`; else `Reserved`. No live-map lock. | `detached` (unset on some removals, set while a stopping launch is live). |
| D3 | Under `SendState` (waited for) the probe copies the facts, stores `latest_intake`, advances the fence, releases, then observes. | Holding it while observing (store-writer jobs wait); a lock-free `SendState` (splits the point). |
| D4 | Two deadlines (§3): the request deadline gates starting; the facts deadline `F` bounds waits. `INTAKE_WAIT` = 500 ms: with the 1 s pid wait it leaves 300 ms of attempt 1's 1.8 s (a budget, not a guarantee), since a retry (`intake_reliable: false`) keeps no facts; waited sections are microseconds, and C1b's early `SessionStart` came ~100 ms after spawn. | 1 s (passes attempt 1); a supervisor-clock `F` (a stopped test clock maps it a day ahead). |
| D5 | Per launch: an async `intake` lock (owned guard, moved to the blocking pool for steps 4–8) and a wait-free `state` mutex (`next`, `pending`, `released`), the only lock ticket drops and `earlier_released` take. | One std mutex per launch (ticket drops wait behind an intake); inline waiting. |
| D6 | An intake waits for publication only while `creation_started`, unpublished, and not `removed`, before `SendState`. `set_published` and `remove` change that predicate and notify under the condvar's mutex, so no wakeup is lost. | `Unobservable` (loses the first `SessionStart`). |
| D7 | The settle job makes one D2 observation of `pass`'s entry, no deadline; `Unobservable` (removed, or overlapping an update) skips the pass until the next tick. | The map lock; `Published` alone (settles removed entries). |

Also rejected: unbounded waits, test retries (real-host defect), worker-side repair of a facts-missing `Stop` (SB15).

## 3. Intake Contract

**Deadlines.**
- The **request deadline** (server clock) is checked on entry, after the async guard before dispatch, and at the start point; past it: 408 before routing, no ordinal.
- The **facts deadline** `F = min(Instant::now() at entry + INTAKE_WAIT, real(request deadline))`. Each wait (publication, `SendState`, triple) makes a first attempt whatever the time and accepts a later attempt only before `F`; so an intake starting after `F` with request time left makes one attempt per step and answers 200.

**Start point (cancellation).** Each request has a start cell (`Pending`, `Started`, `Cancelled`).
Dropping the request future swaps `Pending` → `Cancelled`; the closure, under `state`, checks the request deadline and swaps `Pending` → `Started` immediately before reserving the ordinal. Cancelled first: the closure releases the guard; no ordinal, `latest_intake` and fence untouched. Started first: the intake runs to its end; a later drop finishes it once with no marks.

**Steps.** (1) entry check; (2) async `intake` lock; (3) dispatch check; (4) start point, ordinal under `state`; (5) publication wait (D6); (6) `SendState` (D3); (7) observation (D1, D2); (8) `now()` (pid wait origin), release. `SendState` not taken: store `latest_intake`, advance the fence, `None`. Observation not taken: `None`, fence already advanced.

**Ordering.**
- **The intake point is step 6's section**; the ordinal orders the launch's hooks. A permission, completion, or record commit during steps 4–6 is before it, like delays before intake (body, pre-read).
- `set_running`, `written`, human registration, and the open job's recheck run under `SendState`, so each is before or after it; a record committing after the read leaves its unit running.
- **A write permitted before an intake's first counter read is in its facts**: permission and publication share the odd section; the reader retries while odd. A later permission is refused (the fence moved) or is a reply whose bytes follow publication, so it cannot have caused the hook; `attribution::written_alone`, SB3's basis, and `human_at` miss no earlier write.
- `inflight.position` is visible whenever `written` is.
- No `SendState` holder takes an intake lock or drops a ticket (verified).

## 4. Hook Kinds M1 Needs

`UserPromptSubmit` (ack), `SessionStart` (readiness), `Stop` (settle), `CardOpened` (`human_at`); not `PostDecision` (all rows `StopSettle`); all keep facts outside §5.

## 5. Remaining Limits and User-Visible Effect

- **Facts stay missing** (fail closed; the owner resolves `unconfirmed`, runs `ready`, or awaits the next card) when by `F` `SendState` is not taken, a creation-started launch is unpublished, or the writer is mid-publication; or when the launch is `removed`.
- **Past the request deadline:** 408 before routing; the emitter's retry is `intake_reliable: false` (recorded; no readiness, ack, or `human_at`).
- **During the publication and `SendState` waits** the fence is unmoved.

## 6. Invariants and Acceptance Criteria

Invariants in EARS; new tests fail on `fb5f13a3`, kept ones pass on both.

| ID | Criterion | Test |
|---|---|---|
| SF-1 | When a Claude `SessionStart` is taken in with the writer parked at `Dispatching`, the binding job shall record `session_start` readiness. | `probe::session_start_beside_parked_writer` |
| SF-2 | When a `Stop` is taken in under that park, the queued input shall reach `sent` after `SETTLE`. | `human::same_turn_evidence_beside_parked_writer` |
| SF-3 | If a reply was permitted before a `UserPromptSubmit`'s first counter read, its facts shall include the reply or be missing (no earlier unit acked). Red on `fb5f13a3`: facts missing; mutation (odd store after `Publishing`): acks. | `ack::reply_permitted_before_intake` |
| SF-4 | The reader shall never accept a mixed triple (updates between position loads); its retry returns the new triple. | pty `writer::tests::published_positions`, `…_stress` (macOS arm64) |
| SF-5 | While `SendState` is held until before `F`, the intake shall wait and return facts; held past `F`, `None`; the fence shall advance once, and a later completion be no fact. | `probe::state_wait`, `ack::completion_during_intake` |
| SF-6 | When a record commits during the `SendState` wait, it counts before the intake point. | `turn_order::record_during_state_wait` |
| SF-7 | While one intake waits, ticket drops, `earlier_released`, other launches, and timers shall proceed. | `hook::tests::intake::waiting_intake_blocks_nothing` |
| SF-8 | When a hook arrives after creation started and before publication, the intake shall wait until publication, removal (both waking it mid-wait), or `F`; removed shall be `Unobservable`, detached but unremoved `Started`. | `claude::session_start_before_publication`, `…_after_deadline`; `live::observe_removed`, `live::wake_after_wait_began` |
| SF-9 | If the request is cancelled or expired before the start point, the intake shall take no ordinal; a started one finishes once. | `hook::tests::intake::drop_waiting_for_pool`, `drop_after_ordinal`, `deadline_before_ordinal` |
| SF-10 | When an intake starts after `F` with request time left, it shall try each step once and answer 200; a publication after `F` counts as missing. | `pool_past_facts_deadline`, `claude::published_after_facts_deadline` |
| SF-11 | While the writer is parked at `Dispatching` or the map is held, a candidate shall settle; after removal, none shall. | `evidence::settle_beside_parked_writer`, `…_beside_held_map`, `…_after_removal` |
| SF-12 (kept) | A record after the read leaves its unit running; a first permission with an older fence word gets `FenceMoved`; facts-missing hooks fail closed (PR10b-CL-4, CL-11). | `record_during_intake_keeps_running`, `gate::fence`, `evidence`/`ack` facts-missing cases |

## 7. Proposed Canonical-Doc Edits

1. supervisor-send §2.5 "Intake facts" (§3), §2.4 `stop_settled` (D7), §2.8 (D4).
2. supervisor-send PR10b-CL-7: §3's waits; SF-1, SF-3, SF-5, SF-8 tests.
3. supervisor-send L-12: "A hook whose intake does not, by its facts deadline (`INTAKE_WAIT`, 500 ms), get `L`'s `SendState`, `L`'s publication when its creation started, or a consistent position triple, or whose launch was removed, has its facts missing; past the request deadline it is 408 before routing." Revisit when real hosts reach the bound.
4. supervisor-send PR10b-P-1: its L-12 exception per item 3; §2.3: the fence word's `SeqCst` accesses.
5. supervisor.md §2.11 "Observation", "Where 10b plugs in", code block, identifiers, SD12, PR10a1-CL-30, and the hook-ingest cross-reference's "§4 PR 10 row" note: D1, D2, no `Supervisor::observe`.
6. hook-ingest §2.3 step 5, §2.4: D5, §3's start point; `facts` takes `F`.
7. hook-ingest §2.11 H6 (408s), §4 PR 10 row (D2's observation, waits until `F`), §5 resource paths and "Waits" (D4, D5; pid wait after D6).
8. pty-runtime §2.2 (`published_last_started` replaces `try_last_started_each`), §2.3 (D1), §2.7 (`Publishing`, `BetweenPositionLoads`), §5 waits.
