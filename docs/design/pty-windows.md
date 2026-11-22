# Design: Windows ConPTY Backend (MVP PR 4)

Status: design, not implemented. Scope is the PR 4 row of `docs/mvp-design.md` §4 (cited as "mvp §N"): the Windows backend of `PtyRuntime`, an own `windows-sys` backend with `CreatePseudoConsole` flags `0` from `kernel32.dll` only, `CreateProcessW` with `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` and `PROC_THREAD_ATTRIBUTE_JOB_LIST` (the child is in a kill-on-close job from creation), `CREATE_SUSPENDED` + assign + resume as the fallback when the job list is rejected, stop by job termination plus the recorded-descendant kill, and `close_bounded`. Its design obligations: FakeCli `spawn-tree` survivors and the unrelated negative control on Windows; injected setup failures leave no process or handle (spike F-2c); the job-list branch actually taken is recorded. It also adds the FakeCli features PR 3 deferred to PR 4 (`docs/design/pty-unix.md` §2.8): the `breakaway` role, the Windows console size for `attachable`, and `--emit-output` for the F-2c cases.

The contract implemented is `docs/design/pty-runtime.md` (cited as "PR2 §N") §2.1 "What each backend must supply"; the sibling backend is `docs/design/pty-unix.md` (cited as "PR3 §N"), whose spawn rule (PR3 §2.1) and ownership rule (PR3 §2.4) this backend follows in their Windows form. No trait or public type of PR 2 or PR 3 changes; additions are new `fake`-feature functions, new FakeCli flags and events, and one additive `Cleanup` method.

Out of scope: daemon wiring and the daemon-in-a-job check (PR 8), input encoders including `win_keys` (PR 5), the gate and send path (PR 10), attach (PR 11), and Codex hook trust (PR 14).

Sources read for this document (paths are relative to each root):

| Source | Revision | Read for |
|---|---|---|
| This repository | `6f81b14` | `crates/pty-runtime/src/{backend,spawn,drain,session,lib,fake}.rs`, `src/unix/{mod,tests}.rs`, `tests/lints.rs`; `crates/testkit/src/{report.rs,fake_cli/*}`; `Cargo.toml`, `Cargo.lock`, `.github/workflows/ci.yml`, `lefthook.yml`; spike `spikes/pty-hooks/src/os/windows.rs`, `Cargo.toml`; `docs/spikes/pty-hooks-spike.md` §3, §4.6, F-2a–F-2c; `docs/prior-art.md` rows 1a, 1b, 1d, 3b, 8, 9; `docs/architecture.md` (Windows paragraphs) |
| Rust std | 1.96.0 `rust-src` | `sys/process/windows.rs` (command line, environment block, `inherit_handles`, `CREATE_PROCESS_LOCK`), `sys/args/windows.rs` (argument quoting), `sys/pipe/windows.rs` (`io::pipe`), `sys/pal/windows/handle.rs` (`ERROR_BROKEN_PIPE` on read), `os/windows/io/handle.rs` (`PipeReader`/`PipeWriter` from `OwnedHandle`) |
| `windows-sys` | crate 0.61.2 (in `Cargo.lock`) | `Win32/System/{Console,JobObjects,Threading,Pipes}/mod.rs` (declarations, constants, feature gates) |
| `windows-link` | crate 0.2.1 (in `Cargo.lock`) | `src/lib.rs` (the `link!` macro) |
| `winapi-util` | crate 0.1.11 (in `Cargo.lock`, through `termcolor`) | `src/console.rs`, `src/win.rs` (safe console mode and screen buffer info) |
| `portable-pty` | crate 0.8.1 | `src/win/{psuedocon,mod,procthreadattr}.rs` |
| `alacritty_terminal` | crate 0.26.0 (in `Cargo.lock`) | `src/tty/windows/{conpty,child}.rs` |
| Codex (`openai/codex`, sparse `codex-rs/utils/pty`) | `cf3ff15` (default branch 2026-11-22) | `src/win/{psuedocon,procthreadattr,job,mod}.rs` |
| Windows Terminal (`microsoft/terminal`, sparse `src/winconpty`) | `cf5e6bf` (2026-11-20) | `winconpty.cpp` (the `CreatePseudoConsole` implementation; its comment at line 225 notes that it is built into kernelbase) |

**Facts from source** this design relies on (W). Every behaviour that only a Windows host can show is an assumption (A, below) checked by a named test on `windows-latest`.

| # | Fact | Where |
|---|---|---|
| W1 | `windows-sys` declares `CreatePseudoConsole`, `ResizePseudoConsole`, `ClosePseudoConsole` as `kernel32.dll` imports through `windows_link::link!`, which expands to `#[link(name = "kernel32.dll", kind = "raw-dylib", modifiers = "+verbatim")]`: the binary's import table names `kernel32.dll` itself. `kernel32.dll` is a KnownDLL, mapped from the system directory and never searched for (Microsoft, "Dynamic-link library search order"). No `ReleasePseudoConsole` is declared. | windows-sys `Win32/System/Console/mod.rs:5`, `:9`, `:65`; windows-link `src/lib.rs:20-27` |
| W2 | `std::io::pipe()` on Windows is `CreatePipe(&r, &w, NULL, 0)`: synchronous, non-inheritable handles. `PipeReader::read` maps `ERROR_BROKEN_PIPE` to `Ok(0)`. Both ends convert from and to `OwnedHandle`. | std `sys/pipe/windows.rs:8-19`, `sys/pal/windows/handle.rs:173-180`, `os/windows/io/handle.rs:685-704` |
| W3 | `CreatePseudoConsole` duplicates `hInput` and `hOutput` as **inheritable** handles and makes its signal pipe end inheritable, then starts conhost with `bInheritHandles = TRUE` restricted by `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`; the duplicates are closed when the call returns. A concurrent `CreateProcess` elsewhere in the process with `bInheritHandles = TRUE` and no handle list inherits them during the call. | winconpty `winconpty.cpp:160-167`, `:236-269`, `:494-498` |
| W4 | conhost keeps running while the pseudoconsole's `\Reference` handle is open, so a read of the output pipe returns `ERROR_BROKEN_PIPE` only after `ReleasePseudoConsole` or `ClosePseudoConsole`. The open-source close only closes handles; Microsoft's `ClosePseudoConsole` documentation says the call can wait until the final frame on the output pipe is drained, indefinitely if nobody reads it. | winconpty `:558-576`, `:395-432`, `:585-589`; Microsoft, "ClosePseudoConsole function"; alacritty `tty/windows/conpty.rs:97-104` |
| W5 | Flags `0` start conhost without `--inheritcursor`, so it sends no startup cursor query that would wait for a reply. | winconpty `:169`, `:196` |
| W6 | std spawns default to `bInheritHandles = TRUE`; std's `CREATE_PROCESS_LOCK` serializes only std's own spawns. std sorts the environment block with `CompareStringOrdinal(…, TRUE)`, adds a second NUL for an empty block, strips a `\\?\` prefix from the cwd when the rest is absolute, always quotes the program name, and quotes an argument only when it is empty or contains a space or tab, doubling backslashes before a quote. | std `sys/process/windows.rs:74-93`, `:193`, `:330-341`, `:859-875`, `:887-911`, `:914`; `sys/args/windows.rs:173-212` |
| W7 | Codex requires the job list and has no fallback (`procthreadattr.rs:85-88`); its job sets `JOB_OBJECT_LIMIT_BREAKAWAY_OK` (`job.rs:79-82`); its background launcher resumes a child uncontained when assignment fails (`job.rs:169-196`); "TerminateJobObject can empty the job list before members release their handles" (`job.rs:286`); member lists are sized by `ERROR_MORE_DATA` retries (`job.rs:337-365`); `TerminateProcess` fails `ERROR_ACCESS_DENIED` on an exited process (`job.rs:367-374`). It uses flags `RESIZE_QUIRK` (`psuedocon.rs:62`, `:144`), loads the functions from `kernel32.dll` by name (`:92-98`), keeps its copies of conhost's pipe ends until the child exists (`:116-121`, `:221`), and calls `ReleasePseudoConsole` where exported (`:222-230`). | Codex `utils/pty/src/win/` |
| W8 | portable-pty 0.8.1 prefers a `conpty.dll` found on the DLL search path (`psuedocon.rs:44-59`), passes flags `0x6` (`:86`), and builds a one-attribute list (`:124`); alacritty 0.26.0 also prefers `conpty.dll` (`tty/windows/conpty.rs:31-90`), passes flags `0` (`:123-130`) and `bInheritHandles = FALSE` (`:219-230`). | crates as listed |
| W9 | The spike's raw launch closes its copies of conhost's pipe ends right after `CreatePseudoConsole` (`os/windows.rs:1728-1731`), creates the job without `BREAKAWAY_OK` (`:242-260`), retries a breakaway spawn without the flag on `ERROR_ACCESS_DENIED` (`:456-466`), passes `STARTF_USESTDHANDLES` with `INVALID_HANDLE_VALUE` standard handles (`:1780-1786`; Codex `psuedocon.rs:172-177`), accepts the suspended branch only when `ResumeThread` returns 1 (`:1860-1871`), and releases a failed setup as job and process, pseudoconsole (bounded), pipes, drain (`:1454-1512`). On the spike host every uninjected raw launch took the job-list branch, and F-2c handle counts settled after a warm-up (arch, Windows PTY paragraph). | spike |
| W10 | `winapi-util` 0.1.11 offers safe `console::mode`, `console::set_mode`, and `console::screen_buffer_info(..).window_rect()` over standard handles. | `src/console.rs:31-95`, `:139-151`; `src/win.rs:124-135` |

## 1. Goal and Files

**Goal.** A Windows backend that opens a pseudoconsole at the requested size from the inbox `kernel32.dll`, drains its output from before the child exists, starts the CLI with exactly the requested argv, cwd, and environment inside a kill-on-close job from its creation (job list, else suspended + assign + resume, else no launch), records which branch ran, kills the tree by job termination plus verified recorded descendants and proves death from the job's own state and the handles it holds, closes the pseudoconsole within a deadline, and releases everything on every failure path. FakeCli gains what the Windows tests need. Every claim of §6 is checked by a named test on `windows-latest` with a real ConPTY; `macos-latest` builds everything and runs the platform-independent parts.

**Files.**

```
Cargo.toml                                  # [workspace.dependencies] += windows-sys (features), winapi-util
crates/pty-runtime/Cargo.toml               # [target.'cfg(windows)'.dependencies] windows-sys
crates/pty-runtime/src/lib.rs               # #[cfg(windows)] mod windows; pub use windows::native; dead_code allowance narrowed
crates/pty-runtime/src/spawn.rs             # spawn_lock_held() also for cfg(all(test, windows))
crates/pty-runtime/src/backend.rs           # teardown trace point also for cfg(all(test, windows))
crates/pty-runtime/src/windows/mod.rs       # WindowsPty: PtyRuntime; validation; the spawn sequence (§2.2); native()
crates/pty-runtime/src/windows/cmdline.rs   # command line, environment block, cwd encoding (pure; unit tests)
crates/pty-runtime/src/windows/control.rs   # Pcon, WindowsControl: PtyControl (resize, bounded close) (§2.3)
crates/pty-runtime/src/windows/child.rs     # WindowsChild: PtyChild; identity; wait_timeout (§2.4)
crates/pty-runtime/src/windows/kill.rs      # the kill engine over a `JobOs` trait; unit tests with a synthetic `JobOs` and clock (§2.5)
crates/pty-runtime/src/windows/sys.rs       # the only Windows module allowed `unsafe` (§2.7)
crates/pty-runtime/src/windows/hooks.rs     # #[cfg(any(test, feature = "fake"))] faults, gates, trace (§2.6)
crates/pty-runtime/src/windows/tests.rs     # #[cfg(test)] real-process unit tests with system binaries
crates/pty-runtime/src/fake.rs              # += #[cfg(windows)] identify, kill_verified, process_handle_count,
                                            #    windows_in_job, windows_inject_next, windows_fault_report (§2.6)
