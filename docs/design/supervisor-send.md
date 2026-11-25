# Design: Supervisor, Send and Ack Side (MVP PR 10b)

Status: design. Scope is the send and ack half of the PR 10 row of `docs/mvp-design.md` §4 (cited as "mvp §N"), called **PR 10b**: the `send`, `ready`, `key`, `input_resolve`, and `input_cancel` commands; the readiness gate and its evidence; the writer integration (`encode` at write time, the `WriteUnit`, the recheck, the outcome mapping, `human_write`); ack attribution with `prompt_matches`; the real `IntakeProbe`; input-side attention and approval cards; and the send path's guards in `specs/thread_input.qnt`. It builds on the 10a1 design, `docs/design/supervisor.md` (cited as "sup §N"), and uses only its §2.11 interfaces plus the one addition of §2.9 here.

**What 10b decides and leaves.** 10b owns the daemon's `supervisor::send` module, a `core` key encoder, and the hook adapter wrapper that turns decoded hooks into state. It leaves to PR 13 and PR 14 the decoders that read real Claude and Codex payloads (until then every real hook stays `raw_unknown`, as today), the readiness and resume columns of `GET /api/adapters`, session switches, and the approval correlation rule of mvp §2.10; to PR 11 attach and resizing (F5); to PR 18–19 the UI; to PR 10a2 crash recovery.

**Commitments from earlier PRs this design keeps.**

1. Thread and input state move only through explicit events: hooks, the writer's completion records, and the supervisor's own commands (invariant 1). Nothing reads PTY text; the VT is read only for DECSET 2004 and a diagnostic snapshot (mvp §2.2).
2. No input is resent automatically: nothing leads from `sending`, `sent`, or `unconfirmed` back to `queued` or `sending` (`step`, `crates/core/src/input.rs:178-211`).
3. `reduce`, `step`, `gate`, and `blocks_gate` keep their rules; `GateView` and every public payload keep their fields. New public names are listed in §1 and flagged in §3.
4. `HookAdapter::interpret` stays data-only on the store writer (hook-ingest §2.6): it reads through its `ReadTx` and its `AcceptedHook`, never supervisor memory.
5. Every wait has a deadline through `deadline::settle_before`, `LiveLaunch::with_session`'s deadline, or `supervisor::record`'s policy (§2.8).

Sources read for this document:

| Source | Revision | Read for |
|---|---|---|
| This repository | `6581c36` | `AGENTS.md`; `docs/architecture.md`; `docs/mvp-design.md` §2.1–§2.11, §4, §6, §8; `docs/design/{supervisor,input-encoders,pty-runtime,hook-ingest,http-surface,formal-specs,deadlines,event-store}.md`; `crates/core/src/{state,input,adapter,payload,auth,http,ids}.rs`, `crates/core/src/input/encode.rs`; `crates/pty-runtime/src/{session,writer,origin,snapshot}.rs`; `crates/daemon/src/hook/{adapter,intake}.rs`, `crates/daemon/src/http/commands.rs`, `crates/daemon/src/store/{readers,rows}.rs`, `crates/daemon/src/deadline.rs`; `crates/testkit/src/fake_cli/hooks.rs`; `specs/thread_input.qnt`, `specs/thread_input_traces.qnt` |
| PR 10 split | orchestrator note, 2026-11-25 | the 10a / 10b cut and the 10b obligation ids |

## 1. Goal and Dependencies

**Goal.** A prompt given to `chaldea send` is written to the CLI's PTY exactly once, as one encoded unit, only while the gate holds and the writer's recheck confirms it, or is shown `rejected` or `cancelled` with zero bytes; every written input ends `acked` only on a `UserPromptSubmit` that matches its text and can be attributed to its write, otherwise `unconfirmed` and never resent; and every human write, card, and timer reaches the store as an explicit event.

**Depends on** 10a1 (sup §2.11: `LiveLaunch::with_session`, `Supervisor::observe`, `Published.reference`, `ExitStep`, `TickStep`, `ExitFacts.sealed`, `supervisor::record`, `EffectOutcome`), PR 5 (`check`, `encode`, `prompt_matches`, `capabilities`), PR 9 (`HookAdapter`, `IntakeProbe`, `AcceptedHook`), PR 2 (`Session::submit`, `Guard`, `WriteContext`).

**Files and seams** (every name marked new does not exist at `6581c36` and is not a 10a1 name):

```
crates/core/src/adapter.rs        # ReadinessMode, Capabilities::readiness_mode, evidence_allowed (new)
crates/core/src/input/key.rs      # new: KeyToken, encode_keys, MAX_KEYS
crates/core/src/command.rs        # SendArgs, SendResult, ReadyArgs, ReadyResult, KeyArgs, KeyResult, KeyOutcome,
                                  #   InputResolveArgs, InputCancelArgs, InputCommandResult (new)
crates/core/src/http.rs           # ErrorCode::{NoLiveLaunch, NotBound, CardClosed, KeyNotWritten, InputState} (new)
crates/core/tests/contracts_export.rs, golden.rs # the new wire types
crates/daemon/src/supervisor/send/ # new: mod (SendMap, SendLaunch, Dispatch, MirrorView), driver (Driver, open_job),
                                  #   completion, hooks (SupervisorHooks, HookDecoder, HookFact, NoDecoder),
                                  #   probe (SupervisorProbe, IntakeFactsV1), attribution, exit (SendExitStep),
                                  #   tick (SendTick), commands (the five handlers)
crates/daemon/src/store/readers.rs # ReadTx::settle_candidate (new)
crates/daemon/src/supervisor/      # 10a1's reservation job: an optional queued first input (§2.9)
crates/cli/src/agent_cmd.rs       # send, ready, key, input resolve, input cancel
crates/testkit/src/fake_cli/stall.rs # new FakeCli mode `stall`
crates/testkit/src/fake_decoder.rs # new: FakeDecoder, a HookDecoder over FakeCli payloads (tests only)
specs/thread_input.qnt, thread_input_traces.qnt, traces/ # §7, in the implementation PR
```

**New identifiers** besides those above: `SendMap`, `SendLaunch`, `Dispatch` (fields `input_id`, `position`, `competing`, `last_write_before`), `MirrorView` (fields `as_of`, `view`, `in_flight`, `open_cards`), `Driver`, `open_job`, `SupervisorHooks`, `HookDecoder`, `HookFact` and its variants, `NoDecoder`, `FakeDecoder`, `SupervisorProbe`, `IntakeFactsV1` (fields `v`, `reference`, `last_started`, `inflight`), `SendExitStep`, `SendTick`, `ReadinessMode` and its variants `StopSettle`, `PostDecision`, `OwnerOnly`, `Capabilities::readiness_mode`, `evidence_allowed`, `KeyToken` and its names (`enter`, `escape`, `tab`, `backspace`, `ctrl_c`, `up`, `down`, `right`, `left`, `char`), `encode_keys`, `MAX_KEYS`, `KeyOutcome` and its values (`written`, `incomplete`, `started`), `LaunchInput.queued_prompt`, the wire types of §2.2, the error codes `no_live_launch`, `not_bound`, `card_closed`, `key_not_written`, `input_state`, the constants `DISPATCH_WAIT`, `ACK_WINDOW`, `GATE_TIMEOUT`, `SETTLE`, `ReadTx::settle_candidate`, FakeCli mode `stall`, the Quint names of §7, and every test name in §5.2.

## 2. Design

### 2.1 Rules

