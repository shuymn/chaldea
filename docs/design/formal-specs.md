# Design: Formal Specs (Quint)

Status: Q-A implemented in `specs/` (issue #13, target 2). Q-B, the Sync Seam spec, follows on the same tooling. Cited as "mvp §N" for `docs/mvp-design.md`.

**Roles.** Quint checks the design-stage contract under interleavings: what the daemon may feed the reducers, in which order, and what must then hold. It does not check the Rust code; `proptest` (`crates/core/tests/input_step.rs`, `state_reduce.rs`) checks each reducer alone, and the `stateright` model of PR 17 checks the sync code. Q-A's value is the coupled model: `ThreadState`, the gate projection, several inputs across two launches, and attention, together. The trace bridge (§4) ties the two worlds: committed Quint traces replay against the real reducers.

| File | What |
|---|---|
| `specs/thread_input.qnt` | Q-A: the model, its invariants and witnesses |
| `specs/thread_input_traces.qnt` | the named deterministic traces (`run trace_*`) |
| `specs/traces/*.itf.json` | those traces, exported as ITF and normalized |
| `specs/scripts/{quint,simulate,traces}.mjs` | the Quint runner, the seeded simulation, the trace export and freshness check |
| `specs/package.json`, `package-lock.json` | Quint, exact-pinned |
| `crates/core/tests/quint_traces.rs` | the replay test |

## 1. What Q-A Models

Two layers in one module:

- **Reducer layer**: `reduce`, `stepInput`, and `gate` mirror `crates/core/src/state.rs` `reduce`, `input.rs` `step`, and `input.rs` `gate` branch for branch, including `StatusChanged` output. Their bugs are the spec's bugs; the trace replay keeps them equal.
- **Supervisor layer**: the actions, whose guards are the contract of mvp §2.2, §2.3, §2.4 "Restart", §2.9, §2.10. They are the obligations PR 10 implements.

**Bounds.** Two launches, three inputs, two approval cards; task states `none`, `active`, `completed`; reject reasons `control_character` and `spawn_failed`.

**Abstractions.**

- Stream positions are one monotonic counter `seq`; a position of 0 stands for `None`.
- The PTY writer is the list of counted writes of the current launch (`WInput(id)`, `WHuman`, `WReply`); a position is an index into it. Readiness evidence stores the list length at its intake as its basis. Every VT reply is counted (the default `ClassSet::ALL`, `docs/design/pty-runtime.md` D3).
- Prompt text is abstract: a `UserPromptSubmit` names the set of in-flight inputs of the current launch whose expected form it equals (a nondeterministic choice).
- Timers, write outcomes, the ack window, and the gate timeout are explicit nondeterministic actions.
- Inputs are created for the current launch while it is `starting` or `running`.

**State** (`st`, one record, and `last`, the step's record):

| Variable | Meaning | Source |
|---|---|---|
| `st.thread` | the three axes and the attention set (`ThreadState`) | mvp §2.2 |
| `st.seq` | the thread stream's head | mvp §2.1 "Streams" |
| `st.launch`, `st.manual`, `st.deathConfirmed` | current launch number, manual launch, the current or last launch's death confirmed | mvp §2.9 |
| `st.gp` | gate evidence of the current launch: bound and its seq, readiness seq, evidence kind and basis, last write, last gate-closing human write, last turn start, bracketed paste needed and on | mvp §2.1 "Projections" |
| `st.writer` | counted writes of the current launch, in writer order | docs/design/pty-runtime.md §2.3 |
| `st.inputs` | per input: state, launch, positional, `sent_after_seq`, dispatched, write position, pending ack candidate | mvp §2.3 |
| `st.cardsOpen`, `st.cardSuperseded` | open cards; a card closed by `Stop`, interrupt, prompt, or exit with none opened since | mvp §2.10 |
| `st.sendCount`, `writeCount`, `sendRecords`, `ackRecords`, `reservedWithoutDeath` | history the invariants read | — |
| `last.act`, `last.calls`, `last.gate` | the action and its arguments, the reducer calls it made with their results, the gate decision after it | §4 |
| `last.prevThread`, `last.prevInputs` | the state before the step, for transition invariants | — |

**Actions** (supervisor layer; `step` groups them so the simulator, which picks uniformly among enabled branches of `any`, reaches deep sequences):

| Action | Contract | Source |
|---|---|---|
| `reserve(positional, manual, bp)` | only after `exited` with death confirmed; resets the gate projection and writer; a positional prompt is created `sending` | mvp §2.9:323, §2.3:186 |
| `reserveRefused` | death unconfirmed: raise `resume_blocked`; the reducer then refuses `Reserved` | mvp §2.9:326 |
| `confirmDeath` | `launch_death_confirmed` | mvp §2.1:84 |
| `spawn` / `spawnFail` | `launch_started` moves the positional prompt to `sent` with `sent_after_seq` = its seq; a confirmed spawn failure rejects it | mvp §2.3:186 |
| `bind` | `session_bound`, also a turn start | mvp §2.1:115, §2.9:325 |
| `ready(ev)` / `ownerReady` | readiness of a bound live launch only; `chaldea ready` on an unbound launch writes nothing | mvp §2.3:171 |
| `prompt(matches)` | bound `UserPromptSubmit`: turn start, supersedes cards, ack attribution (unique match, same launch, after the attempt, dispatched, no other counted write after it; a `sending` input keeps the candidate) | mvp §2.3:182, docs/design/pty-runtime.md §2.4 |
| `humanWrite(o)` | `attach` and `key` set the last write and last human write; `card_key` (only with a card open) only the last write | mvp §2.1:115, §2.3:174 |
| `vtReply` | a counted write with no event | docs/design/pty-runtime.md §2.5 |
| `send`, `open`, `dispatch`, `outcome` | `queued`; the gate opens and `sending` advances the last write; the writer's recheck (excluding the input's own attempt) dispatches or cancels; `sent` (then the candidate) or `unconfirmed` | mvp §2.3:163-186 |
| `windowExpired`, `gateTimeout`, `cancelInput`, `rejectInput`, `resolveInput` | the remaining input transitions; the window of a positional prompt is suspended while a startup cause is active | mvp §2.3 |
| `stop`, `exit(reason, dc)`, `restart` | stop; exit recovers the launch's inputs; restart exits the open launch with death unconfirmed and recovers every input | mvp §2.3:183, §2.4:214, §2.9:326 |
| `turnStop`, `interrupt`, `cardOpen`, `cardResolve`, `lateResolve` | turn and card events; a late resolution after supersession | mvp §2.2, §2.10 |
| `sessionTimer`, `stall` | startup causes of a running unbound launch | mvp §2.2:142-143 |
| `setTask`, `decset` | `task_set`; DECSET 2004 | mvp §2.2, §2.3 |
| `raw(i)`, `rawReadyUnbound(ev)` | reducer inputs with no supervisor guard; `stepUnguarded` adds them | §3 |

**Not modelled; added when PR 10 is designed.** The PTY writer lock and the concrete `Basis` (positions here are list indices; PR 10 replaces them with `last_started` per class and makes `counted` an adapter policy), timer durations, the restart sid scan and descendant kills, and worktree and close races. When PR 10 fixes the recheck view, the ack window and settle timers, or the binding promotion order of buffered hooks, the model grows those actions and the traces are re-exported.

## 2. Invariants

All are checked by `npm run simulate` (§5).

| Invariant | Statement | Source |
|---|---|---|
| `sendingAtMostOnce` | an input enters `sending` at most once | CL-4; `input.rs:3-4` |
| `writtenAtMostOnce` | an input's text is dispatched at most once | arch §4.3; mvp §2.3:183 |
| `sendingOnlyThroughOpenGate` | `sending` through the gate only when bound, running, idle, evidence newer than the last write, no other in-flight input of the launch, bracketed paste on where needed (the positional prompt is exempt) | mvp §2.3:171; CL-5 |
| `oneInFlightPerLaunch` | at most one in-flight input per launch | mvp §2.3:171 |
| `earlierLaunchesNeverBlock` | `InputInFlight` only from an input of the current launch | mvp §2.3:171; CL-6 |
| `terminalStatesImmutable` | terminal input states never change | mvp §2.3:185; `input.rs:36-46` |
| `neverResent` | nothing leads from `sending`/`sent`/`unconfirmed` back to `queued`/`sending`; `unconfirmed` leaves only to `acked` or `dismissed` | arch §4.3; mvp §2.3:183 |
| `unconfirmedAttentionIff` | `input_unconfirmed(id)` iff that input is `unconfirmed` | mvp §2.2:144 |
| `gateTimeoutOnlyWhileQueued` | `input_gate_timeout(id)` only while that input is `queued` | mvp §2.2:145 |
| `noStartupCauseAfterBindOrExit` | no startup cause once bound or exited | mvp §2.2:142-143 |
| `lateResolutionChangesNothing` | a resolution after `Stop`, interrupt, or exit changes nothing | mvp §2.10:335 |
| `restartRecovers` | a restart turns `sending`/`sent` into `unconfirmed` and `queued` into `cancelled` | mvp §2.4:214; CL-6 |
| `exitedLaunchHasNoPendingInput` | after exit no input is `queued`, `sending`, or `sent` | mvp §2.3:183, :185 |
| `ackOnlyWhenAttributable` | an attributed ack is of the same launch, after the attempt, a unique match, dispatched, with no other counted write after it | mvp §2.3:182; CL-7 |
| `reservedOnlyAfterDeath` | a launch is reserved only after the previous one's death is confirmed | mvp §2.9:323; CL-18 |
| `resumeBlockedOnlyWhileDeathUnconfirmed` | `resume_blocked` only while exited with death unconfirmed | mvp §2.2:146 |
| `exitedTurnUnknown` | an exited launch's turn is `unknown` | mvp §2.2:135 |
| `taskOnlyByTaskSet` | the task axis changes only on `task_set` | CL-2 |
| `gateEvidenceIsBound` | gate evidence is of the bound launch, after its binding | mvp §2.3:171, §2.9:325 |
| `supervisorInputsAccepted` | the supervisor feeds the reducers only inputs they accept, except the reservation it expects refused while `resume_blocked` is active | — |

**Witnesses** (`witness*` in the spec) count traces reaching an attributed ack through the gate, a late ack, a failed recheck, a dismissal, a second launch, an acked positional prompt, `resume_blocked`, and an open gate beside an earlier launch's `unconfirmed` input. `simulate` fails when one is never reached, so a model change that cuts coverage is caught.

**Model results.** Every invariant holds over the CI simulation. Two consequences of the contract the model makes visible: with one in-flight input per launch and attribution limited to the same launch, "a prompt matching more than one input acks none" (mvp §2.3:182) is unreachable; and the gate projection counts an input's own `sending` as the last write and as in flight, so the writer's recheck must not evaluate `gate()` on the projected view unchanged (`trace_send_ack` shows `WriteAfterEvidence` right after `open`).

## 3. Reducer Gaps the Traces Show

`raw` and `rawReadyUnbound` feed the reducers what the supervisor's guards forbid. `quint run thread_input.qnt --step stepUnguarded --invariant <name> --seed 1 --backend typescript` finds a, c, and d (`reservedOnlyAfterDeath`, `noStartupCauseAfterBindOrExit`, `gateEvidenceIsBound`) and an `input_unconfirmed` raised for an input that is not `unconfirmed` (`unconfirmedAttentionIff`); b breaks no safety invariant, since the gate stays closed. The named traces fix one example each; each ends with an `expect` stating the gap, and the replay shows the real reducers accept every step. Today each guard lives only in the supervisor layer (PR 9, PR 10).

| Trace | What the reducers admit | Contract |
|---|---|---|
| `trace_hole_a_reserve_without_death` | after a restart, `Reserved` before any `resume_blocked`: `Exited` carries no death bit and `Reserved` checks only the attention flag (`state.rs:174`, `:278-288`) | mvp §2.9:323 |
| `trace_hole_b_recheck_keeps_gate_closed` | a failed recheck cancels with zero bytes, yet the attempt advanced the last write (mvp §2.1:115), so the gate stays `WriteAfterEvidence` with no write since the evidence until new evidence or `chaldea ready` | mvp §2.3:171, :175 |
| `trace_hole_c_stale_startup_cause` | `AttentionRaised(StartupStall)` while `exited`, which then survives `Reserved` into the next launch (`state.rs:369-371`) | mvp §2.2:142-143 |
| `trace_hole_d_unbound_readiness` | `ReadinessObserved` of an unbound launch (`state.rs:333-336`); after binding the gate opens on evidence older than the binding | mvp §2.3:171, §2.9:325 |
| `trace_finding_positional_vt_reply` | a VT reply after `launch_started` makes the positional prompt's ack non-attributable under the default counted classes | mvp §2.3:182; docs/design/pty-runtime.md §2.3 "Basis" |

## 4. Trace Bridge

`npm run traces:write` runs `quint test thread_input_traces.qnt --match ^trace_ --out-itf` and writes `specs/traces/<test>.itf.json`, dropping the top-level `#meta` (timestamp and path) and putting one state per line. `npm run traces:check` regenerates them in the OS temp directory and fails on a missing, stale, or different file.

`crates/core/tests/quint_traces.rs` decodes the ITF with `serde_json` (already a `core` dependency; no `itf` crate) and, from `ThreadState::registered(task)`, applies every `last.calls` entry to the real `reduce` or `step` (or creates the input), asserting acceptance or rejection as recorded and, for `reduce`, the exact `StatusChanged` list. After each step it compares the thread state and every input state with the model's and evaluates `gate` on the step's projection, with `in_flight` from `blocks_gate` over the real input states, against `last.gate`. Reverting one reducer branch fails it (verified for `Exited` resetting the turn, `Sending + RecheckFailed`, and the `WriteAfterEvidence` check).

The replay runs in the `workspace` job on macOS and Windows with `cargo test`; Node is needed only to regenerate or check the traces.

## 5. Tooling and CI

Node is in `mise.toml` (major range, exact version and checksums in `mise.lock` for the three lock platforms); Quint is exact-pinned in `specs/package.json` and `package-lock.json`. Every Quint command runs with `--backend typescript`, which evaluates inside the locked npm package; the default Rust backend downloads an evaluator binary at run time.

From `specs/` (after `mise install` and `npm ci`):

```
npm run typecheck      # quint typecheck of both files
npm run simulate       # seeds 1-4 in parallel, --max-samples 1000, --max-steps 40, all invariants and witnesses
npm run traces:check   # spec and specs/traces/ agree
npm run traces:write   # regenerate specs/traces/ after a spec change
```

The `specs` job of `ci.yml` (`ubuntu-latest`) installs Node through mise and runs those three checks; `lefthook.yml` runs typecheck and the trace check on commit and the simulation on push. Samples and steps are above the first plan (250 × 30) because at that size an attributed ack through the gate was reached in under 1 % of traces; the whole job's Quint work takes about 20 s on a development machine.

**Apalache** (`quint verify`) is not in CI and is unverified: it needs a Java runtime and downloads Apalache, and whether it accepts the spec's list folds and maps of records is untested. To try it: `npx quint verify thread_input.qnt --invariant allInvariants --max-steps 10`.

## 6. Extending

- **PR 10** makes the supervisor layer real. Its design decides each §3 gap (guard in the supervisor, or a reducer or projection change) and grows the model where §1 says. The model's guards are its checklist; a changed guard changes the spec and its traces in the same PR.
- **Q-B** (Sync Seam) adds `specs/sync.qnt`, its own `*_traces.qnt`, and a replay test of `decide_replay` and `check_receipt`; `scripts/simulate.mjs` and `traces.mjs` take a spec list instead of the one file when it lands.
- **PR 17**'s `stateright` model checks the Rust sync code; Q-B checks the contract it implements.
