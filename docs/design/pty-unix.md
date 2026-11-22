# Design: Unix PTY Backend and `crates/testkit` (MVP PR 3)

Status: design, not implemented. Scope is the PR 3 row of `docs/mvp-design.md` §4 (cited as "mvp §N"): the Unix backend of `PtyRuntime` (`killpg` HUP→TERM→KILL, `getsid` scan, recorded-pid fallback) and the new `crates/testkit` crate with `chaldea-fake-cli`. Its test obligation: FakeCli `spawn-tree` survivors and the unrelated negative control. The contract implemented is `docs/design/pty-runtime.md` (cited as "PR2 §N"), §2.1 "What each backend must supply"; no trait or public type of PR 2 changes.

The backend does not use portable-pty. It opens the PTY with rustix and starts the child with Darwin `posix_spawn` (§3 "Spawn primitive" gives the evidence; "Related Text in Other Documents" lists the sentences of `AGENTS.md`, `docs/architecture.md`, and `docs/prior-art.md` that name portable-pty for macOS).

Out of scope: the Windows backend (PR 4; the `cfg` layout leaves room for `src/windows/`), daemon wiring and its restart scan (PR 10), input encoders (PR 5), the gate and send path (PR 10), attach (PR 11), and every FakeCli feature only those PRs use.

Sources read for this document (paths are relative to each root):

| Source | Revision | Read for |
|---|---|---|
| This repository | `b35d687` | `crates/pty-runtime/src/{backend,session,drain,fake,lib}.rs`, `Cargo.toml`, `.github/workflows/ci.yml`, `lefthook.yml`; spike `spikes/pty-hooks/src/os/{unix,mod}.rs`, `src/session.rs`, `src/fake_cli.rs`; `docs/spikes/pty-hooks-spike.md` §4.5–§4.7; `docs/prior-art.md` rows 1d, 2 |
| `portable-pty` | crate 0.8.1 | `src/unix.rs`, `src/cmdbuilder.rs` (the defects of §3) |
| `rustix` | crate 1.1.6 (in `Cargo.lock` through `alacritty_terminal`) | `src/pty.rs`, `src/process/{wait,kill,id}.rs`, `src/event/poll.rs`, `src/termios/{tc,tty}.rs`, `src/io/fcntl.rs` |
| `libc` | crate 0.2.191 (in `Cargo.lock`) | `src/unix/bsd/apple/mod.rs`: `posix_spawn*`, `proc_*`, `PROC_PIDTBSDINFO`, `SZOMB`; no `POSIX_SPAWN_SETSID` |
| macOS SDK | Command Line Tools, macOS 26 SDK | `usr/include/sys/spawn.h:61-62` (`POSIX_SPAWN_SETSID 0x0400`, `POSIX_SPAWN_CLOEXEC_DEFAULT 0x4000`), `usr/include/spawn.h:184-185` (`addchdir_np` deprecated in 26.0) |
| `signal-hook` | crate 0.4.6 (in `Cargo.lock`) | `src/flag.rs` |
| Rust std | 1.96.0 `rust-src` | `library/std/src/sys/process/unix/unix.rs` (exec-error pipe), `sys/pipe/unix.rs` (non-atomic CLOEXEC on macOS) |
| `assert_cmd` | crate 2.2.2 | `src/cargo.rs` (how a test finds a binary) |

**Host facts measured for this design** on macOS 26.7.3 (arm64) with throwaway C probes (not committed; the tests named in §6 check each fact this design relies on):

| # | Fact | Observed |
|---|---|---|
| F1 | `posix_spawn` with `POSIX_SPAWN_SETSID` and a file action opening the slave by path onto fd 0 makes the slave the controlling terminal | child pid = pgid, `tpgid` = pid, `tty` = the slave; `SIGWINCH` delivered on `TIOCSWINSZ` |
| F2 | `POSIX_SPAWN_CLOEXEC_DEFAULT` keeps an fd the parent opened without `FD_CLOEXEC` out of the child | fd 50 absent; with fork + exec it is present |
| F3 | `posix_spawn` reports exec failure as its return value, with no pid | missing program, missing shebang interpreter, missing cwd: `ENOENT`; invalid format: `ENOEXEC` (no `/bin/sh` fallback) |
| F4 | After a failed `posix_spawn` a transient child exists briefly, is not waitable, and is gone by itself | listed by `proc_listchildpids` at once (status `SRUN`), gone at 300 ms; `waitpid(-1, WNOHANG)` = `ECHILD` |
| F5 | The environment is exactly `envp`; `POSIX_SPAWN_SETSIGDEF` restores a signal the parent ignores | `/usr/bin/env` prints only `A=1`; `SIGPIPE` default in the child |
| F6 | `posix_openpt(O_RDWR \| O_NOCTTY \| O_CLOEXEC)` sets `FD_CLOEXEC` | yes |
| F7 | `waitid(P_PID, pid, WEXITED \| WNOHANG \| WNOWAIT)` reports an exit repeatedly without reaping | `CLD_EXITED`, twice |
| F8 | A process group whose leader is an unreaped zombie still exists | `killpg(g, 0)`: 0 with a live member, `EPERM` with only the zombie, `ESRCH` only after `waitpid` |
| F9 | `proc_pidinfo(PROC_PIDTBSDINFO)` with `arg` 0 fails for a zombie (`ESRCH`); with `arg` 1 it returns the zombie's record (status `SZOMB`, pgid, start); it fails with `EPERM` for another user's process (pid 1) | all three |
| F10 | `getsid(pid)` reads processes in other sessions; `ESRCH` for a zombie | both |
| F11 | `O_NONBLOCK` set on one duplicate of the master applies to every duplicate | yes |
| F12 | With a nonblocking master: a full slave input queue (~1 KiB raw) gives `EAGAIN`, `poll(POLLOUT)` waits until the slave reads; data flushed by the slave (`tcflush(TCOFLUSH)`) between `poll` and `read` gives `EAGAIN`, not a hang; after the last slave close `poll` gives `POLLIN \| POLLHUP` and `read` 0 | all |
| F13 | Consecutive spawns get consecutive pids | `n`, `n+1` |

## 1. Goal and Files

**Goal.** A macOS backend that opens a PTY, starts the CLI with `posix_spawn` as a session leader with the PTY as controlling terminal and nothing else inherited, hands the session a drain over the master, an input that writes nothing when dropped, and a child whose `kill_tree` kills the CLI's session, the descendants it can verify, and the recorded extras, without ever targeting a group or session it no longer owns; plus a `testkit` crate whose `chaldea-fake-cli` stands in for a CLI in real-PTY tests. Every claim of §6 is checked by a named test on `macos-latest`; `windows-latest` builds everything and runs the `FakePty` and FakeCli smoke tests.

**Files.**

```
Cargo.toml                                # members += "crates/testkit"; [workspace.dependencies] += rustix, libc, signal-hook
crates/pty-runtime/Cargo.toml             # macOS deps; own [lints] table (D6)
crates/pty-runtime/src/lib.rs             # #[cfg(target_os = "macos")] mod unix; pub use unix::native
crates/pty-runtime/src/unix/mod.rs        # UnixPty: PtyRuntime; validation; the spawn sequence (§2.2); native()
crates/pty-runtime/src/unix/io.rs         # MasterReader, MasterWriter, UnixControl, the wake (§2.3, §2.6)
crates/pty-runtime/src/unix/child.rs      # UnixChild: PtyChild; ownership; wait without reaping; reaping (§2.4)
crates/pty-runtime/src/unix/reaper.rs     # one process-wide `pty-reaper` thread for leaders that outlive their kill (§2.4)
crates/pty-runtime/src/unix/table.rs      # Proc, Lookup, Pass: one process-table pass (§2.5)
crates/pty-runtime/src/unix/kill.rs       # the kill engine over an `Os` trait; unit tests with a synthetic `Os` and clock
crates/pty-runtime/src/unix/sys.rs        # the only module allowed `unsafe` (§2.7): posix_spawn, proc_listallpids, proc_pidinfo
crates/pty-runtime/src/unix/tests.rs      # #[cfg(test)] real-process unit tests with system binaries and stage hooks
crates/pty-runtime/src/fake.rs            # += #[cfg(all(feature = "fake", target_os = "macos"))] fn identify (test route, §2.1)
crates/pty-runtime/tests/unix_spawn.rs    # #![cfg(target_os = "macos")] public-API spawn tests; every test holds one lock (§7)
crates/pty-runtime/tests/lints.rs         # lints table and the single unsafe allowance (all OSes)
crates/testkit/Cargo.toml                 # chaldea-testkit, publish = false, [lints] workspace = true
crates/testkit/src/lib.rs                 # pub mod fake_cli; pub mod report
crates/testkit/src/fake_cli/mod.rs        # pub fn main(args) -> i32: option parsing and mode dispatch
crates/testkit/src/fake_cli/{term,echo,queries,attachable,spawn_tree,sleeper}.rs
crates/testkit/src/report.rs              # report wire format; Reporter (fixture side); ReportListener, Fixture, Cleanup (test side)
crates/testkit/src/bin/chaldea-fake-cli.rs   # fn main() { std::process::exit(chaldea_testkit::fake_cli::main(std::env::args_os())) }
crates/testkit/tests/fake_cli.rs          # every PR 3 mode without a PTY (all OSes)
crates/testkit/tests/pty_unix.rs          # #![cfg(target_os = "macos")] native backend + FakeCli (§7)
.github/workflows/ci.yml, lefthook.yml    # `workspace` job: testkit dependency check (§7)
```

PR 4 adds `src/windows/` behind `#[cfg(windows)]` and a Windows arm of `native()`; no Unix file moves.

**Dependencies.** Exact pins, all already in `Cargo.lock`; the lock gains no package.

