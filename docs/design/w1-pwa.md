# Design: PWA Base and Read Path (MVP W1)

`docs/mvp-design.md` §4 row W1 (mvp:437, :480, :607-608): PR 18, M1.

## 1. Scope

**Adds** `packages/web` (React, Vite, Tailwind), decoder, PWA handler, pairing, sync, list, timeline, JS CI, a `chaldea open` check. **Leaves** commands to W2, the terminal to T2, blobs and surfaces to Deferred (mvp:446).

## 2. Contracts

### 2.1 Mount and Build

- **Routes** (http:232): `GET /` → `index.html`; `GET /assets/{*path}` → an exact embedded file of an allowlisted type, else 404. This handler's responses carry `nosniff` and `Cross-Origin-Resource-Policy: same-origin`.
- **`/` headers**: `Content-Security-Policy: default-src 'self'; connect-src 'self' ws://<host>; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'; require-trusted-types-for 'script'; trusted-types 'none'` (`<host>`: validated `Host`, lowercased; mvp:228); `X-Frame-Options: DENY`; `Referrer-Policy: no-referrer`; `Cross-Origin-Opener-Policy: same-origin`.
- **Caching** (http:625): errors and `/` `no-store`; hashed assets `max-age=31536000, immutable`.
- **Embedding**: the daemon's std-only `build.rs` regenerates the asset table from `CHALDEA_WEB_DIST` (absolute or workspace-relative), tracking it and every file; an invalid value fails the build. `Mounts::mvp()` keeps both routes `Mount::unimplemented` (501, http:219); production swaps in `Mount::read` over a non-empty table passed to the handler.
- **Release**: owner binaries on both hosts come from one documented command (Vite build, then `cargo build --release` with the variable); on PRs touching the web, `build.rs`, or the command, each packaged binary is smoked for `/` and the scripts and styles it names. Interactive `chaldea open` first checks `GET /` (no Bearer): 501 reports the missing PWA and exits 1 before minting; other failures keep their path; `--print-url` skips it.

### 2.2 Pairing and Credential

The page removes `#pair=<code>` with `history.replaceState` before the exchange (mvp:289). Requests use fixed relative paths, Fetch mode `cors` (POSTs under `no-referrer` then send the real `Origin`), and `redirect: "error"`. The token travels only in `Authorization`; the events ticket is the only secret in a query string; diagnostics omit URLs, bodies, and headers.

IndexedDB holds `{ generation (UUID), token }`. A 401 deletes it only when the stored generation is the request's (one compare-and-delete transaction); a losing 401 adopts the newer generation silently. After a committed replacement or deletion, other tabs reread and retire the old generation's responses and socket. A failed or ambiguous pairing keeps a valid credential; with none, the page asks for a fresh link.

**Assumption** (daemon-lifecycle:131-133): one OS account; the origin is not an authenticated server. Reopen on a shared host.

### 2.3 Sync Client

A pure reducer `(State, Action) → (State, Effect[])` holds sync state.

- **Followed set** (= subscriptions, http:475): `host`, the selected thread, up to 64 open threads, visible first in list order (open, `cwd`, `thread_id`), recomputed when scrolling settles, evicting hidden ones only for newly visible ones. Unfollowing drops the copy, aging its requests.
- **F1 both ways** (formal-specs:326): host owns membership and `closed`; a valid thread copy owns axes and attention; initial `task_state` comes from registration. Unfollowed rows label host values "last known" or "unavailable".
- **Rules**: formal-specs §7.1, §7.4 (:224-240, :339). A thread 404 removes copy, selection, and row for the epoch; a `host` 404 is an error.
- **Socket**: every `open` makes earlier requests older, resets announced heads, subscribes the full set, and makes streams and pending history due; callbacks bind to it. `hello` sets the socket's epoch only; heads count in the held epoch, merged by max for followed ids, omissions ignored (http D15). Only a current successful forward response sets the epoch; a backward resync waits for a later forward issue.
- **Scheduling**: host, displayed, others; four reads at most, ticket outside; 10 s timeout (daemon 5 s).
- **Failures**: forward 408, 503, and network errors stay due, backing off to 30 s (honouring `retry_after`). Bodies over 16 MiB are refused unparsed: pages retry smaller; a snapshot or event that large shows an error naming the limit. Other failures keep the last good copy and show a retry, never looping.
- **Status** (mvp:220): `connecting`, `live`, `updates paused` (socket down, reads work), `offline` (reads or ticket POST fail in transport); W2 locks input unless `live`. "Last synced" is set when a current forward read completes catch-up of every followed stream not in error (valid copy, cursor ≥ known head), and kept through outages. State is in memory.

### 2.4 History Window

