# Chaldea

PTY-launched AI coding agent client and multi-host control plane for macOS / native Windows hosts and Android remote access.

Canonical design reference: `docs/architecture.md`.

## Core Invariants

1. **No ACP lock-in, no PTY screen-scraping for state**:
   - Launch CLI agents (Claude Code, Codex CLI, etc.) as normal interactive terminal programs inside daemon-owned PTYs.
   - Thread state transitions ONLY via explicit events (Hook HTTP push, transcript ingest, process exit) — never by scraping terminal text.
2. **Negative Space (Do NOT put in core)**:
   - No client-side terminal emulator (`xterm.js` / `libghostty`); native PC terminal access delegates to `chaldea attach`.
   - No hardcoded orchestration strategy (judgment lives in agent skills/instructions, not daemon code).
   - No per-thread long-lived external adapter processes (extensions are One-shot JSON Hooks + authenticated local HTTP push).
3. **5 Seams & Boundaries**:
   - **Sync Seam**: Local SQLite is the sole source of truth. Sync via `stream_id + after_seq` (snapshot fallback) and idempotent `command_id`. Phase 2 Cloudflare Workers DO is strictly a relay/gateway/read-cache.
   - **Ingest & Event Store Seam**: Append-only events with `schema_version` + `idempotency_key`; separate 3-axis state (`process_state`, `turn_state`, `task_state`); atomic generation + `(path, offset)` transcript cursor.
   - **Agent Control & Adapter Seam**: `chaldea spawn / send / wait / stop / mailbox`; input state machine (`queued -> sent -> acked / unconfirmed`, never auto-resend `unconfirmed`); adapter capability table; worktree ownership (never auto-delete dirty/unpublished checkouts; support short worktree base paths for Windows MSVC 260-char limits).
   - **Extension & Rescue Seam**: One-shot JSON hooks (`before_spawn`, `after_close`, `attention`); CLI hooks injected per launch (fixed command + env vars for Codex to avoid repeated trust review prompts); Rust single-binary hook emitter (`chaldea hook-emit`); dedicated Rescue token + scoped A2UI (`Host | Workspace | Thread`).
   - **PTY Seam**: Encapsulate all OS differences (macOS POSIX PTY / Windows ConPTY) inside `crates/pty-runtime` behind `PtyRuntime` trait, using `portable-pty =0.8.1` on macOS/Unix and an own `windows-sys` ConPTY backend on Windows (child joins the kill-on-close job at creation via `PROC_THREAD_ATTRIBUTE_JOB_LIST`), `alacritty_terminal` headless VT responder, process-tree cleanup (Job Object / `setsid`+pgid), and crash-only `--resume` recovery.
4. **Japanese IME is Mandatory**:
   - Use plain `<textarea>` (never `contenteditable`) for prompt inputs; guard `isComposing`, `keyCode === 229`, and post-`compositionend` Enter.
   - Deliver prompts intact (the prompt the CLI records equals the prompt sent, Japanese and emoji included) through each adapter's input encoding: Bracketed Paste for CLIs that read VT input, keyed input for Codex on Windows (it reads Win32 key records); handle wide-char widths in `alacritty_terminal` snapshots.
5. **Security**:
   - Localhost HTTP endpoints require Bearer token + `Host` header validation.
   - Separate regular session tokens from Rescue tokens, and browser CF Access auth from host Service Tokens.

## Platform & Development Policy

- **Host platforms**: macOS (primary development mainline) and native Windows (required — keep OS-specific code isolated in `crates/pty-runtime` and keep `core` + `FakePty` passing on Windows CI).
- **Remote platform**: Android (read & operate only via single PWA; never a host).
- **UI features**: Specific screen features (e.g., diff view, desktop split panes, visual theme details) remain reversible until validated by real usage; `docs/design/chaldea.pen` holds wireframes only.
