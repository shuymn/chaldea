# Design: Supervisor, Launch and Process Side (MVP PR 10a)

Status: design. Scope is the launch and process half of the PR 10 row of `docs/mvp-design.md` §4 (cited as "mvp §N"), called **PR 10a**: reservation, the `spawn`, `resume`, `stop`, and `close` commands and the `chaldea wait` CLI, the process lifecycle and its death proof, the restart reconciler, the hook token and binding facts of each launch, `extra` attribution, `GET /api/adapters`, the host-OS mapping, and the startup attention causes. The send and ack half (`send`, `ready`, `key`, `input_resolve`, `input_cancel`, the gate, the writer, ack attribution, the real `IntakeProbe`, input-side attention and cards) is **PR 10b**, designed against the interfaces of §2.11.

**What 10a decides and leaves.** 10a owns the supervisor module of the daemon, the pure launch builder in `core`, small additions to `pty-runtime` (§1), and the CLI verbs above. It leaves to 10b everything that writes to a live PTY after `launch_started` and every ack decision; to PR 13 and PR 14 the hook configuration each CLI gets (which events, Claude's settings file, Codex's `-c hooks.*`), the payload decoders, and the verification of the argv rows on real binaries; to PR 16 agent tokens and worktree creation and removal; to PR 11 attach; to PR 18–19 the UI; to PR 21 the doctor's orphan finder.

**Owner decision (2026-11-25).** After a daemon crash, a check that covers only a launch's recorded identities does not unlock resume, close, or worktree release. The launch's death is confirmed only by the owner, with `chaldea stop <thread> --assume-dead --launch <id>` after reading the launch's evidence (§2.4).

**Commitments from earlier PRs this design keeps.**

1. Death proof is pty-runtime's. `death_confirmed` becomes true only from `CloseOutcome.kill.tree_dead` of an explicit `Session::close` with the launch's full list whose join returned in time, from a spawn error whose process provably never ran or whose cleanup is `Cleanup::Confirmed`, or from an owner assertion bound to the launch. No census, no signal to a session or group the daemon does not own, no proof from a `Session` drop.
2. Every wait has a deadline through the daemon's helpers (`docs/design/deadlines.md`; `host::{act_before, observed_before}` on the synchronous side), with one stated exception (§2.10).
3. Thread state moves only through explicit events: hooks, the backend's exit report, and the supervisor's own commands (invariant 1). Nothing reads PTY text.
4. Public types change only where §1 lists a new name. `StateInput`, `InputInput`, `reduce`, `step`, and `gate` keep their rules.

Sources read for this document:

| Source | Revision | Read for |
|---|---|---|
| This repository | `cdf9524` | `AGENTS.md`; `docs/architecture.md`; `docs/mvp-design.md` §2.1–§2.11, §4, §6, §8; `docs/prior-art.md` rows 1a, 1b, 1d, 6c; `docs/design/{pty-runtime,pty-unix,pty-windows,daemon-lifecycle,http-surface,hook-ingest,event-store,formal-specs,deadlines,input-encoders}.md`; `crates/core/src/{state,adapter,input,payload,sync,ids,auth,http,command,event}.rs`, `crates/core/src/input/encode.rs`; `crates/pty-runtime/src/{lib,session,backend,process,spawn,fake,limits,origin,writer}.rs`, `host/mod.rs`, `unix/{mod,kill,child,table}.rs`, `windows/{mod,child,cmdline,hooks,sys}.rs`; `crates/daemon/src/http/{commands,ctx,error,routes,auth,mod}.rs`, `lifecycle/{mod,gate,startup,shutdown,files}.rs`, `hook/{mod,adapter,buffer,intake,route}.rs`, `store/{write_tx,readers,rows,project}.rs`, `deadline.rs`, `clippy.toml`; `crates/cli/src/{args,client,daemon_cmd,lib}.rs`, `crates/cli/tests/hook_e2e.rs`; `crates/testkit/src/fake_cli/{mod,hooks,sleeper}.rs`; `specs/thread_input.qnt`, `specs/thread_input_traces.qnt`; `scripts/windows-tests.ps1` |
| Win32 `CreateProcessW` | Microsoft Learn, read 2026-11-25 | the `lpCommandLine` limit of 32,767 characters (L-5) |
| Win32 Job Objects | Microsoft Learn, read 2026-11-25 | a child created by a process in a job is associated with that job by default (A29 finder) |
| Claude Code settings | `code.claude.com/docs/en/settings`, read 2026-11-25 | environment variables under the `CLAUDE_CODE_` prefix that select provider and behaviour (SD13) |

## 1. Goal and Dependencies

**Goal.** A CLI started by `chaldea spawn` or `chaldea resume` runs in a daemon-owned PTY with its own hook token and binding facts; each step of its life reaches the store as one explicit event in one job; every launch ends with `launch_exited`, its death is confirmed exactly as far as a named rule shows it, and what no rule could check is recorded for the owner; a restart records every open launch, recovers every input, and revokes every launch token before the listener exists; and no thread is relaunched until its previous launch's death is confirmed.

**Depends on** PR 3 and PR 4 (backends, `Session`), PR 5 (`capabilities`, `check`), PR 8 (`Components`, reconcilers, participants, `spawn_locked`), PR 9 (`POST /hook`, binding, `buffer`).

**Files and seams** (every name marked new does not exist at `cdf9524`):

```
crates/core/src/adapter.rs        # HostOs gains serde/TS/JsonSchema and from_os_str (new)
crates/core/src/launch.rs         # new: LaunchInput, LaunchPlan, plan(), ENV_SCRUB, workspace_id()
crates/core/src/command.rs        # adds the wire types of §2.2 (SpawnArgs … CloseResult)
crates/core/src/payload.rs        # adds LaunchEvidence and its types (§2.4)
crates/core/src/event.rs          # TypedEvent::LaunchEvidence, kind launch_evidence (new)
crates/pty-runtime/src/orphan.rs  # new: kill_recorded, OrphanReport (+ unix/orphan.rs, windows/orphan.rs)
crates/pty-runtime/src/session.rs # adds Session::seal, Session::observer, WriterObserver, CloseOutcome.exit
crates/pty-runtime/src/env.rs     # new: env_key_eq, env_key_has_prefix (the backend's key comparison)
crates/pty-runtime/src/spawn.rs   # with_spawn_lock_before (new; public form of spawn_lock_before)
crates/pty-runtime/src/fake.rs    # adds FakeChild::fail_wait
crates/daemon/src/supervisor/     # new: components, commands, live, ops, exit, restart, tick, workspace, record, adapters
crates/daemon/src/http/mod.rs     # RevocationNudge, HttpServer::revocation_nudge, StoreFatal::LaunchUnrecorded (new);
                                  #   pub(crate) re-exports of the seams below
crates/daemon/src/http/auth.rs    # mint_token (new free fn; TokenService::mint calls it)
crates/daemon/src/http/ctx.rs     # write_with (new; run_write calls it with the server's parts)
crates/daemon/src/http/commands.rs# CommandHandler::revokes (new); CommandEffect::run returns Option;
                                  #   retryable, backoff_until, BACKOFF_*, COMPLETION_* become pub(crate)
crates/daemon/src/http/routes.rs  # Mounts::mvp_with (new): Mounts::mvp with given HookSeams and the adapters handler
crates/daemon/src/store/readers.rs# ReadTx::inputs_in_states (new): inputs of every thread in given states
crates/daemon/src/store/write_tx.rs # TokenSelector::LaunchScoped (new): every token with a launch
crates/daemon/src/store/project.rs# a no-op arm for launch_evidence
crates/daemon/src/lifecycle/mod.rs# ParticipantEnd (new), returned by ShutdownParticipant::shutdown; spawn_locked_before (new)
crates/daemon/src/lifecycle/shutdown.rs # the lock rule counts ParticipantEnd::LeftRunning
crates/daemon/src/lifecycle/gate.rs     # fatal_label arm for LaunchUnrecorded
crates/cli/src/client.rs          # post_json (new); MAX_BODY becomes REPLAY_MAX_BYTES
crates/cli/src/poll.rs            # new: poll_until moved from daemon_cmd.rs, with a clock seam
crates/cli/src/agent_cmd.rs       # new: spawn, resume, stop, close, wait
crates/cli/src/lib.rs             # Seams gains host_os (new) for the startup refusal test
crates/cli/src/daemon_cmd.rs      # default components: SupervisorComponents instead of MvpComponents
crates/testkit/src/fake_cli/banner.rs # new FakeCli mode
crates/cli/tests/supervisor_e2e.rs    # new
```

**Seams the supervisor needs and how it gets them.** `Components::build` gets only `BuildCtx` (store, gate, sink, port, state dir) at step 11 and `Components::attach(&HttpServer)` at step 12 (`lifecycle/startup.rs:647-676`); `http::Shared` is reachable only from tests (`http/mod.rs:339`), and the `http` submodules are private (`http/mod.rs:19-34`), so each seam is re-exported from `http` as `pub(crate) use`. Tokens are minted with `mint_token` and rows built with `TokenService::new_token` (an associated function), both data only; the supervisor's writes go through `write_with(&Store, &dyn LifecycleSink, clock, not_after, job, after_ok)`, the body of `run_write` without `Shared`; the revocation nudge is a cloneable `RevocationNudge` taken from `HttpServer::revocation_nudge` in `attach` and stored once (before the listener exists, so no revoking job runs earlier); the supervisor's clock is `SupervisorConfig.clock` (`SystemMonotonic` outside tests).

**New identifiers** besides those of the tree above: `supervisor` (module), `Supervisor`, `SupervisorConfig`, `SupervisorComponents`, `LiveLaunch`, `LiveLaunch::with_session`, `Supervisor::live`, `Supervisor::observe`, `Supervisor::admit`, `LaunchObservation` and its variants, `Started` and its fields (`reference`), `SessionError`, `LaunchOp`, `ExitStep` (with `after_commit`), `ExitFacts`, `TickStep`, `Supervisor::tick`, `supervisor::record`, `supervisor::record::Job`, `supervisor::tick_timer`, `supervisor::workspace::common_dir`, `SpawnArgs`, `ResumeArgs`, `StopArgs`, `CloseArgs`, `SpawnResult`, `ResumeResult`, `StopResult`, `CloseResult`, `AdaptersResponse`, `AdapterInfo`, `LaunchEvidence`, `EvidenceSource`, `IdentityFate`, `Fate`, `UncheckedScope` and their fields and values (`unchecked`, `unrecorded_session_members`, `job_members`, …), `launch_evidence`, `OrphanReport`, `OrphanFate`, `orphan::kill_recorded`, `kill_recorded`, `Session::seal`, `Session::observer`, `WriterObserver`, `WriterObserver::last_started_each`, `CloseOutcome.exit`, `env_key_eq`, `env_key_has_prefix`, `with_spawn_lock_before`, `spawn_locked_before`, `FakeChild::fail_wait`, `mint_token`, `RevocationNudge`, `HttpServer::revocation_nudge`, `write_with`, `CommandHandler::revokes`, `StoreFatal::LaunchUnrecorded`, `ParticipantEnd` and its variants, `Mounts::mvp_with`, `ReadTx::inputs_in_states`, `TokenSelector::LaunchScoped`, `post_json`, `poll_until_with`, `HostOs::from_os_str`, `launch::plan`, `launch::workspace_id`, `LaunchInput`, `LaunchPlan`, `ENV_SCRUB`, the reconcilers `supervisor_launches` and `supervisor_buffers`, FakeCli mode `banner`, the wire fields and values of §2.2 (`expected_launch_id`, `assume_dead`, `already_exited`, `probed`, `asserted`, `recorded_gone`), `OrphanReport`'s `all_gone`, the error codes of §2.2's code table, the death method `owner_asserted`, the Quint action `spawnStartedThenFailed` and the traces of §7, and every test name in §5.2. `CLAUDE_CODE_USE_BEDROCK` is a Claude Code variable (its settings page); it, `Chaldea_Hook_Token`, `claudecode`, and `linux` are test inputs.

## 2. Design

### 2.1 Rules

1. **One job per transition.** Reservation, start, stop request, exit, evidence, death confirmation, and close are each one write job that re-reads the rows it decides on and appends its events with the reducer's `status_changed`. Every job after the runner's intent goes through `supervisor::record` (§2.10).
2. **Launch operations outlive requests.** A command's effect hands its work to a `LaunchOp`, a supervisor-owned task, and awaits its result; the op runs to its end when the runner is aborted or the request goes away, and owns any `Session`, spawn, kill, or close result. Every blocking call an op starts (`Session::spawn`, `Session::close`, `kill_recorded`) and every child it leaves (§2.3 step 1) is counted by the supervisor until it ends, also after the op stopped waiting for it.
3. **One op lock per launch.** The live map is keyed by `LaunchId`; its entry, locked by its op, exists before the reservation commits; every use of the `Session` and every record of its outcome holds the lock (§2.11).
4. **One admission point for new processes.** Every process creation (the git probe, `Session::spawn`) first passes `Supervisor::admit`, which refuses once shutdown started (§2.9).
5. **The full list on every kill.** Every `Session::close` (and, for 10b, any `Session::kill`) gets the launch's full recorded-descendant list; a report's survivors are never passed back (pty-windows §2.5 "The caller's obligation").
6. **Kills record what they could not check.** A kill that confirms records its method; a kill that could not confirm records its evidence (§2.4).
7. **Startup causes for the current, live, unbound launch only**, decided in the job that raises them.

### 2.2 Commands

Registered through `CommandRegistry::register` in `Built::commands` (E1). The guard decides kind and action per name (`crates/core/src/auth.rs` `MVP_COMMANDS`, `grant`). Each handler's `prepare` parses the wire args (400 `bad_args` when they do not fit, including `assume_dead: true` without `expected_launch_id`), mints ids and the hook token with `mint_token`, and builds an intent closure from data only; the handler object (holding `Arc<Supervisor>`) is used only by `effect` (http-surface §2.8).

**Target scope first.** The `resume`, `stop`, and `close` intents check the target thread against the token's grant (`may_act_on_thread`) before they read anything else of it; a refusal is `deny_for` (404 for a thread the token cannot read, identical to a missing one; http-surface §2.5 "Hiding"), with nothing written and no op started.

