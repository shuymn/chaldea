# Chaldea

PTY-launched AI coding agent client and multi-host control plane for macOS / Windows hosts and Android remote access.

## Design Summary

- **Execution model**: Interactive CLI agents (Claude Code, Codex CLI, etc.) launched inside daemon-owned PTYs — no ACP lock-in, no PTY screen-scraping for state.
- **Core stack**: Rust Sans-IO workspace (`crates/core`, `crates/pty-runtime`, `crates/daemon`) + SQLite append-only event store + React/TypeScript single PWA (`packages/web`) + Phase 2 Cloudflare Workers & Durable Objects relay (`packages/relay`).
- **5 Seams**:
  1. **Sync Seam**: `stream_id + after_seq`, snapshot recovery, `command_id` idempotency, WS live push.
  2. **Ingest & Event Store Seam**: SQLite append-only events (`schema_version`), 3-axis state (`process` / `turn` / `task`), generation+offset atomic transcript ingestion.
  3. **Agent Control & Adapter Seam**: `chaldea spawn / send / wait / stop / mailbox`, input state machine (`queued -> sent -> acked / unconfirmed`), adapter capability table, worktree ownership (short base path support for Windows MSVC).
  4. **Extension & Rescue Seam**: One-shot JSON hooks (fixed command + env injection), emergency VT keypad, dedicated Rescue Agent token + scoped A2UI (`Host | Workspace | Thread`).
  5. **PTY Seam**: `PtyRuntime` trait encapsulating macOS POSIX / Windows ConPTY (`portable-pty 0.8.1`), `alacritty_terminal` headless VT responder, process-tree cleanup (Job Object / pgid), crash-only `--resume` recovery, and native `chaldea attach`.

## Design Assets

- Wireframes / Pencil files: `docs/design/`
