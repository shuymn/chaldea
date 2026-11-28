# Design: Web Terminal View and the Rescue Skill (MVP T2)

mvp §4 row T2 (mvp:439, :480, :631): M1, after T1 (`docs/design/t1-attach.md`) and W2 (`docs/design/w2-composer.md`). `kastty:` cites `shuymn/kastty@f7578f3`; `gw:` cites the published `ghostty-web@0.4.0` (`dist/ghostty-web.js` unless named).

## 1. Scope

**Adds** to `packages/web`: a lazy-loaded ghostty-web view of the selected thread over `GET /api/attach/{thread}` (mvp:239), an operate switch, a key bar, open approval cards in the thread copy and banner; `skills/chaldea-rescue/SKILL.md` (mvp:332). **Daemon:** one CSP directive (D6).

**Does not add** a built-in Rescue UI, A2UI, `chaldea attach`, `/screen` in the PWA, or new commands, frames, or event kinds (AGENTS.md invariant 2).

## 2. Contract

### 2.1 Loading

A "Terminal" disclosure, closed by default. The first open runs `import()` of the terminal module and `Ghostty.load(<hashed /assets/ wasm URL>)`, once per page; first paint fetches neither. ghostty-web's `init()` is not used: it fetches an inline `data:` URL, which `connect-src` blocks, then `/ghostty-vt.wasm`, which the daemon does not serve (gw:12-27). The ticket and socket follow a successful load, so a failure holds no slot; it shows an error with Reload.

### 2.2 Attach Lifecycle

