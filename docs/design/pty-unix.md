# Design: Unix PTY Backend and `crates/testkit` (MVP PR 3)

Status: implemented in PR 3. Scope is the PR 3 row of `docs/mvp-design.md` §4 (cited as "mvp §N"): the Unix backend of `PtyRuntime` (`killpg` HUP→TERM→KILL, `getsid` scan, recorded-pid fallback) and the new `crates/testkit` crate with `chaldea-fake-cli`. Its test obligation: FakeCli `spawn-tree` survivors and the unrelated negative control. The contract implemented is `docs/design/pty-runtime.md` (cited as "PR2 §N"), §2.1 "What each backend must supply"; no trait or public type of PR 2 changes.

The backend does not use portable-pty. It opens the PTY with rustix and starts the child with `std::process::Command` and a minimal `pre_exec` (§3 "Spawn primitive" gives the evidence; "Related Text in Other Documents" lists the documents that state this as well).

Out of scope: the Windows backend and its FakeCli additions (`docs/design/pty-windows.md`), daemon wiring and its restart scan (PR 10), input encoders (PR 5), the gate and send path (PR 10), attach (PR 11), and every FakeCli feature only those PRs use.

Sources read for this document (paths are relative to each root):

| Source | Revision | Read for |
|---|---|---|
| This repository | `b35d687` | `crates/pty-runtime/src/{backend,session,drain,writer,fake,lib}.rs`, `Cargo.toml`, `.github/workflows/ci.yml`, `lefthook.yml`; spike `spikes/pty-hooks/src/os/{unix,mod}.rs`, `src/session.rs`, `src/fake_cli.rs`; `docs/spikes/pty-hooks-spike.md` §4.5–§4.7; `docs/prior-art.md` rows 1d, 2 |
| Rust std | 1.96.0 `rust-src` | `library/std/src/sys/process/unix/unix.rs` (fork path, exec-error pipe, signal handling), `sys/pipe/unix.rs` (CLOEXEC on macOS) |
| `alacritty_terminal` | crate 0.26.0 (in `Cargo.lock`) | `src/tty/unix.rs:249-275` (its `pre_exec`) |
| `portable-pty` | crate 0.8.1 | `src/unix.rs`, `src/cmdbuilder.rs` (the defects of §3) |
| `rustix` | crate 1.1.6 (in `Cargo.lock` through `alacritty_terminal`) | `src/pty.rs`, `src/process/{wait,kill,id,rlimit}.rs`, `src/event/poll.rs`, `src/termios/{tc,tty}.rs`, `src/io/{fcntl,ioctl}.rs` |
| `libc` | crate 0.2.191 (in `Cargo.lock`) | `src/unix/bsd/apple/mod.rs`: `proc_listallpids`, `proc_pidinfo`, `PROC_PIDTBSDINFO`, `PROC_PIDLISTFDS`, `SZOMB`, `TIOCSCTTY`; no `getdtablesize` for Apple targets |
| `signal-hook` | crate 0.4.6 (in `Cargo.lock`) | `src/flag.rs` |
| `assert_cmd` | crate 2.2.2 | `src/cargo.rs` (how a test finds a binary) |

**Host facts measured for this design** on macOS 26.7.3 (arm64) with throwaway probes (C, Rust programs using the `Command` + `pre_exec` of §2.2, and for F19 an interposed `pipe`; not committed). The tests named in §6 check each fact this design relies on.

| # | Fact | Observed |
|---|---|---|
| F1 | `Command` with `pre_exec` doing `setsid()` then `ioctl(0, TIOCSCTTY, 0)`, stdio the slave: the child leads its own session with the slave as controlling terminal, and a resize reaches it as `SIGWINCH` | `pid = pgid = sid`, `/dev/tty` opens, `tcgetpgrp(0) = pid`, handler saw `SIGWINCH`. Without the ioctl: `/dev/tty` fails `ENXIO`, `tcgetpgrp(0) = -1`, no `SIGWINCH` |
| F2 | `posix_spawn` with `POSIX_SPAWN_SETSID` and the slave opened onto fd 0 by a file action gives a session leader **without** a controlling terminal (XNU applies file actions before `SETSID`) | direct child: `/dev/tty` `ENXIO`, `tcgetpgrp(0) = -1`. A `/bin/sh` child (bash 3.2) acquires the terminal itself, so a shell-based check reports a controlling terminal that the CLI would not have |
| F3 | `TIOCSWINSZ` on the master fails before any slave open and succeeds after | `ENOTTY`, then 0 |
| F4 | The `pre_exec` sweep (close every fd ≥ 3 without `FD_CLOEXEC`) and signal reset leave the child only fds 0–2 and default signal state | a non-CLOEXEC fd 50 opened by another thread, `SIGHUP` blocked in the spawning thread, `SIGINT` ignored: with the sweep and reset the child has fds `0,1,2`, nothing blocked or ignored; without them it has fd 50, `SIGHUP` blocked, `SIGINT` ignored |
| F5 | With the sweep in place, std's exec-error pipe still reports a failure as `Err` with the errno, and std has reaped the forked child before returning | missing program `ENOENT`, missing shebang interpreter `ENOENT`, missing cwd `ENOENT`, not executable `EACCES`; right after each: `proc_listchildpids` 0, `waitpid(-1, WNOHANG)` `ECHILD` |
| F6 | An executable without a known format or shebang is started, by `/bin/sh` (`execvp`'s `ENOEXEC` rule) | `Ok`, the shell's error on the PTY, exit 127 |
| F7 | Sweep cost is about 0.08 µs per fd slot | per spawn: +0.9 ms at a limit of 10 240, +5.5 ms at 65 536, +84 ms at 1 048 576; `getdtablesize()` = 245 760 (`kern.maxfilesperproc`) when the soft limit is 1 048 576 |
| F8 | `waitid(P_PID, pid, WEXITED \| WNOHANG \| WNOWAIT)` reports an exit repeatedly without reaping | `CLD_KILLED` twice |
| F9 | A process group whose leader is an unreaped zombie still exists | `killpg(L, 0)`: 0 with a live member, `EPERM` with only the zombie, `ESRCH` after `waitpid` |
| F10 | `proc_pidinfo(PROC_PIDTBSDINFO)`: `arg` 0 fails for a zombie (`ESRCH`); `arg` 1 returns the zombie's record (136 bytes, status `SZOMB`, pgid); another user's process (pid 1) fails `EPERM` | all three |
| F11 | `getsid` reads a process in another session; `ESRCH` for a zombie | both |
| F12 | `proc_listallpids` returns the number of pids written; a full buffer means saturated; the count call returns an estimate | 4 for a 4-entry buffer; count 949, then 930 written |
| F13 | `O_NONBLOCK` set on one duplicate of the master applies to every duplicate | yes |
| F14 | Nonblocking master, slave in **raw** mode: ~1 KiB accepted, then `EAGAIN`; `POLLOUT` not ready while full, ready once the slave reads. Slave in **canonical** mode: writes never block and excess input is discarded | raw: 1 022 bytes, `EAGAIN`, poll 0, then `POLLOUT`; canonical: 409 600 bytes accepted, no `EAGAIN` |
| F15 | After the last slave close: `poll(POLLOUT)` gives `POLLHUP` without `POLLOUT` and `write` fails `EIO`; `poll(POLLIN)` gives `POLLIN \| POLLHUP` and `read` 0 | `0x10`, `EIO`; `0x11`, 0 |
| F16 | Data flushed by the slave (`tcflush(TCOFLUSH)`) between `poll` and `read` makes the nonblocking `read` fail `EAGAIN`, not block | `EAGAIN` |
| F17 | Consecutive forks get consecutive pids | `n`, `n+1` |
| F18 | `posix_openpt(O_RDWR \| O_NOCTTY \| O_CLOEXEC)` sets `FD_CLOEXEC` | yes |
| F19 | A child forked by another thread while std's exec-error pipe is not yet close-on-exec keeps the write end across its exec, and the first spawn returns only when that child exits; holding one lock around both spawns removes the wait. Once the parent has forked, it closes its write end, so later forks do not hold it | window held open by delaying `pipe()` (interposed): without the lock A returned at 3.1 s, when B's `sleep 3` exited; with it at 0.31 s. Without the delay, a child parked before exec in another thread delayed A by 2.6 ms |
| F20 | A signal pending across the fork (blocked by the inherited mask) whose inherited handler blocks: clearing the mask before resetting dispositions runs that handler in the child before exec; resetting first discards it | mask first: spawn blocked until the child was killed; dispositions first: spawn returned in 5.6 ms, exit 0 |
| F21 | An fd open above a lowered soft `RLIMIT_NOFILE` survives a sweep bounded by `getdtablesize()`; `proc_pidinfo(PROC_PIDLISTFDS)` on the process lists it | fd 400, soft limit 128: `getdtablesize()` 128, child has `0,1,2,400`; with the bound `max(getdtablesize(), highest + 1)` = 401 the child has `0,1,2` |
| F22 | `poll(POLLIN)` on a regular file is ready, also at EOF; on `/dev/zero` it gives `POLLNVAL` | `0x1`, `0x1` at EOF; `0x20` |
| F23 | `getsid` succeeds for processes whose `PROC_PIDTBSDINFO` fails `EPERM` | 313 of 313; `getsid(1)` = 1 |
| F24 | When a session leader with a controlling terminal exits, the terminal is revoked for every holder, in any session: the master reads EOF, and a process that kept the slave on fd 0 (`setsid` escapee) is left with a revoked descriptor | after the kill: `OutputEnded(Eof)`; the escapee alive, `lsof -d 0` lists no fd, `FIONREAD` on fd 0 fails (testkit `leader_exit_revokes_the_tty_for_every_holder`) |
| F25 | `TIOCSCTTY` on a non-terminal fd 0 (`/dev/null`) fails `ENODEV` | `ENODEV` |
| F26 | `proc_pidinfo(PROC_PIDT_SHORTBSDINFO, arg 1)` reads a process that `PROC_PIDTBSDINFO` refuses, and reports a zombie as `SZOMB` | pid 1 (root): full record `EPERM`, short record read, not a zombie; an exited unreaped child: `SZOMB`; after the reap: `ESRCH` (`unix::sys::tests::libproc_records_read_zombies_and_another_users_process`). Another user's zombie could not be built on this host (no second account or root); XNU allows this flavor across users (`bsd/kern/proc_info.c`, xnu-12377.1.9, lines 2046–2090), and the census rule for it is tested through the synthetic `Os` |

## 1. Goal and Dependencies

**Goal.** A macOS backend that opens a PTY, starts the CLI as a session leader with the PTY as controlling terminal and nothing else inherited, hands the session a drain over the master, an input that writes nothing when dropped, and a child whose `kill_tree` kills the CLI's session, the descendants it can verify, and the recorded extras, without ever targeting a group or session it no longer owns; plus a `testkit` crate whose `chaldea-fake-cli` stands in for a CLI in real-PTY tests. Every claim of §6 is checked by a named test on `macos-latest`; `windows-latest` builds everything and runs the `FakePty` and FakeCli smoke tests.

**Dependencies.** `rustix` and `libc` for the macOS target of `pty-runtime` (`libc` only in `unix::sys`, §2.7), and `rustix` and `signal-hook` for testkit on Unix; why each, and why not `nix`, is §3 "Syscall wrappers" and "FakeCli without `unsafe`". Exact pins are in the root `Cargo.toml`.

**Workspace and CI.** On `macos-latest` the `workspace` job runs the real-PTY tests, which `cfg` removes on Windows. A step checks that testkit's normal dependencies contain no workspace crate other than `chaldea-core` (§7); `lefthook.yml` mirrors it.

## 2. Types and Functions

### 2.1 Public Additions

```rust
/// The native backend of this OS (macOS: this document; Windows: `docs/design/pty-windows.md`).
#[cfg(any(target_os = "macos", windows))]
pub fn native() -> Arc<dyn PtyRuntime>;

/// Runs `f` while holding the process-wide spawn lock. Every child the daemon creates is
/// created inside such a closure or by the native backend (all OSes; the lock matters on
/// macOS for the exec-error pipe and on Windows for the inheritable handles
/// `CreatePseudoConsole` holds during the call). Not reentrant: `f` must not call it again or spawn a PTY session.
pub fn with_spawn_lock<R>(f: impl FnOnce() -> R) -> R;
```

**The spawn rule.** One `static SPAWN_LOCK: Mutex<()>` in `pty-runtime` is held across every child creation in the process: the native backend's whole `Command::spawn` (§2.2 stage 4) and every closure passed to `with_spawn_lock`, so a `std::process::Command::spawn`, a `tokio::process::Command::spawn`, or a process-wrap spawn inside the closure runs from before std creates its exec-error pipe until it returns. The closure form serves spawn APIs that take no `&mut std::process::Command` (prior-art row 1c). A poisoned lock is taken over. std on macOS creates that pipe and marks it close-on-exec in two steps (`sys/pipe/unix.rs:34-40`); a fork by another thread in between gives that child the write end across its exec, and the first spawn then waits until that child exits (F19). Under the lock no fork happens in that window, and the parent has closed its write end before it releases the lock. The backend's own non-atomic fd, the wake pipe (§2.3), is created under the lock too: `wake_pipe` takes `&SpawnGuard`, a guard only `SPAWN_LOCK` produces, so it cannot be called without the lock. **Constraint:** a child created outside the lock (a library that forks on its own) can still inherit a handshake pipe and stall a spawn until it exits; that spawn then has no deadline. The daemon therefore creates every child inside `with_spawn_lock` and uses no library that creates children outside its call (a PR 8 / PR 10 obligation, with exclusive reaping, §2.4).

Process identity lookup is internal. Tests reach it through `chaldea_pty_runtime::fake::identify(pid) -> io::Result<Option<ProcessIdentity>>` (`fake` feature, macOS), which no default build contains (PR2 D6); `fake::unix_writer_polls() -> u64` counts the writer's entries into its full-queue `poll` (§2.3) for tests that must gate on that state. The public lookup (`pty_runtime::process`) is added by PR 9, its first consumer (`docs/design/hook-ingest.md` §2.5). A launch passes only stdio to the child; there is no fd-passing API.

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

Validation gives early, classified errors; correctness does not depend on it, because the forked child re-checks `cwd` and `exec` and reports any failure before the program runs.

**Sequence.** Each stage lists what exists when it fails.

1. **Open the PTY.** `rustix::pty::openpt(RDWR | NOCTTY | OpenptFlags::from_bits_retain(O_CLOEXEC))` (rustix declares `CLOEXEC` only for Linux and BSDs, `pty.rs:40-46`, and passes unknown bits through; macOS honours it, F18), `grantpt`, `unlockpt`, `ptsname`. Set `O_NONBLOCK` on the master (shared by every duplicate, F13). Failure: `NeverStarted { Setup }`.
2. **Ends and size.** Open the slave in the parent with `O_RDWR | O_NOCTTY | O_CLOEXEC` (`rustix::fs::open`); it is held until stage 5, so the master never reads EOF before the child holds the slave. Then `tcsetwinsize(master, { ws_row, ws_col, ws_xpixel: cols × 8, ws_ypixel: rows × 16 })`, which needs an open slave (F3), so the PTY has `spec.size` with the 8×16 px cells the responder reports (PR2 §2.5) before any byte. Three `fcntl_dupfd_cloexec` duplicates of the master (reader, writer, control); the original master fd is closed. The wake (§2.3): `std::io::pipe()` under `SPAWN_LOCK` (the function that creates it takes the lock's guard) (std sets `FD_CLOEXEC` right after `pipe(2)`, `sys/pipe/unix.rs:34-40`), then `O_NONBLOCK` on its write end. Failure: everything created is dropped; `NeverStarted { Setup }`.
3. **Drain.** `Drain::start(MasterReader { reader dup, wake }, output)` (PR2 §2.4). Failure: `Drain::start` drops the reader first; the rest is dropped; `NeverStarted { Setup }`.
4. **Spawn**: a `std::process::Command` with the `pre_exec` that `sys::install_pre_exec` installs, spawned under `SPAWN_LOCK` (§2.1):
   - `Command::new(argv[0]).args(&argv[1..]).env_clear().envs(spec.env).current_dir(spec.cwd)`; stdin, stdout, and stderr are three `F_DUPFD_CLOEXEC` duplicates of the slave (std `dup2`s them onto 0–2 in the child; the originals close at exec);
   - `bound = max(getdtablesize(), highest open fd + 1)`, computed before the fork: `getdtablesize()` is the smaller of the soft `RLIMIT_NOFILE` and `kern.maxfilesperproc`, so every fd created after this point lies below it; `proc_pidinfo(PROC_PIDLISTFDS)` on the daemon itself finds fds already open above it, such as one inherited from an ancestor that then lowered the limit (F21);
   - `pre_exec`, which runs in the forked child after std's `dup2`s and `chdir` and before `execvp` (`sys/process/unix/unix.rs:283-292`, `:337-339`, `:383-385`, `:408`), and calls only async-signal-safe functions on values prepared before the fork:
     1. `setsid()`;
     2. `ioctl(0, TIOCSCTTY, NULL)` (the variadic argument is a pointer, which the kernel ignores): the slave becomes the controlling terminal and the leader's group its foreground group (F1);
     3. for each fd in `3..bound`: `fcntl(fd, F_GETFD)`; `EBADF` means no fd there; any other failure ends `pre_exec` with that errno; an fd without `FD_CLOEXEC` is closed (a `close` error is ignored: the descriptor is released either way). Close-on-exec fds, std's exec-error pipe among them, stay until `exec` closes them, so the error pipe keeps working (F5). This removes every fd the CLI should not inherit, whichever thread created it (F4);
     4. `sigaction(sig, SIG_DFL)` for every signal 1–31 except `SIGKILL` and `SIGSTOP`, **then** `sigprocmask(SIG_SETMASK, empty)`. std inherits the parent thread's mask and resets only `SIGPIPE` (`:351-371`). Resetting dispositions first means a signal that became pending while masked is handled by its default action, never by a daemon handler (F20). A signal that arrives unmasked between the fork and step 4 runs the daemon's handler in the child; **constraint:** the daemon's handlers are async-signal-safe and never block (signal-hook's handlers set a flag or write to a nonblocking pipe). The parent blocks nothing around the fork.

   A `pre_exec` closure makes std fork rather than `posix_spawn` (`:459-467`). The child is a session and group leader (pid = pgid = sid) whose controlling terminal is the slave, with only fds 0–2, default signal state, in `spec.cwd`, with exactly `spec.env`: nothing is added, so a spec without `SHELL` runs without it. Cost: the sweep takes about 0.08 µs per fd slot below `bound` (F7): under 1 ms at the launchd default of 256 or at 10 240, about 20 ms at the 245 760 ceiling of this host. This is work, not a deadline: `spawn` has none, and it returns once the lock is free and std's handshake ends.
   - **`Err(e)`**: the program never executed. std reports a failure of `chdir`, `pre_exec`, or `execvp` through its close-on-exec error pipe and waits for (reaps) the forked child before it returns (`:136-160`), so no process remains (F5). This is the positive evidence `NeverStarted` requires. `kind`: `ENOENT` → `NotFound` if `metadata(argv[0])` now fails with `NotFound` (symlinks followed, so a removed link target counts; any other error, such as a directory without search permission, leaves the cause open), else `Setup` (a missing cwd or interpreter); `EACCES` → `PermissionDenied`; other (a `pre_exec` failure included) → `Setup`. Release: drop the parent's slave and the writer, close the control, `Drain::finish(2 s)` (the master reads EOF once the parent's slave is gone); `Cleanup::Confirmed` when the drain ended, else `Unresolved { kill: None, close: Some(..), drain_abandoned: true }`.
   - **`Ok(child)`**: std's handshake read EOF, so the child either exec'd or died before exec (a default-action signal after step 4). Given `SpawnSpec`'s precondition that `argv[0]` is a native executable (`backend.rs:34`), an exec runs the CLI; a file without a known format would instead run under `/bin/sh` (`execvp`'s `ENOEXEC` rule, F6), which is outside that precondition. So `Ok` does not prove that the program ran: a child that died before exec (a signal pending across the fork whose default action ends the process, delivered when step 4 clears the mask) appears in stage 5 as an exited leader, and the session reports that exit. The pid is taken from `child.id()`, the `std::process::Child` is dropped without waiting (std never waits for it), and the `UnixChild` guard (§2.4) is built at once. From here every failure is `StartedThenFailed`.
