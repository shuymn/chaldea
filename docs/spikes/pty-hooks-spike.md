# Spike: PTY Launch, Hook Injection, Paste, VT Snapshot, Tree Kill

Status: design (not implemented). Implementation lives in `spikes/pty-hooks/`, a standalone Cargo project outside the future `crates/` workspace. Canonical constraints: `AGENTS.md`, `docs/architecture.md` §4 (PTY Seam), §5 (External Quirks), §6 (Verification). Prior-art survey: `docs/prior-art.md`.

The spike answers five questions with evidence. It does not build the daemon; nothing here is a public contract except where it pre-validates the irreversible core (architecture §2: hook emitter, Bearer + Host auth, Job Object / pgid cleanup).

Versions observed while writing this design (2026-11-21, macOS arm64): `claude` 2.1.322, `codex-cli` 0.170.0, `rustc`/`cargo` 1.96.0, `portable-pty` 0.8.1 (latest stable is 0.9.0; we stay on `=0.8.1`), `alacritty_terminal` 0.26.0 (published 2026-04-06, latest on crates.io).

## 0. Sequencing Constraint (owner decision, binding)

- **Order: macOS first, Windows second.** All real-CLI verification is planned for this macOS session. Full Windows support and ConPTY tuning happen later on the owner's native Windows machine.
- **Do not over-invest in speculative Windows code on macOS.** Windows code in this spike is the minimum needed to compile, pass FakeCli scenarios, and run the go/no-go probes in set C. No tuning, no alternative mechanisms beyond what a probe needs.
- **Do not defer architectural risk.** Set C exists so that anything forcing a major architecture or technology change on Windows (ConPTY unusable, no tree kill, no reattach path, unavoidable console windows) is found by one owner run of the same binary, before more macOS work builds on it.
- **Cross-platform hygiene from day one:** the control path is localhost HTTP only (no Unix domain sockets, no signals as IPC); the hook emitter is one Rust binary; every OS-specific PTY/process call sits behind one boundary module (`src/os/{unix,windows}.rs` under `cfg`), and the rest of the code is OS-agnostic.

Owner approvals in force: real-CLI paste API usage on both OSes; rewriting the real `~/.codex/hooks.json` only if session-flag hooks cannot persist trust (§4.3). Codex is measured both with `--no-daemon` (containment baseline) and in default mode.

## 1. Purpose Layer: Acceptance Criteria

Three sets. "Source" is the requirement origin; every AC is verified against the real thing (real PTY / ConPTY, real CLIs where named, real process tables). **O** marks an owner action or judgment. Each case outcome is `pass`, `platform_fail` (only with positive evidence of what the CLI or OS did: bytes compared and differing, a successful enumeration showing an escapee, positive death evidence), `harness_fail` (setup, relay, gate, writer, lookup, enumeration, or observer failure), `missing` (no result, e.g. a hook that never arrived), or `not_exercised`. Completeness is checked first: a scenario is `inconclusive` when any required case is `missing`, `harness_fail`, or `not_exercised`, even if another case failed; otherwise `pass` when all passed, else `fail`. A skipped real check, an expected session witness that never attested, ambiguous attribution, or an uncalibrated observer is never `pass`. Codex findings are reported as "observed on codex `<ver>`, source `<x>`, mode `<no-daemon|default>`".

### A. macOS real-device (planned for this session)

| ID | Acceptance criterion (EARS) | Source | Observable pass / failure |
|---|---|---|---|
| A-1 | When the harness launches Claude Code with `--settings <run>/claude-settings.json --session-id <uuid>` in a pre-trusted folder, the receiver shall get a `SessionStart` whose `session_id` equals `<uuid>`, sent with that launch's token. | Arch §5 | Fixture `fixtures/claude/<ver>/<launch-id>/SessionStart-*.json`; fail on timeout or id mismatch. |
| A-2 | When a prompt is submitted to Claude Code, the receiver shall get a correlated `UserPromptSubmit` with a `prompt` field. | Goal 1 | Fixture saved; fail if none within 30 s. |
| A-3 | When Codex is launched N≥3 times with one unchanged hook definition (same source, position, command) and each launch gets a fresh port, token, and thread id, then after one owner-approved trust every launch shall deliver a `SessionStart` bound to that launch (§4.3), with no review dialog. **O** (trust click) | Arch §5 | Correlated SessionStart per launch; fail = timeout + snapshot of the review dialog. |
| A-4 | When one byte of the command changes at the same source and position, that hook shall not deliver until reviewed; when the original is restored, delivery shall resume without review. | Goal 1 | Mutated launch: no event bound to that launch (`raw_unknown` and foreign events do not count as delivery) + snapshot whose review entry names the mutated command; restored launch: bound SessionStart. |
| A-5 | The report shall list registered hook events separately from events observed firing, the source and mode used, `hooks.state` keys added, and the `config.toml` hash before/after each launch. | Goal 1 | Registered ≠ observed is reported, not passed over. |
| A-6 | When the paste corpus is sent to real Claude Code and Codex as bracketed paste after input gating (§4.4), `UserPromptSubmit.prompt` shall equal the payload's logical content (line endings normalized to LF on both sides), all other bytes exact (no Unicode normalization), for each `--paste-newline` mode. | Arch §1 IME | Per-entry compare + first diff offset; per-mode table; DECSET 2004 never seen = `readiness_gate_failed`, reported apart from corruption. |
| A-7 | When real Claude/Codex are launched in a fresh untrusted folder (separate runs from A-6; no input is ever sent), the SessionStart timeout shall produce a VT snapshot showing the trust / update / hooks-review prompt legibly. **O** (judgment) | Arch §2 landmine | Snapshot files reviewed by the owner. |
| A-8 | When real Claude/Codex are stopped after FakeCli `sleeper` has completed MCP `initialize` and attested via `/witness`, no owned process and no witness shall survive 5 s later. | Arch §4 | `survivors=[]`; witness never attested = inconclusive. |
| A-9 | When Codex runs in default mode while a separate, unrelated Codex is already running (warm shared server), our hooks shall run inside our launched tree with our fresh env, and stopping our session shall leave the unrelated server alive. | Arch §4, §5 | Warm server pid identified before launch as the warm fixture's own process (ancestry verified from the fixture launch) and positively alive (pid + start time) right before the measured launch; emitter's nearest CLI ancestor is our CLI; unrelated server positively alive after stop. A valid hook received from the warm server's tree while `--no-daemon` passes = "adapter requires `--no-daemon`"; a hook that never arrived is reported separately as a delivery failure. |

### B. Cross-platform build + FakeCli (both OSes; CI-able, no login, no API)

