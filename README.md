# Chaldea

PTY-launched AI coding agent client and multi-host control plane for macOS / Windows hosts and Android remote access.

**Status**: In progress. The workspace currently contains `crates/core` (contracts), `crates/pty-runtime` (PTY runtime with own macOS and Windows ConPTY backends), and `crates/testkit`; the daemon, CLI, and PWA are not implemented yet. See `docs/mvp-design.md` and issue #2 for the plan and remaining work.

## Design Summary

Canonical design reference: `docs/architecture.md`.

- **Execution model**: Interactive CLI agents (Claude Code, Codex CLI, etc.) launched inside daemon-owned PTYs — no ACP lock-in, no PTY screen-scraping for state.
- **Core stack**: Rust Sans-IO workspace (`crates/core`, `crates/pty-runtime`, `crates/daemon`) + SQLite append-only event store + React/TypeScript single PWA (`packages/web`) + Phase 2 Cloudflare Workers & Durable Objects relay (`packages/relay`).
- **5 Seams**:
  1. **Sync Seam**: `stream_id + after_seq`, snapshot recovery, `command_id` idempotency, notify-then-pull WS live notifications.
  2. **Ingest & Event Store Seam**: SQLite append-only events (`schema_version`), 3-axis state (`process` / `turn` / `task`), generation+offset atomic transcript ingestion.
  3. **Agent Control & Adapter Seam**: `chaldea spawn / send / wait / stop / mailbox`, input state machine (`queued -> sending -> sent -> acked / unconfirmed`), adapter capability table, worktree ownership (short base path support for Windows MSVC).
  4. **Extension & Rescue Seam**: CLI hooks injected per launch (fixed command + env injection); external agents on the PC diagnose and intervene through `GET /api/threads/{id}/screen`, `chaldea inspect`, the existing commands, and `skills/chaldea-rescue/SKILL.md`. One-shot JSON hooks and a built-in Rescue UI with A2UI are deferred.
  5. **PTY Seam**: `PtyRuntime` trait in `crates/pty-runtime` encapsulating own macOS POSIX PTY (rustix + `Command`/`pre_exec`) and own `windows-sys` ConPTY backends (child joins a kill-on-close job at creation via `PROC_THREAD_ATTRIBUTE_JOB_LIST`), `alacritty_terminal` headless VT responder, process-tree cleanup (Job Object / `setsid`+pgid), crash-only `--resume` recovery, and an attach WebSocket feeding the PWA's web terminal view (ghostty-web).

## Design Assets

- Design documents: `docs/architecture.md`, `docs/mvp-design.md`, `docs/design/`
- Wireframes / Pencil file: `docs/design/chaldea.pen`

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
