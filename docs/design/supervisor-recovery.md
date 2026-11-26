# Design: Supervisor, Crash Recovery (MVP PR 10a2)

Status: design. Scope is the crash-recovery part of the PR 10 row of `docs/mvp-design.md` §4 (cited as "mvp §N"), which `docs/design/supervisor.md` (cited as "sup §N") hands over in its §2.12: the restart prober, `launch_evidence`, `chaldea stop --assume-dead` and its preview, the A29 evidence for PR 21's finder, the confirmation of every launch 10a1 leaves unconfirmed, and mvp P-18's survivor policy. It builds on 10a1 and 10b as merged; `docs/design/supervisor-send.md` (cited as "send §N") changes nothing here, since its exit step already runs at restart (send §2.3 "After exit").

**Owner decision (2026-11-25), implemented here.** After a daemon crash, a check that covers only a launch's recorded identities does not unlock resume, close, or worktree release. A launch's death is confirmed only by the owner, with `chaldea stop <thread> --assume-dead --launch <id>` after reading the launch's evidence.

**Open for the owner: P-18's survivor policy** (RD9). This document recommends option (b). Every other part holds under each option; RD9's table names the parts that change per option.

Sources read for this document:

| Source | Revision | Read for |
|---|---|---|
| This repository | `1d4e124` | `AGENTS.md`; `docs/architecture.md`; `docs/mvp-design.md` §2.1, §2.9, §4, §6.1; `docs/design/{supervisor,supervisor-send,pty-runtime,pty-unix,pty-windows,daemon-lifecycle,deadlines,event-store,http-surface,formal-specs}.md`; `docs/spikes/pty-hooks-spike.md` §4.6; `crates/daemon/src/supervisor/{mod,components,commands,ops,exit,restart,record,events,tick}.rs`, `crates/daemon/src/lifecycle/{mod,startup}.rs`, `crates/daemon/src/store/{readers,rows,write_tx,project}.rs`, `crates/daemon/src/deadline.rs`; `crates/core/src/{payload,command,event,sync,auth,http,state}.rs`; `crates/pty-runtime/src/{backend,process,fake}.rs`, `host/mod.rs`, `unix/{kill,table}.rs`, `windows/{kill,hooks,mod,sys}.rs`; `crates/cli/src/{args,agent_cmd}.rs`, `crates/cli/tests/supervisor_e2e.rs`; `crates/testkit/src/fake_cli/{mod,sleeper}.rs`; `specs/thread_input.qnt`, `specs/thread_input_traces.qnt` |
| The PR 10a round-3 reviews | sol, Neon, triage (2026-11-25) | the findings §9 answers |

## 1. Goal and Dependencies

**Goal.** After a daemon crash, every recorded old process of an unconfirmed `daemon_restart` launch, including one an earlier start reconciled, is signalled by verified identity once per daemon start, without any command; what each check observed dead, observed alive, or did not observe is recorded as `launch_evidence`, as is what an owned close or a failed start could not prove; the owner reads it with `chaldea stop <thread> --assume-dead` and confirms one launch with `--launch <id>`, which is then the only way such a launch unlocks; and PR 21's finder receives what it needs for A29.

**Depends on** PR 10a1 (sup §2.12 "What 10a1 records for it", "Where 10a2 plugs in") and PR 10b, both merged.

**Files and seams** (every name marked new does not exist at `1d4e124`):

```
crates/pty-runtime/src/orphan.rs  # new: kill_recorded, OrphanReport, Fate; macOS over OrphanOs (new), Windows over JobOs + open_verified
crates/pty-runtime/src/lib.rs     # pub mod orphan
crates/pty-runtime/src/windows/hooks.rs # GateControl::wait_reached_pid (new): the gate's reached signal carries the created pid
crates/pty-runtime/src/fake.rs    # Windows test helpers (new): fake::suspend_count, fake::TestJob
crates/core/src/payload.rs        # LaunchEvidence, EvidenceSource, IdentityObservation, Observed, Unchecked, OWNER_ASSERTED (new)
crates/core/src/event.rs          # the kind launch_evidence
crates/core/src/command.rs        # StopArgs.assume_dead, StopOutcome::Asserted (new)
crates/core/tests/{golden,schema_agreement,contracts_export}.rs # launch_evidence and the new StopArgs/StopOutcome forms
crates/daemon/src/store/project.rs# launch_evidence: a no-op arm
crates/daemon/src/supervisor/prober.rs # new: the restart prober (§2.3)
crates/daemon/src/supervisor/{mod,components}.rs # Supervisor.probe_slots, SupervisorConfig.kill_recorded, Pause::{ProberStart, ProbeTask} (new); attach starts the prober
crates/daemon/src/supervisor/exit.rs # ExitPlan.evidence (new): appended right after launch_exited
crates/daemon/src/supervisor/ops.rs  # close and spawn_failed evidence where 10a1 only logs (§2.4)
crates/daemon/src/supervisor/commands.rs # the stop intent's assume_dead branch (§2.5)
crates/cli/src/{args,agent_cmd}.rs # chaldea stop --assume-dead [--launch <id>]; the preview; the death_unconfirmed text
crates/cli/tests/supervisor_e2e.rs # the restart cases of §6
crates/cli/tests/support/faults.rs # the helper daemon's prober pause and A29 runtime wrapper (test seams)
```

**New identifiers** besides the tree above: `orphan::kill_recorded`, `OrphanReport` (with `fates`), `Fate` and its variants, `OrphanOs`, `GateControl::wait_reached_pid`, `fake::suspend_count`, `fake::TestJob`, `LaunchEvidence`, `EvidenceSource`, `IdentityObservation`, `Observed`, `Unchecked` and their variants and wire values (§2.4), `OWNER_ASSERTED` and its value `owner_asserted` as a `launch_death_confirmed.method`, the event kind `launch_evidence` (`TypedEvent::LaunchEvidence`), `StopArgs.assume_dead`, `StopOutcome::Asserted` (`asserted`), `supervisor::prober`, `prober::run`, `PROBE_BUDGET`, `PROBE_JOIN_GRACE`, `PROBE_SLOTS`, `PROBE_READ`, `SLOT_WAIT`, `Supervisor.probe_slots`, `SupervisorConfig.kill_recorded`, `KillRecordedFn`, `Pause::ProberStart`, `Pause::ProbeTask` (wire names `prober_start`, `probe_task`), `ExitPlan.evidence`, the CLI flags `--assume-dead` and `--launch`, the idempotency key forms of §2.4, the claim, acceptance, and limit ids and every test name of §5–§6, and the Quint trace of §7.

## 2. Design

### 2.1 Rules

1. **Nothing after a crash confirms death but the owner.** A probe signals only verified identities and records what it observed; it never writes `launch_death_confirmed` and never changes state (owner decision).
2. **Evidence separates three classes.** Each recorded identity a check covered is observed dead, observed alive, or not observed, and each record lists what the check did not cover (§2.4).
3. **A slot is held by the work it bounds.** Each probe's blocking task owns its concurrency slot and returns it when the task itself ends, whatever its awaiter does (§2.3).
4. **One prober per start, candidates from the store.** The set is read after `attach` from the projection, so a launch an earlier start reconciled is probed again by every later start until the owner confirms it (RD1).

