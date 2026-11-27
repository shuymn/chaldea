# Design: The Attach WebSocket (MVP T1)

T1 of `docs/mvp-design.md` §4 (mvp:436, :475; CL-19, :613): the daemon side of `GET /api/attach/{thread}` (PR 11) for T2.

## 1. Scope

**Adds.** The attach handler (`crates/daemon/src/attach/`), frames (`crates/core/src/attach.rs`), a pty-runtime `SessionHandle` and bounded close, the launch lease, attach interval, per-purpose limits.

**Does not add.** A client, `chaldea attach`, P-9, an SGR serializer (deferred, mvp:451-462), commands, event kinds, routes.

## 2. Wire Contract

Admission (PR 7): an owner-only, single-use, 30 s `attach:<thread>` ticket (http-surface §2.4).

| Dir | Frame | Content |
|---|---|---|
| ← | `attached` (first) | `thread_id`, `launch_id`, `cols`, `rows`, `cursor`, `modes { bracketed_paste, alt_screen, show_cursor, app_cursor, app_keypad, kitty_flags }`, `grid` (pty-runtime §2.6) or `null` |
| ← | binary | PTY output after the snapshot, gapless |
| ← | `size` | PTY size after a resize attempt, `ok` |
| ← | `paste_result` | `paste_id`, `outcome` (`written`, `incomplete`, `pending`, `not_written`, `rejected`), `bytes_written: u64 \| null` (null on `pending`), `reason` |
| ← | `input_dropped` | `bytes` not written (whole unit on `pending`), `reason` |
| ← | `ended` | `launch_id`, `reason` (`eof`, `error`, `gone`) |
| → | binary | deliberate input only (keys, IME commits, key bar); empty ignored |
| → | `resize` | `cols` 2–512, `rows` 1–256 |
| → | `paste` | `paste_id` (socket-local, never replayed, ≤ 128 bytes), `text` |

**Reasons.** `rejected`: a `RejectReason`, `prompt_too_large`, `bracketed_paste_off`. Writer and session: `expired`, `bracketed_paste_off`, `queue_full`, `busy`, `incomplete`, else `not_written`. Raw only: `paste_marker`, `too_large`, `pending`.

**Close codes** (best effort). 4000 `no_live_launch`, 4001 `launch_ended` (after `ended`), 4002 `lagged`, 4003 `replaced`, 4004 `busy`; 1001 shutdown, 1003 malformed frame or size, 1007 invalid UTF-8, 1008 token revoked, 1009 over the limit.

**Limits.** Inbound message: 6 × `MAX_PROMPT_BYTES` + 4 KiB for attach (a fully escaped 256 KiB paste), 64 KiB for `/api/ws`; raw frame 64 KiB; paste text 256 KiB; `paste_id` 128 bytes (longer: 1003, nothing written); `attached` ≤ 192 KiB whole; queued output 1 MiB; drain 5 s; send 10 s per frame; idle 75 s; candidacy 15 s from the upgrade, then 4004.

## 3. Decisions

| # | Decision | Rejected alternative |
|---|---|---|
| D1 | One socket serves one launch; resume needs a new one; process state comes from sync. | Following the thread across launches. |
| D2 | One operating socket per launch for M1; a newer one takes over (4003); T2 explains and offers take-back. | Several viewers with last-resize-wins. |
| D3 | Initial screen: the snapshot, then the first resize's forced redraw (one-row nudge at equal size). T2 applies `modes`, keeps operate off after `grid: null` or a failed resize until the human enables it, and offers Redraw. | A daemon VT serializer (deferred). |
| D4 | Clipboard text arrives only as `paste`; T2 intercepts paste and suppresses ghostty-web's query replies. | Recognising pastes in raw input: unmarked ones submit early on Codex for Windows. |
| D5 | A paste on a bracketed-paste row needs DECSET 2004 on at encoding and dispatch. | Unwrapped text: CR newlines submit each line. |
| D6 | One outstanding unit per socket; raw input joined; T2 holds submit and queue keys until `paste_result` and sends them only on `written`, else (or on disconnect) drops them. | One unit per frame: lost middle units, stale Enter after a failed paste. |
| D7 | One launch lease orders resizes against sends, outside the op lock. | A flag beside `running`. |
| D8 | A lagging client gets the prefix, then 4002. | In-place resync. |
| D9 | Raw input is written as sent; T2 notes typed non-ASCII may be lost on Codex for Windows; X2 decides keyed entry. | Keying raw input now. |
| D10 | Inbound limits by ticket purpose. | Chunked paste frames. |
| D11 | Attach records `human_write` once per interval (mvp:88); `key` and `card_key` stay per unit. | One event per attach unit. |
| D12 | The snapshot copy is budgeted; over budget, `grid` is `null`, same offset. | A larger write buffer. |

All are reversible except D4's public frame.

## 4. Invariants

