# Design: PWA Base and Read Path (MVP W1)

W1 of `docs/mvp-design.md` §4 (mvp:437, :480, :607-608): PR 18, the PWA's read side, M1.

## 1. Scope

**Adds.** `packages/web` (React, TypeScript strict, Vite, Tailwind), the `packages/contracts` decoder, the `/` and `/assets/*` handler, pairing, the sync client, thread list, timeline, and the JS CI job.

**Leaves.** W2: composer, commands, attention banner. T2: terminal view, key bar, attach, VT snapshots. Deferred (mvp:446): blob download (P-21), surfaces. No new route, command, or event kind.

## 2. Contracts

### 2.1 Mount

| Item | Contract |
|---|---|
| Routes | `GET /` → `index.html`; `GET /assets/{*path}` → a built file, else 404 `not_found`; no credential (http:232). |
| `/` headers | `Content-Security-Policy: default-src 'self'; connect-src 'self' ws://<Host>; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'; require-trusted-types-for 'script'` with the validated `Host` (mvp:228, :234); `X-Frame-Options: DENY`; `Referrer-Policy: no-referrer`; `Cross-Origin-Opener-Policy: same-origin` (security guide §1.4, §3.2, §3.4). |
| All PWA responses | `nosniff`, `Cross-Origin-Resource-Policy: same-origin`, `Content-Type` by extension. |
| Caching (http:625) | `/` and 404s `no-store`; `/assets/*` (content-hashed names) `max-age=31536000, immutable`. |
| Embedding | `crates/daemon/build.rs` (std only) embeds the Vite build at `CHALDEA_WEB_DIST` (`index.html`, `assets/`). Unset: both routes stay unmounted, answering the existing 501 (http:219, mvp:251), logged at start. Only that variable and directory trigger rebuilds. |

### 2.2 Pairing and the Browser Credential

The page removes `#pair=<code>` with `history.replaceState` before the exchange (mvp:289), so reloads never resend a spent code. `POST /api/pair` (same-origin, so `Origin` matches) returns an `owner` token, stored in IndexedDB, replacing any earlier one. A 401 on any Bearer request deletes the token and every copy and asks for `chaldea open`, as after a new database (which holds the tokens).

**Assumption** (daemon-lifecycle:131-133, D13 :924): one OS account. The stable origin is not an authenticated server; another account holding the port could read the token. W1 trusts the origin no further: the token goes only into `Authorization` headers to its own origin (CSP `connect-src`), never into a URL, log, or other storage. Reopen on a host shared with other accounts.

### 2.3 Sync Client

A pure reducer `(State, Action) → (State, Effect[])` holds the epoch, copies, request generations, announced heads, and socket; a thin runner performs effects and returns results as actions; React reads it through `useSyncExternalStore`.

- **Followed streams** (the subscription set, http:475): `host`, the newest 64 open threads, the displayed thread. Other rows show the host value, marked not live.
- **Rules**: formal-specs §7.1's client table (:224-240) and §7.4's PR 18 obligations (:339) as written, with `heads` per http D15 (:898); under F1 a row's axes come from its thread's stream once followed. 408 and 503 retry with backoff and `retry_after`.
- **Requests**: the reducer decides currency by request id, never by abort; a timeout (`AbortSignal.any`, js guide §2) ends it. At most four in flight, each a fresh connection (http:205).
- **Socket**: a ticket of purpose `events`, then `/api/ws`; opening it invalidates earlier requests (mvp:205); reconnect backs off to 30 s.
- **Offline** (mvp:220): no socket open, or the latest request failed at the network. The page keeps its state (in memory only), exposes `offline` for W2's lock, and shows when it last processed a current forward response.
- **Memory**: undisplayed threads keep projection and cursor; the displayed one at most 2,000 events and 16 MiB, paging older history on demand.

### 2.4 Rendering

`packages/contracts` decodes as `TypedEvent::decode` (`crates/core/src/event.rs:216-229`): `schema_version` above the exported `SCHEMA_VERSION` → `newer_schema`; a kind outside the `TypedEvent` schema → `unknown_kind`; a failing payload → `payload_invalid`. Each renders as "unknown event" (kind, seq, JSON), never failing a page (mvp:58). A body failing its response schema is a failed read. Validators are Ajv standalone code generated from `packages/contracts/schema` at build time (CSP forbids `eval`).

Thread list: agent, cwd, axes, attention, closed. Timeline: events in `seq` order as React text nodes; blobs as preview and size; `vt_snapshot_taken` a marker; no screen text is shown or read. Layout is reversible (AGENTS.md "UI features"); touch targets follow the form-factor guide; announcements use one debounced status region, not the timeline (accessibility guide §8).

## 3. Decisions

