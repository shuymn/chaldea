# Design: Unix PTY Backend and `crates/testkit` (MVP PR 3)

Status: design, not implemented. Scope is the PR 3 row of `docs/mvp-design.md` §4 (cited as "mvp §N"): the Unix backend of `PtyRuntime` (portable-pty `=0.8.1`, `killpg` HUP→TERM→KILL, `getsid` scan, recorded-pid fallback) and the new `crates/testkit` crate with `chaldea-fake-cli`. Its test obligation: FakeCli `spawn-tree` survivors and the unrelated negative control. The contract implemented is `docs/design/pty-runtime.md` (cited as "PR2 §N"), §2.1 "What each backend must supply"; no trait or public type of PR 2 changes.

Out of scope: the Windows backend (PR 4; the `cfg` layout below leaves room for `src/windows.rs`), daemon wiring and its restart scan (PR 10), input encoders (PR 5), the gate and send path (PR 10), attach (PR 11), and every FakeCli feature only those PRs use.

Sources read for this document (paths are relative to each root):

| Source | Revision | Read for |
|---|---|---|
| This repository | `b35d687` | `crates/pty-runtime/src/{backend,session,drain,fake,lib}.rs`, `Cargo.toml`, `.github/workflows/ci.yml`, `lefthook.yml`; spike `spikes/pty-hooks/src/os/{unix,mod}.rs`, `src/session.rs`, `src/fake_cli.rs`; `docs/spikes/pty-hooks-spike.md` §4.5–§4.7; `docs/prior-art.md` row 2 |
| `portable-pty` | crate 0.8.1 | `src/unix.rs`, `src/cmdbuilder.rs`, `src/lib.rs` |
| `rustix` | crate 1.1.6 (already in `Cargo.lock` through `alacritty_terminal`) | `src/process/{wait,kill,id}.rs`, `src/event/poll.rs`, `src/termios/tc.rs` |
| `libc` | crate 0.2.191 (already in `Cargo.lock`) | `src/unix/bsd/apple/mod.rs` (`proc_bsdinfo`, `KERN_PROC_PID`, `SZOMB`; no `kinfo_proc`) |
| `signal-hook` | crate 0.4.6 (already in `Cargo.lock`) | `src/flag.rs`, `Cargo.toml` features |
| Rust std | 1.96.0 `rust-src` | `library/std/src/sys/process/unix/unix.rs` (spawn error path), `sys/pipe/unix.rs` |
| `assert_cmd` | crate 2.2.2 | `src/cargo.rs` (how a test finds a binary) |

**Host facts measured for this design** on macOS 26.7.3 (25G342, arm64) with throwaway C probes (not committed; the named tests in §7 check each on CI):

| Fact | Observed | Used by |
|---|---|---|
| `waitid(P_PID, pid, WEXITED\|WNOHANG\|WNOWAIT)` reports an exited child repeatedly without reaping it | `CLD_EXITED`, status 7, twice | §2.4 |
| A process group whose leader is an unreaped zombie still exists | `killpg(g, 0)` = 0 with a live member; `EPERM` with only the zombie left; `ESRCH` only after `waitpid` | §2.5 group signals |
| `proc_pidinfo(PROC_PIDTBSDINFO)` fails for zombies (`ESRCH`) and for other users' processes (`EPERM`, e.g. pid 1) | both | §2.5 table |
| `sysctl(KERN_PROC_PID)` returns the record of a zombie, start time at offset 0 equal to `pbi_start_tvsec/usec` of a live process | equal values | §2.4 leader start |
| `getsid(pid)` reads processes of other sessions; `ESRCH` for a zombie | both | §2.5 scan |
| `poll` on a pty master reports `POLLIN` for data and `POLLIN\|POLLHUP` once every slave end is closed; `read` then returns 0 | both | §2.3 reader |
| Consecutive forks get consecutive pids | `n`, `n+1` | §2.5 residual window |

## 1. Goal and Files

**Goal.** A macOS backend that opens a PTY with portable-pty, starts the CLI as a session leader in it, hands the session a drain over the master, an input handle that writes nothing when dropped, and a child whose `kill_tree` kills the whole tree (group, session, descendants, recorded extras) without ever signalling a reused pid; plus a `testkit` crate whose `chaldea-fake-cli` stands in for a CLI in real-PTY tests. Every claim of §6 is checked by a named test on `macos-latest`; `windows-latest` keeps building everything and running the `FakePty` and FakeCli smoke tests.

**Files.**

```
Cargo.toml                                # members += "crates/testkit"; [workspace.dependencies] += portable-pty, rustix, libc, signal-hook
crates/pty-runtime/Cargo.toml             # macOS deps; own [lints] table (D6)
crates/pty-runtime/src/lib.rs             # #[cfg(target_os = "macos")] mod unix; pub use unix::{native, identify}
crates/pty-runtime/src/unix/mod.rs        # UnixPty: PtyRuntime; spec validation; the spawn sequence (§2.2); native(), identify()
crates/pty-runtime/src/unix/io.rs         # MasterReader (drain input), MasterWriter (session input), UnixControl (§2.3, §2.6)
crates/pty-runtime/src/unix/child.rs      # UnixChild: PtyChild; wait without reaping; reap on drop (§2.4)
crates/pty-runtime/src/unix/table.rs      # process table: Proc, scan(deadline), lookup(pid) (§2.5)
crates/pty-runtime/src/unix/kill.rs       # the kill engine over a `Os` trait, so unit tests drive it with synthetic tables (§2.5)
crates/pty-runtime/src/unix/sys.rs        # the only module allowed `unsafe` (§2.7): borrow_raw, proc_listallpids, proc_pidinfo, sysctl
crates/pty-runtime/src/unix/tests.rs      # #[cfg(test)] real-process unit tests (system binaries only), serialized (§7)
crates/pty-runtime/tests/lints.rs         # lints table drift and the single unsafe allowance (all OSes)
crates/testkit/Cargo.toml                 # chaldea-testkit, publish = false, [lints] workspace = true
crates/testkit/src/lib.rs                 # pub mod fake_cli; pub mod report
crates/testkit/src/fake_cli/mod.rs        # pub fn main(args) -> i32: option parsing and mode dispatch
crates/testkit/src/fake_cli/{term,echo,queries,attachable,spawn_tree,sleeper}.rs
crates/testkit/src/report.rs              # report wire format; Reporter (fixture side); ReportListener, Fixture (test side)
crates/testkit/src/bin/chaldea-fake-cli.rs   # fn main() { std::process::exit(chaldea_testkit::fake_cli::main(std::env::args_os())) }
crates/testkit/tests/fake_cli.rs          # every PR 3 mode without a PTY (all OSes)
crates/testkit/tests/pty_unix.rs          # #![cfg(target_os = "macos")] native backend + FakeCli (§7)
.github/workflows/ci.yml, lefthook.yml    # `workspace` job: testkit dependency check (§7)
```

PR 4 adds `src/windows/` behind `#[cfg(windows)]` and a Windows arm of `native()` and `identify()`; no Unix file moves.

**Dependencies.** Exact pins, all already in `Cargo.lock` except portable-pty's own tree:

```toml
# root [workspace.dependencies]
portable-pty = "=0.8.1"
rustix = { version = "=1.1.6", default-features = false, features = ["std", "process", "event", "termios", "fs"] }
libc = "=0.2.191"
signal-hook = { version = "=0.4.6", default-features = false }

# crates/pty-runtime/Cargo.toml
[target.'cfg(target_os = "macos")'.dependencies]
portable-pty.workspace = true
rustix.workspace = true        # waitid, kill, killpg, getsid, poll, tcsetwinsize, access, dup: safe wrappers
libc.workspace = true          # proc_listallpids, proc_pidinfo, sysctl: no safe wrapper exists (§2.7)

# crates/testkit/Cargo.toml
[dependencies]
serde_json.workspace = true    # report lines
[target.'cfg(unix)'.dependencies]
rustix.workspace = true        # raw mode, tcgetwinsize, getsid, getpgrp, setsid
signal-hook.workspace = true   # `flag::register` ignores SIGHUP/SIGTERM without `unsafe`
[dev-dependencies]
chaldea-pty-runtime = { path = "../pty-runtime" }   # pty_unix.rs drives the native backend
```