crates/pty-runtime/tests/windows_spawn.rs   # #![cfg(windows)] public-API spawn tests; every test holds one lock
crates/pty-runtime/tests/lints.rs           # two allowances: unix::sys and windows::sys
crates/testkit/Cargo.toml                   # [target.'cfg(windows)'.dependencies] winapi-util
crates/testkit/src/fake_cli/{mod,term,spawn_tree,sleeper,attachable}.rs   # Windows additions (§2.8)
crates/testkit/src/report.rs                # Cleanup::kill_with (additive)
crates/testkit/tests/fake_cli.rs            # Windows role tests without a PTY
crates/testkit/tests/pty_windows.rs         # #![cfg(windows)] native backend + FakeCli (§7)
.github/workflows/ci.yml                    # windows-latest: a step that fails when the real-ConPTY tests are not compiled in
```

No Unix file moves. `crate::windows` is a module of this crate; the external crate is always named `windows_sys`.

**Dependencies.** Exact pins; the lock gains no package (`windows-sys 0.61.2` and `winapi-util 0.1.11` are already locked; features only add declarations).

```toml
# root [workspace.dependencies]
windows-sys = { version = "=0.61.2", features = [
  "Win32_Foundation",          # HANDLE, FILETIME, CloseHandle, error codes, WAIT_*
  "Win32_Security",            # gates CreateProcessW and CreateJobObjectW (SECURITY_ATTRIBUTES pointers)
  "Win32_System_Console",      # CreatePseudoConsole, ResizePseudoConsole, ClosePseudoConsole, COORD, HPCON
  "Win32_System_JobObjects",   # job create, limits, assign, terminate, accounting, pid list, IsProcessInJob
  "Win32_System_Threading",    # CreateProcessW, attribute list, OpenProcess, GetProcessTimes, waits, ResumeThread
  "Win32_System_LibraryLoader",# GetModuleHandleW, GetModuleFileNameW (the conpty_module evidence)
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

`Win32_System_Pipes` is not needed: the pipes come from `std::io::pipe` (W2). No Toolhelp feature: the backend never walks the process list.

**Workspace and CI.** `cargo test --workspace --locked` on `windows-latest` now runs the `#[cfg(windows)]` real-ConPTY tests (pty-runtime unit tests, `tests/windows_spawn.rs`, testkit `tests/pty_windows.rs`); on `macos-latest` `cfg` removes them. One step is added to the `workspace` job, on Windows only: `cargo test -p chaldea-testkit --test pty_windows --locked -- --list` must list at least the tests named in §6 (a `cfg` mistake that compiles them out fails CI instead of passing silently). `lefthook.yml` is unchanged: Windows clippy runs in CI (§7). `lib.rs`'s `allow(dead_code)` for non-macOS default builds becomes `all(not(feature = "fake"), not(any(target_os = "macos", windows)))`.

## 2. Types and Functions

Names may change in review; shapes and rules may not.

### 2.1 Public Additions and the Process Rules in Windows Form

```rust
/// The native backend of this OS (macOS: PR 3; Windows: this document).
#[cfg(windows)]
pub fn native() -> Arc<dyn PtyRuntime>;
```

`with_spawn_lock` is unchanged. **The spawn rule on Windows.** Every child the daemon creates is created under `SPAWN_LOCK` (PR3 §2.1). On Windows the hazard is handle inheritance, not a fork: an inheritable handle that exists while another thread runs `CreateProcess` with `bInheritHandles = TRUE` and no handle list goes to that unrelated child. std's own lock covers only std spawns (W6), and `CreatePseudoConsole` itself holds inheritable duplicates of conhost's pipe ends while it starts conhost (W3); an unrelated child that inherits the output pipe's write end keeps the session's output from reaching EOF until that child exits. So this backend runs **`CreatePseudoConsole` and the `CreateProcessW` stage (both attempts, and assign + resume on the fallback) under `SPAWN_LOCK`**; the functions that call them take the lock's guard as a parameter (§2.7). Everything else the backend creates is non-inheritable from creation (`std::io::pipe`, `CreateJobObjectW(NULL, NULL)`, handles from `CreateProcessW` and `OpenProcess` with `bInheritHandle = FALSE`), and its own `CreateProcessW` passes `bInheritHandles = FALSE`, so it neither gives nor takes an inherited handle. **Constraint:** a library that creates children outside the lock with `bInheritHandles = TRUE` can still catch W3's duplicates; the daemon uses none (a PR 8 / PR 10 obligation, as on macOS).

**Ownership on Windows.** The backend holds the leader's process handle from `CreateProcessW` until the `WindowsChild` drops. Windows has no reaping: the exit code stays readable through that handle, and the process object, with its pid, cannot be reused while the handle is open. So ownership cannot be lost to another waiter, and no `lost` state exists. The one daemon-side rule is that no code closes a handle it did not open (every handle is an `OwnedHandle`; no `CloseHandle` on a raw value), since closing the backend's handle by mistake could free the pid.

**Identity format** (`ProcessIdentity`): `pid`; `start_time` the process creation time from `GetProcessTimes` on a handle to that process, a `FILETIME` as decimal 100-ns units since 1601-01-01 UTC, used only for equality; `group` is `job:<leader pid>` for the leader and for job members the kill reports, and empty for a process outside the job (open question Q1). The creation time is wall-clock: nothing orders processes by it.

### 2.2 Spawn

```rust
struct WindowsPty { #[cfg(any(test, feature = "fake"))] hooks: hooks::Hooks }   // sealed::Sealed + PtyRuntime
```

**Validation** (before anything is created; a failure is `NeverStarted` with `Cleanup::Confirmed`). It also builds the three UTF-16 buffers, so no buffer error can happen after a resource exists.

| Check | Failure |
|---|---|
| `argv` non-empty; no NUL in any argument | `Setup` (`InvalidInput`) |
| `argv[0]` absolute and without `"` | `Setup` (`InvalidInput`) |
| `argv[0]` extension is not `.bat` or `.cmd` (ASCII case-insensitive): `CreateProcessW` would run them through `cmd.exe`, which re-parses the arguments | `Setup` (`InvalidInput`) |
| `metadata(argv[0])` | `NotFound` → `NotFound`, `PermissionDenied` → `PermissionDenied`, other → `Setup` |
| `argv[0]` is a regular file | `PermissionDenied` |
| `cwd` absolute and a directory | `Setup` (`NotFound` kind when missing) |
| env keys non-empty, without `=` or NUL; values without NUL; no two keys equal under `CompareStringOrdinal(…, TRUE)` | `Setup` (`InvalidInput`) |
| command line, NUL included, at most 32 767 UTF-16 units (Microsoft, "CreateProcessW function", `lpCommandLine`) | `Setup` (`InvalidInput`) |

**Buffers** (`cmdline.rs`, pure):

- *Application name*: `argv[0]` as given, NUL-terminated. `CreateProcessW` with a non-null `lpApplicationName` neither searches the path nor appends an extension (Microsoft, "CreateProcessW function").
- *Command line*: `argv[0]` always quoted, then each argument separated by one space and quoted by std's rule (W6): quoted when empty or containing a space or tab; inside, `n` backslashes before a `"` become `2n + 1` followed by `\"`, and `n` trailing backslashes before the closing quote become `2n`. These are the rules the MSVC CRT and Rust's own argument parser undo ("Parsing C++ command-line arguments").
- *Environment block*: `spec.env` sorted with `CompareStringOrdinal(…, TRUE)` ("Changing environment variables": the block is sorted case-insensitively, in Unicode order, without regard to locale), each `KEY=VALUE\0`, then a final `\0`; an empty `spec.env` gives `\0\0`. Nothing is inherited: `CREATE_UNICODE_ENVIRONMENT` with this block.
- *Current directory*: `spec.cwd`, with a `\\?\` (or `\\?\UNC\`) prefix removed when the rest is an absolute path (W6: the current directory does not accept verbatim paths), NUL-terminated.

**Sequence.** Each stage lists what exists when it fails. `release_early` is: `control.close(2 s)` (bounded, §2.3) while the drain reads, drop the input write end, `Drain::finish(2 s)`, drop the job; `Cleanup::Confirmed` when the close returned and the drain ended, else `Unresolved { kill: None, close: Some(..), drain_abandoned }`.

1. **Pipes.** `std::io::pipe()` twice: `(in_r, in_w)` and `(out_r, out_w)` (W2). Failure: what exists is dropped; `NeverStarted { Setup }`.
2. **Pseudoconsole**, under `SPAWN_LOCK`: `sys::create_pseudo_console(&guard, spec.size, &in_r, &out_w)` = `CreatePseudoConsole(COORD { X: cols, Y: rows }, in_r, out_w, 0, &hpc)`. On success the `Pcon` goes straight into a `WindowsControl` (§2.3), whose drop closes it within a bound, so no later path can forget it. Then `in_r` and `out_w` (the backend's copies of conhost's ends) are dropped: conhost holds its own (W3), and output then reaches EOF only when conhost closes its end (W4). Read the `conpty_module` evidence: `GetModuleHandleW("conpty.dll")` must be null (the path is recorded if not; that is a defect the CL-4 test catches), then `GetModuleFileNameW(GetModuleHandleW("kernel32.dll"))`. Failure (HRESULT < 0, mapped to the Win32 code when its facility is `FACILITY_WIN32`, else `other`): drop the pipes; `NeverStarted { Setup }`.
3. **Drain.** `Drain::start(out_r, output)` (PR2 §2.4), before any process exists (PR2 §2.1). Failure: `Drain::start` drops `out_r` first, so a waiting `ClosePseudoConsole` gets a broken pipe instead of an undrained one; then `control.close(2 s)`, drop `in_w`; `NeverStarted { Setup }` with `Confirmed` when the close returned.
4. **Job.** `sys::create_job()`: `CreateJobObjectW(NULL, NULL)` (unnamed, not inheritable), then `SetInformationJobObject(JobObjectExtendedLimitInformation)` with `LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and nothing else (no `BREAKAWAY_OK`, no `SILENT_BREAKAWAY_OK`; §3 "Breakaway"). Failure: `release_early`; `NeverStarted { Setup }`.
5. **Create**, under `SPAWN_LOCK` for the whole stage:
   1. *Job-list attempt.* `AttrList::new(2)`, `set_pseudoconsole(&pcon)`, `set_job_list(&[job])`; `STARTUPINFOEXW` with `cb`, `dwFlags = STARTF_USESTDHANDLES`, the three standard handles `INVALID_HANDLE_VALUE` (so the child's standard handles are the pseudoconsole's, never the daemon's, W9), `lpAttributeList`; `CreateProcessW(app, cmdline, NULL, NULL, FALSE, EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT, env, cwd, &si, &pi)`. Success: the child is a job member before its first instruction (Microsoft, "UpdateProcThreadAttribute function", `PROC_THREAD_ATTRIBUTE_JOB_LIST`). Branch `job_list`.
   2. *Suspended attempt*, only when any step of 5.1 failed (an attribute error included; both are recorded): `AttrList::new(1)` with the pseudoconsole only, the same call with `| CREATE_SUSPENDED`. On success, `AssignProcessToJobObject(job, hProcess)`, then `ResumeThread(hThread)` must return 1. Branch `suspended`.
   3. Both attempts failed: no process exists (`CreateProcessW` returned `FALSE`). `release_early`; `NeverStarted` whose `error` is the suspended attempt's error, rewritten as `io::Error::new(kind, "job_list: <e1>; suspended: <e2>")` when the two Win32 codes differ. `kind`: `ERROR_FILE_NOT_FOUND` or `ERROR_PATH_NOT_FOUND` with `metadata(argv[0])` now `NotFound` → `NotFound`; `ERROR_ACCESS_DENIED` → `PermissionDenied`; anything else (`ERROR_DIRECTORY` for a cwd, `ERROR_BAD_EXE_FORMAT` for a non-PE file) → `Setup`.
   4. Suspended branch failures. The child exists but has run no instruction of the program: assignment failed, `ResumeThread` failed (`u32::MAX`), or returned 2 or more (still suspended). `TerminateProcess(hProcess, 1)`, then `WaitForSingleObject(hProcess, 2 s)`; signalled → `release_early` and `NeverStarted { Setup, error }` with its `Cleanup`; not signalled → `StartedThenFailed { stage: "job_assign" | "resume", identity: Some, cleanup: Unresolved { kill: Some(report naming it), .. } }` (conservative: the daemon then treats a positional prompt as unconfirmed). `ResumeThread` returning 0 means the thread was not suspended and may have run: the child is in the job, so it is handled as a stage 6 failure (`StartedThenFailed { stage: "resume" }` through the teardown).
   The thread handle is closed at the end of the stage. The attribute list is deleted when the stage ends; the job array and the `Pcon` it points to outlive it by construction (§2.7).
6. **Settle.** `creation_time(&hProcess)` gives the start (§2.1). The `WindowsChild` (§2.4) is built at once, owning the process handle and the job. Failure of the read: `StartedThenFailed { stage: "identity", identity: None }`, released by `SpawnedParts::teardown` (`backend.rs:268-293`): `kill_tree(&[], 5 s)` (the job), drop input, `control.close(2 s)`, `Drain::finish(2 s)`.
7. **Hand over** `SpawnedPty::new(SpawnedParts { child, output: drain, input: Box::new(in_w), control: Box::new(control), size: spec.size, evidence })` with `evidence = [("job_branch", "job_list" | "suspended"), ("conpty_module", <path>)]`, plus `("job_list_error", "<stage>: <error>")` on the suspended branch. These are the keys PR2 §2.1 names, plus one additive key; `evidence` stays a crate-internal vector surfaced by `Session::evidence()`.

`spawn` has no deadline of its own: it waits for `SPAWN_LOCK`, and `CreatePseudoConsole` and `CreateProcessW` are synchronous kernel32 calls without a handshake. A failure after stage 6 builds the child is always `StartedThenFailed` released by the teardown, whose order (kill → input → close → drain) differs from `release_early` only in that a tree exists to kill first.

### 2.3 Input, Output, Control

- **Input** is `std::io::PipeWriter` (`in_w`), boxed as the session's `input`. `write` is one `WriteFile` on a synchronous anonymous pipe: it returns only when every byte is in the pipe (Microsoft, "WriteFile function", pipes: a write on a blocking pipe completes when a read makes room), so whole acceptance holds without a loop. A broken pipe (conhost gone) is an error, which the writer turns into `InputBroken` (PR2 §2.3). **Dropping it writes no bytes**: `PipeWriter`'s drop is `CloseHandle`. A write blocked on a full pipe ends when conhost goes away, which `ClosePseudoConsole` causes at close step 4 (PR2 §2.2); the writer thread is reported abandoned if step 3's budget runs out first, exactly as PR2 §2.2 allows. What conhost does when the input pipe reaches EOF is not relied on: every path that drops the input kills the tree first (PR2 §2.2 step 2, teardown) or follows conhost's death.
- **Output** is `std::io::PipeReader` (`out_r`), owned by the drain from stage 3. `ERROR_BROKEN_PIPE` reads as `Ok(0)` (W2), so the drain reports `OutputEnd::Eof` when conhost closes its end; any other read error is `OutputEnd::Error`. **EOF comes only after `ClosePseudoConsole`** (W4): child exit does not end the output on Windows, and the session's `Exited` (from `wait_timeout`) is the exit signal, as PR2 §2.2 already treats it. `ReleasePseudoConsole` is not called (§3).
- **Startup output.** conhost writes its own setup sequences (it requested `?9001h` on the spike host, arch "Windows inbox conhost") from before the child exists; the drain feeds them to the VT from the first byte. With flags `0` conhost sends no cursor query that waits for a reply (W5); if a build sends one, the responder's reply waits in the reply lane until the writer thread exists (PR2 §2.2 "Start"), and `spawn` does not depend on it. conhost answers DSR and DA itself and forwards the kitty query (prior-art row 3b); the responder answers what reaches it.

```rust
struct Pcon(HPCON);                                  // not a kernel handle; closed only by Pcon::close(self); Drop does nothing
struct WindowsControl { pcon: Option<Pcon> }         // PtyControl
```

- `resize(size)`: `ResizePseudoConsole(hpc, COORD { X: cols, Y: rows })`; an HRESULT failure maps as in stage 2. Unbounded (PR2 §2.1): conhost can wait on the drain.
- `close(deadline)`: takes the `Pcon` and calls `close_bounded(move || pcon.close(), deadline − now)` (PR2 §2.4). The drain keeps reading meanwhile (PR2 §2.2 step 4), so a build whose close waits for the output to drain (W4) returns. `CloseReport { elapsed }` is the helper's result: `None` when the `conpty-close` thread did not return in time, or could not be started. **`Pcon`'s drop never closes**: when `close_bounded` cannot start its thread, std drops the closure on the caller's thread, and a closing drop there would block without a bound. The pseudoconsole and its conhost are then leaked until the daemon exits, reported as `elapsed: None` (and the drain, with no EOF, as abandoned).
- `Drop` without `close` (a guard or session dropped on a path that never closed it): the same bounded close with `TEARDOWN_CLOSE` (2 s).

### 2.4 Child: Identity, Exit

```rust
struct WindowsChild {
    process: OwnedHandle,                // from CreateProcessW; PROCESS_ALL_ACCESS
    job: OwnedHandle,                    // KILL_ON_JOB_CLOSE: dropping the child kills what is left
    identity: ProcessIdentity,           // start_time empty only during a stage 6 failure
    exit: OnceLock<ChildExit>,
    kill: Mutex<KillState>,              // retained extras across calls (§2.5); wait_timeout never takes it
}
```

- `identity()` returns the stored value; no panic path.
- `wait_timeout(timeout)`: if `exit` is set, return it. Else `WaitForSingleObject(process, ms)` with `ms = min(remaining, u32::MAX − 1)`, repeated until `timeout` (a timeout above 49.7 days takes several waits). `WAIT_OBJECT_0` → `GetExitCodeProcess` → `ChildExit { code: Some(code as i32), signal: None }` (the 32-bit code reinterpreted, so `0xC000013A` is negative), stored once and returned on every later call; `WAIT_TIMEOUT` → `Ok(None)`; `WAIT_FAILED` → the error (`WaitFailed`). It takes no lock.
- The kill exit code is 1 (`TerminateJobObject(job, 1)`), as in the spike and Codex; a killed leader reports `code: Some(1)`.
- `Drop` closes the process handle and the job handle; closing the last job handle terminates any member still running (`KILL_ON_JOB_CLOSE`), so a session that is dropped never leaves its job's processes behind. The job handle is neither named nor inheritable, so no other process holds one.

### 2.5 Tree Kill

```rust
impl PtyChild for WindowsChild { fn kill_tree(&self, extra: &[ProcessIdentity], deadline: Instant) -> KillReport; }

pub(crate) trait JobOs {                       // the real one wraps sys; tests script it with a synthetic clock
    fn now(&self) -> Instant;
    fn sleep(&self, d: Duration);
    fn terminate_job(&self) -> io::Result<()>;
    fn active_processes(&self) -> io::Result<u32>;
    fn job_pids(&self) -> io::Result<Vec<u32>>;                          // survivor report only
    fn member_identity(&self, pid: u32) -> Option<ProcessIdentity>;     // open + IsProcessInJob + creation time; report only
    fn open(&self, id: &ProcessIdentity) -> Opened;                      // extras
    fn signalled(&self, h: &Held, wait: Duration) -> io::Result<bool>;   // WaitForSingleObject, capped to the budget
    fn terminate(&self, h: &Held) -> io::Result<()>;                     // TerminateProcess; ACCESS_DENIED after exit is not an error
}
pub(crate) enum Opened { Held(Held), Gone, Stale, Unverifiable }
pub(crate) struct KillState { held: BTreeMap<(u32, u64), Held>, unverified: BTreeSet<ProcessIdentity> }
```

**What the job guarantees.** A process created by a job member is a member (nested jobs included: a child job's processes count in the parent's accounting and die with `TerminateJobObject` on it; Microsoft, "Nested Jobs"). The leader is a member from creation (job list) or before it runs (suspended + assign). Nothing can join the job later: only stage 5 assigns, and the job handle is unnamed and not inheritable. So `JobObjectBasicAccountingInformation.ActiveProcesses == 0` read once means no process is associated with the job, and it stays 0.

**Outside the guarantee**, and reached only through `extra`: processes that were never in the job (started on the CLI's behalf by another process, such as a COM or packaged-app activation, a service, or WMI `Win32_Process.Create`; the spike saw an outside-job PowerShell under Codex, arch "Windows PTY backend"), and descendants of an `extra` identity that are outside the job (the backend does not walk the process list to find them, §3). A breakaway is refused: the job lacks `BREAKAWAY_OK`, so `CREATE_BREAKAWAY_FROM_JOB` from a member fails `ERROR_ACCESS_DENIED` (CL-16). Whether a member of a nested child job created by the CLI with `BREAKAWAY_OK` can leave this job is not established and not relied on: such a process would be outside the guarantee.

**Extras.** An `extra` identity is accepted only if its pid is not 0, not 4 (System), not the daemon's own, and its `start_time` parses as `u64`; otherwise it is kept unverified (a survivor until passed correctly). `open`: `OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, FALSE, pid)`, falling back to `PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE` on `ERROR_ACCESS_DENIED` (then it can be waited for but not killed); `GetProcessTimes` **on that handle**: equal start → `Held` (kept in `KillState` across calls, so its pid cannot be reused while the backend tracks it); another start → `Stale` (the recorded process is gone, since its pid was freed; never signalled); `ERROR_INVALID_PARAMETER` → `Gone`; any other failure → `Unverifiable` (kept by identity, retried next call, a survivor). A `Held` extra is terminated only through its own handle, so the identity check and the kill cannot refer to different processes; there is no residual reuse window. Only one remains: two processes with the same pid and the same 100-ns creation stamp after a wall-clock step back, which is not defended.

**The proof rule.** `tree_dead: true` exactly when, in this call: a read of `ActiveProcesses` returned 0; the leader's handle and every `Held` extra are signalled; and nothing is `Unverifiable`. A handle observed signalled is released from `KillState` at that read. Nothing proven by an earlier call is reused. **Bound:** a member counted out of the job has stopped running, but the job does not report when its handles (file locks among them) are released; Codex observes the job list emptying before members release their handles (W7). The leader and the held extras are waited for until signalled; other members are not (§4, worktree removal).

**Steps**, reading `now()` before every OS call:

1. **Expired or busy.** `now ≥ deadline`: no OS call and no blocking lock; `tree_dead: false`, survivors the leader and the accepted extras (one entry per pid); if one `try_lock` gets the state, the extras are stored for the next call. Else `try_lock` polled every 10 ms until the deadline; never acquired → the same report with `method: "windows_busy"`. A poisoned lock is taken over.
2. **Extras.** `open` each new accepted identity.
3. **Opening check.** `active_processes() == 0`, every held handle signalled with a zero wait, nothing unverifiable → `tree_dead: true` with no termination call (an already dead tree).
4. **Terminate.** `terminate_job()`; `terminate()` each held extra not yet signalled. A failure is recorded and the steps continue: the proof does not depend on these calls succeeding, only on what step 5 reads.
5. **Prove**, until the deadline: `active_processes()`; nonzero → `terminate_job()` again (a member that created a child as the termination began), sleep `min(10 ms, deadline − now)`, repeat. Zero → wait on the leader and each held extra for at most `min(100 ms, deadline − now)` each, re-terminating an unsignalled extra once per round; all signalled and nothing unverifiable → `tree_dead: true`. A query error is not proof and the loop continues until the deadline.
6. **At the deadline** `tree_dead: false`. Survivors: the job's members from `JobObjectBasicProcessIdList` (sized by `ERROR_MORE_DATA` retries up to 4 096 entries, W7), each opened with `PROCESS_QUERY_LIMITED_INFORMATION` and listed only if `IsProcessInJob` confirms membership on that handle (the pid may name a new process), with its creation time and `group` `job:<leader pid>` (empty `start_time` when it cannot be read); the leader if unsignalled; every unsignalled held extra and every unverifiable identity. One entry per pid. A failed list query leaves the leader and the extras as the survivors; the report is then incomplete but still `tree_dead: false`. The list is used for this report only, never for the proof.

`method` is `"windows_job"`, or `"windows_busy"`. There is no graceful phase: Windows has no signal for a console process tree that the daemon can send without attaching to the pseudoconsole, and closing the pseudoconsole first would kill console clients before the job proof and hide survivors (spike §4.6). `kill_tree` has no panic path. **Daemon inside a job** (PR 8): this job nests under any job the daemon is in, so nothing here needs breakaway; if the parent job refuses nesting, both attempts of stage 5 fail and the spawn is `NeverStarted` with that error, never an uncontained child.

### 2.6 Test Routes and Fault Injection

Faults live in `windows::hooks`, compiled under `cfg(any(test, feature = "fake"))`, so no default-feature build contains them (PR2 D6). Unit tests set them on a `WindowsPty`; integration tests use `fake::windows_inject_next(fault)`, which applies to the next native spawn **on the calling thread** (a thread-local), so parallel tests cannot see each other's faults. Unit tests also have PR 3's stage hooks (PR3 `src/unix/tests.rs`): `Validated`, `LockAcquired { stage: 2 | 5 }`, and `Spawned { pid }` before stage 6, plus a per-thread release trace at the release calls of §2.2 and `SpawnedParts::teardown`.

```rust
pub enum WindowsFault {                    // fake feature, Windows
    Pipes, PseudoConsole, DrainStart, Job, AttrList,            // fail that stage
    RejectJobList,                                              // fail attempt 5.1 without calling CreateProcessW: the fallback runs
    CreateProcess,                                              // fail both attempts without calling CreateProcessW
    Assign, ResumeFails,                                        // fail on the suspended branch, child suspended
    SuspendedExitUnconfirmed,                                   // the bounded wait in 5.4 reports a timeout
    Identity,                                                   // fail the creation-time read in stage 6
    AfterCreate(Gate),                                          // block right after CreateProcessW until the gate opens, then continue
    AfterSpawn(Gate),                                           // block after stage 6 until the gate opens and output has reached the drain, then fail
    KillExpired,                                                // the teardown's kill runs with an expired deadline
}
pub fn windows_inject_next(f: WindowsFault);
pub fn windows_fault_report() -> Option<FaultReport>;  // stages reached in order, CreateProcessW calls, drain bytes and child running at AfterSpawn
pub fn identify(pid: u32) -> io::Result<Option<ProcessIdentity>>;  // as on macOS; an exited process has an empty group
pub fn kill_verified(pid: u32, start_time: &str) -> bool;          // terminate through one handle after the creation-time check
pub fn process_handle_count() -> Option<u32>;                      // GetProcessHandleCount(GetCurrentProcess())
pub fn windows_in_job(leader_pid: u32, pid: u32) -> io::Result<bool>;  // IsProcessInJob against that session's job
```

`windows_in_job` reads a registry of live jobs keyed by leader pid (weak references, removed when the child drops), present only with `fake`. A `Gate` is a pair of `std::sync::mpsc` ends the test opens from another thread when its milestone arrives (a FakeCli event), so no test waits by sleeping. `AfterSpawn` additionally waits, bounded by 10 s, until `Drain::bytes() > 0` with the child still running; missing either is reported as "milestone not reached", never as the injected failure.

### 2.7 The `unsafe` Carve-Out

The crate's `[lints]` table (PR3 §2.7) is unchanged. A second module is allowed `unsafe`: `#[allow(unsafe_code)] mod sys;` in `src/windows/mod.rs`. Everything else is safe code over `sys`, std, and `OwnedHandle`. Each `unsafe` block holds one operation and a `SAFETY` comment (`multiple_unsafe_ops_per_block`, `undocumented_unsafe_blocks` denied). `sys` exposes safe functions only:

| Safe function | `unsafe` operations inside | Safety argument |
|---|---|---|
| `create_pseudo_console(&MutexGuard, PtySize, &PipeReader, &PipeWriter) -> io::Result<Pcon>` | `CreatePseudoConsole` | Both handles are borrowed, valid for the call; the out-pointer is a local `HPCON`; a `Pcon` is built only when the HRESULT succeeds. The guard parameter proves the spawn lock is held (W3). |
| `Pcon::close(self)`, `resize(&Pcon, PtySize)` | `ClosePseudoConsole`, `ResizePseudoConsole` | `close` consumes the only `Pcon`, so the HPCON is closed once; `resize` borrows it, and the control lock (PR2 §2) orders it before `close`. `Pcon` is `Send` (an opaque pointer owned by one value). |
| `create_job() -> io::Result<OwnedHandle>` | `CreateJobObjectW`, `OwnedHandle::from_raw_handle`, `SetInformationJobObject` | Null name and attributes; the returned handle is checked non-null before wrapping; the limit struct is zero-initialised (integers only) and its exact size passed. |
| `AttrList<'a>::new(n)`, `set_pseudoconsole(&'a Pcon)`, `set_job_list(&'a [HANDLE; 1])`; `Drop` | `InitializeProcThreadAttributeList` (size query, then init), `UpdateProcThreadAttribute`, `DeleteProcThreadAttributeList` | The buffer is a `Vec<usize>` (pointer-aligned) of the queried size; the lifetime `'a` ties the list to the values it points to, so they outlive every use of the list in `CreateProcessW`; delete runs once, in `Drop`. |
| `create_process(&MutexGuard, &Buffers, &mut AttrList, suspended) -> io::Result<Created>` | `CreateProcessW`, two `OwnedHandle::from_raw_handle` | `Buffers` guarantees NUL-terminated application name and cwd, a mutable NUL-terminated command line (as `CreateProcessW` requires), and a double-NUL environment block; `STARTUPINFOEXW` zeroed with `cb` set; `bInheritHandles = FALSE`; the two handles of `PROCESS_INFORMATION` are wrapped exactly once and only on success. |
| `assign(&OwnedHandle, &OwnedHandle)`, `resume(&OwnedHandle) -> io::Result<u32>` | `AssignProcessToJobObject`, `ResumeThread` | Borrowed valid handles; `u32::MAX` from `ResumeThread` is mapped to the last error. |
| `terminate_job(&OwnedHandle, u32)`, `terminate_process(&OwnedHandle, u32)` | `TerminateJobObject`, `TerminateProcess` | Borrowed valid handles. |
| `wait(&OwnedHandle, Duration) -> io::Result<bool>`, `exit_code(&OwnedHandle)`, `creation_time(&OwnedHandle) -> io::Result<u64>` | `WaitForSingleObject`, `GetExitCodeProcess`, `GetProcessTimes` | Borrowed valid handles; the wait is capped below `INFINITE`; `FILETIME` out-parameters are locals. |
| `open_process(pid, access) -> io::Result<OwnedHandle>` | `OpenProcess`, `OwnedHandle::from_raw_handle` | `bInheritHandle = FALSE`; null checked before wrapping. |
| `active_processes(&OwnedHandle) -> io::Result<u32>` | `QueryInformationJobObject` (`JobObjectBasicAccountingInformation`) | Zeroed struct of integers; its size passed. |
| `job_pids(&OwnedHandle, cap) -> io::Result<Vec<u32>>` | `QueryInformationJobObject` (`JobObjectBasicProcessIdList`), reads of the header and of `NumberOfProcessIdsInList` entries | A `Vec<usize>` holding the header plus `n` entries, byte length passed; `ERROR_MORE_DATA` doubles `n` up to `cap`; only `min(NumberOfProcessIdsInList, n)` entries are read. |
| `in_job(&OwnedHandle, &OwnedHandle) -> io::Result<bool>` | `IsProcessInJob` | Borrowed valid handles; the result is a local. |
| `module_path(&str) -> Option<PathBuf>` | `GetModuleHandleW`, `GetModuleFileNameW` | No reference count is taken (`GetModuleHandleW`); the buffer length is passed and a result equal to it is treated as truncated. |
| `compare_ordinal_ignore_case(&[u16], &[u16]) -> Ordering` | `CompareStringOrdinal` | Lengths passed with the pointers; a failure (only for invalid parameters) is mapped to an ordinal comparison of the raw units. |
| `process_handle_count() -> io::Result<u32>` | `GetProcessHandleCount(GetCurrentProcess())` | The pseudo-handle needs no close. |
| `#[cfg(test)] make_inheritable(&OwnedHandle)`, `#[cfg(test)] enter_new_kill_on_close_job() -> io::Result<OwnedHandle>` | `SetHandleInformation`; `AssignProcessToJobObject(job, GetCurrentProcess())` | Test-only helpers (CL-24, CL-27), compiled only into the unit-test binary. |

`tests/lints.rs` changes from one allowance to exactly two, on `unix::sys` (in `src/unix/mod.rs`) and `windows::sys` (in `src/windows/mod.rs`), with no `unsafe` anywhere else; the scan reads every source file, so it runs on both OSes.

### 2.8 `testkit` and `chaldea-fake-cli` on Windows

testkit keeps `forbid(unsafe_code)`. Windows console access goes through `winapi-util` (W10), already in the lock.

**Terminal** (`term.rs`, Windows arm):

- `stdin_is_tty()`: `std::io::stdin().is_terminal()`.
- `make_raw()`: input mode `(mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT)) | ENABLE_VIRTUAL_TERMINAL_INPUT` on stdin and `| ENABLE_VIRTUAL_TERMINAL_PROCESSING` on stdout (spike `os/windows.rs:475-501`), with the documented values as local constants (`0x2`, `0x4`, `0x1`, `0x200`; `0x4` for output). So `echo` and `queries` read bytes under ConPTY as on Unix.
- `size()`: `screen_buffer_info(stdout).window_rect()` → `(right − left + 1, bottom − top + 1)`, pixels `None` (ConPTY has no pixel size).
- `inq()`: `None` (no safe input-event count).

**Hello.** The `Hello` type is unchanged; on Windows its Unix fields are null, as now. Windows facts come in an opt-in event right after the hello, `--report-process`: `{"event":"process","argv":[…],"stdin_console":b,"stdout_console":b}` (`argv` from `std::env::args_os`, lossily as UTF-8), so no existing test sees a new event.

**Additions.**

| Item | Behaviour |
|---|---|
| `breakaway` role (Windows) | `spawn-tree` starts it with `CommandExt::creation_flags(CREATE_BREAKAWAY_FROM_JOB)` (`0x0100_0000`). On `ERROR_ACCESS_DENIED` it starts it again without the flag; either way it sends `{"event":"breakaway","result":"accepted"\|"denied"}` on its own stream before the role's hello (spike `os/windows.rs:456-466`). Requested with `--roles`; the Windows default roles stay `plain`, `double-forked`. |
| `double-forked` (Windows form) | `double-fork-parent` starts it with a piped stdin and exits at once; the child reads stdin to EOF (bounded by 10 s), which happens when its parent has exited, then sends its hello. Windows has no reparenting to wait for. |
| `plain`, `unrelated` | Unchanged. The test starts `unrelated` itself with `std::process::Command`, outside the session's job. |
| `attachable` on Windows | `size` at start from `size()`; then polls `size()` every 20 ms and sends `winch` when it changed (Windows has no `SIGWINCH`; the event reports an observed change, not a signal). |
| `--emit-output` (`spawn-tree`) | After its hello and roles, writes `OUTPUT-<n>\r\n` to stdout every 50 ms and flushes, and after the first line sends `{"event":"output","n":0}`. It keeps ConPTY's output busy through a teardown (F-2c). All OSes. |
| `--report-process` | The `process` event above. All OSes. |

**Test side.** `Cleanup::kill_with(self, f: fn(u32, &str) -> bool) -> Cleanup` (additive; the private fields gain the function): on drop, each tracked identity is passed to `f` (Windows tests pass `fake::kill_verified`, which checks the creation time and terminates through the same handle). The real-ConPTY tests are testkit integration tests (`tests/pty_windows.rs`) for the same reason as on macOS: only they get `CARGO_BIN_EXE_chaldea-fake-cli` (PR3 §2.8). Every spawned FakeCli receives `SystemRoot` in its environment (Winsock needs it for the report stream); the tests take it from their own environment and never change that environment.

## 3. Structure Options

| Fork | Option | Precedent | Verdict |
|---|---|---|---|
| **ConPTY function resolution** | A. `conpty.dll` from the search path, else kernel32 | portable-pty `psuedocon.rs:44-59`; alacritty `conpty.rs:31-90` | Rejected (decided, prior-art row 1d): a planted or sideloaded DLL changes the console host. |
| | B. `GetProcAddress(GetModuleHandleW("kernel32.dll"), …)` | Codex loads `kernel32.dll` by name (`psuedocon.rs:92-98`) | Rejected: a transmute per function and three more `unsafe` blocks for the same binding. |
| | C. The static `raw-dylib` import of `kernel32.dll` that `windows-sys` declares | W1 | **Adopted.** The import table names `kernel32.dll`, a KnownDLL; `conpty_module` records the module path and CL-4 checks that no `conpty.dll` was loaded. |
| **Flags** | `0` | alacritty, Windows Terminal (prior-art row 4d) | **Adopted** (decided after the spike, arch). `0x2` (Codex) is the reserve if a resize problem appears; `0x1` stalls startup (W5). |
| **Job assignment** | A. Job list, fail otherwise | Codex `procthreadattr.rs:85-88` | Rejected: the mvp row requires the suspended fallback. |
| | B. Job list, else `CREATE_SUSPENDED` + assign + `ResumeThread == 1` | spike `os/windows.rs:1813-1874` | **Adopted** (decided). |
| | C. Assign after start | portable-pty; Zed (prior-art row 1b) | Rejected: race escapees observed (spike C-3). |
| **Containment refused** | A. Resume uncontained | Codex `job.rs:169-196` | Rejected: a process tree stop could not reach. |
| | B. Terminate the suspended child, `NeverStarted` (or `StartedThenFailed` when its exit is unconfirmed) | — | **Adopted.** PR 8 decides what the daemon does when its own job refuses nesting. |
| **Breakaway** | A. `BREAKAWAY_OK` | Codex `job.rs:79-82` | Rejected: any member could leave the job and survive stop. |
| | B. No breakaway limit | spike `os/windows.rs:242-260` | **Adopted.** A CLI's breakaway request fails visibly instead of escaping; reversible (one flag) if a CLI needs it. |
| **Proof of death** | A. Toolhelp census and ppid walk | spike `list_processes`; herdr (prior-art row 1a) | Rejected: a walk is a snapshot, so completeness needs a census rule like PR 3's, which the job makes unnecessary. |
| | B. Job completion port, `JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO` | — | Rejected as the authority: delivery is not guaranteed (Microsoft, "JOBOBJECT_ASSOCIATE_COMPLETION_PORT structure"). |
| | C. Per-member handles from the pid list, each waited for | Codex `job.rs:281-335` | Rejected as a requirement: list sizing and opens on every call to prove handle release, which no consumer needs; the list feeds only the survivor report. |
| | D. `ActiveProcesses == 0` plus the leader's and held extras' handles signalled | spike `wait_drained` (`os/windows.rs:378-392`) | **Adopted.** Monotonic once zero (§2.5). |
| **Graceful phase** | A. `CTRL_BREAK` / `CTRL_CLOSE` first | — | Rejected: needs the daemon attached to the pseudoconsole, or closes it first and hides survivors (spike §4.6). |
| | B. Terminate at once | spike, Codex | **Adopted.** CLIs recover by `--resume` (crash-only). |
| **Pipes** | A. Own `CreatePipe` | spike | Rejected: `unsafe` for what std provides. |
| | B. Overlapped pipes with reader and writer threads | alacritty `conpty.rs:118-119` (miow) | Rejected: the drain and writer threads already exist; synchronous ends keep whole writes. |
| | C. `std::io::pipe` | W2 | **Adopted.** Synchronous, non-inheritable, `ERROR_BROKEN_PIPE` as EOF, no `unsafe`. |
| **conhost's pipe ends in the host** | A. Keep until the child exists | Codex `psuedocon.rs:116-121`, `:221` | Rejected: no benefit here, and a failure between the stages would have one more handle to release. |
| | B. Close right after `CreatePseudoConsole` | spike `os/windows.rs:1728-1731` | **Adopted.** conhost holds its own (W3). |
| **`ReleasePseudoConsole`** | A. Call it where exported | Codex `psuedocon.rs:222-230` | Rejected for PR 4: not in `windows-sys 0.61.2` (W1), so a `GetProcAddress` path; output would end at child exit on some builds and at close on others. Reopen if the daemon needs EOF at child exit. |
| | B. EOF at close | W4 | **Adopted.** The session already ends output at close. |
| **Close when the helper cannot start** | A. Closing `Drop` on the HPCON wrapper | spike `Pcon` (`os/windows.rs:1400-1407`) | Rejected: std drops the closure on the caller's thread and the close would block without a bound. |
| | B. Explicit `close(self)`, non-closing `Drop`, leak on that path | — | **Adopted**, reported as `elapsed: None`. |
| **Exit observation** | A. `RegisterWaitForSingleObject` callback | alacritty `child.rs:73` | Rejected: a thread-pool callback where PR 2's waiter already polls. |
| | B. `WaitForSingleObject(timeout)` on the held handle | PR2 §2.1 | **Adopted.** |
| **Handle wrapper** | A. Own `Handle(usize)` | spike | Rejected: `OwnedHandle` closes once and is `Send + Sync`. |
| | B. `std::os::windows::io::OwnedHandle`; an own `Pcon` for the HPCON, which is not a kernel handle | — | **Adopted.** |
| **Quoting** | A. Also quote LF and VT | Codex `psuedocon.rs:330-373` | Rejected: the CRT splits on space and tab only; extra quoting is harmless but diverges from std, the reference the tests compare against. |
| | B. std's rule | W6 | **Adopted.** |
| **Extras' descendants outside the job** | A. Toolhelp walk from each extra | PR 3's ppid walk | Rejected: completeness would rest on the walk. |
| | B. Only named identities | PR2 §2.1 (the obligation names the identities) | **Adopted.** The daemon names each outside-job process it learns of. |
| **FakeCli console access** | A. `terminal_size =0.4.3` | — | Rejected: adds `windows-sys 0.60` and its target crates to the lock, and has no console mode. |
| | B. A testkit `unsafe` carve-out | — | Rejected: mvp §3 keeps `forbid` outside pty-runtime. |
| | C. `winapi-util =0.1.11` | W10 | **Adopted.** Already locked; size and mode. |
| **Fault injection** | A. `cfg(test)` hooks only | PR 3 | Rejected alone: FakeCli is reachable only from testkit's integration tests. |
| | B. An environment variable read by the backend | spike `--inject` | Rejected: it would ship in release builds. |
| | C. `cfg(test)` hooks plus a `fake`-feature thread-local | PR2 D6 | **Adopted.** |
| **Handle-leak oracle** | A. Equal to the count before the first launch | spike F-2c first form | Rejected: one-time lazy initialisation (the spike's unexplained +1). |
| | B. Unchanged across repeated cycles after a warm-up | spike phase 3c (settled at 89) | **Adopted.** A leak grows the count every cycle. |
| **`unsafe` placement** | One `windows::sys` exposing safe functions | PR3 §2.7 | **Adopted.** |

## 4. Same-Class Inventory

Every MVP place on Windows that kills processes, holds handles or pids, or creates children:

| Place | PR | How this design serves it or stays out |
|---|---|---|
| `Session::kill`, `Session::close` step 2, `SpawnedPty` teardown | 2 | `WindowsChild::kill_tree`. |
| `chaldea stop`, `close --stop`, `resume` death check | 10 | `KillReport::method` `windows_job` → `launch_death_confirmed.method`; `tree_dead: false` keeps `death_confirmed: false`. |
| Recorded descendants (`extra`) from hook emitters | 9, 10 | The identity format of §2.1. Outside-job descendants of an extra are not found by the backend; the daemon names every process it records. The public lookup arrives with its first consumer (PR3 D7). |
| Daemon restart: old-tree check | 10 | Stays out. The job handle is unnamed and not inheritable, so the daemon's exit closes it and `KILL_ON_JOB_CLOSE` ends every member; what can remain is outside the job, which PR 10 checks by verified identity (open, creation time). |
| Daemon process rules | 8, 10 | The spawn rule (every child under `with_spawn_lock`; this backend's `CreatePseudoConsole` and `CreateProcessW` under it, §2.1) and the handle rule (no `CloseHandle` on a handle the code did not open). |
| Parent kill-on-close job check | 8 | Stays out. Nesting needs no breakaway; a parent job that refuses nesting makes `spawn` fail `NeverStarted` with the error, never an uncontained child (CL-27). |
| Daemon spawns of git and one-shot hooks | 6, 16, 20 | Not PTY children; created inside `with_spawn_lock` with `CREATE_NO_WINDOW` (prior-art row 9). They cannot inherit a handle of this backend (none is inheritable). |
| Worktree removal in-use check | 16 | Reads launch state. **Note for PR 16:** `tree_dead` on Windows means no job member runs and the leader and held extras have exited; another member's file handles can be released a little later (W7), so a removal right after stop can see a sharing violation, which PR 16's `uncertain`/retry path must absorb. |
| Rescue | 20 | Acts through `surface_action stop`, the stop path. |
| Doctor: FakeCli tree kill, ConPTY module check | 21 | `native()` + `Session` + FakeCli; `Session::evidence()` gives `job_branch` and `conpty_module`. |
| FakeCli attach paste tests on `windows-latest` | 14 | FakeCli raw console mode (`--raw`, `echo`) under ConPTY exists from this PR (§2.8). |
| FakeCli's own children and test cleanup | 3, 4 | `--max-life-secs`; `Cleanup::kill_with(fake::kill_verified)`; the session's job kills its members on drop. |

## 5. Resource Paths

| Resource | Created at | Owner | Bound | Released on |
|---|---|---|---|---|
| Input pipe: conhost's read end (backend copy) | stage 1 | spawn | 1 | right after stage 2, or stage 1–2 failure |
| Input pipe: write end | stage 1 | spawn, then session `input` | 1 | input dropped (writes nothing) |
| Output pipe: read end | stage 1 | drain thread from stage 3 | 1 | drain end (EOF or read error), or `Drain::start` failure (dropped first) |
| Output pipe: conhost's write end (backend copy) | stage 1 | spawn | 1 | right after stage 2, or stage 1–2 failure |
| Inheritable duplicates inside `CreatePseudoConsole` | stage 2 | kernel32 | 3, for the call | before the call returns; created under `SPAWN_LOCK` |
| Pseudoconsole (`Pcon`) and its conhost process | stage 2 | `WindowsControl` | 1 | `close` / drop through `close_bounded`; leaked only when the helper thread cannot start |
| `pty-drain` thread | stage 3 | `Drain` | 1 | EOF after `ClosePseudoConsole`, or a read error; else abandoned by `Drain::finish` |
| `conpty-close` thread | each close | `close_bounded` | 1 per close | `ClosePseudoConsole` returns; abandoned past its bound |
| Job handle | stage 4 | spawn, then `WindowsChild` | 1 | `release_early`, or child drop (kills remaining members) |
| Attribute list | stage 5 | stage-local `AttrList` | 1 per attempt | end of the attempt (`DeleteProcThreadAttributeList`) |
| Command line, environment, cwd buffers | validation | spawn | O(spec) | end of `spawn` |
| Process handle | stage 5 | spawn, then `WindowsChild` | 1 | child drop; on a 5.4 failure after the bounded wait |
| Thread handle | stage 5 | spawn | 1 | end of stage 5 |
| Suspended child of a failed fallback | stage 5.2 | spawn | 1 | `TerminateProcess` + 2 s wait (5.4) |
| Held extras | `kill_tree` | `KillState` | distinct accepted extras | released at the read that shows them signalled; child drop |
| Unverified extras | `kill_tree` | `KillState` | distinct malformed or unopenable extras | a later call that opens them; child drop |
| Member handles for the survivor report | step 6 | the call | ≤ 4 096 | at once, each after its read |
| `SPAWN_LOCK` | process | `pty-runtime` | 1 | held for stage 2, and for stage 5 |
| `fake` registry entry, fault, fault report | spawn (`fake` only) | thread-local / registry | 1 per thread, 1 per live child | next spawn on the thread; child drop |

**Endings.**

| Ending | What happens |
|---|---|
| Validation failure | `NeverStarted`, nothing created. |
| Stage 1–2 failure | Pipes (and nothing else) dropped; `NeverStarted { Setup }`, `Confirmed`. |
| Stage 3 failure | Output read end dropped first, then the bounded close, then the input; `NeverStarted { Setup }`. |
| Stage 4, 5.1–5.3 failure | No process exists. `release_early`; `NeverStarted` with its kind and `Cleanup`. |
| 5.4 failure (assign, resume) | The suspended child (no program instruction run) terminated and its exit confirmed by its handle: `NeverStarted`; unconfirmed: `StartedThenFailed` with `Unresolved`. Then `release_early`. |
| Stage 6 failure, or later | `StartedThenFailed`; `SpawnedParts::teardown`: kill (job), input, close (bounded), drain. |
| Child exit | `wait_timeout` reports it on every call; descendants stay in the job; output continues; no EOF before close. |
| `kill` | §2.5 within the deadline; I/O stays up. |
| `close` | PR2 §2.2: kill; input released (a blocked write ends when step 4 makes conhost go away); `ClosePseudoConsole` bounded while the drain reads; drain to EOF; join. |
| `Session` drop | As close; the child's drop closes the job (kill-on-close for anything left) and the process handle. |
| conhost dies on its own | Output EOF; writes fail (`InputBroken`); the CLI, a console client, is terminated by the console's loss. |
| Reader `ERROR_BROKEN_PIPE` | `OutputEnd::Eof`. Any other read error: `OutputEnd::Error`. |
| Write error | Unit `Incomplete`, `InputBroken` (PR2 §2.3). |
| `ClosePseudoConsole` hang | `close_bounded` returns `None`; `conpty-close` abandoned; the drain finishes at its bound or is abandoned. |
| Panic | No panic path in this backend's code; a poisoned `kill` mutex is taken over. |

**Waits.** Every wait has a deadline except: acquiring `SPAWN_LOCK`; `CreatePseudoConsole` and `CreateProcessW` (synchronous, no handshake); the drain's `ReadFile` and the writer's `WriteFile` (both end when conhost closes its ends, which close step 4 causes); `ResizePseudoConsole` (unbounded by PR2 §2.1). Bounded: `wait_timeout`; `kill_tree` (lock polling, 10 ms rounds, handle waits capped to the remaining budget; one in-flight call can overrun); `close` (`close_bounded`); the suspended child's exit wait (2 s); `release_early` and the teardown (2 s close, 2 s drain, 5 s kill); `AfterSpawn`'s milestone wait (10 s, tests only).

## 6. Purpose Layer and Plan Layer

### 6.1 Purpose Layer: Acceptance Criteria

Checked on `windows-latest` by `cargo test -p chaldea-testkit --test pty_windows`, `cargo test -p chaldea-pty-runtime` (unit tests and `tests/windows_spawn.rs`); on a real Windows host the same, plus Task Manager or `Get-Process chaldea-fake-cli` before and after a kill.

| ID | Criterion (EARS) | Source | How to observe on a real Windows host |
|---|---|---|---|
| PR4-P-1 | When a CLI tree is killed, no process of its job and no verified `extra` identity shall be alive at the deadline, `tree_dead` shall be true, and an unrelated process shall still answer; a process never in the job, or an outside-job descendant of an `extra`, is outside this guarantee unless named in `extra`. | mvp §4 PR 4 obligation; PR2 §2.1 | FakeCli `spawn-tree` (`plain`, `double-forked`, `breakaway`): every stream closes; `unrelated` answers `ping`; an outside-job sleeper dies only when named in `extra`. |
| PR4-P-2 | The backend shall terminate a process only through a handle on which it verified the recorded creation time, or through the job, and never by pid alone. | spike §4.6; §2.5 | A stale `extra` (the unrelated pid with another start) leaves it alive while a matching one dies. |
| PR4-P-3 | When a spawn fails at any stage, the error shall say whether the program could have run, no process or handle of the spawn shall remain when it returns, and a release that could not be proven shall be reported `Unresolved`. | mvp §4 PR 4 obligation (spike F-2c); PR2 §2.1 | Each injected fault: process handle count unchanged across cycles; every fixture stream closed; the error variant. |
| PR4-P-4 | When a spawn succeeds, the CLI shall run `argv[0]` with the requested arguments, cwd, and exact environment, on the pseudoconsole's standard handles at the requested size, with no inherited handle, inside the session's kill-on-close job from before its first instruction, with conhost from the inbox `kernel32.dll`, and the branch taken shall be recorded. | mvp §4 PR 4; PR2 §2.1 | `process` event (`argv`, consoles), `cmd /d /c set`, `attachable` size, `Session::evidence()`. |
| PR4-P-5 | When the session closes, output shall be read until EOF, `ClosePseudoConsole` shall be bounded while output still flows, and `close` shall return within its budgets; written bytes shall reach the CLI whole; a resize shall reach it. | PR2 §2.1–§2.4 | `CloseOutcome` with `--emit-output` running; `echo` reports; `winch`. |
| PR4-P-6 | Every `unsafe` block shall be in `pty_runtime::unix::sys` or `pty_runtime::windows::sys`, and every other crate shall keep `forbid(unsafe_code)`. | PR2 D7; mvp §3 | `tests/lints.rs`; clippy on both runners. |
| PR4-P-7 | The workspace shall build and pass on both runners, with the real-ConPTY tests running on `windows-latest`. | mvp §4 | `workspace (windows-latest)` including the test-list step. |

**Assumptions** that only a Windows host shows, each checked by the named claim: the runner accepts the job list, nested under any job the runner uses (CL-4); a breakaway request from a member is refused (CL-16); `ActiveProcesses` reaches 0 within the kill budget after `TerminateJobObject` (CL-16); `STARTF_USESTDHANDLES` with invalid handles gives the child the pseudoconsole's standard handles while the parent's are pipes (CL-12); the console window equals the pseudoconsole size (CL-11); FakeCli raw mode works under ConPTY (CL-14); conhost accepts a 64 KiB write from a non-reading client without blocking (CL-14); `ClosePseudoConsole` returns within 2 s while output flows (CL-20); the handle count settles after a warm-up (CL-3, CL-7); a command line of exactly 32 767 units is accepted (CL-1); output does not reach EOF before close (CL-13).

### 6.2 Plan Layer: Claims

Unit tests are in `src/windows/tests.rs` (system binaries: `%SystemRoot%\System32\cmd.exe` with `/d /c`), `src/windows/{cmdline,kill}.rs`, and `src/windows/control.rs`; public-API tests in `tests/windows_spawn.rs`; FakeCli tests in testkit `tests/pty_windows.rs` (`pty_windows::…`).

| ID | Claim (EARS) | Source | Falsifying test |
|---|---|---|---|
| PR4-CL-1 | If validation fails, then `spawn` shall return `NeverStarted` with the kind of §2.2's table and create nothing. | PR2 §2.1; §2.2 | `windows_spawn`: `missing_program_is_not_found`, `relative_argv0_is_setup`, `quote_in_argv0_is_setup`, `nul_in_argv_is_setup`, `batch_file_is_refused` (`.cmd` and `.BAT`), `directory_argv0_is_permission_denied`, `missing_cwd_is_setup`, `bad_env_is_setup` (`=` in a key, NUL, `Path` and `PATH` together), `command_line_over_the_limit_is_setup` (32 768 units); unit `invalid_spec_creates_nothing` (no stage hook reached), `command_line_at_the_limit_spawns` (exactly 32 767 units with `cmd.exe /d /c exit 0`: `Ok`, `Exited(0)`; the measurement mvp D5 lacks) |
| PR4-CL-2 | If `CreateProcessW` fails on both attempts, then `spawn` shall return `NeverStarted` with the error's kind, no process shall exist, both attempts shall be in the error when their codes differ, and the release shall be `Confirmed` when the close returned and the drain ended. | §2.2 stage 5.3 | unit `program_removed_after_validation_is_not_found` (a copy of `cmd.exe` in a temp dir, removed by the `Validated` hook), `cwd_removed_after_validation_is_setup`, `non_pe_exe_is_setup` (a text file named `.exe`: `ERROR_BAD_EXE_FORMAT`); each: `fault_report` shows two `CreateProcessW` calls and no process handle |
| PR4-CL-3 | If a stage before a process exists fails (pipes, pseudoconsole, drain start, job, attribute list, both creations), then `spawn` shall return `NeverStarted { Setup }`, release in the order close (bounded) → input → drain finish → job, never call `CreateProcessW` for an earlier stage, and leave the process handle count unchanged. | spike F-2c; §2.2 | unit `pre_process_faults_release_in_order` (one case per fault; the release trace in order; for `DrainStart` the output end dropped before the close); testkit `pty_windows::pre_process_faults_leave_no_handle` (FakeCli without `--report`, so the test holds no sockets; per fault one warm-up cycle, then three: `process_handle_count` equal after the warm-up and after the third; a handle kept per failure fails it) |
| PR4-CL-4 | When a spawn succeeds on `windows-latest`, `evidence` shall hold `job_branch = job_list` and a `conpty_module` that is `kernel32.dll` in the system directory with no `conpty.dll` loaded, and the leader and every role shall be in the session's job, including a child started before the backend's next step. | mvp §4 PR 4 obligation; W1 | `pty_windows::spawn_takes_the_job_list_branch` (`evidence()` values; `conpty_module` equals `%SystemRoot%\System32\kernel32.dll` case-insensitively), `child_is_in_the_job_from_creation` (`AfterCreate` gate opened only after spawn-tree's `plain` role sent its hello: `windows_in_job` true for the root and the role; an assign-after backend would have let the role start outside the job) |
| PR4-CL-5 | When the job list is rejected, the backend shall start the child suspended, assign it, resume it with `ResumeThread == 1`, record `job_branch = suspended` with `job_list_error`, and every role shall be in the job and die on kill. | mvp §4 PR 4; spike F-2c | `pty_windows::forced_fallback_takes_the_suspended_branch` (`RejectJobList`; roles `plain`, `double-forked`, `breakaway`: evidence, `windows_in_job` true for each, each stream closes after `kill`, `unrelated` answers) |
| PR4-CL-6 | If assignment or resume fails on the suspended branch, then the backend shall terminate the suspended child, confirm its exit through its handle, and return `NeverStarted` with the program never run; if the exit is not confirmed, `StartedThenFailed` with `Unresolved`. | §2.2 stage 5.4 | unit `assign_failure_terminates_the_suspended_child`, `resume_failure_terminates_the_suspended_child` (`cmd.exe /d /c type nul > <marker>`: marker absent, `NeverStarted { Setup }`, `Confirmed`, fault report `exited`), `unconfirmed_suspended_exit_is_started_then_failed` (`SuspendedExitUnconfirmed`) |
| PR4-CL-7 | If setup fails after the process exists (identity, after spawn with output flowing, session thread start), then `spawn` shall return `StartedThenFailed`, kill the tree before releasing input, pseudoconsole, and drain, close the pseudoconsole within its bound while output flows, leave no fixture alive, spare the unrelated process, and leave the handle count unchanged. | spike F-2c; PR2 §2.1 | `pty_windows::after_spawn_fault_with_output_flowing_kills_the_tree` (`spawn-tree --emit-output --roles plain,double-forked,breakaway`; `AfterSpawn` gate opened on the `output` event: every role's stream closes, `unrelated` answers, `Cleanup::Confirmed`, fault report drain bytes > 0 with the child running), `identity_fault_kills_the_tree`, `writer_start_fault_kills_the_real_tree` (`StartFault::Writer`), `after_process_faults_leave_no_handle` (without `--report`; warm-up then three cycles); unit `expired_teardown_kill_reports_unresolved` (`KillExpired`: `Unresolved` with a kill report), `teardown_order` (trace: `terminate_job`, input drop, `close`, `drain_finish`) |
| PR4-CL-8 | The child's environment shall be exactly `spec.env`, in a block sorted with `CompareStringOrdinal`, double-NUL when empty. | PR2 §2.1; W6 | unit `env_block_is_sorted_case_insensitively` (`b`, `A`, `C` → `A`, `b`, `C`), `empty_env_block_is_two_nuls`; `windows_spawn::env_is_exactly_the_spec` (`cmd.exe /d /c set` with `A=1` and `SystemRoot`: the snapshot shows both; `CARGO_PKG_NAME` and the test's `PATH` value absent) |
| PR4-CL-9 | When the child starts, its argv shall equal `spec.argv` after the CRT's parsing, for spaces, tabs, quotes, backslash runs before quotes and at the end, empty arguments, and non-ASCII text. | §2.2 buffers | `pty_windows::argv_reaches_the_cli_intact` (`--report-process` with `a b`, `""`, `x\"y`, `C:\dir\`, `\\"`, `日本語`, tab: `argv` equals the spec); unit `command_line_matches_std_quoting` (fixed corpus against expected strings) |
| PR4-CL-10 | The child shall run in `spec.cwd`, with a verbatim prefix removed. | PR2 §2.1; W6 | `windows_spawn::cwd_is_honored` (`cmd.exe /d /c cd` with a `\\?\`-prefixed canonical temp dir: the snapshot shows the path without the prefix) |
| PR4-CL-11 | When `spawn` returns, the pseudoconsole shall have `spec.size`, and a resize shall reach the CLI. | PR2 §2.1 | `pty_windows::size_reaches_the_cli` (`attachable` at 100×30: `size` 100×30, pixels null; after `resize(120, 40)` a `winch` with 120×40) |
| PR4-CL-12 | The child's standard handles shall be the pseudoconsole's even when the daemon's are pipes. | §2.2 stage 5; W9 | `pty_windows::child_stdio_is_the_pseudoconsole` (`process` event: `stdin_console` and `stdout_console` true; on CI the test process's own stdout is a pipe, so removing `STARTF_USESTDHANDLES` gives the child that pipe) |
| PR4-CL-13 | The drain shall start before the process is created and receive output from the first byte; output shall not reach EOF before the pseudoconsole closes, and shall reach EOF after it. | PR2 §2.1, §2.4; W4 | unit `drain_starts_before_the_process` (trace: drain start before the first `CreateProcessW`), `output_ends_only_after_close` (`cmd.exe /d /c echo READY`: after `Exited` and the sink having received `READY`, the drain has no end; after `control.close` it ends `Eof` within 10 s) |
| PR4-CL-14 | A write shall be accepted whole, bytes shall reach the CLI, and dropping the input shall write nothing. | PR2 §2.1 | `pty_windows::input_reaches_the_cli` (`echo` in raw mode: unit `hello` → `input` event `68656c6c6f`), `large_write_is_accepted_whole` (64 KiB to a `sleeper`: `Written`, `bytes_written` 65 536, `split_writes` 0); unit `dropping_input_writes_nothing` (the input type over a `std::io::pipe`: drop, the reader reads `Ok(0)` with no byte) |
| PR4-CL-15 | `wait_timeout` shall return by its timeout, not wait for a running `kill_tree`, and report the exit code on every call, reinterpreted as `i32`. | PR2 §2.1; §2.4 | unit `wait_timeout_returns_by_its_timeout`, `wait_timeout_runs_during_kill` (a hook inside `kill_tree` holding its lock: `wait_timeout(10 ms)` from another thread returns within 200 ms), `exit_code_is_reported_repeatedly` (`exit 7` twice), `large_exit_code_is_negative` (`exit -1073741510` → `0xC000013A as i32`) |
| PR4-CL-16 | When `kill_tree` runs, every job member shall die, a breakaway request by a member shall have been refused, the report shall be `tree_dead: true` with no survivor, and an unrelated process shall survive. | mvp §4 PR 4 obligation | `pty_windows::spawn_tree_dies_and_unrelated_survives` (roles `plain`, `double-forked`, `breakaway`; the `breakaway` event says `denied`; each stream closes; `unrelated` answers `ping` afterwards) |
| PR4-CL-17 | `kill_tree` shall prove death only from a read of `ActiveProcesses == 0` in the same call with the leader and held extras signalled and nothing unverifiable; it shall terminate the job again while members remain, return an already dead tree without terminating, compute each call from its own reads, and report survivors with `tree_dead: false` at the deadline. | §2.5 | unit `windows::kill::tests` (synthetic `JobOs` and clock): `job_empty_and_handles_signalled_proves_death`, `nonzero_count_is_not_proof`, `unsignalled_extra_is_not_proof`, `unverifiable_extra_is_not_proof`, `query_error_is_not_proof`, `terminate_failure_does_not_decide`, `members_left_are_terminated_again`, `dead_tree_returns_without_terminate`, `three_calls` (proven; then a live extra times out as a survivor; then an expired call still lists it), `survivors_are_confirmed_members` (a listed pid whose handle is not in the job is not reported), `list_failure_keeps_leader_and_extras` |
| PR4-CL-18 | An `extra` shall be terminated only through a handle whose creation time matched; a stale, gone, or reserved pid shall never be terminated; an unopenable or malformed one shall be a survivor; a held one shall be kept across calls until seen signalled. | PR2 §2.1; §2.5 | `pty_windows::extra_outside_the_job_dies_and_stale_extra_is_spared` (a sleeper started by the test outside the job with its matching identity, and the `unrelated` pid with start + 1, in one call: the first dies, `unrelated` answers; without `extra` the first survives); unit `stale_extra_is_not_terminated`, `gone_extra_is_resolved`, `access_denied_extra_is_waited_not_killed`, `unopenable_extra_is_a_survivor`, `malformed_start_is_a_survivor`, `reserved_pids_are_never_opened` (0, 4, own), `held_extra_survives_across_calls` |
| PR4-CL-19 | The leader's identity shall be read from its own handle, also when it already exited, and match `fake::identify`; `identity()` shall have no panic path without a record. | §2.1, §2.2 stage 6 | unit `immediate_exit_still_has_an_identity` (`exit 0` with a hook that waits for the exit before stage 6: `Ok`, then `Exited(0)`), `leader_identity_matches_identify`, `identity_failure_has_an_empty_start` |
| PR4-CL-20 | `close` shall return by its deadline with output flowing, end the output with EOF, and never block its caller when the close helper cannot start. | PR2 §2.1, §2.4; W4 | `pty_windows::close_with_output_flowing_is_bounded` (`--emit-output`: within the budgets, `output == Some(Eof)`, `released.control`, `abandoned` empty); unit `unstarted_close_helper_does_not_block` (a `cfg(test)` stand-in for `close_bounded` that drops the closure without running it: `close` returns at once with `elapsed: None`, and the trace has no `ClosePseudoConsole` call) |
| PR4-CL-21 | The child shall inherit no handle of the daemon. | §2.1 | unit `child_inherits_no_handle` (under the module lock, the test makes a pipe write end inheritable and drops its own copy after spawning a `cmd.exe /d /c pause` child: the reader reads `Ok(0)` within 10 s while the child lives) |
| PR4-CL-22 | `#[allow(unsafe_code)]` shall occur exactly twice in `crates/pty-runtime/src`, on `unix::sys` and `windows::sys`, with no `unsafe` elsewhere and a `SAFETY` comment on every block. | PR2 D7 | `tests/lints.rs::unsafe_is_allowed_only_in_sys` (updated), with its existing scan tests; clippy `undocumented_unsafe_blocks` on `windows-latest` |
| PR4-CL-23 | While `CreatePseudoConsole` or the creation stage runs, no other child creation through `SPAWN_LOCK` shall start. | §2.1; W3 | unit `pty_spawn_waits_for_a_helper_spawn` (a `with_spawn_lock` closure parked on a gate; a PTY spawn on another thread does not reach the stage 2 `LockAcquired` hook until it opens), `helper_spawn_waits_for_a_pty_spawn` (the `AfterCreate` gate parks a PTY spawn inside stage 5; a concurrent `with_spawn_lock` closure does not start until it opens); `create_pseudo_console` and `create_process` take the guard, so a call without the lock does not compile |
| PR4-CL-24 | FakeCli shall report the breakaway result, the Windows `double-forked` hello after its parent's exit, the `process` event, console sizes, and `output` events. | §2.8 | testkit `fake_cli::breakaway_role_reports_its_result` (Windows, pipe mode: `accepted` or `denied`, then the role's hello), `double_forked_waits_for_its_parent` (the parent's stream closes before the child's hello arrives), `process_event_reports_argv`, `emit_output_reports_lines`, existing `attachable_on_a_pipe_reports_null_size` |
| PR4-CL-25 | `chaldea-testkit` shall depend on no workspace crate other than `chaldea-core`, on any target. | mvp §3 | CI step (unchanged; `winapi-util` is not a workspace crate) |
| PR4-CL-26 | The Windows CI job shall fail when the real-ConPTY tests are not compiled. | §1 | CI step `List the real-ConPTY tests` (`-- --list` must name `spawn_tree_dies_and_unrelated_survives`) |
| PR4-CL-27 | When the daemon itself runs in a kill-on-close job without breakaway, a spawn shall still take the job-list branch (nested), its tree shall die on kill, and the parent job's other processes shall be unaffected. | §2.5 "Daemon inside a job" | unit `spawn_inside_a_parent_job_nests` (helper process: the test binary re-executed with `--exact` and a flag, which enters a new kill-on-close job through `enter_new_kill_on_close_job`, spawns `cmd.exe /d /c pause`, asserts `job_list`, kills with `tree_dead: true`, and exits 0) |

**Obligation mapping** (PR2 §2.1 → claims): release on failure and `Unresolved` → CL-1, CL-2, CL-3, CL-6, CL-7; opened at `spec.size` → CL-11; environment → CL-8; drain from the first byte, started after `CreatePseudoConsole` and before `CreateProcessW` → CL-13; dropping input writes nothing, whole writes → CL-14; `wait_timeout` → CL-15; `kill_tree` with `extra`, deadline, survivors → CL-16, CL-17, CL-18; `close` through `close_bounded` → CL-20; `evidence` (`job_branch`, `conpty_module`) → CL-4, CL-5; `unsafe` placement → CL-22. PR 4 row obligations: survivors and the unrelated negative control → CL-16, CL-18; F-2c → CL-3, CL-6, CL-7; job-list branch recorded → CL-4, CL-5. PR 3's process rules in Windows form: spawn lock → CL-23, no inheritance → CL-21; ownership → CL-15, CL-19 (exit read only through the held handle). No claim rests on a process-list walk: CL-17's proof reads the job's count and held handles; the job's pid list feeds the survivor report only.

## 7. Tests and Completion Commands

**Where tests run.**

| Tests | File | OS | Processes |
|---|---|---|---|
| Kill engine | `src/windows/kill.rs` `#[cfg(test)]` | Windows | none: synthetic `JobOs` and clock |
| Command line, environment block, cwd | `src/windows/cmdline.rs` `#[cfg(test)]` | Windows | none |
| Stages, faults, exit, identity, inheritance, spawn lock, parent job | `src/windows/tests.rs` | Windows | `cmd.exe`; every test holds the module's lock; parent-job case in a helper process |
| Control close stand-in | `src/windows/control.rs` `#[cfg(test)]` | Windows | real pseudoconsole |
| Public-API spawn: validation, env, cwd | `tests/windows_spawn.rs` | Windows | `cmd.exe`; every test holds the file's lock |
| Lints | `tests/lints.rs` | all | none |
| FakeCli modes without a PTY | `crates/testkit/tests/fake_cli.rs` | all (Windows roles behind `cfg(windows)`) | FakeCli |
| Native backend + FakeCli, F-2c, handle counts | `crates/testkit/tests/pty_windows.rs` | Windows | FakeCli; every test holds the file's lock and a `Cleanup` with `kill_with(fake::kill_verified)` |
| PR 2 `FakePty` tests, PR 3 macOS tests | unchanged | as before | as before |

**Timing.** No test synchronizes by sleeping. A test waits for a FakeCli hello or event (sent only once its mechanism is in place: the breakaway result, the parent's exit, the first output line), `SessionEvent::Exited`, a stage hook, a gate, a pong, or a stream's end, each bounded by 10 s; kill deadlines are 5 s with 500 ms of slack on real processes. Survival is proven by `ping` after `kill_tree` returned. Every event a test asserts is read before the fixture is killed, since Windows resets a killed process's socket and discards unread lines (PR3 §2.8 "Report channel"). Fault gates open on a milestone, so a fault never fires before the state it tests. Handle counts are compared across repeated cycles after a warm-up, in tests whose fixtures hold no report sockets.

**Leaks.** Every FakeCli runs with `--max-life-secs 60`; `Cleanup` kills by verified identity on any exit; a dropped `Session` closes the job, which kills its members.

**Completion commands** (PR3 §7, extended):

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings          # on each runner; Windows code is linted on windows-latest
cargo test --workspace --locked
cargo test -p chaldea-testkit --test pty_windows --locked -- --list     # windows-latest only: must list the §6 tests
deps="$(cargo tree -p chaldea-core -e normal --prefix none)" && ! grep -Eq '^(tokio|rusqlite|axum) ' <<<"$deps"
deps="$(cargo tree -p chaldea-pty-runtime -e normal --prefix none)" && ! grep -Eq '^tokio ' <<<"$deps"
deps="$(cargo tree -p chaldea-testkit -e normal --target all --prefix none)" && bad="$(awk '/^chaldea-/ && !/^chaldea-(testkit|core) /' <<<"$deps")" && [ -z "$bad" ]
```

plus the `workspace` job green on both runners and the `pty-hooks` job unchanged. **Clippy for Windows** is authoritative only in CI (`workspace (windows-latest)`). A macOS developer can check it locally with `rustup target add x86_64-pc-windows-msvc` and `cargo clippy --workspace --all-targets --locked --target x86_64-pc-windows-msvc -- -D warnings` (type-checks without linking; not verified for this workspace while writing this design); lefthook does not run it, because it would fail on machines without the target.

## 8. Decisions

**D1. An own `windows-sys` backend with flags `0`, bound statically to `kernel32.dll`.** No search-path DLL can supply ConPTY (W1); the loaded module is recorded per launch.

**D2. The child is in a kill-on-close job before its first instruction, or it does not run.** Job list first, `CREATE_SUSPENDED` + assign + `ResumeThread == 1` second; a refused assignment terminates the suspended child. The branch, and the job-list error on the fallback, are evidence of every launch.

**D3. `CreatePseudoConsole` and process creation run under the process-wide spawn lock, and the backend creates nothing inheritable.** `CreatePseudoConsole` holds inheritable duplicates of conhost's pipe ends during the call (W3); everything else is non-inheritable from creation and the CLI is created with `bInheritHandles = FALSE`.

**D4. Death is proven from the job's own count and from held handles.** `ActiveProcesses == 0` in the call, the leader and held extras signalled, and nothing unverifiable; the job is terminated again while members remain; the pid list serves only the survivor report. The guarantee covers the job and verified `extra` identities; processes never in the job and outside-job descendants of an extra are outside it.

**D5. No breakaway.** The job sets only `KILL_ON_JOB_CLOSE`; a CLI's breakaway request fails. Reopen with evidence that a supported CLI needs breakaway, at the cost of a stated escape path.

**D6. Extras are opened by pid, verified by creation time on the same handle, and killed and waited for only through it; held handles keep their pids reserved.** No terminate by pid alone; no Toolhelp.

**D7. Output ends at close.** conhost's pipe ends are closed in the host right after `CreatePseudoConsole`, `ReleasePseudoConsole` is not used, `ClosePseudoConsole` runs through `close_bounded` while the drain reads, and an unstartable close helper leaks the pseudoconsole rather than blocking.

**D8. Synchronous std pipes; `PipeWriter` is the input and `PipeReader` the drain's source.** Whole writes, `ERROR_BROKEN_PIPE` as EOF, nothing written on drop, no `unsafe`.

**D9. `unsafe` lives in `windows::sys`, the second and last allowance of the crate.**

**D10. Faults are injected through `cfg(test)` hooks and a `fake`-feature thread-local; leaks are judged by handle counts stable across cycles and by FakeCli streams.** No fault path ships in a default build.

**D11. FakeCli uses `winapi-util` for console mode and size on Windows**, keeping testkit free of `unsafe` and the lock unchanged; new Windows facts arrive in opt-in events, so PR 3's types and tests are unchanged.

### Open Questions

**Q1. The Windows value of `ProcessIdentity::group`, persisted as `launch_started.pgid_or_job`.** Options: (a) `job:<leader pid>`: self-describing, unique per live launch, and distinguishes job members from outside processes in a survivor list; (b) empty: nothing to misread, but `launch_started` then carries no containment fact on Windows; (c) a named job: lets another process open the job, contradicting D3 and §4's restart reasoning. Gain/loss: (a) adds a format the daemon and the PWA must treat as opaque; (b) loses information. Reversibility: once events are stored, readers must accept the old values forever (append-only store, mvp §2.1). Recommendation: (a), documented as opaque text in mvp §2.1.

## Related Text in Other Documents

These documents state parts of this design or would need a change if it is adopted (not edited here):

- `docs/design/pty-runtime.md` §2.1: "On Windows the release order is: terminate job and process, close the pseudoconsole while the drain reads, close pipe ends", while the shared `SpawnedParts::teardown` and close order drop the input write end before closing the pseudoconsole; both are safe because the tree is killed first, and this design's pre-process path (`release_early`) follows the §2.1 order. The drain obligation "returns `Ok(0)` once every slave end is closed" holds on Windows only after `ClosePseudoConsole` (W4). The `evidence` list gains `job_list_error`. §2 threading model: the spawn lock's Windows role (D3).
- `docs/design/pty-unix.md` §2.1 and `crates/pty-runtime/src/spawn.rs`'s doc comment: "the lock matters on macOS" becomes "on macOS (exec-error pipe) and Windows (inheritable handles during `CreatePseudoConsole`)". §2.8: Windows roles (`breakaway`, the Windows `double-forked`), `--report-process`, `--emit-output`, `Cleanup::kill_with`, and the Windows terminal functions. §4's PR 4 row.
- `docs/mvp-design.md` §2.1: the format of `pgid_or_job` on Windows (Q1). §3: testkit's Windows dependency (`winapi-util`) and the FakeCli lift table. §4 PR 8 obligations: the Windows form of the process rules (CreatePseudoConsole under the lock is the backend's; the daemon never closes a handle it did not open; a parent job that refuses nesting makes spawns fail). §4 PR 16: sharing violations right after a confirmed Windows stop (§4). §4 PR 10: on Windows the old tree dies with the daemon's job handle.
- `docs/architecture.md`, "Windows PTY backend and process tree": the job is created without breakaway, and output EOF comes at `ClosePseudoConsole`.