- **Open**: `POST /api/ws-ticket { purpose: "attach:<thread>" }` (W1's request policy, w1:21), then the socket, which takes over the launch's slot (t1 D2).
- **`attached`**: reset the emulator, apply `modes` and the grid, send one `resize` from the fit proposal, later ones debounced; `size` frames resize the emulator.
- **`grid: null` or `size { ok: false }`**: operate goes off until the human turns it on; a notice offers Redraw (a fresh attach) (t1 D3).
- **Close**: thread switch, closing the view, `pagehide` (W1 unmounts, `main.tsx:28-36`), and becoming hidden close the socket. Becoming visible reattaches a socket closed by hiding; nothing else reconnects automatically (kastty `attach-client.ts:84-89`). Server closes show their reason with Redraw (4003: Take back).

### 2.3 Read-only and Operate

`<input type="checkbox" switch>` labelled "Operate" (`switch` Markup), off on every new socket. While off, the client sends only `resize`: no keystroke, IME commit, paste, key bar key, or emulator reply. One gate in the send function enforces it (`disableStdin` misses replies and wheel keys, gw:2157-2164, :2787-2791).

### 2.4 Paste, Replies, Held Input

- **Paste** (t1 D4): a capture-phase `paste` listener on the view's wrapper stops the event and sends one `paste { paste_id, text }`, whatever DECSET 2004 says. ghostty-web never sees it: its container handler forwards unmarked text (gw:963-977).
- **Replies**: data emitted while output is being written is dropped; ghostty-web answers queries inside `write` (gw:2390; kastty `attach-client.ts:202-220`). The whole-line wheel handler claims every wheel event, so no alternate-screen arrows arise (kastty `ghostty-adapter.ts:68-103`).
- **Held input** (t1 D6; D3): from a `paste` until its `paste_result`, all later input is held in order. On `written` it is sent; on any other outcome (`pending` is final, `socket.rs:1353`) or disconnect it is dropped with a notice. `input_dropped` is shown.
- **IME in the view**: Enter within 50 ms of `compositionend` is dropped (gw:851 guards only `isComposing`, 229).
- **Codex** (t1 D9): operating a Codex thread notes "on Windows hosts, typed non-ASCII may be lost; use the composer" (snapshots carry no host OS).

### 2.5 Key Bar

`role="toolbar"` with `focusgroup` and its fallback (`toolbar` Structure, Keyboard navigation): Esc, Tab, Ctrl-C | ←, ↑, ↓, → | 1, 2, 3 | Enter; 44 px coarse targets, wrapping at 390 px (`form-factor-adaptation` Size targets); enabled only while operating (mvp:480).

- **Plain mode**: attach bytes as `KeyToken` encodes them (sup-send:186), arrows by the emulator's DECCKM (D1).
- **Card mode**, while the valid copy has an open card of the current launch this page has not answered (with several, the human picks the one the screen shows): arrows are staged visibly; a deciding key sends one `key { thread_id, launch_id, keys: [staged…, decide], approval_id }` (sup-send:64), so the `card_key` rules hold (mvp:358, :645). W2's slot and reply rules apply (w2:43-54). `written` or `incomplete` marks it answered (plain mode again); refusals are shown.

### 2.6 Cards and Tokens

The thread copy gains `cards`: the snapshot's `open_approvals` (`sync.rs:227`), then the stream's approval events (F1). The banner's `awaiting_approval` entry shows `tool_name`, a `tool_input` preview, and "Answer in terminal". The PWA holds only `owner` tokens (mvp:236), which attach and `key` need (mvp:278); a 403 on the ticket shows "needs the owner pairing", unretried.

### 2.7 Rescue Skill

Draft in §6.4: read `inspect --json` and `/screen` (state from axes and events only); act alone with `ready`, `input resolve`, `wait`; other commands only when asked; hand back prompts, approvals, `resume_blocked`, unclear screens; never resend `unconfirmed`.

## 3. Decisions

All reversible; D6 is a public header.

| # | Decision | Rejected |
|---|---|---|
| D1 | Plain keys over attach; card keys over `key` with `approval_id`. | All `key`: an event per tap, unordered against typing and pastes. All attach: no `approval_id`, so no `card_key` rules. |
| D2 | Card mode stages arrows: one request per deciding key. | A request per arrow: later card keys are refused after any human unit (sup-send:196). |
| D3 | Hold all input behind a paste. | Only submit and queue keys: per-adapter classes in the client, and typed bytes reordered around a held Enter. |
| D4 | Operate per socket, default off. | Remembered: a new screen is unchecked (t1 D3). |
| D5 | Close on hide, reattach on show, no other auto-reconnect. | Open while hidden: throttled timers lag (4002); an unseen tab holds the slot. |
| D6 | CSP adds `script-src 'self' 'wasm-unsafe-eval'`; wasm from `/assets/`. | `'unsafe-eval'` (allows JS `eval`); the inline `data:` wasm (also needs `data:` in `connect-src`). |
| D7 | Pin `ghostty-web` `0.4.0` (`latest`, 2025-12-09). | `0.4.0-next.25` (2026-11-18): prerelease, workarounds unverified. |
| D8 | One `Terminal` per open view, reset per socket. | One per socket: 0.4.0 keeps each disposed renderer (kastty `ghostty-adapter.ts:172-173`). |
| D9 | Read-only sends `resize`. | Only when operating: no redraw. |
| D10 | No switch polyfill. | `input-switch-polyfill`: Chromium's checkbox means the same. |

## 4. Acceptance Criteria

**Purpose layer** (M1 owner; macOS, Claude Code, Safari and Chrome, desktop and 390 px).

| ID | Criterion (EARS) |
|---|---|
| T2-1 | When Claude stalls on its trust prompt, the view shall show it legibly (CJK included) and the key bar shall clear it (P-12). |
| T2-2 | When Claude requests an approval, the banner shall show the card, a card key shall answer it, and the closed card shall disable (P-16). |
| T2-3 | While composing Japanese in the view, a confirming Enter shall not reach the CLI. |
| T2-4 | When an external agent follows the skill on a gate-timed-out thread, it shall act per §2.7. |

**Plan layer** (Vitest; Playwright, Chromium and WebKit, desktop and 390 px, FakeCli; each fails on revert).

| ID | Claim (EARS) | Test |
|---|---|---|
| T2-5 | At first paint no terminal code shall load; after a load failure, no ticket. | `lazy_load`, `wasm_failure` |
| T2-6 | While operate is off, the page shall send only `resize`. | `attach.gate`; `read_only_writes_nothing` |
| T2-7 | When `attached` arrives, it shall be applied and one `resize` sent; after `grid: null` or `size { ok: false }`, operate shall be off. | `attach.initial`, `attach.degraded`; `redraw_fresh_attach` |
| T2-8 | When the human pastes, 2004 on or off, exactly one `paste` frame and no binary frame shall be sent (I-6). | `attach.paste`; `paste_one_frame` |
| T2-9 | When output carries DSR or DA, or the wheel scrolls the alternate screen, no frame shall be sent (I-6). | `attach.replies`; `dsr_no_reply` |
| T2-10 | While a paste is outstanding, later input shall be sent in order only on `written`. | `attach.hold`; `paste_then_enter` |
| T2-11 | The key bar shall follow §2.5. | `keybar.encode`, `keybar.card`; `key_bar_plain`, `card_key_answer`, `card_closed_refused` |
| T2-12 | The socket shall follow §2.2. | `attach.lifecycle`; `switch_closes`, `hide_show_reattach`, `takeover_two_tabs` |
| T2-13 | No sync or command module shall import the terminal module. | `imports.terminal_isolated` |
| T2-14 | With the view open, no CSP violation shall arise and an injected `eval` shall stay blocked. | `csp_clean`, `csp_enforced`; daemon `pwa_mount` |
| T2-15 | At 390 px the view shall fit without page scroll; composer rules shall hold. | `layout_390`, `ime_sequences`, `terminal_ime_enter` |
| T2-16 | Every command the skill names shall parse. | cli `rescue_skill_commands` |

## 5. Risks

- **Screen as state**: the terminal module exports no reducer action and is import-isolated (T2-13); cards come from the stream.
- **Writes while read-only; paste twice or split**: one gate, one listener, one frame (T2-6, T2-8, T2-9).
- **Replies to the PTY**: relies on 0.4.0 answering inside `write`; T2-9 catches a change.
- **Lost Japanese text**: prompts use the composer; dropped held input is announced; Codex for Windows accepted (t1 D9; X2).
- **Token scope**: owner only; the skill's agent has owner authority, so §2.7 limits its own actions.
- **CSP**: `'wasm-unsafe-eval'` allows WebAssembly compilation only; JS `eval` and Trusted Types stay blocked (T2-14).
- **Bundle**: first paint unchanged. Accepted: a 682 KB lazy chunk (564 KB of it unused inline wasm) plus 423 KB wasm, localhost, immutable, once.
- **Stale card** (C1 L-13): a card key may reach the composer; the owner types in the view.

## 6. Implementation Notes

Not part of the design text; for the PR brief.

### 6.0 Contract Details

- **Applying `attached`**: `terminal.resize(cols, rows)`, `reset()` (with the §6.2 fix), then local writes: `ESC[?1049h` when `alt_screen`; each grid row at `ESC[<r>;1H`, cell strings in order, skipping `""` wide tails (pty-runtime §2.6); the cursor `ESC[<row+1>;<col+1>H`; `ESC[?25l` unless `show_cursor`; `ESC[?1h` for `app_cursor`; `ESC =` for `app_keypad`; `ESC[?2004h` for `bracketed_paste`; `ESC[=<flags>;1u` for `kitty_flags`. These local writes go through the same `writing` flag.
- **Resize**: the fit addon's `proposeDimensions()`, clamped to 2–512 × 1–256, sent when it differs from the last sent, debounced 100 ms.
- **Close codes shown**: 4000 "no live launch"; 4001 and `ended` "launch ended" (Redraw attaches a resumed launch); 4002 "fell behind"; 4003 "another view took over" (Take back = fresh attach); 4004, 1001, 1006 "disconnected"; 1003, 1007, 1009 "protocol error" (a client bug; logged without frames); 1008 hands to W1's credential path (re-reads the credential, no retry).
- **Paste results**: `rejected` and `not_written` show their reason (t1 §2 "Reasons"); `incomplete` shows the bytes written.
- **Hold cap**: 64 KiB of raw bytes and one further paste; past it, input is refused with a notice, never queued unbounded.
- **Card answer**: "Answer in terminal" selects the thread, opens the view, and focuses the Operate switch; card mode shows the card's `tool_name` beside the bar. Staged arrows: at most 31 (`MAX_KEYS` 32, sup-send:186); Clear drops them; a card change or socket close drops them.
- **Focus**: key bar buttons call `preventDefault` on `pointerdown` so the soft keyboard and the terminal's focus stay.

### 6.1 Files

| File | Change | Size |
|---|---|---|
| `packages/web/package.json` | `ghostty-web` `0.4.0` exact | — |
| `src/terminal/ghostty.ts` | adapter: `Ghostty.load(wasmUrl)` from `import wasmUrl from "ghostty-web/ghostty-vt.wasm?url"`; kastty workarounds (§6.2) | ~150 |
| `src/terminal/attach.ts` | socket client: frames, gate, `writing`, paste capture, hold queue (cap 64 KiB raw and one paste; over it, refuse input with a notice), lifecycle, close codes | ~300 |
| `src/terminal/snapshot.ts` | `attached` to emulator writes | ~60 |
| `src/terminal/keys.ts` | `KeyToken` bytes (DECCKM), staging | ~60 |
| `src/terminal/TerminalView.tsx`, `KeyBar.tsx`, `OperateSwitch.tsx` | UI; `lazy()` boundary in `App.tsx` | ~250 |
| `src/sync/reducer.ts`, `ops.ts` | `cards` in the thread copy | ~60 |
| `src/commands/*` | `key` slot and replies; `KeyArgs`/`KeyResult` validators (schemas exist in `packages/contracts/schema`) | ~80 |
| `src/ui/AttentionBanner.tsx`, `attention.ts` | card list, "Answer in terminal" | ~40 |
| `src/http.ts` | `postTicket(purpose)` | ~5 |
| `crates/daemon/src/http/pwa/mod.rs` | `CSP_HEAD` gains `script-src 'self' 'wasm-unsafe-eval'; `; `.wasm` is already allowed (`dist.rs:31`) | ~3 |
| `crates/testkit/src/fake_cli/echo.rs` (or `--script`) | lines that emit `PermissionRequest`, write `ESC[6n` and `ESC[c`, toggle 2004 off, enter 1049 | ~60 |
| `crates/cli/tests/rescue_skill.rs` | T2-16 | ~40 |
| `skills/chaldea-rescue/SKILL.md` | §6.4 | ~70 |

`App.tsx`'s header comment ("No terminal view") changes. W2's `request_policy` test (W2-12) keeps its scope while the view is closed; T2 adds attach and `key` only with it open.

### 6.2 ghostty-web 0.4.0 Workarounds

kastty paths are `kastty@f7578f3:web/src/<file>:<line>`; it depends on `ghostty-web` `^0.4.0` (`package.json`).

| Defect in 0.4.0 | Workaround | Source | Chaldea |
|---|---|---|---|
| `scrollback` is a byte capacity, not lines | lines × 2048, clamped to 1 MB–1 GB | kastty `ghostty-adapter.ts:32-36` | reused |
| Fractional `viewportY` renders the scrollback boundary off by one | custom wheel handler scrolling whole lines | `ghostty-adapter.ts:68-103` | reused; it also stops alternate-screen wheel arrows (gw:2157-2164) |
| `reset()` leaves `SelectionManager` on the freed WASM terminal | repoint it and clear the selection | `ghostty-adapter.ts:105-124` | reused (D8) |
| `dispose()` leaves a document listener holding the renderer and canvas | zero the canvas size before `dispose()` | `ghostty-adapter.ts:169-180` | reused |
| Wheel listener left on the element after `dispose()` | a fresh mount element per terminal | kastty `main.ts:80-86` | reused |
| `fit()` would resize locally | only `proposeDimensions()`; the host's `size` decides | `ghostty-adapter.ts:142-145` | reused |
| Resize floods | 100 ms debounce | `ghostty-adapter.ts:12-13`, `:161-168` | reused |
| Query replies emitted inside `write()` | drop data while writing | kastty `attach-client.ts:202-220` | reused (t1 D4) |
| Font family not quoted in the canvas shorthand | quote it | `ghostty-adapter.ts:20-30` | not needed: a fixed system stack (`ui-monospace, Menlo, monospace`) has no digit-leading token |
| Fonts must load before cell measurement | load the font CSS first | kastty `main.ts:27-46` | not needed: no web fonts |
| Host message limit | split input frames | kastty `attach-client.ts:60-78` | not needed: pastes are one `paste` frame within t1's limit, typed input is far below 64 KiB |
| `init()` fetches a `data:` URL, then `/ghostty-vt.wasm` | `Ghostty.load(assetUrl)`, `new Terminal({ ghostty })` | gw:12-27, :17 | Chaldea's own (CSP) |
| Two paste paths, one unmarked | capture-phase interception | gw:963-977, :2368-2371, :2407-2408 | Chaldea's own (t1 D4) |
| Post-`compositionend` Enter not guarded | `attachCustomKeyEventHandler` drops it within 50 ms | gw:851 | Chaldea's own |

Each reused workaround keeps a unit test modelled on kastty's (`web/src/ghostty-adapter.test.ts:70-128`, `web/src/attach-client.test.ts:75`); a pin change re-runs the Playwright rows against the real package. ghostty-web makes its container `contenteditable` with `role="textbox"` (gw:2330); that is the terminal view, not a prompt input (AGENTS.md invariant 4).

### 6.3 Tests

- Vitest: `attach.gate`, `attach.initial`, `attach.degraded`, `attach.paste`, `attach.replies` (a fake terminal emitting during `write`), `attach.hold` (each `PasteOutcome`, disconnect, cap), `attach.lifecycle` (each close code, hide and show), `keybar.encode` (each token, DECCKM on and off), `keybar.card` (staging, more than one card, answered, `card_closed`), `imports.terminal_isolated`, the adapter workaround tests.
- Playwright: frames through `page.routeWebSocket` with `connectToServer`, the screen through `GET /api/threads/{id}/screen` (the canvas cannot be read). `wasm_failure` routes the wasm to 404.

### 6.4 Skill Draft

```markdown
---
name: chaldea-rescue
description: Diagnose a stuck Chaldea thread from an agent on the owner's PC with `chaldea inspect` and the thread screen, act with the existing commands, and hand judgment calls back to the human.
---

# Chaldea Rescue

The `chaldea` CLI runs with the owner token: every command you run, and every key you write, acts on a live agent CLI as the owner would.

## Read

1. `chaldea inspect <thread> --json`: state axes, attention causes, launch, inputs, open cards, recent events, and the live screen. Exit 1: unreadable; exit 4: summary only, so run it again.
2. For the screen alone: `GET /api/threads/<thread>/screen` with the owner Bearer token. A 409 `no_live_launch` means no launch is running.
3. Decide from the axes, causes, and events. Use the screen to see what the CLI shows a human, never as the thread's state.

## Act

| Inspect shows | Screen shows | Do |
|---|---|---|
| `input_gate_timeout`, turn `idle` or `unknown` | the CLI's normal composer, empty, no popup or search | `chaldea ready <thread>` |
| `input_unconfirmed` | the prompt submitted (in the CLI's history) | `chaldea input resolve <input> --outcome delivered` |
| `input_unconfirmed` | the prompt absent, composer empty | `chaldea input resolve <input> --outcome dismissed`, then tell the human; do not send it again |
| turn `running` | output still changing | `chaldea wait <thread> --until idle --timeout 5m` |
| `session_start_timeout`, `startup_stall` | a trust or hook-review prompt | hand back |
| turn `awaiting_approval` | a permission dialog | hand back |
| `resume_blocked` | — | hand back (`stop --assume-dead` needs the human's evidence) |
| anything else, or a screen you cannot read with confidence | — | hand back |

Run `send`, `key`, `stop`, `resume`, or `input cancel` only when the human asked for that action on that thread. Use single named keys (`chaldea key <thread> escape`).

After each action, run `inspect` again before the next one.

## Hand back

Tell the human the thread, the cause, a short screen excerpt, and the command you would run. They answer in the PWA's terminal view or at the CLI.

## Never

- Send an `unconfirmed` prompt again.
- Answer approvals or trust prompts.
- Use `--assume-dead`.
- Open the attach WebSocket.
```

### 6.5 Canonical-Doc Edits (not applied here)

- `docs/design/w1-pwa.md` §2.1 and D6, and mvp §2.5's `/` route row (mvp:234): the CSP gains `script-src 'self' 'wasm-unsafe-eval'` (D6 here).
- `docs/design/t1-attach.md` D6 and mvp:439: "holds submit and queue keys" becomes "holds all later input" (D3 here; a superset).
- `docs/design/w2-composer.md` W2-12 and the `awaiting_approval → none (T2)` banner row: point to §2.5–2.6 here.
- mvp §2.8 (mvp:332): the skill exists; no change of rule.
