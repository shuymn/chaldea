# Chaldea

PTY-launched AI coding agent client and multi-host control plane for macOS / native Windows hosts and Android remote access.

Canonical design reference: `docs/architecture.md`.

## Core Invariants

1. **No ACP lock-in, no PTY screen-scraping for state**:
   - Launch CLI agents (Claude Code, Codex CLI, etc.) as normal interactive terminal programs inside daemon-owned PTYs.
   - Thread state transitions ONLY via explicit events (Hook HTTP push, transcript ingest, process exit) — never by scraping terminal text.
2. **Negative Space (Do NOT put in core)**:
   - The one terminal emulator outside the daemon is the web terminal view in `packages/web`: prebuilt libghostty-vt WASM (ghostty-web first), no `xterm.js`, no Zig in the Rust build. It renders the PTY and forwards human input over the attach WebSocket; it is never a state source (invariant 1). A native `chaldea attach` CLI is deferred (`docs/mvp-design.md` §4 "Deferred").
   - No hardcoded orchestration strategy (judgment lives in agent skills/instructions, not daemon code).
   - No per-thread long-lived external adapter processes (the extension model is one-shot JSON hooks + authenticated local HTTP push, deferred past the MVP).
3. **5 Seams & Boundaries**:
   - **Sync Seam**: Local SQLite is the sole source of truth. Sync via `stream_id + after_seq` (snapshot fallback) and idempotent `command_id`. Phase 2 Cloudflare Workers DO is strictly a relay/gateway/read-cache.
   - **Ingest & Event Store Seam**: Append-only events with `schema_version` + `idempotency_key`; separate 3-axis state (`process_state`, `turn_state`, `task_state`); atomic generation + `(path, offset)` transcript cursor; a hook payload or transcript line that cannot be parsed or bound is kept verbatim as `raw_unknown`, never dropped.
   - **Agent Control & Adapter Seam**: `chaldea spawn / send / wait / stop / mailbox`; input state machine (`queued -> sending -> sent -> acked / unconfirmed`, plus terminal `rejected`, `cancelled`, `dismissed`; never auto-resend `unconfirmed`; invalid input is refused with zero bytes written); a resume is refused until the old launch's death is confirmed; adapter capability table; worktree ownership (never auto-delete dirty/unpublished checkouts; support short worktree base paths for Windows MSVC 260-char limits).
   - **Extension & Rescue Seam**: CLI hooks injected per launch (fixed command + env vars for Codex to avoid repeated trust review prompts); Rust single-binary hook emitter (`chaldea hook-emit`). Rescue is an external-agent surface: an agent on the PC (e.g. Claude or Codex desktop apps) diagnoses through `GET /api/threads/{id}/screen` and `chaldea inspect <thread> [--json]`, intervenes with the existing commands, and follows `skills/chaldea-rescue/SKILL.md`. A built-in Rescue UI, A2UI (`surface_update` / `surface_action`), and one-shot JSON hooks (`before_spawn`, `after_close`, `attention`) are deferred (`docs/mvp-design.md` §4 "Deferred"); deferred features have no stubs or commands.
   - **PTY Seam**: Encapsulate all OS differences (macOS POSIX PTY / Windows ConPTY) inside `crates/pty-runtime` behind `PtyRuntime` trait, with own backends (macOS: rustix PTY + `std::process::Command`/`pre_exec`; Windows: `windows-sys` ConPTY, the child in a kill-on-close job from creation), one shared spawn lock for all process creation (the PTY backends take it internally; every other daemon spawn goes through `with_spawn_lock`), `alacritty_terminal` headless VT responder, process-tree cleanup (Job Object / `setsid`+pgid), and crash-only `--resume` recovery.
4. **Japanese IME is Mandatory**:
   - Use a plain `<textarea>` composer (never `contenteditable`, never the terminal view) for prompt inputs; guard `isComposing`, `keyCode === 229`, and post-`compositionend` Enter.
   - Deliver prompts intact, Japanese and emoji included: the prompt the CLI records equals the adapter's documented expected form of the prompt sent (e.g. composed marks, LF newlines). Each adapter picks its input encoding: Bracketed Paste only when the app has enabled DECSET 2004, keyed input for Codex on Windows (it reads Win32 key records). Handle wide-char widths in `alacritty_terminal` snapshots.
5. **Security**:
   - Localhost HTTP endpoints require Bearer token + `Host` header validation.
   - Every token is scoped by kind (`docs/mvp-design.md` §2.5); terminal writes (raw keys, the attach WebSocket) are owner-only. Phase 2 separates browser CF Access auth from host Service Tokens.

## Platform & Development Policy

- **Host platforms**: macOS (primary development mainline) and native Windows (required — keep OS-specific code isolated in `crates/pty-runtime` and keep `core` + `FakePty` passing on Windows CI).
- **Remote platform**: Android (read & operate only via single PWA; never a host).
- **UI features**: Specific screen features (e.g., diff view, desktop split panes, visual theme details) remain reversible until validated by real usage; `docs/design/chaldea.pen` holds wireframes only.
