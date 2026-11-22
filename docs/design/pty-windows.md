# Design: Windows ConPTY Backend (MVP PR 4)

Status: design, not implemented. Scope is the PR 4 row of `docs/mvp-design.md` §4 (cited as "mvp §N"): the Windows backend of `PtyRuntime`, an own `windows-sys` backend with `CreatePseudoConsole` flags `0` from `kernel32.dll` only, `CreateProcessW` with `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` and `PROC_THREAD_ATTRIBUTE_JOB_LIST` (the child is in a kill-on-close job from creation), `CREATE_SUSPENDED` + assign + resume as the fallback when the job list is rejected, stop by job termination plus the recorded-descendant kill, and a bounded pseudoconsole close. Its design obligations: FakeCli `spawn-tree` survivors and the unrelated negative control on Windows; injected setup failures leave no process or handle (spike F-2c); the job-list branch actually taken is recorded. It also adds the FakeCli features PR 3 deferred to PR 4 (`docs/design/pty-unix.md` §2.8): the `breakaway` role, the Windows console size for `attachable`, and `--emit-output` for the F-2c cases.

The contract implemented is `docs/design/pty-runtime.md` (cited as "PR2 §N") §2.1 "What each backend must supply"; the sibling backend is `docs/design/pty-unix.md` (cited as "PR3 §N"), whose spawn rule (PR3 §2.1) and ownership rule (PR3 §2.4) this backend follows in their Windows form. No trait or public type of PR 2 or PR 3 changes; additions are new `fake`-feature functions, new FakeCli flags, modes, and events, one additive `Cleanup` method, one new value of `KillReport::method`, and crate-internal changes to `drain.rs` (the close worker replaces `close_bounded`) and `spawn.rs` (a guard type for the spawn lock).

Out of scope: daemon wiring and the daemon-in-a-job check (PR 8), input encoders including `win_keys` (PR 5), the gate and send path (PR 10), attach (PR 11), and Codex hook trust (PR 14).

Sources read for this document (paths are relative to each root):

| Source | Revision | Read for |
|---|---|---|
| This repository | `6f81b14` | `crates/pty-runtime/src/{backend,spawn,drain,session,lib,fake}.rs`, `src/unix/{mod,tests}.rs`, `tests/lints.rs`; `crates/testkit/src/{report.rs,fake_cli/*}`; `Cargo.toml`, `Cargo.lock`, `.github/workflows/ci.yml`, `lefthook.yml`; spike `spikes/pty-hooks/src/os/windows.rs`, `Cargo.toml`; `docs/spikes/pty-hooks-spike.md` §3, §4.6, F-2a–F-2c; `docs/prior-art.md` rows 1a, 1b, 1d, 3b, 8, 9; `docs/architecture.md` §5 (Windows paragraphs) |
| Rust std | 1.96.0 `rust-src` | `sys/process/windows.rs` (command line, environment block, `inherit_handles`, `CREATE_PROCESS_LOCK`), `sys/args/windows.rs` (argument quoting), `sys/pipe/windows.rs` (`io::pipe`), `sys/pal/windows/handle.rs` (`ERROR_BROKEN_PIPE` on read), `os/windows/io/handle.rs` (`PipeReader`/`PipeWriter` from `OwnedHandle`) |
| `windows-sys` | crate 0.61.2 (in `Cargo.lock`) | `Win32/{Foundation,System/{Console,JobObjects,Threading,Pipes}}/mod.rs` (declarations, constants, feature gates) |
| `windows-link` | crate 0.2.1 (in `Cargo.lock`) | `src/lib.rs` (the `link!` macro) |
| `winapi-util` | crate 0.1.11 (in `Cargo.lock`, through `termcolor`) | `src/console.rs`, `src/win.rs` (safe console mode and screen buffer info) |
| `portable-pty` | crate 0.8.1 | `src/win/{psuedocon,mod,procthreadattr}.rs` |
| `alacritty_terminal` | crate 0.26.0 (in `Cargo.lock`) | `src/tty/windows/{conpty,child}.rs` |
| Codex (`openai/codex`, sparse `codex-rs/utils/pty`) | `cf3ff15` (default branch 2026-11-22) | `src/win/{psuedocon,procthreadattr,job,mod}.rs` |
| Windows Terminal (`microsoft/terminal`, sparse `src/winconpty`) | `cf5e6bf` (2026-11-20) | `winconpty.cpp` (the `CreatePseudoConsole` implementation; its comment at line 225 notes that it is built into kernelbase) |
| Microsoft Learn | 2026-11-22 | "Nested Jobs" (`procthread/nested-jobs`, page dated 2025-07-14) |

**Facts from source and documentation** this design relies on (W). Every behaviour that only a Windows host can show is an assumption (A, §6.1) checked by a named test on `windows-latest`. The source establishes mechanisms; the tests establish what Chaldea's code does with them.

| # | Fact | Where |
|---|---|---|
| W1 | `windows-sys` declares `CreatePseudoConsole`, `ResizePseudoConsole`, `ClosePseudoConsole` as `kernel32.dll` imports through `windows_link::link!`, which expands to `#[link(name = "kernel32.dll", kind = "raw-dylib", modifiers = "+verbatim")]`: the binary's import table names `kernel32.dll` itself. `kernel32.dll` is a KnownDLL, mapped from the system directory and never searched for (Microsoft, "Dynamic-link library search order"). No `ReleasePseudoConsole` is declared. | windows-sys `Win32/System/Console/mod.rs:5`, `:9`, `:65`; windows-link `src/lib.rs:20-27` |
| W2 | `std::io::pipe()` on Windows is `CreatePipe(&r, &w, NULL, 0)`: synchronous, non-inheritable handles. `PipeReader::read` maps `ERROR_BROKEN_PIPE` to `Ok(0)`. Both ends convert from and to `OwnedHandle`. | std `sys/pipe/windows.rs:8-19`, `sys/pal/windows/handle.rs:173-180`, `os/windows/io/handle.rs:685-704` |
| W3 | `CreatePseudoConsole` duplicates `hInput` and `hOutput` as **inheritable** handles and makes its signal pipe end inheritable, then starts conhost with `bInheritHandles = TRUE` restricted by `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`; the duplicates are closed when the call returns. A concurrent `CreateProcess` elsewhere in the process with `bInheritHandles = TRUE` and no handle list inherits them during the call. | winconpty `winconpty.cpp:160-167`, `:236-269`, `:494-498` |
| W4 | conhost keeps running while the pseudoconsole's `\Reference` handle is open, so with the host's copies of conhost's pipe ends closed, the output pipe's last writer is conhost, which closes it when the pseudoconsole is released (`ReleasePseudoConsole` or `ClosePseudoConsole`) or when conhost dies. The open-source close only closes handles; Microsoft's `ClosePseudoConsole` documentation says the call can wait until the final frame on the output pipe is drained, indefinitely if nobody reads it, and that closing sends `CTRL_CLOSE_EVENT` to every process attached to the console. | winconpty `:558-576`, `:395-432`, `:585-589`; Microsoft, "ClosePseudoConsole function"; alacritty `tty/windows/conpty.rs:97-104` |
| W5 | Flags `0` start conhost without `--inheritcursor`, so it sends no startup cursor query that would wait for a reply. | winconpty `:169`, `:196` |
| W6 | std spawns default to `bInheritHandles = TRUE`; std's `CREATE_PROCESS_LOCK` serializes only std's own spawns. std sorts the environment block with `CompareStringOrdinal(…, TRUE)`, adds a second NUL for an empty block, strips a `\\?\` prefix from the cwd when the rest is absolute, always quotes the program name, quotes an argument only when it is empty or contains a space or tab, doubling backslashes before a quote, and builds the command line afresh for each spawn. | std `sys/process/windows.rs:74-93`, `:193`, `:330-341`, `:859-875`, `:887-911`, `:914`; `sys/args/windows.rs:173-212` |
| W7 | Codex requires the job list and has no fallback (`procthreadattr.rs:85-88`); its job sets `JOB_OBJECT_LIMIT_BREAKAWAY_OK` (`job.rs:79-82`); its background launcher resumes a child uncontained when assignment fails (`job.rs:169-196`); "TerminateJobObject can empty the job list before members release their handles" (`job.rs:286`); member lists are sized by `ERROR_MORE_DATA` retries (`job.rs:337-365`); `TerminateProcess` fails `ERROR_ACCESS_DENIED` on an exited process (`job.rs:367-374`). It uses flags `RESIZE_QUIRK` (`psuedocon.rs:62`, `:144`), loads the functions from `kernel32.dll` by name (`:92-98`), keeps its copies of conhost's pipe ends until the child exists (`:116-121`, `:221`), and calls `ReleasePseudoConsole` where exported (`:222-230`). | Codex `utils/pty/src/win/` |
| W8 | portable-pty 0.8.1 prefers a `conpty.dll` found on the DLL search path (`psuedocon.rs:44-59`), passes flags `0x6` (`:86`), and builds a one-attribute list (`:124`); alacritty 0.26.0 also prefers `conpty.dll` (`tty/windows/conpty.rs:31-90`), passes flags `0` (`:123-130`) and `bInheritHandles = FALSE` (`:219-230`). | crates as listed |
| W9 | The spike's raw launch closes its copies of conhost's pipe ends right after `CreatePseudoConsole` (`os/windows.rs:1728-1731`), creates the job without `BREAKAWAY_OK` (`:242-260`), retries a breakaway spawn without the flag on `ERROR_ACCESS_DENIED` (`:456-466`), passes `STARTF_USESTDHANDLES` with `INVALID_HANDLE_VALUE` standard handles (`:1780-1786`; Codex `psuedocon.rs:172-177`), copies the command line for each `CreateProcessW` attempt (`:1817`), accepts the suspended branch only when `ResumeThread` returns 1 (`:1860-1871`), and releases a failed setup as job and process, pseudoconsole (bounded), pipes, drain (`:1454-1512`). On the spike host every uninjected raw launch took the job-list branch, and F-2c handle counts settled after a warm-up (arch §5, Windows PTY paragraph). | spike |
| W10 | `winapi-util` 0.1.11 offers safe `console::mode`, `console::set_mode`, and `console::screen_buffer_info(..).window_rect()` over standard handles. | `src/console.rs:31-95`, `:139-151`; `src/win.rs:124-135` |
| W11 | Nested jobs (Windows 8 and later): a process already in a job that is added to another job is nested "if the system can form a valid job hierarchy and neither job sets UI limits"; a process created by a job member is associated with every job in its parent's chain; "if the immediate job object allows breakaway, the child process breaks away from the immediate job object and from each job in the parent job chain, moving up the hierarchy until it reaches a job that does not permit breakaway", and if the immediate job does not allow breakaway the child does not break away; a job's accounting covers the processes of its child jobs; terminating a job terminates the processes of its child jobs. | Microsoft, "Nested Jobs" |
| W12 | `close_bounded` (`drain.rs:147-162`) returns `None` and lets std drop the closure on the caller's thread when the thread cannot be spawned, starts its receive timeout after the spawn, and never joins the thread. `Drain` exposes only `wait_delivered(n, deadline)` under `cfg(any(test, feature = "fake"))` (`drain.rs:90`). `ProcessIdentity` derives `Debug, Clone, PartialEq, Eq, Hash` only (`backend.rs:143-149`). `CloseOutcome::abandoned` names `pty-writer`, `pty-drain`, `pty-wait`, and `Released::control` is `close.elapsed.is_some()` (`session.rs:163-166`, `:892-912`). `Cleanup::track` records a start only through the `Identify` function given to `Cleanup::new` (`report.rs:357-374`). | this repository |

## 1. Goal and Files

**Goal.** A Windows backend that opens a pseudoconsole at the requested size from the inbox `kernel32.dll` with its close worker already running, drains its output from before the child exists, starts the CLI with exactly the requested argv, cwd, and environment inside a kill-on-close job from its creation (job list, else suspended + assign + resume, else no launch), records which branch ran, kills the tree by job termination plus verified recorded descendants and proves death from the job's own state and the handles it holds, closes the pseudoconsole within the caller's deadline, and releases everything on every failure path or reports what it could not prove. FakeCli gains what the Windows tests need. Every claim of §6 is checked by a named test on `windows-latest` with a real ConPTY; `macos-latest` builds everything and runs the platform-independent parts.

**Files.**

```
Cargo.toml                                  # [workspace.dependencies] += windows-sys (features), winapi-util
crates/pty-runtime/Cargo.toml               # [target.'cfg(windows)'.dependencies] windows-sys
crates/pty-runtime/src/lib.rs               # #[cfg(windows)] mod windows; pub use windows::native; dead_code allowance narrowed
crates/pty-runtime/src/spawn.rs             # SpawnGuard (§2.1); spawn_lock_held() also for cfg(all(test, windows))
crates/pty-runtime/src/unix/mod.rs          # wake_pipe takes &SpawnGuard instead of &MutexGuard<'static, ()>
crates/pty-runtime/src/drain.rs             # CloseWorker replaces close_bounded (§2.3); test/fake marker watch (§2.6)
crates/pty-runtime/src/backend.rs           # teardown trace point also for cfg(all(test, windows))
crates/pty-runtime/src/windows/mod.rs       # WindowsPty: PtyRuntime; validation; the spawn sequence (§2.2); native()
crates/pty-runtime/src/windows/cmdline.rs   # command line, environment block, cwd encoding (pure; unit tests)
crates/pty-runtime/src/windows/control.rs   # Pcon, WindowsControl: PtyControl (resize, close through the worker) (§2.3)
crates/pty-runtime/src/windows/child.rs     # WindowsChild: PtyChild; identity; wait_timeout (§2.4)
crates/pty-runtime/src/windows/kill.rs      # the kill engine over a `JobOs` trait; unit tests with a synthetic `JobOs` and clock (§2.5)
crates/pty-runtime/src/windows/sys.rs       # the only Windows module allowed `unsafe` (§2.7)
crates/pty-runtime/src/windows/hooks.rs     # #[cfg(any(test, feature = "fake"))] faults, gates, trace (§2.6)
crates/pty-runtime/src/windows/tests.rs     # #[cfg(test)] real-process unit tests with system binaries
crates/pty-runtime/src/fake.rs              # += #[cfg(windows)] routes of §2.6
crates/pty-runtime/tests/windows_spawn.rs   # #![cfg(windows)] public-API spawn tests; every test holds one lock
crates/pty-runtime/tests/lints.rs           # two allowances: unix::sys and windows::sys
crates/testkit/Cargo.toml                   # [target.'cfg(windows)'.dependencies] winapi-util
crates/testkit/src/fake_cli/{mod,term,spawn_tree,sleeper,attachable,echo}.rs   # Windows additions (§2.8)
crates/testkit/src/fake_cli/report_process.rs   # --mode report-process (§2.8)
crates/testkit/src/report.rs                # Cleanup::kill_with (additive)
crates/testkit/tests/fake_cli.rs            # Windows role tests without a PTY
crates/testkit/tests/pty_windows.rs         # #![cfg(windows)] native backend + FakeCli (§7)
.github/workflows/ci.yml                    # windows-latest: a step that fails when the Windows tests are not compiled in
```

No Unix file moves. `crate::windows` is a module of this crate; the external crate is always named `windows_sys`.

**Dependencies.** Exact pins; the lock gains no package (`windows-sys 0.61.2` and `winapi-util 0.1.11` are already locked; features only add declarations).

```toml
# root [workspace.dependencies]
windows-sys = { version = "=0.61.2", features = [
  "Win32_Foundation",          # HANDLE, FILETIME, CloseHandle, DuplicateHandle, error codes, WAIT_*
  "Win32_Security",            # gates CreateProcessW and CreateJobObjectW (SECURITY_ATTRIBUTES pointers)
  "Win32_System_Console",      # CreatePseudoConsole, ResizePseudoConsole, ClosePseudoConsole, COORD, HPCON
  "Win32_System_JobObjects",   # job create, limits, UI limits (tests), assign, terminate, accounting, pid list, IsProcessInJob
  "Win32_System_Threading",    # CreateProcessW, attribute list, OpenProcess, GetProcessTimes, waits, ResumeThread
  "Win32_System_LibraryLoader",# GetModuleHandleW, GetModuleFileNameW (the conpty_module evidence)
  "Win32_System_Pipes",        # PeekNamedPipe (test helper for CL-21)
  "Win32_Globalization",       # CompareStringOrdinal (environment key order and equality)
] }
winapi-util = "=0.1.11"

