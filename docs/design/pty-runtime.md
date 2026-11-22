# Design: `crates/pty-runtime` (MVP PR 2)

Status: design, not implemented. Scope is the PR 2 row of `docs/mvp-design.md` §4 (cited as "mvp §N"): the `PtyRuntime` trait, `FakePty`, the single writer with its origin log, the drain, the VT responder, and the snapshot. Its design obligations: the writer records origin and position of every write; the VT responder replies in one write each; wide characters occupy two cells in snapshots.

Out of scope, designed in their own PRs: the Unix backend (PR 3), the Windows ConPTY backend (PR 4), `testkit` and FakeCli (PR 3), the input encoders in `core::input` (PR 5), and the gate, send path, and supervisor in the daemon (PR 10). This document fixes only what those PRs need from this crate: what a backend must supply (§2.1) and what the daemon can rely on (§2.2–§2.6).

Sources read for this document (paths are relative to each root):

| Source | Revision | Read for |
|---|---|---|
| Spike `spikes/pty-hooks/src/` | this repository | `pty.rs` (writer, gate), `vt.rs` (responder, snapshot), `os/drain.rs`, `os/mod.rs`, §4.4–§4.7 of `docs/spikes/pty-hooks-spike.md` |
| `alacritty_terminal` | crate 0.26.0 | `event.rs`, `term/mod.rs`, `term/cell.rs` |
| `portable-pty` | crate 0.8.1 | `lib.rs`, `unix.rs` |
| herdr (`herdrdev/herdr`) | `e8d7d97` | `src/pty/actor.rs`, `src/pty/actor/unix.rs`, `src/pty/backend.rs`, `src/pane/terminal.rs` |
| Codex (`openai/codex`, sparse `codex-rs/utils/pty`) | `8653ca9` (default branch 2026-11-22) | `src/pty.rs`, `src/process.rs` |

## 1. Goal and Files

**Goal.** A library crate, `chaldea-pty-runtime`, that turns a backend's raw PTY (a child process, an output read end, an input write end, a resize control) into a `Session`: one drain thread that reads output until EOF and feeds a headless `alacritty_terminal` VT, one writer thread that is the only code writing to the PTY and logs every write with its origin and position, and one waiter thread that observes child exit. All of it is tested on macOS and Windows CI through `FakePty`, with no real PTY.

**Files.**

```
Cargo.toml                                # members += "crates/pty-runtime"; [workspace.dependencies] += alacritty_terminal
crates/pty-runtime/Cargo.toml             # chaldea-pty-runtime; feature `fake`; [lints] workspace = true
crates/pty-runtime/src/lib.rs             # re-exports; crate doc states the lock order and the no-tokio rule
crates/pty-runtime/src/backend.rs         # PtyRuntime (sealed), SpawnSpec, SpawnedPty, PtyChild, PtyControl, ProcessIdentity, ChildExit, KillReport
crates/pty-runtime/src/session.rs         # Session, SessionConfig, SessionEvent, SessionStatus, close order
crates/pty-runtime/src/writer.rs          # WriteUnit, Part, Guard, WriteContext, WriteRecord, WriteOutcome, Refusal, origin log, writer thread
crates/pty-runtime/src/origin.rs          # WriteOrigin, ReplyKind, mapping to core::payload::HumanWriteOrigin
crates/pty-runtime/src/drain.rs           # lifted from spikes/pty-hooks/src/os/drain.rs (consumer becomes a sink callback)
crates/pty-runtime/src/vt.rs              # lifted from spikes/pty-hooks/src/vt.rs: Vt, Responder, TermModes
crates/pty-runtime/src/snapshot.rs        # Snapshot, Line, Cell, CellWidth, conversion to core::payload::VtSnapshotTaken
crates/pty-runtime/src/fake.rs            # #[cfg(feature = "fake")] FakePty, FakeChild
crates/pty-runtime/tests/{writer,responder,snapshot,session,drain}.rs
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

**Threading model.** The crate is synchronous: std threads, blocking I/O, no async runtime. PTY ends are blocking on both OSes (portable-pty's reader and writer are blocking `Read`/`Write`, `lib.rs:97-102`; ConPTY uses synchronous anonymous pipes), so an async runtime would only wrap them in blocking tasks, as Codex does (`utils/pty/src/pty.rs:241`, `:285`). Callers never block on the crate's threads: every callback the crate invokes (`Completion`, `OutputSink`, `on_event`) must return without blocking, and the crate never waits on a caller. The daemon bridges to tokio by passing callbacks that `try_send` on a tokio channel or complete a `oneshot`; it calls the few blocking methods (`close`, `kill`, `resize`) from `spawn_blocking`.

**Lock order.** VT lock → queue lock. The drain thread holds the VT lock while it feeds a chunk, enqueues replies, publishes modes, and dispatches the chunk to sinks. The writer thread takes only the queue lock and reads modes from atomics; it never takes the VT lock. `snapshot`, `subscribe`, and `resize` take the VT lock and nothing under it except the queue lock (for `subscribe`, none).

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PtySize { pub cols: u16, pub rows: u16 }   // both ≥ 1

pub struct SpawnedPty {                      // fields pub(crate): only backends build it
    pub(crate) child: Box<dyn PtyChild>,
    pub(crate) output: Drain,                // already started over the PTY's output read end
    pub(crate) input: Box<dyn Write + Send>, // the PTY's input write end
    pub(crate) control: Box<dyn PtyControl>,
    pub(crate) evidence: Vec<(&'static str, String)>,   // e.g. ("job_branch", "job_list"), ("conpty_module", …)
}

/// Methods take `&self` and may be called concurrently from different threads
/// (the waiter polls `wait_timeout` while `kill_tree` runs).
pub trait PtyChild: Send + Sync {
    fn identity(&self) -> &ProcessIdentity;
    fn wait_timeout(&self, timeout: Duration) -> io::Result<Option<ChildExit>>;
    /// Kills the whole tree; idempotent; returns by `deadline`.
    fn kill_tree(&self, deadline: Instant) -> KillReport;
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
pub enum SpawnError { NotFound, PermissionDenied, Setup { stage: &'static str, error: io::Error } }
```

`SpawnError::NotFound` and `PermissionDenied` mean the child provably never existed (the confirmed `spawn_failed` of mvp §2.3); `Setup` after the process was created is still a failure with nothing left behind, per the obligations below.

**What each backend must supply** (the obligations PR 3 and PR 4 implement against this trait unchanged):