5. **Settle.** Drop the parent's slave (EOF then reaches the drain when the tree's last slave holder closes it, PR2 §2.1). Read the leader's identity with `proc_pidinfo(pid, PROC_PIDTBSDINFO, arg 1)`, which also returns a leader that already exited (F10), so a CLI that exits at once still gets an identity and a normal exit. `group` is the pid (`pre_exec` ran `setsid` or the spawn failed). Failure (any error from that read): `StartedThenFailed { stage: "identity", identity: None }`, released by `SpawnedParts::teardown` (`backend.rs:268-290`): `kill_tree(&[], 5 s)` (the owned group, §2.5), drop the writer, close the control, `Drain::finish(2 s)`, with its `Cleanup`.
6. **Hand over** `SpawnedPty::new(SpawnedParts { child: Arc<UnixChild>, output, input: Box<MasterWriter>, control: Box<UnixControl>, size: spec.size, evidence: vec![] })`.

The Unix backend reports no `evidence`; a key added later is additive. A `spawn` has no deadline of its own: it waits for `SPAWN_LOCK`, the forked child does bounded work before `exec`, and std returns once `exec` succeeded or failed; under the spawn rule nothing else holds the handshake open.

### 2.3 Input and Output Ends

```rust
struct Wake { closed: AtomicBool, read: PipeReader }      // Arc<Wake> shared by reader, writer, control
struct MasterReader { master: OwnedFd, wake: Arc<Wake>, after_wake: Option<u64> }       // Read
struct MasterWriter { master: OwnedFd, wake: Arc<Wake>, pending: Option<io::Error> }    // Write; Drop writes nothing
```

**The wake.** `UnixControl::close` (or its drop) sets `closed` (release ordering), then writes one byte into the pipe that nobody reads, so every `poll` including the read end returns at once from then on. The flag is the cheap check before each read or write; the pipe wakes a thread blocked in `poll`. The pipe is created under `SPAWN_LOCK`, so no child created under the spawn rule inherits it (§2.1).

**Output** (`MasterReader::read(buf)`):

1. If `closed` is set or the wake has been seen, arm `after_wake = Some(0)` if unset. If armed and `after_wake ≥ 1 MiB`: return `Err(other("pty closed before eof"))` without reading.
2. `poll([master: IN, wake: IN])`, timeout 0 when armed, none otherwise; `EINTR` repeats it. Any nonzero `revents` on the wake arms the budget, whether or not the master is also ready.
3. Master `POLLIN`, `POLLHUP`, or `POLLERR`: `read` at most `min(buf.len(), 1 MiB − after_wake)` bytes when armed. `Ok(n > 0)` returns `n` (added to `after_wake` when armed); `Ok(0)` returns `Ok(0)` (EOF); `EAGAIN` or `EINTR` goes back to step 1 (data flushed between `poll` and `read`, F16); every other error goes through `map_read`, which turns `EIO` into `Ok(0)` (the hangup error on other Unix systems) and returns the rest. Master `POLLNVAL`: `Err(other("pty master invalid"))`.
4. Armed and the master not ready: `Err(other("pty closed before eof"))`, which the drain reports as `OutputEnd::Error`, never as EOF.

So after a close the drain returns what the master can deliver at once, at most 1 MiB counted from when the reader observes the wake (a flag check in step 1 or a wake event in step 2; a read already past its `poll` when the wake fires is not charged), then ends, even when another process holds the slave and keeps writing. Bytes after the wake are best effort. On macOS a holder outside the leader's session cannot outlive the leader's hold (F24), so the wake ends the drain only while the leader itself is alive, as after a kill that could not finish.

**Input** (`MasterWriter::write(buf)`), not portable-pty's writer, whose `Drop` writes `\n` + VEOF (`unix.rs:351-363`):

1. If `pending` holds an error, take and return it.
2. Loop with `written` bytes accepted so far:
   - if `closed` is set: return `Ok(written)` if `written > 0`, else `Err(BrokenPipe, "pty closed")`;
   - `write(master, &buf[written..])`: `Ok(n > 0)` adds `n`, and returns `Ok(buf.len())` once all is accepted; `Ok(0)` is treated as `Err(WriteZero)`; `EINTR` repeats; `EAGAIN` goes to the poll below; any other error `e` ends the call: `Ok(written)` with `pending = e` if `written > 0`, else `Err(e)`;
   - after `EAGAIN`: `poll([wake: IN, master: OUT])`, no timeout, `EINTR` repeats (a `cfg(test)` hook and the `fake` counter `unix_writer_polls` mark the entry into this `poll`). Nonzero wake `revents` is checked first and ends the call as `closed` does. Master `POLLOUT`, `POLLHUP`, or `POLLERR` retries the `write` (a hangup then fails `EIO`, F15, and ends the call as above; nothing spins); master `POLLNVAL` ends the call with `other("pty master invalid")` under the same progress rule.
3. `flush()` returns `Ok(())`: nothing is buffered.

So the seam keeps whole-write-or-fail semantics on the nonblocking master: a short count is returned only after progress followed by close or an error, and the error, if any, comes on the next call. During `Session::close` a wake-interrupted short write settles through PR 2's next permission check as `Incomplete { error: "closed" }` without another `write` (`writer.rs:851-866`). A write blocked on a CLI that stopped reading ends at close step 4 (PR2 §2.2) instead of holding the writer thread. A slave in canonical mode never blocks the writer: the line discipline discards input beyond its buffer (F14), which is the terminal's behaviour, not the backend's. Dropping a `MasterWriter` closes its duplicate and writes nothing.

### 2.4 Child: Ownership, Exit, Reaping

```rust
struct UnixChild {
    pid: u32,
    identity: ProcessIdentity,           // start_time empty only during a stage 5 failure
    start: Option<u64>,                  // the recorded start; None in that case
    exit: OnceLock<ChildExit>,
    lost: AtomicBool,                     // ownership of the leader's wait status was lost; never cleared
    kill: Mutex<KillState>,               // targets kept across calls (§2.5); wait_timeout never takes it
}
```