| ID | Acceptance criterion (EARS) | Source | Observable pass / failure |
|---|---|---|---|
| B-1 | The project shall build with the pinned toolchain (`rust-toolchain.toml` 1.96.0) via `cargo build --release --locked` on macOS and, on Windows MSVC, under `C:\chs` with `CARGO_TARGET_DIR=C:\chs\t`. | Goal 5, Arch §5 MSVC | Build succeeds; no `LNK1104`. |
| B-2 | When `chs fake all` runs, every FakeCli scenario below shall pass on both OSes and write `report.json`. | Goal 5 | `status=pass`. |
| B-3 | When each corpus entry and size class (≤ 1 KiB, 4 KiB+1, 64 KiB) is pasted to FakeCli in each `--paste-newline` mode, FakeCli shall report wire bytes equal to that mode's expected mapping (LF and CRLF inputs both defined), also with `--chunk 7`. | Invariant 4 | `byte_equal` per case; fail records first diff offset. |
| B-4 | If a payload contains ESC or a C0 control other than TAB/CR/LF, then the paste writer shall reject it and write zero bytes to the PTY. | Paste-injection safety | Writer byte counter unchanged. |
| B-5 | When FakeCli sends DSR 6, DSR 5, DA1, DA2, OSC 10/11, and the kitty keyboard query, each reply shall arrive within 500 ms with the expected content; with the responder disabled (negative control) the check shall fail on macOS. | Arch §4 | `query_log` with content, latency, and who answered (harness write logged vs. reply without harness write = conhost). |
| B-6 | If no `SessionStart` arrives within the timeout, then the harness shall time out and snapshot: FakeCli `stall` (drawn screen, no hook) shall match an independently authored grid (CJK spacers, combining marks, cursor, mode flags); `stall --silent` shall time out with an empty grid. FakeCli `banner` (fake trust prompt plus a valid hook) shall proceed without timeout. | Invariant 1 | Grid diff; `decided_by: "hook_timeout"` on both stall cases, absent on `banner`. |
| B-7 | When stop is requested for FakeCli `spawn-tree`, every owned descendant (incl. SIGTERM+SIGHUP-ignoring, double-forked staying in the original PGID, and a distinct `setsid` variant; on Windows a child forced to start before job assignment) shall be dead within 5 s, and the harness-created `unrelated` fixture (negative control, registered via `/witness`) shall still be alive. | Arch §4 | Expected session witness never seen = inconclusive; surviving session witness or killed negative control = fail; escapees of the primary mechanism listed with `killed_by`. |
| B-8 | If a request has a bad or duplicate `Host`, missing, wrong, or duplicate token, wrong method/path, any `Transfer-Encoding`, a missing or duplicate `Content-Length`, headers > 8 KiB, body > 4 MiB, or exceeds the 5 s absolute request deadline, then the receiver shall reject it and create no fixture and no readiness event. | Invariant 5 | Status codes in report; fixture dir and readiness state unchanged. |
| B-9 | The emitter shall exit 0 quietly with no POST when the URL is missing, when the token is missing (tested independently, with stdin still open), when the URL is not exactly `http://127.0.0.1:<port>/hook` or `http://localhost:<port>/hook`, when stdin exceeds 4 MiB, or when stdin stays open past the 3 s budget. | Safety | Exit 0, empty stdout, receiver saw nothing. |
| B-10 | Every scenario shall stop within its deadline and write `report.json` on pass, fail, panic, Ctrl-C, timeout, post-spawn setup failure (injected), a child that stops reading mid-64 KiB paste, and a query emitted mid-paste. | Goal 5 | Report present with `status`; no surviving owned process; teardown fits inside the deadline. |
| B-11 | The receiver shall store a valid Codex-shaped `SessionStart` with `transcript_path: null`, keep unknown payloads verbatim, and keep every event not bound to the launch (§4.3) as `raw_unknown` (`foreign_session`). | Arch §4, prior-art 6c | FakeCli `echo --nested` starts a nested FakeCli inheriting the same env, whose `SessionStart` arrives both before and after the real one; the real one binds either way. |

### C. Windows real-device go/no-go probes (owner runs `chs win-probe all`)

Each probe answers "is there an architectural blocker?", not "is Windows support finished?". A **blocker** forces a change to the architecture or technology choice (portable-pty / ConPTY, Job Objects, daemon-owned PTY, Rust emitter) and stops further macOS-only investment on that seam. A **tuning issue** goes to the Windows backlog. Probes are independent; a failure in one does not skip the others. The harness does not classify: each probe reports `go` (every required case passed), `fail_observed` (complete, with at least one `platform_fail`), or `inconclusive` (completeness first, as above). The orchestrator reads the raw evidence behind every probe verdict, `go` included, before classifying: `go` stands only if that evidence shows each required case actually exercised, residual races in which only a harness event could be misattributed are checks the orchestrator performs on that evidence, and each `fail_observed` is classified against the two columns below. Windows-only code gets no further read-only review rounds; the owner's real run is the next evidence.

| ID | Probe (EARS) | Blocker if | Tuning issue if |
|---|---|---|---|
| C-1 ConPTY paste | When the required subset (Japanese, astral, decomposed, one multiline Japanese entry) is pasted after input gating through ConPTY into FakeCli (wire bytes, B-3) and into **both** real Claude Code and Codex (logical content, A-6), it shall match. FakeCli-only results are `inconclusive`; `readiness_gate_failed` is reported apart from corruption. | The required subset is corrupted or dropped in every newline mode and write strategy, or bracketed paste never reaches an app that enabled DECSET 2004. | Only chunked or very large pastes fail, a newline mode differs, or the Enter delay setting changes the outcome. |
| C-2 Startup + VT + keys | When real Claude Code and Codex start under ConPTY from a spaced, non-ASCII cwd under the owner's normal account, SessionStart shall arrive (a legible blocked screen passes only the diagnostic part; startup passes after the owner clears a real trust prompt with ungated `write_key` and SessionStart follows), and no CLI shall stall on an unanswered query. Codex uses A-3's trust sequence (≥3 launches) in both modes, with A-9's containment check. | A CLI cannot start under portable-pty 0.8.1 ConPTY; stalls on a query we cannot answer; Down/Enter/Esc fail in both plain VT and win32-input-mode encodings (§4.8); or a trusted hook cannot deliver with fresh env from this host context. Shell parsing vs. executable policy (Defender / SmartScreen) is distinguished first; if policy blocks the emitter and no permitted launch works, C-2 is a blocker for that host. | Snapshot glitches, a reply format, command quoting, `PATHEXT`/wrapper handling once a fix works. |
| C-3 Job Object kill | When stop is requested for FakeCli `spawn-tree` (once with `--hold-assign-until-child` to force the race) and for real CLIs with an MCP witness, the job (`KILL_ON_JOB_CLOSE`, assigned after spawn) plus fallback shall leave no owned process alive, with job members and escapees reported separately (a witness is an escapee only when it is live and missing from a successful membership snapshot; an enumeration failure is `harness_fail`, never an empty job); separately, one job-first launch (§4.6; JOB_LIST, then the shipped suspended branch only if JOB_LIST fails) shall start FakeCli `spawn-tree` with zero escapees. The report records whether the harness itself is in a job (`IsProcessInJob`) and that job's limits. | Survivors after fallback; descendants routinely break away; or assign-after shows race escapees and job-first is impossible together with ConPTY. | Race escapees under assign-after while job-first passes (fix later in `PtyRuntime`). |
| C-4 Reattach | When FakeCli `attachable` runs under ConPTY with no viewer for 30 s while emitting a counter marker, a separate `attach-client` process shall find in the live screen a marker the child recorded as emitted (not necessarily the latest, since it ticks every 500 ms), round-trip a fresh nonce it generated as pasted input, and deliver `write_key` Down, Enter, Esc, all to the same child. | No viewerless progress, or input/keys from another process cannot be delivered. | Replay fidelity, latency, resize. |
| C-5 Console windows | When the harness runs `--detached` (§4.8; verified `GetConsoleWindow()==0`) with a pre-trusted, unattended launch matrix (no attended steps), a calibrated `SetWinEventHook(EVENT_OBJECT_SHOW)` observer shall record no visible window attributable to our tree or its console hosts during startup, hooks, and paste; and after the owner closes the launching terminal, an `attach-client` started from a new terminal shall round-trip a fresh nonce through the same live witnessed child; survival is `go` only on that nonce. If breakaway is denied at launch, the launching process records `breakaway_denied` with the parent job's limits. Uncalibrated = `inconclusive`. | A repeatable flash on a required launch path that neither `CREATE_NO_WINDOW` (on every harness-owned non-interactive launch) nor the GUI-subsystem emitter variant (the same launch, cwd, and hook delivery re-run in the same session; the hook must still deliver) removes; or the session dies on terminal close (including after `breakaway_denied`) with no permitted launch that survives. | A helper flash removed by a verified launch flag. |