| ID | Invariant | Test |
|---|---|---|
| I-1 | `Session::close` returns within one absolute deadline even while a resize holds the control, reporting it unreleased until closed; resizes start only before seal. | pty `close_with_resize_held`, `resize_after_seal_refused` |
| I-2 | Every unit and resize carries its socket's grant from takeover; replacement, revocation, disconnect, and launch end revoke it: no unpermitted old write or resize reaches the PTY. | `old_enter_after_disconnect_and_reattach`, `delayed_old_submission`, `old_resize_parked_before_backend`, `failed_candidate_keeps_current` |
| I-3 | Only the lease holder (by acquisition id) submits or resizes; known rollback or non-admission releases it; an indeterminate commit keeps it until exit; `stop` never waits for resize. | `resize_waits_for_opened_send`, `open_waits_for_resize`, `open_result_unrecorded`, `invalidation_races_conversion`, `stop_with_resize_held` |
| I-4 | Pending input stays ≤ 64 KiB plus one validated message; after `pending`, nothing more until that unit settles. | `crossing_paste_kept`, `max_paste_with_raw_outstanding`, `stuck_writer_after_pending` |
| I-5 | `ready → key → attach` records `human_write { attach }`; `attach → card_key → attach` records it once; gate and ack decisions unchanged. | `ready_key_attach_records`, `attach_card_key_attach_skips`, `typing_records_once`, `worker_exit_overlap` |
| I-6 | No complete bracketed-paste marker reaches the PTY from raw input; a lone Escape is written at once. | `marker_split_at_every_position`, `lone_escape_immediate`, `dsr_answered_once`; T2: paste with 2004 off yields only `paste`, a rendered DSR yields no client frame |
| I-7 | Revocation ends the socket within §6's phase bounds and moves the grant: no new attach unit dispatches after. | `revoked_at_startup`, `revoked_read_parked_in_snapshot`, `revoked_lost_nudge_under_load`, `slot_with_cleanup_held` |
| I-8 | The first terminal event wins and revokes the grant; `ended` is the last application frame; sends stop at the drain end (§6). | `drain_cutoff_drops`, `lag_then_gone_keeps_4002`, `lag_stalled_releases_slot` |

## 5. Acceptance Criteria

FakePty on both runners, Codex `win_keys` on a Windows host row; production mount.

| ID | Criterion (EARS) | Test |
|---|---|---|
| T1-1 | If a non-owner asks for an attach ticket, 403; if another purpose or thread is redeemed, 401 unspent. | `owner_only`, `production_mount_admission` |
| T1-2 | When a socket attaches, `attached` equals the snapshot, then exactly the later bytes; the grid is kept at 192 KiB, `null` past it. | `initial_screen_gapless`, `attached_at_cutoff`, `envelope_tips_over` |
| T1-3 | When the first resize arrives, the PTY resizes, nudging at an equal size; a later equal size is a no-op; a failed leg reports `ok: false`. | `first_resize_forces_redraw`, `nudge_superseded`, `nudge_second_leg_fails` |
| T1-4 | When binary frames arrive, the bytes are written unchanged and in order, and the gate closes; same-turn evidence leaves a send queued and `ready` opens it. | `raw_input_order`, `input_closes_gate` |
| T1-5 | When a paste is accepted, the encoder's `AttachPaste` text is written as one part, no submit, and answers `written`; T2 never sends a held key after another outcome. | `paste_one_part_without_submit` |
| T1-6 | If a paste is refused (empty, 256 KiB + 1, `check`, 2004 off or toggled, writer), zero bytes, no event, the mapped reason. | `paste_refusals`, `paste_2004_toggled`, `refusal_mapping`, `report_channel_lost` |
| T1-7 | When a thread resumes with the old socket open, a new socket attaches the new launch, untouched by the old. | `resume_with_old_socket_held` |
| T1-8 | When output ends or the session goes, `ended` and 4001; no live launch 4000; busy 4004; shutdown 1001. | `ends_on_eof`, `ends_on_error_child_alive`, `ends_on_gone`, `no_live_launch`, `startup_busy`, `shutdown_closes_1001` |
| T1-9 | If a frame breaks a limit or the format, the stated reason or close; the TypeScript contracts include the attach types. | `frame_limits`, `worst_case_escape_fits`, `events_limit_unchanged`, `bad_frames`, `committed_contracts_equal_generated_output` |

## 6. Risks and Accepted Limits

- Attach never feeds state (invariant 1).
- Accepted: unpadded paste triggers (input-encoders D10); L-11's late `Stop` after attach-started turns.
- Accepted: redraw needs the CLI to honour resize; a sequence split at the snapshot renders wrong until redrawn (D3).
- Accepted (I-7): from the nudge, a revoked socket's slot is released ≤ 6 s while operating (5 s read, 1 s close grace), ≤ 10 s in the terminal close, ≤ 11.25 s at startup (two reads, a 250 ms lock, close grace; within the candidacy); lost nudges add the 30 s tick (operating). ≤ 4 blocking reads per socket. Dispatched units finish (pty-runtime §2.3); transport bytes may arrive.
- Accepted (I-6): with raw input in flight (a takeover's held write or the socket's earlier unit), a marker-refused last frame flushes an accepted partial alone: ≤ one split escape (ConPTY may drop it); no refused byte is written; order holds.
- Accepted (I-8): an end latched during candidacy ends with it (15 s from the attach handler's WebSocket handover, plus close grace) or drains 5 s from the first operating-loop turn. One send batch (an application frame, maybe a Ping), even one selected after the latch, may follow the cutoff (≤ 10 s). Order and terminal event hold.

## 7. Canonical Text

mvp (§2.1, §2.3-§2.5, §4, `GateEvidence`), supervisor-send (§2.3, §2.6, SB11), pty-runtime (§2.2, §2.7, §4-§5), http-surface (§2.6, §2.11, §4), input-encoders (§2.3, §4), architecture §5.