### 2.2 `orphan::kill_recorded`

The daemon crate has no OS dependency (sup §2.5; `crates/daemon/Cargo.toml`), so the unowned kill lives in pty-runtime, beside the owned engine it must not reuse (pty-unix §4 row "Daemon restart"; RD10):

```rust
// crates/pty-runtime/src/orphan.rs (new)
pub enum Fate { Gone, Replaced, Alive, Unreadable, NotRead, Invalid }
pub struct OrphanReport { pub fates: Vec<(ProcessIdentity, Fate)> }   // input order, duplicates collapsed
pub fn kill_recorded(identities: &[ProcessIdentity], deadline: Instant) -> OrphanReport;
```

- **Validation.** An identity whose pid is 0, 1 (macOS) or 4 (Windows), or the daemon's own, or whose `start_time` does not parse as `u64` (empty included), is `Invalid`: never read, never signalled. This is the engine's acceptance rule (`unix/kill.rs:518-524` `accept`; pty-windows §2.5 "Extras").
- **Deadline.** The clock is read before every OS call, and no call starts at or after `deadline` (pty-unix §2.5 "Deadline discipline", pty-windows §2.5 "Clock rules"). An identity with no read before the deadline is `NotRead`.
- **macOS**, over `OrphanOs` (new, `pub(crate)`): `lookup`, `kill`, `now`, `sleep` and nothing else, so the type has no list, `getsid`, or group call (RD10). The real one wraps `unix::table::lookup` (the full libproc record, the short one when that is refused, `unix/table.rs:30-52`) and `kill(2)`. With `t0` the entry and `B = deadline − t0`: a round of HUP, reads every 25 ms until `t0 + min(500 ms, B/4)`, a round of TERM, reads until `t0 + min(2 s, B/2)`, a round of KILL, reads every 25 ms until every identity is resolved or the deadline; with `B ≤ 100 ms` only the KILL round. A round reads each unresolved identity and signals it only when that read showed `Live` with the recorded start, with only the clock check between the read and `kill(2)` (pty-unix §2.5 `signal_verified`). A read showing `Gone` or a zombie resolves it `Gone`; `Live` with another start, `Replaced`. At the end an unresolved identity is `Alive` if its last read showed the recorded start, `Unreadable` if every read was refused, `NotRead` if none was made.
- **Windows**, over the backend's `JobOs` with `open_verified` (`windows/kill.rs:93-99`, `:135`): one `open_verified` per valid identity (`Held` kept; `Gone` → `Gone`; `Stale` → `Replaced`; `Unverifiable` → `Unreadable`; `Cut` → `NotRead`, and no further open); then `terminate` on every held handle, before any wait; then rounds of `signalled` on each held handle under the clock rules until all are signalled or the deadline: signalled → `Gone`, else `Alive`. No job call is made.

### 2.3 Restart prober

`SupervisorComponents::attach` spawns `prober::run(sup)` after its 10a1 steps; startup does not wait for it, and the HTTP server starts serving at step 13 meanwhile (`lifecycle/startup.rs:676`, `:736`). The prober, in order:

1. `Pause::ProberStart`. If `Tracker::accepting()` is false (shutdown began), it returns.
2. *Candidates.* One read transaction on a blocking task, `ReadTx::open_launches()` (`readers.rs:815-822`: `exit_reason IS NULL OR death_confirmed = 0`), awaited with `settle_before` and `in_time()` until its start + `PROBE_READ` (10 s), as the stop's re-read is bounded (sup §2.3 Stop step 2). It keeps the rows with `exit_reason = daemon_restart`, death unconfirmed, and `pid` recorded, in `(thread_id, reserved_seq)` order. Every such row's process is from an earlier run, since this run's own launches never get that reason, so the prober cannot reach a launch this daemon owns. The read is a snapshot: an assertion committed after it does not stop that launch's probe, which then signals only the old launch's verified leader and appends evidence after the confirmation. A read not returned in time ends the prober with a log line; the next start reads again.
3. *Per candidate*, while `Tracker::accepting()`:
   1. A slot: `Supervisor.probe_slots` (a `tokio::sync::Semaphore` of `PROBE_SLOTS` = 4) `acquire_owned`, awaited with `settle_before` and `in_time()` until now + `SLOT_WAIT` (30 s); a `Late` permit is dropped at once. No slot in time ends the prober with the remaining launches logged; each holder's work ends at its own deadline unless the blocking pool or the OS stalls, so a 30 s wait means such a stall.
   2. The identity: `ProcessIdentity { pid, start_time, group: pgid_or_job }` from the row, an absent text as empty (then `Invalid`). An attempt id, a UUID v4 the prober mints now. `deadline = now + PROBE_BUDGET` (5 s).
   3. `spawn_blocking` of a closure that owns the permit: it calls `SupervisorConfig.kill_recorded` (`orphan::kill_recorded` outside tests) with `[identity]` and `real(deadline)`, passes `Pause::ProbeTask`, and drops the permit as it returns, on a panic's unwind too. The slot is therefore returned when the blocking work ends, not when the prober stops waiting for it (sol r3 #2b).
   4. A task of its own awaits the join with `settle_before` until `deadline + PROBE_JOIN_GRACE` (1 s). `InTime(Ok(report))` and `Late(Ok(report))` give the fates: a late report is still an observation, and nothing is decided by when it came. `Elapsed`, or a join error (a panic), gives every identity `NotRead` and adds `result_not_received` (Neon LOW). That task then records the evidence (§2.4) through `supervisor::record` with `nudge: false`; the job appends one event and changes no state.
4. The prober returns once every candidate's task is spawned.

**Retry rules** (sol r3 bullet). `supervisor::record` retries the same job, the same payload under the same key, on its retryable outcomes (sup §2.10 "Store jobs"); `Unrecorded` and `ShuttingDown` are logged and the evidence is dropped. No record outcome, and no join outcome, starts a second OS probe of a launch in the same start; the next start probes it again under a new attempt id.

**Shutdown.** The prober reads `Tracker::accepting()` before each slot and after it, as the tick does (`tick.rs:56-66`), and stops when the participant closes admission (sup §2.9 step 1). It creates no process and holds no tracker token, so the participant's result does not depend on it. A probe's blocking task still running when the process exits is abandoned with the blocking pool (sup L-8): it may have sent HUP or TERM without KILL, and the next start probes the launch again (L-6).

**What the prober does not do.** It probes no `unknown_after_crash` launch (no identity), and no `stopped`, `exit`, or `spawn_failed` launch: their sessions were closed by the owned kill, whose report is recorded as evidence (§2.4), and `failed_start` is not projected into the row (`rows.rs:44-70`). `stop` probes nothing (RD2).

### 2.4 Evidence

A new thread-stream event kind, `launch_evidence`, records one check. It changes no state: the projection's arm is a no-op (`crates/daemon/src/store/project.rs`, beside `MailboxMessage` and the others at `:457-461`), and an older client decodes it as a generic event (`event.rs:215-231`). It is additive under mvp §2.1 "`schema_version`".

