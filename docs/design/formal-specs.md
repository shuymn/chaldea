# Design: Formal Specs (Quint)

Status: Q-A (issue #13, target 2) and Q-B, the Sync Seam (target 1), implemented in `specs/`. Cited as "mvp §N" for `docs/mvp-design.md`, "http §N" for `docs/design/http-surface.md`, "store §N" for `docs/design/event-store.md`.

**Roles.** Quint checks the design-stage contract under interleavings: what the daemon may feed the reducers, in which order, and what must then hold. It does not check the Rust code; `proptest` (`crates/core/tests/input_step.rs`, `state_reduce.rs`) checks each reducer alone, and the `stateright` model of PR 17 checks the sync code. Q-A's value is the coupled model: `ThreadState`, the gate projection, several inputs across two launches, approval cards, and attention, together. Q-B's is the Sync Seam protocol before its code exists (§7). The trace bridge (§4, §7.5) ties the specs to the code: committed Quint traces replay against the real `core` functions.

| File | What |
|---|---|
| `specs/thread_input.qnt` | Q-A: the model, its invariants and witnesses |
| `specs/thread_input_traces.qnt` | Q-A's named deterministic traces (`run trace_*`) |
| `specs/sync.qnt`, `specs/sync_traces.qnt` | Q-B read side: replay, paging, epochs, notify-then-pull (§7) |
| `specs/receipts.qnt`, `specs/receipts_traces.qnt` | Q-B command side: receipts (§7) |
| `specs/traces/*.itf.json`, `traces/sync/`, `traces/receipts/` | those traces, exported as ITF and normalized |
| `specs/scripts/{quint,simulate,traces}.mjs` | the Quint runner, the seeded simulation, the trace export and freshness check, each over the list of specs |
| `specs/package.json`, `pnpm-lock.yaml`, `pnpm-workspace.yaml` | Quint, exact-pinned; dependency build scripts denied |
| `crates/core/tests/quint_traces.rs`, `quint_sync_traces.rs` | the replay tests (Q-A; Q-B) |

## 1. What Q-A Models

Two layers in one module:

- **Reducer layer**: `reduce`, `stepInput`, and `gate` mirror `crates/core/src/state.rs` `reduce`, `input.rs` `step`, and `input.rs` `gate` branch for branch, including `StatusChanged` output. The trace replay keeps them equal.
- **Supervisor layer**: the actions, whose guards are the contract of mvp §2.2, §2.3, §2.4 "Restart", §2.7, §2.9, §2.10. They are the obligations PR 9, PR 10, and PR 13–14 implement.

**Bounds.** Two launches, three inputs, two approval cards (each with its own id), three buffered hooks; task states `none`, `active`, `completed`; reject reasons `control_character` and `spawn_failed`.

**Abstractions.**

- Stream positions are one monotonic counter `seq`; a position of 0 stands for `None`.
- The PTY writer is the list of counted writes of the current launch (`WInput(id)`, `WHuman`, `WReply`); a position is an index into it. Readiness evidence stores the list length at its intake as its basis, and an input's attempt keeps the basis of the evidence the gate opened on. Every VT reply is counted (the default `ClassSet::ALL`, `docs/design/pty-runtime.md` D3).
- Prompt text is abstract: a `UserPromptSubmit` names the set of in-flight inputs of the current launch whose expected form it equals (a nondeterministic choice).
- Timers, the settle interval, write outcomes, the ack window, and the gate timeout are explicit nondeterministic actions. A settle completion names the `Stop` it was started for, identified by launch, turn, and intake order.
- A launch has a configuration: adapter (Claude, Codex with a positional prompt, Codex manual), readiness mode (`stop_settle`, `post_decision`, owner only), bracketed paste, and for Codex whether the CLI was upgraded.

**Model assumptions** (not fixed by the contract):

- Inputs are created only while the launch is `starting` or `running`; `launch_exited` has reason `stopped` exactly when a stop was accepted; the session timer also runs while `stopping`.
- Only hooks of the launch's selected session enter the model. Selecting it is PR 9's check: hooks of another session become `raw_unknown` (`foreign_session`, mvp §2.9:325).
- Hooks follow the adapter: `Interrupt` only for Codex (mvp §2.2:135, §2.7:301); a manual Codex launch fires none until a human starts the first turn through attach (`manualFirstTurn`; mvp §2.7:303).
- A post-decision signal is its own entry for the current turn and may arrive at any point after the turn started, before or after the daemon takes in the turn's `Stop`. The CLI emits it only after every `Stop` hook decided (mvp §2.3:173), so this over-approximates; PR 12 checks that such a signal exists and its order, and PR 13–14 attribute it on the adapter side if one is adopted.
- Under `none`, Claude's `SessionStart` is no evidence either (mvp §2.3:173 "only `chaldea ready`"), although mvp §2.7:303 lists it before the after-turn mode. PR 10 decides.
- A bound `UserPromptSubmit` supersedes the open cards, as the `PromptSubmitted` comment in `state.rs` says; mvp §2.10:335 lists only `Stop` and an interrupt, and PR 13–14 adds it.

**Not modelled.** The PTY writer lock and the concrete `Basis` (PR 10 replaces list positions with `last_started` per class and makes `counted` an adapter policy), timer durations, the restart sid scan and descendant kills, worktree and close races, the prompt size limit (it refuses before any input exists), and the `vt_snapshot_taken` of a gate timeout (an output with no state). Ack expiry, settle completion, and buffered promotion are already modelled abstractly (`windowExpired`, `ready(StopSettled, stop)`, `bind`); when PR 10 fixes their concrete rules or the recheck view, the model follows and the traces are re-exported.

**State** (`st`, one record, and `last`, the step's record):

| Variable | Meaning | Source |
|---|---|---|
| `st.thread` | the three axes and the attention set (`ThreadState`) | mvp §2.2 |
| `st.seq` | the thread stream's head | mvp §2.1 "Streams" |
| `st.launch`, `cfg`, `deathConfirmed`, `sessionStartSeen`, `turnNo`, `hooksLive` | current launch and its configuration; its (or the last launch's) death confirmed; its `SessionStart` arrived; turns started in it; its hooks fire | mvp §2.7, §2.9 |
| `st.evidenceCandidate`, `stopCount` | the signal that may become readiness evidence: Claude's `SessionStart`, or a `Stop` identified by launch, turn, and intake order; any later hook of the launch invalidates it | mvp §2.3:173 |
| `st.gp` | gate evidence of the current launch: bound and its seq, readiness seq, kind, basis, launch and turn, last write, last gate-closing human write, last turn start, bracketed paste needed and on | mvp §2.1:115 |
| `st.writer` | counted writes of the current launch, in writer order | docs/design/pty-runtime.md §2.3 |
| `st.buffer`, `bufferedStop` | hooks of an unbound launch with their intake facts; the buffered `Stop` a settle completion may still name | mvp §2.9:325 |
| `st.inputs` | per input: state, launch, positional, `sent_after_seq`, its attempt's seq, last write before it and admitted basis, dispatched, write position, pending ack candidate with its intake facts | mvp §2.3 |
| `st.cards` | per card: launch, turn, open / ran / superseded / expired | mvp §2.10 |
| `st.sendCount`, `writeCount`, `sendRecords`, `dispatchRecords`, `ackRecords`, `ownerDelivered`, `cardKeys`, `reservations` | history the invariants read: facts recorded where they happen | — |
| `last.act`, `last.calls`, `last.gate` | the action and its arguments, the reducer calls it made with their results, the gate decision after it | §4 |
| `last.prevThread`, `last.prevInputs` | the state before the step, for transition invariants | — |

**Actions** (`step` groups them: the simulator picks uniformly among enabled branches of `any`, so progress has twice the weight of hooks and of disturbances):

| Action | Contract | Source |
|---|---|---|
| `reserve(cfg)` | only after `exited` with death confirmed; resets the gate projection, writer, and buffer; a positional prompt is created `sending` | mvp §2.9:323, §2.3:186 |
| `reserveRefused` | death unconfirmed: raise `resume_blocked`; the reducer then refuses `Reserved` | mvp §2.9:326 |
| `confirmDeath` | `launch_death_confirmed` | mvp §2.1:84 |
| `spawn` / `spawnFail` | `launch_started` moves the positional prompt to `sent` with `sent_after_seq` = its seq; a confirmed spawn failure rejects it | mvp §2.3:186 |
| `manualFirstTurn` | a human starts a manual Codex launch's first turn through attach (an attach write); its hooks fire from then on | mvp §2.7:303 |
| `sessionStartHook`, `bind` | the launch's `SessionStart` arrives and invalidates pending candidates; binding records `session_bound` (a turn start), makes Claude's `SessionStart` the evidence candidate, and promotes the buffered hooks in intake order with their intake facts | mvp §2.1:115, §2.7, §2.9:325 |
| `bufferPrompt`, `bufferStop`, `bufferReady(ev, stop)` | hooks of an unbound live launch, buffered; a settle completion only for the buffered `Stop` it names while no later hook arrived; promotion applies it only if that `Stop` is then the current turn's candidate, and keeps an unsettled `Stop` as the candidate only if it was still live at intake | mvp §2.9:325 |
| `ready(ev, stop)` | evidence of the current bound live launch, of a kind its adapter and mode produce: `SessionStart` backed by Claude's own; a settle completion only for the `Stop` it names, of the current launch and turn, with no later hook since; a post-decision signal on its own entry once a turn started | mvp §2.3:171-173, §2.7 |
| `ownerReady` | `chaldea ready`: the bound launch only; otherwise refused, writing nothing | mvp §2.3:171 |
| `prompt(matches)` | bound `UserPromptSubmit`: turn start, supersedes cards, invalidates the evidence candidate; ack attribution from the hook's intake facts (unique match, same launch, after the attempt, dispatched, no other counted write after it); an undispatched match is a `user_message`, a dispatched one acks or is kept as the `sending` input's candidate | mvp §2.3:182; docs/design/pty-runtime.md §2.3 "Ack attribution" |
| `humanWrite(o)` | `attach` and `key` set the last write and last human write; a card key carries its launch and card and sets the last write only; one whose card is not open in the current launch is refused, writing nothing | mvp §2.1:115, §2.3:174, §2.10:336 |
| `vtReply` | a counted write with no event | docs/design/pty-runtime.md §2.5 |
| `send`, `open` | `queued`; the gate opens, `sending` advances the last write, and the attempt keeps the basis the gate opened on | mvp §2.3:163-181 |
| `dispatch` | the writer's recheck: no counted write since the admitted basis, the input's launch current, and the whole gate open on a view without the input's own attempt; else `cancelled` with zero bytes | mvp §2.3:177; docs/design/pty-runtime.md §2.3 "Basis" |
| `outcome` | `sent` (then the kept candidate acks or not from its intake facts) or `unconfirmed` | mvp §2.3:182-183 |
| `windowExpired`, `gateTimeout`, `cancelInput`, `rejectInput`, `resolveInput` | the remaining input transitions; the window of a positional prompt is suspended while a startup cause is active | mvp §2.3 |
| `stop`, `exit(reason, dc)`, `restart` | stop; exit recovers the launch's inputs and expires its cards; restart exits the open launch with death unconfirmed and recovers every input; buffered hooks of a launch that can no longer bind are dropped (`raw_unknown`) | mvp §2.3:183, §2.4:214, §2.9:325-326 |
| `stopOfTurn(t)` (`turnStop` for the current turn), `interrupt` | the current turn's `Stop` supersedes the cards and becomes the evidence candidate; a late `Stop` of the previous turn ends nothing and leaves a candidate of its own turn, which backs no evidence; Codex's interrupt supersedes the cards and invalidates the candidate | mvp §2.2:135, §2.10:335 |
| `cardOpen(c)`, `cardResolve(c)`, `lateResolve(c)` | a card of the current turn; resolving the last open card of the turn feeds `ApprovalsAllResolved`; a resolution of a closed card feeds nothing; each invalidates the evidence candidate | mvp §2.10 |
| `sessionTimer`, `stall` | `session_start_timeout` for a non-manual live launch with no `SessionStart`; `startup_stall` for an upgraded Codex launch before binding | mvp §2.2:142-143, §2.7 |
| `setTask`, `decset` | `task_set`; DECSET 2004 | mvp §2.2, §2.3 |
| `raw(i)`, `rawReadyUnbound(ev)` | reducer inputs with no supervisor guard; `stepUnguarded` adds them | §3 |
| `rawStep(id, ii)` | an input step with no guard, for deterministic replay coverage of a rejected step in a named trace | §4 |

## 2. Invariants

All are checked by `pnpm run simulate` (§5). Each reads facts recorded where they happen, so breaking the guard it protects violates it (checked by mutating the guard). One mutation needs a larger run: dropping the gate-closing human write from the recheck view violates `dispatchOnlyThroughRecheck` after about 5,000 samples per seed, so at the CI size `trace_refused_recheck_same_turn_evidence` catches it.

| Invariant | Statement | Source |
|---|---|---|
| `sendingAtMostOnce` | an input enters `sending` at most once | CL-4; `input.rs:3-4` |
| `writtenAtMostOnce` | an input's text is dispatched at most once | arch §4.3; mvp §2.3:183 |
| `sendingOnlyThroughOpenGate` | `sending` through the gate only when bound, running, idle, evidence newer than the last write and not of the same turn as a gate-closing human write, no other in-flight input of the launch, bracketed paste on where needed (the positional prompt is exempt); recorded from the state, not from the guard's view | mvp §2.3:171, :174; CL-5 |
| `dispatchOnlyThroughRecheck` | every dispatched input passed the recheck at its first byte, re-derived from the facts the dispatch recorded rather than from the guard's view: the same launch, no counted write since the admitted basis, and each gate condition with the input's own attempt not counted as a write | mvp §2.3:171-177 |
| `oneInFlightPerLaunch` | at most one in-flight input per launch | mvp §2.3:171 |
| `earlierLaunchesNeverBlock` | the gate equals the gate with `in_flight` counted over the current launch's inputs only | mvp §2.3:171; CL-6 |
| `terminalStatesImmutable` | terminal input states never change | mvp §2.3:185; `input.rs:36-46` |
| `neverResent` | nothing leads from `sending`/`sent`/`unconfirmed` back to `queued`/`sending`; `unconfirmed` leaves only to `acked` or `dismissed` | arch §4.3; mvp §2.3:183 |
| `unconfirmedAttentionIff` | `input_unconfirmed(id)` iff that input is `unconfirmed` | mvp §2.2:144 |
| `gateTimeoutOnlyWhileQueued` | `input_gate_timeout(id)` only while that input is `queued` | mvp §2.2:145 |
| `noStartupCauseAfterBindOrExit` | no startup cause once bound or exited | mvp §2.2:142-143 |
| `stallOnlyForUpgradedCodex` | `startup_stall` only for an upgraded Codex launch | mvp §2.2:143, §2.7 |
| `openCardsAwaitApproval` | while a card of the current launch is open the turn is `awaiting_approval` | mvp §2.10:333-336 |
| `lateResolutionChangesNothing` | a resolution after `Stop`, interrupt, prompt, or exit leaves `ThreadState` unchanged (it can still invalidate pending readiness) | mvp §2.10:335 |
| `restartRecovers` | a restart turns `sending`/`sent` into `unconfirmed` and `queued` into `cancelled` | mvp §2.4:214; CL-6 |
| `exitedLaunchHasNoPendingInput` | after exit no input is `queued`, `sending`, or `sent` | mvp §2.3:183, :185 |
| `ackOnlyWhenAttributable` | every `AckAttributed` fed has a record of the hook's intake facts, and every record shows all five (same launch, after the attempt, unique match, dispatched, no other counted write after it), checked directly rather than through the guard's `attributable` | mvp §2.3:182; CL-7 |
| `reservedOnlyAfterDeath` | every reservation the reducer accepted came after the previous launch's death was confirmed | mvp §2.9:323; CL-18 |
| `resumeBlockedOnlyWhileDeathUnconfirmed` | `resume_blocked` only while exited with death unconfirmed | mvp §2.2:146 |
| `exitedTurnUnknown` | an exited launch's turn is `unknown` | mvp §2.2:135 |
| `taskOnlyByTaskSet` | the task axis changes only on `task_set` | CL-2 |
| `gateEvidenceIsBound` | gate evidence is of the bound launch, after its binding | mvp §2.3:171, §2.9:325 |
| `openGateEvidenceOfCurrentTurn` | an open gate rests on evidence of the current launch and turn, of a kind the launch's adapter and mode produce per `EVIDENCE_TABLE`, written out apart from the guard's `kindAllowed` | mvp §2.3:171-173, §2.7 |
| `ackedHasCause` | an input is `acked` only through an attributable intake record or the owner resolving it as delivered | mvp §2.3:182-184 |
| `cardKeysOnlyToOpenCards` | every card key the writer accepted came from a card open in the current launch | mvp §2.10:336 |
| `interruptedOnlyForCodex` | `Interrupted` is fed only for a Codex launch | mvp §2.2:135, §2.7:301 |
| `supervisorInputsAccepted` | the supervisor feeds the reducers only inputs they accept; `ReserveRefused` ends with a refused `Reserved` | — |

**Witnesses** (`witness*` in the spec; 12) count traces reaching any `acked` input, an attributed ack through the gate, a late ack, a failed recheck, a dismissal, a second launch, an attributed positional ack, `resume_blocked`, an open gate beside an earlier launch's `unconfirmed` input, a binding that promotes buffered hooks, a late card resolution beside an open card, and two open cards. `simulate` fails when one is never reached, so a model change that cuts coverage is caught.

**Model results.** Every invariant holds over the CI simulation. With one in-flight input per launch and attribution limited to the same launch, "a prompt matching more than one input acks none" (mvp §2.3:182) is unreachable; a unit test of the matcher in PR 10 keeps it. The projection counts an input's own `sending` as the last write and as in flight, so the writer's recheck cannot be `gate()` on the projected view unchanged (`trace_send_ack` shows `WriteAfterEvidence` right after `open`).

## 3. Gaps the Traces Show

`raw` and `rawReadyUnbound` feed the reducers what the supervisor's guards forbid, and `stepUnguarded` adds them to `step`. A manual reproducer, from `specs/`: `pnpm exec quint run thread_input.qnt --step stepUnguarded --invariant <name> --seed 1 --max-samples 1000 --max-steps 40 --backend typescript` finds a, c, d, and the card finding (`reservedOnlyAfterDeath`, `noStartupCauseAfterBindOrExit` and `stallOnlyForUpgradedCodex`, `gateEvidenceIsBound`, `openCardsAwaitApproval`), and an `input_unconfirmed` raised for an input that is not `unconfirmed` (`unconfirmedAttentionIff`; a search case with no named trace). The named traces fix one example of each of a, c, d, and the card finding and end with an `expect` stating the gap; the replay shows the real reducers accept every step.

**Reducer preconditions the supervisor enforces.** The Rust reducers do not receive the facts these guards need; each lives in the supervisor layer, and the PR named in mvp §4 owns it.

| Trace | What the reducers admit | Contract | Owner |
|---|---|---|---|
| `trace_hole_a_reserve_without_death` | after a restart, `Reserved` before any `resume_blocked`: `Exited` carries no death bit and `Reserved` checks only the attention flag (`state.rs:174`, `:278-288`) | mvp §2.9:323 | PR 10 |
| `trace_hole_c_stale_startup_cause` | `AttentionRaised(StartupStall)` while `exited`, which then survives `Reserved` into the next launch (`state.rs:369-371`) | mvp §2.2:142-143 | PR 10 |
| `trace_hole_d_unbound_readiness` | `ReadinessObserved` of an unbound Codex launch (`state.rs:333-336`); fed directly instead of buffered, it survives binding as gate evidence older than the binding | mvp §2.3:171, §2.9:325 | PR 9 |
| `trace_finding_resolution_without_card_identity` | `ApprovalsAllResolved` carries no card identity: fed for card 1 after card 2 opened in the next turn, it ends the wait for card 2 (`state.rs:362-367`) | mvp §2.10:333-336 | PR 10, PR 13–14 |

**A projection consequence, not a missing guard.** `trace_hole_b_recheck_keeps_gate_closed`: a failed recheck cancels the input with zero bytes, yet mvp §2.1:115 records the attempt (`input_state_changed` to `sending`) as the last write, so the gate stays `WriteAfterEvidence` with no write since the evidence until new eligible evidence or `chaldea ready`; the gate timeout does not reopen it. No safety invariant breaks, since the gate stays closed. PR 10 chooses: accept it, or tell attempts from started writes and leave out only attempts known to have written zero bytes, with regression cases that intervening writes, stale evidence, and unknown outcomes keep the gate closed.

**A conservative consequence.** `trace_finding_positional_vt_reply`: a VT reply after `launch_started` is a counted write after the positional prompt's anchor, so under the default counted classes its ack is never attributable (mvp §2.3:182; docs/design/pty-runtime.md §2.3 "Basis"). Startup replies wait in the reply lane from the first output byte and can start before `launch_started` is recorded (docs/design/pty-runtime.md §2.2 "Start"), so PR 10 states where the anchor is taken and which writes around it count, with examples.

**Races the contract refuses** (named `trace_refused_*`):

- evidence of the same turn as a `key` write arriving after the gate opened (`EvidenceOfSameTurn` at the recheck);
- a VT reply after the admitted basis followed by newer owner evidence (the recheck keeps the basis the send relied on);
- readiness after a Codex interrupt (no `Stop` backs it);
- a settle completion after a late card resolution of the same launch (a later hook);
- a late `Stop` of the previous turn backing the current turn, or a settle completion of a `Stop` that a later one replaced;
- a buffered `Stop` followed by `SessionStart` (a later hook): neither a buffered settle completion nor one after binding counts, and no input is written (`trace_buffered_stop_after_session_start` shows a `Stop` buffered after `SessionStart` still settling);
- a key from a card that `Stop` superseded (nothing written, no event).

`trace_undispatched_match_ignored` shows a same-text hook before the dispatch leaving no candidate, `trace_cards_with_identity` a late resolution of a superseded card while the next turn's card is open, `trace_manual_first_turn` a manual Codex launch with no hook before its first turn, and `trace_post_decision_own_entry` post-decision evidence taken in before any `Stop`.

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
pnpm run simulate       # each spec x seeds 1-4 in parallel, --max-samples 1000, --max-steps 40, all invariants and witnesses
pnpm run traces:check   # spec and specs/traces/ agree
pnpm run traces:write   # regenerate specs/traces/ after a spec change
```

The `specs` job of `ci.yml` (`ubuntu-latest`, mise pinned in `mise-action`) installs Node and pnpm through mise and runs those checks. `lefthook.yml` runs typecheck and the trace check on commit and the simulation on push for changes under `specs/`, each after `pnpm install --frozen-lockfile` (about 50 ms when `node_modules` is up to date), and runs the workspace tests on push when a trace changes. `verifyDepsBeforeRun: error` in `pnpm-workspace.yaml` also makes every `pnpm run` fail while `node_modules` does not match `package.json` and the lockfile. The simulation size reaches every witness. The twelve runs (three specs, four seeds) take about 25 s in parallel on a development machine (Q-A alone about 15 s); install, typecheck, simulation, and trace check from an empty `node_modules` (pnpm store warm) about 40 s. The simulation is about 200 CPU-seconds, so CI's four vCPUs need about a minute for it (estimated, not measured on CI).

**Apalache** (`quint verify`) is not in CI and is unverified: it needs a Java runtime and downloads Apalache, and whether it accepts the spec's list folds and maps of records is untested. To try it: `pnpm exec quint verify thread_input.qnt --invariant allInvariants --max-steps 10`.

## 6. Extending

- **PR 9** implements the buffering and promotion the model's `buffer*` actions and `bind` state (hole d).
- **PR 10** makes the supervisor layer real: each guard here is a checklist item, it decides holes a–c and the recheck-attempt choice of §3, and it grows the model where §1 says. A changed guard changes the spec and its traces in the same PR.
- **PR 13–14** correlate card resolutions and card keys by `approval_id` (the card finding).
- **PR 7b, 7c, 18** implement what Q-B states; §7.4 lists the obligations it hands them.
- **PR 17**'s `stateright` model checks the Rust sync code; Q-B checks the contract it implements (§7.7).
- **Phase 2 relay**: §7.1 says where it plugs into Q-B.

## 7. Q-B: Sync Seam

Q-B checks the Sync Seam protocol of mvp §2.4 and the PR 7 design (http §2.6–§2.8, §8 D14, D15) under interleavings before PR 7b (sync reads, WebSocket) and 7c (commands, receipts) implement it. It assumes PR 6's store guarantees (store §2.2, §2.7, §2.8): atomic commits, gapless per-stream `seq`, idempotency, an epoch fixed when the database is created, and one read snapshot per call. Two modules, simulated and traced separately:

- `specs/sync.qnt`: replay, paging, snapshots, epochs, and notify-then-pull;
- `specs/receipts.qnt`: command receipts across retries, lost replies, writer contention, and restarts.

### 7.1 What Q-B Models

Each module has the two layers of Q-A: a mirror of the `core` function, `decideReplay` of `sync::decide_replay` and `checkReceipt` of `command::check_receipt`, branch for branch, whose calls each step records for the replay (§7.5); and the protocol as actions whose guards and effects are the contract.

**Actors** (`sync.qnt`). One host, authoritative: one database with its epoch and a log per stream; a new database is the next epoch with empty streams (the old epochs' logs stay as ghosts, since a client may still hold their events). Two clients, PWA-like: client 1 follows both streams, client 2 a nondeterministic set; each holds one epoch, a local copy per stream (events ending at its cursor, the cursor, the rendered state), a fetch due flag per stream, and its current requests. The network: requests and responses are sets, so any can be lost, delayed, or overtaken (one request per connection orders none of them, mvp §2.5); each client's socket is a FIFO of frames, any of which can be lost (a lagging receiver or a coalesced frame). A daemon restart drops every socket and every request in flight; responses already sent may still arrive.

**Bounds.** Two streams (host, thread), two epochs, five events per stream and epoch, four requests plus responses in flight, event sizes 1 or 3 units. `decide_replay`'s bounds are scaled: `REPLAY_MAX_EVENTS` 2 (one model seq stands for 5 000), `REPLAY_MAX_BYTES` 4 units (one unit for 2 MiB); each comparison in `decide_replay` is linear, so the scaled call takes the same branch, and the replay checks it at the real bounds. A snapshot carries one recent event.

**What a client renders** is a stream's projection, `Proj`: the latest `surface_update` and a counter of the thread's status changes. A snapshot carries it as of `as_of_seq`; events after it update it. A host snapshot also carries the thread's status read in the same snapshot (its `ThreadInfo`, a field of another stream; §7.4 F1).

**Client rules** (the model's reading of mvp §2.4; PR 18 implements them, §7.4):

| Rule | Source |
|---|---|
| A forward fetch sends the cursor and the epoch held (none before the first response) | mvp §2.4 "Replay" |
| The current request per stream and direction is the latest issued; it stops being current when answered or timed out, when a snapshot replaces that stream's copy, and for every stream on an epoch switch or reconnect. A response to any other request is of an older generation and changes nothing | mvp §2.4 "a response from an older request generation is discarded"; the scope is the model's (§7.4 F2) |
| A forward response with another epoch switches epoch and every local copy together: all copies dropped, every followed stream due; a forward page becomes the new copy only if it starts at the stream's beginning | mvp §2.4 "only a forward response … may establish a new epoch" |
| A backward page with another epoch switches nothing and makes a forward resync due | mvp §2.4 "Backward history" |
| A same-epoch snapshot (a bound or `CursorAhead`) replaces the stream's copy | mvp §2.4 "Snapshot fallback" |
| `has_more` makes the next forward page due | — (PR 18 obligation; convergence also holds through `heads` without it, more slowly) |
| `stream_advanced` or a `heads` entry above the cursor of a followed stream makes a fetch due; `heads` is a snapshot of the listed streams | mvp §2.4 "Live notifications"; http §8 D15 |
| Reconnect: a new socket, a `subscribe` of the followed set, and a refetch of every followed stream | mvp §2.4 "the client refetches on every reconnect" |
| `subscribe` replaces the set; a later one replaces an unanswered one, answered by one `heads` for the latest set | http §2.6, §8 D15 |

**Convergence is a bounded-liveness check.** The `quiesce` action restores connectivity (every client without a socket reconnects), stops writes and losses, and runs the protocol on one fixed fair schedule (responses, requests, subscribes, frames, due fetches, timeouts of lost requests, then up to two heads refreshes) for at most 60 steps; `convergedAfterQuiesce` then requires every client to hold the host's epoch, cursor, and rendered state for each stream it follows. A bound rather than an "eventually": Quint's simulator checks state predicates, and an unbounded liveness property cannot fail in a finite run, while a step bound fails as soon as the protocol stalls (dropping the periodic `heads` does, §7.6). The schedule is one fair schedule, not all of them; the simulator samples the same transitions in every order as ordinary steps, where the safety invariants are checked, but the intermediate states inside one `quiesce` are not. Fewer than 40 steps were needed over 1 000 samples.

**Actors** (`receipts.qnt`). Clients sending a command with a `command_id`, a token, and a request identity (two of each; identity 1 is a command completed in its intent job, `send`; identity 2 one with an effect, `stop`), and retrying with the same three at any time (a lost reply is a retry). One writer runs jobs one at a time in order (store §2.2): an intent job whose deadline passed is `Expired` without running; otherwise it looks the receipt up and, for `New`, inserts it `in_progress` with the intent in one transaction (completed in the same job for `send`); its outcome is `Ok` or `CommitUnknown` that did or did not commit, the latter putting the writer in its failed state, where every queued job is `Failed`. The runner starts the effect once after an `Ok` intent and submits the completion when the effect reports. The request replies from the progress it sees once its runner is done or its deadline plus the grace passed (http §2.8 table). A restart decides the running job, drops every connection, queued job, and effect, and turns every `in_progress` receipt into `unknown_after_crash`.

**Not modelled.** Authorization and 404s, blobs, tickets, token revocation, the socket's read and send deadlines (a socket that misses them is a disconnect), `hello`, `busy` and queue-full replies, the completion budget's retries (a completion that cannot commit is `CompletionFailed`), the shutdown drain, and the store's internals.

**Phase 2 relay.** The host stays the only authority, so a relay is one more actor between the clients and the host. In `sync.qnt` it would hold a per-stream cache keyed by `stream_id` and `epoch`, answer `serve` from it (a head at most the host's, of the host's epoch or an older one) or forward the request, and send its own notifications; the client rules and every invariant stay as they are, and convergence is checked after the relay catches up. In `receipts.qnt` it forwards `send` attempts unchanged by `command_id`, possibly losing or repeating them, which the clients' retries already cover.

### 7.2 State and Actions

`sync.qnt` (`st`, one record; `last`: the action, the `decide_replay` calls, the clients before the step):

| Variable | Meaning |
|---|---|
| `st.host` | the current epoch and every epoch's log per stream |
| `st.clients` | per client: epoch held, copy per stream, current forward and backward request per stream, fetch due, followed set; ghosts `lastFwd`, `lastBwd`, `resetAt` for the generation invariant |
| `st.sockets` | per client's socket: open, subscription set, unanswered subscribe, frames in order |
| `st.reqs`, `st.resps`, `st.nextId` | requests not yet served and responses in transit, by request id |

| Action | What |
|---|---|
| `appendA(x, kind, size)` | the host commits one event; every open socket subscribed to its stream gets `stream_advanced` |
| `restart`, `reset` | a daemon restart; a new database (the next epoch) with a restart |
| `connectA(c)`, `disconnectA(c)`, `resubscribeA(c, w)` | reconnect (subscribe and refetch), a dropped socket, a new followed set |
| `fetchA(c, x, limit)`, `fetchBackA(c, x, limit)`, `timeoutA(c, x, dir)` | a forward fetch from the cursor, a backward page before the oldest event held, a request given up |
| `serveA(id)`, `loseReq(id)`, `loseResp(id)`, `deliverA(id)` | the host answers from one read snapshot; the network loses a request or response; the client applies a response |
| `serverSubscribeA(c)`, `headsRefreshA(c)`, `recvNoteA(c)`, `dropNote(c, i)` | `heads` for the latest subscribe; the periodic `heads`; the client takes the next frame; a frame is lost |
| `quiesce` | connectivity returns, writes stop, and the protocol runs to its fixpoint (§7.1) |
| `rawFetch(c, x, q)` | a forward request with any query (replay coverage of refusals and `CursorAhead`) |

`receipts.qnt` (`st`; `last`: the action and the `check_receipt` calls):

| Variable | Meaning |
|---|---|
| `st.receipts` | per `command_id`: token, request identity, status |
| `st.queue`, `st.running`, `st.failed` | the writer's jobs, the job it runs, its failed state |
| `st.effects`, `st.attempts` | effects running; per request: identity, progress, deadline passed, connection alive, replied |
| `st.intents`, `effectStarts`, `creator`, `stored`, `markedUnknown`, `afterUnknown`, `committed`, `replies` | history the invariants read |

| Action | What |
|---|---|
| `send(c, t, h)` | a request or a retry: its runner registered and its intent job queued |
| `tick(a)` | the request's deadline passes |
| `writerStart`, `writerFinish(w)`, `writerFail` | the writer takes the next job (an expired intent job ends `Expired`), ends it `Ok` or `CommitUnknown`, or fails a queued job |
| `effectDone(c)` | the effect reports; the runner queues the completion |
| `replyTo(a)` | the request replies from its progress |
| `restart(committed)` | a crash and restart; the running job committed or not |

### 7.3 Invariants

All are checked by `pnpm run simulate`. Each reads facts recorded where they happen; the last column is a spec mutation that violates it, judged by the exit status of a run at the CI size with only that invariant (§7.6).

| Invariant | Statement | Source | Violated by |
|---|---|---|---|
| `noGapNoDup` | a copy holds consecutive events ending at its cursor, none twice, none missing after a snapshot's recent events | mvp §2.4; CL-13 "without duplicates"; CL-9 | applying responses of any generation |
| `heldEventsOnHost` | every event a client holds is the host's event at that position of the epoch the client holds, and the cursor is within that epoch's head | mvp §2.4; CL-13 | prepending a backward page of another epoch |
| `noMixedEpochs` | every copy and every event a client holds is of the epoch it holds | mvp §2.4 "switching epoch and local state together" | an epoch switch that keeps the other streams' copies |
| `renderedStateAtCursor` | what a copy renders is the host's projection at the copy's cursor | mvp §2.4 "Everything a client renders is in a snapshot or in events after `as_of_seq`" | forward events not applied to the rendered state |
| `olderResponsesChangeNothing` | a response of an older generation (by the ghosts, not by the guard) leaves the client unchanged | mvp §2.4 "a response from an older request generation is discarded" | applying responses of any generation |
| `epochNeverRegresses` | a client never goes back to an epoch the host left (the model numbers epochs in creation order to state it) | mvp §2.4 "only a forward response … may establish a new epoch" | an epoch switch that keeps the other streams' requests current: caught only by `trace_refused_stale_epoch_after_switch` |
| `convergedAfterQuiesce` | after connectivity returns and writes stop, every client converges within 60 fair steps (§7.1) | mvp §2.4 "Live notifications"; CL-13; http §8 D15 | no periodic `heads` |
| `oneIntentPerCommand` | one `command_id` causes at most one intent | mvp §2.4 "Insert"; CL-8; PR7-CL-18 | `check_receipt` answering `New` for `in_progress` |
| `oneEffectPerCommand` | one `command_id` starts at most one effect, across retries and restarts | CL-8; PR7-CL-19 | `check_receipt` answering `New` for `unknown_after_crash` |
| `receiptIffIntent` | a receipt exists exactly when its intent committed | mvp §2.4 "Insert" | a restart deleting `in_progress` receipts |
| `replayOnlyToCreator` | a stored result goes only to the token and request that created the receipt, and is the stored one | mvp §2.4 `ReceiptLookup`; CL-8 | `check_receipt` ignoring the request identity |
| `othersLearnNothing` | another token or request never gets `in_progress` or `unknown_after_crash` | mvp §2.4 `Conflict`; http §2.8 "Receipt visibility" | reading the status before ownership |
| `unknownNeverReexecuted` | no intent or effect after `unknown_after_crash`, and the receipt stays so | mvp §2.4 "Restart" | `check_receipt` answering `New` for `unknown_after_crash` |
| `noInProgressAfterRestart` | no receipt is `in_progress` right after a restart | mvp §2.4 "Restart" | a restart that marks nothing |
| `timeoutOnlyWithoutIntent` | a request answered 408 never commits its intent, now or later | mvp §2.4 "A 408 describes the current attempt only"; http §2.8 "Why 408 is exact" | 408 while the writer runs the job |
| `inProgressOnlyWithReceipt` | 409 `in_progress` only when the receipt exists at reply time (it may have completed since the job read it; the retry replays) | http §8 D14 | 409 `in_progress` for a job still queued |

**Witnesses.** `sync.qnt` (11): an epoch switch, convergence across epochs, an older response discarded, a backward page applied, a backward page of another epoch, each snapshot reason the protocol takes (`TooManyEvents`, `TooManyBytes`, `EpochChanged`), `has_more`, a stream only a heads refresh recovers, a replaced subscribe. `receipts.qnt` (8): a replay, a replay after 503 `outcome_unknown`, 409 `in_progress`, a duplicate while `in_progress`, 409 `unknown_after_crash`, 409 `conflict`, a 408 followed by the command running, a `CommitUnknown` that committed.

### 7.4 Findings

**F1. Snapshot fields of another stream.** A host snapshot carries each thread's `ThreadInfo` (axes and attention) and a thread snapshot the current launch's worktree, read in the same snapshot (store §2.7; http §2.7 "which can change without that stream advancing"). They change through thread-stream events and `worktree_changed` on the host stream, so the snapshot's stream does not advance and nothing tells a client following only that stream. `trace_finding_cross_stream_snapshot_field`: client 2 follows the host stream, gets a host snapshot, the thread's status changes, and after `quiesce` the client has converged on the host stream's events and surfaces yet shows the old status. mvp §2.4 "Everything a client renders is in a snapshot or in events after `as_of_seq`" holds only for fields of the snapshot's own stream. Options: (a) PR 18 renders thread axes and attention only from the thread streams it follows and the worktree from the host stream (or `/api/worktrees`), treating these snapshot fields as initial values; a thread list then follows every listed thread stream, within the 1024-stream cap; (b) the daemon appends a host-stream event (a thread status summary) on every axis or attention change, so the host stream carries what the thread list renders (a change to mvp §2.1 "Streams"); (c) state in mvp §2.4 that these fields are current only at `as_of_seq`, and leave the choice to PR 18. The model's convergence covers the snapshot's own stream only.

**F2. The scope of a request generation.** mvp §2.4 discards "a response from an older request generation" without saying what a generation is. Q-B needs all of: (1) only the latest request per stream and direction is current (`trace_refused_older_generation`: an abandoned forward page arriving after the newer one would duplicate events); (2) an epoch switch or reconnect makes every earlier request of every stream older (`trace_refused_stale_epoch_after_switch`: with per-stream generations, a thread-stream page of the old epoch, requested before the switch on the host stream, is a forward response with another epoch and switches the client back to the epoch the host left); (3) a snapshot that replaces a stream's copy makes that stream's earlier backward request older (`trace_refused_backward_after_snapshot`: its page would be prepended to the new copy with a gap). Options: state these three in mvp §2.4 as the generation's definition, or state the observable rule (apply a forward page only at the cursor it was requested from, a backward page only before the oldest event it was requested for, and adopt a new epoch only from a response requested after the last switch) and leave the bookkeeping to PR 18.

**Model results.** Every invariant holds over the CI simulation. Within one epoch a client that sends its epoch never sees `CursorAhead`, since its cursor never passes the head; only a client that lost its epoch but kept a cursor can (`trace_coverage_queries` reaches it with a raw query). A forward replay page of another epoch reaches a client only when its query omitted the epoch. An intent job of a command with an effect that ends `CommitUnknown` after committing leaves the receipt `in_progress` with no effect started; PR 8 stops the daemon on the failed writer, and the restart turns it into `unknown_after_crash`, so that command never runs and its retry learns only that the outcome is unknown (`trace_commit_unknown_then_restart`), as store §2.2 states.

**Obligations handed on.**

- **PR 7b**: one read snapshot for head, decision, and page or snapshot (`serve`); `heads` on every subscribe for the latest set and on every refresh for the whole set; a commit notifies every open socket subscribed to its stream; restart drops sockets.
- **PR 7c**: 503 `outcome_unknown` while the writer has not replied (never 408, never 409); 408 only after `Expired`; `check_receipt` before any write, the receipt and the intent in one job; `receipts_mark_unknown_after_crash` before commands are accepted.
- **PR 18**: the client rules of §7.1, including the generation of F2 and the choice of F1; continue while `has_more`; refetch every followed stream on reconnect; treat every `heads` frame as a snapshot of the listed streams.

### 7.5 Trace Bridge

`sync_traces.qnt` (13 traces) and `receipts_traces.qnt` (8) follow Q-A's naming: `trace_refused_*` end where the client discards a response or does not switch, `trace_coverage_*` reach `decide_replay` branches the protocol never takes (`limit` out of range checked first, `after_seq` with `before_seq`, the default limit, `CursorAhead`, `after_seq` at the head with the largest limit), `trace_finding_*` end with an `expect` stating the finding. `trace_byte_bound` puts a forward replay exactly at both bounds.

`crates/core/tests/quint_sync_traces.rs` replays every recorded call: `decide_replay` with each query and head scaled back by the factors the trace's `Init` names (asserting they divide the real bounds), the epochs as `epoch-<n>`, and the plan compared exactly; `check_receipt` with receipts, tokens, and request hashes built from the model's numbers and the lookup compared exactly. Reverting any branch of either function (the epoch check, `CursorAhead`, the event bound's `>`, the byte bound, `after_seq` with `before_seq`; the token check, the request-hash check, `in_progress`, `unknown_after_crash`) fails it. The store, the HTTP and WebSocket layers, and the client are not replayed: no `core` function implements them.

### 7.6 Results

The CI size (seeds 1–4, 1 000 samples, 40 steps per spec) reaches every witness of both modules. On a development machine `sync.qnt` takes about 13 s per seed and `receipts.qnt` about 12 s; with Q-A, the twelve runs take about 25 s in parallel. Each mutation of §7.3 fails at that size on all four seeds, except the one caught only by its named trace.

### 7.7 What PR 17 Still Covers

Q-B checks the protocol with the store's guarantees assumed. PR 17's `stateright` model checks the Rust code where Q-B abstracts: the writer thread, the store's commit and crash behaviour (a crash during an append, `CommitUnknown` recovery), the socket loop's single `merge` and frame selection (here a FIFO with losses), the runner's effect and completion under the real deadlines and grace, offline buffering in the client, and retry after a lost response against the real handlers. PR 18's fast-check reducer checks the client rules of §7.1.