# crates/pty-runtime/Cargo.toml
[target.'cfg(windows)'.dependencies]
windows-sys.workspace = true

# crates/testkit/Cargo.toml
[target.'cfg(windows)'.dependencies]
winapi-util.workspace = true   # console mode and size without `unsafe` in testkit
```

The pipes themselves come from `std::io::pipe` (W2). No Toolhelp feature: the backend never walks the process list.

**Workspace and CI.** `cargo test --workspace --locked` on `windows-latest` now runs the `#[cfg(windows)]` real-ConPTY tests (pty-runtime unit tests, `tests/windows_spawn.rs`, testkit `tests/pty_windows.rs`); on `macos-latest` `cfg` removes them. One step is added to the `workspace` job, on Windows only, `List the Windows tests`: for each Windows test binary it runs `-- --list` and requires named tests, so a `cfg` mistake that compiles any of them out fails CI instead of passing silently:

| Binary | Command | Must list |
|---|---|---|
| pty-runtime unit tests | `cargo test -p chaldea-pty-runtime --lib --locked -- --list` | `windows::tests::dropping_the_child_kills_through_the_job`, `windows::kill::tests::job_empty_and_handles_signalled_proves_death` |
| public-API spawn tests | `cargo test -p chaldea-pty-runtime --test windows_spawn --locked -- --list` | `env_is_exactly_the_spec` |
| FakeCli tests | `cargo test -p chaldea-testkit --test pty_windows --locked -- --list` | `spawn_tree_dies_and_unrelated_survives` |

`lefthook.yml` is unchanged: Windows clippy runs in CI (§7). `lib.rs`'s `allow(dead_code)` for non-macOS default builds becomes `all(not(feature = "fake"), not(any(target_os = "macos", windows)))`.

## 2. Types and Functions

Names may change in review; shapes and rules may not.

### 2.1 Public Additions and the Process Rules in Windows Form

```rust
/// The native backend of this OS (macOS: PR 3; Windows: this document).
#[cfg(windows)]
pub fn native() -> Arc<dyn PtyRuntime>;

// spawn.rs, crate-internal
pub(crate) struct SpawnGuard(MutexGuard<'static, ()>);   // private field: only spawn_lock() builds one
pub(crate) fn spawn_lock() -> SpawnGuard;                // was MutexGuard<'static, ()>
```

`with_spawn_lock` is unchanged. A `SpawnGuard` can come only from `SPAWN_LOCK`, so a function that takes `&SpawnGuard` cannot be called under some other mutex (a plain `MutexGuard<()>` could come from any). The macOS backend's `wake_pipe` takes `&SpawnGuard` the same way; its behaviour is unchanged.

**The spawn rule on Windows.** Every child the daemon creates is created under `SPAWN_LOCK` (PR3 §2.1). On Windows the hazard is handle inheritance, not a fork: an inheritable handle that exists while another thread runs `CreateProcess` with `bInheritHandles = TRUE` and no handle list goes to that unrelated child. std's own lock covers only std spawns (W6), and `CreatePseudoConsole` itself holds inheritable duplicates of conhost's pipe ends while it starts conhost (W3); an unrelated child that inherits the output pipe's write end keeps the session's output from reaching EOF until that child exits. So this backend holds the lock **around `CreatePseudoConsole`** (the inheritance window W3 establishes) and **around each `CreateProcessW` attempt** (the uniform rule that every child is created under the lock; the call creates no inheritable handle, so this costs only the call's duration). It does not hold the lock for `AssignProcessToJobObject` and `ResumeThread`: a child created suspended runs no instruction until resumed, and neither call creates a handle, so another spawn in between changes nothing. Every wait and release of a failure path (stage 5.4's bounded wait, `release_early`) runs without the lock, so a failing spawn cannot stall other spawns. `sys::create_pseudo_console` and `sys::create_process` take `&SpawnGuard` (§2.7). Everything else the backend creates is non-inheritable from creation (`std::io::pipe`, `CreateJobObjectW(NULL, NULL)`, handles from `CreateProcessW`, `OpenProcess`, and `DuplicateHandle` with `bInheritHandle = FALSE`), and its own `CreateProcessW` passes `bInheritHandles = FALSE`, so it neither gives nor takes an inherited handle. **Constraint:** a library that creates children outside the lock with `bInheritHandles = TRUE` can still catch W3's duplicates; the daemon uses none (a PR 8 / PR 10 obligation, as on macOS).

**Ownership on Windows.** The backend holds the leader's process handle from `CreateProcessW` until the `WindowsChild` drops. Windows has no reaping: the exit code stays readable through that handle, and the process object, with its pid, cannot be reused while the handle is open. So ownership cannot be lost to another waiter, and no `lost` state exists. The one daemon-side rule is that no code closes a handle it did not open (every handle is an `OwnedHandle`; no `CloseHandle` on a raw value), since closing the backend's handle by mistake could free the pid.

**Identity format** (`ProcessIdentity`): `pid`; `start_time` the process creation time from `GetProcessTimes` on a handle to that process, a `FILETIME` as decimal 100-ns units since 1601-01-01 UTC, used only for equality; `group` is `job:<leader pid>` for the leader and for job members the kill reports, and empty for a process outside the job, including a suspended child whose assignment failed (D12). The creation time is wall-clock: nothing orders processes by it.

### 2.2 Spawn

```rust
struct WindowsPty { #[cfg(any(test, feature = "fake"))] hooks: hooks::Hooks }   // sealed::Sealed + PtyRuntime
```

**Validation** (before anything is created; a failure is `NeverStarted` with `Cleanup::Confirmed`). It also builds the UTF-16 buffers, so no buffer error can happen after a resource exists.