| # | Decision | Rejected alternative |
|---|---|---|
| D1 | Compile-time embedding from `CHALDEA_WEB_DIST`; unset is unmounted, so plain `cargo build`, Windows CI, and the `core` check need no Node. | Disk serving (version skew, not a single binary); a required build (every `cargo build` needs pnpm); `rust-embed` (a new dependency). |
| D2 | A pure reducer, driven by fast-check against a TS model server. | TanStack Query or component state: cannot drop every copy atomically on an epoch switch. Replaying Q-B traces: they record `core` calls, not client steps (formal-specs:345). |
| D3 | Token in IndexedDB (mvp:289). | `HttpOnly` cookie (mvp:289); memory or `sessionStorage` (re-pair per reload or tab). |
| D4 | List liveness by following open threads, capped. | Rereading `GET /api/threads` on notifications: unfollowed subscriptions are outside Q-B. |
| D5 | Strict CSP with Trusted Types; T2 adds what ghostty-web needs (`'wasm-unsafe-eval'`). | `'unsafe-eval'` for runtime Ajv; a report endpoint (a new route). |
| D6 | No manifest or service worker: Android is Phase 2, P-27 deferred, a cached shell outlives daemon upgrades; adding them under `/assets/` is additive (install guide). | Installable now. |
| D7 | A root pnpm workspace (`contracts`, `web`; `specs/` separate), `mise.toml` tools, exact pins. Job `web` on `macos-latest`: typecheck, Vitest with fast-check, Vite build, `cargo build` with `CHALDEA_WEB_DIST`, Playwright (Chromium, WebKit; desktop, phone) against the real `chaldea` with FakeCli; lefthook mirrors typecheck and Vitest. | A fake server: it skips the real Host, Origin, CSP, and connections. Windows: X2's smoke. |
| D8 | Plain-text payloads. | Markdown: an HTML sink (security guide §1.2). |

All reversible except D3 (mvp:289).

## 4. Acceptance Criteria

**Purpose layer** (owner, real browser, macOS, M1).

| ID | Criterion (EARS) |
|---|---|
| W1-1 | When the owner runs `chaldea open`, the browser shall show the thread list with no fragment in the address bar; reusing the URL shall ask to pair again (PR8-P-4). |
| W1-2 | While a Claude thread runs, its row and timeline shall follow its events without reload. |
| W1-3 | When the daemon stops, the page shall keep its content and show offline with the last sync time; after a restart it shall catch up with no duplicate or missing row. |
| W1-4 | On a phone viewport the list and timeline shall be readable (owner evaluation). |

**Plan layer.**

| ID | Claim (EARS) | Test |
|---|---|---|
| W1-5 | When the network quiesces after any interleaving of losses, delays, restarts, database swaps, and notifications, copies shall equal the server's, without duplicates (CL-13). | Vitest `sync.converges` (fast-check) |
| W1-6 | An older-generation response, a foreign-epoch backward page, or a failed read shall never set the epoch or change a copy. | `sync.olderChangesNothing`, `sync.epochOnlyFromForward`, `sync.backwardResync` |
| W1-7 | A 404 shall end following; `has_more` shall continue; a reconnect shall refetch every followed stream and a pending history page; a notification during a fetch shall stay due. | `sync.vanished`, `sync.paging`, `sync.reconnect`, `sync.notifiedDuringFetch` |
| W1-8 | Copies shall stay within §2.3's bounds. | `sync.memoryBound` |
| W1-9 | Unknown kind, newer schema, and invalid payload shall render as "unknown event", agreeing with Rust on one corpus (CL-14). | Vitest `decode.corpus`; core `generic_corpus`; Playwright `unknown_event_row` |
| W1-10 | The PWA routes shall carry §2.1's headers with no CSP violation; other routes keep `no-store`; an unembedded build answers 501. | daemon `pwa_headers`, `pwa_unknown_asset`, `pwa_absent_is_501`; Playwright `csp_clean` |
| W1-11 | The fragment shall be gone before the exchange; the token shall appear only in own-origin headers; a 401 shall clear it. | Playwright `pair_once`, `token_only_in_headers`, `revoked_repairs` |
| W1-12 | A daemon stop and restart shall show offline, then catch up. | Playwright `offline_then_catch_up` |

## 5. Risks

- **Screen text as state** (invariant 1): no `Action` carries terminal or screen data; W1 never calls `/screen` or attach.
- **Token leakage**: closed by §2.2 and CSP; no third-party script, React escapes text, Trusted Types guards sinks. Accepted: Safari may evict IndexedDB after seven idle days; one `chaldea open` re-pairs.
- **Stale epoch, duplicates**: closed by W1-5 to W1-7. A database swap also invalidates the token, so real browsers re-pair first; only fast-check exercises epoch switches.
- **Client memory**: closed by §2.3 and W1-8. Accepted: a first follow may transfer up to 8 MiB (mvp:205) per stream, at most 66; M1 runs few threads, so only load time suffers.
- **Rust–JS coupling**: closed by D1. Accepted: a build without `CHALDEA_WEB_DIST` answers 501 at `/`, logged.

## 6. Silences and Proposed Canonical Edits

- mvp §2.5 (:234): §2.1's CSP (`ws://<Host>` explicit) and caching.
- mvp §3 (:375, :409): build-time validators, an exported `SCHEMA_VERSION`, D7's job, `CHALDEA_WEB_DIST`.
- mvp §2.4 "Offline" (:220), silent on persistence: in memory, no service worker.
- daemon-lifecycle :614: "a binary built without the PWA answers 501 at `/`".
- formal-specs §7.4: an MVP database swap reaches the browser as 401 first.