```toml
# root [workspace.dependencies]
rustix = { version = "=1.1.6", default-features = false, features = ["std", "pty", "process", "event", "termios", "fs"] }
libc = "=0.2.191"
signal-hook = { version = "=0.4.6", default-features = false }

# crates/pty-runtime/Cargo.toml
[target.'cfg(target_os = "macos")'.dependencies]
rustix.workspace = true        # openpt, grantpt, unlockpt, ptsname, open, dup, waitid, waitpid, kill, killpg, getsid, poll, tcsetwinsize
libc.workspace = true          # posix_spawn and libproc: no safe wrapper exists (§2.7)

# crates/testkit/Cargo.toml
[dependencies]
serde_json.workspace = true    # report lines
[target.'cfg(unix)'.dependencies]
rustix.workspace = true        # raw mode, isatty, ttyname, tcgetpgrp, tcgetwinsize, getsid, getpgrp, setsid
signal-hook.workspace = true   # `flag::register` ignores SIGHUP/SIGTERM without `unsafe`
[dev-dependencies]
chaldea-pty-runtime = { path = "../pty-runtime", features = ["fake"] }   # pty_unix.rs: native() and the identify test route
```

**Workspace and CI.** `crates/testkit` joins `members`. The `workspace` job runs unchanged on both runners (fmt, clippy `-D warnings`, `cargo test --workspace --locked`); on `macos-latest` that includes the real-PTY tests, which `cfg` removes on Windows. One step is added: testkit's normal dependencies contain no workspace crate other than `chaldea-core` (§7). `lefthook.yml` mirrors it.

## 2. Types and Functions

Names may change in review; shapes and rules may not.

### 2.1 Public Additions

```rust
/// The native backend of this OS (macOS: this document; PR 4 adds the Windows arm).
#[cfg(target_os = "macos")]
pub fn native() -> Arc<dyn PtyRuntime>;
```

Process identity lookup stays internal in PR 3. Tests reach it through `chaldea_pty_runtime::fake::identify(pid) -> io::Result<Option<ProcessIdentity>>` (`fake` feature, macOS), which no default build contains (PR2 D6). The public lookup the daemon needs to build `extra` (PR 9 / PR 10) is added by the PR that first consumes it.

**Identity format** (`ProcessIdentity`, PR2 §2.1): `pid` the process id; `start_time` the kernel's start stamp in decimal microseconds since the Unix epoch (`pbi_start_tvsec * 1_000_000 + pbi_start_tvusec`, the spike's value, `os/unix.rs:90`); `group` the process group id in decimal (the leader's equals its pid). The start time is used only for equality: equal `(pid, start_time)` identifies a process. It is a wall-clock stamp, so nothing in this backend orders processes by it.

### 2.2 Spawn

```rust
struct UnixPty;                                   // sealed::Sealed + PtyRuntime
```

**Validation** (before anything is created; a failure is `NeverStarted` with `Cleanup::Confirmed`):

| Check | Failure |
|---|---|
| `argv` non-empty; no NUL in any argument | `Setup` (`InvalidInput`) |
| `argv[0]` absolute | `Setup` (`InvalidInput`) |
| `metadata(argv[0])` | by its error kind: `NotFound` → `NotFound`, `PermissionDenied` → `PermissionDenied`, other → `Setup` |
| `argv[0]` is a regular file and `rustix::fs::access(X_OK)` passes | `PermissionDenied` |
| `cwd` absolute and a directory | `Setup` (error kind `NotFound` when missing) |
| env keys non-empty, without `=` or NUL; values without NUL; no key twice | `Setup` (`InvalidInput`) |

Validation gives early, classified errors; correctness does not depend on it, because `posix_spawn` re-checks everything and reports any failure before the program runs.

**Sequence.** Each stage lists what exists when it fails.

1. **Open the PTY.** `rustix::pty::openpt(RDWR | NOCTTY | OpenptFlags::from_bits_retain(O_CLOEXEC))` (rustix declares `CLOEXEC` only for Linux and BSDs, `pty.rs:40-46`, and passes unknown bits through; macOS honours the bit, F6), `grantpt`, `unlockpt`, `ptsname`; `tcsetwinsize(master, { ws_row, ws_col, ws_xpixel: cols × 8, ws_ypixel: rows × 16 })`, so the PTY has `spec.size` with the 8×16 px cells the responder reports (PR2 §2.5). Set `O_NONBLOCK` on the master (shared by every duplicate, F11). Failure: `NeverStarted { Setup }`.
2. **Ends.** Open the slave in the parent with `O_RDWR | O_NOCTTY | O_CLOEXEC` (`rustix::fs::open`), so the master never reads EOF before the child holds the slave. Three `fcntl_dupfd_cloexec` duplicates of the master (reader, writer, control); the original master fd is closed. One wake pipe (`std::io::pipe()`). Every fd this backend creates is close-on-exec from creation except the wake pipe (std sets it after `pipe` on macOS, `sys/pipe/unix.rs:33-39`); §2.3 tolerates a leaked wake pipe. Failure: everything created is dropped; `NeverStarted { Setup }`.
3. **Drain.** `Drain::start(MasterReader { reader dup, wake read end }, output)` (PR2 §2.4). Failure: `Drain::start` drops the reader first; the rest is dropped; `NeverStarted { Setup }`.
4. **Spawn** with `sys::spawn(&Launch)`: `posix_spawn(argv[0], file_actions, attr, argv, envp)` with
   - `attr` flags `POSIX_SPAWN_SETSID | POSIX_SPAWN_CLOEXEC_DEFAULT | POSIX_SPAWN_SETSIGDEF | POSIX_SPAWN_SETSIGMASK`, the default set full (every signal at its default action, `SIGPIPE` included, F5) and the mask empty;
   - file actions: `addopen(0, slave path, O_RDWR)`, `adddup2(0, 1)`, `adddup2(0, 2)`, `addchdir_np(cwd)`;
   - `envp` exactly `spec.env` as `KEY=VALUE` strings, `argv` exactly `spec.argv`.

   The child is a session and group leader (pid = pgid = sid) whose controlling terminal is the slave (F1), with only fds 0–2 (F2), in `spec.cwd`, with exactly `spec.env` (F5): nothing is added, so a spec without `SHELL` runs without it. Nothing forks the daemon.
   - **Nonzero return**: the program never executed (F3). This is the positive evidence `NeverStarted` requires. `kind`: `EACCES`/`EPERM` → `PermissionDenied`; `ENOENT` → `NotFound` if `argv[0]` is missing now, else `Setup` (a missing interpreter or cwd); other → `Setup`. A transient, unwaitable child may still be exiting (F4) and can hold the slave for that time. Release: drop the parent's slave and the writer, close the control (wake), `Drain::finish(2 s)`; `Cleanup::Confirmed` when the drain ended, else `Unresolved { kill: None, close: Some(..), drain_abandoned: true }`.
   - **Zero return**: a process exists and is this process's child. The `UnixChild` guard (§2.4) is built from the pid at once, before any other call. From here every failure is `StartedThenFailed`.
5. **Settle.** Close the parent's slave (EOF then reaches the drain when the tree's last slave holder closes it, PR2 §2.1). Read the leader's identity with `proc_pidinfo(pid, PROC_PIDTBSDINFO, arg 1)`, which also returns a leader that already exited (F9), so a CLI that dies at once still gets an identity and a normal exit. `group` is the pid (stage 4 succeeds only if `setsid` did). Failure (any error from that read): `StartedThenFailed { stage: "identity", identity: None }`; release in PR 2's order: `kill_tree(&[], now + 5 s)` (the owned leader is signalled by its exact pid and group, §2.5), drop the writer, close the control, `Drain::finish(2 s)`; `Cleanup` as `SpawnedParts::teardown` computes it (`backend.rs:266-291`).
6. **Hand over** `SpawnedPty::new(SpawnedParts { child: Arc<UnixChild>, output, input: Box<MasterWriter>, control: Box<UnixControl>, size: spec.size, evidence: vec![] })`.

The Unix backend reports no `evidence`; a key added later is additive.

### 2.3 Input and Output Ends

```rust
struct Wake { read: Arc<PipeReader> }                      // shared by reader and writer; never read from
struct MasterReader { master: OwnedFd, wake: Wake, after_wake: Option<u64> }   // Read
struct MasterWriter { master: OwnedFd, wake: Wake }        // Write; Drop closes one fd and writes nothing
```

**The wake** is level-triggered: `UnixControl::close` (or its drop) writes one byte into the pipe and nobody ever reads it, so every `poll` that includes the read end returns at once from then on. A copy of either pipe end leaked into another process during std's non-atomic CLOEXEC window changes nothing: this process writes the byte, and no process reads it.

**Output** (`MasterReader::read`):

1. `poll([master: IN, wake: IN], no timeout)`; `EINTR` repeats it.
2. Any nonzero `revents` on the wake (`POLLIN`, `POLLHUP`, `POLLERR`, `POLLNVAL`) arms the post-wake budget: `after_wake = Some(0)` if unset, whether or not the master is also ready.
3. Master `POLLIN`, `POLLHUP`, or `POLLERR`: `read`. `Ok(n > 0)` returns `n` (and adds `n` to `after_wake` when armed); `Ok(0)` returns `Ok(0)` (EOF); `EAGAIN` or `EINTR` goes back to step 1 (data flushed between `poll` and `read`, F12); every other error goes through `map_read`, which turns `EIO` into `Ok(0)` (the hangup error on other Unix systems) and returns the rest. Master `POLLNVAL` returns `Err(other("pty master invalid"))`.
4. Armed and the master not ready in a zero-timeout `poll`, or `after_wake` past 1 MiB: `Err(other("pty closed before eof"))`, which the drain reports as `OutputEnd::Error`, never as EOF.

So after a close, the drain returns what the master can deliver at once, bounded by 1 MiB, then ends, even when another process still holds the slave and keeps writing. The bytes delivered after the wake are best effort.

