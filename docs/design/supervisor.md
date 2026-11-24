# Design: Supervisor, Launch and Process Side (MVP PR 10a)

Status: design. Scope is the launch and process half of the PR 10 row of `docs/mvp-design.md` §4 (cited as "mvp §N"), called **PR 10a**: reservation, the `spawn`, `resume`, `stop`, and `close` commands and the `chaldea wait` CLI, the process lifecycle and its death proof, the restart reconciler, the hook token and binding facts of each launch, `extra` attribution, `GET /api/adapters`, the host-OS mapping, and the startup attention causes. The send and ack half (`send`, `ready`, `key`, `input_resolve`, `input_cancel`, the gate, the writer, ack attribution, `IntakeProbe`, input-side attention and cards) is **PR 10b**, designed against the interfaces of §2.11.

**What 10a decides and leaves.** 10a owns the supervisor module of the daemon, the pure launch builder in `core`, one new entry point in `pty-runtime` (§2.4), and the CLI verbs above. It leaves to 10b everything that writes to a live PTY after `launch_started`; to PR 13 and PR 14 the hook configuration each CLI gets (which events, Claude's settings file, Codex's `-c hooks.*`), the payload decoders, and the verification of the argv rows on real binaries; to PR 16 agent tokens and worktree creation and removal; to PR 11 attach; to PR 18–19 the UI.

**Commitments from earlier PRs this design keeps.**

1. Death proof is pty-runtime's. `death_confirmed` becomes true only from `CloseOutcome.kill.tree_dead` of an explicit `Session::close` with the launch's full list, from a spawn error whose process provably never ran or whose cleanup is `Cleanup::Confirmed`, from the restart identity rule of §2.4 (its own method name, scoped to what it checks), or from an owner assertion (its own method name). No new census, no proof from a `Session` drop.
2. Every wait has a deadline through the daemon's helpers (`docs/design/deadlines.md`; `host::{act_before, observed_before}` on the synchronous side), with one stated exception (§2.10).
3. Thread state moves only through explicit events: hooks, the backend's exit report, and the supervisor's own commands (invariant 1). Nothing reads PTY text.
4. Public types change only where §1 lists a new name. `StateInput`, `InputInput`, `reduce`, `step`, and `gate` are used as they are.

Sources read for this document:

| Source | Revision | Read for |
|---|---|---|
| This repository | `cdf9524` | `AGENTS.md`; `docs/architecture.md`; `docs/mvp-design.md` §2.1–§2.11, §4, §6, §8; `docs/prior-art.md` rows 1a, 1b, 1d, 6c; `docs/design/{pty-runtime,pty-unix,pty-windows,daemon-lifecycle,http-surface,hook-ingest,event-store,formal-specs,deadlines,input-encoders}.md`; `crates/core/src/{state,adapter,input,payload,sync,ids,auth,http}.rs`, `crates/core/src/input/encode.rs`; `crates/pty-runtime/src/{lib,session,backend,process,spawn,fake,limits}.rs`, `host/mod.rs`, `unix/mod.rs`, `windows/mod.rs`; `crates/daemon/src/http/{commands,ctx,error,routes,auth,mod}.rs`, `lifecycle/{mod,gate}.rs`, `hook/{mod,adapter,buffer,route}.rs`, `store/{write_tx,readers,rows,project}.rs`, `clippy.toml`; `crates/cli/src/{args,daemon_cmd,lib}.rs`, `crates/cli/tests/hook_e2e.rs`; `crates/testkit/src/fake_cli/{mod,hooks}.rs`; `specs/thread_input.qnt`, `specs/thread_input_traces.qnt`; `scripts/windows-tests.ps1` |
| Win32 `CreateProcessW` | Microsoft Learn, read 2026-11-25 | the `lpCommandLine` limit of 32,767 characters (L-5) |
| Claude Code settings | `code.claude.com/docs/en/settings`, read 2026-11-25 | environment variables under the `CLAUDE_CODE_` prefix that select provider and behaviour (SD13) |

## 1. Goal and Dependencies

**Goal.** A CLI started by `chaldea spawn` or `chaldea resume` runs in a daemon-owned PTY with its own hook token and binding facts; each step of its life reaches the store as one explicit event in one job; every launch ends with `launch_exited`, and its death is confirmed exactly when pty-runtime (or a rule named by its method) can show it; a restart records every open launch and recovers every input before the listener exists; and no thread is relaunched until its previous launch's death is confirmed.

**Depends on** PR 3 and PR 4 (backends, `Session`), PR 5 (`capabilities`, `check`), PR 8 (`Components`, reconcilers, participants, `spawn_locked`), PR 9 (`POST /hook`, binding, `buffer`).

**Files.**

```
crates/core/src/adapter.rs            # HostOs gains serde/TS/JsonSchema and from_os_str
crates/core/src/launch.rs             # new: LaunchPlan, plan(), ENV_SCRUB, workspace_id() (pure)
crates/core/src/sync.rs               # AdaptersResponse, AdapterInfo
crates/core/src/command.rs            # new: SpawnArgs … CloseResult (wire types of §2.2)
crates/core/src/http.rs               # new ErrorCode variants (§2.2)
crates/pty-runtime/src/orphan.rs      # new: kill_recorded, OrphanReport (§2.4)
crates/pty-runtime/src/unix/orphan.rs # new: the unowned Os over the PR 3 engine
crates/pty-runtime/src/windows/orphan.rs  # new: verified handles, no job
crates/daemon/src/supervisor/         # new: mod, commands, live, exit, restart, tick, workspace, adapters
crates/daemon/src/http/commands.rs    # CommandHandler::revokes; the runner passes the nudge
crates/daemon/src/http/mod.rs         # StoreFatal::LaunchUnrecorded
crates/cli/src/agent_cmd.rs           # new: spawn, resume, stop, close, wait
crates/testkit/src/fake_cli/banner.rs # new FakeCli mode
crates/cli/tests/supervisor_e2e.rs    # new
```

**New identifiers** (everything else in backticks exists at `cdf9524`): `supervisor` (module), `Supervisor`, `SupervisorConfig`, `LiveLaunch`, `LiveLaunch::with_session`, `LiveLaunch::try_with_session`, `Started`, `ExitStep`, `ExitFacts`, `SessionGone`, `SessionBusy`, `launch::plan`, `LaunchPlan`, `ENV_SCRUB`, `launch::workspace_id`, `HostOs::from_os_str`, `AdaptersResponse`, `AdapterInfo`, `SpawnArgs`, `ResumeArgs`, `StopArgs`, `CloseArgs`, `SpawnResult`, `ResumeResult`, `StopResult`, `CloseResult`, `CommandHandler::revokes`, `orphan::kill_recorded`, `OrphanReport`, `StoreFatal::LaunchUnrecorded`, `supervisor::tick_timer`, `supervisor::workspace::common_dir`, `Supervisor::tick`, `LaunchInput`, the reconcilers `supervisor_launches` and `supervisor_buffers`, FakeCli mode `banner`, `CHALDEA_TEST_SPAWN_PAUSE`, the wire fields and values of §2.2 (`expected_launch_id`, `assume_dead`, `already_exited`) and `OrphanReport`'s `all_gone`, the error codes of §2.2's code table, the death methods `restart_identity` and `owner_asserted`, the Quint action `spawnStartedThenFailed` and the traces of §7, the files of the tree above, and every test name in §5.2. `CLAUDE_CODE_USE_BEDROCK` is a Claude Code variable (its settings page), used as a test input.

## 2. Design

### 2.1 Rules

1. **One job per transition.** Reservation, start, exit, death confirmation, and close are each one write job that re-reads the rows it decides on (thread, launch, worktree) and appends its events with the reducer's `status_changed`. A refusal that commits (resume's `resume_blocked`) is the command's stored result.
2. **One op lock per launch.** `LiveLaunch` holds the launch's `Session` in one async mutex. Every use of the `Session`, the recording of its spawn outcome, and the recording of its exit hold that lock (§2.11).
3. **The full list on every kill.** Every `Session::kill` and `Session::close` gets the launch's full recorded-descendant list; a report's survivors are never passed back (pty-windows §2.5 "The caller's obligation").
4. **Death from pty-runtime only** (commitment 1); a launch whose death no rule proves stays unresolved and blocks reservation.
5. **Startup causes for the current, live, unbound launch only**, decided in the job that raises them.

### 2.2 Commands

Registered through `CommandRegistry::register` in `Built::commands` (E1). The guard already decides kind and action per name (`crates/core/src/auth.rs` `MVP_COMMANDS`). Each handler's `prepare` parses the wire args (400 `bad_args` when they do not fit), mints ids and the hook token (`TokenService` `mint`, data only), and builds an intent closure from data only; the handler object (holding `Arc<Supervisor>`) is used only by `effect` (http-surface §2.8).

