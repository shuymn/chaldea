# Design: PWA Base and Read Path (MVP W1)

W1 of `docs/mvp-design.md` §4 (mvp:437, :480, :607-608): PR 18, M1.

## 1. Scope

**Adds** `packages/web` (React, Vite, Tailwind), the decoder, the PWA handler, pairing, sync, list, timeline, JS CI, a `chaldea open` probe. **Leaves** commands to W2, the terminal to T2, blobs and surfaces to Deferred (mvp:446). No new route, command, or event kind.

## 2. Contracts

### 2.1 Mount and Build

- **Routes** (http:232): `GET /` → `index.html`; `GET /assets/{*path}` → an exact embedded file of an allowlisted type, else 404. All carry `nosniff` and `Cross-Origin-Resource-Policy: same-origin`.
- **`/` headers**: `Content-Security-Policy: default-src 'self'; connect-src 'self' ws://<host>; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'; require-trusted-types-for 'script'; trusted-types 'none'` (`<host>`: the validated `Host`, lowercased; mvp:228); `X-Frame-Options: DENY`; `Referrer-Policy: no-referrer`; `Cross-Origin-Opener-Policy: same-origin`.
- **Caching** (http:625): errors and `/` `no-store`; found assets (content-hashed) `max-age=31536000, immutable`.
- **Embedding**: the daemon's std-only `build.rs` regenerates the asset table from `CHALDEA_WEB_DIST` (absolute or workspace-relative), tracking it and every file; an invalid set value fails the build. `Mounts::mvp()` keeps both routes `Mount::unimplemented` (501, http:219); the production components replace them with `Mount::read` over a non-empty table, and the handler takes the table as a parameter.
- **Release**: owner binaries on both hosts come from one documented command (Vite build, then `cargo build --release` with the variable); a smoke of the packaged binary fetches `/` and an asset. `chaldea open` probes `/`; on 501 it reports the missing PWA and exits 1.

### 2.2 Pairing and Credential

The page removes `#pair=<code>` with `history.replaceState` before the exchange (mvp:289). Requests use fixed relative paths, Fetch mode `cors` (POSTs under `no-referrer` then send the real `Origin`), and `redirect: "error"`. The token travels only in `Authorization`, the events ticket is the only secret in a URL, and diagnostics and kept traces omit URLs, bodies, and headers.

IndexedDB holds `{ generation, token }`. A 401 deletes it only when the stored generation is the request's (one compare-and-delete transaction), ends that generation's work, and tells other tabs. A failed or ambiguous pairing keeps a valid credential; with none, the page asks for a fresh link.

**Assumption** (daemon-lifecycle:131-133): one OS account; the origin is not an authenticated server and is trusted no further. Reopen on a shared host.

### 2.3 Sync Client

A pure reducer `(State, Action) → (State, Effect[])` holds all sync state; a runner does effects.

- **Followed set** (= subscriptions, http:475): `host`, the selected thread, then up to 64 open threads, visible rows first, in list order (open, `cwd`, `thread_id`). Unfollowing drops the copy and makes its requests older.
- **F1 both ways** (formal-specs:326): the host copy owns membership and `closed`; a valid thread copy owns axes and attention; initial `task_state` comes from registration. Unfollowed rows show "last known" host values, or "unavailable".
- **Rules**: formal-specs §7.1 and §7.4 (:224-240, :339). A current 404 removes copy, selection, and row for the epoch.
- **Socket**: every `open`, the first included, makes earlier requests older, resets announced heads, subscribes the full set, and makes streams and pending history due; callbacks bind to that socket. `hello` sets the socket's epoch only; `heads` and `stream_advanced` count in the held epoch, merged by max for followed ids, omissions ignored (http D15). Only a current successful forward response sets the epoch; a backward resync waits for a later forward issue.
- **Scheduling**: host, displayed thread, others; four reads at most, the ticket outside; 10 s timeout (daemon 5 s).
- **Failures**: forward 408, 503, and network errors stay due, backing off to 30 s (honouring `retry_after`). Backward failures, other statuses, schema failures, and bodies over 16 MiB (refused unparsed) keep the last good copy and show a retry, never looping.
- **Status** (mvp:220): `connecting`, `live`, `updates paused` (socket down), `offline` (reads fail); "last synced" is when every followed stream was last at its announced head. State is in memory.

### 2.4 History Window

The displayed thread has one contiguous window (2,000 events, 16 MiB serialized), loaded by backward reads, leaving the forward cursor Q-B's. Selection loads the newest page (`before_seq = cursor + 1`). While attached (ending at the cursor) live events append and the oldest drop past the cap. "Older" reads before its first seq; past the cap the newest drop, detaching it: live events move only the cursor, "newer" reads `before_seq = last + limit + 1`, "Jump to latest" reloads the tail. A same-epoch snapshot resets an attached window; an epoch switch drops it. Other threads keep list fields only.

### 2.5 Rendering