**Input** (`MasterWriter::write(buf)`): not portable-pty's writer, whose `Drop` writes `\n` + VEOF (`unix.rs:351-363`). Loop: `write(master, rest)`; progress advances `rest`; on `EAGAIN`, `poll([master: OUT, wake: IN], no timeout)`; `EINTR` repeats. It returns `Ok(buf.len())` when every byte was accepted. If the wake fires first it returns `Ok(written)` when some bytes went out (a short write the PR 2 writer records as a split and follows with another call, which then fails) or `Err(BrokenPipe, "pty closed")` when none did. Any other write error is returned. So the seam keeps whole-write-or-fail semantics while the shared master is nonblocking, and a write blocked on a CLI that stopped reading ends at close step 4 (PR2 §2.2) instead of holding the writer thread forever: the session's `Released.input` becomes true once step 6 joins the thread. Dropping a `MasterWriter` closes its duplicate and writes nothing.

### 2.4 Child: Ownership, Exit, Reaping

```rust
struct UnixChild {
    pid: u32,
    identity: Option<ProcessIdentity>,   // None only during a stage 5 failure
    exit: OnceLock<ChildExit>,
    lost: AtomicBool,                     // ownership of the leader's wait status was lost; never cleared
    kill: Mutex<KillState>,               // targets kept across calls (§2.5); wait_timeout never takes it
}
```

**Ownership is a precondition.** The leader's wait status belongs to this `UnixChild` alone: nothing else in the process may reap it. In the daemon (PR 8 / PR 10): no wildcard wait (`waitpid(-1, ..)`, `wait()`, `waitid(P_ALL, ..)`), no `SIGCHLD` set to `SIG_IGN`, no `SA_NOCLDWAIT`; waits for its own other children (git, hooks) are by exact pid. While ownership holds, the leader stays unreaped until the backend reaps it, so its pid, and with it the group and session ids it leads, cannot be reused (F8): every numeric group or session target of §2.5 is then exactly the CLI's.

**Detecting loss.** `owned()` runs `waitid(WaitId::Pid(pid), EXITED | NOHANG | NOWAIT)` and returns false on `ECHILD`, setting `lost` for good. `kill_tree` calls it fresh immediately before every `killpg` and before every pass whose result it targets by pgid or sid. A reaper racing between that check and the signal is a broken precondition, not a case the backend defends; `lost` then makes every later call safe. After loss the backend never targets pgid or sid `L` numerically, never reaps the pid, signals only verified `(pid, start)` identities, and reports `tree_dead: false` (§2.5).

**`wait_timeout(timeout)`.** If `exit` is set, return it. Else, with `end = now + timeout`: loop `waitid(Pid(pid), EXITED | NOHANG | NOWAIT)`; an exit sets `exit` once and is returned; no exit sleeps `min(10 ms, end − now)` and returns `Ok(None)` once `now ≥ end`; `EINTR` repeats, but only while `now < end`; `ECHILD` sets `lost` and returns `Err(other("child reaped outside the backend"))` (the session publishes `WaitFailed`); other errors are returned. `CLD_EXITED` gives `ChildExit { code: Some(status), signal: None }`, `CLD_KILLED`/`CLD_DUMPED` `{ None, Some(sig) }`. It takes no lock (`OnceLock`, atomics), so it runs during `kill_tree` (PR2 §2.1).

**Reaping.** In `Drop` (the last `Arc` goes when the session closes or drops):

- owned and exited (`exit` set or `waitid` reports it): `waitpid(Some(pid), NOHANG)` reaps it;
- owned and alive (its kill failed, already reported `tree_dead: false`): handed to the reaper;
- lost: nothing.

`reaper::adopt(pid)` pushes the pid to a process-wide list and starts the `pty-reaper` thread on first use; the thread polls `waitpid(Some(pid), NOHANG)` for each listed pid every 250 ms and drops a pid once it is reaped or gives `ECHILD`. The handoff takes a mutex for one push and never waits. Bound: one thread per process; one entry per leader that outlived its session's kill.

### 2.5 Tree Kill

```rust
impl PtyChild for UnixChild { fn kill_tree(&self, extra: &[ProcessIdentity], deadline: Instant) -> KillReport; }

pub(crate) struct Proc { pid: u32, ppid: u32, pgid: u32, start: u64 }
pub(crate) enum Lookup { Live(Proc), Zombie, Gone, Unreadable }      // PROC_PIDTBSDINFO, arg 1: SZOMB, ESRCH, EPERM
pub(crate) struct Pass { procs: Vec<(Proc, Option<u32>)>,              // with getsid
                         unreadable: Vec<(u32, Option<u32>)>, clean: bool }
pub(crate) trait Os {
    fn owned(&self) -> bool;
    fn pid_list(&self, capacity: usize) -> io::Result<(Vec<u32>, bool)>;   // (pids, saturated)
    fn lookup(&self, pid: u32) -> Lookup;
    fn getsid(&self, pid: u32) -> io::Result<u32>;
    fn killpg(&self, pgid: u32, sig: Signal) -> io::Result<()>;
    fn kill(&self, pid: u32, sig: Signal) -> io::Result<()>;
    fn now(&self) -> Instant;
    fn sleep(&self, d: Duration);
}
pub(crate) struct KillState { targets: BTreeMap<(u32, u64), Target>, proven: bool }
```

**One pass.** `pid_list` (`proc_listallpids`, a count call plus a buffer of count + 64): a result that fills the buffer is saturated, and the call is retried with double the capacity, up to three times within the deadline. Then for each pid: `lookup`, then `getsid` for a `Live` one. A pass is **clean** only if the list call succeeded unsaturated, the deadline did not cut it short, and no listed pid turned `Gone` before its lookup or `getsid` (a pid that vanishes mid-pass may have forked a child that the list does not contain). A zombie is dead and is ignored; `Unreadable` pids keep their `getsid` result.

**Coverage.** With `L` the leader pid, the tree is:

- *Session members* (only while owned): every `Live` process of the pass with `pgid == L` or `sid == L`, and every unreadable pid with `sid == L`. While owned, nothing outside the CLI can be in session `L` (a process joins a session only by being forked into it, and `L` cannot be reused).
- *Targets*: identities kept in `KillState` across calls: the leader `(L, start)`; each `extra` entry, accepted only if its pid is not 0, 1, or the daemon's own; every identity found below.
- *Descendants*: the ppid closure over the pass from every live member and live target. The kernel reparents a child to launchd when its parent exits, so a `ppid` names the live parent; no wall-clock comparison is made.

**What this covers and what not.** The proof covers the original session (while owned), every descendant of a covered process that was live when a pass saw it, and the verified `extra` identities with their descendants. A process that left the session (`setsid`) and whose parent exited before any pass saw it is outside the tree: the documented macOS containment limit (spike §4.6), recovered only when the daemon names it in `extra`.

**Signals.**

- *Group*: `killpg(L, sig)` only after a fresh `owned()`; never after loss.
- *Leader by pid*: while owned, `kill(L, sig)` without a start check (`L` is the unreaped child); this covers a stage 5 failure without an identity.
- *Everyone else*, by `signal_verified(id, sig)`: `lookup(id.pid)` immediately followed by `kill(id.pid, sig)`, sent only if the lookup is `Live` with the recorded start and the pid is not 0, 1, or the daemon's own. Members found by pgid or sid are signalled this way too, with the start their pass recorded.

**Steps**, with `t0` the entry time and `B = deadline − t0`; every syscall batch is preceded by a deadline check, and every sleep is `min(step, deadline − now)`:

0. If `now ≥ deadline` at entry: no pass, no signal; return `tree_dead: proven && extra ⊆ targets && owned`, with the known live targets as survivors.
1. Take the `kill` mutex with `try_lock` polled every 10 ms until `deadline`; never acquired: `{ method: "unix_busy", tree_dead: false, survivors: known targets }`. A poisoned lock is taken over. Add the accepted `extra` entries to the targets.
2. Pass, extend the targets with members and descendants.
3. If `B > 100 ms`: **HUP** the group and every live target outside it; wait until `t0 + min(500 ms, B/4)`; pass, extend; **TERM** the same way; wait until `t0 + min(2 s, B/2)`. Each wait polls every 25 ms by looking up the targets and ends early once none is live.
4. Pass, extend, **KILL** the group and every live target outside it.
5. **Prove**, until `deadline`: pass, extend. The live set is every live member, every live target, every unreadable member, and every target whose lookup is `Unreadable`. A target is resolved only by `Gone`, `Zombie`, or `Live` with another start. If the pass is clean, ownership holds, and the live set is empty: `proven = true`, return `tree_dead: true`. If the live set is non-empty: KILL it (group and verified per pid). Sleep 25 ms; repeat.
6. At `deadline`: `tree_dead: false`, survivors the live set (`start_time` empty for an unreadable pid, `group` its pgid or empty). After ownership loss steps 2–5 run without members and group signals, and the call returns `tree_dead: false` as soon as no target is live.

`method` is `"unix_killpg_session_scan"`, or `"unix_ownership_lost"` once ownership was lost. On a proven tree a second call with no new `extra` returns from step 5's first clean pass with no signal and no grace wait (step 3 is skipped when `proven`). `kill_tree` has no panic path: every syscall error is a value.

**Never a reused pid.**

- Group and session targets are numeric only while owned, and ownership makes them exactly the CLI's (above); after loss there are none.
- A per-pid signal follows a lookup of the same pid that showed the recorded start. **Residual window:** the process can exit, be reaped by its parent, and the pid be handed to another process between that lookup and the `kill(2)`. macOS allocates pids sequentially (F13), so this needs the pid space to wrap while the daemon is descheduled between two syscalls; nothing bounds the descheduling, and macOS has no pidfd to close the gap. This is the spike's recheck rule (`os/unix.rs:127-133`, spike §4.6), kept as is.
- A pass is not atomic: a pid can be reused between the list and its lookup, or between a parent's and a child's lookup within one pass, so a descendant can be attributed to a parent that has just been replaced. The same wraparound bounds this.