portable-pty brings `anyhow`, `nix 0.25.1`, `filedescriptor`, `serial`, `shell-words`, `downcast-rs`, `log` into the lock (the spike locks the same set); the implementer commits the regenerated `Cargo.lock`. The macOS-only target table keeps portable-pty out of Windows builds. `signal-hook` without default features keeps only `flag` and `low_level` (its `channel`/`iterator` features are unused).

**Workspace and CI.** `crates/testkit` joins `members`. The `workspace` job runs unchanged on both runners (fmt, clippy `-D warnings`, `cargo test --workspace --locked`); on `macos-latest` that now includes the real-PTY tests, which `cfg` removes on Windows. One step is added: testkit depends on no workspace crate other than `core` (§7). `lefthook.yml` mirrors it.

## 2. Types and Functions

Names may change in review; shapes and rules may not.

### 2.1 Public Additions

```rust
/// The native backend of this OS (macOS: this document; PR 4 adds the Windows arm).
#[cfg(target_os = "macos")]
pub fn native() -> Arc<dyn PtyRuntime>;

/// The identity of a live process in the form `kill_tree` matches `extra` against:
/// `Ok(None)` when no live (non-zombie) process has `pid`; `Err(PermissionDenied)` when it
/// belongs to another user. The daemon records hook emitters' ancestors through it (PR 9/10).
#[cfg(target_os = "macos")]
pub fn identify(pid: u32) -> io::Result<Option<ProcessIdentity>>;
```

Both are additive. `identify` exists because `ProcessIdentity::start_time` is a backend format: an `extra` entry built by anyone else could never match.

