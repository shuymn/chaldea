# Design: The Attach WebSocket (MVP T1)

Status: designed. Scope is row T1 of `docs/mvp-design.md` §4 (cited as "mvp §N", row at mvp:436), carrying PR 11's daemon side: `GET /api/attach/{thread}`, its initial screen, input, recognised paste, and resize, for the owner only (milestone M1). W1 builds on it; T2's web terminal view is its only client. Obligations met here: mvp:475 (paste without submit and refused with zero bytes, resizing during a unit, `human_write`, the initial screen, non-owner refusal) and the attach half of CL-19 (mvp:613).

Contracts relied on: AGENTS.md invariants 1 (AGENTS.md:9-11), 2 (:13), 4 (:22-24), 5 (:25-28); arch §4 PTY Seam (`docs/architecture.md:45`) and the Windows paste item (:77); mvp §2.3 (paste has no submit :161, human writes close the gate :175-176, prompt limit :186, rejections :187), §2.5 (deadlines and `Connection` :228, routes :237-239, tickets :253, owner-only attach :278); `docs/design/input-encoders.md` (cited "enc") `Purpose::AttachPaste` (:123-125), the writer mapping (:181), framings (:248, :258), D10 (:449); `docs/design/pty-runtime.md` (cited "pty") `subscribe` and `resize` (:162-164, :201, :203), `WriteOrigin::Attach` (:253), write permission (:307), `human_write` (:322), F5 (:323), `QueueFull` (:325), `Snapshot` and `TermModes` (:366-388), the grid encoding (:399-407), the inventory rows (:500-501), `subscribers` 8 (:512); `docs/design/http-surface.md` (cited "http") tickets and redemption (:363-372), `WsConfig` and the socket loop (:426-460), revocation (:472), the attach seam (:648); `docs/design/supervisor-send.md` (cited "send") the open job (:101), human units (:198), SB11 (:304), L-11 (:418).

Code relied on (origin/main `3cd2673d`): the route spec `crates/daemon/src/http/routes.rs:212-218` and `UpgradeHandler` (:366); `UpgradeGrant`/`UpgradedSocket` `crates/daemon/src/http/upgrade.rs:52-66`; the inbound limit applied to every upgrade (:149-152); the owner check at ticket issue `crates/daemon/src/http/ws.rs:60-74`; close codes (:314-319); `SendLaunch::submit_human` and `KeyReport` `crates/daemon/src/supervisor/send/mod.rs:145-150`, :391-405; the open job's checks `crates/daemon/src/supervisor/send/driver.rs:593`, :629; `LiveLaunch::with_session` `crates/daemon/src/supervisor/live.rs:310`; `Supervisor::live` `crates/daemon/src/supervisor/mod.rs:348`; the default PTY size 120×40 (:269); the screen route's live-launch rule `crates/daemon/src/supervisor/screen.rs:73-80`; `channel_sink` (blocking `std::sync::mpsc` receiver) `crates/pty-runtime/src/session.rs:1156-1186`; `PtySize` bounds `crates/pty-runtime/src/backend.rs:42-76`.

## 1. Scope

**Adds.** The upgrade handler for `GET /api/attach/{thread}` (a `Mount::upgrade` on the existing spec); the attach socket loop in `crates/daemon/src/attach/`; public frame types in `crates/core/src/attach.rs` with `ts-rs` exports; one flag in `SendState` (D7); the inbound message limit chosen by ticket purpose (D10).

**Does not add.** Any client: the `chaldea attach` CLI and its P-9 real-host paste tests stay deferred (mvp:451, :462), T2 builds the terminal view. No SGR serializer (deferred, mvp:452). No new command, event kind, or route. Thread state never reads attach output (invariant 1); the only terminal facts read are `modes()` for the paste guard and the snapshot shown to the human.

## 2. Wire Contract

**Admission.** The client mints `POST /api/ws-ticket { purpose: "attach:<thread>" }` (owner kind and `Attach` grant, else 403; thread missing, 404) and redeems it once within 30 s at `GET /api/attach/<thread>?ticket=` with PR 7's checks: Host, Origin absent or matching, an `attach:` ticket of an owner token naming this path's thread (else 401), single use decided last (http:365-372). `101` carries `Connection: Upgrade`. Socket slots are PR 7's (128 total, 16 per token).