### 2.6 Control

```rust
struct UnixControl { master: OwnedFd, wake: PipeWriter }   // PtyControl
```

- `resize(size)`: `tcsetwinsize(&master, { ws_row, ws_col, ws_xpixel: cols × 8, ws_ypixel: rows × 16 })`; the kernel sends `SIGWINCH` to the foreground group (F1). The `io::Error` keeps its errno. Never blocks.
- `close(deadline)`: writes the wake byte (an empty pipe accepts it at once), drops both fds, returns `CloseReport { elapsed: Some(..) }`. It never blocks, so the deadline is always met; `close_bounded` (PR2 §2.4) is Windows-only. Dropping a `UnixControl` without `close` writes the wake byte too.

### 2.7 The `unsafe` Carve-Out (PR2 D7)

`crates/pty-runtime/Cargo.toml` gets its own `[lints]` table: `[lints.rust] unsafe_code = "deny"`, `unsafe_op_in_unsafe_fn = "deny"`; `[lints.clippy]` the workspace's clippy set restated with the same values, plus `undocumented_unsafe_blocks = "deny"` and `multiple_unsafe_ops_per_block = "deny"`. `deny`, because the workspace's `forbid` cannot be lifted by an `#[allow]`; restated, because Cargo's `[lints] workspace = true` cannot be combined with crate-local keys. Every other crate keeps `[lints] workspace = true` with `forbid`.

Exactly one module is allowed `unsafe`: `#[allow(unsafe_code)] mod sys;` in `src/unix/mod.rs`. Everything else, the kill engine included, is safe code over rustix. `sys` exposes safe functions only:

| Safe function | `unsafe` operations inside | Safety argument |
|---|---|---|
| `spawn(&Launch) -> Result<u32, Errno>` | `posix_spawnattr_init`/`setflags`/`setsigdefault`/`setsigmask`/`destroy`, `sigfillset`/`sigemptyset`, `posix_spawn_file_actions_init`/`addopen`/`adddup2`/`addchdir_np`/`destroy`, `posix_spawn` | `Launch` owns every `CString` and the null-terminated `argv`/`envp` pointer arrays for the whole call; the attr and file-actions objects are `MaybeUninit` values initialised by their `init` call (checked) and destroyed on every path by a guard; `POSIX_SPAWN_SETSID` is declared locally as `0x0400` (`sys/spawn.h:61`, absent from libc 0.2.191); each FFI call is its own block. |
| `pid_list(capacity) -> io::Result<(Vec<u32>, bool)>` | `proc_listallpids` | A `Vec<pid_t>` of `capacity` entries whose byte length is passed; the return (entries written) is checked against `capacity`; equal means saturated. |
| `bsd_info(pid) -> Result<BsdInfo, Errno>` | `MaybeUninit::<proc_bsdinfo>::zeroed().assume_init()`, `proc_pidinfo(pid, PROC_PIDTBSDINFO, 1, ..)` | `proc_bsdinfo` holds only integers and `c_char` arrays, so all-zero is valid; the size passed is `size_of::<proc_bsdinfo>()`; the record is used only when the return equals that size. |

`openpt`, `grantpt`, `unlockpt`, `ptsname`, `open`, `fcntl_dupfd_cloexec`, `fcntl_setfl`, `getsid`, `kill`, `killpg`, `waitid`, `waitpid`, `poll`, `tcsetwinsize`, and `access` are rustix's safe functions. `tests/lints.rs` checks the allowance and the table (§6 CL-20).

### 2.8 `testkit` and `chaldea-fake-cli`

**Crate.** `chaldea-testkit`, `publish = false` (workspace value), `[lints] workspace = true` (so `forbid(unsafe_code)`). Normal dependencies: no workspace crate in PR 3; `core` is the only one it may add later (mvp §3). FakeCli is library code, `chaldea_testkit::fake_cli::main(args) -> i32`, and the binary `chaldea-fake-cli` is a two-line wrapper over it.

**Command line** (PR 3 subset of spike §4.5; an unknown mode or flag exits 2):

```
chaldea-fake-cli --mode <echo|queries|attachable|spawn-tree|sleeper> [--report <127.0.0.1:port>]
                 [--role <role>] [--max-life-secs <n>] [--setsid] [--escape-orphan] [--orphan-keeps-tty]
```

**Terminal.** On start, a mode that reads stdin enters raw mode (`rustix::termios` `make_raw`) only when stdin `isatty`; the hello says which (`raw`). A pipe-backed run is the explicit non-PTY mode the smoke tests use.

**Report channel.** With `--report`, each FakeCli process connects one TCP stream to the address, writes a hello line, then mode events, one JSON object per line, and never closes the stream; it closes when the process exits. A reader thread answers each `ping` line from the test with `{"event":"pong"}`. The test reads EOF on the stream as the process's death and a `pong` as proof it is alive, an oracle independent of the backend's process table. std sockets are close-on-exec, so a fixture's children never hold its stream. The hello: `{"role","pid","ppid","pgid","sid","raw","ttys","ctty","fg","fds"}`: `pgid`/`sid` from `getpgrp`/`getsid`; `ttys` the `ttyname` of fds 0–2; `ctty` whether `/dev/tty` opens; `fg` whether `tcgetpgrp(0) == getpgrp()`; `fds` the open fds other than the `/dev/fd` listing's own handle, listed before connecting. Unix-only fields are null on Windows.

**Modes lifted now** (each is what a PR 3 test uses):

| Mode | Behaviour in PR 3 | Lifted later |
|---|---|---|
| `echo` | `ESC[?2004h`, then every stdin chunk as `{"event":"input","hex":…}` | paste parsing, hooks via `--emitter`, `--nested`, `--query-mid-paste`, `--ignore-input` (PR 9 / PR 10) |
| `queries` | the spike's seven queries, 500 ms reply bound each (`fake_cli.rs:383-481`), one `{"event":"query","kind","ok","received"}` per query, then `{"event":"queries","all_ok"}`; stdin EOF ends the remaining waits at once | — |
| `attachable` | `MARKER-n` every 500 ms; `{"event":"size","cols","rows","xpixel","ypixel"}` at start and whenever `tcgetwinsize(stdin)` changes (polled every 20 ms) | input parser and key reports (PR 11), Windows console size (PR 4) |
| `spawn-tree` | starts sleepers per role, waits for each sleeper's hello on its own report stream relayed as `{"event":"tree","children":[…]}`, then idles | Windows `breakaway` role (PR 4), `--emit-output` (PR 4's F-2c cases) |
| `sleeper` | the roles below; never reads stdin; idles until killed or `--max-life-secs` | `mcp` role (PR 13 / PR 14) |
| `stall`, `banner` | not lifted | the PR that first asserts on them (PR 10: gate timeout snapshot, fake banner) |

**Sleeper roles** (Unix; each isolates one mechanism and sends its hello only once that mechanism is in place; Windows gets `plain` and `double-forked`):

| Role | How it is made; hello after | What reaches it |
|---|---|---|
| `plain` | child of spawn-tree | the group |
| `ignore-term` | handlers installed with `signal_hook::flag::register` for SIGHUP and SIGTERM | the group's KILL |
| `double-forked` | its parent `double-fork-parent` exits at once; hello after `getppid() == 1` | the group (no live parent) |
| `own-group` | spawned with `CommandExt::process_group(0)`: same session, other group | the `getsid` scan |
| `setsid` | `--setsid`: calls `setsid()` first; its parent stays alive | the ppid walk |
| `setsid-child` | child of a `setsid` sleeper | the ppid walk from a target |
| `setsid-orphan` | `--escape-orphan`: a `setsid` sleeper whose parent exits at once; hello after `getppid() == 1`; `--orphan-keeps-tty` keeps stdout on the PTY; it starts one `orphan-child` | only `extra` (the containment limit) |
| `unrelated` | started by the test itself, outside the CLI's session | must survive and answer `ping` |

**Test-side API** (`report.rs`):

```rust
pub struct ReportListener { /* 127.0.0.1:0, nonblocking accept */ }
impl ReportListener {
    pub fn bind() -> io::Result<ReportListener>;
    pub fn arg(&self) -> String;                                                   // value for --report
    pub fn accept(&self, deadline: Instant) -> io::Result<Fixture>;                // next hello
    pub fn expect_roles(&self, roles: &[&str], deadline: Instant) -> Result<BTreeMap<String, Fixture>, String>;  // exactly these
}
pub struct Fixture { pub hello: Hello, /* stream, line buffer */ }
impl Fixture {
    pub fn next_event(&mut self, deadline: Instant) -> io::Result<Option<serde_json::Value>>;  // None: stream closed
    pub fn closed_by(&mut self, deadline: Instant) -> bool;                                    // the process exited by then
    pub fn ping(&mut self, deadline: Instant) -> bool;                                         // alive: a pong came back
}
pub struct Cleanup { /* fixture identities, owned std::process::Child values */ }  // Drop: SIGKILL each still-live identity
```

`Cleanup` kills by identity verified through the `fake::identify` route (pid and start), so a failing assertion leaves no fixture behind and never signals a reused pid.

**Locating the binary.** `CARGO_BIN_EXE_<name>` exists only for integration tests of the package that defines the binary, so the real-PTY tests that need FakeCli are integration tests of testkit (`tests/pty_unix.rs`), with `chaldea-pty-runtime` as a dev-dependency, using `env!("CARGO_BIN_EXE_chaldea-fake-cli")`. `chaldea-pty-runtime`'s own tests use system binaries (`/bin/sh`, `/bin/sleep`, `/usr/bin/env`, `/bin/pwd`, `/bin/stty`, `/bin/echo`, `/usr/bin/true`, `/usr/bin/pgrep`). A later crate whose tests need FakeCli declares a thin `[[bin]]` over `fake_cli::main` or puts the test in testkit; guessing `target/<profile>/` is a legacy fallback that depends on Cargo's `build-dir` (assert_cmd `src/cargo.rs:275-296`).