1. **The store decides, memory mirrors.** Whether an input opens, is acked, or times out is decided in one store job from the projection, the hook's intake facts, and the store clock. Memory (`SendLaunch`) holds only what a non-blocking caller needs and the store cannot give in time: the writer recheck's view, the in-flight dispatch's writer position for the intake probe. It dies with the launch.
2. **Positions, not wall time, order writes against hooks.** Every attribution and staleness test compares writer positions (`last_started` per `OriginClass`, `WriteContext.seq`, `Published.reference`), as pty-runtime §2.3 "Ack attribution" states.
3. **Zero bytes or unknown, never guessed.** `NotStarted` and every refused submission are zero bytes (`RecheckFailed`); `Incomplete` is unknown (`WriteOutcomeUnknown`), pty-runtime D5.
4. **One in-flight input per launch.** Only the open job moves an input to `sending`, and it refuses while another input of the launch is in flight (`gate`'s `InputInFlight`).

### 2.2 Commands

Registered in `Built::commands` beside 10a1's four (E1). Each `prepare` parses `args` (400 `bad_args`), builds an intent from data; each intent checks the target against the token's grant first (`deny_for`: 404, as sup §2.2 "Target scope first").

| Command | Who (`auth.rs:126-175`) | Args | Intent | Effect | Success |
|---|---|---|---|---|---|
| `send` | owner; agent self+ | `SendArgs { thread_id, text, origin? }` (`origin` default `cli`) | `thread_closed`; current launch `starting` or `running`, else `no_live_launch`; `input_created { launch_id: current, initial_state: queued, encoding: caps.pty_encoding, origin, created_by }`; `check(agent, pty_encoding, Send, text)` → `Err(r)`: `queued → rejected(r)` in the same job. `Completed` | none | `SendResult { input_id, state, reject_reason }` (`state` `queued` or `rejected`; `reject_reason` null unless rejected) |
| `ready` | owner | `ReadyArgs { thread_id, expected_launch_id? }` | `thread_closed`; `launch_changed`; current launch `running`, else `no_live_launch`; unbound → `not_bound` (A12). `Pending(L)` | §2.4 "Owner assertion" | `ReadyResult { launch_id }` |
| `key` | owner | `KeyArgs { thread_id, launch_id, keys: [KeyToken], approval_id? }` | `thread_closed`; `launch_changed`; `running`, else `no_live_launch`; with `approval_id`: the card open and of `L`, else `card_closed`. `Pending(L, card)` | §2.6 | `KeyResult { outcome, bytes_written }` |
| `input_resolve` | owner | `InputResolveArgs { input_id, delivered }` | input missing or unreadable → 404; not `unconfirmed` → `input_state`; `Resolved { delivered }` (`acked` by human or `dismissed`), clearing `input_unconfirmed`. `Completed` | none | `InputCommandResult { input_id, state }` |
| `input_cancel` | owner; agent for inputs it created (`CreatedBySelf`) | `InputCancelArgs { input_id }` | 404 as above; not `queued` → `input_state`; `Cancelled`, clearing `input_gate_timeout`. `Completed` | none | `InputCommandResult { input_id, state }` |

`send`'s admission (C8): text over `MAX_PROMPT_BYTES` is 400 `prompt_too_large` in `prepare`, before any receipt or input. A rejected input is created and rejected in one job so that its reason is public (`input_state_changed.reason`), as input-encoders §4's PR 10 row asks. The deadlines are the server's short 5 s (mvp §2.5); `key`'s effect fits it (§2.6).

**Codes** (new `ErrorCode` variants, each in `handler_code` at one status, E2; success types carry no top-level `code`, http-surface.md:544, asserted per type):

| Code (new) | Status | When | Can the work happen later? |
|---|---|---|---|
| `no_live_launch` | 409 | the thread's current launch is not `starting`/`running` (send) or not `running` (ready, key), or the key's effect finds the session gone | no |
| `not_bound` | 409 | `ready` for a launch with no bound session (A12) | no |
| `card_closed` | 409 | a card key whose card is not open in `L` at the intent, or refused at the write boundary (mvp §2.10) | no |
| `key_not_written` | 409 | the key unit was refused before its first byte (zero bytes) | no |
| `input_state` | 409 | `input_resolve` of an input not `unconfirmed`, `input_cancel` of one not `queued` | no |

`thread_closed`, `launch_changed`, and `prompt_too_large` are 10a1's (sup §2.2).

**Claude's first prompt.** `spawn --prompt` for Claude (400 `bad_args` in 10a1) becomes a `send` made at reservation: `prepare` runs the size check and `check(Claude, BracketedPaste, Send, text)` (400 `prompt_too_large`, `prompt_rejected`), and the reservation job appends `input_created { launch_id: L, initial_state: queued, encoding: bracketed_paste }` after `launch_reserved` (§2.9). It is written when the gate first opens; `SpawnResult.input_id` names it.

### 2.3 The Send Path

`Driver` is one supervisor task started in `SupervisorComponents::attach`. It wakes on `Store::subscribe` notifications naming a thread stream, on every tick, and on its own completions; per wake-up it handles, for each notified thread, its current launch `L` when `Supervisor::live(thread, L)` returns an entry:

1. **Read.** One read transaction (`ReadTx::launch`, `ReadTx::thread`, `ReadTx::inputs`, `ReadTx::open_approvals`) on a blocking task, awaited with `settle_before` until now + 1 s; not in time → skip `L` until the next wake-up. The result refreshes `L`'s `MirrorView` (`as_of` = the thread stream head of that read; `view`; `in_flight` input ids; `open_cards`) unless the stored mirror is newer.
2. **Sample.** If the oldest `queued` input `Q` of `L` exists and nothing of `L` is in flight: `with_session(now + 250 ms, |s| (s.modes(), s.last_started(counted)))`, `counted` from the stored `write_basis`. `Busy` or `Gone` → skip.
3. **Open job** (`open_job`, through `supervisor::record`). Re-reads `L`, `Q`, and the thread; builds the `GateView` from the projection with `needs_bracketed_paste` = `pty_encoding == BracketedPaste` and `bracketed_paste_on` from the sampled modes; `gate(view)`; and refuses as `WriteAfterEvidence` when the sampled position is above `write_basis.position` (a counted write after the evidence with no event of its own, such as a VT reply: the evidence is stale, SB3). Open → `input_state_changed(queued → sending, GateOpened, sent_after_seq = head)`, clearing `input_gate_timeout`; it returns the attempt's seq, `write_basis`, the 2004 epoch, `last_write_before` (the projected `last_write_seq` before the attempt), and `competing` (`last_human_write_seq > last_turn_start_seq`, §2.5). Closed → no event; `Q` stays `queued`.
4. **Submit.** `encode(agent, host_os, text, Send)` (C2); an `Err` here cannot happen after `check` accepted the text at `send` (PR5-CL-1) and is treated as a refused submission. The unit is `WriteUnit { origin: Input { input_id }, parts: [Bytes(text), Pause(submit.delay), Bytes(submit.bytes)] }` (input-encoders §2.3). The `Guard`: `not_after` = now + `DISPATCH_WAIT` (10 s); `basis = Basis { seq: write_basis.position, counted }`; `bracketed_paste = Some(epoch)` iff `needs_bracketed_paste`; `recheck` below. `SendLaunch` records `Dispatch { input_id, position: None, competing, last_write_before }`, and the mirror takes the open job's view with `as_of` = the attempt's seq. Then `with_session(now + 1 s, |s| s.submit(unit, guard, completion))`. `Busy`, `Gone`, or a `SubmitError` → the completion job with `RecheckFailed` (nothing was submitted, so zero bytes are certain).
5. **Recheck** (on the writer thread, every earlier unit finished, pty-runtime §2.3). It stores `ctx.seq` into `Dispatch.position`, then evaluates `gate` on the mirror's view *without the input's own attempt* (A33): `last_write` = `last_write_before` when the mirrored `last_write_seq` is the attempt's own seq, `in_flight` over the mirror's in-flight ids minus this input, `bracketed_paste_on` from `ctx.modes`. `as_of` below the attempt's seq, or a closed gate → `Err(reason)`. The writer itself checks the basis against the counted classes (refusing `WriteSinceBasis`, `BasisUnknown`), the 2004 epoch, exit, close, and any mode-word change between the recheck and the first permission (X2: `Recheck("modes_changed")`).
6. **Completion job** (F3; the `Completion` sends the `WriteRecord` to the driver's channel without blocking). Only for an input still `sending` of a launch with no `launch_exited` (a completion after the exit job is ignored, pty-runtime §2.3): `Written` → `WriteCompleted` (`sent`), then `AckAttributed` when the input's `ack_seq` is set (§2.5 "While sending"); `NotStarted(_)` → `RecheckFailed` (`cancelled`); `Incomplete` → `WriteOutcomeUnknown` (`unconfirmed`, raising `input_unconfirmed`), then `AckAttributed` when `ack_seq` is set. `Attach`, `Key`, and `CardKey` records go to §2.6.

**After exit.** `SendExitStep` (10b's `ExitStep`, sup §2.11 "Exit job order") applies each `ExitFacts.sealed` record whose origin is `Input { input_id }` to that input if it is still `sending` (`RecheckFailed`; a sealed unit wrote zero bytes), expires the launch's open cards (`approval_resolved { outcome: expired }`), and its `after_commit` drops `L`'s `SendLaunch`. A unit refused `ChildExited` before the exit job commits is cancelled by its completion job; one whose completion comes after is ignored and the 10a1 sweep has made the input `unconfirmed` (A23).

### 2.4 Gate Evidence

A `readiness_observed` carries `EventAnnex::Readiness(WriteBasis { launch_id, position, counted })` (event-store §2.6), `position` = `last_started(counted)` sampled at the evidence's own intake and `counted` = the adapter's policy, `ClassSet::ALL` for every row (pty-runtime D3; S15). Per kind (A9, X3):

| Evidence | Where it is decided | Its intake (the basis) |
|---|---|---|
| `session_start` | the binding job, in `SupervisorHooks::interpret` for the binding `SessionStart` | the `SessionStart`'s `IntakeFactsV1.last_started` |
| `stop_settled` | `SendTick`'s settle job | `Supervisor::observe` sampled by the tick when it builds the job (an `Unobservable` launch is skipped that tick) |
| `post_decision` | `interpret`, when a decoder reports one | the hook's intake facts |
| `owner_asserted` | `ready`'s effect | `with_session(now + 1 s, \|s\| s.last_started(counted))` |

Evidence is recorded only when `evidence_allowed(row, kind)` holds (the model's `kindAllowed`: `session_start` only for Claude and not `OwnerOnly`; `stop_settled` only under `StopSettle`; `post_decision` only under `PostDecision`; `owner_asserted` always), the hook's `intake_reliable` is true, and its facts decode (D3). `Capabilities::readiness_mode` is `StopSettle` for all four rows (mvp §8 D8); PR 13 and PR 14 may change a row and expose the column.

**Settle completion** (A36). `ReadTx::settle_candidate(t, L)` returns `(receipt_ordinal, received_at)` of `L`'s delivery with the highest receipt ordinal when that delivery's first event is the `status_changed` of a `Stop` (`TurnStopped`) and no `human_write` of `L` follows it. `SendTick` builds the job for each live, bound, `running` launch with a candidate; the job re-reads both and appends `readiness_observed { stop_settled }` when its `recorded_at` minus the candidate's `received_at` is at least `SETTLE`, the turn is still `unknown` with no open card, and no evidence is newer than the `Stop`. Any later delivery of `L` (any routing, a buffered one included) has a higher ordinal and ends the candidate, so a completion counts only for its own `Stop` while no later hook arrived; a `Stop` promoted at binding has an ordinal below `binding_ordinal` and is never the latest (D3). The age is decided on store stamps as `buffer::expire` decides it (`buffer.rs:92-106`).

**Owner assertion** (C3). `chaldea ready` asserts that the CLI's composer is in its normal edit mode and empty, with no search, popup, or dialog open, which no signal shows (input-encoders §2.6). The effect samples the basis, then one `supervisor::record` job re-checks the launch is current, bound, and `running` and appends `readiness_observed { owner_asserted, by: human }`; a recheck that fails answers the intent's code. It writes no PTY byte. A send made when the assertion does not hold follows the ordinary match and attribution rules and is never resent. A write between the command and the sample lies inside the basis (L-3).

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
    CardOpened { tool_name: String, tool_input: Content, turn_ref: Option<String>, correlation: Option<ApprovalCorrelation> },
    CardEvidence { tool_use_id: String },       // a PostToolUse; the card whose correlation names it
    Other,
}
```

`NoDecoder` returns `Other` with no events for every hook, so until PR 13 and PR 14 install theirs every real hook stays `raw_unknown` `unknown_event` (`Interpretation::Raw`), as `MvpHooks` does today. Tests install `FakeDecoder`. Per fact, in the delivery or binding job (`L` is current, live, and bound, hook-ingest §2.1 rule 5):

| Fact | Events appended (besides `extra`) |
|---|---|
| `SessionStart` | in the binding job only: `readiness_observed { session_start }` when §2.4's conditions hold; a later same-session `SessionStart` is PR 13's session-switch decision and appends nothing here |
| `PromptSubmitted` | `status_changed` (`PromptSubmitted`); open cards of `L` → `approval_resolved { superseded }`; `user_message { prompt, input_id }` with the attribution below |
| `Stop` | `status_changed` (`TurnStopped`) first; open cards superseded |
| `Interrupt` (Codex) | `status_changed` (`Interrupted`); open cards superseded |
| `CardOpened` | `approval_requested { approval_id = the delivery's emit_id, launch_id: L, … }`, `status_changed` (`ApprovalOpened`) |
| `CardEvidence` | the open card of `L` whose correlation has that `tool_use_id` → `approval_resolved { ran }`, then `status_changed` (`ApprovalsAllResolved`) only if no other card of `L` stays open (A34; cards close at every turn boundary, so `L`'s open cards are the current turn's); none open → nothing |
| `PostDecision` | `readiness_observed { post_decision }` when allowed |
| `Other` | `extra`, or `Raw(UnknownEvent)` when empty |

**Intake facts** (D1). `SupervisorProbe` (new, built in `build` over `Supervisor::observe` and `SendMap`) is the `IntakeProbe`. It never waits:

```json
{"v":1,"reference":<u64>,"last_started":[<input>,<human>,<reply>],"inflight":{"input_id":"…","position":<u64|null>,"competing":<bool>}|null}
```

`LaunchObservation::Started { reference, last_started }` → `reference` and `last_started` as given; `Reserved` → zeros (no child exists, so no write precedes the hook); `Unobservable`, or `L`'s `SendLaunch` mutex contended (`try_lock`) → `None`. `inflight` is `L`'s `Dispatch` when one exists. `None`, another `v`, or a shape that does not parse are "facts missing": such a hook acks nothing and is no evidence, never "no write happened".

**Attribution** of a `PromptSubmitted` with prompt `P` (A10, X4, A16–A18, B2, C4), decided in the job:

1. `M` = the in-flight inputs of `L` (`sending`, `sent`, `unconfirmed`) with `prompt_matches(agent, encoding, text, P)`. `|M| = 0` → `user_message` without `input_id`. `|M| ≥ 2` → without `input_id`, no input changes (A18).
2. `M = {X}`, attributable only when all hold: `intake_reliable`; facts present; for a positional `X` (`encoding: positional`): `last_started[input] ≤ reference` and `last_started[human] ≤ reference` (SB5); otherwise `inflight.input_id = X`, `position = s` present, `last_started[input] ≥ s` (dispatched at intake), each other counted class's `last_started < s` (no counted write after it), and `competing` false; and no input of `L` that left flight without a hook ack (`acked` with no `ack_seq`, `dismissed`) has `prompt_matches` for `P` (an earlier unattributed dispatch, C4).
3. Attributable → `user_message { input_id: X }`; `X` `sent` or `unconfirmed` → `AckAttributed` (`unconfirmed → acked` is A7's late ack); `X` `sending` → nothing else now (**while sending**: the `user_message` sets `X.ack_seq`, event-store §2.6, and the completion job applies `AckAttributed` after the outcome, `Written` or `Incomplete`, A16). Not attributable → no `input_id`; `X` `sent` → `AckNotAttributable` (`unconfirmed`); `sending` or `unconfirmed` → unchanged (the window, §2.7, ends a `sent` input that no attributable hook acks).

`competing` is the open job's record of a gate-closing human write (`attach`, `key`) after the launch's last turn start: its submission, if any, is unknown history, so the input stays `unconfirmed` (mvp §2.3 "Ack"; formal-specs §3's case: a human submits `P`, its hook is delayed, the owner asserts readiness, a dispatch of `P` is not submitted). A `key` write after `X`'s dispatch is a counted `Human` write after `s` (A17); a human write during `X`'s unit is queued behind it and is such a write too, so a hook whose intake follows it acks nothing (A8).

### 2.6 Keys, Human Writes, Cards

**Key encoding** (`core::input::key`, new): `KeyToken` is one of `enter` (`\r`), `escape` (`\x1b`), `tab` (`\t`), `backspace` (`\x7f`), `ctrl_c` (`\x03`), `up`/`down`/`right`/`left` (`ESC [ A`… or, under DECCKM, `ESC O A`…), or `{ "char": c }` for one printable ASCII scalar; `encode_keys(tokens, app_cursor)` concatenates them into one `Bytes` part, at most `MAX_KEYS` (32) tokens. Keys are VT input on both OSes; ConPTY's input parser turns them into key events, as it does for `win_keys` text (input-encoders §2.5 rule 2's source).

**`key`'s effect** (owner only, mvp §8 "Also decided"). `live(thread, L)` → `None` → `no_live_launch`. `with_session(now + 1 s, …)` reads `modes()`, encodes, and submits `WriteUnit { origin: Key | CardKey { approval_id }, parts: [Bytes] }` with `Guard { not_after: now + 2 s, recheck }`: the recheck refuses unless the mirror shows `L` `running` and, for a card key, the card in `open_cards`. The effect awaits the completion with `settle_before` until `not_after` + 1 s: `Written` → `KeyResult { outcome: written, bytes_written }`; `Incomplete` → `{ outcome: incomplete, bytes_written }`; `NotStarted(Recheck)` of a card key → 409 `card_closed`; other `NotStarted` → 409 `key_not_written`. Not received by then: `WriteTicket::cancel()` true → 409 `key_not_written` (it never dispatches); false → `{ outcome: started, bytes_written: null }` (dispatched; its `human_write` follows its completion). A key is not gated (it is how the owner clears a startup prompt, mvp P-12).

**`human_write`** (F4). The completion of every `Attach`, `Key`, and `CardKey` unit with outcome `Written` or `Incomplete` (bytes may have reached the CLI, any `bytes_written`) is one `supervisor::record` job appending `human_write { launch_id, origin: record.origin.human_write(), approval_id }`; `NotStarted` appends nothing. PR 11 submits attach units with the same completion. The projection then advances `last_write_seq`, and for `attach`/`key` `last_human_write_seq` (event-store §2.6). So after a raw key only evidence of a later turn or `ready` reopens the gate (`gate`'s `EvidenceOfSameTurn`, A11, A20), however late a same-turn evidence is committed; after a card key, which answers a dialog rather than the composer, the turn's next evidence reopens it (A19, mvp §2.3).

### 2.7 Timers (`SendTick`)

`SendTick` is registered as a `TickStep` and runs after 10a1's per tick. Each job re-validates from the store and decides age on store stamps (`recorded_at` of the job against the stored event's), as `buffer::expire` does:

| Timer | Candidate | Job |
|---|---|---|
| Gate timeout (`GATE_TIMEOUT` 60 s, A15) | a `queued` input of a live launch without `input_gate_timeout` | raise `input_gate_timeout(input_id)` (stays `queued`, zero bytes) and append `vt_snapshot_taken` from `Session::snapshot(now)` taken through `with_session` when the tick builds the job; a contended VT (`VtUnavailable` with `vt_failed` false) defers the raise to the next tick; a failed VT raises without a snapshot |
| Ack window (`ACK_WINDOW` 30 s) | a `sent` input whose `sent` event is at least 30 s old; for a positional input, only while neither startup cause is active (mvp §2.3) | `AckWindowExpired` (`unconfirmed`) |
| Settle (`SETTLE`, 2 s) | §2.4 | §2.4 |

A tick job that cannot be recorded is skipped (sup §2.6).

### 2.8 Waits

| Wait | Helper | Bound |
|---|---|---|
| Driver read | `deadline::settle_before` on the blocking read | now + 1 s; otherwise skipped until the next wake-up |
| Mode and position sample; `ready`'s basis | `LiveLaunch::with_session` | 250 ms; 1 s |
| Submit (send, key) | `with_session` | 1 s; `Busy` → zero-byte refusal |
| Unit dispatch | `Guard.not_after` (the writer) | `DISPATCH_WAIT` 10 s (send), 2 s (key); a dispatched unit is not bounded by it (pty-runtime §2.3 "Started units") |
| Key completion | `settle_before` on the completion's oneshot | `not_after` + 1 s; then `cancel()` decides |
| Open, completion, `human_write`, `ready` jobs | `supervisor::record` (sup §2.10) | its budget; an unrecordable `ready` or `key` ends `Unrecorded` |
| Tick jobs | `supervisor::record`, skipped on failure | 1 s period (`supervisor::tick_timer`) |

No new deadline exception.

### 2.9 What sup §2.11 Gives and the One Addition

§2.11 gives 10b everything else it needs: `with_session` for `submit`, `modes`, `last_started`, and `snapshot`; `observe` for the probe and the tick; `Published.reference` as the positional anchor (sup §2.3 step 5); `ExitFacts.sealed`; `ExitStep::after_commit`; `TickStep`; registration in `SupervisorComponents::new` (exit and tick steps, shared `SendMap`) and `build` (`HookSeams { adapter: SupervisorHooks, intake: SupervisorProbe, … }`); `attach` for the driver; `Store::subscribe` from `BuildCtx`.

**Missing: a queued first input at reservation.** sup §2.3 step 3 creates only the positional input. Smallest addition: `LaunchInput` gains `queued_prompt: Option<(InputId, String)>` (Claude only), and the reservation job appends its `input_created` (queued, `bracketed_paste`) after `launch_reserved`. Nothing else in 10a1 changes; the input then follows §2.3 like any `send`.

**Also assigned here:** card expiry at exit, which sup §2.11 lists under PR 13, moves to `SendExitStep`, since 10b's `SupervisorHooks` opens the cards.

## 3. Decisions

Numbered SB (send-side decision). "Public" marks owner-visible contracts.

**SB1. Hole b, a zero-byte recheck (A31).** After `RecheckFailed`, the attempt's `sending` has already advanced `last_write_seq` (event-store §2.6), so the gate stays `WriteAfterEvidence` until new evidence or `chaldea ready`.

| Option | Gain | Loss | Reversibility | Cost |
|---|---|---|---|---|
| **A. Accept** | no projection or public change; stale evidence (the commonest cause, a reply after the basis) keeps the gate closed, as it must | after a recheck failure the next queued input waits for new evidence or `ready`; `input_gate_timeout` shows it after 60 s | a projection change later | none |
| B. Advance `last_write` at `sent`/`unconfirmed` instead of `sending` | a zero-byte attempt leaves the gate as it was | the gate must still tell a stale basis (no event) from a transient refusal; that needs the refusal's cause in a public payload or a new annex, and a projection-version bump | needs rebuild | medium |

**Adopted: A**, also because §2.3 step 3 now keeps a stale basis from opening at all, so recheck failures are races of milliseconds. Concrete input: send `a`, a reply lands between open and dispatch: `a` is `cancelled` (visible), the next send waits for the next `Stop` or `ready`; before, nothing existed. Changes it: owner reports of threads stuck after a cancel.

**SB2. `readiness_evidence: none` withholds Claude's `SessionStart` (A37).** Adopted, as the model assumes (`kindAllowed`): `OwnerOnly` exists to stop automatic sends where the adapter's evidence is not trusted, and mvp §2.3 says "only `chaldea ready`". Alternative: `SessionStart` counts in every mode (mvp §2.7 lists it first) — a mode named "none" would still send at startup. No row uses `OwnerOnly` in the MVP; PR 13 may. Internal now; public with the PR 13/14 column.

**SB3. The gate's evidence basis (A9, X3).** Adopted: the basis is sampled at the evidence's own intake (§2.4's table), and the open job refuses a stale basis (position moved past it) as `WriteAfterEvidence`, so the input stays `queued` instead of being cancelled at dispatch. Alternatives: the settle basis at the `Stop`'s intake (a reply to a query the CLI sends after its turn would cancel every send; the model takes the basis at the completion, `ready(ev, stop)`); no staleness check at open (each send after an evidence-less reply is cancelled, SB1). Internal.

**SB4. The ack candidate rule (A10, X4, A16, A17, B2, C4).** Adopted: §2.5's attribution, decided in the delivery job from the hook's intake facts (positions and the in-flight `Dispatch`) and the store; while `sending`, an attributable match sets `ack_seq` and the completion job applies it after either outcome.

| Option | Gain | Loss | Reversibility | Cost |
|---|---|---|---|---|
| **A. Facts carry the dispatch position; the job decides; `ack_seq` holds the while-sending ack** | `interpret` stays data-only; no new annex; the public `user_message.input_id` is always the attributed input | an ack after an unknown outcome acks (the model dropped it); the window, not an event, ends a non-attributable while-sending match | model and doc change | small |
| B. Keep the candidate in supervisor memory | as the model | `interpret` cannot read memory (hook-ingest §2.6) | — | — |
| C. A new annex on `user_message` marking the candidate | exact model | a CHECK and projection change for a rare race | additive | medium |

An ack after `Incomplete` is sound: the hook's prompt equals the input's expected form, its intake shows the dispatch and no other counted write, so the CLI submitted that text. **Public** in effect (which sends end `acked`); changes it: PR 13's captures narrowing `prompt_matches`.

**SB5. The positional anchor and what counts against it (A35).** Adopted: the anchor is `Published.reference` (sup §2.3 step 5); `Human` and `Input` writes above it make the ack non-attributable; `Reply` writes do not. Examples: Codex answers its startup queries after publication (replies above the anchor) → still attributable; the owner attaches and types before the `UserPromptSubmit` → not attributable; a reply before publication is below the anchor either way. Reason: the prompt is in argv, not on the PTY, so a reply cannot join or displace it, contains no CR or LF (pty-runtime §2.5's reply table), and cannot be text a matching prompt contains (`check` rule 1 refuses ESC). Rejected: counting replies (`trace_finding_positional_vt_reply`: every Codex launch whose queries are answered after publication would leave its first prompt `unconfirmed`, closing the gate until the owner resolves it). Sends keep `ClassSet::ALL` (mvp §2.3). Public in effect.

**SB6. `ready`'s meaning and shape (C3). Public.** Adopted: §2.4 "Owner assertion", accepted only for the current, bound, `running` launch, with optional `expected_launch_id` (the CLI pins it from `GET /api/threads/{thread_id}` as `resume` does, sup SD16), the basis sampled in the effect. Alternatives: no pin (an assertion about one CLI lands on its replacement after a resume); sampling in `prepare` (it has no launch id without a store read). Changes it: a client that needs to assert for a starting launch.

**SB7. The recheck reads a mirror (A33).** Adopted: the recheck evaluates `gate` on the driver's last read (`MirrorView`), no older than the attempt; writes, modes, exit, and stop are checked by the writer itself at the dispatch boundary. Alternatives: a store read on the writer thread (blocks every PTY write on the reader pool); refusing on any thread-stream advance (cancels a send for an unrelated hook); no recheck of hook facts (against mvp §2.3). Limit L-1.

**SB8. Wire types and codes of §2.2. Public.** Adopted as listed; every result field always present (`payload.rs:3-4`). `send` returns `rejected` with its reason instead of a 400 so the PWA shows the input (PR 19) and an agent sees why; `origin` is a client claim, informational only. One `input_state` code for both input commands. Changes it: a client that must tell the two refusals apart (split the code, additive).

**SB9. `key`'s token set and result. Public.** Adopted: §2.6's tokens and `KeyOutcome` `written | incomplete | started`. Alternatives: raw bytes (a client could send ESC sequences the keypad does not need, and arrows would ignore DECCKM); a full key-name table now (unused before PR 19's keypad). Additive later.

**SB10. The decoder seam (PR 13/14 boundary).** Adopted: `SupervisorHooks` owns state effects; `HookDecoder` only reads payloads. Alternative: each adapter writes state events itself (attribution and gate rules duplicated per CLI). Internal.

**SB11. F5 to PR 11.** No 10b path resizes; attach is the first caller of `Session::resize`, and pty-runtime §2.2 leaves resizing during a unit to it.

**SB12. Timers on store stamps.** Adopted for gate timeout, ack window, and settle, as `buffer::expire`; tests set the store clock. Alternative: the supervisor's monotonic clock with first-seen memory (cross-call state). A clock step back delays a timer (as PR 9 states for the buffer).

**SB13. Claude's `spawn --prompt` (§2.2).** Adopted: a queued input at reservation. Alternative: keep 400 (Claude spawns need a second command). Public: `spawn` accepts a Claude prompt. Changes it: none foreseen.

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
| A16 | §2.5 "while sending"; SB4; PR10b-CL-8 |
| A17 | §2.5; PR10b-CL-10 |
| A18 | §2.5 step 1; PR10b-CL-9 |
| A19 | §2.6; PR10b-CL-12 |
| A20 | §2.6; PR10b-CL-12 |
| A23 | §2.3 "After exit"; PR10b-CL-6 |
| A31 | SB1; PR10b-CL-5; §7 |
| A33 | §2.3 step 5; SB7; PR10b-CL-5 |
| A34 | §2.5 `CardEvidence`; PR10b-CL-13 |
| A35 | SB5; PR10b-CL-11; §7 |
| A36 | §2.4 "Settle completion"; PR10b-CL-4 |
| A37 | SB2; PR10b-CL-4 |
| A38 | §7; PR10b-CL-21 |
| B2 | §2.5 `competing`; PR10b-CL-10; §7 |
| B3 | §7 |
| C2 | §2.2, §2.3 steps 3–4; PR10b-CL-1, PR10b-CL-2 |
| C3 | §2.4; SB6; PR10b-CL-14 |
| C4 | §2.5 step 2; PR10b-CL-9 |
| C5 | PR10b-CL-2 |
| C8 | §2.2; PR10b-CL-1 |
| D1 | §2.5 "Intake facts"; PR10b-CL-7 |
| D3 | §2.4, §2.5; PR10b-CL-7 |
| E1 | §2.2; PR10b-CL-17 |
| E2 | §2.2 codes; PR10b-CL-17 |
| F3 | §2.3 step 6; PR10b-CL-6 |
| F4 | §2.6; PR10b-CL-12 |
| F5 | PR 11 (SB11) |
| X2 | §2.3 step 5; PR10b-CL-6 |

**Handed on.** F5 to PR 11. Real-payload decoders, approval correlation (mvp §2.10), session switches, and the readiness column of `GET /api/adapters`: PR 13, PR 14.

## 5. Purpose Layer and Plan Layer

### 5.1 Purpose Layer: Acceptance Criteria

Until PR 13 and PR 14 install decoders, a real CLI never binds (sup L-4), so on real CLIs 10b can show only the paths that need no hook: refusals, queuing, the gate timeout, keys, and the input commands. Rows marked "PR 13/14" are checked by FakeCli now and on real CLIs by those PRs. Windows runs every FakeCli check in CI.

| ID | Criterion (EARS) | Source | Scene | Observable result / failure | Real-CLI check |
|---|---|---|---|---|---|
| PR10b-P-1 | When a prompt is sent to a ready thread, the CLI shall record it once in its expected form, and the input shall end `acked`. | mvp P-6, P-7, P-8; §2.3 | idle bound thread | Pass: one `UserPromptSubmit` equal to `expected_prompt`; `acked`. Fail: a second submit; `unconfirmed` with a matching hook. | PR 13/14 (Claude, Codex macOS; Windows by PR 15) |
| PR10b-P-2 | While the gate is closed, a sent prompt shall stay `queued` with zero bytes, and after 60 s the thread shall carry `input_gate_timeout` with a VT snapshot. | mvp §2.3 | a CLI that never sends hooks | Pass: `queued`, no PTY byte, cause and snapshot. Fail: a write; no cause. | Now: `chaldea spawn --agent claude`, `chaldea send <t> hi`, read the thread after 61 s; FakeCli `stall` in CI |
| PR10b-P-3 | If `chaldea ready` is run for an unbound launch, then it shall be refused with `not_bound` and write nothing. | mvp §2.3; A12 | any real launch before PR 13/14 | Pass: 409 `not_bound`; no event. | Now |
| PR10b-P-4 | When the owner sends keys to a thread, they shall reach the CLI and be recorded as `human_write`, closing the gate. | mvp §2.3, P-12 | Claude's trust prompt | Pass: the prompt clears; `human_write { key }`. Fail: no effect; no event. | Now: `chaldea key <t> enter` |
| PR10b-P-5 | If an input is not acknowledged within the ack window, then it shall become `unconfirmed` with attention and never be sent again. | mvp P-15 | a dialog over the composer | Pass: one write, `unconfirmed`. Fail: a second submit. | PR 13/14 |
| PR10b-P-6 | When the owner cancels a queued input or resolves an `unconfirmed` one, the input shall end `cancelled`, `acked` (by human), or `dismissed`, and its attention shall clear. | mvp §2.3 | — | Pass: the state and cleared cause. | Now (cancel); PR 13/14 (resolve) |
| PR10b-P-7 | No text a CLI prints shall open the gate or ack an input. | invariant 1 | FakeCli `banner` printing a prompt echo | Pass: input stays `queued`. | CI only |

### 5.2 Plan Layer: Claims

`supervisor::send::…` are daemon unit tests with `FakePty`, the in-process store, `FakeDecoder`, and pause points; `supervisor_e2e::…` use the native runtime and FakeCli (sup §6); `key::…` are `core` tests. Every claim is checked on `macos-latest` and `windows-latest`.

| ID | Claim (EARS) | Source | Test | Mutation it kills |
|---|---|---|---|---|
| PR10b-CL-1 | When `send` is given text over `MAX_PROMPT_BYTES`, it shall answer 400 `prompt_too_large` with no receipt or input; when `check` rejects the text, it shall create the input and reject it in one job with that reason; otherwise the input shall be `queued` for the current launch with the row's `pty_encoding`. | C8, C2 | `supervisor::send::admission` (sizes `MAX_PROMPT_BYTES` and +1; `"a\x1bb"`; whitespace-only on each row; a Codex Windows text with a trailing space) | size check after the input; rejection as 400; `encoding` from `first_prompt` |
| PR10b-CL-2 | When an input is written, its unit shall be `[Bytes(text), Pause(submit_delay), Bytes(b"\r")]` from `encode` at write time, and the fake child shall receive one write call per `Bytes` part with every escape sequence inside one call. | C5; input-encoders §2.3 | `supervisor::send::unit_shape` (FakePty write calls for Claude, Codex macOS, Codex Windows rows, multi-line Japanese and emoji) | text and submit in one part; pause dropped; encoding chosen by DECSET 2004 |
| PR10b-CL-3 | If any one gate condition is missing (no evidence, a human write after it, a `Stop` inside the settle interval, an interrupt, DECSET 2004 off, unbound, `OwnerOnly` without `ready`, a counted write after the basis with no event), then the input shall stay `queued` with zero bytes. | A14; mvp CL-5 | `supervisor::send::each_condition_alone` (one case per condition from an open baseline; `FakeChild::writes()` empty) | a condition skipped; stale basis ignored at open |
| PR10b-CL-4 | Readiness evidence shall be recorded only as §2.4 allows, with the basis sampled at its own intake; a settle completion only for the launch's latest delivery when it is a `Stop`, at least `SETTLE` old by store stamps; under `OwnerOnly` no `session_start` evidence. | A9, X3, A36, A37, D3 | `supervisor::send::evidence_kinds` (each kind × mode; a retry hook, `intake_reliable: false`; facts missing); `supervisor::send::settle` (store clock at `SETTLE` − 1 ms and `SETTLE`; a later raw hook, a buffered `Stop` before the binding `SessionStart`, a `human_write` after the `Stop`) | basis from the `Stop` for settle; settle after a later hook; `SessionStart` under `OwnerOnly` |
| PR10b-CL-5 | When the writer rechecks an input, it shall evaluate `gate` on the mirror without the input's own attempt; if a hook committed and read before the recheck closed the gate, or the basis, epoch, or mode word moved, then the input shall be `cancelled` with zero bytes, and the gate shall stay `WriteAfterEvidence` until new evidence or `ready`. | A33, A31, X2 | `supervisor::send::recheck_view` (writer parked at `ProbePoint::RecheckEntered`; a `PromptSubmitted` committed and mirrored → `cancelled`; a reply → `WriteSinceBasis`; a mode toggle → `Recheck("modes_changed")`; without such a change → written); `supervisor::send::hole_b_kept` | recheck on the projected view with the own attempt (always closed); own attempt counted as in flight |
| PR10b-CL-6 | `Written` shall move the input to `sent`, every `NotStarted` and refused submission to `cancelled`, `Incomplete` to `unconfirmed`; a completion after `launch_exited` and a sealed record already applied shall change nothing. | F3, A23, X2; pty-runtime D5 | `supervisor::send::outcomes` (`FakePty` `fail_writes_after`, `hold_writes` with close, `Busy` and `Gone` from `with_session`); `supervisor::send::after_exit` (completion held past the exit job; `ChildExited` before it) | `Incomplete` as `RecheckFailed`; completion applied after exit |
| PR10b-CL-7 | `SupervisorProbe` shall never wait, return `None` for an unobservable launch or a contended `SendLaunch`, and the facts of §2.5 otherwise; a hook with missing facts or `intake_reliable: false` shall ack nothing. | D1, D3, A10, X4 | `supervisor::send::probe` (each observation; the mutex held by the test → `None` at once); `supervisor::send::undispatched_match` (intake before the dispatch → `user_message` without `input_id`, input unchanged) | `Unobservable` as zeros; blocking lock |
| PR10b-CL-8 | When an attributable hook arrives while its input is `sending`, the input shall be `acked` after `Written` and after `Incomplete`; when one arrives for an `unconfirmed` input of the live launch, it shall be `acked`. | A16, A7 | `supervisor::send::ack_while_sending` (hook job before the completion job, both outcomes); `supervisor::send::late_ack` (window expired, then the hook) | `ack_seq` ignored by the completion job; late ack dropped |
| PR10b-CL-9 | If a prompt matches no in-flight input, or two, or an earlier input left flight without a hook ack and matches it, then no input shall be acked. | A18, C4 | `supervisor::send::matching` (zero, one, two matches; an `acked`-by-human and a `dismissed` input with the same text) | first match taken; resolved inputs not searched |
| PR10b-CL-10 | If a counted write other than the input's started after its dispatch and before the hook's intake (a `key`, an attach unit queued during the input's unit, a reply), or a gate-closing human write followed the last turn start before the attempt, then the hook shall not ack the input. | A8, A17, B2 | `supervisor::send::key_after_send`; `supervisor::send::human_during_unit` (attach unit submitted at `PauseEntered`); `supervisor::send::competing_human` (attach write, `ready`, send, delayed matching hook → `unconfirmed`; control without the attach → `acked`) | positions compared with `<=`; `competing` ignored |
| PR10b-CL-11 | A positional prompt's hook shall be attributable exactly when no `Human` or `Input` write is above `Published.reference` at its intake; `Reply` writes shall not count. | A35 | `supervisor::send::positional_anchor` (`FakeScript.startup_output` with a DSR answered after publication → `acked`; an attach write after publication → `unconfirmed`; facts `Unobservable` → `unconfirmed`) | replies counted; anchor taken at the start commit |
| PR10b-CL-12 | When an `Attach`, `Key`, or `CardKey` unit is `Written` or `Incomplete`, one `human_write` with its origin shall be recorded; after a raw key only evidence of a later turn or `ready` shall reopen the gate, however late a same-turn evidence is committed; after a card key the turn's next evidence shall reopen it. | F4, A11, A19, A20 | `supervisor::send::human_writes` (each origin and outcome); `supervisor::send::same_turn_evidence` (a settle completion of the `key`'s turn committed after it: closed; after the next turn start: open); `supervisor::send::card_key_reopens` | `Incomplete` dropped; `card_key` as `key` |
| PR10b-CL-13 | `ApprovalsAllResolved` shall be fed only when the resolved card was the last open card of its launch, matched by `approval_id`; a card key whose card closed shall be refused `card_closed` with zero bytes, at the intent or at the write boundary. | A34, A19; mvp §2.10 | `supervisor::send::cards` (two cards, resolve one then the other; a resolution after `Stop`; a card key with the card superseded after the intent, recheck parked) | feed on any resolution; card key unchecked at the writer |
| PR10b-CL-14 | `ready` shall record `owner_asserted` only for the current, bound, `running` launch with the basis sampled in its effect, and answer `not_bound`, `no_live_launch`, or `launch_changed` with nothing written otherwise. | A12, C3 | `supervisor::send::ready` (each refusal; success then a send opens) | unbound accepted; basis 0 |
| PR10b-CL-15 | A `queued` input shall raise `input_gate_timeout` with a `vt_snapshot_taken` once it is `GATE_TIMEOUT` old by store stamps, and a `sent` input shall become `unconfirmed` once `ACK_WINDOW` old, a positional one only while no startup cause is active. | A15; mvp §2.3 | `supervisor::send::timers` (store clock at the bound − 1 ms and at it; contended VT defers; failed VT raises bare); `supervisor_e2e::stall_times_out` (FakeCli `stall`) | timeout re-raised; window while a startup cause is active |
| PR10b-CL-16 | `key` shall write exactly the encoded tokens and answer `written`, `incomplete`, `started`, `key_not_written`, or `card_closed` as §2.6 states. | §2.6; SB9 | `key::encode` (each token, DECCKM on and off, 33 tokens refused); `supervisor::send::key_outcomes` (held writes for `started`, held writer for `key_not_written`) | arrows ignoring DECCKM; `started` reported for a cancelled unit |
| PR10b-CL-17 | The five handlers shall be registered; two concurrent requests with one `command_id` shall cause one effect; every new code shall be in `handler_code` at one status; no success type shall have a top-level `code`. | A6, E1, E2 | `supervisor::send::duplicates_write_once` (per command); `handler_code` exhaustive match; `supervisor::commands::success_schemas_have_no_code` extended | a code at two statuses |
| PR10b-CL-18 | `input_cancel` shall act only on `queued` and `input_resolve` only on `unconfirmed` inputs, an agent only on inputs it created; a cancel racing the open job shall leave exactly one of them in effect. | mvp §2.3, §2.5 | `supervisor::send::input_commands` (each state; agent token on another thread's input → 404; cancel against a held open job) | cancel of a `sending` input |
| PR10b-CL-19 | A Claude `spawn --prompt` shall create a `queued` input for the launch in the reservation job and write it when the gate first opens. | §2.9; SB13 | `supervisor::send::claude_first_prompt` | input created after `launch_started`; prompt still 400 |
| PR10b-CL-20 | No byte the CLI prints shall open the gate, ack an input, or change state. | invariant 1; mvp CL-1 | `supervisor_e2e::banner_never_acks` (FakeCli `banner` printing a prompt echo with a queued input) | any output reader feeding the driver |
| PR10b-CL-21 | The guards of §7 shall be in `thread_input.qnt` with their traces in the same PR. | A38 | CI `specs` job and `quint_traces.rs` | a guard without its trace |

### 5.3 Accepted Limits

| ID | Limit | Decided by | Why it does not defeat the purpose |
|---|---|---|---|
| L-1 | A hook committed but not yet read into the mirror when the recheck runs does not stop the dispatch. | SB7 | writes, modes, stop, and exit are checked exactly by the writer; hook facts are already asynchronous with the CLI (mvp §2.3 "Residual constraint"), so such a hook is the same as one arriving just after the dispatch: the input is then attributed or left `unconfirmed`, never resent. |
| L-2 | After a gate-closing human write with no later turn start, a send opened by `ready` ends `unconfirmed` even when the CLI submitted it. | mvp §2.3 "unknown history"; B2 | the owner sees `unconfirmed` and resolves it; the alternative risks acking a human's submission as the send. |
| L-3 | A write between `chaldea ready` and its basis sample is inside the basis. | SB6 | only the owner can write such keys (`key` and attach are owner-only), and the assertion is theirs. |
| L-4 | Until PR 13 and PR 14, real CLIs never bind, so `ready` is refused and every `send` waits and times out. | sup L-4; SB10 | the path is tested end to end with FakeCli and `FakeDecoder`; keys work now. |
| L-5 | Claude's matching accepts every subset of its known transforms until PR 13's captures. | input-encoders D4 | a lenient match acks only an input whose text equals the prompt up to those transforms; two matches ack none. |
| L-6 | A `sending` input that a hook attributed is left `unconfirmed` when the launch exits before its completion job. | §2.3 "After exit" | never resent; the owner resolves it. |
| L-7 | `SETTLE` (2 s) is not measured on real CLIs. | SB12; mvp D8 | internal default, an adapter row value; PR 12 measures it (P-7); a user `Stop` hook that blocks longer is mvp D8's known race. |
| L-8 | That ConPTY delivers `key`'s VT bytes (arrows, Escape) to Codex on Windows as the same keys is not run on a real host. | §2.6; OpenConsole's input parser (input-encoders §2.5 rule 2) | FakePty checks the bytes; a key that does not arrive leaves the screen unchanged and is visible to the owner, who uses attach (PR 11); PR 15 runs it on the real host. |

## 6. Tests

Unit tests drive `FakePty` (`hold_writes`, `wait_entered`, `fail_writes_after`, `FakeScript.startup_output`, `ProbePoint::RecheckEntered`, `PauseEntered`), the store with an injected clock, and `FakeDecoder`, which maps FakeCli payloads (`hook_event_name`, `prompt`, `tool_use_id`, `tool_name`) to `HookFact`s. Integration tests run the daemon with FakeCli `hooks` and the native runtime as `crates/cli/tests/hook_e2e.rs` does, with `FakeDecoder` installed through `SupervisorConfig`.

**FakeCli `stall` mode** (new): prints a fixed screen and sleeps until `--max-life-secs`, sending no hook.

**Host speed.** Timers run on store stamps set by the test's store clock and on `Supervisor::tick(now)`; writer races are held with `wait_entered`, `park`, and pause points; intake facts are taken while the test holds the writer at a probe point; no test sleeps in place of an event. Native tests read the published port (`client::discover`).

**Windows CI.** Every `supervisor::send::`, `key::`, and `supervisor_e2e::` test runs on `windows-latest` (`scripts/windows-tests.ps1 -Lane Cargo`); the Codex Windows row is exercised on both runners through `HostOs::Windows`.

**Mutation check.** Each mutation of §5.2 is applied by hand before merging and the named test seen failing.

**Completion commands**: sup §6's.

## 7. Quint

Changes to `specs/thread_input.qnt` (in the implementation PR, with `pnpm run traces:write`):

1. **`attributable` gains the competing-submission condition (B2).** `AckRecord` gains `noCompetingSubmission`; `open` records on the input whether `gp.lastHumanWrite > gp.lastTurnStart`; `intakeFacts` copies it. New traces `trace_competing_human_submission` (`humanWrite(Attach)`, `ownerReady`, `send`, `open`, `dispatch`, `outcome`, `prompt(Set(1))` → `unconfirmed`) and `trace_send_ack` unchanged as the clean control.
2. **Per-class positions, `counted` as policy, the positional exception (B3, A35).** The writer list keeps its classes; `LaunchConfig` gains `counted` (all three classes for every adapter); `noOtherWriteAfter` counts only classes in `counted` and, for a positional input, ignores `WReply`. `trace_finding_positional_vt_reply` becomes `trace_positional_vt_reply_acked` (`expect` `Acked`); new `trace_positional_attach_not_attributable`.
3. **`outcome` applies a kept attributable candidate after either outcome; `applyPrompt` keeps only attributable candidates (A16).** `trace_ack_while_sending` unchanged; new `trace_ack_while_sending_unknown_outcome` (`outcome(1, false)` → `Acked`) and `trace_unattributable_while_sending` (a `key` between dispatch and the hook: `Sending`, then `outcome(1, true)` → `Sent`, then `windowExpired` → `Unconfirmed`).
4. **`open` requires `st.writer.length() == st.gp.basis` (SB3).** New `trace_stale_basis_keeps_queued` (`vtReply` after `ready`: `open(1)` disabled, `Queued`, zero writes). `trace_refused_recheck_reply_after_basis` (reply after `open`) unchanged.
5. **Unchanged:** hole b (`trace_hole_b_recheck_keeps_gate_closed` stays as the statement of SB1); `kindAllowed` (SB2); `cardResolve` (A34 already by identity); `ready(ev, stop)` (A36).

New invariants: none; `ackOnlyWhenAttributable` reads the extended record, so a dropped condition still violates it. `dispatchOnlyThroughRecheck` holds unchanged.

**Guard-to-test table** (daemon guards the replay cannot see):

| Model guard | Daemon site | Test |
|---|---|---|
| `open`: `gate`, basis current | `open_job` (§2.3 step 3) | PR10b-CL-3 |
| `dispatch`: `recheckView` | the recheck (§2.3 step 5) | PR10b-CL-5 |
| `attributable` | §2.5 attribution | PR10b-CL-7–CL-11 |
| `ready`, `ownerReady`, `kindAllowed` | §2.4 | PR10b-CL-4, CL-14 |
| `humanWrite` (card key open) | `key`'s intent and recheck | PR10b-CL-13 |
| `cardResolve` | `SupervisorHooks` `CardEvidence` | PR10b-CL-13 |
| `windowExpired`, `gateTimeout` | `SendTick` | PR10b-CL-15 |

## 8. Proposed Canonical-Doc Changes

Not applied here.

- **mvp §2.3 "Gate"**: a counted write after the evidence's basis keeps the gate closed (the input stays `queued`), not only the writer's refusal; after a zero-byte recheck the gate stays closed until new evidence or `chaldea ready` (hole b accepted, SB1). **"Ack"**: the positional prompt is anchored at the writer position sampled at publication, and only `attach`, `key`, and input writes above it count (SB5); an ack attributed while `sending` is applied after the write outcome, also an unknown one; an earlier input resolved by the owner (`acked` by human, `dismissed`) whose text matches counts as a competing submission. **`ready`**: accepted for a `running` bound launch, optional `expected_launch_id`.
- **mvp §2.4 / §2.5**: the args, results, and codes of §2.2 (`no_live_launch`, `not_bound`, `card_closed`, `key_not_written`, `input_state`); `send` returns a rejected input instead of refusing; the `key` token set and `KeyOutcome`; Claude's `spawn --prompt` is a queued input.
- **mvp §2.7**: `readiness_evidence: none` withholds Claude's `SessionStart` too (SB2).
- **mvp §2.10**: `launch_exited`'s card expiry is applied by 10b's exit step.
- **mvp §4 PR 10 bullet**: 10b decides holes b and the competing-submission condition as above; F5 moves to PR 11.
- **formal-specs §1**: the `none` assumption is decided (SB2). **§3**: hole b owner "PR 10b, accepted"; the competing-submission condition and the positional finding are resolved by §7 here. **§6**: PR 10's bullet points to §7 here.
- **hook-ingest §2.4**: `SupervisorProbe` replaces the default probe; `IntakeFacts` content is `IntakeFactsV1`. **§2.6, §4 PR 10 row**: `SupervisorHooks` wraps `HookDecoder`s; PR 13 and PR 14 supply decoders, not adapters.
- **pty-runtime §2.3 "How the daemon uses it"**: the recheck reads the supervisor's mirror; the positional counted classes (SB5); F5 is PR 11's.
- **input-encoders §4 PR 10 row**: done as §2.3; `core::input::key` added.
- **supervisor.md §2.11**: `LaunchInput.queued_prompt` (§2.9); card expiry in 10b's exit step.
- **http-surface §2.8 "Handler replies"**: the five codes.
