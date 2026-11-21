# Chaldea Architecture

## 1. Purpose Layer (Acceptance Criteria)

- **Native CLI fidelity**: When a user or orchestrator launches Claude Code or Codex CLI through Chaldea, the CLI runs inside an interactive PTY and retains all native interactive features (hooks, skills, resume, permissions).
- **Multi-agent orchestration support**: An orchestrator session can spawn isolated child threads (with git worktrees), exchange messages via pull-based `mailbox`, inspect child outcomes, and keep worktree ownership metadata without losing uncommitted or unpublished work.
- **Cross-platform hosts + Android remote**: macOS and native Windows machines act as independent hosts (local SQLite as source of truth); Android connects remotely via a single PWA (Phase 2: Cloudflare Workers + Durable Objects + Cloudflare Access) for reading, approvals, short prompts, and emergency VT keypad recovery.
- **Japanese IME reliability**: Composing and confirming Japanese text on macOS, Windows, and Android never triggers accidental submission, and multi-byte UTF-8 prompts arrive at the target CLI in the adapter's documented expected form, through its input encoding (PTY Bracketed Paste when DECSET 2004 is on, or keyed input for Codex on Windows).
- **Self-healing boundaries**: Unparseable CLI transcripts/hooks are preserved as `raw_unknown` (never silently dropped); stalled startup/non-hook prompts surface a live `alacritty_terminal` VT snapshot and keypad; a dedicated Rescue Agent can inspect sessions cross-cuttingly and emit scoped A2UI recovery panels.

## 2. Decision Classification

### Irreversible Core (Fix Early)
1. **Event & sync contract**: `stream_id`, monotonic `seq`, `schema_version`, `command_id` idempotency, 3-axis thread state (`process` / `turn` / `task`), worktree ownership (`base_commit`, owner, short base path support).
2. **Auth & permission boundaries**: localhost Bearer token + `Host` check, regular vs Rescue scope tokens, browser CF Access vs host Service Token separation.
3. **Crash-only & Windows-compatible foundation**: persisted `session_id` + `--resume`, `PtyRuntime` trait isolation, Job Object / pgid process-tree cleanup, Rust single-binary hook emitter (`chaldea hook-emit`).
4. **PWA origin & Japanese IME foundation**: single-origin strategy (avoiding Private Network Access conflicts) and plain `<textarea>` IME-safe input.

### Reversible Layers (Defer Intentionally)
- Desktop native shell (PWA -> Tauri v2) and Android native wrapper/app (PWA -> Capacitor / Expo).
- Specific UI features and layout details (diff view, desktop split-pane layout, visual styling — wireframes live in `docs/design/chaldea.pen`).
- Phase 2 Cloudflare Workers + Durable Objects relay (added outside local-first MVP).
- Splitting PTY ownership into a separate long-lived process behind `PtyRuntime`.
- Expanding the A2UI component catalog beyond the initial minimal set (5–6 components).

### Unavoidable Landmines (Self-Healing Seams)
- **External CLI schema drift**: Persist `raw_unknown` + CLI version -> add to `fixtures/` contract corpus -> update adapter & verify.
- **Unexpected TUI prompts (trust, login, update, sandbox network)**: `SessionStart` timeout / `unconfirmed` input triggers `NeedsAttention` + `vt_snapshot` -> emergency VT keypad or Rescue Agent A2UI.
- **OS runtime quirks (ConPTY, worktree locks)**: Built-in `chaldea doctor` round-trip self-check.

## 3. Technology Stack

- **Host Core & Daemon (macOS / Windows)**: Rust workspace with Sans-IO separation (`crates/core`, `crates/pty-runtime`, `crates/daemon`) — `tokio`, `axum`, `rusqlite`, `portable-pty =0.8.1` (macOS/Unix), an own `windows-sys` ConPTY backend (Windows), `alacritty_terminal`.
- **Contracts**: Rust types as source of truth -> generated TypeScript types (`ts-rs`) + runtime boundary validation (`packages/contracts`).
- **Client UI (Desktop / Android)**: React + TypeScript (strict) + Vite + Tailwind as a single PWA (`packages/web`).
- **Remote Relay (Phase 2)**: Cloudflare Workers + SQLite-backed Durable Objects (Hibernatable WebSockets) + Cloudflare Access (`packages/relay`).