- `spawn` returns either a complete `SpawnedPty` or an error after releasing everything it created: no process alive, no handle or descriptor open (spike F-2c). On Windows the release order is: terminate job and process, close the pseudoconsole while the drain reads, close pipe ends (spike §4.6).
- The child's environment is exactly `spec.env` (portable-pty: `env_clear` then `env`; Windows: an explicit `CREATE_UNICODE_ENVIRONMENT` block). The scrub list is the daemon's (mvp §2.7 "Env scrub").
- `output` is a `Drain` (§2.4) the backend started over its output read end. Windows starts it right after `CreatePseudoConsole`, before `CreateProcessW` (spike §4.6); Unix after `openpty`. The reader the drain owns returns `Ok(0)` once every slave end is closed: the Unix backend maps the master's `EIO` after hangup to EOF, and drops the parent's slave handle right after spawn (spike §4.7).
- `input` accepts each `write` call whole or fails: the Unix master fd is blocking; Windows `WriteFile` on a synchronous pipe completes fully. A short write is tolerated by the writer (it loops) but recorded as a split (§2.3), because conhost loses an escape sequence split across writes (arch §5).
- Dropping `input` may write bytes on Unix (portable-pty writes `\n` + VEOF on drop, `unix.rs:351-361`); the session drops it only after `kill_tree` (§2.2).
- `PtyChild::wait_timeout` never blocks past `timeout` (Windows `WaitForSingleObject(timeout)`; Unix `try_wait` polling) and must not hold a lock `kill_tree` needs.
- `kill_tree` is the PR 3 / PR 4 tree kill (`killpg` HUP→TERM→KILL plus `getsid` scan and recorded pids; job termination plus recorded descendants) and returns by its deadline; `tree_dead: false` with survivors when it could not prove death.
- `PtyControl::close` on Windows runs `ClosePseudoConsole` through the lifted `close_bounded` helper (`spikes/pty-hooks/src/os/drain.rs:182-197`), since some builds block in it until output is read.
- `evidence` carries what doctor and the real-host runs record (Windows: `job_branch` = `job_list` or `suspended`, `conpty_module`).
- Every `unsafe` block lives in `unix.rs` / `windows.rs` (see §8 D7 on the workspace lint).

### 2.2 `Session`

```rust
pub struct SessionConfig {
    pub on_event: Box<dyn Fn(SessionEvent) + Send + Sync>,   // non-blocking; called at most once per variant
    pub limits: Limits,                                       // defaults in §5
}
pub enum SessionEvent { Exited(ChildExit), WaitFailed(String), OutputEnded(OutputEnd), InputBroken(String), VtFailed }
pub enum OutputEnd { Eof, Error(String) }

impl Session {
    /// Attaches the VT sink to the drain, then starts the writer and waiter threads.
    /// On any failure after the backend returned: kills the tree and releases the PTY (order below).
    pub fn start(pty: SpawnedPty, config: SessionConfig) -> Result<Session, StartError>;
    pub fn identity(&self) -> &ProcessIdentity;
    pub fn evidence(&self) -> &[(&'static str, String)];

    pub fn submit(&self, unit: WriteUnit, guard: Guard, done: Completion) -> Result<WriteTicket, SubmitError>;  // §2.3
    pub fn write_head(&self) -> u64;                                     // seq of the last record, 0 before any
    pub fn writes_after(&self, seq: u64) -> Result<Vec<WriteRecord>, LogTruncated>;

    pub fn modes(&self) -> ModeView;                                     // atomics; no lock
    pub fn snapshot(&self) -> Result<Snapshot, VtUnavailable>;           // §2.6
    pub fn subscribe(&self, sink: OutputSink) -> Result<(Snapshot, SubscriptionId), SubscribeError>;
    pub fn unsubscribe(&self, id: SubscriptionId);
    pub fn resize(&self, size: PtySize) -> io::Result<()>;               // PTY and VT together

    pub fn status(&self) -> SessionStatus;                               // exit, output end, input state, vt_failed
    pub fn kill(&self, deadline: Instant) -> KillReport;                 // tree kill only; I/O stays up
    pub fn close(self, bounds: CloseBounds) -> CloseOutcome;             // teardown, bounded
}
impl Drop for Session { /* close(CloseBounds::default()) unless already closed */ }

pub type OutputSink = Box<dyn FnMut(OutputEvent<'_>) -> Flow + Send>;   // runs on the drain thread; non-blocking
pub enum OutputEvent<'a> { Bytes { offset: u64, bytes: &'a [u8] }, End(&'a OutputEnd) }
pub enum Flow { Continue, Stop }
pub fn channel_sink(capacity: usize) -> (OutputSink, OutputReceiver);   // std bounded channel; Stop when full
```

**Resize.** Under the VT lock: `control.resize(size)`, then on success the VT grid and the size the responder reports. Output read after the resize call is parsed at the new size (spike test `resize_resizes_the_vt_grid`, `pty.rs:492-530`, where the PTY was resized before the grid).

**Subscribe.** Under the VT lock the session takes a snapshot (with its `output_offset`) and registers the sink, so the sink receives exactly the bytes after that offset: no gap, no duplicate. This is what attach (PR 11) repaints from. At most `limits.subscribers` sinks; a sink that returns `Stop` or panics is removed and never called again.

**Close order** (`close`, `Drop`, and the failure path of `start`), each step bounded by `CloseBounds`:

1. Close the queue: new `submit` calls fail with `Closed`; units not started complete with `NotStarted(Closed)`. A unit in its pause stops there (§2.3).
2. `kill_tree(kill deadline)` unless the tree is already reported dead.
3. Release the input: the writer thread drops `input` and exits (step 2 first, so the Unix VEOF never reaches a live CLI; spike §4.7).
4. `control.close(close deadline)` while the drain keeps reading (ConPTY is never closed under an undrained pipe).
5. Wait for the drain to end (EOF or read error), bounded.
6. Join writer, waiter, and drain threads with the join bound; a thread still blocked is abandoned and named in `CloseOutcome`.

`CloseOutcome { kill: KillReport, close: CloseReport, output: Option<OutputEnd>, abandoned: Vec<&'static str>, elapsed }`. The snapshot stays readable until `close` returns; the daemon takes the final `vt_snapshot_taken` before closing when it needs one.

**After child exit.** The waiter emits `Exited` once. The writer refuses every unit that has not started (`NotStarted(ChildExited)`, zero bytes) and drops VT replies; a started unit finishes or fails. The drain keeps reading until EOF, which can come later than exit while descendants hold the PTY. The session stays up until `close`; it never kills or closes on its own.

### 2.3 Single Writer and Origin Log

