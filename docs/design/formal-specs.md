# Design: Formal Specs (Quint)

Status: Q-A implemented in `specs/` (issue #13, target 2). Q-B, the Sync Seam spec, follows on the same tooling. Cited as "mvp §N" for `docs/mvp-design.md`.

**Roles.** Quint checks the design-stage contract under interleavings: what the daemon may feed the reducers, in which order, and what must then hold. It does not check the Rust code; `proptest` (`crates/core/tests/input_step.rs`, `state_reduce.rs`) checks each reducer alone, and the `stateright` model of PR 17 checks the sync code. Q-A's value is the coupled model: `ThreadState`, the gate projection, several inputs across two launches, approval cards, and attention, together. The trace bridge (§4) ties the two: committed Quint traces replay against the real reducers.

| File | What |
|---|---|
| `specs/thread_input.qnt` | Q-A: the model, its invariants and witnesses |
| `specs/thread_input_traces.qnt` | the named deterministic traces (`run trace_*`) |
| `specs/traces/*.itf.json` | those traces, exported as ITF and normalized |
| `specs/scripts/{quint,simulate,traces}.mjs` | the Quint runner, the seeded simulation, the trace export and freshness check |
| `specs/package.json`, `pnpm-lock.yaml`, `pnpm-workspace.yaml` | Quint, exact-pinned; dependency build scripts denied |
| `crates/core/tests/quint_traces.rs` | the replay test |

## 1. What Q-A Models

Two layers in one module:

- **Reducer layer**: `reduce`, `stepInput`, and `gate` mirror `crates/core/src/state.rs` `reduce`, `input.rs` `step`, and `input.rs` `gate` branch for branch, including `StatusChanged` output. The trace replay keeps them equal.
- **Supervisor layer**: the actions, whose guards are the contract of mvp §2.2, §2.3, §2.4 "Restart", §2.7, §2.9, §2.10. They are the obligations PR 9, PR 10, and PR 13–14 implement.

**Bounds.** Two launches, three inputs, two approval cards (each with its own id), three buffered hooks; task states `none`, `active`, `completed`; reject reasons `control_character` and `spawn_failed`.

**Abstractions.**

- Stream positions are one monotonic counter `seq`; a position of 0 stands for `None`.
- The PTY writer is the list of counted writes of the current launch (`WInput(id)`, `WHuman`, `WReply`); a position is an index into it. Readiness evidence stores the list length at its intake as its basis, and an input's attempt keeps the basis of the evidence the gate opened on. Every VT reply is counted (the default `ClassSet::ALL`, `docs/design/pty-runtime.md` D3).
- Prompt text is abstract: a `UserPromptSubmit` names the set of in-flight inputs of the current launch whose expected form it equals (a nondeterministic choice).
- Timers, the settle interval, write outcomes, the ack window, and the gate timeout are explicit nondeterministic actions.
- A launch has a configuration: adapter (Claude, Codex with a positional prompt, Codex manual), readiness mode (`stop_settle`, `post_decision`, owner only), bracketed paste, and for Codex whether the CLI was upgraded.

**Model assumptions** (not fixed by the contract): inputs are created only while the launch is `starting` or `running`; `launch_exited` has reason `stopped` exactly when a stop was accepted; the session timer also runs while `stopping`.

**Not modelled.** The PTY writer lock and the concrete `Basis` (PR 10 replaces list positions with `last_started` per class and makes `counted` an adapter policy), timer durations, the restart sid scan and descendant kills, worktree and close races, the prompt size limit (it refuses before any input exists), and the `vt_snapshot_taken` of a gate timeout (an output with no state). When PR 10 fixes the recheck view, the ack window and settle timers, or the promotion order of buffered hooks, the model grows those actions and the traces are re-exported.

**State** (`st`, one record, and `last`, the step's record):

| Variable | Meaning | Source |
|---|---|---|
| `st.thread` | the three axes and the attention set (`ThreadState`) | mvp §2.2 |
| `st.seq` | the thread stream's head | mvp §2.1 "Streams" |
| `st.launch`, `cfg`, `deathConfirmed`, `sessionStartSeen`, `turnNo` | current launch and its configuration; its (or the last launch's) death confirmed; its `SessionStart` arrived; turns started in it | mvp §2.7, §2.9 |
| `st.evidenceCandidate` | the signal that may become readiness evidence: Claude's `SessionStart`, or the `Stop` of a turn; a later hook event of the launch invalidates it | mvp §2.3:173 |
| `st.gp` | gate evidence of the current launch: bound and its seq, readiness seq, kind, basis, launch and turn, last write, last gate-closing human write, last turn start, bracketed paste needed and on | mvp §2.1:115 |
| `st.writer` | counted writes of the current launch, in writer order | docs/design/pty-runtime.md §2.3 |
| `st.buffer` | hooks of an unbound launch with their intake facts | mvp §2.9:325 |
| `st.inputs` | per input: state, launch, positional, `sent_after_seq`, its attempt's seq, last write before it and admitted basis, dispatched, write position, pending ack candidate with its intake facts | mvp §2.3 |
| `st.cards` | per card: launch, turn, open / ran / superseded / expired | mvp §2.10 |
| `st.sendCount`, `writeCount`, `sendRecords`, `dispatchRecords`, `ackRecords`, `reservations` | history the invariants read: facts recorded where they happen | — |
| `last.act`, `last.calls`, `last.gate` | the action and its arguments, the reducer calls it made with their results, the gate decision after it | §4 |
| `last.prevThread`, `last.prevInputs` | the state before the step, for transition invariants | — |

**Actions** (`step` groups them: the simulator picks uniformly among enabled branches of `any`, so progress has twice the weight of hooks and of disturbances):

| Action | Contract | Source |
|---|---|---|
| `reserve(cfg)` | only after `exited` with death confirmed; resets the gate projection, writer, and buffer; a positional prompt is created `sending` | mvp §2.9:323, §2.3:186 |
| `reserveRefused` | death unconfirmed: raise `resume_blocked`; the reducer then refuses `Reserved` | mvp §2.9:326 |
| `confirmDeath` | `launch_death_confirmed` | mvp §2.1:84 |
| `spawn` / `spawnFail` | `launch_started` moves the positional prompt to `sent` with `sent_after_seq` = its seq; a confirmed spawn failure rejects it | mvp §2.3:186 |
| `sessionStartHook`, `bind` | the launch's `SessionStart` arrives; binding records `session_bound` (a turn start), makes Claude's `SessionStart` the evidence candidate, and promotes the buffered hooks in intake order with their intake facts | mvp §2.1:115, §2.7, §2.9:325 |
| `bufferPrompt`, `bufferStop`, `bufferReady` | hooks of an unbound live launch, buffered | mvp §2.9:325 |
| `ready(ev)` | evidence of the current bound live launch, of a kind its adapter and mode produce, backed by a candidate of the current turn | mvp §2.3:171-173, §2.7 |
| `ownerReady` | `chaldea ready`: the bound launch only; otherwise refused, writing nothing | mvp §2.3:171 |
| `prompt(matches)` | bound `UserPromptSubmit`: turn start, supersedes cards, invalidates the evidence candidate; ack attribution from the hook's intake facts (unique match, same launch, after the attempt, dispatched, no other counted write after it); an undispatched match is a `user_message`, a dispatched one acks or is kept as the `sending` input's candidate | mvp §2.3:182; docs/design/pty-runtime.md §2.3 "Ack attribution" |
| `humanWrite(o)` | `attach` and `key` set the last write and last human write; a card key, only while its card is open, the last write only | mvp §2.1:115, §2.3:174, §2.10:336 |
| `vtReply` | a counted write with no event | docs/design/pty-runtime.md §2.5 |
| `send`, `open` | `queued`; the gate opens, `sending` advances the last write, and the attempt keeps the basis the gate opened on | mvp §2.3:163-181 |
| `dispatch` | the writer's recheck: no counted write since the admitted basis, the input's launch current, and the whole gate open on a view without the input's own attempt; else `cancelled` with zero bytes | mvp §2.3:177; docs/design/pty-runtime.md §2.3 "Basis" |
| `outcome` | `sent` (then the kept candidate acks or not from its intake facts) or `unconfirmed` | mvp §2.3:182-183 |
| `windowExpired`, `gateTimeout`, `cancelInput`, `rejectInput`, `resolveInput` | the remaining input transitions; the window of a positional prompt is suspended while a startup cause is active | mvp §2.3 |
| `stop`, `exit(reason, dc)`, `restart` | stop; exit recovers the launch's inputs and expires its cards; restart exits the open launch with death unconfirmed and recovers every input; buffered hooks of a launch that can no longer bind are dropped (`raw_unknown`) | mvp §2.3:183, §2.4:214, §2.9:325-326 |
| `turnStop`, `interrupt` | `Stop` supersedes the cards and becomes the evidence candidate; an interrupt supersedes them and invalidates the candidate | mvp §2.2:135, §2.10:335 |
| `cardOpen(c)`, `cardResolve(c)`, `lateResolve(c)` | a card of the current turn; resolving the last open card of the turn feeds `ApprovalsAllResolved`; a resolution of a closed card feeds nothing | mvp §2.10 |
| `sessionTimer`, `stall` | `session_start_timeout` for a non-manual live launch with no `SessionStart`; `startup_stall` for an upgraded Codex launch before binding | mvp §2.2:142-143, §2.7 |
| `setTask`, `decset` | `task_set`; DECSET 2004 | mvp §2.2, §2.3 |
| `raw(i)`, `rawStep(id, ii)`, `rawReadyUnbound(ev)` | reducer inputs with no supervisor guard; `stepUnguarded` adds them | §3 |

## 2. Invariants

All are checked by `pnpm run simulate` (§5). Each reads facts recorded where they happen, so breaking the guard it protects violates it (checked by mutating the guard).

| Invariant | Statement | Source |
|---|---|---|
| `sendingAtMostOnce` | an input enters `sending` at most once | CL-4; `input.rs:3-4` |
| `writtenAtMostOnce` | an input's text is dispatched at most once | arch §4.3; mvp §2.3:183 |
| `sendingOnlyThroughOpenGate` | `sending` through the gate only when bound, running, idle, evidence newer than the last write and not of the same turn as a gate-closing human write, no other in-flight input of the launch, bracketed paste on where needed (the positional prompt is exempt) | mvp §2.3:171, :174; CL-5 |
| `dispatchOnlyThroughRecheck` | every dispatched input passed the recheck at its first byte: no counted write since the admitted basis, the whole gate open without its own attempt | mvp §2.3:177 |
| `oneInFlightPerLaunch` | at most one in-flight input per launch | mvp §2.3:171 |
| `earlierLaunchesNeverBlock` | the gate equals the gate with `in_flight` counted over the current launch's inputs only | mvp §2.3:171; CL-6 |
| `terminalStatesImmutable` | terminal input states never change | mvp §2.3:185; `input.rs:36-46` |
| `neverResent` | nothing leads from `sending`/`sent`/`unconfirmed` back to `queued`/`sending`; `unconfirmed` leaves only to `acked` or `dismissed` | arch §4.3; mvp §2.3:183 |
| `unconfirmedAttentionIff` | `input_unconfirmed(id)` iff that input is `unconfirmed` | mvp §2.2:144 |
| `gateTimeoutOnlyWhileQueued` | `input_gate_timeout(id)` only while that input is `queued` | mvp §2.2:145 |
| `noStartupCauseAfterBindOrExit` | no startup cause once bound or exited | mvp §2.2:142-143 |
| `stallOnlyForUpgradedCodex` | `startup_stall` only for an upgraded Codex launch | mvp §2.2:143, §2.7 |
| `openCardsAwaitApproval` | while a card of the current launch is open the turn is `awaiting_approval` | mvp §2.10:333-336 |
| `lateResolutionChangesNothing` | a resolution after `Stop`, interrupt, prompt, or exit changes nothing | mvp §2.10:335 |
| `restartRecovers` | a restart turns `sending`/`sent` into `unconfirmed` and `queued` into `cancelled` | mvp §2.4:214; CL-6 |
| `exitedLaunchHasNoPendingInput` | after exit no input is `queued`, `sending`, or `sent` | mvp §2.3:183, :185 |
| `ackOnlyWhenAttributable` | every `AckAttributed` fed has a record of the hook's intake facts, and every record is attributable | mvp §2.3:182; CL-7 |
| `reservedOnlyAfterDeath` | every reservation the reducer accepted came after the previous launch's death was confirmed | mvp §2.9:323; CL-18 |
| `resumeBlockedOnlyWhileDeathUnconfirmed` | `resume_blocked` only while exited with death unconfirmed | mvp §2.2:146 |
| `exitedTurnUnknown` | an exited launch's turn is `unknown` | mvp §2.2:135 |
| `taskOnlyByTaskSet` | the task axis changes only on `task_set` | CL-2 |
| `gateEvidenceIsBound` | gate evidence is of the bound launch, after its binding | mvp §2.3:171, §2.9:325 |
| `openGateEvidenceOfCurrentTurn` | an open gate rests on evidence of the current launch and turn, of a kind the launch's adapter and mode produce | mvp §2.3:171-173, §2.7 |
| `supervisorInputsAccepted` | the supervisor feeds the reducers only inputs they accept; `ReserveRefused` ends with a refused `Reserved` | — |

**Witnesses** (`witness*` in the spec; 11) count traces reaching any `acked` input, an attributed ack through the gate, a late ack, a failed recheck, a dismissal, a second launch, an attributed positional ack, `resume_blocked`, an open gate beside an earlier launch's `unconfirmed` input, a binding that promotes buffered hooks, a late card resolution beside an open card, and two open cards. `simulate` fails when one is never reached, so a model change that cuts coverage is caught.

**Model results.** Every invariant holds over the CI simulation. With one in-flight input per launch and attribution limited to the same launch, "a prompt matching more than one input acks none" (mvp §2.3:182) is unreachable; a unit test of the matcher in PR 10 keeps it. The projection counts an input's own `sending` as the last write and as in flight, so the writer's recheck cannot be `gate()` on the projected view unchanged (`trace_send_ack` shows `WriteAfterEvidence` right after `open`).

## 3. Gaps the Traces Show

`raw`, `rawStep`, and `rawReadyUnbound` feed the reducers what the supervisor's guards forbid. `quint run thread_input.qnt --step stepUnguarded --invariant <name> --seed 1 --max-samples 1000 --max-steps 40 --backend typescript` finds a, c, d, and the card finding (`reservedOnlyAfterDeath`, `noStartupCauseAfterBindOrExit` and `stallOnlyForUpgradedCodex`, `gateEvidenceIsBound`, `openCardsAwaitApproval`), and an `input_unconfirmed` raised for an input that is not `unconfirmed` (`unconfirmedAttentionIff`). The named traces fix one example each and end with an `expect` stating the gap; the replay shows the real reducers accept every step.

**Reducer preconditions the supervisor enforces.** The Rust reducers do not receive the facts these guards need; each lives in the supervisor layer, and the PR named in mvp §4 owns it.

| Trace | What the reducers admit | Contract | Owner |
|---|---|---|---|
| `trace_hole_a_reserve_without_death` | after a restart, `Reserved` before any `resume_blocked`: `Exited` carries no death bit and `Reserved` checks only the attention flag (`state.rs:174`, `:278-288`) | mvp §2.9:323 | PR 10 |
| `trace_hole_c_stale_startup_cause` | `AttentionRaised(StartupStall)` while `exited`, which then survives `Reserved` into the next launch (`state.rs:369-371`) | mvp §2.2:142-143 | PR 10 |
| `trace_hole_d_unbound_readiness` | `ReadinessObserved` of an unbound launch (`state.rs:333-336`); fed directly instead of buffered, the gate opens after binding on evidence older than the binding | mvp §2.3:171, §2.9:325 | PR 9 |
| `trace_finding_resolution_without_card_identity` | `ApprovalsAllResolved` carries no card identity: fed for card 1 after card 2 opened in the next turn, it ends the wait for card 2 (`state.rs:362-367`) | mvp §2.10:333-336 | PR 10, PR 13–14 |

**A projection consequence, not a missing guard.** `trace_hole_b_recheck_keeps_gate_closed`: a failed recheck cancels the input with zero bytes, yet mvp §2.1:115 records the attempt (`input_state_changed` to `sending`) as the last write, so the gate stays `WriteAfterEvidence` with no write since the evidence until new eligible evidence or `chaldea ready`; the gate timeout does not reopen it. No safety invariant breaks, since the gate stays closed. PR 10 chooses: accept it, or tell attempts from started writes and leave out only attempts known to have written zero bytes, with regression cases that intervening writes, stale evidence, and unknown outcomes keep the gate closed.

**A conservative consequence.** `trace_finding_positional_vt_reply`: a VT reply after `launch_started` is a counted write after the positional prompt's anchor, so under the default counted classes its ack is never attributable (mvp §2.3:182; docs/design/pty-runtime.md §2.3 "Basis"). Startup replies wait in the reply lane from the first output byte and can start before `launch_started` is recorded (docs/design/pty-runtime.md §2.2 "Start"), so PR 10 states where the anchor is taken and which writes around it count, with examples.

**Races the contract refuses** (named `trace_refused_*`): evidence of the same turn as a `key` write arriving after the gate opened (`EvidenceOfSameTurn` at the recheck); a VT reply after the admitted basis followed by newer owner evidence (the recheck keeps the basis the send relied on); readiness after an interrupt (no `Stop` backs it). `trace_undispatched_match_ignored` shows a same-text hook before the dispatch leaving no candidate, and `trace_cards_with_identity` a late resolution and card key of a superseded card refused while the next turn's card is open.

## 4. Trace Bridge

`pnpm run traces:write` runs `quint test thread_input_traces.qnt --match ^trace_ --out-itf` and writes `specs/traces/<test>.itf.json`, dropping the top-level `#meta` (timestamp and path) and putting one state per line. `pnpm run traces:check` regenerates them in the OS temp directory and fails on a missing, stale, or different file; it ignores CR before LF, and `.gitattributes` already checks `*.json` out with LF.

`crates/core/tests/quint_traces.rs` decodes the ITF with `serde_json` (already a `core` dependency; no `itf` crate) and, from `ThreadState::registered(task)`, applies every `last.calls` entry to the real `reduce` or `step` (or creates the input), asserting acceptance or rejection as recorded and, for `reduce`, the exact `StatusChanged` list. After each step it compares the thread state and every input state with the model's and evaluates `gate` on the step's projection, with `in_flight` from `blocks_gate` over the real input states, against `last.gate`.

The replay guards only the branches the traces record. `trace_coverage_*` reach the ones the others shadow (`BracketedPasteOff`, `InputInFlight`, `Interrupted`, `LaunchExited` from `queued` and `sent`, `Rejected`, `resolve --delivered`, a refused step); reverting any of those, `blocks_gate`, `Exited` resetting the turn, `Sending + RecheckFailed`, or the `WriteAfterEvidence` check fails the replay. Intermediate states inside one model step, `InvalidTransition.rule`, and branches no trace reaches are the `proptest` suites' job.

The replay runs in the `workspace` job on macOS and Windows with `cargo test`; Node is needed only to regenerate or check the traces.

## 5. Tooling and CI

Node and pnpm are in `mise.toml` (major ranges, exact versions and checksums in `mise.lock` for the three lock platforms); Quint is exact-pinned in `specs/package.json` and `pnpm-lock.yaml`, which records integrity hashes only, no registry URL. `specs/pnpm-workspace.yaml` denies every dependency build script (`allowBuilds`); Quint needs none (protobufjs's postinstall is not needed to run it). `package.json` sets no `packageManager`, so `mise.lock` stays the one pnpm pin. Every Quint command runs with `--backend typescript`, which evaluates inside the locked package; the default Rust backend downloads an evaluator binary at run time.

From `specs/`:

```
pnpm install --frozen-lockfile
pnpm run typecheck      # quint typecheck of both files
pnpm run simulate       # seeds 1-4 in parallel, --max-samples 1000, --max-steps 40, all invariants and witnesses
pnpm run traces:check   # spec and specs/traces/ agree
pnpm run traces:write   # regenerate specs/traces/ after a spec change
```

The `specs` job of `ci.yml` (`ubuntu-latest`, mise pinned in `mise-action`) installs Node and pnpm through mise and runs those checks. `lefthook.yml` runs typecheck and the trace check on commit and the simulation on push for changes under `specs/`, each after `pnpm install --frozen-lockfile` (about 50 ms when `node_modules` is up to date), and runs the workspace tests on push when a trace changes. `verifyDepsBeforeRun: error` in `pnpm-workspace.yaml` also makes every `pnpm run` fail while `node_modules` does not match `package.json` and the lockfile. The simulation size reaches every witness: at 250 samples × 30 steps an attributed ack through the gate was reached in under 1 % of traces. The four seeds run in about 15 s on a development machine.

**Apalache** (`quint verify`) is not in CI and is unverified: it needs a Java runtime and downloads Apalache, and whether it accepts the spec's list folds and maps of records is untested. To try it: `pnpm exec quint verify thread_input.qnt --invariant allInvariants --max-steps 10`.

## 6. Extending

- **PR 9** implements the buffering and promotion the model's `buffer*` actions and `bind` state (hole d).
- **PR 10** makes the supervisor layer real: each guard here is a checklist item, it decides holes a–c and the recheck-attempt choice of §3, and it grows the model where §1 says. A changed guard changes the spec and its traces in the same PR.
- **PR 13–14** correlate card resolutions and card keys by `approval_id` (the card finding).
- **Q-B** (Sync Seam) adds `specs/sync.qnt`, its own `*_traces.qnt`, and a replay test of `decide_replay` and `check_receipt`; `scripts/simulate.mjs` and `traces.mjs` take a spec list instead of the one file when it lands.
- **PR 17**'s `stateright` model checks the Rust sync code; Q-B checks the contract it implements.