**Ownership is a precondition.** The leader's wait status belongs to this `UnixChild` alone: nothing else in the process may reap it. In the daemon (PR 8 / PR 10): no wildcard wait (`waitpid(-1, ..)`, `wait()`, `waitid(P_ALL, ..)`), no `SIGCHLD` set to `SIG_IGN`, no `SA_NOCLDWAIT`; waits for its other children (git, hooks) are by exact pid. While ownership holds, the leader stays unreaped until the backend reaps it, so its pid, and with it the group and session ids it leads, cannot be reused (F9): every numeric group or session target of §2.5 is then exactly the CLI's.

`identity()` returns the stored `ProcessIdentity` and has no panic path; during a stage 5 failure it carries the pid, the pid as group, and an empty `start_time`.

**`owned(os, deadline)`**, one function over a `ChildOs` (`waitid`, `lookup`, `now`; the real one wraps rustix and `sys`, tests script it), called fresh before every numeric group or session target and in `Drop`:

1. `lost` set: false, with no other call.
2. Deadline passed (`now()` before each of the next two calls): false for this call, `lost` unchanged.
3. `waitid(Pid(L), EXITED | NOHANG | NOWAIT)`: `ECHILD` sets `lost`, false.
4. With a recorded start: `lookup(L)` must be `Live` or `Zombie` with that start; anything else (another start, `Gone`, `Unreadable`) sets `lost`, false. This rejects a waitable replacement at pid `L` that the same process started after an outside reaper took the leader.

A reaper racing between `owned()` and the signal is a broken precondition, not a case the backend defends; `lost` then makes every later call safe. After loss the backend never targets pgid or sid `L` numerically, never reaps the pid, signals only verified `(pid, start)` identities, and reports `tree_dead: false` with `method: "unix_ownership_lost"` (§2.5), also when the exit had been observed before the loss.

**`wait_timeout(timeout)`.** If `exit` is set, return it. Else, with `end = now + timeout`: loop `waitid(Pid(L), EXITED | NOHANG | NOWAIT)`; an exit sets `exit` once and is returned; no exit sleeps `min(10 ms, end − now)` and returns `Ok(None)` once `now ≥ end`; `EINTR` repeats only while `now < end`; `ECHILD` sets `lost` and returns `Err(other("child reaped outside the backend"))` (the session publishes `WaitFailed`); other errors are returned. `CLD_EXITED` gives `ChildExit { code: Some(status), signal: None }`, `CLD_KILLED`/`CLD_DUMPED` `{ None, Some(sig) }`. It takes no lock (`OnceLock`, atomics), so it runs during `kill_tree` (PR2 §2.1).

**Reaping**, in `Drop` (the last `Arc` goes when the session closes or drops): if `owned()`, one `waitpid(Some(L), NOHANG)`, which reaps an exited leader and returns 0 for a live one. Otherwise nothing: a lost leader is not the backend's. A leader still alive at `Drop` is one SIGKILL could not finish by the kill deadline (the kill reported `tree_dead: false`); it stays unreaped, and its zombie later keeps the pid, group, and session ids reserved, the safe direction. Bound: one retained child or zombie per abandoned launch, until the daemon exits; there is no daemon-wide bound.

### 2.5 Tree Kill

```rust
impl PtyChild for UnixChild { fn kill_tree(&self, extra: &[ProcessIdentity], deadline: Instant) -> KillReport; }

pub(crate) struct Proc { pid: u32, ppid: u32, pgid: u32, start: u64 }
pub(crate) enum Lookup { Live(Proc), Zombie(u64), Gone, Unreadable }   // PROC_PIDTBSDINFO arg 1: SZOMB (with start), ESRCH;
                                                                       // refused: PROC_PIDT_SHORTBSDINFO SZOMB → Zombie(0), else Unreadable
pub(crate) trait Os {
    fn owned(&self, deadline: Instant) -> (bool, Option<Lookup>);   // with the leader record it read
    fn pid_list(&self, capacity: usize) -> io::Result<(Vec<u32>, bool)>;   // (pids, saturated)
    fn lookup(&self, pid: u32) -> Lookup;
    fn getsid(&self, pid: u32) -> Result<u32, Errno>;
    fn killpg(&self, pgid: u32, sig: Sig);
    fn kill(&self, pid: u32, sig: Sig);
    fn now(&self) -> Instant;
    fn sleep(&self, d: Duration);
}
pub(crate) struct KillState { targets: BTreeMap<(u32, u64), u32> }   // retained identities → last group read
struct Pass { clean: bool, churn_only: bool, alive: bool, owned: bool, covered: HashSet<u32>,
              pending: HashMap<u32, Vec<Proc>>, waiting: HashSet<u32> }   // per census pass
```

Known zombies are a set local to one call.

**Ownership for the call.** Right after the `kill` mutex is taken, one `owned()` check (under the deadline rules below) establishes ownership for the call. Session membership covers a process only when that check held and ownership has not been lost since; without it the call can still kill retained identities, but its result is never `tree_dead: true`. Every numeric group signal still takes its own fresh `owned()` (Signals, below).