One writer thread per session is the only code that writes to `input`. Callers submit **units**; a unit's bytes are never interleaved with another unit's bytes.

```rust
pub struct WriteUnit { pub origin: WriteOrigin, pub parts: Vec<Part> }
pub enum Part {
    Bytes(Vec<u8>),          // passed whole to one write_all; the encoder keeps every escape sequence inside one part
    Pause(Duration),         // ≤ limits.max_pause; the only point where VT replies may be written inside a unit
}

pub enum WriteOrigin {
    Input { input_id: InputId },            // send path (PR 10): text, pause, submit as one unit
    Attach,                                 // chaldea attach (PR 11)
    Key,                                    // owner `key`, keypad, `surface_action send_key` (PR 10, 19, 20)
    CardKey { approval_id: ApprovalId },    // key from an open approval card (D1)
    VtReply { kind: ReplyKind },            // the VT responder (§2.5)
}
impl WriteOrigin {
    /// Attach → attach, Key → key, CardKey → card_key; Input and VtReply → None.
    pub fn human_write(&self) -> Option<HumanWriteOrigin>;
    /// Every origin except VtReply: what the gate calls "a write of any origin" and
    /// ack attribution calls "a write of another origin".
    pub fn counts_as_write(&self) -> bool;
}

pub struct Guard {
    pub not_after: Instant,                    // refuse if the first byte has not been written by then
    pub basis: Option<u64>,                    // refuse if a counting write after this seq wrote any byte
    pub bracketed_paste: Option<u64>,          // refuse unless 2004 is on with this epoch (ModeView)
    pub recheck: Option<Recheck>,              // the daemon's gate / card recheck
}
pub type Recheck = Box<dyn FnOnce(&WriteContext<'_>) -> Result<(), String> + Send>;
pub struct WriteContext<'a> { pub head: u64, pub modes: TermModes, pub records_after_basis: &'a [WriteRecord] }

pub type Completion = Box<dyn FnOnce(&WriteRecord) + Send>;            // non-blocking
pub fn completion_channel() -> (Completion, CompletionReceiver);        // recv_deadline(Instant)
pub struct WriteTicket { /* seq is assigned when it starts */ }
impl WriteTicket { pub fn cancel(&self) -> bool; }                      // true: it will never start

pub struct WriteRecord {
    pub seq: u64,                    // per session, from 1, gapless, in the order units start or are refused
    pub origin: WriteOrigin,
    pub submitted_at: Instant, pub started_at: Option<Instant>, pub finished_at: Instant,
    pub byte_start: u64,             // cumulative input bytes written before this unit
    pub bytes_written: u64,
    pub split_writes: u32,           // write() calls that accepted less than their part
    pub outcome: WriteOutcome,
}
pub enum WriteOutcome { Written, NotStarted(Refusal), Incomplete { error: String } }
pub enum Refusal { Cancelled, Expired, Closed, ChildExited, InputBroken, ReplyQueueFull,
                   WriteSinceBasis { seq: u64 }, BracketedPasteChanged, Recheck(String), RecheckPanicked }
pub enum SubmitError { Closed, QueueFull, UnitTooLarge, EmptyUnit, PauseTooLong }
pub struct ModeView { pub modes: TermModes, pub bracketed_paste_epoch: u64 }
```

**Writer loop.** The writer takes the oldest unit of the user lane (FIFO). Before its first byte it checks, in this order, and on the first failure records `NotStarted(reason)` with zero bytes: cancelled; `not_after` passed (`Expired`); queue closed; child exited; input broken; `basis` (any record with `seq > basis`, `counts_as_write()`, and `bytes_written > 0` gives `WriteSinceBasis`); `bracketed_paste` (2004 off, or its epoch changed, gives `BracketedPasteChanged`); then `recheck`, run on the writer thread inside `catch_unwind`, with every earlier unit finished and nothing else writing. The writer thread's exclusive ownership of `input` is the arbitration lock of mvp §2.3: a recheck that passes is followed by the unit's first byte with no write of any origin in between.

Then it writes each part in order. A started unit is committed: `not_after` no longer applies (herdr: "committed Enter cannot expire", `src/pty/actor.rs:698-713`), and later parts are written even if the child exits meanwhile. During a `Pause` the writer waits on the queue condvar until the pause ends, writing any VT replies that arrive (and nothing else); if the queue is closed during the pause, the remaining parts are skipped and the record is `Incomplete { error: "closed_in_pause" }`. An I/O error ends the unit as `Incomplete` with the bytes written so far and marks the input broken: every later unit is refused `InputBroken` and `on_event(InputBroken)` fires once.

**Position.** `seq` orders all records of a session, written or refused; `byte_start..byte_start + bytes_written` locates a unit's bytes in the PTY input stream. The records of written units partition the stream exactly in `seq` order.

**Origin log.** The writer appends every record to a ring of `limits.log_records` entries before calling the unit's `Completion`. `writes_after(seq)` returns the records after `seq`, or `LogTruncated` when the ring no longer holds `seq + 1`; the daemon treats a truncation as an ambiguous order (the input stays `unconfirmed`).

**How the daemon uses it** (PR 10 and PR 11 decide the details; these are the guarantees they get):

- **Send path.** Capture `basis = write_head()` and `epoch = modes().bracketed_paste_epoch` first, then evaluate `core::input::gate`, commit `sending`, and submit `WriteUnit { Input, [text, Pause(submit delay), submit] }` with `Guard { not_after, basis, bracketed_paste (bracketed paste only), recheck }`. The basis covers human writes the writer performed but the store has not yet recorded. The recheck re-reads the daemon's in-memory gate view (process running, same launch, turn idle, no other input in flight).
- **Outcome mapping** to `core::input::InputInput`: `Written` → `WriteCompleted`; `NotStarted(_)` → `RecheckFailed` (zero bytes, so `cancelled`); `Incomplete` → `WriteOutcomeUnknown`.
- **Ack attribution.** At the hook, `writes_after(input_record.seq)` holds a written record with `counts_as_write()` → not attributable. VT replies never make an ack ambiguous.
- **`human_write`.** The completion of an `Attach`, `Key`, or `CardKey` unit carries the record; `origin.human_write()` gives the payload's origin.

**Bounds.** `submit` never blocks: the user lane holds at most `limits.queue_units` units and `limits.queue_bytes` bytes; past that it fails `QueueFull` with nothing written and no record. A unit over `limits.max_unit_bytes`, an empty unit, or a pause over `limits.max_pause` fails at submit.

### 2.4 Drain

