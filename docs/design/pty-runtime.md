# Design: `crates/pty-runtime` (MVP PR 2)

Status: implemented in PR 2. Scope is the PR 2 row of `docs/mvp-design.md` §4 (cited as "mvp §N"): the `PtyRuntime` trait, `FakePty`, the single writer with its origin log, the drain, the VT responder, and the snapshot. Its design obligations: the writer records origin and position of every write; the VT responder replies in one write each; wide characters occupy two cells in snapshots.

**What the crate does and does not decide.** The crate records facts: the origin, order, byte span, and start and finish of every write; the terminal modes; the screen. Gate and ack policy (which writes close the gate or make an ack ambiguous, VT replies included) stay in the daemon and `core` (PR 10). The crate's guards only enforce conditions the daemon states per write.

Designed elsewhere: the macOS backend, `testkit`, and FakeCli (`docs/design/pty-unix.md`), the Windows backend (`docs/design/pty-windows.md`), the input encoders in `core::input` (PR 5), the gate, send path, and supervisor in the daemon (PR 10), and attach's repaint (PR 11). This document fixes what those need from this crate: what a backend must supply (§2.1) and what the daemon can rely on (§2.2–§2.6).

Sources read for this document (paths are relative to each root):

| Source | Revision | Read for |
|---|---|---|
| Spike [`spikes/pty-hooks/src/`](https://github.com/shuymn/chaldea/tree/03b00b8c0462c032f5f4a4f057feafa8e366d1b5/spikes/pty-hooks/src) | this repository | `pty.rs` (writer, gate), `vt.rs` (responder, snapshot), `os/drain.rs`, `os/mod.rs`, `paste.rs` (`win_keys` records), §4.4–§4.7 of `docs/spikes/pty-hooks-spike.md` |
| `alacritty_terminal` | crate 0.26.0 | `event.rs`, `term/mod.rs`, `term/cell.rs` |
| `vte` | crate 0.15.0 (alacritty's parser) | `src/ansi.rs` (synchronized updates) |
| `portable-pty` | crate 0.8.1 | `lib.rs`, `unix.rs` |
| herdr (`herdrdev/herdr`) | `e8d7d97` | `src/pty/actor.rs`, `src/pty/actor/unix.rs`, `src/pty/backend.rs`, `src/pane/terminal.rs` |
| Codex (`openai/codex`, sparse `codex-rs/utils/pty`) | `8653ca9` (default branch 2026-11-22) | `src/pty.rs`, `src/process.rs` |

## 1. Goal and Dependencies

**Goal.** A library crate, `chaldea-pty-runtime`, that turns a backend's raw PTY (a child process, an output read end, an input write end, a resize control) into a `Session`: one drain thread that reads output until EOF and feeds a headless `alacritty_terminal` VT, one writer thread that is the only code writing to the PTY and records every write with its origin and byte span, and one waiter thread that observes child exit. All of it is tested on macOS and Windows CI through `FakePty`, with no real PTY.

**Dependencies** (exact pins and their reasons are in the root and crate `Cargo.toml`). `alacritty_terminal =0.26.0` (default features off) is the version the spike ran on both hosts and the one prior-art row 3a and spike §3 read in source; its only optional feature (`serde`) is not needed. Its parser, `vte 0.15.0`, is reached through `alacritty_terminal::vte` (no direct dependency). `unicode-width =0.2.3` is the version alacritty locks, so it adds nothing to the build; `decode_grid` uses it to tell a wide character from a narrow one, since the grid encoding has no lead marker. `alacritty_terminal` also compiles its own `tty` module and that module's dependencies (`rustix-openpty`, `signal-hook`, `polling` on Unix; `miow`, `piper`, `windows-sys 0.59` on Windows); this crate never calls them. No `tokio` (D2) and no `portable-pty`; the backends' dependencies are in their documents' §1.

**Workspace and CI.** The `workspace` job runs this crate's tests on `macos-latest` and `windows-latest` and checks that `pty-runtime` has no `tokio` in its normal dependency tree (§7); `lefthook.yml` mirrors it.

## 2. Types and Functions

**Threading model.** The crate is synchronous: std threads, blocking I/O, no async runtime. PTY ends block on both OSes (the macOS master is nonblocking behind a reader and writer that block in `poll` and stop on close; ConPTY uses synchronous anonymous pipes), so an async runtime would only wrap them in blocking tasks, as Codex does (`utils/pty/src/pty.rs:241`, `:285`). Every callback the crate invokes (`Completion`, `OutputSink`, `on_event`) must return without blocking, and the crate never waits on a caller. The daemon bridges to tokio by passing callbacks that `try_send` on a tokio channel or complete a `oneshot`, and calls `close`, `kill`, and `resize` from `spawn_blocking`.

**Spawn lock.** Every child the daemon process creates is created under the process-wide spawn lock (`with_spawn_lock`, `src/spawn.rs`; `with_spawn_lock_before` waits for it only until a deadline and returns `None` when the lock was not taken by then, running nothing). On macOS it keeps the exec-error pipe and the PTY slave from leaking into a concurrent fork; on Windows the backend holds it around `CreatePseudoConsole`, which holds inheritable duplicates of conhost's pipe ends during the call, and around each `CreateProcessW`. A crate function that needs the lock takes `&SpawnGuard`, a guard only that lock produces; failure-path waits and releases run without it.

**Locks and callbacks.** Lock order: control lock → VT lock → queue lock; no code takes them in another order.

- *Control lock* guards `PtyControl`; only `resize`, close step 4, and the close's guard take it (the close with `try_lock` until its end).
- *VT lock* guards the `Term`, the parser, the subscriber list, and the output end. The drain thread holds it while it parses one piece of output.
- *Queue lock* guards both write lanes, the unit slot, the counters, and the ring.
- Modes are read from one atomic word (§2.6), without a lock.
- **No callback runs under an internal lock.** Code records the state change and takes the callbacks to fire while it holds a lock, then fires them after unlocking. This holds for completions (writer, `cancel`, close), sinks, and every `on_event`, including `ReplyDropped`. A callback may call the non-blocking `Session` methods (`submit`, `cancel`, `last_started`, `writes_after`, `modes`, `snapshot`, `subscribe`, `unsubscribe`, `status`; `snapshot` and `subscribe` only with a short deadline, which bounds both the lock attempt and the grid copy, so a callback never waits long) but not `resize`, `kill`, or `close`; a caller defers those to its own worker, since a sink runs on the drain thread and ConPTY's resize can wait for that thread. Values handed to the crate (completions, rechecks, sinks, `on_event`, and what they capture) must not panic when dropped and must not call `Session` methods from `Drop`, and a recheck or callback that panics must not panic with a payload whose `Drop` does either: the crate contains panics in callback calls, not in destructors.

### 2.1 Backend Seam: `PtyRuntime`

```rust
/// Starts a child in a new PTY. Sealed: implemented by `unix`, `windows`,
/// and `fake::FakePty`. Object safe; the daemon holds `Arc<dyn PtyRuntime>` and calls only
/// `Session::spawn`, since it cannot build a `DrainSink`.
pub trait PtyRuntime: Send + Sync + sealed::Sealed {
    fn spawn(&self, spec: &SpawnSpec, output: DrainSink) -> Result<SpawnedPty, SpawnError>;
}

pub struct SpawnSpec {
    pub argv: Vec<OsString>,                 // argv[0] is an absolute path to a native executable
    pub cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,      // the child's complete environment; nothing is inherited
    pub size: PtySize,
}

/// Validated: 2 ≤ cols ≤ 4095 (a wide pair fits), 1 ≤ rows ≤ 4095, so the pixel sizes the
/// responder reports (8×16 px cells) fit in u16.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PtySize { cols: u16, rows: u16 }
impl PtySize { pub fn new(cols: u16, rows: u16) -> Result<PtySize, SizeError>; pub fn cols(self) -> u16; pub fn rows(self) -> u16; }

/// The session's output sink (VT, responder, subscribers); opaque outside the crate.
pub struct DrainSink { /* private */ }

/// Internal owning guard for everything a spawn created; `Session::spawn` consumes it. Dropped
/// unconsumed (a failure inside `Session::spawn`) it runs the failure teardown:
/// kill_tree(&[], 5 s) → drop input → control.close(2 s) → drain finish(2 s).
pub struct SpawnedPty {                      // fields private; only backends build it
    child: Box<dyn PtyChild>,
    output: Drain,                           // started over the PTY's output read end, feeding `DrainSink`
    input: Box<dyn Write + Send>,            // the PTY's input write end
    control: Box<dyn PtyControl>,
    size: PtySize,                           // the size the PTY was opened with
    evidence: Vec<(&'static str, String)>,   // e.g. ("job_branch", "job_list"), ("conpty_module", …), ("job_list_error", …)
}

/// Methods take `&self` and may be called concurrently from different threads
/// (the waiter polls `wait_timeout` while `kill_tree` runs).
pub trait PtyChild: Send + Sync {
    fn identity(&self) -> &ProcessIdentity;
    fn wait_timeout(&self, timeout: Duration) -> io::Result<Option<ChildExit>>;
    /// Kills the whole tree plus the recorded descendants in `extra` (identities the daemon
    /// proves belong to the launch, never hook-emitter ancestry); idempotent; returns by `deadline`. The caller passes its
    /// full list of recorded descendants on every call, retries included; the report's
    /// survivors are diagnostic, never a list to resubmit. A kill-state lock another call
    /// holds is waited for only until `lock_by` (at most `deadline`); one held at the first
    /// try and obtained at or after `lock_by` counts as not taken: a busy report, no signal.
    fn kill_tree(&self, extra: &[ProcessIdentity], lock_by: Instant, deadline: Instant) -> KillReport;
}
pub trait PtyControl: Send {
    /// Resizes the PTY. Not bounded by the crate: ConPTY's resize can wait on conhost.
    fn resize(&self, size: PtySize) -> io::Result<()>;
    /// Releases the PTY itself (Unix master, Windows pseudoconsole); returns by `deadline`.
    fn close(self: Box<Self>, deadline: Instant) -> CloseReport;
}

pub struct ProcessIdentity { pub pid: u32, pub start_time: String, pub group: String }  // → launch_started { pid, start_time, pgid_or_job }
pub struct ChildExit { pub code: Option<i32>, pub signal: Option<i32> }               // → launch_exited { code, signal }
pub struct KillReport {                      // method → launch_death_confirmed
    pub method: String, pub tree_dead: bool, pub survivors: Vec<ProcessIdentity>,
    pub attempt: KillAttempt,                // what this call did to the tree
    pub proved_at: Option<Instant>,          // with tree_dead: the backend's clock read right after the proving read
}
impl KillReport { pub fn is_busy(&self) -> bool; }   // unix_busy, windows_busy, fake_busy
pub enum KillAttempt { Signalled, AlreadyDead, NotAttempted }   // tree_dead: true only with the first two
pub struct CloseReport { pub elapsed: Option<Duration> }                              // None: did not return by the deadline

pub enum SpawnError {
    /// The program provably never executed; any process created for it has been reaped:
    /// the confirmed `spawn_failed` of mvp §2.3. Resources created before it (a
    /// pseudoconsole and its drain) may still be unreleased.
    NeverStarted { kind: NeverStartedKind, error: io::Error, cleanup: Cleanup },   // NotFound | PermissionDenied | Setup
    /// A process existed and setup then failed. A Codex positional prompt may have run.
    StartedThenFailed { stage: &'static str, error: io::Error, identity: Option<ProcessIdentity>, cleanup: Cleanup },
}
pub enum Cleanup { Confirmed, Unresolved { kill: Option<KillReport>, close: Option<CloseReport>, drain_abandoned: bool } }
```

**What each backend must supply** (implemented by `docs/design/pty-unix.md` and `docs/design/pty-windows.md`):

- `spawn` returns either a `SpawnedPty` or an error after releasing everything it created: no process alive, no handle or descriptor open (spike F-2c). When it cannot prove that, the error carries `Cleanup::Unresolved`. Every release attempts the kill first and releases I/O even when the kill could not prove death (then `Unresolved`). Once a process exists, release follows `SpawnedParts::teardown`: kill the tree, drop the input write end, close the PTY while the drain reads, finish the drain. Before any process exists the order is the backend's (Windows: close while the drain reads, then input, drain, job; `docs/design/pty-windows.md` §2.2, spike §4.6).
- The PTY is opened at `spec.size`: the session initialises the VT and the responder's size from it before the drain starts.
- The child's environment is exactly `spec.env` (macOS: `Command::env_clear` then `envs`; Windows: an explicit `CREATE_UNICODE_ENVIRONMENT` block). The scrub list is the daemon's (mvp §2.7 "Env scrub"); it compares keys with `env_key_eq` and `env_key_has_prefix`, which compare as the backend's environment block does: `CompareStringOrdinal(…, TRUE)` over UTF-16 units on Windows (a prefix compares the key's first units), exact bytes on macOS.
- The backend starts a `Drain` (§2.4) over its output read end with the given `DrainSink`, so every output byte reaches the session's VT from the first one. Windows starts it right after `CreatePseudoConsole`, before `CreateProcessW` (spike §4.6); macOS after the PTY is opened and sized. The reader the drain owns returns `Ok(0)` once every slave end is closed: the Unix backend maps the master's `EIO` after hangup to EOF, and drops the parent's slave handle right after spawn (spike §4.7); on Windows conhost is the last writer of the output pipe and closes its end at `ClosePseudoConsole`, or earlier if conhost dies.
- **Dropping `input` writes no bytes.** portable-pty's writer writes `\n` + VEOF on drop (`unix.rs:351-361`); the macOS backend writes through its own handle on a dup of the master fd, and the Windows backend through the input pipe's write end; neither drop writes anything. With this obligation the session can release the input whenever the writer thread finishes, without an ordering hazard against a live CLI.
- `input` accepts each `write` call whole or fails: the macOS writer loops on a nonblocking master until every byte is accepted, and returns a short count only after progress followed by close or an error; Windows `WriteFile` on a synchronous pipe completes fully. A short write is tolerated by the writer (it loops) and recorded (§2.3).
- `PtyChild::wait_timeout` never blocks past `timeout` (Windows `WaitForSingleObject(timeout)`; macOS `waitid(WNOWAIT)` polling; the backend owns reaping) and must not hold a lock `kill_tree` needs.
- `kill_tree` is the PR 3 / PR 4 tree kill (`killpg` HUP→TERM→KILL plus `getsid` scan; job termination) applied also to every identity in `extra` that is still alive with the same start time, and returns by its deadline; `tree_dead: false` with survivors when it could not prove death. The caller passes its full list of recorded descendants on every call (the first stop, a retry after a busy report or `tree_dead: false`, and `close` after an earlier `kill`). What a backend keeps across calls is internal margin that the full-list rule makes invisible to the caller (`docs/design/pty-unix.md` §2.5, `docs/design/pty-windows.md` §2.5). `method` names how the kill ran: `unix_*` on macOS (`unix_busy` when another kill of the same child held the kill state past `lock_by`); `windows_job`, `windows_job_incomplete` (the job's member list may be incomplete), or `windows_busy` (another kill of the same child held the kill state past `lock_by`) on Windows, and `windows_process` for the process-only kill of a failed spawn. A busy call, and a call made at or after `deadline`, make no OS call. `attempt` is `Signalled` when this call signalled or terminated anything, `AlreadyDead` when the tree was proven dead without that, and `NotAttempted` otherwise (busy, expired, or, on macOS, ownership lost before any signal). `proved_at` is read right after the proving read, so a proof from the last observation started before the deadline can carry an instant past it.
- `PtyControl::close` returns by the caller's deadline even when the OS close blocks (Windows: `ClosePseudoConsole` on a close worker started before the pseudoconsole exists, since some builds block until output is read; `docs/design/pty-windows.md` §2.3). `CloseReport::elapsed` is `Some` when the close operation returned; a hung close shows as `elapsed: None` and `Released::control == false`.
- `evidence` carries what the doctor (deferred) and the real-host runs record (Windows: `job_branch` = `job_list` or `suspended`, `conpty_module`, and `job_list_error` when the job-list attempt failed and the suspended branch ran).
- Every `unsafe` block lives in `unix::sys` or `windows::sys` (D7).

### 2.2 `Session`

```rust
pub struct SessionConfig {
    pub on_event: Box<dyn Fn(SessionEvent) + Send + Sync>,   // non-blocking; each variant at most once, except ReplyDropped
    pub limits: Limits,                                       // defaults in §5
}
pub enum SessionEvent { Exited(ChildExit), WaitFailed(String), OutputEnded(OutputEnd), InputBroken(String), VtFailed, ReplyDropped }
pub enum OutputEnd { Eof, Error(String) }

impl Session {
    /// Builds the VT sink and the reply lane, calls `runtime.spawn(spec, sink)`, then creates the
    /// waiter and writer threads and hands the input to the writer. A thread-creation failure
    /// stops the threads already created, runs the guard's teardown, and returns `StartedThenFailed`.
    pub fn spawn(runtime: &dyn PtyRuntime, spec: &SpawnSpec, config: SessionConfig) -> Result<Session, SpawnError>;
    pub fn identity(&self) -> &ProcessIdentity;
    pub fn evidence(&self) -> &[(&'static str, String)];
    pub fn size(&self) -> PtySize;

    pub fn submit(&self, unit: WriteUnit, guard: Guard, done: Completion) -> Result<WriteTicket, SubmitError>;  // §2.3
    pub fn last_started(&self, classes: ClassSet) -> u64;               // §2.3 counters; 0 before any
    pub fn writes_after(&self, seq: u64) -> Result<Vec<WriteRecord>, LogTruncated>;   // diagnostic ring

    pub fn modes(&self) -> ModeView;                                     // one atomic load
    pub fn snapshot(&self, deadline: Instant) -> Result<Snapshot, VtUnavailable>;      // §2.6; screen().snapshot(deadline)
    pub fn screen(&self) -> Screen;                                      // the screen, copied without the session
    pub fn subscribe(&self, sink: OutputSink, deadline: Instant) -> Result<(Snapshot, SubscriptionId), SubscribeError>;
    pub fn unsubscribe(&self, id: SubscriptionId);
    pub fn resize(&self, size: PtySize) -> io::Result<Resized>;      // Resized::{PtyAndVt, PtyOnly}
    pub fn handle(&self) -> SessionHandle;                               // subscription and resize side, without the session

    pub fn status(&self) -> SessionStatus;                               // exit, output end, input state, vt_failed, ResponderStatus
    pub fn observer(&self) -> WriterObserver;                            // the writer's positions, outliving the session
    pub fn seal(&self) -> Vec<WriteRecord>;                              // close step 1 alone
    pub fn kill(&self, extra: &[ProcessIdentity], deadline: Instant) -> KillReport;   // kill_by(extra, deadline, deadline)
    pub fn kill_by(&self, extra: &[ProcessIdentity], lock_by: Instant, deadline: Instant) -> KillReport;   // tree kill only; I/O stays up
    pub fn close(self, extra: &[ProcessIdentity], bounds: CloseBounds) -> CloseOutcome;
    pub fn close_by(self, extra: &[ProcessIdentity], kill_deadline: Instant, bounds: CloseBounds) -> CloseOutcome;
    #[cfg(feature = "fake")] pub fn probe(&self) -> Probe;               // test signals (§2.7)
    #[cfg(feature = "fake")] pub fn fail_vt_on_next_piece(&self);
}
impl Drop for Session { /* close(&[], CloseBounds::default()) unless already closed */ }

/// A clone of the session's shared state (like `Screen` and `WriterObserver`): it outlives
/// the session, and every call then refuses or does nothing. The daemon takes it under a
/// short `with_session` and calls it on the blocking pool, never under the op lock.
impl SessionHandle {
    pub fn size(&self) -> PtySize;
    pub fn subscribe(&self, sink: OutputSink, deadline: Instant, grid_budget: impl FnOnce(&ScreenMeta) -> usize)
        -> Result<Subscribed, SubscribeError>;                          // Subscribed { id, meta, lines: Option<Vec<Line>> }
    pub fn unsubscribe(&self, id: SubscriptionId);                      // works after the seal too
    pub fn resize(&self, size: PtySize, fence: Option<&Fence>) -> Result<Resized, ResizeError>;   // Closed, FenceMoved, Failed
}

pub type OutputSink = Box<dyn FnMut(OutputEvent<'_>) -> Flow + Send>;   // non-blocking; may move between threads
pub enum OutputEvent<'a> { Bytes { offset: u64, bytes: &'a [u8] }, End(&'a OutputEnd) }
pub enum Flow { Continue, Stop }
pub fn channel_sink(capacity: usize) -> (OutputSink, OutputReceiver);   // one FIFO channel + byte budget
pub enum Received { Bytes { offset: u64, bytes: Vec<u8> }, End(OutputEnd), Lagged { at: u64 } }   // OutputReceiver::recv_timeout

pub struct Screen { /* the session's VT state */ }
impl Screen {
    pub fn snapshot(&self, deadline: Instant) -> Result<Snapshot, VtUnavailable>;   // as Session::snapshot; outlives the session
}

#[derive(Clone)] pub struct WriterObserver { /* the session's queue lock */ }
impl WriterObserver {
    pub fn last_started(&self, classes: ClassSet) -> u64;
    pub fn last_started_each(&self) -> [u64; 3];                 // per class (OriginClass::index), under one queue lock
    pub fn try_last_started_each(&self) -> Option<[u64; 3]>;     // None while another thread holds the queue lock
}
```

**Start.** The drain feeds the VT from the first output byte, so startup queries are parsed while `spawn` runs; their replies wait in the reply lane until the writer thread exists, and an EOF or read error before then is an ordinary `OutputEnded`. Until the input is handed to the writer, the guard owns every endpoint, so a thread-creation failure leaves no partially started writer owning the input; the tree is killed before any PTY end is released. The VT and the responder are initialised from `spec.size` before the backend spawns, so a startup `CSI 18 t` is answered correctly.

**Exit publication.** The waiter publishes the exit once, under the queue lock, before calling `on_event(Exited)`; the drain publishes the mode word under the queue lock too (VT lock → queue lock). Both facts are thus ordered with every write permission: an exit or mode change visible before a unit's first permission refuses it, and a submit triggered by `Exited` sees the exit and is refused.

**Resize.** Under the control lock: validate (`PtySize`), call `control.resize(size)` without the VT lock (ConPTY's resize can wait on conhost, which can wait on the drain; spike `pty.rs:385-399` also resized outside the VT lock), then on success take the VT lock and resize the grid and the size the responder reports. On failure neither side changes. Resize is not bounded and holds only the control lock, so a stalled resize blocks neither writes nor the drain; the daemon calls it off its runtime. After `VtFailed` there is no grid to resize: the PTY is resized and the call returns `Ok(Resized::PtyOnly)`; otherwise `Ok(Resized::PtyAndVt)`. Guarantee, stated in parse order: output parsed after the VT resize is laid out at the new size; a chunk read before the resize call may be parsed at either size. **Limit:** the drain keeps parsing while the backend resize runs, so bytes the CLI produced for the new size can be parsed at the old size (a wide glyph that does not fit with wrapping disabled is dropped from the grid). The snapshot is diagnostic, and the cell is correct again once the CLI next draws it. A resize between a unit's text and its submit is outside the crate's guarantee; the daemon orders resizes against sends with its launch lease (`docs/design/t1-attach.md` D7). **Permission.** `SessionHandle::resize` takes the control lock, then, under the queue lock immediately before the backend call, refuses `Closed` once the queue is sealed and `FenceMoved` when the caller's fence word moved; so a resize either got permission before the seal or is refused, and every leg of a nudge is checked alike. `subscribe` refuses `Closed` once the queue is sealed; `unsubscribe` always works.

**Subscribe.** A diagnostic snapshot plus the output bytes after its offset, with no gap and no duplicate. Under the VT lock (taken by `try_lock` until `deadline`, else `VtUnavailable`) the session takes the snapshot with its `output_offset`, registers the sink, and reads the output end; the drain publishes the end under the same lock, so a subscriber racing EOF gets exactly one `End` (from dispatch, or at once after registration when the output already ended). Under one VT lock hold the drain feeds a piece, advances `output_offset` past it, and takes the registered sinks; it dispatches the piece to them after releasing the lock, so a sink registered later gets whole pieces from its snapshot's offset on. It is not a replay checkpoint: the snapshot holds no pending escape sequence, rendition, or saved terminal state; attach's repaint is PR 11's design. At most `limits.subscribers` sinks; a sink that returns `Stop` or panics is removed and never called again. `channel_sink` sends byte chunks and the terminal item through one FIFO channel that never blocks the sender; a separate atomic budget bounds the queued bytes, so the terminal item always fits: when a chunk would exceed the budget it drops the bytes, sends `Lagged { at }` (the offset of the first dropped byte), and returns `Stop`; `End` is always sent. Its receiver yields every queued byte chunk, then `Lagged` if a byte was dropped, else `End`; `recv_timeout` applies one deadline to both taking the receiver and the receive.

**Seal.** `seal` is close step 1 alone: no unit is permitted after it; every unit not yet permitted is settled `NotStarted(Closed)` with zero bytes, its completion runs before `seal` returns, and `seal` returns their records. A later call, or the close's own step 1, settles nothing more and returns none.

**Kill and close by an instant.** `kill_by` passes `lock_by` to `PtyChild::kill_tree`: a busy report (`KillReport::is_busy`) means another kill held the kill state past `lock_by` and nothing was signalled. `close_by` is `close` whose kill step runs `kill_by(extra, at, at)`, with `at` the earlier of `kill_deadline` and the close's end, instead of a call bounded by `now + bounds.kill`, and whose reuse rule is the same full `extra` list: the last kill report is reused when it proved the tree dead for an identical list, empty or not. `close` reuses it only when `extra` is empty. The other steps keep `bounds`.

**Close order** (`close` and `Drop`). Every step has an absolute budget from `CloseBounds`, now + its bound capped at the close's end, or a day from now when the bound is too long for an `Instant` (no deadline sum panics: `total()` saturates, and neither a step nor the guard below adds past it); a step that runs out of budget is reported and the next step runs. One absolute end, `bounds.total()` from the close's start, covers every control-lock wait (step 4 and the guard), the backend close, and the cleanup; neither starts a budget of its own. A guard runs on every exit of the close, a panicking step included (e.g. a backend's kill): it stops the writer, stops the waiter, and closes the control by that end unless step 4 already took it, so the threads end and drop their references to the input, the child, and the PTY. On Windows the job's last handle closes with them, so kill-on-close ends the tree after a kill that panicked. On macOS nothing ends the tree after such a panic: closing the PTY master sends only `SIGHUP`, which does not guarantee the tree's death (supervisor L-8).

1. **Close the queue** (closed flag set under the queue lock): no further write call gets permission (§2.3); new `submit` calls fail `Closed`; every queued unit, and a unit in the slot that has not dispatched, is settled `NotStarted(Closed)`, a never-dequeued unit getting its `seq` then; their completions fire after unlocking. Settling never depends on the writer thread.
2. **Kill** `kill_tree(extra, kill)` (for `close_by`, with `kill_deadline` capped at the close's end as both instants) unless the last kill report proved the tree dead and `extra` is empty (for `close_by`: `extra` is that report's list).
3. **Release the input**: signal the writer and wait up to `input` for it to drop `input` and exit. A writer inside a permitted `write` call keeps `input` until that call returns; dropping writes no bytes (§2.1).
4. **Close the PTY**: the control is taken out of the control lock and `control.close(close)` runs after the lock is released, while the drain keeps reading (ConPTY is never closed under an undrained pipe). The lock is waited for with `try_lock` until the step's deadline (never past the close's end). When a resize still holds it then (inside its backend call), the close leaves the control to the resize and reports `released.control = false`: under one handoff lock, either the close takes the control after all or the resize, on releasing it, sees the seal and takes the control out, so exactly one of them closes it; the resize closes it after releasing both locks, so neither the close nor its guard waits for that backend close (`docs/design/t1-attach.md` I-1).
5. **Finish the drain** `Drain::finish(drain)`: wait for EOF or a read error and join the drain thread, else abandon it.
6. **Join and snapshot** within one deadline, `join` from the start of this step: the closer joins the writer until the deadline; if a dispatched unit is then still in the slot, the closer takes it and fires its completion as `Incomplete { error: "unknown_after_close" }` with the bytes known written so far (§2.3 write permission). If the VT lock is free when the step starts and the VT has not failed, `final_snapshot` is copied once with `try_lock`, the copy stopping at the `join` deadline (so it is `None` when the budget is already spent), else it is `None`. The waiter is joined in what remains of the deadline.

```rust
pub struct CloseBounds { pub kill: Duration, pub input: Duration, pub close: Duration, pub drain: Duration, pub join: Duration }
pub struct CloseOutcome {
    pub kill: KillReport, pub close: CloseReport, pub output: Option<OutputEnd>,
    pub released: Released,               // what is confirmed released, not just abandoned
    pub abandoned: Vec<&'static str>,     // threads still running: "pty-writer", "pty-drain", "pty-wait" (a hung ConPTY close shows in `close`)
    pub final_snapshot: Option<Snapshot>, // taken in step 6 only if the VT lock was free
    pub final_after_end: bool,            // true when the drain ended before the snapshot was taken
    pub elapsed: Duration,
    pub exit: Option<ChildExit>,          // the exit the waiter reported by the end of the close
}
pub struct Released { pub input: bool, pub control: bool, pub output: bool, pub tree_dead: bool }
```

`close` returns within `kill + input + close + drain + join`; the one snapshot copy runs inside the `join` deadline (a grid of at most 4095 × 4095 cells, far below the 1 s default for real sizes). Confirmed release is what `Released` says; an abandoned thread still owns its endpoint until its blocking call returns or the process exits. A snapshot taken by the daemon before `close` is pre-teardown; `final_snapshot` with `final_after_end` is the one taken after the output ended (closing ConPTY can be what lets the last output arrive).

**After child exit.** The writer refuses every unit that has not dispatched (`NotStarted(ChildExited)`, zero bytes) and stops a dispatched unit before its next `Bytes` part (§2.3). Queued VT replies are discarded (§2.5). The drain keeps reading until EOF, which can come later than exit while descendants hold the PTY. The session stays up until `close`; it never kills or closes on its own.

**Unowned kill: `orphan::kill_recorded`** (`docs/design/supervisor-recovery.md` §2.2). Outside any `Session`, `pub fn kill_recorded(identities: &[ProcessIdentity], deadline: Instant) -> OrphanReport` signals processes the caller does not own, by verified identity only, and reports one `Fate` per identity in input order, identical identities collapsed: `Gone`, `Replaced` (the pid holds another start), `Alive`, `Unreadable`, `NotRead`, `WaitFailed`, or `Invalid` (pid 0, the OS's reserved pid (1 on macOS, 4 on Windows), the caller's own pid, a pid above the OS's range (`i32::MAX` on macOS), an empty or unparsable start; never read or signalled). No native call starts at or after `deadline`: the clock is read before each, and sleeps and waits are clipped to it. macOS (pty-unix §2.5): a signal goes to a pid only right after a read of it showed the recorded start, in rounds `SIGHUP`, `SIGTERM`, `SIGKILL` on the engine's timing, `SIGKILL` alone for a budget of 100 ms or less; it targets no group or session. Windows (pty-windows §2.5): one `open_verified` per identity over `ProcessOs`, every held handle terminated before any wait. Other platforms report `NotRead`. Its report is an observation, never a death proof of a launch.

### 2.3 Single Writer and Origin Log

One writer thread per session is the only code that writes to `input`. Callers submit **units**; nothing else is written to the PTY while a unit is in progress, so a unit's bytes are one contiguous span of the input stream.

```rust
pub struct WriteUnit { pub origin: WriteOrigin, pub parts: Vec<Part> }
pub enum Part {
    Bytes(Vec<u8>),          // non-empty; one write call on whole acceptance; the encoder keeps every escape sequence inside one part
    Pause(Duration),         // between two Bytes parts only
}
// Shape, checked at submit: 1–8 parts, first and last part Bytes, no empty Bytes, no two
// adjacent Pauses, total pause ≤ limits.max_pause, total bytes ≤ limits.max_unit_bytes.

pub enum WriteOrigin {
    Input { input_id: InputId },            // send path (PR 10): text, pause, submit as one unit
    Attach,                                 // the attach WebSocket (PR 11)
    Key,                                    // owner `key`, keypad (PR 10, 19); `surface_action send_key` (PR 20, deferred)
    CardKey { approval_id: ApprovalId },    // key from an open approval card (mvp §8 D1)
    VtReply { kind: ReplyKind },            // the VT responder (§2.5); never submitted by callers
}
pub enum OriginClass { Input, Human, Reply }          // Attach, Key, CardKey are Human
pub struct ClassSet(u8);                               // ClassSet::ALL, ::of(&[..])
impl WriteOrigin {
    pub fn class(&self) -> OriginClass;
    /// Attach → attach, Key → key, CardKey → card_key; Input and VtReply → None.
    pub fn human_write(&self) -> Option<HumanWriteOrigin>;
}

pub struct Guard {
    pub not_after: Instant,                    // dispatch deadline: refuse if the first write call has no permission by then
    pub basis: Option<Basis>,                  // refuse if a counted write started after the basis
    pub bracketed_paste: Option<u64>,          // refuse unless 2004 is on with this epoch (ModeView)
    pub recheck: Option<Recheck>,              // the daemon's gate / card recheck
    pub fence: Option<Fence>,                  // refuse unless the caller's word still holds the value it read
}
pub struct Fence { pub word: Arc<AtomicU64>, pub expect: u64 }   // advanced by the caller on events the writer cannot see
pub struct Basis { pub seq: u64, pub counted: ClassSet }   // a last_started value the daemon captured; counted classes are its policy
pub type Recheck = Box<dyn FnOnce(&WriteContext) -> Result<(), String> + Send>;
pub struct WriteContext { pub seq: u64, pub modes: ModeView, pub last_started: [u64; 3] }  // per OriginClass

pub type Completion = Box<dyn FnOnce(&WriteRecord) + Send>;            // non-blocking; called exactly once
pub fn completion_channel() -> (Completion, CompletionReceiver);        // recv_deadline(Instant)
pub struct WriteTicket { /* … */ }
impl WriteTicket {
    pub fn cancel(&self) -> bool;                     // true: never dispatches; completes Cancelled
    pub fn try_cancel(&self) -> Option<bool>;         // cancel without waiting: None while the queue lock is held
    pub fn started_at(&self) -> Option<Instant>;      // the first write permission's instant; no queue lock
}

pub struct WriteRecord {
    pub seq: u64,                    // per session, from 1, gapless: assigned at dequeue, or at settling for a never-dequeued unit
    pub origin: WriteOrigin,
    pub submitted_at: Instant, pub started_at: Option<Instant>, pub finished_at: Option<Instant>,
    pub byte_start: u64,             // input bytes written before this unit
    pub bytes_written: u64,          // the unit's span is byte_start..byte_start + bytes_written
    pub split_writes: u32,           // write() calls that accepted less than their part
    pub outcome: WriteOutcome,       // InProgress until finished
}
pub enum WriteOutcome { InProgress, Written, NotStarted(Refusal), Incomplete { error: String } }
pub enum Refusal { Cancelled, Expired, Closed, ChildExited, InputBroken, BasisUnknown { basis: u64, head: u64 },
                   WriteSinceBasis { seq: u64 }, BracketedPasteChanged, FenceMoved, Recheck(String), RecheckPanicked }
pub enum SubmitError { Closed, QueueFull, BadShape(&'static str), UnitTooLarge, ReplyOrigin }
pub struct ModeView { pub modes: TermModes, pub bracketed_paste_epoch: u64 }
```

**Writer loop and progress.** The writer serves two lanes: replies (§2.5) and user units (FIFO). At each unit boundary it takes the number of replies queued at that moment, writes exactly that batch, then dequeues one user unit. So a user unit is dequeued after at most one reply batch, and a reply queued after a batch was sampled is eligible in the next batch. No reply is written inside a unit.

**One slot from dequeue to finish.** The writer moves a dequeued unit into the take-once slot and assigns its `seq` there, before any check. A unit cancelled or closed while still queued gets its `seq` under the queue lock when it is settled; `last_started` does not move. Close and the writer settle the slot's completion only by taking the unit out of the slot under the queue lock, so exactly one of them fires it (§2.2 steps 1 and 6).

**Write permission.** A write call is committed when it obtains permission under the queue lock; the call itself runs after unlocking. Close stops new permissions; a call already permitted may still reach the backend, even after `close` returns. **Before the first byte** the writer checks, in this fixed order, and on the first failure records `NotStarted(reason)` with zero bytes: queue closed; child exited; input broken; `not_after` passed (`Expired`); `basis`; `bracketed_paste` (2004 off, or its epoch differs, gives `BracketedPasteChanged`); `fence` (`word` no longer equals `expect` gives `FenceMoved`); then `recheck`, run outside the queue lock inside `catch_unwind`, with every earlier unit finished and nothing else writing. **Dispatch boundary:** the first write call's permission. Under the queue lock the writer re-checks that the unit is still in the slot, the queue is open, and the exit, input-broken, mode-word, and deadline facts are unchanged (the writer compares the mode word it observed before the recheck with the one it observes at the boundary; any difference refuses, not only a 2004 change, since the recheck decided on the modes in its `WriteContext`, while a toggle that restores the word in between is no change: `BracketedPasteChanged` when the unit has a `bracketed_paste` guard and 2004 moved, else `Recheck("modes_changed")`), and, as the last check, that the `fence` word still equals `expect` (`FenceMoved`); it then marks the unit dispatched, stores the permission's instant on the ticket (`started_at`, readable without the queue lock), advances `last_started` for its class, and grants the permission. The writer thread's exclusive ownership of `input` is the arbitration lock of mvp §2.3: a recheck that passes is followed by the unit's first byte with no write of any origin in between. Tests assert the order only where a consumer depends on it (`ChildExited` before every other check except `Closed`; `Expired` before `Recheck`).

**Basis.** `Basis::seq` is a `last_started(counted)` value the daemon captured when it recorded the readiness evidence the send relies on (mvp §4 PR 10). The writer refuses `WriteSinceBasis` if `last_started(counted)` at the dispatch boundary is above `basis.seq`, and `BasisUnknown` if `basis.seq` is above the highest `seq` assigned before this unit (a basis from another session). The candidate's own class counter advances only at its own dispatch, so it is never compared with itself. With the default `counted = ClassSet::ALL`, a startup reply before readiness is inside the basis and a reply after readiness refuses the send. A write that happens between the CLI emitting the readiness hook and the daemon's intake of it is ordered by intake: this is the residual constraint of mvp §2.3, not closed here.

**Started units.** A dispatched unit is committed: `not_after` no longer applies (herdr: "committed Enter cannot expire", `src/pty/actor.rs:698-713`). Every later write call (a later `Bytes` part, or the rest of a short accept) needs its own permission, granted only while the queue is open and the child has not exited; otherwise the writer records `Incomplete { error: "closed" | "child_exited" }`; a call permitted before close or exit may still complete (§2.3 write permission). During a `Pause` the writer waits on the queue condvar until the pause ends, the queue closes, or the child exits. An I/O error, or `Ok(0)` for a non-empty part, ends the unit as `Incomplete` with the bytes written so far and marks the input broken: every later unit is refused `InputBroken` and `on_event(InputBroken)` fires once.

**Short writes.** If a `write` call accepts less than its part, the writer writes the rest in further calls and increments `split_writes`. The backends guarantee whole acceptance (§2.1), so a nonzero `split_writes` is a backend defect made visible.

**Position and the origin log.** `seq` orders all records of a session, dispatched or refused. A unit's span is contiguous, and the spans of dispatched units, in `seq` order, partition the input stream exactly, except that an `unknown_after_close` or `unknown_after_panic` record holds only its confirmed prefix and its last permitted call may have written more (PR2-CL-1). Three monotonic counters, `last_started(class)` for `Input`, `Human`, `Reply`, advance at a unit's dispatch, before its first write call; so a hook that arrives while a human write is blocked already sees it. They are independent of the ring. Records also go to a ring of `limits.log_records` entries; `writes_after(seq)` returns a copy of the records after `seq` (the in-progress one included), or `LogTruncated`. The ring is diagnostic: no guard and no ack rule depends on it.

**How the daemon uses it** (PR 10 and PR 11 decide the details; these are the facts they get):

- **Send path** (`docs/design/supervisor-send.md` §2.3). Store `last_started(counted)` with each readiness evidence at its intake; admit with `Basis { seq: that value, counted }`, where `counted` is the adapter's policy (`ClassSet::ALL` for every adapter, keeping mvp §2.3's "last write of any origin" literal); `bracketed_paste` for bracketed paste; and a `fence` on the launch's hook-intake word, so a hook taken in before the first write permission refuses the unit. The send has no `recheck`: the gate is decided in the store job that moves the input to `sending`, and the fence covers what arrives after it. Submit `WriteUnit { Input, [text, Pause(submit delay), submit] }`.
- **Outcome mapping** to `core::input::InputInput`: `Written` → `WriteCompleted`; `NotStarted(_)` and every synchronous `SubmitError` → `RecheckFailed` (known zero bytes, so `cancelled`); `Incomplete` → `WriteOutcomeUnknown`. A completion that arrives after `launch_exited` already moved the input to `unconfirmed` is ignored. A unit with no `started_at` well past its `not_after` (queued behind a blocked unit) is cancelled with `try_cancel`; a unit with `started_at` and no record `WRITE_STALL` (30 s) after it shows its input `unconfirmed`, and the next unit waits for its record.
- **Ack attribution.** At hook intake the daemon reads `last_started` for the input's class and for `counted`. The hook is a candidate for an input only if the input had dispatched by then (`last_started(Input) ≥ input.seq`); a counted unit with `seq` in `(input.seq, last_started(counted)]` makes it non-attributable. A Codex positional prompt has no unit: its anchor is the launch's published `last_started`, and only `Input` and `Human` writes above it make the hook non-attributable; `Reply` writes do not.
- **`human_write`.** The completion of an `Attach`, `Key`, or `CardKey` unit carries the record; `origin.human_write()` gives the payload's origin. A partial human write (`Incomplete` with `bytes_written > 0`) is still a human write.
- **Resize during a unit** is ordered by the daemon's launch lease (t1-attach D7): no resize runs while a send's unit is opened or running.

**Bounds.** `submit` never waits for queue space or PTY I/O: past `limits.queue_units` units or `limits.queue_bytes` bytes it fails `QueueFull` with nothing written and no record. A unit with a bad shape, over `limits.max_unit_bytes`, or with origin `VtReply` fails at submit.

### 2.4 Drain

Lifted from the spike's [`os/drain.rs`](https://github.com/shuymn/chaldea/blob/03b00b8c0462c032f5f4a4f057feafa8e366d1b5/spikes/pty-hooks/src/os/drain.rs) without its hold: the drain owns the output read end, reads until EOF or a read error on every path, passes every chunk to the session's sink from the first byte, and reports how it ended (`OutputEnd::Eof` or `Error`, never confusing the two). The spike held output until a consumer attached and forwarded it through an unbounded channel (`drain.rs:93`, `:157-176`); here the sink exists before the backend spawns, so neither is needed.

```rust
pub(crate) struct Drain { /* thread "pty-drain" */ }
pub(crate) enum DrainEvent<'a> { Bytes { offset: u64, bytes: &'a [u8] }, End(&'a OutputEnd) }
impl Drain {
    pub(crate) fn start(output: impl Read + Send + 'static, sink: DrainSink) -> io::Result<Drain>;   // on failure `output` is dropped first
    pub(crate) fn bytes(&self) -> u64;
    pub(crate) fn finish(self, bound: Duration) -> Option<OutputEnd>;              // waits and joins; None: thread abandoned
}
```

**Guarantees.** (a) Every byte read is passed to the sink once, in read order, then the end. (b) The sink splits each chunk before every ESC byte, feeds each piece to the VT under the VT lock and publishes the mode word after it (§2.6), then releases the lock and dispatches the piece to subscribers. VT feeding, each subscriber call, and every `on_event` call made on the drain thread (`ReplyDropped`, `VtFailed`, `OutputEnded`) run inside `catch_unwind`; a panicking `on_event` is ignored; a VT panic sets `vt_failed` (snapshots return `VtUnavailable`, the responder stops, `on_event(VtFailed)` fires) and reading continues; a subscriber panic or `Stop` removes that subscriber. A subscriber that calls `subscribe` or `unsubscribe` from inside its callback has the change applied after the current dispatch. (c) The drain never blocks on a consumer and keeps reading after the session's consumers are gone, until EOF, so a child is never blocked on a full output pipe. (d) `finish` waits at most its bound for the end after `control.close`, then joins or abandons the thread.

### 2.5 VT Responder

Lifted from the spike's [`vt.rs`](https://github.com/shuymn/chaldea/blob/03b00b8c0462c032f5f4a4f057feafa8e366d1b5/spikes/pty-hooks/src/vt.rs): `Term<Responder>` with `Config { kitty_keyboard: true, scrolling_history: 0, .. }`, fed by the drain thread through a `vte::ansi::Processor<NoSync>`. `NoSync` implements vte's `Timeout` trait with `pending_timeout()` always false, so the parser never buffers a synchronized update (`CSI ?2026h`): it would otherwise hold every action until `CSI ?2026l` or a 150 ms timeout (`vte 0.15.0 src/ansi.rs:36`, `:298-311`, `:478-488`, `:1606-1607`), hiding 2004 transitions and queries inside it. The `EventListener` turns each reply event into one reply write of origin `VtReply { kind }`:

| Query | alacritty event | Reply | `ReplyKind` |
|---|---|---|---|
| DA1 `CSI c` / DA2 `CSI > c` | `PtyWrite` (`term/mod.rs:1257-1271`) | `ESC[?6c` / `ESC[>0;<ver>;1c` | `Da1`, `Da2` |
| DSR 5 / DSR 6 | `PtyWrite` (`:1332-1347`) | `ESC[0n` / `ESC[<r>;<c>R` | `Dsr5`, `Dsr6` |
| kitty keyboard `CSI ? u` | `PtyWrite` (`:1275-1284`) | `ESC[?<flags>u` | `KittyKeyboard` |
| DECRQM private and ANSI | `PtyWrite` (`:2090`, `:2147`) | `ESC[?<m>;<s>$y` / `ESC[<m>;<s>$y` | `ModeReport` |
| text area size in cells `CSI 18 t` | `PtyWrite` (`:2268-2270`) | from the VT size | `TextAreaChars` |
| text area size in pixels `CSI 14 t` | `TextAreaSizeRequest` (`event.rs:43`, `term/mod.rs:2259`) | VT size × 8×16 px cells | `TextAreaPixels` |
| OSC 10 / 11 / 12 / 4 color | `ColorRequest` (`event.rs:37`, `term/mod.rs:1675-1688`) | fg `#d0d0d0`, bg `#1e1e1e`, cursor = fg, palette = fg | `Color { index }` |

**Limits of the contract.** OSC 52 clipboard reads (`ClipboardLoad`, `event.rs:31`) and stores get no reply and no effect: the daemon has no clipboard to give a CLI. Colors and pixel sizes are nominal. Character widths are the pinned emulator's. On Windows, conhost answers DSR and DA itself and swallows OSC 10/11; the responder answers what reaches it (kitty), and replies conhost writes are outside this crate's ordering and log (prior-art row 3b). Every other event is ignored.

**One reply per write.** Each event becomes its own reply write of exactly that reply: one `write` call on whole acceptance and one record; replies are never merged with each other or with other bytes (mvp §4 PR 2 obligation; conhost loses a sequence split across writes, arch §5).

**Ordering.** Replies are enqueued while the piece that caused them is parsed, into the reply lane (FIFO, at most `limits.reply_queue` replies), and written in batches at unit boundaries (§2.3): in query order, never inside a unit. Replies have no guard. A reply that does not fit the lane is discarded and counted (`ResponderStatus::dropped_overflow`) and `on_event(ReplyDropped)` fires after the VT lock is released, since a CLI may be waiting for it. Once child exit or input breakage is observed, queued and new replies are discarded and counted (`dropped_after_end`). A discarded reply gets no record. `ResponderStatus::dispatched` counts replies given their write permission; whether one reached the backend is in its record.

### 2.6 Snapshot

```rust
pub struct Snapshot {
    pub cols: u16, pub rows: u16,
    pub lines: Vec<Line>,             // exactly `rows` lines of the visible screen
    pub cursor: Cursor,               // { row, col } zero-based in cells, visible: SHOW_CURSOR
    pub modes: TermModes,
    pub output_offset: u64,           // output bytes fed into the VT when it was taken
}
pub struct Line { pub cells: Vec<Cell> }          // exactly `cols` cells
pub struct Cell { pub text: String, pub width: CellWidth }   // text non-empty except for WideTail
pub enum CellWidth { Single, WideLead, WideTail }
impl Line { pub fn text(&self) -> String; }      // cells without WideTail, right-trimmed (the spike's `lines`)
impl Snapshot { pub fn to_payload(&self) -> VtSnapshotTaken; }
pub fn decode_grid(grid: &str, cols: u16, rows: u16) -> Result<Vec<Line>, GridError>;   // the reader-side validation, for tests and the CLI

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermModes {
    pub bracketed_paste: bool,        // BRACKETED_PASTE (DECSET 2004): the gate, VtModes
    pub alt_screen: bool,             // ALT_SCREEN (1049): VtModes
    pub show_cursor: bool,            // SHOW_CURSOR (DECTCEM): VtCursor.visible
    pub app_cursor: bool,             // APP_CURSOR (DECCKM): arrow encoding for keypad and card keys
    pub app_keypad: bool,             // APP_KEYPAD (DECKPAM)
    pub kitty_flags: u8,              // DISAMBIGUATE_ESC_CODES … REPORT_ASSOCIATED_TEXT as 5 bits
}
```

**Walker** (rewrite of `vt.rs:234-268`). For each visible row and column of `term.grid()`: a cell with `WIDE_CHAR` followed by a `WIDE_CHAR_SPACER` is `WideLead` with `cell.c` plus its `zerowidth()` chars, and that spacer is its `WideTail` with empty text; every other cell is `Single` with `cell.c` plus zero-width chars, or `" "` when that is empty. An orphan flag, a `WIDE_CHAR` without its spacer or a `WIDE_CHAR_SPACER` without its lead (left by an erase of one half, `term/mod.rs:1519`, or a shrink), becomes a `Single` `" "`, as does a `LEADING_WIDE_CHAR_SPACER` (the last column left blank when a wide character wrapped, `term/cell.rs:22-28`). So a wide character occupies exactly two cells, every line has `cols` cells, and column positions after it are exact. Width is alacritty's: an emoji with VS16 (`❤️`) stays one cell (spike §3).

**Mode word.** `TermModes` (10 bits) and `bracketed_paste_epoch` (54 bits) are packed into one `AtomicU64`, so `modes()` is one tear-free load. The drain splits each chunk before every ESC byte and publishes the word after parsing each piece; with `NoSync` every action is applied as it is parsed, so the epoch increments on every change of `bracketed_paste`, including two within one read or inside a synchronized update.

**No scrollback.** The VT keeps no history (`scrolling_history: 0`): only the visible screen is read, and full history (10 000 rows) would cost about 28 MiB per 120-column session. Growing the height adds blank rows below the visible content instead of pulling history back.

**Snapshot budget.** `snapshot(deadline)` and `subscribe(sink, deadline)` take the VT lock with `try_lock` until `deadline` and copy the grid, checking the deadline before each row; past the deadline, during the wait or the copy, or after a VT panic, they return `VtUnavailable`. `SessionHandle::subscribe` also takes a byte budget for the copy (t1-attach D12): the walk checks it per cell, the cell's text with every zero-width character included, while holding the VT lock; over it the copy stops and the subscription is registered as usual with the screen's facts, no cells, and the same `output_offset`.

**Grid encoding (D9).** `to_payload` gives `VtSnapshotTaken { grid, cols, rows, cursor: VtCursor { row, col, visible }, modes: VtModes { bracketed_paste, alternate_screen } }` with `grid = Content::text(json)`. The deferred A2UI `vt_snapshot` component would carry the same encoding under the same rules.

- The JSON is an array of exactly `rows` arrays, one per screen row, top to bottom; each holds at most `cols` strings, one per cell, left to right.
- `""` means a `WideTail` and nothing else; it must directly follow a `WideLead`. A `Single` or `WideLead` string is never empty; a blank cell is `" "`.
- Trailing cells that are `Single` `" "` are omitted; a reader fills them back to `cols`. A trailing `WideTail` is never omitted.
- `cursor.row` and `cursor.col` are zero-based cell coordinates in that grid.
- A reader rejects, as an unreadable snapshot, a grid that is not this shape: a row count other than `rows`, a row longer than `cols`, a non-string, an empty string not preceded by a lead, a row ending in a lead without its tail.
- A grid above 64 KiB is stored as a blob (mvp §2.1); a reader fetches the blob before decoding, since the 1 KiB preview can be cut mid-JSON and is never decoded.
- Example at `cols: 5`, `rows: 2`: the screen `a日b` on row 0 and an empty row 1 is `[["a","日","","b"],[]]`; a blank row 0 with `x` at column 2 is `[[" "," ","x"],[]]`.

### 2.7 `FakePty`

Behind the `fake` feature. A `PtyRuntime` whose child is a script, so the shared drain, writer, VT, and close order run unchanged in tests on both OSes and the daemon's gate and writer tests (PR 10) need no real PTY. Output travels through `std::io::pipe()`, so the drain reads a real pipe and EOF is real.

```rust
pub struct FakePty { /* Arc state */ }
impl FakePty {
    pub fn new() -> FakePty;
    pub fn script_next(&self, s: FakeScript);                     // applies to the next spawn
    pub fn spawns(&self) -> Vec<SpawnSpec>;                      // every spec, in order
    pub fn take_child(&self) -> Option<FakeChild>;               // the controller of the latest spawn
}
pub struct FakeScript {
    pub fail: Option<SpawnError>,                                // spawn returns this
    pub startup_output: Vec<u8>,                                 // written before spawn returns; spawn returns after the sink parsed it
    pub kill: KillBehavior,                                      // ExitOnKill (default) | Ignore | ExitAfter(Duration) | Unconfirmed | ProveOnCall(n) | Panic
    pub hang_control_close: bool,                                // control.close blocks until its deadline, CloseReport { elapsed: None }
    pub hold_waiter: bool,                                       // wait_timeout blocks until FakeChild::release_waiter
}
pub struct FakeChild { /* … */ }
impl FakeChild {
    pub fn emit(&self, bytes: &[u8]);                            // child output (blocks only on a full pipe)
    pub fn on_input(&self, f: impl FnMut(&[u8]) -> Vec<u8> + Send + 'static);  // scripted reaction, e.g. echo or queries
    pub fn close_output(&self);                                  // EOF
    pub fn fail_output(&self, kind: io::ErrorKind);              // sets the error, then writes a wake byte the reader swallows
    pub fn exit(&self, e: ChildExit);
    pub fn fail_wait(&self);                                     // every later wait_timeout fails: WaitFailed, no exit
    pub fn release_waiter(&self);                                // ends hold_waiter's block
    pub fn hold_kill_lock(&self) -> KillLockHold;                // holds the kill state until released or dropped
    pub fn signals(&self) -> u32;                                // signal-equivalent operations so far
    pub fn hold_writes(&self) -> WriteHold;                      // the next write() blocks until released or dropped
    pub fn fail_writes_after(&self, bytes: u64, kind: io::ErrorKind);
    pub fn short_writes(&self, max: usize);                      // write() accepts at most `max` bytes per call
    pub fn panic_on_next_write(&self);
    pub fn fail_next_resize(&self);
    pub fn hold_resizes(&self) -> ResizeHold;                    // the next backend resize parks until the hold drops
    pub fn writes(&self) -> Vec<Vec<u8>>;                        // every write() call's bytes, in order (call boundaries kept)
    pub fn input(&self) -> Vec<u8>;
    pub fn log(&self) -> Vec<FakeEvent>;  // Spawned, WriteEntered, Write(n), Resize(size), ResizeFailed, Kill { extra }, Signal, InputDropped, ControlClosed, OutputClosed, Exited
}
impl WriteHold { pub fn wait_entered(&self, deadline: Instant) -> bool; pub fn release(self); }
impl OutputReceiver { pub fn hold(&self) -> impl Sized + '_; }   // holds the receiver's side; the sink must still make progress

pub struct Probe { /* … */ }                                    // Session::probe()
pub enum ProbePoint { ReplyEnqueued, PauseEntered, RecheckEntered, Permitted, PieceParsing, Dispatching }   // Permitted: after a write permission, before the backend call; PieceParsing: VT lock held; Dispatching: queue lock held, every check but the final deadline check passed, before the first permission
impl Probe {
    pub fn wait(&self, p: ProbePoint, n: u64, deadline: Instant) -> bool;   // the n-th occurrence was reached
    pub fn park(&self, p: ProbePoint) -> Park;                              // the next occurrence blocks until the Park is released
}
```

`SessionConfig` has a `#[cfg(feature = "fake")]` start fault (`StartFault::Writer`, `StartFault::Waiter`, failing that thread's creation). Identity: `pid` from a counter, `start_time` `fake-<n>`, `group` `fake-<n>`. `kill_tree` records `Kill { extra }` on every call; an expired call or a dead child then signals nothing (`NotAttempted` or `AlreadyDead`), and the deadline is checked again right before the signal, so a call that reaches it at or after its deadline also sends nothing (`NotAttempted`); a kill-state lock held by `hold_kill_lock` past `lock_by` gives `fake_busy`; otherwise it records `Signal` exactly where a native backend would signal and reports `method: "fake"` (`fake_unconfirmed` for `Unconfirmed`) and `tree_dead` per `KillBehavior`: `Unconfirmed` returns at once with no signal, `ProveOnCall(n)` returns `Signalled` and `tree_dead: false` before the `n`-th signal and proves the tree dead on it, and `Panic` panics inside the call. `WriteHold::wait_entered` and `Probe` are the deterministic signals that a thread is at a given point; `FakeEvent` order is what the close-order tests assert.

## 3. Structure Options

| Fork | Option | Precedent | Verdict |
|---|---|---|---|
| **Backend dispatch** | A. `cfg`-selected free functions, no trait | herdr `src/pty/backend.rs:1-60` | Rejected: the daemon could not swap in `FakePty`, so its gate tests would need a real PTY. |
| | B. Reuse portable-pty's `MasterPty` / `Child` traits as the seam | spike `os/mod.rs:203-218` plugs the raw ConPTY backend in this way (`os/windows.rs:1524`, `:1615`) | Rejected: `anyhow` errors, no tree kill, no start time, single-pid `kill`, VEOF on writer drop (spike §3). |
| | C. A sealed, object-safe `PtyRuntime` returning an owning `SpawnedPty`; everything above it shared | portable-pty `PtySystem` behind `native_pty_system() -> Box<dyn PtySystem>` (`lib.rs:252-256`, `:389`) | **Adopted.** One writer, drain, and VT for all backends; the trait carries only what differs per OS. |
| **Trait level** | A. The trait is the full session (`spawn`, `write_text`, `write_key`, `snapshot`, `attach`, `kill`, arch §4.5); each backend implements all of it | — | Rejected: the writer, VT, and drain would be written three times, and FakePty tests would not exercise the production ones. |
| | B. Trait = raw spawn; `Session` is one concrete type | Codex `spawn_process` returns one `ProcessHandle` for every OS (`utils/pty/src/pty.rs:150`, `process.rs:111-150`) | **Adopted.** arch §4.5's operations are `Session` methods (`submit` with an origin covers `write_text` and `write_key`; `subscribe` serves attach). |
| **Writer** | A. A `Mutex` around the writer; each caller writes on its own thread | — | Rejected: the drain thread would block on a write to a child that stopped reading, stop reading, and deadlock with it (spike B-10, 64 KiB paste); std `Mutex` is not FIFO. |
| | B. An async writer task holding the writer | Codex: tokio task, blocking `write_all` under a tokio `Mutex` (`pty.rs:259-277`) | Rejected: a blocked write stalls a runtime worker; it needs tokio in the crate. |
| | C. A dedicated writer thread with queues | spike `pty.rs:161-234`; herdr Windows writer thread (`src/pty/actor.rs:229-232`, `:313-345`) | **Adopted**, with a bounded user lane (herdr bounds its input channel and fails fast when full, `actor.rs:87-115`) and per-unit guards checked before the first byte (spike `pty.rs:187-205`). |
| | D. One thread doing nonblocking reads and writes with `poll` | herdr Unix actor (`src/pty/actor/unix.rs:391`, `:541`) | Rejected: Unix-only (herdr itself uses threads on Windows); it would put OS-specific I/O outside the backends. |
| **Reply placement** | A. Replies in the same FIFO as user units | spike `pty.rs:86-100` (one channel) | Rejected: a reply waits behind every queued unit. |
| | B. A reply lane also served inside a unit's pause | herdr writes responses during a submission's delay (`actor.rs:155-163`, test `:563-578`) | Rejected: a unit's bytes would no longer be one span, and a reply could land between a prompt and its submit with unverified effect on the CLI. |
| | C. A reply lane served only at unit boundaries, in sampled batches | — | **Adopted.** Contiguous spans; no starvation of either lane. |
| **Reply atomicity** | A. Fail a reply or escape-bearing part on a short accept | — | Rejected: the split hazard is conhost's, and the Windows backend obligation is whole acceptance on a synchronous pipe; the Unix tty does not lose split sequences. |
| | B. One write call per part, looping on a short accept and counting splits | — | **Adopted**; revisit if the Windows ConPTY input pipe accepts short writes outside a close or error. |
| **Gate basis** | A. The spike's revocable `GateState` (2004 epoch + revocation flag) as the whole recheck | spike `pty.rs:21-63` | Partly adopted: the 2004 epoch is `Guard::bracketed_paste`. Revocation and the rest of mvp §2.3's gate live in the daemon. |
| | B. A watermark of write completions the daemon has applied | — | Rejected: VT replies have no completion, so the watermark could never pass a startup reply, and a lost notification would stall every send. |
| | C. The writer position captured with the readiness evidence at intake, checked at the dispatch boundary | herdr orders responses against input by offset (`src/pane/terminal.rs:1491-1530`) | **Adopted.** Needs no notification stream; which origins count stays the daemon's. |
| **Origin log** | A. Unbounded in memory | — | Rejected: attach keystrokes grow it without bound. |
| | B. Only in the event store | — | Rejected: one event per keystroke and per reply; mvp §2.1 appends `human_write` only on the first human write after evidence. |
| | C. Monotonic per-class counters for decisions, a bounded ring for diagnostics | — | **Adopted.** Ring eviction cannot hide a write from a guard or an ack. |
| **Synchronized updates** | A. vte's default `StdSyncHandler` buffering | alacritty's renderer uses it to avoid tearing | Rejected: a headless VT has no frames to tear; buffering hides mode changes and queries from the gate and the responder. |
| | B. `NoSync`: never buffer | — | **Adopted.** |
| **Resize locking** | A. Resize PTY and VT under the VT lock | — | Rejected: ConPTY's resize can wait on conhost, conhost on the drain, the drain on the VT lock. |
| | B. A control lock; PTY first without the VT lock, then the VT | spike `pty.rs:385-399` | **Adopted.** |
| **Output consumers** | A. Channel reader per consumer (spike `drain.rs:157-176`) | spike | Rejected: unbounded channel, one extra thread. |
| | B. The session's sink passed into `spawn`, fed from the first byte | herdr `on_read` callback (`actor.rs:22`, `:39-44`) | **Adopted.** No hold, no attach handoff; the drain can never be blocked by a consumer. |

## 4. Same-Class Inventory

Every MVP path where bytes reach the PTY or PTY output is consumed:

| Path | PR | How this design covers it |
|---|---|---|
| Send path: text + submit | 10 (encoders 5) | `Input` unit `[text, Pause, submit]`, `Guard` with the readiness basis, the 2004 epoch, and the launch's hook fence, no recheck (§2.3); outcome mapping and ack facts in §2.3; prompt size refused before admission (D8). |
| Codex positional first prompt | 10, 14 | No PTY write: it is in `SpawnSpec::argv`, anchored at the `last_started` the launch published (§2.3); `SpawnError` tells whether it may have run. |
| Attach typing and recognised pastes | 11 | `Attach` units; a paste is one unit without submit, refused before admission above the prompt limit; queued behind any input unit, never inside it; `QueueFull` is returned to the attach client. |
| Attach screen and size | 11 | `SessionHandle::subscribe` gives a budgeted snapshot plus gapless bytes after it; the repaint is the redraw a resize forces (t1-attach D3). `SessionHandle::resize` follows the operating socket's size, one operating socket per launch (t1-attach D2), fenced by its grant and ordered against sends by the launch lease (D7). |
| Raw `key` command, keypad, `surface_action send_key` (PR 20, deferred) | 10, 19, 20 | `Key` units with a recheck (process running, owner). Arrow encoding reads `modes().app_cursor`; a recheck can refuse when the mode changed since encoding. mvp T2's key bar, for keys not bound to a card, uses either this path or the attach path above (`Attach` units); T2's design chooses. |
| Approval card keys (every card-bound key, the key bar's included, whatever its transport) | 10, 19 | `CardKey { approval_id }` whose `Guard` has the hook fence and a `basis` at the human writer position of the card's observation, with the `app_cursor` recheck; the effect reads the card open first (409 `card_closed` otherwise, mvp §2.10); class `Human`, mapped to `card_key`, which does not close the gate. |
| VT responder | 2 | `VtReply` writes in the reply lane, one write each, at unit boundaries (§2.5); class `Reply`, counted by default. |
| Gate timeout, attention; an approval card snapshot if C1's design records one | 10, 13 | `snapshot(deadline).to_payload()` → `vt_snapshot_taken`; `CloseOutcome::final_snapshot` after teardown. |
| Gate's DECSET 2004 condition | 10 | `ModeView` before the decision, `Guard::bracketed_paste` and the dispatch boundary at the writer. |
| Doctor PTY round trip (deferred) | 21 | A `Session` over the native backend: VT query (responder record), paste echo (`Input` or `Key` unit), tree kill; `evidence` carries the ConPTY module. |
| Thread state | all | No API turns output into state input: output reaches only the VT, sinks, and snapshots (invariant 1). |

## 5. Resource Paths

`Limits` defaults (internal; configurable per session): `queue_units` 256, `max_unit_bytes` 9 437 248 (`core::input::MAX_PROMPT_BYTES` 256 KiB × 36 + 64), `queue_bytes` 16 MiB, `max_pause` 5 s, `reply_queue` 64, `log_records` 4096, `subscribers` 8. 36 is the worst-case `win_keys` expansion per input byte, one LF becoming a 36-byte Shift+Enter record pair (spike `paste.rs:427`); 64 covers the padding spaces and the submit; PR 5 supplies the exact encoded-size bound of its encoders. `CloseBounds` defaults: `kill` 5 s, `input` 1 s, `close` 2 s, `drain` 2 s, `join` 1 s. The waiter polls `wait_timeout(100 ms)`.

| Resource | Created by | Bound | Ends on |
|---|---|---|---|
| `pty-drain` thread | backend (`Drain::start`) | one per PTY | EOF or read error; joined by close step 5, else abandoned and reported |
| `pty-writer` thread | `Session::spawn` | one per session | close step 3, or after an I/O error once the queue is closed; joined in step 6 or abandoned |
| `pty-wait` thread | `Session::spawn` | one per session | exit observed, `WaitFailed`, or close; polls every 100 ms |
| Windows close worker | backend, started before `CreatePseudoConsole` | one per PTY | `ClosePseudoConsole` returns, or the control is dropped before the pseudoconsole exists; not waited on past the close deadline, reported as `close.elapsed == None` |
| User lane | `Session` | 256 units, 16 MiB | close step 1 settles the rest `Closed` |
| Reply lane | `Session` | 64 replies | discarded and counted on exit, input breakage, or close |
| Unit slot, counters | `Session` | one slot, 3 counters | the slot's completion fires once (writer or closer) |
| Origin log ring | `Session` | 4096 records | dropped with the session |
| Subscribers | `subscribe`, `SessionHandle::subscribe` | 8 (the daemon's attach: one operating socket per launch, plus overlapping candidates and replaced subscriptions not yet removed on the blocking pool; a candidate over the limit closes 4004, t1-attach D2) | `Stop`, panic, `unsubscribe` (also after the seal), output end, close |
| Completion | `submit` | one per unit | exactly once: written, refused, incomplete, `Cancelled` at cancel, `Closed` at close, or `unknown_after_close` |
| Output pipe / input / control / child | backend, owned by the guard then `Session` | one each | close steps 2–5, or the guard's teardown |

**Endings.**

| Ending | What happens |
|---|---|
| Spawn failure | The backend returns `NeverStarted` or `StartedThenFailed` with its cleanup result (§2.1); nothing is handed over. |
| Thread creation fails in `Session::spawn` | Created threads are stopped and joined, then the guard's teardown runs (the input never left it); `StartedThenFailed` carries the cleanup result, with an abandoned drain reported apart from confirmed release. |
| Child exit | Exit published, then `Exited` once; undispatched units refused `ChildExited`; a dispatched unit stops before its next write call; drain continues to EOF; session stays until close. |
| `kill` / `kill_by` | `kill_tree(extra, lock_by, deadline)`; I/O stays up so the final output and snapshot remain available. |
| `seal` | Close step 1; the session stays up for `kill_by` and `close_by`. |
| `close` / `Drop` | Close order of §2.2, each step bounded; blocked threads abandoned and named; `Released` says what was confirmed. `Drop` blocks at most the sum of the `CloseBounds`; the daemon calls `close` itself from `spawn_blocking`. |
| Reader EOF | `OutputEnded(Eof)`; sinks get `End`; the VT and snapshots stay. |
| Read error | `OutputEnded(Error)`, never reported as EOF. |
| Write error or `Ok(0)` | Unit `Incomplete`; `InputBroken`; later units refused. |
| Panic in a sink (VT or subscriber) | Caught on the drain thread; VT → `VtFailed`, subscriber → removed; reading continues. |
| Panic in a recheck or completion | Recheck → `NotStarted(RecheckPanicked)`; completion → ignored. |
| Panic in the writer thread (e.g. a backend `write`) | `catch_unwind` around the writer loop; then the writer closes the queue, settles queued units `NotStarted(Closed)`, records the slot's unit as `Incomplete { error: "unknown_after_panic" }` if dispatched (its confirmed prefix kept; the call that panicked may have written more), marks input broken (`InputBroken` cause `writer_panicked`). |
| Panic in the waiter | `catch_unwind` around the wait loop; then `WaitFailed("waiter panicked")`. |
| Panic inside an implicit cleanup (`Session`'s `Drop`, an unconsumed `SpawnedPty`'s teardown; e.g. a backend's kill) | `catch_unwind` inside the `Drop`; the parts not yet released drop as values. A drop can run during an unwind, where a panic leaving it aborts the process. |
| Panic inside a close step (`close`, `close_by`, or the implicit close; e.g. a backend's kill) | The close order's guard still stops the writer and the waiter and closes the control, so no thread keeps the child or the PTY; an explicit close then passes the panic to its caller. |

Every timed wait of the crate is bounded by its own deadline: `CompletionReceiver::recv_deadline`, `OutputReceiver::recv_timeout`, `snapshot` and `subscribe` (their deadline), `kill(deadline)` and `kill_by` (the lock wait until `lock_by`, the call until `deadline`), `close(bounds)` and `close_by` (all six steps), `wait_timeout`, `Drain::finish(bound)`, the Windows close worker wait, and the writer's pause (`max_pause`). `resize` has no deadline. `WriterObserver::last_started_each` blocks on the queue lock with no deadline; every holder keeps that lock only for one queue operation, and `try_last_started_each` never waits. FakePty's `FakeScript.hold_waiter` blocks the fake waiter until the test calls `FakeChild::release_waiter`; a close bounds its wait for it and reports `pty-wait` abandoned. Two blocking calls have no deadline of their own, a `write` to a child that stopped reading and a `read` the backend has not unblocked; close does not wait on them past its budgets, reports their threads as abandoned, and their endpoints are released when the call returns.

## 6. Purpose Layer and Plan Layer

### 6.1 Purpose Layer: Acceptance Criteria

Checked by running this crate's tests on `macos-latest` and `windows-latest` CI with `FakePty`.

| ID | Criterion (EARS) | Source | How to observe |
|---|---|---|---|
| PR2-P-1 | The crate shall provide a `PtyRuntime` trait that `FakePty` implements on both OSes, with the backend obligations stated so PR 3 and PR 4 can implement it unchanged. | mvp §4 PR 2; invariant 3 PTY Seam | `FakePty` passes every session test on both runners; §2.1 lists what PR 3 and PR 4 supply (their own PRs prove the native backends). |
| PR2-P-2 | When any write reaches the PTY, the writer shall record its origin and position. | mvp §4 PR 2 obligation | Records partition the fake child's input stream, with origins (PR2-CL-1). |
| PR2-P-3 | When the CLI sends a terminal query whose reply the reply lane admits (room in the lane, child running, input not broken; §2.5), the responder shall answer it in exactly one write. | mvp §4 PR 2 obligation; arch §4.5 | Fake child write calls each hold one whole reply (PR2-CL-9). |
| PR2-P-4 | When the screen holds a wide character, the snapshot shall show it on two cells. | mvp §4 PR 2 obligation; P-12 | Snapshot cells and the decoded payload grid (PR2-CL-12, CL-23). |
| PR2-P-5 | While a unit is being written, no other byte shall reach the PTY, and a unit refused before its first byte shall write zero bytes. | mvp §2.3 writer revalidation | Fake input bytes (PR2-CL-2, CL-3, CL-5). |
| PR2-P-6 | When a session ends in any way, `close` shall return within its budgets and report which resources it confirmed released, and output shall be read until EOF whenever the backend delivers it. | mvp §4 PR 2 "drain"; spike §4.6–§4.7 | Fake event order, `Released`, timings (PR2-CL-17, CL-18, CL-19). |

### 6.2 Plan Layer: Claims

| ID | Claim (EARS) | Source | Falsifying test |
|---|---|---|---|
| PR2-CL-1 | When the writer dequeues a unit, it shall assign it the next `seq` and record its origin; the spans of dispatched units shall be contiguous and, in `seq` order, partition the PTY input stream, except that an `unknown_after_close` or `unknown_after_panic` record holds only its confirmed prefix and its last permitted call may have written more. | PR 2 obligation | `writer::records_partition_the_input_stream`, proptest `writer::records_partition_under_random_units` (random units, replies, and refusals), `writer::panic_after_acceptance_marks_the_span_uncertain` |
| PR2-CL-2 | While a unit is in progress, no byte of another unit or of a reply shall be written; replies shall be written only at unit boundaries. | §2.3, §2.5 | `writer::human_write_waits_for_the_unit`, `writer::reply_waits_for_the_unit_boundary` (query emitted after `PauseEntered`; its reply lands after the submit) |
| PR2-CL-3 | If a unit is cancelled, closed, after child exit, after input breakage, past `not_after`, after a counted write past its basis, with a basis above the head, with 2004 off or its epoch changed, or its recheck fails or panics, then the writer shall record `NotStarted` with that reason and write zero bytes. | mvp §2.3 | `writer::each_refusal_writes_zero_bytes` (one case per reason), `writer::child_exited_precedes_other_checks`, `writer::expired_precedes_recheck` |
| PR2-CL-4 | When a unit dispatches, `last_started` for its class shall advance before its first write call; a basis shall refuse when a counted class advanced past it and shall not refuse for a class outside `counted`, whatever the ring holds. | §2.3 basis | `writer::last_started_advances_on_dispatch` (`hold_writes` + `wait_entered`: counter and in-progress span read while blocked; `fail_writes_after`: partial span), `writer::stale_basis_refuses_with_healthy_input`, `writer::reply_before_basis_admits_after_refuses`, `writer::counted_human_after_reply_only_eviction` (`log_records` 8) |
| PR2-CL-5 | When a recheck passes, every earlier unit shall have finished, and the writer shall refuse if the queue closed or the exit, input, mode word, or deadline changed before the first write permission. | mvp §2.3 arbitration lock | `writer::recheck_sees_every_earlier_write`, `writer::decrst_during_recheck_refuses`, `writer::close_during_recheck_settles_once` (`park(RecheckEntered)`) |
| PR2-CL-6 | When a unit has dispatched, the writer shall write its remaining parts regardless of `not_after`; once the queue closes or the child exits, no further write call shall get permission and the record shall be `Incomplete`; a call permitted before close may still reach the backend and the unit is reported `unknown_after_close`. | herdr committed submit; mvp §2.3 | `writer::started_unit_ignores_its_deadline`, `writer::close_in_pause_skips_the_submit`, `writer::exit_in_pause_skips_the_submit` (both after `PauseEntered`), `writer::close_after_permission_reports_unknown` (`park(Permitted)`, close, release: one backend call, then none; the record holds the prefix before that call and the fake input shows the call's bytes after it) |
| PR2-CL-7 | If a write fails or returns `Ok(0)` for a non-empty part, then the unit shall be `Incomplete` with the bytes written, `InputBroken` shall fire once, and later units shall be refused. | §2.3 | `writer::write_error_breaks_input`, `writer::write_zero_breaks_input` |
| PR2-CL-8 | When `submit` gets a full lane, a bad shape, a unit over the configured `max_unit_bytes`, or a `VtReply` origin, it shall fail at once with nothing written and no record, without waiting for queue space or PTY I/O. | §2.3 bounds | `writer::full_queue_fails_without_blocking` (writes held), `writer::bad_shapes_are_refused`, `writer::unit_at_limit_accepted_limit_plus_one_refused`, `writer::reply_origin_is_refused` |
| PR2-CL-9 | When the VT emits a reply, the writer shall write it as one write call holding exactly that reply on whole acceptance, in query order. | PR 2 obligation | `responder::each_reply_is_one_write`, proptest `responder::replies_are_single_writes` (random output with embedded known queries; asserts the exact count and bytes of the replies) |
| PR2-CL-10 | When the CLI sends DA1, DA2, DSR 5, DSR 6, the kitty query, DECRQM, CSI 14 t, CSI 18 t, or OSC 4/10/11/12, the responder shall answer as in §2.5, and replies in a sampled batch shall precede the next user unit; an OSC 52 read shall get no reply. | arch §4.5; spike B-5 | `responder::answers_each_query`, `responder::batch_precedes_the_next_unit`, `responder::clipboard_read_is_not_answered` (an OSC 52 read followed by DSR 5: exactly one reply, `ESC[0n`) |
| PR2-CL-11 | Under a stream of replies, the next queued user unit shall be dequeued after at most one reply batch; a reply the lane cannot hold, or queued when exit or input breakage is observed, shall be discarded, counted, and an overflow reported by `ReplyDropped`. | §2.3 progress rule | `responder::replies_cannot_starve_units` (`on_input` answers each reply with a new query; `wait(ReplyEnqueued)` before releasing the writer), `responder::overflow_is_reported` (65 queries behind a held write), `responder::replies_after_exit_are_discarded`, `responder::queued_reply_is_counted_on_input_breakage` |
| PR2-CL-12 | When the grid holds a wide character, the snapshot shall give it a `WideLead` and a `WideTail` cell, every line shall have `cols` cells, zero-width characters shall join their base cell, a wrapped wide character shall leave a blank cell at the line end, and an orphan half shall become a blank `Single`. | PR 2 obligation | `snapshot::wide_chars_occupy_two_cells`, `snapshot::wrapped_wide_char_leaves_a_blank`, `snapshot::combining_mark_joins_its_base`, `snapshot::erased_half_of_a_wide_char_is_blank` (ECH on the lead, then on the tail; literal wire values), `snapshot::alt_screen_shrink_leaves_no_orphan` |
| PR2-CL-13 | The mode word shall reflect DECSET 2004, 1049, 25, DECCKM, DECKPAM, and kitty flags in one load, and the 2004 epoch shall advance on every transition, including two within one read and inside a synchronized update. | mvp §2.3 gate | `snapshot::modes_follow_decset`, `snapshot::epoch_counts_transitions_within_a_read`, `snapshot::sync_update_does_not_hide_2004` (`CSI ?2026h ?2004l ?2004h ?2026l`: epoch +2), `snapshot::unterminated_sync_update_still_answers` (`?2026h` then DSR 5 and silence), `snapshot::mode_word_is_never_torn` (a reader thread checks every loaded word against the set of words the writer published) |
| PR2-CL-14 | When a valid size is given, the PTY shall be resized first, then the grid and the responder's size, and output parsed after that shall be laid out at the new size; an invalid size or a failed PTY resize shall change neither side; the size before any resize shall be the spawn size. | spike C-4 | `session::resize_moves_pty_grid_and_responder`, `session::failed_resize_changes_nothing` (`fail_next_resize`), `session::invalid_size_is_refused`, `session::startup_size_answers_csi_18t` |
| PR2-CL-15 | When a sink subscribes, it shall get a snapshot and then exactly the output bytes after the snapshot's offset, and exactly one `End`; a `channel_sink` shall yield every queued chunk, then `Lagged` only if a byte was dropped, else `End`. | §2.2 subscribe | `session::subscribe_is_gapless` (subscribe between pieces of one chunk), `session::subscribe_racing_eof_gets_one_end`, `session::full_channel_sink_at_eof_ends`, `session::full_channel_sink_on_bytes_lags` |
| PR2-CL-16 | The drain shall pass every byte read to the session's sink in order from the first byte, then the end, report a read error as an error, and never block on a sink; a panicking or stopping sink, or a panicking `on_event` handler, shall not stop it. | spike F-2c, §4.6 | unit tests in `src/drain.rs`: `final_output_reaches_the_sink`, `read_error_is_not_eof`, `finish_is_bounded_without_an_end`; `session::startup_reply_waits_for_the_writer` (`FakeScript::startup_output` with DSR 5), `session::sink_panic_keeps_draining`, `session::reply_dropped_panic_keeps_draining`, `session::vt_panic_marks_vt_failed` (`fail_vt_on_next_piece`), `session::read_error_reaches_status` (`fail_output` on a blocked read) |
| PR2-CL-17 | When a session closes, it shall settle every queued and undispatched completion itself, giving a never-dequeued unit its `seq`, kill the tree with the given extra identities, release the input, close the PTY, finish the drain, and join threads, in that order, each step within its budget. | spike §4.7 | `session::close_order` (FakeEvent order, `Kill { extra }`), `session::close_settles_queued_units_with_a_blocked_writer` (seqs gapless, `last_started` unchanged), `session::cancel_while_blocked_gets_a_seq`, `session::close_kills_again_for_new_extra` (`kill(&[])` then `close(&[d])`) |
| PR2-CL-18 | If the writer is blocked when `close` runs, then `close` shall return within the sum of its budgets, fire the in-progress completion once as `Incomplete { unknown_after_close }`, name the writer as abandoned, and report the input as not released. | §2.2 close | `session::close_is_bounded_with_a_blocked_write` (`hold_writes` + `wait_entered`, `KillBehavior::Ignore`; completion count checked after the hold is released) |
| PR2-CL-19 | If thread creation fails in `Session::spawn`, then a bounded kill attempt shall precede dropping the input and closing the PTY, the outcome shall report whether death was confirmed, no partially started writer shall own the input, dropping the input shall write no bytes, and the error shall report an abandoned drain apart from confirmed release. | §2.1, §2.2 start | `session::writer_creation_failure_tears_down` (`StartFault::Writer`), `session::waiter_creation_failure_tears_down` (`StartFault::Waiter`), `session::failed_spawn_reports_unconfirmed_kill_with_ends_released` (`KillBehavior::Ignore`) |
| PR2-CL-20 | When the child exits, the session shall publish the exit before `Exited`, emit `Exited` once, and refuse every unit not yet dispatched with zero bytes. | mvp §4 PR 10 "write-boundary recheck after exit" | `session::exit_is_reported_once`, `session::submit_from_exited_callback_is_refused` |
| PR2-CL-21 | `last_started` shall be monotonic per class and independent of the ring; `writes_after` shall return every record after `seq`, the in-progress one included, or `LogTruncated`. | §2.3 | `writer::counters_are_monotonic`, `writer::writes_after_reports_truncation`, `writer::writes_after_shows_the_blocked_write` |
| PR2-CL-22 | `WriteOrigin` shall map `Attach`, `Key`, `CardKey` to the `human_write` origins and to class `Human`, `Input` to `Input`, `VtReply` to `Reply`. | mvp §2.1, §2.3 | `origin::maps_to_human_write_origins_and_classes` |
| PR2-CL-23 | `Snapshot::to_payload` shall carry cols, rows, the zero-based cursor, `VtModes`, and the grid in the §2.6 encoding, and `decode_grid` shall reject every malformed shape listed there. | D9; mvp §2.1 `vt_snapshot_taken` | `snapshot::payload_matches_the_snapshot`, `snapshot::grid_examples` (the §2.6 examples as literal strings), `snapshot::malformed_grids_are_rejected`; `core` goldens `vt_snapshot_taken.json` and `Component::VtSnapshot` |
| PR2-CL-24 | When `close` waits, each step shall end by its budget and the whole call by their sum, and every other bounded method of §5 shall return by its deadline. | §5 | `session::close_steps_end_by_their_budgets` (blocked write, `KillBehavior::Ignore`, `hang_control_close`, no output EOF), `session::final_snapshot_skipped_with_a_held_vt_lock` and `session::snapshot_with_a_held_vt_lock_is_unavailable` (`park(PieceParsing)`), `session::receivers_end_by_their_deadlines` |
| PR2-CL-25 | `chaldea-pty-runtime` shall build without tokio. | D2 | CI dependency check (§7) |
| PR2-CL-26 | When a backend accepts a part short, the writer shall complete it in further calls and record the number of splits. | §2.3 short writes | `writer::short_writes_are_counted` (`short_writes(3)`) |
| PR2-CL-27 | If an approval card or other recheck condition closes after a unit was submitted, then the recheck shall refuse it with zero bytes. | mvp §2.10 | `writer::card_key_refused_after_enqueue` |
| PR2-CL-28 | No callback of the crate shall run while an internal lock is held. | §2 locks and callbacks | `session::callbacks_may_reenter` (a completion that cancels another ticket; a `ReplyDropped` handler whose `snapshot` succeeds and shows the text emitted before the overflow; a sink that subscribes; each finishes) |

Obligation mapping: "origin and position of every write" → CL-1, CL-4, CL-21, CL-22; "replies in one write each" → CL-9, CL-10, CL-11; "wide characters on two cells" → CL-12, CL-23.

## 7. Tests and Completion Commands

All tests above run in `cargo test --workspace --locked` on `macos-latest` and `windows-latest` with `FakePty`; none needs a real PTY. The real-PTY tests of each backend are in its document's §7. The VT, responder, and snapshot tests feed `Vt` directly or through `FakeChild::emit`. Tests wait for a thread to reach a point with `WriteHold::wait_entered` or `Probe::wait` / `park` before acting, so "while the writer is blocked" or "during the pause" is a fact, not a timing guess. Timing assertions use generous bounds (seconds) so they hold on loaded runners; pauses in tests are 10–50 ms. Drain tests are unit tests in `src/drain.rs` because `Drain` is crate-private.

Completion commands: those of mvp §5, plus the dependency check of PR2-CL-25:

```
deps="$(cargo tree -p chaldea-pty-runtime -e normal --prefix none)" && ! grep -Eq '^tokio ' <<<"$deps"
```

## 8. Decisions

**D1. The trait is the raw spawn; `Session` is shared.** One writer, drain, and VT for every backend, exercised by FakePty in every test. arch §4.5's operation list maps onto `Session` (§3 "Trait level").

**D2. No tokio; callbacks bridge to the daemon.** Blocking PTY ends, one-thread writer arbitration, and runtime-free tests; checked in CI. Rejected: tokio in the crate, as Codex's `utils/pty` does (§2 threading model).

**D3. The crate records; the daemon decides what counts.** Every write carries its origin and class, including VT replies (`Reply`). Which classes close the gate or make an ack ambiguous is the daemon's per-adapter policy, passed as `Basis::counted` and applied to `last_started`; the default counts every class, so mvp §2.3's "last write of any origin" holds literally. Exempting replies for an adapter needs evidence that the CLI consumes them separately (PR 12 captures, the query log of P-13, a deferred real-host row) and is added as an adapter capability.

**D4. Replies only at unit boundaries.** A unit's bytes stay one contiguous span, and no reply lands between a prompt and its submit. The cost: a reply becomes eligible only in the next batch after the unit in progress, so a blocked write delays it as long as it blocks; a CLI that waits for a reply during that time waits that long.

**D5. Every `NotStarted` and every synchronous `SubmitError` maps to `RecheckFailed`, and every `Incomplete` to `WriteOutcomeUnknown`.** Zero bytes is certain only before the first write permission; an I/O error even at zero bytes is treated as unknown, as mvp §2.3 does for any write outcome it cannot prove.

**D6. `FakePty` is behind a `fake` feature**, enabled for this crate's tests through a self dev-dependency and for the daemon's tests through its dev-dependency, so no default-feature build contains it (`--all-features` does).

**D7. Backends need `unsafe`; the workspace forbids it.** The root `[workspace.lints.rust] unsafe_code = "forbid"` cannot be relaxed by `#[allow]`. This crate has its own `[lints]` table (the workspace clippy set plus `unsafe_code = "deny"`, `docs/design/pty-unix.md` §2.7) and allows `unsafe` only in `unix::sys` and `windows::sys`; the workspace value stays `forbid` for every other crate.

**D8. Prompt text is limited to `core::input::MAX_PROMPT_BYTES` = 256 KiB UTF-8**, in `core` because this crate's defaults derive from it. `send` (PR 10) and recognised attach pastes (PR 11) refuse longer text before admission, with zero bytes; PR 5 bounds the encoded size. The writer limits (§5) are internal, configurable defaults: about 9 MiB per unit and 16 MiB queued per session instead of the 144 MiB a 4 MiB body limit would imply. Raising the limit later is compatible.

**D9. `vt_snapshot_taken.grid` is JSON rows of cell strings** with the semantics of §2.6. It makes the geometry exact and observable by the PWA (a wide character provably takes two columns, column positions after it do not depend on font rendering), which is how the PR 2 obligation reaches the reader and what P-12's legible CJK snapshot needs. It was fixed before any producer persisted a grid (the store arrives in PR 6), so the change kept `schema_version` 1 (mvp §2.1). Rejected: LF-joined text (geometry left to the PWA's font) and text plus a wide-column list (two structures that must agree). Attributes added later are additive only as a separate field, since cells stay strings.

## Related Text in Other Documents

- `docs/mvp-design.md` §2.1 `vt_snapshot_taken` and the deferred §2.8 `vt_snapshot` component state the grid encoding (D9); §2.3 states the prompt limit (D8).
- `docs/mvp-design.md` §3 spike table: the drain is lifted without its hold, the session's sink passed into spawn (§2.4); the `unsafe_code` carve-out of D7.
- `docs/mvp-design.md` §4 PR 10 obligations: the readiness basis and the ack candidate rule (§2.3).
- `docs/architecture.md` §4.5: the trait seals spawn and OS differences; the session built on it provides the operations.
