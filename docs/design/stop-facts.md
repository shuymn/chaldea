# Design: Intake Facts Under Lock Contention (fix/stop-facts)

Narrows supervisor-send L-12.

## 1. Scope

**Changes.** Probe, observation, `IntakeOrder::intake`, writer permission and positions, fence ordering, settle.

**Unchanged.** Input state machine, attribution, fence-word layout and meaning, `TurnLog`, turn watermark, Quint. PTY data stays write positions, never screen text.

## 2. Decisions

| # | Decision | Rejected alternative |
|---|---|---|
| D1 | The writer publishes `last_started` in a sequence-counted cell; under its queue lock: counter odd, the permission (deadline and fence, or a reply's dequeue), the changed position, counter even (a refusal or empty lane: even, no position). Counter and position accesses, `fence_advance`'s successful CAS, and `Fence::moved`'s final load are `SeqCst`. Readers accept a triple between equal even counter loads. `Dispatching` precedes the odd store; `Publishing` follows a granted permission. | A queue-lock wait; `Acquire`/`Release` (fence word and counter form a store-buffer pair). |
| D2 | `SendLaunch` holds its `Arc<LiveLaunch>`. One observation: `removed` → `Unobservable`; `Published` → a triple, re-checking `removed` before `Started`; `creation_started` → `Unobservable`; else `Reserved`. `Supervisor::remove` clones the entry, releases the map, sets `removed`, then removes the entry, nesting no lock with the map. | `detached` (unset on some removals; set during stops). |
| D3 | Under `SendState` (waited for) the probe copies the facts, stores `latest_intake`, advances the fence, releases, then observes. | Holding it while observing; lock-free `SendState` (splits the point). |
| D4 | Two deadlines (§3): the request deadline gates starting; the facts deadline `F` bounds waiting, never acceptance. `INTAKE_WAIT` = 500 ms: with the 1 s pid wait it leaves 300 ms of attempt 1's 1.8 s (a budget, not a guarantee), since a retry (`intake_reliable: false`) keeps no facts; waited sections are microseconds; C1b's early `SessionStart` came ~100 ms after spawn. | 1 s; a supervisor-clock `F` (a stopped test clock maps it a day ahead); time-based acceptance (order comes from the odd section and fence). |
| D5 | Per launch: an async `intake` lock (owned guard, moved to the blocking pool for steps 4–8) and a `state` mutex (`next`, `pending`, `released`), the only lock ticket drops and `earlier_released` take; `state` and the outer map lookup hold short sections only. | One std mutex per launch; inline waiting. |
| D6 | An intake waits for publication only while `creation_started`, unpublished, and not `removed`, before `SendState`, by polling the `Published` and `removed` atomics; the waiting side takes no lock. | `Unobservable` (loses the first `SessionStart`); a condvar (unbounded mutex re-acquire). |
| D7 | Settle makes one D2 observation of `pass`'s entry, no deadline (`Unobservable` skips until the next tick); the job's re-check refuses `removed`. | The map lock; `Published` alone (settles removed entries). |

Also rejected: unbounded waits, test retries, worker-side repair (SB15).

## 3. Intake Contract

**Request deadline** (server clock), checked on entry, after the async guard before dispatch, and at the start point:
- 408 before the start point takes no ordinal.
- After it, the handler's last deadline check, made before its write observer registers, can still answer 408; the intake finishes once without marks, leaving an ordinal gap. After the registration hook-ingest §2.11 decides the reply (supervisor-send L-12).
- `F` never causes 408.
- Retries reuse the `emit_id`: newly recorded, `intake_reliable: false`; after a committed first delivery, its response replays.

**Facts deadline** `F = min(Instant::now() at entry + INTAKE_WAIT, real(request deadline))`:
- Each wait (publication, `SendState`, triple) backs off with sleeps capped by the time left to `F`, then makes one final attempt, returning within `F` plus one attempt.
- Facts are missing only when `removed`, when the final attempt of the `SendState` wait (the `SendLaunch` mutex) or of the triple (the position read) fails, or when the observation after the publication wait still finds `L` unpublished once its creation started; a final publication poll that misses is followed by that observation, which can still succeed.

**Start point (cancellation, unwind).** Each request has a start cell (`Pending`, `Started`, `Cancelled`).
- Dropping the request future swaps `Pending` → `Cancelled`; the closure, under `state`, claims (`Pending` → `Started`), then checks the request deadline, immediately before reserving the ordinal.
- **Cancelled first or claimed late:** it releases the guard; no ordinal, `latest_intake`, or fence changes.
- **In time:** the intake runs to its end; a later drop finishes it once with no marks.
- The ticket is owned from the reservation and fence cleanup armed at the advance: an unwind after it finishes once with no marks; one before releases only the ordinal.

**Steps.** (1) entry check; (2) async `intake` lock; (3) dispatch check; (4) start point, ordinal; (5) publication wait; (6) `SendState`; (7) observation; (8) `now()` (pid-wait origin). Without `SendState`: store `latest_intake`, advance the fence, `None`. Without observation: `None`.

**Ordering.**
- **The intake point is step 6's section**; the ordinal orders the launch's hooks; a permission, completion, or record commit during steps 4–6 precedes it.
- `set_running`, `written`, human registration, and the open job's recheck run under `SendState`; a record committing after the read leaves its unit running.
- **A write permitted before an intake's first counter read is in its facts**: permission and publication share the odd section, and the reader retries while it is odd.
  - A later permission is refused (the fence moved) or is a reply whose bytes follow publication, so it cannot have caused the hook.
  - `attribution::written_alone`, SB3's basis, and `human_at` miss no earlier write.

## 4. Hook Kinds M1 Needs

`UserPromptSubmit`, `SessionStart`, `Stop`, `CardOpened` (not `PostDecision`: rows are `StopSettle`).

## 5. Remaining Limits

Facts stay missing in §3's cases, failing closed (the owner resolves `unconfirmed`, runs `ready`, or awaits a card); retries keep no readiness, ack, or `human_at`; the fence is unmoved during the first two waits.

## 6. Invariants and Acceptance Criteria

EARS. Red: `old` fails on `fb5f13a3`; `mut` fails under the named mutation; `kept` passes on both.

| ID | Criterion | Test | Red |
|---|---|---|---|
| SF-1 | When a Claude `SessionStart` is taken in with the writer parked at `Dispatching`, the binding job shall record `session_start`. | `probe::session_start_beside_parked_writer` | old |
| SF-2 | When a `Stop` is taken in under that park, the queued input shall send after `SETTLE`. | `human::same_turn_evidence_beside_parked_writer` | old |
| SF-3 | If a reply was permitted before a `UserPromptSubmit`'s first counter read, its facts shall include the reply or be missing. | `ack::reply_permitted_before_intake` | mut: odd store after `Publishing` |
| SF-4 | Readers shall never accept a mixed triple; retrying returns the new one. | pty `published_positions`, `…_stress` (macOS CI) | mut: no second counter load |
| SF-5 | While `SendState` is held until before `F`, the intake shall wait and return facts; past `F`, `None`; the fence advances once; a later completion is no fact. | `probe::state_wait`, `ack::completion_during_intake` | old |
| SF-6 | A record committing during the `SendState` wait shall count before the intake point. | `turn_order::record_during_state_wait` | old |
| SF-7 | While an intake waits, ticket drops, `earlier_released`, other launches, and timers proceed. | `hook::tests::intake::waiting_intake_blocks_nothing` | old |
| SF-8 | When a hook arrives after creation and before publication, the intake shall wait until publication or removal (made after the wait began) or `F`. | `claude::session_start_before_publication` (old), `…_after_deadline` (kept), `live::observe_removed` (mut: `removed` ignored), `live::poll_sees_later_change` (mut: sleep straight to `F`) | per test |
| SF-9 | If cancelled or expired before the start point, the intake shall take no ordinal; after it, a 408, or a drop before the write observer registers, finishes it once. | `hook::tests::intake::drop_waiting_for_pool`, `deadline_before_ordinal`, `deadline_at_claim`, `hook::intake::tests::deadline_crosses_at_claim`, `drop_after_ordinal`, `deadline_during_wait` | mut: start cell ignored; dispatch check removed (a pool task appears); check before claim |
| SF-10 | When a wait reaches `F`, it shall return within `F` plus one attempt, with facts if that attempt saw them; past `F` with request time left, 200. | `probe::pool_past_facts_deadline`, `probe::state_at_facts_deadline` (mut: no final attempt); `claude::published_at_facts_deadline` (kept) | per test |
| SF-11 | While the writer is parked or the map held, a candidate shall settle; one removed before observation or job shall not. | `evidence::settle_beside_parked_writer` (old), `…_beside_held_map` (old), `…_after_removal` (kept), `…_removed_before_job` (old) | per test |
| SF-12 | If an intake unwinds after the fence advance, it shall finish once without marks, releasing `pending` and the fence; before, only its ordinal. | `hook::tests::intake::panic_after_advance`, `panic_before_advance` | mut: guard armed early or late |
| SF-13 | Kept: a record after the read leaves its unit running; an older fence word gets `FenceMoved`; facts-missing hooks fail closed (PR10b-CL-4, CL-11). | `record_during_intake_keeps_running`, `gate::fence`, facts-missing cases | kept |

## 7. Proposed Canonical-Doc Edits

1. supervisor-send §2.5 "Intake facts" (§3), §2.4 `stop_settled` (D7), §2.8 (D4), §2.3 (`SeqCst`), PR10b-CL-7 (SF-1, 3, 5, 8; the `SendMap` read stays the one wait with no own deadline).
2. supervisor-send L-12, and PR10b-P-1's exception: "A hook has its facts missing when its launch was removed, when the final attempt (the first made at or after its facts deadline: `INTAKE_WAIT`, 500 ms, or the request deadline if sooner) of its `SendLaunch` mutex wait or of its position read fails, or when its observation, after the publication wait, still finds `L` unpublished once its creation started." 408s: the handler's cutoff is its last deadline check before its write observer registers; after it the writer decides by hook-ingest's J rows.
3. supervisor.md §2.11 "Observation", "Where 10b plugs in", code block, identifiers, SD12, PR10a1-CL-30, the hook-ingest "§4 PR 10 row" cross-reference: D1, D2.
4. hook-ingest §2.3 step 5, §2.4 (D5, §3; `facts` takes `F`), §2.11 H6, §4 PR 10 row, §5 resource paths and "Waits" (the pid wait follows D6).
5. pty-runtime §2.2 (`published_last_started` replaces `try_last_started_each`), §2.3 (D1), §2.7 (`Publishing`, `BetweenPositionLoads`), §5 waits.