## 4. The 5 Seams

1. **Sync Seam**: `GET /api/events?stream_id=<id>&after_seq=<N>&limit=200` (snapshot fallback when cursor is invalid), WS live notifications, `command_id` idempotency, read-only lock with freshness indicator when host is offline.
2. **Ingest & Event Store Seam**: SQLite append-only log (`user_message`, `assistant_message`, `tool_activity`, `status_changed`, `approval_requested`, `artifact_diff`, `surface_update`, `raw_unknown`). Explicit `session_id` / `transcript_path` only (no implicit directory guessing); generation + `(path, offset)` committed atomically with events; `transcript_path: null` allowed (Codex); size-bounded/referenced large tool outputs.
3. **Agent Control & Adapter Seam**: `chaldea spawn / send / wait / stop / mailbox`; input state machine `queued -> sent -> acked / unconfirmed` (confirmed via `UserPromptSubmit` etc., never auto-resend `unconfirmed`); adapter capability declaration (hook approval support, ack support, resume flags); worktree short base path override (e.g., `C:\wt\...` for Windows MSVC `link.exe` 260-char limit).
4. **Extension & Rescue Seam**: One-shot JSON hooks (`before_spawn`, `after_close`, `attention`) + authenticated `POST /events`; Rescue Agent token with cross-session read (`events`, `raw_unknown`, `vt_snapshot`, `doctor`) and scoped A2UI (`Host | Workspace | Thread`) + `surface_action` execution.
5. **PTY Seam**: `PtyRuntime` trait (`spawn`, `write_text`, `write_key`, `snapshot`, `attach`, `kill`) sealing POSIX PTY / Windows ConPTY, `alacritty_terminal` headless VT query responder (DSR, DA1, OSC 10/11, kitty keyboard, wide-char), Job Object / pgid termination, and crash-only `--resume`.

## 5. External Quirks (Observed Facts)

Observed on macOS with Claude Code 2.1.322 and Codex CLI 0.170.0, and on native Windows (inbox ConPTY, console code page 932) with Claude Code 2.1.326 and Codex CLI 0.169.0 where marked (spike evidence: `docs/spikes/pty-hooks-spike.md` §3).

- **Claude Code**:
  - New folder trust prompt defaults to `No, exit` — never blindly send `Enter`.
  - Launch-time `--settings <temp.json>` + `--session-id <uuid>` cleanly injects hooks without touching global settings, and `SessionStart` arrives at startup carrying the given session id.
  - `UserPromptSubmit.prompt` is Claude's own rendering of the input, not the pasted bytes: TAB becomes 4 spaces, combining sequences are NFC-composed, and large pastes arrive wrapped in `<pasted_content id="…">`.
  - MCP servers from `--mcp-config` run as descendants of the CLI process; they need `"type": "stdio"` and their env stated explicitly.
- **Codex CLI**:
  - Startup can present prompts before the first turn (Update available, Folder trust, `Hooks need review`).
  - No hook fires at startup: `SessionStart`, `UserPromptSubmit` and `Stop` all fire when the first turn runs. Readiness therefore cannot be gated on `SessionStart`. The adapter passes the first prompt as the positional argument and never writes to the PTY before the session is bound. Inside the review dialog, digit keys pick options (`2` = "Trust all and continue"), so stray input there can approve hooks.
  - Hooks passed via `-c hooks.<Event>=[…]` (source `<session-flags>`) are trusted once and persisted in `config.toml` under `/<session-flags>/config.toml:<event>:<group>:<index>`. The key does not depend on cwd, so the same trust covers every worktree. Changing the command string retriggers `Hooks need review`, so hook command strings must stay fixed while per-session URL/token/thread ID pass via environment variables, which reach the hook process. Trust may need renewal after a Codex upgrade.
  - Without `--no-daemon`, the CLI attaches to a shared, long-lived `codex app-server` daemon, whose version can differ from the CLI's. Session MCP servers then run as children of that daemon, outside the PTY process tree, and survive `stop`. The adapter must launch Codex with `--no-daemon`.
  - `transcript_path` can be `null`.