**Startup**, in the registered socket task: (1) one read snapshot runs `verify_hash_in` (revoked: close 1008 `token_revoked`) and the thread summary: the thread is open and its current launch has no `exit_reason`, and `Supervisor::live(thread, launch)` holds it, else close 4000 `no_live_launch`; (2) an older attach socket of the same thread is told to close 4003 `replaced` (D2); (3) inside `with_session(now + 250 ms)`, `subscribe(sink, deadline)` and `size()`; `VtUnavailable` or `Busy` close 4004 `busy`, `Gone` closes 4000; (4) the `attached` frame. The socket is bound to that one launch (D1).

**Frames.**

| Direction | Frame | Content |
|---|---|---|
| server → client | text `attached` (first) | `thread_id`, `launch_id`, `cols`, `rows`, `cursor { row, col, visible }`, `modes { bracketed_paste, alt_screen, show_cursor, app_cursor, app_keypad, kitty_flags }`, `grid`: the snapshot in pty's grid encoding (pty:399-407) as a JSON value, or `null` when it would exceed 1 MiB |
| server → client | binary | PTY output after the snapshot's `output_offset`, in order, no gap, no duplicate (pty:203); chunks may be merged up to 64 KiB |
| server → client | text `paste_result` | `paste_id`, `outcome`: `written`, `incomplete`, `not_written`, `rejected`; `reason` for the last two (`RejectReason` values, `prompt_too_large`, `bracketed_paste_off`, `busy`, `queue_full`, `expired`) |
| server → client | text `input_dropped` | `bytes`, `reason` (`paste_marker`, `busy`, `queue_full`, `expired`, `not_written`, `incomplete`) |
| server → client | text `ended` | `launch_id`; then close 4001 `launch_ended` |
| client → server | binary | raw input bytes as the terminal view encodes keys; must not contain `ESC[200~` or `ESC[201~` |
| client → server | text `resize` | `cols`, `rows` within `PtySize` (cols 2–4095, rows 1–4095) |
| client → server | text `paste` | `paste_id` (u32, the client's), `text` |

A text frame that is not one of these (an out-of-range size included) closes 1003; a frame over the inbound limit closes 1009 (as http:456). Close codes 4000–4004 and their names are public.

**Initial screen and resize** (D3). The client paints `attached` (plain cells, cursor, and modes, so its key encoding matches DECCKM and kitty flags), then sends `resize`. The first `resize` of a socket forces a redraw: when its size differs from the PTY's, one `Session::resize`; when equal, resize to `rows + 1` (`rows − 1` at 4095) and back. Later resizes act only on a different size. Resizes run on the blocking pool, one in flight per socket, latest pending size wins. **F5** (D7): a resize waits while an input unit of the launch is running (`SendState.running`), and while a resize is in flight the open job opens no input (a `resizing` flag beside `running` and `human_unsettled`).

**Input** (D4, D6). Every write is an `Attach` unit submitted through `SendLaunch::submit_human` inside `with_session(now + 1 s)`, so the send worker records one `human_write { attach }` per `Written` or `Incomplete` unit and the gate closes (send:198; mvp:175). Units carry `Guard::until(now + 30 s)`, no fence and no basis; the writer queues them behind any input unit, never inside it (pty:500). One unit per socket is outstanding until its `KeyReport`; raw frames arriving meanwhile are concatenated into the next unit, up to 64 KiB pending, after which the socket stops reading inbound frames until the unit reports. Frame order is kept: a paste is its own unit.
- **Raw** (binary): written unchanged (D9). A frame containing a bracketed-paste marker writes nothing and answers `input_dropped { paste_marker }`. Refusals and partial writes answer `input_dropped`.
- **Paste** (text `paste`): above `MAX_PROMPT_BYTES` → `rejected { prompt_too_large }`; else `encode(agent, host_os, text, AttachPaste)` (enc:123-125), a `Rejection` → `rejected { reason }`; on a `BracketedPaste` row, DECSET 2004 off at encoding → `rejected { bracketed_paste_off }`, and the unit carries `bracketed_paste: Some(epoch)` from the same `modes()` read (D5). The unit is `WriteUnit { Attach, [Bytes(encoded.text)] }`: one part, no submit (enc:181). Every refusal writes zero bytes and records no `human_write`.

**Backpressure and bounds.** The output sink is the daemon's own: a non-blocking closure onto a `tokio` channel with a 1 MiB queued-byte budget, `channel_sink`'s semantics without its blocking receiver; over budget it records the lag offset and returns `Stop`, so the drain never waits for a socket (pty:341). The socket sends the bytes before the lag offset, then closes 4002 `lagged`; a reconnect gets a fresh screen (D8). Sends: 10 s per frame, 256 KiB write buffer; idle: 75 s without an inbound frame, a `Ping` every 30 s (http:426-435). Inbound limit 1 MiB for `attach` tickets, 64 KiB for `events` (D10). The task unsubscribes at exit (`with_session(now + 250 ms)`; if busy, the sink stops at its next byte), so the 8-subscriber cap (pty:512) holds one live attach sink per thread plus a replaced one.

**What ends the socket.** Output `End` (the launch's PTY reached EOF, normally at the exit op's close): flush, `ended`, close 4001. Fallback for a drain that never ends: every 1 s the task checks `Supervisor::live(thread, launch)`; gone → the same close. Thread close needs an exited launch (mvp §2.11), so it ends the socket the same way. Daemon shutdown: close 1001. Token revocation: re-verify on the nudge and every 30 s, close 1008 (http:472). Send deadline, idle, read error: drop. Every close is best effort within `close_grace` 1 s (http:460).

**A new launch.** A resume creates a new launch; the old launch's socket has closed 4001 by then (a resume needs the old launch exited and dead, mvp §2.9). The client mints a new ticket and attaches the new launch.

## 3. Decisions

| # | Decision | Rejected alternative | Reversible |
|---|---|---|---|
| D1 | One socket serves one launch; a new launch needs a new socket. | Follow the thread across launches (`launch_changed` plus a new `attached`): more state in the loop for one reconnect. | Yes (additive frame) |
| D2 | At most one attach socket per thread; the newer replaces the older (4003), and T2 does not auto-reconnect on 4003. | Several sockets with the last resize winning and the size fanned out to every socket: per-launch size notification, and a client must render a size it did not choose. | Yes (additive) |
| D3 | Initial screen: the diagnostic snapshot as JSON, then the redraw forced by the first resize, nudging when the size is unchanged. | A daemon-side serializer of the snapshot to VT bytes (the deferred SGR serializer, kept for the case where the redraw falls short); a redraw only on size change (equal sizes would show no attributes). | Yes |
| D4 | A paste is an explicit `paste` frame; raw frames carrying `ESC[200~`/`ESC[201~` are refused. | Recognising pastes by scanning raw input for markers: a cross-frame parser, and a client terminal with 2004 off would send an unmarked paste raw and unencoded (Codex on Windows would submit per line, arch:66). | No (public frame) |
| D5 | On a `BracketedPaste` row a paste needs DECSET 2004 on, checked at encoding and at dispatch (the epoch guard). | Sending the text unwrapped, as a terminal does with 2004 off: its CR newlines submit each line (invariant 4: bracketed paste only while 2004 is set). | Yes (loosening) |
| D6 | One outstanding unit per socket, coalescing, backpressure by not reading; 30 s `not_after`. | Submitting each frame at once: an expired unit followed by a written one drops keystrokes from the middle, and one keystroke per unit multiplies `human_write` events. | Yes (internal) |
| D7 | F5: a resize waits for a running input unit, and a resize in flight holds the open job. | Resizing at any time: pty gives no guarantee for a resize between text and submit (pty:201), and on Windows a resize reaches the CLI as an input record between the text and Return. | Yes (internal) |
| D8 | A lagging client is closed (4002) and reconnects. | In-place resync (unsubscribe, resubscribe, a second `attached`): more protocol for a rare case. | Yes (additive) |
| D9 | Raw input is written as the client encoded it, on every row. | Mapping typed non-ASCII BMP scalars to key records on the Codex Windows row: prompts belong to the composer (invariant 4, AGENTS.md:23), and pastes are keyed already. | Yes (additive) |
| D10 | The inbound limit follows the ticket purpose: 1 MiB for `attach` (a 256 KiB prompt as JSON). | Chunked paste frames under the 64 KiB `events` limit: reassembly state in the loop. | Yes (internal) |

## 4. Acceptance Criteria

Tests run on both CI runners with `FakePty` (the encoder row comes from `(agent, host_os)`, so the Codex cases are `win_keys` on `windows-latest` and bracketed paste on macOS). Unit tests are in `crates/daemon/src/attach/tests.rs` (`attach::tests::…`) and `crates/core/src/attach.rs`. Real-host checks belong to the client: T2's M1 checklist (macOS, Claude) and X2 (Windows, Codex), with X1's FakeCli attach paste tests (mvp:460).

| ID | Criterion (EARS) | Test |
|---|---|---|
| T1-1 | If an agent or rescue token asks for an `attach:` ticket, the daemon shall answer 403; if an `events` ticket, or an `attach:` ticket of another thread, is redeemed at `/api/attach/<thread>`, it shall answer 401 and spend nothing; if the owner token is revoked after admission, the socket shall close 1008. | `owner_only`, `ticket_of_other_purpose_or_thread`, `revoked_closes_1008` |
| T1-2 | When a socket attaches to a live launch, the daemon shall send one `attached` frame equal to the subscription's snapshot, then exactly the output bytes after its offset, with output emitted during the subscribe neither lost nor repeated. | `initial_screen_gapless` |
| T1-3 | When the first `resize` arrives, the daemon shall resize the PTY to it, nudging by one row and back when it equals the current size; when a later `resize` equals the current size, it shall not resize. | `first_resize_forces_redraw` (FakePty `Resize` log) |
| T1-4 | When binary frames arrive, the daemon shall write their bytes unchanged, in frame order, as `Attach` units, and record one `human_write { attach }` per written unit; after such a write the next send shall stay `queued` until a later turn's evidence or `ready`. | `raw_input_order_and_human_write`, `input_closes_gate` |
| T1-5 | When a `paste` frame is accepted, the daemon shall write `encode(agent, host_os, text, AttachPaste).text` as one `Attach` unit of one `Bytes` part with no submit, and answer `written`. | `paste_is_one_part_without_submit` (Claude, Codex; multi-line and Japanese text) |
| T1-6 | If a paste is empty, above 256 KiB, refused by `check`, sent while 2004 is off on a bracketed-paste row, or refused by the writer, then the daemon shall write zero bytes, record no `human_write`, and answer `rejected` or `not_written` with the reason. | `paste_refusals` (`FakeChild::writes()` empty; no event) |
| T1-7 | If a binary frame contains `ESC[200~` or `ESC[201~`, then the daemon shall write none of it and answer `input_dropped { paste_marker }`. | `raw_paste_marker_refused` |
| T1-8 | While an input unit of the launch is running, the daemon shall not resize the PTY, and shall apply the latest pending size after the unit ends; while a resize is in flight, no queued input shall open. | `resize_waits_for_input_unit` (unit parked at `PauseEntered`), `open_waits_for_resize` (resize held by a test pause) |
| T1-9 | When the launch's output ends or its session leaves the supervisor, the daemon shall send every queued output byte, then `ended`, and close 4001; when no live launch exists at startup, it shall close 4000 without subscribing; when the daemon shuts down, it shall close 1001. | `ends_with_launch`, `drain_never_ends_fallback`, `no_live_launch`, `shutdown_closes_1001` |
| T1-10 | When a thread is resumed, a socket opened afterwards shall attach the new launch, and no frame of the old launch shall follow its `ended`. | `resume_needs_a_new_socket` |
| T1-11 | If a client does not read and queued output exceeds 1 MiB, then the daemon shall stop the subscription without blocking the drain, send the bytes before the lag offset, and close 4002. | `slow_client_lags` (FakePty floods; the child is never blocked) |
| T1-12 | When a second socket attaches to the same thread, the first shall close 4003 and its subscription shall be removed. | `newer_socket_replaces` (subscriber count) |
| T1-13 | If an inbound frame exceeds 1 MiB or a text frame is not an attach message, then the socket shall close 1009 or 1003; frames shall round-trip through the generated TypeScript types without drift. | `frame_limits`; core `attach::tests::serde_shape`; the existing `ts-rs` drift check |

## 5. Risks

- **State from terminal text (invariant 1).** Attach reads `modes()` only to guard a paste and the snapshot only to show it; the socket's end comes from output `End` or the live map, never from screen text. Closed.
- **IME and paste delivery (invariant 4).** A paste goes through the adapter's encoder and is keyed on Codex for Windows; D4 keeps a client from bypassing it. Accepted: typed non-ASCII in the terminal view to Codex on Windows is written as UTF-8 and may be lost (arch:68; D9), since prompts go through the composer; an unpadded paste on Codex fires composer triggers (enc D10). X2's smoke shows whether typed input needs keying.
- **The send gate.** Every attach unit is a human unit: no input opens while one is unrecorded, and its `human_write` closes the gate until a later turn's evidence or `ready` (send:198). Units queue behind an input unit, never inside it. A resize stalls the open job only while in flight; a resize that never returns (ConPTY waits on conhost, which the drain keeps draining) would keep the launch's inputs `queued`, visible as `input_gate_timeout`. Accepted: L-11 (send:418) also covers a turn the owner starts through attach.
- **Token scope (invariant 5).** Owner only at issue, at redemption, at startup, and on every re-verify; Origin checked; the ticket is redacted in logs. A revoked socket closes within the nudge or 30 s.
- **Resource bounds.** The drain never waits (sink budget); sockets share PR 7's slots; one attach subscriber per thread; inbound 1 MiB and 64 KiB pending input with read backpressure; one resize and one unit in flight per socket. Accepted: a busy unsubscribe leaves the sink until the next output byte; `human_write` events grow with typing, reduced by coalescing.
- **Redraw quality.** The nudge redraws the CLI twice, and rows outside the CLI's redraw keep the snapshot's plain cells. If that falls short in T2's dogfooding, the deferred SGR serializer (mvp:452) replaces the `grid` field.

## 6. Silent or Conflicting Canonical Text, and Proposed Edits

Silent: paste framing (left to PR 11 by enc:5), several sockets per thread, a new launch, slow clients, and F5 (send:304). Resolved by D1–D8 above. Conflicts:

- PR 7 applies the 64 KiB inbound limit to every upgrade (upgrade.rs:149-152), below the 256 KiB paste limit (mvp:186). Resolution: D10.
- enc:178 expects attach to decode bytes and refuse invalid UTF-8; a `paste` text frame is UTF-8 by protocol, and invalid JSON closes 1003. No refusal path is needed.
- mvp §2.4 (mvp:205) invalidates every sync request when the client "opens a WebSocket"; the rule is for the `/api/ws` socket. Resolution: name that socket.

Edits the other documents would need (not made here):

1. mvp §2.3 "Gate" (:176): the gate also stays closed while an attach resize is in flight. mvp §2.4 (:205): "opens a `/api/ws` socket". mvp §2.5 route row (:239): protocol in `docs/design/t1-attach.md`. mvp §4 T1 obligations (:475): F5 decided by D7.
2. supervisor-send §2.3 step 1 (:101) and §2.6 "Human units" (:198): the open job also requires no attach resize in flight; attach units are coalesced, one outstanding per socket.
3. pty-runtime §4 rows (:500-501): the repaint is the `attached` snapshot plus the forced redraw; one attach subscriber per thread.
4. http-surface §2.6 `WsConfig` (:426-435) and §2.11 (:648): the inbound limit by ticket purpose (1 MiB for `attach`); attach close codes 4000–4004.
5. input-encoders §2.3 (:178) and §4 row 11 (:336): paste text arrives as a JSON string; framing is T1's `paste` frame.
6. architecture §5 open items (:77): typed non-ASCII input to Codex on Windows is written as UTF-8 (D9); X2's smoke decides whether it needs keying.