| Command | Args (wire types) | Intent (the runner's job) | Effect: `LaunchOp` | Success | Deadline |
|---|---|---|---|---|---|
| `spawn` | `SpawnArgs { agent, cwd, prompt?, task? }` | object authorization; returns `Pending` | §2.3 "Spawn and resume" | `SpawnResult { thread_id, launch_id, input_id?, outcome: started \| spawn_failed }` | 30 s |
| `resume` | `ResumeArgs { thread_id, expected_launch_id, prompt? }` | target scope; returns `Pending` | §2.3 "Spawn and resume" (the reservation checks run in the op's reservation job) | `ResumeResult { launch_id, input_id?, outcome }` | 30 s |
| `stop` | `StopArgs { thread_id, expected_launch_id?, assume_dead?: bool }` | target scope; the current launch `L` (it must equal `expected_launch_id` when given); then §2.4's results table | §2.3 "Stop" for a live `L`; §2.4 "Probe" for a `daemon_restart` `L` | `StopResult { launch_id, outcome: stopped \| already_exited \| probed \| asserted, death_confirmed, code?, signal?, recorded_gone? }` | 30 s |
| `close` | `CloseArgs { thread_id, expected_launch_id, stop?: bool }` | target scope; `thread_closed`; `launch_changed` (current ≠ `expected_launch_id`); then §2.3 "Close" | stop of a live `L`, then the closing job (with the nudge) | `CloseResult { thread_id }` | 30 s |

`chaldea wait` is a CLI read loop, not a command (SD16).

**Caller.** An owner token spawns a top-level thread (`parent_thread_id: None`); an agent token (PR 16 mints them) spawns a child of its own thread (`parent_thread_id` = the token's thread, the `ChildrenOfSelf` grant of `auth.rs:144`). Both get `role: standard` (`--role rescue` is PR 20's; PR10a-CL-38).

**Prompt admission** (C1; mvp §8 D5). A `prompt` is accepted only for Codex (Claude's first prompt is PTY input after readiness, a 10b `send`: until 10b, a Claude `prompt` is 400 `bad_args`). In `prepare`: longer than `MAX_PROMPT_BYTES` → 400 `prompt_too_large`; `check(Codex, Positional, Purpose::Send, text)` → `Err` is 400 `prompt_rejected`; both before any receipt or event. The CLI runs the same `check` first to print the reason, since the error body carries only the code. A Codex launch without a prompt is a manual launch (`LaunchReserved.manual`).

**Filesystem refusals** come from the op, after the receipt exists: a `cwd` that does not exist or is not a directory is 400 `bad_args` with no reservation, token, event, or spawn; the runner stores that reply, so a retry with the same `command_id` replays it even after the directory appears (a new request needs a new `command_id`). `prepare` reads no filesystem.

**Codes.** New `ErrorCode` variants, each classified in `handler_code` at exactly one status (E2):

| Code (new) | Status | When | The work can happen later? |
|---|---|---|---|
| `thread_closed` | 409 | resume or close on a closed thread (a closed thread's launch is confirmed dead, so its stop is `already_exited`) | no |
| `launch_changed` | 409 | `expected_launch_id` is not the thread's current launch; a stop or close `--stop` whose target launch stopped being current before its op acted | no |
| `launch_live` | 409 | resume or close (without `stop`) while the current launch has not exited; `assume_dead` on a launch that has not exited | no |
| `death_unconfirmed` | 409 | resume or close while the current launch exited without confirmed death (resume also raises `resume_blocked` in the same job); stop of such a launch with nothing to probe (§2.4) | no |
| `worktree_unavailable` | 409 | the launch's worktree is not `active` or `released` | no |
| `no_session` | 409 | resume of a thread with no bound `session_id` | no |
| `prompt_rejected` | 400 | `check` refused the positional prompt | no |
| `prompt_too_large` | 400 | the prompt exceeds `MAX_PROMPT_BYTES` (also 10b's admission code, C8) | no |
| `launch_busy` | 504 | stop could not take the launch's op lock, or the probe lock, in time (§2.3, §2.4) | no: the stop is abandoned |

`assume_dead` from a token other than an owner token is 403 `forbidden`. Success types have no top-level `code` field; a test asserts it per type (E2).

### 2.3 Launch Lifecycle

**Spawn and resume** (the `LaunchOp`; `L` is the launch id minted in `prepare`):

1. *Inputs.* `cwd` (spawn) must be a directory, else 400 `bad_args` (§2.2). Resolve the program (§2.7); a missing one is kept as `None`. Spawn only, the workspace probe: `supervisor::workspace::common_dir(cwd, now + 5 s)` runs `git -C <cwd> rev-parse --path-format=absolute --git-common-dir` (stdin and stderr null, stdout piped; `CREATE_NO_WINDOW` on Windows). It passes `admit`, is created through `spawn_locked_before` with that deadline, and is waited for by exact pid with `host::observed_before` polls of `try_wait`. Exit 0 → the printed path (read after exit; one line, far below a pipe buffer). Not created by the deadline, refused admission, a nonzero exit, or not exited by the deadline → the canonical cwd, and the daemon log names which. A child not exited by the deadline is not killed: it joins the supervisor's leftover children, which the tick reaps with `try_wait`. `launch::workspace_id(path)` = `ws-` + the first 16 hex digits of SHA-256 of the path's UTF-8 bytes (mvp §2.1:103).
2. *Slot.* Insert the `LiveLaunch` for `L` with its op lock taken by this op.
3. *Reservation job.* Resume first re-reads the thread and its current launch and refuses, in this order: `thread_closed`; `launch_changed` (current ≠ `expected_launch_id`); `launch_live` (no `exit_reason`); `death_unconfirmed`, raising `resume_blocked` when absent (mvp §2.9 "Daemon restart"); `worktree_unavailable` (the previous launch's `worktree_id`, if any: only `active` and `released` pass; mvp CL-18); `no_session`. A refusal commits only the raise, removes the slot, and is the op's result. Then, in the same job: spawn's `thread_registered`; `launch_reserved` (`expected_previous`, `argv_digest` of the planned argv, `manual`, and for resume the previous launch's `worktree_id`, so `launches_using_worktree` returns the new launch); `StateInput::Reserved`; the positional `input_created { initial_state: sending, encoding: positional, origin: cli }`; and `token_insert` of the hook token. A rollback removes the slot.
4. *Spawn.* Pass `admit`, then `Session::spawn(runtime, &spec, config)` on `spawn_blocking`, awaited by the op to its result (§2.10). Refused admission or no program → the `NeverStarted` branch of step 6, with no process created. `config.on_event` forwards `SessionEvent::Exited` and `SessionEvent::WaitFailed` to the launch's exit op without blocking.
5. *Publication.* On `Ok(session)`: take `observer = session.observer()` and `reference = observer.last_started(ClassSet::ALL)`, set the slot's `Started { observer, reference, identity, at }`, and put the session in the slot, all before the start job is submitted. From here `observe` answers `Started` (§2.11); a write with a `seq` above `reference` is a write after the positional prompt's anchor (A35's anchor; 10b decides what it means for an ack).
6. *Start job*, by outcome:
   - `Ok(session)` → `launch_started { pid, start_time, pgid_or_job, cli_version: None }` from `Session::identity`, `StateInput::Spawned`, the positional input `sending → sent` (`InputInput::WriteCompleted`, `sent_after_seq` = this `launch_started`'s seq; mvp §2.3:186), and for an upgraded Codex launch the `startup_stall` raise (§2.6). After the commit the op lock is released.
   - `Err(SpawnError::NeverStarted { .. })`, refused admission, or no program → `launch_exited { reason: spawn_failed, death_confirmed: true }` (no process ran and any process created was reaped, `backend.rs:186-193`), the positional input `sending → rejected(spawn_failed)`, and the exit steps of §2.11.
   - `Err(SpawnError::StartedThenFailed { cleanup, identity, .. })` → `launch_exited { reason: spawn_failed, death_confirmed: cleanup == Cleanup::Confirmed }`, the positional input `sending → unconfirmed` (the CLI may have read its argv) with `input_unconfirmed`, `launch_evidence { source: spawn_failed }` when unconfirmed (§2.4), and the exit steps.

Fresh and resumed launches reserve in the op rather than in the intent because the slot must exist before the reservation is visible (rule 3) and spawn needs the workspace probe first (SD11).

**Stop** (the `LaunchOp` for a live `L`):

1. Take the op lock of `L` with `deadline::settle_before(…, now + 10 s)` and `in_time()`; `Elapsed` or `Late` → 504 `launch_busy`, nothing written (a `Late` guard is dropped at once). A spawn in progress, an exit op, or a 10b call holds the lock (X5). The slot exists while `L` is not exited, since it precedes the reservation commit; a missing slot means `L` exited, and the op reads it.
2. Re-read `L` in a job: not current → `launch_changed`; exited → §2.4's results table; running → `Session::seal`, then a job with `StateInput::StopRequested`. The seal is the irreversible stop boundary (SD18, §2.11 "Stop before write").
3. `Session::close(&extra, CloseBounds::default())` on `spawn_blocking`, awaited by `settle_before` until its bound + 1 s. Close's step 2 is the tree kill (`bounds.kill` = 5 s), so the stop makes no separate kill call; mvp P-3's 5 s is that bound, counted from the close's start, which a stop waiting for a spawn in progress reaches up to 10 s later (§8).
4. The exit job of §2.11 with `reason: stopped`, `code` and `signal` from `CloseOutcome.exit` (null when the waiter had not recorded an exit by the join), and `death_confirmed` = `CloseOutcome.kill.tree_dead` when the join returned in time; then `launch_death_confirmed { method: KillReport.method }` in the same job. A `Late` or `Elapsed` join or a panic records `death_confirmed: false` with `launch_evidence { source: close, unchecked: [close_unreported] }`; a close that returned `tree_dead: false` records its survivors as `launch_evidence { source: close }`. A close result that arrives after the op stopped waiting is logged and dropped by its blocking task.

Worst case within the 30 s deadline: lock 10 s + `StopRequested` job + close 11 s + 1 s join margin + exit job, about 22 s plus two jobs' store time. A store slower than the remaining 8 s turns the reply into 409 `in_progress`, and a retry with the same `command_id` replays the stored result.

**Natural exit.** The exit op receives `SessionEvent::Exited(exit)` or `SessionEvent::WaitFailed`. It takes the op lock in 10 s rounds, re-reading the launch after each (a stop holding the lock records the exit itself); every holder's work is bounded (§2.10), so after 60 s it reports `StoreFatal::LaunchUnrecorded`, unless shutdown started, when it ends without a report and the next start records the launch (§2.5). Holding the lock, it returns if `L` has an `exit_reason`; else it runs Stop's step 3 and records `reason: exit` (or `stopped` when the state is `stopping`), with `code` and `signal` from the callback's `ChildExit` (null after `WaitFailed`). `WaitFailed` means the waiter could not report an exit (pty-runtime §2.2; on macOS after ownership was lost, after another wait error, or after a waiter panic, `unix/child.rs:183-200`; on Windows a waiter error independent of the job proof, `windows/child.rs:69-85`); it decides nothing about death, which is the close's `tree_dead` in every case (A25).

**Close.** The intent, with or without `stop`: target scope, `thread_closed`, `launch_changed` (current ≠ `expected_launch_id`). Then without `stop`, or with `stop` and an exited launch: `launch_live`, `death_unconfirmed`, else `thread_closed { by }` and `token_revoke(TokenSelector::Thread(t))` in the same job. With `stop` and a live launch: `Pending(L)`; the op runs Stop for `L`, then the closing job with the same checks (a resume that reserved a newer launch meanwhile gives `launch_changed`; an unconfirmed stop gives `death_unconfirmed`). Close never probes. Worktree hand-over (mvp §2.6) is PR 16's and `after_close` PR 20's, each added to the closing job.

**Requests and ops.** The op's result is the effect's result. `CommandEffect::run` resolves to `Option<CommandResult>`: `None` when a job of the op cannot be recorded within `supervisor::record`'s budget. The runner treats `None` as it treats an effect that ended without a result (`commands.rs:681-688`): no receipt completion (it stays `in_progress` and becomes `unknown_after_crash` at the next start), 503 `outcome_unknown` at once, and `StoreFatal::ReceiptUncompleted` to the sink. A handler cannot reply 503 itself (`result_allowed`, `commands.rs:152-176`), and a stored 500 would claim an outcome nobody knows.

### 2.4 Death Proof

**While the daemon owns the session**, only the explicit close decides (§2.3), with the full list and a join that returned in time. The `Session` leaves the slot only into that `close`; a `Session` dropped elsewhere (a panic, a blocking task outliving the daemon's shutdown) closes with `&[]` and is never recorded as a proof (A27).

**`extra` and its source** (B1, A3, A26). The supervisor passes, on every call, every identity it can prove belongs to the launch: its own spawn records and containment it can show (hook-ingest D10). In the MVP no such identity exists outside what the backend already covers: job members (Windows) and session and group members observed while owned (macOS) are the backend's, the daemon creates no other process per launch, and PR 9 records none (hook-ingest D10). So the full list is the empty list on every call; a descendant that left the session before any observation is PR3-P-1's stated limit. A later producer records identities on the `LiveLaunch` and they are passed from then on.

**Evidence.** A new thread-stream event kind records, for the owner's assertion, what a kill or probe checked and what it could not. It changes no state, and the projection ignores it (a no-op arm in `project.rs`).

```rust
// crates/core/src/payload.rs (new)
pub struct LaunchEvidence {
    pub launch_id: LaunchId,
    pub source: EvidenceSource,         // spawn_failed | close | probe
    pub identities: Vec<IdentityFate>,
    pub unchecked: Vec<UncheckedScope>, // unrecorded_session_members | job_members | no_identity | close_unreported
}
pub struct IdentityFate { pub pid: u32, pub start_time: Option<String>, pub fate: Fate } // gone | replaced | survived | invalid
```

| Source | Written when | `identities` | `unchecked` | Idempotency key |
|---|---|---|---|---|
| `spawn_failed` | `StartedThenFailed` with `Cleanup::Unresolved` | the `identity`, if any, as `survived`; `Unresolved.kill`'s survivors as `survived` | `no_identity` when `identity` is `None` | `launch:<L>:evidence:spawn_failed` |
| `close` | an owned close that returned `tree_dead: false`, or whose result was not received in time | the report's survivors as `survived` (a survivor read without a start has `start_time: None`) | `close_unreported` when the result was not received | `launch:<L>:evidence:close` |
| `probe` | the restart prober or a `stop` probed a `daemon_restart` launch (below) | each given identity with its fate | `unrecorded_session_members` (macOS) or `job_members` (Windows) | `launch:<L>:evidence:probe:<attempt>`: the `stop`'s `command_id`, or a UUID the prober mints per launch |

The first two happen at most once per launch, as `launch:<L>:exited` does (mvp §2.1 "Idempotency keys"). A probe's attempt id is fixed before it runs: a retried record job carries the same result and key, and a replayed `stop` returns its stored reply without probing.

**After a restart** (A1, F1, A28). The new daemon owns no old session: the job handle and the PTY master died with the old daemon, and on macOS the leader was reparented. pty-runtime's rules then allow only signals to verified identities and never `tree_dead: true` (pty-unix §2.5 "Ownership for the call", §4 row "Daemon restart"; pty-windows §4 row "Daemon restart"). 10a adds one entry point that keeps to that:

```rust
// crates/pty-runtime/src/orphan.rs (new)
pub struct OrphanReport {
    /// Every given identity is gone or its pid holds another process; true for an empty list.
    pub all_gone: bool,
    pub fates: Vec<(ProcessIdentity, OrphanFate)>, // gone | replaced | survived | invalid
}
pub fn kill_recorded(identities: &[ProcessIdentity], deadline: Instant) -> OrphanReport;
```

- **Validation.** An identity with pid 0, 1 (macOS) or 4 (Windows), the daemon's own pid, or a `start_time` that does not parse is `invalid`: never signalled, an unresolved survivor. An empty list proves nothing and returns `all_gone: true` with no OS call.
- **macOS:** a loop over the given identities only. Each read is the full libproc record and, when that is refused, the short one (`unix/table.rs:30-51`); each signal is one `kill(2)` to a pid whose read showed the recorded start. `host::act_before(deadline, …)` precedes every one of these calls. HUP, then reads every 25 ms until `t0 + min(500 ms, B/4)`; TERM to the still-live ones, reads until `t0 + min(2 s, B/2)` (the engine's timing, pty-unix §2.5 step 3, with `B` = deadline − `t0`); KILL; reads until the deadline. No `pid_list`, no `getsid`, no group or session signal, no use of `kill_tree`'s rounds, which list every pid and cover descendants by parent (`unix/kill.rs:191-205`). A read showing `Gone` resolves an identity as `gone`, `Live` with another start as `replaced`; anything else at the deadline is `survived`.
- **Windows:** first, per identity, the backend's verified open (`OpenProcess` + `GetProcessTimes` on that handle; `ERROR_INVALID_PARAMETER` → `gone`, another start → `replaced`); then `TerminateProcess` on every verified handle; then one wait on those handles with the time left to the deadline; signalled → `gone`, else `survived` (pty-windows §2.5 "Extras"). Each call is preceded by `act_before`.

**Probe.** A launch `L` with `exit_reason = daemon_restart`, death unconfirmed, and an identity from `launch_started` is probed by the restart prober (§2.5), once per start that recorded it, and by a `stop` of the current `L` (SD19). Each probe holds `L`'s probe lock, a per-launch entry in the supervisor that exists only while held: the prober skips a launch whose lock a `stop` holds, and a `stop` waits for it with `settle_before(…, now + 10 s)` (`Elapsed` or `Late` → 504 `launch_busy`). The `stop` then re-reads `L` (still current, still unconfirmed, still `daemon_restart`, the same identity; otherwise the results table answers); the prober needs no re-read, since `kill_recorded` signals only verified identities. The probe runs `kill_recorded([leader] ++ extra, now + 5 s)` on a blocking thread awaited by `settle_before` until 6 s and appends `launch_evidence { source: probe }`. It never confirms death and never covers more than the recorded identities. A `stop`'s worst case: 10 s + 6 s + one job, within the 30 s deadline.

**Results under the owner decision.** One row per state of the current launch, as the intent and op find it:

| Current launch | `stop` | `close --stop` | `stop --assume-dead --launch L` |
|---|---|---|---|
| live (started in this daemon run, or starting) | Stop (§2.3): `stopped`, `death_confirmed` from the close | Stop, then the closing job: closed, or `death_unconfirmed` when the close did not confirm | `launch_live` |
| exited, death confirmed | `already_exited` | the closing job | `already_exited`, nothing written |
| exited `daemon_restart`, unconfirmed, identity recorded | Probe: `probed`, `recorded_gone` = `all_gone`, death stays unconfirmed | `death_unconfirmed` | `asserted`: `launch_death_confirmed { method: "owner_asserted" }` |
| exited otherwise unconfirmed (an owned close that could not prove, `StartedThenFailed` unresolved, `unknown_after_crash`) | `death_unconfirmed` | `death_unconfirmed` | `asserted` |

Before the listener exists, a restart guarantees only the records of §2.5 (exits, input recovery, token revocation); evidence from a probe is a later observation, made by the restart prober after `attach` or by a `stop`.

**Owner assertion** (A2, A29). `chaldea stop <thread> --assume-dead` (owner token) without `--launch` sends nothing. It reads the thread's snapshot (`GET /api/events?stream_id=<thread>&after_seq=9007199254740991`, the largest accepted value (`MAX_SAFE_INTEGER`), which `decide_replay` answers with a `CursorAhead` snapshot carrying `ThreadInfo` and the current `LaunchInfo`), then pages backward (`before_seq`) from the snapshot's `as_of_seq` + 1 down to that launch's `launch_reserved`, and prints the launch id, exit reason, death state, and each of its `launch_evidence` events; the stream read's own authorization applies. With `--launch L` it sends `StopArgs { assume_dead: true, expected_launch_id: L }`; the intent refuses `launch_changed` unless `L` is current and `launch_live` unless `L` exited, and then, if death is unconfirmed, appends `launch_death_confirmed { method: "owner_asserted" }` and `StateInput::DeathConfirmed` (clearing `resume_blocked`). A settled launch is answered `already_exited` with nothing written.

**One confirmation per launch.** Confirmation comes only from an exit job's owned close or from an owner assertion; a probe appends evidence only. An assertion's job re-reads `L` and appends only if `L` is still current, exited, and unconfirmed; anything else is a no-op that returns the settled state, so two concurrent assertions confirm once. Probes of one launch are serialized by its probe lock; each appends its own evidence under its own key.

**A29 on Windows.** A crash between the suspended `CreateProcessW` and `AssignProcessToJobObject` leaves a reservation without identity (`unknown_after_crash`) and a suspended child outside the launch's job (pty-windows §2.2 "Crash window"). That child is in no Chaldea launch job, but it may be in a job the daemon itself ran in: the create flags have no breakaway (`windows/sys.rs:378-382`), and a child created by a process in a job is associated with that job by default (Job Objects). 10a keeps the suspended fallback and gives PR 21's finder the launch id, its agent, and the reservation's `recorded_at`. Its candidates are suspended processes not in a Chaldea launch job, whether or not in another job, whose image is that agent's executable and whose creation time is not before the reservation (wall clock); program and time are shown as candidate evidence. The doctor kills a candidate only on the owner's confirmation; a candidate is never a death proof (SD2).

### 2.5 Restart

**Reconcilers** (E3; daemon-lifecycle §2.14: each its own job, at step 9, before the listener exists), returned by `SupervisorComponents::reconcilers` in this order:

1. `receipts_mark_unknown_after_crash` (PR 8, unchanged).
2. `supervisor_launches`, one job:
   1. For each `open_launches()` row with `exit_reason` null, in `(thread_id, reserved_seq)` order: `launch_exited { reason: daemon_restart` if `pid` is recorded, else `unknown_after_crash`; `code`, `signal` null; `death_confirmed: false }` with `StateInput::Exited`, then the exit steps of §2.11 with `restart: true`.
   2. Every input of any thread in `queued`, `sending`, or `sent` (`ReadTx::inputs_in_states`): `queued → cancelled`, `sending`/`sent → unconfirmed` through `InputInput::DaemonRestarted`, raising `input_unconfirmed`. Inputs already `unconfirmed` keep their state and attention (`step(Unconfirmed, DaemonRestarted)` is invalid).
   3. `token_revoke(TokenSelector::LaunchScoped)`: every token bound to a launch, exited ones included, since none of them is current and live after a restart (hook-ingest §2.8: revocation at restart ends a hook token). Owner tokens have no launch and stay.
   A second restart finds nothing to change: no open launch, no input in those states, no unrevoked launch token.
3. `supervisor_buffers`, one job: `buffer::resolve_unbindable` for each `buffered_launches()` (A22; every launch is exited by now). It keeps daemon-lifecycle's order "launches, then buffered deliveries" and covers buffers left by launches that exited in an earlier run.

`ExitStep`s and `TickStep`s are assembled on `SupervisorComponents` when it is constructed (before step 9) and shared with the `Supervisor` that `build` creates, so 10b's steps run at restart too. No nudge is needed at restart: no socket exists before the listener. The positional input of a launch that crashed before `launch_started` ends `unconfirmed` through step 2.2 (A21).

**Restart prober** (SD19). `attach` starts one task with the `daemon_restart` launches that `supervisor_launches` recorded in this start (the job's result, kept on `SupervisorComponents`). It probes them as §2.4 "Probe" says, at most 4 at a time, skipping a launch whose probe lock a `stop` holds. A probe whose evidence cannot be recorded within `supervisor::record`'s budget is dropped, and nothing is probed again later; a `stop` probes on demand. `SupervisorConfig` can hold the prober for tests; the participant stops it (§2.9).

### 2.6 Startup Causes and the Tick

One supervisor task ticks every second; its timer is the new exception `supervisor::tick_timer` (`sleep_until`). `Supervisor::tick(now)` is the seam tests drive with an injected instant. Each tick reaps the leftover children (`try_wait`, §2.3 step 1), takes a candidate list from the live map, and then runs, per candidate, a job that re-validates everything; registered `TickStep`s (10b's gate timeout and ack window) run after 10a's:

- **`session_start_timeout`** (D4, A32): a candidate is a non-manual started launch with `now ≥ started.at + 20 s` (mvp §2.2:142) on the supervisor's monotonic clock. The job raises `AttentionCause::SessionStartTimeout` only if the launch is the thread's current launch, has no `exit_reason`, has no `session_id` (unbound), and the cause is absent. The condition is "unbound", not "no `SessionStart` seen": a `SessionStart` whose walk ends below the launch is `foreign_session` and leaves the launch unbound, and hook-ingest §2.5 (line 268) assigns reporting it to this cause (Quint change in §7).
- **`buffer::expire`** for each live, unbound launch with buffered deliveries (A22); its age is decided by store stamps as PR 9 designed (`buffer.rs:92-106`).

A tick job that cannot be recorded within its budget is skipped; the next tick decides again from the store. **`startup_stall`** (A32) is raised in the start job for a Codex launch whose plan says `upgraded`; `upgraded` is false until PR 14 supplies the CLI-version comparison (mvp §2.7 "Hook trust"). Both causes are cleared by the reducer on `StateInput::SessionBound` and `StateInput::Exited`. Hole c (formal-specs §3) is closed by the job-side guards: nothing raises a startup cause for an exited or non-current launch.

### 2.7 Launch Plan: argv, Environment, Token

`launch::plan(&LaunchInput) -> LaunchPlan` (pure, `core`) builds the arguments after the program from mvp §2.7's rows "Session binding", "First prompt", "Extra flags", and "Resume", and owns `ENV_SCRUB`:

| Launch | Claude | Codex |
|---|---|---|
| Fresh | `--session-id <launch_id>` (lowercase hyphenated, as `MvpHooks` `expected_session` computes) | `--no-daemon` [Windows: `-c tui.disable_paste_burst=true`] [`--` `<prompt>`] |
| Resume | `--resume <thread session_id>` | `resume` `--no-daemon` [Windows flag] `<session_id>` [`--` `<prompt>`] |

The prompt is one argv element after a `--` that ends the argv (input-encoders D5). The "Hook injection" row (Claude `--settings`, Codex `-c hooks.<Event>`) is PR 13's and PR 14's addition to `plan` (SD14). `LaunchInput` carries `worktree_id`, which PR 16's spawn path sets when the cwd is a managed worktree. `manual` = Codex without a prompt.

**Program.** The daemon resolves `claude` or `codex` on its own `PATH` to an absolute path of a native executable (`SpawnSpec` requires one, `backend.rs:34`). Resolving a Windows npm shim to its native binary is PR 14's. Tests replace the program through `SupervisorConfig` with `chaldea-fake-cli <mode flags> --`, so FakeCli ignores the adapter arguments and `report-process` records them.

**Environment** (`SpawnSpec.env` is complete; nothing is inherited): the daemon's environment minus `ENV_SCRUB`, plus `CHALDEA_URL` (`http://127.0.0.1:<port>`, mvp §2.5:259), `CHALDEA_HOOK_URL` (`POST /hook` on that port), `CHALDEA_HOOK_TOKEN` (this launch's hook token), `CHALDEA_THREAD_ID`, and on macOS `TERM=xterm-256color`, replacing the daemon's own `TERM` (the VT is the daemon's, not the terminal the daemon was started from); on Windows the daemon's `TERM`, if any, passes unchanged. `ENV_SCRUB` is the identity variables of mvp §2.7 "Env scrub", by name: `CLAUDECODE`, `CLAUDE_CODE_CHILD_SESSION`, `CLAUDE_CODE_SESSION_ID`, `CLAUDE_CODE_MESSAGING_TOKEN`, `CODEX_THREAD_ID`, `CODEX_SESSION_ID`, and every name with the prefix `CHALDEA_` (prior-art row 6c; SD13). The daemon applies it to its `OsStr` keys with `pty_runtime::{env_key_eq, env_key_has_prefix}`, the comparison the backend's environment block uses: on Windows `CompareStringOrdinal(…, TRUE)` over the key's UTF-16 units (the prefix test compares the key's first units, as many as the prefix has; `windows/cmdline.rs:12-13`), so `Chaldea_Hook_Token` is scrubbed there instead of reaching the block as a duplicate key that refuses the spawn (`windows/cmdline.rs:84-90`); exact on macOS. `CHALDEA_AGENT_TOKEN` is PR 16's.

**Token.** One hook token per launch, inserted by the reservation job (D2). Its validity follows hook-ingest §2.8: kept after the launch exits, so late hooks of a live daemon are stored as `raw_unknown`; revoked by close (the thread's tokens) and by the restart reconciler (every launch token). Agent tokens are PR 16's, added to the same job.

### 2.8 `GET /api/adapters` and the Host OS

`HostOs::from_os_str(std::env::consts::OS)` maps `"macos"` and `"windows"`; anything else is `None`, and `chaldea daemon run` exits `Failed` before startup with a message (C7). The CLI computes it once (`Seams.host_os` replaces the string in tests) and passes it to `SupervisorComponents`; the `Supervisor` passes it by value into every `prepare`.

The route (`Mount::read`, `ReadAdapters`, Bearer owner, agent, rescue; the existing Host and Origin rules) answers from `CAPABILITIES` with no store read (C6; SD5):

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

### 2.9 Components, Shutdown, and Fatal Outcomes

**Components.** `chaldea daemon run` builds `SupervisorComponents::new(host_os, SupervisorConfig::default())` instead of `MvpComponents` (`daemon_cmd.rs:136-139`; tests pass their own through `Seams.components`). Its `reconcilers` are §2.5's; its `build` creates the `Supervisor`, the four handlers, the participant, and the mounts `Mounts::mvp_with(hook_seams, adapters)`, where `hook_seams` is `HookSeams::mvp()` in 10a and the construction point where 10b supplies its `IntakeProbe` and PR 13/14 their `HookAdapter`; its `attach` takes the `RevocationNudge` and starts the tick and the restart prober.

**Admission.** The supervisor's map mutex also guards `accepting` and the set of creations in flight. `Supervisor::admit` registers a creation and returns its guard only while `accepting`; the guard ends when the created child is owned (the `Session` in its slot, the git child reaped or in the leftover set) or the creation failed. Before the participant runs, HTTP's drain has requested the abort of the runners still running at its deadline (`http/mod.rs:364-376`), but ops outlive their runners (§2.1 rule 2), so creations can continue until the participant closes admission. Every process the supervisor creates after that is in the participant's snapshot, or is never created.

**Shutdown** (SD10). The participant gets `end` (participants share the 2 s between `C − 5 s` and `C − 3 s`, daemon-lifecycle §2.14); with `t0` its start, `r = end − t0`, `j = min(300 ms, r/4)`, `lock_by = t0 + r/4`, and `close_end = end − j`:

1. Under the map mutex: `accepting = false`, and a snapshot of the creations in flight and of the started live launches. The tick and the restart prober stop (probes in flight finish on their blocking threads).
2. By `lock_by`: each started launch's op lock (`settle_before`, `in_time()`; a lock not taken is left), and the end of each creation in flight, whose session, if any, joins the started ones.
3. For each locked launch in parallel: `Session::seal`, then `Session::close` with `CloseBounds::default()` scaled by `(close_end − now) / CloseBounds::default().total()` (`limits.rs:61-78`), joined with `settle_before` until `close_end`. With `r = 2 s` a close starting at `lock_by` gets 1.2 s, its kill 545 ms: the HUP and TERM waits end at 136 ms and 272 ms (pty-unix §2.5 step 3).
4. For each close whose result was received: one job, submitted with `not_after = end` and its reply awaited until `end`: `StateInput::StopRequested` if the launch is `running` (none if `stopping`; no job if already exited), `launch_exited { reason: stopped }` with the close's `tree_dead`, the method when confirmed, the survivors' `launch_evidence { source: close }` when not, and the exit steps.
5. Returns `ParticipantEnd::LeftRunning` if any creation, blocking close or probe, or leftover child of the supervisor is still running, else `Clean`. The coordinator's lock rule counts `LeftRunning`, and a participant not done by `end`, as a reason to exit with the instance lock held (daemon-lifecycle §2.8 step 6), so no new daemon starts while a child the old one created may still appear.

Each launch ends in one of these classes:

| Class | Record | At the next start |
|---|---|---|
| close confirmed, commit observed | `stopped`, confirmed | nothing |
| close not confirmed, commit observed | `stopped`, unconfirmed, survivors as evidence | the owner asserts (L-7) |
| close result not received, or lock not taken | none | `daemon_restart`; the owner's `stop` probes (§2.4) |
| commit not observed by `end` | either of the first two, or none | as recorded |

An op refused admission after its reservation commits records the `NeverStarted` branch with `not_after = end`; if that does not commit, the next start records `unknown_after_crash` (L-3). Once admission is closed, a job an op cannot record ends the op without a fatal report. A `Session` a blocking task still holds at process exit is dropped there, never a proof.

**Fatal outcomes.** Before shutdown: an exit op that cannot take its lock or record within 60 s reports `StoreFatal::LaunchUnrecorded` through `BuildCtx::sink`; a command op that cannot record resolves its effect to `None`, and the runner reports `ReceiptUncompleted` (§2.3). Nothing else in the supervisor stops the daemon. If a stop's `StopRequested` job cannot be recorded, the op ends with `None`; the sealed session stays in its slot, and the participant of the shutdown that the fatal starts closes it. Children: the PTY children through the backend (which takes the spawn lock), git through `spawn_locked_before`; no other process is created (A25). The CLI verbs use `cli::client` (`discover`, `read_owner_token`, `request`, `post_json`).

### 2.10 Waits

| Wait | Helper | Bound |
|---|---|---|
| Op lock (stop) | `deadline::settle_before` on the lock future, `in_time()` | 10 s; `Elapsed` or `Late` → `launch_busy` |
| Probe lock (stop) | the same | 10 s; `Elapsed` or `Late` → `launch_busy` |
| Restart prober | at most 4 probes at a time; the prober skips a held probe lock | each probe's `kill_recorded` 5 s + 1 s join; an unrecorded result is dropped |
| Op lock (exit op) | the same, in rounds | 10 s rounds, 60 s in all, then `StoreFatal::LaunchUnrecorded` (none once shutdown started) |
| Op lock and creations (participant) | the same | `lock_by` |
| `Session::close` (stop, exit op), `kill_recorded` on `spawn_blocking` | `deadline::settle_before` on the join handle, `in_time()` | their own bounds (`CloseBounds::default()`; the given deadline) + 1 s; otherwise unconfirmed |
| `Session::close` (participant) | the same | `close_end` (§2.9) |
| Store jobs (`supervisor::record`) | the runner's receipt-completion policy (`commands::complete`): `write_with` attempts of `COMPLETION_ATTEMPT`, retried only on `retryable` outcomes (`QueueFull`, `Expired`, `Busy`, `RolledBack(Busy)`) with `backoff_until` from `BACKOFF_FIRST` doubling to `BACKOFF_MAX`, admitted for `COMPLETION_BUDGET`; an admitted attempt is awaited to the writer's reply | then the op ends without a result (§2.9 "Fatal outcomes"); tick jobs are skipped. `CommitUnknown`, `WriterGone`, and `Failed` are reported by `write_with` and end the op; a job's own refusal is a value, not a failure |
| Store jobs (participant) | `write_with` with `not_after = end`; the reply under `settle_before` | `end`; otherwise "commit not observed" |
| Git probe | `spawn_locked_before`, then `host::observed_before` polls of `try_wait` | 5 s; then left to the tick's reaping |
| Tick | `supervisor::tick_timer` (new exception, `sleep_until`) | 1 s period |
| CLI `wait`, reads, and retries | `poll_until_with` | one cutoff, also passed to each request (SD16) |
| **`Session::spawn`** | **none** | Cancelling it cannot stop it: the blocking call would still create the child. The op, not the request, awaits it; its bound is the spawn lock's holders, each of which does bounded work (pty-unix:122; pty-windows §2.2 "`spawn` has no deadline of its own"). The request replies at its own deadline through the runner (409 `in_progress`). |

### 2.11 Interfaces for 10b

```rust
// crates/daemon/src/supervisor/live.rs (new)
pub(crate) struct LiveLaunch {
    pub thread_id: ThreadId,
    pub launch_id: LaunchId,
    pub agent: Agent,
    pub caps: &'static Capabilities,
    started: OnceLock<Started>,                  // set at publication (§2.3 step 5)
    ops: Arc<tokio::sync::Mutex<Option<Session>>>,
}
pub(crate) struct Started {
    pub observer: WriterObserver, pub reference: u64,
    pub identity: ProcessIdentity, pub at: std::time::Instant,
}
pub(crate) enum SessionError { Gone, Busy }
pub(crate) enum LaunchObservation {
    Reserved,                                            // no session yet
    Started { reference: u64, last_started: [u64; 3] },  // per OriginClass::index
    Unobservable,                                        // no live entry for this launch
}

impl Supervisor {
    pub(crate) fn live(&self, thread: ThreadId, launch: LaunchId) -> Option<Arc<LiveLaunch>>;
    /// Never waits on `ops`; safe under the hook intake mutex.
    pub(crate) fn observe(&self, launch: LaunchId) -> LaunchObservation;
}
impl LiveLaunch {
    /// Takes the op lock by `deadline` and runs `f` inline; `f` must not block (`submit`, `modes`).
    pub(crate) async fn with_session<R>(
        &self, deadline: Instant, f: impl FnOnce(&Session) -> R,
    ) -> Result<R, SessionError>;
}

// crates/pty-runtime/src/session.rs (new)
impl Session {
    pub fn seal(&self);                          // close's step 1 alone; idempotent
    pub fn observer(&self) -> WriterObserver;    // Clone; outlives the Session
}
impl WriterObserver {
    pub fn last_started(&self, classes: ClassSet) -> u64;
    pub fn last_started_each(&self) -> [u64; 3]; // one queue lock, per OriginClass::index
}

pub(crate) trait ExitStep: Send + Sync {
    fn in_exit_job(&self, tx: &mut WriteTx<'_>, exit: &ExitFacts) -> Result<(), StoreError>;
    fn after_commit(&self, _exit: &ExitFacts) {}
}
pub(crate) struct ExitFacts {
    pub thread_id: ThreadId, pub launch_id: LaunchId, pub reason: ExitReason,
    pub death_confirmed: bool, pub exited_seq: u64, pub restart: bool,
}
pub(crate) trait TickStep: Send + Sync {
    fn tick(&self, now: std::time::Instant, live: &[Arc<LiveLaunch>]) -> Vec<supervisor::record::Job>;
}
// crates/daemon/src/supervisor/record.rs (new): one job, run again on each attempt.
pub(crate) type Job = Arc<dyn Fn(&mut WriteTx<'_>) -> Result<(), JobError> + Send + Sync>;
```

- **Lookup by generation.** The live map is keyed by `LaunchId`; `live(thread, launch)` returns the entry only if its `thread_id` is `thread`, so a call cannot reach another thread's or a newer launch.
- **Access rule.** The map sits under a `std::sync::Mutex` held only for map operations and admission, never across an await, a lock, or a job; `observe` takes it. Every use of a `Session` goes through `with_session`; no code keeps a `&Session` or the guard after `f` returns. `Gone`: the session left the slot (exit recorded or being recorded); `Busy`: the lock was not taken by `deadline` (`Elapsed` or `Late`, a `Late` guard dropped at once). 10b submits a unit inside `f` and awaits its completion outside it. The supervisor's own blocking calls (`close`) are not run through `with_session`.
- **Observation.** `observe` reads the entry and its `Started` without touching `ops`: no entry → `Unobservable`; no `Started` → `Reserved` (a hook before the session existed: no write can precede it); else `Started { reference, last_started: observer.last_started_each() }`, which takes the writer's queue lock, held by the writer only for permission decisions (pty-runtime §2.3 "Write permission"). A class position above `reference` is a write of that class after the reference. `Started` is published at §2.3 step 5, before the start job, so a hook arriving between the session's creation and the `launch_started` commit sees it. PR 9 samples intake facts once, before the pid wait (`hook/mod.rs:224-230`), so 10b's `IntakeProbe` calls `observe` there and encodes the observation as its `IntakeFacts`.
- **Stop before write** (SD18). `Session::seal` runs close's step 1 alone: the queue closes and every unit not yet permitted is settled `Closed` with zero bytes, its `Completion` run before `seal` returns; close's own step 1 then finds the queue closed. A stop seals before its `StopRequested` job, holding the op lock, so no unit is permitted once the state is `stopping`; a unit permitted before the seal follows the PTY contract (it may still reach the backend, pty-runtime §2.3 "Write permission"). From the seal to the `StopRequested` commit the thread reads `running` and every submission is refused `Closed`. Every exit job of a sealed launch runs after the seal (the stop op holds the lock from the seal to its exit job; the exit op takes the lock after), so 10b's exit step can read each settled completion without waiting and record its zero bytes before 10a's input sweep.
- **Lock holders.** A spawn op (slot insertion → start job commit), a stop op (lock → exit job commit), an exit op (lock → exit job commit), the participant, and 10b's calls, each bounded (§2.10). Order: the map mutex, then `ops`; a store job may run while `ops` is held, never the other way round.
- **Exit job order:** `launch_exited` with the reducer's `Exited`; then the registered `ExitStep`s in registration order (10b: the writer outcomes it can read without waiting, then its gate state; PR 13: card expiry, `expired`); then 10a's input sweep: the thread's inputs still in `queued`, `sending`, or `sent` whose `launch_id` is `L` or none (queued for the thread before the exit) move `queued → cancelled`, `sending`/`sent → unconfirmed` with `input_unconfirmed`, through `InputInput::LaunchExited`, or `InputInput::DaemonRestarted` when `restart` (mvp CL-6); `unconfirmed` inputs keep their state; then `buffer::resolve_unbindable`. The job passes the nudge as `after_ok` (it ends the launch's agent-token validity, PR 16). After commit the `after_commit` hooks run in order (10b: drop the launch's writer queue). A writer completion that arrives after `launch_exited` is 10b's to ignore in its own job (`exit_reason` not null).
- **Restart** runs the same steps with `restart: true` (§2.5); 10b adds only its gate state there.
- **Hook seams.** 10b's `IntakeProbe` (over `observe`) and PR 13/14's `HookAdapter` go into the `HookSeams` that `SupervisorComponents::build` passes to `Mounts::mvp_with` (§2.9).

## 3. Decisions

Decisions are numbered SD (supervisor decision), apart from the obligation ids of §4 such as D2 and D4. Each compares options by gain, loss, reversibility, and cost. "Public" marks a decision visible to users or clients.

**SD1. Nudge for revoking intents (E4 vs X1).** http-surface §2.4 says command PRs pass the revocation nudge as `after_ok` to `run_write`, but a command's intent job is the runner's (`commands.rs:665` passes `None`) and `Prepared` has no `after_ok`.

| Option | Gain | Loss | Reversibility | Cost |
|---|---|---|---|---|
| **A. `CommandHandler::revokes() -> bool` (default false); the runner passes the nudge as `after_ok` for that handler's intent job** | one-transaction close; one line in the runner; PR 16 and PR 20 reuse it | a nudge also after a refusal or a replay (harmless: "the nudge carries no decision; the database does", http-surface §2.12) | trait default; removable | small |
| B. `Prepared` gains `after_ok` | per-call choice | `prepare` needs the nudge, which exists only after `attach` | additive | small |
| C. close always `Pending`, revoking in the op's own job | no runner change | two jobs and a second check; a crash leaves `unknown_after_crash` for a plain close | reversible | medium |
| D. rely on the socket's periodic check | none | breaks http-surface §2.4 | — | — |

**Adopted: A.** The supervisor's own revoking jobs (close `--stop`'s closing job, the exit job) pass the `RevocationNudge` themselves; the restart reconciler needs none.

**SD2. One crash-window rule for A2 and A29.** Both leave a reservation without identity, recorded as `unknown_after_crash`. A29 is its Windows instance whose child also sits suspended outside the launch's job (pty-windows §2.2 "Crash window"); the job-list branch has no such window.

| Option | Gain | Loss | Reversibility | Cost |
|---|---|---|---|---|
| **A. No automatic confirmation; owner assertion bound to the launch; PR 21's finder gets the launch id, agent, and `recorded_at`; the suspended fallback stays** | promises nothing the record cannot show; launches keep working where the job list is refused | the owner acts once per such launch; the suspended child stays until the doctor or the owner ends it | the finder can be added later without a record change | small |
| B. Refuse the suspended fallback | no unrecorded suspended child | every host or case where the job-list attempt fails (pty-windows §2.2 stage 5, "Suspended attempt") can no longer launch | a pty-windows behaviour change, reversible | small |
| C. Census by image name and creation time after restart, as proof | finds candidates | a match by image and time proves no provenance (pty-windows §2.5 "Extras are trusted input") | — | large |

**Adopted: A.** Public: the `stop` flag, the death method `owner_asserted`, and the `launch_evidence` event.

**SD3. Restart death proof (A1, F1).** mvp §4 asks for a restart scan of the launch's session by recorded sid with `launch.start_time ≤ start ≤ restart_now`.

| Option | Gain | Loss | Reversibility | Cost |
|---|---|---|---|---|
| A. The sid scan with a start window | covers session members | start stamps are wall-clock stamps used only for equality (pty-unix §2.1 "Identity format"; pty-windows §2.1); without ownership, sid `L` can name a later session once `L` is reused, so a clean scan proves nothing: pty-unix's census is sound only while the daemon owns the leader (pty-unix §2.4 "Ownership is a precondition") | — | large |
| **B. `kill_recorded` over the recorded identities, its scope recorded as evidence, confirmation by the owner** | no census, no unowned group or session signal, no clock; the owner sees what was not checked | unrecorded members of the old tree are not signalled or checked (L-1, L-2), and every crash needs an owner step | a later census-free source of identities widens what the evidence covers | small |
| C. Reuse `kill_tree` with an unowned `Os` | reuses the engine | its rounds still list every pid and cover descendants by parent (`unix/kill.rs:191-205`), a census in all but name | — | small |

**Adopted: B**, with the owner decision of 2026-11-25: a check that covers only the recorded identities unlocks nothing. The rejected alternative confirmed death after the check and raised an attention cause naming the unchecked scope; it let resume, close, and worktree release proceed while an unrecorded child could still run in the old cwd. Public: P-18's limit and mvp §2.9's restart text change (§8).

**SD4. Stop while starting (X5).** `state.rs:255-258` says the daemon holds such a stop until the spawn outcome is recorded.

| Option | Gain | Loss | Reversibility | Cost |
|---|---|---|---|---|
| **A. The stop op waits on the launch's op lock (10 s), then decides on the recorded outcome** | matches the reducer's rule with existing parts; the slot precedes the reservation, so there is always a lock to wait on | a stop may wait for a slow spawn | — | small |
| B. A new 409 code; the client retries | no wait | every client (CLI, PWA, Rescue button) repeats the retry | additive | small |
| C. A queued stop applied by the start job | no waiting request | cross-call state | — | medium |

**Adopted: A.** A lock not taken in 10 s is 504 `launch_busy` and the stop does not happen later.

**SD5. `GET /api/adapters` wire shape (C6). Public.**

| Option | Gain | Loss | Reversibility | Cost |
|---|---|---|---|---|
| **A. `AdaptersResponse { host_os, adapters: [AdapterInfo { agent, pty_encoding, first_prompt }] }`, this host's rows, open enums** | small; every field exists in `CAPABILITIES`; additive later | clients learn other hosts' rows only from those hosts | fields and rows can be added | small |
| B. All four rows | one static catalog everywhere | rows a client of this host cannot use; removing them later breaks clients | — | small |
| C. The whole mvp §2.7 table as fields | complete | most columns are PR 12–14's and unimplemented | — | large |
| D. A free-form map | no schema churn | no contract for clients | — | small |

**Adopted: A.** What would change it: a client that must plan launches for another OS before contacting that host.

**SD6. Who attributes `extra` (B1, mvp:437 "PR 9 / 10").** hook-ingest D10 gives the obligation to PR 10 entirely and PR 9 records none. Adopted: PR 10; in the MVP the list is empty (§2.4). Rejected: recording hook-emitter ancestry as `extra` (a matching creation time identifies a process, not its provenance; pty-windows §2.5 "Extras are trusted input").

**SD7. Restart reconciler order.** Adopted: receipts → supervisor launches (with inputs and tokens) → supervisor buffers (§2.5). Alternatives: inputs in a separate job after launches (a crash between them leaves exited launches with inputs still `sending`: the next restart fixes it, but one job is simpler); buffers before launches (rejected by daemon-lifecycle §2.14: the launches step decides which launches can no longer bind).

**SD8. Exit hook points for 10b.** Adopted: the ordered `ExitStep` list inside the one exit job, assembled before step 9, with 10a's input sweep after it (§2.11). Alternatives: events 10b subscribes to after commit (a gap between `launch_exited` and 10b's state, which mvp CL-6 forbids for inputs); 10b writing its own exit job (two exit records to order); steps registered on the `Supervisor` at `build` (step 11 comes after the reconcilers, so restart would miss them).

**SD9. The positional input without 10b.** Adopted: created `sending` with the reservation; `sent` with `sent_after_seq` = `launch_started`'s seq in the start job, with `reference` as its writer anchor; `rejected(spawn_failed)` when no process ran; `unconfirmed` on `StartedThenFailed`, exit, or restart. 10a has no ack, so a launched positional input ends `unconfirmed` at exit until 10b attributes its ack (mvp §2.3). Rejected: leaving it `sending` until 10b (mvp CL-6 forbids an input still in flight after exit).

**SD10. Sessions at shutdown. Public.** Adopted: one admission point closes process creation, and the participant tries an owned close of every live session in parallel against one cutoff (`end`), recording what it received and leaving the rest to the next start (§2.9). Rejected: leaving every session to process exit (every graceful stop would end in owner assertions); killing sequentially (the share is 2 s for all sessions); closing only the sessions started when the participant runs (a creation released after the HTTP drain would make a child no one closes or records).

**SD11. Reservation inside the op, slot first.** Adopted: both spawn and resume reserve in the op, after inserting the locked slot; a refusal or rollback removes it. Rejected: resume reserving in its intent (a stop arriving between the intent's commit and the slot's insertion would find a `starting` launch with no lock to wait on); reading `.git` files instead of running git (re-implements git's discovery, which `workspace_id`'s definition names, mvp §2.1:103).

**SD12. Session access.** Adopted: one async op lock holding `Option<Session>` per launch (§2.11), with the writer's position read through a `WriterObserver` outside it. Rejected: `Arc<Session>` clones (no lock wait, but `close` consumes the `Session`, so the closer would wait for every clone to drop: cross-call state and a second wait); reading positions under `ops` (an early hook would meet the spawn op's lock and see nothing).

**SD13. Env scrub by name. Public.** mvp §2.7 lists the prefixes `CLAUDE_CODE_*` and "other `CODEX_*` session variables". Both prefixes also carry user configuration (Claude Code's settings page lists `CLAUDE_CODE_*` variables that select the provider and behaviour; `CODEX_HOME` selects Codex's home); dropping them changes how the user's CLI runs, against mvp P-1 (the CLI runs with the user's own configuration). Adopted: the named identity variables of prior-art row 6c plus `CODEX_SESSION_ID` and every `CHALDEA_*`, compared as the backend compares keys; a new identity variable is a list entry (PR 13/14 per CLI version).

**SD14. Hook configuration stays PR 13's and PR 14's.** 10a supplies the token, the env, and the Claude session flag; which events each CLI reports is the adapters' (mvp §4 rows 13, 14; Codex's trust hash makes the event list part of the first release, mvp §2.7). Consequence: a real CLI launched by 10a alone never binds, and real same-session resume is PR 13's and PR 14's acceptance (L-6).

**SD15. No spike watchdog (F2).** The emergency kill is `chaldea stop` (and the Rescue button, PR 20, which calls it). A watchdog killing on CPU or time would decide orchestration in daemon code (invariant 2).

**SD16. `chaldea wait`. Public.** `chaldea wait <thread> [--until exited|idle|attention] [--timeout <dur>]` (default `exited`, 10 min). One cutoff, taken at start, is passed to `poll_until_with`, to discovery's retries, and to every request's connect and read (`client::request` takes it as `deadline`). Each poll reads the thread's snapshot (§2.4 "Owner assertion"): the condition holds on its `ThreadInfo` → print it as JSON, exit 0 (level-triggered: a condition already true returns at once). Discovery `NotRunning`, `Unknown`, `Starting`, or `Stopping`, a refused connection, a reset or EOF before a full response, 408, or 503 → keep polling; 404 → exit 1, "no such thread or not readable"; 401 or 403 → exit 1; any other status or a body that does not decode (over `MAX_BODY` included) → exit 1 with the reason; the cutoff → exit 4 (`ExitCode::Timeout`; 3 is `AlreadyRunning`). Alternative: an edge-triggered wait on the event stream (`--after <seq>`), left for 10b's `send` then `wait` pattern.

**SD17. Launch operations as supervisor tasks.** Adopted: rule 2 of §2.1, with `None` as the effect's "outcome unknown" (§2.3). Rejected: running spawn, stop, and close inside the runner's effect future (an abort at the drain deadline would drop the future while its blocking call runs on, leaving a `Session` or a close result with no owner); leaving the effect pending when a job cannot be recorded (the runner and the HTTP drain would wait for it until their deadlines, and the receipt's fate would depend on the abort).

**SD18. Stop before write.** Adopted: `Session::seal` before the `StopRequested` job (§2.11). Rejected: committing `StopRequested` and relying on close's step 1 (a unit whose recheck passed could be permitted after the state became `stopping`); a supervisor flag read by 10b's recheck (the recheck runs outside the queue lock, before the dispatch boundary, pty-runtime §2.3).

**SD19. One probe at restart, and on `stop`.** Under the owner decision a probe unlocks nothing, but it ends the old processes the daemon recorded, which mvp P-18 requires after a restart (no surviving old process beyond the stated limit). Adopted: one restart prober started at `attach` that probes each `daemon_restart` launch of this start once, at most 4 at a time, and a probe inside a `stop` of such a launch, serialized per launch by its probe lock (§2.4, §2.5). Rejected: probing only on `stop` (recorded processes would run until the owner acts, against P-18); re-probing on a schedule or after a failed record (a timer and a retry policy for processes `kill_recorded` already signalled to KILL; a `stop` probes again on demand); probing before the listener exists (startup would wait up to the probes' bound for evidence that unlocks nothing).

## 4. Obligation Inventory

Obligation ids number the PR 10 obligations collected from the sources in the second column; 10a and 10b share them.

| Id | Source | 10a's answer |
|---|---|---|
| A1 | mvp:439; pty-unix:409 | §2.4 after a restart and Probe, §2.5 restart prober; SD3, SD19; PR10a-CL-17 |
| A2 | mvp:439 | §2.4 owner assertion; SD2; PR10a-CL-8, PR10a-CL-27, PR10a-CL-35 |
| A3 | mvp:439 | §2.4 "`extra` and its source"; PR10a-CL-10 |
| A4 | mvp:439 | §2.3 Spawn and resume, Close; PR10a-CL-4, PR10a-CL-26 |
| A5 | mvp:439; mvp CL-18 | §2.3 step 3 (check and carry-over); PR10a-CL-3 (removal race: PR 16) |
| A6 | mvp:439; mvp CL-8 | own commands: PR10a-CL-5 |
| A13 | mvp:439; mvp CL-6 | §2.5, §2.11 exit steps; PR10a-CL-14 |
| A21 | mvp:439 | §2.5; PR10a-CL-8 |
| A22 | mvp:439; hook-ingest:381,565 | §2.5, §2.6, §2.11; PR10a-CL-15 |
| A24 | mvp:439; mvp CL-1 | §6 FakeCli `banner`; PR10a-CL-19 |
| A25 | mvp:439; daemon-lifecycle §2.11 | §2.3 Natural exit, §2.9; PR10a-CL-13, PR10a-CL-20, PR10a-CL-40 |
| A26 | mvp:439; pty-windows:202 | §2.1 rule 5, §2.4; PR10a-CL-10 |
| A27 | mvp:439 | §2.3 Stop, §2.4; PR10a-CL-10 |
| A28 | mvp:439; pty-windows:412 | §2.4 after a restart; PR10a-CL-17 |
| A29 | mvp:439; pty-windows:129,465 | §2.4 "A29 on Windows"; SD2; PR10a-CL-8 |
| A30 | mvp:439; formal-specs §3 hole a | §2.3 step 3; PR10a-CL-3; §7 |
| A32 | mvp:439; formal-specs §3 hole c | §2.6; PR10a-CL-18; §7 |
| A35 (anchor part) | mvp:439; formal-specs §3 | §2.3 step 5, §2.11 Observation; PR10a-CL-32 (the ack decision: 10b) |
| B1 | mvp:437; hook-ingest D10 | §2.4; SD6; PR10a-CL-10 |
| C1 | input-encoders:331,179 | §2.2 Prompt admission, §2.7; PR10a-CL-1, PR10a-CL-2 |
| C6 | input-encoders:319 and its D3; http-surface:461,471 | §2.8; SD5; PR10a-CL-25 |
| C7 | input-encoders:114 | §2.8; PR10a-CL-2 |
| D2 | hook-ingest:565,352 | §2.7, §2.3; PR10a-CL-6, PR10a-CL-7, PR10a-CL-34 |
| D4 | hook-ingest:268,565 | §2.6; PR10a-CL-18; §7 |
| E1 | http-surface:582,703 | §2.2 (`spawn`, `resume`, `stop`, `close`); PR10a-CL-22, PR10a-CL-39 |
| E2 | http-surface:505-531 | §2.2 codes, §2.3 "Requests and ops"; PR10a-CL-22, PR10a-CL-23, PR10a-CL-36 |
| E3 | http-surface:337; daemon-lifecycle:622 | §2.5; SD7; PR10a-CL-16 |
| E4, X1 | http-surface:333; `commands.rs:665` | SD1; PR10a-CL-21 |
| E5 | daemon-lifecycle:625-628 | §2.9; SD10, SD17; PR10a-CL-23, PR10a-CL-37, PR10a-CL-40 |
| E6 | daemon-lifecycle PR8-P-3b | PR10a-CL-24 |
| F1 | pty-unix:409 | §2.4 `orphan::kill_recorded`; SD3; PR10a-CL-17 |
| F2 | pty-unix:417 | SD15; PR10a-CL-11 (the emergency kill is `stop`) |
| X5 | `state.rs:255-258` | §2.3 Stop; SD4, SD11; PR10a-CL-12 |
| A38 | formal-specs:192 | own guards: §7; PR10a-CL-29 |

**Handed on.** A7–A12, A14–A20, A23, A31, A33, A34, A35's attribution, A36, A37, B2, B3, C2–C5, C8 for `send`, D1, D3, F3–F5, X2–X4: PR 10b (the send and ack path; 10a defines `prompt_too_large`, the observation, the anchor, and the stop-before-write order it uses). A5's removal race: PR 16 (it owns removal; 10a's reservation check and carry-over are the reading half). Hook configuration per CLI and real same-session resume: PR 13, PR 14 (SD14). A29's orphan finder: PR 21 (SD2).

## 5. Purpose Layer and Plan Layer

### 5.1 Purpose Layer: Acceptance Criteria

10a's acceptance covers native launch, stop, close, and restart with real CLIs on macOS, and binding and resume with FakeCli fixtures on both OSes; Windows is covered by CI with FakeCli (`windows-latest`). Real same-session resume, and so mvp P-18's "a resume shall continue the session", is an acceptance gate of PR 13 and PR 14 (L-6); a FakeCli success does not count for it. Evidence of real runs records variable names, never token values.

| ID | Criterion (EARS) | Source | Scene | Observable result / failure | How to check on the real CLI |
|---|---|---|---|---|---|
| PR10a-P-1 | When the owner runs `chaldea spawn --agent <claude\|codex> [--prompt <text>]`, the CLI shall run in a daemon-owned PTY with this launch's hook token, thread id, and daemon URL in its environment, the identity variables scrubbed, and a Codex prompt as one argument after `--`. | mvp §2.5, §2.7, §2.9; mvp P-1, P-2 | fresh thread, trusted repository | Pass: `SpawnResult.outcome = started`; thread `running`; argv and env as stated. Fail: missing or extra variables; prompt split or before `--`. | `ps -o pid,sid,args -p <pid>`; `ps eww -p <pid> \| tr ' ' '\n' \| cut -d= -f1` lists `CHALDEA_HOOK_TOKEN` and no `CLAUDECODE` (names only); the Codex TUI shows the prompt submitted. |
| PR10a-P-2 | When `chaldea stop` is run for a running thread, no process of its tree shall be alive at the command's reply, the launch shall be `exited` (`stopped`) with death confirmed, and an unrelated process shall survive. | mvp P-3; pty-unix PR3-P-1 | a CLI with a child process (an MCP server, or a shell command it started) | Pass: `death_confirmed: true`, `pgrep -s <sid>` empty, a sleeper started from another terminal alive. Fail: survivor; unrelated killed. | As stated; Codex and Claude each. |
| PR10a-P-3 | When the CLI exits by itself, the launch shall be `exited` (`exit`) with its code, and death confirmed when the close proves it. | mvp §2.9 | a CLI that exits (`/exit` through PR 11's attach, or a prompt that ends it) | Pass: one `launch_exited`; `code` set. Fail: two exits; `stopped`. | `chaldea wait <thread> --until exited` returns the thread JSON. |
| PR10a-P-4 | If the daemon is killed and restarted, then before the first command is served every open launch shall be `exited` (`daemon_restart` or `unknown_after_crash`) with death unconfirmed, every in-flight input recovered, and every launch token revoked; the restart prober shall then end the recorded old processes and record its evidence; a resume shall answer `death_unconfirmed` until the owner, after reading that evidence, asserts with `--assume-dead --launch`. | mvp §2.9; mvp P-18; owner decision 2026-11-25 | kill the daemon by PID mid-run | Pass: the first `GET /api/threads` after restart shows `exited`; no recorded old process survives the prober's bound, and its evidence names the unchecked scope; resume refused, then accepted after the assertion. Fail: a `running` thread after restart; a confirmation without the owner. | `kill -9 <daemon pid>`; restart; `ps -p <old pid>`; `chaldea stop --assume-dead`, then with `--launch`; resume with FakeCli. |
| PR10a-P-5 | If a resume is requested while the current launch's death is unconfirmed, the thread is closed, the expected launch is not current, or the worktree is not `active`/`released`, then no launch shall be reserved and the code shall name the reason; a `death_unconfirmed` refusal shall raise `resume_blocked`. | mvp §2.9; mvp CL-18 | each condition | Pass: 409 with the code; no `launch_reserved`. Fail: a reservation. | FakeCli (worktree rows written directly until PR 16). |
| PR10a-P-6 | When the program cannot be started, the thread shall show `exited` (`spawn_failed`), and a positional prompt shall be `rejected` (`spawn_failed`) when the program never ran, or `unconfirmed` when it may have. | mvp §2.3:186 | `PATH` without `codex` | Pass: as stated. Fail: input `sending` forever; a thread `running`. | `PATH=/usr/bin <absolute path to chaldea> daemon run`, then `chaldea spawn --agent codex --prompt hi`. |
| PR10a-P-7 | When a client reads `GET /api/adapters`, it shall receive this host's rows of the capability table. | mvp §2.7 | owner token | Pass: equal to `CAPABILITIES` for the host. Fail: other host's rows; missing agent. | `curl -H "Authorization: Bearer $T" http://127.0.0.1:$P/api/adapters`. |
| PR10a-P-8 | While a non-manual launch is unbound 20 s after `launch_started`, the thread shall carry `session_start_timeout` until it binds or exits. | mvp §2.2:142; hook-ingest:268 | a CLI that never sends `SessionStart` (every real CLI until PR 13/14) | Pass: the cause after 20 s, cleared at exit. Fail: absent; kept after exit. | Spawn Claude; read the thread after 21 s; stop it. |
| PR10a-P-9 | When the owner closes a thread whose launch exited with death confirmed (or with `--stop`), the thread shall be closed, its tokens revoked, and later resumes refused. | mvp §2.11 | idle thread | Pass: `closed: true`; resume → `thread_closed`. Fail: a resume succeeds. | `chaldea close <thread> --stop`; `chaldea resume <thread>`. |
| PR10a-P-10 | No text a CLI prints shall change thread state. | invariant 1; mvp CL-1 | FakeCli `banner` | Pass: no event beyond those from hooks and the process. Fail: any state change from output. | Not a real-CLI check (PR10a-CL-19). |
| PR10a-P-11 | When the owner stops the daemon gracefully, each session whose owned close returned within the stop budget shall be recorded `stopped` with its result (confirmed, or unconfirmed with its survivors), and each other one shall read `daemon_restart` at the next start. | SD10 | two running threads, one of them a tree that ignores HUP and TERM | Pass: each recorded per §2.9's classes; none left `running`. Fail: a session recorded `stopped` without its close; a thread left `running`. | `chaldea daemon stop` with a Claude thread running; restart; read threads. |

### 5.2 Plan Layer: Claims

Tests named `supervisor::…` are daemon unit tests under `crates/daemon/src/supervisor/tests/` with `FakePty`, the in-process store, the HTTP server, and `cfg(test)` pause points (the `crates/cli/tests/hook_e2e.rs` and lifecycle-test patterns); `supervisor_e2e::…` are in `crates/cli/tests/supervisor_e2e.rs` with the native runtime and FakeCli; `launch::…` are `core` unit tests.

| ID | Claim (EARS) | Source | Test that falsifies it | Mutation it kills |
|---|---|---|---|---|
| PR10a-CL-1 | When `spawn` or `resume` carries a prompt, `prepare` shall refuse a Claude prompt (`bad_args`), a text over `MAX_PROMPT_BYTES` (`prompt_too_large`), and any text `check(Codex, Positional, Send, …)` rejects (`prompt_rejected`) before any receipt or event; an accepted prompt shall be the argv's last element, after a `--`; a `cwd` that is missing or not a directory shall be a stored 400 `bad_args` with no reservation, token, event, or spawn. | C1; input-encoders D5; §2.2 | `supervisor::commands::prompt_admission` (each prompt case: status, code, no receipt row, the host and thread heads unchanged, `FakePty::spawns()` empty; accepted: `spawns()[0].argv` ends with `["--", text]`); `supervisor::commands::bad_cwd` (a file and a missing path: 400, a receipt with that reply, no `thread_registered`, no token row, `spawns()` empty; the directory then created and the same `command_id` replayed: the same 400); `launch::codex_argv` (prompts `--version`, `-c x`, text with spaces and LF) | `check` skipped or run after the reservation; the prompt split or placed before `--`; the size check after `check`; a bad `cwd` reserved |
| PR10a-CL-2 | The capability row and every OS-dependent argument shall come from `(agent, host_os)` with `host_os` mapped once from `std::env::consts::OS`; an unmapped OS shall stop `daemon run` before startup. | C7 | `adapter::host_os_from_str` (`macos`, `windows`, `linux`, empty); `daemon_cmd::tests::unmapped_os_refuses` (`daemon_cmd::run` with `Seams.host_os = "linux"`: exit `Failed`, no state directory files created); `supervisor::launch::windows_row` (a `Supervisor` built with `HostOs::Windows` on any runner: Codex argv has `-c tui.disable_paste_burst=true`) | `cfg!(windows)` instead of the mapped value; Macos hard-coded; unmapped OS accepted |
| PR10a-CL-3 | The reservation shall succeed only against the expected current launch, on a thread not closed, after the previous launch exited with death confirmed, and with its worktree `active` or `released`, carrying the previous launch's `worktree_id`; each refusal shall write nothing except the `resume_blocked` raise of `death_unconfirmed`. | A30, A5; mvp CL-18; mvp §2.9 | `supervisor::reserve::refusals_and_carry_over` (one condition at a time; worktree rows `creating`, `removing`, `uncertain`, `removed`, `failed` refused and `active`, `released` accepted, written directly; on success `launches_using_worktree(W)` returns the new launch); trace `trace_hole_a_reserve_without_death` replays unchanged | death check dropped (hole a); `creating` accepted; `released` refused; `worktree_id` not carried; `expected_launch_id` ignored; `resume_blocked` not raised or raised twice |
| PR10a-CL-4 | Of a `resume` and a `close` expecting the same launch, at most one shall succeed, and the other shall answer `thread_closed`, `launch_changed`, or `launch_live`. | A4 | `supervisor::race::resume_close` (both writer orders, both concurrent through a parked writer, and a resumed launch that exits and is confirmed before the close runs; asserts exactly one success and the final state) | close not checking `expected_launch_id`; resume not re-reading `closed` |
| PR10a-CL-5 | Two concurrent requests with one `command_id` to `spawn`, `resume`, `stop`, or `close` shall cause one effect. | A6; mvp CL-8 | `supervisor::commands::duplicates_write_once` (per command, two requests released together; counts the verb's own effect: `spawns()` and `launch_reserved` for spawn and resume, `StopRequested` and `FakeEvent::Kill` for stop, `thread_closed` for close) | the op started per request; a receipt bypass |
| PR10a-CL-6 | The reservation job shall insert exactly one hook token for the launch, and the spawn shall carry `CHALDEA_URL`, `CHALDEA_HOOK_URL`, `CHALDEA_HOOK_TOKEN` (that token), `CHALDEA_THREAD_ID`, none of `ENV_SCRUB` under the host's comparison, the user's other variables, and for Claude `--session-id <launch_id>` on an unbound thread or `--resume <session_id>` on a bound one, equal to `expected_session` for that launch. | D2; mvp §2.5:259; hook-ingest §2.8 | `supervisor::launch::env_and_session_args` (two generations: a fresh launch, then a resume after binding; daemon env seeded with every scrubbed name, `Chaldea_Hook_Token` and `claudecode` in mixed case, `CODEX_HOME`, `CLAUDE_CODE_USE_BEDROCK`; asserts `spawns()[i].env` and argv; each token's hash equals its row and differs between generations; `expected_session` equals the argv value); on Windows the mixed-case names are gone and the spawn succeeds | token per thread; scrub by prefix (user variables lost); case-sensitive scrub on Windows (duplicate key or leak); URL missing; thread id as session id; resume passing the launch id |
| PR10a-CL-7 | The start job shall record the backend identity and, in the same job, move the positional input to `sent` with `sent_after_seq` = `launch_started`'s seq; a hook that arrived before it shall be decided only after that commit. | D2; mvp §2.3:186 | `supervisor::launch::started_job` (one job's events in order; one commit notification); `supervisor_e2e::early_hook_waits_for_started` (FakeCli `hooks` emits `SessionStart` at once; a pause point holds the op before the start job; the test observes the hook inside the pid wait, then releases; the hook's reply follows the commit, with binding; the pid wait's bound comes from a clock seam, not a race against 1 s) | input moved in a later job; `sent_after_seq` of `launch_reserved`; identity from the request instead of `Session::identity` |
| PR10a-CL-8 | If the daemon crashes between `launch_reserved` and `launch_started`, then restart shall record `unknown_after_crash` with death unconfirmed and the positional input `unconfirmed` with `input_unconfirmed`; `stop` shall answer `death_unconfirmed`; only `owner_asserted` shall confirm; on Windows the suspended child left by the A29 window shall meet the finder's candidate conditions. | A2, A21, A29 | `supervisor::restart::crash_before_started` (store rows of a reservation, the reconcilers; `stop` → `death_unconfirmed`, no probe; `stop --assume-dead --launch L` → confirmed); `supervisor_e2e::crash_after_child_created` (a helper process runs `daemon run` with a pause after `Session::spawn` returned; the test kills it by PID, restarts, and finds `unknown_after_crash` while the real child was created); Windows: the helper sets `WindowsFault::RejectJobList(Some(gate))` inside the `spawn_blocking` closure (the fault is the calling thread's) through a `SupervisorConfig` test hook, and reports reaching the gate and the child's pid on its stdout; the test kills the helper at the gate and checks the child is suspended, in no Chaldea launch job, its image the agent's program, and created after the reservation's `recorded_at`; the test's cleanup ends it | `daemon_restart` for a launch without pid; input left `sending`; a probe or confirmation without an identity; a finder condition that requires "in no job" |
| PR10a-CL-9 | If `Session::spawn` returns `NeverStarted`, the launch shall exit `spawn_failed` with death confirmed and the positional input `rejected(spawn_failed)`; if `StartedThenFailed`, `spawn_failed` with death confirmed exactly when cleanup is `Confirmed`, the input `unconfirmed`, and the `spawn_failed` evidence when unconfirmed. | mvp §2.3:186; `backend.rs:185-201` | `supervisor::launch::spawn_errors` (`FakeScript { fail }` for each variant and each cleanup, `identity` present and `None`) | `StartedThenFailed` rejecting the input; `NeverStarted` leaving death unconfirmed; cleanup ignored; `identity: None` dropped instead of `no_identity` |
| PR10a-CL-10 | Every `Session::close` call shall receive the launch's full recorded list; death shall be confirmed only by a close whose join returned in time with `tree_dead`; a close that cannot prove death shall record its survivors as evidence; a dropped `Session` shall confirm nothing. | A3, A26, A27, B1 | `supervisor::stop::full_list_every_call` (a test seam records one proven identity on the `LiveLaunch`; every `FakeEvent::Kill { extra }` equals it, in stop, exit, and shutdown closes; `KillBehavior::Ignore` with short bounds from `SupervisorConfig`: `death_confirmed` false and survivors evidence); `supervisor::stop::late_join_unconfirmed` (the close's join made `Late` by a controlled clock: unconfirmed despite `tree_dead`, `close_unreported`); `supervisor::stop::drop_never_confirms` | survivors passed back; `extra` dropped on one path; a `Late` result counted; a drop path recording death |
| PR10a-CL-11 | When a running launch is stopped, the supervisor shall seal, record `StopRequested`, close explicitly, and record one `launch_exited` (`stopped`) with `code` and `signal` from `CloseOutcome.exit` (null when the waiter had not reported by the join), `death_confirmed` from the close, and `launch_death_confirmed` with the kill's method when confirmed. | mvp P-3; pty-unix §4 | `supervisor::stop::records_one_exit` (FakePty; the close parked so the exit op's event arrives first: one `launch_exited`, from the stop; `FakeChild::exit` before the join: its code and signal; not before: null); `supervisor_e2e::stop_kills_tree` (FakeCli `spawn-tree` with a sleeper grandchild living far beyond the test, and an unrelated sleeper; after the reply every tree pid is gone by `process::lookup` and the unrelated one lives; on macOS the leader's exit shows a signal, on Windows an exit code and no signal) | exit recorded before close; two `launch_exited`; method dropped; exit status from a `status()` read before close; unrelated process killed |
| PR10a-CL-12 | While a launch is `starting`, a stop shall wait for the spawn outcome on the op lock and then stop a started launch or answer `already_exited`; if the lock is not taken in time (`Elapsed` or `Late`), it shall answer `launch_busy` and never stop later. | X5; `state.rs:255-258` | `supervisor::stop::while_starting` (pause points between slot insertion and the reservation commit, and after the commit before `Session::spawn`: a stop in the first window sees the previous launch, in the second waits; release → stopped; spawn failure → `already_exited`; a controlled clock makes the lock `Elapsed` and `Late` → 504, and after release no `StopRequested`) | `StopRequested` fed while `starting`; a missing slot in the commit window; a `Late` guard used; a 504 followed by a later stop |
| PR10a-CL-13 | When the child exits by itself, the supervisor shall record one `launch_exited` (`exit`) after an explicit close, with the callback's `ChildExit`; after `WaitFailed`, `code` and `signal` shall be null and death shall be the close's result. | mvp §2.9; A25 | `supervisor::exit::natural_exit` (`FakeChild::exit` with code and signal); `supervisor::exit::wait_failed` (`FakeChild::fail_wait`: null status; `KillBehavior::ExitOnKill` → confirmed, `Ignore` → not) | `WaitFailed` treated as proof or as refutation; exit recorded without close; code dropped |
| PR10a-CL-14 | The exit job shall move the thread's inputs still in `queued`, `sending`, or `sent` of the launch or of no launch after the registered exit steps, and the restart job those of every thread, keeping `unconfirmed` inputs and their attention; a second restart shall change nothing. | mvp CL-6; A13 | `supervisor::exit::inputs_recovered` and `supervisor::restart::inputs_recovered` (inputs written directly in each state, of the launch, of no launch, and an earlier launch's `unconfirmed`; a test `ExitStep` that settles one input first: the sweep leaves it; then a second restart: no event) | inputs moved in a later job; `unconfirmed` stepped (job fails); the unassigned queue left; the sweep before the registered steps; restart sweeping only open launches' threads |
| PR10a-CL-15 | `buffer::resolve_unbindable` shall run in every exit job and at restart for every `buffered_launches()`; `buffer::expire` shall run on the tick for live unbound launches. | A22; hook-ingest §2.9 | `supervisor::exit::buffer_resolved`; `supervisor::restart::buffers_resolved` (a buffer left by a launch exited in an earlier run); `supervisor::tick::expires_buffer` (the tick's instant and the stored stamps both controlled) | resolve in a later job; restart skipping exited launches; tick not calling `expire` |
| PR10a-CL-16 | The restart reconcilers shall run in the order receipts, supervisor launches, supervisor buffers, before the listener exists. | E3; daemon-lifecycle §2.14 | `supervisor::restart::reconciler_order` (names); `supervisor_e2e::restart_records_before_serving` (the buffers job parked: the port is not served until it is released; the prober held, so the first read shows the launch exited, unconfirmed, with no evidence) | order swapped; reconciliation in a task after serving |
| PR10a-CL-17 | `kill_recorded` shall signal only pids whose read showed the recorded start, never a group or session and never an invalid identity, check its deadline before every OS call, and report `all_gone` only when every given identity is gone or replaced; after a restart the prober, and a `stop` of a `daemon_restart` launch, shall record the fates and unchecked scope as `probe` evidence and leave death unconfirmed. | A1, F1, A28; owner decision | `unix::orphan::tests` (scripted reads: live leader → HUP, TERM, KILL to that pid only → gone → `all_gone`; another start → resolved without a signal; unreadable → `survived`; a refused full record followed by the short one; pid 0, 1, own pid, bad start → `invalid`, no call; empty → `all_gone` with no call; no `pid_list` or `getsid` call ever; a deadline passed between two calls: no further call); `windows::orphan::tests` (test-owned processes: all terminated before any wait, then resolved; a wrong start: not signalled, replaced); `supervisor_e2e::restart_probe_scope` (`daemon run` killed by PID while FakeCli runs with a `sleeper` child of role `ignore-term` in its session; after restart, with no command sent: on macOS the leader is gone by the prober's bound, the child is still alive afterwards and is ended by the test's cleanup, the evidence names `unrecorded_session_members`; on Windows the job closed with the daemon and the evidence names `job_members`; death unconfirmed in both; a later `stop` appends a second evidence event under its own key); `supervisor::restart::prober_bounded` (six `daemon_restart` launches with scripted identities: at most 4 probes at once; a launch whose probe lock a parked `stop` holds is skipped; a failed evidence record is not retried) | confirmation from a probe; any group signal; `Unreadable` treated as gone; job closing treated as proof; a call after the deadline; sequential terminate-and-wait on Windows |
| PR10a-CL-18 | `session_start_timeout` shall be raised only for the current, live, unbound, non-manual launch at least 20 s after its `launch_started`, also after a `foreign_session` `SessionStart`; `startup_stall` only for an upgraded Codex launch at its start; neither for an exited or previous launch. | D4, A32; mvp §2.2 | `supervisor::tick::session_start_timeout` (`Supervisor::tick` at +19.999 s: none; +20 s: raised; bound: none; foreign `SessionStart` row: raised; exited: none; manual Codex: none; and the tick's candidates taken, then the launch bound, exited, or replaced before its job: none); `supervisor::launch::startup_stall_only_upgraded_codex` | guard on "`SessionStart` seen"; no re-validation in the job; manual Codex raised; Claude stall |
| PR10a-CL-19 | No byte the CLI writes to its PTY shall produce an event or a state change. | A24; mvp CL-1 | `supervisor_e2e::banner_changes_nothing` (FakeCli `banner`; the test first sees its text in a VT snapshot, so the drain read it; events, with ids and stamps removed, equal those of a run with silent output) | any reader of PTY output feeding a reducer input |
| PR10a-CL-20 | Every child the supervisor creates shall pass admission and be created through the backend or `spawn_locked_before`, waited for by exact pid, and its wait bounded; a git child shall be created only before its deadline and never killed. | A25; daemon-lifecycle §2.11 | `cargo clippy` with the existing `Command::spawn` bans (`crates/daemon/clippy.toml`; `lints.rs::clippy_tomls_list_the_bans`); `supervisor::workspace::probe_bounded` (the test holds the spawn lock past the deadline: no git process, canonical cwd; a `SupervisorConfig` git that sleeps beyond 5 s: canonical cwd at 5 s, the child alive and in the leftover set, then reaped by a tick after it exits, `process::lookup` → gone) | a direct `Command::spawn`; an unbounded wait for the spawn lock; a kill of git; a leftover never reaped |
| PR10a-CL-21 | Every committed job that revokes a token or ends a launch's agent-token validity shall nudge sockets through `after_ok` once it committed: close's intent, close `--stop`'s closing job, and the exit job. | E4, X1; http-surface §2.4 | `http::tests::commands::revoking_intent_nudges` (a test handler with `revokes()` true: one nudge after the commit is visible to a read, none before; false: none); `supervisor::close::nudges` (a nudge counter: one per committed revoking job, none for a rolled-back one) | runner ignoring `revokes()`; nudge before commit; exit job without nudge |
| PR10a-CL-22 | The four handlers shall be registered, every new code classified in `handler_code` at one status, and no success type shall have a top-level `code`. | E1, E2 | `handler_code` exhaustive `match` (compile); `supervisor::commands::codes_and_statuses` (each refusal's status); `supervisor::commands::success_schemas_have_no_code` (each success type's JSON schema has no `code` property) | a code at two statuses; a success body with `code`; a handler left 501 |
| PR10a-CL-23 | A command op that cannot record a job shall resolve its effect to `None`, and the runner shall reply 503 `outcome_unknown` at once, leave the receipt `in_progress` (`unknown_after_crash` after the restart), and report `ReceiptUncompleted`; an exit op that cannot record within 60 s shall report `LaunchUnrecorded`, unless shutdown started. | E2, E5 | `http::tests::commands::effect_none_is_outcome_unknown` (a test effect resolving `None`: 503 before the command deadline, no completion attempt, the sink receives it); `supervisor::launch::unrecorded_start_job` (a store fault fails the start job past its budget: the reply is 503 at once, the receipt `in_progress`, the reconcilers make it `unknown_after_crash`); `supervisor::exit::unrecorded_exit_job` (the same for the exit op: `LaunchUnrecorded`; with admission closed: no report) | a pending effect; a completed receipt; a stored 500; a fatal report during shutdown |
| PR10a-CL-24 | A retry of a `spawn` whose daemon died during its op shall answer 409 `unknown_after_crash`, and the op shall not run again. | E6; daemon-lifecycle PR8-P-3b | `supervisor_e2e::retry_after_crash_is_unknown` (a helper process runs `daemon run` with `Seams.components` holding a pause after the reservation commit; the test kills it by PID, restarts, retries with the same `command_id`; one `launch_reserved`) | receipts reconciler skipped; op re-run on retry |
| PR10a-CL-25 | `GET /api/adapters` shall return exactly this host's `CAPABILITIES` rows in `AdaptersResponse`, behind the route's guard. | C6 | `supervisor::adapters::equals_table` (each `HostOs`, compared with `CAPABILITIES` filtered by host); `supervisor::adapters::guarded` (wrong `Host` → 421, no token → 401, cross `Origin` → 403, a query string as the other read routes answer it); contract drift test. That the handler maps `CAPABILITIES` rather than a list of its own is a review rule. | other OS's rows; `submit_delay` exposed; guard bypassed |
| PR10a-CL-26 | `close` shall commit `thread_closed` and revoke the thread's tokens only when the expected current launch exited with death confirmed; `--stop` shall close only the launch it stopped and never probe. | mvp §2.11 | `supervisor::close::cases` (live → `launch_live`; unconfirmed → `death_unconfirmed`; a `daemon_restart` launch with `--stop` → `death_unconfirmed`, no evidence; a stale `expected_launch_id` with and without `--stop` → `launch_changed` before any op; a resume after the stop → `launch_changed`; on success the thread's hook token fails `verify_hash_in`) | close with a live launch; tokens kept; closing a newer launch; `--stop` probing |
| PR10a-CL-27 | `stop --assume-dead` shall be accepted only from an owner token, only with the current launch's id, only for a launch that exited, and shall record `owner_asserted` once; without `--launch` the CLI shall send nothing and print the launch's state and evidence. | SD2; §2.4 | `supervisor::stop::assume_dead` (an agent token whose plain `stop` of the same thread succeeds → 403 with `assume_dead`; `assume_dead` without `expected_launch_id` → 400 in `prepare`; the owner with a stale `--launch` → `launch_changed`; running → `launch_live`; exited unconfirmed → confirmed; repeated → `already_exited`, no second event); `agent_cmd::tests::assume_dead_preview` (a stub server: one snapshot read and backward pages to `launch_reserved`, no POST; the printed evidence equals the stream's) | non-owner allowed; unbound to the launch; allowed on a running launch; a second confirmation; a preview that sends |
| PR10a-CL-28 | `chaldea wait` shall return the thread when the condition holds, exit 4 no later than its cutoff, pass that cutoff to every request and discovery retry, and exit 1 on 404, 401, 403, other statuses, and undecodable bodies. | SD16 | `supervisor_e2e::wait_until_exited` (FakeCli exits; `wait` returns its JSON; an already-exited thread returns at once); `agent_cmd::tests::wait_cutoff` (a stub server answering after the cutoff and a clock seam: exit 4 at the cutoff, not after the slow response; discovery `Starting` and `Stopping`, 408, 503, EOF → keeps polling; 401, 404, 500, a broken body → exit 1) | timeout ignored; exit 3; a request outliving the cutoff; an already-true state missed; a 500 retried |
| PR10a-CL-29 | A guard 10a changes shall change `specs/thread_input.qnt` and its traces in the same PR. | A38; formal-specs §6 | CI `specs` job (`traces:check`) and `quint_traces.rs` replay for the model; §7's guard-to-test table, reviewed in the PR, for the daemon guards the core replay cannot see | a model guard changed without its trace; a daemon guard with no test in §7's table |
| PR10a-CL-30 | When the git probe succeeds, a thread's `workspace_id` shall be derived from git's common directory, so a repository and its worktrees share it; otherwise from the canonical cwd, with the reason logged. | mvp §2.1:103 | `supervisor::workspace::ids` (a temp repository, a `git worktree add` of it, the repository reached through a symlinked path, and a plain directory) | cwd used inside git; per-worktree ids; the alias splitting the id |
| PR10a-CL-31 | After a stop sealed the session, no unit shall be permitted: a unit not yet permitted at the seal shall be settled `Closed` with zero bytes before the `StopRequested` commit, and an exit job of the launch shall find that completion settled. | SD18 | `supervisor::stop::seal_before_stop_requested` (a unit submitted through `with_session`, the writer parked at `ProbePoint::RecheckEntered`; the `StopRequested` job held by a store writer pause; while it is held the unit's completion is `NotStarted(Closed)` and `FakeChild::writes()` empty; then a test `ExitStep` reads it without waiting in the exit job) | `StopRequested` committed before the seal (with the commit held, the released writer writes); a completion run after `seal` returns |
| PR10a-CL-32 | `Supervisor::observe` shall answer without waiting on `ops` and shall tell `Reserved`, `Started` with `reference` and the per-class positions, and `Unobservable` apart. | A35 (anchor part); 10b coupling | `supervisor::live::observe` (synchronous calls, with `ops` held by the spawn op throughout: before publication `Reserved`; between publication and the start job's commit `Started` with `reference`; after a dispatched `Input` unit its class position above `reference`; after the slot is removed `Unobservable`; a launch id of another thread through `live`: `None`) | a read under `ops`; `reference` taken after the start commit; publication after the start job; a thread-blind lookup |
| PR10a-CL-33 | Of two owner assertions for one launch, exactly one shall write; probes of one launch, by the prober or a `stop`, shall run one at a time and never confirm. | §2.4 "One confirmation per launch" | `supervisor::stop::one_confirmation` (two assertions with different `command_id`s released together: one `launch_death_confirmed`, the other `already_exited`; two `stop`s of a `daemon_restart` launch released together: the second probe starts after the first's evidence commit, two evidence events with different keys, no confirmation; a `stop` arriving while the prober probes the launch waits and then probes) | no re-read in the assertion job; two probes at once; a probe confirming |
| PR10a-CL-34 | A hook token shall stay valid after its launch exits in the same daemon run and be refused after the restart or the thread's close; the owner token shall stay valid across the restart. | D2; hook-ingest §2.8 | `supervisor_e2e::late_hooks` (one launch's token sends a hook after exit: stored `raw_unknown`; a second, independent launch's token after a restart and a third's after a close: 401 and the thread head unchanged; in a control run without the revocation the same requests are accepted; the owner token reads after the restart) | restart revoking only open launches; exit revoking the token; `LaunchScoped` revoking the owner token |
| PR10a-CL-35 | An exit that an owned close could not confirm, and an unresolved `StartedThenFailed`, shall record evidence naming what was not checked; an exit an owned close confirmed shall record none. | §2.4 Evidence; SD2 | `supervisor::evidence::recorded` (`StartedThenFailed` with and without identity; close with survivors; `Late` join; confirmed close: no evidence; the keys of each, and a retried record job writing one event) | evidence missing on one path; evidence after an owned confirmation; a key colliding on a second probe |
| PR10a-CL-36 | A launch operation shall run to its end when its runner is aborted, and own its `Session` and results. | SD17; E2 | `supervisor::ops::outlive_runner` (the runner aborted after the reservation commit: the op records `launch_started`, and a stop later finds the session) | the op running inside the runner's future |
| PR10a-CL-37 | When the daemon stops gracefully, each live session shall end in one of §2.9's classes, all decided against `end`. | SD10 | `supervisor::shutdown::owned_close` (four FakePty sessions and a controlled clock: `ExitOnKill` → `stopped` confirmed; `Ignore` → `stopped` unconfirmed with survivors; close held by `hang_control_close` past `close_end` → no record, `daemon_restart` after the restart; the store writer paused past `end` → the participant returns by `end`; one session already `stopping` → its job has no `StopRequested` and commits) | sessions left to process exit; a join waited past `close_end`; `StopRequested` fed to a `stopping` launch; a result-not-received session recorded |
| PR10a-CL-38 | When an agent token spawns, the thread shall be registered as a child of the token's thread with role `standard`; an owner token's spawn shall have no parent. | §2.2 Caller; `auth.rs:144` | `supervisor::commands::spawn_caller` (an agent token row built with `TokenService::new_token`, since PR 16 mints them: `thread_registered.parent_thread_id` is its thread; an owner spawn: none) | parent dropped; the owner's or the request's thread used as parent; role taken from the request |
| PR10a-CL-39 | A `resume`, `stop`, or `close` for a thread outside the token's grant shall be refused by `deny_for` before any read of its launch, with nothing written and no op. | §2.2 Target scope | `supervisor::commands::target_scope` (an agent token whose `stop` of its own thread succeeds; against an unrelated thread: `stop`, `close`, `close --stop`, and `resume` each 404, no receipt beyond the refusal, no `FakeEvent::Kill`, the thread head unchanged) | the scope check after the generation read; a 409 that reveals the launch; a kill of an out-of-scope thread |
| PR10a-CL-40 | Once the participant closed admission, the supervisor shall create no process outside its snapshot; a process creation still running at `end` shall make the coordinator keep the instance lock. | SD10; A25 | `supervisor::shutdown::admission` (an op paused before `Session::spawn`; the HTTP drain aborts its runner; released before the participant's snapshot: it spawns, and the participant closes and records it; released after: refused, `spawns()` empty, the `NeverStarted` exit recorded); `lifecycle` coordinator test (a participant returning `LeftRunning`: exit with `LockDisposition::HeldUntilExit`) | a creation after the snapshot; an op released after the drain never closed; the lock released with a spawn in flight |
| PR10a-CL-41 | `chaldea resume` and `chaldea close` shall read the current launch once, send it as `expected_launch_id`, keep it on every retry of that request, and exit 1 on `launch_changed` without sending again. | §2.2; SD16 | `agent_cmd::tests::expected_launch_pinned` (a stub server: one snapshot read, POSTs with one `command_id` and one id across a transport retry; `launch_changed` → exit 1, no further POST) | the id re-read on retry; a new request after `launch_changed` |

### 5.3 Accepted Limits

| ID | Limit | Decided by | Why it does not defeat the purpose |
|---|---|---|---|
| L-1 | macOS, after a restart: members of the old session that are not recorded identities and survived the PTY hangup are neither signalled nor checked. | pty-unix §2.5 (no group or session target without ownership) | the evidence names the unchecked scope, and nothing unlocks until the owner asserts; a live-daemon `stop` and a graceful daemon stop keep the full owned kill (P-3, SD10). |
| L-2 | Windows, after a restart: job members are terminated by `KILL_ON_JOB_CLOSE` but checked only through the recorded identities. | pty-windows §4 "Daemon restart" | the same as L-1; the job's guarantee (no member escapes, pty-windows §2.5) limits survivors to processes never in the job. |
| L-3 | A launch without a recorded identity (A2, A29, or an admission refusal whose record did not commit) is never confirmed automatically; an A29 child stays suspended until the owner or PR 21's finder ends it. | SD2 | it blocks reservation, so no second CLI runs on the session; the owner can release it; the suspended child never runs. |
| L-4 | A `StartedThenFailed` launch with unresolved cleanup is released only by the owner. | `backend.rs:194-200`; §2.4 | its identity and survivors are recorded as evidence; rare (thread creation or start read failed). |
| L-5 | A Codex positional prompt longer than Windows' command-line limit fails as `spawn_failed`. | `CreateProcessW` `lpCommandLine` (32,767 characters) | the input ends `rejected` with zero bytes written, visible to the owner; PR 14 can refuse earlier. |
| L-6 | Until PR 13 and PR 14, a real CLI launched by 10a never binds: `session_start_timeout` appears and `resume` answers `no_session`. | SD14 | spawn, stop, close, restart, and their proofs work; binding and resume are tested with FakeCli `hooks`; real resume is PR 13/14's gate. |
| L-7 | A launch whose owned close could not prove death is released only by the owner. | §2.4 | its survivors are recorded; `stop` does not probe it, so no narrower check stands in for what the owned close saw. |
| L-8 | A signal of `kill_recorded` can reach a reused pid: on macOS if the pid space wraps between its read and the signal; on Windows only for two processes with the same pid and 100-ns creation stamp after a clock step back. | pty-unix §2.5 "Never a reused pid"; pty-windows §2.5 "Extras" | the same residual cases PR 3 and PR 4 accepted. |
| L-9 | After `chaldea daemon stop`, a session whose close result was not received by `close_end` reads `daemon_restart` on the next start. | SD10 | its history is intact; the owner's `stop` probes it. |
| L-10 | When git is missing, cannot be created before its deadline, or is slower than 5 s, the canonical cwd is used, so worktrees of one repository can get different workspace ids; a slow git keeps running until it exits. | §2.3 step 1 | the reason is logged; the id groups threads and scopes PR 20's Workspace surfaces, and a split id narrows such a scope, never widens it; the tick reaps the child. |
| L-11 | A recorded old process that the restart probe could not end (it survived KILL, or its reads stayed unreadable) is reported as `survived` and is not probed again until a `stop`. | SD19 | the evidence names it for the owner, whose assertion is the only unlock; P-18's survivors are those reported. |
| L-12 | A thread snapshot larger than the CLI's body bound (`REPLAY_MAX_BYTES`) makes `wait`, the `--assume-dead` preview, and the pinned reads exit 1. | `client::MAX_BODY` | each recent event's large fields are inline only up to `INLINE_LIMIT_BYTES` (64 KiB), else blobs; the PWA reads the same thread. |

## 6. Tests

**Per claim**: §5.2's test column. Unit tests drive `FakePty` (`FakeScript`, `FakeChild::exit`, `FakeChild::fail_wait`, `FakeEvent::Kill`, `FakePty::spawns`) and the store directly, with `cfg(test)` pause points of the supervisor; integration tests start the store and `HttpServer` in-process with the native runtime and FakeCli, as `hook_e2e.rs` does; the restart and E6 tests run the daemon in a helper process (the test binary re-executes itself, daemon-lifecycle §7 "Helper-process tests") with `Seams.components` holding a pause, and kill it by PID. No release-binary variable or flag enables a pause.

**Locating FakeCli.** `supervisor_e2e.rs` lives in `crates/cli/tests` because its hooks need the `chaldea` emitter (`CARGO_BIN_EXE_chaldea`), and it finds `chaldea-fake-cli` as `hook_e2e.rs` does (`fake_cli()`, built by `cargo test --workspace`). pty-unix §2.8 calls that lookup a fallback; the alternative, a thin `[[bin]]` in the `cli` package, would ship a test binary with the CLI.

**Ports.** Native tests use the port the daemon published (`client::discover`), never a probed free port.

**FakeCli `banner` mode** (new, `crates/testkit/src/fake_cli/banner.rs`): prints a fixed corpus of text shaped like Claude and Codex trust prompts, `Hooks need review`, a JSON hook payload, and a prompt echo, then sleeps until `--max-life-secs`.

**Host speed.** Timers run through `Supervisor::tick(now)` and `SupervisorConfig.clock`; lock waits and joins are made `Elapsed` or `Late` by that clock; the windows between slot, reservation, spawn, publication, and start are held by pause points, and the restart prober is held where a test reads before it; a close is held by `FakeScript.hang_control_close` and a store commit by a writer pause, each on its own; kill and close bounds are shortened through `SupervisorConfig` only where a test needs `KillBehavior::Ignore`; real-process tests decide pass or fail by `process::lookup` after the command's reply, never by elapsed time.

**Windows CI.** Every `supervisor::` unit test, `launch::`, `adapter::`, `windows::orphan::tests`, the Windows A29 helper test, and `supervisor_e2e::` run on `windows-latest` in the Cargo lane of `scripts/windows-tests.ps1` (nested jobs need no breakaway, pty-windows PR4-CL-27; `daemon run` is not a detached start). `unix::orphan::tests` and the macOS half of `restart_probe_scope` are `cfg(unix)`.

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
3. **Hole a**: no guard change. `reserve` already requires `st.deathConfirmed` (`thread_input.qnt:791-793`); 10a makes the reservation job enforce it (PR10a-CL-3). `trace_hole_a_reserve_without_death` stays as the statement of what `reduce` alone admits, and `reservedOnlyAfterDeath` is the checked invariant.
4. **Hole c**: no model change. `sessionTimer` and `stall` already require a live, current launch; 10a's job-side guards implement them (PR10a-CL-18). `trace_hole_c_stale_startup_cause` stays.
5. **Unchanged and mapped**: `stop` requires `Running` (X5's hold is outside the model: the stop is applied after the outcome); `dispatch` requires the recheck's gate to be open, which a `stopping` launch closes: 10a's seal implements that order (SD18); `restart` uses `UnknownAfterCrash` exactly for a `Starting` launch, which in Rust is "no pid recorded", and recovers inputs other than `unconfirmed`; `confirmDeath` is the owner assertion (the restart probe is outside the model: it changes no state).

**Guard-to-test table** (the daemon guards the core replay cannot see; reviewed with PR10a-CL-29):

| Model guard | Daemon site | Test |
|---|---|---|
| `reserve`: exited, `deathConfirmed` | reservation job (§2.3 step 3) | PR10a-CL-3 |
| `reserveRefused`: raise once | reservation job | PR10a-CL-3 |
| `sessionTimer`, `stall`: live, current, unbound | tick and start jobs (§2.6) | PR10a-CL-18 |
| `stop`: `Running` | stop op step 2; participant job (§2.9 step 4) | PR10a-CL-12, PR10a-CL-37 |
| `restart`: reason by start state; inputs but `unconfirmed` | `supervisor_launches` | PR10a-CL-8, PR10a-CL-14 |
| `confirmDeath`: exited, unconfirmed | owner assertion | PR10a-CL-27, PR10a-CL-33 |

formal-specs §3's owner column for holes a and c becomes "PR 10a", and §6's PR 10 bullet points here (§8).

## 8. Proposed Canonical-Doc Changes

Not applied here.

- **mvp §2.1, event table**: the kind `launch_evidence` (§2.4), with its idempotency keys in the "Idempotency keys" table; `launch_death_confirmed` also follows a `launch_exited { death_confirmed: true }` in the same job, to record its method.
- **mvp §2.2, attention table**: `session_start_timeout` "Raised on: no `session_bound` 20 s after `launch_started` of a non-manual launch (a `SessionStart` that does not bind, such as `foreign_session`, does not stop it)".
- **mvp §2.5:259 / §2.7 "Env scrub"**: the named list of §2.7 here instead of the `CLAUDE_CODE_*` and `CODEX_*` prefixes, compared as the host compares environment keys, with the reason of SD13.
- **mvp §2.9 "Daemon restart"**: tokens revoked are every launch's, not only the open ones'; "`launch_death_confirmed { method }` follows once the old tree is proven dead" → the owner decision of 2026-11-25: a crashed launch's death is confirmed only by the owner (`owner_asserted`), after the restart probe (or a `stop`) has ended the recorded identities and recorded its evidence; the probe runs after the listener exists and unlocks nothing.
- **mvp §2.4 / §2.5**: the command args, success types, and codes of §2.2; the target-scope refusal before any read; `stop --assume-dead --launch` (owner only) and its preview; `chaldea wait` (SD16, exit 4 on timeout).
- **mvp §4, PR 9 / 10 bullet**: "PR 10" (hook-ingest D10 gives it to PR 10 entirely).
- **mvp §4, PR 10 bullet**: replace "restart scan of a launch's session by recorded sid with `launch.start_time ≤ start ≤ restart_now`" with the probe of §2.4; state that PR 10 is designed as 10a (this document) and 10b, and that real same-session resume is PR 13's and PR 14's acceptance.
- **mvp §6.1 P-3**: the 5 s is the close's kill bound, counted from the close's start; a stop that first waits for a spawn in progress (up to 10 s) starts it later.
- **mvp §6.1 P-18**: the restart probe ends the recorded old processes, and survivors are reported in its evidence; a resume after a daemon crash follows the owner's assertion, made after reading that evidence; on macOS the evidence leaves unchecked the members of the old session that survived the hangup without a recorded identity (L-1).
- **http-surface §2.4 "Revocation nudges"**: command intents that revoke set `CommandHandler::revokes`; the runner passes the nudge; components get it as `RevocationNudge` at `attach`. **§2.8**: the `revokes` method in the trait listing; `CommandEffect::run` resolves to `Option<CommandResult>`, `None` taking the runner's "ended without a result" path. **"Handler replies"**: the nine codes of §2.2. **§2.7**: `GET /api/adapters` → `AdaptersResponse` (SD5). **§4 PR 10 row**: split 10a / 10b; `write_with`, `mint_token`, and the completion-policy constants as the seams for components, re-exported from `http`.
- **hook-ingest §2.8**: restart revokes every launch token. **§4 PR 10 row**: `extra` is empty in the MVP (§2.4); the restart buffer pass is PR 10's second reconciler; `IntakeProbe` reads `Supervisor::observe`.
- **daemon-lifecycle §2.14**: reconcilers "receipts → supervisor launches → supervisor buffers", the last implemented by PR 10 (PR 9 shipped none); `ShutdownParticipant::shutdown` resolves to `ParticipantEnd`, and the lock rule (§2.8 step 6) adds "a participant left work running"; participant row: "admission closed, owned close of live sessions against one cutoff, the rest left to restart (SD10)"; `spawn_locked_before` beside `spawn_locked`; `chaldea daemon run`'s default components are `SupervisorComponents`; the CLI client's body bound is `REPLAY_MAX_BYTES`.
- **deadlines.md**: production exceptions gain `supervisor::tick_timer` (14); `commands::backoff_until` is shared with the supervisor; the `Session::spawn` wait without a deadline and its reason (§2.10).
- **pty-runtime.md §2.2 and §4**: `Session::seal` (its completions run before it returns), `Session::observer` and `WriterObserver`, `CloseOutcome.exit`, `orphan::kill_recorded`, `env_key_eq`, `env_key_has_prefix`, and `with_spawn_lock_before` as PR 10a additions. **pty-unix.md §4, pty-windows.md §4**: the "Daemon restart" rows point to `orphan::kill_recorded` (a verified-identity loop, no engine rounds); pty-unix's "Spike watchdog and emergency kill" row: none, `stop` (SD15). **pty-windows.md §2.2 "Crash window"**: what PR 21's finder receives and its candidate conditions (§2.4 "A29 on Windows").
- **formal-specs §1** (`sessionTimer` row: "unbound" instead of "no `SessionStart`"), **§3** (owner "PR 10a" for holes a and c), **§6** (PR 10's bullet split into 10a and 10b; the guard-to-test table of §7 here).
- **AGENTS.md**: none.
