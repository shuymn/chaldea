# Design: `crates/pty-runtime` (MVP PR 2)

Status: design, not implemented. Scope is the PR 2 row of `docs/mvp-design.md` §4 (cited as "mvp §N"): the `PtyRuntime` trait, `FakePty`, the single writer with its origin log, the drain, the VT responder, and the snapshot. Its design obligations: the writer records origin and position of every write; the VT responder replies in one write each; wide characters occupy two cells in snapshots.

**What the crate does and does not decide.** The crate records facts: the origin, order, byte span, and start and finish of every write; the terminal modes; the screen. Gate and ack policy (which writes close the gate or make an ack ambiguous, VT replies included) stay in the daemon and `core` (PR 10). The crate's guards only enforce conditions the daemon states per write.

Out of scope, designed in their own PRs: the Unix backend (PR 3), the Windows ConPTY backend (PR 4), `testkit` and FakeCli (PR 3), the input encoders in `core::input` (PR 5), the gate, send path, and supervisor in the daemon (PR 10), and attach's repaint (PR 11). This document fixes only what those PRs need from this crate: what a backend must supply (§2.1) and what the daemon can rely on (§2.2–§2.6).

Sources read for this document (paths are relative to each root):

| Source | Revision | Read for |
|---|---|---|
| Spike `spikes/pty-hooks/src/` | this repository | `pty.rs` (writer, gate), `vt.rs` (responder, snapshot), `os/drain.rs`, `os/mod.rs`, `paste.rs` (`win_keys` records), §4.4–§4.7 of `docs/spikes/pty-hooks-spike.md` |
| `alacritty_terminal` | crate 0.26.0 | `event.rs`, `term/mod.rs`, `term/cell.rs` |
| `portable-pty` | crate 0.8.1 | `lib.rs`, `unix.rs` |
| herdr (`herdrdev/herdr`) | `e8d7d97` | `src/pty/actor.rs`, `src/pty/actor/unix.rs`, `src/pty/backend.rs`, `src/pane/terminal.rs` |
| Codex (`openai/codex`, sparse `codex-rs/utils/pty`) | `8653ca9` (default branch 2026-11-22) | `src/pty.rs`, `src/process.rs` |

## 1. Goal and Files

**Goal.** A library crate, `chaldea-pty-runtime`, that turns a backend's raw PTY (a child process, an output read end, an input write end, a resize control) into a `Session`: one drain thread that reads output until EOF and feeds a headless `alacritty_terminal` VT, one writer thread that is the only code writing to the PTY and records every write with its origin and byte span, and one waiter thread that observes child exit. All of it is tested on macOS and Windows CI through `FakePty`, with no real PTY.

**Files.**

```
Cargo.toml                                # members += "crates/pty-runtime"; [workspace.dependencies] += alacritty_terminal
crates/pty-runtime/Cargo.toml             # chaldea-pty-runtime; feature `fake`; [lints] workspace = true (PR 3 changes it, D7)
crates/pty-runtime/src/lib.rs             # re-exports; crate doc states the lock order and the no-tokio rule
crates/pty-runtime/src/backend.rs         # PtyRuntime (sealed), SpawnSpec, PtySize, SpawnedPty (owning guard), PtyChild, PtyControl,
                                          # ProcessIdentity, ChildExit, KillReport, CloseReport, SpawnError
crates/pty-runtime/src/session.rs         # Session, SessionConfig, SessionEvent, SessionStatus, CloseBounds, CloseOutcome, close order
crates/pty-runtime/src/writer.rs          # WriteUnit, Part, Guard, Basis, WriteContext, WriteRecord, WriteOutcome, Refusal, counters, ring
crates/pty-runtime/src/origin.rs          # WriteOrigin, OriginClass, ClassSet, ReplyKind, mapping to core::payload::HumanWriteOrigin
crates/pty-runtime/src/limits.rs          # Limits, CloseBounds defaults, the max_unit_bytes derivation
crates/pty-runtime/src/drain.rs           # lifted from spikes/pty-hooks/src/os/drain.rs; ordered handoff to a sink; unit tests inside
crates/pty-runtime/src/vt.rs              # lifted from spikes/pty-hooks/src/vt.rs: Vt, Responder, packed ModeWord
crates/pty-runtime/src/snapshot.rs        # Snapshot, Line, Cell, CellWidth, grid encoding, to_payload
crates/pty-runtime/src/fake.rs            # #[cfg(feature = "fake")] FakePty, FakeScript, FakeChild, fault hooks
crates/pty-runtime/tests/{writer,responder,snapshot,session}.rs
crates/core/tests/golden/events/vt_snapshot_taken.json, crates/core/tests/golden.rs
                                          # grid golden values (event and Component::VtSnapshot) in the §2.6 encoding
.github/workflows/ci.yml, lefthook.yml    # `workspace` job: add the pty-runtime dependency check (§7)
```

PR 3 adds `src/unix.rs` (`#[cfg(unix)]`) and PR 4 adds `src/windows.rs` (`#[cfg(windows)]`) plus a `native()` constructor; neither changes a type of this PR.

**`Cargo.toml`.** Exact pins, as the rest of the workspace:

```toml
# root [workspace.dependencies]
alacritty_terminal = { version = "=0.26.0", default-features = false }

# crates/pty-runtime/Cargo.toml
[dependencies]
chaldea-core = { path = "../core" }
alacritty_terminal.workspace = true
serde_json.workspace = true          # the snapshot grid encoding (§2.6)

[dev-dependencies]
chaldea-pty-runtime = { path = ".", features = ["fake"] }   # integration tests use FakePty
proptest.workspace = true

[features]
fake = []
```

`alacritty_terminal =0.26.0` is the version the spike ran on both hosts and the one prior-art row 3a and spike §3 read in source; its only optional feature (`serde`) is not needed. It also compiles its own `tty` module and that module's dependencies (`rustix-openpty`, `signal-hook`, `polling` on Unix; `miow`, `piper`, `windows-sys 0.59` on Windows); this crate never calls them. No `tokio`, no `portable-pty` in this PR (PR 3 adds `portable-pty =0.8.1` under `[target.'cfg(unix)'.dependencies]`).

