# Chaldea

PTY-launched AI coding agent client and multi-host control plane for macOS and native Windows hosts (localhost-only MVP; Android remote access is Phase 2).

**Status**: In progress. Implemented: `crates/core`, `crates/pty-runtime`, `crates/testkit`, `crates/daemon`, and `crates/cli` (daemon, supervisor with launch/stop/close/restart and crash recovery, send/ack path, and hook ingest), plus the Quint specs under `specs/`. Not yet implemented: the Claude and Codex adapters, the PWA (`packages/web`), the web terminal view, and orchestration base O1 (agent tokens, mailbox, `task set`, `spawn --worktree`). See `docs/mvp-design.md` §4 for the milestone order (C1 = M0.5, T1–T2 = M1, X1–X2 = M2, O1 = M3) and issue #2.

## Design Summary

Canonical design reference: `docs/architecture.md`.

- **Execution model**: Interactive CLI agents (Claude Code, Codex CLI, etc.) launched inside daemon-owned PTYs — no ACP lock-in, no PTY screen-scraping for state.
- **Core stack**: Rust Sans-IO workspace (`crates/core`, `crates/pty-runtime`, `crates/daemon`) + SQLite append-only event store + React/TypeScript single PWA (`packages/web`, whose web terminal view uses prebuilt libghostty-vt WASM via ghostty-web — no `xterm.js`, no Zig in the Rust build) + Phase 2 Cloudflare Workers & Durable Objects relay (`packages/relay`).
- **5 Seams**:
  1. **Sync Seam**: `stream_id + after_seq`, snapshot recovery, `command_id` idempotency, notify-then-pull WS live notifications.
  2. **Ingest & Event Store Seam**: SQLite append-only events (`schema_version`), 3-axis state (`process` / `turn` / `task`), generation+offset atomic transcript ingestion.
  3. **Agent Control & Adapter Seam**: `chaldea spawn / send / wait / stop` (and `mailbox`, planned in O1), input state machine (`queued -> sending -> sent -> acked / unconfirmed`), adapter capability table, worktree ownership and creation with short base path support for Windows MSVC (planned in O1).
  4. **Extension & Rescue Seam**: CLI hooks injected per launch (fixed command + env vars for Codex) delivered by `chaldea hook-emit`. Rescue is an external-agent surface (planned): an agent on the PC diagnoses through `GET /api/threads/{id}/screen` and `chaldea inspect <thread> [--json]` (C1), intervenes with the existing commands, and follows `skills/chaldea-rescue/SKILL.md` (T2). A built-in Rescue UI, A2UI (`surface_update` / `surface_action`), and one-shot JSON hooks (`before_spawn`, `after_close`, `attention`) are deferred.
  5. **PTY Seam**: `PtyRuntime` trait in `crates/pty-runtime` encapsulating own macOS POSIX PTY (rustix + `Command`/`pre_exec`) and own `windows-sys` ConPTY backends (child is in a kill-on-close job before it runs, via `PROC_THREAD_ATTRIBUTE_JOB_LIST` or a `CREATE_SUSPENDED` create, assign, and resume fallback), `alacritty_terminal` headless VT responder, process-tree cleanup (Job Object / `setsid`+pgid), and crash-only `--resume` recovery. Terminal access is the web terminal view in `packages/web` (prebuilt libghostty-vt WASM, ghostty-web first) over the attach WebSocket, never a state source; a native `chaldea attach` CLI is deferred.

## Design Assets

- Design documents: `docs/architecture.md`, `docs/mvp-design.md`, `docs/design/`
- Wireframes / Pencil file: `docs/design/chaldea.pen`

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