| Command | Args (new wire types) | Intent (the runner's job) | Effect | Success | Deadline |
|---|---|---|---|---|---|
| `spawn` | `SpawnArgs { agent, cwd, prompt?, task? }` | object authorization; returns `Pending` | §2.3 "Spawn": workspace probe, reservation job, `Session::spawn`, start job | `SpawnResult { thread_id, launch_id, input_id?, outcome: started \| spawn_failed }` | 30 s |
| `resume` | `ResumeArgs { thread_id, expected_launch_id, prompt? }` | the reservation of §2.3 (checks, events, token); refusals are `Completed` | `Session::spawn`, start job | `ResumeResult { launch_id, input_id?, outcome }` | 30 s |
| `stop` | `StopArgs { thread_id, assume_dead?: bool }` | authorization; `assume_dead` handled here (§2.4); a current launch exited with death confirmed → `Completed(already_exited)`; else `Pending` | §2.3 "Stop" | `StopResult { launch_id, outcome: stopped \| already_exited \| death_confirmed, death_confirmed }` | 30 s |
| `close` | `CloseArgs { thread_id, stop?: bool }` | without `stop`, or with `stop` and nothing live: the closing writes of §2.3 "Close" (`revokes()` is true); with `stop` and a live launch → `Pending` | stop, then the closing job (with the nudge) | `CloseResult { thread_id }` | 30 s |

`chaldea wait` is a CLI read loop, not a command (SD16).

**Prompt admission** (C1, D5 of mvp §8). A `prompt` is accepted only for Codex (Claude's first prompt is PTY input after readiness, a 10b `send`: until 10b, a Claude `prompt` is 400 `bad_args`). In `prepare`: text longer than `MAX_PROMPT_BYTES` → 400 `prompt_too_large`; `check(Codex, Positional, Purpose::Send, text)` → `Err` is 400 `prompt_rejected`; both before any receipt or event. The CLI runs the same `check` first to print the reason, since the error body carries only the code. A Codex launch without a prompt is a manual launch (`LaunchReserved.manual`).

**Codes.** New `ErrorCode` variants, each classified in `handler_code` at exactly one status (E2):

| Code (new) | Status | When | The work can happen later? |
|---|---|---|---|
| `thread_closed` | 409 | resume or close on a closed thread (a closed thread's launch is confirmed dead, so its stop is `already_exited`) | no |
| `launch_changed` | 409 | `expected_launch_id` is not the thread's current launch, or close `--stop` found another launch after its stop | no |
| `launch_live` | 409 | resume or close (without `stop`) while the current launch has not exited; `assume_dead` on a launch that has not exited | no |
| `death_unconfirmed` | 409 | resume or close while the current launch exited without confirmed death (resume also raises `resume_blocked` in the same job); stop of such a launch when no rule of §2.4 confirms it | no |
| `worktree_unavailable` | 409 | the launch's worktree is not `active` or `released` | no |
| `no_session` | 409 | resume of a thread with no bound `session_id` | no |
| `prompt_rejected` | 400 | `check` refused the positional prompt | no |
| `prompt_too_large` | 400 | the prompt exceeds `MAX_PROMPT_BYTES` (also 10b's admission code, C8) | no |
| `launch_busy` | 504 | stop could not take the launch's op lock within its wait (§2.3) | no: the stop is abandoned |

`assume_dead` from a token other than an owner token is 403 `forbidden`. Success types have no top-level `code` field; the contract drift test checks each (E2).

### 2.3 Launch Lifecycle

**Spawn** (effect, holding nothing at first):

1. *Workspace.* `supervisor::workspace::common_dir(cwd, now + 5 s)`: `git -C <cwd> rev-parse --path-format=absolute --git-common-dir` through `spawn_locked` (`CREATE_NO_WINDOW` on Windows), waited for by exact pid with `host::observed_before` polls; at the deadline the child is killed and waited for. Exit 0 → that path; anything else → the canonical cwd. `launch::workspace_id(path)` = `ws-` + the first 16 hex digits of SHA-256 of the path's UTF-8 bytes (mvp §2.1:103).
2. *Reservation job* (one job; §2.1 rule 1): `thread_registered`, `launch_reserved` (`expected_previous: None`, `argv_digest` of the planned argv, `manual`), the reducer's `status_changed` for `StateInput::Reserved { relaunch: false }`, the positional `input_created { initial_state: sending, encoding: positional, origin: cli }` when a prompt exists, and `token_insert` of the launch's hook token (kind `hook`, the thread and launch). Then the `LiveLaunch` is inserted with its op lock **taken** by this effect.
3. *Spawn.* `Session::spawn(runtime, &spec, config)` on `spawn_blocking`, awaited to its result (§2.10). `spec` is §2.7's plan; `config.on_event` forwards `SessionEvent::Exited` and `SessionEvent::WaitFailed` to the launch's exit task without blocking.
4. *Start job*, by outcome:
   - `Ok(session)` → `launch_started { pid, start_time, pgid_or_job, cli_version: None }` from `Session::identity`, `StateInput::Spawned`, and in the same job the positional input `sending → sent` (`InputInput::WriteCompleted`, `sent_after_seq` = this `launch_started`'s seq; mvp §2.3:186), then for an upgraded Codex launch the `startup_stall` raise (§2.6). After the commit the `Session` goes into the slot and the op lock is released; hooks waiting for the pid (hook-ingest §2.3) see the committed identity.
   - `Err(SpawnError::NeverStarted { .. })` → `launch_exited { reason: spawn_failed, death_confirmed: true }` (the program never ran and any process was reaped, `backend.rs:186-193`), the positional input `sending → rejected(spawn_failed)` (`InputInput::Rejected(RejectReason::SpawnFailed)`), and the exit steps of §2.11.
   - `Err(SpawnError::StartedThenFailed { cleanup, identity, .. })` → `launch_exited { reason: spawn_failed, death_confirmed: cleanup == Cleanup::Confirmed }`, the positional input `sending → unconfirmed` (`InputInput::LaunchExited`; the CLI may have read its argv) with `input_unconfirmed` raised, and the exit steps. `identity` goes to the daemon log only (L-4).
   - The executable not found on the daemon's `PATH` (§2.7) is recorded as `NeverStarted`.
5. A start job that cannot commit within the runner's completion budget (http-surface §2.8 step 3: 5 s attempts, backoff, 60 s admission) reports `StoreFatal::LaunchUnrecorded { launch_id }` through `BuildCtx::sink`; the daemon stops and restart reconciles (E5).

**Resume.** The intent job is the reservation of mvp §2.9: it reads the thread and its current launch and refuses, in this order, `thread_closed`, `launch_changed` (current ≠ `expected_launch_id`), `launch_live` (no `exit_reason`), `death_unconfirmed` (raising `resume_blocked` through the reducer when absent; mvp §2.9 "Daemon restart"), `worktree_unavailable` (the launch's `worktree_id`, if any, read by `ReadTx` `worktree`: only `active` and `released` pass; mvp CL-18), `no_session` (thread `session_id` is null). It then appends `launch_reserved` (`expected_previous` = that launch), `StateInput::Reserved { relaunch: true }`, the positional input, and the hook token, all before returning `Pending`. The effect runs steps 3–5 of Spawn. A resume and a close on one thread are decided by the store's single writer: each job re-reads `closed` and `current_launch_id`, so at most one succeeds (A4).

**Stop.** Effect:

1. Take the launch's op lock through `deadline::settle_before` with deadline = now + 15 s (a spawn in progress, or an exit close, holds it; X5). `Elapsed` → 504 `launch_busy`, nothing written.
2. Re-read the launch. Exited and confirmed → `already_exited`. Exited without confirmation → the rule of §2.4 for its exit reason. Running → job: `StateInput::StopRequested`.
3. `Session::kill(&extra, now + 5 s)` on `spawn_blocking` (mvp P-3's 5 s), then `Session::close(&extra, CloseBounds::default())`: close's step 2 reuses the first report only when it was `tree_dead` with an empty `extra`, and kills again otherwise (`session.rs:809`). The close runs whether the kill proved death or not.
4. The exit job of §2.11 with `reason: stopped`, `code` and `signal` from `SessionStatus.exit`, `death_confirmed` = `CloseOutcome.kill.tree_dead`; when true, `launch_death_confirmed { method: KillReport.method }` in the same job, so `death_method` names the proof.

**Natural exit.** The exit task receives `SessionEvent::Exited` or `SessionEvent::WaitFailed`, takes the op lock (§2.10), returns at once if the slot is empty or the launch already has an `exit_reason` (a stop recorded it), else runs Stop's steps 3–4 without the kill call, with `reason: exit` (or `stopped` when the reducer state is `stopping`). `WaitFailed` means wait ownership was lost: the close cannot report `tree_dead: true` after loss (pty-unix §2.5), so `death_confirmed` stays false (A25).

**Close.** The closing writes, in one job: refuse `thread_closed`, `launch_live`, `death_unconfirmed` as in Resume; else `thread_closed { by }`, `token_revoke(TokenSelector::Thread(t))`. Worktree hand-over (mvp §2.6) is PR 16's and `after_close` PR 20's, each added to this job. With `stop` and a live launch: Stop's effect, then the closing job, which also refuses `launch_changed` if a resume reserved a new launch meanwhile.

### 2.4 Death Proof

**While the daemon owns the session**, only the explicit close decides (§2.3), with the full list. The `Session` leaves the slot only into that `close`; a `Session` dropped elsewhere (a cancelled effect, a panic) closes with `&[]` and is never recorded as a proof (A27).

**`extra` and its source** (B1, A3, A26). The supervisor passes, on every call, every identity it can prove belongs to the launch: its own spawn records and containment it can show (hook-ingest D10). In the MVP no such identity exists outside what the backend already covers: job members (Windows) and session and group members observed while owned (macOS) are the backend's, the daemon creates no other process per launch, and PR 9 records none (hook-ingest D10). So the full list is the empty list on every call, retries included; a descendant that left the session before any observation is PR3-P-1's stated limit. A later producer records identities on the `LiveLaunch` and they are passed from then on.

**After a restart** (A1, F1, A28). The new daemon owns no old session: the job handle and the PTY master died with the old daemon, and on macOS the leader was reparented. pty-runtime's existing rules then allow only signals to verified identities and never `tree_dead: true` (pty-unix §2.5 "Ownership for the call", §4 row "Daemon restart"; pty-windows §4 row "Daemon restart"). 10a adds one entry point:

```rust
// crates/pty-runtime/src/orphan.rs (new)
pub struct OrphanReport {
    /// Every given identity is gone or its pid holds another process.
    pub all_gone: bool,
    pub survivors: Vec<ProcessIdentity>,
}
/// Signals each identity only after a lookup shows its recorded start, then rechecks
/// each by identity. Never signals a group or session, never reports a tree.
pub fn kill_recorded(identities: &[ProcessIdentity], deadline: Instant) -> OrphanReport;
```

- macOS: the PR 3 engine (`unix/kill.rs` `kill_tree`) with an `Os` whose `owned` always returns false (`unix/orphan.rs`), the first identity as the leader with its start and the rest as `extra`: exactly the ownership-lost mode (HUP, TERM, KILL to verified pids only; pty-unix §4 says the engine takes its `Os` and leader as parameters for this). Then one `process::lookup` per identity: `Lookup::Gone`, or `Lookup::Live` with another `start_time`, resolves it.
- Windows: per identity the backend's verified open (`OpenProcess` + `GetProcessTimes` on that handle), `TerminateProcess` on a held handle, a wait on it until the deadline; signalled, `Gone`, or another start resolves it (pty-windows §2.5 "Extras").

**The restart identity rule.** A launch with `exit_reason = daemon_restart` and an identity recorded by `launch_started` gets `launch_death_confirmed { method: "restart_identity" }` when `kill_recorded([leader] ++ extra, now + 5 s).all_gone`. It is a narrower statement than `tree_dead`: it covers the recorded identities, not the unrecorded members of the old tree (L-1, L-2). The rule is applied by the restart prober once after the listener starts (§2.5), and by `stop` on such a launch. It is never applied to a launch whose `close` ran and failed (`exit` or `stopped` with `death_confirmed: false`): that close saw more than the identities, and the narrower rule would confirm what it could not (L-7).

**Owner assertion** (A2, A29). A launch without a recorded identity (`unknown_after_crash`, `spawn_failed` from `StartedThenFailed`) or one no rule confirms can be released only by the owner: `chaldea stop <thread> --assume-dead` (owner token) on a current launch that exited with death unconfirmed appends `launch_death_confirmed { method: "owner_asserted" }` and `StateInput::DeathConfirmed` (clearing `resume_blocked`) in the intent job.

### 2.5 Restart

**Reconcilers** (E3; daemon-lifecycle §2.14: each its own job, at step 9, before the listener exists), returned by `Components::reconcilers` in this order:

1. `receipts_mark_unknown_after_crash` (PR 8, unchanged).
2. `supervisor_launches` (new), one job: for each `open_launches()` row with `exit_reason` null, in `(thread_id, reserved_seq)` order: `launch_exited { reason: daemon_restart` if `pid` is recorded, else `unknown_after_crash`; `code`, `signal` null; `death_confirmed: false }` with `StateInput::Exited`, then the exit steps of §2.11 with cause `restart` (inputs of that launch: `queued → cancelled`, `sending`/`sent → unconfirmed` through `InputInput::DaemonRestarted`, raising `input_unconfirmed`; `buffer::resolve_unbindable`), then `token_revoke(TokenSelector::Launch(l))`. After the launches, every remaining non-terminal input of any thread moves the same way (mvp §2.4 "Restart": inputs are recovered from the `inputs` table). Launches first, then inputs, in one job.
3. `supervisor_buffers` (new), one job: `buffer::resolve_unbindable` for each `buffered_launches()` (A22; every launch is exited by now, so none can bind). It keeps daemon-lifecycle's order "launches, then buffered deliveries" and covers buffers left by launches that exited in an earlier run.

No nudge is needed here: no socket exists before the listener. The positional input of a launch that crashed before `launch_started` ends `unconfirmed` through step 2 (A21).

**Restart prober** (after `Components::attach`, a task): for each launch with `exit_reason = daemon_restart`, `death_confirmed` false, and an identity, `kill_recorded` on a blocking thread awaited by `settle_before` (deadline + 1 s), then on `all_gone` a job that re-reads the launch and appends `launch_death_confirmed { method: "restart_identity" }` with `StateInput::DeathConfirmed`. Launches are probed concurrently, each bounded by its own 5 s.

### 2.6 Startup Causes and the Tick

One supervisor task ticks every second; its timer is the new exception `supervisor::tick_timer` (`sleep_until`), and the decision uses the monotonic instants the supervisor holds, never store stamps. `Supervisor::tick(now)` is the seam tests drive with an injected instant. Per tick, for each live launch:

- **`session_start_timeout`** (D4, A32): when `now ≥ started_at + 20 s` (mvp §2.2:142) for a non-manual launch, a job that re-reads and raises `AttentionCause::SessionStartTimeout` only if the launch is the thread's current launch, has no `exit_reason`, has no `session_id` (unbound), and the cause is absent. The condition is "unbound", not "no `SessionStart` seen": a `SessionStart` whose walk ends below the launch is `foreign_session` and leaves the launch unbound, and hook-ingest §2.5 (line 268) assigns reporting it to this cause (Quint change in §7).
- **`buffer::expire`** for each live, unbound launch with buffered deliveries (A22).

**`startup_stall`** (A32) is raised in the start job for a Codex launch whose plan says `upgraded`; `upgraded` is false until PR 14 supplies the CLI-version comparison (mvp §2.7 "Hook trust"). Both causes are cleared by the reducer on `StateInput::SessionBound` and `StateInput::Exited`. Hole c (formal-specs §3) is closed by the job-side guards: nothing raises a startup cause for an exited or non-current launch.

### 2.7 Launch Plan: argv, Environment, Token

`launch::plan(&LaunchInput) -> LaunchPlan` (pure, `core`) builds the arguments after the program from mvp §2.7's rows "Session binding", "First prompt", "Extra flags", and "Resume":

| Launch | Claude | Codex |
|---|---|---|
| Fresh | `--session-id <launch_id>` (lowercase hyphenated, as `MvpHooks` `expected_session` computes) | `--no-daemon` [Windows: `-c tui.disable_paste_burst=true`] [`--` `<prompt>`] |
| Resume | `--resume <thread session_id>` | `resume` `--no-daemon` [Windows flag] `<session_id>` [`--` `<prompt>`] |

The prompt is one argv element after a `--` that ends the argv (input-encoders D5). The "Hook injection" row (Claude `--settings`, Codex `-c hooks.<Event>`) is PR 13's and PR 14's addition to `plan` (SD14). `manual` = Codex without a prompt.

**Program.** The daemon resolves `claude` or `codex` on its own `PATH` to an absolute path of a native executable (`SpawnSpec` requires one, `backend.rs:34`); a missing one is `NeverStarted` (§2.3). Resolving a Windows npm shim to its native binary is PR 14's. Tests replace the program through `SupervisorConfig` with `chaldea-fake-cli <mode flags> --`, so FakeCli ignores the adapter arguments and `report-process` records them.

**Environment** (`SpawnSpec.env` is complete; nothing is inherited): the daemon's environment minus `ENV_SCRUB`, plus `CHALDEA_HOOK_URL` (the daemon's `POST /hook` on its port), `CHALDEA_HOOK_TOKEN` (this launch's hook token), `CHALDEA_THREAD_ID`, and `TERM=xterm-256color` on macOS. `ENV_SCRUB` is the identity variables of mvp §2.7 "Env scrub", by name: `CLAUDECODE`, `CLAUDE_CODE_CHILD_SESSION`, `CLAUDE_CODE_SESSION_ID`, `CLAUDE_CODE_MESSAGING_TOKEN`, `CODEX_THREAD_ID`, `CODEX_SESSION_ID`, and every `CHALDEA_*` (prior-art row 6c; SD13).

**Token.** One hook token per launch, inserted by the reservation job (D2); its validity follows hook-ingest §2.8 (not revoked at exit, so late hooks are kept; revoked at restart and at close). Agent tokens are PR 16's, added to the same job.

### 2.8 `GET /api/adapters` and the Host OS

`HostOs::from_os_str(std::env::consts::OS)` maps `"macos"` and `"windows"`; anything else is `None`, and `chaldea daemon run` exits `Failed` before startup with a message (C7). The value is computed once, held by the `Supervisor`, and passed by value into every `prepare`.

The route (`Mount::read`, `ReadAdapters`, Bearer owner, agent, rescue) answers from `CAPABILITIES` with no store read (C6; SD5):

```rust
// crates/core/src/sync.rs (new types)
pub struct AdaptersResponse {
    pub host_os: Open<HostOs>,          // "macos" | "windows"
    pub adapters: Vec<AdapterInfo>,     // this host's rows, in CAPABILITIES order
}
pub struct AdapterInfo {
    pub agent: Open<Agent>,
    pub pty_encoding: Open<InputEncoding>,   // "bracketed_paste" | "win_keys"
    pub first_prompt: Open<InputEncoding>,   // "bracketed_paste" | "win_keys" | "positional"
}
```

Later columns (readiness mode, resume support) are added as fields by PR 13 and PR 14. `submit_delay` stays internal.

### 2.9 Shutdown and Fatal Outcomes

The supervisor's `ShutdownParticipant` (E5) runs after HTTP has drained (so no command and no effect is running) and before the store: it stops the tick and the restart prober and returns by its `end`. It does not kill sessions: process exit ends them (macOS: the PTY master closes and the session's controlling process gets the hangup; Windows: the job handle closes and `KILL_ON_JOB_CLOSE` terminates its members), and the next start records them as `daemon_restart` and confirms them by §2.4's rule (SD10). An exit task whose job fails because the store is closing drops it; restart reconciles.

Fatal outcomes: a launch transition that cannot commit reports `StoreFatal::LaunchUnrecorded` through `BuildCtx::sink`; nothing else in the supervisor stops the daemon. Children: the PTY children through the backend (which takes the spawn lock), git through `spawn_locked`; no other process is created (A25). The CLI verbs use `cli::client` (`Discovery::read`, `read_owner_token`, `request`).

### 2.10 Waits

| Wait | Helper | Bound |
|---|---|---|
| Op lock (stop, exit task) | `deadline::settle_before` on the lock future | 15 s; `Elapsed` → `launch_busy` (stop); the exit task waits again in 15 s rounds until it holds the lock, since every holder's work is bounded (§2.11) |
| `Session::kill`, `Session::close`, `kill_recorded` on `spawn_blocking` | `deadline::settle_before` on the join handle, deadline = their own bound + 1 s | their own deadlines (pty-runtime §5) |
| Store jobs | `run_write` with `not_after`; completion-style retries through `commands::backoff_until` | http-surface §2.8 step 3 |
| Git probe | `host::observed_before` polls of `try_wait`, then kill and wait | 5 s |
| Tick | `supervisor::tick_timer` (new exception, `sleep_until`) | 1 s period |
| Restart prober | as `kill_recorded` | 5 s + 1 s per launch |
| CLI retries and `wait` | `poll_until` | the CLI's `--timeout` |
| **`Session::spawn`** | **none** | Cancelling it cannot stop it: the blocking call would still create the child, which nothing would then record. Its bound is the spawn lock's holders, each of which does bounded work (pty-unix:122; pty-windows §2.2 "`spawn` has no deadline of its own"). The request itself still replies at its deadline through the runner (409 `in_progress`). |

### 2.11 Interfaces for 10b

```rust
// crates/daemon/src/supervisor/live.rs (new)
pub(crate) struct LiveLaunch {
    pub launch_id: LaunchId,
    pub thread_id: ThreadId,
    pub agent: Agent,
    pub caps: &'static Capabilities,
    started: OnceLock<Started>,                 // set after the start job committed
    ops: Arc<tokio::sync::Mutex<Option<Session>>>,
}
pub(crate) struct Started { pub seq: u64, pub identity: ProcessIdentity, pub at: std::time::Instant }

impl LiveLaunch {
    /// Runs `f` on a blocking thread holding the op lock; `SessionGone` once the
    /// session left the slot (exit recorded or being recorded).
    pub(crate) async fn with_session<R: Send + 'static>(
        &self, deadline: Instant, f: impl FnOnce(&Session) -> R + Send + 'static,
    ) -> Result<R, SessionGone>;
    /// Non-blocking: `SessionBusy` while another holder has the lock.
    pub(crate) fn try_with_session<R>(&self, f: impl FnOnce(&Session) -> R)
        -> Result<R, SessionBusy>;
    pub(crate) fn started(&self) -> Option<&Started>;
}

/// One step of the exit job, in registration order.
pub(crate) trait ExitStep: Send + Sync {
    fn in_exit_job(&self, tx: &mut WriteTx<'_>, exit: &ExitFacts) -> Result<(), StoreError>;
    fn after_commit(&self, _exit: &ExitFacts) {}
}
pub(crate) struct ExitFacts {
    pub thread_id: ThreadId, pub launch_id: LaunchId, pub reason: ExitReason,
    pub death_confirmed: bool, pub exited_seq: u64, pub restart: bool,
}
```

- **Access rule.** The supervisor keeps `thread_id → Arc<LiveLaunch>` for current launches under a `std::sync::Mutex` that is never held across an await, a lock, or a job. Every use of a `Session` goes through `with_session` or `try_with_session`; no code keeps a `&Session` or the guard after `f` returns. 10b submits a unit inside `f` and awaits the `CompletionReceiver` outside it. A hot read on the hook path (`IntakeProbe`) uses `try_with_session` and treats `SessionBusy` as unknown.
- **Lock holders.** The spawn effect (reservation commit → start job commit), stop (lock → exit job commit), the exit task (lock → exit job commit), and 10b's calls. Order: the map mutex, then `ops`; a store job may run while `ops` is held, never the other way round.
- **Exit job order** (the hook points 10b adds to): `launch_exited` with the reducer's `Exited`, then the registered `ExitStep`s: (1) 10a's input recovery (mvp CL-6: `queued → cancelled`, `sending`/`sent → unconfirmed` with `input_unconfirmed`, `InputInput::LaunchExited` or `InputInput::DaemonRestarted` when `restart`); (2) 10a's `buffer::resolve_unbindable`; (3) 10b's gate state; (4) PR 13's card expiry (`expired`). The job passes the nudge as `after_ok` (it ends the launch's agent-token validity, PR 16). After commit the `after_commit` hooks run in order (10b: drop the launch's writer queue). A writer completion that arrives after `launch_exited` is 10b's to ignore in its own job (`exit_reason` not null).
- **Restart** calls the same steps with `restart: true` (§2.5); 10b adds only its gate state there.
- **Stop and close** cancel queued inputs through step (1); 10b adds gate closing as its step.

## 3. Decisions

Decisions are numbered SD (supervisor decision), apart from the obligation ids of §4 such as D2 and D4. Each compares options by gain, loss, reversibility, and cost. "Public" marks a decision visible to users or clients.

**SD1. Nudge for revoking intents (E4 vs X1).** http-surface §2.4 says command PRs pass the revocation nudge as `after_ok` to `run_write`, but a command's intent job is the runner's (`commands.rs:665` passes `None`) and `Prepared` has no `after_ok`.

| Option | Gain | Loss | Reversibility | Cost |
|---|---|---|---|---|
| **A. `CommandHandler::revokes() -> bool` (default false); the runner passes `Shared::nudge` as `after_ok` for that handler's intent job** | one-transaction close; one line in the runner; PR 16 and PR 20 reuse it | a nudge also after a refusal or a replay (harmless: "the nudge carries no decision; the database does", http-surface §2.12) | trait default; removable | small |
| B. `Prepared` gains `after_ok` | per-call choice | `prepare` needs the server's nudge handle, which it does not have before `attach` | additive | small |
| C. close always `Pending`, revoking in the effect's own job | no runner change | two jobs: a resume can slip between check and close, so the closing job must re-check; a crash leaves `unknown_after_crash` for a plain close | reversible | medium |
| D. rely on the socket's periodic check | none | breaks http-surface §2.4 | — | — |

**Adopted: A.** The supervisor's own revoking jobs (close `--stop`'s closing job, the exit job) pass the nudge themselves; the restart reconciler needs none.

**SD2. One crash-window rule for A2 and A29.** A2 (crash between `launch_reserved` and `launch_started`) and A29 (Windows crash between a suspended `CreateProcessW` and `AssignProcessToJobObject`) leave the same record: a reserved launch with no identity, recorded as `unknown_after_crash`. A29 is its Windows instance with one extra property: the suspended child never runs and sits outside any job, holding a pid and its image (pty-windows §2.2 "Crash window"). The job-list branch has no such window.

| Option | Gain | Loss | Reversibility | Cost |
|---|---|---|---|---|
| **A. No automatic confirmation; owner assertion (`stop --assume-dead`); detection of an A29 orphan is PR 21's doctor** | promises nothing the record cannot show | the owner must act once per such launch (rare: a crash inside one store write) | the doctor can add a finder later | small |
| B. Census by image name and creation time after restart | finds candidates | a match by image and time proves no provenance (pty-windows §2.5 "Extras are trusted input"), so it can neither confirm death nor safely kill | — | large |
| C. Durable pre-spawn record | none: the pid is unknown before the child exists | — | — | — |

**Adopted: A.** Public: a new `stop` flag and death method `owner_asserted`.

**SD3. Restart death proof and `restart_now` (A1, F1).** mvp §4 asks for a restart scan of the launch's session by recorded sid with `launch.start_time ≤ start ≤ restart_now`.

| Option | Gain | Loss | Reversibility | Cost |
|---|---|---|---|---|
| A. The sid scan with a start window | covers session members | start stamps are wall-clock stamps used only for equality (pty-unix §2.1 "Identity format"; pty-windows §2.1); without ownership, sid `L` can name a later session once `L` is reused, so a clean scan proves nothing: pty-unix's census is sound only while the daemon owns the leader (pty-unix §2.4 "Ownership is a precondition") | — | large |
| **B. The identity rule of §2.4, `restart_identity`** | uses only the unowned mode pty-unix §4 already defines and the Windows rule pty-windows §4 states; no clock | unrecorded members of the old tree are not checked (L-1, L-2) | a later census-free source of identities widens it | small |
| C. Never confirm after a restart | strongest statement | every restart blocks every resume until an owner assertion, contradicting mvp P-18's "a resume shall continue the session" | — | none |

**Adopted: B.** `restart_now` is not needed. If A were chosen, `restart_now` would be `SystemTime::now()` sampled once at the start of the reconciler job, on the same clock as `start_time`. Public: P-18's limit widens (§8).

**SD4. Stop while starting (X5).** `state.rs:255-258` says the daemon holds such a stop until the spawn outcome is recorded.

| Option | Gain | Loss | Reversibility | Cost |
|---|---|---|---|---|
| **A. The stop effect waits on the launch's op lock (15 s), then decides on the recorded outcome** | matches the reducer's rule with existing parts | a stop may wait for a slow spawn | — | small |
| B. A new 409 code; the client retries | no wait | every client (CLI, PWA, Rescue button) repeats the retry | additive | small |
| C. A queued stop applied by the start job | no waiting request | cross-call state | — | medium |

**Adopted: A.** A lock not taken in 15 s is 504 `launch_busy` and the stop does not happen later.

**SD5. `GET /api/adapters` wire shape (C6). Public.**

| Option | Gain | Loss | Reversibility | Cost |
|---|---|---|---|---|
| **A. `AdaptersResponse { host_os, adapters: [AdapterInfo { agent, pty_encoding, first_prompt }] }`, this host's rows, open enums** | small; every field exists in `CAPABILITIES`; additive later | clients learn other hosts' rows only from those hosts | fields and rows can be added | small |
| B. All four rows | one static catalog everywhere | rows a client of this host cannot use; removing them later breaks clients | — | small |
| C. The whole mvp §2.7 table as fields | complete | most columns are PR 12–14's and unimplemented | — | large |
| D. A free-form map | no schema churn | no contract for clients | — | small |

**Adopted: A.** What would change it: a client that must plan launches for another OS before contacting that host.

**SD6. Who attributes `extra` (B1, mvp:437 "PR 9 / 10").** hook-ingest D10 gives the obligation to PR 10 entirely and PR 9 records none. Adopted: PR 10; in the MVP the list is empty (§2.4). Alternative rejected: recording hook-emitter ancestry as `extra` (a matching creation time identifies a process, not its provenance; pty-windows §2.5 "Extras are trusted input").

**SD7. Restart reconciler order.** Adopted: receipts → supervisor launches (with inputs and tokens) → supervisor buffers (§2.5). Alternatives: inputs in a separate job after launches (a crash between them leaves exited launches with inputs still `sending`: the next restart fixes it, but one job is simpler); buffers before launches (rejected by daemon-lifecycle §2.14: the launches step decides which launches can no longer bind).

**SD8. Exit hook points for 10b.** Adopted: the ordered `ExitStep` list inside the one exit job (§2.11). Alternatives: events 10b subscribes to after commit (a gap between `launch_exited` and 10b's state, which mvp CL-6 forbids for inputs); 10b writing its own exit job (two exit records to order).

**SD9. The positional input without 10b.** Adopted: created `sending` with the reservation; `sent` with `sent_after_seq` = `launch_started`'s seq in the start job; `rejected(spawn_failed)` on `NeverStarted`; `unconfirmed` on `StartedThenFailed`, exit, or restart. 10a has no ack, so a launched positional input ends `unconfirmed` at exit until 10b attributes its ack (mvp §2.3). Alternative rejected: leaving it `sending` until 10b (mvp CL-6 forbids an input still in flight after exit).

**SD10. Sessions at shutdown.** Adopted: left to process exit and the restart path (crash-only recovery, AGENTS.md "PTY Seam"). Alternative: kill every session within the participants' 2 s share and record `stopped` (clean records, but every session and participant splits that share, a tree not dead by its end still needs the restart path, and that path must exist for crashes anyway). Public: after `chaldea daemon stop`, running threads read `daemon_restart` on the next start.

**SD11. Spawn reserves inside its effect.** `thread_registered` needs a `workspace_id`, derived with git (mvp §2.1:103), and `prepare` and the intent cannot run a child. Adopted: the spawn intent writes only the receipt; the effect probes, then reserves. Resume, which needs no probe, reserves in its intent. Alternative: reading `.git` files instead of running git (re-implements git's discovery; rejected).

**SD12. Session access.** Adopted: one async op lock holding `Option<Session>` (§2.11). Alternative: `Arc<Session>` clones (no lock wait, but `close` consumes the `Session`, so the closer would wait for every clone to drop: cross-call state and a second wait).

**SD13. Env scrub by name. Public.** mvp §2.7 lists the prefixes `CLAUDE_CODE_*` and "other `CODEX_*` session variables". Both prefixes also carry user configuration (Claude Code's settings page lists `CLAUDE_CODE_*` variables that select the provider and behaviour; `CODEX_HOME` selects Codex's home); dropping them changes how the user's CLI runs, against mvp P-1 (the CLI runs with the user's own configuration). Adopted: the named identity variables of prior-art row 6c plus `CODEX_SESSION_ID` and every `CHALDEA_*`; a new identity variable is a list entry (PR 13/14 per CLI version).

**SD14. Hook configuration stays PR 13's and PR 14's.** 10a supplies the token, the env, and the Claude session flag; which events each CLI reports is the adapters' (mvp §4 rows 13, 14; Codex's trust hash makes the event list part of the first release, mvp §2.7). Consequence: a real CLI launched by 10a alone never binds (L-6).

**SD15. No spike watchdog (F2).** The emergency kill is `chaldea stop` (and the Rescue button, PR 20, which calls it). A watchdog killing on CPU or time would decide orchestration in daemon code (invariant 2).

**SD16. `chaldea wait`. Public.** `chaldea wait <thread> [--until exited|idle|attention] [--timeout <dur>]` (default `exited`, 10 min): polls `GET /api/threads` through `poll_until` every 500 ms, prints the thread's `ThreadInfo` as JSON when the condition holds (level-triggered: a condition already true returns at once), exits 0, or 3 at the timeout. Alternative: an edge-triggered wait on the event stream (`--after <seq>`), left for 10b's `send` then `wait` pattern.

## 4. Obligation Inventory

Obligation ids number the PR 10 obligations collected from the sources in the second column; 10a and 10b share them.

| Id | Source | 10a's answer |
|---|---|---|
| A1 | mvp:439; pty-unix:409 | §2.4 restart identity rule; SD3; PR10a-CL-17 |
| A2 | mvp:439 | §2.4 owner assertion; SD2; PR10a-CL-8 |
| A3 | mvp:439 | §2.4 "`extra` and its source"; PR10a-CL-10 |
| A4 | mvp:439 | §2.3 Resume, Close; PR10a-CL-4 |
| A5 | mvp:439; mvp CL-18 | §2.3 Resume; PR10a-CL-3 (removal race: PR 16) |
| A6 | mvp:439; mvp CL-8 | own commands: PR10a-CL-5 |
| A13 | mvp:439; mvp CL-6 | §2.5, §2.11 exit steps; PR10a-CL-14 |
| A21 | mvp:439 | §2.5; PR10a-CL-8 |
| A22 | mvp:439; hook-ingest:381,565 | §2.5, §2.6, §2.11; PR10a-CL-15 |
| A24 | mvp:439; mvp CL-1 | §6 FakeCli `banner`; PR10a-CL-19 |
| A25 | mvp:439; daemon-lifecycle §2.11 | §2.3 Natural exit, §2.9; PR10a-CL-20 |
| A26 | mvp:439; pty-windows:202 | §2.1 rule 3, §2.4; PR10a-CL-10 |
| A27 | mvp:439 | §2.3 Stop, §2.4; PR10a-CL-10 |
| A28 | mvp:439; pty-windows:412 | §2.4 restart identity rule; PR10a-CL-17 |
| A29 | mvp:439; pty-windows:129,465 | SD2; PR10a-CL-8 (its record); L-3 |
| A30 | mvp:439; formal-specs §3 hole a | §2.3 Resume; PR10a-CL-3; §7 |
| A32 | mvp:439; formal-specs §3 hole c | §2.6; PR10a-CL-18; §7 |
| B1 | mvp:437; hook-ingest D10 | §2.4; SD6; PR10a-CL-10 |
| C1 | input-encoders:331,179 | §2.2 Prompt admission, §2.7; PR10a-CL-1, PR10a-CL-2 |
| C6 | input-encoders:319 and its D3; http-surface:461,471 | §2.8; SD5; PR10a-CL-25 |
| C7 | input-encoders:114 | §2.8; PR10a-CL-2 |
| D2 | hook-ingest:565,352 | §2.7, §2.3; PR10a-CL-6, PR10a-CL-7 |
| D4 | hook-ingest:268,565 | §2.6; PR10a-CL-18; §7 |
| E1 | http-surface:582,703 | §2.2 (`spawn`, `resume`, `stop`, `close`); PR10a-CL-22 |
| E2 | http-surface:505-531 | §2.2 codes; PR10a-CL-22 |
| E3 | http-surface:337; daemon-lifecycle:622 | §2.5; SD7; PR10a-CL-16 |
| E4, X1 | http-surface:333; `commands.rs:665` | SD1; PR10a-CL-21 |
| E5 | daemon-lifecycle:625-628 | §2.9; PR10a-CL-23 |
| E6 | daemon-lifecycle PR8-P-3b | PR10a-CL-24 |
| F1 | pty-unix:409 | §2.4 `orphan::kill_recorded`; PR10a-CL-17 |
| F2 | pty-unix:417 | SD15; PR10a-CL-11 (the emergency kill is `stop`) |
| X5 | `state.rs:255-258` | §2.3 Stop; SD4; PR10a-CL-12 |
| A38 | formal-specs:192 | own guards: §7; PR10a-CL-29 |

**Handed on.** A7–A12, A14–A20, A23, A31, A33–A37, B2, B3, C2–C5, C8, D1, D3, F3–F5, X2–X4: PR 10b (the send and ack path; 10a defines `prompt_too_large` for C8's reuse). A5's removal race: PR 16 (it owns removal; 10a's reservation check is the reading half). Hook configuration per CLI: PR 13, PR 14 (SD14). A29's orphan detection: PR 21 (SD2).

## 5. Purpose Layer and Plan Layer

### 5.1 Purpose Layer: Acceptance Criteria

Real-CLI checks run on macOS with `chaldea daemon run` and the installed `claude` and `codex`; Windows is covered by CI with FakeCli (`windows-latest`). Until PR 13 and PR 14, real CLIs launch without hook configuration (L-6).

| ID | Criterion (EARS) | Source | Scene | Observable result / failure | How to check on the real CLI |
|---|---|---|---|---|---|
| PR10a-P-1 | When the owner runs `chaldea spawn --agent <claude\|codex> [--prompt <text>]`, the CLI shall run in a daemon-owned PTY with this launch's hook token and thread id in its environment, the identity variables scrubbed, and a Codex prompt as one argument after `--`. | mvp §2.7, §2.9; mvp P-1, P-2 | fresh thread, trusted repository | Pass: `SpawnResult.outcome = started`; thread `running`; argv and env as stated. Fail: missing or extra variables; prompt split or before `--`. | `ps -o pid,sid,args -p <pid>`; `ps eww -p <pid>` lists `CHALDEA_HOOK_*` and no `CLAUDECODE`; the Codex TUI shows the prompt submitted (`codex`'s own session log names it). |
| PR10a-P-2 | When `chaldea stop` is run for a running thread, no process of its tree shall be alive at the command's reply, the launch shall be `exited` (`stopped`) with death confirmed, and an unrelated process shall survive. | mvp P-3; pty-unix PR3-P-1 | a CLI with a child process (an MCP server, or a shell command it started) | Pass: `death_confirmed: true`, `pgrep -s <sid>` empty, a sleeper started from another terminal alive. Fail: survivor; unrelated killed. | As stated; Codex and Claude each. |
| PR10a-P-3 | When the CLI exits by itself, the launch shall be `exited` (`exit`) with its code, and death confirmed when the close proves it. | mvp §2.9 | `/exit` in the TUI (through PR 11's attach, or a CLI that exits on its own) | Pass: one `launch_exited`; `code` set. Fail: two exits; `stopped`. | `chaldea wait <thread> --until exited` returns the thread JSON. |
| PR10a-P-4 | If the daemon is killed and restarted, then before the first command is served every open launch shall be `exited` (`daemon_restart` or `unknown_after_crash`) with death unconfirmed, every in-flight input recovered, and tokens revoked; and once the old leader is gone, death shall be confirmed with `restart_identity`. | mvp §2.9; mvp P-18 | kill the daemon by PID mid-run | Pass: the first `GET /api/threads` after restart shows `exited`; `resume` works after `launch_death_confirmed`. Fail: a `running` thread after restart; resume before confirmation. | `kill -9 <daemon pid>`; restart; read threads; `chaldea resume` (FakeCli until PR 13/14, L-6). |
| PR10a-P-5 | If a resume is requested while the current launch's death is unconfirmed, the thread is closed, the expected launch is not current, or the worktree is not `active`/`released`, then no launch shall be reserved and the code shall name the reason; a `death_unconfirmed` refusal shall raise `resume_blocked`. | mvp §2.9; mvp CL-18 | each condition | Pass: 409 with the code; no `launch_reserved`. Fail: a reservation. | FakeCli (worktree rows written directly until PR 16). |
| PR10a-P-6 | When the program cannot be started, the thread shall show `exited` (`spawn_failed`), and a positional prompt shall be `rejected` (`spawn_failed`) when the program never ran, or `unconfirmed` when it may have. | mvp §2.3:186 | `PATH` without `codex` | Pass: as stated. Fail: input `sending` forever; a thread `running`. | `PATH=/usr/bin chaldea daemon run`, then `chaldea spawn --agent codex --prompt hi`. |
| PR10a-P-7 | When a client reads `GET /api/adapters`, it shall receive this host's rows of the capability table. | mvp §2.7 | owner token | Pass: equal to `CAPABILITIES` for the host. Fail: other host's rows; missing agent. | `curl -H "Authorization: Bearer $T" http://127.0.0.1:$P/api/adapters`. |
| PR10a-P-8 | While a non-manual launch is unbound 20 s after `launch_started`, the thread shall carry `session_start_timeout` until it binds or exits. | mvp §2.2:142; hook-ingest:268 | a CLI that never sends `SessionStart` (every real CLI until PR 13/14) | Pass: the cause after 20 s, cleared at exit. Fail: absent; kept after exit. | Spawn Claude; read the thread after 21 s; stop it. |
| PR10a-P-9 | When the owner closes a thread whose launch exited with death confirmed (or with `--stop`), the thread shall be closed, its tokens revoked, and later resumes refused. | mvp §2.11 | idle thread | Pass: `closed: true`; resume → `thread_closed`. Fail: a resume succeeds. | `chaldea close <thread> --stop`; `chaldea resume <thread>`. |
| PR10a-P-10 | No text a CLI prints shall change thread state. | invariant 1; mvp CL-1 | FakeCli `banner` | Pass: no event beyond those from hooks and the process. Fail: any state change from output. | Not a real-CLI check (PR10a-CL-19). |

### 5.2 Plan Layer: Claims

Tests named `supervisor::…` are daemon unit tests under `crates/daemon/src/supervisor/tests/` with `FakePty`, the in-process store, and the HTTP server (the `crates/cli/tests/hook_e2e.rs` pattern); `supervisor_e2e::…` are in `crates/cli/tests/supervisor_e2e.rs` with the native runtime and FakeCli; `launch::…` are `core` unit tests.

| ID | Claim (EARS) | Source | Test that falsifies it | Mutation it kills |
|---|---|---|---|---|
| PR10a-CL-1 | When `spawn` or `resume` carries a prompt, the handler shall refuse a Claude prompt (`bad_args`), a text over `MAX_PROMPT_BYTES` (`prompt_too_large`), and any text `check(Codex, Positional, Send, …)` rejects (`prompt_rejected`) before any receipt or event; an accepted prompt shall be the argv's last element, after a `--`. | C1; input-encoders D5 | `supervisor::commands::prompt_admission` (each case: status, code, `FakePty::spawns()` empty, host head unchanged; accepted: `spawns()[0].argv` ends with `["--", text]`); `launch::codex_argv` (prompts `--version`, `-c x`, text with spaces and LF) | `check` skipped or run after the reservation; the prompt split on whitespace or placed before `--`; the size check after `check` |
| PR10a-CL-2 | The capability row and every OS-dependent argument shall come from `(agent, host_os)` with `host_os` mapped once from `std::env::consts::OS`; an unmapped OS shall stop `daemon run` before startup. | C7 | `adapter::host_os_from_str` (`macos`, `windows`, `linux`, empty); `supervisor::launch::windows_row` (a `Supervisor` built with `HostOs::Windows` on any runner: Codex argv has `-c tui.disable_paste_burst=true`) | `cfg!(windows)` instead of the mapped value; Macos hard-coded; unmapped OS accepted |
| PR10a-CL-3 | The reservation shall succeed only against the expected current launch, on a thread not closed, after the previous launch exited with death confirmed, and with its worktree `active` or `released`; each refusal shall write nothing except the `resume_blocked` raise of `death_unconfirmed`. | A30, A5; mvp CL-18; mvp §2.9 | `supervisor::reserve::refusals` (one condition at a time; worktree rows `creating`, `removing`, `uncertain`, `removed`, `failed` written directly; asserts code, events, `launch_reserved` absent); trace `trace_hole_a_reserve_without_death` replays unchanged | death check dropped (hole a); `creating` accepted; `expected_launch_id` ignored; `resume_blocked` not raised or raised twice |
| PR10a-CL-4 | Of a `resume` and a `close` of one thread, at most one shall succeed, and the other shall answer `thread_closed` or `launch_live`. | A4 | `supervisor::race::resume_close` (both orders, then both concurrent through a parked writer; asserts exactly one success and the thread's final state) | close not re-reading `current_launch_id`; resume not re-reading `closed` |
| PR10a-CL-5 | Two concurrent requests with one `command_id` to `spawn`, `resume`, `stop`, or `close` shall cause one effect. | A6; mvp CL-8 | `supervisor::commands::duplicates_write_once` (per command, two requests released together; one `launch_reserved`, `FakePty::spawns().len() == 1`, one `FakeEvent::Kill` series) | the effect started per request; a receipt bypass for the spawn effect's reservation job |
| PR10a-CL-6 | The reservation job shall insert exactly one hook token for the launch, and the spawn shall carry `CHALDEA_HOOK_URL`, `CHALDEA_HOOK_TOKEN` (that token), `CHALDEA_THREAD_ID`, none of `ENV_SCRUB`, and for Claude `--session-id <launch_id>` on an unbound thread or `--resume <session_id>` on a bound one, equal to `expected_session` for that launch. | D2; hook-ingest §2.8 | `supervisor::launch::env_and_session_args` (daemon env seeded with every scrubbed name, `CODEX_HOME`, `CLAUDE_CODE_USE_BEDROCK`; asserts `spawns()[0].env` and argv; the token's hash equals the inserted row; `MvpHooks` `expected_session` of the launch row equals the argv value) | token per thread; scrub by prefix (user variables lost) or missing a name; thread id as session id; resume passing the launch id |
| PR10a-CL-7 | The start job shall record the backend identity and, in the same job, move the positional input to `sent` with `sent_after_seq` = `launch_started`'s seq; a hook that arrived before it shall be decided only after that commit. | D2; mvp §2.3:186 | `supervisor::launch::started_job` (one job's events in order); `supervisor_e2e::early_hook_waits_for_started` (FakeCli `hooks` emitting `SessionStart` at once; `CHALDEA_TEST_SPAWN_PAUSE` holds the start job; the hook's reply comes after the commit, with binding) | input moved in a later job; `sent_after_seq` of `launch_reserved`; identity from the request instead of `Session::identity` |
| PR10a-CL-8 | If the daemon crashes between `launch_reserved` and `launch_started`, then restart shall record `unknown_after_crash` with death unconfirmed and the positional input `unconfirmed` with `input_unconfirmed`, and no rule but `owner_asserted` shall confirm its death. | A2, A21, A29 | `supervisor::restart::crash_before_started` (store rows of a reservation, then the reconcilers; then the prober: no confirmation; then `stop --assume-dead`: confirmed); E6's e2e below | `daemon_restart` for a launch without pid; input left `sending`; prober confirming without an identity |
| PR10a-CL-9 | If `Session::spawn` returns `NeverStarted`, the launch shall exit `spawn_failed` with death confirmed and the positional input `rejected(spawn_failed)`; if `StartedThenFailed`, `spawn_failed` with death confirmed exactly when cleanup is `Confirmed`, and the input `unconfirmed`. | mvp §2.3:186; `backend.rs:185-201` | `supervisor::launch::spawn_errors` (`FakeScript { fail }` for each variant and cleanup) | `StartedThenFailed` rejecting the input; `NeverStarted` leaving death unconfirmed; cleanup ignored |
| PR10a-CL-10 | Every `Session::kill` and `Session::close` call shall receive the launch's full recorded list (empty in the MVP), also after a report with survivors; death shall be confirmed only by the explicit close's `tree_dead`; a dropped `Session` shall confirm nothing. | A3, A26, A27, B1 | `supervisor::stop::full_list_every_call` (`KillBehavior::Ignore` with short bounds from `SupervisorConfig`: every `FakeEvent::Kill { extra }` is `[]`, `launch_exited.death_confirmed` false, no `launch_death_confirmed`); `supervisor::stop::drop_never_confirms` (an effect cancelled after spawn: no confirmation event) | survivors passed back; confirmation from the first kill report; a drop path recording death |
| PR10a-CL-11 | When a running launch is stopped, the supervisor shall record `StopRequested`, kill, close explicitly, and record one `launch_exited` (`stopped`) with `code`, `signal`, and `death_confirmed` from the close, plus `launch_death_confirmed` with the kill's method when confirmed. | mvp P-3; pty-unix §4 | `supervisor::stop::records_one_exit` (FakePty; the exit task racing with the stop records nothing); `supervisor_e2e::stop_kills_tree` (FakeCli `spawn-tree` with a sleeper grandchild and an unrelated sleeper; after the reply every tree pid is gone by `process::lookup` and the unrelated one lives) | exit recorded before close; two `launch_exited`; method dropped; unrelated process killed |
| PR10a-CL-12 | While a launch is `starting`, a stop shall wait for the spawn outcome on the op lock and then stop a started launch or answer `already_exited`; if the lock is not taken within its wait, it shall answer `launch_busy` and never stop later. | X5; `state.rs:255-258` | `supervisor::stop::while_starting` (a test seam parks the effect between reservation and spawn; release → stopped; spawn failure → `already_exited`; never released with a 50 ms wait from `SupervisorConfig` → 504 and, after release, no `StopRequested`) | `StopRequested` fed while `starting` (reducer refusal surfaces as 500); a 504 followed by a later stop |
| PR10a-CL-13 | When the child exits by itself, the supervisor shall record one `launch_exited` (`exit`) after an explicit close; after `WaitFailed`, death shall stay unconfirmed. | mvp §2.9; A25 | `supervisor::exit::natural_exit` (`FakeChild::exit` with code and signal); `supervisor::exit::wait_failed_unconfirmed` | `WaitFailed` treated as a proof; exit recorded without close; code dropped |
| PR10a-CL-14 | The job that appends `launch_exited` shall move every non-terminal input of the thread (`queued → cancelled`, `sending`/`sent → unconfirmed` with `input_unconfirmed`); at restart the reason shall be `DaemonRestarted`, and inputs of threads without an open launch shall move in the same job. | mvp CL-6; A13 | `supervisor::exit::inputs_recovered` and `supervisor::restart::inputs_recovered` (inputs written directly in each state) | inputs moved in a later job; `queued → unconfirmed`; attention missing; restart sweeping only open launches' inputs |
| PR10a-CL-15 | `buffer::resolve_unbindable` shall run in every exit job and at restart for every `buffered_launches()`; `buffer::expire` shall run on the tick for live unbound launches. | A22; hook-ingest §2.9 | `supervisor::exit::buffer_resolved`; `supervisor::restart::buffers_resolved` (a buffer left by a launch exited in an earlier run); `supervisor::tick::expires_buffer` (injected instant and stored stamps) | resolve in a later job; restart skipping already exited launches; tick not calling `expire` |
| PR10a-CL-16 | The restart reconcilers shall run in the order receipts, supervisor launches, supervisor buffers, before the listener exists. | E3; daemon-lifecycle §2.14 | `supervisor::restart::reconciler_order` (names of `Components::reconcilers`); `supervisor_e2e::restart_records_before_serving` (the first request after restart sees every launch exited) | order swapped; reconciliation in a task after serving |
| PR10a-CL-17 | After a restart, a `daemon_restart` launch shall get `launch_death_confirmed` (`restart_identity`) only when `kill_recorded` resolves every recorded identity as gone or replaced, and `kill_recorded` shall signal only pids whose lookup showed the recorded start and never a group or session. | A1, F1, A28 | `unix::orphan::tests` (scripted `Os`: live leader → HUP/TERM/KILL to the pid only → gone → `all_gone`; another start → resolved without a signal; unreadable → survivor; no `killpg` call ever); `windows::orphan::tests` (a test-owned process: terminated and resolved; a wrong start: not signalled, resolved as replaced); `supervisor_e2e::restart_confirms_by_identity` (`daemon run` killed by PID; FakeCli `sleeper` with the `ignore-term` role survives the hangup on macOS; the prober kills it and confirms) | confirmation without the recheck; `killpg` after loss; `Unreadable` treated as gone; job closing treated as proof on Windows |
| PR10a-CL-18 | `session_start_timeout` shall be raised only for the current, live, unbound, non-manual launch at least 20 s after its `launch_started`, also after a `foreign_session` `SessionStart`; `startup_stall` only for an upgraded Codex launch at its start; neither for an exited or previous launch. | D4, A32; mvp §2.2 | `supervisor::tick::session_start_timeout` (`Supervisor::tick` at +19.999 s: none; +20 s: raised; bound: none; foreign `SessionStart` row: raised; exited: none; manual Codex: none); `supervisor::launch::startup_stall_only_upgraded_codex` | guard on "`SessionStart` seen"; raise for an exited or non-current launch; manual Codex raised; Claude stall |
| PR10a-CL-19 | No byte the CLI writes to its PTY shall produce an event or a state change. | A24; mvp CL-1 | `supervisor_e2e::banner_changes_nothing` (FakeCli `banner` prints text shaped like hook output, prompts, and trust dialogs; events equal those of the same run with silent output) | any reader of PTY output feeding a reducer input |
| PR10a-CL-20 | Every child the supervisor creates shall be created through the backend or `spawn_locked` and waited for by exact pid. | A25; daemon-lifecycle §2.11 | `cargo clippy` with the existing `Command::spawn` bans (`crates/daemon/clippy.toml`; `lints.rs::clippy_tomls_list_the_bans`); `supervisor::workspace::probe_bounded` (a script that never exits is killed at the deadline and the canonical cwd used) | a direct `Command::spawn`; an unbounded git wait |
| PR10a-CL-21 | Every committed job that revokes a token or ends a launch's agent-token validity shall nudge sockets through `after_ok`: close's intent, close `--stop`'s closing job, and the exit job. | E4, X1; http-surface §2.4 | `http::tests::commands::revoking_intent_nudges` (a test handler with `revokes()` true: one nudge after commit, none before; false: none); `supervisor::close::nudges` (nudge counter on the test server) | runner ignoring `revokes()`; nudge before commit; exit job without nudge |
| PR10a-CL-22 | The four handlers shall be registered, every new code classified in `handler_code` at one status, and no success type shall have a top-level `code`. | E1, E2 | `handler_code` exhaustive `match` (compile); `supervisor::commands::codes_and_statuses` (each refusal's status); contract drift test over the new types | a code at two statuses; a success body with `code`; a handler left 501 |
| PR10a-CL-23 | The participant shall stop the tick and the prober by its `end` and leave sessions alive; a launch transition that cannot commit shall report `StoreFatal::LaunchUnrecorded`. | E5 | `supervisor::shutdown::participant` (no `FakeEvent::Kill`, no job after return); `supervisor::launch::unrecorded_is_fatal` (store fault seam failing the start job; the sink receives it) | participant killing sessions or overrunning `end`; error swallowed |
| PR10a-CL-24 | A retry of a `spawn` whose daemon died during its effect shall answer 409 `unknown_after_crash`, and the effect shall not run again. | E6; daemon-lifecycle PR8-P-3b | `supervisor_e2e::retry_after_crash_is_unknown` (`daemon run` child; `CHALDEA_TEST_SPAWN_PAUSE` parks after the reservation; kill by PID; restart; retry with the same `command_id`; one `launch_reserved`) | receipts reconciler skipped; effect re-run on retry |
| PR10a-CL-25 | `GET /api/adapters` shall return exactly this host's `CAPABILITIES` rows in `AdaptersResponse`. | C6 | `supervisor::adapters::equals_table` (each `HostOs`); contract drift test | other OS's rows; `submit_delay` exposed; a hand-written list |
| PR10a-CL-26 | `close` shall commit `thread_closed` and revoke the thread's tokens only when the current launch exited with death confirmed; `--stop` shall close only the launch it stopped. | mvp §2.11 | `supervisor::close::cases` (live → `launch_live`; unconfirmed → `death_unconfirmed`; `--stop` with a resume racing after the stop → `launch_changed`) | close with a live launch; tokens kept; closing a newer launch |
| PR10a-CL-27 | `stop --assume-dead` shall be accepted only from an owner token for a current launch that exited with death unconfirmed, and shall record `owner_asserted` and clear `resume_blocked`. | SD2 | `supervisor::stop::assume_dead` (rescue token → 403; running → `launch_live`; exited unconfirmed → confirmed) | non-owner allowed; allowed on a running launch |
| PR10a-CL-28 | `chaldea wait` shall return the thread when the condition holds and exit 3 no later than one poll after its timeout. | SD16 | `supervisor_e2e::wait_until_exited` (FakeCli exits; `wait` returns its JSON); `agent_cmd::tests::wait_times_out` (a stub client that never satisfies it) | timeout ignored; edge-triggered behaviour missing an already-true state |
| PR10a-CL-29 | A guard 10a changes shall change `specs/thread_input.qnt` and its traces in the same PR. | A38; formal-specs §6 | CI `specs` job (`traces:check`) and `quint_traces.rs` replay | a Rust guard changed without the model (the new trace's replay fails) |
| PR10a-CL-30 | A thread's `workspace_id` shall be derived from git's common directory, so a repository and its worktrees share it, and from the canonical cwd outside git. | mvp §2.1:103 | `supervisor::workspace::ids` (a temp repository, a `git worktree add` of it, and a plain directory) | cwd used inside git; per-worktree ids |

### 5.3 Accepted Limits

| ID | Limit | Decided by | Why it does not defeat the purpose |
|---|---|---|---|
| L-1 | macOS, after a restart: members of the old session that are not recorded identities and survived the PTY hangup are neither signalled nor checked; `restart_identity` confirms the leader. | pty-unix §2.5 (no group or session target without ownership) | resume safety needs the CLI holding the session dead, which the rule proves; mvp P-18's survivor report (PR 15/23) lists the rest; a live-daemon `stop` keeps the full tree kill (P-3). |
| L-2 | Windows, after a restart: job members are terminated by `KILL_ON_JOB_CLOSE` but checked only through the recorded identities. | pty-windows §4 "Daemon restart" | same as L-1; the job's guarantee (no member escapes, pty-windows §2.5) makes survivors limited to processes never in the job. |
| L-3 | A launch without a recorded identity (A2, A29) is never confirmed automatically; an A29 child stays suspended until terminated. | SD2 | it blocks reservation, so no second CLI runs on the session; the owner can release it; the suspended child never runs. |
| L-4 | A `StartedThenFailed` launch with unresolved cleanup keeps its identity only in the log. | `backend.rs:194-200`; L-3's rule | rare (thread creation or start read failed); handled as L-3. |
| L-5 | A Codex positional prompt longer than Windows' command-line limit fails as `spawn_failed`. | `CreateProcessW` `lpCommandLine` (32,767 characters) | the input ends `rejected` with zero bytes written, visible to the owner; PR 14 can refuse earlier. |
| L-6 | Until PR 13 and PR 14, a real CLI launched by 10a never binds: `session_start_timeout` appears and `resume` answers `no_session`. | SD14 | spawn, stop, close, restart, and their proofs work; binding is tested with FakeCli `hooks`. |
| L-7 | A launch whose close could not prove death while the daemon owned it is released only by the owner. | §2.4 | the narrower restart rule is not allowed to override what the owned close saw. |
| L-8 | A per-pid signal of `kill_recorded` can reach a reused pid if the pid space wraps between its lookup and the signal. | pty-unix §2.5 "Never a reused pid" | the same residual window PR 3 accepted. |
| L-9 | After `chaldea daemon stop`, running threads read `daemon_restart` on the next start. | SD10 | the history is intact and death is confirmed by the restart rule. |
| L-10 | When git is missing or slower than 5 s, the canonical cwd is used, so worktrees of one repository can get different workspace ids. | §2.3 step 1 | it groups threads and scopes PR 20's Workspace surfaces; a split id narrows such a scope, never widens it. |

## 6. Tests

**Per claim**: §5.2's test column. Unit tests drive `FakePty` (`FakeScript`, `FakeChild::exit`, `FakeEvent::Kill`, `FakePty::spawns`) and the store directly; integration tests start the store and `HttpServer` in-process with the native runtime and FakeCli, as `hook_e2e.rs` does; the restart and E6 tests run `chaldea daemon run` as a child process and kill it by PID.

**FakeCli `banner` mode** (new, `crates/testkit/src/fake_cli/banner.rs`): prints a fixed corpus of text shaped like Claude and Codex trust prompts, `Hooks need review`, a JSON hook payload, and a prompt echo, then sleeps until `--max-life-secs`.

**Host speed.** Timers run through `Supervisor::tick(now)` with an injected instant; the spawn-to-start window is held by `CHALDEA_TEST_SPAWN_PAUSE` (parks until the test releases it, as the existing `CHALDEA_TEST_*` pauses); kill and close bounds are shortened through `SupervisorConfig` only where a test needs `KillBehavior::Ignore`; real-process tests decide pass or fail by `process::lookup` after the command's reply, never by elapsed time.

**Windows CI.** Every `supervisor::` unit test, `launch::`, `adapter::`, `windows::orphan::tests`, and `supervisor_e2e::` run on `windows-latest` in the Cargo lane of `scripts/windows-tests.ps1` (nested jobs need no breakaway, pty-windows PR4-CL-27; `daemon run` is not a detached start). `unix::orphan::tests` and the macOS `ignore-term` restart case are `cfg(unix)`.

**Mutation check.** Before merging, each mutation of §5.2 is applied by hand, the named test is seen failing, and the change is reverted; the PR description lists the result per claim.

**Completion commands.**

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked                 # macOS; Windows CI runs scripts/windows-tests.ps1 -Lane Cargo
(cd specs && pnpm run typecheck && pnpm run simulate && pnpm run traces:check)
git diff --exit-code packages/contracts         # after regeneration is committed
```

## 7. Quint

What changes in `specs/thread_input.qnt` (in the implementation PR, with `pnpm run traces:write`):

1. **`sessionTimer` guard** (obligation D4): `not(st.sessionStartSeen)` → `not(st.gp.bound)`. A `SessionStart` that does not bind leaves the launch unbound, and the cause must report it (hook-ingest:268). `noStartupCauseAfterBindOrExit` keeps holding, since binding clears the cause and the guard refuses a bound launch. New trace `trace_session_timer_after_unbound_session_start`: `reserve(CODEX)`, `spawn`, `sessionStartHook` (no `bind`), `sessionTimer` → `SessionStartTimeout`; `exit` → cleared.
2. **New action `spawnStartedThenFailed(dc: bool)`** beside `spawnFail`: from `Starting`, `ExitedBy(ExitSpawnFailed)`, the positional input `sending → unconfirmed` through `LaunchExited`, `deathConfirmed = dc`. New trace `trace_spawn_started_then_failed` (both `dc`), extending `trace_positional_spawn_outcomes`.
3. **Hole a**: no guard change. `reserve` already requires `st.deathConfirmed` (`thread_input.qnt:791`); 10a makes the Rust reservation job enforce it (PR10a-CL-3). `trace_hole_a_reserve_without_death` stays as the statement of what `reduce` alone admits, and `reservedOnlyAfterDeath` is the checked invariant.
4. **Hole c**: no model change. `sessionTimer` and `stall` already require a live, current launch; 10a's job-side guards implement them (PR10a-CL-18). `trace_hole_c_stale_startup_cause` stays.
5. **Unchanged and mapped**: `stop` requires `Running` (X5's hold is outside the model: the stop is applied after the outcome); `restart` uses `UnknownAfterCrash` exactly for a `Starting` launch, which in Rust is "no pid recorded"; `confirmDeath` is the restart prober and the owner assertion.

formal-specs §3's owner column for holes a and c becomes "PR 10a", and §6's PR 10 bullet points here (§8).

## 8. Proposed Canonical-Doc Changes

Not applied here.

- **mvp §2.2, attention table, `session_start_timeout`**: "Raised on: no `session_bound` 20 s after `launch_started` of a non-manual launch (a `SessionStart` that does not bind, such as `foreign_session`, does not stop it)".
- **mvp §2.7 "Env scrub"**: the named list of §2.7 here instead of the `CLAUDE_CODE_*` and `CODEX_*` prefixes, with the reason of SD13.
- **mvp §2.9 "Daemon restart"**: "`launch_death_confirmed { method }` follows once the old tree is proven dead" → once the restart identity rule holds (`restart_identity`, §2.4 here), or the owner asserts it (`owner_asserted`); a launch without a recorded identity needs the assertion.
- **mvp §2.4 / §2.5**: the command args, success types, and codes of §2.2; `stop --assume-dead` (owner only); `chaldea wait` (SD16).
- **mvp §4, PR 9 / 10 bullet**: "PR 10" (hook-ingest D10 gives it to PR 10 entirely).
- **mvp §4, PR 10 bullet**: replace "restart scan of a launch's session by recorded sid with `launch.start_time ≤ start ≤ restart_now`" with the restart identity rule; state that PR 10 is designed as 10a (this document) and 10b.
- **mvp §6.1 P-18**: the reported-survivor limit covers, on macOS, members of the old session that survived the hangup without a recorded identity (L-1), not only `setsid` grandchildren.
- **http-surface §2.4 "Revocation nudges"**: command intents that revoke set `CommandHandler::revokes`; the runner passes the nudge. **§2.8**: the `revokes` method in the trait listing. **"Handler replies"**: the nine codes of §2.2. **§2.7**: `GET /api/adapters` → `AdaptersResponse` (SD5). **§4 PR 10 row**: split 10a / 10b.
- **hook-ingest §4 PR 10 row**: `extra` is empty in the MVP (§2.4); the restart buffer pass is PR 10's second reconciler.
- **daemon-lifecycle §2.14**: reconcilers "receipts → supervisor launches → supervisor buffers", the last implemented by PR 10 (PR 9 shipped none); participant row: "stops the tick and prober; sessions are left to process exit (SD10)".
- **deadlines.md**: production exceptions gain `supervisor::tick_timer` (14); the `Session::spawn` wait without a deadline and its reason (§2.10).
- **pty-unix.md §4, pty-windows.md §4**: the "Daemon restart" rows point to `orphan::kill_recorded` and the identity rule; pty-unix's "Spike watchdog and emergency kill" row: none, `stop` (SD15). **pty-runtime.md §4**: a row for `orphan::kill_recorded` (PR 10a, both backends).
- **formal-specs §1** (`sessionTimer` row: "unbound" instead of "no `SessionStart`"), **§3** (owner "PR 10a" for holes a and c), **§6** (PR 10's bullet split into 10a and 10b).
- **AGENTS.md**: none.