The decoder mirrors `TypedEvent::decode` (`crates/core/src/event.rs:216-229`): `schema_version` above the Rust-exported `SCHEMA_VERSION` → `newer_schema`; unknown kind → `unknown_kind`; failing payload → `payload_invalid`; each renders as "unknown event" without failing a page (mvp:58). Validators are Ajv 2020 standalone code (CSP forbids `eval`) with explicit `uuid`, `date-time`, and integer formats.

The list shows agent, cwd, axes, attention, closed; the timeline shows events in `seq` order as plain text (no Markdown), blobs as preview and size, never screen text.

## 3. Decisions

| # | Decision | Rejected |
|---|---|---|
| D1 | Compile-time embedding; Rust-only builds and the `core` check need no Node; releases require it. | Disk serving (skew); a mandatory web build; `rust-embed`. |
| D2 | A pure reducer: explicit transitions, drivable by fast-check. | Hook or query-cache state; Q-B trace replay (records `core` calls, formal-specs:345). |
| D3 | IndexedDB with generations (mvp:289); reversible by re-pairing (daemon-lifecycle:133). | `HttpOnly` cookie; memory or `sessionStorage`. |
| D4 | Capped, visible-first following. | Rereading `GET /api/threads`: outside Q-B. |
| D5 | One detachable window over backward reads. | Unbounded copies; a second forward cursor. |
| D6 | Strict CSP, no Trusted Types policy, no inlined `data:` assets; T2 extends it. | `'unsafe-eval'`; a report route. |
| D7 | No manifest or service worker: Android is Phase 2; a cached shell outlives upgrades. | Installable now. |
| D8 | Root pnpm workspace, exact pins; job `web` (macOS): typecheck, Vitest, Playwright (Chromium, WebKit) on the production `chaldea`, FakeCli behind a `PATH` `claude` wrapper; release smoke on both hosts. | A fake server: no real Host, Origin, CSP. |

All reversible.

## 4. Acceptance Criteria

**Purpose layer** (owner, real browser).

| ID | Criterion (EARS) |
|---|---|
| W1-1 | When the owner runs `chaldea open`, browser cold and warm, the list shall appear with no fragment left; a spent link in a fresh profile shall ask for a fresh link (PR8-P-4). |
| W1-2 | While a Claude thread runs, its row and timeline shall follow it live. |
| W1-3 | When the daemon stops, the page shall keep its content and show offline with the last sync time, then catch up after restart without duplicate or missing rows. |
| W1-4 | At 390 × 844 CSS px, list and timeline shall show every field and control without horizontal scrolling. |

**Plan layer** (each test fails on revert).

| ID | Claim (EARS) | Test |
|---|---|---|
| W1-5 | When the network quiesces after any losses, delays, restarts, and epoch changes, the client shall hold the server's epoch, cursors, owned projections, and the window's exact ordered events (CL-13). | Vitest `sync.converges` (independent log) |
| W1-6 | Stale responses, foreign-epoch backward pages, and failed reads shall change no epoch or copy. | `sync.generations` |
| W1-7 | Paging, `heads` (stale ones included), F1, 404s, reconnects, and failures shall follow §2.3. | `sync.paging`, `sync.heads`, `sync.f1`, `sync.vanished`, `sync.reconnect`, `sync.failures` |
| W1-8 | The window shall follow §2.4 within its caps, all history reachable. | `sync.window` |
| W1-9 | The decoder shall match Rust's golden cases, reasons, precedence, and additive fields; a bad outer body shall keep the copy. | Vitest `decode.golden`; drift check with `SCHEMA_VERSION`; Playwright `generic_rows` |
| W1-10 | Both builds shall serve §2.1's headers, bytes, types, and 501. | daemon `pwa_mount`; release smoke |
| W1-11 | Pairing and requests shall follow §2.2, a 401 touching only its generation. | Playwright `pair_once`, `request_policy`, `new_database_repairs`; Vitest `credential.generation` |
| W1-12 | The built app shall raise no CSP violation in either engine; an injected sink shall be blocked. | Playwright `csp_clean`, `csp_enforced` |
| W1-13 | A unique event shall survive a daemon restart, offline shown meanwhile. | Playwright `offline_then_catch_up` |

## 5. Risks and Limits

- **Screen as state** (invariant 1): no action carries screen data; W1 never uses `/screen` or attach.
- **Limit, one OS account**: same-origin script could still read the token; the MVP trusts that account.
- **Epochs**: an empty database answers 401; a restored one may keep tokens and epoch, so a divergent same-epoch restore shows only through `CursorAhead`.
- **Limits**: past 64 open threads rows show "last known" until selected (M1 runs few); the window keeps all history one action away; evicted storage costs one `chaldea open`.

## 6. Proposed Canonical Edits

mvp :234 (§2.1 headers), :375 and :409 (validators, `SCHEMA_VERSION`, D8, release command), :220 (statuses); daemon-lifecycle :614 and PR8-CL-24 (the probe); formal-specs §7.4 (an empty database reaches the browser as 401).