Lifted from `spikes/pty-hooks/src/os/drain.rs`: the drain owns the output read end and reads until EOF or a read error on every path, holds output (up to `DRAIN_HOLD` = 1 MiB) until its consumer attaches, then forwards everything through EOF, never depends on its consumer, and reports how it ended (`OutputEnd::Eof` or `Error`, never confusing the two). One change: the consumer is a sink callback run on the drain thread instead of the spike's unbounded `mpsc::channel` (`drain.rs:93`), so the crate holds no unbounded buffer and output needs one thread, not two.

```rust
pub(crate) struct Drain { /* thread "pty-drain" */ }
impl Drain {
    pub(crate) fn start(output: impl Read + Send + 'static) -> io::Result<Drain>;   // on failure `output` is dropped first
    pub(crate) fn attach(&self, sink: DrainSink) -> Result<(), HoldOverflow>;      // held bytes first, then live
    pub(crate) fn bytes(&self) -> u64;
    pub(crate) fn finish(self, bound: Duration) -> Option<OutputEnd>;              // None: no end within bound; thread abandoned
}
pub(crate) fn close_bounded(close: impl FnOnce() + Send + 'static, bound: Duration) -> Option<Duration>;
```

**Guarantees.** (a) Every byte read is passed to the attached sink in order, or, before attach, held; output beyond the hold before attach makes `attach` fail with `HoldOverflow`, which fails `Session::start` (an incomplete screen is never shown as complete). (b) The drain thread never blocks on a consumer: the session's sink feeds the VT and dispatches to subscribers inside `catch_unwind`; a VT panic sets `vt_failed` (snapshots return `VtUnavailable`, the responder stops, `on_event(VtFailed)` fires) and reading continues; a subscriber panic or `Stop` removes that subscriber. (c) Reading continues after the session's consumers are gone, until EOF, so a child is never blocked on a full output pipe. (d) `finish` waits at most its bound (`CloseBounds::drain`, default 2 s) for the end after `control.close`.

### 2.5 VT Responder

Lifted from `spikes/pty-hooks/src/vt.rs`: `Term<Responder>` with `Config { kitty_keyboard: true, .. }` and a `vte` `Processor`, fed by the drain thread. The `EventListener` turns each reply event into one `WriteUnit { origin: VtReply { kind }, parts: [Bytes(reply)] }`:

| Query | alacritty event | Reply | `ReplyKind` |
|---|---|---|---|
| DA1 `CSI c` / DA2 `CSI > c` | `PtyWrite` (`term/mod.rs:1257-1271`) | `ESC[?6c` / `ESC[>0;<ver>;1c` | `Da1`, `Da2` |
| DSR 5 / DSR 6 | `PtyWrite` (`:1332-1347`) | `ESC[0n` / `ESC[<r>;<c>R` | `Dsr5`, `Dsr6` |
| kitty keyboard `CSI ? u` | `PtyWrite` (`:1275-1284`) | `ESC[?<flags>u` | `KittyKeyboard` |
| DECRQM private and ANSI | `PtyWrite` (`:2090`, `:2147`) | `ESC[?<m>;<s>$y` / `ESC[<m>;<s>$y` | `ModeReport` |
| text area size in cells `CSI 18 t` | `PtyWrite` (`:2268-2270`) | from the VT size | `TextAreaChars` |
| text area size in pixels `CSI 14 t` | `TextAreaSizeRequest` (`event.rs:43`, `term/mod.rs:2259`) | VT size × 8×16 px cells | `TextAreaPixels` |
| OSC 10 / 11 / 12 / 4 color | `ColorRequest` (`event.rs:37`, `term/mod.rs:1675-1688`) | fg `#d0d0d0`, bg `#1e1e1e`, cursor = fg, palette = fg | `Color { index }` |

OSC 52 clipboard reads (`ClipboardLoad`, `event.rs:31`) and stores get no reply and no effect: the daemon has no clipboard to give a CLI. Every other event is ignored.

**One reply per write.** Each event becomes its own unit with one part, so each reply is exactly one `write_all` and one record; replies are never merged with each other or with other bytes (mvp §4 PR 2 obligation; conhost loses a sequence split across writes, arch §5).

**Ordering.** Replies go to a separate reply lane (FIFO, at most `limits.reply_queue` replies), which the writer serves first at every unit boundary and during a unit's `Pause`, never inside a part. So replies keep their query order, are written ahead of user units queued before them, and can land between an input's text and its submit, as herdr allows (`src/pty/actor.rs:563-578`, test `submission_sequences_user_input_but_allows_terminal_responses`). A reply lane overflow records `NotStarted(ReplyQueueFull)`; after child exit or input breakage replies are refused like any unit. Replies have no guard.

On Windows, conhost answers DSR and DA itself and swallows OSC 10/11; the responder answers what reaches it (kitty), and replies conhost writes are outside this ordering (prior-art row 3b).

### 2.6 Snapshot