```rust
// crates/core/src/payload.rs (new)
pub struct LaunchEvidence {
    pub launch_id: LaunchId,
    pub source: EvidenceSource,                // "spawn_failed" | "close" | "probe"
    pub identities: Vec<IdentityObservation>,  // each recorded identity the check covered
    pub unchecked: Vec<Unchecked>,             // what the check did not cover; may be empty only for a probe whose every identity was read
}
pub struct IdentityObservation { pub pid: u32, pub start_time: Option<String>, pub observed: Observed }
pub enum Observed { Gone, Replaced, Alive, Unresolved, Unreadable, NotRead, Invalid }
pub enum Unchecked { UnrecordedSessionMembers, JobMembers, TreeUnproven, NoIdentity, CleanupUnreleased, ResultNotReceived }
pub const OWNER_ASSERTED: &str = "owner_asserted";
```

Every field is always present (`payload.rs`'s rule), `start_time` as `null` when the identity had none.

| `observed` (wire) | Class | Meaning |
|---|---|---|
| `gone` | dead | a read showed the pid gone or a zombie, or a proving kill report covered it |
| `replaced` | dead | the pid holds a process with another start, so the recorded one ended |
| `alive` | alive | the last read showed the recorded start, after the KILL round |
| `unresolved` | not observed | a kill report listed it as a survivor: not shown dead, without saying whether its last read was live or refused |
| `unreadable` | not observed | every read was refused |
| `not_read` | not observed | no read before the deadline, or the check's result did not reach the daemon in time |
| `invalid` | not observed | never read or signalled (§2.2 "Validation") |

| `unchecked` (wire) | Meaning |
|---|---|
| `unrecorded_session_members` | macOS probe: members of the old session without a recorded identity (L-1; RD9) |
| `job_members` | Windows probe: members of the old job, terminated by `KILL_ON_JOB_CLOSE` when the old daemon exited, not observed (L-2) |
| `tree_unproven` | the owned kill did not prove the tree dead: processes it did not list may exist |
| `no_identity` | no identity was recorded for the launch |
| `cleanup_unreleased` | the tree was proven dead, but the backend's cleanup of a failed start did not complete |
| `result_not_received` | the check's result did not reach the daemon by its cutoff |

**Sources** (each appended in the job named, right after `launch_exited` when in the exit job, so every evidence event of a launch has a higher seq than its `launch_exited`):

| Source | Written when, in which job | `identities` | `unchecked` | Idempotency key |
|---|---|---|---|---|
| `spawn_failed` | the exit job of a `StartedThenFailed` whose cleanup is `Cleanup::Unresolved` (sup §2.3 step 6), where 10a1 only logs (`ops.rs:206`) | the returned identity: `gone` when `Unresolved.kill` is a proving report (`ops.rs:660-662` `proving`); else that report's survivors and the identity, each `unresolved` | `no_identity` when none was returned; `cleanup_unreleased` with a proving kill; `tree_unproven` without one | `launch:<L>:evidence:spawn_failed` |
| `close` | the exit job of a stop, a natural exit, the participant, or a late creation whose death is unconfirmed (`ops.rs:731-741`, `:1092`) | when the close's result was received by the op's cutoff: its `kill.survivors`, each `unresolved` (possibly none); else none | `tree_unproven`; and `result_not_received` when the result was not received | `launch:<L>:evidence:close` |
| `probe` | the prober's record job (§2.3) | the leader identity with its `Fate` (`NotRead` on a result not received) | `unrecorded_session_members` (macOS) or `job_members` (Windows); `result_not_received` when so | `launch:<L>:evidence:probe:<attempt>` |

The first two are written at most once per launch, as `launch:<L>:exited` is: `exit_in` writes nothing for a launch that already exited (`exit.rs:102-104`), and `ExitPlan.evidence` rides that job. A probe's attempt id is a server-minted UUID, never a client value, so no client `command_id` shares its namespace (sol r3 bullet); a key reused with other content is `KeyConflict` (`write_tx.rs:487-515`), which the fixed payload of a retried job never produces. The evidence a launch 10a1 marked before 10a2 carries is only what the prober adds (sup §2.12 "What 10a1 only logs").

### 2.5 Owner Assertion and Preview

**Wire.** `StopArgs` gains `assume_dead: bool` (`#[serde(default)]`, absent means false; `deny_unknown_fields` stays). `StopOutcome` gains `asserted`. `StopResult` keeps its fields.

**`prepare`**: `assume_dead` without `expected_launch_id` → 400 `bad_args`.

**Intent**, with `assume_dead`, in this order: the target scope (sup §2.2, `commands.rs:73` `target`; 404 for a thread the token cannot read); a token whose `kind` is not `TokenKind::Owner` → 403 `forbidden` (classified at `http/commands.rs:139`); the current launch `L` ≠ `expected_launch_id` → `launch_changed`; `L` without `exit_reason` → `launch_live`; `L` exited with death confirmed → `already_exited`, nothing written (10a1's branch, `commands.rs:255-259`); else, in the intent's own job, `launch_death_confirmed { launch_id: L, method: "owner_asserted" }` under the key `launch:<L>:death_confirmed` with `StateInput::DeathConfirmed` (which clears `resume_blocked`, `state.rs:327-331`), and `Completed(StopResult { outcome: asserted, death_confirmed: true, confirmed_within_deadline: false, exit_code: null, signal: null })`. Without `assume_dead` the intent is 10a1's, `death_unconfirmed` included.

Every reason applies: `daemon_restart`, `unknown_after_crash`, an unconfirmed `stopped` or `exit`, `spawn_failed` with unresolved cleanup. The assertion requires no evidence; what the owner reads first is the preview's job, not the daemon's (L-9).

**One confirmation per launch.** The intent runs in the writer's job, which re-reads `L`; the writer runs one job at a time, so of two assertions for `L` the second reads it confirmed and answers `already_exited`. The key backs this: a second `launch_death_confirmed` for `L` from another command differs in `command_id` and is `KeyConflict`. A probe never writes one (§2.1 rule 1). After the assertion, `resume` and `close` pass their death check (sup §2.3 step 3, "Close"), and the launch no longer counts as in use for worktree removal (mvp §2.9).

**CLI.** `chaldea stop <thread> --assume-dead [--launch <id>]`; `--launch` requires `--assume-dead`. With `--launch`, it sends `StopArgs { thread_id, expected_launch_id: id, assume_dead: true }` and on `asserted` prints "Recorded your confirmation that launch <id> has ended." and exits 0, on `already_exited` prints that it was already confirmed and exits 0; refusals print as 10a1's do. Without `--launch` it sends nothing (the preview).

**Preview** (Neon #3), under one 30 s cutoff for every request (`cli::client`'s deadline rules):

1. `GET /api/threads/{thread_id}` → the current `LaunchInfo` (sup §2.8). No launch, a launch not exited, or one confirmed: print that and exit 0.
2. Page backward through the thread stream, `GET /api/events?stream_id=<thread>&before_seq=<b>&limit=1000`, from `b` = 9007199254740991 (accepted, `sync_api.rs:320-321`), each next `b` the page's lowest seq. Keep the `launch_evidence` events of `L`. Stop at `L`'s `launch_exited`, since every evidence event follows it (§2.4); for `unknown_after_crash` go on to `L`'s `launch_reserved`, which such a launch reaches through no `launch_started` and no session, for its `agent` and `recorded_at`. Also stop at an empty or short page.
3. Print the launch id, exit reason, and "death unconfirmed"; each evidence event in seq order with its `recorded_at`, source, each identity (pid, start, `observed` and its class from §2.4's table), and each `unchecked` item's meaning; "No probe evidence is recorded yet; the daemon probes such a launch after each start." for a `daemon_restart` launch without a `probe` event; for `unknown_after_crash`, "No process identity was recorded" with the agent and `recorded_at`; then the command `chaldea stop <thread> --assume-dead --launch <L>`. Exit 0; a read that fails or ends at the cutoff exits 1.

The scan's length is the number of events after `L`'s exit: a current, exited launch has no later launch, so these are its evidence (one per start for a `daemon_restart` launch) and the thread's attention and input records.

**`death_unconfirmed` text** (sup §2.2). The tail "This version cannot resume or close this thread. Its history is kept, and new threads can be started." becomes "To review what was checked, run `chaldea stop <thread> --assume-dead`; once you have made sure its processes ended, confirm with `--launch <id>`. Its history is kept, and new threads can be started." The reason lines stay.

### 2.6 A29 Hand-off to PR 21

A crash between the suspended `CreateProcessW` and `AssignProcessToJobObject` leaves a reservation without identity, recorded `unknown_after_crash`, and a suspended child in no launch job (pty-windows §2.2 "Crash window"; sup SD2). 10a2 adds no record for it (RD8). PR 21's finder reads, for each current launch exited `unknown_after_crash` with death unconfirmed on a Windows host: the launch id; the agent from its `launch_reserved`; that event's `recorded_at` (RFC 3339 UTC, milliseconds, `ids.rs:117`). Its candidates are processes that are suspended, in no job the running daemon holds (the old launch jobs ended with the old daemon; a candidate may be in another job, since the create flags have no breakaway, `windows/sys.rs:377-381`), whose image file name is the agent's executable name, and whose creation time, truncated to milliseconds, is not before `recorded_at` (Neon #4): `launch_reserved` commits before the spawn (sup §2.3 steps 3–4). The finder shows each candidate with its image path and creation time as candidate evidence, stating that the launch's resolved program path is not recorded (sol r3 bullet); a candidate is never a death proof, the doctor ends one only on the owner's confirmation, and confirming `L` stays the assertion of §2.5. A wall-clock step back between the reservation and the creation hides the child from the time condition (L-3).

### 2.7 Waits

| Wait | Helper | Bound |
|---|---|---|
| Candidate read (prober) | `deadline::settle_before` on the blocking read, `in_time()` | `PROBE_READ` (10 s); else the prober ends, logged |
| Slot (prober) | `deadline::settle_before` on `acquire_owned`, `in_time()`; a `Late` permit dropped | `SLOT_WAIT` (30 s); else the prober ends, logged |
| Probe join | `deadline::settle_before` on the join handle; `InTime` and `Late` values used | `deadline + PROBE_JOIN_GRACE` (6 s from the probe's start); else `not_read`, `result_not_received`; the task runs on with its slot |
| Inside `kill_recorded` | the clock read before every OS call (§2.2) | `deadline` (5 s) |
| Evidence record | `supervisor::record` (sup §2.10 "Store jobs") | its budget; then dropped, logged |
| Assertion | the runner's intent job (http-surface §2.8) | the command's deadline |
| Preview | `cli::client` requests under one cutoff | 30 s; then exit 1 |

No new exception to the deadline lint.

## 3. Decisions

Each compares gain, loss, reversibility, and cost. "Public" marks a decision visible to users or clients.

**RD1. The prober's candidate set** (sol r3 #2a, Neon #2).

| Option | Gain | Loss | Reversibility | Cost |
|---|---|---|---|---|
| A. The launches this start's reconciler marked | none to read | a second crash after the reconcile commit and before the probe leaves the launch unprobed for good; the reconciler's job returns only a count (`lifecycle/mod.rs:170-174`) | — | small |
| **B. Every unconfirmed `daemon_restart` launch with a pid, read after `attach`, every start** | survives any number of crashes; no state beyond the projection | one evidence event per such launch per start until the owner asserts (L-8); one read per identity already gone | narrowing to C later is local | small |
| C. B minus launches whose last probe observed every identity dead | fewer events | needs the evidence read per launch or a new projection column, a cross-start state the probe does not need (a dead identity stays dead, so B's repeat costs one read) | — | medium |

**Adopted: B.**

**RD2. Probe on `stop`, and the probe lock** (sol r3 #2b).

| Option | Gain | Loss | Reversibility | Cost |
|---|---|---|---|---|
| A. The prober and a `stop` both probe, serialized by a per-launch probe lock | an on-demand re-probe | a lock whose entry must outlive waiters and slow blocking tasks, a third `stop` outcome and a `recorded_gone` field (an earlier draft's, never built), an op path, and a namespace for client ids in keys | the flag and outcome stay once published | large |
| **B. Only the prober probes; `stop` without `--assume-dead` keeps 10a1's `death_unconfirmed`** | no lock: one prober per start probes each launch once, so probes of a launch never overlap; no wire change for plain `stop` | a probe whose record failed, or an identity left `alive`, waits for the next start (L-5, L-6) | A can be added later as a new outcome | small |

**Adopted: B.** A probe unlocks nothing (owner decision), so an on-demand probe only refreshes what the owner reads; restarting the daemon gives the same. The rejected design's lock is where sol r3 #2b found the waiter and slow-task gaps.

**RD3. Slot lifetime** (sol r3 #2b). Adopted: an owned semaphore permit moved into the blocking closure, dropped when the closure returns or unwinds. Rejected: a permit held by the awaiting task (released at a join timeout while the blocking work runs on, so more than 4 run); fixed batches of 4 joined together (the same gap at a batch's timeout).

**RD4. Evidence shape** (sol r3 #6). Public.

| Option | Gain | Loss | Reversibility | Cost |
|---|---|---|---|---|
| **A. One event kind; per identity an `observed` value in three classes; a list of what was not covered** | the owner sees what was read dead, read alive, or not read, and what no read covered; an owned close with an empty survivor list still shows `tree_unproven` | a new public kind and enums | additive values later | small |
| B. Fields on `launch_exited` | no new kind | a probe happens after the exit and once per start | — | small |
| C. `survived` for every identity not shown dead (the earlier text) | fewer values | merges alive, refused, and not received; shows a proven-dead failed start's leader as a survivor | — | small |

**Adopted: A.** Public: the kind `launch_evidence`, its payload, and the values of §2.4. Would change if the owner wants raw kill reports instead; they stay in the log.

**RD5. Idempotency keys** (sol r3 bullet). Public. Adopted: `launch:<L>:evidence:spawn_failed`, `launch:<L>:evidence:close`, `launch:<L>:evidence:probe:<attempt>` with a server-minted UUID, and the assertion under the existing `launch:<L>:death_confirmed`. RD2 leaves no client-supplied id in a key. Rejected: `probe:<command_id>` beside the UUID in one namespace (a client id could equal a server one); a per-start counter (needs a stored start number).

**RD6. Where the assertion runs.** Public: `StopArgs.assume_dead`, `StopOutcome::Asserted`, the method `owner_asserted`, and the CLI flags. Adopted: in the `stop` intent's job, with no op: it is one store transition the writer serializes. Rejected: a `LaunchOp` (no process work to own); a separate command name (a new grant row in `MVP_COMMANDS` for one branch of `stop`); `confirmed_within_deadline: true` for `asserted` (an assertion proves nothing within mvp P-3's 5 s). On a concrete input: `stop` with the same args as before behaves as in 10a1; an old daemon refuses `assume_dead` as an unknown field (400 `bad_args`); a client that matches `StopOutcome` exhaustively sees a new value only when it sends `assume_dead`.

**RD7. Preview scan range** (Neon #3). Adopted: backward from the head to `L`'s `launch_exited` (to `launch_reserved` only for `unknown_after_crash`). Rejected: down to `launch_reserved` for every launch (pages through the whole session history of a long launch); a new evidence route (a route and a grant for a read the stream already serves). Public: none beyond the CLI's output.

**RD8. A29 hand-off** (sol r3 bullet, Neon #4).

| Option | Gain | Loss | Reversibility | Cost |
|---|---|---|---|---|
| **A. Existing records (launch id, agent, `recorded_at`); the finder matches the agent's executable file name and says the path was not recorded** | no new field | a resolved program whose file name differs from the agent's (a PR 14 shim resolution) is missed: its suspended child stays until the owner ends it, and it never runs | B can be added later | none |
| B. `launch_reserved` gains the resolved program path | exact image match | a new public field on every reservation | additive | small |

**Adopted: A.** Would change to B if PR 14's program resolution yields file names other than the agent's.

**RD9. P-18's survivor policy** (sol r3 #1). For the owner. mvp P-18: no surviving old process after a daemon kill except the documented macOS `setsid` limit (spike §4.6). After a crash the new daemon owns no old session (pty-unix §2.4 "Ownership is a precondition"), so it signals only recorded identities (§2.2). A member of the old session that survived the PTY hangup and has no recorded identity, such as a same-session child that ignores SIGHUP (a `nohup` command an agent started), is neither signalled nor checked. On Windows the old job's members are terminated by `KILL_ON_JOB_CLOSE` when the old daemon exits (pty-windows §4 row "Daemon restart"), unobserved; the only process outside a job is A29's suspended child, which never runs.

| Option | Owner gains | Owner loses | Reversibility | Cost | Parts of 10a2 that change |
|---|---|---|---|---|---|
| (a) Keep P-18; mark it unmet on macOS until a design finds and ends such members after a crash | P-18 stays the full requirement; nothing claims what the code cannot show | P-18's real-run row fails on macOS whenever such a member exists, with no path to pass in the MVP short of (c) or a later design | text only | none | PR10a2-P-4 is judged as unmet for that case; L-1 states it; mvp P-18 unchanged |
| **(b) Widen P-18's limit: after a crash, processes of the old launch without a recorded identity (on macOS, members of the old session that survived the hangup; on Windows, A29's suspended child), reported as unchecked in the launch's evidence** | P-18 states what 10a2 does; the owner sees the unchecked scope in the preview before asserting, and nothing unlocks without the assertion | such a member keeps running until the owner ends it; "reported" names a scope, not pids | text; (c) narrows it later | none | mvp P-18 text (§8); L-1 as written |
| (c) Record members while owned: a periodic owned census per live launch (macOS: a clean owned pass; Windows: the job's member list), recording members seen in two consecutive snapshots as identities in a new event; the prober adds them | long-lived members (MCP servers, a `nohup` server) are ended and observed after a crash | members younger than two periods stay unchecked; a census per launch per period (a pass is a few ms over ~1 000 pids, pty-unix §2.5); an event per membership change; a new owned-census method on `PtyChild` in both backends | additive event kind, kept in history once written | medium | the prober's identity source, a tick step, a new event, the evidence's `unrecorded_session_members` narrowed to members not snapshotted, new claims; P-18 text as (b) with the snapshot age |
| (d) A census of session `L` bracketed by two verified reads of the recorded leader alive | covers members when the leader itself survived the hangup | no help in sol's case, where the leader died: then sid `L` can name another session; brings the census SD3 rejected into the unowned path | reversible | medium | `orphan` gains a census; the evidence a field |

**Recommendation: (b).** The owner decision already keeps a crashed launch from unlocking without the owner's assertion, so the harm left is a process running on, which the preview names; (b) promises exactly that and costs nothing. (c) is additive to this design: the prober already takes a list of identities, so it can follow as its own PR. **What would change it:** PR 15/23's P-18 run on real CLIs finding a same-session survivor (an MCP server or a backgrounded command that ignores SIGHUP), or the owner treating P-18 as a release gate that a named scope does not meet: then (a) now with (c) scheduled.

**RD10. `kill_recorded` as its own loop.** Adopted: §2.2 over `OrphanOs` / `JobOs` with no list call. Rejected: `kill_tree` with an unowned `Os` (its rounds list every pid and cover descendants by parent, a census in all but name, `unix/kill.rs:595`); the sid scan of mvp §4 (sup SD3); `host::send_stop` (`host/mod.rs:911-956`, TERM only, the daemon's own stop target).

## 4. Obligation Inventory

| Id | Source | 10a2's answer |
|---|---|---|
| A1 | mvp:439; pty-unix:409 | §2.2, §2.3; RD1, RD10; PR10a2-CL-1, PR10a2-CL-3, PR10a2-CL-4 |
| A2 | mvp:439 | §2.5 (assertion for `unknown_after_crash`); PR10a2-CL-8 |
| A28 | mvp:439; pty-windows:412 | §2.2 Windows, §2.4 `job_members`; PR10a2-CL-2, PR10a2-CL-11 |
| A29 | mvp:439; pty-windows:129 | §2.6; RD8; PR10a2-CL-10 |
| F1 | pty-unix:409 | §2.2 macOS; PR10a2-CL-1 |
| sup L-1, L-2 | sup §5.3 | §2.5 replaces L-1's "until PR 10a2"; RD9 decides L-2 |

## 5. Purpose Layer and Plan Layer

### 5.1 Purpose Layer: Acceptance Criteria

Real-CLI checks run on macOS with Claude and Codex; Windows is covered by CI with FakeCli. A resumed session continuing (mvp P-18's last clause) needs PR 13/14's binding and is their gate.

| ID | Criterion (EARS) | Source | Scene | Observable result / failure | How to check on the real CLI |
|---|---|---|---|---|---|
| PR10a2-P-1 | When the daemon restarts after a crash, it shall, with no command, signal by verified identity the recorded leader of every unconfirmed `daemon_restart` launch, including one an earlier start reconciled, and record one `probe` evidence event per such launch, leaving death unconfirmed. | owner decision; mvp §2.9 | a CLI running when the daemon is killed by PID; once with a second kill right after the restart | Pass: one `probe` event per start per launch; the leader `gone` or `replaced`, or `alive` reported; `death_confirmed` false. Fail: no event after the second start; a confirmation. | `kill -9 <daemon pid>`; restart; `chaldea stop <thread> --assume-dead`; `ps -p <leader pid>`. |
| PR10a2-P-2 | When the owner runs `chaldea stop <thread> --assume-dead` without `--launch`, the CLI shall print the current launch's exit reason, death state, and each evidence event after its exit with each identity's class and each unchecked scope, and shall send no command. | §2.5 | after P-1 | Pass: the printed evidence equals the stream's; no `POST`. Fail: a command sent; an identity without its class. | as stated; then `GET /api/events` to compare. |
| PR10a2-P-3 | When the owner runs `chaldea stop <thread> --assume-dead --launch <L>` for the current, exited, unconfirmed launch, the daemon shall record `launch_death_confirmed { method: "owner_asserted" }` once and clear `resume_blocked`, and `close` shall then succeed. | owner decision | after P-2 | Pass: one such event; `close` succeeds. Fail: two events; `close` refused. | as stated; `chaldea close <thread>`; `resume` continuing the session: PR 13/14. |
| PR10a2-P-4 | When the daemon is killed and restarted, no recorded old process shall survive the probe unreported, and every other surviving old process shall be within P-18's stated limit (RD9's adopted option). | mvp P-18; RD9 | a CLI with an MCP server, killed mid-turn | Pass: every recorded identity `gone`, `replaced`, or reported; other survivors only as the limit allows. Fail: a recorded identity alive and unreported; a survivor outside the limit. | `pgrep -s <leader pid>` before the kill and after the probe; the preview. |
| PR10a2-P-5 | If an owned close or a failed start leaves death unconfirmed, then its evidence shall list what the kill could not show dead and what it did not cover. | §2.4 | a stop of a tree that ignores KILL is not reachable on a real CLI | — | Not a real-CLI check (PR10a2-CL-7). |
| PR10a2-P-6 | If the daemon crashes in the A29 window on Windows, then the launch shall read `unknown_after_crash` and its suspended child shall meet the finder's candidate conditions. | A29; §2.6 | FakeCli, CI | Pass: as stated. Fail: a condition unmet by the real child. | Not a real-CLI check (PR10a2-CL-10). |

### 5.2 Plan Layer: Claims

Tests named `supervisor::…` are daemon unit tests with `FakePty`, the in-process store, and the pause points (sup §6); `orphan::…` are pty-runtime unit tests; `supervisor_e2e::…` are in `crates/cli/tests/supervisor_e2e.rs` with the native runtime and FakeCli, the daemon in a helper process killed by PID (the existing `crash_at` pattern).

| ID | Claim (EARS) | Source | Test that falsifies it | Mutation it kills |
|---|---|---|---|---|
| PR10a2-CL-1 | `kill_recorded` on macOS shall signal a pid only right after a read of it showed the recorded start, in rounds HUP, TERM, KILL with the engine's timing, shall make no call at or after the deadline, never signal an `Invalid` identity, and classify each identity as §2.2 states. | A1, F1; RD10 | `orphan::unix_tests` over a scripted `OrphanOs` and a scripted clock: live leader → HUP, TERM, KILL to that pid only, then `Gone`; another start → `Replaced`, no signal; always refused → `Unreadable`; refused then live → `Alive`; pid 0, 1, own pid, empty or bad start → `Invalid`, no call; deadline already passed → `NotRead`, no call; a deadline passed between two calls → no further call; `B ≤ 100 ms` → KILL only; a zombie → `Gone`. `OrphanOs` has no list, `getsid`, or group method. | a signal after a read showing another start; `Unreadable` as `Gone`; a call after the deadline; a signal to an `Invalid` identity |
| PR10a2-CL-2 | `kill_recorded` on Windows shall open each identity with `open_verified`, terminate every held handle before any wait, and classify each identity as §2.2 states. | A28 | `orphan::windows_tests`: a scripted `JobOs` (every terminate call precedes the first `signalled` call; `Stale` → `Replaced`, never terminated; `Unverifiable` → `Unreadable`; `Cut` → `NotRead`, no later open; no job call); test-owned FakeCli sleepers (two live → both `Gone`; one with a wrong start → `Replaced` and still alive, ended by the test) | sequential terminate-and-wait; a `Stale` handle terminated; `Unverifiable` as `Gone` |
| PR10a2-CL-3 | After `attach`, the prober shall probe every launch whose `exit_reason` is `daemon_restart`, death unconfirmed, and pid recorded, whichever start reconciled it, once per start, and no other launch. | RD1; sol r3 #2a | `supervisor::prober::candidates` (store rows: `daemon_restart` from this start and from an earlier one, `daemon_restart` confirmed, `unknown_after_crash`, `stopped` unconfirmed, a live launch of this run; a scripted `SupervisorConfig.kill_recorded` records its calls: exactly the two unconfirmed `daemon_restart` leaders, once each) | candidates from this start only; a confirmed launch probed; a live launch probed |
| PR10a2-CL-4 | If the daemon crashes again after the restart reconciler committed and before the probe, then the next start shall probe the launch with no command sent. | sol r3 #2a; Neon #2 | `supervisor_e2e::second_crash_before_probe` (macOS: FakeCli `sleeper --role ignore-term` as the leader; daemon 1 killed by PID; daemon 2 held at `prober_start` after its reconcilers committed, killed by PID; daemon 3 held at `prober_start`: the leader is `Live` with its recorded start (the control); released: a `probe` event naming the leader `gone`, the leader gone by `process::lookup` at that instant, no `stop` sent, death unconfirmed. Windows, with a plain `sleeper` leader (`ignore-term` is Unix only, `sleeper.rs:4-17`): the same three daemons; there is no live-leader control, since the old job's close ends the leader when daemon 1 dies; the event names it `gone` with `job_members`) | candidates from this start only; a probe without a signal (the control's leader would stay alive) |
| PR10a2-CL-5 | The prober shall run at most `PROBE_SLOTS` blocking probes at once, counting probes whose join timed out, and return a slot only when its blocking task ends; a join not received in time shall record every identity `not_read` with `result_not_received`; a record retry shall reuse the payload and key, and no record or join outcome shall start a second probe of a launch in one start. | RD3; sol r3 #2b, bullet; Neon LOW | `supervisor::prober::bounded` (six candidates; `Pause::ProbeTask` holds every blocking task; the controlled clock passes the joins' cutoff: four tasks started, the fifth only after one held task is released, though all four joins elapsed; the elapsed ones record `not_read` and `result_not_received`); `supervisor::prober::record_retry` (the first attempt rolled back `Busy`: one event, one `kill_recorded` call; an `Unrecorded` record: no second call in this start) | the slot released at the join's timeout; an elapsed join recorded as `alive`; a re-probe after a failed record; a new key per retry |
| PR10a2-CL-6 | A probe shall never confirm death or change state, and the prober shall start no probe once admission is closed. | §2.1 rule 1; owner decision | `supervisor::prober::never_confirms` (every identity `gone`: the launch stays unconfirmed, `resume` → `death_unconfirmed`, the projection unchanged but for the event); `supervisor::prober::stops_at_shutdown` (admission closed while held at `prober_start`: no call) | a probe confirming; a probe after admission closed |
| PR10a2-CL-7 | An exit that an owned close did not confirm, and a `StartedThenFailed` with unresolved cleanup, shall record one evidence event as §2.4's table states, right after `launch_exited`; an exit an owned close confirmed shall record none. | §2.4; sol r3 #6 | `supervisor::evidence::recorded` (`StartedThenFailed` with no identity, with a proving `Unresolved.kill` → `gone` + `cleanup_unreleased`, with a non-proving one → `unresolved` + `tree_unproven`; a stop whose close reports `tree_dead: false` with survivors and with none → `tree_unproven` in both; a stop whose join the clock makes elapse → `result_not_received`; the participant's unconfirmed close; a confirmed stop: no evidence; each key; the event's seq right after `launch_exited`'s) | evidence missing on one path; a proven-dead leader shown `unresolved`; empty `unchecked` for an empty survivor list; evidence after a confirmed close |
| PR10a2-CL-8 | `stop` with `assume_dead` shall be accepted only from an owner token, only with the current launch's id, only for an exited launch, and shall record `owner_asserted` at most once per launch. | RD6; owner decision | `supervisor::stop::assume_dead` (an agent token whose plain `stop` of the same child thread passes the target scope → 403 `forbidden`; no `expected_launch_id` → 400 `bad_args`; a stale id → `launch_changed`; running → `launch_live`; each unconfirmed reason → `asserted`, one event, `resume_blocked` cleared, then `resume` reserves; repeated → `already_exited`, no event); `supervisor::stop::one_confirmation` (two assertions with different `command_id`s released together through a parked writer: one `asserted`, one `already_exited`, one event) | an agent token accepted; no launch binding; a running launch asserted; a second event |
| PR10a2-CL-9 | The preview shall send no command, stop its scan at `L`'s `launch_exited` (at `launch_reserved` for `unknown_after_crash`), and print each evidence event of `L` with each identity's class; the `death_unconfirmed` text shall name the preview. | RD7; Neon #3 | `agent_cmd::tests::assume_dead_preview` (a stub server with 3 000 events before `L`'s exit and 2 evidence events after it: one summary read, backward pages that stop at the exit, no `POST`, the printed lines equal the evidence; an `unknown_after_crash` launch: the scan reaches its `launch_reserved` and prints `recorded_at`); `agent_cmd::tests::unconfirmed_text` | a preview that sends; a scan to `launch_reserved` for every launch; a missing class |
| PR10a2-CL-10 | On Windows, a crash in the A29 window shall leave the launch `unknown_after_crash`, and its child shall be suspended, in the job of the process that created it, with the agent program's image, and created not before the reservation's `recorded_at` at millisecond granularity. | A29; RD8; Neon #4; sol r3 bullet | `supervisor_e2e::a29_child_meets_finder_conditions` (Windows: the test puts the helper daemon in its own `fake::TestJob` at creation; the helper's components (`tests/support/faults.rs`) wrap `SupervisorConfig.runtime` in a `PtyRuntime` that calls `fake::windows_inject_next(WindowsFault::RejectJobList(Some(gate)))` on the spawning thread before delegating `spawn` (the fault is thread-local, `windows/hooks.rs:198-207`), and a thread of the helper waits on `GateControl::wait_reached_pid` and writes the pid to the e2e gate file; the test kills the helper there, restarts, and checks: `unknown_after_crash`; `fake::suspend_count` of the child's thread ≥ 1; `TestJob` contains the child (the outer job observed to exist); its image and creation time as stated; then ends it) | `daemon_restart` for that launch; a strict "after" on equal milliseconds; a condition requiring no job |
| PR10a2-CL-11 | After a crash with a recorded leader that survived the hangup, the probe shall end it and record its fate, and the evidence shall name the unchecked scope of the host. | A1, A28; RD9 | covered by PR10a2-CL-4's control on both hosts | — |

### 5.3 Accepted Limits

| ID | Limit | Decided by | Why it does not defeat the purpose |
|---|---|---|---|
| L-1 | macOS, after a crash: members of the old session without a recorded identity that survived the hangup are neither signalled nor checked; under RD9 (a) this case leaves P-18 unmet, under (b) it is P-18's stated limit. | pty-unix §2.4, §2.5 (no group or session target without ownership); RD9 | the evidence names `unrecorded_session_members`; nothing unlocks until the owner asserts after reading it; a live-daemon stop and a graceful daemon stop keep the full owned kill (sup SD10). |
| L-2 | Windows, after a crash: old job members are terminated by `KILL_ON_JOB_CLOSE`, observed only through the recorded leader. | pty-windows §4 row "Daemon restart" | the job admits no breakaway (pty-windows D5), so survivors are limited to A29's child, which never runs; the evidence names `job_members`. |
| L-3 | A launch without a recorded identity (A2, A29) is never probed; A29's suspended child stays until the owner or PR 21's finder ends it, and a wall-clock step back hides it from the finder's time condition. | sup SD2; RD8 | it never runs; the launch blocks reservation until the owner asserts; the preview says no identity was recorded. |
| L-4 | A signal of `kill_recorded` can reach a reused pid: on macOS if the pid space wraps between its read and the signal; on Windows only for two processes with the same pid and creation stamp after a clock step back. | pty-unix §2.5 "Never a reused pid"; pty-windows §2.5 "Extras" | the residual cases PR 3 and PR 4 accepted. |
| L-5 | A probe whose evidence was not recorded, or whose join timed out, is redone only at the next start. | RD2 | the owner's assertion does not depend on it; the preview says when no probe evidence exists. |
| L-6 | A recorded identity left `alive` (it survived KILL) or not observed is probed again only at the next start; a probe abandoned at process exit may have sent HUP or TERM without KILL. | RD2; sup L-8 | the evidence reports it; every start probes again. |
| L-7 | `stopped`, `exit`, and `spawn_failed` launches left unconfirmed are not probed after a restart. | §2.3 | their kill's survivors are in their evidence; the owner asserts with that. |
| L-8 | A `daemon_restart` launch the owner does not confirm gains one `probe` event per daemon start. | RD1 | each records an observation; the owner's assertion ends it. |
| L-9 | The daemon accepts the owner's assertion without any evidence. | owner decision | the decision is the owner's; the preview shows what exists, including that none does. |

## 6. Tests

**Per claim**: §5.2's test column. The prober's OS work goes through `SupervisorConfig.kill_recorded` (`KillRecordedFn`, `orphan::kill_recorded` outside tests), which unit tests script; its windows are held by `Pause::ProberStart` and `Pause::ProbeTask`; joins are made `Elapsed` or `Late` by the controlled clock (sup §6 "Host speed"). `orphan::unix_tests` script both the OS and the clock, so no test sleeps. Real-process tests decide by `process::lookup` at the instant the event appears (read through `GET /api/events` with one cutoff), never by a sleep; FakeCli fixtures take `--max-life-secs`, so a fixture that escapes still ends; native tests use the port the daemon published.

**The second-crash test** is PR10a2-CL-4: the crash after the reconcile commit and before the probe.

**The limit's documentation test** (not a claim, sol r3 #1): `supervisor_e2e::unrecorded_member_after_crash` (macOS; FakeCli `spawn-tree`, whose SIGHUP- and SIGTERM-ignoring sleeper stays in the session): after the restart's probe the evidence names `unrecorded_session_members`; the sleeper's fate is printed, not asserted, and the test ends it. Under RD9 (a) it records the unmet case; under (b) the limit.

**Windows CI.** `orphan::windows_tests`, every `supervisor::` test, `agent_cmd::` tests, and `supervisor_e2e::second_crash_before_probe` and `a29_child_meets_finder_conditions` run on `windows-latest` in the Cargo lane of `scripts/windows-tests.ps1`.

**Mutation check.** Before merging, each mutation of §5.2 is applied by hand, its test is seen failing, and the change is reverted; the PR description lists the result per claim.

**Completion commands**: sup §6's.

## 7. Quint

No guard changes. `confirmDeath` (`thread_input.qnt:1335-1339`: process `Exited`, `not(st.deathConfirmed)`) is the owner's assertion; `restart` (`:1327-1333`) is unchanged; the prober and `launch_evidence` change no state and stay outside the model. `trace_restart_and_resume` already runs restart, `reserveRefused`, `confirmDeath`, `reserve`.

New trace, written with `pnpm run traces:write` in the implementation PR: `trace_repeated_restart_then_assert` — `readyLaunch`, `restart`, `restart` (expect `Exited`, `deathConfirmed` false), `reserveRefused`, `confirmDeath` (expect `ResumeBlocked` cleared), `reserve(CLAUDE)`. It states RD1's case: a second restart changes nothing the assertion depends on.

Guard-to-test row added to sup §7's table:

| Model guard | Daemon site | Test |
|---|---|---|
| `confirmDeath`: exited, unconfirmed | the `stop` intent with `assume_dead` (§2.5) | PR10a2-CL-8 |

## 8. Proposed Canonical-Doc Changes

Not applied here.

- **mvp §2.1, event table**: the kind `launch_evidence` (§2.4); `launch_death_confirmed`'s method `owner_asserted`. **"Idempotency keys"**: process lifecycle `launch:<launch_id>:evidence:<spawn_failed|close|probe:<attempt>>`, the attempt a UUID the daemon mints.
- **mvp §2.4 / §2.5**: `StopArgs.assume_dead` (owner only, with `expected_launch_id`), `StopOutcome` `asserted`, `chaldea stop --assume-dead [--launch]` and the preview, the new `death_unconfirmed` text.
- **mvp §2.9 "Daemon restart"**: "`launch_death_confirmed { method }` follows once the old tree is proven dead, which the old-tree check and the owner's confirmation after a crash decide (PR 10a2 …)" → after each start the daemon signals the recorded leader of every unconfirmed `daemon_restart` launch by verified identity and records `launch_evidence`; such a launch's death is confirmed only by the owner (`owner_asserted`), after reading the evidence; the probe unlocks nothing.
- **mvp §4, PR 10 bullet**: "The restart scan of a launch's session by recorded sid with `launch.start_time ≤ start ≤ restart_now` is designed by 10a2" → the restart prober of `docs/design/supervisor-recovery.md` (identity-only, RD10).
- **mvp §6.1 P-18**, by the owner's RD9 choice; for (b): "… no surviving old process except the documented macOS containment limit (a `setsid` grandchild whose parent exited, spike §4.6) and, after a crash, processes of the old launch without a recorded identity (on macOS, members of the old session that survived the hangup; on Windows, A29's suspended child), which the launch's evidence reports as unchecked …"; for (a): unchanged.
- **sup §2.2** (the `death_unconfirmed` CLI text), **§2.4 "Unconfirmed launches"** ("10a1 has no rule that confirms such a launch" → the owner's assertion, `docs/design/supervisor-recovery.md` §2.5), **§2.12** (a pointer here), **§4** rows A1, A2, A28, A29, F1 (→ this document), **§5.3** L-1 and L-2 (→ this document's L-1, L-2, and RD9).
- **pty-runtime.md §2.2 and §4**: `orphan::kill_recorded`. **pty-unix.md §4 and pty-windows.md §4**, rows "Daemon restart": `orphan::kill_recorded`, a verified-identity loop with no engine rounds. **pty-windows.md §2.2 "Crash window"**: what PR 21's finder receives and its candidate conditions (§2.6).
- **event-store.md**: `launch_evidence` among the kinds whose projection is a no-op.
- **daemon-lifecycle.md §2.14**: `SupervisorComponents::attach` starts the restart prober, which holds no tracker token and stops when admission closes.
- **formal-specs §6**: 10a2's bullet names the assertion intent as `confirmDeath`'s site, no guard change, and the new trace.
- **AGENTS.md**: none.

## 9. Seed Findings

| Finding | Resolution |
|---|---|
| sol r3 #1 (P-18 survivor counted as success by CL-17) | RD9 lays out the options for the owner; PR10a2-CL-4 and CL-11 test only the recorded identities and the named scope; the survivor's fate is in a limit's documentation test, not a claim (§6); P-18's pass is judged by PR10a2-P-4 against the owner's option. |
| sol r3 #2a (a second crash drops recorded pids; the reconciler cannot hand a result to `attach`) | RD1 B: the prober reads candidates from the store after `attach` (§2.3 step 2), not from the reconciler; PR10a2-CL-3, PR10a2-CL-4 (the second crash after the reconcile commit). |
| sol r3 #2b (lock entry and slot lifetime; `Late`/`Elapsed`/join error unspecified) | RD2 B removes the probe lock (one prober, each launch once per start); RD3 moves the slot into the blocking task; §2.3 step 3.4 defines each join outcome; PR10a2-CL-5. |
| sol r3 #6 (evidence merges dead, alive, unobserved; `tree_dead` with unresolved cleanup shown as survived; empty survivors give empty `unchecked`) | RD4 A: §2.4's three classes, `gone` + `cleanup_unreleased` for a proven tree, `tree_unproven` whenever the owned kill did not prove; PR10a2-CL-7. |
| sol r3 bullet: server UUID and client `command_id` in one key namespace | RD5: no client id in any evidence key, since RD2 removes the `stop` probe. |
| sol r3 bullet: CL-17's fixture passes with the probe's signal removed | PR10a2-CL-4's control: the leader ignores HUP and TERM and is observed alive before the probe. |
| sol r3 bullet: A29 gate carries no pid; positive observation of the outer job; agent name vs resolved executable | `GateControl::wait_reached_pid`; `fake::TestJob` holds the helper, and the test observes the child in it; RD8 and §2.6 state the file-name match and that the path is not recorded; PR10a2-CL-10. |
| sol r3 bullet: record retry vs new OS probe | §2.3 "Retry rules"; PR10a2-CL-5 `record_retry`. |
| sol r3, owner decision note: assertion after a probe vs assertion allowed without one; the preview should show missing evidence | §2.5: the assertion needs no evidence (L-9); the preview prints when no probe evidence exists and every not-observed class; §8's mvp §2.9 text says "after reading the evidence", not "after the probe". P-18 is treated apart (RD9). |
| Neon #2 (prober only sees this start's launches) | RD1 B, as sol r3 #2a. |
| Neon #3 (preview pages to `launch_reserved`) | RD7: stop at `L`'s `launch_exited`; PR10a2-CL-9. |
| Neon #4 ("after" vs "not before" `recorded_at`) | §2.6 and PR10a2-CL-10: not before, at millisecond granularity. |
| Neon LOW (`recorded_gone` and evidence on `Late`/`Elapsed`/panic) | `recorded_gone` is not introduced (RD2); a probe's `Elapsed` or join error records `not_read` with `result_not_received`, and `Late` uses the report (§2.3 step 3.4). |