**The retention invariant: one read function.** Every `lookup` of a process, in a census pass, the recheck, a grace poll, before a per-pid signal, or inside an ownership check (the leader's record, §2.4), goes through one function, and so does every census `getsid`. Before anything else runs (a deadline check, an ownership check, another read), that function applies the read to the retained targets, keyed by the full identity `(pid, start)`:

- a read that shows the pid `Gone` or a zombie, or `getsid` `ESRCH`, removes every identity at that pid; a `Live` read removes every identity at that pid with another start;
- a `Live` record that is covered is inserted at once under its identity: a retained identity, the leader's pid or group `L`, a member of session `L` (at its `getsid`), or a child of a generation this pass already read covered. A live record whose parent the pass has not read covered yet waits under its ppid, and is covered the moment that parent is, together with its own waiting children; a waiting process that a later read shows gone is not retained when its parent is covered, but the ancestry it was read with still leads to its own waiting children;
- an unreadable session member and a leader whose start is unknown (a stage 5 failure) are kept by pid until a read shows them gone; with ownership for the call, the first readable read of such a leader keeps its start.

Nothing else inserts or removes a target. So absence from a list, an expired deadline, an interrupted recheck, or an incomplete pass never removes one, and an identity the call read live and covered is retained however the call ends, whatever the process does before the next call (`setsid`, its parent's exit). The survivor report is derived from the retained targets and the pids kept without a start (one entry per pid: a live read leaves one generation at its pid, so several remain only among identities not read since they were recorded, and the earliest start is listed), so it cannot disagree with them. A confirmed removal takes effect at its read, so a call that ends at its deadline keeps no identity it saw resolved.

**The proof rule.** Death is proven exactly when a clean, owned pass (with ownership for the call) reads nothing covered alive and the recheck that follows leaves nothing retained: no target and no pid kept without a start. Whatever a recheck read keeps (a live target, an unreadable one, or a new live generation it found at a recorded pid) leaves the set non-empty, so the result is not `tree_dead: true`.

**Deadline discipline.** `now()` is read before every other `Os` call, including the calls inside `owned()` and each list retry; an expired deadline stops the work in progress. A pass cut short is not clean. Between a lookup (or `owned()`) and the signal it authorises, the only call is that `now()` check. In-memory work makes no syscall and is bounded by what the call holds: applying one read (with the pending children it covers, at most one pass), copying the retained identities before a loop that reads each of them (each read is itself preceded by a `now()` check), and deriving the report (the retained set and the pids without a start, indexed by pid). That work runs to the end even after the deadline.

**One pass.** `pid_list` (`proc_listallpids`: a count call, then a buffer of count + 64; a result that fills the buffer is saturated, F12, and is retried with double the capacity, up to three times). Then for each listed pid: `lookup`, then `getsid` unless the lookup was `Gone` or a zombie. `lookup` reads `PROC_PIDTBSDINFO` (arg 1); when that is refused with anything but `ESRCH` (another user's process, F10), it reads `PROC_PIDT_SHORTBSDINFO`, which other users' processes allow (F26): `SZOMB` there gives `Zombie(0)` (the short record has no start), anything else `Unreadable`. The pass is `alive` when it read a covered process live, a retained pid unreadable, or an unreadable session member.

**The census rule.** A pass is clean only if the list call succeeded with a nonzero, unsaturated result, the deadline did not cut the pass short, and **every listed pid was resolved without churn**: a `Live` lookup followed by a successful `getsid`, an `Unreadable` lookup followed by a successful `getsid` (F23), or a zombie already seen earlier in this call. Every other outcome makes the pass unclean: `Gone` at lookup, a zombie seen for the first time (then recorded), `ESRCH` at `getsid` after a `Live` or `Unreadable` lookup, and any other `getsid` failure (a sid that could have made the pid a member). The first three are churn, a process that exited during the pass and may have forked a child the list does not contain; `churn_only` marks a pass whose only defects are churn. Another user's zombie confirmed by the short record is a zombie like any other: churn the first time, then known, so one that stays (its parent never reaps it) does not keep a quiet host from a proof. Where the short record cannot establish it, the rule stays conservative: `Unreadable`, then `getsid` `ESRCH`, is churn on every pass. **Constraint (foreign zombies):** such a zombie is known by pid and zombie status only (the short record has no start). A process of another user inside the CLI's session (a privileged helper the CLI ran) whose pid reuses one seen earlier in the same call as a foreign zombie, and that forks and exits within one pass, looks like the known zombie, so its child can be missed. This needs another user's process in the session and pid reuse within one kill call; every recorded identity is still rechecked by itself. **Constraint:** on a host where every pass sees a process exit, death is never proven before the deadline and the result is `tree_dead: false`; churn can delay a proof but never produce a false `tree_dead: true`.

**Constraint (list truncation).** XNU sizes its copy of the process list as `min(nprocs + 20, buffer)` before it takes the process-list lock (`bsd/kern/proc_info.c`, xnu-12377.1.9, lines 356–448); when more than about 20 processes are created between the two, older entries can be left out of a result that is not saturated. An unknown session member left out that way can be missed by a pass, and so by the proof. The leader and every recorded target cannot be: before a pass may prove death, each is looked up by itself (the recheck, step 5).

**Coverage.** With `L` the leader pid, the tree is:

- *Session members* (only with ownership for the call): every `Live` process with `pgid == L` or `sid == L`, and every unreadable pid with `sid == L`. A pid whose sid could not be read is never silently left out: the pass is then unclean. While owned, nothing outside the CLI can be in session `L` (a process joins a session only by being forked into it, and `L` cannot be reused).
- *Targets*: identities kept in `KillState` across calls: the leader `(L, start)`; each `extra` entry, accepted only if its pid is not 0, 1, or the daemon's own; every identity covered below.
- *Descendants*: every live process whose parent's covered generation the same pass read live, transitively. The kernel reparents a child to launchd when its parent exits, so a `ppid` names the live parent; no wall-clock comparison is made.

The proof covers the original session (while owned), every descendant of a covered process that was live when a pass saw it, and the verified `extra` identities with their descendants. A process that left the session (`setsid`) and whose parent exited before any pass saw it is outside the tree: the documented macOS containment limit (spike §4.6), recovered only when the daemon names it in `extra`.

**Signals.**

- *Group*: `killpg(L, sig)` right after a fresh `owned()`; never after loss. It covers the leader, which is always in group `L`.
- *Everyone else*: `signal_verified(id, sig)`: `lookup(id.pid)` (through the read function), the `now()` check, `kill(id.pid, sig)`, sent only if the lookup is `Live` with the recorded start and the pid is not 0, 1, or the daemon's own. Members found by pgid or sid are signalled this way too, with the start their read recorded.

**Steps**, with `t0` the entry time and `B = deadline − t0`. Every call computes its result from its own reads; nothing proven earlier is reused.

1. **Expired or busy.** If `now ≥ deadline`: no blocking lock, no syscall; return `tree_dead: false`, survivors the leader's identity and the accepted `extra` entries, one entry per pid; when one `try_lock` gets the state at once, the accepted `extra` entries are also stored as targets and the targets kept by earlier calls are listed. Else take the `kill` mutex with `try_lock` polled every 10 ms until `deadline`; never acquired: the same report with `method: "unix_busy"`. The caller passes its full list of recorded descendants on every call, retries included, and never resubmits a report's survivors (PR2 §2.1); the stored targets are the backend's internal margin, so a call that could not store its `extra` (busy, or expired with the state held elsewhere) loses nothing. A poisoned lock is taken over. Add the accepted `extra` entries to the targets, then check ownership for the call.
2. **Opening pass**, then a fresh `owned()`. If the pass is not `alive`: clean and owned → the recheck (step 5), and `tree_dead: true` with no signal when it resolves everything; unclean with `churn_only` → another opening pass at once; after loss → `tree_dead: false` with no signal.
3. If `B > 100 ms`: **HUP** the group and every retained target outside it; wait until `t0 + min(500 ms, B/4)`; pass; **TERM** the same way; wait until `t0 + min(2 s, B/2)`. Each wait polls the targets by lookup every 25 ms (each sleep capped to `deadline − now`) and ends early once none is live.
4. Pass, **KILL** the group and every retained target outside it.
5. **Prove**, until `deadline`: pass, then a fresh `owned()`. A pass that is clean, owned (and owned for the call), and not `alive` goes to the **recheck**: every retained identity and every pid kept without a start is looked up by itself, so one the list left out is still seen; the result is `tree_dead: true` only when nothing is retained afterwards (the proof rule). A recheck cut by the deadline proves nothing; what it read is already applied. Alive (the pass or the recheck): KILL (group and verified per pid), sleep 25 ms, repeat. Not alive but unclean: repeat at once when `churn_only`, else after 25 ms. After loss: return `tree_dead: false` as soon as a pass reads nothing alive.
6. At `deadline`: `tree_dead: false`, survivors derived from the retained targets and the pids kept without a start (`start_time` empty for those, `group` the pgid last read or empty).

`method` is `"unix_killpg_session_scan"`, `"unix_ownership_lost"` once ownership was lost, or `"unix_busy"`. `kill_tree` has no panic path: every syscall error is a value. **High churn:** a pass over the whole table (~1 000 pids, a few milliseconds) is unclean whenever any listed process exits during it; step 5 rescans at once, but a host where every pass sees an exit cannot prove death before the deadline and reports `tree_dead: false`, with only what is still retained as survivors.

**Never a reused pid.**

- Group and session targets are numeric only right after `owned()`, which ties `L` to the recorded leader; after loss there are none.
- A per-pid signal follows a lookup of the same pid that showed the recorded start. **Residual window:** the process can exit, be reaped by its parent, and the pid be handed to another process between that lookup and the `kill(2)`. macOS allocates pids sequentially (F17), so this needs the pid space to wrap while the daemon is descheduled between two syscalls; nothing bounds the descheduling, and macOS has no pidfd to close the gap. This is the spike's recheck rule (`os/unix.rs:127-133`, spike §4.6), kept as is. testkit's `Cleanup` has the same window.
- A pass is not atomic: a pid can be reused between the list and its lookup, or between a parent's and a child's lookup within one pass, so a descendant can be attributed to a parent that has just been replaced. The same wraparound bounds this.

### 2.6 Control

```rust
struct UnixControl { master: OwnedFd, wake: Arc<Wake>, wake_write: PipeWriter }   // PtyControl
```

- `resize(size)`: `tcsetwinsize(&master, { ws_row, ws_col, ws_xpixel: cols × 8, ws_ypixel: rows × 16 })`; the kernel sends `SIGWINCH` to the foreground group (F1). The `io::Error` keeps its errno. Never blocks.
- `close(deadline)`: sets `closed`, writes the wake byte once on the nonblocking write end (`EAGAIN` means a byte is already there; `EINTR`, not expected for a nonblocking write, is retried at most three times), drops its fds, returns `CloseReport { elapsed: Some(..) }`. It never blocks, so the deadline is always met. Dropping a `UnixControl` without `close` does the same.

### 2.7 The `unsafe` Carve-Out (PR2 D7)

`crates/pty-runtime/Cargo.toml` gets its own `[lints]` table: `[lints.rust] unsafe_code = "deny"`, `unsafe_op_in_unsafe_fn = "deny"`; `[lints.clippy]` the workspace's clippy set restated with the same values, plus `undocumented_unsafe_blocks = "deny"` and `multiple_unsafe_ops_per_block = "deny"`. `deny`, because the workspace's `forbid` cannot be lifted by an `#[allow]`; restated, because Cargo's `[lints] workspace = true` cannot be combined with crate-local keys. Every other crate keeps `[lints] workspace = true` with `forbid`.

Exactly one module is allowed `unsafe`: `#[allow(unsafe_code)] mod sys;` in `src/unix/mod.rs`. Everything else, the kill engine included, is safe code over rustix and std. `sys` exposes safe functions only:

| Safe function | `unsafe` operations inside | Safety argument |
|---|---|---|
| `install_pre_exec(&mut Command, bound)` (the caller spawns under `SPAWN_LOCK`) | `CommandExt::pre_exec`; in the closure `setsid`, `ioctl(TIOCSCTTY)` (a null pointer argument), `fcntl(F_GETFD)`, `close`, `sigaction`, `sigprocmask` | The closure captures only `bound: c_int` and a `sigaction` value built before the fork; it allocates nothing, takes no lock, and calls only async-signal-safe functions, as `pre_exec` requires in a multithreaded parent. `close` touches only fds without `FD_CLOEXEC`, none of which std uses after this point. |
| `fd_ceiling() -> io::Result<c_int>` | `getdtablesize` (declared locally: `unistd.h`; absent from libc 0.2.191 for Apple targets), `proc_pidinfo(getpid(), PROC_PIDLISTFDS, ..)` | Called before the fork, where allocation is allowed: a size query, then a `Vec<proc_fdinfo>` of that size plus 32 entries whose byte length is passed; only `return / size_of::<proc_fdinfo>()` entries are read. Returns `max(getdtablesize(), highest fd + 1)`. |
| `pid_list(capacity) -> io::Result<(Vec<u32>, bool)>` | `proc_listallpids` | A `Vec<pid_t>` of `capacity` entries whose byte length is passed; the return (entries written) is checked: ≤ 0 is an error, `capacity` means saturated. |
| `bsd_info(pid) -> Result<BsdInfo, Errno>` | `MaybeUninit::<proc_bsdinfo>::zeroed().assume_init()`, `proc_pidinfo(pid, PROC_PIDTBSDINFO, 1, ..)` | `proc_bsdinfo` holds only integers and `c_char` arrays, so all-zero is valid; the size passed is `size_of::<proc_bsdinfo>()`; the record is used only when the return equals that size. |
| `short_zombie(pid) -> Result<bool, Errno>` | `proc_pidinfo(pid, PROC_PIDT_SHORTBSDINFO, 1, ..)`, `assume_init` of the zeroed `proc_bsdshortinfo` | As `bsd_info`, with `proc_bsdshortinfo`; true when its status is `SZOMB`. |

`openpt`, `grantpt`, `unlockpt`, `ptsname`, `open`, `fcntl_dupfd_cloexec`, `fcntl_setfl`, `getsid`, `kill`, `killpg`, `waitid`, `waitpid`, `poll`, `tcsetwinsize`, and `access` are rustix's safe functions. `tests/lints.rs` checks the allowance and the table (§6 CL-20).

### 2.8 `testkit` and `chaldea-fake-cli`

**Crate.** `chaldea-testkit`, `publish = false` (workspace value), `[lints] workspace = true` (so `forbid(unsafe_code)`). Normal dependencies: no workspace crate yet; `core` is the only one it may add (mvp §3). FakeCli is library code, `chaldea_testkit::fake_cli::main(args) -> i32`, and the binary `chaldea-fake-cli` is a two-line wrapper over it.

**Command line** (a subset of spike §4.5; an unknown mode or flag exits 2):

```
chaldea-fake-cli --mode <echo|queries|attachable|spawn-tree|sleeper|report-process> [--report <127.0.0.1:port>]
                 [--role <role>] [--roles <list>] [--max-life-secs <n>] [--reply-timeout-ms <n>]
                 [--raw] [--setsid] [--escape-orphan] [--orphan-keeps-tty]
                 [--out <path>] [--emit-output] [--validate-input <n>] [--] [ignored…]
```

**Terminal.** A mode that reads stdin enters raw mode (`rustix::termios` `make_raw`) only when stdin `isatty`; `--raw` makes a non-reading fixture do the same. The hello says which (`raw`). A pipe-backed run is the explicit non-PTY mode the smoke tests use. The Windows terminal functions are `docs/design/pty-windows.md` §2.8.

**Report channel.** With `--report`, each FakeCli process connects its own TCP stream to the address, writes a hello line, then mode events, one JSON object per line. It ends the stream only as it exits: a killed process by the kernel, and a normal exit (the mode returned, or `--max-life-secs` passed) by `shutdown(Write)` after its last line, right before the process ends. Without that, the socket would still be open at process exit, which Windows resets, and a reset discards the lines the test has not read yet. A reader thread answers each `ping` line from the test with `{"event":"pong","inq":n}`, `inq` being `FIONREAD` on stdin when it is a terminal (`rustix::io::ioctl_fionread`). The test reads EOF on the stream as the process's death (or its normal exit under way) and a `pong` as proof it is alive, an oracle independent of the backend's process table. std sockets are close-on-exec, so a fixture's children never hold its stream. The hello: `{"mode","role","pid","ppid","pgid","sid","raw","ttys","ctty","fg","fds"}`: `pgid`/`sid` from `getpgrp`/`getsid`; `ttys` the `ttyname` of fds 0–2; `ctty` whether `/dev/tty` opens; `fg` whether `tcgetpgrp(0) == getpgrp()`; `fds` the open fds other than the `/dev/fd` listing's own handle, listed before connecting. Unix-only fields are null on Windows.

**Modes** (each lifted with the PR that first tests it; the Windows forms, `report-process`, `--emit-output`, `--validate-input`, and the `breakaway` role are `docs/design/pty-windows.md` §2.8):

| Mode | Behaviour | Lifted later |
|---|---|---|
| `echo` | `ESC[?2004h`, then every stdin chunk as `{"event":"input","hex":…}` (or one `validated` event with `--validate-input`) | paste parsing, hooks via `--emitter`, `--nested`, `--query-mid-paste`, `--ignore-input` (PR 9 / PR 10) |
| `queries` | the spike's seven queries (`fake_cli.rs:383-481`), each with a reply bound of `--reply-timeout-ms` (default 500), one `{"event":"query","kind","ok","received"}` per query, then `{"event":"queries","all_ok"}`; stdin EOF ends the remaining waits at once | — |
| `attachable` | `MARKER-n` every 500 ms. On a terminal: `{"event":"size","cols","rows","xpixel","ypixel"}` at start, and `{"event":"winch",…same fields}` each time its `SIGWINCH` flag (`signal_hook::flag::register`) is set, so the event acknowledges the signal, not a polled size. On a pipe: one `size` event with null fields, no `winch` | input parser and key reports (PR 11) |
| `spawn-tree` | starts one sleeper per role in `--roles` (default: all of the table below except the orphan), passing its own `--report`, so each sleeper sends its own hello; then idles. `--escape-orphan` adds the orphan. `--emit-output` writes output lines on the test's command | — |
| `sleeper` | the roles below; never reads stdin; idles until killed or `--max-life-secs` | `mcp` role (PR 13 / PR 14) |
| `stall`, `banner` | not lifted | the PR that first asserts on them (PR 10: gate timeout snapshot, fake banner) |

**Sleeper roles** (Unix; each isolates one mechanism and sends its hello only once that mechanism is in place; the Windows roles are `docs/design/pty-windows.md` §2.8):

| Role | How it is made; hello after | What reaches it |
|---|---|---|
| `plain` | child of spawn-tree | the group |
| `ignore-term` | handlers installed with `signal_hook::flag::register` for SIGHUP and SIGTERM | the group's KILL |
| `double-forked` | its parent `double-fork-parent` (no hello) exits at once; hello after `getppid() == 1` | the group (no live parent) |
| `own-group` | spawned with `CommandExt::process_group(0)`: same session, other group | the `getsid` scan |
| `setsid` | `--setsid`: calls `setsid()` first; its parent stays alive | the ppid walk |
| `setsid-child` | child of a `setsid` sleeper | the ppid walk from a target |
| `setsid-orphan` | `--escape-orphan`: a `setsid` sleeper whose parent `orphan-parent` exits at once; hello after `getppid() == 1`; `--orphan-keeps-tty` keeps fds 0–2 on the PTY (revoked when the leader exits, F24), `--raw` puts it in raw mode first; it starts one `orphan-child` | only `extra` (the containment limit) |
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
    pub fn ping(&mut self, deadline: Instant) -> Option<Pong>;                                 // alive: a pong came back
}
pub type Identify = fn(u32) -> Option<String>;                                    // pid → start stamp of a live process
pub type KillVerified = fn(u32, &str) -> bool;                                    // (pid, start stamp) → killed that process
pub struct Cleanup { /* fixture identities, owned std::process::Child values */ }  // Drop: kill each still-live identity
impl Cleanup {
    pub fn new(identify: Option<Identify>) -> Cleanup; pub fn track(&mut self, pid: u32); pub fn own(&mut self, child: Child);
    pub fn kill_with(self, kill: KillVerified) -> Cleanup;                        // Windows: fake::kill_verified
}
```

`Cleanup` is independent of the `Session`: each test registers every fixture in it, so a fixture that survives a consumed `Session` (`close`) or a failing assertion is killed by verified identity (pid and start through the `Identify` the test passes, `fake::identify` on macOS, since testkit's normal dependencies cannot include `pty-runtime`; with the residual window of §2.5). On macOS it sends `SIGKILL`; Windows tests pass `fake::kill_verified` through `kill_with` (`docs/design/pty-windows.md` §2.8).

**Locating the binary.** `CARGO_BIN_EXE_<name>` exists only for integration tests of the package that defines the binary, so the real-PTY tests that need FakeCli are integration tests of testkit (`tests/pty_unix.rs`), with `chaldea-pty-runtime` as a dev-dependency, using `env!("CARGO_BIN_EXE_chaldea-fake-cli")`. `chaldea-pty-runtime`'s own tests use system binaries (`/bin/sh`, `/bin/sleep`, `/usr/bin/env`, `/bin/pwd`, `/bin/stty`, `/bin/echo`, `/bin/cat`, `/usr/bin/true`, `/usr/bin/pgrep`). A later crate whose tests need FakeCli declares a thin `[[bin]]` over `fake_cli::main` or puts the test in testkit; guessing `target/<profile>/` is a legacy fallback that depends on Cargo's `build-dir` (assert_cmd `src/cargo.rs:275-296`).

## 3. Structure Options

| Fork | Option | Precedent | Verdict |
|---|---|---|---|
| **Spawn primitive** | A. portable-pty `spawn_command` | spike `session.rs`; prior-art row 1d | Rejected. `close_random_fds()` in `pre_exec` (`unix.rs:118-143`, `:239`) also closes std's exec-error pipe, so an exec failure after fork (a missing shebang interpreter) is reported as a started child; the sweep allocates and reads a directory after fork in a multithreaded process; `as_command` runs the child in `$HOME` when `cwd` is not a directory and injects `SHELL` (`cmdbuilder.rs:452-458`, `:479-480`). |
| | B. Darwin `posix_spawn` with `POSIX_SPAWN_SETSID`, `CLOEXEC_DEFAULT`, the slave opened onto fd 0 by a file action | — | Rejected. The child gets no controlling terminal (F2): XNU applies file actions before `SETSID`, so the open happens outside the new session, and `posix_spawn` has no step after `setsid` to run `TIOCSCTTY`. A same-pid launcher binary that runs `TIOCSCTTY` and then execs the CLI would need its own shipping and exec-error channel. |
| | C. `std::process::Command` with a minimal `pre_exec`: `setsid`, `TIOCSCTTY`, close non-CLOEXEC fds ≥ 3 up to the highest open fd, reset signal dispositions and then the mask | `alacritty_terminal` 0.26.0 `src/tty/unix.rs:249-275` (`setsid`, `TIOCSCTTY`, closes its PTY fds, resets six signals; Zed launches shells through it); portable-pty `unix.rs:200-247` (the same plus a sweep of every fd) | **Adopted.** std keeps its exec-error pipe and reports `chdir`, `pre_exec`, and `exec` failures as `Err` with the errno after reaping the child (F5); the controlling terminal and `SIGWINCH` work (F1); the sweep, limited to non-CLOEXEC fds, closes the inheritance window from other threads without touching the error pipe (F4). Cost: one fork of the daemon per launch under the spawn lock, a sweep up to the highest open fd (F7, F21), and `pre_exec` as the `unsafe` surface (§2.7). |
| **Child creation across threads** | A. No coordination | — | Rejected: a fork in std's pipe-to-CLOEXEC window holds the handshake open until that child exits (F19). |
| | B. One process-wide spawn lock across every child creation, the wake pipe's creation included | — | **Adopted** (§2.1). Cost: spawns are serialized, each for the length of its handshake; children created outside the lock remain a stated constraint. |
| | C. An atomically close-on-exec handshake | — | Rejected: macOS has no `pipe2` or `SOCK_CLOEXEC`, and `posix_spawn`, which needs no handshake, cannot give a controlling terminal (F2). |
| **Child signal reset** | A. Clear the mask, then reset dispositions | alacritty resets six dispositions and leaves the mask | Rejected: a signal pending across the fork runs a daemon handler before exec (F20). |
| | B. Reset dispositions, then clear the mask; nothing blocked in the parent | — | **Adopted.** The window before the reset is covered by the constraint on daemon handlers (§2.2). |
| **Sweep bound** | A. `getdtablesize()` | — | Rejected: misses an fd inherited above a lowered limit (F21). |
| | B. `max(getdtablesize(), highest open fd + 1)` from `PROC_PIDLISTFDS` before the fork | — | **Adopted.** |
| **PTY opening** | A. portable-pty `openpty` | `unix.rs:18-61` | Rejected: it creates both fds without `FD_CLOEXEC` and sets it afterwards, so a concurrent spawn elsewhere in the daemon can inherit the slave and hold EOF; it would be portable-pty's only remaining use. |
| | B. rustix `openpt` with `O_CLOEXEC`, `grantpt`, `unlockpt`, `ptsname`; slave opened with `O_CLOEXEC`; sized after the slave opens | F3, F18 | **Adopted.** Every PTY fd is close-on-exec from creation; all safe calls. |
| **`unsafe` placement** | A. Inline where used | spike `os/unix.rs` (18 blocks) | Rejected: every module an audit site. |
| | B. Workspace `deny`, each other crate adds `#![forbid(unsafe_code)]` | — | Rejected: contradicts PR2 D7 and mvp §3; a new crate that forgets the attribute allows `unsafe`. |
| | C. Crate-local `deny`, one `#[allow]` on `unix::sys` exposing safe functions | PR2 D7 | **Adopted**, with a test for the restated table. |
| **Syscall wrappers** | A. `libc` only | spike | Rejected: every signal, wait, and poll would be `unsafe`. |
| | B. `nix` | portable-pty's `nix 0.25.1` | Rejected: an old release line and a second wrapper next to the rustix already in the lock. |
| | C. `rustix 1.1.6`, `libc` only for the `pre_exec` body and libproc | `alacritty_terminal` uses rustix on Unix | **Adopted.** |
| **Master mode** | A. Blocking master, poll before read | — | Rejected: data flushed between `poll` and `read` leaves the read blocked with no way to cancel it (F16), and a write blocked on a full queue cannot be cancelled. |
| | B. Nonblocking master (shared by all duplicates, F13); the writer loops with `poll(POLLOUT)` to keep whole writes; both stop on the wake | herdr's Unix actor polls a nonblocking master (`src/pty/actor/unix.rs:391`, read for PR2 §3) | **Adopted.** Reads and writes are cancellable by close; PR 2's reader and writer threads are unchanged. |
| **Wake** | A. `std::io::pipe` with a flag, created under `SPAWN_LOCK` | — | **Adopted.** |
| | B. kqueue `EVFILT_USER` | — | Rejected: a kqueue is not inherited across fork, but the spawn lock already keeps the pipe from children, and it would add a second event mechanism next to `poll`. |
| **Exit observation** | A. `try_wait` (reaps) | spike; std | Rejected: once reaped, the leader's ids can be reused under a later `killpg`. |
| | B. `waitid(WNOWAIT)` polling, reaping only in `Drop` | F8, F9 | **Adopted**, with exclusive ownership as a stated precondition and loss detected by `ECHILD` or a start mismatch. |
| **Leader outliving its kill** | A. A process-wide reaper thread | — | Rejected: a thread that must start in `Drop`, a shared list, and its own failure paths, to free a pid that is safer kept. |
| | B. Leave it unreaped | — | **Adopted**: one per abandoned launch, until the daemon exits (§2.4). |
| **Process table** | A. `proc_listallpids` + `proc_pidinfo` (`arg` 1) + `getsid` | spike `os/unix.rs:25-124` | **Adopted**, under the census rule of §2.5 (the spike dropped zombie, gone, and unreadable pids alike). |
| | B. `sysctl(KERN_PROC_ALL)` | `ps` | Rejected: libc has no `kinfo_proc`; a hand-written layout is a liability, and `arg` 1 already reads zombies (F10). |
| **Kill sequence** | A. HUP and TERM together, 2 s, KILL, then recorded-pid fallback | spike `os/unix.rs:200-223`, `session.rs:347-377` | Partly adopted: the per-pid recheck and the snapshot-first order. |
| | B. HUP → TERM → KILL on the `getsid` set, 250 ms each | herdr (prior-art row 2) | Partly adopted: separate HUP and TERM waits, the session set. |
| | C. Pass first, return at once on an empty clean tree, then HUP, TERM, KILL to the group and the targets with budget-capped waits, targets kept across calls, every result from the call's own passes | — | **Adopted.** |
| **FakeCli reporting** | A. HTTP `/witness` | spike `fake_cli.rs:113-139` | Rejected: an HTTP receiver in every test; the daemon's receiver is PR 9. |
| | B. Report files | — | Rejected: no death signal. |
| | C. One loopback TCP stream per fixture: EOF = death, `ping`/`pong` = alive | — | **Adopted.** |
| **FakeCli binary location** | A. Guess `target/<profile>/` | assert_cmd legacy fallback (`src/cargo.rs:275-296`) | Rejected: breaks with a separate `build-dir`; stale with `-p`. |
| | B. A `[[bin]]` in pty-runtime | — | Rejected: a test-only binary in a library crate. |
| | C. Real-PTY FakeCli tests as testkit integration tests | Cargo's `CARGO_BIN_EXE_*` | **Adopted.** |
| **FakeCli without `unsafe`** | A. `libc` termios and `signal` | spike `os/unix.rs:267-303` | Rejected: testkit keeps `forbid`. |
| | B. rustix termios and `FIONREAD`, `signal_hook::flag`, `setsid` by the sleeper itself, `process_group(0)` | — | **Adopted.** |
| **Platform gate** | A. `cfg(unix)` with a Linux `/proc` table | — | Rejected: Linux is not a host (AGENTS.md) and no CI runs it. |
| | B. `cfg(target_os = "macos")` | spike `os/unix.rs:5-6` | **Adopted.** Linux builds `core`, `FakePty`, and testkit only. |

## 4. Same-Class Inventory

Every MVP place that kills processes, reads pids, or spawns children:

| Place | PR | How this design serves it or stays out |
|---|---|---|
| `Session::kill`, `Session::close` step 2, `SpawnedPty` teardown | 2 | `UnixChild::kill_tree`. |
| `chaldea stop`, `close --stop`, `resume` death check | 10 | The supervisor calls `Session::close(extra, ..)`; `KillReport::method` becomes `launch_death_confirmed.method`; `tree_dead: false` keeps `death_confirmed: false`. |
| Recorded descendants (`extra`) | 10 | The identity format of §2.1. PR 10 records only identities it can prove belong to the launch (its containment evidence and its own spawn records); hook-emitter ancestry is not a source (`docs/design/hook-ingest.md` D10). The public lookup arrives with PR 9 (`pty_runtime::process`), which uses it only for hook binding. |
| Daemon restart: old tree check by recorded sid and start window | 10 | Stays out. The new daemon does not own the old leader, so it may signal only verified identities: exactly the ownership-lost mode of §2.5. `unix::kill` takes its `Os` and leader as parameters so PR 10 can add an entry point for an unowned leader without changing the engine. |
| Daemon process rules | 8, 10 | The spawn rule of §2.1 and the ownership precondition of §2.4; loss is detected (`ECHILD` or a start mismatch → `WaitFailed` or the ownership-lost kill). |
| Daemon spawns of git and one-shot hooks | 6, 16, 20 | Not PTY children. They are created inside `with_spawn_lock` (§2.1) and waited for by exact pid (§2.4). They cannot inherit a PTY fd or a wake pipe (close-on-exec from creation, or created under the lock). |
| Worktree removal in-use check | 16 | Stays out: it reads launch state, never processes. |
| Rescue | 20 | Stays out: it acts through `surface_action stop`, the stop path above. |
| Doctor: FakeCli tree kill | 21 | `native()` + `Session` + FakeCli; how the shipped binary gets FakeCli is PR 21's choice. |
| Windows tree kill (job + recorded descendants) | 4 | Shares the `extra` rule, the caller's full-list obligation, `KillReport`, and testkit (`docs/design/pty-windows.md`). |
| FakeCli's own children | 3 | testkit only; every sleeper takes `--max-life-secs`; tests hold a `Cleanup`. |
| Spike watchdog and emergency kill | — | Not lifted; the supervisor (PR 10) decides. |

## 5. Resource Paths

| Resource | Created at | Owner | Bound | Released on |
|---|---|---|---|---|
| Master fd (original) | stage 1 | spawn | 1 | end of stage 2 |
| Parent's slave fd | stage 2 | spawn | 1 | stage 4 failure release, or stage 5 |
| Slave duplicates for stdio | stage 4 | `Command` | 3 | `Command` dropped at the end of stage 4 |
| Master duplicate R | stage 2 | `MasterReader` (drain thread) | 1 fd | drain end: EOF, read error, or wake |
| Master duplicate W | stage 2 | `MasterWriter` (session input) | 1 fd | input dropped; writes nothing |
| Master duplicate C | stage 2 | `UnixControl` | 1 fd | `close` or drop |
| Wake pipe | stage 2, under `SPAWN_LOCK` | read end in `Arc<Wake>` (reader, writer, control), write end in control | 2 fds | the last of reader, writer, control dropped |
| `pty-drain` thread | stage 3 | `Drain` | 1 | EOF, read error, or wake (≤ 1 MiB after it) |
| Forked child of a failed spawn | stage 4 | std | 1 | reaped by std before `spawn` returns (F5) |
| Leader process | stage 4 | the CLI; `UnixChild` | 1 | exit or `kill_tree` |
| Leader zombie | leader exit | `UnixChild` | 1 | `Drop` (`waitpid`) while owned; left unreaped if alive at `Drop` (one per abandoned launch, until the daemon exits) |
| `SPAWN_LOCK` | process | `pty-runtime` | 1 | held for one `Command::spawn` at a time |
| Kill targets | `kill_tree` | `KillState` | identities of the tree not yet read resolved | removed at the read that resolves them; the `UnixChild` |
| Known zombies | each call | call | O(processes) | return |
| Pass buffers | each pass | call | O(processes) | return |

**Endings.**

| Ending | What happens |
|---|---|
| Validation failure | `NeverStarted`, nothing created. |
| Failure at stage 1–3 | Created fds dropped (the reader first when the drain cannot start); `NeverStarted { Setup }`, `Confirmed`. |
| Failure at stage 4 | The program never ran and std reaped the forked child. Slave and writer dropped, control closed, drain finished within 2 s; `NeverStarted`, `Confirmed` or `drain_abandoned`. |
| Failure at stage 5 | `SpawnedParts::teardown`: `kill_tree(&[], 5 s)` on the owned group, writer dropped, control closed, drain finished within 2 s; `StartedThenFailed` with its `Cleanup`. |
| Child exit | `wait_timeout` reports it, repeatably; the leader stays a zombie; the drain reads on until the last slave holder closes (EOF). |
| `kill` | §2.5 within the deadline; I/O stays up. |
| `close` | PR2 §2.2 order: kill; input released (a write blocked in step 3 ends at step 4's wake with its partial count); control closed (wake; never blocks); drain finished (EOF, else the wake ends it); threads joined. |
| `Session` drop | As close; the last `Arc<UnixChild>` reaps an exited owned leader. |
| Reader EOF / `EIO` | `OutputEnd::Eof`. |
| Wake without EOF | `OutputEnd::Error("pty closed before eof")` after at most 1 MiB. |
| Write error | After progress: the count now, the error on the next call; else the error (unit `Incomplete`, `InputBroken`, PR2 §2.3). |
| Ownership lost | `WaitFailed` if seen by `wait_timeout`; numeric group and session targeting disabled; `tree_dead: false` from then on; the backend never reaps the pid. |
| Panic | No panic path in this backend's code; a poisoned `kill` mutex is taken over. |

**Waits.** Every wait has a deadline except two inside `spawn`, which §2.2 leaves without one: acquiring `SPAWN_LOCK` (held for one `Command::spawn` or one `with_spawn_lock` closure at a time) and std's exec handshake (it ends once the forked child's bounded setup reaches `exec`; under the spawn rule nothing else holds it open). The others: `wait_timeout` (its timeout, 10 ms steps, `EINTR` included); `kill_tree` (lock polling, phases, passes, and sleeps all end at `deadline`, with `now()` before every syscall; one in-flight syscall and scheduler latency can run past it); `Drain::finish` (the session's bound); `close` and `resize` (never wait on another process); `UnixChild::drop` (`NOHANG`); spawn's failure teardown (5 s + 2 s). The reader's `poll` and the writer's `poll(POLLOUT)` have no timeout of their own; the wake ends both.

## 6. Purpose Layer and Plan Layer

### 6.1 Purpose Layer: Acceptance Criteria

Checked on a real macOS host (and `macos-latest` CI) by `cargo test -p chaldea-testkit --test pty_unix` and `cargo test -p chaldea-pty-runtime`; a manual run can watch `ps -axo pid,ppid,pgid,tpgid,stat,command | grep chaldea-fake-cli` before and after a kill.

| ID | Criterion (EARS) | Source | How to observe on a real macOS host |
|---|---|---|---|
| PR3-P-1 | When a CLI tree is killed, no process in the CLI's session, no live-observed descendant of a covered process, and no verified `extra` identity shall be alive at the deadline, `tree_dead` shall be true, and an unrelated process shall still answer; a `setsid` escapee whose parent exited before any observation is outside this guarantee unless named in `extra`, and so is an unknown session member that the kernel leaves out of a process list truncated by concurrent process creation (more than about 20 new processes between XNU's sizing and its lock, §2.5); the leader and every recorded target are rechecked by identity, so they cannot be missed that way. Also outside it: a process of another user inside the session whose pid reuses one seen earlier in the same call as a foreign zombie and that forks and exits within one pass (§2.5). | mvp §4 PR 3 obligation; spike B-7, §4.6 | FakeCli `spawn-tree`: every role's stream closes; `unrelated` answers `ping` after the kill; the orphan survives without `extra` and dies with it. |
| PR3-P-2 | The backend shall signal by process group or session only while it owns the leader's wait status and the leader's start matches, and by pid only after a lookup showed the recorded start; the remaining window between that lookup and the signal is stated, not closed. One exception: when stage 5 could not read the leader's start, its teardown signals the group while the wait status is still owned, with no start to compare (§2.4). | spike §4.6; §2.5 | A stale `extra` (the unrelated pid with another start) leaves it alive while a matching `extra` in the same call dies; after the leader is reaped outside the backend, the report is `unix_ownership_lost`, `tree_dead: false`. |
| PR3-P-3 | When a spawn fails, the error shall say whether the program could have run, no fd or process of the spawn shall remain when it returns, a concurrent child creation shall not hold it open, and a cleanup that could not be proven shall be reported `Unresolved`. | PR2 §2.1 | Missing program, missing interpreter, missing cwd: `NeverStarted`; fd set unchanged; `pgrep -P` empty at once; a stage 5 failure with no kill budget reports `Unresolved`. |
| PR3-P-4 | When a spawned CLI's program runs, it shall run as a session leader with the PTY as controlling terminal on fds 0–2 and no other fd, with default signal state, at the requested size, cwd, and exact environment passed to exec, receive `SIGWINCH` on resize, receive written bytes intact (byte for byte while its terminal is in raw mode; canonical mode discards input past its line limit, F14), and receive no byte when the input is dropped. A child that dies before exec is reported as an exited leader of a successful spawn (§2.2). | PR2 §2.1 | FakeCli hello and `winch` events; `/usr/bin/env`, `/bin/pwd`, `/bin/stty size` output; `echo` reports. |
| PR3-P-5 | When the tree has exited or the PTY is closed, the drain shall end (EOF, or a reported error when a holder survives), a write blocked on a CLI that stopped reading shall end at close with its partial count recorded, and `close` shall return within its budgets. | PR2 §2.2, §2.4 | `CloseOutcome`: `output`, `released.input`, `abandoned` empty, `elapsed`. |
| PR3-P-6 | Every `unsafe` block shall be in `pty_runtime::unix::sys`, and every other crate shall keep `forbid(unsafe_code)`. | PR2 D7; mvp §3 | `tests/lints.rs`; clippy. |
| PR3-P-7 | The workspace shall build and pass on `windows-latest` with the native-backend tests removed by `cfg`. | mvp §4 | The `workspace (windows-latest)` job. |

### 6.2 Plan Layer: Claims

| ID | Claim (EARS) | Source | Falsifying test |
|---|---|---|---|
| PR3-CL-1 | If validation fails, then `spawn` shall return `NeverStarted` with the kind of §2.2's table and create nothing. | PR2 §2.1; §2.2 | `tests/unix_spawn.rs`: `missing_program_is_not_found`, `unsearchable_directory_is_permission_denied` (a temp dir with mode 000), `nul_in_argv_is_setup`, `unexecutable_file_is_permission_denied`, `relative_argv0_is_setup`, `missing_cwd_is_setup`, `bad_env_is_setup` (duplicate key, `=` in key, NUL); unit `unix::tests::invalid_spec_creates_nothing` (NUL in argv, relative `argv[0]`, `=` in an env key: `NeverStarted { Setup }` and no stage hook reached, so no PTY, drain, or process) |
| PR3-CL-2 | If the forked child fails before the program runs, then `spawn` shall return `NeverStarted` with the errno's kind, with no process of the spawn left when it returns, and `Cleanup::Confirmed` with no fd of the spawn left when the drain ended within its 2 s bound (else `Unresolved` with `drain_abandoned`, the drain's master duplicate still held by its thread). | PR2 §2.1; F5 | `tests/unix_spawn.rs`: `missing_interpreter_is_never_started` (an executable `#!/nonexistent/interp` script: `Setup`, `ENOENT`), `failed_spawns_leave_no_fd` (`/dev/fd` before and after), `failed_spawns_leave_no_child` (`/usr/bin/pgrep -P <test pid>` empty right after `spawn` returns); unit `unix::tests::cwd_removed_after_validation_is_never_started` (a stage hook removes the dir between validation and stage 4: `NeverStarted { Setup }`, the program's marker file absent), `pre_exec_failure_is_never_started` (a stage hook replaces the stdio with `/dev/null`, so `TIOCSCTTY` fails `ENODEV`: `NeverStarted { Setup }`), `vanished_symlink_target_is_not_found` (`argv[0]` a link to a link to `/usr/bin/true`; the hook after validation removes the middle link: `NotFound`, `ENOENT`, `Confirmed`), `enoent_is_not_found_only_when_the_program_is_missing` (`ENOENT` with `argv[0]` in a directory without search permission: `Setup`; a missing path: `NotFound`) |
| PR3-CL-3 | If a stage after the PTY is opened fails, then `spawn` shall release in the order kill, input, control, drain finish, return `StartedThenFailed` once a process exists, and report `Confirmed` only when the tree is proven dead and the drain ended. | PR2 §2.1 | unit `unix::tests::drain_start_failure_releases_everything` (the milestones read validation, then the wake pipe's constructor returning, and never the lock-acquired hook of child creation; no child remains), `identity_failure_kills_the_started_tree` (`/bin/sh -c "trap '' HUP; sleep 30 & echo $!; exec sleep 30"`: both ignore the hangup that closing the master sends the session, so only the kill under test ends them; the stage 5 hook waits until the drain has seen the `$!` line and both pids are live, then fails: both pids dead, `Confirmed`, and the `cfg(test)` release trace of `UnixChild::kill_tree`, `MasterWriter::drop`, `UnixControl::close`, and the `Drain::finish` call in `SpawnedParts::teardown` (a crate-internal test trace point beside that call) reads exactly in that order), `zero_kill_budget_reports_unresolved` (same, with the teardown's kill budget forced to 0: `Unresolved` with a `kill` report, no `killpg` or `kill` in the `cfg(test)` trace at the backend's signal calls, and both pids still live afterwards) |
| PR3-CL-4 | When `spawn` returns, the PTY shall have `spec.size` with 8×16 px cells, set after the slave opened and before the program runs, and a resize shall reach the CLI as `SIGWINCH` with the same cell size. | PR2 §2.1; F1, F3 | unit `unix::tests::pty_opens_at_spec_size` (`/bin/stty size`); testkit `pty_unix::size_reaches_the_cli` (`attachable`: the first `size` event has `xpixel = 8 × cols`, `ypixel = 16 × rows`; after `resize` a `winch` event carries the new size) |
| PR3-CL-5 | The environment passed to exec shall be exactly `spec.env`, with nothing added or inherited, `SHELL` included (the dynamic loader may still remove `DYLD_*` variables from what some programs see). | PR2 §2.1; mvp §2.7 | `tests/unix_spawn.rs::env_is_exactly_the_spec` (`/usr/bin/env` with `A=1` prints only `A=1`; with an empty env prints nothing; the test process's `PATH` and `CARGO_PKG_NAME` absent; no `SHELL`) |
| PR3-CL-6 | When `spawn` succeeds and the child reaches exec, it shall run `argv[0]` in `spec.cwd` as a session and group leader whose controlling terminal is the PTY and whose group is in the foreground, on fds 0–2, with no other fd (one above a lowered soft limit included), an empty signal mask, no ignored signal, and no daemon handler run for a signal pending across the fork; a child that dies before exec (such a signal with a default action that ends it) is reported as an exited leader, not as a spawn failure (§2.2). | PR2 §2.1; F1, F4, F20, F21 | `unix::tests::cwd_is_honored` (`/bin/pwd` against the canonicalized temp dir); testkit `pty_unix::child_leads_its_session_on_the_pty` (hello: `pid == pgid == sid`, `ttys` three times the slave, `ctty`, `fg`, `fds == [0,1,2]`); helper-process tests in `unix::sys::tests` (§7): `child_gets_no_extra_fd_and_default_signals` (another thread opens a non-CLOEXEC fd at 200; the spawning thread blocks `SIGHUP`; `SIGINT` is ignored; a direct `/bin/sleep 30` child: `PROC_PIDLISTFDS` lists only 0–2, `SIGHUP` kills it, and a second child dies of `SIGINT`), `fd_above_a_lowered_limit_is_closed` (fd 400 opened without `FD_CLOEXEC`, then soft limit 128: the child lists only 0–2), `pending_signal_does_not_run_a_daemon_handler` (a `SIGWINCH` handler that blocks on a pipe, `SIGWINCH` blocked in the spawning thread, a `cfg(test)` flag makes `pre_exec` raise `SIGWINCH` at entry: `spawn` returns within 1 s and the child runs) |
| PR3-CL-7 | When the last process holding the slave exits, the drain shall end with `OutputEnd::Eof`; a read `EIO` shall map to EOF. | PR2 §2.1 | unit `unix::tests::exit_ends_output_with_eof` (`/bin/echo READY`: the sink gets `READY` first, then `Eof`, before `close`), `unix::io::tests::map_read_turns_eio_into_eof` (pure helper with a synthetic `EIO`; other errnos pass through) |
| PR3-CL-8 | When the reader observes the wake, it shall return at most 1 MiB more, checking the budget before each read, then end with an error that is not EOF, also when the source stays readable; a flush between `poll` and `read` shall not block it. | §2.3; F22 | `unix::io::tests::wake_caps_a_readable_source` (`MasterReader` over a 3 MiB regular file, always `POLLIN`, F22: with the wake set first, the reader returns exactly 1 048 576 bytes, then `Err("pty closed before eof")`; the control without the wake returns 3 MiB then `Ok(0)`), `wake_when_master_and_wake_are_both_ready_arms_the_cap`, `flush_between_poll_and_read_does_not_block` (F16 sequence, then the wake ends it), `wake_ends_the_reader_with_a_slave_open`; testkit `pty_unix::close_with_a_surviving_tty_holder_is_bounded` (a `sleeper` leader that survives the close, whose kill budget is 0, so it still holds the slave: `output == Some(Error)`, `abandoned` empty), `pty_unix::leader_exit_revokes_the_tty_for_every_holder` (`--escape-orphan --orphan-keeps-tty`: after the kill `output == Some(Eof)` and the orphan's stdin no longer answers `FIONREAD`, F24) |
| PR3-CL-9 | When the session input is dropped, no byte shall reach the slave. | PR2 §2.1 | `unix::io::tests::dropping_input_writes_nothing` (positive control first: writing `\n` + VEOF through the writer makes a canonical-mode slave read return the line and then 0; after that, with the slave switched to raw mode so any single byte would be readable, the drop leaves nothing readable within 200 ms) |
| PR3-CL-10 | A write shall return the whole length, or after progress the count accepted before a close or error (the error then on the next call), or fail without progress; the wake shall be checked before each further write, and a hangup while waiting in `poll` shall end the call without spinning. | PR2 §2.1, §2.3; F14, F15 | unit tests with the test holding a raw slave it never reads, each gated on the `cfg(test)` hook that the writer entered its full-queue `poll`: `blocked_write_ends_at_the_wake` (wake after the hook: `Ok(n < len)` within 1 s), `hangup_in_poll_ends_the_write` (slave closed after the hook: `Ok(n > 0)`, next call `EIO`, and at most one more entry into the `poll`, so no spinning); `wake_is_checked_between_writes` (a thread keeps reading the slave; the flag set mid-write: `Ok(n < len)`), `error_after_progress_returns_the_count_then_the_error` (an empty write after the short count returns the stored `EPIPE`, the next one `Ok(0)`); testkit `pty_unix::large_write_reaches_the_cli_whole` (one 256 KiB `Bytes` part with CJK and emoji to `echo` after its hello: reported bytes equal, `bytes_written` 262 144, `split_writes == 0`), `pty_unix::close_releases_a_blocked_writer` (`sleeper --raw`: the leader keeps the raw slave open and never reads; a 64 KiB unit is sent; close, with a kill budget of 0 so the leader still holds the slave (F24), is called once `fake::unix_writer_polls()` has increased; afterwards `released.input` is true and the unit's record is `Incomplete { error: "closed" }`, the wake and not a hangup, with `bytes_written > 0`) |
| PR3-CL-11 | `wait_timeout` shall return by its timeout and shall not wait for a running `kill_tree`. **Bound:** while a lookup on the kill thread holds XNU's reference to an exited, unreaped leader, a concurrent `waitid` waits in `waitidcoll` until that one `proc_pidinfo` call returns (microseconds); this delays `wait_timeout`, it does not block it. | PR2 §2.1 | `unix::tests::wait_timeout_returns_by_its_timeout`, `wait_timeout_runs_during_kill` (`sh` trapping HUP and TERM; a kill-entry hook signals that `kill_tree` holds its lock; then `wait_timeout(10 ms)` from another thread returns within 200 ms) |
| PR3-CL-12 | When the leader exits, `wait_timeout` shall report its code or signal on every call and leave it unreaped until the child is dropped, which reaps it with one `waitpid(NOHANG)` while owned; a leader still alive at drop shall be left unreaped. | §2.4 | `unix::tests::exit_reports_code_and_signal` (two calls, same value), `exit_is_observed_without_reaping` (before drop `lookup` is `Zombie`; after drop `Gone`), `live_leader_is_left_unreaped_at_drop` (`sh` trapping HUP and TERM, killed with an expired deadline, so alive at drop; the test then sends KILL to its pid: `lookup` stays `Zombie` for 500 ms; the test reaps it by exact pid) |
| PR3-CL-13 | When `kill_tree` runs while owned, it shall send HUP, TERM, KILL in that order to the group and the live targets, kill session members outside the group, live descendants of members and targets (escapees included, across calls), and report `tree_dead: true` only from a clean pass that read no covered process alive, after the recheck. | mvp §4 PR 3; prior-art row 2 | testkit `pty_unix::spawn_tree_dies_and_unrelated_survives` (roles `plain`, `ignore-term`, `double-forked`, `own-group`, `setsid`, `setsid-child`, each sending its own hello: exactly these roles with the expected pgid, sid, and ppid; each stream closes; `unrelated` answers `ping` afterwards); unit `unix::kill::tests::signals_go_in_order` (exact `Os` call sequence, `now()` included), `session_member_outside_the_group_is_killed`, `escapee_child_forked_after_its_parent_died_is_killed`, `child_listed_before_its_parent_is_covered` (the list reaches a recorded escapee's `setsid` child first: it is covered once the same pass reads the parent, and killed), `targets_survive_across_calls`, `clock_step_does_not_hide_a_child` (a child whose start stamp is before its parent's is still walked) |
| PR3-CL-14 | A pass shall be clean only if the list call succeeded with a nonzero, unsaturated result, the deadline did not cut it short, and every listed pid was resolved without churn under the census rule; only a clean, owned pass that read nothing covered alive, followed by a recheck that resolved every retained identity and every pid kept without a start, shall prove death; every read shall update the retained targets at once under the retention invariant (§2.5); another user's zombie confirmed by the short record shall count as a zombie; and a pass that read nothing alive with only churn shall be rescanned without a sleep. | §2.5 | unit `unix::kill::tests`, one interleaving per case, each where A is listed and has forked B (absent from the list) before A's outcome: `gone_at_lookup_is_unclean`, `new_zombie_is_unclean`, `known_zombie_keeps_the_pass_clean` (a zombie already seen earlier in the call is not), `esrch_at_getsid_after_live_lookup_is_unclean`, `esrch_at_getsid_after_unreadable_lookup_is_unclean`, `other_getsid_failure_is_unclean`; plus `saturated_list_is_retried_then_not_proof`, `zero_list_result_is_not_proof`, `deadline_cut_pass_is_not_proof`, `churn_is_rescanned_without_sleep` (no `sleep` call between two passes with an empty live set), `list_omitting_a_live_leader_is_not_proof` (an unsaturated list without the live leader: the recheck finds it, `tree_dead: false`), `member_is_retained_when_the_deadline_follows_the_ownership_check`, `member_read_before_getsid_is_retained`, `member_read_before_the_ownership_check_is_retained`, `descendant_of_a_tracked_parent_is_retained_when_read` (the deadline passes right after the census read a member, before its `getsid`, after it, or after the pass's ownership check; or right after it read the `setsid` child of a recorded escapee; then the process leaves the session and its parent exits between calls: the next call, with a complete list, still tracks it, `tree_dead: false`), `stale_extra_at_a_reused_pid_does_not_hide_a_live_member` (an `extra` naming an older process at the pid a live member now has: the read removes the stale generation and retains the member under its own start), `recheck_that_retains_a_new_generation_is_not_proof` (the same stale `extra`, the member omitted from every list, the leader exited: the recheck finds and retains the member, `tree_dead: false`), `resolved_child_waiting_for_its_parent_is_not_retained` (a child read before its recorded parent exits before its `getsid`: reading the parent does not retain it), `resolved_pending_parent_still_leads_to_its_child` (the list reaches a grandchild, then its parent, then the recorded escapee; the parent exits before its `getsid`: reading the escapee retains the grandchild and not the parent), `ownership_check_read_is_applied_to_the_targets` (the check at the call's start reads the exited leader and the deadline passes: the leader is no longer retained or reported), `member_needs_ownership_established_at_the_start` and `proof_needs_ownership_established_at_the_start` (the first ownership check fails without loss, later ones hold: a member of session `L` outside group `L` is neither retained nor signalled by pid, and a dead tree is not proven dead), `membership_needs_ownership_for_the_call` (ownership cannot be established: a live process in group and session `L` is neither retained nor signalled, `tree_dead: false`), `recheck_cut_after_finding_a_target_keeps_it` (a recorded escapee every list omits; the deadline passes right after the recheck found it alive: it is a survivor, and the next call with a complete list still tracks it), `absence_from_the_list_does_not_prune_a_target` (the same escapee after ownership loss, so the call returns before its deadline: the escapee stays a target and the next call names it, while the exited leader, a zombie by lookup, is pruned), `foreign_zombie_on_a_quiet_host_proves_death` (a short-record zombie in every pass: `tree_dead: true`; the same pid only `Unreadable` with `getsid` `ESRCH`: `tree_dead: false`; each pass costs 1 ms on the synthetic clock, so a regression ends at the deadline); `unix::table::tests::refused_record_is_a_zombie_only_when_the_short_record_says_so` |
| PR3-CL-15 | When an `extra` identity is live with its recorded start, `kill_tree` shall kill it and its descendants; a stale one shall not be signalled. | PR2 §2.1 | testkit `pty_unix::extra_reaches_an_orphan_and_spares_a_stale_identity` (one call with the orphan's matching identity and the unrelated process's pid with start + 1: the orphan and `orphan-child` die, the unrelated process answers `ping`; without `extra` the orphan survives first) |
| PR3-CL-16 | The backend shall send a group signal only right after `owned()` held; `owned()` shall return false without a wait call once `lost` is set, and shall set `lost` on `ECHILD` or when a waitable pid `L` shows another start; a per-pid signal shall follow a lookup of that pid showing the recorded start, with only a clock read between them, never to pid 0, 1, or its own; after loss `tree_dead` shall be false. | §2.4, §2.5 | `unix::child::tests` on the real `owned()` with a scripted `ChildOs`: `lost_makes_no_wait_call`, `echild_sets_lost`, `waitable_replacement_is_rejected` (`waitid` succeeds, `lookup(L)` shows another start: false, `lost` set), `matching_generation_is_owned`, `expired_deadline_is_not_owned_and_keeps_lost_clear`, `real_ownership_check_returns_the_leader_record` (the adapter over a real exited, unreaped child: owned, with its zombie record); `unix::kill::tests::ownership_is_checked_before_each_killpg`, `lost_ownership_sends_no_group_signal`, `lookup_immediately_precedes_kill`, `start_changed_before_the_signal_is_not_signalled` (the census saw the target; its start changes before the signal-time lookup: no `kill`), `reused_pid_in_extra_is_not_signalled` (with `matching_extra_is_killed` as its positive control), `init_and_self_are_never_signalled`; real-process `unix::tests::loss_before_observation` (the test reaps the exited leader by exact pid before any `wait_timeout`: `wait_timeout` errors, `kill_tree` reports `unix_ownership_lost`, `tree_dead: false`), `loss_after_cached_exit` (`wait_timeout` reports the exit, then the test reaps it: `kill_tree` reports `unix_ownership_lost`, `tree_dead: false`; `Drop` does not call `waitpid`) |
| PR3-CL-17 | `kill_tree` shall read the clock before every syscall (inside `owned()` and list retries included), and return by its deadline plus one in-flight syscall and the in-memory work that follows it, which makes no syscall and is bounded by the retained set and one pass (§2.5); an expired or busy call shall return an unresolved report without a pass or a blocking lock; it shall list survivors with `tree_dead: false` when it cannot prove death, unreadable members and targets included, kept across passes that could not complete (§2.5). | PR2 §2.1 | unit (synthetic clock) `kill_returns_by_its_deadline_with_survivors` (signals have no effect), `expired_call_makes_no_os_call`, `busy_lock_returns_by_the_deadline`, `lookup_consuming_the_budget_stops_before_the_signal` (the lookup that would authorise a per-pid signal shows the recorded start and ends past the deadline: no `kill`), `deadline_inside_owned_stops_before_killpg` (the ownership check before the first group signal holds, but its read of the leader ends past the deadline: no `killpg`; survivors: the leader the opening pass saw), `busy_and_expired_reports_list_each_pid_once` (`extra` repeating the leader and a pid twice), `deadline_between_list_retries_stops_the_pass`, `expired_call_keeps_a_new_extra_for_the_next_call`, `failed_list_keeps_the_last_survivors` (an unreadable session member, never a target; the list fails from the first signal on: survivors still include it), `member_first_seen_in_an_escalation_census_stays_reported` (an unreadable member appearing after the HUP; the list fails from the KILL on: survivors include it), `retained_escapee_omitted_from_a_later_list_stays_reported` (a recorded escapee listed only by the opening census; the deadline passes at the KILL: survivors include it), `stage_5_leader_found_by_the_recheck_keeps_its_start` (a leader without a recorded start that only the recheck reads, live: its survivor carries the start read), `deadline_exits_do_not_keep_confirmed_zombies` (an unreadable member keeps five calls from a proof; each names a new `extra` that is a zombie during the call: no call leaves it in `KillState`), `sleeps_are_capped_to_the_budget`, `unreadable_target_is_an_unresolved_survivor` (an `extra` outside the session turning `EPERM`), `small_budget_sends_kill_only` |
| PR3-CL-18 | Every `kill_tree` call shall compute its result from its own passes: an already dead tree shall return `tree_dead: true` from a clean opening pass with no signal and no wait, a live `extra` shall never be reported dead because an earlier call proved the rest, and targets shall be pruned and kept under the retention invariant (§2.5): pruned only after a direct lookup resolved them, and every identity observed live kept for the next call. | PR2 §2.1 | unit `unix::kill::tests::three_calls` (call 1 proves the tree dead; call 2 adds a live `extra` whose signals have no effect and times out with it as survivor; call 3 with no `extra` and an expired deadline still lists it: `tree_dead: false`), `dead_tree_returns_without_signal` (no `kill`/`killpg`, no `sleep`), `resolved_targets_are_pruned`; the retention tests of CL-14 |
| PR3-CL-19 | The leader's identity shall be read even when it has already exited, equal a later lookup while it lived, and `identity()` shall have no panic path when no record was read. | §2.2 stage 5; F10 | unit `unix::tests::immediate_exit_still_has_an_identity` (`/usr/bin/true` with a hook that waits for its exit before stage 5: `Ok`, then `Exited(0)`), `leader_identity_matches_lookup`, `identity_without_a_record_has_an_empty_start` (stage 5 hook fails the read: `identity()` gives the pid, the pid as group, and an empty `start_time`); `unix::sys::tests::libproc_records_read_zombies_and_another_users_process` (pid 1 → `EPERM`; an exited unreaped child → `Zombie`) |
| PR3-CL-20 | `#[allow(unsafe_code)]` shall occur exactly once in `crates/pty-runtime/src`, on `unix::sys`, with no inner `unsafe_code` attribute or `expect(unsafe_code)` anywhere, whatever comments or whitespace an attribute contains, and no `unsafe` block, fn, impl, trait, or extern outside `unix::sys`; the crate's clippy table shall contain every workspace key with the same value, and only the listed local additions; every unsafe block shall carry a `SAFETY` comment. | PR2 D7 | `tests/lints.rs::unsafe_is_allowed_only_in_sys` (attributes parsed after comments and string contents are blanked), `the_scan_sees_through_comments_and_strings` (`#[allow(/* scope */ unsafe_code)]` is found; `unsafe {` inside a string or comment is not; after `br#"""#`, `cr"\"`, and `c"x"`, the attributes that follow are still found), `inherited_lints_match_the_workspace`, `local_lints_are_the_listed_ones`; clippy `undocumented_unsafe_blocks = deny` |
| PR3-CL-21 | FakeCli shall start every PR 3 mode, report a hello with its pid, end a sleeper by `--max-life-secs`, have each spawned role send its own hello once its mechanism is in place, report `attachable` sizes as nulls on a pipe, and exit 2 on an unknown mode or flag. | mvp §3 lift table | `fake_cli::every_mode_reports_hello`, `spawn_tree_roles_report_directly`, `sleeper_ends_after_max_life` (`--max-life-secs 1`: the report stream closes no sooner than 1 s after the spawn began and within 1.5 s more for process start and teardown), `attachable_on_a_pipe_reports_null_size`, `unknown_mode_exits_2`, `unknown_flag_exits_2`, `queries_in_pipe_mode_reports_each_unanswered_query` (stdin a pipe closed at once: seven `ok: false` events within 10 s) |
| PR3-CL-22 | The VT responder's replies shall reach a CLI through the real PTY. | PR2 §2.5 | testkit `pty_unix::queries_are_answered_through_the_pty` (`--reply-timeout-ms 5000`: each of the seven query events `ok: true`, `all_ok`) |
| PR3-CL-23 | `chaldea-testkit` shall have no normal dependency on a workspace crate other than `chaldea-core`, on any target. | mvp §3 | CI step (§7; `cargo tree --target all`, so a target-specific dependency counts) |
| PR3-CL-24 | While a PTY spawn is inside `Command::spawn` or a `with_spawn_lock` closure runs, no other child creation through `SPAWN_LOCK` shall start, and the wake pipe shall be created under the same lock. | §2.1; F19 | `unix::sys::tests::pty_spawn_waits_for_a_helper_spawn` (a `Command::spawn` inside `with_spawn_lock` whose child parks in its `pre_exec` on a gate pipe holds the lock; a PTY spawn started on another thread does not reach its `cfg(test)` lock-acquired hook until the gate opens), `helper_spawn_waits_for_a_pty_spawn` (the PTY spawn's child parks in `pre_exec` through a `cfg(test)` gate; a concurrent `with_spawn_lock` spawn does not fork until the gate opens, observed by `proc_listchildpids`), `wake_pipe_is_created_under_the_lock` (a `cfg(test)` constructor injected in place of `std::io::pipe` on the production path records that the lock is held when it runs, and that it runs once, before the lock-acquired hook; the lock is free afterwards). The function that creates the wake pipe also takes the spawn lock's guard. Removing the lock from either path, or taking it after `Command::spawn` starts, fails one of them |

**Obligation mapping** (PR2 §2.1 → claims): release on failure, `Unresolved` → CL-1, CL-2, CL-3; opened at `spec.size` → CL-4; environment → CL-5; drain from the first byte (PR 2's `Drain::start` at stage 3), `EIO` → EOF, parent slave dropped → CL-7, CL-8; dropping input writes nothing → CL-9; whole writes → CL-10; `wait_timeout` → CL-11, CL-12; `kill_tree` with `extra`, deadline, survivors → CL-13 to CL-18; `close` by deadline → CL-8, CL-10 (`close` never blocks); `evidence` → none on Unix (§2.2); `unsafe` placement → CL-20. PR 3 row obligation (survivors and negative control) → CL-13, CL-15, CL-16. The spawn rule (§2.1) → CL-24.

## 7. Tests and Completion Commands

**Where tests run.**

| Tests | File | OS | Processes |
|---|---|---|---|
| Kill engine | `src/unix/kill.rs` `#[cfg(test)]` | macOS | none: synthetic `Os` and clock |
| Reader, writer, wake, `map_read` | `src/unix/io.rs` `#[cfg(test)]` | macOS | real PTY, a regular file |
| `owned()` | `src/unix/child.rs` `#[cfg(test)]` | macOS | none: scripted `ChildOs` |
| `sys`: spawn hygiene, spawn lock, `bsd_info` | `src/unix/sys.rs` `#[cfg(test)]` | macOS | direct `/bin/sleep` children; process-wide state only in helper processes |
| Lookup classification, short record | `src/unix/table.rs` `#[cfg(test)]` | macOS | pid 1, a direct `/usr/bin/true` child |
| Spawn stages, exit, kill, ownership loss with system binaries and stage hooks | `src/unix/tests.rs` | macOS | real |
| Public-API spawn: failures, fd and child accounting, env | `tests/unix_spawn.rs` | macOS | real; every test holds the file's lock |
| Lints | `tests/lints.rs` | all | none |
| `FakePty` tests | PR2 §7 | all | none |
| FakeCli modes without a PTY | `crates/testkit/tests/fake_cli.rs` | all (Unix-only roles behind `cfg(unix)`) | FakeCli |
| Native backend + FakeCli | `crates/testkit/tests/pty_unix.rs` | macOS | FakeCli; every test holds the file's lock |

**Isolation.** Tests that count fds or children are in `tests/unix_spawn.rs`, a separate binary in which every test takes the same `static LOCK: Mutex<()>`, so no other thread of that process opens fds or spawns children meanwhile (the library's unit-test binary also runs `drain.rs` tests that open pipes on other threads). Tests that change process-wide state (signal dispositions, the soft fd limit, a fixed high fd) run in a helper process: the test re-executes the current test binary (`std::env::current_exe()`) with `--exact <its own name>` and a flag in that child's environment only, and the helper performs the setup and the assertions; the parent asserts the helper's exit status. No test changes its own environment or waits for any child but its own by exact pid. The real-process unit tests in `src/unix/` share one lock among themselves. The handshake race of F19 itself is not reproduced in the suite: std offers no point between `pipe(2)` and its close-on-exec call where a test could hold the window open (the probe interposed `pipe`); CL-24 instead checks the exclusion that prevents it.

**Timing.** No test synchronizes by sleeping. A test waits for a FakeCli hello or event (sent only once the fixture's mechanism is in place), for `SessionEvent::Exited`, for a sink sentinel, for a stage-hook signal, for a pong, or for a stream's EOF, each bounded by 10 s. Kill deadlines are 5 s; "returns by its deadline" allows 500 ms of slack on real processes and none on the synthetic clock. Survival is proven by `ping` after `kill_tree` returned. Stage hooks fire only after their milestone (both pids live and the `$!` line seen; the leader's exit observed; the writer inside its full-queue `poll`), so a fault never lands before the state it tests.

**Leaks.** Every sleeper runs with `--max-life-secs 60`; every testkit test holds a `Cleanup` that kills its fixtures by verified identity and its own `unrelated` child on any exit, an assertion failure included; dropping the `Session` closes it and kills the leader.

**Runner PTYs.** `posix_openpt` needs no controlling terminal. A runner without PTY access fails these tests; none of them skips.

**Completion commands**: those of PR2 §7, plus the testkit dependency check of CL-23 (the CI step's current form is in `docs/design/pty-windows.md` §7).

## 8. Decisions

**D1. The native Unix backend is macOS-only** (`cfg(target_os = "macos")`). It uses libproc and Darwin process facts, the spike was macOS-only, and no CI runs Linux. Linux builds `core`, `FakePty`, and testkit. Reopen when Linux becomes a host: a `/proc` table behind the same `Os` trait.

**D2. The backend owns the spawn and the PTY; portable-pty is not used.** `std::process::Command` under the process-wide spawn lock, with a `pre_exec` of `setsid`, `TIOCSCTTY`, a sweep of non-CLOEXEC fds up to the highest open fd, and a signal reset (dispositions before mask) gives a session leader with the PTY as controlling terminal, only fds 0–2, default signal state, exact `envp`, and `chdir`/`exec` failures as `Err` after std has reaped the child (F1, F4, F5, F20, F21). Every child of the daemon is created under the same lock, so none can hold a handshake open (F19); children created outside it are a stated constraint. `posix_spawn` cannot give a controlling terminal (F2). rustix opens the PTY with every fd close-on-exec from creation (F18). portable-pty's spawn path reports some exec failures as starts, runs the child in `$HOME` on a bad `cwd`, adds `SHELL`, and sweeps fds after fork including std's error pipe; its `openpty` leaves a CLOEXEC window.

**D3. Exclusive ownership of the leader's wait status is a precondition; its loss is detected and narrows the kill.** While owned, the unreaped leader makes group and session targets exact; ownership, including the leader's start, is re-checked before every numeric target. After loss only verified identities are signalled and death is never reported proven. Reaping happens only in `Drop`; a leader still alive then stays unreaped, one per abandoned launch.

**D4. The kill keeps its unresolved targets across calls, computes every result from its own passes, accepts a pass only under the census rule, escalates HUP → TERM → KILL with budget-capped waits, and accepts death only when a clean, owned pass (with ownership for the call) read nothing covered alive and the recheck after it leaves no retained identity and no pid kept without a start. Every read updates the retained targets at once, keyed by `(pid, start)`: a target leaves only at the read that resolves it, and every covered identity read live stays for the next call; the report is derived from them.** Its guarantee is bounded to the original session, observed descendants, and verified `extra`; the residual windows, the high-churn limit, the list-truncation limit, and the foreign-zombie limit of §2.5 are stated. No census beyond the list and the recheck is attempted: the list call is the only source of unknown members, and it can omit entries under concurrent process creation.

**D5. The master is nonblocking; reads and writes stop on a wake (a flag plus a pipe).** The seam keeps whole-write semantics with partial counts preserved; close ends a blocked read or write; output after the wake is best effort, at most 1 MiB.

**D6. `unsafe` lives in one module, `unix::sys`, under a crate-local `deny`**, with `undocumented_unsafe_blocks` and `multiple_unsafe_ops_per_block` denied and the restated clippy table checked by a test.

**D7. The public additions are `native()` and `with_spawn_lock()`.** The lock takes a closure, not a `&mut std::process::Command`, so `tokio::process` and process-wrap spawns run under it too. Process identity lookup stays internal, reachable by tests through the `fake` feature; the PR that first builds `extra` in the daemon adds the public lookup.

**D8. FakeCli is library code with a thin binary, reports over loopback TCP with `ping`/`pong`, and lifts a mode only with the PR that first tests it.** Its real-PTY tests are testkit integration tests, so Cargo locates the binary. `stall`, `banner`, hooks, the `mcp` role, and attach input parsing are still to come (§2.8).

**D9. Validation classifies early; the forked child decides.** The checks of §2.2 give typed errors; any failure found after the fork is still `NeverStarted`, because std reports it before `exec` succeeds and reaps the child.

## Related Text in Other Documents

These documents also state parts of this design; a change to it updates them too. The evidence for not using portable-pty is §3 "Spawn primitive" and "PTY opening" (F1–F5, F18).

- `AGENTS.md` invariant 3, PTY Seam: the macOS backend and the spawn lock.
- `docs/architecture.md`: Host Core & Daemon, and the `portable-pty` entry (no backend uses it).
- `docs/prior-art.md`: rows 1d and 2, and the closing notes on process-tree libraries.
- `docs/mvp-design.md`: §3 layout, dependency direction (`testkit` may depend on `core` only), and the FakeCli lift table; §4 PR 3 row, and the PR 8 and PR 10 obligations (every daemon child created under the spawn lock, exclusive ownership of each PTY child's wait status, async-signal-safe handlers).
- `docs/design/pty-runtime.md`: §1 dependencies, §2 threading model, §2.1 rules, D7, and the `SpawnError::NeverStarted` wording.
- `crates/pty-runtime/src/backend.rs`: the `SpawnError::NeverStarted` doc comment.