**Identity format** (`ProcessIdentity`, PR2 §2.1): `pid` the process id; `start_time` the process start as decimal microseconds since the Unix epoch (`pbi_start_tvsec * 1_000_000 + pbi_start_tvusec`, the spike's value, `os/unix.rs:90`); `group` the process group id in decimal, which for the leader equals `pid`. The start time is the kernel's wall-clock stamp at fork: equal values identify a process; ordering across processes holds only on one boot without a clock step (PR 10's restart window, mvp §4 PR 10, inherits this).

### 2.2 Spawn

```rust
struct UnixPty;                                   // sealed::Sealed + PtyRuntime
impl PtyRuntime for UnixPty { fn spawn(&self, spec: &SpawnSpec, output: DrainSink) -> Result<SpawnedPty, SpawnError>; }
```

**Validation** (before anything is created; every failure is `NeverStarted` with nothing to release, `Cleanup::Confirmed`):

| Check | Failure |
|---|---|
| `argv` non-empty, `argv[0]` absolute | `Setup`, `InvalidInput` |
| `argv[0]` exists (`metadata`) | `NotFound` |
| `argv[0]` is a regular file and `rustix::fs::access(X_OK)` passes | `PermissionDenied` |
| `cwd` absolute and a directory | `Setup` (the error kind is `NotFound` when missing) |
| every env key non-empty, without `=` or NUL; no value with NUL; no key twice | `Setup`, `InvalidInput` |
| `SHELL` present in `env` (open question OQ-1) | `Setup`, `InvalidInput` |

The `cwd` check is not optional: portable-pty silently runs the child in `$HOME` when the directory does not exist (`cmdbuilder.rs:452-458`). The remaining window, a directory removed between this check and portable-pty's, is accepted.

**Sequence.** Each stage lists what exists when it fails and how the failure is classified.

1. **Open** `native_pty_system().openpty(PtySize { rows, cols, pixel_width: cols * 8, pixel_height: rows * 16 })`: the PTY is opened at `spec.size` with the 8×16 px cells the responder reports (PR2 §2.5). Failure: `NeverStarted { Setup }`.
2. **Ends.** Three `F_DUPFD_CLOEXEC` duplicates of the master (`rustix::io::fcntl_dupfd_cloexec` on the borrowed fd, §2.7): reader, writer, control; one wake pipe (`std::io::pipe()`). The portable-pty master object is then dropped; the open file description lives on in the duplicates. Failure: everything created is dropped, `NeverStarted { Setup }`.
3. **Drain.** `Drain::start(MasterReader { reader dup, wake read end }, output)` (PR2 §2.4), so every output byte reaches the VT from the first one. Failure: `Drain::start` drops the reader first; the rest is dropped; `NeverStarted { Setup }`.
4. **Child.** `CommandBuilder::from_argv(spec.argv)`, `env_clear()` (removes the daemon's environment that `from_argv` copied, `cmdbuilder.rs:72-84`, `:221`), `env(k, v)` for each pair, `cwd(spec.cwd)`; then `slave.spawn_command(builder)`. portable-pty resets six signal dispositions, calls `setsid()` and `TIOCSCTTY`, and closes every fd above 2 in the child (`unix.rs:200-247`), so the child is a session and group leader (pgid = sid = pid) with the PTY as controlling terminal and only fds 0–2. `argv[0]` reaches the child as given (`cmdbuilder.rs:470-472`).
   - **`Err`**: either portable-pty refused before forking (path resolution, `cmdbuilder.rs:410-446`), or std forked and the child reported its `pre_exec` or `exec` errno, after which std reaped it before returning (`library/std/src/sys/process/unix/unix.rs:138-150`). Either way no process from this spawn exists and the program never ran: `NeverStarted`, with `kind` from the `io::Error` found by `anyhow::Error::downcast_ref::<io::Error>()`: `NotFound` (`ENOENT`) when `argv[0]` is now missing, `PermissionDenied` (`EACCES`, `EPERM`), else `Setup`. Release: drop the slave, the writer, close the control (wake), `Drain::finish(2 s)`; `Cleanup::Confirmed` when the drain ended, else `Unresolved { kill: None, close: Some(..), drain_abandoned: true }`.
   - **`Ok(child)`**: a process exists. From here every failure is `StartedThenFailed`.
5. **Settle.** Drop the parent's slave end at once (PR2 §2.1; spike §4.7), so EOF reaches the drain when the tree's last slave holder closes it. Take the pid and drop portable-pty's `Box<dyn Child>` (a `std::process::Child`, whose drop neither waits nor kills). Read the leader's start time (§2.4) and check `pgid == pid` from the same record. Failure: `StartedThenFailed { stage: "identity" | "group", identity: Some(..) when the start time was read, None otherwise }`; release in the PR 2 order: `kill_tree(&[], now + 5 s)` on a `UnixChild` built from the pid (signalling is safe without a start time: the pid is this process's unreaped child, §2.5), drop the writer, close the control, `Drain::finish(2 s)`; `Cleanup` as in `SpawnedParts::teardown` (`backend.rs:266-291`).
6. **Hand over** `SpawnedPty::new(SpawnedParts { child: Arc<UnixChild>, output, input: Box<MasterWriter>, control: Box<UnixControl>, size: spec.size, evidence: vec![] })`.

The Unix backend reports no `evidence`; a key added later is additive.

**Environment.** The child's environment is `spec.env`, except that portable-pty always sets `SHELL` (`cmdbuilder.rs:479-480`): to `spec.env`'s value when present (the later `envs` call overrides it), else to the login shell from the password database (`cmdbuilder.rs:495-508`, never the daemon's environment). The validation row above makes the environment exactly `spec.env`; OQ-1 asks whether to keep that refusal.

### 2.3 Input and Output Ends

```rust
struct MasterWriter(File);                        // Write; Drop closes the fd and writes nothing
struct MasterReader { master: OwnedFd, wake: PipeReader, woken_bytes: Option<u64> }   // Read
```

**Input.** Not portable-pty's writer, whose `Drop` writes `\n` + VEOF (`unix.rs:351-363`): `MasterWriter` is a `File` over its own duplicate of the master. The master is in blocking mode (portable-pty never sets `O_NONBLOCK`), so a `write` blocks until the tty accepts every byte and returns the whole length, or fails; a signal can cut a blocked write short, which the writer loops on and records as a split (PR2 §2.3). Dropping it closes one duplicate and nothing else.

**Output.** `MasterReader::read`:

1. `poll([master: IN, wake: IN], no timeout)` (`rustix::event::poll`); `EINTR` repeats it.
2. Master readable (`POLLIN` or `POLLHUP`): `read`. `Ok(n)` returns `n`; `Ok(0)` and `EIO` return `Ok(0)`, the EOF of PR2 §2.4 (macOS returns 0 at hangup, Linux `EIO`; portable-pty maps the same way, `unix.rs:86-98`); `EINTR`/`EAGAIN` repeat; any other error is returned.
3. Wake readable and master not readable: `Err(io::Error::other("pty closed before eof"))`, which the drain reports as `OutputEnd::Error`, never as EOF.
4. Once the wake fired, the reader still returns master data while `poll` with a zero timeout reports it readable, up to 1 MiB after the wake, then step 3. So a survivor that keeps writing to the slave cannot hold the drain past the close.

The wake turns the drain's end into a bounded event: without it, a process outside the kill's reach that keeps the slave open (an orphaned `setsid` escapee, a child elsewhere in the daemon that inherited the slave during portable-pty's non-atomic CLOEXEC window, `unix.rs:29-58`) would keep the drain thread and its fd alive until that process exits.

### 2.4 Child: Identity, Exit, Reaping

```rust
struct UnixChild {
    pid: Pid,
    identity: ProcessIdentity,
    leader_start: Option<u64>,
    exit: OnceLock<ChildExit>,
    held: AtomicBool,            // true while the leader is this process's unreaped child
    kill: Mutex<()>,             // serializes kill_tree calls; wait_timeout never takes it
}
```

**Leader start time.** `sysctl([CTL_KERN, KERN_PROC, KERN_PROC_PID, pid])` into a byte buffer; the record's first 16 bytes are `kp_proc.p_un.__p_starttime` (`struct timeval`, the first member of `extern_proc`, itself the first member of `kinfo_proc`). Unlike `proc_pidinfo`, this works for a leader that already exited (a CLI that dies at startup still gets an identity, its exit is reported normally). A record shorter than 16 bytes is an error. Elsewhere (scan, `identify`) the start time comes from `proc_pidinfo`; the test `leader_start_matches_identify` pins that both agree.

**`wait_timeout(timeout)`.** If `exit` is set, return it. Else loop: `waitid(WaitId::Pid(pid), EXITED | NOHANG | NOWAIT)`; an exit sets `exit` once and is returned; `None` sleeps `min(10 ms, remaining)` until `timeout`, then returns `Ok(None)`; `EINTR` repeats; `ECHILD` clears `held` and returns `Err(other("child reaped outside the backend"))` (the session publishes `WaitFailed`); other errors are returned. `ChildExit { code: Some(status), signal: None }` for `CLD_EXITED`, `{ None, Some(sig) }` for `CLD_KILLED`/`CLD_DUMPED`. It never blocks past `timeout` and takes no lock (`exit` is a `OnceLock`, `held` an atomic), so it runs freely during `kill_tree` (PR2 §2.1).

**No reaping while the child lives.** `WNOWAIT` leaves the leader a zombie after it exits. While it is unreaped, its pid, and with it the process group and session id it leads, cannot be reused (POSIX process-ID and process-group-ID reuse rules; measured: the group exists until `waitpid`). This is what makes every group signal of §2.5 safe.

**`Drop`** (the last `Arc` goes when the session closes or drops): `rustix::process::waitpid(Some(pid), NOHANG)` reaps the zombie. If the leader is still alive (its kill failed, already reported as `tree_dead: false`), nothing blocks: it stays unreaped and becomes a zombie of the daemon when it dies. Bound: one per such session.

**Daemon constraint** (handed to PR 8 / PR 10, §4): the daemon process must not reap children it does not own: no `waitpid(-1, ..)`, `wait()`, `SIGCHLD` set to `SIG_IGN`, or `SA_NOCLDWAIT`. A violation is detected (`ECHILD`) and degrades the kill (§2.5) instead of risking a reused pid.

### 2.5 Tree Kill

```rust
impl PtyChild for UnixChild { fn kill_tree(&self, extra: &[ProcessIdentity], deadline: Instant) -> KillReport; }

pub(crate) struct Proc { pid: u32, ppid: u32, pgid: u32, sid: Option<u32>, start: u64, zombie: bool }
pub(crate) enum Lookup { Live(Proc), Dead, Unreadable }          // ESRCH (absent or zombie) is Dead; EPERM Unreadable
pub(crate) struct Scan { procs: Vec<Proc>, unreadable: Vec<(u32, Option<u32>)>, complete: bool }   // (pid, sid)
pub(crate) trait Os { fn scan(&self, deadline: Instant) -> Scan; fn lookup(&self, pid: u32) -> Lookup;
                      fn leader_held(&self) -> bool; fn killpg(&self, pgid: u32, sig: Signal) -> io::Result<()>;
                      fn kill(&self, pid: u32, sig: Signal) -> io::Result<()>; fn now(&self) -> Instant; fn sleep(&self, d: Duration); }
pub(crate) fn kill_tree(os: &dyn Os, leader: Leader, extra: &[ProcessIdentity], deadline: Instant) -> KillReport;
```

**Scan.** `proc_listallpids`, then for each pid `proc_pidinfo(PROC_PIDTBSDINFO)` (pid, ppid, `pbi_pgid`, start) and `getsid(pid)`. `ESRCH` from either drops the pid (gone or a zombie: dead). `EPERM` from `proc_pidinfo` puts the pid in `unreadable` with its `getsid` result. The deadline is checked every 64 pids; a scan cut short has `complete: false` and can never prove death.

**Kill set.** With `L` the leader pid and `S` a scan:

- *Group and session members*: processes in `S` with `pgid == L` or `sid == L`, and unreadable pids with `sid == L`.
- *Verified roots*: the leader while live in `S` with its recorded start; each `extra` entry whose pid is live in `S` with a start equal to the entry's `start_time` and is not 0, 1, or the daemon's own pid. An entry that fails this is never signalled.
- *Descendants*: the `ppid` closure in `S` from the roots and members, taking a child only if its start is not before its parent's (spike `owned_by`, `session.rs:384-428`). This catches a `setsid` escapee while its parent lives.
- `K`, the kill set, accumulates every identity found across all scans of the call, so a member reparented after the first scan is still killed.

**Steps**, with `t0` the call time and `B = deadline − t0`:

1. Take `kill` with `try_lock` polled every 10 ms until `deadline`; if it is never free, return `{ method: "unix_busy", tree_dead: false, survivors: [leader identity] }`. A poisoned lock is taken over.
2. Scan, build `K`.
3. If `B > 100 ms`: **HUP** to the group and to every `K` member outside it; wait until `min(t0 + 500 ms, t0 + B/4)`; rescan, extend `K`; **TERM** the same way; wait until `min(t0 + 2 s, t0 + B/2)`. Each wait polls every 25 ms (`lookup` of each `K` member, `killpg(L, 0)`) and ends early when nothing in `K` is live and the group answers `ESRCH` or `EPERM`.
4. Rescan, extend `K`, **KILL** the group and every live `K` member outside it.
5. **Prove**, until `deadline`: scan; the live set is every process of the scan in `K` or a member, plus every unreadable member. If the scan is complete and the live set empty, return `tree_dead: true`. Otherwise KILL the live set again, sleep 25 ms, repeat. A process forked while the tree died is in the group or session and is caught by the next round.
6. At `deadline`: return `tree_dead: false` with the live set as `survivors` (`start_time` empty for an unreadable one; `group` its pgid, empty when unknown).

`method` is `"unix_killpg_session_scan"`, or `"unix_session_scan_unheld"` in degraded mode. `kill_tree` is idempotent: on a dead tree, step 5's first scan proves death and returns at once. It contains no panic path; every syscall error is a value.

**How a reused pid is never signalled.**

- **Group signals** (`killpg(L, ..)`) are sent only while `held` is true and the leader has not been reaped. `L` is then an unreaped child of the daemon, so no other process can have pid `L` and no other group or session can have id `L`; every process in group `L` is a CLI-tree process (a process can join a group only within its own session). No check-then-act window exists.
- **Per-pid signals** (members outside the group, descendants, `extra`) are sent only through `signal_verified(id, sig)`: `lookup(id.pid)` immediately before the `kill(2)`; it signals only `Live` with the recorded start, never pid 0, 1, or the daemon's own. This is the spike's recheck (`os/unix.rs:126-133`, spike §4.6). The residual window is the gap between those two syscalls: the process would have to exit, be reaped by its parent, and its pid be handed out again inside it. macOS allocates pids sequentially (measured), so that needs the pid space to wrap within two syscalls. macOS has no pidfd to close the gap.
- **Degraded mode** (`held` false after `ECHILD`): no group signals. If a live process now has pid `L` with another start time, the ids are reused: members by pgid or sid are not targeted at all, only verified descendants of verified `extra` roots, and the result is `tree_dead: false` unless every `K` identity is proven dead. Otherwise members are signalled per pid, verified.
- **Unit tests drive the engine with a synthetic `Os`** (§7), so reuse, `EPERM`, a stale `extra`, and an unheld leader are exercised deterministically.

### 2.6 Control

```rust
struct UnixControl { master: OwnedFd, wake: PipeWriter }   // PtyControl
```

- `resize(size)`: `rustix::termios::tcsetwinsize(&master, Winsize { ws_row, ws_col, ws_xpixel: cols * 8, ws_ypixel: rows * 16 })`. The kernel sends `SIGWINCH` to the slave's foreground group. The `io::Error` keeps its errno. Never blocks.
- `close(deadline)`: writes one byte to the wake pipe (an empty pipe accepts it at once), drops both fds, returns `CloseReport { elapsed: Some(..) }`. It never blocks, so `deadline` is always met; `close_bounded` (PR2 §2.4) is Windows-only.

### 2.7 The `unsafe` Carve-Out (PR2 D7)

`crates/pty-runtime/Cargo.toml` gets its own `[lints]` table: `[lints.rust] unsafe_code = "deny"`, `unsafe_op_in_unsafe_fn = "deny"`; `[lints.clippy]` the workspace's clippy set restated verbatim, plus `undocumented_unsafe_blocks = "deny"` and `multiple_unsafe_ops_per_block = "deny"`. `deny`, because the workspace's `forbid` cannot be lifted by an `#[allow]`; restated, because Cargo's `[lints] workspace = true` cannot be combined with crate-local keys. Every other crate keeps `[lints] workspace = true` with `forbid`.

Exactly one module is allowed `unsafe`: `#[allow(unsafe_code)] mod sys;` in `src/unix/mod.rs`. Everything else, including the kill engine, is safe code over rustix. `sys` exposes safe functions only:

| `unsafe` operation | Safe function | Safety argument |
|---|---|---|
| `BorrowedFd::borrow_raw(master.as_raw_fd())` | `dup_master(&dyn MasterPty) -> io::Result<OwnedFd>` | `as_raw_fd()` returns the fd the master owns (`unix.rs:328-330`); the borrow does not outlive `&master`, and it is used only to call `fcntl_dupfd_cloexec`, which returns a new `OwnedFd`. |
| `libc::proc_listallpids(buf, bytes)` | `pid_list() -> io::Result<Vec<u32>>` | Called with a null buffer for the count, then with a `Vec<pid_t>` of count + 64 entries whose byte length is passed; the return value (entries written) is checked against the capacity before `truncate`. |
| `libc::proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &mut info, size)` | `bsd_info(pid) -> Result<BsdInfo, Errno>` | `info` is a `libc::proc_bsdinfo` built from `MaybeUninit::zeroed()` in a separate block (all fields integers or `c_char` arrays, so zero is valid); the size passed is `size_of::<proc_bsdinfo>()`; the result is used only when the return equals that size. |
| `libc::sysctl(mib, 4, buf, &mut len, null, 0)` | `start_time_of(pid) -> io::Result<u64>` | `buf` is a `[u8; 1024]` and `len` its length; the kernel writes at most `len` bytes and sets `len`; only `len >= 16` is read, by `from_ne_bytes` on copies, never by casting the buffer. |

`getsid`, `kill`, `killpg`, `waitid`, `waitpid`, `poll`, `tcsetwinsize`, `access`, and `fcntl_dupfd_cloexec` are rustix's safe functions. The `pre_exec` closure is portable-pty's code, not this crate's. `tests/lints.rs` checks that `#[allow(unsafe_code)]` occurs exactly once in `src/` and that the clippy table equals the workspace's.

### 2.8 `testkit` and `chaldea-fake-cli`

**Crate.** `chaldea-testkit`, `publish = false` (workspace value), `[lints] workspace = true` (so `forbid(unsafe_code)`). Normal dependencies: no workspace crate in PR 3; `core` is the only one it may add later (mvp §3). FakeCli is library code, `chaldea_testkit::fake_cli::main(args) -> i32`, and the binary `chaldea-fake-cli` is a two-line wrapper, so a crate that must ship or embed it later (PR 21's doctor) can call the same entry point.

**Command line** (PR 3 subset of spike §4.5; unknown flags exit 2):

```
chaldea-fake-cli --mode <echo|queries|attachable|spawn-tree|sleeper> [--report <127.0.0.1:port>]
                 [--role <role>] [--max-life-secs <n>] [--setsid] [--escape-orphan] [--orphan-keeps-tty]
```

**Report channel** (replaces the spike's HTTP `/witness`, which needs the harness's receiver). With `--report`, each FakeCli process connects one TCP stream to the address, writes a hello line, then mode events, one JSON object per line, and never closes the stream: it closes when the process exits. The test reads the stream's EOF as the process's death, an oracle independent of the backend's process table. The hello is `{"role","pid","ppid","pgid","sid","fds"}` (`pgid`/`sid` from `rustix::process::{getpgrp, getsid}`, null on Windows; `fds` the entries of `/dev/fd` read before connecting, PTY modes only). std sockets are close-on-exec, so a fixture's children never hold its stream.

**Modes lifted now** (each is what a PR 3 test uses):

| Mode | Behaviour in PR 3 | Lifted later |
|---|---|---|
| `echo` | raw mode (`rustix::termios` `make_raw` on stdin), `ESC[?2004h`, then every stdin chunk as `{"event":"input","hex":…}` | paste parsing, hooks via `--emitter`, `--nested`, `--query-mid-paste`, `--ignore-input` (PR 9 / PR 10) |
| `queries` | raw mode, the spike's seven queries with a 500 ms reply bound each (`fake_cli.rs:383-481`), result as `{"event":"queries","all_ok",…}` | — |
| `attachable` | raw mode, `MARKER-n` every 500 ms, `{"event":"size","cols","rows","xpixel","ypixel"}` at start and whenever `tcgetwinsize(stdin)` changes (polled every 20 ms) | the input parser and key reports (PR 11), Windows console size (PR 4) |
| `spawn-tree` | starts sleepers per role and reports `{"event":"tree","children":[…]}`; then idles | Windows `breakaway` role (PR 4), `--emit-output` (PR 4's F-2c cases) |
| `sleeper` | roles below; reports its hello and idles until killed or `--max-life-secs` | `mcp` role (PR 13 / PR 14) |
| `stall`, `banner` | not lifted | the PR that first asserts on them (PR 10: gate timeout snapshot, fake banner) |

**Sleeper roles** (Unix; each isolates one kill mechanism; Windows gets `plain` and `double-forked`):

| Role | How it is made | What kills it |
|---|---|---|
| `plain` | child of spawn-tree | the group |
| `ignore-term` | ignores SIGHUP and SIGTERM through `signal_hook::flag::register` | the group's KILL step |
| `double-forked` | its parent (`double-fork-parent`) exits at once, so it is reparented in the group | the group (no live parent for a ppid walk) |
| `own-group` | spawned with `CommandExt::process_group(0)`: same session, other group | the `getsid` scan |
| `setsid` | `--setsid`: calls `rustix::process::setsid()` before its hello; its parent stays alive | the ppid walk |
| `setsid-orphan` | `--escape-orphan`: a `setsid` sleeper whose parent exits at once; `--orphan-keeps-tty` keeps stdout on the PTY | only `extra` (the macOS containment limit, spike §4.6) |
| `unrelated` | started by the test itself, outside the CLI's session | must survive |

**Test-side API** (`report.rs`):

```rust
pub struct ReportListener { /* 127.0.0.1:0, nonblocking accept */ }
impl ReportListener {
    pub fn bind() -> io::Result<ReportListener>;
    pub fn arg(&self) -> String;                                                    // value for --report
    pub fn accept(&self, deadline: Instant) -> io::Result<Fixture>;                 // next hello
    pub fn expect_roles(&self, roles: &[&str], deadline: Instant) -> Result<BTreeMap<String, Fixture>, String>;
}
pub struct Fixture { pub hello: Hello, /* stream, line buffer */ }
impl Fixture {
    pub fn next_event(&mut self, deadline: Instant) -> io::Result<Option<serde_json::Value>>;  // None: stream closed
    pub fn closed_by(&mut self, deadline: Instant) -> bool;                                    // the process exited by then
}
```

**Locating the binary.** `CARGO_BIN_EXE_<name>` exists only for integration tests of the package that defines the binary, so the real-PTY tests that need FakeCli are integration tests of testkit (`crates/testkit/tests/pty_unix.rs`), with `chaldea-pty-runtime` as a dev-dependency, and use `env!("CARGO_BIN_EXE_chaldea-fake-cli")`; Cargo builds the binary before running them. `chaldea-pty-runtime`'s own tests do not run FakeCli: its unit tests use system binaries (`/bin/sh`, `/bin/sleep`, `/usr/bin/env`, `/bin/pwd`, `/bin/stty`, `/usr/bin/true`), and everything that needs crate internals (the kill engine, the reader, spawn-stage faults) is a unit test there. A later crate whose own tests need FakeCli declares a thin `[[bin]]` over `fake_cli::main` or puts the test in testkit; it does not guess `target/<profile>/` (assert_cmd keeps that only as a legacy fallback that depends on Cargo's `build-dir`, `src/cargo.rs:220-296`).

## 3. Structure Options

| Fork | Option | Precedent | Verdict |
|---|---|---|---|
| **`unsafe` placement** | A. `unsafe` inline where used | spike `os/unix.rs` (18 `unsafe` blocks across enumeration, kill, termios); portable-pty `unix.rs` (inline throughout) | Rejected: every module becomes an audit site, and `deny` would need many allowances. |
| | B. Workspace `deny`, each other crate adds `#![forbid(unsafe_code)]` | — | Rejected: contradicts PR2 D7 and mvp §3 (workspace stays `forbid`); a new crate that forgets the attribute silently allows `unsafe`. |
| | C. Crate-local `deny`, one `#[allow]` on `unix::sys` exposing safe functions | PR2 D7 | **Adopted**, with a drift test for the restated clippy table. |
| **Syscall wrappers** | A. `libc` only | spike (`libc` for every call) | Rejected: every signal, wait, and poll would be `unsafe`. |
| | B. `nix 0.25.1` | portable-pty's dependency | Rejected: an old release line, and a second wrapper crate next to the rustix already in the lock. |
| | C. `rustix 1.1.6` for everything it wraps, `libc` for libproc and `sysctl` | `alacritty_terminal` already uses rustix on Unix (`Cargo.toml:96-99`) | **Adopted.** Nothing new enters the lock; fds are `OwnedFd`/`BorrowedFd`. |
| **Input handle** | A. portable-pty `take_writer` | spike `pty.rs` | Rejected: `\n` + VEOF on drop (`unix.rs:351-363`). |
| | B. A `File` over a CLOEXEC duplicate of the master | PR2 §2.1 obligation | **Adopted.** |
| **Output reader** | A. portable-pty `try_clone_reader` (blocking `read`, `EIO` → 0) | `unix.rs:314-317`, `:86-98` | Rejected: a process outside the kill that holds the slave keeps the drain thread and fd alive until it exits. |
| | B. Own reader: `poll` on master + wake pipe, `EIO` → 0 | herdr's Unix actor polls the master (PR2 §3, `src/pty/actor/unix.rs:391`) | **Adopted.** `poll` on a pty master works on macOS (measured). |
| **Exit observation** | A. `try_wait` (portable-pty → `std::process::Child::try_wait`, reaps) | portable-pty `lib.rs:260-266`; spike | Rejected: once reaped, the leader's pid, pgid, and sid can be reused, so `killpg` has a check-then-act race. |
| | B. kqueue `EVFILT_PROC` + `NOTE_EXITSTATUS` | — | Rejected: needs `unsafe` (`kevent` udata) for no gain over C. |
| | C. `waitid(.., WNOWAIT)` polling; reap in `Drop` | measured | **Adopted.** Makes group signals race-free. |
| **Process table** | A. `proc_listallpids` + `proc_pidinfo` + `getsid` | spike `os/unix.rs:25-124`, §4.6 | **Adopted for scans and `identify`**, with `ESRCH` = dead and `EPERM` = unreadable (the spike silently dropped both, `os/unix.rs:122`). |
| | B. `sysctl(KERN_PROC_ALL)` | `ps` | Rejected for scans: libc has no `kinfo_proc`; a hand-written 648-byte layout is a liability. |
| | C. `sysctl(KERN_PROC_PID)`, offset-0 start time only | measured | **Adopted for the leader's start time**, the one read that must work on a zombie. |
| **Kill sequence** | A. HUP and TERM together, 2 s grace, KILL, then recorded-pid fallback | spike `os/unix.rs:200-223`, `session.rs:347-377` | Partly adopted: the fallback and recheck; the snapshot-first order. |
| | B. HUP → TERM → KILL on the `getsid` set, 250 ms each | herdr (prior-art row 2) | Partly adopted: separate HUP and TERM waits, the session set. |
| | C. Snapshot, then HUP, TERM, KILL to the group and verified members with budget-capped waits, then a proof loop | — | **Adopted.** Escapees are recorded before their parents die; every step fits the deadline. |
| **FakeCli reporting** | A. HTTP `/witness` | spike `fake_cli.rs:113-139` | Rejected: needs an HTTP receiver in every test; the daemon's receiver is PR 9. |
| | B. Report files | — | Rejected: no death signal. |
| | C. One loopback TCP stream per fixture, EOF = death | — | **Adopted.** |
| **FakeCli binary location** | A. Guess `target/<profile>/` from `current_exe` | assert_cmd legacy fallback (`src/cargo.rs:280-296`) | Rejected: breaks with a separate `build-dir`; stale with `-p`. |
| | B. A `[[bin]]` in pty-runtime over `fake_cli::main` | — | Rejected for PR 3: a test-only binary and an optional dependency in a library crate. |
| | C. Real-PTY FakeCli tests as testkit integration tests | Cargo's `CARGO_BIN_EXE_*` | **Adopted.** |
| **FakeCli without `unsafe`** | A. `libc` termios and `signal` | spike `os/unix.rs:267-303` | Rejected: testkit keeps `forbid`. |
| | B. rustix termios, `signal_hook::flag`, `setsid` by the sleeper itself, `process_group(0)` | — | **Adopted.** |
| **Platform gate** | A. `cfg(unix)` with a Linux `/proc` table | — | Rejected: Linux is not a host (AGENTS.md) and no CI runs it; untested code. |
| | B. `cfg(target_os = "macos")` | spike `os/unix.rs:5-6` | **Adopted.** Linux builds `core`, `FakePty`, and testkit only. |

## 4. Same-Class Inventory

Every MVP place that kills processes, reads pids, or spawns children:

| Place | PR | How this design serves it or stays out |
|---|---|---|
| `Session::kill`, `Session::close` step 2, `SpawnedPty` teardown | 2 | `UnixChild::kill_tree`. |
| `chaldea stop`, `close --stop`, `resume` death check | 10 | The supervisor calls `Session::close(extra, ..)`; `KillReport::method` becomes `launch_death_confirmed.method`; `tree_dead: false` keeps `death_confirmed: false`. |
| Recorded descendants (`extra`) from hook emitters | 9, 10 | `identify(pid)` gives identities in the backend's format. The ancestry walk itself is PR 9's design. |
| Daemon restart: old tree check by recorded sid and start window | 10 | Stays out. The daemon is no longer the parent, so the zombie guarantee is gone and only verified per-pid signals are safe. `unix::kill` takes its `Os` and leader as parameters so PR 10 can add a public entry point for an unheld, recorded leader (degraded mode plus mvp §4 PR 10's start window) without changing it. |
| Daemon process rules | 8, 10 | Constraint of §2.4: no reaping of foreign children, no `SIGCHLD` ignore. Detected (`ECHILD` → `WaitFailed`, degraded kill). |
| Worktree removal in-use check | 16 | Stays out: it reads launch state (`death_confirmed`), never processes. |
| Rescue | 20 | Stays out: rescue acts through `surface_action stop`, which is the stop path above. |
| Doctor: FakeCli tree kill | 21 | `native()` + `Session` + FakeCli `spawn-tree` with a report listener; FakeCli is a library entry point so the shipped binary can embed it (PR 21 decides how, and whether `cli → testkit` is allowed). |
| Windows tree kill (job + recorded descendants) | 4 | Shares the `extra` rule (same start time), `KillReport`, `identify`'s shape, and testkit; nothing OS-specific. PR 4 adds the `breakaway` role and Windows raw console mode to FakeCli. |
| git and one-shot hook children | 6, 16, 20 | Not PTY children. A child spawned elsewhere in the daemon can inherit a slave fd during portable-pty's non-atomic CLOEXEC window (`unix.rs:29-58`; std's macOS `pipe` has the same, `sys/pipe/unix.rs:33-39`); §2.3's wake bounds the effect on the drain. |
| FakeCli's own children | 3 | testkit only; every sleeper takes `--max-life-secs`. |
| Spike watchdog and emergency kill | — | Not lifted; the supervisor (PR 10) decides. |

## 5. Resource Paths

| Resource | Created at | Owner | Bound | Released on |
|---|---|---|---|---|
| portable-pty master | stage 1 | spawn | 1 | end of stage 2 (dropped after duplication) |
| Parent's slave fd | stage 1 | spawn | 1 | right after `spawn_command` returns, `Ok` or `Err` |
| Master duplicate R + wake read end | stage 2 | `MasterReader` (drain thread) | 2 fds | drain end: EOF, read error, or close wake |
| Master duplicate W | stage 2 | `MasterWriter` (session input) | 1 fd | input dropped (writer thread end, close step 3, teardown); writes nothing |
| Master duplicate C + wake write end | stage 2 | `UnixControl` | 2 fds | `close` (wakes the reader) or drop |
| `pty-drain` thread | stage 3 | `Drain` | 1 | EOF, read error, or wake (≤ 1 MiB pending after it) |
| Leader process | stage 4 | the CLI; `UnixChild` | 1 | exit or `kill_tree` |
| Leader zombie | leader exit | `UnixChild` | 1 | `UnixChild::drop` (`waitpid` `NOHANG`) |
| Scan buffers | `kill_tree`, `identify` | call | O(processes) | return |

**Endings.**

| Ending | What happens |
|---|---|
| Validation failure | `NeverStarted`, nothing created. |
| Failure at stage 1–3 | Created fds dropped (the reader first when the drain cannot start); `NeverStarted { Setup }`, `Confirmed`. |
| Failure at stage 4 (`spawn_command` `Err`) | No process exists (std reaped the forked child). Slave and writer dropped, control closed (wake), drain finished within 2 s; `NeverStarted`, `Confirmed` or `drain_abandoned`. |
| Failure at stage 5 | `kill_tree(&[], 5 s)` with the held leader, writer dropped, control closed, drain finished within 2 s; `StartedThenFailed` with the cleanup result. |
| Child exit | `wait_timeout` reports it once; the leader stays a zombie; the drain reads on until the last slave holder closes (EOF). |
| `kill` | §2.5 within the deadline; I/O stays up. |
| `close` | PR2 §2.2 order: kill, input dropped, control closed (wake, never blocks), drain finished (EOF normally; the wake ends it otherwise), threads joined. |
| `Session` drop | As close; the last `Arc<UnixChild>` reaps the zombie. |
| Reader EOF / `EIO` | `OutputEnd::Eof`. |
| Wake without EOF | `OutputEnd::Error("pty closed before eof")`. |
| Write error | Returned to the writer (unit `Incomplete`, `InputBroken`, PR2 §2.3). |
| `ECHILD` | `WaitFailed`; `held` false; later kills run degraded. |
| Panic | No panic path in this backend's code; a poisoned `kill` mutex is taken over. |

**Waits.** Every wait has a deadline: `wait_timeout` (its timeout, 10 ms steps), `kill_tree` (lock wait, phases, and scans all end at `deadline`; at most one `getsid`/`proc_pidinfo` pair can run past it), `Drain::finish` (the session's bound), `close` and `resize` (never block), `UnixChild::drop` (`NOHANG`), spawn's failure teardown (5 s + 2 s). Two blocking calls have no deadline of their own, as in PR2 §5: the reader's `poll` (ended by EOF or the close wake) and a `write` to a CLI that stopped reading (abandoned by close). `spawn_command` blocks only until `exec` succeeds or reports its errno through std's CLOEXEC pipe.

## 6. Purpose Layer and Plan Layer

### 6.1 Purpose Layer: Acceptance Criteria

Checked on a real macOS host (and `macos-latest` CI) by running `cargo test -p chaldea-testkit --test pty_unix` and `cargo test -p chaldea-pty-runtime`; a manual run can also watch `ps -axo pid,ppid,pgid,tpgid,stat,command | grep chaldea-fake-cli` before and after a kill.

| ID | Criterion (EARS) | Source | How to observe on a real macOS host |
|---|---|---|---|
| PR3-P-1 | When a CLI tree with members that ignore SIGHUP and SIGTERM, double-fork, change group, or leave the session with a live parent is killed, no member shall be alive at the deadline and `tree_dead` shall be true, while an unrelated process shall survive. | mvp §4 PR 3 obligation; spike B-7, C8 | FakeCli `spawn-tree` + `unrelated`: every role's report stream closes, the unrelated one stays open; `ps` shows no `chaldea-fake-cli` of the session. |
| PR3-P-2 | The backend shall never signal a process that is not the recorded one. | spike §4.6; arch §4.5 | A stale `extra` identity (the unrelated process's pid with another start time) leaves it alive. |
| PR3-P-3 | When a spawn fails, no process and no fd of it shall remain, and the error shall say whether the program ran. | PR2 §2.1; spike F-2c | Missing or non-executable program: `NeverStarted` with its kind; the test process's `/dev/fd` set is unchanged. |
| PR3-P-4 | When a CLI is spawned, it shall run as a session leader in a PTY of the requested size, cwd, and environment, receive written bytes intact and whole, and receive no byte when the input is dropped. | PR2 §2.1 | `/bin/stty size`, `/bin/pwd`, `/usr/bin/env` output; FakeCli `echo` reports; `split_writes == 0`. |
| PR3-P-5 | When the tree has exited or the PTY is closed, the output drain shall end (EOF, or a reported error when a holder survives) and `close` shall return within its budgets. | PR2 §2.2, §2.4 | `CloseOutcome`: `output`, `abandoned` empty, `elapsed`. |
| PR3-P-6 | Every `unsafe` block shall be in `pty_runtime::unix::sys`, and every other crate shall keep `forbid(unsafe_code)`. | PR2 D7; mvp §3 | `tests/lints.rs`; clippy. |
| PR3-P-7 | The workspace shall build and pass on `windows-latest` with the native-backend tests removed by `cfg`. | mvp §4 "each PR keeps core + FakePty green" | The `workspace (windows-latest)` job. |

### 6.2 Plan Layer: Claims

| ID | Claim (EARS) | Source | Falsifying test |
|---|---|---|---|
| PR3-CL-1 | If `argv[0]` is missing or not executable, or the spec fails validation, then `spawn` shall return `NeverStarted` with kind `NotFound`, `PermissionDenied`, or `Setup`, start no process, and leave the caller's open fds unchanged. | PR2 §2.1; §2.2 | `unix::tests::missing_program_is_not_found`, `unexecutable_program_is_permission_denied` (a temp file mode 0644), `invalid_specs_are_refused` (relative `argv[0]`, missing cwd, duplicate key, `=` in key, NUL, no `SHELL`), `failed_spawn_leaves_no_fd` (`/dev/fd` listing before and after) |
| PR3-CL-2 | If a stage after the PTY is opened fails, then `spawn` shall release in the order kill, input, control, drain; return `NeverStarted` when no process from the spawn exists and `StartedThenFailed` otherwise; and report `Cleanup::Confirmed` only when the tree is dead and the drain ended. | PR2 §2.1; spike F-2c | `unix::tests::drain_start_failure_releases_everything`, `exec_failure_after_fork_is_never_started` (an executable file with an invalid format: `ENOEXEC` after fork, no child left: `waitpid(-1, NOHANG)` gives `ECHILD`), `identity_failure_kills_the_started_tree` (`/bin/sh -c 'sleep 30 & echo $!; exec sleep 30'`, fault at stage 5: both pids dead, `StartedThenFailed`, `Confirmed`); faults injected by a `#[cfg(test)]` stage hook |
| PR3-CL-3 | When `spawn` returns, the PTY shall have `spec.size` with 8×16 px cells, and a resize shall reach the CLI with the same cell size. | PR2 §2.1 | `unix::tests::pty_opens_at_spec_size` (`/bin/stty size`), testkit `pty_unix::resize_reaches_the_cli` (`attachable` size report: cols, rows, `xpixel = 8 × cols`, `ypixel = 16 × rows`) |
| PR3-CL-4 | The child's environment shall equal `spec.env` (with `SHELL` required, OQ-1), with nothing from the daemon's environment. | PR2 §2.1; mvp §2.7 env scrub | `unix::tests::env_is_exactly_the_spec` (`/usr/bin/env`; the test process sets a canary variable that must not appear) |
| PR3-CL-5 | When `spawn` succeeds, the child shall run `argv[0]` in `spec.cwd` as a session and group leader with the PTY on fds 0–2 and no other fd. | PR2 §2.1; portable-pty `unix.rs:200-247` | `unix::tests::cwd_is_honored` (`/bin/pwd`), `unix::tests::child_leads_its_session` (`getsid(pid) == pid`, `identify(pid).group == pid`), testkit `pty_unix::child_fds_are_only_the_pty` (`echo` hello `fds == [0,1,2,3]`, 3 being `/dev/fd`'s own handle) |
| PR3-CL-6 | When the last process holding the slave exits, the drain shall end with `OutputEnd::Eof`; a read `EIO` shall be EOF. | PR2 §2.1; spike §4.7 | `unix::tests::exit_ends_output_with_eof` (`/usr/bin/true`; fails if the parent kept its slave), `unix::io::tests::eio_is_eof` |
| PR3-CL-7 | When the control is closed while another process holds the slave, the reader shall return the bytes pending at that moment, then end with an error that is not EOF, and `close` shall not wait for the holder. | §2.3 | `unix::io::tests::wake_ends_the_reader_with_a_slave_open` (test keeps a slave fd), testkit `pty_unix::close_with_a_surviving_tty_holder_is_bounded` (`--escape-orphan --orphan-keeps-tty`, `close(&[])`: `output == Some(Error)`, `abandoned` empty; then killed through `extra`) |
| PR3-CL-8 | When the session input is dropped, no byte shall reach the slave. | PR2 §2.1 | `unix::io::tests::dropping_input_writes_nothing` (with the positive control that dropping portable-pty's writer does deliver `\n` + VEOF, so the test can see bytes) |
| PR3-CL-9 | The input shall be a blocking fd, and a large write shall be accepted whole. | PR2 §2.1 | `unix::io::tests::input_is_blocking` (`fcntl_getfl` without `NONBLOCK`), testkit `pty_unix::input_reaches_the_cli_whole` (256 KiB unit with CJK and emoji to `echo` after its hello: reported bytes equal, `split_writes == 0`) |
| PR3-CL-10 | `wait_timeout` shall return by its timeout and shall not wait for a running `kill_tree`. | PR2 §2.1 | `unix::tests::wait_timeout_returns_by_its_timeout`, `unix::tests::wait_timeout_runs_during_kill` (`sh` trapping HUP and TERM; `wait_timeout(10 ms)` from a second thread during the kill returns within 200 ms) |
| PR3-CL-11 | When the leader exits, `wait_timeout` shall report its code or signal once and leave it unreaped until the child is dropped. | §2.4 | `unix::tests::exit_reports_code_and_signal`, `unix::tests::exit_is_observed_without_reaping` (after `Exited`: `test_kill_process(pid)` is `Ok`; after drop: `ESRCH`) |
| PR3-CL-12 | When `kill_tree` runs, it shall kill the group, the session's other groups, live descendants of members, and verified `extra` identities, escalating HUP → TERM → KILL, and report `tree_dead: true` only from a complete scan with no live member. | mvp §4 PR 3; prior-art row 2 | testkit `pty_unix::spawn_tree_dies_and_unrelated_survives` (roles `plain`, `ignore-term`, `double-forked`, `own-group`, `setsid`); unit `unix::kill::tests::session_member_outside_the_group_is_killed`, `descendant_of_a_member_is_killed`, `term_ignoring_member_gets_kill`, `incomplete_scan_is_not_proof` |
| PR3-CL-13 | When an `extra` identity is live with its recorded start time, `kill_tree` shall kill it and its descendants. | PR2 §2.1; mvp §4 PR 10 | testkit `pty_unix::extra_reaches_an_orphaned_escapee` (without `extra` it survives; with it, it dies) |
| PR3-CL-14 | The backend shall send a group signal only while the leader is its unreaped child, and a per-pid signal only to a live process whose start time equals the recorded one, never to pid 0, 1, or its own. | §2.5 | unit `unix::kill::tests::reused_pid_in_extra_is_not_signalled`, `descendant_needs_a_verified_parent` (a child older than its parent is excluded), `group_signals_need_a_held_leader` (`ECHILD` and pid `L` reused: no `killpg`, no sid/pgid targets), `init_and_self_are_never_signalled`; testkit `pty_unix::stale_extra_identity_is_not_signalled` (the unrelated pid with its start + 1 µs survives) |
| PR3-CL-15 | `kill_tree` shall return by its deadline, plus at most one table read, with survivors listed and `tree_dead: false` when it could not prove death, including members it cannot read or signal. | PR2 §2.1 | unit `unix::kill::tests::kill_returns_by_its_deadline_with_survivors` (an `Os` whose signals have no effect), `unreadable_member_is_a_survivor`, `small_budget_sends_kill_only`, `busy_lock_returns_by_the_deadline` |
| PR3-CL-16 | A second `kill_tree` on a dead tree shall return `tree_dead: true` without waiting. | PR2 §2.1 idempotent | `unix::tests::kill_is_idempotent` (second call under 500 ms) |
| PR3-CL-17 | The leader's start time from `sysctl` shall equal `identify`'s, and `identify` shall return `None` for a zombie or absent pid. | §2.1, §2.4 | `unix::tests::leader_start_matches_identify`, `unix::tests::identify_of_a_zombie_is_none`, `unix::sys::tests::bsd_info_reports_eperm_and_esrch` (pid 1 → `EPERM`; an exited unreaped child → `ESRCH`) |
| PR3-CL-18 | `#[allow(unsafe_code)]` shall occur exactly once in `crates/pty-runtime/src`, on `unix::sys`; pty-runtime's clippy table shall equal the workspace's; every unsafe block shall carry a `SAFETY` comment. | PR2 D7 | `tests/lints.rs::unsafe_is_allowed_only_in_sys`, `tests/lints.rs::clippy_table_matches_workspace`; clippy `undocumented_unsafe_blocks = deny` |
| PR3-CL-19 | FakeCli shall start every PR 3 mode, report a hello with its pid, end a sleeper by `--max-life-secs`, report each spawned role, and exit 2 on an unknown mode or flag. | mvp §3 lift table | `fake_cli::every_mode_reports_hello`, `fake_cli::spawn_tree_reports_each_role`, `fake_cli::sleeper_ends_after_max_life`, `fake_cli::unknown_mode_exits_2`, `fake_cli::queries_reports_unanswered_queries` (stdin a pipe: `all_ok: false` within 4 s) |
| PR3-CL-20 | The VT responder's replies shall reach a CLI through the real PTY. | PR2 §2.5 via a real backend | testkit `pty_unix::queries_are_answered_through_the_pty` (`all_ok: true`) |
| PR3-CL-21 | `chaldea-testkit` shall depend on no workspace crate other than `chaldea-core`. | mvp §3 | CI step (§7) |

**Obligation mapping** (PR2 §2.1 → claims): release on failure and `Unresolved` → CL-1, CL-2; opened at `spec.size` → CL-3; environment → CL-4; drain from the first byte, `EIO` → EOF, slave dropped → CL-6, CL-7 (the drain itself is PR 2's, `Drain::start` at stage 3); dropping input writes nothing → CL-8; whole writes → CL-9; `wait_timeout` → CL-10, CL-11; `kill_tree` with `extra`, deadline, survivors → CL-12 to CL-16; `close` by deadline → CL-7 (`close` never blocks); `evidence` → none on Unix (§2.2); `unsafe` placement → CL-18. PR 3 row obligation (survivors and negative control) → CL-12, CL-13, CL-14.

## 7. Tests and Completion Commands

**Where tests run.**

| Tests | File | OS |
|---|---|---|
| Kill engine over a synthetic `Os` | `crates/pty-runtime/src/unix/kill.rs` `#[cfg(test)]` | macOS (the module is macOS-only) |
| Reader, writer, `sys` | `src/unix/io.rs`, `src/unix/sys.rs` `#[cfg(test)]` | macOS, real PTY via portable-pty |
| Spawn, child, exit, kill with system binaries | `src/unix/tests.rs` | macOS, real PTY |
| Lints | `crates/pty-runtime/tests/lints.rs` | all |
| PR 2's `FakePty` tests | unchanged | all |
| FakeCli modes without a PTY | `crates/testkit/tests/fake_cli.rs` | all (Unix-only roles behind `cfg(unix)`) |
| Native backend + FakeCli | `crates/testkit/tests/pty_unix.rs` | macOS |

**Timing.** No test synchronizes by sleeping. A test waits for a FakeCli hello or event (the fixture is in raw mode once its hello arrives), for `SessionEvent::Exited`, or for a report stream's EOF, each with a 10 s bound. Kill deadlines are 5 s; "returns by its deadline" allows 500 ms of slack. The unit tests of the kill engine use a fake clock (`Os::now`, `Os::sleep`), so phase boundaries and deadline cases are exact. Survival is asserted after `kill_tree` returned, with `closed_by(now + 500 ms) == false`; a wrongly signalled process would have been signalled inside the call.

**Known flaky sources and their controls.**

- *fd inheritance between concurrent spawns.* macOS has no atomic CLOEXEC for `openpty` or std's `pipe`; a child spawned on another test thread can inherit a slave fd and delay that test's EOF. Every test that spawns real processes takes a file-level `static REAL: Mutex<()>` for its whole body (one in `src/unix/tests.rs` shared with `io.rs`/`sys.rs` tests, one in `pty_unix.rs`); separate test binaries are separate processes.
- *Leaked fixtures.* Every sleeper runs with `--max-life-secs 60`; the test kills its `unrelated` child itself (`Child::kill`, `wait`); an orphan made on purpose is killed through `extra` before the test ends.
- *Bytes before raw mode.* Input written before FakeCli's raw mode is cooked by the line discipline; tests write only after the hello.
- *Runner PTYs.* `openpty` needs no controlling terminal. A runner without PTY access fails these tests; none of them skips.

**Completion commands** (PR2 §7, extended):

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
deps="$(cargo tree -p chaldea-core -e normal --prefix none)" && ! grep -Eq '^(tokio|rusqlite|axum) ' <<<"$deps"
deps="$(cargo tree -p chaldea-pty-runtime -e normal --prefix none)" && ! grep -Eq '^tokio ' <<<"$deps"
deps="$(cargo tree -p chaldea-testkit -e normal --prefix none)" && ! grep -Eq '^chaldea-(pty-runtime|daemon|cli) ' <<<"$deps"
```

plus the `workspace` job green on both runners with the new step, and the `pty-hooks` job unchanged.

## 8. Decisions

**D1. The native Unix backend is macOS-only** (`cfg(target_os = "macos")`). Its process table is libproc and `sysctl`, the spike was macOS-only, and no CI runs Linux. Linux still builds `core`, `FakePty`, and testkit. Reopen when Linux becomes a host: add a `/proc` table behind the same `Os` trait.

**D2. The leader stays unreaped until the child is dropped.** `waitid(WNOWAIT)` reports the exit; `Drop` reaps. That makes every group signal race-free. Per-pid signals keep the spike's verify-then-signal rule with the residual window stated in §2.5; macOS offers nothing narrower. The daemon must not reap foreign children (§2.4).

**D3. The kill takes a snapshot first, escalates HUP → TERM → KILL with budget-capped waits, and proves death by a complete scan.** HUP first gives CLIs their terminal-close cleanup (prior-art row 2); the snapshot records escapees while their parents live; the proof loop catches processes forked during the kill.

**D4. The session's input and the drain's reader are this backend's own handles on master duplicates**, the reader with a close wake. Dropping the input writes nothing; closing the PTY ends the drain in bounded time even with a surviving slave holder.

**D5. Scans and `identify` use libproc with `ESRCH` = dead and `EPERM` = unreadable; the leader's start time uses `sysctl`.** An unreadable member is never assumed dead.

**D6. `unsafe` lives in one module, `unix::sys`, under a crate-local `deny`**, with `undocumented_unsafe_blocks` and `multiple_unsafe_ops_per_block` denied and the restated clippy table guarded by a test.

**D7. PR 3 adds `native()` and `identify()`.** Tests need a native runtime now, and `extra` can only be built in the backend's identity format. PR 4 adds the Windows arms.

**D8. FakeCli is library code with a thin binary, reports over loopback TCP, and lifts only the modes PR 3 tests use.** Its real-PTY tests are testkit integration tests, so Cargo locates the binary. `stall`, `banner`, hooks, the `mcp` role, attach input parsing, and Windows console modes come with the PR that first tests them.

**D9. The backend validates the spec before creating anything** (absolute `argv[0]`, executable, `cwd` a directory, env keys) and classifies `NeverStarted` from that check and from the errno std reports, since portable-pty would otherwise run the child in `$HOME` or flatten the errno into a string.

### Open Questions

**OQ-1. `SHELL` and "the environment is exactly `spec.env`" (PR2 §2.1).** portable-pty sets `SHELL` in every child (`cmdbuilder.rs:479-480`): to `spec.env`'s value if present, else to the password database's login shell. The obligation cannot hold for a spec without `SHELL`.

| Option | Gains | Losses | Reversibility |
|---|---|---|---|
| A. The backend refuses a spec without `SHELL` (`NeverStarted { Setup, InvalidInput }`); the daemon always passes it (it is not on the scrub list) | §2.1 stays literal and testable | One more precondition on `SpawnSpec`; a daemon bug shows up as a spawn refusal | Reversible to B by deleting a check |
| B. Amend §2.1: "exactly `spec.env`, plus `SHELL` from the password database when absent (Unix)" | No refusal | The contract gains an OS exception; the added value is invisible to the daemon | Reversible to A |
| C. Bypass portable-pty's `spawn_command` with an own `setsid`/`TIOCSCTTY` `pre_exec` | Exact environment, no precondition | Contradicts the recorded decision to keep portable-pty (prior-art row 1d, AGENTS.md); new `unsafe` (`pre_exec`) | Costly |

Recommendation: A. This design implements A; choosing B removes one validation row and changes CL-4's test.

## Related Text in Other Documents

These change when this design is adopted (not edited here):

- `docs/design/pty-runtime.md` §1: "PR 3 adds `src/unix.rs` (`#[cfg(unix)]`)" → `src/unix/` behind `cfg(target_os = "macos")`; portable-pty under `[target.'cfg(target_os = "macos")'.dependencies]` with rustix and libc; `native()` arrives in PR 3 (PR 4 adds its Windows arm), plus `identify()`.
- `docs/design/pty-runtime.md` §2.1: "Unix `try_wait` polling" → `waitid(WNOWAIT)` polling with reaping on drop; "Every `unsafe` block lives in `unix.rs` / `windows.rs`" and D7 "allows `unsafe` only in `unix.rs` / `windows.rs`" → `unix::sys` (and PR 4's Windows module); the environment sentence per OQ-1.
- `docs/mvp-design.md` §3 lift table, `src/fake_cli.rs` row: PR 3 lifts `echo` (input reporting), `queries`, `attachable` (size reporting), `spawn-tree`, `sleeper`; `stall` and `banner` move to PR 10; `/witness` is replaced by the report stream.
- `docs/mvp-design.md` §4 PR 8 / PR 10 obligations: the daemon never reaps children it does not own and never ignores `SIGCHLD` (§2.4).
- `docs/mvp-design.md` §7 doctor row: shipping FakeCli in `chaldea` needs `cli` to call `chaldea_testkit::fake_cli::main` or an equivalent; the dependency direction of §3 does not yet allow `cli → testkit`.