- **`portable-pty`**: Pin `=0.8.1`; `0.9.0` enables `PSEUDOCONSOLE_INHERIT_CURSOR` on Windows ConPTY which stalls input until `ESC[6n]` is answered. On Windows, 0.8.1 fixes the ConPTY flags at `RESIZE_QUIRK | WIN32_INPUT_MODE`, loads a bare `conpty.dll` through the DLL search path, inverts the result of `kill` (returns `Err` on success), and offers no job list or suspended start, so its child can spawn descendants before a job is assigned (assign-after race escapees observed). Windows therefore uses an own `windows-sys` backend instead (next item); portable-pty stays on macOS/Unix.
- **Windows PTY backend and process tree**: an own `windows-sys` backend calls `CreatePseudoConsole` with flags `0` (inbox `kernel32.dll` only, never a `conpty.dll` from the search path) and `CreateProcessW` with `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` and `PROC_THREAD_ATTRIBUTE_JOB_LIST`, so the child is in a kill-on-close job from creation; `CREATE_SUSPENDED` + assign + resume is the fallback when the job list is rejected. Codex CLI 0.171.0 uses the same pair for its own PTYs (flags `0x2`, `utils/pty/src/win/psuedocon.rs`). On the spike's Windows host, this backend (flags `0` and `0x6`) and portable-pty passed the same checks on real sessions: Claude and Codex hooks, Claude pastes, attach with resize, window observation, and the Claude tree kill; Codex input was not complete under any of them. Every uninjected raw launch took the job-list branch (the forced fallback took `suspended`, as intended). The flags-`0` choice is a design decision taken after the spike's F-2 rule returned no selection (prior-art row 1d). In the Codex tree kill, a live `pwsh.exe` with `codex.exe` as parent, and its `conhost.exe`, were outside Chaldea's job before stop (`pwsh.exe` was in some other job) while `codex.exe` was inside; after job termination both were dead before any pid fallback, and no recorded descendant survived. Codex runs hook commands in a kill-on-close job of its own when it can, launches them uncontained when it cannot, and keeps successful hooks' descendants (`hooks/src/engine/command_runner.rs:286-346`, `utils/pty/src/win/job.rs:205`); so stop keeps the recorded-descendant kill alongside the job. The injected reader-start failure path showed no retained-handle growth: after a warm-up, ten normal and ten injected launches all settled at 89 handles (spike phase 3c, `adbefa2d`); the single +1 in phase 3b (88 → 89) stays unexplained.
- **Windows inbox conhost (ConPTY)**: answers DSR 5/6 and DA1/DA2 itself, swallows OSC 10/11 queries, and forwards the kitty keyboard query to the host. A reply conhost writes can land inside a bracketed paste body. It loses an escape sequence split across two host writes, so every sequence goes out in one write. It buffered a 64 KiB paste to a child that stopped reading without blocking the writer.
- **Claude Code on Windows** (2.1.326): bracketed paste over ConPTY delivers Japanese, astral, and multi-line prompts byte-equal, and combining sequences equal after Claude's own NFC composition.
- **Codex CLI on Windows** (0.169.0, 0.171.0): it clears `ENABLE_VIRTUAL_TERMINAL_INPUT` and reads Win32 key records, so bracketed paste never arrives as a paste (CR newlines submit each line, LF newlines are dropped). The adapter capability table selects keyed input for it, not `?2004h`:
  - Launch with `-c tui.disable_paste_burst=true`. With Codex's paste-burst detection on, fast key input can turn the submitting Return into a newline (`tui/src/bottom_pane/chat_composer/reconnect.rs`): a 1,259-character multi-line prompt got no `UserPromptSubmit` within the observation window in 3 of 3 controlled runs (phase 3c, `adbefa2d`) and 4 of 4 earlier runs (phase 3b, `5bda54b0`), and a 1-character prompt once in phase 3b. With it off, the paired controls (that prompt and the 1-character one, 3 runs each) were each submitted once with the exact text.
  - Text (planned encoding): pad the prompt with one leading and one trailing space (Codex trims both; this keeps a leading `/`, `!`, `?` and a trailing `@` token literal), compose combining marks with their base per grapheme cluster, then send ASCII as UTF-8 and every other BMP scalar as a win32-input-mode key record (`ESC[0;0;<utf16>;1;0;1_` + key-up); astral scalars stay UTF-8. Evidence (phase 3c, `adbefa2d`): at the console boundary (FakeCli `ReadConsoleInputW`), inbox conhost delivered U+200D, U+FE0F, U+2764, U+3099 and U+309A written as UTF-8 only on an Alt-key release, and each as a key-down when written as a record; the crossterm fork turns such a release into a key release, which Codex's composer ignores, and rebuilds astral characters from surrogate units (`parse.rs:46`, `:200`). In Codex itself, records for marks only recovered U+200D and U+309A, the heart fixture `❤️` submitted only U+FE0F because U+2764 went as UTF-8, and the tested kanji, kana, and astral samples arrived intact as UTF-8. Records for every non-ASCII BMP scalar have not run end to end in Codex; that is the encoding's first MVP test.
  - Newline: a win32-input-mode Shift+Enter record (`ESC[13;2u` written by the host loses the newline). Send only to a ready composer (after the previous turn's `Stop`). Submit: one Return after a delay (300 ms in the tests); a missing `UserPromptSubmit` leaves the input `unconfirmed`, never a second Return (an extra Return can accept a popup, run a slash command, or act on a queued draft). Every escape sequence goes out in one write.
  - Leading and trailing whitespace of the prompt itself is trimmed by Codex, and with paste-burst detection off a TAB submitted early (two submits for one prompt, phase 3c), so both are rejected unless explicitly allowed. Composer triggers apply to typed input: a leading `/`, `!`, or `?` and an `@` token at the cursor change how the input is handled, and Tab queues the message. Without `--no-daemon`, Codex on Windows ran `node.exe` / `node_repl.exe` inside the launched tree, and no separate warm server was found.
  - Outside-job PowerShell (phase 3c, `p3c-tree-kill-codex` `outside_job_identity`): a packaged PowerShell (`Microsoft.PowerShell_7.7.0.0_x64__8wekyb3d8bbwe`, image under `WindowsApps`) whose reported parent was `codex.exe`, started about 2.4 s after launch, was in another job and outside Chaldea's; no captured hook ancestry matched it, and it and its `conhost.exe` were dead before any pid fallback. How it came to be outside the job (e.g. packaged-app activation) is unverified.
- **Windows MSVC**: `link.exe` fails (`LNK1104`) on paths exceeding 260 characters; worktree base path must be configurable per workspace.

## 6. Verification Strategy

- **Real CLI contract tests (`fixtures/`)**: Versioned real transcripts and hook payloads (highest priority against upstream CLI drift).
- **PBT (`proptest` / `fast-check`)**: 3-axis state machine, input ack transitions, client reducer.
- **Targeted model checking (`stateright`)**: Scoped strictly to startup crash, offline event buffering, and retry after lost response on the Sync Seam.
- **Fuzzing (`cargo-fuzz`)**: JSONL line parser, hook payload decoder, VT escape responder.
- **Cross-OS CI + E2E**: macOS + Windows CI matrix (`core` + `FakePty`), `chaldea doctor`, Playwright E2E (desktop + mobile viewports, including IME composition guard tests).