**Workspace and CI.** The crate joins `members`. The `workspace` job runs unchanged (fmt, clippy `-D warnings`, `cargo test --workspace --locked`, which now includes this crate's tests on `macos-latest` and `windows-latest`) and gains one step: `pty-runtime` has no `tokio` in its normal dependency tree (§7). `lefthook.yml` mirrors it.

## 2. Types and Functions

Names may change in review; shapes may not.

**Threading model.** The crate is synchronous: std threads, blocking I/O, no async runtime. PTY ends are blocking on both OSes (portable-pty's reader and writer are blocking `Read`/`Write`, `lib.rs:97-102`; ConPTY uses synchronous anonymous pipes), so an async runtime would only wrap them in blocking tasks, as Codex does (`utils/pty/src/pty.rs:241`, `:285`). Callers never wait on the crate's threads except through the bounded methods named in §5: every callback the crate invokes (`Completion`, `OutputSink`, `on_event`) must return without blocking, and the crate never waits on a caller. The daemon bridges to tokio by passing callbacks that `try_send` on a tokio channel or complete a `oneshot`; it calls the bounded blocking methods (`close`, `kill`, `resize`) from `spawn_blocking`.

**Locks and order.** Control lock → VT lock → queue lock; no code takes them in another order, and no lock is held while a caller's callback runs, except where §2.3 and §2.4 say so.

- *Control lock* guards `PtyControl`; only `resize` and close step 4 take it. The drain thread never does.
- *VT lock* guards the `Term`, the parser, and the subscriber list. The drain thread holds it while it parses one piece of output and the responder enqueues replies.
- *Queue lock* guards both write lanes, the in-progress slot, the counters, and the ring.
- Modes are read from one atomic word (§2.6), without a lock.

### 2.1 Backend Seam: `PtyRuntime`

```rust
/// Starts a child in a new PTY. Sealed: implemented by `unix` (PR 3), `windows` (PR 4),
/// and `fake::FakePty`. Object safe; the daemon holds `Arc<dyn PtyRuntime>`.
pub trait PtyRuntime: Send + Sync + sealed::Sealed {
    fn spawn(&self, spec: &SpawnSpec) -> Result<SpawnedPty, SpawnError>;
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

/// Owns everything a spawn created. Dropping it without `Session::start` runs the failure
/// teardown: kill_tree(&[], 5 s) → drop input → control.close(2 s) → drain finish(2 s).
pub struct SpawnedPty {                      // fields private; only backends build it
    child: Box<dyn PtyChild>,
    output: Drain,                           // already started over the PTY's output read end
    input: Box<dyn Write + Send>,            // the PTY's input write end
    control: Box<dyn PtyControl>,
    size: PtySize,                           // the size the PTY was opened with
    evidence: Vec<(&'static str, String)>,   // e.g. ("job_branch", "job_list"), ("conpty_module", …)
}
impl SpawnedPty { pub fn identity(&self) -> &ProcessIdentity; }

/// Methods take `&self` and may be called concurrently from different threads
/// (the waiter polls `wait_timeout` while `kill_tree` runs).
pub trait PtyChild: Send + Sync {
    fn identity(&self) -> &ProcessIdentity;
    fn wait_timeout(&self, timeout: Duration) -> io::Result<Option<ChildExit>>;
    /// Kills the whole tree plus the recorded descendants in `extra` (the daemon learns
    /// them from hook emitters); idempotent; returns by `deadline`.
    fn kill_tree(&self, extra: &[ProcessIdentity], deadline: Instant) -> KillReport;
}
pub trait PtyControl: Send {
    /// Resizes the PTY; returns without waiting for output to be read.
    fn resize(&self, size: PtySize) -> io::Result<()>;
    /// Releases the PTY itself (Unix master, Windows pseudoconsole); returns by `deadline`.
    fn close(self: Box<Self>, deadline: Instant) -> CloseReport;
}

pub struct ProcessIdentity { pub pid: u32, pub start_time: String, pub group: String }  // → launch_started { pid, start_time, pgid_or_job }
pub struct ChildExit { pub code: Option<i32>, pub signal: Option<i32> }               // → launch_exited { code, signal }
pub struct KillReport { pub method: String, pub tree_dead: bool, pub survivors: Vec<ProcessIdentity> }  // method → launch_death_confirmed
pub struct CloseReport { pub elapsed: Option<Duration> }                              // None: did not return by the deadline

pub enum SpawnError {
    /// The child provably never existed: the confirmed `spawn_failed` of mvp §2.3.
    NeverStarted { kind: NeverStartedKind, error: io::Error },     // NotFound | PermissionDenied | Setup
    /// A process existed and setup then failed. A Codex positional prompt may have run.
    StartedThenFailed { stage: &'static str, error: io::Error, identity: Option<ProcessIdentity>, cleanup: Cleanup },
}
pub enum Cleanup { Confirmed, Unresolved { kill: KillReport, close: CloseReport } }
```

**What each backend must supply** (the obligations PR 3 and PR 4 implement against this trait unchanged):

- `spawn` returns either a `SpawnedPty` or an error after releasing everything it created: no process alive, no handle or descriptor open (spike F-2c). When it cannot prove that, the error is `StartedThenFailed` with `Cleanup::Unresolved`. On Windows the release order is: terminate job and process, close the pseudoconsole while the drain reads, close pipe ends (spike §4.6).
- The child's environment is exactly `spec.env` (portable-pty: `env_clear` then `env`; Windows: an explicit `CREATE_UNICODE_ENVIRONMENT` block). The scrub list is the daemon's (mvp §2.7 "Env scrub").
- `output` is a `Drain` (§2.4) the backend started over its output read end. Windows starts it right after `CreatePseudoConsole`, before `CreateProcessW` (spike §4.6); Unix after `openpty`. The reader the drain owns returns `Ok(0)` once every slave end is closed: the Unix backend maps the master's `EIO` after hangup to EOF, and drops the parent's slave handle right after spawn (spike §4.7).
- **Dropping `input` writes no bytes.** portable-pty's writer writes `\n` + VEOF on drop (`unix.rs:351-361`), so PR 3 must not hand that writer over; it writes through its own handle on a dup of the master fd. With this obligation the session can release the input whenever the writer thread finishes, without an ordering hazard against a live CLI.
- `input` accepts each `write` call whole or fails: the Unix master fd is blocking; Windows `WriteFile` on a synchronous pipe completes fully. A short write is tolerated by the writer (it loops) and recorded (§2.3).
- `PtyChild::wait_timeout` never blocks past `timeout` (Windows `WaitForSingleObject(timeout)`; Unix `try_wait` polling) and must not hold a lock `kill_tree` needs.
- `kill_tree` is the PR 3 / PR 4 tree kill (`killpg` HUP→TERM→KILL plus `getsid` scan; job termination) applied also to every identity in `extra` that is still alive with the same start time, and returns by its deadline; `tree_dead: false` with survivors when it could not prove death.
- `PtyControl::close` on Windows runs `ClosePseudoConsole` through the lifted `close_bounded` helper (`spikes/pty-hooks/src/os/drain.rs:182-197`), since some builds block in it until output is read.
- `evidence` carries what doctor and the real-host runs record (Windows: `job_branch` = `job_list` or `suspended`, `conpty_module`).
- Every `unsafe` block lives in `unix.rs` / `windows.rs` (D7).

### 2.2 `Session`

```rust
pub struct SessionConfig {
    pub on_event: Box<dyn Fn(SessionEvent) + Send + Sync>,   // non-blocking; each variant at most once, except ReplyDropped
    pub limits: Limits,                                       // defaults in §5
}
pub enum SessionEvent { Exited(ChildExit), WaitFailed(String), OutputEnded(OutputEnd), InputBroken(String), VtFailed, ReplyDropped }
pub enum OutputEnd { Eof, Error(String) }

impl Session {
    /// Consumes the guard. Creates the waiter and writer threads, then hands the input to the
    /// writer, then attaches the VT sink to the drain. On any failure the guard's teardown runs
    /// and `StartError` carries its reports; the input stays in the guard until every thread exists.
    pub fn start(pty: SpawnedPty, config: SessionConfig) -> Result<Session, StartError>;
    pub fn identity(&self) -> &ProcessIdentity;
    pub fn evidence(&self) -> &[(&'static str, String)];
    pub fn size(&self) -> PtySize;

    pub fn submit(&self, unit: WriteUnit, guard: Guard, done: Completion) -> Result<WriteTicket, SubmitError>;  // §2.3
    pub fn last_started(&self, classes: ClassSet) -> u64;               // §2.3 counters; 0 before any
    pub fn writes_after(&self, seq: u64) -> Result<Vec<WriteRecord>, LogTruncated>;   // diagnostic ring

    pub fn modes(&self) -> ModeView;                                     // one atomic load
    pub fn snapshot(&self) -> Result<Snapshot, VtUnavailable>;           // §2.6
    pub fn subscribe(&self, sink: OutputSink) -> Result<(Snapshot, SubscriptionId), SubscribeError>;
    pub fn unsubscribe(&self, id: SubscriptionId);
    pub fn resize(&self, size: PtySize) -> io::Result<()>;

    pub fn status(&self) -> SessionStatus;                               // exit, output end, input state, vt_failed, ResponderStatus
    pub fn kill(&self, extra: &[ProcessIdentity], deadline: Instant) -> KillReport;   // tree kill only; I/O stays up
    pub fn close(self, extra: &[ProcessIdentity], bounds: CloseBounds) -> CloseOutcome;
    #[cfg(feature = "fake")] pub fn fail_vt_on_next_piece(&self);         // VT panic injection for tests
}
impl Drop for Session { /* close(&[], CloseBounds::default()) unless already closed */ }

pub type OutputSink = Box<dyn FnMut(OutputEvent<'_>) -> Flow + Send>;   // non-blocking; may move between threads
pub enum OutputEvent<'a> { Bytes { offset: u64, bytes: &'a [u8] }, End(&'a OutputEnd), Lagged { at: u64 } }
pub enum Flow { Continue, Stop }
pub fn channel_sink(capacity: usize) -> (OutputSink, OutputReceiver);   // std bounded channel
```

**Start.** The guard keeps `input` until the waiter and writer threads exist; a thread-creation failure runs the guard's teardown (§2.1), so the tree is killed before any PTY end is released and no half-started thread owns an endpoint. The initial `PtySize` comes from the guard, so the VT and the responder answer `CSI 18 t` correctly before any resize.

**Exit publication.** The waiter stores the exit in an atomic (`Release`) before calling `on_event(Exited)`, so a submit triggered by that event sees the exit and is refused.

**Resize.** Under the control lock: validate (`PtySize`), call `control.resize(size)` without the VT lock (ConPTY's resize can wait on conhost, which can wait on the drain; spike `pty.rs:385-399` also resized outside the VT lock), then on success take the VT lock and resize the grid and the size the responder reports. On failure neither side changes. Guarantee, stated in parse order: output parsed after the VT resize is laid out at the new size; a chunk read before the resize call may be parsed at either size. Resize is independent of the writer: whether a resize during a unit invalidates that unit's gate decision is PR 10 / PR 11 policy, which can express it through the unit's `recheck`.

**Subscribe.** A diagnostic snapshot plus the output bytes after its offset, with no gap and no duplicate. Under the VT lock the session takes the snapshot (with its `output_offset`) and registers the sink with that offset. The drain dispatches each piece with its offset after releasing the VT lock, and a subscriber receives only bytes at or after its own offset. It is not a replay checkpoint: the snapshot holds no pending escape sequence, rendition, or saved terminal state, so replaying the suffix on an outer terminal can render differently; attach's repaint is PR 11's design. After output ended, `subscribe` returns the snapshot and the sink immediately receives `End`. At most `limits.subscribers` sinks; a sink that returns `Stop` or panics is removed and never called again. `channel_sink` returns `Stop` when its channel is full, and its receiver then yields `Lagged { at }` (the offset of the first undelivered byte), distinct from `End`.

**Close order** (`close`, `Drop`, and the failure teardown of `SpawnedPty`). Every step has an absolute budget from `CloseBounds`; a step that runs out of budget is reported and the next step runs.

1. **Close the queue** (under the queue lock, on the closing thread): new `submit` calls fail `Closed`; every queued, uncancelled unit completes `NotStarted(Closed)` at once (cancelled units were already completed by `cancel`). Settling never depends on the writer thread.
2. **Kill** `kill_tree(extra, kill)` unless the tree is already reported dead.
3. **Release the input**: signal the writer and wait up to `input` for it to drop `input` and exit. A writer blocked in `write` keeps `input` until that write returns; since dropping writes no bytes (§2.1), the late drop is harmless.
4. **Close the PTY** `control.close(close)` under the control lock, while the drain keeps reading (ConPTY is never closed under an undrained pipe).
5. **Finish the drain** `Drain::finish(drain)`: wait for EOF or a read error and join the drain thread, else abandon it.
6. **Join** the writer and waiter threads within `join`. If the writer has not finished, the closer takes the in-progress completion from its take-once slot and fires it as `Incomplete { error: "unknown_after_close" }` with the bytes known written so far; a writer that returns later finds the slot empty.

```rust
pub struct CloseBounds { pub kill: Duration, pub input: Duration, pub close: Duration, pub drain: Duration, pub join: Duration }
pub struct CloseOutcome {
    pub kill: KillReport, pub close: CloseReport, pub output: Option<OutputEnd>,
    pub released: Released,               // what is confirmed released, not just abandoned
    pub abandoned: Vec<&'static str>,     // threads still running: "pty-writer", "pty-drain", "pty-wait", "conpty-close"
    pub final_snapshot: Result<Snapshot, VtUnavailable>,   // taken after step 5
    pub final_after_end: bool,            // true when the drain ended before the snapshot was taken
    pub elapsed: Duration,
}
pub struct Released { pub input: bool, pub control: bool, pub output: bool, pub tree_dead: bool }
```

`close` returns within the sum of the budgets (bounded return). Confirmed release is what `Released` says; an abandoned thread still owns its endpoint until its blocking call returns or the process exits. A snapshot taken by the daemon before `close` is pre-teardown; `final_snapshot` is the one taken after the output ended (closing ConPTY can be what lets the last output arrive).

**After child exit.** The writer refuses every unit that has not started (`NotStarted(ChildExited)`, zero bytes) and stops a started unit before its next `Bytes` part (§2.3). VT replies are discarded and counted (§2.5). The drain keeps reading until EOF, which can come later than exit while descendants hold the PTY. The session stays up until `close`; it never kills or closes on its own.

### 2.3 Single Writer and Origin Log

One writer thread per session is the only code that writes to `input`. Callers submit **units**; nothing else is written to the PTY while a unit is in progress, so a unit's bytes are one contiguous span of the input stream.

```rust
pub struct WriteUnit { pub origin: WriteOrigin, pub parts: Vec<Part> }
pub enum Part {
    Bytes(Vec<u8>),          // non-empty; one write call (looped on a short accept); the encoder keeps every escape sequence inside one part
    Pause(Duration),         // between two Bytes parts only
}
// Shape, checked at submit: 1–8 parts, first and last part Bytes, no empty Bytes, no two
// adjacent Pauses, total pause ≤ limits.max_pause, total bytes ≤ limits.max_unit_bytes.

pub enum WriteOrigin {
    Input { input_id: InputId },            // send path (PR 10): text, pause, submit as one unit
    Attach,                                 // chaldea attach (PR 11)
    Key,                                    // owner `key`, keypad, `surface_action send_key` (PR 10, 19, 20)
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
    pub not_after: Instant,                    // dispatch deadline: refuse if the first write call has not begun by then
    pub basis: Option<Basis>,                  // refuse if a counted write started after the basis
    pub bracketed_paste: Option<u64>,          // refuse unless 2004 is on with this epoch (ModeView)
    pub recheck: Option<Recheck>,              // the daemon's gate / card recheck
}
pub struct Basis { pub seq: u64, pub counted: ClassSet }   // counted classes are the daemon's policy
pub type Recheck = Box<dyn FnOnce(&WriteContext) -> Result<(), String> + Send>;
pub struct WriteContext { pub seq: u64, pub modes: ModeView, pub last_started: [u64; 3] }  // per OriginClass

pub type Completion = Box<dyn FnOnce(&WriteRecord) + Send>;            // non-blocking; called exactly once
pub fn completion_channel() -> (Completion, CompletionReceiver);        // recv_deadline(Instant)
pub struct WriteTicket { /* … */ }
impl WriteTicket { pub fn cancel(&self) -> bool; }                      // true: never starts; completes Cancelled now

pub struct WriteRecord {
    pub seq: u64,                    // per session, from 1, gapless, assigned when the unit starts or is refused
    pub origin: WriteOrigin,
    pub submitted_at: Instant, pub started_at: Option<Instant>, pub finished_at: Option<Instant>,
    pub byte_start: u64,             // input bytes written before this unit
    pub bytes_written: u64,          // the unit's span is byte_start..byte_start + bytes_written
    pub split_writes: u32,           // write() calls that accepted less than their part
    pub outcome: WriteOutcome,       // InProgress until finished
}
pub enum WriteOutcome { InProgress, Written, NotStarted(Refusal), Incomplete { error: String } }
pub enum Refusal { Cancelled, Expired, Closed, ChildExited, InputBroken, BasisUnknown { basis: u64, head: u64 },
                   WriteSinceBasis { seq: u64 }, BracketedPasteChanged, Recheck(String), RecheckPanicked }
pub enum SubmitError { Closed, QueueFull, BadShape(&'static str), UnitTooLarge, ReplyOrigin }
pub struct ModeView { pub modes: TermModes, pub bracketed_paste_epoch: u64 }
```

**Writer loop.** The writer serves two lanes: replies (§2.5) and user units (FIFO). **Progress rule:** at each unit boundary it takes the number of replies queued at that moment, writes exactly those, then takes one user unit. Replies arriving meanwhile wait for the next boundary, so a stream of replies cannot starve user units, and a reply waits at most for the unit in progress (its writes plus at most `max_pause`). No reply is written inside a unit.

For the user unit, before its first byte the writer assigns its `seq` and checks, in this fixed order, recording `NotStarted(reason)` with zero bytes on the first failure: queue closed; child exited; input broken; `not_after` passed (`Expired`); `basis` (see below); `bracketed_paste` (2004 off, or its epoch differs, gives `BracketedPasteChanged`); then `recheck`, run on the writer thread inside `catch_unwind` with every earlier unit finished and nothing else writing. **Final check:** after the recheck returns, the writer re-reads the exit, input-broken, mode-word, and deadline atomics immediately before the first write call and refuses on any change (a recheck can take time while the drain parses a `DECRST 2004`). The writer thread's exclusive ownership of `input` is the arbitration lock of mvp §2.3: a recheck that passes is followed by the unit's first byte with no write of any origin in between. Tests assert the order only where a consumer depends on it (`Expired` before `Recheck`; `ChildExited` before everything except `Closed`).

**Basis.** `Basis::seq` is the highest write `seq` whose completion the daemon's gate view has applied, completions being applied in `seq` order (mvp §4 PR 10 obligation). The writer refuses `WriteSinceBasis` if a unit of a class in `counted` started with `seq > basis.seq`, and `BasisUnknown` if `basis.seq` is above the highest `seq` assigned so far (a basis the writer cannot verify). Refused units never count; a started unit counts from its first write call, whatever it wrote. The check reads the monotonic counters, never the ring, so ring eviction cannot hide a write.

**Started units.** A started unit is committed: `not_after` no longer applies (herdr: "committed Enter cannot expire", `src/pty/actor.rs:698-713`). Before each later `Bytes` part the writer checks the exit atomic: if the child has exited, the remaining parts are skipped and the record is `Incomplete { error: "child_exited" }`, so a submit is never sent to a descendant that still holds the PTY. During a `Pause` the writer waits on the queue condvar until the pause ends, the queue closes, or the child exits; a close gives `Incomplete { error: "closed_in_pause" }`. An I/O error ends the unit as `Incomplete` with the bytes written so far and marks the input broken: every later unit is refused `InputBroken` and `on_event(InputBroken)` fires once.

**Short writes.** Each `Bytes` part is passed to `write`; if the call accepts less, the writer writes the rest in further calls and increments `split_writes`. The backends guarantee whole acceptance (§2.1), so a nonzero `split_writes` is a backend defect made visible, not a normal path.

**Position and the origin log.** `seq` orders all records of a session, started or refused. A unit's span is contiguous, and the spans of started units, in `seq` order, partition the input stream exactly. Three monotonic counters, `last_started(class)` for `Input`, `Human`, `Reply`, advance when a unit of that class makes its first write call; they are independent of the ring. The writer puts the record in the in-progress slot and advances the counter before its first write call, so a hook that arrives while a human write is blocked already sees it. Records also go to a ring of `limits.log_records` entries; `writes_after(seq)` returns a copy of the records after `seq`, or `LogTruncated`. The ring is diagnostic: no guard and no ack rule depends on it.

**How the daemon uses it** (PR 10 and PR 11 decide the details; these are the facts they get):

- **Send path.** Admit with `Basis { seq: applied, counted }`, where `counted` is the adapter's policy (default `ClassSet::ALL`, VT replies included, keeping mvp §2.3's "last write of any origin" literal); `bracketed_paste` for bracketed paste; and a `recheck` that re-reads the daemon's in-memory gate view (process running, same launch, turn idle, no other input in flight except this one). Submit `WriteUnit { Input, [text, Pause(submit delay), submit] }`.
- **Outcome mapping** to `core::input::InputInput`: `Written` → `WriteCompleted`; `NotStarted(_)` and every synchronous `SubmitError` → `RecheckFailed` (known zero bytes, so `cancelled`); `Incomplete` → `WriteOutcomeUnknown`. A completion can arrive after `launch_exited` has already moved the input to `unconfirmed`; the daemon then ignores it. The daemon validates shape and size before committing `sending` where it can, so `SubmitError` after `sending` is the rare case.
- **Ack attribution.** At hook intake the daemon reads `last_started(counted)`; a counted unit with `seq` in `(input.seq, that value]` makes the ack non-attributable. A write that started after the hook was received does not. Which classes count is the same per-adapter policy.
- **`human_write`.** The completion of an `Attach`, `Key`, or `CardKey` unit carries the record; `origin.human_write()` gives the payload's origin. A partial human write (`Incomplete` with `bytes_written > 0`) is still a human write.

**Bounds.** `submit` never waits for queue space or PTY I/O: the user lane holds at most `limits.queue_units` units and `limits.queue_bytes` bytes; past that it fails `QueueFull` with nothing written and no record. A unit with a bad shape, over `limits.max_unit_bytes`, or with origin `VtReply` fails at submit.

### 2.4 Drain

Lifted from `spikes/pty-hooks/src/os/drain.rs`: the drain owns the output read end and reads until EOF or a read error on every path, holds output (up to `DRAIN_HOLD` = 1 MiB) until its consumer attaches, then forwards everything through EOF, never depends on its consumer, and reports how it ended (`OutputEnd::Eof` or `Error`, never confusing the two). One change: the consumer is a sink callback with an ordered handoff instead of the spike's unbounded `mpsc::channel` reader (`drain.rs:93`, `:157-176`), so the crate holds no unbounded buffer and output needs one thread, not two.

```rust
pub(crate) struct Drain { /* thread "pty-drain" */ }
pub(crate) type DrainSink = Box<dyn FnMut(DrainEvent<'_>) + Send>;
pub(crate) enum DrainEvent<'a> { Bytes { offset: u64, bytes: &'a [u8] }, End(&'a OutputEnd) }
impl Drain {
    pub(crate) fn start(output: impl Read + Send + 'static) -> io::Result<Drain>;   // on failure `output` is dropped first
    pub(crate) fn attach(&self, sink: DrainSink) -> Result<(), HoldOverflow>;
    pub(crate) fn bytes(&self) -> u64;
    pub(crate) fn finish(self, bound: Duration) -> Option<OutputEnd>;              // waits and joins; None: thread abandoned
}
pub(crate) fn close_bounded(close: impl FnOnce() + Send + 'static, bound: Duration) -> Option<Duration>;
```

**Ordered handoff.** `attach` runs on the attaching thread: it delivers the held bytes, in order and outside the drain's lock, then any further bytes the drain held meanwhile, and, if the read loop already ended, the `End`; only when the held buffer is empty does it switch the slot to live under the lock, after which the drain thread calls the sink directly. Bytes therefore reach the sink in read order exactly once, and a startup query followed by silence is parsed (and answered) without waiting for another read.

**Guarantees.** (a) Every byte read is passed to the attached sink in order, or, before attach, held; output beyond the hold before attach makes `attach` fail with `HoldOverflow`, which fails `Session::start` (an incomplete screen is never shown as complete). (b) The drain thread never blocks on a consumer. The session's sink splits each chunk before every ESC byte, feeds each piece to the VT under the VT lock and publishes the mode word after it (§2.6), then releases the lock and dispatches the piece to subscribers. VT feeding and each subscriber call run inside `catch_unwind`: a VT panic sets `vt_failed` (snapshots return `VtUnavailable`, the responder stops, `on_event(VtFailed)` fires) and reading continues; a subscriber panic or `Stop` removes that subscriber. A subscriber that calls `subscribe` or `unsubscribe` from inside its callback has the change applied after the current dispatch. (c) Reading continues after the session's consumers are gone, until EOF, so a child is never blocked on a full output pipe. (d) `finish` waits at most its bound for the end after `control.close`, then joins or abandons the thread.

### 2.5 VT Responder

Lifted from `spikes/pty-hooks/src/vt.rs`: `Term<Responder>` with `Config { kitty_keyboard: true, .. }` and a `vte` `Processor`, fed by the drain thread. The `EventListener` turns each reply event into one reply write of origin `VtReply { kind }`:

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

**One reply per write.** Each event becomes its own reply write of exactly that reply, one `write` call and one record; replies are never merged with each other or with other bytes (mvp §4 PR 2 obligation; conhost loses a sequence split across writes, arch §5).

**Ordering.** Replies are enqueued under the VT lock while the piece that caused them is parsed, into the reply lane (FIFO, at most `limits.reply_queue` replies), and written at unit boundaries by the progress rule (§2.3): in query order, ahead of user units queued before them, never inside a unit. Replies have no guard. A reply that does not fit the lane, or arrives after child exit or input breakage, is discarded: it gets no record, `ResponderStatus` counts it (`dropped_overflow`, `dropped_after_end`), and `on_event(ReplyDropped)` fires for an overflow, since a CLI may be waiting for it.

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

**Walker** (rewrite of `vt.rs:234-268`). For each visible row and column of `term.grid()`: a cell with `WIDE_CHAR` is `WideLead` with `cell.c` plus its `zerowidth()` chars; a cell with `WIDE_CHAR_SPACER` is `WideTail` with empty text; a `LEADING_WIDE_CHAR_SPACER` (the last column left blank when a wide character wrapped, `term/cell.rs:22-28`) is a `Single` `" "`; every other cell is `Single` with `cell.c` plus zero-width chars. A wide character therefore occupies two cells, and the cells of a line always number `cols`, so column positions after it are exact. Width is alacritty's: an emoji with VS16 (`❤️`) stays one cell (spike §3).

**Mode word.** `TermModes` (10 bits) and `bracketed_paste_epoch` (54 bits) are packed into one `AtomicU64`, so `modes()` is one tear-free load. The drain splits each chunk before every ESC byte and publishes the word after parsing each piece; the epoch increments on every change of `bracketed_paste`, so `DECRST 2004` followed by `DECSET 2004` within one read advances it twice.

**Grid encoding (D9).** `to_payload` gives `VtSnapshotTaken { grid, cols, rows, cursor: VtCursor { row, col, visible }, modes: VtModes { bracketed_paste, alternate_screen } }` with `grid = Content::text(json)`:

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
    pub startup_output: Vec<u8>,                                 // written to the pipe before spawn returns
    pub kill: KillBehavior,                                      // ExitOnKill (default) | Ignore | ExitAfter(Duration)
}
pub struct FakeChild { /* … */ }
impl FakeChild {
    pub fn emit(&self, bytes: &[u8]);                            // child output (blocks only on a full pipe)
    pub fn on_input(&self, f: impl FnMut(&[u8]) -> Vec<u8> + Send + 'static);  // scripted reaction, e.g. echo or queries
    pub fn close_output(&self);                                  // EOF
    pub fn fail_output(&self, kind: io::ErrorKind);              // the drain's next read returns this error
    pub fn exit(&self, e: ChildExit);
    pub fn hold_writes(&self) -> WriteHold;                      // the next write() blocks until released or dropped
    pub fn fail_writes_after(&self, bytes: u64, kind: io::ErrorKind);
    pub fn short_writes(&self, max: usize);                      // write() accepts at most `max` bytes per call
    pub fn panic_on_next_write(&self);
    pub fn writes(&self) -> Vec<Vec<u8>>;                        // every write() call's bytes, in order (call boundaries kept)
    pub fn input(&self) -> Vec<u8>;
    pub fn log(&self) -> Vec<FakeEvent>;  // Spawned, WriteEntered, Write(n), Resize(size), ResizeFailed, Kill { extra }, InputDropped, ControlClosed, OutputClosed, Exited
    pub fn fail_next_resize(&self);
}
impl WriteHold { pub fn wait_entered(&self, deadline: Instant) -> bool; pub fn release(self); }
```

`Session` has `#[cfg(feature = "fake")]` start faults (`SessionConfig::start_fault: Option<StartFault>` with `Writer` and `Waiter`, failing that thread's creation) and `fail_vt_on_next_piece`. Identity: `pid` from a counter, `start_time` `fake-<n>`, `group` `fake-<n>`. `kill_tree` records its `extra` and reports `method: "fake"` and `tree_dead` per `KillBehavior`. `WriteHold::wait_entered` is the deterministic signal that the writer is blocked inside `write`; `FakeEvent` order is what the close-order tests assert.

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
| | B. A reply lane also served inside a unit's pause | herdr writes responses during a submission's delay (`actor.rs:155-163`, test `:563-578`) | Rejected: a unit's bytes would no longer be one span, so spans could not partition the stream, and a reply could land between a prompt and its submit with unverified effect on the CLI. |
| | C. A reply lane served only at unit boundaries, by a snapshot count | — | **Adopted.** Contiguous spans; bounded reply delay; no starvation of user units. |
| **Reply atomicity** | A. Fail a reply or escape-bearing part on a short accept | — | Rejected: the split hazard is conhost's, and the Windows backend obligation is whole acceptance on a synchronous pipe; the Unix tty does not lose split sequences. |
| | B. One write call per part, looping on a short accept and counting splits | — | **Adopted**; revisited if PR 4 observes short writes. |
| **Gate recheck** | A. The spike's revocable `GateState` (2004 epoch + revocation flag) as the whole recheck | spike `pty.rs:21-63` | Partly adopted: the 2004 epoch is `Guard::bracketed_paste`. Revocation and the rest of mvp §2.3's gate live in the daemon. |
| | B. Mechanical checks in the writer (deadline, exit, basis, 2004, final atomics) plus a daemon closure run on the writer thread | — | **Adopted.** The basis closes the window where a human write happened but is not yet in the gate view; which origins count stays the daemon's. |
| **Origin log** | A. Unbounded in memory | — | Rejected: attach keystrokes grow it without bound. |
| | B. Only in the event store | — | Rejected: one event per keystroke and per reply; mvp §2.1 appends `human_write` only on the first human write after evidence. |
| | C. Monotonic per-class counters for decisions, a bounded ring for diagnostics | herdr orders responses against input by offset (`src/pane/terminal.rs:1491-1530`) | **Adopted.** Ring eviction cannot hide a write from a guard or an ack. |
| **Resize locking** | A. Resize PTY and VT under the VT lock | — | Rejected: ConPTY's resize can wait on conhost, conhost on the drain, the drain on the VT lock. |
| | B. A control lock; PTY first without the VT lock, then the VT | spike `pty.rs:385-399` | **Adopted.** |
| **Output consumers** | A. Channel reader per consumer (spike `drain.rs:157-176`) | spike | Rejected: unbounded channel, one extra thread. |
| | B. Sink callbacks with an ordered handoff | herdr `on_read` callback (`actor.rs:22`, `:39-44`) | **Adopted.** The drain can never be blocked by a consumer. |
| **Async** | A. tokio in `pty-runtime` | Codex `utils/pty` | Rejected (see §2 threading model). |
| | B. std threads, callbacks, and a no-tokio CI check | spike | **Adopted.** |

## 4. Same-Class Inventory

Every MVP path where bytes reach the PTY or PTY output is consumed:

| Path | PR | How this design covers it |
|---|---|---|
| Send path: text + submit | 10 (encoders 5) | `Input` unit `[text, Pause, submit]`, `Guard` with applied basis, 2004 epoch, and gate recheck; outcome mapping and ack facts in §2.3. |
| Codex positional first prompt | 10, 14 | No PTY write: it is in `SpawnSpec::argv`; `SpawnError` tells whether it may have run. |
| Attach typing and recognised pastes | 11 | `Attach` units; a paste is one unit without submit; queued behind any input unit, never inside it; `QueueFull` is returned to the attach client. |
| Attach screen | 11 | `subscribe` gives a diagnostic snapshot plus gapless bytes after it and reports `Lagged`; the repaint mechanism is PR 11's. `resize` follows the attach client's size. |
| Raw `key`, keypad, `surface_action send_key` | 10, 19, 20 | `Key` units with a recheck (process running, owner). Arrow encoding reads `modes().app_cursor`; a recheck can refuse when the mode changed since encoding. |
| Approval card keys | 10, 19 | `CardKey { approval_id }` with a recheck that the card is still open (409 otherwise, mvp §2.10); class `Human`, mapped to `card_key`, which does not close the gate. |
| VT responder | 2 | `VtReply` writes in the reply lane, one write each, at unit boundaries (§2.5); class `Reply`, counted by default. |
| Gate timeout, attention, approval card snapshot | 10, 13 | `snapshot().to_payload()` → `vt_snapshot_taken`; `CloseOutcome::final_snapshot` after teardown. |
| Gate's DECSET 2004 condition | 10 | `ModeView` before the decision, `Guard::bracketed_paste` and the final check at the writer. |
| Resize during a unit | 10, 11 | Independent of the writer; the daemon's recheck can refuse a unit whose decision a resize invalidated. |
| Doctor PTY round trip | 21 | A `Session` over the native backend: VT query (responder record), paste echo (`Input` or `Key` unit), tree kill; `evidence` carries the ConPTY module. |
| Thread state | all | No API turns output into state input: output reaches only the VT, sinks, and snapshots (invariant 1). |

## 5. Resource Paths

`Limits` defaults (internal; PR 10 may tune): `queue_units` 256, `queue_bytes` = `max_unit_bytes`, `max_pause` 5 s, `reply_queue` 64, `log_records` 4096, `subscribers` 8, `DRAIN_HOLD` 1 MiB. `max_unit_bytes` = 4 MiB (the mvp §2.5 body limit) × 36 + 64 = 150 995 008 bytes: 36 is the worst-case `win_keys` expansion per input byte, one LF becoming a Shift+Enter record pair `ESC[13;28;13;1;16;1_ESC[13;28;13;0;16;1_` of 36 bytes (spike `paste.rs:427`; a two-byte UTF-8 scalar becomes at most 34 bytes), and 64 covers the padding spaces and the submit. A test derives it from these constants, so any prompt the HTTP layer accepts fits one unit. `CloseBounds` defaults: `kill` 5 s, `input` 1 s, `close` 2 s, `drain` 2 s, `join` 1 s. The waiter polls `wait_timeout(100 ms)`.

| Resource | Created by | Bound | Ends on |
|---|---|---|---|
| `pty-drain` thread | backend (`Drain::start`) | one per PTY | EOF or read error; joined by close step 5, else abandoned and reported |
| `pty-writer` thread | `Session::start` | one per session | close step 3, or after an I/O error once the queue is closed; joined in step 6 or abandoned |
| `pty-wait` thread | `Session::start` | one per session | exit observed, `WaitFailed`, or close; polls every 100 ms |
| Windows close helper | backend (`close_bounded`) | one per close | `ClosePseudoConsole` returns; abandoned past its bound and reported |
| User lane | `Session` | 256 units, `queue_bytes` | close step 1 completes the rest `Closed` |
| Reply lane | `Session` | 64 replies | discarded and counted at close |
| Origin log ring | `Session` | 4096 records | dropped with the session |
| Counters, in-progress slot | `Session` | 3 counters, one slot | dropped with the session; the slot's completion fires once (writer or closer) |
| Drain hold | `Drain` | 1 MiB | handed to the sink at attach |
| Subscribers | `subscribe` | 8 | `Stop`, panic, `unsubscribe`, output end, close |
| Completion | `submit` | one per unit | exactly once: written, refused, incomplete, `Cancelled` at cancel, `Closed` at close, or `unknown_after_close` |
| Output pipe / input / control / child | backend, owned by `SpawnedPty` then `Session` | one each | close steps 2–5, or the guard's teardown |

**Endings.**

| Ending | What happens |
|---|---|
| Spawn failure | The backend returns `NeverStarted` or `StartedThenFailed` with its cleanup result (§2.1); nothing is handed over. |
| `SpawnedPty` dropped without `start` | Its teardown: kill, drop input (no bytes), close, finish drain, each bounded. |
| `Session::start` fails (hold overflow, thread creation) | The guard's teardown; `StartError` carries its reports. The input is still in the guard, so no thread owns it. |
| Child exit | Exit published, then `Exited` once; not-started units refused `ChildExited`; a started unit stops before its next part; drain continues to EOF; session stays until close. |
| `kill` | `kill_tree(extra, deadline)`; I/O stays up so the final output and snapshot remain available. |
| `close` / `Drop` | Close order of §2.2, each step bounded; blocked threads abandoned and named; `Released` says what was confirmed. `Drop` blocks at most the sum of the `CloseBounds`; the daemon calls `close` itself from `spawn_blocking`. |
| Reader EOF | `OutputEnded(Eof)`; sinks get `End`; the VT and snapshots stay. |
| Read error | `OutputEnded(Error)`, never reported as EOF. |
| Write error | Unit `Incomplete`; `InputBroken`; later units refused. |
| Panic in a sink (VT or subscriber) | Caught on the drain thread; VT → `VtFailed`, subscriber → removed; reading continues. |
| Panic in a recheck or completion | Recheck → `NotStarted(RecheckPanicked)`; completion → ignored. |
| Panic in the writer thread (e.g. a backend `write`) | A drop guard closes the queue, completes queued units `NotStarted(Closed)`, records the unit in progress as `Incomplete { error: "writer_panicked" }`, marks input broken. |
| Panic in the waiter | Drop guard emits `WaitFailed`. |

Every wait of the crate is bounded: `CompletionReceiver::recv_deadline`, `OutputReceiver::recv_timeout`, `kill(deadline)`, `close(bounds)` (all six steps), `wait_timeout`, `Drain::finish(bound)`, `close_bounded`, the writer's pause (`max_pause`). Two blocking calls have no deadline of their own, a `write` to a child that stopped reading and a `read` the backend has not unblocked; close does not wait on them past its budgets, reports their threads as abandoned, and their endpoints are released when the call returns.

## 6. Purpose Layer and Plan Layer

### 6.1 Purpose Layer: Acceptance Criteria

Checked by running this crate's tests on `macos-latest` and `windows-latest` CI with `FakePty`.

| ID | Criterion (EARS) | Source | How to observe |
|---|---|---|---|
| PR2-P-1 | The crate shall provide a `PtyRuntime` trait that `FakePty` implements on both OSes, with the backend obligations stated so PR 3 and PR 4 can implement it unchanged. | mvp §4 PR 2; invariant 3 PTY Seam | `FakePty` passes every session test on both runners; §2.1 lists what PR 3 and PR 4 supply (their own PRs prove the native backends). |
| PR2-P-2 | When any write reaches the PTY, the writer shall record its origin and position. | mvp §4 PR 2 obligation | Records partition the fake child's input stream, with origins (PR2-CL-1). |
| PR2-P-3 | When the CLI sends a terminal query, the responder shall answer it in exactly one write. | mvp §4 PR 2 obligation; arch §4.5 | Fake child write calls each hold one whole reply (PR2-CL-9). |
| PR2-P-4 | When the screen holds a wide character, the snapshot shall show it on two cells. | mvp §4 PR 2 obligation; P-12 | Snapshot cells and the decoded payload grid (PR2-CL-12, CL-23). |
| PR2-P-5 | While a unit is being written, no other byte shall reach the PTY, and a unit refused before its first byte shall write zero bytes. | mvp §2.3 writer revalidation | Fake input bytes (PR2-CL-2, CL-3, CL-5). |
| PR2-P-6 | When a session ends in any way, `close` shall return within its budgets and report which resources it confirmed released, and output shall be read until EOF whenever the backend delivers it. | mvp §4 PR 2 "drain"; spike §4.6–§4.7 | Fake event order, `Released`, timings (PR2-CL-17, CL-18, CL-19). |

### 6.2 Plan Layer: Claims

| ID | Claim (EARS) | Source | Falsifying test |
|---|---|---|---|
| PR2-CL-1 | When a unit starts or is refused, the writer shall assign it the next `seq` and record its origin; the spans of started units shall be contiguous and, in `seq` order, partition the PTY input stream. | PR 2 obligation | `writer::records_partition_the_input_stream`, proptest `writer::records_partition_under_random_units` (random units, replies, and refusals) |
| PR2-CL-2 | While a unit is in progress, no byte of another unit or of a reply shall be written; replies shall be written only at unit boundaries. | §2.3, §2.5 | `writer::human_write_waits_for_the_unit`, `writer::reply_waits_for_the_unit_boundary` (reply emitted during a pause lands after the submit) |
| PR2-CL-3 | If a unit is cancelled, closed, after child exit, after input breakage, past `not_after`, after a counted write past its basis, with an unverifiable basis, with 2004 off or its epoch changed, or its recheck fails or panics, then the writer shall record `NotStarted` with that reason and write zero bytes. | mvp §2.3 | `writer::each_refusal_writes_zero_bytes` (one case per reason), `writer::expired_precedes_recheck` |
| PR2-CL-4 | When a basis is given, the writer shall refuse if a unit of a counted class started with a higher `seq`, including a partial or still-blocked write, and shall not refuse for a class outside `counted` or for refused units, whatever the ring holds. | §2.3 basis | `writer::basis_counts_a_blocked_human_write` (`hold_writes` + `wait_entered`), `writer::basis_counts_a_partial_human_write` (`fail_writes_after`), `writer::basis_ignores_uncounted_classes`, `writer::basis_survives_ring_eviction` (5000 replies) |
| PR2-CL-5 | When a recheck passes, every earlier unit shall have finished, and the writer shall refuse if the exit, input, mode word, or deadline changed before the first write call. | mvp §2.3 arbitration lock | `writer::recheck_sees_every_earlier_write`, `writer::decrst_during_recheck_refuses` (barrier inside the recheck) |
| PR2-CL-6 | When a unit has started, the writer shall write its remaining parts regardless of `not_after`; if the queue closes during a pause or the child exits before a later part, the rest shall be skipped and the record shall be `Incomplete`. | herdr committed submit; mvp §2.3 | `writer::started_unit_ignores_its_deadline`, `writer::close_in_pause_skips_the_submit`, `writer::exit_in_pause_skips_the_submit` |
| PR2-CL-7 | If a write fails, then the unit shall be `Incomplete` with the bytes written, `InputBroken` shall fire once, and later units shall be refused. | §2.3 | `writer::write_error_breaks_input` |
| PR2-CL-8 | When `submit` gets a full lane, a bad shape, a unit over `max_unit_bytes`, or a `VtReply` origin, it shall fail at once with nothing written and no record, without waiting for queue space or PTY I/O; `max_unit_bytes` shall cover the largest body under the worst `win_keys` expansion. | §2.3 bounds, §5 | `writer::full_queue_fails_without_blocking` (writes held), `writer::bad_shapes_are_refused`, `limits::max_unit_bytes_covers_the_largest_body` |
| PR2-CL-9 | When the VT emits a reply, the writer shall write it as one write call holding exactly that reply, in query order. | PR 2 obligation | `responder::each_reply_is_one_write`, proptest `responder::replies_are_single_writes` (random output with embedded known queries; asserts the exact count and bytes of the replies) |
| PR2-CL-10 | When the CLI sends DA1, DA2, DSR 5, DSR 6, the kitty query, DECRQM, CSI 14 t, CSI 18 t, or OSC 4/10/11/12, the responder shall answer as in §2.5, ahead of user units queued before it; an OSC 52 read shall get no reply. | arch §4.5; spike B-5 | `responder::answers_each_query`, `responder::reply_precedes_queued_units`, `responder::clipboard_read_is_not_answered` (an OSC 52 read followed by DSR 5: exactly one reply, `ESC[0n`) |
| PR2-CL-11 | Under a stream of replies, a queued user unit shall still be written at the next boundary; a reply the lane cannot hold shall be counted and reported by `ReplyDropped`. | §2.3 progress rule | `responder::replies_cannot_starve_units` (`on_input` answers every reply with a new query), `responder::overflow_is_reported` (65 queries behind a held write) |
| PR2-CL-12 | When the grid holds a wide character, the snapshot shall give it a `WideLead` and a `WideTail` cell, every line shall have `cols` cells, zero-width characters shall join their base cell, and a wrapped wide character shall leave a blank cell at the line end. | PR 2 obligation | `snapshot::wide_chars_occupy_two_cells`, `snapshot::wrapped_wide_char_leaves_a_blank`, `snapshot::combining_mark_joins_its_base` |
| PR2-CL-13 | The mode word shall reflect DECSET 2004, 1049, 25, DECCKM, DECKPAM, and kitty flags in one load, and the 2004 epoch shall advance on every transition, including two within one read. | mvp §2.3 gate | `snapshot::modes_follow_decset`, `snapshot::epoch_counts_transitions_within_a_read`, proptest `snapshot::mode_word_is_never_torn` |
| PR2-CL-14 | When a valid size is given, the PTY shall be resized first, then the grid and the responder's size, and output parsed after that shall be laid out at the new size; an invalid size or a failed PTY resize shall change neither side; the size before any resize shall be the spawn size. | spike C-4 | `session::resize_moves_pty_grid_and_responder`, `session::failed_resize_changes_nothing` (`fail_next_resize`), `session::invalid_size_is_refused`, `session::startup_size_answers_csi_18t` |
| PR2-CL-15 | When a sink subscribes, it shall get a snapshot and then exactly the output bytes after the snapshot's offset; after output ended it shall get `End` at once; a full `channel_sink` shall yield `Lagged`, distinct from `End`. | §2.2 subscribe | `session::subscribe_is_gapless` (subscribe between pieces of one chunk), `session::subscribe_after_end_gets_end`, `session::full_channel_sink_lags` |
| PR2-CL-16 | The drain shall read until EOF or a read error on every path, deliver held bytes and an already-observed end at attach in order without another read, report a read error as an error, refuse an overflowed hold, and never block on a sink; a panicking or stopping sink shall not stop it. | spike F-2c, §4.6 | unit tests in `src/drain.rs`: `final_output_reaches_the_sink`, `read_error_is_not_eof`, `hold_overflow_is_reported`, `hung_close_is_bounded`, `startup_query_then_silence_is_delivered`, `end_before_attach_is_delivered`; `session::sink_panic_keeps_draining`, `session::vt_panic_marks_vt_failed` (`fail_vt_on_next_piece`), `session::read_error_reaches_status` (`fail_output`) |
| PR2-CL-17 | When a session closes, it shall settle every queued completion itself, kill the tree with the given extra identities, release the input, close the PTY, finish the drain, and join threads, in that order, each step within its budget. | spike §4.7 | `session::close_order` (FakeEvent order, `Kill { extra }`), `session::close_settles_queued_units_with_a_blocked_writer` |
| PR2-CL-18 | If the writer is blocked when `close` runs, then `close` shall return within the sum of its budgets, fire the in-progress completion once as `Incomplete { unknown_after_close }`, name the writer as abandoned, and report the input as not released. | §2.2 close | `session::close_is_bounded_with_a_blocked_write` (`hold_writes` + `wait_entered`, `KillBehavior::Ignore`; completion count checked after the hold is released) |
| PR2-CL-19 | If a `SpawnedPty` is dropped without `start`, or `start` fails, then the tree shall be killed before the input is dropped and the PTY closed, and dropping the input shall write no bytes. | §2.1, §2.2 start | `session::dropped_spawn_tears_down`, `session::start_failure_kills_first` (hold overflow via `FakeScript::startup_output`; `StartFault::Writer`; `StartFault::Waiter`) |
| PR2-CL-20 | When the child exits, the session shall publish the exit before `Exited`, emit `Exited` once, and refuse every unit not yet started with zero bytes. | mvp §4 PR 10 "write-boundary recheck after exit" | `session::exit_is_reported_once`, `session::submit_from_exited_callback_is_refused` |
| PR2-CL-21 | `last_started` shall be monotonic per class and independent of the ring; `writes_after` shall return every record after `seq`, or `LogTruncated`. | §2.3 | `writer::counters_are_monotonic`, `writer::writes_after_reports_truncation` |
| PR2-CL-22 | `WriteOrigin` shall map `Attach`, `Key`, `CardKey` to the `human_write` origins and to class `Human`, `Input` to `Input`, `VtReply` to `Reply`. | mvp §2.1, §2.3 | `origin::maps_to_human_write_origins_and_classes` |
| PR2-CL-23 | `Snapshot::to_payload` shall carry cols, rows, the zero-based cursor, `VtModes`, and the grid in the §2.6 encoding, and `decode_grid` shall reject every malformed shape listed there. | D9; mvp §2.1 `vt_snapshot_taken` | `snapshot::payload_matches_the_snapshot`, `snapshot::grid_examples` (the §2.6 examples as literal strings), `snapshot::malformed_grids_are_rejected`; `core` golden `vt_snapshot_taken.json` |
| PR2-CL-24 | When `close` waits, each step shall end by its budget, and every other bounded method of §5 shall return by its deadline. | §5 | `session::close_steps_end_by_their_budgets` (blocked write, `KillBehavior::Ignore`, no output EOF), `session::receivers_end_by_their_deadlines` |
| PR2-CL-25 | `chaldea-pty-runtime` shall build without tokio. | D2 | CI dependency check (§7) |
| PR2-CL-26 | When a backend accepts a part short, the writer shall complete it in further calls and record the number of splits. | §2.3 short writes | `writer::short_writes_are_counted` (`short_writes(3)`) |
| PR2-CL-27 | If an approval card or other recheck condition closes after a unit was submitted, then the recheck shall refuse it with zero bytes. | mvp §2.10 | `writer::card_key_refused_after_enqueue` |

Obligation mapping: "origin and position of every write" → CL-1, CL-4, CL-21, CL-22; "replies in one write each" → CL-9, CL-10, CL-11; "wide characters on two cells" → CL-12, CL-23.

## 7. Tests and Completion Commands

All tests above run in `cargo test --workspace --locked` on `macos-latest` and `windows-latest` with `FakePty`; none needs a real PTY, so PR 2 adds no OS-specific test. The VT, responder, and snapshot tests feed `Vt` directly or through `FakeChild::emit`; writer tests block the writer with `hold_writes` and wait for `wait_entered` before acting, so "while the writer is blocked" is a fact, not a timing guess. Timing assertions use generous bounds (seconds, not milliseconds) so they hold on loaded runners; pauses in tests are 10–50 ms. Drain tests are unit tests in `src/drain.rs` because `Drain` is crate-private.

Completion commands (mvp §5, extended):

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
deps="$(cargo tree -p chaldea-core -e normal --prefix none)" && ! grep -Eq '^(tokio|rusqlite|axum) ' <<<"$deps"
deps="$(cargo tree -p chaldea-pty-runtime -e normal --prefix none)" && ! grep -Eq '^tokio ' <<<"$deps"
```

plus the `workspace` job green on both runners with the new step, and the `pty-hooks` job unchanged.

## 8. Decisions

**D1. The trait is the raw spawn; `Session` is shared.** One writer, drain, and VT for every backend, exercised by FakePty in every test. arch §4.5's operation list maps onto `Session` (§3 "Trait level").

**D2. No tokio; callbacks bridge to the daemon.** Blocking PTY ends, one-thread writer arbitration, and runtime-free tests; checked in CI.

**D3. The crate records; the daemon decides what counts.** Every write carries its origin and class, including VT replies (`Reply`). Which classes close the gate or make an ack ambiguous is the daemon's per-adapter policy, passed as `Basis::counted` and applied to `last_started`; the default counts every class, so mvp §2.3's "last write of any origin" holds literally. Exempting replies for an adapter needs evidence that the CLI consumes them separately (PR 12 captures, the P-13 query log) and is added as an adapter capability.

**D4. Replies only at unit boundaries.** A unit's bytes stay one contiguous span, and no reply lands between a prompt and its submit. The cost is a reply delay of at most one unit (bounded by `max_pause` plus its writes); a CLI that waits for a reply during that time waits that long.

**D5. Every `NotStarted` and every synchronous `SubmitError` maps to `RecheckFailed`, and every `Incomplete` to `WriteOutcomeUnknown`.** Zero bytes is certain only before the first write call; an I/O error even at zero bytes is treated as unknown, as mvp §2.3 does for any write outcome it cannot prove.

**D6. `FakePty` is behind a `fake` feature**, enabled for this crate's tests through a self dev-dependency and for the daemon's tests through its dev-dependency, so no default-feature build contains it (`--all-features` does).

**D7. Backends need `unsafe`; the workspace forbids it.** The root `[workspace.lints.rust] unsafe_code = "forbid"` cannot be relaxed by `#[allow]`. PR 2 writes no `unsafe` and keeps `[lints] workspace = true`. PR 3 gives this crate its own `[lints]` table (the workspace clippy set plus `unsafe_code = "deny"`) and allows `unsafe` only in `unix.rs` / `windows.rs`; the workspace value stays `forbid` for every other crate.

**D8. Writer limits and close budgets are internal defaults** (§5); none is part of a public contract. `max_unit_bytes` is derived from the body limit and the `win_keys` expansion so that no accepted prompt is refused for size here; PR 10 can cap prompts lower before `sending`.

**D9. `vt_snapshot_taken.grid` is JSON rows of cell strings** with the semantics of §2.6. It makes the geometry exact and observable by the PWA (a wide character provably takes two columns, column positions after it do not depend on font rendering), which is how the PR 2 obligation reaches the reader and what P-12's legible CJK snapshot needs. No v1 producer has persisted a grid yet (the store arrives in PR 6), so this is the cheapest point to fix it. PR 2 updates the `core` golden values (`vt_snapshot_taken.json`, `Component::VtSnapshot`). Rejected: LF-joined text (geometry left to the PWA's font) and text plus a wide-column list (two structures that must agree). Attributes added later are additive only as a separate field, since cells stay strings.

### Open Questions

None that change a public contract or are irreversible.

## Related Text in Other Documents

- `docs/mvp-design.md` §2.1 `vt_snapshot_taken` states the grid encoding (D9).
- `docs/mvp-design.md` §3 spike table: the drain is lifted with its consumer changed to a sink with an ordered handoff (§2.4); the `unsafe_code` carve-out of D7.
- `docs/mvp-design.md` §4 PR 10 obligations: the write basis is the highest writer `seq` whose completion the gate view has applied (§2.3).
- `docs/architecture.md` §4.5: the trait seals spawn and OS differences; the session built on it provides the operations.