## 3. Structure Options

| Fork | Option | Precedent | Verdict |
|---|---|---|---|
| **Spawn primitive** | A. portable-pty `spawn_command` | spike `session.rs`; prior-art row 1d | Rejected. `close_random_fds()` in `pre_exec` (`unix.rs:118-143`, `:239`) also closes std's exec-error pipe, so an exec failure after fork (a missing shebang interpreter) is reported as a started child; the sweep allocates and reads a directory after fork in a multithreaded process; `as_command` runs the child in `$HOME` when `cwd` is not a directory and injects `SHELL` (`cmdbuilder.rs:452-458`, `:479-480`). |
| | B. `std::process::Command` with a `pre_exec` doing `setsid` + `TIOCSCTTY`, no sweep | std keeps the exec-error pipe (`sys/process/unix/unix.rs:138-150`) | Rejected. It forks the daemon; the child inherits every fd another thread created without `FD_CLOEXEC` at that instant (measured: fd 50 inherited, F2), and std's own pipes get `FD_CLOEXEC` only after creation on macOS (`sys/pipe/unix.rs:33-39`); closing that window needs a process-wide gate over every fd creation. `pre_exec` is new `unsafe` too. |
| | C. Darwin `posix_spawn` with `SETSID`, `CLOEXEC_DEFAULT`, `SETSIGDEF`, `SETSIGMASK`, slave opened by path, `addchdir_np` | measured F1–F5 | **Adopted.** No fork in the daemon; nothing inherited but fds 0–2, by the kernel; the controlling terminal from the open after `setsid`; exec, cwd, and interpreter failures as the return value before the program runs; exact `envp`. Cost: the `sys` module wraps the attr and file-action calls (§2.7), and `addchdir_np` is deprecated in the macOS 26 SDK in favour of `posix_spawn_file_actions_addchdir`, which older macOS lacks. |
| **PTY opening** | A. portable-pty `openpty` | `unix.rs:18-61` | Rejected: `openpty` creates both fds without `FD_CLOEXEC` and sets it afterwards, so a concurrent spawn elsewhere in the daemon can inherit the slave and hold EOF; it would be portable-pty's only remaining use. |
| | B. rustix `openpt` with `O_CLOEXEC`, `grantpt`, `unlockpt`, `ptsname`; slave opened with `O_CLOEXEC` | measured F6 | **Adopted.** Every PTY fd is close-on-exec from creation; all safe calls. |
| **`unsafe` placement** | A. Inline where used | spike `os/unix.rs` (18 blocks) | Rejected: every module an audit site. |
| | B. Workspace `deny`, each other crate adds `#![forbid(unsafe_code)]` | — | Rejected: contradicts PR2 D7 and mvp §3; a new crate that forgets the attribute allows `unsafe`. |
| | C. Crate-local `deny`, one `#[allow]` on `unix::sys` exposing safe functions | PR2 D7 | **Adopted**, with a test for the restated table. |
| **Syscall wrappers** | A. `libc` only | spike | Rejected: every signal, wait, and poll would be `unsafe`. |
| | B. `nix` | portable-pty's `nix 0.25.1` | Rejected: an old release line and a second wrapper next to the rustix already in the lock. |
| | C. `rustix 1.1.6`, `libc` only for `posix_spawn` and libproc | `alacritty_terminal` uses rustix on Unix | **Adopted.** |
| **Master mode** | A. Blocking master, poll before read | — | Rejected: data flushed between `poll` and `read` leaves the read blocked with no way to cancel it (F12), and a write blocked on a full queue cannot be cancelled. |
| | B. Nonblocking master (shared by all duplicates, F11); the writer loops with `poll(POLLOUT)` to keep whole writes; both wait on the wake | herdr's Unix actor polls a nonblocking master (`src/pty/actor/unix.rs:391`, read for PR2 §3) | **Adopted.** Reads and writes are cancellable by close; PR 2's reader and writer threads are unchanged. |
| **Exit observation** | A. `try_wait` (reaps) | spike; std | Rejected: once reaped, the leader's ids can be reused under a later `killpg`. |
| | B. `waitid(WNOWAIT)` polling, reaping in `Drop` or by the reaper | measured F7, F8 | **Adopted**, with exclusive ownership as a stated precondition and detected loss. |
| **Process table** | A. `proc_listallpids` + `proc_pidinfo` (`arg` 1) + `getsid` | spike `os/unix.rs:25-124` | **Adopted**, with zombie, gone, and unreadable kept apart (the spike dropped all three) and saturation retried. |
| | B. `sysctl(KERN_PROC_ALL)` | `ps` | Rejected: libc has no `kinfo_proc`; a hand-written layout is a liability, and `arg` 1 already reads zombies (F9). |
| **Kill sequence** | A. HUP and TERM together, 2 s, KILL, then recorded-pid fallback | spike `os/unix.rs:200-223`, `session.rs:347-377` | Partly adopted: the per-pid recheck and the snapshot-first order. |
| | B. HUP → TERM → KILL on the `getsid` set, 250 ms each | herdr (prior-art row 2) | Partly adopted: separate HUP and TERM waits, the session set. |
| | C. Pass first, then HUP, TERM, KILL to the group and the targets with budget-capped waits, targets kept across calls, proof by a clean pass | — | **Adopted.** |
| **FakeCli reporting** | A. HTTP `/witness` | spike `fake_cli.rs:113-139` | Rejected: an HTTP receiver in every test; the daemon's receiver is PR 9. |
| | B. Report files | — | Rejected: no death signal. |
| | C. One loopback TCP stream per fixture: EOF = death, `ping`/`pong` = alive | — | **Adopted.** |
| **FakeCli binary location** | A. Guess `target/<profile>/` | assert_cmd legacy fallback (`src/cargo.rs:275-296`) | Rejected: breaks with a separate `build-dir`; stale with `-p`. |
| | B. A `[[bin]]` in pty-runtime | — | Rejected for PR 3: a test-only binary in a library crate. |
| | C. Real-PTY FakeCli tests as testkit integration tests | Cargo's `CARGO_BIN_EXE_*` | **Adopted.** |
| **FakeCli without `unsafe`** | A. `libc` termios and `signal` | spike `os/unix.rs:267-303` | Rejected: testkit keeps `forbid`. |
| | B. rustix termios, `signal_hook::flag`, `setsid` by the sleeper itself, `process_group(0)` | — | **Adopted.** |
| **Platform gate** | A. `cfg(unix)` with a Linux `/proc` table | — | Rejected: Linux is not a host (AGENTS.md) and no CI runs it. |
| | B. `cfg(target_os = "macos")` | spike `os/unix.rs:5-6` | **Adopted.** Linux builds `core`, `FakePty`, and testkit only. |

## 4. Same-Class Inventory

Every MVP place that kills processes, reads pids, or spawns children:

| Place | PR | How this design serves it or stays out |
|---|---|---|
| `Session::kill`, `Session::close` step 2, `SpawnedPty` teardown | 2 | `UnixChild::kill_tree`. |
| `chaldea stop`, `close --stop`, `resume` death check | 10 | The supervisor calls `Session::close(extra, ..)`; `KillReport::method` becomes `launch_death_confirmed.method`; `tree_dead: false` keeps `death_confirmed: false`. |
| Recorded descendants (`extra`) from hook emitters | 9, 10 | The identity format of §2.1; the public lookup arrives with its first consumer. The ancestry walk is PR 9's design. |
| Daemon restart: old tree check by recorded sid and start window | 10 | Stays out. The new daemon does not own the old leader, so it may signal only verified identities: exactly the ownership-lost mode of §2.5. `unix::kill` takes its `Os` and leader as parameters so PR 10 can add an entry point for an unowned leader without changing the engine. |
| Daemon process rules | 8, 10 | The ownership precondition of §2.4; loss is detected (`ECHILD` → `WaitFailed`, ownership-lost kill). |
| Daemon spawns of git and one-shot hooks | 6, 16, 20 | Not PTY children. Their waits must be by exact pid (§2.4). They cannot inherit a PTY fd (all created close-on-exec, §2.2); a leaked wake pipe is harmless (§2.3). |
| Worktree removal in-use check | 16 | Stays out: it reads launch state, never processes. |
| Rescue | 20 | Stays out: it acts through `surface_action stop`, the stop path above. |
| Doctor: FakeCli tree kill | 21 | `native()` + `Session` + FakeCli; how the shipped binary gets FakeCli is PR 21's choice. |
| Windows tree kill (job + recorded descendants) | 4 | Shares the `extra` rule, `KillReport`, and testkit; nothing OS-specific. PR 4 adds the `breakaway` role and Windows console modes to FakeCli. |
| FakeCli's own children | 3 | testkit only; every sleeper takes `--max-life-secs`; tests hold a `Cleanup`. |
| Spike watchdog and emergency kill | — | Not lifted; the supervisor (PR 10) decides. |

## 5. Resource Paths

| Resource | Created at | Owner | Bound | Released on |
|---|---|---|---|---|
| Master fd (original) | stage 1 | spawn | 1 | end of stage 2 |
| Parent's slave fd | stage 2 | spawn | 1 | stage 4 failure release, or stage 5 |
| Master duplicate R | stage 2 | `MasterReader` (drain thread) | 1 fd | drain end: EOF, read error, or wake |
| Master duplicate W | stage 2 | `MasterWriter` (session input) | 1 fd | input dropped; writes nothing |
| Master duplicate C | stage 2 | `UnixControl` | 1 fd | `close` or drop |
| Wake pipe | stage 2 | read end shared by reader and writer (`Arc`), write end by control | 2 fds | the last of reader, writer, control dropped |
| `pty-drain` thread | stage 3 | `Drain` | 1 | EOF, read error, or wake (≤ 1 MiB after it) |
| Transient failed-spawn child | stage 4 failure | the kernel | 1 | exits by itself (F4) |
| Leader process | stage 4 | the CLI; `UnixChild` | 1 | exit or `kill_tree` |
| Leader zombie | leader exit | `UnixChild`, then the reaper | 1 | `Drop` (`waitpid`), or the reaper once it exits |
| `pty-reaper` thread | first handoff | process | 1 per process | process exit |
| Kill targets | `kill_tree` | `KillState` | the tree's size | the `UnixChild` |
| Pass buffers | each pass | call | O(processes) | return |