Probes report `verdict: "go" | "fail_observed" | "inconclusive"` with the per-case outcomes as evidence; the harness never emits `blocker` or `tuning`. The orchestrator classifies each `fail_observed` against this table.

## 2. Plan Layer: Claims and Falsifiers

| Claim (EARS one-liner) | Scenario that would falsify it | Set |
|---|---|---|
| C1. When `--settings` carries hooks, Claude Code shall run them without edits to any global hook configuration (folder-trust persistence is allowed). | `real claude-hooks`: diff the `hooks` keys of `~/.claude/settings.json` / `~/.claude.json` before/after. | A |
| C2. Codex hook trust shall survive relaunches that vary only env, for the tested source and version. | A-3 review prompt or uncorrelated SessionStart; A-4 mutated hook delivering without review. | A |
| C3. Session-flag hooks shall keep one trust key across cwds. | A-3 repeated from an explicit second cwd; the exact trust key of the session-flag SessionStart hook under test is compared with the first cwd's (other keys are ignored), and a new or different `hooks.state` key falsifies. | A |
| C4. Codex hooks shall see `CHALDEA_*` from the launch env. Inheritance of the full environment stays a hypothesis. | Emitter `env_missing` log, or no correlated delivery while Codex shows the hook ran. | A, C-2 |
| C5. Bracketed paste of the tested corpus shall survive macOS PTY and ConPTY byte-exactly under a defined newline mapping. | B-3, A-6, C-1. | A, B, C |
| C6. Pastes > 4 KiB shall not deadlock, and teardown shall not hang on a blocked writer. | B-10 non-reading child. | B |
| C7. The VT responder shall answer startup queries so no CLI stalls, and it shall be the harness, not luck, that answers on macOS. | B-5 negative control passing; a real launch stalling with an unanswered `query_log` entry. | B, A, C-2 |
| C8. On Unix, `killpg` reaches the process group; descendants that changed group or session are caught by `getsid` enumeration and the recorded-pid fallback, and nothing outside harness-established ownership is killed. | B-7 variants surviving or the unrelated fixture dying. | B, A |
| C9. On Windows, a job assigned after spawn plus fallback leaves no survivors, and a job-first ConPTY launch is feasible. | B-7 / C-3. | B, C-3 |
| C10. A daemon-owned ConPTY keeps running without a viewer and accepts text and keys relayed from another process. | C-4. | C |
| C11. In the tested launch modes, ConPTY children and hook launches of a console-less parent show no window. | C-5. | C |
| C12. The receiver rejects head, auth, and framing failures before reading any body; payload limits are enforced while reading it. | B-8. | B |

## 3. Facts Verified While Designing

"Source" facts are from reading code; "observed" facts come from help output or binary strings and do not establish runtime behavior.

portable-pty 0.8.1 (source, `~/.cargo/registry/src/index.crates.io-*/portable-pty-0.8.1/src/`):

- Unix spawn calls `setsid()` in `pre_exec` (`unix.rs:220`), then `TIOCSCTTY` (`unix.rs:229-237`), after resetting SIGCHLD/HUP/INT/QUIT/TERM/ALRM to default (`unix.rs:208-217`). Child pid = pgid = sid. `killpg` targets that process group only; a descendant can move to another group (`setpgid`) while staying in the session, or leave the session (`setsid`).
- `Child::kill` on Unix sends SIGHUP to the single pid, polls 5×50 ms, then SIGKILL on that pid only (`lib.rs:330-353`); the cloned killer sends only SIGHUP (`lib.rs:314-318`). Not a tree kill.
- Windows kill is `TerminateProcess` on the direct child only (`win/mod.rs:41-43`, `:71-72`). No Job Object anywhere in the crate.
- `CreatePseudoConsole` flags are `RESIZE_QUIRK | WIN32_INPUT_MODE` (`win/psuedocon.rs:86`); no `INHERIT_CURSOR`. `CreateProcessW` uses `EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT`, a one-entry attribute list, and drops the thread handle (`win/psuedocon.rs:124-147`, `:165`): no `CREATE_SUSPENDED` and no job list through the crate.
- A `conpty.dll` on the DLL search path is preferred over kernel32 (`win/psuedocon.rs:51-58`); which provider actually loaded is recorded via `GetModuleHandleW("conpty.dll")` + `GetModuleFileNameW`, not by checking for a sibling file.
- `ClosePseudoConsole` runs on drop (`win/psuedocon.rs:72-75`); the child exposes `as_raw_handle()` (`win/mod.rs:118`) for `AssignProcessToJobObject`.
- Dropping the Unix writer writes `\n` + VEOF to the PTY (`unix.rs:350-361`).
- Windows lookup walks `PATHEXT` (`cmdbuilder.rs:532-548`) and may pick `codex.cmd`, which `CreateProcessW` cannot run directly; real probes therefore take pinned native binaries.

alacritty_terminal 0.26.0 (source): DA1 `ESC[?6c`, DA2 `ESC[>0;<ver>;1c` (`term/mod.rs:1257-1271`); DSR 5/6 (`:1332-1347`), all as `Event::PtyWrite` (`event.rs:40`). OSC 10/11/4 arrive as `Event::ColorRequest` (`event.rs:37`, `term/mod.rs:1675-1688`) and `TextAreaSizeRequest` (`event.rs:43`); the embedder supplies values. Kitty query answered only with `kitty_keyboard = true` (`term/mod.rs:1275-1278`). Wide cells: `WIDE_CHAR` + `WIDE_CHAR_SPACER`, `LEADING_WIDE_CHAR_SPACER` at line ends (`term/cell.rs:22-28`); VS16 emoji is not widened. `TermMode::BRACKETED_PASTE` tracks DECSET 2004 (`term/mod.rs:61`, `:1970`).