The displayed thread has one contiguous window (2,000 events, 16 MiB) at or below the cursor, filled by backward reads; selection loads the newest page once a forward response set the cursor. Attached (ending at the cursor), live events append and the oldest drop past the cap. "Older" reads before the first retained seq; past the cap the newest drop, detaching it from live events. "Newer" reads `n = min(limit, cursor − last)` events (`before_seq = last + n + 1`), merged by seq, reattaching at the current cursor; "Jump to latest" reloads the tail. A bound snapshot resets an attached window to the new tail and keeps a detached one; `CursorAhead` discards and reloads it, aging its history requests; an epoch switch drops it.

### 2.5 Rendering

The decoder mirrors `TypedEvent::decode` (`event.rs:216-229`) against the Rust-exported `SCHEMA_VERSION`; each generic case renders as "unknown event" without failing a page (mvp:58). Validators are Ajv 2020 standalone code (no `eval`) with explicit formats.

The list shows agent, cwd, axes, attention, closed; the timeline, events in `seq` order as plain text, blobs as preview, never screen text.

## 3. Decisions

| # | Decision | Rejected |
|---|---|---|
| D1 | Compile-time embedding; Rust-only builds need no Node; releases require it. | Disk serving (skew); a mandatory web build; `rust-embed`. |
| D2 | A pure reducer, drivable by fast-check. | Hook state; Q-B trace replay (records `core` calls). |
| D3 | IndexedDB with generations (mvp:289); re-pairing reverses it. | `HttpOnly` cookie; `sessionStorage`. |
| D4 | Capped, visible-first following. | Rereading `GET /api/threads`: outside Q-B. |
| D5 | A detachable window over backward reads. | Unbounded copies; a second cursor. |
| D6 | Strict CSP, no Trusted Types policy or `data:` assets. | `'unsafe-eval'`; a report route. |
| D7 | No service worker: Android is Phase 2; a cached shell outlives upgrades. | Installable now. |
| D8 | pnpm workspace, exact pins; job `web` (macOS): typecheck, Vitest, Playwright (Chromium, WebKit) on the production `chaldea`, FakeCli as `claude`. | A fake server (no real Host, Origin, CSP). |

All reversible.

## 4. Acceptance Criteria

**Purpose layer** (owner, real browser).

| ID | Criterion (EARS) |
|---|---|
| W1-1 | When the owner runs `chaldea open` (browser cold and warm), the list shall appear with no fragment left; a spent link in a fresh profile shall ask for a fresh link (PR8-P-4). |
| W1-2 | While a Claude thread runs, its followed row and attached timeline shall update live. |
| W1-3 | When the daemon stops, the page shall keep its content, show offline and the last sync time, and catch up after restart without duplicate or missing rows. |
| W1-4 | At 390 × 844 CSS px, list and timeline shall show every field and control without horizontal scrolling. |

**Plan layer** (each test fails on revert).

| ID | Claim (EARS) | Test |
|---|---|---|
| W1-5 | When the network quiesces after any losses, delays, restarts, and epoch changes, the client shall hold the server's epoch, cursors, owned projections, and exact window, within §5's oversize limit (CL-13). | Vitest `sync.converges` |
| W1-6 | Stale responses and failed reads shall change no epoch or copy; the rest of §2.3 shall hold. | `sync.generations`, `paging`, `heads`, `f1`, `vanished`, `reconnect`, `failures`, `status` |
| W1-7 | The window shall follow §2.4, all history reachable within §5's limit. | `sync.window` |
| W1-8 | The decoder shall match Rust's golden cases; bad outer bodies keep the copy. | Vitest `decode.golden`; drift check; Playwright `generic_rows` |
| W1-9 | Builds and `chaldea open` shall follow §2.1. | daemon `pwa_mount`; `open_without_pwa`; release smoke |
| W1-10 | Pairing, requests, and tabs shall follow §2.2. | Playwright `pair_once`, `request_policy`, `new_database_repairs`, `two_tabs`; Vitest `credential.generation` |
| W1-11 | The built app shall raise no CSP violation; an injected sink shall be blocked. | Playwright `csp_clean`, `csp_enforced` |
| W1-12 | After a restart, an event absent before the stop shall appear, offline shown meanwhile. | Playwright `offline_then_catch_up` |

## 5. Risks and Limits

- **Screen as state** (invariant 1): no action carries screen data; W1 never uses `/screen` or attach.
- **Limit, one OS account**: same-origin script could read the token; the MVP trusts the account.
- **Epochs**: a restored database may keep tokens and epoch, detectable only through `CursorAhead`.
- **Limits**: unfollowed rows show "last known" until visible or selected; history within the read limit stays one action away; evicted storage costs one `chaldea open`; a snapshot passes 16 MiB only with many unresolved inputs (e.g. about 250 at the 64 KiB inline limit, `payload.rs:18`).

## 6. Proposed Canonical Edits

mvp :234, :375, :409, :220 (headers, validators, D8, release, statuses); daemon-lifecycle :614 (the interactive check); formal-specs §7.4 (an empty database reaches browsers as 401).