**Endings.**

| Ending | What happens |
|---|---|
| Validation failure | `NeverStarted`, nothing created. |
| Failure at stage 1–3 | Created fds dropped (the reader first when the drain cannot start); `NeverStarted { Setup }`, `Confirmed`. |
| Failure at stage 4 | The program never ran. Slave and writer dropped, control closed (wake), drain finished within 2 s; `NeverStarted`, `Confirmed` or `drain_abandoned`. |
| Failure at stage 5 | `kill_tree(&[], 5 s)` on the owned leader, writer dropped, control closed, drain finished within 2 s; `StartedThenFailed` with the cleanup result. |
| Child exit | `wait_timeout` reports it, repeatably; the leader stays a zombie; the drain reads on until the last slave holder closes (EOF). |
| `kill` | §2.5 within the deadline; I/O stays up. |
| `close` | PR2 §2.2 order: kill; input released (a write blocked in step 3 ends at step 4's wake); control closed (wake; never blocks); drain finished (EOF, else the wake ends it); threads joined. |
| `Session` drop | As close; the last `Arc<UnixChild>` reaps or hands off the leader. |
| Reader EOF / `EIO` | `OutputEnd::Eof`. |
| Wake without EOF | `OutputEnd::Error("pty closed before eof")` after at most 1 MiB. |
| Write error | Returned to the writer (unit `Incomplete`, `InputBroken`, PR2 §2.3). |
| Ownership lost | `WaitFailed`; numeric group and session targeting disabled; `tree_dead: false` from then on; the backend never reaps the pid. |
| Panic | No panic path in this backend's code; a poisoned `kill` mutex is taken over. |

**Waits.** Every wait has a deadline: `wait_timeout` (its timeout, 10 ms steps, `EINTR` included); `kill_tree` (lock wait, phases, passes, and sleeps all end at `deadline`, checked before every syscall batch; one in-flight syscall and scheduler latency can run past it); `Drain::finish` (the session's bound); `close`, `resize`, and `posix_spawn` (never wait on another process: the call returns once the exec succeeded or failed); `UnixChild::drop` (`NOHANG`); spawn's failure teardown (5 s + 2 s). The reader's `poll` and the writer's `poll(POLLOUT)` have no timeout of their own; the wake ends both.

## 6. Purpose Layer and Plan Layer

### 6.1 Purpose Layer: Acceptance Criteria

Checked on a real macOS host (and `macos-latest` CI) by `cargo test -p chaldea-testkit --test pty_unix` and `cargo test -p chaldea-pty-runtime`; a manual run can watch `ps -axo pid,ppid,pgid,tpgid,stat,command | grep chaldea-fake-cli` before and after a kill.

| ID | Criterion (EARS) | Source | How to observe on a real macOS host |
|---|---|---|---|
| PR3-P-1 | When a CLI tree is killed, no process in the CLI's session, no live-observed descendant of a covered process, and no verified `extra` identity shall be alive at the deadline, `tree_dead` shall be true, and an unrelated process shall still answer; a `setsid` escapee whose parent exited before any observation is outside this guarantee unless named in `extra`. | mvp §4 PR 3 obligation; spike B-7, §4.6 | FakeCli `spawn-tree`: every role's stream closes; `unrelated` answers `ping` after the kill; the orphan survives without `extra` and dies with it. |
| PR3-P-2 | The backend shall signal by process group or session only while it owns the leader's wait status, and by pid only after a lookup showed the recorded start; the remaining window between that lookup and the signal is stated, not closed. | spike §4.6; §2.5 | A stale `extra` (the unrelated pid with another start) leaves it alive while a matching `extra` in the same call dies; after the leader is reaped outside the backend, the report is `unix_ownership_lost`, `tree_dead: false`. |
| PR3-P-3 | When a spawn fails, the error shall say whether the program could have run, no fd of the spawn shall remain open, no child of it shall remain once the transient one exits, and a cleanup that could not be proven shall be reported `Unresolved`. | PR2 §2.1 | Missing program, missing interpreter, missing cwd, bad format: `NeverStarted`; fd set unchanged; `pgrep -P` empty within 1 s; a stage 5 failure with no kill budget reports `Unresolved`. |
| PR3-P-4 | When a CLI is spawned, it shall run as a session leader with the PTY as controlling terminal on fds 0–2 and no other fd, at the requested size, cwd, and exact environment, receive written bytes intact, and receive no byte when the input is dropped. | PR2 §2.1 | FakeCli hello; `/usr/bin/env`, `/bin/pwd`, `/bin/stty size` output; `echo` reports. |
| PR3-P-5 | When the tree has exited or the PTY is closed, the drain shall end (EOF, or a reported error when a holder survives), a write blocked on a CLI that stopped reading shall end at close, and `close` shall return within its budgets. | PR2 §2.2, §2.4 | `CloseOutcome`: `output`, `released.input`, `abandoned` empty, `elapsed`. |
| PR3-P-6 | Every `unsafe` block shall be in `pty_runtime::unix::sys`, and every other crate shall keep `forbid(unsafe_code)`. | PR2 D7; mvp §3 | `tests/lints.rs`; clippy. |
| PR3-P-7 | The workspace shall build and pass on `windows-latest` with the native-backend tests removed by `cfg`. | mvp §4 | The `workspace (windows-latest)` job. |

### 6.2 Plan Layer: Claims

| ID | Claim (EARS) | Source | Falsifying test |
|---|---|---|---|
| PR3-CL-1 | If validation fails, then `spawn` shall return `NeverStarted` with the kind of §2.2's table and create nothing. | PR2 §2.1; §2.2 | `tests/unix_spawn.rs`: `missing_program_is_not_found`, `unsearchable_directory_is_permission_denied` (a temp dir with mode 000), `nul_in_argv_is_setup`, `unexecutable_file_is_permission_denied`, `relative_argv0_is_setup`, `missing_cwd_is_setup`, `bad_env_is_setup` (duplicate key, `=` in key, NUL) |
| PR3-CL-2 | If `posix_spawn` fails, then `spawn` shall return `NeverStarted` (the program never ran), leave the caller's fd set unchanged, and leave no child once the transient one exits; it shall never report a start for an exec that failed after validation. | PR2 §2.1; F3, F4 | `tests/unix_spawn.rs`: `missing_interpreter_is_never_started` (an executable `#!/nonexistent/interp` script), `bad_format_is_never_started` (`ENOEXEC`, no `/bin/sh` fallback), `failed_spawns_leave_no_fd` (`/dev/fd` before and after), `failed_spawns_leave_no_child` (`/usr/bin/pgrep -P <test pid>` empty within 1 s); unit `unix::tests::cwd_removed_after_validation_fails_before_exec` (a stage hook removes the dir between validation and stage 4: `NeverStarted`, the program's marker file absent) |
| PR3-CL-3 | If a stage after the PTY is opened fails, then `spawn` shall release in the order kill, input, control, drain, return `StartedThenFailed` once a process exists, and report `Confirmed` only when the tree is proven dead and the drain ended. | PR2 §2.1 | unit `unix::tests::drain_start_failure_releases_everything`, `identity_failure_kills_the_started_tree` (`/bin/sh -c 'sleep 30 & echo $!; exec sleep 30'`; the stage 5 hook waits until the drain has seen the `$!` line and both pids are live, then fails: both pids dead, `Confirmed`), `zero_kill_budget_reports_unresolved` (same, with the teardown's kill budget forced to 0: `Unresolved` with a `kill` report) |
| PR3-CL-4 | When `spawn` returns, the PTY shall have `spec.size` with 8×16 px cells from the first byte, and a resize shall reach the CLI with the same cell size. | PR2 §2.1 | unit `unix::tests::pty_opens_at_spec_size` (`/bin/stty size`); testkit `pty_unix::size_reaches_the_cli` (`attachable`: the first `size` event has `xpixel = 8 × cols`, `ypixel = 16 × rows`; after `resize` the next event matches the new size) |
| PR3-CL-5 | The child's environment shall be exactly `spec.env`, with nothing added or inherited, `SHELL` included. | PR2 §2.1; mvp §2.7 | `tests/unix_spawn.rs::env_is_exactly_the_spec` (`/usr/bin/env` with `A=1` prints only `A=1`; with an empty env prints nothing; the test process's `PATH` and `CARGO_PKG_NAME` absent; no `SHELL`) |
| PR3-CL-6 | When `spawn` succeeds, the child shall run `argv[0]` in `spec.cwd` as a session and group leader whose controlling terminal is the PTY, on fds 0–2, with no other fd and every signal at its default action. | PR2 §2.1; F1, F2, F5 | `unix::tests::cwd_is_honored` (`/bin/pwd` against the canonicalized temp dir); testkit `pty_unix::child_leads_its_session_on_the_pty` (hello: `pid == pgid == sid`, `ttys` three times the slave, `ctty`, `fg`, `fds == [0,1,2]`); `tests/unix_spawn.rs::fd_without_cloexec_is_not_inherited` (the test opens a file without `O_CLOEXEC` and spawns `/bin/sh -c 'test -e /dev/fd/<n> && echo LEAK; echo DONE'`: the output has `DONE` and no `LEAK`), `sigpipe_is_default_in_the_child` (`/bin/sh -c 'kill -PIPE $$; echo alive'` exits by signal 13 without `alive`, while the test process ignores `SIGPIPE` as Rust binaries do) |
| PR3-CL-7 | When the last process holding the slave exits, the drain shall end with `OutputEnd::Eof`; a read `EIO` shall map to EOF. | PR2 §2.1 | unit `unix::tests::exit_ends_output_with_eof` (`/bin/echo READY`: the sink gets `READY` first, then `Eof`, before `close`), `unix::io::tests::map_read_turns_eio_into_eof` (pure helper with a synthetic `EIO`; other errnos pass through) |
| PR3-CL-8 | When the wake fires while another process holds the slave, the reader shall return what is readable at once, at most 1 MiB, then end with an error that is not EOF; a flush between `poll` and `read` shall not block it. | §2.3 | `unix::io::tests::wake_ends_the_reader_with_a_slave_open`, `wake_caps_a_continuous_writer` (a thread writes to the slave without pause: the reader ends within 1 MiB + one read after the wake), `wake_when_master_and_wake_are_both_ready_arms_the_cap`, `flush_between_poll_and_read_does_not_block` (F12 sequence, then the wake ends it); testkit `pty_unix::close_with_a_surviving_tty_holder_is_bounded` (`--escape-orphan --orphan-keeps-tty`, `close(&[])`: `output == Some(Error)`, `abandoned` empty; the orphan then killed through `extra`) |
| PR3-CL-9 | When the session input is dropped, no byte shall reach the slave. | PR2 §2.1 | `unix::io::tests::dropping_input_writes_nothing` (positive control: one byte written through the writer is read on the slave, then the drop delivers nothing) |
| PR3-CL-10 | A write shall return the whole length or fail, except a write ended by the wake after progress, which returns the bytes written; a write blocked on a full queue shall end at the wake. | PR2 §2.1, §2.3 | testkit `pty_unix::large_write_reaches_the_cli_whole` (one 256 KiB `Bytes` part with CJK and emoji to `echo` after its hello: reported bytes equal, `bytes_written` 262 144, `split_writes == 0`); `unix::io::tests::blocked_write_ends_at_the_wake` (nobody reads the slave: the write returns `Ok(n < len)` or `BrokenPipe` within 1 s of the wake); testkit `pty_unix::close_releases_a_blocked_writer` (a `sleeper` on the PTY never reads; a 4 MiB send blocks; after `close`, `released.input` is true and `abandoned` empty) |
| PR3-CL-11 | `wait_timeout` shall return by its timeout and shall not wait for a running `kill_tree`. | PR2 §2.1 | `unix::tests::wait_timeout_returns_by_its_timeout`, `wait_timeout_runs_during_kill` (`sh` trapping HUP and TERM; a kill-entry hook signals that `kill_tree` holds its lock; then `wait_timeout(10 ms)` from another thread returns within 200 ms) |
| PR3-CL-12 | When the leader exits, `wait_timeout` shall report its code or signal on every call and leave it unreaped until the child is dropped, which reaps it, or hands it to the reaper when it is still alive. | §2.4 | `unix::tests::exit_reports_code_and_signal` (two calls, same value), `exit_is_observed_without_reaping` (before drop `lookup` is `Zombie`; after drop `Gone`), `live_leader_is_reaped_after_drop` (a `sh` trapping HUP and TERM, killed with an expired deadline, so it is alive at drop; the test then sends KILL to its pid, still unreaped and so not reusable; `lookup` is `Gone` within 2 s) |
| PR3-CL-13 | When `kill_tree` runs while owned, it shall send HUP, TERM, KILL in that order to the group and the live targets, kill session members outside the group, live descendants of members and targets (escapees included, across calls), and report `tree_dead: true` only from a clean pass with no live member or target. | mvp §4 PR 3; prior-art row 2 | testkit `pty_unix::spawn_tree_dies_and_unrelated_survives` (roles `plain`, `ignore-term`, `double-forked`, `own-group`, `setsid`, `setsid-child`; exactly these roles with the expected pgid, sid, and ppid; each stream closes; `unrelated` answers `ping` afterwards); unit `unix::kill::tests::signals_go_in_order` (exact `Os` call sequence), `session_member_outside_the_group_is_killed`, `escapee_child_forked_after_its_parent_died_is_killed`, `targets_survive_across_calls`, `clock_step_does_not_hide_a_child` (a child whose start stamp is before its parent's is still walked) |
| PR3-CL-14 | A pass shall be clean only if the list was complete and unsaturated and no listed pid vanished before its lookup; only a clean pass shall prove death. | §2.5 | unit `unix::kill::tests::saturated_list_is_retried_then_not_proof`, `vanished_pid_makes_the_pass_unclean` (A listed, gone at lookup, its child B absent from the list), `deadline_cut_pass_is_not_proof` |
| PR3-CL-15 | When an `extra` identity is live with its recorded start, `kill_tree` shall kill it and its descendants; a stale one shall not be signalled. | PR2 §2.1 | testkit `pty_unix::extra_reaches_an_orphan_and_spares_a_stale_identity` (one call with the orphan's matching identity and the unrelated process's pid with start + 1: the orphan and `orphan-child` die, the unrelated process answers `ping`; without `extra` the orphan survives first) |
| PR3-CL-16 | The backend shall send a group signal only after a fresh ownership check, never after loss; a per-pid signal shall follow a lookup of that pid showing the recorded start, with no other `Os` call between them, never to pid 0, 1, or its own; after loss `tree_dead` shall be false. | §2.4, §2.5 | unit `unix::kill::tests::ownership_is_checked_before_each_killpg`, `lost_ownership_disables_numeric_targets` (`ECHILD`; a live member with `sid == L` and a replacement process with pid `L` and a new start: neither signalled, `tree_dead: false`), `reused_pid_in_extra_is_not_signalled`, `lookup_immediately_precedes_kill`, `init_and_self_are_never_signalled`; `tests/unix_spawn.rs::reaping_outside_the_backend_is_ownership_loss` (the test reaps the exited leader by its exact pid: `WaitFailed`, then `kill` reports `unix_ownership_lost`, `tree_dead: false`) |
| PR3-CL-17 | `kill_tree` shall return by its deadline plus one in-flight syscall, do nothing when called past it, and list survivors with `tree_dead: false` when it cannot prove death, unreadable members and targets included. | PR2 §2.1 | unit (synthetic clock) `kill_returns_by_its_deadline_with_survivors` (signals have no effect), `expired_call_does_no_pass`, `lookup_consuming_the_budget_ends_the_call`, `sleeps_are_capped_to_the_budget`, `unreadable_target_is_an_unresolved_survivor` (an `extra` outside the session turning `EPERM`), `eperm_group_probe_is_not_empty`, `small_budget_sends_kill_only`, `busy_lock_returns_by_the_deadline` |
| PR3-CL-18 | A second `kill_tree` on a proven tree with no new `extra` shall return `tree_dead: true` without a signal or a grace wait. | PR2 §2.1 idempotent | unit `unix::kill::tests::second_call_on_a_proven_tree_signals_nothing` (no `kill`/`killpg` call, no `sleep`) |
| PR3-CL-19 | The leader's identity shall be read even when it has already exited, and equal a later lookup while it lived. | §2.2 stage 5; F9 | unit `unix::tests::immediate_exit_still_has_an_identity` (`/usr/bin/true` with a hook that waits for its exit before stage 5: `Ok`, then `Exited(0)`), `leader_identity_matches_lookup`; `unix::sys::tests::bsd_info_sees_zombies_and_reports_eperm` (pid 1 → `EPERM`; an exited unreaped child → `Zombie`) |
| PR3-CL-20 | `#[allow(unsafe_code)]` shall occur exactly once in `crates/pty-runtime/src`, on `unix::sys`, with no `#![allow(unsafe_code)]` or `expect(unsafe_code)` anywhere; the crate's clippy table shall contain every workspace key with the same value, and only the listed local additions; every unsafe block shall carry a `SAFETY` comment. | PR2 D7 | `tests/lints.rs::unsafe_is_allowed_only_in_sys`, `inherited_lints_match_the_workspace`, `local_lints_are_the_listed_ones`; clippy `undocumented_unsafe_blocks = deny` |
| PR3-CL-21 | FakeCli shall start every PR 3 mode, report a hello with its pid, end a sleeper by `--max-life-secs`, report each spawned role once its mechanism is in place, and exit 2 on an unknown mode or flag. | mvp §3 lift table | `fake_cli::every_mode_reports_hello`, `spawn_tree_reports_each_role`, `sleeper_ends_after_max_life`, `unknown_mode_exits_2`, `unknown_flag_exits_2`, `queries_in_pipe_mode_reports_each_unanswered_query` (stdin a pipe closed at once: seven `ok: false` events within 10 s) |
| PR3-CL-22 | The VT responder's replies shall reach a CLI through the real PTY. | PR2 §2.5 | testkit `pty_unix::queries_are_answered_through_the_pty` (each of the seven query events `ok: true`, `all_ok`) |
| PR3-CL-23 | `chaldea-testkit` shall have no normal dependency on a workspace crate other than `chaldea-core`. | mvp §3 | CI step (§7) |

**Obligation mapping** (PR2 §2.1 → claims): release on failure, `Unresolved` → CL-1, CL-2, CL-3; opened at `spec.size` → CL-4; environment → CL-5; drain from the first byte (PR 2's `Drain::start` at stage 3), `EIO` → EOF, parent slave dropped → CL-7, CL-8; dropping input writes nothing → CL-9; whole writes → CL-10; `wait_timeout` → CL-11, CL-12; `kill_tree` with `extra`, deadline, survivors, idempotence → CL-13 to CL-18; `close` by deadline → CL-8, CL-10 (`close` never blocks); `evidence` → none on Unix (§2.2); `unsafe` placement → CL-20. PR 3 row obligation (survivors and negative control) → CL-13, CL-15, CL-16.

## 7. Tests and Completion Commands

**Where tests run.**

| Tests | File | OS | Processes |
|---|---|---|---|
| Kill engine | `src/unix/kill.rs` `#[cfg(test)]` | macOS | none: synthetic `Os` and clock |
| Reader, writer, wake, `map_read`, `sys` | `src/unix/io.rs`, `src/unix/sys.rs` `#[cfg(test)]` | macOS | real PTY; `sys` tests one short-lived child |
| Spawn stages, exit, kill with system binaries and stage hooks | `src/unix/tests.rs` | macOS | real |
| Public-API spawn: failures, fd and child accounting, env, ownership loss | `tests/unix_spawn.rs` | macOS | real; every test holds the file's lock |
| Lints | `tests/lints.rs` | all | none |
| PR 2's `FakePty` tests | unchanged | all | none |
| FakeCli modes without a PTY | `crates/testkit/tests/fake_cli.rs` | all (Unix-only roles behind `cfg(unix)`) | FakeCli |
| Native backend + FakeCli | `crates/testkit/tests/pty_unix.rs` | macOS | FakeCli; every test holds the file's lock |

**Isolation.** Tests that count fds or children, or reap a child, are in `tests/unix_spawn.rs`, a separate binary in which every test takes the same `static LOCK: Mutex<()>`, so no other thread of that process opens fds or spawns children meanwhile (the library's unit-test binary also runs `drain.rs` tests that open pipes on other threads). No test changes the environment or waits for any child but its own by exact pid. The real-process unit tests in `src/unix/` share one lock among themselves and never count fds.

**Timing.** No test synchronizes by sleeping. A test waits for a FakeCli hello or event (sent only once the fixture's mechanism is in place), for `SessionEvent::Exited`, for a sink sentinel, for a stage-hook signal, or for a stream's EOF, each bounded by 10 s. Kill deadlines are 5 s; "returns by its deadline" allows 500 ms of slack on real processes and none on the synthetic clock. Survival is proven by `ping` after `kill_tree` returned. Stage hooks fire only after their milestone (both pids live and the `$!` line seen; the leader's exit observed), so a fault never lands before the state it tests.

**Leaks.** Every sleeper runs with `--max-life-secs 60`; every testkit test holds a `Cleanup` that kills its fixtures by verified identity and its own `unrelated` child on any exit, an assertion failure included; dropping the `Session` closes it and kills the leader.

**Runner PTYs.** `posix_openpt` needs no controlling terminal. A runner without PTY access fails these tests; none of them skips.

**Completion commands** (PR2 §7, extended):

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
deps="$(cargo tree -p chaldea-core -e normal --prefix none)" && ! grep -Eq '^(tokio|rusqlite|axum) ' <<<"$deps"
deps="$(cargo tree -p chaldea-pty-runtime -e normal --prefix none)" && ! grep -Eq '^tokio ' <<<"$deps"
deps="$(cargo tree -p chaldea-testkit -e normal --prefix none)" && bad="$(grep -E '^chaldea-' <<<"$deps" | grep -Ev '^chaldea-(testkit|core) ' || true)" && [ -z "$bad" ]
```

plus the `workspace` job green on both runners with the new step, and the `pty-hooks` job unchanged.

## 8. Decisions

**D1. The native Unix backend is macOS-only** (`cfg(target_os = "macos")`). It uses Darwin `posix_spawn` flags and libproc, the spike was macOS-only, and no CI runs Linux. Linux builds `core`, `FakePty`, and testkit. Reopen when Linux becomes a host: a `/proc` table behind the same `Os` trait and a Linux spawn path.

**D2. The backend owns the spawn and the PTY; portable-pty is not used.** `posix_spawn` with `SETSID` and `CLOEXEC_DEFAULT` gives a session leader with the PTY as controlling terminal, only fds 0–2, exact `envp`, and `cwd`, exec, and interpreter failures as a return value, without forking the daemon (F1–F5). rustix opens the PTY with every fd close-on-exec from creation (F6). portable-pty's spawn path reports some exec failures as starts, runs the child in `$HOME` on a bad `cwd`, adds `SHELL`, and sweeps fds after fork; its `openpty` leaves a CLOEXEC window. The environment is exactly `spec.env`.

**D3. Exclusive ownership of the leader's wait status is a precondition; its loss is detected and narrows the kill.** While owned, the unreaped leader makes group and session targets exact; ownership is re-checked before every numeric target. After loss only verified identities are signalled and death is never reported proven. Reaping is the backend's: in `Drop`, or by the reaper for a leader that outlived its kill.

**D4. The kill keeps its targets across calls, escalates HUP → TERM → KILL with budget-capped waits, and accepts death only from a clean pass.** Its guarantee is bounded to the original session, observed descendants, and verified `extra`; the residual windows of §2.5 are stated.

**D5. The master is nonblocking; reads and writes are cancelled by a level-triggered wake.** The seam keeps whole-write semantics; close ends a blocked read or write; output after the wake is best effort, at most 1 MiB.

**D6. `unsafe` lives in one module, `unix::sys`, under a crate-local `deny`**, with `undocumented_unsafe_blocks` and `multiple_unsafe_ops_per_block` denied and the restated clippy table checked by a test.

**D7. PR 3 adds only `native()`.** Process identity lookup stays internal, reachable by tests through the `fake` feature; the PR that first builds `extra` in the daemon adds the public lookup.

**D8. FakeCli is library code with a thin binary, reports over loopback TCP with `ping`/`pong`, and lifts only the modes PR 3 tests use.** Its real-PTY tests are testkit integration tests, so Cargo locates the binary. `stall`, `banner`, hooks, the `mcp` role, attach input parsing, and Windows console modes come with the PR that first tests them.

**D9. Validation classifies early; `posix_spawn` decides.** The checks of §2.2 give typed errors; any failure the kernel finds later is still `NeverStarted`, because the program cannot have run when `posix_spawn` returns nonzero.

### Open Questions

None that change a public contract or are irreversible. The process obligations of D3 and the dependency change of D2 need the edits listed below in documents this design does not own.

## Related Text in Other Documents

These change when this design is adopted (not edited here). The evidence for the portable-pty change is §3 "Spawn primitive" and "PTY opening" with facts F1–F6.

- `AGENTS.md` invariant 3, PTY Seam: "using `portable-pty =0.8.1` on macOS/Unix and an own `windows-sys` ConPTY backend on Windows" → "using an own macOS backend (rustix PTY, Darwin `posix_spawn` with `POSIX_SPAWN_SETSID` and `POSIX_SPAWN_CLOEXEC_DEFAULT`) and an own `windows-sys` ConPTY backend on Windows".
- `docs/architecture.md` line 33 (Host Core & Daemon): "`portable-pty =0.8.1` (macOS/Unix)" → "an own macOS PTY backend (rustix + `posix_spawn`)"; line 61 (`portable-pty` entry): its last sentence "portable-pty stays on macOS/Unix." → "macOS uses an own backend as well: portable-pty's Unix spawn path closes std's exec-error pipe in `pre_exec`, falls back to `$HOME` for a missing cwd, injects `SHELL`, and its `openpty` leaves a CLOEXEC window (`docs/design/pty-unix.md` §3)."
- `docs/prior-art.md` row 1d: "**Keep portable-pty `=0.8.1` on macOS** (architecture decision)." → "macOS uses an own backend (rustix PTY + Darwin `posix_spawn`, `docs/design/pty-unix.md` §3); portable-pty is not used."
- `docs/mvp-design.md` §3 layout line 348: "unix (portable-pty =0.8.1)" → "unix (rustix PTY + `posix_spawn`)"; §4 PR 3 row: "Unix backend (portable-pty `=0.8.1`, …)" → "macOS backend (rustix PTY, `posix_spawn`, …)".
- `docs/mvp-design.md` §3 lift table, `src/fake_cli.rs` row: PR 3 lifts `echo` (input reporting), `queries`, `attachable` (size reporting), `spawn-tree`, `sleeper`; `stall` and `banner` move to PR 10; `/witness` is replaced by the report stream.
- `docs/mvp-design.md` §4 PR 8 and PR 10 obligations: the daemon process has exclusive ownership of each PTY child's wait status: no wildcard waits (`waitpid(-1)`, `wait`, `waitid(P_ALL)`), no `SIGCHLD` ignore or `SA_NOCLDWAIT`, waits for its other children by exact pid; on loss (`ECHILD`) the backend reports `WaitFailed` and the ownership-lost kill, and the supervisor keeps `death_confirmed: false`.
- `docs/design/pty-runtime.md` §1: "PR 3 adds `src/unix.rs` (`#[cfg(unix)]`) and PR 4 adds `src/windows.rs` (`#[cfg(windows)]`) plus a `native()` constructor" → "PR 3 adds `src/unix/` (`#[cfg(target_os = "macos")]`) and `native()`; PR 4 adds `src/windows/` (`#[cfg(windows)]`) and the Windows arm of `native()`"; the sources table's portable-pty row stays as read evidence.
- `docs/design/pty-runtime.md` §2 threading model: "PTY ends are blocking on both OSes (portable-pty's reader and writer are blocking `Read`/`Write`, …)" → the macOS master is nonblocking behind a reader and writer that block on `poll` and are cancelled by close.
- `docs/design/pty-runtime.md` §2.1: "(portable-pty: `env_clear` then `env`; …)" → "(macOS: `posix_spawn` with exactly that `envp`; …)"; "`input` accepts each `write` call whole or fails: the Unix master fd is blocking" → "the macOS writer loops on a nonblocking master until every byte is accepted, and returns a short count only when close interrupts it"; "Unix `try_wait` polling" → "macOS `waitid(WNOWAIT)` polling; the backend owns reaping"; the "Dropping `input`" item keeps its rule, with portable-pty's VEOF writer as background; "Every `unsafe` block lives in `unix.rs` / `windows.rs`" and D7's "allows `unsafe` only in `unix.rs` / `windows.rs`" → `unix::sys` (and PR 4's Windows module).
