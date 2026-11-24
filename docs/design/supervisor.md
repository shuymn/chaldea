# Design: Supervisor, Launch and Process Side (MVP PR 10a1)

Status: design. Scope is the launch and process part of the PR 10 row of `docs/mvp-design.md` §4 (cited as "mvp §N") that runs while the daemon owns its sessions, called **PR 10a1**: reservation, the `spawn`, `resume`, `stop`, and `close` commands and the `chaldea wait` CLI, the live process lifecycle and its death proof, natural exit, the hook token and binding facts of each launch, `extra` attribution, the environment, `GET /api/adapters`, the host-OS mapping, the startup attention causes, the shutdown participant, and the restart reconcilers. The send and ack half (`send`, `ready`, `key`, `input_resolve`, `input_cancel`, the gate, the writer, ack attribution, the real `IntakeProbe`, input-side attention and cards) is **PR 10b**, designed against the interfaces of §2.11. Crash recovery (the restart probe of old processes, launch evidence, the owner's assertion, and mvp P-18) is **PR 10a2**, after 10b and before PR 15 (§2.12).

**What 10a1 decides and leaves.** 10a1 owns the supervisor module of the daemon, the pure launch builder in `core`, small additions to `pty-runtime` (§1), and the CLI verbs above. It leaves to 10b everything that writes to a live PTY after `launch_started` and every ack decision; to 10a2 every rule that confirms the death of a launch the daemon could not prove dead itself; to PR 13 and PR 14 the hook configuration each CLI gets (which events, Claude's settings file, Codex's `-c hooks.*`), the payload decoders, and the verification of the argv rows on real binaries; to PR 16 agent tokens and worktree creation and removal; to PR 11 attach; to PR 18–19 the UI; to PR 21 the doctor's orphan finder.

**Owner decision (2026-11-25).** After a daemon crash, a check that covers only a launch's recorded identities does not unlock resume, close, or worktree release. The launch's death is confirmed only by the owner, with `chaldea stop <thread> --assume-dead --launch <id>` after reading the launch's evidence. PR 10a2 implements it (§2.12); 10a1 confirms nothing it did not prove.

**Commitments from earlier PRs this design keeps.**

1. Death proof is pty-runtime's. `death_confirmed` becomes true only from `CloseOutcome.kill.tree_dead` of an explicit `Session::close` with the launch's full list whose join returned in time, or from a spawn error whose process provably never ran or whose cleanup is `Cleanup::Confirmed`. No census, no signal to a session or group the daemon does not own, no proof from a `Session` drop.
2. Every wait has a deadline through the daemon's helpers (`docs/design/deadlines.md`; `host::{act_before, observed_before}` on the synchronous side), with one stated exception (§2.10).
3. Thread state moves only through explicit events: hooks, the backend's exit report, and the supervisor's own commands (invariant 1). Nothing reads PTY text.
4. Public types change only where §1 lists a new name. `StateInput`, `InputInput`, `reduce`, `step`, and `gate` keep their rules.

Sources read for this document:

| Source | Revision | Read for |
|---|---|---|
| This repository | `cdf9524` | `AGENTS.md`; `docs/architecture.md`; `docs/mvp-design.md` §2.1–§2.11, §4, §6, §8; `docs/prior-art.md` rows 1a, 1b, 1d, 6c; `docs/design/{pty-runtime,pty-unix,pty-windows,daemon-lifecycle,http-surface,hook-ingest,event-store,formal-specs,deadlines,input-encoders}.md`; `crates/core/src/{state,adapter,input,payload,sync,ids,auth,http,command,event}.rs`, `crates/core/src/input/encode.rs`; `crates/pty-runtime/src/{lib,session,backend,process,spawn,fake,limits,origin,writer}.rs`, `host/mod.rs`, `unix/{mod,kill,child,table}.rs`, `windows/{mod,child,cmdline,hooks,sys}.rs`; `crates/daemon/src/http/{commands,ctx,error,routes,auth,mod}.rs`, `lifecycle/{mod,gate,startup,shutdown,files}.rs`, `hook/{mod,adapter,buffer,intake,route}.rs`, `store/{write_tx,readers,rows,project}.rs`, `deadline.rs`, `clippy.toml`; `crates/cli/src/{args,client,daemon_cmd,lib}.rs`, `crates/cli/tests/hook_e2e.rs`; `crates/testkit/src/fake_cli/{mod,hooks,sleeper}.rs`; `specs/thread_input.qnt`, `specs/thread_input_traces.qnt`; `scripts/windows-tests.ps1` |
| Win32 `CreateProcessW` | Microsoft Learn, read 2026-11-25 | the `lpCommandLine` limit of 32,767 characters (L-3) |
| Claude Code settings | `code.claude.com/docs/en/settings`, read 2026-11-25 | environment variables under the `CLAUDE_CODE_` prefix that select provider and behaviour (SD13) |

## 1. Goal and Dependencies

**Goal.** A CLI started by `chaldea spawn` or `chaldea resume` runs in a daemon-owned PTY with its own hook token and binding facts; each step of its life reaches the store as one explicit event in one job; every launch ends with `launch_exited`, its death confirmed exactly when an owned close or a spawn error proves it; a restart records every open launch, recovers every input, and revokes every launch token before the listener exists; and no thread is relaunched until its previous launch's death is confirmed.

**Depends on** PR 3 and PR 4 (backends, `Session`), PR 5 (`capabilities`, `check`), PR 8 (`Components`, reconcilers, participants, `spawn_locked`), PR 9 (`POST /hook`, binding, `buffer`).

**Files and seams** (every name marked new does not exist at `cdf9524`):

```
crates/core/src/adapter.rs        # HostOs gains serde/TS/JsonSchema and from_os_str (new)
crates/core/src/launch.rs         # new: LaunchInput, LaunchPlan, plan(), ENV_SCRUB, workspace_id()
crates/core/src/command.rs        # adds the wire types of §2.2 (SpawnArgs … CloseResult)
crates/core/src/sync.rs           # adds AdaptersResponse, AdapterInfo, ThreadResponse
crates/core/src/http.rs           # adds the ErrorCode variants of §2.2
crates/pty-runtime/src/session.rs # adds Session::seal, Session::observer, WriterObserver, CloseOutcome.exit
crates/pty-runtime/src/env.rs     # new: env_key_eq, env_key_has_prefix (the backend's key comparison)
crates/pty-runtime/src/spawn.rs   # with_spawn_lock_before (new; public form of spawn_lock_before)
crates/pty-runtime/src/fake.rs    # adds FakeChild::fail_wait
crates/daemon/src/supervisor/     # new: components, commands, live, ops, exit, restart, tick, workspace, record, adapters, thread_read
crates/daemon/src/http/mod.rs     # RevocationNudge, HttpServer::revocation_nudge, StoreFatal::LaunchUnrecorded (new);
                                  #   pub(crate) re-exports of the seams below
crates/daemon/src/http/auth.rs    # mint_token (new free fn; TokenService::mint calls it)
crates/daemon/src/http/ctx.rs     # write_with (new; run_write calls it with the server's parts)
crates/daemon/src/http/commands.rs# CommandHandler::revokes (new); CommandEffect::run resolves to EffectOutcome (new);
                                  #   retryable, backoff_until, BACKOFF_*, COMPLETION_* become pub(crate)
crates/daemon/src/http/tests/commands.rs # existing test effects wrap their results in EffectOutcome::Done
crates/daemon/src/http/routes.rs  # Mounts::mvp_with (new); GET /api/threads/{thread_id} (new)
crates/daemon/src/store/readers.rs# ReadTx::inputs_in_states, ReadTx::thread_response (new)
crates/daemon/src/store/write_tx.rs # TokenSelector::LaunchScoped (new): every token with a launch
crates/daemon/src/lifecycle/mod.rs# ParticipantEnd (new), returned by ShutdownParticipant::shutdown; spawn_locked_before (new)
crates/daemon/src/lifecycle/shutdown.rs # step 6's lock rule counts LeftRunning and an elapsed participant
crates/daemon/src/lifecycle/gate.rs     # fatal_label arm for LaunchUnrecorded
crates/cli/src/client.rs          # post_json (new): request with a JSON body, same deadline rules
crates/cli/src/poll.rs            # new: poll_until moved from daemon_cmd.rs, with a clock seam
crates/cli/src/agent_cmd.rs       # new: spawn, resume, stop, close, wait
crates/cli/src/lib.rs             # Seams gains host_os (new) for the startup refusal test
crates/cli/src/daemon_cmd.rs      # default components: SupervisorComponents instead of MvpComponents
crates/testkit/src/fake_cli/banner.rs # new FakeCli mode
crates/cli/tests/supervisor_e2e.rs    # new
```

**Seams the supervisor needs and how it gets them.** `Components::build` gets only `BuildCtx` (store, gate, sink, port, state dir) at step 11 and `Components::attach(&HttpServer)` at step 12 (`lifecycle/startup.rs:647-676`); `http::Shared` is reachable only from tests (`http/mod.rs:339`), and the `http` submodules are private (`http/mod.rs:19-34`), so each seam is re-exported from `http` as `pub(crate) use`. Tokens are minted with `mint_token` and rows built with `TokenService::new_token` (an associated function), both data only; the supervisor's writes go through `write_with(&Store, &dyn LifecycleSink, clock, not_after, job, after_ok)`, the body of `run_write` without `Shared`; the revocation nudge is a cloneable `RevocationNudge` taken from `HttpServer::revocation_nudge` in `attach` and stored once (before the listener exists, so no revoking job runs earlier); the supervisor's clock is `SupervisorConfig.clock` (`SystemMonotonic` outside tests).

**New identifiers** besides those of the tree above: `supervisor` (module), `Supervisor`, `SupervisorConfig`, `SupervisorComponents`, `LiveLaunch`, `LiveLaunch::with_session`, `Supervisor::live`, `Supervisor::observe`, `Supervisor::admit`, `CreationGuard`, `LaunchObservation` and its variants, `Published` and `CommittedStart` and their fields (`reference`), `SessionError`, `LaunchOp`, `ExitStep` (with `after_commit`), `ExitFacts` (with `sealed`), `TickStep`, `Supervisor::tick`, `supervisor::record`, `supervisor::record::Job`, `supervisor::tick_timer`, `supervisor::workspace::common_dir`, `SpawnArgs`, `ResumeArgs`, `StopArgs`, `CloseArgs`, `SpawnResult`, `ResumeResult`, `StopResult`, `CloseResult`, `AdaptersResponse`, `AdapterInfo`, `ThreadResponse`, `Session::seal`, `Session::observer`, `WriterObserver`, `WriterObserver::last_started_each`, `WriterObserver::try_last_started_each`, `CloseOutcome.exit`, `env_key_eq`, `env_key_has_prefix`, `with_spawn_lock_before`, `spawn_locked_before`, `FakeChild::fail_wait`, `mint_token`, `RevocationNudge`, `HttpServer::revocation_nudge`, `write_with`, `CommandHandler::revokes`, `EffectOutcome` and its variants, `StoreFatal::LaunchUnrecorded`, `ParticipantEnd` and its variants, `Mounts::mvp_with`, `ReadTx::inputs_in_states`, `ReadTx::thread_response`, `TokenSelector::LaunchScoped`, `post_json`, `poll_until_with`, `HostOs::from_os_str`, `launch::plan`, `launch::workspace_id`, `LaunchInput`, `LaunchPlan`, `ENV_SCRUB`, the reconcilers `supervisor_launches` and `supervisor_buffers`, FakeCli mode `banner`, the wire fields and values of §2.2 (`expected_launch_id`, `already_exited`), the error codes of §2.2's code table, the Quint action `spawnStartedThenFailed` and the traces of §7, and every test name in §5.2. `CLAUDE_CODE_USE_BEDROCK` is a Claude Code variable (its settings page); it, `Chaldea_Hook_Token`, `claudecode`, and `linux` are test inputs.

## 2. Design

### 2.1 Rules

1. **One job per transition.** Reservation, start, stop request, exit, and close are each one write job that re-reads the rows it decides on and appends its events with the reducer's `status_changed`. Every job after the runner's intent goes through `supervisor::record` (§2.10).
2. **Launch operations outlive requests.** A command's effect hands its work to a `LaunchOp`, a supervisor-owned task, and awaits its result; the op runs to its end when the runner is aborted or the request goes away, and owns any `Session`, spawn, kill, or close result. Every blocking call an op starts (`Session::spawn`, `Session::close`) and the git child it may leave (§2.3 step 1) is counted by the supervisor until it ends, also after the op stopped waiting for it.
3. **One op lock per launch.** The live map is keyed by `LaunchId`; its entry, locked by its op, exists before the reservation commits; every use of the `Session` and every record of its outcome holds the lock (§2.11).
4. **One admission point for new processes.** Every process creation (the git probe, `Session::spawn`) first passes `Supervisor::admit`, which refuses once shutdown started; the `CreationGuard` it returns is handed over, never dropped, while a created process has no other owner (§2.9).
5. **The full list on every kill.** Every `Session::close` (and, for 10b, any `Session::kill`) gets the launch's full recorded-descendant list; a report's survivors are never passed back (pty-windows §2.5 "The caller's obligation").
6. **What a kill shows goes to the record or the log.** A kill that confirms records its method in the exit job; the survivors of one that cannot confirm are logged (§2.12).
7. **Startup causes for the current, started, unbound launch only**, decided in the job that raises them.

### 2.2 Commands

Registered through `CommandRegistry::register` in `Built::commands` (E1). The guard decides kind and action per name (`crates/core/src/auth.rs` `MVP_COMMANDS`, `grant`). Each handler's `prepare` parses the wire args (400 `bad_args` when they do not fit), mints ids and the hook token with `mint_token`, and builds an intent closure from data only; the handler object (holding `Arc<Supervisor>`) is used only by `effect` (http-surface §2.8).

**Target scope first.** The `resume`, `stop`, and `close` intents check the target thread against the token's grant (`may_act_on_thread`) before they read anything else of it; a refusal is `deny_for` (404 for a thread the token cannot read, identical to a missing one; http-surface §2.5 "Hiding"), with nothing written and no op started.

| Command | Args (wire types) | Intent (the runner's job) | Effect: `LaunchOp` | Success | Deadline |
|---|---|---|---|---|---|
| `spawn` | `SpawnArgs { agent, cwd, prompt?, task? }` | object authorization; returns `Pending` | §2.3 "Spawn and resume" | `SpawnResult { thread_id, launch_id, input_id?, outcome: started \| spawn_failed }` | 30 s |
| `resume` | `ResumeArgs { thread_id, expected_launch_id, prompt? }` | target scope; returns `Pending` | §2.3 "Spawn and resume" (the reservation checks run in the op's reservation job) | `ResumeResult { launch_id, input_id?, outcome }` | 30 s |
| `stop` | `StopArgs { thread_id, expected_launch_id? }` | target scope; the current launch `L` (it must equal `expected_launch_id` when given, else `launch_changed`); `L` exited with death confirmed → `Completed(already_exited)`; exited unconfirmed → `death_unconfirmed`; else `Pending(L)` | §2.3 "Stop" for `L` | `StopResult { launch_id, outcome: stopped \| already_exited, death_confirmed, code?, signal? }` | 30 s |
| `close` | `CloseArgs { thread_id, expected_launch_id, stop?: bool }` | target scope; `thread_closed`; `launch_changed` (current ≠ `expected_launch_id`); then §2.3 "Close" | stop of a live `L`, then the closing job (with the nudge) | `CloseResult { thread_id }` | 30 s |

`chaldea wait` is a CLI read loop, not a command (SD16).

**Caller.** An owner token spawns a top-level thread (`parent_thread_id: None`); an agent token (PR 16 mints them) spawns a child of its own thread (`parent_thread_id` = the token's thread, the `ChildrenOfSelf` grant of `auth.rs:144`). Both get `role: standard` (`--role rescue` is PR 20's; PR10a1-CL-34).

**Prompt admission** (C1; mvp §8 D5). A `prompt` is accepted only for Codex (Claude's first prompt is PTY input after readiness, a 10b `send`: until 10b, a Claude `prompt` is 400 `bad_args`). In `prepare`: longer than `MAX_PROMPT_BYTES` → 400 `prompt_too_large`; `check(Codex, Positional, Purpose::Send, text)` → `Err` is 400 `prompt_rejected`; both before any receipt or event. The CLI runs the same `check` first to print the reason, since the error body carries only the code. A Codex launch without a prompt is a manual launch (`LaunchReserved.manual`).

**Filesystem refusals** come from the op, after the receipt exists: a `cwd` that does not exist or is not a directory is 400 `bad_args` with no reservation, token, event, or spawn; the runner stores that reply, so a retry with the same `command_id` replays it even after the directory appears (a new request needs a new `command_id`). `prepare` reads no filesystem.

**Codes.** New `ErrorCode` variants, each classified in `handler_code` at exactly one status (E2):

| Code (new) | Status | When | The work can happen later? |
|---|---|---|---|
| `thread_closed` | 409 | resume or close on a closed thread (a closed thread's launch is confirmed dead, so its stop is `already_exited`) | no |
| `launch_changed` | 409 | `expected_launch_id` is not the thread's current launch; a stop or close `--stop` whose target launch stopped being current before its op acted | no |
| `launch_live` | 409 | resume or close (without `stop`) while the current launch has not exited | no |
| `death_unconfirmed` | 409 | resume, close, or stop while the current launch exited without confirmed death (resume also raises `resume_blocked` in the same job); 10a1 has no rule that confirms it (§2.4) | no |
| `worktree_unavailable` | 409 | the launch's worktree is not `active` or `released` | no |
| `no_session` | 409 | resume of a thread with no bound `session_id` | no |
| `prompt_rejected` | 400 | `check` refused the positional prompt | no |
| `prompt_too_large` | 400 | the prompt exceeds `MAX_PROMPT_BYTES` (also 10b's admission code, C8) | no |
| `launch_busy` | 504 | stop could not take the launch's op lock in time (§2.3) | no: the stop is abandoned |

Success types have no top-level `code` field; a test asserts it per type (E2).

### 2.3 Launch Lifecycle

**Spawn and resume** (the `LaunchOp`; `L` is the launch id minted in `prepare`):

1. *Inputs.* `cwd` (spawn) must be a directory, else 400 `bad_args` (§2.2). Resolve the program (§2.7); a missing one is kept as `None`. Spawn only, the workspace probe: `supervisor::workspace::common_dir(cwd, now + 5 s)` runs `git -C <cwd> rev-parse --path-format=absolute --git-common-dir` (stdin and stderr null, stdout piped; `CREATE_NO_WINDOW` on Windows). One git child exists at a time: the probe takes the supervisor's git slot by its deadline, passes `admit`, is created through `spawn_locked_before` with that deadline, and is waited for by exact pid with `host::observed_before` polls of `try_wait`. Exit 0 → the printed path (read after exit; one line, far below a pipe buffer). The slot not taken, the spawn lock not taken, refused admission, a nonzero exit, or no exit by the deadline → the canonical cwd, and the daemon log names which. A child not exited by the deadline is not killed: it keeps the git slot, the tick waits for it by exact pid (`try_wait`), and until that wait returns its status every probe falls back to the canonical cwd. `launch::workspace_id(path)` = `ws-` + the first 16 hex digits of SHA-256 of the path's UTF-8 bytes (mvp §2.1:103).
2. *Slot.* Insert the `LiveLaunch` for `L` with its op lock taken by this op.
3. *Reservation job.* Resume first re-reads the thread and its current launch and refuses, in this order: `thread_closed`; `launch_changed` (current ≠ `expected_launch_id`); `launch_live` (no `exit_reason`); `death_unconfirmed`, raising `resume_blocked` when absent (mvp §2.9 "Daemon restart"); `worktree_unavailable` (the previous launch's `worktree_id`, if any: only `active` and `released` pass; mvp CL-18); `no_session`. A refusal commits only the raise, removes the slot, and is the op's result. Then, in the same job: spawn's `thread_registered`; `launch_reserved` (`expected_previous`, `argv_digest` of the planned argv, `manual`, and for resume the previous launch's `worktree_id`, so `launches_using_worktree` returns the new launch); `StateInput::Reserved`; the positional `input_created { initial_state: sending, encoding: positional, origin: cli }`; and `token_insert` of the hook token. A rollback removes the slot.
4. *Spawn.* Pass `admit` (its `CreationGuard` marks the slot as creating, §2.11), then `Session::spawn(runtime, &spec, config)` on `spawn_blocking`, awaited by the op to its result (§2.10). Refused admission or no program → the `NeverStarted` branch of step 6, with no process created. `config.on_event` forwards `SessionEvent::Exited` and `SessionEvent::WaitFailed` to the launch's exit op without blocking.
5. *Publication.* On `Ok(session)`: take `observer = session.observer()` and `reference = observer.last_started(ClassSet::ALL)`; then, under the map mutex, if admission is still open, put the session in the slot, set `Published { observer, reference }`, and end the `CreationGuard`, in that one critical section. If admission closed meanwhile, the op keeps the guard and closes the session itself (§2.9 "Late creations"). A write with a `seq` above `reference` is a write after the positional prompt's anchor (A35's anchor; 10b decides what it means for an ack).
6. *Start job*, by outcome:
   - `Ok(session)` → `launch_started { pid, start_time, pgid_or_job, cli_version: None }` from `Session::identity`, `StateInput::Spawned`, the positional input `sending → sent` (`InputInput::WriteCompleted`, `sent_after_seq` = this `launch_started`'s seq; mvp §2.3:186), and for an upgraded Codex launch the `startup_stall` raise (§2.6). After the commit the op sets `CommittedStart { seq, identity, at }` (`at` read after the commit, on the supervisor's clock) and releases the op lock.
   - `Err(SpawnError::NeverStarted { .. })`, refused admission, or no program → `launch_exited { reason: spawn_failed, death_confirmed: true }` (no process ran and any process created was reaped, `backend.rs:186-193`), the positional input `sending → rejected(spawn_failed)`, and the exit steps of §2.11.
   - `Err(SpawnError::StartedThenFailed { cleanup, identity, .. })` → `launch_exited { reason: spawn_failed, death_confirmed: cleanup == Cleanup::Confirmed }`, the positional input `sending → unconfirmed` (the CLI may have read its argv) with `input_unconfirmed`, and the exit steps; an unresolved cleanup's identity and survivors are logged (§2.12).

Fresh and resumed launches reserve in the op rather than in the intent because the slot must exist before the reservation is visible (rule 3) and spawn needs the workspace probe first (SD11).

**Stop** (the `LaunchOp` for a live `L`):

1. Take the op lock of `L` with `deadline::settle_before(…, now + 10 s)` and `in_time()`; `Elapsed` or `Late` → 504 `launch_busy`, nothing written (a `Late` guard is dropped at once). A spawn in progress, an exit op, or a 10b call holds the lock (X5). The slot exists while `L` is not exited, since it precedes the reservation commit; a missing slot means `L` exited, and the op reads it.
2. Re-read `L` in a job: not current → `launch_changed`; exited → `already_exited` or `death_unconfirmed` as the intent decides; running → `sealed = Session::seal()`, then a job with `StateInput::StopRequested`. The seal is the irreversible stop boundary (SD18, §2.11 "Stop before write").
3. `Session::close(&extra, CloseBounds::default())` on `spawn_blocking`, awaited by `settle_before` until its bound + 1 s. Close's step 2 is the tree kill (`bounds.kill` = 5 s), so the stop makes no separate kill call; mvp P-3's 5 s is that bound, counted from the close's start, which a stop waiting for a spawn in progress reaches up to 10 s later (§8).
4. The exit job of §2.11 with `reason: stopped`, `code` and `signal` from `CloseOutcome.exit` (null when the waiter had not recorded an exit by the join), `death_confirmed` = `CloseOutcome.kill.tree_dead` when the join returned in time, and `ExitFacts.sealed` = the seal's records; then `launch_death_confirmed { method: KillReport.method }` in the same job. A `Late` or `Elapsed` join or a panic records `death_confirmed: false`; a close that returned `tree_dead: false` has its survivors logged. A close result that arrives after the op stopped waiting is logged and dropped by its blocking task.

Worst case within the 30 s deadline: lock 10 s + `StopRequested` job + close 11 s + 1 s join margin + exit job, about 22 s plus two jobs' store time. A store slower than the remaining 8 s turns the reply into 409 `in_progress`, and a retry with the same `command_id` replays the stored result.

**Natural exit.** The exit op receives `SessionEvent::Exited(exit)` or `SessionEvent::WaitFailed`. It takes the op lock in 10 s rounds, re-reading the launch after each (a stop holding the lock records the exit itself); every holder's work is bounded (§2.10), so after 60 s it reports `StoreFatal::LaunchUnrecorded`, unless shutdown started, when it ends without a report and the next start records the launch (§2.5). Holding the lock, it returns if `L` has an `exit_reason`; else it runs Stop's step 3 and records `reason: exit` (or `stopped` when the state is `stopping`), with `code` and `signal` from the callback's `ChildExit` (null after `WaitFailed`). `WaitFailed` means the waiter could not report an exit (pty-runtime §2.2; on macOS after ownership was lost, after another wait error, or after a waiter panic, `unix/child.rs:183-200`; on Windows a waiter error independent of the job proof, `windows/child.rs:69-85`); it decides nothing about death, which is the close's `tree_dead` in every case (A25).

**Close.** The intent, with or without `stop`: target scope, `thread_closed`, `launch_changed` (current ≠ `expected_launch_id`). Then without `stop`, or with `stop` and an exited launch: `launch_live`, `death_unconfirmed`, else `thread_closed { by }` and `token_revoke(TokenSelector::Thread(t))` in the same job. With `stop` and a live launch: `Pending(L)`; the op runs Stop for `L`, then the closing job with the same checks (a resume that reserved a newer launch meanwhile gives `launch_changed`; an unconfirmed stop gives `death_unconfirmed`). Worktree hand-over (mvp §2.6) is PR 16's and `after_close` PR 20's, each added to the closing job.

**Requests and ops.** The op's result is the effect's result. `CommandEffect::run` resolves to `EffectOutcome`:

- `Done(CommandResult)`: the runner completes the receipt and replies, as today.
- `Unrecorded`: a job of the op could not be recorded within `supervisor::record`'s budget while the daemon runs. The runner does not complete the receipt (it stays `in_progress` and becomes `unknown_after_crash` at the next start), replies 503 `outcome_unknown` if the request is still waiting, and reports `StoreFatal::ReceiptUncompleted` (`commands.rs:681-697`). A budget longer than the request's deadline means the client has usually received 409 `in_progress` first.
- `ShuttingDown`: the op stopped because admission closed or the store refused a write while shutdown runs. The runner does not complete the receipt, replies 503 `shutting_down` if the request is still waiting, and reports nothing.

A handler cannot reply 503 itself (`result_allowed`, `commands.rs:152-176`), and a stored 500 would claim an outcome nobody knows.

### 2.4 Death Proof

**While the daemon owns the session**, only the explicit close decides (§2.3), with the full list and a join that returned in time. The `Session` leaves the slot only into that `close`; a `Session` dropped elsewhere (a panic, a blocking task outliving the daemon's shutdown) closes with `&[]` and is never recorded as a proof (A27).

**`extra` and its source** (B1, A3, A26). The supervisor passes, on every call, every identity it can prove belongs to the launch: its own spawn records and containment it can show (hook-ingest D10). In the MVP no such identity exists outside what the backend already covers: job members (Windows) and session and group members observed while owned (macOS) are the backend's, the daemon creates no other process per launch, and PR 9 records none (hook-ingest D10). So the full list is the empty list on every call; a descendant that left the session before any observation is PR3-P-1's stated limit. A later producer records identities on the `LiveLaunch` and they are passed from then on.

**Unconfirmed launches.** A launch exits with `death_confirmed: false` when its owned close could not prove death, when a `StartedThenFailed` cleanup is unresolved, or at a restart (`daemon_restart`, `unknown_after_crash`; §2.5). 10a1 has no rule that confirms such a launch: `stop` answers `death_unconfirmed`, `resume` and `close` refuse with it (resume raising `resume_blocked`), and the launch's worktree stays in use. New threads spawn as usual, and history is intact. Confirming such a launch is PR 10a2's (§2.12, L-1).

**A29 on Windows.** A crash between the suspended `CreateProcessW` and `AssignProcessToJobObject` leaves a reservation without identity and a suspended child outside the launch's job (pty-windows §2.2 "Crash window"). 10a1 records the launch `unknown_after_crash` (§2.5) and keeps pty-windows' suspended fallback unchanged (SD2); what PR 21's finder receives is 10a2's.

### 2.5 Restart

**Reconcilers** (E3; daemon-lifecycle §2.14: each its own job, at step 9, before the listener exists), returned by `SupervisorComponents::reconcilers` in this order:

1. `receipts_mark_unknown_after_crash` (PR 8, unchanged).
2. `supervisor_launches`, one job:
   1. For each `open_launches()` row with `exit_reason` null, in `(thread_id, reserved_seq)` order: `launch_exited { reason: daemon_restart` if `pid` is recorded, else `unknown_after_crash`; `code`, `signal` null; `death_confirmed: false }` with `StateInput::Exited`, then the exit steps of §2.11 with `restart: true`.
   2. Every input of any thread in `queued`, `sending`, or `sent` (`ReadTx::inputs_in_states`): `queued → cancelled`, `sending`/`sent → unconfirmed` through `InputInput::DaemonRestarted`, raising `input_unconfirmed`. Inputs already `unconfirmed` keep their state and attention (`step(Unconfirmed, DaemonRestarted)` is invalid).
   3. `token_revoke(TokenSelector::LaunchScoped)`: every token bound to a launch, exited ones included, since none of them is current and live after a restart (hook-ingest §2.8: revocation at restart ends a hook token). Owner tokens have no launch and stay.
   A second restart finds nothing to change: no open launch, no input in those states, no unrevoked launch token.
3. `supervisor_buffers`, one job: `buffer::resolve_unbindable` for each `buffered_launches()` (A22; every launch is exited by now). It keeps daemon-lifecycle's order "launches, then buffered deliveries" and covers buffers left by launches that exited in an earlier run.

`ExitStep`s and `TickStep`s are assembled on `SupervisorComponents` when it is constructed (before step 9) and shared with the `Supervisor` that `build` creates, so 10b's steps run at restart too. No nudge is needed at restart: no socket exists before the listener. The positional input of a launch that crashed before `launch_started` ends `unconfirmed` through step 2.2 (A21). 10a1 neither signals nor checks old processes at startup (SD3, L-2).

### 2.6 Startup Causes and the Tick

One supervisor task ticks every second; its timer is the new exception `supervisor::tick_timer` (`sleep_until`). `Supervisor::tick(now)` is the seam tests drive with an injected instant. Each tick waits for a git child left in the git slot (`try_wait`, §2.3 step 1), takes a candidate list from the live map, and then runs, per candidate, a job that re-validates everything; registered `TickStep`s (10b's gate timeout and ack window) run after 10a1's:

- **`session_start_timeout`** (D4, A32): a candidate is a non-manual launch with a `CommittedStart` and `now ≥ CommittedStart.at + 20 s` (mvp §2.2:142) on the supervisor's monotonic clock; publication for observation (§2.3 step 5) is not a start. The job raises `AttentionCause::SessionStartTimeout` only if the launch is the thread's current launch, its row has `launch_started`'s identity, the thread's process state is `running` or `stopping`, it has no `session_id` (unbound), and the cause is absent. The condition is "unbound", not "no `SessionStart` seen": a `SessionStart` whose walk ends below the launch is `foreign_session` and leaves the launch unbound, and hook-ingest §2.5 (line 268) assigns reporting it to this cause (Quint change in §7).
- **`buffer::expire`** for each live, unbound launch with buffered deliveries (A22); its age is decided by store stamps as PR 9 designed (`buffer.rs:92-106`).

A tick job that cannot be recorded within its budget is skipped; the next tick decides again from the store. **`startup_stall`** (A32) is raised in the start job for a Codex launch whose plan says `upgraded`; `upgraded` is false until PR 14 supplies the CLI-version comparison (mvp §2.7 "Hook trust"). Both causes are cleared by the reducer on `StateInput::SessionBound` and `StateInput::Exited`. Hole c (formal-specs §3) is closed by the job-side guards: nothing raises a startup cause for an exited, unstarted, or non-current launch.

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

### 2.8 Reads, `GET /api/adapters`, and the Host OS

`HostOs::from_os_str(std::env::consts::OS)` maps `"macos"` and `"windows"`; anything else is `None`, and `chaldea daemon run` exits `Failed` before startup with a message (C7). The CLI computes it once (`Seams.host_os` replaces the string in tests) and passes it to `SupervisorComponents`; the `Supervisor` passes it by value into every `prepare`.

`GET /api/adapters` (`Mount::read`, `ReadAdapters`, Bearer owner, agent, rescue; the existing Host and Origin rules) answers from `CAPABILITIES` with no store read (C6; SD5):

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
pub struct ThreadResponse {
    pub thread: ThreadInfo,
    pub launch: Option<LaunchInfo>,          // the current launch
}
```

Later adapter columns (readiness mode, resume support) are added as fields by PR 13 and PR 14. `submit_delay` stays internal.

`GET /api/threads/{thread_id}` (new, `Mount::read`) answers `ThreadResponse` from one read transaction (`ReadTx::thread_response`, built as `thread_snapshot` builds its `thread` and `launch` fields, `readers.rs:511-518` and `readers.rs:587-594`), authorized as a read of that thread's stream (404 for a thread the token cannot read, as `GET /api/events`). Its size does not grow with history, so the CLI reads it with `client::request`'s body bound. `chaldea resume`, `chaldea close`, and `chaldea wait` read it (SD16).

### 2.9 Components, Shutdown, and Fatal Outcomes

**Components.** `chaldea daemon run` builds `SupervisorComponents::new(host_os, SupervisorConfig::default())` instead of `MvpComponents` (`daemon_cmd.rs:136-139`; tests pass their own through `Seams.components`). Its `reconcilers` are §2.5's; its `build` creates the `Supervisor`, the four handlers, the participant, and the mounts `Mounts::mvp_with(hook_seams, extra_reads)`, where `hook_seams` is `HookSeams::mvp()` in 10a1 and the construction point where 10b supplies its `IntakeProbe` and PR 13/14 their `HookAdapter`, and `extra_reads` mounts `GET /api/adapters` and `GET /api/threads/{thread_id}`; its `attach` takes the `RevocationNudge` and starts the tick.

**Admission and ownership.** The supervisor's map mutex also guards `accepting` and the live `CreationGuard`s. `Supervisor::admit` returns a guard only while `accepting`. A guard ends in exactly one of these ways: under the map mutex, in the same critical section that puts the created session into its slot (publication, §2.3 step 5); when the creation made no process; when the op has closed a session created after admission closed (below); or, for git, when the tick's wait returned the child's status. Before the participant runs, HTTP's drain has requested the abort of the runners still running at its deadline (`http/mod.rs:364-376`), but ops outlive their runners (§2.1 rule 2), so creations continue until the participant closes admission.

**Shutdown** (SD10). The participant gets `end` (participants share the 2 s between `C − 5 s` and `C − 3 s`, daemon-lifecycle §2.14); with `t0` its start, `r = end − t0`, `j = min(300 ms, r/4)`, `lock_by = t0 + r/4`, and `close_end = end − j`, all stored on the supervisor when admission closes:

1. Under the map mutex: `accepting = false`. The tick stops.
2. By `lock_by`: the op lock of every slot that holds a published session (`settle_before`, `in_time()`; a lock not taken is left).
3. For each locked launch in parallel: `Session::seal`, then `Session::close` with `CloseBounds::default()` scaled by `(close_end − now) / CloseBounds::default().total()` (`limits.rs:61-78`), joined with `settle_before` until `close_end`. With `r = 2 s` a close starting at `lock_by` gets 1.2 s, its kill 545 ms: the HUP and TERM waits end at 136 ms and 272 ms (pty-unix §2.5 step 3).
4. For each close whose result was received: one job, submitted with `not_after = end` and its reply awaited until `end`, by the launch's recorded state: `starting` (published, but no `launch_started` committed: its start job failed or is pending) → `launch_started` from `Session::identity` and `StateInput::Spawned`, then as `running`; `running` → `StateInput::StopRequested`; `stopping` → nothing more; then `launch_exited { reason: stopped }` with the close's `tree_dead`, the method when confirmed, and the exit steps; `exited` → no job.
5. Final check, under the map mutex: `ParticipantEnd::LeftRunning` if a `CreationGuard` is alive, a slot holds a session this participant did not close, a blocking close is still running, or a git child holds the git slot; else `Clean`.

**Late creations.** An op whose `Session::spawn` returns after admission closed finds `accepting = false` at publication: it neither publishes nor starts the session. It seals and closes it itself with bounds scaled to `close_end` and records the step 4 job for a `starting` launch by `end`; its guard ends when that close returns, so a late creation still running at the final check makes the participant return `LeftRunning`.

**The lock rule.** The coordinator records both `Ok(ParticipantEnd::LeftRunning)` and a participant not done by `end` (`Err(Elapsed)` of `participant_bound`, `shutdown.rs:635-638`) as reasons in step 6, so the daemon exits with the instance lock held and no new daemon starts while a process the old one created may still appear. Only creations, closes, and the git child count; the result is `FailReason::LockHeldAtExit` for that stop.

Each launch ends in one of these classes:

| Class | Record | At the next start |
|---|---|---|
| close confirmed, commit observed | `stopped`, confirmed | nothing |
| close not confirmed, commit observed | `stopped`, unconfirmed; survivors logged | stays unconfirmed (L-1) |
| close result not received, or lock not taken | none | `daemon_restart` if `launch_started` was committed, else `unknown_after_crash`; unconfirmed (L-1, L-5) |
| commit not observed by `end` | whatever committed | as recorded, or as the row above |

A `Session` a blocking task still holds at process exit is dropped there, never a proof.

**Fatal outcomes.** Before shutdown: an exit op that cannot take its lock or record within 60 s reports `StoreFatal::LaunchUnrecorded` through `BuildCtx::sink`; a command op that cannot record resolves `EffectOutcome::Unrecorded`, and the runner reports `ReceiptUncompleted` (§2.3). Once admission is closed, an op that cannot record ends with `ShuttingDown` and no report. Nothing else in the supervisor stops the daemon. If a stop's `StopRequested` job cannot be recorded, the op ends `Unrecorded`; the sealed session stays in its slot, and the participant of the shutdown that the fatal starts closes it. Children: the PTY children through the backend (which takes the spawn lock), git through `spawn_locked_before`; no other process is created (A25). The CLI verbs use `cli::client` (`discover`, `read_owner_token`, `request`, `post_json`).

### 2.10 Waits

| Wait | Helper | Bound |
|---|---|---|
| Op lock (stop) | `deadline::settle_before` on the lock future, `in_time()` | 10 s; `Elapsed` or `Late` → `launch_busy` |
| Op lock (exit op) | the same, in rounds | 10 s rounds, 60 s in all, then `StoreFatal::LaunchUnrecorded` (none once shutdown started) |
| Op lock (participant) | the same | `lock_by` |
| `Session::close` (stop, exit op) on `spawn_blocking` | `deadline::settle_before` on the join handle, `in_time()` | `CloseBounds::default()` + 1 s; otherwise unconfirmed |
| `Session::close` (participant, late creation) | the same | `close_end` (§2.9) |
| Store jobs (`supervisor::record`) | the runner's receipt-completion policy (`commands::complete`): `write_with` attempts of `COMPLETION_ATTEMPT`, retried only on `retryable` outcomes (`QueueFull`, `Expired`, `Busy`, `RolledBack(Busy)`) with `backoff_until` from `BACKOFF_FIRST` doubling to `BACKOFF_MAX`, admitted for `COMPLETION_BUDGET`; an admitted attempt is awaited to the writer's reply | then the op ends `Unrecorded` (`ShuttingDown` once admission closed); tick jobs are skipped. `CommitUnknown`, `WriterGone`, and `Failed` are reported by `write_with` and end the op; a job's own refusal is a value, not a failure |
| Store jobs (participant, late creation) | `write_with` with `not_after = end`; the reply under `settle_before` | `end`; otherwise "commit not observed" |
| Git probe | the git slot and `spawn_locked_before` by the probe's deadline, then `host::observed_before` polls of `try_wait` | 5 s; a child still running keeps the slot until the tick's `try_wait` returns its status |
| Tick | `supervisor::tick_timer` (new exception, `sleep_until`) | 1 s period |
| CLI `wait`, reads, and retries | `poll_until_with` | one cutoff (SD16) |
| **`Session::spawn`** | **none** | Cancelling it cannot stop it: the blocking call would still create the child. The op, not the request, awaits it; its bound is the spawn lock's holders, each of which does bounded work (pty-unix:122; pty-windows §2.2 "`spawn` has no deadline of its own"). The request replies at its own deadline through the runner (409 `in_progress`). |

### 2.11 Interfaces for 10b

```rust
// crates/daemon/src/supervisor/live.rs (new)
pub(crate) struct LiveLaunch {
    pub thread_id: ThreadId,
    pub launch_id: LaunchId,
    pub agent: Agent,
    pub caps: &'static Capabilities,
    creating: AtomicBool,                        // set by admit, cleared at publication
    published: OnceLock<Published>,              // §2.3 step 5
    committed: OnceLock<CommittedStart>,         // after the start job's commit
    ops: Arc<tokio::sync::Mutex<Option<Session>>>,
}
pub(crate) struct Published { pub observer: WriterObserver, pub reference: u64 }
pub(crate) struct CommittedStart { pub seq: u64, pub identity: ProcessIdentity, pub at: std::time::Instant }
pub(crate) enum SessionError { Gone, Busy }
pub(crate) enum LaunchObservation {
    Reserved,                                            // no child creation has begun
    Started { reference: u64, last_started: [u64; 3] },  // per OriginClass::index
    Unobservable,                                        // creating, contended, or no live entry
}

impl Supervisor {
    pub(crate) fn live(&self, thread: ThreadId, launch: LaunchId) -> Option<Arc<LiveLaunch>>;
    /// Never waits: for the hook intake path.
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
    /// Close's step 1 alone; idempotent. Returns the records of the units it settled `Closed`
    /// with zero bytes, after running their completions.
    pub fn seal(&self) -> Vec<WriteRecord>;
    pub fn observer(&self) -> WriterObserver;    // Clone; outlives the Session
}
impl WriterObserver {
    pub fn last_started(&self, classes: ClassSet) -> u64;
    pub fn last_started_each(&self) -> [u64; 3];              // one queue lock, per OriginClass::index
    pub fn try_last_started_each(&self) -> Option<[u64; 3]>;  // None while the queue lock is held
}

pub(crate) trait ExitStep: Send + Sync {
    fn in_exit_job(&self, tx: &mut WriteTx<'_>, exit: &ExitFacts) -> Result<(), StoreError>;
    fn after_commit(&self, _exit: &ExitFacts) {}
}
pub(crate) struct ExitFacts {
    pub thread_id: ThreadId, pub launch_id: LaunchId, pub reason: ExitReason,
    pub death_confirmed: bool, pub exited_seq: u64, pub restart: bool,
    pub sealed: Vec<WriteRecord>,                 // the stop's seal; empty otherwise
}
pub(crate) trait TickStep: Send + Sync {
    fn tick(&self, now: std::time::Instant, live: &[Arc<LiveLaunch>]) -> Vec<supervisor::record::Job>;
}
// crates/daemon/src/supervisor/record.rs (new): one job, run again on each attempt.
pub(crate) type Job = Arc<dyn Fn(&mut WriteTx<'_>) -> Result<(), JobError> + Send + Sync>;
```

- **Lookup by generation.** The live map is keyed by `LaunchId`; `live(thread, launch)` returns the entry only if its `thread_id` is `thread`, so a call cannot reach another thread's or a newer launch.
- **Access rule.** The map sits under a `std::sync::Mutex` held only for map operations and admission, never across an await, a lock, or a job; `live` and the supervisor's own paths take it. Every use of a `Session` goes through `with_session`; no code keeps a `&Session` or the guard after `f` returns. `Gone`: the session left the slot (exit recorded or being recorded); `Busy`: the lock was not taken by `deadline` (`Elapsed` or `Late`, a `Late` guard dropped at once). 10b submits a unit inside `f` and awaits its completion outside it. The supervisor's own blocking calls (`close`) are not run through `with_session`.
- **Observation.** `observe` never waits: it `try_lock`s the map (contended → `Unobservable`), reads the entry (none → `Unobservable`), and then: no creation begun → `Reserved` (no child exists, so no write can precede the hook); `creating` set without `Published` → `Unobservable` (the writer may already dispatch, for example a VT reply to the CLI's startup queries, before the session reaches the op); `Published` → `try_last_started_each` (queue contended → `Unobservable`), giving `Started { reference, last_started }`. A class position above `reference` is a write of that class after the reference; a write between the creation and `reference` is before it. PR 9 samples intake facts once, before the pid wait, under the intake mutex (`hook/mod.rs:224-230`, `hook/intake.rs:18-22`), so 10b's `IntakeProbe` calls `observe` there and encodes the observation as its `IntakeFacts`; `Unobservable` tells 10b the facts are missing, never that no write happened.
- **Stop before write** (SD18). `Session::seal` closes the queue: every unit not yet permitted is settled `Closed` with zero bytes, its `Completion` run before `seal` returns, and its record returned; close's own step 1 then finds the queue closed. A stop seals before its `StopRequested` job, holding the op lock, so no unit is permitted once the state is `stopping`; a unit permitted before the seal follows the PTY contract (it may still reach the backend, pty-runtime §2.3 "Write permission"). From the seal to the `StopRequested` commit the thread reads `running` and every submission is refused `Closed`.
- **Sealed results reach the exit job.** The stop op passes the seal's records as `ExitFacts.sealed`, immutable data in the exit job. Each zero-byte result is applied once: 10b's exit step applies a sealed record only to an input still in flight in that job, and 10b's completion consumer applies a completion only to an input still in flight in its own job and ignores it after `launch_exited` (pty-runtime §2.3 "How the daemon uses it"). Whichever job commits first applies it; the other finds the input settled.
- **Lock holders.** A spawn op (slot insertion → start job commit), a stop op (lock → exit job commit), an exit op (lock → exit job commit), the participant, and 10b's calls, each bounded (§2.10). Order: the map mutex, then `ops`; a store job may run while `ops` is held, never the other way round.
- **Exit job order:** `launch_exited` with the reducer's `Exited`; then the registered `ExitStep`s in registration order (10b: the sealed records and the writer outcomes it holds, then its gate state; PR 13: card expiry, `expired`); then 10a1's input sweep: the thread's inputs still in `queued`, `sending`, or `sent` whose `launch_id` is `L` or none (queued for the thread before the exit) move `queued → cancelled`, `sending`/`sent → unconfirmed` with `input_unconfirmed`, through `InputInput::LaunchExited`, or `InputInput::DaemonRestarted` when `restart` (mvp CL-6); `unconfirmed` inputs keep their state; then `buffer::resolve_unbindable`. The job passes the nudge as `after_ok` (it ends the launch's agent-token validity, PR 16). After commit the `after_commit` hooks run in order (10b: drop the launch's writer queue).
- **Restart** runs the same steps with `restart: true` (§2.5); 10b adds only its gate state there.
- **Hook seams.** 10b's `IntakeProbe` (over `observe`) and PR 13/14's `HookAdapter` go into the `HookSeams` that `SupervisorComponents::build` passes to `Mounts::mvp_with` (§2.9).

### 2.12 Handed to PR 10a2

**What moves.** The restart probe of old processes (the recorded identities of `daemon_restart` launches, including those an earlier start recorded), launch evidence for the owner, `stop --assume-dead` and its preview, the evidence PR 21's finder receives for A29, the confirmation of every launch 10a1 leaves unconfirmed (§2.4), and mvp P-18's survivor policy, an owner decision taken when 10a2 is designed. 10a2 implements the owner decision of 2026-11-25 (header): a check that covers only recorded identities unlocks nothing; the owner confirms after reading the evidence.

**What 10a1 records for it.** All of it is in append-only events, so launches 10a1 marked stay readable across upgrades and repeated restarts, and a later restart changes none of them (§2.5):

- the identity of every started launch in `launch_started { pid, start_time, pgid_or_job }`;
- why each unconfirmed launch is unconfirmed, in `launch_exited { reason, death_confirmed: false }`: `daemon_restart` (identity recorded), `unknown_after_crash` (none recorded: A2, A29, or a late creation whose record did not commit), `stopped` or `exit` (an owned close could not prove), `spawn_failed` (a `StartedThenFailed` cleanup unresolved);
- `launch_reserved`'s `recorded_at`, the thread's agent and cwd, and the `resume_blocked` raise of a refused resume.

**What 10a1 only logs.** The survivors of an owned close that could not prove death, a `StartedThenFailed` identity and its cleanup, and the resolved program path. Recording them is the evidence design of 10a2.

**Where 10a2 plugs in.** The `stop` intent's `death_unconfirmed` branch (§2.2); `SupervisorComponents::attach` for a prober task; the participant's final check (§2.9), which would count probes that create no process only if 10a2 decides so; and the daemon-lifecycle lock rule through `ParticipantEnd`.

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

**SD2. Crash windows without identity (A2, A29).** A crash between `launch_reserved` and `launch_started` leaves no identity; on Windows the A29 instance also leaves a suspended child outside the launch's job (pty-windows §2.2 "Crash window"). Adopted: record `unknown_after_crash` with death unconfirmed and keep pty-windows' suspended fallback; what the owner and PR 21's finder are given is 10a2's (§2.12). Rejected: refusing the suspended fallback (every host or case where the job-list attempt fails, pty-windows §2.2 stage 5 "Suspended attempt", could no longer launch).

**SD3. Restart records and does not probe (A1, F1).** mvp §4 asks for a restart scan of the launch's session by recorded sid with `launch.start_time ≤ start ≤ restart_now`. Adopted: 10a1's restart records exits, recovers inputs, and revokes tokens (§2.5), and touches no old process; the probe of recorded identities is 10a2's, under the owner decision. Rejected: the sid scan (start stamps are wall-clock stamps used only for equality, pty-unix §2.1 "Identity format"; without ownership, sid `L` can name a later session once `L` is reused, so a clean scan proves nothing: pty-unix's census is sound only while the daemon owns the leader, pty-unix §2.4 "Ownership is a precondition"). Public: mvp P-18 is not met until 10a2 (L-2).

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

**SD6. Who attributes `extra` (B1, mvp:437 "PR 9 / 10").** hook-ingest D10 gives the obligation to PR 10 entirely and PR 9 records none. Adopted: PR 10a1; in the MVP the list is empty (§2.4). Rejected: recording hook-emitter ancestry as `extra` (a matching creation time identifies a process, not its provenance; pty-windows §2.5 "Extras are trusted input").

**SD7. Restart reconciler order.** Adopted: receipts → supervisor launches (with inputs and tokens) → supervisor buffers (§2.5). Alternatives: inputs in a separate job after launches (a crash between them leaves exited launches with inputs still `sending`: the next restart fixes it, but one job is simpler); buffers before launches (rejected by daemon-lifecycle §2.14: the launches step decides which launches can no longer bind).

**SD8. Exit hook points for 10b.** Adopted: the ordered `ExitStep` list inside the one exit job, assembled before step 9, with 10a1's input sweep after it (§2.11). Alternatives: events 10b subscribes to after commit (a gap between `launch_exited` and 10b's state, which mvp CL-6 forbids for inputs); 10b writing its own exit job (two exit records to order); steps registered on the `Supervisor` at `build` (step 11 comes after the reconcilers, so restart would miss them).

**SD9. The positional input without 10b.** Adopted: created `sending` with the reservation; `sent` with `sent_after_seq` = `launch_started`'s seq in the start job, with `reference` as its writer anchor; `rejected(spawn_failed)` when no process ran; `unconfirmed` on `StartedThenFailed`, exit, or restart. 10a1 has no ack, so a launched positional input ends `unconfirmed` at exit until 10b attributes its ack (mvp §2.3). Rejected: leaving it `sending` until 10b (mvp CL-6 forbids an input still in flight after exit).

**SD10. Sessions at shutdown. Public.** Adopted: one admission point closes process creation; every created session is handed from its `CreationGuard` to its slot or to its own op's close; the participant closes every published session in parallel against one cutoff (`end`), records what it received, and a final check holds the instance lock while anything it could not close may still run (§2.9). Rejected: leaving every session to process exit (every graceful stop would leave launches unconfirmed until 10a2); killing sequentially (the share is 2 s for all sessions); closing only the sessions published when the participant starts (a creation finishing later would start a session no one closes).

**SD11. Reservation inside the op, slot first.** Adopted: both spawn and resume reserve in the op, after inserting the locked slot; a refusal or rollback removes it. Rejected: resume reserving in its intent (a stop arriving between the intent's commit and the slot's insertion would find a `starting` launch with no lock to wait on); reading `.git` files instead of running git (re-implements git's discovery, which `workspace_id`'s definition names, mvp §2.1:103).

**SD12. Session access.** Adopted: one async op lock holding `Option<Session>` per launch (§2.11), with the writer's position read through a `WriterObserver` outside it and without waiting on the intake path. Rejected: `Arc<Session>` clones (no lock wait, but `close` consumes the `Session`, so the closer would wait for every clone to drop: cross-call state and a second wait); reading positions under `ops` (an early hook would meet the spawn op's lock and see nothing); waiting on the map or queue lock in `observe` (a descheduled holder would stall every launch's hook intake behind PR 9's intake mutex).

**SD13. Env scrub by name. Public.** mvp §2.7 lists the prefixes `CLAUDE_CODE_*` and "other `CODEX_*` session variables". Both prefixes also carry user configuration (Claude Code's settings page lists `CLAUDE_CODE_*` variables that select the provider and behaviour; `CODEX_HOME` selects Codex's home); dropping them changes how the user's CLI runs, against mvp P-1 (the CLI runs with the user's own configuration). Adopted: the named identity variables of prior-art row 6c plus `CODEX_SESSION_ID` and every `CHALDEA_*`, compared as the backend compares keys; a new identity variable is a list entry (PR 13/14 per CLI version).

**SD14. Hook configuration stays PR 13's and PR 14's.** 10a1 supplies the token, the env, and the Claude session flag; which events each CLI reports is the adapters' (mvp §4 rows 13, 14; Codex's trust hash makes the event list part of the first release, mvp §2.7). Consequence: a real CLI launched by 10a1 alone never binds, and real same-session resume is PR 13's and PR 14's acceptance (L-4).

**SD15. No spike watchdog (F2).** The emergency kill is `chaldea stop` (and the Rescue button, PR 20, which calls it). A watchdog killing on CPU or time would decide orchestration in daemon code (invariant 2).

**SD16. `chaldea wait`, and the CLI's reads. Public.** `chaldea wait <thread> [--until exited|idle|attention] [--timeout <dur>]` (default `exited`, 10 min). One cutoff, taken at start, is passed to `poll_until_with` and to every request's connect and read (`client::request` takes it as `deadline`). Each poll reads `GET /api/threads/{thread_id}`: the condition holds on its `ThreadInfo` → print the `ThreadResponse` as JSON, exit 0 (level-triggered: a condition already true returns at once). Discovery `NotRunning`, `Unknown`, `Starting`, or `Stopping`, a refused connection, a reset or EOF before a full response, 408, or 503 → keep polling; 404 → exit 1, "no such thread or not readable"; 401 or 403 → exit 1; any other status or a body that does not decode → exit 1 with the reason; past the cutoff → exit 4 (`ExitCode::Timeout`; 3 is `AlreadyRunning`). A response read after the cutoff is not adopted; discovery reads files and looks up a process synchronously and cannot be cut short, so the exit follows the cutoff by at most that one read. `chaldea resume` and `chaldea close` read the same route once, send its current launch as `expected_launch_id` with a new `command_id`, keep both on every transport retry of that request, and exit 1 on `launch_changed` without sending again. Alternative: an edge-triggered wait on the event stream (`--after <seq>`), left for 10b's `send` then `wait` pattern.

**SD17. Launch operations as supervisor tasks.** Adopted: rule 2 of §2.1, with `EffectOutcome` telling the runner how an op ended (§2.3). Rejected: running spawn, stop, and close inside the runner's effect future (an abort at the drain deadline would drop the future while its blocking call runs on, leaving a `Session` or a close result with no owner); leaving the effect pending when a job cannot be recorded (the runner and the HTTP drain would wait for it until their deadlines); one "no result" value for both an unrecordable job and a shutdown (a graceful stop would report a fatal and end as `StoreFatal`).

**SD18. Stop before write.** Adopted: `Session::seal` before the `StopRequested` job, its zero-byte records passed to the exit job (§2.11). Rejected: committing `StopRequested` and relying on close's step 1 (a unit whose recheck passed could be permitted after the state became `stopping`); a supervisor flag read by 10b's recheck (the recheck runs outside the queue lock, before the dispatch boundary, pty-runtime §2.3); relying on the completion consumer alone (the exit job can commit before the consumer's job, and the sweep would make a zero-byte input `unconfirmed`).

## 4. Obligation Inventory

Obligation ids number the PR 10 obligations collected from the sources in the second column; 10a1, 10a2, and 10b share them.

| Id | Source | 10a1's answer |
|---|---|---|
| A1 | mvp:439; pty-unix:409 | PR 10a2: the restart scan of old processes (§2.12; SD3). 10a1 records `daemon_restart` (§2.5; PR10a1-CL-16) |
| A2 | mvp:439 | §2.5 `unknown_after_crash`, §2.4 refusals; SD2; PR10a1-CL-8. The owner's assertion: PR 10a2 |
| A3 | mvp:439 | §2.4 "`extra` and its source"; PR10a1-CL-10 |
| A4 | mvp:439 | §2.3 Spawn and resume, Close; PR10a1-CL-4, PR10a1-CL-25 |
| A5 | mvp:439; mvp CL-18 | §2.3 step 3 (check and carry-over); PR10a1-CL-3 (removal race: PR 16) |
| A6 | mvp:439; mvp CL-8 | own commands: PR10a1-CL-5 |
| A13 | mvp:439; mvp CL-6 | §2.5, §2.11 exit steps; PR10a1-CL-14 |
| A21 | mvp:439 | §2.5; PR10a1-CL-8 |
| A22 | mvp:439; hook-ingest:381,565 | §2.5, §2.6, §2.11; PR10a1-CL-15 |
| A24 | mvp:439; mvp CL-1 | §6 FakeCli `banner`; PR10a1-CL-18 |
| A25 | mvp:439; daemon-lifecycle §2.11 | §2.3 Natural exit, §2.9; PR10a1-CL-13, PR10a1-CL-19, PR10a1-CL-36 |
| A26 | mvp:439; pty-windows:202 | §2.1 rule 5, §2.4; PR10a1-CL-10 |
| A27 | mvp:439 | §2.3 Stop, §2.4; PR10a1-CL-10 |
| A28 | mvp:439; pty-windows:412 | PR 10a2: the Windows old-tree check after a restart (§2.12) |
| A29 | mvp:439; pty-windows:129,465 | §2.4 "A29 on Windows", SD2: recorded `unknown_after_crash`, fallback kept. The finder's evidence: PR 10a2 |
| A30 | mvp:439; formal-specs §3 hole a | §2.3 step 3; PR10a1-CL-3; §7 |
| A32 | mvp:439; formal-specs §3 hole c | §2.6; PR10a1-CL-17; §7 |
| A35 (anchor part) | mvp:439; formal-specs §3 | §2.3 step 5, §2.11 Observation; PR10a1-CL-30 (the ack decision: 10b) |
| B1 | mvp:437; hook-ingest D10 | §2.4; SD6; PR10a1-CL-10 |
| C1 | input-encoders:331,179 | §2.2 Prompt admission, §2.7; PR10a1-CL-1, PR10a1-CL-2 |
| C6 | input-encoders:319 and its D3; http-surface:461,471 | §2.8; SD5; PR10a1-CL-24 |
| C7 | input-encoders:114 | §2.8; PR10a1-CL-2 |
| D2 | hook-ingest:565,352 | §2.7, §2.3; PR10a1-CL-6, PR10a1-CL-7, PR10a1-CL-31 |
| D4 | hook-ingest:268,565 | §2.6; PR10a1-CL-17; §7 |
| E1 | http-surface:582,703 | §2.2 (`spawn`, `resume`, `stop`, `close`); PR10a1-CL-21, PR10a1-CL-35 |
| E2 | http-surface:505-531 | §2.2 codes, §2.3 "Requests and ops"; PR10a1-CL-21, PR10a1-CL-22, PR10a1-CL-32 |
| E3 | http-surface:337; daemon-lifecycle:622 | §2.5; SD7; PR10a1-CL-16 |
| E4, X1 | http-surface:333; `commands.rs:665` | SD1; PR10a1-CL-20 |
| E5 | daemon-lifecycle:625-628 | §2.9; SD10, SD17; PR10a1-CL-22, PR10a1-CL-33, PR10a1-CL-36 |
| E6 | daemon-lifecycle PR8-P-3b | PR10a1-CL-23 |
| F1 | pty-unix:409 | PR 10a2: the identity-only kill after a restart (§2.12; SD3) |
| F2 | pty-unix:417 | SD15; PR10a1-CL-11 (the emergency kill is `stop`) |
| X5 | `state.rs:255-258` | §2.3 Stop; SD4, SD11; PR10a1-CL-12 |
| A38 | formal-specs:192 | own guards: §7; PR10a1-CL-27 |

**Handed on.** A7–A12, A14–A20, A23, A31, A33, A34, A35's attribution, A36, A37, B2, B3, C2–C5, C8 for `send`, D1, D3, F3–F5, X2–X4: PR 10b (the send and ack path; 10a1 defines `prompt_too_large`, the observation, the anchor, and the stop-before-write order it uses). A1's restart scan, F1, A28, A29's finder evidence, A2's owner assertion: PR 10a2, because they decide what happens to processes the daemon no longer owns, under the owner decision and mvp P-18's survivor policy (§2.12). A5's removal race: PR 16 (it owns removal; 10a1's reservation check and carry-over are the reading half). Hook configuration per CLI and real same-session resume: PR 13, PR 14 (SD14). A29's orphan finder: PR 21.

## 5. Purpose Layer and Plan Layer

### 5.1 Purpose Layer: Acceptance Criteria

10a1's acceptance covers native launch, stop, close, shutdown, and restart records with real CLIs on macOS, and binding and resume with FakeCli fixtures on both OSes; Windows is covered by CI with FakeCli (`windows-latest`). mvp P-18 is not met until PR 10a2 (L-2). Real same-session resume, mvp P-18's "a resume shall continue the session", is an acceptance gate of PR 13 and PR 14 (L-4); a FakeCli success does not count for it. Evidence of real runs records variable names, never token values.

| ID | Criterion (EARS) | Source | Scene | Observable result / failure | How to check on the real CLI |
|---|---|---|---|---|---|
| PR10a1-P-1 | When the owner runs `chaldea spawn --agent <claude\|codex> [--prompt <text>]`, the CLI shall run in a daemon-owned PTY with this launch's hook token, thread id, and daemon URL in its environment, the identity variables scrubbed, and a Codex prompt as one argument after `--`. | mvp §2.5, §2.7, §2.9; mvp P-1, P-2 | fresh thread, trusted repository | Pass: `SpawnResult.outcome = started`; thread `running`; argv and env as stated. Fail: missing or extra variables; prompt split or before `--`. | `ps -o pid,sid,args -p <pid>`; `ps eww -p <pid> \| tr ' ' '\n' \| cut -d= -f1` lists `CHALDEA_HOOK_TOKEN` and no `CLAUDECODE` (names only); the Codex TUI shows the prompt submitted. |
| PR10a1-P-2 | When `chaldea stop` is run for a running thread, no process of its tree shall be alive at the command's reply, the launch shall be `exited` (`stopped`) with death confirmed, and an unrelated process shall survive. | mvp P-3; pty-unix PR3-P-1 | a CLI with a child process (an MCP server, or a shell command it started) | Pass: `death_confirmed: true`, `pgrep -s <sid>` empty, a sleeper started from another terminal alive. Fail: survivor; unrelated killed. | As stated; Codex and Claude each. |
| PR10a1-P-3 | When the CLI exits by itself, the launch shall be `exited` (`exit`) with its code, and death confirmed when the close proves it. | mvp §2.9 | a CLI that exits (`/exit` through PR 11's attach, or a prompt that ends it) | Pass: one `launch_exited`; `code` set. Fail: two exits; `stopped`. | `chaldea wait <thread> --until exited` returns the thread JSON. |
| PR10a1-P-4 | If the daemon is killed and restarted, then before the first command is served every open launch shall be `exited` (`daemon_restart` or `unknown_after_crash`) with death unconfirmed, every in-flight input recovered, and every launch token revoked; resume and close of those threads shall answer `death_unconfirmed`, and a new spawn shall start. | mvp §2.9; rescope to 10a2 | kill the daemon by PID mid-run | Pass: the first read after restart shows `exited`; resume and close refused with `death_unconfirmed`; a new thread runs. Fail: a `running` thread after restart; a resume accepted. | `kill -9 <daemon pid>`; restart; `chaldea wait <thread> --until exited --timeout 1s`; `chaldea resume`; `chaldea spawn`. |
| PR10a1-P-5 | If a resume is requested while the current launch's death is unconfirmed, the thread is closed, the expected launch is not current, or the worktree is not `active`/`released`, then no launch shall be reserved and the code shall name the reason; a `death_unconfirmed` refusal shall raise `resume_blocked`. | mvp §2.9; mvp CL-18 | each condition | Pass: 409 with the code; no `launch_reserved`. Fail: a reservation. | FakeCli (worktree rows written directly until PR 16). |
| PR10a1-P-6 | When the program cannot be started, the thread shall show `exited` (`spawn_failed`), and a positional prompt shall be `rejected` (`spawn_failed`) when the program never ran, or `unconfirmed` when it may have. | mvp §2.3:186 | `PATH` without `codex` | Pass: as stated. Fail: input `sending` forever; a thread `running`. | `PATH=/usr/bin <absolute path to chaldea> daemon run`, then `chaldea spawn --agent codex --prompt hi`. |
| PR10a1-P-7 | When a client reads `GET /api/adapters`, it shall receive this host's rows of the capability table. | mvp §2.7 | owner token | Pass: equal to `CAPABILITIES` for the host. Fail: other host's rows; missing agent. | `curl -H "Authorization: Bearer $T" http://127.0.0.1:$P/api/adapters`. |
| PR10a1-P-8 | While a non-manual launch is unbound 20 s after `launch_started`, the thread shall carry `session_start_timeout` until it binds or exits. | mvp §2.2:142; hook-ingest:268 | a CLI that never sends `SessionStart` (every real CLI until PR 13/14) | Pass: the cause after 20 s, cleared at exit. Fail: absent; kept after exit. | Spawn Claude; read the thread after 21 s; stop it. |
| PR10a1-P-9 | When the owner closes a thread whose launch exited with death confirmed (or with `--stop`), the thread shall be closed, its tokens revoked, and later resumes refused. | mvp §2.11 | idle thread | Pass: `closed: true`; resume → `thread_closed`. Fail: a resume succeeds. | `chaldea close <thread> --stop`; `chaldea resume <thread>`. |
| PR10a1-P-10 | No text a CLI prints shall change thread state. | invariant 1; mvp CL-1 | FakeCli `banner` | Pass: no event beyond those from hooks and the process. Fail: any state change from output. | Not a real-CLI check (PR10a1-CL-18). |
| PR10a1-P-11 | When the owner stops the daemon gracefully, each session shall end in one of §2.9's classes, and no session's process created by this daemon shall be left unclosed while the instance lock is released. | SD10 | two running threads, one of them a tree that ignores HUP and TERM | Pass: each recorded per §2.9's classes; none left `running`. Fail: a session recorded `stopped` without its close; the lock released with a session unclosed. | `chaldea daemon stop` with a Claude thread running; restart; read threads. |

### 5.2 Plan Layer: Claims

Tests named `supervisor::…` are daemon unit tests under `crates/daemon/src/supervisor/tests/` with `FakePty`, the in-process store, the HTTP server, and `cfg(test)` pause points (the `crates/cli/tests/hook_e2e.rs` and lifecycle-test patterns); `supervisor_e2e::…` are in `crates/cli/tests/supervisor_e2e.rs` with the native runtime and FakeCli; `launch::…` are `core` unit tests.

| ID | Claim (EARS) | Source | Test that falsifies it | Mutation it kills |
|---|---|---|---|---|
| PR10a1-CL-1 | When `spawn` or `resume` carries a prompt, `prepare` shall refuse a Claude prompt (`bad_args`), a text over `MAX_PROMPT_BYTES` (`prompt_too_large`), and any text `check(Codex, Positional, Send, …)` rejects (`prompt_rejected`) before any receipt or event; an accepted prompt shall be the argv's last element, after a `--`; a `cwd` that is missing or not a directory shall be a stored 400 `bad_args` with no reservation, token, event, or spawn. | C1; input-encoders D5; §2.2 | `supervisor::commands::prompt_admission` (each prompt case: status, code, no receipt row, the host and thread heads unchanged, `FakePty::spawns()` empty; accepted: `spawns()[0].argv` ends with `["--", text]`); `supervisor::commands::bad_cwd` (a file and a missing path: 400, a receipt with that reply, no `thread_registered`, no token row, `spawns()` empty; the directory then created and the same `command_id` replayed: the same 400); `launch::codex_argv` (prompts `--version`, `-c x`, text with spaces and LF) | `check` skipped or run after the reservation; the prompt split or placed before `--`; the size check after `check`; a bad `cwd` reserved |
| PR10a1-CL-2 | The capability row and every OS-dependent argument shall come from `(agent, host_os)` with `host_os` mapped once from `std::env::consts::OS`; an unmapped OS shall stop `daemon run` before startup. | C7 | `adapter::host_os_from_str` (`macos`, `windows`, `linux`, empty); `daemon_cmd::tests::unmapped_os_refuses` (`daemon_cmd::run` with `Seams.host_os = "linux"`: exit `Failed`, no state directory files created); `supervisor::launch::windows_row` (a `Supervisor` built with `HostOs::Windows` on any runner: Codex argv has `-c tui.disable_paste_burst=true`) | `cfg!(windows)` instead of the mapped value; Macos hard-coded; unmapped OS accepted |
| PR10a1-CL-3 | The reservation shall succeed only against the expected current launch, on a thread not closed, after the previous launch exited with death confirmed, and with its worktree `active` or `released`, carrying the previous launch's `worktree_id`; each refusal shall write nothing except the `resume_blocked` raise of `death_unconfirmed`. | A30, A5; mvp CL-18; mvp §2.9 | `supervisor::reserve::refusals_and_carry_over` (one condition at a time; worktree rows `creating`, `removing`, `uncertain`, `removed`, `failed` refused and `active`, `released` accepted, written directly; on success `launches_using_worktree(W)` returns the new launch); trace `trace_hole_a_reserve_without_death` replays unchanged | death check dropped (hole a); `creating` accepted; `released` refused; `worktree_id` not carried; `expected_launch_id` ignored; `resume_blocked` not raised or raised twice |
| PR10a1-CL-4 | Of a `resume` and a `close` expecting the same launch, at most one shall succeed, and the other shall answer `thread_closed`, `launch_changed`, or `launch_live`. | A4 | `supervisor::race::resume_close` (both writer orders, both concurrent through a parked writer, and a resumed launch that exits and is confirmed before the close runs; asserts exactly one success and the final state) | close not checking `expected_launch_id`; resume not re-reading `closed` |
| PR10a1-CL-5 | Two concurrent requests with one `command_id` to `spawn`, `resume`, `stop`, or `close` shall cause one effect. | A6; mvp CL-8 | `supervisor::commands::duplicates_write_once` (per command, two requests released together; counts the verb's own effect: `spawns()` and `launch_reserved` for spawn and resume, `StopRequested` and `FakeEvent::Kill` for stop, `thread_closed` for close) | the op started per request; a receipt bypass |
| PR10a1-CL-6 | The reservation job shall insert exactly one hook token for the launch, and the spawn shall carry `CHALDEA_URL`, `CHALDEA_HOOK_URL`, `CHALDEA_HOOK_TOKEN` (that token), `CHALDEA_THREAD_ID`, none of `ENV_SCRUB` under the host's comparison, the user's other variables, and for Claude `--session-id <launch_id>` on an unbound thread or `--resume <session_id>` on a bound one, equal to `expected_session` for that launch. | D2; mvp §2.5:259; hook-ingest §2.8 | `supervisor::launch::env_and_session_args` (two generations: a fresh launch, then a resume after binding; daemon env seeded with every scrubbed name, `Chaldea_Hook_Token` and `claudecode` in mixed case, `CODEX_HOME`, `CLAUDE_CODE_USE_BEDROCK`; asserts `spawns()[i].env` and argv; each token's hash equals its row and differs between generations; `expected_session` equals the argv value); on Windows the mixed-case names are gone and the spawn succeeds | token per thread; scrub by prefix (user variables lost); case-sensitive scrub on Windows (duplicate key or leak); URL missing; thread id as session id; resume passing the launch id |
| PR10a1-CL-7 | The start job shall record the backend identity and, in the same job, move the positional input to `sent` with `sent_after_seq` = `launch_started`'s seq; a hook that arrived before it shall be decided only after that commit. | D2; mvp §2.3:186 | `supervisor::launch::started_job` (one job's events in order; one commit notification); `supervisor_e2e::early_hook_waits_for_started` (FakeCli `hooks` emits `SessionStart` at once; a pause point holds the op before the start job; the test observes the hook inside the pid wait, then releases; the hook's reply follows the commit, with binding; the pid wait's bound comes from a clock seam, not a race against 1 s) | input moved in a later job; `sent_after_seq` of `launch_reserved`; identity from the request instead of `Session::identity` |
| PR10a1-CL-8 | If the daemon crashes between `launch_reserved` and `launch_started`, then restart shall record `unknown_after_crash` with death unconfirmed and the positional input `unconfirmed` with `input_unconfirmed`, and `stop`, `resume`, and `close` shall answer `death_unconfirmed`. | A2, A21 | `supervisor::restart::crash_before_started` (store rows of a reservation, the reconcilers; `stop`, `resume`, `close` → `death_unconfirmed`, `resume_blocked` raised once); `supervisor_e2e::crash_after_child_created` (a helper process runs `daemon run` with `Seams.components` holding a pause after `Session::spawn` returned; the test kills it by PID, restarts, and finds `unknown_after_crash` while the real child was created) | `daemon_restart` for a launch without pid; input left `sending`; a stop that answers `already_exited` or kills |
| PR10a1-CL-9 | If `Session::spawn` returns `NeverStarted`, the launch shall exit `spawn_failed` with death confirmed and the positional input `rejected(spawn_failed)`; if `StartedThenFailed`, `spawn_failed` with death confirmed exactly when cleanup is `Confirmed`, and the input `unconfirmed`. | mvp §2.3:186; `backend.rs:185-201` | `supervisor::launch::spawn_errors` (`FakeScript { fail }` for each variant and each cleanup, `identity` present and `None`) | `StartedThenFailed` rejecting the input; `NeverStarted` leaving death unconfirmed; cleanup ignored |
| PR10a1-CL-10 | Every `Session::close` call shall receive the launch's full recorded list; death shall be confirmed only by a close whose join returned in time with `tree_dead`; a dropped `Session` shall confirm nothing. | A3, A26, A27, B1 | `supervisor::stop::full_list_every_call` (a test seam records one proven identity on the `LiveLaunch`; every `FakeEvent::Kill { extra }` equals it, in stop, exit, shutdown, and late-creation closes; `KillBehavior::Ignore` with short bounds from `SupervisorConfig`: `death_confirmed` false); `supervisor::stop::late_join_unconfirmed` (the close's join made `Late` by a controlled clock: unconfirmed despite `tree_dead`); `supervisor::stop::drop_never_confirms` | survivors passed back; `extra` dropped on one path; a `Late` result counted; a drop path recording death |
| PR10a1-CL-11 | When a running launch is stopped, the supervisor shall seal, record `StopRequested`, close explicitly, and record one `launch_exited` (`stopped`) with `code` and `signal` from `CloseOutcome.exit` (null when the waiter had not reported by the join), `death_confirmed` from the close, and `launch_death_confirmed` with the kill's method when confirmed. | mvp P-3; pty-unix §4 | `supervisor::stop::records_one_exit` (FakePty; the close parked so the exit op's event arrives first: one `launch_exited`, from the stop; `FakeChild::exit` before the join: its code and signal; not before: null); `supervisor_e2e::stop_kills_tree` (FakeCli `spawn-tree` with a sleeper grandchild living far beyond the test, and an unrelated sleeper; after the reply every tree pid is gone by `process::lookup` and the unrelated one lives; on macOS the leader's exit shows a signal, on Windows an exit code and no signal) | exit recorded before close; two `launch_exited`; method dropped; exit status from a `status()` read before close; unrelated process killed |
| PR10a1-CL-12 | While a launch is `starting`, a stop shall wait for the spawn outcome on the op lock and then stop a started launch or answer `already_exited`; if the lock is not taken in time (`Elapsed` or `Late`), it shall answer `launch_busy` and never stop later. | X5; `state.rs:255-258` | `supervisor::stop::while_starting` (pause points between slot insertion and the reservation commit, and after the commit before `Session::spawn`: a stop in the first window sees the previous launch, in the second waits; release → stopped; spawn failure → `already_exited`; a controlled clock makes the lock `Elapsed` and `Late` → 504, and after release no `StopRequested`) | `StopRequested` fed while `starting`; a missing slot in the commit window; a `Late` guard used; a 504 followed by a later stop |
| PR10a1-CL-13 | When the child exits by itself, the supervisor shall record one `launch_exited` (`exit`) after an explicit close, with the callback's `ChildExit`; after `WaitFailed`, `code` and `signal` shall be null and death shall be the close's result. | mvp §2.9; A25 | `supervisor::exit::natural_exit` (`FakeChild::exit` with code and signal); `supervisor::exit::wait_failed` (`FakeChild::fail_wait`: null status; `KillBehavior::ExitOnKill` → confirmed, `Ignore` → not) | `WaitFailed` treated as proof or as refutation; exit recorded without close; code dropped |
| PR10a1-CL-14 | The exit job shall move the thread's inputs still in `queued`, `sending`, or `sent` of the launch or of no launch after the registered exit steps, and the restart job those of every thread, keeping `unconfirmed` inputs and their attention; a second restart shall change nothing. | mvp CL-6; A13 | `supervisor::exit::inputs_recovered` and `supervisor::restart::inputs_recovered` (inputs written directly in each state, of the launch, of no launch, and an earlier launch's `unconfirmed`; a test `ExitStep` that settles one input first: the sweep leaves it; then a second restart: no event) | inputs moved in a later job; `unconfirmed` stepped (job fails); the unassigned queue left; the sweep before the registered steps; restart sweeping only open launches' threads |
| PR10a1-CL-15 | `buffer::resolve_unbindable` shall run in every exit job and at restart for every `buffered_launches()`; `buffer::expire` shall run on the tick for live unbound launches. | A22; hook-ingest §2.9 | `supervisor::exit::buffer_resolved`; `supervisor::restart::buffers_resolved` (a buffer left by a launch exited in an earlier run); `supervisor::tick::expires_buffer` (the tick's instant and the stored stamps both controlled) | resolve in a later job; restart skipping exited launches; tick not calling `expire` |
| PR10a1-CL-16 | The restart reconcilers shall run in the order receipts, supervisor launches, supervisor buffers, before the listener exists, and shall signal no old process. | E3; daemon-lifecycle §2.14; SD3 | `supervisor::restart::reconciler_order` (names); `supervisor_e2e::restart_records_before_serving` (the buffers job parked: the port is not served until it is released; the first read shows the launch `daemon_restart`, unconfirmed; a FakeCli leader that survived the old daemon is still alive after startup, then ended by the test's cleanup) | order swapped; reconciliation in a task after serving; a signal at startup |
| PR10a1-CL-17 | `session_start_timeout` shall be raised only for the current, started (`launch_started` committed), live, unbound, non-manual launch at least 20 s after its committed start, also after a `foreign_session` `SessionStart`; `startup_stall` only for an upgraded Codex launch at its start; neither for an exited, unstarted, or previous launch. | D4, A32; mvp §2.2 | `supervisor::tick::session_start_timeout` (`Supervisor::tick` at +19.999 s: none; +20 s: raised; bound: none; foreign `SessionStart` row: raised; exited: none; manual Codex: none; the tick's candidates taken, then the launch bound, exited, or replaced before its job: none; a published launch whose start job is held past 20 s: none, then raised 20 s after the commit); `supervisor::launch::startup_stall_only_upgraded_codex` | guard on "`SessionStart` seen"; the timer from publication; no re-validation in the job; manual Codex raised; Claude stall |
| PR10a1-CL-18 | No byte the CLI writes to its PTY shall produce an event or a state change. | A24; mvp CL-1 | `supervisor_e2e::banner_changes_nothing` (FakeCli `banner`; the test first sees its text in a VT snapshot, so the drain read it; events, with ids and stamps removed, equal those of a run with silent output) | any reader of PTY output feeding a reducer input |
| PR10a1-CL-19 | Every child the supervisor creates shall pass admission and be created through the backend or `spawn_locked_before`, waited for by exact pid, and its wait bounded; one git child shall exist at a time, never killed, and its exit status reaped by exact pid. | A25; daemon-lifecycle §2.11 | `cargo clippy` with the existing `Command::spawn` bans (`crates/daemon/clippy.toml`; `lints.rs::clippy_tomls_list_the_bans`); `supervisor::workspace::probe_bounded` (the test holds the spawn lock past the deadline: no git process, canonical cwd; a `SupervisorConfig` git that sleeps beyond 5 s: canonical cwd at 5 s, a second spawn meanwhile uses the canonical cwd without creating git, and after the child exits a tick's `try_wait` returns its status, after which a probe runs git again) | a direct `Command::spawn`; an unbounded wait for the spawn lock; a kill of git; a second git while one is left; a left child never waited for |
| PR10a1-CL-20 | Every committed job that revokes a token or ends a launch's agent-token validity shall nudge sockets through `after_ok` once it committed: close's intent, close `--stop`'s closing job, and the exit job. | E4, X1; http-surface §2.4 | `http::tests::commands::revoking_intent_nudges` (a test handler with `revokes()` true: one nudge after the commit is visible to a read, none before; false: none); `supervisor::close::nudges` (a nudge counter: one per committed revoking job, none for a rolled-back one) | runner ignoring `revokes()`; nudge before commit; exit job without nudge |
| PR10a1-CL-21 | The four handlers shall be registered, every new code classified in `handler_code` at one status, and no success type shall have a top-level `code`. | E1, E2 | `handler_code` exhaustive `match` (compile); `supervisor::commands::codes_and_statuses` (each refusal's status); `supervisor::commands::success_schemas_have_no_code` (each success type's JSON schema has no `code` property) | a code at two statuses; a success body with `code`; a handler left 501 |
| PR10a1-CL-22 | An op that cannot record a job while the daemon runs shall end `Unrecorded`: the runner shall leave the receipt `in_progress`, reply 503 `outcome_unknown` if the request still waits, and report `ReceiptUncompleted`; an op ended by shutdown shall end `ShuttingDown` with no fatal report; an exit op that cannot record within 60 s shall report `LaunchUnrecorded` unless shutdown started. | E2, E5; SD17 | `http::tests::commands::effect_outcomes` (a test effect: `Unrecorded` while the request waits → 503 at once, no completion attempt, the sink receives it; `Unrecorded` after the request's deadline → the client first got 409 `in_progress`, then the sink receives it; `ShuttingDown` → 503 `shutting_down`, no sink report, the receipt `in_progress`); `supervisor::launch::unrecorded_start_job` (a store fault fails the start job with `CommitUnknown`: 503 before the deadline; the reconcilers make the receipt `unknown_after_crash`); `supervisor::shutdown::closed_store_no_fatal` (a write refused `Closed` with admission closed: `ShuttingDown`, the coordinator's outcome not `StoreFatal`); `supervisor::exit::unrecorded_exit_job` | a pending effect; a completed receipt; a stored 500; a fatal report during shutdown |
| PR10a1-CL-23 | A retry of a `spawn` whose daemon died during its op shall answer 409 `unknown_after_crash`, and the op shall not run again. | E6; daemon-lifecycle PR8-P-3b | `supervisor_e2e::retry_after_crash_is_unknown` (a helper process runs `daemon run` with `Seams.components` holding a pause after the reservation commit; the test kills it by PID, restarts, retries with the same `command_id`; one `launch_reserved`) | receipts reconciler skipped; op re-run on retry |
| PR10a1-CL-24 | `GET /api/adapters` shall return exactly this host's `CAPABILITIES` rows in `AdaptersResponse`, behind the route's guard. | C6 | `supervisor::adapters::equals_table` (each `HostOs`, compared with `CAPABILITIES` filtered by host); `supervisor::adapters::guarded` (wrong `Host` → 421, no token → 401, cross `Origin` → 403, a query string as the other read routes answer it); contract drift test. That the handler maps `CAPABILITIES` rather than a list of its own is a review rule. | other OS's rows; `submit_delay` exposed; guard bypassed |
| PR10a1-CL-25 | `close` shall commit `thread_closed` and revoke the thread's tokens only when the expected current launch exited with death confirmed; `--stop` shall close only the launch it stopped. | mvp §2.11 | `supervisor::close::cases` (live → `launch_live`; unconfirmed → `death_unconfirmed`; a `daemon_restart` launch with `--stop` → `death_unconfirmed`, no `FakeEvent::Kill`; a stale `expected_launch_id` with and without `--stop` → `launch_changed` before any op; a resume after the stop → `launch_changed`; on success the thread's hook token fails `verify_hash_in`) | close with a live launch; tokens kept; closing a newer launch |
| PR10a1-CL-26 | `chaldea wait` shall return the thread when the condition holds, exit 4 once its cutoff passed and adopt no response read after it, pass that cutoff to every request, read one thread whatever its history, and exit 1 on 404, 401, 403, other statuses, and undecodable bodies. | SD16 | `supervisor_e2e::wait_until_exited` (FakeCli exits; `wait` returns its JSON; an already-exited thread returns at once; a thread with history far beyond `client`'s body bound reads normally); `agent_cmd::tests::wait_cutoff` (a stub server answering after the cutoff and a clock seam: exit 4, the late body ignored; discovery `Starting` and `Stopping`, 408, 503, EOF → keeps polling; 401, 404, 500, a broken body → exit 1) | timeout ignored; exit 3; a request outliving the cutoff; a late success adopted; an already-true state missed; a 500 retried; a full-snapshot read |
| PR10a1-CL-27 | A guard 10a1 changes shall change `specs/thread_input.qnt` and its traces in the same PR. | A38; formal-specs §6 | CI `specs` job (`traces:check`) and `quint_traces.rs` replay for the model; §7's guard-to-test table, reviewed in the PR, for the daemon guards the core replay cannot see | a model guard changed without its trace; a daemon guard with no test in §7's table |
| PR10a1-CL-28 | When the git probe succeeds, a thread's `workspace_id` shall be derived from git's common directory, so a repository and its worktrees share it; otherwise from the canonical cwd, with the reason logged. | mvp §2.1:103 | `supervisor::workspace::ids` (a temp repository, a `git worktree add` of it, the repository reached through a symlinked path, and a plain directory) | cwd used inside git; per-worktree ids; the alias splitting the id |
| PR10a1-CL-29 | After a stop sealed the session, no unit shall be permitted; every unit not yet permitted at the seal shall be settled `Closed` with zero bytes before the `StopRequested` commit, and its zero-byte result shall be applied exactly once, by the exit job or by the completion consumer, whichever commits first. | SD18 | `supervisor::stop::seal_before_stop_requested` (a unit submitted through `with_session`, the writer parked at `ProbePoint::RecheckEntered`; the `StopRequested` job held by a store writer pause; while it is held the unit's completion is `NotStarted(Closed)` and `FakeChild::writes()` empty); `supervisor::stop::sealed_applied_once` (test stand-ins for 10b's exit step and completion consumer over one input: the consumer receives the completion and its job is held, the exit job commits with `ExitFacts.sealed` and settles the input, the consumer's job then finds it settled; and the reverse order) | `StopRequested` committed before the seal (with the commit held, the released writer writes); sealed records not passed to the exit job; a result applied twice |
| PR10a1-CL-30 | `Supervisor::observe` shall never wait and shall answer `Reserved` only before child creation, `Unobservable` from creation until publication and under contention, and `Started` with `reference` and the per-class positions after publication. | A35 (anchor part); 10b coupling | `supervisor::live::observe` (synchronous calls with `ops` held by the spawn op throughout: before `admit` `Reserved`; after `admit` and before publication `Unobservable`, also with a real FakeCli that answers a VT query during the spawn; between reference sampling and publication `Unobservable`; after publication `Started`, and after a dispatched `Input` unit its class position above `reference`; with the map mutex held by the test, and separately with the writer's queue lock held: `Unobservable` at once; a launch id of another thread through `live`: `None`) | `Reserved` after creation began; a blocking map or queue lock on the intake path; `reference` taken after the start commit; a thread-blind lookup |
| PR10a1-CL-31 | A hook token shall stay valid after its launch exits in the same daemon run and be refused after the restart or the thread's close; the owner token shall stay valid across the restart. | D2; hook-ingest §2.8 | `supervisor_e2e::late_hooks` (one launch's token sends a hook after exit: stored `raw_unknown`; a second, independent launch's token after a restart and a third's after a close: 401 and the thread head unchanged; in a control run without the revocation the same requests are accepted; the owner token reads after the restart) | restart revoking only open launches; exit revoking the token; `LaunchScoped` revoking the owner token |
| PR10a1-CL-32 | A launch operation shall run to its end when its runner is aborted, and own its `Session` and results. | SD17; E2 | `supervisor::ops::outlive_runner` (the runner aborted after the reservation commit: the op records `launch_started`, and a stop later finds the session) | the op running inside the runner's future |
| PR10a1-CL-33 | When the daemon stops gracefully, each live session shall end in one of §2.9's classes, all decided against `end`. | SD10 | `supervisor::shutdown::owned_close` (sessions and a controlled clock: `ExitOnKill` → `stopped` confirmed; `Ignore` → `stopped` unconfirmed; a close whose join a supervisor pause point holds past `close_end` → no record, and after the restart `daemon_restart` (started) or `unknown_after_crash` (published, start job held); the store writer paused past `end` → the participant returns by `end`, and after the restart the row is either `stopped` or open-then-`daemon_restart`; one session already `stopping` → its job has no `StopRequested` and commits; one published whose start job failed → `launch_started` and `stopped` in one job). `FakeScript.hang_control_close` is not used to hold a close: it returns `elapsed: None` at the control step's own bound. | sessions left to process exit; a join waited past `close_end`; `StopRequested` fed to a `stopping` launch; a result-not-received session recorded; a `starting` launch recorded without `launch_started` |
| PR10a1-CL-34 | When an agent token spawns, the thread shall be registered as a child of the token's thread with role `standard`; an owner token's spawn shall have no parent. | §2.2 Caller; `auth.rs:144` | `supervisor::commands::spawn_caller` (an agent token row built with `TokenService::new_token`, since PR 16 mints them: `thread_registered.parent_thread_id` is its thread; an owner spawn: none) | parent dropped; the owner's or the request's thread used as parent; role taken from the request |
| PR10a1-CL-35 | A `resume`, `stop`, or `close` for a thread outside the token's grant shall be refused by `deny_for` before any read of its launch, with nothing written and no op. | §2.2 Target scope | `supervisor::commands::target_scope` (an agent token whose `stop` of its own thread succeeds; against an unrelated thread: `stop`, `close`, `close --stop`, and `resume` each 404, no receipt beyond the refusal, no `FakeEvent::Kill`, the thread head unchanged) | the scope check after the generation read; a 409 that reveals the launch; a kill of an out-of-scope thread |
| PR10a1-CL-36 | Once admission closed, every process the supervisor created shall be closed by the participant or by its own op, or the participant shall return `LeftRunning`; the coordinator shall then keep the instance lock. | SD10; A25 | `supervisor::shutdown::admission` (an op paused after `admit` and before `Session::spawn` returns; released before admission closes: published and closed by the participant; released after admission closes but before `lock_by`, and again after `lock_by`: the op closes it itself and records `launch_started` and `stopped` by `end`, no `launch_started` without it; held past `end`: `LeftRunning`); `lifecycle` coordinator test (a participant returning `LeftRunning`, and one not done by `end`: exit with `LockDisposition::HeldUntilExit`) | a session started after admission closed; a guard dropped before the hand-over; `Clean` with a creation in flight; the lock released with a session unclosed |
| PR10a1-CL-37 | `chaldea resume` and `chaldea close` shall read the current launch once, send it as `expected_launch_id`, keep it on every retry of that request, and exit 1 on `launch_changed` without sending again. | §2.2; SD16 | `agent_cmd::tests::expected_launch_pinned` (a stub server: one `GET /api/threads/{thread_id}`, POSTs with one `command_id` and one id across a transport retry; `launch_changed` → exit 1, no further POST; a thread with long history reads normally) | the id re-read on retry; a new request after `launch_changed`; a full-snapshot read |

### 5.3 Accepted Limits

| ID | Limit | Decided by | Why it does not defeat the purpose |
|---|---|---|---|
| L-1 | Until PR 10a2, a launch whose death is unconfirmed (after a crash, an owned close that could not prove death, or an unresolved `StartedThenFailed`) keeps its thread from resume and close, and its worktree in use. | the owner decision (2026-11-25); §2.12 | history is intact and new threads spawn; the MVP is unreleased, and 10a2 lands before PR 15. |
| L-2 | mvp P-18 is not met until PR 10a2: after a crash the new daemon neither signals nor checks old processes. | the rescope of crash recovery (§2.12); SD3 | on macOS the PTY hangup at the old daemon's exit ends most CLIs, and on Windows the job's kill-on-close ends the tree; the launch reads unconfirmed, so nothing relaunches on its session. |
| L-3 | A Codex positional prompt longer than Windows' command-line limit fails as `spawn_failed`. | `CreateProcessW` `lpCommandLine` (32,767 characters) | the input ends `rejected` with zero bytes written, visible to the owner; PR 14 can refuse earlier. |
| L-4 | Until PR 13 and PR 14, a real CLI launched by 10a1 never binds: `session_start_timeout` appears and `resume` answers `no_session`. | SD14 | spawn, stop, close, restart, and their proofs work; binding and resume are tested with FakeCli `hooks`; real resume is PR 13/14's gate. |
| L-5 | After `chaldea daemon stop`, a session whose close result was not received by `close_end` reads `daemon_restart` (or `unknown_after_crash` if it had not started) at the next start, and so falls under L-1. | SD10 | its history is intact; the stop's share is 2 s for every session. |
| L-6 | When git is missing, cannot be created before its deadline, or is slower than 5 s, the canonical cwd is used, so worktrees of one repository can get different workspace ids; a git that never exits keeps the git slot, so later probes also use the canonical cwd, and a graceful stop then keeps the instance lock. | §2.3 step 1 | the reason is logged; the id groups threads and scopes PR 20's Workspace surfaces, and a split id narrows such a scope, never widens it; at most one git child exists. |

## 6. Tests

**Per claim**: §5.2's test column. Unit tests drive `FakePty` (`FakeScript`, `FakeChild::exit`, `FakeChild::fail_wait`, `FakeEvent::Kill`, `FakePty::spawns`) and the store directly, with `cfg(test)` pause points of the supervisor; integration tests start the store and `HttpServer` in-process with the native runtime and FakeCli, as `hook_e2e.rs` does; the restart and E6 tests run the daemon in a helper process (the test binary re-executes itself, daemon-lifecycle §7 "Helper-process tests") with `Seams.components` holding a pause, and kill it by PID. No release-binary variable or flag enables a pause.

**Locating FakeCli.** `supervisor_e2e.rs` lives in `crates/cli/tests` because its hooks need the `chaldea` emitter (`CARGO_BIN_EXE_chaldea`), and it finds `chaldea-fake-cli` as `hook_e2e.rs` does (`fake_cli()`, built by `cargo test --workspace`). pty-unix §2.8 calls that lookup a fallback; the alternative, a thin `[[bin]]` in the `cli` package, would ship a test binary with the CLI.

**Ports.** Native tests use the port the daemon published (`client::discover`), never a probed free port.

**FakeCli `banner` mode** (new, `crates/testkit/src/fake_cli/banner.rs`): prints a fixed corpus of text shaped like Claude and Codex trust prompts, `Hooks need review`, a JSON hook payload, and a prompt echo, then sleeps until `--max-life-secs`.

**Host speed.** Timers run through `Supervisor::tick(now)` and `SupervisorConfig.clock`; lock waits and joins are made `Elapsed` or `Late` by that clock; the windows between slot, reservation, admission, spawn, reference sampling, publication, and start are held by pause points; a close's join is held by a supervisor pause point on its blocking task and a store commit by a writer pause, each on its own; kill and close bounds are shortened through `SupervisorConfig` only where a test needs `KillBehavior::Ignore`; real-process tests decide pass or fail by `process::lookup` after the command's reply, never by elapsed time, and a reaped child is shown by the wait that returned its status (on macOS `Lookup::Gone` also covers a zombie).

**Windows CI.** Every `supervisor::` unit test, `launch::`, `adapter::`, and `supervisor_e2e::` run on `windows-latest` in the Cargo lane of `scripts/windows-tests.ps1` (nested jobs need no breakaway, pty-windows PR4-CL-27; `daemon run` is not a detached start).

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
3. **Hole a**: no guard change. `reserve` already requires `st.deathConfirmed` (`thread_input.qnt:791-793`); 10a1 makes the reservation job enforce it (PR10a1-CL-3). `trace_hole_a_reserve_without_death` stays as the statement of what `reduce` alone admits, and `reservedOnlyAfterDeath` is the checked invariant.
4. **Hole c**: no model change. `sessionTimer` and `stall` already require a live, current launch (`alive` includes a recorded start); 10a1's job-side guards implement them, including the committed start (PR10a1-CL-17). `trace_hole_c_stale_startup_cause` stays.
5. **Unchanged and mapped**: `stop` requires `Running` (X5's hold is outside the model: the stop is applied after the outcome); `dispatch` requires the recheck's gate to be open, which a `stopping` launch closes: 10a1's seal implements that order (SD18); `restart` uses `UnknownAfterCrash` exactly for a `Starting` launch, which in Rust is "no pid recorded", and recovers inputs other than `unconfirmed`; `confirmDeath` has no 10a1 site (PR 10a2's owner assertion).

**Guard-to-test table** (the daemon guards the core replay cannot see; reviewed with PR10a1-CL-27):

| Model guard | Daemon site | Test |
|---|---|---|
| `reserve`: exited, `deathConfirmed` | reservation job (§2.3 step 3) | PR10a1-CL-3 |
| `reserveRefused`: raise once | reservation job | PR10a1-CL-3 |
| `sessionTimer`, `stall`: alive, current, unbound | tick and start jobs (§2.6) | PR10a1-CL-17 |
| `stop`: `Running` | stop op step 2; participant job (§2.9 step 4) | PR10a1-CL-12, PR10a1-CL-33 |
| `restart`: reason by start state; inputs but `unconfirmed` | `supervisor_launches` | PR10a1-CL-8, PR10a1-CL-14 |

formal-specs §3's owner column for holes a and c becomes "PR 10a1", and §6's PR 10 bullet points here (§8).

## 8. Proposed Canonical-Doc Changes

Not applied here.

- **mvp §2.1, event table**: `launch_death_confirmed` also follows a `launch_exited { death_confirmed: true }` in the same job, to record its method.
- **mvp §2.2, attention table**: `session_start_timeout` "Raised on: no `session_bound` 20 s after `launch_started` of a non-manual launch (a `SessionStart` that does not bind, such as `foreign_session`, does not stop it)".
- **mvp §2.5:259 / §2.7 "Env scrub"**: the named list of §2.7 here instead of the `CLAUDE_CODE_*` and `CODEX_*` prefixes, compared as the host compares environment keys, with the reason of SD13.
- **mvp §2.9 "Daemon restart"**: tokens revoked are every launch's, not only the open ones'; the old-tree check and the confirmation after a crash are PR 10a2's (§2.12).
- **mvp §2.4 / §2.5**: the command args, success types, and codes of §2.2; the target-scope refusal before any read; `GET /api/threads/{thread_id}`; `chaldea wait` (SD16, exit 4 on timeout).
- **mvp §4, PR 9 / 10 bullet**: "PR 10" (hook-ingest D10 gives it to PR 10 entirely).
- **mvp §4, PR 10 bullet**: PR 10 is designed as 10a1 (this document), 10b, and 10a2, in the order 10a1 → 10b → 10a2, with 10a2 before PR 15; "restart scan of a launch's session by recorded sid with `launch.start_time ≤ start ≤ restart_now`" moves to 10a2 for its design; real same-session resume is PR 13's and PR 14's acceptance.
- **mvp §6.1 P-3**: the 5 s is the close's kill bound, counted from the close's start; a stop that first waits for a spawn in progress (up to 10 s) starts it later.
- **http-surface §2.4 "Revocation nudges"**: command intents that revoke set `CommandHandler::revokes`; the runner passes the nudge; components get it as `RevocationNudge` at `attach`. **§2.8**: the `revokes` method in the trait listing; `CommandEffect::run` resolves to `EffectOutcome` (`Done`, `Unrecorded`, `ShuttingDown`) and the runner's handling of each. **"Handler replies"**: the nine codes of §2.2. **§2.7**: `GET /api/adapters` → `AdaptersResponse` (SD5); `GET /api/threads/{thread_id}` → `ThreadResponse`, authorized as the thread's stream read. **§4 PR 10 row**: split 10a1 / 10b / 10a2; `write_with`, `mint_token`, and the completion-policy constants as the seams for components, re-exported from `http`.
- **hook-ingest §2.8**: restart revokes every launch token. **§4 PR 10 row**: `extra` is empty in the MVP (§2.4); the restart buffer pass is PR 10a1's second reconciler; `IntakeProbe` reads `Supervisor::observe`, which never waits.
- **daemon-lifecycle §2.14**: reconcilers "receipts → supervisor launches → supervisor buffers", the last implemented by PR 10a1 (PR 9 shipped none); `ShutdownParticipant::shutdown` resolves to `ParticipantEnd`, and the lock rule (§2.8 step 6) adds "a participant left work running" and "a participant not done by its end"; participant row: "admission closed, owned close of live sessions against one cutoff, a final check (SD10)"; `spawn_locked_before` beside `spawn_locked`; `chaldea daemon run`'s default components are `SupervisorComponents`.
- **deadlines.md**: production exceptions gain `supervisor::tick_timer` (14); `commands::backoff_until` is shared with the supervisor; the `Session::spawn` wait without a deadline and its reason (§2.10).
- **pty-runtime.md §2.2 and §4**: `Session::seal` (its completions run before it returns, and it returns the settled records), `Session::observer` and `WriterObserver` (with the non-waiting read), `CloseOutcome.exit`, `env_key_eq`, `env_key_has_prefix`, and `with_spawn_lock_before` as PR 10a1 additions. **pty-unix.md §4**: the "Spike watchdog and emergency kill" row: none, `stop` (SD15).
- **formal-specs §1** (`sessionTimer` row: "unbound" instead of "no `SessionStart`"), **§3** (owner "PR 10a1" for holes a and c), **§6** (PR 10's bullet split into 10a1, 10b, and 10a2; the guard-to-test table of §7 here).
- **AGENTS.md**: none.