| Check | Failure |
|---|---|
| `argv` non-empty; no NUL in any argument | `Setup` (`InvalidInput`) |
| `argv[0]` absolute and without `"` | `Setup` (`InvalidInput`) |
| `argv[0]` extension is not `.bat` or `.cmd` (ASCII case-insensitive): `CreateProcessW` would run them through `cmd.exe`, which re-parses the arguments. An adapter resolves a `.cmd` shim to its native `.exe`; passing the shim is `NeverStarted` | `Setup` (`InvalidInput`) |
| `metadata(argv[0])` | `NotFound` → `NotFound`, `PermissionDenied` → `PermissionDenied`, other → `Setup` |
| `argv[0]` is a regular file | `PermissionDenied` |
| `cwd` absolute and a directory | `Setup` (`NotFound` kind when missing) |
| a verbatim `cwd` (`\\?\` or `\\?\UNC\` prefix) names the same directory without its prefix: the stripped form `s` (`\\?\UNC\` becoming `\\`) must satisfy `std::path::absolute(s) == s`, so Win32 path normalization (which drops trailing dots and spaces, resolves `.` and `..`, and turns `/` into `\`) leaves it unchanged | `Setup` (`InvalidInput`) |
| env keys non-empty, without `=` or NUL; values without NUL; no two keys equal under `CompareStringOrdinal(…, TRUE)` | `Setup` (`InvalidInput`) |
| command line, NUL included, at most 32 767 UTF-16 units (Microsoft, "CreateProcessW function", `lpCommandLine`) | `Setup` (`InvalidInput`) |

**Buffers** (`cmdline.rs`, pure):

- *Application name*: `argv[0]` as given, NUL-terminated. `CreateProcessW` with a non-null `lpApplicationName` neither searches the path nor appends an extension (Microsoft, "CreateProcessW function").
- *Command line*: `argv[0]` always quoted, then each argument separated by one space and quoted by std's rule (W6): quoted when empty or containing a space or tab; inside, `n` backslashes before a `"` become `2n + 1` followed by `\"`, and `n` trailing backslashes before the closing quote become `2n`. These are the rules the MSVC CRT and Rust's own argument parser undo ("Parsing C++ command-line arguments"). `CreateProcessW` may modify `lpCommandLine`, so the buffer is kept immutable and `sys::create_process` copies it into a fresh mutable buffer for each attempt (W6, W9).
- *Environment block*: `spec.env` sorted with `CompareStringOrdinal(…, TRUE)` ("Changing environment variables": the block is sorted case-insensitively, in Unicode order, without regard to locale), each `KEY=VALUE\0`, then a final `\0`; an empty `spec.env` gives `\0\0`. Nothing is inherited: `CREATE_UNICODE_ENVIRONMENT` with this block.
- *Current directory*: `spec.cwd`, with a verbatim prefix removed under the validation rule above (W6: the current directory does not accept verbatim paths), NUL-terminated. A `cwd` without the prefix is passed as given.

**Sequence.** Each stage lists what exists when it fails. `release_early` (never under the lock) is: `control.close(now + 2 s)` (§2.3) while the drain reads, drop the input write end, `Drain::finish(2 s)`, drop the job; `Cleanup::Confirmed` when the close returned (pseudoconsole released, close worker joined) and the drain ended (EOF or read error, thread joined), else `Unresolved { kill: None, close: Some(..), drain_abandoned }`. **What `Cleanup::Confirmed` means here:** every handle, thread, and pseudoconsole this spawn created in the daemon's process is released, and no process of the spawn is left running that the backend did not see exit; it does not claim conhost's exit, which the backend holds no handle to observe.

1. **Pipes.** `std::io::pipe()` twice: `(in_r, in_w)` and `(out_r, out_w)` (W2). Failure: what exists is dropped; `NeverStarted { Setup }`, `Confirmed`.
2. **Pseudoconsole.**
   1. *Close worker.* `CloseWorker::start("conpty-close")` (§2.3), before the pseudoconsole exists. Failure: drop the pipes; `NeverStarted { Setup }`, `Confirmed`. No pseudoconsole was created, so nothing can be left without a closer.
   2. *Create*, under `SPAWN_LOCK`: `sys::create_pseudo_console(&guard, spec.size, &in_r, &out_w)` = `CreatePseudoConsole(COORD { X: cols, Y: rows }, in_r, out_w, 0, &hpc)`; the guard is dropped when the call returns. On success the `Pcon` is armed on the worker at once (`arm` cannot fail) and goes into a `WindowsControl` (§2.3), so no later path can forget it. Then `in_r` and `out_w` (the backend's copies of conhost's ends) are dropped: conhost holds its own (W3), and conhost is then the output pipe's last writer (W4). Record the `conpty_module` evidence: `GetModuleFileNameW(GetModuleHandleW("kernel32.dll"))`. Failure (HRESULT < 0, mapped to the Win32 code when its facility is `FACILITY_WIN32`, else `other`): `worker.close(now + 2 s)` (unarmed: the worker exits and is joined), drop the pipes; `NeverStarted { Setup }`, `Confirmed` when the worker was joined.
3. **Drain.** `Drain::start(out_r, output)` (PR2 §2.4), before any process exists (PR2 §2.1). Failure: `Drain::start` drops `out_r` first, so a waiting `ClosePseudoConsole` gets a broken pipe instead of an undrained one; then `control.close(now + 2 s)`, drop `in_w`; `NeverStarted { Setup }` with `Confirmed` when the close returned, else `Unresolved { kill: None, close: Some(..), drain_abandoned: false }` (no drain thread exists; output EOF is not observable on this path and not claimed).
4. **Job.** `sys::create_job()`: `CreateJobObjectW(NULL, NULL)` (unnamed, not inheritable), then `SetInformationJobObject(JobObjectExtendedLimitInformation)` with `LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and nothing else (no `BREAKAWAY_OK`, no `SILENT_BREAKAWAY_OK`; §3 "Breakaway"). Failure of either call (the job handle, if created, is dropped): `release_early`; `NeverStarted { Setup }`.
5. **Create.**
   1. *Job-list attempt.* `AttrList::new(2)`, `set_pseudoconsole(&pcon)`, `set_job_list(&[job])`; `STARTUPINFOEXW` with `cb`, `dwFlags = STARTF_USESTDHANDLES`, the three standard handles `INVALID_HANDLE_VALUE` (so the child's standard handles are the pseudoconsole's, never the daemon's, W9), `lpAttributeList`; under `SPAWN_LOCK`, `CreateProcessW(app, <fresh copy of cmdline>, NULL, NULL, FALSE, EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT, env, cwd, &si, &pi)`. Success: the child is a job member before its first instruction (Microsoft, "UpdateProcThreadAttribute function", `PROC_THREAD_ATTRIBUTE_JOB_LIST`). Branch `job_list`.
   2. *Suspended attempt*, only when the first failure can come from the job list: an error from `AttrList::new` or an attribute update, or a `CreateProcessW` error **outside the launch-input set** `{ERROR_FILE_NOT_FOUND (2), ERROR_PATH_NOT_FOUND (3), ERROR_INVALID_NAME (123), ERROR_BAD_PATHNAME (161), ERROR_BAD_EXE_FORMAT (193), ERROR_EXE_MACHINE_TYPE_MISMATCH (216), ERROR_DIRECTORY (267)}`. Microsoft documents no error code for a rejected job list, so the rule is stated as this exclusion: an error that names the program, the cwd, or the image fails once with its own error, and anything else (`ERROR_ACCESS_DENIED` included, since a job can refuse a process with it) may be the job list and gets the fallback. `AttrList::new(1)` with the pseudoconsole only; under `SPAWN_LOCK`, the same call with `| CREATE_SUSPENDED` and a fresh command-line copy; the guard is dropped when the call returns. On success, `AssignProcessToJobObject(job, hProcess)`, then `ResumeThread(hThread)` must return 1. Branch `suspended`.
   3. *No process.* Attempt 5.1 failed with a launch-input error, or both attempts failed: `CreateProcessW` returned `FALSE`, no process exists. `release_early`; `NeverStarted` with the last attempt's error, rewritten as `io::Error::new(kind, "job_list: <e1>; suspended: <e2>")` when both ran and the two Win32 codes differ. `kind`: `ERROR_FILE_NOT_FOUND` or `ERROR_PATH_NOT_FOUND` with `metadata(argv[0])` now `NotFound` → `NotFound`; `ERROR_ACCESS_DENIED` → `PermissionDenied`; anything else (`ERROR_DIRECTORY` for a cwd, `ERROR_BAD_EXE_FORMAT` for a non-PE file) → `Setup`.
   4. *Suspended-branch failures.* The child exists, created suspended, and none of the program's instructions has run. Without the lock:
      - **Assignment failed.** The job has no member. `TerminateProcess(hProcess, 1)`, then `WaitForSingleObject(hProcess, 2 s)`. Signalled → `release_early`, then `NeverStarted { Setup, error }` with its `Cleanup`. Termination failed or the wait timed out → `release_early`, then `StartedThenFailed { stage: "job_assign", identity: Some(pid, creation time read from hProcess or empty, group ""), cleanup: Unresolved { kill: Some(KillReport { method: "windows_process", tree_dead: false, survivors: [identity] }), close, drain_abandoned } }`. The process handle is then dropped: the backend keeps no retry owner, and the identity in the error is what the daemon has to act on (conservative: the daemon treats a positional prompt as unconfirmed).
      - **Resume failed** (`u32::MAX`, or 2 or more: the thread is still suspended) after a successful assignment. `TerminateJobObject(job, 1)`: the job owns the cleanup from here. Then `WaitForSingleObject(hProcess, 2 s)`. Signalled → `release_early`, `NeverStarted { Setup, error }`. Not signalled, or the termination failed → `StartedThenFailed { stage: "resume", identity: Some(group "job:<pid>"), cleanup: Unresolved { kill: Some(KillReport { method: "windows_job", tree_dead: false, survivors: [identity] }), .. } }` after `release_early`, whose job drop terminates the member by kill-on-close (a termination mechanism, not observed here, so the report stays `Unresolved`).
      - **`ResumeThread` returned 0**: the thread was not suspended and may have run. The child is in the job, so it is handled as a stage 6 failure (`StartedThenFailed { stage: "resume" }` through the teardown).
   The thread handle is closed at the end of the stage. Each attribute list is deleted when its attempt ends; the job array and the `Pcon` it points to outlive it by construction (§2.7).
6. **Settle.** `creation_time(&hProcess)` gives the start (§2.1). The `WindowsChild` (§2.4) is built at once, owning the process handle and the job. Failure of the read: `StartedThenFailed { stage: "identity", identity: None }`, released by `SpawnedParts::teardown` (`backend.rs:268-293`): `kill_tree(&[], now + 5 s)` (the job), drop input, `control.close(now + 2 s)`, `Drain::finish(2 s)`.
7. **Hand over** `SpawnedPty::new(SpawnedParts { child, output: drain, input: Box::new(in_w), control: Box::new(control), size: spec.size, evidence })` with `evidence = [("job_branch", "job_list" | "suspended"), ("conpty_module", <path>)]`, plus `("job_list_error", "<stage>: <error>")` on the suspended branch. These are the keys PR2 §2.1 names, plus one additive key; `evidence` stays a crate-internal vector surfaced by `Session::evidence()`.

`spawn` has no deadline of its own: it waits for `SPAWN_LOCK`, and `CreatePseudoConsole` and `CreateProcessW` are synchronous kernel32 calls without a handshake. A failure after stage 6 builds the child is always `StartedThenFailed` released by the teardown, whose order (kill → input → close → drain) differs from `release_early` only in that a tree exists to kill first.

**Crash window (PR 10).** If the daemon dies between a suspended `CreateProcessW` and `AssignProcessToJobObject`, the suspended child is in no Chaldea job, so the daemon's exit does not kill it, and no `launch_started` names it. It never runs (its only thread is suspended), but it holds a pid and its image until something terminates it. The job-list branch has no such window. Recorded under "Related Text" as a PR 10 obligation.

### 2.3 Input, Output, Control

- **Input** is `std::io::PipeWriter` (`in_w`), boxed as the session's `input`. `write` is one `WriteFile` on a synchronous anonymous pipe: it returns only when every byte is in the pipe (Microsoft, "WriteFile function", pipes: a write on a blocking pipe completes when a read makes room), so whole acceptance holds without a loop. A broken pipe (conhost gone) is an error, which the writer turns into `InputBroken` (PR2 §2.3). **Dropping it writes no bytes**: `PipeWriter`'s drop is `CloseHandle`. A write blocked on a full pipe ends when conhost goes away, which `ClosePseudoConsole` causes at close step 4 (PR2 §2.2); the writer thread is reported abandoned if step 3's budget runs out first, exactly as PR2 §2.2 allows. What conhost does when the input pipe reaches EOF is not relied on: every path that drops the input kills the tree first (PR2 §2.2 step 2, teardown) or follows conhost's death.
- **Output** is `std::io::PipeReader` (`out_r`), owned by the drain from stage 3. `ERROR_BROKEN_PIPE` reads as `Ok(0)` (W2), so the drain reports `OutputEnd::Eof` when the output pipe's last writer closes; any other read error is `OutputEnd::Error`. **Three facts are kept apart:** the pseudoconsole released (`close` returned, `CloseReport::elapsed` is `Some`); output EOF (the last writer of the output pipe closed, which is conhost, normally because the pseudoconsole was released and also when conhost dies on its own, W4); and the process exit observed (`wait_timeout` through the held handle). Child exit does not end the output on Windows, and the session's `Exited` (from `wait_timeout`) is the exit signal, as PR2 §2.2 already treats it. `ReleasePseudoConsole` is not called (§3).
- **Startup output.** conhost writes its own setup sequences (it requested `?9001h` on the spike host, arch §5 "Windows inbox conhost") from before the child exists; the drain feeds them to the VT from the first byte. With flags `0` conhost sends no cursor query that waits for a reply (W5); if a build sends one, the responder's reply waits in the reply lane until the writer thread exists (PR2 §2.2 "Start"), and `spawn` does not depend on it. conhost answers DSR and DA itself and forwards the kitty query (prior-art row 3b); the responder answers what reaches it.

```rust
// drain.rs (crate-internal; replaces close_bounded)
pub(crate) struct CloseWorker { /* sender to the worker, done receiver, JoinHandle */ }
impl CloseWorker {
    pub(crate) fn start(name: &'static str) -> io::Result<CloseWorker>;  // the thread exists and waits
    pub(crate) fn arm(&mut self, close: Box<dyn FnOnce() + Send>);        // hands over the close; cannot fail
    pub(crate) fn close(self, deadline: Instant) -> Option<Duration>;     // run it; Some only if it returned and the thread was joined by `deadline`
}

// windows/control.rs
struct Pcon(HPCON);                                   // not a kernel handle; closed only by Pcon::close(self); Drop does nothing
struct WindowsControl { pcon: Arc<Pcon>, worker: Option<CloseWorker> }   // PtyControl
```

**The close worker.** `start` spawns the thread before the resource it will close exists; the thread waits for a message. `arm` sends it the close closure (the worker is blocked in its receive and nothing else runs on it, so the send cannot fail while the worker lives). `close(deadline)` tells the worker to run the closure, waits for its completion until `deadline`, then waits until the deadline for the thread to finish and joins it: `Some(elapsed)` only when the closure returned **and** the thread was joined, so a caller that sees `Some` knows no `conpty-close` thread remains (the handle-count tests rely on it). Past the deadline it returns `None` and the thread is abandoned, the close recorded as hung. An unarmed worker told to close exits at once. Dropping a `CloseWorker` without `close` disconnects it: an armed worker then still runs the closure on its own thread, and an unarmed one exits; the caller never blocks. Time is measured against the caller's absolute deadline, not from the moment a thread starts. `close_bounded` and its `dead_code` allowance are removed; its unit test `hung_close_is_bounded` becomes `hung_close_worker_is_bounded` with the same bounds, plus `close_worker_joins_its_thread`, `unarmed_worker_exits`, and `dropped_armed_worker_runs_the_close`. The module stays platform-independent.

- `resize(size)`: `ResizePseudoConsole(hpc, COORD { X: cols, Y: rows })` through the control's `Arc<Pcon>`; an HRESULT failure maps as in stage 2. Unbounded (PR2 §2.1): conhost can wait on the drain.
- `close(deadline)`: drops the control's `Arc<Pcon>`, leaving the armed closure's clone the only one, then `worker.close(deadline)`; the closure does `Arc::try_unwrap(pcon)` and `Pcon::close`. The drain keeps reading meanwhile (PR2 §2.2 step 4), so a build whose close waits for the output to drain (W4) returns. `CloseReport { elapsed }` is the worker's result. Because the worker exists before the pseudoconsole, no path holds a pseudoconsole without a thread that can close it.
- `Drop` without `close` (a guard or session dropped on a path that never closed it): the same `close` with `now + TEARDOWN_CLOSE` (2 s).
- `CloseOutcome::abandoned` lists `pty-writer`, `pty-drain`, `pty-wait` (W12) and not `conpty-close`; a hung close is visible as `close.elapsed == None` and `released.control == false`, which the tests assert (listing `conpty-close` is a proposed PR 2 change, "Related Text").

### 2.4 Child: Identity, Exit

```rust
struct WindowsChild {
    process: OwnedHandle,                // from CreateProcessW; PROCESS_ALL_ACCESS
    job: OwnedHandle,                    // KILL_ON_JOB_CLOSE: closing the last handle terminates what is left
    identity: ProcessIdentity,           // start_time empty only during a stage 6 failure
    exit: OnceLock<ChildExit>,
    kill: Mutex<KillState>,              // retained extras across calls (§2.5); wait_timeout never takes it
}
```

- `identity()` returns the stored value; no panic path.
- `wait_timeout(timeout)`: if `exit` is set, return it. Else `WaitForSingleObject(process, ms)` in chunks: a zero `timeout` is one wait of 0 ms (a poll); otherwise each chunk is `ms = clamp(ceil_ms(remaining), 1, u32::MAX − 1)` (a sub-millisecond remainder rounds up to 1 ms, never to `INFINITE`), and a `WAIT_TIMEOUT` on a chunk continues with the next until `timeout` has passed. `WAIT_OBJECT_0` → `GetExitCodeProcess` → `ChildExit { code: Some(code as i32), signal: None }` (the 32-bit code reinterpreted, so `0xC000013A` is negative), stored once and returned on every later call; `WAIT_TIMEOUT` at the end → `Ok(None)`; `WAIT_FAILED` → the error (`WaitFailed`). It takes no lock. The same chunking applies to every handle wait in `sys::wait`.
- The kill exit code is 1 (`TerminateJobObject(job, 1)`), as in the spike and Codex; a killed leader reports `code: Some(1)`.
- `Drop` closes the process handle and the job handle. The job handle is neither named nor inheritable, so the `WindowsChild` holds its only handle, and closing it terminates any member still running (`KILL_ON_JOB_CLOSE`; CL-28). The child is dropped when its last `Arc<dyn PtyChild>` goes, which can be the session's waiter thread after `close` returns. Kill-on-close is a termination mechanism: nothing observes the members' exit at that point, so it is never a death proof (PR 10 confirms death by identity).

### 2.5 Tree Kill

```rust
impl PtyChild for WindowsChild { fn kill_tree(&self, extra: &[ProcessIdentity], deadline: Instant) -> KillReport; }

pub(crate) trait JobOs {                       // the real one wraps sys; tests script it with a synthetic clock
    fn now(&self) -> Instant;
    fn sleep(&self, d: Duration);
    fn terminate_job(&self) -> io::Result<()>;
    fn active_processes(&self) -> io::Result<u32>;
    fn job_pids(&self, capacity: usize) -> io::Result<PidList>;          // one query; survivor report only
    fn member_identity(&self, pid: u32, deadline: Instant) -> Option<ProcessIdentity>;  // open, IsProcessInJob, creation time; checks the clock between them
    fn open(&self, id: &ProcessIdentity) -> Opened;                      // extras
    fn signalled(&self, h: &Held, wait: Duration) -> io::Result<bool>;   // WaitForSingleObject, capped to the budget
    fn terminate(&self, h: &Held) -> io::Result<()>;                     // TerminateProcess; ACCESS_DENIED after exit is not an error
}
pub(crate) enum PidList { Complete(Vec<u32>), TooSmall }
pub(crate) enum Opened { Held(Held), Gone, Stale, Unverifiable }
pub(crate) struct KillState {
    held: HashMap<(u32, u64), Held>,        // verified extras: handle open, so the pid stays reserved
    pending: HashSet<ProcessIdentity>,      // well-formed extras not yet resolved
}
```

`ProcessIdentity` has no `Ord` (W12), so the state uses hash-based collections.

**What the job guarantees.** The job lacks `BREAKAWAY_OK`, so a process created by a member is a member whatever inner jobs exist: a member's breakaway request from Chaldea's job fails, and a descendant that breaks away from a permissive inner job stops at Chaldea's job (W11; CL-16 checks the first). A member cannot leave: assigning it to another job nests that job under Chaldea's or fails (W11). The leader is a member from creation (job list) or before it runs (suspended + assign). Nothing outside can join: only stage 5 assigns, and the job handle is unnamed and not inheritable. `JobObjectBasicAccountingInformation.ActiveProcesses` counts the processes of child jobs too (W11). So `ActiveProcesses == 0` read once means no process is associated with the job, and it stays 0.

**Outside the guarantee**, and reached only through `extra`: processes that were never in the job (started on the CLI's behalf by another process, such as a COM or packaged-app activation, a service, or WMI `Win32_Process.Create`; the spike saw an outside-job packaged PowerShell under Codex, arch §5 "Outside-job PowerShell"), and descendants of an `extra` identity that are outside the job. The backend does not walk the process list to find the latter (§3): in the observed case the outside-job PowerShell and its conhost were dead after job termination before any pid fallback (arch §5), and Codex under `--no-daemon` keeps its MCP servers inside the launched tree (arch §5), so the named identities are the remaining reach.

**Extras are trusted input.** A matching creation time identifies the process object behind a pid, not its provenance: passing the exact identity of an unrelated live process kills it (CL-18 does this on purpose with a test-owned process). The daemon passes only identities attributed to this launch (hook emitters' records); attribution is a PR 9 / PR 10 obligation ("Related Text"). **The unrelated-process guarantee is scoped accordingly:** the backend terminates only through the job and through handles of supplied extras whose creation time matched. Separately, `ClosePseudoConsole` sends `CTRL_CLOSE_EVENT` to every process attached to that console (W4), including an outside process that attached itself with `AttachConsole`; that is the console's behaviour at close, not a termination by the backend.

**Extras.** An `extra` identity is well-formed if its pid is not 0, not 4 (System), not the daemon's own, and its `start_time` parses as `u64`. A malformed entry is never stored: it is a survivor of the call that passed it, and that call cannot report `tree_dead: true`. `open`: `OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, FALSE, pid)`, falling back to `PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE` on `ERROR_ACCESS_DENIED` (then it can be waited for but not killed); `GetProcessTimes` **on that handle**: equal start → `Held` (moved from `pending` to `held`, so its pid cannot be reused while the backend tracks it); another start → `Stale` (the recorded process is gone, since its pid was freed; never signalled; removed from `pending`); `ERROR_INVALID_PARAMETER` → `Gone` (removed); any other failure → `Unverifiable` (stays in `pending`, retried by every later call and every round of this one). An entry leaves `pending` only as `Held`, `Gone`, or `Stale`. A `Held` extra is terminated only through its own handle, so the identity check and the kill cannot refer to different processes; there is no residual reuse window. Only one remains: two processes with the same pid and the same 100-ns creation stamp after a wall-clock step back, which is not defended.

**The proof rule.** `tree_dead: true` exactly when, in this call: a read of `ActiveProcesses` returned 0; the leader's handle and every `Held` extra are signalled; `pending` is empty; and the call was passed no malformed entry. A handle observed signalled is released from `KillState` at that read. Nothing proven by an earlier call is reused, and every identity ever supplied stays in `held` or `pending` until resolved, so a later call with `[]` cannot prove death without it. **Bound:** a member counted out of the job has stopped running, but the job does not report when its handles (file locks among them) are released; Codex observes the job list emptying before members release their handles (W7). The leader and the held extras are waited for until signalled; other members are not (§4, worktree removal).

**Steps.** Every sleep is `min(10 ms, deadline − now)`. `now()` is read before every OS call, including each list-size retry and each member open; no call starts at or after the deadline. The tolerance is one OS call in flight at the deadline, as in PR 3 (handle waits are capped to the remaining budget; the other calls do not block).

1. **Lock.** `now ≥ deadline`: no OS call; one `try_lock`; if it succeeds, the well-formed supplied identities not in `held` are added to `pending` (no OS call); report `tree_dead: false` with method `"windows_job_incomplete"` and the survivors of step 7 without a member snapshot. Else `try_lock` is polled with the sleep above until the deadline; never acquired → `tree_dead: false`, method `"windows_busy"`, survivors the leader and the supplied identities (deduplicated as in step 7). **The contract for busy:** these identities were not recorded, and the caller resubmits them. A poisoned lock is taken over.
2. **Record.** Every well-formed supplied identity not already in `held` goes into `pending`, before the first OS call.
3. **Open.** `open` each `pending` identity, checking the clock before each; at expiry the rest stay `pending`.
4. **Opening check.** `active_processes() == 0`, every held handle signalled with a zero wait, `pending` empty, no malformed entry → `tree_dead: true` with no termination call (an already dead tree).
5. **Terminate.** `terminate_job()`; `terminate()` each held extra not yet signalled. A failure is recorded and the steps continue: the proof does not depend on these calls succeeding, only on what step 6 reads.
6. **Prove**, in rounds until the deadline. Each round: `active_processes()`.
   - Nonzero → refresh the member snapshot: `job_pids` starting at capacity 64 and doubling on `TooSmall` up to 4 096, then `member_identity` for each listed pid not yet identified in this call (listed only if `IsProcessInJob` confirms membership on that handle, since the pid may name a new process; `group` `job:<leader pid>`; empty `start_time` when it cannot be read); a pid absent from the new list leaves the snapshot. Then `terminate_job()` again (a member that created a child as the termination began), and sleep.
   - Zero → wait on the leader and each held extra for at most `min(100 ms, deadline − now)` each, re-terminate an unsignalled held extra once per round, and `open` each `pending` identity once; all signalled, `pending` empty, no malformed entry → `tree_dead: true`.
   - A query error is not proof: sleep and continue.
7. **At the deadline**, `tree_dead: false`. Survivors: the leader unless seen signalled; the last member snapshot; every unsignalled held extra; every `pending` identity; this call's malformed entries. Deduplicated: identical identities collapse, then one entry per pid in the order leader, held extra, snapshot member, pending identity (the smallest `start_time` value), malformed entry (first passed). Method `"windows_job"`, or `"windows_job_incomplete"` when the last `ActiveProcesses` read failed or was nonzero and the member list after it did not complete (a list or open cut off by the deadline, or a failed list query). The member list feeds the report only, never the proof.

`method` is `"windows_job"`, `"windows_job_incomplete"`, or `"windows_busy"`; all three are values of the existing `String` field, so no type changes, and PR 10 treats every `tree_dead: false` alike. There is no graceful phase: Windows has no signal for a console process tree that the daemon can send without attaching to the pseudoconsole, and closing the pseudoconsole first would kill console clients before the job proof and hide survivors (spike §4.6). `kill_tree` has no panic path.

**Daemon inside a job** (PR 8). Nesting is conditional (W11): a parent job that sets UI limits, or with which no valid hierarchy forms, refuses it. Then the job-list attempt fails, the suspended attempt's assignment fails, the suspended child is terminated (stage 5.4), and the spawn is `NeverStarted` (or `StartedThenFailed` with `Unresolved` if its exit is not confirmed), never an uncontained running child (CL-27). When nesting succeeds, the session's job is a child of the parent job and nothing here needs breakaway.

### 2.6 Test Routes and Fault Injection

Faults live in `windows::hooks`, compiled under `cfg(any(test, feature = "fake"))`, so no default-feature build contains them (PR2 D6). Unit tests set them on a `WindowsPty`; integration tests use `fake::windows_inject_next(fault)`, which applies to the next native spawn **on the calling thread** (a thread-local), so parallel tests cannot see each other's faults. Unit tests also have PR 3's stage hooks (PR3 `src/unix/tests.rs`): `Validated`, `LockAcquired { stage: 2 | 5 }` (inside the lock, before `CreatePseudoConsole` and before each `CreateProcessW`), and `Spawned { pid }` before stage 6, plus a per-thread release trace at the release calls of §2.2 and `SpawnedParts::teardown`, each entry recording whether `SPAWN_LOCK` was held.

```rust
pub enum WindowsFault {                    // fake feature, Windows
    FirstPipe, SecondPipe,                 // fail that pipe; SecondPipe: the first pipe exists
    CloseWorker,                           // fail CloseWorker::start: no pseudoconsole is created
    PseudoConsole, DrainStart,
    JobCreate, JobLimits,                  // JobLimits: the job handle exists, setting its limit fails
    AttrInit, AttrUpdate,                  // fail the list init, or an update after the init, in both attempts
    RejectJobList,                         // attempt 5.1 fails with a code outside the launch-input set, without calling CreateProcessW
    CreateProcess,                         // both attempts fail that way without calling CreateProcessW
    Assign,                                // implies RejectJobList; assignment fails (not called); termination is real
    AssignTerminateDenied,                 // as Assign, and TerminateProcess goes through a handle without PROCESS_TERMINATE
    ResumeFails,                           // implies RejectJobList; ResumeThread is not called and fails; termination is real
    ResumeTerminateDenied,                 // as ResumeFails, and TerminateJobObject goes through a handle without JOB_OBJECT_TERMINATE
    Identity,                              // fail the creation-time read in stage 6
    AfterCreate(Gate),                     // block right after a successful CreateProcessW (lock released) until the gate opens
    AfterSpawn { gate: Gate, marker: &'static [u8] },  // block after stage 6 until the gate opens and the drain saw `marker`, then fail
    KillExpired,                           // the teardown's kill runs with an expired deadline
}
pub fn windows_inject_next(f: WindowsFault);
pub fn windows_fault_report() -> Option<FaultReport>;  // stages reached, CreateProcessW calls, attribute-list inits and deletes,
                                                       // termination results, child exited, marker seen with the child running
pub fn identify(pid: u32) -> io::Result<Option<ProcessIdentity>>;  // a live process only (an exited one is None); group ""
pub fn kill_verified(pid: u32, start_time: &str) -> bool;          // terminate through one handle after the creation-time check
pub fn windows_watch(id: &ProcessIdentity) -> io::Result<Watch>;   // SYNCHRONIZE handle opened after the creation-time check
impl Watch { pub fn signalled(&self, timeout: Duration) -> io::Result<bool>; }
pub fn process_handle_count() -> Option<u32>;                      // GetProcessHandleCount(GetCurrentProcess())
pub fn windows_in_job(leader_pid: u32, pid: u32) -> io::Result<bool>;  // IsProcessInJob against that session's job
```

The `…TerminateDenied` faults make termination fail for real: the backend calls the real `TerminateProcess` or `TerminateJobObject` on a duplicate of its handle opened by `DuplicateHandle` without the terminate right, which fails `ERROR_ACCESS_DENIED`, and the bounded wait then times out for real because the child stays suspended. No fault makes a successful kill look unconfirmed. `windows_in_job` reads a registry of live jobs keyed by leader pid (weak references, removed when the child drops), present only with `fake`. A `Gate` is a pair of `std::sync::mpsc` ends the test opens from another thread when its milestone arrives (a FakeCli event), so no test waits by sleeping. `AfterSpawn` waits, bounded by 10 s, until the drain has seen `marker` with the child still running; missing either is reported as "milestone not reached", never as the injected failure. The marker check is a `cfg(any(test, feature = "fake"))` watch in `drain.rs` (`Drain::start_watching(read, sink, marker)` and `Drain::wait_marker(deadline) -> bool`: the drain thread searches each chunk together with the previous chunk's last `marker.len() − 1` bytes), because conhost's startup bytes already satisfy any byte count (§2.3).

### 2.7 The `unsafe` Carve-Out

The crate's `[lints]` table (PR3 §2.7) is unchanged. A second module is allowed `unsafe`: `#[allow(unsafe_code)] mod sys;` in `src/windows/mod.rs`. Everything else is safe code over `sys`, std, and `OwnedHandle`. Each `unsafe` block holds one operation and a `SAFETY` comment (`multiple_unsafe_ops_per_block`, `undocumented_unsafe_blocks` denied). `sys` exposes safe functions only:

| Safe function | `unsafe` operations inside | Safety argument |
|---|---|---|
| `create_pseudo_console(&SpawnGuard, PtySize, &PipeReader, &PipeWriter) -> io::Result<Pcon>` | `CreatePseudoConsole` | Both handles are borrowed, valid for the call; the out-pointer is a local `HPCON`; a `Pcon` is built only when the HRESULT succeeds. The guard parameter proves the spawn lock is held (W3). |
| `Pcon::close(self)`, `Pcon::resize(&self, PtySize)` | `ClosePseudoConsole`, `ResizePseudoConsole` | `close` consumes the `Pcon`, reachable only through `Arc::try_unwrap` once the control's clone is gone, so the HPCON is closed once and no `resize` borrow can exist; `resize` borrows a live `Pcon` through the control's `Arc`. `Pcon` is `Send + Sync` (an opaque pointer that conhost's API serializes). |
| `create_job() -> io::Result<OwnedHandle>` | `CreateJobObjectW`, `OwnedHandle::from_raw_handle`, `SetInformationJobObject` | Null name and attributes; the returned handle is checked non-null before wrapping; the limit struct is zero-initialised (integers only) and its exact size passed. |
| `AttrList<'a>::new(n)`, `set_pseudoconsole(&'a Pcon)`, `set_job_list(&'a [HANDLE; 1])`; `Drop` | `InitializeProcThreadAttributeList` (size query, then init), `UpdateProcThreadAttribute`, `DeleteProcThreadAttributeList` | The buffer is a `Vec<usize>` (pointer-aligned) of the queried size; the lifetime `'a` ties the list to the values it points to, so they outlive every use of the list in `CreateProcessW`; delete runs once, in `Drop`, only after a successful init. |
| `create_process(&SpawnGuard, &Buffers, &mut AttrList, suspended) -> io::Result<Created>` | `CreateProcessW`, two `OwnedHandle::from_raw_handle` | `Buffers` guarantees NUL-terminated application name and cwd and a double-NUL environment block; the command line is copied into a fresh mutable NUL-terminated buffer for this call (`CreateProcessW` may write to it); `STARTUPINFOEXW` zeroed with `cb` set; `bInheritHandles = FALSE`; the two handles of `PROCESS_INFORMATION` are wrapped exactly once and only on success. |
| `assign(&OwnedHandle, &OwnedHandle)`, `resume(&OwnedHandle) -> io::Result<u32>` | `AssignProcessToJobObject`, `ResumeThread` | Borrowed valid handles; `u32::MAX` from `ResumeThread` is mapped to the last error. |
| `terminate_job(&OwnedHandle, u32)`, `terminate_process(&OwnedHandle, u32)` | `TerminateJobObject`, `TerminateProcess` | Borrowed valid handles. |
| `wait(&OwnedHandle, Duration) -> io::Result<bool>`, `exit_code(&OwnedHandle)`, `creation_time(&OwnedHandle) -> io::Result<u64>` | `WaitForSingleObject`, `GetExitCodeProcess`, `GetProcessTimes` | Borrowed valid handles; each wait is a chunk below `INFINITE` (§2.4); `FILETIME` out-parameters are locals. |
| `open_process(pid, access) -> io::Result<OwnedHandle>` | `OpenProcess`, `OwnedHandle::from_raw_handle` | `bInheritHandle = FALSE`; null checked before wrapping. |
| `active_processes(&OwnedHandle) -> io::Result<u32>` | `QueryInformationJobObject` (`JobObjectBasicAccountingInformation`) | Zeroed struct of integers; its size passed. |
| `job_pids(&OwnedHandle, capacity) -> io::Result<PidList>` | `QueryInformationJobObject` (`JobObjectBasicProcessIdList`), reads of the header and of `NumberOfProcessIdsInList` entries | One query: a `Vec<usize>` holding the header plus `capacity` entries, byte length passed; `ERROR_MORE_DATA` → `TooSmall`; only `min(NumberOfProcessIdsInList, capacity)` entries are read. The caller drives the retries (§2.5). |
| `in_job(&OwnedHandle, &OwnedHandle) -> io::Result<bool>` | `IsProcessInJob` | Borrowed valid handles; the result is a local. |
| `module_path(&str) -> Option<PathBuf>` | `GetModuleHandleW`, `GetModuleFileNameW` | No reference count is taken (`GetModuleHandleW`); the buffer length is passed and a result equal to it is treated as truncated. |
| `compare_ordinal_ignore_case(&[u16], &[u16]) -> Ordering` | `CompareStringOrdinal` | Lengths passed with the pointers; a failure (only for invalid parameters) is mapped to an ordinal comparison of the raw units. |
| `process_handle_count() -> io::Result<u32>` | `GetProcessHandleCount(GetCurrentProcess())` | The pseudo-handle needs no close. |
| `#[cfg(any(test, feature = "fake"))] duplicate_with_access(&OwnedHandle, access) -> io::Result<OwnedHandle>` | `DuplicateHandle`, `OwnedHandle::from_raw_handle` | Source and target are the current process; `bInheritHandle = FALSE`; wrapped only on success. Used by the `…TerminateDenied` faults. |
| `#[cfg(test)] make_inheritable(&OwnedHandle)`, `pipe_has_writer(&PipeReader) -> io::Result<bool>`, `set_ui_restrictions(&OwnedHandle)` | `SetHandleInformation`; `PeekNamedPipe` (`ERROR_BROKEN_PIPE` → `false`); `SetInformationJobObject(JobObjectBasicUIRestrictions)` | Test-only helpers (CL-21, CL-27), compiled only into the unit-test binary; borrowed valid handles, null out-pointers where the API allows them. |

`tests/lints.rs` changes from one allowance to exactly two, on `unix::sys` (in `src/unix/mod.rs`) and `windows::sys` (in `src/windows/mod.rs`), with no `unsafe` anywhere else; the scan reads every source file, so it runs on both OSes.

### 2.8 `testkit` and `chaldea-fake-cli` on Windows

testkit keeps `forbid(unsafe_code)`. Windows console access goes through `winapi-util` (W10), already in the lock.

**Terminal** (`term.rs`, Windows arm):

- `stdin_is_tty()`: `std::io::stdin().is_terminal()`.
- `make_raw()`: input mode `(mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT)) | ENABLE_VIRTUAL_TERMINAL_INPUT` on stdin and `| ENABLE_VIRTUAL_TERMINAL_PROCESSING` on stdout (spike `os/windows.rs:475-501`), with the documented values as local constants (`0x2`, `0x4`, `0x1`, `0x200`; `0x4` for output). So `echo` and `queries` read bytes under ConPTY as on Unix.
- `size()`: `screen_buffer_info(stdout).window_rect()` → `(right − left + 1, bottom − top + 1)`, pixels `None` (ConPTY has no pixel size).
- `inq()`: `None` (no safe input-event count).

**Hello.** The `Hello` type and the report stream are unchanged; on Windows the hello's Unix fields are null, as now.

**Additions.**

| Item | Behaviour |
|---|---|
| `--mode report-process --out <path>` | Writes one JSON object to `<path>.tmp` and renames it to `<path>`, then exits 0: `argv` (`std::env::args_os`, lossily as UTF-8), `env` (`std::env::vars_os` as `[key, value]` pairs in block order), `cwd`, and `stdin_console`, `stdout_console`, `stderr_console` (`is_terminal`). It opens no report stream, so it works with an empty environment (no `SystemRoot`, no Winsock). All OSes. |
| `breakaway` role (Windows) | `spawn-tree` starts it with `CommandExt::creation_flags(CREATE_BREAKAWAY_FROM_JOB)` (`0x0100_0000`). On `ERROR_ACCESS_DENIED` it starts it again without the flag; either way it sends `{"event":"breakaway","result":"accepted"\|"denied"}` on its own stream before the role's hello (spike `os/windows.rs:456-466`). Requested with `--roles`; the Windows default roles stay `plain`, `double-forked`. |
| `double-forked` (Windows form) | `double-fork-parent` starts it with a piped stdin, keeps the `ChildStdin` until its own exit (`std::mem::forget`, so the write end closes only when the process ends), and exits at once; it has no report stream. The child reads stdin on a reader thread and waits for its EOF for at most 10 s (`recv_timeout` on a channel), then sends its hello. Windows has no reparenting to wait for. |
| `plain`, `unrelated` | Unchanged. The test starts `unrelated` itself with `std::process::Command`, outside the session's job. |
| `attachable` on Windows | `size` at start from `size()`; then polls `size()` every 20 ms and sends `winch` when it changed (Windows has no `SIGWINCH`; the event reports an observed change, not a signal). |
| `--emit-output` (`spawn-tree`) | After its hello and roles, writes `OUTPUT-<n>\r\n` to stdout every 50 ms and flushes, and after each of the first two lines sends `{"event":"output","n":<n>}`. It keeps ConPTY's output busy through a teardown (F-2c). All OSes. |
| `--validate-input <n>` (`echo`, raw) | Reads exactly `n` bytes, checks that byte `i` is `b'a' + (i % 26)`, and sends `{"event":"validated","bytes":<read>,"ok":<bool>}` instead of per-chunk `input` events. All OSes. |

**Test side.** `Cleanup::kill_with(self, f: fn(u32, &str) -> bool) -> Cleanup` (additive; the private fields gain the function): on drop, each tracked identity is passed to `f`. Windows tests build `Cleanup::new(Some(start_of)).kill_with(fake::kill_verified)`, where `start_of` is a test-local `fn(u32) -> Option<String>` that returns `fake::identify(pid)`'s `start_time`, so `Cleanup::track` records the creation stamp (W12), and `kill_verified` checks it and terminates through the same handle. The real-ConPTY tests are testkit integration tests (`tests/pty_windows.rs`) for the same reason as on macOS: only they get `CARGO_BIN_EXE_chaldea-fake-cli` (PR3 §2.8). Every FakeCli that opens a report stream receives `SystemRoot` in its environment (Winsock needs it); the tests take it from their own environment and never change that environment.

## 3. Structure Options

| Fork | Option | Precedent | Verdict |
|---|---|---|---|
| **ConPTY function resolution** | A. `conpty.dll` from the search path, else kernel32 | portable-pty `psuedocon.rs:44-59`; alacritty `conpty.rs:31-90` | Rejected (decided, prior-art row 1d): a planted or sideloaded DLL changes the console host. |
| | B. `GetProcAddress(GetModuleHandleW("kernel32.dll"), …)` | Codex loads `kernel32.dll` by name (`psuedocon.rs:92-98`) | Rejected: a transmute per function and three more `unsafe` blocks for the same binding. |
| | C. The static `raw-dylib` import of `kernel32.dll` that `windows-sys` declares | W1 | **Adopted.** The import table names `kernel32.dll`, a KnownDLL; CL-4 reads the test binary's PE imports. |
| **Flags** | `0` | alacritty, Windows Terminal (prior-art row 4d) | **Adopted** (decided after the spike, arch §5). `0x2` (Codex) is the reserve if a resize problem appears; `0x1` stalls startup (W5). |
| **Job assignment** | A. Job list, fail otherwise | Codex `procthreadattr.rs:85-88` | Rejected: the mvp row requires the suspended fallback. |
| | B. Job list, else `CREATE_SUSPENDED` + assign + `ResumeThread == 1` | spike `os/windows.rs:1813-1874` | **Adopted** (decided), with the fallback only for errors that can come from the job list (§2.2 stage 5.2). |
| | C. Assign after start | portable-pty; Zed (prior-art row 1b) | Rejected: race escapees observed (spike C-3). |
| **Containment refused** | A. Resume uncontained | Codex `job.rs:169-196` | Rejected: a process tree stop could not reach. |
| | B. Terminate the suspended child (through the job once assigned), `NeverStarted` when its exit is confirmed, else `StartedThenFailed` with `Unresolved` and its identity | — | **Adopted.** |
| | C. B plus a retry owner that keeps the handle and retries termination after `spawn` returns | — | Rejected: background machinery for a failure (termination of an own suspended child refused) that the report already surfaces; the identity lets the daemon act. |
| **Breakaway** | A. `BREAKAWAY_OK` | Codex `job.rs:79-82` | Rejected: any member could leave the job and survive stop. |
| | B. No breakaway limit | spike `os/windows.rs:242-260` | **Adopted.** A CLI's breakaway request fails visibly instead of escaping; reversible (one flag) if a CLI needs it. |
| **Proof of death** | A. Toolhelp census and ppid walk | spike `list_processes`; herdr (prior-art row 1a) | Rejected: a walk is a snapshot, so completeness needs a census rule like PR 3's, which the job makes unnecessary. |
| | B. Job completion port, `JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO` | — | Rejected as the authority: delivery is not guaranteed (Microsoft, "JOBOBJECT_ASSOCIATE_COMPLETION_PORT structure"). |
| | C. Per-member handles from the pid list, each waited for | Codex `job.rs:281-335` | Rejected as a requirement: list sizing and opens on every call to prove handle release, which no consumer needs; the list feeds only the survivor report. |
| | D. `ActiveProcesses == 0` plus the leader's and held extras' handles signalled, nothing pending | spike `wait_drained` (`os/windows.rs:378-392`) | **Adopted.** Monotonic once zero (§2.5). |
| **Busy kill** | A. Hand the supplied identities to the running call | — | Rejected: a handoff between calls for a case the report already names. |
| | B. Return them as survivors with `windows_busy`; the caller resubmits | — | **Adopted.** |
| **Graceful phase** | A. `CTRL_BREAK` / `CTRL_CLOSE` first | — | Rejected: needs the daemon attached to the pseudoconsole, or closes it first and hides survivors (spike §4.6). |
| | B. Terminate at once | spike, Codex | **Adopted.** CLIs recover by `--resume` (crash-only). |
| **Pipes** | A. Own `CreatePipe` | spike | Rejected: `unsafe` for what std provides. |
| | B. Overlapped pipes with reader and writer threads | alacritty `conpty.rs:118-119` (miow) | Rejected: the drain and writer threads already exist; synchronous ends keep whole writes. |
| | C. `std::io::pipe` | W2 | **Adopted.** Synchronous, non-inheritable, `ERROR_BROKEN_PIPE` as EOF, no `unsafe`. |
| **conhost's pipe ends in the host** | A. Keep until the child exists | Codex `psuedocon.rs:116-121`, `:221` | Rejected: no benefit here, and a failure between the stages would have one more handle to release. |
| | B. Close right after `CreatePseudoConsole` | spike `os/windows.rs:1728-1731` | **Adopted.** conhost holds its own (W3). |
| **`ReleasePseudoConsole`** | A. Call it where exported | Codex `psuedocon.rs:222-230` | Rejected for PR 4: not in `windows-sys 0.61.2` (W1), so a `GetProcAddress` path; output would end at child exit on some builds and at close on others. Reopen if the daemon needs EOF at child exit. |
| | B. EOF at close | W4 | **Adopted.** The session already ends output at close. |
| **Who closes the pseudoconsole** | A. A thread spawned at close time (`close_bounded`) | PR 2 | Rejected: when the spawn fails the HPCON has no closer and is leaked (W12). |
| | B. Closing `Drop` on the HPCON wrapper | spike `Pcon` (`os/windows.rs:1400-1407`) | Rejected: a drop on the caller's thread could block without a bound. |
| | C. A close worker started before `CreatePseudoConsole` and armed with the `Pcon` | — | **Adopted.** A worker-start failure is `NeverStarted` with no pseudoconsole to release. |
| **Exit observation** | A. `RegisterWaitForSingleObject` callback | alacritty `child.rs:73` | Rejected: a thread-pool callback where PR 2's waiter already polls. |
| | B. `WaitForSingleObject(timeout)` on the held handle | PR2 §2.1 | **Adopted.** |
| **Handle wrapper** | A. Own `Handle(usize)` | spike | Rejected: `OwnedHandle` closes once and is `Send + Sync`. |
| | B. `std::os::windows::io::OwnedHandle`; an own `Pcon` for the HPCON, which is not a kernel handle | — | **Adopted.** |
| **Quoting** | A. Also quote LF and VT | Codex `psuedocon.rs:330-373` | Rejected: the CRT splits on space and tab only; extra quoting is harmless but diverges from std, the reference the tests compare against. |
| | B. std's rule | W6 | **Adopted.** |
| **Extras' descendants outside the job** | A. Toolhelp walk from each extra | PR 3's ppid walk | Rejected: completeness would rest on the walk, and the observed outside-job process died with the job (arch §5). |
| | B. Only named identities | PR2 §2.1 (the obligation names the identities) | **Adopted.** The daemon names each outside-job process it learns of. |
| **FakeCli console access** | A. `terminal_size =0.4.3` | — | Rejected: adds `windows-sys 0.60` and its target crates to the lock, and has no console mode. |
| | B. A testkit `unsafe` carve-out | — | Rejected: mvp §3 keeps `forbid` outside pty-runtime. |
| | C. `winapi-util =0.1.11` | W10 | **Adopted.** Already locked; size and mode. |
| **Launch oracle** | A. `cmd.exe /d /c set`, `cd`, `exit` | spike | Rejected for argv, env, cwd, and the length limit: `cmd.exe` caps its command line at 8 191 characters and adds its own variables. |
| | B. FakeCli `report-process` writing a file | — | **Adopted.** Exact argv, env, cwd, and console flags, with an empty environment. |
| **Fault injection** | A. `cfg(test)` hooks only | PR 3 | Rejected alone: FakeCli is reachable only from testkit's integration tests. |
| | B. An environment variable read by the backend | spike `--inject` | Rejected: it would ship in release builds. |
| | C. `cfg(test)` hooks plus a `fake`-feature thread-local | PR2 D6 | **Adopted.** |
| **Handle-leak oracle** | A. Equal to the count before the first launch | spike F-2c first form | Rejected: one-time lazy initialisation (the spike's unexplained +1). |
| | B. Unchanged across repeated cycles after a warm-up, in a quiescent helper process | spike phase 3c (settled at 89) | **Adopted**, scoped to the daemon's handle table and paired with traces and exit checks (§7). |
| **`unsafe` placement** | One `windows::sys` exposing safe functions | PR3 §2.7 | **Adopted.** |

## 4. Same-Class Inventory

Every MVP place on Windows that kills processes, holds handles or pids, or creates children:

| Place | PR | How this design serves it or stays out |
|---|---|---|
| `Session::kill`, `Session::close` step 2, `SpawnedPty` teardown | 2 | `WindowsChild::kill_tree`. |
| `chaldea stop`, `close --stop`, `resume` death check | 10 | `KillReport::method` `windows_job` → `launch_death_confirmed.method`; `tree_dead: false` (any method) keeps `death_confirmed: false`; a `windows_busy` report's identities are resubmitted. |
| Recorded descendants (`extra`) from hook emitters | 9, 10 | The identity format of §2.1. `extra` is trusted input: the daemon passes only identities attributed to the launch. Outside-job descendants of an extra are not found by the backend; the daemon names every process it records. The public lookup arrives with its first consumer (PR3 D7). |
| Daemon restart: old-tree check | 10 | Stays out. The job handle is unnamed and not inheritable, so the daemon's exit closes it and `KILL_ON_JOB_CLOSE` terminates every member; that is a termination mechanism, not a proof, so PR 10 still confirms death by verified identity (open, creation time), and handles the suspended-create crash window (§2.2). |
| Daemon process rules | 8, 10 | The spawn rule (every child under `with_spawn_lock`; this backend's `CreatePseudoConsole` and each `CreateProcessW` under it, §2.1) and the handle rule (no `CloseHandle` on a handle the code did not open). |
| Parent kill-on-close job check | 8 | Stays out. Nesting needs no breakaway; a parent job that refuses nesting makes `spawn` fail without an uncontained running child (CL-27). |
| Daemon spawns of git and one-shot hooks | 6, 16, 20 | Not PTY children; created inside `with_spawn_lock` with `CREATE_NO_WINDOW` (prior-art row 9). They cannot inherit a handle of this backend (none is inheritable outside `CreatePseudoConsole`, which runs under the lock). |
| Worktree removal in-use check | 16 | Reads launch state. **Note for PR 16:** `tree_dead` on Windows means no job member runs and the leader and held extras have exited; another member's file handles can be released a little later (W7), so a removal right after stop can see a sharing violation, which PR 16's `uncertain`/retry path must absorb. |
| Rescue | 20 | Acts through `surface_action stop`, the stop path. |
| Doctor: FakeCli tree kill, ConPTY module check | 21 | `native()` + `Session` + FakeCli; `Session::evidence()` gives `job_branch` and `conpty_module`. |
| FakeCli attach paste tests on `windows-latest` | 14 | FakeCli raw console mode (`--raw`, `echo`) under ConPTY exists from this PR (§2.8). |
| FakeCli's own children and test cleanup | 3, 4 | `--max-life-secs`; `Cleanup::kill_with(fake::kill_verified)`; the session's job kills its members on drop. |

## 5. Resource Paths

| Resource | Created at | Owner | Bound | Released on |
|---|---|---|---|---|
| Input pipe: conhost's read end (backend copy) | stage 1 | spawn | 1 | right after stage 2.2, or stage 1–2 failure |
| Input pipe: write end | stage 1 | spawn, then session `input` | 1 | input dropped (writes nothing) |
| Output pipe: read end | stage 1 | drain thread from stage 3 | 1 | drain end (EOF or read error), or `Drain::start` failure (dropped first) |
| Output pipe: conhost's write end (backend copy) | stage 1 | spawn | 1 | right after stage 2.2, or stage 1–2 failure |
| `conpty-close` worker thread | stage 2.1 | `CloseWorker`, in `WindowsControl` from stage 2.2 | 1 per spawn | joined by `close` within its deadline; an unarmed worker exits when closed or dropped; abandoned past the deadline |
| Inheritable duplicates inside `CreatePseudoConsole` | stage 2.2 | kernel32 | 3, for the call | before the call returns; created under `SPAWN_LOCK` |
| Pseudoconsole (`Pcon`) and its conhost process | stage 2.2 | the armed worker (closing) and `WindowsControl` (resize) | 1 | `close` / drop through the worker; no path without a worker |
| `pty-drain` thread | stage 3 | `Drain` | 1 | output EOF or a read error; else abandoned by `Drain::finish` |
| Job handle | stage 4 | spawn, then `WindowsChild` | 1 | `release_early`, or child drop (kill-on-close terminates remaining members) |
| Attribute list | stage 5 | stage-local `AttrList` | 1 per attempt | end of the attempt (`DeleteProcThreadAttributeList`) |
| Command line, environment, cwd buffers | validation | spawn | O(spec), plus one command-line copy per attempt | end of `spawn` |
| Process handle | stage 5 | spawn, then `WindowsChild` | 1 | child drop; on a 5.4 failure after the bounded wait |
| Thread handle | stage 5 | spawn | 1 | end of stage 5 |
| Suspended child of a failed fallback | stage 5.2 | spawn, then the job once assigned | 1 | termination + 2 s wait (5.4); if unconfirmed, reported by identity (and, once assigned, kill-on-close at the job drop) |
| Reduced-access duplicates (`…TerminateDenied` faults) | stage 5.4 | the failure path | 1 | end of the failure path (`fake` / test only) |
| Held extras | `kill_tree` | `KillState` | distinct verified extras | released at the read that shows them signalled; child drop |
| Pending extras | `kill_tree` | `KillState` | distinct well-formed unresolved extras | a later open that returns `Held`, `Gone`, or `Stale`; child drop |
| Member handles for the survivor report | step 6 | the call | ≤ 4 096 | at once, each after its read |
| `SPAWN_LOCK` | process | `pty-runtime` | 1 | held around `CreatePseudoConsole` and around each `CreateProcessW` call only |
| `fake` registry entry, fault, fault report, marker watch | spawn (`fake` only) | thread-local / registry / drain | 1 per thread, 1 per live child | next spawn on the thread; child drop |

**Endings.**

| Ending | What happens |
|---|---|
| Validation failure | `NeverStarted`, nothing created. |
| Stage 1 or 2.1 failure | Pipes (and nothing else) dropped; `NeverStarted { Setup }`, `Confirmed`. No pseudoconsole exists. |
| Stage 2.2 failure | Unarmed worker closed and joined, pipes dropped; `NeverStarted { Setup }`. |
| Stage 3 failure | Output read end dropped first, then the close through the worker, then the input; `NeverStarted { Setup }`, `Confirmed` only when the close returned, else `Unresolved`. |
| Stage 4, 5.1–5.3 failure | No process exists. `release_early`; `NeverStarted` with its kind and `Cleanup`. |
| 5.4 failure (assign, resume) | Termination of the suspended child (through the job once assigned), its exit awaited through its handle for 2 s: confirmed → `NeverStarted`; unconfirmed → `StartedThenFailed` with `Unresolved` and the identity. `release_early` in both cases. |
| Stage 6 failure, or later | `StartedThenFailed`; `SpawnedParts::teardown`: kill (job), input, close (worker), drain. |
| Child exit | `wait_timeout` reports it on every call; descendants stay in the job; output continues; no EOF before close. |
| `kill` | §2.5 within the deadline; I/O stays up. |
| `close` | PR2 §2.2: kill; input released (a blocked write ends when step 4 makes conhost go away); `ClosePseudoConsole` on the worker while the drain reads; drain to EOF; join. |
| `Session` drop | As close; the child's drop closes the job (kill-on-close for anything left) and the process handle. |
| conhost dies on its own | Output EOF; writes fail (`InputBroken`); the CLI, a console client, is terminated by the console's loss. |
| Reader `ERROR_BROKEN_PIPE` | `OutputEnd::Eof`. Any other read error: `OutputEnd::Error`. |
| Write error | Unit `Incomplete`, `InputBroken` (PR2 §2.3). |
| `ClosePseudoConsole` hang | The worker's `close` returns `None` at the deadline; `conpty-close` abandoned; the drain finishes at its bound or is abandoned. |
| Daemon crash between suspended create and assign | The suspended child survives outside any Chaldea job, unrecorded (PR 10, §2.2). |
| Panic | No panic path in this backend's code; a poisoned `kill` mutex is taken over. |

**Waits.** Every wait has a deadline except: acquiring `SPAWN_LOCK`; `CreatePseudoConsole` and `CreateProcessW` (synchronous, no handshake); the drain's `ReadFile` and the writer's `WriteFile` (both end when conhost closes its ends, which close step 4 causes); `ResizePseudoConsole` (unbounded by PR2 §2.1). Bounded: `wait_timeout` (zero is a poll; sub-millisecond rounds up; chunks continue); `kill_tree` (sleeps of `min(10 ms, deadline − now)`, handle waits capped to the remaining budget, no OS call started past the deadline; one in-flight call can overrun); `close` (the worker, against the caller's deadline, joined within it); the suspended child's exit wait (2 s, without the lock); `release_early` and the teardown (2 s close, 2 s drain, 5 s kill); `AfterSpawn`'s milestone wait (10 s, tests only); the Windows `double-forked` fixture's stdin wait (10 s).

## 6. Purpose Layer and Plan Layer

### 6.1 Purpose Layer: Acceptance Criteria

Checked on `windows-latest` by `cargo test -p chaldea-testkit --test pty_windows`, `cargo test -p chaldea-pty-runtime` (unit tests and `tests/windows_spawn.rs`); on a real Windows host the same, plus Task Manager or `Get-Process chaldea-fake-cli` before and after a kill.

| ID | Criterion (EARS) | Source | How to observe on a real Windows host |
|---|---|---|---|
| PR4-P-1 | When a CLI tree is killed, no process of its job and no verified `extra` identity shall be alive at the deadline, `tree_dead` shall be true, and an unrelated process shall still answer; a process never in the job, or an outside-job descendant of an `extra`, is outside this guarantee unless named in `extra`. | mvp §4 PR 4 obligation; PR2 §2.1 | FakeCli `spawn-tree` (`plain`, `double-forked`, `breakaway`): every role's handle signals after the kill; `unrelated` answers `ping`; an outside-job sleeper dies only when named in `extra`. |
| PR4-P-2 | The backend shall terminate a process only through the job or through a handle on which it verified the supplied creation time, never by pid alone; an unrelated process shall be terminated only if the caller names its exact identity. | spike §4.6; §2.5 | A stale `extra` (the unrelated pid with another start) leaves it alive while a matching one dies. |
| PR4-P-3 | When a spawn fails at any stage, the error shall say whether the program could have run, no process or handle of the spawn shall remain when it returns unless the error reports it `Unresolved` with its identity, and no pseudoconsole shall exist without a closer. | mvp §4 PR 4 obligation (spike F-2c); PR2 §2.1 | Each injected fault: process handle count unchanged across cycles; every fixture handle signalled; the error variant. |
| PR4-P-4 | When a spawn succeeds, the CLI shall run `argv[0]` with the requested arguments, cwd, and exact environment, on the pseudoconsole's standard handles at the requested size, with no inherited handle, inside the session's kill-on-close job from before its first instruction, with conhost from the inbox `kernel32.dll`, and the branch taken shall be recorded. | mvp §4 PR 4; PR2 §2.1 | `report-process` file (`argv`, `env`, `cwd`, consoles), `attachable` size, `Session::evidence()`. |
| PR4-P-5 | When the session closes, output shall be read until EOF, `ClosePseudoConsole` shall be bounded while output still flows, and `close` shall return within its budgets; written bytes shall reach the CLI whole; a resize shall reach it. | PR2 §2.1–§2.4 | `CloseOutcome` with `--emit-output` running; `echo --validate-input`; `winch`. |
| PR4-P-6 | Every `unsafe` block shall be in `pty_runtime::unix::sys` or `pty_runtime::windows::sys`, and every other crate shall keep `forbid(unsafe_code)`. | PR2 D7; mvp §3 | `tests/lints.rs`; clippy on both runners. |
| PR4-P-7 | The workspace shall build and pass on both runners, with the Windows tests running on `windows-latest`. | mvp §4 | `workspace (windows-latest)` including the test-list step. |

**Assumptions** that only a Windows host shows, each checked by the named claim. `windows-latest` checks its own Windows build only; behaviour of older inbox builds (notably a `ClosePseudoConsole` that blocks until the output is drained, W4) is handled by design (the drain reads during the close, the close is bounded) but not tested.

| # | Assumption | Claim |
|---|---|---|
| A1 | The runner accepts the job list, nested under any job the runner uses. | CL-4 |
| A2 | A member's `CREATE_BREAKAWAY_FROM_JOB` fails `ERROR_ACCESS_DENIED` when Chaldea's job lacks `BREAKAWAY_OK`. | CL-16 |
| A3 | `ActiveProcesses` reaches 0 within the kill budget after `TerminateJobObject`, while the test still holds handles to the members. | CL-16 |
| A4 | Closing the last job handle terminates a live member (`KILL_ON_JOB_CLOSE`). | CL-28 |
| A5 | `STARTF_USESTDHANDLES` with invalid handles gives the child the pseudoconsole's standard handles while the parent's three are pipes. | CL-12 |
| A6 | The console window equals the pseudoconsole size. | CL-11 |
| A7 | FakeCli raw mode works under ConPTY, and conhost passes printable ASCII input through unchanged. | CL-14 |
| A8 | `ClosePseudoConsole` returns within 2 s while output flows. | CL-20 |
| A9 | The daemon's handle count is stable across cycles after a warm-up. | CL-3, CL-7 |
| A10 | A command line of exactly 32 767 units is accepted and parsed back intact. | CL-1 |
| A11 | Output does not reach EOF before close. | CL-13 |
| A12 | An explicit environment block, empty included, reaches the child unchanged. | CL-8 |
| A13 | A parent job with UI limits refuses nesting, so both create attempts fail to contain the child. | CL-27 |

### 6.2 Plan Layer: Claims

Unit tests are in `src/windows/tests.rs` (system binaries: `%SystemRoot%\System32\cmd.exe` with `/d /c`), `src/windows/{cmdline,kill,control}.rs`, and `src/drain.rs`; public-API tests in `tests/windows_spawn.rs`; FakeCli tests in testkit `tests/pty_windows.rs` (`pty_windows::…`). Process-alive assertions use handles (`fake::windows_watch`, or the unit tests' own `sys::open_process` after a creation-time check), never a stream's closing alone.

| ID | Claim (EARS) | Source | Falsifying test |
|---|---|---|---|
| PR4-CL-1 | If validation fails, then `spawn` shall return `NeverStarted` with the kind of §2.2's table and create nothing; a command line of exactly 32 767 units shall launch with its argv intact. | PR2 §2.1; §2.2 | `windows_spawn`: `missing_program_is_not_found`, `relative_argv0_is_setup`, `quote_in_argv0_is_setup`, `nul_in_argv_is_setup`, `batch_file_is_refused` (`.cmd` and `.BAT`), `directory_argv0_is_permission_denied`, `missing_cwd_is_setup`, `verbatim_cwd_that_normalizes_differently_is_setup` (`\\?\<temp>\dir.` with `<temp>\dir` also existing), `bad_env_is_setup` (`=` in a key, NUL, `Path` and `PATH` together), `command_line_over_the_limit_is_setup` (32 768 units); unit `invalid_spec_creates_nothing` (no stage hook reached); `pty_windows::command_line_at_the_limit_spawns` (FakeCli `report-process` with one padding argument making exactly 32 767 units: `argv` in the file equals the spec; the measurement mvp D5 lacks) |
| PR4-CL-2 | If `CreateProcessW` fails with a launch-input error, then `spawn` shall return `NeverStarted` with that error's kind after one attempt; if it fails otherwise on both attempts, both errors shall be in the message; no process shall exist; the release shall be `Confirmed` when the close returned and the drain ended. | §2.2 stage 5.2–5.3 | unit `program_removed_after_validation_is_not_found` (a copy of `cmd.exe` in a temp dir, removed by the `Validated` hook), `cwd_removed_after_validation_is_setup`, `non_pe_exe_is_setup` (a text file named `.exe`: `ERROR_BAD_EXE_FORMAT`): each `fault_report` shows **one** `CreateProcessW` call and no process handle; `create_process_fault_tries_both` (`CreateProcess`: two attempts, both errors in the message) |
| PR4-CL-3 | If a stage before a process exists fails, then `spawn` shall return `NeverStarted { Setup }`, release in the order close (worker) → input → drain finish → job without holding `SPAWN_LOCK`, delete every attribute list it initialised, never reach a later stage, and leave the process handle count unchanged. | spike F-2c; §2.2 | unit `pre_process_faults_release_in_order` (one case each for `FirstPipe`, `SecondPipe`, `CloseWorker`, `PseudoConsole`, `DrainStart`, `JobCreate`, `JobLimits`, `AttrInit`, `AttrUpdate`, `CreateProcess`: the release trace in order with the lock not held; for `DrainStart` the output end dropped before the close; `fault_report` attribute-list deletes equal inits); `close_worker_fault_creates_no_pseudoconsole` (`CloseWorker`: no `CreatePseudoConsole` call in the trace, `NeverStarted { Setup }`, `Confirmed`); testkit `pty_windows::pre_process_faults_leave_no_handle` (helper process, §7: per fault one warm-up cycle and five measured cycles, the count sampled after each; any change or an unavailable count fails) |
| PR4-CL-4 | When a spawn succeeds on `windows-latest`, `evidence` shall hold `job_branch = job_list` and a `conpty_module` that is `kernel32.dll` in the system directory, the binary shall import `CreatePseudoConsole` from `kernel32.dll` and import nothing from `conpty.dll`, and the leader and every role shall be in the session's job, including a child started before the backend's next step. | mvp §4 PR 4 obligation; W1 | `pty_windows::spawn_takes_the_job_list_branch` (`evidence()` values; `conpty_module` equals `%SystemRoot%\System32\kernel32.dll` case-insensitively); unit `binary_imports_conpty_from_kernel32` (parses the test binary's PE import directory in safe code: the `kernel32.dll` descriptor names `CreatePseudoConsole`, `ResizePseudoConsole`, `ClosePseudoConsole`; no descriptor names `conpty.dll`); `pty_windows::child_is_in_the_job_from_creation` (`AfterCreate` gate opened only after spawn-tree's `plain` role sent its hello: `windows_in_job` true for the root and the role; an assign-after backend would have let the role start outside the job) |
| PR4-CL-5 | When the job list is rejected, the backend shall start the child suspended with exactly the requested argv and environment, assign it, resume it with `ResumeThread == 1`, record `job_branch = suspended` with `job_list_error`, the program shall not run before resume, and every role shall be in the job and die on kill. | mvp §4 PR 4; spike F-2c | `pty_windows::forced_fallback_runs_nothing_before_resume` (`RejectJobList` + `AfterCreate` gate with `report-process --out <f>`: at the gate `<f>` does not exist and the child is suspended; after the gate opens and `Exited` arrives, `<f>` holds exactly the spec's argv and env; evidence `suspended`); `pty_windows::forced_fallback_takes_the_suspended_branch` (`RejectJobList`; roles `plain`, `double-forked`, `breakaway`: `windows_in_job` true for each, each role's pong before the kill, each watch handle signalled after `Session::kill` returns, `unrelated` answers) |
| PR4-CL-6 | If assignment or resume fails on the suspended branch, then the backend shall terminate the suspended child (through the job once assigned), confirm its exit through its handle, and return `NeverStarted` with the program never run; if termination fails or the exit is not confirmed, `StartedThenFailed` with `Unresolved` and the child's identity (`group` empty when unassigned). | §2.2 stage 5.4 | unit `assign_failure_terminates_the_suspended_child`, `resume_failure_terminates_through_the_job` (`cmd.exe /d /c type nul > <marker>`: marker absent, `NeverStarted { Setup }`, `Confirmed`, fault report shows the real termination and the exit), `denied_termination_after_assign_failure_is_unresolved` (`AssignTerminateDenied`: `StartedThenFailed { stage: "job_assign" }`, identity with `group ""` naming a live suspended process; the test then kills it through `kill_verified` and its watch signals), `denied_job_termination_is_unresolved` (`ResumeTerminateDenied`: `StartedThenFailed { stage: "resume" }`, `group "job:<pid>"`; the watch signals after `spawn` returns, by kill-on-close at the job drop) |
| PR4-CL-7 | If setup fails after the process exists (identity, after spawn with output flowing, writer or waiter thread start), then `spawn` shall return `StartedThenFailed`, kill the tree before releasing input, pseudoconsole, and drain, close the pseudoconsole within its bound while output flows, leave no fixture alive, spare the unrelated process, and leave the handle count unchanged. | spike F-2c; PR2 §2.1 | `pty_windows::after_spawn_fault_with_output_flowing_kills_the_tree` (`spawn-tree --emit-output --roles plain,double-forked,breakaway`; `AfterSpawn` with marker `OUTPUT-1`, gate opened on the `output` event `n = 1` after every role's pong: every role's watch signals, `unrelated` answers, `Cleanup::Confirmed`, fault report marker seen with the child running), `identity_fault_kills_the_tree`, `writer_start_fault_kills_the_real_tree` (`StartFault::Writer`), `waiter_start_fault_kills_the_real_tree` (`StartFault::Waiter`), `after_process_faults_leave_no_handle` (helper process, warm-up then five sampled cycles); unit `expired_teardown_kill_reports_unresolved` (`KillExpired`: `Unresolved` with a kill report), `teardown_order` (trace: `terminate_job`, input drop, `close`, `drain_finish`) |
| PR4-CL-8 | The child's environment shall be exactly `spec.env`, in a block sorted with `CompareStringOrdinal`, double-NUL when empty. | PR2 §2.1; W6 | unit `env_block_is_sorted_case_insensitively` (`b`, `A`, `C` → `A`, `b`, `C`), `empty_env_block_is_two_nuls`; `windows_spawn::env_is_exactly_the_spec` (FakeCli is not available there, so `cmd.exe /d /c set` with `A=1` and `SystemRoot`: both present, the test's `PATH` value and `CARGO_PKG_NAME` absent); `pty_windows::report_shows_the_exact_env` (`report-process` with `{b=2, A=1}`: `env` is exactly `[[A,1],[b,2]]`), `empty_env_reaches_the_cli` (`report-process` with an empty env: `env` is empty) |
| PR4-CL-9 | When the child starts, its argv shall equal `spec.argv` after the CRT's parsing, for spaces, tabs, quotes, backslash runs before quotes and at the end, empty arguments, and non-ASCII text, on both branches. | §2.2 buffers | `pty_windows::argv_reaches_the_cli_intact` (`report-process` with `a b`, `""`, `x\"y`, `C:\dir\`, `\\"`, `日本語`, tab: `argv` equals the spec; once plain, once with `RejectJobList`); unit `command_line_matches_std_quoting` (fixed corpus against expected strings), `each_attempt_gets_a_fresh_command_line` (`RejectJobList` with a hook that overwrites the first attempt's buffer: the second attempt's buffer equals the original) |
| PR4-CL-10 | The child shall run in `spec.cwd`, a verbatim prefix removed only when the result names the same directory. | PR2 §2.1; W6 | `pty_windows::cwd_is_honored` (`report-process` with a `\\?\`-prefixed canonical temp dir: `cwd` is the path without the prefix); unit `cwd_normalization_rule` (`\\?\C:\a` → `C:\a`; `\\?\UNC\s\x` → `\\s\x`; `\\?\C:\a.` and `\\?\C:\a\..\b` refused) |
| PR4-CL-11 | When `spawn` returns, the pseudoconsole shall have `spec.size`, and a resize shall reach the CLI. | PR2 §2.1 | `pty_windows::size_reaches_the_cli` (`attachable` at 100×30: `size` 100×30, pixels null; after `resize(120, 40)` a `winch` with 120×40) |
| PR4-CL-12 | The child's standard handles shall be the pseudoconsole's even when the daemon's three are pipes. | §2.2 stage 5; W9 | `pty_windows::child_stdio_is_the_pseudoconsole` (helper process started with stdin, stdout, and stderr all piped: it spawns `report-process`, and the file shows `stdin_console`, `stdout_console`, `stderr_console` all true; without `STARTF_USESTDHANDLES` the child gets the helper's pipes) |
| PR4-CL-13 | The drain shall start before the process is created and receive output from the first byte; output shall not reach EOF before the pseudoconsole closes, and shall reach EOF after it. | PR2 §2.1, §2.4; W4 | unit `drain_starts_before_the_process` (trace: drain start before the first `CreateProcessW`), `output_ends_only_after_close` (`cmd.exe /d /c echo READY`: after `Exited` and the drain's marker watch having seen `READY`, the drain has no end; after `control.close` it ends `Eof` within 10 s) |
| PR4-CL-14 | A write shall be accepted whole, bytes shall reach the CLI intact, and dropping the input shall write nothing. | PR2 §2.1 | `pty_windows::input_reaches_the_cli` (`echo` in raw mode: unit `hello` → `input` event `68656c6c6f`), `large_write_is_accepted_whole` (64 KiB of the `a`–`z` pattern to `echo --raw --validate-input 65536`: `Written`, `bytes_written` 65 536, `split_writes` 0, then `validated` with `bytes` 65 536 and `ok` true); unit `dropping_input_writes_nothing` (the input type over a `std::io::pipe`: drop, the reader reads `Ok(0)` with no byte) |
| PR4-CL-15 | `wait_timeout` shall return by its timeout, poll on zero, not wait for a running `kill_tree`, and report the exit code on every call, reinterpreted as `i32`. | PR2 §2.1; §2.4 | unit `wait_timeout_returns_by_its_timeout`, `zero_timeout_polls`, `sub_millisecond_timeout_waits`, `chunked_wait_continues` (`sys::wait` with a 1 ms chunk cap in test builds: a 50 ms wait on an exiting child still sees the exit), `wait_timeout_runs_during_kill` (a hook inside `kill_tree` holding its lock: `wait_timeout(10 ms)` from another thread returns within 200 ms), `exit_code_is_reported_repeatedly` (`exit 7` twice), `large_exit_code_is_negative` (`exit -1073741510` → `0xC000013A as i32`) |
| PR4-CL-16 | When `kill_tree` runs, every job member shall die, a breakaway request by a member shall have been refused, the report shall be `tree_dead: true` with no survivor, and an unrelated process shall survive. | mvp §4 PR 4 obligation | `pty_windows::spawn_tree_dies_and_unrelated_survives` (roles `plain`, `double-forked`, `breakaway`, checked with `expect_roles`; the `breakaway` event says `denied`; every role pongs and `windows_in_job` is true before the kill; the test holds a watch handle per role across the kill; each watch signals after `Session::kill` returns and before any close or cleanup; `unrelated` answers `ping` afterwards) |
| PR4-CL-17 | `kill_tree` shall prove death only from a read of `ActiveProcesses == 0` in the same call with the leader and held extras signalled, nothing pending, and no malformed entry; it shall terminate the job again while members remain, return an already dead tree without terminating, compute each call from its own reads, start no OS call past the deadline, sleep no longer than the remaining budget, and report survivors with `tree_dead: false` at the deadline. | §2.5 | unit `windows::kill::tests` (synthetic `JobOs` and clock): `job_empty_and_handles_signalled_proves_death`, `nonzero_count_is_not_proof`, `unsignalled_extra_is_not_proof`, `pending_extra_is_not_proof`, `malformed_extra_is_not_proof`, `query_error_is_not_proof`, `terminate_failure_does_not_decide`, `members_left_are_terminated_again`, `dead_tree_returns_without_terminate`, `three_calls` (proven; then a live extra times out as a survivor; then an expired call still lists it), `survivors_are_confirmed_members` (a listed pid whose handle is not in the job is not reported), `list_failure_is_incomplete` (method `windows_job_incomplete`, leader and extras kept), `expiry_inside_a_list_retry_makes_no_further_call`, `expired_call_records_and_reports` (no OS call; supplied identities in `pending`), `busy_returns_the_supplied_identities` (`windows_busy`), `sleeps_never_pass_the_deadline` (every recorded sleep ≤ the remaining budget, including the busy poll and the query-error path) |
| PR4-CL-18 | An `extra` shall be terminated only through a handle whose creation time matched; a stale, gone, or reserved pid shall never be terminated; every supplied well-formed identity shall be kept until resolved, so a later call cannot prove death without it; malformed entries shall be survivors of their call only; the report shall hold one entry per pid. | PR2 §2.1; §2.5 | `pty_windows::extra_outside_the_job_dies_and_stale_extra_is_spared` (a sleeper started by the test outside the job with its matching identity, and the `unrelated` pid with start + 1, in one call: the sleeper's watch signals, `unrelated` answers; without `extra` the sleeper survives); unit `stale_extra_is_not_terminated`, `gone_extra_is_resolved`, `access_denied_extra_is_waited_not_killed`, `unverifiable_extra_is_retried_until_resolved`, `reserved_pids_are_never_opened` (0, 4, own), `held_extra_survives_across_calls`, `expiry_during_the_first_open_keeps_the_second` (call A with two extras expires inside the first `open`; call B with `[]` reports `tree_dead: false` with the second as a survivor; after it resolves `Gone`, call C proves death), `malformed_is_not_persisted` (a malformed entry fails its call only; the next call with `[]` can prove death), `duplicates_report_one_per_pid` |
| PR4-CL-19 | The leader's identity shall be read from its own handle, also when it already exited, and match `fake::identify`; `identity()` shall have no panic path without a record. | §2.1, §2.2 stage 6 | unit `immediate_exit_still_has_an_identity` (`exit 0` with a hook that waits for the exit before stage 6: `Ok`, then `Exited(0)`), `leader_identity_matches_identify` (through the session's `Arc<dyn PtyChild>`: pid and `start_time` equal; `identify`'s `group` is empty), `identity_failure_has_an_empty_start` |
| PR4-CL-20 | `close` shall return by the caller's deadline with output flowing, end the output with EOF, join its worker when it returns `Some`, and never leave a pseudoconsole without a closer. | PR2 §2.1, §2.4; W4 | `pty_windows::close_with_output_flowing_is_bounded` (`--emit-output`: within the budgets, `close.elapsed` is `Some`, `released.control`, `output == Some(Eof)`, `abandoned` empty); `drain.rs` unit `hung_close_worker_is_bounded`, `close_worker_joins_its_thread`, `unarmed_worker_exits`, `dropped_armed_worker_runs_the_close`; CL-3's `close_worker_fault_creates_no_pseudoconsole` |
| PR4-CL-21 | The child shall inherit no handle of the daemon. | §2.1 | unit `child_inherits_no_handle` (the test makes a pipe write end inheritable, spawns `cmd.exe /d /c pause` through the backend, and drops its own copy: `pipe_has_writer` is false while the child's `wait_timeout(0)` is `None`); positive control `inheriting_child_keeps_the_writer` (the same with a std `Command` child, which inherits: `pipe_has_writer` true while it lives, false after it is killed) |
| PR4-CL-22 | `#[allow(unsafe_code)]` shall occur exactly twice in `crates/pty-runtime/src`, on `unix::sys` and `windows::sys`, with no `unsafe` elsewhere and a `SAFETY` comment on every block. | PR2 D7 | `tests/lints.rs::unsafe_is_allowed_only_in_sys` (updated), with its existing scan tests; clippy `undocumented_unsafe_blocks` on `windows-latest` |
| PR4-CL-23 | While `CreatePseudoConsole` or a `CreateProcessW` call runs, no other child creation through `SPAWN_LOCK` shall start, and no failure-path wait or release shall hold the lock. | §2.1; W3 | unit `pty_spawn_waits_for_a_helper_spawn` (a `with_spawn_lock` closure parked on a gate; a PTY spawn on another thread does not reach the `LockAcquired { stage: 2 }` hook until it opens), `helper_spawn_waits_in_stage_2` and `helper_spawn_waits_in_stage_5` (a PTY spawn parked at `LockAcquired { stage: 2 }`, then at `LockAcquired { stage: 5 }`; a concurrent `with_spawn_lock` closure does not start until it is released), `failure_paths_release_the_lock` (`Assign`, `ResumeFails`, `DrainStart`: every release-trace entry records the lock not held); `create_pseudo_console` and `create_process` take `&SpawnGuard`, so a call without the lock does not compile |
| PR4-CL-24 | FakeCli shall report the breakaway result, the Windows `double-forked` hello after its parent's exit, the `report-process` file, console sizes, `output` events, and validated input. | §2.8 | testkit `fake_cli::breakaway_role_reports_its_result` (Windows, pipe mode: `accepted` or `denied`, then the role's hello), `double_forked_waits_for_its_parent` (the test starts `double-fork-parent` itself and holds its `Child`: when the grandchild's hello arrives the parent has exited (`try_wait` is `Some`), and the grandchild then answers `ping`), `report_process_writes_the_file`, `emit_output_reports_lines`, `validate_input_checks_the_pattern`, existing `attachable_on_a_pipe_reports_null_size` |
| PR4-CL-25 | `chaldea-testkit` shall depend on no workspace crate other than `chaldea-core`, on any target. | mvp §3 | CI step (unchanged; `winapi-util` is not a workspace crate) |
| PR4-CL-26 | The Windows CI job shall fail when any Windows test binary's tests are not compiled. | §1 | CI step `List the Windows tests` (§1 table) |
| PR4-CL-27 | When the daemon runs in a kill-on-close job without breakaway, a spawn shall take the job-list branch (nested), its tree shall die on kill, and another process of the parent job shall survive; when the parent job refuses nesting, the spawn shall fail and leave no running child. | §2.5 "Daemon inside a job" | unit `spawn_inside_a_parent_job_nests` (the test creates a kill-on-close job and keeps its handle; it starts a helper, the test binary re-executed with an environment variable naming the case, plus a sibling `cmd.exe /d /c pause`, assigns both to the job, then releases the helper through its stdin; the helper spawns `cmd.exe /d /c pause` through the backend, asserts `job_list`, kills with `tree_dead: true`, and exits 0; afterwards the sibling is unsignalled and still in the parent job; the test then terminates the parent job), `ui_restricted_parent_job_refuses_nesting` (the same with `set_ui_restrictions` on the parent job: the helper's spawn returns `NeverStarted` or `StartedThenFailed` with `Unresolved`, and the helper reports no running child of its own) |
| PR4-CL-28 | When the last handle to the session's job closes with a member running, the member shall be terminated. | §2.4; A4 | unit `dropping_the_child_kills_through_the_job` (spawn `cmd.exe /d /c pause`; take the parts, open a watch on the leader after a creation-time check, drop the only `Arc` of the child without `kill_tree` while the control and drain stay alive: the watch signals within 10 s; removing `KILL_ON_JOB_CLOSE` fails it independently of `TerminateJobObject`) |

**Obligation mapping** (PR2 §2.1 → claims): release on failure and `Unresolved` → CL-1, CL-2, CL-3, CL-6, CL-7; opened at `spec.size` → CL-11; environment → CL-8; drain from the first byte, started after `CreatePseudoConsole` and before `CreateProcessW` → CL-13; dropping input writes nothing, whole writes → CL-14; `wait_timeout` → CL-15; `kill_tree` with `extra`, deadline, survivors → CL-16, CL-17, CL-18; bounded `close` → CL-20; `evidence` (`job_branch`, `conpty_module`) → CL-4, CL-5; `unsafe` placement → CL-22. PR 4 row obligations: survivors and the unrelated negative control → CL-16, CL-18; F-2c → CL-3, CL-6, CL-7; job-list branch recorded → CL-4, CL-5. PR 3's process rules in Windows form: spawn lock → CL-23, no inheritance → CL-21; ownership → CL-15, CL-19 (exit read only through the held handle); kill-on-close → CL-28. No claim rests on a process-list walk: CL-17's proof reads the job's count and held handles; the job's pid list feeds the survivor report only.

## 7. Tests and Completion Commands

**Where tests run.**

| Tests | File | OS | Processes |
|---|---|---|---|
| Kill engine | `src/windows/kill.rs` `#[cfg(test)]` | Windows | none: synthetic `JobOs` and clock |
| Command line, environment block, cwd | `src/windows/cmdline.rs` `#[cfg(test)]` | Windows | none |
| Close worker, marker watch | `src/drain.rs` `#[cfg(test)]` | all | none |
| Stages, faults, exit, identity, inheritance, spawn lock, parent job, kill-on-close, PE imports | `src/windows/tests.rs` | Windows | `cmd.exe`; every test holds the module's lock; parent-job cases in a helper process |
| Public-API spawn: validation, env | `tests/windows_spawn.rs` | Windows | `cmd.exe`; every test holds the file's lock |
| Lints | `tests/lints.rs` | all | none |
| FakeCli modes without a PTY | `crates/testkit/tests/fake_cli.rs` | all (Windows roles behind `cfg(windows)`) | FakeCli |
| Native backend + FakeCli, F-2c, handle counts | `crates/testkit/tests/pty_windows.rs` | Windows | FakeCli; every test holds the file's lock and a `Cleanup` with `kill_with(fake::kill_verified)`; handle counts in a helper process |
| PR 2 `FakePty` tests, PR 3 macOS tests | unchanged | as before | as before |

**Timing.** No test synchronizes by sleeping. A test waits for a FakeCli hello or event (sent only once its mechanism is in place: the breakaway result, the parent's exit, an output line), `SessionEvent::Exited`, a stage hook, a gate, a pong, a handle's signal, or a `report-process` file after `Exited`, each bounded by 10 s; kill deadlines are 5 s with 500 ms of slack on real processes. Survival is proven by `ping` after `kill_tree` returned; death by a watch handle signalled, never by a stream's closing alone (a stream can also close by fixture timeout, a peer failure, or a later pseudoconsole close). Every event a test asserts is read before the fixture is killed, since Windows resets a killed process's socket and discards unread lines (PR3 §2.8 "Report channel"). Fault gates open on a milestone, so a fault never fires before the state it tests.

**Handle counts.** `pre_process_faults_leave_no_handle` and `after_process_faults_leave_no_handle` re-execute the `pty_windows` test binary with an environment variable naming the case (`--exact <test name>` selects the same test; the variable switches it into helper mode). The helper runs nothing else, so no other test thread opens handles; its FakeCli fixtures open no report stream (`report-process` or `sleeper` without `--report`), so it holds no sockets. Per fault it runs one warm-up cycle and five measured cycles; each cycle ends only after `close.elapsed` is `Some` (the close worker joined) and the drain finished, then samples `process_handle_count()`; the five samples must be equal, and `None` fails the test. What the count can see is the daemon's own handle table (process, thread, job, pipe, event, and thread handles); it cannot see an attribute-list heap block, conhost's handles, or a process that holds no handle of the daemon. Those are covered by the fault report (attribute-list deletes equal inits), the release trace, and exit checks (`fake::identify` of each leader is `None` after the cycle). The helper prints its result and exits; the outer test asserts exit 0.

**Leaks.** Every FakeCli runs with `--max-life-secs 60`; `Cleanup` kills by verified identity on any exit; a dropped `Session` closes the job, which kills its members.

**Completion commands** (PR3 §7, extended):

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings          # on each runner; Windows code is linted on windows-latest
cargo test --workspace --locked
cargo test -p chaldea-pty-runtime --lib --locked -- --list              # windows-latest only: the §1 list check
cargo test -p chaldea-pty-runtime --test windows_spawn --locked -- --list
cargo test -p chaldea-testkit --test pty_windows --locked -- --list
deps="$(cargo tree -p chaldea-core -e normal --prefix none)" && ! grep -Eq '^(tokio|rusqlite|axum) ' <<<"$deps"
deps="$(cargo tree -p chaldea-pty-runtime -e normal --prefix none)" && ! grep -Eq '^tokio ' <<<"$deps"
deps="$(cargo tree -p chaldea-testkit -e normal --target all --prefix none)" && bad="$(awk '/^chaldea-/ && !/^chaldea-(testkit|core) /' <<<"$deps")" && [ -z "$bad" ]
```

plus the `workspace` job green on both runners and the `pty-hooks` job unchanged. **Clippy for Windows** is authoritative only in CI (`workspace (windows-latest)`). A macOS developer can check it locally with `rustup target add x86_64-pc-windows-msvc` and `cargo clippy --workspace --all-targets --locked --target x86_64-pc-windows-msvc -- -D warnings` (type-checks without linking; not verified for this workspace while writing this design); lefthook does not run it, because it would fail on machines without the target.

## 8. Decisions

**D1. An own `windows-sys` backend with flags `0`, bound statically to `kernel32.dll`.** No search-path DLL can supply ConPTY (W1); the loaded module is recorded per launch and the binary's imports are checked.

**D2. The child is in a kill-on-close job before its first instruction, or it does not run.** Job list first; `CREATE_SUSPENDED` + assign + `ResumeThread == 1` second, only when the first failure can come from the job list. A failure on the suspended branch terminates the child (through the job once assigned) and reports `NeverStarted` only when its exit is confirmed; otherwise `StartedThenFailed` with `Unresolved` and its identity, with no retry owner. The branch, and the job-list error on the fallback, are evidence of every launch.

**D3. `CreatePseudoConsole` and each `CreateProcessW` call run under the process-wide spawn lock, proven by a guard type only that lock produces, and the backend creates nothing inheritable.** `CreatePseudoConsole` holds inheritable duplicates of conhost's pipe ends during the call (W3); everything else is non-inheritable from creation and the CLI is created with `bInheritHandles = FALSE`. Failure-path waits and releases run without the lock.

**D4. Death is proven from the job's own count and from held handles.** `ActiveProcesses == 0` in the call, the leader and held extras signalled, nothing pending, and no malformed entry; the job is terminated again while members remain; the pid list serves only the survivor report, collected within the deadline. The guarantee covers the job and verified `extra` identities; processes never in the job and outside-job descendants of an extra are outside it. Kill-on-close terminates; it never proves.

**D5. No breakaway.** The job sets only `KILL_ON_JOB_CLOSE`; a CLI's breakaway request fails, and a breakaway from an inner job stops at this one (W11). Reopen with evidence that a supported CLI needs breakaway, at the cost of a stated escape path.

**D6. Extras are trusted, launch-attributed input, opened by pid, verified by creation time on the same handle, and killed and waited for only through it; held handles keep their pids reserved; every supplied identity is kept until resolved.** No terminate by pid alone; no Toolhelp. A busy call returns the supplied identities for the caller to resubmit.

**D7. Output ends at close, and the pseudoconsole always has a closer.** conhost's pipe ends are closed in the host right after `CreatePseudoConsole`, `ReleasePseudoConsole` is not used, and the close worker is started before the pseudoconsole exists and runs `ClosePseudoConsole` against the caller's deadline while the drain reads.

**D8. Synchronous std pipes; `PipeWriter` is the input and `PipeReader` the drain's source.** Whole writes, `ERROR_BROKEN_PIPE` as EOF, nothing written on drop, no `unsafe`.

**D9. `unsafe` lives in `windows::sys`, the second and last allowance of the crate.**

**D10. Faults are injected through `cfg(test)` hooks and a `fake`-feature thread-local, and make the failing call fail for real where the path under test depends on it; leaks are judged by handle counts stable across cycles in a quiescent helper, by traces, and by handles that signal.** No fault path ships in a default build.

**D11. FakeCli uses `winapi-util` for console mode and size on Windows**, keeping testkit free of `unsafe` and the lock unchanged; new Windows facts arrive in opt-in modes, flags, and events, so PR 3's types and tests are unchanged.

**D12. `ProcessIdentity::group` on Windows, persisted as `launch_started.pgid_or_job`, is `job:<leader pid>` for job members and `""` for a process outside the job, including a suspended child whose assignment failed.** It is opaque display and diagnostic text: `launch_id` is the durable correlation, and no reader parses it as a reopenable job, a restart proof, or membership evidence. Rejected: an empty value (loses the containment fact in `launch_started` and survivor lists) and a named job (would let another process open the job, contradicting D3 and §4's restart reasoning). Once events are stored, readers accept the old values forever (append-only store, mvp §2.1).

No open questions remain.

## Related Text in Other Documents

These documents state parts of this design or would need a change if it is adopted (not edited here):

- `docs/design/pty-runtime.md` §2.1: "On Windows the release order is: terminate job and process, close the pseudoconsole while the drain reads, close pipe ends", while the shared `SpawnedParts::teardown` and close order drop the input write end before closing the pseudoconsole; both are safe because the tree is killed first, and this design's pre-process path (`release_early`) follows the §2.1 order. The drain obligation "returns `Ok(0)` once every slave end is closed" holds on Windows when conhost, the last writer, closes its end, normally at `ClosePseudoConsole` (W4). The `evidence` list gains `job_list_error`; `KillReport::method` gains `windows_job_incomplete` and `windows_busy` (resubmit contract). §2.4: `close_bounded` is replaced by `CloseWorker`, started before the pseudoconsole and measured against the caller's deadline; `CloseOutcome::abandoned` could name `conpty-close` (today a hung close shows only as `close.elapsed == None`). §2 threading model: the spawn lock's Windows role and the `SpawnGuard` type (D3).
- `docs/design/pty-unix.md` §2.1 and `crates/pty-runtime/src/spawn.rs`'s doc comment: "the lock matters on macOS" becomes "on macOS (exec-error pipe) and Windows (inheritable handles during `CreatePseudoConsole`)", and `wake_pipe` takes `&SpawnGuard`. §2.8: Windows roles (`breakaway`, the Windows `double-forked`), `report-process`, `--emit-output`, `--validate-input`, `Cleanup::kill_with`, and the Windows terminal functions. §4's PR 4 row.
- `docs/mvp-design.md` §2.1: the format of `pgid_or_job` on Windows (D12), documented as opaque text. §3: testkit's Windows dependency (`winapi-util`) and the FakeCli lift table. §4 PR 8 obligations: the Windows form of the process rules (`CreatePseudoConsole` and `CreateProcessW` under the lock are the backend's; the daemon never closes a handle it did not open; a parent job that refuses nesting makes spawns fail). §4 PR 9 / PR 10: `extra` identities must be attributed to the launch before the daemon passes them (a matching creation time identifies a process, not its provenance). §4 PR 10: on Windows the old tree is terminated by the daemon's job handle closing, which is not a death proof; and a daemon crash between a suspended `CreateProcessW` and `AssignProcessToJobObject` leaves a suspended, unrecorded child outside any Chaldea job. §4 PR 16: sharing violations right after a confirmed Windows stop (§4).
- `docs/architecture.md`, "Windows PTY backend and process tree": the job is created without breakaway, and output EOF comes when conhost closes the output pipe, normally at `ClosePseudoConsole`.
