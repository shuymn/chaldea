# Design: Supervisor, Send and Ack Side (MVP PR 10b)

Status: design. Scope is the send and ack half of the PR 10 row of `docs/mvp-design.md` §4 (cited as "mvp §N"), called **PR 10b**: the `send`, `ready`, `key`, `input_resolve`, and `input_cancel` commands; the readiness gate and its evidence; the writer integration (`encode` at write time, the `WriteUnit`, the recheck, the outcome mapping, `human_write`); ack attribution with `prompt_matches`; the real `IntakeProbe`; input-side attention and approval cards; and the send path's guards in `specs/thread_input.qnt`. It builds on the 10a1 design, `docs/design/supervisor.md` (cited as "sup §N"), through its §2.11 interfaces and the changes listed in §2.9 here.

**What 10b decides and leaves.** 10b owns the daemon's `supervisor::send` module, a `core` key encoder, the hook adapter wrapper that turns decoded hooks into state, and the small changes to PR 9's hook seams and to the event store's input projection that §2.9 lists. It leaves to PR 13 and PR 14 the hook configuration of real CLIs and the decoders that read their payloads (until then no real CLI binds, sup SD14), the readiness and resume columns of `GET /api/adapters`, session switches, and the approval correlation rule of mvp §2.10; to PR 11 attach and resizing (F5); to PR 18–19 the UI; to PR 10a2 crash recovery.

**Commitments from earlier PRs this design keeps.**

1. Thread and input state move only through explicit events: hooks, the writer's completion records, and the supervisor's own commands (invariant 1). Nothing reads PTY text; the VT is read only for its mode word and a diagnostic snapshot (mvp §2.2).
2. No input is resent automatically: nothing leads from `sending`, `sent`, or `unconfirmed` back to `queued` or `sending` (`step`, `crates/core/src/input.rs:178-211`).
3. `reduce`, `step`, `gate`, and `blocks_gate` keep their rules; `GateView` and every public payload keep their fields. New public names are listed in §1 and flagged in §3.
4. `HookAdapter::interpret` stays data-only on the store writer (hook-ingest §2.6): it reads through its `ReadTx` and its `AcceptedHook`, never supervisor memory. Supervisor memory learns of a delivery only after its job committed (§2.9 "PR 9 seams").
5. Every wait has a deadline through `deadline::settle_before`, `LiveLaunch::with_session`'s deadline, or `supervisor::record`'s policy (§2.8).

Sources read for this document:

| Source | Revision | Read for |
|---|---|---|
| This repository | `6581c36` | `AGENTS.md`; `docs/architecture.md`; `docs/mvp-design.md` §2.1–§2.11, §4, §6, §8; `docs/design/{supervisor,input-encoders,pty-runtime,hook-ingest,http-surface,formal-specs,deadlines,event-store}.md`; `crates/core/src/{state,input,adapter,payload,auth,http,ids}.rs`, `crates/core/src/input/encode.rs`; `crates/pty-runtime/src/{session,writer,origin,snapshot,vt}.rs`; `crates/daemon/src/hook/{mod,adapter,intake,route}.rs`, `crates/daemon/src/http/{commands,auth,ctx}.rs`, `crates/daemon/src/store/{readers,rows,schema,project,content,annex}.rs`, `crates/daemon/src/deadline.rs`; `crates/testkit/src/fake_cli/hooks.rs`; `specs/thread_input.qnt`, `specs/thread_input_traces.qnt` |
| PR 10 split | orchestrator note, 2026-11-25 | the 10a / 10b cut and the 10b obligation ids |

## 1. Goal and Dependencies

**Goal.** A prompt given to `chaldea send` is written to the CLI's PTY exactly once, as one encoded unit, only while the gate holds and the writer's recheck confirms it, or is shown `rejected` or `cancelled` with zero bytes; every written input ends `acked` only on a `UserPromptSubmit` that matches its text and can be attributed to its completed write, otherwise `unconfirmed` and never resent; and every human write, card, and timer reaches the store as an explicit event.

