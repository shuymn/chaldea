# Design: Formal Specs (Quint)

Status: Q-A (issue #13, target 2) and Q-B, the Sync Seam (target 1), implemented in `specs/`. Cited as "mvp §N" for `docs/mvp-design.md`, "http §N" for `docs/design/http-surface.md`, "store §N" for `docs/design/event-store.md`.

**Roles.** Quint checks the design-stage contract under interleavings: what the daemon may feed the reducers, in which order, and what must then hold. It does not check the Rust code; `proptest` (`crates/core/tests/input_step.rs`, `state_reduce.rs`) checks each reducer alone, and a `stateright` model of the sync code (PR 17) is deferred (mvp §4 "Deferred"). Q-A's value is the coupled model: `ThreadState`, the gate projection, several inputs across two launches, approval cards, and attention, together. Q-B's is the Sync Seam protocol apart from its code (§7). The trace bridge (§4, §7.5) ties the specs to the code: committed Quint traces replay against the real `core` functions.

| File | What |
|---|---|
| `specs/thread_input.qnt` | Q-A: the model, its invariants and witnesses |
| `specs/thread_input_traces.qnt` | Q-A's named deterministic traces (`run trace_*`) |
| `specs/sync.qnt`, `specs/sync_traces.qnt` | Q-B read side: replay, paging, epochs, notify-then-pull (§7) |
| `specs/receipts.qnt`, `specs/receipts_traces.qnt` | Q-B command side: receipts (§7) |
| `specs/common.qnt` | helpers the Q-B modules share |
| `specs/traces/*.itf.json`, `traces/sync/`, `traces/receipts/` | those traces, exported as ITF and normalized |
| `specs/scripts/{quint,specs,simulate,traces}.mjs` | the Quint runner, the ordered list of spec families, the seeded simulation, the trace export and freshness check |
| `specs/package.json`, `pnpm-lock.yaml`, `pnpm-workspace.yaml` | Quint, exact-pinned; dependency build scripts denied |
| `crates/core/tests/quint_traces.rs`, `quint_sync_traces.rs` | the replay tests (Q-A; Q-B), with their shared trace loader and ITF decoders in `tests/itf/` |

## 1. What Q-A Models

Two layers in one module:

- **Reducer layer**: `reduce`, `stepInput`, and `gate` mirror `crates/core/src/state.rs` `reduce`, `input.rs` `step`, and `input.rs` `gate` branch for branch, including `StatusChanged` output. The trace replay keeps them equal.
- **Supervisor layer**: the actions, whose guards are the contract of mvp §2.2, §2.3, §2.4 "Restart", §2.7, §2.9, §2.10. They are the obligations PR 9, PR 10, and PR 13–14 implement.

**Bounds.** Two launches, three inputs, two approval cards (each with its own id), three buffered hooks; task states `none`, `active`, `completed`; reject reasons `control_character` and `spawn_failed`.

**Abstractions.**

- Stream positions are one monotonic counter `seq`; a position of 0 stands for `None`.
- The PTY writer is the list of writes of the current launch with their origin classes (`WInput(id)`, `WHuman`, `WReply`); a position is an index into it. The launch's `counted` classes are its policy (all three for every adapter, `ClassSet::ALL`, `docs/design/pty-runtime.md` D3); its counted position is the number of writes of those classes. Readiness evidence stores the counted position at its intake as its basis, and an input's attempt keeps the basis of the evidence the gate opened on.
- Prompt text is abstract: a `UserPromptSubmit` names the set of inputs of the current launch, in any state, whose expected form it equals (a nondeterministic choice); its ack candidates are the in-flight ones among them.
- Timers, the settle interval, the writer's record of a unit, write outcomes, the write stall, the ack window, and the gate timeout are explicit nondeterministic actions. A settle completion names the `Stop` it was started for, identified by launch, turn, and intake order.
- Hook actions are atomic, so a hook taken in between the gate's read and the unit's first write cannot be expressed; the daemon's hook fence covers it, and `dispatch` stands for the open job's gate, the fence, and the writer's checks together (`docs/design/supervisor-send.md` §7).
- A launch has a configuration: adapter (Claude, Codex with a positional prompt, Codex manual), readiness mode (`stop_settle`, `post_decision`, owner only), bracketed paste, for Codex whether the CLI was upgraded, and the counted origin classes.

**Model assumptions** (not fixed by the contract):

- Inputs are created only while the launch is `starting` or `running`; `launch_exited` has reason `stopped` exactly when a stop was accepted; the session timer also runs while `stopping`.
- Only hooks of the launch's selected session enter the model. Selecting it is PR 9's check: hooks of another session become `raw_unknown` (`foreign_session`, mvp §2.9).
- Hooks follow the adapter: `Interrupt` only for Codex (mvp §2.2, §2.7); a manual Codex launch fires none until a human starts the first turn through attach (`manualFirstTurn`; mvp §2.7).
- A post-decision signal is its own entry for the current turn and may arrive at any point after the turn started, before or after the daemon takes in the turn's `Stop`. The CLI emits it only after every `Stop` hook decided (mvp §2.3), so this over-approximates; PR 12 checks that such a signal exists and its order, and PR 13–14 attribute it on the adapter side if one is adopted.

Under `none`, Claude's `SessionStart` is no evidence either (mvp §2.3 "only `chaldea ready`"; `docs/design/supervisor-send.md` SB2).

**Not modelled.** The PTY writer lock and the concrete `Basis` (the daemon uses `last_started` per class), the hook fence, timer durations, the restart sid scan and descendant kills, worktree and close races, the prompt size limit (it refuses before any input exists), and the `vt_snapshot_taken` of a gate timeout (an output with no state). Ack expiry, settle completion, the write stall, and buffered promotion are modelled abstractly (`windowExpired`, `ready(StopSettled, stop)`, `writeStalled`, `bind`). The daemon's settle rule takes a subset of the settles the model allows, except in the accepted gap L-11 of `docs/design/supervisor-send.md`, where the model, which names a `Stop`'s turn, refuses a settle the daemon takes.

**State** (`st`, one record, and `last`, the step's record):

| Variable | Meaning | Source |
|---|---|---|
| `st.thread` | the three axes and the attention set (`ThreadState`) | mvp §2.2 |
| `st.seq` | the thread stream's head | mvp §2.1 "Streams" |
| `st.launch`, `cfg`, `deathConfirmed`, `sessionStartSeen`, `turnNo`, `hooksLive` | current launch and its configuration; its (or the last launch's) death confirmed; its `SessionStart` arrived; turns started in it; its hooks fire | mvp §2.7, §2.9 |
| `st.evidenceCandidate`, `stopCount` | the signal that may become readiness evidence: Claude's `SessionStart`, or a `Stop` identified by launch, turn, and intake order; any later hook of the launch invalidates it | mvp §2.3 |
| `st.gp` | gate evidence of the current launch: bound and its seq, readiness seq, kind, basis, launch and turn, last write, last gate-closing human write, last turn start, bracketed paste needed and on | mvp §2.1 |
| `st.writer` | writes of the current launch with their origin classes, in writer order | docs/design/pty-runtime.md §2.3 |
| `st.buffer`, `bufferedStop` | hooks of an unbound launch with their intake facts; the buffered `Stop` a settle completion may still name | mvp §2.9 |
| `st.inputs` | per input: state, launch, positional, `sent_after_seq`, its attempt's seq, last write before it and admitted basis, dispatched, write position, the writer's result for its unit, whether the unit may still write, whether the completion job applied the result, and whether `open` saw a competing human write | mvp §2.3; `docs/design/supervisor-send.md` §2.3, §2.5 |
| `st.cards` | per card: launch, turn, open / ran / superseded / expired | mvp §2.10 |
| `st.sendCount`, `writeCount`, `sendRecords`, `dispatchRecords`, `ackRecords`, `ownerDelivered`, `cardKeys`, `reservations` | history the invariants read: facts recorded where they happen | — |
| `last.act`, `last.calls`, `last.gate` | the action and its arguments, the reducer calls it made with their results, the gate decision after it | §4 |
| `last.prevThread`, `last.prevInputs` | the state before the step, for transition invariants | — |

**Actions** (`step` groups them: the simulator picks uniformly among enabled branches of `any`, so progress has twice the weight of hooks and of disturbances):

| Action | Contract | Source |
|---|---|---|
| `reserve(cfg)` | only after `exited` with death confirmed; resets the gate projection, writer, and buffer; a positional prompt is created `sending` | mvp §2.9, §2.3 |
| `reserveRefused` | death unconfirmed: raise `resume_blocked`; the reducer then refuses `Reserved` | mvp §2.9 |
| `confirmDeath` | `launch_death_confirmed` | mvp §2.1 |
| `spawn` / `spawnFail` / `spawnStartedThenFailed(dc)` | `launch_started` moves the positional prompt to `sent` with `sent_after_seq` = its seq; a spawn that never started rejects it; a process that started and then failed exits the launch `spawn_failed` with no `launch_started`, the positional prompt `unconfirmed` through `LaunchExited`, and death confirmed exactly when the cleanup was (`dc`) | mvp §2.3; `docs/design/supervisor.md` §2.3 |
| `manualFirstTurn` | a human starts a manual Codex launch's first turn through attach (an attach write); its hooks fire from then on | mvp §2.7 |
| `sessionStartHook`, `bind` | the launch's `SessionStart` arrives and invalidates pending candidates; binding records `session_bound` (a turn start), makes Claude's `SessionStart` the evidence candidate, and promotes the buffered hooks in intake order with their intake facts | mvp §2.1, §2.7, §2.9 |
| `bufferPrompt`, `bufferStop`, `bufferReady(ev, stop)` | hooks of an unbound live launch, buffered; a settle completion only for the buffered `Stop` it names while no later hook arrived; promotion applies it only if that `Stop` is then the current turn's candidate, and keeps an unsettled `Stop` as the candidate only if it was still live at intake | mvp §2.9 |
| `ready(ev, stop)` | evidence of the current bound live launch, of a kind its adapter and mode produce: `SessionStart` backed by Claude's own; a settle completion only for the `Stop` it names, of the current launch and turn, with no later hook since; a post-decision signal on its own entry once a turn started | mvp §2.3, §2.7 |
| `ownerReady` | `chaldea ready`: disabled while a unit of the launch may still write (`unit_running`); the bound launch only; otherwise refused, writing nothing | mvp §2.3; `docs/design/supervisor-send.md` SB15 |
| `prompt(matches)` | bound `UserPromptSubmit`: turn start, supersedes cards, invalidates the evidence candidate; ack attribution from the hook's intake facts for its one in-flight candidate (unique candidate, same launch, after the attempt, dispatched, written before the intake, no other counted write after it (VT replies do not count for a positional prompt), no matching input the owner resolved, no competing submission); an attributable record acks a `sent` or `unconfirmed` input, and a `sending` one (`sent`, then `acked`); otherwise a `sent` input becomes `unconfirmed` and the others are unchanged | mvp §2.3, §2.10; docs/design/pty-runtime.md §2.3 "Ack attribution"; `docs/design/supervisor-send.md` §2.5 |
| `humanWrite(o)` | `attach` and `key` set the last write and last human write; a card key carries its launch and card and sets the last write only; one whose card is not open in the current launch is refused, writing nothing | mvp §2.1, §2.3, §2.10 |
| `vtReply` | a counted write with no event | docs/design/pty-runtime.md §2.5 |
| `send`, `open` | `queued`; the gate opens when, beyond `gate`, no counted write started since the evidence's basis and no unit of the launch may still write; `sending` advances the last write, the attempt keeps the basis the gate opened on, records whether a gate-closing human write follows the last turn start, and starts its unit | mvp §2.3; `docs/design/supervisor-send.md` SB3, SB15 |
| `dispatch`, `dispatchFenced` | the writer's recheck: no counted write since the admitted basis, the input's launch current, and the whole gate open on a view without the input's own attempt; else, or on a refusal with no state cause (the fence moved, the deadline passed, a cancel), `cancelled` with zero bytes, the unit ended, and the last write set back to the one before the attempt when no later write moved it | mvp §2.3; docs/design/pty-runtime.md §2.3 "Basis"; `docs/design/supervisor-send.md` SB1 |
| `writerDone(id, ok)`, `outcome(id)`, `writeStalled(id)` | the writer's record of a dispatched unit (written or incomplete) ends the unit; the completion job then makes a `sending` input `sent` or `unconfirmed` and changes nothing for one that left `sending`; a dispatched unit with no record becomes `unconfirmed`, its unit still running | mvp §2.3; `docs/design/supervisor-send.md` SB4, SB15 |
| `windowExpired`, `gateTimeout`, `cancelInput`, `rejectInput`, `resolveInput` | the remaining input transitions; the window of a positional prompt is suspended while a startup cause is active | mvp §2.3 |
| `stop`, `exit(reason, dc)`, `restart` | stop; exit ends the launch's units, recovers its inputs, and expires its cards; restart exits the open launch with death unconfirmed and recovers every input; buffered hooks of a launch that can no longer bind are dropped (`raw_unknown`) | mvp §2.3, §2.4, §2.9 |
| `stopOfTurn(t)` (`turnStop` for the current turn), `interrupt` | the current turn's `Stop` supersedes the cards and becomes the evidence candidate; a late `Stop` of the previous turn ends nothing and leaves a candidate of its own turn, which backs no evidence; Codex's interrupt supersedes the cards and invalidates the candidate | mvp §2.2, §2.10 |
| `cardOpen(c)`, `cardResolve(c)`, `lateResolve(c)` | a card of the current turn; resolving the last open card of the turn feeds `ApprovalsAllResolved`; a resolution of a closed card feeds nothing; each invalidates the evidence candidate | mvp §2.10 |
| `sessionTimer`, `stall` | `session_start_timeout` for a non-manual live launch that is unbound (a `SessionStart` that does not bind, such as `foreign_session`, does not stop it); `startup_stall` for an upgraded Codex launch before binding | mvp §2.2, §2.7 |
| `setTask`, `decset` | `task_set`; DECSET 2004 | mvp §2.2, §2.3 |
| `raw(i)`, `rawReadyUnbound(ev)` | reducer inputs with no supervisor guard; `stepUnguarded` adds them | §3 |
| `rawStep(id, ii)` | an input step with no guard, for deterministic replay coverage of a rejected step in a named trace | §4 |

## 2. Invariants

All are checked by `pnpm run simulate` (§5). Each reads facts recorded where they happen, so breaking the guard it protects violates it (checked by mutating the guard). One mutation needs a larger run: dropping the gate-closing human write from the recheck view violates `dispatchOnlyThroughRecheck` after about 5,000 samples per seed, so at the CI size `trace_refused_recheck_same_turn_evidence` catches it.

| Invariant | Statement | Source |
|---|---|---|
| `sendingAtMostOnce` | an input enters `sending` at most once | CL-4; `input.rs:3-4` |
| `writtenAtMostOnce` | an input's text is dispatched at most once | arch §4.3; mvp §2.3 |
| `sendingOnlyThroughOpenGate` | `sending` through the gate only when bound, running, idle, evidence newer than the last write and not of the same turn as a gate-closing human write, no other in-flight input of the launch, bracketed paste on where needed, no counted write since the evidence's basis, and no running unit of the launch (the positional prompt is exempt); recorded from the state, not from the guard's view | mvp §2.3; CL-5; `docs/design/supervisor-send.md` SB3, SB15 |
| `dispatchOnlyThroughRecheck` | every dispatched input passed the recheck at its first byte, re-derived from the facts the dispatch recorded rather than from the guard's view: the same launch, no counted write since the admitted basis, and each gate condition with the input's own attempt not counted as a write | mvp §2.3 |
| `oneInFlightPerLaunch` | at most one in-flight input per launch | mvp §2.3 |
| `earlierLaunchesNeverBlock` | the gate equals the gate with `in_flight` counted over the current launch's inputs only | mvp §2.3; CL-6 |
| `terminalStatesImmutable` | terminal input states never change | mvp §2.3; `input.rs:36-46` |
| `neverResent` | nothing leads from `sending`/`sent`/`unconfirmed` back to `queued`/`sending`; `unconfirmed` leaves only to `acked` or `dismissed` | arch §4.3; mvp §2.3 |
| `unconfirmedAttentionIff` | `input_unconfirmed(id)` iff that input is `unconfirmed` | mvp §2.2 |
| `gateTimeoutOnlyWhileQueued` | `input_gate_timeout(id)` only while that input is `queued` | mvp §2.2 |
| `noStartupCauseAfterBindOrExit` | no startup cause once bound or exited | mvp §2.2 |
| `stallOnlyForUpgradedCodex` | `startup_stall` only for an upgraded Codex launch | mvp §2.2, §2.7 |
| `openCardsAwaitApproval` | while a card of the current launch is open the turn is `awaiting_approval` | mvp §2.10 |
| `lateResolutionChangesNothing` | a resolution after `Stop`, interrupt, prompt, or exit leaves `ThreadState` unchanged (it can still invalidate pending readiness) | mvp §2.10 |
| `restartRecovers` | a restart turns `sending`/`sent` into `unconfirmed` and `queued` into `cancelled` | mvp §2.4; CL-6 |
| `exitedLaunchHasNoPendingInput` | after exit no input is `queued`, `sending`, or `sent` | mvp §2.3 |
| `ackOnlyWhenAttributable` | every `AckAttributed` fed has a record of the hook's intake facts, and every record shows all eight (same launch, after the attempt, unique candidate, dispatched, no other counted write after it, written before the intake, no matching input the owner resolved, no competing submission), checked directly rather than through the guard's `attributable` | mvp §2.3; CL-7 |
| `noAckOverCompetingSubmission` | no hook ack of an input whose gate opened after a gate-closing human write following the last turn start | `docs/design/supervisor-send.md` §2.5 |
| `noAckOverEarlierMatch` | no hook ack when an input of the launch whose text matches left flight without a hook ack (`dismissed`, or `acked` by the owner) | `docs/design/supervisor-send.md` §2.5 |
| `noDispatchWhileUnitRunning` | at most one unit of a launch may write, so no input opens or dispatches while another's unit runs, a stalled one included | `docs/design/supervisor-send.md` SB15 |
| `reservedOnlyAfterDeath` | every reservation the reducer accepted came after the previous launch's death was confirmed | mvp §2.9; CL-18 |
| `resumeBlockedOnlyWhileDeathUnconfirmed` | `resume_blocked` only while exited with death unconfirmed | mvp §2.2 |
| `exitedTurnUnknown` | an exited launch's turn is `unknown` | mvp §2.2 |
| `taskOnlyByTaskSet` | the task axis changes only on `task_set` | CL-2 |
| `gateEvidenceIsBound` | gate evidence is of the bound launch, after its binding | mvp §2.3, §2.9 |
| `openGateEvidenceOfCurrentTurn` | an open gate rests on evidence of the current launch and turn, of a kind the launch's adapter and mode produce per `EVIDENCE_TABLE`, written out apart from the guard's `kindAllowed` | mvp §2.3, §2.7 |
| `ackedHasCause` | an input is `acked` only through an attributable intake record or the owner resolving it as delivered | mvp §2.3 |
| `cardKeysOnlyToOpenCards` | every card key the writer accepted came from a card open in the current launch | mvp §2.10 |
| `interruptedOnlyForCodex` | `Interrupted` is fed only for a Codex launch | mvp §2.2, §2.7 |
| `supervisorInputsAccepted` | the supervisor feeds the reducers only inputs they accept; `ReserveRefused` ends with a refused `Reserved` | — |

**Witnesses** (`witness*` in the spec; 12) count traces reaching any `acked` input, an attributed ack through the gate, a late ack, a failed recheck, a dismissal, a second launch, an attributed positional ack, `resume_blocked`, an open gate beside an earlier launch's `unconfirmed` input, a binding that promotes buffered hooks, a late card resolution beside an open card, and two open cards. `simulate` fails when one is never reached, so a model change that cuts coverage is caught.

**Model results.** Every invariant holds over the CI simulation. With one in-flight input per launch and attribution limited to the same launch, "a prompt matching more than one input acks none" (mvp §2.3) is unreachable; a unit test of the matcher in PR 10 keeps it. The projection counts an input's own `sending` as the last write and as in flight, so the writer's recheck cannot be `gate()` on the projected view unchanged (`trace_send_ack` shows `WriteAfterEvidence` right after `open`).

## 3. Gaps the Traces Show

`raw` and `rawReadyUnbound` feed the reducers what the supervisor's guards forbid, and `stepUnguarded` adds them to `step`. A manual reproducer, from `specs/`: `pnpm exec quint run thread_input.qnt --step stepUnguarded --invariant <name> --seed 1 --max-samples 1000 --max-steps 40 --backend typescript` finds a, c, d, and the card finding (`reservedOnlyAfterDeath`, `noStartupCauseAfterBindOrExit` and `stallOnlyForUpgradedCodex`, `gateEvidenceIsBound`, `openCardsAwaitApproval`), and an `input_unconfirmed` raised for an input that is not `unconfirmed` (`unconfirmedAttentionIff`; a search case with no named trace). The named traces fix one example of each of a, c, d, and the card finding and end with an `expect` stating the gap; the replay shows the real reducers accept every step.

**Reducer preconditions the supervisor enforces.** The Rust reducers do not receive the facts these guards need; each lives in the supervisor layer, and the PR named in mvp §4 owns it.

| Trace | What the reducers admit | Contract | Owner |
|---|---|---|---|
| `trace_hole_a_reserve_without_death` | after a restart, `Reserved` before any `resume_blocked`: `Exited` carries no death bit and `Reserved` checks only the attention flag (`state.rs:174`, `:278-288`) | mvp §2.9 | PR 10a1 |
| `trace_hole_c_stale_startup_cause` | `AttentionRaised(StartupStall)` while `exited`, which then survives `Reserved` into the next launch (`state.rs:369-371`) | mvp §2.2 | PR 10a1 |
| `trace_hole_d_unbound_readiness` | `ReadinessObserved` of an unbound Codex launch (`state.rs:333-336`); fed directly instead of buffered, it survives binding as gate evidence older than the binding | mvp §2.3, §2.9 | PR 9 |
| `trace_finding_resolution_without_card_identity` | `ApprovalsAllResolved` carries no card identity: fed for card 1 after card 2 opened in the next turn, it ends the wait for card 2 (`state.rs:362-367`) | mvp §2.10 | PR 10, mvp C1, X1 |

**Decided by PR 10b** (`docs/design/supervisor-send.md` §7):

- *Hole b, option B.* A zero-byte attempt gives the launch its write position back while no later write moved it, so the gate stays as it was (`trace_zero_byte_attempt_reopens`); a counted write after the basis still keeps the next input `queued` (`trace_refused_recheck_reply_after_basis`, `trace_stale_basis_keeps_queued`).
- *The positional anchor.* VT replies after `launch_started` do not count against a positional prompt (`trace_positional_vt_reply_acked`); attach and input writes do (`trace_positional_attach_not_attributable`); for an input written through the PTY a reply after its dispatch still counts (`trace_reply_after_dispatch_not_attributable`).
- *The competing-submission and earlier-match conditions.* A gate-closing human write after the last turn start when the gate opened (`trace_competing_human_submission`, with `trace_send_ack` as the clean control), or a matching input the owner resolved (`trace_earlier_dismissed_match_not_acked`), keeps the ack from attributing.
- *The written-at-intake ack and the running unit.* A hook acks a `sending` input only after its unit was written (`trace_ack_while_sending`, `trace_prompt_before_write_done`, `trace_incomplete_never_acked`); a stalled unit keeps `open` and `chaldea ready` refused until its record, also when new evidence opened the gate's projection (`trace_stall_keeps_gate_blocked`, `trace_stall_blocks_open_gate`).

**Races the contract refuses** (named `trace_refused_*`):

- evidence of the same turn as a `key` write arriving after the gate opened (`EvidenceOfSameTurn` at the recheck);
- a VT reply after the admitted basis (the recheck keeps the basis the send relied on, and the next input stays `queued` while the gate's projection is open);
- readiness after a Codex interrupt (no `Stop` backs it);
- a settle completion after a late card resolution of the same launch (a later hook);
- a late `Stop` of the previous turn backing the current turn, or a settle completion of a `Stop` that a later one replaced (the model refuses it; the daemon cannot tell such a `Stop` from the current turn's when the earlier turn's `UserPromptSubmit` was lost or that turn had a second `Stop`, and departs from this refusal there: `docs/design/supervisor-send.md` L-11);
- a buffered `Stop` followed by `SessionStart` (a later hook): neither a buffered settle completion nor one after binding counts, and no input is written (`trace_buffered_stop_after_session_start` shows a `Stop` buffered after `SessionStart` still settling);
- a key from a card that `Stop` superseded (nothing written, no event).

`trace_undispatched_match_ignored` shows a same-text hook before the dispatch acking nothing, `trace_cards_with_identity` a late resolution of a superseded card while the next turn's card is open, `trace_manual_first_turn` a manual Codex launch with no hook before its first turn, and `trace_post_decision_own_entry` post-decision evidence taken in before any `Stop`.

## 4. Trace Bridge

`pnpm run traces:write` runs `quint test <module>_traces.qnt --match ^trace_ --out-itf` for each traces module and writes `specs/traces/<test>.itf.json` (Q-A), `traces/sync/`, and `traces/receipts/` (Q-B), dropping the top-level `#meta` (timestamp and path) and putting one state per line. `pnpm run traces:check` regenerates them in the OS temp directory and fails on a missing, stale, or different file; it ignores CR before LF, and `.gitattributes` already checks `*.json` out with LF.

`crates/core/tests/quint_traces.rs` decodes the ITF with `serde_json` (already a `core` dependency; no `itf` crate) and, from `ThreadState::registered(task)`, applies every `last.calls` entry to the real `reduce` or `step` (or creates the input), asserting acceptance or rejection as recorded and, for `reduce`, the exact `StatusChanged` list. After each step it compares the thread state and every input state with the model's and evaluates `gate` on the step's projection, with `in_flight` from `blocks_gate` over the real input states, against `last.gate`.

The replay guards only the branches the traces record. `trace_coverage_*` reach the ones the others shadow (`BracketedPasteOff`, `InputInFlight`, `Interrupted`, `LaunchExited` from `queued` and `sent`, `Rejected`, `resolve --delivered`, a refused step); reverting any of those, `blocks_gate`, `Exited` resetting the turn, `Sending + RecheckFailed`, or the `WriteAfterEvidence` check fails the replay. Intermediate states inside one model step, `InvalidTransition.rule`, and branches no trace reaches are the `proptest` suites' job.

The replay runs in the `workspace` job on macOS and Windows with `cargo test`; Node is needed only to regenerate or check the traces.

## 5. Tooling and CI

Node and pnpm are in `mise.toml` (major ranges, exact versions and checksums in `mise.lock` for the three lock platforms); Quint is exact-pinned in `specs/package.json` and `pnpm-lock.yaml`, which records integrity hashes only, no registry URL. `specs/pnpm-workspace.yaml` denies every dependency build script (`allowBuilds`); Quint needs none (protobufjs's postinstall is not needed to run it). `package.json` sets no `packageManager`, so `mise.lock` stays the one pnpm pin. Every Quint command runs with `--backend typescript`, which evaluates inside the locked package; the default Rust backend downloads an evaluator binary at run time.

From `specs/`:

```
pnpm install --frozen-lockfile
pnpm run typecheck      # quint typecheck of every spec and traces file
pnpm run simulate       # each spec x seeds 1-4, pooled over the cores, --max-samples 1000, --max-steps 40, all invariants and witnesses
pnpm run traces:check   # spec and specs/traces/ agree
pnpm run traces:write   # regenerate specs/traces/ after a spec change
```

The `specs` job of `ci.yml` (`ubuntu-latest`, mise pinned in `mise-action`) installs Node and pnpm through mise and runs those checks. `lefthook.yml` runs typecheck and the trace check on commit and the simulation on push for changes under `specs/`, each after `pnpm install --frozen-lockfile` (about 50 ms when `node_modules` is up to date), and runs the workspace tests on push when a trace changes. `verifyDepsBeforeRun: error` in `pnpm-workspace.yaml` also makes every `pnpm run` fail while `node_modules` does not match `package.json` and the lockfile. The simulation size reaches every witness. The twelve runs (three specs, four seeds) run at most one per available core (`os.availableParallelism()`) and take about 40 s on a 12-core development machine (Q-A alone about 14 s per seed). The simulation is about 265 CPU-seconds, so CI's four vCPUs need about 70 s for it (estimated, not measured on CI).

**Apalache** (`quint verify`) is not in CI and is unverified: it needs a Java runtime and downloads Apalache, and whether it accepts the spec's list folds and maps of records is untested. To try it: `pnpm exec quint verify thread_input.qnt --invariant allInvariants --max-steps 10`.

## 6. Extending

- **PR 9** implements the buffering and promotion the model's `buffer*` actions and `bind` state (hole d).
- **PR 10** makes the supervisor layer real, in three parts: each guard here is a checklist item, and a changed guard changes the spec and its traces in the same PR.
  - **10a1** (`docs/design/supervisor.md`) implements the launch guards and closes holes a and c in the job that decides them; its guard-to-test table (supervisor §7) maps each daemon guard the core replay cannot see to its test. `sessionTimer` is guarded by `not(st.gp.bound)` (`trace_session_timer_after_unbound_session_start`), and `spawnStartedThenFailed(dc)` models a spawn that started and then failed (`trace_spawn_started_then_failed`, extending `trace_positional_spawn_outcomes`).
  - **10b**, the send and ack half: its guard-to-test table (`docs/design/supervisor-send.md` §7) maps each daemon guard the core replay cannot see to its test, the hook fence among them.
  - **10a2** (`docs/design/supervisor-recovery.md`) owns `confirmDeath`'s site: the `stop` intent with `assume_dead`, the owner's confirmation of a launch exited with death unconfirmed. No guard changes: the restart prober and `launch_evidence` change no state and stay outside the model. `trace_repeated_restart_then_assert` states that a second restart changes nothing the assertion depends on.
- **PR 13–14** correlate card resolutions and card keys by `approval_id` (the card finding).
- **PR 7, 8, 18** implement what Q-B states; §7.4 lists the obligations it hands them.
- **PR 17** (deferred): a `stateright` model would check the Rust sync code; Q-B checks the contract it implements (§7.7).
- **Phase 2 relay**: §7.1 says where it plugs into Q-B.

## 7. Q-B: Sync Seam

Q-B checks the Sync Seam protocol of mvp §2.4 and the PR 7 design (http §2.5–§2.8, §8 D14, D15) under interleavings. PR 7 implements its HTTP side (sync reads, the WebSocket, commands, receipts), PR 8 reconciles receipts at startup, and PR 18 owns the client rules (epoch changes, request invalidation, the backward resync); §7.4 hands each its obligations. It assumes PR 6's store guarantees (store §2.2, §2.7, §2.8): atomic commits, gapless per-stream `seq`, idempotency, an epoch fixed when the database is created, and one read snapshot per call. Two modules, simulated and traced separately:

- `specs/sync.qnt`: replay, paging, snapshots, epochs, stream existence, and notify-then-pull;
- `specs/receipts.qnt`: command receipts across retries, lost replies, deadlines, writer contention, and restarts.

### 7.1 What Q-B Models

Each module has the two layers of Q-A: a mirror of the `core` function, `decideReplay` of `sync::decide_replay` and `checkReceipt` of `command::check_receipt`, branch for branch, whose calls each step records for the replay (§7.5); and the protocol as actions whose guards and effects are the contract.

**Actors** (`sync.qnt`). One host, authoritative: one database with its epoch, a log per stream, and the set of streams that exist; a new database is the next epoch with empty streams, and a thread stream may not exist in it (`reset(threadKept)`); the old epochs' logs stay as ghosts, since a client may still hold their events. The host answers each request from one read: 404 for a stream that does not exist, otherwise `decide_replay` and a page or a snapshot; a read may also fail (408 at its cutoff, 503 `busy`). A host snapshot carries no events (`HostSnapshot`), a thread snapshot one recent event. Two clients, PWA-like: client 1 follows both streams, client 2 a nondeterministic set; each holds one epoch, a local copy per stream (events ending at its cursor, the cursor, the rendered state, whether older events exist), the heads its socket announced for the epoch held, `has_more` per stream, and its current forward and backward request per stream. The network: requests and responses are sets, so any can be lost, delayed, or overtaken (one request per connection orders none of them, mvp §2.5). Each socket keeps, as http §2.6 does, the heads it knows (`known`, merged by subscribes, refreshes, and commits) and builds one frame at send selection: `heads` when one is due (a subscribe or a refresh since the last), otherwise `stream_advanced` for a stream whose known head passed the one announced; a frame in flight can be lost. A daemon restart drops every socket and every request in flight; responses already sent may still arrive.

**Bounds.** Two streams (host, thread), two epochs, five events per stream and epoch, four requests plus responses in flight, event sizes 1 or 3 units. `decide_replay`'s bounds are scaled: `REPLAY_MAX_EVENTS` 2 (one model seq stands for 5 000), `REPLAY_MAX_BYTES` 4 units (one unit for 2 MiB); each comparison in `decide_replay` is linear, so the scaled call takes the same branch (§7.5).

**What a client renders** is a stream's projection, `Proj`: the latest `surface_update` and a counter of the thread's status changes. A snapshot carries it as of `as_of_seq`; events after it update it. A host snapshot also carries the thread's status from the same read, a field the thread stream owns, kept only as an initial value (F1).

**Client rules** (PR 18 implements them, §7.4):

| Rule | Source |
|---|---|
| A forward fetch sends the cursor and the epoch held (none before the first response) | mvp §2.4 "Replay" |
| A response is current, checked just before the client processes it, if its request is the latest of its stream and direction and was not timed out or made older by an event of F2 since; processing it ends the request; a response to any other request is discarded and changes nothing | F2 |
| Only a successful forward response (a page or a snapshot) with another epoch than the one held, none included, establishes or switches the epoch, and every local copy with it: all copies dropped, every other request older, every followed stream due; a page becomes the new copy only if it starts at the stream's beginning; a failed read, a backward page, and an older response never set the epoch | mvp §2.4 "Its first successful forward response … establishes its epoch" |
| A backward page with another epoch switches nothing and makes a forward resync due, which stays due until a forward request is issued after it or the stream is no longer followed (a response to an earlier forward request, processed only if still current, does not satisfy it); a backward page is fetched only while `has_more_before` (or a snapshot's `as_of_seq` above its recent events) says older events exist, from the oldest event held or `as_of_seq + 1` | mvp §2.4 "Backward history" |
| A same-epoch snapshot (a bound or `CursorAhead`) replaces the stream's copy and makes the stream's other requests older | mvp §2.4 "Snapshot fallback"; F2 |
| A followed stream is due while its fetch was asked for (first fetch, reconnect, epoch switch, failed read), while `has_more`, or while a head announced on the socket of the epoch held is above the cursor; a delivered response does not clear an announced head | mvp §2.4 "Live notifications"; http §8 D15 |
| A 404 ends following the stream: its copy is dropped and a `subscribe` without it is sent; `heads` lists only existing streams | §7.4 obligations (PR 18) |
| A failed forward read (408, 503) leaves the forward fetch due; a failed backward read only ends that request | §7.4 obligations (PR 18) |
| Reconnect: a new socket, a `subscribe` of the followed set, a refetch of every followed stream, announced heads forgotten | mvp §2.4 "the client refetches on every reconnect" |
| `subscribe` replaces the set (the empty set and the same set included); a later one replaces an unanswered one, answered by one `heads` for the latest set | http §2.6, §8 D15 |
| A field a snapshot carries for another stream is an initial value: the client shows it until it follows the owning stream and holds a valid copy of it, then renders it from that stream | F1 |

**Convergence is a bounded-liveness check.** The `quiesce` action restores connectivity (every client without a socket reconnects), stops writes and losses, and runs the protocol on one fixed fair schedule (responses, requests, subscribes, frames, due fetches, timeouts of lost requests, then up to two heads refreshes) for up to 60 drain iterations after reconnecting; `convergedAfterQuiesce` then requires every client to follow only existing streams and to hold the host's epoch, cursor, and rendered state for each. A bound rather than an "eventually": Quint's simulator checks state predicates, and an unbounded liveness property cannot fail in a finite run, while a step bound fails as soon as the protocol stalls. The schedule is one fair schedule, not all of them; the simulator samples the same transitions in every order as ordinary steps, and `trace_notified_during_fetch` and `trace_notified_after_response` fix both orders of a notification and a response. Inside a `quiesce`, the safety invariants are evaluated after every response delivered (`last.inner`): only a delivery changes a copy or a client's epoch, the host does not change, and the other steps only make a stream due or fetched, so no invariant can fail on them; an invariant that reads sockets, or that another step could falsify, must widen the steps checked (`drainStep`). About half the starts are a seeded race (`racingState`): client 1 holds a host snapshot of epoch 1, a forward snapshot of epoch 1 and a backward page of the next epoch are in transit in either order, the database having been replaced between them; the random walk alone almost never reaches it (0 of 6 000 traces). Every witness count of §7.6 comes from this mixed distribution.

**Actors** (`receipts.qnt`). Clients sending a command with a `command_id`, a token, and a request identity (two of each; identity 1 is a command completed in its intent job, `send`; identity 2 one with an effect, `stop`), and retrying with the same three at any time (a lost reply is a retry). Four things are kept apart per request (http §2.5, §2.8): its admission deadline (`deadline`: a body not read by then is 408 with nothing submitted, and the writer does not start an intent job after it), the end of the reply grace (`grace`), what the store made durable (`receipts`), and what the runner observed of the writer's reply (`observe`). One writer runs jobs one at a time in order (store §2.2): an intent job past its deadline replies `Expired` without running; otherwise it looks the receipt up and, for `New`, inserts it `in_progress` with the intent in one transaction (completed in the same job for `send`); its outcome is `Ok` or `CommitUnknown` that did or did not commit, the latter putting the writer in its failed state, where every queued job is `Failed`. The runner starts the effect once after it observed the intent committed with the effect pending, and submits the completion when the effect reports a result (200, or a stored 409 when it refused). The request replies once the runner observed an outcome, or when the grace ends from what the runner observed by then (http §2.8 table; PR7-CL-8): unobserved is 503 `outcome_unknown`, even with the receipt durable (http §8 D14), and an outcome observed after the grace does not change the reply. The runner outlives its request: it goes on observing, starts the effect, and completes the receipt after the reply. A restart decides the running job (its lookup recorded), drops every connection, runner, queued job, unobserved reply, and effect, and turns every `in_progress` receipt into `unknown_after_crash`.

**Not modelled.** Authorization (a stream the token may not read is the same 404 as a missing one), blobs, tickets, token revocation, the socket's read and send deadlines (a socket that misses them is a disconnect), `hello`, `busy` and queue-full replies to commands, the completion budget's retries (a completion that cannot commit is `CompletionFailed`), the shutdown drain, real time, and the store's internals.

**Phase 2 relay.** The host stays the only authority, so a relay is one more actor between the clients and the host. In `sync.qnt` it would hold a per-stream cache keyed by `stream_id` and `epoch`, answer `serve` from it (a head at most the host's, of the host's epoch or an older one) or forward the request, and send its own notifications; the client rules and every invariant stay as they are, and convergence is checked after the relay catches up. In `receipts.qnt` it forwards `send` attempts unchanged by `command_id`, possibly losing or repeating them, which the clients' retries already cover.

### 7.2 State and Actions

`sync.qnt` (`st`, one record; `last`: the action, the `decide_replay` calls, the clients before the step, the invariants inside a `quiesce`):

| Variable | Meaning |
|---|---|
| `st.host` | the current epoch, every epoch's log per stream, the streams each epoch has |
| `st.clients` | per client: epoch held, copy per stream, current forward and backward request per stream, fetch asked for, `has_more`, announced heads and the epoch of the socket that announced them, followed set; ghosts `owed` (resyncs asked for after the forward request was issued), `lastFwd`, `lastBwd`, `resetAt`, `cancelled` (generations) and `lost` (streams a lost frame named) |
| `st.sockets` | per client's socket: open, subscription set, unanswered subscribe, known and announced heads, `heads` due, the frame in flight |
| `st.reqs`, `st.resps`, `st.nextId` | requests not yet served and responses in transit, by request id |

| Action | What |
|---|---|
| `appendA(x, kind, size)` | the host commits one event; its head merges into every subscribed socket's `known` |
| `init` | nondeterministic starting host and followed sets, connected or not, or the seeded swap race (§7.1) |
| `restart`, `reset(threadKept)` | a daemon restart; a new database (the next epoch), with or without the thread stream, with a restart |
| `connectA(c)`, `disconnectA(c)`, `resubscribeA(c, w)` | reconnect (subscribe and refetch), a dropped socket, a new followed set (any, the empty and the same set included) |
| `fetchA(c, x, limit)`, `fetchBackA(c, x, limit)`, `timeoutA(c, x, dir)` | a forward fetch from the cursor, a backward page before the oldest event held, a request given up |
| `serveA(id)`, `failReadA(id)`, `loseReq(id)`, `loseResp(id)`, `deliverA(id)` | the host answers from one read; the read fails; the network loses a request or response; the client applies a response |
| `serverSubscribeA(c)`, `headsRefreshA(c)`, `sendFrameA(c)`, `recvNoteA(c)`, `dropNote(c)` | the latest subscribe is answered (`heads` due); a refresh (`heads` due); the socket selects and builds a frame; the client takes it; it is lost |
| `quiesce` | connectivity returns, writes stop, and the protocol runs up to 60 drain iterations, the safety invariants checked after each delivery (§7.1) |
| `rawFetch(c, x, q)` | a forward request with any query (replay coverage of refusals and `CursorAhead`) |

`receipts.qnt` (`st`; `last`: the action and the `check_receipt` calls):

| Variable | Meaning |
|---|---|
| `st.receipts` | per `command_id`: token, request identity, status (a stored result is a body and an HTTP status) |
| `st.queue`, `st.running`, `st.failed` | the writer's jobs, the job it runs, its failed state |
| `st.effects`, `st.attempts` | effects running; per request: identity, submitted, deadline passed, grace over, the writer's unobserved reply, what the runner observed, connection alive, replied |
| `st.intents`, `effectStarts`, `creator`, `stored`, `markedUnknown`, `afterUnknown`, `committed`, `byGrace`, `replies` | history the invariants read; `byGrace` is what each runner had observed when its grace ended or its request replied; each reply records the durable receipt, the observation, and whether its job was submitted |

| Action | What |
|---|---|
| `send(c, t, h)` | a request or a retry arrives; its body is being read |
| `submit(a)`, `deadline(a)`, `grace(a)` | the body was read in time and the intent job queued; the admission deadline passes; the reply grace ends and a request still waiting replies from what its runner observed |
| `writerStart`, `writerFinish(w)`, `writerFail` | the writer takes the next job (an expired intent job replies `Expired`), ends it `Ok` or `CommitUnknown`, or fails a queued job |
| `observe(a)` | the runner observes the writer's reply, before or after its request replied; on a committed intent with its effect pending it starts the effect |
| `effectDone(c, ok)` | the effect reports a result (200, or 409 when it refused); the runner queues the completion |
| `replyTo(a)` | the request replies once its runner observed an outcome, or 408 for a body not read by the deadline |
| `restart(committed)` | a crash and restart; the running job committed or not |

### 7.3 Invariants

All are checked by `pnpm run simulate`. Each reads facts recorded where they happen; the last column is the spec mutation of §7.6 that violates it.

| Invariant | Statement | Source | Violated by |
|---|---|---|---|
| `noGapNoDup` | a copy's events are consecutive and end at its cursor | mvp §2.4; CL-13 "without duplicates"; CL-9 | a forward fetch from one before the cursor |
| `heldEventsOnHost` | every event a client holds is the host's event at that position of the copy's epoch, the cursor is within that epoch's head, and the copy says older events exist exactly when its oldest event held is above 1 | mvp §2.4; CL-13 | a snapshot claiming older events when it holds all |
| `noMixedEpochs` | every copy and every event a client holds is of the epoch it holds | mvp §2.4 "switching epoch and local state together" | an epoch switch that keeps the other streams' copies |
| `renderedStateAtCursor` | what a copy renders is the host's projection at the copy's cursor | mvp §2.4 "the client renders that stream from the snapshot and the events after `as_of_seq`" | forward events not applied to the rendered state |
| `appliedKeepsHeld` | an applied response keeps what the copy held and adds what it carried: a forward page the held events plus its own with the cursor at its last, a backward page the held events plus its own, a snapshot exactly its recent events at `as_of_seq`; a response that establishes or switches the epoch starts from an empty copy | mvp §2.4 "Replay", "Backward history", "Snapshot fallback" | a forward page replacing the held events; an establishing response dropping its events |
| `olderResponsesChangeNothing` | a response of an older generation, by the ghosts recorded at the events of F2 (not by the guard), leaves the client unchanged | F2 | each dropped `clearRequests` call: on reconnect, epoch establishment or switch, dropped stream, same-epoch snapshot; a timeout that keeps the request current |
| `epochOnlyFromCurrentForward` | a client's epoch changes only on a successful forward response (a page or a snapshot), current by the ghosts, carrying that epoch | mvp §2.4 epoch rule and F2 | a backward page, or a failed read, setting the epoch |
| `epochNeverRegresses` | a client never goes back to an epoch the host left (the model numbers epochs in creation order to state it) | mvp §2.4 | applying responses of any generation |
| `owedResyncStaysDue` | a resync asked for after a stream's forward request was issued stays due, or a forward request is in flight, until a forward request is issued after it | mvp §2.4 "Backward history" ("The resync stays due …") | a same-epoch snapshot clearing the resync |
| `convergedAfterQuiesce` | after connectivity returns and writes stop, every client follows only existing streams and holds the host's state of each within 60 fair steps (§7.1) | mvp §2.4 "Live notifications"; CL-13; http §8 D15 | a stream due only on `need` and `has_more`, not on an announced head |
| `oneIntentPerCommand` | one `command_id` causes at most one intent | mvp §2.4 "Insert"; CL-8; PR7-CL-18 | `check_receipt` answering `New` for `in_progress` |
| `oneEffectPerCommand` | one `command_id` starts at most one effect, across retries and restarts (at most once; that a healthy runner starts and completes it is liveness, §7.4) | CL-8; PR7-CL-19 | starting the effect on a lookup |
| `receiptIffIntent` | a receipt exists exactly when its intent committed | mvp §2.4 "Insert" | an intent not counted |
| `replayOnlyToCreator` | a stored result, status and body, goes only to the token and request that created the receipt | mvp §2.4 `ReceiptLookup`; CL-8 | `check_receipt` ignoring the token |
| `othersLearnNothing` | another token or request never gets `in_progress` or `unknown_after_crash` | mvp §2.4 `Conflict`; http §2.8 "Receipt visibility" | reading the status before ownership |
| `unknownNeverReexecuted` | no intent or effect after `unknown_after_crash`, and the receipt stays so | mvp §2.4 "Restart" | an effect surviving the restart |
| `noInProgressAfterRestart` | no receipt is `in_progress` right after a restart | mvp §2.4 "Restart" | a restart that marks nothing |
| `timeoutOnlyWithoutIntent` | a request answered 408 never commits its intent, now or later | mvp §2.4 "A 408 describes the current attempt only" | 408 for any request past its deadline |
| `timeoutOnlyWhenExpiredObserved` | 408 only with nothing submitted or after the runner observed `Expired` | http §2.8 "Why 408 is exact"; PR7-CL-8 | 408 for an unobserved job |
| `inProgressOnlyWithReceipt` | 409 `in_progress` only when the receipt exists at reply time and the runner observed it (its intent committed, or the lookup found it `in_progress`) | http §8 D14 | 409 `in_progress` from the durable receipt alone |
| `replyFromObservedByGrace` | a submitted request's reply is the table's reply for what its runner had observed when the grace ended or it replied, whichever came first | http §2.8; PR7-CL-8 "taken at or after then" | a reply after the grace from a later observation |

**Witnesses**, built from recorded facts (the response, the client before and after the step, the reply records). `sync.qnt` (14): an epoch switch, convergence across epochs, a forward response of an older generation arriving, a backward page applied, a current backward page of another epoch leaving a resync due, each snapshot reason the protocol takes (`TooManyEvents`, `TooManyBytes`, `EpochChanged`), a current forward page that advanced the cursor with `has_more` and left the stream due, a stream only a heads refresh recovers after a lost frame named it, a resubscribe while a subscribe was pending, a 404 ending a followed stream, a failed read leaving the fetch due, a current forward response applied while a later resync is owed and leaving the stream due. `receipts.qnt` (13): a replay, a replay of a stored non-200, a replay to the same request after its 503 `outcome_unknown`, a 503 with the receipt durable and the result unobserved (D14), 409 `in_progress`, a duplicate while `in_progress`, 409 `unknown_after_crash`, 409 `conflict`, a 408 before submission, a 408 followed by the command running, a `CommitUnknown` that committed, a runner starting the effect after its request replied, `Expired` observed after the grace.

### 7.4 Findings

Four rules of mvp §2.4 come from what the model needed; it implements them as written there.

**The epoch rule** (mvp §2.4 "Snapshot fallback", "The client also checks every response's `epoch` …"). Only a successful forward page or snapshot establishes or switches the epoch, a forward page only when the client holds no epoch (afterwards it sends its epoch, so a mismatch comes as an `epoch_changed` snapshot), and a failed read never does (`epochOnlyFromCurrentForward`).

**F1. Snapshot fields of another stream** (mvp §2.4, "A snapshot's `as_of_seq` is the head of the requested stream …"). A host snapshot carries each thread's `ThreadInfo` (axes and attention), and a thread snapshot `thread.closed` and the current launch's worktree, read in the same read (store §2.7; http §2.7 "which can change without that stream advancing"). They change through events of another stream (thread-stream events; `thread_closed` and `worktree_changed` on the host stream), so the snapshot's stream does not advance and nothing tells a client following only that stream. A thread's initial `task_state` comes from `thread_registered` on the host stream, so a replay of the thread stream alone does not carry it either, and an agent token cannot read `host` (http §2.5), so an agent token allowed `GET /api/threads` takes its thread list and `closed` from there. The rule: the snapshot's value is an initial value, and the client renders the field from its owning stream once it follows it and holds a valid copy of it. `trace_cross_stream_field_from_owner`: client 1 gets a host snapshot, the thread's status changes, and after `quiesce` the thread copy shows the new status (`shownThreadStatus`, the model's rendering rule) while the host copy keeps the value read. The model's convergence covers each followed stream's own state.

**F2. The scope of a request generation** (mvp §2.4, "A client keeps one current request per stream and direction …"). It says what an older request generation is. The model checks whether a response is current just before the client processes it (`deliverA`), and processing it ends the request; opening a WebSocket, the first one included, invalidates every request issued before it, a backward one included (a history page in flight is discarded and not refetched; PR 18 reissues it, below). The named traces deliver an invalidated response for these parts: the latest request only (`trace_refused_older_generation`: an abandoned forward page arriving after the newer one would duplicate events); an epoch switch (`trace_refused_stale_epoch_after_switch`: a thread-stream page of the old epoch would switch the client back to the epoch the host left); the first establishment (`trace_refused_stale_epoch_at_first_establishment`: before the client holds an epoch, a host page of epoch 1 is in transit, the database is replaced, and a thread page of epoch 2 establishes the epoch; the host page would switch the client back to epoch 1); a snapshot replacing a copy (`trace_refused_backward_after_snapshot`: its earlier backward page would be prepended to the new copy with a gap); a timeout (`trace_refused_older_generation`). Reconnect and a dropped stream are covered by the simulation and its mutations (§7.6): `trace_reconnect_after_restart` checks the refetch and `trace_vanished_stream` the 404, without delivering an invalidated response. The model records each event as a ghost (`resetAt`, `cancelled`) apart from the client's own bookkeeping, so dropping any of the client's invalidations fails `olderResponsesChangeNothing` (§7.6).

**The backward resync** (mvp §2.4 "Backward history", "The resync stays due …"). A forward request consumes the stream's pending refetch when it is issued. A backward page of another epoch that arrives while that request is in flight asks for a resync the request cannot give: its response is of the old epoch. `trace_resync_kept_after_stale_snapshot`: a snapshot of epoch 1, requested before the database swap, arrives after a backward page of epoch 2; the client applies it, the resync stays due, and the next forward fetch switches the client to epoch 2.

**Model results.** Every invariant holds over the CI simulation. A client's cursor never passes the head of its epoch, so the protocol never meets `CursorAhead`; `trace_coverage_queries` reaches it with a raw query that sends the matching epoch and a cursor past the head. A forward replay page of another epoch reaches a client only when its query omitted the epoch. With announced heads kept across a delivery, a notification that arrives during a fetch still makes the next fetch due (`trace_notified_during_fetch`). A command's reply is fixed when its grace ends: `Expired` observed within the grace is 408 (`trace_expired_observed_within_grace`), observed after it 503 (`trace_refused_expired_after_grace`); the runner then starts and completes the effect, and the retry replays it (`trace_effect_after_outcome_unknown`). An intent job of a command with an effect that ends `CommitUnknown` after committing leaves the receipt `in_progress` with no effect started; PR 8 stops the daemon on the failed writer, and the restart turns it into `unknown_after_crash`, so that command never runs and its retry learns only that the outcome is unknown (`trace_commit_unknown_then_restart`), as store §2.2 states.

**Obligations handed on.**

- **PR 7, reads and WebSocket**: http §6.2 PR7-CL-9–11 (one read per answer, the 404 for an unknown stream, snapshot contents) and PR7-CL-16 (`heads` for the latest subscribed set, refreshes, a commit announced by `stream_advanced` or `heads`); a `heads` frame omits streams that do not exist.
- **PR 7, commands and receipts**: http §6.2 PR7-CL-8 (deadlines and 408) and PR7-CL-18–20 (receipts, the effect once with the receipt completed by the runner, fatal store outcomes reported). Liveness the model does not check: after the runner observed an intent committed with its effect pending, a healthy runner starts the effect and completes the receipt (PR 17's model, deferred).
- **PR 8**: `receipts_mark_unknown_after_crash` at startup, before commands are accepted.
- **PR 18**: the client rules of §7.1 with the epoch rule, F1, F2, and the backward resync; keep announced heads across a delivery (a `stream_advanced` or `heads` that arrives during a fetch); continue while `has_more` and page backward only while `has_more_before`; on a 404 for a followed stream (it is not in the current database, as after a database swap), stop following it: drop its copy, `subscribe` without it, and stop displaying it; retry a failed read (408, 503); refetch every followed stream on reconnect, and keep the intent to load history for a displayed stream across it: opening a socket discards a backward page not yet processed, so reissue the backward fetch from the current copy. A new, empty database holds no token, so it reaches the browser as a 401 on its next request, before any stream is read: the page deletes its credential (compare-and-delete by generation) and asks for a fresh link. Its fast-check reducer covers the `has_more` and reconnect cases.

### 7.5 Trace Bridge

`sync_traces.qnt` (21 traces) and `receipts_traces.qnt` (15) follow Q-A's naming: `trace_refused_*` end where the client discards a response or does not switch, or where a receipt reply is not what a reviewer might expect (503 rather than 408 or 409); `trace_coverage_*` reach `decide_replay` branches the protocol never takes (`limit` out of range checked first, `after_seq` with `before_seq`, the default limit, `CursorAhead`, `after_seq` at the head with the largest limit, and two reasons at once: `EpochChanged` over `CursorAhead`, `TooManyEvents` over `TooManyBytes`). `trace_byte_bound` puts a forward replay exactly at both bounds; `trace_forward_paging`, `trace_host_snapshot_then_backward`, and `trace_thread_snapshot_then_backward` check `has_more` and `has_more_before` and the first backward boundary (`as_of_seq + 1` after a host snapshot); `trace_stored_409_replayed` stores and replays a 409.

`crates/core/tests/quint_sync_traces.rs` replays every recorded call: `decide_replay` with each query and head multiplied by fixed scales (5 000 seqs, 2 MiB), after asserting that the trace's model bounds times those scales are the contract's 10 000 events and 8 MiB and that `core`'s constants equal them, the epochs as `epoch-<n>`, and the plan compared exactly; `check_receipt` with receipts, tokens, request hashes, and stored results (HTTP status and body) built from the model's numbers, the lookup compared exactly, the restart's lookup included. Reverting any branch of either function (the epoch check, `CursorAhead`, the event bound's `>`, the byte bound, `after_seq` with `before_seq`; the token check, the request-hash check, `in_progress`, `unknown_after_crash`, the stored status) fails it. Scaled values cannot sit one seq or one byte past a real bound, so the real `+1` boundaries and the branch order on unscaled values are guarded by `crates/core/tests/sync_command.rs`. The store, the HTTP and WebSocket layers, and the client are not replayed: no `core` function implements them.

### 7.6 Results

The CI size (seeds 1–4, 1 000 samples, 40 steps per spec) reaches every witness of both modules; the rarest are a stream only a heads refresh recovers (12 of 4 000 traces) and a runner starting the effect after its request replied (13). On a development machine one seed takes about 18 s for `sync.qnt` and 8 s for `receipts.qnt` alone (the pooled run: §5).

Each invariant has a spec mutation that violates it, judged by the exit status of `quint run` at the CI size with only that invariant (1 on all four seeds unless stated), and by the named traces (`quint test`):

| Mutation | Invariant | Seeds failing | Named traces failing |
|---|---|---|---|
| a forward fetch from one before the cursor | `noGapNoDup` | 4 | 4 |
| a snapshot claiming older events when it holds all | `heldEventsOnHost` | 4 | — |
| an epoch switch keeping the other copies | `noMixedEpochs` | 4 | 2 |
| forward events not applied to the rendered state | `renderedStateAtCursor` | 4 | 3 |
| a forward page replacing the held events | `appliedKeepsHeld` | 4 | `trace_forward_paging` |
| an establishing snapshot dropping its recent events | `appliedKeepsHeld` | 4 | `trace_thread_snapshot_then_backward` |
| an establishing page dropping its events | `appliedKeepsHeld` | 4 | 8 |
| no `clearRequests` on reconnect | `olderResponsesChangeNothing` | 4 | — |
| no `clearRequests` on an epoch switch | `olderResponsesChangeNothing` | 4 | 2 |
| no `clearRequests` when a stream is dropped | `olderResponsesChangeNothing` | 4 | — |
| no `clearRequests` on a same-epoch snapshot | `olderResponsesChangeNothing` | 4 | `trace_refused_backward_after_snapshot` |
| a timeout that keeps the request current | `olderResponsesChangeNothing` | 4 | 2 |
| the first establishment appending without a switch | `olderResponsesChangeNothing` | 4 | `trace_refused_stale_epoch_at_first_establishment` |
| no current-request check | `epochNeverRegresses` (and `olderResponsesChangeNothing`) | 4 | 4 |
| a backward page of another epoch switching the epoch | `epochOnlyFromCurrentForward` | 4 | 2 |
| a failed read establishing the epoch | `epochOnlyFromCurrentForward` | 4 | — |
| a same-epoch snapshot clearing a later resync | `owedResyncStaysDue` | 4 | `trace_resync_kept_after_stale_snapshot` |
| a stream not due on an announced head | `convergedAfterQuiesce` | 4 | 4 |
| a stream not due on `has_more` | — (heads recover it) | 0 | `trace_forward_paging` |
| `check_receipt` answering `New` for `in_progress` | `oneIntentPerCommand` | 4 | 1 |
| the effect started on a lookup | `oneEffectPerCommand` | 4 | 4 |
| an intent not counted | `receiptIffIntent` | 4 | 2 |
| `check_receipt` ignoring the token | `replayOnlyToCreator` | 4 | `trace_conflict` |
| the status read before ownership | `othersLearnNothing` | 4 | `trace_conflict` |
| an effect surviving the restart | `unknownNeverReexecuted` | 4 | — |
| a restart that marks nothing | `noInProgressAfterRestart` | 4 | 3 |
| 408 at the end of every grace | `timeoutOnlyWithoutIntent` | 4 | 6 |
| 408 for an unobserved job | `timeoutOnlyWhenExpiredObserved` | 4 | 5 |
| 409 `in_progress` from the durable receipt alone | `inProgressOnlyWithReceipt` | 4 | 3 |
| a reply after the grace from a later observation | `replyFromObservedByGrace` | 4 | 6 |

CI runs the four seeds and the named traces, so every mutation fails CI. `has_more` alone is not needed for convergence while `heads` refreshes run, so only its named trace guards it. Keeping the resync on an establishing forward page (`need` set from before the response) cannot be mutated observably: a forward page establishes only for a client without an epoch, which has no copy to page backward from.

### 7.7 What PR 17 Still Covers (deferred)

Q-B checks the protocol with the store's guarantees assumed. A `stateright` model (PR 17, deferred) would check the Rust code where Q-B abstracts: the writer thread, the store's commit and crash behaviour (a crash during an append, `CommitUnknown` recovery), the socket loop's single `merge` and frame selection, the runner's effect and completion under real time (a heads refresh against the reply grace, the deadlines), crashes at every point of a command, the runner liveness of §7.4 (PR 7, commands and receipts), and retry after a lost response against the real handlers. PR 18's fast-check reducer checks the client rules of §7.1.