```rust
pub struct Snapshot {
    pub cols: u16, pub rows: u16,
    pub lines: Vec<Line>,             // `rows` lines of the visible screen
    pub cursor: Cursor,               // { row, col, visible: SHOW_CURSOR }
    pub modes: TermModes,
    pub output_offset: u64,           // output bytes fed into the VT when it was taken
}
pub struct Line { pub cells: Vec<Cell> }          // exactly `cols` cells
pub struct Cell { pub text: String, pub width: CellWidth }
pub enum CellWidth { Single, WideLead, WideTail }
impl Line { pub fn text(&self) -> String; }      // cells without WideTail, right-trimmed (the spike's `lines`)
impl Snapshot { pub fn to_payload(&self) -> VtSnapshotTaken; }

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

**Walker** (rewrite of `vt.rs:234-268`). For each visible row and column of `term.grid()`: a cell with `WIDE_CHAR` is `WideLead` with `cell.c` plus its `zerowidth()` chars; a cell with `WIDE_CHAR_SPACER` is `WideTail` with empty text; a `LEADING_WIDE_CHAR_SPACER` (the last column left blank when a wide character wrapped, `term/cell.rs:22-28`) is a `Single` blank; every other cell is `Single` with `cell.c` plus zero-width chars. A wide character therefore occupies two cells, and the cells of a line always number `cols`, so column positions after it are exact. Width is alacritty's: an emoji with VS16 (`❤️`) stays one cell (spike §3).

**Modes and the 2004 epoch.** After feeding each chunk the drain thread publishes `TermModes` to atomics and increments `bracketed_paste_epoch` when `bracketed_paste` differs from the previous chunk's value. A toggle off and on inside one read chunk is not observed; the guarantee the writer gives is "2004 was on, and seen continuously on at chunk granularity, from the gate decision to the first byte".

**Payload.** `to_payload` gives `VtSnapshotTaken { grid, cols, rows, cursor: VtCursor { row, col, visible }, modes: VtModes { bracketed_paste, alternate_screen } }`, with `grid = Content::text(..)` inline (the store moves it to a blob above 64 KiB, mvp §2.1). The grid encoding is open question Q1 (§8); the recommended form is JSON, one array per row, one string per cell, `""` for a `WideTail`, trailing blank `Single` cells omitted: `[["a","日","","b"],[]]`.

### 2.7 `FakePty`

Behind the `fake` feature. A `PtyRuntime` whose child is a script, so the shared drain, writer, VT, and close order run unchanged in tests on both OSes and the daemon's gate and writer tests (PR 10) need no real PTY. Output travels through `std::io::pipe()`, so the drain reads a real pipe and EOF is real.

```rust
pub struct FakePty { /* Arc state */ }
impl FakePty {
    pub fn new() -> FakePty;
    pub fn fail_next_spawn(&self, e: SpawnError);
    pub fn spawns(&self) -> Vec<SpawnSpec>;                      // every spec, in order
    pub fn take_child(&self) -> Option<FakeChild>;               // the controller of the latest spawn
}
pub struct FakeChild { /* … */ }
impl FakeChild {
    pub fn emit(&self, bytes: &[u8]);                            // child output (blocks only on a full pipe)
    pub fn on_input(&self, f: impl FnMut(&[u8]) -> Vec<u8> + Send + 'static);  // scripted reaction, e.g. echo or queries
    pub fn close_output(&self);                                  // EOF
    pub fn exit(&self, e: ChildExit);
    pub fn hold_writes(&self) -> WriteHold;                      // the next write() blocks until the hold is released or dropped
    pub fn fail_writes_after(&self, bytes: u64, kind: io::ErrorKind);
    pub fn short_writes(&self, max: usize);                      // write() accepts at most `max` bytes per call
    pub fn panic_on_next_write(&self);
    pub fn kill_behavior(&self, k: KillBehavior);                // ExitOnKill (default) | Ignore | ExitAfter(Duration)
    pub fn startup_output(&self, bytes: Vec<u8>);                // set before spawn returns: written before the session attaches
    pub fn writes(&self) -> Vec<Vec<u8>>;                        // every write() call's bytes, in order (call boundaries kept)
    pub fn input(&self) -> Vec<u8>;
    pub fn log(&self) -> Vec<FakeEvent>;                         // Spawned, Write(n), Resize(size), Kill, InputDropped, ControlClosed, OutputClosed, Exited
}
```

Identity: `pid` from a counter, `start_time` `fake-<n>`, `group` `fake-<n>`. `kill_tree` reports `method: "fake"` and `tree_dead` per `KillBehavior`. `FakeEvent` order is what the close-order tests assert.

## 3. Structure Options

| Fork | Option | Precedent | Verdict |
|---|---|---|---|
| **Backend dispatch** | A. `cfg`-selected free functions, no trait | herdr `src/pty/backend.rs:1-60` | Rejected: the daemon could not swap in `FakePty`, so its gate tests would need a real PTY. |
| | B. Reuse portable-pty's `MasterPty` / `Child` traits as the seam | spike `os/mod.rs:203-218` plugs the raw ConPTY backend in this way (`os/windows.rs:1524`, `:1615`) | Rejected: `anyhow` errors, no tree kill, no start time, single-pid `kill` (spike §3); the Windows backend would depend on portable-pty for its traits only. |
| | C. A sealed, object-safe `PtyRuntime` returning a raw `SpawnedPty`; everything above it shared | portable-pty `PtySystem` behind `native_pty_system() -> Box<dyn PtySystem>` (`lib.rs:252-256`, `:389`) | **Adopted.** One writer, drain, and VT for all backends; the trait carries only what differs per OS. |
| **Trait level** | A. The trait is the full session (`spawn`, `write_text`, `write_key`, `snapshot`, `attach`, `kill`, arch §4.5); each backend implements all of it | — | Rejected: the writer, VT, and drain would be written three times, and FakePty tests would not exercise the production ones. |
| | B. Trait = raw spawn; `Session` is one concrete type | Codex `spawn_process` returns one `ProcessHandle` for every OS (`utils/pty/src/pty.rs:150`, `process.rs:111-150`) | **Adopted.** arch §4.5's operations are `Session` methods (`submit` with an origin covers `write_text` and `write_key`; `subscribe` is `attach`). |
| **Writer** | A. A `Mutex` around the writer; each caller writes on its own thread | — | Rejected: the drain thread would block on a write to a child that stopped reading, stop reading, and deadlock with it (spike B-10, 64 KiB paste); std `Mutex` is not FIFO, so arrival order would not be write order. |
| | B. An async writer task holding the writer | Codex: tokio task, blocking `write_all` under a tokio `Mutex` (`pty.rs:259-277`) | Rejected: a blocked write stalls a runtime worker; it needs tokio in the crate. |
| | C. A dedicated writer thread with a queue | spike `pty.rs:161-234`; herdr Windows writer thread (`src/pty/actor.rs:229-232`, `:313-345`) | **Adopted**, with a bounded user lane (herdr bounds its input channel and fails fast when full, `actor.rs:87-115`) and per-unit guards rechecked before the first byte (spike `pty.rs:187-205`). |
| | D. One thread doing nonblocking reads and writes with `poll` | herdr Unix actor (`src/pty/actor/unix.rs:391`, `:541`) | Rejected: Unix-only (herdr itself uses threads on Windows); it would put OS-specific I/O outside the backends. |
| **Reply ordering** | A. Replies in the same FIFO as user writes, after any queued unit | spike `pty.rs:86-100` (one channel) | Rejected: a reply waits behind a whole queued paste and its submit delay. |
| | B. A reply lane served first at unit boundaries and pauses | herdr: responses go straight to the writer channel while user input is sequenced (`actor.rs:155-163`, test `:563-578`); herdr orders replies against input by offset (`src/pane/terminal.rs:1491-1530`) | **Adopted.** A reply is never inside a part; user writes still never interleave. |
| **Gate recheck** | A. The spike's revocable `GateState` (2004 epoch + revocation flag) as the whole recheck | spike `pty.rs:21-63` | Partly adopted: the 2004 epoch is `Guard::bracketed_paste`. Revocation and the rest of mvp §2.3's gate live in the daemon. |
| | B. Mechanical checks in the writer (deadline, exit, basis, 2004) plus a daemon closure run on the writer thread | — | **Adopted.** The basis check closes the window where a human write happened but is not yet an event; the closure keeps gate policy in the daemon and `core`. |
| **Origin log** | A. Unbounded in memory | — | Rejected: attach keystrokes grow it without bound. |
| | B. Only in the event store | — | Rejected: one event per keystroke and per reply; mvp §2.1 appends `human_write` only on the first human write after evidence. |
| | C. A bounded ring in the session, with truncation reported | — | **Adopted.** The daemon persists what it needs from completions; truncation is treated as ambiguity. |
| **Output consumers** | A. Channel reader per consumer (spike `drain.rs:157-176`) | spike | Rejected: unbounded channel, one extra thread. |
| | B. Non-blocking sink callbacks on the drain thread | herdr `on_read` callback (`actor.rs:22`, `:39-44`) | **Adopted.** The drain can never be blocked by a consumer. |
| **Async** | A. tokio in `pty-runtime` | Codex `utils/pty` | Rejected (see §2 threading model). |
| | B. std threads, callbacks, and a no-tokio CI check | spike | **Adopted.** |

## 4. Same-Class Inventory

Every MVP path where bytes reach the PTY or PTY output is consumed:

| Path | PR | How this design covers it |
|---|---|---|
| Send path: text + submit | 10 (encoders 5) | `Input` unit `[text, Pause, submit]`, `Guard` with basis, 2004 epoch, and gate recheck; outcome mapping and ack attribution in §2.3. |
| Codex positional first prompt | 10, 14 | No PTY write: it is in `SpawnSpec::argv`. |
| Attach typing and recognised pastes | 11 | `Attach` units; a paste is one unit without submit; queued behind any input unit, never inside it; `QueueFull` is returned to the attach client. |
| Attach screen | 11 | `subscribe` gives a snapshot plus gapless bytes after it; `resize` follows the attach client's size. |
| Raw `key`, keypad, `surface_action send_key` | 10, 19, 20 | `Key` units with a recheck (process running, owner). Arrow encoding reads `modes().app_cursor`. |
| Approval card keys | 10, 19 | `CardKey { approval_id }` with a recheck that the card is open (409 otherwise, mvp §2.10); it counts as a write for basis and ack but maps to `card_key`, which does not close the gate. |
| VT responder | 2 | `VtReply` units in the reply lane, one write each (§2.5); excluded from the gate and ack. |
| Gate timeout, attention, approval card snapshot | 10, 13 | `snapshot().to_payload()` → `vt_snapshot_taken`. |
| Gate's DECSET 2004 condition | 10 | `ModeView` before the decision, `Guard::bracketed_paste` at the writer. |
| Doctor PTY round trip | 21 | A `Session` over the native backend: VT query (responder record), paste echo (`Input` or `Key` unit), tree kill; `evidence` carries the ConPTY module. |
| Thread state | all | No API turns output into state input: output reaches only the VT, sinks, and snapshots (invariant 1). |

## 5. Resource Paths

`Limits` defaults (internal; PR 10 may tune): `queue_units` 256, `queue_bytes` 4 MiB, `max_unit_bytes` 1 MiB, `max_pause` 5 s, `reply_queue` 64, `log_records` 4096, `subscribers` 8, `DRAIN_HOLD` 1 MiB. `CloseBounds` defaults: `kill` 5 s, `close` 2 s, `drain` 2 s, `join` 1 s. The waiter polls `wait_timeout(100 ms)`.

| Resource | Created by | Bound | Ends on |
|---|---|---|---|
| `pty-drain` thread | backend (`Drain::start`) | one per PTY | EOF or read error; joined in close step 6, else abandoned and reported |
| `pty-writer` thread | `Session::start` | one per session | input released (close step 3), or after an I/O error once the queue is closed; joined or abandoned |
| `pty-wait` thread | `Session::start` | one per session | exit observed, `WaitFailed`, or close; polls every 100 ms |
| Windows close helper | backend (`close_bounded`) | one per close | `ClosePseudoConsole` returns; abandoned past its bound |
| User lane | `Session` | 256 units, 4 MiB | close step 1 refuses the rest with `Closed` |
| Reply lane | `Session` | 64 replies | same |
| Origin log ring | `Session` | 4096 records | dropped with the session |
| Drain hold | `Drain` | 1 MiB | handed to the sink at attach |
| Subscribers | `subscribe` | 8 | `Stop`, panic, `unsubscribe`, output end, close |
| Completion | `submit` | one per unit | called exactly once: written, refused, incomplete, or `Closed` at close |
| Output pipe / input / control / child | backend | one each | close steps 2–5 |

**Endings.**

| Ending | What happens |
|---|---|
| Spawn failure | The backend returns `Err` with nothing left (§2.1); `Session` creates nothing. |
| `Session::start` fails (hold overflow, thread creation) | `kill_tree` → drop `input` → `control.close` → `Drain::finish` → `StartError` with the reports. The tree is killed before the PTY is released. |
| Child exit | `Exited` once; not-started units refused `ChildExited`; drain continues to EOF; session stays until close. |
| `kill` | `kill_tree(deadline)`; I/O stays up so the final output and snapshot remain available. |
| `close` / `Drop` | Close order of §2.2, each step bounded; blocked threads abandoned and named. `Drop` blocks at most the sum of the `CloseBounds`; the daemon calls `close` itself from `spawn_blocking`. |
| Reader EOF | `OutputEnded(Eof)`; sinks get `End`; the VT and snapshots stay. |
| Read error | `OutputEnded(Error)`, never reported as EOF. |
| Write error | Unit `Incomplete`; `InputBroken`; later units refused. |
| Panic in a sink (VT or subscriber) | Caught on the drain thread; VT → `VtFailed`, subscriber → removed; reading continues. |
| Panic in a recheck or completion | Recheck → `NotStarted(RecheckPanicked)`; completion → ignored. |
| Panic in the writer thread (e.g. a backend `write`) | A drop guard closes the queue, completes queued units `NotStarted(Closed)`, records the unit in progress as `Incomplete { error: "writer_panicked" }`, marks input broken. |
| Panic in the waiter | Drop guard emits `WaitFailed`. |

Every blocking wait has a deadline: `CompletionReceiver::recv_deadline`, `OutputReceiver::recv_timeout`, `kill(deadline)`, `close(bounds)`, `wait_timeout`, `Drain::finish(bound)`, `close_bounded`, the writer's pause (`max_pause`). The one unbounded block is a write to a child that stops reading; close step 2 kills the tree, which ends it, and step 6 abandons the thread otherwise.

## 6. Purpose Layer and Plan Layer

### 6.1 Purpose Layer: Acceptance Criteria

Checked by running this crate's tests on `macos-latest` and `windows-latest` CI with `FakePty`.

| ID | Criterion (EARS) | Source | How to observe |
|---|---|---|---|
| PR2-P-1 | The crate shall provide a `PtyRuntime` trait that the Unix and Windows backends can implement without changing it, and a `FakePty` that implements it on both OSes. | mvp §4 PR 2; invariant 3 PTY Seam | `FakePty` passes every session test on both runners; §2.1 lists what PR 3 and PR 4 supply. |
| PR2-P-2 | When any write reaches the PTY, the writer shall record its origin and position. | mvp §4 PR 2 obligation | Records partition the fake child's input stream, with origins (PR2-CL-1). |
| PR2-P-3 | When the CLI sends a terminal query, the responder shall answer it in exactly one write. | mvp §4 PR 2 obligation; arch §4.5 | Fake child write calls each hold one whole reply (PR2-CL-8). |
| PR2-P-4 | When the screen holds a wide character, the snapshot shall show it on two cells. | mvp §4 PR 2 obligation; P-12 | Snapshot cells and cursor column (PR2-CL-10). |
| PR2-P-5 | While a text-and-submit unit is being written, no other write except a VT reply in its pause shall reach the PTY, and a unit refused before its first byte shall write zero bytes. | mvp §2.3 writer revalidation | Fake input bytes (PR2-CL-2, CL-3). |
| PR2-P-6 | When a session ends in any way, its threads, handles, and child shall be released within bounded time, and output shall be read until EOF. | mvp §4 PR 2 "drain"; spike §4.6–§4.7 | Fake event order and timings (PR2-CL-13, CL-14, CL-15). |

### 6.2 Plan Layer: Claims

| ID | Claim (EARS) | Source | Falsifying test |
|---|---|---|---|
| PR2-CL-1 | When a unit enters the writer, the writer shall append exactly one record with its origin, a gapless increasing `seq`, and its byte range; the byte ranges of written records shall partition the PTY input stream in `seq` order. | PR 2 obligation | `writer::records_partition_the_input_stream`, proptest `writer::records_partition_under_random_units` |
| PR2-CL-2 | While a unit is being written, no byte of another user unit shall be written; a VT reply shall be written only at a unit boundary or during a `Pause`. | mvp §2.3 | `writer::human_write_waits_for_the_unit`, `writer::reply_lands_only_in_a_pause` |
| PR2-CL-3 | If a unit is cancelled, expired, closed, after child exit, after input breakage, after a counting write past its basis, with 2004 off or its epoch changed, or its recheck fails or panics, then the writer shall record `NotStarted` with that reason and write zero bytes, checking in that order. | mvp §2.3 | `writer::each_refusal_writes_zero_bytes` (one case per reason), `writer::refusal_order` |
| PR2-CL-4 | When a recheck runs, every earlier unit shall have finished and no other write shall run until the unit's first byte. | mvp §2.3 arbitration lock | `writer::recheck_sees_every_earlier_write` |
| PR2-CL-5 | When a unit has started, the writer shall write its remaining parts regardless of `not_after`; if the queue closes during a `Pause`, the rest shall be skipped and the record shall be `Incomplete`. | herdr committed submit; mvp §2.3 | `writer::started_unit_ignores_its_deadline`, `writer::close_in_pause_skips_the_submit` |
| PR2-CL-6 | If a write fails, then the unit shall be `Incomplete` with the bytes written, `InputBroken` shall fire once, and later units shall be refused. | §2.3 | `writer::write_error_breaks_input` |
| PR2-CL-7 | When `submit` exceeds a bound, it shall fail at once with nothing written and shall never block. | §5 | `writer::full_queue_fails_without_blocking` (writes held) |
| PR2-CL-8 | When the VT emits a reply, the writer shall write it as one write call holding exactly that reply, never merged or split, in query order. | PR 2 obligation | `responder::each_reply_is_one_write`, proptest `responder::replies_are_single_writes` (random output with embedded queries, fake `short_writes` off) |
| PR2-CL-9 | When the CLI sends DA1, DA2, DSR 5, DSR 6, the kitty query, DECRQM, CSI 14 t, CSI 18 t, or OSC 4/10/11/12, the responder shall answer as in §2.5, ahead of user units queued before it; an OSC 52 read shall get no reply. | arch §4.5; spike B-5 | `responder::answers_each_query`, `responder::reply_precedes_queued_units`, `responder::clipboard_read_is_not_answered` |
| PR2-CL-10 | When the grid holds a wide character, the snapshot shall give it a `WideLead` and a `WideTail` cell, every line shall have `cols` cells, zero-width characters shall join their base cell, and a wrapped wide character shall leave a blank cell at the line end. | PR 2 obligation | `snapshot::wide_chars_occupy_two_cells`, `snapshot::wrapped_wide_char_leaves_a_blank`, `snapshot::combining_mark_joins_its_base` |
| PR2-CL-11 | The snapshot and `ModeView` shall reflect DECSET 2004, 1049, 25, DECCKM, DECKPAM, and kitty flags, and the 2004 epoch shall change on every observed transition. | mvp §2.3 gate | `snapshot::modes_follow_decset`, `snapshot::epoch_counts_transitions` |
| PR2-CL-12 | When the session resizes, the PTY, the grid, and the responder's size shall change together, and output after the call shall be laid out at the new size. | spike C-4 | `session::resize_moves_pty_grid_and_responder` |
| PR2-CL-13 | When a sink subscribes, it shall get a snapshot and then exactly the output bytes after the snapshot's offset. | PR 11 attach | `session::subscribe_is_gapless` |
| PR2-CL-14 | The drain shall read until EOF or a read error on every path, deliver final output to its sink, report a read error as an error, refuse an overflowed hold, and never block on a sink; a panicking or stopping sink shall not stop it. | spike F-2c, §4.6 | lifted `drain::final_output_reaches_the_sink`, `drain::read_error_is_not_eof`, `drain::hold_overflow_is_reported`, `drain::hung_close_is_bounded`; new `session::sink_panic_keeps_draining`, `session::vt_panic_marks_vt_failed` |
| PR2-CL-15 | When a session closes, it shall close the queue, kill the tree, drop the input, close the PTY, wait for the drain, and join threads, in that order and within `CloseBounds`, naming any abandoned thread. | spike §4.7 | `session::close_kills_before_dropping_input` (FakeEvent order), `session::close_is_bounded_with_a_blocked_write` (`hold_writes`, `KillBehavior::Ignore`) |
| PR2-CL-16 | If `Session::start` fails after the backend spawned, then it shall kill the tree before releasing the PTY. | spike `pty.rs:267-284` | `session::start_failure_kills_first` (`startup_output` over the hold) |
| PR2-CL-17 | When the child exits, the session shall emit `Exited` once and refuse every unit not yet started with zero bytes. | mvp §4 PR 10 "write-boundary recheck after exit" | `session::exit_is_reported_once`, `session::units_after_exit_are_refused` |
| PR2-CL-18 | If the writer thread panics, then every queued unit shall complete `NotStarted(Closed)`, the unit in progress `Incomplete`, and input shall be marked broken. | §5 | `writer::writer_panic_completes_every_unit` (`panic_on_next_write`) |
| PR2-CL-19 | `writes_after` shall return every record after `seq`, or `LogTruncated` when the ring no longer holds it. | §2.3 | `writer::writes_after_reports_truncation` |
| PR2-CL-20 | `WriteOrigin` shall map `Attach`, `Key`, `CardKey` to the `human_write` origins and count every origin except `VtReply` as a write. | mvp §2.1, §2.3 | `origin::maps_to_human_write_origins` |
| PR2-CL-21 | `Snapshot::to_payload` shall carry cols, rows, cursor, `VtModes`, and the grid in the Q1 encoding. | mvp §2.1 `vt_snapshot_taken` | `snapshot::payload_matches_the_snapshot` (decodes the grid back to cells) |
| PR2-CL-22 | Every blocking wait of the crate shall end by its deadline. | mvp §4 brief; §5 | `session::waits_end_by_their_deadlines` (receivers, `kill` with `Ignore`, `close`) |
| PR2-CL-23 | `chaldea-pty-runtime` shall build without tokio. | §2 threading model | CI dependency check (§7) |

Obligation mapping: "origin and position of every write" → CL-1, CL-19, CL-20; "replies in one write each" → CL-8, CL-9; "wide characters on two cells" → CL-10, CL-21.

## 7. Tests and Completion Commands

All tests above run in `cargo test --workspace --locked` on `macos-latest` and `windows-latest` with `FakePty`; none needs a real PTY, so PR 2 adds no OS-specific test. The VT, responder, and snapshot tests feed `Vt` directly or through `FakeChild::emit`; writer tests use `hold_writes` to queue work behind a blocked write, as the spike's `Gated` writer does (`pty.rs:431-476`). Timing assertions use generous bounds (seconds, not milliseconds) so they hold on loaded runners; pauses in tests are 10–50 ms.

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

**D3. VT replies are not writes for the gate or ack attribution.** A CLI that queries on every redraw would otherwise keep the gate closed and make every ack ambiguous. Replies are still recorded with their kind.

**D4. Replies may land in an input unit's pause.** Replies stay timely behind a submit delay or a queued paste (herdr's behaviour); user writes still never interleave.

**D5. Every `NotStarted` maps to `RecheckFailed` and every `Incomplete` to `WriteOutcomeUnknown`.** Zero bytes is certain only before the first byte; an I/O error even at zero bytes is treated as unknown, as mvp §2.3 does for any write outcome it cannot prove.

**D6. `FakePty` is behind a `fake` feature**, enabled for this crate's tests through a self dev-dependency and for the daemon's tests through its dev-dependency, so no production build contains it.

**D7. Backends need `unsafe`; the workspace forbids it.** The root `[workspace.lints.rust] unsafe_code = "forbid"` cannot be relaxed by `#[allow]`. PR 2 writes no `unsafe` and keeps `[lints] workspace = true`. PR 3 (libc `killpg`, `getsid`, `proc_*`) and PR 4 (`windows-sys`) need a carve-out: change the workspace value to `deny` and allow it in `unix.rs` / `windows.rs` only, or give this crate its own `[lints]` table with `deny`. That choice belongs to PR 3.

**D8. Writer limits and close bounds are internal defaults** (§5); none is part of a public contract.

### Open Questions

**Q1. Encoding of `vt_snapshot_taken.grid`** (public: the PWA and `Component::VtSnapshot` read it; changing its meaning later bumps `schema_version`, mvp §2.1). PR 1 typed it as `Content` without fixing what the text holds.

| Option | Gains | Losses | Reversibility |
|---|---|---|---|
| A. Text: `Line::text()` of each row joined with LF | Smallest; readable as a blob preview | Column positions after a wide character depend on the PWA's font rendering CJK at exactly two columns, so box-drawn prompts can misalign; the "two cells" guarantee does not reach the reader | A later move to B is a meaning change (bump) |
| B. JSON rows of cell strings, `""` for a `WideTail`, trailing blanks omitted | Exact geometry: the PWA places each cell in a fixed grid, so wide characters provably take two columns; plain text is a join away | About 2–4× the bytes of A for CJK-heavy screens (a 120×40 CJK screen is about 22 KiB, still inline) | Adding attributes later is additive (objects in place of strings would not be, so strings are kept) |
| C. Text plus a separate list of wide-cell columns | Readable and exact | Two structures that must agree; more decoding code | Same as B |

Recommendation: **B**. It is the form in which the PR 2 obligation is observable by the PWA, and P-12 asks for a legible CJK snapshot. PR 2 implements `to_payload` with the chosen form, so Q1 must be settled before PR 2 merges.

## Proposed Changes to Other Documents

Not applied here.

- **`docs/mvp-design.md` §2.3** ("last write of any origin", ack "no write of another origin"): say that VT query replies are not writes for either rule (D3).
- **`docs/mvp-design.md` §4 PR 10 obligations**: add "the write basis is captured before the gate is evaluated, and a human write after the basis refuses the input with zero bytes".
- **`docs/mvp-design.md` §2.1** `vt_snapshot_taken`: state the grid encoding once Q1 is settled.
- **`docs/mvp-design.md` §3 spike table**: `src/os/drain.rs` is "lift, consumer as a sink callback" rather than a plain lift.
- **`docs/mvp-design.md` §5 Files / `Cargo.toml`**: the `unsafe_code = "forbid"` lint needs the carve-out of D7 before PR 3.
- **`docs/architecture.md` §4.5**: the `PtyRuntime` trait seals spawn and the OS differences; `write_text`, `write_key`, `snapshot`, `attach`, `kill` are operations of the session built on it.