**Depends on** 10a1 (sup §2.11 with §2.9's changes: `LiveLaunch::with_session`, `Supervisor::observe`, `Published.reference`, `ExitStep`, `TickStep`, `ExitFacts.sealed`, `supervisor::record`, `EffectOutcome`), PR 5 (`check`, `encode`, `prompt_matches`, `capabilities`), PR 9 (`HookAdapter`, `IntakeProbe`, `AcceptedHook`), PR 6 (`prepare_content`, `ReadTx::blob`), PR 2 (`Session::submit`, `Guard`, `WriteContext`).

**Files and seams** (every name marked new does not exist at `6581c36` and is not a 10a1 name):

```
crates/core/src/adapter.rs        # ReadinessMode, Capabilities::readiness_mode, evidence_allowed (new)
crates/core/src/input/key.rs      # new: KeyToken, encode_keys, MAX_KEYS
crates/core/src/command.rs        # SendArgs, SendResult, ReadyArgs, ReadyResult, KeyArgs, KeyResult, KeyOutcome,
                                  #   InputResolveArgs, InputCancelArgs, InputCommandResult (new)
crates/core/src/http.rs           # ErrorCode::{NoLiveLaunch, NotBound, CardClosed, KeyNotWritten, InputState} (new)
crates/core/tests/contracts_export.rs, golden.rs # the new wire types
crates/daemon/src/supervisor/send/ # new: mod (SendMap, SendLaunch, Dispatch, MirrorView, TurnLedger), driver (Driver,
                                  #   open_job), completion, hooks (SupervisorHooks, HookDecoder, HookFact, NoDecoder),
                                  #   probe (SupervisorProbe, IntakeFactsV1), attribution, exit (SendExitStep),
                                  #   tick (SendTick), commands (the five handlers)
crates/daemon/src/hook/{adapter,intake,mod,route}.rs # the PR 9 seam changes of §2.9 (TurnMark, IntakeProbe::finished)
crates/daemon/src/store/{schema,project}.rs # proj_inputs.attempt_seq, last_write_before; schema v3 (§2.9, SB1)
crates/daemon/src/supervisor/      # the 10a1 changes of §2.9
crates/cli/src/agent_cmd.rs       # send, ready, key, input resolve, input cancel
crates/testkit/src/fake_cli/{stall,submit}.rs # new FakeCli modes `stall` and `submit`
crates/testkit/src/fake_decoder.rs # new: FakeDecoder, a HookDecoder over FakeCli payloads (tests only)
specs/thread_input.qnt, thread_input_traces.qnt, traces/ # §7, in the implementation PR
```

**New identifiers** besides those above: `SendMap`, `SendLaunch` (fields `last_taken`, `open_intakes`, `human_recorded`), `Dispatch` (fields `input_id`, `position`, `written`, `competing`), `MirrorView` (fields `as_of`, `view`, `in_flight`), `TurnLedger` (fields `watermark`, `starts`, `ends`, `stop`), `Driver`, `open_job`, `SupervisorHooks`, `HookDecoder`, `HookFact` and its variants, `NoDecoder`, `FakeDecoder`, `SupervisorProbe`, `IntakeFactsV1` (fields `v`, `reference`, `last_started`, `inflight`), `TurnMark` and its variants `Start`, `End` (field `settle`), `Evidence`, `IntakeProbe::finished`, `SendExitStep`, `SendTick`, `QueuedPrompt`, `LiveLaunch::with_session_blocking`, `ReadinessMode` and its variants `StopSettle`, `PostDecision`, `OwnerOnly`, `Capabilities::readiness_mode`, `evidence_allowed`, `KeyToken` and its names (`enter`, `escape`, `tab`, `backspace`, `ctrl_c`, `up`, `down`, `right`, `left`, `char`), `encode_keys`, `MAX_KEYS`, `KeyOutcome` and its values (`written`, `incomplete`, `pending`), the recheck reason `hook_taken_in`, the `proj_inputs` columns `attempt_seq` and `last_write_before`, the wire types of §2.2, the error codes `no_live_launch`, `not_bound`, `card_closed`, `key_not_written`, `input_state`, the constants `DISPATCH_WAIT`, `WRITE_STALL`, `ACK_WINDOW`, `GATE_TIMEOUT`, `SETTLE`, FakeCli modes `stall` and `submit`, the Quint names of §7, and every test name in §5.2.

## 2. Design

### 2.1 Rules

1. **The store decides, memory mirrors.** Whether an input opens, is acked, or times out is decided in one store job that re-reads the projection. Memory (`SendLaunch`) holds only what a non-blocking caller needs and the store cannot give in time: the recheck's view, the in-flight dispatch for the intake probe, intake counters, the turn ledger, and timer start instants. It dies with the launch; a restart exits every launch (sup §2.5).
2. **Positions, not wall time, order writes against hooks.** Every attribution and staleness test compares writer positions (`last_started` per `OriginClass`, `WriteContext.seq`, `Published.reference`), as pty-runtime §2.3 "Ack attribution" states. Durations (settle, gate timeout, ack window, write stall) are measured on the supervisor's monotonic clock (`SupervisorConfig.clock`).
3. **Zero bytes or unknown, never guessed.** `NotStarted` and every refused submission are zero bytes (`RecheckFailed`); `Incomplete` and a dispatched unit with no completion are unknown (`WriteOutcomeUnknown`), pty-runtime D5.
4. **One in-flight input per launch.** Only the open job moves an input to `sending`, and it refuses while another input of the launch is in flight (`gate`'s `InputInFlight`).
5. **Submit after commit.** A unit is submitted only after the job that allows it returned its committed value (§2.9 D1).

### 2.2 Commands

Registered in `Built::commands` beside 10a1's four (E1). Each `prepare` parses `args` (400 `bad_args`) and builds an intent from data. Each intent checks the target against the token's grant first, through `deny_for` on the target thread (403 for a thread the token can read, 404 otherwise, `http/auth.rs:330-339`).

| Command | Who (`auth.rs:126-175`) | Args | Intent | Effect | Success |
|---|---|---|---|---|---|
| `send` | owner; agent self+ | `SendArgs { thread_id, text, origin? }` (`origin` default `cli`) | `thread_closed`; current launch `starting` or `running`, else `no_live_launch`; `input_created { launch_id: current, initial_state: queued, encoding: caps.pty_encoding, origin, created_by }` with `created_by` from the authenticated principal (`Human` for the owner, `Thread(me)` for an agent) and `text` through `prepare_content`; `check(agent, pty_encoding, Send, text)` → `Err(r)`: `queued → rejected(r)` in the same job. `Completed` | none | `SendResult { input_id, state, reject_reason }` (`state` `queued` or `rejected`; `reject_reason` null unless rejected) |
| `ready` | owner | `ReadyArgs { thread_id, expected_launch_id? }` | `thread_closed`; `launch_changed`; current launch `running`, else `no_live_launch`; unbound → `not_bound` (A12). `Pending(L)` | §2.4 "Owner assertion" | `ReadyResult { launch_id }` |
| `key` | owner | `KeyArgs { thread_id, launch_id, keys: [KeyToken], approval_id? }`; `keys` empty or over `MAX_KEYS` → 400 `bad_args` | `thread_closed`; `launch_changed`; `running`, else `no_live_launch`; with `approval_id`: the card open and of `L`, else `card_closed`. `Pending(L, card)` | §2.6 | `KeyResult { outcome, bytes_written }` |
| `input_resolve` | owner | `InputResolveArgs { input_id, delivered }` | input missing → 404; not `unconfirmed` → `input_state`; `Resolved { delivered }` (`acked` by human or `dismissed`), clearing `input_unconfirmed`. `Completed` | none | `InputCommandResult { input_id, state }` |
| `input_cancel` | owner; agent for inputs it created (`CreatedBySelf`) | `InputCancelArgs { input_id }` | reads only the input's `thread_id` and `created_by` first: missing → 404; `deny_for(thread)` unless the token may read it (404) and, for an agent, `created_by == Thread(me)` (403 on a readable thread); then not `queued` → `input_state`; `Cancelled`, clearing `input_gate_timeout`. `Completed` | none | `InputCommandResult { input_id, state }` |

`send`'s admission (C8): text over `MAX_PROMPT_BYTES` is 400 `prompt_too_large` in `prepare`, before any receipt or input. A rejected input is created and rejected in one job so that its reason is public (`input_state_changed.reason`), as input-encoders §4's PR 10 row asks. `origin` is the client's claim and decides nothing. The deadlines are the server's short 5 s (mvp §2.5); `key`'s effect fits it (§2.6). A second request with the same `command_id` replays the stored result, or answers 409 `in_progress` while the first runs (http-surface §2.8).

**Codes** (new `ErrorCode` variants, each in `handler_code` at one status, E2; success types carry no top-level `code`, http-surface.md:544, asserted per type):

| Code (new) | Status | When | Can the work happen later? |
|---|---|---|---|
| `no_live_launch` | 409 | the thread's current launch is not `starting`/`running` (send) or not `running` (ready, key), or the key's effect finds the session gone | no |
| `not_bound` | 409 | `ready` for a launch with no bound session (A12) | no |
| `card_closed` | 409 | a card key whose card is not open in `L` when the intent or the effect reads it (mvp §2.10) | no |
| `key_not_written` | 409 | the key unit wrote zero bytes (§2.6 lists the cases); the owner may retry | no |
| `input_state` | 409 | `input_resolve` of an input not `unconfirmed`, `input_cancel` of one not `queued` | no |

`thread_closed`, `launch_changed`, `prompt_too_large`, and `prompt_rejected` are 10a1's (sup §2.2).

**Claude's first prompt.** `spawn --prompt` and `resume --prompt` for Claude (400 `bad_args` in 10a1) become a queued input made at reservation (§2.9 D3). `spawn`'s `prepare` runs the size check and `check(Claude, BracketedPaste, Send, text)` (400 `prompt_too_large`, `prompt_rejected`); `resume`'s `prepare` runs the size check, and its reservation job, which reads the thread's agent (sup §2.2), runs `check` before it reserves. The reservation job appends `input_created { launch_id: L, initial_state: queued, encoding: bracketed_paste }` after `launch_reserved`; the input is written when the gate first opens, and `SpawnResult.input_id` (`ResumeResult.input_id`) names it.

### 2.3 The Send Path

`Driver` is one supervisor task started in `SupervisorComponents::attach`. It wakes on `Store::subscribe` notifications naming a thread stream, on every tick that `SendTick` forwards (with the live list, §2.7), on `RecvError::Lagged` (then it rescans every live launch of the last tick's list), and on its own completions. Per wake-up it handles each named launch `L` that is its thread's current launch and has a `Supervisor::live(thread, L)` entry:

1. **Read.** One read transaction (`ReadTx::launch`, `ReadTx::thread`, `ReadTx::inputs`) on a blocking task, awaited with `settle_before` until now + 1 s; not in time → skip `L` until the next wake-up. The result replaces `L`'s `MirrorView` (`as_of` = the thread stream head of that read; `view`; `in_flight` input ids) unless the stored mirror is newer.
2. **Sample.** If the oldest `queued` input `Q` of `L` exists and nothing of `L` is in flight: `with_session(now + 250 ms, |s| (s.modes(), s.observer().last_started_each()))` and `SendLaunch.human_recorded`. `Busy` or `Gone` → skip.
3. **Open job** (`open_job`, through `supervisor::record` with a typed value, §2.9 D1). Re-reads `L`, `Q`, and the thread; builds the `GateView` from the projection with `needs_bracketed_paste` = `pty_encoding == BracketedPaste` and `bracketed_paste_on` from the sampled modes; `gate(view)`. It also refuses as `WriteAfterEvidence` (SB3) when the sampled `last_started` over `write_basis.counted` is above `write_basis.position` (a counted write after the evidence with no event of its own, such as a VT reply), or when the sampled `last_started[human]` is above `human_recorded` (a human unit started whose `human_write` is not yet recorded, so the projection cannot show it yet; B2). Open → `input_state_changed(queued → sending, GateOpened, sent_after_seq = head)`, clearing `input_gate_timeout`; the projection records the input's `attempt_seq` and `last_write_before` (§2.9 D6). The job returns `Opened { attempt_seq, write_basis, epoch, competing }` with `competing` = `last_human_write_seq > last_turn_start_seq` (§2.5), or `Closed(reason)`: `Q` stays `queued` and the next wake-up decides again.
4. **Submit** (after `Opened` returned). `Q`'s text, resolved through `ReadTx::blob` when it is `Content::Blob`, goes to `encode(agent, host_os, text, Send)` (C2); an `Err` cannot happen after `check` accepted the text at `send` (PR5-CL-1) and is treated as a refused submission. The unit is `WriteUnit { origin: Input { input_id }, parts: [Bytes(text), Pause(submit.delay), Bytes(submit.bytes)] }` (input-encoders §2.3). The `Guard`: `not_after` = now + `DISPATCH_WAIT` (10 s); `basis = Basis { seq: write_basis.position, counted }`; `bracketed_paste = Some(epoch)` iff `needs_bracketed_paste`; `recheck` below. `SendLaunch` records `Dispatch { input_id, position: None, written: false, competing }`, and the mirror takes the open job's view with `as_of` = the attempt's seq. The unit's `Completion` sets `Dispatch.written` (an atomic) when the outcome is `Written`, then sends the record to the driver's channel; it never blocks. Then `with_session(now + 1 s, |s| s.submit(unit, guard, completion))`. `Busy`, `Gone`, or a `SubmitError` → the completion job with `RecheckFailed` (nothing was submitted, so zero bytes are certain).
5. **Recheck** (on the writer thread, every earlier unit finished, pty-runtime §2.3). It stores `ctx.seq` into `Dispatch.position`, then evaluates `gate` on the mirror's view *without the input's own attempt* (A33): `last_write` = the input's `last_write_before` when the mirrored `last_write_seq` is its `attempt_seq`, `in_flight` over the mirror's in-flight ids minus this input, `bracketed_paste_on` from `ctx.modes`. `as_of` below the attempt's seq, or a closed gate → `Err(reason)`. The writer itself checks the basis against the counted classes (refusing `WriteSinceBasis`, `BasisUnknown`), the 2004 epoch, exit, close, and any mode-word change between the recheck and the first permission (X2: `Recheck("modes_changed")`).
6. **Completion job** (F3). Only for an input still `sending` of a launch with no `launch_exited` (a completion after the exit job is ignored, pty-runtime §2.3): `Written` → `WriteCompleted` (`sent`); `NotStarted(_)` → `RecheckFailed` (`cancelled`; the projection restores `last_write_seq`, SB1); `Incomplete` → `WriteOutcomeUnknown` (`unconfirmed`, raising `input_unconfirmed`). An input that a hook already moved on (§2.5 step 3) is left as it is. A record whose job ended `Unrecorded` stays in `SendLaunch` and is applied again on each tick until a job commits; the job re-validates, so a second application changes nothing. `Attach`, `Key`, and `CardKey` records go to §2.6.
7. **Write stall.** A dispatched unit (`Dispatch.position` set) with no completion `WRITE_STALL` (30 s) after the driver saw its dispatch, measured on the monotonic clock, gets the completion job with `WriteOutcomeUnknown`: the input becomes `unconfirmed` with `input_unconfirmed`, so the owner sees it and the gate stays closed (`InputInFlight`) until they resolve it. The completion that arrives later finds the input settled. The writer's started unit is never cancelled or repeated (pty-runtime §2.3 "Started units").

**After exit.** `SendExitStep` (10b's `ExitStep`, sup §2.11 "Exit job order") applies each `ExitFacts.sealed` record whose origin is `Input { input_id }` to that input if it is still `sending` (`RecheckFailed`; a sealed unit wrote zero bytes), expires the launch's open cards (`approval_resolved { outcome: expired }`, §2.9 D4), and its `after_commit` drops `L`'s `SendLaunch`. A unit refused `ChildExited` before the exit job commits is cancelled by its completion job; one whose completion comes after is ignored and the 10a1 sweep has made the input `unconfirmed` (A23). At restart the exit steps run with `restart: true` (sup §2.5), so cards of every open launch expire there too.

### 2.4 Gate Evidence

A `readiness_observed` carries `EventAnnex::Readiness(WriteBasis { launch_id, position, counted })` (event-store §2.6), `position` = `last_started(counted)` sampled at the evidence's own intake and `counted` = the adapter's policy, `ClassSet::ALL` for every row (pty-runtime D3; S15). Every job that appends one also appends the `status_changed` that `reduce(&thread.state, &StateInput::ReadinessObserved)` returns (the projection sets the turn only from `status_changed`, `project.rs:224-242`), and moves `L`'s `TurnLedger.watermark` after commit (to the evidence hook's receipt ordinal through its `TurnMark::Evidence`, to the settled `Stop`'s ordinal, or to `ready`'s sampled `last_taken`). Per kind (A9, X3):

| Evidence | Where it is decided | Its intake (the basis) |
|---|---|---|
| `session_start` | the binding job, in `SupervisorHooks::interpret` for the binding `SessionStart` | the `SessionStart`'s `IntakeFactsV1.last_started` |
| `stop_settled` | the driver's settle job | `Supervisor::observe` sampled when the driver builds the job (an `Unobservable` launch is skipped that tick) |
| `post_decision` | `interpret`, when a decoder reports one | the hook's intake facts |
| `owner_asserted` | `ready`'s effect | `with_session(now + 1 s, \|s\| s.last_started(counted))` |

Evidence is recorded only when `evidence_allowed(row, kind)` holds (the model's `kindAllowed`: `session_start` only for Claude and not `OwnerOnly`; `stop_settled` only under `StopSettle`; `post_decision` only under `PostDecision`; `owner_asserted` always), and, for a hook, its `intake_reliable` is true and its facts decode (D3). `Capabilities::readiness_mode` is `StopSettle` for all four rows (mvp §8 D8); PR 13 and PR 14 may change a row and expose the column.

**Settle completion** (A36, SB14). A settle completion counts only for the `Stop` it was started for, of the current turn, while no later hook of the launch was taken in. The store cannot tell this alone (a `Stop` on an already `unknown` turn appends no `status_changed`, and a direct delivery keeps neither its attempt nor its facts), so memory carries it, fed only after commits (§2.9 "PR 9 seams"):

1. `interpret` returns a `TurnMark` with the events: `Start` for a `PromptSubmitted`; `End { settle }` for a `Stop` or an `Interrupt`, `settle` true only for a `Stop` with `intake_reliable` and facts present; `Evidence` for a hook that appended readiness evidence.
2. After the delivery (or binding) job commits, `IntakeProbe::finished` adds each mark with its receipt ordinal to `L`'s `TurnLedger`, counting `starts` and `ends` among marks with an ordinal above `watermark` (the ordinal of the latest evidence's intake; ordinals give intake order). An `End { settle: true }` whose ordinal equals `SendLaunch.last_taken` (no hook taken in after it) and leaves `ends ≥ starts` becomes `stop = (ordinal, now)`; any other mark clears `stop`.
3. On each tick the driver checks `stop = (r, t)`: `last_taken` still `r`, `ends ≥ starts`, and now ≥ t + `SETTLE`. Then one job re-reads `L` (current, bound, `running`), the thread (turn `unknown`, no open card of `L`), and that no readiness evidence of `L` is newer than delivery `r`; it appends `readiness_observed { stop_settled }` and its `status_changed`.

Counting turn boundaries is how the daemon tells a late `Stop` of an earlier turn from the current turn's: a `Stop` of `P1` taken in after `P2`'s `UserPromptSubmit` leaves `ends < starts`, so it settles nothing; `P2`'s own `Stop` balances the count. A `Stop` taken in whose delivery is still uncommitted has advanced `last_taken`, so the candidate before it is dead. `SETTLE` runs from the commit, which is after the intake, so the interval is never shorter than `SETTLE` after the hook.

**Owner assertion** (C3). `chaldea ready` asserts that the CLI's composer is in its normal edit mode and empty, with no search, popup, or dialog open, which no signal shows (input-encoders §2.6). The effect samples the basis and `last_taken`, then one `supervisor::record` job re-checks that the thread is not closed and that the launch is current, bound, and `running`, and appends `readiness_observed { owner_asserted, by: human }` with its `status_changed`; a re-check that fails answers the intent's code (`thread_closed`, `launch_changed`, `no_live_launch`, `not_bound`). The ledger's watermark becomes the sampled `last_taken`. It writes no PTY byte. A send made when the assertion does not hold follows the ordinary match and attribution rules and is never resent. A write between the command and the sample lies inside the basis (L-3).

### 2.5 Hooks: Decoders, Intake Facts, Ack Attribution

**Decoder seam** (SB10). `SupervisorHooks` (new) implements `HookAdapter` and replaces `MvpHooks` in `SupervisorComponents::build` (its `rule` and `expected_session` are `MvpHooks`'s). Its `interpret` asks a per-agent `HookDecoder` for a `HookFact` and the adapter's own events:

```rust
// crates/daemon/src/supervisor/send/hooks.rs (new)
pub(crate) trait HookDecoder: Send + Sync + 'static {
    /// Data only, as `interpret`. `extra` are the adapter's own events (assistant text, tool activity).
    fn decode(&self, tx: &ReadTx<'_>, hook: &AcceptedHook<'_>) -> Result<(HookFact, Vec<HookEvent>), StoreError>;
}
pub(crate) enum HookFact {
    SessionStart, PromptSubmitted { prompt: String }, Stop, Interrupt, PostDecision,
    CardOpened { tool_name: String, tool_input: serde_json::Value, turn_ref: Option<String>, correlation: Option<ApprovalCorrelation> },
    CardEvidence { tool_use_id: String },       // a PostToolUse; the card whose correlation names it
    Other,
}
```

`NoDecoder` returns `Other` with no events for every hook. Real CLIs send no hook until PR 13 and PR 14 configure them (sup SD14), and with `NoDecoder` a hook that does arrive stays `raw_unknown` `unknown_event`, as with `MvpHooks` today. Tests install `FakeDecoder`. Per fact, in the delivery or binding job (`L` is current, live, and bound, hook-ingest §2.1 rule 5):

| Fact | Events appended (besides `extra`) | Mark |
|---|---|---|
| `SessionStart` | in the binding job only: `readiness_observed { session_start }` and its `status_changed` when §2.4's conditions hold; a later same-session `SessionStart` is PR 13's session-switch decision and appends nothing here | `Evidence` when appended |
| `PromptSubmitted` | `user_message { prompt, input_id }` with the attribution below (`prompt` through `prepare_content`); the `status_changed` of `PromptSubmitted` when the turn changes; open cards of `L` → `approval_resolved { superseded }` | `Start` |
| `Stop` | the `status_changed` of `TurnStopped` when the turn changes; open cards superseded | `End` |
| `Interrupt` (Codex) | the `status_changed` of `Interrupted` when the turn changes; open cards superseded | `End { settle: false }` |
| `CardOpened` | `approval_requested { approval_id = the delivery's emit_id, launch_id: L, … }`, `status_changed` (`ApprovalOpened`) | none |
| `CardEvidence` | the open card of `L` whose correlation has that `tool_use_id` → `approval_resolved { ran }`, then `status_changed` (`ApprovalsAllResolved`) only if no other card of `L` stays open (A34; cards close at every turn boundary, so `L`'s open cards are the current turn's) | none |
| `PostDecision` | `readiness_observed { post_decision }` and its `status_changed` when allowed | `Evidence` when appended |
| `Other` | `extra` | none |

Each `approval_resolved` of one delivery is named `card:<approval_id>`, so one `Stop` can close several cards (event names unique per delivery, §2.9 P3). When the events and `extra` are both empty (a `Stop` on an `unknown` turn with no card, a `CardEvidence` with no open card, a refused `PostDecision`, a `SessionStart` that is no evidence) `interpret` returns `Interpretation::Raw(UnknownEvent)` with the mark, never an empty `Events` (`route.rs:124-126` refuses one, which would roll back a binding).

**Intake facts** (D1). `SupervisorProbe` (new, built in `build` over `Supervisor::observe` and `SendMap`) is the `IntakeProbe`. It never waits. On each call it stores the receipt ordinal into `SendLaunch.last_taken` and increments `open_intakes` (atomics, outside the mutex), then encodes:

```json
{"v":1,"reference":<u64>,"last_started":[<input>,<human>,<reply>],"inflight":{"input_id":"…","position":<u64|null>,"written":<bool>,"competing":<bool>}|null}
```

`LaunchObservation::Started { reference, last_started }` → `reference` and `last_started` as given; `Reserved` → zeros (no child exists, so no write precedes the hook); `Unobservable`, or `L`'s `SendLaunch` mutex contended (`try_lock`) → `None`. `inflight` is `L`'s `Dispatch`, kept until its input leaves flight or the next dispatch. `None`, another `v`, or a shape that does not parse are "facts missing": such a hook acks nothing and is no evidence, never "no write happened". `IntakeProbe::finished` decrements `open_intakes` and feeds the ledger (§2.4).

**Attribution** of a `PromptSubmitted` with prompt `P` (A10, X4, A16–A18, B2, C4), decided in the job:

1. `M` = the in-flight inputs of `L` (`sending`, `sent`, `unconfirmed`) whose text (through `ReadTx::blob` when a blob) has `prompt_matches(agent, encoding, text, P)`. `|M| = 0` → `user_message` without `input_id`. `|M| ≥ 2` → without `input_id`, no input changes (A18).
2. `M = {X}`, attributable only when all hold: `intake_reliable`; facts present; for a positional `X` (`encoding: positional`): `last_started[input] ≤ reference` and `last_started[human] ≤ reference` (SB5); otherwise `inflight.input_id = X`, `position = s` present, `written` true (the whole unit, submit included, was written before the intake), each other counted class's `last_started < s` (no counted write after it), and `competing` false; and no input of `L` that left flight without a hook ack (`acked` with no `ack_seq`, `dismissed`) has `prompt_matches` for `P` (an earlier unattributed dispatch, C4).
3. Attributable → `user_message { input_id: X }`; `X` `sending` → `WriteCompleted` then `AckAttributed` in this job (the facts show the write completed; the completion job later finds `X` settled); `sent` or `unconfirmed` → `AckAttributed` (`unconfirmed → acked` is A7's late ack). Not attributable → no `input_id`; `X` `sent` → `AckNotAttributable` (`unconfirmed`); `sending` or `unconfirmed` → unchanged (the window, §2.7, ends a `sent` input that no attributable hook acks).

A hook taken in before its input's unit finished acks nothing, whether the unit's Return was still to be written or the write ends `Incomplete`; an input whose `user_message` names it is always `acked` (SB4). `competing` is the open job's record of a gate-closing human write (`attach`, `key`) after the launch's last turn start: its submission, if any, is unknown history, so the input stays `unconfirmed` (mvp §2.3 "Ack"; formal-specs §3's case: a human submits `P`, its hook is delayed, the owner asserts readiness, a dispatch of `P` is not submitted). The open job refuses while a human unit's `human_write` is unrecorded (§2.3 step 3), so `competing` reads every human write the writer started. A `key` write after `X`'s dispatch is a counted `Human` write after `s` (A17); a human write during `X`'s unit is queued behind it and is such a write too, so a hook whose intake follows it acks nothing (A8).

### 2.6 Keys, Human Writes, Cards

**Key encoding** (`core::input::key`, new): `KeyToken` is one of `enter` (`\r`), `escape` (`\x1b`), `tab` (`\t`), `backspace` (`\x7f`), `ctrl_c` (`\x03`), `up`/`down`/`right`/`left` (`ESC [ A`… or, under DECCKM, `ESC O A`…), or `{ "char": c }` for one printable ASCII scalar; `encode_keys(tokens, app_cursor)` concatenates them into one `Bytes` part, 1 to `MAX_KEYS` (32) tokens. Keys are VT input on both OSes; ConPTY's input parser turns them into key events, as it does for `win_keys` text (input-encoders §2.5 rule 2's source).

**`key`'s effect** (owner only, mvp §8 "Also decided"). A key reads its own state and does not use the driver's mirror:

1. `live(thread, L)` → `None` → `no_live_launch`. For a card key, sample `L`'s `last_taken` and `open_intakes`; `open_intakes > 0` (a hook is being taken in, which may close the card) → `key_not_written`.
2. Read `L` and, for a card key, the card, on a blocking task with `settle_before` until now + 1 s (not in time → `key_not_written`). `L` not `running` → `no_live_launch`; the card not open → `card_closed`. Every hook taken in before the sample has finished, so its commit is in this read.
3. `with_session(now + 1 s, …)`: read `modes()`, encode with its `app_cursor`, and submit `WriteUnit { origin: Key | CardKey { approval_id }, parts: [Bytes] }` with `Guard { not_after: now + 2 s, recheck }`. The recheck refuses when `ctx.modes.app_cursor` differs from the encoded one (`modes_changed`) and, for a card key, when `last_taken` or `open_intakes` changed since step 1 (`hook_taken_in`). `Busy` or a `SubmitError` → `key_not_written`; `Gone` → `no_live_launch`.
4. Await the completion with `settle_before` until `not_after` + 1 s. `Written` or `Incomplete` → record the `human_write` (below), then `KeyResult { outcome: written | incomplete, bytes_written }`; that job `Unrecorded` → `EffectOutcome::Unrecorded`. `NotStarted` → `key_not_written`. Not received in time (`Elapsed` or `Late`): `WriteTicket::cancel()` true → `key_not_written` (it never dispatches); false → `{ outcome: pending, bytes_written: null }` (dispatched, or settled with its completion not yet run; the completion goes to the driver, which records the `human_write` if bytes may have been written).

A key is not gated (it is how the owner clears a startup prompt, mvp P-12). Only `card_closed` and `no_live_launch` say the target is gone; every other refusal is `key_not_written`, after which a retry may succeed.

**`human_write`** (F4). Every `Attach`, `Key`, and `CardKey` unit with outcome `Written` or `Incomplete` (bytes may have reached the CLI, any `bytes_written`) gets one `human_write { launch_id, origin: record.origin.human_write(), approval_id }`, through `supervisor::record`, with the idempotency key `derived:human_write:<launch_id>:<record.seq>`: the key's effect records it when it has the outcome, the driver records it for every other record (`pending` keys, PR 11's attach units), and the key makes a second recording an existing event. `NotStarted` appends nothing. After either, `SendLaunch.human_recorded` advances to the record's `seq`. The projection then advances `last_write_seq`, and for `attach`/`key` `last_human_write_seq` (event-store §2.6). So after a raw key only evidence of a later turn or `ready` reopens the gate (`gate`'s `EvidenceOfSameTurn`, A11, A20), however late a same-turn evidence is committed; after a card key, which answers a dialog rather than the composer, the turn's next evidence reopens it (A19, mvp §2.3).

### 2.7 Timers

`SendTick` is registered as a `TickStep`; its `tick(now, live)` returns no job and forwards `now` and the live launches to the driver through a `watch` channel (never blocks). The driver runs every timer on the monotonic clock, from instants it keeps in `SendLaunch`, and decides each in a job that re-validates from the store:

| Timer | Start | Job |
|---|---|---|
| Gate timeout (`GATE_TIMEOUT` 60 s, A15) | the driver first read the input `queued` | raise `input_gate_timeout(input_id)` once (stays `queued`, zero bytes). Before the job the driver tries one VT snapshot, `with_session_blocking(now + 250 ms, \|s\| s.snapshot(now + 200 ms))` (§2.9 D2); a snapshot it gets is appended as `vt_snapshot_taken` (`grid` through `prepare_content`) in the same job; `Busy`, `Gone`, or `VtUnavailable` raise the cause without one |
| Ack window (`ACK_WINDOW` 30 s) | the completion job's commit, or the driver first read the input `sent` (a positional input); for a positional input it runs only while neither startup cause is active (mvp §2.3) | `AckWindowExpired` (`unconfirmed`) |
| Settle (`SETTLE` 2 s) | §2.4 | §2.4 |
| Write stall (`WRITE_STALL` 30 s) | the driver saw `Dispatch.position` set | §2.3 step 7 |

A timer starts at most one tick (1 s) after its event; a job that cannot be recorded is tried again on the next tick (sup §2.6).

### 2.8 Waits

| Wait | Helper | Bound |
|---|---|---|
| Driver read; `key`'s read | `deadline::settle_before` on the blocking read | now + 1 s; otherwise skipped until the next wake-up (`key_not_written` for a key) |
| Mode and position sample; `ready`'s basis | `LiveLaunch::with_session` | 250 ms; 1 s |
| Submit (send, key) | `with_session` | 1 s; `Busy` → zero-byte refusal |
| Unit dispatch | `Guard.not_after` (the writer) | `DISPATCH_WAIT` 10 s (send), 2 s (key); a dispatched unit is not bounded by it (pty-runtime §2.3 "Started units"), so `WRITE_STALL` ends its input's `sending` |
| Key completion | `settle_before` on the completion's oneshot | `not_after` + 1 s; then `cancel()` decides |
| VT snapshot | `with_session_blocking` (op lock and the blocking task's join), and `Session::snapshot`'s own deadline | 250 ms; the snapshot 200 ms |
| Open, completion, `human_write`, `ready`, settle, timer jobs | `supervisor::record` (sup §2.10) | its budget; an unrecordable `ready` or `key` ends `Unrecorded`; the driver's jobs are tried again on the next tick |

No new deadline exception: the driver's tick is 10a1's `supervisor::tick_timer`.

### 2.9 Changes to Earlier Interfaces

**Changes to 10a1 interfaces** (sup §2.11; 10a1 part 2 implements them):

| Id | Change | Reason |
|---|---|---|
| D1 | `supervisor::record` takes a job with a value, `Job<R = ()> = Arc<dyn Fn(&mut WriteTx<'_>) -> Result<R, JobError> + Send + Sync>`, and returns `R` from the attempt whose commit the writer confirmed (`write_with` already returns it, `ctx.rs:190-196`) | the open job and `ready` must hand their committed result (attempt seq, basis, refusal) to the caller; a value set inside an attempt that later rolled back would let a submit follow an uncommitted `sending` |
| D2 | `LiveLaunch::with_session_blocking(deadline, f)`: takes the op lock by `deadline` as `with_session`, then runs `f: FnOnce(&Session) -> R + Send + 'static` on a blocking task holding the owned guard, its join awaited with `settle_before` until `deadline`; `f` bounds itself | `Session::snapshot` waits for the VT lock in a sleep loop and walks the grid (`session.rs:37-49`, `session.rs:699-710`), which `with_session`'s inline `f` may not do |
| D3 | The reservation job (sup §2.3 step 3) takes `queued: Option<QueuedPrompt { input_id, text: Content }>` from `spawn` and `resume` and appends its `input_created` after `launch_reserved`; `core::launch::LaunchInput` is unchanged | Claude's prompt goes to the PTY, not to argv or env, so it is daemon state, not launch-plan input; `resume --prompt` needs it as `spawn --prompt` does |
| D4 | Card expiry at exit moves from PR 13 to 10b's `SendExitStep` | 10b's `SupervisorHooks` opens the cards |

`TickStep` stays as it is: `SendTick` returns no job and forwards the tick to the driver (§2.7), which does the reads and session calls a synchronous `tick` cannot.

**PR 9 seams** (hook-ingest §2.4, §2.6; changed in 10b, since 10b supplies both implementations):

- P1. `IntakeProbe::facts(&self, launch, ordinal)` receives the receipt ordinal it is sampled with.
- P2. `IntakeProbe::finished(&self, launch, ordinal, marks: &[(u64, TurnMark)])` is called once per intake when its request ends, on every path (a drop guard beside the `IntakeTicket`), with the marks of the deliveries its job committed (its own, and in a binding job each promoted one), empty when nothing committed. `interpret` returns its `TurnMark` beside the `Interpretation`.
- P3. `HookEvent.name` becomes `Cow<'static, str>` (still unique per delivery and not starting with `bind`), for `card:<approval_id>`.

**Event store** (event-store §2.6; SB1): `proj_inputs` gains `attempt_seq` and `last_write_before`, set when the input enters `sending` (the transition's seq, and the launch's `last_write_seq` before it). `input_state_changed` from `sending` to `cancelled` (only `RecheckFailed` leads there) sets the launch's `last_write_seq` back to `last_write_before` when it still equals `attempt_seq`. Schema v3 adds the columns; rebuild derives them from the events.

## 3. Decisions

Numbered SB (send-side decision). "Public" marks owner-visible contracts.

**SB1. Hole b, a zero-byte attempt (A31).** Before, `sending` advanced `last_write_seq`, so after `RecheckFailed` the gate stayed `WriteAfterEvidence` until new evidence or `chaldea ready`. Example: on an idle CLI, `a` opens, the writer is busy past `DISPATCH_WAIT` (`Expired`) or `with_session` is `Busy`; `a` is `cancelled` with zero bytes, and `b` is never sent, since an idle CLI sends no `Stop`.

| Option | Gain | Loss | Reversibility | Cost |
|---|---|---|---|---|
| A. Keep, with an immediate notice on the next queued input | no store change | a new public attention cause (core, reducer, model); the owner must act on every transient refusal | additive | medium |
| **B. A zero-byte attempt leaves `last_write_seq` as it was** | `b` opens at once when the gate still holds; stale evidence stays closed through SB3's open-time basis check (a reply after the basis keeps `b` `queued`) | two projection columns and an apply rule, schema v3 | rebuild | medium, internal |

**Adopted: B** (mvp §4 PR 10 (b)'s second option: tell attempts from started writes, leave out only attempts known to have written zero bytes; intervening writes, stale evidence, and unknown outcomes still close). With SB3, the cases that need new evidence are exactly those whose evidence is stale. Internal; owner-visible effect: a transient refusal no longer blocks later sends. Changes it: a refusal cause that should keep later sends waiting.

**SB2. `readiness_evidence: none` withholds Claude's `SessionStart` (A37).** Adopted, as the model assumes (`kindAllowed`): `OwnerOnly` exists to stop automatic sends where the adapter's evidence is not trusted, and mvp §2.3 says "only `chaldea ready`". Alternative: `SessionStart` counts in every mode (mvp §2.7 lists it first) — a mode named "none" would still send at startup. No row uses `OwnerOnly` in the MVP; PR 13 may. Internal now; public with the PR 13/14 column.

**SB3. The gate's evidence basis (A9, X3).** Adopted: the basis is sampled at the evidence's own intake (§2.4's table), and the open job refuses a stale basis (position moved past it) as `WriteAfterEvidence`, so the input stays `queued` instead of being cancelled at dispatch. Alternatives: the settle basis at the `Stop`'s intake (a reply to a query the CLI sends after its turn would cancel every send; the model takes the basis at the completion, `ready(ev, stop)`); no staleness check at open (each send after an evidence-less reply is cancelled). Internal.

**SB4. Ack of an input still `sending` (A10, X4, A16, A17, B2, C4).** A hook can be taken in before the completion job commits. Adopted: the intake facts carry `Dispatch.written`, which the unit's completion sets only for `Written`, and an attributable hook acks a `sending` input only when it is set, applying `WriteCompleted` and `AckAttributed` in the hook's job.

| Option | Gain | Loss | Reversibility | Cost |
|---|---|---|---|---|
| **A. `written` in the intake facts** | `interpret` stays data-only; no candidate kept between jobs; `user_message.input_id` names only an `acked` input; a hook before the Return was written, or with an `Incomplete` write, acks nothing | a hook taken in between the writer's last write call and its completion callback (microseconds; the CLI must first run its hook process) acks nothing, so the input ends `unconfirmed` (L-6) | model change (§7) | small |
| B. The model's candidate: keep a match while `sending`, apply it after `Written`, drop it after `Incomplete`, ambiguous on a second match | as the model | the candidate must outlive the hook's job: a new annex or memory written from `interpret`, which hook-ingest §2.6 forbids | — | medium |
| C. Mark the candidate with `user_message.input_id` and apply it in the completion job | no new state | an `Incomplete` write or an exit leaves a `user_message` naming an input that ends `unconfirmed`; a second match overwrites the first | — | small |

B and C were weighed against the model's rule (an ack while `sending` does not survive `Incomplete`; a second match while `sending` is ambiguous); A keeps that rule's results, since nothing is acked before the outcome is known and a second hook finds the input already out of flight, and removes the candidate. **Public** in effect (which sends end `acked`); changes it: PR 13's captures narrowing `prompt_matches`, or a measured share of hooks inside L-6's window.

**SB5. The positional anchor and what counts against it (A35).** Adopted: the anchor is `Published.reference` (sup §2.3 step 5); `Human` and `Input` writes above it make the ack non-attributable; `Reply` writes do not. Examples: Codex answers its startup queries after publication (replies above the anchor) → still attributable; the owner attaches and types before the `UserPromptSubmit` → not attributable; a reply before publication is below the anchor either way. Reason: the prompt is in argv, not on the PTY, so a reply cannot join or displace it, contains no CR or LF (pty-runtime §2.5's reply table), and cannot be text a matching prompt contains (`check` rule 1 refuses ESC). Rejected: counting replies (`trace_finding_positional_vt_reply`: every Codex launch whose queries are answered after publication would leave its first prompt `unconfirmed`, closing the gate until the owner resolves it). Sends keep `ClassSet::ALL` (mvp §2.3). Public in effect.

**SB6. `ready`'s meaning and shape (C3). Public.** Adopted: §2.4 "Owner assertion", accepted only for the current, bound, `running` launch of an open thread, with optional `expected_launch_id` (the CLI pins it from `GET /api/threads/{thread_id}` as `resume` does, sup SD16), the basis sampled in the effect. Alternatives: no pin (an assertion about one CLI lands on its replacement after a resume); sampling in `prepare` (it has no launch id without a store read). Changes it: a client that needs to assert for a starting launch.

**SB7. The send recheck reads a mirror; keys read their own state (A33, mvp §2.10).** Adopted: the send recheck evaluates `gate` on the driver's last read (`MirrorView`), no older than the attempt; writes, modes, exit, and stop are checked by the writer itself at the dispatch boundary. A card key instead orders itself against hooks with the probe's counters (§2.6), because a key written to a closed dialog cannot be taken back, while a send whose gate a just-committed hook closed is attributed or left `unconfirmed` like one dispatched just before that hook (L-1). Alternatives: a store read on the writer thread (blocks every PTY write on the reader pool); refusing a send on any hook taken in during its dispatch (cancels sends for unrelated hooks, and a cancelled input is not resent); a card key checked against the mirror (a lagging mirror admits a key to a closed card or refuses one to a fresh card). Internal.

**SB8. Wire types and codes of §2.2. Public.** Adopted as listed; every result field always present (`payload.rs:3-4`). `send` returns `rejected` with its reason instead of a 400 so the PWA shows the input (PR 19) and an agent sees why; `origin` is a client claim, informational only. One `input_state` code for both input commands. Changes it: a client that must tell the two refusals apart (split the code, additive).

**SB9. `key`'s token set and result. Public.** Adopted: §2.6's tokens and `KeyOutcome` `written | incomplete | pending`; `pending` says only that no outcome was known at the reply (the writer's `cancel()` false means dispatched or already settled, `writer.rs:216-220`). Alternatives: `started` for that case (claims a dispatch the writer does not promise); raw bytes (a client could send ESC sequences the keypad does not need, and arrows would ignore DECCKM); a full key-name table now (unused before PR 19's keypad). Additive later.

**SB10. The decoder seam (PR 13/14 boundary).** Adopted: `SupervisorHooks` owns state effects and turn marks; `HookDecoder` only reads payloads; memory learns of a delivery through `IntakeProbe::finished` after its commit (§2.9 P2). Alternative: each adapter writes state events itself (attribution and gate rules duplicated per CLI). Internal.

**SB11. F5 to PR 11.** No 10b path resizes; attach is the first caller of `Session::resize`, and pty-runtime §2.2 leaves resizing during a unit to it.

**SB12. Timers on the monotonic clock.** Adopted for settle, gate timeout, ack window, and write stall: the driver keeps each start instant in `SendLaunch`; tests drive `Supervisor::tick(now)`. Alternative: store stamps (`recorded_at`) as `buffer::expire` — a forward step of the wall clock would settle a `Stop` early and open the gate, and the input rows carry no stamps (`rows.rs:73-86`). A timer starts up to one tick late; memory is lost only with the launch.

**SB13. Claude's `--prompt` (§2.2).** Adopted: a queued input at reservation, for `spawn` and `resume`. Alternative: keep 400 (Claude launches need a second command). Public: `spawn` and `resume` accept a Claude prompt. Changes it: none foreseen.

**SB14. Which turn a `Stop` ends (A36).** Hooks carry no turn id the daemon can rely on (hook-ingest §2.6 "Retries": intake order does not tell which submission a hook belongs to), so a late `Stop` of turn `P1` taken in after `P2`'s `UserPromptSubmit` would end `P2` and settle while it runs. Adopted: §2.4's ledger counts turn starts and ends since the latest evidence and settles only when ends cover starts. Alternatives: the latest delivery only (the late `Stop` qualifies); a decoder-supplied turn id (none of the four rows is known to carry one; PR 13/14 may add it and narrow L-9); no automatic settle (every send needs `ready`). Internal; limits L-9, L-10.

## 4. Obligation Inventory

| Id | 10b's answer |
|---|---|
| A6 | own commands: §2.2; PR10b-CL-17 |
| A7 | §2.5 step 3 (late ack); PR10b-CL-8 |
| A8 | §2.5; PR10b-CL-10 |
| A9 / X3 | §2.4; SB3; PR10b-CL-4 |
| A10 / X4 | §2.5; SB4; PR10b-CL-7 |
| A11 | §2.6; PR10b-CL-12 |
| A12 | §2.2 `not_bound`; PR10b-CL-14 |
| A14 | §2.3 step 3; PR10b-CL-3 |
| A15 | §2.7; PR10b-CL-15 |
| A16 | §2.5 step 3; SB4; PR10b-CL-8 |
| A17 | §2.5; PR10b-CL-10 |
| A18 | §2.5 step 1; PR10b-CL-9 |
| A19 | §2.6; PR10b-CL-12 |
| A20 | §2.6; PR10b-CL-12 |
| A23 | §2.3 "After exit"; PR10b-CL-6 |
| A31 | SB1; PR10b-CL-5; §7 |
| A33 | §2.3 step 5; SB7; PR10b-CL-5 |
| A34 | §2.5 `CardEvidence`; PR10b-CL-13 |
| A35 | SB5; PR10b-CL-11; §7 |
| A36 | §2.4 "Settle completion"; SB14; PR10b-CL-4 |
| A37 | SB2; PR10b-CL-4 |
| A38 | §7; PR10b-CL-21 |
| B2 | §2.5 `competing`, §2.3 step 3; PR10b-CL-10; §7 |
| B3 | §7; PR10b-CL-21 |
| C2 | §2.2, §2.3 steps 3–4; PR10b-CL-1, PR10b-CL-2 |
| C3 | §2.4; SB6; PR10b-CL-14 |
| C4 | §2.5 step 2; PR10b-CL-9 |
| C5 | PR10b-CL-2 |
| C8 | §2.2; PR10b-CL-1 |
| D1 | §2.5 "Intake facts"; PR10b-CL-7 |
| D3 | §2.4, §2.5; PR10b-CL-7 |
| E1 | §2.2; PR10b-CL-17 |
| E2 | §2.2 codes; PR10b-CL-17 |
| F3 | §2.3 steps 6–7; PR10b-CL-6 |
| F4 | §2.6; PR10b-CL-12 |
| F5 | PR 11 (SB11) |
| X2 | §2.3 step 5; PR10b-CL-6 |

**Handed on.** F5 to PR 11. Real-CLI hook configuration and decoders, approval correlation (mvp §2.10), session switches, and the readiness column of `GET /api/adapters`: PR 13, PR 14.

## 5. Purpose Layer and Plan Layer

### 5.1 Purpose Layer: Acceptance Criteria

Until PR 13 and PR 14 configure real CLIs' hooks, no real CLI binds (sup SD14, L-4), so on real CLIs 10b can show only the paths that need no hook: refusals, queuing, the gate timeout, keys, and the input commands. Rows marked "PR 13/14" are checked by FakeCli now and on real CLIs by those PRs. Windows runs every FakeCli check in CI.

| ID | Criterion (EARS) | Source | Scene | Observable result / failure | Real-CLI check |
|---|---|---|---|---|---|
| PR10b-P-1 | When a prompt is sent to a ready thread, the CLI shall record it once in its expected form, and the input shall end `acked`, except where input-encoders D9 applies (Codex on macOS takes an existing image path as its own image paste: one turn with that image, the input `unconfirmed`, mvp P-8). | mvp P-6, P-7, P-8; §2.3 | idle bound thread | Pass: one `UserPromptSubmit` equal to `expected_prompt`; `acked`. Fail: a second submit; `unconfirmed` with a matching hook. | PR 13/14 (Claude, Codex macOS; Windows by PR 15); FakeCli `submit` in CI |
| PR10b-P-2 | While the gate is closed, a sent prompt shall stay `queued` with zero bytes, and after 60 s the thread shall carry `input_gate_timeout`, with a VT snapshot when one could be taken. | mvp §2.3 | a CLI that never sends hooks | Pass: `queued`, no PTY byte, the cause (and snapshot). Fail: a write; no cause. | Now: `chaldea spawn --agent claude`, `chaldea send <t> hi`, read the thread after 61 s; FakeCli `stall` in CI |
| PR10b-P-3 | If `chaldea ready` is run for an unbound launch, then it shall be refused with `not_bound` and write nothing. | mvp §2.3; A12 | any real launch before PR 13/14 | Pass: 409 `not_bound`; no event. | Now |
| PR10b-P-4 | When the owner sends keys to a thread, they shall reach the CLI and be recorded as `human_write`, closing the gate. | mvp §2.3, P-12 | Claude's folder trust prompt, whose default choice is "No, exit" (mvp §2.7) | Pass: the prompt clears into the composer; `human_write { key }`. Fail: no effect; the CLI exits; no event. | Now: `chaldea spawn --agent claude` in an untrusted folder; read the thread's VT snapshot (P-2's path) for the number of the "Yes" choice; `chaldea key <t> '{"char":"<n>"}'`; read the snapshot again. Never `enter` first |
| PR10b-P-5 | If an input is not acknowledged within the ack window, then it shall become `unconfirmed` with attention and never be sent again. | mvp P-15 | a dialog over the composer | Pass: one write, `unconfirmed`. Fail: a second submit. | PR 13/14 |
| PR10b-P-6 | When the owner cancels a queued input or resolves an `unconfirmed` one, the input shall end `cancelled`, `acked` (by human), or `dismissed`, and its attention shall clear. | mvp §2.3 | — | Pass: the state and cleared cause. | Now (cancel); PR 13/14 (resolve) |
| PR10b-P-7 | No text a CLI prints shall open the gate or ack an input. | invariant 1 | FakeCli `banner` printing a prompt echo | Pass: the input stays as it was. | CI only |

### 5.2 Plan Layer: Claims

`supervisor::send::…` are daemon unit tests with `FakePty`, the in-process store, `FakeDecoder`, and pause points; `supervisor_e2e::…` use the native runtime and FakeCli (sup §6); `key::…` are `core` tests. Every claim is checked on `macos-latest` and `windows-latest`.

| ID | Claim (EARS) | Source | Test | Mutation it kills |
|---|---|---|---|---|
| PR10b-CL-1 | When `send` is given text over `MAX_PROMPT_BYTES`, it shall answer 400 `prompt_too_large` with no receipt or input; when `check` rejects the text, it shall create the input and reject it in one job with that reason; otherwise the input shall be `queued` for the current launch with the row's `pty_encoding` and `created_by` from the token. | C8, C2 | `supervisor::send::admission` (sizes `MAX_PROMPT_BYTES` and +1; `"a\x1bb"`; whitespace-only on each row; a Codex Windows text with a trailing space; an agent token's `created_by`) | size check after the input; rejection as 400; `encoding` from `first_prompt`; `created_by` from `origin` |
| PR10b-CL-2 | When an input is written, its unit shall be `[Bytes(text), Pause(submit_delay), Bytes(b"\r")]` from `encode` at write time over the stored text, and the fake child shall receive one write call per `Bytes` part with every escape sequence inside one call. | C5; input-encoders §2.3 | `supervisor::send::unit_shape` (FakePty write calls for Claude, Codex macOS, Codex Windows rows, multi-line Japanese and emoji; a 70 KiB Japanese text stored as a blob) | text and submit in one part; pause dropped; blob preview encoded |
| PR10b-CL-3 | If any one gate condition is missing (no evidence, a human write after it, a `Stop` inside the settle interval, an interrupt, DECSET 2004 off, unbound, `OwnerOnly` without `ready`, a counted write after the basis with no event, a started human unit not yet recorded), then the input shall stay `queued` with zero bytes. | A14; mvp CL-5 | `supervisor::send::each_condition_alone` (one case per condition from an open baseline; `FakeChild::writes()` empty) | a condition skipped; stale basis or unrecorded human unit ignored at open |
| PR10b-CL-4 | Readiness evidence shall be recorded only as §2.4 allows, with the basis sampled at its own intake and the turn set `idle` by its `status_changed`; a settle completion only for a `Stop` with `intake_reliable` and facts, with no hook taken in after it, ends covering starts since the latest evidence, at least `SETTLE` after its commit on the monotonic clock; under `OwnerOnly` no `session_start` evidence. | A9, X3, A36, A37, D3 | `supervisor::send::evidence_kinds` (each kind × mode; a retry hook, `intake_reliable: false`; facts missing; the turn `idle` after each); `supervisor::send::settle` (tick at `SETTLE` − 1 ms and `SETTLE`; the store clock stepped forward 1 h does not settle; a later raw hook; a later hook taken in and held before its job; a late `Stop` of the previous turn after the next `UserPromptSubmit`, then that turn's `Stop`; a second `Stop` on an `unknown` turn; a buffered `Stop` before and after the binding `SessionStart`; a card key `human_write` after the `Stop` still settles) | basis from the `Stop` for settle; settle after a later intake; latest-delivery rule without counts; `SessionStart` under `OwnerOnly`; evidence without `status_changed` |
| PR10b-CL-5 | When the writer rechecks an input, it shall evaluate `gate` on the mirror without the input's own attempt; if a hook committed and read before the recheck closed the gate, or the basis, epoch, or mode word moved, then the input shall be `cancelled` with zero bytes; after a zero-byte attempt the next input shall open when the gate otherwise holds, and stay `queued` when the basis is stale. | A33, A31, X2 | `supervisor::send::recheck_view` (writer parked at `ProbePoint::RecheckEntered`; a `PromptSubmitted` committed and mirrored → `cancelled`; a DSR query answered after the open job and before the input dequeues → `WriteSinceBasis`; DECCKM toggled while parked → `Recheck("modes_changed")`; without such a change → written); `supervisor::send::zero_byte_attempt` (design-choice check of SB1: `Expired` on an idle open gate, then `b` → written; a reply after the basis, then `b` → `queued`) | recheck on the projected view with the own attempt (always closed); own attempt counted as in flight; `last_write_seq` not restored, or restored after a human write |
| PR10b-CL-6 | `Written` shall move the input to `sent`, every `NotStarted` and refused submission to `cancelled`, `Incomplete` and a dispatched unit with no completion after `WRITE_STALL` to `unconfirmed`; a completion after `launch_exited` or after the input settled, and a sealed record already applied, shall change nothing; an unrecorded completion shall be applied on a later tick. | F3, A23, X2; pty-runtime D5 | `supervisor::send::outcomes` (`FakePty` `fail_writes_after`, `hold_writes` with close, `Busy` and `Gone` from `with_session`); `supervisor::send::write_stall` (`hold_writes` past `WRITE_STALL` by the injected tick, then released); `supervisor::send::completion_retry` (store writer held past the record budget, then released); `supervisor::send::after_exit` (completion held past the exit job; `ChildExited` before it) | `Incomplete` as `RecheckFailed`; completion applied after exit; stall left `sending`; unrecorded completion dropped |
| PR10b-CL-7 | `SupervisorProbe` shall never wait, return `None` for an unobservable launch or a contended `SendLaunch`, and the facts of §2.5 otherwise; a hook with missing facts or `intake_reliable: false` shall ack nothing. | D1, D3, A10, X4 | `supervisor::send::probe` (each observation; the mutex held by the test → `None` at once, counters still advanced); `supervisor::send::undispatched_match` (intake before the dispatch → `user_message` without `input_id`, input unchanged) | `Unobservable` as zeros; blocking lock |
| PR10b-CL-8 | When an attributable hook is taken in after its input's unit was `Written` and before the completion job, the input shall be `acked` in the hook's job; when one is taken in before the unit finished, or the unit ends `Incomplete`, the input shall not be acked by it; when one arrives for an `unconfirmed` input of the live launch, it shall be `acked`. | A16, A7 | `supervisor::send::ack_while_sending` (completion job held; hook after `Written` → `acked`; hook while parked at `PauseEntered` → not acked, then `sent`; `fail_writes_after` on the submit part → `unconfirmed`, no `input_id`); `supervisor::send::late_ack` (window expired, then the hook) | `written` ignored; ack applied after `Incomplete`; late ack dropped |
| PR10b-CL-9 | If a prompt matches no in-flight input, or two, or an earlier input left flight without a hook ack and matches it, then no input shall be acked. | A18, C4 | `supervisor::send::matcher` (attribution unit over constructed rows: zero, one, two in-flight matches, since one launch holds one in flight; two blobs with the same preview and different tails); `supervisor::send::earlier_match` (an `acked`-by-human and a `dismissed` input with the same text, then a send and its hook → `unconfirmed`; a different text → `acked`) | first match taken; resolved inputs not searched; preview compared |
| PR10b-CL-10 | If a counted write other than the input's started after its dispatch and before the hook's intake (a `key`, an attach unit queued during the input's unit, a reply), or a gate-closing human write followed the last turn start before the attempt, then the hook shall not ack the input. | A8, A17, B2 | `supervisor::send::key_after_send`; `supervisor::send::human_during_unit` (attach unit submitted at `PauseEntered`); `supervisor::send::competing_human` (attach write, `ready`, send, delayed matching hook → `unconfirmed`; control without the attach → `acked`; the attach's `human_write` job held: the send stays `queued` until it commits) | positions compared with `<=`; `competing` ignored; unrecorded human unit ignored |
| PR10b-CL-11 | A positional prompt's hook shall be attributable exactly when no `Human` or `Input` write is above `Published.reference` at its intake; `Reply` writes shall not count. | A35 | `supervisor::send::positional_anchor` (`FakeScript.startup_output` with a DSR answered after publication → `acked`; an attach write after publication → `unconfirmed`; facts `Unobservable` → `unconfirmed`) | replies counted; anchor taken at the start commit |
| PR10b-CL-12 | When an `Attach`, `Key`, or `CardKey` unit is `Written` or `Incomplete`, exactly one `human_write` with its origin shall be recorded, whether the key's effect or the driver records it; after a raw key only evidence of a later turn or `ready` shall reopen the gate, however late a same-turn evidence is committed; after a card key the turn's next evidence shall reopen it. | F4, A11, A19, A20 | `supervisor::send::human_writes` (each origin and outcome; a `pending` key whose completion the driver records; both recorders racing → one event); `supervisor::send::same_turn_evidence` (a settle completion of the `key`'s turn committed after it: closed; after the next turn start: open); `supervisor::send::card_key_reopens` | `Incomplete` dropped; `card_key` as `key`; idempotency key per call |
| PR10b-CL-13 | `ApprovalsAllResolved` shall be fed only when the resolved card was the last open card of its launch, matched by `approval_id`; one turn boundary shall close every open card; cards shall expire at exit and at restart; a card key shall be refused `card_closed` or `key_not_written` with zero bytes when its card closed or a hook was taken in after its read. | A34, A19; mvp §2.10 | `supervisor::send::cards` (two cards, resolve one then the other; one `Stop` superseding two; a resolution after `Stop` while the next turn's card is open; exit and restart expiry); `supervisor::send::card_key_order` (the card superseded and committed but not mirrored, recheck parked → refused; a hook taken in during the key's read → refused; a fresh card right after `ApprovalOpened` → written) | feed on any resolution; duplicate event names; card key checked against the mirror |
| PR10b-CL-14 | `ready` shall record `owner_asserted` only for the current, bound, `running` launch of an open thread, with the basis sampled in its effect, and answer `not_bound`, `no_live_launch`, `launch_changed`, or `thread_closed` with nothing written otherwise. | A12, C3 | `supervisor::send::ready` (each refusal, a close racing the effect; success after a VT reply answered before `ready`, then a send opens, and the stored basis equals the reply's position) | unbound accepted; basis 0 |
| PR10b-CL-15 | A `queued` input shall raise `input_gate_timeout` once `GATE_TIMEOUT` after the driver first read it, with a `vt_snapshot_taken` when one was taken; a `sent` input shall become `unconfirmed` once `ACK_WINDOW` old, a positional one only while no startup cause is active. | A15; mvp §2.3 | `supervisor::send::timers` (tick at the bound − 1 ms and at it; VT lock held throughout → the cause raised once without a snapshot; failed VT → bare); `supervisor_e2e::stall_times_out` (FakeCli `stall`, in process, the tick driven with an injected instant) | timeout re-raised; timeout withheld for a snapshot; window while a startup cause is active |
| PR10b-CL-16 | `key` shall write exactly the encoded tokens and answer `written`, `incomplete`, `pending`, `key_not_written`, `card_closed`, or `no_live_launch` as §2.6 states. | §2.6; SB9 | `key::encode` (each token, DECCKM on and off, 0 and 33 tokens refused); `supervisor::send::key_outcomes` (held writes for `pending`; held writer for `key_not_written`; DECCKM toggled between encode and dispatch → `key_not_written`; `Busy`; a replay with the same `command_id`) | arrows ignoring DECCKM; `started` reported for a settled unit |
| PR10b-CL-17 | The five handlers shall be registered; two concurrent requests with one `command_id` shall cause one effect; every new code shall be in `handler_code`; no success type shall have a top-level `code`. | A6, E1, E2 | `supervisor::send::duplicates_write_once` (per command, through the mounted routes); `handler_code` exhaustive match; `supervisor::commands::success_schemas_have_no_code` extended | a handler not registered; receipt dedup skipped |
| PR10b-CL-18 | `input_cancel` shall act only on `queued` and `input_resolve` only on `unconfirmed` inputs; an agent shall cancel only inputs it created, getting 403 for another creator's input on a thread it can read and 404 for one it cannot; a cancel racing the open job shall leave exactly one of them in effect. | mvp §2.3, §2.5 | `supervisor::send::input_commands` (each state; agent token on an owner-created input of its own thread → 403; on another thread's input → 404; cancel against a held open job) | creator check removed; cancel of a `sending` input |
| PR10b-CL-19 | A Claude `spawn --prompt` or `resume --prompt` shall create a `queued` input for the launch in the reservation job and write it when the gate first opens. | §2.9 D3; SB13 | `supervisor::send::claude_first_prompt` (spawn and resume) | input created after `launch_started`; resume prompt still 400 |
| PR10b-CL-20 | No byte the CLI prints shall change thread or input state, open the gate, or ack an input (DECSET 2004 may change only the VT mode the gate reads). | invariant 1; mvp CL-1 | `supervisor_e2e::banner_never_acks` (FakeCli `banner` printing a prompt echo and hook-like text, on a bound launch with evidence and a `sent` input: thread and input rows unchanged until the window expires it, no `user_message`) | any output reader feeding the driver |
| PR10b-CL-21 | The guards of §7 shall be in `thread_input.qnt` with their traces in the same PR, and deleting a guard's daemon site shall fail its test. | A38, B3 | CI `specs` job and `quint_traces.rs`; the guard-to-test table of §7 | a guard without its trace |

### 5.3 Accepted Limits

| ID | Limit | Decided by | Why it does not defeat the purpose |
|---|---|---|---|
| L-1 | A hook committed but not yet read into the mirror when a send's recheck runs does not stop its dispatch. | SB7 | writes, modes, stop, and exit are checked exactly by the writer; hook facts are already asynchronous with the CLI (mvp §2.3 "Residual constraint"), so such a hook is the same as one arriving just after the dispatch: the input is then attributed or left `unconfirmed`, never resent. Card keys do not have this limit (§2.6). |
| L-2 | After a gate-closing human write with no later turn start, a send opened by `ready` ends `unconfirmed` even when the CLI submitted it. | mvp §2.3 "unknown history"; B2 | the owner sees `unconfirmed` and resolves it; the alternative risks acking a human's submission as the send. |
| L-3 | A write between `chaldea ready` and its basis sample is inside the basis. | SB6 | only the owner can write such keys (`key` and attach are owner-only), and the assertion is theirs. |
| L-4 | Until PR 13 and PR 14 configure real CLIs' hooks, no real CLI binds, so `ready` is refused and every `send` waits and times out. | sup SD14, L-4 | the path is tested end to end with FakeCli and `FakeDecoder`; keys work now. |
| L-5 | Claude's matching accepts every subset of its known transforms until PR 13's captures. | input-encoders D4 | a lenient match acks only an input whose text equals the prompt up to those transforms; two matches ack none. |
| L-6 | A hook taken in between the input unit's last write call and its completion callback acks nothing, so that input ends `unconfirmed`. | SB4 | the callback runs right after the write returns on the writer thread, before the CLI can run its hook process and reach the daemon; the owner resolves the rare case; nothing is resent. |
| L-7 | `SETTLE` (2 s) is not measured on real CLIs. | SB12; mvp D8 | internal default, an adapter row value; PR 12 measures it (P-7); a user `Stop` hook that blocks longer is mvp D8's known race. |
| L-8 | That ConPTY delivers `key`'s VT bytes (arrows, Escape) to Codex on Windows as the same keys is not run on a real host. | §2.6; OpenConsole's input parser (input-encoders §2.5 rule 2) | FakePty checks the bytes; a key that does not arrive leaves the screen unchanged and is visible to the owner, who uses attach (PR 11); PR 15 runs it on the real host. |
| L-9 | A lost `Stop` keeps ends below starts, so the launch's gate opens only through `ready` (or a later evidence kind) until then. | SB14 | ambiguity leaves the gate closed (mvp §2.3 "Residual constraint" (c)); the gate timeout shows the owner the waiting input after 60 s. |
| L-10 | A `Stop` repeated inside one turn (a user `Stop` hook that blocks, mvp D8) and taken in late after the next turn's `UserPromptSubmit` balances the count and can settle while that turn runs. | SB14; mvp D8 | it needs mvp D8's known race and a late intake together; the send it lets through follows the match and attribution rules and is never resent. |

## 6. Tests

Unit tests drive `FakePty` (`hold_writes`, `wait_entered`, `fail_writes_after`, `FakeScript.startup_output`, `ProbePoint::RecheckEntered`, `PauseEntered`), the store, `Supervisor::tick(now)` with injected instants, and `FakeDecoder`, which maps FakeCli payloads (`hook_event_name`, `prompt`, `tool_use_id`, `tool_name`) to `HookFact`s. Integration tests run the daemon with FakeCli and the native runtime as `crates/cli/tests/hook_e2e.rs` does, with `FakeDecoder` installed through `SupervisorConfig`.

**FakeCli `stall` mode** (new): prints a fixed screen and sleeps until `--max-life-secs`, sending no hook.

**FakeCli `submit` mode** (new): enables DECSET 2004, reads its stdin, keeps a bracketed paste whole (line breaks inside it included), and on a CR outside a paste sends one `UserPromptSubmit` whose `prompt` is the text read (CR LF and CR turned into LF, as Claude records it), then a `Stop`. P-1's FakeCli check and CL-2's end-to-end case use it, so the prompt the hook reports is the one the PTY delivered.

**Host speed.** Timers run on instants passed to `Supervisor::tick(now)`; writer races are held with `wait_entered`, `park`, and pause points; intake facts are taken while the test holds the writer at a probe point; hook commits are held at hook-ingest's test points (`after_intake`); no test sleeps in place of an event. Native tests read the published port (`client::discover`).

**Windows CI.** Every `supervisor::send::`, `key::`, and `supervisor_e2e::` test runs on `windows-latest` (`scripts/windows-tests.ps1 -Lane Cargo`); the Codex Windows row is exercised on both runners through `HostOs::Windows`.

**Mutation check.** Each mutation of §5.2 is applied by hand before merging and the named test seen failing.

**Completion commands**: sup §6's.

## 7. Quint

Changes to `specs/thread_input.qnt` (in the implementation PR, with `pnpm run traces:write`):

1. **Positions per class; `counted` as policy; the positional exception (B3, A35).** The writer list keeps its classes; `LaunchConfig` gains `counted` (all three classes for every adapter); the basis, `open`'s staleness check, `recheckPasses`, and `noOtherWriteAfter` count only writes of classes in `counted`, and for a positional input `noOtherWriteAfter` ignores `WReply`. `open` requires the counted writer length to equal `gp.basis` (SB3). Traces: `trace_finding_positional_vt_reply` becomes `trace_positional_vt_reply_acked` (`expect` `Acked`); new `trace_positional_attach_not_attributable`; new `trace_stale_basis_keeps_queued` (`vtReply` after `ready`: `open(1)` disabled, `Queued`, zero writes); new `trace_reply_after_dispatch_not_attributable` (a non-positional input with a `vtReply` after its dispatch → `Unconfirmed`, where the positional trace with the same reply is `Acked`).
2. **The written-at-intake ack (A16, SB4).** `Candidate` is removed. A new action `writerDone(id, ok)` sets `Input.writeDone` for a dispatched input; `outcome(id)` records `writeDone`'s result (`sent` or `unconfirmed`). `AckRecord` gains `writtenAtIntake`; `applyPrompt` on a `Sending` input acks it (`WriteCompleted`, then `AckAttributed`) only when the record is attributable, and otherwise leaves it unchanged. Traces: `trace_ack_while_sending` becomes `dispatch`, `writerDone(1, true)`, `prompt(Set(1))` → `Acked`, then `outcome(1)` changes nothing; new `trace_prompt_before_write_done` (`prompt` before `writerDone` → `Sending`, then `Sent`, then `windowExpired` → `Unconfirmed`); new `trace_incomplete_never_acked` (`writerDone(1, false)`, `prompt` → not acked, `outcome(1)` → `Unconfirmed`).
3. **Competing submission and earlier match (B2, C4).** `AckRecord` gains `noCompetingSubmission` (`open` records `gp.lastHumanWrite > gp.lastTurnStart` on the input; `intakeFacts` copies it) and `noEarlierMatch` (no input of the launch that left flight without a hook ack is in the hook's match set). New invariants `noAckOverCompetingSubmission` and `noAckOverEarlierMatch`, each stated on its own field; `ackOnlyWhenAttributable` enumerates every field of the extended record. Traces: `trace_competing_human_submission` (`humanWrite(Attach)`, `ownerReady`, `send`, `open`, `dispatch`, `writerDone`, `outcome`, `prompt(Set(1))` → `Unconfirmed`) with `trace_send_ack` as the clean control; `trace_earlier_dismissed_match_not_acked` (input 1 `Unconfirmed` → `resolveInput(1, false)`, input 2 sent and its matching `prompt` → `Unconfirmed`) with a control whose input 1 text differs.
4. **Hole b, option B (A31, SB1).** A failing `dispatch` sets `gp.lastWrite` back to the input's `lastWriteBefore` when it still equals its `attemptSeq`. New action `dispatchExpired(id)` (a zero-byte refusal with no state cause). `trace_hole_b_recheck_keeps_gate_closed` becomes `trace_zero_byte_attempt_reopens` (`dispatchExpired(1)` → `Cancelled`, then `send(2)`, `open(2)` → `Sending`); `trace_refused_recheck_reply_after_basis` keeps its `Cancelled` and gains a second input that stays `Queued`.
5. **Unchanged:** `kindAllowed` (SB2); `cardResolve` (A34 already by identity); `ready(ev, stop)` and `stopOfTurn` (A36: the model names a `Stop`'s turn; the daemon realizes it by counting, SB14, tested by PR10b-CL-4); `outcome` with an unknown result for the write stall.

**Guard-to-test table** (daemon guards the replay cannot see; deleting the site fails the test):

| Model guard | Daemon site | Test |
|---|---|---|
| `open`: `gate`, basis current by counted classes | `open_job` (§2.3 step 3) | PR10b-CL-3 |
| `dispatch`: `recheckView`; restore on failure | the recheck (§2.3 step 5); the projection's apply rule (§2.9) | PR10b-CL-5 |
| `attributable` (each field) | §2.5 attribution | PR10b-CL-7–CL-11 |
| `ready`, `ownerReady`, `kindAllowed`, `stopBacks` | §2.4, the turn ledger | PR10b-CL-4, CL-14 |
| `humanWrite` (card key open) | `key`'s read and recheck | PR10b-CL-13 |
| `cardResolve` | `SupervisorHooks` `CardEvidence` | PR10b-CL-13 |
| `windowExpired`, `gateTimeout` | the driver's timers | PR10b-CL-15 |

## 8. Proposed Canonical-Doc Changes

Not applied here.

- **mvp §2.3 "Gate"**: a counted write after the evidence's basis, or a started human write not yet recorded, keeps the gate closed (the input stays `queued`); a zero-byte attempt leaves the gate as it was (hole b option B, SB1); a settle completion needs the launch's turn ends to cover its starts since the latest evidence (SB14). **"Ack"**: the positional prompt is anchored at the writer position sampled at publication, and only `attach`, `key`, and input writes above it count (SB5); an input still `sending` is acked only by a hook taken in after its unit was written, in that hook's job (SB4); an earlier input resolved by the owner (`acked` by human, `dismissed`) whose text matches counts as a competing submission; a dispatched unit with no completion after 30 s makes its input `unconfirmed`. **`ready`**: accepted for a `running` bound launch of an open thread, optional `expected_launch_id`.
- **mvp §2.4 / §2.5**: the args, results, and codes of §2.2 (`no_live_launch`, `not_bound`, `card_closed`, `key_not_written`, `input_state`); `send` returns a rejected input instead of refusing; the `key` token set and `KeyOutcome` `written | incomplete | pending`; Claude's `spawn --prompt` and `resume --prompt` are a queued input; `input_cancel` answers 403 for another creator's input on a readable thread.
- **mvp §2.7**: `readiness_evidence: none` withholds Claude's `SessionStart` too (SB2).
- **mvp §2.10**: `launch_exited`'s card expiry is applied by 10b's exit step, at restart too.
- **mvp §4 PR 10 bullet**: 10b decides hole b (option B), the competing-submission condition, and the late-`Stop` rule as above; F5 moves to PR 11.
- **formal-specs §1**: the `none` assumption is decided (SB2). **§3**: hole b owner "PR 10b, option B"; the competing-submission condition, the earlier-match condition, and the positional finding are resolved by §7 here. **§2**: `Candidate` replaced by `writeDone` and `writerDone`. **§6**: PR 10's bullet points to §7 here.
- **hook-ingest §2.4, §2.6**: `SupervisorProbe` replaces the default probe; `IntakeFacts` content is `IntakeFactsV1`; `facts` takes the receipt ordinal; `IntakeProbe::finished` and `TurnMark`; `HookEvent.name` is a `Cow<'static, str>`. **§4 PR 10 row**: `SupervisorHooks` wraps `HookDecoder`s; PR 13 and PR 14 supply decoders and hook configuration, not adapters.
- **event-store §2.6**: `proj_inputs.attempt_seq`, `last_write_before`, and the `sending → cancelled` apply rule (schema v3).
- **pty-runtime §2.3 "How the daemon uses it"**: the send recheck reads the supervisor's mirror; card keys order against hook intake; the positional counted classes (SB5); a dispatched unit's input becomes `unconfirmed` after `WRITE_STALL`; F5 is PR 11's.
- **input-encoders §4 PR 10 row**: done as §2.3; `core::input::key` added.
- **supervisor.md §2.11**: D1–D4 of §2.9.
- **http-surface §2.8 "Handler replies"**: the five codes.