tiny_http 0.12.0 (source): header lines are read without a length bound (`client.rs:80-100`) and the crate never sets a socket read timeout, so it cannot meet B-8's header and deadline limits. The receiver uses `httparse` instead (§4.3).

Claude Code 2.1.322 (observed): `--settings <file-or-json>` and `--session-id <uuid>` exist; `--bare`/`--safe-mode` skip hooks. Event names in the binary include `SessionStart`, `UserPromptSubmit`, `Stop`, `SessionEnd`, `PreToolUse`, `PostToolUse`, `PermissionRequest`, `Notification`, `SubagentStop`, `PreCompact`. Large pastes may display as `[Pasted text #N …]`; whether `prompt` carries the expanded text is an A-6 observation.

Codex CLI 0.170.0:
- Observed: `hooks` feature `stable true`; event names in the binary include `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, `PostToolUse`, `PreCompact`, `PostCompact`, `SubagentStart`, `SubagentStop`, `Stop`, `Interrupt`; review dialog `Hooks need review` / `Trust all and continue` / `Continue without trusting (hooks won't run)`; flags `--no-daemon`, `--no-alt-screen`, `--dangerously-bypass-hook-trust` (runs all enabled hooks; observation only, never on the acceptance path).
- Observed in the owner's `~/.codex/config.toml`: trust stored as `[hooks.state."<source>:<snake_event>:<group>:<index>"] trusted_hash = "sha256:…"`, so source and position matter, not only the command string; a per-worktree project `.codex/hooks.json` gets a new key per path.
- Source (`docs/prior-art.md` row 6b, `codex-rs/hooks/src/engine/discovery.rs:455-483`): for `-c` hooks the key source is the synthetic `<session-flags>/config.toml`, independent of cwd. Open issue openai/codex#58011: the hash covers resolved defaults, so a Codex upgrade can flip trusted hooks to `Modified`.
- The npm install is `codex.js` (node) → native `codex`: an extra process layer, avoided by pinning the native binary.

## 4. Design

### 4.1 Layout, toolchain, dependencies

```
spikes/pty-hooks/
  Cargo.toml / Cargo.lock      # one package, empty [workspace]; builds use --locked
  rust-toolchain.toml          # channel = "1.96.0" on both hosts
  src/os/mod.rs     # THE OS boundary: spawn helpers, tree record/kill/enumerate, raw console, window observer
  src/os/unix.rs    # cfg(unix): killpg, getsid/pgid scan, proc_listallpids, termios
  src/os/windows.rs # cfg(windows): jobs, job-first spike launch, Toolhelp, console APIs, WinEvent observer
  src/pty.rs        # portable-pty spawn, reader/writer threads, resize (OS-agnostic)
  src/session.rs    # Session guard + watchdog (§4.7)
  src/vt.rs         # alacritty Term + EventListener responder + snapshot
  src/receiver.rs   # std TcpListener + httparse, auth, fixtures, witness registry, C-4 probe routes
  src/paste.rs      # corpus, validator, newline mapping, encoder
  src/report.rs
  src/scenario/{fake,real,win_probe}.rs   # win_probe compiles everywhere, refuses to run off Windows
  src/bin/chs.rs  chs-hook-emit.rs  chs-fake-cli.rs
```

No `cfg` outside `src/os/`. Harness, emitter, FakeCli, and attach-client talk only over localhost HTTP and the PTY; signals appear only inside `os/unix.rs` as the kill mechanism.

Pinned dependencies (exact `=`, versions checked with `cargo info`): `portable-pty =0.8.1`, `alacritty_terminal =0.26.0`, `serde =1.0.229` (derive), `serde_json =1.0.154`, `uuid =1.29.3` (`v4`), `ctrlc =3.5.3`, `httparse =1.10.1`, `subtle =2.6.1`; `cfg(unix)`: `libc =0.2.191`; `cfg(windows)`: `windows-sys =0.61.2` with `Win32_Foundation`, `Win32_Security`, `Win32_System_JobObjects`, `Win32_System_Threading`, `Win32_System_Diagnostics_ToolHelp`, `Win32_System_Console`, `Win32_System_LibraryLoader`, `Win32_UI_WindowsAndMessaging`, `Win32_UI_Accessibility`. The emitter uses std networking (`TcpStream::connect_timeout`) plus the `os` module for its ancestry chain. Payload comparison is byte compare plus first diff offset; no hashing. No async runtime.

### 4.2 Scenario CLI

```
chs install-emitter [--root R]        # copy chs-hook-emit to R/bin, print the fixed hook command strings
chs fake <paste|queries|stall|tree-kill|auth|emit|teardown|all>   [--hold-assign-until-child]
chs real <claude-hooks|claude-paste|claude-stall|codex-hooks|codex-paste|codex-stall|codex-warm|tree-kill>
chs print-launch <claude|codex>       # exact command; env NAMES with placeholder values, never a token
chs win-probe <paste|startup|kill|attach|console|all> [--detached]   # `all` runs each probe as a child `chs` process
chs win-probe attach-client --url U    # C-4 / C-5; per-run credential read from stdin, never argv or file
chs bundle                            # allowlisted evidence zip for owner review (§5)
common: --root R (~/.chs | C:\chs)  --out DIR (R/out/<run-id>)  --deadline-secs 180 (scenario budget = deadline - 10 s)
        --session-start-timeout 20  --keep-temp  --cols 120 --rows 40
real:   --prompt (claude-hooks: also submit one prompt)  --claude-bin PATH --codex-bin PATH (required on Windows and when PATH holds a shim; the resolved file must be the named native binary, so scripts and .cmd/.bat/.ps1 wrappers are rejected) --cwd DIR
paste:  --paste-newline <cr|lf|crlf> (default cr; real runs sweep all three) --enter-delay-ms 300 --chunk N
codex:  --hook-source session-flags --launches N
        --mutate-command --cwd2 DIR --codex-mode <no-daemon|default|both>
```

Real scenarios use the API (approved). `claude-hooks` without `--prompt` checks SessionStart only; with it, A-2. The harness stops the tree as soon as the needed `UserPromptSubmit` arrives. Resolved binary paths, modes, and SessionStart latency go into every report.

### 4.3 Hook injection, receiver, emitter

Fixed artifacts: emitter at `R/bin/chs-hook-emit`, under an owner-private root (`~/.chs`, mode 0700, on macOS); on Unix `install-emitter` checks ownership and write permissions from the spike root down and refuses any group- or world-writable directory there (Windows `C:/chs/bin/chs-hook-emit.exe`: forward slashes for Git Bash, no spaces so cmd/PowerShell/bash parse it unquoted). Command string: `<abs emitter> claude` or `<abs emitter> codex`; the event comes from the payload's `hook_event_name`.

Per-launch environment on the `CommandBuilder`. Before setting it, the harness removes inherited agent identity variables: `CLAUDECODE`, every `CLAUDE_CODE_*`, `CODEX_THREAD_ID`, `CODEX_SESSION_ID`, other `CODEX_*` session variables, and any inherited `CHALDEA_*` (the harness may itself run inside Claude Code).

| Var | Value |
|---|---|
| `CHALDEA_HOOK_URL` | `http://127.0.0.1:<port>/hook` (fresh port per launch) |
| `CHALDEA_HOOK_TOKEN` | 256-bit random hex, fresh per launch |
| `CHALDEA_THREAD_ID` | UUID v4, fresh per launch |
| `CHALDEA_HOOK_LOG` | optional; emitter appends one-line failures (never the token) |

Claude: `R/runs/<id>/claude-settings.json` (no secrets; 0600 on Unix) registers the command for `SessionStart`, `UserPromptSubmit`, `Stop`, `SessionEnd` with `timeout: 10`; launch `claude --settings <file> --session-id <thread uuid>`.

Codex (production must use the real `CODEX_HOME`, so that is what is measured; no disposable profile, which would need the auth credential copied):
- `session-flags` (default): `-c 'hooks.SessionStart=[{hooks=[{type="command",command="<cmd>",timeout=10}]}]'`, same for `UserPromptSubmit` and `Stop`, fixed order. No file written; Codex itself adds `hooks.state` entries to the real `config.toml`. The harness records the file's hash before/after and the added keys.
- `session-flags` is the only source the spike builds. A global `~/.codex/hooks.json` (`codex-home`) source, which the owner approved only if session-flag hooks cannot persist trust, is a follow-up built after A-3 shows that; a per-path project `hooks.json` control is deferred with it. `--hook-source` with any other value exits 2 ("codex-home mode is not built; requires A-3 result showing session-flags cannot persist trust"). A-4 mutates one byte of the session-flag command string.

Receiver: std `TcpListener` on `127.0.0.1:0`, one request per connection (`Connection: close`), thread per connection (max 16). Each connection gets an absolute 5 s deadline; every read and every response write recomputes its timeout from `deadline - now`, the wait for the launched CLI's pid and the fixture store get the remaining time, and a hook or witness request past its deadline is answered 408 without publishing (checked again under the state lock). The head is read up to its `\r\n\r\n` boundary (at most 8 KiB; no body byte is consumed before the checks) and parsed with `httparse` (32-header array); then, before any body byte: method/path `POST /hook` or `POST /witness` (plus `/probe/*` only while C-4 or C-5 runs; `GET /probe/screen` is bodyless and needs no `Content-Length`) else 405/404; exactly one `Host`, equal to `127.0.0.1:<port>` or `localhost:<port>`, else 421; exactly one `Authorization`, Bearer compared with `subtle::ConstantTimeEq`, else 401; any `Transfer-Encoding` → 400; exactly one `Content-Length` ≤ 4 MiB on POST else 411/413. Accepted bodies are stored byte-for-byte as `fixtures/<agent>/<cli-version>/<launch-id>/<Event>-<seq>.json`, where `<launch-id>` is unique per launch, `<Event>` is a known event name from the parsed payload or `unknown`, and `<seq>` is a report-wide counter; `.meta.json` holds only allowlisted fields (receive time, run id, thread id, agent, emitter version, emitter ancestry). Rejections are counted, never stored, and never mark readiness.

Binding: Claude events bind by the predetermined `--session-id`. For Codex, the first `SessionStart` whose `X-Chaldea-Thread-Id` matches the launch and whose emitter's nearest CLI ancestor (Codex or FakeCli process in the chain) is our launched CLI pid binds that launch's `session_id`; a nested CLI inheriting the env fails this test. Every other event is kept as `raw_unknown` (`foreign_session`); unparseable JSON as `raw_unknown`. More than one candidate = `inconclusive`: ambiguity revokes the launch's writer gate for good (bumping its generation, so a queued paste is dropped before its first byte), and launch verdicts are taken from the receiver's final summary after it has stopped and drained. Witnesses (`POST /witness`) are liveness assertions only and never add a pid to the kill set (§4.6); the survivor rule applies to expected session witnesses, and the `unrelated` fixture is a negative control whose survival passes.

Emitter (`chs-hook-emit`): if URL or token is missing, or the URL is not exactly `http://127.0.0.1:<port>/hook` or `http://localhost:<port>/hook`, exit 0 at once without reading stdin. Otherwise read stdin up to 4 MiB + 1; if over, log and exit 0 without POST. POST the raw bytes with `Authorization: Bearer`, `Host`, `X-Chaldea-Thread-Id`, `X-Chaldea-Agent`, `X-Chaldea-Emitter-Version`, and `X-Chaldea-Emitter-Ancestry` (own pid, then up to 8 ancestors as pid + name); connect 1 s, total 3 s including the stdin read. Always exit 0; never write stdout (Claude injects hook stdout into context; exit 2 would block the prompt). Windows: console-subsystem binary that allocates no console; a `windows_subsystem = "windows"` variant is built for C-5's mitigation re-run.

### 4.4 PTY I/O, input gating, paste, VT

- One reader thread drains PTY output continuously into `pty-output.bin` and the `vte` processor feeding `Term`. One writer thread owns the PTY writer and a channel; VT replies (`PtyWrite`, `ColorRequest` with fixed fg `#d0d0d0` / bg `#1e1e1e`, `TextAreaSizeRequest` with 8×16 px cells) share that channel so they stay ordered with input. `query_log` records each query, reply content, latency, and whether the harness wrote the reply.
- Input gating: automated paste is sent only after a bound SessionStart and while `TermMode::BRACKETED_PASTE` is set. This reads a terminal mode flag the app set, not screen text, and only gates input; it never infers thread state. If the mode is never set, the paste stays `unconfirmed`, is reported as `readiness_gate_failed`, and is not retried. `write_key` is not gated: it is the emergency keypad path for startup prompts, used only by an attending owner (C-2) or a probe client (C-4); A-7 sends no input at all.
- Paste: validate (reject ESC and C0 other than TAB/CR/LF; zero bytes written on reject); map line endings per `--paste-newline` (LF and CRLF in the payload both map to the chosen form); write `ESC[200~` + payload + `ESC[201~`; after the write completes, wait `--enter-delay-ms` (300 ms), then send `CR` as a separate write. The delay is a tested setting, not proof that the app consumed the paste. `--chunk N` splits the paste into N-byte writes. Oracles: FakeCli compares wire bytes against the mode's mapping; real `UserPromptSubmit` compares logical content (both sides LF-normalized), all other bytes exact.
- Corpus (`out/corpus.json`): ASCII; `日本語の入力テスト、句読点。「括弧」`; `ｶﾀｶﾅ`; astral `𠮷野家`; `😀`, `👨‍👩‍👧‍👦`, `🇯🇵`, `❤️`; decomposed `か\u{3099}`, `e\u{301}`; multi-line CJK with LF and with CRLF; 4 KiB+1 and 64 KiB mixes (64 KiB FakeCli only; real CLIs ≤ 8 KiB). The required subset for C-1 is the Japanese, astral, and decomposed entries plus one multiline Japanese entry.
- Snapshot: per visible row, `cell.c` + zero-width chars, skipping spacer cells, right-trimmed; JSON adds cols, rows, cursor, `alt_screen`, `bracketed_paste`, `decided_by: "hook_timeout"`. Written only after the SessionStart wait has expired. C-4's live screen uses the same walker on the current grid, on demand.

### 4.5 FakeCli modes

`chs-fake-cli --mode <echo|queries|stall|banner|attachable|spawn-tree|sleeper>`. Raw mode (Unix termios; Windows `SetConsoleMode` + `ENABLE_VIRTUAL_TERMINAL_INPUT`, `ReadConsoleW` → UTF-8), then `ESC[?2004h`. Every FakeCli process registers `(pid, start_time, role)` via `POST /witness` at start (liveness assertion only).
- `echo`: sends SessionStart; on CR after `ESC[201~` sends `UserPromptSubmit` with the pasted bytes through the real emitter; `--nested` starts a nested FakeCli `echo` that inherits the same env and sends its own `SessionStart` before and after the parent's (B-11); `--ignore-input` stops reading (B-10).
- `queries`: sends DSR 6/5, DA1, DA2, OSC 10/11, `CSI ? u`; checks reply content; exit 0 only if all arrive within 500 ms.
- `stall`: draws the fixed expected grid (CJK, combining, emoji) and never sends SessionStart; `--silent` draws nothing. `banner`: draws a fake trust prompt and sends a valid SessionStart (B-6).
- `attachable` (C-4, C-5): sends a bound SessionStart first (so the paste gate opens before any nonce), then prints an incrementing marker every 500 ms and records every value emitted; reports pasted nonces and received key events (Down, Enter, Esc, plain VT or win32-input-mode) to the receiver.
- `spawn-tree`: at t=0 spawns `sleeper` children: plain, SIGTERM+SIGHUP-ignoring, double-forked (reparented, same PGID), and a separate `setsid` escapee (Unix); on Windows a child spawned immediately and one requesting `CREATE_BREAKAWAY_FROM_JOB`. It reports the first live descendant so `--hold-assign-until-child` can delay job assignment until then. Every sleeper takes `--max-life-secs`, so a fixture that escapes containment still ends by itself; `--escape-orphan` (the escape probe, §4.6) adds a `setsid` grandchild whose parent exits at once.
- `sleeper`: minimal MCP stdio server (answers `initialize`, empty `tools/list`), so real CLIs keep it alive; registered as MCP server for real tree-kill (`--mcp-config` for Claude, `-c mcp_servers.chs.command=…` for Codex). In MCP role it attests via `/witness` only after `initialize` completes; A-8 requires that attestation before stop.
- `unrelated` negative control: a `sleeper` the harness starts outside the session; it registers via `/witness`, and its survival passes B-7.

### 4.6 Process-tree cleanup

Enumeration: macOS `proc_listallpids` + `proc_pidinfo(PROC_PIDTBSDINFO)` (pid, ppid, pgid, start time) plus `getsid`; Windows `CreateToolhelp32Snapshot` + `GetProcessTimes`, plus `JobObjectBasicProcessIdList`. Kill set = harness-established ownership only: the process group (Unix), job membership plus the retained child handle (Windows), and pids the harness recorded by its own ppid walk and (Unix) pgid/sid scan from the child before stop, seeded only from identities (pid + start time) it verified. Witnesses never enter it; an expected session witness alive after stop is a survivor (fail); the `unrelated` negative control must survive. Before any fallback kill, (pid, start_time) is rechecked to avoid pid reuse. Containment limit: on macOS a `setsid` grandchild whose parent has already exited cannot be contained by pgid or ppid (no subreaper); `fake tree-kill --escape-probe` (not part of `fake all`) documents it (`containment_limit: true`; the orphan, a fixture the harness spawned, is removed afterwards through its verified witness identity and reported as `fixture_cleanup`), and A-8 with real CLIs shows whether real MCP servers do this.

- macOS: `killpg(SIGHUP)`, `killpg(SIGTERM)`, 2 s grace, `killpg(SIGKILL)`; then SIGKILL each recorded pid still alive (`escaped_primary`, `killed_by: fallback`). portable-pty's `Child::kill` is not used.
- Windows assign-after (B-7, C-3, real probes): `CreateJobObjectW` + `KILL_ON_JOB_CLOSE`, `AssignProcessToJobObject(child.as_raw_handle())` right after `spawn_command`. `--hold-assign-until-child` (FakeCli only) waits until `spawn-tree` reports a live descendant before assigning, forcing the race once. Stop: record job members, `TerminateJobObject`, check job members and escapees separately, run the fallback on recorded escapees, and only then close ConPTY (closing it can itself kill console clients and hide the result).
- Windows job-first spike launch (C-3 only, inside `os/windows.rs`; portable-pty is not patched or vendored): raw `CreatePseudoConsole` + `CreateProcessW` with one attribute list holding `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` and `PROC_THREAD_ATTRIBUTE_JOB_LIST`; the `CREATE_SUSPENDED` + assign + `ResumeThread` branch ships in the same binary and runs only if JOB_LIST fails. The suspended branch counts only if `ResumeThread` returns 1. Spawns FakeCli `spawn-tree`; one passing run establishes feasibility (zero escapees). Its kill set is the job, the retained process handle, and descendants recorded with verified lineage during the wait (witnesses only check survival); verification and fallback run before ConPTY is closed. Production vendoring or patching is deferred. It decides whether assign-after race escapees are tuning or a blocker (C-3).

### 4.7 Lifecycle, deadlines, temp files

- The `Session` guard exists from the instant `spawn_command` returns, before job assignment or I/O setup, and owns the kill handles: the portable-pty child killer and handle (usable even if job or pgid setup failed), pgid / job, and recorded pids. On Unix the parent's slave handle is dropped right after spawn so EOF reaches the reader.
- Teardown order: kill tree → check owned processes and witnesses → fallback on recorded escapees → drop the Unix writer (avoids the stray `\n` + VEOF reaching a live CLI) → close ConPTY while the reader keeps draining → join threads with a bound → stop receiver → remove `R/runs/<id>` unless `--keep-temp`.
- Watchdog thread with an independent stop path: at the scenario cutoff, `--deadline-secs` minus 10 s, it kills every registered tree directly through the guard's kill handles (using the retained child handle whenever job assignment or pgid setup did not succeed, even if a job exists), never via the writer thread, so teardown finishes inside the hard deadline; at the hard deadline, on Ctrl-C (`ctrlc` flag), or on panic (`catch_unwind`) it also kills, restores a real-home `hooks.json` if one is installed, writes `report.json` from what is known, and calls `process::exit`. Normal teardown and the watchdog share one kill path under one lock; a tree is marked done only after its kill and fallback completed, so a Ctrl-C that lands mid-teardown still kills. Blocked writer or reader threads are abandoned, not joined. No separate supervisor process.
- Bounded waits: SessionStart 20 s; UserPromptSubmit 30 s real / 5 s fake; query replies 500 ms; emitter 1 s / 3 s; receiver 5 s per request; kill grace 2 s + verification 5 s; scenario budget `--deadline-secs` − 10 s.
- Short paths: roots `~/.chs` (macOS) and `C:\chs`, 8-hex run ids, `CARGO_TARGET_DIR=C:\chs\t`.

### 4.8 Windows probes (set C, minimum implementation)

- `paste` (C-1): `fake paste` in all newline modes plus `real claude-paste` / `real codex-paste` with the required subset. Records code page, the ConPTY module loaded after each child's first PTY launch, and whether `CSI ?9001h` appeared; paste is never switched to win32-input encoding.
- `startup` (C-2): `real claude-hooks` and `real codex-hooks --launches 3 --codex-mode both`, `codex-warm`, from `--cwd "C:\chs\work\日本 dir"`, plus one attended launch in a fresh folder: `chs` shows the snapshot and relays the owner's Down/Enter/Esc keypresses as ungated `write_key`; the attended case passes only if at least one key was written through the relay while unbound and SessionStart arrived after the writer reported that write done; otherwise it is `not_exercised`. Each launch's query log must show every query answered with a reply the harness wrote. If the CLI requested `CSI ?9001h` and plain VT keys fail, those three keys are retried in win32-input-mode encoding (`CSI Vk;Sc;Uc;Kd;Cs;Rc _`, key-down then key-up); the report names the encoding that worked. If the emitter fails to launch, the probe records the exit code and Windows event (policy block vs. shell parse error).
- `kill` (C-3): `fake tree-kill`, `real tree-kill` with the MCP witness, job-first launch, `IsProcessInJob` + job limits of the harness. The job-first escapee check takes one pre-stop membership snapshot, then checks each session witness's liveness against it: a required witness counts only if it is positively alive and in the job, one that already exited leaves the case `not_exercised`, and an enumeration or liveness lookup failure is `harness_fail`. In `fake tree-kill` (B-7), a witness is platform evidence only if the job assignment succeeded and it was positively alive before the stop; an unknown liveness is `harness_fail`, and race escapees count only from a held run that was itself exercised.
- `attach` (C-4): FakeCli `attachable` viewerless for the full 30 s, starting once the first marker is on screen (cut short = `inconclusive`); the probe routes `GET /probe/screen` (live grid), `POST /probe/input` (gated paste), `POST /probe/key` (ungated `write_key`), and `POST /probe/confirm` (C-5) exist only during this probe, behind the same Host check and a separate per-run credential given to `attach-client` on stdin. The client sends its own nonce and Down/Enter/Esc; each request is attributed to the child by waiting for the child's next report to show that request's effect (504 otherwise), and FakeCli parses its input as one byte stream across reads, treating a lone ESC as Esc only after 500 ms of idle input; the relay sends each key as one write. Pass requires the replayed marker to be one the child recorded as emitted and later than the marker on screen before the viewerless interval, and every request attributed; a relay, gate, writer, or witness-timeout failure, or a PTY reader that stopped (a stale screen), is `harness_fail`.
- `console` (C-5): the supported daemon launch path: `--detached` relaunches the harness with `DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB | CREATE_NEW_PROCESS_GROUP`, passing explicit binary paths, records whether breakaway was permitted and the parent job's limits, and verifies `GetConsoleWindow()==0`. An out-of-context `SetWinEventHook(EVENT_OBJECT_SHOW)` observer on its own thread with a message loop records each event's owner pid and that owner's parent at callback time (no visibility filter, so a flash already hidden still counts) and maps them, including conhost/OpenConsole hosts, to the recorded tree. Calibration: the detached harness spawns a short-lived console exe (`cmd.exe /c exit`) without `CREATE_NO_WINDOW`; its conhost window must be detected and attributed. Every other harness-owned non-interactive launch (`--version` probes, scenario wrapper children) uses `CREATE_NO_WINDOW`. Then the pre-trusted, unattended matrix (`real claude-hooks`, `real codex-hooks` in each Codex mode, `fake paste`; no attended steps), each launch under its own observer and each required to pass for its flash judgment to count. An event whose owner cannot be identified at callback time, or whose parent chain is broken in the samples, is recorded as `unattributed`, and any unattributed event leaves no-flash `not_exercised`. Stopping the observer reports its health with the events; a failed `WM_QUIT` post, a thread that does not exit, or a message loop that ends on an error is `harness_fail`, as is an observer that fails to start. On a flash, the same launch (same cwd and hook delivery, Claude or Codex) is re-run with the GUI-subsystem emitter, and its hook must still deliver. Finally, with FakeCli `attachable` still running, the owner closes the launching terminal and runs `attach-client --confirm-close` from a new terminal: it asks the owner to confirm the close and records it via `/probe/confirm`, then a fresh nonce, from a request that started after the confirmation, must round-trip through the same `/probe/input` route to the same child, with the credential on stdin; anything less is `not_exercised`. A `platform_fail` needs positive death evidence for that child, and a death after the harness's own teardown began (cutoff, interrupt, watchdog) is `harness_fail`. If breakaway is denied, the launching process writes `breakaway_denied` and the parent job's limits to the report before exiting.

On macOS `chs win-probe` exits with `status: "error"`, `reason: "windows_only"`.

### 4.9 Report

```json
{
  "schema": "chs-report/1", "scenario": "win-probe kill", "run_id": "1a2b3c4d",
  "status": "pass|fail|inconclusive|timeout|error",
  "host": {"os": "windows", "os_version": "…", "conpty_module_after_launch": "C:\\Windows\\System32\\kernel32.dll", "in_job": false},
  "config": {"bins": {"codex": "C:\\…\\codex.exe"}, "versions": {"codex": "0.170.0"}, "codex_mode": "no-daemon", "hook_source": "session-flags", "paste_newline": "cr"},
  "checks": [{"id": "B-7", "case": "ignore-term", "result": "pass|platform_fail|harness_fail|missing|not_exercised", "cause": "…", "evidence": {}}],
  "probes": [{"id": "C-3", "verdict": "go|fail_observed|inconclusive", "why": "…", "evidence": {"eval": {}, "cases": [], "extra": {}}}],
  "errors": []
}
```

Reports carry only verdicts, tested configuration, and deciding evidence. Raw material stays beside them: `fixtures/`, `snapshots/`, `pty-output.bin`, `corpus.json`.

## 5. Runbook

Phase 1, macOS (sets A and B):
1. `cd spikes/pty-hooks && cargo build --release --locked && ./target/release/chs install-emitter && ./target/release/chs fake all`.
2. Pre-trust: run the command from `chs print-launch claude|codex` once in a normal terminal in each folder it lists (`~/.chs/work/claude`; `~/.chs/work/codex` and `~/.chs/work/codex2`; and C-2's `~/.chs/work/日本 dir`; env values do not affect trust; the command string is identical) and accept folder trust. For Codex, open "Review hooks", check that every pending entry is one of the spike's commands, and trust only then; if anything else is pending, stop and report. `print-launch` also prints the follow-up commands below, each with the exact resolved `--claude-bin` / `--codex-bin`. The CLI binary is found on PATH and its symlink chain is followed to the file it ends at; unless that file is named `claude` / `codex` and is not a script, `print-launch` and `real` exit 2 and ask for `--claude-bin` / `--codex-bin`. A version-manager shim (e.g. mise) or the npm `codex.js` needs the native binary, on macOS `…/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex`; `print-launch` prints that exact binary and puts it in every follow-up command.
3. Set A, every command with `--allow-api-usage` (real CLIs refuse to launch without it) and the `--claude-bin` / `--codex-bin` that `print-launch` printed: `chs real claude-hooks --prompt`, `chs real codex-hooks --launches 3 --cwd2 ~/.chs/work/codex2 --codex-mode both`, `chs real codex-hooks --mutate-command` (answers the A-4 y/n prompt about the review entry), `chs real codex-warm`, `chs real tree-kill --agent claude|codex`, `chs real claude-paste`, `chs real codex-paste`, then `chs real claude-stall` / `codex-stall` in fresh folders. Each `real` scenario has its own default `--deadline-secs` (paste sweeps longest).
4. Gate to phase 2: set B passes on macOS, `cargo check --target x86_64-pc-windows-msvc` passes (type-checks `cfg(windows)` code only; it does not link or run), set A recorded.

Phase 2, Windows (owner; sets B and C, same commit, one sitting):
1. Prereqs: Windows 10 1809+ (record `winver`), rustup (the repo's `rust-toolchain.toml` installs 1.96.0 MSVC), VS Build Tools "Desktop development with C++", Git for Windows, `claude` and `codex` installed and logged in, the owner's normal account and security policy.
2. Build and set B (PowerShell):
   ```
   mkdir C:\chs; git clone <repo> C:\chs\src
   $env:CARGO_TARGET_DIR = "C:\chs\t"
   cd C:\chs\src\spikes\pty-hooks; cargo build --release --locked
   C:\chs\t\release\chs.exe install-emitter
   C:\chs\t\release\chs.exe fake all
   ```
3. Pre-trust with `chs.exe print-launch claude --claude-bin <native claude.exe>` and `print-launch codex --codex-bin <native codex.exe under the npm package's vendor\…>`, run once in Windows Terminal in each folder it lists, including C-2's `C:\chs\work\日本 dir`, reviewing Codex hook entries as in phase 1.
4. Set C: `chs.exe win-probe all --claude-bin … --codex-bin … --allow-api-usage` (each probe runs as its own `chs` child process with its own budget and `--deadline-secs`; the parent's deadline is the sum of the budgets, and every planned probe gets a verdict; each child writes to a fresh output directory the parent creates and must report the run id the parent assigned, otherwise its result is `not_run` (`harness_fail`, or `inconclusive` for a whole probe)), staying at the keyboard for C-2's attended prompt; then `chs.exe win-probe console --detached --claude-bin … --codex-bin … --allow-api-usage` (unattended until the terminal-close step), closing the terminal when told and running the printed `attach-client --confirm-close` command from a new terminal (it asks you to confirm the close, then sends a fresh nonce).
5. Send back: `chs.exe bundle` writes `C:\chs\chs-out-windows.zip` from an allowlist (reports, fixtures, snapshots, corpus; no `pty-output.bin` unless asked). The harness never writes its own tokens into outputs, but raw payloads and snapshots may contain whatever the CLI sent or displayed; the owner reviews the bundle before sending.

Send the bundle back; the orchestrator classifies. It reads the raw evidence behind every C verdict, `go` included (§1 C), and each `fail_observed` against §1 C: a blocker halts further macOS-only work on that seam until the orchestrator and owner decide; tuning items go to the Windows backlog. An `inconclusive` probe is rerun or investigated, never classified.

## 6. Prior Art

Full survey with evidence: `docs/prior-art.md`. Rows that drive this design:
- Windows tree kill (1a, 1b): Job Objects are the only surveyed mechanism that catches detached descendants; Zed documents the assign-after race; `PROC_THREAD_ATTRIBUTE_JOB_LIST` with a pseudoconsole is unverified anywhere, hence C-3's job-first launch and forced race.
- Bracketed paste (4a, 4c, 4e): every surveyed tool brackets only when DECSET 2004 is on; newline handling inside brackets is split (hence `--paste-newline`); herdr waits 300 ms before Enter.
- Codex trust (6b): session-flag keys are cwd-independent in source; trust can flip on upgrade (openai/codex#58011).
- Nested agents (6c): herdr scrubs inherited agent identity env and drops foreign `session_id` events.
- Reattach (8): herdr serves attach clients for daemon-owned ConPTY panes on Windows and checks for a kill-on-close parent job.

## 7. Open Questions and Risks

Owner decisions: O1 trust clicks on both machines and snapshot legibility (A-7); O2 whether a `codex-home` source is built and run if A-3 shows session-flags cannot persist trust (approved conditionally; the owner sees the result first); O3 running set C.

Orchestrator may choose: timeout and corpus defaults.

Risks:
- R1. Session-flag hooks may not persist trust; the fallback, a global `codex-home` source, is built only after A-3 shows that.
- R2. Codex may filter env for hooks under some `shell_environment_policy`; per-session data would then need another path (lookup by `session_id`).
- R3. Inbox conhost handling of `WIN32_INPUT_MODE` (0x4) is unknown; open-source OpenConsole ignores the flag. Behavior varies by Windows build and loaded ConPTY module.
- R4. Hook shells differ on Windows (Claude: Git Bash; Codex: unknown); the fixed command must parse in each.
- R5. Codex trust can flip to `Modified` after an upgrade (openai/codex#58011), so A-3 holds for one version only. The adapter must treat a post-upgrade review prompt as an expected landmine handled by the A-7 snapshot path, and record the CLI version with trust keys.
- R6. The assign-after race is timing-dependent; one clean run does not prove absence, which is why job-first feasibility decides C-3.
- R7. Set C is a go/no-go screen from one owner run, not a support claim.
- R8. VS16 emoji width differs between `alacritty_terminal` and the CLIs; affects snapshot layout only.
- R9. The absolute emitter path ties the hook string to an install location; moving it re-triggers Codex review once.
- R10. A 20 s SessionStart timeout may fire on a loaded host (T3 Code allows Claude 5 min); the latency recorded in every run checks it.

Known constraints (accepted; the orchestrator checks them on the evidence):
- K1. Ambiguity and gate revocation are separate steps: after the receiver publishes a conflicting SessionStart, one queued paste write can start before the revocation callback runs. The drained receiver summary still makes that launch's verdict inconclusive, and that paste is not counted as evidence.
- K2. B-10's Ctrl-Break delivery runs under the `CREATE_NO_WINDOW` wrapper on Windows and is verified only by the owner's run; a B-10 `inconclusive` or `harness_fail` there is a harness issue, not a platform finding.
- K3. Version-named native Claude binaries (e.g. `~/.local/share/claude/versions/2.1.161`) are refused by the binary-name check; on this Mac the mise-installed `claude` binary is the supported path.

## 8. Out of Scope

`chaldea attach` (only the C-4 feasibility probe), Windows tuning beyond the set C minimum, patching or vendoring portable-pty, Android PWA, SQLite event store and sync, daemon architecture, adapter capability tables, worktree management, Rescue tokens/A2UI, CI, Claude's native `"type": "http"` hooks.
